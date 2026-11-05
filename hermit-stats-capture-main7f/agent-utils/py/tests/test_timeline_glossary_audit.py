"""Tests for fail-closed auditing of retired glossary candidates."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wrkviz import cli as timeline_cli
from wrkviz.archive import JsonValue, write_json_if_changed
from wrkviz.glossary_audit import (
    audit_legacy_glossaries,
    audit_legacy_term,
    format_glossary_audit,
)
from wrkviz.terminology import glossary_term_id


def _term(
    name: str,
    definition: str,
    *,
    status: str = "supported",
    context: str = "Owner discussed this name in project work.",
) -> dict[str, JsonValue]:
    return {
        "term": name,
        "term_id": glossary_term_id(name),
        "definition_status": status,
        "definition": definition,
        "context": context,
    }


def _write_legacy_glossary(
    archive: Path, team_slug: str, terms: list[dict[str, JsonValue]]
) -> None:
    term_values: list[JsonValue] = [item for item in terms]
    write_json_if_changed(
        archive
        / "teams"
        / team_slug
        / "summary_data"
        / "glossary.json",
        {"schema_version": 3, "terms": term_values},
    )


def test_legacy_glossary_audit_rejects_noise_without_publishing_candidates(
    tmp_path: Path,
) -> None:
    _write_legacy_glossary(
        tmp_path,
        "claude-test",
        [
            _term(
                "command-args",
                "A tagged transcript field.",
                context="<command-args></command-args>",
            ),
            _term(
                "and",
                "Insufficient evidence: ordinary connective text.",
                status="insufficient-evidence",
            ),
            _term("20m", "A duration used by a command scheduling example."),
            _term("get-target", "An action that prints a host target."),
            _term("4c70658e", "A commit identifier used as a baseline."),
            _term("recording_metadata", "A method used to read recording metadata."),
            _term("KVM", "A controlled execution backend in the Widget system."),
            _term("e9patch", "A binary rewriting system used as a Widget backend."),
            _term(
                "ancestry-gating Reverie pin bumps",
                "A workstream for safely updating the Reverie dependency.",
            ),
            _term("dev-widget", "The repository and shared development workspace."),
            _term("M9", "A project milestone for userspace execution."),
        ],
    )

    report = audit_legacy_glossaries(tmp_path)

    assert report.legacy_candidates == 11
    assert report.rejected_count == 6
    assert report.review_count == 5
    assert report.to_json_obj()["totals"] == {
        "teams": 1,
        "legacy_candidates": 11,
        "rejected_mechanical": 6,
        "semantic_review_required": 5,
        "published_from_legacy_cache": 0,
    }
    terms = report.teams[0].terms
    assert terms[0].reason == "transcript-markup-field"
    assert terms[1].reason == "definition-not-supported"
    assert terms[2].reason == "duration-or-command-example"
    assert terms[3].reason == "process-action"
    assert terms[4].reason == "opaque-record-identifier"
    assert terms[5].reason == "code-or-configuration-literal"
    assert terms[6].semantic_kind_hints == ("system",)
    assert terms[8].semantic_kind_hints == ("workstream",)
    assert terms[9].semantic_kind_hints == ("project",)
    assert terms[10].semantic_kind_hints == ("milestone",)
    assert all(not item.to_json_obj()["publication_authority"] for item in terms)


def test_legacy_glossary_audit_reports_teams_without_old_cache(tmp_path: Path) -> None:
    (tmp_path / "teams" / "new-team").mkdir(parents=True)

    report = audit_legacy_glossaries(tmp_path)

    assert report.teams[0].team_slug == "new-team"
    assert not report.teams[0].cache_present
    assert "new-team: no legacy glossary cache" in format_glossary_audit(
        report, "text"
    )


def test_legacy_glossary_audit_rejects_mismatched_stable_id() -> None:
    with pytest.raises(ValueError, match="does not match term"):
        audit_legacy_term(
            "e9patch",
            glossary_term_id("different"),
            "supported",
            "A binary rewriting system.",
            "The e9patch backend was tested.",
        )


def test_audit_glossary_cli_is_read_only_and_machine_readable(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _write_legacy_glossary(
        tmp_path,
        "codex-test",
        [_term("e9patch", "A binary rewriting system and backend.")],
    )
    before = tuple(
        (path.relative_to(tmp_path), path.read_bytes())
        for path in sorted(tmp_path.rglob("*"))
        if path.is_file()
    )

    status = timeline_cli.main(
        [
            "audit-glossary",
            "--output",
            str(tmp_path),
            "--format",
            "json",
            "--details",
        ]
    )

    assert status == 0
    output: object = json.loads(capsys.readouterr().out)
    assert isinstance(output, dict)
    assert output["publication_policy"] == "semantic-only-v1"
    assert output["legacy_cache_publication_authority"] is False
    assert output["teams"][0]["terms"][0]["term"] == "e9patch"
    after = tuple(
        (path.relative_to(tmp_path), path.read_bytes())
        for path in sorted(tmp_path.rglob("*"))
        if path.is_file()
    )
    assert after == before
