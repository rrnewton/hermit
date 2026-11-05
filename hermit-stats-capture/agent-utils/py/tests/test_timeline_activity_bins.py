"""Tests for token-free semantic-zoom activity bins."""

from __future__ import annotations

from datetime import datetime, timezone
from zoneinfo import ZoneInfo

import pytest

from wrkviz.activity_bins import ActivityBin, build_activity_bins
from wrkviz.phases import (
    PhaseStats,
    PhaseWindow,
    StateSegment,
    TranscriptEntry,
)


MINUTE_MS = 60 * 1000
MONDAY_MS = int(datetime(2026, 8, 10, tzinfo=timezone.utc).timestamp() * 1000)


def _phase(
    phase_id: str,
    agent_id: str,
    states: tuple[StateSegment, ...],
    transcript: tuple[TranscriptEntry, ...] = (),
) -> PhaseWindow:
    return PhaseWindow(
        phase_id=phase_id,
        summary_key=phase_id,
        agent_id=agent_id,
        agent_label=agent_id,
        start_ms=min(state.start_ms for state in states),
        end_ms=max(state.end_ms for state in states),
        stats=PhaseStats(0, 0, 0, 0),
        states=states,
        transcript_text="",
        prior_context="",
        transcript=transcript,
    )


def _state(kind: str, start_minute: int, end_minute: int) -> StateSegment:
    return StateSegment(
        start_ms=MONDAY_MS + start_minute * MINUTE_MS,
        end_ms=MONDAY_MS + end_minute * MINUTE_MS,
        kind=kind,
    )


def _absolute_state(kind: str, start_ms: int, end_ms: int) -> StateSegment:
    return StateSegment(start_ms=start_ms, end_ms=end_ms, kind=kind)


def _local_ms(
    year: int,
    month: int,
    day: int,
    hour: int = 0,
    minute: int = 0,
    *,
    timezone_name: str = "America/New_York",
) -> int:
    local = datetime(
        year,
        month,
        day,
        hour,
        minute,
        tzinfo=ZoneInfo(timezone_name),
    )
    return int(local.astimezone(timezone.utc).timestamp() * 1000)


def _find(
    bins: tuple[ActivityBin, ...],
    resolution: str,
    role: str,
    start_ms: int,
) -> ActivityBin:
    return next(
        item
        for item in bins
        if item.resolution == resolution
        and item.role == role
        and item.start_ms == start_ms
    )


def test_activity_bins_preserve_gaps_and_measure_concurrency() -> None:
    phases = (
        _phase("root", "root", (_state("active", 10, 40),)),
        _phase(
            "worker-a",
            "a",
            (
                _state("active", 0, 30),
                _state("idle", 30, 45),
                _state("active", 45, 75),
                _state("waiting", 120, 180),
            ),
        ),
        # Overlapping phase windows for one agent must not double-count concurrency.
        _phase("worker-a-overlap", "a", (_state("active", 20, 30),)),
        _phase("worker-b", "b", (_state("active", 15, 45),)),
        # Tool work counts as activity, while the preceding waiting interval does not.
        _phase(
            "worker-b-later",
            "b",
            (_state("waiting", 180, 190), _state("tool", 190, 200)),
        ),
    )
    bins = build_activity_bins(
        "test-team",
        "root",
        phases,
        display_timezone="UTC",
        observed_start_ms=MONDAY_MS,
        observed_end_ms=MONDAY_MS + 7 * 24 * 60 * MINUTE_MS,
    )

    coordinator = _find(bins, "hourly", "coordinator", MONDAY_MS)
    assert coordinator.avg_active_concurrency == 0.5
    assert coordinator.peak_concurrency == 1
    assert coordinator.activity_coverage_fraction == 0.5
    assert coordinator.distinct_active_agents == 1

    first_hour = _find(bins, "hourly", "workers", MONDAY_MS)
    assert first_hour.avg_active_concurrency == 1.25
    assert first_hour.peak_concurrency == 2
    assert first_hour.activity_coverage_fraction == 1.0
    assert first_hour.distinct_active_agents == 2

    second_hour = _find(bins, "hourly", "workers", MONDAY_MS + 60 * MINUTE_MS)
    assert second_hour.avg_active_concurrency == 0.25
    assert second_hour.peak_concurrency == 1
    assert second_hour.activity_coverage_fraction == 0.25
    assert second_hour.distinct_active_agents == 1

    worker_hour_starts = {
        item.start_ms
        for item in bins
        if item.resolution == "hourly" and item.role == "workers"
    }
    assert MONDAY_MS + 120 * MINUTE_MS not in worker_hour_starts
    assert MONDAY_MS + 180 * MINUTE_MS in worker_hour_starts

    day = _find(bins, "daily", "workers", MONDAY_MS)
    assert day.avg_active_concurrency == pytest.approx(100 / (24 * 60), abs=1e-6)
    assert day.activity_coverage_fraction == pytest.approx(85 / (24 * 60), abs=1e-6)
    assert day.peak_concurrency == 2
    assert day.distinct_active_agents == 2

    week = _find(bins, "weekly", "workers", MONDAY_MS)
    assert week.start_ms == MONDAY_MS
    assert week.end_ms == MONDAY_MS + 7 * 24 * 60 * MINUTE_MS
    assert week.avg_active_concurrency > 0


def test_point_activity_uses_inferred_presence_and_evidence_separately() -> None:
    point_ms = MONDAY_MS + 10 * MINUTE_MS
    phase = _phase(
        "worker-point",
        "worker",
        (
            _absolute_state("active", point_ms, point_ms + 1000),
            _absolute_state(
                "idle",
                point_ms + 1000,
                MONDAY_MS + 30 * MINUTE_MS,
            ),
        ),
        (TranscriptEntry(point_ms, "agent", "finished work", ()),),
    )

    bins = build_activity_bins(
        "test-team",
        "root",
        (phase,),
        display_timezone="UTC",
        observed_start_ms=MONDAY_MS,
        observed_end_ms=MONDAY_MS + 60 * MINUTE_MS,
    )

    hour = _find(bins, "hourly", "workers", MONDAY_MS)
    assert hour.avg_active_concurrency == pytest.approx(1 / 3600, abs=1e-6)
    assert hour.avg_present_concurrency == pytest.approx(5 / 60, abs=1e-6)
    assert hour.peak_present_concurrency == 1
    assert hour.activity_evidence_fraction == pytest.approx(5 / 60, abs=1e-6)
    assert hour.activity_evidence_events == 1
    assert hour.timing_quality == "inferred"


def test_daily_and_weekly_bins_follow_local_calendar_boundaries() -> None:
    observed_start = _local_ms(2026, 8, 9, 22)
    monday_start = _local_ms(2026, 8, 10)
    tuesday_start = _local_ms(2026, 8, 11)
    observed_end = _local_ms(2026, 8, 11, 2)
    phases = (
        _phase(
            "coordinator",
            "root",
            (
                _absolute_state(
                    "active",
                    _local_ms(2026, 8, 9, 23),
                    _local_ms(2026, 8, 9, 23, 30),
                ),
            ),
        ),
        _phase(
            "worker",
            "worker",
            (
                _absolute_state(
                    "active",
                    _local_ms(2026, 8, 10, 12),
                    _local_ms(2026, 8, 10, 13),
                ),
                _absolute_state(
                    "active",
                    _local_ms(2026, 8, 11, 0, 30),
                    _local_ms(2026, 8, 11, 1),
                ),
            ),
        ),
    )

    bins = build_activity_bins(
        "test-team",
        "root",
        phases,
        display_timezone="America/New_York",
        observed_start_ms=observed_start,
        observed_end_ms=observed_end,
    )

    sunday = _find(bins, "daily", "coordinator", observed_start)
    assert sunday.end_ms == monday_start
    assert sunday.activity_coverage_fraction == 0.25

    monday = _find(bins, "daily", "workers", monday_start)
    assert monday.end_ms == tuesday_start
    assert monday.activity_coverage_fraction == pytest.approx(1 / 24, abs=1e-6)

    tuesday = _find(bins, "daily", "workers", tuesday_start)
    assert tuesday.end_ms == observed_end
    assert tuesday.activity_coverage_fraction == 0.25

    prior_week = _find(bins, "weekly", "coordinator", observed_start)
    assert prior_week.end_ms == monday_start
    current_week = _find(bins, "weekly", "workers", monday_start)
    assert current_week.end_ms == observed_end


@pytest.mark.parametrize(
    ("week_start", "week_end", "transition_day", "expected_hours"),
    (
        ((2026, 3, 2), (2026, 3, 9), (2026, 3, 8), 167),
        ((2026, 10, 26), (2026, 11, 2), (2026, 11, 1), 169),
    ),
)
def test_calendar_bin_durations_follow_daylight_saving_time(
    week_start: tuple[int, int, int],
    week_end: tuple[int, int, int],
    transition_day: tuple[int, int, int],
    expected_hours: int,
) -> None:
    start_ms = _local_ms(*week_start)
    end_ms = _local_ms(*week_end)
    transition_start_ms = _local_ms(*transition_day)
    transition_end_ms = _local_ms(*transition_day, hour=0) + (
        23 * 60 * MINUTE_MS if expected_hours == 167 else 25 * 60 * MINUTE_MS
    )
    phase = _phase(
        "worker",
        "worker",
        (_absolute_state("active", start_ms, end_ms),),
    )

    bins = build_activity_bins(
        "test-team",
        "root",
        (phase,),
        display_timezone="America/New_York",
        observed_start_ms=start_ms,
        observed_end_ms=end_ms,
    )

    week = _find(bins, "weekly", "workers", start_ms)
    assert week.end_ms - week.start_ms == expected_hours * 60 * MINUTE_MS
    assert week.avg_active_concurrency == 1.0
    assert week.activity_coverage_fraction == 1.0

    transition = _find(bins, "daily", "workers", transition_start_ms)
    assert transition.end_ms == transition_end_ms
    assert transition.avg_active_concurrency == 1.0
    assert transition.activity_coverage_fraction == 1.0


def test_partial_current_day_uses_only_observed_duration() -> None:
    observed_start = _local_ms(2026, 8, 12)
    activity_end = _local_ms(2026, 8, 12, 4)
    observed_end = _local_ms(2026, 8, 12, 8)
    phase = _phase(
        "root",
        "root",
        (_absolute_state("active", observed_start, activity_end),),
    )

    bins = build_activity_bins(
        "test-team",
        "root",
        (phase,),
        display_timezone="America/New_York",
        observed_start_ms=observed_start,
        observed_end_ms=observed_end,
    )

    day = _find(bins, "daily", "coordinator", observed_start)
    assert day.end_ms == observed_end
    assert day.avg_active_concurrency == 0.5
    assert day.activity_coverage_fraction == 0.5
    week = _find(bins, "weekly", "coordinator", observed_start)
    assert week.end_ms == observed_end
    assert week.avg_active_concurrency == 0.5


def test_hourly_bins_keep_utc_boundaries_between_clipped_edges() -> None:
    observed_start = MONDAY_MS + 15 * MINUTE_MS
    utc_hour = MONDAY_MS + 60 * MINUTE_MS
    observed_end = MONDAY_MS + 105 * MINUTE_MS
    phase = _phase(
        "root",
        "root",
        (_absolute_state("active", observed_start, observed_end),),
    )

    bins = build_activity_bins(
        "test-team",
        "root",
        (phase,),
        display_timezone="America/New_York",
        observed_start_ms=observed_start,
        observed_end_ms=observed_end,
    )

    first = _find(bins, "hourly", "coordinator", observed_start)
    assert first.end_ms == utc_hour
    second = _find(bins, "hourly", "coordinator", utc_hour)
    assert second.end_ms == observed_end
    assert first.avg_active_concurrency == 1.0
    assert second.avg_active_concurrency == 1.0


def test_activity_bin_json_is_additive_and_self_describing() -> None:
    item = ActivityBin(
        team="team-a",
        role="workers",
        resolution="daily",
        start_ms=MONDAY_MS,
        end_ms=MONDAY_MS + 24 * 60 * MINUTE_MS,
        avg_active_concurrency=1.5,
        peak_concurrency=3,
        activity_coverage_fraction=0.75,
        distinct_active_agents=4,
        avg_present_concurrency=2.25,
        peak_present_concurrency=5,
        activity_evidence_fraction=0.5,
        activity_evidence_events=12,
    )
    assert item.to_json_obj() == {
        "team": "team-a",
        "role": "workers",
        "resolution": "daily",
        "start_ms": MONDAY_MS,
        "end_ms": MONDAY_MS + 24 * 60 * MINUTE_MS,
        "avg_active_concurrency": 1.5,
        "peak_concurrency": 3,
        "activity_coverage_fraction": 0.75,
        "distinct_active_agents": 4,
        "avg_present_concurrency": 2.25,
        "peak_present_concurrency": 5,
        "activity_evidence_fraction": 0.5,
        "activity_evidence_events": 12,
        "timing_quality": "inferred",
    }
