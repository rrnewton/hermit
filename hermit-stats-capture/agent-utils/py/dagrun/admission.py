"""Host-wide MEMORY admission: grant, queue, or refuse a run before it contends.

WHAT THIS ADDS THAT ``--max-mem`` DOES NOT. ``sizing.box_mem_budget_bytes`` and the ``--max-mem``
refusal in the CLI gate memory at box bring-up: one process, one question, one yes-or-no, answered
against a snapshot of the host. That has no notion of what OTHER runner invocations on the same
host have already committed to, so two boxes started a second apart each see the same headroom and
both take it. Neither is wrong on its own; together they overcommit the machine and the symptom is
swapping or an OOM kill in whichever run happens to touch its pages last.

This module is the missing shared state, and it is deliberately the SIBLING of the core ledger in
:mod:`dagrun.reservation`: a durable, ``flock``-serialized file that every runner on
the host contends on, with dead-holder reclaim fingerprinted by ``(pid, /proc starttime)`` so a
crashed run cannot subtract memory forever and a recycled PID is never mistaken for the original
holder.

THREE ANSWERS, NOT TWO. A yes-or-no can only ever say "no", which tells the caller nothing about
what to do next:

* :attr:`Verdict.GRANT` -- the reservation is recorded and held.
* :attr:`Verdict.QUEUE` -- it would fit on a quiet host, so WAITING CAN HELP. The decision reports
  how many live holders must finish first, and which resource is in the way.
* :attr:`Verdict.REFUSE` -- the request alone exceeds the whole-host budget, so waiting can NEVER
  help however long anyone waits. The decision says the largest number that could be granted, so
  the operator has something to type instead of a closed door.

Collapsing QUEUE into REFUSE turns a transient into a permanent failure; collapsing REFUSE into
QUEUE turns a configuration error into a hang. Both are worse than saying which one it is.

TWO CONDITIONS, EACH NAMED SEPARATELY. A grant requires BOTH:

1. ``reserved + requested <= whole-host budget`` -- the aggregate this tool will ever let itself
   hold, derived from ``MemTotal``. This is the condition other RUNNERS affect.
2. ``requested <= live headroom`` -- ``MemAvailable`` minus the safety margin, re-read on every
   call. This is the condition NON-RUNNER tenants affect: a ledger alone cannot see a database
   that grew, and a budget that ignores the live reading gates nothing on a shared machine.

They are kept apart because the remedies differ: the first waits for a peer run, the second waits
for (or requires action on) something this tool does not manage.
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
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from types import TracebackType
from typing import IO

from dagrun.procstat import parse_process_stat

__all__ = [
    "AdmissionStateError",
    "Decision",
    "MemoryReservation",
    "Verdict",
    "admit",
    "held_by_this_process",
    "held_bytes",
    "host_budget_bytes",
    "live_headroom_bytes",
    "reclaim_dead",
    "request",
    "request_with_limits",
]

#: Environment override for the shared memory ledger path (sibling of ``DAGRUN_CORE_LEDGER``).
MEM_LEDGER_ENV = "DAGRUN_MEM_LEDGER"

#: Environment override for the whole-host aggregate budget, in BYTES.
#:
#: An operator knob, not a test hook: on a shared machine the fraction-of-MemTotal default is a
#: guess about how much of the box this tool may claim, and the person who owns the machine knows
#: better. An unparseable value is reported and ignored rather than silently taken as zero, which
#: would refuse every run.
MEM_BUDGET_BYTES_ENV = "DAGRUN_ADMISSION_BUDGET_BYTES"

#: Environment override for the live-headroom reading, in BYTES. Same posture as above; exists so
#: a host without a readable ``/proc/meminfo`` can still gate on a number the operator supplies.
MEM_HEADROOM_BYTES_ENV = "DAGRUN_ADMISSION_HEADROOM_BYTES"

#: Fraction of ``MemTotal`` this tool will let its runs hold IN AGGREGATE.
#:
#: Not 1.0, and the gap is not timidity: the kernel, the page cache, and whatever else the machine
#: exists to do all need memory, and a runner that plans to the last byte is planning for the OOM
#: killer to arbitrate.
DEFAULT_MEM_BUDGET_FRACTION = 0.85

#: Absolute headroom kept back on top of the fraction.
#:
#: A fraction alone scales the wrong way: 15% of a 512 GiB host is 76 GiB of slack nobody needs,
#: while 15% of an 8 GiB host is 1.2 GiB, which one page-cache spike erases. A flat margin puts a
#: floor under the slack on small hosts, where an overcommit actually kills things.
MEM_SAFETY_MARGIN_BYTES = 8 * 1024**3

#: The flat margin is never allowed to exceed this share of the measure it is taken from.
#:
#: An UNCAPPED flat margin is a gate that never opens. Held back whole, 8 GiB of an 8 GiB machine
#: leaves an aggregate budget of ZERO: every request is REFUSED, and the refusal helpfully advises
#: asking for "at most 0 B" -- while the live headroom reading is pinned at zero too, so nothing
#: can queue its way in either. That is not a conservative gate, it is a broken one, and it breaks
#: precisely on the small hosts the flat margin was added to protect.
MEM_SAFETY_MARGIN_MAX_DIVISOR = 8

#: Longest wait :func:`admit` will honour, in seconds, however large ``wait_s`` is.
#:
#: A year is not a wait, it is a hang with a timestamp -- but the reason this is a CLAMP rather
#: than a rejection is arithmetic parity with the paired engine: there, the deadline is
#: ``Instant + Duration`` and an unbounded seconds value PANICS the process (overflow when adding
#: a duration to an instant) on a number this very function has just accepted. Both editions
#: therefore fold an absurd wait down to the same finite ceiling instead of one waiting forever
#: while the other aborts.
MAX_WAIT_SECONDS = 365 * 24 * 60 * 60

_MAX_U32 = (1 << 32) - 1
_MAX_U64 = (1 << 64) - 1
_MAX_BYTES = 1 << 62


class AdmissionStateError(RuntimeError):
    """The shared memory ledger or its lock is unsafe, unreadable, or corrupt."""


class Verdict(Enum):
    """The three answers admission can give. The values are part of the printed contract, so
    every paired implementation of this runner spells them identically."""

    GRANT = "grant"
    QUEUE = "queue"
    REFUSE = "refuse"


def _meminfo_bytes(key: str) -> int | None:
    """One ``/proc/meminfo`` field in bytes, or ``None`` when it cannot be read.

    ABSENT IS NOT ZERO: an unreadable ``/proc/meminfo`` means the host's memory is UNKNOWN, not
    that it has none. Returning 0 would refuse every run on a host this code simply could not
    measure, which is a worse failure than not gating.
    """
    try:
        with open("/proc/meminfo", encoding="utf-8") as handle:
            for line in handle:
                name, _, rest = line.partition(":")
                if name.strip() != key:
                    continue
                parts = rest.split()
                if not parts:
                    return None
                try:
                    value = int(parts[0])
                except ValueError:
                    return None
                unit = parts[1].lower() if len(parts) > 1 else "kb"
                return value * 1024 if unit == "kb" else value
    except OSError:
        return None
    return None


def _env_bytes(name: str) -> int | None:
    raw = os.environ.get(name)
    if raw is None or not raw.strip():
        return None
    # ONE message for both rejections, and it is worded the same in the Rust engine. A negative
    # count of bytes and a non-numeric string are the same mistake from the operator's side --
    # "this is not a number of bytes" -- and the two editions read and write ONE ledger, so a
    # warning that differs between them is a difference a reader would have to explain.
    try:
        value = int(raw.strip())
    except ValueError:
        value = -1
    if value < 0:
        print(
            f"[dagrun] WARNING: {name}='{raw}' is not a non-negative integer number "
            "of bytes; ignoring it and measuring the host instead."
        )
        return None
    return value


def _safety_margin_bytes(scale: int) -> int:
    """The flat margin, capped so it can never consume the whole of ``scale``.

    From 64 GiB upward the cap is not binding and the margin is the flat 8 GiB. Below that it
    scales down with the host, so the budget stays a positive share of the machine at every size
    instead of collapsing to zero and refusing everything.
    """
    return min(MEM_SAFETY_MARGIN_BYTES, max(0, scale) // MEM_SAFETY_MARGIN_MAX_DIVISOR)


def _budget_from_total(total: int) -> int:
    """The aggregate budget implied by a host of ``total`` bytes.

    Split out from :func:`host_budget_bytes` so the ARITHMETIC can be pinned against literal
    numbers without a readable ``/proc/meminfo``. Both editions carry this function and both
    pin the same literals: the fraction is a production constant two engines share through one
    ledger, and a constant no test names is a constant either engine can drift on quietly.
    """
    return max(0, int(total * DEFAULT_MEM_BUDGET_FRACTION) - _safety_margin_bytes(total))


def _headroom_from(available: int, scale: int) -> int:
    """The live headroom implied by ``available`` bytes free on a host of ``scale`` bytes."""
    return max(0, available - _safety_margin_bytes(scale))


def host_budget_bytes() -> int | None:
    """The aggregate this tool will ever let its runs hold on this host, or ``None`` if unknown.

    Re-read on EVERY call rather than cached, for the same reason the headroom is: the operator
    override can change between runs, and a cached budget is a budget that stops matching the
    machine it is supposed to describe.
    """
    override = _env_bytes(MEM_BUDGET_BYTES_ENV)
    if override is not None:
        return override
    total = _meminfo_bytes("MemTotal")
    if total is None:
        return None
    return _budget_from_total(total)


def live_headroom_bytes() -> int | None:
    """Memory actually available on the host right now, minus the margin; ``None`` if unknown.

    This is the term that sees tenants this tool does not manage. A ledger can only account for
    runs that went through it, so without this reading a host loaded to 99% by something else
    would still look empty and admission would grant into a machine that is already swapping.
    """
    override = _env_bytes(MEM_HEADROOM_BYTES_ENV)
    if override is not None:
        return override
    available = _meminfo_bytes("MemAvailable")
    if available is None:
        return None
    # The margin is a property of the HOST, so it is scaled by MemTotal, not by whatever happens
    # to be free at this instant -- otherwise a momentarily busy host would shrink its own margin
    # exactly when the margin matters. MemTotal is readable whenever MemAvailable is; the fallback
    # only keeps this honest if that ever stops being true.
    total = _meminfo_bytes("MemTotal")
    return _headroom_from(available, total if total is not None else available)


def _default_ledger_path() -> Path:
    """Ledger location, shared by every process on the host that admits a run.

    Same placement rule as the core ledger: prefer ``$XDG_RUNTIME_DIR`` (a per-user tmpfs reaped
    on logout), else a uid-scoped temp dir. The whole point is a SINGLE file every concurrent
    runner contends on, so the path must not vary by working directory.
    """
    env = os.environ.get(MEM_LEDGER_ENV)
    if env:
        return Path(env)
    base = os.environ.get("XDG_RUNTIME_DIR")
    if base and os.path.isdir(base):
        root = Path(base) / "dagrun"
    else:
        root = Path(tempfile.gettempdir()) / f"dagrun-{os.getuid()}"
    return root / "memory-admissions.json"


def _proc_starttime(pid: int) -> int | None:
    """The holder's start time (clock ticks since boot, ``/proc/<pid>/stat`` field 22).

    Combined with the PID it fingerprints a process across PID reuse, so a dead holder's memory
    is reclaimed and a live unrelated process that inherited the number is never released.
    ``None`` means positive disappearance; unreadable or malformed evidence raises so it cannot
    authorize reclaim.
    """
    try:
        data = Path(f"/proc/{pid}/stat").read_bytes()
    except (FileNotFoundError, ProcessLookupError):
        return None
    except OSError as exc:
        raise AdmissionStateError(
            f"cannot inspect process {pid} for memory-ledger ownership: {exc}"
        ) from exc
    parsed = parse_process_stat(data)
    if parsed is None or parsed.pid != pid:
        raise AdmissionStateError(
            f"cannot parse process {pid} identity for memory-ledger ownership"
        )
    return parsed.starttime


def _holder_alive(pid: int, starttime: int | None) -> bool:
    cur = _proc_starttime(pid)
    if cur is None:
        return False
    if starttime is None:
        return True
    return cur == starttime


@dataclass
class _Record:
    pid: int
    starttime: int | None
    bytes_: int
    tag: str
    ts: float

    @classmethod
    def from_json(cls, d: dict[str, object]) -> "_Record":
        """Decode one strict record or raise ``ValueError``.

        The ledger is an ownership boundary, not user-friendly input: coercing a string or
        dropping a malformed size could release a live holder's memory reservation and let the
        host be double-booked. Every field is required and domain checked.
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
        bytes_ = required_int("bytes", minimum=1, maximum=_MAX_BYTES)
        if "tag" not in d or not isinstance(d["tag"], str):
            raise ValueError("tag must be a string")
        if "ts" not in d:
            raise ValueError("missing ts")
        raw_ts = d["ts"]
        if isinstance(raw_ts, bool) or not isinstance(raw_ts, (int, float)):
            raise ValueError("ts must be a number")
        ts = float(raw_ts)
        if not math.isfinite(ts) or ts < 0:
            raise ValueError("ts must be finite and non-negative")
        return cls(pid=pid, starttime=starttime, bytes_=bytes_, tag=d["tag"], ts=ts)

    def to_json(self) -> dict[str, object]:
        return {
            "pid": self.pid,
            "starttime": self.starttime,
            "bytes": self.bytes_,
            "tag": self.tag,
            "ts": self.ts,
        }


class _LedgerLock:
    """Exclusive ``flock`` over the ledger's critical section.

    THE LOCK IS WHAT MAKES TWO SIMULTANEOUS ADMITS SAFE. It is held across the whole
    sweep -> measure -> decide -> record section, so a concurrent request blocks, and when it
    proceeds it sees the first request's reservation already recorded. Without that, both
    requests read the same "reserved" total and both grant -- the exact defect this module
    exists to remove, reproduced inside the fix.

    The lock lives on a sibling ``.lock`` file, never the JSON itself, which is replaced by
    atomic rename.
    """

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
            raise AdmissionStateError(
                f"could not safely open admission lock {self._lock_path}: {exc}"
            ) from exc
        try:
            metadata = os.fstat(fd)
            if (
                not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != os.geteuid()
                or metadata.st_nlink != 1
            ):
                raise AdmissionStateError(
                    f"admission lock {self._lock_path} is not an owned regular file"
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
        raise AdmissionStateError(
            f"could not safely open admission ledger {ledger}: {exc}"
        ) from exc
    try:
        metadata = os.fstat(fd)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
        ):
            raise AdmissionStateError(
                f"admission ledger {ledger} is not an owned regular file"
            )
        if metadata.st_mode & 0o077:
            os.fchmod(fd, 0o600)
        with os.fdopen(fd, encoding="utf-8") as handle:
            fd = -1
            try:
                raw: object = json.load(handle)
            except (json.JSONDecodeError, UnicodeError) as exc:
                raise AdmissionStateError(
                    f"admission ledger {ledger} is corrupt: {exc}"
                ) from exc
    finally:
        if fd >= 0:
            os.close(fd)
    if not isinstance(raw, dict):
        raise AdmissionStateError(f"admission ledger {ledger} root must be an object")
    recs = raw.get("admissions")
    if not isinstance(recs, list):
        raise AdmissionStateError(f"admission ledger {ledger} has no admissions list")
    out: list[_Record] = []
    try:
        for r in recs:
            if not isinstance(r, dict):
                raise ValueError("record is not an object")
            out.append(_Record.from_json(r))
    except (TypeError, ValueError, OverflowError) as exc:
        raise AdmissionStateError(
            f"admission ledger {ledger} has an invalid record: {exc}"
        ) from exc
    return out


def _store(ledger: Path, records: list[_Record]) -> None:
    """Atomically replace the ledger (write a temp file in the same dir, then rename)."""
    ledger.parent.mkdir(parents=True, exist_ok=True)
    payload = {"admissions": [r.to_json() for r in records]}
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
    live: list[_Record] = []
    dead: list[_Record] = []
    for r in records:
        (live if _holder_alive(r.pid, r.starttime) else dead).append(r)
    return live, dead


def _holders_that_must_release(live: list[_Record], need: int) -> int:
    """How many live holders must finish before ``need`` more bytes fit. Largest first.

    A real number, not a ticket counter: it answers "how much has to happen before my turn",
    which is what someone staring at a waiting run wants to know. Largest-first is the optimistic
    reading and is honest about being one -- it is the FEWEST that could suffice.
    """
    if need <= 0:
        return 0
    freed = 0
    count = 0
    for record in sorted(live, key=lambda r: r.bytes_, reverse=True):
        freed += record.bytes_
        count += 1
        if freed >= need:
            return count
    return count


@dataclass(frozen=True)
class Decision:
    """One admission answer, carrying every number it was made from.

    The numbers are not decoration. A run that waits, or refuses, has to be explainable months
    later from one line of log, and "admission denied" is not explainable.
    """

    verdict: Verdict
    reason: str
    requested_bytes: int
    #: Aggregate host budget, or ``None`` when the host could not be measured.
    budget_bytes: int | None
    #: Live headroom, or ``None`` when the host could not be measured.
    headroom_bytes: int | None
    #: Sum of live reservations in the ledger, this request excluded.
    reserved_bytes: int
    #: For QUEUE: the fewest live holders that must finish first. Zero otherwise.
    holders_ahead: int
    #: For REFUSE: the largest request that could ever be granted on this host.
    largest_grantable_bytes: int | None


def _fmt(n: int | None) -> str:
    if n is None:
        return "unknown"
    value = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024.0 or unit == "TiB":
            return f"{value:.1f} {unit}" if unit != "B" else f"{int(value)} B"
        value /= 1024.0
    return f"{int(n)} B"


@dataclass
class MemoryReservation:
    """A granted memory reservation. Release exactly once; releasing twice is a no-op."""

    bytes_: int
    pid: int
    starttime: int | None
    tag: str
    ledger: Path
    _released: bool = field(default=False, repr=False)

    def release(self) -> None:
        """Return this run's share of the host budget. Idempotent and crash-safe: even if this is
        never called, the next request's sweep reclaims the record."""
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
                    and r.bytes_ == self.bytes_
                )
            ]
            _store(self.ledger, kept)
        self._released = True

    def __enter__(self) -> "MemoryReservation":
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.release()


def request(
    requested_bytes: int, *, tag: str = "", ledger: Path | None = None
) -> tuple[Decision, MemoryReservation | None]:
    """Ask for ``requested_bytes`` of the host budget ONCE, without waiting.

    Returns the decision and, on :attr:`Verdict.GRANT`, the held reservation. The entire
    sweep -> measure -> decide -> record sequence runs under the exclusive ledger lock, so two
    overlapping requests cannot both see the same free budget and both grant.
    """
    return request_with_limits(
        requested_bytes,
        tag=tag,
        ledger=ledger,
        budget=host_budget_bytes(),
        headroom=live_headroom_bytes(),
    )


def request_with_limits(
    requested_bytes: int,
    *,
    tag: str = "",
    ledger: Path | None = None,
    budget: int | None,
    headroom: int | None,
) -> tuple[Decision, MemoryReservation | None]:
    """:func:`request` with the two host limits supplied rather than measured.

    The split exists so the DECISION can be exercised against exact numbers. Reading the limits
    from the environment inside the critical section makes the rule untestable without mutating
    process-global state, which under a parallel test runner is a race rather than a fixture --
    and a racy test of an admission rule is worse than none, because it fails for the wrong reason.
    """
    if requested_bytes < 1:
        raise ValueError(f"requested_bytes must be >= 1, got {requested_bytes}")
    path = ledger or _default_ledger_path()
    pid = os.getpid()
    starttime = _proc_starttime(pid)

    with _LedgerLock(path):
        records = _load(path)
        live, dead = _sweep(records)
        if dead:
            _store(path, live)
        reserved = sum(r.bytes_ for r in live)

        if budget is not None and requested_bytes > budget:
            # REFUSE, not QUEUE: no amount of waiting makes the host bigger.
            return (
                Decision(
                    verdict=Verdict.REFUSE,
                    reason=(
                        f"REFUSED: {_fmt(requested_bytes)} exceeds the whole-host budget of "
                        f"{_fmt(budget)}, so waiting can never help. Ask for at most "
                        f"{_fmt(budget)} ({budget} bytes), or raise the budget with "
                        f"{MEM_BUDGET_BYTES_ENV}."
                    ),
                    requested_bytes=requested_bytes,
                    budget_bytes=budget,
                    headroom_bytes=headroom,
                    reserved_bytes=reserved,
                    holders_ahead=0,
                    largest_grantable_bytes=budget,
                ),
                None,
            )

        if budget is not None and reserved + requested_bytes > budget:
            need = reserved + requested_bytes - budget
            return (
                Decision(
                    verdict=Verdict.QUEUE,
                    reason=(
                        f"QUEUED on OTHER RUNS: {_fmt(requested_bytes)} would fit on a quiet "
                        f"host, but {len(live)} live reservation(s) already hold "
                        f"{_fmt(reserved)} of the {_fmt(budget)} budget. Waiting on "
                        f"{_holders_that_must_release(live, need)} holder(s) to finish."
                    ),
                    requested_bytes=requested_bytes,
                    budget_bytes=budget,
                    headroom_bytes=headroom,
                    reserved_bytes=reserved,
                    holders_ahead=_holders_that_must_release(live, need),
                    largest_grantable_bytes=budget,
                ),
                None,
            )

        if headroom is not None and requested_bytes > headroom:
            return (
                Decision(
                    verdict=Verdict.QUEUE,
                    reason=(
                        f"QUEUED on HOST MEMORY held outside this tool: {_fmt(requested_bytes)} "
                        f"is within the {_fmt(budget)} budget, but only {_fmt(headroom)} is "
                        "actually available right now. The ledger cannot see a non-runner "
                        "tenant; this reading can."
                    ),
                    requested_bytes=requested_bytes,
                    budget_bytes=budget,
                    headroom_bytes=headroom,
                    reserved_bytes=reserved,
                    # Nothing in the ledger is in the way, so no holder finishing will help.
                    holders_ahead=0,
                    largest_grantable_bytes=budget,
                ),
                None,
            )

        record = _Record(
            pid=pid,
            starttime=starttime,
            bytes_=requested_bytes,
            tag=tag,
            ts=time.time(),
        )
        _store(path, live + [record])

    reservation = MemoryReservation(
        bytes_=requested_bytes, pid=pid, starttime=starttime, tag=tag, ledger=path
    )
    atexit.register(reservation.release)
    return (
        Decision(
            verdict=Verdict.GRANT,
            reason=(
                f"GRANTED {_fmt(requested_bytes)} (host budget {_fmt(budget)}, "
                f"{_fmt(reserved)} already reserved by {len(live)} other run(s))"
            ),
            requested_bytes=requested_bytes,
            budget_bytes=budget,
            headroom_bytes=headroom,
            reserved_bytes=reserved,
            holders_ahead=0,
            largest_grantable_bytes=budget,
        ),
        reservation,
    )


def _wait_budget_s(wait_s: float) -> float:
    """The wait :func:`admit` will honour: negative and NaN fold to none, absurd to the cap.

    A separate function because the paired engine's version of this line is the one that used to
    ABORT the process -- there the deadline is ``Instant + Duration`` and an unbounded seconds
    value panics -- so both editions pin the folded value rather than trusting the caller.
    """
    return min(max(0.0, wait_s), float(MAX_WAIT_SECONDS))


def admit(
    requested_bytes: int,
    *,
    tag: str = "",
    ledger: Path | None = None,
    poll_s: float = 2.0,
    wait_s: float = 0.0,
    announce: bool = True,
) -> tuple[Decision, MemoryReservation | None]:
    """Request admission, WAITING while the answer is QUEUE, up to ``wait_s`` seconds.

    A WAITING RUN SAYS SO, and says it again whenever the answer changes. A queued run that
    printed nothing is indistinguishable from a wedged one -- which is the same defect class as
    the silent scheduler sleep, and no more acceptable here.

    ``wait_s = 0`` waits not at all and returns the first decision, so the caller can choose
    between queuing and reporting. REFUSE never waits, by construction.
    """
    deadline = time.monotonic() + _wait_budget_s(wait_s)
    last_reason: str | None = None
    while True:
        decision, reservation = request(requested_bytes, tag=tag, ledger=ledger)
        if decision.verdict is not Verdict.QUEUE:
            if announce and decision.reason != last_reason:
                print(f"[admission] {decision.reason}")
            return decision, reservation
        if announce and decision.reason != last_reason:
            print(f"[admission] {decision.reason}")
            last_reason = decision.reason
        if time.monotonic() >= deadline:
            return decision, None
        time.sleep(min(max(0.01, poll_s), max(0.01, deadline - time.monotonic())))


def reclaim_dead(ledger: Path | None = None) -> list[dict[str, object]]:
    """Sweep the ledger and drop records whose holder is dead; return what was reclaimed."""
    path = ledger or _default_ledger_path()
    with _LedgerLock(path):
        records = _load(path)
        live, dead = _sweep(records)
        if dead:
            _store(path, live)
    return [r.to_json() for r in dead]


def held_by_this_process(ledger: Path | None = None) -> int:
    """Bytes THIS process already holds in the ledger, by ``(pid, /proc starttime)``.

    The identity, not a flag. ``execvp`` keeps both fields, so a runner that has re-exec'd into
    its systemd scope can ask the ledger whether the reservation it is about to skip re-asking
    for is genuinely its OWN. An environment variable cannot answer that question: the scope
    exports the in-scope sentinel to every process inside it, so a runner invoked as a STEP of a
    boxed run reads the same "1" while holding nothing at all.

    Zero is the honest answer for an untracked process, and it means "go and ask properly".
    """
    path = ledger or _default_ledger_path()
    pid = os.getpid()
    starttime = _proc_starttime(pid)
    with _LedgerLock(path):
        records = _load(path)
    return sum(r.bytes_ for r in records if r.pid == pid and r.starttime == starttime)


def held_bytes(ledger: Path | None = None) -> int:
    """Total bytes currently reserved by LIVE holders. Sweeps dead holders first, so the answer
    reflects the host rather than the leftovers of runs that crashed."""
    path = ledger or _default_ledger_path()
    with _LedgerLock(path):
        records = _load(path)
        live, dead = _sweep(records)
        if dead:
            _store(path, live)
        return sum(r.bytes_ for r in live)
