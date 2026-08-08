#!/usr/bin/env python3
"""Exercise validate.sh's real signal traps and ledger writer without a build."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "validate.sh"
TEST_ROOTS: list[Path] = []
DEV_HERMIT_PARENT = next(
    (
        parent
        for parent in ROOT.parents
        if (parent / "ci-hub" / "validate" / "qualifying-receipt.json").is_file()
    ),
    ROOT.parent,
)
sys.path.insert(0, str(ROOT / "ci"))
import validate_peer_snapshot as peer_snapshot  # noqa: E402

FORBIDDEN_EVIDENCE_OVERRIDES = (
    "HERMIT_VALIDATE_FINALIZE_RECEIPT_HELPER",
    "HERMIT_VALIDATE_PEER_SNAPSHOT_HELPER",
    "HERMIT_VALIDATE_PROC_ROOT",
)


def process_start_ticks(pid: int) -> int:
    text = Path(f"/proc/{pid}/stat").read_text()
    close = text.rfind(")")
    assert close >= 0, text
    fields = text[close + 1 :].split()
    assert len(fields) >= 20, text
    return int(fields[19])


def isolated_authority_status() -> str:
    """Return a stop-test-only admissible snapshot for this live ancestor.

    validate.sh accepts this only in its intrinsically non-authorizing stop-test
    path. Production always calls the canonical parent authority-status query.
    """
    owner_pid = os.getpid()
    host = subprocess.check_output(["hostname", "-s"], text=True).strip()
    target = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    now = int(time.time())
    return json.dumps(
        {
            "schema_version": 1,
            "admissible": True,
            "state": "held",
            "reason_code": None,
            "holder": {
                "agent": "validate-stop-path-fixture",
                "kind": "validate",
                "target": target,
                "host": host,
                "acquired_at": now,
                "expires_at": now + 300,
            },
            "owner": {
                "host": host,
                "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "pid": owner_pid,
                "start_ticks": process_start_ticks(owner_pid),
                "liveness": "alive",
            },
            "canonical_anchor_held": True,
            "cleanup_state": "active-bound",
        },
        separators=(",", ":"),
    )


def stop_test_env(tmpdir: Path, ledger: Path) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = os.environ.copy()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        DEV_HERMIT_PARENT=str(DEV_HERMIT_PARENT),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
        VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON=isolated_authority_status(),
        TMPDIR=str(tmpdir),
    )
    return env


def wait_for_text(log: Path, text: str, process: subprocess.Popen[bytes]) -> None:
    # The first canonical parent query may cold-compile the rust-script binary.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if log.exists() and text in log.read_text(errors="replace"):
            return
        if process.poll() is not None:
            raise AssertionError(
                f"validate exited before ready: rc={process.returncode}"
            )
        time.sleep(0.05)
    observed = log.read_text(errors="replace") if log.exists() else "<no log>"
    raise AssertionError(
        f"validate stop-test hook did not emit {text!r}; observed:\n{observed}"
    )


def read_ledger(ledger: Path) -> list[dict]:
    if not ledger.exists():
        return []
    return [json.loads(line) for line in ledger.read_text().splitlines()]


def process_state(pid: int) -> str | None:
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return None
    close = text.rfind(")")
    assert close >= 0, text
    return text[close + 1 :].split()[0]


def wait_for_process_state(pid: int, accepted: set[str | None]) -> str | None:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        state = process_state(pid)
        if state in accepted:
            return state
        time.sleep(0.02)
    raise AssertionError(
        f"PID {pid} never reached one of {accepted}; state={process_state(pid)}"
    )


def wait_for_monitor_pid(path: Path, process: subprocess.Popen[bytes]) -> int:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if path.exists():
            value = path.read_text().strip()
            if value.isdigit():
                return int(value)
        if process.poll() is not None:
            raise AssertionError(
                f"validate exited before monitor PID: rc={process.returncode}"
            )
        time.sleep(0.02)
    log = path.parent / "validate.log"
    observed = log.read_text(errors="replace") if log.exists() else "<no fixture log>"
    raise AssertionError(
        f"validate monitor PID was not recorded; observed:\n{observed}"
    )


def find_monitor_state(tmpdir: Path, process: subprocess.Popen[bytes]) -> Path:
    deadline = time.monotonic() + 10
    validation_root = tmpdir / "validation"
    while time.monotonic() < deadline:
        states = list(
            validation_root.glob("hermit-validate.*/concurrent-validate-observed")
        )
        if len(states) == 1:
            return states[0]
        if process.poll() is not None:
            raise AssertionError(
                f"validate exited before monitor state: rc={process.returncode}"
            )
        time.sleep(0.02)
    raise AssertionError("validate monitor state was not found")


def run_signal(
    sig: signal.Signals, expect_record: bool, *, prior_failure: bool = False
) -> None:
    with tempfile.TemporaryDirectory(
        prefix=f"validate-stop-{sig.name.lower()}-"
    ) as tmp:
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
            if sig == signal.SIGKILL:
                # SIGKILL cannot run validate.sh's cleanup trap. Kill the exact
                # private fixture process group so its monitor cannot outlive
                # the leader and recreate the temporary state directory.
                os.killpg(process.pid, sig)
            else:
                process.send_signal(sig)
            rc = process.wait(timeout=10)

        rows = (
            [json.loads(line) for line in ledger.read_text().splitlines()]
            if ledger.exists()
            else []
        )
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


def write_fake_process(
    proc_root: Path,
    pid: int,
    *,
    ppid: int,
    pgid: int,
    start_ticks: int,
    argv: tuple[str, ...],
    cgroup: str,
    state: str = "S",
    flags: int = 0,
) -> None:
    process = proc_root / str(pid)
    process.mkdir(parents=True)
    # /proc/PID/stat fields 3..22: state, ppid, pgrp, then through starttime.
    fields = [
        state,
        str(ppid),
        str(pgid),
        *("0" for _ in range(3)),
        str(flags),
        *("0" for _ in range(12)),
        str(start_ticks),
    ]
    (process / "stat").write_text(f"{pid} (fixture) {' '.join(fields)}\n")
    (process / "cmdline").write_bytes(b"\0".join(arg.encode() for arg in argv) + b"\0")
    (process / "cgroup").write_text(f"0::{cgroup}\n")


def run_peer_identity_fixtures() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-peer-fixture-") as tmp:
        proc_root = Path(tmp)
        write_fake_process(
            proc_root,
            1,
            ppid=0,
            pgid=1,
            start_ticks=1,
            argv=("/sbin/init",),
            cgroup="/init.scope",
        )
        write_fake_process(
            proc_root,
            10,
            ppid=1,
            pgid=10,
            start_ticks=10,
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        write_fake_process(
            proc_root,
            20,
            ppid=10,
            pgid=20,
            start_ticks=20,
            argv=("safe-ci-dag-runner",),
            cgroup="/user.slice/validate-X.service",
        )
        write_fake_process(
            proc_root,
            21,
            ppid=20,
            pgid=21,
            start_ticks=21,
            argv=("bash", "/checkout/validate.sh", "full"),
            cgroup="/user.slice/validate-X.service/safe-ci-N.scope",
        )
        write_fake_process(
            proc_root,
            22,
            ppid=1,
            pgid=22,
            start_ticks=22,
            argv=("bash", "/reparented/validate.sh", "full"),
            cgroup="/user.slice/validate-X.service/safe-ci-R.scope",
        )
        write_fake_process(
            proc_root,
            30,
            ppid=1,
            pgid=30,
            start_ticks=30,
            argv=("bash", "-c", "tool_cost ./validate.sh full"),
            cgroup="/user.slice/validate-Z.service/wrapper.scope",
        )

        # A nested scope must resolve to its owning service (not the scope), and
        # both ancestry-bound and reparented members of validate-X.service are
        # self. Wrapper text is not an execution identity.
        snapshot = peer_snapshot.collect_peer_snapshot(proc_root, 10)
        assert snapshot["peers"] == [], snapshot
        assert snapshot["owner"] == {
            "pid": 10,
            "start_ticks": 10,
            "pgid": 10,
            "cgroup": "0::/user.slice/validate-X.service",
            "cgroup_path": "/user.slice/validate-X.service",
            "systemd_unit": "validate-X.service",
            "systemd_unit_cgroup": "/user.slice/validate-X.service",
        }, snapshot
        same_service = {
            process["pid"]: process for process in snapshot["same_service_processes"]
        }
        assert same_service[21]["classification"] == "owner-ancestry-self", same_service
        assert same_service[21]["systemd_unit"] == "validate-X.service", same_service
        assert (
            same_service[21]["systemd_unit_cgroup"] == "/user.slice/validate-X.service"
        ), same_service
        assert (
            same_service[22]["classification"] == "reparented-same-service-self"
        ), same_service
        assert same_service[22]["systemd_unit"] == "validate-X.service", same_service
        assert set(same_service) == {21, 22}, same_service

        write_fake_process(
            proc_root,
            40,
            ppid=1,
            pgid=40,
            start_ticks=40,
            argv=("bash", "/other/validate.sh", "full"),
            cgroup="/user.slice/validate-Z.service/safe-ci-E.scope",
        )
        peers = peer_snapshot.collect_external_peers(proc_root, 10)
        assert peers == [
            {
                "pid": 40,
                "start_ticks": 40,
                "pgid": 40,
                "cgroup": "0::/user.slice/validate-Z.service/safe-ci-E.scope",
                "cgroup_path": "/user.slice/validate-Z.service/safe-ci-E.scope",
                "systemd_unit": "validate-Z.service",
                "systemd_unit_cgroup": "/user.slice/validate-Z.service",
                "classification": "different-systemd-unit-peer",
            }
        ], peers

        def reused_start(_proc_root: Path, pid: int) -> int:
            if pid == 40:
                return 41
            return peer_snapshot.read_start_ticks(_proc_root, pid)

        try:
            peer_snapshot.collect_external_peers(
                proc_root, 10, start_reader=reused_start
            )
        except peer_snapshot.SnapshotUnresolved as error:
            assert "changed start_ticks 40->41" in str(error), error
        else:
            raise AssertionError("PID-reuse identity mismatch was accepted")

    with tempfile.TemporaryDirectory(prefix="validate-user-manager-fixture-") as tmp:
        proc_root = Path(tmp)
        write_fake_process(
            proc_root,
            1,
            ppid=0,
            pgid=1,
            start_ticks=1,
            argv=("/sbin/init",),
            cgroup="/init.scope",
        )
        write_fake_process(
            proc_root,
            50,
            ppid=1,
            pgid=50,
            start_ticks=50,
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/user@1000.service/app.slice/validate-A.scope",
        )
        write_fake_process(
            proc_root,
            51,
            ppid=1,
            pgid=51,
            start_ticks=51,
            argv=("bash", "/reparented/validate.sh", "full"),
            cgroup="/user.slice/user@1000.service/app.slice/validate-A.scope",
        )
        write_fake_process(
            proc_root,
            52,
            ppid=1,
            pgid=52,
            start_ticks=52,
            argv=("bash", "/external/validate.sh", "full"),
            cgroup="/user.slice/user@1000.service/app.slice/validate-B.scope",
        )
        snapshot = peer_snapshot.collect_peer_snapshot(proc_root, 50)
        assert snapshot["owner"]["systemd_unit"] == "validate-A.scope", snapshot
        assert snapshot["owner"]["systemd_unit"] != "user@1000.service", snapshot
        assert snapshot["same_service_processes"] == [
            {
                "pid": 51,
                "start_ticks": 51,
                "pgid": 51,
                "cgroup": "0::/user.slice/user@1000.service/app.slice/validate-A.scope",
                "cgroup_path": "/user.slice/user@1000.service/app.slice/validate-A.scope",
                "systemd_unit": "validate-A.scope",
                "systemd_unit_cgroup": "/user.slice/user@1000.service/app.slice/validate-A.scope",
                "classification": "reparented-same-service-self",
            }
        ], snapshot
        assert [peer["systemd_unit"] for peer in snapshot["peers"]] == [
            "validate-B.scope"
        ], snapshot


def run_peer_scan_completeness_fixtures() -> None:
    def fixture_root(label: str) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory(prefix=f"validate-peer-{label}-")
        proc_root = Path(temporary.name)
        write_fake_process(
            proc_root,
            10,
            ppid=0,
            pgid=10,
            start_ticks=10,
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        write_fake_process(
            proc_root,
            40,
            ppid=1,
            pgid=40,
            start_ticks=40,
            argv=("bash", "/external/validate.sh", "full"),
            cgroup="/user.slice/validate-Z.service",
        )
        return temporary, proc_root

    # A genuine ENOENT race is safe only after re-enumeration proves that the
    # numeric PID vanished. It must not poison an otherwise complete snapshot.
    temporary, proc_root = fixture_root("exit-race")
    with temporary:

        def vanishing_reader(root: Path, pid: int) -> peer_snapshot.ProcessRecord:
            if pid == 40:
                process = root / str(pid)
                for child in process.iterdir():
                    child.unlink()
                process.rmdir()
                raise FileNotFoundError(f"planted exit race for PID {pid}")
            return peer_snapshot.read_record(root, pid)

        snapshot = peer_snapshot.collect_peer_snapshot(
            proc_root, 10, record_reader=vanishing_reader
        )
        assert snapshot["peers"] == [], snapshot
        assert snapshot["owner"]["pid"] == 10, snapshot

    # The same bracket applies if the candidate was readable initially but
    # exits before its start_ticks identity is confirmed.
    temporary, proc_root = fixture_root("confirmation-exit-race")
    with temporary:

        def vanishing_start(root: Path, pid: int) -> int:
            if pid == 40:
                process = root / str(pid)
                for child in process.iterdir():
                    child.unlink()
                process.rmdir()
                raise FileNotFoundError(f"planted confirmation exit for PID {pid}")
            return peer_snapshot.read_start_ticks(root, pid)

        snapshot = peer_snapshot.collect_peer_snapshot(
            proc_root, 10, start_reader=vanishing_start
        )
        assert snapshot["peers"] == [], snapshot

    # A persistent numeric entry whose evidence cannot be read is unknown, not
    # an absent peer. This is the exact sibling-hole class from the re-review.
    temporary, proc_root = fixture_root("unreadable")
    with temporary:

        def unreadable_reader(root: Path, pid: int) -> peer_snapshot.ProcessRecord:
            if pid == 40:
                raise PermissionError("planted unreadable numeric PID")
            return peer_snapshot.read_record(root, pid)

        try:
            peer_snapshot.collect_peer_snapshot(
                proc_root, 10, record_reader=unreadable_reader
            )
        except peer_snapshot.SnapshotUnresolved as error:
            assert "PID 40" in str(error), error
            assert "unreadable or malformed" in str(error), error
            state = peer_snapshot.persist_state(
                proc_root / "state.json",
                {"owner": None, "same_service_processes": [], "peers": []},
                indeterminate=True,
                indeterminate_detail=f"snapshot-unresolved:{error}",
            )
        else:
            raise AssertionError("persistent unreadable numeric PID was dropped")
        assert state["scan_complete"] is False, state
        assert state["indeterminate"] is True, state
        assert state["scan_count"] == 0, state

        # Sticky means a later readable scan cannot recover authority.
        readable = peer_snapshot.collect_peer_snapshot(proc_root, 10)
        state = peer_snapshot.persist_state(proc_root / "state.json", readable)
        assert state["scan_complete"] is True, state
        assert state["indeterminate"] is True, state
        assert state["indeterminate_detail"].startswith(
            "snapshot-unresolved:process PID 40"
        ), state

    for label, mutate, detail in (
        (
            "malformed-stat",
            lambda root: (root / "40" / "stat").write_text("malformed\n"),
            "stat",
        ),
        (
            "malformed-cmdline",
            lambda root: (root / "40" / "cmdline").write_bytes(b""),
            "empty cmdline",
        ),
        (
            "malformed-cgroup",
            lambda root: (root / "40" / "cgroup").write_text("malformed\n"),
            "cgroup",
        ),
    ):
        temporary, proc_root = fixture_root(label)
        with temporary:
            mutate(proc_root)
            try:
                peer_snapshot.collect_peer_snapshot(proc_root, 10)
            except peer_snapshot.SnapshotUnresolved as error:
                assert "PID 40" in str(error), error
                assert detail in str(error), error
            else:
                raise AssertionError(f"{label} process evidence was dropped")

    # Well-formed kernel-thread evidence is classifiable even though it has no
    # argv or app unit; it must not make every real /proc scan indeterminate.
    temporary, proc_root = fixture_root("kernel-thread")
    with temporary:
        kernel = proc_root / "40"
        for child in kernel.iterdir():
            child.unlink()
        kernel.rmdir()
        write_fake_process(
            proc_root,
            2,
            ppid=0,
            pgid=0,
            start_ticks=2,
            argv=(),
            cgroup="/",
            flags=peer_snapshot.PF_KTHREAD,
        )
        snapshot = peer_snapshot.collect_peer_snapshot(proc_root, 10)
        assert snapshot["peers"] == [], snapshot


def admission_verdict(row: dict) -> dict:
    policy = json.loads(
        (
            DEV_HERMIT_PARENT / "ci-hub" / "validate" / "qualifying-receipt.json"
        ).read_text()
    )
    request = {
        "row": row,
        "admission": policy["admission"],
        "producer": policy["producer"],
        "base": policy["base"],
    }
    result = subprocess.run(
        [
            sys.executable,
            str(DEV_HERMIT_PARENT / "ci-hub" / "qualifying_receipt.py"),
            "--admission-only",
        ],
        input=json.dumps(request),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    assert result.returncode in {0, 1}, (result.stdout, result.stderr)
    verdict = json.loads(result.stdout)
    verdict["_exit_code"] = result.returncode
    return verdict


def qualification_verdict(row: dict) -> dict:
    result = subprocess.run(
        [
            sys.executable,
            str(DEV_HERMIT_PARENT / "ci-hub" / "qualifying_receipt.py"),
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
    assert result.returncode in {0, 1}, (result.stdout, result.stderr)
    verdict = json.loads(result.stdout)
    verdict["_exit_code"] = result.returncode
    return verdict


def assert_indeterminate_diagnostic(
    ledger: Path,
    *,
    expected_detail: str,
    expected_raw_result: str = "fail",
) -> dict:
    rows = read_ledger(ledger)
    assert len(rows) == 1, rows
    row = rows[0]
    assert row["result"] == "no_result", row
    assert row["raw_result"] == expected_raw_result, row
    assert row["concurrent_validates"] is None, row
    assert row["concurrency_proof"] is None, row
    assert row["concurrency_indeterminate"] is True, row
    assert row["concurrency_indeterminate_detail"].startswith(expected_detail), row
    verdict = qualification_verdict(row)
    assert verdict["_exit_code"] == 1 and verdict["accepted"] is False, verdict
    assert sum(qualification_verdict(candidate)["accepted"] for candidate in rows) == 0
    return row


def run_monitor_refusal_fixture(mode: str, expected_detail: str) -> None:
    with tempfile.TemporaryDirectory(prefix=f"validate-monitor-{mode}-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        monitor_pid_file = tmpdir / "monitor-pid"
        owner_file = tmpdir / "owner"
        owner_file.write_text(f"pid={os.getpid()}\n")
        release_file = tmpdir / "success-release"
        publisher_calls = tmpdir / "publisher-calls"
        publisher = tmpdir / "inert-publisher"
        publisher.write_text(
            f"#!{sys.executable}\n"
            "from pathlib import Path\n"
            f"Path({str(publisher_calls)!r}).write_text('called\\n')\n"
        )
        publisher.chmod(0o755)
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_MONITOR_PID_FILE=str(monitor_pid_file),
            CI_HUB_VALIDATE_LOCK_OWNER_PID=str(os.getpid()),
            CI_HUB_VALIDATE_LOCK_OWNER_FILE=str(owner_file),
            CI_HUB_APPLY_LOCAL_LABEL=str(publisher),
            PR_NUMBER="999999",
            VALIDATE_TIMEOUT_KILL_GRACE_SECONDS="1",
        )
        normal_success = mode == "missing-ack"
        if normal_success:
            env.update(
                VALIDATE_STOP_TEST_SUCCESS_RELEASE="1",
                VALIDATE_STOP_TEST_RELEASE_FILE=str(release_file),
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
                monitor_pid = wait_for_monitor_pid(monitor_pid_file, process)
                marker = find_monitor_state(tmpdir, process)
                if mode == "killed":
                    os.kill(monitor_pid, signal.SIGKILL)
                    wait_for_process_state(monitor_pid, {"Z", None})
                elif mode == "stopped":
                    os.kill(monitor_pid, signal.SIGSTOP)
                    wait_for_process_state(monitor_pid, {"T", "t"})
                elif mode == "missing-ack":
                    control_socket = marker.parent / "peer-monitor.sock"
                    assert control_socket.is_socket(), control_socket
                    control_socket.unlink()
                else:
                    raise AssertionError(f"unknown monitor refusal mode {mode}")
                if normal_success:
                    release_file.write_text("release\n")
                else:
                    process.send_signal(signal.SIGTERM)
                rc = process.wait(timeout=15)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
                else:
                    # The exact private process group may still contain the
                    # monitor's one-second sleep after its leader exited.
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass

        assert rc == (0 if normal_success else 130), (
            mode,
            rc,
            log.read_text(errors="replace"),
        )
        assert_indeterminate_diagnostic(
            ledger,
            expected_detail=expected_detail,
            expected_raw_result="pass" if normal_success else "fail",
        )
        assert not publisher_calls.exists(), (
            mode,
            publisher_calls.read_text(errors="replace"),
        )


def run_paused_monitor_recovery_fixture() -> None:
    """A recovered long scan is acknowledged, not rejected by elapsed time."""
    with tempfile.TemporaryDirectory(prefix="validate-monitor-recovered-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        monitor_pid_file = tmpdir / "monitor-pid"
        release_file = tmpdir / "success-release"
        proc_root = tmpdir / "proc"
        proc_root.mkdir()
        write_fake_process(
            proc_root,
            os.getpid(),
            ppid=0,
            pgid=os.getpid(),
            start_ticks=process_start_ticks(os.getpid()),
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        publisher_calls = tmpdir / "publisher-calls"
        publisher = tmpdir / "inert-publisher"
        publisher.write_text(
            f"#!{sys.executable}\n"
            "from pathlib import Path\n"
            f"Path({str(publisher_calls)!r}).write_text('called\\n')\n"
        )
        publisher.chmod(0o755)
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_MONITOR_PID_FILE=str(monitor_pid_file),
            VALIDATE_STOP_TEST_PEER_PROC_ROOT=str(proc_root),
            VALIDATE_STOP_TEST_SUCCESS_RELEASE="1",
            VALIDATE_STOP_TEST_RELEASE_FILE=str(release_file),
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
                monitor_pid = wait_for_monitor_pid(monitor_pid_file, process)
                marker = find_monitor_state(tmpdir, process)
                before = json.loads(marker.read_text())
                assert before["monitor_pid"] == monitor_pid, before
                os.kill(monitor_pid, signal.SIGSTOP)
                wait_for_process_state(monitor_pid, {"T", "t"})
                stopped = json.loads(marker.read_text())
                # Longer than the removed five-second heuristic. The kernel
                # flock remains held while the monitor is descheduled.
                time.sleep(5.75)
                still_stopped = json.loads(marker.read_text())
                assert (
                    still_stopped["monitor_sequence"] == stopped["monitor_sequence"]
                ), (
                    stopped,
                    still_stopped,
                )
                os.kill(monitor_pid, signal.SIGCONT)
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    later = json.loads(marker.read_text())
                    if later["monitor_sequence"] > before["monitor_sequence"]:
                        break
                    time.sleep(0.05)
                else:
                    raise AssertionError("resumed monitor did not advance its sequence")
                release_file.write_text("release\n")
                rc = process.wait(timeout=30)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
                else:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass

        assert rc == 0, (rc, log.read_text(errors="replace"))
        rows = read_ledger(ledger)
        assert len(rows) == 1, rows
        row = rows[0]
        assert row["raw_result"] == "pass", row
        assert row["concurrency_indeterminate"] is False, row
        monitor = row["concurrent_validate_monitor"]
        assert monitor["monitor_sequence"] == monitor["final_ack_sequence"], monitor
        assert monitor["exclusion_held"] is True, monitor
        assert (
            sum(qualification_verdict(candidate)["accepted"] for candidate in rows) == 0
        )
        assert publisher_calls.read_text() == "called\n", publisher_calls


def run_descheduled_exclusion_fixture() -> None:
    """A stopped lock holder excludes a second receipt-producing monitor."""
    with tempfile.TemporaryDirectory(prefix="validate-monitor-exclusion-") as tmp:
        tmpdir = Path(tmp)
        shared_lock = tmpdir / "shared-peer-exclusion.lock"
        proc_root = tmpdir / "proc"
        proc_root.mkdir()
        write_fake_process(
            proc_root,
            10,
            ppid=0,
            pgid=10,
            start_ticks=10,
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        write_fake_process(
            proc_root,
            os.getpid(),
            ppid=0,
            pgid=os.getpid(),
            start_ticks=process_start_ticks(os.getpid()),
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        holder_state = tmpdir / "holder-state.json"
        holder_socket = tmpdir / "holder.sock"
        holder = subprocess.Popen(
            [
                sys.executable,
                str(ROOT / "ci" / "validate_peer_snapshot.py"),
                "--monitor",
                "--owner-pid",
                "10",
                "--state",
                str(holder_state),
                "--control-socket",
                str(holder_socket),
                "--fixture-mode",
                "--fixture-exclusion-lock",
                str(shared_lock),
                "--proc-root",
                str(proc_root),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if holder_state.exists():
                    state = json.loads(holder_state.read_text())
                    if state.get("monitor_ready") is True:
                        break
                if holder.poll() is not None:
                    raise AssertionError(holder.stderr.read().decode(errors="replace"))
                time.sleep(0.05)
            else:
                raise AssertionError("exclusion holder did not become ready")
            assert state["exclusion_held"] is True, state
            os.kill(holder.pid, signal.SIGSTOP)
            wait_for_process_state(holder.pid, {"T", "t"})

            ledger = tmpdir / "ledger.jsonl"
            log = tmpdir / "validate.log"
            release_file = tmpdir / "success-release"
            publisher_calls = tmpdir / "publisher-calls"
            publisher = tmpdir / "inert-publisher"
            publisher.write_text(
                f"#!{sys.executable}\n"
                "from pathlib import Path\n"
                f"Path({str(publisher_calls)!r}).write_text('called\\n')\n"
            )
            publisher.chmod(0o755)
            env = stop_test_env(tmpdir, ledger)
            env.update(
                VALIDATE_STOP_TEST_EXCLUSION_LOCK=str(shared_lock),
                VALIDATE_STOP_TEST_PEER_PROC_ROOT=str(proc_root),
                VALIDATE_STOP_TEST_SUCCESS_RELEASE="1",
                VALIDATE_STOP_TEST_RELEASE_FILE=str(release_file),
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
                    release_file.write_text("release\n")
                    rc = process.wait(timeout=30)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGTERM)
                        process.wait(timeout=10)
                    else:
                        try:
                            os.killpg(process.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
            assert rc == 0, (rc, log.read_text(errors="replace"))
            row = assert_indeterminate_diagnostic(
                ledger,
                expected_detail="final-state-indeterminate:peer-exclusion-lock-contended",
                expected_raw_result="pass",
            )
            assert row["concurrent_validates"] is None, row
            assert row["concurrency_proof"] is None, row
            assert not publisher_calls.exists(), publisher_calls
        finally:
            if process_state(holder.pid) in {"T", "t"}:
                os.kill(holder.pid, signal.SIGCONT)
            holder.terminate()
            holder.wait(timeout=10)


def run_initial_authority_failure_fixture() -> None:
    with tempfile.TemporaryDirectory(
        prefix="validate-authority-initial-failure-"
    ) as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        monitor_pid_file = tmpdir / "monitor-pid"
        owner_file = tmpdir / "owner"
        owner_file.write_text(f"pid={os.getpid()}\n")
        env = stop_test_env(tmpdir, ledger)
        env.pop("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON")
        env.update(
            VALIDATE_STOP_TEST_MONITOR_PID_FILE=str(monitor_pid_file),
            CI_HUB_VALIDATE_LOCK_OWNER_PID=str(os.getpid()),
            CI_HUB_VALIDATE_LOCK_OWNER_FILE=str(owner_file),
            VALIDATE_TIMEOUT_KILL_GRACE_SECONDS="1",
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
                monitor_pid = wait_for_monitor_pid(monitor_pid_file, process)
                assert process_state(monitor_pid) not in {None, "Z", "T", "t"}
                marker = find_monitor_state(tmpdir, process)
                initial_state = json.loads(marker.read_text())
                assert initial_state["indeterminate"] is True, initial_state
                assert initial_state["indeterminate_detail"].startswith(
                    "canonical-lock-authority-"
                ), initial_state
                time.sleep(1.25)
                later_state = json.loads(marker.read_text())
                assert later_state["indeterminate"] is True, later_state
                assert (
                    later_state["indeterminate_detail"]
                    == initial_state["indeterminate_detail"]
                )
                assert (
                    later_state["monitor_sequence"] >= initial_state["monitor_sequence"]
                ), later_state
                assert later_state["monitor_ready"] is True, later_state
                assert process_state(monitor_pid) not in {None, "Z", "T", "t"}
                process.send_signal(signal.SIGTERM)
                rc = process.wait(timeout=15)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
                else:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass

        assert rc == 130, (rc, log.read_text(errors="replace"))
        assert "canonical validate-lock authority unavailable" in log.read_text(
            errors="replace"
        )
        assert_indeterminate_diagnostic(
            ledger,
            expected_detail="final-state-indeterminate:canonical-lock-authority-",
        )


def run_schema5_receipt_fixture() -> dict:
    with tempfile.TemporaryDirectory(prefix="validate-schema5-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        override_marker = tmpdir / "caller-override-ran"
        caller_helper = tmpdir / "caller-helper.py"
        caller_helper.write_text(
            "from pathlib import Path\n"
            f"Path({str(override_marker)!r}).write_text('invoked')\n"
        )
        fake_proc_root = tmpdir / "caller-proc"
        fake_proc_root.mkdir()
        stop_test_proc_root = tmpdir / "stop-test-proc"
        stop_test_proc_root.mkdir()
        write_fake_process(
            stop_test_proc_root,
            os.getpid(),
            ppid=0,
            pgid=os.getpid(),
            start_ticks=process_start_ticks(os.getpid()),
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            VALIDATE_STOP_TEST_PEER_PROC_ROOT=str(stop_test_proc_root),
            HERMIT_VALIDATE_FINALIZE_RECEIPT_HELPER=str(caller_helper),
            HERMIT_VALIDATE_PEER_SNAPSHOT_HELPER=str(caller_helper),
            HERMIT_VALIDATE_PROC_ROOT=str(fake_proc_root),
        )
        result = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        assert result.returncode == 1, result.stdout.decode(errors="replace")
        assert not override_marker.exists(), "caller evidence helper was invoked"
        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert len(rows) == 1, rows
        if rows[0]["admission"] != "ci-hub-validate-lock":
            raise AssertionError(
                (
                    rows[0],
                    result.stdout.decode(errors="replace"),
                    Path(rows[0]["log_file"]).read_text(errors="replace"),
                )
            )
        return rows[0]


def run_forged_sidecar_refusal_fixture() -> tuple[int, int, int]:
    with tempfile.TemporaryDirectory(prefix="validate-forged-lock-sidecar-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        owner_file = tmpdir / "forged-owner"
        owner_file.write_text(f"pid={os.getpid()}\n")
        release_file = tmpdir / "success-release"
        publisher_calls = tmpdir / "publisher-calls"
        publisher = tmpdir / "inert-publisher"
        publisher.write_text(
            f"#!{sys.executable}\n"
            "from pathlib import Path\n"
            f"Path({str(publisher_calls)!r}).write_text('called\\n')\n"
        )
        publisher.chmod(0o755)
        env = stop_test_env(tmpdir, ledger)
        # This is the production consumer path: remove the stop-test authority
        # fixture, then plant exactly the two old caller-controlled proofs.
        env.pop("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON")
        env.update(
            CI_HUB_VALIDATE_LOCK_OWNER_PID=str(os.getpid()),
            CI_HUB_VALIDATE_LOCK_OWNER_FILE=str(owner_file),
            CI_HUB_APPLY_LOCAL_LABEL=str(publisher),
            PR_NUMBER="999999",
            VALIDATE_STOP_TEST_SUCCESS_RELEASE="1",
            VALIDATE_STOP_TEST_RELEASE_FILE=str(release_file),
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
                release_file.write_text("release\n")
                rc = process.wait(timeout=15)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
                else:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass

        assert rc == 0, (rc, log.read_text(errors="replace"))
        row = assert_indeterminate_diagnostic(
            ledger,
            expected_detail="final-state-indeterminate:canonical-lock-authority-",
            expected_raw_result="pass",
        )
        assert row["admission"] is None, row
        assert row["concurrent_validates"] is None, row
        assert row["concurrency_proof"] is None, row
        assert not publisher_calls.exists(), publisher_calls.read_text(errors="replace")
        rows_total = len(read_ledger(ledger))
        qualifying = sum(
            qualification_verdict(candidate)["accepted"]
            for candidate in read_ledger(ledger)
        )
        publisher_count = 1 if publisher_calls.exists() else 0
        assert (rows_total, qualifying, publisher_count) == (1, 0, 0)
        return rows_total, qualifying, publisher_count


def run_unresolvable_process_refusal_fixture() -> tuple[int, int, int]:
    with tempfile.TemporaryDirectory(prefix="validate-unresolvable-process-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        proc_root = tmpdir / "proc"
        proc_root.mkdir()
        write_fake_process(
            proc_root,
            os.getpid(),
            ppid=0,
            pgid=os.getpid(),
            start_ticks=process_start_ticks(os.getpid()),
            argv=("ci-hub", "validate-lock"),
            cgroup="/user.slice/validate-X.service",
        )
        write_fake_process(
            proc_root,
            40,
            ppid=1,
            pgid=40,
            start_ticks=40,
            argv=("bash", "/external/validate.sh", "full"),
            cgroup="/user.slice/validate-Z.service",
        )
        unreadable_stat = proc_root / "40" / "stat"
        unreadable_stat.chmod(0)
        release_file = tmpdir / "success-release"
        publisher_calls = tmpdir / "publisher-calls"
        publisher = tmpdir / "inert-publisher"
        publisher.write_text(
            f"#!{sys.executable}\n"
            "from pathlib import Path\n"
            f"Path({str(publisher_calls)!r}).write_text('called\\n')\n"
        )
        publisher.chmod(0o755)
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_PEER_PROC_ROOT=str(proc_root),
            CI_HUB_APPLY_LOCAL_LABEL=str(publisher),
            PR_NUMBER="999999",
            VALIDATE_STOP_TEST_SUCCESS_RELEASE="1",
            VALIDATE_STOP_TEST_RELEASE_FILE=str(release_file),
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
                release_file.write_text("release\n")
                rc = process.wait(timeout=15)
            finally:
                unreadable_stat.chmod(0o600)
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
                else:
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass

        assert rc == 0, (rc, log.read_text(errors="replace"))
        row = assert_indeterminate_diagnostic(
            ledger,
            expected_detail="final-state-indeterminate:snapshot-unresolved:process PID 40",
            expected_raw_result="pass",
        )
        assert row["concurrent_validates"] is None, row
        assert row["concurrency_proof"] is None, row
        assert not publisher_calls.exists(), publisher_calls.read_text(errors="replace")
        rows_total = len(read_ledger(ledger))
        qualifying = sum(
            qualification_verdict(candidate)["accepted"]
            for candidate in read_ledger(ledger)
        )
        publisher_count = 1 if publisher_calls.exists() else 0
        assert (rows_total, qualifying, publisher_count) == (1, 0, 0)
        return rows_total, qualifying, publisher_count


def run_schema5_receipt_fixtures() -> None:
    production_source = VALIDATE.read_text()
    references = {
        name: production_source.count(name) for name in FORBIDDEN_EVIDENCE_OVERRIDES
    }
    assert references == {name: 0 for name in FORBIDDEN_EVIDENCE_OVERRIDES}, references

    row = run_schema5_receipt_fixture()
    assert row["schema_version"] == 5, row
    assert row["producer"] == "hermit-validate-sh", row
    assert row["repo"] == "hermit", row
    assert row["admission"] == "ci-hub-validate-lock", row
    assert row["concurrent_validates"] == 0, row
    assert row["concurrency_proof"] == "validate_lock_owner_ancestry", row
    assert row["concurrency_indeterminate"] is False, row
    assert row["concurrency_indeterminate_detail"] is None, row
    assert row["concurrent_validate_peers"] == [], row
    monitor = row["concurrent_validate_monitor"]
    assert monitor["monitor_protocol"] == peer_snapshot.MONITOR_PROTOCOL, monitor
    assert monitor["monitor_sequence"] == monitor["final_ack_sequence"], monitor
    assert monitor["exclusion_kind"] == "kernel-flock", monitor
    assert monitor["exclusion_held"] is True, monitor
    assert row["coverage"]["planned_test_nodes"] == 19, row
    expected_base = subprocess.check_output(
        ["git", "merge-base", "HEAD", "origin/main"], cwd=ROOT, text=True
    ).strip()
    expected_base_tree = subprocess.check_output(
        ["git", "rev-parse", f"{expected_base}^{{tree}}"], cwd=ROOT, text=True
    ).strip()
    lock_text = subprocess.check_output(
        ["git", "show", "HEAD:Cargo.lock"], cwd=ROOT, text=True
    )
    reverie_pins = set(
        re.findall(
            r"git\+https://github\.com/(?:rrnewton|facebookexperimental)/"
            r"reverie\.git\?rev=([0-9a-f]{40})",
            lock_text,
        )
    )
    assert len(reverie_pins) == 1, reverie_pins
    expected_reverie = next(iter(reverie_pins))
    expected_reverie_tree = subprocess.check_output(
        ["git", "rev-parse", f"{expected_reverie}^{{tree}}"],
        cwd=DEV_HERMIT_PARENT / "reverie",
        text=True,
    ).strip()
    assert row["base_sha"] == expected_base, row
    assert row["base_tree"] == expected_base_tree, row
    assert row["reverie_base_sha"] == expected_reverie, row
    assert row["reverie_base_tree"] == expected_reverie_tree, row
    verdict = admission_verdict(row)
    assert verdict["_exit_code"] == 0, verdict
    assert verdict["accepted"] is True, verdict
    assert verdict["admission_status"] == "satisfied", verdict
    assert verdict["base_status"] == "satisfied", verdict

    assert run_forged_sidecar_refusal_fixture() == (1, 0, 0)
    assert run_unresolvable_process_refusal_fixture() == (1, 0, 0)


def main() -> None:
    run_peer_identity_fixtures()
    run_peer_scan_completeness_fixtures()
    run_schema5_receipt_fixtures()
    run_initial_authority_failure_fixture()
    run_monitor_refusal_fixture("killed", "monitor-died")
    run_monitor_refusal_fixture("stopped", "monitor-stopped")
    run_monitor_refusal_fixture("missing-ack", "monitor-final-ack-missing")
    run_paused_monitor_recovery_fixture()
    run_descheduled_exclusion_fixture()
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_incomplete_exit()
    run_cleanup_signal_race()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    print(
        "PASS: identity-bound peer fixtures; schema-5 admission/base fixtures; "
        "forged sidecar/PID refusal 1 diagnostic/0 qualifying/0 publisher calls; "
        "unresolvable process refusal 1 diagnostic/0 qualifying/0 publisher calls; "
        "peer scan completeness: exit races 2/2 accepted, unresolved evidence 4/4 refused sticky, "
        "kernel-thread 1/1 classified; "
        "override negative 3 planted/0 production references/0 invoked; "
        "monitor refusal negatives 3/3 => exactly 1 diagnostic + 0 qualifying; "
        "recovered >5s pause => sequence/final-ack accepted; "
        "descheduled exclusion => second monitor 1 diagnostic/0 qualifying/0 publisher; "
        "initial authority failure => sticky + monitor live + 1 diagnostic/0 qualifying; "
        "success disruption => raw pass downgraded + publisher calls 0; "
        "TERM/INT/HUP => NO-RESULT; KILL => no record; prior failure remains "
        "fail; cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
