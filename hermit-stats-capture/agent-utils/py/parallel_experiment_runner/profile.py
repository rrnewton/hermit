"""Stable profile keys + a small per-key sample store for cost estimation and calibration.

This layer is DELIBERATELY separate from ``dagrun``'s own profile store. A seed
sweep's per-round step tags embed the seed (``seed.00000042``) and so cannot be prior-run
keys; instead every seed step in one apples-to-apples round shares ONE stable
:func:`profile_identity` key, and this store groups samples under that key. Keeping it here
means the runner reuses the executor without modifying its model or profile schema — the
additive, conflict-free integration the design calls for.

Apples-to-apples is a correctness decision, not a convenience (design §4.2): the key hashes
the fields that MATERIALLY change execution time/footprint (backend, kernel/image content id,
vCPU/memory class, the command template with the seed removed) and EXCLUDES the seed, run id,
timestamps, temporary paths, and coordinator revision. A caller-supplied label never lets two
incompatible hard dimensions share history — the mandatory dims are always folded into the key.
"""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from parallel_experiment_runner.model import CostEstimate, ExperimentSpec, SEED_PLACEHOLDER

#: Identity fields that MUST be folded into the key: two runs that differ on any of these are
#: never apples-to-apples, so even a manual ``--profile-key`` label cannot merge across them.
MANDATORY_IDENTITY_DIMS: tuple[str, ...] = (
    "backend",
    "kernel_id",
    "image_id",
    "vcpu",
    "guest_memory",
    "accelerator",
)

#: Keep at most this many samples per key (newest wins), so the store stays small and bounded.
MAX_SAMPLES_PER_KEY = 200

#: High percentile used for the CONSERVATIVE memory/CPU estimate (don't under-provision).
CONSERVATIVE_PCTL = 90


@dataclass(frozen=True)
class ProfileIdentity:
    """The resolved stable identity for a spec: a persistable ``key`` (or an ephemeral one).

    ``ephemeral`` is set when no content identity could be established (no ``identity`` fields
    and no manual key): the sweep still runs and calibrates, but its samples are NOT written to
    shared history, so an unattributable run can never contaminate another sweep's estimates.
    """

    key: str
    ephemeral: bool
    fields: Mapping[str, str]


def _normalized_command_template(command: Sequence[str]) -> str:
    """The command with the seed neutralized, so two rounds over different seeds hash equal."""
    # The placeholder is already seed-free; render defensively in case a literal seed slipped in
    # is impossible here (spec validates the placeholder exists), so just join the template.
    return "\x1f".join(part for part in command)


def profile_identity(spec: ExperimentSpec) -> ProfileIdentity:
    """Resolve the stable profile key for ``spec`` (design §4).

    * A manual ``spec.profile_key`` becomes the human-readable prefix but is ALWAYS suffixed
      with a hash of the mandatory hard dimensions present in ``identity``.
    * Otherwise the key is a hash of the canonical identity record (command template with the
      seed removed + sorted identity fields + hit semantics).
    * With neither a manual key nor any identity fields, the key is ephemeral (per-invocation)
      and never persisted.
    """
    mandatory = {k: spec.identity[k] for k in MANDATORY_IDENTITY_DIMS if k in spec.identity}
    canonical: dict[str, object] = {
        "command_template": _normalized_command_template(spec.command),
        "identity": {k: spec.identity[k] for k in sorted(spec.identity)},
        "hit_regex": spec.hit.regex,
        "hit_exit_codes": list(spec.hit.hit_exit_codes),
    }
    if spec.detached_resource_report is not None:
        # Preserve existing profile keys for ordinary workloads while never mixing their
        # wrapper-only samples with complete detached-child accounting.
        mandatory["detached_resource_accounting"] = "true"
        canonical["detached_resource_accounting"] = True
    digest = hashlib.sha256(
        json.dumps(canonical, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()[:12]

    if spec.profile_key:
        mand_digest = hashlib.sha256(
            json.dumps(mandatory, sort_keys=True, separators=(",", ":")).encode("utf-8")
        ).hexdigest()[:8]
        return ProfileIdentity(
            key=f"{spec.profile_key}.{mand_digest}", ephemeral=False, fields=dict(mandatory)
        )
    if not spec.identity:
        # No content identity at all: run, but do not pollute shared history.
        return ProfileIdentity(key=f"ephemeral.{digest}", ephemeral=True, fields={})
    return ProfileIdentity(key=f"auto.{digest}", ephemeral=False, fields=dict(spec.identity))


@dataclass(frozen=True)
class Sample:
    """One measured worker: wall seconds, CPU seconds (user+sys), peak RSS bytes, disk bytes."""

    wall_s: float
    cpu_s: float | None
    peak_bytes: int | None
    disk_bytes: int | None
    tasks_peak: int | None = None


def _percentile(values: Sequence[float], pctl: int) -> float:
    """Nearest-rank percentile of a non-empty sorted-able sequence."""
    ordered = sorted(values)
    if not ordered:
        raise ValueError("percentile of empty sequence")
    rank = max(1, min(len(ordered), round(pctl / 100.0 * len(ordered))))
    return ordered[rank - 1]


class ProfileStore:
    """A tiny JSON-backed store mapping a stable key -> a bounded list of worker samples.

    The file is a single JSON object ``{key: [sample, ...]}``. Writes are atomic (temp file +
    rename). A missing/unreadable file behaves as empty. This never raises into the run path.
    """

    def __init__(self, path: Path) -> None:
        self.path = path
        self._data: dict[str, list[dict[str, object]]] = {}
        self._load()

    def _load(self) -> None:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self._data = {}
            return
        if isinstance(raw, dict):
            data: dict[str, list[dict[str, object]]] = {}
            for key, rows in raw.items():
                if isinstance(key, str) and isinstance(rows, list):
                    data[key] = [r for r in rows if isinstance(r, dict)]
            self._data = data

    def record(self, key: str, samples: Sequence[Sample]) -> None:
        """Append samples under ``key`` (bounded to the newest :data:`MAX_SAMPLES_PER_KEY`)."""
        if not samples:
            return
        bucket = self._data.setdefault(key, [])
        for s in samples:
            bucket.append(
                {
                    "wall_s": s.wall_s,
                    "cpu_s": s.cpu_s,
                    "peak_bytes": s.peak_bytes,
                    "disk_bytes": s.disk_bytes,
                    "tasks_peak": s.tasks_peak,
                }
            )
        if len(bucket) > MAX_SAMPLES_PER_KEY:
            del bucket[: len(bucket) - MAX_SAMPLES_PER_KEY]
        self._flush()

    def _flush(self) -> None:
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            fd, tmp = tempfile.mkstemp(dir=str(self.path.parent), suffix=".tmp")
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(self._data, handle, sort_keys=True, indent=2)
            os.replace(tmp, self.path)
        except OSError:
            pass  # a store write must never fail a sweep

    def _floats(self, key: str, column: str) -> list[float]:
        out: list[float] = []
        for row in self._data.get(key, ()):
            value = row.get(column)
            if isinstance(value, (int, float)):
                out.append(float(value))
        return out

    def estimate(self, key: str) -> CostEstimate:
        """A DERIVED per-worker estimate for ``key``: conservative (p90) CPU/memory/tasks,
        median wall (wall is contention-inflated, so its median is the fairer central figure).

        Returns :meth:`CostEstimate.unset` when the key has no samples — honest "not measured".
        """
        walls = self._floats(key, "wall_s")
        if not walls:
            return CostEstimate.unset(source=key)
        cpus = self._floats(key, "cpu_s")
        peaks = self._floats(key, "peak_bytes")
        task_peaks = self._floats(key, "tasks_peak")
        wall_s = _percentile(walls, 50)
        cpu_s = _percentile(cpus, CONSERVATIVE_PCTL) if cpus else None
        peak = int(_percentile(peaks, CONSERVATIVE_PCTL)) if peaks else None
        peak_tasks = int(_percentile(task_peaks, CONSERVATIVE_PCTL)) if task_peaks else None
        return CostEstimate(
            wall_s=wall_s,
            cpu_s=cpu_s,
            peak_mem_bytes=peak,
            samples=len(walls),
            source=key,
            peak_tasks=peak_tasks,
        )
