#!/usr/bin/env python3
"""Mutation checks for validate.rs's historical-debug admission boundary."""

from __future__ import annotations

from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "scripts" / "validate.rs"
HISTORICAL = ROOT / "scripts" / "historical-debug-validate"


def run(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(VALIDATE), *args],
        cwd=ROOT,
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
    assert "running profile" not in result.stdout, result.stdout
    assert "running historical-debug" not in result.stdout, result.stdout


def main() -> None:
    help_result = run("--help")
    assert help_result.returncode == 0, help_result.stdout
    assert "--historical-debug" in help_result.stdout, help_result.stdout
    assert "NON-QUALIFYING" in help_result.stdout, help_result.stdout

    parallel = run("portable", "--historical-debug", "-j", "2")
    assert_refused(parallel, "sequential and requires -j 1")

    unboxed = run("portable", "--historical-debug", "--allow-cgroup-failure")
    assert_refused(unboxed, "requires cgroup boxing")

    direct = run("portable", "--historical-debug", "-j", "1")
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

    print(
        "PASS: historical-debug wrapper is explicit and the producer refuses parallel, "
        "unboxed, and direct invocation"
    )


if __name__ == "__main__":
    main()
