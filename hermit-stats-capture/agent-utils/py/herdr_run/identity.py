"""Bind a process id to the boot and start tick that make it *that* process.

A bare PID is not an identity. Linux recycles process numbers, so ``kill(pid, 0)`` answers "is
SOMETHING alive with this number", which is the wrong question in both directions: a recycled
number makes an unrelated stranger look like proof of liveness, and a same-numbered stranger makes
a dead process look alive. :mod:`herdr_run.reap` therefore refuses to act on a PID alone and
requires the triple ``(pid, boot_id, start_ticks)``.

``start_ticks`` is field 22 of ``/proc/<pid>/stat`` — the process's start time in clock ticks since
boot. It is assigned by the kernel at fork and never changes, so two processes that ever held the
same number differ in it. ``boot_id`` scopes that tick count: after a reboot the counter restarts,
so ticks alone would let a pre-reboot record match a post-reboot process.

**Parsing ``/proc/<pid>/stat`` needs care.** Field 2 is the executable name in parentheses and may
itself contain spaces AND parentheses — ``(my prog (v2))`` is a legal comm. Splitting the line on
whitespace therefore mis-numbers every later field. The only correct split is at the LAST ``)``,
after which the remaining whitespace-separated tokens are fields 3 onwards.

Every function here answers "unknown" rather than guessing. An unreadable ``/proc`` must reach the
policy as UNKNOWN, never as "the process is gone", because the second reading authorises closing a
tab and the first does not.
"""

from __future__ import annotations

import os
from dataclasses import dataclass

from herdr_run.textfmt import os_error_text, strip_space

__all__ = [
    "BOOT_ID_PATH",
    "ShellProbe",
    "current_boot_id",
    "parse_start_ticks",
    "probe_process",
    "process_start_ticks",
]

#: Where the kernel exposes a per-boot random identifier. Stable for the life of the boot.
BOOT_ID_PATH = "sys/kernel/random/boot_id"

#: Index of ``starttime`` among the whitespace-separated fields that FOLLOW the comm field. ``stat``
#: field 22 is the 20th token after the closing parenthesis, because tokens there begin at field 3.
_START_TICKS_OFFSET = 22 - 3

#: Longest ``/proc/<pid>/stat`` line accepted. Real lines are a few hundred bytes; a cap keeps a
#: hostile or corrupted procfs from turning identity binding into an unbounded read.
_MAX_STAT_BYTES = 8192


@dataclass(frozen=True)
class ShellProbe:
    """What one live-process lookup established, as a tri-state rather than a bool.

    The three cases must stay distinguishable all the way to the reaping policy:

    * ``gone`` — the process does not exist. Only this may contribute to a STALE verdict.
    * an identity with ``start_ticks`` — the process exists and is bound.
    * ``error`` — we could not tell. UNKNOWN, never a licence to reap.
    """

    #: True only when ``/proc/<pid>`` is positively absent.
    gone: bool
    #: Start tick of the live process, or ``None`` when it could not be read.
    start_ticks: int | None
    #: Why the lookup was inconclusive, or ``None`` when it was conclusive.
    error: str | None


def parse_start_ticks(stat_text: str) -> int | None:
    """Extract field 22 (``starttime``) from one ``/proc/<pid>/stat`` line.

    Returns ``None`` for anything that does not parse exactly, including a comm field with no
    closing parenthesis and a truncated line. Guessing here would fabricate an identity.
    """
    closing = stat_text.rfind(")")
    if closing < 0:
        return None
    fields = stat_text[closing + 1 :].split()
    if len(fields) <= _START_TICKS_OFFSET:
        return None
    token = fields[_START_TICKS_OFFSET]
    try:
        ticks = int(token)
    except ValueError:
        return None
    return ticks if ticks >= 0 else None


def current_boot_id(*, proc_root: str = "/proc") -> str | None:
    """Read this boot's identifier, or ``None`` when it cannot be established."""
    try:
        with open(os.path.join(proc_root, BOOT_ID_PATH), "rb") as handle:
            value = strip_space(handle.read().decode("utf-8"))
    except (OSError, UnicodeError):
        return None
    return value or None


def process_start_ticks(pid: int, *, proc_root: str = "/proc") -> int | None:
    """Start tick of ``pid``, or ``None`` when it is absent or unreadable."""
    return probe_process(pid, proc_root=proc_root).start_ticks


def probe_process(pid: int, *, proc_root: str = "/proc") -> ShellProbe:
    """Look one live process up, keeping "absent" and "could not tell" apart."""
    if isinstance(pid, bool) or not isinstance(pid, int) or pid < 1:
        return ShellProbe(gone=False, start_ticks=None, error=f"not a process id: {pid!r}")
    path = os.path.join(proc_root, str(pid), "stat")
    try:
        with open(path, "rb") as handle:
            payload = handle.read(_MAX_STAT_BYTES + 1)
    except FileNotFoundError:
        # The ONE reading that may authorise reaping: the kernel says there is no such process.
        return ShellProbe(gone=True, start_ticks=None, error=None)
    except (OSError, ValueError) as exc:
        return ShellProbe(
            gone=False, start_ticks=None, error=f"cannot read {path}: {os_error_text(exc)}"
        )
    if len(payload) > _MAX_STAT_BYTES:
        return ShellProbe(gone=False, start_ticks=None, error=f"{path} is implausibly long")
    ticks = parse_start_ticks(payload.decode("utf-8", errors="replace"))
    if ticks is None:
        return ShellProbe(gone=False, start_ticks=None, error=f"cannot parse start ticks from {path}")
    return ShellProbe(gone=False, start_ticks=ticks, error=None)
