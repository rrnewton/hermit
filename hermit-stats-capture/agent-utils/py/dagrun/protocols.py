"""Pluggable containment and metrics contracts used by the scheduler.

A :class:`CgroupManager` contains and measures each step, while a :class:`MetricsSink`
records per-step and whole-run results. Implementations may explicitly report unavailable
features, but requested enforcement and recording failures must remain visible.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Protocol, runtime_checkable

from dagrun.model import (
    DEFAULT_CPU_TIMEOUT_MULTIPLIER,
    IntentionalSkipReason,
    step_failure_reason,
)

__all__ = [
    "CgroupManager",
    "MetricsSink",
    "RunWindow",
    "StepOutcome",
    "RunResult",
]


@runtime_checkable
class CgroupManager(Protocol):
    """Per-step containment: create, cap, measure, and tear down a child cgroup per step.

    A single manager is constructed once for the whole run (inside the delegated outer
    scope). Each step is identified by its ``Step.tag`` ("group.job"); the manager lazily
    creates ``step-<tag>`` child cgroups on first :meth:`prepare_command` and reaps them on
    :meth:`kill` / :meth:`cleanup`.
    """

    #: Whether per-step containment is actually usable on this host. ``False`` when the
    #: outer cgroup was not delegated, cgroup-v2 is absent, or controller enablement
    #: failed. When ``False`` every method is a no-op and the scheduler must fall back to
    #: process-group kill for teardown. The scheduler reads this once to decide whether to
    #: treat the manager as present at all.
    enabled: bool

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        """Wrap a step's shell command so its bash leader joins the step's child cgroup
        BEFORE forking any grandchild (the cgroup-v2 fork-inheritance contract), applying
        the step's inner caps.

        Contract:

        * Creates the ``step-<tag>`` child cgroup (idempotent) and returns ``cmd`` prefixed
          with a self-move (``echo $$ > .../cgroup.procs``) so every descendant inherits
          the cgroup at fork — making later :meth:`kill` catch ``setsid``/double-fork
          escapees that a process-group kill misses.
        * ``mem_max`` (bytes), when given, is written to the child's ``memory.max`` as an
          INNER per-step RAM cap so one runaway step is OOM-killed at its own characterized
          limit, leaving the rest of the run and the host alive.
        * ``cpu_count``, when given, is written to the child's ``cpu.max`` as an inner CPU
          cap. A CPU-cap write that cannot be verified MUST make the returned command fail
          loudly (a command that prints an error and exits non-zero), never silently run
          uncapped.
        * When :attr:`enabled` is ``False`` (or the child cannot be created), returns
          ``cmd`` unchanged so the step still runs under the outer cap / killpg fallback.

        If a requested ``mem_max`` cannot be applied (for example, because the ``memory``
        controller was not delegated),
        the implementation MUST emit a visible degraded-enforcement warning. It MAY still
        run the step (outer cap remains the backstop) but MUST NOT skip the cap silently.
        """
        ...

    def prepare_delegated_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> tuple[str, str | None]:
        """Prepare an empty parent-owned step root for a nested scheduler.

        Returns the wrapped command and the verified root path to export. A manager that cannot
        provide cgroup delegation must return no path so the scheduler can fail closed.
        """
        ...

    def kill(self, tag: str) -> bool:
        """SIGKILL the step's entire cgroup subtree atomically (``cgroup.kill``), including
        ``setsid`` escapees that changed session/pgid but not cgroup membership.

        Returns ``True`` if the kill file was written, ``False`` if disabled, the step has
        no child cgroup, or the write failed. Best-effort; never raises.
        """
        ...

    def cleanup(self, tag: str) -> None:
        """Remove the step's now-empty child cgroup directory (best-effort ``rmdir``).

        A leftover process (``EBUSY``) is tolerated — the outer-scope teardown flushes it
        at end of run. Call AFTER reading :meth:`oom_kills` / :meth:`peak_bytes` /
        :meth:`cpu_stats`, which need the directory to still exist.
        """
        ...

    def set_worker_pids_max(self, limit: int | None) -> None:
        """Set a uniform per-step ``pids.max`` cap applied to every subsequently-prepared child
        cgroup (``None`` disables it). Runtime-only, never serialized in the DAG. The PID axis is
        the one a fork bomb exhausts that ``cpu.max`` / ``memory.max`` cannot contain.
        """
        ...

    def pids_events(self, tag: str) -> int:
        """Number of denied ``fork``/``clone`` events inside the step's cgroup (cgroup-v2
        ``pids.events`` ``max`` line).

        ``> 0`` means the step hit its INNER ``pids.max`` and a fork was refused — the
        actionable PID-exhaustion / fork-bomb signal, distinct from an OOM or a wall timeout.
        Unlike an OOM the kernel does not kill the process; it is contained and the cpu/wall
        guard reaps it. Returns ``0`` when disabled, unknown, or unreadable. Read BEFORE
        :meth:`cleanup`.
        """
        ...

    def oom_kills(self, tag: str) -> int:
        """Number of kernel OOM-kill events inside the step's cgroup (cgroup-v2
        ``memory.events`` ``oom_kill`` line).

        ``> 0`` means the step (or a descendant) hit its INNER ``memory.max`` and was
        killed — the actionable-OOM signal that distinguishes a memory-cap hit from an
        exit code or external signal. Returns ``0`` when disabled, unknown, or unreadable.
        Read BEFORE :meth:`cleanup`.
        """
        ...

    def peak_bytes(self, tag: str) -> int | None:
        """Peak resident memory (bytes) of the step's cgroup (``memory.peak``), for
        baseline characterization / retuning caps. ``None`` when disabled, unknown, or
        unreadable. Read BEFORE :meth:`cleanup`.

        A peak read alone does NOT say whether it is a true demand measurement: a peak
        equal to the cap in :meth:`applied_memory_max` is a CENSORED observation (the step
        used everything it was allowed), not an observed maximum. Pair it with
        :meth:`applied_memory_max` and :meth:`memory_events` before treating it as demand.
        """
        ...

    def applied_memory_max(self, tag: str) -> str | None:
        """The TIGHTEST ``memory.max`` AS THE KERNEL HOLDS IT over the step, verbatim: a
        decimal byte count, or the literal ``"max"`` when no level the runner can see bounds it.

        Not the step's own ``memory.max`` alone. cgroup-v2 limits are hierarchical and the
        runner always installs an outer scope cap, so a step given no inner cap reads ``"max"``
        at its own level while the scope's ceiling is what actually held it — and its own
        ``memory.events`` will not show it, because those counters do not count an ANCESTOR's
        limit events. An implementation reports the minimum over the levels it can see.

        Read back rather than echoed from the requested cap, because a requested cap that
        the kernel did not accept (an undelegated ``memory`` controller) is exactly the case
        where the two disagree, and only the accepted value explains a measured peak.

        ``None`` when disabled, unknown, or unreadable — which is DIFFERENT from ``"max"``:
        ``"max"`` says no ceiling was found at any level the runner can see, ``None`` says the
        cap is unknown. Nested managers preserve the finite minimum across the validated parent
        delegation and every visible ancestor through the cgroup namespace root. A consumer
        must not collapse ``None`` and ``"max"``; limits above a top-level run scope or outside
        that namespace remain unknowable. Read BEFORE :meth:`cleanup`.
        """
        ...

    def memory_events(self, tag: str) -> Mapping[str, int] | None:
        """The step cgroup's owned memory events as counter -> value (``low``, ``high``,
        ``max``, ``oom``, ``oom_kill``, ``oom_group_kill``, and any others the kernel exposes).

        These are per-step DELTAS without any subtraction: the child cgroup is created for
        this step and removed after it, so every counter starts at zero. ``max > 0`` with
        every OOM counter zero is the reclaim-at-cap case — the step completed, but only because
        the kernel kept evicting page cache at the ceiling, so its peak is censored and it
        was NOT OOM-killed. ``None`` when disabled, unknown, or unreadable.

        For an ordinary step this is its hierarchical ``memory.events`` file (there are no
        scheduler-created descendants). For a step prepared by
        :meth:`prepare_delegated_command`, it is ``memory.events.local``: inner schedulers own
        and classify their child-cgroup OOMs, while an event charged at the parent delegation
        boundary remains visible locally even when its victim is a descendant. In that last
        case ``oom`` (or ``oom_group_kill``) may be positive while the victim's ``oom_kill`` is
        accounted only in its descendant; schedulers must treat every positive OOM facet as an
        actionable boundary exhaustion, without reading sibling nested-run counters.
        Read BEFORE :meth:`cleanup`.
        """
        ...

    def cpu_stats(self, tag: str) -> Mapping[str, int] | None:
        """Per-step cgroup-v2 CPU counters from ``cpu.stat`` (e.g. ``usage_usec``,
        ``throttled_usec``), used to derive effective-core / quota-utilization metrics.
        ``None`` when disabled, unknown, or unreadable. Read BEFORE :meth:`cleanup`.
        """
        ...

    def cpu_pressure(self, tag: str) -> Mapping[str, float] | None:
        """Per-step CPU pressure-stall averages (``cpu.pressure`` ``some`` line: ``avg10``,
        ``avg60``). Sampled at step start and end to attribute contention. ``None`` when
        disabled, unknown, or unreadable.
        """
        ...

    def thread_count(self, tag: str) -> int | None:
        """Current descendant thread count from the step's ``cgroup.threads`` file, polled
        during the step to track per-step and concurrent thread peaks. ``None`` when
        disabled, unknown, or unreadable.
        """
        ...

    def kill_all_remaining(self) -> int:
        """NORMAL-EXIT backstop: ``cgroup.kill`` + ``rmdir`` EVERY step child cgroup that
        still exists (a ``setsid`` orphan a step left behind lives in that step's cgroup).

        Does NOT touch the supervisor/runner cgroup, so it never kills the runner and
        preserves the process exit code. Returns the count of step cgroups that still
        existed. Best-effort; never raises.
        """
        ...


@runtime_checkable
class RunWindow(Protocol):
    """A started measurement bracket around a whole run; :meth:`finish` records one row.

    Obtained from :meth:`MetricsSink.start_run_window` before the DAG runs. Starting the
    window captures baseline counters (wall clock, child-process CPU rusage, system-wide
    busy jiffies); :meth:`finish` computes the deltas and appends a single summary row.
    """

    def finish(
        self, *, result: str, n_steps: int, jobs: int
    ) -> Mapping[str, object] | None:
        """Compute window metrics and record the whole-run row.

        ``result`` is the run outcome recorded verbatim; ``n_steps`` is the number of steps
        actually run; ``jobs`` is the active-step ceiling (the existing persisted column name is
        unchanged). The remaining columns (wall time, our CPU vs other/system CPU contention,
        cores) are derived from the captured baseline.

        Returns the recorded row (heterogeneous, hence ``Mapping[str, object]``) for
        logging, or ``None`` when recording was skipped (e.g. the opt-in destination does
        not exist). A skip MUST be surfaced with a visible warning by the implementation,
        never dropped silently. MUST NOT raise into the run path — metrics recording never
        fails a run.
        """
        ...


@runtime_checkable
class MetricsSink(Protocol):
    """Durable recording of per-step and whole-run measurements.

    Carries the fixed run context (for example project directory, commit identifier, and machine
    identity) as construction state, so the scheduler passes only varying per-call data. A
    file-backed sink writes CSVs; a no-op sink records nothing.
    """

    def start_run_window(self) -> RunWindow:
        """Open (and start) the whole-run measurement bracket. Call once, immediately
        before the DAG runs; :meth:`RunWindow.finish` closes it after. A no-op sink returns
        a window whose ``finish`` is a no-op returning ``None``.
        """
        ...

    def record_step_profiles(
        self, rows: Sequence[Mapping[str, object]], *, jobs: int
    ) -> str | None:
        """Append per-step cgroup/measurement rows (accumulated during the run) to durable
        storage.

        Each row is a heterogeneous column→value mapping (``Mapping[str, object]``: strings,
        ints, floats) whose schema is owned by the caller and sink. ``jobs`` is the active-step
        ceiling stamped onto every row under the existing ``outer_jobs`` field.

        Returns a human-readable location descriptor for the recorded rows (a file-backed
        sink returns the CSV path as a string), or ``None`` when recording was skipped
        (e.g. the opt-in destination is absent). A skip MUST be surfaced with a visible
        warning, never dropped silently (No Silent Failure).
        """
        ...

    def record_step_timeseries(
        self, rows: Sequence[Mapping[str, object]], *, jobs: int
    ) -> str | None:
        """Write opt-in, interval-level per-step measurements to durable storage.

        Unlike :meth:`record_step_profiles`, these rows are a high-volume trace rather than one
        aggregate row per step. ``jobs`` is the maximum active-step count for the DAG execution.
        Returns an exact human-readable location, or ``None`` when recording was skipped; a
        requested sink must surface a skip.
        """
        ...


@dataclass(frozen=True)
class StepOutcome:
    """Terminal result of one step, including duration, status, and failure details."""

    #: The step's ``Step.tag`` ("group.job").
    tag: str
    #: Whether the step succeeded (exit 0, not timed out, not pids-guarded, detail captured).
    ok: bool
    #: Wall-clock seconds the step ran.
    duration_s: float
    #: One-line summary extracted from the step's detail log ("" when unavailable).
    summary: str
    #: Child process exit code; negative for a Unix signal; ``None`` if never collected.
    returncode: int | None
    #: Whether this step or one of its descendants hit the step's inner memory limit.
    oomed: bool
    #: Number of ``oom_kill`` events observed for this step's inner cgroup.
    oom_kills: int
    #: Whether the step exceeded its wall-clock bound.
    timed_out: bool
    #: Whether the step exceeded its CPU-time bound.
    cpu_timed_out: bool
    #: Human-readable failure reason (from :func:`step_failure_reason`); "" when :attr:`ok`.
    reason: str = ""
    #: True when eager-exit killed this in-flight step after ANOTHER step failed — a
    #: cancellation, reported distinctly from a genuine FAIL.
    aborted: bool = False
    #: Count of ``fork``/``clone`` calls the kernel denied at the step's inner ``pids.max``
    #: (the fork-bomb / PID-exhaustion signal), or 0. Carried IN MEMORY only — deliberately NOT
    #: written to the CSV profile store, so the Python/Rust CSV-header differential stays
    #: byte-identical while a consumer can still classify a PID-cap breach. ``> 0`` means the step
    #: was CONTAINED (fork refused), not kernel-killed; a cpu/wall guard does the actual reaping.
    pids_events: int = 0

    @classmethod
    def failed(
        cls,
        tag: str,
        *,
        duration_s: float,
        summary: str,
        returncode: int | None,
        oomed: bool,
        oom_kills: int,
        timed_out: bool,
        timeout: int,
        pids_guard_tripped: bool,
        pids_guard_reason: str | None,
        detail_write_failure: Sequence[str],
        cpu_timed_out: bool = False,
        cpu_timeout: int = 0,
        cpu_timeout_canonical: int = 0,
        cpu_timeout_multiplier: float = DEFAULT_CPU_TIMEOUT_MULTIPLIER,
        cpu_timeout_platform: str = "",
        aborted: bool = False,
        pids_events: int = 0,
    ) -> "StepOutcome":
        """Build a failed outcome, deriving :attr:`reason` from the shared precedence
        rule (OOM > CPU-timeout > timeout > pids-guard > detail-capture > signal > exit) so
        every caller reports failures identically.

        ``pids_events`` is retained on the instance (see the field docs) so a consumer can
        classify a PID-cap breach even when a higher-precedence reap (cpu/wall) owns the
        human-readable :attr:`reason`; it never affects ``reason``.
        """
        reason = step_failure_reason(
            returncode=returncode,
            oomed=oomed,
            oom_kills=oom_kills,
            timed_out=timed_out,
            timeout=timeout,
            pids_guard_tripped=pids_guard_tripped,
            pids_guard_reason=pids_guard_reason,
            detail_write_failure=detail_write_failure,
            cpu_timed_out=cpu_timed_out,
            cpu_timeout_canonical=cpu_timeout_canonical,
            cpu_timeout_multiplier=cpu_timeout_multiplier,
            cpu_timeout_platform=cpu_timeout_platform,
            cpu_timeout=cpu_timeout,
        )
        return cls(
            tag=tag,
            ok=False,
            duration_s=duration_s,
            summary=summary,
            returncode=returncode,
            oomed=oomed,
            oom_kills=oom_kills,
            timed_out=timed_out,
            cpu_timed_out=cpu_timed_out,
            reason=reason,
            aborted=aborted,
            pids_events=pids_events,
        )


@dataclass(frozen=True)
class RunResult:
    """Aggregate outcome of a whole DAG run, handed from the scheduler to the harness.

    ``ok`` is the overall pass/fail; ``outcomes`` are the per-step terminal results;
    ``skipped`` are tags whose dependencies failed so they never ran; ``not_launched`` are tags
    omitted by fail-fast termination or an outer run timeout; ``step_profile_rows``
    are the accumulated per-step measurement rows to forward to
    :meth:`MetricsSink.record_step_profiles`.
    """

    ok: bool
    wall_s: float
    outcomes: tuple[StepOutcome, ...] = ()
    skipped: tuple[str, ...] = ()
    #: Configured tags with no terminal outcome that were NOT dependency-skipped and NOT
    #: intentionally skipped: work the run simply never got to, because fail-fast tripped or the
    #: outer run budget cut the run short. Kept as its own bucket so absent work can never be
    #: mistaken for passing work by a consumer that only counts outcomes.
    not_launched: tuple[str, ...] = ()
    intentional_skips: tuple[tuple[str, IntentionalSkipReason], ...] = ()
    step_profile_rows: tuple[Mapping[str, object], ...] = field(default_factory=tuple)
    #: The WHOLE RUN hit its outer wall budget and was cut short. Distinct from a step's own
    #: ``timed_out``: no single node necessarily misbehaved, the combination did. A consumer that
    #: records results must be able to tell "this run produced a verdict about the tree" from
    #: "this run was stopped by its own budget with work outstanding", and ``ok`` alone cannot.
    run_timed_out: bool = False
    #: Largest number of step child processes observed alive at the same time. This is measured
    #: from successful process creation until wait() observes exit; it is not inferred from the
    #: requested job count or scheduler admission.
    max_concurrent_steps: int = 0
    #: Opt-in interval measurements of achieved parallelism within each step. Empty unless the
    #: caller requested time-series profiling; retained separately from the one-row-per-step
    #: profile dataset so existing estimators cannot accidentally treat intervals as trials.
    step_timeseries_rows: tuple[Mapping[str, object], ...] = field(default_factory=tuple)
