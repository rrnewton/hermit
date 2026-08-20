use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerificationReport {
    pub verified: bool,
    pub bitwise_parity: bool,
    pub verdict: String,
    /// `None` when the run reached no verdict.
    ///
    /// This mirrors the producer, which documents the field as "`null` when no
    /// verdict was reached" and writes exactly that from
    /// `VerificationReport::no_result()` (`hermit-cli/src/bin/hermit/verify.rs`).
    /// Declaring it non-optional made a legal producer value undeserialisable,
    /// so a truthful `no_result` surfaced as `incomplete verification report:
    /// invalid type: null, expected struct ComparisonReport` — an infrastructure
    /// "unreadable report" rather than a graded verdict. Measured on three
    /// consecutive `full` runs, that mislabelled the same 8 dbt cells every
    /// time, and `ERROR` sinks a bucket exactly like `FAIL`
    /// (`bin/test-harness.rs`: `failed |= result.outcome != "PASS"`).
    ///
    /// Note the sibling below was already `Option` and the producer nulls both
    /// in the same runs, so the two halves of one wire format disagreed.
    pub comparison: Option<ComparisonReport>,
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
        // A reachable, truthful state, not a parse failure: the run recorded no
        // verdict, so there is no comparison to grade. It still fails admission
        // -- absent evidence is not evidence -- but it is reported as what it is
        // rather than as a corrupt receipt, which is the distinction this
        // method's contract already draws between a product result and
        // incomplete evidence.
        let Some(comparison) = self.comparison.as_ref() else {
            return Err(format!(
                "verification recorded no comparison (verdict={}), so there is no canonical INFO evidence to admit",
                self.verdict
            ));
        };
        if comparison.strictness == "canonical"
            && comparison.compare_logs
            && counts.is_some_and(|counts| counts.left > 0 && counts.right > 0)
        {
            Ok(())
        } else {
            Err(format!(
                "verification did not compare canonical non-vacuous INFO evidence: strictness={} compare_logs={} messages={}/{}",
                comparison.strictness,
                comparison.compare_logs,
                counts.map_or(0, |counts| counts.left),
                counts.map_or(0, |counts| counts.right),
            ))
        }
    }

    /// Admit a green only when both the canonical comparison and the match
    /// claim agree. `verified=true` alone is intentionally insufficient: KVM's
    /// output-only fallback can report it with zero compared INFO messages.
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
            comparison: Some(ComparisonReport {
                strictness: strictness.into(),
                compare_logs,
            }),
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
        // Still refused, still for a nested reason -- but the nested value has
        // to be genuinely malformed. `comparison` present with a subfield of the
        // wrong type cannot be graded and is not something the producer emits,
        // so it remains a parse failure.
        let malformed = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "comparison": {"strictness": "canonical", "compare_logs": "yes"},
            "compared_log_messages": {"left": 1, "right": 1}
        });
        assert!(VerificationReport::from_json_value(malformed).is_err());
    }

    /// `comparison: null` USED to be asserted here as a parse failure. That was
    /// the defect, not the safety property: the producer documents `null` as the
    /// legal encoding of "no verdict was reached", so refusing to parse it
    /// reported a truthful receipt as a corrupt one.
    ///
    /// The safety property is unchanged and is what this now asserts directly: a
    /// null comparison must never yield a green. It is only the LABEL that
    /// moves, from "unreadable report" to a refusal that names the real cause.
    #[test]
    fn no_result_receipt_parses_but_can_never_be_admitted() {
        let no_result = serde_json::json!({
            "verified": false,
            "bitwise_parity": false,
            "verdict": "no_result",
            "comparison": null,
            "compared_log_messages": null
        });
        let report = VerificationReport::from_json_value(no_result)
            .expect("a documented no_result receipt must parse");
        assert!(report.comparison.is_none());

        let refusal = report
            .require_canonical_match()
            .expect_err("a no_result receipt must never be admitted as a green");
        assert!(
            refusal.contains("recorded no comparison") && refusal.contains("no_result"),
            "the refusal must name the real cause rather than implying a corrupt file: {refusal}"
        );
        assert!(
            report.require_canonical_comparison().is_err(),
            "absent evidence is not evidence"
        );
    }

    /// A green claim with a null comparison is the dangerous shape: everything
    /// else about the receipt says "matched". It must still be refused.
    #[test]
    fn null_comparison_cannot_be_admitted_even_when_the_receipt_claims_a_match() {
        let forged = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "comparison": null,
            "compared_log_messages": {"left": 110, "right": 110}
        });
        let report = VerificationReport::from_json_value(forged).expect("parses");
        assert!(report.require_canonical_match().is_err());
    }
}
