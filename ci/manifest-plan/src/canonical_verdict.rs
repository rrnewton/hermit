use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerificationReport {
    pub verified: bool,
    pub bitwise_parity: bool,
    pub verdict: String,
    pub comparison: ComparisonReport,
    pub compared_log_messages: Option<ComparedLogMessages>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComparisonReport {
    pub strictness: String,
    pub compare_logs: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComparedLogMessages {
    pub left: u64,
    pub right: u64,
}

impl VerificationReport {
    /// Parse the complete typed receipt. A top-level JSON object is not enough:
    /// every nested field used by admission must deserialize successfully.
    #[allow(dead_code)] // this file is path-included by consumers that use one parse form
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes)
            .map_err(|error| format!("incomplete verification report: {error}"))
    }

    #[allow(dead_code)] // this file is path-included by consumers that use one parse form
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, String> {
        serde_json::from_value(value)
            .map_err(|error| format!("incomplete verification report: {error}"))
    }

    /// Prove that the invocation actually compared the canonical INFO evidence.
    /// This is separate from whether that comparison matched: a canonical
    /// divergence is a product result, while a stripped/output-only/empty-log
    /// comparison is incomplete evidence and therefore an infrastructure result.
    pub fn require_canonical_comparison(&self) -> Result<(), String> {
        let counts = self.compared_log_messages.as_ref();
        if self.comparison.strictness == "canonical"
            && self.comparison.compare_logs
            && counts.is_some_and(|counts| counts.left > 0 && counts.right > 0)
        {
            Ok(())
        } else {
            Err(format!(
                "verification did not compare canonical non-vacuous INFO evidence: strictness={} compare_logs={} messages={}/{}",
                self.comparison.strictness,
                self.comparison.compare_logs,
                counts.map_or(0, |counts| counts.left),
                counts.map_or(0, |counts| counts.right),
            ))
        }
    }

    /// Admit a green only when both the canonical comparison and the match
    /// claim agree. `verified=true` alone is intentionally insufficient: an
    /// output-only invocation can report it with zero compared INFO messages.
    pub fn require_canonical_match(&self) -> Result<(), String> {
        self.require_canonical_comparison()?;
        if self.verified && self.verdict == "matched" && self.bitwise_parity {
            Ok(())
        } else {
            Err(format!(
                "canonical verification did not match: verified={} verdict={} bitwise_parity={}",
                self.verified, self.verdict, self.bitwise_parity
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(strictness: &str, compare_logs: bool, left: u64, right: u64) -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: "matched".into(),
            comparison: ComparisonReport {
                strictness: strictness.into(),
                compare_logs,
            },
            compared_log_messages: Some(ComparedLogMessages { left, right }),
        }
    }

    #[test]
    fn brackets_every_canonical_match_requirement() {
        assert!(
            report("canonical", true, 1, 1)
                .require_canonical_match()
                .is_ok()
        );
        let mut weak = report("canonical", true, 1, 1);
        weak.verified = false;
        assert!(weak.require_canonical_match().is_err());
        let mut weak = report("canonical", true, 1, 1);
        weak.verdict = "diverged".into();
        assert!(weak.require_canonical_match().is_err());
        let mut weak = report("canonical", true, 1, 1);
        weak.bitwise_parity = false;
        assert!(weak.require_canonical_match().is_err());
        assert!(
            report("stripped", true, 1, 1)
                .require_canonical_match()
                .is_err()
        );
        assert!(
            report("canonical", false, 1, 1)
                .require_canonical_match()
                .is_err()
        );
        assert!(
            report("canonical", true, 0, 1)
                .require_canonical_match()
                .is_err()
        );
    }

    #[test]
    fn readable_object_with_unreadable_nested_predicate_is_refused() {
        let malformed = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "comparison": null,
            "compared_log_messages": {"left": 1, "right": 1}
        });
        assert!(VerificationReport::from_json_value(malformed).is_err());
    }
}
