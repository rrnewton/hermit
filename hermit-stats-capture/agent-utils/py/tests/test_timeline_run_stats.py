"""Human-readable accounting bundled into each generated timeline archive."""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path

import wrkviz.run_stats as run_stats
import pytest


def _usage(input_tokens: int, output_tokens: int) -> dict[str, object]:
    return {
        "input_tokens": input_tokens,
        "cached_input_tokens": 0,
        "cache_write_input_tokens": 0,
        "output_tokens": output_tokens,
        "reasoning_output_tokens": 0,
        "total_tokens": input_tokens + output_tokens,
    }


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def _run(
    run_id: str,
    started_at: str,
    completed_at: str,
    status: str,
    summaries: dict[str, object] | None,
    team_slug: str = "codex-coord",
) -> dict[str, object]:
    return {
        "schema_version": 1,
        "run_id": run_id,
        "tool_version": "test",
        "started_at": started_at,
        "completed_at": completed_at,
        "status": status,
        "team_slug": team_slug,
        "command": ["wrkviz", "summarize", "--team", team_slug],
        "ingest": {
            "sources": 2,
            "source_bytes": 4096,
            "agents": 3,
            "events": 9,
            "tool_calls": 4,
            "edges": 2,
            "files_changed": 1,
        },
        "summaries": summaries,
        "build": {"agents": 3, "phases": 5, "files_changed": 7},
        "error": "backend stopped" if status == "failed" else None,
    }


def _summary() -> dict[str, object]:
    return {
        "backend": "codex",
        "model": "gpt-5.6-sol",
        "reasoning_effort": "xhigh",
        "service_tier": "priority",
        "phases": 5,
        "agent_names": 3,
        "rollups": 2,
        "project_overviews": 1,
        "glossary_definitions": 4,
        "cache_hits": 6,
        "cache_misses": 9,
        "backend_batches": 2,
        "newly_spent_usage": _usage(100, 10),
        "newly_spent_unknown_receipts": 0,
        "artifact_generation_usage": _usage(140, 14),
        "artifact_generation_unknown_receipts": 1,
        "unknown_legacy_artifacts": 2,
        "usage_run_paths": ["teams/codex-coord/summary_data/cache/_usage/runs/a.json"],
        "files_changed": 9,
    }


def _receipt(
    _label: str,
    status: str,
    started_at: str,
    completed_at: str,
    usage: dict[str, object] | None,
    service_tier: str | None = None,
    schema_version: int | None = None,
) -> dict[str, object]:
    version = (
        schema_version
        if schema_version is not None
        else (2 if service_tier is not None else 1)
    )
    identity: dict[str, object] = {
        "schema_version": version,
        "backend": "codex",
        "model": "gpt-5.6-sol",
        "reasoning_effort": "xhigh",
        "status": status,
        "started_at": started_at,
        "completed_at": completed_at,
        "input_hashes": ["abc"],
        "usage": usage,
        "error": None if status == "completed" else "failed",
    }
    if version == 2:
        identity["service_tier"] = service_tier
    canonical = json.dumps(
        identity, ensure_ascii=False, indent=2, sort_keys=True
    ) + "\n"
    result = dict(identity)
    result["receipt_id"] = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
    return result


def _archive(tmp_path: Path) -> Path:
    archive = tmp_path / "archive"
    _write_json(
        archive / "runs" / "001.json",
        _run(
            "001",
            "2026-08-06T00:00:00Z",
            "2026-08-06T00:01:00Z",
            "completed",
            _summary(),
        ),
    )
    _write_json(
        archive / "runs" / "002.json",
        _run(
            "002",
            "2026-08-06T00:01:30Z",
            "2026-08-06T00:02:00Z",
            "failed",
            None,
        ),
    )
    receipts = (
        (
            "successful",
            "completed",
            "2026-08-06T00:00:01Z",
            "2026-08-06T00:00:02Z",
            _usage(100, 10),
        ),
        (
            "completed-in-failed-run",
            "completed",
            "2026-08-06T00:01:31Z",
            "2026-08-06T00:01:32Z",
            _usage(7, 3),
        ),
        (
            "failed-unknown",
            "failed",
            "2026-08-06T00:01:33Z",
            "2026-08-06T00:01:34Z",
            None,
        ),
    )
    receipt_root = (
        archive
        / "teams"
        / "codex-coord"
        / "summary_data"
        / "cache"
        / "_usage"
        / "receipts"
    )
    for receipt_id, status, started_at, completed_at, usage in receipts:
        _write_json(
            receipt_root / f"{receipt_id}.json",
            _receipt(receipt_id, status, started_at, completed_at, usage),
        )
    return archive


def test_report_separates_run_spend_from_complete_backend_ledger(
    tmp_path: Path,
) -> None:
    archive = _archive(tmp_path)
    report = run_stats.render_run_stats(archive)

    assert (
        "Top-level runs: 2 (1 completed, 1 failed, 0 other; "
        "2 summarize/refresh attempts, 1 with successful summary reports)"
    ) in report
    assert "Receipts: 3; usage known=2; usage unknown=1" in report
    assert "Actual tokens spent: UNKNOWN (1 receipt lacks usage)" in report
    assert "known subtotal from 2 receipts: total=120; input=107" in report
    assert "attempts=3 (2 completed, 1 failed, 0 other)" in report
    assert "tier=<unspecified>: attempts=3" in report
    assert "Reported newly spent: total=110; input=100" in report
    assert "Ledger difference: +10 known tokens and +1 unknown-usage receipts" in report
    assert "backend receipts attributed by timestamp: 1 (1 completed, 0 failed" in report
    assert "actual model-token spend for this run: total=110" in report
    assert "backend receipts attributed by timestamp: 2 (1 completed, 1 failed" in report
    assert "actual model-token spend for this run: UNKNOWN (1 receipt lacks usage)" in report
    assert "known subtotal from 1 receipt: total=10; input=7" in report
    assert "successful summary report, tokens newly spent: total=110" in report
    assert "summarize: codex / gpt-5.6-sol / effort=xhigh / tier=priority" in report
    assert "returned-artifact generation cost (not new spend): UNKNOWN" in report
    assert "known subtotal: total=154" in report
    assert "2 legacy artifacts also lack provenance" in report
    assert "[001] 2026-08-06T00:01:00Z  COMPLETED" in report
    assert "[002] 2026-08-06T00:02:00Z  FAILED" in report
    assert "run log: runs/001.json" in report


def test_receipt_ledger_separates_legacy_and_priority_service_tiers(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "service-tiers"
    receipt_root = (
        archive
        / "teams"
        / "codex-coord"
        / "summary_data"
        / "cache"
        / "_usage"
        / "receipts"
    )
    _write_json(
        receipt_root / "legacy.json",
        _receipt(
            "legacy",
            "completed",
            "2026-08-06T00:00:01Z",
            "2026-08-06T00:00:02Z",
            _usage(10, 1),
        ),
    )
    _write_json(
        receipt_root / "priority.json",
        _receipt(
            "priority",
            "completed",
            "2026-08-06T00:00:03Z",
            "2026-08-06T00:00:04Z",
            _usage(20, 2),
            service_tier="priority",
        ),
    )

    report = run_stats.render_run_stats(archive)

    assert "effort=xhigh / tier=<unspecified>: attempts=1" in report
    assert "effort=xhigh / tier=priority: attempts=1" in report


def test_receipt_ledger_keeps_missing_and_literal_unspecified_tiers_distinct(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "literal-unspecified"
    receipt_root = (
        archive
        / "teams"
        / "codex-coord"
        / "summary_data"
        / "cache"
        / "_usage"
        / "receipts"
    )
    _write_json(
        receipt_root / "legacy.json",
        _receipt(
            "legacy",
            "completed",
            "2026-08-06T00:00:01Z",
            "2026-08-06T00:00:02Z",
            _usage(10, 1),
        ),
    )
    _write_json(
        receipt_root / "literal.json",
        _receipt(
            "literal",
            "completed",
            "2026-08-06T00:00:03Z",
            "2026-08-06T00:00:04Z",
            _usage(20, 2),
            service_tier="unspecified",
        ),
    )

    report = run_stats.render_run_stats(archive)

    assert "tier=<unspecified>: attempts=1" in report
    assert "tier=unspecified: attempts=1" in report


def test_v2_null_tier_loads_and_malformed_or_tampered_receipts_are_skipped(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "receipt-validation"
    receipt_root = (
        archive
        / "teams"
        / "codex-coord"
        / "summary_data"
        / "cache"
        / "_usage"
        / "receipts"
    )
    valid = _receipt(
        "v2-null",
        "completed",
        "2026-08-06T00:00:01Z",
        "2026-08-06T00:00:02Z",
        _usage(10, 1),
        schema_version=2,
    )
    missing_tier = _receipt(
        "missing-tier",
        "completed",
        "2026-08-06T00:00:03Z",
        "2026-08-06T00:00:04Z",
        _usage(20, 2),
        service_tier="priority",
    )
    missing_tier.pop("service_tier")
    tampered = _receipt(
        "tampered",
        "completed",
        "2026-08-06T00:00:05Z",
        "2026-08-06T00:00:06Z",
        _usage(30, 3),
        service_tier="priority",
    )
    tampered["service_tier"] = "default"
    empty_tier = _receipt(
        "empty-tier",
        "completed",
        "2026-08-06T00:00:07Z",
        "2026-08-06T00:00:08Z",
        _usage(40, 4),
        service_tier="",
    )
    _write_json(receipt_root / "valid.json", valid)
    _write_json(receipt_root / "missing.json", missing_tier)
    _write_json(receipt_root / "tampered.json", tampered)
    _write_json(receipt_root / "empty.json", empty_tier)

    report = run_stats.render_run_stats(archive)

    assert "Receipts: 1; usage known=1; usage unknown=0" in report
    assert "Warnings (3)" in report
    assert "expected exactly" in report
    assert "content hash mismatch" in report
    assert "service_tier: must not be empty" in report


def test_team_scoped_attribution_disambiguates_overlapping_team_runs(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "two-teams"
    for team_slug in ("team-a", "team-b"):
        _write_json(
            archive / "runs" / f"{team_slug}.json",
            _run(
                team_slug,
                "2026-08-06T00:00:00Z",
                "2026-08-06T00:01:00Z",
                "failed",
                None,
                team_slug,
            ),
        )
        receipt = _receipt(
            team_slug,
            "completed",
            "2026-08-06T00:00:10Z",
            "2026-08-06T00:00:20Z",
            _usage(10, 1),
        )
        _write_json(
            archive
            / "teams"
            / team_slug
            / "summary_data"
            / "cache"
            / "_usage"
            / "receipts"
            / f"{team_slug}.json",
            receipt,
        )

    runs, _ = run_stats._load_runs(archive)
    receipts, _ = run_stats._load_receipts(archive)
    associated, unattributed, warnings = run_stats._associate_receipts(
        runs, receipts
    )

    assert not warnings
    assert not unattributed
    assert {item.team_slug for item in associated["team-a"]} == {"team-a"}
    assert {item.team_slug for item in associated["team-b"]} == {"team-b"}


def test_same_team_overlapping_windows_leave_receipt_unattributed(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "overlap"
    for run_id, completed_at in (
        ("first", "2026-08-06T00:01:00Z"),
        ("second", "2026-08-06T00:02:00Z"),
    ):
        _write_json(
            archive / "runs" / f"{run_id}.json",
            _run(
                run_id,
                "2026-08-06T00:00:00Z",
                completed_at,
                "failed",
                None,
            ),
        )
    _write_json(
        archive
        / "teams"
        / "codex-coord"
        / "summary_data"
        / "cache"
        / "_usage"
        / "receipts"
        / "ambiguous.json",
        _receipt(
            "ambiguous",
            "completed",
            "2026-08-06T00:00:10Z",
            "2026-08-06T00:00:20Z",
            _usage(10, 1),
        ),
    )

    runs, _ = run_stats._load_runs(archive)
    receipts, _ = run_stats._load_receipts(archive)
    associated, unattributed, warnings = run_stats._associate_receipts(
        runs, receipts
    )

    assert associated == {"first": (), "second": ()}
    assert len(unattributed) == 1
    assert any("matches 2 summarize runs" in warning for warning in warnings)


def test_summary_report_without_loadable_receipts_makes_archive_total_unknown(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "summary-only"
    _write_json(
        archive / "runs" / "001.json",
        _run(
            "001",
            "2026-08-06T00:00:00Z",
            "2026-08-06T00:01:00Z",
            "completed",
            _summary(),
        ),
    )

    report = run_stats.render_run_stats(archive)

    assert (
        "Actual tokens spent: UNKNOWN (no loadable backend receipts); "
        "successful-report cross-check: total=110"
    ) in report
    assert (
        "actual model-token spend for this run: UNKNOWN "
        "(no loadable backend receipts)"
    ) in report


def test_all_hit_run_with_zero_reported_spend_keeps_per_run_zero(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "all-hit"
    summary = _summary()
    summary["cache_hits"] = 15
    summary["cache_misses"] = 0
    summary["backend_batches"] = 0
    summary["newly_spent_usage"] = _usage(0, 0)
    _write_json(
        archive / "runs" / "001.json",
        _run(
            "001",
            "2026-08-06T00:00:00Z",
            "2026-08-06T00:01:00Z",
            "completed",
            summary,
        ),
    )

    report = run_stats.render_run_stats(archive)

    assert "actual model-token spend for this run: total=0" in report
    assert "Actual tokens spent: UNKNOWN (no loadable backend receipts)" in report


def test_failed_run_without_receipts_or_summary_report_is_unknown(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "failed-without-report"
    _write_json(
        archive / "runs" / "001.json",
        _run(
            "001",
            "2026-08-06T00:00:00Z",
            "2026-08-06T00:01:00Z",
            "failed",
            None,
        ),
    )

    report = run_stats.render_run_stats(archive)

    assert (
        "Actual tokens spent: UNKNOWN (no loadable backend receipts); "
        "no successful summary report is available"
    ) in report
    assert (
        "actual model-token spend for this run: UNKNOWN "
        "(no loadable backend receipts and no successful summary report)"
    ) in report


@pytest.mark.parametrize(
    "field",
    (
        "cache_hits",
        "cache_misses",
        "backend_batches",
        "newly_spent_unknown_receipts",
        "artifact_generation_unknown_receipts",
        "unknown_legacy_artifacts",
    ),
)
def test_negative_unknown_counts_are_rejected(tmp_path: Path, field: str) -> None:
    archive = tmp_path / field
    summary = _summary()
    summary[field] = -1
    _write_json(
        archive / "runs" / "001.json",
        _run(
            "001",
            "2026-08-06T00:00:00Z",
            "2026-08-06T00:01:00Z",
            "completed",
            summary,
        ),
    )

    with pytest.raises(ValueError, match=field + ": expected a non-negative"):
        run_stats.render_run_stats(archive)


def test_unknown_only_receipts_never_render_as_zero_actual_cost(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "unknown-only"
    _write_json(
        archive / "runs" / "001.json",
        _run(
            "001",
            "2026-08-06T00:00:00Z",
            "2026-08-06T00:01:00Z",
            "failed",
            None,
        ),
    )
    receipt_path = (
        archive
        / "teams"
        / "codex-coord"
        / "summary_data"
        / "cache"
        / "_usage"
        / "receipts"
        / "unknown.json"
    )
    _write_json(
        receipt_path,
        _receipt(
            "unknown",
            "failed",
            "2026-08-06T00:00:01Z",
            "2026-08-06T00:00:02Z",
            None,
        ),
    )

    report = run_stats.render_run_stats(archive)

    assert "Actual tokens spent: UNKNOWN (1 receipt lacks usage)" in report
    assert "no known token subtotal was reported" in report
    assert (
        "Reported newly spent: unavailable (no successful summary reports)" in report
    )
    assert "Actual tokens spent: total=0" not in report
    assert "known subtotal: total=0" not in report


def test_bundled_script_defaults_to_its_own_archive_directory(
    tmp_path: Path,
) -> None:
    archive = _archive(tmp_path)
    source_path = Path(run_stats.__file__)
    script = archive / "run_stats.py"
    script.write_text(source_path.read_text(encoding="utf-8"), encoding="utf-8")

    completed = subprocess.run(
        [sys.executable, str(script)],
        text=True,
        capture_output=True,
        check=False,
        cwd=tmp_path,
    )

    assert completed.returncode == 0, completed.stderr
    assert f"Archive: {archive.resolve()}" in completed.stdout
    assert "Actual tokens spent: UNKNOWN" in completed.stdout
