"""Bounded parsing for Linux ``/proc/PID/stat`` process identities.

The kernel's parenthesized ``comm`` field is opaque bytes, not UTF-8.  Only the
ASCII fields following its final closing parenthesis are decoded here.
"""

from __future__ import annotations

from dataclasses import dataclass

__all__ = ["ProcessStat", "parse_process_stat"]

_STARTTIME_OFFSET = 22 - 3
_MAX_PROCESS_ID = (1 << 31) - 1
_MAX_U64 = (1 << 64) - 1
_PROCESS_STATES = frozenset(
    (b"R", b"S", b"D", b"Z", b"T", b"t", b"X", b"x", b"K", b"W", b"P", b"I")
)


@dataclass(frozen=True)
class ProcessStat:
    """Validated process identity fields from one Linux procfs record."""

    pid: int
    starttime: int
    ppid: int
    pgrp: int
    session: int
    state: str


def parse_process_stat(record: bytes | str) -> ProcessStat | None:
    """Return validated process identity fields, or ``None`` for malformed input."""
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
        parsed = ProcessStat(
            pid=int(raw[:opening]),
            state=fields[0].decode("ascii"),
            ppid=int(fields[1]),
            pgrp=int(fields[2]),
            session=int(fields[3]),
            starttime=int(fields[_STARTTIME_OFFSET]),
        )
    except (UnicodeError, ValueError):
        return None
    if (
        not 1 <= parsed.pid <= _MAX_PROCESS_ID
        or not 0 <= parsed.ppid <= _MAX_PROCESS_ID
        or not 0 <= parsed.pgrp <= _MAX_PROCESS_ID
        or not 0 <= parsed.session <= _MAX_PROCESS_ID
        or not 0 <= parsed.starttime <= _MAX_U64
    ):
        return None
    return parsed
