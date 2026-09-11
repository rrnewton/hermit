#!/usr/bin/env python3
"""Regression test: the run-matrix TSV is written WHOLE or not at all.

`write_results` used `csv.DictWriter` at its default ``extrasaction="raise"``
against six hardcoded fieldnames.  The row builder carries two shapes -- GAP
rows with six keys, executed rows with a seventh, `evidence` -- so every
executed row raised.  `writerows` streams, so the raise left a syntactically
valid TSV holding the clean PREFIX of the rows.

Measured before the fix, by calling the writer directly:

    all 10 rows carry evidence          ->  0 of 10 data rows written
    3 GAP rows then 7 evidence rows     ->  3 of 10 data rows written
    no row carries evidence             -> 10 of 10 data rows written

The 3-of-10 case is the dangerous one.  The process does exit non-zero, so this
was never silent to a caller reading ``$?``; it was silent at the ARTIFACT
boundary, because a short file is indistinguishable from a small result set.

Both directions are asserted, because either alone proves nothing:

  1. a KNOWN non-column key (`evidence`) is handled -- every row still lands, so
     the guard did not simply start refusing the normal case; and
  2. UNANTICIPATED skew is REPORTED -- a planted unknown key, and a planted
     missing column, each raise `MatrixError` naming the offending field, and
     leave any previous artifact byte-for-byte untouched.

Assertion 2 without 1 would pass for a writer that refuses everything.
Assertion 1 without 2 would pass for the permissive writer that caused the
silent truncation in the first place.

Run: python3 tests/backend-parity/test_run_matrix_output_skew.py
Exit 0 = all assertions pass, 1 = a real failure.
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parent / "run_matrix.py"

# The six columns the TSV has always carried.  Used as a fallback so this test
# can also be pointed at a PRE-FIX `run_matrix.py`, where the constants below do
# not exist yet.  Without the fallback the test would die on an AttributeError
# and "fail" for the wrong reason -- it would prove the constant is missing, not
# that rows are being lost, and a negative control that fires on the wrong
# signal is not a control.
LEGACY_COLUMNS = (
    "test_name",
    "backend",
    "expectation",
    "result",
    "seconds",
    "detail",
)


def declared_columns(module) -> tuple[str, ...]:
    return tuple(getattr(module, "RESULT_COLUMNS", LEGACY_COLUMNS))


def allowed_non_columns(module) -> frozenset[str]:
    return frozenset(getattr(module, "NON_COLUMN_RESULT_KEYS", frozenset()))


def load_module():
    spec = importlib.util.spec_from_file_location("run_matrix", MODULE_PATH)
    assert spec and spec.loader, f"cannot load {MODULE_PATH}"
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def make_row(name: str, *, evidence: bool = False, extra: str | None = None):
    row = {
        "test_name": name,
        "backend": "ptrace",
        "expectation": "pass",
        "result": "PASS",
        "seconds": "0.001",
        "detail": "detail text",
    }
    if evidence:
        row["evidence"] = {"tier": "T1"}
    if extra is not None:
        row[extra] = "planted"
    return row


def data_rows(path: Path) -> int:
    """Data-row count, excluding the header.  -1 when the file is absent."""
    if not path.exists():
        return -1
    lines = path.read_text(encoding="utf-8").splitlines()
    return max(0, len(lines) - 1)


def main() -> int:
    rm = load_module()
    columns = declared_columns(rm)
    non_columns = allowed_non_columns(rm)
    failures: list[str] = []

    def check(condition: bool, message: str) -> None:
        if condition:
            print(f"PASS: {message}")
        else:
            print(f"FAIL: {message}")
            failures.append(message)

    with tempfile.TemporaryDirectory() as directory:
        workdir = Path(directory)

        # --- DIRECTION 1: the normal cases must still write EVERY row. --------
        for label, rows in (
            ("no row carries evidence", [make_row(f"c{i}") for i in range(10)]),
            (
                "every row carries evidence (the all-executed shape)",
                [make_row(f"e{i}", evidence=True) for i in range(10)],
            ),
            (
                "3 GAP rows then 7 evidence rows (realistic ordering)",
                [make_row(f"g{i}") for i in range(3)]
                + [make_row(f"e{i}", evidence=True) for i in range(7)],
            ),
        ):
            target = workdir / f"clean-{len(rows)}-{abs(hash(label))}.tsv"
            # A pre-fix writer RAISES part-way through here.  Catching it keeps
            # the failure reportable as a ROW COUNT -- "wrote 3 of 10" -- which
            # is the defect.  Letting it escape would abort the run and prove
            # only that something threw.
            threw = None
            try:
                rm.write_results(target, rows)
            except Exception as error:  # noqa: BLE001
                threw = f"{type(error).__name__}: {error}"
            written = data_rows(target)
            check(
                threw is None and written == len(rows),
                f"{label}: wrote {written} of {len(rows)} data rows"
                + (f" (raised {threw})" if threw else ""),
            )
            if written <= 0:
                continue
            header = target.read_text(encoding="utf-8").splitlines()[0]
            check(
                header.split("\t") == list(columns),
                f"{label}: header is the declared {len(columns)} columns",
            )
            check(
                "evidence" not in header,
                f"{label}: `evidence` is projected out, not emitted as a column",
            )

        # --- DIRECTION 2: planted skew must be REPORTED, not written. --------
        # A pre-existing artifact is planted first, so the test can prove the
        # refusal leaves it untouched rather than truncating it.
        preserved = workdir / "preserved.tsv"
        rm.write_results(preserved, [make_row(f"p{i}") for i in range(4)])
        before = preserved.read_bytes()
        check(data_rows(preserved) == 4, "planted prior artifact holds 4 of 4 rows")

        for label, rows, needle in (
            (
                "unexpected extra field",
                [make_row("ok")] + [make_row("bad", extra="tier_evidence")],
                "tier_evidence",
            ),
            (
                "missing declared column",
                [make_row("ok"), {k: v for k, v in make_row("bad").items() if k != "detail"}],
                "detail",
            ),
        ):
            raised = None
            try:
                rm.write_results(preserved, rows)
            except getattr(rm, "MatrixError", ()) as error:
                raised = str(error)
            except Exception as error:  # noqa: BLE001 - any other type is a bug
                raised = f"WRONG EXCEPTION TYPE {type(error).__name__}: {error}"

            check(
                raised is not None and not raised.startswith("WRONG EXCEPTION"),
                f"{label}: raises MatrixError",
            )
            check(
                raised is not None and needle in raised,
                f"{label}: the refusal NAMES the offending field ({needle!r})",
            )
            check(
                preserved.read_bytes() == before,
                f"{label}: prior artifact left byte-for-byte untouched "
                f"(still {data_rows(preserved)} of 4 rows)",
            )
            check(
                not (preserved.parent / f"{preserved.name}.partial").exists(),
                f"{label}: no .partial scratch file left behind",
            )

        # --- The guard must not be inert: the planted row really is skew the
        # --- old writer would have choked on, and the new one names it.
        check(
            {"attempt_outcome", "evidence"}.issubset(non_columns)
            and "tier_evidence" not in non_columns,
            "allowlist admits typed attempt outcomes and `evidence`, but not the planted key",
        )

        # The scheduler must consume a producer-owned file, not a line that a
        # test can counterfeit on stdout. The file is exact, atomic and absent
        # when no scheduler-owned destination was provided.
        counts = workdir / "counts.json"
        prior = os.environ.get("DAGRUN_TEST_COUNTS_PATH")
        os.environ["DAGRUN_TEST_COUNTS_PATH"] = str(counts)
        try:
            terminal_rows = [
                {
                    "test_name": "passes",
                    "backend": "dbt",
                    "result": "PASS",
                    "attempt_outcome": "passed",
                },
                {
                    "test_name": "fails",
                    "backend": "dbt",
                    "result": "FAIL",
                    "attempt_outcome": "failed",
                    "detail": "ordinary exit 17",
                },
                {
                    "test_name": "wall-times-out",
                    "backend": "dbt",
                    "result": "FAIL",
                    "attempt_outcome": "wall_timeout",
                    "detail": "verify run timed out",
                },
                {
                    "test_name": "infrastructure",
                    "backend": "dbt",
                    "result": "ERROR",
                    "attempt_outcome": "infrastructure_error",
                    "detail": "verification report was unreadable",
                },
                {
                    "test_name": "blocked",
                    "backend": "dbt",
                    "result": "BLOCKED",
                    "attempt_outcome": "no_result",
                    "detail": "backend capability unavailable",
                },
            ]
            rm.write_structured_test_results(
                [
                    *terminal_rows,
                    {
                        "test_name": "configured-gap",
                        "backend": "dbt",
                        "result": "GAP",
                        "seconds": "0.000",
                        "detail": "configured gap",
                    },
                ],
                5,
                1,
                "strict",
            )
        finally:
            if prior is None:
                os.environ.pop("DAGRUN_TEST_COUNTS_PATH", None)
            else:
                os.environ["DAGRUN_TEST_COUNTS_PATH"] = prior
        check(
            json.loads(counts.read_text(encoding="utf-8"))
            == {
                "schema": 3,
                "executed_tests": 5,
                "filtered_tests": 1,
                "results": [
                    {
                        "id": "backend-parity/passes [dbt/strict]",
                        "result": "pass",
                        "attempts": 1,
                        "attempt_results": [
                            {"attempt": 1, "outcome": "passed", "detail": None}
                        ],
                    },
                    {
                        "id": "backend-parity/fails [dbt/strict]",
                        "result": "fail",
                        "attempts": 1,
                        "attempt_results": [
                            {
                                "attempt": 1,
                                "outcome": "failed",
                                "detail": "ordinary exit 17",
                            }
                        ],
                    },
                    {
                        "id": "backend-parity/wall-times-out [dbt/strict]",
                        "result": "fail",
                        "attempts": 1,
                        "attempt_results": [
                            {
                                "attempt": 1,
                                "outcome": "wall_timeout",
                                "detail": "verify run timed out",
                            }
                        ],
                    },
                    {
                        "id": "backend-parity/infrastructure [dbt/strict]",
                        "result": "fail",
                        "attempts": 1,
                        "attempt_results": [
                            {
                                "attempt": 1,
                                "outcome": "infrastructure_error",
                                "detail": "verification report was unreadable",
                            }
                        ],
                    },
                    {
                        "id": "backend-parity/blocked [dbt/strict]",
                        "result": "fail",
                        "attempts": 1,
                        "attempt_results": [
                            {
                                "attempt": 1,

        # The typed cause is load-bearing. Removing it or contradicting the
        # terminal result must refuse instead of falling back to FAIL prose.
        for label, mutated, needle in (
            (
                "missing typed cause",
                [{
                    "test_name": "missing",
                    "backend": "dbt",
                    "result": "FAIL",
                    "detail": "run timed out",
                }],
                "no valid typed attempt outcome",
            ),
            (
                "pass with failure cause",
                [{
                    "test_name": "contradiction",
                    "backend": "dbt",
                    "result": "PASS",
                    "attempt_outcome": "wall_timeout",
                    "detail": "run timed out",
                }],
                "says PASS but its attempt outcome is wall_timeout",
            ),
        ):
            refused = None
            try:
                rm.write_structured_test_results(mutated, 1, 0, "strict")
            except getattr(rm, "MatrixError", ()) as error:
                refused = str(error)
            check(
                refused is not None and needle in refused,
                f"{label}: structured writer refuses by typed cause",
            )

        # Exercise the real timeout-return arm, not only a planted result row.
        original_run = rm.run_with_timeout
        rm.run_with_timeout = lambda _command: None
        try:
            timed_out = rm.run_case(
                Path("/planted/hermit"),
                "dbt",
                "hello_stdout",
                rm.CatalogFixtures(),
                strict=True,
                host_capabilities={
                    "cpuid-faulting": {"present": True, "evidence": "fixture"}
                },
                evidence={},
            )
        finally:
            rm.run_with_timeout = original_run
        check(
            timed_out[0] == "FAIL"
            and timed_out[1] == "ptrace reference timed out"
            and timed_out[3] == "wall_timeout",
            "real backend-parity timeout arm emits wall_timeout, not failed",
        )
                                "outcome": "no_result",
                                "detail": "backend capability unavailable",
                            }
                        ],
                    },
                ],
            },
            "structured DBT results retain exact pass, failure, timeout, infrastructure, and no-result causes",
        )
        check(
            not list(workdir.glob("counts.json.tmp.*")),
            "structured DBT count leaves no temporary file",
        )

    print()
    if failures:
        print(f"{len(failures)} assertion(s) failed")
        return 1
    print("all assertions passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
