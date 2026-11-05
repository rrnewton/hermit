"""Tests for the pure fusion: per-PR actions, ordering, freshness, batch, diagnostics."""

from __future__ import annotations

import dataclasses
from collections.abc import Sequence
from pr_landing_planner.graph import held_reasons

from pr_landing_planner.model import (
    CiState,
    CiVerdict,
    ConflictEdge,
    HeldPr,
    OrderingEdge,
    PrAction,
    PrActionDecision,
    PrNode,
    PolicyClass,
    RedClass,
    ValidationAuthority,
    ValidationEvidence,
)
from pr_landing_planner.plan import compute_plan


def _verdict(state: CiState, red: RedClass | None = None, gate_ok: bool = True) -> CiVerdict:
    return CiVerdict(state, red, gate_present=True, gate_ok=gate_ok, gate_missing_run=False)


def _node(
    number: int,
    *,
    state: CiState = CiState.GREEN,
    red: RedClass | None = None,
    behind: int = 0,
    files: frozenset[str] = frozenset(),
    priority: int = 0,
    size: int = 0,
) -> PrNode:
    return PrNode(
        number=number,
        head_ref=f"feat-{number}",
        base_ref="integration",
        head_sha=f"sha-{number}",
        base_sha="base",
        files=files,
        commits_behind=behind,
        ci=_verdict(state, red, gate_ok=state is CiState.PASSED),
        priority=priority,
        additions=size,
    )


def _action(decisions: Sequence[PrActionDecision], pr: int) -> PrAction:
    for d in decisions:
        if d.pr == pr:
            return d.action
    raise AssertionError(f"no decision for #{pr}")


def test_fusion_table_actions() -> None:
    nodes = [
        _node(1, state=CiState.GREEN),
        _node(2, state=CiState.GREEN, behind=5),
        _node(3, state=CiState.RED, red=RedClass.STALE_REQUIRED_CHECK),
        _node(4, state=CiState.RED, red=RedClass.FLAKY),
        _node(5, state=CiState.RED, red=RedClass.REAL),
        _node(6, state=CiState.RED, red=RedClass.EVALUATE_ONCE_RACE),
        _node(7, state=CiState.RED, red=RedClass.RUNNER_OUTAGE),
        _node(8, state=CiState.NO_RESULT),
        _node(9, state=CiState.NO_RESULT),
    ]
    # freshness_max_behind is explicit because this table is about CI state, not
    # freshness: being behind is no longer a reroute reason by default, and node 2
    # exists here to keep REBASE_THEN_LAND covered in the table.
    plan, _ = compute_plan(nodes, [], [], [], freshness_max_behind=0)
    acts = plan.per_pr_actions
    assert _action(acts, 1) is PrAction.LAND_NOW
    assert _action(acts, 2) is PrAction.REBASE_THEN_LAND
    assert _action(acts, 3) is PrAction.REFIRE_STALE_GATE
    assert _action(acts, 4) is PrAction.REFIRE_CI
    assert _action(acts, 5) is PrAction.HOLD_FIX
    assert _action(acts, 6) is PrAction.WAIT
    assert _action(acts, 7) is PrAction.ESCALATE_RUNNER_OUTAGE
    assert _action(acts, 8) is PrAction.REFIRE_CI
    assert _action(acts, 9) is PrAction.REFIRE_CI


def test_freshness_threshold() -> None:
    """The knob still works; the DEFAULT no longer applies it.

    Being behind the base is not by itself a rebase reason -- see the
    OWNER-APPROVED RULE block in the consuming repository, referenced from
    DEFAULT_FRESHNESS_MAX_BEHIND. A caller that wants a bound still gets one.

    This covers strictly more than the previous version did: when the default was
    0, the default case and the explicit-0 case were indistinguishable, so neither
    asserted that the caller's value was the one being applied.
    """
    nodes = [_node(1, behind=3)]
    plan_default, _ = compute_plan(nodes, [], [], [])
    assert _action(plan_default.per_pr_actions, 1) is PrAction.LAND_NOW
    plan_strict, _ = compute_plan(nodes, [], [], [], freshness_max_behind=0)
    assert _action(plan_strict.per_pr_actions, 1) is PrAction.REBASE_THEN_LAND
    plan_loose, _ = compute_plan(nodes, [], [], [], freshness_max_behind=5)
    assert _action(plan_loose.per_pr_actions, 1) is PrAction.LAND_NOW


def test_passing_non_gate_ci_never_lands_without_a_passing_required_gate() -> None:
    missing = _node(10)
    missing = dataclasses.replace(
        missing,
        ci=CiVerdict(
            CiState.PASSED,
            None,
            gate_present=False,
            gate_ok=False,
            gate_missing_run=False,
        ),
    )
    neutral = _node(11)
    neutral = dataclasses.replace(
        neutral,
        ci=CiVerdict(
            CiState.PASSED,
            None,
            gate_present=True,
            gate_ok=False,
            gate_missing_run=False,
        ),
    )
    plan, _ = compute_plan([missing, neutral], [], [], [])
    assert _action(plan.per_pr_actions, 10) is PrAction.REFIRE_CI
    assert _action(plan.per_pr_actions, 11) is PrAction.REFIRE_CI


def test_gate_policy_escalation_precedes_hold_state() -> None:
    policy = dataclasses.replace(_node(12), policy_class=PolicyClass.GATE_POLICY)
    plan, _ = compute_plan([policy], [], [], [HeldPr(12, ("draft",))])
    assert _action(plan.per_pr_actions, 12) is PrAction.ESCALATE_GATE_POLICY
    assert plan.land_now == ()


def test_main_advancing_does_not_by_itself_require_a_rebase() -> None:
    """Main moving under a validated branch is not a landing obstacle.

    A clean validate record with soft-green authority lands from an ancestor. The
    rebase-then-land wording is retained and still asserted, but only for a caller
    that explicitly asks for a freshness bound.
    """
    validated = dataclasses.replace(
        _node(13, behind=2),
        validation_evidence=ValidationEvidence.CLEAN_VALIDATE_RECORD,
        validation_authority=ValidationAuthority.SOFT_GREEN,
    )
    default_plan, _ = compute_plan([validated], [], [], [])
    assert default_plan.per_pr_actions[0].action is PrAction.LAND_NOW

    plan, _ = compute_plan([validated], [], [], [], freshness_max_behind=0)
    decision = plan.per_pr_actions[0]
    assert decision.action is PrAction.REBASE_THEN_LAND
    assert "soft-green authority" in decision.why
    assert "rebase 2 commit(s), then land" in decision.why
    assert "without pre-landing revalidation" in decision.why
    assert "post-facto validation remains due" in decision.why

    fresh_plan, _ = compute_plan(
        [dataclasses.replace(validated, commits_behind=0)], [], [], [], freshness_max_behind=0
    )
    assert fresh_plan.per_pr_actions[0].action is PrAction.LAND_NOW

    unproven = dataclasses.replace(
        validated, validation_authority=ValidationAuthority.NONE
    )
    unproven_plan, _ = compute_plan([unproven], [], [], [])
    unproven_decision = unproven_plan.per_pr_actions[0]
    assert unproven_decision.action is PrAction.WAIT
    assert "no consuming-workspace hard/soft-green authority" in unproven_decision.why


def test_held_actions() -> None:
    nodes = [_node(1), _node(2), _node(3)]
    held = [
        HeldPr(1, ("draft",)),
        HeldPr(2, ("local-base-conflict",)),
        HeldPr(3, ("depends-on-held:#1",)),
    ]
    plan, _ = compute_plan(nodes, [], [], held)
    assert _action(plan.per_pr_actions, 1) is PrAction.WAIT
    assert _action(plan.per_pr_actions, 2) is PrAction.REBASE_THEN_LAND
    assert _action(plan.per_pr_actions, 3) is PrAction.WAIT
    # Held PRs never appear in land_now or the parallel-safe groups.
    assert plan.land_now == ()
    assert all(1 not in g and 2 not in g and 3 not in g for g in plan.parallel_safe_groups)
def test_unavailable_review_evidence_waits_before_base_conflict_rebase() -> None:
    local = dataclasses.replace(
        _node(4),
        review_evidence_unavailable=True,
        base_conflict_paths=("src/conflict.rs",),
        validation_evidence=ValidationEvidence.CLEAN_VALIDATE_RECORD,
        validation_authority=ValidationAuthority.HARD_GREEN,
    )
    github = dataclasses.replace(
        _node(5),
        review_evidence_unavailable=True,
        mergeable="CONFLICTING",
        validation_evidence=ValidationEvidence.CLEAN_VALIDATE_RECORD,
        validation_authority=ValidationAuthority.HARD_GREEN,
    )
    nodes = (local, github)
    held = held_reasons(nodes, ())
    plan, _ = compute_plan(nodes, (), (), held)

    by_pr = {decision.pr: decision for decision in plan.per_pr_actions}
    assert by_pr[4].action is PrAction.WAIT
    assert by_pr[5].action is PrAction.WAIT
    assert "review-evidence-unavailable" in by_pr[4].why
    assert "local-base-conflict" in by_pr[4].why
    assert "review-evidence-unavailable" in by_pr[5].why
    assert "github-base-conflicting" in by_pr[5].why
    assert plan.land_now == ()




def test_land_now_and_order_priority() -> None:
    nodes = [
        _node(1, priority=1, size=5),
        _node(2, priority=0, size=50),
        _node(3, priority=0, size=1),
    ]
    plan, _ = compute_plan(nodes, [], [], [])
    assert set(plan.land_now) == {1, 2, 3}
    # order ranks priority asc, then size asc: #3 (p0,sz1), #2 (p0,sz50), #1 (p1,sz5)
    assert plan.order == (3, 2, 1)


def test_batch_is_conflict_free_green_subset() -> None:
    nodes = [_node(1, files=frozenset({"x"})), _node(2, files=frozenset({"x"})), _node(3)]
    conflicts = [ConflictEdge(1, 2, ("x",))]
    plan, _ = compute_plan(nodes, conflicts, [], [], batch=True)
    # #1 and #2 conflict => the batch keeps only one of them plus #3.
    assert 3 in plan.batch
    assert not (1 in plan.batch and 2 in plan.batch)


def test_nonlandable_dependency_blocks_child_and_batch_excludes_all_children() -> None:
    broken_parent = _node(1, state=CiState.RED, red=RedClass.REAL)
    child = _node(2)
    plan, _ = compute_plan(
        [broken_parent, child],
        [],
        [OrderingEdge(1, 2, "base-ref")],
        [],
        batch=True,
    )
    child_decision = next(item for item in plan.per_pr_actions if item.pr == 2)
    assert child_decision.action is PrAction.WAIT
    assert "dependency #1 requires hold-fix" in child_decision.why
    assert 2 not in plan.land_now
    assert 2 not in plan.batch

    ready_parent = _node(3)
    ready_child = _node(4)
    ready, _ = compute_plan(
        [ready_parent, ready_child],
        [],
        [OrderingEdge(3, 4, "base-ref")],
        [],
        batch=True,
    )
    assert set(ready.land_now) == {3, 4}
    assert ready.batch == (3,)


def test_diagnostics_and_outage_threshold() -> None:
    nodes = [
        _node(1, state=CiState.RED, red=RedClass.RUNNER_OUTAGE),
        _node(2, state=CiState.RED, red=RedClass.RUNNER_OUTAGE),
        _node(3, state=CiState.RED, red=RedClass.STALE_REQUIRED_CHECK),
        _node(4, state=CiState.RED, red=RedClass.FLAKY),
        _node(5, state=CiState.RED, red=RedClass.REAL),
        _node(6, state=CiState.RED, red=RedClass.EVALUATE_ONCE_RACE),
    ]
    _, diag = compute_plan(nodes, [], [], [], outage_min_prs=2)
    assert diag.outage_prs == (1, 2)
    assert diag.outage_suspected is True
    assert diag.stale_gates == (3,)
    assert diag.flaky_reds == (4,)
    assert diag.real_reds == (5,)
    assert diag.evaluate_once_race == (6,)
    # One outage PR below the threshold is not "systemic".
    _, diag_one = compute_plan(nodes[1:], [], [], [], outage_min_prs=2)
    assert diag_one.outage_suspected is False
