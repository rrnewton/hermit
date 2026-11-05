"""Contract tests for the model-artifact logical-key catalog."""

from __future__ import annotations

from pathlib import Path

import pytest

from wrkviz.summary_artifacts import make_summary_provenance
from wrkviz.summary_catalog import (
    SummaryArtifactCatalog,
    SummaryArtifactReference,
    load_summary_catalog,
    merge_summary_catalog,
    select_summary_artifact,
)
from wrkviz.summary_registry import (
    ContextComponent,
    ContextCoverage,
    TECHNICAL_ROLLUP_SUMMARIZER,
)


def _reference(
    *,
    prompt_version: str,
    model: str,
    input_hash: str,
    coverage: int,
    generated_at: str,
) -> SummaryArtifactReference:
    provenance = make_summary_provenance(
        TECHNICAL_ROLLUP_SUMMARIZER,
        logical_key="rollup:daily:2026-08-07",
        team_slug="codex-widget",
        start_ms=100,
        end_ms=200,
        input_hash=input_hash,
        backend="codex",
        model=model,
        reasoning_effort="high",
        service_tier="priority",
        generated_at=generated_at,
        usage_receipt_id=f"receipt-{input_hash}",
        context_coverage=ContextCoverage(
            components=(
                ContextComponent("prior_days", 10, coverage, "summaries"),
            ),
            frontier_status="isolated-backfill",
        ),
        dependency_keys=("phase-input",),
        prompt_version=prompt_version,
    )
    return SummaryArtifactReference(
        provenance=provenance,
        cache_path=f"cache/{input_hash}.json",
    )


def test_catalog_merge_retains_versions_and_has_deterministic_counts(
    tmp_path: Path,
) -> None:
    version_one = _reference(
        prompt_version="agent-team-timeline-technical-rollup-v1",
        model="gpt-5.6-sol",
        input_hash="one",
        coverage=10,
        generated_at="2026-08-07T10:00:00Z",
    )
    version_two = _reference(
        prompt_version="agent-team-timeline-technical-rollup-v2",
        model="gpt-5.6-luna",
        input_hash="two",
        coverage=8,
        generated_at="2026-08-07T11:00:00Z",
    )
    path = tmp_path / "artifacts.json"

    first, changed = merge_summary_catalog(
        path, "codex-widget", (version_two, version_one)
    )
    replay, replay_changed = merge_summary_catalog(
        path, "codex-widget", (version_one, version_two)
    )

    assert changed is True
    assert replay_changed is False
    assert replay == first
    assert load_summary_catalog(path, "codex-widget") == first
    rendered = first.to_json_obj()
    assert rendered["artifact_count"] == 2
    assert rendered["logical_key_count"] == 1
    assert rendered["model_counts"] == {"gpt-5.6-luna": 1, "gpt-5.6-sol": 1}
    assert rendered["version_counts"] == {
        "technical-rollup@1/schema-1": 1,
        "technical-rollup@2/schema-1": 1,
    }


def test_catalog_selects_latest_compatible_or_requested_model() -> None:
    version_one = _reference(
        prompt_version="agent-team-timeline-technical-rollup-v1",
        model="gpt-5.6-sol",
        input_hash="one",
        coverage=10,
        generated_at="2026-08-07T10:00:00Z",
    )
    version_two = _reference(
        prompt_version="agent-team-timeline-technical-rollup-v2",
        model="gpt-5.6-luna",
        input_hash="two",
        coverage=5,
        generated_at="2026-08-07T11:00:00Z",
    )
    catalog = SummaryArtifactCatalog(
        team_slug="codex-widget",
        records=(version_one, version_two),
    )

    selected = select_summary_artifact(
        catalog,
        "rollup:daily:2026-08-07",
        "technical-rollup",
    )
    sol = select_summary_artifact(
        catalog,
        "rollup:daily:2026-08-07",
        "technical-rollup",
        preferred_model="gpt-5.6-sol",
    )

    assert selected == version_two
    assert sol == version_one
    assert (
        select_summary_artifact(catalog, "missing", "technical-rollup") is None
    )


def test_catalog_rejects_unsafe_paths_and_tampered_aggregates() -> None:
    reference = _reference(
        prompt_version="agent-team-timeline-technical-rollup-v2",
        model="gpt-5.6-luna",
        input_hash="two",
        coverage=5,
        generated_at="2026-08-07T11:00:00Z",
    )
    with pytest.raises(ValueError, match="unsafe"):
        SummaryArtifactReference(reference.provenance, "../outside.json")

    catalog = SummaryArtifactCatalog("codex-widget", (reference,))
    tampered = catalog.to_json_obj()
    tampered["artifact_count"] = 9
    with pytest.raises(ValueError, match="derived"):
        SummaryArtifactCatalog.from_json_obj(tampered, "catalog")
