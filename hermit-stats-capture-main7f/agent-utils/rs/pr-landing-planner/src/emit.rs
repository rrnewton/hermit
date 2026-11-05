//! Pure, deterministic renderers for machine and human consumers.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::graph::{cluster_by_conflict, rebases_avoided, review_binding};
use crate::model::{CiState, Cluster, HeldPr, PlanResult, PrAction, PrActionDecision, PrNode};

/// Explanation emitted with the rebase and validation-run savings calculation.
pub const VALIDATE_ECONOMICS_RATIONALE: &str = "A branch behind its fetched base must rebase before landing. The consuming workspace may authorize that rebase to retain soft-green without pre-landing revalidation; post-facto validation remains due. Landing each real-conflict cluster as ONE stack avoids the same count of repeated rebases and post-facto validate runs.";

fn economics(clusters: &[Cluster]) -> Value {
    let saved = rebases_avoided(clusters);
    json!({
        "validate_record_keyed_to": "head_sha+base_sha",
        "rebases_avoided_by_clustering": saved,
        "validate_runs_avoided_by_clustering": saved,
        "rationale": VALIDATE_ECONOMICS_RATIONALE,
    })
}

fn node_obj(node: &PrNode, held: bool) -> Value {
    json!({
        "pr": node.number,
        "title": node.title,
        "author": node.author,
        "head": node.head_sha,
        "updated_at": (!node.updated_at.is_empty()).then_some(node.updated_at.as_str()),
        "base_sha": node.base_sha,
        "base_ref": node.base_ref,
        "ci": node.ci.raw_state.as_str(),
        "ci_detail": node.ci.detail,
        "red_class": node.ci.red_class.map(|red| red.as_str()),
        "gate_ok": node.ci.gate_ok,
        "freshness_behind": node.commits_behind,
        "size": node.size(),
        "held": held,
        "priority": node.priority,
        "labels": node.labels,
        "assigned_agent": (!node.assigned_agent.is_empty()).then_some(node.assigned_agent.as_str()),
        "validation_evidence": node.validation_evidence.as_str(),
        "validation_authority": node.validation_authority.as_str(),
        "policy_class": node.policy_class.as_str(),
        "review_decision": (!node.review_decision.is_empty()).then_some(node.review_decision.as_str()),
        "review_evidence_unavailable": node.review_evidence_unavailable,
        "review_evidence_digest": (!node.review_evidence_digest.is_empty())
            .then_some(node.review_evidence_digest.as_str()),
        "review_objections_resolved": node.review_objections_resolved,
        "review_binding": review_binding(node).0.as_str(),
        "review_pass_heads": node.review_pass_heads,
    })
}

fn decision_obj(decision: &PrActionDecision) -> Value {
    json!({
        "pr": decision.pr,
        "action": decision.action.as_str(),
        "why": decision.why,
        "group": decision.group,
    })
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key.clone(), canonical_json(value));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(&canonical_json(value)).expect("JSON values serialize")
}

/// Render the complete deterministic machine schema as pretty JSON.
pub fn render_json(result: &PlanResult) -> String {
    let graph = &result.graph;
    let held: BTreeSet<_> = result.held.iter().map(|held| held.pr).collect();
    let clusters = cluster_by_conflict(&graph.nodes, &graph.conflict_edges, &graph.ordering_edges);
    json_pretty(&json!({
        "repository": graph.repository,
        "base": graph.base,
        "nodes": graph.nodes.iter().map(|node| node_obj(node, held.contains(&node.number))).collect::<Vec<_>>(),
        "conflict_edges": graph.conflict_edges.iter().map(|edge| json!({"a": edge.a, "b": edge.b, "paths": edge.paths})).collect::<Vec<_>>(),
        "file_overlap_edges": graph.overlap_edges.iter().map(|edge| json!({"a": edge.a, "b": edge.b, "paths": edge.paths})).collect::<Vec<_>>(),
        "ordering_edges": graph.ordering_edges.iter().map(|edge| json!({"before": edge.before, "after": edge.after, "reason": edge.reason})).collect::<Vec<_>>(),
        "mechanism_overlap_edges": graph.mechanism_edges.iter().map(|edge| json!({"a": edge.a, "b": edge.b, "mechanisms": edge.mechanisms})).collect::<Vec<_>>(),
        "unclassified_mechanism_candidates": graph.unclassified_mechanisms.iter().map(|item| json!({"pr": item.pr, "candidates": item.candidates})).collect::<Vec<_>>(),
        "stacks": result.stacks,
        "held_prs": result.held.iter().map(|held| json!({"pr": held.pr, "reasons": held.reasons})).collect::<Vec<_>>(),
        "plan": {
            "parallel_safe_groups": result.plan.parallel_safe_groups,
            "land_now": result.plan.land_now,
            "order": result.plan.order,
            "batch": result.plan.batch,
            "per_pr_actions": result.plan.per_pr_actions.iter().map(decision_obj).collect::<Vec<_>>(),
            "rebase_economics": economics(&clusters),
        },
        "diagnostics": {
            "stale_gates": result.diagnostics.stale_gates,
            "flaky_reds": result.diagnostics.flaky_reds,
            "real_reds": result.diagnostics.real_reds,
            "evaluate_once_race": result.diagnostics.evaluate_once_race,
            "outage_prs": result.diagnostics.outage_prs,
            "outage_suspected": result.diagnostics.outage_suspected,
        },
    }))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn summary_counts(result: &PlanResult) -> Vec<(&'static str, usize)> {
    let mut counts = BTreeMap::new();
    for action in PrAction::ALL {
        counts.insert(action, 0usize);
    }
    for decision in &result.plan.per_pr_actions {
        *counts.entry(decision.action).or_default() += 1;
    }
    let clusters = cluster_by_conflict(
        &result.graph.nodes,
        &result.graph.conflict_edges,
        &result.graph.ordering_edges,
    );
    let saved = rebases_avoided(&clusters);
    vec![
        ("open_prs", result.graph.nodes.len()),
        ("land_now", counts[&PrAction::LandNow]),
        ("rebase", counts[&PrAction::RebaseThenLand]),
        ("refire_stale_gate", counts[&PrAction::RefireStaleGate]),
        ("refire_ci", counts[&PrAction::RefireCi]),
        ("hold_fix", counts[&PrAction::HoldFix]),
        ("escalate_outage", counts[&PrAction::EscalateRunnerOutage]),
        (
            "escalate_gate_policy",
            counts[&PrAction::EscalateGatePolicy],
        ),
        ("wait", counts[&PrAction::Wait]),
        ("held", result.held.len()),
        ("stale_gates", result.diagnostics.stale_gates.len()),
        ("flaky_reds", result.diagnostics.flaky_reds.len()),
        ("real_reds", result.diagnostics.real_reds.len()),
        (
            "evaluate_once_race",
            result.diagnostics.evaluate_once_race.len(),
        ),
        ("mechanism_overlaps", result.graph.mechanism_edges.len()),
        ("rebases_avoided", saved),
        ("validate_runs_avoided", saved),
        (
            "unclassified_mechanisms",
            result.graph.unclassified_mechanisms.len(),
        ),
        ("outage", usize::from(result.diagnostics.outage_suspected)),
    ]
}

fn action_kind(action: PrAction) -> &'static str {
    match action {
        PrAction::LandNow
        | PrAction::RebaseThenLand
        | PrAction::RefireStaleGate
        | PrAction::RefireCi
        | PrAction::HoldFix => "ACTION",
        PrAction::EscalateRunnerOutage | PrAction::EscalateGatePolicy => "ERROR",
        PrAction::Wait => "NOTE",
    }
}

fn action_line(decision: &PrActionDecision, node: Option<&PrNode>) -> String {
    let mut parts = vec![
        format!(
            "{}: {}",
            action_kind(decision.action),
            decision.action.as_str()
        ),
        format!("pr={}", decision.pr),
    ];
    if let Some(group) = decision.group {
        parts.push(format!("group={group}"));
    }
    if let Some(node) = node {
        if decision.action == PrAction::RebaseThenLand && node.commits_behind != 0 {
            parts.push(format!("behind={}", node.commits_behind));
        }
        if decision.action == PrAction::LandNow {
            parts.push(format!("size={}", node.size()));
        }
    }
    parts.push(format!("why={}", quote(&decision.why)));
    parts.join(" ")
}

/// Render stable summary counts, diagnostics, and one line per recommended action.
pub fn render_actions(result: &PlanResult) -> String {
    let by_number: BTreeMap<_, _> = result
        .graph
        .nodes
        .iter()
        .map(|node| (node.number, node))
        .collect();
    let mut lines = summary_counts(result)
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    if result.diagnostics.outage_suspected {
        let prs = result
            .diagnostics
            .outage_prs
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "ERROR: ci-hosted-runner-outage-systemic prs={prs} detail={}",
            quote(&format!(
                "merge-gate job never ran on {} PR(s)",
                result.diagnostics.outage_prs.len()
            ))
        ));
    }
    for pr in &result.diagnostics.evaluate_once_race {
        lines.push(format!(
            "NOTE: evaluate-once-race pr={pr} (benign gate noise; treat as pending)"
        ));
    }
    for edge in &result.graph.mechanism_edges {
        lines.push(format!(
            "NOTE: mechanism-overlap prs={},{} mechanisms={} (same mechanism — review together; may be opposite intent)",
            edge.a, edge.b, quote(&edge.mechanisms.join(","))
        ));
    }
    for item in &result.graph.unclassified_mechanisms {
        lines.push(format!(
            "NOTE: unclassified-mechanism pr={} candidates={} (not in the enum — recognise as an existing member or add a new one)",
            item.pr, quote(&item.candidates.join(","))
        ));
    }
    let decisions: BTreeMap<_, _> = result
        .plan
        .per_pr_actions
        .iter()
        .map(|decision| (decision.pr, decision))
        .collect();
    let mut emitted = BTreeSet::new();
    for pr in &result.plan.order {
        if let Some(decision) = decisions.get(pr) {
            lines.push(action_line(decision, by_number.get(pr).copied()));
            emitted.insert(*pr);
        }
    }
    for decision in &result.plan.per_pr_actions {
        if !emitted.contains(&decision.pr) {
            lines.push(action_line(decision, by_number.get(&decision.pr).copied()));
        }
    }
    lines.join("\n")
}

fn held_line(held: &HeldPr) -> String {
    format!("  #{}: {}", held.pr, held.reasons.join(", "))
}

fn append_diag(lines: &mut Vec<String>, label: &str, prs: &[i64]) {
    if !prs.is_empty() {
        lines.push(format!(
            "  {label}: {}",
            prs.iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
}

/// Render the complete plan for a terminal or report.
pub fn render_human(result: &PlanResult) -> String {
    let graph = &result.graph;
    let held: BTreeSet<_> = result.held.iter().map(|held| held.pr).collect();
    let by_number: BTreeMap<_, _> = graph.nodes.iter().map(|node| (node.number, node)).collect();
    let mut lines = vec![
        format!("Repository: {}  base: {}", graph.repository, graph.base),
        format!(
            "{} open PR(s), {} real conflict(s), {} file-overlap risk(s), {} ordering edge(s), {} mechanism overlap(s)",
            graph.nodes.len(), graph.conflict_edges.len(), graph.overlap_edges.len(), graph.ordering_edges.len(), graph.mechanism_edges.len()
        ),
        String::new(),
        "CI health:".to_owned(),
    ];
    let mut nodes: Vec<_> = graph.nodes.iter().collect();
    nodes.sort_by_key(|node| node.number);
    for node in nodes {
        let red = node
            .ci
            .red_class
            .map(|red| format!(" [{}]", red.as_str()))
            .unwrap_or_default();
        let held_suffix = if held.contains(&node.number) {
            " HELD"
        } else {
            ""
        };
        lines.push(format!(
            "  #{:<5} ci={}{} behind={} size={}{}  {}",
            node.number,
            node.ci.raw_state.as_str(),
            red,
            node.commits_behind,
            node.size(),
            held_suffix,
            node.title
        ));
    }
    lines.extend([
        String::new(),
        "Parallel-safe groups (each group lands in any order):".to_owned(),
    ]);
    if result.plan.parallel_safe_groups.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        for (index, group) in result.plan.parallel_safe_groups.iter().enumerate() {
            lines.push(format!(
                "  group {index}: {}",
                group
                    .iter()
                    .map(|number| format!("#{number}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let numbered = |numbers: &[i64]| {
        numbers
            .iter()
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.extend([
        String::new(),
        format!(
            "Land now: {}",
            if result.plan.land_now.is_empty() {
                "none".to_owned()
            } else {
                numbered(&result.plan.land_now)
            }
        ),
        format!(
            "Recommended order: {}",
            if result.plan.order.is_empty() {
                "none".to_owned()
            } else {
                numbered(&result.plan.order)
            }
        ),
    ]);
    if !result.plan.batch.is_empty() {
        lines.push(format!(
            "Batch (green-only, conflict-free): {}",
            numbered(&result.plan.batch)
        ));
    }
    lines.extend([String::new(), "Per-PR actions:".to_owned()]);
    for decision in &result.plan.per_pr_actions {
        let title = by_number
            .get(&decision.pr)
            .map(|node| node.title.as_str())
            .unwrap_or("");
        lines.push(format!(
            "  #{:<5} {:<22} {}{}",
            decision.pr,
            decision.action.as_str(),
            decision.why,
            if title.is_empty() {
                String::new()
            } else {
                format!("  ({title})")
            }
        ));
    }
    lines.extend([
        String::new(),
        "Mechanism overlaps (same mechanism — review together, may be opposite intent):".to_owned(),
    ]);
    if graph.mechanism_edges.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        lines.extend(graph.mechanism_edges.iter().map(|edge| {
            format!(
                "  #{} <-> #{}: {}",
                edge.a,
                edge.b,
                edge.mechanisms.join(", ")
            )
        }));
    }
    if !graph.unclassified_mechanisms.is_empty() {
        lines.extend([
            String::new(),
            "Unclassified mechanism candidates (recognise, then extend the enum):".to_owned(),
        ]);
        lines.extend(
            graph
                .unclassified_mechanisms
                .iter()
                .map(|item| format!("  #{}: {}", item.pr, item.candidates.join(", "))),
        );
    }
    lines.extend([String::new(), "Diagnostics:".to_owned()]);
    append_diag(
        &mut lines,
        "stale required-check (refire gate)",
        &result.diagnostics.stale_gates,
    );
    append_diag(
        &mut lines,
        "flaky reds (refire CI)",
        &result.diagnostics.flaky_reds,
    );
    append_diag(
        &mut lines,
        "real reds (hold + fix)",
        &result.diagnostics.real_reds,
    );
    append_diag(
        &mut lines,
        "evaluate-once race (benign)",
        &result.diagnostics.evaluate_once_race,
    );
    append_diag(&mut lines, "runner-outage", &result.diagnostics.outage_prs);
    if result.diagnostics.outage_suspected {
        lines.push("  SYSTEMIC RUNNER OUTAGE SUSPECTED — escalate CI".to_owned());
    }
    if !result.held.is_empty() {
        lines.extend([String::new(), "Held PRs:".to_owned()]);
        lines.extend(result.held.iter().map(held_line));
    }
    lines.extend([
        String::new(),
        "Advisory only: this plan recommends; it never arms or merges anything.".to_owned(),
    ]);
    lines.join("\n")
}

fn graph_value(result: &PlanResult) -> Value {
    let graph = &result.graph;
    let held: BTreeSet<_> = result.held.iter().map(|held| held.pr).collect();
    json!({
        "repository": graph.repository,
        "base": graph.base,
        "nodes": graph.nodes.iter().map(|node| node_obj(node, held.contains(&node.number))).collect::<Vec<_>>(),
        "conflict_edges": graph.conflict_edges.iter().map(|edge| json!({"a": edge.a, "b": edge.b, "paths": edge.paths})).collect::<Vec<_>>(),
        "file_overlap_edges": graph.overlap_edges.iter().map(|edge| json!({"a": edge.a, "b": edge.b, "paths": edge.paths})).collect::<Vec<_>>(),
        "ordering_edges": graph.ordering_edges.iter().map(|edge| json!({"before": edge.before, "after": edge.after, "reason": edge.reason})).collect::<Vec<_>>(),
        "mechanism_overlap_edges": graph.mechanism_edges.iter().map(|edge| json!({"a": edge.a, "b": edge.b, "mechanisms": edge.mechanisms})).collect::<Vec<_>>(),
        "unclassified_mechanism_candidates": graph.unclassified_mechanisms.iter().map(|item| json!({"pr": item.pr, "candidates": item.candidates})).collect::<Vec<_>>(),
        "stacks": result.stacks,
        "held_prs": result.held.iter().map(|held| json!({"pr": held.pr, "reasons": held.reasons})).collect::<Vec<_>>(),
    })
}

/// Render only graph structure as deterministic JSON.
pub fn render_graph_json(result: &PlanResult) -> String {
    json_pretty(&graph_value(result))
}

/// Render only graph structure as readable text.
pub fn render_graph_human(result: &PlanResult) -> String {
    let graph = &result.graph;
    let mut lines = vec![
        format!("Repository: {}  base: {}", graph.repository, graph.base),
        format!("{} open PR(s), {} real conflict(s), {} file-overlap risk(s), {} ordering edge(s), {} mechanism overlap(s)", graph.nodes.len(), graph.conflict_edges.len(), graph.overlap_edges.len(), graph.ordering_edges.len(), graph.mechanism_edges.len()),
        String::new(), "Stacks:".to_owned(),
    ];
    if result.stacks.is_empty() {
        lines.push("  (none)".into());
    } else {
        lines.extend(result.stacks.iter().map(|stack| {
            format!(
                "  {}",
                stack
                    .iter()
                    .map(|number| format!("#{number}"))
                    .collect::<Vec<_>>()
                    .join(" -> ")
            )
        }));
    }
    lines.extend([String::new(), "Real conflicts (git merge-tree):".into()]);
    if graph.conflict_edges.is_empty() {
        lines.push("  (none)".into());
    } else {
        for edge in &graph.conflict_edges {
            lines.push(edge_preview(edge.a, edge.b, &edge.paths));
        }
    }
    lines.extend([
        String::new(),
        "File-overlap risks (auto-mergeable but shared files):".into(),
    ]);
    if graph.overlap_edges.is_empty() {
        lines.push("  (none)".into());
    } else {
        for edge in &graph.overlap_edges {
            lines.push(edge_preview(edge.a, edge.b, &edge.paths));
        }
    }
    lines.extend([
        String::new(),
        "Mechanism overlaps (same mechanism — review together, may be opposite intent):".into(),
    ]);
    if graph.mechanism_edges.is_empty() {
        lines.push("  (none)".into());
    } else {
        lines.extend(graph.mechanism_edges.iter().map(|edge| {
            format!(
                "  #{} <-> #{}: {}",
                edge.a,
                edge.b,
                edge.mechanisms.join(", ")
            )
        }));
    }
    lines.extend([
        String::new(),
        "Unclassified mechanism candidates (recognise, then extend the enum):".into(),
    ]);
    if graph.unclassified_mechanisms.is_empty() {
        lines.push("  (none)".into());
    } else {
        lines.extend(
            graph
                .unclassified_mechanisms
                .iter()
                .map(|item| format!("  #{}: {}", item.pr, item.candidates.join(", "))),
        );
    }
    lines.extend([String::new(), "Held PRs:".into()]);
    if result.held.is_empty() {
        lines.push("  (none)".into());
    } else {
        lines.extend(result.held.iter().map(held_line));
    }
    lines.join("\n")
}

fn edge_preview(a: i64, b: i64, paths: &[String]) -> String {
    let preview = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    let more = if paths.len() > 5 {
        format!(" (+{} more)", paths.len() - 5)
    } else {
        String::new()
    };
    format!("  #{a} <-> #{b}: {preview}{more}")
}

/// Render per-PR status as deterministic JSON.
pub fn render_status_json(result: &PlanResult) -> String {
    let mut nodes: Vec<_> = result.graph.nodes.iter().collect();
    nodes.sort_by_key(|node| node.number);
    json_pretty(&json!({
        "repository": result.graph.repository,
        "base": result.graph.base,
        "prs": nodes.iter().map(|node| json!({
            "pr": node.number, "ci": node.ci.raw_state.as_str(),
            "red_class": node.ci.red_class.map(|red| red.as_str()), "draft": node.is_draft,
            "labels": node.labels, "title": node.title,
        })).collect::<Vec<_>>(),
        "summary": {
            "open": result.graph.nodes.len(),
            "passed": result.graph.nodes.iter().filter(|node| node.ci.raw_state == CiState::Passed).count(),
            "failed": result.graph.nodes.iter().filter(|node| node.ci.raw_state == CiState::Failed).count(),
            "no_result": result.graph.nodes.iter().filter(|node| node.ci.raw_state == CiState::NoResult).count(),
            "real_reds": result.diagnostics.real_reds.len(),
            "outage_suspected": result.diagnostics.outage_suspected,
        }
    }))
}

/// Render per-PR status and an optional open-count warning as readable text.
pub fn render_status_human(result: &PlanResult, warn_threshold: usize) -> String {
    let mut nodes: Vec<_> = result.graph.nodes.iter().collect();
    nodes.sort_by_key(|node| node.number);
    let mut lines = vec![
        format!("Open PR health: {}", result.graph.repository),
        String::new(),
    ];
    for node in &nodes {
        let red = node
            .ci
            .red_class
            .map(|red| format!(" [{}]", red.as_str()))
            .unwrap_or_default();
        let draft = if node.is_draft { " draft" } else { "" };
        let labels = if node.labels.is_empty() {
            String::new()
        } else {
            format!("  labels={}", node.labels.join(","))
        };
        lines.push(format!(
            "  #{:<5} ci={}{}{}{}  {}",
            node.number,
            node.ci.raw_state.as_str(),
            red,
            draft,
            labels,
            node.title
        ));
    }
    let reds = nodes
        .iter()
        .filter(|node| node.ci.raw_state == CiState::Failed)
        .count();
    lines.extend([
        String::new(),
        "Summary".into(),
        format!("  open:      {}", nodes.len()),
        format!("  ci-red:    {reds}"),
        format!("  real reds: {}", result.diagnostics.real_reds.len()),
    ]);
    if result.diagnostics.outage_suspected {
        lines.push("  SYSTEMIC RUNNER OUTAGE SUSPECTED".into());
    }
    if nodes.len() > warn_threshold {
        lines.push(format!(
            "  WARNING: {} open PRs exceeds the {} threshold; prioritize landing/CI repair.",
            nodes.len(),
            warn_threshold
        ));
    }
    lines.join("\n")
}

fn clusters(result: &PlanResult) -> Vec<Cluster> {
    cluster_by_conflict(
        &result.graph.nodes,
        &result.graph.conflict_edges,
        &result.graph.ordering_edges,
    )
}

/// Render conflict clusters and rebase economics as deterministic JSON.
pub fn render_clusters_json(result: &PlanResult) -> String {
    let clusters = clusters(result);
    let saved = rebases_avoided(&clusters);
    json_pretty(&json!({
        "repository": result.graph.repository,
        "base": result.graph.base,
        "clusters": clusters.iter().map(|cluster| json!({
            "members": cluster.members, "size": cluster.size(), "conflict_paths": cluster.conflict_paths,
            "rebases_avoided": cluster.rebases_avoided(),
        })).collect::<Vec<_>>(),
        "summary": {
            "open_prs": result.graph.nodes.len(), "clusters": clusters.len(),
            "multi_pr_clusters": clusters.iter().filter(|cluster| cluster.size() >= 2).count(),
            "singletons": clusters.iter().filter(|cluster| cluster.size() == 1).count(),
            "largest_cluster": clusters.iter().map(Cluster::size).max().unwrap_or(0),
            "parallel_lanes": clusters.len(), "rebases_avoided": saved, "validate_runs_avoided": saved,
        }
    }))
}

/// Render conflict clusters and rebase economics as readable text.
pub fn render_clusters_human(result: &PlanResult) -> String {
    let clusters = clusters(result);
    let stacks: Vec<_> = clusters
        .iter()
        .filter(|cluster| cluster.size() >= 2)
        .collect();
    let singleton_count = clusters
        .iter()
        .filter(|cluster| cluster.size() == 1)
        .count();
    let saved = rebases_avoided(&clusters);
    let mut lines = vec![
        format!("Repository: {}  base: {}", result.graph.repository, result.graph.base),
        format!("{} open PR(s), {} real conflict(s) => {} cluster(s): {} multi-PR stack(s), {} independent singleton(s)", result.graph.nodes.len(), result.graph.conflict_edges.len(), clusters.len(), stacks.len(), singleton_count),
        String::new(), "Conflict clusters land each as ONE stack (base -> tip):".into(),
    ];
    if stacks.is_empty() {
        lines.push("  (no multi-PR conflict clusters)".into());
    } else {
        for (index, cluster) in stacks.iter().enumerate() {
            lines.push(format!(
                "  stack {index}: {}  ({} PRs, {} rebases avoided)",
                cluster
                    .members
                    .iter()
                    .map(|number| format!("#{number}"))
                    .collect::<Vec<_>>()
                    .join(" -> "),
                cluster.size(),
                cluster.rebases_avoided()
            ));
            if !cluster.conflict_paths.is_empty() {
                let preview = cluster
                    .conflict_paths
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = if cluster.conflict_paths.len() > 5 {
                    format!(" (+{} more)", cluster.conflict_paths.len() - 5)
                } else {
                    String::new()
                };
                lines.push(format!("      shared conflict set: {preview}{more}"));
            }
        }
    }
    lines.extend([
        String::new(),
        "Parallel lanes (clusters share no conflict => land concurrently):".into(),
    ]);
    let lanes = clusters
        .iter()
        .map(|cluster| {
            if cluster.size() > 1 {
                format!("#{}(+{})", cluster.members[0], cluster.size() - 1)
            } else {
                format!("#{}", cluster.members[0])
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!(
        "  {}",
        if lanes.is_empty() { "(none)" } else { &lanes }
    ));
    lines.extend([
        String::new(), "Metric:".into(),
        format!("  rebases avoided by stacking = {saved} (serial landing of these clusters would cost {saved} extra rebase(s))"),
        format!("  validate runs avoided = {saved} (the validate record is SHA-keyed, so each avoided rebase is an avoided validate run)"),
        format!("  {VALIDATE_ECONOMICS_RATIONALE}"), String::new(),
        "Advisory only: this plan recommends; it never arms or merges anything.".into(),
    ]);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CollectedGraph, Diagnostics, Plan, PlanResult};

    #[test]
    fn empty_json_schema_is_stable_and_sorted() {
        let result = PlanResult {
            graph: CollectedGraph {
                repository: "o/r".into(),
                base: "main".into(),
                ..CollectedGraph::default()
            },
            stacks: vec![],
            held: vec![],
            plan: Plan::default(),
            diagnostics: Diagnostics::default(),
        };
        let output = render_json(&result);
        assert!(output.starts_with("{\n  \"base\""));
        assert!(output.contains("\"validate_record_keyed_to\": \"head_sha+base_sha\""));
        assert_eq!(render_actions(&result).lines().next(), Some("open_prs=0"));
    }

    #[test]
    fn canonical_json_recursively_sorts_keys_with_any_serde_json_map_backend() {
        let mut inner = Map::new();
        inner.insert("z".into(), json!(2));
        inner.insert("a".into(), json!(1));
        let mut array_item = Map::new();
        array_item.insert("b".into(), json!(4));
        array_item.insert("a".into(), json!(3));
        let mut outer = Map::new();
        outer.insert("z".into(), Value::Array(vec![Value::Object(array_item)]));
        outer.insert("a".into(), Value::Object(inner));

        assert_eq!(
            json_pretty(&Value::Object(outer)),
            "{\n  \"a\": {\n    \"a\": 1,\n    \"z\": 2\n  },\n  \"z\": [\n    {\n      \"a\": 3,\n      \"b\": 4\n    }\n  ]\n}"
        );
    }
}
