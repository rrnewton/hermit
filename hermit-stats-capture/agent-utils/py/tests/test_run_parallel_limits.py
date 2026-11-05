"""Focused tests for independent active-step and per-step CPU-width run limits."""

from __future__ import annotations

import argparse
import dataclasses
import os
import shlex
import time
from collections.abc import Callable
from pathlib import Path

import pytest

from dagrun import (
    DagConfig,
    InfeasibleAllocationError,
    Runner,
    ResourceHint,
    RunResult,
    SpeedupLevel,
    Step,
    StepSpeedup,
    allocate_widths,
    apply_plan_to_config,
    build_plan,
    cap_config_cpu_jobs,
    cap_config_max_cpus,
    dag_to_json,
    run_dag_limited,
    schedulable_peak_mem_bytes,
)
from dagrun import cgroup, perflog, profile_enrich
from dagrun.cgroup import NoopCgroups
from dagrun.cli import (
    MAX_RUN_CPUS,
    MAX_STRESS_GENERATED_NODES,
    _inherited_cores_error,
    _planning_budgets,
    _select_max_cpus,
    _select_max_mem_bytes,
    _select_max_steps,
    _stress_expansion_guard,
    _stress_footprints,
    _sweep_cpu_error,
    _sweep_memory_error,
    _uses_inherited_memory_budget,
    build_parser,
    main,
)
from dagrun.estimates import Planner
from dagrun.model import IntentionalSkipReason, StepClass
from dagrun.sizing import stress_copy_footprint_bytes


def _run_args(**overrides: object) -> argparse.Namespace:
    values: dict[str, object] = {
        "max_cpus": None,
        "max_steps": None,
        "max_mem": None,
        "cores": None,
    }
    values.update(overrides)
    return argparse.Namespace(**values)


def _sleep_cfg(*, count: int = 4, width: int = 1) -> DagConfig:
    return DagConfig(
        steps=tuple(
            Step(
                "g",
                f"s{index}",
                "",
                "sleep 0.2",
                hint=ResourceHint(preferred_inner_jobs=width),
                jobs_flag="",
            )
            for index in range(count)
        )
    )


def test_run_parser_separates_bare_max_steps_and_max_cpus() -> None:
    parsed = build_parser().parse_args(["run", "--dag", "dag.json", "-s2", "-j8"])
    assert parsed.max_steps == 2
    assert parsed.max_cpus == 8


def test_stress_guard_sizes_the_cpu_capped_pre_expansion_graph(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    gib = 1024**3
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "wide",
                "",
                "true",
                hint=ResourceHint(
                    rss_baseline_bytes=gib,
                    classification=StepClass.CPU_BOUND,
                    preferred_inner_jobs=128,
                ),
                jobs_flag="-j%d",
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    assert stress_copy_footprint_bytes(cfg) == 32 * gib
    dag = tmp_path / "dag.json"
    dag.write_text(dag_to_json(cfg), encoding="utf-8")
    observed: list[DagConfig] = []

    def fake_stress_guard(sized: DagConfig, copies: int) -> int:
        observed.append(sized)
        assert copies == 2
        assert len(sized.steps) == 1
        assert sized.steps[0].hint.preferred_inner_jobs == 2
        assert stress_copy_footprint_bytes(sized) == gib
        return 0

    def fake_final_guard(sized: DagConfig, copies: int) -> int:
        assert copies == 2
        assert len(sized.steps) == 2
        assert all(step.hint.preferred_inner_jobs == 2 for step in sized.steps)
        return 0

    def fake_run(sized: DagConfig, **_kwargs: object) -> RunResult:
        assert len(sized.steps) == 2
        assert all(step.hint.preferred_inner_jobs == 2 for step in sized.steps)
        return RunResult(ok=True, wall_s=0.0)

    monkeypatch.setattr("dagrun.cli._stress_guard", fake_stress_guard)
    monkeypatch.setattr("dagrun.cli._final_stress_guard", fake_final_guard)
    monkeypatch.setattr("dagrun.cli.run_dag_limited", fake_run)
    assert (
        main(
            [
                "run",
                "--dag",
                str(dag),
                "--stress",
                "2",
                "--max-cpus",
                "2",
                "--unsafe-no-cgroups",
                "--no-profile",
                "--no-profile-feedback",
                "--quiet",
            ]
        )
        == 0
    )
    assert len(observed) == 1
    assert capsys.readouterr().err.count("preferred_inner_jobs=128 exceeds --max-cpus 2") == 1


def test_final_stress_footprint_does_not_multiply_an_expanded_graph_twice() -> None:
    gib = 1024**3
    one = DagConfig(
        steps=(Step("g", "one", "", "true", hint=ResourceHint(rss_baseline_bytes=gib)),),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    expanded = dataclasses.replace(
        one,
        steps=(
            one.steps[0],
            dataclasses.replace(one.steps[0], job="two"),
        ),
    )
    assert _stress_footprints(one, 2, expanded=False) == (gib, 2 * gib)
    assert _stress_footprints(expanded, 2, expanded=True) == (2 * gib, 2 * gib)

    # Even when characterized steps claim one byte, each generated copy retains the configured
    # 1-GiB control-plane floor in the final already-expanded check.
    tiny = dataclasses.replace(
        one,
        steps=(dataclasses.replace(one.steps[0], hint=ResourceHint(hard_mem_max_bytes=1)),),
    )
    tiny_expanded = dataclasses.replace(
        tiny,
        steps=(tiny.steps[0], dataclasses.replace(tiny.steps[0], job="two")),
    )
    assert _stress_footprints(tiny, 2, expanded=False) == (gib, 2 * gib)
    assert _stress_footprints(tiny_expanded, 2, expanded=True) == (gib, 2 * gib)


def test_stress_expansion_guard_bounds_generated_nodes_before_allocation(
    capsys: pytest.CaptureFixture[str],
) -> None:
    cfg = DagConfig(
        steps=(Step("g", "tiny", "", "true", hint=ResourceHint(hard_mem_max_bytes=1)),)
    )
    assert _stress_expansion_guard(cfg, MAX_STRESS_GENERATED_NODES) == 0
    assert _stress_expansion_guard(cfg, MAX_STRESS_GENERATED_NODES + 1) == 2
    assert (
        f"expansion would create {MAX_STRESS_GENERATED_NODES + 1} generated DAG nodes/control "
        f"units, exceeding safety limit {MAX_STRESS_GENERATED_NODES}"
        in capsys.readouterr().err
    )


def test_final_stress_guard_refuses_profile_raised_rss_before_spawn(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setenv("DAGRUN_MACHINE_ID", "stress_final")
    monkeypatch.setenv("DAGRUN_CONTAINER_CLASS", "test")
    perf_dir = tmp_path / "perf"
    perf_dir.mkdir()
    profile = perf_dir / "step_profiles_stress_final_test.csv"
    profile.write_text(
        "step,inner_jobs,elapsed_s,peak_bytes,ok,returncode,timed_out,oom_kills\n"
        f"g.learned#1,1,1.0,{2**63 - 1},True,0,False,0\n"
        f"g.learned#2,1,1.0,{2**63 - 1},True,0,False,0\n",
        encoding="utf-8",
    )
    marker = tmp_path / "spawned"
    dag = tmp_path / "dag.json"
    dag.write_text(
        dag_to_json(
            DagConfig(
                steps=(
                    Step(
                        "g",
                        "learned",
                        "",
                        f"touch {shlex.quote(str(marker))}",
                        hint=ResourceHint(rss_baseline_bytes=1, preferred_inner_jobs=1),
                        jobs_flag="",
                    ),
                ),
                mem_cap_factor=1.0,
                mem_cap_floor_bytes=0,
            )
        ),
        encoding="utf-8",
    )

    rc = main(
        [
            "run",
            "--dag",
            str(dag),
            "--stress",
            "2",
            "--max-cpus",
            "1",
            "--perf-dir",
            str(perf_dir),
            "--no-profile",
            "--unsafe-no-cgroups",
            "--quiet",
        ]
    )

    assert rc == 2
    assert not marker.exists()
    assert "final planned expanded-graph memory footprint is unbounded" in capsys.readouterr().err


@pytest.mark.parametrize(
    "args",
    [
        ["run", "--dag", "dag.json", "--max-steps", "0"],
        ["run", "--dag", "dag.json", "--max-cpus", "0"],
        ["run", "--dag", "dag.json", "--max-cpus", str(MAX_RUN_CPUS + 1)],
        ["run", "--dag", "dag.json", "--jobs", "0"],
    ],
)
def test_run_parser_rejects_invalid_limits(args: list[str]) -> None:
    with pytest.raises(SystemExit) as exc:
        build_parser().parse_args(args)
    assert exc.value.code == 2


def test_run_help_names_both_independent_limits(capsys: pytest.CaptureFixture[str]) -> None:
    with pytest.raises(SystemExit) as exc:
        build_parser().parse_args(["run", "--help"])
    assert exc.value.code == 0
    help_text = capsys.readouterr().out
    normalized = " ".join(help_text.split())
    assert "maximum active DAG steps" in help_text
    assert "outer CPU-bandwidth limit" in help_text
    assert "maximum width of any one runner-controlled step" in normalized
    assert "shared 90% slice" in normalized
    assert "--max-cpus" in help_text
    assert "--jobs" not in help_text


def test_hidden_run_jobs_alias_and_conflict() -> None:
    legacy = build_parser().parse_args(
        ["run", "--dag", "dag.json", "--jobs", "7"]
    )
    assert legacy.max_cpus == 7
    equal = build_parser().parse_args(
        ["run", "--dag", "dag.json", "--max-cpus", "7", "--jobs", "7"]
    )
    assert equal.max_cpus == 7
    with pytest.raises(SystemExit) as exc:
        build_parser().parse_args(
            ["run", "--dag", "dag.json", "--max-cpus", "7", "--jobs", "8"]
        )
    assert exc.value.code == 2


def test_default_max_cpus_takes_tightest_container_affinity_and_shared_slice(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("dagrun.cli.container_core_budget", lambda: 8)
    monkeypatch.setattr(cgroup, "aggregate_slice_max_cpus", lambda: 6)
    assert _select_max_cpus(_run_args()) == 6
    assert _select_max_cpus(_run_args(max_cpus=10)) == 10
    assert _select_max_cpus(_run_args(max_cpus=10, cores=3)) == 3


def test_python_api_keeps_cpu_jobs_compatibility_aliases() -> None:
    cfg = DagConfig(steps=(Step("g", "one", "", "true"),))
    canonical = Runner(
        cfg, max_steps=1, max_cpus=2, cgroups=NoopCgroups(), verbosity=0
    )
    legacy = Runner(
        cfg, max_steps=1, cpu_jobs=2, cgroups=NoopCgroups(), verbosity=0
    )
    assert canonical.max_cpus == legacy.max_cpus == 2
    assert canonical.cpu_jobs == legacy.cpu_jobs == 2
    assert (
        Runner(
            cfg,
            max_steps=1,
            max_cpus=2,
            cpu_jobs=2,
            cgroups=NoopCgroups(),
            verbosity=0,
        ).max_cpus
        == 2
    )
    assert cap_config_cpu_jobs(cfg, 2) == cap_config_max_cpus(cfg, 2)
    assert run_dag_limited(cfg, max_steps=1, cpu_jobs=2, verbosity=0).ok
    assert run_dag_limited(
        cfg, max_steps=1, max_cpus=2, cpu_jobs=2, verbosity=0
    ).ok
    with pytest.raises(TypeError, match="disagree"):
        run_dag_limited(
            cfg, max_steps=1, max_cpus=2, cpu_jobs=3, verbosity=0
        )
    with pytest.raises(TypeError, match="disagree"):
        Runner(
            cfg,
            max_steps=1,
            max_cpus=2,
            cpu_jobs=3,
            cgroups=NoopCgroups(),
            verbosity=0,
        )


def test_an_absent_step_ceiling_defaults_to_the_resolved_cpu_budget() -> None:
    """#36 dag-core-budget-split: an absent ``-s`` follows the RESOLVED ``-j``, at any size.

    ``test_max_mem_and_explicit_max_steps_use_tighter_ceiling`` already pins the default at 7,
    but it hands ``_select_max_steps`` a bare 7 rather than resolving a typed flag, and 7 is
    small enough that a clamp on the default (``min(max_cpus, 8)`` is the one-token version)
    would leave it green.  The README and the user guide tell a user that ``run -j200`` alone
    permits two hundred active nodes, so that literal magnitude is pinned here, resolved from
    the flag a user actually types.  The Rust edition pins the same numbers in
    ``an_absent_step_ceiling_defaults_to_the_resolved_cpu_budget``.

    Every value below is written literally; none is read back out of the constant it pins.
    """
    cfg = _sleep_cfg(count=1)

    two_hundred = build_parser().parse_args(["run", "--dag", "dag.json", "-j", "200"])
    assert two_hundred.max_steps is None
    assert _select_max_cpus(two_hundred) == 200
    assert _select_max_steps(cfg, two_hundred, 200) == 200

    # The hidden 0.13 alias resolves to --max-cpus first and therefore sets the same default.
    legacy = build_parser().parse_args(["run", "--dag", "dag.json", "--jobs", "200"])
    assert legacy.max_steps is None
    assert _select_max_cpus(legacy) == 200
    assert _select_max_steps(cfg, legacy, 200) == 200

    # Nothing runs in the other direction: an explicit -s is not the CPU budget.
    both = build_parser().parse_args(["run", "--dag", "dag.json", "-j", "3", "-s", "5"])
    assert _select_max_cpus(both) == 3
    assert _select_max_steps(cfg, both, 3) == 5


def test_delegated_parent_clamps_explicit_and_default_run_budgets(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    gib = 1024**3
    monkeypatch.setattr("dagrun.cli.container_core_budget", lambda: 16)
    monkeypatch.setattr(cgroup, "aggregate_slice_max_cpus", lambda: 12)
    explicit = _run_args(max_cpus=8, max_mem="8G")
    assert _select_max_cpus(explicit, inherited_max_cpus=2) == 2
    assert _select_max_mem_bytes(explicit, inherited_max_mem_bytes=2 * gib) == 2 * gib

    defaults = _run_args()
    assert _select_max_cpus(defaults, inherited_max_cpus=2) == 2
    assert _select_max_mem_bytes(defaults, inherited_max_mem_bytes=2 * gib) == 2 * gib
    assert _uses_inherited_memory_budget(defaults, 2 * gib)
    assert _uses_inherited_memory_budget(explicit, 2 * gib)
    assert not _uses_inherited_memory_budget(_run_args(max_mem="1G"), 2 * gib)
    assert (
        _select_max_mem_bytes(
            _run_args(max_mem="not-a-size"), inherited_max_mem_bytes=2 * gib
        )
        == 2 * gib
    )
    assert (
        _inherited_cores_error(_run_args(cores=8), inherited_max_cpus=2)
        == "--cores 8 requests an exact reservation wider than the inherited delegated CPU "
        "ceiling 2; refusing"
    )

    evidence = capsys.readouterr().err
    assert evidence.count("inherited delegated CPU ceiling 2") == 2
    assert "--max-mem 8589934592 bytes exceeds inherited delegated memory ceiling" in evidence
    assert "inherited delegated memory ceiling 2147483648 bytes is the effective default" in evidence
    assert "could not parse --max-mem 'not-a-size'" in evidence

    assert _sweep_cpu_error(8, 2) is not None
    assert _sweep_cpu_error(2, 2) is None
    assert _sweep_memory_error(_sleep_cfg(count=1).steps[0], _sleep_cfg(count=1), 8, 1) is not None

    # The 8 GiB top-level modeling floor is not a claim that a 1 GiB step cannot run in a
    # deliberately smaller parent. Clamp only that run-level floor; real step caps still decide.
    cfg = _sleep_cfg(count=4)
    assert _select_max_steps(cfg, defaults, 2, 2 * gib) == 0
    assert (
        _select_max_steps(
            cfg,
            defaults,
            2,
            2 * gib,
            inherited_memory_budget=True,
        )
        == 2
    )


def test_max_mem_and_explicit_max_steps_use_tighter_ceiling(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    cfg = _sleep_cfg(count=1)
    monkeypatch.setattr("dagrun.cli.jobs_for_budget", lambda _cfg, _budget: (5, 123))
    assert _select_max_steps(cfg, _run_args(), 7) == 7
    assert _select_max_steps(cfg, _run_args(max_mem="1G"), 7) == 5
    assert _select_max_steps(cfg, _run_args(max_steps=3, max_mem="1G"), 7) == 3
    assert _select_max_steps(cfg, _run_args(max_steps=9, max_mem="1G"), 7) == 5


def test_nonbinding_max_mem_cannot_loosen_default_max_steps_past_max_cpus(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    cfg = _sleep_cfg(count=1)
    monkeypatch.setattr("dagrun.cli.jobs_for_budget", lambda _cfg, _budget: (316, 123))

    assert _select_max_steps(cfg, _run_args(max_mem="1G"), 2) == 2
    evidence = capsys.readouterr().err
    assert "modeled memory ceiling 316 active steps" in evidence
    assert "base active-step ceiling 2" in evidence
    assert "final --max-steps 2" in evidence


def test_max_mem_refuses_when_one_step_cannot_fit(
    capsys: pytest.CaptureFixture[str],
) -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "large",
                "",
                "true",
                hint=ResourceHint(rss_baseline_bytes=2 * 1024**3),
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
        outer_mem_safety_factor=1.0,
    )
    assert _select_max_steps(cfg, _run_args(max_mem="1G"), 7) == 0
    assert "REFUSED" in capsys.readouterr().err


def test_cpa_infeasible_memory_never_spawns(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    marker = tmp_path / "spawned"
    dag = tmp_path / "dag.json"
    dag.write_text(
        dag_to_json(
            DagConfig(
                steps=(
                    Step(
                        "g",
                        "large",
                        "",
                        f"touch {shlex.quote(str(marker))}",
                        hint=ResourceHint(rss_baseline_bytes=6 * 1024**3),
                    ),
                ),
                mem_cap_factor=1.0,
                mem_cap_floor_bytes=0,
                outer_mem_safety_factor=1.0,
            )
        ),
        encoding="utf-8",
    )

    rc = main(
        [
            "run",
            "--dag",
            str(dag),
            "--planner",
            "cpa",
            "--max-mem",
            "4G",
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ]
    )

    assert rc == 2
    assert not marker.exists()
    assert "infeasible under --max-mem" in capsys.readouterr().err


def test_max_steps_governs_overlap_while_max_cpus_caps_each_step() -> None:
    step_limited = run_dag_limited(
        _sleep_cfg(), max_steps=2, max_cpus=4, verbosity=0
    )
    assert step_limited.ok
    assert step_limited.max_concurrent_steps == 2

    cpu_limited = run_dag_limited(
        _sleep_cfg(), max_steps=4, max_cpus=2, verbosity=0
    )
    assert cpu_limited.ok
    assert cpu_limited.max_concurrent_steps == 4

    overcommitted_widths = run_dag_limited(
        _sleep_cfg(width=2), max_steps=4, max_cpus=4, verbosity=0
    )
    assert overcommitted_widths.ok
    assert overcommitted_widths.max_concurrent_steps == 4


def test_default_step_cpu_count_does_not_reduce_step_overlap() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "one",
                "",
                'check() { [ "$#" -eq 0 ]; }; check; sleep 0.2',
                hint=ResourceHint(preferred_inner_jobs=0),
            ),
            Step(
                "g",
                "two",
                "",
                'check() { [ "$#" -eq 0 ]; }; check; sleep 0.2',
                hint=ResourceHint(preferred_inner_jobs=0),
            ),
        ),
        default_step_cpu_count=4,
    )
    result = run_dag_limited(cfg, max_steps=2, max_cpus=4, verbosity=0)
    assert result.ok
    assert result.max_concurrent_steps == 2


def test_default_step_profiles_the_effective_cpu_width() -> None:
    cfg = DagConfig(
        steps=(Step("g", "default-width", "", "true"),),
        default_step_cpu_count=1,
    )
    result = run_dag_limited(
        cfg,
        max_steps=1,
        max_cpus=8,
        cgroups=_BoxedRecordingCgroups(),
        verbosity=0,
    )
    assert result.ok
    assert len(result.step_profile_rows) == 1
    assert result.step_profile_rows[0]["inner_jobs"] == 1
    assert result.step_profile_rows[0]["quota_utilization_pct"] == 0.0


class _RecordingCgroups(NoopCgroups):
    def __init__(self) -> None:
        self.cpu_counts: list[int | None] = []
        self.mem_caps: list[int | None] = []

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        self.cpu_counts.append(cpu_count)
        self.mem_caps.append(mem_max)
        return cmd


class _BoxedRecordingCgroups(_RecordingCgroups):
    enabled = True

    def cpu_stats(self, tag: str) -> dict[str, int]:
        return {
            "usage_usec": 0,
            "user_usec": 0,
            "system_usec": 0,
            "throttled_usec": 0,
        }


def test_oversized_width_jobs_flag_and_default_cpu_cap_are_clamped(
    capsys: pytest.CaptureFixture[str],
) -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "wide",
                "",
                'check() { [ "$*" = "-j2" ]; }; check',
                hint=ResourceHint(preferred_inner_jobs=8),
                jobs_flag="-j%d",
            ),
        ),
        default_step_cpu_count=8,
    )
    capped = cap_config_max_cpus(cfg, 2)
    assert capped.steps[0].hint.preferred_inner_jobs == 2
    assert capped.default_step_cpu_count == 2

    manager = _RecordingCgroups()
    result = run_dag_limited(
        cfg, max_steps=1, max_cpus=2, cgroups=manager, verbosity=0
    )
    assert result.ok
    assert manager.cpu_counts == [2]
    warnings = capsys.readouterr().err
    assert "preferred_inner_jobs=8" in warnings
    assert "default_step_cpu_count=8" in warnings


def test_runtime_memory_caps_scale_with_preferred_and_default_widths() -> None:
    gib = 1024**3
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "preferred",
                "",
                "true",
                hint=ResourceHint(
                    rss_baseline_bytes=gib,
                    classification=StepClass.CPU_BOUND,
                    preferred_inner_jobs=8,
                ),
            ),
            Step(
                "g",
                "defaulted",
                "",
                "true",
                hint=ResourceHint(
                    rss_baseline_bytes=gib,
                    classification=StepClass.CPU_BOUND,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        default_step_cpu_count=8,
    )
    manager = _BoxedRecordingCgroups()

    result = run_dag_limited(
        cfg, max_steps=2, max_cpus=8, cgroups=manager, verbosity=0
    )

    assert result.ok
    assert manager.cpu_counts == [8, 8]
    assert manager.mem_caps == [2 * gib, 2 * gib]


def test_runtime_nonpositive_memory_hints_use_positive_default() -> None:
    gib = 1024**3
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "invalid",
                "",
                "true",
                hint=ResourceHint(
                    rss_baseline_bytes=0,
                    hard_mem_max_bytes=0,
                    classification=StepClass.CPU_BOUND,
                    preferred_inner_jobs=8,
                ),
            ),
        ),
        mem_cap_factor=1.0,
        default_step_mem_cap_bytes=gib,
    )
    manager = _BoxedRecordingCgroups()

    result = run_dag_limited(
        cfg, max_steps=1, max_cpus=8, cgroups=manager, verbosity=0
    )

    assert result.ok
    assert manager.mem_caps == [2 * gib]


def test_overbudget_self_managed_width_refuses_before_spawn(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    marker = tmp_path / "spawned"
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "fixed",
                "",
                f"touch {shlex.quote(str(marker))}",
                hint=ResourceHint(preferred_inner_jobs=8),
                jobs_flag="",
            ),
        )
    )

    # A truthful cap helper leaves a self-managed width unchanged: it cannot rewrite the command.
    capped = cap_config_max_cpus(cfg, 2)
    assert capped.steps[0].hint.preferred_inner_jobs == 8
    result = run_dag_limited(cfg, max_steps=1, max_cpus=2, verbosity=0)
    assert not result.ok
    assert result.max_concurrent_steps == 0
    assert not marker.exists()
    assert "cannot lower guest parallelism" in capsys.readouterr().err
    # The condition widened: a step is unresizable when it offers NEITHER a jobs_flag NOR an
    # env channel. The message must still name the step AND tell the operator which knob to
    # set, so this asserts both rather than a loose substring.
    with pytest.raises(ValueError, match="offer no width channel"):
        Runner(cfg, max_steps=1, max_cpus=2, cgroups=NoopCgroups(), verbosity=0)

    # An intentional pre-execution skip can never spawn, so its dormant width must not reject the
    # run or erase the typed skip record.
    skipped = DagConfig(
        steps=(
            Step(
                "g",
                "skipped",
                "",
                f"touch {shlex.quote(str(marker))}",
                hint=ResourceHint(preferred_inner_jobs=8),
                jobs_flag="",
                skip_reason=IntentionalSkipReason.EMPTY_MANIFEST_BUCKET,
            ),
        )
    )
    skipped_result = run_dag_limited(
        skipped, max_steps=1, max_cpus=2, verbosity=0
    )
    assert skipped_result.ok
    assert skipped_result.intentional_skips == (
        ("g.skipped", IntentionalSkipReason.EMPTY_MANIFEST_BUCKET),
    )
    assert not marker.exists()


def test_run_cli_refuses_overbudget_self_managed_width_before_cgroup(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    marker = tmp_path / "spawned"
    dag = tmp_path / "dag.json"
    dag.write_text(
        dag_to_json(
            DagConfig(
                steps=(
                    Step(
                        "g",
                        "fixed",
                        "",
                        f"touch {shlex.quote(str(marker))}",
                        hint=ResourceHint(preferred_inner_jobs=8),
                        jobs_flag="",
                    ),
                )
            )
        ),
        encoding="utf-8",
    )
    rc = main(["run", "--dag", str(dag), "--max-cpus", "2", "--quiet"])
    assert rc == 2
    assert not marker.exists()
    assert "cannot lower guest parallelism" in capsys.readouterr().err


def test_plan_application_preserves_top_level_cpu_policy_before_clamp() -> None:
    cfg = DagConfig(steps=(Step("g", "one", "", "true"),), default_step_cpu_count=8)
    applied = apply_plan_to_config(cfg, build_plan(cfg, {}))
    assert applied.default_step_cpu_count == 8
    assert cap_config_max_cpus(applied, 2).default_step_cpu_count == 2


def test_small_default_cap_is_applied_after_run_cpu_job_clamp(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    path = tmp_path / "dag.json"
    path.write_text(
        dag_to_json(
            DagConfig(
                steps=(Step("g", "one", "", "true"),),
                default_step_cpu_count=8,
            )
        )
    )
    observed: list[int | None] = []

    def fake_run(cfg: DagConfig, **_kwargs: object) -> RunResult:
        observed.append(cfg.default_step_cpu_count)
        return RunResult(ok=True, wall_s=0.0)

    monkeypatch.setattr("dagrun.cli.run_dag_limited", fake_run)
    rc = main(
        [
            "run",
            "--dag",
            str(path),
            "--max-cpus",
            "2",
            "--max-steps",
            "1",
            "--small-default-cap",
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "--quiet",
        ]
    )
    assert rc == 0
    assert observed == [1]


@pytest.mark.parametrize(
    ("quota", "affinity", "expected"),
    [
        ("50000_100000", 8, 1),
        ("150000_100000", 8, 1),
        ("250000_100000", 8, 2),
        ("800000_100000", 3, 3),
    ],
)
def test_container_core_budget_floors_fractional_quota_and_mins_affinity(
    monkeypatch: pytest.MonkeyPatch, quota: str, affinity: int, expected: int
) -> None:
    monkeypatch.setattr(perflog, "effective_cpu_quota", lambda: quota)
    monkeypatch.setattr(perflog, "nproc", lambda: affinity)
    assert profile_enrich.container_core_budget() == expected


@pytest.mark.parametrize(("percent", "expected"), [(50, 1), (150, 1), (250, 2)])
def test_shared_slice_max_cpus_floors_fractional_quota(
    monkeypatch: pytest.MonkeyPatch, percent: int, expected: int
) -> None:
    monkeypatch.setattr(cgroup, "cpu_quota_percent", lambda: percent)
    assert cgroup.aggregate_slice_max_cpus() == expected
    assert cgroup.aggregate_slice_cpu_jobs() == expected


def test_cpa_omits_curve_entirely_above_strict_cpu_budget() -> None:
    speedup = StepSpeedup(
        step="g.wide",
        baseline_inner_jobs=4,
        recommended_inner_jobs=8,
        measured_effective_cores=8.0,
        regression_inner_jobs=None,
        levels=(
            SpeedupLevel(
                inner_jobs=4,
                samples=1,
                wall_s=8.0,
                raw_wall_s=8.0,
                wall_min_s=8.0,
                wall_max_s=8.0,
                cpu_s=32.0,
                effective_cores=4.0,
                throttled_s=0.0,
                speedup=1.0,
            ),
            SpeedupLevel(
                inner_jobs=8,
                samples=1,
                wall_s=4.0,
                raw_wall_s=4.0,
                wall_min_s=4.0,
                wall_max_s=4.0,
                cpu_s=32.0,
                effective_cores=8.0,
                throttled_s=0.0,
                speedup=2.0,
            ),
        ),
    )
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "wide",
                "",
                "true",
                hint=ResourceHint(est_duration_s=12.0, preferred_inner_jobs=8),
            ),
        )
    )
    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"g.wide": speedup},
        core_budget=2,
    )
    assert plan.entries[0].alloc_inner_jobs == 2
    assert plan.entries[0].speedup is None


def test_every_planner_bounds_profile_recommendation_to_run_cpu_budget() -> None:
    levels = tuple(
        SpeedupLevel(
            inner_jobs=width,
            samples=1,
            wall_s=16.0 / width,
            raw_wall_s=16.0 / width,
            wall_min_s=16.0 / width,
            wall_max_s=16.0 / width,
            cpu_s=16.0,
            effective_cores=float(width),
            throttled_s=0.0,
            speedup=float(width),
        )
        for width in (1, 2, 4, 8)
    )
    speedup = StepSpeedup(
        step="g.scaling",
        baseline_inner_jobs=1,
        recommended_inner_jobs=8,
        measured_effective_cores=8.0,
        regression_inner_jobs=None,
        levels=levels,
    )
    cfg = DagConfig(steps=(Step("g", "scaling", "", "true"),))

    for planner in (Planner.GREEDY_LPT, Planner.CRITICAL_PATH, Planner.CPA):
        plan = build_plan(
            cfg,
            {},
            planner=planner,
            speedups={"g.scaling": speedup},
            core_budget=2,
        )
        bounded = plan.entries[0].speedup
        assert bounded is not None
        assert bounded.recommended_inner_jobs == 2
        assert any(level.inner_jobs == 8 for level in bounded.levels)
        if planner is Planner.CPA:
            assert plan.entries[0].alloc_inner_jobs is not None
            assert plan.entries[0].alloc_inner_jobs <= 2


def test_cpa_keeps_self_managed_step_at_its_declared_width() -> None:
    levels = tuple(
        SpeedupLevel(
            inner_jobs=width,
            samples=1,
            wall_s=8.0 / width,
            raw_wall_s=8.0 / width,
            wall_min_s=8.0 / width,
            wall_max_s=8.0 / width,
            cpu_s=8.0,
            effective_cores=float(width),
            throttled_s=0.0,
            speedup=float(width),
        )
        for width in (1, 2, 4)
    )
    speedup = StepSpeedup("g.fixed", 1, 4, 4.0, None, levels)
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "fixed",
                "",
                "true",
                hint=ResourceHint(est_duration_s=13.0, preferred_inner_jobs=2),
                jobs_flag="",
            ),
        )
    )
    plan = build_plan(
        cfg,
        {},
        planner=Planner.CPA,
        speedups={"g.fixed": speedup},
        core_budget=4,
    )
    assert plan.entries[0].alloc_inner_jobs is None
    assert plan.entries[0].est_duration_s == 4.0
    assert plan.entries[0].est_source == "store"
    assert apply_plan_to_config(cfg, plan).steps[0].hint.preferred_inner_jobs == 2

    no_exact_cfg = DagConfig(
        steps=(
            Step(
                "g",
                "fixed",
                "",
                "true",
                hint=ResourceHint(est_duration_s=13.0, preferred_inner_jobs=3),
                jobs_flag="",
            ),
        )
    )
    no_exact = build_plan(
        no_exact_cfg,
        {},
        planner=Planner.CPA,
        speedups={"g.fixed": speedup},
        core_budget=4,
    )
    assert no_exact.entries[0].est_duration_s == 13.0
    assert no_exact.entries[0].est_source == "hint"


def test_cpa_excludes_intentional_skips_from_cpu_memory_and_curve_allocation() -> None:
    speedup = StepSpeedup(
        "g.live",
        1,
        2,
        2.0,
        None,
        (
            SpeedupLevel(1, 1, 10.0, 10.0, 10.0, 10.0, 10.0, 1.0, 0.0, 1.0),
            SpeedupLevel(2, 1, 5.0, 5.0, 5.0, 5.0, 10.0, 2.0, 0.0, 2.0),
        ),
    )
    skipped_hint = ResourceHint(
        est_duration_s=100.0,
        rss_baseline_bytes=10**12,
        preferred_inner_jobs=8,
    )
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "skipped",
                "",
                "false",
                hint=skipped_hint,
                jobs_flag="",
                skip_reason=IntentionalSkipReason.EMPTY_MANIFEST_BUCKET,
            ),
            Step(
                "g",
                "live",
                "",
                "true",
                hint=ResourceHint(est_duration_s=10.0, preferred_inner_jobs=1),
                jobs_flag="-j%d",
            ),
        ),
        mem_cap_factor=1.0,
        mem_cap_floor_bytes=0,
    )
    widths = allocate_widths(cfg, {"g.live": speedup}, {"g.skipped": 0.0, "g.live": 10.0}, 2)
    assert widths == {"g.skipped": 1, "g.live": 2}
    assert schedulable_peak_mem_bytes(cfg, 2, widths=widths)[0] == 1024**3
    assert stress_copy_footprint_bytes(cfg, default_step_bytes=123) == 123
    plan = build_plan(
        cfg, {}, planner=Planner.CPA, speedups={"g.live": speedup}, core_budget=2
    )
    entries = plan.by_tag()
    assert entries["g.skipped"].est_duration_s == 0.0
    assert entries["g.skipped"].est_source == "skip"
    assert entries["g.skipped"].alloc_inner_jobs is None
    assert entries["g.skipped"].speedup is None
    assert entries["g.live"].alloc_inner_jobs == 2
    assert plan.allocation is not None
    assert plan.allocation.modeled_makespan_s == 5.0
    applied = apply_plan_to_config(cfg, plan)
    assert applied.steps[0].hint == skipped_hint


def test_cpa_plan_application_cannot_launder_an_overbudget_self_managed_width(
    tmp_path: Path,
) -> None:
    marker = tmp_path / "spawned"
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "fixed",
                "",
                f"touch {shlex.quote(str(marker))}",
                hint=ResourceHint(preferred_inner_jobs=8),
                jobs_flag="",
            ),
        )
    )
    plan = build_plan(cfg, {}, planner=Planner.CPA, core_budget=2)
    assert plan.entries[0].alloc_inner_jobs is None
    with pytest.raises(InfeasibleAllocationError) as excinfo:
        allocate_widths(cfg, {}, {"g.fixed": 8.0}, 2)
    assert excinfo.value.core_budget == 2
    assert excinfo.value.fixed_widths == (("g.fixed", 8),)
    assert plan.allocation is not None
    assert plan.allocation.stop_reason == "infeasible-fixed-width"
    assert plan.allocation.modeled_makespan_s == float("inf")

    applied = apply_plan_to_config(cfg, plan)
    assert applied.steps[0].hint.preferred_inner_jobs == 8
    result = run_dag_limited(applied, max_steps=1, max_cpus=2, verbosity=0)
    assert not result.ok
    assert result.outcomes == ()
    assert not marker.exists()


def test_sweep_refuses_empty_jobs_flag_before_spawn(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    marker = tmp_path / "spawned"
    dag = tmp_path / "dag.json"
    dag.write_text(
        dag_to_json(
            DagConfig(
                steps=(
                    Step(
                        "g",
                        "fixed",
                        "",
                        f"touch {shlex.quote(str(marker))}",
                        jobs_flag="",
                    ),
                )
            )
        ),
        encoding="utf-8",
    )
    rc = main(
        [
            "sweep",
            "--dag",
            str(dag),
            "--step",
            "g.fixed",
            "--jobs",
            "1..2",
            "--no-profile",
        ]
    )
    assert rc == 2
    assert not marker.exists()
    assert "offers no width channel" in capsys.readouterr().err


def test_run_budget_reaches_every_planner_but_memory_remains_cpa_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("dagrun.cli.container_core_budget", lambda: 12)
    for planner in (Planner.GREEDY_LPT, Planner.CRITICAL_PATH, Planner.CPA):
        core_budget, mem_budget = _planning_budgets(planner, "2G", 16)
        assert core_budget == 16
        assert mem_budget == (2 * 1024**3 if planner is Planner.CPA else None)

    assert _planning_budgets(Planner.GREEDY_LPT, None) == (None, None)
    assert _planning_budgets(Planner.CRITICAL_PATH, None) == (None, None)
    assert _planning_budgets(Planner.CPA, None) == (12, None)


def test_cpa_charges_default_step_cpu_count_for_curveless_step() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "default",
                "",
                "true",
                hint=ResourceHint(est_duration_s=1.0, preferred_inner_jobs=0),
            ),
        ),
        default_step_cpu_count=4,
    )
    plan = build_plan(cfg, {}, planner=Planner.CPA, core_budget=4)
    assert plan.entries[0].alloc_inner_jobs == 4

    # An empty jobs flag with no positive explicit preferred width is not a hardcoded guest width:
    # the top-level default is a runner/cgroup cap and may be tightened to P without refusal.
    self_managed_default = DagConfig(
        steps=(Step("g", "default", "", "true", jobs_flag=""),),
        default_step_cpu_count=8,
    )
    assert allocate_widths(
        self_managed_default, {}, {"g.default": 1.0}, 2
    ) == {"g.default": 2}
    default_plan = build_plan(
        self_managed_default, {}, planner=Planner.CPA, core_budget=2
    )
    assert default_plan.entries[0].alloc_inner_jobs is None
    assert default_plan.allocation is not None
    assert default_plan.allocation.stop_reason != "infeasible-fixed-width"


def test_scope_reexec_requests_and_carries_exact_max_cpus(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: list[str] = []

    def fake_execvp(_program: str, argv: list[str]) -> None:
        captured.extend(argv)
        raise OSError("planted exec stop")

    monkeypatch.setattr(cgroup, "systemd_scope_available", lambda naming: True)
    monkeypatch.setattr(cgroup, "ensure_aggregate_slice", lambda naming: False)
    monkeypatch.setattr(os, "execvp", fake_execvp)
    # Exercise a fresh outer re-exec even when this test shard is itself delegated.
    monkeypatch.delenv(cgroup.DEFAULT_NAMING.env_in_scope, raising=False)
    monkeypatch.delenv(cgroup.DELEGATED_CGROUP_ENV, raising=False)
    monkeypatch.delenv(cgroup.DELEGATED_UNBOXED_ENV, raising=False)
    assert not cgroup.reexec_in_scope(
        ["runner", "run"],
        memory_max=1024,
        cpu_count=3,
        skip_in_ci=False,
    )
    assert "CPUQuota=300%" in captured
    assert f"--setenv={cgroup.EXPECTED_OUTER_CPU_COUNT_ENV}=3" in captured


def test_scope_limit_audit_requires_exact_cpu_max(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    (tmp_path / "memory.max").write_text("104857600")
    (tmp_path / "memory.swap.max").write_text("0")
    (tmp_path / "memory.oom.group").write_text("1")
    (tmp_path / "cpu.max").write_text("200000 100000")
    monkeypatch.setattr(cgroup, "scope_cgroup_from_self", lambda naming: tmp_path)
    assert cgroup.verify_scope_limits(104857600, 2)
    (tmp_path / "cpu.max").write_text("300000 100000")
    assert not cgroup.verify_scope_limits(104857600, 2)


def _scripted_probe(results: list[str | None]) -> tuple[list[int], Callable[[int], str | None]]:
    attempts: list[int] = []

    def probe(attempt: int) -> str | None:
        attempts.append(attempt)
        return results[len(attempts) - 1]

    return attempts, probe


def test_scope_probe_refused_once_is_retried_after_the_backoff() -> None:
    attempts, probe = _scripted_probe(["refused", None])
    start = time.monotonic()
    assert cgroup.probe_with_one_retry(probe, 0.5)
    assert time.monotonic() - start >= 0.5
    assert attempts == [1, 2]


def test_scope_probe_refused_twice_fails_closed_without_a_third_try() -> None:
    attempts, probe = _scripted_probe(["refused", "refused", None])
    start = time.monotonic()
    assert not cgroup.probe_with_one_retry(probe, 0.5)
    waited = time.monotonic() - start
    # One pause, between the attempts; none after the last.
    assert 0.5 <= waited < 1.0
    assert attempts == [1, 2]


def test_scope_probe_that_passes_runs_once_without_waiting() -> None:
    attempts, probe = _scripted_probe([None])
    start = time.monotonic()
    # A long backoff costs nothing when nothing waits, and leaves wide slack for a slow host.
    assert cgroup.probe_with_one_retry(probe, 30.0)
    assert time.monotonic() - start < 30.0
    assert attempts == [1]


def test_scope_probe_failures_are_logged_with_their_reason(
    capsys: pytest.CaptureFixture[str],
) -> None:
    _attempts, probe = _scripted_probe(["first reason", "second reason"])
    naming = dataclasses.replace(cgroup.DEFAULT_NAMING, log_prefix="[probe-test]")
    assert not cgroup.probe_with_one_retry(probe, 0.0, naming)
    err = capsys.readouterr().err
    assert "[probe-test] `systemd-run --user --scope` probe attempt 1 of 2 failed: first reason" in err
    assert "[probe-test] `systemd-run --user --scope` probe attempt 2 of 2 failed: second reason" in err


def _install_fake_systemd_run(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, script: str
) -> Path:
    """Put an executable ``systemd-run`` first on PATH and clear the cached probe answer."""
    fake = tmp_path / "systemd-run"
    fake.write_text(script)
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", f"{tmp_path}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setattr(cgroup, "_SCOPE_PROBE", None)
    monkeypatch.setattr(cgroup, "SCOPE_PROBE_RETRY_BACKOFF_S", 0.0)
    return fake


def test_scope_probe_against_a_refusing_systemd_run_tries_two_unit_names(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    calls = tmp_path / "systemd-run.calls"
    _install_fake_systemd_run(
        monkeypatch,
        tmp_path,
        "#!/bin/sh\n"
        f"printf '%s\\n' \"$*\" >> {shlex.quote(str(calls))}\n"
        "echo 'fake refusal: no user bus here' >&2\n"
        "exit 1\n",
    )
    naming = dataclasses.replace(cgroup.DEFAULT_NAMING, log_prefix="[probe-e2e]")
    assert not cgroup.systemd_scope_available(naming)
    lines = calls.read_text().splitlines()
    assert len(lines) == 2
    assert f"--unit=dagrun-probe-{os.getpid()}-1" in lines[0].split()
    assert f"--unit=dagrun-probe-{os.getpid()}-2" in lines[1].split()
    assert all("--collect" in line.split() for line in lines)
    err = capsys.readouterr().err
    for attempt in (1, 2):
        assert (
            f"[probe-e2e] `systemd-run --user --scope` probe attempt {attempt} of 2 failed: "
            "exit status 1; "
            "stderr: 'fake refusal: no user bus here'"
        ) in err
    # The answer is cached: a second ask does not probe again.
    assert not cgroup.systemd_scope_available(naming)
    assert len(calls.read_text().splitlines()) == 2


@pytest.mark.parametrize(
    ("script", "reason"),
    [
        pytest.param(
            # Stopped, not exited, until the timeout kills it.
            "#!/bin/sh\necho 'partial stderr before hang' >&2\nkill -STOP $$\n",
            "timed out after 1 s; stderr: 'partial stderr before hang'",
            id="hangs",
        ),
        pytest.param(
            "#!/nonexistent/interpreter\n",
            "cannot launch systemd-run: ",
            id="cannot-launch",
        ),
        pytest.param(
            "#!/bin/sh\necho 'about to die' >&2\nkill -TERM $$\n",
            "signal 15; stderr: 'about to die'",
            id="killed-by-signal",
        ),
    ],
)
def test_scope_probe_that_cannot_succeed_fails_closed_with_its_reason(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    script: str,
    reason: str,
) -> None:
    _install_fake_systemd_run(monkeypatch, tmp_path, script)
    # The fake alone on PATH: subprocess keeps searching PATH past a candidate it cannot execute,
    # and would otherwise launch the host's real systemd-run.
    monkeypatch.setenv("PATH", str(tmp_path))
    monkeypatch.setattr(cgroup, "_SCOPE_PROBE_TIMEOUT_S", 1)
    assert not cgroup.systemd_scope_available()
    err = capsys.readouterr().err
    for attempt in (1, 2):
        assert f"probe attempt {attempt} of 2 failed: {reason}" in err


def test_scope_probe_without_systemd_run_says_so(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setenv("PATH", str(tmp_path))
    monkeypatch.setattr(cgroup, "_SCOPE_PROBE", None)
    assert not cgroup.systemd_scope_available()
    assert "probe not attempted: systemd-run is not on PATH" in capsys.readouterr().err
