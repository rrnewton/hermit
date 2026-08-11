#!/usr/bin/env python3
"""Bracket validate.rs's historical-debug admission contract."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "scripts" / "validate.rs"
HISTORICAL = ROOT / "scripts" / "historical-debug-validate"


def run_validate(
    *args: str, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(VALIDATE), *args],
        cwd=ROOT,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=30,
    )


def run_historical(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(HISTORICAL), *args],
        cwd=ROOT,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=10,
    )


def assert_refused(result: subprocess.CompletedProcess[str], message: str) -> None:
    assert result.returncode == 2, result.stdout
    assert message in result.stdout, result.stdout
    assert "running historical-debug" not in result.stdout, result.stdout


def find_adapter() -> Path:
    return next(
        candidate / "ci-hub" / "ledger" / "validate_rows.py"
        for candidate in ROOT.parents
        if (candidate / "ci-hub" / "ledger" / "validate_rows.py").is_file()
    )


def ledger_events(root: Path) -> list[dict]:
    paths = [
        *root.glob("ignored/ci-hub/validate-ledger-spool/*.jsonl"),
        *root.glob("ledger/hermit/*/*.jsonl"),
    ]
    return [json.loads(line) for path in paths for line in path.read_text().splitlines()]


def assert_typed_nonqualifying_row() -> None:
    """The accepted path writes one row whose type itself prevents qualification."""
    with tempfile.TemporaryDirectory(prefix="validate-historical-") as tmp:
        test_root = Path(tmp)
        env = os.environ.copy()
        env.update(
            CI_HUB_HISTORICAL_DEBUG_PRODUCER="validate-lock-bench-v1",
            HERMIT_VALIDATE_STOP_TEST_MODE="1",
            VALIDATE_STOP_TEST_LEDGER_TOOL=str(find_adapter()),
            CI_HUB_VALIDATE_LEDGER_TEST_ROOT=str(test_root),
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            VALIDATE_STOP_TEST_TMP_ROOT=str(test_root / "validation"),
            DEV_HERMIT_PARENT=str(ROOT.parents[2]),
            TMPDIR=str(test_root),
        )
        result = run_validate(
            "portable-only", "--historical-debug", "--no-label-pr", "-j", "1", env=env
        )
        assert result.returncode == 1, result.stdout
        events = ledger_events(test_root)
        assert len(events) == 1, (events, result.stdout)
        event = events[0]
        assert event["schema"] == "validate-ledger/v1", event
        row = event["legacy_row"]
        assert row["schema_version"] == 5, row
        assert row["producer"] == "hermit-validate-rs", row
        assert row["profile"] == "portable-only", row
        assert row["selection_mode"] == "historical-debug", row
        assert row["evidence_class"] == "historical-debug", row
        assert row["landing_eligible"] is False, row
        assert row["non_qualifying_reason"] == "historical-debug", row
        assert row["admission"] is None, row


def main() -> None:
    help_result = run_validate("--help")
    assert help_result.returncode == 0, help_result.stdout
    assert "--historical-debug" in help_result.stdout, help_result.stdout
    assert "NON-QUALIFYING" in help_result.stdout, help_result.stdout

    parallel = run_validate("portable-only", "--historical-debug", "-j", "2")
    assert_refused(parallel, "sequential and requires -j 1")

    unboxed = run_validate("portable-only", "--historical-debug", "--allow-cgroup-failure")
    assert_refused(unboxed, "requires cgroup boxing")

    direct = run_validate("portable-only", "--historical-debug", "-j", "1")
    assert_refused(direct, "use ./scripts/historical-debug-validate")

    wrapper_help = run_historical("--help")
    assert wrapper_help.returncode == 0, wrapper_help.stdout
    assert "box-exclusive validate/bench lock" in wrapper_help.stdout
    assert "NON-QUALIFYING" in wrapper_help.stdout

    malformed_target = run_historical(
        "--checkout", str(ROOT), "--agent", "test", "--target", "not-a-sha"
    )
    assert malformed_target.returncode == 2, malformed_target.stdout
    assert "exact lowercase 40-hex SHA" in malformed_target.stdout

    assert_typed_nonqualifying_row()
    print(
        "PASS: historical-debug is serialized, sequential, boxed, and typed "
        "NON-QUALIFYING at the canonical adapter"
    )


if __name__ == "__main__":
    main()
