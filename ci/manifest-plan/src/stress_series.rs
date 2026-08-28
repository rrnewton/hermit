//! Shared serialized type for `stress-series` rows.
//!
//! Hermit owns the artifact consumed by the compatibility scorecard. The
//! parent writer imports this module and checks every proposed row against the
//! same type before appending it, so the Python projection is not a second
//! schema authority.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

pub use crate::host_capability::CapabilityVerdict as HostCapabilityVerdict;
pub use crate::host_capability::HostCapabilities;
pub use crate::host_capability::HostCapability;

pub const STRESS_SERIES_SCHEMA_V1: &str = "stress-series/v1";
pub const STRESS_SERIES_SCHEMA_V2: &str = "stress-series/v2";
// Frozen with v2: extending the machine vocabulary must not retroactively make
// already-written v2 rows unreadable. A new capability therefore requires a
// new stress-series schema before producers may emit it.
const STRESS_SERIES_V2_HOST_CAPABILITIES: [HostCapability; 2] =
    [HostCapability::CpuidFaulting, HostCapability::Kvm];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SeriesSchema {
    #[serde(rename = "stress-series/v1")]
    V1,
    #[serde(rename = "stress-series/v2")]
    V2,
}

impl SeriesSchema {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => STRESS_SERIES_SCHEMA_V1,
            Self::V2 => STRESS_SERIES_SCHEMA_V2,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SeriesProducer {
    #[serde(rename = "validate")]
    Validate,
    #[serde(rename = "pressure-test")]
    PressureTest,
    #[serde(rename = "hermit-repeat")]
    HermitRepeat,
}

impl SeriesProducer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::PressureTest => "pressure-test",
            Self::HermitRepeat => "hermit-repeat",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesOutcome {
    Passed,
    Diverged,
    NoResult,
    Timeout,
    Errored,
    Skipped,
}

impl SeriesOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Diverged => "diverged",
            Self::NoResult => "no_result",
            Self::Timeout => "timeout",
            Self::Errored => "errored",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDepth {
    pub commits: u64,
    pub first_parent: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesCoordinates {
    #[serde(default)]
    pub first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    #[serde(default)]
    pub first_divergent_record: Option<u64>,
    #[serde(default)]
    pub first_divergent_syscall: Option<u64>,
}

impl SeriesCoordinates {
    fn has_position(&self) -> bool {
        self.first_divergent_scheduler_turn.is_some()
            || self.first_divergent_virtual_nanoseconds.is_some()
            || self.first_divergent_record.is_some()
            || self.first_divergent_syscall.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FirstDivergentMessages {
    #[serde(deserialize_with = "deserialize_nullable_string")]
    pub left: Option<String>,
    #[serde(deserialize_with = "deserialize_nullable_string")]
    pub right: Option<String>,
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesRuntimeMeasurement {
    #[serde(default)]
    pub scheduler_turns: Option<u64>,
    #[serde(default)]
    pub virtual_nanoseconds: Option<u64>,
    #[serde(default)]
    pub syscalls: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesRuntime {
    #[serde(default)]
    pub run1: Option<SeriesRuntimeMeasurement>,
    #[serde(default)]
    pub run2: Option<SeriesRuntimeMeasurement>,
    #[serde(default)]
    pub wall_time_min_ms: Option<f64>,
    #[serde(default)]
    pub wall_time_max_ms: Option<f64>,
}

fn one_run() -> u64 {
    1
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesPayload {
    pub cell: String,
    pub tree: String,
    #[serde(default)]
    pub detcore_tree: Option<String>,
    pub outcome: SeriesOutcome,
    pub run_index: u64,
    #[serde(default)]
    pub attempt: Option<u64>,
    #[serde(default = "one_run")]
    pub num_runs: u64,
    #[serde(default)]
    pub last_run_index: Option<u64>,
    #[serde(default)]
    pub main_ancestry: Option<bool>,
    #[serde(default)]
    pub runtime: Option<SeriesRuntime>,
    #[serde(default)]
    pub source_tree_dirty: bool,
    #[serde(default)]
    pub depth: BTreeMap<String, SourceDepth>,
    #[serde(default)]
    pub coordinates: Option<SeriesCoordinates>,
    #[serde(default)]
    pub first_divergent_messages: Option<FirstDivergentMessages>,
    /// Required by `stress-series/v2`. This is the measurement authority; the
    /// parent shard path is only a storage index derived from the envelope host.
    /// Absent only on retained v1 rows.
    #[serde(default)]
    pub machine_shortname: Option<String>,
    /// Required by `stress-series/v2`. Absent only on retained v1 rows.
    #[serde(default)]
    pub kernel_version: Option<String>,
    /// Required by `stress-series/v2`. This is the complete set of capability
    /// verdicts that determined which population could execute. It is not
    /// derivable from the machine name or kernel version.
    #[serde(default)]
    pub host_capabilities: Option<HostCapabilities>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesRow {
    /// Filled by readers after deserialization; never part of the artifact.
    #[serde(skip)]
    pub source: String,
    pub schema: SeriesSchema,
    pub event_id: String,
    pub event_type: String,
    pub emitted_at: String,
    pub team: String,
    pub host: String,
    pub producer: SeriesProducer,
    pub run_id: String,
    pub series: SeriesPayload,
}

impl SeriesRow {
    pub fn cell(&self) -> &str {
        &self.series.cell
    }

    pub fn label(&self) -> String {
        format!(
            "{}:{}: {} from {} run {} repetition {} @ {}",
            self.source,
            self.event_id,
            self.series.cell,
            self.producer.as_str(),
            self.run_id,
            self.series.run_index,
            self.series.tree
        )
    }

    /// Validate a newly written row. Historical v1 rows remain deserializable,
    /// but no new row may omit the machine and kernel that bound its result.
    pub fn validate_for_write(&self) -> Result<(), String> {
        if self.schema != SeriesSchema::V2 {
            return Err(format!(
                "new rows must use {STRESS_SERIES_SCHEMA_V2}, got {}",
                self.schema.as_str()
            ));
        }
        self.validate_common()?;
        self.validate_host_facts()
    }

    /// Validate a stored row before a reader uses its contents.
    ///
    /// Retained v1 rows remain readable. Every v2 row must carry the complete
    /// host facts that its schema promises, even when the reader does not
    /// project the row into the scorecard.
    pub fn validate_for_read(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.schema == SeriesSchema::V2 {
            self.validate_host_facts()?;
        }
        Ok(())
    }

    /// Validate a row before treating it as measurement evidence.
    ///
    /// Retained v1 rows still parse and can be reported, but they cannot safely
    /// compare runs across the same machine name after a kernel change.
    pub fn validate_for_projection(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.schema != SeriesSchema::V2 {
            return Err(format!(
                "{} does not record machine_shortname, kernel_version, and host_capabilities",
                self.schema.as_str()
            ));
        }
        self.validate_host_facts()?;
        if self.series.source_tree_dirty {
            return Err(
                "source_tree_dirty is true; dirty source is not checked-in evidence".into(),
            );
        }
        Ok(())
    }

    fn validate_common(&self) -> Result<(), String> {
        for (name, value) in [
            ("event_id", self.event_id.as_str()),
            ("emitted_at", self.emitted_at.as_str()),
            ("host", self.host.as_str()),
            ("run_id", self.run_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must be nonempty"));
            }
        }
        if self.event_type != "series.observation" {
            return Err(format!(
                "event_type must be series.observation, got {:?}",
                self.event_type
            ));
        }
        if self.team != "hermit" {
            return Err(format!("team must be hermit, got {:?}", self.team));
        }
        if !valid_cell(&self.series.cell) {
            return Err(format!(
                "cell must be '<test>/<mode>/<backend>' with the test's own slashes allowed, got {:?}",
                self.series.cell
            ));
        }
        if !is_object_id(&self.series.tree) {
            return Err(format!(
                "tree must be a 40-hex commit sha, got {:?}",
                self.series.tree
            ));
        }
        if let Some(tree) = &self.series.detcore_tree {
            if !is_object_id(tree) {
                return Err(format!(
                    "detcore_tree must be a 40-hex object id when present, got {tree:?}"
                ));
            }
        }
        if self.series.attempt == Some(0) {
            return Err("attempt must be a positive int when present".into());
        }
        if self.series.num_runs == 0 {
            return Err("num_runs must be a positive int".into());
        }
        if let Some(last) = self.series.last_run_index {
            if last < self.series.run_index {
                return Err("last_run_index must be at least run_index".into());
            }
            if last - self.series.run_index + 1 < self.series.num_runs {
                return Err(
                    "last_run_index span must contain at least num_runs observations".into(),
                );
            }
        }
        if let Some(runtime) = &self.series.runtime {
            for (name, value) in [
                ("wall_time_min_ms", runtime.wall_time_min_ms),
                ("wall_time_max_ms", runtime.wall_time_max_ms),
            ] {
                if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
                    return Err(format!(
                        "runtime {name} must be a non-negative number or null"
                    ));
                }
            }
            if let (Some(minimum), Some(maximum)) =
                (runtime.wall_time_min_ms, runtime.wall_time_max_ms)
            {
                if minimum > maximum {
                    return Err("runtime wall_time_min_ms must not exceed wall_time_max_ms".into());
                }
            }
        }
        for (repository, depth) in &self.series.depth {
            if repository.is_empty() {
                return Err("depth repository keys must be nonempty strings".into());
            }
            if depth.commits == 0 || depth.first_parent == 0 {
                return Err(format!(
                    "depth {repository}.commits and first_parent must be positive"
                ));
            }
        }
        if self.series.outcome != SeriesOutcome::Diverged
            && (self
                .series
                .coordinates
                .as_ref()
                .is_some_and(SeriesCoordinates::has_position)
                || self.series.first_divergent_messages.is_some())
        {
            return Err(format!(
                "outcome {:?} must not carry divergence evidence; only diverged may",
                self.series.outcome.as_str()
            ));
        }
        if let Some(messages) = &self.series.first_divergent_messages {
            for (side, value) in [("left", &messages.left), ("right", &messages.right)] {
                if value.as_ref().is_some_and(|value| value.is_empty()) {
                    return Err(format!(
                        "first_divergent_messages.{side} must be a nonempty string or null"
                    ));
                }
            }
            if messages.left.is_none() && messages.right.is_none() {
                return Err("first_divergent_messages must contain at least one message".into());
            }
        }
        Ok(())
    }

    fn validate_host_facts(&self) -> Result<(), String> {
        let machine = self
            .series
            .machine_shortname
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or("series missing machine_shortname")?;
        if machine.contains('/') || machine.contains('.') {
            return Err(format!(
                "machine_shortname must be a short hostname, got {machine:?}"
            ));
        }
        if self.host != machine {
            return Err(format!(
                "envelope host {:?} does not match machine_shortname {machine:?}",
                self.host
            ));
        }
        self.series
            .kernel_version
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "series missing kernel_version".to_string())?;
        let capabilities = self
            .series
            .host_capabilities
            .as_ref()
            .ok_or_else(|| "series missing host_capabilities".to_string())?;
        for capability in STRESS_SERIES_V2_HOST_CAPABILITIES {
            let verdict = capabilities.get(&capability).ok_or_else(|| {
                format!(
                    "host_capabilities missing required capability {:?}",
                    capability.value()
                )
            })?;
            if verdict.evidence.trim().is_empty() {
                return Err(format!(
                    "host_capabilities.{:?}.evidence must be nonempty",
                    capability.value()
                ));
            }
        }
        if capabilities.len() != STRESS_SERIES_V2_HOST_CAPABILITIES.len() {
            return Err("host_capabilities must contain the complete closed capability set".into());
        }
        Ok(())
    }
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_cell(value: &str) -> bool {
    let mut parts = value.rsplitn(3, '/');
    let backend = parts.next().unwrap_or_default();
    let mode = parts.next().unwrap_or_default();
    let test = parts.next().unwrap_or_default();
    !test.is_empty()
        && !mode.is_empty()
        && !backend.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(schema: SeriesSchema) -> SeriesRow {
        SeriesRow {
            source: String::new(),
            schema,
            event_id: "event".into(),
            event_type: "series.observation".into(),
            emitted_at: "2026-08-27T00:00:00Z".into(),
            team: "hermit".into(),
            host: "fixture-host".into(),
            producer: SeriesProducer::Validate,
            run_id: "run".into(),
            series: SeriesPayload {
                cell: "fixture/test/verify/ptrace".into(),
                tree: "a".repeat(40),
                detcore_tree: None,
                outcome: SeriesOutcome::Passed,
                run_index: 1,
                attempt: None,
                num_runs: 1,
                last_run_index: None,
                main_ancestry: Some(true),
                runtime: None,
                source_tree_dirty: false,
                depth: BTreeMap::new(),
                coordinates: None,
                first_divergent_messages: None,
                machine_shortname: Some("fixture-host".into()),
                kernel_version: Some("7.1.3-test".into()),
                host_capabilities: Some(BTreeMap::from([
                    (
                        HostCapability::CpuidFaulting,
                        HostCapabilityVerdict {
                            present: true,
                            evidence: "fixture cpuid probe".into(),
                        },
                    ),
                    (
                        HostCapability::Kvm,
                        HostCapabilityVerdict {
                            present: false,
                            evidence: "fixture kvm probe".into(),
                        },
                    ),
                ])),
            },
        }
    }

    #[test]
    fn v2_requires_matching_machine_and_kernel() {
        let mut fixture = row(SeriesSchema::V2);
        fixture.validate_for_write().unwrap();
        fixture.series.kernel_version = None;
        assert_eq!(
            fixture.validate_for_write().unwrap_err(),
            "series missing kernel_version"
        );
        fixture.series.kernel_version = Some("7.1.3-test".into());
        fixture.series.machine_shortname = Some("other-host".into());
        assert!(
            fixture
                .validate_for_write()
                .unwrap_err()
                .contains("does not match machine_shortname")
        );
    }

    #[test]
    fn v1_is_readable_but_not_new_measurement_evidence() {
        let mut fixture = row(SeriesSchema::V1);
        fixture.series.machine_shortname = None;
        fixture.series.kernel_version = None;
        fixture.series.host_capabilities = None;
        let encoded = serde_json::to_string(&fixture).unwrap();
        let decoded: SeriesRow = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.schema, SeriesSchema::V1);
        assert!(decoded.validate_for_write().is_err());
        assert!(decoded.validate_for_projection().is_err());
    }

    #[test]
    fn v2_requires_every_capability_verdict_with_evidence() {
        let mut fixture = row(SeriesSchema::V2);
        fixture.series.host_capabilities = None;
        assert_eq!(
            fixture.validate_for_write().unwrap_err(),
            "series missing host_capabilities"
        );

        for capability in STRESS_SERIES_V2_HOST_CAPABILITIES {
            let mut fixture = row(SeriesSchema::V2);
            fixture
                .series
                .host_capabilities
                .as_mut()
                .unwrap()
                .remove(&capability);
            assert!(
                fixture
                    .validate_for_write()
                    .unwrap_err()
                    .contains(capability.value()),
                "missing {} was not named",
                capability.value()
            );
        }

        let mut fixture = row(SeriesSchema::V2);
        fixture
            .series
            .host_capabilities
            .as_mut()
            .unwrap()
            .get_mut(&HostCapability::Kvm)
            .unwrap()
            .evidence
            .clear();
        assert!(fixture.validate_for_write().unwrap_err().contains("kvm"));
    }

    #[test]
    fn read_validation_keeps_v1_readable_and_refuses_incomplete_v2_by_name() {
        let mut legacy = row(SeriesSchema::V1);
        legacy.series.machine_shortname = None;
        legacy.series.kernel_version = None;
        legacy.series.host_capabilities = None;
        legacy.validate_for_read().unwrap();

        let mut current = row(SeriesSchema::V2);
        current.series.host_capabilities = None;
        assert_eq!(
            current.validate_for_read().unwrap_err(),
            "series missing host_capabilities"
        );

        let mut current = row(SeriesSchema::V2);
        current
            .series
            .host_capabilities
            .as_mut()
            .unwrap()
            .remove(&HostCapability::Kvm);
        assert!(current.validate_for_read().unwrap_err().contains("kvm"));
    }

    #[test]
    fn non_diverged_rows_cannot_carry_divergence_evidence() {
        let mut fixture = row(SeriesSchema::V2);
        fixture.series.coordinates = Some(SeriesCoordinates {
            first_divergent_record: Some(9),
            ..SeriesCoordinates::default()
        });
        assert!(
            fixture
                .validate_for_write()
                .unwrap_err()
                .contains("must not carry divergence evidence")
        );
    }

    #[test]
    fn v2_refuses_error_and_accepts_errored() {
        let mut value = serde_json::to_value(row(SeriesSchema::V2)).unwrap();
        value["series"]["outcome"] = serde_json::json!("errored");
        let accepted: SeriesRow = serde_json::from_value(value.clone())
            .expect("errored remains a supported non-verdict outcome");
        accepted.validate_for_write().unwrap();

        value["series"]["outcome"] = serde_json::json!("error");
        let error = serde_json::from_value::<SeriesRow>(value)
            .expect_err("schema-v2 must refuse the unsupported error spelling");
        assert!(error.to_string().contains("unknown variant `error`"));
    }
}
