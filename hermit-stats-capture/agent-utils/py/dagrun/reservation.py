"""Stateful CPU-core RESERVATION ledger — collision-free core allocation.

``pick_least_busy_free_cores`` (in :mod:`dagrun.cgroup`) prevents a
single run from OVER-using cores: it samples ``/proc/stat`` and returns the K
least-busy of this process's allowed set. That heuristic cannot prevent
COLLISION — two benchmarks launched at the same instant sample the same idle
cores and both take them, so their tracer/tracee threads land on the same CPUs
and the "pinned" measurement is contaminated by an unrelated run. Sampling has
no memory of who already holds what.

This module adds the missing state: a durable, ``flock``-serialized ledger that
maps each reserved core to its holder. Every acquire runs under an exclusive
lock, so it observes every prior live reservation and picks from the FREE-and-
UNHELD set only. The caller NEVER picks a core itself.

Design invariants:

  * DISJOINT under concurrency — two acquires that overlap in time get disjoint
    core sets. The exclusive ``flock`` is held across the whole
    sweep→pick→record critical section (including the ``/proc/stat`` sample), so
    a concurrent acquire blocks, then sees the first's cores as HELD and
    excludes them. Serializing acquires is the price of disjointness and is
    correct for benchmark setup (a handful of concurrent requests, not a hot
    path).
  * RELEASE on completion — :meth:`Reservation.release`, the
    :func:`reserve_cores` context manager (``finally``), and an ``atexit`` hook
    all free the cores. Normal exit and exceptions both release.
  * DEAD-HOLDER RECLAIM — a crashed holder cannot release. Every acquire first
    sweeps the ledger and drops records whose holder process is no longer live,
    so a leaked reservation is reclaimed by the next acquire instead of
    permanently subtracting cores (the same failure class as leaked cgroup
    scopes that held cores because systemd only reaps an EMPTY scope). Liveness
    is fingerprinted by (pid, /proc starttime) so a recycled PID is NOT mistaken
    for the original holder.
"""

from __future__ import annotations

import atexit
import fcntl
import json
import math
import os
import stat
import tempfile
import time
from collections.abc import Collection, Iterable
from dataclasses import dataclass, field
from pathlib import Path
from types import TracebackType
from typing import IO

from dagrun.cgroup import pick_least_busy_free_cores
from dagrun.procstat import parse_process_stat

_MAX_U32 = (1 << 32) - 1
_MAX_U64 = (1 << 64) - 1


class InsufficientCoresError(RuntimeError):
    """Raised when fewer than the requested K cores are free-and-unheld.

    The allocator refuses to hand out a colliding core: it is better to fail
    loudly than to return a set that overlaps a live reservation."""


class ReservationStateError(RuntimeError):
    """The shared ledger or lock is unsafe, unreadable, or corrupt."""


def _default_ledger_path() -> Path:
    """Ledger location, shared by every process on the host that reserves cores.

    Prefers ``$XDG_RUNTIME_DIR`` (a per-user tmpfs reaped on logout), falling
    back to a uid-scoped ``/tmp`` dir. The whole point is a SINGLE file all
    concurrent runners contend on, so the path must not vary by CWD."""
    env = os.environ.get("DAGRUN_CORE_LEDGER")
    if env:
        return Path(env)
    base = os.environ.get("XDG_RUNTIME_DIR")
    if base and os.path.isdir(base):
        root = Path(base) / "dagrun"
    else:
        root = Path(tempfile.gettempdir()) / f"dagrun-{os.getuid()}"
    return root / "core-reservations.json"


def _proc_starttime(pid: int) -> int | None:
    """The holder's start time (clock ticks since boot, ``/proc/<pid>/stat``
    field 22). Combined with the PID it fingerprints a process across PID reuse:
    a recycled PID has a different starttime, so a dead holder is never mistaken
    for a live one. Returns ``None`` only if the process is positively gone;
    unreadable or malformed evidence raises rather than releasing its cores."""
    try:
        data = Path(f"/proc/{pid}/stat").read_bytes()
    except (FileNotFoundError, ProcessLookupError):
        return None
    except OSError as exc:
        raise ReservationStateError(
            f"cannot inspect process {pid} for core-ledger ownership: {exc}"
        ) from exc
    parsed = parse_process_stat(data)
    if parsed is None or parsed.pid != pid:
        raise ReservationStateError(
            f"cannot parse process {pid} identity for core-ledger ownership"
        )
    return parsed.starttime


def _holder_alive(pid: int, starttime: int | None) -> bool:
    """True iff a process with this exact (pid, starttime) fingerprint is live.

    A bare ``kill(pid, 0)`` success is not enough — the PID may have been
    recycled by an unrelated process. Requiring the recorded starttime to match
    defeats that."""
    cur = _proc_starttime(pid)
    if cur is None:
        return False
    if starttime is None:
        # Legacy record without a fingerprint: fall back to bare liveness.
        return True
    return cur == starttime


@dataclass
class _Record:
    pid: int
    starttime: int | None
    cores: list[int]
    tag: str
    ts: float

    @classmethod
    def from_json(cls, d: dict[str, object]) -> "_Record":
        """Decode one strict shared-schema record or raise ``ValueError``.

        The ledger is an ownership boundary, not user-friendly input: coercing strings/bools or
        dropping malformed cores could release a live holder's CPU. Every persisted field is
        therefore required and domain checked before the caller considers any record.
        """

        def required_int(key: str, *, minimum: int, maximum: int) -> int:
            if key not in d:
                raise ValueError(f"missing {key}")
            value = d[key]
            if isinstance(value, bool) or not isinstance(value, int):
                raise ValueError(f"{key} must be an integer")
            if not minimum <= value <= maximum:
                raise ValueError(f"{key} is outside [{minimum}, {maximum}]")
            return value

        pid = required_int("pid", minimum=1, maximum=_MAX_U32)

        if "starttime" not in d:
            raise ValueError("missing starttime")
        raw_starttime = d["starttime"]
        starttime = (
            None
            if raw_starttime is None
            else required_int("starttime", minimum=1, maximum=_MAX_U64)
        )

        if "cores" not in d or not isinstance(d["cores"], list):
            raise ValueError("cores must be a non-empty list")
        cores_raw = d["cores"]
        if not cores_raw:
            raise ValueError("cores must be a non-empty list")
        cores: list[int] = []
        for index, value in enumerate(cores_raw):
            if isinstance(value, bool) or not isinstance(value, int):
                raise ValueError(f"cores[{index}] must be an integer")
            if not 0 <= value <= _MAX_U32:
                raise ValueError(f"cores[{index}] is outside [0, {_MAX_U32}]")
            cores.append(value)
        if len(set(cores)) != len(cores):
            raise ValueError("cores must not contain duplicates")

        if "tag" not in d or not isinstance(d["tag"], str):
            raise ValueError("tag must be a string")
        tag = d["tag"]

        if "ts" not in d:
            raise ValueError("missing ts")
        raw_ts = d["ts"]
        if isinstance(raw_ts, bool) or not isinstance(raw_ts, (int, float)):
            raise ValueError("ts must be a number")
        ts = float(raw_ts)
        if not math.isfinite(ts) or ts < 0:
            raise ValueError("ts must be finite and non-negative")

        return cls(
            pid=pid,
            starttime=starttime,
            cores=cores,
            tag=tag,
            ts=ts,
        )

    def to_json(self) -> dict[str, object]:
        return {
            "pid": self.pid,
            "starttime": self.starttime,
            "cores": self.cores,
            "tag": self.tag,
            "ts": self.ts,
        }


class _LedgerLock:
    """Exclusive ``flock`` over the ledger's critical section.

    The lock lives on a sibling ``.lock`` file (never the JSON itself, which is
    replaced by atomic rename). Held across sweep→pick→record so the whole
    allocation is serialized and disjoint."""

    def __init__(self, ledger: Path):
        self._lock_path = ledger.with_suffix(ledger.suffix + ".lock")
        self._lock_path.parent.mkdir(parents=True, exist_ok=True)
        self._fh: IO[str] | None = None

    def __enter__(self) -> "_LedgerLock":
        try:
            fd = os.open(
                self._lock_path,
                os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                0o600,
            )
        except OSError as exc:
            raise ReservationStateError(
                f"could not safely open reservation lock {self._lock_path}: {exc}"
            ) from exc
        try:
            metadata = os.fstat(fd)
            if (
                not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != os.geteuid()
                or metadata.st_nlink != 1
            ):
                raise ReservationStateError(
                    f"reservation lock {self._lock_path} is not an owned regular file"
                )
            if metadata.st_mode & 0o077:
                os.fchmod(fd, 0o600)
            self._fh = os.fdopen(fd, "r+")
        except BaseException:
            os.close(fd)
            raise
        fcntl.flock(self._fh.fileno(), fcntl.LOCK_EX)
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        if self._fh is None:
            return
        fcntl.flock(self._fh.fileno(), fcntl.LOCK_UN)
        self._fh.close()
        self._fh = None


def _load(ledger: Path) -> list[_Record]:
    try:
        fd = os.open(ledger, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return []
    except OSError as exc:
        raise ReservationStateError(f"could not safely open reservation ledger {ledger}: {exc}") from exc
    try:
        metadata = os.fstat(fd)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
        ):
            raise ReservationStateError(
                f"reservation ledger {ledger} is not an owned regular file"
            )
        if metadata.st_mode & 0o077:
            os.fchmod(fd, 0o600)
        with os.fdopen(fd, encoding="utf-8") as handle:
            fd = -1
            try:
                raw: object = json.load(handle)
            except (json.JSONDecodeError, UnicodeError) as exc:
                raise ReservationStateError(f"reservation ledger {ledger} is corrupt: {exc}") from exc
    finally:
        if fd >= 0:
            os.close(fd)
    if not isinstance(raw, dict):
        raise ReservationStateError(f"reservation ledger {ledger} root must be an object")
    recs = raw.get("reservations")
    if not isinstance(recs, list):
        raise ReservationStateError(f"reservation ledger {ledger} has no reservations list")
    out: list[_Record] = []
    try:
        for r in recs:
            if not isinstance(r, dict):
                raise ValueError("record is not an object")
            out.append(_Record.from_json(r))
    except (TypeError, ValueError, OverflowError) as exc:
        raise ReservationStateError(f"reservation ledger {ledger} has an invalid record: {exc}") from exc
    return out


def _store(ledger: Path, records: Iterable[_Record]) -> None:
    """Atomically replace the ledger (write temp in the same dir, then rename)."""
    ledger.parent.mkdir(parents=True, exist_ok=True)
    payload = {"reservations": [r.to_json() for r in records]}
    fd, tmp = tempfile.mkstemp(dir=str(ledger.parent), suffix=".tmp")
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump(payload, fh)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, ledger)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def _sweep(records: list[_Record]) -> tuple[list[_Record], list[_Record]]:
    """Split records into (live, reclaimed-dead) by holder liveness."""
    live: list[_Record] = []
    dead: list[_Record] = []
    for r in records:
        (live if _holder_alive(r.pid, r.starttime) else dead).append(r)
    return live, dead


@dataclass
class Reservation:
    """A held set of cores. Release exactly once (idempotent)."""

    cores: list[int]
    pid: int
    starttime: int | None
    tag: str
    ledger: Path
    _released: bool = field(default=False, repr=False)

    def release(self) -> None:
        """Free this reservation's cores. Idempotent and crash-safe: even if
        this is never called, the next acquire reclaims the record."""
        if self._released:
            return
        with _LedgerLock(self.ledger):
            records = _load(self.ledger)
            kept = [
                r
                for r in records
                if not (
                    r.pid == self.pid
                    and r.starttime == self.starttime
                    and r.tag == self.tag
                    and r.cores == self.cores
                )
            ]
            _store(self.ledger, kept)
        self._released = True

    def __enter__(self) -> "Reservation":
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.release()


def acquire(
    k: int,
    *,
    tag: str = "",
    sample_s: float = 0.3,
    ledger: Path | None = None,
    exclude: Collection[int] = (),
    max_irq_rate: float | None = None,
) -> Reservation:
    """Reserve K disjoint cores for this process, collision-free.

    Under the exclusive ledger lock: sweep dead holders, compute the HELD set
    from live reservations, pick K least-busy cores from the free-and-unheld
    set, record them against this (pid, starttime), and register an ``atexit``
    release. Raises :class:`InsufficientCoresError` if fewer than K cores are
    available — never returns an overlapping set.

    ``exclude`` is unioned into the held set (cores the caller wants avoided for
    reasons outside the ledger, e.g. a reserved housekeeping CPU)."""
    if k < 1:
        raise ValueError(f"k must be >= 1, got {k}")
    if not math.isfinite(sample_s) or sample_s < 0:
        raise ValueError(f"sample_s must be finite and >= 0, got {sample_s}")
    if max_irq_rate is not None:
        if not math.isfinite(max_irq_rate) or max_irq_rate < 0:
            raise ValueError(f"max_irq_rate must be finite and >= 0, got {max_irq_rate}")
        if sample_s <= 0:
            raise ValueError("sample_s must be > 0 when max_irq_rate is set")
    path = ledger or _default_ledger_path()
    pid = os.getpid()
    starttime = _proc_starttime(pid)

    with _LedgerLock(path):
        records = _load(path)
        live, _dead = _sweep(records)
        held: set[int] = set(int(c) for c in exclude)
        for r in live:
            held.update(r.cores)
        cores = pick_least_busy_free_cores(
            k, sample_s=sample_s, exclude=held, max_irq_rate=max_irq_rate
        )
        if len(cores) < k:
            # Persist the sweep even on failure, so leaked cores are reclaimed.
            _store(path, live)
            raise InsufficientCoresError(
                f"requested {k} core(s) but only {len(cores)} free-and-unheld "
                f"(held by {len(live)} live reservation(s): "
                f"{sorted(held)}; allowed set may also be smaller)"
            )
        rec = _Record(pid=pid, starttime=starttime, cores=cores, tag=tag, ts=time.time())
        _store(path, live + [rec])

    reservation = Reservation(
        cores=cores, pid=pid, starttime=starttime, tag=tag, ledger=path
    )
    atexit.register(reservation.release)
    return reservation


def reclaim_dead(ledger: Path | None = None) -> list[dict[str, object]]:
    """Sweep the ledger and drop records whose holder is dead. Returns the
    reclaimed records (as JSON dicts) for logging/tests. Safe to call anytime;
    ``acquire`` also sweeps, so this is mostly for maintenance/inspection."""
    path = ledger or _default_ledger_path()
    with _LedgerLock(path):
        records = _load(path)
        live, dead = _sweep(records)
        if dead:
            _store(path, live)
    return [r.to_json() for r in dead]


def held_cores(ledger: Path | None = None) -> list[int]:
    """The currently-held (live) cores across all reservations. Sweeps dead
    holders first so the answer reflects reality, not leaked records."""
    path = ledger or _default_ledger_path()
    with _LedgerLock(path):
        records = _load(path)
        live, dead = _sweep(records)
        if dead:
            _store(path, live)
        out: set[int] = set()
        for r in live:
            out.update(r.cores)
    return sorted(out)


class reserve_cores:
    """Context manager: acquire on enter, release on exit (normal or exception).

        with reserve_cores(1, tag="ptrace-bench") as cores:
            # HARD-pin the whole tree via a transient scope's cgroup cpuset.
            # (sched_setaffinity is escapable because a child can widen its own
            #  mask. Use AllowedCPUs, not process affinity.)
            subprocess.run(["systemd-run", "--user", "--scope", "--collect",
                            f"-pAllowedCPUs={','.join(map(str, cores))}",
                            "--", *benchmark_cmd])

    The `cpuset-alloc run --cores K -- CMD` CLI
    (:mod:`dagrun.cpuset_allocator`) wraps exactly this."""

    def __init__(
        self,
        k: int,
        *,
        tag: str = "",
        sample_s: float = 0.3,
        ledger: Path | None = None,
        exclude: Collection[int] = (),
        max_irq_rate: float | None = None,
    ):
        self._k = k
        self._tag = tag
        self._sample_s = sample_s
        self._ledger = ledger
        self._exclude = exclude
        self._max_irq_rate = max_irq_rate
        self._reservation: Reservation | None = None

    def __enter__(self) -> list[int]:
        self._reservation = acquire(
            self._k,
            tag=self._tag,
            sample_s=self._sample_s,
            ledger=self._ledger,
            exclude=self._exclude,
            max_irq_rate=self._max_irq_rate,
        )
        return self._reservation.cores

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        if self._reservation is not None:
            self._reservation.release()
