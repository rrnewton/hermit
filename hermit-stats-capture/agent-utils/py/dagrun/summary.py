"""Bounded, mergeable summaries of execution profiles.

Each step-width-workload bucket retains a deterministic sample reservoir. Summary merging is
commutative and associative, so distributed runs can accumulate useful planning history
without an ever-growing raw profile store.
"""

from __future__ import annotations

import json
import math
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import NoReturn

from dagrun.estimates import (
    BucketKey,
    Sample,
    StepSamples,
    StepSpeedup,
    _affinity_width,
    _fmt_secs,
    _json_str,
    buckets_for_workloads,
    bucketize_rows,
    step_samples_from_buckets,
    step_speedups_from_buckets,
)

__all__ = [
    "SUMMARY_VERSION",
    "DEFAULT_RESERVOIR_K",
    "DEFAULT_MAX_BUCKETS",
    "SummaryError",
    "Summary",
    "SummaryBucketKey",
    "empty",
    "summary_from_rows",
    "merge",
    "merge_all",
    "to_json",
    "from_json",
    "step_samples_from_summary",
    "step_speedups_from_summary",
    "summary_stats",
]

#: On-disk schema version. Version 2 partitions reservoirs by workload digest so an old command's
#: samples cannot evict the current command's curve. Version 1 remains readable and is normalized
#: to version 2 in memory; unknown versions fail closed.
SUMMARY_VERSION = 2

#: Default reservoir size K: the maximum samples kept per ``(step, inner_jobs, workload)`` bucket.
#: A merge unions two buckets then subsamples back to K, so a bucket never exceeds K regardless of
#: how many runs contributed. 64 keeps the robust median / p90 / speedup curve statistically stable
#: while bounding the summary at a few KB per bucket.
DEFAULT_RESERVOIR_K = 64

#: Hard cap on the number of buckets, so a pathological caller inventing unbounded step names cannot
#: make the summary grow without bound. Real workloads have far fewer buckets (steps × widths ×
#: retained command cohorts); this only ever bites synthetic abuse. Buckets over the cap are dropped
#: by the same content-derived
#: stable order used for sample subsampling, so the surviving set is deterministic across builds.
DEFAULT_MAX_BUCKETS = 4096

#: FNV-1a (64-bit) constants — a tiny, dependency-free hash implemented identically in both builds,
#: so the content-derived subsample order is byte-identical py<->rs.
_FNV_OFFSET_BASIS = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_U64_MASK = (1 << 64) - 1


class SummaryError(Exception):
    """A summary document was malformed, carried an unknown version, or had a mismatched identity."""


#: A summary reservoir is workload-specific. Raw estimator buckets remain ``(step, inner_jobs)``;
#: selection converts back to that shape only after choosing one compatible cohort per step.
SummaryBucketKey = tuple[str, int, str]


@dataclass(frozen=True)
class Summary:
    """A bounded, mergeable profile summary for ONE ``(machine_id, container_class)`` identity.

    Mirrors the per-machine + per-container scoping of the CSV store (one ``step_profiles_<machine>_
    <container>.csv`` file per identity): a summary carries the identity so merges are only ever
    applied within a homogeneous identity, and the speedup core budget is recovered from the
    ``container_class`` exactly as :func:`~dagrun.estimates.load_step_speedups` does.

    ``buckets`` maps each ``(step, inner_jobs, workload_digest)`` bucket to its reservoir (a tuple
    of up to K :class:`~dagrun.estimates.Sample`). The tuple order is not significant — every
    consumer that cares (serialization, merge) re-derives the canonical content order — but a
    freshly built / merged summary already stores each reservoir in that canonical order.
    """

    version: int
    machine_id: str
    container_class: str
    buckets: Mapping[SummaryBucketKey, tuple[Sample, ...]]


# --------------------------------------------------------------------------- content hashing


def _fnv1a_64(data: bytes) -> int:
    """FNV-1a 64-bit hash of ``data`` (wrapping 64-bit arithmetic), identical to the Rust build."""
    h = _FNV_OFFSET_BASIS
    for byte in data:
        h = ((h ^ byte) * _FNV_PRIME) & _U64_MASK
    return h


def _opt_secs_json(value: float | None) -> str:
    """A JSON value for an optional seconds-like float: ``null`` or a quoted fixed-3-decimal string,
    matching the plan JSON's float discipline (parity never depends on float ``repr``)."""
    return "null" if value is None else f'"{_fmt_secs(value)}"'


def _sample_canonical(sample: Sample) -> str:
    """The canonical one-line JSON object for a sample — used BOTH as the serialized form and as the
    input to the subsample hash, so the two can never drift. Every float is a fixed 3-decimal
    string; ``contention`` is always present; the rest are ``null`` when absent."""
    return (
        "{"
        f'"observation_id": {_json_str(sample.observation_id)}, '
        f'"workload_digest": {_json_str(sample.workload_digest)}, '
        f'"elapsed_s": {_opt_secs_json(sample.elapsed_s)}, '
        f'"contention": "{_fmt_secs(sample.contention)}", '
        f'"cpu_s": {_opt_secs_json(sample.cpu_s)}, '
        f'"effective_cores": {_opt_secs_json(sample.effective_cores)}, '
        f'"throttled_s": {_opt_secs_json(sample.throttled_s)}, '
        f'"peak_bytes": {"null" if sample.peak_bytes is None else str(sample.peak_bytes)}, '
        f'"uncensored_peak_bytes": '
        f'{"null" if sample.uncensored_peak_bytes is None else str(sample.uncensored_peak_bytes)}, '
        f'"peak_floor_bytes": '
        f'{"null" if sample.peak_floor_bytes is None else str(sample.peak_floor_bytes)}'
        "}"
    )


def _sample_sort_key(sample: Sample) -> tuple[int, str]:
    """The stable observation-derived rank ``(fnv_hash, canonical_json)`` for a reservoir.

    New rows carry a unique observation id, so repeated equal-valued observations each retain their
    proper chance of entering the reservoir. Legacy samples without an id fall back to canonical
    content. The canonical JSON tie-break has identical ordering in both implementations."""
    canon = _sample_canonical(sample)
    identity = sample.observation_id or canon
    return (_fnv1a_64(identity.encode("utf-8")), canon)


def _ordered(samples: Sequence[Sample], cap: int | None) -> tuple[Sample, ...]:
    """Return deterministic bottom-k order, optionally truncated to ``cap`` observations.

    The rank is a fixed total order on observation identities, so taking the first ``cap`` of a
    union is commutative and associative — the mergeable-summary property."""
    # The same run may arrive through local history and a downloaded summary. Deduplicate a known
    # observation id before bottom-k selection, choosing the canonical-smaller payload if corrupt
    # inputs disagree so merge remains commutative. Empty legacy ids remain distinct.
    known: dict[str, Sample] = {}
    legacy: list[Sample] = []
    for sample in samples:
        if not sample.observation_id:
            legacy.append(sample)
            continue
        previous = known.get(sample.observation_id)
        if previous is None or _sample_canonical(sample) < _sample_canonical(previous):
            known[sample.observation_id] = sample
    ordered = sorted([*known.values(), *legacy], key=_sample_sort_key)
    if cap is not None and len(ordered) > cap:
        ordered = ordered[:cap]
    return tuple(ordered)


def _bucket_sort_key(key: SummaryBucketKey) -> tuple[int, str]:
    """The content-derived stable order buckets are dropped by when over :data:`DEFAULT_MAX_BUCKETS`
    (same FNV-1a scheme as samples, over a canonical ``step:inner:workload`` rendering)."""
    canon = f"{_json_str(key[0])}:{key[1]}:{_json_str(key[2])}"
    return (_fnv1a_64(canon.encode("utf-8")), canon)


def _cap_buckets(
    buckets: Mapping[SummaryBucketKey, tuple[Sample, ...]], max_buckets: int
) -> dict[SummaryBucketKey, tuple[Sample, ...]]:
    """Drop buckets beyond ``max_buckets`` by the content-derived stable order, so the surviving set
    is deterministic across builds. A no-op for a normal workload (far fewer buckets than the cap)."""
    if len(buckets) <= max_buckets:
        return dict(buckets)
    kept = sorted(buckets, key=_bucket_sort_key)[:max_buckets]
    return {key: buckets[key] for key in kept}


# --------------------------------------------------------------------------- construction / merge


def empty(machine_id: str, container_class: str) -> Summary:
    """An empty summary for ``(machine_id, container_class)`` — what a backend returns when no
    summary has been published yet (the planner then falls back to DAG hints)."""
    return Summary(
        version=SUMMARY_VERSION,
        machine_id=machine_id,
        container_class=container_class,
        buckets={},
    )


def summary_from_rows(
    rows: Sequence[Mapping[str, str]],
    machine_id: str,
    container_class: str,
    affinity_width: int | None,
    *,
    reservoir_cap: int = DEFAULT_RESERVOIR_K,
    max_buckets: int = DEFAULT_MAX_BUCKETS,
) -> Summary:
    """Build a bounded summary from raw profile ``rows`` (a CSV store's rows, or this run's per-step
    profile rows stringified). Each ``(step, inner_jobs, workload_digest)`` reservoir is subsampled to
    ``reservoir_cap``; the whole summary is capped at ``max_buckets`` buckets."""
    identified: list[Mapping[str, str]] = []
    for index, row in enumerate(rows):
        if row.get("observation_id") or row.get("run_id"):
            identified.append(row)
            continue
        material = "\0".join(f"{key}={row[key]}" for key in sorted(row))
        generated = f"legacy-{_fnv1a_64(material.encode('utf-8')):016x}-{index:016x}"
        identified.append({**row, "observation_id": generated})
    raw = bucketize_rows(identified, affinity_width)
    partitioned: dict[SummaryBucketKey, list[Sample]] = {}
    for (step, inner), samples in raw.items():
        for sample in samples:
            partitioned.setdefault((step, inner, sample.workload_digest), []).append(sample)
    capped = {key: _ordered(samples, reservoir_cap) for key, samples in partitioned.items()}
    return Summary(
        version=SUMMARY_VERSION,
        machine_id=machine_id,
        container_class=container_class,
        buckets=_cap_buckets(capped, max_buckets),
    )


def merge(
    a: Summary,
    b: Summary,
    *,
    reservoir_cap: int = DEFAULT_RESERVOIR_K,
    max_buckets: int = DEFAULT_MAX_BUCKETS,
) -> Summary:
    """Merge two summaries of the SAME identity: union each bucket's reservoirs and subsample back to
    ``reservoir_cap`` by the content-derived stable order.

    Deterministic, COMMUTATIVE, and ASSOCIATIVE: because the subsample keeps the smallest-K samples
    by a fixed total order on content, ``merge(merge(a, b), c) == merge(a, merge(b, c)) ==
    merge`` of any permutation. A summary merged with an empty summary of the same identity is
    unchanged (up to reservoir order). Raises :class:`SummaryError` on an identity mismatch — the
    caller must never merge a different machine's / container's history into this one."""
    if a.machine_id != b.machine_id or a.container_class != b.container_class:
        raise SummaryError(
            f"cannot merge summaries of different identities: "
            f"{a.machine_id}/{a.container_class} vs {b.machine_id}/{b.container_class}"
        )
    merged: dict[SummaryBucketKey, tuple[Sample, ...]] = {}
    for key in set(a.buckets) | set(b.buckets):
        combined = list(a.buckets.get(key, ())) + list(b.buckets.get(key, ()))
        merged[key] = _ordered(combined, reservoir_cap)
    return Summary(
        version=SUMMARY_VERSION,
        machine_id=a.machine_id,
        container_class=a.container_class,
        buckets=_cap_buckets(merged, max_buckets),
    )


def merge_all(
    summaries: Sequence[Summary],
    machine_id: str,
    container_class: str,
    *,
    reservoir_cap: int = DEFAULT_RESERVOIR_K,
    max_buckets: int = DEFAULT_MAX_BUCKETS,
) -> Summary:
    """Fold-merge a sequence of same-identity summaries, starting from an empty summary of
    ``(machine_id, container_class)``. Order-independent (merge is associative + commutative)."""
    acc = empty(machine_id, container_class)
    for summary in summaries:
        acc = merge(acc, summary, reservoir_cap=reservoir_cap, max_buckets=max_buckets)
    return acc


# --------------------------------------------------------------------------- serialization


def to_json(summary: Summary) -> str:
    """Canonical, byte-identical (py<->rs) JSON for a summary (2-space indent).

    Buckets are sorted by ``(step, inner_jobs, workload_digest)``; each reservoir's samples are emitted in the
    canonical content order (:func:`_ordered`). Floats are fixed 3-decimal strings, so the output
    depends only on the shared fixed-precision formatting — never on float ``repr``."""
    lines: list[str] = [
        "{",
        f'  "version": {summary.version},',
        f'  "machine_id": {_json_str(summary.machine_id)},',
        f'  "container_class": {_json_str(summary.container_class)},',
    ]
    keys = sorted(summary.buckets)
    if not keys:
        lines.append('  "buckets": []')
        lines.append("}")
        return "\n".join(lines)
    lines.append('  "buckets": [')
    bucket_blocks: list[str] = []
    for step, inner, workload in keys:
        samples = _ordered(summary.buckets[(step, inner, workload)], None)
        block = [
            "    {",
            f'      "step": {_json_str(step)},',
            f'      "inner_jobs": {inner},',
            f'      "workload_digest": {_json_str(workload)},',
        ]
        if not samples:
            block.append('      "samples": []')
        else:
            block.append('      "samples": [')
            sample_lines = [f"        {_sample_canonical(s)}" for s in samples]
            block.append(",\n".join(sample_lines))
            block.append("      ]")
        block.append("    }")
        bucket_blocks.append("\n".join(block))
    lines.append(",\n".join(bucket_blocks))
    lines.append("  ]")
    lines.append("}")
    return "\n".join(lines)


def _reject_json_constant(token: str) -> NoReturn:
    """Reject the JSON non-finite literals (``NaN`` / ``Infinity`` / ``-Infinity``) Python's json
    accepts by default — the Rust build's serde_json rejects them, so both must (parity)."""
    raise SummaryError(f"invalid summary: non-finite JSON constant {token!r}")


def _as_obj(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise SummaryError(f"invalid summary: {where} must be an object")
    return {str(k): v for k, v in value.items()}


def _req_str(m: Mapping[str, object], key: str, where: str) -> str:
    value = m.get(key)
    if not isinstance(value, str):
        raise SummaryError(f"invalid summary: {where}.{key} must be a string")
    return value


def _opt_secs(m: Mapping[str, object], key: str, where: str) -> float | None:
    """Parse an optional fixed-decimal seconds STRING (or ``null``) back to a float. The serialized
    form is always a quoted string or ``null``; anything else is a malformed document."""
    value = m.get(key, None)
    if value is None:
        return None
    if not isinstance(value, str):
        raise SummaryError(f"invalid summary: {where}.{key} must be a decimal string or null")
    try:
        parsed = float(value)
    except ValueError as exc:
        raise SummaryError(f"invalid summary: {where}.{key} is not a number ({value!r})") from exc
    # Reject a non-finite string ("nan"/"inf") too — our serializer never emits one, and both builds
    # must reject the same set so a hand-crafted document parses (or fails) identically.
    if not math.isfinite(parsed):
        raise SummaryError(f"invalid summary: {where}.{key} is not finite ({value!r})")
    return parsed


def _opt_int(m: Mapping[str, object], key: str, where: str) -> int | None:
    value = m.get(key, None)
    if value is None:
        return None
    # A JSON integer parses to ``int``; reject bool (a subclass of int) and floats.
    if isinstance(value, bool) or not isinstance(value, int):
        raise SummaryError(f"invalid summary: {where}.{key} must be an integer or null")
    return value


def _req_int(m: Mapping[str, object], key: str, where: str) -> int:
    value = m.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        raise SummaryError(f"invalid summary: {where}.{key} must be an integer")
    return value


def _sample_from_obj(obj: object, where: str, default_workload_digest: str = "") -> Sample:
    m = _as_obj(obj, where)
    observation_id = m.get("observation_id", "")
    if not isinstance(observation_id, str):
        raise SummaryError(f"invalid summary: {where}.observation_id must be a string")
    workload_digest = m.get("workload_digest", default_workload_digest)
    if not isinstance(workload_digest, str):
        raise SummaryError(f"invalid summary: {where}.workload_digest must be a string")
    peak_bytes = _opt_int(m, "peak_bytes", where)
    # Version-1 summaries written before censoring provenance existed treated every peak as exact.
    # Preserve that behavior only when the new field is absent; an explicit null means modern
    # provenance classified the raw peak as censored or unknown.
    uncensored_peak = (
        _opt_int(m, "uncensored_peak_bytes", where)
        if "uncensored_peak_bytes" in m
        else peak_bytes
    )
    return Sample(
        elapsed_s=_opt_secs(m, "elapsed_s", where),
        contention=_opt_secs(m, "contention", where) or 0.0,
        cpu_s=_opt_secs(m, "cpu_s", where),
        effective_cores=_opt_secs(m, "effective_cores", where),
        throttled_s=_opt_secs(m, "throttled_s", where),
        peak_bytes=peak_bytes,
        uncensored_peak_bytes=uncensored_peak,
        peak_floor_bytes=_opt_int(m, "peak_floor_bytes", where),
        observation_id=observation_id,
        workload_digest=workload_digest,
    )


def from_json(text: str) -> Summary:
    """Parse a summary strictly, rejecting unknown versions and malformed shapes."""
    try:
        raw: object = json.loads(text, parse_constant=_reject_json_constant)
    except json.JSONDecodeError as exc:
        raise SummaryError(f"invalid summary JSON: {exc}") from exc
    doc = _as_obj(raw, "summary")
    version = _req_int(doc, "version", "summary")
    if version not in (1, SUMMARY_VERSION):
        raise SummaryError(
            f"unsupported summary version {version} (this build understands 1 and {SUMMARY_VERSION})"
        )
    machine_id = _req_str(doc, "machine_id", "summary")
    container_class = _req_str(doc, "container_class", "summary")
    buckets_raw = doc.get("buckets", [])
    if not isinstance(buckets_raw, list):
        raise SummaryError("invalid summary: buckets must be a list")
    bucket_lists: dict[SummaryBucketKey, list[Sample]] = {}
    for i, bucket_obj in enumerate(buckets_raw):
        where = f"buckets[{i}]"
        bucket = _as_obj(bucket_obj, where)
        step = _req_str(bucket, "step", where)
        inner = _req_int(bucket, "inner_jobs", where)
        workload = _req_str(bucket, "workload_digest", where) if version >= 2 else ""
        samples_raw = bucket.get("samples", [])
        if not isinstance(samples_raw, list):
            raise SummaryError(f"invalid summary: {where}.samples must be a list")
        samples = [
            _sample_from_obj(s, f"{where}.samples[{j}]", workload)
            for j, s in enumerate(samples_raw)
        ]
        if version == 1:
            # Schema 1 had no workload identity. Treat even an unknown extension field as blank so
            # migration is deterministic and matches the documented compatibility cohort.
            samples = [
                Sample(
                    elapsed_s=sample.elapsed_s,
                    contention=sample.contention,
                    cpu_s=sample.cpu_s,
                    effective_cores=sample.effective_cores,
                    throttled_s=sample.throttled_s,
                    peak_bytes=sample.peak_bytes,
                    uncensored_peak_bytes=sample.uncensored_peak_bytes,
                    peak_floor_bytes=sample.peak_floor_bytes,
                    observation_id=sample.observation_id,
                    workload_digest="",
                )
                for sample in samples
            ]
        if version >= 2 and any(sample.workload_digest != workload for sample in samples):
            raise SummaryError(
                f"invalid summary: {where} sample workload_digest does not match its bucket"
            )
        if samples:
            for sample in samples:
                bucket_lists.setdefault((step, inner, sample.workload_digest), []).append(sample)
        else:
            bucket_lists.setdefault((step, inner, workload), [])
    return Summary(
        version=SUMMARY_VERSION,
        machine_id=machine_id,
        container_class=container_class,
        buckets={key: _ordered(samples, None) for key, samples in bucket_lists.items()},
    )


# --------------------------------------------------------------------------- estimate recompute


def _estimator_buckets(
    summary: Summary, workload_digests: Mapping[str, str] | None
) -> dict[BucketKey, list[Sample]]:
    """Select one workload cohort per step, then collapse summary keys for the estimators."""

    combined: dict[BucketKey, list[Sample]] = {}
    for (step, inner, _workload), samples in summary.buckets.items():
        combined.setdefault((step, inner), []).extend(samples)
    return buckets_for_workloads(combined, workload_digests)


def step_samples_from_summary(
    summary: Summary, workload_digests: Mapping[str, str] | None = None
) -> dict[str, StepSamples]:
    """Recompute the per-step duration + memory estimates from a summary's reservoirs, via the SAME
    estimator core the CSV reader uses (:func:`~dagrun.estimates.step_samples_from_buckets`)."""
    return step_samples_from_buckets(_estimator_buckets(summary, workload_digests))


def step_speedups_from_summary(
    summary: Summary,
    core_budget: int | None = None,
    workload_digests: Mapping[str, str] | None = None,
) -> dict[str, StepSpeedup]:
    """Recompute the per-step parallel-speedup curves from a summary's reservoirs. ``core_budget``
    defaults to the affinity width parsed from the summary's ``container_class`` — exactly what
    :func:`~dagrun.estimates.load_step_speedups` uses — so a summary reproduces the CSV
    reader's recommendations."""
    budget = core_budget if core_budget is not None else _affinity_width(summary.container_class)
    return step_speedups_from_buckets(
        _estimator_buckets(summary, workload_digests), budget
    )


def summary_stats(summary: Summary) -> tuple[int, int, int]:
    """``(bucket_count, total_samples, max_bucket_samples)`` — the bounded-size witness. ``merge``
    keeps ``max_bucket_samples <= reservoir_cap`` and ``bucket_count <= max_buckets`` no matter how
    many runs contributed."""
    bucket_count = len(summary.buckets)
    total = sum(len(s) for s in summary.buckets.values())
    largest = max((len(s) for s in summary.buckets.values()), default=0)
    return bucket_count, total, largest
