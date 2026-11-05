"""Core vocabulary for tick-hub: the declarative reminder set a caller plugs in.

Pure data + pure helpers, no I/O. A caller describes their recurring
responsibilities as a :class:`TickConfig` — a set of :class:`Reminder` values
(each with a cadence, an optional shell :class:`Gate`, and an :class:`Emit`
template) plus a set of :class:`HealthCheck` freshness probes — then hands it to
the engine once per scheduled *tick*.

The engine stays free of caller-specific reminders, handlers, paths, and gates:
each of those lives here as data supplied by the caller.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from enum import Enum

#: A reminder with this cadence fires (is CHECKED) on every tick.
EVERY_TICK = 0


class EmitKind(Enum):
    """What line a fired reminder produces."""

    #: ``ACTION: <skill> [k=v ...] title="..."`` — one unit of work to dispatch.
    ACTION = "action"
    #: ``NOTE: <text>`` — informational only; never dispatched.
    NOTE = "note"


class GateWhen(Enum):
    """When a reminder's gate command counts as "fire this reminder"."""

    #: gate command exits 0.
    SUCCESS = "success"
    #: gate command exits non-zero.
    FAILURE = "failure"
    #: gate command prints non-whitespace to stdout.
    NONEMPTY = "nonempty"
    #: always fire (run the command only to CAPTURE its ``key=value`` output).
    ALWAYS = "always"


@dataclass(frozen=True)
class Gate:
    """An optional shell command that decides whether a due reminder fires.

    The command runs via ``bash -c`` when the reminder is due. ``when`` selects
    the fire condition (:class:`GateWhen`). With ``capture`` set, the command's
    stdout is parsed for ``key=value`` lines, which are merged into the emitted
    ACTION's fields and interpolated into the title/text via ``{key}``
    placeholders — so a reminder can carry live values (a count, a SHA, a URL)
    without the engine hardcoding anything. ``timeout_secs`` optionally raises
    or lowers the subprocess boundary for this gate only; ``None`` retains the
    runner's global default. ``parallel`` permits the production runner to
    start this command ahead of its configuration position and overlap it with
    other gate commands. It defaults to false because callers must establish
    that the command cannot race any other due gate's shared state.
    """

    cmd: str
    when: GateWhen = GateWhen.SUCCESS
    capture: bool = False
    timeout_secs: int | None = None
    parallel: bool = False


@dataclass(frozen=True)
class Emit:
    """What a fired reminder emits.

    For :attr:`EmitKind.ACTION`, ``skill`` names the handler and ``fields`` are
    the ``key=value`` pairs; for :attr:`EmitKind.NOTE`, only ``title`` (the note
    text) is used. Captured gate values override matching ``fields`` and fill
    ``{key}`` placeholders in ``title``.
    """

    kind: EmitKind
    title: str = ""
    skill: str = ""
    fields: Mapping[str, str] = field(default_factory=dict)


@dataclass(frozen=True)
class Reminder:
    """One recurring responsibility, checked once per tick when due.

    ``cadence_secs`` is how often the reminder is CHECKED (``0`` = every tick);
    the check runs when at least that many seconds have elapsed since it last
    ran. ``cadence_offset_secs`` optionally PHASES that cadence against absolute
    time so reminders sharing a cadence need not come due on the same tick: the
    reminder becomes due at the largest instant ``T <= now`` satisfying
    ``T % cadence_secs == cadence_offset_secs``, and only if it has not fired
    since that instant. Absolute time is deliberate -- the phase is a property of
    the clock, not of how many ticks have run, so it survives a restart and is
    identical on replay. ``cadence_window_secs`` bounds how long
    after that instant the reminder may still be offered, so one that did not
    complete is retried on its OWN phase instead of on every following tick --
    without it a cut-off cohort lands wholesale on the tick meant to carry the
    other half. It does not make a missed reminder count as done: no epoch is
    recorded and it is offered again at its next instant. Leaving both ``None``
    keeps the plain elapsed rule unchanged. ``requires_flags`` names caller-state flags that must all be truthy for
    the reminder to run at all (the generic analog of gating on a runtime toggle
    like "am I the ops driver?" or "is benchmarking enabled?"). ``depends_on``
    names other reminders whose explicit ``NO_RESULT`` makes this reminder's
    otherwise-quiet outcome unevaluable. Dependencies never suppress a real
    emission and never prevent this reminder's gate from running. ``gate`` is
    an optional shell check; ``emit`` is what to produce when it fires.
    """

    name: str
    emit: Emit
    cadence_secs: int = EVERY_TICK
    requires_flags: Sequence[str] = field(default_factory=tuple)
    gate: Gate | None = None
    depends_on: Sequence[str] = field(default_factory=tuple)
    # ⚠️ APPENDED, NOT INSERTED. Placing these before requires_flags/gate/depends_on
    # would silently rebind existing POSITIONAL callers: Reminder(name, emit, 3600,
    # ("flag",)) would hand the flags tuple to cadence_offset_secs and typecheck fine.
    # New fields on a public dataclass go last.
    cadence_offset_secs: int | None = None
    cadence_window_secs: int | None = None


@dataclass(frozen=True)
class HealthCheck:
    """A freshness probe over a file glob.

    The engine measures the age of the newest file matching ``glob`` and reports
    ``ok`` (age within ``threshold_secs``), ``stale`` (older), or ``missing`` (no
    match). Health lines are a signal for the caller to investigate, never a
    dispatched work item.
    """

    name: str
    glob: str
    threshold_secs: int
    detail: str = ""


@dataclass(frozen=True)
class TickConfig:
    """A whole reminder set: the caller's plug-in, loadable from JSON or YAML."""

    reminders: tuple[Reminder, ...] = ()
    health_checks: tuple[HealthCheck, ...] = ()
    #: Optional long-form documentation for the whole config (never affects behavior).
    description: str = ""

    def by_name(self) -> dict[str, Reminder]:
        """Index reminders by their configured names."""
        return {r.name: r for r in self.reminders}
