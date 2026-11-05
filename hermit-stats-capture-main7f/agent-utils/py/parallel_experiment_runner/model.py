"""Pure data vocabulary for parallel-experiment-runner.

A seed sweep is described once as an :class:`ExperimentSpec` (an argument-vector template
with a ``{seed}`` placeholder, per-worker hard limits, and a hit predicate) and driven over
a range of seeds. Everything here is immutable data + pure helpers with no I/O, so the
planning/calibration logic is trivially testable and the runner never hides a scheduling or
containment implementation of its own — it lowers each round onto a
:class:`dagrun.DagConfig` and hands it to ``dagrun``.

This is RESOURCE CONTAINMENT, not a security sandbox. The workers run our own code; the
only thing we don't trust about it is its RESOURCE USAGE — code that leaks memory, runs
forever, or fork-bombs. So the box has four enforced axes, each mapped to one named failure
mode, and NONE of them reach for seccomp or user-namespace isolation:

* ``cpu``    — "run forever": a CPU-SECOND budget (not wall), the load-immune guard.
* ``memory`` — "leak memory": an inner ``memory.max`` cap; a breach is a self-contained OOM-kill.
* ``pids``   — "fork bomb": an inner ``pids.max`` cap. A fork bomb exhausts PIDs, not CPU or
  memory, so neither of the two above can contain one — this is a distinct, required axis.
* ``wall``   — defence-in-depth backstop only, load-DEPENDENT; derived at ~3x the CPU budget
  when a spec leaves it unset (see :func:`WorkerLimits.resolved_wall_timeout_s`).

Design note (why these mirror the design doc): the hard requirements — the four-axis box
above with CPU-time (not wall) budgets, a DECLARED+ENFORCED concurrency width, an up-front
cost ESTIMATE plus a measured ACTUAL, and a clean kill that NAMES what breached and by how
much — each map to a field or type below. See
``ai_docs/2026-07-29-parallel-experiment-runner-design.md``.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field

#: Wall-clock backstop (seconds) for a worker when a spec declares NO wall timeout AND no
#: CPU-second budget to derive one from. Wall time is LOAD-DEPENDENT, so it is only a
#: defence-in-depth hang backstop — the CPU-time budget is the real, load-immune guard
#: (see :attr:`WorkerLimits.cpu_timeout_s`).
DEFAULT_WALL_TIMEOUT_S = 1800

#: When a spec sets a CPU-second budget but no explicit wall backstop, the wall backstop is
#: derived at this multiple of the CPU budget. A worker legitimately spending C CPU-seconds
#: can take up to ~C wall-seconds when serialized on one core, plus scheduling slack under
#: load; 3x leaves generous headroom so the wall guard only ever fires on a true hang, never
#: races the (authoritative) CPU-second guard.
WALL_CPU_BACKSTOP_FACTOR = 3

#: Placeholder substituted with each concrete seed in a command template argument.
SEED_PLACEHOLDER = "{seed}"

#: A machine-level safety ceiling.  A caller may choose a smaller lane or command-line limit,
#: but no sweep may fan out beyond the largest host class this runner is intended to manage.
MAX_CONCURRENCY = 316


# Worker terminal-status vocabulary. A HIT is the target condition the sweep hunts for; a
# MISS ran cleanly without it. Everything else is an infrastructure outcome that is NEVER
# counted as a hit (design §8): a nonzero exit that is not a declared hit code, a wall hang,
# a CPU-budget / memory-cap / PID-cap breach, a disk-reserve stop, or an eager-cancel.
STATUS_HIT = "hit"
STATUS_MISS = "miss"
STATUS_COMMAND_ERROR = "command-error"
STATUS_TIMEOUT = "timeout"
STATUS_CPU_TIMEOUT = "cpu-timeout"
STATUS_MEMORY_CAP = "memory-cap"
STATUS_PIDS_CAP = "pids-cap"
STATUS_DISK_CAP = "disk-cap"
STATUS_LOG_CAP = "log-cap"
STATUS_RESOURCE_REPORT_ERROR = "resource-report-error"
STATUS_CANCELLED = "cancelled"

#: Statuses that represent a breach of a declared per-worker limit (reported distinctly and
#: never as a hit). ``command-error`` is a workload failure, not a limit breach.
BREACH_STATUSES = frozenset(
    {
        STATUS_TIMEOUT,
        STATUS_CPU_TIMEOUT,
        STATUS_MEMORY_CAP,
        STATUS_PIDS_CAP,
        STATUS_DISK_CAP,
        STATUS_LOG_CAP,
        STATUS_RESOURCE_REPORT_ERROR,
    }
)


@dataclass(frozen=True)
class WorkerLimits:
    """Per-worker HARD resource envelope — enforced, not advisory.

    Each of these lowers onto one ``dagrun`` per-step control so a single runaway
    seed is capped at its own characterized limit and cannot starve the host or its siblings:

    * ``cpu_cores`` -> the step's inner ``cpu.max`` cap (via ``preferred_inner_jobs``), so a
      worker cannot use more than N cores of quota.
    * ``memory_bytes`` -> the step's inner ``memory.max`` cap; a breach is an OOM-kill inside
      the worker's own child cgroup, leaving the run and host alive.
    * ``cpu_timeout_s`` -> the step's ``cpu_timeout`` (user+system CPU seconds, measured from
      the cgroup ``cpu.stat``). ``None`` means UNSET — the guard is disabled. This is
      DELIBERATE: a CPU budget must come from a real measured sample, never be derived from
      wall time. On an N-core host, N wall-seconds is up to N CPU-seconds, so a wall-derived
      CPU budget is ~1/N of what a worker needs and would FALSE-KILL healthy workers in a way
      indistinguishable from a flaky test. UNSET (honest, wall backstop still applies) beats a
      guessed, too-tight budget. This axis contains "run forever".
    * ``pids_max`` -> the step's inner ``pids.max`` cap (applied uniformly by the cgroup
      manager). This axis contains "fork bomb": a fork bomb exhausts PIDs, not CPU or memory,
      so neither ``cpu_timeout_s`` nor ``memory_bytes`` can stop one. Past the cap the kernel
      refuses further ``fork``/``clone`` (EAGAIN) — the worker is CONTAINED, not killed, and
      the CPU/wall guard reaps it. ``None`` means no PID cap.
    * ``wall_timeout_s`` -> the step's wall ``timeout``, a load-tolerant DEFENCE-IN-DEPTH hang
      backstop only. ``None`` means DERIVE it (see :meth:`resolved_wall_timeout_s`): ~3x the
      CPU-second budget when one is set, else :data:`DEFAULT_WALL_TIMEOUT_S`. The CPU-second
      budget, not this, is the authoritative guard.
    * ``disk_bytes`` -> the per-worker workspace/overlay budget used for the round's
      free-space reserve check (cgroup-v2 has no space controller; see the runner's disk
      handling).
    """

    cpu_cores: int = 1
    memory_bytes: int | None = None
    cpu_timeout_s: int | None = None
    pids_max: int | None = None
    wall_timeout_s: int | None = None
    disk_bytes: int | None = None

    def __post_init__(self) -> None:
        if self.cpu_cores < 1:
            raise ValueError(f"cpu_cores must be >= 1, got {self.cpu_cores}")
        if self.memory_bytes is not None and self.memory_bytes <= 0:
            raise ValueError(
                f"memory_bytes must be positive when set, got {self.memory_bytes}"
            )
        if self.cpu_timeout_s is not None and self.cpu_timeout_s <= 0:
            raise ValueError(
                f"cpu_timeout_s must be positive when set (None = UNSET), got {self.cpu_timeout_s}"
            )
        if self.pids_max is not None and self.pids_max < 1:
            raise ValueError(
                f"pids_max must be >= 1 when set (None = no cap), got {self.pids_max}"
            )
        if self.wall_timeout_s is not None and self.wall_timeout_s <= 0:
            raise ValueError(
                f"wall_timeout_s must be positive when set (None = derive), got {self.wall_timeout_s}"
            )
        if self.disk_bytes is not None and self.disk_bytes <= 0:
            raise ValueError(
                f"disk_bytes must be positive when set, got {self.disk_bytes}"
            )

    def resolved_wall_timeout_s(self) -> int:
        """The effective wall backstop in seconds, applying the derive-when-unset idiom.

        Wall time is only defence-in-depth, so its value is chosen to sit generously ABOVE the
        authoritative CPU-second guard, never to race it:

        * explicit ``wall_timeout_s`` -> used as-is;
        * else a CPU-second budget is set -> ``WALL_CPU_BACKSTOP_FACTOR`` x that budget;
        * else -> :data:`DEFAULT_WALL_TIMEOUT_S`.
        """
        if self.wall_timeout_s is not None:
            return self.wall_timeout_s
        if self.cpu_timeout_s is not None:
            return WALL_CPU_BACKSTOP_FACTOR * self.cpu_timeout_s
        return DEFAULT_WALL_TIMEOUT_S


@dataclass(frozen=True)
class HitCondition:
    """Predicate deciding whether a worker exhibited the target condition.

    A hit is declared when the worker's log matches ``regex`` OR its exit code is in
    ``hit_exit_codes``. An infrastructure breach (CPU/wall timeout, memory/PID/log/disk cap, or
    cancellation) is classified FIRST and is never a hit, so a worker the runner had to kill can
    never be mistaken for a discovered bug.
    """

    regex: str | None = None
    hit_exit_codes: tuple[int, ...] = ()

    def __post_init__(self) -> None:
        if self.regex is None and not self.hit_exit_codes:
            raise ValueError("HitCondition needs at least a regex or one hit exit code")


@dataclass(frozen=True)
class ExperimentSpec:
    """Immutable identity of one seed sweep: what to run, how to cap it, what counts as a hit.

    ``command`` is an argument vector (argv) in which every occurrence of ``{seed}`` in any
    element is replaced with the concrete seed. Using an argv (not a shell string) keeps the
    workload command injection-safe; the runner adds only its own log redirection wrapper.
    ``identity`` carries the apples-to-apples fields hashed into the automatic profile key
    (backend, image, kernel, vCPU/mem class, …); ``profile_key`` overrides the key with a
    human-readable label when an agent has judged the grouping comparable (see profile.py).
    Detached execution requires explicit memory, CPU-time, and PID limits because a producer
    must enforce and attest the same envelope on the child that left the worker cgroup.
    """

    name: str
    command: tuple[str, ...]
    worker_limits: WorkerLimits = field(default_factory=WorkerLimits)
    hit: HitCondition = field(default_factory=lambda: HitCondition(hit_exit_codes=(0,)))
    identity: Mapping[str, str] = field(default_factory=dict)
    profile_key: str | None = None
    env: Mapping[str, str] = field(default_factory=dict)
    detached_resource_report: str | None = None
    detached_unit_prefix: str | None = None

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("ExperimentSpec.name must be non-empty")
        if not self.command:
            raise ValueError("ExperimentSpec.command must have at least one element")
        if not any(SEED_PLACEHOLDER in part for part in self.command):
            raise ValueError(
                f"ExperimentSpec.command must contain the {SEED_PLACEHOLDER!r} placeholder "
                "in at least one argument (else every worker runs the identical command)"
            )
        if self.detached_resource_report is not None and (
            SEED_PLACEHOLDER not in self.detached_resource_report
        ):
            raise ValueError(
                "detached_resource_report must contain the '{seed}' placeholder"
            )
        if (self.detached_resource_report is None) != (
            self.detached_unit_prefix is None
        ):
            raise ValueError(
                "detached_resource_report and detached_unit_prefix must be set together"
            )
        if self.detached_unit_prefix is not None and not self.detached_unit_prefix:
            raise ValueError("detached_unit_prefix must be non-empty when set")
        if self.detached_resource_report is not None and (
            self.worker_limits.memory_bytes is None
            or self.worker_limits.cpu_timeout_s is None
            or self.worker_limits.pids_max is None
        ):
            raise ValueError(
                "detached execution requires explicit memory_bytes, cpu_timeout_s, and pids_max"
            )

    def render(self, seed: int) -> tuple[str, ...]:
        """The concrete argv for one seed (every ``{seed}`` substituted)."""
        return tuple(part.replace(SEED_PLACEHOLDER, str(seed)) for part in self.command)

    def resource_report_path(self, seed: int) -> str | None:
        """Render the optional report produced by a detached child resource domain."""

        if self.detached_resource_report is None:
            return None
        return self.detached_resource_report.replace(SEED_PLACEHOLDER, str(seed))


@dataclass(frozen=True)
class ResourceSlice:
    """A coordinator-assigned envelope for ONE lane at a point in time.

    The coordinator, not an individual runner, decides how the machine is carved among
    concurrent workstreams (a btrfs sweep, backend builds, CI). A runner reloads its lane
    before every round; a shrink takes effect before the next launch, a grow only permits the
    next calibration doubling (never an instant jump). ``revision`` lets the summary record
    exactly which carve each round observed. ``memory_reserve_bytes`` is subtracted from the
    live ``MemAvailable`` reading on every round, rather than only from a stale startup snapshot.
    """

    revision: int
    cpu_cores: int
    memory_bytes: int
    disk_bytes: int
    lane: str = ""
    memory_reserve_bytes: int = 0

    def __post_init__(self) -> None:
        if (
            self.cpu_cores < 0
            or self.memory_bytes < 0
            or self.disk_bytes < 0
            or self.memory_reserve_bytes < 0
        ):
            raise ValueError("ResourceSlice envelope values must be non-negative")


@dataclass(frozen=True)
class CostEstimate:
    """Up-front, DERIVED cost estimate for one worker (never a hardcoded constant).

    ``None`` fields mean UNSET: no comparable sample exists for the profile key yet, so the
    runner honestly reports "not measured" rather than inventing a plausible number. Both a
    per-worker figure and, at plan time, an aggregate for the whole round are rendered so the
    convention "estimate before, measure after, always visible" holds.
    """

    wall_s: float | None
    cpu_s: float | None
    peak_mem_bytes: int | None
    samples: int
    source: str  # the profile key the estimate came from, or "UNSET"
    peak_tasks: int | None = None

    @classmethod
    def unset(cls, source: str = "UNSET") -> "CostEstimate":
        """Return an estimate that truthfully carries no measured samples."""
        return cls(
            wall_s=None, cpu_s=None, peak_mem_bytes=None, samples=0, source=source
        )

    @property
    def is_set(self) -> bool:
        """Whether at least one comparable sample supports this estimate."""
        return self.samples > 0


@dataclass(frozen=True)
class SeedOutcome:
    """Terminal, structured result of ONE seed's worker.

    ``cpu_s``, ``peak_bytes``, and ``tasks_peak`` are the MEASURED actuals (including an
    opted-in detached resource domain), the other half of requirement 3. ``breach`` is a
    human-readable "what breached and by how much" string for :data:`BREACH_STATUSES`, ""
    otherwise.
    """

    seed: int
    status: str
    returncode: int | None
    wall_s: float
    cpu_s: float | None
    peak_bytes: int | None
    tasks_peak: int | None
    breach: str
    log_path: str
    resource_report_error: str = ""

    @property
    def is_hit(self) -> bool:
        """Whether this worker produced the experiment's target signal."""
        return self.status == STATUS_HIT

    @property
    def is_breach(self) -> bool:
        """Whether a declared resource-containment limit was breached."""
        return self.status in BREACH_STATUSES


@dataclass(frozen=True)
class RoundResult:
    """Aggregate outcome of one round (one generated DAG executed by dagrun)."""

    width: int
    seeds: tuple[int, ...]
    outcomes: tuple[SeedOutcome, ...]
    wall_s: float
    cpu_s: float
    slice_revision: int
    limiting_dimension: str
    reaped_leftovers: int = 0

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
        """Measured completed-worker throughput for this round."""
        return len(self.outcomes) / self.wall_s if self.wall_s > 0 else 0.0


@dataclass(frozen=True)
class CalibrationStage:
    """One rung of the mandatory 1 -> 2 -> 4 ramp: the width tried and what it measured."""

    width: int
    per_instance_peak_mem_bytes: int | None
    per_instance_cpu_s: float | None
    per_instance_wall_s: float | None
    limiting_dimension: str
    ok: bool


def render_command(spec: ExperimentSpec, seed: int) -> Sequence[str]:
    """Convenience wrapper for :meth:`ExperimentSpec.render` (kept for symmetry with the CLI)."""
    return spec.render(seed)
