"""PURE fusion: combine the conflict graph + CI verdicts + freshness + priority into a landing PLAN.

This is the missing fusion the predecessor tools never did: neither the conflict-graph tools nor the
CI-health tool ever computed "these PRs are conflict-free, all green, all fresh — land them now,
these reds are benign, refire them, this one is a real regression, hold it." :func:`compute_plan`
does exactly that, deterministically, from already-collected data.

Per-PR action assignment (the fusion table):

* held (base-conflict)          -> ``rebase-then-land``; caller decides validation landability
* held (draft / review / dependency cycle / depends-on-held) -> ``wait``
* gate-policy change            -> ``escalate-gate-policy``
* caller-authorized exact-head evidence -> rebase if behind, then land without pre-landing revalidation
* CI runner-outage              -> ``escalate-runner-outage``
* CI evaluate-once race         -> ``wait`` (benign; treat as pending)
* CI stale required check       -> ``refire-stale-gate``
* CI flaky red                  -> ``refire-ci``
* CI real red                   -> ``hold-fix``
* CI no-result at required gate -> ``refire-ci``
* CI green but behind base      -> ``rebase-then-land`` by default
* CI green, fresh, gate ok      -> ``land-now``

Ordering among actionable PRs follows the parallel-safe layering, which itself ranks by
priority -> diff size -> age -> PR number.
"""

from __future__ import annotations

from collections.abc import Sequence

from pr_landing_planner.graph import build_stacks, held_reasons, partition_parallel_safe
from pr_landing_planner.model import (
    CiState,
    CollectedGraph,
    ConflictEdge,
    Diagnostics,
    HeldPr,
    OrderingEdge,
    Plan,
    PlanResult,
    PrAction,
    PrActionDecision,
    PrNode,
    PolicyClass,
    RedClass,
    ValidationAuthority,
    ValidationEvidence,
)

# Default freshness policy: being behind the fetched base is NOT by itself a
# reason to rebase before landing, so no freshness reroute is applied.
#
# Follow the consuming workspace's own validation-authority rule, which states
# this with its reasoning: "A lagging ancestor is legitimate; requiring the tip
# made the verdict a property of WHEN you looked rather than of the tree."
# That is the single statement of the rule; this comment quotes it rather than
# paraphrasing it, because a paraphrase is what drifts.
#
# A default of 0 is not merely the conservative choice: it reroutes EVERY branch
# behind the base to REBASE_THEN_LAND, and on a busy repository essentially
# nothing is ever not behind, so LAND_NOW becomes close to unreachable. A bound
# nobody can satisfy does not prevent anything; it just routes every row through
# a rebase the rule does not ask for.
#
# A real conflict is a separate question, decided by the conflict graph and by
# mergeability, not by this counter. The knob is retained so a caller that
# genuinely wants a freshness bound can set one.
DEFAULT_FRESHNESS_MAX_BEHIND: int | None = None


def _held_action(reasons: Sequence[str]) -> tuple[PrAction, str]:
    if "review-evidence-unavailable" in reasons:
        return PrAction.WAIT, f"held: {', '.join(reasons)}"
    if "ordering-cycle" in reasons:
        return PrAction.WAIT, "held: ordering-cycle (resolve dependency cycle and rerun)"
    if any(
        r in ("review-required", "changes-requested")
        or r.startswith("review-decision-unknown:")
        or r.startswith("review-pass-")
        for r in reasons
    ):
        return PrAction.WAIT, f"held: {', '.join(reasons)}"
    if any(r.startswith("depends-on-held") for r in reasons):
        return PrAction.WAIT, f"held: {', '.join(reasons)}"
    if "draft" in reasons:
        return PrAction.WAIT, "held: draft (mark ready to land)"
    if any(r in ("local-base-conflict", "github-base-conflicting") for r in reasons):
        return (
            PrAction.REBASE_THEN_LAND,
            f"held: {', '.join(reasons)} — rebase onto base before landing; "
            "validation landability remains delegated to the consuming workspace",
        )
    return PrAction.WAIT, f"held: {', '.join(reasons)}"


def _ci_action(node: PrNode, freshness_max_behind: int | None) -> tuple[PrAction, str]:
    ci = node.ci
    red = ci.red_class
    if node.policy_class is PolicyClass.GATE_POLICY:
        return (
            PrAction.ESCALATE_GATE_POLICY,
            "gate-policy change requires coordinator decision; validation evidence is not approval",
        )
    if node.validation_evidence is ValidationEvidence.CLEAN_VALIDATE_RECORD:
        if node.validation_authority is ValidationAuthority.NONE:
            return (
                PrAction.WAIT,
                "clean-validate-record has no consuming-workspace hard/soft-green authority",
            )
        if (
            freshness_max_behind is not None
            and node.commits_behind > freshness_max_behind
        ):
            return (
                PrAction.REBASE_THEN_LAND,
                f"{node.validation_evidence.value} at exact head with "
                f"{node.validation_authority.value} authority; rebase "
                f"{node.commits_behind} commit(s), then land without pre-landing "
                "revalidation; post-facto validation remains due",
            )
        return (
            PrAction.LAND_NOW,
            f"{node.validation_evidence.value} at exact head with "
            f"{node.validation_authority.value} authority; "
            "no merge-gate wait",
        )
    if red is RedClass.RUNNER_OUTAGE:
        return PrAction.ESCALATE_RUNNER_OUTAGE, ci.detail
    if red is RedClass.EVALUATE_ONCE_RACE:
        return PrAction.WAIT, ci.detail
    if red is RedClass.STALE_REQUIRED_CHECK:
        return PrAction.REFIRE_STALE_GATE, ci.detail
    if red is RedClass.FLAKY:
        return PrAction.REFIRE_CI, ci.detail
    if red is RedClass.REAL:
        return PrAction.HOLD_FIX, ci.detail
    if not ci.gate_present:
        return PrAction.REFIRE_CI, "required gate has NO_RESULT (absent); re-dispatch"
    if not ci.gate_ok:
        return PrAction.REFIRE_CI, "required gate has NO_RESULT; re-dispatch"
    if ci.raw_state is CiState.NO_RESULT:
        return PrAction.WAIT, "required gate passed; another CI check has NO_RESULT"
    # GREEN.
    if freshness_max_behind is not None and node.commits_behind > freshness_max_behind:
        return (
            PrAction.REBASE_THEN_LAND,
            f"authoritative CI green, gate ok, but {node.commits_behind} commit(s) "
            "behind base; rebase before landing",
        )
    return PrAction.LAND_NOW, "authoritative CI green, gate ok"


def _greedy_conflict_free(
    numbers: Sequence[int],
    conflicts: dict[int, set[int]],
    rank: dict[int, tuple[int, int, str, int]],
) -> tuple[int, ...]:
    chosen: list[int] = []
    for number in sorted(numbers, key=lambda n: rank[n]):
        if all(peer not in conflicts.get(number, set()) for peer in chosen):
            chosen.append(number)
    return tuple(chosen)


def compute_plan(
    nodes: Sequence[PrNode],
    conflict_edges: Sequence[ConflictEdge],
    ordering_edges: Sequence[OrderingEdge],
    held: Sequence[HeldPr],
    *,
    freshness_max_behind: int | None = DEFAULT_FRESHNESS_MAX_BEHIND,
    outage_min_prs: int = 2,
    batch: bool = False,
) -> tuple[Plan, Diagnostics]:
    """Fuse everything into a :class:`Plan` + :class:`Diagnostics` (pure; deterministic)."""
    held_by_number = {h.pr: h for h in held}
    held_set = frozenset(held_by_number)

    groups = partition_parallel_safe(nodes, conflict_edges, ordering_edges, exclude=held_set)
    group_of: dict[int, int] = {
        number: idx for idx, group in enumerate(groups) for number in group
    }

    decisions: list[PrActionDecision] = []
    for node in sorted(nodes, key=lambda n: n.number):
        if node.policy_class is PolicyClass.GATE_POLICY:
            action, why = _ci_action(node, freshness_max_behind)
        elif node.number in held_by_number:
            action, why = _held_action(held_by_number[node.number].reasons)
        else:
            action, why = _ci_action(node, freshness_max_behind)
        decisions.append(
            PrActionDecision(
                pr=node.number, action=action, why=why, group=group_of.get(node.number)
            )
        )

    # A child may be independently green while its open predecessor needs repair, policy review,
    # or a rebase. Do not call that child landable until every predecessor is itself land-now.
    by_action = {decision.pr: decision.action for decision in decisions}
    blockers = sorted(ordering_edges, key=lambda edge: (edge.after, edge.before))
    changed = True
    while changed:
        changed = False
        updated: list[PrActionDecision] = []
        for decision in decisions:
            blocker = next(
                (
                    edge.before
                    for edge in blockers
                    if edge.after == decision.pr
                    and by_action.get(edge.before) is not PrAction.LAND_NOW
                ),
                None,
            )
            if blocker is not None and decision.action in (
                PrAction.LAND_NOW,
                PrAction.REBASE_THEN_LAND,
            ):
                action = by_action.get(blocker, PrAction.WAIT)
                replacement = PrActionDecision(
                    pr=decision.pr,
                    action=PrAction.WAIT,
                    why=f"dependency #{blocker} requires {action.value}; wait and rerun",
                    group=decision.group,
                )
                updated.append(replacement)
                if by_action.get(decision.pr) is not PrAction.WAIT:
                    by_action[decision.pr] = PrAction.WAIT
                    changed = True
            else:
                updated.append(decision)
        decisions = updated

    land_now = tuple(d.pr for d in decisions if d.action is PrAction.LAND_NOW)
    # Recommended act sequence: flatten the parallel-safe groups (already priority-ranked per layer).
    order = tuple(number for group in groups for number in group)

    # Optional batch: one conflict- and dependency-free set of land-now root PRs behind one gate.
    batch_prs: tuple[int, ...] = ()
    if batch and land_now:
        conflicts: dict[int, set[int]] = {}
        for edge in conflict_edges:
            conflicts.setdefault(edge.a, set()).add(edge.b)
            conflicts.setdefault(edge.b, set()).add(edge.a)
        rank = {
            n.number: (n.priority, n.size, n.created_at, n.number)
            for n in nodes
        }
        ordered_children = {edge.after for edge in ordering_edges}
        batch_prs = _greedy_conflict_free(
            tuple(number for number in land_now if number not in ordered_children),
            conflicts,
            rank,
        )

    plan = Plan(
        parallel_safe_groups=groups,
        land_now=land_now,
        order=order,
        per_pr_actions=tuple(decisions),
        batch=batch_prs,
    )

    stale_gates = tuple(
        n.number for n in nodes if n.ci.red_class is RedClass.STALE_REQUIRED_CHECK
    )
    flaky_reds = tuple(n.number for n in nodes if n.ci.red_class is RedClass.FLAKY)
    real_reds = tuple(n.number for n in nodes if n.ci.red_class is RedClass.REAL)
    evaluate_once = tuple(
        n.number for n in nodes if n.ci.red_class is RedClass.EVALUATE_ONCE_RACE
    )
    outage_prs = tuple(n.number for n in nodes if n.ci.red_class is RedClass.RUNNER_OUTAGE)
    diagnostics = Diagnostics(
        stale_gates=stale_gates,
        flaky_reds=flaky_reds,
        real_reds=real_reds,
        evaluate_once_race=evaluate_once,
        outage_prs=outage_prs,
        outage_suspected=len(outage_prs) >= outage_min_prs,
    )
    return plan, diagnostics


def assemble_result(
    graph: CollectedGraph,
    *,
    freshness_max_behind: int | None = DEFAULT_FRESHNESS_MAX_BEHIND,
    outage_min_prs: int = 2,
    batch: bool = False,
) -> PlanResult:
    """Pure end-to-end assembly: a collected graph -> stacks + held PRs + the fused plan + diagnostics."""
    stacks = build_stacks(graph.ordering_edges)
    held = held_reasons(graph.nodes, graph.ordering_edges)
    plan, diagnostics = compute_plan(
        graph.nodes,
        graph.conflict_edges,
        graph.ordering_edges,
        held,
        freshness_max_behind=freshness_max_behind,
        outage_min_prs=outage_min_prs,
        batch=batch,
    )
    return PlanResult(
        graph=graph, stacks=stacks, held=held, plan=plan, diagnostics=diagnostics
    )


__all__ = ["compute_plan", "assemble_result", "DEFAULT_FRESHNESS_MAX_BEHIND"]
