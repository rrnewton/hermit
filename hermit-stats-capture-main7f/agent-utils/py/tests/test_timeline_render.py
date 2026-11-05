"""Focused regression tests for indexed rendering helpers."""

from __future__ import annotations

from wrkviz.model import Agent, Edge, Event, TeamData, Turn
from wrkviz.naming import AgentNameResult
from wrkviz.render import _result_edge_objs


START = 1_800_000_000_000
PARENT = "parent"
OTHER_PARENT = "other-parent"
CHILD = "child"


def test_duplicate_turn_identity_preserves_first_match_result_target() -> None:
    agents = (
        Agent(PARENT, None, "/root", None, None, 0, START, START + 20_000, "completed", "root"),
        Agent(OTHER_PARENT, None, "/other", None, None, 0, START, START + 20_000, "completed", "other"),
        Agent(CHILD, PARENT, "/root/child", None, None, 1, START + 1_000, START + 12_000, "completed", "child"),
    )
    duplicate_turns = (
        Turn("duplicate", CHILD, START + 1_000, START + 2_000, "completed", None, None, None),
        Turn("duplicate", CHILD, START + 10_000, START + 11_000, "completed", None, None, None),
    )
    final = Event(
        event_id="final",
        thread_id=CHILD,
        turn_id="duplicate",
        timestamp_ms=START + 11_000,
        kind="assistant_message",
        role="assistant",
        phase="final_answer",
        text="Finished the delegated work.",
        content_availability="plaintext",
        encrypted_content=None,
        author=CHILD,
        recipient=PARENT,
        source_line=1,
    )
    edges = (
        Edge("spawn", "spawn", PARENT, CHILD, "spawn", START + 1_000, "first", "plaintext", None, 1),
        Edge("followup", "followup", OTHER_PARENT, CHILD, "followup", START + 10_000, "second", "plaintext", None, 2),
    )
    team = TeamData(
        team_slug="render-test",
        provider="codex",
        root_thread_id=PARENT,
        display_timezone="UTC",
        sources=(),
        agents=agents,
        turns=duplicate_turns,
        events=(final,),
        tool_calls=(),
        edges=edges,
    )

    names = {
        agent.thread_id: AgentNameResult(
            agent.thread_id,
            agent.agent_path.rsplit("/", 1)[-1] or "Coordinator",
            "test name",
            "test lifetime",
            "test-model",
            "test-prompt",
            "test-input",
            "2026-08-12T00:00:00Z",
        )
        for agent in agents
    }
    result_edges = _result_edge_objs(
        team,
        {agent.thread_id: agent for agent in agents},
        {},
        {},
        names,
    )

    turn_result = next(edge for edge in result_edges if edge["id"] == "turn-result-final")
    assert turn_result["target_id"] == PARENT
