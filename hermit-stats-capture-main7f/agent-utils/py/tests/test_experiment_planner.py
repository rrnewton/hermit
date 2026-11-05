"""Tests for lowering one round onto a dagrun DagConfig and the workload classifier.

These assert the four hard requirements land on the exact per-step controls the executor enforces:
each seed becomes one boxed Step whose cpu_timeout / memory.max / cpu.max / wall timeout come from
the declared per-worker limits, and whose workload argv is never mangled with an inner -j flag.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from dagrun import DagConfig, RunResult
from parallel_experiment_runner import execute
from parallel_experiment_runner.model import (
    STATUS_COMMAND_ERROR,
    STATUS_HIT,
    STATUS_MISS,
    CostEstimate,
    ExperimentSpec,
    HitCondition,
    WorkerLimits,
)
from parallel_experiment_runner.planner import (
    RoundPlan,
    build_worker_command,
    classify_workload,
    generate_round_dag,
    seed_tag,
    worker_log_path,
)


def _spec(**kw: object) -> ExperimentSpec:
    base: dict[str, object] = {
        "name": "s",
        "command": ("widget", "run", "--seed", "{seed}", "./demo"),
    }
    base.update(kw)
    return ExperimentSpec(**base)  # type: ignore[arg-type]


def _plan(spec: ExperimentSpec, seeds: tuple[int, ...], tmp: Path) -> RoundPlan:
    return RoundPlan(
        spec=spec,
        seeds=seeds,
        width=len(seeds),
        slice_revision=0,
        limiting_dimension="cpu",
        log_dir=tmp,
        per_worker_estimate=CostEstimate.unset(),
    )


def test_seed_tag_and_log_path(tmp_path: Path) -> None:
    assert seed_tag(42) == "seed.42"
    assert worker_log_path(tmp_path, 42) == tmp_path / "seed-42.log"


def test_contained_reexec_uses_absolute_repository_launcher(tmp_path: Path) -> None:
    launcher = tmp_path / "bin" / "parallel-experiment-runner"
    launcher.parent.mkdir()
    launcher.write_text("#!/bin/sh\n")
    argv = execute._contained_reexec_argv(("run", "--help"), python_root=tmp_path)
    assert argv == [sys.executable, str(launcher.resolve()), "run", "--help"]


def test_contained_reexec_falls_back_to_installed_module(tmp_path: Path) -> None:
    argv = execute._contained_reexec_argv(("run", "--help"), python_root=tmp_path)
    assert argv == [
        sys.executable,
        "-m",
        "parallel_experiment_runner",
        "run",
        "--help",
    ]


def test_lowering_maps_every_hard_limit(tmp_path: Path) -> None:
    limits = WorkerLimits(
        cpu_cores=3, memory_bytes=4 * 1024**3, cpu_timeout_s=120, wall_timeout_s=900
    )
    spec = _spec(worker_limits=limits)
    dag = generate_round_dag(_plan(spec, (0, 1), tmp_path))
    assert len(dag.steps) == 2
    step = dag.steps[0]
    assert step.tag == "seed.0"
    assert step.cpu_timeout == 120  # -> cgroup cpu.stat CPU-second budget
    assert step.timeout == 900  # -> wall backstop
    assert step.hint.hard_mem_max_bytes == 4 * 1024**3  # -> inner memory.max
    assert step.hint.preferred_inner_jobs == 3  # -> inner cpu.max + width core-unit
    assert step.jobs_flag == ""  # never append -j to a complete workload argv


def test_execute_maps_workers_and_per_worker_cores_to_independent_limits(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    captured: dict[str, int] = {}

    def fake_run(
        _dag: DagConfig,
        *,
        max_steps: int,
        max_cpus: int,
        **_kwargs: object,
    ) -> RunResult:
        captured.update(max_steps=max_steps, max_cpus=max_cpus)
        return RunResult(ok=True, wall_s=0.0)

    monkeypatch.setattr(execute, "run_dag_limited", fake_run)
    plan = _plan(
        _spec(worker_limits=WorkerLimits(cpu_cores=3)),
        (0, 1, 2),
        tmp_path,
    )
    plan = RoundPlan(
        spec=plan.spec,
        seeds=plan.seeds,
        width=2,
        slice_revision=plan.slice_revision,
        limiting_dimension=plan.limiting_dimension,
        log_dir=plan.log_dir,
        per_worker_estimate=plan.per_worker_estimate,
    )
    execute.execute_round(plan, cgroups=None)
    assert captured == {"max_steps": 2, "max_cpus": 6}


def test_execute_enforces_machine_hard_cap_for_direct_plan(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    captured: dict[str, int] = {}

    def fake_run(
        _dag: DagConfig,
        *,
        max_steps: int,
        max_cpus: int,
        **_kwargs: object,
    ) -> RunResult:
        captured.update(max_steps=max_steps, max_cpus=max_cpus)
        return RunResult(ok=True, wall_s=0.0)

    monkeypatch.setattr(execute, "run_dag_limited", fake_run)
    seeds = tuple(range(400))
    plan = _plan(_spec(worker_limits=WorkerLimits(cpu_cores=2)), seeds, tmp_path)
    execute.execute_round(plan, cgroups=None)
    assert captured == {"max_steps": 316, "max_cpus": 632}


def test_execute_cleans_registered_detached_unit_when_executor_raises(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    report = tmp_path / "resource-7.txt"
    calls: list[tuple[str, tuple[tuple[Path, str], ...]]] = []

    def fake_register(reports: tuple[tuple[Path, str], ...]) -> None:
        calls.append(("register", reports))

    def fake_cleanup(reports: tuple[tuple[Path, str], ...]) -> tuple[str, ...]:
        calls.append(("cleanup", reports))
        return ("owned-7.service",)

    def fake_unregister(reports: tuple[tuple[Path, str], ...]) -> None:
        calls.append(("unregister", reports))

    monkeypatch.setattr(execute, "register_reports", fake_register)
    monkeypatch.setattr(execute, "cleanup_reports", fake_cleanup)
    monkeypatch.setattr(execute, "unregister_reports", fake_unregister)

    def fail_run(_dag: DagConfig, **_kwargs: object) -> RunResult:
        report.write_text("wrapper: unit=owned-7\n")
        raise RuntimeError("executor failure")

    monkeypatch.setattr(execute, "run_dag_limited", fail_run)
    spec = _spec(
        worker_limits=WorkerLimits(memory_bytes=1024, cpu_timeout_s=5, pids_max=10),
        detached_resource_report=str(tmp_path / "resource-{seed}.txt"),
        detached_unit_prefix="owned-",
    )
    expected = ((report.resolve(), "owned-"),)
    with pytest.raises(RuntimeError, match="executor failure"):
        execute.execute_round(_plan(spec, (7,), tmp_path), cgroups=None)
    assert calls == [
        ("register", expected),
        ("cleanup", expected),
        ("unregister", expected),
    ]


def test_lowering_cpu_timeout_unset_becomes_zero(tmp_path: Path) -> None:
    # cpu_timeout_s=None (UNSET) lowers to the executor's 0 = "disabled" sentinel, never a guess.
    spec = _spec(worker_limits=WorkerLimits(cpu_cores=1, cpu_timeout_s=None))
    step = generate_round_dag(_plan(spec, (7,), tmp_path)).steps[0]
    assert step.cpu_timeout == 0


def test_lowering_derives_wall_backstop_from_cpu_budget(tmp_path: Path) -> None:
    # wall_timeout_s unset + a CPU budget -> Step.timeout is the derived ~3x backstop, not None.
    spec = _spec(
        worker_limits=WorkerLimits(cpu_cores=1, cpu_timeout_s=120, wall_timeout_s=None)
    )
    step = generate_round_dag(_plan(spec, (7,), tmp_path)).steps[0]
    assert (
        step.timeout == 360
    )  # 3 * 120, sitting above the authoritative CPU-second guard


def test_build_worker_command_quotes_and_redirects(tmp_path: Path) -> None:
    spec = _spec(command=("echo", "hi there", "{seed}"))
    log = worker_log_path(tmp_path, 5)
    cmd = build_worker_command(spec, 5, log)
    # The seed is substituted, the argument with a space is quoted, and stdout+stderr redirect.
    assert "'hi there'" in cmd
    assert cmd.strip().endswith("2>&1")
    assert str(log) in cmd
    assert " 5 " in f" {cmd} "  # the concrete seed appears as its own token


def test_build_worker_command_is_injection_safe(tmp_path: Path) -> None:
    # A metacharacter-laden seed-adjacent arg must be quoted, not interpreted by the wrapper shell.
    spec = _spec(command=("run", "{seed}", "; rm -rf /"))
    cmd = build_worker_command(spec, 1, worker_log_path(tmp_path, 1))
    assert "'; rm -rf /'" in cmd


def test_classify_hit_by_regex() -> None:
    hit = HitCondition(regex="DIVERGENCE")
    assert classify_workload(hit, 0, "...\nDIVERGENCE detected\n...") == STATUS_HIT


def test_classify_hit_by_exit_code() -> None:
    hit = HitCondition(hit_exit_codes=(134,))
    assert classify_workload(hit, 134, "") == STATUS_HIT


def test_classify_miss_clean_zero() -> None:
    hit = HitCondition(regex="panic")
    assert classify_workload(hit, 0, "all good") == STATUS_MISS


def test_classify_command_error_nonzero_non_hit() -> None:
    hit = HitCondition(regex="panic")
    assert classify_workload(hit, 2, "some unrelated failure") == STATUS_COMMAND_ERROR
