"""Provider tests for Claude Code transcript ingestion."""

from __future__ import annotations

import json
from pathlib import Path
import shutil

import pytest

from wrkviz.build_store import team_build_root
from wrkviz.claude import (
    ClaudeParseError,
    ClaudeSourceCopy,
    discover_claude_sources,
    load_claude_team,
    snapshot_claude_lineage,
)
from wrkviz.cli import main
from wrkviz.phases import aggregate_stats, build_phases
from wrkviz.pipeline import (
    build_archive,
    ingest_claude,
    summarize_archive,
)
from wrkviz.window import parse_date_window
from tests.timeline_snapshots import snapshot_root
from tests.timeline_projection import schema_1_timeline_text


SESSION_ID = "11111111-1111-4111-8111-111111111111"
FIXTURE_ROOT = Path(__file__).parent / "fixtures" / "claude"
DAY_TWO_MS = 1_767_312_000_000
DAY_THREE_MS = 1_767_398_400_000


def _session(root: Path = FIXTURE_ROOT) -> Path:
    return root / f"{SESSION_ID}.jsonl"


def test_discovers_root_nested_agent_logs_and_metadata() -> None:
    relative = [
        path.relative_to(FIXTURE_ROOT).as_posix()
        for path in discover_claude_sources(_session())
    ]

    assert relative == [
        f"{SESSION_ID}.jsonl",
        f"{SESSION_ID}/subagents/agent-a-child.jsonl",
        f"{SESSION_ID}/subagents/agent-a-child.meta.json",
        f"{SESSION_ID}/subagents/agent-a-nested.jsonl",
        f"{SESSION_ID}/subagents/agent-a-nested.meta.json",
    ]


def test_loads_provider_neutral_nested_team_and_joins_tools() -> None:
    team = load_claude_team(
        _session(), "claude-fixture", "UTC"
    )

    assert team.provider == "claude"
    assert team.root_thread_id == SESSION_ID
    assert len(team.sources) == 5
    assert len(team.agents) == 3
    agents = {agent.thread_id: agent for agent in team.agents}
    assert agents[SESSION_ID].agent_path == "/root"
    assert agents["a-child"].parent_thread_id == SESSION_ID
    assert agents["a-child"].depth == 1
    assert agents["a-child"].role == "researcher"
    assert agents["a-child"].agent_path == "/root/audit_release_evidence"
    assert agents["a-nested"].parent_thread_id == "a-child"
    assert agents["a-nested"].depth == 2
    assert agents["a-nested"].agent_path.endswith("/check_test_receipts")

    tools = {tool.call_id: tool for tool in team.tool_calls}
    assert set(tools) == {
        "tool-spawn-child",
        "tool-bash-root",
        "tool-message-child",
        "tool-spawn-nested",
        "tool-read-nested",
    }
    assert tools["tool-bash-root"].status == "completed"
    assert tools["tool-bash-root"].output_text == "checks passed"
    assert tools["tool-read-nested"].ended_at_ms is not None

    assert [(edge.kind, edge.from_thread_id, edge.to_thread_id) for edge in team.edges] == [
        ("spawn", SESSION_ID, "a-child"),
        ("spawn", "a-child", "a-nested"),
        ("followup", SESSION_ID, "a-child"),
    ]
    assert len(team.turns) == 4
    assert all(event.turn_id is not None for event in team.events)
    assert "private reasoning" not in "\n".join(
        event.text or "" for event in team.events
    )
    stats = aggregate_stats(build_phases(team))
    assert stats.user_prompts == 2
    assert stats.agent_responses == 2
    assert stats.inter_agent_messages == 5
    child_instruction = next(
        event
        for event in team.events
        if event.thread_id == "a-child" and event.phase == "instruction"
    )
    assert child_instruction.kind == "inter_agent_message"
    assert child_instruction.author == SESSION_ID
    assert child_instruction.recipient == "a-child"
    child_result = next(
        event
        for event in team.events
        if event.thread_id == "a-child" and event.phase == "final_answer"
    )
    assert child_result.kind == "inter_agent_message"
    assert child_result.author == "a-child"
    assert child_result.recipient == SESSION_ID
    root_prompt = next(event for event in team.events if event.event_id == "root-user-1:user")
    assert root_prompt.author_kind == "unknown"
    assert root_prompt.ingress_kind == "claude_legacy"
    assert root_prompt.source_native_id == "root-user-1"


def test_classifies_claude_human_and_synthetic_inputs_from_native_metadata(
    tmp_path: Path,
) -> None:
    session = tmp_path / f"{SESSION_ID}.jsonl"
    records: list[dict[str, object]] = [
        {
            "type": "attachment",
            "uuid": "queued-native",
            "sessionId": SESSION_ID,
            "timestamp": "2026-01-01T10:00:00.000Z",
            "attachment": {
                "type": "queued_command",
                "prompt": "Human input while Claude was busy.",
                "commandMode": "prompt",
                "origin": {"kind": "human"},
            },
        },
        {
            "type": "user",
            "uuid": "typed-native",
            "sessionId": SESSION_ID,
            "timestamp": "2026-01-01T10:01:00.000Z",
            "origin": {"kind": "human"},
            "message": {"role": "user", "content": "Direct human input."},
        },
        {
            "type": "user",
            "uuid": "notification-native",
            "sessionId": SESSION_ID,
            "timestamp": "2026-01-01T10:02:00.000Z",
            "origin": {"kind": "task-notification"},
            "message": {"role": "user", "content": "Synthetic task completion."},
        },
        {
            "type": "user",
            "uuid": "slash-native",
            "sessionId": SESSION_ID,
            "timestamp": "2026-01-01T10:03:00.000Z",
            "message": {
                "role": "user",
                "content": "<command-name>/goal</command-name><command-args>ship it</command-args>",
            },
        },
        {
            "type": "user",
            "uuid": "compact-native",
            "sessionId": SESSION_ID,
            "timestamp": "2026-01-01T10:04:00.000Z",
            "isCompactSummary": True,
            "message": {"role": "user", "content": "Compacted conversation summary."},
        },
        {
            "type": "user",
            "uuid": "meta-native",
            "sessionId": SESSION_ID,
            "timestamp": "2026-01-01T10:05:00.000Z",
            "isMeta": True,
            "message": {"role": "user", "content": "Expanded command instructions."},
        },
    ]
    session.write_text(
        "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records),
        encoding="utf-8",
    )

    team = load_claude_team(session, "claude-authorship", "UTC")
    events = {event.source_native_id: event for event in team.events}

    assert events["queued-native"].kind == "user_prompt"
    assert events["queued-native"].author_kind == "owner_human"
    assert events["queued-native"].ingress_kind == "claude_queued"
    assert events["typed-native"].kind == "user_prompt"
    assert events["typed-native"].author_kind == "owner_human"
    assert events["typed-native"].ingress_kind == "claude_typed"
    assert events["notification-native"].kind == "system_input"
    assert events["notification-native"].author_kind == "system"
    assert events["slash-native"].kind == "user_prompt"
    assert events["slash-native"].ingress_kind == "claude_slash"
    assert events["compact-native"].kind == "system_input"
    assert events["meta-native"].kind == "system_input"
    assert all(event.classification_version == "authorship-v1" for event in events.values())
    assert all(event.turn_id is not None for event in events.values())
    assert len({event.turn_id for event in events.values()}) == len(events)


def test_half_open_window_clips_activity_and_retains_ancestors() -> None:
    team = load_claude_team(
        _session(),
        "claude-window",
        "UTC",
        start_ms=DAY_TWO_MS,
        end_ms=DAY_THREE_MS,
    )

    assert {agent.thread_id for agent in team.agents} == {
        SESSION_ID,
        "a-child",
        "a-nested",
    }
    assert all(DAY_TWO_MS <= event.timestamp_ms < DAY_THREE_MS for event in team.events)
    assert all(tool.call_id != "tool-bash-root" for tool in team.tool_calls)
    root_spawn = next(edge for edge in team.edges if edge.to_thread_id == "a-child")
    assert root_spawn.timestamp_ms == DAY_TWO_MS
    assert all(agent.started_at_ms >= DAY_TWO_MS for agent in team.agents)
    assert any("overnight audit" in (event.text or "") for event in team.events)


def test_trailing_incomplete_json_is_ignored(tmp_path: Path) -> None:
    copied = tmp_path / "claude"
    shutil.copytree(FIXTURE_ROOT, copied)
    path = _session(copied)
    with path.open("ab") as handle:
        handle.write(b'{"type":"assistant"')

    team = load_claude_team(path, "claude-tail", "UTC")

    assert len(team.agents) == 3
    assert team.sources[0].complete_bytes < team.sources[0].size_bytes


def test_send_message_deduplicates_matching_received_child_prompt(
    tmp_path: Path,
) -> None:
    copied = tmp_path / "claude"
    shutil.copytree(FIXTURE_ROOT, copied)
    child = (
        copied
        / SESSION_ID
        / "subagents"
        / "agent-a-child.jsonl"
    )
    received = {
        "type": "user",
        "uuid": "child-user-resumed",
        "parentUuid": "child-final-1",
        "sessionId": SESSION_ID,
        "isSidechain": True,
        "agentId": "a-child",
        "timestamp": "2026-01-02T10:00:02.500Z",
        "message": {
            "role": "user",
            "content": "Please send the final evidence.",
        },
    }
    with child.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(received, separators=(",", ":")) + "\n")

    team = load_claude_team(_session(copied), "claude-deduplicated", "UTC")
    routed = [
        event
        for event in team.events
        if event.kind == "inter_agent_message"
        and event.author == SESSION_ID
        and event.recipient == "a-child"
        and event.text == "Please send the final evidence."
    ]

    assert len(routed) == 1
    assert routed[0].thread_id == "a-child"
    assert aggregate_stats(build_phases(team)).inter_agent_messages == 5


def test_complete_malformed_record_is_rejected(tmp_path: Path) -> None:
    copied = tmp_path / "claude"
    shutil.copytree(FIXTURE_ROOT, copied)
    path = _session(copied)
    with path.open("ab") as handle:
        handle.write(b'{"type": bad}\n')

    with pytest.raises(ClaudeParseError, match="invalid JSON"):
        load_claude_team(path, "claude-bad", "UTC")


def test_window_without_activity_is_rejected() -> None:
    with pytest.raises(ClaudeParseError, match="contains no transcript activity"):
        load_claude_team(
            _session(),
            "claude-empty",
            "UTC",
            start_ms=DAY_THREE_MS,
            end_ms=DAY_THREE_MS + 86_400_000,
        )


def test_snapshot_is_idempotent_and_accepts_only_monotonic_append(
    tmp_path: Path,
) -> None:
    live = tmp_path / "live"
    shutil.copytree(FIXTURE_ROOT, live)
    snapshot = tmp_path / "snapshot"

    first = snapshot_claude_lineage(
        _session(live), snapshot, (), "2026-01-03T00:00:00Z"
    )
    second = snapshot_claude_lineage(
        _session(live), snapshot, first.sources, "2026-01-03T01:00:00Z"
    )

    assert first.files_changed == 5
    assert second.files_changed == 0
    assert second.sources == first.sources

    root = _session(live)
    with root.open("ab") as handle:
        handle.write(
            b'{"type":"system","timestamp":"2026-01-02T10:02:00.000Z",'
            b'"sessionId":"11111111-1111-4111-8111-111111111111"}\n'
        )
    advanced = snapshot_claude_lineage(
        root, snapshot, second.sources, "2026-01-03T02:00:00Z"
    )
    assert advanced.files_changed == 1
    root_copy = next(
        source for source in advanced.sources if source.source_path == root.name
    )
    assert root_copy.copied_bytes > first.sources[0].copied_bytes

    root.write_bytes(root.read_bytes()[:100])
    with pytest.raises(ClaudeParseError, match="truncated or rewritten"):
        snapshot_claude_lineage(
            root, snapshot, advanced.sources, "2026-01-03T03:00:00Z"
        )


def test_source_copy_json_validation_rejects_identity_change(tmp_path: Path) -> None:
    live = tmp_path / "live"
    shutil.copytree(FIXTURE_ROOT, live)
    first = snapshot_claude_lineage(
        _session(live), tmp_path / "snapshot", (), "2026-01-03T00:00:00Z"
    )
    value = first.sources[0].to_json_obj()

    assert ClaudeSourceCopy.from_json_obj(value, "source") == first.sources[0]
    value["thread_id"] = "../different"
    with pytest.raises(ClaudeParseError, match="unsafe thread id"):
        ClaudeSourceCopy.from_json_obj(value, "source")


def test_pipeline_snapshots_claude_and_reuses_unchanged_archive(tmp_path: Path) -> None:
    live = tmp_path / "live"
    shutil.copytree(FIXTURE_ROOT, live)
    archive = tmp_path / "archive"
    window = parse_date_window("2026-01-02", "2026-01-03", "UTC")

    team, first = ingest_claude(
        archive,
        _session(live),
        "claude-fixture",
        "UTC",
        window,
    )
    _, second = ingest_claude(
        archive,
        _session(live),
        "claude-fixture",
        "UTC",
        window,
    )

    assert team.provider == "claude"
    assert team.window_start_ms == DAY_TWO_MS
    assert team.window_end_ms == DAY_THREE_MS
    assert any(event.timestamp_ms < DAY_TWO_MS for event in team.events)
    assert first.sources == 5
    assert second.files_changed == 0
    assert (archive / ".gitignore").read_text(encoding="utf-8").splitlines() == [
        "/.wrkviz.lock",
        "/teams/*/source_snapshots/",
        "/teams/*/payloads/",
        "/.wrkviz-trash/",
    ]
    manifest = json.loads(
        (
            team_build_root(archive, "claude-fixture")
            / "raw"
            / "source-manifest.json"
        ).read_text(encoding="utf-8")
    )
    assert manifest["provider"] == "claude"
    assert manifest["root_thread_id"] == SESSION_ID
    assert manifest["date_window"]["start_date"] == "2026-01-02"
    assert (snapshot_root(archive, "claude-fixture") / f"{SESSION_ID}.jsonl").is_file()
    summarize_archive(archive, "claude-fixture", "heuristic", "fixture")
    build_archive(archive, "claude-fixture")
    timeline = json.loads(schema_1_timeline_text(archive))
    result_edges = [edge for edge in timeline["edges"] if edge["kind"] == "result"]
    assert {
        (edge["source_id"], edge["target_id"])
        for edge in result_edges
    } == {("a-child", SESSION_ID), ("a-nested", "a-child")}
    event_kinds = [event["kind"] for event in timeline["events"]]
    assert event_kinds.count("user_prompt") == 1
    assert event_kinds.count("assistant_message") == 1
    assert event_kinds.count("inter_agent_message") == 4


def test_cli_exposes_bounded_claude_ingest(tmp_path: Path) -> None:
    archive = tmp_path / "archive"

    status = main(
        (
            "ingest-claude",
            "--session-file",
            str(_session()),
            "--team",
            "claude-fixture",
            "--output",
            str(archive),
            "--timezone",
            "UTC",
            "--start-date",
            "2026-01-02",
            "--end-date",
            "2026-01-03",
        )
    )

    assert status == 0
    raw = json.loads(
        (
            team_build_root(archive, "claude-fixture") / "raw" / "team.json"
        ).read_text(encoding="utf-8")
    )
    assert raw["provider"] == "claude"
    assert raw["window_start_ms"] == DAY_TWO_MS
    assert raw["window_end_ms"] == DAY_THREE_MS
