"""Impure layer: cgroup bring-up, one boxed round, and the calibration-ramp orchestrator.

Containment is NOT reimplemented here — it is the exact two-level cgroup-v2 machinery from
``dagrun``, re-branded with an experiment-specific :class:`ScopeNaming` so a sweep's
scope/slice are named distinctly from a CI run's. That is the whole point: N concurrent seed VMs
run under the SAME boxing that CI steps do, with real per-worker ``memory.max`` / ``cpu.max`` /
CPU-second / wall caps and a clean ``cgroup.kill`` on breach.
"""

from __future__ import annotations

import os
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from dagrun import CgroupManager, RunResult, StepOutcome, run_dag_limited
from dagrun.cgroup import ScopeNaming

from parallel_experiment_runner.calibrate import (
    LiveCapacity,
    initial_width,
    live_capacity,
    measured_per_instance,
    ramp_next_width,
    resolve_width,
)
from parallel_experiment_runner.detached import (
    cleanup_registered_reports,
    cleanup_reports,
    read_report,
    register_reports,
    unregister_reports,
    verify_limits,
)
from parallel_experiment_runner.model import (
    MAX_CONCURRENCY,
    STATUS_CANCELLED,
    STATUS_CPU_TIMEOUT,
    STATUS_MEMORY_CAP,
    STATUS_LOG_CAP,
    STATUS_PIDS_CAP,
    STATUS_RESOURCE_REPORT_ERROR,
    STATUS_TIMEOUT,
    BREACH_STATUSES,
    CostEstimate,
    ExperimentSpec,
    ResourceSlice,
    RoundResult,
    SeedOutcome,
)
from parallel_experiment_runner.planner import (
    RoundPlan,
    classify_workload,
    generate_round_dag,
    worker_log_path,
)
from parallel_experiment_runner.profile import (
    ProfileStore,
    Sample,
    profile_identity,
)

#: The experiment scope's names — distinct from ``dagrun.slice`` so a sweep and a CI run box
#: under separate slices/scopes and never share a CPUQuota by accident.
EXPERIMENT_NAMING = ScopeNaming(
    slice_name="parallel-experiment.slice",
    unit_prefix="parallel-experiment",
    env_in_scope="PARALLEL_EXPERIMENT_IN_SCOPE",
    env_scope_unit="PARALLEL_EXPERIMENT_SCOPE_UNIT",
    env_direct_cgroup="PARALLEL_EXPERIMENT_DIRECT_CGROUP",
    log_prefix="[parallel-experiment]",
    supervisor_name="supervisor",
)

#: Program label used in bring-up diagnostics.
PROG = "parallel-experiment-runner"

#: Max bytes of a worker log tail scanned for the hit regex (bounded so a huge log is cheap).
LOG_TAIL_BYTES = 256 * 1024

Emit = Callable[[str], None]


def _contained_reexec_argv(
    arguments: Sequence[str], *, python_root: Path | None = None
) -> list[str]:
    """Build a re-exec argv that survives systemd's clean import environment.

    A repository-local invocation entered through ``py/bin`` only has the source root because
    that launcher inserted it into ``sys.path``. Re-executing ``python -m`` loses that in a fresh
    user-manager environment, so prefer the durable absolute launcher when it exists. Installed
    distributions have no adjacent launcher and can safely use their installed module.
    """

    root = python_root or Path(__file__).resolve().parents[1]
    launcher = root / "bin" / PROG
    if launcher.is_file():
        return [sys.executable, str(launcher.resolve()), *arguments]
    return [sys.executable, "-m", "parallel_experiment_runner", *arguments]


def resolve_cgroup_manager(allow_failure: bool) -> tuple[CgroupManager | None, int]:
    """Establish the two-level cgroup-v2 RESOURCE CONTAINMENT for a sweep (mirrors ``dagrun``'s
    CLI bring-up).

    This is a resource box, not a security sandbox: it defends against a BUG in our own code
    (leak memory, run forever, fork bomb) via cgroup CPU-time / memory / PID caps, and does NOT
    reach for seccomp or user-namespace isolation. Returns ``(manager, 0)`` when containment is
    active, ``(None, 0)`` for an intentional UNCONTAINED run (``allow_failure``), or ``(None, 3)``
    when containment is REQUIRED but unavailable and the caller must exit 3. Containment is ON BY
    DEFAULT: an uncontained sweep is the very failure mode this tool exists to prevent, so it
    happens only with an explicit opt-out.
    """
    from dagrun import cgroup as cg

    naming = EXPERIMENT_NAMING
    if os.environ.get(naming.env_in_scope) == "1":
        manager = cg.Cgroups(naming)
        if manager.enabled:
            cg.install_scope_teardown(
                naming=naming, on_teardown=cleanup_registered_reports
            )
            print(
                f"{PROG}: resource containment ACTIVE (two-level cgroup-v2 scope; per-worker "
                "CPU-time/memory/PID caps + setsid-proof teardown).",
                file=sys.stderr,
            )
            return manager, 0
        if allow_failure:
            print(
                f"{PROG}: warning: inside a scope but per-worker cgroup setup failed; running "
                "best-effort UNCONTAINED (--allow-cgroup-failure).",
                file=sys.stderr,
            )
            return None, 0
        print(
            f"{PROG}: ERROR: inside a managed scope but per-worker cgroups could not be set up; "
            "re-run with --allow-cgroup-failure to run UNCONTAINED.",
            file=sys.stderr,
        )
        return None, 3
    # OUTER process (not yet in a scope): re-exec into a fresh transient scope. We do NOT try to
    # "reap abandoned prior-run scopes" here — reproduction (a consuming repository's CI, five
    # abandonment scenarios) showed abandonment strands NOTHING: a SIGKILLed launcher's direct
    # children die with it, and install_scope_teardown's SIGTERM/SIGKILL/atexit hook cgroup.kill's
    # the whole scope on the exits that DO run a handler. The zombie pile-up that motivated this
    # tool was a LIVE hung `run --strict --verify` of a consuming repository's own tool (main
    # parked in tokio epoll_wait) holding a PID namespace open, NOT a leaked appender thread — and
    # a live hang is exactly what the per-worker cpu-time / wall backstop kills, via a
    # cgroup-subtree cgroup.kill that reclaims the namespace (see
    # dagrun.teardown.reap). Containment IS the fix; a next-run reaper is not needed.
    if allow_failure:
        print(
            f"{PROG}: warning: resource containment not established (--allow-cgroup-failure); "
            "running UNCONTAINED (process-group teardown only, no per-worker caps).",
            file=sys.stderr,
        )
        return None, 0
    argv = _contained_reexec_argv(sys.argv[1:])
    reexeced_or_skipped = cg.reexec_in_scope(argv, memory_max=None, naming=naming)
    detail = (
        "containment was skipped (e.g. CI without a systemd --user scope)"
        if reexeced_or_skipped
        else "cgroup-v2 + a working systemd --user scope are unavailable"
    )
    print(
        f"{PROG}: ERROR: resource containment could not be established: {detail}. Containing every "
        "worker is this tool's primary purpose; re-run with --allow-cgroup-failure to run "
        "UNCONTAINED.",
        file=sys.stderr,
    )
    return None, 3


def _row_float(row: Mapping[str, object], key: str) -> float | None:
    value = row.get(key)
    return float(value) if isinstance(value, (int, float)) else None


def _row_int(row: Mapping[str, object], key: str) -> int:
    value = row.get(key)
    return int(value) if isinstance(value, (int, float)) else 0


def _read_log_tail(path: Path, limit: int = LOG_TAIL_BYTES) -> str:
    """Read at most the last ``limit`` bytes of a worker log for regex hit-detection (never
    raises: a missing/unreadable log yields ""; only a set regex needs it anyway)."""
    try:
        size = path.stat().st_size
        with path.open("rb") as handle:
            if size > limit:
                handle.seek(size - limit)
            return handle.read().decode("utf-8", errors="replace")
    except OSError:
        return ""


def _breach_message(
    status: str,
    *,
    limits_cpu: int | None,
    limits_mem: int | None,
    limits_wall: int,
    limits_pids: int | None,
    cpu_s: float | None,
    peak_bytes: int | None,
    wall_s: float,
    oom_kills: int,
    pids_events: int,
) -> str:
    """A human-readable 'what breached and by how much' line for a breach status (requirement 4)."""
    if status == STATUS_CPU_TIMEOUT:
        used = f"{cpu_s:.1f}s" if cpu_s is not None else "unknown"
        return f"CPU-TIMEOUT: used {used} cpu >= budget {limits_cpu}s"
    if status == STATUS_MEMORY_CAP:
        used = f"{peak_bytes}B" if peak_bytes is not None else "unknown"
        cap = f"{limits_mem}B" if limits_mem is not None else "floor"
        return f"MEMORY-CAP: peak {used} >= memory.max {cap} ({oom_kills} oom_kill event(s))"
    if status == STATUS_PIDS_CAP:
        cap = f"{limits_pids}" if limits_pids is not None else "cap"
        return (
            f"PIDS-CAP: {pids_events} fork/clone(s) denied at pids.max {cap} "
            "(fork-bomb / PID-exhaustion containment; worker contained not killed, reaped by "
            "the cpu/wall guard)"
        )
    if status == STATUS_LOG_CAP:
        return "LOG-CAP: detached producer killed the worker at its output byte cap"
    if status == STATUS_TIMEOUT:
        return f"TIMEOUT: wall {wall_s:.0f}s >= {limits_wall}s backstop"
    if status == STATUS_CANCELLED:
        return "CANCELLED: eager-exit after another worker failed"
    return ""


def _classify_outcome(
    spec: ExperimentSpec,
    outcome: StepOutcome,
    row: Mapping[str, object],
    log_path: Path,
) -> SeedOutcome:
    """Fold a ``dagrun`` ``StepOutcome`` and its profile row into a :class:`SeedOutcome`.

    Breach precedence (cancel > cpu-timeout > memory-cap > pids-cap > timeout) is applied BEFORE
    any hit/miss decision, so an infrastructure kill is never counted as a discovered hit. The
    PID-cap axis sits below the two kill-based axes (a fork-bombing worker may also OOM or exhaust
    CPU) and above the plain wall timeout, since a denied fork is a more specific cause than a hang.
    """
    seed = int(outcome.tag.split(".", 1)[1])
    limits = spec.worker_limits
    cpu_s = _row_float(row, "cpu.usage_usec")
    cpu_s = cpu_s / 1_000_000 if cpu_s is not None else None
    peak = row.get("peak_bytes")
    peak_bytes = int(peak) if isinstance(peak, (int, float)) else None
    tasks_peak: int | None = None
    report_error = ""
    report_path_text = spec.resource_report_path(seed)
    if report_path_text is not None:
        assert spec.detached_unit_prefix is not None
        try:
            detached = read_report(
                Path(report_path_text).resolve(), spec.detached_unit_prefix
            )
            verify_limits(detached, limits)
        except (OSError, UnicodeError, ValueError) as error:
            report_error = str(error)
        else:
            peak_bytes = (peak_bytes or 0) + detached.memory_peak_bytes
            cpu_s = (cpu_s or 0.0) + detached.cpu_usage_nsec / 1_000_000_000
            tasks_peak = detached.tasks_peak
    wall_s = _row_float(row, "elapsed_s") or outcome.duration_s
    oom = _row_int(row, "oom_kills")
    # pids.events rides on the in-memory StepOutcome, NOT the CSV row: it stays off the
    # profile-store schema (Python/Rust CSV-header parity) yet still classifies a fork-bomb.
    pids_events = outcome.pids_events
    cpu_timed_out = bool(row.get("cpu_timed_out"))
    timed_out = bool(row.get("timed_out"))
    log_capped = False
    if report_path_text is not None and not report_error:
        oom += detached.memory_oom_kills
        pids_events += detached.tasks_limit_hits
        cpu_timed_out = cpu_timed_out or bool(detached.cpu_time_limit_hit)
        timed_out = timed_out or bool(detached.wall_time_limit_hit)
        log_capped = bool(detached.log_cap_hit)

    if outcome.aborted:
        status = STATUS_CANCELLED
    elif cpu_timed_out:
        status = STATUS_CPU_TIMEOUT
    elif oom > 0:
        status = STATUS_MEMORY_CAP
    elif pids_events > 0:
        status = STATUS_PIDS_CAP
    elif log_capped:
        status = STATUS_LOG_CAP
    elif timed_out:
        status = STATUS_TIMEOUT
    elif report_error:
        status = STATUS_RESOURCE_REPORT_ERROR
    else:
        log_text = _read_log_tail(log_path) if spec.hit.regex else ""
        status = classify_workload(spec.hit, outcome.returncode, log_text)

    if status == STATUS_RESOURCE_REPORT_ERROR:
        breach = f"RESOURCE-REPORT-ERROR: {report_error}"
    elif status in BREACH_STATUSES:
        breach = _breach_message(
            status,
            limits_cpu=limits.cpu_timeout_s,
            limits_mem=limits.memory_bytes,
            limits_wall=limits.resolved_wall_timeout_s(),
            limits_pids=limits.pids_max,
            cpu_s=cpu_s,
            peak_bytes=peak_bytes,
            wall_s=wall_s,
            oom_kills=oom,
            pids_events=pids_events,
        )
    else:
        breach = ""
    return SeedOutcome(
        seed=seed,
        status=status,
        returncode=outcome.returncode,
        wall_s=wall_s,
        cpu_s=cpu_s,
        peak_bytes=peak_bytes,
        tasks_peak=tasks_peak,
        breach=breach,
        log_path=str(log_path),
        resource_report_error=report_error,
    )


def execute_round(plan: RoundPlan, *, cgroups: CgroupManager | None) -> RoundResult:
    """Execute one round: lower to a DAG, run it BOXED at the declared width, map every worker.

    ``max_steps`` is the plan's worker width and ``max_cpus`` is that width times the declared
    per-worker cores, so worker concurrency and total CPU capacity remain independent.
    ``keep_going=True`` because a hit/crash in one seed must not cancel its siblings: the whole
    point is to sweep every seed in the batch.
    """
    plan.log_dir.mkdir(parents=True, exist_ok=True)
    dag = generate_round_dag(plan)
    # ``run_sweep`` already resolves through the machine cap, but keep the execution boundary
    # independently safe for programmatic callers constructing a RoundPlan directly.
    jobs = max(1, min(plan.width, len(plan.seeds), MAX_CONCURRENCY))
    max_cpus = jobs * plan.spec.worker_limits.cpu_cores

    detached_reports = tuple(
        (Path(path).resolve(), plan.spec.detached_unit_prefix)
        for seed in plan.seeds
        if (path := plan.spec.resource_report_path(seed)) is not None
        and plan.spec.detached_unit_prefix is not None
    )
    duplicate_paths = len({path for path, _prefix in detached_reports}) != len(
        detached_reports
    )
    if duplicate_paths:
        raise ValueError("detached resource report paths must be unique per seed")
    preexisting = [path for path, _prefix in detached_reports if path.exists()]
    if preexisting:
        raise ValueError(
            "detached resource report paths must not exist before launch: "
            + ", ".join(str(path) for path in preexisting)
        )
    register_reports(detached_reports)
    start = time.time()
    try:
        result: RunResult = run_dag_limited(
            dag,
            max_steps=jobs,
            max_cpus=max_cpus,
            cgroups=cgroups,
            keep_going=True,
            verbosity=1,
        )
    finally:
        cleanup_reports(detached_reports)
        unregister_reports(detached_reports)
    wall_s = time.time() - start

    rows_by_tag: dict[str, Mapping[str, object]] = {
        str(r.get("step")): r for r in result.step_profile_rows
    }
    outcomes: list[SeedOutcome] = []
    for outcome in result.outcomes:
        row = rows_by_tag.get(outcome.tag, {})
        log_path = worker_log_path(plan.log_dir, int(outcome.tag.split(".", 1)[1]))
        outcomes.append(_classify_outcome(plan.spec, outcome, row, log_path))
    # A seed whose dependencies never ran cannot happen (seeds are independent), but be defensive:
    seen = {o.seed for o in outcomes}
    for seed in plan.seeds:
        if seed not in seen:
            outcomes.append(
                SeedOutcome(
                    seed=seed,
                    status=STATUS_CANCELLED,
                    returncode=None,
                    wall_s=0.0,
                    cpu_s=None,
                    peak_bytes=None,
                    tasks_peak=None,
                    breach="CANCELLED: worker never ran (skipped by scheduler)",
                    log_path=str(worker_log_path(plan.log_dir, seed)),
                )
            )

    reaped = 0
    if cgroups is not None and getattr(cgroups, "enabled", False):
        reaped = cgroups.kill_all_remaining()

    total_cpu = sum(o.cpu_s for o in outcomes if o.cpu_s is not None)
    outcomes.sort(key=lambda o: o.seed)
    return RoundResult(
        width=jobs,
        seeds=plan.seeds,
        outcomes=tuple(outcomes),
        wall_s=wall_s,
        cpu_s=total_cpu,
        slice_revision=plan.slice_revision,
        limiting_dimension=plan.limiting_dimension,
        reaped_leftovers=reaped,
    )


@dataclass(frozen=True)
class SweepResult:
    """The whole sweep: the up-front ESTIMATE, every round, and the measured ACTUALS."""

    spec_name: str
    profile_key: str
    ephemeral: bool
    up_front: CostEstimate
    rounds: tuple[RoundResult, ...]
    outcomes: tuple[SeedOutcome, ...]
    total_wall_s: float
    total_cpu_s: float

    @property
    def hits(self) -> tuple[int, ...]:
        """Seeds that produced the experiment's target signal."""
        return tuple(o.seed for o in self.outcomes if o.is_hit)

    @property
    def breaches(self) -> tuple[SeedOutcome, ...]:
        """Worker outcomes that breached a declared resource limit."""
        return tuple(o for o in self.outcomes if o.is_breach)

    @property
    def throughput_seeds_per_s(self) -> float:
        """Measured completed-worker throughput for the whole sweep."""
        return len(self.outcomes) / self.total_wall_s if self.total_wall_s > 0 else 0.0


def _record_samples(
    store: ProfileStore, key: str, round_result: RoundResult
) -> int | None:
    """Persist the COMPLETED (non-breach) workers of a round under ``key`` for future estimates.

    Breached workers are excluded: a CPU-timeout worker's CPU seconds are the budget, not the
    workload's real cost, so recording them would bias the next estimate toward the cap. Returns
    the peak bytes observed this round (for calibration tightening), or None.
    """
    samples: list[Sample] = []
    peak_seen: int | None = None
    for o in round_result.outcomes:
        if o.is_breach:
            continue
        samples.append(
            Sample(
                wall_s=o.wall_s,
                cpu_s=o.cpu_s,
                peak_bytes=o.peak_bytes,
                disk_bytes=None,
                tasks_peak=o.tasks_peak,
            )
        )
        if o.peak_bytes is not None:
            peak_seen = (
                o.peak_bytes if peak_seen is None else max(peak_seen, o.peak_bytes)
            )
    store.record(key, samples)
    return peak_seen


def run_sweep(
    spec: ExperimentSpec,
    seeds: Sequence[int],
    *,
    cgroups: CgroupManager | None,
    work_dir: Path,
    log_dir: Path,
    profile_store: ProfileStore,
    slice_provider: Callable[[], ResourceSlice],
    ceiling: int,
    emit: Emit,
) -> SweepResult:
    """Drive the whole sweep: up-front estimate, mandatory 1->2->4 calibration ramp, per-round
    actuals, and a final aggregate — every concurrency width DECLARED and ENFORCED.

    ``slice_provider`` is re-read before every round so a coordinator shrink takes effect before
    the next launch (a grow only permits the next doubling, never an instant jump).
    """
    identity = profile_identity(spec)
    up_front = profile_store.estimate(identity.key)
    _emit_up_front(emit, spec, seeds, identity.key, identity.ephemeral, up_front)

    remaining = list(seeds)
    rounds: list[RoundResult] = []
    all_outcomes: list[SeedOutcome] = []
    width: int | None = None
    measured_peak: int | None = up_front.peak_mem_bytes
    round_index = 0

    while remaining:
        slice_ = slice_provider()
        live = live_capacity(work_dir)
        per_inst = measured_per_instance(spec.worker_limits, measured_peak)
        fit = resolve_width(slice_, live, per_inst, ceiling)
        target = initial_width(fit) if width is None else ramp_next_width(width, fit)
        if target < 1:
            emit(
                f"WAIT: the box is too small for even one worker right now "
                f"(limiting={fit.limiting_dimension}; cpu_slots={fit.cpu_slots}, "
                f"mem_slots={fit.mem_slots}, disk_slots={fit.disk_slots}). "
                f"{len(remaining)} seed(s) left un-run."
            )
            break
        width = target
        batch = tuple(remaining[:width])
        plan = RoundPlan(
            spec=spec,
            seeds=batch,
            width=width,
            slice_revision=slice_.revision,
            limiting_dimension=fit.limiting_dimension,
            log_dir=log_dir,
            per_worker_estimate=up_front,
        )
        round_index += 1
        emit(
            f"round {round_index}: launching width={min(width, len(batch))} "
            f"(limiting={fit.limiting_dimension}; lane rev {slice_.revision}, "
            f"cpu headroom {live.available_cpu_cores}/{live.cpu_cores} cores measured idle) "
            f"over {len(batch)} seed(s)…"
        )
        round_result = execute_round(plan, cgroups=cgroups)
        rounds.append(round_result)
        all_outcomes.extend(round_result.outcomes)
        remaining = remaining[width:]

        if not identity.ephemeral:
            peak = _record_samples(profile_store, identity.key, round_result)
            if peak is not None:
                measured_peak = (
                    peak if measured_peak is None else max(measured_peak, peak)
                )
        _emit_round_actual(emit, round_index, round_result)

    total_wall = sum(r.wall_s for r in rounds)
    total_cpu = sum(r.cpu_s for r in rounds)
    sweep = SweepResult(
        spec_name=spec.name,
        profile_key=identity.key,
        ephemeral=identity.ephemeral,
        up_front=up_front,
        rounds=tuple(rounds),
        outcomes=tuple(all_outcomes),
        total_wall_s=total_wall,
        total_cpu_s=total_cpu,
    )
    _emit_final(emit, sweep)
    return sweep


def _fmt_secs(value: float | None) -> str:
    return "UNSET" if value is None else f"{value:.1f}s"


def _fmt_mem(value: int | None) -> str:
    if value is None:
        return "UNSET"
    gib = value / 1024**3
    return f"{gib:.2f}GiB" if gib >= 1 else f"{value / 1024**2:.0f}MiB"


def _emit_up_front(
    emit: Emit,
    spec: ExperimentSpec,
    seeds: Sequence[int],
    key: str,
    ephemeral: bool,
    est: CostEstimate,
) -> None:
    n = len(seeds)
    marker = " (ephemeral — not persisted)" if ephemeral else ""
    if est.is_set:
        agg_cpu = None if est.cpu_s is None else est.cpu_s * n
        emit(
            f"ESTIMATE for '{spec.name}' [{key}{marker}] over {n} seed(s), from {est.samples} "
            f"prior sample(s): per-worker wall~{_fmt_secs(est.wall_s)}, cpu~{_fmt_secs(est.cpu_s)}, "
            f"peak~{_fmt_mem(est.peak_mem_bytes)}, tasks~"
            f"{est.peak_tasks if est.peak_tasks is not None else 'UNSET'}; "
            f"aggregate cpu~{_fmt_secs(agg_cpu)} "
            f"(wall depends on the calibrated width)."
        )
    else:
        emit(
            f"ESTIMATE for '{spec.name}' [{key}{marker}] over {n} seed(s): UNSET — no comparable "
            f"prior sample for this profile key. Cost is NOT MEASURED yet; calibration will "
            f"measure it. (An honest UNSET beats a fabricated number.)"
        )


def _emit_round_actual(emit: Emit, index: int, r: RoundResult) -> None:
    breaches = r.breaches
    breach_note = ""
    if breaches:
        detail = "; ".join(f"seed {b.seed}: {b.breach}" for b in breaches[:5])
        more = "" if len(breaches) <= 5 else f" (+{len(breaches) - 5} more)"
        breach_note = f" | {len(breaches)} breach(es): {detail}{more}"
    emit(
        f"round {index} ACTUAL: width={r.width}, wall={r.wall_s:.1f}s, cpu={r.cpu_s:.1f}s, "
        f"{len(r.hits)} hit(s), throughput={r.throughput_seeds_per_s:.3f} seed/s, "
        f"leftovers reaped={r.reaped_leftovers}{breach_note}"
    )


def _emit_final(emit: Emit, s: SweepResult) -> None:
    total = len(s.outcomes)
    emit(
        f"SWEEP DONE '{s.spec_name}' [{s.profile_key}]: {total} seed(s) in {len(s.rounds)} round(s), "
        f"wall={s.total_wall_s:.1f}s, cpu={s.total_cpu_s:.1f}s, {len(s.hits)} hit(s), "
        f"{len(s.breaches)} breach(es), throughput={s.throughput_seeds_per_s:.3f} seed/s."
    )
    if s.hits:
        shown = ", ".join(str(x) for x in s.hits[:20])
        more = "" if len(s.hits) <= 20 else f" (+{len(s.hits) - 20} more)"
        emit(f"  HITS: {shown}{more}")
