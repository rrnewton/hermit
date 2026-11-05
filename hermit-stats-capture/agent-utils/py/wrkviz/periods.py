"""Calendar rollups in the viewer's IANA timezone."""

from __future__ import annotations

import calendar
from dataclasses import dataclass
from datetime import date, datetime, time, timedelta, timezone
from zoneinfo import ZoneInfo


ROLLUP_KINDS = ("hourly", "daily", "weekly", "monthly", "quarterly")
DEFAULT_ROLLUP_KINDS = ROLLUP_KINDS[1:]


@dataclass(frozen=True)
class Period:
    """One timezone-aware calendar interval and its archive output path."""

    kind: str
    key: str
    label: str
    start_ms: int
    end_ms: int
    relative_path: str
    partial: bool


def _epoch_ms(local: datetime) -> int:
    return int(local.astimezone(timezone.utc).timestamp() * 1000)


def _midnight(day: date, zone: ZoneInfo) -> datetime:
    return datetime.combine(day, time.min, tzinfo=zone)


def _daily(day: date, zone: ZoneInfo, team: str, last_ms: int) -> Period:
    start = _midnight(day, zone)
    end = _midnight(day + timedelta(days=1), zone)
    week_year, week_number, _ = day.isocalendar()
    key = day.isoformat()
    return Period(
        kind="daily",
        key=key,
        label=day.strftime("%a %b %-d"),
        start_ms=_epoch_ms(start),
        end_ms=_epoch_ms(end),
        relative_path=(
            f"teams/{team}/summaries/daily/{week_year}-W{week_number:02d}/"
            f"{key}-{team}-daily.md"
        ),
        partial=last_ms + 1 < _epoch_ms(end),
    )


def _hourly(start_ms: int, zone: ZoneInfo, team: str, first_ms: int, last_ms: int) -> Period:
    end_ms = start_ms + 60 * 60 * 1000
    local = datetime.fromtimestamp(start_ms / 1000, tz=timezone.utc).astimezone(zone)
    utc = datetime.fromtimestamp(start_ms / 1000, tz=timezone.utc)
    key = utc.strftime("%Y-%m-%dT%HZ")
    offset = local.strftime("%z")
    offset_label = f"UTC{offset[:3]}:{offset[3:]}"
    return Period(
        kind="hourly",
        key=key,
        label=f"{local:%a %b %-d · %H}:00 {offset_label}",
        start_ms=start_ms,
        end_ms=end_ms,
        relative_path=(
            f"teams/{team}/summaries/hourly/{utc:%Y-%m-%d}/"
            f"{key}-{team}-hourly.md"
        ),
        partial=first_ms > start_ms or last_ms + 1 < end_ms,
    )


def _weekly(day: date, zone: ZoneInfo, team: str, last_ms: int) -> Period:
    monday = day - timedelta(days=day.weekday())
    end_day = monday + timedelta(days=7)
    year, number, _ = monday.isocalendar()
    start = _midnight(monday, zone)
    end = _midnight(end_day, zone)
    key = f"{year}-W{number:02d}"
    return Period(
        kind="weekly",
        key=key,
        label=f"Week {number} · {monday.strftime('%b %-d')}",
        start_ms=_epoch_ms(start),
        end_ms=_epoch_ms(end),
        relative_path=f"teams/{team}/summaries/weekly/{year}/{key}-{team}-weekly.md",
        partial=last_ms + 1 < _epoch_ms(end),
    )


def _monthly(day: date, zone: ZoneInfo, team: str, last_ms: int) -> Period:
    first = day.replace(day=1)
    if first.month == 12:
        next_month = date(first.year + 1, 1, 1)
    else:
        next_month = date(first.year, first.month + 1, 1)
    start = _midnight(first, zone)
    end = _midnight(next_month, zone)
    key = first.strftime("%Y-%m")
    return Period(
        kind="monthly",
        key=key,
        label=first.strftime("%B %Y"),
        start_ms=_epoch_ms(start),
        end_ms=_epoch_ms(end),
        relative_path=f"teams/{team}/summaries/monthly/{first.year}/{key}-{team}-monthly.md",
        partial=last_ms + 1 < _epoch_ms(end),
    )


def _quarterly(day: date, zone: ZoneInfo, team: str, last_ms: int) -> Period:
    quarter = (day.month - 1) // 3 + 1
    first_month = 3 * (quarter - 1) + 1
    first = date(day.year, first_month, 1)
    if quarter == 4:
        next_quarter = date(day.year + 1, 1, 1)
    else:
        next_quarter = date(day.year, first_month + 3, 1)
    start = _midnight(first, zone)
    end = _midnight(next_quarter, zone)
    key = f"{day.year}-Q{quarter}"
    return Period(
        kind="quarterly",
        key=key,
        label=f"Q{quarter} {day.year}",
        start_ms=_epoch_ms(start),
        end_ms=_epoch_ms(end),
        relative_path=f"teams/{team}/summaries/quarterly/{day.year}/{key}-{team}-quarterly.md",
        partial=last_ms + 1 < _epoch_ms(end),
    )


def periods_for_range(
    first_ms: int,
    last_ms: int,
    display_timezone: str,
    team_slug: str,
    kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
) -> tuple[Period, ...]:
    """Return each requested calendar interval touched by the inclusive UTC range."""

    if last_ms < first_ms:
        raise ValueError("timeline range ends before it starts")
    if not kinds or len(kinds) != len(set(kinds)):
        raise ValueError("rollup kinds must be a non-empty unique sequence")
    unsupported = sorted(set(kinds) - set(ROLLUP_KINDS))
    if unsupported:
        raise ValueError("unsupported rollup kind(s): " + ", ".join(unsupported))
    zone = ZoneInfo(display_timezone)
    first_day = datetime.fromtimestamp(first_ms / 1000, tz=timezone.utc).astimezone(zone).date()
    last_day = datetime.fromtimestamp(last_ms / 1000, tz=timezone.utc).astimezone(zone).date()
    days: list[date] = []
    cursor = first_day
    while cursor <= last_day:
        days.append(cursor)
        cursor += timedelta(days=1)

    result: list[Period] = []
    if "hourly" in kinds:
        hour_ms = 60 * 60 * 1000
        cursor_ms = (first_ms // hour_ms) * hour_ms
        while cursor_ms <= last_ms:
            result.append(_hourly(cursor_ms, zone, team_slug, first_ms, last_ms))
            cursor_ms += hour_ms
    if "daily" in kinds:
        result.extend(_daily(day, zone, team_slug, last_ms) for day in days)
    weeks = {day - timedelta(days=day.weekday()) for day in days}
    if "weekly" in kinds:
        result.extend(_weekly(day, zone, team_slug, last_ms) for day in sorted(weeks))
    months = {day.replace(day=1) for day in days}
    if "monthly" in kinds:
        result.extend(_monthly(day, zone, team_slug, last_ms) for day in sorted(months))
    quarters = {(day.year, (day.month - 1) // 3 + 1) for day in days}
    if "quarterly" in kinds:
        for year, quarter in sorted(quarters):
            result.append(
                _quarterly(
                    date(year, 3 * (quarter - 1) + 1, 1),
                    zone,
                    team_slug,
                    last_ms,
                )
            )
    order = {kind: index for index, kind in enumerate(ROLLUP_KINDS)}
    result.sort(key=lambda period: (period.start_ms, order[period.kind], period.key))
    return tuple(result)


def period_heading(period: Period, display_timezone: str) -> str:
    """Format a human-readable heading for *period* in the display timezone."""

    zone = ZoneInfo(display_timezone)
    start = datetime.fromtimestamp(period.start_ms / 1000, tz=timezone.utc).astimezone(zone)
    end = datetime.fromtimestamp(period.end_ms / 1000, tz=timezone.utc).astimezone(zone)
    end_inclusive = end - timedelta(milliseconds=1)
    suffix = " (partial)" if period.partial else ""
    if period.kind == "hourly":
        return f"{start:%Y-%m-%d %a %H:%M %Z}{suffix}"
    if period.kind == "daily":
        return start.strftime(f"%Y-%m-%d %a{suffix}")
    if period.kind == "weekly":
        return f"{period.key} · {start:%Y-%m-%d} to {end_inclusive:%Y-%m-%d}{suffix}"
    return f"{period.label}{suffix}"


__all__ = [
    "DEFAULT_ROLLUP_KINDS",
    "ROLLUP_KINDS",
    "Period",
    "period_heading",
    "periods_for_range",
]
