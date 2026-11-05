"""Two-level Linux cgroup-v2 containment for complete runs and individual steps.

A run enters a delegated transient scope and creates one child cgroup per step.
Per-step limits and ``cgroup.kill`` then apply to the complete descendant tree,
including children that start new sessions. Unsupported hosts degrade explicitly:
callers can detect disabled containment, and failed enforcement writes emit warnings.
"""

from __future__ import annotations

import errno
import math
import os
import shlex
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Collection, Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
from itertools import count
from pathlib import Path
from types import FrameType
from typing import TYPE_CHECKING

from dagrun.capabilities import Lane, is_enforced
from dagrun.sizing import (
    BUILD_JOBS_ENV,
    OPERATOR_BUILD_JOBS_ENV,
    mem_available_bytes,
    select_build_jobs,
)

CGROUP_ROOT = Path("/sys/fs/cgroup")

#: Fraction of WHOLE-SYSTEM CPU the shared aggregate slice may use. 0.90 leaves
#: ~10% headroom for SSH, the coordinator, and the OS so the box stays
#: responsive even when concurrent runs saturate their budget.
DEFAULT_CPU_BUDGET_FRACTION = 0.90

#: Fraction of currently allocatable, non-swap memory granted to one outer run
#: scope. This is deliberately generous: per-step caps remain the precise
#: controls, while the outer cap is the last-resort boundary that keeps a
#: runaway run from reaching the host-global OOM killer.
DEFAULT_MEMORY_BUDGET_FRACTION = 0.90

# systemd may populate a new delegated scope asynchronously. Confirm the root is stably empty
# rather than trusting the first empty roster read immediately after moving its initial members.
_SCOPE_DRAIN_ATTEMPTS = 50
_SCOPE_DRAIN_RETRY_SECONDS = 0.01
_SCOPE_DRAIN_EMPTY_SAMPLES = 2
_CONTROLLER_ENABLE_ATTEMPTS = 3
_NESTED_ROOT_COUNTER = count()
_NESTED_COORDINATOR_STACK: list[tuple[Path, Path, ScopeNaming]] = []

#: Optional caller override used by containment tests and constrained hosts.
#: It may only TIGHTEN the derived cap, never widen it.
OUTER_MEMORY_MAX_ENV = "DAGRUN_OUTER_MEMORY_MAX_BYTES"
#: Exact cap carried across the systemd re-exec for in-scope readback.
EXPECTED_OUTER_MEMORY_MAX_ENV = "DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES"
#: Exact total-CPU cap carried across the systemd re-exec for in-scope readback.
EXPECTED_OUTER_CPU_COUNT_ENV = "DAGRUN_EXPECTED_OUTER_CPU_COUNT"
#: Exact cgroup root delegated by an outer dagrun step to a nested scheduler.  The nested
#: process must be observed in this root's immediate ``supervisor`` child before the value is
#: trusted; an inherited or forged path is not admission.
DELEGATED_CGROUP_ENV = "DAGRUN_DELEGATED_CGROUP"
#: Explicit, graph-reviewed fallback for a ``delegated_children`` step whose outer scheduler is
#: itself unboxed (for example a hosted CI runner using ``--allow-cgroup-failure``). This is an
#: authorization to nest, never evidence of containment; only the exact value ``1`` is accepted.
DELEGATED_UNBOXED_ENV = "DAGRUN_DELEGATED_UNBOXED"
#: Carries the outer scope's requested ``RuntimeMaxSec`` into the in-scope child, so the child can
#: read the property back off the live unit instead of trusting the argument vector that asked
#: for it.
EXPECTED_RUNTIME_MAX_ENV = "DAGRUN_EXPECTED_RUNTIME_MAX_SEC"

#: Per-step child cgroup directory prefix (also the scan key for the
#: normal-exit backstop). cgroup-v2 directory names may not contain '/'.
_STEP_PREFIX = "step-"
_MAX_CGROUP_COMPONENT_BYTES = 255


@dataclass(frozen=True)
class ScopeNaming:
    """Caller-supplied names for the outer scope, slice, and sentinels.

    These are the ONLY project-specific strings in this module. A caller keeps
    the defaults or overrides them to brand its own runs; nothing else here
    needs changing to reuse the two-level cgroup machinery.
    """

    #: Shared parent slice for ALL concurrent runs. Every run's transient scope
    #: launches under it, so a single ``CPUQuota`` on the slice bounds the SUM of
    #: CPU across however many runs execute at once (not each one individually).
    slice_name: str = "dagrun.slice"
    #: Prefix for the per-run transient scope unit (``<prefix>-<pid>``).
    unit_prefix: str = "dagrun"
    #: Environment sentinel set in the re-exec'd (in-scope) child.
    env_in_scope: str = "DAGRUN_IN_SCOPE"
    #: Environment var carrying the outer scope unit name to the in-scope child.
    env_scope_unit: str = "DAGRUN_SCOPE_UNIT"
    #: Environment var carrying the delegated (systemd-free) cgroup path.
    env_direct_cgroup: str = "DAGRUN_DIRECT_CGROUP"
    #: Prefix for every log/warning line this module prints.
    log_prefix: str = "[dagrun]"
    #: Child cgroup the runner vacates into (cgroup-v2 no-internal-processes).
    supervisor_name: str = "supervisor"


DEFAULT_NAMING = ScopeNaming()


class CgroupEnforcementKind(str, Enum):
    """A cgroup boundary that can actually constrain a run.

    Members compare equal to their string values for convenient logging and policy checks.
    """

    USER_SCOPE_QUOTA = "user-scope-quota"
    DELEGATED_CGROUPFS = "delegated-cgroupfs"
    CONTAINER_CPUSET = "container-cpuset"
    CONTAINER_QUOTA = "container-quota"


#: The CPU-boundary enum is the same set of kinds; kept as a named alias so
#: call sites reading a CPU-only enforcement result document their intent.
CpuEnforcementKind = CgroupEnforcementKind


def _warn(naming: ScopeNaming, msg: str) -> None:
    """Emit a visible degraded-enforcement warning (No Silent Failure).

    Best-effort cgroupfs writes that fail must NEVER drop a requested cap
    silently. Rather than crash (containment is additive, not a hard
    dependency), we print a clearly-labelled warning to stderr so the operator
    can see that enforcement degraded and why.
    """
    sys.stderr.write(f"{naming.log_prefix} WARNING: degraded enforcement: {msg}\n")
    sys.stderr.flush()


def _fmt_bytes(n: int) -> str:
    """Human-readable byte count for log lines (e.g. ``12.0 GiB``)."""
    value = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024.0 or unit == "TiB":
            return f"{value:.1f} {unit}"
        value /= 1024.0
    return f"{n} B"


def outer_memory_max_bytes(requested_bytes: int | None = None) -> int | None:
    """Outer-scope cap: the SMALLEST of availability, the environment, and an explicit request.

    The outer scope is a correctness boundary, not a per-step sizing model: it
    gets 90% of ``MemAvailable`` so the coordinator, neighbours, and kernel keep
    headroom. ``DAGRUN_OUTER_MEMORY_MAX_BYTES`` can tighten this for a smaller
    host or container, but cannot widen the derived boundary.

    ``requested_bytes`` is the caller's own ceiling — ``--max-mem`` — and obeys the same
    one-way rule: it can tighten the boundary, never widen it.  Without it ``--max-mem 20G``
    only *sized* the schedule and the scope still admitted 90% of the host, so two 20 GiB
    validates on one machine were 20 GiB each by arithmetic and by nothing else.  A budget that
    is a modelling input but not a containment limit is a share of a host nobody is holding.

    A non-positive request is REFUSED (``None``) exactly like a non-positive environment value:
    the caller asked for a ceiling this function cannot express, and returning the derived
    boundary instead would hand back a WIDER scope than was asked for.

    THAT IS A LIBRARY CONTRACT, NOT THE CLI'S BEHAVIOUR, and the difference is worth stating
    plainly because the first version of this docstring did not.  ``--max-mem 0`` never reaches
    here: :func:`cli._requested_max_mem_bytes` drops a non-positive spec, and the run is then
    refused by name by :func:`cli._select_max_steps` ("``--max-mem 0``: REFUSED — minimum
    runnable footprint … cannot fit safely within budget 0 bytes", exit 2, before any step
    starts).  One bad spec, one refusal: refusing here as well would give the same typo two
    different exit codes depending on whether boxing was attempted.  The ``requested <= 0`` arm
    exists for callers of this function that are not the CLI.
    """
    available = mem_available_bytes()
    if available is None:
        return None
    return _outer_memory_max_from(
        available, os.environ.get(OUTER_MEMORY_MAX_ENV), requested_bytes
    )


def _outer_memory_max_from(
    available: int, env_raw: str | None, requested: int | None
) -> int | None:
    """The pure core of :func:`outer_memory_max_bytes`: the smallest of the three ceilings.

    Split out because the other two inputs are ``/proc/meminfo`` and the process environment, and
    a rule about which ceiling wins is not worth much if it can only be exercised by arranging a
    host.
    """
    if available <= 0:
        return None
    ceilings = [max(1, int(available * DEFAULT_MEMORY_BUDGET_FRACTION))]
    if env_raw:
        try:
            from_env = int(env_raw)
        except ValueError:
            return None
        if from_env <= 0:
            return None
        ceilings.append(from_env)
    if requested is not None:
        if requested <= 0:
            return None
        ceilings.append(requested)
    return min(ceilings)


def expected_outer_memory_max_bytes() -> int | None:
    """Cap the parent requested, carried into the re-exec'd scope."""
    try:
        value = int(os.environ.get(EXPECTED_OUTER_MEMORY_MAX_ENV, ""))
    except ValueError:
        return None
    return value if value > 0 else None


def expected_outer_cpu_count() -> int | None:
    """Total-CPU cap the parent requested, carried into the re-exec'd scope."""
    try:
        value = int(os.environ.get(EXPECTED_OUTER_CPU_COUNT_ENV, ""))
    except ValueError:
        return None
    return value if value > 0 else None


# --------------------------------------------------------------------------- #
# Low-level cgroup-v2 filesystem helpers                                       #
# --------------------------------------------------------------------------- #


def _sanitize(tag: str) -> str:
    """An injective cgroup filename encoding for one UTF-8 step tag.

    Replacing every unsafe character with ``_`` made distinct tags such as ``a/b.j`` and
    ``a_b.j`` share one cgroup. A timeout or cleanup of either could therefore kill the other.
    ``~HH`` escapes are reversible; ``~`` itself is escaped, so no two byte strings collide.
    """
    safe = bytearray()
    for byte in tag.encode("utf-8"):
        if (
            ord("A") <= byte <= ord("Z")
            or ord("a") <= byte <= ord("z")
            or ord("0") <= byte <= ord("9")
            or byte in b"._-"
        ):
            safe.append(byte)
        else:
            safe.extend(f"~{byte:02x}".encode("ascii"))
    return f"{_STEP_PREFIX}{safe.decode('ascii')}"


def _my_cgroup_path() -> Path | None:
    """Filesystem path of THIS process's cgroup v2, via ``/proc/self/cgroup``
    (``0::<path>`` for the unified hierarchy). None if not cgroup v2."""
    try:
        for line in Path("/proc/self/cgroup").read_text().splitlines():
            if line.startswith("0::"):
                rel = line[3:].lstrip("/")
                return CGROUP_ROOT / rel
    except OSError:
        pass
    return None


def _read_cgroup_value(group: Path | None, name: str) -> str | None:
    if group is None:
        return None
    try:
        return (group / name).read_text().strip()
    except OSError:
        return None


def _cpu_max_cores(value: str | None) -> int | None:
    """Whole granted cores from a ``cpu.max`` string (``"<quota> <period>"``), or None
    when unbounded (``"max ..."``) / unreadable. Floors to >=1 for any positive quota so a
    sub-core slice still affords one build job."""
    if not value:
        return None
    parts = value.split()
    if len(parts) != 2 or parts[0] == "max":
        return None
    try:
        quota, period = int(parts[0]), int(parts[1])
    except ValueError:
        return None
    if quota <= 0 or period <= 0:
        return None
    return max(1, quota // period)


def _memory_max_bytes(value: str | None) -> int | None:
    """Byte cap from a ``memory.max`` string, or None when unbounded/unreadable."""
    if not value or value == "max":
        return None
    try:
        return int(value)
    except ValueError:
        return None


def _memory_limit_matches(expected: int, actual: int | None) -> bool:
    """Whether cgroup v2 retained ``expected``, allowing only safe page rounding down."""
    if actual is None:
        return False
    try:
        page_size = os.sysconf("SC_PAGE_SIZE")
    except (OSError, ValueError):
        return False
    return actual <= expected and expected - actual < page_size


def _delegated_ancestor_memory_max(
    root: Path,
    *,
    cgroup_root: Path = CGROUP_ROOT,
) -> tuple[int | None, bool]:
    """Return the tightest visible ancestor cap and whether the scan was complete.

    ``root`` is the exact parent step cgroup already validated by
    :func:`delegated_cgroup_root`.  A nested scheduler adds a private run root below it, but a
    newly-created cgroup starts with ``memory.max=max``; reading only that new root would lose a
    finite cap on the parent step or any ancestor up to the cgroup namespace root.

    The boolean distinguishes a hierarchy known to be wholly unbounded (``None, True``) from an
    unreadable hierarchy (``None, False``).  That distinction is required by censor-aware
    profiling: an unknown ancestor may not be treated as proof that a peak was uncensored.
    """
    try:
        boundary = cgroup_root.resolve(strict=True)
        group = root.resolve(strict=True)
    except OSError:
        return None, False
    if group != boundary and not group.is_relative_to(boundary):
        return None, False

    tightest: int | None = None
    complete = True
    while True:
        raw = _read_cgroup_value(group, "memory.max")
        if raw == "max" or (group == boundary and raw is None):
            # The cgroup-v2 hierarchy root has no parent to constrain and some kernels omit its
            # controller limit files entirely. That specific absence means unbounded, not unknown.
            pass
        else:
            parsed = _memory_max_bytes(raw)
            if parsed is None or parsed < 0:
                complete = False
            else:
                tightest = parsed if tightest is None else min(tightest, parsed)
        if group == boundary:
            break
        group = group.parent
    return tightest, complete


def _delegated_ancestor_cpu_count(
    root: Path,
    *,
    cgroup_root: Path = CGROUP_ROOT,
) -> tuple[int | None, bool]:
    """Return the tightest visible integer CPU quota and whether the scan was complete."""
    try:
        boundary = cgroup_root.resolve(strict=True)
        group = root.resolve(strict=True)
    except OSError:
        return None, False
    if group != boundary and not group.is_relative_to(boundary):
        return None, False

    tightest: int | None = None
    complete = True
    while True:
        raw = _read_cgroup_value(group, "cpu.max")
        fields = raw.split() if raw is not None else []
        if group == boundary and raw is None:
            pass
        elif len(fields) != 2:
            complete = False
        elif fields[0] == "max":
            try:
                if int(fields[1]) <= 0:
                    complete = False
            except ValueError:
                complete = False
        else:
            parsed = _cpu_max_cores(raw)
            if parsed is None:
                complete = False
            else:
                tightest = parsed if tightest is None else min(tightest, parsed)
        if group == boundary:
            break
        group = group.parent
    return tightest, complete


@dataclass(frozen=True)
class DelegatedResourceLimits:
    """Effective parent envelope inherited by one nested scheduler."""

    memory_max_bytes: int | None
    memory_known: bool
    max_cpus: int | None
    cpu_known: bool


def delegated_resource_limits(
    root: Path,
    *,
    cgroup_root: Path = CGROUP_ROOT,
) -> DelegatedResourceLimits:
    """Read the effective resource envelope above a validated delegated step root."""
    memory_max, memory_known = _delegated_ancestor_memory_max(
        root, cgroup_root=cgroup_root
    )
    max_cpus, cpu_known = _delegated_ancestor_cpu_count(root, cgroup_root=cgroup_root)
    # cpu.max bounds bandwidth; cpuset.cpus.effective independently bounds which CPUs are usable.
    # The latter already reflects every ancestor, so reading it at the delegated root is enough.
    cpuset_raw = _read_cgroup_value(root, "cpuset.cpus.effective")
    if cpuset_raw is not None:
        cpuset_cpus = _cpuset_count(cpuset_raw)
        if cpuset_cpus is None or cpuset_cpus < 1:
            cpu_known = False
        else:
            max_cpus = cpuset_cpus if max_cpus is None else min(max_cpus, cpuset_cpus)
    return DelegatedResourceLimits(memory_max, memory_known, max_cpus, cpu_known)


def _create_nested_run_root(
    parent: Path,
    run_root: Path,
    *,
    naming: ScopeNaming = DEFAULT_NAMING,
    cgroup_root: Path = CGROUP_ROOT,
    requested_max_cpus: int | None = None,
    requested_max_mem_bytes: int | None = None,
) -> DelegatedResourceLimits:
    """Create ``run_root`` and stamp the effective delegated ancestor resource envelope.

    The returned snapshot is retained by :class:`Cgroups` as profiling provenance.  Failure to
    mirror an inherited cap is survivable because the physical ancestor still enforces it.
    Failure to apply a *tighter requested* invocation cap is not: that would silently turn a
    nested ``--max-cpus``/``--max-mem`` promise into modeling-only advice, so this helper raises.
    """
    inherited = delegated_resource_limits(parent, cgroup_root=cgroup_root)
    memory_candidates = [
        value
        for value in (inherited.memory_max_bytes, requested_max_mem_bytes)
        if value is not None
    ]
    cpu_candidates = [
        value
        for value in (inherited.max_cpus, requested_max_cpus)
        if value is not None
    ]
    effective_memory = min(memory_candidates) if memory_candidates else None
    effective_cpus = min(cpu_candidates) if cpu_candidates else None
    effective = DelegatedResourceLimits(
        effective_memory,
        inherited.memory_known,
        effective_cpus,
        inherited.cpu_known,
    )
    memory_stamp_required = requested_max_mem_bytes is not None and (
        inherited.memory_max_bytes is None
        or requested_max_mem_bytes < inherited.memory_max_bytes
    )
    cpu_stamp_required = requested_max_cpus is not None and (
        inherited.max_cpus is None or requested_max_cpus < inherited.max_cpus
    )
    run_root.mkdir()
    if effective.memory_max_bytes is not None:
        try:
            memory_max = run_root / "memory.max"
            memory_max.write_text(str(effective.memory_max_bytes))
            applied = _memory_max_bytes(memory_max.read_text().strip())
            if not _memory_limit_matches(effective.memory_max_bytes, applied):
                message = (
                    f"nested scheduler cgroup {run_root} did not retain "
                    f"memory.max={effective.memory_max_bytes} (read back {applied!r})"
                )
                if memory_stamp_required:
                    raise OSError(message)
                _warn(naming, f"{message}; ancestor cap still applies")
        except OSError as error:
            if memory_stamp_required:
                raise OSError(
                    f"could not enforce requested nested memory.max="
                    f"{effective.memory_max_bytes} on {run_root}: {error}"
                ) from error
            _warn(
                naming,
                f"could not stamp inherited memory.max={effective.memory_max_bytes} on nested "
                f"scheduler cgroup {run_root} ({error}); ancestor cap still applies",
            )
    if effective.max_cpus is not None:
        try:
            cpu_max = run_root / "cpu.max"
            expected = f"{effective.max_cpus * 100_000} 100000"
            cpu_max.write_text(expected)
            applied_cpus = _cpu_max_cores(cpu_max.read_text().strip())
            if applied_cpus != effective.max_cpus:
                message = (
                    f"nested scheduler cgroup {run_root} did not retain CPU ceiling "
                    f"{effective.max_cpus} (read back {applied_cpus!r})"
                )
                if cpu_stamp_required:
                    raise OSError(message)
                _warn(naming, f"{message}; ancestor quota still applies")
        except OSError as error:
            if cpu_stamp_required:
                raise OSError(
                    f"could not enforce requested nested CPU ceiling "
                    f"{effective.max_cpus} on {run_root}: {error}"
                ) from error
            _warn(
                naming,
                f"could not stamp inherited CPU ceiling {effective.max_cpus} on nested "
                f"scheduler cgroup {run_root} ({error}); ancestor quota still applies",
            )
    return effective


def _bounded(value: str | None) -> bool:
    """True for a numeric cgroup-v2 limit, never for ``max`` or unreadable."""
    try:
        return value is not None and value != "max" and int(value) >= 0
    except ValueError:
        return False


def _quota_is_bounded(value: str | None) -> bool:
    try:
        quota, period = (int(part) for part in (value or "").split())
        return quota > 0 and period > 0
    except (TypeError, ValueError):
        return False


def _cpuset_count(value: str | None) -> int | None:
    """Count Linux cpulist entries such as ``0-3,8,10-11``."""
    if not value:
        return None
    count = 0
    try:
        for item in value.split(","):
            bounds = item.split("-", maxsplit=1)
            start = int(bounds[0])
            end = int(bounds[-1])
            if end < start:
                return None
            count += end - start + 1
    except ValueError:
        return None
    return count


def _parse_cpuset(value: str | None) -> set[int] | None:
    """Parse a Linux cpulist, rejecting malformed or overlapping ranges."""
    if not value:
        return None
    cpus: set[int] = set()
    try:
        for item in value.split(","):
            bounds = item.split("-", maxsplit=1)
            start = int(bounds[0])
            end = int(bounds[-1])
            if start < 0 or end < start:
                return None
            current = set(range(start, end + 1))
            if cpus.intersection(current):
                return None
            cpus.update(current)
    except ValueError:
        return None
    return cpus


def _is_below_root(group: Path) -> bool:
    return group == CGROUP_ROOT or CGROUP_ROOT in group.parents


def _controller_set(group: Path | None, name: str) -> set[str]:
    """Controller names from a cgroup interface file. Leading ``+``/``-`` are
    stripped so a value seeded via a raw ``+cpu +memory`` write parses
    identically to the kernel's plain rendering."""
    raw = _read_cgroup_value(group, name) or ""
    return {token.lstrip("+-") for token in raw.split() if token.lstrip("+-")}


def _delegated_cgroupfs_available(group: Path | None) -> bool:
    """Probe whether THIS cgroup may create constrained child cgroups.

    Merely reading cgroup accounting is not enough. A delegated hierarchy must
    expose cpu and memory to children, have them enabled in ``subtree_control``,
    and permit a throwaway child directory to be created and removed.
    """
    if group is None:
        return False
    controllers = _controller_set(group, "cgroup.controllers")
    enabled = _controller_set(group, "cgroup.subtree_control")
    if not {"cpu", "memory"}.issubset(controllers | enabled):
        return False
    if not {"cpu", "memory"}.issubset(enabled):
        return False
    probe = group / f"dagrun-probe-{os.getpid()}"
    try:
        probe.mkdir()
        probe.rmdir()
        return True
    except OSError:
        try:
            probe.rmdir()
        except OSError:
            pass
        return False


def _delegated_cgroup_base(current: Path | None) -> Path | None:
    """The TOPMOST ancestor (current leaf up to the namespace root) that can
    host constrained child cgroups, or None.

    Why walk up: in a systemd-less container every process lives in a POPULATED
    leaf (e.g. ``/init`` after the standard no-internal-processes dance), and a
    populated cgroup can never enable subtree controllers — so probing only the
    current cgroup wrongly reports "unavailable" even when the namespace root
    has cpu+memory+pids delegated and accepts child cgroups. Job cgroups must
    then be created as SIBLINGS under this base (next to the process leaf),
    never as children of the populated leaf itself."""
    if current is None:
        return None
    if not _is_below_root(current):
        return current if _delegated_cgroupfs_available(current) else None
    best: Path | None = None
    group = current
    while True:
        if _delegated_cgroupfs_available(group):
            best = group  # keep walking: prefer the topmost passing ancestor
        if group == CGROUP_ROOT:
            return best
        group = group.parent


# --------------------------------------------------------------------------- #
# systemd user-slice aggregate cap                                            #
# --------------------------------------------------------------------------- #


def cpu_quota_percent(fraction: float = DEFAULT_CPU_BUDGET_FRACTION) -> int:
    """systemd ``CPUQuota`` percentage for ``fraction`` of ALL cores, as an
    integer percent. With 16 cores and fraction 0.90 this is 1440 (``"1440%"``),
    i.e. up to 14.4 cores of aggregate CPU shared across everything in the
    slice. Minimum 100% (one core) so a 1-core box still makes progress."""
    ncpu = os.cpu_count() or 1
    return max(100, int(round(ncpu * fraction * 100)))


def aggregate_slice_max_cpus() -> int:
    """Whole-core capacity represented by the shared aggregate slice's quota."""
    # Never claim a whole core the fractional shared-slice quota cannot sustain.
    return max(1, cpu_quota_percent() // 100)


def aggregate_slice_cpu_jobs() -> int:
    """Compatibility alias for :func:`aggregate_slice_max_cpus`."""

    return aggregate_slice_max_cpus()


def ensure_aggregate_slice(
    fraction: float = DEFAULT_CPU_BUDGET_FRACTION,
    naming: ScopeNaming = DEFAULT_NAMING,
) -> bool:
    """Create/refresh the shared aggregate slice with a ``CPUQuota`` capping the
    AGGREGATE CPU of every scope launched under it. Idempotent:
    ``systemctl --user set-property`` updates a live slice in place and starting
    an already-active slice is a no-op. Best-effort — returns False (caller just
    omits ``--slice`` and runs unconstrained) if systemd/user-cgroup is
    unavailable.

    cgroup-v2 detail: a slice's ``cpu.max`` is only enforced once the ``cpu``
    controller is delegated to it by its parent. ``set-property CPUQuota=...``
    makes systemd enable ``+cpu`` in the parent's ``subtree_control``, so the cap
    actually bites; without going through systemd we'd have to do that by hand."""
    quota = cpu_quota_percent(fraction)
    try:
        r1 = subprocess.run(
            ["systemctl", "--user", "start", naming.slice_name],
            capture_output=True, timeout=10,
        )
        r2 = subprocess.run(
            ["systemctl", "--user", "--runtime", "set-property",
             naming.slice_name, f"CPUQuota={quota}%"],
            capture_output=True, timeout=10,
        )
        return r1.returncode == 0 and r2.returncode == 0
    except (subprocess.TimeoutExpired, OSError):
        return False


_SCOPE_PROBE: bool | None = None

#: Pause before the one retry of a refused scope probe.
SCOPE_PROBE_RETRY_BACKOFF_S = 1.0
_SCOPE_PROBE_TIMEOUT_S = 8


def systemd_scope_available(naming: ScopeNaming = DEFAULT_NAMING) -> bool:
    """True iff ``systemd-run --user --scope`` actually works here (cached)."""
    global _SCOPE_PROBE
    if _SCOPE_PROBE is None:
        if not shutil.which("systemd-run"):
            sys.stderr.write(
                f"{naming.log_prefix} `systemd-run --user --scope` probe not attempted: "
                "systemd-run is not on PATH\n"
            )
            _SCOPE_PROBE = False
        else:
            _SCOPE_PROBE = probe_with_one_retry(
                _run_scope_probe, SCOPE_PROBE_RETRY_BACKOFF_S, naming
            )
    return _SCOPE_PROBE


def _run_scope_probe(attempt: int) -> str | None:
    """One ``systemd-run --user --scope true``. None when it worked; otherwise why it failed,
    with the probe's stderr, which is the only record of why systemd refused."""
    # A unit name per attempt, so a retry never collides with what its first attempt left, and
    # --collect, so a probe unit that fails is unloaded at once instead of staying listed as failed.
    try:
        r = subprocess.run(
            ["systemd-run", "--user", "--scope", "--collect", "--quiet",
             f"--unit=dagrun-probe-{os.getpid()}-{attempt}", "true"],
            capture_output=True, timeout=_SCOPE_PROBE_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired as expired:
        # Whatever systemd-run printed before it hung is still the best record of why.
        return f"timed out after {_SCOPE_PROBE_TIMEOUT_S} s; stderr: {_stderr_text(expired.stderr)!r}"
    except OSError as error:
        return f"cannot launch systemd-run: {error}"
    if r.returncode == 0:
        return None
    status = f"signal {-r.returncode}" if r.returncode < 0 else f"exit status {r.returncode}"
    return f"{status}; stderr: {_stderr_text(r.stderr)!r}"


def _stderr_text(raw: bytes | str | None) -> str:
    if raw is None:
        return ""
    text = raw.decode(errors="replace") if isinstance(raw, bytes) else raw
    return text.strip()


def probe_with_one_retry(
    probe: Callable[[int], str | None], backoff_s: float, naming: ScopeNaming = DEFAULT_NAMING
) -> bool:
    """Run ``probe``, and once more after ``backoff_s`` if it fails. Seen twice in about six weeks
    on a shared host: the user manager refused one probe within a second while the same
    environment set up scopes before and after it, and with nothing retried a whole validation
    failed closed. A second refusal still fails closed. Every failure goes to stderr with its
    cause."""
    for attempt in (1, 2):
        why = probe(attempt)
        if why is None:
            return True
        sys.stderr.write(
            f"{naming.log_prefix} `systemd-run --user --scope` probe attempt {attempt} of 2 "
            f"failed: {why}\n"
        )
        if attempt == 1:
            time.sleep(backoff_s)
    return False


# --------------------------------------------------------------------------- #
# systemd-free delegated-cgroupfs fallback                                    #
# --------------------------------------------------------------------------- #


def self_heal_namespace_root() -> tuple[bool, str]:
    """One-shot self-heal of a private-namespace cgroup root (the
    "no-internal-processes dance"), for containers whose entrypoint did not
    perform it. Idempotent and additive: it only MOVES processes out of the
    namespace ROOT into ``<root>/init`` and ENABLES controllers; it never moves
    processes out of any other cgroup and never disables anything.

    Preconditions (all checked; returns ``(False, why)`` when any fails):
      * the root delegates cpu+memory in ``cgroup.controllers``,
      * cpu or memory is still MISSING from root ``cgroup.subtree_control``,
      * root ``cgroup.subtree_control`` is writable,
      * this process's own cgroup path is SHALLOW (namespace-root evidence —
        refuses to touch what looks like a shared host hierarchy).

    Returns ``(True, detail)`` only when cpu+memory are verifiably enabled after
    the dance. Callers must log the detail loudly either way (no silent skip)."""
    root = CGROUP_ROOT
    controllers = _controller_set(root, "cgroup.controllers")
    if not {"cpu", "memory"}.issubset(controllers):
        return False, (f"namespace root {root} does not delegate cpu+memory "
                       f"(controllers: {sorted(controllers) or 'unreadable'})")
    enabled = _controller_set(root, "cgroup.subtree_control")
    if {"cpu", "memory"}.issubset(enabled):
        return False, f"namespace root {root} already enables cpu+memory for children"
    if not os.access(root / "cgroup.subtree_control", os.W_OK):
        return False, f"{root}/cgroup.subtree_control is not writable"
    mine = _my_cgroup_path()
    if mine is None:
        return False, "own cgroup path unreadable; refusing to touch the root"
    depth = (0 if mine == root
             else len(mine.relative_to(root).parts) if _is_below_root(mine) else 99)
    if depth > 1:
        return False, (f"own cgroup {mine} is nested {depth} deep — looks like a shared "
                       "host hierarchy, not a private namespace root; refusing the dance")
    # cgroup-v2 "no internal processes": the root cannot enable controllers for
    # children while it directly holds processes. Drain them into `init/`.
    init = root / "init"
    moved = 0
    if (_read_cgroup_value(root, "cgroup.procs") or "").split():
        try:
            init.mkdir(exist_ok=True)
        except OSError as exc:
            return False, f"could not create {init}: {exc}"
        for _ in range(5):  # a moved leader can reveal late-forked children
            pids = (_read_cgroup_value(root, "cgroup.procs") or "").split()
            if not pids:
                break
            for pid in pids:
                try:
                    (init / "cgroup.procs").write_text(pid)
                    moved += 1
                except OSError:
                    pass  # pid may have exited mid-move; racy, legitimately ignored
    # Enable controllers: atomic combined write first, then per-controller
    # fallback (the combined form fails wholesale if any one is unavailable).
    want = [c for c in ("cpu", "memory", "pids") if c in controllers]
    try:
        (root / "cgroup.subtree_control").write_text(" ".join(f"+{c}" for c in want))
    except OSError:
        for c in want:
            try:
                (root / "cgroup.subtree_control").write_text(f"+{c}")
            except OSError:
                pass
    enabled = _controller_set(root, "cgroup.subtree_control")
    if {"cpu", "memory"}.issubset(enabled):
        return True, (f"enabled {'+'.join(want)} on namespace root {root} "
                      f"(moved {moved} root process(es) into {init})")
    return False, (f"dance attempted (moved {moved} process(es)) but subtree_control "
                   f"still lacks cpu+memory: {sorted(enabled) or 'empty'}")


def probe_current_enforcement() -> tuple[CgroupEnforcementKind | None, str]:
    """Return verified container/delegated enforcement, never accounting alone."""
    current = _my_cgroup_path()
    if current is None:
        return None, "current cgroup is unreadable"

    base = _delegated_cgroup_base(current)
    if base is not None:
        where = "current cgroup" if base == current else f"ancestor {base}"
        return CgroupEnforcementKind.DELEGATED_CGROUPFS, (
            f"writable delegated cpu+memory cgroupfs at {where}")

    group = current
    while _is_below_root(group):
        cpu_max = _read_cgroup_value(group, "cpu.max")
        memory_max = _read_cgroup_value(group, "memory.max")
        if _quota_is_bounded(cpu_max) or _bounded(memory_max):
            return CgroupEnforcementKind.CONTAINER_QUOTA, (
                f"bounded container limit at {group}: cpu.max={cpu_max or 'UNREADABLE'}, "
                f"memory.max={memory_max or 'UNREADABLE'}"
            )
        cpuset = _read_cgroup_value(group, "cpuset.cpus.effective")
        if _cpuset_count(cpuset):
            return CgroupEnforcementKind.CONTAINER_CPUSET, (
                f"container cpuset at {group}: {cpuset}"
            )
        if group == CGROUP_ROOT:
            break
        group = group.parent
    return None, "no writable delegated cgroupfs, bounded quota, or effective cpuset"


def enter_delegated_scope(
    memory_max: int | None,
    cpu_count: int | None = None,
    naming: ScopeNaming = DEFAULT_NAMING,
) -> tuple[bool, str]:
    """Move this process into a constrained delegated child cgroup.

    NOT CALLED. This function has no caller anywhere in the repository — not in ``cli.py``, not in
    the tests, and it is not exported from ``dagrun/__init__.py``. The systemd re-exec
    (``reexec_in_scope``) is the only containment entry point actually taken, so the build-width
    announcement below cannot print today.
    ``test_operator_build_width.py::test_the_systemd_free_fallback_still_has_no_caller`` holds this
    sentence honest: wire the function up and that test fails, which is the prompt to delete this
    paragraph rather than let it rot into a lie. It is kept, rather than deleted, because it is the
    systemd-free fallback the cgroup probe's CONTAINER_* outcomes were written for.

    The cgroup probe has already established
    that a delegated ancestor exposes cpu and memory; failures here are fatal to
    callers because continuing would make the advertised limits advisory only —
    which is why the whole body reports its ``OSError`` loudly via the returned
    ``(False, detail)`` rather than swallowing it.

    The job cgroup is created under the topmost delegated ancestor — as a SIBLING
    of the process's own (possibly populated) leaf, not a child of it: a
    populated leaf can never enable subtree controllers, so nesting under it
    would make every limit advisory."""
    base = _delegated_cgroup_base(_my_cgroup_path())
    if base is None:
        return False, "delegated cgroupfs became unavailable"
    if memory_max is None:
        memory_max = outer_memory_max_bytes()
    if memory_max is None:
        return False, "could not derive a positive outer MemoryMax"
    child = base / f"{naming.unit_prefix}-{os.getpid()}"
    try:
        child.mkdir()
        # Keep the complete hierarchy swapless. Some hosts' userspace OOM policy
        # may choose swap-heavy cgroups even when the kernel did not record a
        # cgroup OOM event.
        (child / "memory.swap.max").write_text("0")
        (child / "memory.max").write_text(str(memory_max))
        # Keep hard caps only. Soft reclaim throttling would mark a legitimately
        # long run as memory-pressure-heavy on some hosts.
        (child / "memory.high").write_text("max")
        # Die as a unit on OOM so the breach lands on THIS job whole, not one
        # victim process (see prepare_command). Best-effort: a kernel without
        # oom.group must not abort an otherwise-valid boxed scope.
        try:
            (child / "memory.oom.group").write_text("1")
        except OSError as exc:
            _warn(naming, f"job cgroup: could not set memory.oom.group ({exc}); an OOM in this "
                  "scope may kill a single process instead of the whole job "
                  "(mis-attributed blast radius)")
        quota = cpu_count * 100 if cpu_count is not None else cpu_quota_percent()
        (child / "cpu.max").write_text(f"{quota * 1000} 100000")
        # Scope-wide build width. When the operator stated none, this is derived from THIS
        # granted quota + memory cap, so a command run directly in the scope (not via a per-step
        # child) still can't compute NUM_JOBS=<all-cores>, and per-step prepare_command refines it
        # downward. When the operator DID state one, theirs wins and is said out loud.
        choice = select_build_jobs(quota // 100, memory_max)
        print(f"{naming.log_prefix} {choice.describe()}", file=sys.stderr)
        os.environ[BUILD_JOBS_ENV] = str(choice.jobs)
        # Forward the resolved INTENT under its own name, so this write cannot later be read
        # back as an instruction by anything that inherits this environment.
        os.environ[OPERATOR_BUILD_JOBS_ENV] = (
            "" if choice.operator is None else str(choice.operator)
        )
        (child / "cgroup.procs").write_text(str(os.getpid()))
        os.environ[naming.env_in_scope] = "1"
        os.environ[naming.env_direct_cgroup] = str(child)
        os.environ[EXPECTED_OUTER_MEMORY_MAX_ENV] = str(memory_max)
        return True, f"entered delegated cgroup {child}"
    except OSError as exc:
        try:
            child.rmdir()
        except OSError:
            pass
        return False, str(exc)


# --------------------------------------------------------------------------- #
# Outer systemd-run --user --scope (Delegate=yes) re-exec                     #
# --------------------------------------------------------------------------- #



@dataclass(frozen=True)
class ContainmentProof:
    """Evidence that THIS LIVE PROCESS is inside a specific cgroup, observed rather than declared."""

    cgroup: Path
    pid: int
    unit: str | None = None


@dataclass(frozen=True)
class ContainmentEvidence:
    """Whether the running process could be OBSERVED inside the cgroup it is claimed to be in."""

    proof: ContainmentProof | None
    detail: str = ""

    def describe(self) -> str:
        """One clause for a diagnostic."""
        if self.proof is None:
            return self.detail
        unit = f" (promised unit {self.proof.unit})" if self.proof.unit else ""
        return f"pid {self.proof.pid} observed in {self.proof.cgroup}{unit}"


def observe_own_containment(
    expected_unit: str | None, *, naming: ScopeNaming = DEFAULT_NAMING
) -> ContainmentEvidence:
    """OBSERVE the running process inside its cgroup. A declaration of containment is not containment.

    The in-scope check is an ENVIRONMENT VARIABLE this process set for itself before re-exec-ing,
    and every "cgroup boxing ACTIVE" line downstream rested on it. Anything can export it, a scope
    can be stopped under a live process, and systemd can place a unit elsewhere than asked.

    TWO DIRECTIONS, because each catches what the other cannot: ``/proc/self/cgroup`` is the
    KERNEL's view of where this task is (and being in the ROOT is the tell a one-sided check waves
    through, since every process is in *some* cgroup), while ``<cgroup>/cgroup.procs`` is the
    CGROUP'S OWN ROSTER, which disagrees if the directory is gone or the task was migrated.
    """
    _ = naming
    pid = os.getpid()
    cgroup = _my_cgroup_path()
    if cgroup is None:
        return ContainmentEvidence(
            None, "/proc/self/cgroup carries no cgroup-v2 (0::) entry for this process"
        )
    if cgroup == CGROUP_ROOT:
        return ContainmentEvidence(
            None, f"pid {pid} is in the cgroup ROOT ({CGROUP_ROOT}); nothing contains it"
        )
    if not cgroup.is_dir():
        return ContainmentEvidence(
            None, f"pid {pid} claims cgroup {cgroup} but that directory does not exist"
        )
    procs = cgroup / "cgroup.procs"
    try:
        roster = procs.read_text().split()
    except OSError:
        return ContainmentEvidence(None, f"cannot read {procs} to confirm membership")
    if str(pid) not in roster:
        return ContainmentEvidence(
            None, f"pid {pid} is NOT listed in {procs}; the kernel and the cgroup disagree"
        )
    if expected_unit is not None:
        names = [cgroup.name, *(p.name for p in cgroup.parents)]
        if expected_unit not in names:
            return ContainmentEvidence(
                None,
                f"pid {pid} is contained in {cgroup}, but the promised unit was "
                f"{expected_unit}; the caps and the kill path were arranged on a different cgroup",
            )
    return ContainmentEvidence(ContainmentProof(cgroup, pid, expected_unit))


def _validate_delegated_cgroup_root_at(
    root: Path,
    current: Path,
    pid: int,
    *,
    cgroup_root: Path = CGROUP_ROOT,
    supervisor_name: str = DEFAULT_NAMING.supervisor_name,
) -> str | None:
    """Return an actionable refusal, or ``None`` when ``root`` is this process's delegation.

    The exact immediate-parent shape is deliberate. Merely checking that a supplied path is an
    ancestor would let a nested process claim the enclosing run scope (and therefore siblings) as
    its authority. The outer scheduler places the command in ``<root>/supervisor``; both the
    kernel-side current path and that leaf's roster must agree before the inner scheduler may
    create descendants under ``root``.
    """
    try:
        resolved_base = cgroup_root.resolve(strict=True)
        resolved_root = root.resolve(strict=True)
        resolved_current = current.resolve(strict=True)
    except OSError as error:
        return f"delegated cgroup path cannot be resolved: {error}"
    if resolved_root == resolved_base or not resolved_root.is_relative_to(resolved_base):
        return f"delegated cgroup {resolved_root} is outside the cgroup-v2 hierarchy"
    if resolved_current.parent != resolved_root or resolved_current.name != supervisor_name:
        return (
            f"current cgroup {resolved_current} is not the exact "
            f"{supervisor_name!r} leaf beneath delegated root {resolved_root}"
        )
    try:
        roster = (resolved_current / "cgroup.procs").read_text().splitlines()
    except OSError as error:
        return f"cannot read delegated supervisor roster: {error}"
    if str(pid) not in {line.strip() for line in roster}:
        return (
            f"pid {pid} is not listed in delegated supervisor "
            f"{resolved_current / 'cgroup.procs'}"
        )
    if not (resolved_root / "cgroup.kill").exists():
        return f"delegated root {resolved_root} has no cgroup.kill ownership boundary"
    return None


def delegated_cgroup_root(
    naming: ScopeNaming = DEFAULT_NAMING,
) -> tuple[Path | None, str | None]:
    """Validate and return the outer step root delegated to this nested scheduler.

    ``(None, None)`` means no delegation was offered. ``(None, reason)`` means an environment
    value was present but did not match the live kernel membership and must be refused.
    """
    raw = os.environ.get(DELEGATED_CGROUP_ENV)
    if raw is None:
        return None, None
    if not raw or "\0" in raw:
        return None, f"${DELEGATED_CGROUP_ENV} is empty or malformed"
    root = Path(raw)
    current = _my_cgroup_path()
    if not root.is_absolute() or current is None:
        return None, (
            f"${DELEGATED_CGROUP_ENV} must be an absolute live cgroup path and the current "
            "cgroup must be observable"
        )
    refusal = _validate_delegated_cgroup_root_at(
        root,
        current,
        os.getpid(),
        supervisor_name=naming.supervisor_name,
    )
    if refusal is not None:
        return None, refusal
    return root.resolve(), None


def promised_unit(*, naming: ScopeNaming = DEFAULT_NAMING) -> str | None:
    """The unit name the parent promised this child, if any."""
    return os.environ.get(naming.env_scope_unit) or None


def expected_scope_runtime_max_s() -> int | None:
    """The ``RuntimeMaxSec`` the parent asked systemd to enforce on this run's scope, if any."""
    raw = os.environ.get(EXPECTED_RUNTIME_MAX_ENV, "")
    try:
        value = int(raw)
    except ValueError:
        return None
    return value if value > 0 else None


def parse_systemd_duration_secs(raw: str) -> int | None:
    """Seconds from a systemd-rendered duration, or ``None`` for ``infinity``/unparsable.

    A ``*USec`` property is NOT necessarily printed as a number. ``systemctl show --value``
    renders it however the running systemd chooses: systemd 259 prints ``"1min 6s"`` where a
    microsecond integer was assumed, so an integer-only parser reads a correctly-enforced bound as
    unproven and fails the run closed. Accepts a bare microsecond integer and the human form.
    """
    text = raw.strip()
    if not text or text.lower() == "infinity":
        return None
    if text.isdigit():
        return int(text) // 1_000_000
    scales = {
        "us": 1e-6, "usec": 1e-6, "\u00b5s": 1e-6,
        "ms": 1e-3, "msec": 1e-3,
        "": 1.0, "s": 1.0, "sec": 1.0, "second": 1.0, "seconds": 1.0,
        "m": 60.0, "min": 60.0, "minute": 60.0, "minutes": 60.0,
        "h": 3600.0, "hr": 3600.0, "hour": 3600.0, "hours": 3600.0,
        "d": 86400.0, "day": 86400.0, "days": 86400.0,
        "w": 604800.0, "week": 604800.0, "weeks": 604800.0,
    }
    total = 0.0
    for token in text.split():
        idx = len(token)
        for i, ch in enumerate(token):
            if not (ch.isdigit() or ch == "."):
                idx = i
                break
        number, unit = token[:idx], token[idx:]
        try:
            value = float(number)
        except ValueError:
            return None
        if unit not in scales:
            return None
        total += value * scales[unit]
    return round(total)


def verify_scope_runtime_max(
    expected_s: int, *, naming: ScopeNaming = DEFAULT_NAMING
) -> bool:
    """Confirm the OUTER scope really carries the ``RuntimeMaxSec`` that was requested.

    PROXY BINDING: passing ``-p RuntimeMaxSec=N`` to ``systemd-run`` is a request, not enforcement.
    This reads the property back off the live unit and compares, so "the run is bounded" is a
    statement about the running unit rather than about an argument vector.
    """
    unit = os.environ.get(naming.env_scope_unit, "")
    if not unit:
        _warn(
            naming,
            "outer RuntimeMaxSec audit unavailable: scope unit name not carried into this child",
        )
        return False
    try:
        out = subprocess.run(
            ["systemctl", "--user", "show", unit, "--property=RuntimeMaxUSec", "--value"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        _warn(naming, "outer RuntimeMaxSec audit unavailable: systemctl could not be run")
        return False
    raw = out.stdout.strip()
    actual = parse_systemd_duration_secs(raw)
    if actual == expected_s:
        print(
            f"{naming.log_prefix} outer scope run budget ENFORCED: {unit} "
            f"RuntimeMaxSec={expected_s}s (read back from the live unit).",
            file=sys.stderr,
        )
        return True
    _warn(
        naming,
        f"outer RuntimeMaxSec readback MISMATCH on {unit}: requested {expected_s}s, unit "
        f"reports {raw!r}; the outer scope bound is NOT proven",
    )
    return False


#: Set to ``1`` to run the capability probe even under ``CI``/``GITHUB_ACTIONS``.
#:
#: THE MEASUREMENT INSTRUMENT THE POLICY SKIP REMOVED. Ephemeral hosted runners are a population
#: nobody has measured precisely because the skip means the probe never runs there. This makes the
#: question answerable on any runner without editing code or changing the default.
FORCE_ATTEMPT_ENV = "DAGRUN_FORCE_SCOPE_ATTEMPT"


class ScopeAttemptKind(Enum):
    """Why :func:`attempt_scope_reexec` returned instead of exec-ing into a scope."""

    #: Already inside the managed scope (anti-recursion). Genuinely boxed.
    ALREADY_IN_SCOPE = "already-in-scope"
    #: No attempt was made, by policy. Says NOTHING about whether boxing is possible here.
    SKIPPED_BY_POLICY = "skipped-by-policy"
    #: An attempt was made and the environment cannot support it.
    UNAVAILABLE = "unavailable"
    #: The scope was buildable but ``execvp`` came back, which means it failed.
    EXEC_FAILED = "exec-failed"


@dataclass(frozen=True)
class ScopeAttempt:
    """The outcome of a scope re-exec attempt, and why.

    THE BOOL THIS REPLACES RETURNED SUCCESS TO MEAN DID-NOT-ATTEMPT. ``True`` covered both "already
    boxed, proceed" and "policy said skip, we never asked whether boxing was possible"; ``False``
    covered both "the probe said no" and "the exec failed". The only caller folded all four back
    into one error and chose its wording from the bool, so the policy-skip path printed a claim
    about a capability nothing had tested.
    """

    kind: ScopeAttemptKind
    detail: str = ""

    @property
    def may_proceed(self) -> bool:
        """Whether the caller may run (as opposed to refusing).

        Exactly the bool this replaces, so no policy rides in the change: the two "carry on"
        outcomes are the two that returned ``True``.
        """
        return self.kind in (
            ScopeAttemptKind.ALREADY_IN_SCOPE,
            ScopeAttemptKind.SKIPPED_BY_POLICY,
        )

    @property
    def is_contained(self) -> bool:
        """Whether containment was actually established."""
        return self.kind is ScopeAttemptKind.ALREADY_IN_SCOPE

    @property
    def attempted(self) -> bool:
        """Whether the capability question was even asked."""
        return self.kind in (ScopeAttemptKind.UNAVAILABLE, ScopeAttemptKind.EXEC_FAILED)

    def describe(self) -> str:
        """One clause naming what happened, for a caller composing a diagnostic."""
        if self.kind is ScopeAttemptKind.ALREADY_IN_SCOPE:
            return "already inside the managed scope"
        if self.kind is ScopeAttemptKind.SKIPPED_BY_POLICY:
            return (
                f"scope setup was SKIPPED BY POLICY because ${self.detail} is set; whether "
                f"boxing is possible here was NOT tested (set {FORCE_ATTEMPT_ENV}=1 to find out)"
            )
        if self.kind is ScopeAttemptKind.UNAVAILABLE:
            return f"scope setup was attempted and failed: {self.detail}"
        return f"the scope was created but exec failed: {self.detail}"


def policy_skip_reason(*, skip_in_ci: bool = True) -> str | None:
    """The environment variable selecting a policy skip here, or ``None``."""
    if not skip_in_ci:
        return None
    if os.environ.get(FORCE_ATTEMPT_ENV) == "1":
        return None
    if os.environ.get("GITHUB_ACTIONS"):
        return "GITHUB_ACTIONS"
    if os.environ.get("CI"):
        return "CI"
    return None


def reexec_in_scope(
    argv: Sequence[str],
    *,
    memory_max: int | None,
    cpu_count: int | None = None,
    naming: ScopeNaming = DEFAULT_NAMING,
    use_aggregate_slice: bool = True,
    skip_in_ci: bool = True,
    runtime_max_s: int | None = None,
) -> bool:
    """Re-exec ``argv`` inside a transient ``systemd-run --user --scope`` (a
    delegated cgroup), so EVERY descendant — including ``setsid``/double-forked
    escapees the per-step ``killpg`` reaper can't catch — is contained and reaped
    atomically when the run ends or via ``systemctl --user stop <unit>.scope``.

    On success ``os.execvp`` REPLACES this process and never returns. The bool
    return distinguishes the paths that DON'T re-exec:

      * ``True``  — already in-scope (anti-recursion) or intentionally skipped in
        CI: the caller should proceed to run directly.
      * ``False`` — systemd scope is unavailable or the exec failed: the caller
        must refuse to run advisory-only (No Silent Failure — the reason is
        written to stderr).

    ``Delegate=yes`` makes the scope a DELEGATED cgroup so the in-scope runner
    can carve per-step CHILD cgroups under it (:class:`Cgroups`) for
    ``setsid``-proof teardown via ``cgroup.kill``. ``MemorySwapMax=0`` is applied
    independently of the optional RAM hard cap: a run must never become a
    host-side swap-kill candidate."""
    if os.environ.get(naming.env_in_scope) == "1":
        return True  # already re-exec'd into the scope (anti-recursion)
    # POLICY SKIP, UNCHANGED IN EFFECT AND NOW HONEST ABOUT ITSELF: stated rather than silent, no
    # longer borrowing the capability probe's wording for a probe it did not run, and liftable for
    # one run via FORCE_ATTEMPT_ENV. Whether the skip is CORRECT is a separate question.
    skip_reason = policy_skip_reason(skip_in_ci=skip_in_ci)
    if skip_reason is not None:
        sys.stderr.write(
            f"{naming.log_prefix} scope setup SKIPPED BY POLICY (${skip_reason} is set). This "
            f"did NOT test whether cgroup boxing is available here; set {FORCE_ATTEMPT_ENV}=1 to "
            "probe instead of skipping.\n"
        )
        return True
    if memory_max is None:
        memory_max = outer_memory_max_bytes()
    if memory_max is None:
        sys.stderr.write(
            f"{naming.log_prefix} ERROR: cannot derive a positive outer MemoryMax; "
            "refusing an unbounded scope.\n"
        )
        return False
    if not systemd_scope_available(naming):
        sys.stderr.write(f"{naming.log_prefix} ERROR: systemd --user scope is unavailable; "
                         "refusing advisory-only containment.\n")
        return False

    unit = f"{naming.unit_prefix}-{os.getpid()}"
    # Swaplessness is independent of the optional RAM hard cap (a 40 GB runaway
    # swapping into a smaller RAM still thrashes the host).
    props = [
        "-p", "Delegate=yes",
        "-p", "MemorySwapMax=0",
    ]
    if cpu_count is not None:
        if cpu_count > (2**63 - 1) // 100:
            sys.stderr.write(
                f"{naming.log_prefix} ERROR: requested CPU count {cpu_count} is too large to "
                "encode as a systemd CPUQuota percentage.\n"
            )
            return False
        props += ["-p", f"CPUQuota={cpu_count * 100}%"]
        print(
            f"{naming.log_prefix} per-run CPU cap: CPUQuota={cpu_count * 100}% "
            f"({cpu_count} total core{'s' if cpu_count != 1 else ''})."
        )

    # Launch under the SHARED aggregate slice so the kernel shares its CPUQuota
    # across however many runs execute concurrently — bounding the AGGREGATE, not
    # each one. Best-effort: without the slice we run unconstrained (as before).
    slice_args: list[str] = []
    if use_aggregate_slice and ensure_aggregate_slice(naming=naming):
        slice_args = [f"--slice={naming.slice_name}"]
        quota = cpu_quota_percent()
        print(f"{naming.log_prefix} CPU cap: shared {naming.slice_name} CPUQuota={quota}% "
              f"(~90% of {os.cpu_count()} cores, AGGREGATE across concurrent runs).")

    if memory_max:
        # MemoryMax is the sole memory limit; no MemoryHigh (reclaim throttling
        # creates spurious pressure signals). MemorySwapMax=0 already OOM-kills a
        # runaway at the cap rather than letting it swap.
        props += ["-p", f"MemoryMax={memory_max}"]
    if runtime_max_s is not None and runtime_max_s > 0:
        # OUTERMOST bound the machine itself enforces, and a LAST RESORT rather than the working
        # timeout: systemd terminates the whole scope, so anything not already flushed to disk is
        # lost. Set it strictly LARGER than the runner's own in-process run budget, so the
        # ordering is per-step < in-process run budget < scope RuntimeMaxSec < CI job kill. Each
        # level exists to stop the next one from being the thing that fires.
        props += ["-p", f"RuntimeMaxSec={runtime_max_s}"]
        props += [f"--setenv={EXPECTED_RUNTIME_MAX_ENV}={runtime_max_s}"]
        print(
            f"{naming.log_prefix} outer scope run budget: RuntimeMaxSec={runtime_max_s}s "
            "(systemd terminates the whole scope; the runner's own budget must fire first).",
            file=sys.stderr,
        )
        print(f"{naming.log_prefix} outer scope memory cap: MemoryMax="
              f"{_fmt_bytes(memory_max)} (hard-cap-only, swap=0).")

    # Scope-wide build width. When the operator stated none, this is derived from the granted
    # cores + memory cap, so a command run directly in the scope (not via a per-step child) still
    # can't compute NUM_JOBS=<all cores>, and per-step prepare_command refines it downward. When
    # the operator DID state one, theirs wins. Mirrors reexec_in_scope in the Rust engine.
    build_jobs = select_build_jobs(cpu_count, memory_max)
    print(f"{naming.log_prefix} {build_jobs.describe()}", file=sys.stderr)
    cmd = ["systemd-run", "--user", "--scope", "--collect", "--quiet",
           f"--unit={unit}", *slice_args, *props,
           f"--setenv={BUILD_JOBS_ENV}={build_jobs.jobs}",
           # ACROSS THE RE-EXEC, INTENT TRAVELS UNDER ITS OWN NAME. The line above puts the
           # runner's own number into the child's CARGO_BUILD_JOBS; without this the child would
           # read it back as an operator instruction and stop refining per step.
           f"--setenv={OPERATOR_BUILD_JOBS_ENV}="
           f"{'' if build_jobs.operator is None else build_jobs.operator}",
           f"--setenv={naming.env_in_scope}=1",
           f"--setenv={naming.env_scope_unit}={unit}.scope",
           f"--setenv={EXPECTED_OUTER_MEMORY_MAX_ENV}={memory_max or ''}",
           f"--setenv={EXPECTED_OUTER_CPU_COUNT_ENV}={cpu_count or ''}",
           "--", *argv]
    print(f"{naming.log_prefix} re-exec inside transient systemd scope {unit}.scope "
          "(two-level cgroup; full-descendant cleanup on exit)…")
    sys.stdout.flush()
    try:
        os.execvp("systemd-run", cmd)  # replaces this process; never returns
    except OSError as exc:
        sys.stderr.write(f"{naming.log_prefix} ERROR: systemd-run exec failed ({exc}); "
                         "refusing to run without cgroup enforcement.\n")
        return False


def install_scope_teardown(
    naming: ScopeNaming = DEFAULT_NAMING,
    on_teardown: Callable[[], None] | None = None,
) -> None:
    """Inside the scope, make Ctrl-C / ``kill`` of the runner tear down the WHOLE
    cgroup — not just this PID. Killing only the scoped runner would leave
    ``setsid``-escapee orphans alive in the scope cgroup (``killpg`` AND
    ``--collect`` both miss them). On SIGINT/SIGTERM this ``systemctl --user
    stop``s our OWN scope (or, for the systemd-free delegated case, writes the
    scope's ``cgroup.kill``), which SIGKILLs every child step cgroup + escapee
    atomically and kills us too (an aborted run exits with the signal code).

    The NORMAL-exit backstop is separate (:meth:`Cgroups.kill_all_remaining`),
    which does NOT stop the scope so a SUCCESSFUL run's exit code is preserved.

    ``on_teardown`` runs BEFORE the scope is stopped (e.g. to release a lock the
    ``finally`` won't reach once the SIGKILL lands). No-op when not in-scope."""
    if os.environ.get(naming.env_in_scope) != "1":
        return
    unit = os.environ.get(naming.env_scope_unit)
    # Resolve the scope cgroup path NOW (not inside the handler) so the handler's
    # cgroup.kill is a single fast file-write with no systemctl shell-out (which
    # can stall under load).
    scope_cg = scope_cgroup_from_self(naming)
    if not unit and scope_cg is None:
        return

    def _on_signal(signum: int, _frame: FrameType | None) -> None:
        try:
            scope_name = unit or str(scope_cg)
            sys.stderr.write(f"\n{naming.log_prefix} signal {signum} — stopping scope "
                             f"{scope_name} (tears down all steps + orphans)…\n")
            sys.stderr.flush()
        except Exception:
            pass
        if on_teardown is not None:
            try:
                on_teardown()
            except Exception:
                pass
        if unit:
            stop_scope(unit, scope_cg, naming=naming)
        elif scope_cg is not None:
            kill_scope_cgroup(scope_cg)
        os._exit(128 + signum)

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, _on_signal)
        except (ValueError, OSError):
            pass


# --------------------------------------------------------------------------- #
# Scope discovery + teardown                                                  #
# --------------------------------------------------------------------------- #


def _scope_cgroup_path(unit: str) -> Path | None:
    """Filesystem cgroup path of a --user transient scope (via systemctl)."""
    try:
        r = subprocess.run(
            ["systemctl", "--user", "show", unit, "--property=ControlGroup", "--value"],
            capture_output=True, text=True, timeout=8)
    except (subprocess.TimeoutExpired, OSError):
        return None
    cg = r.stdout.strip()
    return (CGROUP_ROOT / cg.lstrip("/")) if cg else None


def scope_cgroup_from_self(naming: ScopeNaming = DEFAULT_NAMING) -> Path | None:
    """The OUTER scope's cgroup path, derived from THIS process's own cgroup — no
    systemctl shell-out (fast + contention-proof, for the signal handler). Inside
    the scope the runner lives in ``<scope>/supervisor``, so the scope is the
    parent; if for some reason we're at the scope root, return it directly.
    Returns None when not in a scope."""
    direct = os.environ.get(naming.env_direct_cgroup)
    if direct:
        scope = Path(direct)
        if scope.is_dir():
            return scope
    mine = _my_cgroup_path()
    if mine is None:
        return None
    if mine.name == naming.supervisor_name:
        return mine.parent
    if mine.name.endswith(".scope"):
        return mine
    return None


def scope_memory_peak(naming: ScopeNaming = DEFAULT_NAMING) -> int | None:
    """Peak memory (bytes) of the WHOLE scope cgroup — the authoritative peak RSS
    across every step, read from the scope's ``memory.peak`` (no sampling). None
    when not in a scope / file absent."""
    scope = scope_cgroup_from_self(naming)
    if scope is None:
        return None
    try:
        return int((scope / "memory.peak").read_text().strip())
    except (OSError, ValueError):
        return None


def kill_scope_cgroup(scope: Path) -> bool:
    """Atomically SIGKILL a delegated scope cgroup and every descendant."""
    try:
        (scope / "cgroup.kill").write_text("1")
        return True
    except OSError:
        return False


def stop_scope(
    unit: str,
    scope_cg: Path | None = None,
    naming: ScopeNaming = DEFAULT_NAMING,
) -> bool:
    """Tear down the whole outer scope. Two-step for SPEED + cleanliness:

      1. ``cgroup.kill`` the scope's cgroup directly — an INSTANT, atomic SIGKILL
         of every member (including a browser, which IGNORES the SIGTERM that
         ``systemctl stop`` sends first). Without this, ``systemctl stop`` sits
         in ``stop-sigterm`` for the scope's full ``TimeoutStopSec``.
      2. ``systemctl --user stop`` to deactivate + GC the (now-empty) unit.

    Either step alone flushes the descendants; doing both makes teardown both
    immediate and tidy. SIGKILL-proof, setsid-proof. Best-effort throughout.

    ``scope_cg``, if given (from :func:`scope_cgroup_from_self`), skips the
    ``systemctl show`` lookup for step 1 — important in a SIGNAL HANDLER where
    shelling out under load can stall."""
    if not unit:
        return False
    cg = scope_cg or _scope_cgroup_path(unit)
    if cg is not None:
        kill_scope_cgroup(cg)
    try:
        subprocess.run(["systemctl", "--user", "stop", unit],
                       capture_output=True, timeout=15)
        return True
    except (subprocess.TimeoutExpired, OSError):
        return False


# --------------------------------------------------------------------------- #
# Enforcement verification + usage reporting                                  #
# --------------------------------------------------------------------------- #


def verify_scope_limits(
    expected_memory_max: int | None,
    expected_cpu_count: int | None,
    naming: ScopeNaming = DEFAULT_NAMING,
) -> bool:
    """Verify the requested outer limits reached cgroup v2; print actionable
    evidence. Intentionally a hard boolean rather than a best-effort metric: a
    run that claims containment while its requested limits are absent is unsafe."""
    scope = scope_cgroup_from_self(naming)
    if scope is None:
        print(f"{naming.log_prefix} ERROR: outer cgroup limit audit unavailable: "
              "scope not found")
        return False

    memory_max = _read_cgroup_value(scope, "memory.max")
    memory_swap_max = _read_cgroup_value(scope, "memory.swap.max")
    memory_oom_group = _read_cgroup_value(scope, "memory.oom.group")
    cpu_max = _read_cgroup_value(scope, "cpu.max")
    if expected_memory_max is None:
        memory_ok = memory_max == "max"
    else:
        actual_memory_max = _memory_max_bytes(memory_max)
        # cgroup v2 rounds a byte limit down to its page boundary. Accept exactly that kernel
        # representation, but never a broader limit.
        memory_ok = _memory_limit_matches(expected_memory_max, actual_memory_max)
    swap_ok = memory_swap_max == "0"
    oom_group_ok = memory_oom_group == "1"
    cpu_ok = True
    if expected_cpu_count is not None:
        try:
            quota, period = (int(part) for part in (cpu_max or "").split())
            cpu_ok = quota == expected_cpu_count * period
        except (TypeError, ValueError):
            cpu_ok = False

    print(f"{naming.log_prefix} outer cgroup audit: memory.max={memory_max or 'UNREADABLE'} "
          f"({'bound' if memory_ok else 'MISMATCH'}), "
          f"memory.swap.max={memory_swap_max or 'UNREADABLE'} "
          f"({'disabled' if swap_ok else 'MISMATCH'}), "
          f"memory.oom.group={memory_oom_group or 'UNREADABLE'} "
          f"({'enabled' if oom_group_ok else 'MISMATCH'}), "
          f"cpu.max={cpu_max or 'UNREADABLE'} "
          f"({'bound' if cpu_ok else 'MISMATCH'})")
    return memory_ok and swap_ok and oom_group_ok and cpu_ok


def enable_outer_oom_group(naming: ScopeNaming = DEFAULT_NAMING) -> bool:
    """Write and read back ``memory.oom.group=1`` on the outer scope.

    Older systemd versions on supported hosts reject the ``MemoryOOMGroup=``
    unit property, so the re-exec'd scoped process performs the cgroup-v2 write
    directly. A successful write is not trusted until the kernel file reads
    back as ``1``.
    """
    scope = scope_cgroup_from_self(naming)
    if scope is None:
        _warn(naming, "outer memory.oom.group: scope not found")
        return False
    control = scope / "memory.oom.group"
    try:
        control.write_text("1")
    except OSError as exc:
        _warn(naming, f"outer memory.oom.group=1 write failed ({exc})")
        return False
    actual = _read_cgroup_value(scope, "memory.oom.group")
    if actual != "1":
        _warn(naming, f"outer memory.oom.group readback mismatch: {actual or 'UNREADABLE'}")
        return False
    return True


def _quota_matches(cpu_max: str | None, expected_cpu_count: int) -> bool:
    try:
        quota, period = (int(part) for part in (cpu_max or "").split())
        return quota == expected_cpu_count * period
    except (TypeError, ValueError):
        return False


def verify_current_cpu_enforcement(
    expected_cpu_count: int,
    naming: ScopeNaming = DEFAULT_NAMING,
) -> CgroupEnforcementKind | None:
    """Prove the container CPU boundary from quota or effective cpuset state."""
    current = _my_cgroup_path()
    if current is None:
        print(f"{naming.log_prefix} container cgroup audit: current cgroup UNREADABLE")
        return None

    cpu_max = _read_cgroup_value(current, "cpu.max")
    cpuset = _read_cgroup_value(current, "cpuset.cpus.effective")
    cpuset_count = _cpuset_count(cpuset)
    kind: CgroupEnforcementKind | None
    if _quota_matches(cpu_max, expected_cpu_count):
        kind = CgroupEnforcementKind.CONTAINER_QUOTA
    elif cpuset_count == expected_cpu_count:
        kind = CgroupEnforcementKind.CONTAINER_CPUSET
    else:
        kind = None
        parent = current.parent
        while parent == CGROUP_ROOT or CGROUP_ROOT in parent.parents:
            if _quota_matches(_read_cgroup_value(parent, "cpu.max"), expected_cpu_count):
                kind = CgroupEnforcementKind.CONTAINER_QUOTA
                break
            if parent == CGROUP_ROOT:
                break
            parent = parent.parent

    print(f"{naming.log_prefix} container cgroup audit: cpu.max={cpu_max or 'UNREADABLE'}; "
          f"cpuset.cpus.effective={cpuset or 'UNREADABLE'} "
          f"(count={cpuset_count if cpuset_count is not None else 'unknown'}); "
          f"enforcement={kind.value if kind is not None else 'UNVERIFIED'}")
    return kind


def report_scope_usage(naming: ScopeNaming = DEFAULT_NAMING) -> bool:
    """Print outer cgroup peak/OOM/CPU evidence; return False on OOM or unreadable
    stats."""
    scope = scope_cgroup_from_self(naming)
    if scope is None:
        print(f"{naming.log_prefix} ERROR: outer cgroup usage audit unavailable: "
              "scope not found")
        return False
    peak = _read_cgroup_value(scope, "memory.peak")
    events = _read_cgroup_value(scope, "memory.events")
    cpu_stat = _read_cgroup_value(scope, "cpu.stat")
    if peak is None or events is None or cpu_stat is None:
        print(f"{naming.log_prefix} ERROR: outer cgroup usage audit could not read "
              "memory.peak, memory.events, and cpu.stat")
        return False
    event_values = dict(line.split(maxsplit=1) for line in events.splitlines())
    oom_kill = int(event_values.get("oom_kill", "0"))
    print(f"{naming.log_prefix} outer cgroup usage: memory.peak={peak} bytes; "
          f"memory.events oom={event_values.get('oom', '0')} oom_kill={oom_kill}; "
          f"cpu.stat {cpu_stat.replace(chr(10), ' ')}")
    return oom_kill == 0


def report_current_usage(naming: ScopeNaming = DEFAULT_NAMING) -> bool:
    """Print the current (container) cgroup usage evidence; return False on OOM or
    unreadable stats — the systemd-free/container counterpart of
    :func:`report_scope_usage`."""
    current = _my_cgroup_path()
    if current is None:
        print(f"{naming.log_prefix} ERROR: current cgroup unavailable")
        return False
    peak = _read_cgroup_value(current, "memory.peak")
    events = _read_cgroup_value(current, "memory.events")
    cpu_stat = _read_cgroup_value(current, "cpu.stat")
    if peak is None or events is None or cpu_stat is None:
        print(f"{naming.log_prefix} ERROR: container cgroup accounting files unreadable")
        return False
    event_values = dict(line.split(maxsplit=1) for line in events.splitlines())
    oom_kill = int(event_values.get("oom_kill", "0"))
    print(f"{naming.log_prefix} container cgroup usage: memory.peak={peak} bytes; "
          f"oom_kill={oom_kill}; cpu.stat {cpu_stat.replace(chr(10), ' ')}")
    return oom_kill == 0


# --------------------------------------------------------------------------- #
# Opt-in size-K core box (whole-tree cpuset)                                  #
# --------------------------------------------------------------------------- #
#
# The ``--cores K`` feature constrains the WHOLE run process tree to K least-busy
# FREE cores. It never pins a fixed core id (a fixed core may be busy): it reads
# THIS process's allowed set, samples ``/proc/stat`` briefly, and picks the K
# LEAST-BUSY of them.
#
# The only accepted mechanism is a cgroup cpuset on the containing scope. A root
# process affinity mask is not containment: a descendant can widen or replace it.
# If the exact effective cpuset cannot be verified, the operation fails closed.


def read_proc_text(path: str) -> str | None:
    """Read a ``/proc`` file, or ``None`` when it cannot be read.

    A deliberate seam. Both per-CPU signals go through it so a test can supply
    fixture text without monkeypatching ``builtins.open``, and so "unreadable"
    is one explicit value rather than an exception caught in two places.
    """
    try:
        with open(path) as handle:
            return handle.read()
    except OSError:
        return None


def device_irq_counts() -> dict[int, int]:
    """Cumulative DEVICE interrupt count per CPU, from ``/proc/interrupts``.

    Only numerically-named rows are summed. That restriction is measured, not
    stylistic: on a 316-CPU host the architectural/IPI rows (``LOC``, ``RES``,
    ``CAL``, ``TLB``, ...) spanned only 49-4846/s across CPUs -- a 3.6x spread
    with ZERO CPUs above 4x the median -- so including them would rank cores by
    "how busy is this CPU", which :func:`pick_least_busy_free_cores` already
    measures from ``/proc/stat``. The device rows on the same host spanned
    0.0-1100.6/s with 45.6% of CPUs at exactly zero: a real, ~5000x signal about
    where a benchmark would be interrupted by hardware it does not control.

    Parsing is deliberately tight. ``/proc/interrupts`` is ~2.7 MB and ~758 rows
    wide here, and this runs inside the reservation ledger's ``flock``.
    """
    text = read_proc_text("/proc/interrupts")
    if text is None:
        # No /proc/interrupts (non-Linux, restricted sandbox) is a MISSING
        # signal, not a quiet host. The caller degrades to /proc/stat ranking
        # rather than treating every core as interrupt-free.
        return {}
    return parse_device_irq_counts(text)


def parse_device_irq_counts(text: str) -> dict[int, int]:
    """Pure parser for ``/proc/interrupts`` text.

    Split out from the read so it can be exercised on fixture text: the core
    ordering guarantee is only as good as agreement on what counts as a device
    row, and that is worth pinning down directly rather than through a live
    ``/proc`` read.
    """
    lines = text.splitlines()
    if not lines:
        return {}
    header = lines[0].split()
    ncpu = len(header)
    if ncpu == 0:
        return {}
    totals = [0] * ncpu
    for line in lines[1:]:
        colon = line.find(":")
        if colon <= 0:
            continue
        name = line[:colon].strip()
        if not name or not name.isdigit():
            continue
        fields = line[colon + 1 :].split(maxsplit=ncpu)
        if len(fields) < ncpu:
            continue
        for index in range(ncpu):
            value = fields[index]
            if value.isdigit():
                totals[index] += int(value)
    counts: dict[int, int] = {}
    for index, name in enumerate(header):
        lowered = name.lower()
        if lowered.startswith("cpu") and lowered[3:].isdigit():
            counts[int(lowered[3:])] = totals[index]
    return counts


def device_irq_rates(sample_s: float = 0.3) -> dict[int, float]:
    """Per-CPU device interrupts per second, sampled over ``sample_s``.

    Returns ``{}`` when the signal is unavailable, so a caller can distinguish
    "no interrupts here" from "could not look". Reporting an absent signal as
    zero would let a restricted sandbox certify every core as interrupt-free.
    """
    if not math.isfinite(sample_s) or sample_s <= 0:
        return {}
    before = device_irq_counts()
    if not before:
        return {}
    start = time.monotonic()
    time.sleep(sample_s)
    after = device_irq_counts()
    elapsed = time.monotonic() - start
    if not after or elapsed <= 0:
        return {}
    rates: dict[int, float] = {}
    for cpu, first in before.items():
        second = after.get(cpu)
        if second is None:
            continue
        if second >= first:
            rates[cpu] = (second - first) / elapsed
    return rates


def pick_least_busy_free_cores(
    k: int,
    sample_s: float = 0.3,
    exclude: Collection[int] = (),
    max_irq_rate: float | None = None,
) -> list[int]:
    """Pick ``k`` QUIETEST cores from THIS process's allowed CPU set.

    Reads the allowed set (``sched_getaffinity``), samples per-CPU idle jiffies
    from ``/proc/stat`` over ``sample_s`` seconds, and returns the ``k`` cores
    with the highest idle fraction rather than assuming a fixed core is free.
    ``k`` is clamped to ``[1, len(allowed)]``.

    ``exclude`` removes cores already HELD by a concurrent reservation BEFORE
    ranking, so the least-busy heuristic (which prevents over-use) is composed
    with a held-set (which prevents COLLISION). Idle-fraction sampling alone
    cannot prevent two callers picking the same idle core at the same instant;
    the caller passes the reservation ledger's held set here. Additive: the
    default empty ``exclude`` preserves the standalone behavior.

    IRQ AWARENESS. Device interrupt rate is the PRIMARY key, ahead of idle
    fraction, because an interrupt-hot core is a confound a benchmark cannot
    control, not merely a loaded one. The mechanism is a RANKING rather than a
    threshold, and that choice is measured: across 8 independent 0.3s samples on
    a 316-CPU host, only 4 of the 6 extreme cores (>100 IRQ/s) appeared in the
    top-32 of every sample and the worst was detected in 2 of 8, so ANY fixed
    threshold would misclassify a bursty signal. Ranking needs no core to be
    correctly classified -- only that enough quiet cores exist to fill ``k``.
    Measured on the same host, ``quietest-k`` handed out zero hot and zero
    extreme cores for k of 1, 4 and 16, versus 2.3 hot expected from a uniform
    draw at k=16; the guarantee begins to erode near k=32, where the quiet
    population stops covering the request. Those figures describe ONE host and
    are quoted to show the rule was derived rather than guessed; re-measure
    before relying on them elsewhere, since both the interrupt distribution and
    the size of the quiet population are host properties.

    ``max_irq_rate`` is opt-in and has NO default, for the same reason: a caller
    that knows its own interrupt budget states it, and the tool does not invent
    one. When set, cores above it are dropped before ranking, and fewer than
    ``k`` cores may be returned -- an observable shortfall the caller can act on
    rather than a silent downgrade."""
    if k < 1 or not math.isfinite(sample_s) or sample_s < 0:
        return []
    if max_irq_rate is not None:
        if not math.isfinite(max_irq_rate) or max_irq_rate < 0 or sample_s <= 0:
            return []
    excluded = frozenset(int(c) for c in exclude)
    allowed = [c for c in sorted(os.sched_getaffinity(0)) if c not in excluded]
    if not allowed:
        return []

    def snap() -> dict[int, tuple[int, int]]:
        d: dict[int, tuple[int, int]] = {}
        text = read_proc_text("/proc/stat")
        if text is None:
            return d
        for line in text.splitlines():
            if line.startswith("cpu") and len(line) > 3 and line[3].isdigit():
                p = line.split()
                cid = int(p[0][3:])
                idle = int(p[4]) + int(p[5])  # idle + iowait
                total = sum(int(x) for x in p[1:])
                d[cid] = (idle, total)
        return d

    # One sleep serves both signals: interleaving the interrupt snapshot with
    # the /proc/stat snapshot keeps the critical section the same length it was
    # before IRQ awareness existed.
    a = snap()
    irq_a = device_irq_counts()
    started = time.monotonic()
    time.sleep(sample_s)
    b = snap()
    irq_b = device_irq_counts()
    elapsed = time.monotonic() - started

    irq_rates: dict[int, float] = {}
    if irq_a and irq_b and elapsed > 0:
        for cpu, first in irq_a.items():
            second = irq_b.get(cpu)
            if second is not None and second >= first:
                irq_rates[cpu] = (second - first) / elapsed

    if max_irq_rate is not None:
        if not irq_rates:
            # The budget cannot be honoured because the signal is missing. A
            # silent full-set fallback would report success for a guarantee
            # that was never checked.
            return []
        allowed = [
            c for c in allowed if c in irq_rates and irq_rates[c] <= max_irq_rate
        ]
        if not allowed:
            return []
    k = max(1, min(int(k), len(allowed)))

    def idle_frac(c: int) -> float:
        di = b[c][0] - a[c][0]
        dt = b[c][1] - a[c][1]
        return di / dt if dt else 1.0

    def valid_cpu_delta(c: int) -> bool:
        return c in a and c in b and b[c][0] >= a[c][0] and b[c][1] >= a[c][1]

    # Interrupt rate leads; idle fraction breaks its ties; core id breaks those,
    # so the order is total and both engines agree exactly. When /proc/interrupts
    # is unreadable every rate is absent and this degrades to the historical
    # idle-fraction ranking.
    ranked = sorted(
        (c for c in allowed if valid_cpu_delta(c)),
        key=lambda c: (c not in irq_rates, irq_rates.get(c, 0.0), -idle_frac(c), c),
    )
    return ranked[:k]


def _cpulist(cores: Sequence[int]) -> str:
    """Render cores as a Linux cpulist (plain comma form; the kernel may re-render
    it with ranges in ``cpuset.cpus.effective``, which :func:`_parse_cpuset`
    parses back)."""
    return ",".join(str(c) for c in cores)


def _try_cgroup_cpuset(scope: Path, cores: Sequence[int]) -> bool:
    """Write ``cpuset.cpus`` on ``scope`` and VERIFY via ``cpuset.cpus.effective``.

    Returns True only when the ``cpuset`` controller is present on the scope AND
    the effective cpuset is exactly the requested set after the write. Checking
    only the count is insufficient because a different same-sized set would break
    collision-free reservations. On
    success, best-effort enables ``+cpuset`` in the scope's ``subtree_control`` so
    per-step child cgroups inherit the constraint."""
    if "cpuset" not in _controller_set(scope, "cgroup.controllers"):
        return False
    wanted = set(cores)
    try:
        (scope / "cpuset.cpus").write_text(_cpulist(cores))
    except OSError:
        return False
    if _parse_cpuset(_read_cgroup_value(scope, "cpuset.cpus.effective")) != wanted:
        return False
    # Verified. Let per-step child cgroups inherit the cpuset constraint.
    try:
        (scope / "cgroup.subtree_control").write_text("+cpuset")
    except OSError:
        pass  # best-effort: the scope-level cpuset already bounds the whole tree
    return True


def apply_core_box(
    k: int, naming: ScopeNaming = DEFAULT_NAMING
) -> tuple[str, list[int]] | None:
    """Constrain the WHOLE run process tree to ``k`` least-busy FREE cores.

    Requires a cgroup ``cpuset`` on the containing scope and verifies the exact
    effective set. Returns ``None`` with a warning when hard tree-wide pinning is
    unavailable. It never falls back to process affinity, which descendants can
    widen and therefore cannot enforce a reservation.

    Call this in the runner BEFORE the scheduler spawns worker threads or forks
    any step: pthreads inherit the creator's affinity and forked steps inherit at
    fork, so an early application covers the whole tree.

    This is the STANDALONE picker path (no cross-process reservation): it prevents
    OVER-use but two concurrent calls can pick the same idle core. For
    collision-free allocation across concurrent runs, use
    :func:`dagrun.reservation.acquire` and pass its cores to
    :func:`apply_specific_cores`."""
    cores = pick_least_busy_free_cores(k)
    if not cores:
        _warn(naming, f"--cores {k}: no allowed CPUs found (sched_getaffinity empty); "
              "cannot constrain the run tree to a core box")
        return None
    return apply_specific_cores(cores, naming, label=f"--cores {k}")


def apply_specific_cores(
    cores: Sequence[int], naming: ScopeNaming = DEFAULT_NAMING, label: str = "core box"
) -> tuple[str, list[int]] | None:
    """Constrain the WHOLE run tree to an EXPLICIT set of ``cores``.

    The caller supplies the cores (for example, from the reservation ledger).
    Success requires an exact cgroup ``cpuset.cpus.effective`` match; otherwise
    this function fails closed with ``None``."""
    cores = list(cores)
    if not cores:
        _warn(naming, f"{label}: empty core set; cannot constrain the run tree")
        return None
    want = len(cores)

    # A cgroup cpuset is a hard descendant-tree bound. Process affinity is not:
    # any child may call sched_setaffinity and escape a mask it inherited.
    owns_scope = (
        os.environ.get(naming.env_in_scope) == "1"
        or bool(os.environ.get(naming.env_direct_cgroup))
    )
    scope = scope_cgroup_from_self(naming) if owns_scope else None
    if scope is not None and _try_cgroup_cpuset(scope, cores):
        print(
            f"{naming.log_prefix} core box: constrained to {want} core(s) "
            f"{cores} via cgroup cpuset",
            file=sys.stderr,
            flush=True,
        )
        return "cgroup cpuset", cores
    _warn(
        naming,
        f"{label}: exact cgroup cpuset unavailable; refusing a soft process-affinity "
        "fallback because descendants can escape it",
    )
    return None


# --------------------------------------------------------------------------- #
# Per-step child cgroups (the concrete CgroupManager)                         #
# --------------------------------------------------------------------------- #


def _drain_scope_root_with(
    read_pids: Callable[[], list[str]],
    move_pid: Callable[[str], None],
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    """Drain a scope root and require two consecutive empty observations.

    A single empty read is not a stable systemd hand-off: a late helper may enter before the
    subsequent ``cgroup.subtree_control`` write, which then fails with ``EBUSY``. The callbacks
    keep that timing contract testable without pretending a regular fixture file has cgroupfs's
    process-migration semantics.
    """
    consecutive_empty = 0
    for attempt in range(_SCOPE_DRAIN_ATTEMPTS):
        pids = read_pids()
        if not pids:
            consecutive_empty += 1
            if consecutive_empty >= _SCOPE_DRAIN_EMPTY_SAMPLES:
                return
        else:
            consecutive_empty = 0
            for pid in pids:
                try:
                    move_pid(pid)
                except OSError:
                    # ESRCH is expected when a helper exits between the roster read and move.
                    # A process that really remains is caught by the next roster read.
                    pass
        if attempt + 1 < _SCOPE_DRAIN_ATTEMPTS:
            sleep(_SCOPE_DRAIN_RETRY_SECONDS)
    remaining = read_pids()
    remaining_text = ",".join(remaining) if remaining else "<empty on one unconfirmed final sample>"
    raise BlockingIOError(
        errno.EBUSY,
        "scope root did not quiesce after "
        f"{(_SCOPE_DRAIN_ATTEMPTS - 1) * _SCOPE_DRAIN_RETRY_SECONDS:.3f}s; "
        f"remaining pid(s): {remaining_text}",
    )


def _enable_controller_with(
    read_pids: Callable[[], list[str]],
    move_pid: Callable[[str], None],
    write_controller: Callable[[], None],
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    for attempt in range(_CONTROLLER_ENABLE_ATTEMPTS):
        _drain_scope_root_with(read_pids, move_pid, sleep)
        try:
            write_controller()
            return
        except OSError as exc:
            if exc.errno != errno.EBUSY or attempt + 1 >= _CONTROLLER_ENABLE_ATTEMPTS:
                raise
            # A member arrived after the stable-empty observation but before the controller
            # write. Drain the newly observed member and retry the guarded transition.
    raise AssertionError("the final controller-enable attempt returns its error")


def _enable_controller(scope: Path, supervisor: Path, controller: str) -> None:
    def move(pid: str) -> None:
        (supervisor / "cgroup.procs").write_text(pid)

    def write_controller() -> None:
        (scope / "cgroup.subtree_control").write_text(f"+{controller}")

    _enable_controller_with(
        lambda: (scope / "cgroup.procs").read_text().split(),
        move,
        write_controller,
    )


def _remove_empty_cgroup_descendants(root: Path) -> None:
    """Best-effort depth-first removal of child cgroups, never files or ``root`` itself."""
    try:
        children = [entry for entry in root.iterdir() if entry.is_dir()]
    except OSError:
        return
    for child in children:
        _remove_empty_cgroup_descendants(child)
        try:
            child.rmdir()
        except OSError:
            pass


def _move_current_to_nested_supervisor(
    run_root: Path,
    naming: ScopeNaming = DEFAULT_NAMING,
) -> None:
    """Move this scheduler into its capped run root and verify live membership.

    Inner steps already join descendants of ``run_root``. The coordinator must join as well or a
    tighter nested ``--max-mem``/``--max-cpus`` would not be a whole-invocation boundary.
    """
    supervisor = run_root / naming.supervisor_name
    return_supervisor = _my_cgroup_path()
    if return_supervisor is None:
        raise OSError("current cgroup is unreadable before nested scheduler migration")
    pid = os.getpid()
    (supervisor / "cgroup.procs").write_text(str(pid))
    try:
        current = _my_cgroup_path()
        if current is None or current.resolve() != supervisor.resolve():
            raise OSError(
                f"nested scheduler pid {pid} did not enter {supervisor}; "
                f"current cgroup is {current}"
            )
        roster = (supervisor / "cgroup.procs").read_text().split()
        if str(pid) not in roster:
            raise OSError(
                f"nested scheduler pid {pid} is absent from {supervisor / 'cgroup.procs'}"
            )
    except OSError:
        # The write may have migrated us even when the subsequent proof failed. Never strand an
        # in-process caller under a partial/tighter invocation boundary.
        try:
            _restore_pid_to_cgroup(return_supervisor, pid)
        except OSError as rollback_error:
            # Retain the exact recovery target. The manager destructor and the public CLI's
            # invocation boundary will retry; a persistent failure turns success into exit 3.
            _NESTED_COORDINATOR_STACK.append((run_root, return_supervisor, naming))
            _warn(
                naming,
                f"could not roll back failed nested scheduler migration to "
                f"{return_supervisor} ({rollback_error})",
            )
        raise
    _NESTED_COORDINATOR_STACK.append((run_root, return_supervisor, naming))


def _restore_pid_to_cgroup(target: Path, pid: int) -> None:
    """Move ``pid`` to ``target`` and prove both path and roster membership."""
    (target / "cgroup.procs").write_text(str(pid))
    current = _my_cgroup_path()
    if current is None or current.resolve() != target.resolve():
        raise OSError(
            f"nested scheduler pid {pid} did not return to {target}; "
            f"current cgroup is {current}"
        )
    roster = (target / "cgroup.procs").read_text().split()
    if str(pid) not in roster:
        raise OSError(
            f"nested scheduler pid {pid} is absent from {target / 'cgroup.procs'}"
        )


def restore_nested_coordinator() -> bool:
    """Restore one in-process nested CLI invocation to its delegated parent supervisor.

    A command-line process normally exits immediately, but the Python API is intentionally
    callable repeatedly in one process (including the test runner). Leaving that caller in a
    completed invocation's tighter aggregate cgroup would leak limits and invalidate its original
    delegation. This LIFO restore is paired with :func:`_move_current_to_nested_supervisor` by the
    CLI's outer ``finally`` block.
    """
    if not _NESTED_COORDINATOR_STACK:
        return True
    run_root, return_supervisor, naming = _NESTED_COORDINATOR_STACK[-1]
    pid = os.getpid()
    try:
        _restore_pid_to_cgroup(return_supervisor, pid)
    except OSError as error:
        _warn(
            naming,
            f"could not restore nested scheduler to delegated parent {return_supervisor} "
            f"({error}); outer owner must clean up {run_root}",
        )
        # Keep the entry so the enclosing public CLI invocation can retry once and, if the
        # caller is still trapped, turn an otherwise-successful result into a containment error.
        return False
    _NESTED_COORDINATOR_STACK.pop()
    _remove_empty_cgroup_descendants(run_root)
    try:
        run_root.rmdir()
    except OSError as error:
        _warn(naming, f"could not remove completed nested scheduler cgroup {run_root} ({error})")
    return True


def nested_coordinator_depth() -> int:
    """Return the process-local nesting depth used to scope one public CLI invocation."""
    return len(_NESTED_COORDINATOR_STACK)


def restore_nested_coordinators_to(depth: int) -> bool:
    """Restore only placements created after a caller's captured ``depth`` token."""
    while len(_NESTED_COORDINATOR_STACK) > depth:
        if not restore_nested_coordinator():
            return False
    return True


class Cgroups:
    """Per-step child cgroups under the delegated outer scope — the concrete
    :class:`dagrun.protocols.CgroupManager` for a real Linux
    cgroup-v2 host.

    Lifecycle:
      * construct once (in the in-scope runner). :attr:`enabled` tells the caller
        whether per-step cgroups are usable; if not, it must use ``killpg``.
      * :meth:`prepare_command` wraps a step's shell command so its bash leader
        self-moves into its child cgroup BEFORE forking any grandchild.
      * :meth:`kill` SIGKILLs the step's whole subtree (setsid-proof).
      * :meth:`cleanup` removes the now-empty child cgroup dir (best-effort).

    Every best-effort cgroupfs write that would drop a requested cap emits a visible warning via
    :func:`_warn` instead of swallowing the ``OSError``.
    """

    def __init__(
        self,
        naming: ScopeNaming = DEFAULT_NAMING,
        *,
        delegated_root: Path | None = None,
    ) -> None:
        self._naming = naming
        self.enabled: bool = False
        self.root: Path | None = None  # the delegated scope cgroup root
        self._made: set[str] = set()
        self._delegated: set[str] = set()
        # A nested scheduler snapshots every visible memory.max from its validated parent step
        # through the cgroup namespace root before creating its private run root. Keep that
        # provenance independently of the stamp below: a failed local write does not make the
        # physical ancestor cap disappear, and an incomplete scan must keep profile rows unknown.
        self._delegated_ancestor_memory_max: int | None = None
        self._delegated_ancestor_memory_max_known = True
        self._delegated_ancestor_max_cpus: int | None = None
        self._delegated_ancestor_cpu_known = True
        self._nested_coordinator_depth: int | None = None
        # Uniform per-step ``pids.max`` cap applied to EVERY child cgroup, or None for no cap.
        # Set at runtime (never serialized in the DAG) so a caller like parallel-experiment-runner
        # can bound each worker's PID count — the one axis cpu.max/memory.max cannot contain (a
        # fork bomb exhausts PIDs, not CPU or RAM). See :meth:`set_worker_pids_max`.
        self.worker_pids_max: int | None = None
        # A top-level run discovers its systemd scope from the in-scope sentinel. A nested run
        # receives an already-empty step root from its parent and adopts that exact root only
        # after the CLI verified the live ``root/supervisor`` membership.
        if delegated_root is None:
            # A process somewhere below a delegated outer step still inherits the top-level
            # DAGRUN_IN_SCOPE sentinel. Treating its current leaf as a fresh scope root repeatedly
            # creates ``supervisor/supervisor/...`` and invalidates the delegation. Only the CLI's
            # validated ``from_delegated_root`` route may construct a manager in this context.
            if os.environ.get(DELEGATED_CGROUP_ENV) is not None:
                return
            if os.environ.get(naming.env_in_scope) != "1":
                return
        scope_cg = delegated_root if delegated_root is not None else _my_cgroup_path()
        if scope_cg is None or not scope_cg.is_dir():
            return
        try:
            controllers = (scope_cg / "cgroup.controllers").read_text().split()
        except OSError as exc:
            _warn(naming, f"outer scope cgroup.controllers unreadable ({exc}); "
                  "per-step containment disabled — falling back to process-group kill")
            return
        # Move EVERY process out of the scope root into the `supervisor/` child
        # cgroup so the root holds NO processes and may then enable controllers
        # for its children (cgroup-v2 "no internal processes" rule). Draining
        # only os.getpid() is not enough: sibling helpers already running in the
        # scope root would leave it populated and make the subtree_control write
        # below fail with EBUSY (silently no-op'ing every per-step cap). Drain the
        # whole root, repeatedly (moving a leader can reveal late-forked
        # children), best-effort per pid.
        sup = scope_cg / naming.supervisor_name
        try:
            sup.mkdir(exist_ok=True)
        except OSError as exc:
            _warn(naming, f"could not create supervisor cgroup {sup} ({exc}); "
                  "per-step containment disabled — falling back to process-group kill")
            return
        # Enable each controller independently so one unavailable controller cannot make an
        # atomic multi-controller write fail wholesale. Every write is guarded by a fresh bounded
        # drain and an EBUSY re-drain, closing the check/act gap around late scope members.
        for c in ("memory", "cpu", "pids"):
            if c in controllers:
                try:
                    _enable_controller(scope_cg, sup, c)
                except OSError as exc:
                    _warn(naming, f"could not delegate '{c}' controller to per-step cgroups "
                          f"after bounded drain/retry ({exc}); per-step containment disabled")
                    return
        self.root = scope_cg
        self.enabled = True

    @classmethod
    def from_delegated_root(
        cls,
        root: Path,
        naming: ScopeNaming = DEFAULT_NAMING,
        *,
        max_cpus: int | None = None,
        max_mem_bytes: int | None = None,
    ) -> "Cgroups":
        """Create a per-invocation scheduler root below a validated parent delegation.

        Several child schedulers may be launched concurrently by one outer command (the
        differential harness does exactly that). Giving all of them the same step-root namespace
        would alias equal inner tags and let one scheduler clean up another's cgroup. A live PID
        is unique within the parent subtree, so each invocation receives its own empty root.
        """
        run_root = root / f"nested-run-{os.getpid()}-{next(_NESTED_ROOT_COUNTER)}"
        try:
            inherited = _create_nested_run_root(
                root,
                run_root,
                naming=naming,
                requested_max_cpus=max_cpus,
                requested_max_mem_bytes=max_mem_bytes,
            )
        except OSError as error:
            _warn(naming, f"could not create nested scheduler cgroup {run_root} ({error})")
            return cls(naming, delegated_root=Path("/definitely/missing/dagrun-delegation"))
        manager = cls(naming, delegated_root=run_root)
        if manager.enabled:
            coordinator_depth = nested_coordinator_depth()
            try:
                _move_current_to_nested_supervisor(run_root, naming)
            except OSError as error:
                _warn(
                    naming,
                    f"could not place nested scheduler inside aggregate cgroup {run_root} "
                    f"({error}); per-step containment disabled",
                )
                manager.enabled = False
                manager.root = None
                if nested_coordinator_depth() > coordinator_depth:
                    manager._nested_coordinator_depth = coordinator_depth
            else:
                manager._nested_coordinator_depth = coordinator_depth
        manager._delegated_ancestor_memory_max = inherited.memory_max_bytes
        manager._delegated_ancestor_memory_max_known = inherited.memory_known
        manager._delegated_ancestor_max_cpus = inherited.max_cpus
        manager._delegated_ancestor_cpu_known = inherited.cpu_known
        return manager

    def close(self) -> bool:
        """Restore a nested caller placement owned by this manager, if any.

        The CLI also has an invocation-scoped fallback so every early return is covered. Direct
        library callers can use ``with Cgroups.from_delegated_root(...)`` or call ``close``.
        """
        depth = self._nested_coordinator_depth
        if depth is None:
            return True
        if nested_coordinator_depth() <= depth:
            self._nested_coordinator_depth = None
            return True
        if nested_coordinator_depth() != depth + 1:
            _warn(
                self._naming,
                "nested scheduler cgroup managers must be closed in LIFO order",
            )
            return False
        restored = restore_nested_coordinators_to(depth)
        if restored:
            self._nested_coordinator_depth = None
        return restored

    def __enter__(self) -> "Cgroups":
        return self

    def __exit__(self, *_exc: object) -> None:
        if not self.close():
            raise RuntimeError("could not restore nested scheduler cgroup membership")

    def __del__(self) -> None:
        # CPython releases ordinary managers promptly; the explicit context-manager/close API is
        # the deterministic route. Never let interpreter-shutdown ordering surface from __del__.
        try:
            self.close()
        except Exception:
            pass

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        """Wrap an ordinary step command with this run's cgroup membership and caps."""
        return self._prepare_command(tag, cmd, mem_max, cpu_count, delegated_children=False)

    def prepare_delegated_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> tuple[str, str | None]:
        """Prepare a step root that a child dagrun may safely subdivide.

        The command itself is moved to ``<step>/supervisor``. The step cgroup remains empty,
        owns the outer cap, and has controllers enabled for descendants. The returned path is
        exported by the scheduler only for the opted-in step.
        """
        prepared = self._prepare_command(
            tag, cmd, mem_max, cpu_count, delegated_children=True
        )
        root = (
            self.root / _sanitize(tag)
            if self.enabled and self.root is not None and tag in self._delegated
            else None
        )
        return prepared, str(root) if root is not None else None

    def _prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None,
        cpu_count: int | None,
        *,
        delegated_children: bool,
    ) -> str:
        """Wrap ``cmd`` so its bash leader joins the step's child cgroup FIRST
        (before forking grandchildren). No-op string-wrap when disabled.

        ``mem_max`` (bytes), if given, is the INNER per-step ``memory.max`` cap so
        a single runaway step is OOM-killed at its own characterized limit,
        leaving the rest of the run + the host alive. ``cpu_count``, if given, is
        the inner ``cpu.max`` cap — and a cpu-cap write that cannot be verified
        makes the returned command FAIL loudly (never silently run uncapped).

        No Silent Failure: a ``mem_max`` / swap / soft-cap write that fails (e.g.
        the ``memory`` controller was not delegated) emits a visible warning; the
        step still runs under the outer cap, but the degradation is never
        invisible."""
        if not self.enabled or self.root is None:
            return cmd
        encoded_tag = _sanitize(tag)
        if len(encoded_tag.encode("ascii")) > _MAX_CGROUP_COMPONENT_BYTES:
            return (
                "echo 'ERROR: encoded step tag exceeds the cgroup filename limit; "
                "refusing uncontained run' >&2\nexit 125\n"
            )
        child = self.root / encoded_tag
        try:
            child.mkdir(exist_ok=True)
            self._made.add(tag)
        except OSError as exc:
            _warn(self._naming, f"step {tag}: could not create child cgroup {child} "
                  f"({exc}); refusing to run without the requested per-step cgroup")
            return (
                "echo 'ERROR: step cgroup could not be created; refusing uncontained run' >&2\n"
                "exit 125\n"
            )
        try:
            # Every step is swapless, including uncharacterized ones without an
            # inner memory.max, so host-side swap policy never selects a step
            # cgroup as a kill target. Also clear any inherited soft cap.
            (child / "memory.swap.max").write_text("0")
            (child / "memory.high").write_text("max")
        except OSError as exc:
            _warn(self._naming, f"step {tag}: could not disable swap / clear soft cap "
                  f"({exc}); memory controller may not be delegated — outer cap still applies")
        # Kill the WHOLE step cgroup as a unit on OOM (`memory.oom.group=1`). Without it the
        # kernel picks one victim process inside whichever cgroup it OOMs: a capped step is left
        # half-dead (its build leader survives a killed cc1plus and fails confusingly), and — when
        # a runaway escalates past its own cap to a shared ancestor — the victim is chosen by
        # badness across ALL steps, so the kill can land on an INNOCENT NEIGHBOUR and be attributed
        # to the wrong PR. Set on every per-step cgroup (best-effort: a kernel without oom.group
        # must not lose the swap/mem caps above), so the breach lands on the offending step whole.
        try:
            (child / "memory.oom.group").write_text("1")
        except OSError as exc:
            _warn(self._naming, f"step {tag}: could not set memory.oom.group ({exc}); an OOM may "
                  "kill a single process instead of the whole step (mis-attributed blast radius)")
        # `memory_max` is one of the guards the `capabilities` manifest advertises, so the
        # decision to apply it is read from the same registry that publishes it: turning the
        # advertisement off turns the write off, and vice versa. See capabilities.py. This
        # method only ever runs for a real, enabled manager, so the lane is CONTAINED by
        # construction -- there is no cgroup child to write to on the other one.
        if mem_max and is_enforced("memory_max", Lane.CONTAINED):
            try:
                (child / "memory.max").write_text(str(int(mem_max)))
            except OSError as exc:
                _warn(self._naming, f"step {tag}: could not apply inner memory cap "
                      f"memory.max={mem_max} ({exc}); step runs under the outer cap only")
        if cpu_count:
            period = 100_000
            expected = f"{int(cpu_count) * period} {period}"
            try:
                cpu_max = child / "cpu.max"
                cpu_max.write_text(expected)
                applied = cpu_max.read_text().strip()
                if applied != expected:
                    return (f"echo 'ERROR: step {tag} cpu.max mismatch: expected {expected}, "
                            f"got {applied}' >&2\nexit 1\n")
            except OSError as exc:
                return (f"echo 'ERROR: step {tag} cpu.max could not be applied: {exc}' >&2\n"
                        "exit 1\n")
        # A PID cap contains a fork bomb (fork() returns EAGAIN past the cap) rather than
        # killing it, and the breach surfaces via pids_events. The manifest advertises
        # `pids_guard` as NOT enforced -- nothing sets `worker_pids_max` and the Rust engine has
        # no pids plumbing at all -- so the write is gated on that same declaration instead of
        # being reachable behind it. A caller that sets the limit today gets what the manifest
        # promises: nothing. Flipping the registry flag turns both the claim and the write on.
        if self.worker_pids_max is not None and is_enforced("pids_guard", Lane.CONTAINED):
            try:
                (child / "pids.max").write_text(str(int(self.worker_pids_max)))
            except OSError as exc:
                _warn(
                    self._naming,
                    f"step {tag}: could not apply inner pids cap "
                    f"pids.max={self.worker_pids_max} ({exc}); pids controller may not be "
                    "delegated — step runs under the outer cap only",
                )

        process_cgroup = child
        if delegated_children:
            supervisor = child / self._naming.supervisor_name
            try:
                supervisor.mkdir(exist_ok=True)
                controllers = (child / "cgroup.controllers").read_text().split()
                for controller in ("memory", "cpu", "pids"):
                    if controller in controllers:
                        _enable_controller(child, supervisor, controller)
            except OSError as exc:
                _warn(
                    self._naming,
                    f"step {tag}: could not prepare delegated child cgroup {child} ({exc}); "
                    "refusing nested execution",
                )
                return (
                    "echo 'ERROR: delegated child cgroup could not be prepared; "
                    "refusing nested execution' >&2\nexit 125\n"
                )
            self._delegated.add(tag)
            process_cgroup = supervisor

        # Carry the build ``-j`` WITH the caps just written. cargo (and the NUM_JOBS it
        # exports to build scripts) auto-detects parallelism from the effective CPU quota;
        # an UNPINNED step (cpu_count None -> no per-step cpu.max) inherits the wide scope
        # quota and computes NUM_JOBS=<all-granted-cores> (observed 284), OOM-racing the
        # linker (<repo>#1584 build.dbi_release, 8.0 GiB cap, oom_kill=2). Derive the cap
        # here, where the quota is granted, from the step's cores+mem if pinned else the
        # SCOPE's effective cpu.max/memory.max, so even an unpinned step is bounded. Only
        # CARGO_BUILD_JOBS is set (never MAKEFLAGS): a global make -j could parallelize a
        # determinism-sensitive target (cf. make -jN nondeterminism #1157). An explicit
        # ``cargo -j`` in the step command still overrides this env floor.
        #
        # An operator's OWN width, resolved once in the outermost process, is not refined: they
        # sized their caps against that pool. Only the derivation refines downward, and that is
        # the leg `test_unpinned_step_is_bounded_not_284` pins.
        core_limits = [
            limit
            for limit in (
                cpu_count,
                _cpu_max_cores(_read_cgroup_value(self.root, "cpu.max")),
                self._delegated_ancestor_max_cpus,
            )
            if limit is not None and limit > 0
        ]
        if not self._delegated_ancestor_cpu_known:
            # The public scheduler path rejects an unreadable inherited envelope. Library callers
            # still get the narrowest representable integer width rather than host-width fan-out.
            core_limits.append(1)
        memory_limits = [
            limit
            for limit in (
                mem_max,
                _memory_max_bytes(_read_cgroup_value(self.root, "memory.max")),
                self._delegated_ancestor_memory_max,
            )
            if limit is not None and limit >= 0
        ]
        eff_cores = min(core_limits) if core_limits else None
        eff_mem = min(memory_limits) if memory_limits else None
        jobs = select_build_jobs(eff_cores, eff_mem).jobs
        procs = process_cgroup / "cgroup.procs"
        # $$ is the bash leader's own pid. Writing it migrates the leader; every
        # subsequently-forked child/grandchild inherits this cgroup at fork.
        # A cap on an empty child is not containment. The migration is therefore fail-closed: if
        # the shell cannot join the exact cgroup whose limits were verified above, do not execute
        # the user command and do not claim those limits governed it.
        quoted_procs = shlex.quote(str(procs))
        return (
            f"if ! printf '%s\\n' \"$$\" 2>/dev/null > {quoted_procs}; then\n"
            "  echo 'ERROR: step could not join its dagrun cgroup; refusing uncontained run' >&2\n"
            "  exit 125\n"
            "fi\n"
            f"export {BUILD_JOBS_ENV}={jobs}\n{cmd}"
        )

    def set_worker_pids_max(self, limit: int | None) -> None:
        """Set the uniform per-step ``pids.max`` cap applied to every subsequently-prepared
        child cgroup (``None`` disables it). Runtime-only; never part of the serialized DAG."""
        self.worker_pids_max = limit

    def pids_events(self, tag: str) -> int:
        """``pids.events`` ``max`` counter inside the step's cgroup — the number of times a
        ``fork``/``clone`` was denied because the step hit its inner ``pids.max``. ``> 0`` is
        the actionable fork-bomb / PID-exhaustion signal (distinct from an OOM or a wall
        timeout). 0 if absent/unreadable. Read BEFORE :meth:`cleanup`."""
        if not self.enabled or self.root is None or tag not in self._made:
            return 0
        try:
            events = (self.root / _sanitize(tag) / "pids.events").read_text()
            for line in events.splitlines():
                if line.startswith("max "):
                    return int(line.split()[1])
        except (OSError, ValueError, IndexError):
            pass
        return 0

    def memory_events(self, tag: str) -> Mapping[str, int] | None:
        """The step cgroup's owned memory events as counter -> value.

        These need no baseline subtraction to be per-step deltas: the child cgroup is
        created by :meth:`prepare_command` for this step and removed by :meth:`cleanup`
        after it, so every counter starts at zero. ``None`` if absent/unreadable. Read
        BEFORE :meth:`cleanup`.

        An ordinary step has no child cgroups, so ``memory.events`` is its owned view. A
        ``delegated_children`` step deliberately contains a nested scheduler's cgroups;
        there we read ``memory.events.local``. The hierarchical file would also count an
        inner step's independently handled OOM and falsely fail the otherwise-correct outer
        scheduler. The local file still counts an OOM charged at the delegated root's own
        ``memory.max`` even when the selected victim lives in a descendant, so the outer cap
        remains an actionable backstop.

        A line that does not parse as ``<name> <integer>`` is SKIPPED, not fatal: the kernel is
        free to add counters, and discarding every counter in the file because one line was new
        or blank would blank all five CSV cells and silently drop the recorded OOM count. The
        sibling implementation of this runner skips per line too, and two builds must not
        disagree about the same file."""
        if not self.enabled or self.root is None or tag not in self._made:
            return None
        filename = "memory.events.local" if tag in self._delegated else "memory.events"
        try:
            lines = (self.root / _sanitize(tag) / filename).read_text().splitlines()
        except OSError:
            return None
        events: dict[str, int] = {}
        for line in lines:
            fields = line.split()
            if len(fields) < 2:
                continue
            try:
                events[fields[0]] = int(fields[1])
            except ValueError:
                continue
        return events

    def applied_memory_max(self, tag: str) -> str | None:
        """The TIGHTEST ``memory.max`` in force over the step, read back from cgroupfs: a
        decimal byte count, or the literal ``"max"`` when no level the runner can see bounds it.

        The step's OWN ``memory.max`` is not on its own the ceiling it ran under. cgroup-v2
        limits are hierarchical, and this runner always installs an outer cap on the delegated
        scope, so a step that got no inner cap (an uncharacterised step has no
        ``rss_baseline_bytes``, so :func:`~dagrun.sizing.step_mem_cap` returns
        ``None`` and no child ``memory.max`` is written) reads ``"max"`` at its own level while
        the scope's ceiling is what actually held it. Reporting the child value alone would say
        "known unbounded" for exactly the population being profiled to learn its footprint, and
        the child's ``memory.events`` cannot correct the record either: those counters do not
        count an ANCESTOR's limit events. So the minimum of the step's own cap and the scope's
        is reported, because that is the number ``peak_bytes`` is interpretable against.

        Read back rather than echoed from the ``mem_max`` argument to :meth:`prepare_command`,
        because the case that matters — a requested cap the kernel never accepted — is exactly
        the case where the two disagree, and only the accepted value explains the measured peak.
        ``None`` (unknown) is deliberately distinct from ``"max"`` (known unbounded).

        A nested scheduler additionally snapshots every readable ``memory.max`` from its
        validated parent delegation through the cgroup namespace root and stamps the finite
        minimum onto its own run root. Limits above a top-level run scope or outside the
        process's cgroup namespace remain outside that run's recorded view. Read BEFORE
        :meth:`cleanup`."""
        if not self.enabled or self.root is None or tag not in self._made:
            return None
        if not self._delegated_ancestor_memory_max_known:
            return None
        try:
            own = (self.root / _sanitize(tag) / "memory.max").read_text().strip()
        except OSError:
            return None
        if not own:
            return None
        own_bytes = _memory_max_bytes(own)
        limits = [
            limit
            for limit in (
                own_bytes,
                _memory_max_bytes(_read_cgroup_value(self.root, "memory.max")),
                self._delegated_ancestor_memory_max,
            )
            if limit is not None
        ]
        return str(min(limits)) if limits else own

    def oom_kills(self, tag: str) -> int:
        """OOM-kill count in the owned view selected by :meth:`memory_events`.

        ``> 0`` means the step's own limit (or a delegated root's boundary limit) killed a
        process — one actionable-OOM signal. It does not cover root-local ``oom`` whose victim is
        accounted in a descendant; the scheduler classifies the complete event map. Reclaim at
        the cap has ``max > 0`` while every OOM counter stays zero and the step exits cleanly.
        Returns 0 if absent/unreadable. Read BEFORE :meth:`cleanup`."""
        events = self.memory_events(tag)
        return 0 if events is None else events.get("oom_kill", 0)

    def peak_bytes(self, tag: str) -> int | None:
        """Peak RSS (bytes) of the step's cgroup (``memory.peak``), for baseline
        characterization. None if unreadable. Read BEFORE :meth:`cleanup`."""
        if not self.enabled or self.root is None or tag not in self._made:
            return None
        try:
            return int((self.root / _sanitize(tag) / "memory.peak").read_text().strip())
        except (OSError, ValueError):
            return None

    def thread_count(self, tag: str) -> int | None:
        """Current descendant thread count from the step's ``cgroup.threads``."""
        if not self.enabled or self.root is None or tag not in self._made:
            return None
        try:
            threads = (self.root / _sanitize(tag) / "cgroup.threads").read_text()
            return len(threads.splitlines())
        except OSError:
            return None

    def cpu_stats(self, tag: str) -> Mapping[str, int] | None:
        """Current per-step cgroup-v2 CPU counters from ``cpu.stat``."""
        if not self.enabled or self.root is None or tag not in self._made:
            return None
        try:
            lines = (self.root / _sanitize(tag) / "cpu.stat").read_text().splitlines()
            return {
                key: int(value)
                for key, value in (line.split(maxsplit=1) for line in lines)
            }
        except (OSError, ValueError):
            return None

    def cpu_pressure(self, tag: str) -> Mapping[str, float] | None:
        """Per-step CPU pressure-stall averages (``cpu.pressure`` ``some`` line)."""
        if not self.enabled or self.root is None or tag not in self._made:
            return None
        try:
            lines = (self.root / _sanitize(tag) / "cpu.pressure").read_text().splitlines()
            some = next(line for line in lines if line.startswith("some "))
            values = dict(item.split("=", 1) for item in some.split()[1:])
            return {name: float(values[name]) for name in ("avg10", "avg60")}
        except (OSError, StopIteration, ValueError, KeyError):
            return None

    def kill(self, tag: str) -> bool:
        """SIGKILL the step's entire cgroup subtree (setsid escapees included).
        Returns True if the kill file was written. Never raises; a write failure
        emits a warning and returns False so the caller falls back to
        process-group kill."""
        if not self.enabled or self.root is None or tag not in self._made:
            return False
        killf = self.root / _sanitize(tag) / "cgroup.kill"
        try:
            killf.write_text("1")
            return True
        except OSError as exc:
            _warn(self._naming, f"step {tag}: cgroup.kill write failed ({exc}); "
                  "falling back to process-group kill for this step")
            return False

    def cleanup(self, tag: str) -> None:
        """Remove the step's (now-empty) child cgroup directory. Best-effort:
        ``rmdir`` fails with EBUSY if procs remain, which is FINE (expected) — the
        outer-scope stop flushes it at end of run, so this is NOT warned on."""
        if not self.enabled or self.root is None or tag not in self._made:
            return
        child = self.root / _sanitize(tag)
        if tag in self._delegated:
            _remove_empty_cgroup_descendants(child)
        try:
            child.rmdir()
        except OSError:
            pass
        self._made.discard(tag)
        self._delegated.discard(tag)

    def kill_all_remaining(self) -> int:
        """NORMAL-EXIT backstop: ``cgroup.kill`` + ``rmdir`` EVERY step child
        cgroup we ever created that still exists (catches a setsid orphan a step
        left behind). A manager never removes its own root: one CLI command can reuse the same
        manager for several scheduler passes (notably ``sweep``), and the outer scheduler owns
        and removes the complete delegated subtree when this process exits. Returns the count of
        step cgroups that still existed."""
        if not self.enabled or self.root is None:
            return 0
        n = 0
        # Scan the real directory, not just self._made: a step whose cleanup ran
        # may be gone, while a crashed step's dir lingers.
        try:
            children = [p for p in self.root.iterdir()
                        if p.is_dir() and p.name.startswith(_STEP_PREFIX)]
        except OSError:
            children = []
        for child in children:
            n += 1
            try:
                (child / "cgroup.kill").write_text("1")
            except OSError as exc:
                _warn(self._naming, f"backstop: cgroup.kill on {child.name} failed ({exc}); "
                      "a leftover orphan may survive the run")
            _remove_empty_cgroup_descendants(child)
            try:
                child.rmdir()
            except OSError:
                pass  # EBUSY: procs still dying; outer-scope stop will flush it
        return n


class NoopCgroups:
    """Advisory-only stand-in for a non-Linux / non-systemd / non-delegated host.

    :attr:`enabled` is always ``False``. Every method is a safe no-op so a caller
    can still use the full DAG/scheduler/logging core — teardown falls back to
    process-group kill and no per-step metrics are available. Structurally
    satisfies :class:`dagrun.protocols.CgroupManager`."""

    enabled: bool = False

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        """Return ``cmd`` unchanged because containment is disabled."""
        return cmd

    def prepare_delegated_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> tuple[str, str | None]:
        """Return the command for the explicit unboxed-delegation fallback.

        The scheduler marks only this opted-in step with ``DAGRUN_DELEGATED_UNBOXED=1``. That
        marker authorizes nesting but does not claim cgroup containment.
        """
        return cmd, None

    def kill(self, tag: str) -> bool:
        """Report that no cgroup subtree was available to kill."""
        return False

    def cleanup(self, tag: str) -> None:
        """Perform no cleanup because no child cgroup was created."""
        return None

    def set_worker_pids_max(self, limit: int | None) -> None:
        """Ignore the requested PID cap because containment is disabled."""
        return None

    def pids_events(self, tag: str) -> int:
        """Return zero because no cgroup PID counter is available."""
        return 0

    def oom_kills(self, tag: str) -> int:
        """Return zero because no cgroup OOM counter is available."""
        return 0

    def memory_events(self, tag: str) -> Mapping[str, int] | None:
        """Return ``None`` because no cgroup memory event counters are available."""
        return None

    def applied_memory_max(self, tag: str) -> str | None:
        """Return ``None`` (cap UNKNOWN) because no cgroup cap was applied or read.

        Not ``"max"``: this manager did not observe an unbounded step, it observed
        nothing, and a consumer must be able to tell those apart before deciding whether
        a peak is censored."""
        return None

    def peak_bytes(self, tag: str) -> int | None:
        """Return ``None`` because no cgroup memory peak is available."""
        return None

    def cpu_stats(self, tag: str) -> Mapping[str, int] | None:
        """Return ``None`` because no cgroup CPU counters are available."""
        return None

    def cpu_pressure(self, tag: str) -> Mapping[str, float] | None:
        """Return ``None`` because no cgroup pressure data is available."""
        return None

    def thread_count(self, tag: str) -> int | None:
        """Return ``None`` because no cgroup thread count is available."""
        return None

    def kill_all_remaining(self) -> int:
        """Return zero because this manager owns no child cgroups."""
        return 0


if TYPE_CHECKING:
    # Compile-time guarantee that both concrete managers structurally satisfy the
    # CgroupManager protocol (matched signatures — no adapter shim needed).
    from dagrun.protocols import CgroupManager

    def _assert_protocol_conformance() -> None:
        _real: CgroupManager = Cgroups()
        _noop: CgroupManager = NoopCgroups()
