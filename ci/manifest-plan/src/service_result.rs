//! Serialized terminal result written by the validation framework.

use serde::Deserialize;
use serde::Serialize;

pub const SCHEMA_VERSION: u64 = 1;
pub const FIELD_NAMES: [&str; 7] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
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
}

impl ValidationServiceResult {
    pub fn new(
        commit: String,
        profile: String,
        final_validate_status: FinalValidateStatus,
        exit_code: i32,
        executed_nodes: u64,
        executed_tests: Option<i64>,
    ) -> Result<Self, String> {
        let result = Self {
            schema_version: SCHEMA_VERSION,
            commit,
            profile,
            final_validate_status,
            exit_code,
            executed_nodes,
            executed_tests,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, String> {
        let result: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("validation-service-result-shape: {error}"))?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "validation-service-result-schema_version: expected {SCHEMA_VERSION}, got {}",
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
        let expected = self.final_validate_status.exit_code();
        if self.exit_code != expected {
            return Err(format!(
                "validation-service-result-exit_code: {} requires {expected}, got {}",
                self.final_validate_status.as_str(),
                self.exit_code
            ));
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
        assert!(error.contains("validation-service-result-shape"));
        assert!(error.contains("future"));
    }

    #[test]
    fn status_and_exit_must_agree() {
        let mut result = valid();
        result.exit_code = 1;
        assert_eq!(
            result.validate().unwrap_err(),
            "validation-service-result-exit_code: PASSED requires 0, got 1"
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
            assert_eq!(row["exit_code"], status.exit_code());
            assert_eq!(row["state"], status.service_state());
            assert_eq!(row["result"], status.service_result());
        }
    }
}
