"""Command-line interface for durable agent-team timeline archives."""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections.abc import Sequence
from importlib.resources import files
from pathlib import Path

from wrkviz import __version__
from wrkviz.build_store import (
    format_migration_report as format_build_migration_report,
    ingested_team_slugs,
    migrate_build_state,
)
from wrkviz.archive import as_array, as_object, read_json
from wrkviz.archive_gc import collect, format_gc_report
from wrkviz.claude import ClaudeParseError
from wrkviz.codex import CodexParseError
from wrkviz.github_enrich import PullMetadataReport, enrich_pull_request_metadata
from wrkviz.glossary_audit import (
    audit_legacy_glossaries,
    format_glossary_audit,
)
from wrkviz.identity import IdentityOverrides, parse_identity_overrides
from wrkviz.losslessness import (
    audit_archive_losslessness,
    format_losslessness_audit,
)
from wrkviz.multi_team import build_combined_archive
from wrkviz.naming import AgentNameError
from wrkviz.orc import OrcContinuationSpec, OrcParseError
from wrkviz.pipeline import (
    IngestReport,
    SummarizeReport,
    archive_writer_lock,
    build_archive,
    extract_transcripts_archive,
    ingest_claude,
    ingest_claude_history,
    ingest_codex,
    ingest_orc,
    load_archived_team,
    record_run,
    summarize_archive,
    utc_now,
)
from wrkviz.project_config import (
    ProjectIngestConfig,
    ingest_project,
    load_project_ingest_config,
)
from wrkviz.query import (
    QueryFilters,
    SEARCH_CORPORA,
    SEARCH_LINKAGES,
    SEARCH_MATCH_MODES,
    SEARCH_PROMPT_AUTHORS,
    SEARCH_ROLES,
    SEARCH_SORTS,
    TimelineQuery,
    TranscriptQuery,
    archive_stats,
    format_query,
    format_search_results,
    format_stats,
    parse_ordinal_range,
    schema_3_completeness,
    schema_3_record_counts,
)
from wrkviz.server import serve
from wrkviz.snapshot_store import (
    format_migration_report,
    migrate_snapshots,
    pointer_summary,
)
from wrkviz.summarize import SummaryError
from wrkviz.token_usage import TokenUsage
from wrkviz.transcript_export import TranscriptExportReport
from wrkviz.window import DateWindow, parse_date_window

PROG = "wrkviz"
DEFAULT_MODEL = os.environ.get("WRKVIZ_MODEL", "gpt-5.5")
DEFAULT_REASONING_EFFORT = os.environ.get(
    "WRKVIZ_REASONING_EFFORT", "medium"
)


def _load_userguide() -> str:
    return (files("wrkviz") / "USER_GUIDE.md").read_text(encoding="utf-8")


def _quickstart() -> str:
    return f"""{PROG} v{__version__}

Build a version-controllable, multi-resolution website from an agent team's append-only logs.

1. Ingest, summarize only cache misses, and build the site:

   {PROG} refresh \\
     --root-session SESSION_UUID --team example-team \\
     --output ./timelines/example-team \\
     --timezone America/New_York --model {DEFAULT_MODEL}

2. Open it locally:

   cd ./timelines/example-team
   make serve
   # http://127.0.0.1:8765/

3. Rerun the same refresh later. New log records are normalized; content-addressed summaries are
   reused; only changed time windows and calendar rollups spend model tokens.

The stages are separate on purpose: `ingest`, `summarize`, and `build`. The `build` command never
calls a model, so formatting and UI changes can be regenerated for free.
"""


def _path(raw: str) -> Path:
    return Path(raw).expanduser()


def _orc_continuation_arg(raw: str) -> OrcContinuationSpec:
    value: object = raw
    if raw.lstrip().startswith("{"):
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as error:
            raise ValueError(
                f"invalid --continuation-session JSON: {error.msg}"
            ) from error
    spec = OrcContinuationSpec.from_value(value, "--continuation-session")
    if not isinstance(value, str) and spec.start_message_id is None:
        raise ValueError(
            "--continuation-session.start_message_id: expected a non-empty string"
        )
    return spec


def _add_archive(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--output", required=True, help="version-controllable archive directory")
    parser.add_argument("--team", required=True, help="stable team slug, for example example-team")


def _add_build_root(parser: argparse.ArgumentParser) -> None:
    """Offer the build store's location, on the same commands that can create one.

    The counterpart to `--snapshot-root`, but offered on `migrate-build-state` **only**, which is
    the difference worth explaining. Snapshot resolution takes the requested root as an argument
    and hands it to the one function every provider addresses through, so a flag on `ingest` has
    somewhere to go. Build-state paths are resolved independently at a dozen call sites deep
    inside the pipeline, and the thing that makes them agree is the recorded pointer, not a
    parameter -- so the way to put the store somewhere else is to establish it there, which is
    what `migrate-build-state --build-root DIR --move` does. It writes the pointer even when there
    is nothing to move, so it is also how a fresh archive is told where its store goes.

    Accepting the flag on `ingest` and ignoring it would be the worse option: a flag that is
    accepted and has no effect is indistinguishable from one that worked.
    """

    parser.add_argument(
        "--build-root",
        dest="build_root",
        default=None,
        metavar="DIR",
        help=(
            "where to keep intermediate build state -- the normalized team, the artifact "
            "catalogue, the provenance manifests, the payload store -- which is what a rebuild "
            "reads and what nothing else does (default: <output>.build, a sibling of the "
            "archive; an archive that already keeps it inside itself keeps doing so until "
            "`migrate-build-state` is run)"
        ),
    )


def _add_snapshot_root(parser: argparse.ArgumentParser) -> None:
    """Offer the store location on every command that can create one.

    Named `--snapshot-root` rather than `--snapshot-dir` because it is the root of a store holding
    every team, not one team's directory: the team is a slug-named subdirectory inside it, so an
    archive with twelve teams sets this once. Offered on the ingest and refresh commands and not
    on `build`, `query` or `serve`, because those never create a store -- they read the layout the
    archive already recorded, and a flag they would have to ignore is worse than no flag.
    """

    parser.add_argument(
        "--snapshot-root",
        dest="snapshot_root",
        default=None,
        metavar="DIR",
        help=(
            "where to keep the vendor source snapshots, which are ingest input rather than "
            "published output and are the largest thing here (default: <output>.sources, a "
            "sibling of the archive; an archive that already keeps them inside itself keeps "
            "doing so until `migrate-snapshots` is run)"
        ),
    )


def _add_date_window(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--timezone",
        default="America/New_York",
        help="IANA display/calendar timezone (UTC instants remain canonical)",
    )
    start = parser.add_mutually_exclusive_group()
    start.add_argument(
        "--start-date",
        help="first local calendar date to include (YYYY-MM-DD, inclusive)",
    )
    start.add_argument(
        "--start-time",
        help="first instant to include (RFC3339 with offset or Z, inclusive)",
    )
    end = parser.add_mutually_exclusive_group()
    end.add_argument(
        "--end-date",
        help="local calendar boundary to stop at (YYYY-MM-DD, exclusive)",
    )
    end.add_argument(
        "--end-time",
        help="instant to stop at (RFC3339 with offset or Z, exclusive)",
    )


def _add_site_identity(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--project",
        action="append",
        default=[],
        metavar="LABEL=REPOSITORY_URL",
        help=(
            "project/repository identity; repeat for multi-repo work (the first is primary); "
            "Codex git metadata is used when omitted"
        ),
    )
    parser.add_argument(
        "--source-host",
        action="append",
        default=[],
        metavar="HOSTNAME",
        help="execution hostname captured by the source data; repeat for multi-host work",
    )


def _add_ingest(parser: argparse.ArgumentParser) -> None:
    _add_archive(parser)
    _add_snapshot_root(parser)
    parser.add_argument(
        "--sessions-root", default="~/.codex/sessions", help="Codex rollout tree (default: %(default)s)"
    )
    parser.add_argument("--root-session", required=True, help="coordinator thread UUID")
    parser.add_argument(
        "--continuation-session",
        action="append",
        default=[],
        metavar="UUID",
        help=(
            "explicit successor coordinator session; repeat in chronological order"
        ),
    )
    _add_date_window(parser)
    _add_site_identity(parser)


def _add_claude_ingest(parser: argparse.ArgumentParser) -> None:
    _add_archive(parser)
    _add_snapshot_root(parser)
    parser.add_argument(
        "--session-file",
        required=True,
        help="Claude coordinator JSONL; nested subagents are discovered beside it",
    )
    _add_date_window(parser)
    _add_site_identity(parser)


def _add_claude_history_ingest(parser: argparse.ArgumentParser) -> None:
    _add_archive(parser)
    _add_snapshot_root(parser)
    parser.add_argument(
        "--history-file",
        required=True,
        help=(
            "Claude Code prompt history, normally ~/.claude/history.jsonl; one line per prompt "
            "the owner submitted, kept by Claude Code after the transcript itself is cleaned up"
        ),
    )
    parser.add_argument(
        "--project-pattern",
        required=True,
        metavar="REGEX",
        help=(
            "Python regular expression searched against each prompt's recorded working "
            "directory; only matching prompts join this team, for example "
            "'/work/widget|/worktrees/widget-'"
        ),
    )
    parser.add_argument(
        "--covered-session",
        action="append",
        default=[],
        metavar="SESSION_UUID",
        help=(
            "a Claude session whose full transcript is archived as its own team; its prompts "
            "are left out of this team so they are not projected twice; repeatable. "
            "ingest-project derives this list from the manifest's claude teams by itself"
        ),
    )
    _add_date_window(parser)
    _add_site_identity(parser)


def _add_orc_ingest(parser: argparse.ArgumentParser) -> None:
    _add_archive(parser)
    _add_snapshot_root(parser)
    parser.add_argument(
        "--source-root",
        required=True,
        help="project root containing Orc .orc/ and task .tg/ directories",
    )
    parser.add_argument("--root-session", required=True, help="Orc coordinator session UUID")
    parser.add_argument(
        "--continuation-session",
        action="append",
        default=[],
        metavar="SESSION_OR_JSON",
        help=(
            "explicit successor coordinator session; use a session id for the whole "
            "session or compact JSON with session_id and start_message_id for a reused "
            "session; repeat in chronological order"
        ),
    )
    # Named for the provider even though `ingest-orc` is already provider-specific, where `orc` is
    # a redundant word. `ingest-project` ingests all three providers under one subcommand and needs
    # this same flag there, and a flag that means one thing on one subcommand and is spelled
    # differently on another is worse than a redundant word on this one.
    #
    # Per session, not a blanket switch, and repeatable in the same `action="append"` shape as
    # `--continuation-session` above and `--team` on `ingest-project`. A lineage is a session tree,
    # the guard runs once per session in it, and the refusal names exactly one of them -- so a bare
    # boolean would authorize sessions the operator has structurally never seen, because the run
    # ends at the first refusal. The refusal prints this flag with the session id already filled
    # in, so the operator's next command is a copy of what they were just shown.
    parser.add_argument(
        "--accept-orc-prefix-rewrite",
        action="append",
        default=[],
        metavar="SESSION",
        help=(
            "operator override: accept this one session's recorded append prefix having been "
            "rewritten in place, re-baselining its digest and marking the source degraded; "
            "repeat per session; the changed rows and columns are printed and recorded; a "
            "rewritten session not named here still refuses, and rows that disappeared from or "
            "appeared inside the prefix are refused for every session"
        ),
    )
    _add_date_window(parser)
    _add_site_identity(parser)


def _add_summary(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--backend",
        choices=("codex", "claude", "heuristic"),
        default="codex",
        help="summary backend (heuristic is deterministic/offline)",
    )
    parser.add_argument(
        "--model",
        default=None,
        help=(
            "backend model name; required for Claude, otherwise defaults to "
            f"{DEFAULT_MODEL!r} (or WRKVIZ_MODEL)"
        ),
    )
    parser.add_argument(
        "--reasoning-effort",
        default=DEFAULT_REASONING_EFFORT,
        help=(
            "backend reasoning effort, recorded in cache provenance "
            "(default: %(default)s). "
            "A model/provider failure aborts instead of selecting another model or backend"
        ),
    )
    parser.add_argument(
        "--service-tier",
        default=None,
        help=(
            "Codex service tier; Fast uses 'priority', omission means 'default', "
            "and the effective value is recorded in cache and run provenance"
        ),
    )
    parser.add_argument("--summary-workers", type=int, default=3)
    parser.add_argument("--summary-batch-size", type=int, default=6)
    parser.add_argument(
        "--name-batch-size",
        type=int,
        default=12,
        help="agents per hindsight-naming model call",
    )
    parser.add_argument("--phase-minutes", type=int, default=30)
    parser.add_argument("--context-chars", type=int, default=16000)
    parser.add_argument("--transcript-chars", type=int, default=30000)
    parser.add_argument(
        "--codex-command", default="codex", help="Codex executable (primarily for testing/wrappers)"
    )
    parser.add_argument(
        "--claude-command",
        default="claude",
        help="Claude executable (primarily for testing/wrappers)",
    )
    summary_start = parser.add_mutually_exclusive_group()
    summary_start.add_argument(
        "--summary-start-date",
        help="first local date to summarize without truncating archived data",
    )
    summary_start.add_argument(
        "--summary-start-time",
        help="first RFC3339 instant to summarize, inclusive",
    )
    summary_end = parser.add_mutually_exclusive_group()
    summary_end.add_argument(
        "--summary-end-date",
        help="local date boundary where summarization stops, exclusive",
    )
    summary_end.add_argument(
        "--summary-end-time",
        help="RFC3339 instant where summarization stops, exclusive",
    )
    parser.add_argument(
        "--summary-timezone",
        default=None,
        help="IANA timezone for summary date bounds (defaults to the archived team timezone)",
    )
    parser.add_argument(
        "--rollup-kind",
        action="append",
        choices=("hourly", "daily", "weekly", "monthly", "quarterly"),
        default=[],
        help=(
            "calendar summary level to generate; repeat as needed "
            "(default: daily, weekly, monthly, quarterly)"
        ),
    )


def _add_github_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--github-token-env",
        default="",
        help="environment variable containing an optional GitHub token; the token is never stored",
    )
    parser.add_argument(
        "--github-timeout",
        type=float,
        default=15.0,
        help="per-request GitHub API timeout in seconds (default: %(default)s)",
    )


def _add_export_selection(parser: argparse.ArgumentParser) -> None:
    start = parser.add_mutually_exclusive_group()
    start.add_argument("--start-date", help="first local date to export")
    start.add_argument("--start-time", help="first RFC3339 instant to export")
    end = parser.add_mutually_exclusive_group()
    end.add_argument("--end-date", help="exclusive local date boundary")
    end.add_argument("--end-time", help="exclusive RFC3339 instant")
    parser.add_argument(
        "--timezone",
        default=None,
        help="IANA timezone for date bounds (defaults to the archived team timezone)",
    )
    parser.add_argument(
        "--rollup-kind",
        action="append",
        choices=("hourly", "daily", "weekly", "monthly", "quarterly"),
        default=[],
        help="calendar summary level to include; repeat as needed",
    )


def _add_query_filters(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--team",
        action="append",
        default=[],
        help="team slug to include; repeat to select multiple teams",
    )
    parser.add_argument(
        "--start-time",
        help="first RFC3339 instant to overlap (inclusive)",
    )
    parser.add_argument(
        "--end-time",
        help="exclusive RFC3339 overlap boundary",
    )
    parser.add_argument(
        "--kind",
        action="append",
        choices=("hourly", "daily", "weekly", "monthly", "quarterly"),
        default=[],
        help="rollup kind to include; repeat as needed",
    )
    parser.add_argument(
        "--agent",
        help="canonical agent:TEAM::ID reference to select",
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog=PROG,
        description=(
            "An agent timeline reconstruction, visualisation and summary tool. Turns "
            "coordinator/subagent transcripts into a zoomable, multi-level summary site."
        ),
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    parser.add_argument("--userguide", action="store_true", help="print the complete embedded guide")
    sub = parser.add_subparsers(dest="command")

    quick = sub.add_parser("quickstart", help="print a short end-to-end example", description="print a short end-to-end example")
    quick.set_defaults(handler="quickstart")

    ingest_project_parser = sub.add_parser(
        "ingest-project",
        help="ingest registered project teams and extract transcripts; zero tokens",
        description=(
            "ingest teams from a strict JSON manifest, then refresh the global transcript "
            "JSONL; does not call a model or build the website"
        ),
    )
    ingest_project_parser.add_argument(
        "--config", required=True, help="schema-v1 project ingest JSON file"
    )
    ingest_project_parser.add_argument(
        "--team",
        action="append",
        default=[],
        help=(
            "registered team slug to ingest; repeat as needed (default: every team); "
            "transcript extraction still covers every normalized archive team"
        ),
    )
    # Per team *and* per session, not a blanket switch. A project run ingests every registered team
    # at once and each Orc team is a whole session tree, so a bare boolean would extend one
    # diagnosed rewrite to every other team in the manifest, and a bare team slug would do the same
    # thing to every other session under that team. The composite value needs no quoting rule:
    # neither half can contain a colon, so `TEAM:SESSION` has exactly one parse.
    #
    # A flag, not the manifest field every other per-team knob is (`date_window`,
    # `identity_overrides`, `continuation_sessions`), and that asymmetry is the point: those
    # describe what a team *is*, and are meant to persist. An accepted rewrite is a one-time
    # judgement about one diagnosed event, and a manifest field would silently re-authorize it on
    # every future run -- precisely the standing invitation to launder a real rewrite that
    # the receipt's `accepted_prefix_rewrite_sessions` exists to make visible.
    ingest_project_parser.add_argument(
        "--accept-orc-prefix-rewrite",
        action="append",
        default=[],
        metavar="TEAM:SESSION",
        help=(
            "operator override: accept an in-place append-prefix rewrite for this one session of "
            "this one Orc team, re-baselining its digest and marking the source degraded; repeat "
            "per session; the slug must be an Orc team this run ingests and the session id is the "
            "one that team's refusal named"
        ),
    )
    ingest_project_parser.set_defaults(handler="ingest_project")

    ingest = sub.add_parser("ingest", help="normalize Codex logs; do not call a model", description="normalize Codex logs; do not call a model")
    _add_ingest(ingest)
    ingest.set_defaults(handler="ingest")

    ingest_claude_parser = sub.add_parser(
        "ingest-claude", help="normalize Claude logs; do not call a model",
        description="normalize Claude logs; do not call a model",
    )
    _add_claude_ingest(ingest_claude_parser)
    ingest_claude_parser.set_defaults(handler="ingest_claude")

    ingest_claude_history_parser = sub.add_parser(
        "ingest-claude-history",
        help="normalize the owner's Claude Code prompt history; do not call a model",
        description=(
            "normalize ~/.claude/history.jsonl into one prompt-only team, so prompts whose "
            "transcripts Claude Code has since deleted still reach the timeline and the prompt "
            "projection; does not call a model. Example: wrkviz ingest-claude-history "
            "--history-file ~/.claude/history.jsonl --project-pattern '/work/widget' "
            "--team claude-history-host01 --output ./timelines/widget"
        ),
    )
    _add_claude_history_ingest(ingest_claude_history_parser)
    ingest_claude_history_parser.set_defaults(handler="ingest_claude_history")

    ingest_orc_parser = sub.add_parser(
        "ingest-orc", help="snapshot and normalize Orc SQLite logs; do not call a model",
        description="snapshot and normalize Orc SQLite logs; do not call a model",
    )
    _add_orc_ingest(ingest_orc_parser)
    ingest_orc_parser.set_defaults(handler="ingest_orc")

    summarize = sub.add_parser("summarize", help="fill structured summary cache misses", description="fill structured summary cache misses")
    _add_archive(summarize)
    _add_summary(summarize)
    summarize.set_defaults(handler="summarize")

    build = sub.add_parser("build", help="regenerate Markdown/site from cached data; zero tokens", description="regenerate Markdown/site from cached data; zero tokens")
    _add_archive(build)
    build.add_argument("--phase-minutes", type=int, default=30)
    build.set_defaults(handler="build")

    extract_transcripts = sub.add_parser(
        "extract-transcripts",
        help="write append-only prompt/response JSONL from normalized logs; zero tokens",
        description="write append-only prompt/response JSONL from normalized logs; zero tokens",
    )
    extract_transcripts.add_argument(
        "--output", required=True, help="durable archive directory"
    )
    extract_transcripts.add_argument(
        "--team",
        action="append",
        default=[],
        help="ingested team slug to include; repeat as needed (default: all teams)",
    )
    extract_transcripts.set_defaults(handler="extract_transcripts")

    export = sub.add_parser(
        "export", help="build a zero-token website slice in a separate directory",
        description="build a zero-token website slice in a separate directory",
    )
    export.add_argument("--archive", required=True, help="durable source archive")
    export.add_argument("--output", required=True, help="website export directory")
    export.add_argument(
        "--team",
        action="append",
        required=True,
        help="team slug to export; repeat to align multiple teams in one site",
    )
    export.add_argument("--phase-minutes", type=int, default=30)
    _add_export_selection(export)
    export.set_defaults(handler="export")

    refresh = sub.add_parser("refresh", help="idempotent ingest + summarize + build", description="idempotent ingest + summarize + build")
    _add_ingest(refresh)
    _add_summary(refresh)
    refresh.add_argument(
        "--github-metadata",
        action="store_true",
        help="conditionally cache titles for evidenced GitHub pull links after the build",
    )
    _add_github_options(refresh)
    refresh.set_defaults(handler="refresh")

    github = sub.add_parser(
        "github-metadata",
        help="conditionally cache GitHub pull titles, then rebuild the site",
        description="conditionally cache GitHub pull titles, then rebuild the site",
    )
    _add_archive(github)
    github.add_argument("--phase-minutes", type=int, default=30)
    _add_github_options(github)
    github.set_defaults(handler="github-metadata")

    refresh_claude = sub.add_parser(
        "refresh-claude", help="idempotent Claude ingest + summarize + build",
        description="idempotent Claude ingest + summarize + build",
    )
    _add_claude_ingest(refresh_claude)
    _add_summary(refresh_claude)
    refresh_claude.add_argument(
        "--github-metadata",
        action="store_true",
        help="conditionally cache titles for evidenced GitHub pull links after the build",
    )
    _add_github_options(refresh_claude)
    refresh_claude.set_defaults(handler="refresh_claude")

    refresh_orc = sub.add_parser(
        "refresh-orc", help="idempotent Orc ingest + summarize + build",
        description="idempotent Orc ingest + summarize + build",
    )
    _add_orc_ingest(refresh_orc)
    _add_summary(refresh_orc)
    refresh_orc.add_argument(
        "--github-metadata",
        action="store_true",
        help="conditionally cache titles for evidenced GitHub pull links after the build",
    )
    _add_github_options(refresh_orc)
    refresh_orc.set_defaults(handler="refresh_orc")

    serve_parser = sub.add_parser("serve", help="serve a built archive on localhost", description="serve a built archive on localhost")
    serve_parser.add_argument("--output", required=True, help="built archive directory")
    serve_parser.add_argument("--port", type=int, default=8765, help="0 chooses an available port")
    serve_parser.add_argument("--open", action="store_true", dest="open_browser")
    serve_parser.set_defaults(handler="serve")

    inspect = sub.add_parser("inspect", help="print archive/run/source counts as JSON", description="print archive/run/source counts as JSON")
    inspect.add_argument("--output", required=True)
    inspect.set_defaults(handler="inspect")

    collect_garbage = sub.add_parser(
        "gc",
        help="report what a built archive no longer produces; delete only when asked",
        description=(
            "classify every file in a built archive as live, reclaimable or held, and print the "
            "bytes for each category. Reclaims nothing unless --delete is given, and even then "
            "moves files into .wrkviz-trash/ rather than unlinking them, so a "
            "mistake costs a copy back instead of an eight-hour rebuild. Emptying that trash is "
            "a separate --empty-trash pass. Nothing is ever reclaimed on the strength of 'the "
            "last build did not write it': a file goes only when a named superseding artifact "
            "is present and complete, or when a manifest authoritative over its directory "
            "disowns it"
        ),
    )
    collect_garbage.add_argument(
        "--output", required=True, help="built archive directory"
    )
    collect_garbage.add_argument(
        "--delete",
        action="store_true",
        help="move every reclaimable file into the archive's trash directory",
    )
    collect_garbage.add_argument(
        "--empty-trash",
        action="store_true",
        dest="empty_trash",
        help="permanently delete the trash directory; this is the irreversible pass",
    )
    collect_garbage.add_argument(
        "--format",
        choices=("text", "json"),
        default="text",
        help="report format (default: %(default)s)",
    )
    collect_garbage.set_defaults(handler="gc")

    migrate_build_parser = sub.add_parser(
        "migrate-build-state",
        help="move intermediate build state out of the published archive",
        description=(
            "Relocate teams/*/raw and teams/*/payloads to a sibling store, so the archive holds "
            "only what a consumer of it opens. That state is what ingestion computes from the "
            "vendor snapshots -- the normalized team, the artifact catalogue, the provenance "
            "manifests -- and no browser or CLI reads any of it: on the measured archive it was "
            "717 MiB of 1.5 GiB, shipped to every machine that served the site. It is relocated "
            "rather than deleted because it is what makes the next ingest incremental. Nothing "
            "moves unless --move is given; without it this prints what would move, from where, "
            "and to where. The move is a rename when the store is on the same filesystem, which "
            "it is by default. teams/*/summaries and teams/*/summary_data stay: the first is "
            "output the shipped CLI reads, the second holds the paid summary cache and the "
            "spend receipts run_stats reports from"
        ),
    )
    migrate_build_parser.add_argument(
        "--output", required=True, help="built or ingested archive directory"
    )
    _add_build_root(migrate_build_parser)
    migrate_build_parser.add_argument(
        "--move",
        action="store_true",
        help="actually relocate the build state; without this the command only reports",
    )
    migrate_build_parser.set_defaults(handler="migrate_build_state")

    migrate_snapshots_parser = sub.add_parser(
        "migrate-snapshots",
        help="move an archive's vendor source snapshots out of the published tree",
        description=(
            "relocate teams/<slug>/source_snapshots/ into a store outside --output. The "
            "snapshots are ingest input, not published output: they are typically two thirds of "
            "an archive's bytes, they are reachable over HTTP from `make serve`, and the tool "
            "has always gitignored them. Nothing moves unless --move is given; without it this "
            "prints what would move, from where, to where, and what it refuses to touch. The "
            "move is a rename when the store is on the same filesystem, which it is by default, "
            "and the archive records the new layout before the first byte moves so that an "
            "interrupted run is resumable rather than ambiguous"
        ),
    )
    migrate_snapshots_parser.add_argument(
        "--output", required=True, help="built or ingested archive directory"
    )
    _add_snapshot_root(migrate_snapshots_parser)
    migrate_snapshots_parser.add_argument(
        "--move",
        action="store_true",
        help="actually relocate the snapshots; without this the command only reports",
    )
    migrate_snapshots_parser.add_argument(
        "--copy",
        action="store_true",
        help=(
            "permit a copy-verify-delete when the store is on a different filesystem; unlike a "
            "rename this is interruptible and the bytes exist twice while it runs"
        ),
    )
    migrate_snapshots_parser.add_argument(
        "--format",
        choices=("text", "json"),
        default="text",
        help="report format (default: %(default)s)",
    )
    migrate_snapshots_parser.set_defaults(handler="migrate_snapshots")

    audit_glossary = sub.add_parser(
        "audit-glossary",
        help="audit retired glossary candidates without publishing links",
        description=(
            "read retired schema-3 glossary caches without a lock or model call; "
            "classify definite mechanical noise and candidates requiring a future "
            "semantic pass; no retired entry receives publication authority"
        ),
    )
    audit_glossary.add_argument(
        "--output", required=True, help="durable archive directory"
    )
    audit_glossary.add_argument(
        "--team",
        action="append",
        default=[],
        help="team slug to inspect; repeat as needed (default: every archive team)",
    )
    audit_glossary.add_argument(
        "--format",
        choices=("text", "json"),
        default="text",
        help="report format (default: %(default)s)",
    )
    audit_glossary.add_argument(
        "--details",
        action="store_true",
        help="include every term and its disposition",
    )
    audit_glossary.set_defaults(handler="audit_glossary")

    audit_lossless = sub.add_parser(
        "audit-losslessness",
        help="account for every vendor row against the archive; no lock, no model call",
        description=(
            "re-enumerate each team's vendor source snapshots and require every row to fall "
            "under a declared rule saying what became of it, then check the rule's claim against "
            "raw/team.json and the payload store. Exits 1 when a row matches no rule or a rule's "
            "claim is false; exits 0, and reports the remaining inventory, when the archive's "
            "account of itself holds. Run this before deleting source snapshots"
        ),
    )
    audit_lossless.add_argument(
        "--output", required=True, help="durable archive directory"
    )
    audit_lossless.add_argument(
        "--team",
        action="append",
        default=[],
        help="team slug to audit; repeat as needed (default: every archive team)",
    )
    audit_lossless.add_argument(
        "--format",
        choices=("text", "json"),
        default="text",
        help="report format (default: %(default)s)",
    )
    audit_lossless.add_argument(
        "--require-lossless",
        action="store_true",
        help=(
            "also exit 1 while any vendor row's content is still absent from the archive; this "
            "is the check to gate an actual deletion on"
        ),
    )
    audit_lossless.set_defaults(handler="audit_losslessness")

    query = sub.add_parser(
        "query", help="navigate a built timeline without starting the website",
        description="navigate a built timeline without starting the website",
    )
    query.add_argument("--output", required=True, help="built single- or multi-team archive")
    query.add_argument(
        "--format",
        choices=("json", "jsonl", "markdown", "text"),
        default="json",
        help="response format (default: %(default)s)",
    )
    query_sub = query.add_subparsers(dest="query_action", required=True)
    query_list = query_sub.add_parser("list", help="list concise records and stable references", description="list concise records and stable references")
    query_list.add_argument(
        "resource", choices=("teams", "agents", "phases", "rollups")
    )
    _add_query_filters(query_list)
    query_list.set_defaults(handler="query_list")
    query_show = query_sub.add_parser("show", help="resolve one stable reference", description="resolve one stable reference")
    query_show.add_argument(
        "reference",
        help="team:, agent:, phase:, rollup:, message:, or tool: reference",
    )
    query_show.add_argument(
        "--transcript",
        action="store_true",
        help="include condensed transcript messages when showing a work phase",
    )
    query_show.set_defaults(handler="query_show")
    query_search = query_sub.add_parser(
        "search", help="search prompts, responses, full transcript text, and summaries",
        description=(
            "search the canonical transcript corpus with stable message references, "
            "or use --scope for compatibility phase-transcript search"
        ),
    )
    query_search.add_argument("text", help="text or pattern to find")
    query_search.add_argument(
        "--in",
        dest="search_corpus",
        choices=SEARCH_CORPORA,
        help="search-v2 corpus: owner prompts, agent responses, or all transcript",
    )
    query_search.add_argument(
        "--scope",
        choices=("summaries", "transcripts", "all"),
        default=None,
        help="compatibility search scope (default when --in is absent: summaries)",
    )
    query_search.add_argument(
        "--match", choices=SEARCH_MATCH_MODES, default=None
    )
    query_search.add_argument("--sort", choices=SEARCH_SORTS, default=None)
    query_search.add_argument(
        "--prompt-author", choices=SEARCH_PROMPT_AUTHORS, default=None
    )
    query_search.add_argument(
        "--linkage", choices=SEARCH_LINKAGES, default=None
    )
    query_search.add_argument(
        "--role", action="append", choices=SEARCH_ROLES, default=[]
    )
    query_search.add_argument("--case-sensitive", action="store_true")
    query_search.add_argument("--offset", type=int, default=None)
    query_search.add_argument("--limit", type=int, default=50)
    _add_query_filters(query_search)
    query_search.set_defaults(handler="query_search")
    for action, help_text in (
        ("prompts", "list verbatim human-authored prompts in global timestamp order"),
        ("messages", "list prompts and their mechanically associated responses"),
    ):
        query_transcript = query_sub.add_parser(action, help=help_text, description=help_text)
        query_transcript.add_argument(
            "--range",
            dest="ordinal_range",
            help="one prompt ordinal or inclusive range, for example 200-300",
        )
        query_transcript.add_argument(
            "--which",
            choices=("human", "bot", "all"),
            default="human",
            help="select human, bot, or all prompt authorship (default: %(default)s)",
        )
        query_transcript.add_argument(
            "--limit",
            type=int,
            default=None,
            metavar="N",
            help="return at most N records from the start of the selection",
        )
        query_transcript.add_argument(
            "--tail",
            type=int,
            default=None,
            metavar="N",
            help="return the last N records of the selection",
        )
        query_transcript.add_argument(
            "--verify",
            action="store_true",
            help=(
                "re-read every consulted projection end to end and reproduce its manifest "
                "digest, and resolve prompt/response linkage by full scan rather than seek"
            ),
        )
        query_transcript.add_argument(
            "--team", action="append", default=[], help="team slug; repeat as needed"
        )
        query_transcript.add_argument("--start-time", help="inclusive RFC3339 instant")
        query_transcript.add_argument("--end-time", help="exclusive RFC3339 instant")
        query_transcript.set_defaults(handler=f"query_{action}")
    query_stats = query_sub.add_parser(
        "stats",
        help="count prompt, response, and generated-summary text",
        description=(
            "count records, whitespace-delimited words, and UTF-8 text bytes for "
            "attributed and unattributed prompts, responses, and generated summaries; "
            "read-only and zero-model"
        ),
    )
    query_stats.add_argument(
        "--team", action="append", default=[], help="team slug; repeat as needed"
    )
    query_stats.add_argument("--start-time", help="inclusive RFC3339 instant")
    query_stats.add_argument("--end-time", help="exclusive RFC3339 instant")
    query_stats.add_argument(
        "--kind",
        action="append",
        choices=("hourly", "daily", "weekly", "monthly", "quarterly"),
        default=[],
        help="rollup kind to count; repeat as needed",
    )
    query_stats.set_defaults(handler="query_stats")
    return parser


def _summary_call(ns: argparse.Namespace) -> SummarizeReport:
    backend = str(ns.backend)
    raw_model: object = ns.model
    if raw_model is None:
        if backend == "claude":
            raise ValueError("--model is required when --backend=claude")
        model = DEFAULT_MODEL
    else:
        model = str(raw_model).strip()
        if not model:
            raise ValueError("--model must not be empty")
    raw_service_tier: object = ns.service_tier
    service_tier = (
        None if raw_service_tier is None else str(raw_service_tier)
    )
    archive = _path(str(ns.output))
    raw_summary_timezone: object = ns.summary_timezone
    has_summary_date_bound = (
        ns.summary_start_date is not None or ns.summary_end_date is not None
    )
    if raw_summary_timezone is None and has_summary_date_bound:
        summary_timezone = load_archived_team(
            archive, str(ns.team)
        ).display_timezone
    elif raw_summary_timezone is None:
        summary_timezone = "UTC"
    else:
        summary_timezone = str(raw_summary_timezone)
    summary_window = parse_date_window(
        (
            str(ns.summary_start_date)
            if ns.summary_start_date is not None
            else None
        ),
        str(ns.summary_end_date) if ns.summary_end_date is not None else None,
        summary_timezone,
        start_time=(
            str(ns.summary_start_time)
            if ns.summary_start_time is not None
            else None
        ),
        end_time=(
            str(ns.summary_end_time)
            if ns.summary_end_time is not None
            else None
        ),
    )
    raw_rollup_kinds: object = ns.rollup_kind
    if not isinstance(raw_rollup_kinds, list) or not all(
        isinstance(item, str) for item in raw_rollup_kinds
    ):
        raise ValueError("--rollup-kind values must be strings")
    rollup_kinds = (
        tuple(raw_rollup_kinds)
        if raw_rollup_kinds
        else ("daily", "weekly", "monthly", "quarterly")
    )
    return summarize_archive(
        archive,
        str(ns.team),
        backend,
        model,
        max_workers=int(ns.summary_workers),
        batch_size=int(ns.summary_batch_size),
        name_batch_size=int(ns.name_batch_size),
        phase_minutes=int(ns.phase_minutes),
        context_chars=int(ns.context_chars),
        transcript_chars=int(ns.transcript_chars),
        codex_command=(str(ns.codex_command),),
        claude_command=(str(ns.claude_command),),
        reasoning_effort=str(ns.reasoning_effort),
        service_tier=service_tier,
        summary_window=summary_window,
        rollup_kinds=rollup_kinds,
    )


def _string_list(raw: object, flag: str) -> tuple[str, ...]:
    """Narrow one repeatable ``action="append"`` value to the strings the callee is typed for.

    argparse hands back ``Any``, and the alternative -- ``tuple(str(item) for item in raw)`` --
    silences the type checker by *coercing* rather than checking, which turns a namespace this
    package mis-populates into a plausible-looking argument instead of an error. That matters most
    for an override flag, where a value the operator did not write is the whole hazard.
    """

    if not isinstance(raw, list) or not all(isinstance(item, str) for item in raw):
        raise ValueError(f"{flag} values must be strings")
    return tuple(item for item in raw if isinstance(item, str))


def _identity_overrides(
    ns: argparse.Namespace, command_args: Sequence[str]
) -> IdentityOverrides:
    projects, hosts = parse_identity_overrides(
        _string_list(getattr(ns, "project", []), "--project"),
        _string_list(getattr(ns, "source_host", []), "--source-host"),
    )
    timezone_explicit = any(
        item == "--timezone" or item.startswith("--timezone=")
        for item in command_args
    )
    return IdentityOverrides(
        projects,
        hosts,
        "explicit" if timezone_explicit else "default",
    )


def _print_ingest(report: IngestReport) -> None:
    print(
        f"ingest: {report.agents} agents, {report.events} messages/events, "
        f"{report.tool_calls} outer tools, {report.edges} edges from {report.sources} source files "
        f"({report.source_bytes / (1024 * 1024):.1f} MiB); {report.files_changed} archive files changed"
    )


def _print_history_exclusions(report: IngestReport) -> None:
    """Say what a prompt-history ingest left out and why, on stderr.

    Both numbers are exclusions the operator configured, and both are exclusions all the same: a
    team that shows 1,900 prompts out of a 4,200-line history should not require opening the
    receipt to learn where the other 2,300 went.
    """

    if report.history_unmatched_prompts == 0 and report.history_covered_prompts == 0:
        return
    parts: list[str] = []
    if report.history_unmatched_prompts:
        parts.append(
            f"{report.history_unmatched_prompts} prompt(s) typed outside the project pattern"
        )
    if report.history_covered_prompts:
        parts.append(
            f"{report.history_covered_prompts} prompt(s) from {report.history_covered_sessions} "
            "session(s) whose full transcript is archived as its own team"
        )
    print(f"{PROG}: {report.team_slug}: left out " + "; ".join(parts), file=sys.stderr)


def _print_retired_projections(report: IngestReport) -> None:
    """Announce the one-time sweep of the retired per-thread projection, on stderr.

    Silence would be wrong here: the operator's archive just lost hundreds of megabytes and, if it
    is versioned, gained a deletion of a few thousand tracked files, and they are entitled to see
    why before `git status` tells them. Stderr for the same reason the override notice uses it --
    this is a notice about the archive, not a result of the query, and it has to survive stdout
    being redirected. It prints once: the next ingest finds nothing to sweep and says nothing.
    """

    if report.retired_message_projections == 0:
        return
    mib = report.retired_message_projection_bytes / (1024 * 1024)
    print(
        f"{PROG}: {report.team_slug}: removed {report.retired_message_projections} retired "
        f"raw/messages/<thread-id>.json file(s) ({mib:.1f} MiB); the tool no longer writes that "
        "per-thread projection, which nothing read and which raw/team.json still fully describes",
        file=sys.stderr,
    )


def _print_snapshot_root(report: IngestReport, archive: Path) -> None:
    """Say where the vendor snapshots went, exactly once per archive, on stderr.

    This is the only thing the tool writes outside `--output`, and an operator who typed
    `--output X` is entitled to be told the first time several gigabytes start accumulating
    somewhere that is not `X`. Said once -- on the run that records the layout -- and silent
    afterwards, for the same reason the task-note and payload announcements are: a line printed
    on every run is a line nobody reads on the run that matters.

    An archive that keeps its snapshots in the old in-archive place gets a different sentence,
    naming the command that changes that. It is not a warning and not a deprecation: the old
    layout keeps working indefinitely, and the only thing wrong with it is the thing the operator
    can now choose to fix.
    """

    if not report.snapshot_root_established:
        return
    # Resolved for display. The report keeps the archive-relative form so that the receipt it
    # lands in stays free of absolute paths; a terminal wants the opposite.
    where = (archive / report.snapshot_root).resolve()
    if report.snapshot_root_layout == "in-archive-legacy":
        print(
            f"{PROG}: {report.team_slug}: this archive keeps its vendor source snapshots inside "
            f"itself, at {where}; they are ingest input rather than published output, and they "
            "are reachable over HTTP from `make serve` until they are moved. "
            f"`{PROG} migrate-snapshots --output {archive}` says what moving them would do",
            file=sys.stderr,
        )
        return
    print(
        f"{PROG}: {report.team_slug}: vendor source snapshots are being kept in {where}, outside "
        "the published archive, because they are ingest input and are usually the largest thing "
        "here. The archive records the location; back it up with the archive if you want "
        "incremental ingest to keep working",
        file=sys.stderr,
    )


def _print_promoted_task_notes(report: IngestReport) -> None:
    """Announce newly promoted task notes, on stderr, and say why the archive just grew.

    Silent on the steady state, where the number is the handful of notes written upstream since
    the last run and the operator learns nothing from being told. Loud exactly once per team, on
    the ingest that first lifts an existing frozen projection into `raw/task-notes.jsonl`: that
    run adds tens of megabytes of *tracked* files to a versioned archive, and an operator who
    finds that in `git status` without having been told deserves better than guessing.

    The wording names what is new rather than what changed, because the content itself is not new
    -- it was already in the archive, inside gitignored `source_snapshots/`, one deletion away
    from being gone. That is the sentence worth printing.
    """

    if report.newly_promoted_task_notes == 0:
        return
    print(
        f"{PROG}: {report.team_slug}: promoted {report.newly_promoted_task_notes} task note(s) "
        f"into raw/task-notes.jsonl ({report.task_notes} total); the archive now keeps this text "
        "under version control instead of only inside gitignored source_snapshots/",
        file=sys.stderr,
    )
    # And the number that makes the sentence above worth reading. These notes are gone upstream:
    # the file just written is the only copy of them anywhere, which is a different claim from
    # "the archive keeps a copy" and the one an operator should see before deciding what to back
    # up. It is printed alongside the promotion rather than on its own line every run, because on
    # the steady state nothing has been promoted and the standing count has not moved.
    if report.task_notes_upstream_deleted:
        print(
            f"{PROG}: {report.team_slug}: {report.task_notes_upstream_deleted} of those notes no "
            "longer exist in the upstream task table; for those this archive is the only copy",
            file=sys.stderr,
        )


def _print_stored_payloads(report: IngestReport) -> None:
    """Announce newly stored tool payloads, on stderr, and say where they went.

    Loud exactly once per team, on the ingest that first rescues the command arguments and stdout
    that `_archive_team` used to delete outright, because that run grows the operator's archive by
    a fifth of the size of its vendor snapshots and they should learn it here rather than from
    `df`. Silent on the steady state, where the number is whatever the agents have run since the
    last ingest.

    It says the tree is gitignored because that is the first question an operator will have about
    hundreds of megabytes of command output appearing under `teams/`, and the answer -- the same
    answer `source_snapshots/` has always had -- is the reason it is safe to keep at all.
    """

    # Said first and unconditionally, because unlike everything else in this function it is not
    # news about growth. A shard whose bytes stopped matching the digest recorded for them is a
    # state a content-addressed union should not be able to reach; the merge re-measured it rather
    # than refusing, so this line is the only trace that anything was lost. A prune is quieter --
    # it is a supported operation someone performed on purpose -- but it is still named, because a
    # supported operation that leaves no record is indistinguishable from data loss later.
    for shard in report.damaged_payload_shards:
        print(f"{PROG}: {report.team_slug}: payload store: {shard}", file=sys.stderr)
    if report.pruned_payload_shards:
        print(
            f"{PROG}: {report.team_slug}: {len(report.pruned_payload_shards)} payload shard(s) "
            "recorded by the previous ingest are no longer in the tree; the manifest no longer "
            "claims them",
            file=sys.stderr,
        )
    if report.newly_stored_tool_payloads == 0:
        return
    mib = report.newly_stored_tool_payload_bytes / (1024 * 1024)
    print(
        f"{PROG}: {report.team_slug}: stored {report.newly_stored_tool_payloads} tool payload(s) "
        f"({mib:.1f} MiB) into gitignored teams/{report.team_slug}/payloads/ "
        f"({report.tool_payloads} total); the archive now keeps command arguments and output "
        "instead of discarding them and deferring to the vendor logs",
        file=sys.stderr,
    )


def _print_prefix_overrides(report: IngestReport) -> None:
    """Report every append-prefix rewrite this ingest accepted, on stderr, in full.

    On stderr because this is a warning about degraded data rather than a result: it must survive
    the operator redirecting stdout to a file, and it must be impossible to miss in a scheduled
    run's log. Nothing here is summarized away -- the record is already bounded at the point it is
    built, so printing all of it costs at most a couple of dozen lines.
    """

    for override in report.orc_prefix_overrides:
        for line in override.describe():
            print(f"{PROG}: {report.team_slug}: {line}", file=sys.stderr)


def _print_transcript_partiality(report: TranscriptExportReport) -> None:
    """Name every team the projection carried and every rule it set aside, on stderr, in full.

    On stderr for the same reason as `_print_prefix_overrides`: this is a warning about degraded
    output, not a result, so it must survive stdout being redirected to a file and it must be
    unmissable in a scheduled run's log. One line per team and one per rule, never a count -- the
    count is already on the stdout summary line, and a count alone is the shape that lets a
    permanent skip fade into background noise, since "1 team carried" reads the same on the run
    where it started as on the four hundredth run afterwards. The team slug and the exception are
    what an operator can act on.

    A traceback is printed whenever one was kept, which is only for exception types the exporter
    does not classify as data/IO failure -- for those the one-line message is a defect report with
    the evidence removed, and this is often the only place it survives an unattended overnight run.
    """

    for skip in report.skipped_teams:
        print(
            f"{PROG}: transcripts: team {skip.summary} -- carried forward unchanged from its last "
            "good extraction; nothing since is projected",
            file=sys.stderr,
        )
        if skip.traceback is not None:
            print(skip.traceback, end="", file=sys.stderr)
    for dropped in report.dropped_authorship_rules:
        print(
            f"{PROG}: transcripts: prompt authorship rule {dropped.summary} -- not applied",
            file=sys.stderr,
        )


def _print_summaries(report: SummarizeReport) -> None:
    tier = report.service_tier or "unspecified"
    print(
        f"summarize ({report.backend} / {report.model} / tier={tier}): "
        f"{report.phases} phases + {report.agent_names} hindsight agent names + "
        f"{report.rollups} calendar periods × 2 summary audiences; "
        f"cache {report.cache_hits} hit / {report.cache_misses} miss in "
        f"{report.backend_batches} backend batch(es); {report.project_overviews} project overview + "
        f"{report.glossary_definitions} glossary definitions; "
        f"catalog {report.catalog_artifacts} immutable artifacts"
    )
    print(
        _usage_line(
            "tokens newly spent",
            report.newly_spent_usage,
            report.newly_spent_unknown_receipts,
        )
    )
    print(
        _usage_line(
            "tokens behind returned cached artifacts",
            report.artifact_generation_usage,
            report.artifact_generation_unknown_receipts,
        )
    )
    if report.unknown_legacy_artifacts:
        print(
            "token accounting: "
            f"{report.unknown_legacy_artifacts} legacy artifact(s) have no usage receipt"
        )


def _usage_line(label: str, usage: TokenUsage, unknown_receipts: int) -> str:
    return (
        f"{label}: input={usage.input_tokens}, "
        f"cached_input={usage.cached_input_tokens}, "
        f"cache_write_input={usage.cache_write_input_tokens}, "
        f"output={usage.output_tokens}, "
        f"reasoning_output={usage.reasoning_output_tokens}, "
        f"total={usage.total_tokens}; unknown_receipts={unknown_receipts}"
    )


def _github_token(ns: argparse.Namespace) -> str | None:
    requested = str(ns.github_token_env).strip()
    if requested:
        value = os.environ.get(requested)
        if not value:
            raise ValueError(
                f"GitHub token environment variable {requested!r} is unset or empty"
            )
        return value
    return os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or None


def _print_github_metadata(report: PullMetadataReport) -> None:
    print(
        f"github metadata: {report.references} evidenced references / "
        f"{report.distinct_pulls} distinct pulls; {report.fetched} fetched, "
        f"{report.not_modified} unchanged, {len(report.failures)} failed; "
        f"cache {report.cache_path}"
    )


def _export_selection(
    ns: argparse.Namespace, archive: Path, team_slugs: Sequence[str]
) -> tuple[DateWindow | None, tuple[str, ...], str]:
    if not team_slugs:
        raise ValueError("at least one --team is required")
    archived_timezones = {
        load_archived_team(archive, team_slug).display_timezone
        for team_slug in team_slugs
    }
    raw_timezone: object = ns.timezone
    if raw_timezone is None:
        if len(archived_timezones) != 1:
            raise ValueError(
                "teams use different display timezones; pass --timezone for one shared axis"
            )
        timezone_name = next(iter(archived_timezones))
    else:
        timezone_name = str(raw_timezone)
    window = parse_date_window(
        str(ns.start_date) if ns.start_date is not None else None,
        str(ns.end_date) if ns.end_date is not None else None,
        timezone_name,
        start_time=str(ns.start_time) if ns.start_time is not None else None,
        end_time=str(ns.end_time) if ns.end_time is not None else None,
    )
    raw_kinds: object = ns.rollup_kind
    if not isinstance(raw_kinds, list) or not all(
        isinstance(item, str) for item in raw_kinds
    ):
        raise ValueError("--rollup-kind values must be strings")
    kinds = (
        tuple(raw_kinds)
        if raw_kinds
        else ("daily", "weekly", "monthly", "quarterly")
    )
    return window, kinds, timezone_name


def _inspect_counts(archive: Path) -> dict[str, int]:
    """The six collection sizes, from the cheapest generation that can state them.

    Schema 3 publishes all six in its 168,703-byte bootstrap, so a complete generation answers
    without opening a shard. Before this, the only implementation parsed ``data/timeline.json``
    -- 246,973,399 bytes at 1.44 GiB resident on the measured archive -- to print six integers,
    which is also why a published build no longer writing that file could not simply have left
    this alone.

    The schema-1 branch is kept, and kept second, for the archives that still only have it: one
    written before schema 3 existed, or the combined export's per-team intermediate. It is a
    fallback in the same sense the reader's is, and it disappears when those archives do.
    """

    counts = schema_3_record_counts(archive)
    if counts is not None:
        return counts
    timeline_path = archive / "data" / "timeline.json"
    if not timeline_path.is_file():
        _complete, declined = schema_3_completeness(archive)
        raise ValueError(
            f"no built timeline in {archive}: {declined}, and no {timeline_path}"
        )
    timeline = as_object(read_json(timeline_path), str(timeline_path))
    return {
        "agents": len(as_array(timeline.get("agents"), "timeline.agents")),
        "phases": len(as_array(timeline.get("phases"), "timeline.phases")),
        "edges": len(as_array(timeline.get("edges"), "timeline.edges")),
        "events": len(as_array(timeline.get("events"), "timeline.events")),
        "rollups": len(as_array(timeline.get("rollups"), "timeline.rollups")),
        "summary_files": len(
            as_array(timeline.get("summary_files"), "timeline.summary_files")
        ),
    }


def _inspect(archive: Path) -> int:
    manifest_path = archive / "manifest.json"
    manifest = as_object(read_json(manifest_path), str(manifest_path)) if manifest_path.is_file() else {}
    result: dict[str, object] = dict(_inspect_counts(archive))
    result["archive"] = str(archive.resolve())
    result["manifest"] = manifest
    # Cheap -- one small JSON read plus a walk of whatever is still inside the archive, which for
    # a migrated archive is nothing -- and it is the answer to the first question anyone asks
    # after noticing that an 8.8 GB archive is now 2.9 GB.
    result["source_snapshots"] = pointer_summary(archive)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


def _migrate_build_state(
    archive: Path, requested: Path | None, *, move: bool
) -> int:
    """Report or perform the relocation, under the archive writer lock in both cases.

    The dry run takes the lock for the reason `_migrate_snapshots` does: the report is the input
    to a human's decision, and one computed while a build was moving files underneath it would
    describe an archive that never existed.
    """

    with archive_writer_lock(archive):
        result = migrate_build_state(archive, requested, apply=move)
    print(format_build_migration_report(result, applied=move), end="")
    return 0


def _migrate_snapshots(
    archive: Path,
    requested: Path | None,
    *,
    move: bool,
    copy_across_devices: bool,
    report_format: str,
) -> int:
    """Report or perform the relocation, under the archive writer lock in both cases.

    The dry run takes the lock too. It is the same argument `gc` makes: the report is the input to
    a human's decision about several gigabytes, and one computed while a build was moving files
    underneath it would describe an archive that never existed.
    """

    if report_format not in {"text", "json"}:
        raise ValueError(f"unsupported migrate-snapshots report format {report_format!r}")
    if copy_across_devices and not move:
        # Not an error the operator can act on any other way: --copy authorizes a destructive,
        # interruptible path, and a dry run performs no path at all. Refusing is how the flag
        # stops meaning "I meant to move" without ever having moved.
        raise ValueError("--copy has no effect without --move; add --move or drop --copy")
    with archive_writer_lock(archive):
        result = migrate_snapshots(
            archive,
            requested,
            move=move,
            copy_across_devices=copy_across_devices,
        )
    if report_format == "json":
        print(json.dumps(result.to_json_obj(), indent=2, sort_keys=True))
    else:
        print(format_migration_report(result, moved=move), end="")
    return 0


def _transcript_teams(archive: Path, raw: object) -> tuple[str, ...]:
    if not isinstance(raw, list) or not all(isinstance(item, str) for item in raw):
        raise ValueError("--team values must be strings")
    selected = tuple(raw)
    if selected:
        if len(set(selected)) != len(selected):
            raise ValueError("--team must not repeat a team slug")
        return selected
    discovered = ingested_team_slugs(archive)
    if not discovered:
        raise ValueError(f"no ingested teams found in {archive}")
    return discovered


def _query_filters(ns: argparse.Namespace) -> QueryFilters:
    raw_teams: object = ns.team
    if not isinstance(raw_teams, list) or not all(
        isinstance(item, str) for item in raw_teams
    ):
        raise ValueError("--team values must be strings")
    raw_kinds: object = getattr(ns, "kind", [])
    if not isinstance(raw_kinds, list) or not all(
        isinstance(item, str) for item in raw_kinds
    ):
        raise ValueError("--kind values must be strings")
    raw_agent: object = getattr(ns, "agent", None)
    if raw_agent is not None and not isinstance(raw_agent, str):
        raise ValueError("--agent must be a string")
    window = parse_date_window(
        None,
        None,
        "UTC",
        start_time=(
            str(ns.start_time) if ns.start_time is not None else None
        ),
        end_time=str(ns.end_time) if ns.end_time is not None else None,
    )
    return QueryFilters(
        teams=tuple(raw_teams),
        window=window,
        rollup_kinds=tuple(raw_kinds),
        agent_ref=raw_agent,
    )


def _run_query(ns: argparse.Namespace, handler: str) -> int:
    output = _path(str(ns.output))
    command: str
    if handler == "query_list":
        query = TimelineQuery(output)
        resource = str(ns.resource)
        command = f"list {resource}"
        items = query.list_records(resource, _query_filters(ns))
    elif handler == "query_show":
        query = TimelineQuery(output)
        reference = str(ns.reference)
        command = f"show {reference}"
        items = [query.show(reference, transcript=bool(ns.transcript))]
    elif handler == "query_search":
        query = TimelineQuery(output)
        needle = str(ns.text)
        command = f"search {needle}"
        raw_corpus: object = ns.search_corpus
        raw_scope: object = ns.scope
        if raw_corpus is not None:
            if raw_scope is not None:
                raise ValueError("--in and --scope cannot be combined")
            raw_roles: object = ns.role
            if not isinstance(raw_roles, list) or not all(
                isinstance(role, str) for role in raw_roles
            ):
                raise ValueError("--role values must be strings")
            search_results = query.search_v2(
                needle,
                corpus=str(raw_corpus),
                filters=_query_filters(ns),
                case_sensitive=bool(ns.case_sensitive),
                match_mode=str(ns.match or "smart"),
                sort=str(ns.sort or "relevance"),
                prompt_author=str(ns.prompt_author or "any"),
                linkage=str(ns.linkage or "any"),
                roles=tuple(raw_roles),
                offset=int(ns.offset or 0),
                limit=int(ns.limit),
            )
            print(
                format_search_results(command, search_results, str(ns.format)),
                end="",
            )
            return 0
        if (
            ns.match is not None
            or ns.sort is not None
            or ns.prompt_author is not None
            or ns.linkage is not None
            or ns.offset is not None
            or ns.role
        ):
            raise ValueError("search-v2 options require --in")
        items = query.search(
            needle,
            scope=str(raw_scope or "summaries"),
            filters=_query_filters(ns),
            case_sensitive=bool(ns.case_sensitive),
            limit=int(ns.limit),
        )
    elif handler in {"query_prompts", "query_messages"}:
        raw_limit: object = ns.limit
        raw_tail: object = ns.tail
        if raw_limit is not None and not isinstance(raw_limit, int):
            raise ValueError("--limit must be an integer")
        if raw_tail is not None and not isinstance(raw_tail, int):
            raise ValueError("--tail must be an integer")
        transcript_query = TranscriptQuery(output, verify=bool(ns.verify))
        raw_range: object = ns.ordinal_range
        ordinal_range = (
            parse_ordinal_range(raw_range) if isinstance(raw_range, str) else None
        )
        command = "prompts" if handler == "query_prompts" else "messages"
        items = (
            transcript_query.list_prompts(
                _query_filters(ns),
                ordinal_range,
                str(ns.which),
                limit=raw_limit,
                tail=raw_tail,
            )
            if handler == "query_prompts"
            else transcript_query.list_messages(
                _query_filters(ns),
                ordinal_range,
                str(ns.which),
                limit=raw_limit,
                tail=raw_tail,
            )
        )
    elif handler == "query_stats":
        print(
            format_stats(archive_stats(output, _query_filters(ns)), str(ns.format)),
            end="",
        )
        return 0
    else:
        raise ValueError(f"unsupported query handler {handler!r}")
    print(format_query(command, items, str(ns.format)), end="")
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the command-line interface and return its process exit status."""

    args = list(argv) if argv is not None else sys.argv[1:]
    parser = _parser()
    ns = parser.parse_args(args)
    if bool(ns.userguide):
        print(_load_userguide(), end="")
        return 0
    handler = getattr(ns, "handler", None)
    if handler is None:
        parser.print_help()
        return 0
    if handler == "quickstart":
        print(_quickstart())
        return 0
    if handler == "serve":
        serve(_path(str(ns.output)), "127.0.0.1", int(ns.port), bool(ns.open_browser))
        return 0
    if handler == "inspect":
        try:
            return _inspect(_path(str(ns.output)))
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "gc":
        try:
            gc_format = str(ns.format)
            if gc_format not in {"text", "json"}:
                raise ValueError(f"unsupported gc report format {gc_format!r}")
            print(
                format_gc_report(
                    collect(
                        _path(str(ns.output)),
                        delete=bool(ns.delete),
                        empty_trash=bool(ns.empty_trash),
                    ),
                    gc_format,
                ),
                end="",
            )
            return 0
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "migrate_build_state":
        try:
            return _migrate_build_state(
                _path(str(ns.output)),
                _path(str(ns.build_root)) if ns.build_root is not None else None,
                move=bool(ns.move),
            )
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "migrate_snapshots":
        try:
            return _migrate_snapshots(
                _path(str(ns.output)),
                _path(str(ns.snapshot_root)) if ns.snapshot_root is not None else None,
                move=bool(ns.move),
                copy_across_devices=bool(ns.copy),
                report_format=str(ns.format),
            )
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "audit_glossary":
        try:
            glossary_team_values: object = ns.team
            if not isinstance(glossary_team_values, list) or not all(
                isinstance(item, str) for item in glossary_team_values
            ):
                raise ValueError("--team values must be strings")
            glossary_report = audit_legacy_glossaries(
                _path(str(ns.output)), tuple(glossary_team_values)
            )
            output_format = str(ns.format)
            if output_format not in {"text", "json"}:
                raise ValueError(f"unsupported glossary audit format {output_format!r}")
            print(
                format_glossary_audit(
                    glossary_report,
                    "json" if output_format == "json" else "text",
                    include_terms=bool(ns.details),
                ),
                end="",
            )
            return 0
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "audit_losslessness":
        try:
            lossless_report = audit_archive_losslessness(
                _path(str(ns.output)),
                _string_list(ns.team, "--team"),
            )
            lossless_format = str(ns.format)
            if lossless_format not in {"text", "json"}:
                raise ValueError(
                    f"unsupported losslessness audit format {lossless_format!r}"
                )
            print(
                format_losslessness_audit(
                    lossless_report, "json" if lossless_format == "json" else "text"
                ),
                end="",
            )
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
        # Exit 1, not 2: this is a finding about the archive, not a failure to run. `make` and CI
        # both distinguish them, and an operator scripting a deletion behind this command needs
        # "the gate says no" to be tellable from "the gate could not be evaluated".
        if not lossless_report.sound:
            return 1
        if bool(ns.require_lossless) and not lossless_report.lossless:
            return 1
        return 0
    if handler == "ingest_project":
        project_started = utc_now()
        project_command = [PROG, *args]
        config: ProjectIngestConfig | None = None
        selected_slugs: tuple[str, ...] = ()
        try:
            config = load_project_ingest_config(_path(str(ns.config)))
            selected_slugs = tuple(
                team.slug
                for team in config.select_teams(_string_list(ns.team, "--team"))
            )
            project_report = ingest_project(
                config,
                selected_slugs,
                _string_list(
                    ns.accept_orc_prefix_rewrite, "--accept-orc-prefix-rewrite"
                ),
            )
            project_failure_summary = project_report.failure_summary()
            # A partial run is recorded as "failed", not as a third status. `run_stats` buckets
            # anything outside completed/failed as "other", so inventing "partial" would quietly
            # move these runs out of the failed count that a human actually watches. The structured
            # detail -- which teams, and why -- lives in the mechanical payload below, where it can
            # be read without guessing from a status word.
            project_run_path = record_run(
                config.output,
                project_command,
                project_started,
                "failed" if project_report.failed else "completed",
                selected_slugs[0],
                None,
                None,
                None,
                error=project_failure_summary,
                team_slugs=selected_slugs,
                mechanical={"project_ingest": project_report.to_json_obj()},
            )
            for team in project_report.teams:
                print(f"{team.team_slug} ({team.provider}):", end=" ")
                _print_ingest(team.ingest)
                _print_snapshot_root(team.ingest, config.output)
                _print_history_exclusions(team.ingest)
                _print_retired_projections(team.ingest)
                _print_promoted_task_notes(team.ingest)
                _print_stored_payloads(team.ingest)
                _print_prefix_overrides(team.ingest)
            for failure in project_report.failures:
                print(f"{PROG}: team {failure.summary}", file=sys.stderr)
                if failure.traceback is not None:
                    print(failure.traceback, end="", file=sys.stderr)
            print(
                f"teams: {len(project_report.teams)} succeeded, "
                f"{len(project_report.failures)} failed"
            )
            project_transcripts = project_report.transcripts
            if project_transcripts is None:
                print(
                    f"{PROG}: transcript extraction failed: "
                    f"{project_report.transcript_error}",
                    file=sys.stderr,
                )
                # Deliberately no JSONL path here. The file still exists from the last good run,
                # and naming it after a failed extraction would read as a pointer to fresh output.
                print("transcripts: not extracted")
            else:
                _print_transcript_partiality(project_transcripts)
                project_skipped_note = (
                    f" ({len(project_transcripts.skipped_teams)} further team(s) carried "
                    "forward unread)"
                    if project_transcripts.skipped_teams
                    else ""
                )
                print(
                    f"transcripts: {project_transcripts.prompts} prompts, "
                    f"{project_transcripts.responses} responses, "
                    f"{project_transcripts.system_inputs} system inputs across "
                    f"{project_transcripts.teams} normalized archive teams"
                    f"{project_skipped_note}; "
                    f"{project_transcripts.files_changed} files changed"
                )
                print(
                    "JSONL: "
                    f"{config.output / 'extracted' / 'transcripts' / 'prompts.jsonl'}"
                )
            print("website: not built (run `wrkviz build` separately)")
            print(f"run metadata: {project_run_path}")
            if project_report.failed:
                print(f"{PROG}: {project_failure_summary}", file=sys.stderr)
                return 2
            return 0
        except (
            ClaudeParseError,
            CodexParseError,
            OrcParseError,
            OSError,
            ValueError,
        ) as error:
            if config is not None and selected_slugs:
                try:
                    project_run_path = record_run(
                        config.output,
                        project_command,
                        project_started,
                        "failed",
                        selected_slugs[0],
                        None,
                        None,
                        None,
                        error=str(error),
                        team_slugs=selected_slugs,
                        mechanical={
                            "project_ingest": {
                                "schema_version": 1,
                                "config_sha256": config.config_sha256,
                                "model_calls": 0,
                                "model_tokens": 0,
                                "website_build_performed": False,
                            }
                        },
                    )
                    print(f"run metadata: {project_run_path}", file=sys.stderr)
                except (OSError, ValueError):
                    pass
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler in {
        "query_list",
        "query_show",
        "query_search",
        "query_prompts",
        "query_messages",
        "query_stats",
    }:
        try:
            return _run_query(ns, str(handler))
        except (OSError, ValueError) as error:
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "extract_transcripts":
        archive = _path(str(ns.output))
        started = utc_now()
        command = [PROG, *args]
        selected: tuple[str, ...] = ()
        try:
            selected = _transcript_teams(archive, ns.team)
            report = extract_transcripts_archive(archive, selected)
            partiality = report.partiality_summary()
            # A partial extraction is recorded as "failed", not as a third status, for the same
            # reason `ingest-project` does it: `run_stats` buckets anything outside
            # completed/failed as "other", so inventing "partial" would quietly move these runs
            # out of the failed count a human actually watches. Which teams, and why, is in the
            # mechanical payload and on stderr.
            run_path = record_run(
                archive,
                command,
                started,
                "failed" if partiality is not None else "completed",
                selected[0],
                None,
                None,
                None,
                error=partiality,
                team_slugs=selected,
                mechanical={"transcript_extraction": report.to_json_obj()},
            )
            _print_transcript_partiality(report)
            skipped_note = (
                f" ({len(report.skipped_teams)} further team(s) carried forward unread)"
                if report.skipped_teams
                else ""
            )
            print(
                f"transcripts: {report.prompts} prompts, {report.responses} responses, "
                f"{report.system_inputs} system inputs across {report.teams} teams"
                f"{skipped_note}; "
                f"{report.carried_forward} historical records retained, "
                f"{report.reclassified} source records reclassified, "
                f"{report.files_changed} files changed"
            )
            print(f"JSONL: {archive / 'extracted' / 'transcripts' / 'prompts.jsonl'}")
            print(f"run metadata: {run_path}")
            if partiality is not None:
                print(f"{PROG}: {partiality}", file=sys.stderr)
                return 2
            return 0
        except (OSError, ValueError) as error:
            if selected:
                try:
                    run_path = record_run(
                        archive,
                        command,
                        started,
                        "failed",
                        selected[0],
                        None,
                        None,
                        None,
                        error=str(error),
                        team_slugs=selected,
                    )
                    print(f"run metadata: {run_path}", file=sys.stderr)
                except (OSError, ValueError):
                    pass
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2
    if handler == "export":
        source_archive = _path(str(ns.archive))
        target = _path(str(ns.output))
        raw_teams: object = ns.team
        if not isinstance(raw_teams, list) or not all(
            isinstance(item, str) for item in raw_teams
        ):
            raise ValueError("--team values must be strings")
        team_slugs = tuple(raw_teams)
        team_slug = team_slugs[0]
        started = utc_now()
        command = [PROG, *args]
        try:
            window, rollup_kinds, display_timezone = _export_selection(
                ns, source_archive, team_slugs
            )
            export_report = (
                build_archive(
                    source_archive,
                    team_slug,
                    phase_minutes=int(ns.phase_minutes),
                    display_window=window,
                    rollup_kinds=rollup_kinds,
                    output=target,
                )
                if len(team_slugs) == 1
                else build_combined_archive(
                    source_archive,
                    team_slugs,
                    output=target,
                    display_timezone=display_timezone,
                    phase_minutes=int(ns.phase_minutes),
                    display_window=window,
                    rollup_kinds=rollup_kinds,
                )
            )
            run_path = record_run(
                target,
                command,
                started,
                "completed",
                team_slug,
                None,
                None,
                export_report,
                team_slugs=team_slugs,
                mechanical={
                    "website_export": {
                        "schema_version": 1,
                        "model_calls": 0,
                        "model_tokens": 0,
                        "website_build_performed": True,
                    }
                },
            )
            print(
                f"export: {len(team_slugs)} team(s), "
                f"{export_report['agents']} tracks, "
                f"{export_report['phases']} phases, "
                f"{export_report['rollups']} rollups; "
                f"{export_report['files_changed']} files changed"
            )
            print(f"run metadata: {run_path}")
            print(f"open: cd {target} && make serve")
            return 0
        except (OSError, ValueError) as error:
            try:
                run_path = record_run(
                    target,
                    command,
                    started,
                    "failed",
                    team_slug,
                    None,
                    None,
                    None,
                    error=str(error),
                    team_slugs=team_slugs,
                    mechanical={
                        "website_export": {
                            "schema_version": 1,
                            "model_calls": 0,
                            "model_tokens": 0,
                            "website_build_performed": False,
                        }
                    },
                )
                print(f"run metadata: {run_path}", file=sys.stderr)
            except (OSError, ValueError):
                pass
            print(f"{PROG}: {error}", file=sys.stderr)
            return 2

    archive = _path(str(ns.output))
    team_slug = str(ns.team)
    started = utc_now()
    command = [PROG, *args]
    ingest_report: IngestReport | None = None
    summary_report: SummarizeReport | None = None
    build_report: dict[str, int] | None = None
    try:
        ingest_handlers = (
            "ingest",
            "refresh",
            "ingest_claude",
            "refresh_claude",
            "ingest_claude_history",
            "ingest_orc",
            "refresh_orc",
        )
        if handler in ingest_handlers:
            identity_overrides = _identity_overrides(ns, args)
            date_window = parse_date_window(
                str(ns.start_date) if ns.start_date is not None else None,
                str(ns.end_date) if ns.end_date is not None else None,
                str(ns.timezone),
                start_time=(
                    str(ns.start_time) if ns.start_time is not None else None
                ),
                end_time=str(ns.end_time) if ns.end_time is not None else None,
            )
            requested_snapshot_root = (
                _path(str(ns.snapshot_root)) if ns.snapshot_root is not None else None
            )
            if handler in ("ingest_claude", "refresh_claude"):
                _, ingest_report = ingest_claude(
                    archive,
                    _path(str(ns.session_file)),
                    team_slug,
                    str(ns.timezone),
                    date_window,
                    identity_overrides,
                    requested_snapshot_root,
                )
            elif handler == "ingest_claude_history":
                _, ingest_report = ingest_claude_history(
                    archive,
                    _path(str(ns.history_file)),
                    team_slug,
                    str(ns.timezone),
                    str(ns.project_pattern),
                    _string_list(ns.covered_session, "--covered-session"),
                    date_window,
                    identity_overrides,
                    requested_snapshot_root,
                )
            elif handler in ("ingest_orc", "refresh_orc"):
                _, ingest_report = ingest_orc(
                    archive,
                    _path(str(ns.source_root)),
                    str(ns.root_session),
                    team_slug,
                    str(ns.timezone),
                    date_window,
                    identity_overrides,
                    tuple(
                        _orc_continuation_arg(str(item))
                        for item in ns.continuation_session
                    ),
                    _string_list(
                        ns.accept_orc_prefix_rewrite, "--accept-orc-prefix-rewrite"
                    ),
                    requested_snapshot_root,
                )
            else:
                _, ingest_report = ingest_codex(
                    archive,
                    _path(str(ns.sessions_root)),
                    str(ns.root_session),
                    team_slug,
                    str(ns.timezone),
                    date_window,
                    identity_overrides,
                    tuple(str(item) for item in ns.continuation_session),
                    requested_snapshot_root,
                )
            _print_ingest(ingest_report)
            _print_snapshot_root(ingest_report, archive)
            _print_history_exclusions(ingest_report)
            _print_retired_projections(ingest_report)
            _print_promoted_task_notes(ingest_report)
            _print_stored_payloads(ingest_report)
            _print_prefix_overrides(ingest_report)
        refresh_handlers = ("refresh", "refresh_claude", "refresh_orc")
        if handler == "summarize" or handler in refresh_handlers:
            summary_report = _summary_call(ns)
            _print_summaries(summary_report)
        if handler == "build" or handler in refresh_handlers:
            build_report = build_archive(
                archive, team_slug, phase_minutes=int(ns.phase_minutes)
            )
            print(
                f"build: {build_report['agents']} tracks, {build_report['phases']} phases, "
                f"{build_report['edges']} edges, {build_report['summary_files']} Markdown files; "
                f"{build_report['files_changed']} presentation files changed"
            )
        wants_github = handler == "github-metadata" or (
            handler in refresh_handlers and bool(ns.github_metadata)
        )
        if wants_github:
            github_report = enrich_pull_request_metadata(
                archive,
                team_slug,
                token=_github_token(ns),
                timeout_seconds=float(ns.github_timeout),
            )
            _print_github_metadata(github_report)
            build_report = build_archive(
                archive, team_slug, phase_minutes=int(ns.phase_minutes)
            )
            print(
                f"rebuild: {build_report['files_changed']} presentation files changed "
                "after GitHub metadata"
            )
            if github_report.failures:
                raise ValueError(
                    "GitHub metadata refresh had failures (successful records were retained): "
                    + "; ".join(github_report.failures)
                )
        run_path = record_run(
            archive,
            command,
            started,
            "completed",
            team_slug,
            ingest_report,
            summary_report,
            build_report,
        )
        print(f"run metadata: {run_path}")
        if build_report is not None:
            print(f"open: cd {archive} && make serve")
        return 0
    except KeyboardInterrupt:
        interruption = "interrupted by user"
        try:
            run_path = record_run(
                archive,
                command,
                started,
                "interrupted",
                team_slug,
                ingest_report,
                summary_report,
                build_report,
                error=interruption,
            )
            print(f"run metadata: {run_path}", file=sys.stderr)
        except (OSError, ValueError):
            pass
        print(f"{PROG}: {interruption}", file=sys.stderr)
        return 130
    except (
        AgentNameError,
        ClaudeParseError,
        CodexParseError,
        OrcParseError,
        SummaryError,
        OSError,
        ValueError,
    ) as error:
        try:
            run_path = record_run(
                archive,
                command,
                started,
                "failed",
                team_slug,
                ingest_report,
                summary_report,
                build_report,
                error=str(error),
            )
            print(f"run metadata: {run_path}", file=sys.stderr)
        except (OSError, ValueError):
            pass
        print(f"{PROG}: {error}", file=sys.stderr)
        return 2


__all__ = ["main"]
