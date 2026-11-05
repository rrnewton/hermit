//! Pure CI check selection and red classification.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};
use serde_json::{json, Map, Value};

use crate::model::{CheckRun, CiState, CiVerdict, RedClass};

const FAILED_CONCLUSIONS: [&str; 4] = ["FAILURE", "TIMED_OUT", "ERROR", "STARTUP_FAILURE"];
const PASSED_CONCLUSIONS: [&str; 1] = ["SUCCESS"];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Caller-supplied regular expressions identifying a known flaky failure.
pub struct FlakySignature {
    /// Case-insensitive expression matched against the check name.
    pub name_regex: String,
    /// Case-insensitive expression matched against the check message.
    pub text_regex: String,
    /// Optional operator-facing explanation of the signature.
    pub note: String,
}

impl FlakySignature {
    /// Compile both expressions and return a descriptive error for an invalid pattern.
    pub fn validate(&self) -> Result<(), String> {
        for (field, pattern) in [
            ("name_regex", self.name_regex.as_str()),
            ("text_regex", self.text_regex.as_str()),
        ] {
            if !pattern.is_empty() {
                RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map_err(|error| {
                        format!("invalid flaky signature {field} {pattern:?}: {error}")
                    })?;
            }
        }
        Ok(())
    }

    /// Return whether this signature matches a failed check.
    pub fn matches(&self, check: &CheckRun) -> bool {
        if self.name_regex.is_empty() && self.text_regex.is_empty() {
            return false;
        }
        let matches = |pattern: &str, value: &str| {
            pattern.is_empty()
                || RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map(|regex| regex.is_match(value))
                    .unwrap_or(false)
        };
        matches(&self.name_regex, &check.name) && matches(&self.text_regex, &check.text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Configuration for deterministic CI rollup classification.
pub struct ClassifyConfig {
    /// Exact name of the required landing gate.
    pub gate_check: String,
    /// Known flaky-failure signatures.
    pub flaky_signatures: Vec<FlakySignature>,
    /// Message fragments identifying an evaluate-once race.
    pub evaluate_once_markers: Vec<String>,
    /// Message fragments identifying a runner outage.
    pub outage_markers: Vec<String>,
    /// Maximum failed-gate duration treated as a job that never ran.
    pub outage_max_duration_secs: i64,
    /// Number of affected PRs required for the systemic-outage diagnostic.
    pub outage_min_prs: usize,
}

impl Default for ClassifyConfig {
    fn default() -> Self {
        Self {
            gate_check: "merge-gate".to_owned(),
            flaky_signatures: Vec::new(),
            evaluate_once_markers: vec![
                "full ci still queued".to_owned(),
                "rerun after ci completes".to_owned(),
                "still queued".to_owned(),
            ],
            outage_markers: vec![
                "blobnotfound".to_owned(),
                "no runner".to_owned(),
                "runner not found".to_owned(),
            ],
            outage_max_duration_secs: 5,
            outage_min_prs: 2,
        }
    }
}

impl ClassifyConfig {
    /// Validate the required gate name, thresholds, and regular expressions.
    pub fn validate(&self) -> Result<(), String> {
        if self.gate_check.trim().is_empty() {
            return Err("gate check name must be non-empty".to_owned());
        }
        if self.outage_max_duration_secs < 0 {
            return Err("outage thresholds must be nonnegative".to_owned());
        }
        for signature in &self.flaky_signatures {
            signature.validate()?;
        }
        Ok(())
    }
}

/// Interpret one check as passed, failed, or having no authoritative result.
pub fn classify_check(status: &str, conclusion: &str) -> CiState {
    let status = status.trim().to_ascii_uppercase();
    let conclusion = conclusion.trim().to_ascii_uppercase();
    if !status.is_empty() && status != "COMPLETED" {
        CiState::NoResult
    } else if FAILED_CONCLUSIONS.contains(&conclusion.as_str()) {
        CiState::Failed
    } else if PASSED_CONCLUSIONS.contains(&conclusion.as_str()) {
        CiState::Passed
    } else {
        CiState::NoResult
    }
}

fn scalar_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(value)) => if *value { "True" } else { "False" }.to_owned(),
        _ => String::new(),
    }
}

fn first_string(obj: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .map(|key| scalar_string(obj.get(*key)))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn int_field(obj: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_i64))
}

fn selection_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(value)) => value.trim().to_owned(),
        Some(Value::Number(value)) if value.as_f64() == Some(0.0) => String::new(),
        Some(Value::Number(value)) => value.to_string().trim().to_owned(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(_) => String::new(),
    }
}

fn head_matches(obj: &Map<String, Value>, requested: &str) -> bool {
    if requested.is_empty() {
        return true;
    }
    let observed = ["headSha", "head_sha", "headRefOid"]
        .iter()
        .map(|key| selection_text(obj.get(*key)))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    observed.is_empty() || observed == requested
}

fn check_context(obj: &Map<String, Value>) -> String {
    ["name", "context"]
        .iter()
        .map(|key| selection_text(obj.get(*key)))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn run_id(obj: &Map<String, Value>) -> i64 {
    for key in ["runId", "run_id"] {
        let Some(value) = obj.get(key) else { continue };
        if selection_text(Some(value)).is_empty() {
            continue;
        }
        let parsed = match value {
            Value::String(value) => value.parse().ok(),
            Value::Number(value) => value
                .as_i64()
                .or_else(|| value.as_f64().map(|value| value as i64)),
            Value::Bool(value) => Some(i64::from(*value)),
            _ => None,
        };
        if let Some(value) = parsed {
            return value;
        }
    }
    let url = ["detailsUrl", "details_url", "url", "html_url"]
        .iter()
        .map(|key| selection_text(obj.get(*key)))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    static RUN_URL: OnceLock<Regex> = OnceLock::new();
    RUN_URL
        .get_or_init(|| Regex::new(r"/actions/runs/(\d+)(?:/|$)").expect("constant regex"))
        .captures(&url)
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse().ok())
        .unwrap_or(0)
}

fn timestamp(obj: &Map<String, Value>) -> String {
    [
        "createdAt",
        "created_at",
        "startedAt",
        "started_at",
        "completedAt",
    ]
    .iter()
    .map(|key| selection_text(obj.get(*key)))
    .find(|value| !value.is_empty() && !value.starts_with("0001-01-01"))
    .unwrap_or_default()
}

fn recency_key(obj: &Map<String, Value>) -> (i64, String) {
    (run_id(obj), timestamp(obj))
}

fn outcome_identity(obj: &Map<String, Value>) -> (String, String) {
    let status = selection_text(obj.get("status"));
    let conclusion = {
        let conclusion = selection_text(obj.get("conclusion"));
        if conclusion.is_empty() {
            selection_text(obj.get("state"))
        } else {
            conclusion
        }
    };
    (status, conclusion)
}

struct SelectedCheck {
    recency: (i64, String),
    index: usize,
    check: Map<String, Value>,
}

/// Select one latest exact-head attempt per check context.
///
/// If equally recent attempts for one context carry contrary terminal outcomes, the selected
/// record is deliberately converted to NO_RESULT instead of depending on input order.
pub fn select_latest_checks(raw: &Value, head_sha: &str) -> Vec<Value> {
    let entries = raw.as_array().or_else(|| {
        raw.as_object().and_then(|obj| {
            obj.get("statusCheckRollup")
                .or_else(|| obj.get("check_runs"))
                .and_then(Value::as_array)
        })
    });
    let Some(entries) = entries else {
        return Vec::new();
    };
    let mut latest: BTreeMap<String, SelectedCheck> = BTreeMap::new();
    let mut order = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        if !head_matches(obj, head_sha) {
            continue;
        }
        let context = check_context(obj);
        let key = if context.is_empty() {
            format!("\0unnamed-{index}")
        } else {
            context.clone()
        };
        let candidate_key = recency_key(obj);
        let Some(previous) = latest.get(&key) else {
            order.push(key.clone());
            latest.insert(
                key,
                SelectedCheck {
                    recency: candidate_key,
                    index,
                    check: obj.clone(),
                },
            );
            continue;
        };
        match candidate_key.cmp(&previous.recency) {
            std::cmp::Ordering::Greater => {
                latest.insert(
                    key,
                    SelectedCheck {
                        recency: candidate_key,
                        index,
                        check: obj.clone(),
                    },
                );
            }
            std::cmp::Ordering::Equal => {
                if outcome_identity(&previous.check) != outcome_identity(obj) {
                    latest.insert(
                        key,
                        SelectedCheck {
                            recency: candidate_key,
                            index: previous.index.max(index),
                            check: json!({
                                "name": context,
                                "status": "AMBIGUOUS",
                                "conclusion": "",
                                "_selectionError": "duplicate check context has equal ordering identity and contrary verdicts",
                            })
                            .as_object()
                            .expect("object literal")
                            .clone(),
                        },
                    );
                } else if index > previous.index {
                    latest.insert(
                        key,
                        SelectedCheck {
                            recency: candidate_key,
                            index,
                            check: obj.clone(),
                        },
                    );
                }
            }
            std::cmp::Ordering::Less => {}
        }
    }
    order
        .into_iter()
        .filter_map(|key| latest.remove(&key))
        .map(|selected| Value::Object(selected.check))
        .collect()
}

/// Narrow a repository-host check rollup to the fields used by the planner.
pub fn parse_rollup(raw: &Value, head_sha: &str) -> Vec<CheckRun> {
    select_latest_checks(raw, head_sha)
        .into_iter()
        .filter_map(|entry| {
            let obj = entry.as_object()?;
            let mut conclusion = first_string(obj, &["conclusion"]).to_ascii_uppercase();
            if conclusion.is_empty() {
                conclusion = first_string(obj, &["state"]).to_ascii_uppercase();
            }
            Some(CheckRun {
                name: first_string(obj, &["name", "context", "workflowName"]),
                status: first_string(obj, &["status"]).to_ascii_uppercase(),
                conclusion,
                text: first_string(obj, &["text", "title", "description", "summary"]),
                workflow: first_string(obj, &["workflowName", "workflow"]),
                duration_secs: int_field(obj, &["duration_secs", "durationSecs"]),
            })
        })
        .collect()
}

/// Combine all selected checks into a three-state CI result.
pub fn classify_state(checks: &[CheckRun]) -> CiState {
    if checks.is_empty() {
        return CiState::NoResult;
    }
    let mut saw_no_result = false;
    for check in checks {
        match classify_check(&check.status, &check.conclusion) {
            CiState::Failed => return CiState::Failed,
            CiState::NoResult => saw_no_result = true,
            CiState::Passed => {}
        }
    }
    if saw_no_result {
        CiState::NoResult
    } else {
        CiState::Passed
    }
}

fn looks_like_missing_run(check: &CheckRun, cfg: &ClassifyConfig) -> bool {
    let text = check.text.to_ascii_lowercase();
    cfg.outage_markers
        .iter()
        .any(|marker| text.contains(&marker.to_ascii_lowercase()))
        || check.conclusion == "STARTUP_FAILURE"
        || (check
            .duration_secs
            .is_some_and(|duration| duration <= cfg.outage_max_duration_secs)
            && FAILED_CONCLUSIONS.contains(&check.conclusion.as_str()))
}

/// Classify a PR's CI state and, when red, identify the recommended failure class.
pub fn classify_pr(checks: &[CheckRun], cfg: &ClassifyConfig) -> CiVerdict {
    let raw_state = classify_state(checks);
    let gate = checks.iter().find(|check| check.name == cfg.gate_check);
    let gate_present = gate.is_some();
    let gate_ok = gate
        .is_some_and(|check| classify_check(&check.status, &check.conclusion) == CiState::Passed);
    let gate_missing_run = gate.is_some_and(|check| looks_like_missing_run(check, cfg));
    if raw_state != CiState::Failed {
        return CiVerdict {
            raw_state,
            red_class: None,
            gate_present,
            gate_ok,
            gate_missing_run,
            detail: raw_state.as_str().to_owned(),
        };
    }
    let red_checks: Vec<_> = checks
        .iter()
        .filter(|check| classify_check(&check.status, &check.conclusion) == CiState::Failed)
        .collect();
    let non_gate: Vec<_> = checks
        .iter()
        .filter(|check| check.name != cfg.gate_check)
        .cloned()
        .collect();
    let non_gate_state = classify_state(&non_gate);
    let verdict = |red_class, detail: String, missing| CiVerdict {
        raw_state,
        red_class: Some(red_class),
        gate_present,
        gate_ok,
        gate_missing_run: missing,
        detail,
    };
    if gate_missing_run {
        return verdict(
            RedClass::RunnerOutage,
            format!(
                "gate check '{}' never ran (runner outage signature)",
                cfg.gate_check
            ),
            true,
        );
    }
    if !red_checks.is_empty()
        && red_checks.iter().all(|check| {
            let text = check.text.to_ascii_lowercase();
            check.name == cfg.gate_check
                && cfg
                    .evaluate_once_markers
                    .iter()
                    .any(|marker| text.contains(&marker.to_ascii_lowercase()))
        })
    {
        return verdict(
            RedClass::EvaluateOnceRace,
            "gate evaluated once while full CI was still queued (benign; treat as pending)"
                .to_owned(),
            false,
        );
    }
    let gate_is_red = gate
        .is_some_and(|check| classify_check(&check.status, &check.conclusion) == CiState::Failed);
    let non_gate_reds = red_checks.iter().any(|check| check.name != cfg.gate_check);
    if gate_is_red && !non_gate_reds && non_gate_state == CiState::Passed {
        return verdict(
            RedClass::StaleRequiredCheck,
            format!(
                "CI green on head; required gate '{}' is stale",
                cfg.gate_check
            ),
            false,
        );
    }
    if !red_checks.is_empty()
        && red_checks
            .iter()
            .all(|check| cfg.flaky_signatures.iter().any(|sig| sig.matches(check)))
    {
        return verdict(
            RedClass::Flaky,
            "all red checks match a known flaky signature; refire CI".to_owned(),
            false,
        );
    }
    let names: BTreeSet<_> = red_checks
        .iter()
        .filter(|check| !cfg.flaky_signatures.iter().any(|sig| sig.matches(check)))
        .map(|check| check.name.clone())
        .collect();
    let detail = if names.is_empty() {
        "real red".to_owned()
    } else {
        format!(
            "real red on: {}",
            names.into_iter().collect::<Vec<_>>().join(", ")
        )
    };
    verdict(RedClass::Real, detail, false)
}

/// Parse flaky signatures from a top-level list or a `signatures` object field.
pub fn flaky_signatures_from_value(raw: &Value) -> Vec<FlakySignature> {
    let entries = raw
        .as_array()
        .or_else(|| raw.get("signatures").and_then(Value::as_array));
    entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let obj = entry.as_object()?;
            let sig = FlakySignature {
                name_regex: obj
                    .get("name_regex")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                text_regex: obj
                    .get("text_regex")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                note: obj
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            };
            (!sig.name_regex.is_empty() || !sig.text_regex.is_empty()).then_some(sig)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(name: &str, conclusion: &str) -> CheckRun {
        CheckRun {
            name: name.into(),
            status: "COMPLETED".into(),
            conclusion: conclusion.into(),
            ..Default::default()
        }
    }

    #[test]
    fn state_vocabulary_matches_reference() {
        assert_eq!(classify_state(&[]), CiState::NoResult);
        assert_eq!(classify_state(&[check("ci", "SUCCESS")]), CiState::Passed);
        assert_eq!(
            classify_state(&[check("ci", "CANCELLED")]),
            CiState::NoResult
        );
        assert_eq!(classify_state(&[check("ci", "TIMED_OUT")]), CiState::Failed);
    }

    #[test]
    fn exact_head_latest_and_tied_contrary_attempts() {
        let sha = "a".repeat(40);
        let raw = json!([
            {"name":"merge-gate","headSha":sha,"status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-01-01T00:00:00Z","detailsUrl":"https://x/runs/10/job/1"},
            {"name":"merge-gate","headSha":sha,"status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-01-02T00:00:00Z","detailsUrl":"https://x/runs/11/job/1"},
            {"name":"merge-gate","headSha":"wrong","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2027-01-01T00:00:00Z"}
        ]);
        let checks = parse_rollup(&raw, &sha);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].conclusion, "SUCCESS");

        let tied = json!([
            {"name":"gate","status":"COMPLETED","conclusion":"FAILURE","startedAt":"x","detailsUrl":"https://x/runs/11/job/100"},
            {"name":"gate","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"x","detailsUrl":"https://x/runs/11/job/101"}
        ]);
        assert_eq!(classify_state(&parse_rollup(&tied, "")), CiState::NoResult);
    }

    #[test]
    fn canonical_vocabulary_keeps_nonfailures_as_no_result() {
        for conclusion in [
            "CANCELLED",
            "SKIPPED",
            "NEUTRAL",
            "STALE",
            "ACTION_REQUIRED",
            "FUTURE_STATE",
        ] {
            assert_eq!(
                classify_state(&[check("ci", conclusion)]),
                CiState::NoResult
            );
        }
        for status in ["QUEUED", "IN_PROGRESS", "WAITING", "REQUESTED", "MISSING"] {
            assert_eq!(
                classify_state(&[CheckRun {
                    name: "ci".into(),
                    status: status.into(),
                    ..CheckRun::default()
                }]),
                CiState::NoResult
            );
        }
        for conclusion in ["FAILURE", "TIMED_OUT", "ERROR", "STARTUP_FAILURE"] {
            assert_eq!(classify_state(&[check("ci", conclusion)]), CiState::Failed);
        }
        assert_eq!(classify_check("IN_PROGRESS", "FAILURE"), CiState::NoResult);
        assert_eq!(classify_check("QUEUED", "SUCCESS"), CiState::NoResult);
    }

    #[test]
    fn five_red_classes_and_precedence_match_contract() {
        let cfg = ClassifyConfig::default();
        let verdict = classify_pr(
            &[check("ci", "SUCCESS"), check("merge-gate", "FAILURE")],
            &cfg,
        );
        assert_eq!(verdict.red_class, Some(RedClass::StaleRequiredCheck));

        let queued = CheckRun {
            name: "merge-gate".into(),
            status: "COMPLETED".into(),
            conclusion: "FAILURE".into(),
            text: "Full CI still queued".into(),
            ..CheckRun::default()
        };
        assert_eq!(
            classify_pr(&[queued], &cfg).red_class,
            Some(RedClass::EvaluateOnceRace)
        );
        let non_gate_queued = CheckRun {
            name: "unit".into(),
            status: "COMPLETED".into(),
            conclusion: "FAILURE".into(),
            text: "dependency still queued".into(),
            ..CheckRun::default()
        };
        assert_eq!(
            classify_pr(&[non_gate_queued, check("merge-gate", "SUCCESS")], &cfg).red_class,
            Some(RedClass::Real)
        );

        let outage = CheckRun {
            name: "merge-gate".into(),
            status: "COMPLETED".into(),
            conclusion: "FAILURE".into(),
            text: "BlobNotFound".into(),
            duration_secs: Some(1),
            ..CheckRun::default()
        };
        assert_eq!(
            classify_pr(std::slice::from_ref(&outage), &cfg).red_class,
            Some(RedClass::RunnerOutage)
        );

        let flaky_cfg = ClassifyConfig {
            flaky_signatures: vec![FlakySignature {
                name_regex: "unit".into(),
                ..FlakySignature::default()
            }],
            ..cfg.clone()
        };
        assert_eq!(
            classify_pr(&[check("unit", "FAILURE")], &flaky_cfg).red_class,
            Some(RedClass::Flaky)
        );
        assert_eq!(
            classify_pr(
                &[check("unit", "FAILURE"), check("other", "FAILURE")],
                &flaky_cfg
            )
            .red_class,
            Some(RedClass::Real)
        );

        let outage_and_flaky = ClassifyConfig {
            flaky_signatures: vec![FlakySignature {
                name_regex: "merge-gate".into(),
                ..FlakySignature::default()
            }],
            ..cfg
        };
        assert_eq!(
            classify_pr(&[outage], &outage_and_flaky).red_class,
            Some(RedClass::RunnerOutage)
        );
    }

    #[test]
    fn selection_accepts_wrappers_orders_by_run_then_time_and_preserves_context_order() {
        let wrapped = json!({"check_runs": [
            {"name":"z-first","status":"COMPLETED","conclusion":"SUCCESS"},
            {"name":"gate","runId":12,"startedAt":"2025-01-01","status":"COMPLETED","conclusion":"SUCCESS"},
            {"name":"gate","runId":11,"startedAt":"2027-01-01","status":"COMPLETED","conclusion":"FAILURE"},
            {"workflowName":"unnamed-a","status":"COMPLETED","conclusion":"SUCCESS"},
            {"workflowName":"unnamed-b","status":"COMPLETED","conclusion":"SUCCESS"}
        ]});
        let checks = parse_rollup(&wrapped, "");
        assert_eq!(checks.len(), 4);
        assert_eq!(checks[0].name, "z-first");
        assert_eq!(checks[1].name, "gate");
        assert_eq!(checks[1].conclusion, "SUCCESS");
        assert_eq!(checks[2].name, "unnamed-a");
        assert_eq!(checks[3].name, "unnamed-b");

        let sentinel = json!([
            {"name":"gate","runId":1,"startedAt":"2026-01-01","status":"COMPLETED","conclusion":"FAILURE"},
            {"name":"gate","runId":1,"createdAt":"2026-01-02","startedAt":"0001-01-01T00:00:00Z","status":"COMPLETED","conclusion":"SUCCESS"}
        ]);
        assert_eq!(parse_rollup(&sentinel, "")[0].conclusion, "SUCCESS");
    }
}
