//! Caller-owned landing context bound to exact fetched head and base identities.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::{
    CiState, PolicyClass, PrNode, ReviewEvidenceSnapshot, ValidationAuthority, ValidationEvidence,
};

/// Label prefix carrying an assigned-agent identifier.
pub const AGENT_PREFIX: &str = "agent:";
/// Label prefix carrying a [`PolicyClass`].
pub const POLICY_PREFIX: &str = "landing-policy:";
/// Review lanes required by the adversarial-review protocol.
pub const REQUIRED_REVIEW_LANES: [&str; 2] = ["codex", "claude"];
/// Repository permission levels that may authoritatively retire an objection.
pub const ALLOWED_RETIREMENT_PERMISSIONS: [&str; 4] = ["triage", "write", "maintain", "admin"];

fn regex<'a>(slot: &'a OnceLock<Regex>, pattern: &str) -> &'a Regex {
    slot.get_or_init(|| Regex::new(pattern).expect("static review-evidence regex is valid"))
}

fn trim_ascii_whitespace(value: &str) -> &str {
    value.trim_matches(|character: char| character.is_ascii_whitespace())
}

fn prose_line_indexes(body: &str) -> Vec<usize> {
    static FENCE: OnceLock<Regex> = OnceLock::new();
    let fence_re = regex(&FENCE, r"^ {0,3}(?P<f>`{3,}|~{3,})[ \t]*(?P<info>.*)$");
    let mut indexes = Vec::new();
    let mut fence = String::new();
    let mut indented = false;
    let mut previous_blank = true;
    for (index, raw) in body.split('\n').enumerate() {
        let blank = trim_ascii_whitespace(raw).is_empty();
        if !fence.is_empty() {
            if let Some(captures) = fence_re.captures(raw) {
                let token = captures.name("f").map(|value| value.as_str()).unwrap_or("");
                let info = captures
                    .name("info")
                    .map(|value| value.as_str())
                    .unwrap_or("");
                if token.starts_with(&fence[..1])
                    && token.len() >= fence.len()
                    && trim_ascii_whitespace(info).is_empty()
                {
                    fence.clear();
                }
            }
            previous_blank = blank;
            continue;
        }
        if let Some(captures) = fence_re.captures(raw) {
            fence = captures
                .name("f")
                .map(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            indented = false;
            previous_blank = false;
            continue;
        }
        if indented {
            if blank {
                previous_blank = true;
                continue;
            }
            if raw.starts_with("    ") || raw.starts_with('\t') {
                continue;
            }
            indented = false;
        } else if previous_blank && (raw.starts_with("    ") || raw.starts_with('\t')) && !blank {
            indented = true;
            previous_blank = false;
            continue;
        }
        indexes.push(index);
        previous_blank = blank;
    }
    indexes
}

fn prose_lines(body: &str) -> Vec<&str> {
    let lines = body.split('\n').collect::<Vec<_>>();
    prose_line_indexes(body)
        .into_iter()
        .map(|index| lines[index])
        .collect()
}

fn undecorate(line: &str) -> String {
    static BLOCK_PREFIX: OnceLock<Regex> = OnceLock::new();
    let mut normalized = regex(&BLOCK_PREFIX, r"^(?:#{1,6}[ \t]+|[-+*][ \t]+)")
        .replace(trim_ascii_whitespace(line), "")
        .trim_matches(|character: char| character.is_ascii_whitespace())
        .to_owned();
    loop {
        let mut changed = false;
        for wrapper in ["`", "**", "__", "*", "_"] {
            if normalized.starts_with(wrapper)
                && normalized.ends_with(wrapper)
                && normalized.len() > 2 * wrapper.len()
            {
                normalized = normalized[wrapper.len()..normalized.len() - wrapper.len()]
                    .trim_matches(|character: char| character.is_ascii_whitespace())
                    .to_owned();
                changed = true;
                break;
            }
        }
        if !changed {
            return normalized;
        }
    }
}

fn marker_matches(pattern: &Regex, line: &str) -> bool {
    let normalized = undecorate(line);
    pattern.find(&normalized).is_some_and(|matched| {
        matched.start() == 0
            && (matched.end() == normalized.len()
                || matches!(normalized.as_bytes()[matched.end()], b' ' | b'\t'))
    })
}

fn normalized_review_body(body: &str) -> String {
    static DISCLOSURE: OnceLock<Regex> = OnceLock::new();
    static REVIEW_MARKER: OnceLock<Regex> = OnceLock::new();
    static BY_IDENTITY: OnceLock<Regex> = OnceLock::new();
    static WHO_METADATA: OnceLock<Regex> = OnceLock::new();
    let disclosure = regex(
        &DISCLOSURE,
        r"^\[[A-Za-z0-9_.-]+,[ \t]*[A-Za-z0-9_.-]+,[ \t]*[^,\[\]\r\n]+,[ \t]*[A-Za-z0-9_.-]+,[ \t]*(?i-u:role)=[A-Za-z0-9_.-]+\]\r?$",
    );
    let review_marker = regex(
        &REVIEW_MARKER,
        r"^(?i-u:CHANGES-REQUESTED-WITHDRAWN-AT|CHANGES-REQUESTED-AT|APPROVED-AT):[ \t]*(?i-u:claude|codex)[ \t]+(?i-u:[0-9a-f]{40})",
    );
    let by_identity = regex(&BY_IDENTITY, r"[ \t]+(?i-u:BY)[ \t]+[A-Za-z0-9_.-]+$");
    let who_metadata = regex(
        &WHO_METADATA,
        r"^Unverified --who metadata: [A-Za-z0-9_.-]+\r?$",
    );
    let lines = body.split('\n').collect::<Vec<_>>();
    let prose_indexes = prose_line_indexes(body)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let first_nonblank = lines
        .iter()
        .position(|line| !trim_ascii_whitespace(line).is_empty());
    let mut normalized = Vec::with_capacity(lines.len());
    for (index, raw) in lines.into_iter().enumerate() {
        if Some(index) == first_nonblank
            && prose_indexes.contains(&index)
            && disclosure.is_match(raw)
        {
            continue;
        }
        if prose_indexes.contains(&index) && who_metadata.is_match(raw) {
            continue;
        }
        if prose_indexes.contains(&index) && marker_matches(review_marker, raw) {
            let marker_line = undecorate(raw);
            if let Some(claimed_identity) = by_identity.find(&marker_line) {
                let claimed_text = claimed_identity.as_str();
                if let Some(claimed_start) = raw.rfind(claimed_text) {
                    normalized.push(format!(
                        "{}{}",
                        &raw[..claimed_start],
                        &raw[claimed_start + claimed_text.len()..]
                    ));
                    continue;
                }
            }
        }
        normalized.push(raw.to_owned());
    }
    normalized.join("\n")
}

pub(crate) fn has_comment_changes_requested(snapshot: &ReviewEvidenceSnapshot) -> bool {
    static COMMENT_OBJECTION: OnceLock<Regex> = OnceLock::new();
    let comment_objection = regex(
        &COMMENT_OBJECTION,
        r"^(?i-u:CHANGES-REQUESTED-AT):[ \t]*(?i-u:claude|codex)[ \t]+(?i-u:[0-9a-f]{40})",
    );
    snapshot.events.iter().any(|event| {
        matches!(event.kind.as_str(), "issue-comment" | "review-comment")
            && prose_lines(&event.body)
                .iter()
                .any(|line| marker_matches(comment_objection, line))
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetirementRecord {
    pub(crate) target_comment_id: String,
    pub(crate) lane: String,
    pub(crate) head_sha: String,
}

/// Exact objection-retirement linkage, without optional attribution text.
pub(crate) fn retirement_record(body: &str) -> Result<Option<RetirementRecord>, String> {
    static TARGET: OnceLock<Regex> = OnceLock::new();
    static WITHDRAWAL: OnceLock<Regex> = OnceLock::new();
    let target = regex(
        &TARGET,
        r"^[ \t]*(?i-u:RETIRES)[ \t]+#?([0-9]{6,})[ \t]*\r?$",
    );
    let withdrawal = regex(
        &WITHDRAWAL,
        r"^(?i-u:CHANGES-REQUESTED-WITHDRAWN-AT):[ \t]*(?P<lane>(?i-u:claude|codex))[ \t]+(?P<head>(?i-u:[0-9a-f]{40}))",
    );
    let lines = prose_lines(body);
    let targets = lines
        .iter()
        .filter_map(|line| target.captures(line))
        .map(|captures| captures[1].to_owned())
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Ok(None);
    }
    if targets.len() != 1 {
        return Err("review evidence retirement must name exactly one target".to_owned());
    }
    let mut withdrawals = Vec::new();
    for line in &lines {
        let normalized = undecorate(line);
        if marker_matches(withdrawal, line) {
            let captures = withdrawal
                .captures(&normalized)
                .expect("matched withdrawal");
            withdrawals.push((
                captures["lane"].to_ascii_lowercase(),
                captures["head"].to_ascii_lowercase(),
            ));
        }
    }
    if withdrawals.len() != 1 {
        return Err("review evidence retirement needs one canonical withdrawal".to_owned());
    }
    let (lane, head_sha) = withdrawals.pop().expect("exactly one withdrawal");
    Ok(Some(RetirementRecord {
        target_comment_id: targets[0].clone(),
        lane,
        head_sha,
    }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Caller-owned facts for one PR, optionally guarded by fetched commit identities.
pub struct LandingContext {
    /// Positive PR number.
    pub pr: i64,
    /// Exact fetched head SHA to which the context applies.
    pub head_sha: String,
    /// Exact fetched base SHA to which validation evidence applies.
    pub base_sha: String,
    /// Optional operator or agent assignment.
    pub assigned_agent: String,
    /// Optional validation-evidence override.
    pub validation_evidence: Option<ValidationEvidence>,
    /// Optional consuming-workspace hard/soft-green authorization.
    pub validation_authority: Option<ValidationAuthority>,
    /// Caller-verified review lane to exact reviewed-head SHA receipts.
    pub review_pass_heads: BTreeMap<String, String>,
    /// Whether the consuming repository's authority found every objection resolved.
    pub review_objections_resolved: bool,
    /// Digest of the exact review/comment snapshot used by that authority decision.
    pub review_evidence_digest: String,
    /// Optional policy classification override.
    pub policy_class: Option<PolicyClass>,
}

fn is_exact_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_exact_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Digest one complete exact-head review/comment event set canonically.
pub fn review_evidence_digest(snapshot: &ReviewEvidenceSnapshot) -> Result<String, String> {
    if !is_exact_sha(&snapshot.head_sha) {
        return Err("review evidence snapshot head must be an exact lowercase SHA".to_owned());
    }
    if !matches!(
        snapshot.review_decision.as_str(),
        "" | "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED"
    ) {
        return Err("review evidence snapshot has an unknown aggregate decision".to_owned());
    }
    let mut events = Vec::with_capacity(snapshot.events.len());
    let mut seen = BTreeSet::new();
    for source in &snapshot.events {
        let mut event = source.clone();
        if event.kind.is_empty() || event.identity.is_empty() {
            return Err("review evidence event lacks a stable kind or identity".to_owned());
        }
        if !matches!(
            event.kind.as_str(),
            "review" | "issue-comment" | "review-comment"
        ) {
            return Err(format!(
                "review evidence event has unknown kind {:?}",
                event.kind
            ));
        }
        let retirement = retirement_record(&event.body)?;
        if let Some(retirement) = retirement {
            if !event.retirement_actor_permission.is_empty()
                && (retirement.head_sha != snapshot.head_sha || event.state != "ACTIVE")
            {
                return Err(
                    "repository permission is bound to an inactive or stale retirement".to_owned(),
                );
            }
            if !event.retirement_actor_permission.is_empty() && event.author.is_empty() {
                return Err(
                    "review evidence retirement permission lacks a GitHub event author".to_owned(),
                );
            }
            if !event.retirement_actor_permission.is_empty()
                && !ALLOWED_RETIREMENT_PERMISSIONS
                    .contains(&event.retirement_actor_permission.as_str())
            {
                return Err(
                    "review evidence retirement lacks current triage-or-higher permission"
                        .to_owned(),
                );
            }
            if event.retirement_actor_permission.is_empty() {
                event.author.clear();
            }
        } else if !event.retirement_actor_permission.is_empty() {
            return Err("non-retirement review evidence carries repository permission".to_owned());
        } else {
            event.author.clear();
        }
        event.body = normalized_review_body(&event.body);
        if event.state.is_empty() {
            return Err("review evidence event lacks a state".to_owned());
        }
        if event.kind == "review" && !is_exact_sha(&event.head_sha) {
            return Err("native review evidence requires an exact lowercase head SHA".to_owned());
        }
        if !event.head_sha.is_empty() && !is_exact_sha(&event.head_sha) {
            return Err(
                "review evidence event head must be empty or an exact lowercase SHA".to_owned(),
            );
        }
        if event.created_at.is_empty() || event.updated_at.is_empty() {
            return Err("review evidence event lacks a creation or version timestamp".to_owned());
        }
        if !seen.insert((event.kind.clone(), event.identity.clone())) {
            return Err(format!(
                "review evidence contains duplicate stable identity {}:{}",
                event.kind, event.identity
            ));
        }
        events.push(event);
    }
    events.sort();
    let has_changes_requested = events
        .iter()
        .any(|event| event.kind == "review" && event.state == "CHANGES_REQUESTED");
    if snapshot.review_decision.is_empty() && has_changes_requested {
        return Err(
            "review evidence has a changes-requested review but no aggregate decision".to_owned(),
        );
    }
    if snapshot.review_decision == "CHANGES_REQUESTED" && !has_changes_requested {
        return Err(
            "review evidence has a changes-requested aggregate but no matching review".to_owned(),
        );
    }

    fn feed(digest: &mut Sha256, value: &str) {
        let bytes = value.as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }

    let mut digest = Sha256::new();
    digest.update(b"pr-landing-planner-review-evidence-v3");
    feed(&mut digest, &snapshot.head_sha);
    feed(&mut digest, &snapshot.review_decision);
    digest.update((events.len() as u64).to_be_bytes());
    for event in &events {
        for value in [
            &event.kind,
            &event.identity,
            &event.author,
            &event.state,
            &event.head_sha,
            &event.created_at,
            &event.updated_at,
            &event.last_edited_at,
            &event.body,
            &event.retirement_actor_permission,
        ] {
            feed(&mut digest, value);
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Parse and validate a `{"prs": [...]}` landing-context document.
pub fn parse_landing_context(raw: &Value) -> Result<Vec<LandingContext>, String> {
    let obj = raw
        .as_object()
        .ok_or("landing context must be an object with a 'prs' array")?;
    let prs = obj
        .get("prs")
        .and_then(Value::as_array)
        .ok_or("landing context field 'prs' must be an array")?;
    let mut seen = BTreeSet::new();
    let mut contexts = Vec::new();
    for item in prs {
        let item = item
            .as_object()
            .ok_or("each landing context PR entry must be an object")?;
        let pr = item.get("pr").and_then(Value::as_i64).unwrap_or(0);
        if pr <= 0 {
            return Err("landing context PR entry needs a positive integer 'pr'".to_owned());
        }
        if !seen.insert(pr) {
            return Err(format!("landing context contains duplicate PR #{pr}"));
        }
        let string = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let evidence_raw = string("validation_evidence");
        let evidence = if evidence_raw.is_empty() {
            None
        } else {
            Some(ValidationEvidence::parse(&evidence_raw).ok_or_else(|| {
                format!("PR #{pr} has unknown validation_evidence {evidence_raw:?}")
            })?)
        };
        let authority_raw = string("validation_authority");
        let authority = if authority_raw.is_empty() {
            None
        } else {
            Some(ValidationAuthority::parse(&authority_raw).ok_or_else(|| {
                format!("PR #{pr} has unknown validation_authority {authority_raw:?}")
            })?)
        };
        let policy_raw = string("policy_class");
        let policy = if policy_raw.is_empty() {
            None
        } else {
            Some(
                PolicyClass::parse(&policy_raw)
                    .ok_or_else(|| format!("PR #{pr} has unknown policy_class {policy_raw:?}"))?,
            )
        };
        let head_sha = string("head_sha");
        let base_sha = string("base_sha");
        let review_objections_resolved = match item.get("review_objections_resolved") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(format!(
                    "PR #{pr} review_objections_resolved must be a boolean"
                ))
            }
        };
        let review_digest = string("review_evidence_digest");
        if review_objections_resolved && !is_exact_sha(&head_sha) {
            return Err(format!(
                "PR #{pr} review_objections_resolved requires exact 'head_sha'"
            ));
        }
        if review_objections_resolved && !is_exact_sha256(&review_digest) {
            return Err(format!(
                "PR #{pr} review_objections_resolved requires an exact lowercase 'review_evidence_digest'; run an uncontexted exact-head plan, have the review authority assess that snapshot, and copy nodes[].review_evidence_digest into the generated context"
            ));
        }
        if !review_objections_resolved && !review_digest.is_empty() {
            return Err(format!(
                "PR #{pr} review_evidence_digest requires review_objections_resolved=true"
            ));
        }
        if matches!(
            evidence,
            Some(ValidationEvidence::LocallyValidated | ValidationEvidence::CleanValidateRecord)
        ) && (head_sha.is_empty() || base_sha.is_empty())
        {
            return Err(format!(
                "PR #{pr} {} evidence requires exact 'head_sha' and 'base_sha'; revalidate and record both fetched identities",
                evidence.expect("matched local evidence").as_str()
            ));
        }
        if !matches!(authority, None | Some(ValidationAuthority::None))
            && evidence != Some(ValidationEvidence::CleanValidateRecord)
        {
            return Err(format!(
                "PR #{pr} {} validation_authority requires validation_evidence 'clean-validate-record'",
                authority.expect("matched validation authority").as_str()
            ));
        }
        if evidence == Some(ValidationEvidence::CleanValidateRecord)
            && !matches!(
                authority,
                Some(ValidationAuthority::HardGreen | ValidationAuthority::SoftGreen)
            )
        {
            return Err(format!(
                "PR #{pr} clean-validate-record requires explicit validation_authority 'hard-green' or 'soft-green'"
            ));
        }
        let mut review_pass_heads = BTreeMap::new();
        if let Some(raw_heads) = item.get("review_pass_heads") {
            let heads = raw_heads.as_object().ok_or_else(|| {
                format!("PR #{pr} review_pass_heads must be an object mapping lane to exact SHA")
            })?;
            for (lane, raw_sha) in heads {
                if !REQUIRED_REVIEW_LANES.contains(&lane.as_str()) {
                    return Err(format!("PR #{pr} has unknown review lane {lane:?}"));
                }
                let sha = raw_sha.as_str().unwrap_or("");
                if !is_exact_sha(sha) {
                    return Err(format!(
                        "PR #{pr} review_pass_heads[{lane:?}] must be an exact 40-character lowercase hex SHA"
                    ));
                }
                review_pass_heads.insert(lane.clone(), sha.to_owned());
            }
        }
        contexts.push(LandingContext {
            pr,
            head_sha,
            base_sha,
            assigned_agent: string("assigned_agent"),
            validation_evidence: evidence,
            validation_authority: authority,
            review_pass_heads,
            review_objections_resolved,
            review_evidence_digest: review_digest,
            policy_class: policy,
        });
    }
    Ok(contexts)
}

fn one_label_value(
    labels: &[String],
    prefix: &str,
    field: &str,
    pr: i64,
) -> Result<String, String> {
    let values: BTreeSet<_> = labels
        .iter()
        .filter_map(|label| label.strip_prefix(prefix))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    if values.len() > 1 {
        return Err(format!(
            "PR #{pr} has multiple {field} labels: {}",
            values.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(values.into_iter().next().unwrap_or_default())
}

fn optional_agent_label(labels: &[String]) -> String {
    let values: BTreeSet<_> = labels
        .iter()
        .filter_map(|label| label.strip_prefix(AGENT_PREFIX))
        .filter(|value| !value.is_empty())
        .collect();
    if values.len() == 1 {
        values.into_iter().next().unwrap_or_default().to_owned()
    } else {
        String::new()
    }
}

fn apply_labels(mut node: PrNode) -> Result<PrNode, String> {
    node.assigned_agent = optional_agent_label(&node.labels);
    let raw = one_label_value(&node.labels, POLICY_PREFIX, "landing-policy", node.number)?;
    node.policy_class = if raw.is_empty() {
        PolicyClass::Unclassified
    } else {
        PolicyClass::parse(&raw).ok_or_else(|| {
            format!(
                "PR #{} has unknown landing-policy label {raw:?}",
                node.number
            )
        })?
    };
    node.validation_evidence = if node.ci.raw_state == CiState::Passed && node.ci.gate_ok {
        ValidationEvidence::AuthoritativeCi
    } else {
        // A locally-validated label is only a cache hint. It deliberately maps to
        // None; only caller context bound to the fetched head and base may produce
        // LocallyValidated.
        ValidationEvidence::None
    };
    Ok(node)
}

/// Apply label-derived facts and caller authority, keeping head binding exact.
pub fn apply_landing_context(
    nodes: Vec<PrNode>,
    contexts: &[LandingContext],
) -> Result<Vec<PrNode>, String> {
    let by_context: BTreeMap<_, _> = contexts.iter().map(|ctx| (ctx.pr, ctx)).collect();
    let node_numbers: BTreeSet<_> = nodes.iter().map(|node| node.number).collect();
    let unknown: Vec<_> = by_context
        .keys()
        .filter(|number| !node_numbers.contains(number))
        .copied()
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "landing context names PRs absent from this plan: {}",
            unknown
                .iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    nodes
        .into_iter()
        .map(|raw| {
            let mut node = apply_labels(raw)?;
            let Some(context) = by_context.get(&node.number) else {
                return Ok(node);
            };
            if !context.head_sha.is_empty() && context.head_sha != node.head_sha {
                return Err(format!(
                    "PR #{} landing context is stale: context={}, current={}",
                    node.number, context.head_sha, node.head_sha
                ));
            }
            if context.review_objections_resolved
                && node.review_evidence_digest != context.review_evidence_digest
            {
                return Err(format!(
                    "PR #{} review objection resolution is stale: context digest {:?}, host digest {:?}; rerun an uncontexted exact-head plan, have the authority reassess that snapshot, and copy nodes[].review_evidence_digest into fresh context",
                    node.number, context.review_evidence_digest, node.review_evidence_digest
                ));
            }
            if !context.base_sha.is_empty()
                && context.base_sha != node.base_sha
                && context.validation_authority != Some(ValidationAuthority::SoftGreen)
            {
                return Err(format!(
                    "PR #{} landing context base differs: context={}, current={}; the consuming workspace supplied no soft-green authority",
                    node.number, context.base_sha, node.base_sha
                ));
            }
            if !context.assigned_agent.is_empty() {
                node.assigned_agent.clone_from(&context.assigned_agent);
            }
            if let Some(evidence) = context.validation_evidence {
                node.validation_evidence = evidence;
            }
            if let Some(authority) = context.validation_authority {
                node.validation_authority = authority;
            }
            node.review_pass_heads
                .clone_from(&context.review_pass_heads);
            node.review_objections_resolved = context.review_objections_resolved;
            if let Some(policy) = context.policy_class {
                node.policy_class = policy;
            }
            Ok(node)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{held_reasons, review_binding};
    use crate::model::{PrAction, ReviewBinding, ReviewEvidenceEvent, ReviewEvidenceSnapshot};
    use crate::plan::compute_plan;
    use serde_json::json;

    const REVIEWED_HEAD: &str = "92e1e0d0af65e50cd2991d4deaa25f726832fbf4";
    const REBASED_HEAD: &str = "0fc9f61edc01d6425def2efb0ed82f01410c7fcc";
    const CHANGED_HEAD: &str = "1111111111111111111111111111111111111111";

    fn node(number: i64, head: &str, labels: &[&str]) -> PrNode {
        PrNode {
            number,
            head_sha: head.into(),
            base_sha: "base".into(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            ..PrNode::default()
        }
    }

    #[test]
    fn clean_records_keep_exact_head_and_delegate_older_base_to_authority() {
        let missing = json!({"prs":[{"pr":1,"validation_evidence":"clean-validate-record"}]});
        assert!(parse_landing_context(&missing)
            .unwrap_err()
            .contains("requires exact 'head_sha'"));
        let context = parse_landing_context(&json!({"prs":[{
            "pr":1,"head_sha":"stale","base_sha":"base",
            "validation_evidence":"clean-validate-record",
            "validation_authority":"hard-green"
        }]}))
        .unwrap();
        assert!(
            apply_landing_context(vec![node(1, "current", &[])], &context)
                .unwrap_err()
                .contains("stale")
        );
        let no_authority = json!({"prs":[{
            "pr":1,
            "head_sha":"current",
            "base_sha":"base",
            "validation_evidence":"clean-validate-record"
        }]});
        assert!(parse_landing_context(&no_authority)
            .unwrap_err()
            .contains("requires explicit validation_authority"));
        let hard_green = parse_landing_context(&json!({"prs":[{
            "pr":1,
            "head_sha":"current",
            "base_sha":"base",
            "validation_evidence":"clean-validate-record",
            "validation_authority":"hard-green"
        }]}))
        .unwrap();
        let hard_green_node = apply_landing_context(vec![node(1, "current", &[])], &hard_green)
            .unwrap()
            .remove(0);
        assert_eq!(
            hard_green_node.validation_authority,
            ValidationAuthority::HardGreen
        );
        let (hard_green_plan, _) = compute_plan(&[hard_green_node], &[], &[], &[], None, 2, false);
        assert_eq!(hard_green_plan.per_pr_actions[0].action, PrAction::LandNow);
        let earlier_green = parse_landing_context(&json!({"prs":[{
            "pr":1,
            "head_sha":"current",
            "base_sha":"earlier-green-base",
            "validation_evidence":"clean-validate-record",
            "validation_authority":"soft-green"
        }]}))
        .unwrap();
        let mut current = node(1, "current", &[]);
        current.commits_behind = 5;
        let authorized = apply_landing_context(vec![current], &earlier_green)
            .unwrap()
            .remove(0);
        assert_eq!(
            authorized.validation_authority,
            ValidationAuthority::SoftGreen
        );
        // Explicit bound: this case is about the soft-green authority path, and
        // being behind is no longer a reroute reason by default.
        let one = std::slice::from_ref(&authorized);
        let (plan, _) = compute_plan(one, &[], &[], &[], Some(0), 2, false);
        assert_eq!(plan.per_pr_actions[0].action, PrAction::RebaseThenLand);
        let (default_plan, _) = compute_plan(
            one,
            &[],
            &[],
            &[],
            crate::plan::DEFAULT_FRESHNESS_MAX_BEHIND,
            2,
            false,
        );
        assert_eq!(default_plan.per_pr_actions[0].action, PrAction::LandNow);
        assert!(plan.per_pr_actions[0]
            .why
            .contains("without pre-landing revalidation"));
        let hard_green_on_other_base = parse_landing_context(&json!({"prs":[{
            "pr":1,
            "head_sha":"current",
            "base_sha":"divergent-base",
            "validation_evidence":"clean-validate-record",
            "validation_authority":"hard-green"
        }]}))
        .unwrap();
        assert!(
            apply_landing_context(vec![node(1, "current", &[])], &hard_green_on_other_base)
                .unwrap_err()
                .contains("supplied no soft-green authority")
        );
        let unknown = parse_landing_context(&json!({"prs":[{"pr":9}]})).unwrap();
        assert!(
            apply_landing_context(vec![node(1, "current", &[])], &unknown)
                .unwrap_err()
                .contains("absent")
        );
    }

    #[test]
    fn validation_authority_requires_a_clean_record() {
        let missing = json!({"prs":[{
            "pr":1,
            "head_sha":"head",
            "base_sha":"base",
            "validation_authority":"soft-green"
        }]});
        assert!(parse_landing_context(&missing)
            .unwrap_err()
            .contains("requires validation_evidence"));
    }

    #[test]
    fn bare_label_is_cache_only_but_dereferenced_context_is_evidence() {
        let raw = node(
            1,
            "head",
            &[
                "locally-validated",
                "agent:one",
                "landing-policy:ci-hygiene",
            ],
        );
        let enriched = apply_landing_context(vec![raw], &[]).unwrap().remove(0);
        assert_eq!(enriched.assigned_agent, "one");
        assert_eq!(enriched.policy_class, PolicyClass::CiHygiene);
        // Negative bracket: one bare cache label produces zero validation evidence.
        assert_eq!(enriched.validation_evidence, ValidationEvidence::None);

        let context = parse_landing_context(&json!({"prs":[{
            "pr":1,
            "head_sha":"head",
            "base_sha":"base",
            "validation_evidence":"locally-validated"
        }]}))
        .unwrap();
        let dereferenced = apply_landing_context(vec![node(1, "head", &[])], &context)
            .unwrap()
            .remove(0);
        // Positive bracket: one caller-dereferenced exact-identity record is accepted.
        assert_eq!(
            dereferenced.validation_evidence,
            ValidationEvidence::LocallyValidated
        );

        let missing_identity = json!({"prs":[{"pr":1,"validation_evidence":"locally-validated"}]});
        assert!(parse_landing_context(&missing_identity)
            .unwrap_err()
            .contains("locally-validated evidence requires"));

        let duplicate = node(2, "head", &["agent:one", "agent:two"]);
        let multiply_labeled = apply_landing_context(vec![duplicate], &[])
            .unwrap()
            .remove(0);
        assert!(multiply_labeled.assigned_agent.is_empty());
        assert_eq!(multiply_labeled.number, 2);
    }

    #[test]
    fn review_passes_bind_exact_head_and_head_moves_fail_closed() {
        let labels = [
            "post-facto-human-review",
            "passed-review-codex",
            "passed-review-claude",
        ];
        let exact = parse_landing_context(&json!({"prs":[{
            "pr":394,
            "review_pass_heads":{
                "codex":REBASED_HEAD,
                "claude":REBASED_HEAD
            }
        }]}))
        .unwrap();
        let exact_node = apply_landing_context(vec![node(394, REBASED_HEAD, &labels)], &exact)
            .unwrap()
            .remove(0);
        assert_eq!(
            review_binding(&exact_node),
            (ReviewBinding::ExactHead, vec![])
        );
        assert!(held_reasons(&[exact_node], &[]).is_empty());

        let stale = parse_landing_context(&json!({"prs":[{
            "pr":394,
            "review_pass_heads":{
                "codex":REBASED_HEAD,
                "claude":REVIEWED_HEAD
            }
        }]}))
        .unwrap();
        let stale_node = apply_landing_context(vec![node(394, REBASED_HEAD, &labels)], &stale)
            .unwrap()
            .remove(0);
        let (binding, reasons) = review_binding(&stale_node);
        assert_eq!(binding, ReviewBinding::Stale);
        assert_eq!(
            reasons,
            vec![format!(
                "review-pass-stale:claude:reviewed={REVIEWED_HEAD}:current={REBASED_HEAD}"
            )]
        );
        assert_eq!(held_reasons(&[stale_node], &[])[0].reasons, reasons);

        let changed_node = apply_landing_context(vec![node(394, CHANGED_HEAD, &labels)], &stale)
            .unwrap()
            .remove(0);
        assert_eq!(review_binding(&changed_node).0, ReviewBinding::Stale);
    }

    #[test]
    fn review_objection_resolution_is_boolean_and_exact_head_bound() {
        let invalid = json!({"prs":[{"pr":394,"review_objections_resolved":"yes"}]});
        assert!(parse_landing_context(&invalid)
            .unwrap_err()
            .contains("must be a boolean"));
        let missing_head = json!({"prs":[{"pr":394,"review_objections_resolved":true}]});
        assert!(parse_landing_context(&missing_head)
            .unwrap_err()
            .contains("requires exact 'head_sha'"));
        let missing_digest = json!({"prs":[{
            "pr":394,
            "head_sha":REBASED_HEAD,
            "review_objections_resolved":true
        }]});
        assert!(parse_landing_context(&missing_digest)
            .unwrap_err()
            .contains("review_evidence_digest"));

        let observed_at = "2026-09-04T12:00:00Z";
        let snapshot = ReviewEvidenceSnapshot {
            head_sha: REBASED_HEAD.into(),
            review_decision: "CHANGES_REQUESTED".into(),
            events: vec![
                ReviewEvidenceEvent {
                    kind: "review".into(),
                    identity: "review-1".into(),
                    author: "reviewer".into(),
                    state: "CHANGES_REQUESTED".into(),
                    head_sha: REBASED_HEAD.into(),
                    created_at: observed_at.into(),
                    updated_at: observed_at.into(),
                    last_edited_at: String::new(),
                    body: "please address the race".into(),
                    retirement_actor_permission: String::new(),
                },
                ReviewEvidenceEvent {
                    kind: "issue-comment".into(),
                    identity: "comment-1".into(),
                    author: "release-authority".into(),
                    state: "ACTIVE".into(),
                    head_sha: String::new(),
                    created_at: observed_at.into(),
                    updated_at: observed_at.into(),
                    last_edited_at: String::new(),
                    body: "resolved by the latest patch".into(),
                    retirement_actor_permission: String::new(),
                },
                ReviewEvidenceEvent {
                    kind: "review-comment".into(),
                    identity: "thread-1".into(),
                    author: "reviewer".into(),
                    state: "RESOLVED".into(),
                    head_sha: String::new(),
                    created_at: observed_at.into(),
                    updated_at: observed_at.into(),
                    last_edited_at: String::new(),
                    body: "inline objection retired".into(),
                    retirement_actor_permission: String::new(),
                },
            ],
        };
        let digest = review_evidence_digest(&snapshot).unwrap();

        let context = parse_landing_context(&json!({"prs":[{
            "pr":394,
            "head_sha":REBASED_HEAD,
            "review_objections_resolved":true,
            "review_evidence_digest":digest.clone()
        }]}))
        .unwrap();
        let mut raw = node(394, REBASED_HEAD, &[]);
        raw.updated_at = observed_at.into();
        raw.review_decision = "CHANGES_REQUESTED".into();
        raw.review_evidence_digest = digest.clone();
        let resolved = apply_landing_context(vec![raw], &context)
            .unwrap()
            .remove(0);
        assert!(resolved.review_objections_resolved);
        assert!(held_reasons(std::slice::from_ref(&resolved), &[]).is_empty());

        let mut changed = resolved.clone();
        changed.head_sha = CHANGED_HEAD.into();
        assert!(apply_landing_context(vec![changed], &context)
            .unwrap_err()
            .contains("landing context is stale"));

        let mut same_second_objection = snapshot.clone();
        same_second_objection.events.push(ReviewEvidenceEvent {
            kind: "review-comment".into(),
            identity: "thread-2".into(),
            author: "reviewer".into(),
            state: "ACTIVE".into(),
            head_sha: String::new(),
            created_at: observed_at.into(),
            updated_at: observed_at.into(),
            last_edited_at: String::new(),
            body: "new same-second objection".into(),
            retirement_actor_permission: String::new(),
        });
        let mut changed_review = resolved;
        changed_review.review_evidence_digest =
            review_evidence_digest(&same_second_objection).unwrap();
        let stale_error = apply_landing_context(vec![changed_review], &context).unwrap_err();
        assert!(stale_error.contains("review objection resolution is stale"));
        assert!(stale_error.contains("uncontexted exact-head plan"));
        assert!(stale_error.contains("nodes[].review_evidence_digest"));

        let digest_without_resolution = json!({"prs":[{
            "pr":394,"review_evidence_digest":digest
        }]});
        assert!(parse_landing_context(&digest_without_resolution)
            .unwrap_err()
            .contains("requires review_objections_resolved=true"));

        let mut missing_identity = snapshot.clone();
        missing_identity.events[0].identity.clear();
        assert!(review_evidence_digest(&missing_identity)
            .unwrap_err()
            .contains("stable kind or identity"));
        let mut missing_author = snapshot.clone();
        missing_author.events[0].author.clear();
        assert_eq!(review_evidence_digest(&missing_author).unwrap(), digest);
        let mut missing_decision = snapshot.clone();
        missing_decision.review_decision.clear();
        assert!(review_evidence_digest(&missing_decision)
            .unwrap_err()
            .contains("no aggregate decision"));
        let mut unknown_decision = snapshot.clone();
        unknown_decision.review_decision = "UNKNOWN".into();
        assert!(review_evidence_digest(&unknown_decision)
            .unwrap_err()
            .contains("unknown aggregate decision"));
        let mut missing_changes_requested = snapshot.clone();
        missing_changes_requested.events.remove(0);
        assert!(review_evidence_digest(&missing_changes_requested)
            .unwrap_err()
            .contains("no matching review"));
        let mut author_changed = snapshot.clone();
        author_changed.events[0].author = "different-reviewer".into();
        assert_eq!(review_evidence_digest(&author_changed).unwrap(), digest);
        let mut duplicate = snapshot;
        duplicate.events.push(duplicate.events[0].clone());
        assert!(review_evidence_digest(&duplicate)
            .unwrap_err()
            .contains("duplicate stable identity"));
    }

    #[test]
    fn review_digest_covers_every_normalized_authority_field() {
        let event = ReviewEvidenceEvent {
            kind: "review".into(),
            identity: "review-1".into(),
            author: "reviewer".into(),
            state: "APPROVED".into(),
            head_sha: REBASED_HEAD.into(),
            created_at: "2026-09-04T11:59:00Z".into(),
            updated_at: "2026-09-04T12:00:00Z".into(),
            last_edited_at: String::new(),
            body: "looks good".into(),
            retirement_actor_permission: String::new(),
        };
        // Deliberately exhaustive: adding another normalized authority input must update
        // this audit instead of silently escaping the digest contract.
        let ReviewEvidenceEvent {
            kind: _,
            identity: _,
            author: _,
            state: _,
            head_sha: _,
            created_at: _,
            updated_at: _,
            last_edited_at: _,
            body: _,
            retirement_actor_permission: _,
        } = event.clone();
        let snapshot = ReviewEvidenceSnapshot {
            head_sha: REBASED_HEAD.into(),
            review_decision: "APPROVED".into(),
            events: vec![event.clone()],
        };
        let ReviewEvidenceSnapshot {
            head_sha: _,
            review_decision: _,
            events: _,
        } = snapshot.clone();
        let digest = review_evidence_digest(&snapshot).unwrap();
        let mutations = [
            ReviewEvidenceEvent {
                kind: "issue-comment".into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                identity: "review-2".into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                state: "DISMISSED".into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                head_sha: CHANGED_HEAD.into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                created_at: "2026-09-04T11:58:00Z".into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                updated_at: "2026-09-04T12:00:01Z".into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                last_edited_at: "2026-09-04T12:00:01Z".into(),
                ..event.clone()
            },
            ReviewEvidenceEvent {
                body: "new objection".into(),
                ..event
            },
        ];
        for mutation in mutations {
            let mut changed = snapshot.clone();
            changed.events = vec![mutation];
            let mut changed_author = snapshot.clone();
            changed_author.events[0].author = "different-reviewer".into();
            assert_eq!(review_evidence_digest(&changed_author).unwrap(), digest);

            assert_ne!(review_evidence_digest(&changed).unwrap(), digest);
        }
        let mut changed_head = snapshot.clone();
        changed_head.head_sha = CHANGED_HEAD.into();
        assert_ne!(review_evidence_digest(&changed_head).unwrap(), digest);
        let mut changed_decision = snapshot;
        changed_decision.review_decision = "REVIEW_REQUIRED".into();
        assert_ne!(review_evidence_digest(&changed_decision).unwrap(), digest);
    }

    #[test]
    fn retirement_uses_event_author_permission_and_ignores_claimed_identity() {
        let body = format!(
            "[team, release-authority, session, model, role=observer]\n\
             CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD} BY release-authority\n\
             RETIRES 123456"
        );
        let event = ReviewEvidenceEvent {
            kind: "issue-comment".into(),
            identity: "comment-1".into(),
            author: "release-authority".into(),
            state: "ACTIVE".into(),
            head_sha: String::new(),
            created_at: "2026-09-04T12:00:00Z".into(),
            updated_at: "2026-09-04T12:00:01Z".into(),
            last_edited_at: String::new(),
            body: body.clone(),
            retirement_actor_permission: "write".into(),
        };
        let snapshot = ReviewEvidenceSnapshot {
            head_sha: REBASED_HEAD.into(),
            review_decision: "APPROVED".into(),
            events: vec![event.clone()],
        };
        let record = retirement_record(&body).unwrap().unwrap();
        assert_eq!(record.target_comment_id, "123456");
        assert_eq!(record.lane, "codex");
        assert_eq!(record.head_sha, REBASED_HEAD);
        let write_digest = review_evidence_digest(&snapshot).unwrap();
        assert_eq!(
            write_digest,
            "83f83abe9e2e319f0a65a4f100d1b6c91f8a6c0ca29e6797d120166e6cca3b58"
        );
        let mut appended_objection = snapshot.clone();
        appended_objection.events[0].body = format!("{body}\nDo not land: the race remains.");
        assert_ne!(
            review_evidence_digest(&appended_objection).unwrap(),
            write_digest
        );
        let mut maintain = snapshot.clone();
        maintain.events[0].retirement_actor_permission = "maintain".into();
        assert_ne!(review_evidence_digest(&maintain).unwrap(), write_digest);

        let mut unverified = snapshot.clone();
        unverified.events[0].author.clear();
        unverified.events[0].retirement_actor_permission.clear();
        let unverified_digest = review_evidence_digest(&unverified).unwrap();
        assert_ne!(unverified_digest, write_digest);
        unverified.events[0].author = "departed".into();
        assert_eq!(
            review_evidence_digest(&unverified).unwrap(),
            unverified_digest
        );

        let mut invalid = snapshot.clone();
        invalid.events[0].retirement_actor_permission = "read".into();
        assert!(review_evidence_digest(&invalid)
            .unwrap_err()
            .contains("triage-or-higher"));
        let mut missing_author = snapshot.clone();
        missing_author.events[0].author.clear();
        assert!(review_evidence_digest(&missing_author)
            .unwrap_err()
            .contains("lacks a GitHub event author"));
        let mut hostname_author = snapshot.clone();
        hostname_author.events[0].author = "devbig014".into();
        assert_ne!(
            review_evidence_digest(&hostname_author).unwrap(),
            write_digest
        );
        for claimed_identity in ["other-agent", "other_agent", "other.agent", "K"] {
            let mut false_by = snapshot.clone();
            false_by.events[0].body =
                body.replace("BY release-authority", &format!("BY {claimed_identity}"));
            assert_eq!(
                retirement_record(&false_by.events[0].body).unwrap(),
                Some(record.clone())
            );
            assert_eq!(review_evidence_digest(&false_by).unwrap(), write_digest);
        }
        let mut malformed_by = snapshot.clone();
        malformed_by.events[0].body = body.replace("BY release-authority", "BY other/agent");
        assert_eq!(
            retirement_record(&malformed_by.events[0].body).unwrap(),
            Some(record.clone())
        );
        let malformed_digest = review_evidence_digest(&malformed_by).unwrap();
        assert_ne!(malformed_digest, write_digest);
        let mut mutated_malformed = malformed_by.clone();
        mutated_malformed.events[0].body = malformed_by.events[0]
            .body
            .replace("other/agent", "other agent");
        assert_ne!(
            review_evidence_digest(&mutated_malformed).unwrap(),
            malformed_digest
        );
        for malformed_suffix in ["BY: other.agent", "BY/other.agent"] {
            let mut alternate_malformed = snapshot.clone();
            alternate_malformed.events[0].body =
                body.replace("BY release-authority", malformed_suffix);
            assert_eq!(
                retirement_record(&alternate_malformed.events[0].body).unwrap(),
                Some(record.clone())
            );
            assert_ne!(
                review_evidence_digest(&alternate_malformed).unwrap(),
                write_digest
            );
        }
        for non_ascii_identity in ["K", "İ", "ı", "ſ"] {
            let mut non_ascii = snapshot.clone();
            non_ascii.events[0].body =
                body.replace("BY release-authority", &format!("BY {non_ascii_identity}"));
            assert_eq!(
                retirement_record(&non_ascii.events[0].body).unwrap(),
                Some(record.clone())
            );
            assert_ne!(review_evidence_digest(&non_ascii).unwrap(), write_digest);
        }
        let mut different_disclosure = snapshot.clone();
        different_disclosure.events[0].body = body.replace(
            "[team, release-authority, session, model, role=observer]",
            "[team, departed, old-session, model, role=observer]",
        );
        assert_eq!(
            review_evidence_digest(&different_disclosure).unwrap(),
            write_digest
        );
        let mut inactive = snapshot.clone();
        inactive.events[0].state = "MINIMIZED:OUTDATED".into();
        assert!(review_evidence_digest(&inactive)
            .unwrap_err()
            .contains("inactive or stale retirement"));
        let mut stale = snapshot.clone();
        stale.events[0].body = body.replace(REBASED_HEAD, CHANGED_HEAD);
        assert!(review_evidence_digest(&stale)
            .unwrap_err()
            .contains("inactive or stale retirement"));
        let mut not_retirement = snapshot;
        not_retirement.events[0].body = "ordinary comment".into();
        assert!(review_evidence_digest(&not_retirement)
            .unwrap_err()
            .contains("non-retirement"));
    }

    #[test]
    fn review_marker_attribution_is_metadata_for_every_marker() {
        let body = format!(
            "[team, current-reviewer, session, model, role=reviewer]\n\
             CHANGES-REQUESTED-AT: codex {REBASED_HEAD} BY current-reviewer\n\
             CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD} BY current-reviewer\n\
             APPROVED-AT: codex {REBASED_HEAD} BY current-reviewer\n\
             The objection is resolved by the changed bounds check."
        );
        let event = ReviewEvidenceEvent {
            kind: "issue-comment".into(),
            identity: "comment-markers".into(),
            author: "current-reviewer".into(),
            state: "ACTIVE".into(),
            head_sha: String::new(),
            created_at: "2026-09-05T15:03:48Z".into(),
            updated_at: "2026-09-05T15:03:48Z".into(),
            last_edited_at: String::new(),
            body: body.clone(),
            retirement_actor_permission: String::new(),
        };
        let snapshot = ReviewEvidenceSnapshot {
            head_sha: REBASED_HEAD.into(),
            review_decision: String::new(),
            events: vec![event.clone()],
        };
        let digest = review_evidence_digest(&snapshot).unwrap();
        for disclosure_agent in [
            "departed-reviewer",
            "departed_reviewer",
            "departed.reviewer",
            "K",
        ] {
            let mut metadata_changed = snapshot.clone();
            metadata_changed.events[0].body = body.replace(
                "[team, current-reviewer, session, model, role=reviewer]",
                &format!("[team, {disclosure_agent}, old-session, other-model, role=reviewer]"),
            );
            assert_eq!(review_evidence_digest(&metadata_changed).unwrap(), digest);
        }
        for non_ascii_identity in ["K", "İ", "ı", "ſ"] {
            let mut metadata_changed = snapshot.clone();
            metadata_changed.events[0].body = body.replace(
                "[team, current-reviewer, session, model, role=reviewer]",
                &format!("[team, {non_ascii_identity}, session, model, role=reviewer]"),
            );
            assert_ne!(review_evidence_digest(&metadata_changed).unwrap(), digest);
        }
        for claimed_identity in [
            "departed-reviewer",
            "departed_reviewer",
            "departed.reviewer",
        ] {
            let mut metadata_changed = snapshot.clone();
            metadata_changed.events[0].body =
                body.replace("BY current-reviewer", &format!("BY {claimed_identity}"));
            assert_eq!(review_evidence_digest(&metadata_changed).unwrap(), digest);
        }
        let mut metadata_removed = snapshot.clone();
        metadata_removed.events[0].body = body
            .split('\n')
            .skip(1)
            .map(|line| line.replace(" BY current-reviewer", ""))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(review_evidence_digest(&metadata_removed).unwrap(), digest);
        let mut malformed = snapshot.clone();
        malformed.events[0].body = body.replace("BY current-reviewer", "BY current/reviewer");
        let malformed_digest = review_evidence_digest(&malformed).unwrap();
        assert_ne!(malformed_digest, digest);
        assert!(has_comment_changes_requested(&malformed));
        let mut mutated_malformed = malformed.clone();
        mutated_malformed.events[0].body = malformed.events[0]
            .body
            .replace("current/reviewer", "current reviewer");
        assert_ne!(
            review_evidence_digest(&mutated_malformed).unwrap(),
            malformed_digest
        );
        assert!(has_comment_changes_requested(&mutated_malformed));
        let mut fenced = snapshot.clone();
        fenced.events[0].body =
            format!("{body}\n```text\nAPPROVED-AT: codex {REBASED_HEAD} BY quoted\n```");
        let mut changed_fenced = fenced.clone();
        changed_fenced.events[0].body = fenced.events[0]
            .body
            .replace("BY quoted", "BY other-quoted");
        assert_ne!(
            review_evidence_digest(&fenced).unwrap(),
            review_evidence_digest(&changed_fenced).unwrap()
        );
    }

    #[test]
    fn unverified_who_metadata_is_optional_only_as_an_exact_prose_line() {
        let base_body = "The bounds check resolves the objection.";
        for (kind, state, head_sha) in [
            ("review", "APPROVED", REBASED_HEAD),
            ("issue-comment", "ACTIVE", ""),
            ("review-comment", "ACTIVE", ""),
        ] {
            let event = ReviewEvidenceEvent {
                kind: kind.into(),
                identity: format!("{kind}-metadata"),
                author: "reviewer".into(),
                state: state.into(),
                head_sha: head_sha.into(),
                created_at: "2026-09-05T15:03:48Z".into(),
                updated_at: "2026-09-05T15:03:48Z".into(),
                last_edited_at: String::new(),
                body: base_body.into(),
                retirement_actor_permission: String::new(),
            };
            let snapshot = ReviewEvidenceSnapshot {
                head_sha: REBASED_HEAD.into(),
                review_decision: "APPROVED".into(),
                events: vec![event.clone()],
            };
            let digest = review_evidence_digest(&snapshot).unwrap();
            for agent in ["review-agent", "review_agent", "review.agent", "K"] {
                let mut with_metadata = snapshot.clone();
                with_metadata.events[0].body =
                    format!("{base_body}\nUnverified --who metadata: {agent}");
                assert_eq!(review_evidence_digest(&with_metadata).unwrap(), digest);
            }
            for non_ascii_identity in ["K", "İ", "ı", "ſ"] {
                let mut with_metadata = snapshot.clone();
                with_metadata.events[0].body =
                    format!("{base_body}\nUnverified --who metadata: {non_ascii_identity}");
                assert_ne!(review_evidence_digest(&with_metadata).unwrap(), digest);
            }

            let preserved_forms = [
                format!("{base_body}\nUnverified --who metadata: review.agent extra"),
                format!("{base_body}\n> Unverified --who metadata: review.agent"),
                format!("{base_body}\n`Unverified --who metadata: review.agent`"),
                format!("{base_body}\n```text\nUnverified --who metadata: review.agent\n```"),
                format!("{base_body}\n\n    Unverified --who metadata: review.agent"),
            ];
            for preserved in preserved_forms {
                let mut first = snapshot.clone();
                first.events[0].body = preserved.clone();
                let mut second = snapshot.clone();
                second.events[0].body = preserved.replace("review.agent", "other_agent");
                assert_ne!(review_evidence_digest(&first).unwrap(), digest);
                assert_ne!(
                    review_evidence_digest(&first).unwrap(),
                    review_evidence_digest(&second).unwrap()
                );
            }
        }
    }

    #[test]
    fn comment_refusal_survives_valid_and_malformed_by_metadata() {
        let marker = format!("CHANGES-REQUESTED-AT: codex {REBASED_HEAD}");
        for kind in ["issue-comment", "review-comment"] {
            let event = ReviewEvidenceEvent {
                kind: kind.into(),
                identity: format!("{kind}-refusal"),
                author: "reviewer".into(),
                state: "ACTIVE".into(),
                head_sha: String::new(),
                created_at: "2026-09-05T15:03:48Z".into(),
                updated_at: "2026-09-05T15:03:48Z".into(),
                last_edited_at: String::new(),
                body: marker.clone(),
                retirement_actor_permission: String::new(),
            };
            for suffix in [
                "",
                " BY review-agent",
                " BY review_agent",
                " BY review.agent",
                " BY review/agent",
                " BY review agent",
                " BY",
                " BY: review.agent",
                " BY/review.agent",
                " BY K",
                " BY İ",
                " BY ı",
                " BY ſ",
            ] {
                let snapshot = ReviewEvidenceSnapshot {
                    head_sha: REBASED_HEAD.into(),
                    review_decision: String::new(),
                    events: vec![ReviewEvidenceEvent {
                        body: format!("{marker}{suffix}"),
                        ..event.clone()
                    }],
                };
                assert!(has_comment_changes_requested(&snapshot));
            }
        }
    }

    #[test]
    fn non_ascii_whitespace_is_not_review_evidence_syntax() {
        for space in ["\u{a0}", "\u{2003}"] {
            let marker = format!("CHANGES-REQUESTED-AT: codex {REBASED_HEAD}");
            let event = ReviewEvidenceEvent {
                kind: "issue-comment".into(),
                identity: "comment-whitespace".into(),
                author: "reviewer".into(),
                state: "ACTIVE".into(),
                head_sha: String::new(),
                created_at: "2026-09-05T15:03:48Z".into(),
                updated_at: "2026-09-05T15:03:48Z".into(),
                last_edited_at: String::new(),
                body: marker.clone(),
                retirement_actor_permission: String::new(),
            };
            let canonical = ReviewEvidenceSnapshot {
                head_sha: REBASED_HEAD.into(),
                review_decision: String::new(),
                events: vec![event.clone()],
            };
            let canonical_digest = review_evidence_digest(&canonical).unwrap();
            assert!(has_comment_changes_requested(&canonical));

            for malformed_marker in [
                format!("{space}{marker}"),
                marker.replacen(": ", &format!(":{space}"), 1),
                marker.replacen("codex ", &format!("codex{space}"), 1),
                format!("#{space}{marker}"),
            ] {
                let mut malformed = canonical.clone();
                malformed.events[0].body = malformed_marker;
                assert!(!has_comment_changes_requested(&malformed));
                assert_ne!(
                    review_evidence_digest(&malformed).unwrap(),
                    canonical_digest
                );
            }

            let mut canonical_by = canonical.clone();
            canonical_by.events[0].body = format!("{marker} BY K");
            let mut malformed_by = canonical.clone();
            malformed_by.events[0].body = format!("{marker} BY{space}K");
            assert_eq!(
                review_evidence_digest(&canonical_by).unwrap(),
                canonical_digest
            );
            assert!(has_comment_changes_requested(&malformed_by));
            assert_ne!(
                review_evidence_digest(&malformed_by).unwrap(),
                canonical_digest
            );

            let mut summary = canonical.clone();
            summary.events[0].body = "substantive result".into();
            let summary_digest = review_evidence_digest(&summary).unwrap();
            let mut canonical_disclosure = summary.clone();
            canonical_disclosure.events[0].body =
                "[team, reviewer, session, model, role=reviewer]\nsubstantive result".into();
            let mut malformed_disclosure = summary.clone();
            malformed_disclosure.events[0].body = format!(
                "[team,{space}reviewer, session, model, role=reviewer]\nsubstantive result"
            );
            assert_eq!(
                review_evidence_digest(&canonical_disclosure).unwrap(),
                summary_digest
            );
            assert_ne!(
                review_evidence_digest(&malformed_disclosure).unwrap(),
                summary_digest
            );

            let mut canonical_metadata = summary.clone();
            canonical_metadata.events[0].body =
                "substantive result\nUnverified --who metadata: K".into();
            let mut malformed_metadata = summary.clone();
            malformed_metadata.events[0].body =
                format!("substantive result\nUnverified --who metadata:{space}K");
            assert_eq!(
                review_evidence_digest(&canonical_metadata).unwrap(),
                summary_digest
            );
            assert_ne!(
                review_evidence_digest(&malformed_metadata).unwrap(),
                summary_digest
            );

            let withdrawal = format!("CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD}");
            assert!(retirement_record(&format!("{withdrawal}\nRETIRES 123456"))
                .unwrap()
                .is_some());
            let malformed_withdrawal = withdrawal.replacen(": ", &format!(":{space}"), 1);
            assert!(
                retirement_record(&format!("{malformed_withdrawal}\nRETIRES 123456"))
                    .unwrap_err()
                    .contains("canonical withdrawal")
            );
            assert!(
                retirement_record(&format!("{withdrawal}\nRETIRES{space}123456"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn unverified_retirement_stays_visible_and_does_not_clear_objection() {
        let observed_at = "2026-09-04T12:00:00Z";
        let retirement = ReviewEvidenceEvent {
            kind: "issue-comment".into(),
            identity: "comment-2".into(),
            author: String::new(),
            state: "ACTIVE".into(),
            head_sha: String::new(),
            created_at: observed_at.into(),
            updated_at: observed_at.into(),
            last_edited_at: String::new(),
            body: format!(
                "CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD} BY departed\nRETIRES 123456"
            ),
            retirement_actor_permission: String::new(),
        };
        let snapshot = ReviewEvidenceSnapshot {
            head_sha: REBASED_HEAD.into(),
            review_decision: "CHANGES_REQUESTED".into(),
            events: vec![
                ReviewEvidenceEvent {
                    kind: "review".into(),
                    identity: "review-1".into(),
                    author: "departed-reviewer".into(),
                    state: "CHANGES_REQUESTED".into(),
                    head_sha: REBASED_HEAD.into(),
                    created_at: observed_at.into(),
                    updated_at: observed_at.into(),
                    last_edited_at: String::new(),
                    body: "still unresolved".into(),
                    retirement_actor_permission: String::new(),
                },
                retirement,
            ],
        };
        let digest = review_evidence_digest(&snapshot).unwrap();
        let mut raw = node(394, REBASED_HEAD, &[]);
        raw.review_decision = "CHANGES_REQUESTED".into();
        raw.review_evidence_digest = digest;
        let applied = apply_landing_context(vec![raw], &[]).unwrap().remove(0);
        assert!(!applied.review_objections_resolved);
        assert_eq!(
            held_reasons(&[applied], &[])[0].reasons,
            ["changes-requested"]
        );
    }

    #[test]
    fn review_pass_labels_without_receipts_are_unbound_and_bad_receipts_refuse() {
        let labels = [
            "post-facto-human-review",
            "passed-review-codex",
            "passed-review-claude",
        ];
        let unbound = apply_landing_context(vec![node(394, REBASED_HEAD, &labels)], &[])
            .unwrap()
            .remove(0);
        assert_eq!(review_binding(&unbound).0, ReviewBinding::Unbound);

        let malformed = json!({"prs":[{
            "pr":394,
            "review_pass_heads":{"claude":"0fc9f61e"}
        }]});
        assert!(parse_landing_context(&malformed)
            .unwrap_err()
            .contains("exact 40-character lowercase hex SHA"));
        let unknown = json!({"prs":[{
            "pr":394,
            "review_pass_heads":{"other":REBASED_HEAD}
        }]});
        assert!(parse_landing_context(&unknown)
            .unwrap_err()
            .contains("unknown review lane"));
    }
}
