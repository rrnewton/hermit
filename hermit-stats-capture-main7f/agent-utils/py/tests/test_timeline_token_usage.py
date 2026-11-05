"""Tests for strict model-token accounting and immutable receipts."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue
from wrkviz.token_usage import (
    BatchUsageReceipt,
    TokenUsage,
    content_hash_from_json,
    load_batch_receipt,
    parse_codex_jsonl_usage,
    resolve_service_tier,
    write_batch_receipt,
    write_usage_run_receipt,
)


def test_parse_codex_jsonl_usage_ignores_banner_and_uses_completed_turn() -> None:
    output = "\n".join(
        (
            "Codex CLI banner",
            json.dumps({"type": "turn.started"}),
            json.dumps(
                {
                    "type": "turn.completed",
                    "usage": {
                        "input_tokens": 10_631,
                        "cached_input_tokens": 7_424,
                        "cache_write_input_tokens": 0,
                        "output_tokens": 5,
                        "reasoning_output_tokens": 0,
                    },
                }
            ),
        )
    )
    usage = parse_codex_jsonl_usage(output)
    assert usage == TokenUsage(
        input_tokens=10_631,
        cached_input_tokens=7_424,
        output_tokens=5,
    )
    assert usage.total_tokens == 10_636


def test_parse_installed_codex_usage_with_only_guaranteed_fields() -> None:
    output = json.dumps(
        {
            "type": "turn.completed",
            "usage": {
                "input_tokens": 10_631,
                "cached_input_tokens": 7_424,
                "output_tokens": 5,
            },
        }
    )
    assert parse_codex_jsonl_usage(output) == TokenUsage(
        input_tokens=10_631,
        cached_input_tokens=7_424,
        output_tokens=5,
    )


def test_absent_completed_turn_is_explicitly_unknown() -> None:
    assert parse_codex_jsonl_usage('{"type":"turn.started"}\n') is None


@pytest.mark.parametrize(
    "missing",
    ("input_tokens", "cached_input_tokens", "output_tokens"),
)
def test_installed_codex_required_usage_fields_remain_strict(missing: str) -> None:
    usage = {
        "input_tokens": 10,
        "cached_input_tokens": 4,
        "output_tokens": 2,
    }
    del usage[missing]
    output = json.dumps({"type": "turn.completed", "usage": usage})
    with pytest.raises(ValueError, match=missing):
        parse_codex_jsonl_usage(output)


def test_invalid_completed_usage_is_rejected() -> None:
    output = json.dumps(
        {
            "type": "turn.completed",
            "usage": {
                "input_tokens": 3,
                "cached_input_tokens": 4,
                "cache_write_input_tokens": 0,
                "output_tokens": 1,
                "reasoning_output_tokens": 0,
            },
        }
    )
    with pytest.raises(ValueError, match="cached input"):
        parse_codex_jsonl_usage(output)


def test_batch_receipt_round_trips_and_detects_tampering(tmp_path: Path) -> None:
    receipt = BatchUsageReceipt.create(
        backend="codex",
        model="gpt-5.6-sol",
        reasoning_effort="xhigh",
        service_tier="priority",
        status="completed",
        started_at="2026-08-05T20:00:00+00:00",
        completed_at="2026-08-05T20:00:01+00:00",
        input_hashes=("a", "b"),
        usage=TokenUsage(input_tokens=20, cached_input_tokens=5, output_tokens=3),
        error=None,
    )
    root = tmp_path / "usage"
    path = write_batch_receipt(root, receipt)
    assert load_batch_receipt(root, receipt.receipt_id) == receipt
    raw = json.loads(path.read_text(encoding="utf-8"))
    assert raw["schema_version"] == 2
    assert raw["service_tier"] == "priority"
    raw["usage"]["output_tokens"] = 4
    path.write_text(json.dumps(raw), encoding="utf-8")
    assert load_batch_receipt(root, receipt.receipt_id) is None


def test_unknown_usage_is_not_serialized_as_zero(tmp_path: Path) -> None:
    receipt = BatchUsageReceipt.create(
        backend="codex",
        model="legacy-test",
        reasoning_effort=None,
        status="completed",
        started_at="2026-08-05T20:00:00+00:00",
        completed_at="2026-08-05T20:00:01+00:00",
        input_hashes=("a",),
        usage=None,
        error=None,
    )
    path = write_batch_receipt(tmp_path, receipt)
    raw = json.loads(path.read_text(encoding="utf-8"))
    assert raw["usage"] is None


def test_effective_service_tier_defaults_and_rejects_non_codex_tiers() -> None:
    assert resolve_service_tier("codex", None) == "default"
    assert resolve_service_tier("codex", "default") == "default"
    assert resolve_service_tier("codex", "priority") == "priority"
    assert resolve_service_tier("heuristic", None) is None
    with pytest.raises(ValueError, match="only supported by the codex backend"):
        resolve_service_tier("heuristic", "priority")


def test_usage_run_receipt_service_tier_keyword_remains_optional(
    tmp_path: Path,
) -> None:
    accounting = write_usage_run_receipt(
        tmp_path,
        started_at="2026-08-05T20:00:00+00:00",
        completed_at="2026-08-05T20:00:01+00:00",
        backend="heuristic",
        model="offline",
        reasoning_effort=None,
        job_count=0,
        hits=0,
        misses=0,
        new_receipts=(),
        artifact_receipts=(),
        unreadable_artifact_receipt_ids=(),
        unknown_legacy_artifacts=0,
    )

    raw = json.loads(accounting.path.read_text(encoding="utf-8"))
    assert raw["service_tier"] is None


def test_v1_batch_receipt_loads_with_unspecified_service_tier(
    tmp_path: Path,
) -> None:
    legacy_identity: dict[str, JsonValue] = {
        "schema_version": 1,
        "backend": "codex",
        "model": "gpt-5.6-sol",
        "reasoning_effort": "xhigh",
        "status": "completed",
        "started_at": "2026-08-05T20:00:00+00:00",
        "completed_at": "2026-08-05T20:00:01+00:00",
        "input_hashes": ["legacy-input"],
        "usage": TokenUsage(input_tokens=20, output_tokens=3).to_json(),
        "error": None,
    }
    receipt_id = content_hash_from_json(legacy_identity)
    legacy_receipt = dict(legacy_identity)
    legacy_receipt["receipt_id"] = receipt_id
    root = tmp_path / "usage"
    path = root / "receipts" / f"{receipt_id}.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(legacy_receipt), encoding="utf-8")

    loaded = load_batch_receipt(root, receipt_id)

    assert loaded is not None
    assert loaded.schema_version == 1
    assert loaded.service_tier is None
    assert loaded.to_json() == legacy_receipt
