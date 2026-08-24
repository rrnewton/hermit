use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerificationReport {
    pub verified: bool,
    pub bitwise_parity: bool,
    pub verdict: String,
    /// `None` when the producer reached no verdict. This is a DOCUMENTED
    /// producer state, not a malformed report: `VerificationReport::no_result()`
    /// in hermit-cli sets it, and its doc says "`null` when no verdict was
    /// reached". Declaring it non-optional made a legitimate no-result
    /// deserialise as "unreadable report", which reported an infrastructure
    /// fault where the truth was that nothing had been recorded.
    #[serde(deserialize_with = "present_but_nullable_comparison")]
    pub comparison: Option<ComparisonReport>,
    pub compared_log_messages: Option<ComparedLogMessages>,
    /// WHERE the divergence began, in scheduler-turn units. `None` when the
    /// logs matched, when no comparison ran, or when the report predates the
    /// field.
    ///
    /// `#[serde(default)]` is deliberate here and is the OPPOSITE choice from
    /// `ComparisonReport::record_envelope` above. That field is an admission
    /// gate, so a missing value must never be supplied on the producer's
    /// behalf. This one is a diagnostic position: absent means "not located",
    /// which is exactly what `None` says. Requiring it would turn every
    /// retained pre-field report into an unreadable-report ERROR and
    /// misclassify old evidence as an infrastructure fault -- the same trap
    /// documented on `comparison` just above.
    #[serde(default)]
    pub first_divergent_scheduler_turn: Option<u64>,
    /// WHERE the divergence began, in virtual-nanosecond units. Same
    /// tolerant-default rationale as the field above.
    ///
    /// NOTE both of these are the position of the PRECEDING scheduler COMMIT,
    /// so when no COMMIT precedes the differing record they collapse to the
    /// origin and bound the divergence rather than locating it. The record
    /// index that locates it exactly is a separate field being added by
    /// hermit#2386; this struct will gain it when that lands.
    #[serde(default)]
    pub first_divergent_virtual_nanoseconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComparisonReport {
    pub strictness: String,
    pub compare_logs: bool,
    /// REQUIRED, and deliberately so: do not add `#[serde(default)]`.
    ///
    /// This field exists to make record selection disclosed, so that a filter
    /// cannot remove records while the comparison still looks unfiltered.
    /// Giving it a default would supply the disclosure on the producer's behalf
    /// — undisclosed selection reported as if it had been stated, which is the
    /// exact failure the envelope was introduced to prevent. There is also no
    /// honest value to default to: `all_records_v1` would admit an unstated
    /// selection as canonical, and `caller_defined` would label a producer that
    /// said nothing as having said something.
    ///
    /// It is consumed only as an admission-gate input, by `is_canonical()` in
    /// [`VerificationReport::require_canonical_comparison`]. A report that never
    /// stated its envelope has not supplied admissible canonical evidence, so
    /// refusing it is the correct answer rather than a strictness accident.
    ///
    /// This is NOT the same situation as `comparison` above, which is
    /// present-but-nullable. There, `null` is a documented producer state and
    /// treating it as malformed reported a fault against the reader. Here,
    /// absence is not a producer state of any hermit that emits envelopes; it
    /// means an older binary — reachable through `HERMIT_BIN` — whose record
    /// selection is genuinely unknown. Note that the two go opposite ways for
    /// the same underlying reason: `present_but_nullable_comparison` exists to
    /// REMOVE serde's default leniency, not to add it.
    ///
    /// The refusal names this field, and
    /// `comparison_record_envelope_is_required_and_unknown_values_are_refused`
    /// asserts that it does, so an operator can distinguish a producer that
    /// predates the envelope from a corrupt report.
    pub record_envelope: RecordEnvelopeReport,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordEnvelopeReport {
    AllRecordsV1,
    DbtEvidenceTransportV1,
    CallerDefined,
}

impl RecordEnvelopeReport {
    fn is_canonical(self) -> bool {
        matches!(self, Self::AllRecordsV1 | Self::DbtEvidenceTransportV1)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AllRecordsV1 => "all_records_v1",
            Self::DbtEvidenceTransportV1 => "dbt_evidence_transport_v1",
            Self::CallerDefined => "caller_defined",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComparedLogMessages {
    pub left: u64,
    pub right: u64,
}

/// Accept `null` but not a MISSING field. serde maps an absent `Option` field to
/// `None` by default, which would let a report that never mentioned a comparison
/// read as a legitimate no-result. Naming a `deserialize_with` makes the field
/// required again while still admitting an explicit null.
fn present_but_nullable_comparison<'de, D>(
    deserializer: D,
) -> Result<Option<ComparisonReport>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<ComparisonReport>::deserialize(deserializer)
}

impl VerificationReport {
    /// `null` is legal ONLY for a report that reached no verdict. The producer
    /// documents it that way ("`null` when no verdict was reached"), and the
    /// sibling consumer in ci/compat-envelope/pressure-test.rs already applies
    /// exactly this rule. A null comparison beside a "matched" verdict is a
    /// self-contradictory report and stays refused at parse.
    fn require_null_comparison_matches_verdict(self) -> Result<Self, String> {
        if self.comparison.is_none() && self.verdict != "no_result" {
            return Err(format!(
                "incomplete verification report: comparison is null but verdict is {}; null is legal only for no_result",
                self.verdict
            ));
        }
        Ok(self)
    }

    /// Parse the complete typed receipt. A top-level JSON object is not enough:
    /// every nested field used by admission must deserialize successfully.
    #[allow(dead_code)] // this file is path-included by consumers that use one parse form
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice::<Self>(bytes)
            .map_err(|error| format!("incomplete verification report: {error}"))?
            .require_null_comparison_matches_verdict()
    }

    #[allow(dead_code)] // this file is path-included by consumers that use one parse form
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, String> {
        serde_json::from_value::<Self>(value)
            .map_err(|error| format!("incomplete verification report: {error}"))?
            .require_null_comparison_matches_verdict()
    }

    /// Prove that the invocation actually compared the canonical INFO evidence.
    /// This is separate from whether that comparison matched: a canonical
    /// divergence is a product result, while a stripped/output-only/empty-log
    /// comparison is incomplete evidence and therefore an infrastructure result.
    pub fn require_canonical_comparison(&self) -> Result<(), String> {
        let counts = self.compared_log_messages.as_ref();
        // No comparison at all is a distinct outcome from a comparison that was
        // too weak, and it is still not admissible. Naming it separately is the
        // whole point: "no verdict was recorded" is actionable against the
        // producer, while the old "unreadable report" pointed at the reader.
        let Some(comparison) = self.comparison.as_ref() else {
            return Err(format!(
                "verification recorded no comparison at all (verdict={}), so there is no canonical INFO evidence to admit",
                self.verdict
            ));
        };
        if comparison.strictness == "canonical"
            && comparison.compare_logs
            && comparison.record_envelope.is_canonical()
            && counts.is_some_and(|counts| counts.left > 0 && counts.right > 0)
        {
            Ok(())
        } else {
            Err(format!(
                "verification did not compare canonical non-vacuous INFO evidence: strictness={} compare_logs={} record_envelope={} messages={}/{}",
                comparison.strictness,
                comparison.compare_logs,
                comparison.record_envelope.as_str(),
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

    /// The divergence position must survive the parse. It was already being
    /// emitted by hermit and already present in the retained report string, but
    /// this struct dropped it, so no cell could ever report where its
    /// divergence began.
    #[test]
    fn divergence_position_survives_the_parse() {
        let json = br#"{"verified":false,"bitwise_parity":false,"verdict":"diverged",
            "comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":180,"right":180},
            "first_divergent_scheduler_turn":4,
            "first_divergent_virtual_nanoseconds":1767225600002825515}"#;
        let report = VerificationReport::from_json_slice(json).expect("diverged report parses");
        assert_eq!(report.first_divergent_scheduler_turn, Some(4));
        assert_eq!(
            report.first_divergent_virtual_nanoseconds,
            Some(1_767_225_600_002_825_515)
        );
    }

    /// An EARLIER divergence must report a SMALLER value in both units. This is
    /// the ordering the whole metric rests on: a fix that moves the divergence
    /// later has to be visible as a larger number, and a regression as a
    /// smaller one. Both figures below were measured with `hermit log-diff
    /// --json` against one real 131-line INFO log, diverged at two different
    /// depths -- after COMMIT turn 4 and after COMMIT turn 1.
    #[test]
    fn an_earlier_divergence_reports_a_smaller_position() {
        let parse = |turn: u64, nanos: u64| {
            let json = format!(
                r#"{{"verified":false,"bitwise_parity":false,"verdict":"diverged",
                    "comparison":{{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"}},"compared_log_messages":{{"left":180,"right":180}},
                    "first_divergent_scheduler_turn":{turn},
                    "first_divergent_virtual_nanoseconds":{nanos}}}"#
            );
            VerificationReport::from_json_slice(json.as_bytes()).expect("report parses")
        };
        let late = parse(4, 1_767_225_600_002_825_515);
        let early = parse(1, 1_767_225_600_000_500_000);
        assert!(
            early.first_divergent_scheduler_turn < late.first_divergent_scheduler_turn,
            "an earlier divergence must order before a later one in scheduler turns"
        );
        assert!(
            early.first_divergent_virtual_nanoseconds < late.first_divergent_virtual_nanoseconds,
            "and in virtual nanoseconds"
        );
    }

    /// A report written before the field existed must still parse. Requiring it
    /// would reclassify every retained pre-field report as an unreadable-report
    /// infrastructure fault.
    #[test]
    fn a_report_without_the_divergence_position_still_parses() {
        let json = br#"{"verified":true,"bitwise_parity":true,"verdict":"matched",
            "comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":180,"right":180}}"#;
        let report = VerificationReport::from_json_slice(json);
        let report = report.expect("a pre-field report is not malformed");
        assert_eq!(report.first_divergent_scheduler_turn, None);
        assert_eq!(report.first_divergent_virtual_nanoseconds, None);
    }

    fn report(strictness: &str, compare_logs: bool, left: u64, right: u64) -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: "matched".into(),
            comparison: Some(ComparisonReport {
                strictness: strictness.into(),
                compare_logs,
                record_envelope: RecordEnvelopeReport::AllRecordsV1,
            }),
            compared_log_messages: Some(ComparedLogMessages { left, right }),
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
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
        let mut weak = report("canonical", true, 1, 1);
        weak.comparison.as_mut().unwrap().record_envelope = RecordEnvelopeReport::CallerDefined;
        assert!(weak.require_canonical_match().is_err());
    }

    /// The exact bytes hermit writes for a run that reached no verdict, copied
    /// from a real capture rather than reconstructed. Before this was `Option`,
    /// parsing these failed with "invalid type: null, expected struct
    /// ComparisonReport at line 1 column 80" and the cell was scored ERROR
    /// "verification report is unreadable" -- an infrastructure fault where the
    /// truth was that the producer had recorded nothing.
    const REAL_NO_RESULT_REPORT: &str = concat!(
        r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","#,
        r#""comparison":null,"compared_log_messages":null,"guest_exit_code":null,"#,
        r#""guest_signal":null,"first_divergent_scheduler_turn":null,"#,
        r#""first_divergent_virtual_nanoseconds":null,"first_divergent_record":null}"#,
    );

    #[test]
    fn a_real_no_result_report_parses_instead_of_reading_as_unreadable() {
        let parsed = VerificationReport::from_json_slice(REAL_NO_RESULT_REPORT.as_bytes())
            .expect("a documented no-result report must parse");
        assert_eq!(parsed.verdict, "no_result");
        assert!(parsed.comparison.is_none());
    }

    #[test]
    fn a_no_result_report_is_still_inadmissible_and_names_the_real_reason() {
        // Parsing it is NOT admitting it. The point of the change is that the
        // refusal now points at the producer that recorded nothing, instead of
        // claiming the report could not be read.
        let parsed = VerificationReport::from_json_slice(REAL_NO_RESULT_REPORT.as_bytes())
            .expect("a documented no-result report must parse");
        let refusal = parsed
            .require_canonical_comparison()
            .expect_err("a report with no comparison must never be admitted");
        assert!(
            refusal.contains("recorded no comparison at all"),
            "refusal must name the missing comparison, got: {refusal}"
        );
        assert!(
            refusal.contains("no_result"),
            "refusal must carry the verdict, got: {refusal}"
        );
        assert!(parsed.require_canonical_match().is_err());
    }

    #[test]
    fn a_missing_comparison_field_is_refused_rather_than_defaulting_to_none() {
        // serde would otherwise map an absent Option field to None, letting a
        // report that never mentioned a comparison read as a legitimate
        // no-result. Optional must not mean absent.
        let absent = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","compared_log_messages":null}"#;
        assert!(VerificationReport::from_json_slice(absent.as_bytes()).is_err());
    }

    #[test]
    fn a_malformed_comparison_object_is_still_refused_at_parse() {
        // Optional must not mean lenient: a comparison that is present but the
        // wrong shape is still an unreadable report, not a silent None.
        let malformed = REAL_NO_RESULT_REPORT.replace(r#""comparison":null"#, r#""comparison":7"#);
        assert!(VerificationReport::from_json_slice(malformed.as_bytes()).is_err());
        let missing_field = REAL_NO_RESULT_REPORT.replace(
            r#""comparison":null"#,
            r#""comparison":{"compare_logs":true,"record_envelope":"all_records_v1"}"#,
        );
        assert!(VerificationReport::from_json_slice(missing_field.as_bytes()).is_err());
    }

    #[test]
    fn comparison_record_envelope_is_required_and_unknown_values_are_refused() {
        let missing = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "comparison": {"strictness": "canonical", "compare_logs": true},
            "compared_log_messages": {"left": 1, "right": 1}
        });
        let error = VerificationReport::from_json_value(missing)
            .expect_err("a comparison that never states its record envelope is not admissible")
            .to_string();
        // The runner surfaces this verbatim as "verification report is
        // unreadable: {error}". Refusing is only useful if the operator can
        // tell WHICH producer contract was unmet -- an old binary that predates
        // the field looks identical to a corrupt file unless the message names
        // it. Assert the naming rather than trusting serde to keep doing it.
        assert!(
            error.contains("record_envelope"),
            "the refusal must name the missing field so an operator can tell a producer that \
             predates the envelope from a corrupt report; got: {error}"
        );

        let unknown = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "comparison": {
                "strictness": "canonical",
                "compare_logs": true,
                "record_envelope": "unknown_v9"
            },
            "compared_log_messages": {"left": 1, "right": 1}
        });
        assert!(VerificationReport::from_json_value(unknown).is_err());
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
