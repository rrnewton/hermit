"""Read-only audit of retired mechanical glossary projections.

Schema-3 ``glossary.json`` files are immutable cost/provenance records.  Their model result answers
"can this string be explained from its occurrences?", not "is this a durable project concept?".
This module therefore never grants publication authority to a retired entry.  It only separates
definite mechanical noise from plausible inputs to a future semantic-discovery pass.
"""

from __future__ import annotations

import re
from collections import Counter
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from wrkviz.build_store import team_build_root as _build_root
from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    read_json,
)
from wrkviz.terminology import glossary_term_id

LegacyDisposition = Literal["rejected-mechanical", "semantic-review-required"]
SemanticKind = Literal["project", "system", "workstream", "milestone"]

AUDIT_SCHEMA_VERSION = 1
PUBLICATION_POLICY = "semantic-only-v1"

_TEAM_SLUG = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
_COMMAND_FIELD = re.compile(
    r"(?:local-)?command-(?:args|caveat|message|name|stdout|stderr)",
    re.IGNORECASE,
)
_DURATION = re.compile(r"(?:\d+(?:\.\d+)?|N)[smhdw]", re.IGNORECASE)
_HEX_LITERAL = re.compile(r"[0-9a-f]{7,64}", re.IGNORECASE)
_HEX_RANGE = re.compile(r"[0-9a-f]{7,64}\.\.[0-9a-f]{7,64}", re.IGNORECASE)
_ISSUE_COMMENT = re.compile(r"issuecomment-[0-9]+", re.IGNORECASE)
_ENV_ASSIGNMENT = re.compile(r"[A-Z][A-Z0-9_]{2,}=")
_METHOD_OR_CALL = re.compile(r"(?:::|\(\)|\([^)]*\))")
_ERRNO_NAME = re.compile(r"E[A-Z0-9_]{3,}")
_TEAM_OR_RECYCLED_AGENT = re.compile(
    r"(?:(?:claude|codex|orc)-coord(?:-[0-9]+)?|agent-[0-9]+|"
    r"goal-[a-z0-9-]+)",
    re.IGNORECASE,
)
_OPERATIONAL_DEFINITION = re.compile(
    r"\b(?:is|means|names)\s+(?:an?\s+|the\s+)?(?:`[^`]+`\s+)?"
    r"(?:command|option|flag|field|method|function|branch|label|team|"
    r"environment variable)\b",
    re.IGNORECASE,
)
_SCHEDULE_EXAMPLE = re.compile(
    r"(?:every\s+(?:\d+|N)\s*(?:seconds?|minutes?|hours?|days?|weeks?)|"
    r"(?:check|run)\s+.+)",
    re.IGNORECASE,
)

# These are ordinary language or generic workflow labels, not named durable concepts.  Keep the
# set intentionally small and syntax-independent; ambiguous leftovers still remain unpublished.
_GENERIC_LANGUAGE = frozenset(
    {
        "and",
        "or",
        "true",
        "false",
        "prompt",
        "recurring",
        "human-readable",
        "whitespace-delimited",
        "anti-pattern",
        "as-is",
        "case-by-case",
        "codex-led",
        "counter-example",
        "cross-backend",
        "dev-repo",
        "in-guest",
        "half-finished",
        "in-process",
        "in-repo",
        "multi-day",
        "main",
        "no-progress",
        "non-green",
        "non-functional",
        "one-line",
        "output-file",
        "per-category",
        "per-coordinator",
        "per-session",
        "per-syscall",
        "per-team",
        "per-thread",
        "pre-commit",
        "post-facto",
        "project-specific",
        "read-only",
        "self-hosted",
        "session-scoped",
        "the-coordinator",
        "top-level",
        "agent-friendly",
        "agent-driven",
        "best-in-class",
        "bot-created",
        "bpf-based",
        "built-in",
        "compile-time",
        "continuous-virtual-time-is-sacred",
        "fail-closed",
        "fetch-and-add",
        "guest-visible",
        "human-created",
        "human-review",
        "locally-validated",
        "non-leftmost",
        "syscall-heavy",
        "wall-clock",
    }
)

# Inflected commands and transient coordination actions can look like attractive hyphenated
# "terms" to a regex.  They are useful transcript text, but not project ontology entries.
_PROCESS_ACTIONS = frozenset(
    {
        "auto-clears",
        "auto-expire",
        "auto-skip",
        "battle-testing",
        "fast-land",
        "ff-merged",
        "force-push",
        "get-target",
        "kicking-the-tires",
        "list-hosts",
        "over-index",
        "pre-work",
        "re-measured",
        "run-on-every",
        "support-yet",
        "testing-no-reboots",
        "throw-away",
    }
)

_GENERIC_ACRONYMS = frozenset(
    {
        "API",
        "ARGUMENT",
        "BAD",
        "CI",
        "CLI",
        "CLAUDE",
        "CPU",
        "DRY",
        "FAIL",
        "FAST",
        "HW",
        "JSON",
        "JSONL",
        "OSS",
        "P0",
        "PR",
        "TUI",
        "UH",
        "WAY",
    }
)

_KIND_PATTERNS: tuple[tuple[SemanticKind, re.Pattern[str]], ...] = (
    (
        "project",
        re.compile(
            r"\b(?:repository|repo|workspace|codebase|development tree)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "system",
        re.compile(
            r"\b(?:system|subsystem|backend|runtime|engine|tool|server|service|"
            r"framework|library|interface|protocol|platform|scheduler|runner|"
            r"benchmark|workload|crate|package|matrix|instrumentation|front[ -]?end|"
            r"test suite)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "workstream",
        re.compile(
            r"\b(?:workstream|task|initiative|track|investigation|audit|migration|"
            r"landing workflow)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "milestone",
        re.compile(
            r"\b(?:milestone|release|achievement|shipped|landed|completed)\b",
            re.IGNORECASE,
        ),
    ),
)


@dataclass(frozen=True)
class LegacyTermAudit:
    """One retired term's mechanical triage; never a publication decision."""

    term: str
    term_id: str
    definition_status: str
    disposition: LegacyDisposition
    reason: str
    semantic_kind_hints: tuple[SemanticKind, ...]

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return a stable machine-readable audit record."""

        return {
            "term": self.term,
            "term_id": self.term_id,
            "definition_status": self.definition_status,
            "disposition": self.disposition,
            "reason": self.reason,
            "semantic_kind_hints": list(self.semantic_kind_hints),
            "publication_authority": False,
        }


@dataclass(frozen=True)
class LegacyTeamGlossaryAudit:
    """Read-only audit of one team's retired glossary projection."""

    team_slug: str
    cache_path: str
    cache_present: bool
    cache_schema_version: int | None
    terms: tuple[LegacyTermAudit, ...]

    @property
    def rejected_count(self) -> int:
        """Return the number of definite mechanical candidates."""

        return sum(item.disposition == "rejected-mechanical" for item in self.terms)

    @property
    def review_count(self) -> int:
        """Return plausible semantic candidates that still require classification."""

        return sum(
            item.disposition == "semantic-review-required" for item in self.terms
        )

    def reason_counts(self) -> dict[str, int]:
        """Return deterministic rejection/review reason counts."""

        return dict(sorted(Counter(item.reason for item in self.terms).items()))

    def to_json_obj(self, *, include_terms: bool) -> dict[str, JsonValue]:
        """Return a stable machine-readable team report."""

        result: dict[str, JsonValue] = {
            "team_slug": self.team_slug,
            "cache_path": self.cache_path,
            "cache_present": self.cache_present,
            "cache_schema_version": self.cache_schema_version,
            "legacy_candidates": len(self.terms),
            "rejected_mechanical": self.rejected_count,
            "semantic_review_required": self.review_count,
            "published_from_legacy_cache": 0,
            "reason_counts": {
                reason: count for reason, count in self.reason_counts().items()
            },
        }
        if include_terms:
            result["terms"] = [item.to_json_obj() for item in self.terms]
        return result


@dataclass(frozen=True)
class LegacyGlossaryAuditReport:
    """Archive-wide compatibility audit of retired glossary caches."""

    archive: str
    teams: tuple[LegacyTeamGlossaryAudit, ...]

    @property
    def legacy_candidates(self) -> int:
        """Return the number of retired mechanically selected strings."""

        return sum(len(team.terms) for team in self.teams)

    @property
    def rejected_count(self) -> int:
        """Return the number of candidates mechanically rejected as noise."""

        return sum(team.rejected_count for team in self.teams)

    @property
    def review_count(self) -> int:
        """Return the number needing a semantic discovery/classification pass."""

        return sum(team.review_count for team in self.teams)

    def to_json_obj(self, *, include_terms: bool = False) -> dict[str, JsonValue]:
        """Return a stable machine-readable archive report."""

        return {
            "schema_version": AUDIT_SCHEMA_VERSION,
            "archive": self.archive,
            "publication_policy": PUBLICATION_POLICY,
            "legacy_cache_publication_authority": False,
            "totals": {
                "teams": len(self.teams),
                "legacy_candidates": self.legacy_candidates,
                "rejected_mechanical": self.rejected_count,
                "semantic_review_required": self.review_count,
                "published_from_legacy_cache": 0,
            },
            "teams": [
                team.to_json_obj(include_terms=include_terms) for team in self.teams
            ],
        }


def _semantic_kind_hints(definition: str) -> tuple[SemanticKind, ...]:
    return tuple(
        kind for kind, pattern in _KIND_PATTERNS if pattern.search(definition)
    )


def _mechanical_rejection_reason(
    term: str, definition_status: str, definition: str, context: str
) -> str | None:
    clean = term.strip()
    folded = clean.casefold()
    if definition_status != "supported":
        return "definition-not-supported"
    if _COMMAND_FIELD.fullmatch(clean) or f"<{clean}>".casefold() in context.casefold():
        return "transcript-markup-field"
    if folded in _GENERIC_LANGUAGE or clean in _GENERIC_ACRONYMS:
        return "generic-language-or-workflow"
    if folded in _PROCESS_ACTIONS:
        return "process-action"
    if _DURATION.fullmatch(clean) or _SCHEDULE_EXAMPLE.fullmatch(clean):
        return "duration-or-command-example"
    if (
        _HEX_LITERAL.fullmatch(clean)
        or _HEX_RANGE.fullmatch(clean)
        or _ISSUE_COMMENT.fullmatch(clean)
        or _ERRNO_NAME.fullmatch(clean)
        or _TEAM_OR_RECYCLED_AGENT.fullmatch(clean)
    ):
        return "opaque-record-identifier"
    if (
        not clean
        or clean[0] in ".),"
        or clean[-1] in ",:("
        or "," in clean
        or ":" in clean
        or "..." in clean
        or len(clean.split()) > 8
    ):
        return "opaque-or-prose-fragment"
    if (
        "_" in clean
        or _ENV_ASSIGNMENT.search(clean)
        or _METHOD_OR_CALL.search(clean)
        or "=" in clean
    ):
        return "code-or-configuration-literal"
    if folded.startswith(("sudo ", "with-proxy ")):
        return "operational-command"
    if _OPERATIONAL_DEFINITION.search(definition[:240]):
        return "operational-command-or-label"
    return None


def audit_legacy_term(
    term: str,
    term_id: str,
    definition_status: str,
    definition: str,
    context: str,
) -> LegacyTermAudit:
    """Triage one retired entry without ever making it linkable."""

    expected_id = glossary_term_id(term)
    if term_id != expected_id:
        raise ValueError(
            f"legacy glossary ID {term_id!r} does not match term {term!r}"
        )
    rejection = _mechanical_rejection_reason(
        term, definition_status, definition, context
    )
    hints = _semantic_kind_hints(definition)
    if rejection is not None:
        return LegacyTermAudit(
            term,
            term_id,
            definition_status,
            "rejected-mechanical",
            rejection,
            (),
        )
    if not hints:
        return LegacyTermAudit(
            term,
            term_id,
            definition_status,
            "rejected-mechanical",
            "no-semantic-kind-evidence",
            (),
        )
    return LegacyTermAudit(
        term,
        term_id,
        definition_status,
        "semantic-review-required",
        "legacy-schema-lacks-semantic-classification",
        hints,
    )


def _selected_teams(archive: Path, team_slugs: Sequence[str]) -> tuple[str, ...]:
    if team_slugs:
        selected = tuple(team_slugs)
        if len(set(selected)) != len(selected):
            raise ValueError("glossary audit team selection contains duplicates")
    else:
        teams_root = archive / "teams"
        if not teams_root.is_dir():
            raise ValueError(f"no team directory found at {teams_root}")
        selected = tuple(
            sorted(
                path.name
                for path in teams_root.iterdir()
                if path.is_dir() and not path.is_symlink()
            )
        )
    if not selected:
        raise ValueError(f"no teams found in {archive}")
    for team_slug in selected:
        if _TEAM_SLUG.fullmatch(team_slug) is None:
            raise ValueError(f"invalid team slug {team_slug!r}")
    return selected


def _audit_team(archive: Path, team_slug: str) -> LegacyTeamGlossaryAudit:
    path = archive / "teams" / team_slug / "summary_data" / "glossary.json"
    relative_path = str(path.relative_to(archive))
    if path.is_symlink():
        raise ValueError(f"refusing symlinked legacy glossary cache: {path}")
    if not path.is_file():
        return LegacyTeamGlossaryAudit(team_slug, relative_path, False, None, ())
    root = as_object(read_json(path), str(path))
    schema_version = as_int(root.get("schema_version"), f"{path}.schema_version")
    if schema_version != 3:
        raise ValueError(
            f"{path}: audit supports retired glossary schema 3, found {schema_version}"
        )
    terms: list[LegacyTermAudit] = []
    for index, raw_term in enumerate(as_array(root.get("terms"), f"{path}.terms")):
        where = f"{path}.terms[{index}]"
        item = as_object(raw_term, where)
        terms.append(
            audit_legacy_term(
                as_string(item.get("term"), f"{where}.term"),
                as_string(item.get("term_id"), f"{where}.term_id"),
                as_string(
                    item.get("definition_status"), f"{where}.definition_status"
                ),
                as_string(item.get("definition"), f"{where}.definition"),
                as_string(item.get("context"), f"{where}.context"),
            )
        )
    return LegacyTeamGlossaryAudit(
        team_slug, relative_path, True, schema_version, tuple(terms)
    )


def audit_legacy_glossaries(
    archive: Path, team_slugs: Sequence[str] = ()
) -> LegacyGlossaryAuditReport:
    """Audit retired caches without taking a lock, writing files, or calling a model."""

    selected = _selected_teams(archive, team_slugs)
    return LegacyGlossaryAuditReport(
        archive=str(archive.resolve()),
        teams=tuple(_audit_team(archive, team_slug) for team_slug in selected),
    )


def format_glossary_audit(
    report: LegacyGlossaryAuditReport,
    output_format: Literal["json", "text"],
    *,
    include_terms: bool = False,
) -> str:
    """Format a deterministic read-only audit report."""

    if output_format == "json":
        return canonical_json(report.to_json_obj(include_terms=include_terms))
    if output_format != "text":
        raise ValueError(f"unsupported glossary audit format {output_format!r}")
    lines = [
        f"Glossary publication policy: {PUBLICATION_POLICY}",
        "Legacy schema-3 definitions have no publication authority; builds link 0 of them.",
        (
            f"Archive total: {report.legacy_candidates} legacy candidates; "
            f"{report.rejected_count} mechanically rejected; "
            f"{report.review_count} require semantic review; 0 published"
        ),
    ]
    for team in report.teams:
        if not team.cache_present:
            lines.append(f"{team.team_slug}: no legacy glossary cache")
            continue
        lines.append(
            f"{team.team_slug}: {len(team.terms)} candidates; "
            f"{team.rejected_count} rejected; {team.review_count} review; 0 published"
        )
        lines.extend(
            f"  {reason}: {count}"
            for reason, count in team.reason_counts().items()
        )
        if include_terms:
            lines.extend(
                f"  [{item.disposition}] {item.term}: {item.reason}"
                for item in team.terms
            )
    return "\n".join(lines) + "\n"


__all__ = [
    "AUDIT_SCHEMA_VERSION",
    "PUBLICATION_POLICY",
    "LegacyGlossaryAuditReport",
    "LegacyTeamGlossaryAudit",
    "LegacyTermAudit",
    "audit_legacy_glossaries",
    "audit_legacy_term",
    "format_glossary_audit",
]
