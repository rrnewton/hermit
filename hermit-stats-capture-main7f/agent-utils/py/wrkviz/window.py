"""Provider-neutral calendar-date and exact-time windows for timeline archives."""

from __future__ import annotations

from dataclasses import dataclass, replace
from datetime import date, datetime, time, timezone
import re
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from wrkviz.model import TeamData


@dataclass(frozen=True)
class DateWindow:
    """A half-open UTC interval derived from calendar dates or exact instants."""

    start_date: str | None
    end_date: str | None
    start_ms: int | None
    end_ms: int | None
    start_time: str | None = None
    end_time: str | None = None

    def to_json_obj(self) -> dict[str, object]:
        """Return stable manifest metadata for this local-calendar selection."""

        result: dict[str, object] = {
            "start_date": self.start_date,
            "end_date": self.end_date,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
        }
        # Keep date-only manifests byte-compatible with archives created before exact bounds.
        if self.start_time is not None or self.end_time is not None:
            result["start_time"] = self.start_time
            result["end_time"] = self.end_time
        return result

    def contains(self, timestamp_ms: int) -> bool:
        """Return whether a point timestamp is inside this half-open interval."""

        if self.start_ms is not None and timestamp_ms < self.start_ms:
            return False
        return self.end_ms is None or timestamp_ms < self.end_ms

    def overlaps(self, start_ms: int, end_ms: int | None) -> bool:
        """Return whether a non-empty lifetime intersects the interval."""

        effective_end = end_ms if end_ms is not None else start_ms + 1
        if effective_end <= start_ms:
            effective_end = start_ms + 1
        if self.end_ms is not None and start_ms >= self.end_ms:
            return False
        return self.start_ms is None or effective_end > self.start_ms


def _parse_date(raw: str, label: str) -> date:
    try:
        parsed = date.fromisoformat(raw)
    except ValueError as error:
        raise ValueError(f"{label} must be a calendar date in YYYY-MM-DD form") from error
    if parsed.isoformat() != raw:
        raise ValueError(f"{label} must be a calendar date in YYYY-MM-DD form")
    return parsed


def _midnight_ms(value: date, timezone_name: str) -> int:
    try:
        display_timezone = ZoneInfo(timezone_name)
    except ZoneInfoNotFoundError as error:
        raise ValueError(f"unknown display timezone {timezone_name!r}") from error
    local_midnight = datetime.combine(value, time.min, tzinfo=display_timezone)
    return int(local_midnight.timestamp() * 1000)


_RFC3339_INSTANT = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"
    r"(?:\.[0-9]{1,6})?(?:Z|[+-][0-9]{2}:[0-9]{2})$"
)


def _parse_time(raw: str, label: str) -> tuple[str, int]:
    if _RFC3339_INSTANT.fullmatch(raw) is None:
        raise ValueError(
            f"{label} must be an RFC3339 timestamp with an explicit offset or Z"
        )
    value = raw[:-1] + "+00:00" if raw.endswith("Z") else raw
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError as error:
        raise ValueError(
            f"{label} must be an RFC3339 timestamp with an explicit offset or Z"
        ) from error
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError(f"{label} must include an explicit offset or Z")
    timestamp_ms = int(parsed.timestamp() * 1000)
    canonical = datetime.fromtimestamp(
        timestamp_ms / 1000, tz=timezone.utc
    ).isoformat(timespec="milliseconds").replace("+00:00", "Z")
    return canonical, timestamp_ms


def parse_date_window(
    start_date: str | None,
    end_date: str | None,
    timezone_name: str,
    *,
    start_time: str | None = None,
    end_time: str | None = None,
) -> DateWindow | None:
    """Parse an inclusive start and exclusive end as dates or exact instants.

    Calendar dates denote midnight in ``timezone_name``. Exact times must be RFC3339
    timestamps with an explicit offset or ``Z``. Either bound may be omitted, and a date
    may be paired with an exact instant at the opposite bound.
    """

    try:
        ZoneInfo(timezone_name)
    except ZoneInfoNotFoundError as error:
        raise ValueError(f"unknown display timezone {timezone_name!r}") from error
    if start_date is not None and start_time is not None:
        raise ValueError("choose either start date or start time, not both")
    if end_date is not None and end_time is not None:
        raise ValueError("choose either end date or end time, not both")
    if (
        start_date is None
        and end_date is None
        and start_time is None
        and end_time is None
    ):
        return None
    parsed_start = _parse_date(start_date, "start date") if start_date is not None else None
    parsed_end = _parse_date(end_date, "end date") if end_date is not None else None
    canonical_start_time: str | None = None
    canonical_end_time: str | None = None
    start_ms: int | None
    end_ms: int | None
    if start_time is not None:
        canonical_start_time, start_ms = _parse_time(start_time, "start time")
    else:
        start_ms = (
            _midnight_ms(parsed_start, timezone_name)
            if parsed_start is not None
            else None
        )
    if end_time is not None:
        canonical_end_time, end_ms = _parse_time(end_time, "end time")
    else:
        end_ms = (
            _midnight_ms(parsed_end, timezone_name)
            if parsed_end is not None
            else None
        )
    if start_ms is not None and end_ms is not None and start_ms >= end_ms:
        raise ValueError("start bound must be earlier than end bound")
    return DateWindow(
        start_date=start_date,
        end_date=end_date,
        start_ms=start_ms,
        end_ms=end_ms,
        start_time=canonical_start_time,
        end_time=canonical_end_time,
    )


def apply_date_window(team: TeamData, window: DateWindow | None) -> TeamData:
    """Attach a display/summarization window while retaining earlier context.

    Providers deliberately keep their complete normalized event set. Phase construction and
    rendering honor these bounds, while summary prompts can still read earlier parent history for
    consistent terminology.
    """

    if window is None:
        return replace(team, window_start_ms=None, window_end_ms=None)
    return replace(
        team,
        window_start_ms=window.start_ms,
        window_end_ms=window.end_ms,
    )


__all__ = ["DateWindow", "apply_date_window", "parse_date_window"]
