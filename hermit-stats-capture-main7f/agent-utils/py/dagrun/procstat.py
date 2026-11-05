"""Parse the stable, ASCII tail of Linux ``/proc/PID/stat`` records.

The ``comm`` field is opaque kernel data wrapped in parentheses.  It can contain
whitespace, parentheses, newlines, and bytes which are not valid text in the
caller's locale.  Readers therefore must not decode or split the whole record.
Only the fields after the final closing parenthesis have a textual grammar.
"""

from __future__ import annotations

from dataclasses import dataclass

__all__ = ["ProcessStat", "parse_process_stat"]

_UTIME_OFFSET = 14 - 3
_STIME_OFFSET = 15 - 3
_STARTTIME_OFFSET = 22 - 3
_MAX_PID = (1 << 31) - 1
_MAX_U64 = (1 << 64) - 1
_PROCESS_STATES = frozenset(
    (b"R", b"S", b"D", b"Z", b"T", b"t", b"X", b"x", b"K", b"W", b"P", b"I")
)


@dataclass(frozen=True)
class ProcessStat:
    """The process-identity fields shared by dagrun's procfs readers."""

    pid: int
    state: str
    ppid: int
    pgrp: int
    session: int
    utime: int
    stime: int
    starttime: int


def parse_process_stat(record: bytes | str) -> ProcessStat | None:
    """Parse identity fields from one Linux ``/proc/PID/stat`` record.

    ``bytes`` is the production form.  ``str`` remains accepted for synthetic
    callers and is encoded with ``surrogateescape``, which round-trips a procfs
    record decoded that way.  Malformed or truncated records return
    ``None`` rather than supplying a partial process identity.
    """
    try:
        raw = (
            record
            if isinstance(record, bytes)
            else record.encode("utf-8", errors="surrogateescape")
        )
    except UnicodeEncodeError:
        return None

    opening = raw.find(b" (")
    closing = raw.rfind(b")")
    if (
        opening <= 0
        or closing <= opening + 1
        or raw[closing + 1 : closing + 2] != b" "
    ):
        return None
    fields = raw[closing + 2 :].split()
    if len(fields) <= _STARTTIME_OFFSET or fields[0] not in _PROCESS_STATES:
        return None
    try:
        pid = int(raw[:opening])
        ppid = int(fields[1])
        pgrp = int(fields[2])
        session = int(fields[3])
        utime = int(fields[_UTIME_OFFSET])
        stime = int(fields[_STIME_OFFSET])
        starttime = int(fields[_STARTTIME_OFFSET])
        state = fields[0].decode("ascii")
    except (UnicodeError, ValueError):
        return None
    if (
        not 1 <= pid <= _MAX_PID
        or not 0 <= ppid <= _MAX_PID
        or not 0 <= pgrp <= _MAX_PID
        or not 0 <= session <= _MAX_PID
        or not 0 <= utime <= _MAX_U64
        or not 0 <= stime <= _MAX_U64
        or not 0 <= starttime <= _MAX_U64
    ):
        return None
    return ProcessStat(
        pid=pid,
        state=state,
        ppid=ppid,
        pgrp=pgrp,
        session=session,
        utime=utime,
        stime=stime,
        starttime=starttime,
    )
