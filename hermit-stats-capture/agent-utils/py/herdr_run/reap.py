"""Reaping policy for stale herdr tabs.

Agents are coined and destroyed continuously, and every one that ever ran a command through
``herdr-run`` leaves a tab behind. This module decides which of those tabs are *provably* finished
with, so they can be closed, and -- much more importantly -- which ones must be left alone.

WHY THE OBVIOUS SIGNALS DO NOT WORK
-----------------------------------
herdr exposes no tab creation time and no last-activity time. ``PaneInfo.revision`` looks like an
activity counter and is not: it counts pane *metadata* revisions, and stays put while the terminal
produces visible output. So there is no "last used" field to threshold on.

Worse, the tempting substitute -- "no output for N minutes" -- cannot work even in principle here.
A *finished* agent and a *dead* agent are identical on the silence axis. Any absence-of-output
detector conflates them, and the expensive direction of that mistake is killing an agent that is
merely thinking.

WHY THE RUN-DIRECTORY PID IS THE WRONG ANCHOR (measured, not assumed)
---------------------------------------------------------------------
The run spool is ``.herdr-run/runs/<utc>-<agent>-<pid>/``, and it is tempting to read that trailing
PID as "the process that owns this tab" and treat its death as evidence of staleness. **That PID is
the short-lived ``herdr-run`` CLI invocation, not the agent and not the pane.** It exits the moment
the command finishes, so it is dead by construction for every completed run.

Measured on the live spool (15 run directories, 2026-08-06): the run-directory PID was dead for
**15 of 15**, while the pane shell recorded in the same ``meta.json`` was alive for **15 of 15**.
A policy anchored on the run-directory PID would therefore have classified every tab whose commands
had all finished -- which is every healthy idle agent -- as stale, and reaped the entire workspace.

The correct anchor is the **pane's shell process**, which is what actually owns the tab.

THE PREDICATE
-------------
A tab is STALE only when all of these hold, each positive evidence rather than absence:

* **R1 no in-flight work.** Every run naming this pane recorded an ``exit_code``. A run with no
  recorded exit code is IN FLIGHT -- that is the "agent is thinking" case, and it is positively
  distinguishable from a finished one rather than being inferred from silence. The state is
  written by exactly one place: :func:`herdr_run.runner.execute` records the run with a null exit
  code when collection times out, because the command is then still running in a pane this process
  does not own. That is the whole reason the rule exists, and until the timeout path recorded
  anything the rule was unreachable -- documented as a protection while no writer could produce the
  state it tests for. :func:`herdr_run.sweep.load_run_records` re-reads the run's ``exit_code``
  file afterwards, so a command that finished after we stopped waiting stops looking in flight.
* **R2 the pane's shell is gone.** Not the run-directory PID; the shell PID.
* **R3 reboot and PID reuse are excluded.** ``kill(pid, 0)`` alone is not sufficient on a busy
  host: a recycled PID makes an unrelated live process look like proof of liveness, and a
  same-numbered stranger look like proof of death. Identity is bound as
  ``(pid, boot_id, start_ticks)`` -- field 22 of ``/proc/<pid>/stat`` -- which is the shape
  needed to distinguish a process from a recycled number. The recorded boot must also match the
  current boot before absence of the old PID can authorize anything.

Anything else -- unreadable ``/proc``, missing ``meta.json``, absent ``pane_id``, a boot-id
mismatch, a workspace herdr could not list -- is UNKNOWN, and unknown is never reaped. The cost
asymmetry is the whole reason: killing an agent mid-work is far worse than clutter, and a reaper
that is wrong once in the expensive direction gets switched off permanently.

GONE: A RECORD WHOSE TAB IS ALREADY CLOSED
-----------------------------------------
A run record outlives its tab: a tab closed by hand keeps its records until retention prunes them.
When herdr answered with a listing of the workspace and the recorded pane is not in it, the tab is
GONE. There is nothing to close, and it holds none of ``max_panes``, because the cap counts herdr's
live pane list rather than run records. GONE needs a listing that succeeded: a listing that failed
is UNKNOWN, since "herdr did not answer" must never read as "every pane is gone". Measured
2026-10-02 on devbig030: 15 of the 32 records ``reap`` reported were such closed tabs, reported as
UNKNOWN, and read as capacity nobody could reclaim while the cap was in fact held by live panes
with no surviving record.

SCOPE IS ENFORCED, NOT TRUSTED
------------------------------
Only panes belonging to tabs in *our* workspace, whose labels match the configured tab schema, are
even considered. A tab a human opened by hand, or another tool's workspace, is not a candidate no
matter how idle it looks.

REPORT-ONLY BY DEFAULT
----------------------
``plan_reap`` decides; it never closes anything. Closing is a separate, explicit step. A reaper that
is trusted before it has been checked against a known-good population is how the whole workspace
gets deleted once.

This module holds no I/O. Evidence is passed in, so the policy is testable without a live herdr and
without waiting for real processes to die -- and so the planted-stale and planted-live cases can
both be exercised deterministically.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field

from herdr_run.textfmt import quote, quote_optional, strip_space

__all__ = [
    "PaneEvidence",
    "ProcessIdentity",
    "ReapDecision",
    "ReapPlan",
    "Verdict",
    "plan_reap",
]


class Verdict:
    """Why a pane was or was not selected. Strings so they survive into JSON and logs."""

    STALE = "STALE"
    IN_FLIGHT = "IN_FLIGHT"
    SHELL_ALIVE = "SHELL_ALIVE"
    GONE = "GONE"
    UNKNOWN = "UNKNOWN"
    OUT_OF_SCOPE = "OUT_OF_SCOPE"


@dataclass(frozen=True)
class ProcessIdentity:
    """A PID bound to the boot and start-tick that make it *that* process.

    ``boot_id`` and ``start_ticks`` are what stop a recycled PID from being mistaken for the
    original. Either being ``None`` means we could not establish identity, which is UNKNOWN --
    never a licence to reap.
    """

    pid: int | None
    boot_id: str | None = None
    start_ticks: int | None = None

    def is_bound(self) -> bool:
        """True when pid, boot_id and start_ticks are all known, so reuse can be excluded."""
        return (
            type(self.pid) is int
            and 1 <= self.pid <= 2_147_483_647
            and isinstance(self.boot_id, str)
            and bool(strip_space(self.boot_id))
            and type(self.start_ticks) is int
            and self.start_ticks >= 0
        )

    def same_process_as(self, other: "ProcessIdentity") -> bool:
        """True only if both identities are bound AND agree on pid, boot and start tick."""
        if not (self.is_bound() and other.is_bound()):
            return False
        return (
            self.pid == other.pid
            and self.boot_id == other.boot_id
            and self.start_ticks == other.start_ticks
        )


@dataclass(frozen=True)
class PaneEvidence:
    """Everything the policy is allowed to look at for one pane.

    ``recorded_shell`` is the identity captured in ``meta.json`` when the run happened.
    ``live_shell`` is the identity of the pane's shell *right now*, or ``None`` if the shell is
    gone. The two are compared rather than either being trusted alone: that comparison excludes
    both PID reuse and a new pane incarnation that reused the old pane id.
    """

    pane_id: str
    tab_id: str | None = None
    tab_label: str | None = None
    workspace_label: str | None = None
    in_scope: bool = False
    #: One entry per run naming this pane. ``True`` means an exit code was recorded.
    run_exit_codes_recorded: Sequence[bool] = field(default_factory=tuple)
    recorded_shell: ProcessIdentity | None = None
    live_shell: ProcessIdentity | None = None
    #: Current kernel boot id, gathered independently even when the recorded pid is absent.
    current_boot_id: str | None = None
    #: Whether herdr's listing of the workspace includes the pane. Read as GONE only when
    #: ``evidence_error`` is ``None``, so a failed listing must set ``evidence_error``.
    pane_known_to_herdr: bool = True
    #: Set when evidence could not be gathered, including a failed workspace listing; forces
    #: UNKNOWN with this reason.
    evidence_error: str | None = None


@dataclass(frozen=True)
class ReapDecision:
    """One pane's verdict, with the reason recorded so a refusal can be audited."""

    pane_id: str
    tab_id: str | None
    tab_label: str | None
    verdict: str
    reason: str

    @property
    def reapable(self) -> bool:
        """Only a STALE verdict authorises closing the tab."""
        return self.verdict == Verdict.STALE


@dataclass(frozen=True)
class ReapPlan:
    """The full set of verdicts for one sweep. Decides only; closes nothing."""

    decisions: tuple[ReapDecision, ...]

    @property
    def reapable(self) -> tuple[ReapDecision, ...]:
        """Panes proven stale. Everything else is spared."""
        return tuple(d for d in self.decisions if d.reapable)

    @property
    def declined(self) -> tuple[ReapDecision, ...]:
        """Panes left alone, each with the reason -- so "reaped nothing" is never silent."""
        return tuple(d for d in self.decisions if not d.reapable)

    def counts(self) -> dict[str, int]:
        """Counts for EVERY verdict, including the zeros.

        Reported unconditionally because "reaped 0 because nothing was stale" and "reaped 0 because
        the detector is inert" are indistinguishable from a bare total.
        """
        out = {
            Verdict.STALE: 0,
            Verdict.IN_FLIGHT: 0,
            Verdict.SHELL_ALIVE: 0,
            Verdict.GONE: 0,
            Verdict.UNKNOWN: 0,
            Verdict.OUT_OF_SCOPE: 0,
        }
        for decision in self.decisions:
            out[decision.verdict] = out.get(decision.verdict, 0) + 1
        out["considered"] = len(self.decisions)
        return out


def _shown(value: object) -> str:
    """Render an identity field for a reason: the value, or ``?`` when it is unknown."""
    return "?" if value is None else str(value)


def _decide(evidence: PaneEvidence) -> ReapDecision:
    def decision(verdict: str, reason: str) -> ReapDecision:
        return ReapDecision(
            pane_id=evidence.pane_id,
            tab_id=evidence.tab_id,
            tab_label=evidence.tab_label,
            verdict=verdict,
            reason=reason,
        )

    # Scope first: a tab outside our workspace/schema is never a candidate, however idle it looks.
    if not evidence.in_scope:
        return decision(
            Verdict.OUT_OF_SCOPE,
            f"tab {quote_optional(evidence.tab_label)} in workspace "
            f"{quote_optional(evidence.workspace_label)} is not one of ours",
        )

    if evidence.evidence_error is not None:
        return decision(Verdict.UNKNOWN, f"evidence unavailable: {evidence.evidence_error}")

    if not evidence.pane_known_to_herdr:
        # The listing succeeded (a failed one set evidence_error above) and does not include the
        # pane, so its tab is already closed: nothing to close, and no share of max_panes.
        return decision(
            Verdict.GONE,
            "herdr's listing of the workspace does not include this pane: its tab is already "
            "closed and holds none of max_panes",
        )

    # R1 -- in-flight work beats every other signal. This is the "agent is thinking" case.
    unfinished = sum(1 for recorded in evidence.run_exit_codes_recorded if not recorded)
    if unfinished:
        return decision(
            Verdict.IN_FLIGHT,
            f"{unfinished} run(s) for this pane have no recorded exit_code",
        )

    # R3 -- we must have a bound identity from when the run happened, or we cannot reason at all.
    recorded = evidence.recorded_shell
    if recorded is None or not recorded.is_bound():
        return decision(
            Verdict.UNKNOWN,
            "no identity-bound shell recorded for this pane (need pid+boot_id+start_ticks)",
        )

    # A missing recorded PID is only evidence of death within the SAME boot. Across a reboot every
    # old pid is absent by construction, which cannot authorize closing a newly-created live tab.
    current_boot = evidence.current_boot_id
    if not isinstance(current_boot, str) or not strip_space(current_boot):
        return decision(Verdict.UNKNOWN, "current boot_id is unavailable")
    if recorded.boot_id != current_boot:
        return decision(
            Verdict.UNKNOWN,
            f"recorded shell belongs to boot {quote_optional(recorded.boot_id)}, current boot is "
            f"{quote(current_boot)}",
        )

    live = evidence.live_shell

    # R2 -- the shell is gone. Note this is the PANE shell, not the run-directory PID, which is the
    # short-lived herdr-run CLI and is dead for every completed run.
    if live is None:
        return decision(
            Verdict.STALE,
            f"all runs finished and pane shell {recorded.pid} is gone "
            f"(identity {recorded.boot_id}:{recorded.start_ticks})",
        )

    if not live.is_bound():
        return decision(
            Verdict.UNKNOWN, f"could not bind identity of live pid {_shown(live.pid)}"
        )

    if recorded.same_process_as(live):
        return decision(Verdict.SHELL_ALIVE, f"pane shell {live.pid} is still the original process")

    # A different identity may be PID reuse or a new pane incarnation. Deliberately UNKNOWN, not
    # STALE: the original is probably gone, but the currently-listed pane is not proven to be it.
    return decision(
        Verdict.UNKNOWN,
        f"recorded pane shell {recorded.pid} is now a DIFFERENT process identity "
        f"(recorded {recorded.boot_id}:{recorded.start_ticks}, "
        f"live {_shown(live.pid)}/{_shown(live.boot_id)}:{_shown(live.start_ticks)}) -- PID "
        "reuse or new pane incarnation, "
        "refusing to guess",
    )


def plan_reap(evidence: Iterable[PaneEvidence]) -> ReapPlan:
    """Decide, for each pane, whether it is provably stale. Closes nothing."""
    return ReapPlan(tuple(_decide(item) for item in evidence))


def evidence_from_runs(
    pane_id: str,
    runs: Iterable[Mapping[str, object]],
) -> tuple[tuple[bool, ...], ProcessIdentity | None]:
    """Fold this pane's run records into (exit-code-recorded flags, recorded shell identity).

    ``runs`` are parsed ``meta.json`` mappings. Only records naming ``pane_id`` contribute. A record
    with ``from_cache`` true never launched anything, so it is not evidence that a pane exists and
    is skipped -- otherwise a cache hit would manufacture a phantom candidate.
    """
    recorded_flags: list[bool] = []
    identity: ProcessIdentity | None = None
    identity_conflict = False
    for record in runs:
        if record.get("pane_id") != pane_id:
            continue
        if record.get("from_cache") is True:
            continue
        recorded_flags.append(record.get("exit_code") is not None)
        readiness = record.get("readiness")
        if isinstance(readiness, Mapping):
            shell_pid = readiness.get("shell_pid")
            # Typed exactly as a 64-bit JSON reader types them: the pid a signed 64-bit integer,
            # the start tick an unsigned one. Anything else is not that field at all.
            if type(shell_pid) is int and -(2**63) <= shell_pid <= 2**63 - 1:
                boot = readiness.get("boot_id")
                ticks = readiness.get("shell_start_ticks")
                candidate = ProcessIdentity(
                    pid=shell_pid,
                    boot_id=boot if isinstance(boot, str) and strip_space(boot) else None,
                    start_ticks=ticks if type(ticks) is int and 0 <= ticks <= 2**64 - 1 else None,
                )
                if identity is None:
                    identity = candidate
                elif identity != candidate:
                    identity_conflict = True
    return tuple(recorded_flags), None if identity_conflict else identity
