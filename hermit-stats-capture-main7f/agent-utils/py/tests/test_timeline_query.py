"""Read-only CLI navigation for built timeline archives."""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

import pytest

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    narrow_json,
    read_json,
)
from wrkviz.cli import main as timeline_main
from wrkviz.query import _compile_search_matcher
from wrkviz.search_bloom import (
    bloom_might_contain,
    build_trigram_bloom,
    compact_search_text,
)
from wrkviz.timeline_shards import write_timeline_shards
from tests.timeline_legacy_generations import schema_2_writer_enabled


START = 1_786_068_000_000


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def _site(tmp_path: Path) -> Path:
    root = tmp_path / "site"
    detail_path = "data/details/alpha/phase-work.json"
    technical_path = "teams/alpha/summaries/hourly/technical.md"
    plain_path = "teams/alpha/summaries/hourly/plain.md"
    timeline = {
        "schema_version": 1,
        "generated_at": "2026-08-07T03:00:00Z",
        "source_digest": "query-fixture",
        "display_timezone": "America/New_York",
        "display_timezone_source": "explicit",
        "range": {"start_ms": START, "end_ms": START + 10_800_000},
        "teams": [
            {
                "slug": "alpha",
                "label": "Alpha team",
                "provider": "codex",
                "projects": [],
                "hosts": [],
                "stats": {"active_agents": 2},
            },
            {
                "slug": "beta",
                "label": "Beta team",
                "provider": "claude",
                "projects": [],
                "hosts": [],
                "stats": {"active_agents": 1},
            },
        ],
        "agents": [
            {
                "id": "alpha::root",
                "team": "alpha",
                "parent_id": None,
                "short_name": "Coordinator",
                "official_name": "/root",
                "nickname": "",
                "lifetime_summary": "Coordinated the reproducible build investigation.",
                "naming_rationale": "Root coordinator",
                "depth": 0,
                "status": "completed",
                "start_ms": START,
                "end_ms": START + 3_600_000,
            },
            {
                "id": "alpha::child",
                "team": "alpha",
                "parent_id": "root",
                "short_name": "Build verifier",
                "official_name": "/root/build_verifier",
                "nickname": "Ada",
                "lifetime_summary": "Verified deterministic GHC build output.",
                "naming_rationale": "Completed verification work",
                "depth": 1,
                "status": "completed",
                "start_ms": START + 60_000,
                "end_ms": START + 1_800_000,
            },
            {
                "id": "beta::root",
                "team": "beta",
                "parent_id": None,
                "short_name": "Coordinator",
                "official_name": "/root",
                "nickname": "",
                "lifetime_summary": "Reviewed deployment logs.",
                "naming_rationale": "Root coordinator",
                "depth": 0,
                "status": "completed",
                "start_ms": START + 7_200_000,
                "end_ms": START + 10_800_000,
            },
        ],
        "phases": [
            {
                "id": "alpha::phase-work",
                "team": "alpha",
                "agent_id": "alpha::child",
                "detail_path": detail_path,
                "phrase": "Verified reproducible GHC builds.",
                "paragraph": "The verifier compared two builds and found identical artifacts.",
                "stats": {"tool_calls": 3},
                "states": [],
                "start_ms": START + 60_000,
                "end_ms": START + 1_800_000,
            }
        ],
        "rollups": [
            {
                "team": "alpha",
                "kind": "hourly",
                "label": "Thu Aug 6 · 22:00",
                "technical_path": technical_path,
                "plain_language_path": plain_path,
                "start_ms": START,
                "end_ms": START + 3_600_000,
            }
        ],
        "activity_bins": [],
        "edges": [],
        "events": [],
        "glossary": [],
        "summary_files": [],
        "artifact_catalog_path": "data/artifacts.json",
        "project_overviews": [],
    }
    _write_json(root / "data" / "timeline.json", timeline)
    _write_json(
        root / detail_path,
        {
            "team": "alpha",
            "phrase": "Verified reproducible GHC builds.",
            "paragraph": "The verifier compared two builds.",
            "stats": {"tool_calls": 3},
            "transcript": [
                {
                    "at_ms": START + 120_000,
                    "role": "assistant",
                    "text": "The GHC output hashes match exactly.",
                    "tools": [],
                    "pull_requests": [],
                },
                {
                    "at_ms": START + 180_000,
                    "role": "assistant",
                    "text": "The second GHC check is outside the requested boundary.",
                    "tools": [],
                    "pull_requests": [],
                },
            ],
        },
    )
    technical = root / technical_path
    technical.parent.mkdir(parents=True, exist_ok=True)
    technical.write_text(
        "# Technical\n\nReproducible GHC builds passed.\n", encoding="utf-8"
    )
    plain = root / plain_path
    plain.write_text(
        "# Plain language\n\nTwo builds produced the same files.\n", encoding="utf-8"
    )
    search_records: list[dict[str, JsonValue]] = [
        {
            "schema_version": 1,
            "ref": "message:alpha::owner-b3",
            "record_type": "prompt",
            "role": "user",
            "team": "alpha",
            "agent_id": "alpha::root",
            "agent_ref": "agent:alpha::root",
            "agent_path": "/root",
            "event_id": "owner-b3",
            "turn_id": "turn-b3",
            "at_ms": START + 90_000,
            "text": "What does backend maturity B3 require?",
            "author_kind": "owner_human",
            "ingress_kind": "web",
            "prompt_ref": "message:alpha::owner-b3",
            "prompt_author_kind": "owner_human",
            "content_fidelity": "verbatim",
        },
        {
            "schema_version": 1,
            "ref": "message:alpha::answer-b3",
            "record_type": "response",
            "role": "assistant",
            "team": "alpha",
            "agent_id": "alpha::child",
            "agent_ref": "agent:alpha::child",
            "agent_path": "/root/build_verifier",
            "event_id": "answer-b3",
            "turn_id": "turn-b3",
            "at_ms": START + 120_000,
            "text": "B3 means 50% or more of the ptrace corpus passes.",
            "author_kind": "agent",
            "ingress_kind": "codex",
            "prompt_ref": "message:alpha::owner-b3",
            "prompt_author_kind": "owner_human",
            "content_fidelity": "verbatim",
        },
        {
            "schema_version": 1,
            "ref": "message:alpha::hash-noise",
            "record_type": "response",
            "role": "assistant",
            "team": "alpha",
            "agent_id": "alpha::child",
            "agent_ref": "agent:alpha::child",
            "agent_path": "/root/build_verifier",
            "event_id": "hash-noise",
            "turn_id": None,
            "at_ms": START + 180_000,
            "text": "Commit 91b3a19 contains no maturity grade.",
            "author_kind": "agent",
            "ingress_kind": "codex",
            "prompt_ref": None,
            "prompt_author_kind": None,
            "content_fidelity": "verbatim",
        },
    ]
    # An archive an older tool wrote: schema 2 and nothing else. That is the whole point of this
    # fixture -- `query.py` still reads this generation, for archives nobody has rebuilt, and the
    # only honest way to produce one is to ask the writer that produced them.
    with schema_2_writer_enabled():
        write_timeline_shards(
            root,
            as_object(narrow_json(timeline), "timeline"),
            search_records=search_records,
        )
    return root


def _response(capsys: pytest.CaptureFixture[str]) -> dict[str, JsonValue]:
    return as_object(narrow_json(json.loads(capsys.readouterr().out)), "response")


def _items(response: dict[str, JsonValue]) -> list[dict[str, JsonValue]]:
    return [
        as_object(value, f"response.items[{index}]")
        for index, value in enumerate(as_array(response["items"], "response.items"))
    ]


def _search_counts(records: list[dict[str, JsonValue]]) -> dict[str, JsonValue]:
    record_types = [
        as_string(record.get("record_type"), "search record.record_type")
        for record in records
    ]
    return {
        "records": len(records),
        "prompts": sum(value == "prompt" for value in record_types),
        "responses": sum(value == "response" for value in record_types),
        "inter_agent": sum(value.startswith("inter_agent") for value in record_types),
        "tools": sum(value == "tool" for value in record_types),
    }


def _write_test_object(root: Path, value: dict[str, JsonValue]) -> dict[str, JsonValue]:
    content = canonical_json(value)
    encoded = content.encode("utf-8")
    digest = hashlib.sha256(encoded).hexdigest()
    relative = f"data/timeline-v2/objects/{digest}.json"
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encoded)
    return {
        "url": relative,
        "sha256": digest,
        "bytes": len(encoded),
        "gzip_bytes": None,
    }


def _rewrite_search_records(
    root: Path, mutate: Callable[[list[dict[str, JsonValue]]], None]
) -> None:
    """Rewrite the fixture's search shard and keep its content-addressed catalog valid."""

    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    search = as_object(bootstrap.get("search"), "timeline-v2.search")
    shards = as_array(search.get("shards"), "timeline-v2.search.shards")
    assert len(shards) == 1
    shard = as_object(shards[0], "timeline-v2.search.shards[0]")
    old_relative = as_string(shard.get("url"), "search shard.url")
    search_object = as_object(read_json(root / old_relative), "search shard")
    raw_records = as_array(search_object.get("records"), "search shard.records")
    records = [
        as_object(value, f"search shard.records[{index}]")
        for index, value in enumerate(raw_records)
    ]
    mutate(records)
    search_object["records"] = [record for record in records]

    content = canonical_json(search_object)
    encoded = content.encode("utf-8")
    digest = hashlib.sha256(encoded).hexdigest()
    relative = f"data/timeline-v2/objects/{digest}.json"
    object_path = root / relative
    object_path.parent.mkdir(parents=True, exist_ok=True)
    object_path.write_bytes(encoded)

    counts = _search_counts(records)
    shard["url"] = relative
    shard["sha256"] = digest
    shard["bytes"] = len(encoded)
    shard["gzip_bytes"] = None
    shard["counts"] = counts
    shard.pop("linkage", None)
    shard["trigram_bloom"] = build_trigram_bloom(
        as_string(record.get("text"), "search record.text") for record in records
    ).catalog_obj()
    search["counts"] = counts
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")


def _rewrite_content_addressed_object(
    root: Path,
    reference: dict[str, JsonValue],
    where: str,
    mutate: Callable[[dict[str, JsonValue]], None],
) -> None:
    old_relative = as_string(reference.get("url"), where + ".url")
    record = as_object(read_json(root / old_relative), old_relative)
    mutate(record)
    content = canonical_json(record)
    encoded = content.encode("utf-8")
    digest = hashlib.sha256(encoded).hexdigest()
    relative = f"data/timeline-v2/objects/{digest}.json"
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encoded)
    reference["url"] = relative
    reference["sha256"] = digest
    reference["bytes"] = len(encoded)
    reference["gzip_bytes"] = None


def _rewrite_schema_2_object(
    root: Path,
    field: str,
    mutate: Callable[[dict[str, JsonValue]], None],
) -> None:
    """Rewrite one content-addressed root object and repoint the bootstrap."""

    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    reference = as_object(bootstrap.get(field), f"timeline-v2.{field}")
    _rewrite_content_addressed_object(root, reference, f"timeline-v2.{field}", mutate)
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")


def _search_record(
    *,
    event_id: str,
    record_type: str,
    role: str,
    text: str,
    at_ms: int,
    agent_id: str = "root",
    author_kind: str | None = "agent",
    prompt_ref: str | None = None,
    prompt_author_kind: str | None = None,
) -> dict[str, JsonValue]:
    agent_path = "/root" if agent_id == "root" else f"/root/{agent_id}"
    return {
        "schema_version": 1,
        "ref": f"message:alpha::{event_id}",
        "record_type": record_type,
        "role": role,
        "team": "alpha",
        "agent_id": f"alpha::{agent_id}",
        "agent_ref": f"agent:alpha::{agent_id}",
        "agent_path": agent_path,
        "event_id": event_id,
        "turn_id": f"turn-{event_id}",
        "at_ms": at_ms,
        "text": text,
        "author_kind": author_kind,
        "ingress_kind": "test",
        "prompt_ref": prompt_ref,
        "prompt_author_kind": prompt_author_kind,
        "content_fidelity": "verbatim",
    }


def test_list_uses_export_independent_stable_refs_and_filters(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 0
    teams = _items(_response(capsys))
    assert [item["ref"] for item in teams] == ["team:alpha", "team:beta"]

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "list",
                "agents",
                "--team",
                "alpha",
                "--end-time",
                "2026-08-07T03:00:00Z",
            )
        )
        == 0
    )
    agents = _items(_response(capsys))
    assert [item["ref"] for item in agents] == [
        "agent:alpha::root",
        "agent:alpha::child",
    ]
    assert agents[1]["parent_ref"] == "agent:alpha::root"

    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 0
    phases = _items(_response(capsys))
    assert phases[0]["stats"] == {"tool_calls": 3}

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "list",
                "rollups",
                "--kind",
                "daily",
            )
        )
        == 0
    )
    assert _items(_response(capsys)) == []


def test_show_expands_relationships_rollups_and_optional_transcript(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    assert (
        timeline_main(("query", "--output", str(root), "show", "agent:alpha::root"))
        == 0
    )
    agent = _items(_response(capsys))[0]
    assert agent["child_refs"] == ["agent:alpha::child"]

    phase_ref = "phase:alpha::phase-work"
    assert timeline_main(("query", "--output", str(root), "show", phase_ref)) == 0
    phase = _items(_response(capsys))[0]
    detail = phase["detail"]
    assert isinstance(detail, dict)
    assert "transcript" not in detail

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "show",
                phase_ref,
                "--transcript",
            )
        )
        == 0
    )
    expanded = _items(_response(capsys))[0]
    expanded_detail = expanded["detail"]
    assert isinstance(expanded_detail, dict)
    assert "transcript" in expanded_detail

    rollup_ref = f"rollup:alpha::hourly::{START}"
    assert timeline_main(("query", "--output", str(root), "show", rollup_ref)) == 0
    rollup = _items(_response(capsys))[0]
    assert "Reproducible GHC" in str(rollup["technical_markdown"])
    assert "same files" in str(rollup["plain_language_markdown"])


def test_sparse_summaries_remain_queryable_without_markdown(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    timeline_path = root / "data" / "timeline.json"
    timeline = as_object(read_json(timeline_path), "timeline")
    agents = [
        as_object(value, f"timeline.agents[{index}]")
        for index, value in enumerate(as_array(timeline["agents"], "timeline.agents"))
    ]
    child = next(record for record in agents if record["id"] == "alpha::child")
    child["summary_available"] = False
    phase = as_object(as_array(timeline["phases"], "timeline.phases")[0], "phase")
    phase["summary_available"] = False
    rollup = as_object(as_array(timeline["rollups"], "timeline.rollups")[0], "rollup")
    technical_path = as_string(rollup["technical_path"], "rollup.technical_path")
    plain_path = as_string(rollup["plain_language_path"], "rollup.plain_language_path")
    rollup["summary_available"] = False
    rollup["technical_summary_available"] = False
    rollup["plain_language_summary_available"] = False
    rollup["technical_path"] = ""
    rollup["plain_language_path"] = ""
    _write_json(timeline_path, timeline)
    (root / technical_path).unlink()
    (root / plain_path).unlink()
    (root / "data" / "timeline-v2.json").unlink()

    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 0
    listed_phase = _items(_response(capsys))[0]
    assert listed_phase["summary_available"] is False

    rollup_ref = f"rollup:alpha::hourly::{START}"
    assert timeline_main(("query", "--output", str(root), "show", rollup_ref)) == 0
    shown_rollup = _items(_response(capsys))[0]
    assert shown_rollup["summary_available"] is False
    assert "technical_markdown" not in shown_rollup
    assert "plain_language_markdown" not in shown_rollup

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "GHC",
                "--scope",
                "summaries",
                "--agent",
                "agent:alpha::child",
            )
        )
        == 0
    )
    assert _items(_response(capsys)) == []

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "GHC",
                "--scope",
                "transcripts",
                "--agent",
                "agent:alpha::child",
            )
        )
        == 0
    )
    assert len(_items(_response(capsys))) == 2


def test_search_supports_summary_transcript_agent_and_jsonl_output(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "ghc",
                "--scope",
                "all",
                "--team",
                "alpha",
                "--agent",
                "agent:alpha::child",
                "--start-time",
                "2026-08-07T02:00:00Z",
                "--end-time",
                "2026-08-07T02:03:00Z",
            )
        )
        == 0
    )
    matches = _items(_response(capsys))
    assert {item["record_type"] for item in matches} == {
        "agent",
        "phase",
        "transcript",
    }
    assert all(item["team"] == "alpha" for item in matches)
    assert sum(item["record_type"] == "transcript" for item in matches) == 1

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "--format",
                "jsonl",
                "search",
                "same files",
            )
        )
        == 0
    )
    output = capsys.readouterr().out
    lines = output.splitlines()
    assert len(lines) == 1
    line = as_object(narrow_json(json.loads(lines[0])), "JSONL record")
    assert line["record_type"] == "rollup"


def test_search_v2_finds_owner_prompts_and_linked_agent_responses_without_hash_noise(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "B3",
                "--in",
                "agent-responses",
                "--prompt-author",
                "owner",
            )
        )
        == 0
    )
    response = _response(capsys)
    assert response["total_matches"] == 1
    assert response["returned"] == 1
    assert response["truncated"] is False
    items = _items(response)
    assert items[0]["ref"] == "message:alpha::answer-b3"
    assert items[0]["prompt_ref"] == "message:alpha::owner-b3"
    assert items[0]["prompt_excerpt"] == "What does backend maturity B3 require?"
    assert items[0]["phase_ref"] == "phase:alpha::phase-work"

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "B3",
                "--in",
                "all-transcript",
                "--match",
                "literal",
            )
        )
        == 0
    )
    literal = _response(capsys)
    assert literal["total_matches"] == 3
    assert {item["ref"] for item in _items(literal)} == {
        "message:alpha::owner-b3",
        "message:alpha::answer-b3",
        "message:alpha::hash-noise",
    }


def test_search_v2_bloom_skips_a_definite_miss_without_fetching_its_object(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    search = as_object(bootstrap.get("search"), "timeline-v2.search")
    shards = as_array(search.get("shards"), "timeline-v2.search.shards")
    assert len(shards) == 1
    first = as_object(shards[0], "timeline-v2.search.shards[0]")
    phantom_start = as_int(first.get("start_ms"), "search shard.start_ms") + 86_400_000
    phantom_end = as_int(first.get("end_ms"), "search shard.end_ms") + 86_400_000
    missing_digest = "f" * 64
    phantom_linkage = _write_test_object(
        root,
        {
            "schema_version": 1,
            "kind": "timeline-search-links-day",
            "source_digest": "query-fixture",
            "team": "alpha",
            "range": {"start_ms": phantom_start, "end_ms": phantom_end},
            "prompts": [],
            "responses": [],
        },
    )
    phantom_linkage["counts"] = {"prompts": 0, "responses": 0}
    shards.append(
        {
            "kind": "utc-day",
            "team": "alpha",
            "day": "2026-08-08",
            "start_ms": phantom_start,
            "end_ms": phantom_end,
            "url": f"data/timeline-v2/objects/{missing_digest}.json",
            "sha256": missing_digest,
            "bytes": 1,
            "gzip_bytes": None,
            "counts": {
                "records": 1,
                "prompts": 0,
                "responses": 1,
                "inter_agent": 0,
                "tools": 0,
            },
            "linkage": phantom_linkage,
            "trigram_bloom": build_trigram_bloom(
                ("unrelated deployment logs",)
            ).catalog_obj(),
        }
    )
    bootstrap_range = as_object(bootstrap.get("range"), "timeline-v2.range")
    bootstrap_range["end_ms"] = phantom_end
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")
    timeline_path = root / "data" / "timeline.json"
    timeline = as_object(read_json(timeline_path), "timeline")
    timeline_range = as_object(timeline.get("range"), "timeline.range")
    timeline_range["end_ms"] = phantom_end
    timeline_path.write_text(canonical_json(timeline), encoding="utf-8")

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "backend maturity B3",
                "--in",
                "all-transcript",
            )
        )
        == 0
    )
    response = _response(capsys)
    assert response["total_matches"] == 1
    assert _items(response)[0]["ref"] == "message:alpha::owner-b3"


def test_search_matcher_and_bloom_share_ascii_case_normalization() -> None:
    ascii_matcher = _compile_search_matcher(
        "Kelvin", match_mode="literal", case_sensitive=False
    )
    assert ascii_matcher.match("KELVIN") is not None
    assert ascii_matcher.match("Kelvin") is None
    ascii_filter = build_trigram_bloom(("KELVIN",))
    assert all(
        bloom_might_contain(ascii_filter, term) for term in ascii_matcher.bloom_terms
    )

    unicode_matcher = _compile_search_matcher(
        "Kelvin", match_mode="literal", case_sensitive=False
    )
    assert unicode_matcher.match("KELVIN") is not None
    assert unicode_matcher.match("KELVIN") is None
    assert (
        _compile_search_matcher(
            "réverie", match_mode="literal", case_sensitive=False
        ).match("RÉVERIE")
        is None
    )
    unicode_filter = build_trigram_bloom(("KELVIN",))
    assert all(
        bloom_might_contain(unicode_filter, term)
        for term in unicode_matcher.bloom_terms
    )
    assert compact_search_text("foo\u0085bar") == "foo bar"
    assert compact_search_text("foo\ufeffbar") == "foo\ufeffbar"
    assert _compile_search_matcher(
        "foo\ufeffbar", match_mode="smart", case_sensitive=False
    ).bloom_terms == ("foo\ufeffbar",)
    smart_b3 = _compile_search_matcher(
        "B3", match_mode="smart", case_sensitive=False
    )
    assert smart_b3.match("standalone B3 result") is not None
    assert smart_b3.match("éB3é") is None
    smart_backend = _compile_search_matcher(
        "backend", match_mode="smart", case_sensitive=False
    )
    assert smart_backend.match("αbackendβ") is None


def test_search_v2_rejects_nonportable_bloom_hash_count(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    search = as_object(bootstrap.get("search"), "timeline-v2.search")
    shard = as_object(
        as_array(search.get("shards"), "timeline-v2.search.shards")[0],
        "timeline-v2.search.shards[0]",
    )
    bloom = as_object(shard.get("trigram_bloom"), "search shard.trigram_bloom")
    bloom["hash_count"] = 6
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "backend maturity",
                "--in",
                "all-transcript",
            )
        )
        == 2
    )
    assert "hash_count: expected 7" in capsys.readouterr().err


def test_search_v2_keeps_cross_day_prompt_linkage_when_text_shards_are_pruned(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    timeline = as_object(read_json(root / "data" / "timeline.json"), "timeline")
    timeline_range = as_object(timeline.get("range"), "timeline.range")
    timeline_range["end_ms"] = START + 26 * 60 * 60 * 1000
    prompt_reference = "message:alpha::nightly-protocol"
    records = [
        _search_record(
            event_id="nightly-protocol",
            record_type="prompt",
            role="user",
            text="Explain the nightly protocol.",
            at_ms=START,
            author_kind="owner_human",
            prompt_ref=prompt_reference,
            prompt_author_kind="owner_human",
        ),
        _search_record(
            event_id="nightly-result",
            record_type="response",
            role="assistant",
            text="QUARTZ completed the overnight verification.",
            at_ms=START + 23 * 60 * 60 * 1000,
            agent_id="child",
            prompt_ref=prompt_reference,
            prompt_author_kind="owner_human",
        ),
    ]
    with schema_2_writer_enabled():
        write_timeline_shards(root, timeline, search_records=records)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "QUARTZ",
                "--in",
                "agent-responses",
                "--start-time",
                "2026-08-08T00:00:00Z",
            )
        )
        == 0
    )
    response_item = _items(_response(capsys))[0]
    assert response_item["prompt_ref"] == prompt_reference
    assert response_item["prompt_excerpt"] == "Explain the nightly protocol."

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "nightly protocol",
                "--in",
                "owner-prompts",
                "--linkage",
                "linked",
            )
        )
        == 0
    )
    prompt_item = _items(_response(capsys))[0]
    assert prompt_item["ref"] == prompt_reference
    assert prompt_item["linked_response_count"] == 1

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "nightly protocol",
                "--in",
                "owner-prompts",
                "--linkage",
                "unlinked",
                "--end-time",
                "2026-08-08T00:00:00Z",
            )
        )
        == 0
    )
    scoped_prompt = _items(_response(capsys))[0]
    assert scoped_prompt["linked_response_count"] == 0

    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    search = as_object(bootstrap.get("search"), "timeline-v2.search")
    for index, raw_shard in enumerate(
        as_array(search.get("shards"), "timeline-v2.search.shards")
    ):
        as_object(raw_shard, f"search shard {index}").pop("linkage", None)
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "QUARTZ",
                "--in",
                "agent-responses",
                "--start-time",
                "2026-08-08T00:00:00Z",
            )
        )
        == 0
    )
    legacy_item = _items(_response(capsys))[0]
    assert legacy_item["prompt_excerpt"] == "Explain the nightly protocol."


def test_search_v2_reports_paging_and_show_resolves_prompt_response_context(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "maturity",
                "--in",
                "all-transcript",
                "--limit",
                "1",
            )
        )
        == 0
    )
    page = _response(capsys)
    assert page["total_matches"] == 2
    assert page["returned"] == 1
    assert page["truncated"] is True

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "show",
                "message:alpha::answer-b3",
            )
        )
        == 0
    )
    shown = _items(_response(capsys))[0]
    assert shown["text"] == "B3 means 50% or more of the ptrace corpus passes."
    prompt = as_object(shown["linked_prompt"], "linked prompt")
    assert prompt["ref"] == "message:alpha::owner-b3"
    assert shown["phase_ref"] == "phase:alpha::phase-work"


def test_search_v2_rejects_mixing_new_corpus_and_compatibility_scope(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "B3",
                "--in",
                "all-transcript",
                "--scope",
                "transcripts",
            )
        )
        == 2
    )
    assert "--in and --scope cannot be combined" in capsys.readouterr().err


def test_search_v2_ignores_a_newer_partially_published_schema_1_generation(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    timeline_path = root / "data" / "timeline.json"
    timeline = as_object(read_json(timeline_path), "timeline")
    timeline["source_digest"] = "replacement-generation"
    timeline_path.write_text(canonical_json(timeline), encoding="utf-8")

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "B3",
                "--in",
                "all-transcript",
            )
        )
        == 0
    )
    assert _response(capsys)["total_matches"] == 2


def test_query_prefers_complete_schema_2_and_does_not_require_schema_1(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    (root / "data" / "timeline.json").unlink()

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 0
    assert [item["ref"] for item in _items(_response(capsys))] == [
        "team:alpha",
        "team:beta",
    ]


def test_query_falls_back_to_schema_1_when_schema_2_is_absent(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    (root / "data" / "timeline-v2.json").unlink()

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 0
    assert len(_items(_response(capsys))) == 2


def test_query_accepts_a_pre_binding_schema_2_global_object(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    def remove_source_binding(record: dict[str, JsonValue]) -> None:
        record.pop("source_digest", None)

    _rewrite_schema_2_object(root, "global", remove_source_binding)

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 0
    assert len(_items(_response(capsys))) == 2


def test_query_accepts_a_pre_binding_schema_2_phase_index(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    def remove_source_binding(record: dict[str, JsonValue]) -> None:
        record.pop("source_digest", None)

    _rewrite_schema_2_object(root, "phase_index", remove_source_binding)
    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 0
    assert len(_items(_response(capsys))) == 1


def test_query_falls_back_for_a_schema_2_bootstrap_predating_phase_index(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    bootstrap.pop("phase_index")
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")

    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 0
    phase = _items(_response(capsys))[0]
    assert phase["team"] == "alpha"


def test_query_does_not_treat_a_malformed_bootstrap_as_pre_phase_index(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    bootstrap.pop("phase_index")
    bootstrap["schema_version"] = 2.0
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")

    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 2
    assert "timeline-v2.schema_version: expected an integer" in capsys.readouterr().err


@pytest.mark.parametrize(
    ("corruption", "expected_error"),
    (
        ("digest", "object digest mismatch"),
        ("source", "different source generation"),
    ),
)
def test_query_rejects_schema_2_object_digest_and_source_mismatches(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    corruption: str,
    expected_error: str,
) -> None:
    root = _site(tmp_path)
    bootstrap = as_object(read_json(root / "data" / "timeline-v2.json"), "timeline-v2")
    global_reference = as_object(bootstrap.get("global"), "timeline-v2.global")
    if corruption == "digest":
        relative = as_string(global_reference.get("url"), "timeline-v2.global.url")
        path = root / relative
        path.write_bytes(path.read_bytes() + b" ")
    else:
        _rewrite_schema_2_object(
            root,
            "global",
            lambda record: record.__setitem__(
                "source_digest", "different-source-generation"
            ),
        )

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 2
    assert expected_error in capsys.readouterr().err


def test_query_rejects_a_mismatched_phase_index_source_generation(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    _rewrite_schema_2_object(
        root,
        "phase_index",
        lambda record: record.__setitem__(
            "source_digest", "different-source-generation"
        ),
    )

    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 2
    assert "phase index belongs to a different source generation" in (
        capsys.readouterr().err
    )


@pytest.mark.parametrize("source_digest", (None, "different-source-generation"))
def test_query_compatibly_validates_search_shard_source_generation(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    source_digest: str | None,
) -> None:
    root = _site(tmp_path)
    bootstrap_path = root / "data" / "timeline-v2.json"
    bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
    search = as_object(bootstrap.get("search"), "timeline-v2.search")
    shards = as_array(search.get("shards"), "timeline-v2.search.shards")
    reference = as_object(shards[0], "timeline-v2.search.shards[0]")

    def change_binding(record: dict[str, JsonValue]) -> None:
        if source_digest is None:
            record.pop("source_digest", None)
        else:
            record["source_digest"] = source_digest

    _rewrite_content_addressed_object(
        root, reference, "timeline-v2.search.shards[0]", change_binding
    )
    bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")

    result = timeline_main(
        (
            "query",
            "--output",
            str(root),
            "search",
            "B3",
            "--in",
            "all-transcript",
        )
    )
    if source_digest is None:
        assert result == 0
        assert _response(capsys)["total_matches"] == 2
    else:
        assert result == 2
        assert "search shard belongs to a different source generation" in (
            capsys.readouterr().err
        )


def test_query_maps_phase_team_from_an_exact_global_agent_id(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 0
    phase = _items(_response(capsys))[0]
    assert phase["team"] == "alpha"
    assert phase["agent_ref"] == "agent:alpha::child"

    def remove_agent_namespace(record: dict[str, JsonValue]) -> None:
        phases = as_array(record.get("phases"), "phase-index.phases")
        first = as_object(phases[0], "phase-index.phases[0]")
        first["agent_id"] = "child"

    _rewrite_schema_2_object(root, "phase_index", remove_agent_namespace)
    assert timeline_main(("query", "--output", str(root), "list", "phases")) == 2
    assert "no exact global agent match" in capsys.readouterr().err


@pytest.mark.parametrize(
    ("corruption", "expected_error"),
    (
        ("team", "unknown team 'ghost'"),
        ("range", "agent interval is outside timeline range"),
        ("phase-schema", "unsupported timeline phase index"),
    ),
)
def test_query_validates_schema_2_team_range_and_phase_index_metadata(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    corruption: str,
    expected_error: str,
) -> None:
    root = _site(tmp_path)
    if corruption == "team":

        def replace_team(record: dict[str, JsonValue]) -> None:
            agents = as_array(record.get("agents"), "timeline-global.agents")
            as_object(agents[0], "timeline-global.agents[0]")["team"] = "ghost"

        _rewrite_schema_2_object(root, "global", replace_team)
    elif corruption == "range":
        bootstrap_path = root / "data" / "timeline-v2.json"
        bootstrap = as_object(read_json(bootstrap_path), "timeline-v2")
        time_range = as_object(bootstrap.get("range"), "timeline-v2.range")
        time_range["start_ms"] = START + 1
        bootstrap_path.write_text(canonical_json(bootstrap), encoding="utf-8")
    else:
        _rewrite_schema_2_object(
            root,
            "phase_index",
            lambda record: record.__setitem__("schema_version", 99),
        )

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 2
    assert expected_error in capsys.readouterr().err


@pytest.mark.parametrize(
    ("corruption", "expected_error"),
    (
        ("schema", "unsupported transcript search record"),
        ("team", "transcript search record escapes shard"),
        ("agent", "transcript search record has unknown agent"),
    ),
)
def test_search_v2_rejects_malformed_records_with_a_valid_object_catalog(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    corruption: str,
    expected_error: str,
) -> None:
    root = _site(tmp_path)

    def corrupt(records: list[dict[str, JsonValue]]) -> None:
        record = records[0]
        if corruption == "schema":
            record["schema_version"] = 99
        elif corruption == "team":
            record["team"] = "ghost"
            record["agent_id"] = "ghost::root"
            record["agent_ref"] = "agent:ghost::root"
            record["ref"] = "message:ghost::owner-b3"
        elif corruption == "agent":
            record["agent_id"] = "alpha::ghost"
            record["agent_ref"] = "agent:alpha::ghost"
        else:
            raise AssertionError(f"unsupported corruption {corruption}")

    _rewrite_search_records(root, corrupt)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "B3",
                "--in",
                "all-transcript",
            )
        )
        == 2
    )
    assert expected_error in capsys.readouterr().err


@pytest.mark.parametrize(
    "v2_option",
    (
        ("--match", "literal"),
        ("--sort", "newest"),
        ("--prompt-author", "agent"),
        ("--linkage", "linked"),
        ("--role", "user"),
        ("--offset", "1"),
    ),
)
def test_search_rejects_v2_only_options_without_a_corpus(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    v2_option: tuple[str, str],
) -> None:
    root = _site(tmp_path)
    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "GHC",
                *v2_option,
            )
        )
        == 2
    )
    assert "search-v2 options require --in" in capsys.readouterr().err


def test_standalone_search_rejects_v2_only_options_without_a_corpus(
    tmp_path: Path,
) -> None:
    root = _site(tmp_path)
    source = Path(__file__).resolve().parents[1] / "wrkviz" / "query.py"
    bundled = root / "query.py"
    shutil.copyfile(source, bundled)

    completed = subprocess.run(
        (
            sys.executable,
            str(bundled),
            "--output",
            str(root),
            "search",
            "GHC",
            "--offset",
            "1",
        ),
        cwd=tmp_path,
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 2
    assert "search-v2 options require --in" in completed.stderr


def test_search_v2_caps_excerpts_and_preserves_original_unicode_ranges(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    long_text = "Straße UNIQUE " + "x" * 10_000 + " UNIQUE"

    def replace_text(records: list[dict[str, JsonValue]]) -> None:
        record = next(value for value in records if value["event_id"] == "hash-noise")
        record["text"] = long_text

    _rewrite_search_records(root, replace_text)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "unique",
                "--in",
                "all-transcript",
                "--match",
                "literal",
            )
        )
        == 0
    )
    item = _items(_response(capsys))[0]
    excerpt = as_object(item["excerpt_details"], "search result excerpt")
    assert len(as_string(excerpt["text"], "excerpt.text")) == 480
    assert excerpt["match_ranges"] == [[7, 13]]
    assert excerpt["full_characters"] == len(long_text)
    assert excerpt["trailing_omitted_characters"] == len(long_text) - 480
    assert excerpt["is_truncated"] is True


def test_search_v2_requires_a_built_search_corpus(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    (root / "data" / "timeline-v2.json").unlink()

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "B3",
                "--in",
                "all-transcript",
            )
        )
        == 2
    )
    error = capsys.readouterr().err
    assert "no transcript search corpus" in error
    assert "rebuild the website" in error


def test_show_prompt_excludes_linked_tool_records(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    prompt_ref = "message:alpha::owner-b3"

    def append_tool(records: list[dict[str, JsonValue]]) -> None:
        tool = _search_record(
            event_id="call-b3",
            record_type="tool",
            role="tool",
            text="1 tool used: functions.exec_command",
            at_ms=START + 100_000,
            prompt_ref=prompt_ref,
            prompt_author_kind="owner_human",
        )
        tool["ref"] = "tool:alpha::call-b3"
        records.append(tool)

    _rewrite_search_records(root, append_tool)

    assert timeline_main(("query", "--output", str(root), "show", prompt_ref)) == 0
    shown = _items(_response(capsys))[0]
    responses = [
        as_object(value, f"linked_responses[{index}]")
        for index, value in enumerate(
            as_array(shown["linked_responses"], "linked_responses")
        )
    ]
    assert [value["record_type"] for value in responses] == ["response"]
    assert [value["ref"] for value in responses] == ["message:alpha::answer-b3"]


def test_search_v2_can_filter_searchable_system_events(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)

    def append_system(records: list[dict[str, JsonValue]]) -> None:
        records.append(
            _search_record(
                event_id="system-policy",
                record_type="system",
                role="system",
                text="System policy requires deterministic receipts.",
                at_ms=START + 200_000,
                author_kind="system",
            )
        )

    _rewrite_search_records(root, append_system)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "deterministic receipts",
                "--in",
                "all-transcript",
                "--role",
                "system",
            )
        )
        == 0
    )
    items = _items(_response(capsys))
    assert [value["ref"] for value in items] == ["message:alpha::system-policy"]
    assert items[0]["record_type"] == "system"
    assert items[0]["role"] == "system"


def test_search_v2_links_inter_agent_responses_to_agent_prompts(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    prompt_ref = "message:alpha::delegated-audit"

    def append_inter_agent_exchange(records: list[dict[str, JsonValue]]) -> None:
        records.extend(
            (
                _search_record(
                    event_id="delegated-audit",
                    record_type="inter_agent_prompt",
                    role="agent",
                    text="Audit the timeline search implementation.",
                    at_ms=START + 140_000,
                    agent_id="child",
                    prompt_ref=prompt_ref,
                    prompt_author_kind="agent",
                ),
                _search_record(
                    event_id="delegated-audit-response",
                    record_type="inter_agent_response",
                    role="agent",
                    text="Finished the delegated audit with two findings.",
                    at_ms=START + 150_000,
                    agent_id="child",
                    prompt_ref=prompt_ref,
                    prompt_author_kind="agent",
                ),
            )
        )

    _rewrite_search_records(root, append_inter_agent_exchange)

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(root),
                "search",
                "delegated audit",
                "--in",
                "agent-responses",
                "--prompt-author",
                "agent",
            )
        )
        == 0
    )
    items = _items(_response(capsys))
    assert [value["ref"] for value in items] == [
        "message:alpha::delegated-audit-response"
    ]
    assert items[0]["record_type"] == "inter_agent_response"
    assert items[0]["prompt_ref"] == prompt_ref
    assert items[0]["prompt_author_kind"] == "agent"


def test_query_fails_closed_on_unknown_refs_and_archive_path_escape(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    assert timeline_main(("query", "--output", str(root), "show", "agent:nope")) == 2
    error = capsys.readouterr().err
    assert "unknown stable reference" in error

    timeline_path = root / "data" / "timeline.json"
    timeline = as_object(
        narrow_json(json.loads(timeline_path.read_text(encoding="utf-8"))),
        "timeline",
    )
    rollups = as_array(timeline["rollups"], "timeline.rollups")
    first = as_object(rollups[0], "timeline.rollups[0]")
    first["technical_path"] = "../../outside.md"
    timeline_path.write_text(json.dumps(timeline), encoding="utf-8")
    (root / "data" / "timeline-v2.json").unlink()
    reference = f"rollup:alpha::hourly::{START}"
    assert timeline_main(("query", "--output", str(root), "show", reference)) == 2
    error = capsys.readouterr().err
    assert "escapes root" in error


def test_query_rejects_unknown_timeline_schema_and_same_root_path_confusion(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _site(tmp_path)
    timeline_path = root / "data" / "timeline.json"
    original = as_object(
        narrow_json(json.loads(timeline_path.read_text(encoding="utf-8"))),
        "timeline",
    )
    invalid_schema = dict(original)
    invalid_schema["schema_version"] = 99
    timeline_path.write_text(json.dumps(invalid_schema), encoding="utf-8")
    (root / "data" / "timeline-v2.json").unlink()

    assert timeline_main(("query", "--output", str(root), "list", "teams")) == 2
    assert "unsupported timeline schema version 99" in capsys.readouterr().err

    phases = as_array(original["phases"], "timeline.phases")
    first_phase = as_object(phases[0], "timeline.phases[0]")
    first_phase["detail_path"] = "data/timeline.json"
    timeline_path.write_text(json.dumps(original), encoding="utf-8")
    phase_id = as_string(first_phase["id"], "timeline.phases[0].id")
    team = as_string(first_phase["team"], "timeline.phases[0].team")
    reference = f"phase:{team}::{phase_id.removeprefix(team + '::')}"

    assert timeline_main(("query", "--output", str(root), "show", reference)) == 2
    assert "outside data/details" in capsys.readouterr().err


def test_bundled_query_source_runs_without_the_installed_package(
    tmp_path: Path,
) -> None:
    root = _site(tmp_path)
    source = Path(__file__).resolve().parents[1] / "wrkviz" / "query.py"
    bundled = root / "query.py"
    shutil.copyfile(source, bundled)

    completed = subprocess.run(
        (sys.executable, str(bundled), "--output", str(root), "list", "teams"),
        cwd=tmp_path,
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr
    response = as_object(
        narrow_json(json.loads(completed.stdout)), "standalone response"
    )
    assert response["count"] == 2
