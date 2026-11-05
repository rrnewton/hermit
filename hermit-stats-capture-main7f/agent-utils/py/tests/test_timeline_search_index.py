from __future__ import annotations

from dataclasses import replace

from wrkviz.model import Agent, Event, TeamData, ToolCall
from wrkviz.search_index import build_search_records


START = 1_800_000_000_000
ROOT = "root"


def _event(
    event_id: str,
    at_ms: int,
    kind: str,
    text: str,
    *,
    turn_id: str | None,
    author_kind: str | None,
) -> Event:
    return Event(
        event_id=event_id,
        thread_id=ROOT,
        turn_id=turn_id,
        timestamp_ms=at_ms,
        kind=kind,
        role="user" if kind == "user_prompt" else "assistant",
        phase=None,
        text=text,
        content_availability="plaintext",
        encrypted_content=None,
        author=None,
        recipient=None,
        source_line=at_ms - START,
        author_kind=author_kind,
    )


def _team() -> TeamData:
    return TeamData(
        team_slug="alpha",
        provider="codex",
        root_thread_id=ROOT,
        display_timezone="UTC",
        sources=(),
        agents=(
            Agent(
                ROOT,
                None,
                "/root",
                "Coordinator",
                None,
                0,
                START,
                START + 10_000,
                "completed",
                "session.jsonl",
            ),
        ),
        turns=(),
        events=(
            _event(
                "owner-prompt",
                START + 1_000,
                "user_prompt",
                "Measure backend maturity.",
                turn_id="turn-1",
                author_kind="owner_human",
            ),
            _event(
                "answer",
                START + 2_000,
                "assistant_message",
                "DBI is a solid B3 at 130/152.",
                turn_id="turn-1",
                author_kind="agent",
            ),
            _event(
                "unlinked",
                START + 3_000,
                "assistant_message",
                "A scheduled update has no prompt.",
                turn_id="turn-2",
                author_kind="agent",
            ),
        ),
        tool_calls=(
            ToolCall(
                call_id="call-1",
                item_id=None,
                thread_id=ROOT,
                turn_id="turn-1",
                name="exec_command",
                namespace="functions",
                started_at_ms=START + 1_500,
                ended_at_ms=START + 1_600,
                status="completed",
                input_text="secret raw command",
                output_text="secret raw output",
                nested_tools=(("bash", 2), ("git", 1)),
                source_line=3,
            ),
        ),
        edges=(),
    )


def test_search_records_link_responses_and_condense_tools() -> None:
    records = build_search_records(_team(), frozenset({ROOT}))

    assert [record["role"] for record in records] == [
        "user",
        "tool",
        "assistant",
        "assistant",
    ]
    prompt, tool, response, unlinked = records
    prompt_ref = "message:alpha::owner-prompt"
    assert prompt["ref"] == prompt_ref
    assert prompt["prompt_ref"] == prompt_ref
    assert prompt["prompt_author_kind"] == "owner_human"
    assert tool["text"] == "3 tools used: 2 bash, 1 git"
    assert "secret" not in str(tool)
    assert tool["prompt_ref"] == prompt_ref
    assert tool["prompt_at_ms"] == START + 1_000
    assert response["prompt_ref"] == prompt_ref
    assert response["prompt_at_ms"] == START + 1_000
    assert response["prompt_author_kind"] == "owner_human"
    assert unlinked["prompt_ref"] is None


def test_search_records_are_windowed_and_support_combined_agent_ids() -> None:
    records = build_search_records(
        _team(),
        frozenset({ROOT}),
        namespace_agents=True,
        start_ms=START + 1_500,
        end_ms=START + 2_500,
    )

    assert [record["ref"] for record in records] == [
        "tool:alpha::call-1",
        "message:alpha::answer",
    ]
    assert all(record["agent_id"] == "alpha::root" for record in records)
    assert records[1]["prompt_ref"] == "message:alpha::owner-prompt"


def test_search_records_reject_an_empty_window() -> None:
    try:
        build_search_records(
            _team(),
            frozenset({ROOT}),
            start_ms=START,
            end_ms=START,
        )
    except ValueError as error:
        assert str(error) == "search record start must be earlier than end"
    else:
        raise AssertionError("empty search window should fail")


def test_inter_agent_instruction_links_the_child_final_response() -> None:
    child = "child"
    instruction = Event(
        event_id="instruction",
        thread_id=child,
        turn_id="child-turn",
        timestamp_ms=START + 1_000,
        kind="inter_agent_message",
        role=None,
        phase="instruction",
        text="Audit the backend scorecard.",
        content_availability="plaintext",
        encrypted_content=None,
        author=ROOT,
        recipient=child,
        source_line=1,
        author_kind="agent",
    )
    final = Event(
        event_id="final",
        thread_id=child,
        turn_id="child-turn",
        timestamp_ms=START + 2_000,
        kind="inter_agent_message",
        role=None,
        phase="final_answer",
        text="The scorecard is source-grounded.",
        content_availability="plaintext",
        encrypted_content=None,
        author=child,
        recipient=ROOT,
        source_line=2,
        author_kind="agent",
    )
    team = TeamData(
        team_slug="alpha",
        provider="claude",
        root_thread_id=ROOT,
        display_timezone="UTC",
        sources=(),
        agents=(
            Agent(ROOT, None, "/root", None, None, 0, START, None, "active", "root"),
            Agent(
                child,
                ROOT,
                "/root/child",
                None,
                None,
                1,
                START,
                START + 2_000,
                "completed",
                "child",
            ),
        ),
        turns=(),
        events=(instruction, final),
        tool_calls=(),
        edges=(),
    )

    records = build_search_records(team, frozenset((ROOT, child)))

    assert [record["record_type"] for record in records] == [
        "inter_agent_prompt",
        "inter_agent_response",
    ]
    assert records[1]["prompt_ref"] == "message:alpha::instruction"
    assert records[1]["prompt_author_kind"] == "agent"
    assert records[1]["author"] == child
    assert records[1]["recipient"] == ROOT


def test_codex_continuation_routes_use_native_paths_and_receiver_threads() -> None:
    continuation_a = "continuation-a"
    child_a = "child-a"
    continuation_b = "continuation-b"
    child_b = "child-b"
    agents = (
        Agent(ROOT, None, "/root", None, None, 0, START, None, "active", "root"),
        Agent(
            continuation_a,
            ROOT,
            "/root/continuation-continuation-a",
            None,
            "coordinator",
            1,
            START + 1_000,
            START + 5_000,
            "completed",
            "continuation-a",
        ),
        Agent(
            child_a,
            continuation_a,
            "/root/continuation-continuation-a/worker",
            None,
            None,
            2,
            START + 2_000,
            START + 4_000,
            "completed",
            "child-a",
        ),
        Agent(
            continuation_b,
            continuation_a,
            "/root/continuation-continuation-b",
            None,
            "coordinator",
            2,
            START + 6_000,
            None,
            "active",
            "continuation-b",
        ),
        Agent(
            child_b,
            continuation_b,
            "/root/continuation-continuation-b/worker",
            None,
            None,
            3,
            START + 7_000,
            START + 9_000,
            "completed",
            "child-b",
        ),
    )

    def routed_event(
        event_id: str,
        thread_id: str,
        at_ms: int,
        text: str,
        author: str,
        recipient: str,
        source_line: int | None = None,
    ) -> Event:
        return Event(
            event_id=event_id,
            thread_id=thread_id,
            turn_id=f"turn-{thread_id}",
            timestamp_ms=at_ms,
            kind="inter_agent_message",
            role=None,
            phase=None,
            text=text,
            content_availability="plaintext",
            encrypted_content=None,
            author=author,
            recipient=recipient,
            source_line=at_ms - START if source_line is None else source_line,
            author_kind="agent",
        )

    raw_events = (
        routed_event(
            "instruction-a",
            child_a,
            START + 2_000,
            "Message Type: NEW_TASK\nPayload:\nAudit A",
            "/root",
            "/root/worker",
        ),
        _event(
            "owner-a",
            START + 3_000,
            "user_prompt",
            "Unrelated coordinator prompt A",
            turn_id=f"turn-{continuation_a}",
            author_kind="owner_human",
        ),
        routed_event(
            "return-a",
            continuation_a,
            START + 4_000,
            "Message Type: FINAL_ANSWER\nPayload:\nDone A",
            "/root/worker",
            "/root",
        ),
        routed_event(
            "instruction-b",
            child_b,
            START + 8_000,
            "Message Type: NEW_TASK\nPayload:\nAudit B",
            "/root",
            "/root/worker",
            999,
        ),
        routed_event(
            "return-b",
            continuation_b,
            START + 8_000,
            "Message Type: FINAL_ANSWER\nPayload:\nDone B",
            "/root/worker",
            "/root",
            1,
        ),
        routed_event(
            "later-instruction-b",
            child_b,
            START + 8_000,
            "Message Type: FOLLOWUP_TASK\nPayload:\nFuture work B",
            "/root",
            "/root/worker",
            0,
        ),
    )
    events = tuple(
        replace(event, thread_id=continuation_a)
        if event.event_id == "owner-a"
        else event
        for event in raw_events
    )
    team = TeamData(
        team_slug="codex-continuations",
        provider="codex",
        root_thread_id=ROOT,
        display_timezone="UTC",
        sources=(),
        agents=agents,
        turns=(),
        events=events,
        tool_calls=(),
        edges=(),
    )

    records = build_search_records(
        team, frozenset(agent.thread_id for agent in agents)
    )
    by_event = {str(record["event_id"]): record for record in records}

    assert by_event["return-a"]["record_type"] == "inter_agent_response"
    assert by_event["return-a"]["agent_id"] == child_a
    assert by_event["return-a"]["prompt_author_kind"] == "agent"
    assert by_event["return-a"]["prompt_ref"] == (
        "message:codex-continuations::instruction-a"
    )
    assert by_event["return-b"]["record_type"] == "inter_agent_response"
    assert by_event["return-b"]["agent_id"] == child_b
    assert by_event["return-b"]["prompt_ref"] is None


def test_same_timestamp_tool_links_only_to_a_preceding_source_prompt() -> None:
    prompt_a = replace(
        _event(
            "prompt-a",
            START + 1_000,
            "user_prompt",
            "First prompt",
            turn_id="turn-1",
            author_kind="owner_human",
        ),
        source_line=1,
    )
    prompt_b = replace(
        _event(
            "prompt-b",
            START + 1_000,
            "user_prompt",
            "Later prompt at the same timestamp",
            turn_id="turn-1",
            author_kind="owner_human",
        ),
        source_line=3,
    )
    tool = ToolCall(
        call_id="same-time-tool",
        item_id=None,
        thread_id=ROOT,
        turn_id="turn-1",
        name="exec_command",
        namespace="functions",
        started_at_ms=START + 1_000,
        ended_at_ms=START + 1_000,
        status="completed",
        input_text=None,
        output_text=None,
        nested_tools=(),
        source_line=2,
    )
    base = _team()
    team = TeamData(
        team_slug=base.team_slug,
        provider=base.provider,
        root_thread_id=base.root_thread_id,
        display_timezone=base.display_timezone,
        sources=base.sources,
        agents=base.agents,
        turns=(),
        events=(prompt_a, prompt_b),
        tool_calls=(tool,),
        edges=(),
    )

    records = build_search_records(team, frozenset({ROOT}))
    tool_record = next(record for record in records if record["role"] == "tool")

    assert tool_record["prompt_ref"] == "message:alpha::prompt-a"
