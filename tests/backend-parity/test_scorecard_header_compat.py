#!/usr/bin/env python3
"""Regression test: the outer scorecard's schema is owned by the PARENT.

`run_matrix.py` appends live parity observations to the dev-hermit workspace's
`compat-envelope/scorecard.csv`.  That file's schema belongs to the parent, and
the parent adds columns without touching Hermit.  The consumer used to demand
exact tuple equality with its own `SCORECARD_HEADER`, so when the parent added
`verify_compare` every Hermit validate that reached `test.dbi_parity` died --
with no Hermit-side change, AFTER running the whole matrix, and with a message
naming a header while every parity cell had actually passed.

Two things are asserted here, and the second is the one that bites quietly:

  1. a wider parent header is ACCEPTED (the reported outage), and
  2. rows are written at the FILE's width, so values stay in their columns.
     Merely relaxing the equality check while still writing the 19-name
     `SCORECARD_HEADER` would append short rows under a 20-column header and
     silently shift every field after `reason`.

Fail-closed is preserved and narrowed: a column this producer WRITES must
exist, and the refusal names it.

Run: python3 tests/backend-parity/test_scorecard_header_compat.py
Exit 0 = all assertions pass, 1 = a real failure.
"""

from __future__ import annotations

import csv
import importlib.util
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run_matrix", HERE / "run_matrix.py")
assert spec and spec.loader
run_matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run_matrix)

LEGACY_19 = (
    "run_id,run_utc,hermit_sha,reverie_sha,dirty,run_mode,lane,bucket,test_id,"
    "test_mode,backend,cell_state,outcome,deterministic,parity,output_hash,"
    "duration_ms,max_rss_kb,reason"
)
CURRENT_20 = LEGACY_19 + ",verify_compare"
RENAMED_20 = CURRENT_20.replace(",parity,", ",stdout_parity,")

# A planted dbi PASS and a planted dbi FAIL. #323: the point is not that the
# writer runs, it is that a REAL pass reads back as pass and a REAL diff reads
# back as fail -- a schema skew that silently swaps those is the failure mode
# this whole gate exists to prevent.
PLANTED = [
    {
        "result": "PASS",
        "backend": "dbi",
        "test_name": "planted-dbi-pass",
        "expectation": "pass",
        "seconds": "1.0",
        "detail": "planted genuine dbi parity",
    },
    {
        "result": "FAIL",
        "backend": "dbi",
        "test_name": "planted-dbi-diff",
        "expectation": "pass",
        "seconds": "2.0",
        "detail": "planted genuine dbi divergence",
    },
]

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if ok:
        print(f"  \033[32mok\033[0m    {label}")
    else:
        print(f"  \033[31mFAIL\033[0m  {label}{(' -- ' + detail) if detail else ''}")
        FAILURES.append(label)


def append(header: str | None, *, seed_row: str | None = None) -> tuple[Path, object]:
    """Write a scorecard with `header`, append the planted rows, return (path, err)."""
    tmp = Path(tempfile.mkdtemp(prefix="scorecard-compat-"))
    path = tmp / "scorecard.csv"
    if header is not None:
        body = header + "\n" + (seed_row + "\n" if seed_row else "")
        path.write_text(body, encoding="utf-8")
    err = None
    try:
        run_matrix.append_parent_scorecard(
            path,
            [dict(r) for r in PLANTED],
            strict=True,
            verify=True,
            probe_gaps=False,
        )
    except Exception as exc:  # noqa: BLE001 - the refusal is the thing under test
        err = exc
    return path, err


def read_planted(path: Path) -> dict[str, dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    return {r["test_id"].split("/")[-1]: r for r in rows if r.get("test_id")}


def parity_of(row: dict[str, str]) -> str | None:
    # getattr, not attribute access: this test must also be runnable against the
    # PRE-FIX run_matrix.py (which has no PARITY_COLUMNS) so the not-inert
    # comparison is a like-for-like run rather than an import error.
    for name in getattr(run_matrix, "PARITY_COLUMNS", ("parity", "stdout_parity")):
        if name in row and row[name] is not None:
            return row[name]
    return None


print("case CURRENT-20 — parent added verify_compare (THE REPORTED OUTAGE)")
path, err = append(CURRENT_20)
check("append is accepted, not refused", err is None, repr(err))
if err is None:
    got = read_planted(path)
    check("planted dbi PASS reads outcome=pass", got["planted-dbi-pass"]["outcome"] == "pass")
    check("planted dbi PASS reads parity=1", parity_of(got["planted-dbi-pass"]) == "1")
    check("planted dbi FAIL reads outcome=fail", got["planted-dbi-diff"]["outcome"] == "fail")
    check("planted dbi FAIL reads parity=0", parity_of(got["planted-dbi-diff"]) == "0")
    check("backend column says dbi", got["planted-dbi-pass"]["backend"] == "dbi")
    # Alignment: the latent short-write bug.
    widths = {
        len(r) for r in csv.reader(path.read_text(encoding="utf-8").splitlines()) if r
    }
    check("every row is 20 fields wide (no short write)", widths == {20}, str(widths))
    check(
        "reason did not shift into verify_compare",
        got["planted-dbi-pass"]["verify_compare"] == "",
        repr(got["planted-dbi-pass"].get("verify_compare")),
    )
    check(
        "reason still holds the detail",
        "planted genuine dbi parity" in got["planted-dbi-pass"]["reason"],
    )

print("case LEGACY-19 — a parent file predating verify_compare still works")
path, err = append(LEGACY_19)
check("append is accepted", err is None, repr(err))
if err is None:
    got = read_planted(path)
    check("PASS still reads pass", got["planted-dbi-pass"]["outcome"] == "pass")
    check("FAIL still reads fail", got["planted-dbi-diff"]["outcome"] == "fail")
    widths = {
        len(r) for r in csv.reader(path.read_text(encoding="utf-8").splitlines()) if r
    }
    check("rows are 19 fields wide", widths == {19}, str(widths))

print("case RENAMED — forward-compat with parity -> stdout_parity")
path, err = append(RENAMED_20)
check("append is accepted", err is None, repr(err))
if err is None:
    got = read_planted(path)
    check("parity value landed in stdout_parity", got["planted-dbi-pass"].get("stdout_parity") == "1")
    check("FAIL landed as 0", got["planted-dbi-diff"].get("stdout_parity") == "0")

print("case PRESERVE — an existing parent row keeps its verify_compare value")
seed = (
    "prior-run,@1,abc,unknown,false,regression,portable,backend-parity,"
    "backend-parity/prior,verify,ptrace,enabled,pass,1,1,,10,,prior detail,BITWISE"
)
path, err = append(CURRENT_20, seed_row=seed)
check("append is accepted", err is None, repr(err))
if err is None:
    with path.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    prior = next(r for r in rows if r["run_id"] == "prior-run")
    check("pre-existing verify_compare survives", prior["verify_compare"] == "BITWISE")

print("case REFUSAL — a column this producer WRITES is missing (fail-closed)")
path, err = append(CURRENT_20.replace(",outcome,", ","))
check("refused", isinstance(err, run_matrix.MatrixError), repr(err))
check("names the missing column", "outcome" in str(err), str(err))
check("carries the header size (#319)", "column(s):" in str(err), str(err))

print("case FRESH — an absent file is created at the canonical schema")
path, err = append(None)
check("append is accepted", err is None, repr(err))
if err is None:
    hdr = path.read_text(encoding="utf-8").splitlines()[0]
    # The canonical schema grew from 20 to 23 when the tier-evidence columns
    # landed: a bare `deterministic=1` cannot say WHICH comparison earned it, so
    # the verdict now travels with its strictness, its parity boolean and the
    # counts that make the boolean falsifiable.
    #
    # 23 -> 24 when the default destination became a fresh per-run file. Until
    # then this producer only ever APPENDED to a parent that already carried
    # `comparison_tier`, so omitting it from the created header was invisible.
    # Creating the file made it load-bearing: `extrasaction="ignore"` drops any
    # column the file lacks, so a 23-column header silently discarded the tier on
    # the way out and every folded-in row would have been untiered — the exact
    # defect this producer was fixed to stop emitting, reintroduced by the
    # redirect. A created file must be publishable as-is.
    check(
        "created header carries the tier-evidence columns",
        hdr.endswith(",verify_compare,bitwise_parity,compared_log_messages,tier,comparison_tier"),
        hdr,
    )
    check("created header is 24 columns", len(hdr.split(",")) == 24, hdr)
    check(
        "a CREATED file's rows are tiered, so it can be folded in without repair",
        all(
            (row.get("comparison_tier") or "").strip() == run_matrix.COMPARISON_TIER
            for row in read_planted(path).values()
        ),
        str({k: v.get("comparison_tier") for k, v in read_planted(path).items()}),
    )

print("case COMPARISON-TIER — every appended row states its comparison standard")
# WHY. `restval=""` fills any column the outer file has and this producer does
# not populate, so `comparison_tier` -- a column the PARENT added and this
# producer had never heard of -- was written BLANK on every row. The parent
# refuses a blank outright, because an untiered row is an unqualified green: a
# verdict with no record of what comparison produced it. Measured before the
# fix: six runs appended 168 rows to the parent scorecard, 168 blank-tier,
# reddening the parent's ci-hub shard.
#
# The parent owns this vocabulary (compat-envelope/check-scorecard-tier.py).
# It is restated rather than imported because Hermit must build standalone, and
# the two QUALIFYING values are listed separately so this test fails loudly if
# this producer ever starts minting a green it cannot earn.
PARENT_QUALIFYING_TIERS = {
    "full-stdout-info-stack-heap",
    "stdout-info-stack-heap-spot-check",
}
PARENT_UNQUALIFIED_TIERS = {
    "legacy-unqualified",
    "unqualified-stdout-only",
    "unqualified-tool-count-only",
}
TIERED_HEADER = CURRENT_20 + ",comparison_tier"
path, err = append(TIERED_HEADER)
check("append is accepted", err is None, repr(err))
if err is None:
    with path.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    tiers = [(r.get("comparison_tier") or "").strip() for r in rows]
    blank = sum(1 for t in tiers if not t)
    # The count travels with the claim (#319): "no blanks" out of how many?
    check(f"all {len(tiers)} appended rows are tiered (blank={blank})", blank == 0)
    check(
        "every tier is a value the parent knows",
        all(t in PARENT_QUALIFYING_TIERS | PARENT_UNQUALIFIED_TIERS for t in tiers),
        str(sorted(set(tiers))),
    )
    # This harness records no stack/heap evidence, so it must NEVER mint green.
    check(
        "no row claims a QUALIFYING (green) tier",
        not any(t in PARENT_QUALIFYING_TIERS for t in tiers),
        str(sorted(set(tiers))),
    )
    check(
        "the failing row is tiered too, not just the passing one",
        (read_planted(path)["planted-dbi-diff"].get("comparison_tier") or "").strip()
        in PARENT_UNQUALIFIED_TIERS,
    )

# NOT INERT. An assertion that cannot fail is not evidence, so plant the exact
# defect -- a producer that emits a blank tier -- and confirm the check above
# catches it. The fixture is inert: it mutates only this in-process module and
# writes to a temp file, and a blank tier is data, never an authorization.
saved = run_matrix.COMPARISON_TIER
try:
    run_matrix.COMPARISON_TIER = ""
    path, err = append(TIERED_HEADER)
    with path.open(newline="", encoding="utf-8") as fh:
        regressed = [(r.get("comparison_tier") or "").strip() for r in csv.DictReader(fh)]
    check(
        f"planting a blank tier DOES produce blanks ({sum(1 for t in regressed if not t)}"
        f"/{len(regressed)}) — the check above is not vacuous",
        err is None and len(regressed) > 0 and all(not t for t in regressed),
        repr(err) if err else str(regressed),
    )
finally:
    run_matrix.COMPARISON_TIER = saved

print("case UNTIERED-PARENT — a file without the column is still accepted")
# Backward compatibility, and the reason `comparison_tier` is NOT added to
# SCORECARD_HEADER: that would put it in PRODUCED_COLUMNS and make it REQUIRED,
# turning "this producer learned to record more" into a hard refusal of every
# older parent scorecard -- the exact fleet outage this module exists to prevent.
path, err = append(CURRENT_20)
check("append is accepted", err is None, repr(err))
if err is None:
    hdr = path.read_text(encoding="utf-8").splitlines()[0]
    check("no comparison_tier column was invented", "comparison_tier" not in hdr, hdr)
    widths = {
        len(r) for r in csv.reader(path.read_text(encoding="utf-8").splitlines()) if r
    }
    check("rows are still 20 fields wide (no widening)", widths == {20}, str(widths))

print()
if FAILURES:
    print(f"FAIL ({len(FAILURES)} assertions)")
    sys.exit(1)
print("PASS")
