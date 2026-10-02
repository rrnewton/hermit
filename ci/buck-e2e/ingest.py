#!/usr/bin/env python3
"""Rebuild the e2e result files of a Buck test run from its Tpx results.

usage: ingest.py --plan ci/expected-e2e-plan.json --out E2E_RESULT_ROOT [--work DIR]
                 [--local-artifacts BUCK_OUT_TEST_DIR --since EPOCH] TEST_RUN_ID...

TEST_RUN_ID is what `buck2 test --write-test-id FILE` wrote (one per invocation; the
hybrid run makes two, one for RE cells and one for local cells). Every cell execution
uploads the rows test-harness wrote as its results.jsonl Tpx artifact; this fetches
them with testx (TESTX env, default `testx`) and writes the files the cargo flow's e2e
nodes write, one bucket per (lane, category):

    E2E_RESULT_ROOT/<lane>/manifest_<category>/results.jsonl

    E2E_RESULT_ROOT/<lane>/manifest_<category>/summary.json

so the ledger, coverage and verdict consumers read a Buck run exactly as a cargo run.
summary.json aggregates each cell's final execution: counts are summed and cell lists
joined, which is what one harness process over the bucket writes. A host-inapplicable
cell has no row, only its entry in summary.json's host_inapplicable_cells.
Tpx owns retries (cells run the harness with --no-retry): a cell's executions, in Tpx
order, become attempts 1, 2, ..., so "passed only on rerun" stays visible as a failed
attempt 1 followed by a passing attempt 2. Every plan cell must have a row; a cell with
none, or an unexpected cell, is an error. Prints one JSON summary line.

--local-artifacts: Buck materializes each test's artifact directory locally
(buck-out/v2/test/execution/<cell>/<target hash>/<config hash>/default/artifacts_directory),
keeping the newest execution per target. A cell's final execution is read from there
when that directory was written by this run (result.json newer than --since and naming
the same cell); every other execution, and any final one without a local copy, is
fetched with testx.
"""
import argparse, collections, concurrent.futures as cf, json, os, re, subprocess, sys, tempfile, time

TESTX = os.environ.get("TESTX", "testx")

def testx(*args, tries=4):
    for i in range(tries):
        p = subprocess.run([TESTX, *args], capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout
        time.sleep(5 * (i + 1))  # TestX answers 500 transiently
    sys.exit(f"ingest: testx {' '.join(args[:3])} failed: {p.stderr[-500:]}")

def rows_of(path):
    if path.endswith(".zst"):
        text = subprocess.run(["zstd", "-q", "-d", "-c", path], check=True, capture_output=True, text=True).stdout
    else:
        text = open(path).read()
    return [json.loads(l) for l in text.splitlines() if l.strip()]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--plan", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--work", help="where fetched artifacts go (default: a temporary directory)")
    ap.add_argument("-j", type=int, default=32)
    ap.add_argument("--local-artifacts", help="buck-out/v2/test/execution: read final executions from here when fresh")
    ap.add_argument("--since", type=float, default=0.0, help="epoch seconds the Buck run started")
    ap.add_argument("run_ids", nargs="+")
    a = ap.parse_args()
    plan = json.load(open(a.plan))
    want = {"{}/{}@{}".format(c["test"], c["mode"], c["backend"]): c for c in plan["cells"]}
    work = a.work or tempfile.mkdtemp(prefix="buck-e2e-ingest-")
    os.makedirs(work, exist_ok=True)  # testx needs the parent of --output-dir to exist
    executions = []  # (cell, order key, run id, result)
    for rid in a.run_ids:
        for t in json.loads(testx("--as-json", "results", "list", rid))["results"]["test_results"]:
            name = t["test_details"]["name"]
            # Each target also reports an "unmanaged" twin of the same execution.
            if " - " in name and not name.endswith(" - unmanaged"):
                executions.append((name.split(" - ", 1)[1], (int(t["end_time"]), t["test_details"]["id"]), rid, t))

    # The newest local artifact directory per cell, if this run wrote it.
    local = {}
    if a.local_artifacts:
        for dirpath, dirnames, filenames in os.walk(a.local_artifacts):
            if os.path.basename(dirpath) == "artifacts_directory" and "result.json" in filenames:
                dirnames[:] = []
                p = os.path.join(dirpath, "result.json")
                mtime = os.path.getmtime(p)
                if mtime < a.since:
                    continue
                try:
                    cell = json.load(open(p))["cell"]
                except (ValueError, KeyError):
                    continue
                if cell not in local or local[cell][0] < mtime:
                    local[cell] = (mtime, dirpath)
    final_key = {}
    for cell, key, rid, t in executions:
        if cell not in final_key or final_key[cell] < key:
            final_key[cell] = key
    used_local = collections.Counter()

    def fetch(item):
        cell, (end, tid), rid, t = item
        if (end, tid) == final_key[cell] and cell in local:
            d = local[cell][1]
            rows_name = next((n for n in ("results.jsonl", "results.jsonl.zst") if os.path.exists(os.path.join(d, n))), None)
            if rows_name or os.path.exists(os.path.join(d, "summary.json")):
                used_local["local"] += 1
                rows = rows_of(os.path.join(d, rows_name)) if rows_name else []
                summary_path = os.path.join(d, "summary.json")
                summary = json.load(open(summary_path)) if os.path.exists(summary_path) else None
                return cell, end, rows, summary
        used_local["testx"] += 1
        names = [x["name"] for x in t.get("artifacts") or []]
        rows_name = next((n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None)
        wanted = [n for n in (rows_name, "summary.json") if n in names]
        d = os.path.join(work, re.sub(r"[^A-Za-z0-9._-]+", "_", f"{cell}__{tid}__{end}"))
        if wanted and not all(os.path.exists(os.path.join(d, n)) for n in wanted):
            args = ["artifacts", "get", f"{rid}.{tid}.{end}", "--output-dir", d]
            for n in wanted:
                args += ["--artifact-names", n]
            testx(*args)
        rows = rows_of(os.path.join(d, rows_name)) if rows_name else []
        summary = json.load(open(os.path.join(d, "summary.json"))) if "summary.json" in wanted else None
        return cell, end, rows, summary

    with cf.ThreadPoolExecutor(a.j) as ex:
        fetched = list(ex.map(fetch, executions))
    per_cell = collections.defaultdict(list)
    final_summary = {}
    for cell, end, rows, summary in sorted(fetched, key=lambda x: (x[0], x[1])):
        per_cell[cell].append(rows)
        final_summary[cell] = summary
    def host_inapplicable(cell):
        return any("{}/{}@{}".format(h["test"], h["mode"], h.get("backend") or "native") == cell
                   for h in (final_summary.get(cell) or {}).get("host_inapplicable_cells", []))
    missing = sorted(c for c in want if not any(per_cell.get(c, [])) and not host_inapplicable(c))
    extra = sorted(set(per_cell) - set(want))
    if missing or extra:
        sys.exit(f"ingest: cells without rows: {missing[:20]} ({len(missing)}); unexpected cells: {extra[:20]} ({len(extra)})")
    buckets = collections.defaultdict(list)
    summaries = collections.defaultdict(list)
    attempts = collections.Counter()
    final = collections.Counter()
    for cell, runs in per_cell.items():
        lane, category = want[cell]["lane"], want[cell]["category"]
        if final_summary.get(cell):
            summaries[(lane, category)].append(final_summary[cell])
        attempt = 0
        for rows in runs:
            for row in rows:
                attempt += 1
                buckets[(lane, category)].append(dict(row, attempt=attempt))
                attempts[attempt] += 1
        final[runs[-1][-1]["outcome"] if runs[-1] else "HOST-INAPPLICABLE"] += 1
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
        with open(os.path.join(d, "summary.json"), "w") as f:
            json.dump(total, f, indent=2, sort_keys=True)
            f.write("\n")
    print(json.dumps({"sources": dict(used_local), "cells": len(per_cell), "buckets": len(set(buckets) | set(summaries)), "rows": sum(attempts.values()),
                      "attempts": dict(attempts), "final_outcomes": dict(final)}))

if __name__ == "__main__":
    main()
