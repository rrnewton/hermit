use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OpenFlags, Transaction, TransactionBehavior};
use rustix::fs::{renameat_with, RenameFlags, CWD};

use crate::canonical::{canonical_json, canonical_sha256};
use crate::config::{is_sha256, ShadowConfig};
use crate::evidence::EvidenceBundle;
use crate::plan::{self, PressurePlan};
use crate::policy::{self, PolicyDecision, Verdict};
use crate::replay::{
    canonical_payload, replay_indexed_events, replay_stream, Event, ReplayGuard, ReplaySummary,
    ReplayedLog, SlotHold,
};
use crate::ObserverError;

const INDEX_SCHEMA: u64 = 2;
const LEGACY_INDEX_SCHEMA: u64 = 1;
const PRIVATE_INDEX_ATTEMPTS: u64 = 128;
static PRIVATE_INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const LEGACY_INDEX_TABLES: &[&str] = &[
    "active_records",
    "archive_records",
    "event_log",
    "observer_metadata",
];
const INDEX_TABLES: &[&str] = &[
    "active_records",
    "archive_records",
    "event_log",
    "holds",
    "observer_metadata",
    "policy_decisions",
];

struct PolicyInputs {
    config: ShadowConfig,
    config_sha256: String,
    config_json: String,
    evidence: EvidenceBundle,
    evidence_sha256: String,
    evidence_json: String,
    evaluated_at: Option<String>,
}

type PolicyMetadataRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u64>,
);

/// Replay an event directory and atomically replace its disposable SQLite index.
///
/// One immediate SQLite transaction serializes rebuilders, streams validated
/// events directly into replacement tables, and commits only after semantic
/// replay finishes. A first index is built under a private sibling name and
/// atomically renamed into place with no-replace semantics only when complete.
/// Initial publication therefore requires Linux `renameat2(RENAME_NOREPLACE)`
/// support from the filesystem containing the index.
/// An existing index is replaced only by the same chain or a strict extension
/// of its recorded tip. The terminal index path is opened with no-follow
/// semantics and must have one hard link before and after opening; callers must
/// still place the disposable database in a directory they trust against
/// concurrent directory-entry replacement.
pub fn rebuild_index(events_dir: &Path, index_path: &Path) -> Result<ReplaySummary, ObserverError> {
    rebuild_index_inner(events_dir, index_path, None)
}

/// Rebuild an index and materialize digest-bound read-only policy decisions.
pub(crate) fn rebuild_policy_index(
    events_dir: &Path,
    index_path: &Path,
    config_path: &Path,
    evidence_path: &Path,
) -> Result<ReplaySummary, ObserverError> {
    let config = ShadowConfig::load(config_path)?;
    let evidence = EvidenceBundle::load(evidence_path)?;
    let inputs = PolicyInputs {
        config: config.value,
        config_sha256: config.sha256,
        config_json: config.canonical_json,
        evidence: evidence.value,
        evidence_sha256: evidence.sha256,
        evidence_json: evidence.canonical_json,
        evaluated_at: None,
    };
    rebuild_index_inner(events_dir, index_path, Some(&inputs))
}

fn rebuild_index_inner(
    events_dir: &Path,
    index_path: &Path,
    policy: Option<&PolicyInputs>,
) -> Result<ReplaySummary, ObserverError> {
    reject_index_in_event_directory(events_dir, index_path)?;
    let before = inspect_index_path(index_path)?;
    match before {
        Some(identity) => rebuild_database(events_dir, index_path, Some(identity), policy),
        None => rebuild_initial_index(events_dir, index_path, policy, || {}, |_| {}),
    }
}

fn rebuild_initial_index(
    events_dir: &Path,
    index_path: &Path,
    policy: Option<&PolicyInputs>,
    before_publish: impl FnOnce(),
    after_publish: impl FnOnce(&Path),
) -> Result<ReplaySummary, ObserverError> {
    let private = PrivateIndex::create(index_path)?;
    let identity = private.identity;
    let summary = rebuild_database(events_dir, &private.path, Some(identity), policy)?;
    before_publish();

    match renameat_with(CWD, &private.path, CWD, index_path, RenameFlags::NOREPLACE) {
        Ok(()) => {
            // RENAME_NOREPLACE moves the completed inode into place in one
            // operation: the private name is gone and the terminal name has
            // exactly one link as soon as it becomes visible.
            after_publish(&private.path);
            drop(private);
            let published = inspect_index_path(index_path)?.ok_or_else(|| {
                ObserverError::invalid(format!(
                    "derived index disappeared while publishing: {}",
                    index_path.display()
                ))
            })?;
            if published != identity {
                return Err(ObserverError::invalid(format!(
                    "derived index identity changed while publishing: {}",
                    index_path.display()
                )));
            }
            Ok(summary)
        }
        Err(error) if error == rustix::io::Errno::EXIST => {
            // Another initial rebuilder won publication. Re-enter through the
            // existing-index path so its chain and schema guards remain the
            // authority for whether this replay may replace it.
            drop(private);
            rebuild_index_inner(events_dir, index_path, policy)
        }
        Err(error) => Err(ObserverError::with_source(
            format!(
                "cannot publish completed derived index {} with renameat2(RENAME_NOREPLACE); the index filesystem must support it",
                index_path.display()
            ),
            error,
        )),
    }
}

#[cfg(test)]
pub(crate) fn rebuild_index_with_prepublication_hook(
    events_dir: &Path,
    index_path: &Path,
    before_publish: impl FnOnce(),
) -> Result<ReplaySummary, ObserverError> {
    reject_index_in_event_directory(events_dir, index_path)?;
    if inspect_index_path(index_path)?.is_some() {
        return Err(ObserverError::invalid(
            "prepublication hook requires an absent index",
        ));
    }
    rebuild_initial_index(events_dir, index_path, None, before_publish, |_| {})
}

#[cfg(test)]
pub(crate) fn rebuild_index_with_postpublication_hook(
    events_dir: &Path,
    index_path: &Path,
    after_publish: impl FnOnce(&Path),
) -> Result<ReplaySummary, ObserverError> {
    reject_index_in_event_directory(events_dir, index_path)?;
    if inspect_index_path(index_path)?.is_some() {
        return Err(ObserverError::invalid(
            "postpublication hook requires an absent index",
        ));
    }
    rebuild_initial_index(events_dir, index_path, None, || {}, after_publish)
}

fn rebuild_database(
    events_dir: &Path,
    index_path: &Path,
    before: Option<FileIdentity>,
    policy_inputs: Option<&PolicyInputs>,
) -> Result<ReplaySummary, ObserverError> {
    let mut connection = Connection::open_with_flags(
        index_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| {
        ObserverError::with_source(
            format!("cannot open derived index {}", index_path.display()),
            error,
        )
    })?;
    verify_opened_index(index_path, before)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| ObserverError::with_source("cannot begin index rebuild", error))?;
    let existing = existing_index(&transaction)?;
    let prior_evaluated_at = if existing.is_some() {
        refuse_nonmonotonic_evidence(&transaction, policy_inputs)?
    } else {
        None
    };
    replace_schema(&transaction)?;

    let guard = existing.as_ref().map(|summary| ReplayGuard {
        replay_count: summary.replay_count,
        tip_sha256: summary.tip_sha256.as_str(),
    });
    let replayed = {
        let mut statement = transaction
            .prepare(
                "INSERT INTO event_log (
                    sequence, machine, previous_sha256, recorded_at, kind, payload_json, sha256
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|error| ObserverError::with_source("cannot prepare event indexing", error))?;
        replay_stream(events_dir, guard, |event| {
            statement
                .execute(params![
                    to_sql_integer(event.sequence, "event sequence")?,
                    event.machine,
                    event.previous_sha256,
                    event.recorded_at,
                    event.kind,
                    canonical_payload(event)?,
                    event.sha256,
                ])
                .map_err(|error| ObserverError::with_source("cannot index an event", error))?;
            Ok(())
        })?
    };
    if existing
        .as_ref()
        .is_some_and(|summary| summary.machine != replayed.summary.machine)
    {
        return Err(ObserverError::invalid(
            "refusing to replace an index for another machine",
        ));
    }
    // Capture freshness only after the full authoritative replay finishes, so
    // a slow replay cannot spend the evidence age budget before evaluation.
    let evaluated_at = policy_inputs.map(|_| current_rfc3339()).transpose()?;
    if let (Some(prior), Some(replacement)) =
        (prior_evaluated_at.as_deref(), evaluated_at.as_deref())
    {
        let prior = chrono::DateTime::parse_from_rfc3339(prior)
            .map_err(|_| ObserverError::invalid("prior evaluated_at is invalid"))?;
        let replacement = chrono::DateTime::parse_from_rfc3339(replacement)
            .map_err(|_| ObserverError::invalid("replacement evaluated_at is invalid"))?;
        if replacement < prior {
            return Err(ObserverError::invalid(
                "refusing a policy rebuild whose evaluated_at regresses",
            ));
        }
    }
    let decisions = policy_inputs
        .map(|inputs| {
            let evaluated_at = evaluated_at
                .as_deref()
                .ok_or_else(|| ObserverError::invalid("policy evaluation timestamp is missing"))?;
            policy::evaluate(
                &replayed,
                &inputs.config,
                &inputs.config_sha256,
                &inputs.evidence,
                &inputs.evidence_sha256,
                evaluated_at,
            )
        })
        .transpose()?;
    materialize_state(&transaction, &replayed, decisions.as_deref())?;
    write_summary(
        &transaction,
        &replayed.summary,
        policy_inputs,
        evaluated_at.as_deref(),
    )?;
    transaction
        .commit()
        .map_err(|error| ObserverError::with_source("cannot commit index rebuild", error))?;
    Ok(replayed.summary)
}

struct PrivateIndex {
    path: PathBuf,
    identity: FileIdentity,
}

impl PrivateIndex {
    fn create(index_path: &Path) -> Result<Self, ObserverError> {
        let file_name = index_path
            .file_name()
            .ok_or_else(|| ObserverError::invalid("derived index path has no file name"))?;
        for _ in 0..PRIVATE_INDEX_ATTEMPTS {
            let sequence = PRIVATE_INDEX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut private_name = OsString::from(".");
            private_name.push(file_name);
            private_name.push(format!(
                ".wrkslotsd-private-{}-{sequence}",
                std::process::id()
            ));
            let path = index_path.with_file_name(private_name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    let metadata = file.metadata().map_err(|error| {
                        ObserverError::with_source(
                            format!("cannot inspect private index {}", path.display()),
                            error,
                        )
                    })?;
                    if !metadata.is_file() || metadata.nlink() != 1 {
                        return Err(ObserverError::invalid(format!(
                            "private index is not a singly linked regular file: {}",
                            path.display()
                        )));
                    }
                    return Ok(Self {
                        path,
                        identity: FileIdentity {
                            device: metadata.dev(),
                            inode: metadata.ino(),
                        },
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(ObserverError::with_source(
                        format!(
                            "cannot create private derived index beside {}",
                            index_path.display()
                        ),
                        error,
                    ))
                }
            }
        }
        Err(ObserverError::invalid(format!(
            "cannot allocate a private derived index beside {}",
            index_path.display()
        )))
    }
}

impl Drop for PrivateIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut sidecar = self.path.as_os_str().to_os_string();
            sidecar.push(suffix);
            let _ = fs::remove_file(PathBuf::from(sidecar));
        }
    }
}

/// Read and cross-check an existing derived index in one SQLite snapshot.
pub fn read_index(index_path: &Path) -> Result<ReplaySummary, ObserverError> {
    let before = inspect_index_path(index_path)?.ok_or_else(|| {
        ObserverError::invalid(format!(
            "derived index does not exist: {}",
            index_path.display()
        ))
    })?;
    let mut connection = Connection::open_with_flags(
        index_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| {
        ObserverError::with_source(
            format!(
                "cannot open derived index read-only: {}",
                index_path.display()
            ),
            error,
        )
    })?;
    verify_opened_index(index_path, Some(before))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|error| ObserverError::with_source("cannot begin index snapshot", error))?;
    let summary = existing_index(&transaction)?.ok_or_else(|| {
        ObserverError::invalid(format!("derived index is empty: {}", index_path.display()))
    })?;
    transaction
        .commit()
        .map_err(|error| ObserverError::with_source("cannot close index snapshot", error))?;
    Ok(summary)
}

/// Read one previously materialized decision without changing the index.
pub(crate) fn read_decision(
    index_path: &Path,
    slot: &str,
) -> Result<PolicyDecision, ObserverError> {
    read_decision_at(index_path, slot, &current_rfc3339()?)
}

pub(crate) fn read_decision_at(
    index_path: &Path,
    slot: &str,
    current_at: &str,
) -> Result<PolicyDecision, ObserverError> {
    let (connection, before) = open_index_read_only(index_path)?;
    verify_opened_index(index_path, Some(before))?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| ObserverError::with_source("cannot begin index snapshot", error))?;
    let summary = existing_index(&transaction)?.ok_or_else(|| {
        ObserverError::invalid(format!("derived index is empty: {}", index_path.display()))
    })?;
    let schema: u64 = transaction
        .query_row(
            "SELECT schema_version FROM observer_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| ObserverError::with_source("cannot read index schema", error))?;
    let decision = read_validated_decisions(&transaction, &summary, schema, current_at)?
        .into_iter()
        .find(|decision| decision.slot == slot)
        .ok_or_else(|| ObserverError::invalid(format!("unknown active or pending slot {slot}")))?;
    transaction
        .commit()
        .map_err(|error| ObserverError::with_source("cannot close index snapshot", error))?;
    verify_opened_index(index_path, Some(before))?;
    Ok(decision)
}

/// Build a non-executing pressure plan from decisions captured in the index.
pub(crate) fn read_pressure_plan(
    index_path: &Path,
    target_bytes: u64,
    requested_limit: Option<usize>,
) -> Result<PressurePlan, ObserverError> {
    read_pressure_plan_at(
        index_path,
        target_bytes,
        requested_limit,
        &current_rfc3339()?,
    )
}

pub(crate) fn read_pressure_plan_at(
    index_path: &Path,
    target_bytes: u64,
    requested_limit: Option<usize>,
    current_at: &str,
) -> Result<PressurePlan, ObserverError> {
    let (connection, before) = open_index_read_only(index_path)?;
    verify_opened_index(index_path, Some(before))?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| ObserverError::with_source("cannot begin index snapshot", error))?;
    let summary = existing_index(&transaction)?.ok_or_else(|| {
        ObserverError::invalid(format!("derived index is empty: {}", index_path.display()))
    })?;
    let schema: u64 = transaction
        .query_row(
            "SELECT schema_version FROM observer_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| ObserverError::with_source("cannot read index schema", error))?;
    let decisions = read_validated_decisions(&transaction, &summary, schema, current_at)?;
    let configured_limit = if schema == INDEX_SCHEMA {
        transaction
            .query_row(
                "SELECT max_plan_slots FROM observer_metadata WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<u64>>(0),
            )
            .map_err(|error| ObserverError::with_source("cannot read plan limit", error))?
            .unwrap_or(250)
    } else {
        250
    };
    let configured_limit = usize::try_from(configured_limit)
        .map_err(|_| ObserverError::invalid("indexed plan limit does not fit usize"))?;
    let limit = requested_limit
        .unwrap_or(configured_limit)
        .min(configured_limit);
    if limit == 0 {
        return Err(ObserverError::invalid(
            "pressure plan limit must be positive",
        ));
    }
    let result = plan::build(decisions, target_bytes, limit);
    transaction
        .commit()
        .map_err(|error| ObserverError::with_source("cannot close index snapshot", error))?;
    verify_opened_index(index_path, Some(before))?;
    Ok(result)
}

fn open_index_read_only(index_path: &Path) -> Result<(Connection, FileIdentity), ObserverError> {
    let before = inspect_index_path(index_path)?.ok_or_else(|| {
        ObserverError::invalid(format!(
            "derived index does not exist: {}",
            index_path.display()
        ))
    })?;
    let connection = Connection::open_with_flags(
        index_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| {
        ObserverError::with_source(
            format!(
                "cannot open derived index read-only: {}",
                index_path.display()
            ),
            error,
        )
    })?;
    Ok((connection, before))
}

fn refuse_nonmonotonic_evidence(
    transaction: &Transaction<'_>,
    replacement: Option<&PolicyInputs>,
) -> Result<Option<String>, ObserverError> {
    let schema: u64 = transaction
        .query_row(
            "SELECT schema_version FROM observer_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| ObserverError::with_source("cannot read prior index schema", error))?;
    if schema != INDEX_SCHEMA {
        return Ok(None);
    }
    let prior: (Option<String>, Option<String>, Option<String>) = transaction
        .query_row(
            "SELECT evidence_sha256, evidence_observed_at, evaluated_at
             FROM observer_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| ObserverError::with_source("cannot read prior evidence stamp", error))?;
    let (Some(prior_digest), Some(prior_observed_at), Some(prior_evaluated_at)) = prior else {
        return Ok(None);
    };
    let Some(replacement) = replacement else {
        return Err(ObserverError::invalid(
            "refusing to discard the indexed policy evidence high-water mark; \
             delete the disposable index to rebuild without policy inputs",
        ));
    };
    let prior_time = chrono::DateTime::parse_from_rfc3339(&prior_observed_at)
        .map_err(|_| ObserverError::invalid("prior indexed evidence timestamp is invalid"))?;
    let replacement_time = chrono::DateTime::parse_from_rfc3339(&replacement.evidence.observed_at)
        .map_err(|_| ObserverError::invalid("replacement evidence timestamp is invalid"))?;
    if replacement_time < prior_time
        || (replacement_time == prior_time && replacement.evidence_sha256 != prior_digest)
    {
        return Err(ObserverError::invalid(
            "refusing to replace indexed policy decisions with older or equivocal evidence",
        ));
    }
    Ok(Some(prior_evaluated_at))
}

fn read_validated_decisions(
    transaction: &Transaction<'_>,
    summary: &ReplaySummary,
    schema: u64,
    current_at: &str,
) -> Result<Vec<PolicyDecision>, ObserverError> {
    if schema == LEGACY_INDEX_SCHEMA {
        let replayed = replay_indexed_log(transaction, summary)?;
        return policy::evaluate_without_inputs(&replayed, "LEGACY_INDEX_SCHEMA");
    }
    let replayed = load_indexed_replay(transaction, summary)?;
    let Some(inputs) = load_policy_inputs(transaction)? else {
        return policy::evaluate_without_inputs(&replayed, "POLICY_INPUTS_MISSING");
    };
    let evaluated_at = inputs
        .evaluated_at
        .as_deref()
        .ok_or_else(|| ObserverError::invalid("indexed policy evaluation time is missing"))?;
    policy::validate_current_evidence_age(
        &inputs.config,
        &inputs.evidence,
        evaluated_at,
        current_at,
    )?;
    let expected = policy::evaluate(
        &replayed,
        &inputs.config,
        &inputs.config_sha256,
        &inputs.evidence,
        &inputs.evidence_sha256,
        evaluated_at,
    )?;
    let mut stored = BTreeMap::new();
    let mut statement = transaction
        .prepare(
            "SELECT slot, generation, verdict, active_record_sha256, event_sha256,
                    config_sha256, evidence_sha256, decision_json
             FROM policy_decisions ORDER BY slot",
        )
        .map_err(|error| {
            ObserverError::with_source("cannot prepare policy decision read", error)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|error| ObserverError::with_source("cannot list policy decisions", error))?;
    for row in rows {
        let (slot, generation, verdict, active, event, config, evidence, encoded) =
            row.map_err(|error| ObserverError::with_source("cannot read policy decision", error))?;
        if stored
            .insert(
                slot,
                (
                    generation, verdict, active, event, config, evidence, encoded,
                ),
            )
            .is_some()
        {
            return Err(ObserverError::invalid("duplicate indexed policy decision"));
        }
    }
    if stored.len() != expected.len() {
        return Err(ObserverError::invalid(
            "indexed policy decisions do not exactly cover replayed active and pending state",
        ));
    }
    for decision in &expected {
        let Some((generation, verdict, active, event, config, evidence, encoded)) =
            stored.get(&decision.slot)
        else {
            return Err(ObserverError::invalid(
                "indexed policy decisions do not exactly cover replayed active and pending state",
            ));
        };
        let expected_json = serde_json::to_string(decision).map_err(|error| {
            ObserverError::with_source("cannot encode recomputed policy decision", error)
        })?;
        let decoded: PolicyDecision = serde_json::from_str(encoded).map_err(|error| {
            ObserverError::with_source("indexed policy decision is invalid", error)
        })?;
        if decoded != *decision
            || encoded != &expected_json
            || generation != &decision.generation.map(|value| value.to_string())
            || verdict != verdict_name(decision.verdict)
            || active != &decision.active_record_sha256
            || event != &decision.event_sha256
            || config != &inputs.config_sha256
            || evidence != &inputs.evidence_sha256
        {
            return Err(ObserverError::invalid(
                "indexed policy decision differs from canonical re-evaluation",
            ));
        }
    }
    Ok(expected)
}

fn current_rfc3339() -> Result<String, ObserverError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ObserverError::invalid("system clock is before the Unix epoch"))?;
    let seconds = i64::try_from(elapsed.as_secs())
        .map_err(|_| ObserverError::invalid("system clock does not fit an RFC 3339 timestamp"))?;
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, elapsed.subsec_nanos())
        .ok_or_else(|| ObserverError::invalid("system clock does not fit an RFC 3339 timestamp"))
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
}

fn load_policy_inputs(
    transaction: &Transaction<'_>,
) -> Result<Option<PolicyInputs>, ObserverError> {
    let row: PolicyMetadataRow = transaction
        .query_row(
            "SELECT config_sha256, config_json, evidence_sha256, evidence_json,
                    evidence_observed_at, evaluated_at, max_plan_slots
             FROM observer_metadata WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .map_err(|error| ObserverError::with_source("cannot read policy inputs", error))?;
    if row.0.is_none()
        && row.1.is_none()
        && row.2.is_none()
        && row.3.is_none()
        && row.4.is_none()
        && row.5.is_none()
        && row.6.is_none()
    {
        return Ok(None);
    }
    let (
        Some(config_sha256),
        Some(config_json),
        Some(evidence_sha256),
        Some(evidence_json),
        Some(evidence_observed_at),
        Some(evaluated_at),
        Some(max_plan_slots),
    ) = row
    else {
        return Err(ObserverError::invalid(
            "indexed policy inputs are only partially populated",
        ));
    };
    let config_value: serde_json::Value = serde_json::from_str(&config_json).map_err(|error| {
        ObserverError::with_source("indexed shadow configuration is invalid", error)
    })?;
    let evidence_value: serde_json::Value = serde_json::from_str(&evidence_json)
        .map_err(|error| ObserverError::with_source("indexed shadow evidence is invalid", error))?;
    if canonical_json(&config_value)? != config_json
        || canonical_sha256(&config_value)? != config_sha256
        || canonical_json(&evidence_value)? != evidence_json
        || canonical_sha256(&evidence_value)? != evidence_sha256
    {
        return Err(ObserverError::invalid(
            "indexed policy input JSON does not match its canonical digest",
        ));
    }
    let config: ShadowConfig = serde_json::from_value(config_value).map_err(|error| {
        ObserverError::with_source("indexed shadow configuration is invalid", error)
    })?;
    let evidence: EvidenceBundle = serde_json::from_value(evidence_value)
        .map_err(|error| ObserverError::with_source("indexed shadow evidence is invalid", error))?;
    config.validate()?;
    evidence.validate()?;
    if evidence.observed_at != evidence_observed_at
        || config.max_plan_slots as u64 != max_plan_slots
        || chrono::DateTime::parse_from_rfc3339(&evaluated_at).is_err()
    {
        return Err(ObserverError::invalid(
            "indexed policy input metadata differs from canonical inputs",
        ));
    }
    Ok(Some(PolicyInputs {
        config,
        config_sha256,
        config_json,
        evidence,
        evidence_sha256,
        evidence_json,
        evaluated_at: Some(evaluated_at),
    }))
}

fn load_indexed_replay(
    transaction: &Transaction<'_>,
    summary: &ReplaySummary,
) -> Result<ReplayedLog, ObserverError> {
    let replayed = replay_indexed_log(transaction, summary)?;
    validate_materialized_state(transaction, &replayed)?;
    Ok(replayed)
}

fn replay_indexed_log(
    transaction: &Transaction<'_>,
    summary: &ReplaySummary,
) -> Result<ReplayedLog, ObserverError> {
    let mut statement = transaction
        .prepare(
            "SELECT sequence, machine, previous_sha256, recorded_at, kind, payload_json, sha256
             FROM event_log ORDER BY sequence",
        )
        .map_err(|error| {
            ObserverError::with_source("cannot prepare indexed event replay", error)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|error| ObserverError::with_source("cannot list indexed events", error))?;
    let mut events = Vec::new();
    for row in rows {
        let (sequence, machine, previous_sha256, recorded_at, kind, payload_json, sha256) =
            row.map_err(|error| ObserverError::with_source("cannot read indexed event", error))?;
        let payload: serde_json::Value = serde_json::from_str(&payload_json).map_err(|error| {
            ObserverError::with_source("indexed event payload is invalid", error)
        })?;
        if canonical_json(&payload)? != payload_json {
            return Err(ObserverError::invalid(
                "indexed event payload is not canonical JSON",
            ));
        }
        events.push(Event {
            machine,
            sequence,
            previous_sha256,
            recorded_at,
            kind,
            payload,
            sha256,
        });
    }
    let replayed = replay_indexed_events(&summary.machine, events)?;
    if replayed.summary != *summary {
        return Err(ObserverError::invalid(
            "indexed event replay differs from index metadata",
        ));
    }
    Ok(replayed)
}

fn validate_materialized_state(
    transaction: &Transaction<'_>,
    replayed: &ReplayedLog,
) -> Result<(), ObserverError> {
    let mut active_rows = BTreeMap::new();
    let mut statement = transaction
        .prepare(
            "SELECT slot, generation, record_sha256, heartbeat_at,
                    heartbeat_ttl_seconds, scope_json, record_json
             FROM active_records ORDER BY slot",
        )
        .map_err(|error| {
            ObserverError::with_source("cannot prepare active projection read", error)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|error| ObserverError::with_source("cannot list active projections", error))?;
    for row in rows {
        let row = row
            .map_err(|error| ObserverError::with_source("cannot read active projection", error))?;
        active_rows.insert(row.0.clone(), row);
    }
    if active_rows.len() != replayed.active_records.len() {
        return Err(ObserverError::invalid(
            "active projection does not exactly cover replayed state",
        ));
    }
    for (slot, record) in &replayed.active_records {
        let meta = replayed.active_metadata.get(slot).ok_or_else(|| {
            ObserverError::invalid(format!("missing replayed active metadata for {slot}"))
        })?;
        let expected_scope = meta
            .task_scope
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| ObserverError::with_source("cannot encode task scope", error))?;
        let expected_json = canonical_json(record)?;
        let Some((_, generation, digest, heartbeat, ttl, scope, encoded)) = active_rows.get(slot)
        else {
            return Err(ObserverError::invalid(
                "active projection does not exactly cover replayed state",
            ));
        };
        if generation != &meta.generation.to_string()
            || digest != &canonical_sha256(record)?
            || heartbeat != &meta.heartbeat_at
            || ttl != &meta.heartbeat_ttl_seconds.to_string()
            || scope != &expected_scope
            || encoded != &expected_json
        {
            return Err(ObserverError::invalid(
                "active projection differs from indexed event replay",
            ));
        }
    }

    let mut archives = BTreeMap::new();
    let mut statement = transaction
        .prepare("SELECT archive_id, slot, record_json FROM archive_records ORDER BY archive_id")
        .map_err(|error| {
            ObserverError::with_source("cannot prepare archive projection read", error)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| ObserverError::with_source("cannot list archive projections", error))?;
    for row in rows {
        let (archive_id, slot, encoded) = row
            .map_err(|error| ObserverError::with_source("cannot read archive projection", error))?;
        archives.insert(archive_id, (slot, encoded));
    }
    let mut expected_archives = BTreeMap::new();
    for record in &replayed.archive_records {
        let object = record
            .as_object()
            .ok_or_else(|| ObserverError::invalid("replayed archive is not an object"))?;
        let archive_id = object["archive_id"]
            .as_str()
            .ok_or_else(|| ObserverError::invalid("replayed archive has no archive_id"))?;
        let slot = object["slot"]
            .as_str()
            .ok_or_else(|| ObserverError::invalid("replayed archive has no slot"))?;
        expected_archives.insert(
            archive_id.to_owned(),
            (slot.to_owned(), canonical_json(record)?),
        );
    }
    if archives != expected_archives {
        return Err(ObserverError::invalid(
            "archive projection differs from indexed event replay",
        ));
    }

    let mut holds = BTreeMap::new();
    let mut statement = transaction
        .prepare("SELECT slot, generation, held_at, reason, event_sha256 FROM holds ORDER BY slot")
        .map_err(|error| {
            ObserverError::with_source("cannot prepare hold projection read", error)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| ObserverError::with_source("cannot list hold projections", error))?;
    for row in rows {
        let (slot, generation, held_at, reason, event_sha256) =
            row.map_err(|error| ObserverError::with_source("cannot read hold projection", error))?;
        let hold = SlotHold {
            slot,
            generation: parse_index_u64(&generation, "hold generation")?,
            held_at,
            reason,
            event_sha256,
        };
        holds.insert(hold.slot.clone(), hold);
    }
    if holds != replayed.holds {
        return Err(ObserverError::invalid(
            "hold projection differs from indexed event replay",
        ));
    }
    Ok(())
}

fn existing_index(transaction: &Transaction<'_>) -> Result<Option<ReplaySummary>, ObserverError> {
    let mut statement = transaction
        .prepare(
            "SELECT name, type FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' AND type IN ('table', 'view')
             ORDER BY name",
        )
        .map_err(|error| ObserverError::with_source("cannot inspect index schema", error))?;
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| ObserverError::with_source("cannot list index schema", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ObserverError::with_source("cannot read index schema", error))?;
    drop(statement);
    if objects.is_empty() {
        return Ok(None);
    }
    let actual = objects
        .iter()
        .filter(|(_, kind)| kind == "table")
        .map(|(name, _)| name.as_str())
        .collect::<BTreeSet<_>>();
    let legacy_tables = LEGACY_INDEX_TABLES.iter().copied().collect::<BTreeSet<_>>();
    let current_tables = INDEX_TABLES.iter().copied().collect::<BTreeSet<_>>();
    if (actual != legacy_tables && actual != current_tables)
        || objects.iter().any(|(_, kind)| kind != "table")
    {
        return Err(ObserverError::invalid(
            "existing database is not a compatible observer index",
        ));
    }
    let (
        schema,
        machine,
        replay_count,
        tip_sha256,
        active_revision,
        active_count,
        archive_revision,
        archive_count,
    ) = transaction
        .query_row(
            "SELECT schema_version, machine, replay_count, tip_sha256,
             active_revision, active_count, archive_revision, archive_count
             FROM observer_metadata WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, u64>(7)?,
                ))
            },
        )
        .map_err(|error| ObserverError::with_source("cannot read index metadata", error))?;
    let summary = ReplaySummary {
        machine,
        replay_count,
        tip_sha256,
        active_revision: parse_index_u64(&active_revision, "active revision")?,
        active_count,
        archive_revision: parse_index_u64(&archive_revision, "archive revision")?,
        archive_count,
    };
    let expected_tables = if schema == LEGACY_INDEX_SCHEMA {
        &legacy_tables
    } else if schema == INDEX_SCHEMA {
        &current_tables
    } else {
        return Err(ObserverError::invalid(format!(
            "unsupported derived index schema {schema}"
        )));
    };
    if &actual != expected_tables {
        return Err(ObserverError::invalid(format!(
            "derived index schema {schema} has incompatible tables"
        )));
    }
    if summary.replay_count == 0 || !is_sha256(&summary.tip_sha256) {
        return Err(ObserverError::invalid("derived index metadata is invalid"));
    }
    let event_count = table_count(transaction, "event_log")?;
    let active_count = table_count(transaction, "active_records")?;
    let archive_count = table_count(transaction, "archive_records")?;
    if (event_count, active_count, archive_count)
        != (
            summary.replay_count,
            summary.active_count,
            summary.archive_count,
        )
    {
        return Err(ObserverError::invalid(
            "derived index counters do not match its materialized rows",
        ));
    }
    let indexed_tip: String = transaction
        .query_row(
            "SELECT sha256 FROM event_log WHERE sequence = ?1",
            [to_sql_integer(summary.replay_count, "replay count")?],
            |row| row.get(0),
        )
        .map_err(|error| ObserverError::with_source("cannot read indexed tip", error))?;
    if indexed_tip != summary.tip_sha256 {
        return Err(ObserverError::invalid(
            "derived index metadata does not match its event tip",
        ));
    }
    if schema == INDEX_SCHEMA {
        validate_current_index(transaction, &summary)?;
    }
    Ok(Some(summary))
}

fn validate_current_index(
    transaction: &Transaction<'_>,
    summary: &ReplaySummary,
) -> Result<(), ObserverError> {
    let (
        config_sha256,
        config_json,
        evidence_sha256,
        evidence_json,
        evidence_observed_at,
        evaluated_at,
        max_plan_slots,
    ): PolicyMetadataRow = transaction
        .query_row(
            "SELECT config_sha256, config_json, evidence_sha256, evidence_json,
                    evidence_observed_at, evaluated_at, max_plan_slots
             FROM observer_metadata WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .map_err(|error| ObserverError::with_source("cannot read policy index metadata", error))?;
    let populated = [
        config_sha256.is_some(),
        config_json.is_some(),
        evidence_sha256.is_some(),
        evidence_json.is_some(),
        evidence_observed_at.is_some(),
        evaluated_at.is_some(),
        max_plan_slots.is_some(),
    ];
    if populated.iter().any(|value| *value) && !populated.iter().all(|value| *value) {
        return Err(ObserverError::invalid(
            "derived index policy inputs are only partially populated",
        ));
    }
    if config_sha256
        .as_deref()
        .is_some_and(|digest| !is_sha256(digest))
        || evidence_sha256
            .as_deref()
            .is_some_and(|digest| !is_sha256(digest))
    {
        return Err(ObserverError::invalid(
            "derived index policy digests are invalid",
        ));
    }
    let decision_count = table_count(transaction, "policy_decisions")?;
    if config_sha256.is_some() {
        let missing_active: u64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM active_records AS active
                 LEFT JOIN policy_decisions AS decision ON decision.slot = active.slot
                 WHERE decision.slot IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                ObserverError::with_source("cannot verify active policy coverage", error)
            })?;
        if missing_active != 0 {
            return Err(ObserverError::invalid(
                "derived index policy decisions do not cover every active row",
            ));
        }
    }
    if config_sha256.is_none() && decision_count != 0 {
        return Err(ObserverError::invalid(
            "derived index has decisions without policy input digests",
        ));
    }
    if config_sha256.is_some() && load_policy_inputs(transaction)?.is_none() {
        return Err(ObserverError::invalid(
            "derived index lost its canonical policy inputs",
        ));
    }
    let hold_count = table_count(transaction, "holds")?;
    if hold_count > summary.active_count {
        return Err(ObserverError::invalid(
            "derived index has more holds than active rows",
        ));
    }
    Ok(())
}

fn replace_schema(transaction: &Transaction<'_>) -> Result<(), ObserverError> {
    // Python accepts revisions through u64::MAX, while SQLite INTEGER is
    // signed. Canonical decimal TEXT preserves the complete authority domain.
    transaction
        .execute_batch(
            "DROP TABLE IF EXISTS policy_decisions;
             DROP TABLE IF EXISTS holds;
             DROP TABLE IF EXISTS observer_metadata;
             CREATE TABLE observer_metadata (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 schema_version INTEGER NOT NULL,
                 machine TEXT NOT NULL,
                 replay_count INTEGER NOT NULL,
                 tip_sha256 TEXT NOT NULL,
                 active_revision TEXT NOT NULL,
                 active_count INTEGER NOT NULL,
                 archive_revision TEXT NOT NULL,
                 archive_count INTEGER NOT NULL,
                 config_sha256 TEXT,
                 config_json TEXT,
                 evidence_sha256 TEXT,
                 evidence_json TEXT,
                 evidence_observed_at TEXT,
                 evaluated_at TEXT,
                 max_plan_slots INTEGER
             );
             DROP TABLE IF EXISTS event_log;
             CREATE TABLE event_log (
                 sequence INTEGER PRIMARY KEY,
                 machine TEXT NOT NULL,
                 previous_sha256 TEXT NOT NULL,
                 recorded_at TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 payload_json TEXT NOT NULL,
                 sha256 TEXT NOT NULL UNIQUE
             );
             DROP TABLE IF EXISTS active_records;
             CREATE TABLE active_records (
                 slot TEXT PRIMARY KEY,
                 generation TEXT NOT NULL,
                 record_sha256 TEXT NOT NULL UNIQUE,
                 heartbeat_at TEXT NOT NULL,
                 heartbeat_ttl_seconds TEXT NOT NULL,
                 scope_json TEXT,
                 record_json TEXT NOT NULL
             );
             DROP TABLE IF EXISTS archive_records;
             CREATE TABLE archive_records (
                 archive_id TEXT PRIMARY KEY,
                 slot TEXT NOT NULL UNIQUE,
                 record_json TEXT NOT NULL
             );
             DROP TABLE IF EXISTS holds;
             CREATE TABLE holds (
                 slot TEXT PRIMARY KEY REFERENCES active_records(slot),
                 generation TEXT NOT NULL,
                 held_at TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 event_sha256 TEXT NOT NULL
             );
             DROP TABLE IF EXISTS policy_decisions;
             CREATE TABLE policy_decisions (
                 slot TEXT PRIMARY KEY,
                 generation TEXT,
                 verdict TEXT NOT NULL CHECK (verdict IN ('ELIGIBLE', 'BLOCKED', 'UNKNOWN')),
                 active_record_sha256 TEXT,
                 event_sha256 TEXT NOT NULL,
                 config_sha256 TEXT NOT NULL,
                 evidence_sha256 TEXT NOT NULL,
                 decision_json TEXT NOT NULL
             );",
        )
        .map_err(|error| ObserverError::with_source("cannot replace index schema", error))
}

fn materialize_state(
    transaction: &Transaction<'_>,
    replayed: &ReplayedLog,
    decisions: Option<&[PolicyDecision]>,
) -> Result<(), ObserverError> {
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO active_records (
                    slot, generation, record_sha256, heartbeat_at,
                    heartbeat_ttl_seconds, scope_json, record_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|error| {
                ObserverError::with_source("cannot prepare active record indexing", error)
            })?;
        for (slot, record) in &replayed.active_records {
            let meta = replayed.active_metadata.get(slot).ok_or_else(|| {
                ObserverError::invalid(format!("missing active metadata for {slot}"))
            })?;
            statement
                .execute(params![
                    slot,
                    meta.generation.to_string(),
                    crate::canonical::canonical_sha256(record)?,
                    meta.heartbeat_at,
                    meta.heartbeat_ttl_seconds.to_string(),
                    meta.task_scope
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(|error| ObserverError::with_source(
                            "cannot encode task scope identity",
                            error,
                        ))?,
                    canonical_json(record)?,
                ])
                .map_err(|error| {
                    ObserverError::with_source("cannot index an active record", error)
                })?;
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO archive_records (archive_id, slot, record_json) VALUES (?1, ?2, ?3)",
            )
            .map_err(|error| {
                ObserverError::with_source("cannot prepare archive record indexing", error)
            })?;
        for record in &replayed.archive_records {
            let object = record.as_object().ok_or_else(|| {
                ObserverError::invalid("replayed archive record is not an object")
            })?;
            let archive_id = object["archive_id"].as_str().ok_or_else(|| {
                ObserverError::invalid("replayed archive record has no archive_id")
            })?;
            let slot = object["slot"]
                .as_str()
                .ok_or_else(|| ObserverError::invalid("replayed archive record has no slot"))?;
            statement
                .execute(params![archive_id, slot, canonical_json(record)?])
                .map_err(|error| {
                    ObserverError::with_source("cannot index an archive record", error)
                })?;
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO holds (
                    slot, generation, held_at, reason, event_sha256
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|error| ObserverError::with_source("cannot prepare hold indexing", error))?;
        for hold in replayed.holds.values() {
            statement
                .execute(params![
                    hold.slot,
                    hold.generation.to_string(),
                    hold.held_at,
                    hold.reason,
                    hold.event_sha256,
                ])
                .map_err(|error| ObserverError::with_source("cannot index a hold", error))?;
        }
    }
    if let Some(decisions) = decisions {
        let mut statement = transaction
            .prepare(
                "INSERT INTO policy_decisions (
                    slot, generation, verdict, active_record_sha256,
                    event_sha256, config_sha256, evidence_sha256, decision_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .map_err(|error| {
                ObserverError::with_source("cannot prepare policy decision indexing", error)
            })?;
        for decision in decisions {
            statement
                .execute(params![
                    decision.slot,
                    decision.generation.map(|value| value.to_string()),
                    verdict_name(decision.verdict),
                    decision.active_record_sha256,
                    decision.event_sha256,
                    decision.config_sha256,
                    decision.evidence_sha256,
                    serde_json::to_string(decision).map_err(|error| {
                        ObserverError::with_source("cannot encode policy decision", error)
                    })?,
                ])
                .map_err(|error| {
                    ObserverError::with_source("cannot index a policy decision", error)
                })?;
        }
    }
    Ok(())
}

fn write_summary(
    transaction: &Transaction<'_>,
    summary: &ReplaySummary,
    policy: Option<&PolicyInputs>,
    evaluated_at: Option<&str>,
) -> Result<(), ObserverError> {
    transaction
        .execute(
            "INSERT INTO observer_metadata (
                singleton, schema_version, machine, replay_count, tip_sha256,
                active_revision, active_count, archive_revision, archive_count,
                config_sha256, config_json, evidence_sha256, evidence_json,
                evidence_observed_at, evaluated_at, max_plan_slots
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                to_sql_integer(INDEX_SCHEMA, "index schema")?,
                summary.machine,
                to_sql_integer(summary.replay_count, "replay count")?,
                summary.tip_sha256,
                summary.active_revision.to_string(),
                to_sql_integer(summary.active_count, "active count")?,
                summary.archive_revision.to_string(),
                to_sql_integer(summary.archive_count, "archive count")?,
                policy.map(|inputs| inputs.config_sha256.as_str()),
                policy.map(|inputs| inputs.config_json.as_str()),
                policy.map(|inputs| inputs.evidence_sha256.as_str()),
                policy.map(|inputs| inputs.evidence_json.as_str()),
                policy.map(|inputs| inputs.evidence.observed_at.as_str()),
                evaluated_at,
                policy
                    .map(|inputs| to_sql_integer(
                        inputs.config.max_plan_slots as u64,
                        "max plan slots"
                    ))
                    .transpose()?,
            ],
        )
        .map_err(|error| ObserverError::with_source("cannot write index metadata", error))?;
    Ok(())
}

fn table_count(connection: &Connection, table: &str) -> Result<u64, ObserverError> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    connection
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|error| ObserverError::with_source(format!("cannot count {table}"), error))
}

fn to_sql_integer(value: u64, label: &str) -> Result<i64, ObserverError> {
    i64::try_from(value)
        .map_err(|_| ObserverError::invalid(format!("{label} does not fit in SQLite INTEGER")))
}

fn parse_index_u64(value: &str, label: &str) -> Result<u64, ObserverError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| ObserverError::invalid(format!("derived index {label} is invalid")))?;
    if parsed.to_string() != value {
        return Err(ObserverError::invalid(format!(
            "derived index {label} is not canonical"
        )));
    }
    Ok(parsed)
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Eligible => "ELIGIBLE",
        Verdict::Blocked => "BLOCKED",
        Verdict::Unknown => "UNKNOWN",
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn inspect_index_path(path: &Path) -> Result<Option<FileIdentity>, ObserverError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ObserverError::with_source(
                format!("cannot inspect derived index {}", path.display()),
                error,
            ))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ObserverError::invalid(format!(
            "derived index is not a real regular file: {}",
            path.display()
        )));
    }
    if metadata.nlink() != 1 {
        return Err(ObserverError::invalid(format!(
            "derived index has multiple hard links: {}",
            path.display()
        )));
    }
    Ok(Some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

fn verify_opened_index(path: &Path, before: Option<FileIdentity>) -> Result<(), ObserverError> {
    let after = inspect_index_path(path)?.ok_or_else(|| {
        ObserverError::invalid(format!(
            "derived index disappeared while opening: {}",
            path.display()
        ))
    })?;
    if before.is_some_and(|before| before.device != after.device || before.inode != after.inode) {
        return Err(ObserverError::invalid(format!(
            "derived index identity changed while opening: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_index_in_event_directory(
    events_dir: &Path,
    index_path: &Path,
) -> Result<(), ObserverError> {
    let event_directory = events_dir.canonicalize().map_err(|error| {
        ObserverError::with_source(
            format!("cannot resolve event directory {}", events_dir.display()),
            error,
        )
    })?;
    let resolved_index = resolve_target(index_path)?;
    if resolved_index.starts_with(&event_directory) {
        return Err(ObserverError::invalid(format!(
            "derived index must not be placed in the event directory: {}",
            index_path.display()
        )));
    }
    Ok(())
}

fn resolve_target(path: &Path) -> Result<PathBuf, ObserverError> {
    if path.exists() {
        return path.canonicalize().map_err(|error| {
            ObserverError::with_source(format!("cannot resolve {}", path.display()), error)
        });
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let resolved_parent = parent.canonicalize().map_err(|error| {
        ObserverError::with_source(
            format!("cannot resolve index parent {}", parent.display()),
            error,
        )
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| ObserverError::invalid("derived index path has no file name"))?;
    Ok(resolved_parent.join(name))
}
