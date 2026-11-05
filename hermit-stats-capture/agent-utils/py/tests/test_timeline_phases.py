"""Focused equivalence and argument-boundary tests for phase construction."""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from wrkviz.model import Agent, Event, TeamData
from wrkviz.phases import build_phases


ROOT = "root"
CHILD = "child"
NESTED = "nested"
START = 1_800_000_000_000
NEXT_BUCKET = START + 30 * 60 * 1000


def _event(event_id: str, thread_id: str, offset_ms: int, text: str) -> Event:
    return Event(
        event_id=event_id,
        thread_id=thread_id,
        turn_id=None,
        timestamp_ms=START + offset_ms,
        kind="assistant_message",
        role="assistant",
        phase="commentary",
        text=text,
        content_availability="plaintext",
        encrypted_content=None,
        author=thread_id,
        recipient=None,
        source_line=1,
    )


def _team() -> TeamData:
    # Deliberately scramble the source order and share one timestamp across ancestors.
    # The reference behavior orders all eligible context by (timestamp, event_id).
    events = (
        _event("z-root-tie", ROOT, 400, "root event ordered second at the tie"),
        _event("nested-early", NESTED, 300, "nested terminology before this phase"),
        _event("root-early", ROOT, 100, "root owner terminology " * 6),
        _event("a-child-tie", CHILD, 400, "child event ordered first at the tie"),
        _event("child-early", CHILD, 200, "child workstream terminology " * 4),
        _event(
            "root-boundary",
            ROOT,
            NEXT_BUCKET - START,
            "equal to the phase start and therefore excluded from prior context",
        ),
        _event(
            "nested-activity",
            NESTED,
            NEXT_BUCKET - START,
            "activity that creates the phase under test",
        ),
    )
    return TeamData(
        team_slug="phase-test",
        provider="codex",
        root_thread_id=ROOT,
        display_timezone="UTC",
        sources=(),
        agents=(
            Agent(ROOT, None, "/root", None, None, 0, START, NEXT_BUCKET + 1_000, "completed", "root"),
            Agent(CHILD, ROOT, "/root/child", None, None, 1, START, NEXT_BUCKET + 1_000, "completed", "child"),
            Agent(NESTED, CHILD, "/root/child/nested", None, None, 2, START, NEXT_BUCKET + 1_000, "completed", "nested"),
        ),
        turns=(),
        events=events,
        tool_calls=(),
        edges=(),
    )


def _reference_prior_context(team: TeamData, start_ms: int, max_chars: int) -> str:
    ancestor_ids = {ROOT, CHILD, NESTED}
    relevant = sorted(
        (
            event
            for event in team.events
            if event.thread_id in ancestor_ids and event.timestamp_ms < start_ms
        ),
        key=lambda event: (event.timestamp_ms, event.event_id),
    )
    lines = []
    for event in relevant:
        at = datetime.fromtimestamp(
            event.timestamp_ms / 1000, tz=timezone.utc
        ).isoformat().replace("+00:00", "Z")
        lines.append(f"[{at}] {event.kind}: {(event.text or '').strip()}")
    return "\n\n".join(lines)[-max_chars:]


def test_prior_context_matches_global_reference_when_truncated_inside_line() -> None:
    team = _team()
    context_chars = 137
    phase = next(
        item
        for item in build_phases(team, context_chars=context_chars)
        if item.agent_id == NESTED and item.start_ms == NEXT_BUCKET
    )

    expected = _reference_prior_context(team, phase.start_ms, context_chars)
    assert len(expected) == context_chars
    assert not expected.startswith("[")
    assert phase.prior_context == expected
    assert "equal to the phase start" not in phase.prior_context


@pytest.mark.parametrize(
    ("context_chars", "transcript_chars", "message"),
    [
        (0, 30_000, "context_chars must be positive"),
        (16_000, 0, "transcript_chars must be positive"),
    ],
)
def test_phase_character_limits_must_be_positive(
    context_chars: int, transcript_chars: int, message: str
) -> None:
    with pytest.raises(ValueError, match=message):
        build_phases(
            _team(),
            context_chars=context_chars,
            transcript_chars=transcript_chars,
        )


def test_final_event_at_agent_end_is_included_in_half_open_phase() -> None:
    final_at = START + 2_000
    final = _event("final", ROOT, 2_000, "final response must remain searchable")
    team = TeamData(
        team_slug="phase-final-event",
        provider="codex",
        root_thread_id=ROOT,
        display_timezone="UTC",
        sources=(),
        agents=(
            Agent(
                ROOT,
                None,
                "/root",
                None,
                None,
                0,
                START,
                final_at,
                "completed",
                "root",
            ),
        ),
        turns=(),
        events=(final,),
        tool_calls=(),
        edges=(),
    )

    phases = build_phases(team)

    assert len(phases) == 1
    assert phases[0].end_ms == final_at + 1
    assert [entry.text for entry in phases[0].transcript] == [
        "final response must remain searchable"
    ]
