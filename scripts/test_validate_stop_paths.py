#!/usr/bin/env python3
"""Exercise the Rust validate driver's signal traps and ledger writer without a build."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "scripts" / "validate.rs"
TEST_ROOTS: list[Path] = []
DEV_HERMIT = next(parent for parent in ROOT.parents if (parent / "ci-hub" / "ci-hub").is_file())


def process_start_ticks(pid: int) -> int:
    return int(Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19])


def write_fake_process(
    proc_root: Path,
    pid: int,
    *,
    ppid: int,
    pgid: int,
    start_ticks: int,
    argv: tuple[str, ...],
    cgroup: str,
    flags: int = 0,
) -> None:
    process = proc_root / str(pid)
    process.mkdir(parents=True)
    fields = ["0"] * 20
    fields[0] = "S"
    fields[1] = str(ppid)
    fields[2] = str(pgid)
    fields[6] = str(flags)
    fields[19] = str(start_ticks)
    (process / "stat").write_text(f"{pid} (fixture) {' '.join(fields)}\n")
    (process / "cmdline").write_bytes(
        b"\0".join(argument.encode() for argument in argv) + (b"\0" if argv else b"")
    )
    (process / "cgroup").write_text(f"0::{cgroup}\n")


def peer_proc_fixture(tmpdir: Path, *, unresolved: bool = False) -> Path:
    proc_root = tmpdir / "proc"
    proc_root.mkdir()
    write_fake_process(
        proc_root,
        1,
        ppid=0,
        pgid=1,
        start_ticks=1,
        argv=(),
        cgroup="/",
        flags=0x00200000,
    )
    write_fake_process(
        proc_root,
        os.getpid(),
        ppid=1,
        pgid=os.getpid(),
        start_ticks=process_start_ticks(os.getpid()),
        argv=("ci-hub", "validate-lock"),
        cgroup="/user.slice/user@1000.service/app.slice/validate-X.service",
    )
    if unresolved:
        write_fake_process(
            proc_root,
            40,
            ppid=1,
            pgid=40,
            start_ticks=40,
            argv=("/other/scripts/validate.rs", "full"),
            cgroup="/user.slice/user@1000.service/app.slice/validate-Z.service",
        )
        (proc_root / "40" / "stat").write_text("malformed stat\n")
    return proc_root


def stop_test_env(
    tmpdir: Path,
    ledger: Path,
    *,
    lock_proven: bool = False,
    forged_owner: bool = False,
    unresolved_peer: bool = False,
) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = os.environ.copy()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        DEV_HERMIT_PARENT=str(DEV_HERMIT),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
        TMPDIR=str(tmpdir),
    )
    if lock_proven:
        stat_fields = Path(f"/proc/{os.getpid()}/stat").read_text().rsplit(")", 1)[1].split()
        start_ticks = int(stat_fields[19])
        host = socket.gethostname().split(".", 1)[0]
        commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip()
        authority = {
            "schema_version": 1,
            "admissible": True,
            "state": "held",
            "reason_code": None,
            "canonical_anchor_held": True,
            "cleanup_state": "none",
            "holder": {
                "kind": "validate",
                "target": commit,
                "host": host,
            },
            "owner": {
                "host": host,
                "liveness": "alive",
                "pid": os.getpid(),
                "start_ticks": start_ticks,
                "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            },
        }
        env.update(
            VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON=json.dumps(authority),
            # Plant the weaker legacy observation too. Canonical owner ancestry
            # must win; otherwise parked stop-test fixtures make every genuine
            # admitted schema-5 receipt unqualifiable.
            CI_HUB_VALIDATE_CONCURRENT="true",
            VALIDATE_STOP_TEST_PEER_PROC_ROOT=str(
                peer_proc_fixture(tmpdir, unresolved=unresolved_peer)
            ),
            VALIDATE_STOP_TEST_EXCLUSION_LOCK=str(tmpdir / "peer-exclusion.lock"),
            VALIDATE_STOP_TEST_CONTROL_SOCKET=str(tmpdir / "peer-monitor.sock"),
        )
    if forged_owner:
        owner_file = tmpdir / "caller-chosen.owner"
        owner_file.write_text(f"pid={os.getpid()}\n")
        env.update(
            CI_HUB_VALIDATE_LOCK_OWNER_PID=str(os.getpid()),
            CI_HUB_VALIDATE_LOCK_OWNER_FILE=str(owner_file),
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


def assert_schema5_contract(row: dict, *, admitted: bool = False) -> None:
    """A current writer never escapes strict evidence by downgrading schema."""
    assert row["schema_version"] == 5, row
    assert row["repo"] == "hermit", row
    assert row["producer"] == "hermit-validate-rs", row
    if admitted:
        assert row["admission"] == "ci-hub-validate-lock", row
        assert row["concurrent_validates"] == 0, row
        assert row["concurrency_proof"] == "validate_lock_owner_ancestry", row
    else:
        # These direct stop-path tests do not run through validate-lock, so their
        # honest admission result is unknown. A schema-4 fallback would incorrectly
        # grandfather that absence; schema 5 plus null is correctly unqualifiable.
        assert row["admission"] is None, row


def run_signal(
    sig: signal.Signals,
    expect_record: bool,
    *,
    prior_failure: bool = False,
    lock_proven: bool = False,
    forged_owner: bool = False,
) -> None:
    with tempfile.TemporaryDirectory(prefix=f"validate-stop-{sig.name.lower()}-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        env = stop_test_env(
            tmpdir, ledger, lock_proven=lock_proven, forged_owner=forged_owner
        )
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
        assert_schema5_contract(row, admitted=lock_proven)
        assert row["result"] == ("fail" if prior_failure else "no_result"), row
        assert row["raw_result"] == "fail", row
        assert row["gates_run"] == row["checks"] == 2, row
        assert row["failures"] == (1 if prior_failure else 0), row
        assert row["interruption_signal"] == sig.name.removeprefix("SIG"), row


def run_incomplete_exit() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-incomplete-") as tmp:
        tmpdir = Path(tmp)
        # A fixture path named exactly `ledger` must remain an explicit file;
        # basename matching alone must never redirect a caller-selected path to
        # a sibling adapter.
        ledger = tmpdir / "ledger"
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
        assert_schema5_contract(row)
        # An ordinary early exit is not an operator stop. It remains a raw
        # failure unless the producer carries an explicit interruption signal.
        assert row["result"] == "fail", row
        assert row["gates_run"] == 2 and row["gates_expected"] is None, row
        assert row["interruption_signal"] is None, row


def run_canonical_adapter_contract(*, refuse: bool) -> None:
    """Production-shaped writes use the parent adapter, never a raw shadow."""
    with tempfile.TemporaryDirectory(prefix="validate-canonical-adapter-") as tmp:
        tmpdir = Path(tmp)
        canonical_root = tmpdir / "canonical-root"
        if refuse:
            parent = tmpdir / "parent"
            adapter = parent / "ci-hub" / "ledger" / "validate_rows.py"
            adapter.parent.mkdir(parents=True)
            adapter.write_text(
                "import sys\n"
                "sys.stdin.read()\n"
                "print('planted refusal', file=sys.stderr)\n"
                "raise SystemExit(2)\n"
            )
        else:
            # Exercise the REAL parent adapter, redirected only through its
            # explicit stop-test root. This proves the producer/consumer
            # contract without appending the machine's authoritative ledger.
            parent = next(
                candidate
                for candidate in ROOT.parents
                if (candidate / "ci-hub" / "ledger" / "validate_rows.py").is_file()
            )
        raw_shadow = parent / "ignored" / "validate-run-ledger.jsonl"
        raw_before = raw_shadow.read_bytes() if raw_shadow.exists() else None
        env = os.environ.copy()
        env.update(
            HERMIT_VALIDATE_STOP_TEST_MODE="1",
            DEV_HERMIT_PARENT=str(parent),
            CI_HUB_VALIDATE_LEDGER_TEST_ROOT=str(canonical_root),
            VALIDATE_RUN_ON_DIRTY_TREE="1",
            VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            TMPDIR=str(tmpdir),
        )
        env.pop("HERMIT_VALIDATE_LEDGER", None)
        process = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        output = process.stdout.decode(errors="replace")
        assert process.returncode == 1, output
        raw_after = raw_shadow.read_bytes() if raw_shadow.exists() else None
        assert raw_after == raw_before, "canonical write touched the retired raw shadow"
        if refuse:
            assert not list(canonical_root.glob("ledger/**/*.jsonl")), output
            assert "canonical ledger writer" in output and "refused" in output, output
        else:
            shards = list(canonical_root.glob("ledger/hermit/*/*.jsonl"))
            assert len(shards) == 1, (shards, output)
            events = [json.loads(line) for line in shards[0].read_text().splitlines()]
            assert len(events) == 1, events
            assert events[0]["schema"] == "validate-ledger/v1", events[0]
            assert_schema5_contract(events[0]["legacy_row"])
            assert "canonical ledger record appended" in output, output


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
        assert_schema5_contract(rows[0])
        assert rows[0]["result"] == "fail", rows[0]
        assert rows[0]["interruption_signal"] is None, rows[0]


def qualifying(row: dict) -> bool:
    result = subprocess.run(
        [
            "python3",
            str(DEV_HERMIT / "ci-hub" / "qualifying_receipt.py"),
            "--sha",
            row["commit"],
            "--json",
        ],
        input=json.dumps(row),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    try:
        return result.returncode == 0 and json.loads(result.stdout)["accepted"] is True
    except (json.JSONDecodeError, KeyError):
        return False


def inert_publisher(tmpdir: Path) -> tuple[Path, Path]:
    calls = tmpdir / "publisher-calls"
    publisher = tmpdir / "inert-publisher"
    publisher.write_text(
        "#!/bin/sh\n"
        f"printf 'called\\n' >> {calls}\n"
        "exit 0\n"
    )
    publisher.chmod(0o755)
    return publisher, calls


def run_authority_causal_fixture(mode: str) -> tuple[int, int, int]:
    with tempfile.TemporaryDirectory(prefix=f"validate-authority-{mode}-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        release = tmpdir / "success-release"
        publisher, publisher_calls = inert_publisher(tmpdir)
        if mode == "forged-sidecar":
            env = stop_test_env(tmpdir, ledger, forged_owner=True)
        elif mode == "unresolvable-process":
            env = stop_test_env(tmpdir, ledger, lock_proven=True, unresolved_peer=True)
        elif mode == "same-uid-nonowner":
            env = stop_test_env(tmpdir, ledger, lock_proven=True)
        else:
            raise AssertionError(f"unknown authority fixture {mode}")
        env.update(
            VALIDATE_STOP_TEST_SUCCESS_RELEASE="1",
            VALIDATE_STOP_TEST_RELEASE_FILE=str(release),
            VALIDATE_STOP_TEST_COMMIT_ANCHORED="1",
            CI_HUB_APPLY_LOCAL_LABEL=str(publisher),
            PR_NUMBER="999999",
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
            try:
                wait_for_text(log, "VALIDATE_STOP_TEST_READY", process)
                if mode == "same-uid-nonowner":
                    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    client.connect(str(tmpdir / "peer-monitor.sock"))
                    client.sendall(b"final\n")
                    response = json.loads(client.recv(4096))
                    client.close()
                    assert response == {
                        "ok": False,
                        "error": "unauthorized-controller",
                    }, response
                release.write_text("release\n")
                rc = process.wait(timeout=20)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)

        assert rc == 0, (mode, rc, log.read_text(errors="replace"))
        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert len(rows) == 1, (mode, rows)
        row = rows[0]
        assert row["raw_result"] == "pass", row
        qualifying_count = sum(qualifying(candidate) for candidate in rows)
        publisher_count = (
            len(publisher_calls.read_text().splitlines()) if publisher_calls.exists() else 0
        )
        if mode in {"forged-sidecar", "unresolvable-process"}:
            assert row["result"] == "no_result", row
            assert row["concurrency_indeterminate"] is True, row
            assert row["concurrent_validates"] is None, row
            assert row["concurrency_proof"] is None, row
            assert (len(rows), qualifying_count, publisher_count) == (1, 0, 0)
            if mode == "forged-sidecar":
                assert row["admission"] is None, row
            else:
                assert "snapshot-unresolved:process PID 40" in row[
                    "concurrency_indeterminate_detail"
                ], row
        else:
            assert row["concurrency_indeterminate"] is False, row
            assert row["concurrent_validates"] == 0, row
            monitor = row["concurrent_validate_monitor"]
            assert monitor["monitor_sequence"] == monitor["final_ack_sequence"], monitor
            # Positive side of the publication latch: after the unauthorized
            # request was refused, the legitimate controller still finalized
            # and the inert publisher was invoked exactly once.
            assert publisher_count == 1, (row, publisher_count)
        return len(rows), qualifying_count, publisher_count


def main() -> None:
    assert run_authority_causal_fixture("forged-sidecar") == (1, 0, 0)
    assert run_authority_causal_fixture("unresolvable-process") == (1, 0, 0)
    unauthorized = run_authority_causal_fixture("same-uid-nonowner")
    assert unauthorized[0] == 1 and unauthorized[2] == 1, unauthorized
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_signal(signal.SIGTERM, expect_record=True, lock_proven=True)
    # The retired shell contract trusted these caller-selected values. Rust must
    # ignore them: only the canonical authority query can establish admission.
    run_signal(signal.SIGTERM, expect_record=True, forged_owner=True)
    run_incomplete_exit()
    run_canonical_adapter_contract(refuse=False)
    run_canonical_adapter_contract(refuse=True)
    run_cleanup_signal_race()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    print(
        "PASS: forged sidecar 1 diagnostic/0 qualifying/0 publisher; "
        "unresolvable process sticky 1/0/0; same-uid non-owner final refused and "
        "legitimate final published inertly 1/1; TERM/INT/HUP => NO-RESULT; KILL => no record; "
        "prior failure remains fail; forged owner path is unadmitted; canonical adapter "
        "accept/refuse bracketed; cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
