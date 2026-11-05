"""Memory-footprint modeling and safe concurrency selection.

The model enumerates dependency-compatible and resource-compatible steps that may run
together, then chooses the largest concurrency that fits the supplied memory budget.
"""

from __future__ import annotations

import itertools
import math
import os
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from dagrun.model import (
    DagConfig,
    Step,
    StepClass,
    effective_cpu_count,
    step_classification,
)

_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1
_MAX_EXACT_MEM_COMBINATIONS = 100_000


def _clamp_i64(value: int) -> int:
    """Clamp arbitrary-precision Python arithmetic to Rust's signed-64-bit domain."""
    return max(_I64_MIN, min(_I64_MAX, value))


def _saturating_add_i64(left: int, right: int) -> int:
    return _clamp_i64(left + right)


def _saturating_sum_i64(values: Sequence[int]) -> int:
    total = 0
    for value in values:
        total = _saturating_add_i64(total, value)
    return total


def _scaled_i64(value: int, factor: float) -> int:
    """Multiply through IEEE-754 binary64, then truncate/saturate exactly like Rust."""
    product = float(value) * factor
    if math.isnan(product):
        return 0
    if product >= float(_I64_MAX):
        return _I64_MAX
    if product <= float(_I64_MIN):
        return _I64_MIN
    return int(product)


def _scaled_for_width_i64(cap: int, inner_jobs: int) -> int:
    """Exact truncating ``cap * inner_jobs / 4`` with an i64-saturated result."""
    product = cap * inner_jobs
    scaled = product // 4 if product >= 0 else -((-product) // 4)
    return _clamp_i64(scaled)


def _scaled_between_widths_i64(cap: int, inner_jobs: int, baseline_jobs: int) -> int:
    """Exact truncating ``cap * inner_jobs / baseline_jobs`` with an i64-saturated result."""
    product = cap * inner_jobs
    scaled = product // baseline_jobs if product >= 0 else -((-product) // baseline_jobs)
    return _clamp_i64(scaled)


def step_mem_cap_bytes(
    step: Step, *, mem_cap_factor: float, default_cap_bytes: int | None = None
) -> int | None:
    """Inner-cgroup MemoryMax for a step. An explicit hard cap wins; otherwise ``factor x`` the
    RSS baseline. When the step declares NEITHER (uncharacterized), fall back to
    ``default_cap_bytes`` — the SMALL forcing-function default the scheduler passes from
    ``DagConfig.default_step_mem_cap_bytes`` — or None if no default is supplied."""
    if step.hint.hard_mem_max_bytes is not None and step.hint.hard_mem_max_bytes > 0:
        return step.hint.hard_mem_max_bytes
    base = step.hint.rss_baseline_bytes
    if base is not None and base > 0 and math.isfinite(mem_cap_factor) and mem_cap_factor > 0.0:
        return max(1, _scaled_i64(base, mem_cap_factor))
    return default_cap_bytes if default_cap_bytes is not None and default_cap_bytes > 0 else None


def step_mem_cap_for_inner_jobs(
    step: Step, inner_jobs: int | None, *, mem_cap_factor: float
) -> int:
    """Per-step cap scaled for internal parallelism.

    Conservative ``P x J`` model pending measured matrices: an explicit hard cap and
    non-CPU-bound steps keep the base cap; CPU-bound steps scale linearly above J=4.
    """
    return (
        _step_mem_cap_for_inner_jobs(
            step,
            inner_jobs,
            mem_cap_factor=mem_cap_factor,
            default_cap_bytes=None,
        )
        or 0
    )


def _step_mem_cap_for_inner_jobs(
    step: Step,
    inner_jobs: int | None,
    *,
    mem_cap_factor: float,
    default_cap_bytes: int | None,
) -> int | None:
    """Width-scaled cap while preserving an absent cap as ``None``.

    The public sizing helper historically maps an uncharacterized step to zero because such a
    step is excluded from its footprint sum. Runtime enforcement instead supplies the DAG's
    forcing-function default and must retain ``None`` when that default is disabled. Keeping the
    scaling arithmetic here gives planning and enforcement one implementation.
    """
    cap = step_mem_cap_bytes(
        step,
        mem_cap_factor=mem_cap_factor,
        default_cap_bytes=default_cap_bytes,
    )
    if cap is None:
        return None
    if (
        (
            step.hint.hard_mem_max_bytes is not None
            and step.hint.hard_mem_max_bytes > 0
        )
        or inner_jobs is None
        or step_classification(step) is not StepClass.CPU_BOUND
    ):
        return cap
    measured_width = step.hint.rss_baseline_inner_jobs
    if measured_width is not None and measured_width > 0:
        # An empirical M(p) already describes the complete step at ``measured_width``. Keep that
        # cap unchanged at the measured (or a narrower) width; only extrapolate if a caller later
        # widens the step beyond the point the planner installed.
        if inner_jobs <= measured_width:
            return cap
        return max(cap, _scaled_between_widths_i64(cap, inner_jobs, measured_width))
    return max(cap, _scaled_for_width_i64(cap, inner_jobs))


def transitive_deps(steps: Sequence[Step]) -> dict[str, set[str]]:
    """Map each step tag to all transitive dependencies without recursive call depth.

    Each root uses an explicit input-ordered DFS stack. This remains deterministic for reverse-
    topological graphs thousands of nodes deep and cannot hit Python's recursion limit.
    """
    direct = {step.tag: tuple(step.deps) for step in steps}
    result: dict[str, set[str]] = {}
    for tag in direct:
        deps: set[str] = set()
        stack = list(reversed(direct[tag]))
        while stack:
            dep = stack.pop()
            if dep in deps:
                continue
            deps.add(dep)
            stack.extend(reversed(direct.get(dep, ())))
        result[tag] = deps
    return result


def _too_many_combinations(n: int, width: int) -> bool:
    """Whether exact subsets up to ``width`` would exceed the deterministic search budget."""
    term = 1
    total = 0
    for count in range(1, width + 1):
        term = term * (n - count + 1) // count
        total += term
        if total > _MAX_EXACT_MEM_COMBINATIONS:
            return True
    return False


def schedulable_peak_mem_bytes(
    cfg: DagConfig,
    jobs: int,
    inner_jobs: int | None = None,
    *,
    widths: Mapping[str, int] | None = None,
) -> tuple[int, tuple[str, ...]]:
    """Maximum per-step-cap sum over any scheduler-reachable concurrent set of size <= jobs.

    A set is reachable only when no member transitively depends on another and the summed
    scarce-resource demand fits ``cfg.resource_caps``. Every runnable, non-skipped step
    participates: an explicit hard cap wins, an RSS baseline derives a cap, and an undeclared step
    uses ``default_step_mem_cap_bytes``. If that default is disabled, an uncharacterized runnable
    step is conservatively treated as unbounded and cannot fit a finite ``--max-mem`` budget.

    ``inner_jobs`` applies ONE internal-parallelism width to every step. ``widths`` instead
    supplies a PER-STEP width map. If neither supplies a width for a step, its effective applied
    width (positive ``preferred_inner_jobs``, else ``default_step_cpu_count``) is used. Thus the
    ordinary ``--max-mem`` path sizes the post-plan configuration it will actually execute, while
    the CPA allocator can override selected widths explicitly. Passing both is allowed;
    ``widths`` wins per tag.
    """
    by_tag = {step.tag: step for step in cfg.steps if step.skip_reason is None}
    tags = tuple(by_tag)
    width = min(max(1, jobs), len(tags))
    best_total = 0
    best: tuple[str, ...] = ()

    def cap_of(tag: str) -> int:
        step = by_tag[tag]
        width_for_step: int | None
        if widths is not None and tag in widths:
            width_for_step = widths[tag]
        elif inner_jobs is not None:
            width_for_step = inner_jobs
        else:
            width_for_step = effective_cpu_count(step, cfg.default_step_cpu_count)
        cap = _step_mem_cap_for_inner_jobs(
            step,
            width_for_step,
            mem_cap_factor=cfg.mem_cap_factor,
            default_cap_bytes=cfg.default_step_mem_cap_bytes,
        )
        return _I64_MAX if cap is None else cap

    if width == 0:
        return 0, ()
    if width == 1:
        # No dependency closure is needed when only one step may run. `max` keeps the first tag on
        # equal caps, so the tie rule is deterministic authored-config order.
        chosen_tag = max(tags, key=cap_of)
        return cap_of(chosen_tag), (chosen_tag,)

    # Exact antichain/resource enumeration is exponential. Above a fixed shared search budget,
    # conservatively ignore dependencies/resources and sum the largest `jobs` caps. That can only
    # overestimate reachable memory, never admit unsafe concurrency, and is deterministic in cfg
    # order for equal caps.
    if _too_many_combinations(len(tags), width):
        ranked = sorted(tags, key=cap_of, reverse=True)
        chosen = tuple(ranked[:width])
        return _saturating_sum_i64([cap_of(tag) for tag in chosen]), chosen

    dependencies = transitive_deps(list(cfg.steps))
    for count in range(1, width + 1):
        for candidate in itertools.combinations(tags, count):
            if any(
                left in dependencies[right] or right in dependencies[left]
                for left, right in itertools.combinations(candidate, 2)
            ):
                continue
            if any(
                _saturating_sum_i64(
                    [by_tag[tag].hint.resources.get(resource, 0) for tag in candidate]
                )
                > cap
                for resource, cap in cfg.resource_caps.items()
            ):
                continue
            total = 0
            for tag in candidate:
                total = _saturating_add_i64(total, cap_of(tag))
            if total > best_total:
                best_total, best = total, candidate
    return best_total, best


def jobs_footprint_bytes(cfg: DagConfig, jobs: int, inner_jobs: int | None = None) -> int:
    """Worst-case footprint at the given active-step count, clamped to the configured floor."""
    peak, _ = schedulable_peak_mem_bytes(cfg, jobs, inner_jobs)
    return _outer_mem_footprint_bytes(cfg, peak)


def _outer_mem_footprint_bytes(cfg: DagConfig, peak: int) -> int:
    """Apply the run-level floor and safety factor to one modeled peak, saturating to i64."""
    if peak >= _I64_MAX:
        return _I64_MAX
    factor = cfg.outer_mem_safety_factor
    if math.isfinite(factor) and factor > 0.0:
        scaled = max(1, _scaled_i64(peak, factor)) if peak > 0 else 0
    else:
        scaled = _I64_MAX
    return max(
        max(0, _clamp_i64(cfg.mem_cap_floor_bytes)),
        scaled,
    )


def _memory_footprint_fits(footprint: int, budget: int) -> bool:
    """Whether a finite, non-overflowed footprint fits a finite budget."""
    return 0 <= footprint < _I64_MAX and footprint <= budget


def jobs_for_budget(cfg: DagConfig, budget: int) -> tuple[int, int]:
    """Largest active-step count whose worst-case footprint fits ``budget``.

    The count is capped at the CPU count. Returns ``(0, one_step_footprint)`` when even one runnable
    step (or the configured floor) exceeds ``budget``; callers must refuse rather than silently
    execute an infeasible graph.
    """
    ncpu = os.cpu_count() or 4
    one = jobs_footprint_bytes(cfg, 1)
    if not _memory_footprint_fits(one, budget):
        return 0, one
    best = 1
    for n in range(1, ncpu + 1):
        if _memory_footprint_fits(jobs_footprint_bytes(cfg, n), budget):
            best = n
        else:
            break  # footprint is monotonic non-decreasing in n
    return best, jobs_footprint_bytes(cfg, best)


# Worst-case peak RSS of ONE compile/link job, used to bound build parallelism by the
# memory a boxed scope actually grants. DERIVED from a measured footprint, never guessed:
# <repo>#1584's build.dbi_release OOM-killed at the 8.0 GiB cap under j=32 (memory.events
# oom_kill=2) yet is stable at j8 (dag-mem-caps-pinned-jobs-fix #1583), i.e. ~8.0 GiB / 8
# ~= 1.0 GiB is the largest per-job footprint that still fits. This constant is the single
# source for "how many build jobs does a given memory cap afford" so the -j a cap implies
# is never re-derived (or drift) at a second site.
PER_BUILD_JOB_MEM_BYTES = 1 * 1024**3


def derive_build_jobs(cpu_count: int | None, mem_max_bytes: int | None) -> int:
    """Derive a bounded build-job count from colocated CPU and memory caps.

    Build tools commonly auto-detect all granted cores without accounting for the memory
    needed by each concurrent compilation. Keep the parallelism decision with its caps:

      jobs = min(granted_cores, mem_cap // PER_BUILD_JOB_MEM_BYTES)

    ``cpu_count`` is the granted cores (a step's inner ``cpu.max``, or the scope's effective
    quota for an unpinned step); ``mem_max_bytes`` is the co-located memory cap (the step's
    ``memory.max`` or the scope's). Either bound alone is insufficient. The result is always
    at least one; rejecting an undersized cap remains the caller's scheduling decision.
    """
    cores = cpu_count if (cpu_count is not None and cpu_count > 0) else (os.cpu_count() or 1)
    jobs = int(cores)
    if mem_max_bytes is not None and mem_max_bytes > 0:
        jobs = min(jobs, mem_max_bytes // PER_BUILD_JOB_MEM_BYTES)
    return max(1, int(jobs))


#: The build-width variable the runner controls. cargo reads it, and the ``NUM_JOBS`` it exports
#: to build scripts follows it.
BUILD_JOBS_ENV = "CARGO_BUILD_JOBS"

#: The operator's own build width, carried ACROSS the runner's systemd re-exec.
#:
#: This exists because ``CARGO_BUILD_JOBS`` alone cannot answer "did a human ask for this?".
#: The runner writes that variable itself: ``reexec_in_scope`` passes
#: ``--setenv=CARGO_BUILD_JOBS=...`` into the scope in BOTH engines, which is the live path.
#: (``enter_delegated_scope`` assigns it too, but that function has no caller — see its docstring
#: — so the re-exec is the only way this happens today.) The in-scope process therefore reads back
#: the runner's own derivation and would mistake it for an instruction. Then
#: per-step downward refinement stops, every step keeps the whole scope's width, and that is
#: exactly the 284-wide-against-8-GiB condition this machinery exists to prevent.
#:
#: So intent is resolved ONCE, in the outermost process, from the truly ambient environment, and
#: passed forward under its own name. PRESENCE of this variable means "already resolved"; an
#: empty value means "resolved, and the operator asked for nothing".
OPERATOR_BUILD_JOBS_ENV = "DAGRUN_OPERATOR_BUILD_JOBS"


def parse_build_jobs(raw: str | None) -> int | None:
    """A build width an operator can be said to have CHOSEN: a positive decimal integer.

    Absent, empty, malformed, or non-positive is ``None`` — not an instruction. A zero or a typo
    read as intent would hand the whole run a width nobody picked, which is worse than falling
    back to a derivation whose reasoning is stated.

    ASCII, and bounded, both DELIBERATELY:

    * ``str.isdigit()`` is true for characters ``int()`` cannot parse. ``CARGO_BUILD_JOBS='8²'``
      raised ``ValueError`` out of this function — and since the module-level capture below runs
      at IMPORT, that traceback took down ``import dagrun`` itself, so even
      ``capabilities`` and ``--help`` died. It is also true for characters ``int()`` CAN parse but
      no ASCII-digit test accepts: a full-width ``'８'`` is not a width an operator typed into a
      build variable, and honouring it while a stricter reader ignores it turns one input into two
      answers.
    * a width wider than a signed 64-bit integer is not a width. Python's ``int`` is unbounded and
      has to be told, via the same :data:`_I64_MAX` the rest of this module already uses to keep
      its arithmetic inside the domain every reader of these numbers shares. Leading zeros are
      stripped first, because a signed-integer parse accepts them: ``'000000008'`` is 8.
    """
    if raw is None:
        return None
    text = raw.strip()
    if not text.isascii() or not text.isdigit():
        return None
    if len(text.lstrip("0")) > len(str(_I64_MAX)):
        # Bail before int(): CPython refuses to convert very long digit strings at all
        # (``int_max_str_digits``), and that refusal is another ValueError at import time.
        return None
    value = int(text)
    if value > _I64_MAX:
        return None
    return value if value > 0 else None


def resolve_operator_build_jobs(forwarded: str | None, ambient: str | None) -> int | None:
    """Resolve operator intent from the two variables, without touching the environment.

    ``forwarded`` is ``DAGRUN_OPERATOR_BUILD_JOBS`` and ``ambient`` is ``CARGO_BUILD_JOBS``.
    PRESENCE of the first wins outright, empty included: an outer runner already asked this
    question of the real environment, and its answer — including "the operator asked for
    nothing" — is the only one that is still trustworthy, because that same runner has since
    written ``ambient`` itself.
    """
    if forwarded is not None:
        return parse_build_jobs(forwarded)
    return parse_build_jobs(ambient)


#: Captured ONCE, at import, so that this process answers the same way before and after it has
#: written a build width into any child's environment. Rust memoizes in a ``OnceLock`` for the
#: same reason.
#:
#: BE PRECISE ABOUT WHY, because the obvious reason is not currently true. Nothing in this package
#: mutates ``os.environ[CARGO_BUILD_JOBS]`` in-process: ``enter_delegated_scope`` would, and it
#: has no caller. The live in-scope path is the systemd re-exec, and there a FRESH interpreter
#: reads a ``CARGO_BUILD_JOBS`` its parent set via ``--setenv``, which the forwarded
#: ``DAGRUN_OPERATOR_BUILD_JOBS`` — present, possibly empty — is what disarms. So today the
#: import-time capture is equivalent to a lazy read; it is written this way so that wiring up
#: ``enter_delegated_scope``, or adding any other in-process write, cannot silently turn the
#: runner's own number into "operator intent" without anyone noticing.
_OPERATOR_BUILD_JOBS: int | None = resolve_operator_build_jobs(
    os.environ.get(OPERATOR_BUILD_JOBS_ENV), os.environ.get(BUILD_JOBS_ENV)
)


def operator_build_jobs() -> int | None:
    """The build width the operator chose, or ``None`` if they expressed none."""
    return _OPERATOR_BUILD_JOBS


@dataclass(frozen=True)
class BuildJobsChoice:
    """Which build width governs, what the alternative was, and why — so an OOM is explicable."""

    #: The width that will actually be exported.
    jobs: int
    #: What the containment derivation would have chosen from the caps in force.
    derived: int
    #: The operator's stated width, or ``None`` when they stated none.
    operator: int | None

    @property
    def source(self) -> str:
        """``"operator"`` when a stated width governs, ``"containment"`` when the derivation does."""
        return "operator" if self.operator is not None else "containment"

    def describe(self) -> str:
        """One sentence naming the winner, the loser, and the consequence."""
        if self.operator is not None:
            return (
                f"build width: honouring {BUILD_JOBS_ENV}={self.operator} from the environment; "
                f"the containment default would have chosen {self.derived}. Per-step downward "
                "refinement is OFF for this run, so a memory cap sized for a narrower pool can "
                "still OOM."
            )
        return (
            f"build width: no {BUILD_JOBS_ENV} in the environment, so the containment default "
            f"governs at {self.derived} for this scope, refined downward per step."
        )


def choose_build_jobs(
    operator: int | None, cpu_count: int | None, mem_max_bytes: int | None
) -> BuildJobsChoice:
    """Choose the build width for a scope or a step, and record what lost.

    A cgroup quota is a CEILING, not a parallelism instruction. When an operator has stated a
    width — having, presumably, sized their memory cap against exactly that pool — the derived
    number does not get to replace it silently. When they have stated nothing, the derivation
    governs and stays free to narrow per step, which is the behaviour
    ``test_derive_build_jobs_caps_the_284_leak`` and ``test_build_job_cap.py`` pin.
    """
    derived = derive_build_jobs(cpu_count, mem_max_bytes)
    return BuildJobsChoice(
        jobs=operator if operator is not None else derived,
        derived=derived,
        operator=operator,
    )


def select_build_jobs(cpu_count: int | None, mem_max_bytes: int | None) -> BuildJobsChoice:
    """:func:`choose_build_jobs` against this process's captured operator intent."""
    return choose_build_jobs(_OPERATOR_BUILD_JOBS, cpu_count, mem_max_bytes)


_SIZE_RE = re.compile(r"\s*(\d+(?:\.\d+)?)\s*([KkMmGgTt]?)([Bb]?)\s*")
_SIZE_MULT = {"": 1, "k": 1024, "m": 1024**2, "g": 1024**3, "t": 1024**4}


def parse_size(spec: str | int | None) -> int | None:
    """Parse '8G' / '4096M' / '2048K' / '12345' (bytes) into an int, or None if bad."""
    if spec is None or spec == "":
        return None
    match = _SIZE_RE.fullmatch(str(spec))
    if not match:
        return None
    value = float(match.group(1))
    product = value * _SIZE_MULT[match.group(2).lower()]
    if math.isnan(product) or product < 0.0:
        return None
    if product >= float(_I64_MAX):
        return _I64_MAX
    return int(product)


def mem_available_bytes() -> int | None:
    """Bytes currently allocatable without swapping (MemAvailable), or None if unreadable."""
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        return None
    return None


#: cgroup-v2 unified memory limit for the whole run's container/machine.
_MEMORY_MAX_PATH = Path("/sys/fs/cgroup/memory.max")


def cgroup_mem_max_bytes() -> int | None:
    """The cgroup-v2 ``memory.max`` cap (bytes) of the container the run is boxed in, or ``None``
    when it is unbounded (``max``) or unreadable. This is the HARD ceiling the kernel enforces on
    the whole boxed subtree, so it bounds how many concurrent copies can coexist before the OOM
    killer fires."""
    try:
        raw = _MEMORY_MAX_PATH.read_text().strip()
    except OSError:
        return None
    if raw == "max":
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def box_mem_budget_bytes() -> int | None:
    """The memory budget (bytes) a stress fan-out must fit inside, or ``None`` when it cannot be
    determined.

    Takes the MINIMUM of the readable signals — the cgroup ``memory.max`` hard cap (what the
    kernel will OOM-kill above) and ``/proc/meminfo`` ``MemAvailable`` (headroom actually free
    right now, which respects sibling agents on a shared box). The minimum is deliberately
    conservative: exceeding EITHER risks an OOM, so the smaller wins."""
    signals = [b for b in (cgroup_mem_max_bytes(), mem_available_bytes()) if b is not None]
    return min(signals) if signals else None


def stress_copy_footprint_bytes(
    cfg: DagConfig, *, default_step_bytes: int | None = None
) -> int:
    """Conservative worst-case memory footprint (bytes) of ONE copy (shard) of ``cfg``, used to
    derive the safe ``--stress`` fan-out.

    Sums each step's width-aware inner memory cap; an explicit ``hard_mem_max_bytes`` wins, a
    CPU-bound RSS/default cap scales with its effective preferred/default width, and an active step
    that declares no memory is charged ``default_step_bytes``. Summing every step (rather than the reachable
    concurrent set) is a deliberate UPPER BOUND: it never under-charges, so the derived ceiling
    errs toward refusing rather than OOMing sibling agents. Intentional pre-execution skips are
    excluded because no copy can spawn them. For the common single-node stress
    (``--selected dbi.file_metadata --ignore-selected-deps --stress N``) the sum is exactly that
    one node's cap."""
    effective_default = (
        default_step_bytes
        if default_step_bytes is not None and default_step_bytes > 0
        else (
            cfg.default_step_mem_cap_bytes
            if cfg.default_step_mem_cap_bytes is not None
            and cfg.default_step_mem_cap_bytes > 0
            else None
        )
    )
    control_floor = stress_control_floor_bytes(cfg, default_step_bytes=default_step_bytes)
    total = 0
    runnable = 0
    for step in cfg.steps:
        if step.skip_reason is not None:
            continue
        runnable += 1
        cap = _step_mem_cap_for_inner_jobs(
            step,
            effective_cpu_count(step, cfg.default_step_cpu_count),
            mem_cap_factor=cfg.mem_cap_factor,
            default_cap_bytes=effective_default,
        )
        total = _saturating_add_i64(total, cap if cap is not None else _I64_MAX)
    if runnable > 0:
        return max(total, control_floor)
    return control_floor


def stress_control_floor_bytes(
    cfg: DagConfig, *, default_step_bytes: int | None = None
) -> int:
    """Minimum control-plane memory charged to each complete stress graph copy.

    A positive explicit/configured step default is already the repository's conservative SMALL
    forcing-function allowance (normally 1 GiB), so even a characterized one-byte command cannot
    make graph/process bookkeeping look free. If the default is deliberately disabled, preserve a
    positive finite hard-cap model by falling back to ``mem_cap_floor_bytes`` or one byte. The CLI
    separately caps generated graph nodes, which is the deterministic object-allocation guard.
    """
    effective_default = (
        default_step_bytes
        if default_step_bytes is not None and default_step_bytes > 0
        else (
            cfg.default_step_mem_cap_bytes
            if cfg.default_step_mem_cap_bytes is not None
            and cfg.default_step_mem_cap_bytes > 0
            else None
        )
    )
    if effective_default is not None:
        return effective_default
    return max(1, max(0, _clamp_i64(cfg.mem_cap_floor_bytes)))
