from __future__ import annotations

import gzip
import hashlib
import json
import os
import subprocess
import sys
import threading
import urllib.request
from dataclasses import replace
from pathlib import Path

import pytest

from wrkviz.build_store import team_build_root
from wrkviz.archive import narrow_json, write_json_if_changed
from wrkviz.artifacts import extract_artifacts
from wrkviz.cli import main as timeline_main
from wrkviz.github_enrich import pull_metadata_path
from wrkviz.github_metadata import (
    PullRequestKey,
    PullRequestMetadata,
    PullRequestMetadataCache,
    save_pull_request_metadata_cache,
)
from wrkviz.identity import HostIdentity, ProjectIdentity, SiteIdentity
from wrkviz.model import (
    Agent,
    Edge,
    Event,
    SourceSnapshot,
    TeamData,
    ToolCall,
    Turn,
    source_digest,
)
from wrkviz.multi_team import (
    _remove_stale_files,
    build_combined_archive,
)
from wrkviz.periods import Period, periods_for_range
from wrkviz.phases import PhaseStats, PhaseWindow, build_phases
from wrkviz.pipeline import (
    IngestReport,
    _agent_name_jobs,
    _definition_evidence,
    _glossary_terms,
    _load_agent_names,
    _root_overview_input,
    _rollup_jobs_for_level,
    _write_ingested_team,
    build_archive,
    extract_transcripts_archive,
    load_archived_team,
    record_run,
    summarize_archive,
)
from wrkviz.render import (
    _remove_stale_presentation_files,
    prune_retired_query_artifacts,
)
from wrkviz.query import (
    SCHEMA_3_BOOTSTRAP_PATH,
    QueryFilters,
    TimelineQuery,
)
from wrkviz.server import make_server
from wrkviz.summarize import PLAIN_LANGUAGE_ROLLUP_STYLE, SummaryResult
from wrkviz.terminology import GlossaryTerm, glossary_term_id
from wrkviz.window import DateWindow
from wrkviz.snapshot_store import resolve_snapshot_root
from tests.timeline_projection import (
    read_stored_text,
    detail_documents,
    schema_1_timeline_text,
    stored_path,
)
from tests.timeline_legacy_generations import write_legacy_schema_2


ROOT = "00000000-0000-0000-0000-000000000001"
CHILD = "00000000-0000-0000-0000-000000000002"
NESTED = "00000000-0000-0000-0000-000000000003"
CONTINUATION = "00000000-0000-0000-0000-000000000004"
START = 1_775_000_000_000


def _event(
    event_id: str,
    thread: str,
    offset: int,
    kind: str,
    text: str | None,
    *,
    phase: str | None = None,
) -> Event:
    return Event(
        event_id=event_id,
        thread_id=thread,
        turn_id="turn-" + thread[-1],
        timestamp_ms=START + offset,
        kind=kind,
        role="user" if kind == "user_prompt" else "assistant",
        phase=phase,
        text=text,
        content_availability="plaintext" if text else "none",
        encrypted_content=None,
        author=None,
        recipient=None,
        source_line=1,
    )


def _team(extra_root_text: str = "") -> TeamData:
    events = (
        _event(
            "prompt-1",
            ROOT,
            1_000,
            "user_prompt",
            "Investigate the safe-landing protocol and keep exact-head validation terminology.",
        ),
        _event(
            "root-response",
            ROOT,
            3_000,
            "assistant_message",
            "The coordinator assigned a focused safe-landing audit. " + extra_root_text,
        ),
        _event(
            "child-update",
            CHILD,
            8_000,
            "assistant_message",
            "Found that exact-head validation was not bound to the release receipt.",
        ),
        _event(
            "child-final",
            CHILD,
            18_000,
            "assistant_message",
            "Added receipt binding and proved six negative cases plus two positive cases.",
            phase="final_answer",
        ),
        _event(
            "root-final",
            ROOT,
            22_000,
            "assistant_message",
            "Receipt binding is complete with eight focused cases passing.",
        ),
    )
    return TeamData(
        team_slug="codex-test",
        provider="codex",
        root_thread_id=ROOT,
        display_timezone="America/New_York",
        sources=(SourceSnapshot("root.jsonl", ROOT, 100, 1, "a" * 64, 100, 10),),
        agents=(
            Agent(ROOT, None, "/root", None, None, 0, START, START + 23_000, "completed", "root"),
            Agent(
                CHILD,
                ROOT,
                "/root/release_receipt_audit",
                "Ada",
                None,
                1,
                START + 5_000,
                START + 19_000,
                "completed",
                "child",
            ),
        ),
        turns=(
            Turn("turn-1", ROOT, START, START + 23_000, "completed", 10, None, None),
            Turn("turn-2", CHILD, START + 5_000, START + 19_000, "completed", 10, None, None),
        ),
        events=events,
        tool_calls=(
            ToolCall(
                "tool-1",
                "item-1",
                CHILD,
                "turn-2",
                "exec",
                None,
                START + 10_000,
                START + 12_000,
                "completed",
                None,
                None,
                (("bash", 2), ("git", 1)),
                2,
            ),
        ),
        edges=(
            Edge(
                "spawn-1",
                "spawn-1",
                ROOT,
                CHILD,
                "spawn",
                START + 5_000,
                None,
                "encrypted",
                "gAAAA-test",
                3,
            ),
        ),
    )


def _write_team(archive: Path, team: TeamData) -> None:
    write_json_if_changed(
        team_build_root(archive, team.team_slug) / "raw" / "team.json",
        narrow_json(team.to_json_obj()),
    )


def test_knowledge_evidence_keeps_prior_context_but_excludes_post_window_events() -> None:
    team = _team()
    future = _event(
        "future-root",
        ROOT,
        30_000,
        "assistant_message",
        "FUTURE_ONLY_MARKER discussed exact-head after the selected day.",
    )
    bounded = replace(
        team,
        events=team.events + (future,),
        window_start_ms=START + 7_000,
        window_end_ms=START + 30_000,
    )

    overview = _root_overview_input(bounded)
    exact_head = next(term for term in _glossary_terms(bounded) if term.term == "exact-head")
    evidence = _definition_evidence(bounded, exact_head)

    assert "prompt-1" in overview.event_ids
    assert "future-root" not in overview.event_ids
    assert "Investigate the safe-landing protocol" in overview.transcript
    assert "FUTURE_ONLY_MARKER" not in overview.transcript
    assert evidence
    assert all(item.event_id != "future-root" for item in evidence)
    assert all("FUTURE_ONLY_MARKER" not in item.context for item in evidence)


def test_legacy_glossary_is_preserved_without_model_work(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    glossary_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "glossary.json"
    )
    write_json_if_changed(glossary_path, narrow_json({"terms": []}))

    before = glossary_path.read_bytes()
    report = summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")

    assert glossary_path.read_bytes() == before
    assert report.glossary_terms == 0
    assert report.glossary_definitions == 0


def test_cached_pipeline_builds_self_contained_site_idempotently(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    first = summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "test-model",
        reasoning_effort="high",
    )
    built = build_archive(tmp_path, team.team_slug)

    assert first.agent_names == 2
    assert first.project_overviews == 1
    assert first.glossary_terms == 0
    assert first.glossary_definitions == 0
    assert first.glossary_definitions == first.glossary_terms
    assert first.cache_misses == (
        first.phases
        + (2 * first.rollups)
        + first.agent_names
        + first.project_overviews
        + first.glossary_definitions
    )
    assert first.catalog_artifacts == first.cache_misses
    assert first.backend == "heuristic"
    assert first.model == "test-model"
    assert first.reasoning_effort == "high"
    assert first.service_tier is None
    assert first.newly_spent_usage.total_tokens == 0
    assert first.newly_spent_unknown_receipts == 0
    assert first.artifact_generation_unknown_receipts == 0
    assert first.unknown_legacy_artifacts == 0
    assert len(first.usage_run_paths) == (
        (2 * first.rollups) + 3 + int(first.glossary_definitions > 0)
    )
    assert all((tmp_path / path).is_file() for path in first.usage_run_paths)
    assert built["agents"] == 2
    assert (tmp_path / "index.html").is_file()
    assert (tmp_path / "timeline-core.js").is_file()
    markdown_bundle = tmp_path / "vendor" / "markdown-it-15.0.0.min.js"
    assert hashlib.sha256(markdown_bundle.read_bytes()).hexdigest() == (
        "8d0f6aca8f4de3321b6d07e03286176c59ec19b7b84abb6eb31f0fa795e83abc"
    )
    index_text = (tmp_path / "index.html").read_text(encoding="utf-8")
    assert index_text.index("markdown-it-15.0.0.min.js") < index_text.index("timeline-core.js")
    assert index_text.index("timeline-core.js") < index_text.index("app.js")
    generated_makefile = (tmp_path / "Makefile").read_text(encoding="utf-8")
    assert generated_makefile.startswith(".DEFAULT_GOAL := help")
    assert "help:\n" in generated_makefile
    assert "run-stats:\n\t$(PYTHON) ./run_stats.py\n" in generated_makefile
    assert "query:\n\t@./timeline $(QUERY_ARGS)\n" in generated_makefile
    make_environment = dict(os.environ)
    for variable in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL"):
        make_environment.pop(variable, None)
    make_help = subprocess.run(
        ("make", "help"),
        cwd=tmp_path,
        check=False,
        capture_output=True,
        text=True,
        env=make_environment,
    )
    assert make_help.returncode == 0, make_help.stderr
    assert "Agent timeline archive commands:" in make_help.stdout
    assert "./timeline --help" in make_help.stdout
    assert (tmp_path / "run_stats.py").is_file()
    assert (tmp_path / "run_stats.py").stat().st_mode & 0o111
    assert (tmp_path / "timeline").stat().st_mode & 0o111
    assert not (tmp_path / "query.py").exists()
    # The schema-1 monolith is not written any more, and neither is its gzip twin. Both readers
    # reached it only behind the two generations this build does write, and it was the single
    # largest file in the archive.
    assert not (tmp_path / "data" / "timeline.json").exists()
    assert not (tmp_path / "data" / "timeline.json.gz").exists()
    # Nor is the schema-2 presentation generation, since the website reads schema 3. One
    # generation is written; two more are still *readable*, for archives an older tool built.
    assert not (tmp_path / "data" / "timeline-v2.json").exists()
    assert not (tmp_path / "data" / "timeline-v2").exists()
    schema_3_bootstrap = json.loads(
        (tmp_path / "data" / "timeline-v3.json").read_text(encoding="utf-8")
    )
    assert schema_3_bootstrap["schema_version"] == 3
    assert schema_3_bootstrap["kind"] == "timeline-v3-bootstrap"
    assert schema_3_bootstrap["streams"]["timeline"]["shards"]
    schema_3_shards = [
        shard
        for stream in schema_3_bootstrap["streams"].values()
        for shard in stream["shards"]
    ]
    assert all((tmp_path / shard["path"]).is_file() for shard in schema_3_shards)
    assert all((tmp_path / shard["index_path"]).is_file() for shard in schema_3_shards)
    # No plain twin and no `.gz` twin: a schema-3 shard is stored compressed and served as stored,
    # which is the duplication the generation exists to remove.
    assert not any((tmp_path / (shard["path"] + ".gz")).exists() for shard in schema_3_shards)
    assert gzip.decompress((tmp_path / "app.js.gz").read_bytes()) == (
        tmp_path / "app.js"
    ).read_bytes()
    assert 'cache: "no-store"' not in (tmp_path / "app.js").read_text(
        encoding="utf-8"
    )
    assert "Content-Encoding" in (tmp_path / "serve.py").read_text(encoding="utf-8")
    generated_readme = (tmp_path / "README.md").read_text(encoding="utf-8")
    assert "## Human-facing methods" in generated_readme
    assert "## Read-only query quickstart" in generated_readme
    assert "## Top-level map" in generated_readme
    assert "There is deliberately no second query launcher" in generated_readme
    assert "./timeline agents --team TEAM --format jsonl" in generated_readme
    assert "./timeline show phase:TEAM::PHASE_ID --transcript" in generated_readme
    assert "data/export.json" in generated_readme
    timeline = json.loads(schema_1_timeline_text(tmp_path))
    assert len(timeline["agents"]) == 2
    child_track = next(agent for agent in timeline["agents"] if agent["id"] == CHILD)
    assert child_track["short_name"] == "Release receipt audit"
    assert child_track["official_name"] == "/root/release_receipt_audit"
    assert child_track["official_leaf"] == "release_receipt_audit"
    assert child_track["nickname"] == "Ada"
    assert "receipt binding" in child_track["lifetime_summary"]
    assert child_track["summary_available"] is True
    summary_prose = " ".join(
        [child_track["lifetime_summary"]]
        + [
            value
            for phase in timeline["phases"]
            for value in (phase["phrase"], phase["paragraph"])
        ]
    )
    assert "ASSISTANT:" not in summary_prose
    assert "TOOLS:" not in summary_prose
    assert "[2026-" not in summary_prose
    assert timeline["source_digest"] == source_digest(team)
    assert timeline["display_timezone_source"] == "legacy_team_data"
    assert timeline["teams"][0]["projects"] == []
    assert timeline["teams"][0]["hosts"] == []
    assert timeline["teams"][0]["stats"] == timeline["stats"]
    assert timeline["stats"]["events"] == len(timeline["events"])
    assert timeline["stats"]["active_agents"] == len(timeline["agents"])
    assert timeline["stats"]["external_messages"] == 0
    # The same totals reach the browser's entry point, which is now the schema-3 bootstrap: it
    # inlines `stats` for the reason it inlines `teams`, because a frame cannot be drawn without
    # them and a second round trip would be spent on nothing.
    assert schema_3_bootstrap["stats"] == timeline["stats"]
    assert any(edge["kind"] == "spawn" for edge in timeline["edges"])
    result_edges = [edge for edge in timeline["edges"] if edge["kind"] == "result"]
    assert len(result_edges) == 1
    assert result_edges[0]["source_id"] == CHILD
    assert result_edges[0]["target_id"] == ROOT
    assert result_edges[0]["source_ms"] == child_track["end_ms"]
    assert result_edges[0]["target_ms"] == child_track["end_ms"]
    detail = json.loads(next(iter(detail_documents(tmp_path / "data" / "details").values())))
    assert all("at_ms" in entry and "role" in entry for entry in detail["transcript"])
    rollup = timeline["rollups"][0]
    assert rollup["summary_available"] is True
    assert rollup["technical_summary_available"] is True
    assert rollup["plain_language_summary_available"] is True
    assert (tmp_path / rollup["technical_path"]).is_file()
    assert (tmp_path / rollup["plain_language_path"]).is_file()
    assert "plain-language summary" in (tmp_path / rollup["plain_language_path"]).read_text(
        encoding="utf-8"
    )
    rollup_record = json.loads(
        next(
            (tmp_path / "teams" / team.team_slug / "summary_data" / "rollups").rglob(
                "*.json"
            )
        ).read_text(encoding="utf-8")
    )
    assert rollup_record["schema_version"] == 2
    assert rollup_record["technical_summary"]["prompt_version"].endswith(
        "technical-rollup-v3"
    )
    assert rollup_record["plain_language_summary"]["prompt_version"].endswith(
        "plain-rollup-v4"
    )
    overview_record = json.loads(
        (
            tmp_path
            / "teams"
            / team.team_slug
            / "summary_data"
            / "project_overview.json"
        ).read_text(encoding="utf-8")
    )
    assert overview_record["schema_version"] == 3
    assert overview_record["knowledge_epoch"]["cutoff_reason"] == (
        "first-summary-source-frontier"
    )
    assert "transcript" not in overview_record["source"]
    assert overview_record["summary"]["prompt_version"].endswith(
        "project-overview-v2"
    )
    assert overview_record["summary"]["phrase"] == "Insufficient evidence"
    assert overview_record["summary"]["artifact_provenance"]["model"] == (
        "test-model"
    )
    summary_catalog = json.loads(
        (
            tmp_path
            / "teams"
            / team.team_slug
            / "summary_data"
            / "artifacts.json"
        ).read_text(encoding="utf-8")
    )
    assert summary_catalog["artifact_count"] == first.catalog_artifacts
    assert summary_catalog["logical_key_count"] == first.catalog_artifacts
    assert summary_catalog["model_counts"] == {"test-model": first.catalog_artifacts}
    assert not any(
        item["artifact"]["logical_key"].startswith("glossary-definition:")
        for item in summary_catalog["artifacts"]
    )
    assert all(
        (tmp_path / "teams" / team.team_slug / "summary_data" / item["cache_path"])
        .is_file()
        for item in summary_catalog["artifacts"]
    )
    assert not (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "glossary.json"
    ).exists()
    phase_record = json.loads(
        next(
            (
                tmp_path
                / "teams"
                / team.team_slug
                / "summary_data"
                / "phases"
            ).glob("*.json")
        ).read_text(encoding="utf-8")
    )
    assert phase_record["summary"]["prompt_version"].endswith("summary-v2")
    assert "summary_available" not in phase_record["summary"]
    assert timeline["glossary_path"].endswith("codex-test-glossary.md")
    assert timeline["glossary"] == []
    assert timeline["project_overview"]["evidence_status"] == "insufficient-evidence"
    glossary_catalog = tmp_path / timeline["glossary_path"]
    catalog_text = glossary_catalog.read_text(encoding="utf-8")
    assert catalog_text.index("## Project overview") < catalog_text.index(
        "## Project terms"
    )
    assert "_No supported semantic project concepts are available._" in catalog_text
    assert timeline["events"].count(
        {"agent_id": CHILD, "at_ms": START + 10_000, "kind": "tool_call"}
    ) == 3
    detail_path = stored_path(tmp_path / timeline["phases"][0]["detail_path"])
    assert detail_path.is_file()
    name_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "agents"
        / f"{CHILD}.json"
    )
    name_record = json.loads(name_path.read_text(encoding="utf-8"))
    assert name_record["schema_version"] == 3
    assert name_record["agent"]["official_path"] == "/root/release_receipt_audit"
    assert name_record["name"]["short_name"] == "Release receipt audit"
    assert "receipt binding" in name_record["name"]["lifetime_summary"]
    assert "summary_available" not in name_record["name"]
    agent_markdown = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summaries"
        / "agents"
        / f"{CHILD}.md"
    ).read_text(encoding="utf-8")
    assert "## Lifetime summary" in agent_markdown
    assert name_record["name"]["lifetime_summary"] in agent_markdown

    second = summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "test-model",
        reasoning_effort="high",
    )
    rebuilt = build_archive(tmp_path, team.team_slug)
    assert second.cache_misses == 0
    assert second.cache_hits == first.cache_misses
    assert second.files_changed == 0
    assert second.newly_spent_usage.total_tokens == 0
    assert second.artifact_generation_usage == first.artifact_generation_usage
    assert rebuilt["files_changed"] == 0

    run_path = record_run(
        tmp_path,
        ("wrkviz", "summarize"),
        "2026-08-05T00:00:00Z",
        "completed",
        team.team_slug,
        None,
        second,
        None,
    )
    run = json.loads(run_path.read_text(encoding="utf-8"))
    assert run["summaries"]["reasoning_effort"] == "high"
    assert run["summaries"]["service_tier"] is None
    assert run["summaries"]["newly_spent_usage"]["total_tokens"] == 0
    assert run["summaries"]["usage_run_paths"] == list(second.usage_run_paths)


def test_rebuild_prunes_retired_query_alias_and_only_its_cache(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    build_archive(tmp_path, team.team_slug)

    retired = tmp_path / "query.py"
    retired.write_bytes((tmp_path / "timeline").read_bytes())
    cache = tmp_path / "__pycache__"
    cache.mkdir()
    retired_cache = cache / "query.cpython-312.pyc"
    retained_cache = cache / "operator_helper.cpython-312.pyc"
    retired_cache.write_bytes(b"retired generated cache")
    retained_cache.write_bytes(b"unrelated cache")
    export_path = tmp_path / "data" / "export.json"
    export = json.loads(export_path.read_text(encoding="utf-8"))
    export["generated_files"].append("query.py")
    write_json_if_changed(export_path, export)

    rebuilt = build_archive(tmp_path, team.team_slug)

    assert rebuilt["files_changed"] >= 3
    assert not retired.exists()
    assert not retired_cache.exists()
    assert retained_cache.read_bytes() == b"unrelated cache"
    updated = json.loads(export_path.read_text(encoding="utf-8"))
    assert "query.py" not in updated["generated_files"]


def test_retired_query_cleanup_preserves_unowned_custom_launcher(
    tmp_path: Path,
) -> None:
    custom = tmp_path / "query.py"
    custom.write_text("print('custom operator helper')\n", encoding="utf-8")
    cache = tmp_path / "__pycache__"
    cache.mkdir()
    custom_cache = cache / "query.cpython-312.pyc"
    custom_cache.write_bytes(b"custom cache")

    assert prune_retired_query_artifacts(tmp_path) == 0
    assert custom.read_text(encoding="utf-8") == "print('custom operator helper')\n"
    assert custom_cache.read_bytes() == b"custom cache"


def _legacy_message_projection(archived: TeamData) -> dict[str, dict[str, object]]:
    """Rebuild `raw/messages/<thread-id>.json` exactly as the retired ingest writer produced it.

    Reproduced verbatim rather than described, because every test below turns on whether the
    archive still holds these five record sets once the writer is gone. Prose could not be checked
    against `raw/team.json`; this can, and it also lets a legacy directory be seeded with the real
    payload instead of a placeholder that a lenient sweep would pass by accident.
    """

    payloads: dict[str, dict[str, object]] = {}
    for agent in archived.agents:
        thread_id = agent.thread_id
        payloads[thread_id] = {
            "agent": agent.to_json_obj(),
            "turns": [
                turn.to_json_obj()
                for turn in archived.turns
                if turn.thread_id == thread_id
            ],
            "messages": [
                event.to_json_obj()
                for event in archived.events
                if event.thread_id == thread_id
            ],
            "tools": [
                tool.to_json_obj()
                for tool in archived.tool_calls
                if tool.thread_id == thread_id
            ],
            "edges": [
                edge.to_json_obj()
                for edge in archived.edges
                if edge.from_thread_id == thread_id or edge.to_thread_id == thread_id
            ],
        }
    return payloads


def _seed_legacy_message_projection(archive: Path, archived: TeamData) -> tuple[Path, int]:
    root = team_build_root(archive, archived.team_slug) / "raw" / "messages"
    root.mkdir(parents=True, exist_ok=True)
    written = 0
    for thread_id, payload in _legacy_message_projection(archived).items():
        path = root / f"{thread_id}.json"
        write_json_if_changed(path, narrow_json(payload))
        written += path.stat().st_size
    return root, written


def test_ingest_no_longer_writes_the_per_thread_message_projection(
    tmp_path: Path,
) -> None:
    team = _team()
    archived, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )

    assert not (team_build_root(tmp_path, team.team_slug) / "raw" / "messages").exists()
    assert report.retired_message_projections == 0
    assert report.retired_message_projection_bytes == 0

    # The removal is a deduplication, not a deletion: rebuilding the retired payload from the
    # `raw/team.json` that ingest just wrote reproduces it exactly, thread for thread.
    reloaded = load_archived_team(tmp_path, team.team_slug)
    projection = _legacy_message_projection(archived)
    assert set(projection) == {ROOT, CHILD}
    assert _legacy_message_projection(reloaded) == projection


def test_ingest_retires_a_legacy_message_projection_and_reports_what_it_freed(
    tmp_path: Path,
) -> None:
    team = _team()
    archived, _ = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    root, seeded_bytes = _seed_legacy_message_projection(tmp_path, archived)
    assert seeded_bytes > 0

    _, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )

    assert not root.exists()
    assert report.retired_message_projections == 2
    assert report.retired_message_projection_bytes == seeded_bytes
    assert report.files_changed == 2

    receipt = report.to_json_obj()
    assert receipt["retired_message_projections"] == 2
    assert receipt["retired_message_projection_bytes"] == seeded_bytes

    # Sweeping is a one-time event, so the very next ingest must be silent about it rather than
    # re-reporting a reclamation that already happened.
    _, again = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    assert again.retired_message_projections == 0
    assert again.retired_message_projection_bytes == 0
    assert again.files_changed == 0


def test_message_projection_sweep_leaves_every_file_it_did_not_write(
    tmp_path: Path,
) -> None:
    team = _team()
    archived, _ = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    root, _ = _seed_legacy_message_projection(tmp_path, archived)

    # Four shapes the writer never produced. If the sweep took any of them it would be deleting
    # somebody else's data on the strength of a directory name.
    notes = root / "operator-notes.txt"
    notes.write_text("why this lineage was re-ingested\n", encoding="utf-8")
    sidecar = root / f"{ROOT}.json.gz"
    sidecar.write_bytes(b"not a projection")
    nested = root / "subdirectory"
    nested.mkdir()
    (nested / "kept.json").write_text("{}\n", encoding="utf-8")
    link = root / "elsewhere.json"
    link.symlink_to(team_build_root(tmp_path, team.team_slug) / "raw" / "team.json")

    _, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )

    assert report.retired_message_projections == 2
    assert not (root / f"{ROOT}.json").exists()
    assert not (root / f"{CHILD}.json").exists()
    # The directory itself survives because it is no longer empty of things that are not ours.
    assert root.is_dir()
    assert notes.read_text(encoding="utf-8") == "why this lineage was re-ingested\n"
    assert sidecar.read_bytes() == b"not a projection"
    assert (nested / "kept.json").read_text(encoding="utf-8") == "{}\n"
    assert link.is_symlink()
    assert (team_build_root(tmp_path, team.team_slug) / "raw" / "team.json").is_file()


def test_message_projection_sweep_takes_every_thread_shaped_name_not_only_this_run_s(
    tmp_path: Path,
) -> None:
    """Pin the deliberate breadth of the predicate, and the price it charges.

    The sweep matches the writer's name *shape*, not the thread ids of the team being ingested.
    That is what reclaims an orphan left by an earlier, wider ingest -- the retired writer never
    deleted anything, so narrowing `--team` or the date window stranded projections that no
    exact-name rule could ever name again, and leaving them would also keep the directory
    permanently non-empty and therefore unremovable. The same rule necessarily takes a `.json` file
    an operator put in this ingest-owned directory. Both halves are asserted here so that neither
    can be changed by accident: the second is a cost that was accepted, not an oversight.
    """

    team = _team()
    archived, _ = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    root, seeded_bytes = _seed_legacy_message_projection(tmp_path, archived)

    orphan = root / "01hzzzzzzzzzzzzzzzzzzzzzzz.json"
    orphan.write_text('{"agent": {}}\n', encoding="utf-8")
    operator_file = root / "notes.2026.json"
    operator_file.write_text('{"why": "re-ingested"}\n', encoding="utf-8")
    expected = seeded_bytes + orphan.stat().st_size + operator_file.stat().st_size

    _, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )

    assert not root.exists()
    assert report.retired_message_projections == 4
    assert report.retired_message_projection_bytes == expected


def test_message_projection_sweep_ignores_a_symlinked_directory(tmp_path: Path) -> None:
    team = _team()
    _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    # A symlink where the retired directory used to be points the sweep at files that are not in
    # the archive at all. It must decline rather than follow the name out of the tree.
    decoy = tmp_path / "decoy"
    decoy.mkdir()
    victim = decoy / f"{ROOT}.json"
    victim.write_text("{}\n", encoding="utf-8")
    root = team_build_root(tmp_path, team.team_slug) / "raw" / "messages"
    root.symlink_to(decoy, target_is_directory=True)

    _, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )

    assert report.retired_message_projections == 0
    assert victim.read_text(encoding="utf-8") == "{}\n"
    assert root.is_symlink()


def test_build_after_retiring_the_message_projection_serves_a_working_archive(
    tmp_path: Path,
) -> None:
    team = _team()
    archived, _ = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    root, _ = _seed_legacy_message_projection(tmp_path, archived)
    assert root.is_dir()

    _, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    assert report.retired_message_projections == 2
    assert not root.exists()

    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    built = build_archive(tmp_path, team.team_slug)

    assert built["files_changed"] > 0
    assert not root.exists()
    assert (tmp_path / "index.html").is_file()
    timeline = json.loads(schema_1_timeline_text(tmp_path))
    assert {agent["id"] for agent in timeline["agents"]} == {ROOT, CHILD}
    for phase in timeline["phases"]:
        assert stored_path(tmp_path / phase["detail_path"]).is_file()

    # The sweep must not leave the presentation inventory believing it still owns something, so a
    # second build has to be a no-op rather than re-converging on the changed tree.
    assert build_archive(tmp_path, team.team_slug)["files_changed"] == 0

    # Serve it the way an operator would, and fetch the two files the page cannot start without.
    schema_3_bootstrap = json.loads(
        (tmp_path / "data" / "timeline-v3.json").read_text(encoding="utf-8")
    )
    server = make_server(tmp_path, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = int(server.server_address[1])
        for relative in ("index.html", "data/timeline-v3.json"):
            url = f"http://127.0.0.1:{port}/{relative}"
            with urllib.request.urlopen(url, timeout=5) as response:
                assert response.status == 200
                assert response.read()
        # And one gzip member of one shard, by the byte range its sidecar published, which is how
        # the page reads every shard. `Accept-Ranges` and the 206 are the server half of the split
        # `static/app.js` depends on; nothing else in this suite fetches a range from the real
        # server against a real build.
        shard = schema_3_bootstrap["streams"]["spine"]["shards"][0]
        index_lines = (tmp_path / shard["index_path"]).read_text(
            encoding="utf-8"
        ).strip().split("\n")
        member = json.loads(index_lines[1])
        request = urllib.request.Request(
            f"http://127.0.0.1:{port}/{shard['path']}",
            headers={"Range": f"bytes=0-{member['c_len'] - 1}"},
        )
        with urllib.request.urlopen(request, timeout=5) as response:
            assert response.status == 206
            assert response.headers["Accept-Ranges"] == "bytes"
            assert response.headers["Content-Range"] == (
                f"bytes 0-{member['c_len'] - 1}/{shard['c_bytes']}"
            )
            body = response.read()
        assert len(body) == member["c_len"]
        assert len(gzip.decompress(body)) == member["u_len"]
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def test_append_catchup_keeps_completed_historical_overview_stable(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")

    daily_dir = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "rollups"
        / "daily"
    )
    original_daily_path = next(daily_dir.glob("*.json"))
    original_daily = json.loads(original_daily_path.read_text(encoding="utf-8"))
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    original_overview = json.loads(overview_path.read_text(encoding="utf-8"))

    later_offset = 2 * 24 * 60 * 60 * 1000
    late_events = (
        _event(
            "late-dbi-first",
            ROOT,
            later_offset,
            "user_prompt",
            "Investigate DBI behavior without changing earlier summaries.",
        ),
        _event(
            "late-dbi-second",
            ROOT,
            later_offset + 1_000,
            "user_prompt",
            "Use DBI consistently in this newly active workstream.",
        ),
        _event(
            "late-dbi-response",
            ROOT,
            later_offset + 2_000,
            "assistant_message",
            "DBI work belongs only to the later calendar period.",
        ),
    )
    later_end = START + later_offset + 3_000
    updated_agents = tuple(
        replace(agent, ended_at_ms=later_end)
        if agent.thread_id == ROOT
        else agent
        for agent in team.agents
    )
    updated_turns = tuple(
        replace(turn, ended_at_ms=later_end)
        if turn.thread_id == ROOT
        else turn
        for turn in team.turns
    )
    appended = replace(
        team,
        agents=updated_agents,
        turns=updated_turns,
        events=team.events + late_events,
    )
    _write_team(tmp_path, appended)

    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")

    refreshed_daily = json.loads(original_daily_path.read_text(encoding="utf-8"))
    refreshed_overview = json.loads(overview_path.read_text(encoding="utf-8"))

    assert refreshed_daily["technical_summary"]["input_hash"] == (
        original_daily["technical_summary"]["input_hash"]
    )
    assert refreshed_daily["plain_language_summary"]["input_hash"] == (
        original_daily["plain_language_summary"]["input_hash"]
    )
    assert refreshed_overview["knowledge_epoch"] == original_overview["knowledge_epoch"]
    assert refreshed_overview["source"] == original_overview["source"]
    assert refreshed_overview["summary"]["input_hash"] == (
        original_overview["summary"]["input_hash"]
    )
    assert not (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "glossary.json"
    ).exists()


def test_backfill_renews_overview_epoch_and_preserves_immutable_cache(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    summary_root = tmp_path / "teams" / team.team_slug / "summary_data"
    overview_path = summary_root / "project_overview.json"
    original = json.loads(overview_path.read_text(encoding="utf-8"))
    original_hash = original["summary"]["input_hash"]
    original_cache_path = summary_root / "cache" / f"{original_hash}.json"
    original_cache_bytes = original_cache_path.read_bytes()

    backfill = _event(
        "backfilled-root-intent",
        ROOT,
        500,
        "user_prompt",
        "Define safe landing as exact-head validation before any release receipt.",
    )
    _write_team(tmp_path, replace(team, events=team.events + (backfill,)))

    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    renewed = json.loads(overview_path.read_text(encoding="utf-8"))
    renewed_hash = renewed["summary"]["input_hash"]

    assert renewed["knowledge_epoch"]["epoch_id"] != (
        original["knowledge_epoch"]["epoch_id"]
    )
    assert renewed["knowledge_epoch"]["cutoff_ms"] == (
        original["knowledge_epoch"]["cutoff_ms"]
    )
    assert renewed["source"]["event_ids"][0] == "backfilled-root-intent"
    assert renewed_hash != original_hash
    assert original_cache_path.read_bytes() == original_cache_bytes
    assert (summary_root / "cache" / f"{renewed_hash}.json").is_file()
    catalog = json.loads((summary_root / "artifacts.json").read_text(encoding="utf-8"))
    overview_hashes = {
        item["artifact"]["input_hash"]
        for item in catalog["artifacts"]
        if item["artifact"]["logical_key"]
        == f"project-overview:{team.team_slug}"
    }
    assert {original_hash, renewed_hash}.issubset(overview_hashes)


def test_frozen_overview_rejects_historical_source_mutation(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    changed_events = tuple(
        replace(event, text="Historically rewritten coordinator response.")
        if event.event_id == "root-response"
        else event
        for event in team.events
    )
    _write_team(tmp_path, replace(team, events=changed_events))

    with pytest.raises(ValueError, match="overview evidence was mutated or truncated"):
        summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")


def test_corrected_system_input_classification_can_renew_overview_epoch(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    original = json.loads(overview_path.read_text(encoding="utf-8"))
    reclassified = tuple(
        replace(
            event,
            kind="system_input",
            ingress_kind="claude_system",
            author_kind="system",
        )
        if event.event_id == "prompt-1"
        else event
        for event in team.events
    )
    _write_team(tmp_path, replace(team, events=reclassified))

    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    renewed = json.loads(overview_path.read_text(encoding="utf-8"))

    assert renewed["knowledge_epoch"]["epoch_id"] != (
        original["knowledge_epoch"]["epoch_id"]
    )
    assert "prompt-1" not in renewed["source"]["event_ids"]


def test_frozen_overview_rejects_mutation_even_when_source_set_grows(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    changed_events = tuple(
        replace(event, text="Historically rewritten coordinator response.")
        if event.event_id == "root-response"
        else event
        for event in team.events
    ) + (
        _event(
            "backfilled-root-intent",
            ROOT,
            500,
            "user_prompt",
            "Newly recovered owner intent from the same historical interval.",
        ),
    )
    _write_team(tmp_path, replace(team, events=changed_events))

    with pytest.raises(ValueError, match="overview evidence was mutated or truncated"):
        summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")


def test_retired_glossary_does_not_block_source_updates_or_change_bytes(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    glossary_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "glossary.json"
    )
    write_json_if_changed(glossary_path, narrow_json({"schema_version": 3, "terms": []}))
    glossary_before = glossary_path.read_bytes()
    changed_events = tuple(
        replace(event, text="Rewritten child evidence no longer names the term.")
        if event.event_id == "child-update"
        else event
        for event in team.events
    )
    _write_team(tmp_path, replace(team, events=changed_events))

    report = summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")

    assert report.glossary_terms == 0
    assert report.glossary_definitions == 0
    assert glossary_path.read_bytes() == glossary_before


def test_build_embeds_standalone_site_identity(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    identity = SiteIdentity(
        team.team_slug,
        (
            ProjectIdentity(
                "dev-widget",
                "https://github.com/example-org/dev-widget",
                True,
                "session_metadata",
            ),
        ),
        (HostIdentity("devbig014", "explicit"),),
        team.display_timezone,
        "explicit",
    )
    write_json_if_changed(
        team_build_root(tmp_path, team.team_slug) / "raw" / "site-identity.json",
        narrow_json(identity.to_json_obj()),
    )
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, team.team_slug)

    timeline = json.loads(schema_1_timeline_text(tmp_path))
    assert timeline["display_timezone"] == "America/New_York"
    assert timeline["display_timezone_source"] == "explicit"
    assert timeline["teams"] == [
        {
            "slug": team.team_slug,
            "label": team.team_slug,
            "projects": [identity.projects[0].to_json_obj()],
            "hosts": [identity.hosts[0].to_json_obj()],
            "stats": timeline["stats"],
        }
    ]


def test_phase_details_emit_conservative_pull_request_link_spans(tmp_path: Path) -> None:
    team = _team(
        "Reviewed https://github.com/example-org/dev-widget/pull/38 and "
        "sched-ext/scx#3668; naked #7 is ambiguous."
    )
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, team.team_slug)

    detail_objects = [
        json.loads(text)
        for text in detail_documents(tmp_path / "data" / "details").values()
    ]
    entry = next(
        transcript_entry
        for detail in detail_objects
        for transcript_entry in detail["transcript"]
        if "dev-widget/pull/38" in transcript_entry["text"]
    )
    references = entry["pull_requests"]
    assert [reference["repository"] for reference in references] == [
        "example-org/dev-widget",
        "sched-ext/scx",
    ]
    assert [reference["number"] for reference in references] == [38, 3668]
    assert all(
        entry["text"][reference["start"] : reference["end"]] == reference["text"]
        for reference in references
    )
    assert all(reference["text"] != "#7" for reference in references)

    pull = PullRequestMetadata(
        key=PullRequestKey("example-org/dev-widget", 38),
        title="Repair archive refresh",
        state="closed",
        draft=False,
        merged_at="2026-08-05T10:00:00Z",
        body_excerpt="Makes refresh append-safe.",
        base_ref="main",
        head_label="example-org:archive-refresh",
        author="alice",
        updated_at="2026-08-05T10:00:00Z",
        etag='W/"pull-38"',
        fetched_at="2026-08-05T11:00:00Z",
    )
    save_pull_request_metadata_cache(
        pull_metadata_path(tmp_path, team.team_slug),
        PullRequestMetadataCache((pull,)),
    )
    build_archive(tmp_path, team.team_slug)
    enriched_details = [
        json.loads(text)
        for text in detail_documents(tmp_path / "data" / "details").values()
    ]
    enriched_entry = next(
        transcript_entry
        for detail in enriched_details
        for transcript_entry in detail["transcript"]
        if "dev-widget/pull/38" in transcript_entry["text"]
    )
    assert enriched_entry["pull_requests"][0]["title"] == "Repair archive refresh"
    assert enriched_entry["pull_requests"][0]["merged_at"] == "2026-08-05T10:00:00Z"


def test_reused_subagent_gets_one_structural_lifetime_result(tmp_path: Path) -> None:
    team = _team()
    second_final = _event(
        "child-final-again",
        CHILD,
        19_000,
        "assistant_message",
        "A resumed follow-up independently confirmed the receipt binding.",
        phase="final_answer",
    )
    updated = replace(team, events=team.events + (second_final,))
    _write_team(tmp_path, updated)
    summarize_archive(tmp_path, updated.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, updated.team_slug)
    timeline = json.loads(schema_1_timeline_text(tmp_path))
    result_edges = [edge for edge in timeline["edges"] if edge["kind"] == "result"]
    assert len(result_edges) == 1
    lifetime_result = result_edges[0]
    assert lifetime_result["id"] == f"result-{CHILD}"
    assert lifetime_result["source_id"] == CHILD
    assert lifetime_result["target_id"] == ROOT
    assert lifetime_result["source_ms"] == START + 19_000
    assert lifetime_result["target_ms"] == START + 19_000
    assert lifetime_result["phrase"] == "Release receipt audit returns to Coordinator"
    assert lifetime_result["full_text"] == ""
    assert lifetime_result["content_status"].startswith("Structural lifetime join.")
    turn_results = {
        edge["id"]: edge
        for edge in timeline["edges"]
        if edge["id"].startswith("turn-result-")
    }
    assert set(turn_results) == {
        "turn-result-child-final",
        "turn-result-child-final-again",
    }
    assert {edge["kind"] for edge in turn_results.values()} == {"message"}
    assert {edge["target_id"] for edge in turn_results.values()} == {ROOT}


def test_explicit_coordinator_continuation_is_structural_without_fake_join(
    tmp_path: Path,
) -> None:
    team = _team()
    continuation_start = START + 24_000
    continuation = Agent(
        CONTINUATION,
        ROOT,
        f"/root/continuation-{CONTINUATION}",
        None,
        "coordinator",
        1,
        continuation_start,
        START + 30_000,
        "completed",
        "root",
    )
    continuation_turn = Turn(
        "continuation-turn",
        CONTINUATION,
        continuation_start,
        START + 30_000,
        "completed",
        10,
        None,
        None,
    )
    continuation_final = replace(
        _event(
            "continuation-final",
            CONTINUATION,
            29_000,
            "assistant_message",
            "The replacement coordinator completed its continued work.",
            phase="final_answer",
        ),
        turn_id="continuation-turn",
    )
    continuation_edge = Edge(
        f"codex-continuation-{CONTINUATION}",
        f"codex-continuation-{CONTINUATION}",
        ROOT,
        CONTINUATION,
        "continuation",
        START + 23_000,
        "Explicit Codex session continuation, one second later.",
        "plaintext",
        None,
        1,
    )
    updated = replace(
        team,
        agents=team.agents + (continuation,),
        turns=team.turns + (continuation_turn,),
        events=team.events + (continuation_final,),
        edges=team.edges + (continuation_edge,),
    )
    _write_team(tmp_path, updated)
    summarize_archive(tmp_path, updated.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, updated.team_slug)

    timeline = json.loads(schema_1_timeline_text(tmp_path))
    edges = {edge["id"]: edge for edge in timeline["edges"]}
    rendered = edges[f"codex-continuation-{CONTINUATION}"]
    assert rendered["kind"] == "continuation"
    assert rendered["source_ms"] == START + 23_000
    assert rendered["target_ms"] == continuation_start
    assert rendered["phrase"].startswith("Continue as ")
    assert f"result-{CONTINUATION}" not in edges
    assert "turn-result-continuation-final" not in edges


@pytest.mark.parametrize(
    ("status", "ended_at_ms", "expected_results"),
    (("interrupted", START + 19_000, 1), ("running", None, 0)),
)
def test_only_ended_agent_lifetimes_get_a_structural_join(
    tmp_path: Path,
    status: str,
    ended_at_ms: int | None,
    expected_results: int,
) -> None:
    team = _team()
    child = replace(team.agents[1], status=status, ended_at_ms=ended_at_ms)
    updated = replace(team, agents=(team.agents[0], child))
    _write_team(tmp_path, updated)
    summarize_archive(tmp_path, updated.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, updated.team_slug)

    timeline = json.loads(schema_1_timeline_text(tmp_path))
    result_edges = [edge for edge in timeline["edges"] if edge["kind"] == "result"]
    assert len(result_edges) == expected_results
    if expected_results:
        assert result_edges[0]["source_id"] == CHILD
        assert result_edges[0]["target_id"] == ROOT
        assert result_edges[0]["phrase"].endswith(f"({status})")


def test_resumed_nested_agent_joins_parent_while_turn_results_reach_initiators(
    tmp_path: Path,
) -> None:
    team = _team()
    nested_initial = replace(
        _event(
            "nested-initial-final",
            NESTED,
            11_000,
            "assistant_message",
            "The nested agent completed its initial audit for its parent.",
            phase="final_answer",
        ),
        turn_id="nested-initial",
    )
    nested_resumed = replace(
        _event(
            "nested-resumed-final",
            NESTED,
            28_000,
            "assistant_message",
            "The root coordinator resumed the nested agent after its parent finished.",
            phase="final_answer",
        ),
        turn_id="nested-resumed",
    )
    agents = (
        replace(team.agents[0], ended_at_ms=START + 30_000),
        team.agents[1],
        Agent(
            NESTED,
            CHILD,
            "/root/release_receipt_audit/nested_review",
            "Emmy",
            None,
            2,
            START + 6_000,
            START + 29_000,
            "completed",
            "nested",
        ),
    )
    turns = team.turns + (
        Turn(
            "nested-initial",
            NESTED,
            START + 6_000,
            START + 12_000,
            "completed",
            10,
            None,
            None,
        ),
        Turn(
            "nested-resumed",
            NESTED,
            START + 24_000,
            START + 29_000,
            "completed",
            10,
            None,
            None,
        ),
    )
    edges = team.edges + (
        Edge(
            "spawn-nested",
            "spawn-nested",
            CHILD,
            NESTED,
            "spawn",
            START + 6_400,
            None,
            "encrypted",
            "gAAAA-nested",
            4,
        ),
        Edge(
            "resume-nested",
            "resume-nested",
            ROOT,
            NESTED,
            "followup",
            START + 24_600,
            None,
            "encrypted",
            "gAAAA-resume",
            5,
        ),
    )
    updated = replace(
        team,
        agents=agents,
        turns=turns,
        events=team.events + (nested_initial, nested_resumed),
        edges=edges,
    )
    _write_team(tmp_path, updated)
    summarize_archive(tmp_path, updated.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, updated.team_slug)

    timeline = json.loads(schema_1_timeline_text(tmp_path))
    result_edges = {
        edge["id"]: edge["target_id"]
        for edge in timeline["edges"]
        if edge["kind"] == "result"
    }
    assert result_edges[f"result-{NESTED}"] == CHILD
    turn_result_targets = {
        edge["id"]: edge["target_id"]
        for edge in timeline["edges"]
        if edge["id"].startswith("turn-result-nested-")
    }
    assert turn_result_targets == {
        "turn-result-nested-initial-final": CHILD,
        "turn-result-nested-resumed-final": ROOT,
    }
    nested_track = next(agent for agent in timeline["agents"] if agent["id"] == NESTED)
    assert nested_track["parent_id"] == CHILD
    assert nested_track["end_ms"] > team.agents[1].ended_at_ms


def test_team_slug_and_archived_identity_cannot_escape_archive(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="team slug"):
        load_archived_team(tmp_path, "../../outside")

    team = replace(_team(), team_slug="other-team")
    target = team_build_root(tmp_path, "codex-test") / "raw" / "team.json"
    write_json_if_changed(target, narrow_json(team.to_json_obj()))
    with pytest.raises(ValueError, match="does not match requested"):
        load_archived_team(tmp_path, "codex-test")


def test_build_refuses_to_clobber_non_archive_directory(tmp_path: Path) -> None:
    project = tmp_path / "real-project"
    project.mkdir()
    readme = project / "README.md"
    makefile = project / "Makefile"
    readme.write_text("valuable project readme\n", encoding="utf-8")
    makefile.write_text("all:\n\t@true\n", encoding="utf-8")

    with pytest.raises(ValueError, match="refusing non-empty non-archive"):
        build_archive(project, "codex-test")
    assert readme.read_text(encoding="utf-8") == "valuable project readme\n"
    assert makefile.read_text(encoding="utf-8") == "all:\n\t@true\n"


def test_build_refuses_symlinked_generated_parent_before_touching_victim(
    tmp_path: Path,
) -> None:
    source = tmp_path / "source"
    team = _team()
    _write_team(source, team)
    output = tmp_path / "output"
    output.mkdir()
    write_json_if_changed(
        output / ".wrkviz.json",
        {"schema_version": 1, "tool": "wrkviz"},
    )
    victim = tmp_path / "victim"
    victim.mkdir()
    sentinel = victim / "sentinel.txt"
    sentinel.write_text("preserve me\n", encoding="utf-8")
    (output / "data").symlink_to(victim, target_is_directory=True)

    with pytest.raises(ValueError, match="parent symlink"):
        build_archive(source, team.team_slug, output=output)

    assert sentinel.read_text(encoding="utf-8") == "preserve me\n"
    assert sorted(path.name for path in victim.iterdir()) == ["sentinel.txt"]


def test_build_only_run_preserves_source_digest_and_team_history(tmp_path: Path) -> None:
    ingest = IngestReport(
        team_slug="codex-test",
        source_digest="a" * 64,
        sources=1,
        source_bytes=10,
        agents=1,
        events=2,
        tool_calls=3,
        edges=1,
        files_changed=1,
    )
    record_run(
        tmp_path,
        ("wrkviz", "refresh"),
        "2026-08-05T00:00:00Z",
        "completed",
        "codex-test",
        ingest,
        None,
        None,
    )
    record_run(
        tmp_path,
        ("wrkviz", "build"),
        "2026-08-05T01:00:00Z",
        "completed",
        "other-team",
        None,
        None,
        {"files_changed": 0},
    )
    manifest = json.loads((tmp_path / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["latest_source_digest"] == "a" * 64
    assert manifest["teams"] == ["codex-test", "other-team"]
    assert manifest["run_count"] == 2


def test_concurrent_run_records_are_serialized_without_lost_updates(
    tmp_path: Path,
) -> None:
    count = 8
    barrier = threading.Barrier(count)
    failures: list[BaseException] = []

    def record(index: int) -> None:
        try:
            barrier.wait()
            record_run(
                tmp_path,
                ("wrkviz", "build", str(index)),
                "2026-08-05T01:00:00Z",
                "completed",
                "codex-test",
                None,
                None,
                {"files_changed": index},
            )
        except BaseException as error:
            failures.append(error)

    threads = [threading.Thread(target=record, args=(index,)) for index in range(count)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=10)

    assert failures == []
    assert all(not thread.is_alive() for thread in threads)
    manifest = json.loads((tmp_path / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["run_count"] == count
    assert len(list((tmp_path / "runs").glob("*.json"))) == count


def _two_day_team(child_result: str) -> TeamData:
    base = _team()
    changed_events = tuple(
        replace(event, text=child_result) if event.event_id == "child-final" else event
        for event in base.events
    )
    day = 26 * 60 * 60 * 1000
    later_events = (
        _event("day-two-prompt", ROOT, day, "user_prompt", "Verify the archived report."),
        _event(
            "day-two-response",
            ROOT,
            day + 2_000,
            "assistant_message",
            "The archived report remains verified.",
        ),
    )
    agents = tuple(
        replace(agent, ended_at_ms=START + day + 3_000)
        if agent.thread_id == ROOT
        else agent
        for agent in base.agents
    )
    return replace(base, agents=agents, events=changed_events + later_events)


def _daily_hashes(archive: Path) -> list[str]:
    paths = sorted(
        (archive / "teams" / "codex-test" / "summary_data" / "rollups" / "daily").glob(
            "*.json"
        )
    )
    return [
        json.loads(path.read_text(encoding="utf-8"))["technical_summary"]["input_hash"]
        for path in paths
    ]


def _daily_plain_hashes(archive: Path) -> list[str]:
    paths = sorted(
        (archive / "teams" / "codex-test" / "summary_data" / "rollups" / "daily").glob(
            "*.json"
        )
    )
    return [
        json.loads(path.read_text(encoding="utf-8"))["plain_language_summary"][
            "input_hash"
        ]
        for path in paths
    ]


def test_later_daily_rollup_hash_includes_prior_daily_summary(tmp_path: Path) -> None:
    first_team = _two_day_team(
        "Implemented alpha receipt binding; 101 focused tests passed and released the benchmark."
    )
    _write_team(tmp_path, first_team)
    summarize_archive(tmp_path, first_team.team_slug, "heuristic", "test-model")
    first = _daily_hashes(tmp_path)
    first_plain = _daily_plain_hashes(tmp_path)
    assert len(first) >= 2
    assert len(first_plain) == len(first)

    changed_team = _two_day_team(
        "Implemented beta scheduler isolation; 909 focused tests passed and released the benchmark."
    )
    _write_team(tmp_path, changed_team)
    summarize_archive(tmp_path, changed_team.team_slug, "heuristic", "test-model")
    second = _daily_hashes(tmp_path)
    second_plain = _daily_plain_hashes(tmp_path)
    assert first[0] != second[0]
    assert first[1] != second[1]
    assert first_plain[0] != second_plain[0]
    assert first_plain[1] != second_plain[1]


def test_isolated_day_backfill_loads_prior_day_from_artifact_catalog(
    tmp_path: Path,
) -> None:
    team = _two_day_team(
        "Implemented exact receipt binding and verified the focused tests."
    )
    _write_team(tmp_path, team)
    event_start = min(event.timestamp_ms for event in team.events)
    event_end = max(event.timestamp_ms for event in team.events)
    days = [
        period
        for period in periods_for_range(
            event_start,
            event_end,
            team.display_timezone,
            team.team_slug,
        )
        if period.kind == "daily"
    ]
    assert len(days) >= 2

    for period in days[:2]:
        summarize_archive(
            tmp_path,
            team.team_slug,
            "heuristic",
            "offline",
            summary_window=DateWindow(
                None,
                None,
                period.start_ms,
                period.end_ms,
            ),
            rollup_kinds=("daily",),
        )

    second_record = json.loads(
        (
            tmp_path
            / "teams"
            / team.team_slug
            / "summary_data"
            / "rollups"
            / "daily"
            / f"{days[1].key}.json"
        ).read_text(encoding="utf-8")
    )
    coverage = second_record["technical_summary"]["artifact_provenance"][
        "context_coverage"
    ]
    prior = next(
        item
        for item in coverage["components"]
        if item["name"] == "prior_same_level_summaries"
    )
    assert prior["requested"] == 1
    assert prior["provided"] == 1
    assert coverage["coverage_percent"] == 100
    assert coverage["frontier_status"] == "contiguous-extension"


def test_append_only_changed_window_and_rollups_invalidate(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    first = summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    appended_event = _event(
        "root-followup",
        ROOT,
        25_000,
        "assistant_message",
        "A new root-cause sentence extends only the coordinator window.",
    )
    changed = replace(
        team,
        events=team.events + (appended_event,),
        agents=tuple(
            replace(agent, ended_at_ms=START + 26_000)
            if agent.thread_id == ROOT
            else agent
            for agent in team.agents
        ),
        turns=tuple(
            replace(turn, ended_at_ms=START + 26_000)
            if turn.thread_id == ROOT
            else turn
            for turn in team.turns
        ),
    )
    _write_team(tmp_path, changed)
    second = summarize_archive(tmp_path, changed.team_slug, "heuristic", "test-model")
    assert 0 < second.cache_misses < first.cache_misses
    assert second.cache_hits > 0


def test_cross_spawn_context_reaches_child_phase() -> None:
    team = _team()
    child_phase = next(phase for phase in build_phases(team) if phase.agent_id == CHILD)
    assert "safe-landing protocol" in child_phase.prior_context
    assert "exact-head validation" in child_phase.prior_context


def test_hindsight_name_job_combines_phase_summary_with_parent_context() -> None:
    team = _team()
    phases = build_phases(team)
    results = {
        phase.summary_key: SummaryResult(
            key=phase.summary_key,
            phrase="Receipt binding audit",
            paragraph="Verified exact-head release receipt binding with eight focused cases.",
            work_summary=(),
            model="test-model",
            prompt_version="test-prompt",
            input_hash="a" * 64,
            generated_at="2026-08-05T00:00:00Z",
        )
        for phase in phases
    }

    jobs = _agent_name_jobs(team, phases, results)
    child = next(job for job in jobs if job.thread_id == CHILD)

    assert child.official_path == "/root/release_receipt_audit"
    assert child.parent_official_path == "/root"
    assert child.depth == 1
    assert "safe-landing protocol" in child.prior_context
    assert "exact-head release receipt binding" in child.work_summary


def test_summary_window_can_backfill_one_hour_without_other_rollup_levels(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 20_000,
        start_time="2026-04-01T00:00:00.000Z",
        end_time="2026-04-01T00:00:20.000Z",
    )

    report = summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "offline",
        summary_window=window,
        rollup_kinds=("hourly",),
    )

    assert report.rollups == 1
    rollup_root = tmp_path / "teams" / team.team_slug / "summary_data" / "rollups"
    assert len(list((rollup_root / "hourly").glob("*.json"))) == 1
    assert not (rollup_root / "daily").exists()
    phase_records = list(
        (tmp_path / "teams" / team.team_slug / "summary_data" / "phases").glob(
            "*.json"
        )
    )
    assert phase_records
    assert all(
        START <= json.loads(path.read_text(encoding="utf-8"))["start_ms"]
        < START + 20_000
        for path in phase_records
    )
    export = tmp_path / "hour-export"
    first_export = build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        rollup_kinds=("hourly",),
        output=export,
    )
    second_export = build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        rollup_kinds=("hourly",),
        output=export,
    )
    assert first_export["files_changed"] > 0
    assert second_export["files_changed"] == 0
    assert (export / ".wrkviz.json").is_file()
    timeline = json.loads(schema_1_timeline_text(export))
    assert {item["kind"] for item in timeline["rollups"]} == {"hourly"}
    assert all(
        START <= phase["start_ms"] < START + 20_000
        for phase in timeline["phases"]
    )


def test_single_team_export_wide_to_narrow_removes_stale_slice_data(
    tmp_path: Path,
) -> None:
    team = _two_day_team("first-day child result")
    _write_team(tmp_path, team)
    output = tmp_path / "slice-export"
    build_archive(tmp_path, team.team_slug, output=output)
    wide_documents = detail_documents(output / "data" / "details")
    wide_details = set(wide_documents)
    assert len(wide_details) > 1
    # Against the parsed document, not the stored bytes: the stored form is compressed, so a
    # substring search over the file would report "absent" for text that is plainly there.
    assert any(
        "Verify the archived report" in text for text in wide_documents.values()
    )

    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 20_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:40.000Z",
    )
    build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        output=output,
    )
    narrow_details = set(detail_documents(output / "data" / "details"))
    stale_details = wide_details - narrow_details
    assert stale_details
    assert all(not path.exists() for path in stale_details)
    # And no identity twin left behind either: the stored name is `<phase>.json.gz`, so the
    # thing a sweep could plausibly miss is the `.json` an older layout wrote beside it.
    assert all(
        not path.with_name(path.name.removesuffix(".gz")).exists()
        for path in stale_details
    )
    for path in output.rglob("*"):
        if not path.is_file():
            continue
        payload = path.read_bytes()
        if path.suffix == ".gz":
            payload = gzip.decompress(payload)
        assert b"Verify the archived report" not in payload
    manifest = json.loads(
        (output / "data" / "export.json").read_text(encoding="utf-8")
    )
    assert manifest["kind"] == "single-team-export"
    assert manifest["display_window"]["start_ms"] == START
    assert manifest["display_window"]["end_ms"] == START + 20_000


def test_build_without_summary_cache_uses_presentation_only_fallbacks(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 20_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:40.000Z",
    )
    output = tmp_path / "zero-summary-site"
    source_summary_root = tmp_path / "teams" / team.team_slug / "summary_data"

    first = build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        rollup_kinds=("hourly",),
        output=output,
    )
    second = build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        rollup_kinds=("hourly",),
        output=output,
    )

    assert first["files_changed"] > 0
    assert second["files_changed"] == 0
    assert not source_summary_root.exists()
    timeline = json.loads(schema_1_timeline_text(output))
    assert timeline["phases"]
    assert {phase["phrase"] for phase in timeline["phases"]} == {
        "Summary unavailable"
    }
    assert all(
        "normalized logs" in phase["paragraph"] for phase in timeline["phases"]
    )
    assert all(phase["summary_available"] is False for phase in timeline["phases"])
    assert all(
        "no cached hindsight name" in agent["naming_rationale"]
        for agent in timeline["agents"]
    )
    assert all(agent["summary_available"] is False for agent in timeline["agents"])
    assert timeline["activity_bins"]
    assert {item["team"] for item in timeline["activity_bins"]} == {
        team.team_slug
    }
    assert "no cached model summary" in timeline["project_overview"]["text"]
    assert timeline["project_overview"]["summary_available"] is False
    assert timeline["summary_files"] == []
    assert timeline["glossary_path"] == ""
    assert {rollup["kind"] for rollup in timeline["rollups"]} == {"hourly"}
    for phase in timeline["phases"]:
        detail = json.loads(read_stored_text(output / phase["detail_path"]))
        assert detail["transcript"]
        assert detail["stats"] == phase["stats"]
        assert detail["phrase"] == "Summary unavailable"
        assert detail["summary_available"] is False
        assert detail["raw_summary_path"] == ""
        raw_path = (
            output
            / "teams"
            / team.team_slug
            / "summaries"
            / "phases"
            / f"{phase['id']}.md"
        )
        assert not raw_path.exists()
    rollup = timeline["rollups"][0]
    assert rollup["summary_available"] is False
    assert rollup["technical_summary_available"] is False
    assert rollup["plain_language_summary_available"] is False
    assert rollup["technical_path"] == ""
    assert rollup["plain_language_path"] == ""
    assert rollup["stats"]
    assert not list((output / "teams" / team.team_slug / "summaries").rglob("*.md"))


def test_build_preserves_available_phase_summaries_in_patchy_archive(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    phases = build_phases(team, phase_minutes=30)
    assert len(phases) >= 2
    missing = phases[-1]
    summary_root = tmp_path / "teams" / team.team_slug / "summary_data"
    output = tmp_path / "patchy-site"
    build_archive(tmp_path, team.team_slug, output=output)
    stale_markdown = (
        output
        / "teams"
        / team.team_slug
        / "summaries"
        / "phases"
        / f"{missing.phase_id}.md"
    )
    assert stale_markdown.is_file()
    (summary_root / "phases" / f"{missing.phase_id}.json").unlink()
    (summary_root / "artifacts.json").unlink()

    build_archive(tmp_path, team.team_slug, output=output)
    timeline = json.loads(schema_1_timeline_text(output))
    phrases = {phase["id"]: phase["phrase"] for phase in timeline["phases"]}
    availability = {
        phase["id"]: phase["summary_available"] for phase in timeline["phases"]
    }

    assert phrases[missing.phase_id] == "Summary unavailable"
    assert availability[missing.phase_id] is False
    assert any(
        phrase != "Summary unavailable"
        for phase_id, phrase in phrases.items()
        if phase_id != missing.phase_id
    )
    assert all(
        available is True
        for phase_id, available in availability.items()
        if phase_id != missing.phase_id
    )
    missing_record = next(
        phase for phase in timeline["phases"] if phase["id"] == missing.phase_id
    )
    missing_detail = json.loads(read_stored_text(output / missing_record["detail_path"]))
    assert missing_detail["summary_available"] is False
    assert missing_detail["raw_summary_path"] == ""
    assert not stale_markdown.exists()


def test_build_invalidates_backfilled_phase_and_dependent_summaries(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "offline",
        rollup_kinds=("daily",),
    )
    output = tmp_path / "freshness-site"
    build_archive(
        tmp_path,
        team.team_slug,
        rollup_kinds=("daily",),
        output=output,
    )

    backfilled = replace(
        team,
        events=team.events
        + (
            _event(
                "late-recovered-child-evidence",
                CHILD,
                15_000,
                "assistant_message",
                "Recovered evidence changes the receipt-binding conclusion.",
            ),
        ),
    )
    _write_team(tmp_path, backfilled)
    build_archive(
        tmp_path,
        team.team_slug,
        rollup_kinds=("daily",),
        output=output,
    )
    timeline = json.loads(schema_1_timeline_text(output))
    child_phase = next(
        phase for phase in timeline["phases"] if phase["agent_id"] == CHILD
    )
    child_agent = next(agent for agent in timeline["agents"] if agent["id"] == CHILD)
    assert child_phase["summary_available"] is False
    assert child_agent["summary_available"] is False
    assert timeline["rollups"][0]["summary_available"] is False
    detail = json.loads(read_stored_text(output / child_phase["detail_path"]))
    assert "Recovered evidence changes" in json.dumps(detail["transcript"])

    summary_root = tmp_path / "teams" / team.team_slug / "summary_data"
    (summary_root / "phases" / f"{child_phase['id']}.json").unlink()
    (summary_root / "agents" / f"{CHILD}.json").unlink()
    daily_projections = list((summary_root / "rollups" / "daily").glob("*.json"))
    assert len(daily_projections) == 1
    daily_projections[0].unlink()
    build_archive(
        tmp_path,
        team.team_slug,
        rollup_kinds=("daily",),
        output=output,
    )
    catalog_only = json.loads(schema_1_timeline_text(output))
    assert next(
        phase
        for phase in catalog_only["phases"]
        if phase["agent_id"] == CHILD
    )["summary_available"] is False
    assert next(
        agent for agent in catalog_only["agents"] if agent["id"] == CHILD
    )["summary_available"] is False
    assert catalog_only["rollups"][0]["summary_available"] is False

    rerun = summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "offline",
        rollup_kinds=("daily",),
    )
    assert rerun.cache_misses > 0
    build_archive(
        tmp_path,
        team.team_slug,
        rollup_kinds=("daily",),
        output=output,
    )
    refreshed = json.loads(schema_1_timeline_text(output))
    assert next(
        phase for phase in refreshed["phases"] if phase["agent_id"] == CHILD
    )["summary_available"] is True
    assert next(
        agent for agent in refreshed["agents"] if agent["id"] == CHILD
    )["summary_available"] is True
    assert refreshed["rollups"][0]["summary_available"] is True


def test_build_recovers_compatible_paid_name_from_catalog(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    summary_root = tmp_path / "teams" / team.team_slug / "summary_data"
    name_path = summary_root / "agents" / f"{CHILD}.json"
    overview_path = summary_root / "project_overview.json"
    expected_name = json.loads(name_path.read_text(encoding="utf-8"))["name"]
    name_path.unlink()
    overview_path.unlink()

    output = tmp_path / "catalog-recovery-site"
    build_archive(tmp_path, team.team_slug, output=output)
    timeline = json.loads(schema_1_timeline_text(output))
    child = next(agent for agent in timeline["agents"] if agent["id"] == CHILD)
    assert child["short_name"] == expected_name["short_name"]
    assert child["lifetime_summary"] == expected_name["lifetime_summary"]
    assert "no cached model summary" in timeline["project_overview"]["text"]
    assert timeline["glossary"] == []


def test_build_does_not_recover_future_catalog_knowledge_into_slice(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 10_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:30.000Z",
    )

    output = tmp_path / "bounded-catalog-site"
    build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        rollup_kinds=("hourly",),
        output=output,
    )
    timeline = json.loads(schema_1_timeline_text(output))
    child = next(agent for agent in timeline["agents"] if agent["id"] == CHILD)
    assert "no cached hindsight name" in child["naming_rationale"]
    assert all(
        phase["phrase"] == "Summary unavailable" for phase in timeline["phases"]
    )
    assert all("final" not in phase["paragraph"].lower() for phase in timeline["phases"])
    assert all(rollup["summary_available"] is False for rollup in timeline["rollups"])
    assert all(rollup["technical_path"] == "" for rollup in timeline["rollups"])
    assert all(rollup["plain_language_path"] == "" for rollup in timeline["rollups"])
    assert "no cached model summary" in timeline["project_overview"]["text"]
    assert timeline["glossary"] == []


def test_build_validates_corrupt_partial_rollup_before_suppressing(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "offline",
        rollup_kinds=("hourly",),
    )
    period = periods_for_range(
        START,
        START + 9_999,
        team.display_timezone,
        team.team_slug,
        ("hourly",),
    )[0]
    rollup_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "rollups"
        / "hourly"
        / f"{period.key}.json"
    )
    write_json_if_changed(rollup_path, {"schema_version": 2})
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 10_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:30.000Z",
    )

    with pytest.raises(ValueError, match="schema mismatch"):
        build_archive(
            tmp_path,
            team.team_slug,
            display_window=window,
            rollup_kinds=("hourly",),
            output=tmp_path / "corrupt-partial-rollup-site",
        )


def test_build_does_not_render_stale_partial_rollup_as_complete(
    tmp_path: Path,
) -> None:
    hour_ms = 60 * 60 * 1000
    hour_start = (START // hour_ms) * hour_ms
    hour_end = hour_start + hour_ms
    team = replace(
        _team(),
        window_start_ms=hour_start,
        window_end_ms=hour_end,
    )
    _write_team(tmp_path, team)
    summarize_archive(
        tmp_path,
        team.team_slug,
        "heuristic",
        "offline",
        rollup_kinds=("hourly",),
    )
    period = periods_for_range(
        hour_start,
        hour_end - 1,
        team.display_timezone,
        team.team_slug,
        ("hourly",),
    )[0]
    assert not period.partial
    rollup_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "rollups"
        / "hourly"
        / f"{period.key}.json"
    )
    rollup = json.loads(rollup_path.read_text(encoding="utf-8"))
    rollup["partial"] = True
    write_json_if_changed(rollup_path, rollup)

    output = tmp_path / "stale-partial-rollup-site"
    build_archive(
        tmp_path,
        team.team_slug,
        rollup_kinds=("hourly",),
        output=output,
    )
    timeline = json.loads(schema_1_timeline_text(output))
    assert len(timeline["rollups"]) == 1
    assert timeline["rollups"][0]["summary_available"] is False
    assert timeline["rollups"][0]["technical_path"] == ""
    assert timeline["rollups"][0]["plain_language_path"] == ""


def test_build_suppresses_stale_out_of_window_overview_source(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    overview = json.loads(overview_path.read_text(encoding="utf-8"))
    overview["source"]["context_sha256"] = "0" * 64
    write_json_if_changed(overview_path, overview)
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 10_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:30.000Z",
    )

    output = tmp_path / "corrupt-out-of-window-overview-site"
    build_archive(
        tmp_path,
        team.team_slug,
        display_window=window,
        output=output,
    )
    timeline = json.loads(schema_1_timeline_text(output))
    assert timeline["project_overview"]["summary_available"] is False
    assert "Summary unavailable" in timeline["project_overview"]["text"]


def test_build_validates_out_of_window_overview_summary(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    overview = json.loads(overview_path.read_text(encoding="utf-8"))
    overview["summary"]["phrase"] = "corrupt evidence status"
    write_json_if_changed(overview_path, overview)
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 10_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:30.000Z",
    )

    with pytest.raises(ValueError, match="invalid project-overview evidence status"):
        build_archive(
            tmp_path,
            team.team_slug,
            display_window=window,
            output=tmp_path / "corrupt-out-of-window-summary-site",
        )


def test_build_validates_legacy_v2_overview_transcript_digest(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    overview = json.loads(overview_path.read_text(encoding="utf-8"))
    overview["schema_version"] = 2
    overview["source"]["transcript"] = _root_overview_input(team).transcript
    overview["source"]["context_sha256"] = "0" * 64
    write_json_if_changed(overview_path, overview)

    with pytest.raises(ValueError, match="transcript digest mismatch"):
        build_archive(
            tmp_path,
            team.team_slug,
            output=tmp_path / "corrupt-legacy-overview-site",
        )


def test_build_rejects_present_corrupt_overview_projection(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    write_json_if_changed(overview_path, {"legacy": "corrupt-shape"})

    with pytest.raises(ValueError, match="schema_version"):
        build_archive(tmp_path, team.team_slug, output=tmp_path / "corrupt-site")


def test_build_rejects_non_regular_summary_projection(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    overview_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "project_overview.json"
    )
    overview_path.mkdir(parents=True)

    with pytest.raises(ValueError, match="not a regular file"):
        build_archive(tmp_path, team.team_slug, output=tmp_path / "directory-site")


def test_combined_export_namespaces_teams_and_is_byte_idempotent(
    tmp_path: Path,
) -> None:
    first_team = _team(
        "Reviewed https://github.com/example/project/pull/12 as part of the result."
    )
    second_team = replace(first_team, team_slug="claude-test", provider="claude")
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 20_000,
        start_time="2026-04-01T00:00:00.000Z",
        end_time="2026-04-01T00:00:20.000Z",
    )
    for team in (first_team, second_team):
        _write_team(tmp_path, team)
        write_json_if_changed(
            team_build_root(tmp_path, team.team_slug) / "raw" / "artifacts.json",
            narrow_json(extract_artifacts(team).to_json_obj()),
        )
        summarize_archive(
            tmp_path,
            team.team_slug,
            "heuristic",
            "offline",
            summary_window=window,
            rollup_kinds=("hourly",),
        )
    output = tmp_path / "combined"

    first = build_combined_archive(
        tmp_path,
        ("claude-test", first_team.team_slug),
        output=output,
        display_timezone="America/New_York",
        display_window=window,
        rollup_kinds=("hourly",),
    )
    second = build_combined_archive(
        tmp_path,
        (first_team.team_slug, "claude-test"),
        output=output,
        display_timezone="America/New_York",
        display_window=window,
        rollup_kinds=("hourly",),
    )

    assert first["teams"] == 2
    assert first["files_changed"] > 0
    assert second["files_changed"] == 0
    timeline = json.loads(schema_1_timeline_text(output))
    assert timeline["range"] == {"start_ms": START, "end_ms": START + 20_000}
    assert [team["slug"] for team in timeline["teams"]] == [
        "claude-test",
        first_team.team_slug,
    ]
    assert {team["provider"] for team in timeline["teams"]} == {"claude", "codex"}
    agent_ids = {agent["id"] for agent in timeline["agents"]}
    assert len(agent_ids) == len(timeline["agents"])
    assert all("::" in agent_id for agent_id in agent_ids)
    assert all(phase["team"] in {"claude-test", first_team.team_slug} for phase in timeline["phases"])
    assert all(phase["agent_id"] in agent_ids for phase in timeline["phases"])
    assert timeline["activity_bins"]
    assert {item["team"] for item in timeline["activity_bins"]} == {
        "claude-test",
        first_team.team_slug,
    }
    assert {item["resolution"] for item in timeline["activity_bins"]} == {
        "hourly",
        "daily",
        "weekly",
    }
    assert all(edge["source_id"] in agent_ids for edge in timeline["edges"])
    assert all(edge["target_id"] in agent_ids for edge in timeline["edges"])
    assert {rollup["team"] for rollup in timeline["rollups"]} == {
        "claude-test",
        first_team.team_slug,
    }
    assert {rollup["kind"] for rollup in timeline["rollups"]} == {"hourly"}
    assert {item["team"] for item in timeline["summary_files"]} == {
        "claude-test",
        first_team.team_slug,
    }
    for phase in timeline["phases"]:
        detail_path = stored_path(output / phase["detail_path"])
        assert detail_path.is_file()
        assert phase["detail_path"].startswith(f"data/details/{phase['team']}/")
    assert (output / "Makefile").is_file()
    assert not (output / "query.py").exists()
    assert (output / "timeline").stat().st_mode & 0o111
    query_result = subprocess.run(
        (
            str(output / "timeline"),
            "teams",
        ),
        cwd=tmp_path,
        check=False,
        capture_output=True,
        text=True,
    )
    assert query_result.returncode == 0, query_result.stderr
    assert json.loads(query_result.stdout)["count"] == 2
    make_environment = dict(os.environ)
    for variable in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL"):
        make_environment.pop(variable, None)
    make_query_result = subprocess.run(
        ("make", "query", "QUERY_ARGS=list teams"),
        cwd=output,
        check=False,
        capture_output=True,
        text=True,
        env=make_environment,
    )
    assert make_query_result.returncode == 0, make_query_result.stderr
    assert json.loads(make_query_result.stdout)["count"] == 2
    exported_readme = (output / "README.md").read_text(encoding="utf-8")
    assert "## Human-facing methods" in exported_readme
    assert "## Read-only query quickstart" in exported_readme
    assert "## Top-level map" in exported_readme
    assert "./timeline agents --team TEAM --format jsonl" in exported_readme
    assert "./timeline show phase:TEAM::PHASE_ID --transcript" in exported_readme
    assert "data/export.json" in exported_readme
    assert (output / ".wrkviz.json").is_file()
    export_manifest = json.loads(
        (output / "data" / "export.json").read_text(encoding="utf-8")
    )
    assert export_manifest["teams"] == ["claude-test", first_team.team_slug]
    assert "query.py" not in export_manifest["generated_files"]
    assert "data/timeline.json" not in export_manifest["generated_files"]
    assert "data/timeline.json.gz" not in export_manifest["generated_files"]
    assert export_manifest["retired_files"] == []
    assert "data/timeline-v2.json" not in export_manifest["generated_files"]
    assert "data/timeline-v3.json" in export_manifest["generated_files"]
    assert not any(
        value.startswith("data/timeline-v2/") for value in export_manifest["generated_files"]
    )
    assert any(
        value.startswith("data/timeline-v3/") and value.endswith(".jsonl.gz")
        for value in export_manifest["generated_files"]
    )
    assert "app.js.gz" in export_manifest["generated_files"]
    # The manifest is an inventory of THIS archive, so every name in it must be a file that is
    # actually here. The regression this pins shipped: the per-phase details became gzip-only,
    # and the line recording the plain relative was left beside the one recording the stored
    # `.gz` -- so a real export named 11,899 `data/details/*.json` that no build writes any more.
    # Nothing crashed, which is the problem: the manifest quietly stopped describing the archive,
    # and it is what stale-file removal and any downstream integrity check read.
    absent = sorted(
        relative
        for relative in export_manifest["generated_files"]
        if not (output / relative).is_file()
    )
    assert absent == [], f"export manifest names {len(absent)} file(s) that do not exist"
    artifact_catalog = json.loads(read_stored_text(output / "data" / "artifacts.json"))
    artifact_ids = {
        artifact["artifact_id"] for artifact in artifact_catalog["artifacts"]
    }
    assert len(artifact_ids) == 2
    assert any(value.startswith("artifact-claude-test-") for value in artifact_ids)
    assert any(
        value.startswith(f"artifact-{first_team.team_slug}-")
        for value in artifact_ids
    )
    assert {artifact["team"] for artifact in artifact_catalog["artifacts"]} == {
        "claude-test",
        first_team.team_slug,
    }

    assert timeline_main(
        (
            "export",
            "--archive",
            str(tmp_path),
            "--output",
            str(output),
            "--team",
            first_team.team_slug,
            "--team",
            "claude-test",
            "--start-time",
            "2026-03-31T23:33:20Z",
            "--end-time",
            "2026-03-31T23:33:40Z",
            "--rollup-kind",
            "hourly",
            "--timezone",
            "America/New_York",
        )
    ) == 0
    manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["teams"] == ["claude-test", first_team.team_slug]
    run = json.loads(
        (output / "runs" / f"{manifest['last_run_id']}.json").read_text(
            encoding="utf-8"
        )
    )
    assert run["team_slugs"] == ["claude-test", first_team.team_slug]
    assert run["mechanical"]["website_export"] == {
        "schema_version": 1,
        "model_calls": 0,
        "model_tokens": 0,
        "website_build_performed": True,
    }


def test_combined_export_builds_two_zero_summary_teams(tmp_path: Path) -> None:
    codex_team = _team()
    claude_team = replace(codex_team, team_slug="claude-test", provider="claude")
    for team in (codex_team, claude_team):
        _write_team(tmp_path, team)
    window = DateWindow(
        start_date=None,
        end_date=None,
        start_ms=START,
        end_ms=START + 20_000,
        start_time="2026-03-31T23:33:20.000Z",
        end_time="2026-03-31T23:33:40.000Z",
    )
    output = tmp_path / "combined-zero-summary"

    first = build_combined_archive(
        tmp_path,
        (codex_team.team_slug, claude_team.team_slug),
        output=output,
        display_timezone="America/New_York",
        display_window=window,
        rollup_kinds=("hourly",),
    )
    second = build_combined_archive(
        tmp_path,
        (claude_team.team_slug, codex_team.team_slug),
        output=output,
        display_timezone="America/New_York",
        display_window=window,
        rollup_kinds=("hourly",),
    )

    assert first["teams"] == 2
    assert first["files_changed"] > 0
    assert second["files_changed"] == 0
    assert all(
        not (tmp_path / "teams" / team.team_slug / "summary_data").exists()
        for team in (codex_team, claude_team)
    )
    timeline = json.loads(schema_1_timeline_text(output))
    assert {item["slug"] for item in timeline["teams"]} == {
        codex_team.team_slug,
        claude_team.team_slug,
    }
    assert timeline["phases"]
    assert all(phase["phrase"] == "Summary unavailable" for phase in timeline["phases"])
    assert all(phase["summary_available"] is False for phase in timeline["phases"])
    assert all("::" in phase["agent_id"] for phase in timeline["phases"])
    assert all(agent["summary_available"] is False for agent in timeline["agents"])
    assert {rollup["team"] for rollup in timeline["rollups"]} == {
        codex_team.team_slug,
        claude_team.team_slug,
    }
    assert all(rollup["summary_available"] is False for rollup in timeline["rollups"])
    assert timeline["summary_files"] == []
    sparse_rollup = timeline["rollups"][0]
    reference = (
        f"rollup:{sparse_rollup['team']}::{sparse_rollup['kind']}::"
        f"{sparse_rollup['start_ms']}"
    )
    query_result = subprocess.run(
        (str(output / "timeline"), "show", reference),
        cwd=tmp_path,
        check=False,
        capture_output=True,
        text=True,
    )
    assert query_result.returncode == 0, query_result.stderr
    query_item = json.loads(query_result.stdout)["items"][0]
    assert query_item["summary_available"] is False
    assert "technical_markdown" not in query_item
    assert "plain_language_markdown" not in query_item


def test_combined_export_stale_cleanup_cannot_delete_raw_team_data(
    tmp_path: Path,
) -> None:
    raw_relative = "teams/victim/raw/team.json"
    raw_path = tmp_path / raw_relative
    raw_path.parent.mkdir(parents=True)
    raw_path.write_text("valuable raw transcript\n", encoding="utf-8")

    with pytest.raises(ValueError, match="unrecognized generated path"):
        _remove_stale_files(tmp_path, {raw_relative}, set())

    assert raw_path.read_text(encoding="utf-8") == "valuable raw transcript\n"


def test_presentation_stale_cleanup_cannot_delete_raw_team_data(tmp_path: Path) -> None:
    """The single-team build sweeper is as structurally blind to `raw/` as the combined one.

    The twin of `test_combined_export_stale_cleanup_cannot_delete_raw_team_data` above, and it
    exists for the same reason that one does. The retirement of `raw/messages/` argues that a
    sweep of ingest data belongs in ingest because *both* build sweepers are unable to name a path
    under `raw/`; only the `multi_team` half of that claim was pinned by a test, which is exactly
    the half a future refactor is least likely to break silently.
    """

    raw_relative = "teams/victim/raw/messages/00000000-0000-0000-0000-000000000001.json"
    raw_path = tmp_path / raw_relative
    raw_path.parent.mkdir(parents=True)
    raw_path.write_text("valuable ingest data\n", encoding="utf-8")

    with pytest.raises(ValueError, match="unrecognized presentation manifest path"):
        _remove_stale_presentation_files(tmp_path, {raw_relative}, set())

    assert raw_path.read_text(encoding="utf-8") == "valuable ingest data\n"


def test_combined_export_stale_cleanup_rejects_symlinked_summary_parent(
    tmp_path: Path,
) -> None:
    # A directory outside the summaries tree, holding something the sweep must not reach.
    # Deliberately a plain path rather than the build store: what matters is only that it
    # is somewhere else, and routing it through the store would imply a relevance it has
    # none of.
    raw_root = tmp_path / "teams" / "victim" / "raw"
    raw_root.mkdir(parents=True)
    raw_path = raw_root / "valuable.md"
    raw_path.write_text("valuable raw data\n", encoding="utf-8")
    summary_root = tmp_path / "teams" / "victim" / "summaries"
    summary_root.symlink_to(raw_root, target_is_directory=True)

    with pytest.raises(ValueError, match="crosses a symlink"):
        _remove_stale_files(
            tmp_path,
            {"teams/victim/summaries/valuable.md"},
            set(),
        )

    assert raw_path.read_text(encoding="utf-8") == "valuable raw data\n"


def test_combined_export_refuses_unmarked_nonempty_output(tmp_path: Path) -> None:
    team = _team()
    other = replace(team, team_slug="claude-test", provider="claude")
    for item in (team, other):
        _write_team(tmp_path, item)
    output = tmp_path / "not-an-archive"
    output.mkdir()
    (output / "valuable.txt").write_text("keep me\n", encoding="utf-8")

    with pytest.raises(ValueError, match="refusing non-empty non-archive"):
        build_combined_archive(
            tmp_path,
            (team.team_slug, other.team_slug),
            output=output,
            display_timezone="UTC",
        )
    assert (output / "valuable.txt").read_text(encoding="utf-8") == "keep me\n"


def test_agent_name_v1_projection_degrades_without_lifetime_summary(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    child = next(agent for agent in team.agents if agent.thread_id == CHILD)
    path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "agents"
        / f"{CHILD}.json"
    )
    write_json_if_changed(
        path,
        {
            "schema_version": 1,
            "agent": {
                "thread_id": CHILD,
                "official_path": child.agent_path,
                "coordinator_nickname": child.nickname,
                "role": child.role,
                "depth": child.depth,
                "parent_official_path": "/root",
            },
            "name": {
                "thread_id": CHILD,
                "short_name": "Release receipt audit",
                "rationale": "The work audited release receipt binding.",
                "model": "gpt-5.6-sol",
                "prompt_version": "agent-team-timeline-agent-name-v1",
                "input_hash": "legacy-name-hash",
                "generated_at": "2026-08-05T00:00:00Z",
            },
        },
    )

    phases = build_phases(team)
    phase_results = {
        phase.summary_key: SummaryResult(
            key=phase.summary_key,
            phrase="Cached phase",
            paragraph="Cached phase detail.",
            work_summary=(),
            model="test-model",
            prompt_version="test-prompt",
            input_hash=phase.summary_key,
            generated_at="2026-08-05T00:00:00Z",
        )
        for phase in phases
    }
    jobs = _agent_name_jobs(team, phases, phase_results)
    loaded = _load_agent_names(
        tmp_path, team, frozenset({CHILD}), jobs
    )[CHILD]

    assert loaded.short_name == "Release receipt audit"
    assert loaded.lifetime_summary is None

    build_archive(tmp_path, team.team_slug)
    timeline = json.loads(schema_1_timeline_text(tmp_path))
    child_track = next(agent for agent in timeline["agents"] if agent["id"] == CHILD)
    assert child_track["short_name"] == "Release receipt audit"
    assert child_track["summary_available"] is False


def test_spanning_tool_is_not_repeated_before_later_phase_boundary() -> None:
    team = _team()
    long_tool = replace(
        team.tool_calls[0],
        started_at_ms=START + 10_000,
        ended_at_ms=START + 40 * 60 * 1000,
    )
    later = _event(
        "later-child-update",
        CHILD,
        35 * 60 * 1000,
        "assistant_message",
        "The long validation command is still running.",
    )
    updated = replace(team, events=team.events + (later,), tool_calls=(long_tool,))
    for phase in build_phases(updated):
        assert all(phase.start_ms <= entry.at_ms < phase.end_ms for entry in phase.transcript)


def _rollup_result(key: str, phrase: str) -> SummaryResult:
    return SummaryResult(key, phrase, phrase, (), "test", "v1", key, "now")


def test_monthly_rollup_excludes_cross_boundary_week_summary() -> None:
    team = _team()
    month = Period("monthly", "month", "Month", 100, 300, "month.md", False)
    crossing = Period("weekly", "cross", "Cross", 50, 150, "cross.md", False)
    contained = Period("weekly", "inside", "Inside", 150, 250, "inside.md", False)
    stats = PhaseStats(0, 0, 0, 0)

    def phase(key: str, start: int) -> PhaseWindow:
        return PhaseWindow(
            key,
            key,
            ROOT,
            "Coordinator",
            start,
            start + 10,
            stats,
            (),
            key,
            "",
            (),
        )

    boundary_start = phase("boundary-start", 110)
    covered = phase("covered", 160)
    boundary_end = phase("boundary-end", 260)
    phase_results = {
        item.summary_key: _rollup_result(item.summary_key, item.summary_key.upper())
        for item in (boundary_start, covered, boundary_end)
    }
    lower_results = {
        "cross:weekly": _rollup_result("cross", "CROSS_WEEK"),
        "inside:weekly": _rollup_result("inside", "CONTAINED_WEEK"),
    }
    job = _rollup_jobs_for_level(
        team,
        (month,),
        (boundary_start, covered, boundary_end),
        phase_results,
        (crossing, contained),
        lower_results,
        (),
        (),
    )[0]
    assert "CROSS_WEEK" not in job.transcript
    assert "CONTAINED_WEEK" in job.transcript
    assert "BOUNDARY-START" in job.transcript
    assert "BOUNDARY-END" in job.transcript
    assert "COVERED" not in job.transcript


def test_plain_rollup_gets_overview_and_supported_definitions_only() -> None:
    team = _team()
    period = Period("daily", "day", "Day", START, START + 30_000, "day.md", False)
    phases = build_phases(team)
    phase_results = {
        phase.summary_key: _rollup_result(phase.summary_key, phase.summary_key)
        for phase in phases
    }
    supported = GlossaryTerm(
        "exact-head",
        START,
        2,
        "First-use evidence for exact-head.",
        "2026-W13",
        glossary_term_id("exact-head"),
        "A release check that requires one exact revision.",
        "supported",
    )
    unsupported = GlossaryTerm(
        "DBI",
        START,
        2,
        "DBI first-use evidence.",
        "2026-W13",
        glossary_term_id("DBI"),
        "Insufficient evidence: DBI was never expanded.",
        "insufficient-evidence",
    )

    technical = _rollup_jobs_for_level(
        team,
        (period,),
        phases,
        phase_results,
        (),
        {},
        (),
        (supported, unsupported),
    )[0]
    same_period_technical = _rollup_result(
        "rollup:daily:day",
        "The exact-head work was validated but still awaited review; it did not land.",
    )
    plain = _rollup_jobs_for_level(
        team,
        (period,),
        phases,
        phase_results,
        (),
        {},
        (),
        (supported, unsupported),
        PLAIN_LANGUAGE_ROLLUP_STYLE,
        _rollup_result(
            "project-overview",
            "Widget runs guest software in a repeatable environment.",
        ),
        {"day:daily": same_period_technical},
    )[0]
    without_definitions = tuple(
        replace(term, definition="", definition_status="unavailable")
        for term in (supported, unsupported)
    )
    technical_without_definitions = _rollup_jobs_for_level(
        team,
        (period,),
        phases,
        phase_results,
        (),
        {},
        (),
        without_definitions,
    )[0]

    assert technical.glossary == technical_without_definitions.glossary
    assert "Widget runs guest software" in plain.glossary
    assert "A release check that requires one exact revision" in plain.glossary
    assert "DBI" not in plain.glossary
    assert "did not land" in plain.factual_context
    assert same_period_technical.input_hash in plain.dependency_keys
    assert any(
        component.name == "technical_summary"
        and component.requested == 1
        and component.provided == 1
        for component in plain.context_coverage.components
    )


def test_build_ignores_retired_glossary_schema(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    glossary_path = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summary_data"
        / "glossary.json"
    )
    write_json_if_changed(glossary_path, narrow_json({"schema_version": 1, "terms": []}))
    before = glossary_path.read_bytes()

    build_archive(tmp_path, team.team_slug)

    timeline = json.loads(schema_1_timeline_text(tmp_path))
    assert timeline["glossary"] == []
    assert glossary_path.read_bytes() == before


def test_build_excludes_legacy_glossary_without_mutating_immutable_data(
    tmp_path: Path,
) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    summary_root = tmp_path / "teams" / team.team_slug / "summary_data"
    glossary_path = summary_root / "glossary.json"
    write_json_if_changed(
        glossary_path,
        narrow_json(
            {
                "schema_version": 3,
                "terms": [{"term": "and", "definition": "Retired junk."}],
            }
        ),
    )
    glossary_before = glossary_path.read_bytes()
    legacy_glossary = json.loads(glossary_before)
    assert legacy_glossary["schema_version"] == 3
    assert legacy_glossary["terms"]
    cache_before = {
        path.relative_to(summary_root): path.read_bytes()
        for path in sorted((summary_root / "cache").glob("*.json"))
    }
    assert cache_before
    stale_week = (
        tmp_path
        / "teams"
        / team.team_slug
        / "summaries"
        / "glossary"
        / "2026"
        / f"2026-W31-{team.team_slug}-glossary.md"
    )
    stale_week.parent.mkdir(parents=True, exist_ok=True)
    stale_week.write_text("# stale retired glossary\n", encoding="utf-8")
    user_notes = stale_week.parent / f"notes-{team.team_slug}-glossary.md"
    user_notes.write_text("# user-owned notes\n", encoding="utf-8")

    build_archive(tmp_path, team.team_slug)

    timeline = json.loads(schema_1_timeline_text(tmp_path))
    assert timeline["glossary"] == []
    assert not stale_week.exists()
    assert user_notes.read_text(encoding="utf-8") == "# user-owned notes\n"
    assert glossary_path.read_bytes() == glossary_before
    assert {
        path.relative_to(summary_root): path.read_bytes()
        for path in sorted((summary_root / "cache").glob("*.json"))
    } == cache_before


def test_build_refuses_symlinked_generated_glossary_directory(tmp_path: Path) -> None:
    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "offline")
    glossary_root = (
        tmp_path / "teams" / team.team_slug / "summaries" / "glossary"
    )
    glossary_root.mkdir(parents=True)
    outside = tmp_path / "outside"
    outside.mkdir()
    (glossary_root / "2026").symlink_to(outside, target_is_directory=True)

    with pytest.raises(ValueError, match="refusing symlink in generated glossary"):
        build_archive(tmp_path, team.team_slug)


def test_calendar_periods_use_local_dst_boundaries() -> None:
    # 2026-03-08 is the US spring-forward day: local midnight-to-midnight is 23 hours.
    first_ms = 1_772_946_000_000  # 2026-03-08T05:00:00Z == midnight EST
    last_ms = first_ms + 22 * 60 * 60 * 1000
    periods = periods_for_range(first_ms, last_ms, "America/New_York", "dst-test")
    daily = next(period for period in periods if period.kind == "daily")
    assert daily.end_ms - daily.start_ms == 23 * 60 * 60 * 1000


def test_loopback_server_serves_json_with_safe_headers(tmp_path: Path) -> None:
    (tmp_path / "index.html").write_text("ok", encoding="utf-8")
    server = make_server(tmp_path, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = int(server.server_address[1])
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=5) as response:
            assert response.read() == b"ok"
            assert response.headers["X-Content-Type-Options"] == "nosniff"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def _tear_normalized_generation(archive: Path, team_slug: str) -> None:
    """Leave a team in the exact state a crash between two ingest writes leaves it in.

    ``_ingest_orc_locked`` writes ``raw/source-manifest.json`` and only afterwards
    ``raw/normalized-generation.json``. A process killed between those two lines -- an OOM, a
    scheduler timeout, a host reboot -- leaves a team whose ``raw/team.json`` is perfectly readable
    and whose generation marker says nothing, which is precisely what
    ``_validate_normalized_generation`` is there to refuse.
    """

    write_json_if_changed(
        team_build_root(archive, team_slug) / "raw" / "source-manifest.json",
        narrow_json(
            {
                "schema_version": 2,
                "provider": "orc",
                "root_session_id": ROOT,
                "source_root": "/nonexistent/state",
                "snapshot_root": f"teams/{team_slug}/source_snapshots",
                "date_window": None,
                "sources": [],
            }
        ),
    )


def test_transcript_extraction_carries_a_torn_team_and_still_projects_the_healthy_one(
    tmp_path: Path,
) -> None:
    """One team torn between its two ingest writes must not withhold the others' new prompts.

    This is the archive-level half of the fix: the failure is produced by real
    ``load_archived_team`` validation against real files on disk, not by a fake, so it exercises
    the classification and the skip together.
    """

    healthy = _team()
    torn = replace(_team(), team_slug="orc-test", provider="orc")
    _write_team(tmp_path, healthy)
    _write_team(tmp_path, torn)

    whole = extract_transcripts_archive(tmp_path)
    assert whole.teams == 2
    assert whole.partial is False

    _tear_normalized_generation(tmp_path, "orc-test")
    grown = replace(
        healthy,
        events=(
            *healthy.events,
            _event("prompt-2", ROOT, 30_000, "user_prompt", "A brand new question."),
        ),
    )
    _write_team(tmp_path, grown)

    partial = extract_transcripts_archive(tmp_path)

    assert partial.teams == 1
    assert partial.partial is True
    assert [skip.team_slug for skip in partial.skipped_teams] == ["orc-test"]
    assert partial.skipped_teams[0].error_type == "ValueError"
    assert "incomplete Orc normalized generation" in partial.skipped_teams[0].error
    # A ValueError is a classified data fault, so no traceback is kept for it.
    assert partial.skipped_teams[0].traceback is None

    prompts = [
        json.loads(line)
        for line in (tmp_path / "extracted" / "transcripts" / "prompts.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    texts = {str(record["text"]) for record in prompts}
    assert "A brand new question." in texts
    assert {
        team for record in prompts for team in record["occurrence_teams"]
    } == {"codex-test", "orc-test"}

    # Repairing the team needs no extraction-side intervention.
    write_json_if_changed(
        team_build_root(tmp_path, "orc-test") / "raw" / "source-manifest.json",
        narrow_json({"schema_version": 1, "provider": "codex", "sources": []}),
    )
    assert extract_transcripts_archive(tmp_path).partial is False


def test_transcript_extraction_still_fails_when_no_team_can_be_read(
    tmp_path: Path,
) -> None:
    """An archive nobody can read is the one state not to rewrite the corpus in.

    Every record would be carried forward, so the projection would gain nothing, and it would be
    written while the archive is in a state that no reader can validate. The refusal names every
    team and every cause, because "no ingested teams found" would send the operator looking for a
    missing directory rather than a torn one.
    """

    _write_team(tmp_path, replace(_team(), team_slug="orc-one", provider="orc"))
    _write_team(tmp_path, replace(_team(), team_slug="orc-two", provider="orc"))
    extract_transcripts_archive(tmp_path)
    _tear_normalized_generation(tmp_path, "orc-one")
    _tear_normalized_generation(tmp_path, "orc-two")

    with pytest.raises(ValueError) as raised:
        extract_transcripts_archive(tmp_path)
    message = str(raised.value)
    assert "no archive team" in message
    assert "orc-one: ValueError: incomplete Orc normalized generation" in message
    assert "orc-two: ValueError: incomplete Orc normalized generation" in message


def test_extract_transcripts_cli_exits_two_and_names_the_team_it_carried(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """A partial projection that exits 0 is a silent data gap in every scheduled caller."""

    _write_team(tmp_path, _team())
    _write_team(tmp_path, replace(_team(), team_slug="orc-test", provider="orc"))
    assert timeline_main(("extract-transcripts", "--output", str(tmp_path))) == 0
    capsys.readouterr()

    _tear_normalized_generation(tmp_path, "orc-test")

    assert timeline_main(("extract-transcripts", "--output", str(tmp_path))) == 2
    captured = capsys.readouterr()
    assert "across 1 teams (1 further team(s) carried forward unread)" in captured.out
    assert "team orc-test: ValueError: incomplete Orc normalized generation" in (
        captured.err
    )
    assert "carried forward unchanged from its last good extraction" in captured.err
    # The JSONL is still named, because unlike a failed extraction it really was refreshed.
    assert "JSONL:" in captured.out

    run = json.loads(
        sorted((tmp_path / "runs").glob("*.json"))[-1].read_text(encoding="utf-8")
    )
    assert run["status"] == "failed"
    assert "1 of 2 archive teams could not be read" in run["error"]
    extraction = run["mechanical"]["transcript_extraction"]
    assert extraction["teams"] == 1
    assert extraction["teams_skipped"] == 1
    assert extraction["skipped_teams"][0]["team_slug"] == "orc-test"
    assert extraction["dropped_prompt_authorship_rules"] == []


def test_a_real_build_is_read_through_schema_three(tmp_path: Path) -> None:
    """The end-to-end the fixture tests cannot reach: a build, then a query over it.

    Everything else that exercises the schema-3 read path constructs the three generations by
    calling the writers directly, which proves the reader and the writer agree and proves
    nothing about the *renderer*. Two things only a real build produces are checked here.

    The first is the single-team case. `render` does not stamp a ``team`` field on every record
    -- there is only one team, and schema 1 does not carry it -- so a spine record can arrive
    without one, and a reader that assumed the field would raise on the very archive shape most
    installations have.

    The second is that the completeness rule accepts what the build actually writes. The rule is
    a conjunction of four clauses about files and byte counts, and it is easy to write one that
    is correct about corruption and wrong about a healthy archive; the symptom would be a silent
    permanent fallback to schema 2, with every byte-count claim in
    `test_query_schema_3.py` still passing.
    """

    team = _team()
    _write_team(tmp_path, team)
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")
    build_archive(tmp_path, team.team_slug)

    query = TimelineQuery(tmp_path)
    assert query.schema_3_declined == ""
    agents = query.list_records("agents", QueryFilters())
    assert agents
    assert all(record["team"] == team.team_slug for record in agents)

    # And the answers are the schema-2 answers. A build no longer writes schema 2, so the middle
    # generation is produced here the way the archives that still contain one were produced -- by
    # the writer that made them, over this build's own records. Hiding the schema-3 entry point is
    # then the whole difference between the two readings, so the comparison is over one tree.
    write_legacy_schema_2(tmp_path)
    (tmp_path / SCHEMA_3_BOOTSTRAP_PATH).rename(tmp_path / "hidden-timeline-v3.json")
    fallback = TimelineQuery(tmp_path)
    assert fallback.schema_3_declined == "no schema-3 bootstrap"
    assert fallback.list_records("agents", QueryFilters()) == agents
    assert fallback.list_records("phases", QueryFilters()) == query.list_records(
        "phases", QueryFilters()
    )
    assert fallback.show(str(agents[0]["ref"])) == query.show(str(agents[0]["ref"]))
    assert query.bytes_read < fallback.bytes_read

    # And so is schema 1, the last fallback. A single-team export puts no `team` field on a
    # phase, a rollup or a summary file, and every reference in the query surface is
    # team-qualified, so this is the reading that used to raise `expected a string` from the
    # middle of the loader on the archive shape most exports have.
    #
    # A published build no longer writes the monolith, so it is produced here the one way the
    # tool still produces one -- the combiner's unpublished render -- and dropped into the tree.
    # Reading it back is what keeps this a comparison between three generations of the *same*
    # build rather than between two builds.
    intermediate = tmp_path.parent / "intermediate"
    build_archive(tmp_path, team.team_slug, output=intermediate, _published=False)
    (tmp_path / "data" / "timeline.json").write_bytes(
        (intermediate / "data" / "timeline.json").read_bytes()
    )
    (tmp_path / "data" / "timeline-v2.json").rename(tmp_path / "hidden-timeline-v2.json")
    oldest = TimelineQuery(tmp_path)
    assert oldest.list_records("agents", QueryFilters()) == agents
    assert oldest.list_records("rollups", QueryFilters()) == query.list_records(
        "rollups", QueryFilters()
    )
    assert oldest.activity_bins(QueryFilters()) == query.activity_bins(QueryFilters())


# --- the rename must not orphan an archive built before it ---------------------------------------


def test_an_archive_built_before_the_rename_is_still_an_archive(tmp_path: Path) -> None:
    """`.agent-team-timeline.json` saying `"tool": "agent-team-timeline"` must still be recognised.

    Both the marker's NAME and the `tool` value inside it carried the old name, so a rename that
    changed only the code would have made every existing archive unrecognisable. The failure would
    not even have looked like a rename: `_ensure_archive` would fall through to its non-empty-
    directory guard and refuse with "refusing non-empty non-archive output directory", which reads
    as a safety refusal about the operator's own data.
    """

    from wrkviz.archive import (
        ARCHIVE_MARKER_FILE,
        LEGACY_ARCHIVE_MARKER_FILE,
        archive_marker_path,
        is_archive_marker,
    )

    archive = tmp_path / "built-before-the-rename"
    archive.mkdir()
    (archive / LEGACY_ARCHIVE_MARKER_FILE).write_text(
        '{\n  "schema_version": 1,\n  "tool": "agent-team-timeline"\n}\n', encoding="utf-8"
    )

    assert archive_marker_path(archive).name == LEGACY_ARCHIVE_MARKER_FILE
    assert is_archive_marker({"schema_version": 1, "tool": "agent-team-timeline"})
    assert is_archive_marker({"schema_version": 1, "tool": "wrkviz"})
    # Still not a licence to accept anything: a foreign marker is refused as before.
    assert not is_archive_marker({"schema_version": 1, "tool": "something-else"})
    assert not is_archive_marker({"schema_version": 2, "tool": "wrkviz"})

    # A fresh archive is created under the CURRENT name, not the one it is compatible with.
    fresh = tmp_path / "fresh"
    fresh.mkdir()
    assert archive_marker_path(fresh).name == ARCHIVE_MARKER_FILE


def test_a_build_migrates_a_pre_rename_marker_and_leaves_no_second_one(tmp_path: Path) -> None:
    """Migration happens on the next build, not through a command an operator must know about.

    The marker is two hundred bytes and the build already holds the writer lock, so there is no
    reason to make somebody run a migration for it -- and every reason not to leave two markers
    behind, since a directory carrying both is ambiguous about which one a reader should trust.
    """

    team = _team()
    _write_team(tmp_path, team)
    output = tmp_path / "site"
    output.mkdir()
    (output / ".agent-team-timeline.json").write_text(
        '{"schema_version": 1, "tool": "agent-team-timeline"}\n', encoding="utf-8"
    )

    build_archive(tmp_path, team.team_slug, output=output)

    assert (output / ".wrkviz.json").is_file()
    assert not (output / ".agent-team-timeline.json").exists(), (
        "the pre-rename marker must be removed, not left beside the new one"
    )
