#!/usr/bin/env python3
"""Exercise validate's real signal traps and ledger writer without a build.

`./validate.sh` is a shim that `exec`s `scripts/validate.rs`, so this drives the
real driver through the entrypoint every other caller uses.

THE FIXTURE MUST NOT OUTLIVE THIS TEST. Each child is spawned with
``start_new_session=True`` so a signal aimed at the fixture cannot reach the test
runner -- but that also means nothing in the child's new session will ever stop
it if this process dies first (an assertion before the signal, a ``wait``
timeout, or the agent being recycled). Measured on this box 2026-08-07: six
orphaned ``validate.sh full`` process groups, all ``ppid=1``, ages 2h20m to
4h30m, each parked in ``sleep 1`` at CPU/wall ~0.00, silently inflating every
concurrency count that scanned the process table.

Three independent guards close that:

1. every spawn goes through :func:`spawned`, whose ``finally`` kills the child's
   OWN process group -- and only that group, identified by the pid this process
   created, never by a name or pattern match;
2. ``VALIDATE_STOP_TEST_MAX_SECONDS`` bounds the fixture's lifetime from inside;
3. the driver itself exits as soon as it observes ``getppid() == 1``.
"""

from __future__ import annotations

import contextlib
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


def stop_test_env(tmpdir: Path, ledger: Path) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = os.environ.copy()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        DEV_HERMIT_PARENT=str(ROOT.parent),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
        # Backstop: even a fixture this test never reaches must die on its own.
        VALIDATE_STOP_TEST_MAX_SECONDS="120",
        TMPDIR=str(tmpdir),
    )
    return env


@contextlib.contextmanager
def spawned(**kwargs):
    """Run a stop-test fixture, guaranteeing its process group is reaped.

    ``start_new_session=True`` makes the child a session and process-group leader
    whose pgid equals its pid, so ``killpg(child.pid, ...)`` addresses EXACTLY the
    group this test created. That is the only kill this file performs: no name
    match, no pattern, no ``-f`` substring -- up to eighteen agents share this box
    and its binary paths, and a pattern kill would take out their live work.
    """
    process = subprocess.Popen(start_new_session=True, **kwargs)  # noqa: S603
    try:
        yield process
    finally:
        if process.poll() is None:
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.killpg(process.pid, signal.SIGKILL)
            with contextlib.suppress(subprocess.TimeoutExpired):
                process.wait(timeout=5)


def wait_for_text(log: Path, text: str, process: subprocess.Popen[bytes]) -> None:
    # Generous: `./validate.sh` execs a rust-script, so the FIRST invocation on a
    # cold cache compiles the driver before it can print anything.
    deadline = time.monotonic() + 300
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
        with log.open("wb") as output, spawned(
            args=[str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
        ) as process:
            wait_for_text(log, "VALIDATE_STOP_TEST_READY", process)
            process.send_signal(sig)
            rc = process.wait(timeout=30)

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
            timeout=300,
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
        with log.open("wb") as output, spawned(
            args=[str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
        ) as process:
            deadline = time.monotonic() + 60
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
            rc = process.wait(timeout=30)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert rc == 1, (rc, log.read_text(errors="replace"))
        assert len(rows) == 1, rows
        assert rows[0]["result"] == "fail", rows[0]
        assert rows[0]["interruption_signal"] is None, rows[0]


def main() -> None:
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_incomplete_exit()
    run_cleanup_signal_race()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    print(
        "PASS: TERM/INT/HUP => NO-RESULT; KILL => no record; "
        "prior failure remains fail; cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
