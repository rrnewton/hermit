#!/usr/bin/env python3
"""Analyze timestamped CPU-footprint JSONL emitted by :mod:`cpu_footprint_guest`.

CPU identity and CPU capacity are deliberately separate in this module.  An unpinned two-worker
run may use CPUs A/B in one bucket and C/D in the next; the four-CPU union is useful provenance,
but it is not four-way simultaneous execution.  Concurrency limits are evaluated from half-open
step/worker lifetimes, while cgroup ``cpu.max`` is evaluated as a long-window bandwidth limit with
an explicit CFS-period boundary allowance.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import sys
from collections import defaultdict
from collections.abc import Iterable, Mapping, Sequence


Event = dict[str, object]


@dataclass(frozen=True, order=True)
class WorkerIdentity:
    step: str
    worker: int
    pid: int

    def label(self) -> str:
        return f"{self.step}/w{self.worker}/p{self.pid}"


@dataclass(frozen=True)
class Interval:
    label: str
    start_ns: int
    end_ns: int

    def overlaps(self, start_ns: int, end_ns: int) -> bool:
        return self.start_ns < end_ns and start_ns < self.end_ns


@dataclass(frozen=True)
class BucketStats:
    start_ns: int
    end_ns: int
    live_steps: tuple[str, ...]
    live_workers: tuple[str, ...]
    sampled_cpus: tuple[int, ...]
    process_cpu_ns: int

    def to_object(self) -> dict[str, object]:
        return {
            "start_ns": self.start_ns,
            "end_ns": self.end_ns,
            "live_steps": list(self.live_steps),
            "live_workers": list(self.live_workers),
            "sampled_cpus": list(self.sampled_cpus),
            "process_cpu_ns": self.process_cpu_ns,
        }


@dataclass(frozen=True)
class FootprintStats:
    step_intervals: tuple[Interval, ...]
    worker_intervals: tuple[Interval, ...]
    max_live_steps: int
    max_live_workers: int
    sampled_cpu_union: tuple[int, ...]
    buckets: tuple[BucketStats, ...]
    issues: tuple[str, ...]

    def to_object(self) -> dict[str, object]:
        return {
            "step_intervals": [
                {"label": i.label, "start_ns": i.start_ns, "end_ns": i.end_ns}
                for i in self.step_intervals
            ],
            "worker_intervals": [
                {"label": i.label, "start_ns": i.start_ns, "end_ns": i.end_ns}
                for i in self.worker_intervals
            ],
            "max_live_steps": self.max_live_steps,
            "max_live_workers": self.max_live_workers,
            "sampled_cpu_union": list(self.sampled_cpu_union),
            "buckets": [bucket.to_object() for bucket in self.buckets],
            "issues": list(self.issues),
        }


@dataclass(frozen=True)
class LimitVerdict:
    ok: bool
    violations: tuple[str, ...]

    def to_object(self) -> dict[str, object]:
        return {"ok": self.ok, "violations": list(self.violations)}


@dataclass(frozen=True)
class BandwidthViolation:
    cgroup_path: str
    sampler_pid: int
    start_ns: int
    end_ns: int
    usage_delta_usec: int
    allowed_usec: int

    def to_object(self) -> dict[str, object]:
        return {
            "cgroup_path": self.cgroup_path,
            "sampler_pid": self.sampler_pid,
            "start_ns": self.start_ns,
            "end_ns": self.end_ns,
            "usage_delta_usec": self.usage_delta_usec,
            "allowed_usec": self.allowed_usec,
        }


@dataclass(frozen=True)
class BandwidthCheck:
    checkable: bool
    ok: bool
    checked_windows: int
    quota_cores: tuple[float, ...]
    max_observed_cores: float | None
    violations: tuple[BandwidthViolation, ...]
    reasons: tuple[str, ...]

    def to_object(self) -> dict[str, object]:
        return {
            "checkable": self.checkable,
            "ok": self.ok,
            "checked_windows": self.checked_windows,
            "quota_cores": list(self.quota_cores),
            "max_observed_cores": self.max_observed_cores,
            "violations": [violation.to_object() for violation in self.violations],
            "reasons": list(self.reasons),
        }


def _as_str(record: Mapping[str, object], key: str) -> str | None:
    value = record.get(key)
    return value if isinstance(value, str) else None


def _as_int(record: Mapping[str, object], key: str) -> int | None:
    value = record.get(key)
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def load_events(paths: Sequence[Path]) -> list[Event]:
    """Load one or more append-only guest logs, rejecting malformed JSONL records."""

    events: list[Event] = []
    for path in paths:
        with path.open(encoding="utf-8") as handle:
            for line_number, line in enumerate(handle, 1):
                if not line.strip():
                    continue
                try:
                    value: object = json.loads(line)
                except json.JSONDecodeError as exc:
                    raise ValueError(f"{path}:{line_number}: invalid JSON: {exc}") from exc
                if not isinstance(value, dict) or not all(
                    isinstance(key, str) for key in value
                ):
                    raise ValueError(f"{path}:{line_number}: record must be a JSON object")
                record: Event = {str(key): item for key, item in value.items()}
                if _as_str(record, "event") is None or _as_int(record, "monotonic_ns") is None:
                    raise ValueError(
                        f"{path}:{line_number}: record lacks string event/integer monotonic_ns"
                    )
                events.append(record)
    return events


def _completed_intervals(
    events: Sequence[Event], start_event: str, end_event: str, *, workers: bool
) -> tuple[list[Interval], list[str]]:
    starts: dict[object, int] = {}
    intervals: list[Interval] = []
    issues: list[str] = []
    ordered = sorted(events, key=lambda row: (_as_int(row, "monotonic_ns") or -1))
    for record in ordered:
        kind = _as_str(record, "event")
        if kind not in (start_event, end_event):
            continue
        timestamp = _as_int(record, "monotonic_ns")
        step = _as_str(record, "step")
        pid = _as_int(record, "pid")
        worker = _as_int(record, "worker") if workers else None
        if timestamp is None or step is None or pid is None or (workers and worker is None):
            issues.append(f"malformed {kind} record")
            continue
        if workers:
            assert worker is not None
            key: object = WorkerIdentity(step, worker, pid)
        else:
            key = (step, pid)
        label = key.label() if isinstance(key, WorkerIdentity) else f"{step}/p{pid}"
        if kind == start_event:
            if key in starts:
                issues.append(f"duplicate {start_event} for {label}")
            else:
                starts[key] = timestamp
            continue
        start = starts.pop(key, None)
        if start is None:
            issues.append(f"{end_event} without {start_event} for {label}")
        elif timestamp < start:
            issues.append(f"negative interval for {label}")
        else:
            intervals.append(Interval(label, start, timestamp))
    for key in sorted(starts, key=str):
        label = key.label() if isinstance(key, WorkerIdentity) else str(key)
        issues.append(f"{start_event} without {end_event} for {label}")
    return intervals, issues


def _max_overlap(intervals: Iterable[Interval]) -> int:
    boundaries: list[tuple[int, int]] = []
    for interval in intervals:
        if interval.end_ns <= interval.start_ns:
            continue
        boundaries.append((interval.start_ns, 1))
        boundaries.append((interval.end_ns, -1))
    # Half-open intervals: an end at t is processed before a start at t.
    boundaries.sort(key=lambda item: (item[0], item[1]))
    active = 0
    peak = 0
    for _timestamp, delta in boundaries:
        active += delta
        peak = max(peak, active)
    return peak


def _bucket_index(timestamp_ns: int, origin_ns: int, bucket_ns: int) -> int:
    return max(0, (timestamp_ns - origin_ns) // bucket_ns)


def analyze(events: Sequence[Event], *, bucket_ns: int = 50_000_000) -> FootprintStats:
    """Compute exact lifetime overlap and descriptive per-time-bucket CPU statistics."""

    if bucket_ns <= 0:
        raise ValueError("bucket_ns must be positive")
    steps, step_issues = _completed_intervals(
        events, "step_start", "step_end", workers=False
    )
    workers, worker_issues = _completed_intervals(
        events, "worker_start", "worker_end", workers=True
    )
    all_intervals = [*steps, *workers]
    if not all_intervals:
        return FootprintStats(
            tuple(), tuple(), 0, 0, tuple(), tuple(), tuple(step_issues + worker_issues)
        )

    first_ns = min(interval.start_ns for interval in all_intervals)
    last_ns = max(interval.end_ns for interval in all_intervals)
    origin_ns = (first_ns // bucket_ns) * bucket_ns
    bucket_count = max(1, (last_ns - origin_ns + bucket_ns - 1) // bucket_ns)
    if bucket_count > 1_000_000:
        raise ValueError(f"refusing {bucket_count} analysis buckets")

    live_steps: list[set[str]] = [set() for _ in range(bucket_count)]
    live_workers: list[set[str]] = [set() for _ in range(bucket_count)]
    sampled_cpus: list[set[int]] = [set() for _ in range(bucket_count)]
    process_cpu: list[int] = [0 for _ in range(bucket_count)]

    for interval, destinations in (
        *((interval, live_steps) for interval in steps),
        *((interval, live_workers) for interval in workers),
    ):
        first = _bucket_index(interval.start_ns, origin_ns, bucket_ns)
        last = _bucket_index(max(interval.start_ns, interval.end_ns - 1), origin_ns, bucket_ns)
        for index in range(first, min(last + 1, bucket_count)):
            bucket_start = origin_ns + index * bucket_ns
            if interval.overlaps(bucket_start, bucket_start + bucket_ns):
                destinations[index].add(interval.label)

    worker_samples: dict[WorkerIdentity, list[tuple[int, int]]] = defaultdict(list)
    for record in events:
        kind = _as_str(record, "event")
        if kind not in ("worker_start", "worker_sample", "worker_end"):
            continue
        timestamp = _as_int(record, "monotonic_ns")
        cpu_time = _as_int(record, "process_cpu_ns")
        cpu = _as_int(record, "cpu")
        step = _as_str(record, "step")
        worker = _as_int(record, "worker")
        pid = _as_int(record, "pid")
        if timestamp is None or step is None or worker is None or pid is None:
            continue
        index = _bucket_index(timestamp, origin_ns, bucket_ns)
        if 0 <= index < bucket_count and cpu is not None and cpu >= 0:
            sampled_cpus[index].add(cpu)
        if cpu_time is not None:
            worker_samples[WorkerIdentity(step, worker, pid)].append((timestamp, cpu_time))

    # Allocate cumulative process-CPU deltas across crossed wall buckets.  Integer interpolation
    # preserves each pair's exact total while avoiding a false spike in the bucket containing the
    # later sample.  These are descriptive statistics; hard quota checks use cpu.stat below.
    for samples in worker_samples.values():
        samples.sort()
        for (start_ns, start_cpu), (end_ns, end_cpu) in zip(samples, samples[1:]):
            wall_delta = end_ns - start_ns
            cpu_delta = end_cpu - start_cpu
            if wall_delta <= 0 or cpu_delta < 0:
                continue
            first = _bucket_index(start_ns, origin_ns, bucket_ns)
            last = _bucket_index(max(start_ns, end_ns - 1), origin_ns, bucket_ns)
            for index in range(first, min(last + 1, bucket_count)):
                bucket_start = max(start_ns, origin_ns + index * bucket_ns)
                bucket_end = min(end_ns, origin_ns + (index + 1) * bucket_ns)
                if bucket_end <= bucket_start:
                    continue
                before = cpu_delta * (bucket_start - start_ns) // wall_delta
                after = cpu_delta * (bucket_end - start_ns) // wall_delta
                process_cpu[index] += after - before

    buckets = tuple(
        BucketStats(
            start_ns=origin_ns + index * bucket_ns,
            end_ns=origin_ns + (index + 1) * bucket_ns,
            live_steps=tuple(sorted(live_steps[index])),
            live_workers=tuple(sorted(live_workers[index])),
            sampled_cpus=tuple(sorted(sampled_cpus[index])),
            process_cpu_ns=process_cpu[index],
        )
        for index in range(bucket_count)
    )
    union = tuple(sorted({cpu for bucket in sampled_cpus for cpu in bucket}))
    return FootprintStats(
        step_intervals=tuple(sorted(steps, key=lambda i: (i.start_ns, i.label))),
        worker_intervals=tuple(sorted(workers, key=lambda i: (i.start_ns, i.label))),
        max_live_steps=_max_overlap(steps),
        max_live_workers=_max_overlap(workers),
        sampled_cpu_union=union,
        buckets=buckets,
        issues=tuple(step_issues + worker_issues),
    )


def check_limits(
    stats: FootprintStats, *, max_steps: int | None = None, max_workers: int | None = None
) -> LimitVerdict:
    """Apply optional lifetime limits.  CPU-ID migration is intentionally not a violation."""

    violations = list(stats.issues)
    if max_steps is not None and stats.max_live_steps > max_steps:
        violations.append(
            f"live steps peaked at {stats.max_live_steps}, limit is {max_steps}"
        )
    if max_workers is not None and stats.max_live_workers > max_workers:
        violations.append(
            f"live workers peaked at {stats.max_live_workers}, limit is {max_workers}"
        )
    return LimitVerdict(not violations, tuple(violations))


def _parse_cpu_max(raw: str | None) -> tuple[int, int] | None:
    if raw is None:
        return None
    fields = raw.split()
    if len(fields) != 2 or fields[0] == "max":
        return None
    try:
        quota, period = int(fields[0]), int(fields[1])
    except ValueError:
        return None
    return (quota, period) if quota > 0 and period > 0 else None


def _parse_nonnegative(raw: str | None) -> int:
    try:
        value = int(raw or "0")
    except ValueError:
        return 0
    return max(0, value)


def _ceil_div(numerator: int, denominator: int) -> int:
    return (numerator + denominator - 1) // denominator


def check_cgroup_bandwidth(
    events: Sequence[Event],
    *,
    min_window_periods: int = 10,
    scheduler_slack_usec: int = 50_000,
) -> BandwidthCheck:
    """Check bounded ``cpu.stat`` windows against the recorded ``cpu.max``.

    A quota of Q microseconds per period P may legally spend Q near the end of one period and
    another Q immediately after replenishment.  An arbitrary wall interval therefore intersects
    up to ``ceil(window/P) + 1`` quota periods.  ``scheduler_slack_usec`` covers the small amount
    of per-CPU CFS runtime that may remain cached after global quota distribution; callers can set
    it to zero for synthetic traces and choose a host-appropriate explicit allowance for live data.
    """

    if min_window_periods < 1:
        raise ValueError("min_window_periods must be >= 1")
    if scheduler_slack_usec < 0:
        raise ValueError("scheduler_slack_usec must be >= 0")

    configs: dict[tuple[str, int], tuple[int, int, int]] = {}
    path_configs: dict[str, tuple[int, int, int]] = {}
    reasons: list[str] = []
    for record in events:
        if _as_str(record, "event") == "cgroup_unavailable":
            detail = _as_str(record, "reason") or "cgroup sampling unavailable"
            reasons.append(detail)
            continue
        if _as_str(record, "event") != "cgroup_config":
            continue
        path = _as_str(record, "cgroup_path")
        pid = _as_int(record, "pid")
        parsed = _parse_cpu_max(_as_str(record, "cpu_max"))
        if path is None or pid is None:
            continue
        if parsed is None:
            reasons.append(f"{path}: cpu.max is absent, malformed, or unbounded")
            continue
        quota, period = parsed
        burst = _parse_nonnegative(_as_str(record, "cpu_max_burst"))
        configs[(path, pid)] = (quota, period, burst)
        path_configs[path] = (quota, period, burst)

    series: dict[tuple[str, int], list[tuple[int, int, int]]] = defaultdict(list)
    for record in events:
        if _as_str(record, "event") != "cgroup_cpu_stat":
            continue
        path = _as_str(record, "cgroup_path")
        pid = _as_int(record, "pid")
        read_start = _as_int(record, "read_start_ns")
        read_end = _as_int(record, "read_end_ns")
        usage = _as_int(record, "usage_usec")
        if None in (path, pid, read_start, read_end, usage):
            continue
        assert path is not None and pid is not None
        assert read_start is not None and read_end is not None and usage is not None
        series[(path, pid)].append((read_start, read_end, usage))

    checked = 0
    violations: list[BandwidthViolation] = []
    quotas: set[float] = set()
    max_observed: float | None = None
    for key, samples in sorted(series.items()):
        path, pid = key
        config = configs.get(key) or path_configs.get(path)
        if config is None:
            reasons.append(f"{path}/pid {pid}: no bounded cpu.max config for cpu.stat series")
            continue
        quota_usec, period_usec, burst_usec = config
        quotas.add(quota_usec / period_usec)
        period_ns = period_usec * 1000
        minimum_ns = min_window_periods * period_ns
        samples.sort()
        for index, (first_start, first_end, first_usage) in enumerate(samples):
            for last_start, last_end, last_usage in samples[index + 1 :]:
                # Even under the narrowest placement of the two reads, this is a long window.
                if last_start - first_end < minimum_ns:
                    continue
                checked += 1
                if last_usage < first_usage:
                    violations.append(
                        BandwidthViolation(
                            path, pid, first_start, last_end, last_usage - first_usage, 0
                        )
                    )
                    continue
                elapsed_ns = max(1, last_end - first_start)
                periods_touched = _ceil_div(elapsed_ns, period_ns) + 1
                allowed = (
                    quota_usec * periods_touched + burst_usec + scheduler_slack_usec
                )
                used = last_usage - first_usage
                observed = used * 1000.0 / elapsed_ns
                max_observed = observed if max_observed is None else max(max_observed, observed)
                if used > allowed:
                    violations.append(
                        BandwidthViolation(path, pid, first_start, last_end, used, allowed)
                    )

    checkable = checked > 0
    if not series:
        reasons.append("no cgroup_cpu_stat samples")
    elif not checkable:
        reasons.append(
            f"no cpu.stat sample pair spans {min_window_periods} complete quota periods"
        )
    return BandwidthCheck(
        checkable=checkable,
        ok=checkable and not violations,
        checked_windows=checked,
        quota_cores=tuple(sorted(quotas)),
        max_observed_cores=max_observed,
        violations=tuple(violations),
        reasons=tuple(dict.fromkeys(reasons)),
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", help="guest JSONL file(s)")
    parser.add_argument("--bucket-ms", type=float, default=50.0)
    parser.add_argument("--max-steps", type=int)
    parser.add_argument("--max-workers", type=int)
    parser.add_argument("--check-bandwidth", action="store_true")
    parser.add_argument("--min-window-periods", type=int, default=10)
    parser.add_argument("--scheduler-slack-usec", type=int, default=50_000)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    ns = _parser().parse_args(list(argv) if argv is not None else None)
    bucket_ms = float(ns.bucket_ms)
    if bucket_ms <= 0:
        print("cpu_footprint_analysis: --bucket-ms must be positive", file=sys.stderr)
        return 2
    paths = [Path(str(raw)) for raw in ns.logs]
    try:
        events = load_events(paths)
        stats = analyze(events, bucket_ns=int(bucket_ms * 1_000_000))
        verdict = check_limits(
            stats,
            max_steps=int(ns.max_steps) if ns.max_steps is not None else None,
            max_workers=int(ns.max_workers) if ns.max_workers is not None else None,
        )
        bandwidth = (
            check_cgroup_bandwidth(
                events,
                min_window_periods=int(ns.min_window_periods),
                scheduler_slack_usec=int(ns.scheduler_slack_usec),
            )
            if bool(ns.check_bandwidth)
            else None
        )
    except (OSError, ValueError) as exc:
        print(f"cpu_footprint_analysis: {exc}", file=sys.stderr)
        return 2
    output: dict[str, object] = {
        "footprint": stats.to_object(),
        "limits": verdict.to_object(),
    }
    if bandwidth is not None:
        output["bandwidth"] = bandwidth.to_object()
    print(json.dumps(output, sort_keys=True, separators=(",", ":")))
    return 0 if verdict.ok and (bandwidth is None or bandwidth.ok) else 1


if __name__ == "__main__":
    raise SystemExit(main())
