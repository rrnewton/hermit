"""Resource reports and cleanup for workload units outside a worker cgroup.

Some trusted workload wrappers delegate their expensive child into a transient user
service.  That service is no longer a descendant of the experiment runner's per-worker
cgroup, so the ordinary cgroup sample under-counts it and cgroup teardown cannot reach it.
This module provides a deliberately small, opt-in contract for those wrappers.

The report is append-friendly UTF-8 text containing one ``key=value`` field per line.
An arbitrary prefix ending in ``": "`` is ignored.  A complete report contains:

``unit``
    The transient user-unit name.
``memory_peak_bytes``
    Peak memory attributed to the detached unit.
``cpu_usage_nsec``
    CPU usage attributed to the detached unit.
``tasks_peak``
    Peak task count attributed to the detached unit.

The report also attests the hard controls applied to that unit and reports their
monotonic breach counters.  Detached execution is refused unless those controls
exactly match the runner's declared per-worker limits.

Only units beginning with the caller-declared prefix are ever passed to ``systemctl``.
Commands use argv, never a shell, and unit names are syntax-checked first.
"""

from __future__ import annotations

import re
import subprocess
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

from parallel_experiment_runner.model import WorkerLimits


_UNIT_RE = re.compile(r"[A-Za-z0-9_.@:-]+")
_LOCK = threading.RLock()
_ACTIVE_REPORTS: dict[Path, str] = {}


@dataclass(frozen=True)
class DetachedResourceReport:
    """A complete resource sample for one detached transient unit."""

    unit: str
    memory_peak_bytes: int
    cpu_usage_nsec: int
    tasks_peak: int
    limit_memory_bytes: int
    limit_swap_bytes: int
    limit_tasks: int
    limit_cpu_cores_micros: int
    limit_cpu_time_nsec: int
    limit_wall_time_usec: int
    cpu_time_limit_hit: int
    wall_time_limit_hit: int
    log_cap_hit: int
    memory_oom_kills: int
    tasks_limit_hits: int


_NUMERIC_FIELDS = (
    "memory_peak_bytes",
    "cpu_usage_nsec",
    "tasks_peak",
    "limit_memory_bytes",
    "limit_swap_bytes",
    "limit_tasks",
    "limit_cpu_cores_micros",
    "limit_cpu_time_nsec",
    "limit_wall_time_usec",
    "cpu_time_limit_hit",
    "wall_time_limit_hit",
    "log_cap_hit",
    "memory_oom_kills",
    "tasks_limit_hits",
)
_REPORT_FIELDS = frozenset(("unit", *_NUMERIC_FIELDS))


def _fields(path: Path) -> dict[str, str]:
    fields: dict[str, str] = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        payload = raw.rsplit(": ", 1)[-1]
        key, separator, value = payload.partition("=")
        if not separator or key not in _REPORT_FIELDS:
            continue
        if key in fields:
            raise ValueError(f"detached resource report duplicates {key!r}")
        fields[key] = value
    return fields


def _validated_unit(fields: dict[str, str], unit_prefix: str) -> str | None:
    unit = fields.get("unit")
    if (
        unit is None
        or unit.startswith("-")
        or not unit.startswith(unit_prefix)
        or _UNIT_RE.fullmatch(unit) is None
    ):
        return None
    return unit if unit.endswith(".service") else f"{unit}.service"


def read_report(path: Path, unit_prefix: str) -> DetachedResourceReport:
    """Read one complete report, rejecting missing, malformed, or unowned fields."""

    fields = _fields(path)
    unit = _validated_unit(fields, unit_prefix)
    if unit is None:
        raise ValueError("detached resource report has no valid owned unit")
    numbers: dict[str, int] = {}
    for key in _NUMERIC_FIELDS:
        value = fields.get(key)
        if value is None or not value.isascii() or not value.isdecimal():
            raise ValueError(f"detached resource report has no unsigned {key}")
        numbers[key] = int(value)
    for field in ("cpu_time_limit_hit", "wall_time_limit_hit", "log_cap_hit"):
        if numbers[field] not in {0, 1}:
            raise ValueError(f"detached resource report {field} must be 0 or 1")
    return DetachedResourceReport(
        unit=unit,
        memory_peak_bytes=numbers["memory_peak_bytes"],
        cpu_usage_nsec=numbers["cpu_usage_nsec"],
        tasks_peak=numbers["tasks_peak"],
        limit_memory_bytes=numbers["limit_memory_bytes"],
        limit_swap_bytes=numbers["limit_swap_bytes"],
        limit_tasks=numbers["limit_tasks"],
        limit_cpu_cores_micros=numbers["limit_cpu_cores_micros"],
        limit_cpu_time_nsec=numbers["limit_cpu_time_nsec"],
        limit_wall_time_usec=numbers["limit_wall_time_usec"],
        cpu_time_limit_hit=numbers["cpu_time_limit_hit"],
        wall_time_limit_hit=numbers["wall_time_limit_hit"],
        log_cap_hit=numbers["log_cap_hit"],
        memory_oom_kills=numbers["memory_oom_kills"],
        tasks_limit_hits=numbers["tasks_limit_hits"],
    )


def verify_limits(report: DetachedResourceReport, limits: WorkerLimits) -> None:
    """Require the detached unit's applied controls to equal the declared envelope."""

    if (
        limits.memory_bytes is None
        or limits.cpu_timeout_s is None
        or limits.pids_max is None
    ):
        raise ValueError(
            "detached execution requires explicit memory, CPU-time, and task limits"
        )
    expected = {
        "limit_memory_bytes": limits.memory_bytes,
        "limit_swap_bytes": 0,
        "limit_tasks": limits.pids_max,
        "limit_cpu_cores_micros": limits.cpu_cores * 1_000_000,
        "limit_cpu_time_nsec": limits.cpu_timeout_s * 1_000_000_000,
        "limit_wall_time_usec": limits.resolved_wall_timeout_s() * 1_000_000,
    }
    for field, wanted in expected.items():
        observed = getattr(report, field)
        if observed != wanted:
            raise ValueError(
                f"detached resource report {field}={observed} does not match declared {wanted}"
            )


def known_unit(path: Path, unit_prefix: str) -> str | None:
    """Return a validated unit from an incomplete append-in-progress report."""

    try:
        return _validated_unit(_fields(path), unit_prefix)
    except (OSError, UnicodeError, ValueError):
        return None


def cleanup_reports(
    reports: Iterable[tuple[Path, str]], *, timeout_s: float = 5.0
) -> tuple[str, ...]:
    """Best-effort kill and stop of every distinct, validated unit named by reports."""

    units = tuple(
        sorted(
            {
                unit
                for path, prefix in reports
                if (unit := known_unit(path, prefix)) is not None
            }
        )
    )
    if not units:
        return ()
    commands = (
        [
            "systemctl",
            "--user",
            "kill",
            "--kill-whom=all",
            "--signal=SIGKILL",
            "--",
            *units,
        ],
        ["systemctl", "--user", "stop", "--", *units],
        ["systemctl", "--user", "reset-failed", "--", *units],
    )
    for command in commands:
        try:
            subprocess.run(
                command,
                check=False,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=timeout_s,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
    return units


def register_reports(reports: Iterable[tuple[Path, str]]) -> None:
    """Make report paths visible to the process-wide signal cleanup callback."""

    with _LOCK:
        _ACTIVE_REPORTS.update(reports)


def unregister_reports(reports: Iterable[tuple[Path, str]]) -> None:
    """Remove completed report paths from the signal cleanup registry."""

    with _LOCK:
        for path, _prefix in reports:
            _ACTIVE_REPORTS.pop(path, None)


def cleanup_registered_reports() -> None:
    """Bounded signal-teardown callback for the currently active report set."""

    with _LOCK:
        reports = tuple(_ACTIVE_REPORTS.items())
    cleanup_reports(reports, timeout_s=2.0)
