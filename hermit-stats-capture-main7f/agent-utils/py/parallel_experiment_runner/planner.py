"""Lower one calibrated round onto a ``dagrun.DagConfig`` (pure) + classify outcomes.

This is where the runner "reuses dagrun; does NOT write a sibling": each seed becomes
one :class:`dagrun.Step`, and every per-worker HARD limit is lowered onto the exact
per-step control the executor already enforces —

* ``worker_limits.memory_bytes`` -> ``ResourceHint.hard_mem_max_bytes`` (inner ``memory.max``),
* ``worker_limits.cpu_cores``    -> ``ResourceHint.preferred_inner_jobs`` (inner ``cpu.max``, and
  the core-budget width unit),
* ``worker_limits.cpu_timeout_s``-> ``Step.cpu_timeout`` (cgroup ``cpu.stat`` CPU-second budget),
* ``worker_limits.wall_timeout_s``-> ``Step.timeout`` (load-tolerant wall backstop, DERIVED at
  ~3x the CPU budget when the spec leaves it unset; see ``WorkerLimits.resolved_wall_timeout_s``).

The fourth containment axis, ``worker_limits.pids_max`` (inner ``pids.max``, the fork-bomb
guard), is NOT a per-step serialized field: it is applied uniformly to every child cgroup by the
cgroup manager at runtime (``manager.set_worker_pids_max(...)``), which keeps it out of the
serialized DAG and profile schema.

``jobs_flag=""`` is set on every step so the executor never appends its own ``-j N`` to the
workload command — a seed sweep's command is a complete argv, not a build tool that takes a
parallelism flag. The CPU cap is applied independently of that flag, so the box still enforces it.
"""

from __future__ import annotations

import re
import shlex
from dataclasses import dataclass
from pathlib import Path

from dagrun import DagConfig, ResourceHint, Step

from parallel_experiment_runner.model import (
    STATUS_COMMAND_ERROR,
    STATUS_HIT,
    STATUS_MISS,
    CostEstimate,
    ExperimentSpec,
    HitCondition,
)

#: The DAG group every seed step lives under; the per-seed job is the seed value, so a step's
#: tag is ``seed.<value>`` — stable, and the key we map profile rows back to a seed by.
SEED_GROUP = "seed"


@dataclass(frozen=True)
class RoundPlan:
    """Everything one round needs: the spec, its seeds, the ENFORCED width, and where logs go."""

    spec: ExperimentSpec
    seeds: tuple[int, ...]
    width: int
    slice_revision: int
    limiting_dimension: str
    log_dir: Path
    per_worker_estimate: CostEstimate


def seed_tag(seed: int) -> str:
    """The stable DAG tag for a seed's worker step (``seed.<value>``)."""
    return f"{SEED_GROUP}.{seed}"


def worker_log_path(log_dir: Path, seed: int) -> Path:
    """The per-worker durable log path. Each worker redirects its OWN stdout+stderr here; the
    aggregate run prints only a summary, so a 400-wide round does not interleave 400 streams."""
    return log_dir / f"seed-{seed}.log"


def build_worker_command(spec: ExperimentSpec, seed: int, log_path: Path) -> str:
    """The shell command for one seed: the rendered argv (shell-quoted, injection-safe) with its
    own stdout+stderr redirected to ``log_path``.

    ``shlex.join`` quotes every rendered argument, so a seed or a spec argument containing shell
    metacharacters cannot break out of the command — the only shell syntax introduced is the
    redirection the runner itself adds.
    """
    argv = spec.render(seed)
    quoted = shlex.join(argv)
    return f"{quoted} > {shlex.quote(str(log_path))} 2>&1"


def generate_round_dag(plan: RoundPlan) -> DagConfig:
    """Build the ``DagConfig`` for a round: one independent, boxed :class:`Step` per seed.

    The steps have no ``deps`` (seeds are independent), so the executor runs up to ``plan.width``
    of them at once. The caller maps that to ``max_steps`` and separately sets
    ``max_cpus = width * per-worker cores``, making both limits declared and enforced.
    """
    limits = plan.spec.worker_limits
    est = plan.per_worker_estimate.wall_s or 0.0
    steps: list[Step] = []
    for seed in plan.seeds:
        log_path = worker_log_path(plan.log_dir, seed)
        hint = ResourceHint(
            hard_mem_max_bytes=limits.memory_bytes,
            preferred_inner_jobs=limits.cpu_cores,
            est_duration_s=est,
        )
        steps.append(
            Step(
                group=SEED_GROUP,
                job=str(seed),
                desc=f"seed {seed}",
                cmd=build_worker_command(plan.spec, seed, log_path),
                env=dict(plan.spec.env),
                hint=hint,
                timeout=limits.resolved_wall_timeout_s(),
                cpu_timeout=limits.cpu_timeout_s or 0,
                jobs_flag="",  # never append -j to a complete workload argv
            )
        )
    return DagConfig(steps=tuple(steps))


def classify_workload(hit: HitCondition, returncode: int | None, log_text: str) -> str:
    """Classify a NON-BREACHED worker as HIT / MISS / COMMAND-ERROR (design §8).

    Any infrastructure breach is decided by the caller FIRST and never reaches here, so a worker
    the runner had to kill can never be mistaken for a hit. Among clean exits: a regex match or a
    declared hit exit code is a HIT; a clean zero exit with no hit is a MISS; any other nonzero
    exit is a COMMAND-ERROR (workload failed for an unrelated reason).
    """
    matched_regex = bool(hit.regex) and re.search(hit.regex or "", log_text) is not None
    matched_exit = returncode is not None and returncode in hit.hit_exit_codes
    if matched_regex or matched_exit:
        return STATUS_HIT
    if returncode == 0:
        return STATUS_MISS
    return STATUS_COMMAND_ERROR
