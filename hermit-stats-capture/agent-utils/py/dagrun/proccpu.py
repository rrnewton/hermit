"""Generation-bound, best-effort process-group CPU observations.

Each member contributes its original own plus waited-child ticks. All samples precede
all lifecycle validations: a child credited to a sampled parent must subsequently be
excluded. A pidfd alone is insufficient: an unreaped zombie still owns its CPU. For a
terminal pidfd we therefore reread the *held, paired* stat file and retain only Z.

This is not cgroup accounting. Escaped process groups and activity between samples
can be missed. Missing, ambiguous or resource-limited observations are unavailable,
never zero. Successful observations are shared for at most half a second.
One shared observation retains at most 1024 members across 256 registered groups,
with a one-second scan deadline and 64 descriptors reserved below the soft limit.
Descriptor pressure can lower this ceiling. A shared refusal affects all readers
of that snapshot; it does not permit a partial population to be reported.
"""

from __future__ import annotations

import ctypes
import errno
import os
import resource
import select
import threading
import time
import weakref
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Protocol

__all__ = ["CPU_SOURCE_CGROUP", "CPU_SOURCE_PROCFS", "ProcessGroupCpu", "Unavailable", "subtree_cpu_seconds"]

CPU_SOURCE_CGROUP = "cgroup"
CPU_SOURCE_PROCFS = "procfs-subtree"
_MAX_RECORD = 16384
_MAX_MEMBERS = 1024
_MAX_GROUPS = 256
_MAX_ENTRIES = 65536
_FD_RESERVE = 64
_MAX_INTERRUPTS = 16
_SCAN_SECONDS = 1.0
_SNAPSHOT_TTL_S = 0.5
_U64_MAX = (1 << 64) - 1


class Unavailable(Exception):
    """No complete, authenticated CPU observation is available."""


def _clk_tck() -> float:
    """A missing unit conversion is unavailable, never an invented rate."""
    try:
        value = os.sysconf("SC_CLK_TCK")
    except (OSError, ValueError) as exc:
        raise Unavailable(f"SC_CLK_TCK unavailable: {exc}") from exc
    if value <= 0:
        raise Unavailable(f"SC_CLK_TCK unavailable: nonpositive value {value}")
    return float(value)


def _check(deadline: float) -> None:
    if time.monotonic() >= deadline:
        raise Unavailable("scan deadline")


def _native(call: Callable[[], int], deadline: float) -> int:
    for _ in range(_MAX_INTERRUPTS + 1):
        _check(deadline)
        result = call()
        if result >= 0:
            return result
        code = ctypes.get_errno()
        if code != errno.EINTR:
            raise OSError(code, os.strerror(code))
    raise Unavailable("system call interrupted repeatedly")


def _libc() -> ctypes.CDLL:
    return ctypes.CDLL(None, use_errno=True)


def _openat(directory: int, path: str, deadline: float) -> int:
    libc = _libc()
    libc.openat.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
    libc.openat.restype = ctypes.c_int
    return _native(lambda: int(libc.openat(directory, os.fsencode(path), os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)), deadline)


def _pidfd(pid: int, deadline: float) -> int:
    # CPython's pidfd_open is one direct syscall, including on older libc versions
    # without a pidfd_open symbol. Bound its EINTR handling here, without an ABI table.
    for _ in range(_MAX_INTERRUPTS + 1):
        _check(deadline)
        try:
            return os.pidfd_open(pid, 0)
        except InterruptedError:
            continue
        except AttributeError as exc:
            raise Unavailable("pidfd_open unavailable") from exc
    raise Unavailable("pidfd_open interrupted repeatedly")


class _Pollfd(ctypes.Structure):
    _fields_ = [("fd", ctypes.c_int), ("events", ctypes.c_short), ("revents", ctypes.c_short)]


def _poll(fd: int, deadline: float) -> int:
    libc = _libc()
    libc.poll.argtypes = [ctypes.POINTER(_Pollfd), ctypes.c_ulong, ctypes.c_int]
    libc.poll.restype = ctypes.c_int
    event = _Pollfd(fd, select.POLLIN, 0)
    count = _native(lambda: int(libc.poll(ctypes.byref(event), 1, 0)), deadline)
    return 0 if count == 0 else int(event.revents)


def _read(fd: int, deadline: float, limit: int = _MAX_RECORD) -> str:
    """Offset zero is essential: procfs must regenerate the phase-two record."""
    data = bytearray()
    libc = _libc()
    libc.pread64.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_longlong]
    libc.pread64.restype = ctypes.c_ssize_t
    while len(data) <= limit:
        buffer = ctypes.create_string_buffer(1024)
        size = _native(lambda: int(libc.pread64(fd, buffer, len(buffer), len(data))), deadline)
        if size == 0:
            return data.decode("ascii", errors="surrogateescape")
        data.extend(buffer.raw[:size])
    raise Unavailable("oversized proc record")


@dataclass(frozen=True)
class _Stat:
    pid: int
    group: int
    state: str
    ticks: int
    session: int = 0


def _stat(text: str) -> _Stat:
    try:
        opening = text.index(" (")
        close = text.rindex(")")
        pid = int(text[:opening])
        fields = text[close + 1 :].split()
        group = int(fields[2])
        session = int(fields[3])
        cpu = [int(fields[index]) for index in (11, 12, 13, 14)]
        if close < opening or pid <= 0 or group < 0 or len(fields[0]) != 1:
            raise ValueError("identity")
        if any(value < 0 or value > _U64_MAX for value in cpu) or sum(cpu) > _U64_MAX:
            raise ValueError("CPU overflow")
        return _Stat(pid, group, fields[0], sum(cpu), session)
    except (ValueError, IndexError) as exc:
        raise Unavailable("malformed stat") from exc


def _fd_pid(text: str) -> int:
    values = [line[4:].strip() for line in text.splitlines() if line.startswith("Pid:")]
    try:
        if len(values) != 1:
            raise ValueError("missing or duplicate Pid")
        value = int(values[0])
        if value < -1 or value == 0:
            raise ValueError("invalid Pid")
        return value
    except ValueError as exc:
        raise Unavailable("malformed pidfd fdinfo") from exc


def _gone(exc: OSError) -> bool:
    return exc.errno in (errno.ENOENT, errno.ESRCH)


def _allow_fds(required: int) -> None:
    soft, _ = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft == resource.RLIM_INFINITY:
        return
    used = 0
    with os.scandir("/proc/self/fd") as entries:
        for _ in entries:
            used += 1
            if used > _MAX_ENTRIES:
                raise Unavailable("descriptor census bound")
    if used + required + _FD_RESERVE > soft:
        raise Unavailable("descriptor reserve")


# PIDs below this are never handed out again after the allocator wraps (Linux
# RESERVED_PIDS), so a lap of the PID space is at most pid_max - _RESERVED_PIDS steps.
_RESERVED_PIDS = 300
# /proc/stat holds one line per CPU; this bounds it on hosts with thousands of CPUs.
_MAX_STAT_RECORD = 1 << 20


@dataclass(frozen=True)
class _Allocator:
    """Where the kernel's PID allocator stands: the last PID it handed out and the live
    task count (/proc/loadavg), and the tasks created since boot (/proc/stat processes).
    Threads take their IDs from the same space, and all three count them."""

    last_pid: int
    tasks: int
    forks: int


def _parse_loadavg(text: str) -> tuple[int, int] | None:
    fields = text.split()
    try:
        _, tasks = fields[3].split("/", 1)
        return int(fields[4]), int(tasks)
    except (IndexError, ValueError):
        return None


def _parse_forks(text: str) -> int | None:
    values = [line[len("processes ") :] for line in text.splitlines() if line.startswith("processes ")]
    if len(values) != 1:
        return None
    try:
        return int(values[0].strip())
    except ValueError:
        return None


def _reallocated(start: _Allocator, end: _Allocator, pid_max: int, pid: int) -> bool:
    """Whether the allocator may have handed out pid again on its way from start to end.
    It walks the PID space in a circle, one step per new task or per PID still in use, so
    every PID it handed out lies on the arc after start.last_pid up to end.last_pid, unless
    it may have gone the whole way round: then any PID may have been."""
    created = end.forks - start.forks
    if created < 0:
        return True
    if created + max(start.tasks, end.tasks) >= max(pid_max - _RESERVED_PIDS, 0):
        return True
    if created == 0:
        return False
    first, last = start.last_pid, end.last_pid
    if first < last:
        return first < pid <= last
    return pid > first or pid <= last


class _SessionCache:
    """The session of each PID an earlier snapshot read. A process joins a group only
    through setpgid, only within its own session, and changes its session only through
    setsid, which makes its session its own PID. So a cached PID's session is now either the
    cached one or the PID itself, and the PID is skipped only when neither is the session of
    a registered group: then it can never be a member of one. That covers a child read
    between fork and setsid that then leads a registered group. The cache holds sessions,
    not verdicts, so a group registered later re-evaluates every cached PID without a read.
    An entry is trusted only while the kernel cannot have reused its PID; when that cannot
    be proven the cache is emptied and the next snapshot reads every PID."""

    def __init__(self) -> None:
        self.sessions: dict[int, int] = {}
        self.at: _Allocator | None = None

    def begin(self, now: _Allocator | None, pid_max: int | None) -> None:
        previous = self.at
        if previous is not None and now is not None and pid_max is not None:
            self.sessions = {
                pid: session for pid, session in self.sessions.items() if not _reallocated(previous, now, pid_max, pid)
            }
        else:
            self.sessions.clear()
        self.at = now

    def skip(self, pid: int, sessions: set[int]) -> bool:
        if pid in sessions:
            return False
        session = self.sessions.get(pid)
        return session is not None and session not in sessions

    def record(self, pid: int, session: int) -> None:
        self.sessions[pid] = session

    def retain(self, seen: set[int]) -> None:
        self.sessions = {pid: session for pid, session in self.sessions.items() if pid in seen}


class _Proc:
    def __init__(self, path: Path) -> None:
        _allow_fds(2)
        self.fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW)
        try:
            # A real procfs has a kernel-generated mount identity; ordinary fixture trees
            # cannot authenticate processes. PID equality also refuses a foreign PID view.
            libc = ctypes.CDLL(None, use_errno=True)
            buffer = ctypes.create_string_buffer(256)
            libc.fstatfs.argtypes = [ctypes.c_int, ctypes.c_void_p]
            libc.fstatfs.restype = ctypes.c_int
            if libc.fstatfs(self.fd, buffer) != 0:
                code = ctypes.get_errno()
                raise OSError(code, os.strerror(code))
            if ctypes.c_long.from_buffer(buffer).value != 0x9FA0:
                raise Unavailable("root is not procfs")
            statusfd = self.open("self/status")
            try:
                status = _read(statusfd, time.monotonic() + _SCAN_SECONDS)
            finally:
                os.close(statusfd)
            namespaces = [line.split()[1:] for line in status.splitlines() if line.startswith("NSpid:")]
            if not namespaces:
                raise Unavailable("procfs namespace metadata unavailable: NSpid missing")
            if namespaces != [[str(os.getpid())]]:
                raise Unavailable("procfs PID namespace mismatch")
            with os.fdopen(self.open("self/stat"), "rb") as record:
                own = _stat(_read(record.fileno(), time.monotonic() + _SCAN_SECONDS))
            if own.pid != os.getpid():
                raise Unavailable("procfs PID namespace mismatch")
        except BaseException:
            self.close()
            raise

    def close(self) -> None:
        if self.fd >= 0:
            os.close(self.fd)
            self.fd = -1

    def open(self, path: str, deadline: float | None = None) -> int:
        return _openat(self.fd, path, time.monotonic() + _SCAN_SECONDS if deadline is None else deadline)

    def _record(self, path: str, deadline: float, limit: int = _MAX_RECORD) -> str | None:
        try:
            fd = self.open(path, deadline)
        except OSError:
            return None
        try:
            return _read(fd, deadline, limit)
        except (OSError, Unavailable):
            return None
        finally:
            os.close(fd)

    def allocator(self, deadline: float) -> _Allocator | None:
        """The allocator position, or None if procfs does not report it."""
        loadavg = self._record("loadavg", deadline)
        stat = self._record("stat", deadline, _MAX_STAT_RECORD)
        position = None if loadavg is None else _parse_loadavg(loadavg)
        forks = None if stat is None else _parse_forks(stat)
        if position is None or forks is None:
            return None
        return _Allocator(position[0], position[1], forks)

    def pid_max(self, deadline: float) -> int | None:
        text = self._record("sys/kernel/pid_max", deadline)
        try:
            return None if text is None else int(text.strip())
        except ValueError:
            return None

    def pid(self, pidfd: int, deadline: float) -> int:
        fd = self.open(f"self/fdinfo/{pidfd}", deadline)
        try:
            return _fd_pid(_read(fd, deadline))
        finally:
            os.close(fd)

    def pair(self, pid: int, deadline: float) -> _Pair:
        _check(deadline)
        _allow_fds(3)
        pidfd = _pidfd(pid, deadline)
        statfd = -1
        try:
            statfd = self.open(f"{pid}/stat", deadline)
            original = _stat(_read(statfd, deadline))
            # Read AFTER opening stat. A detached original pidfd must never authenticate
            # the stat of a recycled numeric PID, including a new zombie.
            if self.pid(pidfd, deadline) != pid or original.pid != pid:
                raise Unavailable("pidfd/stat generation mismatch")
            return _Pair(pidfd, statfd, original)
        except BaseException:
            os.close(pidfd)
            if statfd >= 0:
                os.close(statfd)
            raise


class _Pair:
    def __init__(self, pidfd: int, statfd: int, original: _Stat) -> None:
        self.pidfd = pidfd
        self.statfd = statfd
        self.original = original

    def close(self) -> None:
        if self.pidfd >= 0:
            os.close(self.pidfd)
            os.close(self.statfd)
            self.pidfd = self.statfd = -1

    def valid(self, deadline: float) -> bool:
        _check(deadline)
        bits = _poll(self.pidfd, deadline)

        def fresh() -> _Stat | None:
            try:
                return _stat(_read(self.statfd, deadline))
            except OSError as exc:
                if exc.errno == errno.ESRCH:
                    return None
                raise

        return _accept(bits, self.original, fresh)


def _accept(bits: int, original: _Stat, fresh: Callable[[], _Stat | None]) -> bool:
    if bits == 0:
        return True  # flags=0 pidfd includes a live worker after leader exit.
    if bits & ~(select.POLLIN | select.POLLHUP | select.POLLRDNORM):
        raise Unavailable("unexpected pidfd poll result")
    if not bits & (select.POLLIN | select.POLLHUP):
        raise Unavailable("ambiguous pidfd poll result")
    current = fresh()
    if current is None:
        return False
    if current.pid != original.pid:
        raise Unavailable("held stat identity changed")
    if current.state == "Z":
        return True
    if current.state == "X":
        return False
    raise Unavailable("terminal pidfd with ambiguous task state")


class _Observation(Protocol):
    original: _Stat

    def valid(self, deadline: float) -> bool: ...
    def close(self) -> None: ...


class _Source(Protocol):
    def samples(self, groups: set[int], deadline: float) -> list[_Observation]: ...


class _KernelSource:
    """The live procfs source. sessions are the registered groups' sessions, and cache
    persists across snapshots for the shared registry."""

    def __init__(self, proc: _Proc, sessions: set[int] | None = None, cache: _SessionCache | None = None) -> None:
        self.proc = proc
        self.sessions = set() if sessions is None else sessions
        self.cache = _SessionCache() if cache is None else cache
        self.reads = 0

    def _consider(self, pid: int, groups: set[int], rows: list[_Observation], deadline: float) -> None:
        """Read pid's stat, cache its session and, if it is in one of groups, pair it."""
        self.reads += 1
        try:
            fd = self.proc.open(f"{pid}/stat", deadline)
            try:
                hint = _stat(_read(fd, deadline))
            finally:
                os.close(fd)
            self.cache.record(pid, hint.session)
            if hint.group not in groups:
                return
            if len(rows) >= _MAX_MEMBERS:
                raise Unavailable("member bound")
            pair = self.proc.pair(pid, deadline)
        except OSError as exc:
            if _gone(exc):
                return
            raise
        if pair.original.group not in groups:
            pair.close()
        else:
            rows.append(pair)

    def samples(self, groups: set[int], deadline: float) -> list[_Observation]:
        rows: list[_Observation] = []
        try:
            pid_max = self.proc.pid_max(deadline)
            start = self.proc.allocator(deadline)
            self.cache.begin(start, pid_max)
            seen: set[int] = set()
            skipped: list[int] = []
            with os.scandir(self.proc.fd) as entries:
                for index, entry in enumerate(entries):
                    _check(deadline)
                    if index >= _MAX_ENTRIES:
                        raise Unavailable("proc entry bound")
                    if not entry.name.isascii() or not entry.name.isdigit():
                        continue
                    pid = int(entry.name)
                    seen.add(pid)
                    if self.cache.skip(pid, self.sessions):
                        skipped.append(pid)
                        continue
                    self._consider(pid, groups, rows, deadline)
            # A skipped PID may have been handed to a new task while this snapshot ran.
            end = self.proc.allocator(deadline)
            for pid in skipped:
                if start is None or end is None or pid_max is None or _reallocated(start, end, pid_max, pid):
                    _check(deadline)
                    self._consider(pid, groups, rows, deadline)
            self.cache.retain(seen)
            return rows
        except BaseException:
            for row in rows:
                row.close()
            raise


def _scan(source: _Source, groups: set[int], deadline: float) -> dict[int, int]:
    rows = source.samples(groups, deadline)
    try:
        # This barrier is global to the whole aggregate. Do not validate chunks while
        # later members' original counters can still be sampled after CPU transfer.
        totals: dict[int, int] = {}
        for row in rows:
            _check(deadline)
            if row.valid(deadline):
                group = row.original.group
                value = totals.get(group, 0) + row.original.ticks
                if value > _U64_MAX:
                    raise Unavailable("aggregate overflow")
                totals[group] = value
        return totals
    finally:
        for row in rows:
            row.close()


_lock = threading.RLock()
_owners: weakref.WeakValueDictionary[int, ProcessGroupCpu] = weakref.WeakValueDictionary()
_compat: dict[int, tuple[ProcessGroupCpu, float]] = {}
_next_owner = 0
_snapshot_at = float("-inf")
_snapshot: dict[int, int] = {}
_snapshot_keys: set[int] = set()
_snapshot_error: str | None = None
_snapshot_started = float("-inf")
_snapshot_reads = 0
_census = _SessionCache()


def _expire_compatibility(now: float) -> None:
    # Called under _lock by either API so abandoned compatibility registrations
    # cannot consume the owned-reader capacity indefinitely.
    for key, (old, used) in list(_compat.items()):
        if now - used > _SNAPSHOT_TTL_S:
            old.close()
            del _compat[key]


class ProcessGroupCpu:
    """Owned invocation identity. Create at spawn and close after its monitor stops.

    ``seconds`` returns a measurement or raises ``Unavailable``. The object never
    rebinds to a later occupant of the same PID/process-group number.
    """

    def __init__(self, pgid: int, *, proc_root: Path = Path("/proc")) -> None:
        global _next_owner
        self.proc: _Proc | None = None
        self.owner: _Pair | None = None
        self.pgid = pgid
        self.session = 0
        self.key = -1
        self.shared = proc_root == Path("/proc")
        if not 1 < pgid <= 2147483647:
            raise Unavailable("invalid process group")
        try:
            with _lock:
                _expire_compatibility(time.monotonic())
                if len(_owners) >= _MAX_GROUPS:
                    raise Unavailable("owner bound")
                proc = _Proc(proc_root)
                self.proc = proc
                owner = proc.pair(pgid, time.monotonic() + _SCAN_SECONDS)
                self.owner = owner
                if owner.original.group != pgid:
                    raise Unavailable("owner is not the process-group leader")
                # A group leader cannot leave its session, so this is the group's session.
                self.session = owner.original.session
                _next_owner += 1
                self.key = _next_owner
                if self.shared:
                    _owners[self.key] = self
        except (OSError, Unavailable) as exc:
            self.close()
            raise Unavailable(str(exc)) from exc

    def close(self) -> None:
        """Release only this observation's descriptors, never signal the process."""
        with _lock:
            _owners.pop(self.key, None)
            if self.owner is not None:
                self.owner.close()
                self.owner = None
            if self.proc is not None:
                self.proc.close()
                self.proc = None

    def __del__(self) -> None:
        self.close()

    def _authenticate(self, deadline: float) -> None:
        if self.owner is None or self.proc is None:
            raise Unavailable("closed owner")
        if self.proc.pid(self.owner.pidfd, deadline) != self.pgid:
            raise Unavailable("owner generation gone")
        current = _stat(_read(self.owner.statfd, deadline))
        if current.pid != self.pgid or current.group != self.pgid:
            raise Unavailable("owner identity changed")
        if not self.owner.valid(deadline):
            raise Unavailable("owner dead")

    def seconds(self) -> float:
        """Return checked own+waited CPU seconds from one authenticated observation."""
        global _snapshot_at, _snapshot, _snapshot_keys, _snapshot_error, _snapshot_started, _snapshot_reads
        try:
            with _lock:
                deadline = time.monotonic() + _SCAN_SECONDS
                self._authenticate(deadline)
                assert self.proc is not None
                if not self.shared:
                    totals = _scan(_KernelSource(self.proc), {self.pgid}, deadline)
                    self._authenticate(deadline)
                    if self.pgid not in totals:
                        raise Unavailable("no measured members")
                    return totals[self.pgid] / _clk_tck()
                if self.key not in _snapshot_keys or time.monotonic() - _snapshot_at > _SNAPSHOT_TTL_S:
                    owners = list(_owners.values())
                    active: list[ProcessGroupCpu] = []
                    for owner in owners:
                        try:
                            owner._authenticate(deadline)
                        except (OSError, Unavailable):
                            continue
                        active.append(owner)
                    _snapshot_started = time.monotonic()
                    _snapshot_keys = {owner.key for owner in owners}
                    source = _KernelSource(self.proc, {owner.session for owner in active}, _census)
                    try:
                        totals = _scan(source, {owner.pgid for owner in active}, deadline)
                    except (OSError, Unavailable) as exc:
                        _snapshot = {}
                        _snapshot_error = str(exc)
                    else:
                        values: dict[int, int] = {}
                        for owner in active:
                            try:
                                owner._authenticate(deadline)
                            except (OSError, Unavailable):
                                continue
                            if owner.pgid in totals:
                                values[owner.key] = totals[owner.pgid]
                        _snapshot = values
                        _snapshot_error = None
                    _snapshot_reads = source.reads
                    _snapshot_at = time.monotonic()
                self._authenticate(deadline)
                if _snapshot_at - _snapshot_started > _SCAN_SECONDS:
                    raise Unavailable("scan deadline")
                if _snapshot_error is not None:
                    raise Unavailable(_snapshot_error)
                if self.key not in _snapshot:
                    raise Unavailable("no measured members")
                return _snapshot[self.key] / _clk_tck()
        except OSError as exc:
            raise Unavailable(str(exc)) from exc


def subtree_cpu_seconds(pgid: int, *, proc_root: Path | None = None) -> float | None:
    """Compatibility projection; live callers should retain ``ProcessGroupCpu``.

    Custom roots must expose the same authentic procfs/PID view, not synthetic stat
    files. Failed authentication and unsupported pidfds return None, never old sums.
    """
    if not 1 < pgid <= 2147483647:
        return None
    try:
        if proc_root is not None:
            owner = ProcessGroupCpu(pgid, proc_root=proc_root)
            try:
                return owner.seconds()
            finally:
                owner.close()
        with _lock:
            now = time.monotonic()
            _expire_compatibility(now)
            entry = _compat.get(pgid)
            if entry is not None:
                try:
                    entry[0]._authenticate(now + _SCAN_SECONDS)
                except (OSError, Unavailable):
                    entry[0].close()
                    del _compat[pgid]
                    entry = None
            if entry is None:
                owner = ProcessGroupCpu(pgid)
            else:
                owner = entry[0]
            _compat[pgid] = (owner, now)
            return owner.seconds()
    except (OSError, Unavailable):
        return None
