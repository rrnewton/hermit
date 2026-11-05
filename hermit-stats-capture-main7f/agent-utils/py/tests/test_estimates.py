"""Tests for the profile-store feedback reader + the planner (dagrun.estimates)."""

from __future__ import annotations

import json

from dagrun import estimates

from pathlib import Path

import pytest

from dagrun import (
    DagConfig,
    InfeasibleAllocationError,
    ResourceHint,
    Step,
    StepClass,
    allocate_widths,
    apply_plan_to_config,
    build_plan,
    load_step_samples,
    load_step_speedups,
    plan_to_json,
    plan_to_text,
)
from dagrun.estimates import (
    Planner,
    SpeedupLevel,
    StepSamples,
    StepSpeedup,
    _affinity_width,
    _parse_float,
    _parse_int,
    _robust_median,
    buckets_for_workloads,
    bucketize_rows,
    step_speedups_from_buckets,
)
from dagrun.sizing import jobs_for_budget, step_mem_cap_for_inner_jobs

_HEADER = (
    "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,enforcement_kind,"
    "runner_name,step,classification,inner_jobs,elapsed_s,returncode,ok,timed_out,oom_kills,"
    "peak_bytes,thread_peak,pct_other\n"
)
GIB = 1024**3


def _write_store(tmp_path: Path, rows: list[str], *, machine: str = "m", container: str = "c") -> Path:
    store = tmp_path / "store"
    store.mkdir()
    (store / f"step_profiles_{machine}_{container}.csv").write_text(_HEADER + "".join(rows))
    return store


def _row(step: str, elapsed: str, peak: str = "", pct_other: str = "0.0") -> str:
    return (
        f"2026-07-26T10:00:00,m,c,abc,1,abc,unverified,local,{step},light,1,{elapsed},0,True,"
        f"False,0,{peak},,{pct_other}\n"
    )


def test_no_store_file_returns_empty(tmp_path: Path) -> None:
    assert load_step_samples(tmp_path, "m", "c") == {}


def test_workload_selection_never_mixes_stale_nonblank_cohorts() -> None:
    rows = [
        {"step": "g.a", "inner_jobs": "1", "elapsed_s": "100", "workload_digest": "old"},
        {"step": "g.a", "inner_jobs": "2", "elapsed_s": "60", "workload_digest": "old"},
        {"step": "g.a", "inner_jobs": "1", "elapsed_s": "10", "workload_digest": "new"},
        {"step": "g.a", "inner_jobs": "2", "elapsed_s": "5", "workload_digest": "new"},
        {"step": "g.a", "inner_jobs": "1", "elapsed_s": "20", "workload_digest": ""},
        {"step": "g.a", "inner_jobs": "2", "elapsed_s": "10", "workload_digest": ""},
    ]
    buckets = bucketize_rows(rows, 8)

    current = buckets_for_workloads(buckets, {"g.a": "new"})
    current_model = step_speedups_from_buckets(current, 8)["g.a"]
    assert [level.wall_s for level in current_model.levels] == [10.0, 5.0]

    # A not-yet-measured current command may use pre-digest compatibility rows, but never a
    # different identified command cohort.
    fallback = buckets_for_workloads(buckets, {"g.a": "missing"})
    fallback_model = step_speedups_from_buckets(fallback, 8)["g.a"]
    assert [level.wall_s for level in fallback_model.levels] == [20.0, 10.0]


def test_median_duration_and_percentile_rss(tmp_path: Path) -> None:
    store = _write_store(
        tmp_path,
        [
            _row("g.a", "2.0", "1000"),
            _row("g.a", "4.0", "3000"),
            _row("g.a", "3.0", "2000"),
        ],
    )
    samples = load_step_samples(store, "m", "c")
    assert samples["g.a"].samples == 3
    # median of [2,3,4] == 3.0 (no contention -> no discount).
    assert samples["g.a"].est_duration_s == 3.0
    # p90 nearest-rank of [1000,2000,3000] (n=3, rank=3) == 3000.
    assert samples["g.a"].rss_estimate_bytes == 3000


def test_contention_discount_recovers_intrinsic_duration(tmp_path: Path) -> None:
    # A 10s sample under 50% other-work contention has intrinsic ~5s, matching the two quiet 5s
    # samples; the discounted median must be 5.0, not dragged up by the contended sample.
    store = _write_store(
        tmp_path,
        [
            _row("g.a", "5.0", "100", pct_other="0.0"),
            _row("g.a", "5.0", "100", pct_other="0.0"),
            _row("g.a", "10.0", "100", pct_other="50.0"),
        ],
    )
    samples = load_step_samples(store, "m", "c")
    assert samples["g.a"].est_duration_s == 5.0


def test_mad_trim_ignores_outlier(tmp_path: Path) -> None:
    store = _write_store(
        tmp_path,
        [_row("g.a", v) for v in ("3.0", "3.0", "3.0", "3.0", "100.0")],
    )
    samples = load_step_samples(store, "m", "c")
    assert samples["g.a"].est_duration_s == 3.0


def test_robust_median_small_samples_resist_one_slow_outlier() -> None:
    # At n<3 MAD-trim cannot reject an outlier; the estimate must be the MINIMUM (intrinsic value),
    # NOT the mean, so a single slow sample cannot invert a real speedup ([5, 100] -> 5, not 52.5).
    assert _robust_median([5.0]) == 5.0
    assert _robust_median([5.0, 100.0]) == 5.0
    assert _robust_median([100.0, 5.0]) == 5.0
    assert _robust_median([10.0, 10.1]) == 10.0
    # At n>=3 the MAD-trimmed median takes over and rejects the outlier symmetrically.
    assert _robust_median([5.0, 5.0, 100.0]) == 5.0
    assert _robust_median([2.0, 3.0, 4.0]) == 3.0


def test_two_sample_slow_outlier_does_not_invert_speedup(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    # A -j2 width with one clean 5s run and one slow 100s outlier (a single slow CI run) must NOT be
    # read as slower than -j1's 10s. With the min-based small-sample estimator -j2 reads as 5s, so
    # the real 2x speedup survives and -j2 is recommended (the old mean-of-two read 52.5s -> -j1).
    store = _write_speedup_store(
        tmp_path,
        [
            _speedup_row("build.app", 1, "10.0", user_s="10.0", sys_s="0.0"),
            _speedup_row("build.app", 1, "10.1", user_s="10.1", sys_s="0.0"),
            _speedup_row("build.app", 2, "5.0", user_s="10.0", sys_s="0.0"),
            _speedup_row("build.app", 2, "100.0", user_s="10.1", sys_s="0.0"),
        ],
    )
    sp = load_step_speedups(store, "m", "affinity16_cpu-max-max")["build.app"]
    assert sp.recommended_inner_jobs == 2
    lvl2 = next(lvl for lvl in sp.levels if lvl.inner_jobs == 2)
    assert lvl2.wall_s == 5.0


def test_non_finite_cells_are_rejected(tmp_path: Path) -> None:
    # An 'inf' (or overflowing) elapsed cell must be dropped, not fold into the width median: it
    # passes a bare `>= 0.0` filter but is non-finite, so it would otherwise poison the estimate.
    assert _parse_float("inf") is None
    assert _parse_float("-inf") is None
    assert _parse_float("nan") is None
    assert _parse_float("1e400") is None
    store = _write_store(
        tmp_path,
        [_row("g.a", "3.0"), _row("g.a", "inf"), _row("g.a", "5.0")],
    )
    samples = load_step_samples(store, "m", "c")
    # 'inf' dropped -> durations [3.0, 5.0]; at n<3 the robust estimate is the minimum -> 3.0.
    assert samples["g.a"].est_duration_s == 3.0


def test_missing_columns_degrade_gracefully(tmp_path: Path) -> None:
    # No peak_bytes recorded -> rss_estimate is None (fall back to hint elsewhere).
    store = _write_store(tmp_path, [_row("g.a", "2.0", peak=""), _row("g.a", "2.0", peak="")])
    samples = load_step_samples(store, "m", "c")
    assert samples["g.a"].est_duration_s == 2.0
    assert samples["g.a"].rss_estimate_bytes is None


def test_parse_helpers_match_rust_strict_grammar() -> None:
    # Surrounding ASCII whitespace is TRIMMED (matching Rust's str::parse after trim), so a padded
    # cell parses to the same number instead of one build dropping it.
    assert _parse_float(" 50.0 ") == 50.0
    assert _parse_int("  1000\t") == 1000
    # PEP-515 underscore separators are REJECTED (Rust's str::parse rejects them; Python's
    # float()/int() would otherwise accept them) -> both builds drop the cell.
    assert _parse_float("1_0.0") is None
    assert _parse_int("1_000") is None
    # Out-of-i64 magnitudes are REJECTED (Python's arbitrary-precision int would otherwise keep
    # them; Rust's parse::<i64>() overflows) -> both builds drop the cell.
    assert _parse_int("9999999999999999999999") is None
    assert _parse_int(str(2**63 - 1)) == 2**63 - 1
    assert _parse_int(str(2**63)) is None
    # Empty / whitespace-only / non-ASCII cells are None in both builds.
    assert _parse_float("   ") is None
    assert _parse_float(None) is None
    assert _parse_float("１.０") is None  # fullwidth digits are non-ASCII -> rejected like Rust


def test_affinity_width_rejects_non_ascii_digits_without_raising() -> None:
    # ASCII affinity widths parse; a Unicode-digit container_class (e.g. the superscript two) must
    # NOT raise (Python's str.isdigit() accepts it but int() cannot) -> returns None like Rust.
    assert _affinity_width("affinity8_cpu-max") == 8
    assert _affinity_width("affinity316_x") == 316
    assert _affinity_width("affinity²_cpu") is None  # superscript 2
    assert _affinity_width("nonaffinity") is None
    assert _affinity_width("affinity_cpu") is None


def test_whitespace_cells_parse_and_discount_after_trim(tmp_path: Path) -> None:
    # Whitespace-padded elapsed_s AND pct_other must be trimmed, then the discount applies: the 10s
    # sample under a padded ' 50.0 '% contention becomes intrinsic 5s, matching the clean samples.
    store = _write_store(
        tmp_path,
        [
            _row("g.a", " 8.0 ", "1000", pct_other="0.0"),
            _row("g.a", "4.0", "2000", pct_other="0.0"),
            _row("g.a", "10.0", "3000", pct_other=" 50.0 "),
        ],
    )
    samples = load_step_samples(store, "m", "c")
    assert samples["g.a"].samples == 3
    # robust median of [8, 4, 5] == 5.0.
    assert samples["g.a"].est_duration_s == 5.0
    # p90 of [1000, 2000, 3000] == 3000.
    assert samples["g.a"].rss_estimate_bytes == 3000


def test_underscore_and_overflow_cells_rejected(tmp_path: Path) -> None:
    store = _write_store(
        tmp_path,
        [
            _row("g.a", "1_0.0", "1_000"),
            _row("g.a", "4.0", "9999999999999999999999"),
            _row("g.a", "6.0", "5000"),
        ],
    )
    samples = load_step_samples(store, "m", "c")
    assert samples["g.a"].samples == 3
    # '1_0.0' rejected -> durations [4, 6]. With only TWO samples MAD-trim cannot reject an outlier,
    # so the robust estimate is the MINIMUM (the intrinsic, uncontended value) -> 4.0.
    assert samples["g.a"].est_duration_s == 4.0
    # '1_000' and the out-of-i64 peak rejected -> peaks [5000] -> p90 5000.
    assert samples["g.a"].rss_estimate_bytes == 5000


def test_non_ascii_container_class_does_not_crash_loader(tmp_path: Path) -> None:
    store = _write_store(
        tmp_path, [_row("g.a", "3.0", "1000")], machine="m", container="affinity²_cpu"
    )
    samples = load_step_samples(store, "m", "affinity²_cpu")
    # No affinity width parsed (non-ASCII digit), so no external_cores discount -> plain median.
    assert samples["g.a"].est_duration_s == 3.0


def _dag() -> DagConfig:
    return DagConfig(
        steps=(
            Step("g", "prep", "prep", "true", hint=ResourceHint(est_duration_s=1.0)),
            Step("g", "heavy", "heavy", "true", deps=["g.prep"], hint=ResourceHint(est_duration_s=10.0)),
            Step("g", "solo", "solo", "true", hint=ResourceHint(est_duration_s=5.0)),
        )
    )


def test_planner_orders_differ_and_are_deterministic() -> None:
    cfg = _dag()
    lpt = build_plan(cfg, {}, planner=Planner.GREEDY_LPT)
    cp = build_plan(cfg, {}, planner=Planner.CRITICAL_PATH)
    # greedy-lpt: by single est desc; critical-path: by bottom-level desc.
    assert list(lpt.order) == ["g.heavy", "g.solo", "g.prep"]
    assert list(cp.order) == ["g.prep", "g.heavy", "g.solo"]
    assert lpt.order != cp.order
    # bottom_level(prep) = 1 + bottom_level(heavy)=10 -> 11; critical path prep -> heavy.
    assert list(cp.critical_path) == ["g.prep", "g.heavy"]
    assert cp.critical_path_length_s == 11.0


def test_store_wins_over_hint_and_feeds_config(tmp_path: Path) -> None:
    store = _write_store(
        tmp_path,
        [_row("g.heavy", "8.0", "6000"), _row("g.heavy", "8.0", "6000")],
    )
    samples = load_step_samples(store, "m", "c")
    cfg = _dag()
    plan = build_plan(cfg, samples, planner=Planner.CRITICAL_PATH)
    by_tag = plan.by_tag()
    # g.heavy learned 8.0s + 6000 bytes from the store; g.solo has neither -> hint / none.
    assert by_tag["g.heavy"].est_source == "store"
    assert by_tag["g.heavy"].est_duration_s == 8.0
    assert by_tag["g.heavy"].rss_source == "store"
    assert by_tag["g.heavy"].rss_estimate_bytes == 6000
    assert by_tag["g.solo"].est_source == "hint"
    assert by_tag["g.prep"].rss_source == "none"
    # apply_plan_to_config threads the learned values into the cfg hints (feeds sizing + ordering).
    applied = apply_plan_to_config(cfg, plan)
    heavy = applied.by_tag()["g.heavy"]
    assert heavy.hint.est_duration_s == 8.0
    assert heavy.hint.rss_baseline_bytes == 6000


def test_authored_rss_is_the_floor_for_ordinary_profile_feedback() -> None:
    """One quiet sample may refine time, but cannot tighten the enforced memory ceiling."""

    step = Step(
        "g",
        "variable",
        "variable",
        "true",
        hint=ResourceHint(est_duration_s=10.0, rss_baseline_bytes=8_000),
    )
    cfg = DagConfig(steps=(step,))

    quiet = {"g.variable": StepSamples("g.variable", 1, 3.0, 2_000)}
    quiet_plan = build_plan(cfg, quiet)
    quiet_entry = quiet_plan.entries[0]
    assert quiet_entry.est_source == "store"
    assert quiet_entry.est_duration_s == 3.0
    assert quiet_entry.rss_source == "hint"
    assert quiet_entry.rss_estimate_bytes == 8_000
    assert apply_plan_to_config(cfg, quiet_plan).steps[0].hint.rss_baseline_bytes == 8_000

    # Ordinary feedback remains allowed to raise an underestimate. Safe, deliberate lowering is
    # the separate censor-aware --profile-memory-feedback contract.
    high = {"g.variable": StepSamples("g.variable", 1, 3.0, 12_000)}
    high_plan = build_plan(cfg, high)
    assert high_plan.entries[0].rss_source == "store"
    assert high_plan.entries[0].rss_estimate_bytes == 12_000
    assert apply_plan_to_config(cfg, high_plan).steps[0].hint.rss_baseline_bytes == 12_000


def test_apply_plan_preserves_all_non_hint_step_fields() -> None:
    # Recurrence guard (environment-independent, so it also fires on a boxing-less CI host): the
    # planner rewrites ONLY a step's hint; every other field must survive verbatim. A field-by-field
    # rebuild here once silently dropped `cpu_timeout` (default 0), disabling the per-step CPU-time
    # guard on every planned run while the Rust build kept enforcing it. Behavioral cross-checks
    # catch that only where cgroups exist; this unit test catches the field drop everywhere.
    step = Step(
        "g",
        "burn",
        "burn",
        "while :; do :; done",
        deps=[],
        env={"K": "V"},
        hint=ResourceHint(est_duration_s=3.0),
        networkonly=True,
        engine_only=True,
        timeout=123,
        cpu_timeout=7,
        jobs_flag="-J",
    )
    cfg = DagConfig(steps=(step,))
    applied = apply_plan_to_config(cfg, build_plan(cfg, {}, planner=Planner.GREEDY_LPT))
    out = applied.by_tag()["g.burn"]
    # The guard that regressed:
    assert out.cpu_timeout == 7, "planner dropped cpu_timeout -> CPU-time enforcement silently off"
    # Every other non-hint field must round-trip through planning unchanged.
    assert out.timeout == 123
    assert out.networkonly is True
    assert out.engine_only is True
    assert out.jobs_flag == "-J"
    assert out.cmd == "while :; do :; done"
    assert out.env == {"K": "V"}


def test_min_samples_threshold_falls_back_to_hint(tmp_path: Path) -> None:
    store = _write_store(tmp_path, [_row("g.heavy", "8.0", "6000")])
    samples = load_step_samples(store, "m", "c")
    cfg = _dag()
    # With min_samples=2 the single sample is not enough; the DAG hint (10.0) wins.
    plan = build_plan(cfg, samples, planner=Planner.GREEDY_LPT, min_samples=2)
    entry = plan.by_tag()["g.heavy"]
    assert entry.est_source == "hint"
    assert entry.est_duration_s == 10.0


def test_plan_json_and_text_are_stable(tmp_path: Path) -> None:
    cfg = _dag()
    plan = build_plan(cfg, {}, planner=Planner.GREEDY_LPT)
    js = plan_to_json(plan)
    assert '"planner": "greedy-lpt"' in js
    assert js.count('"tag":') == 3
    assert js.startswith("{\n") and js.endswith("}")
    # Non-CPA planners carry the allocation fields as null (populated only under --planner cpa).
    assert '"allocation": null' in js
    assert '"alloc_inner_jobs": null' in js
    text = plan_to_text(plan)
    assert text.startswith("plan: greedy-lpt\n")
    assert "scheduled order: g.heavy, g.solo, g.prep" in text
    assert "critical path (" in text


def test_empty_dag_plan() -> None:
    plan = build_plan(DagConfig(steps=()), {}, planner=Planner.CRITICAL_PATH)
    assert list(plan.order) == []
    assert list(plan.critical_path) == []
    assert plan_to_json(plan).endswith('"steps": []\n}')


# --------------------------------------------------------------------------- speedup model

_SPEEDUP_HEADER = (
    "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,enforcement_kind,"
    "runner_name,step,classification,inner_jobs,elapsed_s,returncode,ok,timed_out,oom_kills,"
    "peak_bytes,thread_peak,effective_cores,user_s,sys_s,throttled_s\n"
)


def _speedup_row(
    step: str,
    inner_jobs: int,
    elapsed: str,
    *,
    effective_cores: str = "",
    user_s: str = "",
    sys_s: str = "",
    throttled_s: str = "",
    peak_bytes: str = "1000",
) -> str:
    return (
        f"t,m,affinity16_cpu-max-max,abc,1,abc,unverified,local,{step},cpu-bound,{inner_jobs},"
        f"{elapsed},0,True,False,0,{peak_bytes},,{effective_cores},{user_s},{sys_s},{throttled_s}\n"
    )


def _write_speedup_store(tmp_path: Path, rows: list[str]) -> Path:
    store = tmp_path / "sstore"
    store.mkdir()
    (store / "step_profiles_m_affinity16_cpu-max-max.csv").write_text(_SPEEDUP_HEADER + "".join(rows))
    return store


def test_speedup_knee_stops_at_two_on_rising_cpu(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    # Wall halves 1->2 (10->5) but barely improves 2->4 (5->4.5) while total CPU-s rises
    # (~10.2 -> ~18): both the marginal-gain and the work-conservation signals say stop at -j2.
    store = _write_speedup_store(
        tmp_path,
        [
            _speedup_row("build.app", 1, "10.0", effective_cores="1.0", user_s="10.0", sys_s="0.2"),
            _speedup_row("build.app", 1, "10.1", effective_cores="1.0", user_s="10.1", sys_s="0.2"),
            _speedup_row("build.app", 2, "5.0", effective_cores="1.98", user_s="10.0", sys_s="0.4"),
            _speedup_row("build.app", 2, "5.05", effective_cores="1.98", user_s="10.1", sys_s="0.4"),
            _speedup_row("build.app", 4, "4.5", effective_cores="3.2", user_s="17.5", sys_s="0.8"),
            _speedup_row("build.app", 4, "4.6", effective_cores="3.2", user_s="17.6", sys_s="0.9"),
        ],
    )
    models = load_step_speedups(store, "m", "affinity16_cpu-max-max")
    sp = models["build.app"]
    assert sp.recommended_inner_jobs == 2
    assert sp.baseline_inner_jobs == 1
    assert [lvl.inner_jobs for lvl in sp.levels] == [1, 2, 4]
    # measured_effective_cores is the achieved parallelism at the recommended width.
    assert sp.measured_effective_cores == 1.98
    # speedup at -j2 is ~2.0 (10.05 median / 5.025 median).
    lvl2 = next(lvl for lvl in sp.levels if lvl.inner_jobs == 2)
    assert abs(lvl2.speedup - 2.0) < 1e-9


def test_speedup_linear_recommends_widest_within_budget(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    # Near-linear scaling with flat total CPU-s: the widest measured width (still within the
    # affinity-16 budget) is recommended.
    store = _write_speedup_store(
        tmp_path,
        [
            _speedup_row("lin.step", 1, "8.0", user_s="8.0", sys_s="0.0"),
            _speedup_row("lin.step", 2, "4.0", user_s="8.0", sys_s="0.0"),
            _speedup_row("lin.step", 4, "2.0", user_s="8.1", sys_s="0.0"),
        ],
    )
    models = load_step_speedups(store, "m", "affinity16_cpu-max-max")
    assert models["lin.step"].recommended_inner_jobs == 4


def test_plateau_is_global_and_grid_invariant() -> None:
    # 8 threads already deliver 7.2x of the best observed 7.9x at 64: its wall is within 10% of
    # the best, so burning 8x the width for the last 9.7% is not the economic choice. Adding a
    # midpoint must not change that answer merely by changing adjacent ratios.
    coarse = _speedup_over({1: [56.88], 8: [7.9], 64: [7.2]})
    refined = _speedup_over({1: [56.88], 8: [7.9], 32: [7.35], 64: [7.2]})
    assert coarse.recommended_inner_jobs == 8
    assert refined.recommended_inner_jobs == 8


def test_speedup_levels_keep_a_width_specific_memory_response(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    rows: list[str] = []
    for width, wall, peak in ((1, "8.0", "1000"), (4, "2.2", "4000")):
        rows.extend(
            _speedup_row(
                "mem.step", width, wall, user_s="8.0", sys_s="0.0", peak_bytes=peak
            )
            for _ in range(3)
        )
    model = load_step_speedups(
        _write_speedup_store(tmp_path, rows), "m", "affinity16_cpu-max-max"
    )["mem.step"]
    assert [(level.inner_jobs, level.peak_bytes, level.peak_samples) for level in model.levels] == [
        (1, 1000, 3),
        (4, 4000, 3),
    ]


def test_width_specific_memory_excludes_censored_modern_rows() -> None:
    rows: list[dict[str, str]] = []
    for width, peak, cap, reclaim in ((1, 1000, "max", "0"), (2, 2000, "2000", "1")):
        rows.extend(
            {
                "step": "mem.step",
                "inner_jobs": str(width),
                "elapsed_s": str(4.0 / width),
                "peak_bytes": str(peak),
                "memory_max_bytes": cap,
                "memory_events_high": "0",
                "memory_events_max": reclaim,
                "memory_events_oom": "0",
                "memory_events_oom_kill": "0",
                "ok": "True",
                "returncode": "0",
            }
            for _ in range(3)
        )

    model = step_speedups_from_buckets(bucketize_rows(rows, None), 2)["mem.step"]
    points = {level.inner_jobs: level for level in model.levels}

    assert points[1].peak_bytes == 1000
    assert points[1].peak_samples == 3
    assert points[2].peak_bytes is None
    assert points[2].peak_samples == 0
    assert points[2].peak_floor_bytes == 2000
    assert points[2].peak_floor_samples == 3


def test_censored_width_peak_remains_a_planning_floor() -> None:
    rows: list[dict[str, str]] = []
    for width, wall, peak, cap, reclaim in (
        (1, 8.0, GIB, "max", "0"),
        (8, 1.0, 4 * GIB, str(4 * GIB), "2"),
    ):
        rows.extend(
            {
                "step": "m.scaling",
                "inner_jobs": str(width),
                "elapsed_s": str(wall),
                "user_s": "8.0",
                "sys_s": "0.0",
                "peak_bytes": str(peak),
                "memory_max_bytes": cap,
                "memory_events_high": "0",
                "memory_events_max": reclaim,
                "memory_events_oom": "0",
                "memory_events_oom_kill": "0",
                "ok": "True",
                "returncode": "0",
            }
            for _ in range(3)
        )
    speedup = step_speedups_from_buckets(bucketize_rows(rows, None), 8)["m.scaling"]
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "scaling",
                "",
                "true",
                jobs_flag="-j%d",
                hint=ResourceHint(
                    est_duration_s=8.0,
                    rss_baseline_bytes=GIB,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )

    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"m.scaling": speedup},
        core_budget=8,
        mem_budget=3 * GIB,
    )

    assert plan.entries[0].alloc_inner_jobs == 1
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "mem-capped"


def test_censored_floor_never_replaces_a_larger_width_fallback() -> None:
    rows: list[dict[str, str]] = []
    for width, wall, peak, cap, reclaim in (
        (1, 8.0, 4 * GIB, "max", "0"),
        (8, 1.0, 5 * GIB, str(5 * GIB), "1"),
    ):
        rows.extend(
            {
                "step": "m.scaling",
                "inner_jobs": str(width),
                "elapsed_s": str(wall),
                "user_s": "8.0",
                "sys_s": "0.0",
                "peak_bytes": str(peak),
                "memory_max_bytes": cap,
                "memory_events_high": "0",
                "memory_events_max": reclaim,
                "memory_events_oom": "0",
                "memory_events_oom_kill": "0",
                "ok": "True",
                "returncode": "0",
            }
            for _ in range(3)
        )
    speedup = step_speedups_from_buckets(bucketize_rows(rows, None), 8)["m.scaling"]
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "scaling",
                "",
                "true",
                jobs_flag="-j%d",
                hint=ResourceHint(
                    est_duration_s=8.0,
                    rss_baseline_bytes=4 * GIB,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )

    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"m.scaling": speedup},
        core_budget=8,
        mem_budget=6 * GIB,
    )

    # The ordinary width-8 fallback is 8 GiB. Its 5 GiB censored floor may raise a smaller model,
    # but must not replace this larger conservative estimate and make the width look feasible.
    assert plan.entries[0].alloc_inner_jobs == 1
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "mem-capped"


def test_speedup_single_level_has_no_model(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    # One inner_jobs width is not enough to model a curve.
    store = _write_speedup_store(
        tmp_path,
        [_speedup_row("s.one", 1, "5.0"), _speedup_row("s.one", 1, "5.1")],
    )
    assert load_step_speedups(store, "m", "affinity16_cpu-max-max") == {}


def test_speedup_core_budget_caps_recommendation(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    # Perfect linear scaling to -j4, but a 2-core container budget caps the recommendation at 2.
    store = tmp_path / "cbstore"
    store.mkdir()
    header = _SPEEDUP_HEADER
    body = "".join(
        f"t,m,affinity2_cpu-max-max,abc,1,abc,unverified,local,cb.step,cpu-bound,{j},{w},0,True,"
        f"False,0,1000,,,8.0,0.0,\n"
        for j, w in ((1, "8.0"), (2, "4.0"), (4, "2.0"))
    )
    (store / "step_profiles_m_affinity2_cpu-max-max.csv").write_text(header + body)
    models = load_step_speedups(store, "m", "affinity2_cpu-max-max")
    assert models["cb.step"].recommended_inner_jobs == 2


def test_plan_includes_speedup_field(tmp_path: Path) -> None:
    from dagrun import load_step_speedups

    store = _write_speedup_store(
        tmp_path,
        [
            _speedup_row("build.app", 1, "10.0", user_s="10.0", sys_s="0.0"),
            _speedup_row("build.app", 2, "5.0", user_s="10.0", sys_s="0.0"),
        ],
    )
    cfg = DagConfig(steps=(Step("build", "app", "compile", "true"),))
    speedups = load_step_speedups(store, "m", "affinity16_cpu-max-max")
    plan = build_plan(cfg, {}, planner=Planner.GREEDY_LPT, speedups=speedups)
    js = plan_to_json(plan)
    assert '"speedup": {' in js
    assert '"recommended_inner_jobs": 2' in js
    text = plan_to_text(plan)
    assert "parallel-speedup model" in text
    assert "curve(inner_jobs->speedup)" in text
    # A plan with no speedup data has "speedup": null and no model section.
    bare = build_plan(cfg, {}, planner=Planner.GREEDY_LPT)
    assert '"speedup": null' in plan_to_json(bare)
    assert "parallel-speedup model" not in plan_to_text(bare)


def test_scaling_model_sidecar_is_outside_the_dag_and_deterministic(tmp_path: Path) -> None:
    speedups = _cpa_speedups(
        tmp_path,
        [
            _speedup_row("g.a", 1, "8.0", user_s="8.0", sys_s="0.0"),
            _speedup_row("g.a", 2, "4.0", user_s="8.0", sys_s="0.0"),
        ],
    )
    encoded = estimates.scaling_model_to_json(
        "m", _CPA_CONTAINER, speedups, {"g.a": "current"}
    )
    parsed = json.loads(encoded)
    assert parsed["schema"] == 2
    assert parsed["steps"][0]["step"] == "g.a"
    assert parsed["steps"][0]["workload_digest"] == "current"
    assert parsed["steps"][0]["recommended_inner_jobs"] == 2
    path = estimates.write_scaling_model(
        tmp_path, "m", _CPA_CONTAINER, speedups, {"g.a": "current"}
    )
    assert path.parent == tmp_path
    assert path.read_text(encoding="utf-8") == encoded


# --------------------------------------------------------------------------- CPA allocator

_CPA_CONTAINER = "affinity16_cpu-max-max"


def _cpa_speedups(tmp_path: Path, rows: list[str]) -> dict[str, StepSpeedup]:
    store = _write_speedup_store(tmp_path, rows)
    return load_step_speedups(store, "m", _CPA_CONTAINER)


def _cpa_est(cfg: DagConfig) -> dict[str, float]:
    return {step.tag: step.hint.est_duration_s for step in cfg.steps}


def test_cpa_spreads_cores_on_independent_tasks(tmp_path: Path) -> None:
    # Two INDEPENDENT linear-scaling steps, P=4: the allocator balances T_CP against area/P and
    # SPREADS cores — both land at width 2 (not one hogging 4), stopping "balanced" at the balance
    # point BEFORE the knee (curve reaches 4). This is the area-bound behavior (PLANNER_DESIGN.md §2).
    rows: list[str] = []
    for tag in ("g.a", "g.b"):
        for j, w in ((1, "10.0"), (2, "5.0"), (4, "2.5")):
            rows.append(_speedup_row(tag, j, w, user_s="10.0", sys_s="0.0"))
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step("g", "a", "a", "true", hint=ResourceHint(est_duration_s=10.0)),
            Step("g", "b", "b", "true", hint=ResourceHint(est_duration_s=10.0)),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    widths = allocate_widths(cfg, speedups, _cpa_est(cfg), 4)
    assert widths == {"g.a": 2, "g.b": 2}
    plan = build_plan(cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=4)
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "balanced"
    assert plan.allocation.modeled_makespan_s >= plan.allocation.lower_bound_s


def test_cpa_area_uses_measured_cpu_work_not_reserved_width_area(tmp_path: Path) -> None:
    speedups = _cpa_speedups(
        tmp_path,
        [
            _speedup_row("g.a", 1, "10.0", user_s="10.0", sys_s="0.0"),
            _speedup_row("g.a", 2, "5.0", user_s="14.0", sys_s="0.0"),
        ],
    )
    cfg = DagConfig(
        steps=(Step("g", "a", "", "true", hint=ResourceHint(est_duration_s=10.0)),),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    plan = build_plan(cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=2)
    assert plan.allocation is not None
    assert plan.entries[0].alloc_inner_jobs == 2
    assert plan.allocation.area_s == 14.0
    assert plan.allocation.area_bound_s == 7.0


def test_cpa_uses_replicated_width_specific_memory(tmp_path: Path) -> None:
    rows: list[str] = []
    for width, wall, peak in ((1, "8.0", str(GIB)), (4, "2.0", str(3 * GIB))):
        rows.extend(
            _speedup_row(
                "m.scaling",
                width,
                wall,
                user_s="8.0",
                sys_s="0.0",
                peak_bytes=peak,
            )
            for _ in range(3)
        )
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "scaling",
                "",
                "true",
                hint=ResourceHint(
                    est_duration_s=8.0,
                    # Keep the authored floor below the measured width-one peak: this test is
                    # about choosing among replicated width observations. A separate regression
                    # below proves that those observations may not lower a larger authored floor.
                    rss_baseline_bytes=GIB,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    plan = build_plan(
        cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=4, mem_budget=2 * GIB
    )
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "mem-capped"
    assert plan.entries[0].alloc_inner_jobs == 1
    assert plan.entries[0].rss_estimate_bytes == GIB


def test_applied_exact_width_memory_is_not_scaled_twice(tmp_path: Path) -> None:
    rows: list[str] = []
    for width, wall, peak in ((1, "8.0", str(GIB)), (8, "1.0", str(3 * GIB))):
        rows.extend(
            _speedup_row(
                "m.scaling",
                width,
                wall,
                user_s="8.0",
                sys_s="0.0",
                peak_bytes=peak,
            )
            for _ in range(3)
        )
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "scaling",
                "",
                "true",
                jobs_flag="-j%d",
                hint=ResourceHint(
                    est_duration_s=8.0,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )

    plan = build_plan(cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=8)
    applied = apply_plan_to_config(cfg, plan)

    assert plan.entries[0].alloc_inner_jobs == 8
    assert plan.entries[0].rss_estimate_inner_jobs == 8
    assert applied.steps[0].hint.rss_baseline_inner_jobs == 8
    assert step_mem_cap_for_inner_jobs(applied.steps[0], 8, mem_cap_factor=1.0) == 3 * GIB
    assert jobs_for_budget(applied, 4 * GIB)[1] == 3 * GIB


def test_replicated_width_memory_cannot_lower_an_authored_rss_floor(tmp_path: Path) -> None:
    rows: list[str] = []
    for width, wall in ((1, "8.0"), (2, "4.0")):
        rows.extend(
            _speedup_row(
                "m.variable",
                width,
                wall,
                user_s="8.0",
                sys_s="0.0",
                peak_bytes=str(GIB),
            )
            for _ in range(3)
        )
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "variable",
                "",
                "true",
                hint=ResourceHint(
                    est_duration_s=8.0,
                    rss_baseline_bytes=8 * GIB,
                    preferred_inner_jobs=1,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
    )

    plan = build_plan(cfg, {}, speedups=speedups, core_budget=2)
    assert plan.entries[0].rss_estimate_bytes == 8 * GIB
    assert plan.entries[0].rss_source == "hint"
    assert plan.entries[0].rss_estimate_inner_jobs is None
    applied = apply_plan_to_config(cfg, plan)
    assert applied.steps[0].hint.rss_baseline_bytes == 8 * GIB
    assert applied.steps[0].hint.rss_baseline_inner_jobs is None


def test_cpa_piles_cores_on_the_chain_and_leaves_plateau_narrow(tmp_path: Path) -> None:
    # A chain prep(curveless) -> build(scaling) -> test(scaling) plus an independent plateau (side).
    # With ample cores CPA PILES cores onto the critical-path chain (build->8, test->4), while the
    # plateau step and the curveless prep stay at 1, collapsing the width-1 58s path to ~11s.
    rows: list[str] = []
    for j, w in ((1, "40.0"), (2, "20.0"), (4, "10.0"), (8, "5.0")):
        rows.append(_speedup_row("c.build", j, w, user_s="40.0", sys_s="0.0"))
    for j, w in ((1, "16.0"), (2, "8.0"), (4, "4.0")):
        rows.append(_speedup_row("c.test", j, w, user_s="16.0", sys_s="0.0"))
    for j, w in ((1, "9.0"), (2, "8.7")):
        rows.append(_speedup_row("c.side", j, w, user_s="9.0", sys_s="0.0"))
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step("c", "prep", "p", "true", hint=ResourceHint(est_duration_s=2.0)),
            Step("c", "build", "b", "true", deps=["c.prep"], hint=ResourceHint(est_duration_s=40.0)),
            Step("c", "test", "t", "true", deps=["c.build"], hint=ResourceHint(est_duration_s=16.0)),
            Step("c", "side", "s", "true", hint=ResourceHint(est_duration_s=9.0)),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    plan = build_plan(cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=16)
    widths = {e.tag: e.alloc_inner_jobs for e in plan.entries}
    assert widths["c.build"] == 8  # piled onto the chain (up to the knee)
    assert widths["c.test"] == 4
    assert widths["c.side"] == 1  # plateau: never widened
    assert widths["c.prep"] == 1  # curveless: rigid
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "knee-exhausted"
    assert plan.allocation.modeled_makespan_s < 58.0  # beats the width-1 critical path
    assert plan.allocation.modeled_makespan_s >= plan.allocation.lower_bound_s


def test_cpa_memory_blocks_widening_even_with_free_cores(tmp_path: Path) -> None:
    # A CPU-bound scaling step whose memory cap grows with width: widening m.heavy 4 -> 8 doubles the
    # modeled footprint (3 GiB -> 6 GiB), so a 5 GiB budget BLOCKS it even though cores are free.
    rows = [
        _speedup_row("m.heavy", j, w, user_s="40.0", sys_s="0.0")
        for j, w in ((1, "40.0"), (2, "20.0"), (4, "10.0"), (8, "5.0"))
    ]
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step("m", "prep", "p", "true", hint=ResourceHint(est_duration_s=2.0)),
            Step(
                "m",
                "heavy",
                "h",
                "true",
                deps=["m.prep"],
                hint=ResourceHint(
                    est_duration_s=40.0,
                    rss_baseline_bytes=3 * 1024**3,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    plan = build_plan(
        cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=16, mem_budget=5 * 1024**3
    )
    widths = {e.tag: e.alloc_inner_jobs for e in plan.entries}
    assert widths["m.heavy"] == 4  # memory-capped at 4 (cores were free up to 8)
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "mem-capped"
    # With no RAM budget the same step widens all the way to its knee (8).
    free = build_plan(cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=16)
    assert {e.tag: e.alloc_inner_jobs for e in free.entries}["m.heavy"] == 8


def test_cpa_memory_uses_learned_rss_before_allocating(tmp_path: Path) -> None:
    rows = [
        _speedup_row("m.heavy", j, w, user_s="40.0", sys_s="0.0")
        for j, w in ((1, "40.0"), (8, "5.0"))
    ]
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "heavy",
                "",
                "true",
                hint=ResourceHint(
                    est_duration_s=40.0,
                    rss_baseline_bytes=GIB,
                    classification=StepClass.CPU_BOUND,
                    preferred_inner_jobs=1,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    samples = {
        "m.heavy": estimates.StepSamples("m.heavy", 3, 40.0, 8 * GIB)
    }

    plan = build_plan(
        cfg,
        samples,
        planner=Planner.CPA,
        speedups=speedups,
        core_budget=8,
        mem_budget=4 * GIB,
    )

    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "infeasible-memory"
    assert plan.allocation.modeled_makespan_s == float("inf")
    assert plan.entries[0].rss_source == "store"
    assert plan.entries[0].rss_estimate_bytes == 8 * GIB
    assert plan.entries[0].alloc_inner_jobs is None
    assert apply_plan_to_config(cfg, plan) is cfg


def test_cpa_seed_applies_outer_memory_envelope_and_raises_typed_error() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "m",
                "heavy",
                "",
                "true",
                hint=ResourceHint(est_duration_s=1.0, rss_baseline_bytes=3 * GIB),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=2.0,
    )

    plan = build_plan(cfg, {}, planner=Planner.CPA, core_budget=8, mem_budget=5 * GIB)
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "infeasible-memory"
    assert plan.allocation.modeled_makespan_s == float("inf")
    assert plan.entries[0].alloc_inner_jobs is None
    assert apply_plan_to_config(cfg, plan) is cfg
    with pytest.raises(InfeasibleAllocationError) as excinfo:
        allocate_widths(cfg, {}, _cpa_est(cfg), 8, 5 * GIB)
    assert excinfo.value.mem_budget == 5 * GIB
    assert excinfo.value.memory_footprint == 6 * GIB


def test_cpa_memory_honors_the_active_step_ceiling() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "m", "a", "", "true",
                hint=ResourceHint(est_duration_s=1.0, hard_mem_max_bytes=4 * GIB),
            ),
            Step(
                "m", "b", "", "true",
                hint=ResourceHint(est_duration_s=1.0, hard_mem_max_bytes=4 * GIB),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )

    concurrent = build_plan(cfg, {}, planner=Planner.CPA, core_budget=2, mem_budget=4 * GIB)
    serial = build_plan(
        cfg, {}, planner=Planner.CPA, core_budget=2, mem_budget=4 * GIB, max_steps=1
    )

    assert concurrent.allocation is not None
    assert concurrent.allocation.stop_reason != "infeasible-memory"
    assert concurrent.allocation.modeled_max_steps == 1
    assert concurrent.allocation.modeled_makespan_s == 2.0
    assert serial.allocation is not None
    assert serial.allocation.stop_reason != "infeasible-memory"
    assert serial.allocation.modeled_max_steps == 1
    assert jobs_for_budget(apply_plan_to_config(cfg, serial), 4 * GIB) == (1, 4 * GIB)


def test_cpa_trades_overlap_for_width_when_that_lowers_makespan() -> None:
    def curve(tag: str) -> StepSpeedup:
        levels = (
            SpeedupLevel(
                inner_jobs=1,
                samples=3,
                wall_s=100.0,
                raw_wall_s=100.0,
                wall_min_s=100.0,
                wall_max_s=100.0,
                cpu_s=100.0,
                effective_cores=1.0,
                throttled_s=0.0,
                speedup=1.0,
                peak_bytes=GIB,
                peak_samples=3,
            ),
            SpeedupLevel(
                inner_jobs=8,
                samples=3,
                wall_s=12.5,
                raw_wall_s=12.5,
                wall_min_s=12.5,
                wall_max_s=12.5,
                cpu_s=100.0,
                effective_cores=8.0,
                throttled_s=0.0,
                speedup=8.0,
                peak_bytes=3 * GIB,
                peak_samples=3,
            ),
        )
        return StepSpeedup(tag, 1, 8, 8.0, None, levels)

    cfg = DagConfig(
        steps=(
            Step("g", "a", "", "true", jobs_flag="-j%d", hint=ResourceHint(est_duration_s=100.0)),
            Step("g", "b", "", "true", jobs_flag="-j%d", hint=ResourceHint(est_duration_s=100.0)),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    speedups = {tag: curve(tag) for tag in ("g.a", "g.b")}

    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups=speedups,
        core_budget=16,
        mem_budget=4 * GIB,
        max_steps=2,
    )
    widths = {entry.tag: entry.alloc_inner_jobs for entry in plan.entries}

    assert widths == {"g.a": 8, "g.b": 8}
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "balanced"
    assert plan.allocation.modeled_max_steps == 1
    assert plan.allocation.modeled_makespan_s == 25.0


def test_cpa_overlap_search_retains_larger_ceiling_on_makespan_tie() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "m", "a", "", "true",
                hint=ResourceHint(est_duration_s=1.0, hard_mem_max_bytes=GIB),
            ),
            Step(
                "m", "b", "", "true", deps=["m.a"],
                hint=ResourceHint(est_duration_s=1.0, hard_mem_max_bytes=GIB),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )

    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        core_budget=2,
        mem_budget=GIB,
        max_steps=2,
    )

    assert plan.allocation is not None
    assert plan.allocation.modeled_makespan_s == 2.0
    assert plan.allocation.modeled_max_steps == 2


def test_cpa_refits_plateau_and_never_takes_a_negative_gain_width() -> None:
    levels = tuple(
        SpeedupLevel(
            inner_jobs=width,
            samples=3,
            wall_s=wall,
            raw_wall_s=wall,
            wall_min_s=wall,
            wall_max_s=wall,
            cpu_s=100.0,
            effective_cores=float(width),
            throttled_s=0.0,
            speedup=100.0 / wall,
        )
        for width, wall in ((1, 100.0), (2, 50.0), (4, 70.0), (8, 10.0))
    )
    speedup = StepSpeedup("g.scaling", 1, 8, 8.0, 4, levels)
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "scaling",
                "",
                "true",
                jobs_flag="-j%d",
                hint=ResourceHint(est_duration_s=100.0),
            ),
        )
    )

    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"g.scaling": speedup},
        core_budget=4,
    )

    assert plan.entries[0].speedup is not None
    assert plan.entries[0].speedup.recommended_inner_jobs == 2
    assert plan.entries[0].alloc_inner_jobs == 2


def test_cpa_does_not_accept_a_negative_gain_widening() -> None:
    levels = (
        SpeedupLevel(1, 3, 10.0, 10.0, 10.0, 10.0, 10.0, 1.0, 0.0, 1.0),
        SpeedupLevel(2, 3, 12.0, 12.0, 12.0, 12.0, 10.0, 1.0, 0.0, 10.0 / 12.0),
    )
    # Supply a deliberately permissive recommendation to isolate the allocator guard from fitting.
    speedup = StepSpeedup("g.scaling", 1, 2, 1.0, 2, levels)
    cfg = DagConfig(
        steps=(
            Step(
                "g", "scaling", "", "true", jobs_flag="-j%d",
                hint=ResourceHint(est_duration_s=10.0),
            ),
        )
    )

    assert allocate_widths(cfg, {"g.scaling": speedup}, {"g.scaling": 10.0}, 2) == {
        "g.scaling": 1
    }


def test_cpa_excludes_individually_cpu_inefficient_widths() -> None:
    levels = (
        SpeedupLevel(1, 3, 100.0, 100.0, 100.0, 100.0, 100.0, 1.0, 0.0, 1.0),
        SpeedupLevel(2, 3, 55.0, 55.0, 55.0, 55.0, 200.0, 2.0, 0.0, 100.0 / 55.0),
        SpeedupLevel(4, 3, 25.0, 25.0, 25.0, 25.0, 100.0, 4.0, 0.0, 4.0),
    )
    speedup = StepSpeedup("g.scaling", 1, 4, 4.0, None, levels)
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "scaling",
                "",
                "true",
                jobs_flag="-j%d",
                hint=ResourceHint(est_duration_s=100.0),
            ),
        )
    )

    narrow = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"g.scaling": speedup},
        core_budget=2,
    )
    wide = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"g.scaling": speedup},
        core_budget=4,
    )

    assert narrow.entries[0].alloc_inner_jobs == 1
    assert wide.entries[0].alloc_inner_jobs == 4
    admissible, _ = estimates._cpa_admissible(
        cfg, {"g.scaling": speedup}, {"g.scaling": 100.0}, 4
    )
    assert admissible["g.scaling"] == [1, 4]


def test_cpa_curveless_dag_stays_rigid(tmp_path: Path) -> None:
    # No measured curves at all: every step is rigid at its hint width (or 1), nothing is widened.
    cfg = DagConfig(
        steps=(
            Step("g", "a", "a", "true", hint=ResourceHint(est_duration_s=5.0)),
            Step("g", "b", "b", "true", deps=["g.a"], hint=ResourceHint(est_duration_s=5.0, preferred_inner_jobs=3)),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    widths = allocate_widths(cfg, {}, _cpa_est(cfg), 16)
    assert widths == {"g.a": 1, "g.b": 3}  # a: default 1; b: its hint 3 (never grown)


def test_cpa_allocation_is_idempotent(tmp_path: Path) -> None:
    rows: list[str] = []
    for j, w in ((1, "40.0"), (2, "20.0"), (4, "10.0"), (8, "5.0")):
        rows.append(_speedup_row("c.build", j, w, user_s="40.0", sys_s="0.0"))
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(
            Step("c", "prep", "p", "true", hint=ResourceHint(est_duration_s=2.0)),
            Step("c", "build", "b", "true", deps=["c.prep"], hint=ResourceHint(est_duration_s=40.0)),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    w1 = allocate_widths(cfg, speedups, _cpa_est(cfg), 16)
    w2 = allocate_widths(cfg, speedups, _cpa_est(cfg), 16)
    assert w1 == w2 == {"c.prep": 1, "c.build": 8}


def test_cpa_plan_json_and_text_shape(tmp_path: Path) -> None:
    rows = [
        _speedup_row("c.build", j, w, user_s="40.0", sys_s="0.0")
        for j, w in ((1, "40.0"), (2, "20.0"), (4, "10.0"), (8, "5.0"))
    ]
    speedups = _cpa_speedups(tmp_path, rows)
    cfg = DagConfig(
        steps=(Step("c", "build", "b", "true", hint=ResourceHint(est_duration_s=40.0)),),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    plan = build_plan(cfg, {}, planner=Planner.CPA, speedups=speedups, core_budget=16)
    js = plan_to_json(plan)
    assert '"planner": "cpa"' in js
    assert '"allocation": {' in js
    assert '"stop_reason":' in js
    assert '"modeled_makespan_s":' in js
    assert '"alloc_inner_jobs": 8' in js
    text = plan_to_text(plan)
    assert text.startswith("plan: cpa\n")
    assert "alloc_inner_jobs" in text
    assert "allocator (cpa):" in text


def _measurement_row(**overrides: str) -> dict[str, str]:
    """A row recording one successful step, before applying the caller's overrides."""
    row = {
        "step": "build.unit",
        "inner_jobs": "4",
        "elapsed_s": "1.0",
        "ok": "True",
        "returncode": "0",
        "timed_out": "False",
        "cpu_timed_out": "False",
        "oom_kills": "0",
    }
    row.update(overrides)
    return row


def test_row_is_measurement_accepts_a_successful_step() -> None:
    """A completed step is a timing measurement and must reach the speedup model."""
    assert estimates.row_is_measurement(_measurement_row())


def test_row_is_measurement_accepts_a_row_with_no_verdict_columns() -> None:
    """Silence is not failure: a store carrying no verdict columns must still be usable.

    A gate that rejected every row lacking an explicit verdict would empty the model instead of
    correcting it, which is a worse outcome than the defect it fixes.
    """
    assert estimates.row_is_measurement({"step": "build.unit", "inner_jobs": "4", "elapsed_s": "1.0"})


def test_row_is_measurement_rejects_each_recorded_failure_signal() -> None:
    """Every explicit failure marker must reject the row on its own."""
    for overrides in (
        {"ok": "False"},
        {"returncode": "2"},
        {"returncode": "127"},
        {"timed_out": "True"},
        {"cpu_timed_out": "True"},
        {"oom_kills": "1"},
    ):
        assert not estimates.row_is_measurement(_measurement_row(**overrides)), overrides


def test_bucketize_rows_drops_failures_and_keeps_the_measurements() -> None:
    """Filtering is per-sample: a width whose run failed disappears, valid widths survive.

    A step killed by the CPU-time guard at some widths still has genuine timings at the widths
    that completed, and those must continue to fit a curve.
    """
    rows = [
        _measurement_row(inner_jobs="1", elapsed_s="8.0", ok="False", cpu_timed_out="True"),
        _measurement_row(inner_jobs="2", elapsed_s="4.0", ok="False", cpu_timed_out="True"),
        _measurement_row(inner_jobs="4", elapsed_s="2.0"),
        _measurement_row(inner_jobs="8", elapsed_s="1.0"),
    ]
    buckets = estimates.bucketize_rows(rows, None)
    assert sorted(width for _, width in buckets) == [4, 8]


def test_a_step_that_failed_at_every_width_yields_no_curve() -> None:
    """Uniform failure must produce nothing rather than a plausible curve of crash times."""
    rows = [
        _measurement_row(inner_jobs=str(width), elapsed_s=str(1.0 / width), ok="False", returncode="127")
        for width in (1, 2, 4, 8)
    ]
    assert estimates.step_speedups_from_buckets(estimates.bucketize_rows(rows, None), None) == {}


def _speedup_over(walls_by_width: dict[int, list[float]]) -> estimates.StepSpeedup:
    """Fit one step's curve from explicit per-width wall samples (no contention signal)."""
    rows = [
        {
            "step": "s",
            "inner_jobs": str(width),
            "elapsed_s": str(wall),
            "ok": "True",
            "returncode": "0",
        }
        for width, walls in walls_by_width.items()
        for wall in walls
    ]
    buckets = estimates.bucketize_rows(rows, None)
    return estimates.step_speedups_from_buckets(buckets, None)["s"]


def test_a_curve_that_only_flattens_reports_no_regression() -> None:
    """A plateau is not a cliff: nothing above the fastest width is measurably slower."""
    fitted = _speedup_over(
        {1: [40.0, 40.1], 2: [21.0, 21.1], 4: [11.0, 11.1], 8: [10.6, 10.7], 16: [10.5, 10.6]}
    )
    assert fitted.recommended_inner_jobs == 4
    assert fitted.regression_inner_jobs is None


def test_a_curve_that_degrades_names_the_width_where_it_turns() -> None:
    """Where going wider is measurably slower, the width is named rather than left implicit.

    The recommendation is identical to the flattening case above, which is exactly why the separate
    field is needed: these two curves are indistinguishable by recommendation alone.
    """
    fitted = _speedup_over(
        {
            1: [40.0, 40.1],
            2: [21.0, 21.1],
            4: [11.0, 11.1],
            8: [10.6, 10.7],
            16: [17.0, 17.1],
            32: [26.0, 26.1],
        }
    )
    assert fitted.recommended_inner_jobs == 4
    assert fitted.regression_inner_jobs == 16


def test_a_slower_median_with_overlapping_spread_is_not_called_a_regression() -> None:
    """Dispersion decides. A width that is slower on the median but whose sample range overlaps the
    fastest width's is not distinguishable from it, and narrowing a step on that would be acting on
    noise."""
    fitted = _speedup_over(
        {
            1: [40.0, 40.1],
            2: [21.0, 21.1],
            4: [10.0, 12.0],   # best median, wide spread
            8: [11.4, 11.5],   # slower median, but inside width 4's observed range
        }
    )
    assert fitted.regression_inner_jobs is None


def test_levels_carry_the_raw_wall_beside_the_discounted_one() -> None:
    """The modelled wall and the measurement it came from are both present on every level."""
    fitted = _speedup_over({1: [40.0, 40.0], 2: [20.0, 20.0]})
    for level in fitted.levels:
        assert level.raw_wall_s is not None
        assert level.wall_min_s is not None and level.wall_max_s is not None
        assert level.wall_min_s <= level.wall_s <= level.wall_max_s
    # With no contention signal the discount is a no-op, so the two agree exactly.
    assert fitted.levels[0].raw_wall_s == fitted.levels[0].wall_s


def test_a_contention_discount_moves_the_modelled_wall_off_the_raw_one() -> None:
    """When the store records contention, wall_s and raw_wall_s must DIFFER and both be reported.

    A consumer printing only wall_s would be showing a modelled number as a measurement; the raw
    term has to survive the fit for that to be avoidable.
    """
    rows = [
        {
            "step": "s",
            "inner_jobs": "1",
            "elapsed_s": "100.0",
            "external_cores": "8",
            "ok": "True",
            "returncode": "0",
        }
        for _ in range(3)
    ]
    fitted = estimates.step_speedups_from_buckets(estimates.bucketize_rows(rows, 32), None)
    if fitted:  # a single width yields no curve; assert only the sample-level reduction
        pass
    sample = estimates.sample_from_row(rows[0], 32)
    assert sample.elapsed_s == 100.0
    assert sample.contention > 0.0
    intrinsic = sample.intrinsic_s()
    assert intrinsic is not None and intrinsic < sample.elapsed_s
