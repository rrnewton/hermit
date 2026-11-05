"""Least-recently-used replacement of idle tabs once a workspace reaches ``max_panes``.

A new agent that needs a tab in a full workspace no longer has to wait for somebody to close one by
hand. Instead the tab whose last recorded ``herdr-run`` run is OLDEST is closed, provided its shell
is provably idle, and the new tab takes its place.

"Provably idle" is deliberately narrower than "looks quiet":

* no other ``herdr-run`` holds the pane's lock (the runner holds it for a whole command);
* the shell alone owns the terminal's foreground process group (:func:`assess_process`);
* the shell leads its own session and NO other process is in that session, so a background job
  (``cmd &``) or a stopped job keeps the tab open;
* the tab holds exactly one pane, because closing a tab closes every pane in it.

The verdict is taken under the pane lock, immediately before ``tab close``, never from an earlier
survey. When ``/proc`` cannot be read the tab counts as busy: failing closed only costs a refusal,
failing open would kill somebody's command. One window remains, and it is harmless: a caller that
resolved its target before the close, and takes the pane lock after it, finds the pane gone and
fails its readiness check before anything is typed.

Order comes from the run spool. Panes with no record at all -- tabs nobody has used through
``herdr-run``, including leaked ones ``reap`` cannot judge -- come first, in listing order.
"""

from __future__ import annotations

import fcntl
import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from herdr_run import audit, output
from herdr_run.client import HerdrClient, Pane
from herdr_run.config import Config
from herdr_run.errors import HerdrRunError, HerdrUnavailable
from herdr_run.readiness import ProcessSignal, assess_process
from herdr_run.state import open_lock_file, pane_lock_path
from herdr_run.sweep import load_run_records
from herdr_run.textfmt import os_error_text, quote

__all__ = [
    "Candidate",
    "Eviction",
    "SessionScan",
    "describe_skipped",
    "evict_one",
    "judge_idle",
    "lru_candidates",
    "parse_session_id",
    "run_time",
    "scan_session",
]

#: How many skipped tabs a refusal names before summarising the rest as a count.
_REFUSAL_DETAIL_LIMIT = 3


@dataclass(frozen=True)
class Candidate:
    """One tab considered for replacement, in least-recently-used order."""

    pane_id: str
    tab_id: str
    #: Run ID (spool directory name) of the pane's most recent recorded run, if any.
    last_run: str | None
    #: Agent label of that run, if any.
    agent: str | None
    #: Number of panes the listing shows in this tab.
    tab_panes: int


@dataclass(frozen=True)
class SessionScan:
    """What ``/proc`` says about the shell's session."""

    shell_pid: int
    #: Session ID of the shell, or None when its ``stat`` could not be read.
    shell_sid: int | None
    #: Other PIDs in the shell's session, ascending, or None when ``/proc`` could not be listed.
    others: tuple[int, ...] | None


@dataclass(frozen=True)
class Eviction:
    """One closed tab, and why it was judged idle."""

    candidate: Candidate
    reason: str


def lru_candidates(panes: Sequence[Pane], records: Sequence[Mapping[str, object]]) -> list[Candidate]:
    """Order the workspace's panes least-recently-used first.

    ``records`` must be oldest run first, as :func:`load_run_records` returns them, so the LAST
    record naming a pane is its most recent run. Panes without a record sort first; ties keep
    listing order.
    """
    latest: dict[str, tuple[str, str | None]] = {}
    for record in records:
        pane_id = record.get("pane_id")
        run_id = record.get("run_id")
        if not isinstance(pane_id, str) or not isinstance(run_id, str):
            continue
        agent = record.get("agent")
        latest[pane_id] = (run_id, agent if isinstance(agent, str) else None)
    candidates = []
    for pane in panes:
        last = latest.get(pane.pane_id)
        candidates.append(
            Candidate(
                pane_id=pane.pane_id,
                tab_id=pane.tab_id,
                last_run=last[0] if last else None,
                agent=last[1] if last else None,
                tab_panes=sum(1 for other in panes if other.tab_id == pane.tab_id),
            )
        )
    # Unrecorded panes first; `sorted` is stable, so ties keep listing order.
    return sorted(candidates, key=lambda c: (c.last_run is not None, c.last_run or ""))


def parse_session_id(stat_text: str) -> int | None:
    """Read the session ID (field 6) from the text of ``/proc/<pid>/stat``."""
    # The command name may itself contain spaces and parentheses; the fields resume after the LAST
    # ')'. They are then: state, ppid, pgrp, session.
    close = stat_text.rfind(")")
    if close < 0:
        return None
    fields = stat_text[close + 1 :].split()
    if len(fields) < 4:
        return None
    try:
        return int(fields[3])
    except ValueError:
        return None


def _read_session_id(pid: int, proc_root: str) -> int | None:
    try:
        with open(os.path.join(proc_root, str(pid), "stat"), encoding="utf-8", errors="replace") as handle:
            return parse_session_id(handle.read())
    except OSError:
        return None


def scan_session(shell_pid: int, proc_root: str = "/proc") -> SessionScan:
    """Find every other process in the session ``shell_pid`` leads."""
    shell_sid = _read_session_id(shell_pid, proc_root)
    others: tuple[int, ...] | None = None
    if shell_sid == shell_pid:
        try:
            names = os.listdir(proc_root)
        except OSError:
            names = None
        if names is not None:
            members = [
                int(name)
                for name in names
                if name.isdigit()
                and int(name) != shell_pid
                and _read_session_id(int(name), proc_root) == shell_pid
            ]
            others = tuple(sorted(members))
    return SessionScan(shell_pid=shell_pid, shell_sid=shell_sid, others=others)


def judge_idle(signal: ProcessSignal, session: SessionScan) -> tuple[bool, str]:
    """Combine the foreground-group verdict with the session scan into one idle verdict."""
    if not signal.idle:
        return False, signal.reason
    shell = session.shell_pid
    if session.shell_sid is None:
        return False, f"cannot read the session of shell {shell}; treating the tab as busy"
    if session.shell_sid != shell:
        return (
            False,
            f"shell {shell} does not lead its session (session {session.shell_sid}); "
            "treating the tab as busy",
        )
    if session.others is None:
        return False, f"cannot list processes in session {shell}; treating the tab as busy"
    if session.others:
        listed = ", ".join(str(pid) for pid in session.others)
        return False, f"session {shell} still holds {len(session.others)} other process(es): {listed}"
    return True, f"{signal.reason}; no other process in session {shell}"


def run_time(run_id: str) -> str | None:
    """Render a run ID's ``YYYYMMDDTHHMMSS`` prefix as an RFC 3339 UTC time."""
    stamp = run_id[:15]
    if len(stamp) != 15 or stamp[8] != "T" or not _ascii_digits(stamp[:8]) or not _ascii_digits(stamp[9:]):
        return None
    return f"{stamp[:4]}-{stamp[4:6]}-{stamp[6:8]}T{stamp[9:11]}:{stamp[11:13]}:{stamp[13:15]}Z"


def _ascii_digits(text: str) -> bool:
    return all("0" <= char <= "9" for char in text)


def evict_one(
    client: HerdrClient,
    config: Config,
    workspace_id: str,
    agent: str,
    proc_root: str = "/proc",
) -> Eviction | list[tuple[str, str]]:
    """Close the least-recently-used idle tab in ``workspace_id``.

    Returns the :class:`Eviction` after one close, or a list naming every tab that was considered
    and why it stayed open. Only listing the panes can fail outright.
    """
    panes = client.panes(workspace_id)
    records = load_run_records(config)
    skipped: list[tuple[str, str]] = []
    for candidate in lru_candidates(panes, records):
        verdict = _try_evict(client, config, candidate, proc_root)
        if isinstance(verdict, Eviction):
            _log_eviction(config, workspace_id, agent, verdict)
            return verdict
        skipped.append((candidate.pane_id, verdict))
    return skipped


def _try_evict(client: HerdrClient, config: Config, candidate: Candidate, proc_root: str) -> Eviction | str:
    refusal = _require_unsplit(candidate)
    if refusal is not None:
        return refusal
    try:
        lock = open_lock_file(pane_lock_path(candidate.pane_id))
    except HerdrRunError as exc:
        return f"cannot open the pane lock: {exc}"
    try:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return "another herdr-run holds the pane lock"
        except OSError as exc:
            return f"cannot lock the pane: {os_error_text(exc)}"
        # Judged now, under the lock, and closed at once: never from an earlier survey.
        idle, reason = _judge_shell(client, config, candidate, proc_root)
        if not idle:
            return reason
        try:
            client.close_tab(candidate.tab_id)
        except HerdrRunError as exc:
            return f"tab close failed: {exc}"
        return Eviction(candidate=candidate, reason=reason)
    finally:
        lock.close()


def _require_unsplit(candidate: Candidate) -> str | None:
    if candidate.tab_panes != 1:
        return f"tab {candidate.tab_id} holds {candidate.tab_panes} panes; only an unsplit tab is replaced"
    return None


def _judge_shell(client: HerdrClient, config: Config, candidate: Candidate, proc_root: str) -> tuple[bool, str]:
    try:
        info = client.process_info(candidate.pane_id)
    except HerdrRunError as exc:
        return False, f"process-info failed: {exc}"
    return judge_idle(assess_process(info, config), scan_session(info.shell_pid, proc_root))


@dataclass(frozen=True)
class Survey:
    """What the cap path would find if a new agent arrived now."""

    #: Panes whose tab the cap path could replace, least-recently-used first.
    replaceable: list[str]
    #: Every other pane, with the reason its tab would stay open.
    kept: list[tuple[str, str]]


def survey(
    client: HerdrClient,
    config: Config,
    panes: Sequence[Pane],
    records: Sequence[Mapping[str, object]],
    proc_root: str = "/proc",
) -> Survey:
    """Judge every tab as :func:`evict_one` would, and close nothing.

    The pane lock is not taken: a report must not make a runner wait, and the lock is held only
    while a command runs, when the shell does not own the foreground anyway. So this is a reading
    of the moment, not a promise; the cap path decides again, under the lock, before any close.
    """
    replaceable: list[str] = []
    kept: list[tuple[str, str]] = []
    for candidate in lru_candidates(panes, records):
        refusal = _require_unsplit(candidate)
        if refusal is None:
            idle, reason = _judge_shell(client, config, candidate, proc_root)
            refusal = None if idle else reason
        if refusal is None:
            replaceable.append(candidate.pane_id)
        else:
            kept.append((candidate.pane_id, refusal))
    return Survey(replaceable=replaceable, kept=kept)


def survey_workspace(
    client: HerdrClient,
    config: Config,
    records: Sequence[Mapping[str, object]],
    proc_root: str = "/proc",
) -> Survey:
    """:func:`survey` the workspace ``config`` names, listed as the cap path lists it."""
    workspace_id = client.workspace_id_for_label(config.workspace)
    if workspace_id is None:
        raise HerdrUnavailable(f"herdr has no workspace labelled {quote(config.workspace)}")
    return survey(client, config, client.panes(workspace_id), records, proc_root)


def _log_eviction(config: Config, workspace_id: str, agent: str, eviction: Eviction) -> None:
    candidate = eviction.candidate
    last_run_at = run_time(candidate.last_run) if candidate.last_run is not None else None
    output.diagnostic(
        f"herdr-run: replaced idle tab {candidate.tab_id} (pane {candidate.pane_id}, agent "
        f"{candidate.agent or 'unknown'}, last run {last_run_at or 'none recorded'}) to make room "
        f"for '{agent}'"
    )
    path = audit.audit_path(config.project_root, config.spool_dir)
    if not audit.record(
        path,
        agent=agent,
        command=f"tab close {candidate.tab_id}",
        verdict="EVICTED",
        detail=eviction.reason,
        fields={
            "pane_id": candidate.pane_id,
            "tab_id": candidate.tab_id,
            "workspace_id": workspace_id,
            "evicted_agent": candidate.agent,
            "last_run": candidate.last_run,
            "last_run_at": last_run_at,
        },
    ):
        output.diagnostic(f"herdr-run: WARNING: could not append audit record to {path}")


def describe_skipped(skipped: Sequence[tuple[str, str]]) -> str:
    """Describe the tabs that stayed open, for the cap refusal."""
    parts = [f"pane {pane}: {reason}" for pane, reason in skipped[:_REFUSAL_DETAIL_LIMIT]]
    if len(skipped) > _REFUSAL_DETAIL_LIMIT:
        parts.append(f"and {len(skipped) - _REFUSAL_DETAIL_LIMIT} more")
    return f"None of its {len(skipped)} tab(s) could be replaced: {'; '.join(parts)}."
