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
use crate::runner::FailureClass;
use crate::runner::ObservedResult;

pub const STRESS_SERIES_SCHEMA_V1: &str = "stress-series/v1";
pub const STRESS_SERIES_SCHEMA_V2: &str = "stress-series/v2";
pub const STRESS_SERIES_SCHEMA_V3: &str = "stress-series/v3";
// Frozen when introduced in v2 and retained by v3: extending the machine
// vocabulary must not retroactively make already-written rows unreadable. A
// new capability therefore requires a new stress-series schema before
// producers may emit it.
const STRESS_SERIES_V2_HOST_CAPABILITIES: [HostCapability; 2] =
    [HostCapability::CpuidFaulting, HostCapability::Kvm];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SeriesSchema {
    #[serde(rename = "stress-series/v1")]
    V1,
    #[serde(rename = "stress-series/v2")]
    V2,
    #[serde(rename = "stress-series/v3")]
    V3,
}

impl SeriesSchema {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => STRESS_SERIES_SCHEMA_V1,
            Self::V2 => STRESS_SERIES_SCHEMA_V2,
            Self::V3 => STRESS_SERIES_SCHEMA_V3,
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

/// Producer-owned evidence for one inner invocation that did not produce a
/// canonical comparison. The containing series row identifies the outer cell
/// attempt; this retains the typed process disposition used to distinguish a
/// timeout from an unavailable result without inferring from duration or
/// backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesNoVerdictKind {
    NotRun,
    FirstRunRejected,
    InfrastructureError,
    MissingReportTimeout,
    NoncanonicalMatch,
    NoncanonicalDivergence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesAttemptDisposition {
    pub index: String,
    pub kind: SeriesNoVerdictKind,
    pub attempt_outcome: String,
    pub disposition: SeriesOutcome,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub verification_report_sha256: Option<String>,
}

/// Exact source-row identity and its non-comparison dispositions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesNoVerdictEvidence {
    pub evidence_sha256: String,
    pub attempts: Vec<SeriesAttemptDisposition>,
}

fn is_prelaunch_timeout_disposition(disposition: &SeriesAttemptDisposition) -> bool {
    disposition.timed_out
        && disposition.status.is_none()
        && disposition.signal.is_none()
        && matches!(
            disposition.error_kind.as_deref(),
            Some("incomplete-verification-evidence" | "cpu-timeout" | "wall-timeout")
        )
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
    /// The exact framework-written result. Required by `stress-series/v3`;
    /// absent only on retained v1/v2 rows.
    #[serde(default)]
    pub result: Option<ObservedResult>,
    /// Attribution for a non-pass. Required by `stress-series/v3` whenever the
    /// exact result is not `pass`; absent only for passes and retained v1/v2
    /// rows.
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    /// Present on newly emitted comparison-mode rows that produced no complete
    /// canonical product verdict. Historical v3 rows predate this optional
    /// evidence and remain readable, but consumers must not infer the missing
    /// facts from duration, backend, or exit status alone.
    #[serde(default)]
    pub no_verdict_evidence: Option<SeriesNoVerdictEvidence>,
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

    /// Validate a newly written row. Historical v1/v2 rows remain
    /// deserializable, but no new row may omit the framework's exact result and
    /// attribution.
    pub fn validate_for_write(&self) -> Result<(), String> {
        if self.schema != SeriesSchema::V3 {
            return Err(format!(
                "new rows must use {STRESS_SERIES_SCHEMA_V3}, got {}",
                self.schema.as_str()
            ));
        }
        self.validate_common()?;
        self.validate_host_facts()?;
        self.validate_classification()?;
        self.require_current_no_verdict_evidence()
    }

    /// Validate a stored row before a reader uses its contents.
    ///
    /// Retained v1/v2 rows remain readable. Every v2/v3 row must carry the
    /// complete host facts that its schema promises, and every v3 row must
    /// carry the exact result classification.
    pub fn validate_for_read(&self) -> Result<(), String> {
        self.validate_common()?;
        if matches!(self.schema, SeriesSchema::V2 | SeriesSchema::V3) {
            self.validate_host_facts()?;
        }
        if self.schema == SeriesSchema::V3 {
            self.validate_classification()?;
        }
        Ok(())
    }

    /// Validate a row before treating it as measurement evidence.
    ///
    /// Retained v1 rows still parse and can be reported, but they cannot safely
    /// compare runs across the same machine name after a kernel change.
    pub fn validate_for_projection(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.schema == SeriesSchema::V1 {
            return Err(format!(
                "{} does not record machine_shortname, kernel_version, and host_capabilities",
                self.schema.as_str()
            ));
        }
        self.validate_host_facts()?;
        if self.schema == SeriesSchema::V3 {
            self.validate_classification()?;
        }
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
        if let Some(evidence) = &self.series.no_verdict_evidence {
            self.validate_no_verdict_evidence(evidence)?;
        }
        Ok(())
    }

    fn validate_no_verdict_evidence(
        &self,
        evidence: &SeriesNoVerdictEvidence,
    ) -> Result<(), String> {
        if !is_sha256(&evidence.evidence_sha256) {
            return Err(format!(
                "no_verdict_evidence.evidence_sha256 must be lowercase 64-hex, got {:?}",
                evidence.evidence_sha256
            ));
        }
        if self.schema != SeriesSchema::V3 {
            return Err("no_verdict_evidence is supported only by stress-series/v3".into());
        }
        let attempt = self
            .series
            .attempt
            .ok_or("no_verdict_evidence requires an explicit outer attempt")?;
        let mode = self.series.cell.rsplit('/').nth(1).unwrap_or_default();
        if !matches!(mode, "verify" | "replay" | "chaos") {
            return Err("no_verdict_evidence is valid only for comparison modes".into());
        }
        if attempt == 0 || self.series.num_runs != 1 || self.series.last_run_index.is_some() {
            return Err(
                "no_verdict_evidence must identify one uncollapsed positive outer attempt".into(),
            );
        }
        if self.producer == SeriesProducer::Validate && attempt != self.series.run_index {
            return Err("validate no_verdict_evidence outer attempt must equal run_index".into());
        }
        if matches!(
            self.series.outcome,
            SeriesOutcome::Passed | SeriesOutcome::Diverged | SeriesOutcome::Skipped
        ) {
            return Err(format!(
                "outcome {} cannot carry no_verdict_evidence",
                self.series.outcome.as_str()
            ));
        }
        if evidence.attempts.is_empty() {
            return Err("no_verdict_evidence.attempts must be nonempty".into());
        }
        let mut indices = std::collections::BTreeSet::new();
        let mut saw_timeout = false;
        for disposition in &evidence.attempts {
            if disposition.index.trim().is_empty() || !indices.insert(&disposition.index) {
                return Err(
                    "no_verdict_evidence attempt indices must be nonempty and unique".into(),
                );
            }
            if disposition
                .error_kind
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err("no_verdict_evidence error_kind must be nonempty when present".into());
            }
            if disposition.status.is_some_and(|status| status < 0)
                || disposition.signal.is_some_and(|signal| signal <= 0)
                || (disposition.status.is_some() && disposition.signal.is_some())
            {
                return Err("no_verdict_evidence status/signal disposition is invalid".into());
            }
            if disposition
                .verification_report_sha256
                .as_ref()
                .is_some_and(|sha| !is_sha256(sha))
            {
                return Err(
                    "no_verdict_evidence verification_report_sha256 must be lowercase 64-hex"
                        .into(),
                );
            }
            let has_nonzero_process_disposition = matches!(
                (disposition.status, disposition.signal),
                (Some(status), None) if status > 0
            ) || matches!(
                (disposition.status, disposition.signal),
                (None, Some(signal)) if signal > 0
            );
            match disposition.kind {
                SeriesNoVerdictKind::NotRun => {
                    let expected = if disposition.timed_out {
                        SeriesOutcome::Timeout
                    } else {
                        SeriesOutcome::NoResult
                    };
                    let no_process_timeout = is_prelaunch_timeout_disposition(disposition);
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != expected
                        || disposition
                            .error_kind
                            .as_ref()
                            .is_none_or(|value| value.trim().is_empty())
                        || !(has_nonzero_process_disposition || no_process_timeout)
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "not_run evidence must carry attempt outcome ERROR, an error_kind, a verification report, either one nonzero process disposition or an explicit pre-launch timeout, and a matching typed disposition"
                                .into(),
                        );
                    }
                    saw_timeout |= disposition.timed_out;
                }
                SeriesNoVerdictKind::FirstRunRejected => {
                    if disposition.attempt_outcome != "FAIL"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition.error_kind.is_some()
                        || disposition.status.is_none_or(|status| status <= 0)
                        || disposition.signal.is_some()
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "first_run_rejected evidence must carry attempt outcome FAIL, no error_kind, a nonzero status without signal, timed_out=false, a verification report, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::NoncanonicalMatch => {
                    let status_is_valid = disposition
                        .status
                        .is_some_and(|status| status >= 0 && (mode == "chaos" || status == 0));
                    if disposition.attempt_outcome != "PASS"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition.error_kind.is_some()
                        || !status_is_valid
                        || disposition.signal.is_some()
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "noncanonical_match evidence must carry attempt outcome PASS, no error_kind, a valid status without signal, timed_out=false, a verification report, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::NoncanonicalDivergence => {
                    if disposition.attempt_outcome != "FAIL"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition.error_kind.is_some()
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "noncanonical_divergence evidence must carry attempt outcome FAIL, no error_kind, exactly one nonzero status or signal, timed_out=false, a verification report, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::InfrastructureError => {
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != SeriesOutcome::Errored
                        || disposition.timed_out
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "infrastructure_error evidence must carry attempt outcome ERROR, a verification report, exactly one nonzero status or signal, timed_out=false, and errored disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::MissingReportTimeout => {
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != SeriesOutcome::Timeout
                        || !disposition.timed_out
                        || disposition
                            .error_kind
                            .as_ref()
                            .is_none_or(|value| value.trim().is_empty())
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_some()
                    {
                        return Err(
                            "missing_report_timeout evidence must carry attempt outcome ERROR, an error_kind, no verification report, exactly one nonzero status or signal, timed_out=true, and timeout disposition"
                                .into(),
                        );
                    }
                    saw_timeout = true;
                }
            }
        }
        if self.series.result == Some(ObservedResult::Timeout) && !saw_timeout {
            return Err("timeout result has no timed-out attempt disposition".into());
        }
        if saw_timeout
            && self.series.result.is_some()
            && self.series.result != Some(ObservedResult::Timeout)
        {
            return Err(format!(
                "timed-out attempt disposition contradicts result {:?}",
                self.series.result
            ));
        }
        Ok(())
    }

    fn require_current_no_verdict_evidence(&self) -> Result<(), String> {
        let mode = self.series.cell.rsplit('/').nth(1).unwrap_or_default();
        let requires_evidence = matches!(
            self.series.outcome,
            SeriesOutcome::NoResult | SeriesOutcome::Timeout | SeriesOutcome::Errored
        );
        if matches!(mode, "verify" | "replay" | "chaos")
            && requires_evidence
            && self.series.no_verdict_evidence.is_none()
        {
            return Err(format!(
                "new comparison-mode {} row must carry no_verdict_evidence",
                self.series.outcome.as_str()
            ));
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

    fn validate_classification(&self) -> Result<(), String> {
        let valid = matches!(
            (
                self.series.outcome,
                self.series.result,
                self.series.failure_class,
            ),
            (SeriesOutcome::Passed, Some(ObservedResult::Pass), None)
                | (
                    SeriesOutcome::Diverged,
                    Some(
                        ObservedResult::DeterminismFailure
                            | ObservedResult::ParityFailure
                            | ObservedResult::ReplayFailure
                    ),
                    Some(FailureClass::ProductFailure)
                )
                | (
                    SeriesOutcome::Errored,
                    Some(ObservedResult::CrashError),
                    Some(FailureClass::ProductFailure)
                )
                | (
                    SeriesOutcome::Timeout,
                    Some(ObservedResult::Timeout),
                    Some(FailureClass::NoResult)
                )
                | (
                    SeriesOutcome::NoResult,
                    Some(ObservedResult::Oom),
                    Some(FailureClass::NoResult)
                )
                | (
                    SeriesOutcome::Errored,
                    None,
                    Some(FailureClass::UnderstoodInfrastructureFailure)
                )
                | (
                    SeriesOutcome::Skipped,
                    None,
                    Some(FailureClass::UnderstoodPrerequisiteFailure)
                )
                | (SeriesOutcome::NoResult, None, Some(FailureClass::NoResult))
        );
        if valid {
            Ok(())
        } else {
            Err(format!(
                "stress-series/v3 classification mismatch: outcome={} result={:?} failure_class={:?}",
                self.series.outcome.as_str(),
                self.series.result,
                self.series.failure_class
            ))
        }
    }
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
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
                result: Some(ObservedResult::Pass),
                failure_class: None,
                no_verdict_evidence: None,
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

    fn no_verdict_row() -> SeriesRow {
        let mut fixture = row(SeriesSchema::V3);
        fixture.series.outcome = SeriesOutcome::NoResult;
        fixture.series.result = None;
        fixture.series.failure_class = Some(FailureClass::NoResult);
        fixture.series.attempt = Some(1);
        fixture.series.no_verdict_evidence = Some(SeriesNoVerdictEvidence {
            evidence_sha256: "b".repeat(64),
            attempts: vec![SeriesAttemptDisposition {
                index: "1".into(),
                kind: SeriesNoVerdictKind::NotRun,
                attempt_outcome: "ERROR".into(),
                disposition: SeriesOutcome::NoResult,
                error_kind: Some("incomplete-verification-evidence".into()),
                status: Some(125),
                signal: None,
                timed_out: false,
                verification_report_sha256: Some("c".repeat(64)),
            }],
        });
        fixture
    }

    #[test]
    fn v3_requires_matching_machine_and_kernel() {
        let mut fixture = row(SeriesSchema::V3);
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
    fn v3_requires_every_capability_verdict_with_evidence() {
        let mut fixture = row(SeriesSchema::V3);
        fixture.series.host_capabilities = None;
        assert_eq!(
            fixture.validate_for_write().unwrap_err(),
            "series missing host_capabilities"
        );

        for capability in STRESS_SERIES_V2_HOST_CAPABILITIES {
            let mut fixture = row(SeriesSchema::V3);
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

        let mut fixture = row(SeriesSchema::V3);
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

        for schema in [SeriesSchema::V2, SeriesSchema::V3] {
            let mut current = row(schema);
            current.series.host_capabilities = None;
            assert_eq!(
                current.validate_for_read().unwrap_err(),
                "series missing host_capabilities"
            );

            let mut current = row(schema);
            current
                .series
                .host_capabilities
                .as_mut()
                .unwrap()
                .remove(&HostCapability::Kvm);
            assert!(current.validate_for_read().unwrap_err().contains("kvm"));
        }
    }

    #[test]
    fn non_diverged_rows_cannot_carry_divergence_evidence() {
        let mut fixture = row(SeriesSchema::V3);
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
        accepted.validate_for_read().unwrap();

        value["series"]["outcome"] = serde_json::json!("error");
        let error = serde_json::from_value::<SeriesRow>(value)
            .expect_err("schema-v2 must refuse the unsupported error spelling");
        assert!(error.to_string().contains("unknown variant `error`"));
    }

    #[test]
    fn v3_refuses_missing_or_mismatched_classification_by_name() {
        let mut missing_result = row(SeriesSchema::V3);
        missing_result.series.result = None;
        for error in [
            missing_result.validate_for_read().unwrap_err(),
            missing_result.validate_for_write().unwrap_err(),
        ] {
            assert!(error.contains("result=None"));
        }

        let mut missing_class = row(SeriesSchema::V3);
        missing_class.series.outcome = SeriesOutcome::Diverged;
        missing_class.series.result = Some(ObservedResult::DeterminismFailure);
        assert!(
            missing_class
                .validate_for_write()
                .unwrap_err()
                .contains("failure_class=None")
        );

        let mut wrong_result = row(SeriesSchema::V3);
        wrong_result.series.outcome = SeriesOutcome::Diverged;
        wrong_result.series.result = Some(ObservedResult::CrashError);
        wrong_result.series.failure_class = Some(FailureClass::ProductFailure);
        assert!(
            wrong_result
                .validate_for_write()
                .unwrap_err()
                .contains("result=Some(CrashError)")
        );

        let mut retained_v2 = row(SeriesSchema::V2);
        retained_v2.series.result = None;
        retained_v2.series.failure_class = None;
        retained_v2.validate_for_read().unwrap();
        retained_v2.validate_for_projection().unwrap();
        assert_eq!(
            retained_v2.validate_for_write().unwrap_err(),
            "new rows must use stress-series/v3, got stress-series/v2"
        );
    }

    #[test]
    fn current_no_verdict_rows_require_exact_uncollapsed_evidence() {
        let fixture = no_verdict_row();
        fixture.validate_for_read().unwrap();
        fixture.validate_for_write().unwrap();

        let mut legacy = fixture.clone();
        legacy.series.no_verdict_evidence = None;
        legacy.validate_for_read().unwrap();
        assert!(
            legacy
                .validate_for_write()
                .unwrap_err()
                .contains("must carry no_verdict_evidence")
        );

        let mut collapsed = fixture.clone();
        collapsed.series.num_runs = 2;
        collapsed.series.last_run_index = Some(2);
        assert!(
            collapsed
                .validate_for_read()
                .unwrap_err()
                .contains("one uncollapsed positive outer attempt")
        );

        let mut missing_attempt = fixture;
        missing_attempt.series.attempt = None;
        assert!(
            missing_attempt
                .validate_for_read()
                .unwrap_err()
                .contains("explicit outer attempt")
        );

        let mut mismatched_attempt = no_verdict_row();
        mismatched_attempt.series.attempt = Some(2);
        assert!(
            mismatched_attempt
                .validate_for_read()
                .unwrap_err()
                .contains("outer attempt must equal run_index")
        );

        let mut v2 = no_verdict_row();
        v2.schema = SeriesSchema::V2;
        assert!(
            v2.validate_for_read()
                .unwrap_err()
                .contains("supported only by stress-series/v3")
        );
    }

    #[test]
    fn no_verdict_timeout_disposition_is_typed_and_contradictions_refuse() {
        let mut timeout = no_verdict_row();
        timeout.series.outcome = SeriesOutcome::Timeout;
        timeout.series.result = Some(ObservedResult::Timeout);
        let disposition = &mut timeout
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        disposition.disposition = SeriesOutcome::Timeout;
        disposition.status = None;
        disposition.signal = Some(15);
        disposition.timed_out = true;
        timeout.validate_for_write().unwrap();

        let mut missing_timeout = timeout.clone();
        missing_timeout
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .timed_out = false;
        assert!(
            missing_timeout
                .validate_for_read()
                .unwrap_err()
                .contains("not_run evidence must carry")
        );

        let mut invented_result = no_verdict_row();
        invented_result.series.result = Some(ObservedResult::CrashError);
        assert!(
            invented_result
                .validate_for_read()
                .unwrap_err()
                .contains("result=Some(CrashError)")
        );

        for status in [Some(0), None] {
            let mut incomplete = no_verdict_row();
            let disposition = &mut incomplete
                .series
                .no_verdict_evidence
                .as_mut()
                .unwrap()
                .attempts[0];
            disposition.status = status;
            disposition.signal = None;
            assert!(incomplete.validate_for_read().unwrap_err().contains(
                "either one nonzero process disposition or an explicit pre-launch timeout"
            ));
        }

        let mut prelaunch_timeout = no_verdict_row();
        let disposition = &mut prelaunch_timeout
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        disposition.disposition = SeriesOutcome::Timeout;
        disposition.status = None;
        disposition.signal = None;
        disposition.timed_out = true;
        for error_kind in [
            "incomplete-verification-evidence",
            "cpu-timeout",
            "wall-timeout",
        ] {
            let mut typed = prelaunch_timeout.clone();
            typed.series.no_verdict_evidence.as_mut().unwrap().attempts[0].error_kind =
                Some(error_kind.into());
            typed.validate_for_write().unwrap_or_else(|error| {
                panic!("typed pre-launch timeout {error_kind} was refused: {error}")
            });
        }

        let mut wrong_prelaunch_kind = prelaunch_timeout;
        wrong_prelaunch_kind
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .error_kind = Some("infrastructure".into());
        assert!(
            wrong_prelaunch_kind
                .validate_for_write()
                .unwrap_err()
                .contains("explicit pre-launch timeout")
        );

        let mut historical_errored = row(SeriesSchema::V3);
        historical_errored.series.outcome = SeriesOutcome::Errored;
        historical_errored.series.result = Some(ObservedResult::CrashError);
        historical_errored.series.failure_class = Some(FailureClass::ProductFailure);
        historical_errored.validate_for_read().unwrap();
        assert!(
            historical_errored
                .validate_for_write()
                .unwrap_err()
                .contains("must carry no_verdict_evidence")
        );

        historical_errored.series.attempt = Some(1);
        historical_errored.series.no_verdict_evidence = Some(SeriesNoVerdictEvidence {
            evidence_sha256: "e".repeat(64),
            attempts: vec![SeriesAttemptDisposition {
                index: "1".into(),
                kind: SeriesNoVerdictKind::FirstRunRejected,
                attempt_outcome: "FAIL".into(),
                disposition: SeriesOutcome::NoResult,
                error_kind: None,
                status: Some(125),
                signal: None,
                timed_out: false,
                verification_report_sha256: Some("f".repeat(64)),
            }],
        });
        historical_errored.validate_for_write().unwrap();

        let mut noncanonical = no_verdict_row();
        let disposition = &mut noncanonical
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        disposition.kind = SeriesNoVerdictKind::NoncanonicalMatch;
        disposition.attempt_outcome = "PASS".into();
        disposition.error_kind = None;
        disposition.status = Some(0);
        noncanonical.validate_for_write().unwrap();

        let mut noncanonical_nonzero = noncanonical;
        noncanonical_nonzero
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .status = Some(1);
        assert!(
            noncanonical_nonzero
                .validate_for_read()
                .unwrap_err()
                .contains("noncanonical_match evidence")
        );
    }
}
