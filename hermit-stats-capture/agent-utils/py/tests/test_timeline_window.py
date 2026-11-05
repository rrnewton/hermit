from __future__ import annotations

import json
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path

import pytest

from wrkviz.build_store import team_build_root
from wrkviz.archive import as_array, as_object, narrow_json, write_json_if_changed
from wrkviz.cli import _parser
from wrkviz.model import Agent, Edge, Event, SourceSnapshot, TeamData
from wrkviz.model_io import team_from_json_obj
from wrkviz.phases import build_phases
from wrkviz.pipeline import build_archive, summarize_archive
from wrkviz.window import apply_date_window, parse_date_window
from tests.timeline_projection import schema_1_timeline_text


ROOT = "root-thread"
OUTSIDE = "outside-thread"
SILENT = "silent-crossing-thread"
QUIET_CROSSING = "quiet-full-window-crossing"
ENDED_AT_START = "ended-at-window-start"
STARTED_AT_END = "started-at-window-end"


def _ms(value: str) -> int:
    return int(datetime.fromisoformat(value).astimezone(timezone.utc).timestamp() * 1000)


def _event(event_id: str, thread_id: str, timestamp_ms: int, text: str) -> Event:
    return Event(
        event_id=event_id,
        thread_id=thread_id,
        turn_id=None,
        timestamp_ms=timestamp_ms,
        kind="assistant_message",
        role="assistant",
        phase=None,
        text=text,
        content_availability="plaintext",
        encrypted_content=None,
        author=None,
        recipient=None,
        source_line=1,
    )


def _team() -> TeamData:
    start_ms = _ms("2026-07-21T04:00:00+00:00")
    end_ms = _ms("2026-07-22T04:00:00+00:00")
    return TeamData(
        team_slug="window-test",
        provider="fixture",
        root_thread_id=ROOT,
        display_timezone="America/New_York",
        sources=(SourceSnapshot("fixture", ROOT, 1, 1, "a" * 64, 1, 1),),
        agents=(
            Agent(ROOT, None, "/root", None, None, 0, start_ms - 10_000, None, "active", "x"),
            Agent(
                OUTSIDE,
                None,
                "/outside",
                None,
                None,
                0,
                end_ms + 1_000,
                None,
                "active",
                "y",
            ),
        ),
        turns=(),
        events=(
            _event("prior", ROOT, start_ms - 1_000, "Prior terminology context"),
            _event("start", ROOT, start_ms, "Included at the start"),
            _event("last", ROOT, end_ms - 1, "Included before the end"),
            _event("end", ROOT, end_ms, "Excluded at the end"),
            _event("outside", OUTSIDE, end_ms + 1_000, "Unrelated later work"),
        ),
        tool_calls=(),
        edges=(),
    )


def test_local_dates_become_half_open_utc_bounds_with_dst() -> None:
    spring = parse_date_window("2026-03-08", "2026-03-09", "America/New_York")
    assert spring is not None
    assert spring.end_ms is not None and spring.start_ms is not None
    assert spring.end_ms - spring.start_ms == 23 * 60 * 60 * 1000

    fall = parse_date_window("2026-11-01", "2026-11-02", "America/New_York")
    assert fall is not None
    assert fall.end_ms is not None and fall.start_ms is not None
    assert fall.end_ms - fall.start_ms == 25 * 60 * 60 * 1000


def test_exact_times_are_canonical_half_open_utc_bounds() -> None:
    window = parse_date_window(
        None,
        None,
        "America/New_York",
        start_time="2026-08-06T20:35:17.752-04:00",
        end_time="2026-08-07T14:25:28.290Z",
    )

    assert window is not None
    assert window.start_time == "2026-08-07T00:35:17.752Z"
    assert window.end_time == "2026-08-07T14:25:28.290Z"
    assert window.contains(_ms("2026-08-07T00:35:17.752+00:00"))
    assert not window.contains(_ms("2026-08-07T14:25:28.290+00:00"))
    assert window.to_json_obj() == {
        "start_date": None,
        "end_date": None,
        "start_ms": window.start_ms,
        "end_ms": window.end_ms,
        "start_time": "2026-08-07T00:35:17.752Z",
        "end_time": "2026-08-07T14:25:28.290Z",
    }


@pytest.mark.parametrize(
    ("start_time", "end_time", "message"),
    [
        ("2026-08-07T00:35:17", None, "explicit offset"),
        ("2026-08-07 00:35:17Z", None, "RFC3339"),
        (
            "2026-08-07T14:25:28.290Z",
            "2026-08-07T00:35:17.752Z",
            "earlier",
        ),
    ],
)
def test_invalid_exact_time_windows_are_rejected(
    start_time: str, end_time: str | None, message: str
) -> None:
    with pytest.raises(ValueError, match=message):
        parse_date_window(
            None,
            None,
            "UTC",
            start_time=start_time,
            end_time=end_time,
        )


def test_same_bound_cannot_mix_date_and_exact_time() -> None:
    with pytest.raises(ValueError, match="either start date or start time"):
        parse_date_window(
            "2026-08-07",
            None,
            "UTC",
            start_time="2026-08-07T00:35:17Z",
        )


def test_cli_accepts_exact_bounds_and_rejects_same_bound_date_mix() -> None:
    parser = _parser()
    parsed = parser.parse_args(
        (
            "ingest",
            "--output",
            "archive",
            "--team",
            "example",
            "--root-session",
            "root",
            "--start-time",
            "2026-08-06T22:00:00-04:00",
            "--end-time",
            "2026-08-07T07:00:00-04:00",
        )
    )
    assert parsed.start_time == "2026-08-06T22:00:00-04:00"
    assert parsed.end_time == "2026-08-07T07:00:00-04:00"

    exported = parser.parse_args(
        (
            "export",
            "--archive",
            "archive",
            "--output",
            "site",
            "--team",
            "codex-coord-030",
            "--team",
            "claude-coord-176",
            "--team",
            "orc-coord-014",
            "--start-time",
            "2026-08-06T22:00:00-04:00",
            "--end-time",
            "2026-08-07T07:00:00-04:00",
            "--rollup-kind",
            "hourly",
        )
    )
    assert exported.team == [
        "codex-coord-030",
        "claude-coord-176",
        "orc-coord-014",
    ]
    assert exported.rollup_kind == ["hourly"]

    with pytest.raises(SystemExit):
        parser.parse_args(
            (
                "ingest",
                "--output",
                "archive",
                "--team",
                "example",
                "--root-session",
                "root",
                "--start-date",
                "2026-08-06",
                "--start-time",
                "2026-08-06T22:00:00-04:00",
            )
        )


@pytest.mark.parametrize(
    ("start", "end", "zone", "message"),
    [
        ("07/21/2026", "2026-07-22", "UTC", "YYYY-MM-DD"),
        ("2026-07-22", "2026-07-22", "UTC", "earlier"),
        ("2026-07-23", "2026-07-22", "UTC", "earlier"),
        ("2026-07-21", "2026-07-22", "Mars/Olympus", "unknown display timezone"),
    ],
)
def test_invalid_date_windows_are_rejected(
    start: str, end: str, zone: str, message: str
) -> None:
    with pytest.raises(ValueError, match=message):
        parse_date_window(start, end, zone)


def test_phases_use_prior_context_but_exclude_both_outside_sides() -> None:
    window = parse_date_window("2026-07-21", "2026-07-22", "America/New_York")
    assert window is not None
    phases = build_phases(apply_date_window(_team(), window), phase_minutes=30)

    transcript = "\n".join(phase.transcript_text for phase in phases)
    context = "\n".join(phase.prior_context for phase in phases)
    assert "Included at the start" in transcript
    assert "Included before the end" in transcript
    assert "Prior terminology context" not in transcript
    assert "Excluded at the end" not in transcript
    assert "Unrelated later work" not in transcript
    assert "Prior terminology context" in context
    assert {phase.agent_id for phase in phases} == {ROOT}


def test_built_site_uses_exact_window_and_only_selected_agents(tmp_path: Path) -> None:
    window = parse_date_window("2026-07-21", "2026-07-22", "America/New_York")
    assert window is not None
    team = apply_date_window(_team(), window)
    raw_path = team_build_root(tmp_path, team.team_slug) / "raw" / "team.json"
    write_json_if_changed(raw_path, narrow_json(team.to_json_obj()))

    report = summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    built = build_archive(tmp_path, team.team_slug)
    timeline = json.loads(schema_1_timeline_text(tmp_path))

    assert report.agent_names == 1
    assert built["agents"] == 1
    assert timeline["range"] == {
        "start_ms": window.start_ms,
        "end_ms": window.end_ms,
    }
    assert [agent["id"] for agent in timeline["agents"]] == [ROOT]
    assert all(event["at_ms"] < window.end_ms for event in timeline["events"])
    assert [rollup["kind"] for rollup in timeline["rollups"]].count("daily") == 1
    daily = next(rollup for rollup in timeline["rollups"] if rollup["kind"] == "daily")
    assert "partial" not in daily["label"]


def test_built_site_keeps_silent_agent_whose_lifetime_overlaps_window(
    tmp_path: Path,
) -> None:
    window = parse_date_window("2026-07-21", "2026-07-22", "America/New_York")
    assert window is not None
    assert window.start_ms is not None
    assert window.end_ms is not None
    base = _team()
    spawn_at = window.start_ms + 1_000
    crossing = Agent(
        SILENT,
        ROOT,
        "/root/silent-crossing",
        None,
        "worker",
        1,
        spawn_at,
        window.end_ms + 1_000,
        "active",
        "silent",
    )
    quiet_crossing = Agent(
        QUIET_CROSSING,
        ROOT,
        "/root/quiet-full-window-crossing",
        None,
        "worker",
        1,
        window.start_ms - 1_000,
        window.end_ms + 1_000,
        "active",
        "quiet-crossing",
    )
    ended_at_start = Agent(
        ENDED_AT_START,
        ROOT,
        "/root/ended-at-start",
        None,
        "worker",
        1,
        window.start_ms - 1_000,
        window.start_ms,
        "completed",
        "before",
    )
    started_at_end = Agent(
        STARTED_AT_END,
        ROOT,
        "/root/started-at-end",
        None,
        "worker",
        1,
        window.end_ms,
        window.end_ms + 1_000,
        "active",
        "after",
    )
    spawn = Edge(
        "spawn-silent",
        "call-silent",
        ROOT,
        SILENT,
        "spawn",
        spawn_at,
        "Continue across midnight",
        "plaintext",
        None,
        1,
    )
    team = apply_date_window(
        replace(
            base,
            agents=base.agents
            + (crossing, quiet_crossing, ended_at_start, started_at_end),
            edges=(spawn,),
        ),
        window,
    )
    raw_path = team_build_root(tmp_path, team.team_slug) / "raw" / "team.json"
    write_json_if_changed(raw_path, narrow_json(team.to_json_obj()))

    report = summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    built = build_archive(tmp_path, team.team_slug)
    timeline = json.loads(schema_1_timeline_text(tmp_path))

    assert report.agent_names == 3
    assert built["agents"] == 3
    assert {agent["id"] for agent in timeline["agents"]} == {
        ROOT,
        SILENT,
        QUIET_CROSSING,
    }
    assert [edge["id"] for edge in timeline["edges"]] == ["spawn-silent"]


def test_team_json_round_trip_keeps_window_fields() -> None:
    window = parse_date_window("2026-07-21", "2026-07-22", "America/New_York")
    assert window is not None
    bounded = apply_date_window(_team(), window)
    restored = team_from_json_obj(bounded.to_json_obj())
    assert restored.window_start_ms == window.start_ms
    assert restored.window_end_ms == window.end_ms


def test_team_json_accepts_events_before_authorship_provenance_fields() -> None:
    raw = as_object(narrow_json(_team().to_json_obj()), "team")
    events = as_array(raw.get("events"), "team.events")
    for index, value in enumerate(events):
        event = as_object(value, f"team.events[{index}]")
        event.pop("ingress_kind")
        event.pop("author_kind")
        event.pop("source_native_id")
        event.pop("classification_version")

    restored = team_from_json_obj(raw)

    assert all(event.ingress_kind is None for event in restored.events)
    assert all(event.author_kind is None for event in restored.events)
    assert all(event.source_native_id is None for event in restored.events)
    assert all(event.classification_version is None for event in restored.events)
