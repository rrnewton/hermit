"""Tests for independently selectable calendar summary intervals."""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from wrkviz.periods import period_heading, periods_for_range


def _ms(value: str) -> int:
    return int(datetime.fromisoformat(value).replace(tzinfo=timezone.utc).timestamp() * 1000)


def test_exact_hour_selection_produces_one_complete_hour() -> None:
    start = _ms("2026-08-07T02:00:00")
    periods = periods_for_range(
        start,
        _ms("2026-08-07T03:00:00") - 1,
        "America/New_York",
        "codex-coord-030",
        ("hourly",),
    )

    assert len(periods) == 1
    assert periods[0].kind == "hourly"
    assert periods[0].key == "2026-08-07T02Z"
    assert periods[0].partial is False
    assert "2026-08-06 Thu 22:00 EDT" == period_heading(
        periods[0], "America/New_York"
    )


def test_repeated_dst_hour_has_two_unambiguous_utc_keys() -> None:
    periods = periods_for_range(
        _ms("2026-11-01T05:00:00"),
        _ms("2026-11-01T07:00:00") - 1,
        "America/New_York",
        "team",
        ("hourly",),
    )

    assert [period.key for period in periods] == [
        "2026-11-01T05Z",
        "2026-11-01T06Z",
    ]
    assert "UTC-04:00" in periods[0].label
    assert "UTC-05:00" in periods[1].label


def test_rollup_kind_selection_fails_closed() -> None:
    with pytest.raises(ValueError, match="non-empty unique"):
        periods_for_range(0, 1, "UTC", "team", ())
    with pytest.raises(ValueError, match="unsupported"):
        periods_for_range(0, 1, "UTC", "team", ("fortnightly",))
