"""Tests for strict, zero-model project ingestion manifests."""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import replace
import json
from pathlib import Path
import shutil
from typing import cast

import pytest

import wrkviz.cli as cli_module
import wrkviz.project_config as project_config_module
from wrkviz.build_store import team_build_root
from wrkviz.claude import ClaudeParseError
from wrkviz.cli import main as timeline_main
from wrkviz.identity import IdentityOverrides
from wrkviz.model import TeamData
from wrkviz.orc import (
    OrcAppendPrefixOverride,
    OrcContinuationSpec,
    OrcPrefixColumnChange,
    OrcPrefixRowChange,
)
from wrkviz.pipeline import IngestReport
from wrkviz.project_config import (
    ClaudeProjectSource,
    CodexProjectSource,
    OrcProjectSource,
    ProjectIngestReport,
    ProjectTeamIngestFailure,
    ProjectTeamIngestResult,
    ingest_project,
    load_project_ingest_config,
)
from wrkviz.transcript_export import (
    DroppedAuthorshipRule,
    PromptAuthorshipRule,
    TranscriptExportReport,
    TranscriptTeamSkip,
)
from wrkviz.window import DateWindow


CLAUDE_SESSION_ID = "11111111-1111-4111-8111-111111111111"
CLAUDE_FIXTURE_ROOT = Path(__file__).parent / "fixtures" / "claude"


def _manifest(output: str = "../summary/widget") -> dict[str, object]:
    return {
        "schema_version": 1,
        "output": output,
        "timezone": "America/New_York",
        "projects": [
            {
                "label": "dev-widget",
                "repository_url": "https://github.com/example-org/dev-widget.git",
            }
        ],
        "source_hosts": ["shared.example.com"],
        "window": {"start_date": "2026-08-01", "end_date": "2026-08-13"},
        "teams": [
            {
                "slug": "codex-team",
                "provider": "codex",
                "source_hosts": ["codex.example.com"],
                "source": {
                    "sessions_root": "../raw/devbig014/.codex/sessions",
                    "root_session": "codex-root",
                    "continuation_sessions": ["codex-next", "codex-last"],
                },
            },
            {
                "slug": "claude-team",
                "provider": "claude",
                "timezone": "UTC",
                "source": {
                    "session_file": "../raw/devbig176/.claude/projects/p/root.jsonl"
                },
            },
            {
                "slug": "orc-team",
                "provider": "orc",
                "projects": [],
                "window": {},
                "prompt_authorship_rules": [
                    {
                        "id": "legacy-owner-web",
                        "ingress_kind": "submitted_web",
                        "author_kind": "owner_human",
                        "reason": "This audited legacy interval was owner-only.",
                        "end_time": "2026-08-10T00:00:00Z",
                    }
                ],
                "source": {
                    "source_root": "../raw/devbig014",
                    "root_session": "orc-root",
                    "continuation_sessions": [
                        "orc-next",
                        {
                            "session_id": "orc-last",
                            "start_message_id": "restart-message",
                        },
                    ],
                },
            },
        ],
    }


def _write_manifest(tmp_path: Path, value: dict[str, object]) -> Path:
    path = tmp_path / "configs" / "widget.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(value), encoding="utf-8")
    return path


def _report(team_slug: str) -> IngestReport:
    return IngestReport(team_slug, "a" * 64, 1, 10, 2, 3, 4, 1, 1)


def _orc_spec(value: str | OrcContinuationSpec) -> OrcContinuationSpec:
    return (
        value
        if isinstance(value, OrcContinuationSpec)
        else OrcContinuationSpec.from_value(value, "test")
    )


def _set_new_schema(value: dict[str, object]) -> None:
    value["schema_version"] = 2


def _add_unknown_root(value: dict[str, object]) -> None:
    value["unexpected"] = True


def _set_absolute_output(value: dict[str, object]) -> None:
    value["output"] = "/tmp/unsafe"


def _add_unknown_source(value: dict[str, object]) -> None:
    teams = cast(list[dict[str, object]], value["teams"])
    source = cast(dict[str, object], teams[0]["source"])
    source["mystery"] = "x"


def test_load_project_config_resolves_paths_and_provider_schema(tmp_path: Path) -> None:
    config_path = _write_manifest(tmp_path, _manifest())

    config = load_project_ingest_config(config_path)

    assert config.output == (tmp_path / "summary" / "widget").resolve()
    assert tuple(team.slug for team in config.teams) == (
        "codex-team",
        "claude-team",
        "orc-team",
    )
    codex, claude, orc = config.teams
    assert isinstance(codex.source, CodexProjectSource)
    assert codex.source.sessions_root == (
        tmp_path / "raw" / "devbig014" / ".codex" / "sessions"
    ).resolve()
    assert codex.source.continuation_sessions == ("codex-next", "codex-last")
    assert codex.date_window is not None
    assert codex.date_window.start_date == "2026-08-01"
    assert codex.identity_overrides.projects[0].repository_url == (
        "https://github.com/example-org/dev-widget"
    )
    assert codex.identity_overrides.hosts[0].hostname == "codex.example.com"
    assert isinstance(claude.source, ClaudeProjectSource)
    assert claude.timezone == "UTC"
    assert isinstance(orc.source, OrcProjectSource)
    assert orc.source.continuation_sessions == (
        OrcContinuationSpec("orc-next"),
        OrcContinuationSpec("orc-last", "restart-message"),
    )
    assert orc.date_window is None
    assert orc.identity_overrides.projects == ()
    assert orc.prompt_authorship_rules[0].rule_id == "legacy-owner-web"
    assert orc.prompt_authorship_rules[0].end_ms == 1786320000000


@pytest.mark.parametrize(
    ("mutate", "match"),
    (
        (_set_new_schema, "unsupported schema_version"),
        (_add_unknown_root, "unknown=\\['unexpected'\\]"),
        (_set_absolute_output, "output must be relative"),
        (_add_unknown_source, "unknown=\\['mystery'\\]"),
    ),
)
def test_project_config_rejects_schema_drift(
    tmp_path: Path,
    mutate: Callable[[dict[str, object]], None],
    match: str,
) -> None:
    value = _manifest()
    mutate(value)
    path = _write_manifest(tmp_path, value)
    with pytest.raises(ValueError, match=match):
        load_project_ingest_config(path)


def test_project_config_rejects_root_repeated_as_orc_continuation(
    tmp_path: Path,
) -> None:
    value = _manifest()
    teams = cast(list[dict[str, object]], value["teams"])
    source = cast(dict[str, object], teams[2]["source"])
    source["continuation_sessions"] = ["orc-next", "orc-root"]

    with pytest.raises(ValueError, match="root_session cannot also be a continuation"):
        load_project_ingest_config(_write_manifest(tmp_path, value))


def test_project_config_keeps_orc_continuations_optional(tmp_path: Path) -> None:
    value = _manifest()
    teams = cast(list[dict[str, object]], value["teams"])
    source = cast(dict[str, object], teams[2]["source"])
    del source["continuation_sessions"]

    config = load_project_ingest_config(_write_manifest(tmp_path, value))
    orc = config.teams[2].source

    assert isinstance(orc, OrcProjectSource)
    assert orc.continuation_sessions == ()


@pytest.mark.parametrize(
    ("continuations", "match"),
    (
        (
            [
                "orc-next",
                {
                    "session_id": "orc-next",
                    "start_message_id": "restart-message",
                },
            ],
            "duplicate session ids",
        ),
        (
            [{"session_id": "orc-next", "start_message_id": None}],
            "expected a non-empty string",
        ),
    ),
)
def test_project_config_rejects_noncanonical_orc_continuation_specs(
    tmp_path: Path,
    continuations: list[object],
    match: str,
) -> None:
    value = _manifest()
    teams = cast(list[dict[str, object]], value["teams"])
    source = cast(dict[str, object], teams[2]["source"])
    source["continuation_sessions"] = continuations

    with pytest.raises(ValueError, match=match):
        load_project_ingest_config(_write_manifest(tmp_path, value))


@pytest.mark.parametrize("command", ("ingest-orc", "refresh-orc"))
def test_orc_cli_accepts_ordered_repeatable_continuation_sessions(
    command: str,
) -> None:
    namespace = cli_module._parser().parse_args(
        [
            command,
            "--output",
            "/tmp/archive",
            "--team",
            "orc-test",
            "--source-root",
            "/tmp/source",
            "--root-session",
            "orc-root",
            "--continuation-session",
            "orc-next",
            "--continuation-session",
            "orc-last",
        ]
    )

    assert namespace.continuation_session == ["orc-next", "orc-last"]


def test_orc_cli_parses_legacy_and_bounded_continuation_specs() -> None:
    assert cli_module._orc_continuation_arg("orc-next") == OrcContinuationSpec(
        "orc-next"
    )
    assert cli_module._orc_continuation_arg(
        '{"session_id":"orc-last","start_message_id":"restart-message"}'
    ) == OrcContinuationSpec("orc-last", "restart-message")
    with pytest.raises(ValueError, match="expected a non-empty string"):
        cli_module._orc_continuation_arg(
            '{"session_id":"orc-last","start_message_id":null}'
        )


def test_orc_cli_dispatches_ordered_continuation_sessions(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    received: list[tuple[str, ...]] = []

    def fake_orc(
        archive: Path,
        source_root: Path,
        root_session_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_specs: Sequence[str | OrcContinuationSpec] = (),
        accept_prefix_rewrite: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        normalized = tuple(_orc_spec(spec) for spec in continuation_specs)
        received.append(
            tuple(
                spec.session_id
                + (
                    "=" + spec.start_message_id
                    if spec.start_message_id is not None
                    else ""
                )
                for spec in normalized
            )
        )
        return cast(TeamData, object()), _report(team_slug)

    monkeypatch.setattr(cli_module, "ingest_orc", fake_orc)

    assert timeline_main(
        [
            "ingest-orc",
            "--output",
            str(tmp_path / "archive"),
            "--team",
            "orc-test",
            "--source-root",
            str(tmp_path / "source"),
            "--root-session",
            "orc-root",
            "--continuation-session",
            "orc-next",
            "--continuation-session",
            '{"session_id":"orc-last","start_message_id":"restart-message"}',
        ]
    ) == 0
    assert received == [("orc-next", "orc-last=restart-message")]


def test_project_ingest_dispatches_all_providers_then_extracts_all(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[tuple[str, ...]] = []

    def fake_codex(
        archive: Path,
        sessions_root: Path,
        root_thread_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_thread_ids: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        calls.append(
            (
                "codex",
                str(archive),
                str(sessions_root),
                root_thread_id,
                team_slug,
                display_timezone,
                *continuation_thread_ids,
            )
        )
        return cast(TeamData, object()), _report(team_slug)

    def fake_claude(
        archive: Path,
        session_file: Path,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        calls.append(
            (
                "claude",
                str(archive),
                str(session_file),
                team_slug,
                display_timezone,
            )
        )
        return cast(TeamData, object()), _report(team_slug)

    def fake_orc(
        archive: Path,
        source_root: Path,
        root_session_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_specs: Sequence[str | OrcContinuationSpec] = (),
        accept_prefix_rewrite: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        normalized = tuple(_orc_spec(spec) for spec in continuation_specs)
        calls.append(
            (
                "orc",
                str(archive),
                str(source_root),
                root_session_id,
                team_slug,
                display_timezone,
                *(
                    spec.session_id
                    + (
                        "=" + spec.start_message_id
                        if spec.start_message_id is not None
                        else ""
                    )
                    for spec in normalized
                ),
            )
        )
        return cast(TeamData, object()), _report(team_slug)

    def fake_extract(
        archive: Path,
        team_slugs: Sequence[str] = (),
        authorship_rules: Sequence[PromptAuthorshipRule] | None = None,
    ) -> TranscriptExportReport:
        calls.append(
            (
                "extract",
                str(archive),
                *(rule.rule_id for rule in authorship_rules or ()),
                *team_slugs,
            )
        )
        return TranscriptExportReport(3, 20, 30, 4, 0, 2)

    monkeypatch.setattr(project_config_module, "ingest_codex", fake_codex)
    monkeypatch.setattr(project_config_module, "ingest_claude", fake_claude)
    monkeypatch.setattr(project_config_module, "ingest_orc", fake_orc)
    monkeypatch.setattr(project_config_module, "extract_transcripts_archive", fake_extract)

    result = ingest_project(config)

    assert tuple(item.team_slug for item in result.teams) == (
        "codex-team",
        "claude-team",
        "orc-team",
    )
    assert [call[0] for call in calls] == ["codex", "claude", "orc", "extract"]
    assert calls[2][-2:] == ("orc-next", "orc-last=restart-message")
    assert calls[-1] == ("extract", str(config.output), "legacy-owner-web")
    assert result.to_json_obj()["model_calls"] == 0
    assert result.to_json_obj()["website_build_performed"] is False


def _install_isolation_fakes(
    monkeypatch: pytest.MonkeyPatch,
    calls: list[str],
    team_errors: dict[str, BaseException] | None = None,
    extract_error: BaseException | None = None,
    extract_report: TranscriptExportReport | None = None,
) -> None:
    """Fake all three importers plus extraction, failing exactly the named teams.

    The per-team dispatch is exercised through the real ``ingest_project`` so the test observes the
    isolation boundary itself rather than a re-implementation of it. ``calls`` records what actually
    ran, which is the only way to prove that a team after a failing team still executed.
    """

    errors = team_errors or {}

    def fail_or_report(team_slug: str) -> IngestReport:
        calls.append(team_slug)
        error = errors.get(team_slug)
        if error is not None:
            raise error
        return _report(team_slug)

    def fake_codex(
        archive: Path,
        sessions_root: Path,
        root_thread_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_thread_ids: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        return cast(TeamData, object()), fail_or_report(team_slug)

    def fake_claude(
        archive: Path,
        session_file: Path,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        return cast(TeamData, object()), fail_or_report(team_slug)

    def fake_orc(
        archive: Path,
        source_root: Path,
        root_session_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_specs: Sequence[str | OrcContinuationSpec] = (),
        accept_prefix_rewrite: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        return cast(TeamData, object()), fail_or_report(team_slug)

    def fake_extract(
        archive: Path,
        team_slugs: Sequence[str] = (),
        authorship_rules: Sequence[PromptAuthorshipRule] | None = None,
    ) -> TranscriptExportReport:
        calls.append("extract")
        if extract_error is not None:
            raise extract_error
        if extract_report is not None:
            return extract_report
        return TranscriptExportReport(3, 20, 30, 4, 0, 2)

    monkeypatch.setattr(project_config_module, "ingest_codex", fake_codex)
    monkeypatch.setattr(project_config_module, "ingest_claude", fake_claude)
    monkeypatch.setattr(project_config_module, "ingest_orc", fake_orc)
    monkeypatch.setattr(project_config_module, "extract_transcripts_archive", fake_extract)


def test_project_ingest_isolates_one_failing_team(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(
        monkeypatch,
        calls,
        {"claude-team": ClaudeParseError("session file ended mid-record")},
    )

    result = ingest_project(config)

    # The team after the failing one ran, and so did extraction. Before per-team isolation, both
    # were unreachable: one raise ended the whole project run.
    assert calls == ["codex-team", "claude-team", "orc-team", "extract"]
    assert tuple(item.team_slug for item in result.teams) == ("codex-team", "orc-team")
    assert tuple(item.team_slug for item in result.failures) == ("claude-team",)
    failure = result.failures[0]
    assert failure.provider == "claude"
    assert failure.error_type == "ClaudeParseError"
    assert failure.error == "session file ended mid-record"
    assert failure.traceback is None
    assert result.transcripts is not None
    assert result.failed is True
    payload = result.to_json_obj()
    assert payload["status"] == "failed"
    assert payload["teams_succeeded"] == 2
    assert payload["teams_failed"] == 1
    assert payload["failed_teams"] == [
        {
            "team_slug": "claude-team",
            "provider": "claude",
            "error_type": "ClaudeParseError",
            "error": "session file ended mid-record",
        }
    ]
    summary = result.failure_summary()
    assert summary is not None
    assert "1 of 3 teams failed" in summary
    assert (
        "claude-team (claude): ClaudeParseError: session file ended mid-record"
        in summary
    )


def test_project_ingest_records_every_failure_when_several_teams_fail(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(
        monkeypatch,
        calls,
        {
            "codex-team": OSError("sessions root is not mounted"),
            "orc-team": ValueError("root_session is not in the snapshot"),
        },
    )

    result = ingest_project(config)

    assert calls == ["codex-team", "claude-team", "orc-team", "extract"]
    assert tuple(item.team_slug for item in result.teams) == ("claude-team",)
    # Failures keep manifest order, so the receipt reads in the same sequence as the config file.
    assert tuple(item.team_slug for item in result.failures) == (
        "codex-team",
        "orc-team",
    )
    assert tuple(item.error_type for item in result.failures) == (
        "OSError",
        "ValueError",
    )
    assert result.transcripts is not None
    summary = result.failure_summary()
    assert summary is not None
    assert "2 of 3 teams failed" in summary
    assert "sessions root is not mounted" in summary
    assert "root_session is not in the snapshot" in summary


def test_project_ingest_keeps_the_traceback_of_an_unclassified_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(monkeypatch, calls, {"codex-team": KeyError("messages")})

    result = ingest_project(config)

    assert calls == ["codex-team", "claude-team", "orc-team", "extract"]
    failure = result.failures[0]
    assert failure.error_type == "KeyError"
    # A KeyError's str() is just the missing key, which diagnoses nothing on its own. An
    # unclassified exception is a defect in this package, so the receipt keeps its traceback.
    assert failure.traceback is not None
    assert "KeyError" in failure.traceback
    assert failure.to_json_obj()["traceback"] == failure.traceback


def test_project_ingest_does_not_swallow_an_interrupt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(
        monkeypatch, calls, {"codex-team": KeyboardInterrupt()}
    )

    # Continuing to the next team after Ctrl-C would be ignoring the operator, not surviving a bad
    # team. BaseException must still end the run.
    with pytest.raises(KeyboardInterrupt):
        ingest_project(config)
    assert calls == ["codex-team"]


def test_project_ingest_reports_a_failed_extraction_without_losing_team_results(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(
        monkeypatch,
        calls,
        extract_error=ValueError("stale or incomplete Orc normalized generation"),
    )

    result = ingest_project(config)

    assert len(result.teams) == 3
    assert result.failures == ()
    assert result.transcripts is None
    assert result.transcript_error == (
        "ValueError: stale or incomplete Orc normalized generation"
    )
    assert result.failed is True
    payload = result.to_json_obj()
    assert payload["status"] == "failed"
    assert payload["transcript_extraction"] is None
    assert payload["transcript_extraction_error"] == (
        "ValueError: stale or incomplete Orc normalized generation"
    )
    summary = result.failure_summary()
    assert summary is not None
    assert summary.startswith("transcript extraction failed: ValueError:")


def test_project_ingest_reports_a_partial_projection_as_a_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Every team ingested cleanly and the projection is still incomplete.

    This combination is reachable and unremarkable: extraction runs over every normalized team in
    the archive, not over this run's selection, so a team that `--team` excluded -- or one whose
    ingest was never attempted because it belongs to a different config -- can be unreadable while
    every team this run touched succeeded. `failures` is therefore no evidence at all about the
    projection, and an exit status derived from it alone would report this run as clean.
    """

    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(
        monkeypatch,
        calls,
        extract_report=TranscriptExportReport(
            3,
            20,
            30,
            4,
            0,
            2,
            0,
            (TranscriptTeamSkip("stale-team", "ValueError", "torn generation"),),
            (DroppedAuthorshipRule("newcomer-owner", "newcomer"),),
        ),
    )

    result = ingest_project(config)

    assert result.failures == ()
    assert result.transcript_error is None
    assert result.transcripts is not None
    assert result.failed is True
    summary = result.failure_summary()
    assert summary is not None
    assert summary.startswith("transcript extraction was partial: ")
    assert "1 of 4 archive teams could not be read" in summary
    assert "stale-team: ValueError: torn generation" in summary
    assert "newcomer-owner (team newcomer has no normalized data)" in summary
    payload = result.to_json_obj()
    assert payload["status"] == "failed"
    extraction = cast(dict[str, object], payload["transcript_extraction"])
    assert extraction["teams_skipped"] == 1
    assert extraction["skipped_teams"] == [
        {
            "team_slug": "stale-team",
            "error_type": "ValueError",
            "error": "torn generation",
        }
    ]
    assert extraction["dropped_prompt_authorship_rules"] == [
        {"rule_id": "newcomer-owner", "team_slug": "newcomer"}
    ]


def test_ingest_project_cli_shouts_about_a_partial_projection(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config_path = _write_manifest(tmp_path, _manifest())

    def fake_ingest(
        config: project_config_module.ProjectIngestConfig,
        requested_teams: Sequence[str] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> ProjectIngestReport:
        return ProjectIngestReport(
            config.config_sha256,
            (ProjectTeamIngestResult("codex-team", "codex", _report("codex-team")),),
            TranscriptExportReport(
                1,
                7,
                8,
                1,
                0,
                4,
                0,
                (TranscriptTeamSkip("orc-team", "ValueError", "torn generation"),),
                (DroppedAuthorshipRule("newcomer-owner", "newcomer"),),
            ),
        )

    monkeypatch.setattr(cli_module, "ingest_project", fake_ingest)

    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 2
    captured = capsys.readouterr()
    assert "teams: 1 succeeded, 0 failed" in captured.out
    # The count on stdout must not read as complete, and the reason must be on stderr where a
    # scheduled run's log keeps it even when stdout is redirected to a file.
    assert (
        "across 1 normalized archive teams (1 further team(s) carried forward unread)"
        in captured.out
    )
    assert "JSONL:" in captured.out
    assert "transcripts: team orc-team: ValueError: torn generation" in captured.err
    assert "carried forward unchanged from its last good extraction" in captured.err
    assert (
        "prompt authorship rule newcomer-owner (team newcomer has no normalized data) "
        "-- not applied" in captured.err
    )
    run_paths = tuple((tmp_path / "summary" / "widget" / "runs").glob("*.json"))
    run = json.loads(run_paths[0].read_text(encoding="utf-8"))
    assert run["status"] == "failed"
    assert "transcript extraction was partial" in run["error"]


def test_project_ingest_reports_a_clean_run_as_completed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    calls: list[str] = []
    _install_isolation_fakes(monkeypatch, calls)

    result = ingest_project(config)

    assert result.failed is False
    assert result.failure_summary() is None
    payload = result.to_json_obj()
    assert payload["status"] == "completed"
    assert payload["failed_teams"] == []
    assert payload["teams_failed"] == 0
    assert payload["transcript_extraction_error"] is None


def test_project_config_team_filter_is_strict(tmp_path: Path) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    assert tuple(
        team.slug
        for team in config.select_teams(("orc-team", "codex-team"))
    ) == ("codex-team", "orc-team")
    with pytest.raises(ValueError, match="not registered"):
        config.select_teams(("missing-team",))
    with pytest.raises(ValueError, match="duplicates"):
        config.select_teams(("codex-team", "codex-team"))


def test_ingest_project_cli_records_zero_model_contract(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config_path = _write_manifest(tmp_path, _manifest())

    def fake_ingest(
        config: project_config_module.ProjectIngestConfig,
        requested_teams: Sequence[str] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> ProjectIngestReport:
        assert requested_teams == ("codex-team",)
        return ProjectIngestReport(
            config.config_sha256,
            (
                ProjectTeamIngestResult(
                    "codex-team", "codex", _report("codex-team")
                ),
            ),
            TranscriptExportReport(1, 7, 8, 1, 0, 4),
        )

    monkeypatch.setattr(cli_module, "ingest_project", fake_ingest)

    assert timeline_main(
        ["ingest-project", "--config", str(config_path), "--team", "codex-team"]
    ) == 0
    output = capsys.readouterr().out
    assert "teams: 1 succeeded, 0 failed" in output
    assert "7 prompts" in output
    assert "website: not built" in output
    run_paths = tuple((tmp_path / "summary" / "widget" / "runs").glob("*.json"))
    assert len(run_paths) == 1
    run = json.loads(run_paths[0].read_text(encoding="utf-8"))
    assert run["status"] == "completed"
    assert run["error"] is None
    assert run["mechanical"]["project_ingest"]["model_calls"] == 0
    assert run["mechanical"]["project_ingest"]["website_build_performed"] is False


def _claude_team(slug: str, session_file: Path) -> dict[str, object]:
    return {
        "slug": slug,
        "provider": "claude",
        "source": {"session_file": str(session_file)},
    }


def test_ingest_project_keeps_a_failed_team_in_the_projection_end_to_end(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Run the real importers and the real exporter, then break one team's source.

    Everything above this test fakes the provider boundary, which cannot show what happens to the
    archive on disk. This one proves the property the isolation depends on: a team whose ingest
    raises keeps its normalized snapshot byte for byte, so the transcript projection that runs
    afterwards still contains that team's prompts alongside the refreshed team's.
    """

    first_root = tmp_path / "sources" / "one"
    second_root = tmp_path / "sources" / "two"
    shutil.copytree(CLAUDE_FIXTURE_ROOT, first_root)
    shutil.copytree(CLAUDE_FIXTURE_ROOT, second_root)
    first_session = first_root / f"{CLAUDE_SESSION_ID}.jsonl"
    second_session = second_root / f"{CLAUDE_SESSION_ID}.jsonl"
    config_path = _write_manifest(
        tmp_path,
        {
            "schema_version": 1,
            "output": "../archive",
            "timezone": "UTC",
            "teams": [
                _claude_team("team-one", first_session),
                _claude_team("team-two", second_session),
            ],
        },
    )
    archive = tmp_path / "archive"
    prompts_path = archive / "extracted" / "transcripts" / "prompts.jsonl"

    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 0
    assert "teams: 2 succeeded, 0 failed" in capsys.readouterr().out
    normalized_two = team_build_root(archive, "team-two") / "raw" / "team.json"
    snapshot_before = normalized_two.read_bytes()
    prompts_before = prompts_path.read_text(encoding="utf-8")
    assert prompts_before

    # A provider-side corruption of exactly one team's source, which is the shape of the failure
    # this isolation exists for: the bytes are there, they no longer parse.
    second_session.write_text("{ this is not a transcript\n", encoding="utf-8")

    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 2
    captured = capsys.readouterr()
    assert "teams: 1 succeeded, 1 failed" in captured.out
    assert "team team-two (claude):" in captured.err
    # The healthy team was still refreshed and the projection still ran over both teams.
    assert "team-one (claude): ingest:" in captured.out
    assert "2 normalized archive teams" in captured.out
    assert normalized_two.read_bytes() == snapshot_before
    prompts_after = prompts_path.read_text(encoding="utf-8")
    assert prompts_after == prompts_before
    occurrence_teams = {
        team
        for line in prompts_after.splitlines()
        for team in json.loads(line)["occurrence_teams"]
    }
    assert occurrence_teams == {"team-one", "team-two"}

    runs = sorted((archive / "runs").glob("*.json"))
    assert len(runs) == 2
    failed_run = json.loads(runs[-1].read_text(encoding="utf-8"))
    assert failed_run["status"] == "failed"
    project = failed_run["mechanical"]["project_ingest"]
    assert [item["team_slug"] for item in project["teams"]] == ["team-one"]
    assert [item["team_slug"] for item in project["failed_teams"]] == ["team-two"]
    assert project["transcript_extraction"]["teams"] == 2


def test_ingest_project_cli_reports_partial_failure_loudly(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config_path = _write_manifest(tmp_path, _manifest())

    def fake_ingest(
        config: project_config_module.ProjectIngestConfig,
        requested_teams: Sequence[str] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> ProjectIngestReport:
        return ProjectIngestReport(
            config.config_sha256,
            (
                ProjectTeamIngestResult(
                    "codex-team", "codex", _report("codex-team")
                ),
                ProjectTeamIngestResult(
                    "claude-team", "claude", _report("claude-team")
                ),
            ),
            TranscriptExportReport(3, 7, 8, 1, 0, 4),
            (
                ProjectTeamIngestFailure(
                    "orc-team",
                    "orc",
                    "OrcParseError",
                    "Orc session existing append prefix was rewritten for state/x.db",
                ),
            ),
        )

    monkeypatch.setattr(cli_module, "ingest_project", fake_ingest)

    # Non-zero exit is the whole point: a partial run that exits 0 is a silent data gap in every
    # caller that checks only the status code.
    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 2
    captured = capsys.readouterr()
    assert "teams: 2 succeeded, 1 failed" in captured.out
    # The healthy teams still report their counts, and extraction still ran.
    assert "codex-team (codex):" in captured.out
    assert "7 prompts" in captured.out
    assert (
        "team orc-team (orc): OrcParseError: Orc session existing append prefix "
        "was rewritten for state/x.db" in captured.err
    )
    run_paths = tuple((tmp_path / "summary" / "widget" / "runs").glob("*.json"))
    assert len(run_paths) == 1
    run = json.loads(run_paths[0].read_text(encoding="utf-8"))
    assert run["status"] == "failed"
    assert "1 of 3 teams failed" in run["error"]
    project = run["mechanical"]["project_ingest"]
    assert project["status"] == "failed"
    assert project["teams_succeeded"] == 2
    assert project["teams_failed"] == 1
    assert project["failed_teams"][0]["team_slug"] == "orc-team"
    assert project["failed_teams"][0]["error_type"] == "OrcParseError"
    assert project["transcript_extraction"]["prompts"] == 7
    manifest = json.loads(
        (tmp_path / "summary" / "widget" / "manifest.json").read_text(encoding="utf-8")
    )
    assert manifest["last_run_status"] == "failed"


def test_ingest_project_cli_reports_a_failed_extraction(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config_path = _write_manifest(tmp_path, _manifest())

    def fake_ingest(
        config: project_config_module.ProjectIngestConfig,
        requested_teams: Sequence[str] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> ProjectIngestReport:
        return ProjectIngestReport(
            config.config_sha256,
            (
                ProjectTeamIngestResult(
                    "codex-team", "codex", _report("codex-team")
                ),
            ),
            None,
            (),
            "ValueError: no ingested teams found",
        )

    monkeypatch.setattr(cli_module, "ingest_project", fake_ingest)

    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 2
    captured = capsys.readouterr()
    assert "teams: 1 succeeded, 0 failed" in captured.out
    assert "transcripts: not extracted" in captured.out
    # Naming prompts.jsonl after a failed extraction would advertise stale bytes as fresh output.
    assert "JSONL:" not in captured.out
    assert "transcript extraction failed: ValueError: no ingested teams found" in (
        captured.err
    )
    run_paths = tuple((tmp_path / "summary" / "widget" / "runs").glob("*.json"))
    run = json.loads(run_paths[0].read_text(encoding="utf-8"))
    assert run["status"] == "failed"
    project = run["mechanical"]["project_ingest"]
    assert project["transcript_extraction"] is None
    assert project["transcript_extraction_error"] == (
        "ValueError: no ingested teams found"
    )


def _install_prefix_rewrite_probe(
    monkeypatch: pytest.MonkeyPatch, granted: dict[str, tuple[str, ...]]
) -> None:
    """Record, per team, the exact session ids each importer was actually handed.

    A boolean probe could not have caught the defect this pins: it can say *that* a team was
    authorized but not *which of its sessions*, which is the whole distinction.
    """

    def fake_codex(
        archive: Path,
        sessions_root: Path,
        root_thread_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_thread_ids: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        return cast(TeamData, object()), _report(team_slug)

    def fake_claude(
        archive: Path,
        session_file: Path,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        return cast(TeamData, object()), _report(team_slug)

    def fake_orc(
        archive: Path,
        source_root: Path,
        root_session_id: str,
        team_slug: str,
        display_timezone: str,
        date_window: DateWindow | None = None,
        identity_overrides: IdentityOverrides | None = None,
        continuation_specs: Sequence[str | OrcContinuationSpec] = (),
        accept_prefix_rewrite: Sequence[str] = (),
        snapshot_root: Path | None = None,
    ) -> tuple[TeamData, IngestReport]:
        granted[team_slug] = tuple(accept_prefix_rewrite)
        return cast(TeamData, object()), _report(team_slug)

    def fake_extract(
        archive: Path,
        team_slugs: Sequence[str] = (),
        authorship_rules: Sequence[PromptAuthorshipRule] | None = None,
    ) -> TranscriptExportReport:
        return TranscriptExportReport(3, 20, 30, 4, 0, 2)

    monkeypatch.setattr(project_config_module, "ingest_codex", fake_codex)
    monkeypatch.setattr(project_config_module, "ingest_claude", fake_claude)
    monkeypatch.setattr(project_config_module, "ingest_orc", fake_orc)
    monkeypatch.setattr(
        project_config_module, "extract_transcripts_archive", fake_extract
    )


def test_project_prefix_rewrite_override_reaches_only_the_named_team_and_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    value = _manifest()
    teams = cast(list[dict[str, object]], value["teams"])
    teams.append(
        {
            "slug": "orc-other",
            "provider": "orc",
            "projects": [],
            "source": {
                "source_root": "../raw/devbig015",
                "root_session": "orc-other-root",
            },
        }
    )
    config = load_project_ingest_config(_write_manifest(tmp_path, value))
    granted: dict[str, tuple[str, ...]] = {}
    _install_prefix_rewrite_probe(monkeypatch, granted)

    report = ingest_project(
        config, (), ("orc-team:session-a", "orc-team:session-b")
    )

    # Two sessions of one team, and nothing at all for the team nobody looked at. The receipt keeps
    # the pairs rather than the slug, because "orc-team was permitted" would read as permission
    # over that team's whole session tree.
    assert granted == {
        "orc-team": ("session-a", "session-b"),
        "orc-other": (),
    }
    assert report.accept_prefix_rewrite_sessions == (
        "orc-team:session-a",
        "orc-team:session-b",
    )
    assert report.to_json_obj()["accepted_prefix_rewrite_sessions"] == [
        "orc-team:session-a",
        "orc-team:session-b",
    ]


def test_project_clean_run_records_no_prefix_rewrite_grant(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    granted: dict[str, tuple[str, ...]] = {}
    _install_prefix_rewrite_probe(monkeypatch, granted)

    report = ingest_project(config)

    assert granted == {"orc-team": ()}
    assert report.accept_prefix_rewrite_sessions == ()
    assert report.to_json_obj()["accepted_prefix_rewrite_sessions"] == []


@pytest.mark.parametrize(
    ("requested_teams", "accepted", "match"),
    (
        (
            (),
            ("orc-team:s1", "orc-team:s1"),
            "contains duplicates",
        ),
        ((), ("orc-teem:s1",), "does not ingest: orc-teem"),
        ((), ("codex-team:s1",), "applies only to orc teams"),
        (("codex-team",), ("orc-team:s1",), "does not ingest: orc-team"),
        # The paste an operator actually makes: `ingest-orc`'s refusal prints the bare session id,
        # and pasting it here has to say what is missing rather than be read as a team slug.
        ((), ("11111111-1111-1111-1111-111111111111",), "takes TEAM:SESSION"),
        ((), ("orc-team",), "takes TEAM:SESSION"),
        ((), ("orc-team:",), "takes TEAM:SESSION"),
        ((), (":session-a",), "takes TEAM:SESSION"),
    ),
)
def test_project_prefix_rewrite_override_rejects_every_near_miss(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    requested_teams: tuple[str, ...],
    accepted: tuple[str, ...],
    match: str,
) -> None:
    config = load_project_ingest_config(_write_manifest(tmp_path, _manifest()))
    granted: dict[str, tuple[str, ...]] = {}
    _install_prefix_rewrite_probe(monkeypatch, granted)

    with pytest.raises(ValueError, match=match):
        ingest_project(config, requested_teams, accepted)

    # Rejected before anything ran, so a mistyped override cannot half-ingest a project.
    assert granted == {}


def test_ingest_project_cli_passes_named_prefix_rewrite_sessions_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config_path = _write_manifest(tmp_path, _manifest())
    seen: list[tuple[str, ...]] = []

    def fake_ingest(
        config: project_config_module.ProjectIngestConfig,
        requested_teams: Sequence[str] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> ProjectIngestReport:
        seen.append(tuple(accept_prefix_rewrite))
        return ProjectIngestReport(
            config.config_sha256,
            (ProjectTeamIngestResult("orc-team", "orc", _report("orc-team")),),
            TranscriptExportReport(1, 7, 8, 1, 0, 4),
            (),
            None,
            tuple(accept_prefix_rewrite),
        )

    monkeypatch.setattr(cli_module, "ingest_project", fake_ingest)

    assert (
        timeline_main(
            [
                "ingest-project",
                "--config",
                str(config_path),
                "--team",
                "orc-team",
                "--accept-orc-prefix-rewrite",
                "orc-team:orc-root",
            ]
        )
        == 0
    )

    assert seen == [("orc-team:orc-root",)]


def test_ingest_project_cli_shouts_about_an_accepted_prefix_rewrite(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config_path = _write_manifest(tmp_path, _manifest())
    override = OrcAppendPrefixOverride(
        policy="operator-accepted-prefix-rewrite-v1",
        source_path=".orc/sessions/orc-root/session.db",
        accepted_at="2026-08-22T00:00:00.000000+00:00",
        override_count=1,
        previous_append_prefix_sha256="a" * 64,
        observed_append_prefix_sha256="b" * 64,
        superseded_snapshot_path=f".objects/cc/{'c' * 64}.db",
        superseded_sha256="c" * 64,
        changed_row_count=1,
        changed_rows=(
            OrcPrefixRowChange(
                table="messages",
                row_id=878292,
                columns=(
                    OrcPrefixColumnChange(
                        column="message_json",
                        previous='text:..."token_count":null}',
                        observed='text:..."token_count":445}',
                        bounded=True,
                        json_paths=("token_count",),
                        json_paths_bounded=False,
                    ),
                ),
            ),
        ),
        changed_rows_bounded=False,
        degraded=True,
        degradation_reason=(
            "append-prefix-rewritten-operator-accepted-rows-preserved"
        ),
    )

    def fake_ingest(
        config: project_config_module.ProjectIngestConfig,
        requested_teams: Sequence[str] = (),
        accept_prefix_rewrite: Sequence[str] = (),
    ) -> ProjectIngestReport:
        return ProjectIngestReport(
            config.config_sha256,
            (
                ProjectTeamIngestResult(
                    "orc-team",
                    "orc",
                    replace(
                        _report("orc-team"), orc_prefix_overrides=(override,)
                    ),
                ),
            ),
            TranscriptExportReport(1, 7, 8, 1, 0, 4),
            (),
            None,
            tuple(accept_prefix_rewrite),
        )

    monkeypatch.setattr(cli_module, "ingest_project", fake_ingest)

    assert (
        timeline_main(
            [
                "ingest-project",
                "--config",
                str(config_path),
                "--team",
                "orc-team",
                "--accept-orc-prefix-rewrite",
                "orc-team:orc-root",
            ]
        )
        == 0
    )

    captured = capsys.readouterr()
    assert "accepted append-prefix rewrite" in captured.err
    assert "append-prefix-rewritten-operator-accepted-rows-preserved" in captured.err
    assert "messages row 878292: message_json .token_count" in captured.err
    # The per-team project path prints the whole report, retention line included, so an operator
    # running one project ingest across many teams still learns where each team's pre-rewrite
    # snapshot went.
    assert (
        f"pre-rewrite snapshot retained for comparison: .objects/cc/{'c' * 64}.db"
        in captured.err
    )
    assert "accepted append-prefix rewrite" not in captured.out


def test_ingest_project_projects_around_a_new_team_whose_first_ingest_failed(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Route (a) end to end: register a new team with authorship rules, break its first ingest.

    Until that team ingests once, the archive has no ``teams/<slug>/raw/team.json`` for it, so it
    is not in the extraction's selection at all -- and its configured rules point at a slug the
    exporter has never heard of. That used to raise ``prompt authorship rule ... selects unknown
    team``, and every healthy team's new prompts went unprojected on every run until a human fixed
    the newcomer. Nothing here is faked below ``timeline_main``: the real importers run, the real
    exporter runs, and the assertion that matters is that ``team-one``'s *second* prompt reaches
    ``prompts.jsonl`` on the run where the newcomer is broken.
    """

    source_root = tmp_path / "sources" / "one"
    shutil.copytree(CLAUDE_FIXTURE_ROOT, source_root)
    session = source_root / f"{CLAUDE_SESSION_ID}.jsonl"
    config_path = _write_manifest(
        tmp_path,
        {
            "schema_version": 1,
            "output": "../archive",
            "timezone": "UTC",
            "teams": [
                _claude_team("team-one", session),
                {
                    "slug": "newcomer",
                    "provider": "orc",
                    "prompt_authorship_rules": [
                        {
                            "id": "newcomer-owner",
                            "ingress_kind": "submitted_web",
                            "author_kind": "owner_human",
                            "reason": "Audited: the new team's web ingress is the owner.",
                        }
                    ],
                    "source": {
                        "source_root": str(tmp_path / "sources" / "never-existed"),
                        "root_session": "22222222-2222-4222-8222-222222222222",
                    },
                },
            ],
        },
    )
    archive = tmp_path / "archive"
    prompts_path = archive / "extracted" / "transcripts" / "prompts.jsonl"

    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 2
    captured = capsys.readouterr()
    assert "teams: 1 succeeded, 1 failed" in captured.out
    assert "team newcomer (orc):" in captured.err
    # The projection ran, over the one team that has data, and said what it left out.
    assert "across 1 normalized archive teams" in captured.out
    assert "transcripts: not extracted" not in captured.out
    assert (
        "prompt authorship rule newcomer-owner (team newcomer has no normalized data)"
        in captured.err
    )
    first_prompts = prompts_path.read_text(encoding="utf-8")
    assert first_prompts

    # A new prompt arrives for the healthy team while the newcomer is still broken.
    with session.open("a", encoding="utf-8") as handle:
        handle.write(
            json.dumps(
                {
                    "type": "user",
                    "uuid": "99999999-9999-4999-8999-999999999999",
                    "parentUuid": None,
                    "sessionId": CLAUDE_SESSION_ID,
                    "timestamp": "2026-08-12T12:00:00.000Z",
                    "cwd": "/repo",
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "A brand new question."}
                        ],
                    },
                }
            )
            + "\n"
        )

    assert timeline_main(["ingest-project", "--config", str(config_path)]) == 2
    capsys.readouterr()
    second_prompts = prompts_path.read_text(encoding="utf-8")
    assert "A brand new question." in second_prompts
    assert len(second_prompts.splitlines()) == len(first_prompts.splitlines()) + 1

    run = json.loads(
        sorted((archive / "runs").glob("*.json"))[-1].read_text(encoding="utf-8")
    )
    project = run["mechanical"]["project_ingest"]
    extraction = project["transcript_extraction"]
    assert extraction is not None
    assert extraction["teams"] == 1
    assert extraction["skipped_teams"] == []
    assert extraction["dropped_prompt_authorship_rules"] == [
        {"rule_id": "newcomer-owner", "team_slug": "newcomer"}
    ]
