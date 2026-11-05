"""Idempotent ingest -> summarize -> format pipeline for timeline archives."""

from __future__ import annotations

import fcntl
import hashlib
import os
import re
import stat
from collections.abc import Iterator, Mapping, Sequence
from contextlib import ExitStack, contextmanager
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING

from wrkviz import __version__
from wrkviz.build_store import (
    ingested_team_slugs,
    ensure_build_store,
    resolve_build_root,
    team_build_root as _build_root,
)
from wrkviz.archive import (
    ARCHIVE_MARKER_FILE,
    ARCHIVE_MARKER_TOOL,
    LEGACY_ARCHIVE_MARKER_FILE,
    LEGACY_ARCHIVE_MARKER_TOOL,
    archive_marker_path,
    is_archive_marker,
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    canonical_jsonl,
    narrow_json,
    read_json,
    read_jsonl,
    validate_team_slug,
    write_json_durable,
    write_json_if_changed,
    write_text_durable,
    write_text_if_changed,
)
from wrkviz.artifacts import (
    ArtifactCatalog,
    artifact_catalog_from_json,
    canonical_repository_url,
    extract_artifacts,
)
from wrkviz.codex import (
    CodexContinuationLink,
    CodexParseError,
    CodexSourceCopy,
    codex_identity_metadata,
    load_codex_team,
    snapshot_codex_lineage,
)
from wrkviz.claude import (
    ClaudeParseError,
    ClaudeSourceCopy,
    load_claude_team,
    snapshot_claude_lineage,
)
from wrkviz.claude_history import (
    PROVIDER as CLAUDE_HISTORY_PROVIDER,
    ClaudeHistoryParseError,
    load_claude_history_team,
    snapshot_claude_history,
)
from wrkviz.model import (
    Agent,
    Event,
    TaskNote,
    TeamData,
    ToolCall,
    source_digest,
    task_note_key,
)
from wrkviz.model_io import task_note_from_json_obj, team_from_json_obj
from wrkviz.payloads import merge_payloads, payload_ref, resolve_payloads
from wrkviz.naming import (
    AgentNameJob,
    AgentNameResult,
    input_hash_for_provenance as agent_name_input_hash_for_provenance,
    name_agents,
)
from wrkviz.github_metadata import load_pull_request_metadata_cache
from wrkviz.github_enrich import pull_metadata_path
from wrkviz.identity import (
    HostIdentity,
    IdentityOverrides,
    ProjectIdentity,
    SiteIdentity,
    infer_structured_identity,
    merge_site_identity,
    site_identity_from_json_obj,
)
from wrkviz.orc import (
    OrcAppendPrefixOverride,
    OrcContinuationLink,
    OrcContinuationSpec,
    OrcParseError,
    OrcSourceCopy,
    load_orc_team,
    prune_orc_staging,
    prune_orc_snapshot_objects,
    snapshot_orc_lineage,
)
from wrkviz.periods import (
    DEFAULT_ROLLUP_KINDS,
    ROLLUP_KINDS,
    Period,
    periods_for_range,
)
from wrkviz.phases import (
    PhaseStats,
    PhaseWindow,
    aggregate_stats,
    build_phases,
    phase_agent_ids,
)
from wrkviz.render import render_archive
from wrkviz.snapshot_store import (
    SnapshotLocation,
    ensure_snapshot_store,
    read_pointer,
    resolve_snapshot_root,
    write_pointer,
)
from wrkviz.summarize import (
    GLOSSARY_DEFINITION_STYLE,
    PLAIN_LANGUAGE_ROLLUP_STYLE,
    PROJECT_OVERVIEW_STYLE,
    SummaryJob,
    SummaryResult,
    SummaryRunStats,
    TECHNICAL_ROLLUP_STYLE,
    WorkBullet,
    clean_summary_prose,
    clean_summary_result,
    input_hash_for_provenance as summary_input_hash_for_provenance,
    knowledge_text_has_link,
    summarize_jobs,
)
from wrkviz.summary_registry import (
    AGENT_LIFETIME_SUMMARIZER,
    ContextComponent,
    ContextCoverage,
    GLOSSARY_DEFINITION_SUMMARIZER,
    PROJECT_OVERVIEW_SUMMARIZER,
    registry_json_obj,
    summarizer_change_for_prompt,
)
from wrkviz.summary_artifacts import (
    ARTIFACT_ENVELOPE_FORMAT,
    ARTIFACT_ENVELOPE_VERSION,
    SummaryArtifactProvenance,
)
from wrkviz.summary_catalog import (
    SummaryArtifactCatalog,
    SummaryArtifactReference,
    load_summary_catalog,
    merge_summary_catalog,
    select_summary_artifact,
)
from wrkviz.token_usage import TokenUsage, resolve_service_tier
from wrkviz.terminology import (
    GlossaryTerm,
    TermSource,
    glossary_prompt_text,
    glossary_term_id,
    plain_language_context_text,
    scan_terminology,
)
from wrkviz.window import DateWindow, apply_date_window

if TYPE_CHECKING:
    from wrkviz.transcript_export import (
        PromptAuthorshipRule,
        TranscriptExportReport,
        TranscriptTeamSkip,
    )


_ARCHIVE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
_ARCHIVE_MARKER = ARCHIVE_MARKER_FILE
#: Renamed with the tool, and deliberately WITHOUT a legacy fallback. The lock serialises writers
#: of this tool against each other; two DIFFERENT versions of the tool writing one archive at the
#: same moment is not a supported configuration and never was, so holding both names would add a
#: second lock to protect a case that is already outside the contract. A stale
#: `.agent-team-timeline.lock` left by an older build is an empty file that nothing opens.
_ARCHIVE_LOCK = ".wrkviz.lock"
LEGACY_ARCHIVE_LOCK = ".agent-team-timeline.lock"

#: The third archive control name, beside the marker and the lock: where
#: :mod:`wrkviz.archive_gc` puts what it reclaims, so that the first destructive
#: pass is a rename rather than an unlink. Declared here rather than there because
#: `_ensure_bulk_content_ignored` has to name it and `archive_gc` imports *this* module for the
#: writer lock -- putting it the other way round would be a cycle, and duplicating the string
#: would let the ignore rule and the directory drift apart in exactly the case that matters,
#: which is the one where an operator is about to commit.
#: Renamed with the tool. `gc` still recognises the former directory so an operator who has
#: deleted into the old trash can still empty it -- see `LEGACY_ARCHIVE_TRASH_ROOT`.
ARCHIVE_TRASH_ROOT = ".wrkviz-trash"
LEGACY_ARCHIVE_TRASH_ROOT = ".agent-team-timeline-trash"
_PROJECT_OVERVIEW_SCHEMA_VERSION = 3
_GLOSSARY_SCHEMA_VERSION = 3
_ORC_NORMALIZER_SCHEMA_VERSION = 3
_OVERVIEW_CONTEXT_CHARS = 48_000
_TERM_EVIDENCE_LIMIT = 6


#: The slug grammar moved to :mod:`wrkviz.archive` when the snapshot store started
#: naming a directory after a slug *outside* the archive: three modules now depend on the same
#: rule, and the furthest one from the archive is the one where a slug that is not a slug becomes
#: a path traversal. This alias keeps the call sites here unchanged.
_validate_team_slug = validate_team_slug


def _validate_archive_id(value: str, label: str) -> None:
    if _ARCHIVE_ID.fullmatch(value) is None:
        raise ValueError(f"{label} is not a safe archive identifier: {value!r}")


@contextmanager
def archive_writer_lock(archive: Path) -> Iterator[None]:
    """Serialize raw archive transactions across processes using Linux ``flock``.

    Public because reclaiming disk is a writer too. :mod:`wrkviz.archive_gc` takes
    this same lock for both its dry run and its sweep -- the dry run because a report computed
    beside a running build names files the build is in the middle of replacing, and a report
    that cannot be trusted is worse than one that waited.
    """

    if archive.exists() and not archive.is_dir():
        raise ValueError(f"archive output is not a directory: {archive}")
    archive.mkdir(parents=True, exist_ok=True)
    lock_path = archive / _ARCHIVE_LOCK
    try:
        descriptor = os.open(
            lock_path,
            os.O_RDWR | os.O_CREAT | os.O_CLOEXEC | os.O_NOFOLLOW,
            0o600,
        )
    except OSError as exc:
        raise ValueError(f"cannot safely open archive writer lock {lock_path}: {exc}") from exc
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)


@contextmanager
def _archive_writer_locks(*archives: Path) -> Iterator[None]:
    """Acquire several archive locks in canonical order without alias deadlocks."""

    ordered = {
        str(archive.resolve()): archive.resolve()
        for archive in archives
    }
    with ExitStack() as stack:
        for key in sorted(ordered):
            stack.enter_context(archive_writer_lock(ordered[key]))
        yield


def _ensure_archive(archive: Path, team_slug: str, *, create: bool) -> None:
    """Reject non-archive, non-empty output directories before root files can be replaced."""

    _validate_team_slug(team_slug)
    marker_path = archive_marker_path(archive)
    if marker_path.is_file():
        marker = as_object(read_json(marker_path), str(marker_path))
        if not is_archive_marker(marker):
            raise ValueError(f"invalid wrkviz archive marker at {marker_path}")
        # Migrate an archive written before the rename, here rather than in a separate command:
        # this runs at the start of every build, the marker is two hundred bytes, and an operator
        # should not have to know the tool was ever called anything else. Idempotent -- an archive
        # already on the current spelling takes neither branch.
        if marker_path.name == LEGACY_ARCHIVE_MARKER_FILE:
            write_json_if_changed(
                archive / ARCHIVE_MARKER_FILE,
                narrow_json({"schema_version": 1, "tool": ARCHIVE_MARKER_TOOL}),
            )
            marker_path.unlink()
            # And the former lock, which is a zero-byte file nothing opens any more. Left behind
            # it is litter in the directory an operator ships, and this is the one moment the
            # tool knows for certain that it belongs to a layout that no longer exists.
            legacy_lock = archive / LEGACY_ARCHIVE_LOCK
            if legacy_lock.is_file():
                legacy_lock.unlink()
        return
    legacy_raw = _build_root(archive, team_slug) / "raw" / "team.json"
    if legacy_raw.is_file():
        legacy_marker: dict[str, JsonValue] = {
            "schema_version": 1,
            "tool": ARCHIVE_MARKER_TOOL,
        }
        write_json_if_changed(archive / ARCHIVE_MARKER_FILE, legacy_marker)
        return
    if archive.exists():
        if not archive.is_dir():
            raise ValueError(f"archive output is not a directory: {archive}")
        if any(path.name != _ARCHIVE_LOCK for path in archive.iterdir()):
            raise ValueError(
                f"refusing non-empty non-archive output directory {archive}; "
                "choose a new directory"
            )
    if not create:
        raise ValueError(f"not a wrkviz archive: {archive}")
    new_marker: dict[str, JsonValue] = {
        "schema_version": 1,
        "tool": "wrkviz",
    }
    write_json_if_changed(marker_path, new_marker)


@dataclass(frozen=True)
class IngestReport:
    """Counts and source identity produced by one archive ingest."""

    team_slug: str
    source_digest: str
    sources: int
    source_bytes: int
    agents: int
    events: int
    tool_calls: int
    edges: int
    files_changed: int
    artifacts: int = 0
    projects: int = 0
    # Append-prefix rewrites an operator explicitly accepted *during this ingest*. Empty for every
    # provider but Orc, and empty for Orc unless the override flag was passed and fired. It rides
    # on the ingest report because that is what lands in the run receipt, and an override that
    # existed only as a line of terminal output would be unauditable the moment the terminal
    # scrolled.
    orc_prefix_overrides: tuple[OrcAppendPrefixOverride, ...] = ()
    # One-time reclamation of the retired `raw/messages/<thread-id>.json` projection, counted only
    # on the ingest that actually swept it and zero on every ingest afterwards. It is recorded in
    # the run receipt for the same reason the override above is: a several-hundred-megabyte
    # deletion inside an operator's version-controlled archive should be explicable months later
    # from the archive itself, not only from whichever terminal happened to be open.
    retired_message_projections: int = 0
    retired_message_projection_bytes: int = 0
    # Every task note this archive holds for the team, and how many of them this ingest is the
    # first to have promoted. The second number is the one worth watching: it is large exactly
    # once, on the ingest that first lifts an existing frozen projection into the model, and
    # after that it is the count of notes genuinely written upstream since the last run. A run
    # that reports a large number twice for the same team is reporting a bug -- the merge is not
    # recognizing what it already has -- which is precisely the failure this counter exists to
    # make visible in a receipt rather than in a diff nobody reads.
    task_notes: int = 0
    newly_promoted_task_notes: int = 0
    # How many of those notes upstream no longer has, so the archive is their only copy. A standing
    # inventory rather than a delta, because that is the form of the number anyone acts on: it is
    # the answer to "what would be gone if this file were", and it is the one thing here that
    # cannot be recomputed once `source_snapshots/` is deleted.
    task_notes_upstream_deleted: int = 0
    # The content-addressed tool text this archive holds for the team, and what this ingest was
    # the first to store. Reported in the same shape and for the same reason as the task-note
    # counters above: the "newly" number is large exactly once per team, on the ingest that first
    # rescues the text `_archive_team` used to delete, and afterwards it is the text produced
    # since the last run. Two large numbers in a row for one team means the content-addressed
    # merge is not recognizing what it already has, and a receipt is where that should be visible.
    tool_payloads: int = 0
    tool_payload_bytes: int = 0
    newly_stored_tool_payloads: int = 0
    newly_stored_tool_payload_bytes: int = 0
    # What the payload tree was found to be missing. Pruning it is a supported operation, so
    # neither of these is an error -- but a supported operation that leaves no trace is
    # indistinguishable from data loss six months later, and the receipt is the only place either
    # can be seen. `pruned` is shards the previous manifest recorded and this ingest did not find;
    # `damaged` is shards whose bytes disagreed with their recorded digest and were re-measured,
    # which a content-addressed union is not supposed to be able to reach at all.
    pruned_payload_shards: tuple[str, ...] = ()
    damaged_payload_shards: tuple[str, ...] = ()
    # Where this ingest put the vendor snapshots, and whether it is the run that decided. The
    # location is in the receipt because it is the one thing about a build that is now *outside*
    # the directory the operator named: a run given `--output X` that writes several gigabytes to
    # a sibling of `X` owes the record an explicit statement of where, and "read the code and
    # recompute the default" is not one. `snapshot_root_established` is true only on the run that
    # first records a layout for the archive, which is the run whose stderr says so out loud.
    #
    # Archive-relative, like the source manifest's copy and for the same two reasons. A receipt
    # under `runs/` is inside the served, version-controllable tree, and the receipts this archive
    # already writes record relative paths -- an absolute one here would be the first home
    # directory published in that file. It is also the encoding that stays true when the pair is
    # moved. The CLI resolves it against the archive before printing, because on a terminal the
    # useful form is the one you can paste into `du`.
    snapshot_root: str = ""
    snapshot_root_layout: str = ""
    snapshot_root_established: bool = False
    # What a prompt-history ingest read and deliberately left out: prompts typed in a working
    # directory the team's pattern does not select, and prompts belonging to sessions the operator
    # declared *covered* by a full-transcript team. Zero for every other provider. They are in the
    # receipt because an exclusion the operator asked for is still an exclusion, and "the history
    # has 4,226 prompts but the team shows 1,900" needs to be answerable from the archive alone.
    history_unmatched_prompts: int = 0
    history_covered_prompts: int = 0
    history_covered_sessions: int = 0

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the ingest report as a JSON-serializable object."""

        overrides: list[JsonValue] = [
            narrow_json(override.to_json_obj())
            for override in self.orc_prefix_overrides
        ]
        return {
            "team_slug": self.team_slug,
            "source_digest": self.source_digest,
            "sources": self.sources,
            "source_bytes": self.source_bytes,
            "agents": self.agents,
            "events": self.events,
            "tool_calls": self.tool_calls,
            "edges": self.edges,
            "files_changed": self.files_changed,
            "artifacts": self.artifacts,
            "projects": self.projects,
            "orc_prefix_overrides": overrides,
            "retired_message_projections": self.retired_message_projections,
            "retired_message_projection_bytes": self.retired_message_projection_bytes,
            "task_notes": self.task_notes,
            "newly_promoted_task_notes": self.newly_promoted_task_notes,
            "task_notes_upstream_deleted": self.task_notes_upstream_deleted,
            "tool_payloads": self.tool_payloads,
            "tool_payload_bytes": self.tool_payload_bytes,
            "newly_stored_tool_payloads": self.newly_stored_tool_payloads,
            "newly_stored_tool_payload_bytes": self.newly_stored_tool_payload_bytes,
            "pruned_payload_shards": list(self.pruned_payload_shards),
            "damaged_payload_shards": list(self.damaged_payload_shards),
            "snapshot_root": self.snapshot_root,
            "snapshot_root_layout": self.snapshot_root_layout,
            "snapshot_root_established": self.snapshot_root_established,
            "history_unmatched_prompts": self.history_unmatched_prompts,
            "history_covered_prompts": self.history_covered_prompts,
            "history_covered_sessions": self.history_covered_sessions,
        }


@dataclass(frozen=True)
class SummarizeReport:
    """Cache, backend, and output counts from one summarization run."""

    backend: str
    model: str
    reasoning_effort: str | None
    service_tier: str | None
    phases: int
    rollups: int
    agent_names: int
    glossary_terms: int
    project_overviews: int
    glossary_definitions: int
    catalog_artifacts: int
    cache_hits: int
    cache_misses: int
    backend_batches: int
    newly_spent_usage: TokenUsage
    newly_spent_unknown_receipts: int
    artifact_generation_usage: TokenUsage
    artifact_generation_unknown_receipts: int
    unknown_legacy_artifacts: int
    usage_run_paths: tuple[str, ...]
    files_changed: int

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the summarization report as a JSON-serializable object."""

        return {
            "backend": self.backend,
            "model": self.model,
            "reasoning_effort": self.reasoning_effort,
            "service_tier": self.service_tier,
            "phases": self.phases,
            "rollups": self.rollups,
            "plain_language_rollups": self.rollups,
            "rollup_summary_artifacts": self.rollups * 2,
            "agent_names": self.agent_names,
            "glossary_terms": self.glossary_terms,
            "project_overviews": self.project_overviews,
            "glossary_definitions": self.glossary_definitions,
            "catalog_artifacts": self.catalog_artifacts,
            "cache_hits": self.cache_hits,
            "cache_misses": self.cache_misses,
            "backend_batches": self.backend_batches,
            "newly_spent_usage": self.newly_spent_usage.to_json(),
            "newly_spent_unknown_receipts": self.newly_spent_unknown_receipts,
            "artifact_generation_usage": self.artifact_generation_usage.to_json(),
            "artifact_generation_unknown_receipts": (
                self.artifact_generation_unknown_receipts
            ),
            "unknown_legacy_artifacts": self.unknown_legacy_artifacts,
            "usage_run_paths": list(self.usage_run_paths),
            "files_changed": self.files_changed,
        }


def utc_now() -> str:
    """Return the current UTC time in ISO 8601 form."""

    return datetime.now(tz=timezone.utc).isoformat().replace("+00:00", "Z")


#: Both moved to :mod:`wrkviz.archive`, unchanged, when the snapshot store needed the
#: same durability for the pointer file it writes at the archive root. These aliases keep the
#: several dozen call sites in this module reading as they did.
_write_json_durable = write_json_durable
_write_text_durable = write_text_durable


def _archive_team(team: TeamData) -> tuple[TeamData, tuple[str, ...]]:
    """Detach the bulky tool text into content-addressed payloads; redact the source cwd.

    Returns the team as ``raw/team.json`` will hold it, plus every payload the caller must store
    before that file is durable.

    **What changed and why.** This function used to say "the original Codex JSONL remains the
    authority for command stdout and patch bodies", and it meant it: it set ``input_text`` and
    ``output_text`` to ``None`` and nothing anywhere else kept them. 0 of the 30,921 tool calls in
    one archived team retained either field. That single line is why 3.59 GB of vendor JSONL under
    gitignored ``teams/*/source_snapshots/`` cannot be deleted -- not because the archive prefers
    the vendor format, but because for those two fields the vendor file is the only copy. The text
    now goes to :mod:`wrkviz.payloads` and the tool call keeps a digest and a byte
    count, so the model still does not carry the bulk and the archive no longer loses it.

    **The two redactions are deliberate and stay.** ``working_directory`` and any credential
    embedded in ``repository_url`` are removed rather than relocated, and
    ``test_ingest_never_persists_cwd_or_repository_credentials`` pins that. They are not the same
    kind of thing as the tool text: they are *policy* losses, taken knowingly, on content whose
    value to a timeline is near zero and whose cost when leaked is not. The losslessness audit
    knows about them by name for exactly that reason -- a declared redaction and a silent drop
    read identically in a diff, and only one of them is acceptable.

    A payload is emitted for an empty string too. ``""`` and ``None`` are different observations
    about a tool call -- "it produced nothing" and "we do not know what it produced" -- and this
    is the layer that used to conflate them.
    """

    payloads: list[str] = []
    tools: list[ToolCall] = []
    for tool in team.tool_calls:
        input_ref = None
        output_ref = None
        if tool.input_text is not None:
            payloads.append(tool.input_text)
            input_ref = payload_ref(tool.input_text)
        if tool.output_text is not None:
            payloads.append(tool.output_text)
            output_ref = payload_ref(tool.output_text)
        tools.append(
            replace(
                tool,
                input_text=None,
                output_text=None,
                input_payload=input_ref,
                output_payload=output_ref,
            )
        )
    sources = tuple(
        replace(
            source,
            working_directory=None,
            repository_url=canonical_repository_url(source.repository_url),
        )
        for source in team.sources
    )
    return replace(team, sources=sources, tool_calls=tuple(tools)), tuple(payloads)


def _payload_root(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "payloads"


def rehydrate_tool_payloads(archive: Path, team: TeamData) -> TeamData:
    """Return *team* with every resolvable tool payload put back inline.

    ``load_archived_team`` deliberately does not do this. The whole reason the text lives in a
    separate tree is that it is the bulk -- 290 MB against a 55 MB ``team.json`` on one measured
    team -- and every existing caller of the loader wants the graph, not the stdout. Paying that
    on every build, every query and every summarize pass to serve the handful of callers that want
    the text would invert the reason the split exists.

    So materialization is explicit, and it is honest about a pruned tree: a reference that does not
    resolve leaves ``input_text``/``output_text`` as ``None`` and keeps the reference, so the
    caller can still see the digest and the byte count of what is missing rather than being told
    the tool produced nothing.
    """

    refs = [
        ref
        for tool in team.tool_calls
        for ref in (tool.input_payload, tool.output_payload)
        if ref is not None
    ]
    if not refs:
        return team
    resolved = resolve_payloads(_payload_root(archive, team.team_slug), refs)
    return replace(
        team,
        tool_calls=tuple(
            replace(
                tool,
                input_text=(
                    resolved.get(tool.input_payload.sha256)
                    if tool.input_payload is not None
                    else tool.input_text
                ),
                output_text=(
                    resolved.get(tool.output_payload.sha256)
                    if tool.output_payload is not None
                    else tool.output_text
                ),
            )
            for tool in team.tool_calls
        ),
    )


def _raw_team_path(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "team.json"


def _artifact_catalog_path(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "artifacts.json"


def _summary_root(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return archive / "teams" / team_slug / "summary_data"


def _summary_projection_missing(path: Path) -> bool:
    """Return true only for an absent optional projection, not a malformed path."""

    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        return True
    if not stat.S_ISREG(mode):
        raise ValueError(f"{path}: summary projection is not a regular file")
    return False


def _source_snapshot_location(
    archive: Path, team_slug: str, requested: Path | None
) -> SnapshotLocation:
    """Decide where this team's vendor snapshots live, and make sure the directory exists.

    The one place in ingestion that knows about the location at all. Everything downstream --
    every provider snapshotter, every loader, every digest recorded in the source manifest -- is
    addressed relative to the root this returns, which is the property that made moving 6.5 GB out
    of the published tree a change to one function rather than to three providers.

    Resolution never moves anything; see :mod:`wrkviz.snapshot_store` for the four
    branches and the two refusals. What this adds is the side effect resolution deliberately does
    not have: creating the store, its marker, and the ``.gitignore`` that keeps a multi-gigabyte
    tree out of somebody's ``git status``.
    """

    location = resolve_snapshot_root(archive, team_slug, requested)
    ensure_snapshot_store(location)
    return location


def snapshot_root_for(archive: Path, team_slug: str) -> Path:
    """Return one team's snapshot root without creating anything.

    Public for the read-only callers -- the losslessness audit and the garbage collector -- which
    need the same answer ingestion gets and must not bring a store into existence to get it.
    """

    return resolve_snapshot_root(archive, team_slug).root


def _source_manifest_path(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "source-manifest.json"


def _normalized_generation_path(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "normalized-generation.json"


def _task_notes_path(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "task-notes.jsonl"


def _load_promoted_task_notes(archive: Path, team_slug: str) -> tuple[TaskNote, ...]:
    """Read the task notes this archive already holds, as of before the current ingest.

    Strictly ordered on read, not merely deduplicated. The file is an accumulator that only ever
    grows, and the one failure mode that would quietly lose content is a writer that emitted the
    same key twice with different bodies -- a reader that took "the last one wins" would make
    that unobservable. Requiring the order the writer promises turns it into a refusal instead.
    """

    path = _task_notes_path(archive, team_slug)
    notes: list[TaskNote] = []
    previous_key: tuple[str, int] | None = None
    for index, record in enumerate(read_jsonl(path)):
        note = task_note_from_json_obj(record, f"{path}:{index + 1}")
        key = task_note_key(note)
        if previous_key is not None and key <= previous_key:
            raise ValueError(
                f"{path}:{index + 1}: promoted task notes are not strictly ordered by "
                "(source_path, note_id)"
            )
        previous_key = key
        notes.append(note)
    return tuple(notes)


def _merge_promoted_task_notes(
    promoted: Sequence[TaskNote], observed: Sequence[TaskNote], where: str
) -> tuple[tuple[TaskNote, ...], int]:
    """Union the already-promoted notes with this ingest's, first promotion winning.

    Three properties, and each one is load-bearing.

    **Union, not replacement.** This is the entire reason the promotion exists. Orc's ``task_notes``
    table is mutable upstream and rows are genuinely deleted from it: on the archive that prompted
    this, 74 of the 4,583 notes in one projection had no counterpart left in the live table, among
    them a "POST-LAND AUTHORITY DRIFT" report and the note recording that an owner authorized
    landing a pull request over stale dependency edges. Anything that recomputed this file from
    what is currently observable would delete them. So an observed set that has *lost* a note
    leaves the promoted record exactly where it is; nothing in this function can shrink the file.

    **First promotion wins, with exactly one exception.** A note already here keeps its stored
    body and its stored provenance even when today's projection presents it differently. That is
    the same freeze-first rule ``orc._advance_task_candidate`` applies to the projection itself,
    and applying it again here is what makes the file byte-stable: without it, one appended note
    would restamp the provenance of every other record with the new projection generation.

    The exception is ``upstream_present``, which latches from true to false and never back. It is
    not provenance about where the bytes came from; it is the archive's record of *when it became
    the last copy*, and freezing it at first promotion would have answered that question only for
    notes upstream had already deleted before this archive first ran -- the one part of the
    population that cannot grow. Orc refuses to reuse a note id below its frozen high-water mark,
    so a deleted note cannot come back and the latch cannot be wrong in the other direction. One
    such transition rewrites one line of the file, which is a real change and should be visible as
    one.

    **Divergence in the immutable core is a refusal, not a merge.** ``task_id``, ``content`` and
    ``created_at`` are the fields Orc never legitimately edits, and the snapshot layer already
    refuses when they change. If they differ here, the two sides disagree about what a note *is*,
    and picking either one silently would be choosing which copy of the historical record to
    believe on the operator's behalf. Enrichment -- title, owner, author -- is not in that set,
    because it demonstrably does change upstream and the frozen copy is deliberately the older
    one.
    """

    merged = {task_note_key(note): note for note in promoted}
    newly_promoted = 0
    for note in observed:
        key = task_note_key(note)
        prior = merged.get(key)
        if prior is None:
            merged[key] = note
            newly_promoted += 1
            continue
        if (prior.task_id, prior.content, prior.created_at) != (
            note.task_id,
            note.content,
            note.created_at,
        ):
            raise ValueError(
                f"{where}: promoted task note {prior.source_path}#{prior.note_id} "
                "disagrees with the current source about its immutable core"
            )
        if prior.upstream_present and not note.upstream_present:
            merged[key] = replace(prior, upstream_present=False)
    return tuple(sorted(merged.values(), key=task_note_key)), newly_promoted


def _retired_message_projection_root(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "messages"


def _retire_message_projections(archive: Path, team_slug: str) -> tuple[int, int]:
    """Remove the retired per-thread projection of ``raw/team.json``; report files and bytes.

    ``teams/<slug>/raw/messages/<thread-id>.json`` was written by every ingest from this tool's
    first commit and read by nothing, ever. There is no reader in this package, in its tests, in
    the browser bundle, in the standalone ``timeline`` CLI, ``serve.py`` or ``run_stats.py``
    shipped into each archive, nor in any revision of any of those: the only path join naming that
    directory in the entire repository history was the writer. Each file was a pure per-thread
    partition of ``raw/team.json`` -- the same agent, turn, event, tool-call and edge records,
    re-serialized once per thread -- so it carried no information the archive does not still hold
    and can be recomputed from ``raw/team.json`` at any time. On the twelve-team archive that
    prompted this it was 675,371,783 bytes across 2,932 files -- 50.9% of everything under
    ``teams/`` that is not a gitignored source snapshot, so it roughly doubled the size of the
    version-controlled archive -- and every ingest rewrote all of it.

    **Why remove it rather than leave it.** An archive is meant to be version-controlled, and
    ``raw/messages/`` is not in the generated ``.gitignore``, so every one of those files is
    tracked. A tracked file the tool has stopped maintaining is worse than an absent one: it keeps
    being cloned, it keeps being diffed, and it reads as current when it is frozen. That is the
    same judgement ``render.prune_retired_query_artifacts`` already made for the retired generated
    ``query.py`` launcher, and this reuses its discipline rather than inventing a second one.

    **Why at ingest rather than at build.** ``raw/`` is the ingest namespace. The build sweepers --
    ``render._safe_presentation_file`` and ``multi_team._safe_generated_path`` -- cannot even name
    a path under ``raw/``; they raise, and a test pins that refusal precisely so a presentation bug
    can never delete ingest data. Retiring from build would mean punching a hole in the guard whose
    whole job is to protect this directory. Ingest already creates, writes and fsyncs it, so the
    retirement belongs exactly where the writer was. A separate ``gc`` command was the other
    option and was rejected: it is a second mechanism for something the repository already has an
    idiom for, and it reclaims nothing for an operator who never learns it exists.

    **Why this is safe for an old archive read by an old reader.** No reader ever existed, in any
    version, so there is no older reader to break; the content remains derivable from
    ``raw/team.json``, which is written and fsynced immediately before this runs and is never
    removed; and in a versioned archive the deleted bytes stay in history.

    **What the sweep deletes, stated exactly.** Regular files directly inside the directory whose
    names have the *shape* the writer used -- ``<archive-id>.json``, the same identifier grammar
    ``_validate_archive_id`` enforces on a thread id -- and nothing else. It deliberately does not
    narrow that to the thread ids of the team being ingested right now, which are in hand at the
    only call site. The old writer never deleted anything it had written, so a lineage re-ingested
    under a narrower ``--team`` selection or date window left projections behind for threads the
    current ``raw/team.json`` no longer mentions. Those orphans are the oldest and least
    recoverable bytes in the directory, an exact-name sweep would strand them permanently, and
    because they would then keep the directory non-empty forever it could never be removed either
    -- so the narrower rule fails at the one job this function has. The price is real and is not
    hidden: a file of your own directly inside this directory named ``<something>.json`` is swept
    with the rest. That is the bargain every other path under ``raw/`` already makes -- ``raw/`` is
    the ingest namespace, not a place to keep notes -- and it is the one respect in which this
    sweep is broader than ``render.prune_retired_query_artifacts``, which can afford byte equality
    because its retired artifact had exactly one possible content. A per-thread projection does
    not: its bytes depend on the thread, so there is no fixed content to compare against.

    A symlink, a subdirectory, a ``.gz`` sidecar and any name outside the grammar are left
    untouched, and any one of them left behind leaves the directory itself in place.

    The sweep needs no durability barrier of its own, unlike the writes around it: it is
    idempotent, so a crash that loses the unlinks costs one repeat on the next ingest.
    """

    root = _retired_message_projection_root(archive, team_slug)
    if root.is_symlink() or not root.is_dir():
        return 0, 0
    files = 0
    freed = 0
    foreign = False
    for entry in sorted(root.iterdir()):
        name = entry.name
        stem = name.removesuffix(".json")
        # `_ARCHIVE_ID` is the identifier grammar, not the current team's thread ids: see the
        # docstring for why matching the writer's name *shape* is deliberate and what it costs.
        if (
            name == stem
            or entry.is_symlink()
            or not entry.is_file()
            or _ARCHIVE_ID.fullmatch(stem) is None
        ):
            foreign = True
            continue
        freed += entry.stat().st_size
        entry.unlink()
        files += 1
    if not foreign:
        root.rmdir()
    return files, freed


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                return digest.hexdigest()
            digest.update(chunk)


def _canonical_json_file_sha256(path: Path) -> str:
    encoded = canonical_json(read_json(path)).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _site_identity_path(archive: Path, team_slug: str) -> Path:
    _validate_team_slug(team_slug)
    return _build_root(archive, team_slug) / "raw" / "site-identity.json"


def load_site_identity(
    archive: Path, team: TeamData, *, required: bool = False
) -> SiteIdentity:
    """Load standalone site identity, with a team-data fallback when absent."""

    path = _site_identity_path(archive, team.team_slug)
    if not path.is_file():
        if required:
            raise ValueError(f"missing site identity at {path}")
        return SiteIdentity(
            team_slug=team.team_slug,
            projects=(),
            hosts=(),
            display_timezone=team.display_timezone,
            display_timezone_source="legacy_team_data",
        )
    identity = site_identity_from_json_obj(read_json(path), str(path))
    if identity.team_slug != team.team_slug:
        raise ValueError(
            f"site identity team {identity.team_slug!r} does not match {team.team_slug!r}"
        )
    if identity.display_timezone != team.display_timezone:
        raise ValueError(
            "site identity display timezone differs from normalized team data; rerun ingest"
        )
    return identity


def _record_site_identity(
    archive: Path,
    team: TeamData,
    inferred: tuple[tuple[ProjectIdentity, ...], tuple[HostIdentity, ...]],
    overrides: IdentityOverrides | None,
) -> int:
    """Merge durable identity evidence and atomically update its standalone record."""

    path = _site_identity_path(archive, team.team_slug)
    previous = (
        site_identity_from_json_obj(read_json(path), str(path))
        if path.is_file()
        else None
    )
    if previous is not None and previous.team_slug != team.team_slug:
        raise ValueError(
            f"site identity team {previous.team_slug!r} does not match {team.team_slug!r}"
        )
    selected = overrides or IdentityOverrides()
    identity = merge_site_identity(
        team.team_slug,
        team.display_timezone,
        selected.display_timezone_source,
        inferred[0],
        inferred[1],
        selected.projects,
        selected.hosts,
        previous,
    )
    return int(_write_json_durable(path, narrow_json(identity.to_json_obj())))


def _ensure_bulk_content_ignored(archive: Path) -> bool:
    """Keep the two trees that hold raw bulk out of version control, and nothing else.

    ``payloads/`` joins ``source_snapshots/`` here rather than living under tracked ``raw/``, and
    that placement is the whole reason it is safe for the archive to start keeping command stdout
    and patch bodies at all. Those bytes are the ones most likely to contain an absolute path
    under someone's home directory or a token that leaked into a log line -- the archive has a
    test, ``test_ingest_never_persists_cwd_or_repository_credentials``, whose entire subject is
    that tracked files must not carry that kind of content. Putting the payload tree beside the
    snapshots preserves that promise exactly: what is tracked is the model and the digests, what
    is ignored is the bulk. An operator who wants the bulk versioned or replicated can say so
    themselves; the default cannot make that decision for them.

    ``/teams/*/source_snapshots/`` stays here even though a new archive no longer puts anything
    there. It is the rule that keeps an *unmigrated* archive working exactly as it did, and it
    costs one line. The relocated store cannot be covered from this file at all -- it is outside
    the archive -- so it ignores itself instead; see
    :data:`wrkviz.snapshot_store.STORE_GITIGNORE_FILE`.
    """

    path = archive / ".gitignore"
    required = (
        f"/{_ARCHIVE_LOCK}",
        "/teams/*/source_snapshots/",
        "/teams/*/payloads/",
        # The `gc` trash. Ignored for the same reason the lock is -- it is local recovery state,
        # not content -- and ignored *before* it can exist, so that an operator who runs `gc
        # --delete` on a tracked archive does not find a quarter-gigabyte of deleted files
        # proposed as an addition to their next commit.
        f"/{ARCHIVE_TRASH_ROOT}/",
    )
    if path.exists() and not path.is_file():
        raise ValueError(f"archive .gitignore is not a file: {path}")
    existing = path.read_text(encoding="utf-8") if path.is_file() else ""
    existing_lines = set(existing.splitlines())
    missing = [line for line in required if line not in existing_lines]
    if not missing:
        return False
    prefix = existing
    if prefix and not prefix.endswith("\n"):
        prefix += "\n"
    return write_text_if_changed(path, prefix + "\n".join(missing) + "\n")


def _manifest_window_value(date_window: DateWindow | None) -> JsonValue:
    if date_window is None:
        return None
    return narrow_json(date_window.to_json_obj())


def _manifest_window_bounds(
    value: JsonValue, where: str
) -> tuple[int | None, int | None]:
    if value is None:
        return None, None
    obj = as_object(value, where)
    base_fields = {"start_date", "end_date", "start_ms", "end_ms"}
    exact_fields = base_fields | {"start_time", "end_time"}
    if set(obj) not in (base_fields, exact_fields):
        raise ValueError(f"{where}: invalid date-window fields")
    start_value = obj.get("start_ms")
    end_value = obj.get("end_ms")
    start_ms = (
        None
        if start_value is None
        else as_int(start_value, f"{where}.start_ms")
    )
    end_ms = (
        None if end_value is None else as_int(end_value, f"{where}.end_ms")
    )
    return start_ms, end_ms


def _window_is_same_or_narrower(
    recorded: JsonValue,
    requested: JsonValue,
    where: str,
) -> bool:
    """Return whether a new half-open ingest window is a subset of the old one."""

    recorded_start, recorded_end = _manifest_window_bounds(
        recorded, where + ".recorded"
    )
    requested_start, requested_end = _manifest_window_bounds(
        requested, where + ".requested"
    )
    start_is_narrower = (
        recorded_start is None
        or (
            requested_start is not None
            and requested_start >= recorded_start
        )
    )
    end_is_narrower = (
        recorded_end is None
        or (
            requested_end is not None
            and requested_end <= recorded_end
        )
    )
    return start_is_narrower and end_is_narrower


def _validate_manifest_window(
    recorded: JsonValue,
    date_window: DateWindow | None,
    where: str,
    error_type: type[ValueError],
) -> None:
    requested = _manifest_window_value(date_window)
    if recorded == requested:
        return
    try:
        if _window_is_same_or_narrower(recorded, requested, where):
            return
    except ValueError as error:
        raise error_type(f"invalid archived date window: {error}") from error
    raise error_type(
        "archive date window may only stay unchanged or become narrower; "
        "choose a new output directory to widen it"
    )


@dataclass(frozen=True)
class _CodexManifestState:
    sources: tuple[CodexSourceCopy, ...]
    continuation_links: tuple[CodexContinuationLink, ...]
    continuation_thread_ids: tuple[str, ...]


def _load_source_manifest(
    archive: Path,
    team_slug: str,
    root_thread_id: str,
    date_window: DateWindow | None,
    requested_continuation_thread_ids: Sequence[str] = (),
) -> _CodexManifestState:
    path = _source_manifest_path(archive, team_slug)
    if not path.is_file():
        return _CodexManifestState(
            (), (), tuple(requested_continuation_thread_ids)
        )
    obj = as_object(read_json(path), str(path))
    if obj.get("schema_version") != 1 or obj.get("provider") != "codex":
        raise CodexParseError(f"invalid Codex source manifest at {path}")
    recorded_root = as_string(obj.get("root_thread_id"), f"{path}: root_thread_id")
    if recorded_root != root_thread_id:
        raise CodexParseError(
            f"source manifest belongs to root {recorded_root!r}, not {root_thread_id!r}"
        )
    _validate_manifest_window(
        obj.get("date_window"), date_window, str(path), CodexParseError
    )
    raw_sources = as_array(obj.get("sources"), f"{path}: sources")
    result: list[CodexSourceCopy] = []
    for index, raw_source in enumerate(raw_sources):
        source = as_object(raw_source, f"{path}: sources[{index}]")
        result.append(CodexSourceCopy.from_json_obj(source, f"{path}: sources[{index}]"))
    raw_continuations = obj.get("continuation_sessions")
    links: list[CodexContinuationLink] = []
    if raw_continuations is not None:
        for index, raw_link in enumerate(
            as_array(raw_continuations, f"{path}: continuation_sessions")
        ):
            link = as_object(
                raw_link, f"{path}: continuation_sessions[{index}]"
            )
            links.append(
                CodexContinuationLink.from_json_obj(
                    link, f"{path}: continuation_sessions[{index}]"
                )
            )
    recorded_ids = tuple(link.thread_id for link in links)
    requested_ids = tuple(requested_continuation_thread_ids)
    if requested_ids:
        if recorded_ids != requested_ids[: len(recorded_ids)]:
            raise CodexParseError(
                "requested continuation sessions do not extend the recorded ordered prefix"
            )
        effective_ids = requested_ids
    else:
        effective_ids = recorded_ids
    return _CodexManifestState(tuple(result), tuple(links), effective_ids)


def _load_claude_source_manifest(
    archive: Path,
    team_slug: str,
    root_thread_id: str,
    date_window: DateWindow | None,
) -> tuple[ClaudeSourceCopy, ...]:
    path = _source_manifest_path(archive, team_slug)
    if not path.is_file():
        return ()
    obj = as_object(read_json(path), str(path))
    if obj.get("schema_version") != 1 or obj.get("provider") != "claude":
        raise ClaudeParseError(f"invalid Claude source manifest at {path}")
    recorded_root = as_string(obj.get("root_thread_id"), f"{path}: root_thread_id")
    if recorded_root != root_thread_id:
        raise ClaudeParseError(
            f"source manifest belongs to root {recorded_root!r}, not {root_thread_id!r}"
        )
    _validate_manifest_window(
        obj.get("date_window"), date_window, str(path), ClaudeParseError
    )
    raw_sources = as_array(obj.get("sources"), f"{path}: sources")
    result: list[ClaudeSourceCopy] = []
    for index, raw_source in enumerate(raw_sources):
        source = as_object(raw_source, f"{path}: sources[{index}]")
        result.append(
            ClaudeSourceCopy.from_json_obj(source, f"{path}: sources[{index}]")
        )
    return tuple(result)


def _write_ingested_team(
    archive: Path,
    team_slug: str,
    team: TeamData,
    date_window: DateWindow | None,
    files_changed: int,
    location: SnapshotLocation,
) -> tuple[TeamData, IngestReport]:
    """Write provider-neutral normalized data after a provider snapshot is durable."""

    changed = files_changed
    # The payload tree is written first, and everything the marker commits follows it. Two
    # orderings had to be reconciled here.
    #
    # `team.json` is about to stop carrying the tool text and start carrying a digest that points
    # at it, so the text has to be durable before the pointer is: an archive holding the pointer
    # and not the text claims content it does not have. The reverse -- payloads durable, pointers
    # not yet written -- costs nothing, because a payload nobody references is inert and the next
    # ingest merges it back into the same place.
    #
    # It also has to come before `raw/task-notes.jsonl`, and that is a durability argument rather
    # than a content one. This merge is the fallible step in the sequence; the writes after it are
    # not. Everything below is bound by the Orc generation marker, which is written by the caller,
    # so an exception in the middle leaves a marker that no longer describes the files -- and
    # `load_archived_team` refuses the team until an ingest completes. That state is recoverable
    # only if the *next* ingest can get past the same step, so the step most likely to raise
    # belongs before the first durable write, not between two of them.
    archived, detached_payloads = _archive_team(team)
    payload_report = merge_payloads(_payload_root(archive, team_slug), detached_payloads)
    changed += payload_report.files_changed
    # Then the task notes, before anything derived from them, because this file -- not the
    # provider snapshot, and not the frozen projection inside it -- is now the archive's copy of
    # that text. `teams/*/source_snapshots/` is gitignored, so until this existed the only
    # version-controlled trace of a note was the message it renders into inside `raw/team.json`,
    # and that rendering is lossy and window-filtered. See `_merge_promoted_task_notes` for why
    # the union, rather than a recomputation, is the entire point.
    notes_path = _task_notes_path(archive, team_slug)
    promoted, newly_promoted = _merge_promoted_task_notes(
        _load_promoted_task_notes(archive, team_slug), team.task_notes, str(notes_path)
    )
    if promoted:
        changed += int(
            _write_text_durable(
                notes_path,
                canonical_jsonl(
                    as_object(narrow_json(note.to_json_obj()), "task note")
                    for note in promoted
                ),
            )
        )
    artifact_catalog = extract_artifacts(team)
    changed += int(
        _write_json_durable(
            _artifact_catalog_path(archive, team_slug),
            narrow_json(artifact_catalog.to_json_obj()),
        )
    )
    # These identifiers still reach the filesystem even though the per-thread projection that first
    # motivated the check is gone: `summary_data/agents/<thread-id>.json` and
    # `summaries/agents/<thread-id>.md` are both built by interpolating `agent.thread_id`
    # directly, with no guard of their own. Do not remove this loop with the writer below it.
    _validate_archive_id(archived.root_thread_id, "root thread id")
    for agent in archived.agents:
        _validate_archive_id(agent.thread_id, "thread id")
    changed += int(
        _write_json_durable(
            _raw_team_path(archive, team_slug),
            # Written without the task notes, which `raw/task-notes.jsonl` above now holds. They
            # are already in this file once, as the message text of the events they render into,
            # and a second verbatim copy would add roughly 70 MB to an archive whose largest
            # `team.json` is already 220 MB -- for content the loader reattaches from a file it
            # has to read anyway. `load_archived_team` puts them back, so no consumer of the
            # model can tell the difference; only the bytes on disk can.
            narrow_json(replace(archived, task_notes=()).to_json_obj()),
        )
    )
    # Sweep the retired per-thread projection only once the file it was a projection *of* is
    # durable, so no instant exists in which the archive holds neither.
    retired_files, retired_bytes = _retire_message_projections(archive, team_slug)
    changed += retired_files
    digest = source_digest(team)
    snapshot: dict[str, object] = {
        "provider": team.provider,
        "root_thread_id": team.root_thread_id,
        "team_slug": team.team_slug,
        "display_timezone": team.display_timezone,
        "date_window": date_window.to_json_obj() if date_window is not None else None,
        "source_digest": digest,
        "sources": [source.to_json_obj() for source in archived.sources],
    }
    changed += int(
        _write_json_durable(
            _build_root(archive, team_slug) / "raw" / "source-snapshot.json",
            narrow_json(snapshot),
        )
    )
    # Recorded last, after everything it describes is durable, and recorded even when it merely
    # restates the default. An archive that only *derived* its layout would answer "where are my
    # snapshots?" by recomputing today's default -- so the day the default changes, or the day an
    # operator passes `--snapshot-root` once and forgets it the next time, the archive would
    # quietly start a second tree beside the first. Neither tree is invalid on its own, which is
    # exactly why the filesystem cannot report the mistake and the archive has to.
    pointer_established = read_pointer(archive) is None
    changed += int(write_pointer(archive, location))
    report = IngestReport(
        team_slug=team_slug,
        source_digest=digest,
        sources=len(team.sources),
        source_bytes=sum(source.complete_bytes for source in team.sources),
        agents=len(team.agents),
        events=len(team.events),
        tool_calls=len(team.tool_calls),
        edges=len(team.edges),
        files_changed=changed,
        artifacts=len(artifact_catalog.artifacts),
        projects=len(artifact_catalog.projects),
        retired_message_projections=retired_files,
        retired_message_projection_bytes=retired_bytes,
        task_notes=len(promoted),
        newly_promoted_task_notes=newly_promoted,
        task_notes_upstream_deleted=sum(
            1 for note in promoted if not note.upstream_present
        ),
        tool_payloads=payload_report.stored,
        tool_payload_bytes=payload_report.stored_bytes,
        newly_stored_tool_payloads=payload_report.newly_stored,
        newly_stored_tool_payload_bytes=payload_report.newly_stored_bytes,
        pruned_payload_shards=payload_report.pruned_shards,
        damaged_payload_shards=payload_report.damaged_shards,
        snapshot_root=location.archive_relative,
        snapshot_root_layout=location.layout,
        snapshot_root_established=pointer_established,
    )
    return replace(archived, task_notes=promoted), report


def _normalized_generation_value(
    archive: Path, team_slug: str, team: TeamData
) -> dict[str, JsonValue]:
    """Describe the complete normalized Orc generation committed by the marker."""

    notes_path = _task_notes_path(archive, team_slug)
    return {
        "schema_version": 1,
        "tool": "wrkviz",
        "normalizer_schema_version": _ORC_NORMALIZER_SCHEMA_VERSION,
        "provider": "orc",
        "source_manifest_sha256": _canonical_json_file_sha256(
            _source_manifest_path(archive, team_slug)
        ),
        "team_sha256": _file_sha256(_raw_team_path(archive, team_slug)),
        "artifact_catalog_sha256": _file_sha256(
            _artifact_catalog_path(archive, team_slug)
        ),
        # An Orc team with no task source never writes the file at all, and hashes as the empty
        # one it would have been -- so "no notes" and "an empty accumulator" are the same
        # generation, and a torn or truncated write of a file that *is* the last copy of its
        # content is caught by the marker exactly like a torn `team.json`.
        "task_notes_sha256": (
            _file_sha256(notes_path)
            if notes_path.is_file()
            else hashlib.sha256(b"").hexdigest()
        ),
        # The payload tree is deliberately NOT bound here, and the reason is worth stating because
        # it looks like an omission. Everything this marker covers is something that can go stale:
        # `team.json`, the artifact catalog and the task notes are all derived, and a mixed
        # generation of them is a real and silent failure. A payload cannot be stale. Its name is
        # its content, references to it either resolve or do not, and the store is a union that
        # never rewrites a record -- so there is no state in which the tree is *wrong* about what
        # `team.json` points at, only states in which it is incomplete. Incompleteness is exactly
        # what the tree is meant to permit: it is gitignored bulk that an operator is invited to
        # prune, permission or move to cold storage. Binding it here would turn every one of those
        # into a refusal on the next build and force a re-ingest that recreated the very bytes the
        # operator had just removed. `payloads.verify_payload_store`, run by the losslessness
        # audit, is what checks the tree, and it reports a pruned shard as the absence it is.
        "source_digest": source_digest(team),
    }


def _validate_normalized_generation(
    archive: Path, team_slug: str, team: TeamData
) -> None:
    marker_path = _normalized_generation_path(archive, team_slug)
    if marker_path.is_symlink() or not marker_path.is_file():
        raise ValueError(
            f"incomplete Orc normalized generation for {team_slug!r}; rerun ingest"
        )
    marker = as_object(read_json(marker_path), str(marker_path))
    # Each accepted generation is the previous one plus exactly the digests a later change added,
    # so the list reads as the history it is. An older marker describes an archive that genuinely
    # had no task-note file and no payload tree, and it describes that archive completely and
    # correctly; refusing it would force a full re-ingest of every existing Orc team before any of
    # them could be built again -- several hours of penalty for additions that changed none of the
    # bytes the old marker vouches for. The next ingest writes the current field set and the
    # tolerance stops applying to that team, which is the staged-migration shape
    # `OrcTaskProjection.from_json_obj` already uses for its own historical field sets.
    base_fields = {
        "schema_version",
        "tool",
        "normalizer_schema_version",
        "provider",
        "source_manifest_sha256",
        "team_sha256",
        "artifact_catalog_sha256",
        "source_digest",
    }
    accepted_field_sets = (
        base_fields,
        base_fields | {"task_notes_sha256"},
    )
    if set(marker) not in accepted_field_sets:
        raise ValueError(
            f"invalid Orc normalized generation marker at {marker_path}"
        )
    expected = {
        key: value
        for key, value in _normalized_generation_value(archive, team_slug, team).items()
        if key in set(marker)
    }
    # The `tool` field is compared LENIENTLY, and only that field. It records which tool wrote the
    # generation, not anything about the bytes, and it changed when the tool was renamed -- so a
    # strict comparison would declare every previously-ingested Orc team stale and demand a full
    # re-ingest of each, to reach a marker identical in every field that describes the data. That
    # is a large, silent cost for a spelling change, and the refusal would say "rerun ingest",
    # which points at the data rather than at the rename.
    #
    # Every other field stays exact: these are digests of the normalized team, its manifest, its
    # catalogue and its notes, and each is the whole point of the marker.
    comparable = {key: value for key, value in marker.items() if key != "tool"}
    comparable_expected = {key: value for key, value in expected.items() if key != "tool"}
    tool_recognised = marker.get("tool") in (
        ARCHIVE_MARKER_TOOL,
        LEGACY_ARCHIVE_MARKER_TOOL,
    )
    if comparable != comparable_expected or not tool_recognised:
        raise ValueError(
            f"stale or incomplete Orc normalized generation for {team_slug!r}; "
            "rerun ingest"
        )


def _ingest_codex_locked(
    archive: Path,
    sessions_root: Path,
    root_thread_id: str,
    team_slug: str,
    display_timezone: str,
    date_window: DateWindow | None,
    identity_overrides: IdentityOverrides | None,
    continuation_thread_ids: Sequence[str],
    requested_snapshot_root: Path | None,
) -> tuple[TeamData, IngestReport]:
    """Normalize one complete Codex lineage and write canonical raw JSON."""

    _ensure_archive(archive, team_slug, create=True)
    changed = int(_ensure_bulk_content_ignored(archive))
    manifest_state = _load_source_manifest(
        archive,
        team_slug,
        root_thread_id,
        date_window,
        continuation_thread_ids,
    )
    location = _source_snapshot_location(archive, team_slug, requested_snapshot_root)
    snapshot_root = location.root
    source_copies = (
        snapshot_codex_lineage(
            sessions_root,
            root_thread_id,
            snapshot_root,
            manifest_state.sources,
            utc_now(),
            manifest_state.continuation_thread_ids,
            manifest_state.continuation_links,
        )
        if manifest_state.continuation_thread_ids
        else snapshot_codex_lineage(
            sessions_root,
            root_thread_id,
            snapshot_root,
            manifest_state.sources,
            utc_now(),
        )
    )
    changed += source_copies.files_changed
    # Parsing deliberately starts only after the original logs have been closed. Everything from
    # this point onward consumes the archive-local, newline-complete backup.
    allowed_sources = tuple(source.snapshot_path for source in source_copies.sources)
    team = apply_date_window(
        load_codex_team(
            snapshot_root,
            root_thread_id,
            team_slug,
            display_timezone,
            source_paths=allowed_sources,
            continuation_links=source_copies.continuations,
        ),
        date_window,
    )
    if tuple(sorted(source.path for source in team.sources)) != tuple(sorted(allowed_sources)):
        raise CodexParseError(
            "parsed source set differs from the just-validated source snapshot manifest"
        )
    changed += _record_site_identity(
        archive,
        team,
        infer_structured_identity(
            codex_identity_metadata(
                snapshot_root,
                allowed_sources,
                root_thread_id,
                manifest_state.continuation_thread_ids,
            )
        ),
        identity_overrides,
    )
    source_manifest: dict[str, object] = {
        "schema_version": 1,
        "provider": "codex",
        "root_thread_id": root_thread_id,
        "source_root": str(sessions_root.resolve()),
        "snapshot_root": location.archive_relative,
        "date_window": date_window.to_json_obj() if date_window is not None else None,
        "sources": [source.to_json_obj() for source in source_copies.sources],
    }
    if source_copies.continuations:
        source_manifest["continuation_sessions"] = [
            link.to_json_obj() for link in source_copies.continuations
        ]
    changed += int(
        _write_json_durable(
            _source_manifest_path(archive, team_slug), narrow_json(source_manifest)
        )
    )
    return _write_ingested_team(
        archive, team_slug, team, date_window, changed, location
    )


def ingest_codex(
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
    """Snapshot and normalize one Codex lineage as one serialized raw-data transaction.

    ``snapshot_root`` names the *store*, not this team's directory inside it -- the team is one
    slug-named subdirectory, so one setting serves an archive with twelve teams. Omitted, the
    archive's recorded layout decides, and a brand-new archive gets ``<archive>.sources``.
    """

    with archive_writer_lock(archive):
        return _ingest_codex_locked(
            archive,
            sessions_root,
            root_thread_id,
            team_slug,
            display_timezone,
            date_window,
            identity_overrides,
            continuation_thread_ids,
            snapshot_root,
        )


def _ingest_claude_locked(
    archive: Path,
    session_file: Path,
    team_slug: str,
    display_timezone: str,
    date_window: DateWindow | None,
    identity_overrides: IdentityOverrides | None,
    requested_snapshot_root: Path | None,
) -> tuple[TeamData, IngestReport]:
    """Snapshot and normalize one Claude coordinator lineage."""

    root_thread_id = session_file.stem
    _ensure_archive(archive, team_slug, create=True)
    changed = int(_ensure_bulk_content_ignored(archive))
    previous_sources = _load_claude_source_manifest(
        archive, team_slug, root_thread_id, date_window
    )
    location = _source_snapshot_location(archive, team_slug, requested_snapshot_root)
    snapshot_root = location.root
    source_copies = snapshot_claude_lineage(
        session_file,
        snapshot_root,
        previous_sources,
        utc_now(),
    )
    changed += source_copies.files_changed
    allowed_sources = tuple(source.snapshot_path for source in source_copies.sources)
    snapshot_session = snapshot_root / session_file.name
    team = apply_date_window(
        load_claude_team(
            snapshot_session,
            team_slug,
            display_timezone,
            source_paths=allowed_sources,
        ),
        date_window,
    )
    if tuple(sorted(source.path for source in team.sources)) != tuple(
        sorted(allowed_sources)
    ):
        raise ClaudeParseError(
            "parsed source set differs from the just-validated source snapshot manifest"
        )
    changed += _record_site_identity(
        archive, team, ((), ()), identity_overrides
    )
    source_manifest: dict[str, object] = {
        "schema_version": 1,
        "provider": "claude",
        "root_thread_id": root_thread_id,
        "source_root": str(session_file.parent.resolve()),
        "root_session_file": str(session_file.resolve()),
        "snapshot_root": location.archive_relative,
        "date_window": date_window.to_json_obj() if date_window is not None else None,
        "sources": [source.to_json_obj() for source in source_copies.sources],
    }
    changed += int(
        _write_json_durable(
            _source_manifest_path(archive, team_slug), narrow_json(source_manifest)
        )
    )
    return _write_ingested_team(
        archive, team_slug, team, date_window, changed, location
    )


def ingest_claude(
    archive: Path,
    session_file: Path,
    team_slug: str,
    display_timezone: str,
    date_window: DateWindow | None = None,
    identity_overrides: IdentityOverrides | None = None,
    snapshot_root: Path | None = None,
) -> tuple[TeamData, IngestReport]:
    """Snapshot and normalize one Claude lineage as one serialized transaction."""

    with archive_writer_lock(archive):
        return _ingest_claude_locked(
            archive,
            session_file,
            team_slug,
            display_timezone,
            date_window,
            identity_overrides,
            snapshot_root,
        )


def _load_claude_history_source_manifest(
    archive: Path, team_slug: str, project_pattern: str
) -> ClaudeSourceCopy | None:
    path = _source_manifest_path(archive, team_slug)
    if not path.is_file():
        return None
    obj = as_object(read_json(path), str(path))
    if obj.get("schema_version") != 1 or obj.get("provider") != CLAUDE_HISTORY_PROVIDER:
        raise ClaudeHistoryParseError(f"invalid Claude history source manifest at {path}")
    recorded_pattern = as_string(obj.get("project_pattern"), f"{path}: project_pattern")
    if recorded_pattern != project_pattern:
        # The pattern is the team's identity: it decides which prompts the team *is*. A team
        # re-ingested under a different one would keep its slug and silently become a different
        # selection, and the projection's monotonic union would then carry both. Another pattern
        # is another team.
        raise ClaudeHistoryParseError(
            f"team {team_slug!r} was ingested with project pattern {recorded_pattern!r}, "
            f"not {project_pattern!r}; register a different team slug for a different selection"
        )
    raw_sources = as_array(obj.get("sources"), f"{path}: sources")
    if len(raw_sources) != 1:
        raise ClaudeHistoryParseError(f"{path}: expected exactly one history source")
    return ClaudeSourceCopy.from_json_obj(
        as_object(raw_sources[0], f"{path}: sources[0]"), f"{path}: sources[0]"
    )


def _ingest_claude_history_locked(
    archive: Path,
    history_file: Path,
    team_slug: str,
    display_timezone: str,
    project_pattern: str,
    covered_session_ids: Sequence[str],
    date_window: DateWindow | None,
    identity_overrides: IdentityOverrides | None,
    requested_snapshot_root: Path | None,
) -> tuple[TeamData, IngestReport]:
    """Snapshot and normalize the owner's Claude prompt history as one prompt-only team."""

    _ensure_archive(archive, team_slug, create=True)
    changed = int(_ensure_bulk_content_ignored(archive))
    previous_source = _load_claude_history_source_manifest(
        archive, team_slug, project_pattern
    )
    location = _source_snapshot_location(archive, team_slug, requested_snapshot_root)
    snapshot = snapshot_claude_history(
        history_file, location.root, previous_source, utc_now()
    )
    changed += snapshot.files_changed
    team, selection = load_claude_history_team(
        location.root / snapshot.source.snapshot_path,
        team_slug,
        display_timezone,
        project_pattern=project_pattern,
        covered_session_ids=covered_session_ids,
    )
    team = apply_date_window(team, date_window)
    changed += _record_site_identity(archive, team, ((), ()), identity_overrides)
    source_manifest: dict[str, object] = {
        "schema_version": 1,
        "provider": CLAUDE_HISTORY_PROVIDER,
        "root_thread_id": team.root_thread_id,
        "history_file": str(history_file.resolve()),
        "project_pattern": project_pattern,
        "covered_session_ids": sorted(set(covered_session_ids)),
        "snapshot_root": location.archive_relative,
        "date_window": date_window.to_json_obj() if date_window is not None else None,
        "sources": [snapshot.source.to_json_obj()],
    }
    changed += int(
        _write_json_durable(
            _source_manifest_path(archive, team_slug), narrow_json(source_manifest)
        )
    )
    team, report = _write_ingested_team(
        archive, team_slug, team, date_window, changed, location
    )
    return team, replace(
        report,
        history_unmatched_prompts=selection.unmatched_prompts,
        history_covered_prompts=selection.covered_prompts,
        history_covered_sessions=selection.covered_sessions,
    )


def ingest_claude_history(
    archive: Path,
    history_file: Path,
    team_slug: str,
    display_timezone: str,
    project_pattern: str,
    covered_session_ids: Sequence[str] = (),
    date_window: DateWindow | None = None,
    identity_overrides: IdentityOverrides | None = None,
    snapshot_root: Path | None = None,
) -> tuple[TeamData, IngestReport]:
    """Snapshot and normalize one Claude prompt history as one serialized transaction."""

    with archive_writer_lock(archive):
        return _ingest_claude_history_locked(
            archive,
            history_file,
            team_slug,
            display_timezone,
            project_pattern,
            covered_session_ids,
            date_window,
            identity_overrides,
            snapshot_root,
        )


@dataclass(frozen=True)
class _OrcManifestState:
    sources: tuple[OrcSourceCopy, ...]
    continuation_links: tuple[OrcContinuationLink, ...]
    continuation_specs: tuple[OrcContinuationSpec, ...]


def _orc_continuation_specs(
    values: Sequence[str | OrcContinuationSpec], where: str
) -> tuple[OrcContinuationSpec, ...]:
    result = tuple(
        OrcContinuationSpec.from_value(value, f"{where}[{index}]")
        for index, value in enumerate(values)
    )
    session_ids = tuple(spec.session_id for spec in result)
    if len(set(session_ids)) != len(session_ids):
        raise OrcParseError(f"{where}: duplicate session ids are not allowed")
    return result


def _load_orc_source_manifest(
    archive: Path,
    team_slug: str,
    root_session_id: str,
    date_window: DateWindow | None,
    requested_continuations: Sequence[str | OrcContinuationSpec] = (),
) -> _OrcManifestState:
    requested_specs = _orc_continuation_specs(
        requested_continuations, "requested continuation sessions"
    )
    path = _source_manifest_path(archive, team_slug)
    if not path.is_file():
        return _OrcManifestState((), (), requested_specs)
    obj = as_object(read_json(path), str(path))
    schema_version = as_int(obj.get("schema_version"), f"{path}: schema_version")
    if schema_version not in (1, 2, 3, 4, 5) or obj.get("provider") != "orc":
        raise OrcParseError(f"invalid Orc source manifest at {path}")
    if schema_version in (2, 3, 4, 5):
        expected_fields = {
            "schema_version",
            "provider",
            "root_session_id",
            "source_root",
            "snapshot_root",
            "date_window",
            "sources",
        }
        if schema_version in (3, 4, 5):
            expected_fields.add("continuation_sessions")
        if set(obj) != expected_fields:
            missing = sorted(expected_fields - set(obj))
            unknown = sorted(set(obj) - expected_fields)
            raise OrcParseError(
                f"invalid Orc source manifest fields at {path}: "
                f"missing={missing!r}, unknown={unknown!r}"
            )
        recorded_source_root = as_string(
            obj.get("source_root"), f"{path}: source_root"
        )
        if not Path(recorded_source_root).is_absolute():
            raise OrcParseError(f"{path}: source_root must be absolute")
        # Recorded, shape-checked, and deliberately *not* compared against where this run is
        # about to write.
        #
        # It used to be compared, against the single hardcoded `teams/<slug>/source_snapshots`,
        # and that check stopped being expressible the moment the store became configurable and
        # relocatable: after a migration the manifest still says where the bytes were at the last
        # ingest, which is the honest thing for it to say and is no longer where they are. Making
        # `migrate-snapshots` rewrite the manifest instead was rejected -- `raw/source-manifest.json`
        # is bound by digest into `raw/normalized-generation.json`, so touching it would leave
        # every reader refusing the archive as a mixed generation until someone re-ingested, which
        # is a much worse outcome than a stale descriptive field.
        #
        # Nothing is lost, because the string equality was only ever a proxy for the question that
        # matters -- "are the previous snapshots this manifest names actually here?" -- and that
        # question is answered properly, per object and by SHA-256, in `_prepare_snapshot_candidate`
        # a few frames down. A relocated tree whose contents are wrong fails there, on the bytes,
        # rather than here, on a path.
        #
        # The shape check stays: relative, non-empty. An absolute path in a tracked, HTTP-served
        # file would publish somebody's home directory, and a manifest that recorded one would keep
        # being wrong every time the archive moved.
        recorded_snapshot_root = as_string(
            obj.get("snapshot_root"), f"{path}: snapshot_root"
        )
        if not recorded_snapshot_root or Path(recorded_snapshot_root).is_absolute():
            raise OrcParseError(
                f"{path}: snapshot_root must be a non-empty path relative to the archive"
            )
    recorded_root = as_string(obj.get("root_session_id"), f"{path}: root_session_id")
    if recorded_root != root_session_id:
        raise OrcParseError(
            f"source manifest belongs to root {recorded_root!r}, not {root_session_id!r}"
        )
    _validate_manifest_window(
        obj.get("date_window"), date_window, str(path), OrcParseError
    )
    result: list[OrcSourceCopy] = []
    for index, raw_source in enumerate(
        as_array(obj.get("sources"), f"{path}: sources")
    ):
        source = as_object(raw_source, f"{path}: sources[{index}]")
        result.append(
            OrcSourceCopy.from_json_obj(
                source,
                f"{path}: sources[{index}]",
                2 if schema_version in (3, 4, 5) else schema_version,
            )
        )
    links: list[OrcContinuationLink] = []
    raw_continuations = obj.get("continuation_sessions")
    if raw_continuations is not None:
        if schema_version not in (3, 4, 5):
            raise OrcParseError(
                f"{path}: continuation_sessions requires manifest schema version 3, 4 or 5"
            )
        for index, raw_link in enumerate(
            as_array(raw_continuations, f"{path}: continuation_sessions")
        ):
            link = as_object(
                raw_link, f"{path}: continuation_sessions[{index}]"
            )
            has_bounded_fields = (
                "start_message_id" in link or "start_source_line" in link
            )
            has_boundary_table = "predecessor_source_table" in link
            if schema_version == 3 and has_bounded_fields:
                raise OrcParseError(
                    f"{path}: schema-v3 continuation record cannot contain bounded fields"
                )
            if schema_version in (3, 4) and has_boundary_table:
                raise OrcParseError(
                    f"{path}: schema-v{schema_version} continuation record cannot "
                    "name a predecessor boundary table"
                )
            if schema_version in (4, 5) and not (
                "start_message_id" in link and "start_source_line" in link
            ):
                raise OrcParseError(
                    f"{path}: schema-v{schema_version} continuation record lacks "
                    "bounded fields"
                )
            # v5 is the version at which the boundary ordinal stopped being a bare integer whose
            # table had to be re-derived at read time. The key must be present; its *value* may
            # still be null, because a boundary whose ordinal resolves identically in both candidate
            # tables is deliberately left unresolved rather than guessed. Presence is therefore the
            # only thing worth demanding: it distinguishes "a table-aware writer looked at this
            # link" from "this link predates the question".
            if schema_version == 5 and not has_boundary_table:
                raise OrcParseError(
                    f"{path}: schema-v5 continuation record lacks its "
                    "predecessor_source_table field"
                )
            links.append(
                OrcContinuationLink.from_json_obj(
                    link, f"{path}: continuation_sessions[{index}]"
                )
            )
    recorded_specs = tuple(
        OrcContinuationSpec.from_value(
            {
                "session_id": link.session_id,
                "start_message_id": link.start_message_id,
            },
            f"{path}: continuation_sessions[{index}]",
        )
        for index, link in enumerate(links)
    )
    if requested_specs:
        if recorded_specs != requested_specs[: len(recorded_specs)]:
            raise OrcParseError(
                "requested continuation sessions do not extend the recorded ordered prefix"
            )
        effective_specs = requested_specs
    else:
        effective_specs = recorded_specs
    return _OrcManifestState(tuple(result), tuple(links), effective_specs)


def _ingest_orc_locked(
    archive: Path,
    source_root: Path,
    root_session_id: str,
    team_slug: str,
    display_timezone: str,
    date_window: DateWindow | None,
    identity_overrides: IdentityOverrides | None,
    continuation_specs: Sequence[str | OrcContinuationSpec],
    accept_prefix_rewrite: Sequence[str],
    requested_snapshot_root: Path | None,
) -> tuple[TeamData, IngestReport]:
    """Snapshot and normalize one Orc coordinator lineage."""

    _ensure_archive(archive, team_slug, create=True)
    changed = int(_ensure_bulk_content_ignored(archive))
    manifest_state = _load_orc_source_manifest(
        archive,
        team_slug,
        root_session_id,
        date_window,
        continuation_specs,
    )
    location = _source_snapshot_location(archive, team_slug, requested_snapshot_root)
    snapshot_root = location.root
    changed += prune_orc_staging(snapshot_root)
    snapshot = snapshot_orc_lineage(
        source_root,
        root_session_id,
        snapshot_root,
        manifest_state.sources,
        utc_now(),
        manifest_state.continuation_specs,
        manifest_state.continuation_links,
        accept_prefix_rewrite,
    )
    changed += snapshot.files_changed
    resolved_specs = tuple(
        OrcContinuationSpec.from_value(
            {
                "session_id": link.session_id,
                "start_message_id": link.start_message_id,
            },
            f"resolved continuation sessions[{index}]",
        )
        for index, link in enumerate(snapshot.continuations)
    )
    if resolved_specs != manifest_state.continuation_specs:
        raise OrcParseError(
            "resolved continuation sessions differ from the requested ordered specs"
        )
    team = apply_date_window(
        load_orc_team(
            snapshot_root,
            root_session_id,
            team_slug,
            display_timezone,
            snapshot.sources,
            snapshot.continuations,
        ),
        date_window,
    )
    parsed_paths = tuple(sorted(source.path for source in team.sources))
    expected_paths = tuple(
        sorted(source.source_path for source in snapshot.sources)
    )
    if parsed_paths != expected_paths:
        raise OrcParseError(
            "parsed source set differs from the validated Orc source snapshots"
        )
    changed += _record_site_identity(
        archive, team, ((), ()), identity_overrides
    )
    source_manifest: dict[str, object] = {
        "schema_version": 5 if snapshot.continuations else 2,
        "provider": "orc",
        "root_session_id": root_session_id,
        "source_root": str(source_root.resolve()),
        "snapshot_root": location.archive_relative,
        "date_window": date_window.to_json_obj() if date_window is not None else None,
        "sources": [source.to_json_obj() for source in snapshot.sources],
    }
    if snapshot.continuations:
        source_manifest["continuation_sessions"] = [
            link.to_json_obj() for link in snapshot.continuations
        ]
    changed += int(
        _write_json_durable(
            _source_manifest_path(archive, team_slug),
            narrow_json(source_manifest),
        )
    )
    archived, report = _write_ingested_team(
        archive, team_slug, team, date_window, changed, location
    )
    marker_changed = int(
        _write_json_durable(
            _normalized_generation_path(archive, team_slug),
            _normalized_generation_value(archive, team_slug, archived),
        )
    )
    gc_changed = prune_orc_snapshot_objects(snapshot_root, snapshot.sources)
    return archived, replace(
        report,
        files_changed=report.files_changed + marker_changed + gc_changed,
        orc_prefix_overrides=snapshot.prefix_overrides,
    )


def ingest_orc(
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
    """Snapshot and normalize one Orc lineage as a serialized raw-data transaction.

    ``accept_prefix_rewrite`` is empty by default and stays empty unless an operator names sessions
    on the command line. When a *named* session's recorded append prefix has been rewritten in
    place, the digest is re-baselined, the change is described row by row and column by column in
    ``IngestReport.orc_prefix_overrides`` and in the source manifest, and the source is marked
    degraded. Nothing is deleted and no row is skipped on that path. A rewritten session the list
    does not name still refuses the whole ingest, because the operator has not been shown it yet.
    """

    with archive_writer_lock(archive):
        return _ingest_orc_locked(
            archive,
            source_root,
            root_session_id,
            team_slug,
            display_timezone,
            date_window,
            identity_overrides,
            continuation_specs,
            accept_prefix_rewrite,
            snapshot_root,
        )


def load_archived_team(archive: Path, team_slug: str) -> TeamData:
    """Load and validate the normalized team snapshot stored in *archive*."""

    _ensure_archive(archive, team_slug, create=False)
    path = _raw_team_path(archive, team_slug)
    if not path.is_file():
        raise ValueError(f"no ingested team {team_slug!r}; run `wrkviz ingest`")
    # `raw/team.json` and `raw/task-notes.jsonl` are two files holding one model; the split is a
    # storage decision made in `_write_ingested_team` and stops here. Everything above this line
    # in the archive and everything below it in the program sees a single `TeamData`.
    team = replace(
        team_from_json_obj(read_json(path)),
        task_notes=_load_promoted_task_notes(archive, team_slug),
    )
    if team.team_slug != team_slug:
        raise ValueError(
            f"archived team slug {team.team_slug!r} does not match requested {team_slug!r}"
        )
    _validate_team_slug(team.team_slug)
    _validate_archive_id(team.root_thread_id, "root thread id")
    for agent in team.agents:
        _validate_archive_id(agent.thread_id, "thread id")
    source_manifest_path = _source_manifest_path(archive, team_slug)
    if source_manifest_path.is_file():
        source_manifest = as_object(
            read_json(source_manifest_path), str(source_manifest_path)
        )
        if (
            source_manifest.get("provider") == "orc"
            and source_manifest.get("schema_version") in (2, 3, 4, 5)
        ):
            _validate_normalized_generation(archive, team_slug, team)
    return team


def extract_transcripts_archive(
    archive: Path,
    team_slugs: Sequence[str] = (),
    authorship_rules: Sequence[PromptAuthorshipRule] | None = None,
) -> TranscriptExportReport:
    """Mechanically export coordinator prompts/responses for selected ingested teams.

    An empty selection means every team with normalized raw data in this archive. The archive
    writer lock makes the multi-file JSONL generation serial with provider ingestion and site
    builds; no summarizer or model adapter is reachable from this operation.

    A team that cannot be loaded is skipped and reported rather than ending the extraction. Loading
    is per team and reads only ``teams/<slug>/``, so one team's torn or unreadable state says
    nothing about the others -- and the projection is a monotonic union seeded with everything
    already in ``occurrences.jsonl``, so a skipped team keeps contributing exactly what it
    contributed on its last good run. Raising instead, as this did, meant a single team caught
    between its source manifest and its normalized-generation marker withheld every *other* team's
    new prompts on every run until a human intervened. The returned report names the skipped teams
    and their causes; callers must consult :attr:`TranscriptExportReport.partial` and report it,
    because a partial projection that reads as complete is worse than the failure it replaced.

    All of them failing is still fatal: see :func:`export_transcripts` for why an archive nobody
    can read is the one state not to rewrite the corpus in.
    """

    from wrkviz.transcript_export import (
        TranscriptTeamSkip as _TranscriptTeamSkip,
        export_transcripts,
    )

    with archive_writer_lock(archive):
        selected = tuple(team_slugs)
        if not selected:
            selected = ingested_team_slugs(archive)
        if not selected:
            raise ValueError(f"no ingested teams found in {archive}")
        if len(set(selected)) != len(selected):
            raise ValueError("transcript extraction team selection contains duplicates")
        teams: list[TeamData] = []
        skipped: list[TranscriptTeamSkip] = []
        for team_slug in selected:
            try:
                teams.append(load_archived_team(archive, team_slug))
            except Exception as error:  # Deliberately broad; see TranscriptTeamSkip.from_exception.
                # BaseException is deliberately NOT caught: a KeyboardInterrupt or SystemExit is the
                # operator or the runtime stopping this process, and carrying on to the next team
                # would be ignoring that, not being robust to one bad team.
                skipped.append(_TranscriptTeamSkip.from_exception(team_slug, error))
        if not teams:
            raise ValueError(
                f"no archive team in {archive} could be loaded for transcript extraction: "
                + "; ".join(skip.summary for skip in skipped)
            )
        return export_transcripts(archive, teams, authorship_rules, skipped)


def load_artifact_catalog(
    archive: Path, team_slug: str, team: TeamData | None = None
) -> ArtifactCatalog:
    """Load mechanical artifact data, accepting pre-artifact archives as empty.

    Existing summary caches remain valid because artifact extraction is independent of model
    inputs. Re-running ingest creates the catalog from source snapshots; until then, an archive
    without a catalog builds with an empty one rather than demanding summary regeneration.
    """

    archived_team = team if team is not None else load_archived_team(archive, team_slug)
    path = _artifact_catalog_path(archive, team_slug)
    if not path.is_file():
        return ArtifactCatalog(source_digest(archived_team), (), ())
    catalog = artifact_catalog_from_json(read_json(path))
    expected_digest = source_digest(archived_team)
    if catalog.source_digest != expected_digest:
        raise ValueError(
            f"stale artifact catalog for {team_slug!r}; rerun ingest before building"
        )
    return catalog


def _glossary_terms(team: TeamData) -> tuple[GlossaryTerm, ...]:
    sources = [
        TermSource(event.timestamp_ms, event.text or "")
        for event in team.events
        if event.thread_id == team.root_thread_id
        and event.kind == "user_prompt"
        and event.text
        and (team.window_end_ms is None or event.timestamp_ms < team.window_end_ms)
    ]
    return scan_terminology(sources, team.display_timezone)


@dataclass(frozen=True)
class _KnowledgeEpoch:
    """An immutable source frontier for reusable generated knowledge."""

    epoch_id: str
    cutoff_ms: int
    cutoff_reason: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return stable epoch metadata for durable summary provenance."""

        return {
            "epoch_id": self.epoch_id,
            "cutoff_ms": self.cutoff_ms,
            "cutoff_reason": self.cutoff_reason,
        }


@dataclass(frozen=True)
class _ProjectOverviewInput:
    """Bounded early/root evidence used by the durable project-overview job."""

    epoch: _KnowledgeEpoch
    start_ms: int
    end_ms: int
    transcript: str
    event_ids: tuple[str, ...]
    context_sha256: str


class _ProjectOverviewSourceSetChanged(ValueError):
    """Signal a verified append/backfill that needs a fresh knowledge epoch."""

    def __init__(self, path: Path, previous_epoch: _KnowledgeEpoch) -> None:
        super().__init__(
            f"{path}.source: frozen overview source set changed with prior evidence intact"
        )
        self.previous_epoch = previous_epoch


@dataclass(frozen=True)
class _GlossaryEvidence:
    """One bounded source occurrence supplied to a glossary-definition job."""

    event_id: str
    thread_id: str
    at_ms: int
    kind: str
    context: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return stable provenance for the structured glossary artifact."""

        return {
            "event_id": self.event_id,
            "thread_id": self.thread_id,
            "at_ms": self.at_ms,
            "kind": self.kind,
            "context": self.context,
        }


@dataclass(frozen=True)
class _TermKnowledge:
    """Frozen definition evidence and chronological availability for one term."""

    evidence: tuple[_GlossaryEvidence, ...]
    available_at_ms: int
    definition_cutoff_ms: int
    definition_epoch_id: str


def _one_line(value: str) -> str:
    return " ".join(value.strip().split())


def _current_knowledge_epoch(team: TeamData) -> _KnowledgeEpoch:
    if team.window_end_ms is not None:
        cutoff_ms = team.window_end_ms
        reason = "archive-window-end"
    else:
        latest_ms = max((event.timestamp_ms for event in team.events), default=0)
        cutoff_ms = latest_ms + 1
        reason = "first-summary-source-frontier"
    digest = hashlib.sha256(
        f"{team.team_slug}\0{cutoff_ms}\0{reason}".encode("utf-8")
    ).hexdigest()[:20]
    return _KnowledgeEpoch(f"knowledge-{digest}", cutoff_ms, reason)


def _root_overview_input(
    team: TeamData, epoch: _KnowledgeEpoch | None = None
) -> _ProjectOverviewInput:
    selected_epoch = epoch if epoch is not None else _current_knowledge_epoch(team)
    events = sorted(
        (
            event
            for event in team.events
            if event.thread_id == team.root_thread_id
            and event.kind in {"user_prompt", "assistant_message"}
            and event.text
            and event.timestamp_ms < selected_epoch.cutoff_ms
        ),
        key=lambda event: (event.timestamp_ms, event.event_id),
    )
    return _overview_input_from_events(team, selected_epoch, events)


def _overview_input_from_events(
    team: TeamData,
    epoch: _KnowledgeEpoch,
    events: Sequence[Event],
) -> _ProjectOverviewInput:
    """Build overview evidence from an already ordered root-event sequence."""

    parts: list[str] = []
    event_ids: list[str] = []
    used = 0
    retained: list[Event] = []
    for event in events:
        assert event.text is not None
        prefix = f"[{event.timestamp_ms}] {event.kind}: "
        remaining = _OVERVIEW_CONTEXT_CHARS - used
        if remaining <= len(prefix):
            break
        line = prefix + _one_line(event.text)
        excerpt = line[:remaining]
        if excerpt:
            parts.append(excerpt)
            event_ids.append(event.event_id)
            retained.append(event)
            used += len(excerpt) + 2
        if len(excerpt) < len(line) or used >= _OVERVIEW_CONTEXT_CHARS:
            break
    if retained:
        start_ms = retained[0].timestamp_ms
        end_ms = retained[-1].timestamp_ms
    else:
        fallback = team.window_start_ms
        if fallback is None or fallback >= epoch.cutoff_ms:
            fallback = min(
                (
                    event.timestamp_ms
                    for event in team.events
                    if event.timestamp_ms < epoch.cutoff_ms
                ),
                default=max(0, epoch.cutoff_ms - 1),
            )
        start_ms = fallback
        end_ms = fallback
        parts.append("No root user or assistant transcript text was retained.")
    transcript = "\n\n".join(parts)
    return _ProjectOverviewInput(
        epoch=epoch,
        start_ms=start_ms,
        end_ms=end_ms,
        transcript=transcript,
        event_ids=tuple(event_ids),
        context_sha256=hashlib.sha256(transcript.encode("utf-8")).hexdigest(),
    )


def _recorded_overview_input(
    team: TeamData,
    epoch: _KnowledgeEpoch,
    event_ids: tuple[str, ...],
) -> _ProjectOverviewInput | None:
    """Reconstruct prior evidence by ID so new early events do not hide mutation."""

    if len(event_ids) != len(set(event_ids)):
        return None
    by_id = {event.event_id: event for event in team.events}
    events: list[Event] = []
    for event_id in event_ids:
        event = by_id.get(event_id)
        if (
            event is None
            or event.thread_id != team.root_thread_id
            or not event.text
            or event.timestamp_ms >= epoch.cutoff_ms
        ):
            return None
        if event.kind not in {"user_prompt", "assistant_message"}:
            # Claude ingestion formerly treated provider-authored user-role envelopes as owner
            # prompts. Reconstruct that prior prefix to prove their text/timestamp stayed intact
            # while allowing the corrected classification to start a new source-set epoch.
            if (
                event.kind != "system_input"
                or event.role != "user"
                or event.ingress_kind != "claude_system"
                or event.author_kind != "system"
            ):
                return None
            event = replace(event, kind="user_prompt")
        events.append(event)
    result = _overview_input_from_events(team, epoch, events)
    return result if result.event_ids == event_ids else None


def _renewed_project_overview_input(
    team: TeamData, previous_epoch: _KnowledgeEpoch
) -> _ProjectOverviewInput:
    """Create a deterministic new epoch for verified append/backfill evidence."""

    current = _root_overview_input(team)
    digest = hashlib.sha256(
        canonical_json(
            {
                "team_slug": team.team_slug,
                "previous_epoch_id": previous_epoch.epoch_id,
                "cutoff_ms": current.epoch.cutoff_ms,
                "cutoff_reason": current.epoch.cutoff_reason,
                "event_ids": list(current.event_ids),
                "context_sha256": current.context_sha256,
            }
        ).encode("utf-8")
    ).hexdigest()[:20]
    epoch = _KnowledgeEpoch(
        epoch_id=f"knowledge-{digest}",
        cutoff_ms=current.epoch.cutoff_ms,
        cutoff_reason=current.epoch.cutoff_reason,
    )
    return replace(current, epoch=epoch)


def _project_overview_job(
    team: TeamData, source: _ProjectOverviewInput
) -> SummaryJob:
    return SummaryJob(
        key=f"project-overview:{team.team_slug}",
        team_slug=team.team_slug,
        agent_label=f"{team.team_slug} project overview",
        start_ms=source.start_ms,
        end_ms=source.end_ms,
        prior_context="",
        transcript=source.transcript,
        glossary="",
        stats={
            "source_events": len(source.event_ids),
            "source_characters": len(source.transcript),
        },
        summary_style=PROJECT_OVERVIEW_STYLE,
        context_coverage=ContextCoverage(
            components=(
                ContextComponent(
                    "early_root_transcript",
                    _OVERVIEW_CONTEXT_CHARS,
                    min(_OVERVIEW_CONTEXT_CHARS, len(source.transcript)),
                    "characters",
                ),
            )
        ),
    )


def _term_context(text: str, term: str) -> str:
    clean = _one_line(text)
    position = clean.find(term)
    if position < 0:
        return clean[:520]
    start = max(0, position - 180)
    end = min(len(clean), position + len(term) + 320)
    return clean[start:end].strip()


def _definition_evidence(
    team: TeamData, term: GlossaryTerm, cutoff_ms: int | None = None
) -> tuple[_GlossaryEvidence, ...]:
    effective_cutoff = (
        _current_knowledge_epoch(team).cutoff_ms
        if cutoff_ms is None
        else cutoff_ms
    )
    result: list[_GlossaryEvidence] = []
    for event in sorted(team.events, key=lambda item: (item.timestamp_ms, item.event_id)):
        if (
            not event.text
            or term.term not in event.text
            or event.timestamp_ms >= effective_cutoff
        ):
            continue
        result.append(
            _GlossaryEvidence(
                event_id=event.event_id,
                thread_id=event.thread_id,
                at_ms=event.timestamp_ms,
                kind=event.kind,
                context=_term_context(event.text, term.term),
            )
        )
        if len(result) == _TERM_EVIDENCE_LIMIT:
            break
    return tuple(result)


def _definition_epoch_id(
    term_id: str,
    project_epoch_id: str,
    cutoff_ms: int,
    evidence: Sequence[_GlossaryEvidence],
) -> str:
    fields = [term_id, project_epoch_id, str(cutoff_ms)]
    fields.extend(
        f"{item.thread_id}:{item.event_id}:{item.at_ms}:{item.kind}:{item.context}"
        for item in evidence
    )
    digest = hashlib.sha256("\0".join(fields).encode("utf-8")).hexdigest()[:20]
    return f"definition-{digest}"


def _glossary_definition_job(
    team: TeamData,
    term: GlossaryTerm,
    evidence: Sequence[_GlossaryEvidence],
    project_overview: SummaryResult,
) -> SummaryJob:
    transcript_lines = [f"Glossary term (exact spelling): {term.term}", "Source occurrences:"]
    transcript_lines.extend(
        f"- [{item.at_ms}] {item.kind} in {item.thread_id}: {item.context}"
        for item in evidence
    )
    if not evidence:
        transcript_lines.append("- No retained source occurrence was found.")
    start_ms = evidence[0].at_ms if evidence else term.introduced_at_ms
    end_ms = evidence[-1].at_ms if evidence else term.introduced_at_ms
    return SummaryJob(
        key=f"glossary-definition:{term.term_id}",
        team_slug=team.team_slug,
        agent_label=f"Glossary definition · {term.term}",
        start_ms=start_ms,
        end_ms=end_ms,
        prior_context=(
            "Durable source-bounded project overview:\n" + project_overview.paragraph
        ),
        transcript="\n".join(transcript_lines),
        glossary=f"Exact glossary name: {term.term}",
        stats={"source_occurrences": len(evidence)},
        summary_style=GLOSSARY_DEFINITION_STYLE,
        context_coverage=ContextCoverage(
            components=(
                ContextComponent(
                    "source_occurrences",
                    _TERM_EVIDENCE_LIMIT,
                    min(_TERM_EVIDENCE_LIMIT, len(evidence)),
                    "occurrences",
                ),
                ContextComponent("project_overview", 1, 1, "artifacts"),
            )
        ),
        dependency_keys=(
            (
                project_overview.artifact_provenance.artifact_id
                if project_overview.artifact_provenance is not None
                else project_overview.input_hash
            ),
        ),
    )


def _definition_status(summary: SummaryResult) -> str:
    if summary.phrase == "Definition supported":
        return "supported"
    if summary.phrase == "Insufficient evidence":
        return "insufficient-evidence"
    raise ValueError(
        f"glossary definition {summary.key!r} has invalid evidence status {summary.phrase!r}"
    )


def _phase_jobs(
    team: TeamData,
    phases: Sequence[PhaseWindow],
    terms: Sequence[GlossaryTerm],
    context_chars: int,
) -> tuple[SummaryJob, ...]:
    jobs: list[SummaryJob] = []
    for phase in phases:
        chronological_terms = [
            term
            for term in terms
            if term.summary_available_at_ms < phase.end_ms
        ]
        jobs.append(
            SummaryJob(
                key=phase.summary_key,
                team_slug=team.team_slug,
                agent_label=phase.agent_label,
                start_ms=phase.start_ms,
                end_ms=phase.end_ms,
                prior_context=phase.prior_context,
                transcript=phase.transcript_text,
                glossary=glossary_prompt_text(chronological_terms),
                stats=phase.stats.to_mapping(),
                context_coverage=ContextCoverage(
                    components=(
                        ContextComponent(
                            "ancestor_transcript",
                            context_chars,
                            min(context_chars, len(phase.prior_context)),
                            "characters",
                        ),
                    )
                ),
            )
        )
    return tuple(jobs)


def _summary_json(summary: SummaryResult) -> dict[str, JsonValue]:
    cleaned = clean_summary_result(summary)
    result: dict[str, JsonValue] = {
        "key": cleaned.key,
        "phrase": cleaned.phrase,
        "paragraph": cleaned.paragraph,
        "work_summary": [
            {"at_ms": item.at_ms, "text": item.text} for item in cleaned.work_summary
        ],
        "model": cleaned.model,
        "prompt_version": cleaned.prompt_version,
        "input_hash": cleaned.input_hash,
        "generated_at": cleaned.generated_at,
    }
    if cleaned.artifact_provenance is not None:
        result["artifact_provenance"] = cleaned.artifact_provenance.to_json_obj()
    return result


def _require_exact_keys(
    obj: Mapping[str, JsonValue], expected: set[str], where: str
) -> None:
    actual = set(obj)
    if actual == expected:
        return
    missing = sorted(expected - actual)
    extra = sorted(actual - expected)
    details: list[str] = []
    if missing:
        details.append("missing " + ", ".join(missing))
    if extra:
        details.append("extra " + ", ".join(extra))
    raise ValueError(f"{where}: schema mismatch ({'; '.join(details)})")


def _summary_from_json(value: JsonValue, where: str) -> SummaryResult:
    obj = as_object(value, where)
    base_keys = {
        "key",
        "phrase",
        "paragraph",
        "work_summary",
        "model",
        "prompt_version",
        "input_hash",
        "generated_at",
    }
    actual_keys = set(obj)
    if actual_keys not in (base_keys, base_keys | {"artifact_provenance"}):
        _require_exact_keys(obj, base_keys, where)
    raw_bullets = as_array(obj.get("work_summary"), f"{where}.work_summary")
    for index, raw_bullet in enumerate(raw_bullets):
        bullet = as_object(raw_bullet, f"{where}.work_summary[{index}]")
        _require_exact_keys(
            bullet, {"at_ms", "text"}, f"{where}.work_summary[{index}]"
        )
    bullets = tuple(
        WorkBullet(
            at_ms=as_int(as_object(item, f"{where}.work_summary[]").get("at_ms"), "at_ms"),
            text=as_string(
                as_object(item, f"{where}.work_summary[]").get("text"), "text"
            ),
        )
        for item in raw_bullets
    )
    return clean_summary_result(
        SummaryResult(
            key=as_string(obj.get("key"), f"{where}.key"),
            phrase=as_string(obj.get("phrase"), f"{where}.phrase"),
            paragraph=as_string(obj.get("paragraph"), f"{where}.paragraph"),
            work_summary=bullets,
            model=as_string(obj.get("model"), f"{where}.model"),
            prompt_version=as_string(
                obj.get("prompt_version"), f"{where}.prompt_version"
            ),
            input_hash=as_string(obj.get("input_hash"), f"{where}.input_hash"),
            generated_at=as_string(obj.get("generated_at"), f"{where}.generated_at"),
            artifact_provenance=(
                SummaryArtifactProvenance.from_json_obj(
                    obj.get("artifact_provenance"), f"{where}.artifact_provenance"
                )
                if "artifact_provenance" in obj
                else None
            ),
        )
    )


def _summary_from_catalog_reference(
    summary_root: Path, reference: SummaryArtifactReference
) -> SummaryResult:
    path = summary_root / reference.cache_path
    root = as_object(read_json(path), str(path))
    provenance = reference.provenance
    if root.get("format") == ARTIFACT_ENVELOPE_FORMAT:
        if as_int(root.get("schema_version"), f"{path}.schema_version") != (
            ARTIFACT_ENVELOPE_VERSION
        ):
            raise ValueError(f"{path}: unsupported model-artifact envelope")
        cached_provenance = SummaryArtifactProvenance.from_json_obj(
            root.get("artifact"), f"{path}.artifact"
        )
        if cached_provenance != provenance:
            raise ValueError(f"{path}: cache provenance differs from artifact catalog")
    elif not provenance.legacy_storage:
        raise ValueError(f"{path}: cataloged artifact lacks its common envelope")
    result = _summary_from_json(root.get("result"), f"{path}.result")
    if (
        result.input_hash != provenance.input_hash
        or result.model != provenance.model
        or result.prompt_version != provenance.prompt_version
    ):
        raise ValueError(f"{path}: cached result differs from artifact provenance")
    return replace(result, artifact_provenance=provenance)


def _select_catalog_summary(
    summary_root: Path,
    catalog: SummaryArtifactCatalog,
    logical_key: str,
    summarizer_id: str,
    fallback: SummaryResult,
) -> SummaryResult:
    reference = select_summary_artifact(
        catalog,
        logical_key,
        summarizer_id,
    )
    if reference is None:
        return fallback
    return _summary_from_catalog_reference(summary_root, reference)


def _select_catalog_summary_for_range(
    summary_root: Path,
    catalog: SummaryArtifactCatalog,
    logical_key: str,
    summarizer_id: str,
    start_ms: int,
    end_ms: int,
    fallback: SummaryResult,
) -> SummaryResult:
    """Use a catalog result only when its source interval exactly matches the view."""

    reference = select_summary_artifact(catalog, logical_key, summarizer_id)
    if reference is None or (
        reference.provenance.start_ms != start_ms
        or reference.provenance.end_ms != end_ms
    ):
        return fallback
    return _summary_from_catalog_reference(summary_root, reference)


def _select_catalog_summary_for_job(
    summary_root: Path,
    catalog: SummaryArtifactCatalog,
    job: SummaryJob,
    summarizer_id: str,
    fallback: SummaryResult,
) -> SummaryResult:
    """Select the strongest catalog artifact whose full source identity is current."""

    compatible = SummaryArtifactCatalog(
        team_slug=catalog.team_slug,
        records=tuple(
            reference
            for reference in catalog.records
            if reference.provenance.summarizer_id == summarizer_id
            and _summary_provenance_matches_job(reference.provenance, job)
        ),
    )
    reference = select_summary_artifact(
        compatible,
        job.key,
        summarizer_id,
    )
    if reference is None:
        return fallback
    return _summary_from_catalog_reference(summary_root, reference)


def _prior_catalog_rollups(
    summary_root: Path,
    catalog: SummaryArtifactCatalog,
    kind: str,
    summary_style: str,
    before_ms: int,
) -> list[tuple[Period, SummaryResult]]:
    plain_language = summary_style == PLAIN_LANGUAGE_ROLLUP_STYLE
    summarizer_id = (
        "plain-language-rollup" if plain_language else "technical-rollup"
    )
    key_prefix = "rollup-plain" if plain_language else "rollup"
    logical_prefix = f"{key_prefix}:{kind}:"
    logical_keys = sorted(
        {
            record.provenance.logical_key
            for record in catalog.records
            if record.provenance.summarizer_id == summarizer_id
            and record.provenance.logical_key.startswith(logical_prefix)
            and record.provenance.end_ms <= before_ms
        }
    )
    result: list[tuple[Period, SummaryResult]] = []
    for logical_key in logical_keys:
        reference = select_summary_artifact(
            catalog,
            logical_key,
            summarizer_id,
        )
        if reference is None:
            continue
        provenance = reference.provenance
        key = logical_key[len(logical_prefix) :]
        result.append(
            (
                Period(
                    kind=kind,
                    key=key,
                    label=f"Prior {kind} · {key}",
                    start_ms=provenance.start_ms,
                    end_ms=provenance.end_ms,
                    relative_path="",
                    partial=False,
                ),
                _summary_from_catalog_reference(summary_root, reference),
            )
        )
    result.sort(key=lambda item: (item[0].end_ms, item[0].key))
    return result[-10:]


def _validate_project_overview(summary: SummaryResult, where: str) -> None:
    try:
        summarizer_change_for_prompt(
            PROJECT_OVERVIEW_SUMMARIZER, summary.prompt_version
        )
    except ValueError as error:
        raise ValueError(f"{where}: unregistered project overview version") from error
    if summary.phrase not in {"Project overview supported", "Insufficient evidence"}:
        raise ValueError(f"{where}: invalid project-overview evidence status")
    if summary.work_summary:
        raise ValueError(f"{where}: project overview must not contain work bullets")
    if summary.phrase == "Insufficient evidence" and not summary.paragraph.startswith(
        "Insufficient evidence:"
    ):
        raise ValueError(f"{where}: insufficient overview lacks an evidence limit")
    if knowledge_text_has_link(summary.paragraph):
        raise ValueError(f"{where}: project overview contains an unverified link")


def _validate_glossary_definition(summary: SummaryResult, where: str) -> None:
    try:
        summarizer_change_for_prompt(
            GLOSSARY_DEFINITION_SUMMARIZER, summary.prompt_version
        )
    except ValueError as error:
        raise ValueError(f"{where}: unregistered glossary definition version") from error
    _definition_status(summary)
    if summary.work_summary:
        raise ValueError(f"{where}: glossary definition must not contain work bullets")
    if summary.phrase == "Insufficient evidence" and not summary.paragraph.startswith(
        "Insufficient evidence:"
    ):
        raise ValueError(f"{where}: insufficient definition lacks an evidence limit")
    if knowledge_text_has_link(summary.paragraph):
        raise ValueError(f"{where}: glossary definition contains an unverified link")


def _write_project_overview_data(
    archive: Path,
    team_slug: str,
    source: _ProjectOverviewInput,
    summary: SummaryResult,
) -> int:
    obj: dict[str, JsonValue] = {
        "schema_version": _PROJECT_OVERVIEW_SCHEMA_VERSION,
        "style": PROJECT_OVERVIEW_STYLE,
        "knowledge_epoch": source.epoch.to_json_obj(),
        "source": {
            "event_ids": list(source.event_ids),
            "start_ms": source.start_ms,
            "end_ms": source.end_ms,
            "context_sha256": source.context_sha256,
        },
        "summary": _summary_json(summary),
    }
    return int(
        write_json_if_changed(
            _summary_root(archive, team_slug) / "project_overview.json", obj
        )
    )


def _knowledge_epoch_from_json(value: JsonValue, where: str) -> _KnowledgeEpoch:
    obj = as_object(value, where)
    _require_exact_keys(obj, {"epoch_id", "cutoff_ms", "cutoff_reason"}, where)
    epoch_id = as_string(obj.get("epoch_id"), f"{where}.epoch_id")
    if not epoch_id.startswith("knowledge-"):
        raise ValueError(f"{where}.epoch_id: invalid knowledge epoch ID")
    cutoff_ms = as_int(obj.get("cutoff_ms"), f"{where}.cutoff_ms")
    cutoff_reason = as_string(obj.get("cutoff_reason"), f"{where}.cutoff_reason")
    if cutoff_reason not in {"archive-window-end", "first-summary-source-frontier"}:
        raise ValueError(f"{where}.cutoff_reason: invalid cutoff reason")
    return _KnowledgeEpoch(epoch_id, cutoff_ms, cutoff_reason)


def _frozen_project_overview_input(
    archive: Path, team: TeamData
) -> _ProjectOverviewInput | None:
    """Load and validate the projected immutable project evidence epoch."""

    path = _summary_root(archive, team.team_slug) / "project_overview.json"
    if not path.is_file():
        return None
    obj = as_object(read_json(path), str(path))
    if "schema_version" not in obj:
        return None
    version = as_int(obj.get("schema_version"), f"{path}.schema_version")
    if version != _PROJECT_OVERVIEW_SCHEMA_VERSION:
        # Older schemas did not have enough immutable provenance to distinguish append-only
        # growth from a historical mutation.
        return None
    _require_exact_keys(
        obj,
        {"schema_version", "style", "knowledge_epoch", "source", "summary"},
        str(path),
    )
    if as_string(obj.get("style"), f"{path}.style") != PROJECT_OVERVIEW_STYLE:
        raise ValueError(f"{path}: invalid project-overview style")
    epoch = _knowledge_epoch_from_json(
        obj.get("knowledge_epoch"), f"{path}.knowledge_epoch"
    )
    source = as_object(obj.get("source"), f"{path}.source")
    _require_exact_keys(
        source,
        {"event_ids", "start_ms", "end_ms", "context_sha256"},
        f"{path}.source",
    )
    event_ids = tuple(
        as_string(item, f"{path}.source.event_ids[]")
        for item in as_array(source.get("event_ids"), f"{path}.source.event_ids")
    )
    start_ms = as_int(source.get("start_ms"), f"{path}.source.start_ms")
    end_ms = as_int(source.get("end_ms"), f"{path}.source.end_ms")
    expected_sha256 = as_string(
        source.get("context_sha256"), f"{path}.source.context_sha256"
    )
    if end_ms < start_ms or end_ms >= epoch.cutoff_ms:
        raise ValueError(f"{path}.source: timestamps escape the knowledge epoch")
    outside_window = (
        team.window_end_ms is not None and epoch.cutoff_ms > team.window_end_ms
    )
    validation_team = (
        load_archived_team(archive, team.team_slug) if outside_window else team
    )
    current = _root_overview_input(validation_team, epoch)
    exact_match = (
        current.event_ids == event_ids
        and current.start_ms == start_ms
        and current.end_ms == end_ms
        and current.context_sha256 == expected_sha256
    )
    if not exact_match:
        reconstructed = _recorded_overview_input(
            validation_team, epoch, event_ids
        )
        recorded_evidence_unchanged = reconstructed is not None and (
            reconstructed.start_ms == start_ms
            and reconstructed.end_ms == end_ms
            and reconstructed.context_sha256 == expected_sha256
        )
        if recorded_evidence_unchanged and current.event_ids != event_ids:
            raise _ProjectOverviewSourceSetChanged(path, epoch)
        raise ValueError(
            f"{path}.source: frozen overview evidence was mutated or truncated"
        )
    return None if outside_window else current


def _glossary_evidence_from_json(
    value: JsonValue, where: str
) -> _GlossaryEvidence:
    obj = as_object(value, where)
    _require_exact_keys(
        obj, {"event_id", "thread_id", "at_ms", "kind", "context"}, where
    )
    return _GlossaryEvidence(
        event_id=as_string(obj.get("event_id"), f"{where}.event_id"),
        thread_id=as_string(obj.get("thread_id"), f"{where}.thread_id"),
        at_ms=as_int(obj.get("at_ms"), f"{where}.at_ms"),
        kind=as_string(obj.get("kind"), f"{where}.kind"),
        context=as_string(obj.get("context"), f"{where}.context"),
    )


def _frozen_term_knowledge(
    archive: Path,
    team: TeamData,
    project_epoch_id: str,
    current_cutoff_ms: int,
) -> tuple[int | None, dict[str, _TermKnowledge]]:
    """Load immutable per-term evidence from the preceding glossary generation."""

    path = _summary_root(archive, team.team_slug) / "glossary.json"
    if not path.is_file():
        return None, {}
    obj = as_object(read_json(path), str(path))
    if "schema_version" not in obj:
        return None, {}
    version = as_int(obj.get("schema_version"), f"{path}.schema_version")
    if version != _GLOSSARY_SCHEMA_VERSION:
        return None, {}
    _require_exact_keys(
        obj,
        {
            "schema_version",
            "project_overview_input_hash",
            "project_overview_epoch_id",
            "observed_through_ms",
            "terms",
        },
        str(path),
    )
    recorded_project_epoch = as_string(
        obj.get("project_overview_epoch_id"),
        f"{path}.project_overview_epoch_id",
    )
    observed_through_ms = as_int(
        obj.get("observed_through_ms"), f"{path}.observed_through_ms"
    )
    if recorded_project_epoch != project_epoch_id:
        return None, {}
    if observed_through_ms > current_cutoff_ms:
        raise ValueError(f"{path}: knowledge source frontier moved backwards")
    result: dict[str, _TermKnowledge] = {}
    for index, raw_term in enumerate(as_array(obj.get("terms"), f"{path}.terms")):
        where = f"{path}.terms[{index}]"
        term = as_object(raw_term, where)
        _require_exact_keys(
            term,
            {
                "term",
                "introduced_at_ms",
                "occurrences",
                "context",
                "week",
                "term_id",
                "available_at_ms",
                "definition",
                "definition_status",
                "definition_cutoff_ms",
                "definition_epoch_id",
                "definition_summary",
                "evidence",
            },
            where,
        )
        term_name = as_string(term.get("term"), f"{where}.term")
        term_id = as_string(term.get("term_id"), f"{where}.term_id")
        if term_id != glossary_term_id(term_name):
            raise ValueError(f"{where}.term_id: does not match the exact term")
        available_at_ms = as_int(
            term.get("available_at_ms"), f"{where}.available_at_ms"
        )
        definition_cutoff_ms = as_int(
            term.get("definition_cutoff_ms"), f"{where}.definition_cutoff_ms"
        )
        definition_epoch_id = as_string(
            term.get("definition_epoch_id"), f"{where}.definition_epoch_id"
        )
        if definition_cutoff_ms > current_cutoff_ms:
            raise ValueError(f"{where}: definition source frontier moved backwards")
        evidence = tuple(
            _glossary_evidence_from_json(item, f"{where}.evidence[{evidence_index}]")
            for evidence_index, item in enumerate(
                as_array(term.get("evidence"), f"{where}.evidence")
            )
        )
        if any(item.at_ms >= definition_cutoff_ms for item in evidence):
            raise ValueError(f"{where}.evidence: occurrence escapes definition cutoff")
        expected_epoch_id = _definition_epoch_id(
            term_id,
            project_epoch_id,
            definition_cutoff_ms,
            evidence,
        )
        if definition_epoch_id != expected_epoch_id:
            raise ValueError(f"{where}.definition_epoch_id: provenance mismatch")
        current_evidence = _definition_evidence(
            team,
            GlossaryTerm(
                term=term_name,
                introduced_at_ms=0,
                occurrences=0,
                context="",
                week="",
                term_id=term_id,
            ),
            cutoff_ms=definition_cutoff_ms,
        )
        if current_evidence != evidence:
            raise ValueError(
                f"{where}.evidence: frozen source was mutated or truncated"
            )
        if term_id in result:
            raise ValueError(f"{where}.term_id: duplicate glossary term")
        result[term_id] = _TermKnowledge(
            evidence=evidence,
            available_at_ms=available_at_ms,
            definition_cutoff_ms=definition_cutoff_ms,
            definition_epoch_id=definition_epoch_id,
        )
    return observed_through_ms, result


def _write_phase_data(
    archive: Path,
    team_slug: str,
    phases: Sequence[PhaseWindow],
    results: Mapping[str, SummaryResult],
) -> int:
    changed = 0
    for phase in phases:
        obj: dict[str, JsonValue] = {
            "phase_id": phase.phase_id,
            "agent_id": phase.agent_id,
            "start_ms": phase.start_ms,
            "end_ms": phase.end_ms,
            "summary": _summary_json(results[phase.summary_key]),
        }
        changed += int(
            write_json_if_changed(
                _summary_root(archive, team_slug) / "phases" / f"{phase.phase_id}.json",
                obj,
            )
        )
    return changed


def _period_key(period: Period) -> str:
    return period.key + ":" + period.kind


def _phases_in(period: Period, phases: Sequence[PhaseWindow]) -> list[PhaseWindow]:
    return [
        phase
        for phase in phases
        if phase.start_ms < period.end_ms and phase.end_ms > period.start_ms
    ]


def _result_line(result: SummaryResult, label: str, at_ms: int) -> str:
    bullets = " ".join(item.text for item in result.work_summary)
    return f"[{at_ms}] {label}: {result.phrase}. {result.paragraph} {bullets}".strip()


def _result_artifact_key(result: SummaryResult) -> str:
    provenance = result.artifact_provenance
    return provenance.artifact_id if provenance is not None else result.input_hash


def _summary_provenance_matches_job(
    provenance: SummaryArtifactProvenance, job: SummaryJob
) -> bool:
    if (
        provenance.logical_key != job.key
        or provenance.team_slug != job.team_slug
        or provenance.start_ms != job.start_ms
        or provenance.end_ms != job.end_ms
        or provenance.dependency_keys != job.dependency_keys
        or provenance.context_coverage != job.context_coverage
    ):
        return False
    return summary_input_hash_for_provenance(job, provenance) == provenance.input_hash


def _summary_matches_job(result: SummaryResult, job: SummaryJob) -> bool:
    """Return whether a cached result was derived from the current mechanical input."""

    provenance = result.artifact_provenance
    return (
        provenance is not None
        and provenance.input_hash == result.input_hash
        and _summary_provenance_matches_job(provenance, job)
    )


def _agent_name_provenance_matches_job(
    provenance: SummaryArtifactProvenance, job: AgentNameJob
) -> bool:
    if (
        provenance.logical_key != job.key
        or provenance.team_slug != job.team_slug
        or provenance.start_ms != job.start_ms
        or provenance.end_ms != job.end_ms
        or provenance.dependency_keys != job.dependency_keys
        or provenance.context_coverage != job.context_coverage
    ):
        return False
    return agent_name_input_hash_for_provenance(job, provenance) == provenance.input_hash


def _agent_name_matches_job(result: AgentNameResult, job: AgentNameJob) -> bool:
    """Return whether a cached lifetime summary covers the current agent input."""

    provenance = result.artifact_provenance
    return (
        provenance is not None
        and provenance.input_hash == result.input_hash
        and _agent_name_provenance_matches_job(provenance, job)
    )


def _summary_catalog_reference(
    result: SummaryResult | AgentNameResult, cache_directory: str
) -> SummaryArtifactReference:
    provenance = result.artifact_provenance
    if provenance is None:
        raise ValueError(
            f"model artifact {result.input_hash!r} lacks common provenance"
        )
    return SummaryArtifactReference(
        provenance=provenance,
        cache_path=f"{cache_directory}/{result.input_hash}.json",
    )


def _agent_name_jobs(
    team: TeamData,
    phases: Sequence[PhaseWindow],
    phase_results: Mapping[str, SummaryResult],
    context_chars: int = 16_000,
) -> tuple[AgentNameJob, ...]:
    """Build hindsight naming inputs after all phase-level work has been summarized."""

    agents_by_id = {agent.thread_id: agent for agent in team.agents}
    phases_by_agent: dict[str, list[PhaseWindow]] = {}
    for phase in phases:
        phases_by_agent.setdefault(phase.agent_id, []).append(phase)
    jobs: list[AgentNameJob] = []
    selected_ids = phase_agent_ids(team, phases)
    for agent in team.agents:
        if agent.thread_id not in selected_ids:
            continue
        own_phases = sorted(
            phases_by_agent.get(agent.thread_id, []),
            key=lambda phase: (phase.start_ms, phase.phase_id),
        )
        work_lines: list[str] = []
        for phase in own_phases:
            result = phase_results[phase.summary_key]
            work_lines.append(
                _result_line(result, phase.agent_label, phase.start_ms)
            )
        parent = (
            agents_by_id.get(agent.parent_thread_id)
            if agent.parent_thread_id is not None
            else None
        )
        if own_phases:
            job_start_ms = own_phases[0].start_ms
            job_end_ms = own_phases[-1].end_ms
        else:
            job_start_ms = max(
                agent.started_at_ms,
                team.window_start_ms or agent.started_at_ms,
            )
            natural_end_ms = agent.ended_at_ms or team.window_end_ms or job_start_ms
            job_end_ms = max(
                job_start_ms,
                min(natural_end_ms, team.window_end_ms or natural_end_ms),
            )
        jobs.append(
            AgentNameJob(
                key=f"agent-name:{agent.thread_id}",
                team_slug=team.team_slug,
                thread_id=agent.thread_id,
                start_ms=job_start_ms,
                end_ms=job_end_ms,
                official_path=agent.agent_path,
                coordinator_nickname=agent.nickname,
                role=agent.role,
                depth=agent.depth,
                parent_official_path=parent.agent_path if parent is not None else None,
                prior_context=own_phases[0].prior_context if own_phases else "",
                work_summary=(
                    "\n\n".join(work_lines)
                    if work_lines
                        else "No substantive phase summary was available for this thread."
                ),
                context_coverage=ContextCoverage(
                    components=(
                        ContextComponent(
                            "ancestor_transcript",
                            context_chars,
                            min(
                                context_chars,
                                len(own_phases[0].prior_context) if own_phases else 0,
                            ),
                            "characters",
                        ),
                        ContextComponent(
                            "phase_work_summaries",
                            len(own_phases),
                            len(own_phases),
                            "summaries",
                        ),
                    )
                ),
                dependency_keys=tuple(
                    _result_artifact_key(phase_results[phase.summary_key])
                    for phase in own_phases
                ),
            )
        )
    return tuple(jobs)


def _agent_name_json(
    job: AgentNameJob, result: AgentNameResult
) -> dict[str, JsonValue]:
    name: dict[str, JsonValue] = {
        "thread_id": result.thread_id,
        "short_name": result.short_name,
        "rationale": result.rationale,
        "lifetime_summary": result.lifetime_summary,
        "model": result.model,
        "prompt_version": result.prompt_version,
        "input_hash": result.input_hash,
        "generated_at": result.generated_at,
    }
    if result.artifact_provenance is not None:
        name["artifact_provenance"] = result.artifact_provenance.to_json_obj()
    return {
        "schema_version": 3,
        "agent": {
            "thread_id": job.thread_id,
            "official_path": job.official_path,
            "coordinator_nickname": job.coordinator_nickname,
            "role": job.role,
            "depth": job.depth,
            "parent_official_path": job.parent_official_path,
        },
        "name": name,
    }


def _write_agent_name_data(
    archive: Path,
    team_slug: str,
    jobs: Sequence[AgentNameJob],
    results: Mapping[str, AgentNameResult],
) -> int:
    changed = 0
    for job in jobs:
        result = results[job.thread_id]
        changed += int(
            write_json_if_changed(
                _summary_root(archive, team_slug)
                / "agents"
                / f"{job.thread_id}.json",
                _agent_name_json(job, result),
            )
        )
    return changed


def _nonempty_string(value: JsonValue, where: str) -> str:
    result = as_string(value, where).strip()
    if not result:
        raise ValueError(f"{where}: must not be empty")
    return result


def _select_catalog_agent_name_for_job(
    summary_root: Path,
    catalog: SummaryArtifactCatalog,
    agent: Agent,
    job: AgentNameJob,
) -> AgentNameResult:
    compatible = SummaryArtifactCatalog(
        team_slug=catalog.team_slug,
        records=tuple(
            reference
            for reference in catalog.records
            if reference.provenance.summarizer_id
            == AGENT_LIFETIME_SUMMARIZER.summarizer_id
            and _agent_name_provenance_matches_job(reference.provenance, job)
        ),
    )
    reference = select_summary_artifact(
        compatible,
        job.key,
        AGENT_LIFETIME_SUMMARIZER.summarizer_id,
        minimum_output_schema=2,
    )
    if reference is None:
        return _fallback_agent_name(agent)
    return _agent_name_from_catalog_reference(summary_root, reference, agent)


def _load_agent_names(
    archive: Path,
    team: TeamData,
    selected_ids: frozenset[str],
    jobs: Sequence[AgentNameJob],
    catalog: SummaryArtifactCatalog | None = None,
) -> dict[str, AgentNameResult]:
    selected_catalog = catalog or load_summary_catalog(
        _summary_root(archive, team.team_slug) / "artifacts.json",
        team.team_slug,
    )
    jobs_by_thread = {job.thread_id: job for job in jobs}
    if len(jobs_by_thread) != len(jobs):
        raise ValueError("duplicate agent-name job thread")
    results: dict[str, AgentNameResult] = {}
    for agent in team.agents:
        if agent.thread_id not in selected_ids:
            continue
        job = jobs_by_thread.get(agent.thread_id)
        if job is None:
            raise ValueError(f"missing agent-name job for {agent.thread_id!r}")
        path = (
            _summary_root(archive, team.team_slug)
            / "agents"
            / f"{agent.thread_id}.json"
        )
        if _summary_projection_missing(path):
            candidate = _select_catalog_agent_name_for_job(
                _summary_root(archive, team.team_slug),
                selected_catalog,
                agent,
                job,
            )
            results[agent.thread_id] = candidate
            continue
        root = as_object(read_json(path), str(path))
        schema_version = as_int(
            root.get("schema_version"), f"{path}.schema_version"
        )
        if schema_version not in {1, 2, 3}:
            raise ValueError(f"unsupported agent-name schema at {path}")
        identity = as_object(root.get("agent"), f"{path}.agent")
        recorded_thread = _nonempty_string(
            identity.get("thread_id"), f"{path}.agent.thread_id"
        )
        recorded_path = _nonempty_string(
            identity.get("official_path"), f"{path}.agent.official_path"
        )
        recorded_depth = as_int(identity.get("depth"), f"{path}.agent.depth")
        if (
            recorded_thread != agent.thread_id
            or recorded_path != agent.agent_path
            or recorded_depth != agent.depth
        ):
            raise ValueError(
                f"stale hindsight name metadata for {agent.agent_path}; run summarize"
            )
        raw_name = as_object(root.get("name"), f"{path}.name")
        prompt_version = _nonempty_string(
            raw_name.get("prompt_version"), f"{path}.name.prompt_version"
        )
        try:
            prompt_contract = summarizer_change_for_prompt(
                AGENT_LIFETIME_SUMMARIZER, prompt_version
            )
        except ValueError as error:
            raise ValueError(f"{path}: unregistered agent-lifetime version") from error
        if schema_version == 1:
            if prompt_contract.output_schema_version != 1:
                raise ValueError(
                    f"{path}: agent-name wrapper and prompt schemas disagree"
                )
            lifetime_summary: str | None = None
        else:
            if prompt_contract.output_schema_version < 2:
                raise ValueError(
                    f"{path}: agent-name wrapper and prompt schemas disagree"
                )
            lifetime_summary = _nonempty_string(
                clean_summary_prose(
                    _nonempty_string(
                        raw_name.get("lifetime_summary"),
                        f"{path}.name.lifetime_summary",
                    )
                ),
                f"{path}.name.lifetime_summary",
            )
        result = AgentNameResult(
            thread_id=_nonempty_string(
                raw_name.get("thread_id"), f"{path}.name.thread_id"
            ),
            short_name=_nonempty_string(
                raw_name.get("short_name"), f"{path}.name.short_name"
            ),
            rationale=_nonempty_string(
                raw_name.get("rationale"), f"{path}.name.rationale"
            ),
            lifetime_summary=lifetime_summary,
            model=_nonempty_string(raw_name.get("model"), f"{path}.name.model"),
            prompt_version=prompt_version,
            input_hash=_nonempty_string(
                raw_name.get("input_hash"), f"{path}.name.input_hash"
            ),
            generated_at=_nonempty_string(
                raw_name.get("generated_at"), f"{path}.name.generated_at"
            ),
            artifact_provenance=(
                SummaryArtifactProvenance.from_json_obj(
                    raw_name.get("artifact_provenance"),
                    f"{path}.name.artifact_provenance",
                )
                if "artifact_provenance" in raw_name
                else None
            ),
        )
        if result.thread_id != agent.thread_id:
            raise ValueError(f"agent-name result thread mismatch at {path}")
        provenance = result.artifact_provenance
        if provenance is not None and (
            provenance.summarizer_id != AGENT_LIFETIME_SUMMARIZER.summarizer_id
            or provenance.logical_key != f"agent-name:{agent.thread_id}"
            or provenance.team_slug != team.team_slug
            or provenance.input_hash != result.input_hash
            or provenance.model != result.model
            or provenance.prompt_version != result.prompt_version
        ):
            raise ValueError(f"{path}: agent-name provenance differs from result")
        if result.lifetime_summary is None:
            results[agent.thread_id] = result
        elif not _agent_name_matches_job(result, job):
            results[agent.thread_id] = _select_catalog_agent_name_for_job(
                _summary_root(archive, team.team_slug),
                selected_catalog,
                agent,
                job,
            )
        else:
            results[agent.thread_id] = result
    return results


def _rollup_jobs_for_level(
    team: TeamData,
    periods: Sequence[Period],
    phases: Sequence[PhaseWindow],
    phase_results: Mapping[str, SummaryResult],
    lower_periods: Sequence[Period],
    lower_results: Mapping[str, SummaryResult],
    completed_same_level: Sequence[tuple[Period, SummaryResult]],
    terms: Sequence[GlossaryTerm],
    summary_style: str = TECHNICAL_ROLLUP_STYLE,
    project_overview: SummaryResult | None = None,
    same_period_technical: Mapping[str, SummaryResult] | None = None,
) -> tuple[SummaryJob, ...]:
    jobs: list[SummaryJob] = []
    for period in periods:
        own_phases = _phases_in(period, phases)
        lower = [
            item
            for item in lower_periods
            if item.start_ms >= period.start_ms and item.end_ms <= period.end_ms
        ]
        transcript_parts = [
            (
                item.start_ms,
                _result_line(lower_results[_period_key(item)], item.label, item.start_ms),
            )
            for item in lower
        ]
        dependency_results = [lower_results[_period_key(item)] for item in lower]
        # Calendar levels do not nest perfectly: an ISO week can straddle a month. Summarize only
        # fully-contained lower periods and fill uncovered boundary time from phase summaries, so
        # January's monthly report never imports work from December or February.
        uncovered_phases = [
            phase
            for phase in own_phases
            if not any(
                item.start_ms <= phase.start_ms and phase.end_ms <= item.end_ms
                for item in lower
            )
        ]
        transcript_parts.extend(
            (
                phase.start_ms,
                _result_line(
                    phase_results[phase.summary_key], phase.agent_label, phase.start_ms
                ),
            )
            for phase in uncovered_phases
        )
        dependency_results.extend(
            phase_results[phase.summary_key] for phase in uncovered_phases
        )
        transcript = "\n\n".join(
            line for _, line in sorted(transcript_parts, key=lambda item: item[0])
        )
        prior = [
            (item, result)
            for item, result in completed_same_level
            if item.end_ms <= period.start_ms
        ][-10:]
        prior_context = "\n\n".join(
            _result_line(result, item.label, item.start_ms) for item, result in prior
        )
        dependency_results.extend(result for _, result in prior)
        stats = aggregate_stats(own_phases)
        plain_language = summary_style == PLAIN_LANGUAGE_ROLLUP_STYLE
        key_prefix = "rollup-plain" if plain_language else "rollup"
        audience = "Plain-language" if plain_language else "Technical"
        technical_result = (
            same_period_technical.get(_period_key(period))
            if plain_language and same_period_technical is not None
            else None
        )
        if plain_language and technical_result is None:
            raise ValueError(
                f"plain-language {period.kind} rollup {period.key!r} lacks "
                "its same-period technical summary"
            )
        glossary = glossary_prompt_text(
            [term for term in terms if term.summary_available_at_ms < period.end_ms]
        )
        if plain_language:
            glossary = plain_language_context_text(
                project_overview.paragraph if project_overview is not None else "",
                [
                    term
                    for term in terms
                    if term.summary_available_at_ms < period.end_ms
                ],
            )
            if project_overview is not None:
                dependency_results.append(project_overview)
            if technical_result is not None:
                dependency_results.append(technical_result)
        earliest_activity = min(
            (event.timestamp_ms for event in team.events),
            default=period.start_ms,
        )
        previous_period = completed_same_level[-1][0] if completed_same_level else None
        if period.start_ms <= earliest_activity < period.end_ms:
            frontier_status = "project-start"
        elif previous_period is not None and previous_period.end_ms == period.start_ms:
            frontier_status = "contiguous-extension"
        else:
            frontier_status = "isolated-backfill"
        expected_prior_summaries = 0
        if earliest_activity < period.start_ms:
            expected_prior_summaries = min(
                10,
                len(
                    periods_for_range(
                        earliest_activity,
                        period.start_ms - 1,
                        team.display_timezone,
                        team.team_slug,
                        (period.kind,),
                    )
                ),
            )
        jobs.append(
            SummaryJob(
                key=f"{key_prefix}:{period.kind}:{period.key}",
                team_slug=team.team_slug,
                agent_label=(
                    f"{audience} {period.kind} super-summary · {period.label}"
                ),
                start_ms=period.start_ms,
                end_ms=period.end_ms,
                prior_context=prior_context,
                transcript=transcript,
                glossary=glossary,
                stats=stats.to_mapping(),
                summary_style=summary_style,
                factual_context=(
                    _result_line(
                        technical_result,
                        f"Authoritative technical {period.kind} summary",
                        period.start_ms,
                    )
                    if technical_result is not None
                    else ""
                ),
                context_coverage=ContextCoverage(
                    components=(
                        ContextComponent(
                            "lower_level_summaries",
                            len(transcript_parts),
                            len(transcript_parts),
                            "summaries",
                        ),
                        ContextComponent(
                            "prior_same_level_summaries",
                            expected_prior_summaries,
                            min(expected_prior_summaries, len(prior)),
                            "summaries",
                        ),
                    )
                    + (
                        (ContextComponent("project_overview", 1, 1, "artifacts"),)
                        if plain_language and project_overview is not None
                        else ()
                    )
                    + (
                        (ContextComponent("technical_summary", 1, 1, "summaries"),)
                        if technical_result is not None
                        else ()
                    ),
                    frontier_status=frontier_status,
                    predecessor_keys=tuple(_period_key(item) for item, _ in prior),
                ),
                dependency_keys=tuple(
                    dict.fromkeys(_result_artifact_key(result) for result in dependency_results)
                ),
            )
        )
    return tuple(jobs)


def _accumulate_stats(stats: Sequence[SummaryRunStats]) -> SummaryRunStats:
    newly_spent = TokenUsage()
    artifact_generation = TokenUsage()
    for item in stats:
        newly_spent += item.newly_spent_usage
        artifact_generation += item.artifact_generation_usage
    return SummaryRunStats(
        hits=sum(item.hits for item in stats),
        misses=sum(item.misses for item in stats),
        batches=sum(item.batches for item in stats),
        newly_spent_usage=newly_spent,
        newly_spent_unknown_receipts=sum(
            item.newly_spent_unknown_receipts for item in stats
        ),
        artifact_generation_usage=artifact_generation,
        artifact_generation_unknown_receipts=sum(
            item.artifact_generation_unknown_receipts for item in stats
        ),
        unknown_legacy_artifacts=sum(
            item.unknown_legacy_artifacts for item in stats
        ),
    )


def _period_range(
    team: TeamData, phases: Sequence[PhaseWindow]
) -> tuple[int, int]:
    """Return an inclusive range for calendar rollups from half-open phase/window bounds."""

    phase_start = min((phase.start_ms for phase in phases), default=None)
    phase_end = max((phase.end_ms for phase in phases), default=None)
    start_ms = team.window_start_ms if team.window_start_ms is not None else phase_start
    end_ms = team.window_end_ms if team.window_end_ms is not None else phase_end
    if start_ms is None or end_ms is None:
        raise ValueError("selected timeline range contains no activity and lacks both date bounds")
    if end_ms <= start_ms:
        raise ValueError("selected timeline range is empty")
    return start_ms, end_ms - 1


def _summarize_archive_locked(
    archive: Path,
    team_slug: str,
    backend: str,
    model: str,
    *,
    max_workers: int = 3,
    batch_size: int = 6,
    name_batch_size: int = 12,
    phase_minutes: int = 30,
    context_chars: int = 16_000,
    transcript_chars: int = 30_000,
    codex_command: Sequence[str] = ("codex",),
    claude_command: Sequence[str] = ("claude",),
    reasoning_effort: str | None = None,
    service_tier: str | None = None,
    summary_window: DateWindow | None = None,
    rollup_kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
) -> SummarizeReport:
    """Fill only missing/changed structured summaries; never format the website."""

    service_tier = resolve_service_tier(backend, service_tier)
    team = load_archived_team(archive, team_slug)
    if summary_window is not None:
        team = apply_date_window(team, summary_window)
    summary_root = _summary_root(archive, team_slug)
    existing_summary_catalog = load_summary_catalog(
        summary_root / "artifacts.json", team_slug
    )
    changed = int(
        write_json_if_changed(
            summary_root / "summarizers.json",
            registry_json_obj(),
        )
    )
    phases = build_phases(
        team,
        phase_minutes=phase_minutes,
        context_chars=context_chars,
        transcript_chars=transcript_chars,
    )
    # Legacy schema-3 glossary records contain mechanically selected strings, not a supported
    # semantic project ontology. Until the bounded semantic discovery pipeline is complete, fail
    # closed: no candidate enters a model prompt and no glossary-specific model job can run.
    supported_terms: tuple[GlossaryTerm, ...] = ()
    cache = summary_root / "cache"
    try:
        overview_source = _frozen_project_overview_input(archive, team)
    except _ProjectOverviewSourceSetChanged as change:
        overview_source = _renewed_project_overview_input(
            team, change.previous_epoch
        )
    if overview_source is None:
        overview_source = _root_overview_input(team)
    phase_results, phase_stats = summarize_jobs(
        _phase_jobs(team, phases, supported_terms, context_chars),
        cache,
        backend,
        model,
        max_workers=max_workers,
        batch_size=batch_size,
        codex_command=codex_command,
        claude_command=claude_command,
        reasoning_effort=reasoning_effort,
        service_tier=service_tier,
    )
    changed += _write_phase_data(archive, team_slug, phases, phase_results)
    name_jobs = _agent_name_jobs(team, phases, phase_results, context_chars)
    agent_names, name_stats = name_agents(
        name_jobs,
        summary_root / "name_cache",
        backend,
        model,
        max_workers=max_workers,
        batch_size=name_batch_size,
        codex_command=codex_command,
        claude_command=claude_command,
        reasoning_effort=reasoning_effort,
        service_tier=service_tier,
    )
    changed += _write_agent_name_data(
        archive, team_slug, name_jobs, agent_names
    )
    overview_job = _project_overview_job(team, overview_source)
    overview_results, overview_stats = summarize_jobs(
        (overview_job,),
        cache,
        backend,
        model,
        max_workers=max_workers,
        batch_size=batch_size,
        codex_command=codex_command,
        claude_command=claude_command,
        reasoning_effort=reasoning_effort,
        service_tier=service_tier,
    )
    project_overview = overview_results[overview_job.key]
    _validate_project_overview(project_overview, "generated project overview")
    changed += _write_project_overview_data(
        archive, team_slug, overview_source, project_overview
    )
    definition_jobs: tuple[SummaryJob, ...] = ()
    definition_results: dict[str, SummaryResult] = {}
    enriched_terms = supported_terms

    start_ms, end_ms = _period_range(team, phases)
    periods = periods_for_range(
        start_ms,
        end_ms,
        team.display_timezone,
        team.team_slug,
        rollup_kinds,
    )
    selected_kinds = tuple(kind for kind in ROLLUP_KINDS if kind in rollup_kinds)
    by_kind = {
        kind: [period for period in periods if period.kind == kind]
        for kind in selected_kinds
    }
    all_results: dict[str, SummaryResult] = {}
    all_plain_results: dict[str, SummaryResult] = {}
    backend_stats: list[SummaryRunStats] = [
        phase_stats,
        name_stats,
        overview_stats,
    ]
    previous_periods: list[Period] = []
    previous_results: dict[str, SummaryResult] = {}
    previous_plain_results: dict[str, SummaryResult] = {}
    for kind in selected_kinds:
        current = by_kind[kind]
        first_start_ms = current[0].start_ms if current else start_ms
        completed = _prior_catalog_rollups(
            summary_root,
            existing_summary_catalog,
            kind,
            TECHNICAL_ROLLUP_STYLE,
            first_start_ms,
        )
        completed_plain = _prior_catalog_rollups(
            summary_root,
            existing_summary_catalog,
            kind,
            PLAIN_LANGUAGE_ROLLUP_STYLE,
            first_start_ms,
        )
        # Same-level context is an intentional chronology dependency: day N reads up to ten
        # earlier daily summaries (and likewise for weeks/months/quarters). Generate those jobs
        # one at a time so the actual prior summaries—not empty placeholders—enter the next
        # content hash and terminology context.
        for period in current:
            jobs = _rollup_jobs_for_level(
                team,
                (period,),
                phases,
                phase_results,
                previous_periods,
                previous_results,
                completed,
                supported_terms,
                TECHNICAL_ROLLUP_STYLE,
            )
            results, stats = summarize_jobs(
                jobs,
                cache,
                backend,
                model,
                max_workers=max_workers,
                batch_size=batch_size,
                codex_command=codex_command,
                claude_command=claude_command,
                reasoning_effort=reasoning_effort,
                service_tier=service_tier,
            )
            backend_stats.append(stats)
            result = results[jobs[0].key]
            all_results[_period_key(period)] = result
            completed.append((period, result))
            plain_jobs = _rollup_jobs_for_level(
                team,
                (period,),
                phases,
                phase_results,
                previous_periods,
                previous_plain_results,
                completed_plain,
                enriched_terms,
                PLAIN_LANGUAGE_ROLLUP_STYLE,
                project_overview,
                {_period_key(period): result},
            )
            plain_results, plain_stats = summarize_jobs(
                plain_jobs,
                cache,
                backend,
                model,
                max_workers=max_workers,
                batch_size=batch_size,
                codex_command=codex_command,
                claude_command=claude_command,
                reasoning_effort=reasoning_effort,
                service_tier=service_tier,
            )
            backend_stats.append(plain_stats)
            plain_result = plain_results[plain_jobs[0].key]
            all_plain_results[_period_key(period)] = plain_result
            completed_plain.append((period, plain_result))
        previous_periods = current
        previous_results = {
            _period_key(period): all_results[_period_key(period)] for period in current
        }
        previous_plain_results = {
            _period_key(period): all_plain_results[_period_key(period)]
            for period in current
        }

    for period in periods:
        result = all_results[_period_key(period)]
        plain_result = all_plain_results[_period_key(period)]
        obj: dict[str, JsonValue] = {
            "schema_version": 2,
            "kind": period.kind,
            "key": period.key,
            "start_ms": period.start_ms,
            "end_ms": period.end_ms,
            "partial": period.partial,
            "technical_summary": _summary_json(result),
            "plain_language_summary": _summary_json(plain_result),
        }
        changed += int(
            write_json_if_changed(
                _summary_root(archive, team_slug)
                / "rollups"
                / period.kind
                / f"{period.key}.json",
                obj,
            )
        )
    # Preserve historical schema-3 glossary bytes for provenance. The build path ignores that
    # mechanically selected projection, and no new legacy projection is written.
    summary_results = (
        tuple(phase_results.values())
        + (project_overview,)
        + tuple(definition_results.values())
        + tuple(all_results.values())
        + tuple(all_plain_results.values())
    )
    catalog_additions = tuple(
        _summary_catalog_reference(result, "cache") for result in summary_results
    ) + tuple(
        _summary_catalog_reference(result, "name_cache")
        for result in agent_names.values()
    )
    for reference in catalog_additions:
        cache_path = summary_root / reference.cache_path
        if not cache_path.is_file():
            raise ValueError(
                f"summary artifact cache entry is missing: {cache_path}"
            )
    catalog, catalog_changed = merge_summary_catalog(
        summary_root / "artifacts.json",
        team_slug,
        catalog_additions,
    )
    changed += int(catalog_changed)
    combined = _accumulate_stats(backend_stats)
    return SummarizeReport(
        backend=backend,
        model=model,
        reasoning_effort=reasoning_effort,
        service_tier=service_tier,
        phases=len(phases),
        rollups=len(periods),
        agent_names=len(agent_names),
        glossary_terms=len(supported_terms),
        project_overviews=1,
        glossary_definitions=len(definition_jobs),
        catalog_artifacts=len(catalog.records),
        cache_hits=combined.hits,
        cache_misses=combined.misses,
        backend_batches=combined.batches,
        newly_spent_usage=combined.newly_spent_usage,
        newly_spent_unknown_receipts=combined.newly_spent_unknown_receipts,
        artifact_generation_usage=combined.artifact_generation_usage,
        artifact_generation_unknown_receipts=(
            combined.artifact_generation_unknown_receipts
        ),
        unknown_legacy_artifacts=combined.unknown_legacy_artifacts,
        usage_run_paths=tuple(
            str(item.usage_run_path.relative_to(archive))
            for item in backend_stats
            if item.usage_run_path is not None
        ),
        files_changed=changed,
    )


def summarize_archive(
    archive: Path,
    team_slug: str,
    backend: str,
    model: str,
    *,
    max_workers: int = 3,
    batch_size: int = 6,
    name_batch_size: int = 12,
    phase_minutes: int = 30,
    context_chars: int = 16_000,
    transcript_chars: int = 30_000,
    codex_command: Sequence[str] = ("codex",),
    claude_command: Sequence[str] = ("claude",),
    reasoning_effort: str | None = None,
    service_tier: str | None = None,
    summary_window: DateWindow | None = None,
    rollup_kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
) -> SummarizeReport:
    """Fill structured summaries/names while serializing token-spending cache misses."""

    with archive_writer_lock(archive):
        return _summarize_archive_locked(
            archive,
            team_slug,
            backend,
            model,
            max_workers=max_workers,
            batch_size=batch_size,
            name_batch_size=name_batch_size,
            phase_minutes=phase_minutes,
            context_chars=context_chars,
            transcript_chars=transcript_chars,
            codex_command=codex_command,
            claude_command=claude_command,
            reasoning_effort=reasoning_effort,
            service_tier=service_tier,
            summary_window=summary_window,
            rollup_kinds=rollup_kinds,
        )


def _load_phase_summaries(
    archive: Path,
    team_slug: str,
    phases: Sequence[PhaseWindow],
    jobs: Sequence[SummaryJob],
    catalog: SummaryArtifactCatalog,
) -> dict[str, SummaryResult]:
    result: dict[str, SummaryResult] = {}
    summary_root = _summary_root(archive, team_slug)
    jobs_by_key = {job.key: job for job in jobs}
    if len(jobs_by_key) != len(jobs):
        raise ValueError("duplicate phase summary job key")
    for phase in phases:
        path = _summary_root(archive, team_slug) / "phases" / f"{phase.phase_id}.json"
        job = jobs_by_key.get(phase.summary_key)
        if job is None:
            raise ValueError(f"missing phase summary job {phase.summary_key!r}")
        fallback = _unavailable_summary(
            phase.summary_key,
            phase.end_ms,
            "work phase",
        )
        if _summary_projection_missing(path):
            candidate = _select_catalog_summary_for_job(
                summary_root,
                catalog,
                job,
                "phase-work-summary",
                fallback,
            )
        else:
            obj = as_object(read_json(path), str(path))
            _require_exact_keys(
                obj,
                {"phase_id", "agent_id", "start_ms", "end_ms", "summary"},
                str(path),
            )
            recorded_phase_id = as_string(obj.get("phase_id"), f"{path}.phase_id")
            recorded_agent_id = as_string(obj.get("agent_id"), f"{path}.agent_id")
            recorded_start_ms = as_int(obj.get("start_ms"), f"{path}.start_ms")
            recorded_end_ms = as_int(obj.get("end_ms"), f"{path}.end_ms")
            if (
                recorded_phase_id != phase.phase_id
                or recorded_agent_id != phase.agent_id
            ):
                raise ValueError(f"{path}: phase projection identity mismatch")
            projected = _summary_from_json(obj.get("summary"), str(path))
            if (
                recorded_start_ms != phase.start_ms
                or recorded_end_ms != phase.end_ms
            ):
                candidate = fallback
            else:
                candidate = projected
        if not _summary_matches_job(candidate, job):
            candidate = _select_catalog_summary_for_job(
                summary_root,
                catalog,
                job,
                "phase-work-summary",
                fallback,
            )
        result[phase.summary_key] = candidate
    return result


_PRESENTATION_FALLBACK_VERSION = "agent-team-timeline-presentation-fallback-v1"


def _fallback_generated_at(at_ms: int) -> str:
    return datetime.fromtimestamp(at_ms / 1000, tz=timezone.utc).isoformat().replace(
        "+00:00", "Z"
    )


def _unavailable_summary(key: str, at_ms: int, subject: str) -> SummaryResult:
    """Return an ephemeral build-only placeholder; never persist it as model output."""

    identity = hashlib.sha256(
        f"{_PRESENTATION_FALLBACK_VERSION}\0{key}\0{subject}".encode("utf-8")
    ).hexdigest()
    return SummaryResult(
        key=key,
        phrase="Summary unavailable",
        paragraph=(
            f"Summary unavailable: no cached model summary exists for this {subject}. "
            "The transcript and statistics shown here come directly from normalized logs."
        ),
        work_summary=(),
        model="none",
        prompt_version=_PRESENTATION_FALLBACK_VERSION,
        input_hash=identity,
        generated_at=_fallback_generated_at(at_ms),
        summary_available=False,
    )


def _fallback_agent_name(agent: Agent) -> AgentNameResult:
    leaf = agent.agent_path.rstrip("/").rsplit("/", 1)[-1]
    if agent.parent_thread_id is None:
        short_name = "Coordinator"
    else:
        readable = re.sub(r"[-_]+", " ", leaf).strip()
        short_name = (readable or "Unnamed agent")[:48].rstrip()
    identity = hashlib.sha256(
        (
            f"{_PRESENTATION_FALLBACK_VERSION}\0{agent.thread_id}\0"
            f"{agent.agent_path}"
        ).encode("utf-8")
    ).hexdigest()
    return AgentNameResult(
        thread_id=agent.thread_id,
        short_name=short_name,
        rationale=(
            "Mechanical name from the official agent path; no cached hindsight name "
            "is available."
        ),
        lifetime_summary=(
            "Summary unavailable: no cached agent-lifetime summary exists. "
            "Open a work phase to inspect normalized transcript and statistics."
        ),
        model="none",
        prompt_version=_PRESENTATION_FALLBACK_VERSION,
        input_hash=identity,
        generated_at=_fallback_generated_at(agent.ended_at_ms or agent.started_at_ms),
        summary_available=False,
    )


def _agent_name_from_catalog_reference(
    summary_root: Path,
    reference: SummaryArtifactReference,
    agent: Agent,
) -> AgentNameResult:
    """Recover a compatible paid lifetime result when its projection is absent."""

    path = summary_root / reference.cache_path
    root = as_object(read_json(path), str(path))
    provenance = reference.provenance
    if root.get("format") == ARTIFACT_ENVELOPE_FORMAT:
        _require_exact_keys(
            root,
            {"format", "schema_version", "artifact", "result"},
            str(path),
        )
        if as_int(root.get("schema_version"), f"{path}.schema_version") != (
            ARTIFACT_ENVELOPE_VERSION
        ):
            raise ValueError(f"{path}: unsupported model-artifact envelope")
        cached_provenance = SummaryArtifactProvenance.from_json_obj(
            root.get("artifact"), f"{path}.artifact"
        )
        if cached_provenance != provenance:
            raise ValueError(f"{path}: cache provenance differs from artifact catalog")
    else:
        if not provenance.legacy_storage:
            raise ValueError(f"{path}: cataloged artifact lacks its common envelope")
        _require_exact_keys(
            root,
            {"cache_version", "backend", "usage_receipt_id", "result"},
            str(path),
        )
        if as_string(root.get("backend"), f"{path}.backend") != provenance.backend:
            raise ValueError(f"{path}: legacy cache backend differs from catalog")

    where = f"{path}.result"
    raw = as_object(root.get("result"), where)
    _require_exact_keys(
        raw,
        {
            "thread_id",
            "short_name",
            "rationale",
            "lifetime_summary",
            "model",
            "prompt_version",
            "input_hash",
            "generated_at",
        },
        where,
    )
    prompt_version = _nonempty_string(
        raw.get("prompt_version"), f"{where}.prompt_version"
    )
    prompt_contract = summarizer_change_for_prompt(
        AGENT_LIFETIME_SUMMARIZER, prompt_version
    )
    if prompt_contract.output_schema_version < 2:
        raise ValueError(f"{path}: agent-lifetime artifact lacks a lifetime summary")
    result = AgentNameResult(
        thread_id=_nonempty_string(raw.get("thread_id"), f"{where}.thread_id"),
        short_name=_nonempty_string(raw.get("short_name"), f"{where}.short_name"),
        rationale=_nonempty_string(raw.get("rationale"), f"{where}.rationale"),
        lifetime_summary=_nonempty_string(
            clean_summary_prose(
                _nonempty_string(
                    raw.get("lifetime_summary"), f"{where}.lifetime_summary"
                )
            ),
            f"{where}.lifetime_summary",
        ),
        model=_nonempty_string(raw.get("model"), f"{where}.model"),
        prompt_version=prompt_version,
        input_hash=_nonempty_string(raw.get("input_hash"), f"{where}.input_hash"),
        generated_at=_nonempty_string(
            raw.get("generated_at"), f"{where}.generated_at"
        ),
        artifact_provenance=provenance,
    )
    if (
        result.thread_id != agent.thread_id
        or result.input_hash != provenance.input_hash
        or result.model != provenance.model
        or result.prompt_version != provenance.prompt_version
    ):
        raise ValueError(f"{path}: cached agent lifetime differs from provenance")
    return result


def _validate_legacy_project_overview(
    root: Mapping[str, JsonValue], path: Path, version: int
) -> None:
    """Distinguish a complete old projection from a truncated/corrupt file."""

    root_keys = {"schema_version", "style", "source", "summary"}
    if version == 2:
        root_keys.add("knowledge_epoch")
    _require_exact_keys(root, root_keys, str(path))
    if as_string(root.get("style"), f"{path}.style") != PROJECT_OVERVIEW_STYLE:
        raise ValueError(f"{path}: invalid project-overview style")
    if version == 2:
        _knowledge_epoch_from_json(
            root.get("knowledge_epoch"), f"{path}.knowledge_epoch"
        )
    source = as_object(root.get("source"), f"{path}.source")
    source_keys = {"event_ids", "start_ms", "end_ms", "context_sha256"}
    if version == 2:
        source_keys.add("transcript")
    _require_exact_keys(source, source_keys, f"{path}.source")
    for index, value in enumerate(
        as_array(source.get("event_ids"), f"{path}.source.event_ids")
    ):
        as_string(value, f"{path}.source.event_ids[{index}]")
    as_int(source.get("start_ms"), f"{path}.source.start_ms")
    as_int(source.get("end_ms"), f"{path}.source.end_ms")
    expected_sha256 = _nonempty_string(
        source.get("context_sha256"), f"{path}.source.context_sha256"
    )
    if version == 2:
        transcript = as_string(
            source.get("transcript"), f"{path}.source.transcript"
        )
        actual_sha256 = hashlib.sha256(transcript.encode("utf-8")).hexdigest()
        if actual_sha256 != expected_sha256:
            raise ValueError(f"{path}.source: transcript digest mismatch")
    summary = _summary_from_json(root.get("summary"), f"{path}.summary")
    _validate_project_overview(summary, f"{path}.summary")


def _load_project_overview(
    archive: Path,
    team: TeamData,
) -> tuple[SummaryResult, _ProjectOverviewInput | None]:
    path = _summary_root(archive, team.team_slug) / "project_overview.json"
    if _summary_projection_missing(path):
        fallback_source = _root_overview_input(team)
        return (
            _unavailable_summary(
                f"project-overview:{team.team_slug}",
                fallback_source.end_ms,
                "project overview",
            ),
            fallback_source,
        )
    root = as_object(read_json(path), str(path))
    version = as_int(root.get("schema_version"), f"{path}.schema_version")
    if version in {1, 2}:
        _validate_legacy_project_overview(root, path, version)
        fallback_source = _root_overview_input(team)
        return (
            _unavailable_summary(
                f"project-overview:{team.team_slug}",
                fallback_source.end_ms,
                "project overview stored with a legacy schema",
            ),
            fallback_source,
        )
    if version != _PROJECT_OVERVIEW_SCHEMA_VERSION:
        raise ValueError(f"{path}: unsupported project-overview schema; run summarize")
    summary = _summary_from_json(root.get("summary"), f"{path}.summary")
    _validate_project_overview(summary, f"{path}.summary")
    try:
        source = _frozen_project_overview_input(archive, team)
    except _ProjectOverviewSourceSetChanged:
        fallback_source = _root_overview_input(team)
        return (
            _unavailable_summary(
                f"project-overview:{team.team_slug}",
                fallback_source.end_ms,
                "stale project overview",
            ),
            fallback_source,
        )
    except ValueError as error:
        if "frozen overview evidence was mutated or truncated" not in str(error):
            raise
        fallback_source = _root_overview_input(team)
        return (
            _unavailable_summary(
                f"project-overview:{team.team_slug}",
                fallback_source.end_ms,
                "stale project overview",
            ),
            fallback_source,
        )
    if source is None:
        fallback_source = _root_overview_input(team)
        return (
            _unavailable_summary(
                f"project-overview:{team.team_slug}",
                fallback_source.end_ms,
                "project overview for this time slice",
            ),
            fallback_source,
        )
    return summary, source


def _validate_legacy_glossary(
    archive: Path,
    team_slug: str,
    project_overview: SummaryResult,
    project_epoch_id: str | None,
) -> tuple[GlossaryTerm, ...]:
    """Validate the retired schema-3 projection without granting publication authority."""

    suppress = project_overview.prompt_version == _PRESENTATION_FALLBACK_VERSION
    path = _summary_root(archive, team_slug) / "glossary.json"
    if _summary_projection_missing(path):
        return ()
    obj = as_object(read_json(path), str(path))
    _require_exact_keys(
        obj,
        {
            "schema_version",
            "project_overview_input_hash",
            "project_overview_epoch_id",
            "observed_through_ms",
            "terms",
        },
        str(path),
    )
    if as_int(obj.get("schema_version"), f"{path}.schema_version") != (
        _GLOSSARY_SCHEMA_VERSION
    ):
        raise ValueError(f"{path}: unsupported glossary schema; run summarize")
    recorded_overview_hash = as_string(
        obj.get("project_overview_input_hash"),
        f"{path}.project_overview_input_hash",
    )
    if not suppress and recorded_overview_hash != project_overview.input_hash:
        raise ValueError(f"{path}: glossary used a stale project overview; run summarize")
    recorded_project_epoch_id = as_string(
        obj.get("project_overview_epoch_id"),
        f"{path}.project_overview_epoch_id",
    )
    if (
        not suppress
        and project_epoch_id is not None
        and recorded_project_epoch_id != project_epoch_id
    ):
        raise ValueError(f"{path}: glossary used a different knowledge epoch")
    project_epoch_id = recorded_project_epoch_id
    as_int(obj.get("observed_through_ms"), f"{path}.observed_through_ms")
    for index, raw in enumerate(as_array(obj.get("terms"), f"{path}.terms")):
        where = f"{path}.terms[{index}]"
        item = as_object(raw, where)
        _require_exact_keys(
            item,
            {
                "term",
                "introduced_at_ms",
                "occurrences",
                "context",
                "week",
                "term_id",
                "available_at_ms",
                "definition",
                "definition_status",
                "definition_cutoff_ms",
                "definition_epoch_id",
                "definition_summary",
                "evidence",
            },
            where,
        )
        term = as_string(item.get("term"), f"{where}.term")
        expected_id = glossary_term_id(term)
        term_id = as_string(item.get("term_id"), f"{where}.term_id")
        if term_id != expected_id:
            raise ValueError(
                f"{path}: glossary ID {term_id!r} does not match term {term!r}"
            )
        definition = as_string(item.get("definition"), f"{where}.definition")
        definition_status = as_string(
            item.get("definition_status"), f"{where}.definition_status"
        )
        if definition_status not in {"supported", "insufficient-evidence"}:
            raise ValueError(f"{where}: invalid definition status")
        as_int(item.get("available_at_ms"), f"{where}.available_at_ms")
        as_int(item.get("introduced_at_ms"), f"{where}.introduced_at_ms")
        as_int(item.get("occurrences"), f"{where}.occurrences")
        as_string(item.get("context"), f"{where}.context")
        as_string(item.get("week"), f"{where}.week")
        definition_cutoff_ms = as_int(
            item.get("definition_cutoff_ms"), f"{where}.definition_cutoff_ms"
        )
        definition_epoch_id = as_string(
            item.get("definition_epoch_id"), f"{where}.definition_epoch_id"
        )
        definition_summary = _summary_from_json(
            item.get("definition_summary"), f"{where}.definition_summary"
        )
        _validate_glossary_definition(
            definition_summary, f"{where}.definition_summary"
        )
        if (
            definition_summary.key != f"glossary-definition:{term_id}"
            or definition_summary.paragraph != definition
            or _definition_status(definition_summary) != definition_status
        ):
            raise ValueError(f"{where}: definition provenance does not match entry")
        parsed_evidence: list[_GlossaryEvidence] = []
        for evidence_index, raw_evidence in enumerate(
            as_array(item.get("evidence"), f"{where}.evidence")
        ):
            evidence_where = f"{where}.evidence[{evidence_index}]"
            evidence = as_object(raw_evidence, evidence_where)
            _require_exact_keys(
                evidence,
                {"event_id", "thread_id", "at_ms", "kind", "context"},
                evidence_where,
            )
            as_string(evidence.get("event_id"), f"{evidence_where}.event_id")
            as_string(evidence.get("thread_id"), f"{evidence_where}.thread_id")
            as_int(evidence.get("at_ms"), f"{evidence_where}.at_ms")
            as_string(evidence.get("kind"), f"{evidence_where}.kind")
            as_string(evidence.get("context"), f"{evidence_where}.context")
            parsed_evidence.append(
                _glossary_evidence_from_json(raw_evidence, evidence_where)
            )
        if any(
            evidence.at_ms >= definition_cutoff_ms for evidence in parsed_evidence
        ):
            raise ValueError(f"{where}.evidence: occurrence escapes definition cutoff")
        expected_definition_epoch = _definition_epoch_id(
            term_id,
            project_epoch_id,
            definition_cutoff_ms,
            parsed_evidence,
        )
        if definition_epoch_id != expected_definition_epoch:
            raise ValueError(f"{where}: definition provenance does not match evidence")
    # Schema 3 proves only that a model could define each mechanically selected string. It does
    # not classify the string as a durable project concept, so publishing it would turn ordinary
    # prose and workflow language into site-wide links. Keep validating the immutable projection
    # above for provenance/integrity, but fail closed until a semantic discovery projection exists.
    return ()


def _load_glossary(
    archive: Path,
    team_slug: str,
    project_overview: SummaryResult,
    project_epoch_id: str | None,
) -> tuple[GlossaryTerm, ...]:
    """Return supported semantic concepts, never legacy mechanical candidates."""

    # Keep these arguments in the stable loader boundary for the eventual semantic projection,
    # whose provenance will be checked against the project overview and knowledge epoch.
    _ = project_overview, project_epoch_id
    path = _summary_root(archive, team_slug) / "semantic_glossary.json"
    if _summary_projection_missing(path):
        return ()
    raise ValueError(f"{path}: semantic glossary projection is not supported by this version")


def _load_rollup_projection(
    path: Path, period: Period
) -> tuple[SummaryResult, SummaryResult, bool]:
    """Strictly validate one presentation projection before it can be suppressed."""

    obj = as_object(read_json(path), str(path))
    _require_exact_keys(
        obj,
        {
            "schema_version",
            "kind",
            "key",
            "start_ms",
            "end_ms",
            "partial",
            "technical_summary",
            "plain_language_summary",
        },
        str(path),
    )
    if as_int(obj.get("schema_version"), f"{path}.schema_version") != 2:
        raise ValueError(
            f"{path}: rollup lacks a plain-language summary; run summarize"
        )
    if (
        as_string(obj.get("kind"), f"{path}.kind") != period.kind
        or as_string(obj.get("key"), f"{path}.key") != period.key
        or as_int(obj.get("start_ms"), f"{path}.start_ms") != period.start_ms
        or as_int(obj.get("end_ms"), f"{path}.end_ms") != period.end_ms
    ):
        raise ValueError(f"{path}: rollup projection identity mismatch")
    recorded_partial = obj.get("partial")
    if not isinstance(recorded_partial, bool):
        raise ValueError(f"{path}.partial: expected a boolean")
    return (
        _summary_from_json(
            obj.get("technical_summary"), f"{path}.technical_summary"
        ),
        _summary_from_json(
            obj.get("plain_language_summary"),
            f"{path}.plain_language_summary",
        ),
        recorded_partial,
    )


def _validate_rollup_inputs(
    team: TeamData,
    periods: Sequence[Period],
    phases: Sequence[PhaseWindow],
    phase_results: Mapping[str, SummaryResult],
    project_overview: SummaryResult,
    terms: Sequence[GlossaryTerm],
    technical_candidates: Mapping[str, SummaryResult],
    plain_candidates: Mapping[str, SummaryResult],
    suppressed_keys: frozenset[str],
    summary_root: Path,
    catalog: SummaryArtifactCatalog,
) -> tuple[dict[str, SummaryResult], dict[str, SummaryResult]]:
    """Fail closed when current rollup inputs differ from cached model inputs."""

    selected_kinds = tuple(
        kind for kind in ROLLUP_KINDS if any(period.kind == kind for period in periods)
    )
    by_kind = {
        kind: [period for period in periods if period.kind == kind]
        for kind in selected_kinds
    }
    validated: dict[str, SummaryResult] = {}
    validated_plain: dict[str, SummaryResult] = {}
    previous_periods: list[Period] = []
    previous_results: dict[str, SummaryResult] = {}
    previous_plain_results: dict[str, SummaryResult] = {}
    for kind in selected_kinds:
        current = by_kind[kind]
        first_start_ms = current[0].start_ms
        completed = _prior_catalog_rollups(
            summary_root,
            catalog,
            kind,
            TECHNICAL_ROLLUP_STYLE,
            first_start_ms,
        )
        completed_plain = _prior_catalog_rollups(
            summary_root,
            catalog,
            kind,
            PLAIN_LANGUAGE_ROLLUP_STYLE,
            first_start_ms,
        )
        for period in current:
            key = _period_key(period)
            technical_job = _rollup_jobs_for_level(
                team,
                (period,),
                phases,
                phase_results,
                previous_periods,
                previous_results,
                completed,
                terms,
                TECHNICAL_ROLLUP_STYLE,
            )[0]
            technical = technical_candidates[key]
            if not _summary_matches_job(technical, technical_job):
                technical_fallback = _unavailable_summary(
                    technical_job.key,
                    period.end_ms,
                    f"stale {period.kind} rollup",
                )
                technical = (
                    technical_fallback
                    if key in suppressed_keys
                    else _select_catalog_summary_for_job(
                        summary_root,
                        catalog,
                        technical_job,
                        "technical-rollup",
                        technical_fallback,
                    )
                )
            validated[key] = technical
            completed.append((period, technical))

            plain_job = _rollup_jobs_for_level(
                team,
                (period,),
                phases,
                phase_results,
                previous_periods,
                previous_plain_results,
                completed_plain,
                terms,
                PLAIN_LANGUAGE_ROLLUP_STYLE,
                project_overview,
                {key: technical},
            )[0]
            plain = plain_candidates[key]
            if not _summary_matches_job(plain, plain_job):
                plain_fallback = _unavailable_summary(
                    plain_job.key,
                    period.end_ms,
                    f"stale plain-language {period.kind} rollup",
                )
                plain = (
                    plain_fallback
                    if key in suppressed_keys
                    else _select_catalog_summary_for_job(
                        summary_root,
                        catalog,
                        plain_job,
                        "plain-language-rollup",
                        plain_fallback,
                    )
                )
            validated_plain[key] = plain
            completed_plain.append((period, plain))
        previous_periods = current
        previous_results = {
            _period_key(period): validated[_period_key(period)] for period in current
        }
        previous_plain_results = {
            _period_key(period): validated_plain[_period_key(period)]
            for period in current
        }
    return validated, validated_plain


def _build_archive_locked(
    archive: Path,
    team_slug: str,
    *,
    phase_minutes: int = 30,
    display_window: DateWindow | None = None,
    rollup_kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
    output: Path | None = None,
    _precompress: bool = True,
    _published: bool = True,
) -> dict[str, int]:
    """Regenerate Markdown/HTML/JSON exclusively from cached structured data."""

    target = output or archive
    _ensure_archive(target, team_slug, create=output is not None)
    team = load_archived_team(archive, team_slug)
    if display_window is not None:
        team = apply_date_window(team, display_window)
    summary_root = _summary_root(archive, team_slug)
    summary_catalog = load_summary_catalog(
        summary_root / "artifacts.json", team_slug
    )
    phases = build_phases(team, phase_minutes=phase_minutes)
    phase_jobs = _phase_jobs(team, phases, (), 16_000)
    phase_results = _load_phase_summaries(
        archive, team_slug, phases, phase_jobs, summary_catalog
    )
    agent_name_jobs = _agent_name_jobs(team, phases, phase_results)
    agent_names = _load_agent_names(
        archive,
        team,
        phase_agent_ids(team, phases),
        agent_name_jobs,
        summary_catalog,
    )
    project_overview, project_overview_source = _load_project_overview(archive, team)
    start_ms, end_ms = _period_range(team, phases)
    periods = periods_for_range(
        start_ms,
        end_ms,
        team.display_timezone,
        team.team_slug,
        rollup_kinds,
    )
    rollup_results: dict[str, SummaryResult] = {}
    plain_rollup_results: dict[str, SummaryResult] = {}
    rollup_stats: dict[str, PhaseStats] = {}
    suppressed_rollup_keys: set[str] = set()
    for period in periods:
        path = (
            _summary_root(archive, team_slug)
            / "rollups"
            / period.kind
            / f"{period.key}.json"
        )
        key = _period_key(period)
        rollup_stats[key] = aggregate_stats(_phases_in(period, phases))
        projection_missing = _summary_projection_missing(path)
        if display_window is not None and period.partial:
            suppressed_rollup_keys.add(key)
            if not projection_missing:
                _load_rollup_projection(path, period)
            rollup_results[key] = _unavailable_summary(
                f"rollup:{period.kind}:{period.key}",
                period.end_ms,
                f"partial {period.kind} rollup",
            )
            plain_rollup_results[key] = _unavailable_summary(
                f"rollup-plain:{period.kind}:{period.key}",
                period.end_ms,
                f"partial plain-language {period.kind} rollup",
            )
            continue
        if projection_missing:
            rollup_results[key] = _select_catalog_summary(
                summary_root,
                summary_catalog,
                f"rollup:{period.kind}:{period.key}",
                "technical-rollup",
                _unavailable_summary(
                    f"rollup:{period.kind}:{period.key}",
                    period.end_ms,
                    f"{period.kind} rollup",
                ),
            )
            plain_rollup_results[key] = _select_catalog_summary(
                summary_root,
                summary_catalog,
                f"rollup-plain:{period.kind}:{period.key}",
                "plain-language-rollup",
                _unavailable_summary(
                    f"rollup-plain:{period.kind}:{period.key}",
                    period.end_ms,
                    f"plain-language {period.kind} rollup",
                ),
            )
            continue
        projected_technical, projected_plain, recorded_partial = (
            _load_rollup_projection(path, period)
        )
        if recorded_partial != period.partial:
            suppressed_rollup_keys.add(key)
            rollup_results[key] = _unavailable_summary(
                f"rollup:{period.kind}:{period.key}",
                period.end_ms,
                f"stale {period.kind} rollup",
            )
            plain_rollup_results[key] = _unavailable_summary(
                f"rollup-plain:{period.kind}:{period.key}",
                period.end_ms,
                f"stale plain-language {period.kind} rollup",
            )
            continue
        rollup_results[key] = _select_catalog_summary(
            summary_root,
            summary_catalog,
            f"rollup:{period.kind}:{period.key}",
            "technical-rollup",
            projected_technical,
        )
        plain_rollup_results[key] = _select_catalog_summary(
            summary_root,
            summary_catalog,
            f"rollup-plain:{period.kind}:{period.key}",
            "plain-language-rollup",
            projected_plain,
        )
    terms = _load_glossary(
        archive,
        team_slug,
        project_overview,
        (
            project_overview_source.epoch.epoch_id
            if project_overview_source is not None
            else None
        ),
    )
    rollup_results, plain_rollup_results = _validate_rollup_inputs(
        team,
        periods,
        phases,
        phase_results,
        project_overview,
        terms,
        rollup_results,
        plain_rollup_results,
        frozenset(suppressed_rollup_keys),
        summary_root,
        summary_catalog,
    )
    artifact_catalog = load_artifact_catalog(archive, team_slug, team)
    pull_cache = load_pull_request_metadata_cache(
        pull_metadata_path(archive, team_slug)
    )
    pull_metadata = {record.key: record for record in pull_cache.records}
    site_identity = load_site_identity(archive, team)
    return render_archive(
        target,
        team,
        phases,
        phase_results,
        periods,
        rollup_results,
        plain_rollup_results,
        rollup_stats,
        terms,
        project_overview,
        agent_names,
        pull_metadata,
        artifact_catalog,
        site_identity,
        _precompress=_precompress,
        _published=_published,
        _export_window=display_window,
    )


def build_archive(
    archive: Path,
    team_slug: str,
    *,
    phase_minutes: int = 30,
    display_window: DateWindow | None = None,
    rollup_kinds: tuple[str, ...] = DEFAULT_ROLLUP_KINDS,
    output: Path | None = None,
    _precompress: bool = True,
    _published: bool = True,
) -> dict[str, int]:
    """Regenerate one site while serializing its source snapshot and output files."""

    target = output or archive
    with _archive_writer_locks(archive, target):
        return _build_archive_locked(
            archive,
            team_slug,
            phase_minutes=phase_minutes,
            display_window=display_window,
            rollup_kinds=rollup_kinds,
            output=output,
            _precompress=_precompress,
            _published=_published,
        )


def _record_run_locked(
    archive: Path,
    command: Sequence[str],
    started_at: str,
    status: str,
    team_slug: str,
    ingest: IngestReport | None,
    summaries: SummarizeReport | None,
    build: Mapping[str, int] | None,
    error: str | None = None,
    *,
    team_slugs: Sequence[str] = (),
    mechanical: Mapping[str, JsonValue] | None = None,
) -> Path:
    """Append immutable run provenance and update the small archive manifest."""

    recorded_teams = tuple(sorted(set(team_slugs or (team_slug,))))
    if team_slug not in recorded_teams:
        raise ValueError("primary run team must be present in team_slugs")
    for recorded_team in recorded_teams:
        _validate_team_slug(recorded_team)
    _ensure_archive(archive, team_slug, create=True)
    completed_at = utc_now()
    stamp = completed_at.replace("-", "").replace(":", "").replace(".", "")
    stamp = stamp.replace("+0000", "Z")
    material = "\0".join(command) + "\0" + completed_at
    run_id = stamp + "-" + hashlib.sha256(material.encode("utf-8")).hexdigest()[:8]
    run_obj: dict[str, JsonValue] = {
        "schema_version": 1,
        "run_id": run_id,
        "tool_version": __version__,
        "started_at": started_at,
        "completed_at": completed_at,
        "status": status,
        "team_slug": team_slug,
        "command": list(command),
        "ingest": ingest.to_json_obj() if ingest else None,
        "summaries": summaries.to_json_obj() if summaries else None,
        "build": {key: value for key, value in sorted((build or {}).items())},
        "error": error,
    }
    if mechanical is not None:
        run_obj["mechanical"] = {
            key: value for key, value in sorted(mechanical.items())
        }
    if len(recorded_teams) > 1:
        run_obj["team_slugs"] = list(recorded_teams)
    path = archive / "runs" / f"{run_id}.json"
    write_json_if_changed(path, run_obj)
    manifest_path = archive / "manifest.json"
    created_at = started_at
    run_count = 0
    old: dict[str, JsonValue] = {}
    if manifest_path.is_file():
        old = as_object(read_json(manifest_path), str(manifest_path))
        old_created = old.get("created_at")
        if isinstance(old_created, str):
            created_at = old_created
        old_count = old.get("run_count")
        if isinstance(old_count, int) and not isinstance(old_count, bool):
            run_count = old_count
    manifest: dict[str, JsonValue] = dict(old)
    old_teams = old.get("teams")
    teams = (
        {value for value in old_teams if isinstance(value, str)}
        if isinstance(old_teams, list)
        else set()
    )
    teams.update(recorded_teams)
    team_values: list[JsonValue] = []
    for team in sorted(teams):
        team_values.append(team)
    manifest.update({
        "schema_version": 1,
        "tool_version": __version__,
        "created_at": created_at,
        "last_run_at": completed_at,
        "last_run_id": run_id,
        "last_run_status": status,
        "run_count": run_count + 1,
        "teams": team_values,
    })
    if ingest is not None:
        manifest["latest_source_digest"] = ingest.source_digest
    write_json_if_changed(manifest_path, manifest)
    return path


def record_run(
    archive: Path,
    command: Sequence[str],
    started_at: str,
    status: str,
    team_slug: str,
    ingest: IngestReport | None,
    summaries: SummarizeReport | None,
    build: Mapping[str, int] | None,
    error: str | None = None,
    *,
    team_slugs: Sequence[str] = (),
    mechanical: Mapping[str, JsonValue] | None = None,
) -> Path:
    """Append run provenance as one serialized manifest transaction."""

    with _archive_writer_locks(archive):
        return _record_run_locked(
            archive,
            command,
            started_at,
            status,
            team_slug,
            ingest,
            summaries,
            build,
            error,
            team_slugs=team_slugs,
            mechanical=mechanical,
        )


__all__ = [
    "IngestReport",
    "SummarizeReport",
    "build_archive",
    "extract_transcripts_archive",
    "ingest_claude",
    "ingest_claude_history",
    "ingest_codex",
    "ingest_orc",
    "load_archived_team",
    "record_run",
    "rehydrate_tool_payloads",
    "source_digest",
    "summarize_archive",
    "utc_now",
]
