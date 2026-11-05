"""Bounded retention for the run spool: delete completed runs older than the retention window.

Two deliberate choices.

**Pruned ON WRITE, not on a timer.** A scheduled job that silently stops running is indistinguishable
from one that runs and finds nothing to do, and that inert-guard failure is exactly the class this
repository keeps rediscovering. Pruning as a side effect of creating a new run means retention can
only lapse if the tool itself stops being used -- in which case nothing is accumulating either.

**Scoped by construction, not by care.** This module DELETES DIRECTORIES, so the scope is enforced
rather than trusted: only entries whose real path's parent is exactly the resolved runs root are
considered, symlinks are skipped outright rather than followed, and the runs root itself is never
removed. Nothing above the capture root is reachable from here even if a caller passes a hostile
spool path.

The audit log is deliberately NOT pruned. It is the best-effort operational record of attempts to
cross the sandbox boundary, it is small, and an evidence trail that quietly deletes itself would
be misleading.
"""

from __future__ import annotations

import math
import os
import shutil
import stat
import time
from collections.abc import Callable
from dataclasses import dataclass

from herdr_run.textfmt import parse_exit_marker

__all__ = [
    "MAX_RETENTION_DAYS",
    "RETENTION_DAYS",
    "PruneResult",
    "prune_runs",
    "runs_root",
]

#: How long a run's captured output is kept. Four days spans a long weekend, so a failure on Friday
#: is still inspectable on Monday.
RETENTION_DAYS = 4

#: Largest accepted retention window. A shared finite bound keeps configuration behavior identical
#: across implementations and prevents absurd integers from overflowing wall-clock arithmetic.
MAX_RETENTION_DAYS = 365_000

_SECONDS_PER_DAY = 86_400


@dataclass(frozen=True)
class PruneResult:
    """What a prune pass did, so the caller can log or assert on it rather than infer."""

    removed: tuple[str, ...]
    kept: int
    skipped: tuple[str, ...]

    @property
    def removed_count(self) -> int:
        """How many run directories this pass removed."""
        return len(self.removed)


def runs_root(spool_dir: str, project_root: str) -> str:
    """Absolute path of the directory holding per-run spool directories."""
    root = spool_dir if os.path.isabs(spool_dir) else os.path.join(project_root, spool_dir)
    return os.path.join(root, "runs")


def prune_runs(
    root: str,
    *,
    retention_days: int = RETENTION_DAYS,
    now: Callable[[], float] = time.time,
) -> PruneResult:
    """Remove completed runs whose ``exit_code`` mtime is older than the retention window.

    Never raises: retention is housekeeping, and a failure to tidy must not fail the run that
    triggered it. Returns what happened so a caller can assert on both halves -- what was removed
    AND what was kept, because a prune that removes everything also passes a removal test. A run
    without a valid regular ``exit_code`` is still active or incomplete and is always retained.
    """
    removed: list[str] = []
    skipped: list[str] = []
    kept = 0

    absolute_root = os.path.abspath(root)
    if (
        isinstance(retention_days, bool)
        or not isinstance(retention_days, int)
        or not 0 <= retention_days <= MAX_RETENTION_DAYS
    ):
        return PruneResult((), 0, (absolute_root,))
    try:
        # Reject a symlink in the runs root OR any existing ancestor. Following a configured root
        # before applying containment would redefine the deletion boundary to an attacker-chosen
        # directory.
        if os.path.realpath(absolute_root) != absolute_root:
            return PruneResult((), 0, (absolute_root,))
    except (OSError, ValueError):
        return PruneResult((), 0, ())
    if not os.path.isdir(absolute_root):
        return PruneResult((), 0, ())

    try:
        current_time = now()
        cutoff = current_time - retention_days * _SECONDS_PER_DAY
        if not math.isfinite(current_time) or not math.isfinite(cutoff):
            return PruneResult((), 0, (absolute_root,))
    except (OverflowError, TypeError, ValueError):
        return PruneResult((), 0, (absolute_root,))

    try:
        # By raw bytes, as the native edition sorts: a name that is not UTF-8 is held as surrogate
        # escapes here, and code-point order would place it differently.
        entries = sorted(os.listdir(absolute_root), key=os.fsencode)
    except OSError:
        return PruneResult((), 0, ())

    for name in entries:
        path = os.path.join(absolute_root, name)

        # A symlink is never followed and never removed: following one would let a link planted in
        # the spool reach arbitrary paths, which is the whole risk this module has to not have.
        if os.path.islink(path):
            skipped.append(path)
            continue
        if not os.path.isdir(path):
            skipped.append(path)
            continue

        # Containment, checked rather than assumed: lexical and canonical parents must both be the
        # non-symlink root. The canonical check also catches replacement before deletion.
        try:
            if os.path.dirname(os.path.abspath(path)) != absolute_root:
                skipped.append(path)
                continue
            if os.path.dirname(os.path.realpath(path)) != absolute_root:
                skipped.append(path)
                continue
        except OSError:
            skipped.append(path)
            continue

        # `exit_code` is written only after redirection closes, so it is the completion marker.
        # Never age an active run by its allocation-time directory mtime: another pane may be
        # pruning concurrently, and a long-running command's spool must remain available.
        mtime = _completion_mtime(os.path.join(path, "exit_code"))
        if mtime is None:
            kept += 1
            continue

        if mtime >= cutoff:
            kept += 1
            continue

        try:
            shutil.rmtree(path)
        except OSError:
            skipped.append(path)
            continue
        removed.append(path)

    return PruneResult(tuple(removed), kept, tuple(skipped))


def _completion_mtime(path: str) -> float | None:
    """Return a valid regular completion marker's mtime without following a symlink."""
    flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    try:
        descriptor = os.open(path, flags)
        try:
            metadata = os.fstat(descriptor)
            if not stat.S_ISREG(metadata.st_mode):
                return None
            payload = os.read(descriptor, 129)
        finally:
            os.close(descriptor)
        if parse_exit_marker(payload) is None:
            return None
        return metadata.st_mtime
    except (OSError, ValueError):
        return None
