"""Tests for the concurrency calibrator: the mandatory 1->2->4 ramp, downshift, and the
resource-slot width resolution that makes concurrency DECLARED + ENFORCED rather than unbounded."""

from __future__ import annotations

import pytest

from parallel_experiment_runner.calibrate import (
    LiveCapacity,
    PerInstance,
    affinity_cpu_count,
    initial_width,
    measured_per_instance,
    ramp_next_width,
    resolve_width,
)
from parallel_experiment_runner.model import ResourceSlice, WorkerLimits

_GIB = 1024**3


def _slice(cpu: int = 1000, mem: int = 1000 * _GIB, disk: int = 1000 * _GIB) -> ResourceSlice:
    return ResourceSlice(revision=0, cpu_cores=cpu, memory_bytes=mem, disk_bytes=disk)


def _live(
    cpu: int = 8,
    mem: int = 64 * _GIB,
    disk: int = 500 * _GIB,
    available: int | None = None,
) -> LiveCapacity:
    # By default the whole box is idle (available == total), so tests that don't exercise the
    # headroom axis bind on lane/ceiling exactly as before.
    return LiveCapacity(
        cpu_cores=cpu,
        available_cpu_cores=cpu if available is None else available,
        mem_available_bytes=mem,
        disk_free_bytes=disk,
    )


def test_resolve_width_cpu_limited() -> None:
    # Lane grants 1000 cores but the host only has 8 live: live capacity can only REDUCE.
    fit = resolve_width(_slice(), _live(cpu=8), PerInstance(1, None, None), ceiling=64)
    assert fit.width == 8
    assert fit.limiting_dimension == "cpu"
    assert fit.cpu_slots == 8


def test_resolve_width_uses_measured_headroom_not_total_cores() -> None:
    # 64 cores total but only 4 measured idle => the busy box binds width to real headroom, not
    # the full core count (the phantom-saturation fix: size from measured idle, not total/load).
    fit = resolve_width(_slice(), _live(cpu=64, available=4), PerInstance(1, None, None), ceiling=64)
    assert fit.width == 4
    assert fit.limiting_dimension == "cpu"
    assert fit.cpu_slots == 4


def test_resolve_width_idle_box_scales_generously() -> None:
    # A fully-idle 64-core box (available == total) is bound only by the ceiling, not by a
    # phantom load reading — the runner may fan out to the granted ceiling.
    fit = resolve_width(_slice(), _live(cpu=64, available=64), PerInstance(1, None, None), ceiling=48)
    assert fit.width == 48
    assert fit.limiting_dimension == "ceiling"


def test_resolve_width_memory_limited() -> None:
    # 64 GiB live / 16 GiB per worker => 4 memory slots is the binding constraint.
    fit = resolve_width(_slice(), _live(cpu=64), PerInstance(1, 16 * _GIB, None), ceiling=64)
    assert fit.width == 4
    assert fit.limiting_dimension == "memory"
    assert fit.mem_slots == 4


def test_live_memory_reserve_is_subtracted_before_sizing() -> None:
    slice_ = ResourceSlice(
        revision=0,
        cpu_cores=64,
        memory_bytes=64 * _GIB,
        disk_bytes=500 * _GIB,
        memory_reserve_bytes=16 * _GIB,
    )
    fit = resolve_width(
        slice_, _live(cpu=64, mem=64 * _GIB), PerInstance(1, 8 * _GIB, None), ceiling=64
    )
    assert fit.width == 6
    assert fit.mem_slots == 6
    assert fit.limiting_dimension == "memory"


def test_memory_reserve_can_refuse_the_first_worker() -> None:
    slice_ = ResourceSlice(
        revision=0,
        cpu_cores=64,
        memory_bytes=64 * _GIB,
        disk_bytes=500 * _GIB,
        memory_reserve_bytes=64 * _GIB,
    )
    fit = resolve_width(
        slice_, _live(cpu=64, mem=64 * _GIB), PerInstance(1, _GIB, None), ceiling=64
    )
    assert fit.width == 0
    assert initial_width(fit) == 0


def test_negative_memory_reserve_is_rejected() -> None:
    with pytest.raises(ValueError, match="non-negative"):
        ResourceSlice(
            revision=0,
            cpu_cores=1,
            memory_bytes=_GIB,
            disk_bytes=_GIB,
            memory_reserve_bytes=-1,
        )


def test_resolve_width_ceiling_limited() -> None:
    fit = resolve_width(_slice(), _live(cpu=64), PerInstance(1, None, None), ceiling=5)
    assert fit.width == 5
    assert fit.limiting_dimension == "ceiling"


def test_machine_hard_cap_is_316() -> None:
    fit = resolve_width(
        _slice(), _live(cpu=1000, available=1000), PerInstance(1, None, None), ceiling=1000
    )
    assert fit.width == 316
    assert fit.ceiling == 316
    assert fit.limiting_dimension == "ceiling"


def test_affinity_count_wins_over_host_cpu_count(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "parallel_experiment_runner.calibrate.os.sched_getaffinity",
        lambda _pid: {2, 4, 9},
    )
    monkeypatch.setattr("parallel_experiment_runner.calibrate.os.cpu_count", lambda: 316)
    assert affinity_cpu_count() == 3


def test_affinity_count_falls_back_when_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    def unavailable(_pid: int) -> set[int]:
        raise OSError("not supported")

    monkeypatch.setattr("parallel_experiment_runner.calibrate.os.sched_getaffinity", unavailable)
    monkeypatch.setattr("parallel_experiment_runner.calibrate.os.cpu_count", lambda: 8)
    assert affinity_cpu_count() == 8


def test_resolve_width_lane_shrinks_below_live() -> None:
    # A coordinator lane narrower than the live host wins: usable = min(lane, live).
    fit = resolve_width(_slice(cpu=3), _live(cpu=64), PerInstance(1, None, None), ceiling=64)
    assert fit.width == 3
    assert fit.limiting_dimension == "cpu"


def test_ramp_doubles_then_caps() -> None:
    wide = resolve_width(_slice(), _live(cpu=64), PerInstance(1, None, None), ceiling=64)
    assert ramp_next_width(1, wide) == 2  # 1 -> 2
    assert ramp_next_width(2, wide) == 4  # 2 -> 4
    # Doubling never exceeds what currently fits.
    narrow = resolve_width(_slice(), _live(cpu=6), PerInstance(1, None, None), ceiling=64)
    assert ramp_next_width(4, narrow) == 6


def test_ramp_downshifts_immediately_on_shrink() -> None:
    shrunk = resolve_width(_slice(), _live(cpu=2), PerInstance(1, None, None), ceiling=64)
    # Was running 8 wide; the lane/host shrank to 2 -> drop straight to 2, no reliance on history.
    assert ramp_next_width(8, shrunk) == 2


def test_initial_width_is_one_worker_or_wait() -> None:
    ok = resolve_width(_slice(), _live(cpu=8), PerInstance(1, None, None), ceiling=64)
    assert initial_width(ok) == 1  # stage 1 profiles a single worker
    # Box too small for even one worker => 0, caller must WAIT.
    none_fit = resolve_width(_slice(), _live(cpu=1), PerInstance(4, None, None), ceiling=64)
    assert none_fit.width == 0
    assert initial_width(none_fit) == 0


def test_measured_per_instance_clamps_to_hard_cap() -> None:
    limits = WorkerLimits(cpu_cores=2, memory_bytes=1500)
    # Measured peak * headroom would exceed the hard cap => clamp to the enforced memory.max.
    fit = measured_per_instance(limits, peak_mem_bytes=1000, mem_headroom=2.0)
    assert fit.memory_bytes == 1500
    assert fit.cpu_cores == 2


def test_measured_per_instance_uses_headroom_under_cap() -> None:
    limits = WorkerLimits(cpu_cores=1, memory_bytes=10_000)
    fit = measured_per_instance(limits, peak_mem_bytes=1000, mem_headroom=1.25)
    assert fit.memory_bytes == 1250


def test_measured_per_instance_no_sample_falls_back_to_declared() -> None:
    limits = WorkerLimits(cpu_cores=1, memory_bytes=4096)
    fit = measured_per_instance(limits, peak_mem_bytes=None)
    assert fit.memory_bytes == 4096
