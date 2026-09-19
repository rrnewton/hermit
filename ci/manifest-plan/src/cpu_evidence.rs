//! Optional per-invocation CPU observations, independent of charged CPU and verdicts.
//!
//! Readers validate supplied evidence; current producers do not emit it yet.
//! A missing envelope is historical absence, never a measured zero.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;

use crate::ledger::CellIdentity;
use crate::ledger::RequiredNullable;

pub fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Elapsed {
    pub seconds: u64,
    pub nanoseconds: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CpuPoint {
    pub poll: u64,
    pub at: Elapsed,
    pub cpu_usec: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellCpuBinding {
    pub run_id: String,
    pub hermit_sha: String,
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: RequiredNullable<String>,
    pub outer_attempt: u64,
    pub run_index: RequiredNullable<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellCpuObservationsV1 {
    pub version: u64,
    pub binding: CellCpuBinding,
    pub invocations: Vec<InvocationCpuObservation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationCpuObservation {
    pub ordinal: u64,
    pub role: InvocationRole,
    pub command: CommandIdentity,
    pub launch: LaunchObservation,
    pub live: LiveCpuObservation,
    pub final_wait: FinalWaitObservation,
    pub termination: TerminationPath,
    pub returned_cpu_charge: ReturnedCpuCharge,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationRole {
    Preparation,
    Execution {
        attempt_index: String,
        backend: RequiredNullable<String>,
    },
    PtraceNormalization {
        execution_ordinal: u64,
    },
    ParityComparison {
        candidate_execution: u64,
        reference_execution: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandIdentity {
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(deserialize_with = "deserialize_env")]
    pub env_overrides: BTreeMap<String, String>,
}

// A direct typed reader must not silently overwrite duplicate environment keys.
// The schema10 raw reader separately rejects duplicates before Value buffering.
fn deserialize_env<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unique string environment overrides")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if values.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate environment key {key}"
                    )));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Visitor)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchObservation {
    NotStarted { reason: NotStartedReason },
    SpawnFailed { stage: SpawnStage, reason: String },
    Spawned { pid: u32 },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotStartedReason {
    CpuBudgetAlreadyExhausted,
    WallBudgetAlreadyExhausted,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpawnStage {
    StdoutCapture,
    StderrCapture,
    Spawn,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveCpuObservation {
    Disabled,
    Enabled(Box<LiveCpuEnabled>),
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LiveCpuEnabled {
    pub source: LiveCpuSource,
    pub registration: RegistrationObservation,
    pub polls: u64,
    pub source_sample_calls: u64,
    pub valid_polls: u64,
    pub unavailable_polls: u64,
    pub first: RequiredNullable<CpuPoint>,
    pub last: RequiredNullable<CpuPoint>,
    pub high_water: RequiredNullable<CpuPoint>,
    pub timeout_trigger: RequiredNullable<CpuPoint>,
    pub last_error: RequiredNullable<CpuError>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveCpuSource {
    AgentUtilsPairedPidfdStatV1,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegistrationObservation {
    NotAttempted,
    BoundOnce,
    Unavailable { reason: String },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CpuError {
    pub stage: CpuErrorStage,
    pub reason: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CpuErrorStage {
    Registration,
    Sampling,
    Conversion,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawTimeval {
    pub seconds: i64,
    pub microseconds: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitCpuObservation {
    Measured {
        user_usec: u64,
        system_usec: u64,
        total_usec: u64,
    },
    Invalid {
        user: RawTimeval,
        system: RawTimeval,
        reason: String,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinalWaitObservation {
    NotApplicable,
    Reaped {
        source: FinalCpuSource,
        pid: u32,
        raw_status: i32,
        at: Elapsed,
        cpu: WaitCpuObservation,
    },
    Unavailable {
        operation: WaitOperation,
        errno: RequiredNullable<i32>,
        reason: String,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinalCpuSource {
    Wait4,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitOperation {
    Poll,
    StopGrace,
    BlockingStop,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminationPath {
    NotStarted,
    SpawnFailed,
    CompletedWait4,
    CpuBudgetStop,
    WallBudgetStop,
    AccountingUnavailableStop,
    WaitError,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReturnedCpuCharge {
    Value { cpu_usec: u64, basis: ChargeBasis },
    Unavailable,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChargeBasis {
    FinalWait4,
    MaxTriggerAndFinalWait4,
}

fn require(condition: bool, reason: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("invalid CPU observations: {reason}"))
    }
}
fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}
fn nullable<T>(value: &RequiredNullable<T>) -> Option<&T> {
    match value {
        RequiredNullable::Null => None,
        RequiredNullable::Value(value) => Some(value),
    }
}
fn valid_elapsed(value: &Elapsed) -> Result<(), String> {
    require(
        value.nanoseconds < 1_000_000_000,
        "elapsed nanoseconds out of range",
    )
}
fn raw_usec(value: &RawTimeval) -> Option<u64> {
    u64::try_from(value.seconds)
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(u64::try_from(value.microseconds).ok()?)
}

impl LiveCpuEnabled {
    fn validate(&self, launched: bool, final_at: Option<&Elapsed>) -> Result<(), String> {
        require(
            self.valid_polls.checked_add(self.unavailable_polls) == Some(self.polls),
            "poll counts differ",
        )?;
        require(
            self.source_sample_calls <= self.polls,
            "more reader calls than polls",
        )?;
        match &self.registration {
            RegistrationObservation::NotAttempted => require(
                !launched && self.polls == 0 && self.source_sample_calls == 0,
                "registration not attempted after launch",
            )?,
            RegistrationObservation::BoundOnce => require(
                launched && self.source_sample_calls == self.polls,
                "bound reader call count differs",
            )?,
            RegistrationObservation::Unavailable { reason } => {
                require(
                    launched
                        && nonempty(reason)
                        && self.source_sample_calls == 0
                        && self.valid_polls == 0,
                    "unavailable registration claims samples",
                )?;
                if let Some(error) = nullable(&self.last_error) {
                    require(
                        error.stage == CpuErrorStage::Registration,
                        "registration refusal has a different error stage",
                    )?;
                }
            }
        }
        require(
            nullable(&self.last_error).is_some() == (self.unavailable_polls > 0),
            "unavailable polls and error presence differ",
        )?;
        if let Some(error) = nullable(&self.last_error) {
            require(nonempty(&error.reason), "empty live error")?;
            if matches!(self.registration, RegistrationObservation::BoundOnce) {
                require(
                    error.stage != CpuErrorStage::Registration,
                    "bound reader claims registration error",
                )?;
            }
        }
        let (first, last, high) = (
            nullable(&self.first),
            nullable(&self.last),
            nullable(&self.high_water),
        );
        require(
            [first, last, high]
                .iter()
                .all(|point| point.is_some() == (self.valid_polls > 0)),
            "valid polls and point presence differ",
        )?;
        for point in [first, last, high, nullable(&self.timeout_trigger)]
            .into_iter()
            .flatten()
        {
            valid_elapsed(&point.at)?;
            require(
                point.poll > 0 && point.poll <= self.polls,
                "point poll is out of range",
            )?;
            if let Some(at) = final_at {
                require(point.at <= *at, "live point follows final wait")?;
            }
        }
        if let (Some(first), Some(last), Some(high)) = (first, last, high) {
            require(
                first.poll <= high.poll
                    && high.poll <= last.poll
                    && first.at <= high.at
                    && high.at <= last.at,
                "point order differs",
            )?;
            require(
                high.cpu_usec >= first.cpu_usec && high.cpu_usec >= last.cpu_usec,
                "high water is below an endpoint",
            )?;
            require(
                self.valid_polls <= last.poll - first.poll + 1,
                "valid poll count exceeds point interval",
            )?;
            for point in [last, high] {
                if point.poll == first.poll {
                    require(point == first, "same poll has different observations")?;
                }
                if point.poll == last.poll {
                    require(point == last, "same last poll has different observations")?;
                }
            }
            if self.valid_polls == 1 {
                require(
                    first == last && last == high,
                    "single valid poll differs across summaries",
                )?;
            }
        }
        if let Some(trigger) = nullable(&self.timeout_trigger) {
            require(
                Some(trigger) == last && trigger.poll == self.polls,
                "timeout trigger is not the final poll and last valid observation",
            )?;
        }
        Ok(())
    }
}

impl InvocationCpuObservation {
    fn validate(&self) -> Result<(), String> {
        require(
            self.ordinal > 0
                && self.command.argv.first().is_some_and(|p| nonempty(p))
                && nonempty(&self.command.cwd),
            "missing invocation identity/command",
        )?;
        let pid = match &self.launch {
            LaunchObservation::Spawned { pid } => {
                require(*pid > 0, "zero launched pid")?;
                Some(*pid)
            }
            LaunchObservation::NotStarted { .. } => {
                require(
                    self.termination == TerminationPath::NotStarted,
                    "not-started termination differs",
                )?;
                None
            }
            LaunchObservation::SpawnFailed { reason, .. } => {
                require(
                    nonempty(reason) && self.termination == TerminationPath::SpawnFailed,
                    "spawn failure termination differs",
                )?;
                None
            }
        };
        let mut final_at = None;
        let final_cpu = match &self.final_wait {
            FinalWaitObservation::NotApplicable => {
                require(pid.is_none(), "launched child has no final disposition")?;
                None
            }
            FinalWaitObservation::Unavailable { reason, .. } => {
                require(
                    pid.is_some() && nonempty(reason),
                    "unavailable wait without a child/reason",
                )?;
                None
            }
            FinalWaitObservation::Reaped {
                pid: waited,
                raw_status,
                at,
                cpu,
                ..
            } => {
                require(pid == Some(*waited), "waited pid differs from launched pid")?;
                require(
                    (0..=u16::MAX as i32).contains(raw_status)
                        && (libc::WIFEXITED(*raw_status) || libc::WIFSIGNALED(*raw_status)),
                    "reaped wait status is not terminal",
                )?;
                valid_elapsed(at)?;
                final_at = Some(at);
                match cpu {
                    WaitCpuObservation::Measured {
                        user_usec,
                        system_usec,
                        total_usec,
                    } => {
                        require(
                            user_usec.checked_add(*system_usec) == Some(*total_usec),
                            "wait CPU sum differs or overflows",
                        )?;
                        Some(*total_usec)
                    }
                    WaitCpuObservation::Invalid {
                        user,
                        system,
                        reason,
                    } => {
                        require(
                            nonempty(reason)
                                && raw_usec(user)
                                    .and_then(|u| raw_usec(system).and_then(|s| u.checked_add(s)))
                                    .is_none(),
                            "invalid wait CPU is actually representable or lacks reason",
                        )?;
                        None
                    }
                }
            }
        };
        let trigger = match &self.live {
            LiveCpuObservation::Disabled => None,
            LiveCpuObservation::Enabled(live) => {
                live.validate(pid.is_some(), final_at)?;
                nullable(&live.timeout_trigger)
            }
        };
        require(
            trigger.is_some() == (self.termination == TerminationPath::CpuBudgetStop),
            "CPU stop and trigger presence differ",
        )?;
        if pid.is_none() {
            require(
                matches!(self.final_wait, FinalWaitObservation::NotApplicable)
                    && matches!(self.returned_cpu_charge, ReturnedCpuCharge::Unavailable),
                "unstarted child claims CPU",
            )?;
            return Ok(());
        }
        require(
            !matches!(
                self.termination,
                TerminationPath::NotStarted | TerminationPath::SpawnFailed
            ),
            "started child marked unstarted",
        )?;
        if self.termination == TerminationPath::AccountingUnavailableStop {
            require(
                matches!(&self.live, LiveCpuObservation::Enabled(live) if live.unavailable_polls > 0),
                "accounting stop without an unavailable poll",
            )?;
        }
        let expected = match self.termination {
            TerminationPath::CompletedWait4 => Some((
                final_cpu.ok_or("completed wait has no valid CPU receipt")?,
                ChargeBasis::FinalWait4,
            )),
            TerminationPath::WallBudgetStop => final_cpu.map(|cpu| (cpu, ChargeBasis::FinalWait4)),
            TerminationPath::CpuBudgetStop => final_cpu.map(|cpu| {
                (
                    cpu.max(trigger.expect("validated trigger").cpu_usec),
                    ChargeBasis::MaxTriggerAndFinalWait4,
                )
            }),
            TerminationPath::WaitError => {
                require(final_cpu.is_none(), "wait error carries a valid receipt")?;
                None
            }
            TerminationPath::AccountingUnavailableStop
            | TerminationPath::NotStarted
            | TerminationPath::SpawnFailed => None,
        };
        require(
            match (&self.returned_cpu_charge, expected) {
                (ReturnedCpuCharge::Unavailable, None) => true,
                (ReturnedCpuCharge::Value { cpu_usec, basis }, Some((cpu, source))) => {
                    *cpu_usec == cpu && *basis == source
                }
                _ => false,
            },
            "returned charge differs from its actual branch operands",
        )
    }
}

impl CellCpuBinding {
    pub fn from_source_row(row: &Value) -> Result<Self, String> {
        let string = |name: &str| {
            row.get(name)
                .and_then(Value::as_str)
                .filter(|s| nonempty(s))
                .map(str::to_owned)
                .ok_or_else(|| format!("CPU source row lacks {name}"))
        };
        let backend = match row.get("backend") {
            Some(Value::Null) => RequiredNullable::Null,
            Some(Value::String(value)) if nonempty(value) => RequiredNullable::Value(value.clone()),
            _ => return Err("CPU source row lacks a valid backend".into()),
        };
        let run_index = match row.get("run_index") {
            None | Some(Value::Null) => RequiredNullable::Null,
            Some(value) => RequiredNullable::Value(
                value
                    .as_u64()
                    .ok_or("CPU source run_index is not an exact integer")?,
            ),
        };
        Ok(Self {
            run_id: string("run_id")?,
            hermit_sha: string("hermit_sha")?,
            lane: string("lane")?,
            category: string("category")?,
            test: string("test")?,
            mode: string("mode")?,
            backend,
            outer_attempt: row
                .get("attempt")
                .and_then(Value::as_u64)
                .ok_or("CPU source row lacks attempt")?,
            run_index,
        })
    }
}

impl CellCpuObservationsV1 {
    pub fn validate(&self) -> Result<(), String> {
        require(self.version == 1, "unknown observation version")?;
        let b = &self.binding;
        require(
            [
                &b.run_id,
                &b.hermit_sha,
                &b.lane,
                &b.category,
                &b.test,
                &b.mode,
            ]
            .iter()
            .all(|s| nonempty(s)),
            "empty cell binding",
        )?;
        require(
            b.outer_attempt > 0 && b.outer_attempt <= crate::runner::MAX_ATTEMPTS_PER_CELL,
            "outer attempt out of range",
        )?;
        if let Some(backend) = nullable(&b.backend) {
            require(nonempty(backend), "empty backend")?;
        }
        let mut executions = BTreeSet::new();
        let mut preparation = false;
        let mut normalizations = BTreeSet::new();
        let mut comparison = false;
        for (index, invocation) in self.invocations.iter().enumerate() {
            require(
                u64::try_from(index).ok().and_then(|n| n.checked_add(1))
                    == Some(invocation.ordinal),
                "invocation ordinals are not contiguous",
            )?;
            invocation.validate()?;
            let execution = |ordinal: u64| -> Result<&InvocationCpuObservation, String> {
                let item = usize::try_from(ordinal)
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .filter(|n| *n < index)
                    .and_then(|n| self.invocations.get(n))
                    .ok_or("CPU role reference is not an earlier invocation")?;
                require(
                    matches!(item.role, InvocationRole::Execution { .. }),
                    "CPU role does not reference execution",
                )?;
                Ok(item)
            };
            match &invocation.role {
                InvocationRole::Preparation => {
                    require(!preparation && index == 0, "repeated or late preparation")?;
                    preparation = true;
                }
                InvocationRole::Execution {
                    attempt_index,
                    backend,
                } => {
                    require(nonempty(attempt_index), "empty execution index")?;
                    let backend = nullable(backend).cloned();
                    require(
                        executions.insert((attempt_index.clone(), backend.clone())),
                        "repeated execution identity",
                    )?;
                    let expected = if attempt_index == "parity-reference" {
                        Some("ptrace")
                    } else {
                        nullable(&b.backend).map(String::as_str)
                    };
                    require(
                        backend.as_deref() == expected,
                        "execution backend differs from its role",
                    )?;
                }
                InvocationRole::PtraceNormalization { execution_ordinal } => {
                    let prior = execution(*execution_ordinal)?;
                    require(
                        b.mode == "verify"
                            && prior.termination == TerminationPath::CompletedWait4
                            && matches!(&prior.role, InvocationRole::Execution { backend, .. }
                                if nullable(backend).map(String::as_str) == Some("ptrace")),
                        "normalization does not reference a completed verify ptrace execution",
                    )?;
                    require(
                        normalizations.insert(*execution_ordinal),
                        "repeated normalization",
                    )?;
                }
                InvocationRole::ParityComparison {
                    candidate_execution,
                    reference_execution,
                } => {
                    require(
                        !comparison && candidate_execution != reference_execution,
                        "repeated or self comparison",
                    )?;
                    let candidate = execution(*candidate_execution)?;
                    let reference = execution(*reference_execution)?;
                    require(
                        b.mode == "verify"
                            && nullable(&b.backend).is_some_and(|backend| backend != "ptrace"),
                        "comparison is not a verify non-ptrace candidate",
                    )?;
                    require(
                        matches!(&candidate.role, InvocationRole::Execution{attempt_index,..} if attempt_index != "parity-reference")
                            && matches!(&reference.role, InvocationRole::Execution{attempt_index,..} if attempt_index == "parity-reference"),
                        "comparison roles are reversed or foreign",
                    )?;
                    comparison = true;
                }
            }
        }
        Ok(())
    }

    pub fn validate_binding(&self, binding: &CellCpuBinding) -> Result<(), String> {
        self.validate()?;
        require(
            self.binding == *binding,
            "source row and observation binding differ",
        )
    }

    pub fn validate_attempt(
        &self,
        index: &str,
        argv: &[String],
        cwd: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let invocation=self.invocations.iter().find(|item|matches!(&item.role,InvocationRole::Execution{attempt_index,..} if attempt_index == index)).ok_or("CPU observations omit a retained semantic attempt")?;
        require(
            invocation.command.argv == argv
                && invocation.command.cwd == cwd
                && invocation.command.env_overrides == *env,
            "CPU command differs from retained semantic attempt",
        )
    }
}

pub fn validate_cpu_observations_in_source_row(
    row: &Value,
) -> Result<Option<CellCpuObservationsV1>, String> {
    let Some(raw) = row.get("cpu_observations") else {
        return Ok(None);
    };
    let observations: CellCpuObservationsV1 = serde_json::from_value(raw.clone())
        .map_err(|e| format!("malformed CPU observations: {e}"))?;
    observations.validate_binding(&CellCpuBinding::from_source_row(row)?)?;
    for attempt in row
        .get("attempts")
        .and_then(Value::as_array)
        .ok_or("CPU source attempts is absent or not an array")?
    {
        let index = attempt
            .get("index")
            .and_then(Value::as_str)
            .ok_or("CPU source attempt lacks index")?;
        let argv: Vec<String> = serde_json::from_value(
            attempt
                .get("argv")
                .cloned()
                .ok_or("CPU source attempt lacks argv")?,
        )
        .map_err(|e| e.to_string())?;
        let cwd = attempt
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or("CPU source attempt lacks cwd")?;
        let env: BTreeMap<String, String> = serde_json::from_value(
            attempt
                .get("env")
                .cloned()
                .ok_or("CPU source attempt lacks env")?,
        )
        .map_err(|e| e.to_string())?;
        observations.validate_attempt(index, &argv, cwd, &env)?;
    }
    Ok(Some(observations))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellCpuHistoryV1 {
    pub version: u64,
    pub attempts: Vec<CpuAttemptHistory>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CpuAttemptHistory {
    Unrecorded {
        outer_attempt: u64,
    },
    Recorded {
        outer_attempt: u64,
        observations: Box<CellCpuObservationsV1>,
    },
}
impl CpuAttemptHistory {
    pub fn outer_attempt(&self) -> u64 {
        match self {
            Self::Unrecorded { outer_attempt } | Self::Recorded { outer_attempt, .. } => {
                *outer_attempt
            }
        }
    }
    pub fn observations(&self) -> Option<&CellCpuObservationsV1> {
        match self {
            Self::Unrecorded { .. } => None,
            Self::Recorded { observations, .. } => Some(observations),
        }
    }
}
impl CellCpuHistoryV1 {
    pub fn from_source_rows(rows: &[(u64, Value)]) -> Result<Option<Self>, String> {
        let mut attempts = Vec::new();
        let mut recorded = false;
        for (outer_attempt, row) in rows {
            if let Some(observations) = validate_cpu_observations_in_source_row(row)? {
                require(
                    observations.binding.outer_attempt == *outer_attempt,
                    "projected outer attempt differs",
                )?;
                attempts.push(CpuAttemptHistory::Recorded {
                    outer_attempt: *outer_attempt,
                    observations: Box::new(observations),
                });
                recorded = true;
            } else {
                attempts.push(CpuAttemptHistory::Unrecorded {
                    outer_attempt: *outer_attempt,
                });
            }
        }
        Ok(recorded.then_some(Self {
            version: 1,
            attempts,
        }))
    }
    pub fn validate_for_artifact(
        &self,
        run_id: &str,
        hermit_sha: &str,
        identity: &CellIdentity,
        selected_attempt: u64,
    ) -> Result<(), String> {
        require(self.version == 1, "unknown history version")?;
        require(
            !self.attempts.is_empty()
                && self.attempts.len() as u64 <= crate::runner::MAX_ATTEMPTS_PER_CELL,
            "history attempt population out of range",
        )?;
        require(
            selected_attempt > 0 && selected_attempt <= self.attempts.len() as u64,
            "selected attempt absent from CPU history",
        )?;
        let mut recorded = false;
        for (index, attempt) in self.attempts.iter().enumerate() {
            let ordinal = index as u64 + 1;
            require(
                attempt.outer_attempt() == ordinal,
                "history outer attempts are not contiguous",
            )?;
            if let Some(observations) = attempt.observations() {
                observations.validate()?;
                let b = &observations.binding;
                require(
                    b.outer_attempt == ordinal
                        && b.run_id == run_id
                        && b.hermit_sha == hermit_sha
                        && b.lane == identity.lane
                        && b.category == identity.category
                        && b.test == identity.test
                        && b.mode == identity.mode
                        && nullable(&b.backend) == Some(&identity.backend),
                    "artifact and CPU history binding differ",
                )?;
                recorded = true;
            }
        }
        require(recorded, "all-unrecorded history must be absent")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    /// Synthetic decoder input, not a native CPU measurement.
    pub(crate) fn source_row() -> Value {
        json!({"run_id":"run","hermit_sha":"a".repeat(40),"lane":"portable",
            "category":"fixture","test":"fixture/test","mode":"verify","backend":"ptrace","attempt":1,"attempts":[]})
    }

    /// Bind synthetic execution records to the fixture's actual semantic operands.
    pub(crate) fn envelope(row: &Value) -> Value {
        let mut invocations = Vec::new();
        if let Some(attempts) = row.get("attempts").and_then(Value::as_array) {
            for (i, attempt) in attempts.iter().enumerate() {
                invocations.push(json!({
                    "ordinal":i+1,"role":{"kind":"execution","attempt_index":attempt["index"],"backend":if attempt["index"]=="parity-reference" {json!("ptrace")} else {row["backend"].clone()}},
                    "command":{"argv":attempt["argv"],"cwd":attempt["cwd"],"env_overrides":attempt["env"]},
                    "launch":{"state":"spawned","pid":17},"live":{"state":"disabled"},
                    "final_wait":{"state":"reaped","source":"wait4","pid":17,"raw_status":0,"at":{"seconds":1,"nanoseconds":0},"cpu":{"state":"measured","user_usec":3,"system_usec":2,"total_usec":5}},
                    "termination":"completed_wait4","returned_cpu_charge":{"state":"value","cpu_usec":5,"basis":"final_wait4"}
                }));
            }
        }
        json!({"version":1,"binding":{"run_id":row["run_id"],"hermit_sha":row["hermit_sha"],"lane":row["lane"],"category":row["category"],"test":row["test"],"mode":row["mode"],"backend":row["backend"],"outer_attempt":row["attempt"],"run_index":row.get("run_index").cloned().unwrap_or(Value::Null)},"invocations":invocations})
    }

    fn executed_row() -> Value {
        let mut row = source_row();
        row["attempts"] =
            json!([{"index":"verify-1","argv":["native-fixture"],"cwd":"/fixture","env":{}}]);
        row["cpu_observations"] = envelope(&row);
        row
    }
    fn valid(row: &Value) -> bool {
        validate_cpu_observations_in_source_row(row).is_ok()
    }
    fn enabled() -> Value {
        json!({"state":"enabled","source":"agent_utils_paired_pidfd_stat_v1","registration":{"state":"bound_once"},"polls":2,"source_sample_calls":2,"valid_polls":2,"unavailable_polls":0,
            "first":{"poll":1,"at":{"seconds":0,"nanoseconds":100},"cpu_usec":9},
            "last":{"poll":2,"at":{"seconds":0,"nanoseconds":200},"cpu_usec":7},
            "high_water":{"poll":1,"at":{"seconds":0,"nanoseconds":100},"cpu_usec":9},"timeout_trigger":null,"last_error":null})
    }

    #[test]
    fn historical_absence_and_strict_present_source_binding() {
        assert!(valid(&source_row()));
        let row = executed_row();
        assert!(valid(&row));
        for field in [
            "run_id",
            "hermit_sha",
            "lane",
            "category",
            "test",
            "mode",
            "backend",
        ] {
            let mut bad = row.clone();
            bad["cpu_observations"]["binding"][field] = json!("foreign");
            assert!(!valid(&bad), "{field}");
        }
        for (pointer, value) in [
            ("/cpu_observations", Value::Null),
            ("/cpu_observations/version", json!(2)),
            ("/cpu_observations/version", json!(1.0)),
            ("/cpu_observations/binding/outer_attempt", json!(2)),
            ("/cpu_observations/binding/run_index", json!(0)),
            ("/cpu_observations/invocations/0/ordinal", json!(2)),
            (
                "/cpu_observations/invocations/0/command/argv",
                json!(["foreign"]),
            ),
        ] {
            let mut bad = row.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(!valid(&bad), "{pointer}");
        }
        let mut bad = row.clone();
        bad["cpu_observations"]["extra"] = json!(true);
        assert!(!valid(&bad));
        let mut missing = row.clone();
        missing["cpu_observations"]["binding"]
            .as_object_mut()
            .unwrap()
            .remove("run_index");
        assert!(!valid(&missing));
        let mut missing_attempts = row.clone();
        missing_attempts.as_object_mut().unwrap().remove("attempts");
        assert!(!valid(&missing_attempts));
        missing_attempts
            .as_object_mut()
            .unwrap()
            .remove("cpu_observations");
        assert!(valid(&missing_attempts)); // historical absence adds no source requirements
        let raw = serde_json::to_string(&row).unwrap().replace(
            "\"env_overrides\":{}",
            "\"env_overrides\":{\"A\":\"1\",\"A\":\"2\"}",
        );
        assert!(crate::ledger::read_schema10_source_result(raw.as_bytes()).is_err());
        let raw = serde_json::to_string(&row["cpu_observations"])
            .unwrap()
            .replace(
                "\"env_overrides\":{}",
                "\"env_overrides\":{\"A\":\"1\",\"A\":\"2\"}",
            );
        assert!(serde_json::from_str::<CellCpuObservationsV1>(&raw).is_err());
    }

    #[test]
    fn branch_charges_preserve_independent_live_and_final_values() {
        let mut row = executed_row();
        row["cpu_observations"]["invocations"][0]["live"] = enabled();
        assert!(valid(&row)); // completed wait charges final5, not live high-water9
        let mut wall = row.clone();
        wall["cpu_observations"]["invocations"][0]["termination"] = json!("wall_budget_stop");
        assert!(valid(&wall));
        for mut base in [row.clone(), wall] {
            base["cpu_observations"]["invocations"][0]["returned_cpu_charge"]["cpu_usec"] =
                json!(9);
            assert!(!valid(&base));
        }
        let i = &mut row["cpu_observations"]["invocations"][0];
        i["termination"] = json!("cpu_budget_stop");
        i["live"]["timeout_trigger"] = i["live"]["last"].clone();
        i["returned_cpu_charge"] =
            json!({"state":"value","cpu_usec":7,"basis":"max_trigger_and_final_wait4"});
        assert!(valid(&row));
        let mut later_unavailable = row.clone();
        let live = &mut later_unavailable["cpu_observations"]["invocations"][0]["live"];
        live["polls"] = json!(3);
        live["source_sample_calls"] = json!(3);
        live["unavailable_polls"] = json!(1);
        live["last_error"] = json!({"stage":"sampling","reason":"later unavailable poll"});
        assert!(!valid(&later_unavailable));
        row["cpu_observations"]["invocations"][0]["returned_cpu_charge"]["cpu_usec"] = json!(9);
        assert!(!valid(&row));
    }

    #[test]
    fn unavailable_zero_and_known_reap_are_distinct() {
        for status in [0, 256, libc::SIGTERM, libc::SIGABRT | 128] {
            let mut terminal = executed_row();
            terminal["cpu_observations"]["invocations"][0]["final_wait"]["raw_status"] =
                json!(status);
            assert!(valid(&terminal), "terminal status {status}");
        }
        for status in [127, (libc::SIGSTOP << 8) | 127, 65535, -1, 65536] {
            let mut nonterminal = executed_row();
            nonterminal["cpu_observations"]["invocations"][0]["final_wait"]["raw_status"] =
                json!(status);
            assert!(!valid(&nonterminal), "nonterminal status {status}");
        }
        let mut row = executed_row();
        let i = &mut row["cpu_observations"]["invocations"][0];
        i["live"] = json!({"state":"enabled","source":"agent_utils_paired_pidfd_stat_v1","registration":{"state":"unavailable","reason":"fixture refusal"},"polls":1,"source_sample_calls":0,"valid_polls":0,"unavailable_polls":1,"first":null,"last":null,"high_water":null,"timeout_trigger":null,"last_error":{"stage":"registration","reason":"fixture refusal"}});
        i["termination"] = json!("accounting_unavailable_stop");
        i["returned_cpu_charge"] = json!({"state":"unavailable"});
        assert!(valid(&row));
        // A real-shaped final receipt must survive the unavailable return charge.
        let mut bad = row.clone();
        bad["cpu_observations"]["invocations"][0]["returned_cpu_charge"] =
            json!({"state":"value","cpu_usec":5,"basis":"final_wait4"});
        assert!(!valid(&bad));
        let mut fast = executed_row();
        let i = &mut fast["cpu_observations"]["invocations"][0];
        i["live"] = row["cpu_observations"]["invocations"][0]["live"].clone();
        i["live"]["polls"] = json!(0);
        i["live"]["unavailable_polls"] = json!(0);
        i["live"]["last_error"] = Value::Null;
        assert!(valid(&fast));
        let mut zero = executed_row();
        let i = &mut zero["cpu_observations"]["invocations"][0];
        i["final_wait"]["cpu"] =
            json!({"state":"measured","user_usec":0,"system_usec":0,"total_usec":0});
        i["returned_cpu_charge"]["cpu_usec"] = json!(0);
        assert!(valid(&zero));
        let mut invalid = executed_row();
        let i = &mut invalid["cpu_observations"]["invocations"][0];
        i["termination"] = json!("wait_error");
        i["returned_cpu_charge"] = json!({"state":"unavailable"});
        i["final_wait"]["cpu"] = json!({"state":"invalid","user":{"seconds":-1,"microseconds":0},"system":{"seconds":0,"microseconds":0},"reason":"negative CPU"});
        assert!(valid(&invalid));
        invalid["cpu_observations"]["invocations"][0]["final_wait"]["cpu"]["user"]["seconds"] =
            json!(0);
        assert!(!valid(&invalid));
    }

    #[test]
    fn role_references_are_local_ordered_and_not_numeric_pid_identity() {
        let mut row = executed_row();
        row["backend"] = json!("kvm");
        row["attempts"][0]["index"] = json!("verify-1");
        row["attempts"].as_array_mut().unwrap().push(
            json!({"index":"parity-reference","argv":["reference"],"cwd":"/fixture","env":{}}),
        );
        row["cpu_observations"] = envelope(&row);
        assert!(valid(&row)); // descriptive PID17 may legitimately repeat
        let mut comparison = row["cpu_observations"]["invocations"][0].clone();
        comparison["ordinal"] = json!(3);
        comparison["role"] =
            json!({"kind":"parity_comparison","candidate_execution":1,"reference_execution":2});
        row["cpu_observations"]["invocations"]
            .as_array_mut()
            .unwrap()
            .push(comparison);
        assert!(valid(&row));
        for foreign in [0, 1, 3, 100] {
            let mut bad = row.clone();
            bad["cpu_observations"]["invocations"][2]["role"]["reference_execution"] =
                json!(foreign);
            assert!(!valid(&bad), "reference {foreign}");
        }
        let mut reversed = row.clone();
        reversed["cpu_observations"]["invocations"][2]["role"] =
            json!({"kind":"parity_comparison","candidate_execution":2,"reference_execution":1});
        assert!(!valid(&reversed));
        let mut same_backend = row.clone();
        same_backend["backend"] = json!("ptrace");
        same_backend["cpu_observations"]["binding"]["backend"] = json!("ptrace");
        same_backend["cpu_observations"]["invocations"][0]["role"]["backend"] = json!("ptrace");
        assert!(!valid(&same_backend));
        let mut naked = row.clone();
        naked["mode"] = json!("naked");
        naked["cpu_observations"]["binding"]["mode"] = json!("naked");
        assert!(!valid(&naked));
        let mut normalized = row.clone();
        normalized["cpu_observations"]["invocations"][2]["role"] =
            json!({"kind":"ptrace_normalization","execution_ordinal":2});
        assert!(valid(&normalized));
        let mut nonzero_exit = normalized.clone();
        nonzero_exit["cpu_observations"]["invocations"][1]["final_wait"]["raw_status"] = json!(256);
        assert!(valid(&nonzero_exit)); // a nonzero exit still has no timeout
        for termination in ["wall_budget_stop", "cpu_budget_stop"] {
            let mut timed_out_parent = normalized.clone();
            let prior = &mut timed_out_parent["cpu_observations"]["invocations"][1];
            prior["termination"] = json!(termination);
            if termination == "cpu_budget_stop" {
                prior["live"] = enabled();
                prior["live"]["timeout_trigger"] = prior["live"]["last"].clone();
                prior["returned_cpu_charge"] =
                    json!({"state":"value","cpu_usec":7,"basis":"max_trigger_and_final_wait4"});
            }
            let mut without_normalization = timed_out_parent.clone();
            without_normalization["cpu_observations"]["invocations"]
                .as_array_mut()
                .unwrap()
                .pop();
            assert!(valid(&without_normalization), "valid {termination} parent");
            assert!(
                !valid(&timed_out_parent),
                "normalization admitted after {termination}"
            );
        }
        let mut wrong_backend = normalized.clone();
        wrong_backend["cpu_observations"]["invocations"][2]["role"]["execution_ordinal"] = json!(1);
        assert!(!valid(&wrong_backend));
        let mut naked = normalized.clone();
        naked["mode"] = json!("naked");
        naked["cpu_observations"]["binding"]["mode"] = json!("naked");
        assert!(!valid(&naked));
        normalized["cpu_observations"]["invocations"][2]["role"]["execution_ordinal"] = json!(3);
        assert!(!valid(&normalized));
    }

    #[test]
    fn conditional_history_retains_unrecorded_rows_and_rejects_substitution() {
        let first = source_row();
        let mut second = executed_row();
        second["attempt"] = json!(2);
        second["cpu_observations"]["binding"]["outer_attempt"] = json!(2);
        assert!(
            CellCpuHistoryV1::from_source_rows(&[(1, first.clone())])
                .unwrap()
                .is_none()
        );
        let history = CellCpuHistoryV1::from_source_rows(&[(1, first), (2, second.clone())])
            .unwrap()
            .unwrap();
        assert!(matches!(
            history.attempts[0],
            CpuAttemptHistory::Unrecorded { outer_attempt: 1 }
        ));
        assert!(history.attempts[1].observations().is_some());
        let id = CellIdentity {
            lane: "portable".into(),
            category: "fixture".into(),
            test: "fixture/test".into(),
            mode: "verify".into(),
            backend: "ptrace".into(),
        };
        history
            .validate_for_artifact("run", &"a".repeat(40), &id, 2)
            .unwrap();
        let mut missing = history.clone();
        missing.attempts.remove(0);
        assert!(
            missing
                .validate_for_artifact("run", &"a".repeat(40), &id, 2)
                .is_err()
        );
        let mut substituted = history.clone();
        if let CpuAttemptHistory::Recorded { observations, .. } = &mut substituted.attempts[1] {
            observations.binding.run_id = "foreign".into();
        }
        assert!(
            substituted
                .validate_for_artifact("run", &"a".repeat(40), &id, 2)
                .is_err()
        );
        let mut all_absent = history.clone();
        all_absent.attempts[1] = CpuAttemptHistory::Unrecorded { outer_attempt: 2 };
        assert!(
            all_absent
                .validate_for_artifact("run", &"a".repeat(40), &id, 2)
                .is_err()
        );
        assert!(CellCpuHistoryV1::from_source_rows(&[(1, second)]).is_err());
    }
}
