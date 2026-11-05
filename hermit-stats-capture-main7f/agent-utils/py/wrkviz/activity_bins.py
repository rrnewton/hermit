"""Token-free activity bins for semantic timeline zoom levels."""

from __future__ import annotations

from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from datetime import date, datetime, time, timedelta, timezone
from typing import Literal, TypeAlias
from zoneinfo import ZoneInfo

from wrkviz.phases import PhaseWindow


ActivityResolution: TypeAlias = Literal["hourly", "daily", "weekly"]
ActivityRole: TypeAlias = Literal["coordinator", "workers"]
Interval: TypeAlias = tuple[int, int]

_HOUR_MS = 60 * 60 * 1000
_ACTIVITY_EVIDENCE_HALF_WINDOW_MS = 150 * 1000
_ACTIVE_STATE_KINDS = frozenset({"active", "tool"})
_ROLES: tuple[ActivityRole, ...] = ("coordinator", "workers")
_RESOLUTIONS: tuple[ActivityResolution, ...] = ("hourly", "daily", "weekly")


@dataclass(frozen=True)
class ActivityBin:
    """One non-empty aggregate activity interval for a team role."""

    team: str
    role: ActivityRole
    resolution: ActivityResolution
    start_ms: int
    end_ms: int
    avg_active_concurrency: float
    peak_concurrency: int
    activity_coverage_fraction: float
    distinct_active_agents: int
    avg_present_concurrency: float = 0.0
    peak_present_concurrency: int = 0
    activity_evidence_fraction: float = 0.0
    activity_evidence_events: int = 0
    timing_quality: str = "inferred"

    def to_json_obj(self) -> dict[str, object]:
        """Return a stable JSON representation consumed by the static site."""

        return {
            "team": self.team,
            "role": self.role,
            "resolution": self.resolution,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "avg_active_concurrency": self.avg_active_concurrency,
            "peak_concurrency": self.peak_concurrency,
            "activity_coverage_fraction": self.activity_coverage_fraction,
            "distinct_active_agents": self.distinct_active_agents,
            "avg_present_concurrency": self.avg_present_concurrency,
            "peak_present_concurrency": self.peak_present_concurrency,
            "activity_evidence_fraction": self.activity_evidence_fraction,
            "activity_evidence_events": self.activity_evidence_events,
            "timing_quality": self.timing_quality,
        }


def _merge_intervals(intervals: Iterable[Interval]) -> tuple[Interval, ...]:
    ordered = sorted((start, end) for start, end in intervals if end > start)
    merged: list[Interval] = []
    for start, end in ordered:
        if not merged or start > merged[-1][1]:
            merged.append((start, end))
            continue
        previous_start, previous_end = merged[-1]
        merged[-1] = (previous_start, max(previous_end, end))
    return tuple(merged)


def _aligned_start(timestamp_ms: int, size_ms: int, offset_ms: int) -> int:
    return ((timestamp_ms - offset_ms) // size_ms) * size_ms + offset_ms


def _epoch_ms(value: datetime) -> int:
    return int(value.astimezone(timezone.utc).timestamp() * 1000)


def _midnight(day: date, zone: ZoneInfo) -> datetime:
    return datetime.combine(day, time.min, tzinfo=zone)


def _bucket_bounds(
    timestamp_ms: int,
    resolution: ActivityResolution,
    zone: ZoneInfo,
) -> Interval:
    if resolution == "hourly":
        start_ms = _aligned_start(timestamp_ms, _HOUR_MS, 0)
        return start_ms, start_ms + _HOUR_MS

    local_day = datetime.fromtimestamp(
        timestamp_ms / 1000, tz=timezone.utc
    ).astimezone(zone).date()
    if resolution == "weekly":
        local_day -= timedelta(days=local_day.weekday())
        next_day = local_day + timedelta(days=7)
    else:
        next_day = local_day + timedelta(days=1)
    return _epoch_ms(_midnight(local_day, zone)), _epoch_ms(
        _midnight(next_day, zone)
    )


def _peak_concurrency(intervals_by_agent: dict[str, tuple[Interval, ...]]) -> int:
    deltas: dict[int, int] = {}
    for intervals in intervals_by_agent.values():
        for start, end in intervals:
            deltas[start] = deltas.get(start, 0) + 1
            deltas[end] = deltas.get(end, 0) - 1
    current = 0
    peak = 0
    for timestamp_ms in sorted(deltas):
        current += deltas[timestamp_ms]
        peak = max(peak, current)
    return peak


def _build_resolution(
    team: str,
    active_by_role: dict[ActivityRole, dict[str, tuple[Interval, ...]]],
    present_by_role: dict[ActivityRole, dict[str, tuple[Interval, ...]]],
    evidence_by_role: dict[ActivityRole, dict[str, tuple[Interval, ...]]],
    evidence_events_by_role: dict[ActivityRole, tuple[int, ...]],
    resolution: ActivityResolution,
    zone: ZoneInfo,
    observed_start_ms: int,
    observed_end_ms: int,
) -> list[ActivityBin]:
    def split_intervals(
        intervals_by_role: dict[
            ActivityRole, dict[str, tuple[Interval, ...]]
        ],
    ) -> dict[tuple[ActivityRole, int, int], dict[str, list[Interval]]]:
        bins: dict[tuple[ActivityRole, int, int], dict[str, list[Interval]]] = {}
        for role in _ROLES:
            for agent_id, intervals in intervals_by_role[role].items():
                for start, end in intervals:
                    cursor_ms = max(start, observed_start_ms)
                    effective_end_ms = min(end, observed_end_ms)
                    while cursor_ms < effective_end_ms:
                        calendar_start_ms, calendar_end_ms = _bucket_bounds(
                            cursor_ms, resolution, zone
                        )
                        bucket_start_ms = max(calendar_start_ms, observed_start_ms)
                        bucket_end_ms = min(calendar_end_ms, observed_end_ms)
                        clipped_start = max(start, bucket_start_ms)
                        clipped_end = min(end, bucket_end_ms)
                        if clipped_end > clipped_start:
                            agents = bins.setdefault(
                                (role, bucket_start_ms, bucket_end_ms), {}
                            )
                            agents.setdefault(agent_id, []).append(
                                (clipped_start, clipped_end)
                            )
                        cursor_ms = calendar_end_ms
        return bins

    active_bins = split_intervals(active_by_role)
    present_bins = split_intervals(present_by_role)
    evidence_bins = split_intervals(evidence_by_role)
    evidence_counts: dict[tuple[ActivityRole, int, int], int] = {}
    for role in _ROLES:
        for timestamp_ms in evidence_events_by_role[role]:
            if timestamp_ms < observed_start_ms or timestamp_ms >= observed_end_ms:
                continue
            calendar_start_ms, calendar_end_ms = _bucket_bounds(
                timestamp_ms, resolution, zone
            )
            key = (
                role,
                max(calendar_start_ms, observed_start_ms),
                min(calendar_end_ms, observed_end_ms),
            )
            evidence_counts[key] = evidence_counts.get(key, 0) + 1

    def merged_agents(
        bins: dict[tuple[ActivityRole, int, int], dict[str, list[Interval]]],
        key: tuple[ActivityRole, int, int],
    ) -> dict[str, tuple[Interval, ...]]:
        return {
            agent_id: _merge_intervals(intervals)
            for agent_id, intervals in bins.get(key, {}).items()
        }

    def total_agent_ms(intervals_by_agent: dict[str, tuple[Interval, ...]]) -> int:
        return sum(
            end - start
            for intervals in intervals_by_agent.values()
            for start, end in intervals
        )

    result: list[ActivityBin] = []
    for role in _ROLES:
        bounds = sorted(
            (start_ms, end_ms)
            for bin_role, start_ms, end_ms in (
                set(active_bins) | set(present_bins) | set(evidence_bins)
            )
            if bin_role == role
        )
        for start_ms, end_ms in bounds:
            key = (role, start_ms, end_ms)
            intervals_by_agent = merged_agents(active_bins, key)
            present_intervals_by_agent = merged_agents(present_bins, key)
            evidence_intervals_by_agent = merged_agents(evidence_bins, key)
            active_agent_ms = total_agent_ms(intervals_by_agent)
            present_agent_ms = total_agent_ms(present_intervals_by_agent)
            all_intervals = (
                interval
                for intervals in intervals_by_agent.values()
                for interval in intervals
            )
            covered_ms = sum(
                end - start for start, end in _merge_intervals(all_intervals)
            )
            duration_ms = end_ms - start_ms
            if duration_ms <= 0:
                continue
            evidence_intervals = (
                interval
                for intervals in evidence_intervals_by_agent.values()
                for interval in intervals
            )
            evidence_covered_ms = sum(
                end - start for start, end in _merge_intervals(evidence_intervals)
            )
            result.append(
                ActivityBin(
                    team=team,
                    role=role,
                    resolution=resolution,
                    start_ms=start_ms,
                    end_ms=end_ms,
                    avg_active_concurrency=round(active_agent_ms / duration_ms, 6),
                    peak_concurrency=_peak_concurrency(intervals_by_agent),
                    activity_coverage_fraction=round(covered_ms / duration_ms, 6),
                    distinct_active_agents=len(intervals_by_agent),
                    avg_present_concurrency=round(
                        present_agent_ms / duration_ms, 6
                    ),
                    peak_present_concurrency=_peak_concurrency(
                        present_intervals_by_agent
                    ),
                    activity_evidence_fraction=round(
                        evidence_covered_ms / duration_ms, 6
                    ),
                    activity_evidence_events=evidence_counts.get(key, 0),
                )
            )
    return result


def build_activity_bins(
    team: str,
    coordinator_agent_id: str,
    phases: Sequence[PhaseWindow],
    *,
    display_timezone: str,
    observed_start_ms: int,
    observed_end_ms: int,
) -> tuple[ActivityBin, ...]:
    """Aggregate active/tool intervals inside the observed archive range."""

    if observed_end_ms <= observed_start_ms:
        raise ValueError("observed activity range must have positive duration")
    zone = ZoneInfo(display_timezone)

    raw_active_by_role: dict[ActivityRole, dict[str, list[Interval]]] = {
        "coordinator": {},
        "workers": {},
    }
    raw_present_by_role: dict[ActivityRole, dict[str, list[Interval]]] = {
        "coordinator": {},
        "workers": {},
    }
    raw_evidence_by_role: dict[ActivityRole, dict[str, list[Interval]]] = {
        "coordinator": {},
        "workers": {},
    }
    evidence_events_by_role: dict[ActivityRole, list[int]] = {
        "coordinator": [],
        "workers": [],
    }
    for phase in phases:
        role: ActivityRole = (
            "coordinator" if phase.agent_id == coordinator_agent_id else "workers"
        )
        phase_active_intervals = [
            (state.start_ms, state.end_ms)
            for state in phase.states
            if state.kind.lower() in _ACTIVE_STATE_KINDS
            and state.end_ms > state.start_ms
        ]
        active_intervals = raw_active_by_role[role].setdefault(phase.agent_id, [])
        active_intervals.extend(phase_active_intervals)
        evidence_intervals = raw_evidence_by_role[role].setdefault(
            phase.agent_id, []
        )
        phase_evidence_intervals: list[Interval] = []
        for entry in phase.transcript:
            evidence_events_by_role[role].append(entry.at_ms)
            phase_evidence_intervals.append(
                (
                    entry.at_ms - _ACTIVITY_EVIDENCE_HALF_WINDOW_MS,
                    entry.at_ms + _ACTIVITY_EVIDENCE_HALF_WINDOW_MS,
                )
            )
        evidence_intervals.extend(phase_evidence_intervals)
        # Outer-zoom worker height is intentionally based on provider-neutral,
        # fixed-width evidence pulses. Exact state spans remain available in the
        # legacy duration fields, but Orc's one-second point surrogates are not
        # comparable to observed Codex/Claude turn durations.
        raw_present_by_role[role].setdefault(phase.agent_id, []).extend(
            phase_evidence_intervals or phase_active_intervals
        )

    def merge_roles(
        source: dict[ActivityRole, dict[str, list[Interval]]],
    ) -> dict[ActivityRole, dict[str, tuple[Interval, ...]]]:
        return {
            role: {
                agent_id: merged
                for agent_id, intervals in source[role].items()
                if (merged := _merge_intervals(intervals))
            }
            for role in _ROLES
        }

    active_by_role = merge_roles(raw_active_by_role)
    present_by_role = merge_roles(raw_present_by_role)
    evidence_by_role = merge_roles(raw_evidence_by_role)
    evidence_events = {
        role: tuple(evidence_events_by_role[role]) for role in _ROLES
    }
    result: list[ActivityBin] = []
    for resolution in _RESOLUTIONS:
        result.extend(
            _build_resolution(
                team,
                active_by_role,
                present_by_role,
                evidence_by_role,
                evidence_events,
                resolution,
                zone,
                observed_start_ms,
                observed_end_ms,
            )
        )
    return tuple(result)


__all__ = ["ActivityBin", "ActivityResolution", "ActivityRole", "build_activity_bins"]
