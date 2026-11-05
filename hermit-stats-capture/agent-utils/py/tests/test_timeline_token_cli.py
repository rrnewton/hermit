"""CLI contract for exact summary token-cost reporting."""

from __future__ import annotations

from pathlib import Path

import pytest

import wrkviz.cli as timeline_cli
from wrkviz.pipeline import SummarizeReport
from wrkviz.token_usage import TokenUsage


def _report() -> SummarizeReport:
    return SummarizeReport(
        backend="codex",
        model="gpt-5.6-sol",
        reasoning_effort="xhigh",
        service_tier="priority",
        phases=4,
        rollups=1,
        agent_names=2,
        glossary_terms=3,
        project_overviews=1,
        glossary_definitions=3,
        catalog_artifacts=13,
        cache_hits=1,
        cache_misses=6,
        backend_batches=2,
        newly_spent_usage=TokenUsage(
            input_tokens=1_000,
            cached_input_tokens=600,
            cache_write_input_tokens=10,
            output_tokens=80,
            reasoning_output_tokens=30,
        ),
        newly_spent_unknown_receipts=1,
        artifact_generation_usage=TokenUsage(
            input_tokens=1_400,
            cached_input_tokens=800,
            cache_write_input_tokens=10,
            output_tokens=100,
            reasoning_output_tokens=40,
        ),
        artifact_generation_unknown_receipts=2,
        unknown_legacy_artifacts=3,
        usage_run_paths=("teams/test/summary_data/cache/_usage/runs/one.json",),
        files_changed=7,
    )


def test_summary_output_separates_new_spend_from_artifact_cost(
    capsys: pytest.CaptureFixture[str],
) -> None:
    timeline_cli._print_summaries(_report())
    output = capsys.readouterr().out
    assert (
        "tokens newly spent: input=1000, cached_input=600, cache_write_input=10, "
        "output=80, reasoning_output=30, total=1080; unknown_receipts=1"
    ) in output
    assert (
        "tokens behind returned cached artifacts: input=1400, cached_input=800, "
        "cache_write_input=10, output=100, reasoning_output=40, total=1500; "
        "unknown_receipts=2"
    ) in output
    assert "3 legacy artifact(s) have no usage receipt" in output


def test_reasoning_effort_and_service_tier_reach_pipeline(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    seen: dict[str, object] = {}

    def fake_summarize_archive(
        archive: Path,
        team_slug: str,
        backend: str,
        model: str,
        **kwargs: object,
    ) -> SummarizeReport:
        seen.update(
            archive=archive,
            team_slug=team_slug,
            backend=backend,
            model=model,
            reasoning_effort=kwargs.get("reasoning_effort"),
            service_tier=kwargs.get("service_tier"),
            summary_window=kwargs.get("summary_window"),
            rollup_kinds=kwargs.get("rollup_kinds"),
        )
        return _report()

    monkeypatch.setattr(timeline_cli, "summarize_archive", fake_summarize_archive)
    parser = timeline_cli._parser()
    ns = parser.parse_args(
        [
            "summarize",
            "--output",
            str(tmp_path),
            "--team",
            "test-team",
            "--model",
            "model-under-test",
            "--reasoning-effort",
            "high",
            "--service-tier",
            "priority",
            "--summary-start-time",
            "2026-08-07T02:00:00Z",
            "--summary-end-time",
            "2026-08-07T03:00:00Z",
            "--rollup-kind",
            "hourly",
        ]
    )
    timeline_cli._summary_call(ns)
    assert seen["model"] == "model-under-test"
    assert seen["reasoning_effort"] == "high"
    assert seen["service_tier"] == "priority"
    summary_window = seen["summary_window"]
    assert summary_window is not None
    assert getattr(summary_window, "start_ms") == 1_786_068_000_000
    assert getattr(summary_window, "end_ms") == 1_786_071_600_000
    assert seen["rollup_kinds"] == ("hourly",)
    default_ns = parser.parse_args(
        ["summarize", "--output", str(tmp_path), "--team", "test-team"]
    )
    assert default_ns.service_tier is None


def test_default_summary_model_is_supported_configuration(
    capsys: pytest.CaptureFixture[str],
) -> None:
    parser = timeline_cli._parser()
    ns = parser.parse_args(
        ["summarize", "--output", "unused", "--team", "test-team"]
    )

    assert ns.backend == "codex"
    # The parser keeps omission visible so Claude can require an explicit model;
    # `_summary_call` resolves the Codex omission to DEFAULT_MODEL.
    assert ns.model is None
    assert ns.reasoning_effort == "medium"

    with pytest.raises(SystemExit) as raised:
        timeline_cli.main(["summarize", "--help"])
    assert raised.value.code == 0
    help_text = " ".join(capsys.readouterr().out.split())
    assert "instead of selecting another model or backend" in help_text
