"""Derive resource estimates and execution plans from recorded step profiles.

The estimators use robust duration and memory statistics, tolerate sparse profile stores,
and fall back to DAG-authored hints when no usable samples exist. Planning and rendering
are deterministic for a fixed store and DAG.
"""

from __future__ import annotations

import csv
import math
import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from enum import Enum
from pathlib import Path

from dagrun import perflog
from dagrun.model import (
    DagConfig,
    ResourceHint,
    Step,
    effective_cpu_count,
    step_width_is_resizable,
    validate_jobs_env_config,
    validate_cmdtype_config,
)
from dagrun.sizing import (
    _memory_footprint_fits,
    _outer_mem_footprint_bytes,
    schedulable_peak_mem_bytes,
)

__all__ = [
    "MACHINE_ID_ENV",
    "CONTAINER_CLASS_ENV",
    "DEFAULT_MIN_SAMPLES",
    "Planner",
    "StepSamples",
    "Sample",
    "BucketKey",
    "SpeedupLevel",
    "StepSpeedup",
    "InfeasibleAllocationError",
    "PlanEntry",
    "Allocation",
    "Plan",
    "feedback_identity",
    "sample_from_row",
    "bucketize_rows",
    "buckets_for_workloads",
    "step_samples_from_buckets",
    "step_speedups_from_buckets",
    "load_step_samples",
    "load_step_speedups",
    "allocate_widths",
    "build_plan",
    "apply_plan_to_config",
    "plan_to_json",
    "plan_to_text",
    "scaling_model_path",
    "scaling_model_to_json",
    "write_scaling_model",
    "row_is_measurement",
]

#: Environment overrides for the feedback identity. Normally the reader derives the machine id
#: and container class from the current host (so a run learns from THIS machine's own history);
#: these let a test (or a caller pinning heterogeneous-but-equivalent runners to one identity)
#: force the ``step_profiles_<machine>_<container>.csv`` the reader loads. Used by the cross
#: differential to read a fixed synthetic store host-independently.
MACHINE_ID_ENV = "DAGRUN_MACHINE_ID"
CONTAINER_CLASS_ENV = "DAGRUN_CONTAINER_CLASS"

#: Minimum recorded samples before the store may refine a step. Below this the DAG-authored hint
#: wins (the store has too little signal to trust yet). Duration feedback may replace its hint;
#: ordinary RSS feedback may raise an authored baseline but never lower it. Deliberate memory-cap
#: lowering is the separate censor-aware ``--profile-memory-feedback`` contract.
DEFAULT_MIN_SAMPLES = 1

#: Robust-stat tunables (named so both builds share one source of truth).
#: MAD-trim: drop duration samples more than this many MADs from the median before re-medianing.
_MAD_TRIM_K = 3.5
#: RSS high-water percentile expressed as an exact integer fraction (num/den) so the nearest-rank
#: index is computed with integer arithmetic — no floating ``ceil`` whose rounding could diverge
#: across languages. 9/10 == the 90th percentile.
_RSS_PCTL_NUM = 9
_RSS_PCTL_DEN = 10
#: A contention fraction is clamped to this maximum so a pathological signal (e.g. a bogus
#: ``pct_other`` of 100) cannot discount a duration all the way to zero.
_MAX_CONTENTION = 0.95

#: The inclusive range of a signed 64-bit integer. Python's ``int`` is arbitrary-precision, so an
#: out-of-range value it would happily keep must be REJECTED here to match Rust's
#: ``str::parse::<i64>()`` (which fails on overflow). Otherwise the two builds would derive different
#: estimates from the SAME store cell (e.g. a huge ``peak_bytes``), breaking cross-language parity.
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1

#: ASCII whitespace trimmed from a numeric cell before parsing — EXACTLY the five characters Rust's
#: ``char::is_ascii_whitespace`` trims (tab, newline, form-feed, carriage-return, space; note it does
#: NOT include the vertical tab 0x0b). Trimming the same set in both builds lets a whitespace-padded
#: cell (a common CSV-producer artifact from spreadsheets/awk/hand-edited fixtures) parse to the same
#: number in both, instead of Python accepting it while Rust's strict parse silently drops it.
_ASCII_WS = "\t\n\x0c\r "


class Planner(Enum):
    """Which scheduling planner to use for dispatch ordering."""

    GREEDY_LPT = "greedy-lpt"
    CRITICAL_PATH = "critical-path"
    #: CPA (Radulescu & van Gemund 2001): a two-phase moldable allocator that first picks each
    #: step's inner-jobs width by balancing the critical path against the per-core area over the
    #: MEASURED speedup curves, then list-schedules by critical-path order at the allocated widths.
    #: See ``common/docs/dagrun/PLANNER_DESIGN.md``.
    CPA = "cpa"

    @classmethod
    def from_value(cls, text: str) -> "Planner | None":
        """Return the planner named by ``text``, or ``None`` for an unknown value."""
        for planner in cls:
            if planner.value == text:
                return planner
        return None


# --------------------------------------------------------------------------- reader


@dataclass(frozen=True)
class StepSamples:
    """Aggregated store estimates for ONE step, from its recorded samples.

    ``est_duration_s`` / ``rss_estimate_bytes`` are ``None`` when no sample carried a parseable
    value for that column, so the caller can fall back to the DAG hint.
    """

    step: str
    samples: int
    est_duration_s: float | None
    rss_estimate_bytes: int | None


@dataclass(frozen=True)
class Sample:
    """ONE recorded per-step measurement, reduced to exactly the fields the estimator + speedup
    model consume. This is the atom of the mergeable profile SUMMARY
    (:mod:`dagrun.summary`): a bounded reservoir of these per ``(step, inner_jobs)``
    bucket is what the summary stores, so the summary's recomputed estimates equal the estimates
    over the raw rows the samples came from (the reservoir is the only lossy step).

    * ``elapsed_s`` — raw recorded wall seconds (``>= 0``), or ``None`` when the row had no
      parseable ``elapsed_s``. The INTRINSIC (uncontended) wall the estimator medians is
      ``elapsed_s * (1 - contention)`` (see :meth:`intrinsic_s`); raw elapsed + contention are kept
      SEPARATELY (rather than pre-multiplied) so a summary is inspectable and the discount is
      transparent.
    * ``contention`` — the fraction of machine capacity OTHER work took during the sample
      (:func:`_contention_fraction`), clamped to ``[0, _MAX_CONTENTION]``. Always present (``0.0``
      when the store carried no contention signal).
    * ``cpu_s`` — total CPU-seconds (``user_s`` + ``sys_s``), the work-conservation signal, or
      ``None``.
    * ``effective_cores`` / ``throttled_s`` — achieved parallelism and throttling at this width, or
      ``None``.
    * ``peak_bytes`` — the raw recorded peak retained for pooled compatibility feedback.
    * ``uncensored_peak_bytes`` — a peak proven usable as an exact-width estimate (rows without
      provenance retain the compatibility treatment).
    * ``peak_floor_bytes`` — a censored peak usable only as a lower bound.
    * ``observation_id`` — stable identity used only by bounded summary sampling/merge. Ordinary
      estimators ignore it.
    * ``workload_digest`` — stable identity for the command shape measured by a sweep. Empty for
      pre-digest rows; planners use it to avoid combining incompatible curves.
    """

    elapsed_s: float | None
    contention: float
    cpu_s: float | None
    effective_cores: float | None
    throttled_s: float | None
    peak_bytes: int | None
    uncensored_peak_bytes: int | None = None
    peak_floor_bytes: int | None = None
    observation_id: str = ""
    workload_digest: str = ""

    def intrinsic_s(self) -> float | None:
        """The contention-discounted (intrinsic / uncontended) wall the estimator medians, or
        ``None`` when this sample carried no ``elapsed_s``."""
        if self.elapsed_s is None:
            return None
        return self.elapsed_s * (1.0 - self.contention)


def feedback_identity() -> tuple[str, str]:
    """The ``(machine_id, container_class)`` the feedback reader selects the store file by.

    Values come from the current host unless :data:`MACHINE_ID_ENV` or
    :data:`CONTAINER_CLASS_ENV` supplies an override."""
    machine_id = os.environ.get(MACHINE_ID_ENV) or perflog.machine_id()
    container_class = os.environ.get(CONTAINER_CLASS_ENV) or perflog.container_class()
    return machine_id, container_class


def _median(sorted_values: Sequence[float]) -> float:
    """Median of an already-sorted, non-empty sequence (average of the two middle values for an
    even count). The caller guarantees non-emptiness."""
    n = len(sorted_values)
    mid = n // 2
    if n % 2 == 1:
        return sorted_values[mid]
    return (sorted_values[mid - 1] + sorted_values[mid]) / 2.0


def _robust_median(values: Sequence[float]) -> float:
    """A robust central estimate of contention-/noise-inflated samples (wall, CPU-seconds, ...).

    At THREE or more samples this is a MAD-trimmed median: the median after dropping samples more
    than ``_MAD_TRIM_K`` MADs from it (median-absolute-deviation outlier rejection), falling back to
    the plain median when the MAD is zero (all-equal).

    At FEWER than three samples MAD-trim provably cannot reject an outlier — the median of two points
    sits midway between them, so any symmetric cutoff keeps BOTH and the estimate collapses to their
    MEAN, which a single slow sample drags by half its excess (e.g. ``[5, 100] -> 52.5``, inverting a
    real 2x speedup into an apparent slowdown). Since every quantity fed here can only be INFLATED by
    contention/noise (a step never runs faster than its intrinsic cost), the smaller observation is
    the better intrinsic estimate, so at ``n < 3`` we return the MINIMUM: robust to one upward
    (slow/contended) outlier at ``n == 2`` and self-healing to the MAD-trimmed median as samples
    accumulate. The caller guarantees a non-empty sequence."""
    xs = sorted(values)
    if len(xs) < 3:
        return xs[0]  # sorted ascending -> the minimum (see docstring: robust to a slow outlier)
    m = _median(xs)
    deviations = sorted(abs(x - m) for x in xs)
    mad = _median(deviations)
    if mad > 0.0:
        cutoff = _MAD_TRIM_K * mad
        kept = [x for x in xs if abs(x - m) <= cutoff]
        if kept:
            return _median(sorted(kept))
    return m


def _high_percentile(values: Sequence[int]) -> int:
    """The :data:`_RSS_PCTL_NUM`/:data:`_RSS_PCTL_DEN` percentile of a non-empty int sequence by
    NEAREST-RANK, using integer arithmetic so the chosen index is identical across languages.

    ``rank = ceil(num*n/den)`` via the integer identity ``ceil(a/b) == (a + b - 1) // b``; the
    1-based rank clamps to ``n`` and the returned value is the rank-th smallest."""
    xs = sorted(values)
    n = len(xs)
    rank = (_RSS_PCTL_NUM * n + _RSS_PCTL_DEN - 1) // _RSS_PCTL_DEN
    if rank < 1:
        rank = 1
    if rank > n:
        rank = n
    return xs[rank - 1]


#: Contention columns the reader understands, in priority order. Each maps a recorded value to a
#: fraction ``c`` in ``[0, 1)`` of machine capacity consumed by OTHER work during the sample; the
#: intrinsic (uncontended) duration is estimated as ``elapsed * (1 - c)``. Only the FIRST present,
#: parseable column is used. ``pct_*`` are percentages (0..100); ``external_cores`` is absolute
#: cores (divided by the affinity width parsed from ``container_class``); ``co_tenants`` is a count
#: of co-running tenants (n co-tenants sharing equally => the step keeps ``1/(n+1)`` => ``c =
#: n/(n+1)``). These are consumed "where present": the boxed writer now populates ``external_cores``
#: on every real per-step row (see :func:`dagrun.profile_enrich.step_enrichment_columns`),
#: so a real boxed run's median IS contention-discounted via that signal. The ``pct_*`` columns are
#: still supplied only by synthetic fixtures (they exercise the percentage-discount path); the
#: ``co_tenants`` / PSI fallback NAMES here predate the writer and do NOT yet match the columns it
#: emits (``co_tenants_start``/``_end``, ``*_psi_avg10_start``/``_end``) — folding those start/end
#: pairs into a discount is a scoped follow-on, so they are inert on real data today.
_CONTENTION_PCT_COLUMNS = ("pct_other", "psi_cpu_some_avg10", "cpu_pressure_some_avg10")


def _affinity_width(container_class: str) -> int | None:
    """Parse the affinity (core) width out of a ``container_class`` like
    ``affinity316_cpu-max-...`` -> ``316``. ``None`` if the shape is unexpected."""
    if not container_class.startswith("affinity"):
        return None
    rest = container_class[len("affinity"):]
    digits = ""
    for ch in rest:
        # ASCII digits ONLY, matching Rust's ``char::is_ascii_digit``. Python's ``str.isdigit()``
        # also returns True for Unicode digit characters (e.g. the superscript ``²``) that ``int()``
        # then cannot parse, which would raise here — restricting to ASCII keeps this function total
        # (the module never raises, per the docstring) and identical to the Rust build.
        if ch.isascii() and ch.isdigit():
            digits += ch
        else:
            break
    if not digits:
        return None
    value = int(digits)
    # Bound to i64 as Rust's ``digits.parse::<i64>().ok()`` does, so an absurdly wide affinity token
    # is rejected identically in both builds instead of Python keeping an out-of-range int.
    if value < _I64_MIN or value > _I64_MAX:
        return None
    return value


def _clean_numeric_cell(cell: str | None) -> str | None:
    """Trim surrounding ASCII whitespace and reject anything Rust's strict ``str::parse`` would
    reject anyway (non-ASCII characters and PEP-515 ``_`` separators), returning the cleaned token or
    ``None``.

    This is the single source of truth that keeps :func:`_parse_float` / :func:`_parse_int` accepting
    EXACTLY the same set of numeric tokens as the Rust build: Python's ``float()`` / ``int()`` are
    permissive (they accept leading/trailing whitespace, underscore separators, and — for ``int`` —
    out-of-``i64`` magnitudes) while Rust's ``parse`` is strict, so without this normalization the
    two builds would silently derive different estimates from the same store."""
    if cell is None:
        return None
    token = cell.strip(_ASCII_WS)
    if not token or not token.isascii() or "_" in token:
        return None
    return token


def _parse_float(cell: str | None) -> float | None:
    token = _clean_numeric_cell(cell)
    if token is None:
        return None
    try:
        value = float(token)
    except ValueError:
        return None
    # Reject non-finite values. Python's ``float()`` accepts ``"inf"`` / ``"nan"`` and overflowing
    # literals (``"1e400"`` -> ``inf``); Rust's ``str::parse::<f64>()`` accepts the same set, so both
    # builds MUST drop them to stay byte-identical AND to keep one bogus cell from poisoning a median
    # or a contention fraction (a lone ``inf`` elapsed_s would otherwise dominate a width's wall).
    if not math.isfinite(value):
        return None
    return value


def _parse_int(cell: str | None) -> int | None:
    token = _clean_numeric_cell(cell)
    if token is None:
        return None
    try:
        value = int(token)
    except ValueError:
        return None
    # Reject out-of-i64 magnitudes so a value Python's arbitrary-precision ``int`` would keep is
    # dropped exactly as Rust's ``str::parse::<i64>()`` drops it (see :data:`_I64_MIN`).
    if value < _I64_MIN or value > _I64_MAX:
        return None
    return value


def _contention_fraction(row: Mapping[str, str], affinity_width: int | None) -> float:
    """The fraction of machine capacity taken by OTHER work during a sample, from whichever
    contention column is present (see :data:`_CONTENTION_PCT_COLUMNS`). ``0.0`` when the store
    carries no usable contention signal (=> no discount). Clamped to :data:`_MAX_CONTENTION`."""
    fraction: float | None = None
    for col in _CONTENTION_PCT_COLUMNS:
        pct = _parse_float(row.get(col))
        if pct is not None:
            fraction = pct / 100.0
            break
    if fraction is None:
        external = _parse_float(row.get("external_cores"))
        if external is not None and affinity_width and affinity_width > 0:
            fraction = external / float(affinity_width)
    if fraction is None:
        co_tenants = _parse_float(row.get("co_tenants"))
        if co_tenants is not None and co_tenants > 0.0:
            fraction = co_tenants / (co_tenants + 1.0)
    if fraction is None:
        return 0.0
    if fraction < 0.0:
        return 0.0
    if fraction > _MAX_CONTENTION:
        return _MAX_CONTENTION
    return fraction


def _load_store(
    profile_dir: str | Path, machine_id: str, container_class: str
) -> tuple[list[dict[str, str]], int | None] | None:
    """Read ``<profile_dir>/step_profiles_<machine_id>_<container_class>.csv`` into a list of
    column->value row dicts plus the parsed affinity (core) width, or ``None`` when the file is
    absent. The single CSV-reading path shared by :func:`load_step_samples` and
    :func:`load_step_speedups`, so both consume the store identically (DRY)."""
    path = Path(profile_dir) / f"step_profiles_{machine_id}_{container_class}.csv"
    if not path.is_file():
        return None
    rows: list[dict[str, str]] = []
    with path.open(newline="", encoding="utf-8") as handle:
        for raw in csv.DictReader(handle):
            rows.append(
                {k: (v if isinstance(v, str) else "") for k, v in raw.items() if isinstance(k, str)}
            )
    return rows, _affinity_width(container_class)


#: The bucket key for the mergeable summary + the shared aggregation core: ``(step, inner_jobs)``.
#: ``inner_jobs`` is ``0`` for a row whose ``inner_jobs`` cell is absent / unparseable / non-positive
#: (such rows still count toward :func:`step_samples_from_buckets`, which aggregates across widths,
#: but are excluded from :func:`step_speedups_from_buckets`, which needs a real width). Real boxed
#: rows always resolve ``inner_jobs`` to ``>= 1``, so the ``0`` bucket only appears for hand-written
#: / synthetic data.
BucketKey = tuple[str, int]


def sample_from_row(row: Mapping[str, str], affinity_width: int | None) -> Sample:
    """Reduce one raw profile row to the :class:`Sample` the estimator + speedup model consume.

    The SINGLE extraction path: both the CSV readers (:func:`load_step_samples` /
    :func:`load_step_speedups`) and the mergeable summary
    (:func:`dagrun.summary.summary_from_rows`) go through here, so a summary built from
    a store yields the same estimates as reading the store directly. A cell that fails strict
    numeric parsing becomes ``None`` for that field rather than being coerced; negative values are
    discarded by both readers."""
    elapsed = _parse_float(row.get("elapsed_s"))
    elapsed_s = elapsed if (elapsed is not None and elapsed >= 0.0) else None
    user = _parse_float(row.get("user_s"))
    system = _parse_float(row.get("sys_s"))
    if user is not None and system is not None and user >= 0.0 and system >= 0.0:
        cpu_s: float | None = user + system
    else:
        cpu_s = None
    eff = _parse_float(row.get("effective_cores"))
    effective_cores = eff if (eff is not None and eff >= 0.0) else None
    throttled = _parse_float(row.get("throttled_s"))
    throttled_s = throttled if (throttled is not None and throttled >= 0.0) else None
    peak = _parse_int(row.get("peak_bytes"))
    peak_bytes = peak if (peak is not None and peak >= 0) else None
    uncensored_peak_bytes = peak_bytes
    peak_floor_bytes: int | None = None
    if peak_bytes is not None:
        # Old stores predate memory-cap/event provenance. Preserve their historical behavior, but
        # when a modern row carries that schema accept its peak only if the censoring classifier can
        # prove the measurement was unconstrained. Censored/unknown peaks remain available to the
        # dedicated memory-feedback path as floors; they are not exact M(p) observations.
        provenance_columns = (
            "memory_max_bytes",
            "memory_events_high",
            "memory_events_max",
            "memory_events_oom",
            "memory_events_oom_kill",
            "memory_events_oom_group_kill",
        )
        if any(column in row for column in provenance_columns):
            from dagrun.memory_feedback import Censoring, peak_observation_from_row

            verdict = peak_observation_from_row(row).verdict
            if verdict is Censoring.CENSORED:
                peak_floor_bytes = peak_bytes
                uncensored_peak_bytes = None
            elif verdict is not Censoring.UNCENSORED:
                uncensored_peak_bytes = None
    return Sample(
        elapsed_s=elapsed_s,
        contention=_contention_fraction(row, affinity_width),
        cpu_s=cpu_s,
        effective_cores=effective_cores,
        throttled_s=throttled_s,
        peak_bytes=peak_bytes,
        uncensored_peak_bytes=uncensored_peak_bytes,
        peak_floor_bytes=peak_floor_bytes,
        observation_id=(row.get("observation_id") or row.get("run_id") or ""),
        workload_digest=(row.get("workload_digest") or "").strip(),
    )


def _row_inner_jobs(row: Mapping[str, str]) -> int:
    """The ``inner_jobs`` bucket component for a row: the parsed positive width, else ``0``."""
    inner = _parse_int(row.get("inner_jobs"))
    return inner if (inner is not None and inner > 0) else 0


#: Row cells that, when explicitly truthy, mean the step did NOT complete its work.
_FAILURE_FLAG_COLUMNS = ("timed_out", "cpu_timed_out")


def _is_truthy_flag(value: str | None) -> bool:
    """True only for an explicit affirmative cell. A blank/absent cell is NOT a failure."""
    return (value or "").strip().lower() in {"true", "1", "yes"}


def row_is_measurement(row: Mapping[str, str]) -> bool:
    """True when a profile row is a TIMING MEASUREMENT rather than a record of a failed step.

    A timed-out run's duration is the moment the guard fired, and an OOM-killed run's duration is
    the moment the kernel intervened; neither is how long the work takes. Admitting them fits the
    speedup curve partly to failures, and the result is indistinguishable from ordinary data: a
    step that dies fast at every width looks exactly like a step that is flat and very quick.

    FAIL-OPEN ON SILENCE, BY DESIGN. Only an EXPLICIT failure signal rejects a row:

    * ``ok`` explicitly falsy, or
    * ``returncode`` present, parseable and non-zero, or
    * ``timed_out`` / ``cpu_timed_out`` explicitly truthy, or
    * ``oom_kills`` present, parseable and greater than zero.

    A row whose verdict cells are absent or blank is ACCEPTED. A store may carry no verdict columns
    at all, and a filter that rejected silence would discard every such sample and leave the model
    with nothing -- trading a wrong answer for no answer. This gate rejects recorded failure, not
    unfamiliarity.
    """
    ok_cell = (row.get("ok") or "").strip().lower()
    if ok_cell and ok_cell not in {"true", "1", "yes"}:
        return False
    rc = _parse_int(row.get("returncode"))
    if rc is not None and rc != 0:
        return False
    if any(_is_truthy_flag(row.get(col)) for col in _FAILURE_FLAG_COLUMNS):
        return False
    oom = _parse_int(row.get("oom_kills"))
    if oom is not None and oom > 0:
        return False
    return True


def bucketize_rows(
    rows: Sequence[Mapping[str, str]], affinity_width: int | None
) -> dict[BucketKey, list[Sample]]:
    """Group raw profile ``rows`` into per-``(step, inner_jobs)`` sample lists.

    A row with no ``step`` cell is skipped (it cannot be attributed), and so is a row that records
    a FAILED step rather than a measurement (:func:`row_is_measurement`) -- a timed-out or
    OOM-killed duration is not a timing. Every other row becomes one :class:`Sample`. This is the
    raw (UNCAPPED) bucketization shared by the CSV readers and the summary builder; the summary
    builder additionally caps each bucket to its reservoir size."""
    buckets: dict[BucketKey, list[Sample]] = {}
    for row in rows:
        step = row.get("step")
        if not step:
            continue
        if not row_is_measurement(row):
            continue
        key: BucketKey = (step, _row_inner_jobs(row))
        buckets.setdefault(key, []).append(sample_from_row(row, affinity_width))
    return buckets


def buckets_for_workloads(
    buckets: Mapping[BucketKey, Sequence[Sample]],
    workload_digests: Mapping[str, str] | None,
) -> dict[BucketKey, list[Sample]]:
    """Select the cohort matching the current DAG without mixing command revisions.

    As soon as any sample for a step matches its expected non-empty digest, only exact matches for
    that step remain eligible. Until then, blank pre-digest samples are a compatibility fallback;
    samples carrying a different non-empty digest are never blended into either cohort. Steps not
    present in ``workload_digests`` are unchanged.
    """

    expected = workload_digests or {}
    if not expected:
        return {key: list(samples) for key, samples in buckets.items()}
    matched_steps = {
        step
        for (step, _inner), samples in buckets.items()
        if (wanted := expected.get(step)) is not None
        and any(sample.workload_digest == wanted for sample in samples)
    }
    selected: dict[BucketKey, list[Sample]] = {}
    for key, samples in buckets.items():
        step = key[0]
        wanted = expected.get(step)
        if wanted is None:
            kept = list(samples)
        elif step in matched_steps:
            kept = [sample for sample in samples if sample.workload_digest == wanted]
        else:
            kept = [sample for sample in samples if not sample.workload_digest]
        if kept:
            selected[key] = kept
    return selected


def step_samples_from_buckets(buckets: Mapping[BucketKey, Sequence[Sample]]) -> dict[str, StepSamples]:
    """Aggregate per-``(step, inner_jobs)`` sample buckets into per-step robust estimates.

    Aggregates across ALL of a step's widths (the memory + duration model does not distinguish
    inner_jobs): the contention-discounted MAD-trimmed median intrinsic wall and the high-percentile
    ``peak_bytes``, plus the total sample count. The shared core behind :func:`load_step_samples`
    (CSV) and :func:`dagrun.summary.step_samples_from_summary` (mergeable summary)."""
    durations: dict[str, list[float]] = {}
    peaks: dict[str, list[int]] = {}
    counts: dict[str, int] = {}
    for (step, _inner), samples in buckets.items():
        counts[step] = counts.get(step, 0) + len(samples)
        for sample in samples:
            intrinsic = sample.intrinsic_s()
            if intrinsic is not None:
                durations.setdefault(step, []).append(intrinsic)
            if sample.peak_bytes is not None:
                peaks.setdefault(step, []).append(sample.peak_bytes)
    result: dict[str, StepSamples] = {}
    for step, n in counts.items():
        step_durations = durations.get(step, [])
        step_peaks = peaks.get(step, [])
        result[step] = StepSamples(
            step=step,
            samples=n,
            est_duration_s=_robust_median(step_durations) if step_durations else None,
            rss_estimate_bytes=_high_percentile(step_peaks) if step_peaks else None,
        )
    return result


def load_step_samples(
    profile_dir: str | Path,
    machine_id: str,
    container_class: str,
    workload_digests: Mapping[str, str] | None = None,
) -> dict[str, StepSamples]:
    """Read the per-step profile CSV for ``(machine_id, container_class)`` under ``profile_dir``
    and aggregate the recorded samples per step into robust estimates.

    Reads exactly ``<profile_dir>/step_profiles_<machine_id>_<container_class>.csv`` (the file the
    writer names per machine + container). Returns ``{}`` when that file is absent — the caller
    then falls back to DAG hints for every step (graceful sparse-data handling). Each returned
    :class:`StepSamples` carries the contention-discounted MAD-trimmed median ``elapsed_s`` and the
    high-percentile ``peak_bytes`` for that step, or ``None`` for a column that no sample supplied.
    """
    loaded = _load_store(profile_dir, machine_id, container_class)
    if loaded is None:
        return {}
    rows, affinity_width = loaded
    buckets = bucketize_rows(rows, affinity_width)
    return step_samples_from_buckets(buckets_for_workloads(buckets, workload_digests))


# --------------------------------------------------------------------------- speedup model

#: The economic plateau is the NARROWEST measured width whose wall is within this fraction of the
#: best measured wall. This definition is deliberately global rather than adjacent-grid based:
#: inserting a midpoint such as 48 between 32 and 64 must not move the recommendation merely
#: because two neighboring ratios changed. Ten percent gives the intended result for a curve such
#: as 7.2x at 8 threads versus 7.9x at 64: 8 is already within 10% of the best wall while consuming
#: one eighth of the advertised width.
_PLATEAU_WALL_TOLERANCE = 0.10
#: If total CPU-seconds (user+sys) grow by more than this factor relative to the BASELINE width, the
#: point is not an economic plateau candidate. It may remain useful diagnostically (and is retained
#: in the curve), but spending >50% more CPU work for the wall-time gain should not become the
#: default recommendation when other productive work can use the machine.
_SPEEDUP_MAX_WORK_GROWTH = 1.5
#: A step needs at least this many DISTINCT inner_jobs levels (with wall data) to model a curve;
#: fewer and there is nothing to say about speedup vs. parallelism.
_SPEEDUP_MIN_LEVELS = 2
#: A width-specific memory response must have enough repeats to be more than a one-off peak before
#: it replaces the conservative authored/pooled fallback in allocation.
_MEMORY_MIN_SAMPLES_PER_LEVEL = 3
#: A wider level must be at least this much SLOWER than the fastest measured level before it can be
#: called a regression. Paired with the dispersion test in :func:`_regression_inner_jobs`; neither
#: check is sufficient alone.
_REGRESSION_MIN_SLOWDOWN = 1.05


def _regression_inner_jobs(levels: Sequence[SpeedupLevel]) -> int | None:
    """The narrowest width above the fastest one where going wider is measurably SLOWER.

    Two conditions must BOTH hold, and requiring both is the whole point:

    1. the level's median wall exceeds the fastest level's by :data:`_REGRESSION_MIN_SLOWDOWN`, and
    2. its observed [min, max] wall range is DISJOINT from the fastest level's range.

    Condition 2 is what keeps this honest on a shared machine. A percentage test alone reports
    ordinary sample noise as a cliff: a width measured 13.7% slower than the best, whose sample
    range still overlaps the best width's, is not distinguishable from it, and acting on that would
    narrow a step for no reason. A level missing either bound cannot be separated from the best one
    and is skipped rather than guessed at.

    Returns ``None`` when nothing above the fastest width regresses, which is the common case for a
    curve that merely flattens.
    """
    ranked = [lvl for lvl in levels if lvl.wall_s > 0.0]
    if not ranked:
        return None
    best = min(ranked, key=lambda lvl: lvl.wall_s)
    if best.wall_min_s is None or best.wall_max_s is None:
        return None
    for level in sorted(ranked, key=lambda lvl: lvl.inner_jobs):
        if level.inner_jobs <= best.inner_jobs:
            continue
        if level.wall_min_s is None or level.wall_max_s is None:
            continue
        if level.wall_s <= best.wall_s * _REGRESSION_MIN_SLOWDOWN:
            continue
        overlaps = not (level.wall_min_s > best.wall_max_s or level.wall_max_s < best.wall_min_s)
        if not overlaps:
            return level.inner_jobs
    return None


@dataclass(frozen=True)
class SpeedupLevel:
    """One measured point on a step's speedup curve: its robust wall (and work) at one inner_jobs
    width, plus the speedup relative to the smallest measured width.

    ``wall_s`` is MODELLED and ``raw_wall_s`` is MEASURED, and they are carried side by side on
    purpose. ``wall_s`` has a contention discount applied (see :meth:`Sample.intrinsic_s`), which is
    not a small correction -- on a busy host it has been observed to move a width-1 wall from a
    measured 94.435 s to a discounted 71.278 s, 25%. A consumer that prints only ``wall_s`` is
    presenting a modelled number as a measurement; print both, or say which one it is.
    """

    inner_jobs: int
    samples: int
    #: Contention-discounted MAD-trimmed median ``elapsed_s`` at this width. MODELLED: this is what
    #: the curve is fitted to, so the speedup and the recommendation both derive from it.
    wall_s: float
    #: MAD-trimmed median of the RAW recorded ``elapsed_s`` at this width, with no discount applied
    #: -- the measurement ``wall_s`` was derived from. ``None`` when no sample carried a wall.
    raw_wall_s: float | None
    #: Smallest and largest contention-discounted wall observed at this width. The spread of the
    #: samples, kept so a regression verdict can be checked against dispersion rather than taken on
    #: trust: two widths whose ranges overlap are not distinguishable.
    wall_min_s: float | None
    wall_max_s: float | None
    #: Robust-median total CPU-seconds (``user_s`` + ``sys_s``) — the work-conservation signal.
    cpu_s: float | None
    #: Robust-median achieved parallelism (``effective_cores``) at this width.
    effective_cores: float | None
    #: Robust-median ``throttled_s`` at this width (rising throttling is a saturation signal).
    throttled_s: float | None
    #: ``wall(baseline) / wall(this)`` — the measured speedup vs. the smallest width.
    speedup: float
    #: High-percentile measured cgroup ``memory.peak`` at this width. This is the empirical
    #: memory-response point M(p), kept per width rather than pooled across unrelated widths.
    peak_bytes: int | None = None
    peak_samples: int = 0
    #: Largest censored peak observed at this width. It is not an estimate of the maximum, but the
    #: planner may never model demand below this proven lower bound.
    peak_floor_bytes: int | None = None
    peak_floor_samples: int = 0


@dataclass(frozen=True)
class StepSpeedup:
    """A step's fitted speedup curve across inner_jobs widths, plus the recommended width.

    ``recommended_inner_jobs`` is the NARROWEST width within
    :data:`_PLATEAU_WALL_TOLERANCE` of the best measured wall, subject to the core budget and the
    global work-inflation guard (:data:`_SPEEDUP_MAX_WORK_GROWTH`). This is grid-invariant: adding a
    midpoint measurement cannot move the plateau merely by changing an adjacent ratio.

    A PLATEAU AND A CLIFF ARE DIFFERENT THINGS and the recommendation alone cannot tell them apart.
    A curve that flattens above the plateau and a curve that becomes 2.4x SLOWER above it can yield
    the same ``recommended_inner_jobs``. The recommendation chooses the narrow economic point, but
    "safe" is not "visible", and an operator widening a step by hand still needs to know a cliff is
    there. ``regression_inner_jobs`` names it.
    """

    step: str
    baseline_inner_jobs: int
    recommended_inner_jobs: int
    #: Achieved parallelism at the recommended width (``None`` if not measured there).
    measured_effective_cores: float | None
    #: The narrowest width ABOVE the fastest measured one where going wider is measurably SLOWER,
    #: or ``None`` when no width regresses within the measured range. See
    #: :func:`_regression_inner_jobs` for the dispersion test that must be satisfied before a width
    #: is named here; a slower median alone is not enough.
    regression_inner_jobs: int | None
    levels: tuple[SpeedupLevel, ...]


@dataclass(frozen=True)
class LevelAggregate:
    """The per-width aggregate :func:`_build_step_speedup` fits one :class:`SpeedupLevel` from.

    A named carrier rather than a positional tuple: the fit needs the discounted median, the raw
    median it came from, and the observed spread, and a nine-slot tuple makes those trivially easy
    to transpose at a call site."""

    inner_jobs: int
    samples: int
    #: Contention-discounted MAD-trimmed median wall (what the curve is fitted to).
    wall_s: float
    #: MAD-trimmed median of the raw recorded walls, undiscounted.
    raw_wall_s: float | None
    #: Smallest / largest discounted wall observed at this width.
    wall_min_s: float | None
    wall_max_s: float | None
    cpu_s: float | None
    effective_cores: float | None
    throttled_s: float | None
    peak_bytes: int | None
    peak_samples: int
    peak_floor_bytes: int | None
    peak_floor_samples: int


def _build_step_speedup(
    step: str,
    raw_levels: Sequence[LevelAggregate],
    core_budget: int | None,
) -> StepSpeedup:
    """Assemble a :class:`StepSpeedup` from per-level aggregates SORTED ascending by inner_jobs.
    Deterministic across builds (only compares robust medians of identical inputs)."""
    baseline_j = raw_levels[0].inner_jobs
    baseline_wall = raw_levels[0].wall_s
    levels: list[SpeedupLevel] = []
    baseline_cpu = raw_levels[0].cpu_s
    eff_by_j: dict[int, float | None] = {}
    for aggregate in raw_levels:
        j, wall, cpu, eff = (
            aggregate.inner_jobs,
            aggregate.wall_s,
            aggregate.cpu_s,
            aggregate.effective_cores,
        )
        speedup = baseline_wall / wall if wall > 0.0 else 1.0
        levels.append(
            SpeedupLevel(
                inner_jobs=j,
                samples=aggregate.samples,
                wall_s=wall,
                raw_wall_s=aggregate.raw_wall_s,
                wall_min_s=aggregate.wall_min_s,
                wall_max_s=aggregate.wall_max_s,
                cpu_s=cpu,
                effective_cores=eff,
                throttled_s=aggregate.throttled_s,
                peak_bytes=aggregate.peak_bytes,
                peak_samples=aggregate.peak_samples,
                peak_floor_bytes=aggregate.peak_floor_bytes,
                peak_floor_samples=aggregate.peak_floor_samples,
                speedup=speedup,
            )
        )
        eff_by_j[j] = eff
    within_budget = [
        level
        for level in levels
        if core_budget is None or level.inner_jobs <= core_budget
    ]
    economic = [
        level
        for level in within_budget
        if (
            level.cpu_s is None
            or baseline_cpu is None
            or baseline_cpu <= 0.0
            or level.cpu_s / baseline_cpu <= _SPEEDUP_MAX_WORK_GROWTH
        )
    ]
    candidates = economic or within_budget or levels
    best_wall = min(level.wall_s for level in candidates)
    plateau_limit = best_wall * (1.0 + _PLATEAU_WALL_TOLERANCE)
    recommended = min(
        level.inner_jobs for level in candidates if level.wall_s <= plateau_limit
    )
    return StepSpeedup(
        step=step,
        baseline_inner_jobs=baseline_j,
        recommended_inner_jobs=recommended,
        measured_effective_cores=eff_by_j.get(recommended),
        regression_inner_jobs=_regression_inner_jobs(levels),
        levels=tuple(levels),
    )


def step_speedups_from_buckets(
    buckets: Mapping[BucketKey, Sequence[Sample]], core_budget: int | None
) -> dict[str, StepSpeedup]:
    """Fit each step's PARALLEL-SPEEDUP curve from per-``(step, inner_jobs)`` sample buckets.

    For each width (``inner_jobs > 0``) with wall data, derives a robust (MAD-trimmed median)
    contention-discounted wall, the work-conservation signal (median total CPU-seconds), achieved
    ``effective_cores``, and ``throttled_s``, then fits the speedup curve + RECOMMENDED width within
    the knee and ``core_budget``. Steps with fewer than :data:`_SPEEDUP_MIN_LEVELS` measured widths
    are absent. The shared core behind :func:`load_step_speedups` (CSV) and
    :func:`dagrun.summary.step_speedups_from_summary` (mergeable summary)."""
    walls: dict[BucketKey, list[float]] = {}
    raws: dict[BucketKey, list[float]] = {}
    cpus: dict[BucketKey, list[float]] = {}
    effs: dict[BucketKey, list[float]] = {}
    thrs: dict[BucketKey, list[float]] = {}
    peaks: dict[BucketKey, list[int]] = {}
    peak_floors: dict[BucketKey, list[int]] = {}
    for (step, inner), samples in buckets.items():
        if inner <= 0:
            continue
        key: BucketKey = (step, inner)
        for sample in samples:
            intrinsic = sample.intrinsic_s()
            if intrinsic is not None:
                walls.setdefault(key, []).append(intrinsic)
            if sample.elapsed_s is not None:
                raws.setdefault(key, []).append(sample.elapsed_s)
            if sample.cpu_s is not None:
                cpus.setdefault(key, []).append(sample.cpu_s)
            if sample.effective_cores is not None:
                effs.setdefault(key, []).append(sample.effective_cores)
            if sample.throttled_s is not None:
                thrs.setdefault(key, []).append(sample.throttled_s)
            if sample.uncensored_peak_bytes is not None:
                peaks.setdefault(key, []).append(sample.uncensored_peak_bytes)
            if sample.peak_floor_bytes is not None:
                peak_floors.setdefault(key, []).append(sample.peak_floor_bytes)
    by_step: dict[str, list[int]] = {}
    for step, inner in walls:
        by_step.setdefault(step, []).append(inner)
    result: dict[str, StepSpeedup] = {}
    for step, widths in by_step.items():
        levels_j = sorted(set(widths))
        if len(levels_j) < _SPEEDUP_MIN_LEVELS:
            continue
        raw_levels: list[LevelAggregate] = []
        for inner in levels_j:
            key = (step, inner)
            wall_samples = walls[key]
            raw_samples = raws.get(key)
            cpu_samples = cpus.get(key)
            eff_samples = effs.get(key)
            thr_samples = thrs.get(key)
            peak_samples = peaks.get(key)
            floor_samples = peak_floors.get(key)
            raw_levels.append(
                LevelAggregate(
                    inner_jobs=inner,
                    samples=len(wall_samples),
                    wall_s=_robust_median(wall_samples),
                    raw_wall_s=_robust_median(raw_samples) if raw_samples else None,
                    wall_min_s=min(wall_samples),
                    wall_max_s=max(wall_samples),
                    cpu_s=_robust_median(cpu_samples) if cpu_samples else None,
                    effective_cores=_robust_median(eff_samples) if eff_samples else None,
                    throttled_s=_robust_median(thr_samples) if thr_samples else None,
                    peak_bytes=_high_percentile(peak_samples) if peak_samples else None,
                    peak_samples=len(peak_samples) if peak_samples else 0,
                    peak_floor_bytes=max(floor_samples) if floor_samples else None,
                    peak_floor_samples=len(floor_samples) if floor_samples else 0,
                )
            )
        result[step] = _build_step_speedup(step, raw_levels, core_budget)
    return result


def load_step_speedups(
    profile_dir: str | Path,
    machine_id: str,
    container_class: str,
    workload_digests: Mapping[str, str] | None = None,
) -> dict[str, StepSpeedup]:
    """Model each step's PARALLEL-SPEEDUP curve from its samples ACROSS inner_jobs widths.

    Reads the same per-step store as :func:`load_step_samples`, groups samples by
    ``(step, inner_jobs)``, and for each width derives a robust (MAD-trimmed median),
    contention-discounted wall time plus the work-conservation signal (median total CPU-seconds
    ``user_s`` + ``sys_s``), achieved ``effective_cores``, and ``throttled_s``. From those it fits
    the per-step speedup(inner_jobs) curve and a RECOMMENDED width (the narrowest point within 10%
    of the best eligible wall, subject to CPU-work growth and the machine's core budget, with the
    affinity width parsed from ``container_class``).

    Only steps with at least :data:`_SPEEDUP_MIN_LEVELS` distinct widths (with wall data) get a
    model; the rest are absent (the caller shows no speedup curve for them). Never raises on a
    malformed row (an unparseable cell is skipped, matching :func:`load_step_samples`).
    """
    loaded = _load_store(profile_dir, machine_id, container_class)
    if loaded is None:
        return {}
    rows, affinity_width = loaded
    buckets = bucketize_rows(rows, affinity_width)
    return step_speedups_from_buckets(
        buckets_for_workloads(buckets, workload_digests), affinity_width
    )


# --------------------------------------------------------------------------- planner


@dataclass(frozen=True)
class PlanEntry:
    """The resolved estimate + planner metadata for one step (what the plan display shows)."""

    tag: str
    est_duration_s: float
    est_source: str  # "store" | "hint" | "default" | "skip"
    rss_estimate_bytes: int | None
    rss_source: str  # "store" | "hint" | "none"
    bottom_level_s: float
    samples: int
    #: The learned parallel-speedup curve for this step, or ``None`` when the store has fewer than
    #: two inner_jobs widths for it (nothing to model).
    speedup: "StepSpeedup | None" = None
    #: The executable inner-jobs width CPA assigned to a runner-controlled step, or ``None`` for
    #: ordering-only planners and self-managed commands whose empty jobs flag prevents rewriting.
    #: Run-level ``-j`` is the outer bandwidth/per-step ceiling, not an admission reservation.
    alloc_inner_jobs: int | None = None
    #: Width at which ``rss_estimate_bytes`` is an empirical M(p), rather than a pooled/scalar
    #: baseline. Applied configs carry this transient provenance into runtime memory sizing so the
    #: exact-width observation is not scaled by the legacy width heuristic a second time.
    rss_estimate_inner_jobs: int | None = None


@dataclass(frozen=True)
class Allocation:
    """The CPA allocator's whole-DAG summary (``--planner cpa`` only).

    Records the core budget it balanced against, the resulting area and critical-path terms, the
    capacity-model LOWER BOUND ``max(T_CP, area/P)`` (Graham/Brent), the no-overcommit reference
    makespan (a deterministic greedy list-schedule of the allocated widths), and WHY the gradient
    loop stopped. The live runtime may overlap widths beyond ``P`` under the outer quota, so these
    values explain allocation rather than predict contended execution."""

    core_budget: int
    #: Total modeled CPU service Σ C_i(p_i) at the allocated widths. A measured ``cpu_s`` wins;
    #: ``p_i * T_i(p_i)`` is the conservative fallback when the profile lacks CPU counters.
    area_s: float
    #: The area lower-bound term ``area / P`` (= ``T_A``).
    area_bound_s: float
    #: The critical-path length ``T_CP`` at the allocated widths (== ``Plan.critical_path_length_s``).
    critical_path_s: float
    #: The makespan lower bound ``max(T_CP, area/P)``.
    lower_bound_s: float
    #: The no-overcommit reference makespan from a greedy list-schedule of the allocated widths;
    #: always ``>=`` :attr:`lower_bound_s`, but not a live-runtime prediction.
    modeled_makespan_s: float
    #: One of ``balanced`` / ``knee-exhausted`` / ``core-capped`` / ``mem-capped`` /
    #: ``fixed-point`` / ``infeasible-fixed-width`` / ``infeasible-memory``.
    stop_reason: str
    #: Active-step overlap CPA could sustain under the memory budget (bounded by the requested
    #: max-steps ceiling). The reference makespan uses this same concurrency.
    modeled_max_steps: int = 1


@dataclass(frozen=True)
class Plan:
    """A complete plan: per-step resolved estimates, the dispatch order, and the critical path."""

    planner: Planner
    order: tuple[str, ...]
    critical_path: tuple[str, ...]
    critical_path_length_s: float
    entries: tuple[PlanEntry, ...]
    #: The CPA allocator summary (``--planner cpa`` only), else ``None``.
    allocation: "Allocation | None" = None

    def by_tag(self) -> dict[str, PlanEntry]:
        """Index plan entries by their step tags."""
        return {entry.tag: entry for entry in self.entries}


def _resolved_estimate(
    step: Step, samples: StepSamples | None, min_samples: int
) -> tuple[float, str, int | None, str, int]:
    """Resolve one step's effective ``(est_duration_s, est_source, rss, rss_source, samples)``:
    duration uses store-over-hint-over-default, while an authored RSS baseline is a FLOOR.

    Ordinary feedback starts after one sample and exists to improve scheduling estimates. A
    quiet subprocess run is not enough evidence to shrink the next run's enforced ``memory.max``:
    process fan-out and allocator high-water marks vary discontinuously. Therefore a store RSS
    replaces an authored baseline only when it is larger. The separate, explicit
    ``--profile-memory-feedback`` path may lower a baseline after its censor-aware five-sample
    policy and margin; keeping that authority out of the ordinary planner makes the safe default
    independent of sparse local history.
    """
    if step.skip_reason is not None:
        return 0.0, "skip", None, "none", 0

    n = samples.samples if samples is not None else 0
    store_ok = samples is not None and n >= min_samples

    if store_ok and samples is not None and samples.est_duration_s is not None:
        est, est_source = samples.est_duration_s, "store"
    elif step.hint.est_duration_s != 0.0:
        est, est_source = step.hint.est_duration_s, "hint"
    else:
        est, est_source = step.hint.est_duration_s, "default"

    if (
        store_ok
        and samples is not None
        and samples.rss_estimate_bytes is not None
        and (
            step.hint.rss_baseline_bytes is None
            or samples.rss_estimate_bytes > step.hint.rss_baseline_bytes
        )
    ):
        rss, rss_source = samples.rss_estimate_bytes, "store"
    elif step.hint.rss_baseline_bytes is not None:
        rss, rss_source = step.hint.rss_baseline_bytes, "hint"
    else:
        rss, rss_source = None, "none"

    return est, est_source, rss, rss_source, n


def _successors(cfg: DagConfig) -> dict[str, list[str]]:
    """Map each step tag to the tags that DEPEND on it (its downstream successors), in cfg order."""
    succ: dict[str, list[str]] = {step.tag: [] for step in cfg.steps}
    for step in cfg.steps:
        for dep in step.deps:
            if dep in succ:
                succ[dep].append(step.tag)
    return succ


def _bottom_levels(
    cfg: DagConfig, est: Mapping[str, float], successors: Mapping[str, list[str]]
) -> dict[str, float]:
    """Bottom-level (longest remaining est-weighted path to a sink) for every step.

    ``bottom_level(v) = est(v) + max(bottom_level(w) for w in successors(v))`` (just ``est(v)`` at
    a sink). Memoized DFS; the DAG is acyclic so recursion terminates."""
    bottom: dict[str, float] = {}

    def visit(tag: str) -> float:
        cached = bottom.get(tag)
        if cached is not None:
            return cached
        succ = successors.get(tag, [])
        value = est.get(tag, 0.0)
        if succ:
            value += max(visit(w) for w in succ)
        bottom[tag] = value
        return value

    for step in cfg.steps:
        visit(step.tag)
    return bottom


def _critical_path(
    cfg: DagConfig, bottom: Mapping[str, float], successors: Mapping[str, list[str]]
) -> tuple[list[str], float]:
    """The longest est-weighted path through the DAG: start at the step with the greatest
    bottom-level (tie-break by tag) and follow the successor with the greatest bottom-level (same
    tie-break) to a sink. Returns ``(path, length)``; empty for an empty DAG."""
    if not cfg.steps:
        return [], 0.0
    tags = [step.tag for step in cfg.steps]
    start = max(tags, key=lambda t: (bottom.get(t, 0.0), _neg_key(t)))
    path = [start]
    current = start
    while True:
        succ = successors.get(current, [])
        if not succ:
            break
        current = max(succ, key=lambda t: (bottom.get(t, 0.0), _neg_key(t)))
        path.append(current)
    return path, bottom.get(start, 0.0)


class _NegKey:
    """Reverse-ordering wrapper so ``max(..., key=...)`` breaks bottom-level ties by SMALLEST tag
    (ascending), matching the ``(-bottom_level, tag)`` sort key used for dispatch order."""

    __slots__ = ("value",)

    def __init__(self, value: str) -> None:
        self.value = value

    def __lt__(self, other: "_NegKey") -> bool:
        return self.value > other.value

    def __eq__(self, other: object) -> bool:
        return isinstance(other, _NegKey) and self.value == other.value


def _neg_key(tag: str) -> _NegKey:
    return _NegKey(tag)


def _plan_order(
    cfg: DagConfig, planner: Planner, est: Mapping[str, float], bottom: Mapping[str, float]
) -> list[str]:
    """The deterministic dispatch order the scheduler considers ready steps in.

    * ``greedy-lpt``: by ``est_duration`` DESCENDING, ties keeping registration (cfg) order — a
      STABLE reverse sort, matching the scheduler's built-in default.
    * ``critical-path``: by ``bottom_level`` DESCENDING, ties broken by tag ASCENDING — so among
      ready steps the one on the longest remaining path is launched first (critical-path-first
      list scheduling).
    Both are identical across the Python and Rust builds for the same estimates + DAG."""
    tags = [step.tag for step in cfg.steps]
    if planner is Planner.CRITICAL_PATH:
        return sorted(tags, key=lambda t: (-bottom.get(t, 0.0), t))
    # greedy-lpt: stable reverse sort by est (ties keep registration order).
    return sorted(tags, key=lambda t: est.get(t, 0.0), reverse=True)


# --------------------------------------------------------------------------- CPA allocator

#: Stop-reason labels the CPA gradient loop can end on (PLANNER_DESIGN.md §5.8). Deterministic
#: given the same store + DAG + budgets, so the two builds report the same one bit-for-bit.
_CPA_BALANCED = "balanced"
_CPA_KNEE_EXHAUSTED = "knee-exhausted"
_CPA_CORE_CAPPED = "core-capped"
_CPA_MEM_CAPPED = "mem-capped"
_CPA_FIXED_POINT = "fixed-point"
_CPA_INFEASIBLE_FIXED_WIDTH = "infeasible-fixed-width"
_CPA_INFEASIBLE_MEMORY = "infeasible-memory"


class InfeasibleAllocationError(ValueError):
    """CPA cannot fit the seed allocation inside its core or memory budget."""

    def __init__(
        self,
        core_budget: int,
        fixed_widths: Sequence[tuple[str, int]] = (),
        *,
        mem_budget: int | None = None,
        memory_footprint: int | None = None,
    ) -> None:
        self.core_budget = max(1, core_budget)
        self.fixed_widths = tuple(fixed_widths)
        self.mem_budget = mem_budget
        self.memory_footprint = memory_footprint
        if mem_budget is not None and memory_footprint is not None:
            super().__init__(
                f"CPA allocation is infeasible under memory budget {mem_budget}: "
                f"minimum runnable footprint is {memory_footprint}"
            )
        else:
            detail = ", ".join(f"{tag}={width}" for tag, width in self.fixed_widths)
            super().__init__(
                f"CPA allocation is infeasible under core budget {self.core_budget}: "
                f"self-managed fixed width(s) exceed the budget: {detail}"
            )


def _infeasible_fixed_widths(
    cfg: DagConfig, widths: Mapping[str, int], core_budget: int
) -> tuple[tuple[str, int], ...]:
    """Sorted active self-managed widths above ``P`` in an allocation candidate."""

    budget = max(1, core_budget)
    return tuple(
        sorted(
            (step.tag, widths[step.tag])
            for step in cfg.steps
            if step.skip_reason is None
            and not step_width_is_resizable(step, cfg.default_jobs_flag, cfg.default_jobs_env)
            and widths[step.tag] > budget
        )
    )


def _speedup_within_budget(
    speedup: StepSpeedup, core_budget: int
) -> StepSpeedup | None:
    """Restrict a learned speedup model's recommendation to the run budget.

    Historical curve points above ``P`` remain visible, but neither the recommended width nor a
    regression marker may claim a width this run cannot execute. A curve with no measured point
    at or below ``P`` is unavailable for this run and is omitted from the plan.
    """
    budget = max(1, core_budget)
    measured = [level for level in speedup.levels if level.inner_jobs <= budget]
    if not measured:
        return None
    # Re-fit from the points this run can actually execute. Merely clamping a recommendation fitted
    # against wider points can select a local regression (for example T={1:100,2:50,4:70,8:10}
    # under P=4 would incorrectly recommend 4 instead of 2).
    aggregates = tuple(
        LevelAggregate(
            inner_jobs=level.inner_jobs,
            samples=level.samples,
            wall_s=level.wall_s,
            raw_wall_s=level.raw_wall_s,
            wall_min_s=level.wall_min_s,
            wall_max_s=level.wall_max_s,
            cpu_s=level.cpu_s,
            effective_cores=level.effective_cores,
            throttled_s=level.throttled_s,
            peak_bytes=level.peak_bytes,
            peak_samples=level.peak_samples,
            peak_floor_bytes=level.peak_floor_bytes,
            peak_floor_samples=level.peak_floor_samples,
        )
        for level in measured
    )
    fitted = _build_step_speedup(speedup.step, aggregates, None)
    # Keep above-budget measurements visible for diagnostics; only the recommendation/regression
    # and achieved-core marker are re-fitted to the executable subset.
    return replace(fitted, levels=speedup.levels)


def _cpu_work_eligible(level: SpeedupLevel, baseline_cpu: float | None) -> bool:
    """Whether one width satisfies the same CPU-work guard used by plateau fitting."""
    return (
        level.cpu_s is None
        or baseline_cpu is None
        or baseline_cpu <= 0.0
        or level.cpu_s / baseline_cpu <= _SPEEDUP_MAX_WORK_GROWTH
    )


def _cpa_admissible(
    cfg: DagConfig,
    speedups: Mapping[str, StepSpeedup],
    est: Mapping[str, float],
    core_budget: int,
) -> tuple[dict[str, list[int]], dict[str, dict[int, float]]]:
    """Per-step admissible width set ``W_i`` (ascending) and the MEASURED wall ``T_i(p)`` at each
    admissible width (PLANNER_DESIGN.md §5.2).

    A step with a measured curve admits its measured widths up to its work-conservation knee
    (``recommended_inner_jobs``) and the core budget ``P``; a curveless step is rigid at
    ``min(hint or default_step_cpu_count or 1, P)`` with ``T_i`` the resolved scalar estimate.
    ``T_i`` is only ever evaluated at these measured widths — no interpolation or extrapolation."""
    admissible: dict[str, list[int]] = {}
    wall: dict[str, dict[int, float]] = {}
    for step in cfg.steps:
        tag = step.tag
        if step.skip_reason is not None:
            admissible[tag] = [1]
            wall[tag] = {1: 0.0}
            continue
        if not step_width_is_resizable(step, cfg.default_jobs_flag, cfg.default_jobs_env):
            # An empty jobs_flag means the command manages its own fixed width. The planner cannot
            # safely act on a learned curve because changing preferred_inner_jobs would not change
            # the guest command. Run entry points refuse this configuration when the declared width
            # exceeds P. Keep the declared width intact so the public allocator and plan can
            # report an infeasible fixed-width configuration instead of inventing a smaller
            # command width.
            declared = step.hint.preferred_inner_jobs
            if declared is not None and declared > 0:
                width = declared
            else:
                effective = effective_cpu_count(step, cfg.default_step_cpu_count)
                width = min(
                    effective if (effective is not None and effective > 0) else 1,
                    max(1, core_budget),
                )
            admissible[tag] = [width]
            speedup = speedups.get(tag)
            exact = (
                next(
                    (level for level in speedup.levels if level.inner_jobs == width),
                    None,
                )
                if speedup is not None
                else None
            )
            wall[tag] = {width: exact.wall_s if exact is not None else est.get(tag, 0.0)}
            continue
        sp = speedups.get(tag)
        if sp is not None and sp.levels:
            baseline_cpu = sp.levels[0].cpu_s
            knee_ok = [
                lvl
                for lvl in sp.levels
                if lvl.inner_jobs <= sp.recommended_inner_jobs
                and _cpu_work_eligible(lvl, baseline_cpu)
            ]
            within_budget = [lvl for lvl in knee_ok if lvl.inner_jobs <= core_budget]
            if within_budget:
                admissible[tag] = sorted(lvl.inner_jobs for lvl in within_budget)
                wall[tag] = {lvl.inner_jobs: lvl.wall_s for lvl in within_budget}
            else:
                # A curve measured only above P cannot justify violating the strict run budget.
                # Treat it as unavailable and keep the step rigid at its effective configured
                # width, capped to P, using the resolved scalar estimate.
                effective = effective_cpu_count(step, cfg.default_step_cpu_count)
                width = effective if (effective is not None and effective > 0) else 1
                width = min(width, max(1, core_budget))
                admissible[tag] = [width]
                wall[tag] = {width: est.get(tag, 0.0)}
        else:
            effective = effective_cpu_count(step, cfg.default_step_cpu_count)
            w = effective if (effective is not None and effective > 0) else 1
            if core_budget > 0 and w > core_budget:
                w = core_budget
            if w < 1:
                w = 1
            admissible[tag] = [w]
            wall[tag] = {w: est.get(tag, 0.0)}
    return admissible, wall


def _cpa_next_width(widths: Sequence[int], current: int) -> int | None:
    """The next-larger admissible width after ``current`` in the ascending ``widths``, or ``None``
    when ``current`` is already the widest."""
    for w in widths:
        if w > current:
            return w
    return None


def _cpu_work_at(
    speedups: Mapping[str, StepSpeedup], tag: str, width: int, wall_s: float
) -> float:
    """Modeled CPU service ``C_i(p)`` for one allocated point.

    The boxed profile's user+system CPU seconds are the direct work-conservation measurement and
    therefore the right capacity term for a runtime whose width is a quota, not a reservation.
    Older/unboxed rows may lack it; ``p*T(p)`` remains a conservative fallback in that case.
    """
    speedup = speedups.get(tag)
    if speedup is not None:
        level = next((level for level in speedup.levels if level.inner_jobs == width), None)
        if level is not None and level.cpu_s is not None:
            return level.cpu_s
    return width * wall_s


def _memory_evidence_at(
    speedups: Mapping[str, StepSpeedup], tag: str, width: int
) -> tuple[int | None, int | None]:
    """Return ``(exact_M_p, censored_floor)`` for one measured width.

    ``exact_M_p`` exists only after the replication threshold and is raised by a censored floor.
    A floor without exact evidence remains separate: replacing the ordinary fallback by a smaller
    lower bound would turn incomplete evidence into an unsafe upper-bound claim.
    """
    speedup = speedups.get(tag)
    if speedup is None:
        return None, None
    level = next((point for point in speedup.levels if point.inner_jobs == width), None)
    if level is None:
        return None, None
    exact = (
        level.peak_bytes
        if level.peak_bytes is not None
        and level.peak_samples >= _MEMORY_MIN_SAMPLES_PER_LEVEL
        else None
    )
    if exact is not None and level.peak_floor_bytes is not None:
        exact = max(exact, level.peak_floor_bytes)
    return exact, level.peak_floor_bytes


def _modeled_memory_at(
    speedups: Mapping[str, StepSpeedup],
    tag: str,
    width: int,
    fallback: int | None,
) -> tuple[int | None, bool]:
    """The displayed/applied memory value and whether it is exact at ``width``.

    Replicated width-specific evidence is still ordinary profile feedback. It may raise the
    fallback, but only the explicit censor-aware memory-feedback mode may lower an authored floor.
    """
    exact, floor = _memory_evidence_at(speedups, tag, width)
    if exact is not None and (fallback is None or exact > fallback):
        return exact, True
    candidates = [value for value in (fallback, floor) if value is not None]
    return (max(candidates), False) if candidates else (None, False)


def _cpa_footprint(
    cfg: DagConfig,
    widths: Mapping[str, int],
    speedups: Mapping[str, StepSpeedup],
    max_steps: int,
) -> int:
    """Largest reachable concurrent footprint at the widths and active-step ceiling.

    A per-width measured ``memory.peak`` point may raise, but never lower, the existing baseline.
    When it raises, exact-width provenance prevents the legacy ``base * p/4`` placeholder from
    scaling an already-at-p measurement a second time. An authored hard cap still wins in
    :func:`step_mem_cap_bytes`. Dependency/resource constraints and ``max_steps`` bound the set that
    can actually overlap, matching the runtime memory admission model.
    """
    active: list[Step] = []
    for step in cfg.steps:
        if step.skip_reason is not None:
            # Keep the node in the dependency graph; schedulable_peak_mem_bytes excludes it from
            # candidates but transitive dependency discovery still needs every referenced tag.
            active.append(step)
            continue
        exact, floor = _memory_evidence_at(speedups, step.tag, widths[step.tag])
        if exact is None and floor is None:
            active.append(step)
            continue
        authored = step.hint.rss_baseline_bytes
        if exact is None or (authored is not None and exact <= authored):
            # A censored peak is only a lower bound. Raise the ordinary baseline to that floor but
            # retain fallback width scaling; the same applies when an exact profile peak is below
            # the authored floor. Marking either exact could shrink the cap through width
            # provenance even when the numeric baseline happened to be equal.
            baseline = max(authored or 0, floor or 0)
            active.append(
                replace(
                    step,
                    hint=replace(
                        step.hint,
                        rss_baseline_bytes=baseline,
                        rss_baseline_inner_jobs=None,
                    ),
                    deps=list(step.deps),
                    env=dict(step.env),
                )
            )
            continue
        active.append(
            replace(
                step,
                hint=replace(
                    step.hint,
                    rss_baseline_bytes=exact,
                    rss_baseline_inner_jobs=widths[step.tag],
                ),
                deps=list(step.deps),
                env=dict(step.env),
            )
        )
    active_cfg = replace(cfg, steps=tuple(active), resource_caps=dict(cfg.resource_caps))
    peak, _ = schedulable_peak_mem_bytes(active_cfg, max(1, max_steps), widths=widths)
    return _outer_mem_footprint_bytes(active_cfg, peak)


def _cpa_allocate_at_overlap(
    cfg: DagConfig,
    speedups: Mapping[str, StepSpeedup],
    admissible: Mapping[str, Sequence[int]],
    wall: Mapping[str, Mapping[int, float]],
    successors: Mapping[str, list[str]],
    core_budget: int,
    mem_budget: int | None,
    modeled_max_steps: int,
) -> tuple[dict[str, int], str] | None:
    """Run the CPA gradient at one FIXED active-step overlap.

    ``None`` means even the narrow seed cannot fit the memory budget at this overlap.  Keeping the
    overlap fixed inside this helper makes each result a comparable candidate for
    :func:`_cpa_allocate`; otherwise accepting the first feasible seed can strand CPA at a wide-
    concurrency, narrow-width local optimum.
    """
    P = max(1, core_budget)
    widths: dict[str, int] = {step.tag: admissible[step.tag][0] for step in cfg.steps}
    if mem_budget is not None and not _memory_footprint_fits(
        _cpa_footprint(cfg, widths, speedups, modeled_max_steps), mem_budget
    ):
        return None
    stop_reason = _CPA_FIXED_POINT
    # Each applied widening strictly increases some p_i within a finite W_i, so the loop is bounded
    # by Σ(|W_i|-1); +2 headroom lets the final balance/exhaustion check run.
    max_iters = sum(max(0, len(admissible[step.tag]) - 1) for step in cfg.steps) + 2
    for _ in range(max_iters):
        weight = {step.tag: wall[step.tag][widths[step.tag]] for step in cfg.steps}
        bottom = _bottom_levels(cfg, weight, successors)
        cp, t_cp = _critical_path(cfg, bottom, successors)
        area = sum(
            _cpu_work_at(speedups, step.tag, widths[step.tag], weight[step.tag])
            for step in cfg.steps
        )
        t_a = area / P
        if t_cp <= t_a:
            stop_reason = _CPA_BALANCED
            break
        cp_set = set(cp)
        widenable = [
            tag
            for tag in (s.tag for s in cfg.steps)
            if tag in cp_set and _cpa_next_width(admissible[tag], widths[tag]) is not None
        ]
        if not widenable:
            stop_reason = _CPA_KNEE_EXHAUSTED
            break
        best_tag: str | None = None
        best_gain = 0.0
        blocked_mem = False
        positive_candidate = False
        # Iterate tag-ascending and keep the FIRST maximum, so ties resolve to the smallest tag.
        for tag in sorted(widenable):
            nxt = _cpa_next_width(admissible[tag], widths[tag])
            assert nxt is not None
            if nxt > P:
                continue  # defensive per-step ceiling; admissible curves are already truncated
            cur = widths[tag]
            gain = (wall[tag][cur] - wall[tag][nxt]) / (nxt - cur)
            if gain <= 0.0:
                continue
            positive_candidate = True
            if mem_budget is not None:
                tentative = dict(widths)
                tentative[tag] = nxt
                if not _memory_footprint_fits(
                    _cpa_footprint(cfg, tentative, speedups, modeled_max_steps), mem_budget
                ):
                    blocked_mem = True
                    continue
            if best_tag is None or gain > best_gain:
                best_tag, best_gain = tag, gain
        if best_tag is None:
            if blocked_mem and positive_candidate:
                stop_reason = _CPA_MEM_CAPPED
            else:
                stop_reason = _CPA_KNEE_EXHAUSTED
            break
        nxt = _cpa_next_width(admissible[best_tag], widths[best_tag])
        assert nxt is not None
        widths[best_tag] = nxt
    return widths, stop_reason


def _cpa_candidate_makespan(
    cfg: DagConfig,
    widths: Mapping[str, int],
    wall: Mapping[str, Mapping[int, float]],
    successors: Mapping[str, list[str]],
    core_budget: int,
    modeled_max_steps: int,
) -> float:
    """Score one fixed-overlap allocation with the plan's deterministic reference schedule."""
    weight = {step.tag: wall[step.tag][widths[step.tag]] for step in cfg.steps}
    bottom = _bottom_levels(cfg, weight, successors)
    order = _plan_order(cfg, Planner.CRITICAL_PATH, weight, bottom)
    return _cpa_simulate_makespan(
        cfg, widths, weight, order, max(1, core_budget), modeled_max_steps
    )


def _cpa_allocate(
    cfg: DagConfig,
    speedups: Mapping[str, StepSpeedup],
    est: Mapping[str, float],
    core_budget: int,
    mem_budget: int | None,
    max_steps: int,
) -> tuple[dict[str, int], dict[str, list[int]], dict[str, dict[int, float]], str, int]:
    """Choose the best fixed-overlap CPA allocation up to the active-step ceiling.

    For each feasible ``modeled_max_steps`` value, run the ordinary monotone CPA gradient with
    that overlap fixed, then score its deterministic no-overcommit reference schedule.  The least
    modeled makespan wins; iteration from the ceiling down and replacement only on a STRICT
    improvement retain the larger overlap on an exact tie.  This joint search avoids the local
    optimum where a barely feasible wide-overlap seed prevents useful widths that would finish
    sooner with fewer concurrent steps.

    Returns ``(widths, admissible, wall, stop_reason, modeled_max_steps)`` — deterministic across
    builds (identical float ops in cfg order, integer core arithmetic, total tag tie-break)."""
    P = max(1, core_budget)
    admissible, wall = _cpa_admissible(cfg, speedups, est, P)
    seed: dict[str, int] = {step.tag: admissible[step.tag][0] for step in cfg.steps}
    successors = _successors(cfg)
    ceiling = max(1, max_steps)
    if _infeasible_fixed_widths(cfg, seed, P):
        return seed, admissible, wall, _CPA_INFEASIBLE_FIXED_WIDTH, ceiling

    # Without a memory constraint the gradient result is independent of modeled overlap, and the
    # reference makespan cannot improve by reducing its concurrency ceiling. Avoid repeating the
    # identical allocation for every value (the usual default ceiling is the machine width).
    overlaps = (ceiling,) if mem_budget is None else range(ceiling, 0, -1)
    best: tuple[dict[str, int], str, int] | None = None
    best_makespan = math.inf
    for modeled_max_steps in overlaps:
        candidate = _cpa_allocate_at_overlap(
            cfg,
            speedups,
            admissible,
            wall,
            successors,
            P,
            mem_budget,
            modeled_max_steps,
        )
        if candidate is None:
            continue
        widths, stop_reason = candidate
        makespan = _cpa_candidate_makespan(
            cfg, widths, wall, successors, P, modeled_max_steps
        )
        if best is None or makespan < best_makespan:
            best = widths, stop_reason, modeled_max_steps
            best_makespan = makespan

    if best is None:
        # Footprint is monotone in the overlap ceiling, so no feasible candidate means a single
        # runnable step at its narrowest admissible width exceeds the memory budget.
        return seed, admissible, wall, _CPA_INFEASIBLE_MEMORY, 1
    widths, stop_reason, modeled_max_steps = best
    return widths, admissible, wall, stop_reason, modeled_max_steps


def allocate_widths(
    cfg: DagConfig,
    speedups: Mapping[str, StepSpeedup],
    est: Mapping[str, float],
    core_budget: int,
    mem_budget: int | None = None,
    max_steps: int | None = None,
) -> dict[str, int]:
    """The pure CPA allocator: pick each step's inner-jobs width to balance the critical path
    against the per-core area over the measured speedup curves, subject to the core budget ``P``
    and (optionally) the RAM budget.

    Raises :class:`InfeasibleAllocationError` when the minimum allocation exceeds either budget;
    unlike :func:`build_plan`, this low-level API has no plan summary in which to carry an
    infeasibility stop reason. See PLANNER_DESIGN.md and :func:`_cpa_allocate`.
    """
    active_budget = max(1, max_steps if max_steps is not None else max(1, core_budget))
    widths, _admissible, _wall, reason, _modeled_max_steps = _cpa_allocate(
        cfg, speedups, est, core_budget, mem_budget, active_budget
    )
    if reason == _CPA_INFEASIBLE_FIXED_WIDTH:
        raise InfeasibleAllocationError(
            core_budget, _infeasible_fixed_widths(cfg, widths, core_budget)
        )
    if reason == _CPA_INFEASIBLE_MEMORY:
        assert mem_budget is not None
        raise InfeasibleAllocationError(
            core_budget,
            mem_budget=mem_budget,
            memory_footprint=_cpa_footprint(cfg, widths, speedups, 1),
        )
    return widths


def _cpa_simulate_makespan(
    cfg: DagConfig,
    widths: Mapping[str, int],
    weight: Mapping[str, float],
    order: Sequence[str],
    core_budget: int,
    max_steps: int,
) -> float:
    """A deterministic no-overcommit reference schedule of the allocated widths.

    Launches ready steps (deps done, reference capacity ``Σ running widths + p_i <= P``, named resources
    free) in ``order`` (critical-path first), advancing to the next finish event. Allocated widths
    are required to lie in ``1..=P``; there is no over-budget run-alone escape. Respecting deps AND
    the reference capacity makes the result ``>= max(T_CP, area/P)`` (PLANNER_DESIGN.md §2). The
    live scheduler intentionally permits wider aggregate overlap. Uses the same f64 ops in canonical
    ``order`` in both builds, so the 3-decimal makespan is byte-identical."""
    P = core_budget if core_budget > 0 else 1
    by_tag = cfg.by_tag()
    done: dict[str, float] = {}
    running: dict[str, float] = {}
    res_avail: dict[str, int] = dict(cfg.resource_caps)
    cores_used = 0
    now = 0.0
    pending: set[str] = {step.tag for step in cfg.steps}
    if any(widths[tag] < 1 or widths[tag] > P for tag in pending):
        raise ValueError("CPA width lies outside the total CPU budget")
    while pending or running:
        launched = True
        while launched:
            launched = False
            for tag in order:
                if tag not in pending:
                    continue
                if len(running) >= max(1, max_steps):
                    break
                step = by_tag[tag]
                if not all(d in done for d in step.deps):
                    continue
                w = widths[tag]
                if cores_used + w > P:
                    continue
                if any(res_avail.get(r, 0) < n for r, n in step.hint.resources.items()):
                    continue
                running[tag] = now + weight[tag]
                pending.discard(tag)
                cores_used += w
                for r, n in step.hint.resources.items():
                    res_avail[r] = res_avail.get(r, 0) - n
                launched = True
        if not running:
            break  # nothing else can be scheduled (unsatisfiable resource demand)
        finish = min(running.values())
        now = finish
        for tag in order:
            if tag in running and running[tag] == finish:
                done[tag] = finish
                del running[tag]
                cores_used -= widths[tag]
                for r, n in by_tag[tag].hint.resources.items():
                    res_avail[r] = res_avail.get(r, 0) + n
    return max(done.values()) if done else 0.0


def _build_cpa_plan(
    cfg: DagConfig,
    resolved: Mapping[str, tuple[float, str, int | None, str, int]],
    est: Mapping[str, float],
    speedups: Mapping[str, StepSpeedup],
    successors: Mapping[str, list[str]],
    core_budget: int | None,
    mem_budget: int | None,
    max_steps: int | None,
) -> Plan:
    """Two-phase CPA plan: allocate widths (phase 1), then critical-path list-schedule at the
    allocated weights ``T_i(p_i)`` (phase 2). See PLANNER_DESIGN.md §4."""
    P = core_budget if (core_budget is not None and core_budget > 0) else 1
    active_budget = max(1, max_steps if max_steps is not None else P)
    bounded_speedups: dict[str, StepSpeedup] = {}
    by_tag = cfg.by_tag()
    for tag, speedup in speedups.items():
        bounded = _speedup_within_budget(speedup, P)
        if bounded is not None:
            bounded_speedups[tag] = bounded
        elif tag in by_tag and not step_width_is_resizable(
            by_tag[tag], cfg.default_jobs_flag, cfg.default_jobs_env
        ):
            # A self-managed infeasible step may have measurements only above P. Keep that curve
            # diagnostic and use its exact fixed-width level if present; no allocation is applied.
            bounded_speedups[tag] = speedup
    # CPA's memory constraint must see the same learned RSS values that execution and ordinary
    # --max-mem sizing will see after plan application. Duration resolution already feeds `est`;
    # clone only the memory hints here so width allocation cannot approve against stale authored
    # baselines and then install a larger store estimate afterward.
    memory_steps: list[Step] = []
    for step in cfg.steps:
        r = resolved[step.tag]
        rss = r[2] if r[3] == "store" else step.hint.rss_baseline_bytes
        memory_steps.append(
            replace(
                step,
                hint=replace(
                    step.hint,
                    rss_baseline_bytes=rss,
                    rss_baseline_inner_jobs=None,
                ),
                deps=list(step.deps),
                env=dict(step.env),
            )
        )
    memory_cfg = replace(
        cfg, steps=tuple(memory_steps), resource_caps=dict(cfg.resource_caps)
    )
    widths, _admissible, wall, stop_reason, modeled_max_steps = _cpa_allocate(
        memory_cfg, bounded_speedups, est, P, mem_budget, active_budget
    )
    weight = {step.tag: wall[step.tag][widths[step.tag]] for step in cfg.steps}
    bottom = _bottom_levels(cfg, weight, successors)
    critical, t_cp = _critical_path(cfg, bottom, successors)
    order = _plan_order(cfg, Planner.CRITICAL_PATH, weight, bottom)
    area = sum(
        _cpu_work_at(bounded_speedups, step.tag, widths[step.tag], weight[step.tag])
        for step in cfg.steps
    )
    t_a = area / P
    lower_bound = t_cp if t_cp >= t_a else t_a
    modeled = (
        math.inf
        if stop_reason in {_CPA_INFEASIBLE_FIXED_WIDTH, _CPA_INFEASIBLE_MEMORY}
        else _cpa_simulate_makespan(cfg, widths, weight, order, P, modeled_max_steps)
    )
    allocation = Allocation(
        core_budget=P,
        area_s=area,
        area_bound_s=t_a,
        critical_path_s=t_cp,
        lower_bound_s=lower_bound,
        modeled_makespan_s=modeled,
        stop_reason=stop_reason,
        modeled_max_steps=modeled_max_steps,
    )
    entries: list[PlanEntry] = []
    for step in cfg.steps:
        tag = step.tag
        r = resolved[tag]
        entry_speedup = bounded_speedups.get(tag)
        curve_level = (
            next(
                (level for level in entry_speedup.levels if level.inner_jobs == widths[tag]),
                None,
            )
            if step.skip_reason is None and entry_speedup is not None
            else None
        )
        uses_curve = curve_level is not None
        modeled_rss, rss_is_exact = _modeled_memory_at(
            bounded_speedups, tag, widths[tag], r[2]
        )
        _exact, floor = _memory_evidence_at(bounded_speedups, tag, widths[tag])
        modeled_rss_source = "store" if rss_is_exact or floor is not None else r[3]
        entries.append(
            PlanEntry(
                tag=tag,
                est_duration_s=weight[tag],
                est_source="store" if uses_curve else r[1],
                rss_estimate_bytes=modeled_rss,
                rss_source=modeled_rss_source,
                rss_estimate_inner_jobs=(widths[tag] if rss_is_exact else None),
                bottom_level_s=bottom[tag],
                samples=curve_level.samples if curve_level is not None else r[4],
                speedup=None if step.skip_reason is not None else bounded_speedups.get(tag),
                # An empty effective jobs flag opts out of command rewriting. CPA still charges
                # the fixed width in its schedule model, but must not publish an allocation that
                # apply_plan_to_config could mistake for an executable guest-width rewrite.
                alloc_inner_jobs=(
                    widths[tag]
                    if step.skip_reason is None
                    and stop_reason != _CPA_INFEASIBLE_MEMORY
                    and step_width_is_resizable(
                        step, cfg.default_jobs_flag, cfg.default_jobs_env
                    )
                    else None
                ),
            )
        )
    return Plan(
        planner=Planner.CPA,
        order=tuple(order),
        critical_path=tuple(critical),
        critical_path_length_s=t_cp,
        entries=tuple(entries),
        allocation=allocation,
    )


def build_plan(
    cfg: DagConfig,
    store_samples: Mapping[str, StepSamples],
    *,
    planner: Planner = Planner.GREEDY_LPT,
    min_samples: int = DEFAULT_MIN_SAMPLES,
    speedups: Mapping[str, StepSpeedup] | None = None,
    core_budget: int | None = None,
    mem_budget: int | None = None,
    max_steps: int | None = None,
) -> Plan:
    """Resolve every step's estimate and build the plan for ``planner``: duration uses
    store-over-hint-over-default, while authored RSS is a floor; the result contains the per-step
    estimates, critical path, and dispatch order.

    ``speedups`` (from :func:`load_step_speedups`) attaches each step's learned parallel-speedup
    curve for the plan display. For ``planner=Planner.CPA`` it also DRIVES runner-controlled width
    allocation over those curves; self-managed commands remain fixed. The dispatch order is the
    critical-path order at the modeled weights ``T_i(p_i)``, and a whole-DAG :class:`Allocation`
    summary (widths, lower bound, modeled makespan, stop reason) is attached.
    ``core_budget`` (``P``, from :func:`profile_enrich.container_core_budget`) bounds every
    attached speedup recommendation so the plan never recommends a per-step width the run cannot
    execute. Under CPA it also bounds executable width allocation; an over-budget self-managed
    fixed width is reported as infeasible instead. ``mem_budget`` (the ``--max-mem`` RAM budget)
    applies only to CPA allocation.
    """
    validate_jobs_env_config(cfg)
    validate_cmdtype_config(cfg)
    speedups = speedups or {}
    resolved: dict[str, tuple[float, str, int | None, str, int]] = {}
    est: dict[str, float] = {}
    for step in cfg.steps:
        r = _resolved_estimate(step, store_samples.get(step.tag), min_samples)
        resolved[step.tag] = r
        est[step.tag] = r[0]
    successors = _successors(cfg)
    if planner is Planner.CPA:
        return _build_cpa_plan(
            cfg,
            resolved,
            est,
            speedups,
            successors,
            core_budget,
            mem_budget,
            max_steps,
        )
    display_speedups = speedups
    if core_budget is not None:
        display_speedups = {
            tag: bounded
            for tag, speedup in speedups.items()
            if (bounded := _speedup_within_budget(speedup, core_budget)) is not None
        }
    level_by_tag: dict[str, SpeedupLevel] = {}
    modeled_est = dict(est)
    for step in cfg.steps:
        speedup = display_speedups.get(step.tag)
        effective = effective_cpu_count(step, cfg.default_step_cpu_count)
        if speedup is None or effective is None:
            continue
        level = next(
            (point for point in speedup.levels if point.inner_jobs == effective), None
        )
        if level is not None:
            level_by_tag[step.tag] = level
            modeled_est[step.tag] = level.wall_s
    bottom = _bottom_levels(cfg, modeled_est, successors)
    critical, length = _critical_path(cfg, bottom, successors)
    order = _plan_order(cfg, planner, modeled_est, bottom)
    entries_list: list[PlanEntry] = []
    for step in cfg.steps:
        r = resolved[step.tag]
        speedup = None if step.skip_reason is not None else display_speedups.get(step.tag)
        level = level_by_tag.get(step.tag)
        effective = effective_cpu_count(step, cfg.default_step_cpu_count)
        width_memory, rss_is_exact = (
            _modeled_memory_at(display_speedups, step.tag, effective, r[2])
            if effective is not None
            else (r[2], False)
        )
        _exact, floor = (
            _memory_evidence_at(display_speedups, step.tag, effective)
            if effective is not None
            else (None, None)
        )
        entries_list.append(
            PlanEntry(
                tag=step.tag,
                est_duration_s=level.wall_s if level is not None else r[0],
                est_source="store" if level is not None else r[1],
                rss_estimate_bytes=width_memory,
                rss_source="store" if rss_is_exact or floor is not None else r[3],
                rss_estimate_inner_jobs=(
                    effective if rss_is_exact else None
                ),
                bottom_level_s=bottom[step.tag],
                samples=level.samples if level is not None else r[4],
                speedup=speedup,
            )
        )
    entries = tuple(entries_list)
    return Plan(
        planner=planner,
        order=tuple(order),
        critical_path=tuple(critical),
        critical_path_length_s=length,
        entries=entries,
    )


def apply_plan_to_config(cfg: DagConfig, plan: Plan) -> DagConfig:
    """Return a copy of ``cfg`` whose per-step hints carry the plan's resolved estimates.

    ``est_duration_s`` is set for every step (store value when the store won, else the unchanged
    hint value), and ``rss_baseline_bytes`` is overridden ONLY when the store won (so a hint-only
    or unmeasured baseline is preserved). This is what feeds both the scheduler's ordering and the
    memory-aware ``--max-mem`` sizing, so planning improves automatically as runs accumulate.

    For a CPA plan, a runner-controlled step also gets
    ``preferred_inner_jobs = alloc_inner_jobs`` — the inner width the allocator chose — so its
    jobs flag carries that width (via :func:`model.command_with_inner_jobs`) and its memory cap
    scales accordingly. A step with an empty/whitespace-only effective jobs flag keeps its
    declared fixed width: the planner cannot rewrite that guest command, and preserving the hint
    ensures run-budget validation can still refuse an over-budget command. Run-level ``-j``
    remains the total CPU budget."""
    if (
        plan.allocation is not None
        and plan.allocation.stop_reason == _CPA_INFEASIBLE_MEMORY
    ):
        return cfg
    by_tag = plan.by_tag()
    new_steps: list[Step] = []
    for step in cfg.steps:
        entry = by_tag.get(step.tag)
        if entry is None:
            new_steps.append(step)
            continue
        if step.skip_reason is not None:
            # Planning gives an intentional skip zero executable demand, but applying a plan must
            # not erase its authored hints in case a caller later reclassifies/reuses the graph.
            new_steps.append(step)
            continue
        rss = (
            entry.rss_estimate_bytes
            if entry.rss_source == "store"
            else step.hint.rss_baseline_bytes
        )
        inner = step.hint.preferred_inner_jobs
        if step_width_is_resizable(step, cfg.default_jobs_flag, cfg.default_jobs_env):
            inner = (
                entry.alloc_inner_jobs
                if entry.alloc_inner_jobs is not None
                else step.hint.preferred_inner_jobs
            )
        new_hint = ResourceHint(
            resources=step.hint.resources,
            est_duration_s=entry.est_duration_s,
            rss_baseline_bytes=rss,
            rss_baseline_inner_jobs=(
                entry.rss_estimate_inner_jobs if entry.rss_source == "store" else None
            ),
            hard_mem_max_bytes=step.hint.hard_mem_max_bytes,
            classification=step.hint.classification,
            preferred_inner_jobs=inner,
            measured_effective_cores=step.hint.measured_effective_cores,
            measured_cpu_utilization=step.hint.measured_cpu_utilization,
        )
        # Clone-and-override, mirroring the Rust build's `let mut s = step.clone(); s.hint = …`.
        # Only `hint` (and its defensive copies of the mutable containers) is replaced; EVERY other
        # field is carried verbatim. A prior field-by-field rebuild silently dropped `cpu_timeout`
        # (defaulting it to 0), disabling the per-step CPU-time guard on every planned run while the
        # Rust build kept enforcing it — a divergence that behavioral cross-checks catch only where
        # cgroups exist. `replace` makes it structurally impossible for a newly added Step field to
        # go missing here again.
        new_steps.append(
            replace(step, hint=new_hint, deps=list(step.deps), env=dict(step.env))
        )
    # Carry the top-level policy BY CONSTRUCTION. A field-by-field reconstruction silently resets
    # every newly added DagConfig field; in particular it used to discard default_step_cpu_count
    # immediately before the run-budget clamp was applied. `with_steps` is the safe spelling
    # (#21 scarce-resource-deadlock), and this is one of the two places in the product that
    # rebuilds a DagConfig around a new step list.
    return cfg.with_steps(new_steps)


# --------------------------------------------------------------------------- rendering


def _fmt_secs(value: float) -> str:
    """Fixed 3-decimal seconds, byte-identical to the Rust ``format!(\"{:.3}\")``."""
    return f"{value:.3f}"


def _opt_secs_json(value: float | None) -> str:
    """JSON value for an optional fixed-3-decimal number: ``null`` or a quoted string (so parity
    does not depend on float ``repr``)."""
    return "null" if value is None else f'"{_fmt_secs(value)}"'


def _opt_int_json(value: int | None) -> str:
    """An optional integer as a JSON literal: the number, or ``null``."""
    return "null" if value is None else str(value)


def _speedup_level_json(level: SpeedupLevel, baseline_inner_jobs: int) -> str:
    """One speedup-curve level as a single-line JSON object (byte-identical across builds)."""
    width_ratio = level.inner_jobs / max(1, baseline_inner_jobs)
    parallel_efficiency = level.speedup / width_ratio if width_ratio > 0.0 else 0.0
    return (
        f'{{"inner_jobs": {level.inner_jobs}, "wall_s": "{_fmt_secs(level.wall_s)}", '
        f'"raw_wall_s": {_opt_secs_json(level.raw_wall_s)}, '
        f'"wall_min_s": {_opt_secs_json(level.wall_min_s)}, '
        f'"wall_max_s": {_opt_secs_json(level.wall_max_s)}, '
        f'"speedup": "{_fmt_secs(level.speedup)}", "cpu_s": {_opt_secs_json(level.cpu_s)}, '
        f'"effective_cores": {_opt_secs_json(level.effective_cores)}, '
        f'"throttled_s": {_opt_secs_json(level.throttled_s)}, '
        f'"peak_bytes": {_opt_int_json(level.peak_bytes)}, '
        f'"peak_samples": {level.peak_samples}, '
        f'"peak_floor_bytes": {_opt_int_json(level.peak_floor_bytes)}, '
        f'"peak_floor_samples": {level.peak_floor_samples}, '
        f'"parallel_efficiency": "{_fmt_secs(parallel_efficiency)}", '
        f'"samples": {level.samples}}}'
    )


def _speedup_to_json(speedup: "StepSpeedup | None") -> str:
    """The ``"speedup"`` field value for a step in the plan JSON: ``null`` or a nested object with
    the recommended width, achieved cores, and the full measured curve. Indented to embed after
    ``\"speedup\": `` at the step object's 6-space field indent."""
    if speedup is None:
        return "null"
    levels = ",\n".join(
        f"          {_speedup_level_json(level, speedup.baseline_inner_jobs)}"
        for level in speedup.levels
    )
    return (
        "{\n"
        f'        "baseline_inner_jobs": {speedup.baseline_inner_jobs},\n'
        f'        "recommended_inner_jobs": {speedup.recommended_inner_jobs},\n'
        f'        "measured_effective_cores": {_opt_secs_json(speedup.measured_effective_cores)},\n'
        f'        "regression_inner_jobs": {_opt_int_json(speedup.regression_inner_jobs)},\n'
        '        "levels": [\n'
        f"{levels}\n"
        "        ]\n"
        "      }"
    )


def scaling_model_path(
    profile_dir: str | Path, machine_id: str, container_class: str
) -> Path:
    """Path of the rebuildable, machine/container-specific scaling-model sidecar."""
    return Path(profile_dir) / f"scaling_model_{machine_id}_{container_class}.json"


def scaling_model_to_json(
    machine_id: str,
    container_class: str,
    speedups: Mapping[str, StepSpeedup],
    workload_digests: Mapping[str, str] | None = None,
) -> str:
    """Serialize the fitted empirical scaling model as deterministic JSON.

    The authored DAG remains portable policy. This sidecar is an inspectable, rebuildable cache of
    the machine-specific model derived from the raw CSV/portable summary; planners continue to use
    the same fitting functions, so the saved explanation and live decisions cannot drift.
    """
    steps: list[str] = []
    workloads = workload_digests or {}
    for tag in sorted(speedups):
        speedup = speedups[tag]
        levels = ",\n".join(
            f"        {_speedup_level_json(level, speedup.baseline_inner_jobs)}"
            for level in speedup.levels
        )
        steps.append(
            "    {\n"
            f'      "step": {_json_str(tag)},\n'
            f'      "workload_digest": {_json_str(workloads.get(tag, ""))},\n'
            f'      "baseline_inner_jobs": {speedup.baseline_inner_jobs},\n'
            f'      "recommended_inner_jobs": {speedup.recommended_inner_jobs},\n'
            f'      "regression_inner_jobs": {_opt_int_json(speedup.regression_inner_jobs)},\n'
            '      "levels": [\n'
            f"{levels}\n"
            "      ]\n"
            "    }"
        )
    body = ",\n".join(steps)
    return (
        "{\n"
        '  "schema": 2,\n'
        f'  "machine_id": {_json_str(machine_id)},\n'
        f'  "container_class": {_json_str(container_class)},\n'
        '  "plateau_wall_tolerance": "0.100",\n'
        f'  "max_cpu_work_growth": "{_fmt_secs(_SPEEDUP_MAX_WORK_GROWTH)}",\n'
        f'  "memory_min_samples_per_width": {_MEMORY_MIN_SAMPLES_PER_LEVEL},\n'
        '  "steps": [\n'
        f"{body}\n"
        "  ]\n"
        "}\n"
    )


def write_scaling_model(
    profile_dir: str | Path,
    machine_id: str,
    container_class: str,
    speedups: Mapping[str, StepSpeedup],
    workload_digests: Mapping[str, str] | None = None,
) -> Path:
    """Atomically refresh the derived scaling-model sidecar and return its path."""
    path = scaling_model_path(profile_dir, machine_id, container_class)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(
        scaling_model_to_json(
            machine_id, container_class, speedups, workload_digests
        ),
        encoding="utf-8",
    )
    os.replace(temporary, path)
    return path


def _allocation_to_json(alloc: "Allocation | None") -> str:
    """The ``"allocation"`` field value in the plan JSON: ``null`` (non-CPA planners) or a nested
    object with the core budget, area / critical-path terms, makespan lower bound + modeled
    makespan, and the stop reason. Floats are fixed-3-decimal STRINGS like the rest of the plan."""
    if alloc is None:
        return "null"
    return (
        "{\n"
        f'    "stop_reason": {_json_str(alloc.stop_reason)},\n'
        f'    "core_budget": {alloc.core_budget},\n'
        f'    "modeled_max_steps": {alloc.modeled_max_steps},\n'
        f'    "area_s": "{_fmt_secs(alloc.area_s)}",\n'
        f'    "area_bound_s": "{_fmt_secs(alloc.area_bound_s)}",\n'
        f'    "critical_path_s": "{_fmt_secs(alloc.critical_path_s)}",\n'
        f'    "lower_bound_s": "{_fmt_secs(alloc.lower_bound_s)}",\n'
        f'    "modeled_makespan_s": "{_fmt_secs(alloc.modeled_makespan_s)}"\n'
        "  }"
    )


def plan_to_json(plan: Plan) -> str:
    """Return canonical, machine-readable plan JSON with two-space indentation.

    Computed floats are emitted as fixed-three-decimal strings rather than JSON numbers, so output
    does not depend on platform float representation. Steps are listed in
    dispatch order. The top-level ``allocation`` object is populated only under
    ``--planner cpa``. Per-step ``alloc_inner_jobs`` is populated only for feasible,
    runner-controlled CPA steps; ordering-only, self-managed, and intentionally skipped steps use
    ``null``."""
    by_tag = plan.by_tag()
    steps_json: list[str] = []
    for tag in plan.order:
        entry = by_tag[tag]
        rss = "null" if entry.rss_estimate_bytes is None else str(entry.rss_estimate_bytes)
        alloc = "null" if entry.alloc_inner_jobs is None else str(entry.alloc_inner_jobs)
        steps_json.append(
            "    {\n"
            f'      "tag": {_json_str(entry.tag)},\n'
            f'      "est_duration_s": "{_fmt_secs(entry.est_duration_s)}",\n'
            f'      "est_source": {_json_str(entry.est_source)},\n'
            f'      "rss_estimate_bytes": {rss},\n'
            f'      "rss_source": {_json_str(entry.rss_source)},\n'
            f'      "bottom_level_s": "{_fmt_secs(entry.bottom_level_s)}",\n'
            f'      "samples": {entry.samples},\n'
            f'      "alloc_inner_jobs": {alloc},\n'
            f'      "speedup": {_speedup_to_json(entry.speedup)}\n'
            "    }"
        )
    parts = [
        "{",
        f'  "planner": {_json_str(plan.planner.value)},',
        f'  "critical_path": {_json_str_list(plan.critical_path)},',
        f'  "critical_path_length_s": "{_fmt_secs(plan.critical_path_length_s)}",',
        f'  "order": {_json_str_list(plan.order)},',
        f'  "allocation": {_allocation_to_json(plan.allocation)},',
    ]
    if steps_json:
        parts.append('  "steps": [')
        parts.append(",\n".join(steps_json))
        parts.append("  ]")
    else:
        parts.append('  "steps": []')
    parts.append("}")
    return "\n".join(parts)


def _json_str(value: str) -> str:
    """JSON-quote/escape a string exactly like the Rust ``json_str`` (ASCII control set)."""
    out = ['"']
    for ch in value:
        code = ord(ch)
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\b":
            out.append("\\b")
        elif ch == "\f":
            out.append("\\f")
        elif code < 0x20:
            out.append(f"\\u{code:04x}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def _json_str_list(items: Sequence[str]) -> str:
    """A JSON string array formatted like ``json.dumps(indent=2)`` at 2-space base indent."""
    if not items:
        return "[]"
    inner = ",\n".join(f"    {_json_str(item)}" for item in items)
    return "[\n" + inner + "\n  ]"


def _human_bytes(n: int | None) -> str:
    if n is None:
        return "-"
    value = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024.0 or unit == "TiB":
            return f"{int(value)} B" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024.0
    return f"{n} B"


def plan_to_text(plan: Plan) -> str:
    """A compact, human-readable plan for the terminal: the planner, a per-step estimate table
    (with the SOURCE of each estimate), the critical path, and the scheduled order.

    Deterministic (no color) so it is also cross-checkable; the CLI wraps it in optional color."""
    by_tag = plan.by_tag()
    is_cpa = plan.allocation is not None
    headers = [
        "step",
        "est_duration_s",
        "source",
        "rss_estimate",
        "rss_source",
        "bottom_level_s",
        "samples",
    ]
    if is_cpa:
        headers.append("alloc_inner_jobs")
    rows: list[list[str]] = []
    for tag in plan.order:
        entry = by_tag[tag]
        row = [
            tag,
            _fmt_secs(entry.est_duration_s),
            entry.est_source,
            _human_bytes(entry.rss_estimate_bytes),
            entry.rss_source,
            _fmt_secs(entry.bottom_level_s),
            str(entry.samples),
        ]
        if is_cpa:
            row.append("-" if entry.alloc_inner_jobs is None else str(entry.alloc_inner_jobs))
        rows.append(row)
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    def fmt(cells: Sequence[str]) -> str:
        return "  ".join(
            f"{cells[i]:<{widths[i]}}" if i == 0 else f"{cells[i]:>{widths[i]}}"
            for i in range(len(headers))
        )

    lines = [
        f"plan: {plan.planner.value}",
        "per-step estimates (source: store = learned from the profile store; "
        "hint = DAG-authored; default = none; skip = intentional pre-execution skip):",
        fmt(headers),
        "  ".join("-" * w for w in widths),
    ]
    lines.extend(fmt(row) for row in rows)
    crit = " -> ".join(plan.critical_path) if plan.critical_path else "(none)"
    lines.append(f"critical path ({_fmt_secs(plan.critical_path_length_s)}s): {crit}")
    lines.append("scheduled order: " + (", ".join(plan.order) if plan.order else "(none)"))
    lines.extend(_allocation_text_lines(plan))
    lines.extend(_speedup_text_lines(plan))
    return "\n".join(lines) + "\n"


def _allocation_text_lines(plan: Plan) -> list[str]:
    """The one-line CPA allocator summary for :func:`plan_to_text` (``--planner cpa`` only): the
    stop reason, the core budget, and the balancing terms — the critical path vs. per-core area, the
    makespan lower bound, and the modeled makespan. Empty for the non-allocating planners."""
    alloc = plan.allocation
    if alloc is None:
        return []
    return [
        f"allocator (cpa): {alloc.stop_reason}; P={alloc.core_budget} cores; "
        f"modeled-max-steps={alloc.modeled_max_steps}; "
        f"critical-path={_fmt_secs(alloc.critical_path_s)}s, "
        f"area/P={_fmt_secs(alloc.area_bound_s)}s, "
        f"lower-bound={_fmt_secs(alloc.lower_bound_s)}s, "
        f"no-overcommit-model={_fmt_secs(alloc.modeled_makespan_s)}s"
    ]


def _speedup_text_lines(plan: Plan) -> list[str]:
    """The optional parallel-speedup section for :func:`plan_to_text`: one row per step that HAS a
    learned curve (>=2 inner_jobs widths).

    Two columns exist to keep a modelled number from reading as a measurement. ``regress_at`` names
    the width where going wider becomes measurably slower, so a cliff is distinguishable from a
    plateau at a glance. ``wall@rec discounted/raw`` prints the contention-discounted wall the curve
    was fitted to NEXT TO the raw measured wall it came from, so the size of the adjustment is
    visible in the output rather than something the reader has to know about.

    Empty (no lines) when no step has a model, so a store without multi-width samples renders
    exactly as before."""
    by_tag = plan.by_tag()
    modeled = [(tag, by_tag[tag].speedup) for tag in plan.order if by_tag[tag].speedup is not None]
    if not modeled:
        return []
    headers = [
        "step",
        "rec_inner_jobs",
        "regress_at",
        "eff_cores",
        "speedup@rec",
        "par_eff@rec",
        "cpu_growth@rec",
        "memory@rec",
        "wall@rec discounted/raw",
        "curve(inner_jobs->speedup)",
    ]
    rows: list[list[str]] = []
    for tag, speedup in modeled:
        assert speedup is not None  # narrowed by the filter above (for the type checker)
        knee = next(
            (lvl.speedup for lvl in speedup.levels if lvl.inner_jobs == speedup.recommended_inner_jobs),
            1.0,
        )
        eff = (
            f"{speedup.measured_effective_cores:.3f}"
            if speedup.measured_effective_cores is not None
            else "-"
        )
        curve = " ".join(f"{lvl.inner_jobs}:{lvl.speedup:.2f}x" for lvl in speedup.levels)
        at_rec = next(
            (lvl for lvl in speedup.levels if lvl.inner_jobs == speedup.recommended_inner_jobs),
            None,
        )
        baseline = speedup.levels[0]
        par_eff = (
            at_rec.speedup / (at_rec.inner_jobs / speedup.baseline_inner_jobs)
            if at_rec is not None and at_rec.inner_jobs > 0
            else 0.0
        )
        cpu_growth = (
            at_rec.cpu_s / baseline.cpu_s
            if at_rec is not None
            and at_rec.cpu_s is not None
            and baseline.cpu_s is not None
            and baseline.cpu_s > 0.0
            else None
        )
        # Both terms, always: the left number is discounted (modelled), the right one measured.
        if at_rec is None:
            walls = "-"
        elif at_rec.raw_wall_s is None:
            walls = f"{at_rec.wall_s:.3f}/-"
        else:
            walls = f"{at_rec.wall_s:.3f}/{at_rec.raw_wall_s:.3f}"
        regress = (
            "-" if speedup.regression_inner_jobs is None else str(speedup.regression_inner_jobs)
        )
        rows.append(
            [
                tag,
                str(speedup.recommended_inner_jobs),
                regress,
                eff,
                f"{knee:.2f}x",
                f"{par_eff:.2f}",
                "-" if cpu_growth is None else f"{cpu_growth:.2f}x",
                _human_bytes(None if at_rec is None else at_rec.peak_bytes),
                walls,
                curve,
            ]
        )
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    def fmt(cells: Sequence[str]) -> str:
        return "  ".join(
            f"{cells[i]:<{widths[i]}}" if i == 0 else f"{cells[i]:>{widths[i]}}"
            for i in range(len(headers))
        )

    out = [
        "",
        "parallel-speedup model (recommended inner_jobs = narrowest width within 10% of the best "
        "wall, subject to CPU-work + core budgets):",
        fmt(headers),
        "  ".join("-" * w for w in widths),
    ]
    out.extend(fmt(row) for row in rows)
    return out
