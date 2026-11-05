"""Reaping policy: both directions, with the confusable cases planted explicitly.

The negative half deliberately does NOT use an obviously-busy tab. A reaper that spares a tab with
a command visibly running proves nothing -- the cases that matter are the ones that look exactly
like a dead tab: an agent thinking silently, and a PID that has been recycled.

Every test asserts a COUNT as well as a verdict. "Reaped 0" from a correct policy and "reaped 0"
from an inert one are indistinguishable otherwise, which is why the planted-positive test exists at
all.
"""

from __future__ import annotations

from herdr_run.reap import (
    PaneEvidence,
    ProcessIdentity,
    Verdict,
    evidence_from_runs,
    plan_reap,
)

BOOT = "boot-aaaa"


def ident(pid: int, *, boot: str = BOOT, ticks: int = 1000) -> ProcessIdentity:
    return ProcessIdentity(pid=pid, boot_id=boot, start_ticks=ticks)


def scoped(**kw: object) -> PaneEvidence:
    base: dict[str, object] = {
        "pane_id": "wE:p1",
        "tab_id": "wE:t1",
        "tab_label": "widget-w1",
        "workspace_label": "agent-cmds",
        "in_scope": True,
        "run_exit_codes_recorded": (True,),
        "recorded_shell": ident(4242),
        "live_shell": None,
        "current_boot_id": BOOT,
    }
    base.update(kw)
    return PaneEvidence(**base)  # type: ignore[arg-type]


# --- POSITIVE CONTROL: the detector must actually fire -----------------------------------------


def test_planted_stale_tab_is_reaped() -> None:
    """All runs finished, pane shell gone, identity bound. This must be selected."""
    plan = plan_reap([scoped()])
    assert plan.counts()["STALE"] == 1
    assert len(plan.reapable) == 1
    assert plan.reapable[0].pane_id == "wE:p1"
    assert "is gone" in plan.reapable[0].reason


# --- NEGATIVE: the confusable cases, one per failure mode --------------------------------------


def test_agent_thinking_is_not_reaped() -> None:
    """A run with no recorded exit_code is IN FLIGHT.

    This is the case that defeats absence-of-output detectors: silent, but working. Note the shell
    is ALSO gone here, so the only thing standing between this tab and the axe is R1.
    """
    plan = plan_reap([scoped(run_exit_codes_recorded=(True, False), live_shell=None)])
    assert plan.counts()["IN_FLIGHT"] == 1
    assert plan.counts()["STALE"] == 0
    assert plan.reapable == ()


def test_live_shell_is_not_reaped() -> None:
    plan = plan_reap([scoped(live_shell=ident(4242))])
    assert plan.counts()["SHELL_ALIVE"] == 1
    assert plan.reapable == ()


def test_recycled_pid_is_unknown_not_stale() -> None:
    """Same PID number, different process. Must NOT be reaped, and must NOT be called stale."""
    plan = plan_reap([scoped(live_shell=ident(4242, ticks=999_999))])
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.counts()["STALE"] == 0
    assert "PID reuse" in plan.declined[0].reason


def test_different_boot_is_unknown() -> None:
    plan = plan_reap([scoped(live_shell=ident(4242, boot="boot-bbbb"))])
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.reapable == ()


def test_rebooted_host_is_unknown_even_when_recorded_pid_is_gone() -> None:
    """Every old pid disappears at reboot; absence cannot prove this live tab is stale."""
    plan = plan_reap(
        [scoped(recorded_shell=ident(4242, boot="old-boot"), current_boot_id="new-boot")]
    )
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.reapable == ()
    assert "current boot" in plan.declined[0].reason


def test_missing_current_boot_is_unknown() -> None:
    plan = plan_reap([scoped(current_boot_id=None)])
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.reapable == ()


def test_new_pane_incarnation_with_different_shell_pid_is_unknown() -> None:
    plan = plan_reap([scoped(live_shell=ident(9001, ticks=2000))])
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.reapable == ()
    assert "new pane incarnation" in plan.declined[0].reason


def test_unbound_recorded_identity_is_unknown() -> None:
    """Without boot_id+start_ticks we cannot exclude reuse, so we must not act."""
    plan = plan_reap([scoped(recorded_shell=ProcessIdentity(pid=4242))])
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.reapable == ()


def test_pane_missing_from_a_successful_listing_is_gone_not_stale() -> None:
    plan = plan_reap([scoped(pane_known_to_herdr=False)])
    assert plan.counts()["GONE"] == 1
    assert plan.counts()["UNKNOWN"] == 0
    assert plan.reapable == ()
    assert "already closed" in plan.declined[0].reason


def test_pane_missing_because_the_listing_failed_is_unknown_not_gone() -> None:
    plan = plan_reap([scoped(pane_known_to_herdr=False, evidence_error="pane list: timed out")])
    assert plan.counts()["UNKNOWN"] == 1
    assert plan.counts()["GONE"] == 0


def test_out_of_scope_beats_gone() -> None:
    plan = plan_reap([scoped(pane_known_to_herdr=False, in_scope=False)])
    assert plan.counts()["OUT_OF_SCOPE"] == 1


def test_evidence_error_is_unknown() -> None:
    plan = plan_reap([scoped(evidence_error="/proc unreadable")])
    assert plan.counts()["UNKNOWN"] == 1
    assert "/proc unreadable" in plan.declined[0].reason


def test_out_of_scope_tab_is_never_considered() -> None:
    """A human's tab, idle and dead-looking, is still not ours to close."""
    plan = plan_reap(
        [scoped(in_scope=False, workspace_label="someone-else", tab_label="scratch")]
    )
    assert plan.counts()["OUT_OF_SCOPE"] == 1
    assert plan.counts()["STALE"] == 0
    assert plan.reapable == ()


# --- BOTH SIDES IN ONE POPULATION, with counts -------------------------------------------------


def test_mixed_population_reports_counts_on_both_sides() -> None:
    plan = plan_reap(
        [
            scoped(pane_id="wE:p1"),  # stale
            scoped(pane_id="wE:p2", live_shell=ident(4242)),  # alive
            scoped(pane_id="wE:p3", run_exit_codes_recorded=(False,)),  # thinking
            scoped(pane_id="wE:p4", live_shell=ident(4242, ticks=7)),  # recycled
            scoped(pane_id="wE:p5", in_scope=False),  # not ours
            scoped(pane_id="wE:p6", pane_known_to_herdr=False),  # tab already closed
        ]
    )
    counts = plan.counts()
    assert counts["considered"] == 6
    assert counts["GONE"] == 1
    assert counts["STALE"] == 1
    assert counts["SHELL_ALIVE"] == 1
    assert counts["IN_FLIGHT"] == 1
    assert counts["UNKNOWN"] == 1
    assert counts["OUT_OF_SCOPE"] == 1
    assert {d.pane_id for d in plan.reapable} == {"wE:p1"}
    # Every declined pane carries a reason; "declined silently" is the failure mode to prevent.
    assert all(d.reason for d in plan.declined)


# --- the run-spool fold, including the measured trap -------------------------------------------


def test_run_fold_marks_missing_exit_code_as_unfinished() -> None:
    flags, identity = evidence_from_runs(
        "wE:p1",
        [
            {"pane_id": "wE:p1", "exit_code": 0, "readiness": {"shell_pid": 897490}},
            {"pane_id": "wE:p1", "exit_code": None, "readiness": {"shell_pid": 897490}},
            {"pane_id": "wE:p9", "exit_code": 0},  # different pane, ignored
        ],
    )
    assert flags == (True, False)
    assert identity is not None and identity.pid == 897490
    # No boot_id/start_ticks recorded yet, so identity is unbound and the policy must say UNKNOWN.
    assert not identity.is_bound()
    plan = plan_reap([scoped(run_exit_codes_recorded=flags, recorded_shell=identity)])
    assert plan.counts()["IN_FLIGHT"] == 1


def test_cache_hits_do_not_manufacture_a_candidate() -> None:
    """A from_cache run never launched anything, so it is not evidence a pane exists."""
    flags, identity = evidence_from_runs(
        "wE:p1",
        [{"pane_id": "wE:p1", "exit_code": 0, "from_cache": True, "readiness": {"shell_pid": 1}}],
    )
    assert flags == ()
    assert identity is None


def test_run_fold_rejects_conflicting_pane_shell_incarnations() -> None:
    flags, identity = evidence_from_runs(
        "wE:p1",
        [
            {
                "pane_id": "wE:p1",
                "exit_code": 0,
                "readiness": {
                    "shell_pid": 100,
                    "boot_id": BOOT,
                    "shell_start_ticks": 10,
                },
            },
            {
                "pane_id": "wE:p1",
                "exit_code": 0,
                "readiness": {
                    "shell_pid": 200,
                    "boot_id": BOOT,
                    "shell_start_ticks": 20,
                },
            },
        ],
    )
    assert flags == (True, True)
    assert identity is None, "a recycled pane id must not inherit the first shell's authority"


def test_boolean_pid_is_not_a_bound_process_identity() -> None:
    assert not ProcessIdentity(pid=True, boot_id=BOOT, start_ticks=1).is_bound()
