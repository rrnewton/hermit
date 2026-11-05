"""Tests for the pure data vocabulary: the four-axis resource-containment envelope.

These pin the two behaviours the fork-bomb / wall-backstop work added to ``WorkerLimits``:
the PID axis (``pids_max`` validation + membership in the breach vocabulary) and the wall
backstop's derive-when-unset idiom (~3x the CPU budget, never racing the authoritative
CPU-second guard)."""

from __future__ import annotations

import pytest

from parallel_experiment_runner.model import (
    BREACH_STATUSES,
    DEFAULT_WALL_TIMEOUT_S,
    STATUS_CPU_TIMEOUT,
    STATUS_DISK_CAP,
    STATUS_MEMORY_CAP,
    STATUS_LOG_CAP,
    STATUS_PIDS_CAP,
    STATUS_RESOURCE_REPORT_ERROR,
    STATUS_TIMEOUT,
    WALL_CPU_BACKSTOP_FACTOR,
    ExperimentSpec,
    WorkerLimits,
)


def test_pids_cap_is_a_breach_status() -> None:
    # The fork-bomb axis must be counted as a limit breach (never a hit), alongside the others.
    assert STATUS_PIDS_CAP in BREACH_STATUSES
    assert BREACH_STATUSES >= {
        STATUS_TIMEOUT,
        STATUS_CPU_TIMEOUT,
        STATUS_MEMORY_CAP,
        STATUS_PIDS_CAP,
        STATUS_DISK_CAP,
        STATUS_LOG_CAP,
        STATUS_RESOURCE_REPORT_ERROR,
    }


def test_pids_max_rejects_zero_and_negative() -> None:
    with pytest.raises(ValueError):
        WorkerLimits(pids_max=0)
    with pytest.raises(ValueError):
        WorkerLimits(pids_max=-1)


def test_pids_max_none_is_no_cap() -> None:
    assert WorkerLimits().pids_max is None
    assert WorkerLimits(pids_max=64).pids_max == 64


def test_wall_derived_from_cpu_budget_when_unset() -> None:
    # No explicit wall + a CPU budget -> derive ~3x that budget (defence-in-depth headroom).
    limits = WorkerLimits(cpu_timeout_s=120)
    assert limits.wall_timeout_s is None
    assert limits.resolved_wall_timeout_s() == WALL_CPU_BACKSTOP_FACTOR * 120


def test_wall_falls_back_to_default_when_no_cpu_budget() -> None:
    # No explicit wall AND no CPU budget to derive from -> the static hang backstop.
    limits = WorkerLimits(cpu_timeout_s=None, wall_timeout_s=None)
    assert limits.resolved_wall_timeout_s() == DEFAULT_WALL_TIMEOUT_S


def test_explicit_wall_wins_over_derivation() -> None:
    # An operator-set wall backstop is honoured verbatim, never overridden by the 3x rule.
    limits = WorkerLimits(cpu_timeout_s=120, wall_timeout_s=900)
    assert limits.resolved_wall_timeout_s() == 900


def test_wall_timeout_rejects_non_positive_when_set() -> None:
    with pytest.raises(ValueError):
        WorkerLimits(wall_timeout_s=0)


def test_detached_resource_report_and_unit_prefix_are_atomic() -> None:
    with pytest.raises(ValueError, match="must be set together"):
        ExperimentSpec(
            name="s",
            command=("run", "{seed}"),
            detached_resource_report="reports/{seed}.txt",
        )
    with pytest.raises(ValueError, match="must contain"):
        ExperimentSpec(
            name="s",
            command=("run", "{seed}"),
            detached_resource_report="reports/static.txt",
            detached_unit_prefix="worker-",
        )


def test_detached_execution_requires_every_enforceable_limit() -> None:
    with pytest.raises(
        ValueError, match="explicit memory_bytes, cpu_timeout_s, and pids_max"
    ):
        ExperimentSpec(
            name="s",
            command=("run", "{seed}"),
            detached_resource_report="reports/{seed}.txt",
            detached_unit_prefix="worker-",
        )
