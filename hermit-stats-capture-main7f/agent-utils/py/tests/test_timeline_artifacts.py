"""Evidence, safety, and idempotence tests for mechanical work artifacts."""

from __future__ import annotations

import json
from dataclasses import replace
from pathlib import Path

import pytest

from wrkviz.build_store import team_build_root
from wrkviz.archive import narrow_json, read_json
from wrkviz.artifacts import (
    ArtifactKind,
    ArtifactRangeIndex,
    EvidenceRelation,
    artifact_catalog_from_json,
    artifact_ids_for_range,
    extract_artifacts,
    output_artifact_ids_for_range,
)
from wrkviz.model import Agent, Event, SourceSnapshot, TeamData, ToolCall
from wrkviz.pipeline import (
    _write_ingested_team,
    build_archive,
    load_artifact_catalog,
    summarize_archive,
)
from wrkviz.snapshot_store import resolve_snapshot_root
from tests.timeline_projection import read_stored_text, schema_1_timeline_text


ROOT = "00000000-0000-0000-0000-000000000001"
START = 1_775_000_000_000
REPOSITORY = "https://github.com/example-org/example"


def _tool(
    call_id: str,
    offset: int,
    command: str,
    output: str,
    *,
    status: str = "completed",
    workdir: str = "/work/example",
) -> ToolCall:
    input_text = json.dumps({"cmd": command, "workdir": workdir})
    # Codex custom tool output is commonly a structured array rather than a plain string.
    output_text = json.dumps(
        [
            {"type": "input_text", "text": "Script completed\n"},
            {"type": "input_text", "text": output},
        ]
    )
    return ToolCall(
        call_id=call_id,
        item_id=None,
        thread_id=ROOT,
        turn_id="turn-1",
        name="exec",
        namespace="functions",
        started_at_ms=START + offset,
        ended_at_ms=START + offset + 100,
        status=status,
        input_text=input_text,
        output_text=output_text,
        nested_tools=(("exec_command", 1),),
        source_line=offset,
    )


def _event(event_id: str, offset: int, text: str) -> Event:
    return Event(
        event_id=event_id,
        thread_id=ROOT,
        turn_id="turn-1",
        timestamp_ms=START + offset,
        kind="assistant_message",
        role="assistant",
        phase="commentary",
        text=text,
        content_availability="plaintext",
        encrypted_content=None,
        author=ROOT,
        recipient=None,
        source_line=offset,
    )


def _team(
    *, events: tuple[Event, ...] = (), tools: tuple[ToolCall, ...] = ()
) -> TeamData:
    return TeamData(
        team_slug="artifact-test",
        provider="codex",
        root_thread_id=ROOT,
        display_timezone="America/New_York",
        sources=(
            SourceSnapshot(
                path="root.jsonl",
                thread_id=ROOT,
                size_bytes=100,
                mtime_ns=1,
                sha256="a" * 64,
                complete_bytes=100,
                line_count=10,
                working_directory="/work/example",
                repository_url=REPOSITORY + ".git",
            ),
        ),
        agents=(
            Agent(
                thread_id=ROOT,
                parent_thread_id=None,
                agent_path="/root",
                nickname=None,
                role=None,
                depth=0,
                started_at_ms=START,
                ended_at_ms=START + 100_000,
                status="completed",
                source_path="root.jsonl",
            ),
        ),
        turns=(),
        events=events,
        tool_calls=tools,
        edges=(),
    )


def test_extracts_confirmed_commit_push_and_pull_request_without_duplicates() -> None:
    commit = _tool(
        "commit",
        1_000,
        'git commit -m "Add deterministic receipts"',
        "[topic abc1234] Add deterministic receipts\n 2 files changed",
    )
    push = _tool(
        "push",
        2_000,
        "with-proxy git push origin HEAD:refs/heads/topic",
        "To https://github.com/example-org/example.git\n   1111111..abc1234  HEAD -> topic",
    )
    pull = _tool(
        "pr",
        3_000,
        "with-proxy gh pr create -R example-org/example --title Fix --body body",
        "https://github.com/example-org/example/pull/42\nexit_code=0",
    )
    mention = _event(
        "mention",
        4_000,
        "The output is https://github.com/example-org/example/pull/42.",
    )

    catalog = extract_artifacts(_team(events=(mention,), tools=(commit, push, pull)))

    commits = [item for item in catalog.artifacts if item.kind is ArtifactKind.COMMIT]
    assert len(commits) == 1
    commit_artifact = commits[0]
    assert commit_artifact.url == REPOSITORY + "/commit/abc1234"
    assert commit_artifact.title == "Add deterministic receipts"
    assert commit_artifact.producer_thread_id == ROOT
    assert [item.relation for item in commit_artifact.evidence] == [
        EvidenceRelation.PRODUCED,
        EvidenceRelation.PUBLISHED,
    ]
    assert all(item.turn_id == "turn-1" for item in commit_artifact.evidence)

    pulls = [item for item in catalog.artifacts if item.kind is ArtifactKind.PULL_REQUEST]
    assert len(pulls) == 1
    assert pulls[0].url == REPOSITORY + "/pull/42"
    assert pulls[0].title == "Fix"
    assert [item.relation for item in pulls[0].evidence] == [
        EvidenceRelation.PRODUCED,
        EvidenceRelation.REFERENCED,
    ]
    assert [project.url for project in catalog.projects] == [REPOSITORY]
    assert output_artifact_ids_for_range(catalog, START, START + 10_000) == tuple(
        sorted((commit_artifact.artifact_id, pulls[0].artifact_id))
    )
    assert set(artifact_ids_for_range(catalog, START, START + 10_000)) >= {
        commit_artifact.artifact_id,
        pulls[0].artifact_id,
    }
    index = ArtifactRangeIndex.from_catalog(catalog)
    assert index.ids_for_range(START, START + 10_000) == artifact_ids_for_range(
        catalog, START, START + 10_000
    )
    assert index.ids_for_range(
        START, START + 10_000, ROOT, outputs_only=True
    ) == output_artifact_ids_for_range(catalog, START, START + 10_000, ROOT)
    assert index.ids_for_range(START + 10_000, START + 20_000) == ()
    assert catalog.to_json_obj() == extract_artifacts(
        _team(events=(mention,), tools=(commit, push, pull))
    ).to_json_obj()


def test_artifact_range_index_preserves_half_open_output_and_dedupe_semantics() -> None:
    commit = _tool(
        "commit",
        1_000,
        'git commit -m "Add deterministic receipts"',
        "[topic abc1234] Add deterministic receipts",
    )
    push = _tool(
        "push",
        2_000,
        "git push origin HEAD:refs/heads/topic",
        "To https://github.com/example-org/example.git\n"
        "   1111111..abc1234  HEAD -> topic",
    )
    reference = _event(
        "reference",
        3_000,
        "Review https://github.com/example-org/example/commit/abc1234.",
    )
    catalog = extract_artifacts(_team(events=(reference,), tools=(commit, push)))
    artifact = next(item for item in catalog.artifacts if item.kind is ArtifactKind.COMMIT)
    assert [evidence.relation for evidence in artifact.evidence] == [
        EvidenceRelation.PRODUCED,
        EvidenceRelation.PUBLISHED,
        EvidenceRelation.REFERENCED,
    ]
    index = ArtifactRangeIndex.from_catalog(catalog)
    produced_at, published_at, referenced_at = (
        evidence.timestamp_ms for evidence in artifact.evidence
    )

    all_ids = index.ids_for_range(produced_at, referenced_at + 1)
    assert all_ids.count(artifact.artifact_id) == 1
    assert index.ids_for_range(
        produced_at, published_at + 1, outputs_only=True
    ) == (artifact.artifact_id,)
    assert index.ids_for_range(
        referenced_at, referenced_at + 1, outputs_only=True
    ) == ()
    assert index.ids_for_range(produced_at + 1, published_at) == ()
    assert index.ids_for_range(produced_at, produced_at) == ()
    assert index.ids_for_range(published_at, produced_at) == ()
    assert index.ids_for_range(
        START, START + 10_000, "unknown-thread"
    ) == ()


def test_policy_search_and_failed_commands_never_claim_outputs() -> None:
    search = _tool(
        "search",
        1_000,
        "rg -n 'git commit|git push|gh pr create' docs",
        "example: [main deadbee] Not an executed commit\n"
        "https://github.com/example-org/example/pull/7",
    )
    failed_create = _tool(
        "failed-pr",
        2_000,
        "with-proxy gh pr create -R example-org/example --title Nope --body Nope",
        "https://github.com/example-org/example/pull/8\nfatal: authentication failed\nexit=128",
    )
    commit = _tool(
        "commit",
        3_000,
        "git commit -m Local",
        "[topic cafebabe] Local only",
    )
    failed_push = _tool(
        "push",
        4_000,
        "with-proxy git push origin HEAD:topic",
        "fatal: could not read Username\nexit_code=128",
    )

    catalog = extract_artifacts(_team(tools=(search, failed_create, commit, failed_push)))

    assert all(item.external_id != "deadbee" for item in catalog.artifacts)
    pull_relations = {
        item.external_id: tuple(evidence.relation for evidence in item.evidence)
        for item in catalog.artifacts
        if item.kind is ArtifactKind.PULL_REQUEST
    }
    assert pull_relations == {
        "7": (EvidenceRelation.REFERENCED,),
        "8": (EvidenceRelation.REFERENCED,),
    }
    local = next(
        item
        for item in catalog.artifacts
        if item.kind is ArtifactKind.COMMIT and item.external_id == "cafebabe"
    )
    assert [evidence.relation for evidence in local.evidence] == [
        EvidenceRelation.PRODUCED
    ]
    assert local.locator == REPOSITORY + "/commit/cafebabe"
    assert local.url is None


def test_multiline_quoted_pr_body_keeps_true_command_boundary() -> None:
    create = _tool(
        "multiline-pr",
        1_000,
        "with-proxy gh pr create -R example-org/example --title Fix "
        "--body 'Summary\nThe prose says git commit and git push, but executes neither.'",
        REPOSITORY + "/pull/13",
    )

    catalog = extract_artifacts(_team(tools=(create,)))

    pull = next(
        item for item in catalog.artifacts if item.kind is ArtifactKind.PULL_REQUEST
    )
    assert pull.producer_thread_id == ROOT
    assert pull.evidence[0].relation is EvidenceRelation.PRODUCED
    assert all(item.kind is not ArtifactKind.COMMIT for item in catalog.artifacts)


def test_generic_links_are_excluded_but_successful_upload_is_retained_safely() -> None:
    docs = _event(
        "docs",
        1_000,
        "Read https://docs.example.org/guide?token=secret&utm_source=chat#part.",
    )
    upload = _tool(
        "upload",
        2_000,
        "curl --upload-file result.tar https://files.example.org/result.tar",
        "https://files.example.org/result.tar?X-Amz-Signature=VERYSECRET&Expires=1",
    )
    credential_url = _event(
        "credential",
        3_000,
        "Never persist https://alice:password@example.org/private.",
    )

    catalog = extract_artifacts(
        _team(events=(docs, credential_url), tools=(upload,))
    )

    urls = [item for item in catalog.artifacts if item.kind is ArtifactKind.URL]
    assert len(urls) == 1
    assert urls[0].url == "https://files.example.org/result.tar"
    assert "SECRET" not in json.dumps(catalog.to_json_obj())
    assert "password" not in json.dumps(catalog.to_json_obj())
    assert urls[0].evidence[0].relation is EvidenceRelation.PUBLISHED


def test_malformed_ipv6_like_url_does_not_abort_artifact_extraction() -> None:
    malformed = _event(
        "malformed",
        1_000,
        "Offline marker https://[Encrypted Codex collaboration message unavailable.",
    )
    valid = _event("valid", 2_000, REPOSITORY + "/pull/61")

    catalog = extract_artifacts(_team(events=(malformed, valid)))

    assert any(
        item.kind is ArtifactKind.PULL_REQUEST and item.external_id == "61"
        for item in catalog.artifacts
    )
    assert "Encrypted" not in json.dumps(catalog.to_json_obj())


def test_context_only_links_explicit_pr_and_issue_markers() -> None:
    catalog = extract_artifacts(
        _team(
            events=(
                _event(
                    "refs",
                    1_000,
                    "Opened PR #12, followed issue #9, and mentioned naked #77.",
                ),
            )
        )
    )

    identities = {(item.kind, item.external_id) for item in catalog.artifacts}
    assert (ArtifactKind.PULL_REQUEST, "12") in identities
    assert (ArtifactKind.ISSUE, "9") in identities
    assert all(item.external_id != "77" for item in catalog.artifacts)
    assert output_artifact_ids_for_range(catalog, START, START + 10_000) == ()


def test_catalog_round_trip_rejects_tampered_stable_id() -> None:
    catalog = extract_artifacts(
        _team(events=(_event("pr", 1_000, REPOSITORY + "/pull/3"),))
    )
    encoded = narrow_json(catalog.to_json_obj())

    assert artifact_catalog_from_json(encoded) == catalog
    assert isinstance(encoded, dict)
    artifacts = encoded["artifacts"]
    assert isinstance(artifacts, list)
    first = artifacts[0]
    assert isinstance(first, dict)
    first["artifact_id"] = "artifact-tampered"
    with pytest.raises(ValueError, match="artifact_id"):
        artifact_catalog_from_json(encoded)


def test_ingest_writes_catalog_before_redacting_tool_payloads(tmp_path: Path) -> None:
    team = _team(
        tools=(
            _tool(
                "pr",
                1_000,
                "with-proxy gh pr create -R example-org/example --title Fix --body body",
                REPOSITORY + "/pull/55",
            ),
        )
    )

    archived, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )

    assert archived.tool_calls[0].input_text is None
    assert archived.tool_calls[0].output_text is None
    assert archived.sources[0].working_directory is None
    assert archived.sources[0].repository_url == REPOSITORY
    assert report.artifacts >= 2  # repository plus pull request
    assert report.projects == 1
    catalog_path = (
        team_build_root(tmp_path, team.team_slug) / "raw" / "artifacts.json"
    )
    assert catalog_path.is_file()
    catalog = load_artifact_catalog(tmp_path, team.team_slug, archived)
    assert any(item.external_id == "55" for item in catalog.artifacts)
    assert artifact_catalog_from_json(read_json(catalog_path)) == catalog


def test_build_associates_catalog_with_phase_agent_and_rollups(tmp_path: Path) -> None:
    team = _team(
        tools=(
            _tool(
                "pr",
                10_000,
                "with-proxy gh pr create -R example-org/example --title Fix --body body",
                REPOSITORY + "/pull/56",
            ),
        )
    )
    _, report = _write_ingested_team(
        tmp_path, team.team_slug, team, None, 0,
        resolve_snapshot_root(tmp_path, team.team_slug),
    )
    assert report.artifacts >= 2
    summarize_archive(tmp_path, team.team_slug, "heuristic", "test-model")

    first = build_archive(tmp_path, team.team_slug)
    second = build_archive(tmp_path, team.team_slug)

    catalog = json.loads(read_stored_text(tmp_path / "data" / "artifacts.json"))
    pull = next(
        item
        for item in catalog["artifacts"]
        if item["kind"] == ArtifactKind.PULL_REQUEST.value
        and item["external_id"] == "56"
    )
    artifact_id = pull["artifact_id"]
    timeline = json.loads(schema_1_timeline_text(tmp_path))
    assert timeline["artifact_catalog_path"] == "data/artifacts.json"
    assert [project["url"] for project in timeline["projects"]] == [REPOSITORY]
    phase = next(
        item
        for item in timeline["phases"]
        if artifact_id in item["output_artifact_ids"]
    )
    detail = json.loads(read_stored_text(tmp_path / phase["detail_path"]))
    assert artifact_id in detail["artifact_ids"]
    assert artifact_id in detail["output_artifact_ids"]
    assert artifact_id in timeline["agents"][0]["output_artifact_ids"]
    assert all(
        artifact_id in rollup["output_artifact_ids"] for rollup in timeline["rollups"]
    )
    assert first["artifacts"] == len(catalog["artifacts"])
    assert first["projects"] == 1
    assert second["files_changed"] == 0


def test_legacy_archive_gets_empty_catalog_without_summary_migration(
    tmp_path: Path,
) -> None:
    team = _team()

    catalog = load_artifact_catalog(tmp_path, team.team_slug, team)

    assert catalog.artifacts == ()
    assert catalog.projects == ()


def test_ingest_never_persists_cwd_or_repository_credentials(tmp_path: Path) -> None:
    team = _team()
    unsafe_source = replace(
        team.sources[0],
        working_directory="/home/private/customer/project",
        repository_url="https://alice:supersecret@github.com/example-org/example.git",
    )
    unsafe_team = replace(team, sources=(unsafe_source,))

    archived, _ = _write_ingested_team(
        tmp_path,
        unsafe_team.team_slug,
        unsafe_team,
        None,
        0,
        resolve_snapshot_root(tmp_path, unsafe_team.team_slug),
    )

    assert archived.sources[0].working_directory is None
    assert archived.sources[0].repository_url is None
    persisted = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (team_build_root(tmp_path, team.team_slug) / "raw").rglob("*.json")
    )
    assert "supersecret" not in persisted
    assert "/home/private" not in persisted
