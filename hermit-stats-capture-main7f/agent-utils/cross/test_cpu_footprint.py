"""Focused, host-light tests for the shared CPU-footprint guest and analyzer."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys

import pytest

from cpu_footprint_analysis import (
    Event,
    analyze,
    check_cgroup_bandwidth,
    check_limits,
    load_events,
)
from cpu_footprint_guest import main as guest_main


MS = 1_000_000


def _event(
    kind: str,
    timestamp_ns: int,
    *,
    step: str,
    pid: int,
    worker: int | None = None,
    cpu: int = 0,
    process_cpu_ns: int = 0,
) -> Event:
    event: Event = {
        "event": kind,
        "monotonic_ns": timestamp_ns,
        "step": step,
        "pid": pid,
        "cpu": cpu,
        "process_cpu_ns": process_cpu_ns,
    }
    if worker is not None:
        event["worker"] = worker
    return event


def _worker_trace(
    *, step: str, worker: int, pid: int, cpus: tuple[int, int], start_ns: int = 0
) -> list[Event]:
    return [
        _event(
            "worker_start",
            start_ns,
            step=step,
            worker=worker,
            pid=pid,
            cpu=cpus[0],
            process_cpu_ns=0,
        ),
        _event(
            "worker_sample",
            start_ns + 25 * MS,
            step=step,
            worker=worker,
            pid=pid,
            cpu=cpus[0],
            process_cpu_ns=20 * MS,
        ),
        _event(
            "worker_sample",
            start_ns + 125 * MS,
            step=step,
            worker=worker,
            pid=pid,
            cpu=cpus[1],
            process_cpu_ns=70 * MS,
        ),
        _event(
            "worker_end",
            start_ns + 200 * MS,
            step=step,
            worker=worker,
            pid=pid,
            cpu=cpus[1],
            process_cpu_ns=100 * MS,
        ),
    ]


def test_migration_across_buckets_does_not_inflate_concurrency() -> None:
    events = [
        _event("step_start", 0, step="g.work", pid=100),
        *_worker_trace(step="g.work", worker=0, pid=101, cpus=(0, 2)),
        *_worker_trace(step="g.work", worker=1, pid=102, cpus=(1, 3)),
        _event("step_end", 200 * MS, step="g.work", pid=100),
    ]

    stats = analyze(events, bucket_ns=100 * MS)
    verdict = check_limits(stats, max_steps=1, max_workers=2)

    assert verdict.ok
    assert stats.max_live_steps == 1
    assert stats.max_live_workers == 2
    # The same two workers migrate A/B -> C/D.  Four CPUs over the whole run are provenance, not
    # evidence of four simultaneous workers.
    assert stats.sampled_cpu_union == (0, 1, 2, 3)
    assert stats.buckets[0].sampled_cpus == (0, 1)
    assert stats.buckets[1].sampled_cpus == (2, 3)


def test_excess_simultaneous_workers_fails_the_requested_limit() -> None:
    events = [
        _event("step_start", 0, step="g.work", pid=100),
        *_worker_trace(step="g.work", worker=0, pid=101, cpus=(0, 0)),
        *_worker_trace(step="g.work", worker=1, pid=102, cpus=(1, 1)),
        *_worker_trace(step="g.work", worker=2, pid=103, cpus=(2, 2)),
        _event("step_end", 200 * MS, step="g.work", pid=100),
    ]
    verdict = check_limits(analyze(events), max_steps=1, max_workers=2)
    assert not verdict.ok
    assert any("live workers peaked at 3" in violation for violation in verdict.violations)


def test_excess_simultaneous_steps_fails_the_requested_limit() -> None:
    events: list[Event] = []
    for index in range(3):
        events.extend(
            [
                _event("step_start", 0, step=f"g.s{index}", pid=100 + index),
                _event("step_end", 100 * MS, step=f"g.s{index}", pid=100 + index),
            ]
        )
    verdict = check_limits(analyze(events), max_steps=2)
    assert not verdict.ok
    assert any("live steps peaked at 3" in violation for violation in verdict.violations)


def test_half_open_intervals_do_not_overlap_at_a_handoff() -> None:
    events = [
        _event("step_start", 0, step="g.first", pid=100),
        _event("step_end", 100 * MS, step="g.first", pid=100),
        _event("step_start", 100 * MS, step="g.second", pid=101),
        _event("step_end", 200 * MS, step="g.second", pid=101),
    ]
    stats = analyze(events)
    assert stats.max_live_steps == 1
    assert check_limits(stats, max_steps=1).ok


def test_incomplete_worker_evidence_fails_closed() -> None:
    events = [
        _event("step_start", 0, step="g.work", pid=100),
        _event("worker_start", 0, step="g.work", worker=0, pid=101),
        _event("step_end", 100 * MS, step="g.work", pid=100),
    ]
    stats = analyze(events)
    verdict = check_limits(stats, max_steps=1, max_workers=1)
    assert not verdict.ok
    assert any("worker_start without worker_end" in issue for issue in stats.issues)


def _cgroup_config(*, quota: int = 200_000, period: int = 100_000) -> Event:
    return {
        "event": "cgroup_config",
        "step": "g.work",
        "pid": 100,
        "monotonic_ns": 0,
        "cgroup_path": "/sys/fs/cgroup/test.scope",
        "cpu_max": f"{quota} {period}",
        "cpu_max_burst": "0",
    }


def _cpu_stat(timestamp_ns: int, usage_usec: int) -> Event:
    return {
        "event": "cgroup_cpu_stat",
        "step": "g.work",
        "pid": 100,
        "monotonic_ns": timestamp_ns,
        "read_start_ns": timestamp_ns,
        "read_end_ns": timestamp_ns,
        "cgroup_path": "/sys/fs/cgroup/test.scope",
        "usage_usec": usage_usec,
    }


def test_long_window_two_core_bandwidth_with_boundary_slack_passes() -> None:
    # A 1-second arbitrary interval can touch eleven 100ms periods.  At Q=200ms/period, 2.1 CPU
    # seconds is legal even though it is slightly over the nominal 2.0-core average.
    result = check_cgroup_bandwidth(
        [_cgroup_config(), _cpu_stat(0, 10_000), _cpu_stat(1_000 * MS, 2_110_000)],
        min_window_periods=10,
        scheduler_slack_usec=0,
    )
    assert result.checkable and result.ok
    assert result.checked_windows == 1
    assert result.quota_cores == (2.0,)
    assert result.max_observed_cores == pytest.approx(2.1)


def test_sustained_four_core_bandwidth_fails_a_two_core_quota() -> None:
    result = check_cgroup_bandwidth(
        [_cgroup_config(), _cpu_stat(0, 0), _cpu_stat(1_000 * MS, 4_000_000)],
        min_window_periods=10,
        scheduler_slack_usec=0,
    )
    assert result.checkable and not result.ok
    assert len(result.violations) == 1
    assert result.violations[0].usage_delta_usec == 4_000_000
    assert result.violations[0].allowed_usec == 2_200_000


def test_short_cgroup_trace_is_explicitly_not_checkable() -> None:
    result = check_cgroup_bandwidth(
        [_cgroup_config(), _cpu_stat(0, 0), _cpu_stat(500 * MS, 1_000_000)],
        min_window_periods=10,
        scheduler_slack_usec=0,
    )
    assert not result.checkable
    assert not result.ok
    assert any("no cpu.stat sample pair" in reason for reason in result.reasons)


def test_unbounded_cpu_max_is_never_reported_as_a_passing_check() -> None:
    config = _cgroup_config()
    config["cpu_max"] = "max 100000"
    result = check_cgroup_bandwidth(
        [config, _cpu_stat(0, 0), _cpu_stat(1_000 * MS, 4_000_000)],
        min_window_periods=10,
        scheduler_slack_usec=0,
    )
    assert not result.checkable and not result.ok
    assert any("unbounded" in reason for reason in result.reasons)


@pytest.mark.skipif(sys.platform != "linux", reason="the guest intentionally targets Linux")
def test_guest_forks_workers_and_writes_parseable_atomic_jsonl(tmp_path: Path) -> None:
    log = tmp_path / "footprint.jsonl"
    assert (
        guest_main(
            [
                "--output",
                str(log),
                "--step",
                "guest.smoke",
                "--workers=2",
                "--duration-s",
                "0.06",
                "--sample-ms",
                "5",
                "--start-delay-ms",
                "10",
            ]
        )
        == 0
    )
    events = load_events([log])
    stats = analyze(events, bucket_ns=10 * MS)
    assert len(stats.step_intervals) == 1
    assert len(stats.worker_intervals) == 2
    assert not stats.issues
    assert all(_event_record.get("schema") == 1 for _event_record in events)
    assert all(
        isinstance(_event_record.get("cpu"), int)
        for _event_record in events
        if str(_event_record.get("event", "")).startswith("worker_")
    )


@pytest.mark.skipif(sys.platform != "linux", reason="the guest intentionally targets Linux")
def test_guest_barrier_releases_two_steps_on_one_future_epoch(tmp_path: Path) -> None:
    log = tmp_path / "barrier.jsonl"
    barrier = tmp_path / "ready"
    guest = Path(__file__).with_name("cpu_footprint_guest.py")

    def command(step: str) -> list[str]:
        return [
            sys.executable,
            str(guest),
            "--output",
            str(log),
            "--step",
            step,
            "--workers=1",
            "--duration-s",
            "0.08",
            "--sample-ms",
            "5",
            "--start-delay-ms",
            "30",
            "--barrier-file",
            str(barrier),
            "--barrier-participants",
            "2",
        ]

    processes = [subprocess.Popen(command(step)) for step in ("barrier.a", "barrier.b")]
    assert [process.wait(timeout=5) for process in processes] == [0, 0]
    stats = analyze(load_events([log]), bucket_ns=10 * MS)
    assert stats.max_live_steps == 2
    assert stats.max_live_workers == 2


def test_loader_rejects_a_torn_or_non_json_line(tmp_path: Path) -> None:
    log = tmp_path / "bad.jsonl"
    log.write_text(json.dumps({"event": "step_start", "monotonic_ns": 0}) + "\n{", encoding="utf-8")
    with pytest.raises(ValueError, match="invalid JSON"):
        load_events([log])
