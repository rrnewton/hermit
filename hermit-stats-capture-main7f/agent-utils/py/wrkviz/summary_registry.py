"""Registry and provenance vocabulary for every model-backed computation.

The registry is deliberately data-only. Prompt builders, cache runners, and archive writers import
the same immutable specifications so a new prompt contract cannot exist without a visible version
and changelog entry.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Final

from wrkviz.archive import JsonValue, as_array, as_int, as_object, as_string


PHASE_STYLE: Final = "phase"
TECHNICAL_ROLLUP_STYLE: Final = "technical-rollup"
PLAIN_LANGUAGE_ROLLUP_STYLE: Final = "plain-language-rollup"
PROJECT_OVERVIEW_STYLE: Final = "project-overview"
GLOSSARY_DEFINITION_STYLE: Final = "glossary-definition"
AGENT_LIFETIME_STYLE: Final = "agent-lifetime"


@dataclass(frozen=True, slots=True)
class SummarizerChange:
    """One user-visible prompt/output contract revision."""

    version: int
    prompt_version: str
    output_schema_version: int
    change: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return deterministic registry metadata."""

        return {
            "version": self.version,
            "prompt_version": self.prompt_version,
            "output_schema_version": self.output_schema_version,
            "change": self.change,
        }


@dataclass(frozen=True, slots=True)
class SummarizerSpec:
    """The selected contract and complete known version ledger for one model task."""

    summarizer_id: str
    summary_style: str
    current_version: int
    prompt_version: str
    output_schema_version: int
    description: str
    input_fields: tuple[str, ...]
    output_fields: tuple[str, ...]
    granularities: tuple[str, ...]
    changelog: tuple[SummarizerChange, ...]
    lifecycle: str = "active"

    def __post_init__(self) -> None:
        if not self.summarizer_id or not self.summary_style:
            raise ValueError("summarizer ID and style must not be empty")
        if self.current_version <= 0 or self.output_schema_version <= 0:
            raise ValueError("summarizer versions must be positive")
        if not self.changelog:
            raise ValueError(f"summarizer {self.summarizer_id!r} has no changelog")
        if self.lifecycle not in {"active", "historical-disabled", "planned"}:
            raise ValueError(
                f"summarizer {self.summarizer_id!r} has invalid lifecycle {self.lifecycle!r}"
            )
        versions = tuple(item.version for item in self.changelog)
        if versions != tuple(sorted(set(versions))):
            raise ValueError(
                f"summarizer {self.summarizer_id!r} changelog versions are not unique and ordered"
            )
        current = self.changelog[-1]
        if (
            current.version != self.current_version
            or current.prompt_version != self.prompt_version
            or current.output_schema_version != self.output_schema_version
        ):
            raise ValueError(
                f"summarizer {self.summarizer_id!r} current contract differs from its changelog"
            )

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return deterministic registry metadata for archive inspection."""

        return {
            "summarizer_id": self.summarizer_id,
            "summary_style": self.summary_style,
            "current_version": self.current_version,
            "prompt_version": self.prompt_version,
            "output_schema_version": self.output_schema_version,
            "description": self.description,
            "input_fields": list(self.input_fields),
            "output_fields": list(self.output_fields),
            "granularities": list(self.granularities),
            "changelog": [item.to_json_obj() for item in self.changelog],
            "lifecycle": self.lifecycle,
        }


@dataclass(frozen=True, slots=True)
class ContextComponent:
    """Availability of one optional context channel at generation time."""

    name: str
    requested: int
    provided: int
    unit: str

    def __post_init__(self) -> None:
        if not self.name or not self.unit:
            raise ValueError("context component name and unit must not be empty")
        if self.requested < 0 or self.provided < 0:
            raise ValueError("context component counts must not be negative")
        if self.provided > self.requested:
            raise ValueError("provided context must not exceed requested context")

    @property
    def coverage_basis_points(self) -> int:
        """Return this channel's coverage from 0 through 10,000 basis points."""

        if self.requested == 0:
            return 10_000
        return (self.provided * 10_000) // self.requested

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return deterministic context provenance."""

        return {
            "name": self.name,
            "requested": self.requested,
            "provided": self.provided,
            "unit": self.unit,
            "coverage_basis_points": self.coverage_basis_points,
        }

    @classmethod
    def from_json_obj(cls, value: JsonValue, where: str) -> ContextComponent:
        """Strictly parse one persisted context channel."""

        obj = as_object(value, where)
        expected = {
            "name",
            "requested",
            "provided",
            "unit",
            "coverage_basis_points",
        }
        if set(obj) != expected:
            raise ValueError(f"{where}: invalid context component fields")
        result = cls(
            name=as_string(obj.get("name"), f"{where}.name"),
            requested=as_int(obj.get("requested"), f"{where}.requested"),
            provided=as_int(obj.get("provided"), f"{where}.provided"),
            unit=as_string(obj.get("unit"), f"{where}.unit"),
        )
        recorded = as_int(
            obj.get("coverage_basis_points"), f"{where}.coverage_basis_points"
        )
        if recorded != result.coverage_basis_points:
            raise ValueError(f"{where}: context coverage does not match raw counts")
        return result


@dataclass(frozen=True, slots=True)
class ContextCoverage:
    """Comparable context completeness plus chronological frontier provenance."""

    components: tuple[ContextComponent, ...] = ()
    frontier_status: str = "not-applicable"
    predecessor_keys: tuple[str, ...] = ()
    known: bool = True

    def __post_init__(self) -> None:
        if self.frontier_status not in {
            "not-applicable",
            "project-start",
            "contiguous-extension",
            "isolated-backfill",
            "unknown-legacy",
        }:
            raise ValueError(f"unsupported frontier status {self.frontier_status!r}")
        names = tuple(item.name for item in self.components)
        if len(names) != len(set(names)):
            raise ValueError("context component names must be unique")
        if not self.known and self.components:
            raise ValueError("unknown context coverage cannot contain measured components")

    @property
    def coverage_basis_points(self) -> int | None:
        """Average optional-channel completeness without mixing unlike raw units."""

        if not self.known:
            return None
        if not self.components:
            return 10_000
        return sum(item.coverage_basis_points for item in self.components) // len(
            self.components
        )

    @property
    def coverage_percent(self) -> int | None:
        """Return a simple whole-number context coverage score."""

        basis_points = self.coverage_basis_points
        return None if basis_points is None else (basis_points + 50) // 100

    @property
    def missing_percent(self) -> int | None:
        """Return the complementary whole-number missing-context score."""

        coverage = self.coverage_percent
        return None if coverage is None else 100 - coverage

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return deterministic context and frontier metadata."""

        return {
            "known": self.known,
            "coverage_basis_points": self.coverage_basis_points,
            "coverage_percent": self.coverage_percent,
            "missing_percent": self.missing_percent,
            "frontier_status": self.frontier_status,
            "predecessor_keys": list(self.predecessor_keys),
            "components": [item.to_json_obj() for item in self.components],
        }

    @classmethod
    def unknown_legacy(cls) -> ContextCoverage:
        """Represent an artifact whose context measurements are unavailable."""

        return cls(frontier_status="unknown-legacy", known=False)

    @classmethod
    def from_json_obj(cls, value: JsonValue, where: str) -> ContextCoverage:
        """Strictly parse persisted context and verify all derived percentages."""

        obj = as_object(value, where)
        expected = {
            "known",
            "coverage_basis_points",
            "coverage_percent",
            "missing_percent",
            "frontier_status",
            "predecessor_keys",
            "components",
        }
        if set(obj) != expected:
            raise ValueError(f"{where}: invalid context coverage fields")
        known = obj.get("known")
        if not isinstance(known, bool):
            raise ValueError(f"{where}.known: expected a boolean")
        components = tuple(
            ContextComponent.from_json_obj(item, f"{where}.components[{index}]")
            for index, item in enumerate(
                as_array(obj.get("components"), f"{where}.components")
            )
        )
        result = cls(
            components=components,
            frontier_status=as_string(
                obj.get("frontier_status"), f"{where}.frontier_status"
            ),
            predecessor_keys=tuple(
                as_string(item, f"{where}.predecessor_keys[{index}]")
                for index, item in enumerate(
                    as_array(
                        obj.get("predecessor_keys"), f"{where}.predecessor_keys"
                    )
                )
            ),
            known=known,
        )
        for key, expected_value in (
            ("coverage_basis_points", result.coverage_basis_points),
            ("coverage_percent", result.coverage_percent),
            ("missing_percent", result.missing_percent),
        ):
            raw = obj.get(key)
            if expected_value is None:
                if raw is not None:
                    raise ValueError(f"{where}.{key}: expected null")
            elif as_int(raw, f"{where}.{key}") != expected_value:
                raise ValueError(f"{where}.{key}: derived value mismatch")
        return result


#: PROMPT VERSIONS KEEP THE TOOL'S FORMER NAME, DELIBERATELY. Do not "fix" them.
#:
#: A prompt version identifies A PARTICULAR PROMPT, not the program that sent it. It is the cache
#: key for summaries that cost real money to produce: `SummaryRecord` treats a changed
#: `prompt_version` as "this was generated by a different prompt and must be regenerated". So
#: renaming these strings does not tidy anything -- it declares every paid summary in every
#: existing archive stale, and the next `summarize` spends tokens reproducing text that was
#: already correct.
#:
#: This was caught the hard way: the wrkviz rename rewrote them along with everything else, and
#: the build refused with "summarizer 'agent-lifetime' has no prompt version
#: 'agent-team-timeline-agent-name-v2'" -- which is the cache telling the truth about a mismatch
#: the rename had invented. They are reverted and pinned here.
#:
#: When a prompt's TEXT changes, bump the number. That is the only reason these strings ever move.

PHASE_SUMMARIZER: Final = SummarizerSpec(
    summarizer_id="phase-work-summary",
    summary_style=PHASE_STYLE,
    current_version=2,
    prompt_version="agent-team-timeline-summary-v2",
    output_schema_version=1,
    description="Phrase, hover paragraph, and chronological work bullets for one agent phase.",
    input_fields=(
        "bounded phase transcript",
        "ancestor transcript scroll-back",
        "chronologically available glossary",
        "phase activity statistics",
    ),
    output_fields=("phrase", "paragraph", "work_summary"),
    granularities=("phase",),
    changelog=(
        SummarizerChange(
            1,
            "agent-team-timeline-summary-v1",
            1,
            "Initial substantive phase summary with three display resolutions.",
        ),
        SummarizerChange(
            2,
            "agent-team-timeline-summary-v2",
            1,
            "Use only supported semantic concepts; exclude mechanical glossary candidates.",
        ),
    ),
)

TECHNICAL_ROLLUP_SUMMARIZER: Final = SummarizerSpec(
    summarizer_id="technical-rollup",
    summary_style=TECHNICAL_ROLLUP_STYLE,
    current_version=3,
    prompt_version="agent-team-timeline-technical-rollup-v3",
    output_schema_version=1,
    description="Content-led technical rollup over lower-level cached summaries.",
    input_fields=(
        "lower-level summaries or uncovered phase summaries",
        "up to ten prior same-level technical summaries",
        "chronologically available glossary",
        "aggregate activity statistics",
    ),
    output_fields=("phrase", "paragraph", "work_summary"),
    granularities=("hourly", "daily", "weekly", "monthly", "quarterly"),
    changelog=(
        SummarizerChange(
            1,
            "agent-team-timeline-technical-rollup-v1",
            1,
            "Initial calendar super-summary.",
        ),
        SummarizerChange(
            2,
            "agent-team-timeline-technical-rollup-v2",
            1,
            "Require content before work-management identifiers and expand opaque shorthand.",
        ),
        SummarizerChange(
            3,
            "agent-team-timeline-technical-rollup-v3",
            1,
            "Use only supported semantic concepts; exclude mechanical glossary candidates.",
        ),
    ),
)

PLAIN_LANGUAGE_ROLLUP_SUMMARIZER: Final = SummarizerSpec(
    summarizer_id="plain-language-rollup",
    summary_style=PLAIN_LANGUAGE_ROLLUP_STYLE,
    current_version=4,
    prompt_version="agent-team-timeline-plain-rollup-v4",
    output_schema_version=1,
    description="Newcomer-oriented rewrite grounded in the same-period technical summary.",
    input_fields=(
        "same-period technical summary as the factual source of truth",
        "lower-level plain-language summaries or uncovered phase summaries",
        "up to ten prior same-level plain-language summaries",
        "project overview",
        "supported chronologically available glossary definitions",
        "aggregate activity statistics",
    ),
    output_fields=("phrase", "paragraph", "work_summary"),
    granularities=("hourly", "daily", "weekly", "monthly", "quarterly"),
    changelog=(
        SummarizerChange(
            1,
            "agent-team-timeline-plain-rollup-v1",
            1,
            "Initial separate plain-language calendar summary.",
        ),
        SummarizerChange(
            2,
            "agent-team-timeline-plain-rollup-v2",
            1,
            "Ground newcomer explanations in the project overview and verified glossary.",
        ),
        SummarizerChange(
            3,
            "agent-team-timeline-plain-rollup-v3",
            1,
            "Use only supported semantic concepts; exclude mechanical glossary candidates.",
        ),
        SummarizerChange(
            4,
            "agent-team-timeline-plain-rollup-v4",
            1,
            "Ground status, outcomes, counts, and scope in the same-period technical summary.",
        ),
    ),
)

PROJECT_OVERVIEW_SUMMARIZER: Final = SummarizerSpec(
    summarizer_id="project-overview",
    summary_style=PROJECT_OVERVIEW_STYLE,
    current_version=2,
    prompt_version="agent-team-timeline-project-overview-v2",
    output_schema_version=1,
    description="Durable newcomer overview from bounded early coordinator evidence.",
    input_fields=("up to 48,000 characters of early root transcript",),
    output_fields=("support_status", "paragraph"),
    granularities=("project",),
    changelog=(
        SummarizerChange(
            1,
            "agent-team-timeline-project-overview-v1",
            1,
            "Initial project knowledge summary.",
        ),
        SummarizerChange(
            2,
            "agent-team-timeline-project-overview-v2",
            1,
            "Freeze evidence frontiers and require explicit insufficient-evidence results.",
        ),
    ),
)

GLOSSARY_DEFINITION_SUMMARIZER: Final = SummarizerSpec(
    summarizer_id="glossary-definition",
    summary_style=GLOSSARY_DEFINITION_STYLE,
    current_version=2,
    prompt_version="agent-team-timeline-glossary-definition-v2",
    output_schema_version=1,
    description="Evidence-bounded newcomer definition for one deterministic glossary term.",
    input_fields=(
        "exact glossary spelling",
        "up to six retained source occurrences",
        "frozen project overview",
    ),
    output_fields=("support_status", "definition"),
    granularities=("term",),
    changelog=(
        SummarizerChange(
            1,
            "agent-team-timeline-glossary-definition-v1",
            1,
            "Initial model-backed glossary definition.",
        ),
        SummarizerChange(
            2,
            "agent-team-timeline-glossary-definition-v2",
            1,
            "Bind definitions to frozen occurrences and reject unsupported inference.",
        ),
    ),
    lifecycle="historical-disabled",
)

AGENT_LIFETIME_SUMMARIZER: Final = SummarizerSpec(
    summarizer_id="agent-lifetime",
    summary_style=AGENT_LIFETIME_STYLE,
    current_version=2,
    prompt_version="agent-team-timeline-agent-name-v2",
    output_schema_version=2,
    description="Hindsight short name and lifetime paragraph for one agent identity.",
    input_fields=(
        "official path, coordinator nickname, role, depth, and parent path",
        "ancestor transcript scroll-back",
        "all available phase work summaries for the agent lifetime",
    ),
    output_fields=("short_name", "rationale", "lifetime_summary"),
    granularities=("agent-lifetime",),
    changelog=(
        SummarizerChange(
            1,
            "agent-team-timeline-agent-name-v1",
            1,
            "Initial hindsight short name.",
        ),
        SummarizerChange(
            2,
            "agent-team-timeline-agent-name-v2",
            2,
            "Add the substantive lifetime summary used by agent hover cards.",
        ),
    ),
)

SUMMARIZER_REGISTRY: Final = (
    PHASE_SUMMARIZER,
    AGENT_LIFETIME_SUMMARIZER,
    PROJECT_OVERVIEW_SUMMARIZER,
    GLOSSARY_DEFINITION_SUMMARIZER,
    TECHNICAL_ROLLUP_SUMMARIZER,
    PLAIN_LANGUAGE_ROLLUP_SUMMARIZER,
)

_BY_STYLE: Final = {item.summary_style: item for item in SUMMARIZER_REGISTRY}
_BY_ID: Final = {item.summarizer_id: item for item in SUMMARIZER_REGISTRY}

if len(_BY_STYLE) != len(SUMMARIZER_REGISTRY):
    raise ValueError("summarizer registry contains duplicate styles")
if len({item.summarizer_id for item in SUMMARIZER_REGISTRY}) != len(
    SUMMARIZER_REGISTRY
):
    raise ValueError("summarizer registry contains duplicate IDs")


def summarizer_for_style(summary_style: str) -> SummarizerSpec:
    """Return the registered contract for an internal summary style."""

    try:
        return _BY_STYLE[summary_style]
    except KeyError as error:
        raise ValueError(f"unregistered summary style {summary_style!r}") from error


def summarizer_for_id(summarizer_id: str) -> SummarizerSpec:
    """Return the registered contract for a durable summarizer ID."""

    try:
        return _BY_ID[summarizer_id]
    except KeyError as error:
        raise ValueError(f"unregistered summarizer ID {summarizer_id!r}") from error


def summarizer_version_for_prompt(spec: SummarizerSpec, prompt_version: str) -> int:
    """Resolve a prompt identifier to its registered numeric version."""

    for change in spec.changelog:
        if change.prompt_version == prompt_version:
            return change.version
    raise ValueError(
        f"summarizer {spec.summarizer_id!r} has no prompt version {prompt_version!r}"
    )


def summarizer_change_for_prompt(
    spec: SummarizerSpec, prompt_version: str
) -> SummarizerChange:
    """Return the complete registered contract selected by a prompt identifier."""

    for change in spec.changelog:
        if change.prompt_version == prompt_version:
            return change
    raise ValueError(
        f"summarizer {spec.summarizer_id!r} has no prompt version {prompt_version!r}"
    )


def registry_json_obj() -> dict[str, JsonValue]:
    """Return the complete deterministic registry for docs and archive manifests."""

    return {
        "schema_version": 2,
        "summarizers": [item.to_json_obj() for item in SUMMARIZER_REGISTRY],
    }


__all__ = [
    "AGENT_LIFETIME_STYLE",
    "AGENT_LIFETIME_SUMMARIZER",
    "ContextComponent",
    "ContextCoverage",
    "GLOSSARY_DEFINITION_STYLE",
    "GLOSSARY_DEFINITION_SUMMARIZER",
    "PHASE_STYLE",
    "PHASE_SUMMARIZER",
    "PLAIN_LANGUAGE_ROLLUP_STYLE",
    "PLAIN_LANGUAGE_ROLLUP_SUMMARIZER",
    "PROJECT_OVERVIEW_STYLE",
    "PROJECT_OVERVIEW_SUMMARIZER",
    "SUMMARIZER_REGISTRY",
    "SummarizerChange",
    "SummarizerSpec",
    "TECHNICAL_ROLLUP_STYLE",
    "TECHNICAL_ROLLUP_SUMMARIZER",
    "registry_json_obj",
    "summarizer_for_id",
    "summarizer_for_style",
    "summarizer_change_for_prompt",
    "summarizer_version_for_prompt",
]
