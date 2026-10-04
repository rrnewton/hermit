#!/usr/bin/env python3
"""Rebuild the e2e result files of a Buck test run from its Tpx results.

usage: ingest.py --plan ci/expected-e2e-plan.json --out IMPORT_DIR [--work DIR]
                 [--local-artifacts BUCK_OUT_TEST_DIR] TEST_RUN_ID...

TEST_RUN_ID is what `buck2 test --write-test-id FILE` wrote (one per invocation; the
hybrid run makes two, one for RE cells and one for local cells). Every cell execution
uploads the rows test-harness wrote as its results.jsonl Tpx artifact; this fetches
them with testx (TESTX env, default `testx`) and writes one bucket per (lane, category):

    IMPORT_DIR/<lane>/manifest_<category>/results.jsonl

    IMPORT_DIR/<lane>/manifest_<category>/summary.json

IMPORT_DIR is input for `test-harness run` in import mode (E2E_IMPORT_RESULTS=IMPORT_DIR)
and for nothing else. Import mode checks every cell's history, staleness and evidence,
then publishes the result files the cargo flow's e2e nodes write. Read directly, these
files skip all of those checks, and summary.json's counts leave out every cell whose
final execution wrote no summary.json.
summary.json aggregates each cell's final execution: counts are summed and cell lists
joined, which is what one harness process over the bucket writes.
summary.json's evidence_complete_executions lists each execution whose result.json
(written by cell.sh) records complete evidence, by its run id; import mode refuses a
PASS whose own execution is not listed. An execution's rows, result.json and run_id.
marker (see --local-artifacts) must name one run id, and no two executions may share one.
Tpx owns retries (cells run the harness with --no-retry). A cell's Nth execution in Tpx
order is attempt N, and its row (at most one, for its own cell) carries that number. An
execution that wrote no row still takes its number, so the next row shows the gap and
import mode refuses the history. "Passed only on rerun" therefore stays visible, as
attempt 1 failing before attempt 2 passes, even when attempt 1 died before writing a row.
A cell with no rows is host-inapplicable only if it ran and every one of its executions
says so in its own summary.json. Any other plan cell with no rows is an error, and so is
a cell outside the plan. Executions of one cell that end in the same second are refused:
their order is unknown, and testx addresses an artifact as RUN.TEST.END. Prints one
JSON summary line.

Import mode's parity post-pass compares each verify cell's first-run log (the one hermit
keeps as the golden copy) with ptrace's. A row records the --verify-log-dir its cell ran
with, in scratch space that is gone; cell.sh returned that directory's files as
artifacts (cell__<path below the row's artifact_dir, / as __>__<file>, .zst when
compressed). Every run1_log_ file directly in it is checked byte-exact against the
sha256 its result.json recorded, decompressed, and written to

    IMPORT_DIR/retained-verify-logs/<run id>/<path below the row's artifact_dir>/

IMPORT_DIR/retained-verify-logs/index.jsonl maps each recorded directory to the
directory below IMPORT_DIR its logs were restored to, or to why none were (no such log
among the artifacts, a failed fetch, no recorded or a different sha256). A directory is
restored whole or not at all. A log that is not restored leaves only its parity cell
unmeasured: it never fails the ingest. Each entry also records where the execution that
recorded the directory ran, as its result.json recorded it: route (local or re),
container (pinned-root or empty) and re_platform, each null when it recorded none or
several executions recorded the directory. The post-pass gives a pair clean credit only
when both cells ran on the same route.

--local-artifacts: Buck materializes each test's artifact directory locally
(buck-out/v2/test/execution/<cell>/<target hash>/<config hash>/default/artifacts_directory),
keeping the newest execution per target. cell.sh writes an artifact named run_id.<run id>,
which the testx listing shows without a fetch. An execution is read from the one local
directory holding its marker; every other execution, including one whose marker is in
no local directory or in several, is fetched with testx.
"""
import argparse, collections, concurrent.futures as cf, hashlib, json, os, re, shutil, subprocess, sys, tempfile, time

TESTX = os.environ.get("TESTX", "testx")
MARKER = "run_id."  # cell.sh: an artifact named run_id.<run id>
VERIFY_LOG_DIR = "--verify-log-dir"  # the harness's flag naming where hermit --verify keeps its logs
RUN1_LOG = "run1_log_"  # hermit's first-run log; parity.rs RETAINED_LOG_PREFIX
LOGS_DIR = "retained-verify-logs"  # parity.rs IMPORTED_LOGS_DIR
LOGS_INDEX = "index.jsonl"  # parity.rs IMPORTED_LOGS_INDEX
LOGS_SCHEMA = 2  # parity.rs IMPORTED_LOGS_SCHEMA
ROUTE_KEYS = ("route", "container", "re_platform")  # cell.sh's result.json: where the execution ran
SAFE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")  # a path component, never . or ..

def try_testx(*args, tries=4):
    """(testx's stdout, None), or (None, why it failed)."""
    for i in range(tries):
        p = subprocess.run([TESTX, *args], capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout, None
        time.sleep(5 * (i + 1))  # TestX answers 500 transiently
    return None, f"testx {' '.join(args[:3])} failed: {p.stderr[-500:]}"

def testx(*args, tries=4):
    stdout, error = try_testx(*args, tries=tries)
    if error is not None:
        sys.exit(f"ingest: {error}")
    return stdout

def rows_of(path):
    if path.endswith(".zst"):
        text = subprocess.run(["zstd", "-q", "-d", "-c", path], check=True, capture_output=True, text=True).stdout
    else:
        text = open(path).read()
    return [json.loads(l) for l in text.splitlines() if l.strip()]

def markers(names):
    return [n[len(MARKER):] for n in names if n.startswith(MARKER)]

def cell_of(entry):
    return "{}/{}@{}".format(entry.get("test"), entry.get("mode"), entry.get("backend") or "native")

def claims_host_inapplicable(cell, summary):
    return any(cell_of(h) == cell for h in (summary or {}).get("host_inapplicable_cells", []))

def run1_log_name(name, prefix):
    """The name hermit gave the first-run log that cell.sh stored as artifact NAME,
    when NAME is PREFIX + that name (+ .zst for a log cell.sh compressed), else None."""
    if not name.startswith(prefix):
        return None
    base = name[len(prefix):]
    base = base[:-len(".zst")] if base.endswith(".zst") else base
    # A name with __ in it was in a subdirectory, which hermit never writes logs to.
    return base if base.startswith(RUN1_LOG) and "__" not in base and SAFE.fullmatch(base) else None

def verify_logs(rows, names):
    """{recorded verify-log directory: (its path below the row's artifact_dir, prefix,
    [artifact name of each first-run log in it])} for every row run with --verify-log-dir,
    or a reason in place of the tuple when its logs cannot be among the artifacts NAMES.
    cell.sh stores the file <artifact_dir>/A/B as the artifact cell__A__B."""
    found = {}
    for row in rows:
        argv = row.get("argv") or []
        flag = argv.index(VERIFY_LOG_DIR) if VERIFY_LOG_DIR in argv else None
        if flag is None or flag + 1 >= len(argv):
            continue
        recorded, adir = argv[flag + 1], row.get("artifact_dir") or ""
        if not adir or not recorded.startswith(adir.rstrip("/") + "/"):
            found[recorded] = f"it is not below the row's artifact directory {adir!r}, the only one cell.sh returns"
            continue
        parts = recorded[len(adir.rstrip("/")) + 1:].split("/")
        if not all(SAFE.fullmatch(part) and "__" not in part for part in parts):
            found[recorded] = f"its path below the artifact directory, {'/'.join(parts)!r}, has no cell.sh artifact name"
            continue
        prefix = "cell__" + "__".join(parts) + "__"
        found[recorded] = ("/".join(parts), prefix, sorted(n for n in names if run1_log_name(n, prefix)))
    return found

def route_of(result):
    """{key: the string the execution's RESULT records for it, else None} for each of
    ROUTE_KEYS."""
    result = result if isinstance(result, dict) else {}
    return {k: result[k] if isinstance(result.get(k), str) else None for k in ROUTE_KEYS}

def restore(out, run_id, result, logs, log_dir, fetch_error):
    """Copy each first-run log LOGS (see verify_logs) names out of LOG_DIR into
    OUT/LOGS_DIR/RUN_ID/<its path below the artifact_dir>, decompressed, after checking
    the bytes against the sha256 cell.sh recorded in the execution's RESULT. Returns
    {recorded verify-log directory: (restored directory relative to OUT, None) or
    (None, why none)}. A directory is restored whole or not at all."""
    hashes = (result or {}).get("artifact_sha256")
    entries = {}
    for recorded, found in sorted(logs.items()):
        if isinstance(found, str):
            entries[recorded] = (None, found)
            continue
        rel, prefix, names = found
        if not names:
            entries[recorded] = (None, f"the execution's artifacts hold no {RUN1_LOG}* log from it")
            continue
        if fetch_error is not None:
            entries[recorded] = (None, f"its logs could not be fetched: {fetch_error}")
            continue
        if not isinstance(run_id, str) or not SAFE.fullmatch(run_id):
            entries[recorded] = (None, f"the execution's run id {run_id!r} cannot name a directory")
            continue
        if not isinstance(hashes, dict):
            entries[recorded] = (None, "the execution's result.json records no artifact_sha256, "
                                       "so its logs cannot be checked byte-exact")
            continue
        restored, reason = {}, None
        for name in names:
            try:
                with open(os.path.join(log_dir, name), "rb") as f:
                    blob = f.read()
            except OSError as error:
                reason = f"{name} cannot be read: {error}"
                break
            if hashes.get(name) != hashlib.sha256(blob).hexdigest():
                reason = f"{name} does not have the sha256 its cell recorded"
                break
            if name.endswith(".zst"):
                try:
                    p = subprocess.run(["zstd", "-q", "-d", "-c"], input=blob, capture_output=True)
                    failure = p.stderr[-300:].decode(errors="replace") if p.returncode != 0 else None
                except OSError as error:
                    failure = str(error)
                if failure is not None:
                    reason = f"{name} cannot be decompressed: {failure}"
                    break
                blob = p.stdout
            log = run1_log_name(name, prefix)
            if log in restored:
                reason = f"two artifacts restore to {log}"
                break
            restored[log] = blob
        if reason is None:
            target = os.path.join(LOGS_DIR, run_id, *rel.split("/"))
            try:
                os.makedirs(os.path.join(out, target))
                for log, blob in sorted(restored.items()):
                    with open(os.path.join(out, target, log), "wb") as f:
                        f.write(blob)
            except OSError as error:
                reason = f"its logs cannot be written below {out}: {error}"
        entries[recorded] = (target, None) if reason is None else (None, reason)
    return entries

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--plan", required=True)
    ap.add_argument("--out", required=True, help="IMPORT_DIR: input for test-harness run's import mode only")
    ap.add_argument("--work", help="where fetched artifacts go (default: a temporary directory)")
    ap.add_argument("-j", type=int, default=32)
    ap.add_argument("--local-artifacts", help="buck-out/v2/test/execution: read an execution from here when one directory holds its marker")
    ap.add_argument("run_ids", nargs="+")
    a = ap.parse_args()
    plan = json.load(open(a.plan))
    want = {"{}/{}@{}".format(c["test"], c["mode"], c["backend"]): c for c in plan["cells"]}
    work = a.work or tempfile.mkdtemp(prefix="buck-e2e-ingest-")
    os.makedirs(work, exist_ok=True)  # testx needs the parent of --output-dir to exist
    executions = []  # (cell, end, test id, test run id, marker run id, result)
    for rid in a.run_ids:
        for t in json.loads(testx("--as-json", "results", "list", rid))["results"]["test_results"]:
            name = t["test_details"]["name"]
            # Each target also reports an "unmanaged" twin of the same execution.
            if " - " in name and not name.endswith(" - unmanaged"):
                cell, end, tid = name.split(" - ", 1)[1], int(t["end_time"]), t["test_details"]["id"]
                found = markers(x["name"] for x in t.get("artifacts") or [])
                if len(found) > 1:
                    sys.exit(f"ingest: the execution of {cell} that ended at {end} has {len(found)} run id markers: {found[:5]}")
                executions.append((cell, end, tid, rid, found[0] if found else None, t))
    ends = collections.Counter((cell, end) for cell, end, *_ in executions)
    ties = sorted(f"{cell} at {end}" for (cell, end), n in ends.items() if n > 1)
    if ties:
        sys.exit(f"ingest: executions of one cell ended in the same second, so their order is unknown "
                 f"and testx (RUN.TEST.END) cannot tell their artifacts apart: {ties[:20]} ({len(ties)})")

    local = collections.defaultdict(list)  # run id -> local artifact directories holding its marker
    if a.local_artifacts:
        for dirpath, dirnames, filenames in os.walk(a.local_artifacts):
            if os.path.basename(dirpath) == "artifacts_directory":
                dirnames[:] = []
                found = markers(filenames)
                if len(found) == 1:
                    local[found[0]].append(dirpath)

    def fetch(item):
        cell, end, tid, rid, marker, t = item
        if marker is not None and len(local.get(marker, [])) == 1:
            source, d = "local", local[marker][0]
            names = os.listdir(d)
            rows_name = next((n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None)
            wanted = [n for n in (rows_name, "summary.json", "result.json") if n in names]
        else:
            source = "testx"
            names = [x["name"] for x in t.get("artifacts") or []]
            rows_name = next((n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None)
            wanted = [n for n in (rows_name, "summary.json", "result.json") if n in names]
            d = os.path.join(work, re.sub(r"[^A-Za-z0-9._-]+", "_", f"{cell}__{tid}__{end}"))
            if wanted and not all(os.path.exists(os.path.join(d, n)) for n in wanted):
                args = ["artifacts", "get", f"{rid}.{tid}.{end}", "--output-dir", d]
                for n in wanted:
                    args += ["--artifact-names", n]
                testx(*args)
        rows = rows_of(os.path.join(d, rows_name)) if rows_name else []
        summary = json.load(open(os.path.join(d, "summary.json"))) if "summary.json" in wanted else None
        result = json.load(open(os.path.join(d, "result.json"))) if "result.json" in wanted else None
        # The logs only feed the parity post-pass, so failing to fetch them never fails the ingest.
        logs, log_dir, fetch_error = verify_logs(rows, names), d, None
        log_names = sorted({n for found in logs.values() if not isinstance(found, str) for n in found[2]})
        if source == "testx" and log_names:
            log_dir = d + "__verify-logs"
            if not all(os.path.exists(os.path.join(log_dir, n)) for n in log_names):
                args = ["artifacts", "get", f"{rid}.{tid}.{end}", "--output-dir", log_dir]
                for n in log_names:
                    args += ["--artifact-names", n]
                _, fetch_error = try_testx(*args)
        return cell, end, marker, source, rows, summary, result, (logs, log_dir, fetch_error)

    with cf.ThreadPoolExecutor(a.j) as ex:
        fetched = list(ex.map(fetch, executions))
    sources = collections.Counter(source for _, _, _, source, *_ in fetched)
    per_cell = collections.defaultdict(list)  # cell -> [(rows, summary)] in Tpx order
    complete_runs = collections.defaultdict(list)  # cell -> run ids of evidence-complete executions
    run_cells = {}
    log_sources = []  # (run id, result, (logs, log_dir, fetch_error)) of each execution
    for cell, end, marker, source, rows, summary, result, logs in sorted(fetched, key=lambda x: (x[0], x[1])):
        where = f"the execution of {cell} that ended at {end}"
        if result is not None and result.get("cell") != cell:
            sys.exit(f"ingest: {where} has a result.json for {result.get('cell')}")
        if len(rows) > 1 or any(cell_of(row) != cell for row in rows):
            sys.exit(f"ingest: {where} has {len(rows)} rows, for cells {sorted({cell_of(row) for row in rows})}; "
                     f"a cell runs the harness once (--no-retry), which writes one row, for that cell")
        ids = [row.get("run_id") for row in rows] + ([result.get("run_id")] if result is not None else [])
        ids += [marker] if marker is not None else []
        if len(set(ids)) > 1 or None in ids:
            sys.exit(f"ingest: {where} has run ids {sorted(set(map(str, ids)))}; "
                     f"its rows, its result.json and its {MARKER} marker must all name the same one")
        run_id = ids[0] if ids else None
        if run_id is not None:
            if run_id in run_cells:
                sys.exit(f"ingest: run {run_id} names two executions: {run_cells[run_id]} and {where}")
            run_cells[run_id] = where
        if result is not None and result.get("evidence_complete") is True:
            complete_runs[cell].append(run_id)
        per_cell[cell].append((rows, summary))
        log_sources.append((run_id, result, logs))

    def covered(cell):
        runs = per_cell.get(cell, [])
        return any(rows for rows, _ in runs) or (runs and all(claims_host_inapplicable(cell, s) for _, s in runs))
    missing = sorted(c for c in want if not covered(c))
    extra = sorted(set(per_cell) - set(want))
    if missing or extra:
        sys.exit(f"ingest: cells with neither a row nor a host-inapplicable claim from every execution: "
                 f"{missing[:20]} ({len(missing)}); unexpected cells: {extra[:20]} ({len(extra)})")
    buckets = collections.defaultdict(list)
    summaries = collections.defaultdict(list)
    evidence_complete = collections.defaultdict(list)
    attempts = collections.Counter()
    final = collections.Counter()
    no_row = []  # "cell attempt N" for each execution that wrote no row and claimed no host inapplicability
    for cell, runs in per_cell.items():
        lane, category = want[cell]["lane"], want[cell]["category"]
        final_rows, final_summary = runs[-1]
        if final_summary:
            summaries[(lane, category)].append(final_summary)
        c = want[cell]
        for run_id in complete_runs[cell]:
            evidence_complete[(lane, category)].append(
                {"test": c["test"], "mode": c["mode"], "backend": None if c["backend"] == "native" else c["backend"],
                 "run_id": run_id})
        for attempt, (rows, summary) in enumerate(runs, 1):
            for row in rows:
                buckets[(lane, category)].append(dict(row, attempt=attempt))
                attempts[attempt] += 1
            if not rows and not claims_host_inapplicable(cell, summary):
                no_row.append(f"{cell} attempt {attempt}")
        final[final_rows[-1]["outcome"] if final_rows else
              "HOST-INAPPLICABLE" if claims_host_inapplicable(cell, final_summary) else "NO-ROW"] += 1
    for key in sorted(set(buckets) | set(summaries)):
        lane, category = key
        d = os.path.join(a.out, lane, "manifest_" + category.replace("-", "_"))
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "results.jsonl"), "w") as f:
            for row in sorted(buckets[key], key=lambda r: ("{}/{}@{}".format(r["test"], r["mode"], r.get("backend")), r["attempt"])):
                f.write(json.dumps(row, sort_keys=True) + "\n")
        total = {}
        for one in summaries[key]:
            for k, v in one.items():
                if k == "schema":
                    total[k] = v
                elif isinstance(v, list):
                    total.setdefault(k, []).extend(v)
                elif isinstance(v, (int, float)):
                    total[k] = total.get(k, 0) + v
        total["evidence_complete_executions"] = sorted(
            evidence_complete[key], key=lambda c: (c["test"], c["mode"], c["backend"] or "", c["run_id"]))
        with open(os.path.join(d, "summary.json"), "w") as f:
            json.dump(total, f, indent=2, sort_keys=True)
            f.write("\n")
    logs_root = os.path.join(a.out, LOGS_DIR)
    shutil.rmtree(logs_root, ignore_errors=True)
    os.makedirs(logs_root)  # fails if a previous ingest's logs could not all be removed
    recorded = collections.Counter(r for _, _, (logs, _, _) in log_sources for r in logs)
    index = {}  # recorded verify-log directory -> (restored directory, None) or (None, why none)
    routes = {}  # recorded verify-log directory -> route_of the execution that recorded it
    for run_id, result, (logs, log_dir, fetch_error) in log_sources:
        shared = {r for r in logs if recorded[r] > 1}
        index.update((r, (None, f"{recorded[r]} executions recorded it, so their logs cannot be told apart")) for r in shared)
        index.update(restore(a.out, run_id, result, {r: v for r, v in logs.items() if r not in shared}, log_dir, fetch_error))
        routes.update((r, route_of(None if r in shared else result)) for r in logs)
    with open(os.path.join(logs_root, LOGS_INDEX), "w") as f:
        for r, (restored, reason) in sorted(index.items()):
            f.write(json.dumps({"schema": LOGS_SCHEMA, "verify_log_dir": r, "restored": restored, "reason": reason,
                                **routes[r]}, sort_keys=True) + "\n")
    unrestored = sorted(f"{r}: {reason}" for r, (_, reason) in index.items() if reason is not None)
    print(json.dumps({"sources": dict(sources), "cells": len(per_cell), "buckets": len(set(buckets) | set(summaries)), "rows": sum(attempts.values()),
                      "attempts": dict(attempts), "final_outcomes": dict(final),
                      "no_row_executions": len(no_row), "no_row_examples": sorted(no_row)[:20],
                      "evidence_complete_executions": sum(len(v) for v in evidence_complete.values()),
                      "verify_logs_restored": len(index) - len(unrestored), "verify_logs_unrestored": len(unrestored),
                      "verify_logs_unrestored_examples": unrestored[:5]}))

if __name__ == "__main__":
    main()
