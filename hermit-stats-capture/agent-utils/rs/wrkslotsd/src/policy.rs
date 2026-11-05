//! Pure shadow policy evaluation over replayed state and captured evidence.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::canonical::canonical_sha256;
use crate::config::ShadowConfig;
use crate::evidence::{
    CheckoutEvidence, EvidenceBundle, JournalState, LeaderState, ProcessUse, ScopeIdentity,
    ScopeState, SlotEvidence,
};
use crate::replay::PendingOperationKind;
use crate::replay::ReplayedLog;
use crate::schema::{parse_timestamp_instant, ActiveRecordMeta};
use crate::ObserverError;

/// Stable three-valued outcome of shadow policy evaluation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Verdict {
    Eligible,
    Blocked,
    Unknown,
}

/// One stable filesystem identity to be rechecked by a future executor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CheckoutBinding {
    pub(crate) name: String,
    pub(crate) path: String,
    #[serde(with = "crate::config::decimal_u64")]
    pub(crate) device: u64,
    #[serde(with = "crate::config::decimal_u64")]
    pub(crate) inode: u64,
    #[serde(with = "crate::config::decimal_u64")]
    pub(crate) mount_id: u64,
}

/// A read-only decision bound to all authority and observation inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PolicyDecision {
    pub(crate) schema: u64,
    pub(crate) verdict: Verdict,
    pub(crate) reason_codes: Vec<String>,
    pub(crate) machine: String,
    pub(crate) slot: String,
    #[serde(with = "crate::config::optional_decimal_u64")]
    pub(crate) generation: Option<u64>,
    pub(crate) event_sha256: String,
    pub(crate) active_record_sha256: Option<String>,
    pub(crate) config_sha256: Option<String>,
    pub(crate) evidence_sha256: Option<String>,
    pub(crate) evaluated_at: Option<String>,
    pub(crate) census_started_at: Option<String>,
    pub(crate) evidence_observed_at: Option<String>,
    pub(crate) evidence_age_at_evaluation_nanoseconds: Option<String>,
    pub(crate) heartbeat_at: Option<String>,
    #[serde(with = "crate::config::optional_decimal_u64")]
    pub(crate) heartbeat_ttl_seconds: Option<u64>,
    pub(crate) task_scope: Option<ScopeIdentity>,
    pub(crate) checkout_identities: Vec<CheckoutBinding>,
    #[serde(with = "crate::config::decimal_u64")]
    pub(crate) reclaimable_bytes: u64,
}

impl PolicyDecision {
    pub(crate) fn legacy(
        machine: &str,
        event_sha256: &str,
        meta: &ActiveRecordMeta,
        active_record_sha256: String,
        reason: &str,
    ) -> Self {
        Self {
            schema: 1,
            verdict: Verdict::Unknown,
            reason_codes: vec![reason.to_owned()],
            machine: machine.to_owned(),
            slot: meta.slot.clone(),
            generation: Some(meta.generation),
            event_sha256: event_sha256.to_owned(),
            active_record_sha256: Some(active_record_sha256),
            config_sha256: None,
            evidence_sha256: None,
            evaluated_at: None,
            census_started_at: None,
            evidence_observed_at: None,
            evidence_age_at_evaluation_nanoseconds: None,
            heartbeat_at: Some(meta.heartbeat_at.clone()),
            heartbeat_ttl_seconds: Some(meta.heartbeat_ttl_seconds),
            task_scope: meta.task_scope.clone(),
            checkout_identities: Vec::new(),
            reclaimable_bytes: 0,
        }
    }
}

/// Reconstruct fail-closed decisions when a derived index has no policy inputs.
pub(crate) fn evaluate_without_inputs(
    replayed: &ReplayedLog,
    reason: &str,
) -> Result<Vec<PolicyDecision>, ObserverError> {
    let mut decisions = replayed
        .active_metadata
        .iter()
        .map(|(slot, meta)| {
            let record = replayed.active_records.get(slot).ok_or_else(|| {
                ObserverError::invalid(format!("missing replayed active record for {slot}"))
            })?;
            let mut decision = PolicyDecision::legacy(
                &replayed.summary.machine,
                &replayed.summary.tip_sha256,
                meta,
                canonical_sha256(record)?,
                reason,
            );
            block_on_hold_and_pending(&mut decision, replayed, slot, meta.generation);
            Ok(decision)
        })
        .collect::<Result<Vec<_>, ObserverError>>()?;
    decisions.extend(pending_only_decisions(
        replayed,
        &PendingOnlyInputs::Missing(reason),
    ));
    Ok(decisions)
}

/// Evaluate every active record and every pending-only slot without mutation.
pub(crate) fn evaluate(
    replayed: &ReplayedLog,
    config: &ShadowConfig,
    config_sha256: &str,
    evidence: &EvidenceBundle,
    evidence_sha256: &str,
    evaluated_at: &str,
) -> Result<Vec<PolicyDecision>, ObserverError> {
    if config.machine != replayed.summary.machine || evidence.machine != replayed.summary.machine {
        return Err(ObserverError::invalid(
            "shadow configuration, evidence, and event log name different machines",
        ));
    }
    let freshness = validate_fresh_evidence(replayed, config, evidence, evaluated_at)?;
    let evidence_by_slot = evidence
        .slots
        .iter()
        .map(|row| (row.slot.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let active_slots = replayed
        .active_metadata
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if let Some(extra) = evidence_by_slot
        .keys()
        .find(|slot| !active_slots.contains(**slot))
    {
        return Err(ObserverError::invalid(format!(
            "shadow evidence names inactive slot {extra}"
        )));
    }

    let mut decisions = replayed
        .active_metadata
        .iter()
        .map(|(slot, meta)| {
            let record = replayed.active_records.get(slot).ok_or_else(|| {
                ObserverError::invalid(format!("missing replayed active record for {slot}"))
            })?;
            let record_sha256 = canonical_sha256(record)?;
            let mut decision = PolicyDecision {
                schema: 1,
                verdict: Verdict::Eligible,
                reason_codes: Vec::new(),
                machine: replayed.summary.machine.clone(),
                slot: slot.clone(),
                generation: Some(meta.generation),
                event_sha256: replayed.summary.tip_sha256.clone(),
                active_record_sha256: Some(record_sha256.clone()),
                config_sha256: Some(config_sha256.to_owned()),
                evidence_sha256: Some(evidence_sha256.to_owned()),
                evaluated_at: Some(evaluated_at.to_owned()),
                census_started_at: Some(evidence.census_started_at.clone()),
                evidence_observed_at: Some(evidence.observed_at.clone()),
                evidence_age_at_evaluation_nanoseconds: Some(
                    freshness.age_at_evaluation_nanoseconds.to_string(),
                ),
                heartbeat_at: Some(meta.heartbeat_at.clone()),
                heartbeat_ttl_seconds: Some(meta.heartbeat_ttl_seconds),
                task_scope: meta.task_scope.clone(),
                checkout_identities: Vec::new(),
                reclaimable_bytes: 0,
            };
            // This migration slice accepts a captured scope claim but has no
            // independent read-time /proc, boot-id, systemd InvocationID, or
            // cgroup verifier. It also has no fresh TaskGraph claim input bound
            // to repository, task, owner, and lifecycle attempt. Neither gap is
            // evidence of absence, so every otherwise-actionable row remains
            // UNKNOWN until those authorities exist.
            if meta.task_scope.is_some() {
                decision.unknown("TASK_SCOPE_RUNTIME_UNVERIFIED");
            }
            decision.unknown("TASKGRAPH_CLAIM_UNVERIFIED");
            let event_operation_pending =
                block_on_hold_and_pending(&mut decision, replayed, slot, meta.generation);
            // Heartbeat age depends only on the record and the census clock, so
            // a slot absent from the census keeps its time-based blockers.
            evaluate_time(&mut decision, config, freshness.lifecycle_at)?;
            let Some(observed) = evidence_by_slot.get(slot.as_str()).copied() else {
                decision.unknown("EVIDENCE_MISSING");
                return Ok(decision);
            };
            decision.reclaimable_bytes = observed.reclaimable_bytes;
            if observed.generation != meta.generation {
                decision.unknown("GENERATION_DRIFT");
            }
            if observed.active_record_sha256 != record_sha256 {
                decision.unknown("ACTIVE_RECORD_DIGEST_DRIFT");
            }
            match (&meta.task_scope, &observed.scope) {
                (Some(scope), Some(scope_evidence)) => {
                    if &scope_evidence.recorded != scope {
                        decision.unknown("SCOPE_IDENTITY_DRIFT");
                    }
                    evaluate_scope(&mut decision, scope, scope_evidence, &evidence.boot_id);
                }
                (Some(_), None) => decision.unknown("SCOPE_EVIDENCE_MISSING"),
                (None, _) => decision.unknown("LEGACY_SCOPE_IDENTITY_MISSING"),
            }
            evaluate_storage(&mut decision, meta.checkouts.as_slice(), observed);
            match observed.journal {
                JournalState::Absent => {
                    if event_operation_pending {
                        decision.unknown("JOURNAL_EVENT_EVIDENCE_CONFLICT");
                    }
                }
                JournalState::Present => decision.block("JOURNAL_PRESENT"),
                JournalState::Unknown => decision.unknown("JOURNAL_STATE_UNKNOWN"),
            }
            match observed.process_use {
                ProcessUse::Unused => {}
                ProcessUse::InUse => decision.block("PROCESS_USES_SLOT"),
                ProcessUse::Unknown => decision.unknown("PROCESS_USE_UNKNOWN"),
            }
            Ok(decision)
        })
        .collect::<Result<Vec<_>, ObserverError>>()?;

    decisions.extend(pending_only_decisions(
        replayed,
        &PendingOnlyInputs::Bound {
            config_sha256,
            evidence_sha256,
            evaluated_at,
            evidence,
            age_at_evaluation_nanoseconds: freshness.age_at_evaluation_nanoseconds,
        },
    ));
    Ok(decisions)
}

/// Record the hold and pending-marker blockers of one ACTIVE row and report
/// whether the event history leaves any operation pending for it.
fn block_on_hold_and_pending(
    decision: &mut PolicyDecision,
    replayed: &ReplayedLog,
    slot: &str,
    generation: u64,
) -> bool {
    if replayed.holds.contains_key(slot) {
        decision.block("ACTIVE_HOLD");
    }
    let mut operation_pending = false;
    for pending in replayed
        .pending_operations
        .iter()
        .filter(|pending| pending.slot == slot)
    {
        operation_pending = true;
        if pending
            .generation
            .is_some_and(|pending_generation| pending_generation != generation)
        {
            decision.block("PENDING_GENERATION_CONFLICT");
        }
        decision.block(pending_reason(pending.kind));
    }
    operation_pending
}

/// Inputs to which a pending-only decision is bound.
enum PendingOnlyInputs<'a> {
    /// No policy inputs were supplied; the reason says why.
    Missing(&'a str),
    Bound {
        config_sha256: &'a str,
        evidence_sha256: &'a str,
        evaluated_at: &'a str,
        evidence: &'a EvidenceBundle,
        age_at_evaluation_nanoseconds: i128,
    },
}

/// A slot with pending markers but no ACTIVE row is always BLOCKED: a
/// lifecycle operation may still own its checkout, and there is no record
/// whose evidence could be checked.
fn pending_only_decisions(
    replayed: &ReplayedLog,
    inputs: &PendingOnlyInputs<'_>,
) -> Vec<PolicyDecision> {
    let mut pending_without_active = BTreeMap::<&str, Vec<_>>::new();
    for pending in &replayed.pending_operations {
        if !replayed.active_metadata.contains_key(&pending.slot) {
            pending_without_active
                .entry(pending.slot.as_str())
                .or_default()
                .push(pending);
        }
    }
    pending_without_active
        .into_iter()
        .map(|(slot, pending)| {
            let first_generation = pending[0].generation;
            let generation = pending
                .iter()
                .all(|marker| marker.generation == first_generation)
                .then_some(first_generation)
                .flatten();
            let mut decision = PolicyDecision {
                schema: 1,
                verdict: Verdict::Blocked,
                reason_codes: Vec::new(),
                machine: replayed.summary.machine.clone(),
                slot: slot.to_owned(),
                generation,
                event_sha256: replayed.summary.tip_sha256.clone(),
                active_record_sha256: None,
                config_sha256: None,
                evidence_sha256: None,
                evaluated_at: None,
                census_started_at: None,
                evidence_observed_at: None,
                evidence_age_at_evaluation_nanoseconds: None,
                heartbeat_at: None,
                heartbeat_ttl_seconds: None,
                task_scope: None,
                checkout_identities: Vec::new(),
                reclaimable_bytes: 0,
            };
            match inputs {
                PendingOnlyInputs::Missing(reason) => decision.unknown(reason),
                PendingOnlyInputs::Bound {
                    config_sha256,
                    evidence_sha256,
                    evaluated_at,
                    evidence,
                    age_at_evaluation_nanoseconds,
                } => {
                    decision.config_sha256 = Some((*config_sha256).to_owned());
                    decision.evidence_sha256 = Some((*evidence_sha256).to_owned());
                    decision.evaluated_at = Some((*evaluated_at).to_owned());
                    decision.census_started_at = Some(evidence.census_started_at.clone());
                    decision.evidence_observed_at = Some(evidence.observed_at.clone());
                    decision.evidence_age_at_evaluation_nanoseconds =
                        Some(age_at_evaluation_nanoseconds.to_string());
                }
            }
            decision.block("ACTIVE_RECORD_MISSING");
            if generation.is_none() && pending.iter().any(|marker| marker.generation.is_some()) {
                decision.unknown("PENDING_GENERATION_CONFLICT");
            }
            for marker in pending {
                decision.block(pending_reason(marker.kind));
            }
            decision
        })
        .collect()
}

fn pending_reason(kind: PendingOperationKind) -> &'static str {
    match kind {
        PendingOperationKind::Journal => "OPERATION_JOURNAL_PENDING",
        PendingOperationKind::Reclaim => "RECLAIM_PENDING",
        PendingOperationKind::Recovery => "RECOVERY_PENDING",
        PendingOperationKind::Retirement => "RETIREMENT_PENDING",
    }
}

struct FreshEvidence {
    lifecycle_at: DateTime<chrono::FixedOffset>,
    age_at_evaluation_nanoseconds: i128,
}

fn validate_fresh_evidence(
    replayed: &ReplayedLog,
    config: &ShadowConfig,
    evidence: &EvidenceBundle,
    evaluated_at: &str,
) -> Result<FreshEvidence, ObserverError> {
    if evidence.event_tip_sha256 != replayed.summary.tip_sha256 {
        return Err(ObserverError::invalid(
            "shadow evidence is not bound to the replayed event tip",
        ));
    }
    let started_at = DateTime::parse_from_rfc3339(&evidence.census_started_at)
        .map_err(|_| ObserverError::invalid("invalid shadow evidence census_started_at"))?;
    let observed_at = DateTime::parse_from_rfc3339(&evidence.observed_at)
        .map_err(|_| ObserverError::invalid("invalid shadow evidence observed_at"))?;
    let evaluated_at = DateTime::parse_from_rfc3339(evaluated_at)
        .map_err(|_| ObserverError::invalid("invalid shadow policy evaluated_at"))?;
    // Replay admits event timestamps in the CPython grammar, not only RFC 3339.
    let event_tip_recorded_at =
        parse_timestamp_instant(&replayed.tip_recorded_at, "replayed event tip")?;
    if event_tip_recorded_at.signed_duration_since(started_at)
        > bounded_duration(config.maximum_future_skew_seconds)?
    {
        return Err(ObserverError::invalid(
            "shadow evidence census began before the replayed event tip beyond permitted skew",
        ));
    }
    let census_window = observed_at.signed_duration_since(started_at);
    if census_window < chrono::Duration::zero()
        || census_window > bounded_duration(config.maximum_census_seconds)?
    {
        return Err(ObserverError::invalid(
            "shadow evidence census window is negative or exceeds its configured bound",
        ));
    }
    validate_evidence_age(config, observed_at, evaluated_at)?;
    Ok(FreshEvidence {
        lifecycle_at: std::cmp::min(started_at, evaluated_at),
        age_at_evaluation_nanoseconds: exact_nanoseconds(
            evaluated_at.signed_duration_since(observed_at),
        )?,
    })
}

/// Recheck a captured census against a later read time without trusting its
/// stored decision. This does not authenticate the SQLite file or live EVENTS.
pub(crate) fn validate_current_evidence_age(
    config: &ShadowConfig,
    evidence: &EvidenceBundle,
    evaluated_at: &str,
    current_at: &str,
) -> Result<(), ObserverError> {
    let observed_at = DateTime::parse_from_rfc3339(&evidence.observed_at)
        .map_err(|_| ObserverError::invalid("invalid shadow evidence observed_at"))?;
    let evaluated_at = DateTime::parse_from_rfc3339(evaluated_at)
        .map_err(|_| ObserverError::invalid("invalid indexed policy evaluated_at"))?;
    let current_at = DateTime::parse_from_rfc3339(current_at)
        .map_err(|_| ObserverError::invalid("invalid current policy read timestamp"))?;
    if current_at < evaluated_at {
        return Err(ObserverError::invalid(
            "current policy read timestamp precedes indexed evaluated_at (clock rollback)",
        ));
    }
    validate_evidence_age(config, observed_at, current_at)
}

fn validate_evidence_age(
    config: &ShadowConfig,
    observed_at: DateTime<chrono::FixedOffset>,
    reference_at: DateTime<chrono::FixedOffset>,
) -> Result<(), ObserverError> {
    let age = reference_at.signed_duration_since(observed_at);
    if age < -bounded_duration(config.maximum_future_skew_seconds)? {
        return Err(ObserverError::invalid(
            "shadow evidence timestamp is too far in the future",
        ));
    }
    if age > bounded_duration(config.evidence_max_age_seconds)? {
        return Err(ObserverError::invalid(
            "shadow evidence is older than its configured maximum age",
        ));
    }
    Ok(())
}

fn bounded_duration(seconds: u64) -> Result<chrono::Duration, ObserverError> {
    let seconds = i64::try_from(seconds)
        .map_err(|_| ObserverError::invalid("shadow policy duration is not representable"))?;
    chrono::Duration::try_seconds(seconds)
        .ok_or_else(|| ObserverError::invalid("shadow policy duration is not representable"))
}

fn exact_nanoseconds(duration: chrono::Duration) -> Result<i128, ObserverError> {
    let seconds = duration.num_seconds();
    let remainder = duration - chrono::Duration::seconds(seconds);
    let remainder = remainder
        .num_nanoseconds()
        .ok_or_else(|| ObserverError::invalid("timestamp remainder exceeds nanosecond range"))?;
    Ok(i128::from(seconds) * 1_000_000_000 + i128::from(remainder))
}

pub(crate) fn evaluate_time(
    decision: &mut PolicyDecision,
    config: &ShadowConfig,
    lifecycle_at: DateTime<chrono::FixedOffset>,
) -> Result<(), ObserverError> {
    // Replay requires a heartbeat it can parse, so neither branch below is
    // reachable from a replayed record. A missing or unageable heartbeat may
    // be recent, so both block rather than merely withholding a verdict.
    let Some(heartbeat_at) = decision.heartbeat_at.as_deref() else {
        decision.block("ACTIVE_HEARTBEAT_MISSING");
        return Ok(());
    };
    let Ok(heartbeat) = parse_timestamp_instant(heartbeat_at, "active heartbeat") else {
        decision.block("HEARTBEAT_TIMESTAMP_UNSUPPORTED");
        return Ok(());
    };
    // A heartbeat after the lifecycle instant has a negative age and is
    // younger than every threshold, as in the Python diagnosis that clamps
    // age at zero. Only a lead beyond the permitted skew is clock evidence.
    let age_nanoseconds = exact_nanoseconds(lifecycle_at.signed_duration_since(heartbeat))?;
    if age_nanoseconds < -i128::from(config.maximum_future_skew_seconds) * 1_000_000_000 {
        decision.unknown("CLOCK_BEFORE_HEARTBEAT");
    }
    match decision.heartbeat_ttl_seconds {
        Some(heartbeat_ttl_seconds)
            if age_nanoseconds <= i128::from(heartbeat_ttl_seconds) * 1_000_000_000 =>
        {
            decision.block("HEARTBEAT_TTL_ACTIVE");
        }
        Some(_) => {}
        None => decision.unknown("ACTIVE_HEARTBEAT_TTL_MISSING"),
    }
    if age_nanoseconds <= i128::from(config.minimum_stale_seconds) * 1_000_000_000 {
        decision.block("MINIMUM_STALE_AGE_ACTIVE");
    }
    Ok(())
}

fn evaluate_scope(
    decision: &mut PolicyDecision,
    recorded: &ScopeIdentity,
    observed: &crate::evidence::ScopeEvidence,
    current_boot_id: &str,
) {
    if recorded.boot_id != current_boot_id && observed.state != ScopeState::Dead {
        decision.unknown("REBOOT_SCOPE_STATE_CONFLICT");
    }
    match observed.state {
        ScopeState::Live => decision.block("TASK_SCOPE_LIVE"),
        ScopeState::Dead => {}
        ScopeState::Unknown => decision.unknown("TASK_SCOPE_STATE_UNKNOWN"),
    }
    match observed.leader_state {
        LeaderState::Same => decision.block("TASK_LEADER_LIVE"),
        LeaderState::Reused | LeaderState::Absent => {}
        LeaderState::Unknown => decision.unknown("TASK_LEADER_STATE_UNKNOWN"),
    }
}

fn evaluate_storage(
    decision: &mut PolicyDecision,
    expected: &[(String, String)],
    observed: &SlotEvidence,
) {
    let by_name = observed
        .checkouts
        .iter()
        .map(|checkout| (checkout.name.as_str(), checkout))
        .collect::<BTreeMap<_, _>>();
    if by_name.len() != expected.len() {
        decision.unknown("CHECKOUT_EVIDENCE_SET_DRIFT");
    }
    for (name, path) in expected {
        let Some(checkout) = by_name.get(name.as_str()).copied() else {
            decision.unknown("CHECKOUT_EVIDENCE_MISSING");
            continue;
        };
        if checkout.path != *path {
            decision.unknown("CHECKOUT_PATH_DRIFT");
        }
        decision
            .checkout_identities
            .push(checkout_binding(checkout));
        if !checkout.directory {
            decision.block("CHECKOUT_NOT_DIRECTORY");
        }
        if !checkout.symlink_free {
            decision.block("CHECKOUT_SYMLINK_COMPONENT");
        }
        if !checkout.mount_stable {
            decision.block("CHECKOUT_MOUNT_CROSSING");
        }
    }
}

fn checkout_binding(checkout: &CheckoutEvidence) -> CheckoutBinding {
    CheckoutBinding {
        name: checkout.name.clone(),
        path: checkout.path.clone(),
        device: checkout.device,
        inode: checkout.inode,
        mount_id: checkout.mount_id,
    }
}

impl PolicyDecision {
    fn block(&mut self, reason: &str) {
        // A known destructive blocker is stronger than incomplete evidence.
        self.verdict = Verdict::Blocked;
        self.push_reason(reason);
    }

    fn unknown(&mut self, reason: &str) {
        if self.verdict == Verdict::Eligible {
            self.verdict = Verdict::Unknown;
        }
        self.push_reason(reason);
    }

    fn push_reason(&mut self, reason: &str) {
        if !self.reason_codes.iter().any(|existing| existing == reason) {
            self.reason_codes.push(reason.to_owned());
        }
    }
}
