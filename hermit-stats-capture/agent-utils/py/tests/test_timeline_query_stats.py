"""Read-only content accounting for generated timeline archives."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue, as_object, narrow_json
from wrkviz.cli import main as timeline_main
from wrkviz.model import Agent, Event, TeamData
from wrkviz.query import TextTotals
from wrkviz.transcript_export import export_transcripts


def _event(
    event_id: str,
    turn_id: str,
    timestamp_ms: int,
    kind: str,
    text: str,
) -> Event:
    prompt = kind == "user_prompt"
    return Event(
        event_id=event_id,
        thread_id="root",
        turn_id=turn_id,
        timestamp_ms=timestamp_ms,
        kind=kind,
        role="user" if prompt else "assistant",
        phase=None,
        text=text,
        content_availability="plain",
        encrypted_content=None,
        author="user" if prompt else "root",
        recipient="root" if prompt else None,
        source_line=timestamp_ms,
        ingress_kind="typed" if prompt else "agent",
        author_kind="owner_human" if prompt else "agent",
        source_native_id=event_id,
        classification_version="authorship-v1",
    )


def _archive(tmp_path: Path) -> Path:
    team = TeamData(
        team_slug="alpha",
        provider="codex",
        root_thread_id="root",
        display_timezone="UTC",
        sources=(),
        agents=(
            Agent(
                thread_id="root",
                parent_thread_id=None,
                agent_path="/root",
                nickname=None,
                role="coordinator",
                depth=0,
                started_at_ms=50,
                ended_at_ms=900,
                status="completed",
                source_path="root.jsonl",
            ),
        ),
        turns=(),
        events=(
            _event("prompt", "turn-linked", 100, "user_prompt", "Owner α words"),
            _event(
                "linked",
                "turn-linked",
                150,
                "assistant_message",
                "Linked résumé",
            ),
            _event(
                "unlinked",
                "turn-unlinked",
                200,
                "assistant_message",
                "Unlinked response here",
            ),
        ),
        tool_calls=(),
        edges=(),
    )
    export_transcripts(tmp_path, (team,))

    technical = "# Daily\n\nTechnical rollup.\n"
    technical_path = (
        tmp_path / "teams" / "alpha" / "summaries" / "daily" / "technical.md"
    )
    technical_path.parent.mkdir(parents=True)
    technical_path.write_text(technical, encoding="utf-8")
    timeline: dict[str, JsonValue] = {
        "schema_version": 1,
        "teams": [{"slug": "alpha"}],
        "agents": [
            {
                "id": "root",
                "team": "alpha",
                "start_ms": 50,
                "end_ms": 300,
                "summary_available": True,
                "lifetime_summary": "Agent lifetime summary",
            },
            {
                "id": "worker",
                "team": "alpha",
                "start_ms": 400,
                "end_ms": 900,
                "summary_available": False,
            },
        ],
        "phases": [
            {
                "id": "phase-one",
                "agent_id": "root",
                "team": "alpha",
                "start_ms": 100,
                "end_ms": 250,
                "summary_available": True,
                "phrase": "Phase phrase",
                "paragraph": "Phase paragraph with β.",
            },
            {
                "id": "phase-two",
                "agent_id": "worker",
                "team": "alpha",
                "start_ms": 500,
                "end_ms": 700,
                "summary_available": False,
            },
        ],
        "rollups": [
            {
                "team": "alpha",
                "kind": "daily",
                "start_ms": 0,
                "end_ms": 1_000,
                "technical_summary_available": True,
                "technical_path": "teams/alpha/summaries/daily/technical.md",
                "plain_language_summary_available": False,
            }
        ],
        "project_overview": {
            "summary_available": True,
            "text": "Project overview",
        },
    }
    data = tmp_path / "data"
    data.mkdir()
    (data / "timeline.json").write_text(
        json.dumps(timeline, ensure_ascii=False), encoding="utf-8"
    )
    return tmp_path


def _run_json(args: tuple[str, ...]) -> dict[str, JsonValue]:
    completed = subprocess.run(args, check=True, capture_output=True, text=True)
    raw: object = json.loads(completed.stdout)
    return as_object(narrow_json(raw), "stats output")


def test_stats_reports_text_totals_and_sparse_summary_coverage(
    tmp_path: Path,
) -> None:
    archive = _archive(tmp_path)
    result = _run_json((str(archive / "timeline"), "stats", "--format", "json"))
    content = as_object(result["content"], "content")

    assert content["human_prompts"] == TextTotals.from_texts(
        ("Owner α words",)
    ).to_mapping()
    assert content["all_prompts"] == content["human_prompts"]
    assert content["mechanically_linked_responses"] == TextTotals.from_texts(
        ("Linked résumé",)
    ).to_mapping()
    assert content["unlinked_responses"] == TextTotals.from_texts(
        ("Unlinked response here",)
    ).to_mapping()
    generated_texts = (
        "Project overview",
        "Agent lifetime summary",
        "Phase phrase\nPhase paragraph with β.",
        "# Daily\n\nTechnical rollup.\n",
    )
    assert content["generated_summaries"] == TextTotals.from_texts(
        generated_texts
    ).to_mapping()

    availability = as_object(result["summary_availability"], "availability")
    assert availability["available"] == 4
    assert availability["unavailable"] == 3
    by_kind = as_object(availability["by_kind"], "availability.by_kind")
    assert as_object(by_kind["work_phases"], "work phases") == {
        "available": 1,
        "unavailable": 1,
        "total": 2,
        "content": TextTotals.from_texts(
            ("Phase phrase\nPhase paragraph with β.",)
        ).to_mapping(),
    }


def test_stats_time_filter_excludes_unbounded_project_overview(
    tmp_path: Path,
) -> None:
    archive = _archive(tmp_path)
    result = _run_json(
        (
            str(archive / "timeline"),
            "stats",
            "--start-time",
            "1970-01-01T00:00:00.090Z",
            "--end-time",
            "1970-01-01T00:00:00.300Z",
            "--format",
            "json",
        )
    )
    availability = as_object(result["summary_availability"], "availability")
    by_kind = as_object(availability["by_kind"], "availability.by_kind")
    project = as_object(by_kind["project_overviews"], "project overviews")
    assert project == {
        "available": 0,
        "unavailable": 0,
        "total": 0,
        "content": TextTotals().to_mapping(),
    }
    content = as_object(result["content"], "content")
    assert as_object(content["human_prompts"], "human prompts")["records"] == 1
    assert as_object(content["total_responses"], "responses")["records"] == 2


def test_installed_stats_action_is_text_by_explicit_format(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    archive = _archive(tmp_path)
    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(archive),
                "--format",
                "text",
                "stats",
            )
        )
        == 0
    )
    output = capsys.readouterr().out
    assert "Archive content statistics" in output
    assert "Identified human prompts" in output
    assert "Mechanically linked responses" in output
    assert "Summary availability" in output
