//! Pure fusion of graph, freshness, evidence, and CI into recommended actions.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::{build_stacks, held_reasons, partition_parallel_safe};
use crate::model::{
    CiState, CollectedGraph, ConflictEdge, Diagnostics, HeldPr, OrderingEdge, Plan, PlanResult,
    PolicyClass, PrAction, PrActionDecision, PrNode, RedClass, ValidationAuthority,
    ValidationEvidence,
};

/// Default freshness policy: being behind the fetched base is NOT by itself a
/// reason to rebase before landing, so no freshness reroute is applied.
///
/// Follow the consuming workspace's own validation-authority rule, which states
/// this with its reasoning: "A lagging ancestor is legitimate; requiring the tip
/// made the verdict a property of WHEN you looked rather than of the tree."
/// That is the single statement of the rule; this comment quotes it rather than
/// paraphrasing it, because a paraphrase is what drifts.
///
/// A default of `Some(0)` is not merely the conservative choice: it reroutes
/// EVERY branch behind the base to `RebaseThenLand`, and on a busy repository
/// essentially nothing is ever not behind, so `LandNow` becomes close to
/// unreachable. A bound nobody can satisfy does not prevent anything; it just
/// routes every row through a rebase the rule does not ask for.
///
/// A real conflict is a separate question, decided by the conflict graph and by
/// mergeability, not by this counter. The knob is retained so a caller that
/// genuinely wants a freshness bound can set one.
pub const DEFAULT_FRESHNESS_MAX_BEHIND: Option<i64> = None;

fn held_action(reasons: &[String]) -> (PrAction, String) {
    if reasons
        .iter()
        .any(|reason| reason == "review-evidence-unavailable")
    {
        return (PrAction::Wait, format!("held: {}", reasons.join(", ")));
    }
    if reasons.iter().any(|reason| reason == "ordering-cycle") {
        return (
            PrAction::Wait,
            "held: ordering-cycle (resolve dependency cycle and rerun)".to_owned(),
        );
    }
    if reasons.iter().any(|reason| {
        matches!(reason.as_str(), "review-required" | "changes-requested")
            || reason.starts_with("review-decision-unknown:")
            || reason.starts_with("review-pass-")
    }) {
        return (PrAction::Wait, format!("held: {}", reasons.join(", ")));
    }
    if reasons
        .iter()
        .any(|reason| reason.starts_with("depends-on-held"))
    {
        return (PrAction::Wait, format!("held: {}", reasons.join(", ")));
    }
    if reasons.iter().any(|reason| reason == "draft") {
        return (
            PrAction::Wait,
            "held: draft (mark ready to land)".to_owned(),
        );
    }
    if reasons
        .iter()
        .any(|reason| reason == "local-base-conflict" || reason == "github-base-conflicting")
    {
        return (
            PrAction::RebaseThenLand,
            format!(
                "held: {} — rebase onto base before landing; validation landability remains delegated to the consuming workspace",
                reasons.join(", ")
            ),
        );
    }
    (PrAction::Wait, format!("held: {}", reasons.join(", ")))
}

fn ci_action(node: &PrNode, freshness_max_behind: Option<i64>) -> (PrAction, String) {
    if node.policy_class == PolicyClass::GatePolicy {
        return (
            PrAction::EscalateGatePolicy,
            "gate-policy change requires coordinator decision; validation evidence is not approval"
                .to_owned(),
        );
    }
    if node.validation_evidence == ValidationEvidence::CleanValidateRecord {
        if node.validation_authority == ValidationAuthority::None {
            return (
                PrAction::Wait,
                "clean-validate-record has no consuming-workspace hard/soft-green authority"
                    .to_owned(),
            );
        }
        if freshness_max_behind.is_some_and(|limit| node.commits_behind > limit) {
            return (
                PrAction::RebaseThenLand,
                format!(
                    "{} at exact head with {} authority; rebase {} commit(s), then land without pre-landing revalidation; post-facto validation remains due",
                    node.validation_evidence.as_str(),
                    node.validation_authority.as_str(),
                    node.commits_behind,
                ),
            );
        }
        return (
            PrAction::LandNow,
            format!(
                "{} at exact head with {} authority; no merge-gate wait",
                node.validation_evidence.as_str(),
                node.validation_authority.as_str(),
            ),
        );
    }
    if let Some(red) = node.ci.red_class {
        let action = match red {
            RedClass::RunnerOutage => PrAction::EscalateRunnerOutage,
            RedClass::EvaluateOnceRace => PrAction::Wait,
            RedClass::StaleRequiredCheck => PrAction::RefireStaleGate,
            RedClass::Flaky => PrAction::RefireCi,
            RedClass::Real => PrAction::HoldFix,
        };
        return (action, node.ci.detail.clone());
    }
    if !node.ci.gate_present {
        return (
            PrAction::RefireCi,
            "required gate has NO_RESULT (absent); re-dispatch".to_owned(),
        );
    }
    if !node.ci.gate_ok {
        return (
            PrAction::RefireCi,
            "required gate has NO_RESULT; re-dispatch".to_owned(),
        );
    }
    if node.ci.raw_state == CiState::NoResult {
        return (
            PrAction::Wait,
            "required gate passed; another CI check has NO_RESULT".to_owned(),
        );
    }
    if freshness_max_behind.is_some_and(|limit| node.commits_behind > limit) {
        return (
            PrAction::RebaseThenLand,
            format!(
                "authoritative CI green, gate ok, but {} commit(s) behind base; rebase before landing",
                node.commits_behind
            ),
        );
    }
    (
        PrAction::LandNow,
        "authoritative CI green, gate ok".to_owned(),
    )
}

fn greedy_conflict_free(
    numbers: &[i64],
    conflicts: &BTreeMap<i64, BTreeSet<i64>>,
    by_number: &BTreeMap<i64, &PrNode>,
) -> Vec<i64> {
    let mut sorted = numbers.to_vec();
    sorted.sort_by_key(|number| {
        let node = by_number[number];
        (
            node.priority,
            node.size(),
            node.created_at.as_str(),
            node.number,
        )
    });
    let mut chosen = Vec::new();
    for number in sorted {
        if chosen
            .iter()
            .all(|peer| !conflicts.get(&number).is_some_and(|set| set.contains(peer)))
        {
            chosen.push(number);
        }
    }
    chosen
}

/// Fuse graph, CI, holds, evidence, freshness, and dependencies into recommendations.
pub fn compute_plan(
    nodes: &[PrNode],
    conflict_edges: &[ConflictEdge],
    ordering_edges: &[OrderingEdge],
    held: &[HeldPr],
    freshness_max_behind: Option<i64>,
    outage_min_prs: usize,
    batch: bool,
) -> (Plan, Diagnostics) {
    let held_by_number: BTreeMap<_, _> = held.iter().map(|held| (held.pr, held)).collect();
    let held_set = held_by_number.keys().copied().collect();
    let groups = partition_parallel_safe(nodes, conflict_edges, ordering_edges, &held_set);
    let group_of: BTreeMap<_, _> = groups
        .iter()
        .enumerate()
        .flat_map(|(group, numbers)| numbers.iter().map(move |number| (*number, group)))
        .collect();

    let mut sorted_nodes: Vec<_> = nodes.iter().collect();
    sorted_nodes.sort_by_key(|node| node.number);
    let mut decisions = sorted_nodes
        .into_iter()
        .map(|node| {
            let (action, why) = if node.policy_class == PolicyClass::GatePolicy {
                ci_action(node, freshness_max_behind)
            } else if let Some(held) = held_by_number.get(&node.number) {
                held_action(&held.reasons)
            } else {
                ci_action(node, freshness_max_behind)
            };
            PrActionDecision {
                pr: node.number,
                action,
                why,
                group: group_of.get(&node.number).copied(),
            }
        })
        .collect::<Vec<_>>();
    let mut by_action: BTreeMap<_, _> = decisions
        .iter()
        .map(|decision| (decision.pr, decision.action))
        .collect();
    let mut blockers = ordering_edges.to_vec();
    blockers.sort_by_key(|edge| (edge.after, edge.before));
    loop {
        let mut changed = false;
        for decision in &mut decisions {
            let blocker = blockers.iter().find_map(|edge| {
                (edge.after == decision.pr
                    && by_action.get(&edge.before) != Some(&PrAction::LandNow))
                .then_some(edge.before)
            });
            if let Some(blocker) = blocker {
                if matches!(
                    decision.action,
                    PrAction::LandNow | PrAction::RebaseThenLand
                ) {
                    let action = by_action.get(&blocker).copied().unwrap_or(PrAction::Wait);
                    decision.action = PrAction::Wait;
                    decision.why = format!(
                        "dependency #{blocker} requires {}; wait and rerun",
                        action.as_str()
                    );
                    if by_action.insert(decision.pr, PrAction::Wait) != Some(PrAction::Wait) {
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    let land_now = decisions
        .iter()
        .filter(|decision| decision.action == PrAction::LandNow)
        .map(|decision| decision.pr)
        .collect::<Vec<_>>();
    let order = groups.iter().flatten().copied().collect();
    let mut batch_prs = Vec::new();
    if batch && !land_now.is_empty() {
        let mut conflicts: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
        for edge in conflict_edges {
            conflicts.entry(edge.a).or_default().insert(edge.b);
            conflicts.entry(edge.b).or_default().insert(edge.a);
        }
        let by_number = nodes.iter().map(|node| (node.number, node)).collect();
        let ordered_children: BTreeSet<_> = ordering_edges.iter().map(|edge| edge.after).collect();
        let candidates = land_now
            .iter()
            .filter(|number| !ordered_children.contains(number))
            .copied()
            .collect::<Vec<_>>();
        batch_prs = greedy_conflict_free(&candidates, &conflicts, &by_number);
    }
    let red = |class| {
        nodes
            .iter()
            .filter(|node| node.ci.red_class == Some(class))
            .map(|node| node.number)
            .collect::<Vec<_>>()
    };
    let outage_prs = red(RedClass::RunnerOutage);
    let diagnostics = Diagnostics {
        stale_gates: red(RedClass::StaleRequiredCheck),
        flaky_reds: red(RedClass::Flaky),
        real_reds: red(RedClass::Real),
        evaluate_once_race: red(RedClass::EvaluateOnceRace),
        outage_suspected: outage_prs.len() >= outage_min_prs,
        outage_prs,
    };
    (
        Plan {
            parallel_safe_groups: groups,
            land_now,
            order,
            per_pr_actions: decisions,
            batch: batch_prs,
        },
        diagnostics,
    )
}

/// Derive stacks and holds, then compute a complete result from a collected graph.
pub fn assemble_result(
    graph: CollectedGraph,
    freshness_max_behind: Option<i64>,
    outage_min_prs: usize,
    batch: bool,
) -> PlanResult {
    let stacks = build_stacks(&graph.ordering_edges);
    let held = held_reasons(&graph.nodes, &graph.ordering_edges);
    let (plan, diagnostics) = compute_plan(
        &graph.nodes,
        &graph.conflict_edges,
        &graph.ordering_edges,
        &held,
        freshness_max_behind,
        outage_min_prs,
        batch,
    );
    PlanResult {
        graph,
        stacks,
        held,
        plan,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CiVerdict, PolicyClass};

    fn green(number: i64) -> PrNode {
        PrNode {
            number,
            ci: CiVerdict {
                raw_state: CiState::Passed,
                gate_present: true,
                gate_ok: true,
                detail: "passed".into(),
                ..CiVerdict::default()
            },
            ..PrNode::default()
        }
    }

    #[test]
    fn fusion_table_respects_evidence_policy_and_freshness() {
        let mut evidence = green(1);
        evidence.validation_evidence = ValidationEvidence::CleanValidateRecord;
        evidence.validation_authority = ValidationAuthority::HardGreen;
        evidence.ci = CiVerdict::default();
        let mut policy = green(2);
        policy.policy_class = PolicyClass::GatePolicy;
        let mut behind = green(3);
        behind.commits_behind = 1;
        // Explicit bound: this table is about evidence and policy, not freshness,
        // and being behind is no longer a reroute reason by default. `behind`
        // exists here to keep RebaseThenLand covered in the table.
        let (plan, _) = compute_plan(
            &[evidence, policy, behind],
            &[],
            &[],
            &[],
            Some(0),
            2,
            false,
        );
        assert_eq!(plan.per_pr_actions[0].action, PrAction::LandNow);
        assert_eq!(plan.per_pr_actions[1].action, PrAction::EscalateGatePolicy);
        assert_eq!(plan.per_pr_actions[2].action, PrAction::RebaseThenLand);

        let mut strict_behind = green(4);
        strict_behind.commits_behind = 1;
        let (strict, _) = compute_plan(&[strict_behind], &[], &[], &[], Some(0), 2, false);
        assert_eq!(strict.per_pr_actions[0].action, PrAction::RebaseThenLand);

        let mut unproven = green(5);
        unproven.validation_evidence = ValidationEvidence::CleanValidateRecord;
        let (unproven_plan, _) = compute_plan(&[unproven], &[], &[], &[], None, 2, false);
        assert_eq!(unproven_plan.per_pr_actions[0].action, PrAction::Wait);
        assert!(unproven_plan.per_pr_actions[0]
            .why
            .contains("no consuming-workspace hard/soft-green authority"));
    }

    #[test]
    fn passing_non_gate_ci_never_substitutes_for_required_gate() {
        let missing = PrNode {
            number: 1,
            ci: CiVerdict {
                raw_state: CiState::Passed,
                gate_present: false,
                gate_ok: false,
                ..CiVerdict::default()
            },
            ..PrNode::default()
        };
        let non_passing = PrNode {
            number: 2,
            ci: CiVerdict {
                raw_state: CiState::Passed,
                gate_present: true,
                gate_ok: false,
                ..CiVerdict::default()
            },
            ..PrNode::default()
        };
        let (plan, _) = compute_plan(&[missing, non_passing], &[], &[], &[], None, 2, false);
        assert_eq!(plan.per_pr_actions[0].action, PrAction::RefireCi);
        assert_eq!(plan.per_pr_actions[1].action, PrAction::RefireCi);
        assert!(plan.land_now.is_empty());
    }

    #[test]
    fn dependencies_block_landable_children_and_batches_exclude_children() {
        let parent = PrNode {
            number: 1,
            ci: CiVerdict {
                raw_state: CiState::Failed,
                red_class: Some(RedClass::Real),
                gate_present: true,
                detail: "real red".into(),
                ..CiVerdict::default()
            },
            ..PrNode::default()
        };
        let child = green(2);
        let edge = OrderingEdge {
            before: 1,
            after: 2,
            reason: "base-ref".into(),
        };
        let (blocked, _) = compute_plan(
            &[parent, child],
            &[],
            std::slice::from_ref(&edge),
            &[],
            None,
            2,
            true,
        );
        let child = blocked
            .per_pr_actions
            .iter()
            .find(|decision| decision.pr == 2)
            .unwrap();
        assert_eq!(child.action, PrAction::Wait);
        assert!(child.why.contains("dependency #1 requires hold-fix"));
        assert!(!blocked.land_now.contains(&2));
        assert!(!blocked.batch.contains(&2));

        let (ready, _) = compute_plan(
            &[green(3), green(4)],
            &[],
            &[OrderingEdge {
                before: 3,
                after: 4,
                reason: "base-ref".into(),
            }],
            &[],
            None,
            2,
            true,
        );
        assert_eq!(ready.land_now, vec![3, 4]);
        assert_eq!(ready.batch, vec![3]);
    }

    #[test]
    fn unavailable_review_evidence_waits_before_base_conflict_rebase() {
        let mut local = green(4);
        local.review_evidence_unavailable = true;
        local.base_conflict_paths = vec!["src/conflict.rs".into()];
        local.validation_evidence = ValidationEvidence::CleanValidateRecord;
        local.validation_authority = ValidationAuthority::HardGreen;

        let mut github = green(5);
        github.review_evidence_unavailable = true;
        github.mergeable = "CONFLICTING".into();
        github.validation_evidence = ValidationEvidence::CleanValidateRecord;
        github.validation_authority = ValidationAuthority::HardGreen;

        let nodes = vec![local, github];
        let held = held_reasons(&nodes, &[]);
        let (plan, _) = compute_plan(&nodes, &[], &[], &held, None, 2, false);
        let by_pr = plan
            .per_pr_actions
            .iter()
            .map(|decision| (decision.pr, decision))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_pr[&4].action, PrAction::Wait);
        assert_eq!(by_pr[&5].action, PrAction::Wait);
        assert!(by_pr[&4].why.contains("review-evidence-unavailable"));
        assert!(by_pr[&4].why.contains("local-base-conflict"));
        assert!(by_pr[&5].why.contains("review-evidence-unavailable"));
        assert!(by_pr[&5].why.contains("github-base-conflicting"));
        assert!(plan.land_now.is_empty());
    }

    #[test]
    fn gate_policy_escalates_and_main_advance_requires_rebase_without_revalidation() {
        let mut policy = green(1);
        policy.policy_class = PolicyClass::GatePolicy;
        let held = HeldPr {
            pr: 1,
            reasons: vec!["draft".into()],
        };
        let (policy_plan, _) = compute_plan(&[policy], &[], &[], &[held], None, 2, false);
        assert_eq!(
            policy_plan.per_pr_actions[0].action,
            PrAction::EscalateGatePolicy
        );

        let mut evidence = green(2);
        evidence.validation_evidence = ValidationEvidence::CleanValidateRecord;
        evidence.validation_authority = ValidationAuthority::SoftGreen;
        evidence.commits_behind = 3;
        // Main moving under a validated branch is not a landing obstacle: by
        // default a soft-green ancestor lands.
        let (default_plan, _) = compute_plan(
            &[evidence.clone()],
            &[],
            &[],
            &[],
            DEFAULT_FRESHNESS_MAX_BEHIND,
            2,
            false,
        );
        assert_eq!(default_plan.per_pr_actions[0].action, PrAction::LandNow);

        // The reroute and its exact wording are retained for a caller that asks.
        let (evidence_plan, _) =
            compute_plan(&[evidence.clone()], &[], &[], &[], Some(0), 2, false);
        let decision = &evidence_plan.per_pr_actions[0];
        assert_eq!(decision.action, PrAction::RebaseThenLand);
        assert!(decision.why.contains("soft-green authority"));
        assert!(decision.why.contains("rebase 3 commit(s), then land"));
        assert!(decision.why.contains("without pre-landing revalidation"));
        assert!(decision.why.contains("post-facto validation remains due"));

        evidence.commits_behind = 0;
        let (fresh_plan, _) = compute_plan(&[evidence], &[], &[], &[], Some(0), 2, false);
        let fresh_decision = &fresh_plan.per_pr_actions[0];
        assert_eq!(fresh_decision.action, PrAction::LandNow);
        assert!(!fresh_decision.why.contains("rebase"));
    }

    #[test]
    fn all_red_actions_no_result_variants_batch_and_diagnostics() {
        let red_node = |number, class| PrNode {
            number,
            ci: CiVerdict {
                raw_state: CiState::Failed,
                red_class: Some(class),
                gate_present: true,
                detail: class.as_str().into(),
                ..CiVerdict::default()
            },
            ..PrNode::default()
        };
        let mut nodes = vec![
            red_node(1, RedClass::StaleRequiredCheck),
            red_node(2, RedClass::Flaky),
            red_node(3, RedClass::Real),
            red_node(4, RedClass::EvaluateOnceRace),
            red_node(5, RedClass::RunnerOutage),
            red_node(6, RedClass::RunnerOutage),
            PrNode {
                number: 7,
                ci: CiVerdict {
                    raw_state: CiState::NoResult,
                    ..CiVerdict::default()
                },
                ..PrNode::default()
            },
            PrNode {
                number: 8,
                ci: CiVerdict {
                    raw_state: CiState::NoResult,
                    gate_present: true,
                    gate_ok: true,
                    ..CiVerdict::default()
                },
                ..PrNode::default()
            },
            green(9),
            green(10),
            green(11),
        ];
        nodes[8].priority = 1;
        nodes[9].priority = 0;
        nodes[10].priority = 0;
        nodes[10].additions = 1;
        let conflicts = vec![ConflictEdge {
            a: 9,
            b: 10,
            paths: vec!["x".into()],
        }];
        let (plan, diagnostics) = compute_plan(&nodes, &conflicts, &[], &[], None, 2, true);
        let actions: BTreeMap<_, _> = plan
            .per_pr_actions
            .iter()
            .map(|decision| (decision.pr, decision.action))
            .collect();
        assert_eq!(actions[&1], PrAction::RefireStaleGate);
        assert_eq!(actions[&2], PrAction::RefireCi);
        assert_eq!(actions[&3], PrAction::HoldFix);
        assert_eq!(actions[&4], PrAction::Wait);
        assert_eq!(actions[&5], PrAction::EscalateRunnerOutage);
        assert_eq!(actions[&7], PrAction::RefireCi);
        assert_eq!(actions[&8], PrAction::Wait);
        assert!(diagnostics.outage_suspected);
        assert_eq!(diagnostics.outage_prs, vec![5, 6]);
        assert!(!(plan.batch.contains(&9) && plan.batch.contains(&10)));
        assert!(plan.batch.contains(&11));
    }
}
