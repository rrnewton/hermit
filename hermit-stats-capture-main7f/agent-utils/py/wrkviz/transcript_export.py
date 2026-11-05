"""Deterministic, zero-model exports of coordinator prompts and responses."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from traceback import format_exception
from typing import Iterable, Mapping, Sequence
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    canonical_jsonl,
    read_json,
    read_jsonl,
    write_text_if_changed,
)
from wrkviz.model import Event, TeamData, source_digest
from wrkviz.build_store import shared_build_file, shared_build_root
from wrkviz.render import (
    archive_makefile,
    prune_retired_query_artifacts,
    standalone_query_source,
)


TRANSCRIPT_EXPORT_SCHEMA_VERSION = 1
#: The projections the archive publishes: what a reader of a shipped archive opens.
_MANAGED_FILES = (
    "prompts.jsonl",
    "messages.jsonl",
    "system-inputs.jsonl",
)

#: The monotonic union, which is *input to the next run* rather than output of this one.
#:
#: It is the largest thing this module writes -- 106.3 MiB against 110.2 MiB for all three
#: published projections combined -- and nothing that consumes an archive opens it: not the
#: browser, not any subcommand of the shipped CLI. Its one reader is `export_transcripts`
#: itself, which reads the previous generation to carry forward the records of teams that could
#: not be loaded this run. That is the definition of rerun state, so it lives in the build store
#: beside the other rerun state rather than in the directory an operator ships.
#:
#: Its digest stays in the manifest. Where the bytes live is a packaging decision; whether the
#: generation can be checked against what produced it is not, and dropping the record because
#: the file moved would trade a real guarantee for a directory listing.
_BASELINE_FILE = "occurrences.jsonl"
_AUTHORSHIP_RULES_FILE = "authorship-rules.json"
_UNCLASSIFIED_AUTHOR_KINDS = frozenset({"external_or_unknown", "unknown"})
_RULE_AUTHOR_KINDS = frozenset(
    {"owner_human", "other_human", "agent", "system"}
)


@dataclass(frozen=True)
class PromptAuthorshipRule:
    """Auditable correction for an ingress interval without sender identity."""

    rule_id: str
    team_slug: str
    ingress_kind: str
    author_kind: str
    reason: str
    start_ms: int | None = None
    end_ms: int | None = None
    source_native_ids: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        for label, value in (
            ("rule_id", self.rule_id),
            ("team_slug", self.team_slug),
            ("ingress_kind", self.ingress_kind),
            ("reason", self.reason),
        ):
            if not value.strip() or "\0" in value:
                raise ValueError(f"prompt authorship {label} must be non-empty")
        if self.author_kind not in _RULE_AUTHOR_KINDS:
            raise ValueError(
                "prompt authorship author_kind must be owner_human, other_human, "
                "agent, or system"
            )
        if self.start_ms is not None and self.start_ms < 0:
            raise ValueError("prompt authorship start_ms must be non-negative")
        if self.end_ms is not None and self.end_ms < 0:
            raise ValueError("prompt authorship end_ms must be non-negative")
        if (
            self.start_ms is not None
            and self.end_ms is not None
            and self.start_ms >= self.end_ms
        ):
            raise ValueError("prompt authorship interval must be non-empty")
        if len(set(self.source_native_ids)) != len(self.source_native_ids):
            raise ValueError("prompt authorship source_native_ids contain duplicates")
        if any(not value or "\0" in value for value in self.source_native_ids):
            raise ValueError(
                "prompt authorship source_native_ids must contain non-empty strings"
            )

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the complete, deterministic rule representation."""

        native_ids: list[JsonValue] = list(self.source_native_ids)
        return {
            "rule_id": self.rule_id,
            "team_slug": self.team_slug,
            "ingress_kind": self.ingress_kind,
            "author_kind": self.author_kind,
            "reason": self.reason,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "source_native_ids": native_ids,
        }

    def matches(self, record: dict[str, JsonValue]) -> bool:
        """Return whether this rule selects one prompt occurrence."""

        if not self.matches_scope(record):
            return False
        if not self.source_native_ids:
            return True
        native_id = record.get("source_native_id")
        return isinstance(native_id, str) and native_id in self.source_native_ids

    def matches_scope(self, record: dict[str, JsonValue]) -> bool:
        """Return whether every selector except source-native provenance matches."""

        if record.get("team_slug") != self.team_slug:
            return False
        if record.get("ingress_kind") != self.ingress_kind:
            return False
        timestamp_ms = as_int(record.get("timestamp_ms"), "prompt.timestamp_ms")
        if self.start_ms is not None and timestamp_ms < self.start_ms:
            return False
        if self.end_ms is not None and timestamp_ms >= self.end_ms:
            return False
        return True


@dataclass(frozen=True)
class TranscriptTeamSkip:
    """One archive team the projection carried forward because it could not be re-read.

    A skip is *not* the same thing as a team with nothing new to contribute. The occurrences that
    team contributed on its last good run are still projected -- ``_monotonic_union`` seeds itself
    with every record already in ``occurrences.jsonl`` and only replaces the ones the current run
    re-derives -- but nothing that has happened to that team since is, and the manifest's
    ``source_generations`` has no entry for it because no current generation was read. That is a
    materially different artifact from a complete projection, and the difference is invisible in
    the record counts, so it is named here rather than inferred.

    The fields mirror :class:`~wrkviz.project_config.ProjectTeamIngestFailure` on
    purpose, and for the same reasons: ``error_type`` is the exception class name because that is
    what distinguishes one archive's torn bytes from a missing mount, and ``traceback`` is kept
    only for exception types this package does not classify as data/IO failure, since for those a
    one-line message is a defect report with the evidence deleted.
    """

    team_slug: str
    error_type: str
    error: str
    traceback: str | None = None

    @classmethod
    def from_exception(cls, team_slug: str, error: BaseException) -> TranscriptTeamSkip:
        """Classify one team's load failure without deciding what the run should do about it."""

        # Formatted from the exception object rather than from format_exc(), so this classifier
        # does not silently produce "NoneType: None" if it is ever called outside an except block.
        expected = isinstance(error, (OSError, ValueError))
        return cls(
            team_slug,
            type(error).__name__,
            str(error),
            None if expected else "".join(format_exception(error)),
        )

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the complete skip, traceback included, for a run receipt."""

        value: dict[str, JsonValue] = {
            "team_slug": self.team_slug,
            "error_type": self.error_type,
            "error": self.error,
        }
        if self.traceback is not None:
            value["traceback"] = self.traceback
        return value

    def to_manifest_obj(self) -> dict[str, JsonValue]:
        """Return the skip as durable projection metadata, deliberately without the traceback.

        The run receipt is the place a traceback belongs: it is dated, it is one run's evidence,
        and nothing compares two of them. ``manifest.json`` is neither -- it is rewritten in place
        every run and its bytes decide ``files_changed`` -- so embedding interpreter line numbers
        in it would make an unrelated refactor of this module show up as a projection change.
        """

        return {
            "team_slug": self.team_slug,
            "error_type": self.error_type,
            "error": self.error,
        }

    @property
    def summary(self) -> str:
        """Return one operator-readable line naming the team and why it was carried."""

        return f"{self.team_slug}: {self.error_type}: {self.error}"


@dataclass(frozen=True)
class DroppedAuthorshipRule:
    """One configured prompt-authorship rule the archive has no team to apply it to.

    Dropped rather than fatal, and the distinction is narrower than it looks. A rule's
    ``team_slug`` is not typed by a human: the project config nests rules inside their team and
    :func:`wrkviz.project_config._prompt_authorship_rules` stamps the enclosing
    team's slug onto each one, so a rule naming an unknown team never means "typo". It means the
    archive holds no normalized data for a team that is genuinely registered -- overwhelmingly, a
    new team whose *first* ingest just failed. Refusing the whole extraction over that is the
    "one broken lineage withholds eleven healthy teams' prompts" failure this projection exists to
    avoid, so the rule is set aside and reported at the same volume as a skipped team.

    It is reported rather than silently ignored because a rule that appears configured and does
    nothing is the worst state for an authorship correction: the next reader sees the rule in the
    config, sees prompts still labelled ``unknown``, and has no way to tell which of the two is
    wrong.
    """

    rule_id: str
    team_slug: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the dropped rule as receipt and manifest metadata."""

        return {"rule_id": self.rule_id, "team_slug": self.team_slug}

    @property
    def summary(self) -> str:
        """Return one operator-readable line naming the rule and the team it cannot reach."""

        return f"{self.rule_id} (team {self.team_slug} has no normalized data)"


@dataclass(frozen=True)
class TranscriptExportReport:
    """Counts produced by one mechanical transcript export.

    ``teams`` counts the teams whose *current* normalized data was read. Teams carried forward
    without being re-read are in ``skipped_teams`` and are deliberately not added to that count:
    a partial projection reported as "across 12 teams" is exactly the partial-run-that-looks-
    complete this reporting exists to prevent.
    """

    teams: int
    prompts: int
    responses: int
    system_inputs: int
    carried_forward: int
    files_changed: int
    reclassified: int = 0
    skipped_teams: tuple[TranscriptTeamSkip, ...] = ()
    dropped_authorship_rules: tuple[DroppedAuthorshipRule, ...] = ()

    @property
    def partial(self) -> bool:
        """Return whether this projection is missing anything a complete one would have."""

        return bool(self.skipped_teams or self.dropped_authorship_rules)

    def partiality_summary(self) -> str | None:
        """Return one line naming everything missing, or ``None`` when the projection is whole."""

        if not self.partial:
            return None
        parts: list[str] = []
        if self.skipped_teams:
            parts.append(
                f"{len(self.skipped_teams)} of "
                f"{self.teams + len(self.skipped_teams)} archive teams could not be read "
                "and were carried forward unchanged: "
                + "; ".join(skip.summary for skip in self.skipped_teams)
            )
        if self.dropped_authorship_rules:
            parts.append(
                f"{len(self.dropped_authorship_rules)} prompt authorship rule(s) were not "
                "applied: "
                + "; ".join(
                    rule.summary for rule in self.dropped_authorship_rules
                )
            )
        return " | ".join(parts)

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return a run-receipt-compatible JSON object.

        The two partiality arrays are always written, empty on a whole projection, so a reader
        never has to distinguish an absent key from an empty one -- the same choice
        ``ProjectIngestReport.to_json_obj`` made for ``failed_teams``.
        """

        skipped_values: list[JsonValue] = [
            skip.to_json_obj() for skip in self.skipped_teams
        ]
        dropped_values: list[JsonValue] = [
            rule.to_json_obj() for rule in self.dropped_authorship_rules
        ]
        return {
            "teams": self.teams,
            "prompts": self.prompts,
            "responses": self.responses,
            "system_inputs": self.system_inputs,
            "carried_forward": self.carried_forward,
            "reclassified": self.reclassified,
            "files_changed": self.files_changed,
            "skipped_teams": skipped_values,
            "teams_skipped": len(self.skipped_teams),
            "dropped_prompt_authorship_rules": dropped_values,
            "model_calls": 0,
            "model_tokens": 0,
        }


@dataclass(frozen=True)
class _GroupedEvent:
    team: TeamData
    events: tuple[Event, ...]
    text: str

    @property
    def first(self) -> Event:
        return self.events[0]


@dataclass(frozen=True)
class _AuthorshipMigration:
    """Prior explicit rule evidence retained across a provenance-only migration."""

    rule_id: str
    source_native_ids: tuple[str, ...]


def _sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _stable_id(prefix: str, fields: Iterable[str]) -> str:
    material = "\0".join(fields)
    return f"{prefix}:{hashlib.sha256(material.encode('utf-8')).hexdigest()[:32]}"


def _instant(timestamp_ms: int, zone: timezone | ZoneInfo) -> str:
    return datetime.fromtimestamp(timestamp_ms / 1000, tz=zone).isoformat(
        timespec="milliseconds"
    )


def _timezone(team: TeamData) -> timezone | ZoneInfo:
    try:
        return ZoneInfo(team.display_timezone)
    except ZoneInfoNotFoundError as error:
        raise ValueError(
            f"team {team.team_slug!r} has unknown display timezone "
            f"{team.display_timezone!r}"
        ) from error


def _coordinator_threads(team: TeamData) -> set[str]:
    result = {team.root_thread_id}
    result.update(
        agent.thread_id for agent in team.agents if agent.role == "coordinator"
    )
    return result


def _group_key(event: Event) -> tuple[str, str, int, int, str, str]:
    native = event.source_native_id or ""
    fallback = "" if native else event.event_id.rsplit(":", 1)[0]
    return (
        event.thread_id,
        event.turn_id or "",
        event.timestamp_ms,
        event.source_line,
        event.kind,
        native or fallback,
    )


def _group_events(team: TeamData, kinds: set[str]) -> tuple[_GroupedEvent, ...]:
    coordinator_threads = _coordinator_threads(team)
    grouped: dict[tuple[str, str, int, int, str, str], list[Event]] = {}
    for event in team.events:
        if (
            event.thread_id not in coordinator_threads
            or event.kind not in kinds
            or (
                team.window_start_ms is not None
                and event.timestamp_ms < team.window_start_ms
            )
            or (
                team.window_end_ms is not None
                and event.timestamp_ms >= team.window_end_ms
            )
            or event.text is None
            or not event.text.strip()
        ):
            continue
        grouped.setdefault(_group_key(event), []).append(event)
    result: list[_GroupedEvent] = []
    for events in grouped.values():
        ordered = tuple(sorted(events, key=lambda event: event.event_id))
        text = "\n\n".join(
            event.text.strip() for event in ordered if event.text is not None
        )
        result.append(_GroupedEvent(team, ordered, text))
    return tuple(
        sorted(
            result,
            key=lambda group: (
                group.first.timestamp_ms,
                group.first.source_line,
                group.first.event_id,
            ),
        )
    )


def _agent_fields(team: TeamData, thread_id: str) -> tuple[str, str]:
    for agent in team.agents:
        if agent.thread_id == thread_id:
            return agent.agent_path, agent.source_path
    return "/root", ""


def _base_record(group: _GroupedEvent, record_type: str) -> dict[str, JsonValue]:
    event = group.first
    team = group.team
    agent_path, source_path = _agent_fields(team, event.thread_id)
    digest = _sha256_text(group.text)
    event_ids: list[JsonValue] = [item.event_id for item in group.events]
    native_id = event.source_native_id
    occurrence_fields = (
        team.team_slug,
        team.provider,
        event.thread_id,
        source_path,
        native_id or "",
        str(event.source_line),
        str(event.timestamp_ms),
        digest,
        *[item.event_id for item in group.events],
    )
    logical_fields = (
        team.provider,
        event.thread_id,
        native_id or event.event_id,
        str(event.timestamp_ms),
        digest,
    )
    zone = _timezone(team)
    return {
        "schema_version": TRANSCRIPT_EXPORT_SCHEMA_VERSION,
        "record_type": record_type,
        "record_id": _stable_id(record_type, occurrence_fields),
        "logical_record_id": _stable_id(f"logical-{record_type}", logical_fields),
        "team_slug": team.team_slug,
        "provider": team.provider,
        "thread_id": event.thread_id,
        "turn_id": event.turn_id,
        "agent_path": agent_path,
        "timestamp_ms": event.timestamp_ms,
        "timestamp_utc": _instant(event.timestamp_ms, timezone.utc).replace(
            "+00:00", "Z"
        ),
        "timestamp_local": _instant(event.timestamp_ms, zone),
        "display_timezone": team.display_timezone,
        "text": group.text,
        "content_sha256": digest,
        "source_path": source_path,
        "source_line": event.source_line,
        "source_event_ids": event_ids,
        "source_native_id": native_id,
        "ingress_kind": event.ingress_kind,
        "author_kind": event.author_kind,
        "classification_version": event.classification_version,
    }


def _response_records(
    teams: tuple[TeamData, ...],
    prompts: list[dict[str, JsonValue]],
) -> list[dict[str, JsonValue]]:
    prompt_candidates: dict[tuple[str, str, str], list[dict[str, JsonValue]]] = {}
    for prompt in prompts:
        turn_id = prompt.get("turn_id")
        if not isinstance(turn_id, str):
            continue
        key = (
            as_string(prompt.get("team_slug"), "prompt.team_slug"),
            as_string(prompt.get("thread_id"), "prompt.thread_id"),
            turn_id,
        )
        prompt_candidates.setdefault(key, []).append(prompt)
    for values in prompt_candidates.values():
        values.sort(
            key=lambda item: (
                as_int(item.get("timestamp_ms"), "prompt.timestamp_ms"),
                as_int(item.get("source_line"), "prompt.source_line"),
            )
        )

    responses: list[dict[str, JsonValue]] = []
    for team in teams:
        for group in _group_events(team, {"assistant_message"}):
            record = _base_record(group, "response")
            turn_id = group.first.turn_id
            candidates = (
                prompt_candidates.get(
                    (team.team_slug, group.first.thread_id, turn_id), ()
                )
                if turn_id is not None
                else ()
            )
            eligible = [
                prompt
                for prompt in candidates
                if as_int(prompt.get("timestamp_ms"), "prompt.timestamp_ms")
                <= group.first.timestamp_ms
            ]
            linked = eligible[-1] if eligible else None
            record["in_reply_to_prompt_id"] = (
                as_string(linked.get("record_id"), "prompt.record_id")
                if linked is not None
                else None
            )
            responses.append(record)
    return responses


def _record_sort_key(record: dict[str, JsonValue]) -> tuple[int, int, str, str]:
    record_type = as_string(record.get("record_type"), "record.record_type")
    type_rank = 0 if record_type == "prompt" else 1
    return (
        as_int(record.get("timestamp_ms"), "record.timestamp_ms"),
        type_rank,
        as_string(record.get("team_slug"), "record.team_slug"),
        as_string(record.get("record_id"), "record.record_id"),
    )


def _prompt_authorship_rule_from_json(
    value: JsonValue, where: str
) -> PromptAuthorshipRule:
    obj = as_object(value, where)
    expected = {
        "rule_id",
        "team_slug",
        "ingress_kind",
        "author_kind",
        "reason",
        "start_ms",
        "end_ms",
        "source_native_ids",
    }
    if set(obj) != expected:
        raise ValueError(
            f"{where}: invalid fields; missing={sorted(expected - set(obj))!r}, "
            f"unknown={sorted(set(obj) - expected)!r}"
        )
    start_value = obj.get("start_ms")
    end_value = obj.get("end_ms")
    start_ms = None if start_value is None else as_int(start_value, where + ".start_ms")
    end_ms = None if end_value is None else as_int(end_value, where + ".end_ms")
    native_ids = tuple(
        as_string(item, f"{where}.source_native_ids[{index}]")
        for index, item in enumerate(
            as_array(obj.get("source_native_ids"), where + ".source_native_ids")
        )
    )
    return PromptAuthorshipRule(
        as_string(obj.get("rule_id"), where + ".rule_id"),
        as_string(obj.get("team_slug"), where + ".team_slug"),
        as_string(obj.get("ingress_kind"), where + ".ingress_kind"),
        as_string(obj.get("author_kind"), where + ".author_kind"),
        as_string(obj.get("reason"), where + ".reason"),
        start_ms,
        end_ms,
        native_ids,
    )


def _rules_overlap(left: PromptAuthorshipRule, right: PromptAuthorshipRule) -> bool:
    if left.team_slug != right.team_slug or left.ingress_kind != right.ingress_kind:
        return False
    if (
        left.end_ms is not None
        and right.start_ms is not None
        and left.end_ms <= right.start_ms
    ):
        return False
    if (
        right.end_ms is not None
        and left.start_ms is not None
        and right.end_ms <= left.start_ms
    ):
        return False
    if not left.source_native_ids or not right.source_native_ids:
        return True
    return bool(set(left.source_native_ids) & set(right.source_native_ids))


def _validate_rules(
    rules: Sequence[PromptAuthorshipRule], team_slugs: set[str]
) -> tuple[PromptAuthorshipRule, ...]:
    ordered = tuple(
        sorted(
            rules,
            key=lambda rule: (
                rule.team_slug,
                rule.ingress_kind,
                rule.start_ms if rule.start_ms is not None else -1,
                rule.end_ms if rule.end_ms is not None else 2**63,
                rule.rule_id,
            ),
        )
    )
    ids: set[str] = set()
    for rule in ordered:
        if rule.rule_id in ids:
            raise ValueError(f"duplicate prompt authorship rule {rule.rule_id!r}")
        ids.add(rule.rule_id)
        if rule.team_slug not in team_slugs:
            raise ValueError(
                f"prompt authorship rule {rule.rule_id!r} selects unknown team "
                f"{rule.team_slug!r}"
            )
    for index, left in enumerate(ordered):
        for right in ordered[index + 1 :]:
            if _rules_overlap(left, right):
                raise ValueError(
                    "overlapping prompt authorship rules "
                    f"{left.rule_id!r} and {right.rule_id!r}"
                )
    return ordered


def _rules_text(rules: Sequence[PromptAuthorshipRule]) -> str:
    values: list[JsonValue] = [rule.to_json_obj() for rule in rules]
    return canonical_json({"schema_version": 1, "rules": values})


def _load_rules(path: Path) -> tuple[PromptAuthorshipRule, ...]:
    if not path.exists():
        return ()
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"prompt authorship rules are not a regular file: {path}")
    root = as_object(read_json(path), str(path))
    if set(root) != {"schema_version", "rules"}:
        raise ValueError(f"invalid prompt authorship rules document at {path}")
    if as_int(root.get("schema_version"), f"{path}.schema_version") != 1:
        raise ValueError(f"unsupported prompt authorship rules schema at {path}")
    return tuple(
        _prompt_authorship_rule_from_json(value, f"{path}.rules[{index}]")
        for index, value in enumerate(as_array(root.get("rules"), f"{path}.rules"))
    )


def _apply_authorship_rules(
    records: Sequence[dict[str, JsonValue]],
    rules: Sequence[PromptAuthorshipRule],
    migrated_authorship: Mapping[str, _AuthorshipMigration] | None = None,
) -> dict[str, int]:
    applied = {rule.rule_id: 0 for rule in rules}
    rules_by_id = {rule.rule_id: rule for rule in rules}
    migrations = migrated_authorship or {}
    for record in records:
        if record.get("record_type") != "prompt":
            continue
        record_id = as_string(record.get("record_id"), "prompt.record_id")
        migrated = migrations.get(record_id)
        current_kind = record.get("source_author_kind", record.get("author_kind"))
        source_kind = current_kind if isinstance(current_kind, str) else None
        source_version = record.get(
            "source_classification_version", record.get("classification_version")
        )
        record["source_author_kind"] = source_kind
        record["source_classification_version"] = source_version
        record["author_kind"] = source_kind
        record["classification_version"] = source_version
        record.pop("authorship_rule_id", None)
        record.pop("authorship_rule_reason", None)
        if source_kind not in _UNCLASSIFIED_AUTHOR_KINDS:
            continue
        matches = [rule for rule in rules if rule.matches(record)]
        if not matches and migrated is not None:
            prior_rule = rules_by_id.get(migrated.rule_id)
            if prior_rule is not None:
                for prior_native_id in migrated.source_native_ids:
                    prior_record = dict(record)
                    prior_record["source_native_id"] = prior_native_id
                    if prior_rule.matches(prior_record):
                        matches = [prior_rule]
                        break
        if len(matches) > 1:
            raise ValueError(
                "multiple prompt authorship rules matched record "
                f"{record.get('record_id')!r}"
            )
        if not matches:
            continue
        rule = matches[0]
        record["author_kind"] = rule.author_kind
        base_version = source_version if isinstance(source_version, str) else "unknown"
        record["classification_version"] = f"{base_version}+rule:{rule.rule_id}"
        record["authorship_rule_id"] = rule.rule_id
        record["authorship_rule_reason"] = rule.reason
        applied[rule.rule_id] += 1
    return applied


def _immutable_projection(record: dict[str, JsonValue]) -> dict[str, JsonValue]:
    mutable = {
        "ordinal",
        "ingress_kind",
        "author_kind",
        "classification_version",
        "source_author_kind",
        "source_classification_version",
        "authorship_rule_id",
        "authorship_rule_reason",
        "in_reply_to_prompt_id",
    }
    return {key: value for key, value in record.items() if key not in mutable}


def _source_occurrence_id(record: dict[str, JsonValue]) -> str:
    """Return the provider occurrence identity without its projected message class."""

    source_event_ids = tuple(
        as_string(value, "record.source_event_ids[]")
        for value in as_array(record.get("source_event_ids"), "record.source_event_ids")
    )
    native_id = record.get("source_native_id")
    return _stable_id(
        "source-occurrence",
        (
            as_string(record.get("team_slug"), "record.team_slug"),
            as_string(record.get("provider"), "record.provider"),
            as_string(record.get("thread_id"), "record.thread_id"),
            as_string(record.get("source_path"), "record.source_path"),
            native_id if isinstance(native_id, str) else "",
            str(as_int(record.get("source_line"), "record.source_line")),
            str(as_int(record.get("timestamp_ms"), "record.timestamp_ms")),
            as_string(record.get("content_sha256"), "record.content_sha256"),
            *source_event_ids,
        ),
    )


def _provenance_migration_id(record: dict[str, JsonValue]) -> str:
    """Return the identity that is stable across a provider provenance overlay."""

    source_event_ids = tuple(
        as_string(value, "record.source_event_ids[]")
        for value in as_array(record.get("source_event_ids"), "record.source_event_ids")
    )
    return _stable_id(
        "provenance-migration",
        (
            as_string(record.get("team_slug"), "record.team_slug"),
            as_string(record.get("provider"), "record.provider"),
            as_string(record.get("thread_id"), "record.thread_id"),
            as_string(record.get("source_path"), "record.source_path"),
            str(as_int(record.get("timestamp_ms"), "record.timestamp_ms")),
            as_string(record.get("content_sha256"), "record.content_sha256"),
            *source_event_ids,
        ),
    )


def _reclassification_projection(
    record: dict[str, JsonValue],
) -> dict[str, JsonValue]:
    """Return fields that must survive a prompt/system/response refinement."""

    projected = _immutable_projection(record)
    for key in ("record_type", "record_id", "logical_record_id"):
        projected.pop(key, None)
    return projected


def _source_native_id_history(
    record: dict[str, JsonValue],
) -> tuple[str, ...]:
    """Return validated prior native IDs retained by a provenance migration."""

    raw = record.get("source_native_id_history")
    if raw is None:
        return ()
    values = tuple(
        as_string(value, "record.source_native_id_history[]")
        for value in as_array(raw, "record.source_native_id_history")
    )
    if len(set(values)) != len(values):
        raise ValueError("record.source_native_id_history contains duplicates")
    return values


def _provenance_migration_projection(
    record: dict[str, JsonValue],
) -> dict[str, JsonValue]:
    """Return fields that cannot change when only source provenance improves."""

    projected = _immutable_projection(record)
    for key in (
        "record_id",
        "logical_record_id",
        "source_native_id",
        "source_native_id_history",
        "source_line",
    ):
        projected.pop(key, None)
    return projected


def _preserve_occurrence_identity(
    record: dict[str, JsonValue], candidate: dict[str, JsonValue]
) -> tuple[str, str, _AuthorshipMigration | None]:
    """Retain durable occurrence/logical IDs while adopting current source fields."""

    incoming_id = as_string(record.get("record_id"), "new record.record_id")
    previous_id = as_string(candidate.get("record_id"), "old record.record_id")
    record["record_id"] = previous_id
    record["logical_record_id"] = as_string(
        candidate.get("logical_record_id"), "old record.logical_record_id"
    )
    prior_native_ids = set(_source_native_id_history(candidate))
    prior_native_id = candidate.get("source_native_id")
    current_native_id = record.get("source_native_id")
    if (
        isinstance(prior_native_id, str)
        and prior_native_id != current_native_id
    ):
        prior_native_ids.add(prior_native_id)
    if isinstance(current_native_id, str):
        prior_native_ids.discard(current_native_id)
    if prior_native_ids:
        record["source_native_id_history"] = list(sorted(prior_native_ids))
    else:
        record.pop("source_native_id_history", None)
    prior_rule_id = candidate.get("authorship_rule_id")
    migration: _AuthorshipMigration | None = None
    if isinstance(prior_rule_id, str):
        migration = _AuthorshipMigration(
            prior_rule_id,
            tuple(sorted(prior_native_ids)),
        )
    return incoming_id, previous_id, migration


def _monotonic_union(
    old: list[dict[str, JsonValue]], new: list[dict[str, JsonValue]]
) -> tuple[
    list[dict[str, JsonValue]],
    int,
    int,
    dict[str, _AuthorshipMigration],
]:
    merged: dict[str, dict[str, JsonValue]] = {}
    old_by_source: dict[str, list[dict[str, JsonValue]]] = {}
    old_by_migration: dict[str, list[dict[str, JsonValue]]] = {}
    for record in old:
        record_id = as_string(record.get("record_id"), "old record.record_id")
        if record_id in merged:
            raise ValueError(f"duplicate existing transcript record {record_id!r}")
        merged[record_id] = record
        old_by_source.setdefault(_source_occurrence_id(record), []).append(record)
        old_by_migration.setdefault(_provenance_migration_id(record), []).append(
            record
        )
    incoming_ids: set[str] = set()
    effective_new_ids: set[str] = set()
    new_source_ids: set[str] = set()
    new_migration_ids: set[str] = set()
    record_id_rewrites: dict[str, str] = {}
    migrated_authorship: dict[str, _AuthorshipMigration] = {}
    reclassified = 0
    for incoming in new:
        record = dict(incoming)
        linked_prompt = record.get("in_reply_to_prompt_id")
        if isinstance(linked_prompt, str):
            record["in_reply_to_prompt_id"] = record_id_rewrites.get(
                linked_prompt, linked_prompt
            )
        record_id = as_string(record.get("record_id"), "new record.record_id")
        if record_id in incoming_ids:
            raise ValueError(f"duplicate new transcript record {record_id!r}")
        incoming_ids.add(record_id)
        source_id = _source_occurrence_id(record)
        if source_id in new_source_ids:
            raise ValueError(
                f"duplicate new transcript source occurrence {source_id!r}"
            )
        new_source_ids.add(source_id)
        migration_id = _provenance_migration_id(record)
        if migration_id in new_migration_ids:
            raise ValueError(
                f"duplicate new transcript migration occurrence {migration_id!r}"
            )
        new_migration_ids.add(migration_id)
        previous = merged.get(record_id)
        if previous is not None:
            if _immutable_projection(previous) != _immutable_projection(record):
                raise ValueError(
                    f"immutable transcript occurrence changed for {record_id!r}"
                )
            merged[record_id] = record
            effective_new_ids.add(record_id)
            continue

        active_source_classes = [
            candidate
            for candidate in old_by_source.get(source_id, ())
            if as_string(candidate.get("record_id"), "old record.record_id") in merged
        ]
        same_class = [
            candidate
            for candidate in active_source_classes
            if candidate.get("record_type") == record.get("record_type")
        ]
        if len(same_class) > 1 or (same_class and len(active_source_classes) > 1):
            raise ValueError(
                f"ambiguous stable transcript identity for {source_id!r}"
            )
        if same_class:
            candidate = same_class[0]
            if _provenance_migration_projection(
                candidate
            ) != _provenance_migration_projection(record):
                raise ValueError(
                    "immutable transcript occurrence changed while retaining its "
                    f"stable identity for {source_id!r}"
                )
            incoming_id, previous_id, authorship = _preserve_occurrence_identity(
                record, candidate
            )
            if authorship is not None:
                migrated_authorship[previous_id] = authorship
            merged[previous_id] = record
            effective_new_ids.add(previous_id)
            record_id_rewrites[incoming_id] = previous_id
            continue

        for previous_class in active_source_classes:
            previous_id = as_string(
                previous_class.get("record_id"), "old record.record_id"
            )
            if _reclassification_projection(
                previous_class
            ) != _reclassification_projection(record):
                raise ValueError(
                    "immutable transcript occurrence changed while its message class "
                    f"was refined for {source_id!r}"
                )
            del merged[previous_id]
            reclassified += 1
        if active_source_classes:
            merged[record_id] = record
            effective_new_ids.add(record_id)
            continue

        migration_candidates = [
            candidate
            for candidate in old_by_migration.get(migration_id, ())
            if as_string(candidate.get("record_id"), "old record.record_id") in merged
        ]
        if len(migration_candidates) > 1:
            raise ValueError(
                "ambiguous transcript provenance migration for "
                f"{migration_id!r}"
            )
        if migration_candidates:
            candidate = migration_candidates[0]
            if _provenance_migration_projection(
                candidate
            ) != _provenance_migration_projection(record):
                raise ValueError(
                    "immutable transcript occurrence changed during provenance "
                    f"migration for {migration_id!r}"
                )
            incoming_id, previous_id, authorship = _preserve_occurrence_identity(
                record, candidate
            )
            if authorship is not None:
                migrated_authorship[previous_id] = authorship
            merged[previous_id] = record
            effective_new_ids.add(previous_id)
            record_id_rewrites[incoming_id] = previous_id
            continue

        merged[record_id] = record
        effective_new_ids.add(record_id)
    carried = len(set(merged) - effective_new_ids)
    return (
        sorted(merged.values(), key=_record_sort_key),
        carried,
        reclassified,
        migrated_authorship,
    )


def _logical_records(
    occurrences: Iterable[dict[str, JsonValue]], record_type: str
) -> list[dict[str, JsonValue]]:
    grouped: dict[str, list[dict[str, JsonValue]]] = {}
    for occurrence in occurrences:
        if occurrence.get("record_type") != record_type:
            continue
        logical_id = as_string(
            occurrence.get("logical_record_id"), "occurrence.logical_record_id"
        )
        grouped.setdefault(logical_id, []).append(occurrence)
    result: list[dict[str, JsonValue]] = []
    for logical_id, group in grouped.items():
        ordered = sorted(group, key=_record_sort_key)
        attributed = [
            item
            for item in ordered
            if isinstance(item.get("author_kind"), str)
            and item.get("author_kind") not in _UNCLASSIFIED_AUTHOR_KINDS
        ]
        attributed_kinds = {
            item.get("author_kind") for item in attributed
        }
        if len(attributed_kinds) > 1:
            raise ValueError(
                f"conflicting authorship classifications for {logical_id!r}: "
                f"{sorted(str(value) for value in attributed_kinds)!r}"
            )
        representative = attributed[0] if attributed else ordered[0]
        record = dict(representative)
        record["record_id"] = logical_id
        occurrence_ids: list[JsonValue] = []
        for occurrence_id in sorted(
            as_string(item.get("record_id"), "occurrence.record_id")
            for item in ordered
        ):
            occurrence_ids.append(occurrence_id)
        occurrence_teams: list[JsonValue] = []
        for occurrence_team in sorted(
            {
                as_string(item.get("team_slug"), "occurrence.team_slug")
                for item in ordered
            }
        ):
            occurrence_teams.append(occurrence_team)
        record["occurrence_ids"] = occurrence_ids
        record["occurrence_teams"] = occurrence_teams
        record["occurrence_count"] = len(ordered)
        result.append(record)
    return sorted(result, key=_record_sort_key)


def _validate_previous_manifest(
    root: Path, archive: Path
) -> dict[str, JsonValue] | None:
    path = root / "manifest.json"
    if not path.exists():
        return None
    manifest = as_object(read_json(path), str(path))
    if manifest.get("schema_version") != TRANSCRIPT_EXPORT_SCHEMA_VERSION:
        raise ValueError(f"unsupported transcript export manifest at {path}")
    files = as_object(manifest.get("files"), f"{path}.files")
    for name in (*_MANAGED_FILES, _BASELINE_FILE):
        entry = as_object(files.get(name), f"{path}.files.{name}")
        expected = as_string(entry.get("sha256"), f"{path}.files.{name}.sha256")
        managed = (
            shared_build_file(archive, name)
            if name == _BASELINE_FILE
            else root / name
        )
        if not managed.is_file() or managed.is_symlink():
            raise ValueError(f"transcript export file is missing or unsafe: {managed}")
        actual = hashlib.sha256(managed.read_bytes()).hexdigest()
        if actual != expected:
            raise ValueError(
                f"transcript export generation is incomplete: {managed} digest mismatch"
            )
    rules_entry = files.get(_AUTHORSHIP_RULES_FILE)
    if rules_entry is not None:
        entry = as_object(
            rules_entry, f"{path}.files.{_AUTHORSHIP_RULES_FILE}"
        )
        expected = as_string(
            entry.get("sha256"),
            f"{path}.files.{_AUTHORSHIP_RULES_FILE}.sha256",
        )
        rules_path = root / _AUTHORSHIP_RULES_FILE
        if not rules_path.is_file() or rules_path.is_symlink():
            raise ValueError(
                f"transcript export file is missing or unsafe: {rules_path}"
            )
        actual = hashlib.sha256(rules_path.read_bytes()).hexdigest()
        if actual != expected:
            raise ValueError(
                "transcript export generation is incomplete: "
                f"{rules_path} digest mismatch"
            )
    return manifest


def export_transcripts(
    archive: Path,
    teams: Iterable[TeamData],
    prompt_authorship_rules: Sequence[PromptAuthorshipRule] | None = None,
    skipped_teams: Sequence[TranscriptTeamSkip] = (),
) -> TranscriptExportReport:
    """Update the archive's append-only coordinator transcript projection.

    This function performs no network requests and has no model integration. Existing source
    occurrences are retained when a provider's current snapshot no longer contains them, which
    protects the extracted corpus from provider-side history rewriting or log rotation.

    ``skipped_teams`` names archive teams the caller could not load. They are *represented* in the
    output -- their occurrences are carried forward and their authorship rules still applied -- but
    nothing that happened to them since their last good run is, so every artifact this function
    writes says which teams they were and why. At least one team must still have loaded: a
    projection assembled entirely from carried-forward records would rewrite the corpus while the
    archive is in a state nobody could read, which is the one moment not to touch it, and it would
    also tell the operator nothing they did not already know from the load failures themselves.
    """

    ordered_teams = tuple(sorted(teams, key=lambda team: team.team_slug))
    if not ordered_teams:
        raise ValueError("transcript extraction requires at least one ingested team")
    if len({team.team_slug for team in ordered_teams}) != len(ordered_teams):
        raise ValueError("transcript extraction received duplicate team slugs")
    ordered_skips = tuple(sorted(skipped_teams, key=lambda skip: skip.team_slug))
    skipped_slugs = {skip.team_slug for skip in ordered_skips}
    if len(skipped_slugs) != len(ordered_skips):
        raise ValueError("transcript extraction received duplicate skipped team slugs")
    loaded_slugs = {team.team_slug for team in ordered_teams}
    if skipped_slugs & loaded_slugs:
        raise ValueError(
            "transcript extraction received a team as both loaded and skipped: "
            + ", ".join(sorted(skipped_slugs & loaded_slugs))
        )
    root = archive / "extracted" / "transcripts"
    baseline_root = shared_build_root(archive)
    previous_manifest = _validate_previous_manifest(root, archive)
    if previous_manifest is not None:
        previous_teams = {
            as_string(value, "transcript manifest team")
            for value in as_array(previous_manifest.get("teams"), "transcript manifest teams")
        }
        # A skipped team is not an omitted team. This guard exists to stop the projection being
        # silently *narrowed* -- by a `--team` filter, or by a team directory that was deleted --
        # and both of those are decisions, recoverable only by a human who knows what was dropped.
        # A team that merely failed to load is still in the archive, still in this manifest's
        # `teams`, still carried in `occurrences.jsonl`, and named in `skipped_teams`, so nothing
        # the guard protects is at risk. Refusing it anyway is precisely how one torn team came to
        # withhold eleven healthy teams' new prompts on every run until someone intervened.
        omitted = sorted(previous_teams - loaded_slugs - skipped_slugs)
        if omitted:
            raise ValueError(
                "monotonic transcript export cannot omit previously extracted teams: "
                + ", ".join(omitted)
            )
    previous_occurrences = read_jsonl(shared_build_file(archive, _BASELINE_FILE))
    configured_rules = (
        tuple(prompt_authorship_rules)
        if prompt_authorship_rules is not None
        else _load_rules(root / _AUTHORSHIP_RULES_FILE)
    )
    # Rules are validated against every team the projection *represents*, not just the teams read
    # this run, and the difference is load-bearing rather than lenient. `_apply_authorship_rules`
    # rebuilds authorship from the source label on every occurrence on every run -- it resets
    # `author_kind` and drops `authorship_rule_id` before re-matching -- so a rule withdrawn
    # because its team could not be loaded would not merely fail to classify anything new: it
    # would *un-classify* that team's carried-forward prompts, silently reverting audited
    # corrections to `unknown` in a projection that otherwise looks complete. Carrying a team's
    # records forward is only safe together with carrying its rules forward.
    projected_teams = (
        loaded_slugs
        | skipped_slugs
        | {
            as_string(record.get("team_slug"), "old record.team_slug")
            for record in previous_occurrences
        }
    )
    dropped_authorship_rules = tuple(
        DroppedAuthorshipRule(rule.rule_id, rule.team_slug)
        for rule in configured_rules
        if rule.team_slug not in projected_teams
    )
    rules = _validate_rules(
        [rule for rule in configured_rules if rule.team_slug in projected_teams],
        projected_teams,
    )

    current_prompts = [
        _base_record(group, "prompt")
        for team in ordered_teams
        for group in _group_events(team, {"user_prompt"})
    ]
    current_responses = _response_records(ordered_teams, current_prompts)
    current_system = [
        _base_record(group, "system_input")
        for team in ordered_teams
        for group in _group_events(team, {"system_input"})
    ]

    current_occurrences = [*current_prompts, *current_responses, *current_system]
    occurrences, carried, reclassified, migrated_authorship = _monotonic_union(
        previous_occurrences, current_occurrences
    )
    rule_counts = _apply_authorship_rules(
        occurrences, rules, migrated_authorship
    )
    prompts = _logical_records(occurrences, "prompt")
    for index, prompt in enumerate(prompts, 1):
        prompt["ordinal"] = index
    occurrence_to_logical_prompt = {
        as_string(occurrence_id, "prompt.occurrence_ids[]"): as_string(
            prompt.get("record_id"), "prompt.record_id"
        )
        for prompt in prompts
        for occurrence_id in as_array(
            prompt.get("occurrence_ids"), "prompt.occurrence_ids"
        )
    }
    responses = _logical_records(occurrences, "response")
    for response in responses:
        linked = response.get("in_reply_to_prompt_id")
        response["in_reply_to_prompt_id"] = (
            occurrence_to_logical_prompt.get(linked)
            if isinstance(linked, str)
            else None
        )
    system_inputs = _logical_records(occurrences, "system_input")
    messages = sorted([*prompts, *responses], key=_record_sort_key)

    texts = {
        "occurrences.jsonl": canonical_jsonl(occurrences),
        "prompts.jsonl": canonical_jsonl(prompts),
        "messages.jsonl": canonical_jsonl(messages),
        "system-inputs.jsonl": canonical_jsonl(system_inputs),
    }
    rules_text = _rules_text(rules)
    changed = 0
    for name in _MANAGED_FILES:
        changed += int(write_text_if_changed(root / name, texts[name]))
    baseline_root.mkdir(parents=True, exist_ok=True)
    changed += int(
        write_text_if_changed(baseline_root / _BASELINE_FILE, texts[_BASELINE_FILE])
    )
    changed += int(
        write_text_if_changed(root / _AUTHORSHIP_RULES_FILE, rules_text)
    )

    file_manifest: dict[str, JsonValue] = {}
    for name in (*_MANAGED_FILES, _BASELINE_FILE):
        text = texts[name]
        file_manifest[name] = {
            "sha256": _sha256_text(text),
            "bytes": len(text.encode("utf-8")),
            "records": len(
                occurrences
                if name == "occurrences.jsonl"
                else prompts
                if name == "prompts.jsonl"
                else messages
                if name == "messages.jsonl"
                else system_inputs
            ),
        }
    file_manifest[_AUTHORSHIP_RULES_FILE] = {
        "sha256": _sha256_text(rules_text),
        "bytes": len(rules_text.encode("utf-8")),
        "records": len(rules),
    }
    source_generations: list[JsonValue] = [
        {
            "team_slug": team.team_slug,
            "provider": team.provider,
            "root_thread_id": team.root_thread_id,
            "source_digest": source_digest(team),
            "sources": len(team.sources),
        }
        for team in ordered_teams
    ]
    rule_count_values: dict[str, JsonValue] = {
        rule_id: count for rule_id, count in rule_counts.items()
    }
    team_values: list[JsonValue] = list(sorted(loaded_slugs | skipped_slugs))
    skipped_values: list[JsonValue] = [
        skip.to_manifest_obj() for skip in ordered_skips
    ]
    dropped_rule_values: list[JsonValue] = [
        rule.to_json_obj() for rule in dropped_authorship_rules
    ]
    manifest: dict[str, JsonValue] = {
        "schema_version": TRANSCRIPT_EXPORT_SCHEMA_VERSION,
        "kind": "mechanical-coordinator-transcript-export",
        "model_calls": 0,
        "model_tokens": 0,
        # Every team this projection represents, read or carried -- which is what the next run's
        # omission guard needs. Listing only the teams read this run would let a skipped team fall
        # out of the manifest, and a team that has fallen out can afterwards be deleted from the
        # archive entirely without the guard ever noticing.
        "teams": team_values,
        # ...whereas `source_generations` holds only the teams actually read, because a generation
        # entry asserts a source digest that was computed this run. There is no honest entry to
        # write for a team nobody could load, and inventing one from the previous manifest would
        # be a claim about bytes this run never saw.
        "source_generations": source_generations,
        "skipped_teams": skipped_values,
        "dropped_prompt_authorship_rules": dropped_rule_values,
        "counts": {
            "prompts": len(prompts),
            "responses": len(responses),
            "system_inputs": len(system_inputs),
            "occurrences": len(occurrences),
            "carried_forward": carried,
            "reclassified": reclassified,
            "prompt_authorship_rules": len(rules),
            "teams_read": len(ordered_teams),
            "teams_skipped": len(ordered_skips),
        },
        "prompt_authorship": {
            "rule_application_counts": rule_count_values,
            "unclassified_prompts": sum(
                1
                for prompt in prompts
                if prompt.get("author_kind") in _UNCLASSIFIED_AUTHOR_KINDS
            ),
        },
        "ordinal_contract": (
            "Prompt ordinals are 1-based chronological projection indexes; stable record_id "
            "values, not ordinals, are durable identities."
        ),
        "files": file_manifest,
    }
    changed += int(write_text_if_changed(root / "manifest.json", canonical_json(manifest)))
    changed += int(
        write_text_if_changed(
            archive / "timeline", standalone_query_source(), executable=True
        )
    )
    changed += int(write_text_if_changed(archive / "Makefile", archive_makefile()))
    changed += prune_retired_query_artifacts(archive)
    return TranscriptExportReport(
        teams=len(ordered_teams),
        prompts=len(prompts),
        responses=len(responses),
        system_inputs=len(system_inputs),
        carried_forward=carried,
        files_changed=changed,
        reclassified=reclassified,
        skipped_teams=ordered_skips,
        dropped_authorship_rules=dropped_authorship_rules,
    )


__all__ = [
    "DroppedAuthorshipRule",
    "PromptAuthorshipRule",
    "TRANSCRIPT_EXPORT_SCHEMA_VERSION",
    "TranscriptExportReport",
    "TranscriptTeamSkip",
    "export_transcripts",
]
