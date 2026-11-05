"""Append-only source-backup tests for the Codex timeline ingest."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import threading

import pytest

import wrkviz.codex as codex_module
import wrkviz.pipeline as pipeline_module
from wrkviz.build_store import team_build_root
from wrkviz.cli import main as timeline_main
from wrkviz.codex import (
    CodexParseError,
    CodexSnapshotResult,
    CodexSourceCopy,
    load_codex_team,
    snapshot_codex_lineage,
)
from wrkviz.pipeline import ingest_codex
from wrkviz.phases import build_phases
from wrkviz.pipeline import _phase_jobs
from wrkviz.summarize import _input_hash
from tests.timeline_snapshots import snapshot_root


ROOT = "root-thread"
CHILD = "child-thread"
CONTINUATION = "continuation-thread"
CONTINUATION_CHILD = "continuation-child"
CONTINUATION_TWO = "continuation-thread-two"


def _iso(seconds: float) -> str:
    return datetime.fromtimestamp(seconds, timezone.utc).isoformat().replace("+00:00", "Z")


def _line(timestamp: float, kind: str, payload: dict[str, object]) -> bytes:
    record: dict[str, object] = {
        "timestamp": _iso(timestamp),
        "type": kind,
        "payload": payload,
    }
    return json.dumps(record, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"


def _root_bytes(*, incomplete_tail: bytes = b"") -> bytes:
    records = (
        _line(
            1_000,
            "session_meta",
            {
                "id": ROOT,
                "session_id": ROOT,
                "timestamp": _iso(1_000),
                "cwd": "/work/project",
                "git": {
                    "repository_url": "git@github.com:example-org/dev-widget.git"
                },
                "source": "cli",
            },
        ),
        _line(
            1_001,
            "event_msg",
            {"type": "task_started", "turn_id": "root-turn", "started_at": 1_001},
        ),
        _line(
            1_002,
            "response_item",
            {
                "type": "message",
                "id": "root-answer",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Root done"}],
            },
        ),
        _line(
            1_002.1,
            "response_item",
            {
                "type": "function_call",
                "id": "shared-tool-item",
                "call_id": "shared-call",
                "name": "exec",
                "arguments": "{}",
            },
        ),
        _line(
            1_002.2,
            "response_item",
            {
                "type": "function_call_output",
                "call_id": "shared-call",
                "output": "canonical output",
            },
        ),
        _line(
            1_003,
            "event_msg",
            {
                "type": "task_complete",
                "turn_id": "root-turn",
                "started_at": 1_001,
                "completed_at": 1_003,
            },
        ),
    )
    return b"".join(records) + incomplete_tail


def _child_bytes() -> bytes:
    return b"".join(
        (
            _line(
                1_010,
                "session_meta",
                {
                    "id": CHILD,
                    "session_id": ROOT,
                    "parent_thread_id": ROOT,
                    "timestamp": _iso(1_010),
                    "agent_path": "/root/worker",
                    "source": {
                        "subagent": {
                            "thread_spawn": {
                                "parent_thread_id": ROOT,
                                "agent_path": "/root/worker",
                                "depth": 1,
                            }
                        }
                    },
                },
            ),
            _line(
                1_011,
                "event_msg",
                {"type": "task_started", "turn_id": "child-turn", "started_at": 1_011},
            ),
            _line(
                1_012,
                "response_item",
                {
                    "type": "message",
                    "id": "child-answer",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "Child done"}],
                },
            ),
            _line(
                1_013,
                "event_msg",
                {
                    "type": "task_complete",
                    "turn_id": "child-turn",
                    "started_at": 1_011,
                    "completed_at": 1_013,
                },
            ),
        )
    )


def _continuation_root_bytes() -> bytes:
    return b"".join(
        (
            _line(
                1_005.263,
                "session_meta",
                {
                    "id": CONTINUATION,
                    "session_id": CONTINUATION,
                    "timestamp": _iso(1_005.263),
                    "cwd": "/work/project",
                    "source": "cli",
                    "thread_source": "user",
                },
            ),
            _line(
                1_006,
                "event_msg",
                {"type": "task_started", "turn_id": "root-turn", "started_at": 1_006},
            ),
            _line(
                1_006.1,
                "response_item",
                {
                    "type": "message",
                    "id": "root-answer",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Continuation work"}],
                },
            ),
            _line(
                1_006.11,
                "response_item",
                {
                    "type": "function_call",
                    "id": "shared-tool-item",
                    "call_id": "shared-call",
                    "name": "exec",
                    "arguments": "{}",
                },
            ),
            _line(
                1_006.12,
                "response_item",
                {
                    "type": "function_call_output",
                    "call_id": "shared-call",
                    "output": "continuation output",
                },
            ),
            _line(
                1_006.2,
                "response_item",
                {
                    "type": "function_call",
                    "id": "spawn-item",
                    "call_id": "call-continuation-spawn",
                    "namespace": "collaboration",
                    "name": "spawn_agent",
                    "arguments": json.dumps(
                        {"task_name": "worker", "message": "continue child"}
                    ),
                },
            ),
            _line(
                1_006.3,
                "event_msg",
                {
                    "type": "sub_agent_activity",
                    "event_id": "call-continuation-spawn",
                    "occurred_at_ms": 1_006_300,
                    "agent_thread_id": CONTINUATION_CHILD,
                    "agent_path": "/root/worker",
                    "kind": "started",
                },
            ),
            _line(
                1_007,
                "event_msg",
                {
                    "type": "task_complete",
                    "turn_id": "root-turn",
                    "started_at": 1_006,
                    "completed_at": 1_007,
                },
            ),
        )
    )


def _continuation_child_bytes() -> bytes:
    return b"".join(
        (
            _line(
                1_006.5,
                "session_meta",
                {
                    "id": CONTINUATION_CHILD,
                    "session_id": CONTINUATION,
                    "parent_thread_id": CONTINUATION,
                    "timestamp": _iso(1_006.5),
                    "agent_path": "/root/worker",
                    "source": {
                        "subagent": {
                            "thread_spawn": {
                                "parent_thread_id": CONTINUATION,
                                "agent_path": "/root/worker",
                                "depth": 1,
                            }
                        }
                    },
                },
            ),
            _line(
                1_006.6,
                "event_msg",
                {"type": "task_started", "turn_id": "child-turn", "started_at": 1_006.6},
            ),
            _line(
                1_006.7,
                "response_item",
                {
                    "type": "message",
                    "id": "child-answer",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Continuation child"}],
                },
            ),
        )
    )


def _second_continuation_bytes() -> bytes:
    return b"".join(
        (
            _line(
                1_009.5,
                "session_meta",
                {
                    "id": CONTINUATION_TWO,
                    "session_id": CONTINUATION_TWO,
                    "timestamp": _iso(1_009.5),
                    "cwd": "/work/project",
                    "source": "cli",
                    "thread_source": "user",
                },
            ),
            _line(
                1_010,
                "event_msg",
                {"type": "task_started", "turn_id": "root-turn", "started_at": 1_010},
            ),
            _line(
                1_011,
                "response_item",
                {
                    "type": "message",
                    "id": "root-answer",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "Second continuation"}],
                },
            ),
            _line(
                1_012,
                "event_msg",
                {
                    "type": "task_complete",
                    "turn_id": "root-turn",
                    "started_at": 1_010,
                    "completed_at": 1_012,
                },
            ),
        )
    )


def _write_continuation_sources(sessions: Path) -> tuple[Path, Path]:
    root = sessions / "2026" / "08" / "05" / "rollout-continuation.jsonl"
    child = sessions / "2026" / "08" / "05" / "rollout-continuation-child.jsonl"
    root.write_bytes(_continuation_root_bytes())
    child.write_bytes(_continuation_child_bytes())
    return root, child


def _write_second_continuation_source(sessions: Path) -> Path:
    path = sessions / "2026" / "08" / "05" / "rollout-continuation-two.jsonl"
    path.write_bytes(_second_continuation_bytes())
    return path


def _origin(sessions: Path) -> Path:
    return sessions / "2026" / "08" / "05" / "rollout-root.jsonl"


def _snapshot(archive: Path) -> Path:
    return (
        snapshot_root(archive, "codex-test")
        / "2026"
        / "08"
        / "05"
        / "rollout-root.jsonl"
    )


def _manifest(archive: Path) -> Path:
    return team_build_root(archive, "codex-test") / "raw" / "source-manifest.json"


def _first_ingest(tmp_path: Path, data: bytes | None = None) -> tuple[Path, Path, Path]:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes() if data is None else data)
    ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")
    return sessions, archive, origin


def test_ingest_copies_complete_lines_then_parses_the_backup(tmp_path: Path) -> None:
    incomplete = b'{"timestamp":"unfinished"'
    complete = _root_bytes()
    sessions, archive, _ = _first_ingest(tmp_path, complete + incomplete)

    copied = _snapshot(archive)
    assert copied.read_bytes() == complete
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    source = manifest["sources"][0]
    assert source["copied_bytes"] == len(complete)
    assert source["sha256"] == hashlib.sha256(complete).hexdigest()
    assert source["line_count"] == complete.count(b"\n")
    assert "/teams/*/source_snapshots/" in (
        archive / ".gitignore"
    ).read_text(encoding="utf-8").splitlines()

    # The normalized parser has everything it needs in the copy, independent of the live root.
    parsed = load_codex_team(
        snapshot_root(archive, "codex-test"),
        ROOT,
        "codex-test",
        "UTC",
    )
    assert [event.text for event in parsed.events] == ["Root done"]
    assert manifest["source_root"] == str(sessions.resolve())
    identity = json.loads(
        (
            team_build_root(archive, "codex-test")
            / "raw"
            / "site-identity.json"
        ).read_text(encoding="utf-8")
    )
    assert identity["projects"] == [
        {
            "label": "dev-widget",
            "repository_url": "https://github.com/example-org/dev-widget",
            "primary": True,
            "source": "session_metadata",
        }
    ]
    assert identity["display_timezone"] == "UTC"
    assert identity["display_timezone_source"] == "api"

    _, repeat = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")
    assert repeat.files_changed == 0


def test_append_replaces_snapshot_with_longer_complete_prefix(tmp_path: Path) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    before = _snapshot(archive).read_bytes()
    appended = _line(
        1_004,
        "event_msg",
        {"type": "thread_goal_updated", "goal": {"status": "complete"}},
    )
    origin.write_bytes(before + appended + b'{"partial":')

    _, report = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    expected = before + appended
    assert _snapshot(archive).read_bytes() == expected
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    assert manifest["sources"][0]["copied_bytes"] == len(expected)
    assert manifest["sources"][0]["sha256"] == hashlib.sha256(expected).hexdigest()
    assert report.source_bytes == len(expected)


def test_real_ingest_writes_no_per_thread_message_projection(tmp_path: Path) -> None:
    _, archive, _ = _first_ingest(tmp_path)

    # Nothing anywhere in the archive, not just under the one team: this is the check that catches
    # a second writer being added later under a different team root.
    assert [path for path in archive.rglob("messages") if path.is_dir()] == []
    raw = team_build_root(archive, "codex-test") / "raw"
    assert sorted(path.name for path in raw.iterdir()) == [
        "artifacts.json",
        "site-identity.json",
        "source-manifest.json",
        "source-snapshot.json",
        "team.json",
    ]


def test_real_ingest_retires_a_legacy_projection_and_says_so_on_stderr(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    legacy = team_build_root(archive, "codex-test") / "raw" / "messages"
    legacy.mkdir()
    payload = json.dumps({"agent": {}, "turns": [], "messages": []}) + "\n"
    for thread_id in (ROOT, CHILD):
        (legacy / f"{thread_id}.json").write_text(payload, encoding="utf-8")
    freed = 2 * len(payload.encode("utf-8"))
    capsys.readouterr()

    status = timeline_main(
        (
            "ingest",
            "--sessions-root",
            str(sessions),
            "--root-session",
            ROOT,
            "--team",
            "codex-test",
            "--output",
            str(archive),
            "--timezone",
            "UTC",
        )
    )

    assert status == 0
    assert not legacy.exists()
    captured = capsys.readouterr()
    # On stderr, not stdout: a scheduled run redirects stdout, and this is the operator's only
    # live notice that a few thousand tracked files just left their archive.
    assert "retired" in captured.err and "raw/messages" in captured.err
    assert "removed 2 retired" in captured.err
    assert "raw/messages" not in captured.out

    run_path = sorted((archive / "runs").glob("*.json"))[-1]
    run = json.loads(run_path.read_text(encoding="utf-8"))
    assert run["ingest"]["retired_message_projections"] == 2
    assert run["ingest"]["retired_message_projection_bytes"] == freed

    # Second run: nothing left to sweep, and nothing said about it.
    capsys.readouterr()
    assert (
        timeline_main(
            (
                "ingest",
                "--sessions-root",
                str(sessions),
                "--root-session",
                ROOT,
                "--team",
                "codex-test",
                "--output",
                str(archive),
                "--timezone",
                "UTC",
            )
        )
        == 0
    )
    assert "raw/messages" not in capsys.readouterr().err


def test_disappeared_source_fails_and_preserves_snapshot(tmp_path: Path) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    copied_before = _snapshot(archive).read_bytes()
    manifest_before = _manifest(archive).read_bytes()
    origin.unlink()

    with pytest.raises(CodexParseError, match="previously observed rollout disappeared"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert _snapshot(archive).read_bytes() == copied_before
    assert _manifest(archive).read_bytes() == manifest_before


def test_truncated_source_fails_and_preserves_snapshot(tmp_path: Path) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    copied_before = _snapshot(archive).read_bytes()
    manifest_before = _manifest(archive).read_bytes()
    final_line = copied_before.rsplit(b"\n", 2)[1] + b"\n"
    origin.write_bytes(copied_before[: -len(final_line)])

    with pytest.raises(CodexParseError, match="newline-complete prefix shrank"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert _snapshot(archive).read_bytes() == copied_before
    assert _manifest(archive).read_bytes() == manifest_before


def test_rewritten_same_length_prefix_fails_and_preserves_snapshot(tmp_path: Path) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    copied_before = _snapshot(archive).read_bytes()
    manifest_before = _manifest(archive).read_bytes()
    rewritten = copied_before.replace(b"Root done", b"Root gone")
    assert len(rewritten) == len(copied_before)
    origin.write_bytes(rewritten)

    with pytest.raises(CodexParseError, match="existing prefix was rewritten"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert _snapshot(archive).read_bytes() == copied_before
    assert _manifest(archive).read_bytes() == manifest_before


def test_new_lineage_child_is_added_without_recopying_root(tmp_path: Path) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    root_snapshot = _snapshot(archive)
    root_mtime = root_snapshot.stat().st_mtime_ns
    child = sessions / "2026" / "08" / "05" / "rollout-child.jsonl"
    child.write_bytes(_child_bytes())

    team, _ = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert {agent.thread_id for agent in team.agents} == {ROOT, CHILD}
    assert root_snapshot.stat().st_mtime_ns == root_mtime
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    assert {source["thread_id"] for source in manifest["sources"]} == {ROOT, CHILD}


def test_explicit_continuation_preserves_old_ids_hashes_and_is_idempotent(
    tmp_path: Path,
) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    before_team, before_report = ingest_codex(
        archive, sessions, ROOT, "codex-test", "UTC"
    )
    assert before_report.files_changed == 0
    before_snapshot = _snapshot(archive)
    before_snapshot_bytes = before_snapshot.read_bytes()
    before_snapshot_mtime = before_snapshot.stat().st_mtime_ns
    before_agents = {agent.thread_id: agent for agent in before_team.agents}
    before_phases = build_phases(before_team)
    before_jobs = {
        job.key: job for job in _phase_jobs(before_team, before_phases, (), 16_000)
    }
    before_hashes = {
        key: _input_hash(job, "heuristic", "fixture")
        for key, job in before_jobs.items()
    }
    _write_continuation_sources(sessions)

    after, _ = ingest_codex(
        archive,
        sessions,
        ROOT,
        "codex-test",
        "UTC",
        continuation_thread_ids=(CONTINUATION,),
    )
    manifest_bytes = _manifest(archive).read_bytes()
    manifest = json.loads(manifest_bytes)
    agents = {agent.thread_id: agent for agent in after.agents}
    edges = {edge.edge_id: edge for edge in after.edges}

    assert after.root_thread_id == ROOT
    assert before_snapshot.read_bytes() == before_snapshot_bytes
    assert before_snapshot.stat().st_mtime_ns == before_snapshot_mtime
    assert {source["thread_id"] for source in manifest["sources"]} == {
        ROOT,
        CONTINUATION,
        CONTINUATION_CHILD,
    }
    assert manifest["schema_version"] == 1
    assert manifest["continuation_sessions"] == [
        {
            "predecessor_thread_id": ROOT,
            "thread_id": CONTINUATION,
            "predecessor_source_path": "2026/08/05/rollout-root.jsonl",
            "predecessor_source_line": 6,
            "predecessor_at_ms": 1_003_000,
            "source_path": "2026/08/05/rollout-continuation.jsonl",
            "started_at_ms": 1_005_263,
            "gap_ms": 2_263,
        }
    ]
    assert agents[ROOT] == before_agents[ROOT]
    assert agents[CONTINUATION].parent_thread_id == ROOT
    assert agents[CONTINUATION].depth == 1
    assert agents[CONTINUATION].role == "coordinator"
    assert agents[CONTINUATION].agent_path == (
        f"/root/continuation-{CONTINUATION}"
    )
    assert agents[CONTINUATION_CHILD].parent_thread_id == CONTINUATION
    assert agents[CONTINUATION_CHILD].depth == 2
    assert agents[CONTINUATION_CHILD].agent_path == (
        f"/root/continuation-{CONTINUATION}/worker"
    )
    continuation_edge = edges[f"codex-continuation-{CONTINUATION}"]
    assert (
        continuation_edge.from_thread_id,
        continuation_edge.to_thread_id,
        continuation_edge.kind,
        continuation_edge.timestamp_ms,
    ) == (ROOT, CONTINUATION, "continuation", 1_003_000)
    assert continuation_edge.source_line == 6
    assert "2263 ms later" in (continuation_edge.message_text or "")
    assert "root-answer" in {event.event_id for event in after.events}
    scoped_answer = codex_module._scoped_id(
        CONTINUATION, ROOT, "root-answer"
    )
    assert (
        scoped_answer
        in {event.event_id for event in after.events}
    )
    assert "root-turn" in {turn.turn_id for turn in after.turns}
    scoped_turn = codex_module._scoped_id(CONTINUATION, ROOT, "root-turn")
    assert (
        scoped_turn
        in {turn.turn_id for turn in after.turns}
    )
    assert {"shared-call", codex_module._scoped_id(CONTINUATION, ROOT, "shared-call")} <= {
        tool.call_id for tool in after.tool_calls
    }
    assert codex_module._scoped_id("a-b", ROOT, "c") != codex_module._scoped_id(
        "a", ROOT, "b-c"
    )

    after_phases = build_phases(after)
    after_jobs = {
        job.key: job
        for job in _phase_jobs(after, after_phases, (), 16_000)
        if job.end_ms < 1_005_263
    }
    assert set(after_jobs) == set(before_jobs)
    assert after_jobs == before_jobs
    assert {
        key: _input_hash(job, "heuristic", "fixture")
        for key, job in after_jobs.items()
    } == before_hashes

    repeated, repeat_report = ingest_codex(
        archive, sessions, ROOT, "codex-test", "UTC"
    )
    assert repeat_report.files_changed == 0
    assert repeated == after
    assert _manifest(archive).read_bytes() == manifest_bytes


def test_unconfigured_continuation_is_not_auto_linked(tmp_path: Path) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    _write_continuation_sources(sessions)

    team, _ = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))

    assert {agent.thread_id for agent in team.agents} == {ROOT}
    assert "continuation_sessions" not in manifest
    assert {source["thread_id"] for source in manifest["sources"]} == {ROOT}


def test_cli_accepts_ordered_repeatable_continuation_sessions(tmp_path: Path) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    _write_continuation_sources(sessions)
    _write_second_continuation_source(sessions)

    status = timeline_main(
        (
            "ingest",
            "--sessions-root",
            str(sessions),
            "--root-session",
            ROOT,
            "--continuation-session",
            CONTINUATION,
            "--continuation-session",
            CONTINUATION_TWO,
            "--team",
            "codex-test",
            "--output",
            str(archive),
            "--timezone",
            "UTC",
        )
    )

    assert status == 0
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    assert [
        link["thread_id"] for link in manifest["continuation_sessions"]
    ] == [CONTINUATION, CONTINUATION_TWO]
    assert manifest["continuation_sessions"][1]["predecessor_thread_id"] == CONTINUATION
    raw = json.loads(
        (team_build_root(archive, "codex-test") / "raw" / "team.json").read_text(
            encoding="utf-8"
        )
    )
    agents = {agent["thread_id"]: agent for agent in raw["agents"]}
    assert agents[CONTINUATION_TWO]["parent_thread_id"] == CONTINUATION
    assert agents[CONTINUATION_TWO]["depth"] == 2


def test_refresh_records_and_reuses_explicit_continuation(tmp_path: Path) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    _write_continuation_sources(sessions)
    common = (
        "--sessions-root",
        str(sessions),
        "--root-session",
        ROOT,
        "--team",
        "codex-test",
        "--output",
        str(archive),
        "--timezone",
        "UTC",
        "--backend",
        "heuristic",
        "--model",
        "deterministic-local",
    )

    first_status = timeline_main(
        ("refresh", *common, "--continuation-session", CONTINUATION)
    )
    first_manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    second_status = timeline_main(("refresh", *common))
    second_manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))

    assert first_status == second_status == 0
    assert first_manifest["continuation_sessions"] == second_manifest[
        "continuation_sessions"
    ]
    assert second_manifest["continuation_sessions"][0]["thread_id"] == CONTINUATION


def test_recorded_continuation_prefix_can_only_be_extended_in_order(
    tmp_path: Path,
) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    _write_continuation_sources(sessions)
    ingest_codex(
        archive,
        sessions,
        ROOT,
        "codex-test",
        "UTC",
        continuation_thread_ids=(CONTINUATION,),
    )
    one_link_manifest = _manifest(archive).read_bytes()
    _write_second_continuation_source(sessions)

    with pytest.raises(CodexParseError, match="recorded ordered prefix"):
        ingest_codex(
            archive,
            sessions,
            ROOT,
            "codex-test",
            "UTC",
            continuation_thread_ids=(CONTINUATION_TWO,),
        )
    assert _manifest(archive).read_bytes() == one_link_manifest

    extended, _ = ingest_codex(
        archive,
        sessions,
        ROOT,
        "codex-test",
        "UTC",
        continuation_thread_ids=(CONTINUATION, CONTINUATION_TWO),
    )
    assert [
        edge.to_thread_id for edge in extended.edges if edge.kind == "continuation"
    ] == [CONTINUATION, CONTINUATION_TWO]

    with pytest.raises(CodexParseError, match="recorded ordered prefix"):
        ingest_codex(
            archive,
            sessions,
            ROOT,
            "codex-test",
            "UTC",
            continuation_thread_ids=(CONTINUATION,),
        )
    repeated, report = ingest_codex(
        archive, sessions, ROOT, "codex-test", "UTC"
    )
    assert repeated == extended
    assert report.files_changed == 0


def test_cross_lineage_raw_thread_collision_fails_closed(tmp_path: Path) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    root = _origin(sessions)
    root.parent.mkdir(parents=True)
    root.write_bytes(_root_bytes())
    (root.parent / "rollout-child.jsonl").write_bytes(_child_bytes())
    (root.parent / "rollout-continuation.jsonl").write_bytes(
        _continuation_root_bytes()
    )
    collision = _continuation_child_bytes().replace(
        CONTINUATION_CHILD.encode("utf-8"), CHILD.encode("utf-8")
    )
    (root.parent / "rollout-continuation-collision.jsonl").write_bytes(collision)

    with pytest.raises(CodexParseError, match="occurs in multiple configured lineages"):
        ingest_codex(
            archive,
            sessions,
            ROOT,
            "codex-test",
            "UTC",
            continuation_thread_ids=(CONTINUATION,),
        )

    assert not _manifest(archive).exists()


def test_encoded_continuation_tool_id_cannot_overwrite_canonical_tool(
    tmp_path: Path,
) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    root = _origin(sessions)
    root.parent.mkdir(parents=True)
    encoded = codex_module._scoped_id(CONTINUATION, ROOT, "shared-call")
    root.write_bytes(
        _root_bytes().replace(b'"shared-call"', json.dumps(encoded).encode("utf-8"))
    )
    _write_continuation_sources(sessions)

    with pytest.raises(CodexParseError, match="collides across Codex lineages"):
        ingest_codex(
            archive,
            sessions,
            ROOT,
            "codex-test",
            "UTC",
            continuation_thread_ids=(CONTINUATION,),
        )

    assert not _manifest(archive).exists()


def test_continuation_path_component_is_injective_and_path_safe() -> None:
    slash_root = codex_module._continuation_path("a/b")
    percent_root = codex_module._continuation_path("a%2Fb")

    assert slash_root == "/root/continuation-a%2Fb"
    assert percent_root == "/root/continuation-a%252Fb"
    assert slash_root != percent_root
    assert slash_root != codex_module._continuation_path("a") + "/b"


def test_collaboration_edge_cannot_overwrite_continuation_edge(tmp_path: Path) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    child = origin.with_name("rollout-child.jsonl")
    child.write_bytes(_child_bytes())
    baseline, _ = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")
    raw_team = team_build_root(archive, "codex-test") / "raw" / "team.json"
    raw_before = raw_team.read_bytes()
    edge_id = f"codex-continuation-{CONTINUATION}"
    origin.write_bytes(
        origin.read_bytes()
        + _line(
            1_004.1,
            "response_item",
            {
                "type": "function_call",
                "id": "collision-item",
                "call_id": edge_id,
                "namespace": "collaboration",
                "name": "send_message",
                "arguments": json.dumps({"target": CHILD, "message": "collision"}),
            },
        )
        + _line(
            1_004.2,
            "event_msg",
            {
                "type": "sub_agent_activity",
                "event_id": edge_id,
                "occurred_at_ms": 1_004_200,
                "agent_thread_id": CHILD,
                "kind": "message",
            },
        )
    )
    _write_continuation_sources(sessions)

    with pytest.raises(CodexParseError, match="collides with a continuation edge"):
        ingest_codex(
            archive,
            sessions,
            ROOT,
            "codex-test",
            "UTC",
            continuation_thread_ids=(CONTINUATION,),
        )

    assert raw_team.read_bytes() == raw_before
    assert baseline.root_thread_id == ROOT


def test_persisted_continuation_boundary_is_frozen_when_predecessor_advances(
    tmp_path: Path,
) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    _write_continuation_sources(sessions)
    ingest_codex(
        archive,
        sessions,
        ROOT,
        "codex-test",
        "UTC",
        continuation_thread_ids=(CONTINUATION,),
    )
    initial = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    initial_link = initial["continuation_sessions"][0]
    # This appended predecessor record would be a closer boundary if the evidence
    # were recomputed instead of reused from the durable manifest.
    origin.write_bytes(
        origin.read_bytes()
        + _line(1_005.1, "ignored_record", {"type": "ignored_after_link"})
    )

    _, report = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert report.files_changed > 0
    updated = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    assert updated["continuation_sessions"][0] == initial_link


def test_continuation_source_truncation_fails_without_replacing_archive(
    tmp_path: Path,
) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    _, continuation_child = _write_continuation_sources(sessions)
    before, _ = ingest_codex(
        archive,
        sessions,
        ROOT,
        "codex-test",
        "UTC",
        continuation_thread_ids=(CONTINUATION,),
    )
    manifest_before = _manifest(archive).read_bytes()
    raw_team = team_build_root(archive, "codex-test") / "raw" / "team.json"
    team_before = raw_team.read_bytes()
    child_bytes = continuation_child.read_bytes()
    continuation_child.write_bytes(child_bytes.rsplit(b"\n", 2)[0] + b"\n")

    with pytest.raises(CodexParseError, match="newline-complete prefix shrank"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert _manifest(archive).read_bytes() == manifest_before
    assert raw_team.read_bytes() == team_before
    assert before.root_thread_id == ROOT


def test_orphan_snapshot_is_excluded_from_manifest_bound_parse(tmp_path: Path) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    orphan = snapshot_root(archive, "codex-test") / "orphan" / "rollout-child.jsonl"
    orphan.parent.mkdir(parents=True)
    orphan.write_bytes(_child_bytes())

    team, _ = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert [agent.thread_id for agent in team.agents] == [ROOT]
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    assert [source["thread_id"] for source in manifest["sources"]] == [ROOT]
    assert orphan.is_file()  # ignored, not silently adopted or destructively removed


@pytest.mark.parametrize("symlink_level", ["root", "intermediate"])
def test_snapshot_directory_symlink_escape_is_rejected(
    tmp_path: Path, symlink_level: str
) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    archive.mkdir()
    (archive / ".wrkviz.json").write_text(
        '{"schema_version":1,"tool":"wrkviz"}\n', encoding="utf-8"
    )
    snapshots = snapshot_root(archive, "codex-test")
    outside = tmp_path / "outside"
    outside.mkdir()
    if symlink_level == "root":
        snapshots.parent.mkdir(parents=True)
        snapshots.symlink_to(outside, target_is_directory=True)
    else:
        snapshots.mkdir(parents=True)
        (snapshots / "2026").symlink_to(outside, target_is_directory=True)

    with pytest.raises(CodexParseError, match="symlink or non-directory"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert not (outside / "08" / "05" / "rollout-root.jsonl").exists()


def test_same_content_snapshot_target_symlink_is_rejected(tmp_path: Path) -> None:
    sessions, archive, _ = _first_ingest(tmp_path)
    snapshot = _snapshot(archive)
    outside = tmp_path / "outside-rollout.jsonl"
    outside.write_bytes(snapshot.read_bytes())
    snapshot.unlink()
    snapshot.symlink_to(outside)

    with pytest.raises(CodexParseError, match="target must not be a symlink"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert outside.read_bytes() == _root_bytes()


def _manifest_sources(archive: Path) -> tuple[CodexSourceCopy, ...]:
    manifest: object = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise AssertionError("source manifest is not an object")
    raw_sources = manifest.get("sources")
    if not isinstance(raw_sources, list):
        raise AssertionError("source manifest lacks a sources array")
    result: list[CodexSourceCopy] = []
    for index, raw_source in enumerate(raw_sources):
        if not isinstance(raw_source, dict):
            raise AssertionError("source record is not an object")
        result.append(
            CodexSourceCopy.from_json_obj(
                {str(key): value for key, value in raw_source.items()},
                f"sources[{index}]",
            )
        )
    return tuple(result)


def test_retry_recovers_copy_that_advanced_before_manifest(tmp_path: Path) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    old_manifest = _manifest(archive).read_bytes()
    appended = _line(
        1_004,
        "event_msg",
        {"type": "thread_goal_updated", "goal": {"status": "complete"}},
    )
    expected = _root_bytes() + appended
    origin.write_bytes(expected)

    copied_only = snapshot_codex_lineage(
        sessions,
        ROOT,
        snapshot_root(archive, "codex-test"),
        _manifest_sources(archive),
        "2026-08-05T12:00:00Z",
    )
    assert copied_only.files_changed == 1
    assert _snapshot(archive).read_bytes() == expected
    assert _manifest(archive).read_bytes() == old_manifest

    _, report = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert _snapshot(archive).read_bytes() == expected
    assert report.source_bytes == len(expected)
    assert json.loads(_manifest(archive).read_text(encoding="utf-8"))["sources"][0][
        "copied_bytes"
    ] == len(expected)


def test_pipeline_does_not_reopen_live_original_after_copy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    real_snapshot = snapshot_codex_lineage

    def snapshot_then_remove(
        sessions_root: Path,
        root_thread_id: str,
        snapshot_root: Path,
        previous_sources: Sequence[CodexSourceCopy],
        updated_at: str,
    ) -> CodexSnapshotResult:
        result = real_snapshot(
            sessions_root,
            root_thread_id,
            snapshot_root,
            previous_sources,
            updated_at,
        )
        origin.unlink()
        return result

    monkeypatch.setattr(pipeline_module, "snapshot_codex_lineage", snapshot_then_remove)

    team, report = ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert report.sources == 1
    assert [event.text for event in team.events] == ["Root done"]
    assert not origin.exists()


def test_archive_lock_prevents_interleaved_writer_regression(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sessions, archive, origin = _first_ingest(tmp_path)
    medium_line = _line(
        1_004,
        "event_msg",
        {"type": "thread_goal_updated", "goal": {"status": "working"}},
    )
    newest_line = _line(
        1_005,
        "event_msg",
        {"type": "thread_goal_updated", "goal": {"status": "complete"}},
    )
    medium = _root_bytes() + medium_line
    newest = medium + newest_line
    origin.write_bytes(medium)
    older_at_write = threading.Event()
    release_older = threading.Event()
    newer_done = threading.Event()
    errors: list[BaseException] = []
    real_write = codex_module._write_snapshot_file

    def delayed_write(
        snapshot_root: Path,
        source_path: str,
        data: bytes,
        expected: bytes | None,
    ) -> bool:
        if threading.current_thread().name == "older-ingest":
            older_at_write.set()
            if not release_older.wait(timeout=5):
                raise AssertionError("timed out releasing older ingest")
        return real_write(snapshot_root, source_path, data, expected)

    monkeypatch.setattr(codex_module, "_write_snapshot_file", delayed_write)

    def run_ingest(done: threading.Event | None = None) -> None:
        try:
            ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")
        except BaseException as error:
            errors.append(error)
        finally:
            if done is not None:
                done.set()

    older = threading.Thread(target=run_ingest, name="older-ingest")
    older.start()
    assert older_at_write.wait(timeout=5)
    origin.write_bytes(newest)
    newer = threading.Thread(target=run_ingest, args=(newer_done,), name="newer-ingest")
    newer.start()
    assert not newer_done.wait(timeout=0.1)
    release_older.set()
    older.join(timeout=5)
    newer.join(timeout=5)

    assert not older.is_alive()
    assert not newer.is_alive()
    assert errors == []
    assert _snapshot(archive).read_bytes() == newest
    manifest = json.loads(_manifest(archive).read_text(encoding="utf-8"))
    assert manifest["sources"][0]["copied_bytes"] == len(newest)


def test_metadata_replacement_between_discovery_and_read_is_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    real_discover = codex_module._discover_codex_lineage

    def discover_then_replace(
        sessions_root: Path,
        root_thread_id: str,
        *,
        exclude_root: Path | None = None,
    ) -> tuple[tuple[Path, Mapping[str, object]], ...]:
        result = real_discover(
            sessions_root,
            root_thread_id,
            exclude_root=exclude_root,
        )
        origin.write_bytes(_root_bytes().replace(b'"cwd":"/work/project"', b'"cwd":"/work/changed"'))
        return result

    monkeypatch.setattr(codex_module, "_discover_codex_lineage", discover_then_replace)

    with pytest.raises(CodexParseError, match="metadata changed between discovery"):
        ingest_codex(archive, sessions, ROOT, "codex-test", "UTC")

    assert not _snapshot(archive).exists()
