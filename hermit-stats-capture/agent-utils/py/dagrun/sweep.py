"""Pure planning helpers for graph-wide parallel-scaling sweeps.

The command-line sweep runner owns process execution and persistence.  This module keeps the
deterministic pieces separate: dependency order, CPU-topology discovery, the coarse first-pass
grid, cumulative midpoint refinement, and target-duration parsing.  Keeping these functions pure
or dependency-injected makes the expensive runner easy to test without launching benchmark work.
"""

from __future__ import annotations

import heapq
import math
import os
import re
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

from dagrun.model import Step, effective_jobs_env, effective_jobs_flag

__all__ = [
    "CpuTopology",
    "SweepWidth",
    "WidthSource",
    "cpu_topology_from_core_ids",
    "detect_cpu_topology",
    "initial_widths",
    "labeled_width_grid_for_pass",
    "limit_topology",
    "parse_target_duration",
    "parse_widths",
    "refine_width_grid",
    "stable_topological_steps",
    "workload_digest",
    "width_grid_for_pass",
]

_FNV_OFFSET_BASIS = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_U64_MASK = (1 << 64) - 1


@dataclass(frozen=True)
class CpuTopology:
    """The logical CPUs available to this process and their physical-core count.

    ``allowed_logical_cpus`` is the process affinity mask, not the host-wide online CPU list: a
    sweep must not advertise widths it cannot place work on.  ``physical_core_count`` is ``None``
    when the sysfs topology was incomplete.  In that case the coarse grid still covers powers of
    two through the allowed logical width, but does not invent a physical-core boundary.
    """

    allowed_logical_cpus: tuple[int, ...]
    physical_core_count: int | None

    def __post_init__(self) -> None:
        if not self.allowed_logical_cpus:
            raise ValueError("CPU topology needs at least one allowed logical CPU")
        if self.allowed_logical_cpus != tuple(sorted(set(self.allowed_logical_cpus))):
            raise ValueError("allowed logical CPUs must be sorted and unique")
        if any(cpu < 0 for cpu in self.allowed_logical_cpus):
            raise ValueError("allowed logical CPU ids must be non-negative")
        if self.physical_core_count is not None and not (
            1 <= self.physical_core_count <= self.logical_thread_count
        ):
            raise ValueError("physical core count must be within the allowed logical CPU count")

    @property
    def logical_thread_count(self) -> int:
        """Number of hardware threads the process affinity mask permits."""

        return len(self.allowed_logical_cpus)


class WidthSource(Enum):
    """Why a width appears in a scaling grid."""

    EXPLICIT = "explicit"
    POWER_OF_TWO = "power-of-two"
    PHYSICAL_CORES = "physical-cores"
    LOGICAL_THREADS = "logical-threads"
    MIDPOINT = "midpoint"


_SOURCE_ORDER = {
    WidthSource.EXPLICIT: 0,
    WidthSource.POWER_OF_TWO: 1,
    WidthSource.PHYSICAL_CORES: 2,
    WidthSource.LOGICAL_THREADS: 3,
    WidthSource.MIDPOINT: 4,
}


@dataclass(frozen=True)
class SweepWidth:
    """One width in a cumulative pass grid, with stable provenance.

    A width may have several reasons: on an eight-core/sixteen-thread machine, width eight is both
    a power of two and the physical-core boundary.  ``introduced_in_pass`` remains one for every
    coarse point and records the refinement pass that first introduced a midpoint.
    """

    inner_jobs: int
    sources: tuple[WidthSource, ...]
    introduced_in_pass: int

    def __post_init__(self) -> None:
        if self.inner_jobs < 1:
            raise ValueError("sweep width must be positive")
        if not self.sources:
            raise ValueError("sweep width needs at least one source")
        canonical = tuple(sorted(set(self.sources), key=_SOURCE_ORDER.__getitem__))
        if self.sources != canonical:
            raise ValueError("sweep width sources must be unique and in canonical order")
        if self.introduced_in_pass < 1:
            raise ValueError("introduced pass must be positive")

    @property
    def source_label(self) -> str:
        """Stable, CSV-friendly label for all reasons this width was selected."""

        return "+".join(source.value for source in self.sources)


CoreIdentity = tuple[int, int]


def workload_digest(step: Step, default_jobs_flag: str, default_jobs_env: str) -> str:
    """Stable identity for the work whose scaling curve is being measured.

    The payload contains the ``tag``, command, command type, effective width flag, and effective
    width environment name as NUL-delimited fields, followed by sorted ``key=value`` step
    environment entries. FNV-1a keeps the identity deterministic and dependency-free.
    """

    fields = (
        step.tag,
        step.cmd,
        step.cmdtype.value,
        effective_jobs_flag(step, default_jobs_flag),
        effective_jobs_env(step, default_jobs_env),
    )
    payload = "".join(f"{field}\0" for field in fields)
    payload += "".join(f"{key}={value}\0" for key, value in sorted(step.env.items()))
    if step.delegated_children:
        payload += "delegated_children=true\0"
    digest = _FNV_OFFSET_BASIS
    for byte in payload.encode("utf-8"):
        digest ^= byte
        digest = (digest * _FNV_PRIME) & _U64_MASK
    return f"{digest:016x}"


def cpu_topology_from_core_ids(
    allowed_logical_cpus: Iterable[int],
    core_ids: Mapping[int, CoreIdentity],
) -> CpuTopology:
    """Build topology from an allowed CPU set and ``cpu -> (package, core)`` identities.

    Only CPUs in the supplied affinity set participate.  The physical count is deliberately
    unknown when even one allowed CPU lacks an identity: extrapolating from a partial map can
    mistake an SMT sibling for another core and put a falsely precise boundary in pass one.
    """

    allowed = tuple(sorted(set(allowed_logical_cpus)))
    if not allowed:
        raise ValueError("CPU topology needs at least one allowed logical CPU")
    if any(cpu < 0 for cpu in allowed):
        raise ValueError("allowed logical CPU ids must be non-negative")
    identities: list[CoreIdentity] = []
    for cpu in allowed:
        identity = core_ids.get(cpu)
        if identity is None:
            return CpuTopology(allowed_logical_cpus=allowed, physical_core_count=None)
        identities.append(identity)
    return CpuTopology(
        allowed_logical_cpus=allowed,
        physical_core_count=len(set(identities)),
    )


def _allowed_logical_cpus() -> tuple[int, ...]:
    """Best-effort process affinity, with a positive portable fallback."""

    try:
        allowed = tuple(sorted(os.sched_getaffinity(0)))
    except (AttributeError, OSError):
        allowed = tuple(range(max(1, os.cpu_count() or 1)))
    return allowed or (0,)


def _read_core_identity(sysfs_root: Path, cpu: int) -> CoreIdentity | None:
    topology = sysfs_root / f"cpu{cpu}" / "topology"
    try:
        package = int((topology / "physical_package_id").read_text(encoding="ascii").strip())
        core = int((topology / "core_id").read_text(encoding="ascii").strip())
    except (OSError, UnicodeError, ValueError):
        return None
    if package < 0 or core < 0:
        return None
    return package, core


def detect_cpu_topology(
    allowed_logical_cpus: Iterable[int] | None = None,
    *,
    sysfs_root: str | Path = "/sys/devices/system/cpu",
) -> CpuTopology:
    """Discover allowed hardware threads and physical cores from Linux sysfs.

    ``allowed_logical_cpus`` and ``sysfs_root`` are injectable for tests.  Omitting the former uses
    ``sched_getaffinity(0)`` so cpuset restrictions are respected.  Missing or malformed topology
    leaves the physical count unknown rather than guessing that a particular SMT factor applies.
    """

    allowed = (
        _allowed_logical_cpus()
        if allowed_logical_cpus is None
        else tuple(sorted(set(allowed_logical_cpus)))
    )
    root = Path(sysfs_root)
    core_ids = {
        cpu: identity
        for cpu in allowed
        if (identity := _read_core_identity(root, cpu)) is not None
    }
    return cpu_topology_from_core_ids(allowed, core_ids)


def limit_topology(topology: CpuTopology, logical_limit: int) -> CpuTopology:
    """Cap a discovered topology to an effective logical CPU budget.

    CPU affinity can expose more hardware threads than a binding ancestor ``cpu.max`` grants.
    Sweep planning therefore applies the effective :func:`container_core_budget` separately from
    discovery.  Only counts matter at this stage: the returned CPU-id tuple is a deterministic
    prefix with the requested cardinality, while the physical boundary is conservatively capped at
    the same count.  Unknown physical topology remains unknown rather than acquiring a guessed
    boundary from the quota.
    """

    if logical_limit < 1:
        raise ValueError("logical CPU limit must be positive")
    effective = min(topology.logical_thread_count, logical_limit)
    if effective == topology.logical_thread_count:
        return topology
    physical = (
        None
        if topology.physical_core_count is None
        else min(topology.physical_core_count, effective)
    )
    return CpuTopology(
        allowed_logical_cpus=topology.allowed_logical_cpus[:effective],
        physical_core_count=physical,
    )


def stable_topological_steps(steps: Sequence[Step]) -> tuple[Step, ...]:
    """Return a deterministic topological order, preserving registration order among ready nodes.

    The ordinary loader already validates a DAG, but keeping this helper total is valuable for
    library callers and unit tests.  Duplicate tags, unknown dependencies, and cycles are rejected
    before an execution loop could silently omit work.
    """

    by_tag: dict[str, Step] = {}
    index: dict[str, int] = {}
    for position, step in enumerate(steps):
        if step.tag in by_tag:
            raise ValueError(f"duplicate step tag {step.tag!r}")
        by_tag[step.tag] = step
        index[step.tag] = position

    indegree: dict[str, int] = {}
    successors: dict[str, list[str]] = {step.tag: [] for step in steps}
    for step in steps:
        dependencies = set(step.deps)
        unknown = sorted(dependencies - set(by_tag))
        if unknown:
            names = ", ".join(unknown)
            raise ValueError(f"step {step.tag!r} has unknown dependencies: {names}")
        indegree[step.tag] = len(dependencies)
        for dependency in dependencies:
            successors[dependency].append(step.tag)

    ready = [index[tag] for tag, count in indegree.items() if count == 0]
    heapq.heapify(ready)
    ordered: list[Step] = []
    while ready:
        position = heapq.heappop(ready)
        step = steps[position]
        ordered.append(step)
        for successor in sorted(successors[step.tag], key=index.__getitem__):
            indegree[successor] -= 1
            if indegree[successor] == 0:
                heapq.heappush(ready, index[successor])

    if len(ordered) != len(steps):
        cyclic = ", ".join(step.tag for step in steps if indegree[step.tag] > 0)
        raise ValueError(f"dependency cycle among: {cyclic}")
    return tuple(ordered)


def _initial_width_points(topology: CpuTopology) -> tuple[SweepWidth, ...]:
    logical = topology.logical_thread_count
    power_limit = topology.physical_core_count or logical
    sources: dict[int, set[WidthSource]] = {}

    power = 1
    while power <= power_limit:
        sources.setdefault(power, set()).add(WidthSource.POWER_OF_TWO)
        power *= 2
    if topology.physical_core_count is not None:
        sources.setdefault(topology.physical_core_count, set()).add(WidthSource.PHYSICAL_CORES)
    sources.setdefault(logical, set()).add(WidthSource.LOGICAL_THREADS)

    return tuple(
        SweepWidth(
            inner_jobs=width,
            sources=tuple(sorted(width_sources, key=_SOURCE_ORDER.__getitem__)),
            introduced_in_pass=1,
        )
        for width, width_sources in sorted(sources.items())
    )


def initial_widths(topology: CpuTopology) -> tuple[int, ...]:
    """Pass-one widths: powers of two through the core boundary, then cores and threads.

    For a 158-core / 316-thread affinity this is exactly
    ``(1, 2, 4, 8, 16, 32, 64, 128, 158, 316)``.  Duplicate landmarks are folded.
    """

    return tuple(point.inner_jobs for point in _initial_width_points(topology))


def refine_width_grid(widths: Sequence[int]) -> tuple[int, ...]:
    """Return ``widths`` plus one integer midpoint in every non-adjacent gap.

    The result is cumulative: pass two retains every pass-one width while adding, for example, 48
    between 32 and 64.  Repeated application eventually fills every integer width in the bounded
    range.  Input order and duplicates do not affect the deterministic result.
    """

    ordered = sorted(set(widths))
    if not ordered or ordered[0] < 1:
        raise ValueError("width grid must contain positive integers")
    refined = set(ordered)
    for lower, upper in zip(ordered, ordered[1:]):
        if upper - lower > 1:
            refined.add((lower + upper) // 2)
    return tuple(sorted(refined))


def width_grid_for_pass(initial: Sequence[int], pass_number: int) -> tuple[int, ...]:
    """Return the cumulative width grid for a one-based sweep pass number."""

    if pass_number < 1:
        raise ValueError("sweep pass number must be positive")
    grid = tuple(sorted(set(initial)))
    if not grid or grid[0] < 1:
        raise ValueError("initial width grid must contain positive integers")
    for _ in range(1, pass_number):
        grid = refine_width_grid(grid)
    return grid


def _parse_inclusive_range(raw: str, *, bare_means_one_to_n: bool) -> tuple[int, int]:
    if ".." in raw:
        pieces = raw.split("..")
        if len(pieces) != 2:
            raise ValueError(f"invalid --jobs range {raw!r}: expected LO..HI")
        try:
            lo, hi = (int(piece.strip()) for piece in pieces)
        except ValueError:
            raise ValueError(f"invalid --jobs range {raw!r}: not an integer") from None
    else:
        try:
            value = int(raw.strip())
        except ValueError:
            raise ValueError(f"invalid --jobs {raw!r}: not an integer") from None
        lo, hi = (1, value) if bare_means_one_to_n else (value, value)
    if lo < 1 or hi < lo:
        raise ValueError(f"invalid --jobs range {raw!r}: need 1 <= LO <= HI")
    return lo, hi


def parse_widths(raw: str) -> tuple[int, ...]:
    """Parse explicit widths, including sparse comma lists in target-time mode.

    A bare ``N`` expands to ``1..N`` and ``LO..HI`` expands to the inclusive dense range. A comma
    list names exact widths while also permitting inclusive range items; its result is sorted and
    deduplicated.
    """

    text = raw.strip()
    if not text:
        raise ValueError("invalid --jobs '': expected a positive width or range")
    if "," not in text:
        lo, hi = _parse_inclusive_range(text, bare_means_one_to_n=".." not in text)
        return tuple(range(lo, hi + 1))

    widths: set[int] = set()
    for raw_item in text.split(","):
        item = raw_item.strip()
        if not item:
            raise ValueError(f"invalid --jobs {raw!r}: empty comma-list item")
        if ".." in item:
            lo, hi = _parse_inclusive_range(item, bare_means_one_to_n=False)
            widths.update(range(lo, hi + 1))
        else:
            try:
                width = int(item)
            except ValueError:
                raise ValueError(
                    f"invalid --jobs {raw!r}: {item!r} is not an integer"
                ) from None
            if width < 1:
                raise ValueError(f"invalid --jobs {raw!r}: widths must be >= 1")
            widths.add(width)
    return tuple(sorted(widths))


def labeled_width_grid_for_pass(
    topology: CpuTopology, pass_number: int
) -> tuple[SweepWidth, ...]:
    """Return a cumulative pass grid with each width's origin and introduction pass."""

    if pass_number < 1:
        raise ValueError("sweep pass number must be positive")
    points = {point.inner_jobs: point for point in _initial_width_points(topology)}
    for current_pass in range(2, pass_number + 1):
        before = tuple(sorted(points))
        after = refine_width_grid(before)
        for width in after:
            if width not in points:
                points[width] = SweepWidth(
                    inner_jobs=width,
                    sources=(WidthSource.MIDPOINT,),
                    introduced_in_pass=current_pass,
                )
    return tuple(points[width] for width in sorted(points))


_TARGET_DURATION_RE = re.compile(
    r"^(?P<number>(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+))(?P<unit>ms|s|m|h)?$",
    re.IGNORECASE,
)
_TARGET_DURATION_SCALE = {"": 1.0, "ms": 0.001, "s": 1.0, "m": 60.0, "h": 3600.0}


def parse_target_duration(raw: str) -> float:
    """Parse a non-negative finite target allowance into seconds.

    A bare number means seconds; ``ms``, ``s``, ``m``, and ``h`` suffixes are accepted.  This is a
    soft sweep target, not a process timeout, so the parser deliberately returns a duration value
    without attaching any kill semantics to it. Zero means “complete mandatory pass one, then
    start no optional refinement pass.”
    """

    text = raw.strip()
    match = _TARGET_DURATION_RE.fullmatch(text)
    if match is None:
        raise ValueError(
            f"invalid --target-time {raw!r}: expected seconds or an ms/s/m/h suffix"
        )
    value = float(match.group("number"))
    unit_text = match.group("unit")
    unit = unit_text.lower() if unit_text is not None else ""
    seconds = value * _TARGET_DURATION_SCALE[unit]
    if not math.isfinite(seconds) or seconds < 0.0:
        raise ValueError(
            f"invalid --target-time {raw!r}: duration must be finite and >= 0"
        )
    return seconds
