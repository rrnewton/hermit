"""Synthetic, hermetic tests for the Codex team-timeline importer."""

from __future__ import annotations

from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path

from wrkviz.codex import load_codex_team
from wrkviz.phases import build_phases
from wrkviz.search_index import build_search_records


ROOT = "root-thread"
CHILD = "child-thread"


def _iso(seconds: float) -> str:
    return datetime.fromtimestamp(seconds, timezone.utc).isoformat().replace("+00:00", "Z")


def _record(timestamp: float, kind: str, payload: dict[str, object]) -> dict[str, object]:
    return {"timestamp": _iso(timestamp), "type": kind, "payload": payload}


def _write_rollout(
    path: Path, records: list[dict[str, object]], *, incomplete_tail: bytes = b""
) -> bytes:
    path.parent.mkdir(parents=True, exist_ok=True)
    complete = b"".join(
        json.dumps(item, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"
        for item in records
    )
    data = complete + incomplete_tail
    path.write_bytes(data)
    return data


def _fixture(sessions_root: Path) -> tuple[Path, Path, bytes, bytes]:
    root_path = sessions_root / "2026" / "08" / "05" / "rollout-root.jsonl"
    child_path = sessions_root / "2026" / "08" / "05" / "rollout-child.jsonl"

    root_meta: dict[str, object] = {
        "id": ROOT,
        "session_id": ROOT,
        "timestamp": _iso(1000),
        "cwd": "/work/project",
        "source": "cli",
        "thread_source": "user",
    }
    root_records = [
        _record(1000, "session_meta", root_meta),
        _record(
            1010,
            "event_msg",
            {"type": "task_started", "turn_id": "root-turn", "started_at": 1010},
        ),
        _record(
            1011,
            "response_item",
            {
                "type": "message",
                "id": "msg-user",
                "role": "user",
                "content": [{"type": "input_text", "text": "Build the thing"}],
            },
        ),
        _record(
            1011,
            "event_msg",
            {"type": "user_message", "message": "Build the thing"},
        ),
        # The event_msg copy is a UI duplicate and must not become transcript prose.
        _record(
            1012,
            "event_msg",
            {"type": "agent_message", "phase": "commentary", "message": "Working"},
        ),
        _record(
            1012,
            "response_item",
            {
                "type": "message",
                "id": "msg-assistant",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "Working"}],
            },
        ),
        _record(
            1013,
            "response_item",
            {
                "type": "custom_tool_call",
                "id": "ctc-exec",
                "call_id": "call-exec",
                "name": "exec",
                "status": "completed",
                "input": (
                    "await tools.exec_command({}); "
                    "await tools.exec_command({}); await tools.apply_patch('x')"
                ),
            },
        ),
        _record(
            1014,
            "response_item",
            {
                "type": "custom_tool_call_output",
                "id": "ctco-exec",
                "call_id": "call-exec",
                "output": "ok",
            },
        ),
        _record(
            1014.1,
            "response_item",
            {
                "type": "function_call",
                "id": "fc-wait",
                "call_id": "call-wait",
                "namespace": "collaboration",
                "name": "wait_agent",
                "arguments": '{"timeout_ms":10000}',
            },
        ),
        _record(
            1015,
            "response_item",
            {
                "type": "function_call_output",
                "id": "fco-wait",
                "call_id": "call-wait",
                "output": '{"status":"timeout"}',
            },
        ),
        _record(
            1015.1,
            "response_item",
            {
                "type": "function_call",
                "id": "fc-spawn",
                "call_id": "call-spawn",
                "namespace": "collaboration",
                "name": "spawn_agent",
                "arguments": json.dumps(
                    {
                        "task_name": "worker",
                        "fork_turns": "all",
                        "message": "gAAAA-encrypted-spawn",
                    }
                ),
            },
        ),
        _record(
            1015.2,
            "event_msg",
            {
                "type": "sub_agent_activity",
                "event_id": "call-spawn",
                "occurred_at_ms": 1_015_200,
                "agent_thread_id": CHILD,
                "agent_path": "/root/worker",
                "kind": "started",
            },
        ),
        _record(
            1015.3,
            "response_item",
            {
                "type": "function_call_output",
                "id": "fco-spawn",
                "call_id": "call-spawn",
                "output": '{"task_name":"/root/worker"}',
            },
        ),
        # This activity joins to a call stored in the child rollout, proving the
        # importer correlates communication globally rather than file-locally.
        _record(
            1025.2,
            "event_msg",
            {
                "type": "sub_agent_activity",
                "event_id": "call-child-message",
                "occurred_at_ms": 1_025_200,
                "agent_thread_id": CHILD,
                "agent_path": "/root/worker",
                "kind": "interacted",
            },
        ),
        _record(
            1025.3,
            "response_item",
            {
                "type": "agent_message",
                "id": "amsg-encrypted",
                "author": "/root/worker",
                "recipient": "/root",
                "content": [
                    {"type": "input_text", "text": "Message Type: MESSAGE\nPayload:\n"},
                    {"type": "encrypted_content", "encrypted_content": "gAAAA-mid"},
                ],
            },
        ),
        # Interrupting a subagent turn is not permanently terminal: the same thread may be
        # resumed later. The child's completion below must supersede this lifecycle event.
        _record(
            1030,
            "response_item",
            {
                "type": "function_call",
                "id": "fc-interrupt",
                "call_id": "call-interrupt",
                "namespace": "collaboration",
                "name": "interrupt_agent",
                "arguments": json.dumps({"target": "/root/worker"}),
            },
        ),
        _record(
            1030.1,
            "event_msg",
            {
                "type": "sub_agent_activity",
                "event_id": "call-interrupt",
                "occurred_at_ms": 1_030_100,
                "agent_thread_id": CHILD,
                "agent_path": "/root/worker",
                "kind": "interrupted",
            },
        ),
        _record(
            1040.1,
            "response_item",
            {
                "type": "agent_message",
                "id": "amsg-final",
                "author": "/root/worker",
                "recipient": "/root",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Message Type: FINAL_ANSWER\nPayload:\nFinished child work",
                    }
                ],
            },
        ),
        _record(
            1050,
            "event_msg",
            {
                "type": "task_complete",
                "turn_id": "root-turn",
                "started_at": 1010,
                "completed_at": 1050,
                "duration_ms": 40_000,
                "time_to_first_token_ms": 12,
                "last_agent_message": "Root done",
            },
        ),
    ]

    child_meta: dict[str, object] = {
        "id": CHILD,
        "session_id": ROOT,
        "forked_from_id": ROOT,
        "parent_thread_id": ROOT,
        "timestamp": _iso(1020),
        "cwd": "/work/project",
        "thread_source": "subagent",
        "agent_path": "/root/worker",
        "agent_nickname": "Ada",
        "source": {
            "subagent": {
                "thread_spawn": {
                    "parent_thread_id": ROOT,
                    "depth": 1,
                    "agent_path": "/root/worker",
                    "agent_nickname": "Ada",
                    "agent_role": "reviewer",
                }
            }
        },
    }
    child_records = [
        _record(1020, "session_meta", child_meta),
        # A fork_turns history prefix: these records are context, not child activity.
        _record(1020, "session_meta", root_meta),
        _record(
            1020,
            "event_msg",
            {"type": "task_started", "turn_id": "copied-turn", "started_at": 900},
        ),
        _record(
            1020,
            "event_msg",
            {"type": "user_message", "message": "copied prompt"},
        ),
        _record(
            1020,
            "response_item",
            {
                "type": "message",
                "id": "copied-assistant",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "copied prose"}],
            },
        ),
        _record(
            1020.1,
            "event_msg",
            {"type": "task_started", "turn_id": "child-turn", "started_at": 1020},
        ),
        _record(
            1020.2,
            "turn_context",
            {"turn_id": "child-turn", "timezone": "America/Los_Angeles"},
        ),
        _record(
            1020.3,
            "response_item",
            {
                "type": "agent_message",
                "id": "amsg-new-task",
                "author": "/root",
                "recipient": "/root/worker",
                "internal_chat_message_metadata_passthrough": {"turn_id": "child-turn"},
                "content": [
                    {"type": "input_text", "text": "Message Type: NEW_TASK\nPayload:\n"},
                    {"type": "encrypted_content", "encrypted_content": "gAAAA-task"},
                ],
            },
        ),
        _record(
            1025,
            "response_item",
            {
                "type": "function_call",
                "id": "fc-child-message",
                "call_id": "call-child-message",
                "namespace": "collaboration",
                "name": "send_message",
                "arguments": json.dumps({"target": "/root", "message": "gAAAA-mid"}),
            },
        ),
        _record(
            1025.1,
            "response_item",
            {
                "type": "function_call_output",
                "id": "fco-child-message",
                "call_id": "call-child-message",
                "output": "{}",
            },
        ),
        _record(
            1040,
            "response_item",
            {
                "type": "message",
                "id": "child-final",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": "Finished child work"}],
            },
        ),
        _record(
            1040.05,
            "event_msg",
            {
                "type": "task_complete",
                "turn_id": "child-turn",
                "started_at": 1020,
                "completed_at": 1040,
                "duration_ms": 20_000,
                "last_agent_message": "Finished child work",
            },
        ),
        # Codex rounds lifecycle payloads to seconds, so the next turn may start at exactly the
        # previous completion timestamp. The later start must win that tie.
        _record(
            1040.1,
            "event_msg",
            {"type": "task_started", "turn_id": "child-turn-2", "started_at": 1040},
        ),
    ]

    root_data = _write_rollout(root_path, root_records, incomplete_tail=b'{"partial"')
    child_data = _write_rollout(child_path, child_records)
    return root_path, child_path, root_data, child_data


def test_load_codex_team_canonicalizes_lineage_and_transcript(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    _fixture(sessions_root)

    team = load_codex_team(
        sessions_root, ROOT, "codex-project", "America/Los_Angeles"
    )

    assert team.team_slug == "codex-project"
    assert team.provider == "codex"
    assert team.root_thread_id == ROOT
    assert [agent.thread_id for agent in team.agents] == [ROOT, CHILD]

    child = next(agent for agent in team.agents if agent.thread_id == CHILD)
    assert child.parent_thread_id == ROOT
    assert child.agent_path == "/root/worker"
    assert child.nickname == "Ada"
    assert child.role == "reviewer"
    assert child.depth == 1
    assert child.status == "running"
    assert child.ended_at_ms is None

    assert {turn.turn_id for turn in team.turns} == {
        "root-turn",
        "child-turn",
        "child-turn-2",
    }
    assert "copied-turn" not in {turn.turn_id for turn in team.turns}
    child_turn = next(turn for turn in team.turns if turn.turn_id == "child-turn")
    assert child_turn.last_agent_message == "Finished child work"

    by_event_id = {event.event_id: event for event in team.events}
    assert by_event_id["msg-user"].kind == "user_prompt"
    assert by_event_id["msg-user"].text == "Build the thing"
    assert by_event_id["msg-user"].turn_id == "root-turn"
    assert by_event_id["msg-user"].author == "user"
    assert by_event_id["msg-user"].author_kind == "owner_human"
    assert by_event_id["msg-user"].ingress_kind == "codex"
    assert by_event_id["msg-user"].source_native_id == "msg-user"
    assert by_event_id["msg-user"].classification_version == "authorship-v1"
    assert by_event_id["msg-assistant"].text == "Working"
    assert by_event_id["child-final"].phase == "final_answer"
    assert by_event_id["amsg-new-task"].content_availability == "encrypted"
    assert by_event_id["amsg-new-task"].encrypted_content == "gAAAA-task"
    assert by_event_id["amsg-encrypted"].content_availability == "encrypted"
    assert by_event_id["amsg-final"].content_availability == "plaintext"
    assert "Finished child work" in (by_event_id["amsg-final"].text or "")
    assert "copied-assistant" not in by_event_id
    assert all(event.text != "copied prompt" for event in team.events)
    assert [event.text for event in team.events].count("Working") == 1

    transcript_text = "\n".join(phase.transcript_text for phase in build_phases(team))
    assert "Message Type: NEW_TASK" not in transcript_text
    assert "Message Type: MESSAGE\nPayload:\n" not in transcript_text
    assert "message body unavailable offline" in transcript_text
    assert "from /root to /root/worker" in transcript_text


def test_codex_search_uses_parent_child_routes_across_rollouts(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    _fixture(sessions_root)
    team = load_codex_team(sessions_root, ROOT, "codex-project", "UTC")

    records = build_search_records(team, frozenset((ROOT, CHILD)))
    by_event_id = {
        str(record["event_id"]): record
        for record in records
        if record["record_type"] != "tool"
    }

    instruction = by_event_id["amsg-new-task"]
    assert instruction["agent_id"] == CHILD
    assert instruction["record_type"] == "inter_agent_prompt"
    assert instruction["author_kind"] == "agent"
    assert instruction["prompt_ref"] == "message:codex-project::amsg-new-task"

    # Codex records delivered collaboration messages on the receiver's rollout. The
    # receiver thread therefore cannot determine direction or work ownership: resolve the
    # immutable parent/child route, attribute the return to its child author, and link it.
    for event_id in ("amsg-encrypted", "amsg-final"):
        returned = by_event_id[event_id]
        assert returned["agent_id"] == CHILD
        assert returned["record_type"] == "inter_agent_response"
        assert returned["author_kind"] == "agent"
        assert returned["prompt_author_kind"] == "agent"
        assert returned["prompt_ref"] == "message:codex-project::amsg-new-task"

    child_final = by_event_id["child-final"]
    assert child_final["record_type"] == "response"
    assert child_final["prompt_ref"] == "message:codex-project::amsg-new-task"


def test_load_codex_team_joins_tools_edges_and_source_snapshots(tmp_path: Path) -> None:
    sessions_root = tmp_path / "sessions"
    root_path, child_path, root_data, child_data = _fixture(sessions_root)

    team = load_codex_team(sessions_root, ROOT, "codex-project", "UTC")

    tools = {tool.call_id: tool for tool in team.tool_calls}
    assert tools["call-exec"].started_at_ms == 1_013_000
    assert tools["call-exec"].ended_at_ms == 1_014_000
    assert tools["call-exec"].output_text == "ok"
    assert tools["call-exec"].nested_tools == (
        ("apply_patch", 1),
        ("exec_command", 2),
    )
    assert tools["call-wait"].name == "wait_agent"
    assert tools["call-wait"].namespace == "collaboration"

    edges = {edge.edge_id: edge for edge in team.edges}
    spawn = edges["call-spawn"]
    assert (spawn.from_thread_id, spawn.to_thread_id, spawn.kind) == (
        ROOT,
        CHILD,
        "spawn",
    )
    assert spawn.content_availability == "encrypted"
    assert spawn.encrypted_content == "gAAAA-encrypted-spawn"
    message = edges["call-child-message"]
    assert (message.from_thread_id, message.to_thread_id, message.kind) == (
        CHILD,
        ROOT,
        "message",
    )
    assert message.content_availability == "encrypted"

    snapshots = {snapshot.thread_id: snapshot for snapshot in team.sources}
    root_snapshot = snapshots[ROOT]
    assert root_snapshot.path == root_path.relative_to(sessions_root).as_posix()
    assert root_snapshot.size_bytes == len(root_data)
    assert root_snapshot.complete_bytes == root_data.rfind(b"\n") + 1
    assert root_snapshot.complete_bytes < root_snapshot.size_bytes
    assert root_snapshot.sha256 == hashlib.sha256(root_data).hexdigest()
    child_snapshot = snapshots[CHILD]
    assert child_snapshot.path == child_path.relative_to(sessions_root).as_posix()
    assert child_snapshot.size_bytes == child_snapshot.complete_bytes == len(child_data)

    first = json.dumps(team.to_json_obj(), sort_keys=True, separators=(",", ":"))
    second = json.dumps(team.to_json_obj(), sort_keys=True, separators=(",", ":"))
    assert first == second
