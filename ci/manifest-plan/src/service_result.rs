//! Serialized terminal result written by the validation framework.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const HISTORICAL_SCHEMA_VERSION: u64 = 1;
pub const SCHEMA_VERSION: u64 = 2;
pub const HISTORICAL_FIELD_NAMES: [&str; 7] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
];
pub const FIELD_NAMES: [&str; 8] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
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

/// Outcome of publishing the already-final validation evidence into the
/// scorecard. This is bookkeeping about a completed validation, not the
/// validation verdict itself.
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
    pub final_validate_status: FinalValidateStatus,
    pub exit_code: i32,
    pub executed_nodes: u64,
    pub executed_tests: Option<i64>,
    /// Kept separate from `final_validate_status`: a write-back failure must
    /// make the command fail loudly without rewriting a real tree verdict.
    #[serde(default)]
    pub scorecard_writeback: Option<ScorecardWriteback>,
}

impl ValidationServiceResult {
    pub fn new(
        commit: String,
        profile: String,
        final_validate_status: FinalValidateStatus,
        exit_code: i32,
        executed_nodes: u64,
        executed_tests: Option<i64>,
        scorecard_writeback: Option<ScorecardWriteback>,
    ) -> Result<Self, String> {
        let result = Self {
            schema_version: SCHEMA_VERSION,
            commit,
            profile,
            final_validate_status,
            exit_code,
            executed_nodes,
            executed_tests,
            scorecard_writeback,
        };
        result.validate()?;
        Ok(result)
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
            SCHEMA_VERSION => FIELD_NAMES.into_iter().collect(),
            other => {
                return Err(format!(
                    "validation-service-result-schema_version: expected {HISTORICAL_SCHEMA_VERSION} or {SCHEMA_VERSION}, got {other}"
                ));
            }
        };
        let actual_fields: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        if actual_fields != expected_fields {
            return Err(format!(
                "validation-service-result-fields: schema {schema_version} expected {expected_fields:?}, got {actual_fields:?}"
            ));
        }
        let result: Self = serde_json::from_value(value)
            .map_err(|error| format!("validation-service-result-shape: {error}"))?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if ![HISTORICAL_SCHEMA_VERSION, SCHEMA_VERSION].contains(&self.schema_version) {
            return Err(format!(
                "validation-service-result-schema_version: expected {HISTORICAL_SCHEMA_VERSION} or {SCHEMA_VERSION}, got {}",
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
        let validation_exit = self.final_validate_status.exit_code();
        if self.schema_version == HISTORICAL_SCHEMA_VERSION {
            if self.scorecard_writeback.is_some() {
                return Err(
                    "validation-service-result-scorecard_writeback: schema 1 cannot carry this field"
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    fn valid() -> ValidationServiceResult {
        ValidationServiceResult::new(
            "a".repeat(40),
            "full".into(),
            FinalValidateStatus::Passed,
            0,
            76,
            Some(2129),
            None,
        )
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
    fn historical_schema_is_readable_only_in_its_exact_old_shape() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(HISTORICAL_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("scorecard_writeback");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, HISTORICAL_SCHEMA_VERSION);
        assert_eq!(decoded.scorecard_writeback, None);

        value["scorecard_writeback"] = Value::Null;
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 1 expected")
        );
    }

    #[test]
    fn current_schema_refuses_a_missing_writeback_field() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("scorecard_writeback");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 2 expected")
        );
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
    }
}
