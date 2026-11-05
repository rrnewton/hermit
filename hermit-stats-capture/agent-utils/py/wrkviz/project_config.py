"""Strict, versioned configuration for zero-model multi-team ingestion."""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
from traceback import format_exception
from typing import Literal, cast

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    narrow_json,
)
from wrkviz.claude_history import PROVIDER as CLAUDE_HISTORY_PROVIDER, compile_project_pattern
from wrkviz.identity import IdentityOverrides, parse_identity_overrides
from wrkviz.orc import OrcContinuationSpec, OrcParseError
from wrkviz.pipeline import (
    IngestReport,
    extract_transcripts_archive,
    ingest_claude,
    ingest_claude_history,
    ingest_codex,
    ingest_orc,
)
from wrkviz.transcript_export import (
    PromptAuthorshipRule,
    TranscriptExportReport,
)
from wrkviz.window import DateWindow, parse_date_window


PROJECT_INGEST_SCHEMA_VERSION = 1
_TEAM_SLUG = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
Provider = Literal["codex", "claude", "orc", "claude-history"]


@dataclass(frozen=True)
class CodexProjectSource:
    """One Codex coordinator root and its explicitly ordered continuations."""

    sessions_root: Path
    root_session: str
    continuation_sessions: tuple[str, ...]


@dataclass(frozen=True)
class ClaudeProjectSource:
    """One canonical Claude coordinator JSONL file."""

    session_file: Path


@dataclass(frozen=True)
class ClaudeHistoryProjectSource:
    """One Claude Code ``history.jsonl`` and the working-directory pattern that selects a team.

    ``covered_sessions`` is ``"config"`` -- every ``claude`` team in the same manifest is a
    covered session, which is the answer almost every project wants and the one that cannot
    drift from the manifest -- or ``"none"``, for a history archived on its own.
    """

    history_file: Path
    project_pattern: str
    covered_sessions: Literal["config", "none"]


@dataclass(frozen=True)
class OrcProjectSource:
    """One Orc coordinator root and its explicitly ordered continuations."""

    source_root: Path
    root_session: str
    continuation_sessions: tuple[OrcContinuationSpec, ...]


ProjectSource = (
    CodexProjectSource
    | ClaudeProjectSource
    | OrcProjectSource
    | ClaudeHistoryProjectSource
)


@dataclass(frozen=True)
class ProjectTeamConfig:
    """Validated provider inputs and archive identity for one registered team."""

    slug: str
    provider: Provider
    source: ProjectSource
    timezone: str
    date_window: DateWindow | None
    identity_overrides: IdentityOverrides
    prompt_authorship_rules: tuple[PromptAuthorshipRule, ...]


@dataclass(frozen=True)
class ProjectIngestConfig:
    """A validated schema-v1 project manifest resolved relative to its file."""

    config_path: Path
    config_sha256: str
    output: Path
    #: Where the vendor source snapshots go, or ``None`` to let the archive's recorded layout --
    #: and, for a brand-new archive, ``<output>.sources`` -- decide. Optional rather than required
    #: because the default is the answer almost every project wants, and a manifest that had to
    #: state it would make every existing manifest invalid for no gain.
    snapshot_root: Path | None
    teams: tuple[ProjectTeamConfig, ...]

    @property
    def prompt_authorship_rules(self) -> tuple[PromptAuthorshipRule, ...]:
        """Return every team-scoped rule in deterministic manifest order."""

        return tuple(
            rule
            for team in self.teams
            for rule in team.prompt_authorship_rules
        )

    @property
    def claude_session_ids(self) -> tuple[str, ...]:
        """Every session a ``claude`` team in this manifest archives in full, by transcript stem.

        This is what a ``claude-history`` team with ``covered_sessions: config`` excludes. It is
        read off the whole manifest rather than off the run's ``--team`` selection, so that
        ingesting the history team alone gives the same answer as ingesting everything.
        """

        return tuple(
            sorted(
                {
                    team.source.session_file.stem
                    for team in self.teams
                    if isinstance(team.source, ClaudeProjectSource)
                }
            )
        )

    def select_teams(self, requested: Sequence[str] = ()) -> tuple[ProjectTeamConfig, ...]:
        """Return configured teams in manifest order, validating an optional filter."""

        if len(set(requested)) != len(requested):
            raise ValueError("--team selection contains duplicates")
        by_slug = {team.slug: team for team in self.teams}
        unknown = sorted(set(requested) - set(by_slug))
        if unknown:
            raise ValueError(
                "team selection is not registered in project config: "
                + ", ".join(unknown)
            )
        if not requested:
            return self.teams
        selected = set(requested)
        return tuple(team for team in self.teams if team.slug in selected)


@dataclass(frozen=True)
class ProjectTeamIngestResult:
    """Mechanical ingest result for one configured team."""

    team_slug: str
    provider: Provider
    ingest: IngestReport

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return this result as run-receipt metadata."""

        return {
            "team_slug": self.team_slug,
            "provider": self.provider,
            "ingest": self.ingest.to_json_obj(),
        }


@dataclass(frozen=True)
class ProjectTeamIngestFailure:
    """One configured team whose ingest raised, recorded instead of aborting the run.

    ``error_type`` is the exception class name rather than a category of our own invention: it is
    what a reader needs to tell an ``OrcParseError`` about one archive's bytes apart from an
    ``OSError`` about a missing mount, and it costs nothing to keep exact. ``traceback`` is present
    only for exception types this package does not classify as data/IO failure, because for those
    the one-line message is a defect report with the evidence removed.
    """

    team_slug: str
    provider: Provider
    error_type: str
    error: str
    traceback: str | None = None

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return this failure as run-receipt metadata."""

        value: dict[str, JsonValue] = {
            "team_slug": self.team_slug,
            "provider": self.provider,
            "error_type": self.error_type,
            "error": self.error,
        }
        if self.traceback is not None:
            value["traceback"] = self.traceback
        return value

    @property
    def summary(self) -> str:
        """Return one operator-readable line naming the team, provider, and cause."""

        return f"{self.team_slug} ({self.provider}): {self.error_type}: {self.error}"


@dataclass(frozen=True)
class ProjectIngestReport:
    """Results of configured team ingests and the global transcript projection.

    ``transcripts`` is optional and ``failures`` may be non-empty because a project run reports a
    partial outcome instead of collapsing into a single exception. Callers must consult
    :attr:`failed`: reporting a partial run as a success would make the missing team invisible to
    everything that reads only an exit status.
    """

    config_sha256: str
    teams: tuple[ProjectTeamIngestResult, ...]
    transcripts: TranscriptExportReport | None
    failures: tuple[ProjectTeamIngestFailure, ...] = ()
    transcript_error: str | None = None
    # The ``team:session`` pairs this run was *permitted* to re-baseline, whether or not any of
    # them needed it. Recorded separately from the overrides that actually fired, because "the
    # operator left the override switched on in a scheduled job" and "no rewrite happened" look
    # identical otherwise, and the first of those is a standing invitation to launder a real
    # rewrite. The session half is kept here rather than reduced to a team list precisely because
    # the authorization is per session: a receipt saying only "orc-team" would read as a standing
    # permission over that team's whole session tree, which is the thing the pair exists to deny.
    accept_prefix_rewrite_sessions: tuple[str, ...] = ()

    @property
    def failed(self) -> bool:
        """Return whether any selected team, or the projection itself, did not complete.

        A *partial* projection counts as a failure. It is tempting to call it a success because
        the file was written and most teams are in it, but the exit status is the only signal an
        unattended scheduler reads, and "eleven of twelve teams' prompts are current" needs a
        human exactly as much as "no prompts are current" does. A skipped team is also not
        self-announcing from the ingest side: it can be skipped by extraction while its own ingest
        succeeded or was never attempted at all, so ``failures`` being empty proves nothing about
        the projection.
        """

        return (
            bool(self.failures)
            or self.transcripts is None
            or self.transcripts.partial
        )

    def failure_summary(self) -> str | None:
        """Return one line naming every failure, or ``None`` when the run was clean."""

        if not self.failed:
            return None
        parts: list[str] = []
        if self.failures:
            selected = len(self.teams) + len(self.failures)
            parts.append(
                f"{len(self.failures)} of {selected} teams failed: "
                + "; ".join(failure.summary for failure in self.failures)
            )
        if self.transcript_error is not None:
            parts.append(f"transcript extraction failed: {self.transcript_error}")
        if self.transcripts is not None:
            partiality = self.transcripts.partiality_summary()
            if partiality is not None:
                parts.append(f"transcript extraction was partial: {partiality}")
        return " | ".join(parts)

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return an explicit zero-model run receipt payload.

        ``schema_version`` deliberately does not move for the failure fields. It is the *config*
        manifest version as well as the receipt version, so raising it here would reject every
        manifest on disk in order to describe a purely additive receipt change. The failure keys
        are always written -- ``failed_teams`` as an empty list and
        ``transcript_extraction_error`` as null on a clean run -- so a reader never has to
        distinguish an absent key from an empty one.
        """

        team_values: list[JsonValue] = [team.to_json_obj() for team in self.teams]
        failure_values: list[JsonValue] = [
            failure.to_json_obj() for failure in self.failures
        ]
        accepted_values: list[JsonValue] = list(self.accept_prefix_rewrite_sessions)
        return {
            "schema_version": PROJECT_INGEST_SCHEMA_VERSION,
            "config_sha256": self.config_sha256,
            "status": "failed" if self.failed else "completed",
            "teams": team_values,
            "teams_succeeded": len(self.teams),
            "failed_teams": failure_values,
            "teams_failed": len(self.failures),
            "transcript_extraction": (
                None if self.transcripts is None else self.transcripts.to_json_obj()
            ),
            "transcript_extraction_error": self.transcript_error,
            "accepted_prefix_rewrite_sessions": accepted_values,
            "model_calls": 0,
            "model_tokens": 0,
            "website_build_performed": False,
        }


@dataclass(frozen=True)
class _WindowSpec:
    start_date: str | None = None
    start_time: str | None = None
    end_date: str | None = None
    end_time: str | None = None

    def resolve(self, timezone: str) -> DateWindow | None:
        return parse_date_window(
            self.start_date,
            self.end_date,
            timezone,
            start_time=self.start_time,
            end_time=self.end_time,
        )


def _check_fields(
    value: dict[str, JsonValue],
    where: str,
    required: frozenset[str],
    optional: frozenset[str] = frozenset(),
) -> None:
    missing = sorted(required - set(value))
    unknown = sorted(set(value) - required - optional)
    if missing or unknown:
        raise ValueError(
            f"{where}: invalid fields; missing={missing!r}, unknown={unknown!r}"
        )


def _required_string(value: JsonValue, where: str) -> str:
    result = as_string(value, where).strip()
    if not result or "\0" in result:
        raise ValueError(f"{where}: expected a non-empty string")
    return result


def _optional_string(
    value: dict[str, JsonValue], key: str, where: str
) -> str | None:
    if key not in value:
        return None
    return _required_string(value[key], f"{where}.{key}")


def _string_array(value: JsonValue, where: str) -> tuple[str, ...]:
    result = tuple(
        _required_string(item, f"{where}[{index}]")
        for index, item in enumerate(as_array(value, where))
    )
    if len(set(result)) != len(result):
        raise ValueError(f"{where}: duplicate values are not allowed")
    return result


def _orc_continuation_array(
    value: JsonValue, where: str
) -> tuple[OrcContinuationSpec, ...]:
    result: list[OrcContinuationSpec] = []
    for index, item in enumerate(as_array(value, where)):
        item_where = f"{where}[{index}]"
        try:
            spec = OrcContinuationSpec.from_value(item, item_where)
        except OrcParseError as error:
            raise ValueError(str(error)) from error
        if not isinstance(item, str) and spec.start_message_id is None:
            raise ValueError(
                f"{item_where}.start_message_id: expected a non-empty string"
            )
        result.append(spec)
    session_ids = tuple(spec.session_id for spec in result)
    if len(set(session_ids)) != len(session_ids):
        raise ValueError(f"{where}: duplicate session ids are not allowed")
    return tuple(result)


def _relative_output(config_path: Path, raw: str) -> Path:
    candidate = Path(raw).expanduser()
    if candidate.is_absolute():
        raise ValueError("project config output must be relative to the config file")
    return (config_path.parent / candidate).resolve()


def _snapshot_root(config_path: Path, raw: str) -> Path:
    """Resolve the snapshot store, allowing an absolute path where ``output`` does not.

    ``output`` is required to be relative because the archive is the artifact and a manifest that
    hardcoded where somebody's artifact lives could not be shared. The snapshot store is the
    opposite kind of thing: it is machine-local bulk, and the reason an operator overrides its
    location at all is usually that the artifact and the bulk belong on *different filesystems* --
    a small versioned tree on the laptop's disk, six gigabytes of vendor logs on the big one. A
    rule that forced that to be spelled as `../../../../mnt/big/sources` would be pedantry.
    """

    candidate = Path(raw).expanduser()
    if not candidate.is_absolute():
        candidate = config_path.parent / candidate
    return candidate.resolve()


def _source_path(config_path: Path, raw: str) -> Path:
    candidate = Path(raw).expanduser()
    if not candidate.is_absolute():
        candidate = config_path.parent / candidate
    return candidate.resolve()


def _project_values(value: JsonValue, where: str) -> tuple[str, ...]:
    result: list[str] = []
    for index, raw in enumerate(as_array(value, where)):
        item_where = f"{where}[{index}]"
        item = as_object(raw, item_where)
        _check_fields(
            item,
            item_where,
            frozenset({"label", "repository_url"}),
        )
        label = _required_string(item.get("label"), item_where + ".label")
        repository_url = _required_string(
            item.get("repository_url"), item_where + ".repository_url"
        )
        result.append(f"{label}={repository_url}")
    return tuple(result)


def _window_spec(value: JsonValue, where: str) -> _WindowSpec:
    obj = as_object(value, where)
    _check_fields(
        obj,
        where,
        frozenset(),
        frozenset({"start_date", "start_time", "end_date", "end_time"}),
    )
    start_date = _optional_string(obj, "start_date", where)
    start_time = _optional_string(obj, "start_time", where)
    end_date = _optional_string(obj, "end_date", where)
    end_time = _optional_string(obj, "end_time", where)
    if start_date is not None and start_time is not None:
        raise ValueError(f"{where}: choose start_date or start_time, not both")
    if end_date is not None and end_time is not None:
        raise ValueError(f"{where}: choose end_date or end_time, not both")
    return _WindowSpec(start_date, start_time, end_date, end_time)


def _prompt_authorship_rules(
    value: JsonValue, where: str, team_slug: str
) -> tuple[PromptAuthorshipRule, ...]:
    result: list[PromptAuthorshipRule] = []
    for index, raw in enumerate(as_array(value, where)):
        item_where = f"{where}[{index}]"
        item = as_object(raw, item_where)
        _check_fields(
            item,
            item_where,
            frozenset(
                {"id", "ingress_kind", "author_kind", "reason"}
            ),
            frozenset({"start_time", "end_time", "source_native_ids"}),
        )
        window = parse_date_window(
            None,
            None,
            "UTC",
            start_time=_optional_string(item, "start_time", item_where),
            end_time=_optional_string(item, "end_time", item_where),
        )
        source_native_ids = (
            _string_array(
                item["source_native_ids"], item_where + ".source_native_ids"
            )
            if "source_native_ids" in item
            else ()
        )
        result.append(
            PromptAuthorshipRule(
                _required_string(item.get("id"), item_where + ".id"),
                team_slug,
                _required_string(
                    item.get("ingress_kind"), item_where + ".ingress_kind"
                ),
                _required_string(
                    item.get("author_kind"), item_where + ".author_kind"
                ),
                _required_string(item.get("reason"), item_where + ".reason"),
                window.start_ms if window is not None else None,
                window.end_ms if window is not None else None,
                source_native_ids,
            )
        )
    return tuple(result)


def _codex_source(
    value: dict[str, JsonValue], where: str, config_path: Path
) -> CodexProjectSource:
    _check_fields(
        value,
        where,
        frozenset({"sessions_root", "root_session"}),
        frozenset({"continuation_sessions"}),
    )
    root_session = _required_string(value.get("root_session"), where + ".root_session")
    continuations = (
        _string_array(
            value["continuation_sessions"], where + ".continuation_sessions"
        )
        if "continuation_sessions" in value
        else ()
    )
    if root_session in continuations:
        raise ValueError(f"{where}: root_session cannot also be a continuation")
    return CodexProjectSource(
        _source_path(
            config_path,
            _required_string(value.get("sessions_root"), where + ".sessions_root"),
        ),
        root_session,
        continuations,
    )


def _claude_source(
    value: dict[str, JsonValue], where: str, config_path: Path
) -> ClaudeProjectSource:
    _check_fields(value, where, frozenset({"session_file"}))
    return ClaudeProjectSource(
        _source_path(
            config_path,
            _required_string(value.get("session_file"), where + ".session_file"),
        )
    )


def _claude_history_source(
    value: dict[str, JsonValue], where: str, config_path: Path
) -> ClaudeHistoryProjectSource:
    _check_fields(
        value,
        where,
        frozenset({"history_file", "project_pattern"}),
        frozenset({"covered_sessions"}),
    )
    pattern = _required_string(value.get("project_pattern"), where + ".project_pattern")
    try:
        compile_project_pattern(pattern)
    except ValueError as exc:
        raise ValueError(f"{where}.project_pattern: {exc}") from exc
    covered = (
        _required_string(value["covered_sessions"], where + ".covered_sessions")
        if "covered_sessions" in value
        else "config"
    )
    if covered not in ("config", "none"):
        raise ValueError(f"{where}.covered_sessions: expected config or none")
    return ClaudeHistoryProjectSource(
        _source_path(
            config_path,
            _required_string(value.get("history_file"), where + ".history_file"),
        ),
        pattern,
        cast(Literal["config", "none"], covered),
    )


def _orc_source(
    value: dict[str, JsonValue], where: str, config_path: Path
) -> OrcProjectSource:
    _check_fields(
        value,
        where,
        frozenset({"source_root", "root_session"}),
        frozenset({"continuation_sessions"}),
    )
    root_session = _required_string(value.get("root_session"), where + ".root_session")
    continuations = (
        _orc_continuation_array(
            value["continuation_sessions"], where + ".continuation_sessions"
        )
        if "continuation_sessions" in value
        else ()
    )
    if root_session in (spec.session_id for spec in continuations):
        raise ValueError(f"{where}: root_session cannot also be a continuation")
    return OrcProjectSource(
        _source_path(
            config_path,
            _required_string(value.get("source_root"), where + ".source_root"),
        ),
        root_session,
        continuations,
    )


def _team_config(
    value: JsonValue,
    where: str,
    config_path: Path,
    default_timezone: str,
    default_projects: tuple[str, ...],
    default_hosts: tuple[str, ...],
    default_window: _WindowSpec,
) -> ProjectTeamConfig:
    obj = as_object(value, where)
    _check_fields(
        obj,
        where,
        frozenset({"slug", "provider", "source"}),
        frozenset(
            {
                "timezone",
                "projects",
                "source_hosts",
                "window",
                "prompt_authorship_rules",
            }
        ),
    )
    slug = _required_string(obj.get("slug"), where + ".slug")
    if len(slug) > 64 or _TEAM_SLUG.fullmatch(slug) is None:
        raise ValueError(
            f"{where}.slug: expected 1-64 lowercase letters/digits separated by hyphens"
        )
    raw_provider = _required_string(obj.get("provider"), where + ".provider")
    if raw_provider not in ("codex", "claude", "orc", CLAUDE_HISTORY_PROVIDER):
        raise ValueError(
            f"{where}.provider: expected codex, claude, orc, or {CLAUDE_HISTORY_PROVIDER}"
        )
    provider = cast(Provider, raw_provider)
    source_value = as_object(obj.get("source"), where + ".source")
    source: ProjectSource
    if provider == "codex":
        source = _codex_source(source_value, where + ".source", config_path)
    elif provider == "claude":
        source = _claude_source(source_value, where + ".source", config_path)
    elif provider == CLAUDE_HISTORY_PROVIDER:
        source = _claude_history_source(source_value, where + ".source", config_path)
    else:
        source = _orc_source(source_value, where + ".source", config_path)
    timezone = (
        _required_string(obj["timezone"], where + ".timezone")
        if "timezone" in obj
        else default_timezone
    )
    project_values = (
        _project_values(obj["projects"], where + ".projects")
        if "projects" in obj
        else default_projects
    )
    host_values = (
        _string_array(obj["source_hosts"], where + ".source_hosts")
        if "source_hosts" in obj
        else default_hosts
    )
    projects, hosts = parse_identity_overrides(project_values, host_values)
    window_spec = (
        _window_spec(obj["window"], where + ".window")
        if "window" in obj
        else default_window
    )
    authorship_rules = (
        _prompt_authorship_rules(
            obj["prompt_authorship_rules"],
            where + ".prompt_authorship_rules",
            slug,
        )
        if "prompt_authorship_rules" in obj
        else ()
    )
    return ProjectTeamConfig(
        slug,
        provider,
        source,
        timezone,
        window_spec.resolve(timezone),
        IdentityOverrides(projects, hosts, "config"),
        authorship_rules,
    )


def load_project_ingest_config(path: Path) -> ProjectIngestConfig:
    """Load and strictly validate one project-ingest schema-v1 JSON file."""

    config_path = path.expanduser().resolve()
    config_bytes = config_path.read_bytes()
    raw: object = json.loads(config_bytes.decode("utf-8"))
    root = as_object(narrow_json(raw, str(config_path)), str(config_path))
    _check_fields(
        root,
        str(config_path),
        frozenset({"schema_version", "output", "teams"}),
        frozenset(
            {"timezone", "projects", "source_hosts", "window", "snapshot_root"}
        ),
    )
    schema_version = as_int(root.get("schema_version"), str(config_path) + ".schema_version")
    if schema_version != PROJECT_INGEST_SCHEMA_VERSION:
        raise ValueError(
            f"{config_path}: unsupported schema_version {schema_version}; "
            f"expected {PROJECT_INGEST_SCHEMA_VERSION}"
        )
    output = _relative_output(
        config_path,
        _required_string(root.get("output"), str(config_path) + ".output"),
    )
    snapshot_root = (
        _snapshot_root(
            config_path,
            _required_string(root["snapshot_root"], str(config_path) + ".snapshot_root"),
        )
        if "snapshot_root" in root
        else None
    )
    timezone = (
        _required_string(root["timezone"], str(config_path) + ".timezone")
        if "timezone" in root
        else "America/New_York"
    )
    projects = (
        _project_values(root["projects"], str(config_path) + ".projects")
        if "projects" in root
        else ()
    )
    source_hosts = (
        _string_array(root["source_hosts"], str(config_path) + ".source_hosts")
        if "source_hosts" in root
        else ()
    )
    window = (
        _window_spec(root["window"], str(config_path) + ".window")
        if "window" in root
        else _WindowSpec()
    )
    raw_teams = as_array(root.get("teams"), str(config_path) + ".teams")
    if not raw_teams:
        raise ValueError(f"{config_path}.teams: expected at least one team")
    teams = tuple(
        _team_config(
            raw,
            f"{config_path}.teams[{index}]",
            config_path,
            timezone,
            projects,
            source_hosts,
            window,
        )
        for index, raw in enumerate(raw_teams)
    )
    slugs = tuple(team.slug for team in teams)
    if len(set(slugs)) != len(slugs):
        raise ValueError(f"{config_path}.teams: duplicate team slugs are not allowed")
    rule_ids = [
        rule.rule_id
        for team in teams
        for rule in team.prompt_authorship_rules
    ]
    if len(set(rule_ids)) != len(rule_ids):
        raise ValueError(
            f"{config_path}.teams: duplicate prompt authorship rule ids are not allowed"
        )
    config_sha256 = hashlib.sha256(config_bytes).hexdigest()
    return ProjectIngestConfig(
        config_path, config_sha256, output, snapshot_root, teams
    )


def _ingest_team(
    output: Path,
    team: ProjectTeamConfig,
    accept_prefix_rewrite: Sequence[str],
    snapshot_root: Path | None,
    claude_session_ids: Sequence[str] = (),
) -> IngestReport:
    """Dispatch one configured team to its provider importer."""

    source = team.source
    if isinstance(source, ClaudeHistoryProjectSource):
        _, report = ingest_claude_history(
            output,
            source.history_file,
            team.slug,
            team.timezone,
            source.project_pattern,
            claude_session_ids if source.covered_sessions == "config" else (),
            team.date_window,
            team.identity_overrides,
            snapshot_root,
        )
    elif isinstance(source, CodexProjectSource):
        _, report = ingest_codex(
            output,
            source.sessions_root,
            source.root_session,
            team.slug,
            team.timezone,
            team.date_window,
            team.identity_overrides,
            source.continuation_sessions,
            snapshot_root,
        )
    elif isinstance(source, ClaudeProjectSource):
        _, report = ingest_claude(
            output,
            source.session_file,
            team.slug,
            team.timezone,
            team.date_window,
            team.identity_overrides,
            snapshot_root,
        )
    else:
        _, report = ingest_orc(
            output,
            source.source_root,
            source.root_session,
            team.slug,
            team.timezone,
            team.date_window,
            team.identity_overrides,
            source.continuation_sessions,
            accept_prefix_rewrite,
            snapshot_root,
        )
    return report


def _team_failure(
    team: ProjectTeamConfig, error: Exception
) -> ProjectTeamIngestFailure:
    """Describe one team's raised exception without deciding what the run should do about it."""

    # Every provider parse error subclasses ValueError, and every filesystem fault arrives as
    # OSError, so this pair is the whole classified failure surface of the importers -- the same
    # pair the CLI has always treated as "expected, report it and exit 2". Anything else is a defect
    # in this package rather than a property of one team's logs, so it keeps its traceback: a
    # KeyError reduced to the single line "'messages'" is unactionable, and this receipt is the only
    # place the evidence would otherwise survive an unattended overnight run.
    #
    # The traceback is formatted from the exception object rather than from format_exc(), so this
    # function does not silently produce "NoneType: None" if it is ever called outside an active
    # `except` block.
    expected = isinstance(error, (OSError, ValueError))
    return ProjectTeamIngestFailure(
        team.slug,
        team.provider,
        type(error).__name__,
        str(error),
        None if expected else "".join(format_exception(error)),
    )


def _prefix_rewrite_selection(
    selected: Sequence[ProjectTeamConfig], requested: Sequence[str]
) -> dict[str, frozenset[str]]:
    """Validate the ``TEAM:SESSION`` append-prefix overrides against the teams this run ingests.

    The override is named per team *and* per session rather than passed as one blanket switch,
    because both scopes fan out. A project run ingests a dozen teams at once, so a bare boolean
    would silently extend one team's known, diagnosed metadata backfill to eleven others whose
    sources nobody had looked at; and each Orc team is a whole session tree, so a per-team boolean
    would do the identical thing one level down, to sessions the operator has structurally never
    been shown -- the guard raises on the first mismatching source, so a second rewritten session
    is never printed before the flag is passed. The pair is the smallest thing that names only what
    was actually inspected.

    Every kind of near miss is rejected here, before any ingest starts, rather than quietly doing
    nothing: a value that is not a ``TEAM:SESSION`` pair at all (which is exactly what an operator
    produces by pasting the bare session id that ``ingest-orc``'s refusal prints), a typo'd slug, a
    slug for a Codex or Claude team that has no append-prefix guard, and a slug that is registered
    but excluded from this run's ``--team`` filter. A flag that appears to have been accepted but
    had no effect is the worst possible outcome for a safety override, because the next person
    reads the command line and believes it did something.

    The session half is deliberately *not* validated here. Which sessions a lineage contains is not
    knowable from the manifest -- discovery walks the source root's session index -- so checking it
    here would mean opening every team's databases before the run, duplicating work the snapshotter
    does anyway. It is validated inside :func:`wrkviz.orc.snapshot_orc_lineage`
    against the discovered lineage, before a single database is copied, and that error arrives on
    the failing team's receipt line where the team slug is already printed.
    """

    if len(set(requested)) != len(requested):
        raise ValueError(
            "--accept-orc-prefix-rewrite selection contains duplicates"
        )
    by_slug = {team.slug: team for team in selected}
    accepted: dict[str, set[str]] = {}
    malformed: list[str] = []
    for value in requested:
        slug, separator, session_id = value.partition(":")
        # Neither half can itself contain a colon -- team slugs are lowercase alphanumerics and
        # hyphens, session ids are safe path components -- so one partition is a complete parse and
        # there is no quoting rule for an operator to get wrong.
        if not separator or not slug or not session_id:
            malformed.append(value)
            continue
        accepted.setdefault(slug, set()).add(session_id)
    if malformed:
        raise ValueError(
            "--accept-orc-prefix-rewrite takes TEAM:SESSION, naming the one team and the "
            "one session the refusal showed you; these do not: " + ", ".join(malformed)
        )
    unknown = sorted(set(accepted) - set(by_slug))
    if unknown:
        raise ValueError(
            "--accept-orc-prefix-rewrite names teams this run does not ingest: "
            + ", ".join(unknown)
        )
    not_orc = sorted(slug for slug in accepted if by_slug[slug].provider != "orc")
    if not_orc:
        raise ValueError(
            "--accept-orc-prefix-rewrite applies only to orc teams; these are not: "
            + ", ".join(not_orc)
        )
    return {slug: frozenset(sessions) for slug, sessions in accepted.items()}


def ingest_project(
    config: ProjectIngestConfig,
    requested_teams: Sequence[str] = (),
    accept_prefix_rewrite: Sequence[str] = (),
) -> ProjectIngestReport:
    """Ingest selected registered teams, then refresh the global transcript JSONL.

    This operation invokes provider importers and transcript extraction only. It cannot invoke a
    summarizer and deliberately does not build or rewrite the website presentation.

    Teams are isolated from one another: a team that raises is recorded as a failure and the run
    continues with the next one. The provider importers share no mutable state across teams -- each
    writes only under ``teams/<slug>/`` and under its own slug-named directory in the snapshot
    store, while holding the archive writer lock -- and a team that fails keeps, byte for byte,
    the normalized snapshot its last successful ingest wrote.

    ``accept_prefix_rewrite`` holds ``TEAM:SESSION`` pairs naming, individually, the Orc sessions
    whose append-prefix guard this run may re-baseline; the team half is validated against the
    selection and raises before any team is touched, and the session half is validated by the
    snapshotter against the discovered lineage before it copies anything.

    The outcome is returned, never raised, even when every team failed, because the caller needs
    the whole picture to write one honest receipt. Callers must therefore consult
    :attr:`ProjectIngestReport.failed` and set their own exit status from it.
    """

    selected = config.select_teams(requested_teams)
    accepted = _prefix_rewrite_selection(selected, accept_prefix_rewrite)
    results: list[ProjectTeamIngestResult] = []
    failures: list[ProjectTeamIngestFailure] = []
    # One team must not be able to end the run. With no boundary inside this loop, a single
    # unreadable source lineage took down eleven healthy teams *and* the transcript extraction
    # after them, on four consecutive runs of roughly seven minutes each, and the operator's only
    # evidence was one exception naming the one team that was already known to be broken.
    for team in selected:
        try:
            report = _ingest_team(
                config.output,
                team,
                sorted(accepted.get(team.slug, ())),
                config.snapshot_root,
                config.claude_session_ids,
            )
        except Exception as error:  # Deliberately broad; see _team_failure.
            # BaseException is deliberately NOT caught: a KeyboardInterrupt or SystemExit means the
            # operator or the runtime is stopping this process, and continuing on to the next team
            # would be ignoring that instruction, not being robust to a bad team.
            failures.append(_team_failure(team, error))
            continue
        results.append(ProjectTeamIngestResult(team.slug, team.provider, report))

    # Always project every normalized team already in the durable archive. The projection is a
    # monotonic global database and therefore cannot safely omit teams merely because this run's
    # ingest filter selected a subset -- or because a team failed. Extraction reads each team's
    # durable `teams/<slug>/raw/team.json`, so a team that failed today contributes exactly the
    # records it contributed on its last good run, and `_monotonic_union` carries forward any
    # occurrence whose source has since disappeared. Skipping extraction whenever any team failed
    # would therefore withhold the *successful* teams' new prompts for no safety gain -- and the
    # same argument applies one level down, inside extraction, to a team whose durable snapshot
    # cannot be *read*: it is skipped and named there rather than ending the projection.
    transcripts: TranscriptExportReport | None = None
    transcript_error: str | None = None
    try:
        transcripts = extract_transcripts_archive(
            config.output, authorship_rules=config.prompt_authorship_rules
        )
    except Exception as error:  # Deliberately broad, for the same reason as the team loop.
        # Extraction is itself isolated per team now -- a team that fails to load is carried
        # forward and named in `transcripts.skipped_teams`, and a rule pointing at a team the
        # archive has no data for is set aside and named in
        # `transcripts.dropped_authorship_rules` -- so the two shapes that used to arrive here as
        # exceptions no longer do. This stays because the exhaustive claim it would otherwise be
        # making is not one this function can enforce: everything below the loaded teams (the
        # monotonic union's own invariants, a corrupt `occurrences.jsonl`, a full disk mid-write)
        # can still raise, and letting that propagate would discard the per-team results collected
        # above, which is precisely the "one failure erases eleven successes" shape being removed
        # here. Record it and let the caller report both.
        transcript_error = f"{type(error).__name__}: {error}"
    return ProjectIngestReport(
        config.config_sha256,
        tuple(results),
        transcripts,
        tuple(failures),
        transcript_error,
        tuple(
            sorted(
                f"{slug}:{session_id}"
                for slug, sessions in accepted.items()
                for session_id in sessions
            )
        ),
    )


__all__ = [
    "ClaudeHistoryProjectSource",
    "ClaudeProjectSource",
    "CodexProjectSource",
    "ingest_project",
    "load_project_ingest_config",
    "OrcProjectSource",
    "ProjectIngestConfig",
    "ProjectIngestReport",
    "ProjectTeamConfig",
    "ProjectTeamIngestFailure",
    "ProjectTeamIngestResult",
]
