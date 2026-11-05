"""The enforcement manifest, derived from the guards that implement it, PER LANE.

The ``capabilities`` subcommand advertises which containment guards this engine really
applies. That manifest used to be a hand-typed JSON string literal, which meant it could
claim enforcement that no longer happened: nothing tied the literal to a guard site, so
deleting or short-circuiting a guard left the advertisement intact. The py-vs-rs
differential compares the two manifests byte-for-byte, which stops the two ENGINES from
drifting apart but cannot notice both being wrong together, nor a manifest drifting from
the guard sites inside its own engine.

So the manifest is generated here instead. :data:`ENFORCEMENT_REGISTRY` is the single
declaration; :func:`enforcement_manifest` serializes it key-sorted, compact and with
lowercase booleans, so the two editions agree BY CONSTRUCTION rather than because two
people kept two literals in step; and :func:`is_enforced` is consulted by the contractual
guards. The uncontained CPU-time fallback is the explicit exception described below.

THE MANIFEST IS PER LANE, BECAUSE ENFORCEMENT IS. Most of these guards are implemented by
reading or writing a cgroup, and a run that could not get one (``--allow-cgroup-failure``,
``--unsafe-no-cgroups``, or a library call with no manager) does not have them -- the step
still runs, still exits 0, and is still reported green. A single flat column could only
ever describe one of those two worlds, and it described the boxed one, so an uncontained
run was advertised guards it was not getting: a step declaring ``cpu_timeout: 3`` could
burn 60 CPU-seconds and be reported green while the manifest said the budget held. Each
:class:`Capability` therefore carries TWO flags, ``contained`` and ``uncontained``, and
:func:`is_enforced` takes the :class:`Lane` the run is actually on. The lane is not a
comment about the guard site; it is the argument the contractual guard site passes.
The best-effort uncontained CPU fallback is deliberately outside that Boolean guarantee.

:func:`is_enforced` raises on an unknown key. A guard site that misspells its capability
therefore fails loudly at the moment it runs, rather than reading as "not enforced" and
silently switching the guard off -- which is the exact failure this module exists to
prevent.

WHICH KEYS ACTUALLY GATE SOMETHING. Five of the nine participate in guard code. Four
have load-bearing flags on both lanes; ``cpu_timeout`` gates the exact cgroup guard while
the non-contractual uncontained fallback remains active despite its deliberately false flag:

    ==============  ================================================================
    key             guard site that consults ``is_enforced``
    ==============  ================================================================
    cpu_timeout     exact cgroup ``cpu.stat`` guard; uncontained fallback is non-contractual
    wall_timeout    the scheduler's per-step wait deadline
    oom_detection   the post-step ``memory.events`` read: any positive ``oom``, ``oom_kill`` or ``oom_group_kill``
    memory_max      the per-step inner ``memory.max`` write in the cgroup manager
    pids_guard      the per-step inner ``pids.max`` write in the cgroup manager
    ==============  ================================================================

The remaining four -- ``cpu_affinity``, ``cpu_bandwidth``, ``run_timeout`` and
``write_domains`` -- are declarations only: the registry records them and the manifest
publishes them, but no code consults their flag, so flipping one changes the
advertisement WITHOUT changing behaviour. Wiring those is a further step, and saying so
here is better than implying a coverage that does not exist.
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from enum import Enum

__all__ = [
    "Capability",
    "ENFORCEMENT_REGISTRY",
    "Lane",
    "enforcement_manifest",
    "is_enforced",
    "registry_override",
]


class Lane(Enum):
    """Which containment world a run is in; the manifest publishes one column per lane.

    ``CONTAINED`` is the boxed lane: a cgroup-v2 child per step, so the cgroup reads and
    writes the guards are made of actually happen. ``UNCONTAINED`` is what
    ``--allow-cgroup-failure``, ``--unsafe-no-cgroups`` or a library call with no manager
    gives you: the step still runs, but every cgroup-backed guard is absent.
    """

    CONTAINED = "contained"
    UNCONTAINED = "uncontained"


@dataclass(frozen=True)
class Capability:
    """One advertised containment guard, on both lanes.

    ``key`` is the manifest key (and what a guard site passes to :func:`is_enforced`);
    ``contained`` and ``uncontained`` are whether this engine really applies it with and
    without cgroup boxing; ``summary`` is the human sentence that used to live in the
    comment above the string literal.
    """

    key: str
    contained: bool
    uncontained: bool
    summary: str

    def enforced_on(self, lane: Lane) -> bool:
        """This capability's flag for ``lane``."""
        return self.contained if lane is Lane.CONTAINED else self.uncontained


#: Every enforcement guard this engine advertises, and whether it is real ON EACH LANE.
#: This is the ONLY place the answer is written down: the manifest is generated from it and
#: the guard sites listed in the module docstring consult it. The boxed smoke tests in each
#: build anchor the ``contained`` column to real behaviour wherever a cgroup-v2 + systemd
#: --user scope exists; in the ``uncontained`` column only ``run_timeout``, ``wall_timeout``
#: and ``write_domains`` are guaranteed. A best-effort procfs CPU floor may also act, but
#: remains outside the Boolean contract because it misses process-group escapees.
ENFORCEMENT_REGISTRY: tuple[Capability, ...] = (
    Capability(
        key="cpu_affinity",
        contained=True,
        # `--cores` REFUSES rather than degrading, so the guard is not in force here: it is
        # not that a weaker version runs, it is that the run does not start.
        uncontained=False,
        summary=(
            "opt-in --cores K: constrain the WHOLE run tree to K least-busy free cores "
            "with an exact, verified cgroup cpuset; refuse when unavailable"
        ),
    ),
    Capability(
        key="cpu_bandwidth",
        contained=True,
        uncontained=False,
        summary=(
            "boxed run: exact outer cpu.max = --max-cpus x period, read back before execution"
        ),
    ),
    Capability(
        key="cpu_timeout",
        contained=True,
        # THE DEFECT #75 NAMES: no cgroup, no cpu.stat, no CPU-time enforcement at all.
        uncontained=False,
        summary="per-step user+system CPU budget (cgroup cpu.stat), reaped over budget",
    ),
    Capability(
        key="memory_max",
        contained=True,
        uncontained=False,
        summary="per-step inner memory.max cap (kernel OOM-kills the step at its cap)",
    ),
    Capability(
        key="oom_detection",
        contained=True,
        uncontained=False,
        summary=(
            "failure attributed to OOM when any cgroup memory.events oom, oom_kill or "
            "oom_group_kill count is positive"
        ),
    ),
    Capability(
        key="pids_guard",
        contained=False,
        uncontained=False,
        summary=(
            "per-step PID/thread ceiling: the cgroup manager can write pids.max, but no "
            "caller sets the limit and the companion native engine has no pids plumbing "
            "at all, so the write is gated off here and nothing applies a PID ceiling"
        ),
    ),
    Capability(
        key="run_timeout",
        contained=True,
        uncontained=True,
        summary=(
            "OUTER wall budget for the WHOLE run: the scheduler cuts in-flight steps and "
            "still reports (works boxed or unboxed); under boxing it is additionally backed "
            "by the scope's systemd RuntimeMaxSec, set strictly later so the reporting "
            "bound fires first"
        ),
    ),
    Capability(
        key="wall_timeout",
        contained=True,
        uncontained=True,
        summary="per-step wall-clock ceiling (load-dependent; active with or without boxing)",
    ),
    Capability(
        key="write_domains",
        contained=True,
        uncontained=True,
        summary=(
            "pre-execution closed-vocabulary declaration guard; omission/unknown/duplicate "
            "domains refuse before any node starts when the DAG opts in"
        ),
    ),
)

#: Bracket support only (see :func:`registry_override`); ``None`` means "use the real one".
_OVERRIDE: tuple[Capability, ...] | None = None


def _active() -> tuple[Capability, ...]:
    return ENFORCEMENT_REGISTRY if _OVERRIDE is None else _OVERRIDE


@contextmanager
def registry_override(key: str, enforced: bool, lane: Lane | None = None) -> Iterator[None]:
    """Temporarily flip one capability's flag. **Brackets only.**

    With ``lane=None`` BOTH lanes move, which is what a bracket asserting "this flag is
    load-bearing" wants: the guard site is then off whichever lane the test's manager puts
    it on. Pass a lane to move exactly one column, which is how a bracket proves the lane
    argument is honoured rather than ignored.

    This exists so a test can prove the coupling the module claims: flip one flag and
    assert that BOTH the published manifest and the guarded behaviour move. A test that
    only re-derived the manifest from a registry it had just built itself would be
    tautological; a test that only checked behaviour would not notice the manifest lying.

    Mirrored by the companion native engine's ``with_registry_override``, and like it,
    raises for an unknown key so a stale bracket cannot silently flip nothing.
    """
    global _OVERRIDE
    base = _active()
    if not any(c.key == key for c in base):
        raise KeyError(f"unknown enforcement capability {key!r}")
    previous = _OVERRIDE

    def flipped(c: Capability) -> Capability:
        if c.key != key:
            return c
        return Capability(
            key=c.key,
            contained=enforced if lane in (None, Lane.CONTAINED) else c.contained,
            uncontained=enforced if lane in (None, Lane.UNCONTAINED) else c.uncontained,
            summary=c.summary,
        )

    _OVERRIDE = tuple(flipped(c) for c in base)
    try:
        yield
    finally:
        _OVERRIDE = previous


def enforcement_manifest() -> str:
    """The machine-readable manifest: two lanes, key-sorted, compact, lowercase booleans.

    Byte-identical to the companion native engine's manifest by construction -- both
    serialize the same shape from their own registry, so a guard present in one build and
    missing from the other shows up as a differing manifest in the cross-engine check.

    Both lanes carry the same sorted key set, so a reader can diff the two columns: a key
    present in one and absent from the other would read as "not applicable" when it means
    "nobody wrote it down".
    """
    active = sorted(_active(), key=lambda c: c.key)
    return json.dumps(
        {lane.value: {c.key: c.enforced_on(lane) for c in active} for lane in Lane},
        separators=(",", ":"),
        sort_keys=True,
    )


def is_enforced(key: str, lane: Lane) -> bool:
    """Whether this engine really applies the guard named ``key`` on ``lane``.

    Call this AT the guard site, not near it, and pass the lane the RUN is on rather than
    the lane the guard was written for, so the flag and the behaviour cannot part company
    on either lane. Raises :class:`KeyError` for an unknown key: a misspelled capability
    at a guard site is a bug that must be loud, because the quiet reading of it --
    "unknown, so not enforced" -- would silently disable the very guard the caller was
    writing.
    """
    for capability in _active():
        if capability.key == key:
            return capability.enforced_on(lane)
    known = ", ".join(sorted(c.key for c in _active()))
    raise KeyError(
        f"unknown enforcement capability {key!r}; a guard site must name a capability "
        f"declared in ENFORCEMENT_REGISTRY (known: {known})"
    )
