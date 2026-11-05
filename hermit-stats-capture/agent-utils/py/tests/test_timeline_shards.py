"""The schema-2 projection, which no build performs any more.

`timeline_shards.SCHEMA_2_IS_PUBLISHED` is ``False`` and :func:`write_timeline_shards` refuses to
run while it is. The writer survives its own retirement for one reason: `query.py` and
`static/app.js` both still *read* schema 2, for archives an older tool built, and a reader whose
input cannot be produced cannot be tested. That makes this file the description of what those
archives contain -- so it is kept, whole, rather than reduced to a test of the refusal.

The refusal itself is asserted once, at the bottom, and every other test here runs inside
:func:`schema_2_writer_enabled`. The autouse fixture is deliberate and deliberately module-scoped
in effect: enabling the writer globally would let a *build* test silently produce a generation this
tool no longer emits, which is the one mistake the constant exists to prevent.
"""

from __future__ import annotations

import gzip
import hashlib
from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue, as_array, as_object, read_json
from wrkviz.search_bloom import (
    TRIGRAM_BLOOM_HASH_COUNT,
    build_trigram_bloom,
    bloom_might_contain,
    trigram_bloom_from_catalog,
)
from wrkviz.timeline_shards import (
    SCHEMA_2_BOOTSTRAP_PATH,
    write_timeline_shards,
)

from tests.timeline_legacy_generations import schema_2_writer_enabled


@pytest.fixture(autouse=True)
def _legacy_writer() -> Iterator[None]:
    """Every test in this file is about the retired writer, so every test may run it."""

    with schema_2_writer_enabled():
        yield


def _ms(value: str) -> int:
    return int(
        datetime.fromisoformat(value).astimezone(timezone.utc).timestamp() * 1000
    )


def _timeline() -> dict[str, JsonValue]:
    start = _ms("2026-08-11T23:30:00+00:00")
    midnight = _ms("2026-08-12T00:00:00+00:00")
    end = _ms("2026-08-12T00:30:00+00:00")
    return {
        "schema_version": 1,
        "generated_at": "2026-08-12T00:30:00Z",
        "source_digest": "source-digest",
        "display_timezone": "America/New_York",
        "display_timezone_source": "explicit",
        "range": {"start_ms": start, "end_ms": end},
        "teams": [{"slug": "test-team", "label": "Test team"}],
        "agents": [
            {
                "id": "root",
                "team": "test-team",
                "parent_id": None,
                "start_ms": start,
                "end_ms": end,
            }
        ],
        "phases": [
            {
                "id": "phase-spanning-midnight",
                "agent_id": "root",
                "start_ms": start,
                "end_ms": end,
                "stats": {"tool_calls": 2},
                "states": [],
            },
            {
                "id": "phase-ending-at-midnight",
                "agent_id": "root",
                "start_ms": start,
                "end_ms": midnight,
                "stats": {"tool_calls": 1},
                "states": [],
            },
        ],
        "activity_bins": [
            {
                "team": "test-team",
                "role": "coordinator",
                "resolution": "hourly",
                "start_ms": start,
                "end_ms": end,
                "avg_active_concurrency": 1.0,
            }
        ],
        "edges": [
            {
                "id": "edge-spanning-midnight",
                "source_id": "root",
                "target_id": "root",
                "source_ms": start,
                "target_ms": end,
            },
            {
                "id": "edge-ending-at-midnight",
                "source_id": "root",
                "target_id": "root",
                "source_ms": start,
                "target_ms": midnight,
            },
            {
                "id": "structural-result",
                "kind": "result",
                "source_id": "root",
                "target_id": "root",
                "source_ms": midnight,
                "target_ms": midnight,
            },
        ],
        "events": [
            {"agent_id": "root", "at_ms": start, "kind": "user_prompt"},
            {"agent_id": "root", "at_ms": midnight, "kind": "tool_call"},
        ],
        "rollups": [],
        "glossary": [],
        "summary_files": [],
        "artifact_catalog_path": "data/artifacts.json",
        "project_overview": {"text": "Test project. " * 500},
    }


def _object(root: Path, reference: dict[str, JsonValue]) -> dict[str, JsonValue]:
    url = reference["url"]
    digest = reference["sha256"]
    assert isinstance(url, str)
    assert isinstance(digest, str)
    path = root / url
    assert path.name == f"{digest}.json"
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest
    if (path.with_name(path.name + ".gz")).is_file():
        assert gzip.decompress(path.with_name(path.name + ".gz").read_bytes()) == (
            path.read_bytes()
        )
    return as_object(read_json(path), str(path))


def test_schema_2_projection_is_content_addressed_range_sharded_and_idempotent(
    tmp_path: Path,
) -> None:
    schema_1 = tmp_path / "data" / "timeline.json"
    schema_1.parent.mkdir(parents=True)
    schema_1.write_text("schema-one-is-owned-by-the-caller\n", encoding="utf-8")

    first = write_timeline_shards(tmp_path, _timeline())
    assert first.files_changed > 0
    assert first.detail_shards == 2
    assert first.bootstrap_bytes < first.object_bytes
    assert first.bootstrap_gzip_bytes is not None
    assert first.bootstrap_gzip_bytes < first.bootstrap_bytes
    assert first.object_gzip_bytes < first.object_bytes
    assert schema_1.read_text(encoding="utf-8") == "schema-one-is-owned-by-the-caller\n"

    bootstrap_path = tmp_path / SCHEMA_2_BOOTSTRAP_PATH
    bootstrap = as_object(read_json(bootstrap_path), str(bootstrap_path))
    assert bootstrap["schema_version"] == 2
    assert bootstrap["kind"] == "timeline-bootstrap"
    assert bootstrap["teams"] == [{"label": "Test team", "slug": "test-team"}]
    assert len(as_array(bootstrap["activity_bins"], "activity bins")) == 1
    assert bootstrap["search"] == {
        "fields": ["agent", "phase", "edge"],
        "strategy": "load-all-detail-shards-on-first-query",
    }
    phase_index_reference = as_object(bootstrap["phase_index"], "phase index reference")
    phase_index = _object(tmp_path, phase_index_reference)
    assert phase_index["kind"] == "timeline-phase-index"
    assert phase_index["source_digest"] == "source-digest"
    indexed_phases = [
        as_object(value, "indexed phase")
        for value in as_array(phase_index["phases"], "indexed phases")
    ]
    assert [phase["id"] for phase in indexed_phases] == [
        "phase-spanning-midnight",
        "phase-ending-at-midnight",
    ]
    assert all("states" not in phase for phase in indexed_phases)
    assert all("activity_start_ms" in phase for phase in indexed_phases)
    assert all(
        set(phase)
        == {
            "id",
            "agent_id",
            "start_ms",
            "end_ms",
            "activity_start_ms",
            "activity_end_ms",
            "stats",
        }
        for phase in indexed_phases
    )

    global_reference = as_object(bootstrap["global"], "global reference")
    global_object = _object(tmp_path, global_reference)
    assert global_object["kind"] == "timeline-global"
    assert global_object["source_digest"] == "source-digest"
    assert len(as_array(global_object["agents"], "global agents")) == 1
    assert [
        as_object(value, "structural edge")["id"]
        for value in as_array(global_object["edges"], "global edges")
    ] == ["structural-result"]
    assert "phases" not in global_object
    assert "events" not in global_object
    assert "activity_bins" not in global_object

    catalog = [
        as_object(value, "detail shard")
        for value in as_array(bootstrap["detail_shards"], "detail shards")
    ]
    assert [value["day"] for value in catalog] == ["2026-08-11", "2026-08-12"]
    first_day = _object(tmp_path, catalog[0])
    second_day = _object(tmp_path, catalog[1])
    assert [
        as_object(value, "phase")["id"]
        for value in as_array(first_day["phases"], "first phases")
    ] == ["phase-spanning-midnight", "phase-ending-at-midnight"]
    assert [
        as_object(value, "phase")["id"]
        for value in as_array(second_day["phases"], "second phases")
    ] == ["phase-spanning-midnight"]
    assert len(as_array(first_day["edges"], "first edges")) == 2
    assert len(as_array(second_day["edges"], "second edges")) == 2
    assert len(as_array(first_day["events"], "first events")) == 1
    assert len(as_array(second_day["events"], "second events")) == 1

    assert bootstrap_path.stat().st_mtime_ns >= max(
        (tmp_path / relative).stat().st_mtime_ns
        for relative in first.generated_files
        if relative != SCHEMA_2_BOOTSTRAP_PATH and not relative.endswith(".gz")
    )
    second = write_timeline_shards(tmp_path, _timeline())
    assert second.files_changed == 0
    assert second.generated_files == first.generated_files


def test_schema_2_publishes_content_addressed_transcript_search_shards(
    tmp_path: Path,
) -> None:
    start = _ms("2026-08-11T23:30:00+00:00")
    midnight = _ms("2026-08-12T00:00:00+00:00")
    records: list[dict[str, JsonValue]] = [
        {
            "schema_version": 1,
            "ref": "message:test-team::prompt",
            "record_type": "prompt",
            "role": "user",
            "team": "test-team",
            "agent_id": "root",
            "agent_ref": "agent:test-team::root",
            "agent_path": "/root",
            "event_id": "prompt",
            "turn_id": "turn-1",
            "at_ms": start,
            "text": "Where is backend maturity documented?",
            "author_kind": "owner_human",
            "ingress_kind": "web",
            "prompt_ref": "message:test-team::prompt",
            "prompt_author_kind": "owner_human",
            "content_fidelity": "verbatim",
        },
        {
            "schema_version": 1,
            "ref": "message:test-team::response",
            "record_type": "response",
            "role": "assistant",
            "team": "test-team",
            "agent_id": "root",
            "agent_ref": "agent:test-team::root",
            "agent_path": "/root",
            "event_id": "response",
            "turn_id": "turn-1",
            "at_ms": midnight,
            "text": "B3 means at least half of the ptrace corpus passes.",
            "author_kind": "agent",
            "ingress_kind": "codex",
            "prompt_ref": "message:test-team::prompt",
            "prompt_author_kind": "owner_human",
            "content_fidelity": "verbatim",
        },
    ]

    first = write_timeline_shards(tmp_path, _timeline(), search_records=records)

    assert first.search_shards == 2
    bootstrap = as_object(
        read_json(tmp_path / SCHEMA_2_BOOTSTRAP_PATH), "search bootstrap"
    )
    search = as_object(bootstrap["search"], "search")
    assert search["strategy"] == "transcript-message-shards"
    assert search["counts"] == {
        "inter_agent": 0,
        "prompts": 1,
        "records": 2,
        "responses": 1,
        "tools": 0,
    }
    catalog = [
        as_object(value, "search shard")
        for value in as_array(search["shards"], "search shards")
    ]
    assert [value["day"] for value in catalog] == ["2026-08-11", "2026-08-12"]
    prompt_filter = trigram_bloom_from_catalog(
        as_object(catalog[0]["trigram_bloom"], "prompt trigram bloom")
    )
    response_filter = trigram_bloom_from_catalog(
        as_object(catalog[1]["trigram_bloom"], "response trigram bloom")
    )
    assert bloom_might_contain(prompt_filter, "backend maturity")
    assert not bloom_might_contain(prompt_filter, "ptrace corpus")
    assert bloom_might_contain(response_filter, "ptrace corpus")
    prompt_shard = _object(tmp_path, catalog[0])
    response_shard = _object(tmp_path, catalog[1])
    prompt_linkage = _object(
        tmp_path, as_object(catalog[0]["linkage"], "prompt linkage")
    )
    response_linkage = _object(
        tmp_path, as_object(catalog[1]["linkage"], "response linkage")
    )
    assert prompt_shard["source_digest"] == "source-digest"
    assert response_shard["source_digest"] == "source-digest"
    assert [
        as_object(value, "search record")["ref"]
        for value in as_array(prompt_shard["records"], "records")
    ] == ["message:test-team::prompt"]
    assert [
        as_object(value, "search record")["ref"]
        for value in as_array(response_shard["records"], "records")
    ] == ["message:test-team::response"]
    assert prompt_linkage["prompts"] == [
        {
            "excerpt": "Where is backend maturity documented?",
            "ref": "message:test-team::prompt",
        }
    ]
    assert prompt_linkage["responses"] == []
    assert response_linkage["prompts"] == []
    assert response_linkage["responses"] == [
        {
            "agent_ref": "agent:test-team::root",
            "at_ms": midnight,
            "prompt_ref": "message:test-team::prompt",
            "ref": "message:test-team::response",
        }
    ]

    second = write_timeline_shards(tmp_path, _timeline(), search_records=records)
    assert second.files_changed == 0
    assert second.generated_files == first.generated_files


def test_trigram_bloom_catalog_requires_the_portable_hash_count() -> None:
    catalog = build_trigram_bloom(("backend maturity",)).catalog_obj()
    assert catalog["hash_count"] == TRIGRAM_BLOOM_HASH_COUNT
    catalog["hash_count"] = TRIGRAM_BLOOM_HASH_COUNT - 1
    with pytest.raises(ValueError, match=r"hash_count: expected 7"):
        trigram_bloom_from_catalog(catalog)


def _shard_manifest(root: Path) -> dict[str, JsonValue]:
    path = root / "data" / "timeline-v2" / "manifest.json"
    return as_object(read_json(path), str(path))


def _object_file_set(manifest: dict[str, JsonValue], field: str) -> set[str]:
    return {
        value for value in as_array(manifest[field], field) if isinstance(value, str)
    }


def test_schema_2_retains_one_distinct_generation_and_idempotent_reruns(
    tmp_path: Path,
) -> None:
    first_timeline = _timeline()
    first_timeline["project_overview"] = {"text": "generation one"}
    write_timeline_shards(tmp_path, first_timeline)
    first_current = _object_file_set(_shard_manifest(tmp_path), "current_objects")

    second_timeline = _timeline()
    second_timeline["project_overview"] = {"text": "generation two"}
    write_timeline_shards(tmp_path, second_timeline)
    second_manifest = _shard_manifest(tmp_path)
    second_current = _object_file_set(second_manifest, "current_objects")
    second_retained = _object_file_set(second_manifest, "retained_objects")
    assert second_retained == first_current - second_current
    assert second_retained
    assert all((tmp_path / relative).is_file() for relative in second_retained)

    unchanged = write_timeline_shards(tmp_path, second_timeline)
    assert unchanged.files_changed == 0
    assert (
        _object_file_set(_shard_manifest(tmp_path), "retained_objects")
        == second_retained
    )

    third_timeline = _timeline()
    third_timeline["project_overview"] = {"text": "generation three"}
    write_timeline_shards(tmp_path, third_timeline)
    third_manifest = _shard_manifest(tmp_path)
    assert _object_file_set(third_manifest, "retained_objects") == (
        second_current - _object_file_set(third_manifest, "current_objects")
    )
    assert all(not (tmp_path / relative).exists() for relative in second_retained)


def test_schema_2_narrower_scope_drops_prior_generation_immediately(
    tmp_path: Path,
) -> None:
    wide = _timeline()
    wide["project_overview"] = {"text": "wide secret"}
    write_timeline_shards(tmp_path, wide)
    wide_objects = _object_file_set(_shard_manifest(tmp_path), "current_objects")

    narrow = _timeline()
    time_range = as_object(narrow["range"], "range")
    time_range["start_ms"] = _ms("2026-08-12T00:00:00+00:00")
    narrow["project_overview"] = {"text": "narrow public"}
    write_timeline_shards(tmp_path, narrow)
    manifest = _shard_manifest(tmp_path)
    current = _object_file_set(manifest, "current_objects")

    assert _object_file_set(manifest, "retained_objects") == set()
    assert all(
        not (tmp_path / relative).exists() for relative in wide_objects - current
    )


def test_schema_2_projection_can_skip_sidecars_for_temporary_builds(
    tmp_path: Path,
) -> None:
    report = write_timeline_shards(tmp_path, _timeline(), precompress=False)

    assert all(not path.endswith(".gz") for path in report.generated_files)
    assert report.object_gzip_bytes == report.object_bytes


def test_schema_2_projection_rejects_symlinked_object_parent(tmp_path: Path) -> None:
    victim = tmp_path / "victim"
    victim.mkdir()
    data = tmp_path / "data"
    data.mkdir()
    (data / "timeline-v2").symlink_to(victim, target_is_directory=True)

    with pytest.raises(ValueError, match="parent symlink"):
        write_timeline_shards(tmp_path, _timeline())

    assert list(victim.iterdir()) == []


@pytest.mark.parametrize(
    "relative",
    ("data/timeline-v2.json", "data/timeline-v2.json.gz"),
)
def test_schema_2_projection_rejects_symlinked_bootstrap_files(
    tmp_path: Path, relative: str
) -> None:
    victim = tmp_path / "victim"
    victim.write_text("preserve me\n", encoding="utf-8")
    candidate = tmp_path / relative
    candidate.parent.mkdir(parents=True)
    candidate.symlink_to(victim)

    with pytest.raises(ValueError, match="output symlink"):
        write_timeline_shards(tmp_path, _timeline())

    assert victim.read_text(encoding="utf-8") == "preserve me\n"


def test_the_writer_refuses_to_run_for_a_build(tmp_path: Path) -> None:
    """The refusal, with the fixture's licence explicitly withdrawn.

    This is the enforcement `archive_gc` leans on when it offers a whole 1.4 GB generation to the
    trash. A constant that merely *said* the writer had stopped would be a comment; this makes it a
    fact, and makes the flip impossible to land as a one-line lie that left the writer running.
    """

    import wrkviz.timeline_shards as module

    # The autouse fixture above has lifted the constant for this test as for every other, so it is
    # put back here -- which is also the only way to reach the branch a build would reach. What the
    # *default* is is asserted where it belongs, beside the collector that depends on it, in
    # `test_timeline_archive_gc.py`.
    module.SCHEMA_2_IS_PUBLISHED = False
    try:
        with pytest.raises(ValueError, match="must not publish schema 2"):
            write_timeline_shards(tmp_path, _timeline())
    finally:
        module.SCHEMA_2_IS_PUBLISHED = True
    assert not (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).exists()
