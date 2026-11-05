"""Evidence gathering for the reaper, and the process-identity binding it depends on.

The point of these tests is that the reaper must be able to FIRE. A sweep that returns "UNKNOWN,
UNKNOWN, UNKNOWN" is indistinguishable from a correct sweep over a healthy workspace, so every case
here asserts the verdict it planted, and the planted-stale case asserts a non-zero STALE count.
"""

from __future__ import annotations

import json
import os
import time
from typing import cast

from herdr_run.client import HerdrClient
from herdr_run.config import Config
from herdr_run.identity import parse_start_ticks, probe_process
from herdr_run.reap import Verdict
from herdr_run.retention import prune_runs
from herdr_run.sweep import (
    build_evidence,
    load_run_records,
    measure_occupancy,
    pane_ids_in,
    sweep,
)
from tests.herdr_fake import FakeHerdrClient

BOOT = "3f2b1c8e-0000-4000-8000-000000000001"


# --- /proc/<pid>/stat parsing ------------------------------------------------------------------


def test_start_ticks_come_from_field_22() -> None:
    # Seven tokens follow the comm (fields 3..9), then the numbers 10..22 in order, so the value
    # standing in field 22 is literally 22. Anything else means the field numbering slipped.
    line = "1234 (bash) S 1 1234 1234 0 -1 4194304 " + " ".join(str(n) for n in range(10, 23))
    assert parse_start_ticks(line) == 22


def test_a_comm_containing_spaces_and_parentheses_does_not_shift_the_fields() -> None:
    """This is why the split is at the LAST ')' and not on whitespace."""
    honest = "77 (bash) S " + " ".join(str(n) for n in range(3, 40))
    hostile = "77 (my prog (v2)) S " + " ".join(str(n) for n in range(3, 40))
    assert parse_start_ticks(hostile) == parse_start_ticks(honest)


def test_a_truncated_stat_line_is_unknown_not_a_guess() -> None:
    assert parse_start_ticks("77 (bash) S 1 2 3") is None
    assert parse_start_ticks("no parenthesis here") is None
    assert parse_start_ticks("77 (bash) S " + " ".join(["x"] * 30)) is None


def test_an_absent_process_is_gone_and_an_unreadable_one_is_not(tmp_path: object) -> None:
    """"Gone" authorises reaping; "could not tell" must never be mistaken for it."""
    proc = str(tmp_path)
    absent = probe_process(4242, proc_root=proc)
    assert absent.gone is True and absent.error is None

    os.makedirs(os.path.join(proc, "77"))
    with open(os.path.join(proc, "77", "stat"), "w", encoding="utf-8") as handle:
        handle.write("garbage without the right shape\n")
    unreadable = probe_process(77, proc_root=proc)
    assert unreadable.gone is False
    assert unreadable.error is not None


def test_a_stat_file_that_cannot_be_opened_is_unknown_not_gone(tmp_path: object) -> None:
    """The READ-error arm, which is a different branch from the parse-error arm above.

    ``open`` failing is the case where ``/proc`` exists and refuses us. Reporting that as ``gone``
    would hand the policy a death certificate written by an EACCES.
    """
    proc = str(tmp_path)
    # A directory where the kernel puts a file: opening it raises IsADirectoryError, an OSError
    # that is emphatically NOT FileNotFoundError.
    os.makedirs(os.path.join(proc, "88", "stat"))
    probe = probe_process(88, proc_root=proc)
    assert probe.gone is False, "an unopenable /proc entry must never authorise reaping"
    assert probe.error is not None and "cannot read" in probe.error
    assert probe.start_ticks is None


def test_an_implausibly_long_stat_file_is_unknown_not_gone(tmp_path: object) -> None:
    """The size-guard arm. A refusal to read is still a refusal to conclude."""
    proc = str(tmp_path)
    os.makedirs(os.path.join(proc, "99"))
    with open(os.path.join(proc, "99", "stat"), "w", encoding="utf-8") as handle:
        handle.write("99 (bash) S " + "0 " * 40000 + "\n")
    probe = probe_process(99, proc_root=proc)
    assert probe.gone is False, "an oversized /proc entry must never authorise reaping"
    assert probe.error is not None and "implausibly long" in probe.error
    assert probe.start_ticks is None


def test_a_value_that_is_not_a_process_id_is_unknown_not_gone(tmp_path: object) -> None:
    """The bad-pid arm. ``/proc/0/stat`` and ``/proc/-1/stat`` are absent for the WRONG reason.

    ``True`` is in the list because ``bool`` is a subclass of ``int`` and ``True >= 1``, so only the
    explicit ``isinstance(pid, bool)`` guard stops it reaching ``/proc/True/stat`` -- which is
    absent, and would therefore come back as a death certificate.
    """
    proc = str(tmp_path)
    for candidate in (0, -1, True):
        probe = probe_process(candidate, proc_root=proc)
        assert probe.gone is False, f"{candidate!r} must not read as a dead process"
        assert probe.error is not None and "not a process id" in probe.error


def test_this_process_can_be_bound_against_the_real_proc() -> None:
    probe = probe_process(os.getpid())
    assert probe.gone is False and probe.error is None
    assert probe.start_ticks is not None and probe.start_ticks > 0


# --- the sweep ---------------------------------------------------------------------------------


def _config(root: str, **kwargs: object) -> Config:
    base: dict[str, object] = {"project_root": root, "spool_dir": "spool"}
    base.update(kwargs)
    return Config(**base)  # type: ignore[arg-type]


def _client(fake: FakeHerdrClient) -> HerdrClient:
    return cast(HerdrClient, fake)


def _record(
    *,
    pane_id: str,
    agent: str = "kvm",
    workspace: str = "agent-cmds",
    tab_label: str | None = None,
    exit_code: int | None = 0,
    shell_pid: int = 4242,
    boot_id: str | None = BOOT,
    start_ticks: int | None = 900,
) -> dict[str, object]:
    return {
        "pane_id": pane_id,
        "agent": agent,
        "exit_code": exit_code,
        "workspace": {"label": workspace, "id": "w1"},
        "tab": {"label": agent if tab_label is None else tab_label, "id": "w1:t1"},
        "readiness": {
            "shell_pid": shell_pid,
            "boot_id": boot_id,
            "shell_start_ticks": start_ticks,
        },
    }


def _write_spool(root: str, records: list[dict[str, object]]) -> None:
    runs = os.path.join(root, "spool", "runs")
    for index, record in enumerate(records):
        directory = os.path.join(runs, f"20260819T00000{index}-agent-{index}")
        os.makedirs(directory)
        with open(os.path.join(directory, "meta.json"), "w", encoding="utf-8") as handle:
            json.dump(record, handle)


def _fake_with_pane(pane_id: str, *, shell_pid: int) -> FakeHerdrClient:
    """A live session holding exactly one workspace/tab/pane with a chosen shell pid."""
    fake = FakeHerdrClient()
    fake.create_workspace(label="agent-cmds", cwd="/tmp")
    workspace = next(iter(fake.workspaces.values()))
    tab = next(iter(workspace.tabs.values()))
    tab.label = "kvm"
    tab.pane_ids[:] = [pane_id]
    fake.shell_pids[pane_id] = shell_pid
    return fake


def _proc_with(tmp_path: str, pid: int, ticks: int) -> str:
    proc = os.path.join(tmp_path, "proc")
    os.makedirs(os.path.join(proc, "sys", "kernel", "random"), exist_ok=True)
    with open(os.path.join(proc, "sys", "kernel", "random", "boot_id"), "w", encoding="utf-8") as h:
        h.write(BOOT + "\n")
    if pid:
        os.makedirs(os.path.join(proc, str(pid)), exist_ok=True)
        with open(os.path.join(proc, str(pid), "stat"), "w", encoding="utf-8") as handle:
            handle.write(f"{pid} (bash) S " + " ".join(["0"] * 18) + f" {ticks}\n")
    return proc


def test_load_run_records_skips_a_corrupt_entry_without_losing_the_rest(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1"), _record(pane_id="w1:p2")])
    broken = os.path.join(root, "spool", "runs", "20260819T999999-broken")
    os.makedirs(broken)
    with open(os.path.join(broken, "meta.json"), "w", encoding="utf-8") as handle:
        handle.write("{not json")

    records = load_run_records(_config(root))
    assert pane_ids_in(records) == ("w1:p1", "w1:p2")


def test_load_run_records_skips_an_absurdly_large_entry(tmp_path: object) -> None:
    """The size guard, which otherwise only exists as a comment."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1"), _record(pane_id="w1:p2")])
    huge = os.path.join(root, "spool", "runs", "20260819T000000a-huge")
    os.makedirs(huge)
    with open(os.path.join(huge, "meta.json"), "w", encoding="utf-8") as handle:
        json.dump({"pane_id": "w1:pHUGE", "padding": "x" * (1 << 21)}, handle)

    records = load_run_records(_config(root))
    assert pane_ids_in(records) == ("w1:p1", "w1:p2"), "an unbounded read must not be attempted"


def test_records_arrive_in_run_order_whatever_the_directory_listing_says(
    tmp_path: object,
) -> None:
    """``sorted()`` is what makes "the LAST record wins" mean "the most recent run".

    The run directories are CREATED newest-first here, so a listing taken in creation order would
    hand the sweep the records backwards and make the oldest labels authoritative.
    """
    root = str(tmp_path)
    runs = os.path.join(root, "spool", "runs")
    for stamp, pane in (("20260819T000003", "w1:p3"), ("20260819T000001", "w1:p1")):
        directory = os.path.join(runs, f"{stamp}-agent")
        os.makedirs(directory)
        with open(os.path.join(directory, "meta.json"), "w", encoding="utf-8") as handle:
            json.dump(_record(pane_id=pane), handle)

    assert pane_ids_in(load_run_records(_config(root))) == ("w1:p1", "w1:p3")


def test_retention_takes_the_oldest_leaked_tabs_out_of_the_candidate_set(
    tmp_path: object,
) -> None:
    """Pins the known gap so it cannot become news later.

    The reaper's candidates are the panes named by SURVIVING run records, and herdr-run's own
    retention deletes those records. A tab whose agent last ran longer ago than the window is
    invisible here while still counting against ``max_panes`` -- the oldest leaks are the ones this
    sweep cannot see. ``herdr-run reap`` prints the window for exactly this reason.
    """
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:pLEAK")])
    runs = os.path.join(root, "spool", "runs")
    directory = os.path.join(runs, os.listdir(runs)[0])
    exit_code = os.path.join(directory, "exit_code")
    with open(exit_code, "w", encoding="utf-8") as handle:
        handle.write("0\n")
    ancient = time.time() - 6 * 86400
    os.utime(exit_code, (ancient, ancient))

    assert pane_ids_in(load_run_records(_config(root))) == ("w1:pLEAK",)
    result = prune_runs(runs, retention_days=4)
    assert len(result.removed) == 1
    assert pane_ids_in(load_run_records(_config(root))) == ()


def test_scope_comes_from_the_latest_record_not_the_first(tmp_path: object) -> None:
    """A pane retargeted into another workspace must lose the authority its first run had.

    Keeping the FIRST record is the unsafe direction: the tab would stay in scope on labels that
    no longer describe it, and this planted population would then be reaped.
    """
    root = str(tmp_path)
    _write_spool(
        root,
        [
            _record(pane_id="w1:p1"),
            _record(pane_id="w1:p1", workspace="someone-elses"),
        ],
    )
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)  # the shell is gone, so only scope can spare this pane

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["OUT_OF_SCOPE"] == 1
    assert plan.counts()["STALE"] == 0


def test_a_husk_pane_whose_shell_died_is_reaped(tmp_path: object) -> None:
    """POSITIVE CONTROL. Herdr still lists the tab; the shell that owned it no longer exists."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)  # no /proc/4242: the shell is positively gone

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["STALE"] == 1
    assert [decision.pane_id for decision in plan.reapable] == ["w1:p1"]


def test_a_live_shell_is_spared(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 4242, 900)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["SHELL_ALIVE"] == 1
    assert plan.reapable == ()


def test_an_unfinished_run_beats_a_dead_shell(tmp_path: object) -> None:
    """The agent-is-thinking case, planted with the shell ALSO gone so only R1 can spare it."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1"), _record(pane_id="w1:p1", exit_code=None)])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["IN_FLIGHT"] == 1
    assert plan.counts()["STALE"] == 0


def test_a_run_that_finished_after_the_timeout_stops_looking_in_flight(tmp_path: object) -> None:
    """The completion signal outlives the process that was waiting for it.

    A timed-out run is recorded with a null exit code, which is the truth when it is written. The
    command then keeps running and writes ``exit_code`` into the same directory. Reading that back
    is what stops one timeout from making the pane permanently unreapable.
    """
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1", exit_code=None)])
    runs = os.path.join(root, "spool", "runs")
    with open(os.path.join(runs, os.listdir(runs)[0], "exit_code"), "w", encoding="utf-8") as h:
        h.write("0\n")
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["IN_FLIGHT"] == 0
    assert plan.counts()["STALE"] == 1


def test_a_run_still_waiting_on_its_exit_code_stays_in_flight(tmp_path: object) -> None:
    """Negative control for the case above: no completion signal, no re-reading it into one."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1", exit_code=None)])
    runs = os.path.join(root, "spool", "runs")
    # Created but empty, as during a torn write: the command has NOT reported a status.
    with open(os.path.join(runs, os.listdir(runs)[0], "exit_code"), "w", encoding="utf-8") as h:
        h.write("")
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["IN_FLIGHT"] == 1
    assert plan.counts()["STALE"] == 0


def test_an_unreadable_proc_entry_is_unknown_not_stale(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)
    os.makedirs(os.path.join(proc, "4242"))
    with open(os.path.join(proc, "4242", "stat"), "w", encoding="utf-8") as handle:
        handle.write("truncated\n")

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.counts()["STALE"] == 0


def test_a_proc_entry_that_cannot_be_opened_is_unknown_not_stale(tmp_path: object) -> None:
    """Same verdict as the unparseable case, reached through the READ-error arm instead.

    Planted with no ``/proc/4242`` sibling, so the only thing standing between this pane and a
    STALE verdict is the refusal to read an ``open`` failure as a death.
    """
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)
    os.makedirs(os.path.join(proc, "4242", "stat"))

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.counts()["STALE"] == 0
    assert "cannot read" in plan.declined[0].reason


def test_an_oversized_proc_entry_is_unknown_not_stale(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)
    os.makedirs(os.path.join(proc, "4242"))
    with open(os.path.join(proc, "4242", "stat"), "w", encoding="utf-8") as handle:
        handle.write("4242 (bash) S " + "0 " * 40000 + "\n")

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.counts()["STALE"] == 0
    assert "implausibly long" in plan.declined[0].reason


def test_a_failing_process_info_call_is_unknown_not_stale(tmp_path: object) -> None:
    """A control call that did not answer is not a report that the shell died.

    The pane is listed and ``/proc`` holds no such pid, so every other signal points at STALE.
    Only the refusal to treat a failed ``pane process-info`` as evidence keeps the tab open.
    """
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    fake.fail_process_info = True
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["STALE"] == 0
    assert plan.counts()["UNKNOWN"] == 1
    assert "cannot read process info for w1:p1" in plan.declined[0].reason


def test_a_workspace_herdr_does_not_know_is_reported_as_a_missing_listing(
    tmp_path: object,
) -> None:
    """No workspace by that label means no listing was obtained, not that the tabs are gone."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    next(iter(fake.workspaces.values())).label = "renamed-away"
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert plan.counts()["STALE"] == 0
    assert plan.counts()["UNKNOWN"] == 1
    assert "no workspace labelled" in plan.declined[0].reason
    assert plan.counts()["GONE"] == 0
    assert "already closed" not in plan.declined[0].reason


def test_a_tab_retargeted_out_of_the_schema_is_out_of_scope(tmp_path: object) -> None:
    """Scope is recomputed from the CURRENT config, not read back from the record."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root, tab_name="{project}-{agent}"), proc_root=proc)
    assert plan.counts()["OUT_OF_SCOPE"] == 1
    assert plan.counts()["STALE"] == 0


def test_a_pane_in_another_workspace_is_out_of_scope(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1", workspace="someone-elses")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    fake.calls.clear()
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert fake.calls == []
    assert plan.counts()["OUT_OF_SCOPE"] == 1
    assert plan.reapable == ()


def test_a_pane_missing_from_herdrs_listing_is_gone(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1"), _record(pane_id="w1:pGONE")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 4242, 900)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    verdicts = {decision.pane_id: decision.verdict for decision in plan.decisions}
    assert verdicts["w1:pGONE"] == Verdict.GONE
    assert verdicts["w1:p1"] == Verdict.SHELL_ALIVE


def test_an_unanswering_herdr_reaps_nothing(tmp_path: object) -> None:
    """"herdr did not answer" must never be read as "every pane is gone"."""
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    fake.calls.clear()
    fake.fail_pane_list = True
    proc = _proc_with(root, 0, 0)

    plan = sweep(_client(fake), _config(root), proc_root=proc)
    assert fake.calls == ["workspace_id_for_label(agent-cmds)"]
    assert plan.counts()["STALE"] == 0
    assert plan.counts()["UNKNOWN"] == 1
    # And it must be UNKNOWN saying the SERVER did not answer, never GONE: GONE tells an operator
    # the tabs are already closed -- which is the opposite of what happened. Anchored on the two
    # strings
    # PRODUCTION owns: the sweep's "evidence unavailable" wrapper and the control call's own
    # purpose prefix, which HerdrClient._call puts in front of every failure it raises.
    reason = plan.declined[0].reason
    assert reason.startswith("evidence unavailable: "), reason
    assert "pane list" in reason, reason
    assert plan.counts()["GONE"] == 0
    assert "already closed" not in reason, reason


def test_an_empty_spool_reports_zero_of_everything(tmp_path: object) -> None:
    root = str(tmp_path)
    fake = FakeHerdrClient()
    plan = sweep(_client(fake), _config(root), proc_root=str(tmp_path))
    assert fake.calls == []
    counts = plan.counts()
    assert counts["considered"] == 0
    # Every verdict is present even at zero, so an inert sweep still prints its own shape.
    assert set(counts) >= {"STALE", "IN_FLIGHT", "SHELL_ALIVE", "GONE", "UNKNOWN", "OUT_OF_SCOPE"}


def test_evidence_carries_the_recorded_identity_the_policy_needs(tmp_path: object) -> None:
    root = str(tmp_path)
    _write_spool(root, [_record(pane_id="w1:p1")])
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    proc = _proc_with(root, 4242, 900)

    evidence = build_evidence(_client(fake), _config(root), proc_root=proc)
    assert len(evidence) == 1
    recorded = evidence[0].recorded_shell
    assert recorded is not None and recorded.is_bound(), "a bare pid can never authorise anything"


def _occupancy_fake() -> FakeHerdrClient:
    fake = _fake_with_pane("w1:p1", shell_pid=4242)
    tab = next(iter(next(iter(fake.workspaces.values())).tabs.values()))
    tab.pane_ids[:] = ["w1:p1", "w1:p2", "w1:p3"]
    return fake


def test_occupancy_counts_live_panes_no_record_names(tmp_path: object) -> None:
    records = [
        _record(pane_id="w1:p1"),
        # A record whose pane herdr no longer lists is not occupancy.
        _record(pane_id="w1:p9", agent="old"),
    ]
    occupancy = measure_occupancy(_client(_occupancy_fake()), _config(str(tmp_path)), records)
    assert (occupancy.live_panes, occupancy.with_record, occupancy.without_record) == (3, 1, 2)
    assert occupancy.listing_error is None


def test_occupancy_of_a_failed_listing_is_unknown_not_zero(tmp_path: object) -> None:
    fake = _occupancy_fake()
    fake.fail_pane_list = True
    occupancy = measure_occupancy(_client(fake), _config(str(tmp_path)), [])
    assert occupancy.live_panes is None
    assert occupancy.without_record is None
    assert occupancy.listing_error is not None and "pane list" in occupancy.listing_error


def test_occupancy_of_a_missing_workspace_names_it(tmp_path: object) -> None:
    occupancy = measure_occupancy(_client(FakeHerdrClient()), _config(str(tmp_path)), [])
    assert occupancy.with_record is None
    assert occupancy.listing_error == 'herdr has no workspace labelled "agent-cmds"'
