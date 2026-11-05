//! Core vocabulary: pure data and enums, with no I/O.

use std::collections::{BTreeMap, BTreeSet};

/// Empty repository default, forcing live callers to choose one explicitly.
pub const DEFAULT_REPO: &str = "";
/// Default target branch.
pub const DEFAULT_BASE: &str = "main";
/// Default required landing-gate check name.
pub const DEFAULT_GATE_CHECK: &str = "merge-gate";
/// Complete native GitHub pull-request review state vocabulary accepted as evidence.
pub const NATIVE_REVIEW_STATES: [&str; 5] = [
    "APPROVED",
    "CHANGES_REQUESTED",
    "COMMENTED",
    "DISMISSED",
    "PENDING",
];

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
/// Three-state interpretation of CI check results.
pub enum CiState {
    /// Every selected check passed.
    Passed,
    /// At least one selected check failed.
    Failed,
    /// At least one required result is absent, pending, neutral, or unknown.
    #[default]
    NoResult,
}

impl CiState {
    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::NoResult => "no-result",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Refined reason for a failed or apparently failed rollup.
pub enum RedClass {
    /// Genuine regression requiring a fix.
    Real,
    /// All failures match configured flaky signatures.
    Flaky,
    /// Underlying checks passed while the required gate is stale-red.
    StaleRequiredCheck,
    /// The gate ran before its prerequisite work completed.
    EvaluateOnceRace,
    /// The required gate job did not actually execute.
    RunnerOutage,
}

impl RedClass {
    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Real => "real",
            Self::Flaky => "flaky",
            Self::StaleRequiredCheck => "stale-required-check",
            Self::EvaluateOnceRace => "evaluate-once-race",
            Self::RunnerOutage => "runner-outage",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
/// Advisory next action for one pull request.
pub enum PrAction {
    /// Land the current identities after respecting emitted ordering.
    LandNow,
    /// Rebase, revalidate the new identities, and then land.
    RebaseThenLand,
    /// Re-dispatch only the stale required gate.
    RefireStaleGate,
    /// Escalate a runner-infrastructure outage.
    EscalateRunnerOutage,
    /// Escalate a change to landing-gate policy.
    EscalateGatePolicy,
    /// Re-dispatch CI.
    RefireCi,
    /// Hold for a genuine failure to be fixed.
    HoldFix,
    /// Wait for a prerequisite or operator action.
    Wait,
}

impl PrAction {
    /// Complete stable action vocabulary.
    pub const ALL: [Self; 8] = [
        Self::LandNow,
        Self::RebaseThenLand,
        Self::RefireStaleGate,
        Self::EscalateRunnerOutage,
        Self::EscalateGatePolicy,
        Self::RefireCi,
        Self::HoldFix,
        Self::Wait,
    ];

    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LandNow => "land-now",
            Self::RebaseThenLand => "rebase-then-land",
            Self::RefireStaleGate => "refire-stale-gate",
            Self::EscalateRunnerOutage => "escalate-runner-outage",
            Self::EscalateGatePolicy => "escalate-gate-policy",
            Self::RefireCi => "refire-ci",
            Self::HoldFix => "hold-fix",
            Self::Wait => "wait",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Validation evidence attached to the collected PR identities.
pub enum ValidationEvidence {
    /// No validation evidence.
    #[default]
    None,
    /// Required repository-host CI passed.
    AuthoritativeCi,
    /// Caller-supplied dereferenced local record bound to fetched head and base identities.
    /// A bare label never produces this variant.
    LocallyValidated,
    /// Caller record bound to the exact fetched head and base SHAs.
    CleanValidateRecord,
}

impl ValidationEvidence {
    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AuthoritativeCi => "authoritative-ci",
            Self::LocallyValidated => "locally-validated",
            Self::CleanValidateRecord => "clean-validate-record",
        }
    }

    /// Parse a machine-facing validation-evidence value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "authoritative-ci" => Some(Self::AuthoritativeCi),
            "locally-validated" => Some(Self::LocallyValidated),
            "clean-validate-record" => Some(Self::CleanValidateRecord),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Consuming-workspace decision about whether recorded validation authorizes landing.
pub enum ValidationAuthority {
    /// No hard/soft-green authorization was supplied.
    #[default]
    None,
    /// The consuming workspace classified the evidence as hard green.
    HardGreen,
    /// The consuming workspace classified the evidence as soft green.
    SoftGreen,
}

impl ValidationAuthority {
    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HardGreen => "hard-green",
            Self::SoftGreen => "soft-green",
        }
    }

    /// Parse a machine-facing validation-authority value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "hard-green" => Some(Self::HardGreen),
            "soft-green" => Some(Self::SoftGreen),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Binding state of required adversarial-review PASS receipts.
pub enum ReviewBinding {
    /// No review-protocol label or receipt is present.
    #[default]
    NotRequired,
    /// One or more required review lanes have no PASS label.
    Missing,
    /// A PASS label exists without a receipt naming the reviewed head SHA.
    Unbound,
    /// A PASS receipt names a head other than the current fetched head.
    Stale,
    /// Every required lane has a PASS label and receipt for the current head.
    ExactHead,
}

impl ReviewBinding {
    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not-required",
            Self::Missing => "missing",
            Self::Unbound => "unbound",
            Self::Stale => "stale",
            Self::ExactHead => "exact-head",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Whether a change alters landing-gate policy.
pub enum PolicyClass {
    /// No explicit policy classification.
    #[default]
    Unclassified,
    /// Routine CI maintenance that does not alter gate policy.
    CiHygiene,
    /// Gate-policy change requiring coordinator review.
    GatePolicy,
}

impl PolicyClass {
    /// Return the stable machine-facing value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::CiHygiene => "ci-hygiene",
            Self::GatePolicy => "gate-policy",
        }
    }

    /// Parse a machine-facing policy value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unclassified" => Some(Self::Unclassified),
            "ci-hygiene" => Some(Self::CiHygiene),
            "gate-policy" => Some(Self::GatePolicy),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// One narrowed status-check attempt.
pub struct CheckRun {
    /// Check context or name.
    pub name: String,
    /// Repository-host execution status.
    pub status: String,
    /// Repository-host terminal conclusion.
    pub conclusion: String,
    /// Optional message used for configured signatures.
    pub text: String,
    /// Optional owning workflow name.
    pub workflow: String,
    /// Runtime when known.
    pub duration_secs: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
/// One stable repository-host event that can change objection authority.
pub struct ReviewEvidenceEvent {
    /// Event class, such as `review`, `issue-comment`, or `review-comment`.
    pub kind: String,
    /// Stable repository-host identity within that event class.
    pub identity: String,
    /// Immutable repository-host login when available; authority only with verified permission.
    pub author: String,
    /// Current event state, including dismissal or retirement state when surfaced.
    pub state: String,
    /// Exact event head when the source supplies one; otherwise canonical empty.
    pub head_sha: String,
    /// Immutable creation/submission timestamp supplied by the host.
    pub created_at: String,
    /// Current object-version timestamp supplied by the host.
    pub updated_at: String,
    /// Native review body-edit timestamp; canonical empty for unedited/non-review events.
    pub last_edited_at: String,
    /// Event text; included because edits can create or remove an objection.
    pub body: String,
    /// Current repository permission for an exact retirement's immutable GitHub event author.
    /// Empty for every event that is not an exact retirement.
    pub retirement_actor_permission: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Exact review/comment evidence observed together at one PR head.
pub struct ReviewEvidenceSnapshot {
    /// PR head observed with this event set.
    pub head_sha: String,
    /// Aggregate repository-host review decision observed with the event set.
    pub review_decision: String,
    /// Every surfaced event that can affect objection authority.
    pub events: Vec<ReviewEvidenceEvent>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Pull-request metadata returned by a repository host before local collection.
pub struct RawPr {
    /// Positive PR number.
    pub number: i64,
    /// Source branch name.
    pub head_ref: String,
    /// Target branch name.
    pub base_ref: String,
    /// Head SHA observed by the repository host.
    pub api_head_sha: String,
    /// PR title.
    pub title: String,
    /// Author login or display name.
    pub author: String,
    /// Whether the PR is a draft.
    pub is_draft: bool,
    /// Repository-host mergeability token.
    pub mergeable: String,
    /// Repository-host review-decision token.
    pub review_decision: String,
    /// Whether the host could not produce the complete exact-head review snapshot.
    pub review_evidence_unavailable: bool,
    /// Creation timestamp used as a deterministic tie-breaker.
    pub created_at: String,
    /// Last-update timestamp when supplied by the host.
    pub updated_at: String,
    /// Added line count.
    pub additions: i64,
    /// Deleted line count.
    pub deletions: i64,
    /// PR label names.
    pub labels: Vec<String>,
    /// Selected check attempts.
    pub checks: Vec<CheckRun>,
    /// Exact-head review/comment snapshot, absent when host enrichment failed.
    pub review_snapshot: Option<ReviewEvidenceSnapshot>,
    /// Mechanism candidates derived by the host adapter.
    pub mechanism_symbols: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Refined CI verdict for one PR.
pub struct CiVerdict {
    /// Three-state aggregate over selected checks.
    pub raw_state: CiState,
    /// Failure class when a red-like anomaly is present.
    pub red_class: Option<RedClass>,
    /// Whether the required gate appeared in the rollup.
    pub gate_present: bool,
    /// Whether the required gate passed.
    pub gate_ok: bool,
    /// Whether the gate has a never-ran outage signature.
    pub gate_missing_run: bool,
    /// Operator-facing explanation.
    pub detail: String,
}

impl Default for CiVerdict {
    fn default() -> Self {
        Self {
            raw_state: CiState::NoResult,
            red_class: None,
            gate_present: false,
            gate_ok: false,
            gate_missing_run: false,
            detail: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Fully collected PR metadata and exact fetched repository state.
pub struct PrNode {
    /// Positive PR number.
    pub number: i64,
    /// Source branch name.
    pub head_ref: String,
    /// Target branch name.
    pub base_ref: String,
    /// Exact fetched head SHA.
    pub head_sha: String,
    /// Exact fetched base SHA.
    pub base_sha: String,
    /// PR title.
    pub title: String,
    /// Author login or display name.
    pub author: String,
    /// Whether the PR is a draft.
    pub is_draft: bool,
    /// Repository-host mergeability token.
    pub mergeable: String,
    /// Repository-host review-decision token.
    pub review_decision: String,
    /// Whether review evidence collection was incomplete or indeterminate.
    pub review_evidence_unavailable: bool,
    /// Creation timestamp used as a tie-breaker.
    pub created_at: String,
    /// Last-update timestamp observed from the repository host.
    pub updated_at: String,
    /// Added line count.
    pub additions: i64,
    /// Deleted line count.
    pub deletions: i64,
    /// PR labels.
    pub labels: Vec<String>,
    /// Mechanism candidates derived from labels or changes.
    pub mechanism_symbols: Vec<String>,
    /// Files changed from merge base through head.
    pub files: BTreeSet<String>,
    /// Paths conflicting with the fetched base.
    pub base_conflict_paths: Vec<String>,
    /// Number of fetched-base commits absent from the head.
    pub commits_behind: i64,
    /// Refined CI verdict.
    pub ci: CiVerdict,
    /// Resolved priority; lower values sort first.
    pub priority: i64,
    /// Optional caller-assigned operator or agent.
    pub assigned_agent: String,
    /// Validation evidence associated with this snapshot.
    pub validation_evidence: ValidationEvidence,
    /// Consuming-workspace hard/soft-green authorization for that evidence.
    pub validation_authority: ValidationAuthority,
    /// Caller-verified review lane to exact reviewed-head SHA receipts.
    pub review_pass_heads: BTreeMap<String, String>,
    /// Digest of the exact review/comment event snapshot collected at the head.
    pub review_evidence_digest: String,
    /// Caller authority, bound to the exact head and review-evidence digest,
    /// that every recorded review objection has an exact resolution record.
    pub review_objections_resolved: bool,
    /// Landing-policy classification.
    pub policy_class: PolicyClass,
}

impl PrNode {
    /// Return total changed lines.
    pub const fn size(&self) -> i64 {
        self.additions + self.deletions
    }

    /// Return whether local or repository-host evidence reports a base conflict.
    pub fn base_conflicting(&self) -> bool {
        !self.base_conflict_paths.is_empty() || self.mergeable == "CONFLICTING"
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Confirmed merge conflict between two PR heads.
pub struct ConflictEdge {
    /// First PR number.
    pub a: i64,
    /// Second PR number.
    pub b: i64,
    /// Conflicting paths.
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Informational changed-file overlap between two PRs.
pub struct OverlapEdge {
    /// First PR number.
    pub a: i64,
    /// Second PR number.
    pub b: i64,
    /// Shared changed paths.
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Semantic overlap between two PRs affecting the same recognized mechanism.
pub struct MechanismEdge {
    /// First PR number.
    pub a: i64,
    /// Second PR number.
    pub b: i64,
    /// Canonical shared mechanism slugs.
    pub mechanisms: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Stack-ordered connected component of the real-conflict graph.
pub struct Cluster {
    /// PR numbers in recommended stack order.
    pub members: Vec<i64>,
    /// Union of conflict paths inside the cluster.
    pub conflict_paths: Vec<String>,
}

impl Cluster {
    /// Return the number of PRs in the cluster.
    pub fn size(&self) -> usize {
        self.members.len()
    }

    /// Return serial rebases avoided by stacking the cluster once.
    pub fn rebases_avoided(&self) -> usize {
        self.size().saturating_sub(1)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Mechanism candidates for one PR that have no recognized classification.
pub struct UnclassifiedMechanism {
    /// PR number.
    pub pr: i64,
    /// Unrecognized candidate strings.
    pub candidates: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Directed dependency requiring one PR to land before another.
pub struct OrderingEdge {
    /// Predecessor PR number.
    pub before: i64,
    /// Dependent PR number.
    pub after: i64,
    /// Evidence source such as base-ref stacking or ancestry.
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// PR excluded from direct landing with explicit reasons.
pub struct HeldPr {
    /// PR number.
    pub pr: i64,
    /// Stable hold-reason strings.
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Immutable collection snapshot containing nodes and all relationship dimensions.
pub struct CollectedGraph {
    /// Repository identifier.
    pub repository: String,
    /// Requested base branch.
    pub base: String,
    /// Fully collected PR nodes.
    pub nodes: Vec<PrNode>,
    /// Confirmed merge-conflict edges.
    pub conflict_edges: Vec<ConflictEdge>,
    /// Informational file-overlap edges.
    pub overlap_edges: Vec<OverlapEdge>,
    /// Directed dependency edges.
    pub ordering_edges: Vec<OrderingEdge>,
    /// Recognized mechanism-overlap edges.
    pub mechanism_edges: Vec<MechanismEdge>,
    /// Unrecognized mechanism candidates.
    pub unclassified_mechanisms: Vec<UnclassifiedMechanism>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One PR's advisory action and explanation.
pub struct PrActionDecision {
    /// PR number.
    pub pr: i64,
    /// Recommended next action.
    pub action: PrAction,
    /// Operator-facing rationale.
    pub why: String,
    /// Parallel-safe group index, absent for held PRs.
    pub group: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Deterministic advisory landing plan.
pub struct Plan {
    /// Conflict- and ordering-safe layers.
    pub parallel_safe_groups: Vec<Vec<i64>>,
    /// PRs landable in emitted order at current identities.
    pub land_now: Vec<i64>,
    /// Flattened recommended action order.
    pub order: Vec<i64>,
    /// One action decision per PR.
    pub per_pr_actions: Vec<PrActionDecision>,
    /// Optional conflict- and dependency-free batch of root PRs.
    pub batch: Vec<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// First-class CI anomaly summaries.
pub struct Diagnostics {
    /// PRs with stale required gates.
    pub stale_gates: Vec<i64>,
    /// PRs whose failures all match flaky signatures.
    pub flaky_reds: Vec<i64>,
    /// PRs with genuine failures.
    pub real_reds: Vec<i64>,
    /// PRs exhibiting evaluate-once races.
    pub evaluate_once_race: Vec<i64>,
    /// PRs exhibiting runner-outage signatures.
    pub outage_prs: Vec<i64>,
    /// Whether the configured systemic-outage threshold is met.
    pub outage_suspected: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Complete graph, hold, plan, stack, and diagnostic result.
pub struct PlanResult {
    /// Collected immutable graph.
    pub graph: CollectedGraph,
    /// Root-to-leaf dependency chains.
    pub stacks: Vec<Vec<i64>>,
    /// Structurally or administratively held PRs.
    pub held: Vec<HeldPr>,
    /// Advisory plan.
    pub plan: Plan,
    /// CI diagnostics.
    pub diagnostics: Diagnostics,
}

/// Canonicalize an undirected PR pair with the lower number first.
pub const fn edge_key(a: i64, b: i64) -> (i64, i64) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Remove duplicate labels while preserving their first-seen order.
pub fn dedupe_priority(labels: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    labels
        .iter()
        .filter(|label| seen.insert((*label).clone()))
        .cloned()
        .collect()
}
