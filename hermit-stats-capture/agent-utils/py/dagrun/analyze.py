"""Library helpers for summarizing per-step profile rows under comparable load.

This module is an importable compatibility surface, not a command-line application. Use the
package's ``summary`` command family to build and consume portable profile-summary artifacts.
"""

from __future__ import annotations

import csv
import statistics
import sys
from collections import defaultdict
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path

__all__ = ["summarize"]

#: Step identity (``group.job``).
COL_STEP = "step"
#: Inner command parallelism width.
COL_INNER_JOBS = "inner_jobs"
#: Ambient load bucket used to isolate comparable samples.
COL_AMBIENT = "ambient_bucket"
#: Measured wall-clock seconds for the step. This MUST match the column
#: :data:`dagrun.perflog.STEP_PROFILE_COLUMNS` actually writes. It read ``duration_s`` until
#: 2026-08-09, a column the store has never written, so every group was dropped as
#: unparseable: executed against the real 52-step store this module returned ZERO rows. It
#: failed loudly, which is why it went unnoticed -- nothing ran it.
COL_DURATION_S = "elapsed_s"
#: Measured effective cores kept busy by the step.
COL_EFFECTIVE_CORES = "effective_cores"
#: Measured cgroup memory high-water mark in bytes. This MUST match the writer's ``peak_bytes``
#: column; the older ``memory_peak_bytes`` spelling never existed in the profile schema and made
#: every analysis summary silently omit memory.
COL_MEMORY_PEAK_BYTES = "peak_bytes"

#: Sentinel ambient value that disables filtering.
AMBIENT_ALL = "all"
#: Default bucket containing samples from an otherwise-idle host.
AMBIENT_QUIET = "quiet"


def _numeric_column(rows: Sequence[Mapping[str, str]], field: str) -> list[float]:
    """Parse one numeric column, skipping missing or empty cells."""
    return [float(row[field]) for row in rows if row.get(field)]


def summarize(
    paths: Iterable[Path], ambient: str = AMBIENT_QUIET
) -> list[dict[str, object]]:
    """Collapse profile CSVs into one scaling row per ``(step, inner_jobs)``.

    Samples outside ``ambient`` are ignored unless ``ambient`` is ``"all"``. Each result contains
    the group identity, matching sample count, median duration, median effective cores, and maximum
    peak memory. A group with no parseable duration is omitted with a visible stderr diagnostic.
    """
    groups: dict[tuple[str, str], list[Mapping[str, str]]] = defaultdict(list)
    for path in paths:
        with path.open(newline="") as source:
            for row in csv.DictReader(source):
                if ambient != AMBIENT_ALL and row.get(COL_AMBIENT) != ambient:
                    continue
                groups[(row[COL_STEP], row[COL_INNER_JOBS])].append(row)

    result: list[dict[str, object]] = []
    for (step, inner_jobs), rows in sorted(groups.items()):
        durations = _numeric_column(rows, COL_DURATION_S)
        if not durations:
            sys.stderr.write(
                f"[analyze] {step} J={inner_jobs}: {len(rows)} matching sample(s) but no "
                f"parseable {COL_DURATION_S}; dropping from the summary\n"
            )
            continue
        memory = _numeric_column(rows, COL_MEMORY_PEAK_BYTES)
        cores = _numeric_column(rows, COL_EFFECTIVE_CORES)
        result.append(
            {
                "step": step,
                "inner_jobs": inner_jobs,
                "samples": len(rows),
                "duration_median_s": round(statistics.median(durations), 3),
                "effective_cores_median": round(statistics.median(cores), 3),
                "memory_peak_max_bytes": int(max(memory)) if memory else "",
            }
        )
    return result
