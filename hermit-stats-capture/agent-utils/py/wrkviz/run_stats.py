#!/usr/bin/env python3
"""Print human-readable run and model-token statistics for a timeline archive.

This file is deliberately standard-library-only because ``render_archive`` copies it into every
generated archive.  The copied script remains useful on a machine where wrkviz is not
installed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shlex
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path


JsonScalar = str | int | float | bool | None
JsonValue = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]


def _narrow_json(raw: object, where: str) -> JsonValue:
    if raw is None or isinstance(raw, (str, bool, int, float)):
        return raw
    if isinstance(raw, list):
        return [_narrow_json(item, where) for item in raw]
    if isinstance(raw, dict):
        result: dict[str, JsonValue] = {}
        for key, item in raw.items():
            if not isinstance(key, str):
                raise ValueError(f"{where}: object key is not a string")
            result[key] = _narrow_json(item, where)
        return result
    raise ValueError(f"{where}: unsupported JSON value {type(raw).__name__}")


def _read_object(path: Path) -> dict[str, JsonValue]:
    raw: object = json.loads(path.read_text(encoding="utf-8"))
    value = _narrow_json(raw, str(path))
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected a JSON object")
    return value


def _required_string(obj: dict[str, JsonValue], key: str, where: str) -> str:
    value = obj.get(key)
    if not isinstance(value, str):
        raise ValueError(f"{where}.{key}: expected a string")
    return value


def _optional_string(obj: dict[str, JsonValue], key: str, where: str) -> str | None:
    value = obj.get(key)
    if value is None:
        return None
    if not isinstance(value, str):
        raise ValueError(f"{where}.{key}: expected a string or null")
    return value


def _required_int(obj: dict[str, JsonValue], key: str, where: str) -> int:
    value = obj.get(key)
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{where}.{key}: expected an integer")
    return value


def _optional_int(obj: dict[str, JsonValue], key: str, where: str) -> int | None:
    value = obj.get(key)
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{where}.{key}: expected an integer or null")
    return value


def _optional_object(
    obj: dict[str, JsonValue], key: str, where: str
) -> dict[str, JsonValue] | None:
    value = obj.get(key)
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ValueError(f"{where}.{key}: expected an object or null")
    return value


@dataclass(frozen=True)
class Usage:
    """One additive model-token breakdown."""

    input_tokens: int = 0
    cached_input_tokens: int = 0
    cache_write_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_output_tokens: int = 0

    @property
    def total_tokens(self) -> int:
        """Return billable input plus output tokens without double-counting subsets."""

        return self.input_tokens + self.output_tokens

    def __add__(self, other: Usage) -> Usage:
        return Usage(
            input_tokens=self.input_tokens + other.input_tokens,
            cached_input_tokens=self.cached_input_tokens + other.cached_input_tokens,
            cache_write_input_tokens=(
                self.cache_write_input_tokens + other.cache_write_input_tokens
            ),
            output_tokens=self.output_tokens + other.output_tokens,
            reasoning_output_tokens=(
                self.reasoning_output_tokens + other.reasoning_output_tokens
            ),
        )

    @classmethod
    def from_json(cls, value: JsonValue, where: str) -> Usage:
        """Parse and validate one serialized token-usage breakdown."""

        if not isinstance(value, dict):
            raise ValueError(f"{where}: expected an object")
        usage = cls(
            input_tokens=_required_int(value, "input_tokens", where),
            cached_input_tokens=_required_int(value, "cached_input_tokens", where),
            cache_write_input_tokens=_required_int(
                value, "cache_write_input_tokens", where
            ),
            output_tokens=_required_int(value, "output_tokens", where),
            reasoning_output_tokens=_required_int(
                value, "reasoning_output_tokens", where
            ),
        )
        if min(
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_write_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
        ) < 0:
            raise ValueError(f"{where}: token counts must be non-negative")
        if usage.cached_input_tokens > usage.input_tokens:
            raise ValueError(f"{where}: cached input exceeds input")
        if usage.reasoning_output_tokens > usage.output_tokens:
            raise ValueError(f"{where}: reasoning output exceeds output")
        recorded_total = _required_int(value, "total_tokens", where)
        if recorded_total != usage.total_tokens:
            raise ValueError(f"{where}.total_tokens: inconsistent token total")
        return usage


@dataclass(frozen=True)
class RunRecord:
    """The top-level provenance record for one pipeline command."""

    path: Path
    run_id: str
    started_at: str
    completed_at: str
    status: str
    team_slug: str
    command: tuple[str, ...]
    ingest: dict[str, JsonValue] | None
    summaries: dict[str, JsonValue] | None
    build: dict[str, JsonValue] | None
    error: str | None

    @classmethod
    def from_path(cls, path: Path) -> RunRecord:
        """Load one immutable top-level pipeline run record."""

        obj = _read_object(path)
        where = str(path)
        raw_command = obj.get("command")
        if not isinstance(raw_command, list) or not all(
            isinstance(item, str) for item in raw_command
        ):
            raise ValueError(f"{where}.command: expected an array of strings")
        return cls(
            path=path,
            run_id=_required_string(obj, "run_id", where),
            started_at=_required_string(obj, "started_at", where),
            completed_at=_required_string(obj, "completed_at", where),
            status=_required_string(obj, "status", where),
            team_slug=_required_string(obj, "team_slug", where),
            command=tuple(item for item in raw_command if isinstance(item, str)),
            ingest=_optional_object(obj, "ingest", where),
            summaries=_optional_object(obj, "summaries", where),
            build=_optional_object(obj, "build", where),
            error=_optional_string(obj, "error", where),
        )


@dataclass(frozen=True)
class BatchReceipt:
    """One actual backend attempt from the immutable receipt ledger."""

    team_slug: str
    schema_version: int
    receipt_id: str
    backend: str
    model: str
    reasoning_effort: str | None
    service_tier: str | None
    status: str
    started_at: str
    completed_at: str
    usage: Usage | None

    @classmethod
    def from_path(cls, path: Path, team_slug: str) -> BatchReceipt:
        """Load one immutable backend-attempt receipt."""

        obj = _read_object(path)
        where = str(path)
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
        schema_version = _required_int(obj, "schema_version", where)
        if schema_version == 1:
            expected = base_expected
        elif schema_version == 2:
            expected = base_expected | {"service_tier"}
        else:
            raise ValueError(f"{where}: unsupported schema version {schema_version}")
        if set(obj) != expected:
            raise ValueError(f"{where}: expected exactly {sorted(expected)!r}")
        raw_hashes = obj.get("input_hashes")
        if not isinstance(raw_hashes, list) or not all(
            isinstance(item, str) for item in raw_hashes
        ):
            raise ValueError(f"{where}.input_hashes: expected an array of strings")
        _optional_string(obj, "error", where)
        receipt_id = _required_string(obj, "receipt_id", where)
        identity: dict[str, JsonValue] = dict(obj)
        identity.pop("receipt_id")
        canonical = json.dumps(
            identity, ensure_ascii=False, indent=2, sort_keys=True
        ) + "\n"
        expected_receipt_id = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
        if receipt_id != expected_receipt_id:
            raise ValueError(f"{where}.receipt_id: content hash mismatch")
        raw_usage = obj.get("usage")
        service_tier = (
            None
            if schema_version == 1
            else _optional_string(obj, "service_tier", where)
        )
        if service_tier is not None and not service_tier.strip():
            raise ValueError(f"{where}.service_tier: must not be empty")
        return cls(
            team_slug=team_slug,
            schema_version=schema_version,
            receipt_id=receipt_id,
            backend=_required_string(obj, "backend", where),
            model=_required_string(obj, "model", where),
            reasoning_effort=_optional_string(obj, "reasoning_effort", where),
            service_tier=service_tier,
            status=_required_string(obj, "status", where),
            started_at=_required_string(obj, "started_at", where),
            completed_at=_required_string(obj, "completed_at", where),
            usage=(
                None
                if raw_usage is None
                else Usage.from_json(raw_usage, f"{where}.usage")
            ),
        )


@dataclass(frozen=True)
class ReceiptLedger:
    """Aggregated statuses and known usage for a set of backend receipts."""

    attempts: int = 0
    completed: int = 0
    failed: int = 0
    other_status: int = 0
    unknown_usage: int = 0
    usage: Usage = Usage()

    @property
    def known_usage_receipts(self) -> int:
        """Return how many receipts reported a token breakdown."""

        return self.attempts - self.unknown_usage

    def add(self, receipt: BatchReceipt) -> ReceiptLedger:
        """Return this ledger with one backend receipt incorporated."""

        return ReceiptLedger(
            attempts=self.attempts + 1,
            completed=self.completed + int(receipt.status == "completed"),
            failed=self.failed + int(receipt.status == "failed"),
            other_status=self.other_status
            + int(receipt.status not in ("completed", "failed")),
            unknown_usage=self.unknown_usage + int(receipt.usage is None),
            usage=self.usage + (receipt.usage or Usage()),
        )


def _load_runs(archive: Path) -> tuple[list[RunRecord], list[str]]:
    records: list[RunRecord] = []
    warnings: list[str] = []
    for path in sorted((archive / "runs").glob("*.json")):
        try:
            records.append(RunRecord.from_path(path))
        except (OSError, ValueError, json.JSONDecodeError) as error:
            warnings.append(f"could not read run log {path.relative_to(archive)}: {error}")
    records.sort(key=lambda item: (item.completed_at, item.run_id))
    return records, warnings


def _receipt_paths(archive: Path) -> list[Path]:
    teams = archive / "teams"
    if not teams.is_dir():
        return []
    return sorted(
        path
        for path in teams.rglob("*.json")
        if path.parent.name == "receipts" and path.parent.parent.name == "_usage"
    )


def _load_receipts(archive: Path) -> tuple[list[BatchReceipt], list[str]]:
    by_id: dict[tuple[str, str], BatchReceipt] = {}
    warnings: list[str] = []
    for path in _receipt_paths(archive):
        try:
            relative = path.relative_to(archive)
            if len(relative.parts) < 3 or relative.parts[0] != "teams":
                raise ValueError("receipt is not beneath teams/<team-slug>")
            team_slug = relative.parts[1]
            receipt = BatchReceipt.from_path(path, team_slug)
        except (OSError, ValueError, json.JSONDecodeError) as error:
            warnings.append(
                f"could not read backend receipt {path.relative_to(archive)}: {error}"
            )
            continue
        key = (receipt.team_slug, receipt.receipt_id)
        previous = by_id.get(key)
        if previous is not None and previous != receipt:
            warnings.append(
                f"conflicting duplicate backend receipt {receipt.team_slug}/"
                f"{receipt.receipt_id}"
            )
            continue
        by_id[key] = receipt
    return sorted(
        by_id.values(), key=lambda item: (item.team_slug, item.receipt_id)
    ), warnings


_SUMMARIZATION_ACTIONS = frozenset(
    ("summarize", "refresh", "refresh-claude", "refresh-orc")
)


def _run_action(record: RunRecord) -> str:
    return record.command[1] if len(record.command) > 1 else "command"


def _is_summarization_run(record: RunRecord) -> bool:
    return _run_action(record) in _SUMMARIZATION_ACTIONS


def _timestamp(value: str) -> datetime | None:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        return None
    return parsed


def _receipt_ledger(receipts: Sequence[BatchReceipt]) -> ReceiptLedger:
    ledger = ReceiptLedger()
    for receipt in receipts:
        ledger = ledger.add(receipt)
    return ledger


def _associate_receipts(
    runs: Sequence[RunRecord], receipts: Sequence[BatchReceipt]
) -> tuple[dict[str, tuple[BatchReceipt, ...]], tuple[BatchReceipt, ...], list[str]]:
    """Associate receipts with non-overlapping summarize runs by interval containment."""

    windows: list[tuple[RunRecord, datetime, datetime]] = []
    warnings: list[str] = []
    associated: dict[str, list[BatchReceipt]] = {}
    for run in runs:
        if not _is_summarization_run(run):
            continue
        associated[run.run_id] = []
        started = _timestamp(run.started_at)
        completed = _timestamp(run.completed_at)
        if started is None or completed is None or completed < started:
            warnings.append(
                f"cannot attribute receipts to run {run.run_id}: invalid timestamp interval"
            )
            continue
        windows.append((run, started, completed))

    unattributed: list[BatchReceipt] = []
    for receipt in receipts:
        started = _timestamp(receipt.started_at)
        completed = _timestamp(receipt.completed_at)
        if started is None or completed is None or completed < started:
            warnings.append(
                f"cannot attribute receipt {receipt.receipt_id}: invalid timestamp interval"
            )
            unattributed.append(receipt)
            continue
        matches = [
            run
            for run, run_started, run_completed in windows
            if run.team_slug == receipt.team_slug
            and run_started <= started
            and completed <= run_completed
        ]
        if len(matches) == 1:
            associated[matches[0].run_id].append(receipt)
            continue
        if len(matches) > 1:
            warnings.append(
                f"cannot uniquely attribute receipt {receipt.team_slug}/"
                f"{receipt.receipt_id}: "
                f"matches {len(matches)} summarize runs"
            )
        unattributed.append(receipt)

    frozen = {
        run_id: tuple(sorted(items, key=lambda item: item.receipt_id))
        for run_id, items in associated.items()
    }
    return frozen, tuple(unattributed), warnings


def _format_usage(usage: Usage) -> str:
    return (
        f"total={usage.total_tokens:,}; input={usage.input_tokens:,} "
        f"(cached={usage.cached_input_tokens:,}, cache-write={usage.cache_write_input_tokens:,}); "
        f"output={usage.output_tokens:,} (reasoning={usage.reasoning_output_tokens:,})"
    )


def _format_actual_usage(ledger: ReceiptLedger) -> str:
    if ledger.unknown_usage == 0:
        return _format_usage(ledger.usage)
    receipt_word = "receipt" if ledger.unknown_usage == 1 else "receipts"
    verb = "lacks" if ledger.unknown_usage == 1 else "lack"
    prefix = f"UNKNOWN ({ledger.unknown_usage:,} {receipt_word} {verb} usage)"
    if ledger.known_usage_receipts == 0:
        return prefix + "; no known token subtotal was reported"
    known_word = "receipt" if ledger.known_usage_receipts == 1 else "receipts"
    return (
        prefix
        + f"; known subtotal from {ledger.known_usage_receipts:,} {known_word}: "
        + _format_usage(ledger.usage)
    )


def _format_reported_usage(usage: Usage | None, unknown_receipts: int) -> str:
    receipt_word = "receipt" if unknown_receipts == 1 else "receipts"
    verb = "lacks" if unknown_receipts == 1 else "lack"
    prefix = f"UNKNOWN ({unknown_receipts:,} {receipt_word} {verb} usage)"
    if usage is None:
        return (
            prefix + "; no known token subtotal was reported"
            if unknown_receipts
            else "unavailable"
        )
    if unknown_receipts == 0:
        return _format_usage(usage)
    if usage == Usage():
        return prefix + "; no known token subtotal was reported"
    return prefix + "; known subtotal: " + _format_usage(usage)


def _duration(started_at: str, completed_at: str) -> str:
    started = _timestamp(started_at)
    completed = _timestamp(completed_at)
    if started is None or completed is None:
        return "unknown"
    seconds = max(0, round((completed - started).total_seconds()))
    hours, remainder = divmod(seconds, 3_600)
    minutes, final_seconds = divmod(remainder, 60)
    if hours:
        return f"{hours}h {minutes}m {final_seconds}s"
    if minutes:
        return f"{minutes}m {final_seconds}s"
    return f"{final_seconds}s"


def _object_counts(obj: dict[str, JsonValue]) -> str:
    parts: list[str] = []
    for key in sorted(obj):
        value = obj[key]
        if isinstance(value, bool) or not isinstance(value, (int, str)):
            continue
        parts.append(f"{key}={value:,}" if isinstance(value, int) else f"{key}={value}")
    return ", ".join(parts) if parts else "none"


def _summary_usage(
    summary: dict[str, JsonValue], key: str, where: str
) -> Usage | None:
    value = summary.get(key)
    if value is None:
        return None
    return Usage.from_json(value, f"{where}.{key}")


def _summary_int(
    summary: dict[str, JsonValue], key: str, where: str, default: int = 0
) -> int:
    value = _optional_int(summary, key, where)
    return default if value is None else value


def _summary_nonnegative_int(
    summary: dict[str, JsonValue], key: str, where: str, default: int = 0
) -> int:
    value = _summary_int(summary, key, where, default)
    if value < 0:
        raise ValueError(f"{where}.{key}: expected a non-negative integer")
    return value


def _format_run_actual_usage(record: RunRecord, ledger: ReceiptLedger) -> str:
    if ledger.attempts:
        return _format_actual_usage(ledger)
    if record.summaries is None:
        return (
            "UNKNOWN (no loadable backend receipts and no successful summary report)"
        )
    where = str(record.path) + ".summaries"
    _summary_nonnegative_int(record.summaries, "cache_hits", where)
    misses = _summary_nonnegative_int(record.summaries, "cache_misses", where)
    batches = _summary_nonnegative_int(
        record.summaries, "backend_batches", where
    )
    reported_usage = _summary_usage(
        record.summaries, "newly_spent_usage", where
    )
    reported_unknown = _summary_nonnegative_int(
        record.summaries, "newly_spent_unknown_receipts", where
    )
    if (
        misses == 0
        and batches == 0
        and reported_usage == Usage()
        and reported_unknown == 0
    ):
        return _format_usage(Usage())
    return (
        "UNKNOWN (no loadable backend receipts); successful-report cross-check: "
        + _format_reported_usage(reported_usage, reported_unknown)
    )


def _format_run(
    record: RunRecord,
    archive: Path,
    index: int,
    attributed_receipts: Sequence[BatchReceipt] | None,
) -> list[str]:
    action = _run_action(record)
    lines = [
        f"[{index:03d}] {record.completed_at}  {record.status.upper()}  "
        f"{record.team_slug} / {action}",
        f"      duration: {_duration(record.started_at, record.completed_at)}",
        f"      command: {shlex.join(record.command)}",
    ]
    if attributed_receipts is not None:
        ledger = _receipt_ledger(attributed_receipts)
        statuses = (
            f"{ledger.completed:,} completed, {ledger.failed:,} failed, "
            f"{ledger.other_status:,} other"
        )
        lines.extend(
            (
                f"      backend receipts attributed by timestamp: {ledger.attempts:,} "
                f"({statuses}); usage known={ledger.known_usage_receipts:,}; "
                f"usage unknown={ledger.unknown_usage:,}",
                "      actual model-token spend for this run: "
                + _format_run_actual_usage(record, ledger),
            )
        )
    if record.ingest is not None:
        lines.append(f"      ingest: {_object_counts(record.ingest)}")
    if record.summaries is not None:
        where = str(record.path) + ".summaries"
        backend = _required_string(record.summaries, "backend", where)
        model = _required_string(record.summaries, "model", where)
        effort = (
            _optional_string(record.summaries, "reasoning_effort", where)
            or "unspecified"
        )
        service_tier = (
            _optional_string(record.summaries, "service_tier", where)
            or "unspecified"
        )
        hits = _summary_nonnegative_int(record.summaries, "cache_hits", where)
        misses = _summary_nonnegative_int(record.summaries, "cache_misses", where)
        batches = _summary_nonnegative_int(
            record.summaries, "backend_batches", where
        )
        lines.extend(
            (
                f"      summarize: {backend} / {model} / effort={effort} / "
                f"tier={service_tier}; "
                f"jobs={hits + misses:,}, cache={hits:,} hit + {misses:,} miss, "
                f"backend_batches={batches:,}",
                "      products: "
                + ", ".join(
                    f"{key}={_summary_int(record.summaries, key, where):,}"
                    for key in (
                        "phases",
                        "agent_names",
                        "rollups",
                        "plain_language_rollups",
                        "rollup_summary_artifacts",
                        "glossary_terms",
                        "project_overviews",
                        "glossary_definitions",
                        "files_changed",
                    )
                ),
            )
        )
        new_usage = _summary_usage(record.summaries, "newly_spent_usage", where)
        new_unknown = _summary_nonnegative_int(
            record.summaries, "newly_spent_unknown_receipts", where
        )
        lines.append(
            "      successful summary report, tokens newly spent: "
            + _format_reported_usage(new_usage, new_unknown)
        )
        artifact_usage = _summary_usage(
            record.summaries, "artifact_generation_usage", where
        )
        artifact_unknown = _summary_nonnegative_int(
            record.summaries, "artifact_generation_unknown_receipts", where
        )
        legacy_unknown = _summary_nonnegative_int(
            record.summaries, "unknown_legacy_artifacts", where
        )
        artifact_text = _format_reported_usage(artifact_usage, artifact_unknown)
        if legacy_unknown:
            legacy_word = "artifact" if legacy_unknown == 1 else "artifacts"
            legacy_verb = "lacks" if legacy_unknown == 1 else "lack"
            known_text = (
                "no known receipt subtotal was reported"
                if artifact_usage is None or artifact_usage == Usage()
                else "known receipt subtotal: " + _format_usage(artifact_usage)
            )
            artifact_text = (
                f"UNKNOWN ({legacy_unknown:,} legacy {legacy_word} "
                f"{legacy_verb} provenance); {known_text}"
                if artifact_unknown == 0
                else artifact_text
                + f"; {legacy_unknown:,} legacy {legacy_word} also "
                + f"{legacy_verb} provenance"
            )
        lines.append(
            "      returned-artifact generation cost (not new spend): "
            + artifact_text
        )
        raw_paths = record.summaries.get("usage_run_paths")
        if isinstance(raw_paths, list):
            lines.append(f"      internal accounting records: {len(raw_paths):,}")
    if record.build is not None:
        lines.append(f"      build: {_object_counts(record.build)}")
    if record.error is not None:
        lines.append(f"      error: {record.error}")
    lines.append(f"      run log: {record.path.relative_to(archive)}")
    return lines


def render_run_stats(archive: Path) -> str:
    """Return a complete human-readable accounting report for *archive*."""

    root = archive.resolve()
    if not root.is_dir():
        raise ValueError(f"archive directory does not exist: {root}")
    runs, run_warnings = _load_runs(root)
    receipts, receipt_warnings = _load_receipts(root)
    receipts_by_run, unattributed_receipts, attribution_warnings = (
        _associate_receipts(runs, receipts)
    )

    model_ledgers: dict[
        tuple[str, str, str | None, str | None], ReceiptLedger
    ] = {}
    for receipt in receipts:
        key = (
            receipt.backend,
            receipt.model,
            receipt.reasoning_effort,
            receipt.service_tier,
        )
        model_ledgers[key] = model_ledgers.get(key, ReceiptLedger()).add(receipt)
    receipt_ledger = _receipt_ledger(receipts)
    unattributed_ledger = _receipt_ledger(unattributed_receipts)

    logged_usage = Usage()
    logged_usage_available = False
    logged_unknown = 0
    successful_summary_reports = 0
    for run in runs:
        if not _is_summarization_run(run) or run.summaries is None:
            continue
        successful_summary_reports += 1
        where = str(run.path) + ".summaries"
        usage = _summary_usage(run.summaries, "newly_spent_usage", where)
        if usage is not None:
            logged_usage += usage
            logged_usage_available = True
        logged_unknown += _summary_nonnegative_int(
            run.summaries, "newly_spent_unknown_receipts", where
        )

    completed = sum(run.status == "completed" for run in runs)
    failed = sum(run.status == "failed" for run in runs)
    other = len(runs) - completed - failed
    summarization_runs = sum(_is_summarization_run(run) for run in runs)
    attributed_count = len(receipts) - len(unattributed_receipts)
    attributed_receipts = tuple(
        receipt
        for items in receipts_by_run.values()
        for receipt in items
    )
    attributed_ledger = _receipt_ledger(attributed_receipts)
    lines = [
        "wrkviz run statistics",
        f"Archive: {root}",
        f"Top-level runs: {len(runs):,} "
        f"({completed:,} completed, {failed:,} failed, {other:,} other; "
        f"{summarization_runs:,} summarize/refresh attempts, "
        f"{successful_summary_reports:,} with successful summary reports)",
        "",
        "Backend receipt ledger (archive-wide actual attempts)",
        f"  Receipts: {len(receipts):,}; usage known={receipt_ledger.known_usage_receipts:,}; "
        f"usage unknown={receipt_ledger.unknown_usage:,}",
        "  Actual tokens spent: "
        + (
            "UNKNOWN (no loadable backend receipts)"
            + (
                "; successful-report cross-check: "
                + _format_reported_usage(
                    logged_usage if logged_usage_available else None,
                    logged_unknown,
                )
                if successful_summary_reports
                else "; no successful summary report is available"
            )
            if receipt_ledger.attempts == 0 and summarization_runs
            else _format_actual_usage(receipt_ledger)
        ),
    ]
    if model_ledgers:
        lines.append("  By backend / model / reasoning effort / service tier:")
        for key in sorted(
            model_ledgers,
            key=lambda item: (
                item[0],
                item[1],
                item[2] is not None,
                item[2] or "",
                item[3] is not None,
                item[3] or "",
            ),
        ):
            ledger = model_ledgers[key]
            effort = key[2] if key[2] is not None else "<unspecified>"
            service_tier = key[3] if key[3] is not None else "<unspecified>"
            statuses = (
                f"{ledger.completed:,} completed, {ledger.failed:,} failed, "
                f"{ledger.other_status:,} other"
            )
            lines.append(
                f"    {key[0]} / {key[1]} / effort={effort} / tier={service_tier}: "
                f"attempts={ledger.attempts:,} "
                f"({statuses}); usage known={ledger.known_usage_receipts:,}; "
                f"usage unknown={ledger.unknown_usage:,}; actual tokens: "
                f"{_format_actual_usage(ledger)}"
            )
    else:
        lines.append("  No backend receipts found.")

    lines.extend(
        (
            "",
            "Top-level summarize-run receipt attribution",
            f"  Attributed receipts: {attributed_count:,} of {len(receipts):,}; "
            f"actual tokens: {_format_actual_usage(attributed_ledger)}",
            f"  Unattributed receipts: {len(unattributed_receipts):,}; actual tokens: "
            f"{_format_actual_usage(unattributed_ledger)}",
            "",
            "Successful summary-report fields (cross-check, not archive totals)",
            "  Reported newly spent: "
            + (
                _format_reported_usage(
                    logged_usage if logged_usage_available else None,
                    logged_unknown,
                )
                if successful_summary_reports
                else "unavailable (no successful summary reports)"
            ),
        )
    )
    delta = receipt_ledger.usage.total_tokens - logged_usage.total_tokens
    if delta != 0 or receipt_ledger.unknown_usage != logged_unknown:
        lines.append(
            f"  Ledger difference: {delta:+,} known tokens and "
            f"{receipt_ledger.unknown_usage - logged_unknown:+,} unknown-usage receipts. "
            "This includes successful batches retained from failed top-level runs, failed "
            "backend calls, unattributed receipts, or missing top-level logs."
        )

    lines.extend(("", "Runs (oldest to newest)"))
    if not runs:
        lines.append("  No top-level run logs found.")
    for index, run in enumerate(runs, start=1):
        if index > 1:
            lines.append("")
        attributed = receipts_by_run.get(run.run_id)
        lines.extend(_format_run(run, root, index, attributed))

    warnings = run_warnings + receipt_warnings + attribution_warnings
    if warnings:
        lines.extend(("", f"Warnings ({len(warnings):,})"))
        lines.extend(f"  - {warning}" for warning in warnings)
    lines.extend(
        (
            "",
            "Notes: cached input is a subset of input; reasoning output is a subset of output.",
            "Backend receipts are attributed conservatively by team and timestamp. Summary "
            "backend sections are lock-serialized, but top-level command windows can overlap "
            "while waiting; ambiguous receipts remain unattributed.",
            "If any receipt lacks usage, the actual total is UNKNOWN; a known subtotal is "
            "not a complete cost.",
            "Returned-artifact generation cost is provenance, not additional spend, and is not "
            "added to totals.",
        )
    )
    return "\n".join(lines) + "\n"


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Print per-run and model-token statistics from a timeline archive."
    )
    parser.add_argument(
        "archive",
        nargs="?",
        type=Path,
        default=Path(__file__).resolve().parent,
        help="archive directory (default: directory containing this script)",
    )
    return parser


def _main(argv: Sequence[str] | None = None) -> int:
    args = list(argv) if argv is not None else sys.argv[1:]
    namespace = _parser().parse_args(args)
    archive = namespace.archive
    if not isinstance(archive, Path):
        raise TypeError("archive argument was not parsed as a path")
    try:
        print(render_run_stats(archive), end="")
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"run_stats.py: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
