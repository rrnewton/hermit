#!/usr/bin/env python3
"""Exercise validate.sh's real signal traps and ledger writer without a build."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "validate.sh"
TEST_ROOTS: list[Path] = []
SUPPORTED_ENTRYPOINT = "./ci-hub/ci-hub validate-run"


def admission_env() -> dict[str, str]:
    env = os.environ.copy()
    env.pop("CI_HUB_VALIDATE_PRODUCER", None)
    env.pop("GITHUB_ACTIONS", None)
    return env


def run_entrypoint_admission() -> None:
    started = time.monotonic()
    direct = subprocess.run(
        [str(VALIDATE), "--help"],
        cwd=ROOT,
        env=admission_env(),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
        timeout=2,
    )
    elapsed = time.monotonic() - started
    direct_output = direct.stdout.decode(errors="replace")
    assert direct.returncode != 0, direct_output
    assert elapsed < 2, elapsed
    assert "direct invocation is disabled" in direct_output, direct_output
    assert SUPPORTED_ENTRYPOINT in direct_output, direct_output
    assert "Validation level:" not in direct_output, direct_output

    forged_github = admission_env()
    forged_github["CI_HUB_VALIDATE_PRODUCER"] = "github-actions-v1"
    forged = subprocess.run(
        [str(VALIDATE), "--help"],
        cwd=ROOT,
        env=forged_github,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
        timeout=2,
    )
    forged_output = forged.stdout.decode(errors="replace")
    assert forged.returncode != 0, forged_output
    assert "invalid GitHub Actions producer binding" in forged_output, forged_output
    assert SUPPORTED_ENTRYPOINT in forged_output, forged_output

    admitted = admission_env()
    admitted["CI_HUB_VALIDATE_PRODUCER"] = "systemd-user-v1"
    supported = subprocess.run(
        [str(VALIDATE), "--help"],
        cwd=ROOT,
        env=admitted,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
        timeout=2,
    )
    supported_output = supported.stdout.decode(errors="replace")
    assert supported.returncode == 0, supported_output
    assert "Usage: ./validate.sh" in supported_output, supported_output

    github = admission_env()
    github.update(
        CI_HUB_VALIDATE_PRODUCER="github-actions-v1",
        GITHUB_ACTIONS="true",
    )
    github_supported = subprocess.run(
        [str(VALIDATE), "--help"],
        cwd=ROOT,
        env=github,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
        timeout=2,
    )
    github_output = github_supported.stdout.decode(errors="replace")
    assert github_supported.returncode == 0, github_output
    assert "Usage: ./validate.sh" in github_output, github_output


def stop_test_env(tmpdir: Path, ledger: Path) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = os.environ.copy()
    env.update(
        CI_HUB_VALIDATE_PRODUCER="systemd-user-v1",
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        DEV_HERMIT_PARENT=str(ROOT.parent),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
        TMPDIR=str(tmpdir),
    )
    return env


def wait_for_text(log: Path, text: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if log.exists() and text in log.read_text(errors="replace"):
            return
        if process.poll() is not None:
            raise AssertionError(f"validate exited before ready: rc={process.returncode}")
        time.sleep(0.05)
    raise AssertionError(f"validate stop-test hook did not emit {text!r}")


def run_signal(
    sig: signal.Signals, expect_record: bool, *, prior_failure: bool = False
) -> None:
    with tempfile.TemporaryDirectory(prefix=f"validate-stop-{sig.name.lower()}-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_PRIOR_FAILURE="1" if prior_failure else "0",
        )
        with log.open("wb") as output:
            process = subprocess.Popen(
                [str(VALIDATE), "full"],
                cwd=ROOT,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            wait_for_text(log, "VALIDATE_STOP_TEST_READY", process)
            process.send_signal(sig)
            rc = process.wait(timeout=10)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
        if not expect_record:
            assert not rows, (sig.name, rows)
            assert rc == -sig.value, (sig.name, rc)
            return

        assert rc == 130, (sig.name, rc, log.read_text(errors="replace"))
        assert len(rows) == 1, (sig.name, rows)
        row = rows[0]
        assert row["result"] == ("fail" if prior_failure else "no_result"), row
        assert row["raw_result"] == "fail", row
        assert row["gates_run"] == row["checks"] == 2, row
        assert row["failures"] == (1 if prior_failure else 0), row
        assert row["interruption_signal"] == sig.name.removeprefix("SIG"), row


def run_incomplete_exit() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-incomplete-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
        )
        process = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        assert process.returncode == 1, process.stdout.decode(errors="replace")
        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert len(rows) == 1, rows
        row = rows[0]
        # An ordinary early exit is not an operator stop. It remains a raw
        # failure unless the producer carries an explicit interruption signal.
        assert row["result"] == "fail", row
        assert row["gates_run"] == 2 and row["gates_expected"] is None, row
        assert row["interruption_signal"] is None, row


def run_cleanup_signal_race() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-cleanup-race-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        cleanup_ready = tmpdir / "cleanup-ready"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            VALIDATE_STOP_TEST_CLEANUP_READY_FILE=str(cleanup_ready),
            VALIDATE_STOP_TEST_CLEANUP_DELAY_SECONDS="1",
        )
        with log.open("wb") as output:
            process = subprocess.Popen(
                [str(VALIDATE), "full"],
                cwd=ROOT,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not cleanup_ready.exists():
                if process.poll() is not None:
                    raise AssertionError(
                        f"validate exited before cleanup hook: rc={process.returncode}"
                    )
                time.sleep(0.01)
            assert cleanup_ready.exists(), "cleanup hook did not become ready"
            for _ in range(20):
                process.send_signal(signal.SIGTERM)
                time.sleep(0.01)
            rc = process.wait(timeout=10)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert rc == 1, (rc, log.read_text(errors="replace"))
        assert len(rows) == 1, rows
        assert rows[0]["result"] == "fail", rows[0]
        assert rows[0]["interruption_signal"] is None, rows[0]


def main() -> None:
    run_entrypoint_admission()
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_incomplete_exit()
    run_cleanup_signal_race()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    print(
        "PASS: direct validate refuses; admitted validate proceeds; "
        "TERM/INT/HUP => NO-RESULT; KILL => no record; "
        "prior failure remains fail; cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
