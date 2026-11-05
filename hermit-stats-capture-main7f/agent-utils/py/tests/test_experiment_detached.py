"""Tests for accounting and cleanup of workload units outside a worker cgroup."""

from __future__ import annotations

import subprocess
from dataclasses import replace
from pathlib import Path

import pytest

from dagrun import StepOutcome
from parallel_experiment_runner.detached import cleanup_reports, known_unit, read_report
from parallel_experiment_runner.execute import _classify_outcome, _record_samples
from parallel_experiment_runner.model import (
    STATUS_CPU_TIMEOUT,
    STATUS_HIT,
    STATUS_MEMORY_CAP,
    STATUS_LOG_CAP,
    STATUS_PIDS_CAP,
    STATUS_RESOURCE_REPORT_ERROR,
    STATUS_TIMEOUT,
    ExperimentSpec,
    HitCondition,
    RoundResult,
    WorkerLimits,
)
from parallel_experiment_runner.profile import ProfileStore


def _write_report(path: Path, *, unit: str = "worker-7") -> None:
    path.write_text(
        "wrapper: unit=" + unit + "\n"
        "wrapper: memory_peak_bytes=200\n"
        "wrapper: cpu_usage_nsec=2000000000\n"
        "wrapper: tasks_peak=7\n"
        "wrapper: limit_memory_bytes=1000\n"
        "wrapper: limit_swap_bytes=0\n"
        "wrapper: limit_tasks=10\n"
        "wrapper: limit_cpu_cores_micros=1000000\n"
        "wrapper: limit_cpu_time_nsec=5000000000\n"
        "wrapper: limit_wall_time_usec=15000000\n"
        "wrapper: cpu_time_limit_hit=0\n"
        "wrapper: wall_time_limit_hit=0\n"
        "wrapper: log_cap_hit=0\n"
        "wrapper: memory_oom_kills=0\n"
        "wrapper: tasks_limit_hits=0\n"
    )


def _outcome() -> StepOutcome:
    return StepOutcome(
        tag="seed.3",
        ok=True,
        duration_s=1.0,
        summary="",
        returncode=0,
        oomed=False,
        oom_kills=0,
        timed_out=False,
        cpu_timed_out=False,
        aborted=False,
        pids_events=0,
    )


def _spec(report: Path) -> ExperimentSpec:
    return ExperimentSpec(
        name="detached",
        command=("run", "{seed}"),
        worker_limits=WorkerLimits(memory_bytes=1000, cpu_timeout_s=5, pids_max=10),
        hit=HitCondition(hit_exit_codes=(0,)),
        detached_resource_report=str(report).replace("3.txt", "{seed}.txt"),
        detached_unit_prefix="worker-",
    )


def test_complete_detached_report_is_added_to_worker_measurement(
    tmp_path: Path,
) -> None:
    report = tmp_path / "3.txt"
    _write_report(report)
    result = _classify_outcome(
        _spec(report),
        _outcome(),
        {"peak_bytes": 100, "cpu.usage_usec": 1_000_000},
        tmp_path / "worker.log",
    )
    assert result.status == STATUS_HIT
    assert result.peak_bytes == 300
    assert result.cpu_s == 3.0
    assert result.tasks_peak == 7
    assert result.resource_report_error == ""
    round_result = RoundResult(
        width=1,
        seeds=(3,),
        outcomes=(result,),
        wall_s=1.0,
        cpu_s=3.0,
        slice_revision=0,
        limiting_dimension="memory",
    )
    store = ProfileStore(tmp_path / "profile.json")
    assert _record_samples(store, "key", round_result) == 300
    estimate = store.estimate("key")
    assert estimate.peak_mem_bytes == 300
    assert estimate.cpu_s == 3.0
    assert estimate.peak_tasks == 7


def test_missing_detached_metrics_cannot_look_like_a_clean_sample(
    tmp_path: Path,
) -> None:
    report = tmp_path / "3.txt"
    report.write_text("wrapper: unit=worker-7\n")
    result = _classify_outcome(_spec(report), _outcome(), {}, tmp_path / "worker.log")
    assert result.status == STATUS_RESOURCE_REPORT_ERROR
    assert result.is_breach
    assert result.breach.startswith("RESOURCE-REPORT-ERROR:")
    assert "memory_peak_bytes" in result.resource_report_error
    round_result = RoundResult(
        width=1,
        seeds=(3,),
        outcomes=(result,),
        wall_s=1.0,
        cpu_s=0.0,
        slice_revision=0,
        limiting_dimension="memory",
    )
    store = ProfileStore(tmp_path / "profile.json")
    assert _record_samples(store, "key", round_result) is None
    assert store.estimate("key").is_set is False


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("limit_memory_bytes", "999"),
        ("limit_swap_bytes", "1"),
        ("limit_tasks", "9"),
        ("limit_cpu_cores_micros", "2000000"),
        ("limit_cpu_time_nsec", "4000000000"),
        ("limit_wall_time_usec", "14000000"),
    ],
)
def test_mismatched_detached_limit_is_refused(
    field: str, value: str, tmp_path: Path
) -> None:
    report = tmp_path / "3.txt"
    _write_report(report)
    text = report.read_text()
    report.write_text(
        text.replace(f"wrapper: {field}=", f"wrapper: ignored_{field}=")
        + f"wrapper: {field}={value}\n"
    )
    result = _classify_outcome(_spec(report), _outcome(), {}, tmp_path / "worker.log")
    assert result.status == STATUS_RESOURCE_REPORT_ERROR
    assert field in result.resource_report_error
    assert "does not match declared" in result.resource_report_error


@pytest.mark.parametrize(
    ("field", "status"),
    [
        ("cpu_time_limit_hit", STATUS_CPU_TIMEOUT),
        ("memory_oom_kills", STATUS_MEMORY_CAP),
        ("tasks_limit_hits", STATUS_PIDS_CAP),
        ("wall_time_limit_hit", STATUS_TIMEOUT),
        ("log_cap_hit", STATUS_LOG_CAP),
    ],
)
def test_detached_breach_counter_precedes_hit(
    field: str, status: str, tmp_path: Path
) -> None:
    report = tmp_path / "3.txt"
    _write_report(report)
    text = report.read_text()
    report.write_text(text.replace(f"wrapper: {field}=0", f"wrapper: {field}=1"))
    result = _classify_outcome(_spec(report), _outcome(), {}, tmp_path / "worker.log")
    assert result.status == status
    assert result.is_hit is False


def test_detached_log_cap_exit_125_cannot_satisfy_hit_exit_code(tmp_path: Path) -> None:
    report = tmp_path / "3.txt"
    _write_report(report)
    report.write_text(
        report.read_text().replace("wrapper: log_cap_hit=0", "wrapper: log_cap_hit=1")
    )
    spec = replace(_spec(report), hit=HitCondition(hit_exit_codes=(125,)))
    outcome = replace(_outcome(), returncode=125)
    result = _classify_outcome(spec, outcome, {}, tmp_path / "worker.log")
    assert result.status == STATUS_LOG_CAP
    assert result.is_hit is False
    assert result.is_breach


def test_report_requires_owned_syntax_checked_unit(tmp_path: Path) -> None:
    report = tmp_path / "report.txt"
    _write_report(report, unit="foreign-7")
    with pytest.raises(ValueError, match="owned unit"):
        read_report(report, "worker-")
    _write_report(report, unit="worker-7;poweroff")
    assert known_unit(report, "worker-") is None
    _write_report(report, unit="-Hhost")
    with pytest.raises(ValueError, match="owned unit"):
        read_report(report, "-H")
    assert known_unit(report, "-H") is None


def test_cleanup_batches_only_valid_owned_units(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    first = tmp_path / "first.txt"
    second = tmp_path / "second.txt"
    foreign = tmp_path / "foreign.txt"
    _write_report(first, unit="worker-2")
    _write_report(second, unit="worker-1.service")
    _write_report(foreign, unit="other-9")
    commands: list[list[str]] = []

    def fake_run(
        command: list[str], **_kwargs: object
    ) -> subprocess.CompletedProcess[bytes]:
        commands.append(command)
        return subprocess.CompletedProcess(command, 0)

    monkeypatch.setattr("parallel_experiment_runner.detached.subprocess.run", fake_run)
    cleaned = cleanup_reports(
        ((first, "worker-"), (second, "worker-"), (foreign, "worker-"))
    )
    assert cleaned == ("worker-1.service", "worker-2.service")
    assert commands == [
        [
            "systemctl",
            "--user",
            "kill",
            "--kill-whom=all",
            "--signal=SIGKILL",
            "--",
            "worker-1.service",
            "worker-2.service",
        ],
        ["systemctl", "--user", "stop", "--", "worker-1.service", "worker-2.service"],
        [
            "systemctl",
            "--user",
            "reset-failed",
            "--",
            "worker-1.service",
            "worker-2.service",
        ],
    ]
