"""Provider tests for the Claude Code prompt-history importer."""

from __future__ import annotations

import json
from pathlib import Path
import shutil
import pytest

from wrkviz.build_store import team_build_root
from wrkviz.claude import ClaudeSourceCopy
from wrkviz.claude_history import (
    ClaudeHistoryParseError,
    UNATTRIBUTED_SESSION,
    load_claude_history_team,
    snapshot_claude_history,
)
from wrkviz.cli import main
from wrkviz.pipeline import (
    build_archive,
    extract_transcripts_archive,
    ingest_claude,
    ingest_claude_history,
)
from wrkviz.project_config import (
    ClaudeHistoryProjectSource,
    ingest_project,
    load_project_ingest_config,
)
from tests.timeline_projection import schema_1_timeline_text
from tests.timeline_snapshots import snapshot_root


CLAUDE_FIXTURE_ROOT = Path(__file__).parent / "fixtures" / "claude"
CLAUDE_SESSION_ID = "11111111-1111-4111-8111-111111111111"
SESSION_A = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
SESSION_B = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
DAY_MS = 86_400_000
T0 = 1_767_225_600_000  # 2026-01-01T00:00:00Z


def _line(
    display: str,
    timestamp: int,
    project: str,
    session: str | None,
    pasted: dict[str, object] | None = None,
) -> str:
    record: dict[str, object] = {
        "display": display,
        "pastedContents": pasted or {},
        "timestamp": timestamp,
        "project": project,
    }
    if session is not None:
        record["sessionId"] = session
    return json.dumps(record)


def _history_lines() -> list[str]:
    return [
        _line("oldest, before session ids", T0 - 30 * DAY_MS, "/home/o/work/widget", None),
        _line("unrelated project", T0, "/home/o/work/other", SESSION_B),
        _line(
            "fix the widget build",
            T0 + 1_000,
            "/home/o/work/widget",
            SESSION_A,
            {"1": {"id": 1, "type": "text", "content": "error: line 3\n"}},
        ),
        _line("now the tests", T0 + 60_000, "/home/o/work/widget", SESSION_A),
        _line("in a worktree", T0 + DAY_MS, "/home/o/worktrees/widget-x", SESSION_B),
        _line("covered by a full transcript", T0 + 2 * DAY_MS, "/home/o/work/widget", CLAUDE_SESSION_ID),
    ]


def _write_history(path: Path, lines: list[str] | None = None, *, trailing: str = "") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines or _history_lines()) + "\n" + trailing, encoding="utf-8")
    return path


def test_loads_prompt_only_team_selected_by_project_pattern(tmp_path: Path) -> None:
    history = _write_history(tmp_path / "history.jsonl")

    team, selection = load_claude_history_team(
        history, "claude-history-h1", "UTC", project_pattern="widget"
    )

    assert team.provider == "claude-history"
    assert selection.matched_prompts == 5
    assert selection.unmatched_prompts == 1
    assert selection.covered_prompts == 0
    assert [agent.thread_id for agent in team.agents] == [
        UNATTRIBUTED_SESSION,
        SESSION_A,
        SESSION_B,
        CLAUDE_SESSION_ID,
    ]
    assert team.root_thread_id == UNATTRIBUTED_SESSION
    assert all(agent.role == "coordinator" and agent.depth == 0 for agent in team.agents)
    assert {event.kind for event in team.events} == {"user_prompt"}
    assert {event.ingress_kind for event in team.events} == {"claude_history"}
    assert {event.author_kind for event in team.events} == {"owner_human"}
    fix = next(event for event in team.events if (event.text or "").startswith("fix the widget"))
    assert fix.text == "fix the widget build\n\n[Pasted text #1]\nerror: line 3"
    assert fix.thread_id == SESSION_A
    assert fix.source_line == 3
    assert fix.source_native_id == "history.jsonl:3"
    session_a = next(agent for agent in team.agents if agent.thread_id == SESSION_A)
    assert session_a.started_at_ms == T0 + 1_000
    assert session_a.ended_at_ms == T0 + 60_000 + 1_000
    assert team.tool_calls == () and team.edges == ()
    assert len(team.turns) == 5
    assert team.sources[0].path == "history.jsonl"
    assert team.sources[0].line_count == 6


def test_covered_sessions_are_left_out_and_counted(tmp_path: Path) -> None:
    history = _write_history(tmp_path / "history.jsonl")

    team, selection = load_claude_history_team(
        history,
        "claude-history-h1",
        "UTC",
        project_pattern="widget",
        covered_session_ids=[CLAUDE_SESSION_ID, SESSION_B],
    )

    assert selection.matched_prompts == 3
    assert selection.covered_prompts == 2
    assert selection.covered_sessions == 2
    assert CLAUDE_SESSION_ID not in {agent.thread_id for agent in team.agents}
    assert SESSION_B not in {agent.thread_id for agent in team.agents}


def test_no_match_and_bad_pattern_are_refused(tmp_path: Path) -> None:
    history = _write_history(tmp_path / "history.jsonl")

    with pytest.raises(ClaudeHistoryParseError, match="no history prompt matched"):
        load_claude_history_team(
            history, "claude-history-h1", "UTC", project_pattern="nowhere"
        )
    with pytest.raises(ClaudeHistoryParseError, match="invalid project pattern"):
        load_claude_history_team(
            history, "claude-history-h1", "UTC", project_pattern="("
        )
    with pytest.raises(ClaudeHistoryParseError, match="must not be empty"):
        load_claude_history_team(
            history, "claude-history-h1", "UTC", project_pattern=""
        )


def test_malformed_history_lines_are_refused(tmp_path: Path) -> None:
    history = _write_history(
        tmp_path / "history.jsonl",
        [json.dumps({"display": "x", "timestamp": "soon", "project": "/p"})],
    )

    with pytest.raises(ClaudeHistoryParseError, match="timestamp"):
        load_claude_history_team(history, "t", "UTC", project_pattern="/p")


def test_snapshot_copies_complete_prefix_and_refuses_rewrite(tmp_path: Path) -> None:
    lines = _history_lines()
    history = _write_history(tmp_path / "live" / "history.jsonl", lines[:3], trailing='{"partial":')
    store = tmp_path / "store"

    first = snapshot_claude_history(history, store, None, "2026-01-05T00:00:00Z")
    copied = (store / "history.jsonl").read_text(encoding="utf-8")

    assert copied == "\n".join(lines[:3]) + "\n"
    assert first.files_changed == 1
    assert first.source.line_count == 3
    assert first.source.thread_id == "history"

    _write_history(history, lines)
    second = snapshot_claude_history(history, store, first.source, "2026-01-06T00:00:00Z")
    assert second.source.line_count == 6
    assert second.source.updated_at == "2026-01-06T00:00:00Z"

    third = snapshot_claude_history(history, store, second.source, "2026-01-07T00:00:00Z")
    assert third.files_changed == 0
    assert third.source.updated_at == "2026-01-06T00:00:00Z"

    _write_history(history, lines[1:])
    with pytest.raises(ClaudeHistoryParseError, match="truncated or rewritten"):
        snapshot_claude_history(history, store, third.source, "2026-01-08T00:00:00Z")

    renamed = ClaudeSourceCopy.from_json_obj(
        {**third.source.to_json_obj(), "source_path": "other.jsonl", "snapshot_path": "other.jsonl"},
        "test",
    )
    with pytest.raises(ClaudeHistoryParseError, match="is not 'history.jsonl'"):
        snapshot_claude_history(history, store, renamed, "2026-01-08T00:00:00Z")


def test_pipeline_ingests_builds_and_projects_history_prompts(tmp_path: Path) -> None:
    history = _write_history(tmp_path / "live" / "history.jsonl")
    archive = tmp_path / "archive"

    team, first = ingest_claude_history(
        archive, history, "claude-history-h1", "UTC", "widget", [CLAUDE_SESSION_ID]
    )
    _, second = ingest_claude_history(
        archive, history, "claude-history-h1", "UTC", "widget", [CLAUDE_SESSION_ID]
    )

    assert team.provider == "claude-history"
    assert first.sources == 1
    assert first.agents == 3
    assert first.events == 4
    assert first.history_unmatched_prompts == 1
    assert first.history_covered_prompts == 1
    assert first.history_covered_sessions == 1
    assert second.files_changed == 0
    assert (snapshot_root(archive, "claude-history-h1") / "history.jsonl").is_file()
    manifest = json.loads(
        (team_build_root(archive, "claude-history-h1") / "raw" / "source-manifest.json")
        .read_text(encoding="utf-8")
    )
    assert manifest["provider"] == "claude-history"
    assert manifest["project_pattern"] == "widget"
    assert manifest["covered_session_ids"] == [CLAUDE_SESSION_ID]
    assert first.to_json_obj()["history_covered_prompts"] == 1

    with pytest.raises(ClaudeHistoryParseError, match="register a different team slug"):
        ingest_claude_history(archive, history, "claude-history-h1", "UTC", "other")

    build_archive(archive, "claude-history-h1")
    timeline = json.loads(schema_1_timeline_text(archive))
    assert [event["kind"] for event in timeline["events"]].count("user_prompt") == 4
    assert len(timeline["agents"]) == 3

    report = extract_transcripts_archive(archive)
    prompts = [
        json.loads(line)
        for line in (archive / "extracted" / "transcripts" / "prompts.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    assert report.prompts == 4
    assert [prompt["text"] for prompt in prompts] == [
        "oldest, before session ids",
        "fix the widget build\n\n[Pasted text #1]\nerror: line 3",
        "now the tests",
        "in a worktree",
    ]
    assert {prompt["ingress_kind"] for prompt in prompts} == {"claude_history"}
    assert {prompt["author_kind"] for prompt in prompts} == {"owner_human"}


def test_cli_exposes_history_ingest_and_reports_exclusions(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    history = _write_history(tmp_path / "live" / "history.jsonl")
    archive = tmp_path / "archive"

    status = main(
        (
            "ingest-claude-history",
            "--history-file",
            str(history),
            "--project-pattern",
            "widget",
            "--covered-session",
            CLAUDE_SESSION_ID,
            "--team",
            "claude-history-h1",
            "--output",
            str(archive),
            "--timezone",
            "UTC",
        )
    )

    captured = capsys.readouterr()
    assert status == 0
    assert "ingest: 3 agents, 4 messages/events" in captured.out
    assert "left out 1 prompt(s) typed outside the project pattern; 1 prompt(s) from 1 session(s)" in (
        captured.err
    )
    raw = json.loads(
        (team_build_root(archive, "claude-history-h1") / "raw" / "team.json")
        .read_text(encoding="utf-8")
    )
    assert raw["provider"] == "claude-history"


def test_project_config_derives_covered_sessions_from_claude_teams(tmp_path: Path) -> None:
    live = tmp_path / "raw" / "host01"
    shutil.copytree(CLAUDE_FIXTURE_ROOT, live / "projects")
    history = _write_history(live / "history.jsonl")
    config_path = tmp_path / "configs" / "widget.json"
    config_path.parent.mkdir(parents=True)
    config_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "output": "../summary/widget",
                "timezone": "UTC",
                "teams": [
                    {
                        "slug": "claude-team",
                        "provider": "claude",
                        "source": {
                            "session_file": f"../raw/host01/projects/{CLAUDE_SESSION_ID}.jsonl"
                        },
                    },
                    {
                        "slug": "claude-history-host01",
                        "provider": "claude-history",
                        "source": {
                            "history_file": "../raw/host01/history.jsonl",
                            "project_pattern": "widget",
                        },
                    },
                ],
            }
        ),
        encoding="utf-8",
    )

    config = load_project_ingest_config(config_path)
    source = config.teams[1].source
    assert isinstance(source, ClaudeHistoryProjectSource)
    assert source.history_file == history.resolve()
    assert source.project_pattern == "widget"
    assert source.covered_sessions == "config"
    assert config.claude_session_ids == (CLAUDE_SESSION_ID,)

    report = ingest_project(config)
    assert report.failures == ()
    by_slug = {team.team_slug: team.ingest for team in report.teams}
    assert by_slug["claude-history-host01"].history_covered_prompts == 1
    assert by_slug["claude-history-host01"].history_covered_sessions == 1
    assert by_slug["claude-history-host01"].events == 4
    # Ingesting the history team alone derives the same covered set from the whole manifest.
    alone = ingest_project(config, ["claude-history-host01"])
    assert alone.failures == ()
    assert alone.teams[0].ingest.history_covered_prompts == 1

    # The projection carries the full-transcript prompt once, from its own team, and the
    # history prompts from theirs.
    prompts = [
        json.loads(line)
        for line in (config.output / "extracted" / "transcripts" / "prompts.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    by_team: dict[str, int] = {}
    for prompt in prompts:
        by_team[prompt["team_slug"]] = by_team.get(prompt["team_slug"], 0) + 1
    assert by_team["claude-history-host01"] == 4
    assert "covered by a full transcript" not in {prompt["text"] for prompt in prompts}


@pytest.mark.parametrize(
    ("source", "match"),
    (
        ({"history_file": "h.jsonl"}, "missing=\\['project_pattern'\\]"),
        ({"history_file": "h.jsonl", "project_pattern": "("}, "project_pattern: invalid"),
        (
            {"history_file": "h.jsonl", "project_pattern": "x", "covered_sessions": "some"},
            "expected config or none",
        ),
        (
            {"history_file": "h.jsonl", "project_pattern": "x", "extra": 1},
            "unknown=\\['extra'\\]",
        ),
    ),
)
def test_project_config_rejects_history_source_drift(
    tmp_path: Path, source: dict[str, object], match: str
) -> None:
    config_path = tmp_path / "widget.json"
    config_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "output": "../summary/widget",
                "teams": [
                    {"slug": "claude-history-h1", "provider": "claude-history", "source": source}
                ],
            }
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match=match):
        load_project_ingest_config(config_path)


def test_history_team_sits_beside_a_full_claude_team_in_one_archive(tmp_path: Path) -> None:
    live = tmp_path / "live"
    shutil.copytree(CLAUDE_FIXTURE_ROOT, live)
    history = _write_history(tmp_path / "history.jsonl")
    archive = tmp_path / "archive"

    ingest_claude(archive, live / f"{CLAUDE_SESSION_ID}.jsonl", "claude-team", "UTC")
    ingest_claude_history(
        archive, history, "claude-history-h1", "UTC", "widget", [CLAUDE_SESSION_ID]
    )
    report = extract_transcripts_archive(archive)

    assert report.prompts >= 5
    assert (team_build_root(archive, "claude-team") / "raw" / "team.json").is_file()
    assert (team_build_root(archive, "claude-history-h1") / "raw" / "team.json").is_file()
