//! Mechanical mechanism derivation and deterministic recognition.

use std::collections::BTreeSet;

use regex::Regex;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
/// Recognized operational mechanisms whose overlap deserves explicit review.
pub enum Mechanism {
    /// Workflow cancellation of an earlier in-progress run.
    CancelInProgress,
    /// Automatic pull-request workflow triggering.
    PrAutoTrigger,
    /// Scheduler or job-concurrency width.
    DagSchedulerWidth,
    /// Label used to report local validation.
    LocallyValidatedLabel,
    /// Storage location for validation evidence.
    ValidateLedgerPath,
    /// Required-check policy for the merge gate.
    MergeGateRequiredChecks,
}

impl Mechanism {
    /// Complete stable mechanism vocabulary.
    pub const ALL: [Self; 6] = [
        Self::CancelInProgress,
        Self::PrAutoTrigger,
        Self::DagSchedulerWidth,
        Self::LocallyValidatedLabel,
        Self::ValidateLedgerPath,
        Self::MergeGateRequiredChecks,
    ];

    /// Return the canonical machine-facing slug.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CancelInProgress => "cancel-in-progress",
            Self::PrAutoTrigger => "pr-auto-trigger",
            Self::DagSchedulerWidth => "dag-scheduler-width",
            Self::LocallyValidatedLabel => "locally-validated-label",
            Self::ValidateLedgerPath => "validate-ledger-path",
            Self::MergeGateRequiredChecks => "merge-gate-required-checks",
        }
    }

    fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::CancelInProgress => &["cancel-in-progress"],
            Self::PrAutoTrigger => &[
                "pr-auto-trigger",
                "on-pull-request",
                "pull-request-trigger",
                "pr-trigger",
                "pr-triggers",
            ],
            Self::DagSchedulerWidth => &[
                "dag-scheduler-width",
                "ci-dag-jobs",
                "dag-jobs",
                "dag-width",
            ],
            Self::LocallyValidatedLabel => &[
                "locally-validated-label",
                "locally-validated",
                "validated-locally",
            ],
            Self::ValidateLedgerPath => &["validate-ledger-path", "ledger-path", "validate-ledger"],
            Self::MergeGateRequiredChecks => &[
                "merge-gate-required-checks",
                "merge-gate",
                "required-checks",
            ],
        }
    }
}

fn normalize(raw: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in raw.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            if separator && !out.is_empty() {
                out.push('-');
            }
            separator = false;
            out.push(ch);
        } else {
            separator = true;
        }
    }
    out.trim_matches('-').to_owned()
}

fn alias_matches(normalized: &str, alias: &str) -> bool {
    normalized == alias
        || normalized.starts_with(&format!("{alias}-"))
        || normalized.ends_with(&format!("-{alias}"))
        || normalized.contains(&format!("-{alias}-"))
}

/// Classify a raw symbol or label slug as a recognized mechanism.
pub fn classify(raw: &str) -> Option<Mechanism> {
    let normalized = normalize(raw);
    Mechanism::ALL.into_iter().find(|mechanism| {
        mechanism
            .aliases()
            .iter()
            .any(|alias| alias_matches(&normalized, alias))
    })
}

/// Derive candidate constant names and mapping keys from added diff lines.
pub fn derive_symbols_from_diff(diff_text: &str) -> Vec<String> {
    let screaming = Regex::new(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b").expect("constant regex");
    let yaml_key = Regex::new(r"^\s*([A-Za-z][A-Za-z0-9_.-]*)\s*:").expect("constant regex");
    let mut symbols = BTreeSet::new();
    for line in diff_text.lines() {
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let body = &line[1..];
        symbols.extend(screaming.find_iter(body).map(|m| m.as_str().to_owned()));
        if let Some(captures) = yaml_key.captures(body) {
            symbols.insert(captures[1].to_owned());
        }
    }
    symbols.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_spellings_and_preserves_unknown() {
        assert_eq!(
            classify("concurrency.cancel-in-progress"),
            Some(Mechanism::CancelInProgress)
        );
        assert_eq!(
            classify("CANCEL_IN_PROGRESS"),
            Some(Mechanism::CancelInProgress)
        );
        assert_eq!(classify("NEW_UNSEEN_SWITCH"), None);
    }
}
