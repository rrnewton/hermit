"""Best-effort ambient host-load measurements.

The module reads Linux ``/proc`` counters, captures pressure and co-tenant activity,
and classifies a run as quiet, moderate, or busy. Missing or malformed measurements
produce explicit optional values or documented defaults; the readers never modify
host state.
"""

from __future__ import annotations

import os
from collections.abc import Collection
from dataclasses import dataclass
from pathlib import Path
from typing import Final, Literal

__all__ = [
    "PsiReading",
    "AmbientSnapshot",
    "AmbientBucket",
    "PROC_STAT_PATH",
    "PROC_CPU_PRESSURE_PATH",
    "PROC_MEMORY_PRESSURE_PATH",
    "PROC_IO_PRESSURE_PATH",
    "read_loadavg",
    "host_busy_jiffies",
    "read_pressure",
    "count_external_build_processes",
    "capture_ambient_snapshot",
    "attribute_external_cores",
    "ambient_bucket",
]

# Canonical /proc locations, named so callers/tests can point at fixtures.
PROC_STAT_PATH: Final = Path("/proc/stat")
PROC_CPU_PRESSURE_PATH: Final = Path("/proc/pressure/cpu")
PROC_MEMORY_PRESSURE_PATH: Final = Path("/proc/pressure/memory")
PROC_IO_PRESSURE_PATH: Final = Path("/proc/pressure/io")

# ``ambient_bucket`` cut-offs. PRESERVE THESE EXACTLY -- a sibling implementation in
# another language must produce the identical bucket for the identical inputs.
_BUSY_EXTERNAL_CORES: Final = 2.0
_BUSY_PSI_AVG10: Final = 20.0
_BUSY_CO_TENANTS: Final = 8
_QUIET_EXTERNAL_CORES: Final = 0.5
_QUIET_PSI_AVG10: Final = 5.0
_QUIET_CO_TENANTS: Final = 2

# Fallback USER_HZ when ``os.sysconf("SC_CLK_TCK")`` is unavailable, matching the
# originating implementation's ``os.sysconf(...) or 100``.
_DEFAULT_CLK_TCK: Final = 100

#: The three-level ambient-load verdict, by string value (the value is the parity key).
AmbientBucket = Literal["quiet", "moderate", "busy"]


@dataclass(frozen=True)
class PsiReading:
    """One Pressure-Stall-Information (PSI) ``some`` line: the ``avg10`` / ``avg60``
    fractions of time at least one task was stalled on a resource.

    ``None`` rather than an empty reading marks an absent or unparseable
    ``/proc/pressure/*`` file, allowing callers to distinguish zero pressure from an
    unavailable kernel metric.
    """

    avg10: float
    avg60: float


@dataclass(frozen=True)
class AmbientSnapshot:
    """One instant of host-wide load used to attribute contention around a step."""

    #: System-wide non-idle CPU jiffies (``/proc/stat`` ``cpu`` line), or ``None`` when
    #: unreadable. Only meaningful as a delta between two snapshots.
    busy_jiffies: int | None
    #: 1- and 5-minute load averages (``os.getloadavg``).
    load1: float
    load5: float
    #: Host CPU / memory / IO pressure, each ``None`` when that PSI file is absent.
    cpu_psi: PsiReading | None
    memory_psi: PsiReading | None
    io_psi: PsiReading | None
    #: Count of matching build processes running OUTSIDE this run's cgroup scope.
    co_tenants: int


def read_loadavg() -> tuple[float, float, float]:
    """Return the 1/5/15-minute load averages via :func:`os.getloadavg`.

    Raises ``OSError`` on platforms that cannot report load averages.
    """
    load1, load5, load15 = os.getloadavg()
    return load1, load5, load15


def host_busy_jiffies(stat_path: Path = PROC_STAT_PATH) -> int | None:
    """Sum the system-wide non-idle CPU jiffies from ``/proc/stat``'s first ``cpu`` line.

    ``busy = total - idle - iowait`` (idle is field 3, iowait field 4 after the ``cpu``
    label). Returns ``None`` on any read or parse error rather than fabricating a count.
    """
    try:
        parts = stat_path.read_text().splitlines()[0].split()
        values = [int(value) for value in parts[1:]]
        return sum(values) - values[3] - (values[4] if len(values) > 4 else 0)
    except (OSError, ValueError, IndexError):
        return None


def read_pressure(path: Path) -> PsiReading | None:
    """Parse the ``some`` line of a PSI file (``/proc/pressure/{cpu,memory,io}``).

    Returns a :class:`PsiReading` with the ``avg10`` / ``avg60`` fractions, or ``None``
    when the file is missing, has no ``some`` line, or is malformed.
    """
    try:
        some = next(line for line in path.read_text().splitlines() if line.startswith("some "))
        values = dict(item.split("=", 1) for item in some.split()[1:])
        return PsiReading(avg10=float(values["avg10"]), avg60=float(values["avg60"]))
    except (OSError, StopIteration, ValueError, KeyError):
        return None


def count_external_build_processes(
    build_process_names: Collection[str],
    *,
    scope_marker: str | None = None,
) -> int:
    """Count named processes running outside this run's cgroup scope.

    * ``build_process_names`` -- exact ``/proc/<pid>/comm`` values to count. Kernel
      truncates ``comm`` to 15 bytes, so pass already-truncated names for long binaries.
    * ``scope_marker`` -- a substring identifying THIS run's cgroup. A process is counted
      only when the marker does NOT appear in its ``/proc/<pid>/cgroup`` (i.e. it is a
      genuine co-tenant, not one of our own children). ``None`` / empty counts every
      matching process, ours included.

    Per-process read errors (races on exiting pids, permission) are skipped silently by
    design: they are transient and cannot be attributed. The aggregate count is always
    returned.
    """
    names = set(build_process_names)
    if not names:
        # No build-process names to match => nothing to count. Return early so a caller that only
        # wants the load/PSI parts of a snapshot (e.g. the generic runner, which has no project
        # build-process names) does not pay for a full /proc scan that could only ever return 0.
        return 0
    marker = scope_marker or ""
    count = 0
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            comm = (entry / "comm").read_text().strip()
            cgroup = (entry / "cgroup").read_text()
        except OSError:
            continue
        if comm in names and (not marker or marker not in cgroup):
            count += 1
    return count


def capture_ambient_snapshot(
    build_process_names: Collection[str],
    *,
    scope_marker: str | None = None,
) -> AmbientSnapshot:
    """Take one :class:`AmbientSnapshot` of current host load.

    Reads load averages, busy jiffies, the three host PSI files, and the co-tenant build count. The
    ``build_process_names`` / ``scope_marker`` arguments are forwarded verbatim to
    :func:`count_external_build_processes`.
    """
    load1, load5, _ = read_loadavg()
    return AmbientSnapshot(
        busy_jiffies=host_busy_jiffies(),
        load1=load1,
        load5=load5,
        cpu_psi=read_pressure(PROC_CPU_PRESSURE_PATH),
        memory_psi=read_pressure(PROC_MEMORY_PRESSURE_PATH),
        io_psi=read_pressure(PROC_IO_PRESSURE_PATH),
        co_tenants=count_external_build_processes(build_process_names, scope_marker=scope_marker),
    )


def _clk_tck() -> int:
    """USER_HZ (jiffies per second), falling back to 100 when ``sysconf`` cannot report."""
    try:
        return os.sysconf("SC_CLK_TCK") or _DEFAULT_CLK_TCK
    except (ValueError, OSError):
        return _DEFAULT_CLK_TCK


def attribute_external_cores(
    *,
    busy_jiffies_start: int | None,
    busy_jiffies_end: int | None,
    own_cpu_usec: int,
    elapsed_s: float,
) -> float:
    """Estimate how many CPU cores OTHER tenants burned during a step window.

    * ``host_busy_s`` -- system-wide non-idle CPU seconds over the window, from the
      busy-jiffies delta divided by USER_HZ. ``0`` if either endpoint is missing.
    * ``external_cpu_s = max(0, host_busy_s - own_cpu_usec/1e6)`` -- host CPU not
      attributable to this run's own cgroup.
    * ``external_cores = external_cpu_s / elapsed_s`` -- that CPU expressed as cores.

    Returns ``0.0`` for a non-positive ``elapsed_s`` because there is no window to divide by.
    """
    if busy_jiffies_start is not None and busy_jiffies_end is not None:
        host_busy_s = (busy_jiffies_end - busy_jiffies_start) / _clk_tck()
    else:
        host_busy_s = 0.0
    external_cpu_s = max(0.0, host_busy_s - own_cpu_usec / 1_000_000)
    if elapsed_s <= 0.0:
        return 0.0
    return external_cpu_s / elapsed_s


def ambient_bucket(external_cores: float, snapshot: AmbientSnapshot) -> AmbientBucket:
    """Classify host load as ``"quiet"`` / ``"moderate"`` / ``"busy"``.

    * ``busy``  -- external cores > 2.0, OR any host PSI ``avg10`` >= 20, OR co-tenants >= 8.
    * ``quiet`` -- external cores < 0.5, AND max PSI ``avg10`` < 5, AND co-tenants <= 2.
    * ``moderate`` -- everything in between.

    A missing PSI reading contributes ``avg10`` 0.0.
    """
    max_avg10 = max(
        (psi.avg10 for psi in (snapshot.cpu_psi, snapshot.memory_psi, snapshot.io_psi) if psi is not None),
        default=0.0,
    )
    co_tenants = snapshot.co_tenants
    if (
        external_cores > _BUSY_EXTERNAL_CORES
        or max_avg10 >= _BUSY_PSI_AVG10
        or co_tenants >= _BUSY_CO_TENANTS
    ):
        return "busy"
    if (
        external_cores < _QUIET_EXTERNAL_CORES
        and max_avg10 < _QUIET_PSI_AVG10
        and co_tenants <= _QUIET_CO_TENANTS
    ):
        return "quiet"
    return "moderate"
