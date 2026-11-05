"""Zero-model transcript extraction and prompt-range query contracts."""

from __future__ import annotations

import json
import stat
import subprocess
from dataclasses import replace
from pathlib import Path

import pytest

from wrkviz.build_store import shared_build_root
from wrkviz.archive import JsonValue, as_object, narrow_json
from wrkviz.cli import main as timeline_main
from wrkviz.model import Agent, Event, TeamData
from wrkviz.transcript_export import (
    PromptAuthorshipRule,
    TranscriptTeamSkip,
    export_transcripts,
)


def _event(
    event_id: str,
    turn_id: str,
    timestamp_ms: int,
    kind: str,
    text: str,
    source_line: int,
) -> Event:
    prompt = kind == "user_prompt"
    system = kind == "system_input"
    return Event(
        event_id=event_id,
        thread_id="root",
        turn_id=turn_id,
        timestamp_ms=timestamp_ms,
        kind=kind,
        role="user" if prompt else "system" if system else "assistant",
        phase=None,
        text=text,
        content_availability="plain",
        encrypted_content=None,
        author="user" if prompt else "system" if system else "root",
        recipient="root" if prompt or system else None,
        source_line=source_line,
        ingress_kind="typed" if prompt else "scheduled" if system else "agent",
        author_kind="owner_human" if prompt else "system" if system else "agent",
        source_native_id=event_id,
        classification_version="authorship-v1",
    )


def _team(events: tuple[Event, ...]) -> TeamData:
    return TeamData(
        team_slug="alpha",
        provider="codex",
        root_thread_id="root",
        display_timezone="America/New_York",
        sources=(),
        agents=(
            Agent(
                thread_id="root",
                parent_thread_id=None,
                agent_path="/root",
                nickname=None,
                role="coordinator",
                depth=0,
                started_at_ms=100,
                ended_at_ms=1_000,
                status="completed",
                source_path="root.jsonl",
            ),
        ),
        turns=(),
        events=events,
        tool_calls=(),
        edges=(),
    )


def _jsonl(path: Path) -> list[dict[str, JsonValue]]:
    result: list[dict[str, JsonValue]] = []
    for index, line in enumerate(path.read_text(encoding="utf-8").splitlines()):
        raw: object = json.loads(line)
        result.append(as_object(narrow_json(raw), f"record {index}"))
    return result


def test_export_is_idempotent_and_monotonic_across_missing_source_records(
    tmp_path: Path,
) -> None:
    first = _team(
        (
            _event("prompt-one", "turn-one", 200, "user_prompt", "One", 1),
            _event("response-one", "turn-one", 250, "assistant_message", "Done", 2),
            _event("scheduled", "turn-system", 300, "system_input", "Tick", 3),
            _event("prompt-two", "turn-two", 400, "user_prompt", "Two", 4),
        )
    )

    report = export_transcripts(tmp_path, (first,))
    assert report.prompts == 2
    assert report.responses == 1
    assert report.system_inputs == 1
    assert report.files_changed == 8
    assert export_transcripts(tmp_path, (first,)).files_changed == 0
    assert (tmp_path / "timeline").is_file()
    assert not (tmp_path / "query.py").exists()

    # Simulate a provider rewrite that drops an old prompt/response while exposing
    # newly discovered earlier history. The durable occurrence set only grows.
    second = _team(
        (
            _event("prompt-zero", "turn-zero", 100, "user_prompt", "Zero", 1),
            _event("prompt-two", "turn-two", 400, "user_prompt", "Two", 4),
        )
    )
    updated = export_transcripts(tmp_path, (second,))
    assert updated.prompts == 3
    assert updated.responses == 1
    assert updated.system_inputs == 1
    assert updated.carried_forward == 3

    root = tmp_path / "extracted" / "transcripts"
    prompts = _jsonl(root / "prompts.jsonl")
    assert [record["text"] for record in prompts] == ["Zero", "One", "Two"]
    assert [record["ordinal"] for record in prompts] == [1, 2, 3]
    messages = _jsonl(root / "messages.jsonl")
    response = next(record for record in messages if record["record_type"] == "response")
    prompt_one = next(record for record in prompts if record["text"] == "One")
    assert response["in_reply_to_prompt_id"] == prompt_one["record_id"]


def test_current_snapshot_excludes_events_outside_team_window(
    tmp_path: Path,
) -> None:
    events = (
        _event("prompt-before", "turn-before", 199, "user_prompt", "Before", 1),
        _event("prompt-start", "turn-start", 200, "user_prompt", "Start", 2),
        _event(
            "response-inside",
            "turn-start",
            250,
            "assistant_message",
            "Inside response",
            3,
        ),
        _event(
            "system-inside",
            "turn-system",
            300,
            "system_input",
            "Inside system input",
            4,
        ),
        _event(
            "response-end",
            "turn-end",
            500,
            "assistant_message",
            "At end",
            5,
        ),
        _event("system-after", "turn-after", 501, "system_input", "After", 6),
    )
    team = replace(_team(events), window_start_ms=200, window_end_ms=500)

    report = export_transcripts(tmp_path, (team,))

    assert report.prompts == 1
    assert report.responses == 1
    assert report.system_inputs == 1
    root = tmp_path / "extracted" / "transcripts"
    assert [record["text"] for record in _jsonl(root / "prompts.jsonl")] == [
        "Start"
    ]
    assert [record["text"] for record in _jsonl(root / "messages.jsonl")] == [
        "Start",
        "Inside response",
    ]
    assert [
        record["text"] for record in _jsonl(root / "system-inputs.jsonl")
    ] == ["Inside system input"]


def test_current_message_class_supersedes_stale_occurrence_projection(
    tmp_path: Path,
) -> None:
    legacy_prompt = _event(
        "scheduled", "turn-system", 300, "user_prompt", "Tick", 3
    )
    first = export_transcripts(tmp_path, (_team((legacy_prompt,)),))
    assert first.prompts == 1
    assert first.system_inputs == 0
    assert first.reclassified == 0

    corrected_system = replace(
        legacy_prompt,
        kind="system_input",
        role="system",
        author="system",
        ingress_kind="scheduled",
        author_kind="system",
        classification_version="authorship-v2",
    )
    corrected = export_transcripts(tmp_path, (_team((corrected_system,)),))
    assert corrected.prompts == 0
    assert corrected.system_inputs == 1
    assert corrected.carried_forward == 0
    assert corrected.reclassified == 1

    root = tmp_path / "extracted" / "transcripts"
    occurrences = _jsonl(shared_build_root(tmp_path) / "occurrences.jsonl")
    assert len(occurrences) == 1
    assert occurrences[0]["record_type"] == "system_input"
    assert _jsonl(root / "prompts.jsonl") == []
    assert len(_jsonl(root / "system-inputs.jsonl")) == 1


def test_missing_source_occurrence_still_retains_its_last_message_class(
    tmp_path: Path,
) -> None:
    legacy_prompt = _event(
        "scheduled", "turn-system", 300, "user_prompt", "Tick", 3
    )
    export_transcripts(tmp_path, (_team((legacy_prompt,)),))
    retained = export_transcripts(tmp_path, (_team(()),))
    assert retained.prompts == 1
    assert retained.system_inputs == 0
    assert retained.carried_forward == 1
    assert retained.reclassified == 0


def test_provenance_overlay_migrates_occurrence_without_duplicates_or_id_churn(
    tmp_path: Path,
) -> None:
    legacy_prompt = replace(
        _event("stable-prompt", "turn", 200, "user_prompt", "Resume Widget", 10),
        source_native_id="legacy-message",
        ingress_kind="submitted_web",
        author_kind="external_or_unknown",
    )
    legacy_response = replace(
        _event("stable-response", "turn", 250, "assistant_message", "Resumed", 11),
        source_native_id="legacy-response",
    )
    legacy_team = replace(
        _team((legacy_prompt, legacy_response)), provider="orc"
    )
    rule = PromptAuthorshipRule(
        "legacy-owner",
        "alpha",
        "submitted_web",
        "owner_human",
        "The audited legacy message was submitted by the owner.",
        source_native_ids=("legacy-message",),
    )

    export_transcripts(tmp_path, (legacy_team,), (rule,))
    root = tmp_path / "extracted" / "transcripts"
    before_occurrences = _jsonl(shared_build_root(tmp_path) / "occurrences.jsonl")
    before_by_type = {
        str(record["record_type"]): record for record in before_occurrences
    }
    before_prompt = _jsonl(root / "prompts.jsonl")[0]

    modern_prompt = replace(
        legacy_prompt,
        source_native_id="4b37d53f-modern-prompt",
        source_line=101,
    )
    modern_response = replace(
        legacy_response,
        source_native_id="3cbe2d0f-modern-response",
        source_line=102,
    )
    modern_team = replace(
        legacy_team, events=(modern_prompt, modern_response)
    )
    migrated = export_transcripts(tmp_path, (modern_team,))

    assert migrated.prompts == 1
    assert migrated.responses == 1
    assert migrated.carried_forward == 0
    assert migrated.reclassified == 0
    occurrences = _jsonl(shared_build_root(tmp_path) / "occurrences.jsonl")
    assert len(occurrences) == 2
    by_type = {str(record["record_type"]): record for record in occurrences}
    assert by_type["prompt"]["record_id"] == before_by_type["prompt"]["record_id"]
    assert by_type["prompt"]["logical_record_id"] == before_by_type["prompt"][
        "logical_record_id"
    ]
    assert by_type["response"]["record_id"] == before_by_type["response"][
        "record_id"
    ]
    assert by_type["prompt"]["source_native_id"] == "4b37d53f-modern-prompt"
    assert by_type["prompt"]["source_line"] == 101
    assert by_type["response"]["source_native_id"] == (
        "3cbe2d0f-modern-response"
    )
    assert by_type["response"]["source_line"] == 102
    assert by_type["response"]["in_reply_to_prompt_id"] == by_type["prompt"][
        "record_id"
    ]
    prompt = _jsonl(root / "prompts.jsonl")[0]
    assert prompt["record_id"] == before_prompt["record_id"]
    assert prompt["authorship_rule_id"] == "legacy-owner"
    assert prompt["author_kind"] == "owner_human"
    assert prompt["source_native_id_history"] == ["legacy-message"]
    response = next(
        record
        for record in _jsonl(root / "messages.jsonl")
        if record["record_type"] == "response"
    )
    assert response["in_reply_to_prompt_id"] == prompt["record_id"]
    assert export_transcripts(tmp_path, (modern_team,)).files_changed == 0


def test_provenance_overlay_rejects_other_immutable_changes(
    tmp_path: Path,
) -> None:
    legacy = replace(
        _event("stable-prompt", "turn-old", 200, "user_prompt", "Resume", 10),
        source_native_id="legacy-message",
    )
    team = replace(_team((legacy,)), provider="orc")
    export_transcripts(tmp_path, (team,))
    occurrences = (
        shared_build_root(tmp_path) / "occurrences.jsonl"
    )
    before = occurrences.read_bytes()

    adversarial = replace(
        legacy,
        turn_id="turn-changed",
        source_native_id="modern-message",
        source_line=101,
    )
    with pytest.raises(
        ValueError, match="immutable transcript occurrence changed during provenance"
    ):
        export_transcripts(
            tmp_path, (replace(team, events=(adversarial,)),)
        )
    assert occurrences.read_bytes() == before


def test_provenance_overlay_stops_rule_when_native_selector_changes(
    tmp_path: Path,
) -> None:
    legacy = replace(
        _event("stable-prompt", "turn", 200, "user_prompt", "Resume", 10),
        source_native_id="legacy-message",
        ingress_kind="submitted_web",
        author_kind="external_or_unknown",
    )
    modern = replace(
        legacy, source_native_id="modern-message", source_line=101
    )
    team = replace(_team((legacy,)), provider="orc")
    rule = PromptAuthorshipRule(
        "legacy-owner",
        "alpha",
        "submitted_web",
        "owner_human",
        "The audited legacy message was submitted by the owner.",
        source_native_ids=("legacy-message",),
    )
    export_transcripts(tmp_path, (team,), (rule,))
    export_transcripts(
        tmp_path, (replace(team, events=(modern,)),), (rule,)
    )

    changed_rule = replace(rule, source_native_ids=("different-message",))
    export_transcripts(
        tmp_path, (replace(team, events=(modern,)),), (changed_rule,)
    )

    prompt = _jsonl(
        tmp_path / "extracted" / "transcripts" / "prompts.jsonl"
    )[0]
    assert prompt["source_native_id"] == "modern-message"
    assert prompt["source_native_id_history"] == ["legacy-message"]
    assert prompt["author_kind"] == "external_or_unknown"
    assert "authorship_rule_id" not in prompt


def test_provenance_overlay_stops_rule_when_rule_is_removed(
    tmp_path: Path,
) -> None:
    legacy = replace(
        _event("stable-prompt", "turn", 200, "user_prompt", "Resume", 10),
        source_native_id="legacy-message",
        ingress_kind="submitted_web",
        author_kind="external_or_unknown",
    )
    modern = replace(
        legacy, source_native_id="modern-message", source_line=101
    )
    team = replace(_team((legacy,)), provider="orc")
    rule = PromptAuthorshipRule(
        "legacy-owner",
        "alpha",
        "submitted_web",
        "owner_human",
        "The audited legacy message was submitted by the owner.",
        source_native_ids=("legacy-message",),
    )
    export_transcripts(tmp_path, (team,), (rule,))
    modern_team = replace(team, events=(modern,))
    export_transcripts(tmp_path, (modern_team,), (rule,))

    export_transcripts(tmp_path, (modern_team,), ())

    prompt = _jsonl(
        tmp_path / "extracted" / "transcripts" / "prompts.jsonl"
    )[0]
    assert prompt["author_kind"] == "external_or_unknown"
    assert "authorship_rule_id" not in prompt


def test_authorship_rules_are_auditable_persisted_and_reclassifiable(
    tmp_path: Path,
) -> None:
    unknown = replace(
        _event("legacy-web", "turn-web", 200, "user_prompt", "Relay", 1),
        ingress_kind="submitted_web",
        author_kind="external_or_unknown",
    )
    team = _team((unknown,))
    bot_rule = PromptAuthorshipRule(
        "legacy-web-bot",
        "alpha",
        "submitted_web",
        "agent",
        "This legacy Web endpoint was reserved for agent relays in this interval.",
        100,
        300,
    )

    export_transcripts(tmp_path, (team,), (bot_rule,))
    root = tmp_path / "extracted" / "transcripts"
    prompt = _jsonl(root / "prompts.jsonl")[0]
    assert prompt["source_author_kind"] == "external_or_unknown"
    assert prompt["author_kind"] == "agent"
    assert prompt["authorship_rule_id"] == "legacy-web-bot"
    assert prompt["authorship_rule_reason"] == bot_rule.reason
    assert prompt["classification_version"] == (
        "authorship-v1+rule:legacy-web-bot"
    )
    persisted_rules = (root / "authorship-rules.json").read_text(encoding="utf-8")
    assert export_transcripts(tmp_path, (team,)).files_changed == 0
    assert (root / "authorship-rules.json").read_text(encoding="utf-8") == persisted_rules

    owner_rule = replace(
        bot_rule,
        rule_id="legacy-web-owner",
        author_kind="owner_human",
        reason="The audited endpoint interval was owner-only.",
    )
    export_transcripts(tmp_path, (team,), (owner_rule,))
    reclassified = _jsonl(root / "prompts.jsonl")[0]
    assert reclassified["source_author_kind"] == "external_or_unknown"
    assert reclassified["author_kind"] == "owner_human"
    assert reclassified["authorship_rule_id"] == "legacy-web-owner"


def test_authorship_rules_reject_overlap_even_before_a_record_matches(
    tmp_path: Path,
) -> None:
    team = _team(
        (
            replace(
                _event("legacy-web", "turn-web", 200, "user_prompt", "Relay", 1),
                ingress_kind="submitted_web",
                author_kind="external_or_unknown",
            ),
        )
    )
    first = PromptAuthorshipRule(
        "first", "alpha", "submitted_web", "agent", "First audited interval.", 100, 300
    )
    second = PromptAuthorshipRule(
        "second",
        "alpha",
        "submitted_web",
        "owner_human",
        "Conflicting audited interval.",
        200,
        400,
    )

    with pytest.raises(ValueError, match="overlapping prompt authorship rules"):
        export_transcripts(tmp_path, (team,), (first, second))


def test_prompt_query_range_works_without_a_built_timeline(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    team = _team(
        tuple(
            _event(f"prompt-{index}", f"turn-{index}", index * 100, "user_prompt", text, index)
            for index, text in enumerate(("First", "Second", "Third", "Fourth"), 1)
        )
    )
    export_transcripts(tmp_path, (team,))
    assert not (tmp_path / "data" / "timeline.json").exists()

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(tmp_path),
                "--format",
                "text",
                "prompts",
                "--range",
                "2-3",
            )
        )
        == 0
    )
    output = capsys.readouterr().out
    assert "prompt #2" in output
    assert "Second" in output
    assert "Third" in output
    assert "First" not in output
    assert "Fourth" not in output

    assert (
        timeline_main(
            (
                "query",
                "--output",
                str(tmp_path),
                "--format",
                "jsonl",
                "prompts",
                "--range",
                "3",
            )
        )
        == 0
    )
    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 1
    record = as_object(narrow_json(json.loads(lines[0])), "query record")
    assert record["ordinal"] == 3
    assert record["text"] == "Third"


def test_archive_timeline_cli_is_discoverable_and_cwd_independent(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "archive with spaces"
    archive.mkdir()
    team = _team(
        tuple(
            _event(f"prompt-{index}", f"turn-{index}", index * 100, "user_prompt", text, index)
            for index, text in enumerate(("First", "Second", "Third", "Fourth"), 1)
        )
    )
    export_transcripts(archive, (team,))
    launcher = archive / "timeline"
    assert launcher.is_file()
    assert launcher.stat().st_mode & stat.S_IXUSR

    unrelated = tmp_path / "unrelated working directory"
    unrelated.mkdir()
    top_help = subprocess.run(
        (str(launcher), "--help"),
        cwd=unrelated,
        check=False,
        capture_output=True,
        text=True,
    )
    assert top_help.returncode == 0, top_help.stderr
    assert "./timeline prompts --range 200-300" in top_help.stdout
    assert "directory containing this executable" in top_help.stdout
    for action in (
        "prompts",
        "messages",
        "stats",
        "teams",
        "agents",
        "phases",
        "rollups",
        "show",
        "search",
        "list",
    ):
        action_help = subprocess.run(
            (str(launcher), action, "--help"),
            cwd=unrelated,
            check=False,
            capture_output=True,
            text=True,
        )
        assert action_help.returncode == 0, action_help.stderr
        assert "options:" in action_help.stdout

    readable = subprocess.run(
        (str(launcher), "prompts", "--range", "2-3"),
        cwd=unrelated,
        check=False,
        capture_output=True,
        text=True,
    )
    assert readable.returncode == 0, readable.stderr
    assert readable.stderr == ""
    assert "prompt #2" in readable.stdout
    assert "Second" in readable.stdout
    assert "Third" in readable.stdout
    assert "First" not in readable.stdout
    assert "Fourth" not in readable.stdout

    machine = subprocess.run(
        (str(launcher), "prompts", "--range", "3", "--format", "jsonl"),
        cwd=unrelated,
        check=False,
        capture_output=True,
        text=True,
    )
    assert machine.returncode == 0, machine.stderr
    assert machine.stderr == ""
    lines = machine.stdout.splitlines()
    assert len(lines) == 1
    record = as_object(narrow_json(json.loads(lines[0])), "timeline JSONL record")
    assert record["ordinal"] == 3
    assert record["text"] == "Third"


def test_logical_prompt_report_deduplicates_identical_fork_occurrences(
    tmp_path: Path,
) -> None:
    event = _event("shared-native", "turn", 100, "user_prompt", "Shared", 1)
    first = _team((event,))
    second = replace(first, team_slug="beta")

    report = export_transcripts(tmp_path, (first, second))

    assert report.prompts == 1
    occurrences = _jsonl(
        shared_build_root(tmp_path) / "occurrences.jsonl"
    )
    assert len(occurrences) == 2
    prompts = _jsonl(tmp_path / "extracted" / "transcripts" / "prompts.jsonl")
    assert prompts[0]["occurrence_count"] == 2
    assert prompts[0]["occurrence_teams"] == ["alpha", "beta"]


def test_prompt_query_fails_closed_on_digest_mismatch(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    export_transcripts(
        tmp_path,
        (_team((_event("prompt", "turn", 100, "user_prompt", "Original", 1),)),),
    )
    prompts = tmp_path / "extracted" / "transcripts" / "prompts.jsonl"
    prompts.write_text(prompts.read_text(encoding="utf-8") + "{}\n", encoding="utf-8")

    assert (
        timeline_main(
            ("query", "--output", str(tmp_path), "prompts")
        )
        == 2
    )
    assert "generation is incomplete" in capsys.readouterr().err


def test_prompt_query_selects_durable_authorship_without_prose_guessing(
    tmp_path: Path,
) -> None:
    authored = (
        ("Owner", "owner_human"),
        ("Other human", "other_human"),
        ("Agent generated", "agent"),
        ("System generated", "system"),
        ("Ambiguous [impl agent] prose is not authority", "unknown"),
    )
    events = tuple(
        replace(
            _event(
                f"prompt-{index}",
                f"turn-{index}",
                index * 100,
                "user_prompt",
                text,
                index,
            ),
            author_kind=author_kind,
        )
        for index, (text, author_kind) in enumerate(authored, 1)
    ) + (
        _event(
            "bot-response",
            "turn-3",
            350,
            "assistant_message",
            "Response linked to the agent prompt",
            10,
        ),
    )
    export_transcripts(tmp_path, (_team(events),))
    launcher = tmp_path / "timeline"

    def selected(*arguments: str) -> list[dict[str, JsonValue]]:
        completed = subprocess.run(
            (str(launcher), *arguments, "--format", "jsonl"),
            check=True,
            capture_output=True,
            text=True,
        )
        return [
            as_object(narrow_json(json.loads(line)), "query record")
            for line in completed.stdout.splitlines()
        ]

    assert [record["text"] for record in selected("prompts")] == [
        "Owner",
        "Other human",
    ]
    assert [record["text"] for record in selected("prompts", "--which", "bot")] == [
        "Agent generated",
        "System generated",
    ]
    assert len(selected("prompts", "--which", "all")) == 5

    bot_messages = selected("messages", "--which", "bot")
    assert [record["record_type"] for record in bot_messages] == [
        "prompt",
        "response",
        "prompt",
    ]
    assert bot_messages[1]["text"] == "Response linked to the agent prompt"


def _skip(team_slug: str, message: str = "torn normalized generation") -> TranscriptTeamSkip:
    return TranscriptTeamSkip.from_exception(team_slug, ValueError(message))


def test_an_unreadable_team_is_carried_forward_and_named_rather_than_stopping_the_rest(
    tmp_path: Path,
) -> None:
    """The reviewer's scenario: one team cannot be loaded, every other team still projects.

    The authorship rule is the part that makes this more than an exception-handling change.
    ``_apply_authorship_rules`` rebuilds attribution from the *source* label on every occurrence
    on every run, so dropping the unreadable team's rule along with the team would not merely
    fail to classify anything new -- it would silently revert an audited correction on records
    that are still in the projection. Filtering rules to the loaded teams, which is the obvious
    reading of "filter authorship rules", fails this test on the ``owner_human`` assertion.
    """

    legacy_prompt = replace(
        _event("alpha-prompt", "turn-a", 200, "user_prompt", "Legacy", 10),
        source_native_id="legacy-message",
        ingress_kind="submitted_web",
        author_kind="external_or_unknown",
    )
    alpha = replace(_team((legacy_prompt,)), provider="orc")
    beta = replace(
        _team((_event("beta-prompt", "turn-b", 300, "user_prompt", "First", 1),)),
        team_slug="beta",
    )
    rule = PromptAuthorshipRule(
        "legacy-owner",
        "alpha",
        "submitted_web",
        "owner_human",
        "The audited legacy message was submitted by the owner.",
        source_native_ids=("legacy-message",),
    )
    root = tmp_path / "extracted" / "transcripts"

    whole = export_transcripts(tmp_path, (alpha, beta), (rule,))
    assert whole.teams == 2
    assert whole.partial is False
    assert whole.partiality_summary() is None

    # Alpha is now unreadable, and beta has meanwhile received a new prompt.
    grown_beta = replace(
        beta,
        events=(
            *beta.events,
            _event("beta-prompt-2", "turn-b2", 400, "user_prompt", "Second", 2),
        ),
    )
    partial = export_transcripts(tmp_path, (grown_beta,), (rule,), (_skip("alpha"),))

    assert partial.teams == 1
    assert partial.partial is True
    assert [skip.team_slug for skip in partial.skipped_teams] == ["alpha"]
    assert partial.skipped_teams[0].error_type == "ValueError"
    assert partial.skipped_teams[0].error == "torn normalized generation"
    assert partial.skipped_teams[0].traceback is None
    assert partial.dropped_authorship_rules == ()
    summary = partial.partiality_summary()
    assert summary is not None
    assert "1 of 2 archive teams could not be read" in summary
    assert "alpha: ValueError: torn normalized generation" in summary

    prompts = {str(record["text"]): record for record in _jsonl(root / "prompts.jsonl")}
    # Beta's new prompt is projected -- the headline promise -- and alpha's is still there.
    assert set(prompts) == {"Legacy", "First", "Second"}
    assert prompts["Legacy"]["team_slug"] == "alpha"
    assert prompts["Legacy"]["author_kind"] == "owner_human"
    assert prompts["Legacy"]["authorship_rule_id"] == "legacy-owner"
    assert prompts["Second"]["team_slug"] == "beta"


def test_a_partial_projection_says_so_in_the_manifest_it_leaves_behind(
    tmp_path: Path,
) -> None:
    """Whoever reads the archive later was not watching the run that wrote it."""

    alpha = _team((_event("alpha-prompt", "turn-a", 200, "user_prompt", "One", 1),))
    beta = replace(
        _team((_event("beta-prompt", "turn-b", 300, "user_prompt", "Two", 1),)),
        team_slug="beta",
    )
    root = tmp_path / "extracted" / "transcripts"

    export_transcripts(tmp_path, (alpha, beta))
    export_transcripts(tmp_path, (beta,), None, (_skip("alpha"),))

    manifest = as_object(
        narrow_json(json.loads((root / "manifest.json").read_text(encoding="utf-8"))),
        "manifest",
    )
    # `teams` keeps every team the projection represents, so the omission guard can still see
    # alpha next run; `source_generations` holds only the team whose bytes were actually read.
    assert manifest["teams"] == ["alpha", "beta"]
    generations = as_object(
        narrow_json(json.loads(json.dumps(manifest["source_generations"]))[0]),
        "generation",
    )
    assert generations["team_slug"] == "beta"
    assert manifest["skipped_teams"] == [
        {
            "team_slug": "alpha",
            "error_type": "ValueError",
            "error": "torn normalized generation",
        }
    ]
    counts = as_object(manifest["counts"], "counts")
    assert counts["teams_read"] == 1
    assert counts["teams_skipped"] == 1


def test_a_skipped_team_does_not_open_the_door_to_deleting_a_team_from_the_projection(
    tmp_path: Path,
) -> None:
    """The omission guard is relaxed for skips and for nothing else."""

    alpha = _team((_event("alpha-prompt", "turn-a", 200, "user_prompt", "One", 1),))
    beta = replace(
        _team((_event("beta-prompt", "turn-b", 300, "user_prompt", "Two", 1),)),
        team_slug="beta",
    )
    export_transcripts(tmp_path, (alpha, beta))

    with pytest.raises(ValueError, match="cannot omit previously extracted teams: alpha"):
        export_transcripts(tmp_path, (beta,))

    # Naming it as a skip is what makes it legal, and it stays legal on the run after that.
    export_transcripts(tmp_path, (beta,), None, (_skip("alpha"),))
    export_transcripts(tmp_path, (beta,), None, (_skip("alpha"),))
    with pytest.raises(ValueError, match="cannot omit previously extracted teams: alpha"):
        export_transcripts(tmp_path, (beta,))


def test_a_rule_for_a_team_the_archive_has_no_data_for_is_set_aside_not_fatal(
    tmp_path: Path,
) -> None:
    """Route (a): a newly registered team whose very first ingest failed.

    Its configured rules point at a slug the archive has never heard of. That used to raise
    ``prompt authorship rule ... selects unknown team`` and take the whole projection down with
    it, every run, until a human fixed the new team.
    """

    alpha = _team((_event("alpha-prompt", "turn-a", 200, "user_prompt", "One", 1),))
    reachable = PromptAuthorshipRule(
        "alpha-owner",
        "alpha",
        "typed",
        "owner_human",
        "Audited: alpha's typed ingress is the owner.",
    )
    orphan = PromptAuthorshipRule(
        "newcomer-owner",
        "newcomer",
        "submitted_web",
        "owner_human",
        "Audited: the new team's web ingress is the owner.",
    )

    report = export_transcripts(tmp_path, (alpha,), (reachable, orphan))

    assert report.prompts == 1
    assert report.partial is True
    assert report.skipped_teams == ()
    assert [rule.rule_id for rule in report.dropped_authorship_rules] == [
        "newcomer-owner"
    ]
    assert report.dropped_authorship_rules[0].team_slug == "newcomer"
    summary = report.partiality_summary()
    assert summary is not None
    assert "1 prompt authorship rule(s) were not applied" in summary
    assert "newcomer-owner (team newcomer has no normalized data)" in summary

    root = tmp_path / "extracted" / "transcripts"
    persisted = as_object(
        narrow_json(
            json.loads((root / "authorship-rules.json").read_text(encoding="utf-8"))
        ),
        "rules",
    )
    rules_json = json.loads(json.dumps(persisted["rules"]))
    assert [rule["rule_id"] for rule in rules_json] == ["alpha-owner"]

    # Once the newcomer finally ingests, its rule is live again with no operator action.
    newcomer_prompt = replace(
        _event("newcomer-prompt", "turn-n", 400, "user_prompt", "Hello", 1),
        ingress_kind="submitted_web",
        author_kind="external_or_unknown",
    )
    healed = export_transcripts(
        tmp_path,
        (alpha, replace(_team((newcomer_prompt,)), team_slug="newcomer")),
        (reachable, orphan),
    )
    assert healed.partial is False
    prompts = {str(record["text"]): record for record in _jsonl(root / "prompts.jsonl")}
    assert prompts["Hello"]["author_kind"] == "owner_human"
    assert prompts["Hello"]["authorship_rule_id"] == "newcomer-owner"


def test_an_unclassified_load_failure_keeps_its_traceback_off_the_durable_manifest(
    tmp_path: Path,
) -> None:
    """The evidence goes to the receipt; the manifest keeps bytes that do not churn."""

    skip = TranscriptTeamSkip.from_exception("alpha", KeyError("messages"))
    assert skip.error_type == "KeyError"
    assert skip.traceback is not None
    assert "KeyError" in skip.traceback
    assert skip.to_json_obj()["traceback"] == skip.traceback
    assert "traceback" not in skip.to_manifest_obj()

    beta = replace(
        _team((_event("beta-prompt", "turn-b", 300, "user_prompt", "Two", 1),)),
        team_slug="beta",
    )
    export_transcripts(tmp_path, (beta,), None, (skip,))
    manifest = as_object(
        narrow_json(
            json.loads(
                (tmp_path / "extracted" / "transcripts" / "manifest.json").read_text(
                    encoding="utf-8"
                )
            )
        ),
        "manifest",
    )
    assert manifest["skipped_teams"] == [
        {"team_slug": "alpha", "error_type": "KeyError", "error": "'messages'"}
    ]


def test_export_still_refuses_a_team_offered_as_both_loaded_and_skipped(
    tmp_path: Path,
) -> None:
    alpha = _team((_event("alpha-prompt", "turn-a", 200, "user_prompt", "One", 1),))
    with pytest.raises(ValueError, match="both loaded and skipped: alpha"):
        export_transcripts(tmp_path, (alpha,), None, (_skip("alpha"),))
