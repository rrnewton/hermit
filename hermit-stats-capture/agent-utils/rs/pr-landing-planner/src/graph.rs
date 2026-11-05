//! Deterministic graph algorithms over already-collected pull requests.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use crate::context::REQUIRED_REVIEW_LANES;
use crate::mechanism::{classify, Mechanism};
use crate::model::{
    Cluster, ConflictEdge, HeldPr, MechanismEdge, OrderingEdge, OverlapEdge, PrNode, ReviewBinding,
    UnclassifiedMechanism,
};

/// Label prefix used to declare an affected operational mechanism.
pub const MECHANISM_LABEL_PREFIX: &str = "mechanism:";

/// Extract non-empty mechanism slugs from labels.
pub fn mechanism_slugs(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|label| label.strip_prefix(MECHANISM_LABEL_PREFIX))
        .filter(|slug| !slug.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Combine diff-derived symbols and label-declared slugs for one PR.
pub fn mechanism_candidates(node: &PrNode) -> Vec<String> {
    mechanism_slugs(&node.labels)
        .into_iter()
        .chain(node.mechanism_symbols.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Classify one PR's candidates into recognized mechanisms and unknown strings.
pub fn classify_node_mechanisms(node: &PrNode) -> (BTreeSet<Mechanism>, Vec<String>) {
    let mut recognised = BTreeSet::new();
    let mut unknown = Vec::new();
    for candidate in mechanism_candidates(node) {
        if let Some(mechanism) = classify(&candidate) {
            recognised.insert(mechanism);
        } else {
            unknown.push(candidate);
        }
    }
    (recognised, unknown)
}

/// Link PR pairs that affect at least one common recognized mechanism.
pub fn build_mechanism_edges(nodes: &[PrNode]) -> Vec<MechanismEdge> {
    let classes: Vec<_> = nodes
        .iter()
        .map(|node| classify_node_mechanisms(node).0)
        .collect();
    let mut edges = Vec::new();
    for (i, a) in nodes.iter().enumerate() {
        if classes[i].is_empty() {
            continue;
        }
        for (j, b) in nodes.iter().enumerate().skip(i + 1) {
            let mechanisms = classes[i]
                .intersection(&classes[j])
                .map(|m| m.as_str().to_owned())
                .collect::<Vec<_>>();
            if !mechanisms.is_empty() {
                edges.push(MechanismEdge {
                    a: a.number,
                    b: b.number,
                    mechanisms,
                });
            }
        }
    }
    edges
}

/// Collect each PR's mechanism candidates that have no recognized classification.
pub fn build_unclassified_mechanisms(nodes: &[PrNode]) -> Vec<UnclassifiedMechanism> {
    nodes
        .iter()
        .filter_map(|node| {
            let candidates = classify_node_mechanisms(node).1;
            (!candidates.is_empty()).then_some(UnclassifiedMechanism {
                pr: node.number,
                candidates,
            })
        })
        .collect()
}

/// Link PR pairs whose changed-file sets intersect.
pub fn build_overlap_edges(nodes: &[PrNode]) -> Vec<OverlapEdge> {
    let mut edges = Vec::new();
    for (i, a) in nodes.iter().enumerate() {
        for b in nodes.iter().skip(i + 1) {
            let paths = a.files.intersection(&b.files).cloned().collect::<Vec<_>>();
            if !paths.is_empty() {
                edges.push(OverlapEdge {
                    a: a.number,
                    b: b.number,
                    paths,
                });
            }
        }
    }
    edges
}

/// Conservatively treat every shared-file overlap as a conflict.
pub fn build_conflict_edges_file_overlap(nodes: &[PrNode]) -> Vec<ConflictEdge> {
    build_overlap_edges(nodes)
        .into_iter()
        .map(|edge| ConflictEdge {
            a: edge.a,
            b: edge.b,
            paths: edge.paths,
        })
        .collect()
}

/// Build ordering edges where one PR targets another PR's head branch.
pub fn build_ordering_edges_base_ref(nodes: &[PrNode]) -> Vec<OrderingEdge> {
    let by_head: BTreeMap<_, _> = nodes
        .iter()
        .map(|node| (node.head_ref.as_str(), node.number))
        .collect();
    nodes
        .iter()
        .filter_map(|node| {
            let before = *by_head.get(node.base_ref.as_str())?;
            (before != node.number).then_some(OrderingEdge {
                before,
                after: node.number,
                reason: "base-ref".to_owned(),
            })
        })
        .collect()
}

/// Sort ordering edges and retain the first reason for each directed pair.
pub fn dedupe_ordering(edges: &[OrderingEdge]) -> Vec<OrderingEdge> {
    let mut seen = BTreeMap::new();
    for edge in edges {
        seen.entry((edge.before, edge.after))
            .or_insert_with(|| edge.clone());
    }
    seen.into_values().collect()
}

fn has_path(
    adjacency: &BTreeMap<i64, BTreeSet<i64>>,
    start: i64,
    target: i64,
    skip: (i64, i64),
) -> bool {
    let mut pending = vec![start];
    let mut seen = BTreeSet::new();
    while let Some(current) = pending.pop() {
        if !seen.insert(current) {
            continue;
        }
        for child in adjacency.get(&current).into_iter().flatten() {
            if (current, *child) == skip {
                continue;
            }
            if *child == target {
                return true;
            }
            pending.push(*child);
        }
    }
    false
}

/// Remove ordering edges already implied by a longer directed path.
pub fn transitive_reduce(edges: &[OrderingEdge]) -> Vec<OrderingEdge> {
    let mut adjacency: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for edge in edges {
        adjacency.entry(edge.before).or_default().insert(edge.after);
    }
    edges
        .iter()
        .filter(|edge| {
            !has_path(
                &adjacency,
                edge.before,
                edge.after,
                (edge.before, edge.after),
            )
        })
        .cloned()
        .collect()
}

/// Enumerate root-to-leaf dependency chains from the reduced ordering graph.
pub fn build_stacks(edges: &[OrderingEdge]) -> Vec<Vec<i64>> {
    let reduced = transitive_reduce(edges);
    let mut children: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let mut parents: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let mut involved = BTreeSet::new();
    for edge in reduced {
        children.entry(edge.before).or_default().insert(edge.after);
        parents.entry(edge.after).or_default().insert(edge.before);
        involved.extend([edge.before, edge.after]);
    }
    fn visit(
        node: i64,
        path: &mut Vec<i64>,
        children: &BTreeMap<i64, BTreeSet<i64>>,
        stacks: &mut Vec<Vec<i64>>,
    ) {
        let next = children.get(&node).cloned().unwrap_or_default();
        if next.is_empty() {
            if path.len() > 1 {
                stacks.push(path.clone());
            }
            return;
        }
        for child in next {
            if path.contains(&child) {
                continue;
            }
            path.push(child);
            visit(child, path, children, stacks);
            path.pop();
        }
    }
    let mut stacks = Vec::new();
    for root in involved
        .into_iter()
        .filter(|number| parents.get(number).is_none_or(BTreeSet::is_empty))
    {
        visit(root, &mut vec![root], &children, &mut stacks);
    }
    stacks
}

/// Compute fail-closed review, conflict, draft, cycle, and dependent hold reasons.
pub fn held_reasons(nodes: &[PrNode], ordering_edges: &[OrderingEdge]) -> Vec<HeldPr> {
    let mut adjacency: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for edge in ordering_edges {
        adjacency.entry(edge.before).or_default().insert(edge.after);
    }
    let mut cycle_nodes = BTreeSet::new();
    for start in adjacency.keys().copied() {
        let mut pending = adjacency
            .get(&start)
            .into_iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if current == start {
                cycle_nodes.insert(start);
                break;
            }
            if !seen.insert(current) {
                continue;
            }
            pending.extend(adjacency.get(&current).into_iter().flatten().copied());
        }
    }

    let mut reasons: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for node in nodes {
        let mut why = Vec::new();
        if node.is_draft {
            why.push("draft".to_owned());
        }
        if node.review_evidence_unavailable {
            why.push("review-evidence-unavailable".to_owned());
        }
        let review = node.review_decision.trim().to_ascii_uppercase();
        match review.as_str() {
            "REVIEW_REQUIRED" => why.push("review-required".to_owned()),
            "CHANGES_REQUESTED" => {
                if !node.review_objections_resolved {
                    why.push("changes-requested".to_owned());
                }
            }
            "" | "APPROVED" => {}
            _ => why.push(format!("review-decision-unknown:{review}")),
        }
        why.extend(review_binding(node).1);
        if !node.base_conflict_paths.is_empty() {
            why.push("local-base-conflict".to_owned());
        }
        if node.mergeable == "CONFLICTING" {
            why.push("github-base-conflicting".to_owned());
        }
        if cycle_nodes.contains(&node.number) {
            why.push("ordering-cycle".to_owned());
        }
        if !why.is_empty() {
            reasons.insert(node.number, why);
        }
    }
    loop {
        let mut additions = Vec::new();
        for edge in ordering_edges {
            if reasons.contains_key(&edge.before) && !reasons.contains_key(&edge.after) {
                additions.push((
                    edge.after,
                    vec![format!("depends-on-held:#{}", edge.before)],
                ));
            }
        }
        if additions.is_empty() {
            break;
        }
        for (number, why) in additions {
            reasons.entry(number).or_insert(why);
        }
    }
    reasons
        .into_iter()
        .map(|(pr, reasons)| HeldPr { pr, reasons })
        .collect()
}

/// Dereference review-protocol labels through exact-head caller receipts.
///
/// PASS labels are caches, never authority by themselves. A protocol-active PR is
/// landable only when every required lane has both its label and a receipt for the
/// current fetched head. Any head change deliberately makes the receipt stale,
/// including a patch-identical rebase, until the reviewer re-attests the delta.
pub fn review_binding(node: &PrNode) -> (ReviewBinding, Vec<String>) {
    let protocol_active = !node.review_pass_heads.is_empty()
        || node.labels.iter().any(|label| {
            label == "post-facto-human-review"
                || label.starts_with("passed-review-")
                || label.starts_with("adversarial-review-")
        });
    if !protocol_active {
        return (ReviewBinding::NotRequired, Vec::new());
    }

    let mut reasons = Vec::new();
    for lane in REQUIRED_REVIEW_LANES {
        let pass_label = format!("passed-review-{lane}");
        if !node.labels.iter().any(|label| label == &pass_label) {
            reasons.push(format!("review-pass-missing:{lane}"));
            continue;
        }
        match node.review_pass_heads.get(lane) {
            None => reasons.push(format!("review-pass-unbound:{lane}")),
            Some(reviewed) if reviewed != &node.head_sha => reasons.push(format!(
                "review-pass-stale:{lane}:reviewed={reviewed}:current={}",
                node.head_sha
            )),
            Some(_) => {}
        }
    }

    let binding = if reasons.is_empty() {
        ReviewBinding::ExactHead
    } else if reasons
        .iter()
        .any(|reason| reason.starts_with("review-pass-stale:"))
    {
        ReviewBinding::Stale
    } else if reasons
        .iter()
        .any(|reason| reason.starts_with("review-pass-unbound:"))
    {
        ReviewBinding::Unbound
    } else {
        ReviewBinding::Missing
    };
    (binding, reasons)
}

fn rank(node: &PrNode) -> (i64, i64, &str, i64) {
    (
        node.priority,
        node.size(),
        node.created_at.as_str(),
        node.number,
    )
}

/// Greedily layer non-held PRs into deterministic conflict- and ordering-safe groups.
pub fn partition_parallel_safe(
    nodes: &[PrNode],
    conflict_edges: &[ConflictEdge],
    ordering_edges: &[OrderingEdge],
    exclude: &BTreeSet<i64>,
) -> Vec<Vec<i64>> {
    let by_number: BTreeMap<_, _> = nodes
        .iter()
        .filter(|node| !exclude.contains(&node.number))
        .map(|node| (node.number, node))
        .collect();
    let numbers: BTreeSet<_> = by_number.keys().copied().collect();
    let mut conflicts: BTreeMap<i64, BTreeSet<i64>> = numbers
        .iter()
        .map(|number| (*number, BTreeSet::new()))
        .collect();
    for edge in conflict_edges {
        if numbers.contains(&edge.a) && numbers.contains(&edge.b) {
            conflicts.entry(edge.a).or_default().insert(edge.b);
            conflicts.entry(edge.b).or_default().insert(edge.a);
        }
    }
    let mut predecessors: BTreeMap<i64, BTreeSet<i64>> = numbers
        .iter()
        .map(|number| (*number, BTreeSet::new()))
        .collect();
    for edge in ordering_edges {
        if numbers.contains(&edge.before) && numbers.contains(&edge.after) {
            predecessors
                .entry(edge.after)
                .or_default()
                .insert(edge.before);
        }
    }
    let mut remaining = numbers;
    let mut placed = BTreeSet::new();
    let mut groups = Vec::new();
    while !remaining.is_empty() {
        let mut ready: Vec<_> = remaining
            .iter()
            .filter(|number| predecessors[*number].is_subset(&placed))
            .copied()
            .collect();
        ready.sort_by_key(|number| rank(by_number[number]));
        if ready.is_empty() {
            let mut rest: Vec<_> = remaining.into_iter().collect();
            rest.sort_by_key(|number| rank(by_number[number]));
            groups.extend(rest.into_iter().map(|number| vec![number]));
            break;
        }
        let mut group = Vec::new();
        for number in ready {
            if group.iter().all(|peer| !conflicts[&number].contains(peer)) {
                group.push(number);
            }
        }
        for number in &group {
            remaining.remove(number);
            placed.insert(*number);
        }
        groups.push(group);
    }
    groups
}

/// Return deterministic connected components of an undirected conflict graph.
pub fn connected_components(numbers: &[i64], edges: &[ConflictEdge]) -> Vec<Vec<i64>> {
    let known: BTreeSet<_> = numbers.iter().copied().collect();
    let mut adjacency: BTreeMap<i64, BTreeSet<i64>> = known
        .iter()
        .map(|number| (*number, BTreeSet::new()))
        .collect();
    for edge in edges {
        if known.contains(&edge.a) && known.contains(&edge.b) {
            adjacency.entry(edge.a).or_default().insert(edge.b);
            adjacency.entry(edge.b).or_default().insert(edge.a);
        }
    }
    let mut remaining = known;
    let mut components = Vec::new();
    while let Some(start) = remaining.iter().next().copied() {
        let mut pending = vec![start];
        let mut component = BTreeSet::new();
        while let Some(number) = pending.pop() {
            if !component.insert(number) {
                continue;
            }
            pending.extend(adjacency[&number].iter().copied());
        }
        for number in &component {
            remaining.remove(number);
        }
        components.push(component.into_iter().collect::<Vec<_>>());
    }
    components.sort_by_key(|component| (Reverse(component.len()), component[0]));
    components
}

fn stack_order(
    members: &[i64],
    by_number: &BTreeMap<i64, &PrNode>,
    ordering_edges: &[OrderingEdge],
) -> Vec<i64> {
    let member_set: BTreeSet<_> = members.iter().copied().collect();
    let mut successors: BTreeMap<i64, BTreeSet<i64>> = member_set
        .iter()
        .map(|number| (*number, BTreeSet::new()))
        .collect();
    let mut indegree: BTreeMap<i64, usize> = member_set.iter().map(|number| (*number, 0)).collect();
    for edge in ordering_edges {
        if member_set.contains(&edge.before)
            && member_set.contains(&edge.after)
            && successors
                .entry(edge.before)
                .or_default()
                .insert(edge.after)
        {
            *indegree.entry(edge.after).or_default() += 1;
        }
    }
    let mut ready: Vec<_> = member_set
        .iter()
        .filter(|number| indegree[*number] == 0)
        .copied()
        .collect();
    ready.sort_by_key(|number| rank(by_number[number]));
    let mut ordered = Vec::new();
    let mut placed = BTreeSet::new();
    while !ready.is_empty() {
        let number = ready.remove(0);
        ordered.push(number);
        placed.insert(number);
        for child in successors[&number].clone() {
            indegree.entry(child).and_modify(|value| *value -= 1);
            if indegree[&child] == 0 {
                ready.push(child);
            }
        }
        ready.sort_by_key(|candidate| rank(by_number[candidate]));
    }
    if ordered.len() < member_set.len() {
        let mut rest: Vec<_> = member_set.difference(&placed).copied().collect();
        rest.sort_by_key(|number| rank(by_number[number]));
        ordered.extend(rest);
    }
    ordered
}

/// Build stack-ordered connected components of the real-conflict graph.
pub fn cluster_by_conflict(
    nodes: &[PrNode],
    conflict_edges: &[ConflictEdge],
    ordering_edges: &[OrderingEdge],
) -> Vec<Cluster> {
    let by_number: BTreeMap<_, _> = nodes.iter().map(|node| (node.number, node)).collect();
    connected_components(
        &nodes.iter().map(|node| node.number).collect::<Vec<_>>(),
        conflict_edges,
    )
    .into_iter()
    .map(|component| {
        let members: BTreeSet<_> = component.iter().copied().collect();
        let conflict_paths = conflict_edges
            .iter()
            .filter(|edge| members.contains(&edge.a) && members.contains(&edge.b))
            .flat_map(|edge| edge.paths.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Cluster {
            members: stack_order(&component, &by_number, ordering_edges),
            conflict_paths,
        }
    })
    .collect()
}

/// Sum the serial rebases avoided by treating each conflict cluster as one stack.
pub fn rebases_avoided(clusters: &[Cluster]) -> usize {
    clusters.iter().map(Cluster::rebases_avoided).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(number: i64, priority: i64) -> PrNode {
        PrNode {
            number,
            priority,
            created_at: format!("2026-{number:02}"),
            ..PrNode::default()
        }
    }

    #[test]
    fn reduction_stacks_holds_and_partition_are_deterministic() {
        let edges = vec![
            OrderingEdge {
                before: 1,
                after: 2,
                reason: "x".into(),
            },
            OrderingEdge {
                before: 2,
                after: 3,
                reason: "x".into(),
            },
            OrderingEdge {
                before: 1,
                after: 3,
                reason: "x".into(),
            },
        ];
        assert_eq!(transitive_reduce(&edges).len(), 2);
        assert_eq!(build_stacks(&edges), vec![vec![1, 2, 3]]);
        let mut nodes = vec![node(1, 3), node(2, 2), node(3, 1)];
        nodes[0].is_draft = true;
        assert_eq!(held_reasons(&nodes, &edges).len(), 3);
        assert_eq!(
            partition_parallel_safe(&nodes, &[], &edges, &BTreeSet::new()),
            vec![vec![1], vec![2], vec![3]]
        );
    }

    #[test]
    fn review_and_ordering_cycles_are_held_fail_closed() {
        let mut review_required = node(1, 0);
        review_required.review_decision = "REVIEW_REQUIRED".into();
        let mut changes_requested = node(2, 0);
        changes_requested.review_decision = "CHANGES_REQUESTED".into();
        let approved = PrNode {
            review_decision: "APPROVED".into(),
            ..node(3, 0)
        };
        let nodes = vec![
            review_required,
            changes_requested,
            approved,
            node(4, 0),
            node(5, 0),
            node(6, 0),
        ];
        let edges = vec![
            OrderingEdge {
                before: 4,
                after: 5,
                reason: "base-ref".into(),
            },
            OrderingEdge {
                before: 5,
                after: 4,
                reason: "ancestry".into(),
            },
            OrderingEdge {
                before: 5,
                after: 6,
                reason: "base-ref".into(),
            },
        ];
        let held: BTreeMap<_, _> = held_reasons(&nodes, &edges)
            .into_iter()
            .map(|held| (held.pr, held.reasons))
            .collect();
        assert_eq!(held[&1], vec!["review-required"]);
        assert_eq!(held[&2], vec!["changes-requested"]);
        assert!(!held.contains_key(&3));
        assert!(held[&4].contains(&"ordering-cycle".to_owned()));
        assert!(held[&5].contains(&"ordering-cycle".to_owned()));
        assert_eq!(held[&6], vec!["depends-on-held:#5"]);

        let unavailable = PrNode {
            review_decision: "APPROVED".into(),
            review_evidence_unavailable: true,
            ..node(7, 0)
        };
        assert_eq!(
            held_reasons(&[unavailable], &[])[0].reasons,
            ["review-evidence-unavailable"]
        );
    }

    #[test]
    fn exact_head_objection_resolution_only_clears_changes_requested() {
        let mut resolved = node(1, 0);
        resolved.review_decision = "CHANGES_REQUESTED".into();
        resolved.review_objections_resolved = true;
        let mut unresolved = node(2, 0);
        unresolved.review_decision = "CHANGES_REQUESTED".into();
        let mut review_required = node(3, 0);
        review_required.review_decision = "REVIEW_REQUIRED".into();
        review_required.review_objections_resolved = true;

        let held: BTreeMap<_, _> = held_reasons(&[resolved, unresolved, review_required], &[])
            .into_iter()
            .map(|held| (held.pr, held.reasons))
            .collect();
        assert!(!held.contains_key(&1));
        assert_eq!(held[&2], vec!["changes-requested"]);
        assert_eq!(held[&3], vec!["review-required"]);
    }

    #[test]
    fn mechanism_aliases_form_semantic_edges() {
        let mut a = node(1, 0);
        a.mechanism_symbols = vec!["CANCEL_IN_PROGRESS".into()];
        let mut b = node(2, 0);
        b.labels = vec!["mechanism:cancel-in-progress".into()];
        assert_eq!(
            build_mechanism_edges(&[a, b])[0].mechanisms,
            vec!["cancel-in-progress"]
        );
    }

    #[test]
    fn overlap_components_and_stack_order_match_contract() {
        let mut one = node(1, 0);
        one.files = BTreeSet::from(["shared.rs".into(), "one.rs".into()]);
        one.additions = 5;
        let mut two = node(2, 0);
        two.files = BTreeSet::from(["shared.rs".into()]);
        two.additions = 1;
        let mut three = node(3, 0);
        three.additions = 9;
        let overlap = build_overlap_edges(&[one.clone(), two.clone(), three.clone()]);
        assert_eq!(overlap.len(), 1);
        assert_eq!(overlap[0].paths, vec!["shared.rs"]);

        let conflicts = vec![
            ConflictEdge {
                a: 1,
                b: 2,
                paths: vec!["shared.rs".into()],
            },
            ConflictEdge {
                a: 2,
                b: 3,
                paths: vec!["other.rs".into()],
            },
        ];
        assert_eq!(
            connected_components(&[1, 2, 3, 4], &conflicts),
            vec![vec![1, 2, 3], vec![4]]
        );
        let clusters = cluster_by_conflict(
            &[one, two, three],
            &conflicts,
            &[OrderingEdge {
                before: 3,
                after: 1,
                reason: "base-ref".into(),
            }],
        );
        assert_eq!(clusters[0].members[0], 2);
        assert!(
            clusters[0].members.iter().position(|n| *n == 3)
                < clusters[0].members.iter().position(|n| *n == 1)
        );
        assert_eq!(clusters[0].conflict_paths, vec!["other.rs", "shared.rs"]);
        assert_eq!(rebases_avoided(&clusters), 2);
    }

    #[test]
    fn partition_ranks_and_degrades_ordering_cycles_without_looping() {
        let mut low = node(1, 1);
        low.additions = 1;
        let mut high_large = node(2, 0);
        high_large.additions = 50;
        let mut high_small = node(3, 0);
        high_small.additions = 1;
        assert_eq!(
            partition_parallel_safe(&[low, high_large, high_small], &[], &[], &BTreeSet::new()),
            vec![vec![3, 2, 1]]
        );
        let cycle = vec![
            OrderingEdge {
                before: 1,
                after: 2,
                reason: "x".into(),
            },
            OrderingEdge {
                before: 2,
                after: 1,
                reason: "x".into(),
            },
        ];
        let cycle_nodes = [node(1, 0), node(2, 0)];
        assert_eq!(
            partition_parallel_safe(&cycle_nodes, &[], &cycle, &BTreeSet::new()),
            vec![vec![1], vec![2]]
        );
    }
}
