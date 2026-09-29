#!/usr/bin/env python3
"""Regression test: `run_matrix.py --case` / `--exclude-case` select cases
without inventing outcomes.

The hosted-portable DBT parity node passes `--exclude-case cpuid_policy`
because GitHub-hosted runners have no CPUID faulting, so the ptrace reference
for that case is BLOCKED and the matrix would publish the dbt row as a
structured failure. The omission has to be a selection, not a relabelled
result:

  1. an omitted case is not run, writes no structured row, and is counted
     neither as executed nor as filtered -- so it cannot read as a pass or as
     a skip;
  2. every other case still runs and is reported exactly as before;
  3. an unknown, repeated, or conflicting name, or a selection that leaves
     nothing, refuses the run instead of silently choosing another
     population.

Assertion 1 without 2 would pass for a selector that drops everything, and 2
without 1 would pass for the old command. Both are checked against the real
`main()`, with only the guest-running functions replaced.

Run: python3 tests/backend-parity/test_run_matrix_case_selection.py
Exit 0 = all assertions pass, 1 = a real failure.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parent / "run_matrix.py"


def load_module():
    spec = importlib.util.spec_from_file_location("run_matrix_case_selection", MODULE_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def check(condition: bool, message: str, failures: list[str]) -> None:
    if not condition:
        failures.append(message)


def refused(module, names, cases, exclude_cases) -> str | None:
    try:
        module.select_cases(names, cases, exclude_cases)
    except module.MatrixError as error:
        return str(error)
    return None


def run_main(module, argv: list[str], counts: Path) -> tuple[int, list[tuple[str, str]]]:
    """Run the real `main()` with every guest-running function replaced."""
    ran: list[tuple[str, str]] = []

    def fake_run_case(hermit, backend, name, *rest):
        ran.append((backend, name))
        return "PASS", "stubbed", 0.0

    module.run_case = fake_run_case
    module.backend_block = lambda backend, hermit, strict: None
    module.read_host_capabilities = lambda hermit: {
        "cpuid-faulting": {"present": False},
        "kvm": {"present": True},
    }
    saved_argv = sys.argv
    saved_counts = os.environ.get("DAGRUN_TEST_COUNTS_PATH")
    sys.argv = ["run_matrix.py", *argv]
    os.environ["DAGRUN_TEST_COUNTS_PATH"] = str(counts)
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            status = module.main()
    finally:
        sys.argv = saved_argv
        if saved_counts is None:
            os.environ.pop("DAGRUN_TEST_COUNTS_PATH", None)
        else:
            os.environ["DAGRUN_TEST_COUNTS_PATH"] = saved_counts
    return status, ran


def main() -> int:
    failures: list[str] = []
    module = load_module()
    catalogue = module.validate_catalog()
    check("cpuid_policy" in catalogue, "catalogue lost cpuid_policy", failures)

    selected, omitted = module.select_cases(catalogue, None, ["cpuid_policy"])
    check(omitted == ["cpuid_policy"], f"omitted {omitted}", failures)
    check(
        selected == [name for name in catalogue if name != "cpuid_policy"],
        "exclusion changed the order or dropped another case",
        failures,
    )
    selected, omitted = module.select_cases(catalogue, ["cpuid_policy"], None)
    check(selected == ["cpuid_policy"], f"--case selected {selected}", failures)
    check(len(omitted) == len(catalogue) - 1, "--case omitted the wrong count", failures)
    selected, omitted = module.select_cases(catalogue, None, None)
    check(selected == catalogue and omitted == [], "no flag must select everything", failures)

    for cases, excludes, expected in [
        (None, ["cpuid_polcy"], "--exclude-case 'cpuid_polcy' names no backend-parity case"),
        (["nope"], None, "--case 'nope' names no backend-parity case"),
        (None, ["cpuid_policy", "cpuid_policy"], "--exclude-case cpuid_policy was given twice"),
        (["cpuid_policy"], ["virtual_pid"], "--case and --exclude-case cannot be used together"),
        (None, list(catalogue), "--exclude-case leaves no backend-parity case to run"),
    ]:
        message = refused(module, catalogue, cases, excludes)
        check(message == expected, f"{cases}/{excludes}: refused with {message!r}", failures)

    with tempfile.TemporaryDirectory(prefix="run-matrix-case-selection-") as tempdir:
        counts = Path(tempdir) / "counts.json"
        base = [
            "--hermit",
            "/bin/true",
            "--backend",
            "dbt",
            "--strict",
            "--require-backend",
            "--no-parent-scorecard",
        ]
        gaps = sum(1 for backend, _ in module.L1_GAPS if backend == "dbt")

        status, ran = run_main(load_module(), base, counts)
        full = json.loads(counts.read_text())
        check(status == 0, f"full catalogue exited {status}", failures)
        check(("dbt", "cpuid_policy") in ran, "full catalogue skipped cpuid_policy", failures)
        check(
            full["executed_tests"] == len(catalogue) - gaps,
            f"full executed {full['executed_tests']}",
            failures,
        )

        counts.unlink()
        status, ran = run_main(load_module(), [*base, "--exclude-case", "cpuid_policy"], counts)
        hosted = json.loads(counts.read_text())
        ids = [row["id"] for row in hosted["results"]]
        check(status == 0, f"hosted selection exited {status}", failures)
        check(("dbt", "cpuid_policy") not in ran, "omitted case was run", failures)
        check(
            not any("cpuid_policy" in identity for identity in ids),
            f"omitted case wrote a row: {ids}",
            failures,
        )
        check(
            hosted["executed_tests"] == full["executed_tests"] - 1,
            f"hosted executed {hosted['executed_tests']}, full {full['executed_tests']}",
            failures,
        )
        check(
            hosted["filtered_tests"] == full["filtered_tests"],
            "an omitted case must not be counted as filtered",
            failures,
        )
        check(
            sorted(ids)
            == sorted(
                row["id"]
                for row in full["results"]
                if row["id"] != "backend-parity/cpuid_policy [dbt/strict]"
            ),
            "the remaining rows changed",
            failures,
        )

    # The committed hosted command is accepted by the real CLI without a
    # guest, and a misspelling is refused with exit 2.
    command = [sys.executable, str(MODULE_PATH), "--check", "--backend", "dbt"]
    accepted = subprocess.run(
        [*command, "--exclude-case", "cpuid_policy"], capture_output=True, text=True, check=False
    )
    check(accepted.returncode == 0, f"--check exited {accepted.returncode}", failures)
    check(
        "omitted cpuid_policy (not run, no result row)" in accepted.stdout,
        f"--check did not report the omission: {accepted.stdout!r}",
        failures,
    )
    misspelled = subprocess.run(
        [*command, "--exclude-case", "cpuid"], capture_output=True, text=True, check=False
    )
    check(misspelled.returncode == 2, f"misspelled exit {misspelled.returncode}", failures)
    check(
        "--exclude-case 'cpuid' names no backend-parity case" in misspelled.stderr,
        f"misspelled stderr {misspelled.stderr!r}",
        failures,
    )

    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    if failures:
        return 1
    print("PASS: run_matrix.py case selection omits without inventing outcomes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
