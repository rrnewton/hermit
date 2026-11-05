"""Synthetic lifecycle brackets and actual kernel controls for CPU observations."""

import ctypes
import errno
import json
import os
import resource
import select
import subprocess
import time
from pathlib import Path

import pytest

from dagrun import proccpu as cpu
from dagrun.proccpu import subtree_cpu_seconds


def _write_stat(root: Path, pid: int, comm: str, pgrp: int, ticks: tuple[int, int, int, int]) -> None:
    directory = root / str(pid)
    directory.mkdir(parents=True)
    fields = [str(pid), f"({comm})", "R", "1", str(pgrp), *("0" for _ in range(8)), *(str(v) for v in ticks)]
    (directory / "stat").write_text(" ".join(fields), encoding="utf-8")


class _Row:
    def __init__(self, original: cpu._Stat, bits: int = 0, fresh: cpu._Stat | None = None, error: str = "") -> None:
        self.original = original
        self.bits = bits
        self.fresh = fresh
        self.error = error
        self.closed = False

    def valid(self, deadline: float) -> bool:
        def read() -> cpu._Stat | None:
            if self.error:
                raise cpu.Unavailable(self.error)
            return self.fresh
        return cpu._accept(self.bits, self.original, read)

    def close(self) -> None:
        self.closed = True


class _Files:
    """Explicit kernel-observation seam; synthetic files are never a live API fallback."""
    def __init__(self, root: Path) -> None:
        self.root = root

    def samples(self, groups: set[int], deadline: float) -> list[cpu._Observation]:
        rows: list[cpu._Observation] = []
        for path in sorted(self.root.iterdir()):
            original = cpu._stat((path / "stat").read_text())
            if original.group in groups:
                rows.append(_Row(original))
        return rows


def _synthetic_seconds(pgid: int, root: Path) -> float | None:
    try:
        totals = cpu._scan(_Files(root), {pgid}, time.monotonic() + 1)
    except (OSError, cpu.Unavailable):
        return None
    return None if pgid not in totals else totals[pgid] / os.sysconf("SC_CLK_TCK")


def test_sums_only_the_named_group_and_includes_reaped_children(tmp_path: Path) -> None:
    _write_stat(tmp_path, 100, "leader", 100, (10, 5, 20, 5))
    _write_stat(tmp_path, 101, "child (x)", 100, (30, 0, 0, 0))
    _write_stat(tmp_path, 200, "stranger", 200, (9999, 9999, 9999, 9999))
    got = _synthetic_seconds(100, tmp_path)
    assert got == pytest.approx(70 / os.sysconf("SC_CLK_TCK"))


def test_zero_is_a_reading_but_absence_is_unknown(tmp_path: Path) -> None:
    _write_stat(tmp_path, 100, "leader", 100, (0, 0, 0, 0))
    assert _synthetic_seconds(100, tmp_path) == 0.0
    assert _synthetic_seconds(999, tmp_path) is None


def test_unreadable_or_malformed_procfs_is_unknown(tmp_path: Path) -> None:
    assert _synthetic_seconds(100, tmp_path / "missing") is None
    bad = tmp_path / "100"
    bad.mkdir()
    (bad / "stat").write_text("malformed", encoding="utf-8")
    assert _synthetic_seconds(100, tmp_path) is None
    assert subtree_cpu_seconds(100, proc_root=tmp_path) is None


def test_refuses_degenerate_process_groups(tmp_path: Path) -> None:
    assert subtree_cpu_seconds(0, proc_root=tmp_path) is None
    assert subtree_cpu_seconds(1, proc_root=tmp_path) is None


def test_shared_canonical_lifecycle_corpus() -> None:
    data: object = json.loads((Path(__file__).parents[2] / "rs/dagrun/tests/fixtures/proccpu-generation.json").read_text())
    assert isinstance(data, list)
    for case in data:
        assert isinstance(case, dict)
        events: list[str] = []
        rows: list[_Row] = []
        class Source:
            def samples(self, groups: set[int], deadline: float) -> list[cpu._Observation]:
                for raw in case["rows"]:
                    events.append(f"sample:{raw['pid']}")
                    original = cpu._Stat(raw["pid"], raw["group"], "R", raw["ticks"])
                    if original.group not in groups:
                        continue
                    fresh = None if raw["state"] == "gone" else cpu._Stat(raw.get("fresh_pid", raw["pid"]), raw["group"], raw["state"], raw.get("fresh_ticks", 0))
                    rows.append(_Row(original, raw["poll"], fresh, raw.get("error", "")))
                return list(rows)
        try:
            result = cpu._scan(Source(), {100}, time.monotonic() + 1)
        except cpu.Unavailable:
            assert case.get("error"), case["name"]
        else:
            assert not case.get("error"), case["name"]
            assert result.get(100) == case["expected"], case["name"]
        assert all(row.closed for row in rows), case["name"]


def test_scan_samples_all_members_before_validating() -> None:
    events: list[str] = []
    class Row(_Row):
        def valid(self, deadline: float) -> bool:
            assert events[:3] == ["sample:0", "sample:1", "sample:2"]
            events.append("validate")
            return True
    class Source:
        def samples(self, groups: set[int], deadline: float) -> list[cpu._Observation]:
            rows: list[cpu._Observation] = []
            for index in range(3):
                events.append(f"sample:{index}")
                rows.append(Row(cpu._Stat(100 + index, 100, "R", 1)))
            return rows
    assert cpu._scan(Source(), {100}, time.monotonic() + 1) == {100: 3}
    assert events == ["sample:0", "sample:1", "sample:2", "validate", "validate", "validate"]


def test_pairing_reads_fdinfo_after_stat_open_and_rejects_reuse(monkeypatch: pytest.MonkeyPatch) -> None:
    events: list[str] = []
    class Proc(cpu._Proc):
        def __init__(self) -> None:
            pass
        def open(self, path: str, deadline: float | None = None) -> int:
            events.append("stat-open")
            return os.open("/dev/null", os.O_RDONLY)
        def pid(self, pidfd: int, deadline: float) -> int:
            events.append("fdinfo")
            return -1
    def pidfd(pid: int, deadline: float) -> int:
        events.append("pidfd")
        return os.open("/dev/null", os.O_RDONLY)
    monkeypatch.setattr(cpu, "_pidfd", pidfd)
    monkeypatch.setattr(cpu, "_stat", lambda text: cpu._Stat(100, 100, "Z", 999))
    before = len(list(Path("/proc/self/fd").iterdir()))
    with pytest.raises(cpu.Unavailable, match="generation mismatch"):
        Proc().pair(100, time.monotonic() + 1)
    assert events == ["pidfd", "stat-open", "fdinfo"]
    assert len(list(Path("/proc/self/fd").iterdir())) == before


def test_bounded_interrupts_and_deadline() -> None:
    calls = 0
    def interrupted() -> int:
        nonlocal calls
        calls += 1
        ctypes.set_errno(errno.EINTR)
        return -1
    with pytest.raises(cpu.Unavailable, match="interrupted"):
        cpu._native(interrupted, time.monotonic() + 1)
    assert calls == cpu._MAX_INTERRUPTS + 1
    with pytest.raises(cpu.Unavailable, match="deadline"):
        cpu._native(interrupted, time.monotonic() - 1)
    assert calls == cpu._MAX_INTERRUPTS + 1


def test_stat_and_fdinfo_refusals() -> None:
    for raw in ["Pid: -1\nPid: 100\n", "Pid: 0", "Pid: nope", ""]:
        with pytest.raises(cpu.Unavailable):
            cpu._fd_pid(raw)
    assert cpu._fd_pid("Pid: -1\n") == -1
    assert cpu._fd_pid("Pid: 100\n") == 100
    for field in ["-1", "18446744073709551616"]:
        raw = "100 (x) R 1 100 " + "0 " * 8 + field + " 0 0 0"
        with pytest.raises(cpu.Unavailable):
            cpu._stat(raw)


@pytest.fixture
def native_helper(tmp_path: Path) -> Path:
    source = Path(__file__).parents[2] / "rs/dagrun/tests/fixtures/proccpu-helper.c"
    target = tmp_path / "cpu-helper"
    subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", "-pthread", str(source), "-o", str(target)], check=True, timeout=30)
    return target


def _line(child: subprocess.Popen[str]) -> str:
    assert child.stdout is not None
    ready, _, _ = select.select([child.stdout], [], [], 5)
    assert ready, "native fixture output deadline"
    line: object = child.stdout.readline()
    assert isinstance(line, str)
    return line.strip()


def test_native_unreaped_zombie_and_reaping_keep_cpu_and_close_fds(native_helper: Path) -> None:
    before = len(list(Path("/proc/self/fd").iterdir()))
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None and child.stdout is not None
        zombie = int(_line(child))
        reader = cpu.ProcessGroupCpu(child.pid)
        try:
            assert reader.proc is not None
            pair = reader.proc.pair(zombie, time.monotonic() + 1)
            try:
                assert pair.original.state == "Z"
                assert pair.original.ticks > 0
                assert cpu._poll(pair.pidfd, time.monotonic() + 1) & select.POLLIN
                assert pair.valid(time.monotonic() + 1)
                first = reader.seconds()
                assert first >= pair.original.ticks / cpu._clk_tck()
                child.stdin.write("r"); child.stdin.flush()
                assert _line(child) == "reaped"
                assert not pair.valid(time.monotonic() + 1)
                # Force an observation after reaping without sleeping for cache expiry.
                with cpu._lock:
                    cpu._snapshot_at = float("-inf")
                assert reader.seconds() >= first
            finally:
                pair.close()
        finally:
            reader.close()
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)
        with pytest.raises(cpu.Unavailable, match="closed"):
            reader.seconds()
    assert len(list(Path("/proc/self/fd").iterdir())) == before


def test_native_exited_leader_with_live_worker(native_helper: Path) -> None:
    with subprocess.Popen([str(native_helper), "leader"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        assert _line(child) == "worker"
        reader = cpu.ProcessGroupCpu(child.pid)
        try:
            assert reader.owner is not None
            deadline = time.monotonic() + 5
            while cpu._stat(cpu._read(reader.owner.statfd, deadline)).state != "Z":
                assert time.monotonic() < deadline
                time.sleep(.001)
            assert cpu._poll(reader.owner.pidfd, deadline) == 0
            assert reader.owner.valid(deadline)
            before = reader.seconds()
            child.stdin.write("b"); child.stdin.flush()
            assert _line(child) == "worked"
            with cpu._lock:
                cpu._snapshot_at = float("-inf")
            assert reader.seconds() > before
        finally:
            reader.close()
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)


def test_native_cache_reuses_success_and_refusal_but_requires_owner(native_helper: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        _line(child)
        first = cpu.ProcessGroupCpu(child.pid)
        second = cpu.ProcessGroupCpu(child.pid)
        original = cpu._KernelSource.samples
        calls = 0
        def failure(source: cpu._KernelSource, groups: set[int], deadline: float) -> list[cpu._Observation]:
            nonlocal calls
            calls += 1
            raise cpu.Unavailable("injected scanner EACCES")
        try:
            assert first.key != second.key
            monkeypatch.setattr(cpu._KernelSource, "samples", failure)
            for reader in (first, second):
                with pytest.raises(cpu.Unavailable, match="EACCES"):
                    reader.seconds()
            assert calls == 1
            monkeypatch.setattr(cpu._KernelSource, "samples", original)
            with cpu._lock:
                cpu._snapshot_at = float("-inf")
            value = first.seconds()
            at = cpu._snapshot_at
            assert second.seconds() == value
            assert cpu._snapshot_at == at
            child.stdin.write("r"); child.stdin.flush()
            assert _line(child) == "reaped"
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)
            for reader in (first, second):
                with pytest.raises(cpu.Unavailable):
                    reader.seconds()  # A still-fresh successful cache cannot revive a gone owner.
        finally:
            first.close(); second.close()
            if child.poll() is None:
                child.kill(); child.wait(timeout=5)


def test_resource_and_scan_errors_are_unavailable_without_fd_leaks(native_helper: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        _line(child)
        reader = cpu.ProcessGroupCpu(child.pid)
        before = len(list(Path("/proc/self/fd").iterdir()))
        try:
            with monkeypatch.context() as patch:
                patch.setattr(cpu, "_MAX_MEMBERS", 1)
                with pytest.raises(cpu.Unavailable, match="member bound"):
                    reader.seconds()
            assert len(list(Path("/proc/self/fd").iterdir())) == before
            with monkeypatch.context() as patch:
                patch.setattr(resource, "getrlimit", lambda resource: (64, 64))
                with pytest.raises(cpu.Unavailable, match="descriptor reserve"):
                    cpu.ProcessGroupCpu(child.pid)
            assert len(list(Path("/proc/self/fd").iterdir())) == before
            with monkeypatch.context() as patch:
                patch.setattr(cpu, "_MAX_GROUPS", 0)
                with pytest.raises(cpu.Unavailable, match="owner bound"):
                    cpu.ProcessGroupCpu(child.pid)
            assert len(list(Path("/proc/self/fd").iterdir())) == before
        finally:
            reader.close()
            child.stdin.write("r"); child.stdin.flush()
            assert _line(child) == "reaped"
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)


def test_pid_t_range_is_checked_before_opening_proc(monkeypatch: pytest.MonkeyPatch) -> None:
    def forbidden_open(self: cpu._Proc, path: Path) -> None:
        raise AssertionError("invalid PID must be refused before opening proc")
    monkeypatch.setattr(cpu._Proc, "__init__", forbidden_open)
    for pid in [0, 1, -1, 2147483648, 18446744073709551616]:
        assert cpu.subtree_cpu_seconds(pid) is None
        assert cpu.subtree_cpu_seconds(pid, proc_root=Path("/missing")) is None
        with pytest.raises(cpu.Unavailable, match="invalid process group"):
            cpu.ProcessGroupCpu(pid)


def test_new_owned_reader_evicts_expired_compatibility_registration(native_helper: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        _line(child)
        old = cpu.ProcessGroupCpu(child.pid)
        try:
            old.seconds()
            with cpu._lock:
                cpu._compat[child.pid] = (old, time.monotonic() - 1)
            monkeypatch.setattr(cpu, "_MAX_GROUPS", 1)
            new = cpu.ProcessGroupCpu(child.pid)
            try:
                assert new.key != old.key
                assert old.owner is None
                assert new.seconds() >= 0
                with pytest.raises(cpu.Unavailable, match="closed"):
                    old.seconds()
            finally:
                new.close()
        finally:
            old.close()
            with cpu._lock:
                cpu._compat.pop(child.pid, None)
            child.stdin.write("r"); child.stdin.flush()
            assert _line(child) == "reaped"
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)


def test_native_nonleader_exec_preserves_held_identity(native_helper: Path) -> None:
    with subprocess.Popen([str(native_helper), "exec"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        assert _line(child) == "thread"
        reader = cpu.ProcessGroupCpu(child.pid)
        try:
            assert reader.owner is not None and reader.proc is not None
            key = reader.key
            before = reader.seconds()
            child.stdin.write("e"); child.stdin.flush()
            assert _line(child) == "executed"
            assert reader.proc.pid(reader.owner.pidfd, time.monotonic() + 1) == child.pid
            assert cpu._stat(cpu._read(reader.owner.statfd, time.monotonic() + 1)).pid == child.pid
            assert reader.owner.valid(time.monotonic() + 1)
            child.stdin.write("b"); child.stdin.flush()
            assert _line(child) == "worked"
            with cpu._lock:
                cpu._snapshot_at = float("-inf")
            after = reader.seconds()
            assert after > before
            assert reader.key == key
            print(json.dumps({"native": "nonleader-exec", "pid": child.pid, "owner": key, "before_cpu": before, "after_cpu": after}))
            child.stdin.write("x"); child.stdin.flush()
            assert child.wait(timeout=5) == 0
        finally:
            reader.close()
            if child.poll() is None:
                child.kill(); child.wait(timeout=5)


def test_native_ptrace_reparent_keeps_zombie_until_real_parent_reaps(native_helper: Path) -> None:
    with subprocess.Popen([str(native_helper), "trace"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        tracee = int(_line(child))
        reader = cpu.ProcessGroupCpu(child.pid)
        try:
            assert reader.proc is not None and reader.owner is not None
            pair = reader.proc.pair(tracee, time.monotonic() + 1)
            def waited_ticks() -> int:
                assert reader.owner is not None
                fields = cpu._read(reader.owner.statfd, time.monotonic() + 1).rsplit(")", 1)[1].split()
                return int(fields[13]) + int(fields[14])
            try:
                assert pair.original.state == "Z" and pair.original.ticks > 0
                assert pair.valid(time.monotonic() + 1)
                assert f"TracerPid:\t{child.pid}\n" in Path(f"/proc/{tracee}/status").read_text()
                before_waited = waited_ticks()
                before = reader.seconds()
                child.stdin.write("t"); child.stdin.flush()
                assert _line(child) == "detached"
                assert "TracerPid:\t0\n" in Path(f"/proc/{tracee}/status").read_text()
                assert waited_ticks() == before_waited, "ptracer EXIT_TRACE branch must not credit child CPU"
                assert pair.valid(time.monotonic() + 1)
                assert cpu._stat(cpu._read(pair.statfd, time.monotonic() + 1)).state == "Z"
                child.stdin.write("r"); child.stdin.flush()
                assert _line(child) == "reaped"
                assert not pair.valid(time.monotonic() + 1)
                with cpu._lock:
                    cpu._snapshot_at = float("-inf")
                after = reader.seconds()
                assert after >= before
                print(json.dumps({"native": "ptrace-reparent", "pid": child.pid, "tracee": tracee, "zombie_ticks": pair.original.ticks, "ptracer_waited_ticks": before_waited, "before_cpu": before, "after_cpu": after}))
            finally:
                pair.close()
            child.stdin.write("x"); child.stdin.flush()
            assert child.wait(timeout=5) == 0
        finally:
            reader.close()
            if child.poll() is None:
                child.kill(); child.wait(timeout=5)


def test_native_public_compatibility_and_unusual_comm(native_helper: Path) -> None:
    with subprocess.Popen([str(native_helper), "comm"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        zombie = int(_line(child))
        raw = Path(f"/proc/{zombie}/stat").read_bytes()
        assert b"p\xff)\n(\x80" in raw
        first = cpu.subtree_cpu_seconds(child.pid)
        assert first is not None and first > 0
        key = cpu._compat[child.pid][0].key
        assert cpu.subtree_cpu_seconds(child.pid) == first
        assert cpu._compat[child.pid][0].key == key
        explicit = cpu.subtree_cpu_seconds(child.pid, proc_root=Path("/proc"))
        assert explicit is not None and explicit >= first
        time.sleep(cpu._SNAPSHOT_TTL_S + .05)
        later = cpu.subtree_cpu_seconds(child.pid)
        assert later is not None and later >= first
        assert cpu._compat[child.pid][0].key != key
        child.stdin.write("r"); child.stdin.flush()
        assert _line(child) == "reaped"
        child.stdin.write("x"); child.stdin.flush()
        child.wait(timeout=5)
        assert cpu.subtree_cpu_seconds(child.pid) is None
        assert child.pid not in cpu._compat


def test_clock_rate_errors_are_typed_at_owned_and_compatibility_apis(native_helper: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        _line(child)
        reader = cpu.ProcessGroupCpu(child.pid)
        try:
            assert reader.seconds() > 0
            for value in (100, 250):
                with monkeypatch.context() as patch:
                    patch.setattr(os, "sysconf", lambda name, value=value: value)
                    assert cpu._clk_tck() == float(value)
            for value in (0, -1):
                with monkeypatch.context() as patch:
                    patch.setattr(os, "sysconf", lambda name, value=value: value)
                    with pytest.raises(cpu.Unavailable, match="SC_CLK_TCK unavailable"):
                        reader.seconds()
                    assert cpu.subtree_cpu_seconds(child.pid) is None
            for error in (OSError(errno.EIO, "injected query error"), ValueError("injected unsupported name")):
                def fail(name: str, error: Exception = error) -> int:
                    raise error
                with monkeypatch.context() as patch:
                    patch.setattr(os, "sysconf", fail)
                    with pytest.raises(cpu.Unavailable, match="SC_CLK_TCK unavailable"):
                        reader.seconds()
                    assert cpu.subtree_cpu_seconds(child.pid) is None
            assert reader.seconds() > 0
        finally:
            reader.close()
            child.stdin.write("r"); child.stdin.flush()
            assert _line(child) == "reaped"
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)
            assert cpu.subtree_cpu_seconds(child.pid) is None


def test_missing_namespace_metadata_is_distinct_and_closes_fds(native_helper: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    original = cpu._read
    def without_namespace(fd: int, deadline: float) -> str:
        text = original(fd, deadline)
        if os.readlink(f"/proc/self/fd/{fd}").endswith("/status"):
            return "\n".join(line for line in text.splitlines() if not line.startswith("NSpid:"))
        return text
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None
        _line(child)
        before = len(list(Path("/proc/self/fd").iterdir()))
        with monkeypatch.context() as patch:
            patch.setattr(cpu, "_read", without_namespace)
            with pytest.raises(cpu.Unavailable, match="namespace metadata unavailable: NSpid missing"):
                cpu.ProcessGroupCpu(child.pid)
            assert cpu.subtree_cpu_seconds(child.pid) is None
        assert len(list(Path("/proc/self/fd").iterdir())) == before
        child.stdin.write("r"); child.stdin.flush()
        assert _line(child) == "reaped"
        child.stdin.write("x"); child.stdin.flush()
        child.wait(timeout=5)


@pytest.mark.parametrize("groups", [1, 4, 16])
def test_native_shared_sampling_cost_and_availability(native_helper: Path, groups: int) -> None:
    children: list[subprocess.Popen[str]] = []
    readers: list[cpu.ProcessGroupCpu] = []
    before = len(list(Path("/proc/self/fd").iterdir()))
    try:
        for _ in range(groups):
            child = subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            children.append(child)
            _line(child)
            readers.append(cpu.ProcessGroupCpu(child.pid))
        with cpu._lock:
            cpu._snapshot_at = float("-inf")
        first_start = time.perf_counter(); first_cpu = time.process_time()
        values = [reader.seconds() for reader in readers]
        first_cpu = time.process_time() - first_cpu; first_seconds = time.perf_counter() - first_start
        assert all(value > 0 for value in values)
        captured = cpu._snapshot_at
        cached_start = time.perf_counter(); cached_cpu = time.process_time()
        for _ in range(10):
            for reader in readers:
                assert reader.seconds() > 0
        cached_cpu = time.process_time() - cached_cpu; cached_seconds = time.perf_counter() - cached_start
        # Report whether this batch fit in one cache interval instead of changing its TTL.
        one_snapshot = cpu._snapshot_at == captured
        held = len(list(Path("/proc/self/fd").iterdir()))
        print("PROCCPU_COST " + json.dumps({"edition": "python", "groups": groups, "matching_processes": 2 * groups, "first_wall_s": first_seconds, "first_self_cpu_s": first_cpu, "cached_wall_s": cached_seconds, "cached_self_cpu_s": cached_cpu, "cached_calls": groups * 10, "one_cached_snapshot": one_snapshot, "fd_before": before, "fd_held": held, "rlimit_nofile": resource.getrlimit(resource.RLIMIT_NOFILE)}))
    finally:
        for reader in readers:
            reader.close()
        for child in children:
            assert child.stdin is not None
            child.stdin.write("r"); child.stdin.flush()
            assert _line(child) == "reaped"
            child.stdin.write("x"); child.stdin.flush()
            child.wait(timeout=5)
            child.stdin.close()
            assert child.stdout is not None
            child.stdout.close()
    assert len(list(Path("/proc/self/fd").iterdir())) == before


def test_shared_session_cache_corpus() -> None:
    """The Rust edition runs the same corpus, so both make the same read decisions."""
    data: object = json.loads((Path(__file__).parents[2] / "rs/dagrun/tests/fixtures/proccpu-session-cache.json").read_text())
    assert isinstance(data, list)

    def allocator(raw: object) -> cpu._Allocator | None:
        if raw is None:
            return None
        assert isinstance(raw, dict)
        return cpu._Allocator(raw["last_pid"], raw["tasks"], raw["forks"])

    for case in data:
        assert isinstance(case, dict)
        pid_max = case.get("pid_max")
        cache = cpu._SessionCache()
        for index, step in enumerate(case["steps"]):
            start = allocator(step["allocator"])
            end = allocator(step["end"]) if "end" in step else start
            sessions = set(step["group_sessions"])
            cache.begin(start, pid_max)
            read: list[int] = []
            skipped: list[tuple[int, int]] = []
            seen: set[int] = set()
            for process in step["processes"]:
                pid, session = process["pid"], process["session"]
                seen.add(pid)
                if cache.skip(pid, sessions):
                    skipped.append((pid, session))
                else:
                    cache.record(pid, session)
                    read.append(pid)
            for pid, session in skipped:
                if start is None or end is None or pid_max is None or cpu._reallocated(start, end, pid_max, pid):
                    cache.record(pid, session)
                    read.append(pid)
            cache.retain(seen)
            assert sorted(read) == sorted(step["read"]), f"{case['name']} step {index}"


def test_allocator_records_parse_and_refuse_ambiguity() -> None:
    assert cpu._parse_loadavg("170.04 167.90 165.36 49/63307 3954637\n") == (3954637, 63307)
    assert cpu._parse_loadavg("1.0 1.0 1.0 1/2") is None
    assert cpu._parse_forks("cpu  1 2 3\nprocesses 1547387564\nprocs_running 4\n") == 1547387564
    assert cpu._parse_forks("cpu 1\n") is None
    assert cpu._parse_forks("processes 1\nprocesses 2\n") is None


def test_native_second_snapshot_skips_other_sessions(native_helper: Path) -> None:
    """On the live host a second snapshot reads only the PIDs it could not skip, and
    measures the same members."""
    with subprocess.Popen([str(native_helper), "zombie"], start_new_session=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        assert child.stdin is not None and child.stdout is not None
        _line(child)
        reader = cpu.ProcessGroupCpu(child.pid)
        try:
            with cpu._lock:
                cpu._snapshot_at = float("-inf")
                cpu._census.begin(None, None)
            first = reader.seconds()
            cold = cpu._snapshot_reads
            with cpu._lock:
                cpu._snapshot_at = float("-inf")
            second = reader.seconds()
            warm = cpu._snapshot_reads
            print("PROCCPU_SESSION_CACHE", json.dumps({"edition": "python", "cold_reads": cold, "warm_reads": warm}))
            assert first > 0 and second >= first
            assert warm * 4 < cold, f"the second snapshot read {warm} stat files of the {cold} the first read"
        finally:
            reader.close()
            child.stdin.write("r"); child.stdin.flush()
            assert _line(child) == "reaped"
            child.stdin.write("x"); child.stdin.flush()
            assert child.wait(5) == 0
