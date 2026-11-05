"""Contract tests for registered model-backed timeline computations."""

from __future__ import annotations

import pytest

from wrkviz.summary_artifacts import (
    SummaryArtifactProvenance,
    make_summary_provenance,
)
from wrkviz.summary_registry import (
    ContextComponent,
    ContextCoverage,
    GLOSSARY_DEFINITION_SUMMARIZER,
    PHASE_STYLE,
    PHASE_SUMMARIZER,
    SUMMARIZER_REGISTRY,
    registry_json_obj,
    summarizer_for_style,
)


def test_registry_has_unique_complete_current_contracts() -> None:
    assert len(SUMMARIZER_REGISTRY) == 6
    assert len({item.summarizer_id for item in SUMMARIZER_REGISTRY}) == 6
    assert len({item.summary_style for item in SUMMARIZER_REGISTRY}) == 6
    for spec in SUMMARIZER_REGISTRY:
        assert spec.current_version == spec.changelog[-1].version
        assert spec.prompt_version == spec.changelog[-1].prompt_version
        assert spec.input_fields
        assert spec.output_fields
        assert spec.lifecycle in {"active", "historical-disabled", "planned"}
        assert summarizer_for_style(spec.summary_style) is spec

    manifest = registry_json_obj()
    assert manifest["schema_version"] == 2
    summarizers = manifest["summarizers"]
    assert isinstance(summarizers, list)
    assert len(summarizers) == 6
    assert summarizer_for_style(PHASE_STYLE) is PHASE_SUMMARIZER
    assert PHASE_SUMMARIZER.lifecycle == "active"
    assert GLOSSARY_DEFINITION_SUMMARIZER.lifecycle == "historical-disabled"


def test_unknown_summary_style_fails_closed() -> None:
    with pytest.raises(ValueError, match="unregistered summary style"):
        summarizer_for_style("future-unversioned-task")


def test_context_coverage_averages_channels_without_mixing_units() -> None:
    coverage = ContextCoverage(
        components=(
            ContextComponent("ancestor_transcript", 16_000, 8_000, "characters"),
            ContextComponent("prior_days", 10, 8, "summaries"),
        ),
        frontier_status="isolated-backfill",
        predecessor_keys=("daily:2026-08-05",),
    )

    assert coverage.coverage_basis_points == 6_500
    assert coverage.coverage_percent == 65
    assert coverage.missing_percent == 35
    assert coverage.to_json_obj()["frontier_status"] == "isolated-backfill"


def test_context_coverage_rejects_impossible_provenance() -> None:
    with pytest.raises(ValueError, match="must not exceed"):
        ContextComponent("prior_weeks", 4, 5, "summaries")
    with pytest.raises(ValueError, match="unique"):
        ContextCoverage(
            components=(
                ContextComponent("prior_days", 1, 1, "summaries"),
                ContextComponent("prior_days", 1, 1, "summaries"),
            )
        )


def test_context_coverage_round_trips_known_and_unknown_records() -> None:
    known = ContextCoverage(
        components=(ContextComponent("prior_days", 10, 7, "summaries"),),
        frontier_status="isolated-backfill",
        predecessor_keys=("daily:2026-08-05",),
    )
    unknown = ContextCoverage.unknown_legacy()

    assert ContextCoverage.from_json_obj(known.to_json_obj(), "known") == known
    assert ContextCoverage.from_json_obj(unknown.to_json_obj(), "unknown") == unknown
    assert unknown.coverage_percent is None


def test_summary_artifact_provenance_has_stable_validated_identity() -> None:
    provenance = make_summary_provenance(
        PHASE_SUMMARIZER,
        logical_key="phase:one",
        team_slug="codex-widget",
        start_ms=100,
        end_ms=200,
        input_hash="abc123",
        backend="codex",
        model="gpt-5.6-luna",
        reasoning_effort="high",
        service_tier="priority",
        generated_at="2026-08-07T15:00:00Z",
        usage_receipt_id="receipt-1",
        context_coverage=ContextCoverage(
            components=(ContextComponent("ancestor_transcript", 16_000, 8_000, "characters"),)
        ),
        dependency_keys=("summary-parent",),
    )

    restored = SummaryArtifactProvenance.from_json_obj(
        provenance.to_json_obj(), "artifact"
    )
    assert restored == provenance
    assert restored.artifact_id.startswith("summary-")
    assert restored.summarizer_version == PHASE_SUMMARIZER.current_version
    assert restored.model == "gpt-5.6-luna"

    tampered = provenance.to_json_obj()
    tampered["model"] = "gpt-5.6-sol"
    with pytest.raises(ValueError, match="artifact ID"):
        SummaryArtifactProvenance.from_json_obj(tampered, "artifact")
