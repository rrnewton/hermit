"""Shared immutable provenance for every model-backed timeline artifact."""

from __future__ import annotations

from dataclasses import dataclass

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    content_hash,
)
from wrkviz.summary_registry import (
    ContextCoverage,
    SummarizerSpec,
    summarizer_change_for_prompt,
    summarizer_for_id,
)


#: KEEPS THE TOOL'S FORMER NAME, for the same reason the prompt versions in `summary_registry`
#: do: it is written INTO the paid artifacts, and every one already on disk carries this exact
#: string. Renaming it makes each of them fail the envelope check -- observed as
#: "cataloged artifact lacks its common envelope" against a cache that was perfectly intact.
#:
#: The general rule the rename established: an identifier the tool STORES is part of the data
#: format, not part of the tool's name, and it moves only when the thing it identifies changes.
ARTIFACT_ENVELOPE_FORMAT = "agent-team-timeline-model-artifact"
ARTIFACT_ENVELOPE_VERSION = 1


def _optional_string(value: JsonValue, where: str) -> str | None:
    if value is None:
        return None
    return as_string(value, where)


def _artifact_identity(
    *,
    summarizer_id: str,
    summarizer_version: int,
    prompt_version: str,
    output_schema_version: int,
    logical_key: str,
    team_slug: str,
    start_ms: int,
    end_ms: int,
    input_hash: str,
    backend: str,
    model: str,
    reasoning_effort: str | None,
    service_tier: str | None,
) -> dict[str, JsonValue]:
    return {
        "summarizer_id": summarizer_id,
        "summarizer_version": summarizer_version,
        "prompt_version": prompt_version,
        "output_schema_version": output_schema_version,
        "logical_key": logical_key,
        "team_slug": team_slug,
        "start_ms": start_ms,
        "end_ms": end_ms,
        "input_hash": input_hash,
        "backend": backend,
        "model": model,
        "reasoning_effort": reasoning_effort,
        "service_tier": service_tier,
    }


def _artifact_id(identity: dict[str, JsonValue]) -> str:
    return "summary-" + content_hash(canonical_json(identity))


@dataclass(frozen=True, slots=True)
class SummaryArtifactProvenance:
    """Version, context, dependency, model, and receipt identity for one result."""

    artifact_id: str
    summarizer_id: str
    summarizer_version: int
    prompt_version: str
    output_schema_version: int
    logical_key: str
    team_slug: str
    start_ms: int
    end_ms: int
    input_hash: str
    backend: str
    model: str
    reasoning_effort: str | None
    service_tier: str | None
    generated_at: str
    usage_receipt_id: str | None
    context_coverage: ContextCoverage
    dependency_keys: tuple[str, ...]
    legacy_storage: bool

    def __post_init__(self) -> None:
        spec = summarizer_for_id(self.summarizer_id)
        change = summarizer_change_for_prompt(spec, self.prompt_version)
        if change.version != self.summarizer_version:
            raise ValueError("summarizer version does not match prompt version")
        if change.output_schema_version != self.output_schema_version:
            raise ValueError("output schema does not match summarizer version")
        if self.end_ms < self.start_ms:
            raise ValueError("summary artifact ends before it starts")
        if not self.logical_key or not self.team_slug or not self.input_hash:
            raise ValueError("summary artifact identity fields must not be empty")
        expected_id = _artifact_id(self.identity_json_obj())
        if self.artifact_id != expected_id:
            raise ValueError("summary artifact ID does not match its identity")

    def identity_json_obj(self) -> dict[str, JsonValue]:
        """Return the fields that determine immutable artifact identity."""

        return _artifact_identity(
            summarizer_id=self.summarizer_id,
            summarizer_version=self.summarizer_version,
            prompt_version=self.prompt_version,
            output_schema_version=self.output_schema_version,
            logical_key=self.logical_key,
            team_slug=self.team_slug,
            start_ms=self.start_ms,
            end_ms=self.end_ms,
            input_hash=self.input_hash,
            backend=self.backend,
            model=self.model,
            reasoning_effort=self.reasoning_effort,
            service_tier=self.service_tier,
        )

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return complete persisted provenance."""

        result = self.identity_json_obj()
        result.update(
            {
                "artifact_id": self.artifact_id,
                "generated_at": self.generated_at,
                "usage_receipt_id": self.usage_receipt_id,
                "context_coverage": self.context_coverage.to_json_obj(),
                "dependency_keys": list(self.dependency_keys),
                "legacy_storage": self.legacy_storage,
            }
        )
        return result

    @classmethod
    def from_json_obj(
        cls, value: JsonValue, where: str
    ) -> SummaryArtifactProvenance:
        """Strictly parse and identity-validate persisted provenance."""

        obj = as_object(value, where)
        expected = {
            "artifact_id",
            "summarizer_id",
            "summarizer_version",
            "prompt_version",
            "output_schema_version",
            "logical_key",
            "team_slug",
            "start_ms",
            "end_ms",
            "input_hash",
            "backend",
            "model",
            "reasoning_effort",
            "service_tier",
            "generated_at",
            "usage_receipt_id",
            "context_coverage",
            "dependency_keys",
            "legacy_storage",
        }
        if set(obj) != expected:
            raise ValueError(f"{where}: invalid summary provenance fields")
        legacy_storage = obj.get("legacy_storage")
        if not isinstance(legacy_storage, bool):
            raise ValueError(f"{where}.legacy_storage: expected a boolean")
        return cls(
            artifact_id=as_string(obj.get("artifact_id"), f"{where}.artifact_id"),
            summarizer_id=as_string(
                obj.get("summarizer_id"), f"{where}.summarizer_id"
            ),
            summarizer_version=as_int(
                obj.get("summarizer_version"), f"{where}.summarizer_version"
            ),
            prompt_version=as_string(
                obj.get("prompt_version"), f"{where}.prompt_version"
            ),
            output_schema_version=as_int(
                obj.get("output_schema_version"),
                f"{where}.output_schema_version",
            ),
            logical_key=as_string(obj.get("logical_key"), f"{where}.logical_key"),
            team_slug=as_string(obj.get("team_slug"), f"{where}.team_slug"),
            start_ms=as_int(obj.get("start_ms"), f"{where}.start_ms"),
            end_ms=as_int(obj.get("end_ms"), f"{where}.end_ms"),
            input_hash=as_string(obj.get("input_hash"), f"{where}.input_hash"),
            backend=as_string(obj.get("backend"), f"{where}.backend"),
            model=as_string(obj.get("model"), f"{where}.model"),
            reasoning_effort=_optional_string(
                obj.get("reasoning_effort"), f"{where}.reasoning_effort"
            ),
            service_tier=_optional_string(
                obj.get("service_tier"), f"{where}.service_tier"
            ),
            generated_at=as_string(obj.get("generated_at"), f"{where}.generated_at"),
            usage_receipt_id=_optional_string(
                obj.get("usage_receipt_id"), f"{where}.usage_receipt_id"
            ),
            context_coverage=ContextCoverage.from_json_obj(
                obj.get("context_coverage"), f"{where}.context_coverage"
            ),
            dependency_keys=tuple(
                as_string(item, f"{where}.dependency_keys[{index}]")
                for index, item in enumerate(
                    as_array(obj.get("dependency_keys"), f"{where}.dependency_keys")
                )
            ),
            legacy_storage=legacy_storage,
        )


def make_summary_provenance(
    spec: SummarizerSpec,
    *,
    logical_key: str,
    team_slug: str,
    start_ms: int,
    end_ms: int,
    input_hash: str,
    backend: str,
    model: str,
    reasoning_effort: str | None,
    service_tier: str | None,
    generated_at: str,
    usage_receipt_id: str | None,
    context_coverage: ContextCoverage,
    dependency_keys: tuple[str, ...],
    legacy_storage: bool = False,
    prompt_version: str | None = None,
) -> SummaryArtifactProvenance:
    """Construct identity-checked provenance for a registered prompt version."""

    selected_prompt = prompt_version or spec.prompt_version
    change = summarizer_change_for_prompt(spec, selected_prompt)
    identity = _artifact_identity(
        summarizer_id=spec.summarizer_id,
        summarizer_version=change.version,
        prompt_version=change.prompt_version,
        output_schema_version=change.output_schema_version,
        logical_key=logical_key,
        team_slug=team_slug,
        start_ms=start_ms,
        end_ms=end_ms,
        input_hash=input_hash,
        backend=backend,
        model=model,
        reasoning_effort=reasoning_effort,
        service_tier=service_tier,
    )
    return SummaryArtifactProvenance(
        artifact_id=_artifact_id(identity),
        summarizer_id=spec.summarizer_id,
        summarizer_version=change.version,
        prompt_version=change.prompt_version,
        output_schema_version=change.output_schema_version,
        logical_key=logical_key,
        team_slug=team_slug,
        start_ms=start_ms,
        end_ms=end_ms,
        input_hash=input_hash,
        backend=backend,
        model=model,
        reasoning_effort=reasoning_effort,
        service_tier=service_tier,
        generated_at=generated_at,
        usage_receipt_id=usage_receipt_id,
        context_coverage=context_coverage,
        dependency_keys=dependency_keys,
        legacy_storage=legacy_storage,
    )


__all__ = [
    "ARTIFACT_ENVELOPE_FORMAT",
    "ARTIFACT_ENVELOPE_VERSION",
    "SummaryArtifactProvenance",
    "make_summary_provenance",
]
