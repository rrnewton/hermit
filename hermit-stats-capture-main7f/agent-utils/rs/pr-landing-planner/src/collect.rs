//! Collection boundary: turn one host snapshot into a trustworthy graph.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::classify::{classify_pr, ClassifyConfig};
use crate::context::{
    apply_landing_context, has_comment_changes_requested, review_evidence_digest, LandingContext,
};
use crate::graph::{
    build_mechanism_edges, build_ordering_edges_base_ref, build_overlap_edges,
    build_unclassified_mechanisms, dedupe_ordering,
};
use crate::host::VcsHost;
use crate::model::{CollectedGraph, ConflictEdge, OrderingEdge, PrNode, RawPr};
use crate::priority::PriorityProvider;

/// Exact conflict detection using local `git merge-tree` probes.
pub const CONFLICT_DETECTOR_MERGE_TREE: &str = "merge-tree";
/// Conservative fallback that treats every shared file as a conflict.
pub const CONFLICT_DETECTOR_FILE_OVERLAP: &str = "file-overlap";

/// Inputs for one collection snapshot. Keeping these together makes the library boundary explicit
/// and leaves room for compatible collection options without a long positional argument list.
pub struct CollectOptions<'a> {
    /// Repository identifier understood by the host adapter.
    pub repo: &'a str,
    /// Target base branch.
    pub base: &'a str,
    /// Optional restriction to these PR numbers.
    pub only: Option<&'a BTreeSet<i64>>,
    /// Conflict detector name.
    pub conflict_detector: &'a str,
    /// CI classification configuration.
    pub classify_config: &'a ClassifyConfig,
    /// Priority source consulted once per selected PR.
    pub priority_provider: &'a mut dyn PriorityProvider,
    /// Caller-owned exact-identity evidence and policy context.
    pub landing_context: &'a [LandingContext],
}

impl<'a> CollectOptions<'a> {
    /// Construct options with exact merge-tree conflicts and no selection or context overrides.
    pub fn new(
        repo: &'a str,
        base: &'a str,
        classify_config: &'a ClassifyConfig,
        priority_provider: &'a mut dyn PriorityProvider,
    ) -> Self {
        Self {
            repo,
            base,
            only: None,
            conflict_detector: CONFLICT_DETECTOR_MERGE_TREE,
            classify_config,
            priority_provider,
            landing_context: &[],
        }
    }
}

/// Select PRs targeting a base, include transitive stacks, and apply an optional number filter.
pub fn select_prs(prs: &[RawPr], base: Option<&str>, only: Option<&BTreeSet<i64>>) -> Vec<RawPr> {
    let mut selected = prs.to_vec();
    if let Some(base) = base {
        let mut included: BTreeSet<_> = selected
            .iter()
            .filter(|pr| pr.base_ref == base)
            .map(|pr| pr.number)
            .collect();
        loop {
            let heads: BTreeSet<_> = selected
                .iter()
                .filter(|pr| included.contains(&pr.number))
                .map(|pr| pr.head_ref.as_str())
                .collect();
            let additions: Vec<_> = selected
                .iter()
                .filter(|pr| !included.contains(&pr.number) && heads.contains(pr.base_ref.as_str()))
                .map(|pr| pr.number)
                .collect();
            if additions.is_empty() {
                break;
            }
            included.extend(additions);
        }
        selected.retain(|pr| included.contains(&pr.number));
    }
    if let Some(only) = only {
        selected.retain(|pr| only.contains(&pr.number));
    }
    selected.sort_by_key(|pr| pr.number);
    selected
}

fn validate_pr_identities(prs: &[RawPr], base: &str) -> Result<(), String> {
    if base.is_empty() {
        return Err("base branch must be non-empty".to_owned());
    }
    let mut numbers = BTreeSet::new();
    let mut head_refs = BTreeSet::new();
    for pr in prs {
        if pr.number <= 0 {
            return Err(format!(
                "invalid PR number {:?}; expected a positive i64",
                pr.number
            ));
        }
        if !numbers.insert(pr.number) {
            return Err(format!(
                "duplicate PR number #{} in host snapshot",
                pr.number
            ));
        }
        if pr.head_ref.is_empty() || pr.base_ref.is_empty() || pr.api_head_sha.is_empty() {
            return Err(format!(
                "PR #{} is missing head_ref, base_ref, or API head SHA",
                pr.number
            ));
        }
        if !head_refs.insert(pr.head_ref.clone()) {
            return Err(format!(
                "duplicate head ref {:?} makes dependency ordering ambiguous",
                pr.head_ref
            ));
        }
    }
    Ok(())
}

fn local_base_ref(base: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(base.as_bytes()));
    format!("refs/pr-landing-planner/base-{}", &digest[..16])
}

/// Collect and validate one immutable host snapshot into a conflict and ordering graph.
pub fn collect_graph(
    host: &mut dyn VcsHost,
    options: CollectOptions<'_>,
) -> Result<CollectedGraph, String> {
    let CollectOptions {
        repo,
        base,
        only,
        conflict_detector,
        classify_config,
        priority_provider,
        landing_context,
    } = options;
    if !matches!(
        conflict_detector,
        CONFLICT_DETECTOR_MERGE_TREE | CONFLICT_DETECTOR_FILE_OVERLAP
    ) {
        return Err(format!(
            "unknown conflict detector {conflict_detector:?} (want merge-tree|file-overlap)"
        ));
    }
    classify_config.validate()?;
    let listed = host.list_open_prs(repo, Some(base))?;
    validate_pr_identities(&listed, base)?;
    let raw = select_prs(&listed, Some(base), only);
    let mut base_dest = BTreeMap::new();
    let mut pr_dest = BTreeMap::new();
    let mut refspecs = Vec::new();
    for pr in &raw {
        if !base_dest.contains_key(&pr.base_ref) {
            let dest = local_base_ref(&pr.base_ref);
            base_dest.insert(pr.base_ref.clone(), dest.clone());
            refspecs.push((format!("refs/heads/{}", pr.base_ref), dest));
        }
    }
    for pr in &raw {
        let dest = format!("refs/pr-landing-planner/pr-{}", pr.number);
        pr_dest.insert(pr.number, dest.clone());
        refspecs.push((format!("refs/pull/{}/head", pr.number), dest));
    }
    let resolved = host.prefetch_refs(&refspecs)?;
    let mut nodes = Vec::new();
    for pr in raw {
        let base_key = &base_dest[&pr.base_ref];
        let head_key = &pr_dest[&pr.number];
        let base_sha = resolved
            .get(base_key)
            .cloned()
            .ok_or_else(|| format!("host did not resolve fetched ref {base_key}"))?;
        let head_sha = resolved
            .get(head_key)
            .cloned()
            .ok_or_else(|| format!("host did not resolve fetched ref {head_key}"))?;
        if base_sha.is_empty() || head_sha.is_empty() {
            return Err(format!(
                "PR #{} resolved to an empty base or head SHA",
                pr.number
            ));
        }
        if !pr.api_head_sha.is_empty() && head_sha != pr.api_head_sha {
            return Err(format!(
                "PR #{} changed during collection: API={}, fetched={}; rerun",
                pr.number, pr.api_head_sha, head_sha
            ));
        }
        let files = host.changed_files(&base_sha, &head_sha)?;
        let mut base_conflict_paths = host.merge_tree(&base_sha, &head_sha)?;
        base_conflict_paths.sort();
        let commits_behind = host.commits_behind(&head_sha, &base_sha)?;
        if commits_behind < 0 {
            return Err(format!(
                "PR #{} host returned negative commits-behind value {commits_behind}",
                pr.number
            ));
        }
        let priority = priority_provider.priority(pr.number, &pr.labels);
        let mut review_decision = pr.review_decision.clone();
        let review_digest = if let Some(snapshot) = &pr.review_snapshot {
            if snapshot.head_sha != head_sha {
                return Err(format!(
                    "PR #{} review evidence changed during collection: snapshot={}, fetched={}; rerun",
                    pr.number, snapshot.head_sha, head_sha
                ));
            }
            if !review_decision.is_empty() && snapshot.review_decision != review_decision {
                return Err(format!(
                    "PR #{} aggregate review decision changed during collection: list={:?}, snapshot={:?}; rerun",
                    pr.number, review_decision, snapshot.review_decision
                ));
            }
            if !snapshot.review_decision.is_empty() {
                review_decision = snapshot.review_decision.clone();
            }
            let digest = review_evidence_digest(snapshot).map_err(|error| {
                format!(
                    "PR #{} review evidence is not safely identifiable: {error}",
                    pr.number
                )
            })?;
            if matches!(review_decision.as_str(), "" | "APPROVED")
                && has_comment_changes_requested(snapshot)
            {
                review_decision = "CHANGES_REQUESTED".to_owned();
            }
            digest
        } else {
            String::new()
        };
        nodes.push(PrNode {
            number: pr.number,
            head_ref: pr.head_ref,
            base_ref: pr.base_ref,
            head_sha,
            base_sha,
            title: pr.title,
            author: pr.author,
            is_draft: pr.is_draft,
            mergeable: pr.mergeable,
            review_decision,
            review_evidence_unavailable: pr.review_evidence_unavailable,
            created_at: pr.created_at,
            updated_at: pr.updated_at,
            additions: pr.additions,
            deletions: pr.deletions,
            labels: pr.labels,
            mechanism_symbols: pr.mechanism_symbols,
            files,
            base_conflict_paths,
            commits_behind,
            ci: classify_pr(&pr.checks, classify_config),
            priority,
            review_evidence_digest: review_digest,
            ..PrNode::default()
        });
    }
    let nodes = apply_landing_context(nodes, landing_context)?;
    let mut conflict_edges = Vec::new();
    for (index, a) in nodes.iter().enumerate() {
        for b in nodes.iter().skip(index + 1) {
            let mut paths = if conflict_detector == CONFLICT_DETECTOR_FILE_OVERLAP {
                a.files.intersection(&b.files).cloned().collect()
            } else {
                host.merge_tree(&a.head_sha, &b.head_sha)?
            };
            paths.sort();
            if !paths.is_empty() {
                conflict_edges.push(ConflictEdge {
                    a: a.number,
                    b: b.number,
                    paths,
                });
            }
        }
    }
    let overlap_edges = build_overlap_edges(&nodes);
    let mut ordering = build_ordering_edges_base_ref(&nodes);
    for (index, a) in nodes.iter().enumerate() {
        for b in nodes.iter().skip(index + 1) {
            if a.head_sha == b.head_sha {
                continue;
            }
            if host.is_ancestor(&a.head_sha, &b.head_sha)? {
                ordering.push(OrderingEdge {
                    before: a.number,
                    after: b.number,
                    reason: "ancestry".to_owned(),
                });
            } else if host.is_ancestor(&b.head_sha, &a.head_sha)? {
                ordering.push(OrderingEdge {
                    before: b.number,
                    after: a.number,
                    reason: "ancestry".to_owned(),
                });
            }
        }
    }
    let ordering_edges = dedupe_ordering(&ordering);
    Ok(CollectedGraph {
        repository: repo.to_owned(),
        base: base.to_owned(),
        mechanism_edges: build_mechanism_edges(&nodes),
        unclassified_mechanisms: build_unclassified_mechanisms(&nodes),
        nodes,
        conflict_edges,
        overlap_edges,
        ordering_edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{load_fixture_text, FakeHost};
    use crate::priority::NonePriority;

    fn raw(number: i64, base: &str, head: &str) -> RawPr {
        RawPr {
            number,
            base_ref: base.into(),
            head_ref: head.into(),
            ..RawPr::default()
        }
    }

    #[test]
    fn selection_includes_transitive_stacks_then_applies_only() {
        let prs = vec![
            raw(1, "main", "a"),
            raw(2, "a", "b"),
            raw(3, "b", "c"),
            raw(4, "other", "d"),
        ];
        assert_eq!(
            select_prs(&prs, Some("main"), None)
                .iter()
                .map(|p| p.number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            select_prs(&prs, Some("main"), Some(&BTreeSet::from([2]))).len(),
            1
        );
    }

    #[test]
    fn fixture_collection_builds_real_and_semantic_dimensions() {
        let value = load_fixture_text(
            r#"
repo: owner/repo
base: main
prs:
  - number: 1
    head_ref: root
    changed_files: [shared.rs, one.rs]
    commits_behind: 4
    mechanism_symbols: [CANCEL_IN_PROGRESS]
    checks: [{name: merge-gate, conclusion: SUCCESS}]
  - number: 2
    head_ref: child
    base_ref: root
    changed_files: [shared.rs]
    labels: [mechanism:cancel-in-progress]
    checks: [{name: merge-gate, conclusion: SUCCESS}]
conflicts: [{a: 1, b: 2, paths: [shared.rs, shared.rs]}]
ancestry: [{before: 1, after: 2}]
"#,
            true,
        )
        .unwrap();
        let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
        let mut priority = NonePriority;
        let classify = ClassifyConfig::default();
        let graph = collect_graph(
            &mut host,
            CollectOptions::new(&repo, &base, &classify, &mut priority),
        )
        .unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.nodes[0].commits_behind, 4);
        assert_eq!(graph.conflict_edges.len(), 1);
        assert_eq!(
            graph.conflict_edges[0].paths,
            vec!["shared.rs", "shared.rs"]
        );
        assert_eq!(graph.overlap_edges.len(), 1);
        assert_eq!(graph.ordering_edges.len(), 1);
        assert_eq!(graph.ordering_edges[0].reason, "base-ref");
        assert_eq!(
            graph.mechanism_edges[0].mechanisms,
            vec!["cancel-in-progress"]
        );
    }

    #[test]
    fn content_identity_guard_aborts_before_planning() {
        let value = load_fixture_text(
            r#"{"repo":"r","base":"main","prs":[{
            "number":1,"api_head_sha":"old","fetched_head_sha":"new"
        }]}"#,
            false,
        )
        .unwrap();
        let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
        let mut priority = NonePriority;
        let classify = ClassifyConfig::default();
        let error = collect_graph(
            &mut host,
            CollectOptions::new(&repo, &base, &classify, &mut priority),
        )
        .unwrap_err();
        assert!(error.contains("changed during collection"));
        assert!(error.contains("rerun"));
    }

    #[test]
    fn review_snapshot_head_must_match_the_exact_fetched_head() {
        let value = load_fixture_text(
            r#"{"repo":"r","base":"main","prs":[{
            "number":1,
            "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "api_head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "fetched_head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "review_snapshot_head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "review_events":[]
        }]}"#,
            false,
        )
        .unwrap();
        let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
        let mut priority = NonePriority;
        let classify = ClassifyConfig::default();
        let error = collect_graph(
            &mut host,
            CollectOptions::new(&repo, &base, &classify, &mut priority),
        )
        .unwrap_err();
        assert!(error.contains("review evidence changed during collection"));
    }

    #[test]
    fn review_snapshot_cannot_erase_or_contradict_changes_requested() {
        for snapshot_decision in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("APPROVED"),
        ] {
            let value = serde_json::json!({
                "repo":"r",
                "base":"main",
                "prs":[{
                    "number":1,
                    "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "review_decision":"CHANGES_REQUESTED",
                    "review_snapshot_review_decision":snapshot_decision,
                    "review_events":[{
                        "kind":"review",
                        "identity":"review-1",
                        "author":"reviewer",
                        "state":"CHANGES_REQUESTED",
                        "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "created_at":"2026-09-04T12:00:00Z",
                        "updated_at":"2026-09-04T12:00:00Z",
                        "last_edited_at":"",
                        "body":"please fix"
                    }]
                }]
            });
            let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
            let mut priority = NonePriority;
            let classify = ClassifyConfig::default();
            let error = collect_graph(
                &mut host,
                CollectOptions::new(&repo, &base, &classify, &mut priority),
            )
            .unwrap_err();
            assert!(error.contains("aggregate review decision changed"));
        }
    }

    #[test]
    fn matching_review_decisions_are_accepted() {
        let value = serde_json::json!({
            "repo":"r",
            "base":"main",
            "prs":[{
                "number":1,
                "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "review_decision":"CHANGES_REQUESTED",
                "review_snapshot_review_decision":"CHANGES_REQUESTED",
                "review_events":[{
                    "kind":"review",
                    "identity":"review-1",
                    "author":"reviewer",
                    "state":"CHANGES_REQUESTED",
                    "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "created_at":"2026-09-04T12:00:00Z",
                    "updated_at":"2026-09-04T12:00:00Z",
                    "last_edited_at":"",
                    "body":"please fix"
                }]
            }]
        });
        let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
        let mut priority = NonePriority;
        let classify = ClassifyConfig::default();
        let graph = collect_graph(
            &mut host,
            CollectOptions::new(&repo, &base, &classify, &mut priority),
        )
        .unwrap();
        assert_eq!(graph.nodes[0].review_decision, "CHANGES_REQUESTED");
    }

    #[test]
    fn canonical_comment_refusal_holds_without_native_review_decision() {
        let value = serde_json::json!({
            "repo":"r",
            "base":"main",
            "prs":[{
                "number":1,
                "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "review_decision":"",
                "review_snapshot_review_decision":"",
                "review_events":[{
                    "kind":"issue-comment",
                    "identity":"comment-123456",
                    "author":"reviewer",
                    "state":"ACTIVE",
                    "head_sha":"",
                    "created_at":"2026-09-04T12:00:00Z",
                    "updated_at":"2026-09-04T12:00:00Z",
                    "last_edited_at":"",
                    "body":concat!(
                        "[team, reviewer, unresolved, build-host, role=reviewer]\n",
                        "CHANGES-REQUESTED-AT: codex ",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
                        "The race remains."
                    )
                }]
            }]
        });
        let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
        let mut priority = NonePriority;
        let classify = ClassifyConfig::default();
        let graph = collect_graph(
            &mut host,
            CollectOptions::new(&repo, &base, &classify, &mut priority),
        )
        .unwrap();
        assert_eq!(graph.nodes[0].review_decision, "CHANGES_REQUESTED");
    }

    #[test]
    fn review_evidence_unavailable_survives_collection() {
        let value = serde_json::json!({
            "repo":"r",
            "base":"main",
            "prs":[{
                "number":1,
                "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "review_decision":"APPROVED",
                "review_evidence_unavailable":true,
                "checks":[{"name":"merge-gate","conclusion":"SUCCESS"}]
            }]
        });
        let (mut host, repo, base) = FakeHost::from_value(&value).unwrap();
        let mut priority = NonePriority;
        let classify = ClassifyConfig::default();
        let graph = collect_graph(
            &mut host,
            CollectOptions::new(&repo, &base, &classify, &mut priority),
        )
        .unwrap();
        assert_eq!(graph.nodes[0].review_decision, "APPROVED");
        assert!(graph.nodes[0].review_evidence_unavailable);
        assert!(graph.nodes[0].review_evidence_digest.is_empty());
    }
}
