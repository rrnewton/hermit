"""Pluggable side-effect boundaries, so the tick engine core stays pure and testable.

The two things a tick does that touch the outside world — running a reminder's shell **gate** and
measuring a health check's **file age** — are expressed as small :class:`typing.Protocol` interfaces
here. The engine takes them as parameters; production uses the default implementations in
:mod:`tick_hub.probes`, and callers can pass deterministic fakes in tests.
"""

from __future__ import annotations

from dataclasses import dataclass
from contextlib import AbstractContextManager
from typing import Optional, Protocol, runtime_checkable


@dataclass(frozen=True)
class GateResult:
    """The outcome of running a reminder's gate command."""

    #: Exit code of the command (``-1`` when no completed process result exists).
    returncode: int
    #: Captured standard output.
    stdout: str
    #: True iff the command ran to completion (False on launch failure or timeout).
    ok: bool
    #: Human-readable reason when ``ok`` is False; otherwise ``None``.
    error: Optional[str] = None


class GateRunner(Protocol):
    """Runs a reminder's gate command and reports its exit code + stdout."""

    def run(self, cmd: str, *, timeout: int | None = None) -> GateResult:
        """Run ``cmd`` and return its captured result.

        ``timeout`` overrides the runner's default for this invocation only.
        """
        ...


@runtime_checkable
class ParallelGateRunner(GateRunner, Protocol):
    """A gate runner that can safely execute ``run`` from concurrent threads."""

    def parallel_scope(self) -> AbstractContextManager[None]:
        """Own all gate processes launched during one concurrent evaluation scope."""
        ...


class FileAgeProbe(Protocol):
    """Reports the age (seconds) of the newest file matching a glob, relative to ``now``.

    Returns ``None`` when nothing matches (so the caller can report ``missing``)."""

    def newest_age_secs(self, pattern: str, now: int) -> Optional[int]:
        """Return the newest matching file's age, or ``None`` when no match is usable."""
        ...
