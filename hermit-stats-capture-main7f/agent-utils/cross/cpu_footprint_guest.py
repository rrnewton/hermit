#!/usr/bin/env python3
"""Small Linux CPU-footprint workload used by the cross-engine tests.

The program deliberately uses OS processes rather than Python threads: every worker is an
independently schedulable task, so the GIL cannot turn a requested width into serial execution.
Every JSONL record is emitted with one ``O_APPEND`` ``write(2)`` call.  Several DAG steps and all
of their workers may therefore share one output file without a userspace lock or torn records.

``--workers`` is intentionally suitable for the ``jobs_flag`` a ``dagrun`` step declares, such as
``--workers=``.  The
``-j`` and ``--jobs`` spellings are accepted as conveniences for hand-written fixtures.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
from pathlib import Path
import signal
import sys
import time
from collections.abc import Mapping, Sequence


SCHEMA_VERSION = 1
CGROUP_ROOT = Path("/sys/fs/cgroup")
_MAX_RECORD_BYTES = 16 * 1024


def _current_cpu() -> int:
    """Return Linux's current logical CPU, or ``-1`` if neither probe is available."""

    try:
        libc = ctypes.CDLL(None, use_errno=True)
        sched_getcpu = libc.sched_getcpu
        sched_getcpu.argtypes = []
        sched_getcpu.restype = ctypes.c_int
        cpu = int(sched_getcpu())
        if cpu >= 0:
            return cpu
    except (AttributeError, OSError):
        pass

    # ``processor`` is field 39.  The command name in field 2 may contain spaces and parentheses,
    # so split only after its final closing parenthesis; the resulting first token is field 3.
    try:
        raw = Path("/proc/self/stat").read_text(encoding="utf-8")
        close = raw.rfind(")")
        fields = raw[close + 2 :].split()
        return int(fields[36])
    except (OSError, ValueError, IndexError):
        return -1


def _event(event: str, step: str, *, worker: int | None = None) -> dict[str, object]:
    record: dict[str, object] = {
        "schema": SCHEMA_VERSION,
        "event": event,
        "step": step,
        "monotonic_ns": time.monotonic_ns(),
        "process_cpu_ns": time.process_time_ns(),
        "pid": os.getpid(),
        "tid": os.getpid(),  # each forked worker is deliberately single-threaded
        "cpu": _current_cpu(),
    }
    if worker is not None:
        record["worker"] = worker
    return record


def _append_record(fd: int, record: Mapping[str, object]) -> None:
    """Append one compact JSON record with exactly one kernel write."""

    payload = (json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(payload) > _MAX_RECORD_BYTES:
        raise ValueError(f"CPU-footprint record is unexpectedly large ({len(payload)} bytes)")
    written = os.write(fd, payload)
    if written != len(payload):
        # Retrying a partial append could let another writer splice a record between the pieces.
        # Treat this as evidence loss instead of manufacturing a syntactically valid lie.
        raise OSError(f"short JSONL append: wrote {written} of {len(payload)} bytes")


def _read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def _current_cgroup() -> Path | None:
    try:
        lines = Path("/proc/self/cgroup").read_text(encoding="utf-8").splitlines()
    except OSError:
        return None
    for line in lines:
        fields = line.split(":", 2)
        if len(fields) == 3 and fields[0] == "0" and fields[1] == "":
            relative = fields[2].lstrip("/")
            return CGROUP_ROOT.joinpath(relative)
    return None


def _cgroup_ancestor(parent_levels: int) -> Path | None:
    current = _current_cgroup()
    if current is None:
        return None
    target = current
    for _ in range(parent_levels):
        if target == CGROUP_ROOT or CGROUP_ROOT not in target.parents:
            return None
        target = target.parent
    if target != CGROUP_ROOT and CGROUP_ROOT not in target.parents:
        return None
    return target


def _parse_cpu_stat(raw: str | None) -> dict[str, int]:
    values: dict[str, int] = {}
    if raw is None:
        return values
    for line in raw.splitlines():
        fields = line.split()
        if len(fields) != 2:
            continue
        try:
            values[fields[0]] = int(fields[1])
        except ValueError:
            continue
    return values


def _record_cgroup_config(fd: int, step: str, target: Path) -> None:
    record = _event("cgroup_config", step)
    record.update(
        {
            "cgroup_path": str(target),
            "cpu_max": _read_text(target / "cpu.max"),
            "cpu_max_burst": _read_text(target / "cpu.max.burst"),
            "cpuset_cpus_effective": _read_text(target / "cpuset.cpus.effective"),
        }
    )
    _append_record(fd, record)


def _record_cgroup_stat(fd: int, step: str, target: Path) -> bool:
    read_start_ns = time.monotonic_ns()
    values = _parse_cpu_stat(_read_text(target / "cpu.stat"))
    read_end_ns = time.monotonic_ns()
    if "usage_usec" not in values:
        return False
    record = _event("cgroup_cpu_stat", step)
    record.update(
        {
            "cgroup_path": str(target),
            "read_start_ns": read_start_ns,
            "read_end_ns": read_end_ns,
            "monotonic_ns": read_end_ns,
            "usage_usec": values["usage_usec"],
            "nr_periods": values.get("nr_periods"),
            "nr_throttled": values.get("nr_throttled"),
            "throttled_usec": values.get("throttled_usec"),
        }
    )
    _append_record(fd, record)
    return True


def _wait_until(deadline_ns: int) -> None:
    while True:
        remaining_ns = deadline_ns - time.monotonic_ns()
        if remaining_ns <= 0:
            return
        # Sleeping avoids making process creation part of the measured load.  Keep the final
        # interval short enough that workers begin close together even on coarse timers.
        time.sleep(min(remaining_ns / 1_000_000_000, 0.005))


def _barrier_release_ns(
    fd: int,
    step: str,
    barrier: Path,
    participants: int,
    timeout_s: float,
    release_delay_ms: float,
) -> int:
    """Join a cross-step barrier and return its shared future start epoch."""

    barrier.parent.mkdir(parents=True, exist_ok=True)
    ready_fd = os.open(barrier, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        identity = f"{step}:{os.getpid()}\n".encode()
        if os.write(ready_fd, identity) != len(identity):
            raise OSError("short barrier readiness append")
    finally:
        os.close(ready_fd)
    ready = _event("barrier_ready", step)
    ready["barrier"] = str(barrier)
    ready["participants"] = participants
    _append_record(fd, ready)

    release = barrier.with_name(barrier.name + ".release")
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        raw_release = _read_text(release)
        if raw_release is not None:
            try:
                release_ns = int(raw_release)
            except ValueError:
                # The release is published with rename below, so malformed text is persistent
                # corruption rather than a partially-observed write.
                raise RuntimeError(f"malformed barrier release epoch: {raw_release!r}")
            event = _event("barrier_release", step)
            event["barrier"] = str(barrier)
            event["release_ns"] = release_ns
            _append_record(fd, event)
            return release_ns
        try:
            identities = set(barrier.read_text(encoding="utf-8").splitlines())
        except OSError:
            identities = set()
        if len(identities) >= participants:
            release_ns = time.monotonic_ns() + int(release_delay_ms * 1_000_000)
            lock = release.with_name(release.name + ".lock")
            try:
                lock.mkdir(mode=0o700)
            except FileExistsError:
                continue
            temporary = release.with_name(f"{release.name}.tmp-{os.getpid()}")
            release_fd = os.open(
                temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
            )
            try:
                payload = f"{release_ns}\n".encode()
                if os.write(release_fd, payload) != len(payload):
                    raise OSError("short barrier release write")
            finally:
                os.close(release_fd)
            os.replace(temporary, release)
            continue
        time.sleep(0.005)
    raise TimeoutError(
        f"barrier {barrier} did not reach {participants} participants within {timeout_s:g}s"
    )


def _run_worker(
    fd: int,
    step: str,
    worker: int,
    start_at_ns: int,
    duration_ns: int,
    sample_ns: int,
) -> int:
    try:
        _wait_until(start_at_ns)
        _append_record(fd, _event("worker_start", step, worker=worker))
        deadline_ns = time.monotonic_ns() + duration_ns
        next_sample_ns = 0
        state = (worker + 1) * 0x9E3779B1
        while True:
            # Enough arithmetic between clock reads to make the process CPU-bound without relying
            # on an optimizer, native extension, or external executable.
            for _ in range(128):
                state = ((state * 1_664_525) + 1_013_904_223) & 0xFFFFFFFF
            now_ns = time.monotonic_ns()
            if now_ns >= next_sample_ns:
                record = _event("worker_sample", step, worker=worker)
                record["state"] = state
                _append_record(fd, record)
                next_sample_ns = now_ns + sample_ns
            if now_ns >= deadline_ns:
                break
        _append_record(fd, _event("worker_end", step, worker=worker))
        return 0
    except BaseException as exc:  # child must leave a durable reason before its immediate exit
        try:
            record = _event("worker_error", step, worker=worker)
            record["error"] = f"{type(exc).__name__}: {exc}"
            _append_record(fd, record)
        except BaseException:
            pass
        return 70


def _reap_nonblocking(remaining: set[int], statuses: dict[int, int]) -> None:
    for pid in tuple(remaining):
        try:
            found, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            remaining.discard(pid)
            statuses[pid] = 70
            continue
        if found == 0:
            continue
        remaining.remove(pid)
        statuses[pid] = os.waitstatus_to_exitcode(status)


def run_guest(
    *,
    output: Path,
    step: str,
    workers: int,
    duration_s: float,
    sample_ms: float,
    start_delay_ms: float,
    barrier_file: Path | None,
    barrier_participants: int | None,
    barrier_timeout_s: float,
    cgroup_parent_levels: int | None,
    cgroup_sample_ms: float,
) -> int:
    """Run one instrumented DAG-step workload and return its process status."""

    output.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    children: list[int] = []
    statuses: dict[int, int] = {}
    try:
        start = _event("step_start", step)
        start["workers"] = workers
        _append_record(fd, start)

        cgroup_target = (
            _cgroup_ancestor(cgroup_parent_levels)
            if cgroup_parent_levels is not None
            else None
        )
        if cgroup_target is not None:
            _record_cgroup_config(fd, step, cgroup_target)
            _record_cgroup_stat(fd, step, cgroup_target)
        elif cgroup_parent_levels is not None:
            unavailable = _event("cgroup_unavailable", step)
            unavailable["requested_parent_levels"] = cgroup_parent_levels
            unavailable["reason"] = "cgroup-v2 path could not be resolved"
            _append_record(fd, unavailable)

        if barrier_file is not None and barrier_participants is not None:
            try:
                start_at_ns = _barrier_release_ns(
                    fd,
                    step,
                    barrier_file,
                    barrier_participants,
                    barrier_timeout_s,
                    start_delay_ms,
                )
            except (OSError, RuntimeError, TimeoutError) as exc:
                record = _event("step_error", step)
                record["error"] = f"barrier failed: {exc}"
                _append_record(fd, record)
                end = _event("step_end", step)
                end["workers"] = workers
                end["ok"] = False
                end["worker_statuses"] = {}
                _append_record(fd, end)
                return 1
        else:
            start_at_ns = time.monotonic_ns() + int(start_delay_ms * 1_000_000)
        duration_ns = int(duration_s * 1_000_000_000)
        sample_ns = max(1, int(sample_ms * 1_000_000))
        try:
            for worker in range(workers):
                pid = os.fork()
                if pid == 0:
                    code = _run_worker(
                        fd, step, worker, start_at_ns, duration_ns, sample_ns
                    )
                    os.close(fd)
                    os._exit(code)
                children.append(pid)
        except OSError as exc:
            for pid in children:
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            record = _event("step_error", step)
            record["error"] = f"fork failed: {exc}"
            _append_record(fd, record)

        remaining = set(children)
        poll_s = max(0.001, cgroup_sample_ms / 1000.0)
        while remaining:
            _reap_nonblocking(remaining, statuses)
            if cgroup_target is not None:
                _record_cgroup_stat(fd, step, cgroup_target)
            if remaining:
                time.sleep(poll_s)
        if cgroup_target is not None:
            _record_cgroup_stat(fd, step, cgroup_target)

        ok = len(children) == workers and all(code == 0 for code in statuses.values())
        end = _event("step_end", step)
        end["workers"] = workers
        end["ok"] = ok
        end["worker_statuses"] = {str(pid): code for pid, code in sorted(statuses.items())}
        _append_record(fd, end)
        return 0 if ok else 1
    finally:
        os.close(fd)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="shared append-only JSONL output")
    parser.add_argument("--step", required=True, help="stable DAG step identity")
    parser.add_argument(
        "-j",
        "--jobs",
        "--workers",
        dest="workers",
        type=int,
        required=True,
        help="number of independently schedulable worker processes",
    )
    parser.add_argument("--duration-s", type=float, default=1.0)
    parser.add_argument("--sample-ms", type=float, default=10.0)
    parser.add_argument("--start-delay-ms", type=float, default=50.0)
    parser.add_argument("--barrier-file", default=None)
    parser.add_argument("--barrier-participants", type=int, default=None)
    parser.add_argument("--barrier-timeout-s", type=float, default=5.0)
    parser.add_argument(
        "--cgroup-parent-levels",
        type=int,
        default=None,
        help="also sample cpu.max/cpu.stat from this many cgroup parents above the step",
    )
    parser.add_argument("--cgroup-sample-ms", type=float, default=25.0)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    if sys.platform != "linux" or not hasattr(os, "fork"):
        print("cpu_footprint_guest: Linux with fork() is required", file=sys.stderr)
        return 2
    ns = _parser().parse_args(list(argv) if argv is not None else None)
    workers = int(ns.workers)
    duration_s = float(ns.duration_s)
    sample_ms = float(ns.sample_ms)
    start_delay_ms = float(ns.start_delay_ms)
    cgroup_sample_ms = float(ns.cgroup_sample_ms)
    barrier_file = Path(str(ns.barrier_file)) if ns.barrier_file is not None else None
    barrier_participants = (
        int(ns.barrier_participants) if ns.barrier_participants is not None else None
    )
    barrier_timeout_s = float(ns.barrier_timeout_s)
    parent_levels = (
        int(ns.cgroup_parent_levels) if ns.cgroup_parent_levels is not None else None
    )
    if workers < 1:
        print("cpu_footprint_guest: --workers must be >= 1", file=sys.stderr)
        return 2
    if duration_s <= 0 or sample_ms <= 0 or start_delay_ms < 0 or cgroup_sample_ms <= 0:
        print(
            "cpu_footprint_guest: duration/sample intervals must be positive "
            "(start delay may be zero)",
            file=sys.stderr,
        )
        return 2
    if parent_levels is not None and parent_levels < 0:
        print("cpu_footprint_guest: --cgroup-parent-levels must be >= 0", file=sys.stderr)
        return 2
    if (barrier_file is None) != (barrier_participants is None):
        print(
            "cpu_footprint_guest: --barrier-file and --barrier-participants must be given together",
            file=sys.stderr,
        )
        return 2
    if barrier_participants is not None and barrier_participants < 1:
        print("cpu_footprint_guest: --barrier-participants must be >= 1", file=sys.stderr)
        return 2
    if barrier_timeout_s <= 0:
        print("cpu_footprint_guest: --barrier-timeout-s must be positive", file=sys.stderr)
        return 2
    return run_guest(
        output=Path(str(ns.output)),
        step=str(ns.step),
        workers=workers,
        duration_s=duration_s,
        sample_ms=sample_ms,
        start_delay_ms=start_delay_ms,
        barrier_file=barrier_file,
        barrier_participants=barrier_participants,
        barrier_timeout_s=barrier_timeout_s,
        cgroup_parent_levels=parent_levels,
        cgroup_sample_ms=cgroup_sample_ms,
    )


if __name__ == "__main__":
    raise SystemExit(main())
