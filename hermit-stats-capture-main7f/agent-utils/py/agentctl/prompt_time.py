"""The send-time opening of a chat request prompt, decided when the prompt is typed.

The chat bridge queues a request prompt with its first line opened for the moment it was queued,
as ``Sent 2026.10.07:08:45 EDT. ``, and records the message's create time (``sent_at``) and that
exact opening (``opening``) in the queue document. A drain replaces the opening with one for the
moment it types the prompt, which may be long after a busy agent let it through, adding
``, delivered 1 h 12 min later`` once the prompt is ``LATE_PROMPT_AFTER_SECONDS`` or more late.
Every drain of the same queue must render the same text for the same document and instant.

The zone is the process's local zone (``TZ``, else ``/etc/localtime``), and its abbreviation is
always printed, or its numeric offset as ``UTC-04:00`` when the abbreviation is unusable.
"""

from __future__ import annotations

import re
import time

LATE_PROMPT_AFTER_SECONDS = 120

# ASCII digits only, as the chat subscription accepts.
_RFC3339 = re.compile(
    r"([0-9]{4})-([0-9]{2})-([0-9]{2})[Tt]([0-9]{2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]+))?"
    r"(?:([Zz])|([+-])([0-9]{2}):([0-9]{2}))",
    re.ASCII,
)
_ABBREVIATION = re.compile(r"[A-Za-z0-9+-]{1,8}", re.ASCII)


def _days_in_month(year: int, month: int) -> int:
    if month == 2:
        return 29 if year % 4 == 0 and (year % 100 != 0 or year % 400 == 0) else 28
    return 30 if month in (4, 6, 9, 11) else 31


def _days_from_civil(year: int, month: int, day: int) -> int:
    year -= month <= 2
    era = year // 400
    year_of_era = year - era * 400
    day_of_year = (153 * ((month + 9) % 12) + 2) // 5 + day - 1
    day_of_era = year_of_era * 365 + year_of_era // 4 - year_of_era // 100 + day_of_year
    return era * 146_097 + day_of_era - 719_468


def _civil_from_days(days: int) -> tuple[int, int, int]:
    """The proleptic Gregorian date ``days`` after 1970-01-01."""
    days += 719_468
    era = days // 146_097
    day_of_era = days - era * 146_097
    year_of_era = (day_of_era - day_of_era // 1_460 + day_of_era // 36_524 - day_of_era // 146_096) // 365
    day_of_year = day_of_era - (365 * year_of_era + year_of_era // 4 - year_of_era // 100)
    month_index = (5 * day_of_year + 2) // 153
    day = day_of_year - (153 * month_index + 2) // 5 + 1
    month = month_index + 3 if month_index < 10 else month_index - 9
    return year_of_era + era * 400 + (month <= 2), month, day


def rfc3339_instant(value: str) -> tuple[int, int, bool] | None:
    """``(seconds, nanoseconds, finer)`` for an RFC 3339 timestamp, or ``None``.

    Digits finer than a nanosecond are not rounded in; ``finer`` says a nonzero one was there.
    A leap second reads as the last nanosecond of the second before it.
    """
    match = _RFC3339.fullmatch(value)
    if match is None:
        return None
    year, month, day, hour, minute, second = (int(match.group(index)) for index in range(1, 7))
    if (not 1 <= month <= 12 or not 1 <= day <= _days_in_month(year, month)
            or hour > 23 or minute > 59 or second > 60):
        return None
    fraction = match.group(7) or ""
    nanos = int((fraction[:9] or "0").ljust(9, "0"))
    finer = any(digit != "0" for digit in fraction[9:])
    offset = 0
    if match.group(9) is not None:
        hours, minutes = int(match.group(10)), int(match.group(11))
        if hours > 23 or minutes > 59:
            return None
        offset = (hours * 3_600 + minutes * 60) * (-1 if match.group(9) == "-" else 1)
    if second == 60:
        second, nanos, finer = 59, 999_999_999, False
    local = _days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
    return local - offset, nanos, finer


def _numeric_offset(offset_seconds: int) -> str:
    sign = "-" if offset_seconds < 0 else "+"
    minutes = abs(offset_seconds) // 60
    return f"UTC{sign}{minutes // 60:02}:{minutes % 60:02}"


def _zone(seconds: int) -> tuple[int, str]:
    """The process's UTC offset and zone name at ``seconds``; UTC when it cannot be read."""
    try:
        local = time.localtime(seconds)
    except (OverflowError, OSError, ValueError):
        return 0, "UTC"
    offset = local.tm_gmtoff
    name = local.tm_zone if _ABBREVIATION.fullmatch(local.tm_zone or "") else _numeric_offset(offset)
    return offset, name


def _stamp(seconds: int) -> str:
    # The local date is computed from the offset, as the compiled drain does, not by the C library,
    # so the two agree under every zone, including one that counts leap seconds.
    offset, name = _zone(seconds)
    local = seconds + offset
    year, month, day = _civil_from_days(local // 86_400)
    minute_of_day = local % 86_400 // 60
    return f"{year:04}.{month:02}.{day:02}:{minute_of_day // 60:02}:{minute_of_day % 60:02} {name}"


def _duration_words(seconds: int) -> str:
    minutes = seconds // 60
    days, hours, minutes = minutes // 1_440, minutes // 60 % 24, minutes % 60
    if days:
        return f"{days} d {hours} h"
    if hours:
        return f"{hours} h {minutes} min"
    return f"{minutes} min"


def opening(created_at: str, now_nanos: int) -> str:
    """The opening for a message created at ``created_at``, typed at ``now_nanos``."""
    instant = rfc3339_instant(created_at)
    if instant is None:
        return ""
    seconds, nanos, finer = instant
    stamp = _stamp(seconds)
    waited = now_nanos - (seconds * 1_000_000_000 + nanos + int(finer))
    if waited >= LATE_PROMPT_AFTER_SECONDS * 1_000_000_000:
        return f"Sent {stamp}, delivered {_duration_words(waited // 1_000_000_000)} later. "
    return f"Sent {stamp}. "


def retime(text: str, sent_at: object, recorded: object, now_nanos: int | None = None) -> str:
    """The text to type for a queued prompt: ``recorded``, its opening, replaced by one for now.

    The text is returned unchanged unless it begins with exactly the recorded opening and
    ``sent_at`` is a readable create time, so a record that does not fit is never used to cut it.
    """
    if (not isinstance(sent_at, str) or not isinstance(recorded, str)
            or not recorded.startswith("Sent ") or not text.startswith(recorded)
            or rfc3339_instant(sent_at) is None):
        return text
    try:
        now = time.time_ns() if now_nanos is None else now_nanos
        # The compiled drain reads its clock in milliseconds; the same instant renders the same
        # text.
        fresh = opening(sent_at, now // 1_000_000 * 1_000_000)
    except (ArithmeticError, OSError, ValueError, TypeError):
        # A clock or calendar failure never stops a prompt being typed. Anything else, such as
        # the service's own shutdown signal, propagates.
        return text
    return fresh + text[len(recorded):] if fresh else text
