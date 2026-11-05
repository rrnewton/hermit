"""Provider-neutral pipeline tests for explicit Orc coordinator continuations."""

from __future__ import annotations

from collections.abc import Sequence
import json
from pathlib import Path

import pytest

import wrkviz.pipeline as pipeline_module
from wrkviz.build_store import team_build_root
from wrkviz.model import TeamData
from wrkviz.orc import (
    OrcContinuationLink,
    OrcContinuationSpec,
    OrcParseError,
    OrcSnapshotResult,
    OrcSourceCopy,
)
from wrkviz.pipeline import (
    _load_orc_source_manifest,
    ingest_orc,
    load_archived_team,
)
from tests.test_timeline_orc import (
    ROOT as FIXTURE_ROOT,
    SUCCESSOR as FIXTURE_SUCCESSOR,
    _continuation_fixture,
    _fixture,
)


ROOT = "orc-root"
NEXT = "orc-next"
LAST = "orc-last"


def _spec(value: str | OrcContinuationSpec) -> OrcContinuationSpec:
    return (
        value
        if isinstance(value, OrcContinuationSpec)
        else OrcContinuationSpec.from_value(value, "test")
    )


def _manifest_path(archive: Path) -> Path:
    path = team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def _link(
    predecessor: str = ROOT,
    session: str = NEXT,
    predecessor_at_ms: int = 1_000,
    started_at_ms: int = 2_000,
    start_message_id: str | None = None,
    start_source_line: int | None = None,
    predecessor_source_table: str | None = None,
) -> OrcContinuationLink:
    return OrcContinuationLink(
        predecessor_session_id=predecessor,
        session_id=session,
        predecessor_source_path=f".orc/sessions/{predecessor}/state.db",
        predecessor_source_line=7,
        predecessor_at_ms=predecessor_at_ms,
        source_path=f".orc/sessions/{session}/state.db",
        started_at_ms=started_at_ms,
        gap_ms=started_at_ms - predecessor_at_ms,
        start_message_id=start_message_id,
        start_source_line=start_source_line,
        predecessor_source_table=predecessor_source_table,
    )


def _write_manifest(
    archive: Path,
    schema_version: int,
    continuations: tuple[OrcContinuationLink, ...] = (),
) -> None:
    value: dict[str, object] = {
        "schema_version": schema_version,
        "provider": "orc",
        "root_session_id": ROOT,
        "source_root": str((archive / "source").resolve()),
        "snapshot_root": "teams/orc-test/source_snapshots",
        "date_window": None,
        "sources": [],
    }
    if continuations:
        records = [continuation.to_json_obj() for continuation in continuations]
        # Each manifest version is written in exactly the shape that version defines, rather than
        # the newest shape with fields the loader happens to tolerate. A test that wrote a v3 record
        # carrying v5 fields would be asserting nothing about v3.
        for record in records:
            if schema_version == 3:
                del record["start_message_id"]
                del record["start_source_line"]
            if schema_version in (3, 4):
                del record["predecessor_source_table"]
        value["continuation_sessions"] = records
    _manifest_path(archive).write_text(json.dumps(value), encoding="utf-8")


def test_new_orc_archive_preserves_requested_continuation_order(
    tmp_path: Path,
) -> None:
    state = _load_orc_source_manifest(
        tmp_path, "orc-test", ROOT, None, (NEXT, LAST)
    )

    assert state.sources == ()
    assert state.continuation_links == ()
    assert state.continuation_specs == (
        OrcContinuationSpec(NEXT),
        OrcContinuationSpec(LAST),
    )


def test_orc_schema_two_manifest_can_upgrade_with_continuations(
    tmp_path: Path,
) -> None:
    _write_manifest(tmp_path, 2)

    state = _load_orc_source_manifest(
        tmp_path, "orc-test", ROOT, None, (NEXT,)
    )

    assert state.sources == ()
    assert state.continuation_links == ()
    assert state.continuation_specs == (OrcContinuationSpec(NEXT),)


@pytest.mark.parametrize("schema_version", (3, 4, 5))
def test_orc_continuation_manifest_requires_continuation_records(
    tmp_path: Path, schema_version: int
) -> None:
    _write_manifest(tmp_path, schema_version)

    with pytest.raises(OrcParseError, match="continuation_sessions"):
        _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None)


def test_manifest_version_and_boundary_table_field_must_agree(
    tmp_path: Path,
) -> None:
    """The field's presence is the version's whole meaning, so neither may drift from the other.

    A v4 record carrying a boundary table would claim a fact the v4 writer could not have
    established, and a v5 record without the key would be a v4 record wearing a v5 label -- read as
    "already resolved" and therefore never migrated. Both directions refuse, because a manifest that
    silently tolerated either would make the version number stop meaning anything.
    """

    link = _link(
        start_message_id="restart-message",
        start_source_line=11,
        predecessor_source_table="messages",
    )
    _write_manifest(tmp_path, 4, (link,))
    forward_dated = json.loads(
        _manifest_path(tmp_path).read_text(encoding="utf-8")
    )
    forward_dated["continuation_sessions"][0]["predecessor_source_table"] = "messages"
    _manifest_path(tmp_path).write_text(
        json.dumps(forward_dated), encoding="utf-8"
    )
    with pytest.raises(OrcParseError, match="cannot name a predecessor boundary table"):
        _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None)

    _write_manifest(tmp_path, 5, (link,))
    back_dated = json.loads(_manifest_path(tmp_path).read_text(encoding="utf-8"))
    del back_dated["continuation_sessions"][0]["predecessor_source_table"]
    _manifest_path(tmp_path).write_text(json.dumps(back_dated), encoding="utf-8")
    with pytest.raises(OrcParseError, match="lacks its predecessor_source_table"):
        _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None)


def test_unresolved_boundary_table_survives_a_manifest_round_trip(
    tmp_path: Path,
) -> None:
    """A null table is a recorded state, not a missing field, and must decode back as one."""

    link = _link(
        start_message_id="restart-message",
        start_source_line=11,
        predecessor_source_table=None,
    )
    _write_manifest(tmp_path, 5, (link,))

    state = _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None)

    assert state.continuation_links == (link,)
    assert state.continuation_links[0].predecessor_source_table is None


def test_recorded_orc_continuations_are_reused_and_only_prefix_extended(
    tmp_path: Path,
) -> None:
    first = _link()
    second = _link(NEXT, LAST, 2_500, 3_000)
    _write_manifest(tmp_path, 3, (first,))

    reused = _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None)
    extended = _load_orc_source_manifest(
        tmp_path, "orc-test", ROOT, None, (NEXT, LAST)
    )

    assert reused.continuation_links == (first,)
    assert reused.continuation_specs == (OrcContinuationSpec(NEXT),)
    assert extended.continuation_links == (first,)
    assert extended.continuation_specs == (
        OrcContinuationSpec(NEXT),
        OrcContinuationSpec(LAST),
    )

    with pytest.raises(OrcParseError, match="recorded ordered prefix"):
        _load_orc_source_manifest(
            tmp_path, "orc-test", ROOT, None, (LAST,)
        )

    _write_manifest(tmp_path, 3, (first, second))
    with pytest.raises(OrcParseError, match="recorded ordered prefix"):
        _load_orc_source_manifest(
            tmp_path, "orc-test", ROOT, None, (NEXT,)
        )


def test_bounded_orc_continuation_is_part_of_the_frozen_prefix(
    tmp_path: Path,
) -> None:
    bounded = OrcContinuationSpec(NEXT, "restart-message")
    link = _link(
        start_message_id=bounded.start_message_id,
        start_source_line=11,
    )
    _write_manifest(tmp_path, 4, (link,))

    reused = _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None)
    extended = _load_orc_source_manifest(
        tmp_path, "orc-test", ROOT, None, (bounded, LAST)
    )

    assert reused.continuation_specs == (bounded,)
    assert extended.continuation_specs == (
        bounded,
        OrcContinuationSpec(LAST),
    )
    with pytest.raises(OrcParseError, match="recorded ordered prefix"):
        _load_orc_source_manifest(tmp_path, "orc-test", ROOT, None, (NEXT,))


def test_orc_pipeline_persists_and_reuses_continuation_manifest_state(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    link = _link(
        start_message_id="restart-message",
        start_source_line=11,
        predecessor_source_table="messages",
    )
    snapshot_calls: list[
        tuple[
            tuple[OrcSourceCopy, ...],
            tuple[OrcContinuationSpec, ...],
            tuple[OrcContinuationLink, ...],
        ]
    ] = []
    loaded_links: list[tuple[OrcContinuationLink, ...]] = []

    def fake_snapshot(
        source_root: Path,
        root_session_id: str,
        snapshot_root: Path,
        previous_sources: Sequence[OrcSourceCopy],
        captured_at: str,
        continuation_specs: Sequence[str | OrcContinuationSpec] = (),
        previous_continuations: Sequence[OrcContinuationLink] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> OrcSnapshotResult:
        del source_root, snapshot_root, captured_at, accept_prefix_rewrite
        assert root_session_id == ROOT
        snapshot_calls.append(
            (
                    tuple(previous_sources),
                    tuple(_spec(spec) for spec in continuation_specs),
                tuple(previous_continuations),
            )
        )
        return OrcSnapshotResult((), 0, (link,))

    def fake_load(
        snapshot_root: Path,
        root_session_id: str,
        team_slug: str,
        display_timezone: str,
        source_copies: Sequence[OrcSourceCopy],
        continuation_links: Sequence[OrcContinuationLink] = (),
    ) -> TeamData:
        del snapshot_root, source_copies
        loaded_links.append(tuple(continuation_links))
        return TeamData(
            team_slug=team_slug,
            provider="orc",
            root_thread_id=root_session_id,
            display_timezone=display_timezone,
            sources=(),
            agents=(),
            turns=(),
            events=(),
            tool_calls=(),
            edges=(),
        )

    monkeypatch.setattr(pipeline_module, "snapshot_orc_lineage", fake_snapshot)
    monkeypatch.setattr(pipeline_module, "load_orc_team", fake_load)

    archive = tmp_path / "archive"
    source = tmp_path / "source"
    bounded = OrcContinuationSpec(NEXT, "restart-message")
    ingest_orc(archive, source, ROOT, "orc-test", "UTC", None, None, (bounded,))
    ingest_orc(archive, source, ROOT, "orc-test", "UTC")

    manifest = json.loads(
        _manifest_path(archive).read_text(encoding="utf-8")
    )
    assert manifest["schema_version"] == 5
    assert manifest["continuation_sessions"] == [link.to_json_obj()]
    assert snapshot_calls == [
        ((), (bounded,), ()),
        ((), (bounded,), (link,)),
    ]
    assert loaded_links == [(link,), (link,)]


def test_orc_pipeline_rejects_resolved_boundary_drift_before_normalization(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fake_snapshot(
        source_root: Path,
        root_session_id: str,
        snapshot_root: Path,
        previous_sources: Sequence[OrcSourceCopy],
        captured_at: str,
        continuation_specs: Sequence[str | OrcContinuationSpec] = (),
        previous_continuations: Sequence[OrcContinuationLink] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> OrcSnapshotResult:
        del (
            source_root,
            root_session_id,
            snapshot_root,
            previous_sources,
            captured_at,
            continuation_specs,
            previous_continuations,
            accept_prefix_rewrite,
        )
        return OrcSnapshotResult((), 0, (_link(),))

    def unexpected_load(
        snapshot_root: Path,
        root_session_id: str,
        team_slug: str,
        display_timezone: str,
        source_copies: Sequence[OrcSourceCopy],
        continuation_links: Sequence[OrcContinuationLink] = (),
    ) -> TeamData:
        del (
            snapshot_root,
            root_session_id,
            team_slug,
            display_timezone,
            source_copies,
            continuation_links,
        )
        raise AssertionError("normalization must not run after boundary drift")

    monkeypatch.setattr(pipeline_module, "snapshot_orc_lineage", fake_snapshot)
    monkeypatch.setattr(pipeline_module, "load_orc_team", unexpected_load)

    archive = tmp_path / "archive"
    with pytest.raises(OrcParseError, match="differ from the requested"):
        ingest_orc(
            archive,
            tmp_path / "source",
            ROOT,
            "orc-test",
            "UTC",
            None,
            None,
            (OrcContinuationSpec(NEXT, "restart-message"),),
        )
    assert not (
        team_build_root(archive, "orc-test") / "raw" / "source-manifest.json"
    ).exists()


def test_orc_pipeline_dispatches_typed_spec_into_real_snapshot(
    tmp_path: Path,
) -> None:
    source, _ = _continuation_fixture(tmp_path)
    archive = tmp_path / "archive"
    spec = OrcContinuationSpec(FIXTURE_SUCCESSOR)

    team, first = ingest_orc(
        archive,
        source,
        FIXTURE_ROOT,
        "orc-test",
        "UTC",
        None,
        None,
        (spec,),
    )
    repeated, second = ingest_orc(
        archive, source, FIXTURE_ROOT, "orc-test", "UTC"
    )

    assert any(
        edge.kind == "continuation"
        and edge.from_thread_id == FIXTURE_ROOT
        and edge.to_thread_id == FIXTURE_SUCCESSOR
        for edge in team.edges
    )
    assert repeated == team
    assert first.sources > 0
    assert second.files_changed == 0
    manifest = json.loads(_manifest_path(archive).read_text(encoding="utf-8"))
    assert manifest["schema_version"] == 5
    assert manifest["continuation_sessions"][0]["session_id"] == FIXTURE_SUCCESSOR
    assert manifest["continuation_sessions"][0]["start_message_id"] is None
    assert (
        manifest["continuation_sessions"][0]["predecessor_source_table"]
        == "content_blocks"
    )


def test_orc_normalizer_schema_bump_fails_closed_until_reingest(
    tmp_path: Path,
) -> None:
    source, _, _ = _fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, FIXTURE_ROOT, "orc-test", "UTC")
    marker_path = (
        team_build_root(archive, "orc-test") / "raw" / "normalized-generation.json"
    )
    marker = json.loads(marker_path.read_text(encoding="utf-8"))

    assert marker["normalizer_schema_version"] == 3
    marker["normalizer_schema_version"] = 1
    marker_path.write_text(json.dumps(marker), encoding="utf-8")
    with pytest.raises(ValueError, match="stale or incomplete Orc normalized generation"):
        load_archived_team(archive, "orc-test")

    ingest_orc(archive, source, FIXTURE_ROOT, "orc-test", "UTC")
    assert load_archived_team(archive, "orc-test").provider == "orc"
