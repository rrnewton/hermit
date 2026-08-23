use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CiSelectionSpec {
    Uniform(bool),
    PerBackend(BTreeMap<String, bool>),
}

impl Default for CiSelectionSpec {
    fn default() -> Self {
        Self::Uniform(false)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CiDisabledReasonSpec {
    Uniform(String),
    PerBackend(BTreeMap<String, BackendCiDisabledReason>),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CiDisabledReasonDefault {
    BackendsDisabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CiDisabledResult {
    NotMeasured,
    DeterminismFailure,
    ParityFailure,
    ReplayFailure,
    CrashError,
    Timeout,
    Oom,
    InfrastructureError,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendCiDisabledReason {
    pub result: CiDisabledResult,
    pub evidence: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CiDisabledReasonData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CiDisabledResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct CiSelection {
    selected: BTreeMap<String, bool>,
    reasons: BTreeMap<String, CiDisabledReasonData>,
}

impl CiSelection {
    pub fn validate(
        enabled: &BTreeSet<String>,
        disabled: &BTreeSet<String>,
        selection: &CiSelectionSpec,
        reasons: Option<&CiDisabledReasonSpec>,
    ) -> Result<Self, String> {
        match selection {
            CiSelectionSpec::Uniform(selected) => {
                let selected_by_backend = enabled
                    .iter()
                    .cloned()
                    .map(|backend| (backend, *selected))
                    .collect();
                let reasons_by_backend = match (*selected, enabled.is_empty(), reasons) {
                    (true, true, _) => {
                        return Err("ci=true requires at least one enabled backend".into());
                    }
                    (true, false, None) => BTreeMap::new(),
                    (true, false, Some(_)) => {
                        return Err("ci=true must not carry ci_disabled_reason".into());
                    }
                    (false, false, Some(CiDisabledReasonSpec::Uniform(reason))) => {
                        let reason = validate_reason_text("ci_disabled_reason", reason)?;
                        enabled
                            .iter()
                            .cloned()
                            .map(|backend| {
                                (
                                    backend,
                                    CiDisabledReasonData {
                                        result: None,
                                        evidence: None,
                                        reason: reason.clone(),
                                    },
                                )
                            })
                            .collect()
                    }
                    (false, false, Some(CiDisabledReasonSpec::PerBackend(_))) => {
                        return Err(
                            "boolean ci=false requires one shared ci_disabled_reason string".into(),
                        );
                    }
                    (false, false, None) => {
                        return Err("ci=false requires ci_disabled_reason".into());
                    }
                    (false, true, None) => {
                        return Err("ci=false requires ci_disabled_reason".into());
                    }
                    (false, true, Some(CiDisabledReasonSpec::Uniform(reason))) => {
                        validate_reason_text("ci_disabled_reason", reason)?;
                        BTreeMap::new()
                    }
                    (false, true, Some(CiDisabledReasonSpec::PerBackend(_))) => {
                        return Err(
                            "boolean ci=false requires one shared ci_disabled_reason string".into(),
                        );
                    }
                };
                Ok(Self {
                    selected: selected_by_backend,
                    reasons: reasons_by_backend,
                })
            }
            CiSelectionSpec::PerBackend(selected) => {
                if enabled.is_empty() {
                    return Err("per-backend ci requires at least one enabled backend".into());
                }
                for backend in selected.keys() {
                    if disabled.contains(backend) {
                        return Err(format!(
                            "disabled backend {backend} must not appear in per-backend ci"
                        ));
                    }
                    if !enabled.contains(backend) {
                        return Err(format!(
                            "unknown backend {backend} must not appear in per-backend ci"
                        ));
                    }
                }
                for backend in enabled {
                    if !selected.contains_key(backend) {
                        return Err(format!("per-backend ci omits enabled backend {backend}"));
                    }
                }

                let reasons = match reasons {
                    Some(CiDisabledReasonSpec::PerBackend(reasons)) => reasons,
                    Some(CiDisabledReasonSpec::Uniform(_)) => {
                        return Err(
                            "per-backend ci requires per-backend ci_disabled_reason entries".into(),
                        );
                    }
                    None => {
                        if selected.values().any(|selected| !selected) {
                            return Err(
                                "per-backend ci=false requires its own ci_disabled_reason".into()
                            );
                        }
                        return Ok(Self {
                            selected: selected.clone(),
                            reasons: BTreeMap::new(),
                        });
                    }
                };

                for backend in reasons.keys() {
                    match selected.get(backend) {
                        Some(true) => {
                            return Err(format!(
                                "ci=true backend {backend} must not carry ci_disabled_reason"
                            ));
                        }
                        Some(false) => {}
                        None if disabled.contains(backend) => {
                            return Err(format!(
                                "disabled backend {backend} must not carry ci_disabled_reason"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "ci_disabled_reason names unknown backend {backend}"
                            ));
                        }
                    }
                }

                let mut normalized = BTreeMap::new();
                for (backend, is_selected) in selected {
                    if *is_selected {
                        continue;
                    }
                    let reason = reasons.get(backend).ok_or_else(|| {
                        format!("ci=false backend {backend} requires its own ci_disabled_reason")
                    })?;
                    validate_backend_reason(backend, reason)?;
                    normalized.insert(
                        backend.clone(),
                        CiDisabledReasonData {
                            result: Some(reason.result),
                            evidence: Some(reason.evidence.trim().to_string()),
                            reason: reason.reason.trim().to_string(),
                        },
                    );
                }
                Ok(Self {
                    selected: selected.clone(),
                    reasons: normalized,
                })
            }
        }
    }

    pub fn selected(&self, backend: &str) -> bool {
        self.selected.get(backend).copied().unwrap_or(false)
    }

    pub fn any_selected(&self) -> bool {
        self.selected.values().any(|selected| *selected)
    }

    pub fn reason(&self, backend: &str) -> Option<&CiDisabledReasonData> {
        self.reasons.get(backend)
    }
}

pub fn effective_ci_disabled_reason(
    enabled: &BTreeSet<String>,
    selection: &CiSelectionSpec,
    explicit: Option<&CiDisabledReasonSpec>,
    default: Option<CiDisabledReasonDefault>,
    disabled_reasons: &BTreeMap<String, String>,
) -> Result<Option<CiDisabledReasonSpec>, String> {
    if let Some(explicit) = explicit {
        return Ok(Some(explicit.clone()));
    }
    if !matches!(selection, CiSelectionSpec::Uniform(false)) || !enabled.is_empty() {
        return Ok(None);
    }
    let Some(CiDisabledReasonDefault::BackendsDisabled) = default else {
        return Ok(None);
    };
    if disabled_reasons.is_empty() {
        return Err(
            "ci_disabled_reason default backends-disabled requires backend-disabled reasons".into(),
        );
    }
    let mut parts = Vec::with_capacity(disabled_reasons.len());
    for (backend, reason) in disabled_reasons {
        let reason =
            validate_reason_text(&format!("backends_disabled reason for {backend}"), reason)?;
        parts.push(format!("{backend}: {reason}"));
    }
    Ok(Some(CiDisabledReasonSpec::Uniform(parts.join("; "))))
}

fn validate_reason_text(label: &str, reason: &str) -> Result<String, String> {
    let reason = reason.trim();
    if reason.len() < 16 || reason.split_whitespace().count() < 3 {
        return Err(format!(
            "{label} must explain why CI is disabled in at least three words"
        ));
    }
    let normalized = reason.to_ascii_lowercase();
    if [
        "not yet validated",
        "not yet qualified",
        "not selected yet",
        "temporarily disabled",
    ]
    .iter()
    .any(|placeholder| normalized.contains(placeholder))
    {
        return Err(format!(
            "{label} must state the measured result, not placeholder text"
        ));
    }
    Ok(reason.to_string())
}

fn validate_backend_reason(backend: &str, reason: &BackendCiDisabledReason) -> Result<(), String> {
    validate_reason_text(&format!("ci_disabled_reason for {backend}"), &reason.reason)?;
    let evidence = reason.evidence.trim();
    let evidence_is_locator = evidence.len() >= 8
        && (evidence.contains('/')
            || evidence.contains('#')
            || evidence.split_whitespace().count() >= 2
            || (evidence.len() == 40 && evidence.bytes().all(|byte| byte.is_ascii_hexdigit())));
    if !evidence_is_locator {
        return Err(format!(
            "ci_disabled_reason for {backend} must name retained evidence, a task, or an exact commit"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> BTreeSet<String> {
        ["ptrace", "liteinst"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn disabled() -> BTreeSet<String> {
        ["dbt", "kvm", "sabre"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn reason() -> BackendCiDisabledReason {
        BackendCiDisabledReason {
            result: CiDisabledResult::DeterminismFailure,
            evidence: "ignored/results/liteinst.jsonl".into(),
            reason: "canonical comparison diverged at scheduler turn 10".into(),
        }
    }

    #[test]
    fn uniform_booleans_keep_mode_wide_selection_semantics() {
        let selected = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::Uniform(true),
            None,
        )
        .unwrap();
        assert!(selected.selected("ptrace"));
        assert!(selected.selected("liteinst"));

        let unselected = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::Uniform(false),
            Some(&CiDisabledReasonSpec::Uniform(
                "not selected by the ordinary validation profile".into(),
            )),
        )
        .unwrap();
        assert!(!unselected.selected("ptrace"));
        assert!(!unselected.selected("liteinst"));
    }

    #[test]
    fn uniform_false_rejects_map_reason_even_when_no_backend_is_enabled() {
        let error = CiSelection::validate(
            &BTreeSet::new(),
            &disabled(),
            &CiSelectionSpec::Uniform(false),
            Some(&CiDisabledReasonSpec::PerBackend(BTreeMap::new())),
        )
        .unwrap_err();
        assert!(error.contains("boolean ci=false requires one shared"));
    }

    #[test]
    fn uniform_false_requires_a_named_reason_with_or_without_enabled_backends() {
        for enabled in [enabled(), BTreeSet::new()] {
            let error = CiSelection::validate(
                &enabled,
                &disabled(),
                &CiSelectionSpec::Uniform(false),
                None,
            )
            .unwrap_err();
            assert_eq!(error, "ci=false requires ci_disabled_reason");
        }
    }

    #[test]
    fn uniform_false_rejects_an_explanation_below_the_substance_floor() {
        let error = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::Uniform(false),
            Some(&CiDisabledReasonSpec::Uniform("not selected".into())),
        )
        .unwrap_err();
        assert!(error.contains("at least three words"));
    }

    #[test]
    fn explicit_backend_reason_provenance_derives_a_uniform_reason() {
        let disabled_reasons = BTreeMap::from([
            (
                "dbt".into(),
                "DBT does not implement this execution mode".into(),
            ),
            (
                "ptrace".into(),
                "The guest has no schedule-diversity oracle".into(),
            ),
        ]);
        let effective = effective_ci_disabled_reason(
            &BTreeSet::new(),
            &CiSelectionSpec::Uniform(false),
            None,
            Some(CiDisabledReasonDefault::BackendsDisabled),
            &disabled_reasons,
        )
        .unwrap();
        let policy = CiSelection::validate(
            &BTreeSet::new(),
            &disabled_reasons.keys().cloned().collect(),
            &CiSelectionSpec::Uniform(false),
            effective.as_ref(),
        )
        .unwrap();
        assert!(!policy.any_selected());
    }

    #[test]
    fn derived_backend_reason_must_meet_the_substance_floor() {
        let error = effective_ci_disabled_reason(
            &BTreeSet::new(),
            &CiSelectionSpec::Uniform(false),
            None,
            Some(CiDisabledReasonDefault::BackendsDisabled),
            &BTreeMap::from([("ptrace".into(), "unsupported".into())]),
        )
        .unwrap_err();
        assert!(error.contains("backends_disabled reason for ptrace"));
        assert!(error.contains("at least three words"));
    }

    #[test]
    fn accepts_mixed_backend_selection_and_preserves_the_failure_reason() {
        let policy = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::from([
                ("ptrace".into(), true),
                ("liteinst".into(), false),
            ])),
            Some(&CiDisabledReasonSpec::PerBackend(BTreeMap::from([(
                "liteinst".into(),
                reason(),
            )]))),
        )
        .unwrap();
        assert!(policy.selected("ptrace"));
        assert!(!policy.selected("liteinst"));
        assert_eq!(
            policy.reason("liteinst").unwrap().result,
            Some(CiDisabledResult::DeterminismFailure)
        );
    }

    #[test]
    fn rejects_omitted_enabled_backend() {
        let error = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::from([("ptrace".into(), true)])),
            None,
        )
        .unwrap_err();
        assert!(error.contains("omits enabled backend liteinst"));
    }

    #[test]
    fn rejects_empty_per_backend_selection_without_an_enabled_backend() {
        let error = CiSelection::validate(
            &BTreeSet::new(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::new()),
            None,
        )
        .unwrap_err();
        assert!(error.contains("requires at least one enabled backend"));
    }

    #[test]
    fn rejects_false_backend_without_its_own_reason() {
        let error = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::from([
                ("ptrace".into(), true),
                ("liteinst".into(), false),
            ])),
            None,
        )
        .unwrap_err();
        assert!(error.contains("requires its own ci_disabled_reason"));
    }

    #[test]
    fn rejects_true_backend_carrying_disabled_reason() {
        let error = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::from([
                ("ptrace".into(), true),
                ("liteinst".into(), false),
            ])),
            Some(&CiDisabledReasonSpec::PerBackend(BTreeMap::from([
                ("ptrace".into(), reason()),
                ("liteinst".into(), reason()),
            ]))),
        )
        .unwrap_err();
        assert!(error.contains("ci=true backend ptrace"));
    }

    #[test]
    fn rejects_disabled_backend_in_selection() {
        let error = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::from([
                ("ptrace".into(), true),
                ("liteinst".into(), false),
                ("dbt".into(), false),
            ])),
            Some(&CiDisabledReasonSpec::PerBackend(BTreeMap::from([
                ("liteinst".into(), reason()),
                ("dbt".into(), reason()),
            ]))),
        )
        .unwrap_err();
        assert!(error.contains("disabled backend dbt"));
    }

    #[test]
    fn rejects_placeholder_backend_reason() {
        let mut placeholder = reason();
        placeholder.evidence = "ignored/results/liteinst.jsonl".into();
        placeholder.reason = "not yet validated for ci".into();
        let error = CiSelection::validate(
            &enabled(),
            &disabled(),
            &CiSelectionSpec::PerBackend(BTreeMap::from([
                ("ptrace".into(), true),
                ("liteinst".into(), false),
            ])),
            Some(&CiDisabledReasonSpec::PerBackend(BTreeMap::from([(
                "liteinst".into(),
                placeholder,
            )]))),
        )
        .unwrap_err();
        assert!(error.contains("not placeholder text"));
    }
}
