"""Collect the evidence :mod:`herdr_run.reap` decides on, from the run spool and the live session.

:mod:`herdr_run.reap` deliberately holds no I/O, so that its policy can be exercised against
planted-stale and planted-live populations without a Herdr server. This module is the other half:
it gathers the evidence and hands it over. Keeping the two apart is what makes "the reaper spared
everything" testable rather than a claim.

WHERE THE CANDIDATE SET COMES FROM
----------------------------------
From **our own run spool**, not from ``herdr pane list``. Every run herdr-run has performed wrote a
``meta.json`` naming the pane it used, the workspace and tab labels it resolved, and the agent it
ran for. That is a record we authored, so a pane appearing in it is a pane *this tool* opened —
which is exactly the population the tab leak is made of. Enumerating live panes instead would sweep
up tabs a human opened in the same workspace and make scope a matter of pattern-matching a label.

**AND THE CANDIDATE SET IS BOUNDED BY OUR OWN RETENTION.** :func:`herdr_run.retention.prune_runs`
deletes a run directory, ``meta.json`` included, ``retention_days`` (default 4) after the run
finished. A tab whose owning agent last ran a week ago therefore has no surviving record and is not
a candidate at all — while still holding a pane and still counting against ``max_panes``. The
oldest leaks, which are the ones the cap exists to bound, are precisely the ones this sweep cannot
see. That is a real gap, not a rounding error, and it is why ``herdr-run reap`` prints the window
alongside the counts instead of letting "considered: 3" imply it looked at everything. Closing it
properly needs a pane ledger that outlives output retention, which is a separate change; widening
the candidate set to ``herdr pane list`` is NOT the answer, because scope would then be a matter of
pattern-matching a label on tabs we may not have opened.

SCOPE IS RE-DERIVED, NOT READ BACK
----------------------------------
A recorded tab label is not taken as proof that the tab is ours. The label is recomputed from the
CURRENT configuration and the recorded agent name, and must match exactly; the recorded workspace
label must equal the configured one. So retargeting ``workspace`` or ``tab_name`` immediately takes
the old tabs out of scope, rather than leaving this tool authorised over tabs it would no longer
create.

WHAT "THE SHELL IS GONE" MEANS HERE
-----------------------------------
Herdr still lists the pane, and ``pane process-info`` still names a shell pid, but ``/proc`` says no
such process exists. That is the husk case: the tab outlived the shell that owned it. Any other
outcome -- ``/proc`` unreadable, the pane no longer listed, the control call failing -- is UNKNOWN,
because only positive absence may authorise closing a tab.
"""

from __future__ import annotations

import os
from collections.abc import Iterable, Sequence
from dataclasses import dataclass

from herdr_run.client import HerdrClient
from herdr_run.config import Config
from herdr_run.errors import HerdrRunError
from herdr_run.identity import current_boot_id, probe_process
from herdr_run.reap import PaneEvidence, ProcessIdentity, ReapPlan, evidence_from_runs, plan_reap
from herdr_run.retention import runs_root
from herdr_run.runner import read_exit_code
from herdr_run.session import tab_label_for
from herdr_run.textfmt import quote, strict_json_loads

__all__ = ["load_run_records", "pane_ids_in", "build_evidence", "sweep"]

#: Largest ``meta.json`` accepted. Run metadata is a few kilobytes; the bound keeps one corrupted
#: spool entry from turning a sweep into an unbounded read.
_MAX_META_BYTES = 1 << 20


def load_run_records(config: Config) -> tuple[dict[str, object], ...]:
    """Parse every readable ``meta.json`` under the run spool, oldest run directory first.

    Unreadable or malformed entries are skipped rather than raising: the spool is operational
    output, and one truncated file must not stop the sweep from reporting on every other pane.
    Directory names are timestamp-prefixed, so sorting them orders the records by run time -- which
    is what lets the LAST record for a pane be treated as its current scope.
    """
    root = runs_root(config.spool_dir, config.project_root)
    try:
        # By raw bytes, as the native edition sorts: a name that is not UTF-8 is held as surrogate
        # escapes here, and code-point order would place it differently.
        entries = sorted(os.listdir(root), key=os.fsencode)
    except OSError:
        return ()
    records: list[dict[str, object]] = []
    for name in entries:
        path = os.path.join(root, name, "meta.json")
        try:
            with open(path, "rb") as handle:
                payload = handle.read(_MAX_META_BYTES + 1)
            if len(payload) > _MAX_META_BYTES:
                continue
            document: object = strict_json_loads(payload.decode("utf-8"))
        except (OSError, UnicodeError, ValueError):
            continue
        if not isinstance(document, dict):
            continue
        record: dict[str, object] = document
        _resolve_late_completion(record, os.path.join(root, name))
        records.append(record)
    return tuple(records)


def _resolve_late_completion(record: dict[str, object], directory: str) -> None:
    """Fill in an ``exit_code`` the run's own timeout could not wait for.

    A run that timed out is recorded with a null exit code, which the policy reads as IN FLIGHT.
    That is correct at the moment it is written and wrong forever afterwards: the command keeps
    running in the pane and eventually writes ``exit_code``, which is the completion signal the
    runner was waiting for. Reading it here is what stops one timeout from making a pane
    permanently unreapable -- the safe direction, but still a leak.
    """
    if record.get("exit_code") is not None:
        return
    # The same marker rule the runner and retention use: an unreadable or undecodable marker means
    # "not completed", never a crash.
    code = read_exit_code(os.path.join(directory, "exit_code"))
    if code is not None:
        record["exit_code"] = code


def pane_ids_in(records: Iterable[dict[str, object]]) -> tuple[str, ...]:
    """Every distinct pane id named by these records, in first-seen order."""
    seen: list[str] = []
    for record in records:
        pane_id = record.get("pane_id")
        if isinstance(pane_id, str) and pane_id and pane_id not in seen:
            seen.append(pane_id)
    return tuple(seen)


def _scope_of(
    pane_id: str, records: Sequence[dict[str, object]], config: Config
) -> tuple[bool, str | None, str | None, str | None]:
    """Decide scope for one pane, returning ``(in_scope, tab_id, tab_label, workspace_label)``.

    The last record naming the pane wins: a pane id can be reused by a later incarnation, and the
    most recent run is the only one whose labels describe the tab as it is now.
    """
    latest: dict[str, object] | None = None
    for record in records:
        if record.get("pane_id") == pane_id:
            latest = record
    if latest is None:
        return False, None, None, None
    # Fail closed on shape: `tab` and `workspace` must be objects and every label a string (or
    # absent). Any other shape is out of scope with no labels shown, because a record that does
    # not say clearly which tab it ran in cannot authorise anything about one.
    tab = latest.get("tab")
    workspace = latest.get("workspace")
    if not isinstance(tab, dict) or not isinstance(workspace, dict):
        return False, None, None, None
    fields = (tab.get("id"), tab.get("label"), workspace.get("label"), latest.get("agent"))
    if not all(value is None or isinstance(value, str) for value in fields):
        return False, None, None, None
    tab_id, tab_label, workspace_label, agent = (
        value if isinstance(value, str) else None for value in fields
    )
    if workspace_label != config.workspace or agent is None or tab_label is None:
        return False, tab_id, tab_label, workspace_label
    try:
        expected = tab_label_for(config, agent)
    except HerdrRunError:
        return False, tab_id, tab_label, workspace_label
    return tab_label == expected, tab_id, tab_label, workspace_label


def build_evidence(
    client: HerdrClient,
    config: Config,
    *,
    records: Sequence[dict[str, object]] | None = None,
    proc_root: str = "/proc",
) -> tuple[PaneEvidence, ...]:
    """Gather one :class:`PaneEvidence` per pane this tool has run a command in."""
    spool = load_run_records(config) if records is None else tuple(records)
    boot = current_boot_id(proc_root=proc_root)
    pane_ids = pane_ids_in(spool)
    scopes = tuple(_scope_of(pane_id, spool, config) for pane_id in pane_ids)

    live_pane_ids: frozenset[str] = frozenset()
    listing_error: str | None = None
    if any(scope[0] for scope in scopes):
        try:
            workspace_id = client.workspace_id_for_label(config.workspace)
            if workspace_id is None:
                # No workspace by that label means no listing was obtained. Saying nothing here
                # would leave every pane GONE, telling an operator the tabs are already closed --
                # the opposite of what happened.
                listing_error = f"herdr has no workspace labelled {quote(config.workspace)}"
            else:
                live_pane_ids = frozenset(pane.pane_id for pane in client.panes(workspace_id))
        except HerdrRunError as exc:
            # Not fatal, and NOT an empty listing: "herdr did not answer" must not be read as
            # "every pane is gone", which is the one mistake that would close the workspace.
            listing_error = str(exc)

    evidence: list[PaneEvidence] = []
    for pane_id, (in_scope, tab_id, tab_label, workspace_label) in zip(pane_ids, scopes):
        flags, recorded = evidence_from_runs(pane_id, spool)
        live: ProcessIdentity | None = None
        error = listing_error
        known = pane_id in live_pane_ids
        if in_scope and error is None and known:
            live, error = _live_shell(client, pane_id, boot, proc_root)
        evidence.append(
            PaneEvidence(
                pane_id=pane_id,
                tab_id=tab_id,
                tab_label=tab_label,
                workspace_label=workspace_label,
                in_scope=in_scope,
                run_exit_codes_recorded=flags,
                recorded_shell=recorded,
                live_shell=live,
                current_boot_id=boot,
                pane_known_to_herdr=known,
                evidence_error=error,
            )
        )
    return tuple(evidence)


def _live_shell(
    client: HerdrClient, pane_id: str, boot: str | None, proc_root: str
) -> tuple[ProcessIdentity | None, str | None]:
    """Bind the pane's CURRENT shell, or say why we could not.

    ``None`` for the identity means the shell is positively gone -- the only reading the policy may
    turn into a STALE verdict. Everything inconclusive comes back as an error string instead.
    """
    try:
        info = client.process_info(pane_id)
    except HerdrRunError as exc:
        return None, f"cannot read process info for {pane_id}: {exc}"
    probe = probe_process(info.shell_pid, proc_root=proc_root)
    if probe.error is not None:
        return None, probe.error
    if probe.gone:
        return None, None
    return ProcessIdentity(pid=info.shell_pid, boot_id=boot, start_ticks=probe.start_ticks), None


@dataclass(frozen=True)
class Occupancy:
    """What holds ``max_panes``: herdr's live pane list, split by whether a run record names the pane.

    The cap counts every pane herdr lists in the workspace, while ``reap`` can judge only the panes a
    surviving run record names. ``without_record`` is the difference: panes holding a share of the
    cap that no verdict covers and only a human can close. All three counts are ``None`` when the
    listing failed, because a failed listing is not an empty workspace.
    """

    live_panes: int | None
    with_record: int | None
    listing_error: str | None

    @property
    def without_record(self) -> int | None:
        """Live panes no run record names: they hold a share of the cap and only a human can close them."""
        if self.live_panes is None or self.with_record is None:
            return None
        return self.live_panes - self.with_record


def measure_occupancy(
    client: HerdrClient, config: Config, records: Sequence[dict[str, object]]
) -> Occupancy:
    """List the workspace the way the pane cap does, and count the panes no run record names."""
    try:
        workspace_id = client.workspace_id_for_label(config.workspace)
        if workspace_id is None:
            return Occupancy(
                None, None, f"herdr has no workspace labelled {quote(config.workspace)}"
            )
        panes = client.panes(workspace_id)
    except HerdrRunError as exc:
        return Occupancy(None, None, str(exc))
    recorded = set(pane_ids_in(records))
    return Occupancy(len(panes), sum(1 for pane in panes if pane.pane_id in recorded), None)


def sweep(
    client: HerdrClient,
    config: Config,
    *,
    records: Sequence[dict[str, object]] | None = None,
    proc_root: str = "/proc",
) -> ReapPlan:
    """Gather evidence and run the reaping policy over it. Decides only; closes nothing."""
    return plan_reap(build_evidence(client, config, records=records, proc_root=proc_root))
