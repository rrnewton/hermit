"""CLI behavior for optional GitHub pull metadata."""

from __future__ import annotations

from pathlib import Path

import pytest

import wrkviz.cli as timeline_cli
from wrkviz.github_enrich import PullMetadataReport


def test_partial_metadata_failure_still_rebuilds_successes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    builds: list[tuple[Path, str, int]] = []

    def fake_enrich(
        archive: Path,
        team_slug: str,
        *,
        token: str | None,
        timeout_seconds: float,
    ) -> PullMetadataReport:
        del token, timeout_seconds
        return PullMetadataReport(
            references=2,
            distinct_pulls=2,
            fetched=1,
            not_modified=0,
            failures=("owner/repository#2: GitHub API returned HTTP 404",),
            cache_path=str(archive / "pulls.json"),
        )

    def fake_build(archive: Path, team_slug: str, phase_minutes: int) -> dict[str, int]:
        builds.append((archive, team_slug, phase_minutes))
        return {"files_changed": 1}

    monkeypatch.setattr(timeline_cli, "enrich_pull_request_metadata", fake_enrich)
    monkeypatch.setattr(timeline_cli, "build_archive", fake_build)

    result = timeline_cli.main(
        [
            "github-metadata",
            "--output",
            str(tmp_path),
            "--team",
            "test-team",
            "--phase-minutes",
            "45",
        ]
    )

    assert result == 2
    assert builds == [(tmp_path, "test-team", 45)]


def test_named_token_environment_variable_must_exist(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("MISSING_TIMELINE_GITHUB_TOKEN", raising=False)
    called = False

    def fake_enrich(
        archive: Path,
        team_slug: str,
        *,
        token: str | None,
        timeout_seconds: float,
    ) -> PullMetadataReport:
        del archive, team_slug, token, timeout_seconds
        nonlocal called
        called = True
        raise AssertionError("metadata fetch must not start without the requested token")

    monkeypatch.setattr(timeline_cli, "enrich_pull_request_metadata", fake_enrich)

    result = timeline_cli.main(
        [
            "github-metadata",
            "--output",
            str(tmp_path),
            "--team",
            "test-team",
            "--github-token-env",
            "MISSING_TIMELINE_GITHUB_TOKEN",
        ]
    )

    assert result == 2
    assert not called
