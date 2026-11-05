"""Durable, strictly typed token-usage records for model-backed work.

Codex and Claude expose different native token fields.  This module normalizes
them into one durable record while keeping parsing and persistence independent
from summary content, so cache hits can report the original generation cost
without pretending those tokens were spent again.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from wrkviz.archive import (
    JsonValue,
    as_int,
    as_object,
    as_string,
    canonical_json,
    content_hash,
    narrow_json,
    read_json,
    write_json_if_changed,
)


USAGE_SCHEMA_VERSION = 2
_LEGACY_USAGE_SCHEMA_VERSION = 1
DEFAULT_SERVICE_TIER = "default"


def resolve_service_tier(backend: str, service_tier: str | None) -> str | None:
    """Return the effective tier, rejecting tiers for backends that ignore them."""

    if service_tier is not None and not service_tier.strip():
        raise ValueError("service tier must not be empty")
    if backend == "codex":
        return DEFAULT_SERVICE_TIER if service_tier is None else service_tier
    if service_tier is not None:
        raise ValueError("service tier is only supported by the codex backend")
    return None


@dataclass(frozen=True)
class TokenUsage:
    """The stable token breakdown exposed by one model invocation."""

    input_tokens: int = 0
    cached_input_tokens: int = 0
    cache_write_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_output_tokens: int = 0

    def __post_init__(self) -> None:
        values = (
            self.input_tokens,
            self.cached_input_tokens,
            self.cache_write_input_tokens,
            self.output_tokens,
            self.reasoning_output_tokens,
        )
        if any(value < 0 for value in values):
            raise ValueError("token counts must be non-negative")
        if self.cached_input_tokens > self.input_tokens:
            raise ValueError("cached input tokens exceed input tokens")
        if self.reasoning_output_tokens > self.output_tokens:
            raise ValueError("reasoning output tokens exceed output tokens")

    @property
    def total_tokens(self) -> int:
        """Return the archive convention: all input plus all output."""

        return self.input_tokens + self.output_tokens

    def __add__(self, other: TokenUsage) -> TokenUsage:
        return TokenUsage(
            input_tokens=self.input_tokens + other.input_tokens,
            cached_input_tokens=self.cached_input_tokens
            + other.cached_input_tokens,
            cache_write_input_tokens=self.cache_write_input_tokens
            + other.cache_write_input_tokens,
            output_tokens=self.output_tokens + other.output_tokens,
            reasoning_output_tokens=self.reasoning_output_tokens
            + other.reasoning_output_tokens,
        )

    def to_json(self) -> dict[str, JsonValue]:
        """Return the token counters as a JSON-compatible object."""

        return {
            "input_tokens": self.input_tokens,
            "cached_input_tokens": self.cached_input_tokens,
            "cache_write_input_tokens": self.cache_write_input_tokens,
            "output_tokens": self.output_tokens,
            "reasoning_output_tokens": self.reasoning_output_tokens,
            "total_tokens": self.total_tokens,
        }

    @classmethod
    def from_json(cls, value: JsonValue, where: str) -> TokenUsage:
        """Validate and load token counters from a JSON value."""

        obj = as_object(value, where)
        expected = {
            "input_tokens",
            "cached_input_tokens",
            "cache_write_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
            "total_tokens",
        }
        if set(obj) != expected:
            raise ValueError(f"{where}: expected exactly {sorted(expected)!r}")
        usage = cls(
            input_tokens=as_int(obj.get("input_tokens"), f"{where}.input_tokens"),
            cached_input_tokens=as_int(
                obj.get("cached_input_tokens"), f"{where}.cached_input_tokens"
            ),
            cache_write_input_tokens=as_int(
                obj.get("cache_write_input_tokens"),
                f"{where}.cache_write_input_tokens",
            ),
            output_tokens=as_int(obj.get("output_tokens"), f"{where}.output_tokens"),
            reasoning_output_tokens=as_int(
                obj.get("reasoning_output_tokens"),
                f"{where}.reasoning_output_tokens",
            ),
        )
        recorded_total = as_int(obj.get("total_tokens"), f"{where}.total_tokens")
        if recorded_total != usage.total_tokens:
            raise ValueError(f"{where}.total_tokens: inconsistent token total")
        return usage


@dataclass(frozen=True)
class BatchUsageReceipt:
    """Immutable provenance and usage for one backend batch call."""

    receipt_id: str
    backend: str
    model: str
    reasoning_effort: str | None
    service_tier: str | None
    status: str
    started_at: str
    completed_at: str
    input_hashes: tuple[str, ...]
    usage: TokenUsage | None
    error: str | None
    schema_version: int = USAGE_SCHEMA_VERSION

    def __post_init__(self) -> None:
        if self.schema_version not in (
            _LEGACY_USAGE_SCHEMA_VERSION,
            USAGE_SCHEMA_VERSION,
        ):
            raise ValueError(f"unsupported usage schema version {self.schema_version}")
        if self.service_tier is not None and not self.service_tier.strip():
            raise ValueError("service tier must not be empty")
        if (
            self.schema_version == _LEGACY_USAGE_SCHEMA_VERSION
            and self.service_tier is not None
        ):
            raise ValueError("schema v1 batch receipts cannot contain a service tier")

    def to_json(self) -> dict[str, JsonValue]:
        """Return this immutable batch receipt as a JSON-compatible object."""

        usage: JsonValue = self.usage.to_json() if self.usage is not None else None
        result: dict[str, JsonValue] = {
            "schema_version": self.schema_version,
            "receipt_id": self.receipt_id,
            "backend": self.backend,
            "model": self.model,
            "reasoning_effort": self.reasoning_effort,
            "status": self.status,
            "started_at": self.started_at,
            "completed_at": self.completed_at,
            "input_hashes": list(self.input_hashes),
            "usage": usage,
            "error": self.error,
        }
        if self.schema_version == USAGE_SCHEMA_VERSION:
            result["service_tier"] = self.service_tier
        return result

    @classmethod
    def create(
        cls,
        *,
        backend: str,
        model: str,
        reasoning_effort: str | None,
        service_tier: str | None = None,
        status: str,
        started_at: str,
        completed_at: str,
        input_hashes: tuple[str, ...],
        usage: TokenUsage | None,
        error: str | None,
    ) -> BatchUsageReceipt:
        """Create a content-addressed receipt from one backend attempt."""

        identity: JsonValue = {
            "schema_version": USAGE_SCHEMA_VERSION,
            "backend": backend,
            "model": model,
            "reasoning_effort": reasoning_effort,
            "service_tier": service_tier,
            "status": status,
            "started_at": started_at,
            "completed_at": completed_at,
            "input_hashes": list(input_hashes),
            "usage": usage.to_json() if usage is not None else None,
            "error": error,
        }
        receipt_id = content_hash_from_json(identity)
        return cls(
            receipt_id=receipt_id,
            backend=backend,
            model=model,
            reasoning_effort=reasoning_effort,
            service_tier=service_tier,
            status=status,
            started_at=started_at,
            completed_at=completed_at,
            input_hashes=input_hashes,
            usage=usage,
            error=error,
        )

    @classmethod
    def from_json(cls, value: JsonValue, where: str) -> BatchUsageReceipt:
        """Validate and load one immutable batch receipt."""

        obj = as_object(value, where)
        base_expected = {
            "schema_version",
            "receipt_id",
            "backend",
            "model",
            "reasoning_effort",
            "status",
            "started_at",
            "completed_at",
            "input_hashes",
            "usage",
            "error",
        }
        version = as_int(obj.get("schema_version"), f"{where}.schema_version")
        if version == _LEGACY_USAGE_SCHEMA_VERSION:
            expected = base_expected
        elif version == USAGE_SCHEMA_VERSION:
            expected = base_expected | {"service_tier"}
        else:
            raise ValueError(f"{where}: unsupported schema version {version}")
        if set(obj) != expected:
            raise ValueError(f"{where}: expected exactly {sorted(expected)!r}")
        raw_hashes = obj.get("input_hashes")
        if not isinstance(raw_hashes, list):
            raise ValueError(f"{where}.input_hashes: expected an array")
        input_hashes = tuple(
            as_string(item, f"{where}.input_hashes[{index}]")
            for index, item in enumerate(raw_hashes)
        )
        raw_usage = obj.get("usage")
        usage = (
            None
            if raw_usage is None
            else TokenUsage.from_json(raw_usage, f"{where}.usage")
        )
        raw_error = obj.get("error")
        error = None if raw_error is None else as_string(raw_error, f"{where}.error")
        receipt = cls(
            receipt_id=as_string(obj.get("receipt_id"), f"{where}.receipt_id"),
            backend=as_string(obj.get("backend"), f"{where}.backend"),
            model=as_string(obj.get("model"), f"{where}.model"),
            reasoning_effort=(
                None
                if obj.get("reasoning_effort") is None
                else as_string(
                    obj.get("reasoning_effort"), f"{where}.reasoning_effort"
                )
            ),
            service_tier=(
                None
                if obj.get("service_tier") is None
                else as_string(obj.get("service_tier"), f"{where}.service_tier")
            ),
            status=as_string(obj.get("status"), f"{where}.status"),
            started_at=as_string(obj.get("started_at"), f"{where}.started_at"),
            completed_at=as_string(obj.get("completed_at"), f"{where}.completed_at"),
            input_hashes=input_hashes,
            usage=usage,
            error=error,
            schema_version=version,
        )
        identity: JsonValue = dict(receipt.to_json())
        if isinstance(identity, dict):
            identity.pop("receipt_id")
        if receipt.receipt_id != content_hash_from_json(identity):
            raise ValueError(f"{where}.receipt_id: content hash mismatch")
        return receipt


@dataclass(frozen=True)
class UsageRunAccounting:
    """Aggregate usage and receipt location for one cache operation."""

    path: Path
    newly_spent_usage: TokenUsage
    newly_spent_unknown_receipts: int
    artifact_generation_usage: TokenUsage
    artifact_generation_unknown_receipts: int


def content_hash_from_json(value: JsonValue) -> str:
    """Hash a JSON value without relying on mapping insertion order."""

    return content_hash(canonical_json(value))


def parse_codex_jsonl_usage(text: str) -> TokenUsage | None:
    """Return the final completed-turn usage, or ``None`` when unavailable.

    Banner and warning lines are intentionally ignored.  A successful current
    Codex CLI emits a JSON object with ``type=turn.completed``; older/fake
    backends remain supported but are reported explicitly as unknown usage.
    """

    found: TokenUsage | None = None
    for line_number, line in enumerate(text.splitlines(), start=1):
        candidate = line.strip()
        if not candidate.startswith("{"):
            continue
        try:
            raw: object = json.loads(candidate)
            event = as_object(narrow_json(raw, f"codex JSONL line {line_number}"), "event")
        except (ValueError, json.JSONDecodeError):
            continue
        if event.get("type") != "turn.completed":
            continue
        usage_obj = as_object(event.get("usage"), "turn.completed.usage")
        found = TokenUsage(
            input_tokens=as_int(
                usage_obj.get("input_tokens"), "turn.completed.usage.input_tokens"
            ),
            cached_input_tokens=as_int(
                usage_obj.get("cached_input_tokens"),
                "turn.completed.usage.cached_input_tokens",
            ),
            cache_write_input_tokens=(
                as_int(
                    usage_obj.get("cache_write_input_tokens"),
                    "turn.completed.usage.cache_write_input_tokens",
                )
                if "cache_write_input_tokens" in usage_obj
                else 0
            ),
            output_tokens=as_int(
                usage_obj.get("output_tokens"), "turn.completed.usage.output_tokens"
            ),
            reasoning_output_tokens=(
                as_int(
                    usage_obj.get("reasoning_output_tokens"),
                    "turn.completed.usage.reasoning_output_tokens",
                )
                if "reasoning_output_tokens" in usage_obj
                else 0
            ),
        )
    return found


def parse_claude_json_usage(text: str) -> TokenUsage:
    """Parse one Claude ``--output-format json`` result's exact token usage.

    Claude reports cache reads and cache writes as separate native input-token
    counters.  ``TokenUsage.input_tokens`` is the full billable/logical input
    total, so it includes all three native input categories; the two cache
    counters remain available separately as well.
    """

    try:
        raw: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise ValueError(f"claude JSON result is invalid: {error}") from error
    root = as_object(narrow_json(raw, "claude JSON result"), "claude JSON result")
    usage = as_object(root.get("usage"), "claude JSON result.usage")
    direct_input = as_int(
        usage.get("input_tokens"), "claude JSON result.usage.input_tokens"
    )
    cache_write = as_int(
        usage.get("cache_creation_input_tokens"),
        "claude JSON result.usage.cache_creation_input_tokens",
    )
    cache_read = as_int(
        usage.get("cache_read_input_tokens"),
        "claude JSON result.usage.cache_read_input_tokens",
    )
    output = as_int(
        usage.get("output_tokens"), "claude JSON result.usage.output_tokens"
    )
    return TokenUsage(
        input_tokens=direct_input + cache_write + cache_read,
        cached_input_tokens=cache_read,
        cache_write_input_tokens=cache_write,
        output_tokens=output,
    )


def write_batch_receipt(root: Path, receipt: BatchUsageReceipt) -> Path:
    """Persist one batch receipt and return its path."""

    path = root / "receipts" / f"{receipt.receipt_id}.json"
    write_json_if_changed(path, receipt.to_json())
    return path


def load_batch_receipt(root: Path, receipt_id: str) -> BatchUsageReceipt | None:
    """Load one receipt, returning ``None`` when it is absent or invalid."""

    path = root / "receipts" / f"{receipt_id}.json"
    try:
        return BatchUsageReceipt.from_json(read_json(path), str(path))
    except (OSError, ValueError):
        return None


def _sum_receipt_usage(
    receipts: Sequence[BatchUsageReceipt],
) -> tuple[TokenUsage, int]:
    total = TokenUsage()
    unknown = 0
    for receipt in receipts:
        if receipt.usage is None:
            unknown += 1
        else:
            total += receipt.usage
    return total, unknown


def write_usage_run_receipt(
    root: Path,
    *,
    started_at: str,
    completed_at: str,
    backend: str,
    model: str,
    reasoning_effort: str | None,
    service_tier: str | None = None,
    job_count: int,
    hits: int,
    misses: int,
    new_receipts: Sequence[BatchUsageReceipt],
    artifact_receipts: Sequence[BatchUsageReceipt],
    unreadable_artifact_receipt_ids: Sequence[str],
    unknown_legacy_artifacts: int,
) -> UsageRunAccounting:
    """Persist an immutable invocation receipt and return its aggregates."""

    if service_tier is not None and not service_tier.strip():
        raise ValueError("service tier must not be empty")
    newly_spent_usage, newly_unknown = _sum_receipt_usage(new_receipts)
    artifact_usage, artifact_unknown_known = _sum_receipt_usage(artifact_receipts)
    artifact_unknown = artifact_unknown_known + len(unreadable_artifact_receipt_ids)
    batch_ids = tuple(sorted(receipt.receipt_id for receipt in new_receipts))
    artifact_ids = tuple(
        sorted(
            [receipt.receipt_id for receipt in artifact_receipts]
            + list(unreadable_artifact_receipt_ids)
        )
    )
    identity: dict[str, JsonValue] = {
        "started_at": started_at,
        "completed_at": completed_at,
        "backend": backend,
        "model": model,
        "reasoning_effort": reasoning_effort,
        "service_tier": service_tier,
        "job_count": job_count,
        "cache_hits": hits,
        "cache_misses": misses,
        "batch_receipt_ids": list(batch_ids),
        "artifact_receipt_ids": list(artifact_ids),
    }
    run_id = content_hash_from_json(identity)
    obj: dict[str, JsonValue] = {
        "schema_version": USAGE_SCHEMA_VERSION,
        "run_id": run_id,
        "started_at": started_at,
        "completed_at": completed_at,
        "backend": backend,
        "model": model,
        "reasoning_effort": reasoning_effort,
        "service_tier": service_tier,
        "job_count": job_count,
        "cache_hits": hits,
        "cache_misses": misses,
        "backend_batches": len(batch_ids),
        "batch_receipt_ids": list(batch_ids),
        "artifact_receipt_ids": list(artifact_ids),
        "newly_spent_usage": newly_spent_usage.to_json(),
        "newly_spent_unknown_receipts": newly_unknown,
        "artifact_generation_usage": artifact_usage.to_json(),
        "artifact_generation_unknown_receipts": artifact_unknown,
        "unknown_legacy_artifacts": unknown_legacy_artifacts,
    }
    path = root / "runs" / f"{run_id}.json"
    write_json_if_changed(path, obj)
    return UsageRunAccounting(
        path=path,
        newly_spent_usage=newly_spent_usage,
        newly_spent_unknown_receipts=newly_unknown,
        artifact_generation_usage=artifact_usage,
        artifact_generation_unknown_receipts=artifact_unknown,
    )


__all__ = [
    "BatchUsageReceipt",
    "DEFAULT_SERVICE_TIER",
    "TokenUsage",
    "UsageRunAccounting",
    "load_batch_receipt",
    "parse_claude_json_usage",
    "parse_codex_jsonl_usage",
    "resolve_service_tier",
    "write_batch_receipt",
    "write_usage_run_receipt",
]
