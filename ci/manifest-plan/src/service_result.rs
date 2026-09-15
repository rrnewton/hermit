//! Serialized terminal result written by the validation framework.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const HISTORICAL_SCHEMA_VERSION: u64 = 1;
pub const WRITEBACK_SCHEMA_VERSION: u64 = 2;
pub const SELECTION_SCHEMA_VERSION: u64 = 3;
pub const TEST_COUNTS_SCHEMA_VERSION: u64 = 4;
pub const DETAIL_SCHEMA_VERSION: u64 = 5;
pub const SCHEMA_VERSION: u64 = 6;
pub const HISTORICAL_FIELD_NAMES: [&str; 7] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
];
pub const WRITEBACK_FIELD_NAMES: [&str; 8] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "scorecard_writeback",
];
pub const SELECTION_FIELD_NAMES: [&str; 9] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "scorecard_writeback",
];
pub const TEST_COUNTS_FIELD_NAMES: [&str; 10] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "passed_tests",
    "scorecard_writeback",
];
pub const DETAIL_FIELD_NAMES: [&str; 11] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "detail",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "passed_tests",
    "scorecard_writeback",
];
pub const FIELD_NAMES: [&str; 12] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "could_not_run_kind",
    "detail",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "passed_tests",
    "scorecard_writeback",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FinalValidateStatus {
    Passed,
    Failed,
    CouldNotRun,
}

impl FinalValidateStatus {
    pub const ALL: [Self; 3] = [Self::Passed, Self::Failed, Self::CouldNotRun];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "PASSED",
            Self::Failed => "FAILED",
            Self::CouldNotRun => "COULD_NOT_RUN",
        }
    }

    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Passed => 0,
            Self::Failed => 1,
            Self::CouldNotRun => 75,
        }
    }

    pub const fn service_state(self) -> &'static str {
        "completed"
    }

    pub const fn service_result(self) -> &'static str {
        match self {
            Self::Passed => "success",
            Self::Failed => "failure",
            Self::CouldNotRun => "no-result",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CouldNotRunKind {
    NoResult,
    Refused,
    Interrupted,
}

impl CouldNotRunKind {
    pub const ALL: [Self; 3] = [Self::NoResult, Self::Refused, Self::Interrupted];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoResult => "no_result",
            Self::Refused => "refused",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Outcome of publishing already-final validation evidence into the scorecard.
/// This is bookkeeping about a completed validation, not the validation verdict.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ScorecardWriteback {
    Completed,
    Failed { error: String },
}

impl ScorecardWriteback {
    pub const ALL: [&'static str; 2] = ["completed", "failed"];

    pub fn field_names(status: &str) -> &'static [&'static str] {
        match status {
            "completed" => &["status"],
            "failed" => &["status", "error"],
            _ => &[],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationServiceResult {
    pub schema_version: u64,
    pub commit: String,
    pub profile: String,
    /// The plan selection that was actually executed. Historical schemas did
    /// not carry this, so retained rows decode as unknown rather than being
    /// mistaken for a complete full-profile run.
    #[serde(default)]
    pub selection_mode: Option<String>,
    pub final_validate_status: FinalValidateStatus,
    /// The producer path that reached a current COULD_NOT_RUN verdict. Schema 6
    /// makes this required for that status so readers never infer the cause
    /// from prose. Older schemas decode it as absent.
    #[serde(default)]
    pub could_not_run_kind: Option<CouldNotRunKind>,
    /// Ordered terminal detail rendered by validation. A current producer
    /// writes the field even when no cause exists, so null is honest absence
    /// rather than a missing field.
    #[serde(default)]
    pub detail: Option<Vec<String>>,
    /// The command exit, not the validation verdict's exit. A PASSED validation
    /// may therefore carry 75 when its required scorecard write-back failed.
    pub exit_code: i32,
    pub executed_nodes: u64,
    pub executed_tests: Option<i64>,
    /// Exact terminal passes recorded by the test frameworks. Historical
    /// schemas omitted this field, so absence remains unknown rather than being
    /// reconstructed from the verdict or executed-test count.
    #[serde(default)]
    pub passed_tests: Option<i64>,
    /// Kept separate from `final_validate_status`: a write-back failure must
    /// make the command fail loudly without rewriting a real tree verdict.
    #[serde(default)]
    pub scorecard_writeback: Option<ScorecardWriteback>,
}

impl ValidationServiceResult {
    pub fn validated(self) -> Result<Self, String> {
        self.validate()?;
        Ok(self)
    }

    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, String> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("validation-service-result-shape: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "validation-service-result-shape: expected an object".to_string())?;
        let schema_version = object
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                "validation-service-result-schema_version: must be an unsigned integer".to_string()
            })?;
        let expected_fields: BTreeSet<&str> = match schema_version {
            HISTORICAL_SCHEMA_VERSION => HISTORICAL_FIELD_NAMES.into_iter().collect(),
            WRITEBACK_SCHEMA_VERSION => WRITEBACK_FIELD_NAMES.into_iter().collect(),
            SELECTION_SCHEMA_VERSION => SELECTION_FIELD_NAMES.into_iter().collect(),
            TEST_COUNTS_SCHEMA_VERSION => TEST_COUNTS_FIELD_NAMES.into_iter().collect(),
            DETAIL_SCHEMA_VERSION => DETAIL_FIELD_NAMES.into_iter().collect(),
            SCHEMA_VERSION => FIELD_NAMES.into_iter().collect(),
            other => {
                return Err(format!(
                    "validation-service-result-schema_version: expected {HISTORICAL_SCHEMA_VERSION}, {WRITEBACK_SCHEMA_VERSION}, {SELECTION_SCHEMA_VERSION}, {TEST_COUNTS_SCHEMA_VERSION}, {DETAIL_SCHEMA_VERSION}, or {SCHEMA_VERSION}, got {other}"
                ));
            }
        };
        let actual_fields: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        if actual_fields != expected_fields {
            return Err(format!(
                "validation-service-result-fields: schema {schema_version} expected {expected_fields:?}, got {actual_fields:?}"
            ));
        }
        let result: Self = if schema_version == SCHEMA_VERSION {
            // A Value has already collapsed duplicate fields. Parse current
            // evidence from the original bytes so causes cannot be relabelled
            // by a later occurrence, while keeping historical readers intact.
            serde_json::from_slice(bytes)
        } else {
            serde_json::from_value(value)
        }
        .map_err(|error| format!("validation-service-result-shape: {error}"))?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if ![
            HISTORICAL_SCHEMA_VERSION,
            WRITEBACK_SCHEMA_VERSION,
            SELECTION_SCHEMA_VERSION,
            TEST_COUNTS_SCHEMA_VERSION,
            DETAIL_SCHEMA_VERSION,
            SCHEMA_VERSION,
        ]
        .contains(&self.schema_version)
        {
            return Err(format!(
                "validation-service-result-schema_version: expected {HISTORICAL_SCHEMA_VERSION}, {WRITEBACK_SCHEMA_VERSION}, {SELECTION_SCHEMA_VERSION}, {TEST_COUNTS_SCHEMA_VERSION}, {DETAIL_SCHEMA_VERSION}, or {SCHEMA_VERSION}, got {}",
                self.schema_version
            ));
        }
        if self.commit.len() != 40
            || !self
                .commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(
                "validation-service-result-commit: must be lowercase forty-hex".to_string(),
            );
        }
        if self.profile.is_empty() {
            return Err("validation-service-result-profile: must be nonempty".to_string());
        }
        if self
            .selection_mode
            .as_ref()
            .is_some_and(|selection| selection.trim().is_empty())
        {
            return Err(
                "validation-service-result-selection_mode: must be nonempty or null".to_string(),
            );
        }
        if let Some(detail) = &self.detail {
            if detail.is_empty() || detail.iter().any(|line| line.trim().is_empty()) {
                return Err(
                    "validation-service-result-detail: must be a nonempty list of nonempty strings or null"
                        .to_string(),
                );
            }
        }
        if self.schema_version < DETAIL_SCHEMA_VERSION && self.detail.is_some() {
            return Err(format!(
                "validation-service-result-historical-fields: schema {} cannot carry detail",
                self.schema_version
            ));
        }
        if self.final_validate_status != FinalValidateStatus::CouldNotRun && self.detail.is_some() {
            return Err(format!(
                "validation-service-result-detail: {} must carry null",
                self.final_validate_status.as_str()
            ));
        }
        if self.schema_version < SCHEMA_VERSION && self.could_not_run_kind.is_some() {
            return Err(format!(
                "validation-service-result-historical-fields: schema {} cannot carry could_not_run_kind",
                self.schema_version
            ));
        }
        if self.schema_version == SCHEMA_VERSION {
            match self.final_validate_status {
                FinalValidateStatus::CouldNotRun => {
                    if self.could_not_run_kind.is_none() {
                        return Err(
                            "validation-service-result-could_not_run_kind: COULD_NOT_RUN requires no_result, refused, or interrupted"
                                .to_string(),
                        );
                    }
                    if self.detail.is_none() {
                        return Err(
                            "validation-service-result-detail: schema 6 COULD_NOT_RUN requires a concrete reason"
                                .to_string(),
                        );
                    }
                }
                FinalValidateStatus::Passed | FinalValidateStatus::Failed => {
                    if self.could_not_run_kind.is_some() {
                        return Err(format!(
                            "validation-service-result-could_not_run_kind: {} must carry null",
                            self.final_validate_status.as_str()
                        ));
                    }
                }
            }
        }
        let validation_exit = self.final_validate_status.exit_code();
        if self.schema_version == HISTORICAL_SCHEMA_VERSION {
            if self.scorecard_writeback.is_some() || self.selection_mode.is_some() {
                return Err(
                    "validation-service-result-historical-fields: schema 1 cannot carry selection_mode or scorecard_writeback"
                        .to_string(),
                );
            }
            if self.exit_code != validation_exit {
                return Err(format!(
                    "validation-service-result-exit_code: historical schema 1 {} requires {validation_exit}, got {}",
                    self.final_validate_status.as_str(),
                    self.exit_code
                ));
            }
        } else {
            if self.schema_version == WRITEBACK_SCHEMA_VERSION && self.selection_mode.is_some() {
                return Err(
                    "validation-service-result-selection_mode: schema 2 cannot carry this field"
                        .to_string(),
                );
            }
            if let Some(ScorecardWriteback::Failed { error }) = &self.scorecard_writeback {
                if error.trim().is_empty() {
                    return Err(
                        "validation-service-result-scorecard_writeback: failed requires a nonempty error"
                            .to_string(),
                    );
                }
            }
            let expected_command_exit = match (
                self.final_validate_status,
                self.scorecard_writeback.as_ref(),
            ) {
                (FinalValidateStatus::Passed, Some(ScorecardWriteback::Failed { .. })) => 75,
                _ => validation_exit,
            };
            if self.exit_code != expected_command_exit {
                return Err(format!(
                    "validation-service-result-exit_code: {} with scorecard_writeback {:?} requires {expected_command_exit}, got {}",
                    self.final_validate_status.as_str(),
                    self.scorecard_writeback,
                    self.exit_code
                ));
            }
        }
        if self.executed_tests.is_some_and(|count| count < 0) {
            return Err(
                "validation-service-result-executed_tests: must be nonnegative or null".to_string(),
            );
        }
        if self.schema_version < TEST_COUNTS_SCHEMA_VERSION && self.passed_tests.is_some() {
            return Err(format!(
                "validation-service-result-historical-fields: schema {} cannot carry passed_tests",
                self.schema_version
            ));
        }
        if self.schema_version >= TEST_COUNTS_SCHEMA_VERSION
            && self.executed_tests.is_some()
            && self.passed_tests.is_none()
        {
            return Err(
                "validation-service-result-passed_tests: current result with executed_tests requires an exact count"
                    .to_string(),
            );
        }
        if self.passed_tests.is_some_and(|count| count < 0) {
            return Err(
                "validation-service-result-passed_tests: must be nonnegative or null".to_string(),
            );
        }
        if let Some(passed) = self.passed_tests {
            let executed = self.executed_tests.ok_or_else(|| {
                "validation-service-result-passed_tests: cannot be present when executed_tests is null"
                    .to_string()
            })?;
            if passed > executed {
                return Err(format!(
                    "validation-service-result-passed_tests: {passed} exceeds executed_tests {executed}"
                ));
            }
        }
        if self.schema_version >= TEST_COUNTS_SCHEMA_VERSION
            && self.final_validate_status == FinalValidateStatus::Passed
        {
            let passed = self.passed_tests.ok_or_else(|| {
                "validation-service-result-passed_tests: current PASSED result requires an exact count"
                    .to_string()
            })?;
            let executed = self.executed_tests.ok_or_else(|| {
                "validation-service-result-executed_tests: current PASSED result requires an exact count"
                    .to_string()
            })?;
            if passed != executed {
                return Err(format!(
                    "validation-service-result-passed_tests: current PASSED result requires passed_tests == executed_tests, got {passed} != {executed}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ValidationServiceResult {
        ValidationServiceResult {
            schema_version: SCHEMA_VERSION,
            commit: "a".repeat(40),
            profile: "full".into(),
            selection_mode: Some("full".into()),
            final_validate_status: FinalValidateStatus::Passed,
            could_not_run_kind: None,
            detail: None,
            exit_code: 0,
            executed_nodes: 76,
            executed_tests: Some(2129),
            passed_tests: Some(2129),
            scorecard_writeback: None,
        }
        .validated()
        .unwrap()
    }

    #[test]
    fn current_shape_round_trips_and_refuses_unknown_fields() {
        let encoded = serde_json::to_vec(&valid()).unwrap();
        assert_eq!(
            ValidationServiceResult::from_json_slice(&encoded).unwrap(),
            valid()
        );
        let mut value = serde_json::to_value(valid()).unwrap();
        value["future"] = Value::Bool(true);
        let error = ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(error.contains("validation-service-result-fields"));
        assert!(error.contains("future"));
    }

    #[test]
    fn status_and_exit_must_agree() {
        let mut result = valid();
        result.exit_code = 1;
        assert_eq!(
            result.validate().unwrap_err(),
            "validation-service-result-exit_code: PASSED with scorecard_writeback None requires 0, got 1"
        );
    }

    #[test]
    fn failed_writeback_preserves_pass_and_requires_loud_command_exit() {
        let mut result = valid();
        result.exit_code = 75;
        result.scorecard_writeback = Some(ScorecardWriteback::Failed {
            error: "fixture refusal".into(),
        });
        result.validate().unwrap();

        result.exit_code = 0;
        assert!(
            result
                .validate()
                .unwrap_err()
                .contains("requires 75, got 0")
        );
    }

    #[test]
    fn genuine_could_not_run_remains_distinct_from_writeback_failure() {
        for detail in [
            vec![
                "refused by: pre.reverie_pin".into(),
                "recorded pin is not an ancestor".into(),
            ],
            vec![
                "REFUSED ON COMPLETENESS: ZERO tests executed".into(),
                "0 test(s) executed, 0 passed, 4 filtered".into(),
            ],
        ] {
            let result = ValidationServiceResult {
                schema_version: SCHEMA_VERSION,
                commit: "a".repeat(40),
                profile: "full".into(),
                selection_mode: Some("full".into()),
                final_validate_status: FinalValidateStatus::CouldNotRun,
                could_not_run_kind: Some(CouldNotRunKind::Refused),
                detail: Some(detail.clone()),
                exit_code: 75,
                executed_nodes: 0,
                executed_tests: None,
                passed_tests: None,
                scorecard_writeback: None,
            }
            .validated()
            .unwrap();
            assert_eq!(
                result.final_validate_status,
                FinalValidateStatus::CouldNotRun
            );
            assert_eq!(result.detail, Some(detail));
            assert_eq!(result.scorecard_writeback, None);
        }
    }

    #[test]
    fn historical_schemas_are_readable_only_in_their_exact_old_shapes() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(HISTORICAL_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("selection_mode");
        value.as_object_mut().unwrap().remove("could_not_run_kind");
        value.as_object_mut().unwrap().remove("detail");
        value.as_object_mut().unwrap().remove("passed_tests");
        value.as_object_mut().unwrap().remove("scorecard_writeback");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, HISTORICAL_SCHEMA_VERSION);
        assert_eq!(decoded.selection_mode, None);
        assert_eq!(decoded.passed_tests, None);
        assert_eq!(decoded.scorecard_writeback, None);

        value["scorecard_writeback"] = Value::Null;
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 1 expected")
        );

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(WRITEBACK_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("selection_mode");
        value.as_object_mut().unwrap().remove("could_not_run_kind");
        value.as_object_mut().unwrap().remove("detail");
        value.as_object_mut().unwrap().remove("passed_tests");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, WRITEBACK_SCHEMA_VERSION);
        assert_eq!(decoded.selection_mode, None);
        assert_eq!(decoded.passed_tests, None);

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(SELECTION_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("could_not_run_kind");
        value.as_object_mut().unwrap().remove("detail");
        value.as_object_mut().unwrap().remove("passed_tests");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, SELECTION_SCHEMA_VERSION);
        assert_eq!(decoded.passed_tests, None);

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(TEST_COUNTS_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("could_not_run_kind");
        value.as_object_mut().unwrap().remove("detail");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, TEST_COUNTS_SCHEMA_VERSION);
        assert_eq!(decoded.passed_tests, Some(2129));
        assert_eq!(decoded.detail, None);

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(DETAIL_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("could_not_run_kind");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, DETAIL_SCHEMA_VERSION);
        assert_eq!(decoded.could_not_run_kind, None);
        assert_eq!(decoded.detail, None);
    }

    #[test]
    fn current_schema_refuses_a_missing_writeback_field() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("scorecard_writeback");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 6 expected")
        );
    }

    #[test]
    fn current_schema_refuses_a_missing_selection_field() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("selection_mode");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 6 expected")
        );
    }

    #[test]
    fn current_schema_refuses_a_missing_passed_count() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("passed_tests");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 6 expected")
        );
    }

    #[test]
    fn current_detail_and_could_not_run_kind_are_required_and_consistent() {
        let mut missing = serde_json::to_value(valid()).unwrap();
        missing.as_object_mut().unwrap().remove("detail");
        let error =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&missing).unwrap())
                .unwrap_err();
        assert!(error.contains("schema 6 expected"), "{error}");

        let mut missing_kind = serde_json::to_value(valid()).unwrap();
        missing_kind
            .as_object_mut()
            .unwrap()
            .remove("could_not_run_kind");
        let error =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&missing_kind).unwrap())
                .unwrap_err();
        assert!(error.contains("schema 6 expected"), "{error}");

        for status in [FinalValidateStatus::Passed, FinalValidateStatus::Failed] {
            let mut result = valid();
            result.final_validate_status = status;
            result.exit_code = status.exit_code();
            result.detail = Some(vec!["must not survive".into()]);
            let error = result.validate().unwrap_err();
            assert!(
                error.contains(&format!("{} must carry null", status.as_str())),
                "{error}"
            );
        }

        let mut no_result = valid();
        no_result.final_validate_status = FinalValidateStatus::CouldNotRun;
        no_result.exit_code = 75;
        no_result.executed_tests = None;
        no_result.passed_tests = None;
        no_result.could_not_run_kind = Some(CouldNotRunKind::NoResult);
        no_result.detail = None;
        assert!(
            no_result
                .validate()
                .unwrap_err()
                .contains("concrete reason")
        );
        no_result.detail = Some(vec![]);
        assert!(no_result.validate().unwrap_err().contains("nonempty list"));
        no_result.detail = Some(vec![" ".into()]);
        assert!(no_result.validate().unwrap_err().contains("nonempty list"));

        let mut malformed = serde_json::to_value(&no_result).unwrap();
        malformed["detail"] = Value::String("not an ordered list".into());
        let error =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&malformed).unwrap())
                .unwrap_err();
        assert!(error.contains("validation-service-result-shape"), "{error}");

        no_result.detail = Some(vec!["exact reason".into()]);
        for kind in CouldNotRunKind::ALL {
            no_result.could_not_run_kind = Some(kind);
            no_result.validate().unwrap();
            let encoded = serde_json::to_value(&no_result).unwrap();
            assert_eq!(encoded["could_not_run_kind"], kind.as_str());
        }

        let encoded = serde_json::to_string(&no_result).unwrap();
        for duplicate in [
            r#""could_not_run_kind":"refused""#,
            r#""detail":["different reason"]"#,
            r#""schema_version":6"#,
        ] {
            let repeated = format!("{},{duplicate}}}", encoded.strip_suffix('}').unwrap());
            let error = ValidationServiceResult::from_json_slice(repeated.as_bytes()).unwrap_err();
            assert!(error.contains("duplicate field"), "{error}");
        }

        no_result.could_not_run_kind = None;
        assert!(
            no_result
                .validate()
                .unwrap_err()
                .contains("COULD_NOT_RUN requires")
        );

        let mut failed_with_kind = valid();
        failed_with_kind.final_validate_status = FinalValidateStatus::Failed;
        failed_with_kind.exit_code = 1;
        failed_with_kind.could_not_run_kind = Some(CouldNotRunKind::Interrupted);
        assert!(
            failed_with_kind
                .validate()
                .unwrap_err()
                .contains("must carry null")
        );

        let mut legacy_no_result = no_result.clone();
        legacy_no_result.schema_version = DETAIL_SCHEMA_VERSION;
        legacy_no_result.could_not_run_kind = None;
        legacy_no_result.detail = None;
        legacy_no_result
            .validate()
            .expect("schema 5 explicit null remains readable");
    }

    #[test]
    fn current_pass_refuses_inexact_or_contradictory_passed_counts() {
        let mut missing = valid();
        missing.passed_tests = None;
        assert!(
            missing
                .validate()
                .unwrap_err()
                .contains("requires an exact count")
        );

        let mut lower = valid();
        lower.passed_tests = Some(2128);
        assert!(
            lower
                .validate()
                .unwrap_err()
                .contains("requires passed_tests == executed_tests")
        );

        let mut greater = valid();
        greater.passed_tests = Some(2130);
        assert!(
            greater
                .validate()
                .unwrap_err()
                .contains("exceeds executed_tests")
        );

        let mut missing_executed = valid();
        missing_executed.executed_tests = None;
        assert!(
            missing_executed
                .validate()
                .unwrap_err()
                .contains("cannot be present when executed_tests is null")
        );
    }

    #[test]
    fn current_failure_and_no_result_keep_distinct_count_semantics() {
        let mut failed = valid();
        failed.final_validate_status = FinalValidateStatus::Failed;
        failed.exit_code = 1;
        failed.passed_tests = Some(2128);
        failed.validate().unwrap();

        let mut failed_missing = failed.clone();
        failed_missing.passed_tests = None;
        assert!(
            failed_missing
                .validate()
                .unwrap_err()
                .contains("current result with executed_tests requires an exact count")
        );

        let mut no_result = valid();
        no_result.final_validate_status = FinalValidateStatus::CouldNotRun;
        no_result.exit_code = 75;
        no_result.executed_tests = None;
        no_result.passed_tests = None;
        no_result.could_not_run_kind = Some(CouldNotRunKind::NoResult);
        no_result.detail = Some(vec!["test framework produced no result".into()]);
        no_result.validate().unwrap();
    }

    #[test]
    fn checked_schema_projection_matches_the_shared_type() {
        let schema: Value =
            serde_json::from_str(include_str!("../validation-service-result-schema.json")).unwrap();
        let fields: Vec<&str> = schema["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(fields, FIELD_NAMES);
        let outcomes = schema["outcomes"].as_object().unwrap();
        let declared: BTreeSet<&str> = outcomes.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = FinalValidateStatus::ALL
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(declared, expected);
        for status in FinalValidateStatus::ALL {
            let row = &outcomes[status.as_str()];
            assert_eq!(row["validation_exit_code"], status.exit_code());
            assert_eq!(row["state"], status.service_state());
            assert_eq!(row["result"], status.service_result());
        }
        let writeback = schema["scorecard_writeback"].as_object().unwrap();
        assert_eq!(writeback["nullable"], true);
        let variants = writeback["variants"].as_object().unwrap();
        let declared: BTreeSet<&str> = variants.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = ScorecardWriteback::ALL.into_iter().collect();
        assert_eq!(declared, expected);
        for status in ScorecardWriteback::ALL {
            let fields: Vec<&str> = variants[status]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(fields, ScorecardWriteback::field_names(status));
        }
        let cause = schema["could_not_run_kind"].as_object().unwrap();
        assert_eq!(cause["nullable"], true);
        assert_eq!(cause["required_for"], "COULD_NOT_RUN");
        let variants: Vec<&str> = cause["variants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            variants,
            CouldNotRunKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
        );
    }
}
