"""Tests for dagrun.sizing (synthetic, deterministic)."""

from __future__ import annotations

from unittest.mock import patch

from dagrun.model import DagConfig, ResourceHint, Step, StepClass
from dagrun.sizing import (
    PER_BUILD_JOB_MEM_BYTES,
    derive_build_jobs,
    jobs_footprint_bytes,
    jobs_for_budget,
    parse_size,
    schedulable_peak_mem_bytes,
    step_mem_cap_bytes,
    step_mem_cap_for_inner_jobs,
    stress_copy_footprint_bytes,
    transitive_deps,
)

GIB = 1024**3


def test_derive_build_jobs_caps_the_284_leak() -> None:
    """Three-part bracket for the CARGO_BUILD_JOBS/NUM_JOBS quota leak (<repo>#1584)."""
    # CAUGHT: an unpinned wide-quota step under an 8 GiB cap must be bounded by memory,
    # never the granted core count (the observed NUM_JOBS=284 -> OOM-killed linker).
    assert derive_build_jobs(284, 8 * GIB) == 8
    assert 8 == 8 * GIB // PER_BUILD_JOB_MEM_BYTES  # the bound is the #1584-safe j8

    # LEGITIMATE, N=3 (a mechanism that clamps EVERYTHING to 1 would pass the CAUGHT
    # assertion too, so these prove it does not over-constrain honest configs):
    assert derive_build_jobs(4, 64 * GIB) == 4  # (1) cpu-bound small box, unharmed
    assert derive_build_jobs(8, None) == 8  # (2) no mem cap -> cpu-bound, unharmed
    assert derive_build_jobs(32, 8 * GIB) == 8  # (3) mem-bound == #1584 safe width

    # FLOOR: never 0 / never negative, even when a cap fits less than one job.
    assert derive_build_jobs(284, 512 * 1024**2) == 1
    assert derive_build_jobs(1, 1) == 1


def _cfg() -> DagConfig:
    steps = (
        Step("g", "A", "", "true", hint=ResourceHint(rss_baseline_bytes=3 * GIB)),
        Step("g", "B", "", "true", deps=["g.A"], hint=ResourceHint(rss_baseline_bytes=2 * GIB)),
        Step("g", "C", "", "true", hint=ResourceHint(rss_baseline_bytes=4 * GIB, resources={"gpu": 1})),
        Step("g", "D", "", "true", hint=ResourceHint(rss_baseline_bytes=1 * GIB, resources={"gpu": 1})),
    )
    return DagConfig(
        steps=steps,
        resource_caps={"gpu": 1},
        mem_cap_factor=1.0,
        outer_mem_safety_factor=1.0,
        mem_cap_floor_bytes=0,
    )


def test_parse_size() -> None:
    assert parse_size("8G") == 8 * GIB
    assert parse_size("4096M") == 4096 * 1024**2
    assert parse_size("2048K") == 2048 * 1024
    assert parse_size("12345") == 12345
    assert parse_size(None) is None
    assert parse_size("nonsense") is None


def test_transitive_deps() -> None:
    deps = transitive_deps(list(_cfg().steps))
    assert deps["g.B"] == {"g.A"}
    assert deps["g.A"] == set()


def test_step_mem_cap_hard_override_wins() -> None:
    step = Step(
        "g",
        "X",
        "",
        "true",
        hint=ResourceHint(rss_baseline_bytes=2 * GIB, hard_mem_max_bytes=9 * GIB),
    )
    assert step_mem_cap_bytes(step, mem_cap_factor=1.25) == 9 * GIB


def test_schedulable_peak_picks_best_feasible_set() -> None:
    # gpu cap 1 forbids C+D together; A (3G) + C (4G) are independent -> 7G is the max.
    total, chosen = schedulable_peak_mem_bytes(_cfg(), jobs=4)
    assert total == 7 * GIB
    assert set(chosen) == {"g.A", "g.C"}


def test_jobs_for_budget_monotonic_and_at_least_one() -> None:
    cfg = _cfg()
    # -j1 footprint is the single largest cap (C = 4G).
    assert jobs_footprint_bytes(cfg, 1) == 4 * GIB
    # budget below the -j2 peak (7G) but >= -j1 (4G) yields exactly 1.
    assert jobs_for_budget(cfg, 6 * GIB) == (1, 4 * GIB)
    # A budget below even -j1 is infeasible and must refuse rather than running one step anyway.
    assert jobs_for_budget(cfg, 1 * GIB) == (0, 4 * GIB)


def test_jobs_for_budget_scales_each_cpu_bound_steps_effective_width() -> None:
    preferred = Step(
        "g",
        "preferred",
        "",
        "true",
        hint=ResourceHint(
            rss_baseline_bytes=GIB,
            classification=StepClass.CPU_BOUND,
            preferred_inner_jobs=8,
        ),
    )
    defaulted = Step(
        "g",
        "defaulted",
        "",
        "true",
        hint=ResourceHint(
            rss_baseline_bytes=GIB,
            classification=StepClass.CPU_BOUND,
        ),
    )
    cfg = DagConfig(
        steps=(preferred, defaulted),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
        default_step_cpu_count=8,
    )

    # The width model is linear above j4, so each j8 step costs 2 GiB and the pair costs 4 GiB.
    assert schedulable_peak_mem_bytes(cfg, jobs=2)[0] == 4 * GIB
    assert jobs_footprint_bytes(cfg, 1) == 2 * GIB
    assert jobs_for_budget(cfg, 3 * GIB) == (1, 2 * GIB)


def test_memory_classes_and_hard_cap_width_rules() -> None:
    cpu = Step(
        "g",
        "cpu",
        "",
        "true",
        hint=ResourceHint(rss_baseline_bytes=GIB, classification=StepClass.CPU_BOUND),
    )
    light = Step("g", "light", "", "true", hint=ResourceHint(rss_baseline_bytes=GIB))
    hard = Step(
        "g",
        "hard",
        "",
        "true",
        hint=ResourceHint(
            rss_baseline_bytes=GIB,
            hard_mem_max_bytes=3 * GIB,
            classification=StepClass.CPU_BOUND,
        ),
    )
    assert step_mem_cap_for_inner_jobs(cpu, 4, mem_cap_factor=1.0) == GIB
    assert step_mem_cap_for_inner_jobs(cpu, 8, mem_cap_factor=1.0) == 2 * GIB
    assert step_mem_cap_for_inner_jobs(light, 8, mem_cap_factor=1.0) == GIB
    assert step_mem_cap_for_inner_jobs(hard, 8, mem_cap_factor=1.0) == 3 * GIB


def test_sizing_counts_hard_default_and_selected_engine_steps() -> None:
    hard = Step(
        "g", "hard", "", "true", hint=ResourceHint(hard_mem_max_bytes=6 * GIB)
    )
    defaulted = Step(
        "g",
        "defaulted",
        "",
        "true",
        hint=ResourceHint(classification=StepClass.CPU_BOUND),
        engine_only=True,
    )
    cfg = DagConfig(
        steps=(hard, defaulted),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
        default_step_mem_cap_bytes=GIB,
        default_step_cpu_count=8,
    )
    assert schedulable_peak_mem_bytes(cfg, jobs=2)[0] == 8 * GIB
    assert jobs_for_budget(cfg, 5 * GIB) == (0, 6 * GIB)


def test_sizing_saturates_i64_instead_of_overflowing() -> None:
    maximum = 2**63 - 1
    cfg = DagConfig(
        steps=(
            Step("g", "a", "", "true", hint=ResourceHint(rss_baseline_bytes=maximum)),
            Step("g", "b", "", "true", hint=ResourceHint(rss_baseline_bytes=maximum)),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    assert schedulable_peak_mem_bytes(cfg, jobs=2)[0] == maximum
    assert jobs_for_budget(cfg, maximum) == (0, maximum)
    discounted = DagConfig(
        steps=cfg.steps,
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=0.5,
    )
    assert jobs_footprint_bytes(discounted, 2) == maximum

    unknown = DagConfig(
        steps=(Step("g", "unknown", "", "true"),),
        default_step_mem_cap_bytes=None,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=0.5,
    )
    assert jobs_footprint_bytes(unknown, 1) == maximum


def test_stress_footprint_uses_width_aware_runtime_caps() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "wide",
                "",
                "true",
                hint=ResourceHint(
                    rss_baseline_bytes=GIB,
                    classification=StepClass.CPU_BOUND,
                    preferred_inner_jobs=8,
                ),
            ),
        ),
        mem_cap_factor=1.0,
    )
    assert stress_copy_footprint_bytes(cfg) == 2 * GIB


def test_invalid_nonpositive_memory_hints_fall_back_safely() -> None:
    invalid = Step(
        "g",
        "invalid",
        "",
        "true",
        hint=ResourceHint(rss_baseline_bytes=0, hard_mem_max_bytes=0),
    )
    assert step_mem_cap_bytes(invalid, mem_cap_factor=1.0, default_cap_bytes=GIB) == GIB
    baseline = Step(
        "g", "factor", "", "true", hint=ResourceHint(rss_baseline_bytes=8 * GIB)
    )
    assert step_mem_cap_bytes(baseline, mem_cap_factor=0.0, default_cap_bytes=GIB) == GIB
    assert step_mem_cap_bytes(baseline, mem_cap_factor=1e-300, default_cap_bytes=GIB) == 1
    cfg = DagConfig(
        steps=(baseline,),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=-1,
        outer_mem_safety_factor=0.0,
    )
    assert jobs_for_budget(cfg, 16 * GIB) == (0, 2**63 - 1)


def test_wide_dag_uses_bounded_conservative_memory_fallback() -> None:
    cfg = DagConfig(
        steps=tuple(
            Step(
                "wide",
                f"s{index:02d}",
                "",
                "true",
                hint=ResourceHint(hard_mem_max_bytes=GIB),
            )
            for index in range(51)
        ),
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    total, chosen = schedulable_peak_mem_bytes(cfg, jobs=51)
    assert total == 51 * GIB
    assert chosen == tuple(f"wide.s{index:02d}" for index in range(51))


def test_stress_copy_footprint_sums_declared_and_default_charges() -> None:
    # Per-copy footprint sums each step's cap: a declared hard cap verbatim, an rss_baseline
    # scaled by mem_cap_factor, and an UNDECLARED step charged the SMALL default (1 GiB).
    from dagrun.model import DEFAULT_SMALL_MEM_CAP_BYTES, DagConfig, ResourceHint, Step
    cfg = DagConfig(
        steps=(
            Step("g", "hard", "d", "true", hint=ResourceHint(hard_mem_max_bytes=2 * 1024**3)),
            Step("g", "base", "d", "true", hint=ResourceHint(rss_baseline_bytes=1024**3)),
            Step("g", "bare", "d", "true"),  # undeclared -> SMALL default charge
        ),
        mem_cap_factor=1.25,
    )
    got = stress_copy_footprint_bytes(cfg)
    expected = (2 * 1024**3) + int(1024**3 * 1.25) + DEFAULT_SMALL_MEM_CAP_BYTES
    assert got == expected


def test_stress_copy_footprint_single_node_is_that_node_cap() -> None:
    from dagrun.model import DagConfig, ResourceHint, Step
    cfg = DagConfig(
        steps=(Step("dbi", "file_metadata", "d", "true",
                    hint=ResourceHint(hard_mem_max_bytes=3 * 1024**3)),)
    )
    assert stress_copy_footprint_bytes(cfg) == 3 * 1024**3


def test_stress_copy_footprint_keeps_configured_control_plane_floor() -> None:
    cfg = DagConfig(
        steps=(Step("g", "tiny", "", "true", hint=ResourceHint(hard_mem_max_bytes=1)),),
        default_step_mem_cap_bytes=GIB,
        mem_cap_floor_bytes=0,
    )
    assert stress_copy_footprint_bytes(cfg) == GIB


def test_stress_without_default_only_marks_uncharacterized_steps_unbounded() -> None:
    characterized = DagConfig(
        steps=(
            Step("g", "hard", "", "true", hint=ResourceHint(hard_mem_max_bytes=2 * GIB)),
            Step("g", "rss", "", "true", hint=ResourceHint(rss_baseline_bytes=3 * GIB)),
        ),
        mem_cap_factor=1.0,
        default_step_mem_cap_bytes=None,
        mem_cap_floor_bytes=0,
    )
    assert stress_copy_footprint_bytes(characterized) == 5 * GIB

    uncharacterized = DagConfig(
        steps=(Step("g", "bare", "", "true"),),
        default_step_mem_cap_bytes=None,
    )
    assert stress_copy_footprint_bytes(uncharacterized) == 2**63 - 1


def test_empty_stress_footprint_uses_default_then_floor() -> None:
    with_default = DagConfig(steps=(), default_step_mem_cap_bytes=GIB, mem_cap_floor_bytes=2 * GIB)
    assert stress_copy_footprint_bytes(with_default) == GIB
    floor_only = DagConfig(steps=(), default_step_mem_cap_bytes=None, mem_cap_floor_bytes=2 * GIB)
    assert stress_copy_footprint_bytes(floor_only) == 2 * GIB


def test_transitive_deps_handles_1100_node_reverse_chain_iteratively() -> None:
    steps = tuple(
        Step(
            "chain",
            f"s{index}",
            "",
            "true",
            deps=[f"chain.s{index - 1}"] if index else [],
        )
        for index in reversed(range(1100))
    )
    deps = transitive_deps(steps)
    assert len(deps["chain.s1099"]) == 1099
    assert "chain.s0" in deps["chain.s1099"]
    assert deps["chain.s0"] == set()

    # Wide sizing takes the bounded conservative fallback before computing any closure.
    cfg = DagConfig(
        steps=steps,
        default_step_mem_cap_bytes=1,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    assert schedulable_peak_mem_bytes(cfg, 1100)[0] == 1100


def test_width_one_sizing_skips_closure_on_5000_node_reverse_chain() -> None:
    steps = tuple(
        Step(
            "wide",
            f"s{index}",
            "",
            "true",
            deps=[f"wide.s{index - 1}"] if index else [],
            hint=ResourceHint(hard_mem_max_bytes=2 if index in (4999, 4000) else 1),
        )
        for index in reversed(range(5000))
    )
    assert schedulable_peak_mem_bytes(DagConfig(steps=steps), 1) == (2, ("wide.s4999",))


def test_box_mem_budget_is_min_of_readable_signals() -> None:
    # Use one deterministic snapshot. Comparing the function's MemAvailable read
    # to a later live /proc read races with other processes on the host.
    import dagrun.sizing as sizing

    cases = (
        (8 * GIB, 6 * GIB, 6 * GIB),
        (8 * GIB, None, 8 * GIB),
        (None, 6 * GIB, 6 * GIB),
        (None, None, None),
    )
    for cgroup_limit, available, expected in cases:
        with (
            patch.object(sizing, "cgroup_mem_max_bytes", return_value=cgroup_limit),
            patch.object(sizing, "mem_available_bytes", return_value=available),
        ):
            assert sizing.box_mem_budget_bytes() == expected
