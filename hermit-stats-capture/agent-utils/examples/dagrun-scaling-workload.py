#!/usr/bin/env python3
"""Deterministic CPU/memory workload shapes for dagrun scaling sweeps.

This is intentionally dependency-free.  Every mode accepts ``--jobs`` so the DAG can use the
ordinary generic jobs-flag channel today and a typed command adapter later without changing the
benchmark itself.
"""

from __future__ import annotations

import argparse
import multiprocessing
import os
from collections.abc import Sequence
from multiprocessing.context import BaseContext


def _burn(arguments: tuple[int, int, int]) -> int:
    iterations, seed, memory_bytes = arguments
    # Touch every page so the requested memory contributes to the measured cgroup high-water mark.
    memory = bytearray(max(0, memory_bytes))
    for offset in range(0, len(memory), 4096):
        memory[offset] = (offset // 4096 + seed) & 0xFF
    value = (seed + 1) & 0xFFFFFFFFFFFFFFFF
    for index in range(iterations):
        value ^= (index + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF
        value = (value * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
        value ^= value >> 31
    return value ^ sum(memory[::4096])


def _split(total: int, workers: int) -> list[int]:
    quotient, remainder = divmod(max(0, total), workers)
    return [quotient + (1 if index < remainder else 0) for index in range(workers)]


def _run_workers(workers: int, total_work: int, memory_per_worker: int) -> int:
    arguments = [
        (iterations, index, memory_per_worker)
        for index, iterations in enumerate(_split(total_work, workers))
    ]
    if workers == 1:
        return _burn(arguments[0])
    # Fork avoids importing this module once per worker and keeps the fixture useful at large
    # widths.  Linux is dagrun's boxed execution platform; fall back to the default context where
    # fork is unavailable so the example remains runnable elsewhere.
    context: BaseContext
    try:
        context = multiprocessing.get_context("fork")
    except ValueError:
        context = multiprocessing.get_context()
    with context.Pool(processes=workers) as pool:
        return sum(pool.map(_burn, arguments)) & 0xFFFFFFFFFFFFFFFF


def run(kind: str, jobs: int, work: int, memory_per_worker_mib: int) -> int:
    if jobs < 1:
        raise ValueError("--jobs must be positive")
    memory_bytes = max(0, memory_per_worker_mib) * 1024 * 1024
    if kind == "parallel":
        active_workers = jobs
        checksum = _run_workers(active_workers, work, memory_bytes)
    elif kind == "four-core":
        active_workers = min(jobs, 4)
        checksum = _run_workers(active_workers, work, memory_bytes)
    elif kind == "sequential":
        # The useful work is serial. Wider requests add competing useless work, making CPU work
        # and memory rise without shortening the critical serial section.
        active_workers = 1
        checksum = _run_workers(1, work, memory_bytes)
        if jobs > 1:
            checksum ^= _run_workers(jobs - 1, work * (jobs - 1) // jobs, memory_bytes)
    else:  # argparse keeps this unreachable for the CLI; useful for library callers.
        raise ValueError(f"unknown workload kind {kind!r}")
    print(
        f"kind={kind} requested_jobs={jobs} active_workers={active_workers} "
        f"work={work} checksum={checksum:016x} pid={os.getpid()}"
    )
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kind", choices=("parallel", "four-core", "sequential"), required=True)
    parser.add_argument("--jobs", type=int, required=True)
    parser.add_argument("--work", type=int, default=30_000_000)
    parser.add_argument("--memory-per-worker-mib", type=int, default=1)
    args = parser.parse_args(argv)
    return run(args.kind, args.jobs, args.work, args.memory_per_worker_mib)


if __name__ == "__main__":
    raise SystemExit(main())
