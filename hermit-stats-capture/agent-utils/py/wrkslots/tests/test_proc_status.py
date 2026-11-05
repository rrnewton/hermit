"""Ownership samples must distinguish a vanished proc inode from a live PID."""

from __future__ import annotations

import contextlib
import dataclasses
import errno
import os
import selectors
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Iterator, Sequence
from pathlib import Path
from typing import Literal, TextIO

import pytest

from wrkslots import cli


def scripted_status(
    monkeypatch: pytest.MonkeyPatch,
    pid_dir: Path,
    samples: Sequence[str | bytes | BaseException],
) -> list[Path]:
    pending = iter(samples)
    calls: list[Path] = []

    def read_bytes(path: Path) -> bytes:
        assert path == pid_dir / "status"
        calls.append(path)
        sample = next(pending)
        if isinstance(sample, BaseException):
            raise sample
        return sample if isinstance(sample, bytes) else sample.encode("ascii")

    monkeypatch.setattr(Path, "read_bytes", read_bytes)
    return calls


def opaque_stat(
    pid: int, parent: int, *, flags: int = 0, start_ticks: int = 17
) -> bytes:
    """One kernel-shaped stat record whose comm is deliberately not text."""
    fields = [b"S", str(parent).encode(), str(pid).encode(), str(pid).encode()]
    fields.extend([b"0", b"0", str(flags).encode()])
    fields.extend([b"0"] * 12)
    fields.append(str(start_ticks).encode())
    return str(pid).encode() + b" (p\xff) Z\n(\x80) " + b" ".join(fields) + b"\n"


def test_process_stat_and_ancestry_treat_comm_as_opaque_bytes(tmp_path: Path) -> None:
    proc_root = tmp_path / "proc"
    pid_dir = proc_root / "123"
    pid_dir.mkdir(parents=True)
    (pid_dir / "stat").write_bytes(opaque_stat(123, 42, flags=9, start_ticks=99))

    assert cli._read_process_stat(pid_dir) == cli._ProcessStat(99, 9)
    assert cli._read_process_parent(123, proc_root=proc_root) == 42


def test_global_process_snapshot_accepts_opaque_comm_bytes(tmp_path: Path) -> None:
    """One valid opaque process name cannot make the all-process census unavailable."""
    proc_root = tmp_path / "proc"
    pid_dir = proc_root / "123"
    (pid_dir / "ns").mkdir(parents=True)
    (pid_dir / "stat").write_bytes(opaque_stat(123, 42, flags=0, start_ticks=99))
    uid = os.getuid()
    (pid_dir / "status").write_bytes(
        b"Name:\tp\xff\rUid:\t0\vState:\tZ\\n(\x80\nState:\tS (sleeping)\nUid:\t"
        + f"{uid}\t{uid}\t{uid}\t{uid}\n".encode()
    )
    (pid_dir / "cgroup").write_text("0::/fixture\n", encoding="ascii")
    (pid_dir / "ns" / "mnt").symlink_to("mnt:[123]")

    assert cli._absent_validate_process_snapshot(proc_root) == (
        cli._AbsentProcessObservation(123, 99, "/fixture", "mnt:[123]"),
    )


@contextlib.contextmanager
def waiting_child(cwd: Path | None = None) -> Iterator[subprocess.Popen[bytes]]:
    child = subprocess.Popen(
        [sys.executable, "-B", "-c", "import sys; print('ready', flush=True); sys.stdin.read()"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=cwd,
    )
    pidfd: int | None = None
    try:
        pidfd = os.pidfd_open(child.pid)
        assert child.stdout is not None
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            assert selector.select(5), "owned child did not report readiness"
            assert child.stdout.readline() == b"ready\n"
        yield child
    finally:
        assert child.stdin is not None
        if not child.stdin.closed:
            child.stdin.close()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            if pidfd is None:
                # Acquisition failed before this direct child was reaped.
                child.kill()
            else:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
            child.wait(timeout=5)
            raise
        finally:
            for stream in (child.stdout, child.stderr):
                if stream is not None:
                    stream.close()
            if pidfd is not None:
                os.close(pidfd)


def test_process_uids_reopens_after_real_status_read_races_reap(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_open = Path.open
    with waiting_child() as child:
        pid_dir = Path("/proc") / str(child.pid)
        status_path = pid_dir / "status"
        opens = 0

        def open_then_reap(
            path: Path, mode: Literal["r"] = "r", buffering: int = -1,
            encoding: str | None = None, errors: str | None = None,
            newline: str | None = None,
        ) -> TextIO:
            nonlocal opens
            if path != status_path:
                return original_open(path, mode, buffering, encoding, errors, newline)
            opens += 1
            stream = original_open(path, mode, buffering, encoding, errors, newline)
            try:
                assert opens == 1, "the fresh path should be absent after reap"
                assert child.stdin is not None
                child.stdin.close()
                assert child.wait(timeout=3) == 0
            except BaseException:
                stream.close()
                raise
            return stream

        monkeypatch.setattr(Path, "open", open_then_reap)
        assert cli._process_uids(pid_dir) is None
        assert opens == 2, "ESRCH must lead to one fresh pathname observation"


def test_process_uids_keeps_fresh_replacement_and_zombie_leader_credentials(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    pid_dir = Path("/proc/123")
    calls = scripted_status(
        monkeypatch,
        pid_dir,
        [ProcessLookupError(errno.ESRCH, "old generation exited"),
         "State:\tZ (zombie)\nThreads:\t2\nUid:\t10\t11\t12\t13\n"],
    )
    assert cli._process_uids(pid_dir) == (10, 11, 12, 13)
    assert len(calls) == 2


@pytest.mark.parametrize("after_esrch", [False, True])
@pytest.mark.parametrize("number", [errno.EACCES, errno.EPERM, errno.EIO])
def test_process_uids_refuses_unexpected_errors_without_more_resampling(
    monkeypatch: pytest.MonkeyPatch, after_esrch: bool, number: int
) -> None:
    error = OSError(number, os.strerror(number))
    samples: list[str | BaseException] = [error]
    if after_esrch:
        samples.insert(0, ProcessLookupError(errno.ESRCH, "old inode"))
    calls = scripted_status(monkeypatch, Path("/proc/123"), samples)
    with pytest.raises(cli.Refusal, match="process ownership is indeterminate") as refused:
        cli._process_uids(Path("/proc/123"))
    assert refused.value.__cause__ is error
    assert len(calls) == 1 + int(after_esrch)


def test_process_uids_refuses_a_second_esrch(monkeypatch: pytest.MonkeyPatch) -> None:
    second = ProcessLookupError(errno.ESRCH, "fresh inode also exited")
    calls = scripted_status(
        monkeypatch, Path("/proc/123"),
        [ProcessLookupError(errno.ESRCH, "old inode"), second],
    )
    with pytest.raises(cli.Refusal, match="process ownership is indeterminate") as refused:
        cli._process_uids(Path("/proc/123"))
    assert refused.value.__cause__ is second
    assert len(calls) == 2


@pytest.mark.parametrize("after_esrch", [False, True])
@pytest.mark.parametrize("status", [
    "State:\tS (sleeping)\n",
    "Uid:\t1\t2\t3\n",
    "Uid:\t1\tbroken\t3\t4\n",
    b"Name:\topaque\xff\n",
])
def test_process_uids_refuses_missing_malformed_or_incomplete_status(
    monkeypatch: pytest.MonkeyPatch, after_esrch: bool,
    status: str | bytes | BaseException,
) -> None:
    samples = [status]
    if after_esrch:
        samples.insert(0, ProcessLookupError(errno.ESRCH, "old inode"))
    calls = scripted_status(monkeypatch, Path("/proc/123"), samples)
    with pytest.raises(cli.Refusal, match="process ownership is indeterminate"):
        cli._process_uids(Path("/proc/123"))
    assert len(calls) == 1 + int(after_esrch)


@pytest.mark.parametrize("same_uid", [False, True])
def test_fresh_status_does_not_hide_changed_process_generation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, same_uid: bool
) -> None:
    proc_root = tmp_path / "proc"
    pid_dir = proc_root / "123"
    pid_dir.mkdir(parents=True)
    uid = os.getuid()
    calls = scripted_status(
        monkeypatch, pid_dir,
        [ProcessLookupError(errno.ESRCH, "old inode"), f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"],
    )
    generations = iter([cli._ProcessStat(17, 0), cli._ProcessStat(18, 0)])
    monkeypatch.setattr(cli, "_read_process_stat", lambda _path: next(generations))
    monkeypatch.setattr(cli, "_mount_namespace", lambda _path: "mnt:[123]")
    monkeypatch.setattr(cli, "_read_process_cgroup", lambda _path: "/fixture")
    budget = cli._ReadOnlyCommandBudget.start(
        timeout_seconds=30, stdout_limit=1024, stderr_limit=1024,
    )
    with pytest.raises(cli._ProcessEvidenceChanged, match="process generation changed"):
        if same_uid:
            cli._same_uid_process_observations(budget, proc_root)
        else:
            cli._absent_validate_process_snapshot(proc_root)
    assert len(calls) == 2


def test_direct_census_refuses_a_live_holder_after_fresh_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    proc_root = tmp_path / "proc"
    pid_dir = proc_root / str(os.getpid() + 1000)
    pid_dir.mkdir(parents=True)
    uid = os.getuid()
    calls = scripted_status(
        monkeypatch, pid_dir,
        [ProcessLookupError(errno.ESRCH, "old inode"), f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"],
    )
    monkeypatch.setattr(
        cli, "_process_uses_slot", lambda _pid, _slot, **_kw: [f"cwd={slot}"]
    )
    with pytest.raises(cli.Refusal, match="live process .* uses slot"):
        cli._assert_slot_unused(slot, use_lsof=False, proc_root=proc_root)
    assert len(calls) == 2


def test_lsof_guard_refuses_a_real_live_user_with_attributed_evidence(
    tmp_path: Path,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    assert Path("/usr/bin/lsof").is_file(), "the slot guard requires the installed lsof"

    with waiting_child(cwd=slot) as child:
        with pytest.raises(cli.Refusal) as refused:
            cli._assert_slot_unused(slot, proc_root=empty_proc)

    rendered = str(refused.value)
    assert f"live process {child.pid} uses slot {slot}" in rendered
    assert "command=" in rendered
    assert 'fd="cwd"' in rendered
    assert f'file="{slot}"' in rendered
    assert len(rendered) < 1024


def scripted_lsof(
    monkeypatch: pytest.MonkeyPatch,
    slot: Path,
    responses: list[subprocess.CompletedProcess[str]],
    *,
    events: list[str] | None = None,
) -> list[list[str]]:
    pending = iter(responses)
    calls: list[list[str]] = []

    def run(
        args: list[str], *, text: bool, capture_output: bool, check: bool
    ) -> subprocess.CompletedProcess[str]:
        assert args == ["/usr/bin/lsof", "-nP", "-Fpcfn", "+D", str(slot)]
        assert text is True and capture_output is True and check is False
        calls.append(args)
        if events is not None:
            events.append("lsof")
        return next(pending)

    monkeypatch.setattr(subprocess, "run", run)
    return calls


def lsof_holder(
    pid: int, slot: Path, *, returncode: int = 0
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(
        ["lsof"],
        returncode,
        f"p{pid}\ncpython\nfcwd\nn{slot}\n",
        "",
    )


def lsof_unused() -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(["lsof"], 1, "", "")


def test_reclaim_restarts_lsof_and_proc_after_exact_lsof_generation_exits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    with waiting_child(cwd=slot) as child:
        events: list[str] = []
        calls = scripted_lsof(
            monkeypatch,
            slot,
            [
                lsof_holder(child.pid, slot),
                lsof_holder(child.pid, slot),
                lsof_unused(),
            ],
            events=events,
        )
        real_read_process_stat = cli._read_process_stat
        real_pidfd_open = os.pidfd_open
        generation_reads = 0

        def release_after_pidfd_generation_check(
            path: Path,
        ) -> cli._ProcessStat | None:
            nonlocal generation_reads
            observed = real_read_process_stat(path)
            if path == Path("/proc") / str(child.pid):
                events.append("stat")
                generation_reads += 1
                if generation_reads == 2:
                    assert child.stdin is not None
                    child.stdin.close()
            return observed

        monkeypatch.setattr(
            cli, "_read_process_stat", release_after_pidfd_generation_check
        )

        def record_pidfd_open(pid: int) -> int:
            assert pid == child.pid
            events.append("pidfd")
            return real_pidfd_open(pid)

        monkeypatch.setattr(os, "pidfd_open", record_pidfd_open)
        budget = cli._LiveUseRecheckBudget()

        cli._assert_slot_unused(
            slot,
            proc_root=empty_proc,
            live_use_recheck=budget,
        )

        assert child.wait(timeout=1) == 0
        assert len(calls) == 3
        assert generation_reads == 2
        assert events == ["lsof", "stat", "pidfd", "lsof", "stat", "lsof"]
        assert budget.remaining_seconds < cli._RECLAIM_LIVE_USE_RECHECK_SECONDS


def test_reclaim_restarts_proc_after_exact_proc_generation_exits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    proc_root = tmp_path / "proc"
    with waiting_child(cwd=slot) as child:
        pid_dir = proc_root / str(child.pid)
        pid_dir.mkdir(parents=True)
        uid = os.getuid()
        (pid_dir / "status").write_text(
            f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n", encoding="ascii"
        )
        real_generation = cli._read_process_stat(Path("/proc") / str(child.pid))
        assert real_generation is not None
        generation_reads = 0
        use_reads = 0

        def stable_generation(path: Path) -> cli._ProcessStat | None:
            nonlocal generation_reads
            assert path == pid_dir
            generation_reads += 1
            if generation_reads == 2:
                assert child.stdin is not None
                child.stdin.close()
            return real_generation

        def transient_use(_pid_dir: Path, _slot: Path, **_kw: object) -> list[str]:
            nonlocal use_reads
            use_reads += 1
            return [f"cwd={slot}"] if use_reads == 1 else []

        monkeypatch.setattr(cli, "_read_process_stat", stable_generation)
        monkeypatch.setattr(cli, "_process_uses_slot", transient_use)
        budget = cli._LiveUseRecheckBudget()

        cli._assert_slot_unused(
            slot,
            use_lsof=False,
            proc_root=proc_root,
            live_use_recheck=budget,
        )

        assert child.wait(timeout=1) == 0
        assert generation_reads == 3
        assert use_reads == 2
        assert budget.remaining_seconds < cli._RECLAIM_LIVE_USE_RECHECK_SECONDS


def test_reclaim_recheck_catches_replacement_without_signaling_any_holder(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    with (
        waiting_child(cwd=slot) as first,
        waiting_child(cwd=slot) as replacement,
        waiting_child() as unrelated,
    ):
        calls = scripted_lsof(
            monkeypatch,
            slot,
            [
                lsof_holder(first.pid, slot),
                lsof_holder(first.pid, slot),
                lsof_holder(replacement.pid, slot),
                lsof_holder(replacement.pid, slot),
            ],
        )
        real_read_process_stat = cli._read_process_stat
        first_reads = 0

        def release_first_after_generation_check(
            path: Path,
        ) -> cli._ProcessStat | None:
            nonlocal first_reads
            observed = real_read_process_stat(path)
            if path == Path("/proc") / str(first.pid):
                first_reads += 1
                if first_reads == 2:
                    assert first.stdin is not None
                    first.stdin.close()
            return observed

        monkeypatch.setattr(
            cli, "_read_process_stat", release_first_after_generation_check
        )
        budget = cli._LiveUseRecheckBudget(0.05)

        with pytest.raises(cli.Refusal) as refused:
            cli._assert_slot_unused(
                slot,
                proc_root=empty_proc,
                live_use_recheck=budget,
            )

        assert f"live process {replacement.pid} uses slot {slot}" in str(refused.value)
        assert len(calls) == 4
        assert budget.remaining_seconds == 0
        assert first.poll() == 0
        assert replacement.poll() is None
        assert unrelated.poll() is None


def test_reclaim_recheck_persistent_holder_exhausts_one_deadline(
    tmp_path: Path,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    with waiting_child(cwd=slot) as child:
        generation = cli._read_process_stat(Path("/proc") / str(child.pid))
        assert generation is not None
        budget = cli._LiveUseRecheckBudget(0.02)

        assert budget.wait_for_exit(
            child.pid,
            start_ticks=generation.start_ticks,
        ) is False

        assert budget.remaining_seconds == 0
        assert child.poll() is None


def test_reclaim_recheck_deadline_is_shared_across_later_checks(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    samples = iter((10.0, 10.01, 10.06))
    opened: list[int] = []

    def process_exited(pid: int) -> int:
        opened.append(pid)
        raise ProcessLookupError(errno.ESRCH, "fixture generation exited")

    monkeypatch.setattr(time, "monotonic", lambda: next(samples))
    monkeypatch.setattr(os, "pidfd_open", process_exited)
    budget = cli._LiveUseRecheckBudget(0.05)

    assert budget.wait_for_exit(111, start_ticks=17) is True
    assert budget.wait_for_exit(222, start_ticks=23) is False
    assert opened == [111]
    assert budget.remaining_seconds == 0


@pytest.mark.parametrize("failure", ["pid-reuse", "pidfd-failure", "incomplete"])
def test_reclaim_never_retries_unbound_process_generation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    failure: str,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    calls = scripted_lsof(
        monkeypatch,
        slot,
        [
            lsof_holder(123, slot),
            *(
                [lsof_holder(123, slot)]
                if failure == "pid-reuse"
                else []
            ),
        ],
    )
    opened: list[int] = []
    read_fd, write_fd = os.pipe()
    generations = iter(
        (
            (None,)
            if failure == "incomplete"
            else (cli._ProcessStat(17, 0), cli._ProcessStat(18, 0))
        )
    )

    def generation(_path: Path) -> cli._ProcessStat | None:
        return next(generations)

    def pidfd_open(pid: int) -> int:
        opened.append(pid)
        if failure == "pidfd-failure":
            raise PermissionError(errno.EPERM, "fixture pidfd denial")
        return os.dup(read_fd)

    monkeypatch.setattr(cli, "_read_process_stat", generation)
    monkeypatch.setattr(os, "pidfd_open", pidfd_open)
    try:
        with pytest.raises(cli.Refusal, match="live process 123 uses slot"):
            cli._assert_slot_unused(
                slot,
                proc_root=empty_proc,
                live_use_recheck=cli._LiveUseRecheckBudget(0.25),
            )
    finally:
        os.close(read_fd)
        os.close(write_fd)

    assert len(calls) == (2 if failure == "pid-reuse" else 1)
    assert opened == ([] if failure == "incomplete" else [123])


def test_reclaim_never_retries_lsof_pid_reused_before_generation_binding(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    events: list[str] = []
    calls = scripted_lsof(
        monkeypatch,
        slot,
        [lsof_holder(123, slot), lsof_unused()],
        events=events,
    )

    def replacement_generation(_path: Path) -> cli._ProcessStat:
        events.append("stat")
        return cli._ProcessStat(18, 0)

    monkeypatch.setattr(cli, "_read_process_stat", replacement_generation)
    opened: list[int] = []
    read_fd, write_fd = os.pipe()

    def pidfd_open(pid: int) -> int:
        opened.append(pid)
        events.append("pidfd")
        return os.dup(read_fd)

    monkeypatch.setattr(os, "pidfd_open", pidfd_open)
    try:
        with pytest.raises(cli.Refusal, match="live process 123 uses slot"):
            cli._assert_slot_unused(
                slot,
                proc_root=empty_proc,
                live_use_recheck=cli._LiveUseRecheckBudget(0.25),
            )
    finally:
        os.close(read_fd)
        os.close(write_fd)

    assert len(calls) == 2
    assert opened == [123]
    assert events == ["lsof", "stat", "pidfd", "lsof"]


def test_reclaim_never_retries_lsof_holder_gone_before_pidfd_binding(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    calls = scripted_lsof(monkeypatch, slot, [lsof_holder(123, slot)])
    monkeypatch.setattr(
        cli, "_read_process_stat", lambda _path: cli._ProcessStat(17, 0)
    )
    opened: list[int] = []

    def exited_before_pidfd(pid: int) -> int:
        opened.append(pid)
        raise ProcessLookupError(errno.ESRCH, "fixture lsof holder exited")

    monkeypatch.setattr(os, "pidfd_open", exited_before_pidfd)

    with pytest.raises(cli.Refusal, match="live process 123 uses slot"):
        cli._assert_slot_unused(
            slot,
            proc_root=empty_proc,
            live_use_recheck=cli._LiveUseRecheckBudget(0.25),
        )

    assert len(calls) == 1
    assert opened == [123]


@pytest.mark.parametrize(
    ("response", "message"),
    [
        (
            subprocess.CompletedProcess(["lsof"], 0, "", "fatal lsof error\n"),
            "lsof reported",
        ),
        (
            subprocess.CompletedProcess(["lsof"], 2, "", ""),
            "lsof exited 2",
        ),
        (
            subprocess.CompletedProcess(["lsof"], 0, "unexpected\n", ""),
            "malformed field output",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"], 2, "p123\ncgit\nfcwd\nn/slot\n", ""
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"], 0, "p123\ncgit\nxunknown\nfcwd\nn/slot\n", ""
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(["lsof"], 0, "p123\n", ""),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"], 0, "p123\ncgit\nfcwd\n", ""
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"], 0, "p123\nfcwd\nn/slot\n", ""
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"],
                0,
                "p123\ncgit\nfcwd\nn/slot\np124\n",
                "",
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"], 0, "p123\nfcwd\ncgit\nn/slot\n", ""
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"], 0, "p123\nfcwd\nn/slot\ncgit\n", ""
            ),
            "live process 123 uses slot",
        ),
        (
            subprocess.CompletedProcess(
                ["lsof"],
                0,
                (
                    "p123\ncgit\nfcwd\nn/slot\n"
                    "p123\ncgit\nfcwd\nn/slot\n"
                ),
                "",
            ),
            "live process 123 uses slot",
        ),
    ],
)
def test_reclaim_never_retries_indeterminate_lsof_evidence(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    response: subprocess.CompletedProcess[str],
    message: str,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    empty_proc = tmp_path / "empty-proc"
    empty_proc.mkdir()
    if "n/slot\n" in response.stdout:
        response = subprocess.CompletedProcess(
            response.args,
            response.returncode,
            response.stdout.replace("/slot", str(slot)),
            response.stderr,
        )
    calls = scripted_lsof(monkeypatch, slot, [response])
    budget = cli._LiveUseRecheckBudget(0.25)
    opened: list[int] = []

    def unexpected_pidfd_open(pid: int) -> int:
        opened.append(pid)
        raise AssertionError("indeterminate lsof evidence must not reach pidfd")

    monkeypatch.setattr(os, "pidfd_open", unexpected_pidfd_open)

    with pytest.raises(cli.Refusal, match=message):
        cli._assert_slot_unused(
            slot,
            proc_root=empty_proc,
            live_use_recheck=budget,
        )

    assert len(calls) == 1
    assert opened == []
    assert budget.remaining_seconds == 0.25


def test_reclaim_never_retries_incomplete_proc_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    proc_root = tmp_path / "proc"
    fixture_pid = os.getpid() + 1_000
    pid_dir = proc_root / str(fixture_pid)
    pid_dir.mkdir(parents=True)
    uid = os.getuid()
    (pid_dir / "status").write_text(
        f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n", encoding="ascii"
    )
    monkeypatch.setattr(
        cli, "_read_process_stat", lambda _path: cli._ProcessStat(17, 0)
    )
    monkeypatch.setattr(
        cli,
        "_process_uses_slot",
        lambda _pid, _slot, **_kw: (_ for _ in ()).throw(
            cli.Refusal("fixture proc evidence is incomplete")
        ),
    )
    waits: list[int] = []

    def unexpected_wait(
        _budget: cli._LiveUseRecheckBudget,
        pid: int,
        *,
        start_ticks: int | None,
        proc_root: Path = Path("/proc"),
        revalidate: Callable[[], bool] | None = None,
    ) -> bool:
        del start_ticks, proc_root, revalidate
        waits.append(pid)
        return True

    monkeypatch.setattr(cli._LiveUseRecheckBudget, "wait_for_exit", unexpected_wait)

    with pytest.raises(cli.Refusal, match="fixture proc evidence is incomplete"):
        cli._assert_slot_unused(
            slot,
            use_lsof=False,
            fallback_census=cli._ProcessPathCensus((), ()),
            proc_root=proc_root,
            live_use_recheck=cli._LiveUseRecheckBudget(),
        )

    assert waits == []


def test_reclaim_keeps_first_proc_holder_refusal_when_later_evidence_is_incomplete(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    proc_root = tmp_path / "proc"
    holder_pid = os.getpid() + 1_000
    incomplete_pid = holder_pid + 1
    for pid in (holder_pid, incomplete_pid):
        (proc_root / str(pid)).mkdir(parents=True)
    monkeypatch.setattr(
        cli, "_read_process_stat", lambda _path: cli._ProcessStat(17, 0)
    )

    def process_uids(pid_dir: Path) -> tuple[int, int, int, int]:
        if int(pid_dir.name) == incomplete_pid:
            raise cli.Refusal("later fixture ownership is incomplete")
        uid = os.getuid()
        return uid, uid, uid, uid

    monkeypatch.setattr(cli, "_process_uids", process_uids)
    monkeypatch.setattr(
        cli,
        "_process_uses_slot",
        lambda pid_dir, _slot, **_kw: (
            [f"cwd={slot}"] if int(pid_dir.name) == holder_pid else []
        ),
    )
    opened: list[int] = []

    def unexpected_pidfd_open(pid: int) -> int:
        opened.append(pid)
        raise AssertionError("incomplete census must suppress the pidfd wait")

    monkeypatch.setattr(os, "pidfd_open", unexpected_pidfd_open)

    with pytest.raises(
        cli.Refusal,
        match=rf"live process {holder_pid} uses slot",
    ):
        cli._assert_slot_unused(
            slot,
            use_lsof=False,
            proc_root=proc_root,
            live_use_recheck=cli._LiveUseRecheckBudget(),
        )

    assert opened == []


def test_reclaim_never_retries_before_an_unbound_second_proc_holder(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    proc_root = tmp_path / "proc"
    first_pid = os.getpid() + 1_000
    unbound_pid = first_pid + 1
    for pid in (first_pid, unbound_pid):
        (proc_root / str(pid)).mkdir(parents=True)

    def generation(pid_dir: Path) -> cli._ProcessStat | None:
        return (
            cli._ProcessStat(17, 0)
            if int(pid_dir.name) == first_pid
            else None
        )

    uid = os.getuid()
    monkeypatch.setattr(cli, "_read_process_stat", generation)
    monkeypatch.setattr(
        cli, "_process_uids", lambda _path: (uid, uid, uid, uid)
    )
    monkeypatch.setattr(
        cli, "_process_uses_slot", lambda _pid, _slot, **_kw: [f"cwd={slot}"]
    )
    opened: list[int] = []

    def unexpected_pidfd_open(pid: int) -> int:
        opened.append(pid)
        raise AssertionError("an unbound holder must suppress the pidfd wait")

    monkeypatch.setattr(os, "pidfd_open", unexpected_pidfd_open)

    with pytest.raises(cli.Refusal, match=rf"live process {first_pid} uses slot"):
        cli._assert_slot_unused(
            slot,
            use_lsof=False,
            proc_root=proc_root,
            live_use_recheck=cli._LiveUseRecheckBudget(),
        )

    assert opened == []


def test_reclaim_fallback_exit_restarts_the_original_proc_census(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    slot = tmp_path / "slot"
    slot.mkdir()
    proc_root = tmp_path / "proc"
    fixture_pid_dir = proc_root / str(os.getpid() + 1_000)
    fixture_pid_dir.mkdir(parents=True)
    with waiting_child(cwd=slot) as child:
        lsof_calls = scripted_lsof(
            monkeypatch,
            slot,
            [lsof_holder(child.pid, slot), lsof_holder(child.pid, slot)],
        )
        real_read_process_stat = cli._read_process_stat
        generation_reads = 0
        proc_reads = 0

        def generations(path: Path) -> cli._ProcessStat | None:
            nonlocal generation_reads
            if path == fixture_pid_dir:
                return cli._ProcessStat(17, 0)
            observed = real_read_process_stat(path)
            if path == Path("/proc") / str(child.pid):
                generation_reads += 1
                if generation_reads == 2:
                    assert child.stdin is not None
                    child.stdin.close()
            return observed

        def process_uids(_pid_dir: Path) -> tuple[int, int, int, int]:
            uid = os.getuid()
            return uid, uid, uid, uid

        def indeterminate_then_clear(
            _pid_dir: Path, _slot: Path, **_kw: object
        ) -> list[str]:
            nonlocal proc_reads
            proc_reads += 1
            if proc_reads == 1:
                raise cli.Refusal("fixture direct proc evidence is incomplete")
            return []

        monkeypatch.setattr(cli, "_read_process_stat", generations)
        monkeypatch.setattr(cli, "_process_uids", process_uids)
        monkeypatch.setattr(cli, "_process_uses_slot", indeterminate_then_clear)

        cli._assert_slot_unused(
            slot,
            use_lsof=False,
            proc_root=proc_root,
            live_use_recheck=cli._LiveUseRecheckBudget(),
        )

        assert child.wait(timeout=1) == 0
        assert len(lsof_calls) == 2
        assert generation_reads == 2
        assert proc_reads == 2


def test_lsof_diagnostic_associates_process_command_and_first_file_record() -> None:
    evidence, malformed = cli._parse_lsof_use_diagnostic(
        "\n".join(
            (
                "p900",
                "cother",
                "f9",
                "n/other/file",
                "p123",
                "cgit maintenance",
                "fcwd",
                "n/selected/slot",
                "f7",
                "n/selected/slot/later",
                "p456",
                "clater",
                "fcwd",
                "n/later/slot",
            )
        )
        + "\n"
    )

    assert malformed is False
    assert evidence == cli._LsofUseDiagnostic(
        pid=123,
        command_excerpt='"git maintenance"',
        descriptor_excerpt='"cwd"',
        name_excerpt='"/selected/slot"',
    )
    assert cli._parse_lsof_use_diagnostic("") == (None, False)


def test_lsof_diagnostic_is_bounded_terminal_safe_and_malformed_safe() -> None:
    long_command = "x" * 10_000 + "\x1b[31m"
    long_descriptor = "9" * 10_000
    long_name = "/slot/" + "y" * 10_000
    evidence, malformed = cli._parse_lsof_use_diagnostic(
        f"p123\nc{long_command}\nf{long_descriptor}\nn{long_name}\n"
    )
    assert evidence is not None
    assert malformed is False
    rendered = cli._format_lsof_use_diagnostic(evidence, malformed=malformed)
    assert len(rendered) < 600
    assert rendered.count(cli._LSOF_DIAGNOSTIC_TRUNCATION) == 3
    assert "\x1b" not in rendered
    assert "\n" not in rendered

    forged, forged_malformed = cli._parse_lsof_use_diagnostic(
        f"p123\nc{long_command}\nf{long_descriptor}\nn{long_name}\nforged refusal\n"
    )
    assert forged is not None
    assert forged_malformed is True
    forged_rendered = cli._format_lsof_use_diagnostic(
        forged, malformed=forged_malformed
    )
    assert "malformed lsof fields omitted" in forged_rendered
    assert len(forged_rendered) < 600
    assert "\x1b" not in forged_rendered
    assert "\n" not in forged_rendered

    for broken in (
        "corphan\nfcwd\nn/slot\n",
        f"p{'9' * 10_000}\ncbogus\nfcwd\nn/slot\n",
        "p123\ncright\nn/orphan\nf1\nn/slot/file\n",
        "p123\ncright\nxunknown\nn/slot/file\n",
    ):
        parsed, parse_malformed = cli._parse_lsof_use_diagnostic(broken)
        assert parse_malformed is True
        if parsed is not None:
            diagnostic = cli._format_lsof_use_diagnostic(
                parsed, malformed=parse_malformed
            )
            assert "malformed lsof fields omitted" in diagnostic
            assert len(diagnostic) < 600


def test_the_machines_init_is_refused_as_a_process_identity() -> None:
    """PID 1 must never be recorded as an owner, coordinator or actor.

    ⚠️ THE READ-TIME CHECKS CANNOT CATCH THIS, which is why the guard is at
    write time. Liveness compares the recorded generation against the live one,
    and for the machine's init that comparison answers "alive" until reboot --
    so a slot registered this way is unreclaimable and nothing downstream can
    tell it from a genuine owner.

    This runs against the real /proc rather than a fixture, because the
    property being tested is about this machine's actual init.
    """
    if Path("/proc/1/cgroup").read_text(encoding="ascii").strip() != "0::/init.scope":
        pytest.skip("this machine's init is not in /init.scope")
    with pytest.raises(cli.Refusal) as refused:
        cli._read_process_identity(1)
    rendered = str(refused.value)
    assert "the machine's init process" in rendered
    assert "is not live" not in rendered


def test_a_namespaced_pid_1_is_still_recorded(monkeypatch: pytest.MonkeyPatch) -> None:
    """The guard asks what the process IS, not what it is numbered.

    ⚠️ THIS IS THE CONTROL, AND IT IS NOT HYPOTHETICAL. A first version of this
    guard refused PID 1 by number and broke the process-and-git stress suite,
    which runs its whole harness under a PID namespace where the runner
    legitimately is PID 1. Inside such a namespace PID 1 reports an ordinary
    session scope, and it must still be recordable.
    """
    real = cli._read_process_cgroup

    def session_scope(pid_dir: Path) -> str:
        if pid_dir.name == "1":
            return "/user.slice/user-1000.slice/session-3.scope"
        return real(pid_dir)

    monkeypatch.setattr(cli, "_read_process_cgroup", session_scope)
    identity = cli._read_process_identity(1)
    assert identity.pid == 1
    assert identity.cgroup_path == "/user.slice/user-1000.slice/session-3.scope"


def test_the_kernel_thread_daemon_is_refused_as_a_process_identity() -> None:
    if Path("/proc/2/cgroup").read_text(encoding="ascii").strip() != "0::/":
        pytest.skip("this machine's PID 2 is not the kernel thread daemon")
    with pytest.raises(cli.Refusal) as refused:
        cli._read_process_identity(2)
    assert "the kernel thread daemon" in str(refused.value)


def test_an_ordinary_live_pid_is_still_recorded() -> None:
    """The guard must refuse two specific processes, not narrow registration."""
    identity = cli._read_process_identity(os.getpid())
    assert identity.pid == os.getpid()
    assert identity.start_ticks > 0


def test_a_live_owner_cgroup_is_still_evidence() -> None:
    """The narrowing must be conditional, not a deletion.

    ⚠️ THIS IS THE CONTROL FOR THE CGROUP NARROWING. The change it guards makes
    a shared owner cgroup stop blocking reclaim once the recorded owner
    generation is proven dead. If the condition were dropped rather than
    narrowed, this test is what notices: a live recorded owner must still make
    its cgroup count as evidence.
    """
    record = cli.ActiveRecord.__new__(cli.ActiveRecord)
    object.__setattr__(record, "owner", cli._read_process_identity(os.getpid()))
    assert cli._owner_cgroup_is_evidence(record) is True


def test_a_dead_owner_cgroup_is_not_evidence() -> None:
    live = cli._read_process_identity(os.getpid())
    record = cli.ActiveRecord.__new__(cli.ActiveRecord)
    # A boot that has ended is the cheapest proof of death available here, and
    # it is the same one the lifecycle fixtures use.
    object.__setattr__(record, "owner", dataclasses.replace(live, boot_id="finished-boot"))
    assert cli._owner_cgroup_is_evidence(record) is False


def test_no_recorded_owner_is_not_evidence() -> None:
    record = cli.ActiveRecord.__new__(cli.ActiveRecord)
    object.__setattr__(record, "owner", None)
    assert cli._owner_cgroup_is_evidence(record) is False
    assert cli._owner_cgroup_is_evidence(None) is False


def _record(owner: "cli.ProcessIdentity | None") -> "cli.ActiveRecord":
    record = cli.ActiveRecord.__new__(cli.ActiveRecord)
    object.__setattr__(record, "owner", owner)
    object.__setattr__(record, "slot", "slot01")
    return record


def test_an_absent_owner_record_no_longer_preserves_a_slot() -> None:
    assert cli._owner_record_is_absent(_record(None)) is True


def test_the_machines_init_as_owner_counts_as_absent() -> None:
    """A degenerate owner is an error on its face, not a fact to preserve."""
    live = cli._read_process_identity(os.getpid())
    init = dataclasses.replace(live, pid=1, cgroup_path="/init.scope")
    assert cli._owner_record_is_absent(_record(init)) is True


def test_a_real_owner_is_not_treated_as_absent() -> None:
    """⚠️ THE CONTROL. If this ever returns True the unblocking has escaped to
    every slot, which would drop the proven-dead requirement for rows whose
    owner is perfectly well recorded."""
    live = cli._read_process_identity(os.getpid())
    assert cli._owner_record_is_absent(_record(live)) is False
    # PID 1 inside its own namespace is a real process in an ordinary scope and
    # must NOT be swept in.
    namespaced = dataclasses.replace(live, pid=1, cgroup_path="/user.slice/session-3.scope")
    assert cli._owner_record_is_absent(_record(namespaced)) is False


def test_the_audit_publishes_each_slot_once_even_from_several_routes() -> None:
    """One slot described by two routes must be published once, not twice.

    ⚠️ A DEADLOCK TEST, NOT A TIDINESS TEST. The audit's `rows` is built from
    FOUR separate append sites, so a slot present both in the registry and as an
    on-disk worktree legitimately produces two rows -- which is why
    `owner_state == "unregistered"` is itself an attention condition. The
    published list carried both, and the downstream contract then refused the
    WHOLE census with "audit.attention_slots contains duplicates". Nothing
    downstream could clear that: the duplicate is produced inside the audit
    every time two routes describe one slot, so no later participant could
    complete the operation from the record.

    ⚠️ AND IT CALLS THE REAL FUNCTION. An earlier version of this test
    reimplemented the dedup inline and passed whatever the production code did,
    which is the exact shape of a check that cannot fail for the reason it
    names.
    """
    # Two routes to one slot, plus the same NAME on a second machine -- the case
    # a (slot, machine) key could not have caught, because the published list
    # carries the name alone.
    rows = ["slot01", "slot01", "slot02", "slot01"]
    assert cli._first_seen_names(rows) == ["slot01", "slot02"]

    published = cli._first_seen_names(rows)
    assert len(published) == len(set(published))
    # First-seen order, so the report is stable between runs.
    assert cli._first_seen_names(["b", "a", "b"]) == ["b", "a"]
    assert cli._first_seen_names([]) == []


def exiting_init_proc(
    proc_root: Path, pid: int = 2991802, *, wchan: str = "zap_pid_ns_processes",
    nspid: str = "2991802\t1", cwd: str | None = None, root: str | None = None,
    threads: int = 1,
) -> Path:
    """The measured shape of a PID-namespace init being torn down.

    Its wait channel is zap_pid_ns_processes and its cwd, root and ns/mnt
    are gone (ENOENT): the kernel released them before it began to wait.
    """
    pid_dir = proc_root / str(pid)
    (pid_dir / "ns").mkdir(parents=True)
    (pid_dir / "stat").write_bytes(opaque_stat(pid, 1, flags=0, start_ticks=99))
    uid = os.getuid()
    (pid_dir / "status").write_text(
        f"Name:\tpidns_recheck\nState:\tS (sleeping)\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"
        f"NSpid:\t{nspid}\nThreads:\t{threads}\n", encoding="ascii")
    (pid_dir / "cgroup").write_text("0::/fixture\n", encoding="ascii")
    (pid_dir / "wchan").write_text(wchan, encoding="ascii")
    if cwd is not None:
        (pid_dir / "cwd").symlink_to(cwd)
    if root is not None:
        (pid_dir / "root").symlink_to(root)
    return pid_dir


def test_an_exiting_namespace_init_holds_no_checkout(tmp_path: Path, capsys) -> None:
    # 2026-10-13: every `wrkslots remove <validate-fresh-...> --validate-complete`
    # refused "mount namespace is indeterminate for live PID 2991802", a
    # namespace init stuck in zap_pid_ns_processes, and 49 GB of finished
    # validation checkouts could not be removed.
    proc_root = tmp_path / "proc"
    exiting_init_proc(proc_root)
    assert cli._absent_validate_process_snapshot(proc_root) == ()
    budget = cli._ReadOnlyCommandBudget.start(timeout_seconds=30, stdout_limit=1 << 20,
                                              stderr_limit=1 << 20)
    assert cli._same_uid_process_observations(budget, proc_root) == ()
    err = capsys.readouterr().err
    assert "PID 2991802 (pidns_recheck) is a PID-namespace init being torn down" in err


@pytest.mark.parametrize("near_miss", [
    {"wchan": "do_wait"},                   # same name, not tearing down: still refused
    {"nspid": "2991802\t7"},               # not the init of its namespace
    {"nspid": "1\t7"},                     # PID 1 only at an OUTER level: not the init
    {"cwd": "/home/somewhere/checkout"},   # still holds a working directory
    {"root": "/"},                          # still holds a root
    {"threads": 2},                         # another thread may still hold files
], ids=["other-wait-channel", "not-a-namespace-init", "init-only-at-an-outer-level",
        "cwd-still-held", "root-still-held", "two-threads"])
def test_a_near_miss_of_an_exiting_namespace_init_is_still_refused(
        tmp_path: Path, near_miss: dict[str, str]) -> None:
    proc_root = tmp_path / "proc"
    exiting_init_proc(proc_root, **near_miss)
    with pytest.raises(cli.Refusal, match="mount namespace is indeterminate for live PID 2991802"):
        cli._absent_validate_process_snapshot(proc_root)
    budget = cli._ReadOnlyCommandBudget.start(timeout_seconds=30, stdout_limit=1 << 20,
                                              stderr_limit=1 << 20)
    with pytest.raises(cli.Refusal, match="mount namespace is indeterminate for live PID 2991802"):
        cli._same_uid_process_observations(budget, proc_root)



def test_a_link_that_is_unreadable_rather_than_released_is_still_refused(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # Only ENOENT means released; a permission error on cwd says nothing.
    proc_root = tmp_path / "proc"
    pid_dir = exiting_init_proc(proc_root)
    real_readlink = os.readlink

    def readlink(path, *args, **kwargs):
        if Path(path) == pid_dir / "cwd":
            raise PermissionError(errno.EACCES, "Permission denied", str(path))
        return real_readlink(path, *args, **kwargs)

    monkeypatch.setattr(cli.os, "readlink", readlink)
    assert not cli._process_is_exiting_namespace_init(pid_dir)
