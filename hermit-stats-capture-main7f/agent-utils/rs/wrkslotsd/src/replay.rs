use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use serde::de::IgnoredAny;
use serde_json::{Map, Value};

use crate::canonical::{canonical_json, canonical_sha256};
use crate::config::is_sha256;
use crate::schema::{
    exact_keys, exact_keys_optional, parse_timestamp, validate_active_record,
    validate_archive_record, ActiveRecordMeta,
};
use crate::ObserverError;

const EVENT_SCHEMA: u64 = 1;
const STATE_SCHEMA: u64 = 2;
const ZERO_DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";
// State-import events contain full snapshots, so the bound leaves substantial
// room above ordinary events while guaranteeing one corrupt file cannot force
// unbounded allocation. Event rows themselves are streamed into SQLite.
pub(crate) const MAX_EVENT_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_JSON_CONTAINER_DEPTH: usize = 127;

const NON_STATE_EVENT_KINDS: &[&str] = &[
    "handoff-read",
    "handoff-removed",
    "handoff-write-completed",
    "handoff-write-intended",
    "handoff-written",
    "legacy-validate-checkout-removed",
    "owner-released",
    "ownerless-agent-cache-relocated",
    "ownerless-agent-worktree-removed",
    "ownerless-validate-path-removed",
    "partial-updates-recovered",
];

/// The replay tip and materialized active/archive counters for one machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySummary {
    /// Machine shard encoded by the `EVENTS.<machine>` directory name.
    pub machine: String,
    /// Number of validated events in the hash chain.
    pub replay_count: u64,
    /// SHA-256 digest of the final validated event.
    pub tip_sha256: String,
    /// Revision reached by replaying active-state events.
    pub active_revision: u64,
    /// Number of active records after replay.
    pub active_count: u64,
    /// Revision reached by replaying archive-state events.
    pub archive_revision: u64,
    /// Number of archived records after replay.
    pub archive_count: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub(crate) machine: String,
    pub(crate) sequence: u64,
    pub(crate) previous_sha256: String,
    pub(crate) recorded_at: String,
    pub(crate) kind: String,
    pub(crate) payload: Value,
    pub(crate) sha256: String,
}

#[derive(Debug)]
pub(crate) struct ReplayedLog {
    pub(crate) summary: ReplaySummary,
    pub(crate) tip_recorded_at: String,
    pub(crate) active_records: BTreeMap<String, Value>,
    pub(crate) active_metadata: BTreeMap<String, ActiveRecordMeta>,
    pub(crate) archive_records: Vec<Value>,
    pub(crate) holds: BTreeMap<String, SlotHold>,
    pub(crate) pending_operations: Vec<PendingOperation>,
}

/// One event-derived operation that has begun but has no durable completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingOperation {
    pub(crate) slot: String,
    pub(crate) generation: Option<u64>,
    pub(crate) kind: PendingOperationKind,
    pub(crate) operation: Option<String>,
    pub(crate) journal_path: Option<String>,
    pub(crate) journal_sha256: Option<String>,
    pub(crate) event_sha256: String,
    /// Storage a create or import journal places, carried from the journal to
    /// the recovery attempt that inherits it.
    storage: Option<AttemptStorage>,
}

/// The slot type and checkout paths one create or import attempt placed or
/// planned. Python derives every checkout path from the slot type's root, so
/// a slot name alone does not locate an attempt's storage.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AttemptStorage {
    slot_type: String,
    paths: BTreeSet<String>,
}

impl AttemptStorage {
    /// Read the storage an embedded create or import journal describes.
    /// `None` means the journal does not identify it, so no later row can
    /// prove that storage owned. Python treats an absent `slot_type` as
    /// `agent` in both journal kinds. An import places no files, and a
    /// historical import whose checkouts are all missing records none, so
    /// only a create must name at least one path.
    fn from_journal(operation: &str, journal: &Map<String, Value>) -> Option<Self> {
        let (holder, lists, may_be_empty): (&Map<String, Value>, &[(&str, &str)], bool) =
            match operation {
                "create" => (journal, &[("planned", "destination")], false),
                "import-existing" => (
                    journal.get("record")?.as_object()?,
                    &[("checkouts", "path")],
                    true,
                ),
                _ => return None,
            };
        let slot_type = match holder.get("slot_type") {
            None => "agent",
            Some(value) => value.as_str()?,
        };
        let mut paths = BTreeSet::new();
        for (list, field) in lists {
            for item in holder.get(*list)?.as_array()? {
                paths.insert(item.as_object()?.get(*field)?.as_str()?.to_owned());
            }
        }
        (may_be_empty || !paths.is_empty()).then(|| Self {
            slot_type: slot_type.to_owned(),
            paths,
        })
    }

    /// Storage of two attempts recovered through one journal path. A row must
    /// own both, and no row owns attempts under different slot types.
    fn merge(older: Option<Self>, newer: Option<Self>) -> Option<Self> {
        let (mut older, newer) = (older?, newer?);
        (older.slot_type == newer.slot_type).then(|| {
            older.paths.extend(newer.paths);
            older
        })
    }

    /// Whether `row` now owns every path of this attempt under the same slot
    /// type.
    fn owned_by(&self, row: &ActiveRecordMeta) -> bool {
        row.slot_type == self.slot_type
            && self
                .paths
                .iter()
                .all(|path| row.checkouts.iter().any(|(_, owned)| owned == path))
    }
}

/// Stable classes of unfinished lifecycle work understood by the observer.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PendingOperationKind {
    Journal,
    Reclaim,
    Recovery,
    Retirement,
}

/// A validated hold bound to one active slot generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SlotHold {
    pub(crate) slot: String,
    pub(crate) generation: u64,
    pub(crate) held_at: String,
    pub(crate) reason: String,
    pub(crate) event_sha256: String,
}

#[derive(Debug)]
struct ActiveEntry {
    value: Value,
    meta: ActiveRecordMeta,
}

#[derive(Default)]
struct State {
    active_revision: Option<u64>,
    active_records: BTreeMap<String, ActiveEntry>,
    active_agents: BTreeMap<String, String>,
    archive_revision: Option<u64>,
    archive_records: Vec<Value>,
    archive_ids: BTreeSet<String>,
    archived_generations: BTreeMap<String, u64>,
    holds: BTreeMap<String, SlotHold>,
    pending_operations: BTreeMap<String, PendingOperation>,
    completed_operations: BTreeMap<String, CompletedOperation>,
}

/// The last completion recorded at one journal path.
struct CompletedOperation {
    slot: String,
    operation: String,
    /// Event sequence of the completion, so recovery can bind the journal
    /// that completed most recently rather than the path that sorts first.
    sequence: u64,
    /// Storage of the completed journal, for a recovery that loads it again.
    storage: Option<AttemptStorage>,
}

impl CompletedOperation {
    fn is(&self, slot: &str, operation: &str) -> bool {
        self.slot == slot && self.operation == operation
    }
}

pub(crate) struct ReplayGuard<'a> {
    pub(crate) replay_count: u64,
    pub(crate) tip_sha256: &'a str,
}

/// Validate and replay one `EVENTS.<machine>` directory without changing it.
pub fn replay(events_dir: &Path) -> Result<ReplaySummary, ObserverError> {
    replay_stream(events_dir, None, |_| Ok(())).map(|replayed| replayed.summary)
}

pub(crate) fn replay_stream(
    events_dir: &Path,
    guard: Option<ReplayGuard<'_>>,
    consume: impl FnMut(&Event) -> Result<(), ObserverError>,
) -> Result<ReplayedLog, ObserverError> {
    replay_stream_after_count(events_dir, guard, || {}, consume)
}

#[cfg(test)]
pub(crate) fn replay_with_post_count_hook(
    events_dir: &Path,
    after_count: impl FnOnce(),
) -> Result<ReplaySummary, ObserverError> {
    replay_stream_after_count(events_dir, None, after_count, |_| Ok(()))
        .map(|replayed| replayed.summary)
}

fn replay_stream_after_count(
    events_dir: &Path,
    guard: Option<ReplayGuard<'_>>,
    after_count: impl FnOnce(),
    mut consume: impl FnMut(&Event) -> Result<(), ObserverError>,
) -> Result<ReplayedLog, ObserverError> {
    let machine = machine_from_directory(events_dir)?;
    let event_count = event_count(events_dir)?;
    if event_count == 0 {
        return Err(ObserverError::invalid(format!(
            "event log {} is empty and has no imported state",
            events_dir.display()
        )));
    }
    if let Some(guard) = &guard {
        if event_count < guard.replay_count {
            return Err(ObserverError::invalid(format!(
                "refusing to replace a {0}-event index with an older {event_count}-event replay",
                guard.replay_count
            )));
        }
    }
    after_count();

    let mut previous = ZERO_DIGEST.to_owned();
    let mut tip_recorded_at = None;
    let mut state = State::default();
    for expected_sequence in 1..=event_count {
        let path = events_dir.join(format!("{expected_sequence:020}.json"));
        let event = load_event(&path, &machine, expected_sequence, &previous)?;
        if guard
            .as_ref()
            .is_some_and(|guard| guard.replay_count == expected_sequence)
            && guard
                .as_ref()
                .is_some_and(|guard| guard.tip_sha256 != event.sha256)
        {
            return Err(ObserverError::invalid(
                "event replay does not extend the indexed hash-chain tip",
            ));
        }
        apply_event(&event, &mut state)?;
        consume(&event)?;
        previous.clone_from(&event.sha256);
        tip_recorded_at = Some(event.recorded_at);
    }

    finish_replay(
        machine,
        event_count,
        previous,
        tip_recorded_at
            .ok_or_else(|| ObserverError::invalid("event log has no terminal timestamp"))?,
        state,
    )
}

/// Revalidate and replay canonical event rows retained in a disposable index.
///
/// This keeps policy reads dependent on the indexed event chain rather than on
/// independently editable active/hold projection rows.
pub(crate) fn replay_indexed_events(
    machine: &str,
    events: Vec<Event>,
) -> Result<ReplayedLog, ObserverError> {
    validate_name(machine, "indexed event machine")?;
    if events.is_empty() {
        return Err(ObserverError::invalid("indexed event log is empty"));
    }
    let mut previous = ZERO_DIGEST.to_owned();
    let mut tip_recorded_at = None;
    let mut state = State::default();
    for (offset, event) in events.iter().enumerate() {
        let expected_sequence = u64::try_from(offset)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| ObserverError::invalid("indexed event sequence overflow"))?;
        if event.sequence != expected_sequence
            || event.machine != machine
            || event.previous_sha256 != previous
            || event.kind.is_empty()
            || !event.payload.is_object()
        {
            return Err(ObserverError::invalid(format!(
                "indexed event envelope or hash chain is invalid at sequence {expected_sequence}"
            )));
        }
        parse_timestamp(&event.recorded_at, "indexed event.recorded_at")?;
        let core = serde_json::json!({
            "schema": EVENT_SCHEMA,
            "machine": event.machine,
            "sequence": event.sequence,
            "previous_sha256": event.previous_sha256,
            "recorded_at": event.recorded_at,
            "kind": event.kind,
            "payload": event.payload,
        });
        if canonical_sha256(&core)? != event.sha256 {
            return Err(ObserverError::invalid(format!(
                "indexed event digest is invalid at sequence {expected_sequence}"
            )));
        }
        apply_event(event, &mut state)?;
        previous.clone_from(&event.sha256);
        tip_recorded_at = Some(event.recorded_at.clone());
    }
    finish_replay(
        machine.to_owned(),
        u64::try_from(events.len())
            .map_err(|_| ObserverError::invalid("indexed event count does not fit u64"))?,
        previous,
        tip_recorded_at
            .ok_or_else(|| ObserverError::invalid("indexed event log has no terminal timestamp"))?,
        state,
    )
}

fn finish_replay(
    machine: String,
    event_count: u64,
    tip_sha256: String,
    tip_recorded_at: String,
    state: State,
) -> Result<ReplayedLog, ObserverError> {
    let active_revision = state
        .active_revision
        .ok_or_else(|| ObserverError::invalid("event log has no complete imported active state"))?;
    let archive_revision = state.archive_revision.ok_or_else(|| {
        ObserverError::invalid("event log has no complete imported archive state")
    })?;
    let active_count = u64::try_from(state.active_records.len())
        .map_err(|_| ObserverError::invalid("active record count does not fit in u64"))?;
    let archive_count = u64::try_from(state.archive_records.len())
        .map_err(|_| ObserverError::invalid("archive record count does not fit in u64"))?;
    let active_metadata = state
        .active_records
        .iter()
        .map(|(slot, entry)| (slot.clone(), entry.meta.clone()))
        .collect();
    Ok(ReplayedLog {
        summary: ReplaySummary {
            machine,
            replay_count: event_count,
            tip_sha256,
            active_revision,
            active_count,
            archive_revision,
            archive_count,
        },
        tip_recorded_at,
        active_records: state
            .active_records
            .into_iter()
            .map(|(slot, entry)| (slot, entry.value))
            .collect(),
        active_metadata,
        archive_records: state.archive_records,
        holds: state.holds,
        pending_operations: state.pending_operations.into_values().collect(),
    })
}

fn machine_from_directory(events_dir: &Path) -> Result<String, ObserverError> {
    let metadata = fs::symlink_metadata(events_dir).map_err(|error| {
        ObserverError::with_source(
            format!("cannot inspect event directory {}", events_dir.display()),
            error,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ObserverError::invalid(format!(
            "event log is not a real directory: {}",
            events_dir.display()
        )));
    }
    let name = events_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ObserverError::invalid("event directory name is not UTF-8"))?;
    let machine = name.strip_prefix("EVENTS.").ok_or_else(|| {
        ObserverError::invalid(format!(
            "event directory must be named EVENTS.<machine>: {}",
            events_dir.display()
        ))
    })?;
    validate_name(machine, "machine")?;
    Ok(machine.to_owned())
}

fn event_count(events_dir: &Path) -> Result<u64, ObserverError> {
    let entries = fs::read_dir(events_dir).map_err(|error| {
        ObserverError::with_source(
            format!("cannot read event directory {}", events_dir.display()),
            error,
        )
    })?;
    let mut complete = 0_u64;
    let mut minimum = u64::MAX;
    let mut maximum = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|error| {
            ObserverError::with_source(
                format!("cannot read an entry in {}", events_dir.display()),
                error,
            )
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            ObserverError::invalid(format!(
                "event log contains a non-UTF-8 entry in {}",
                events_dir.display()
            ))
        })?;
        if is_event_filename(&name) {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                ObserverError::with_source(
                    format!("cannot inspect event file {}", entry.path().display()),
                    error,
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ObserverError::invalid(format!(
                    "event is not a real file: {}",
                    entry.path().display()
                )));
            }
            let sequence = name[..20].parse::<u64>().map_err(|_| {
                ObserverError::invalid(format!(
                    "event filename has an out-of-range sequence: {}",
                    entry.path().display()
                ))
            })?;
            complete = complete
                .checked_add(1)
                .ok_or_else(|| ObserverError::invalid("event file count overflow"))?;
            minimum = minimum.min(sequence);
            maximum = maximum.max(sequence);
        } else if !is_temporary_event_filename(&name) {
            return Err(ObserverError::invalid(format!(
                "event log contains an unexpected entry: {}",
                entry.path().display()
            )));
        }
    }
    if complete != 0 && (minimum != 1 || maximum != complete) {
        return Err(ObserverError::invalid(format!(
            "event log has a sequence gap in {}",
            events_dir.display()
        )));
    }
    Ok(complete)
}

fn is_event_filename(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 25
        && bytes[..20].iter().all(u8::is_ascii_digit)
        && bytes.get(20..) == Some(b".json")
}

fn is_temporary_event_filename(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 30
        && bytes[..20].iter().all(u8::is_ascii_digit)
        && bytes.get(20..30) == Some(b".json.tmp.")
}

fn load_event(
    path: &Path,
    machine: &str,
    expected_sequence: u64,
    previous: &str,
) -> Result<Event, ObserverError> {
    let expected_name = format!("{expected_sequence:020}.json");
    if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err(ObserverError::invalid(format!(
            "event log has a sequence gap at {}",
            path.display()
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        // A file that was regular during directory enumeration can be
        // replaced by a FIFO before this open.  O_NONBLOCK makes that race
        // fail closed in the post-open regular-file check instead of hanging.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            ObserverError::with_source(format!("cannot open event {}", path.display()), error)
        })?;
    if !file
        .metadata()
        .map_err(|error| {
            ObserverError::with_source(format!("cannot inspect event {}", path.display()), error)
        })?
        .is_file()
    {
        return Err(ObserverError::invalid(format!(
            "event is not a regular file: {}",
            path.display()
        )));
    }
    let mut contents = Vec::new();
    file.take(MAX_EVENT_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| {
            ObserverError::with_source(format!("cannot read event {}", path.display()), error)
        })?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_EVENT_BYTES {
        return Err(ObserverError::invalid(format!(
            "event exceeds the {MAX_EVENT_BYTES}-byte replay limit: {}",
            path.display()
        )));
    }
    validate_and_normalize_json_domain(&mut contents, path)?;
    let value: Value = serde_json::from_slice(&contents).map_err(|error| {
        ObserverError::with_source(format!("cannot parse event {}", path.display()), error)
    })?;
    let object = value.as_object().ok_or_else(|| {
        ObserverError::invalid(format!("event {} must be a JSON object", path.display()))
    })?;
    exact_keys(
        object,
        &[
            "schema",
            "machine",
            "sequence",
            "previous_sha256",
            "recorded_at",
            "kind",
            "payload",
            "sha256",
        ],
        "event envelope",
    )?;
    if unsigned(&object["schema"], "event.schema")? != EVENT_SCHEMA {
        return Err(ObserverError::invalid(format!(
            "unsupported event schema in {}",
            path.display()
        )));
    }
    if string(&object["machine"], "event.machine")? != machine {
        return Err(ObserverError::invalid(format!(
            "event belongs to another machine: {}",
            path.display()
        )));
    }
    let sequence = unsigned(&object["sequence"], "event.sequence")?;
    if sequence != expected_sequence {
        return Err(ObserverError::invalid(format!(
            "event sequence does not match its filename: {}",
            path.display()
        )));
    }
    let recorded_previous = string(&object["previous_sha256"], "event.previous_sha256")?;
    if recorded_previous != previous {
        return Err(ObserverError::invalid(format!(
            "event hash chain is broken at {}",
            path.display()
        )));
    }
    let recorded_at = string(&object["recorded_at"], "event.recorded_at")?;
    parse_timestamp(recorded_at, "event.recorded_at")?;
    let kind = string(&object["kind"], "event.kind")?;
    if kind.is_empty() {
        return Err(ObserverError::invalid("event.kind must not be empty"));
    }
    if !object["payload"].is_object() {
        return Err(ObserverError::invalid(
            "event.payload must be a JSON object",
        ));
    }
    let recorded_digest = string(&object["sha256"], "event.sha256")?;
    let mut core = object.clone();
    core.remove("sha256");
    let computed_digest = canonical_sha256(&Value::Object(core))?;
    if recorded_digest != computed_digest {
        return Err(ObserverError::invalid(format!(
            "event digest does not match its content: {}",
            path.display()
        )));
    }
    Ok(Event {
        machine: machine.to_owned(),
        sequence,
        previous_sha256: recorded_previous.to_owned(),
        recorded_at: recorded_at.to_owned(),
        kind: kind.to_owned(),
        payload: object["payload"].clone(),
        sha256: recorded_digest.to_owned(),
    })
}

/// Enforce the JSON subset that can be represented and walked safely before
/// decoding. The Python authority accepts a wider language, but current event
/// schemas use shallow structures and non-negative integers. The observer
/// additionally accepts up to [`MAX_JSON_CONTAINER_DEPTH`] nested containers,
/// exact signed/unsigned 64-bit integers, and finite binary64 floats, whose
/// canonical spelling is handled by `canonical::python_number`.
///
/// Python parses the integer spelling `-0` as integer zero, while `serde_json`
/// classifies it as negative floating zero. Replacing its minus sign with JSON
/// whitespace before decoding preserves the authoritative value without
/// changing the event file. Malformed number candidates are left untouched so
/// the JSON parser, rather than this domain check, classifies their grammar.
fn validate_and_normalize_json_domain(
    contents: &mut [u8],
    path: &Path,
) -> Result<(), ObserverError> {
    validate_json_structure_domain(contents, path)?;
    serde_json::from_slice::<IgnoredAny>(contents).map_err(|error| {
        ObserverError::with_source(format!("cannot parse event {}", path.display()), error)
    })?;

    normalize_json_numbers(contents, path)
}

fn validate_json_structure_domain(contents: &[u8], path: &Path) -> Result<(), ObserverError> {
    let mut offset = 0;
    let mut depth = 0_usize;
    while offset < contents.len() {
        match contents[offset] {
            b'"' => {
                offset += 1;
                while offset < contents.len() {
                    match contents[offset] {
                        b'\\' if contents.get(offset + 1) == Some(&b'u') => {
                            let Some(unit) = unicode_escape(contents, offset) else {
                                offset = (offset + 2).min(contents.len());
                                continue;
                            };
                            if (0xd800..=0xdbff).contains(&unit) {
                                let second = offset + 6;
                                if unicode_escape(contents, second)
                                    .is_some_and(|unit| (0xdc00..=0xdfff).contains(&unit))
                                {
                                    offset += 12;
                                    continue;
                                }
                                return Err(ObserverError::invalid(format!(
                                    "event contains a lone Unicode surrogate escape at byte {offset}: {}",
                                    path.display()
                                )));
                            }
                            if (0xdc00..=0xdfff).contains(&unit) {
                                return Err(ObserverError::invalid(format!(
                                    "event contains a lone Unicode surrogate escape at byte {offset}: {}",
                                    path.display()
                                )));
                            }
                            offset += 6;
                        }
                        b'\\' => offset = (offset + 2).min(contents.len()),
                        b'"' => {
                            offset += 1;
                            break;
                        }
                        _ => offset += 1,
                    }
                }
            }
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_JSON_CONTAINER_DEPTH {
                    return Err(ObserverError::invalid(format!(
                        "event exceeds the {MAX_JSON_CONTAINER_DEPTH}-container JSON nesting limit: {}",
                        path.display()
                    )));
                }
                offset += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                offset += 1;
            }
            _ => offset += 1,
        }
    }
    Ok(())
}

fn normalize_json_numbers(contents: &mut [u8], path: &Path) -> Result<(), ObserverError> {
    let mut offset = 0;
    while offset < contents.len() {
        match contents[offset] {
            b'"' => {
                offset += 1;
                while offset < contents.len() {
                    match contents[offset] {
                        b'\\' => offset = (offset + 2).min(contents.len()),
                        b'"' => {
                            offset += 1;
                            break;
                        }
                        _ => offset += 1,
                    }
                }
            }
            byte @ (b'-' | b'0'..=b'9')
                if byte != b'-' || contents.get(offset + 1).is_some_and(u8::is_ascii_digit) =>
            {
                let start = offset;
                offset += 1;
                while offset < contents.len()
                    && matches!(
                        contents[offset],
                        b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                    )
                {
                    offset += 1;
                }
                let token = std::str::from_utf8(&contents[start..offset]).map_err(|error| {
                    ObserverError::with_source(
                        format!(
                            "event contains an invalid number at byte {start}: {}",
                            path.display()
                        ),
                        error,
                    )
                })?;
                if !is_json_number(token) {
                    continue;
                }
                if token.contains(['.', 'e', 'E']) {
                    if !token.parse::<f64>().is_ok_and(f64::is_finite) {
                        return Err(ObserverError::invalid(format!(
                            "event contains a non-finite number at byte {start}: {}",
                            path.display()
                        )));
                    }
                } else {
                    let exact = if token.starts_with('-') {
                        token.parse::<i64>().is_ok()
                    } else {
                        token.parse::<u64>().is_ok()
                    };
                    if !exact {
                        return Err(ObserverError::invalid(format!(
                            "event contains an integer outside the lossless observer domain at byte {start}: {}",
                            path.display()
                        )));
                    }
                    if token == "-0" {
                        contents[start] = b' ';
                    }
                }
            }
            _ => offset += 1,
        }
    }
    Ok(())
}

fn is_json_number(token: &str) -> bool {
    let bytes = token.as_bytes();
    let mut offset = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(offset) {
        Some(b'0') => offset += 1,
        Some(b'1'..=b'9') => {
            offset += 1;
            while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                offset += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(offset) == Some(&b'.') {
        offset += 1;
        let fraction_start = offset;
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
        if offset == fraction_start {
            return false;
        }
    }
    if matches!(bytes.get(offset), Some(b'e' | b'E')) {
        offset += 1;
        if matches!(bytes.get(offset), Some(b'+' | b'-')) {
            offset += 1;
        }
        let exponent_start = offset;
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
        if offset == exponent_start {
            return false;
        }
    }
    offset == bytes.len()
}

fn unicode_escape(contents: &[u8], offset: usize) -> Option<u16> {
    if contents.get(offset..offset + 2)? != b"\\u" {
        return None;
    }
    let digits = contents.get(offset + 2..offset + 6)?;
    digits.iter().try_fold(0_u16, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => u16::from(byte - b'0'),
            b'a'..=b'f' => u16::from(byte - b'a') + 10,
            b'A'..=b'F' => u16::from(byte - b'A') + 10,
            _ => return None,
        };
        Some(value * 16 + digit)
    })
}

fn apply_event(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    match event.kind.as_str() {
        "state-imported" => apply_import(event, state),
        "active-state-recorded" => apply_active(event, state),
        "archive-state-recorded" => apply_archive(event, state),
        "slot-held" => apply_hold(event, state),
        "slot-hold-released" => apply_hold_release(event, state),
        "operation-progress-recorded" => apply_operation_progress(event, state),
        "operation-completed" => apply_operation_completed(event, state),
        "reclaim-started" => apply_reclaim_started(event, state),
        "recovery-started" => apply_recovery_started(event, state),
        "retirement-attempted" => apply_retirement_attempted(event, state),
        kind if NON_STATE_EVENT_KINDS.contains(&kind) => Ok(()),
        kind => Err(ObserverError::invalid(format!(
            "unknown event kind {kind:?} at sequence {}",
            event.sequence
        ))),
    }
}

const JOURNAL_OPERATIONS: &[&str] = &[
    "create",
    "finish",
    "import-existing",
    "legacy-validate-remove",
    "ownerless-validate-remove",
    "ownerless-agent-remove",
    "ownerless-agent-cache-relocate",
    "recover-absent-validate-rows",
    "recover-absent-agent-row",
];

/// Operations whose journal completion is terminal for a recovery attempt when
/// the slot has an ACTIVE row. Finish and the ownerless cleanups also complete
/// journals on rollback or refusal paths that keep their storage, so they are
/// excluded.
const TERMINAL_COMPLETION_OPERATIONS: &[&str] = &["create", "import-existing"];

fn active_generation(state: &State, slot: &str) -> Option<u64> {
    state
        .active_records
        .get(slot)
        .map(|entry| entry.meta.generation)
}

fn pending_key(kind: PendingOperationKind, slot: &str, suffix: &str) -> String {
    format!("{kind:?}\0{slot}\0{suffix}")
}

/// Recovery markers are distinct per bound journal, so a later attempt
/// recovered through another journal cannot replace an open one.
fn recovery_suffix(operation: &str, journal_path: &str) -> String {
    format!("{operation}\0{journal_path}")
}

fn insert_pending(state: &mut State, suffix: &str, pending: PendingOperation) {
    state
        .pending_operations
        .insert(pending_key(pending.kind, &pending.slot, suffix), pending);
}

fn validate_operation(operation: &str, label: &str) -> Result<(), ObserverError> {
    if JOURNAL_OPERATIONS.contains(&operation) {
        Ok(())
    } else {
        Err(ObserverError::invalid(format!(
            "{label} is not a known operation: {operation:?}"
        )))
    }
}

fn expected_journal_path(machine: &str, slot: &str, operation: &str, path: &str) -> String {
    let singleton = format!("ACTIVE.{machine}.journal");
    if operation == "create" && path.starts_with("CREATE.") {
        format!(
            "CREATE.{}.{machine}.{}.{slot}.journal",
            machine.len(),
            slot.len()
        )
    } else if operation == "finish" && path.starts_with("FINISH.") {
        format!(
            "FINISH.{}.{machine}.{}.{slot}.journal",
            machine.len(),
            slot.len()
        )
    } else {
        singleton
    }
}

fn operation_identity(
    payload: &Map<String, Value>,
    machine: &str,
    label: &str,
) -> Result<(String, String, String), ObserverError> {
    let slot = string(&payload["slot"], &format!("{label}.slot"))?;
    validate_name(slot, &format!("{label}.slot"))?;
    let operation = string(&payload["operation"], &format!("{label}.operation"))?;
    validate_operation(operation, &format!("{label}.operation"))?;
    let default_path = format!("ACTIVE.{machine}.journal");
    let journal_path = payload
        .get("journal_path")
        .map(|value| string(value, &format!("{label}.journal_path")))
        .transpose()?
        .unwrap_or(&default_path);
    if journal_path.contains('/')
        || journal_path != expected_journal_path(machine, slot, operation, journal_path)
    {
        return Err(ObserverError::invalid(format!(
            "{label}.journal_path does not match its machine, slot, and operation"
        )));
    }
    Ok((
        slot.to_owned(),
        operation.to_owned(),
        journal_path.to_owned(),
    ))
}

fn apply_operation_progress(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "operation-progress-recorded payload")?;
    exact_keys_optional(
        payload,
        &["slot", "operation", "journal"],
        &["journal_path"],
        "operation-progress-recorded payload",
    )?;
    let (slot, operation, journal_path) = operation_identity(
        payload,
        &event.machine,
        "operation-progress-recorded payload",
    )?;
    let journal = object(
        &payload["journal"],
        "operation-progress-recorded payload.journal",
    )?;
    if string(
        journal
            .get("machine")
            .ok_or_else(|| ObserverError::invalid("operation journal has no machine"))?,
        "operation journal.machine",
    )? != event.machine
        || string(
            journal
                .get("slot")
                .ok_or_else(|| ObserverError::invalid("operation journal has no slot"))?,
            "operation journal.slot",
        )? != slot
        || string(
            journal
                .get("kind")
                .ok_or_else(|| ObserverError::invalid("operation journal has no kind"))?,
            "operation journal.kind",
        )? != operation
    {
        return Err(ObserverError::invalid(
            "operation-progress-recorded identity differs from its embedded journal",
        ));
    }
    if let Some(completed) = state.completed_operations.get(&journal_path) {
        if (journal_path.starts_with("CREATE.") || journal_path.starts_with("FINISH."))
            && !completed.is(&slot, &operation)
        {
            return Err(ObserverError::invalid(
                "scoped operation journal path was reused for a different identity",
            ));
        }
        // Scoped filenames are deterministic per machine and slot, so a
        // supported abort/refusal followed by a retry legitimately reuses the
        // same path.  New progress begins a new attempt and supersedes only an
        // exactly matching completed identity.  The singleton compatibility
        // journal is intentionally reusable across identities as well.
        state.completed_operations.remove(&journal_path);
    }
    // Python lets progress at a pending path replace another identity's
    // marker. Replay refuses instead: the overwritten operation may still own
    // its checkout, so accepting the history could drop a blocker.
    if state.pending_operations.values().any(|pending| {
        pending.kind == PendingOperationKind::Journal
            && pending.journal_path.as_deref() == Some(journal_path.as_str())
            && (pending.slot != slot || pending.operation.as_deref() != Some(operation.as_str()))
    }) {
        return Err(ObserverError::invalid(
            "append-only history reuses a pending journal path for a different operation identity",
        ));
    }
    if state.pending_operations.values().any(|pending| {
        pending.kind == PendingOperationKind::Journal
            && pending.slot == slot
            && pending.operation.as_deref() == Some(operation.as_str())
            && pending.journal_path.as_deref() != Some(journal_path.as_str())
    }) {
        return Err(ObserverError::invalid(
            "append-only history has the same pending operation under multiple journal paths",
        ));
    }
    let generation = active_generation(state, &slot);
    let storage = AttemptStorage::from_journal(&operation, journal);
    insert_pending(
        state,
        &journal_path,
        PendingOperation {
            slot,
            generation,
            kind: PendingOperationKind::Journal,
            operation: Some(operation),
            journal_path: Some(journal_path.clone()),
            journal_sha256: Some(canonical_sha256(&Value::Object(journal.clone()))?),
            event_sha256: event.sha256.clone(),
            storage,
        },
    );
    Ok(())
}

fn apply_operation_completed(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "operation-completed payload")?;
    exact_keys_optional(
        payload,
        &["slot", "operation"],
        &["journal_path"],
        "operation-completed payload",
    )?;
    let (slot, operation, journal_path) =
        operation_identity(payload, &event.machine, "operation-completed payload")?;
    let journal_key = pending_key(PendingOperationKind::Journal, &slot, &journal_path);
    let pending_journal = state.pending_operations.remove(&journal_key);
    let matching_recovery = state.pending_operations.values().any(|pending| {
        pending.kind == PendingOperationKind::Recovery
            && pending.slot == slot
            && pending.operation.as_deref() == Some(operation.as_str())
    });
    let duplicate_completion = state
        .completed_operations
        .get(&journal_path)
        .is_some_and(|completed| completed.is(&slot, &operation));
    // Python ignores such a completion. Replay refuses it because a history
    // that closes an operation it never opened is not one Python writes.
    if pending_journal.is_none() && !matching_recovery && !duplicate_completion {
        return Err(ObserverError::invalid(
            "operation-completed has no pending progress or recovery attempt",
        ));
    }
    let storage = match pending_journal {
        Some(pending)
            if pending.operation.as_deref() == Some(operation.as_str())
                && pending.journal_path.as_deref() == Some(journal_path.as_str()) =>
        {
            // The completion closes this physical journal only. Lifecycle
            // attempts have a different terminal condition below.
            pending.storage
        }
        Some(_) => {
            return Err(ObserverError::invalid(
                "append-only completion does not match its pending operation",
            ))
        }
        None => state
            .completed_operations
            .get(&journal_path)
            .filter(|completed| completed.is(&slot, &operation))
            .and_then(|completed| completed.storage.clone()),
    };
    if TERMINAL_COMPLETION_OPERATIONS.contains(&operation.as_str()) {
        // A create or import completion ends a recovery attempt bound to the
        // same journal only when an ACTIVE row now owns every checkout path
        // that attempt placed or planned, under the same slot type. Without
        // such a row the completion proves nothing about storage: older
        // writers completed these journals while leaving provisioned
        // worktrees behind, and a create of the same slot name under another
        // slot type proves only that its own root was clear. An attempt whose
        // storage is unknown, or a recovery bound elsewhere such as the legacy
        // singleton default, stays pending.
        let recovery_key = pending_key(
            PendingOperationKind::Recovery,
            &slot,
            &recovery_suffix(&operation, &journal_path),
        );
        let closed = state.active_records.get(&slot).is_some_and(|row| {
            state
                .pending_operations
                .get(&recovery_key)
                .and_then(|pending| pending.storage.as_ref())
                .is_some_and(|storage| storage.owned_by(&row.meta))
        });
        if closed {
            state.pending_operations.remove(&recovery_key);
        }
    }
    state.completed_operations.insert(
        journal_path,
        CompletedOperation {
            slot: slot.clone(),
            operation,
            sequence: event.sequence,
            storage,
        },
    );
    clear_archived_removal_markers(state, &slot);
    Ok(())
}

/// Close lifecycle attempts only after the append-only history proves that the
/// same generation was archived with removed storage and left ACTIVE. Python's
/// `operation-completed` means only that a journal was cleared: rollback and
/// late-refusal paths emit it while deliberately retaining the slot. Create and
/// import completions that leave an ACTIVE row owning the attempt's storage are
/// the exception handled in `apply_operation_completed`.
fn clear_archived_removal_markers(state: &mut State, slot: &str) {
    let Some(generation) = state.archived_generations.get(slot).copied() else {
        return;
    };
    if state.active_records.contains_key(slot) {
        return;
    }
    let existing_journals = state
        .pending_operations
        .values()
        .filter(|pending| pending.kind == PendingOperationKind::Journal && pending.slot == slot)
        .filter_map(|pending| Some((pending.operation.clone()?, pending.journal_path.clone()?)))
        .collect::<BTreeSet<_>>();
    let mut legacy_journals = Vec::new();
    state.pending_operations.retain(|_, pending| {
        if pending.slot != slot || pending.kind == PendingOperationKind::Journal {
            return true;
        }
        if pending.operation.as_deref() != Some("finish") {
            return true;
        }
        if pending.generation != Some(generation) {
            return true;
        }
        if pending.kind == PendingOperationKind::Recovery {
            if let Some(journal_path) = pending.journal_path.as_ref() {
                let journal_identity = ("finish".to_owned(), journal_path.clone());
                if !existing_journals.contains(&journal_identity)
                    && !state
                        .completed_operations
                        .get(journal_path)
                        .is_some_and(|completed| completed.is(&pending.slot, "finish"))
                {
                    legacy_journals.push(PendingOperation {
                        slot: pending.slot.clone(),
                        generation: pending.generation,
                        kind: PendingOperationKind::Journal,
                        operation: pending.operation.clone(),
                        journal_path: Some(journal_path.clone()),
                        journal_sha256: None,
                        event_sha256: pending.event_sha256.clone(),
                        storage: None,
                    });
                }
            }
        }
        false
    });
    for pending in legacy_journals {
        let journal_path = pending
            .journal_path
            .clone()
            .expect("legacy recovery journal path is present");
        insert_pending(state, &journal_path, pending);
    }
}

fn apply_reclaim_started(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "reclaim-started payload")?;
    // The append-only log predates both authenticated host-runner evidence
    // (e5074d1) and local-salvage configuration (2a61b65).  Accept only the
    // three exact shapes the Python authority has emitted; an arbitrary
    // subset would turn corruption into an undocumented compatibility form.
    let expected_fields: &[&str] = if payload.contains_key("salvage_archive_root") {
        &[
            "slot",
            "generation",
            "actor",
            "runner",
            "handoff_writer",
            "coordinator_authorized",
            "owner_state",
            "registered_liveness",
            "heartbeat_age_seconds",
            "heartbeat_ttl_seconds",
            "validate_complete",
            "live_validate_owner",
            "salvage_archive_root",
        ]
    } else if payload.contains_key("runner") || payload.contains_key("handoff_writer") {
        &[
            "slot",
            "generation",
            "actor",
            "runner",
            "handoff_writer",
            "coordinator_authorized",
            "owner_state",
            "registered_liveness",
            "heartbeat_age_seconds",
            "heartbeat_ttl_seconds",
            "validate_complete",
            "live_validate_owner",
        ]
    } else {
        &[
            "slot",
            "generation",
            "actor",
            "coordinator_authorized",
            "owner_state",
            "registered_liveness",
            "heartbeat_age_seconds",
            "heartbeat_ttl_seconds",
            "validate_complete",
            "live_validate_owner",
        ]
    };
    exact_keys(payload, expected_fields, "reclaim-started payload")?;
    let slot = string(&payload["slot"], "reclaim-started payload.slot")?;
    validate_name(slot, "reclaim-started payload.slot")?;
    let generation = unsigned(&payload["generation"], "reclaim-started payload.generation")?;
    if generation == 0 || active_generation(state, slot) != Some(generation) {
        return Err(ObserverError::invalid(
            "reclaim-started does not match an active slot generation",
        ));
    }
    object(&payload["actor"], "reclaim-started payload.actor")?;
    if let Some(runner) = payload.get("runner") {
        object(runner, "reclaim-started payload.runner")?;
    }
    if payload
        .get("handoff_writer")
        .is_some_and(|writer| !writer.is_null())
    {
        object(
            &payload["handoff_writer"],
            "reclaim-started payload.handoff_writer",
        )?;
    }
    if payload["coordinator_authorized"].as_bool().is_none()
        || payload["validate_complete"].as_bool().is_none()
        || payload["live_validate_owner"].as_bool().is_none()
        || payload
            .get("salvage_archive_root")
            .is_some_and(|root| !matches!(root, Value::Null | Value::String(_)))
    {
        return Err(ObserverError::invalid(
            "reclaim-started has invalid typed evidence fields",
        ));
    }
    string(
        &payload["owner_state"],
        "reclaim-started payload.owner_state",
    )?;
    string(
        &payload["registered_liveness"],
        "reclaim-started payload.registered_liveness",
    )?;
    unsigned(
        &payload["heartbeat_age_seconds"],
        "reclaim-started payload.heartbeat_age_seconds",
    )?;
    unsigned(
        &payload["heartbeat_ttl_seconds"],
        "reclaim-started payload.heartbeat_ttl_seconds",
    )?;
    insert_pending(
        state,
        &generation.to_string(),
        PendingOperation {
            slot: slot.to_owned(),
            generation: Some(generation),
            kind: PendingOperationKind::Reclaim,
            operation: Some("finish".to_owned()),
            journal_path: None,
            journal_sha256: None,
            event_sha256: event.sha256.clone(),
            storage: None,
        },
    );
    Ok(())
}

fn apply_recovery_started(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "recovery-started payload")?;
    // recovery-started originally carried only the durable actor.  Runner and
    // handoff-writer evidence were added together in ded39ae; preserve the old
    // event as a pending recovery without accepting a half-modern shape.
    let expected_fields: &[&str] =
        if payload.contains_key("runner") || payload.contains_key("handoff_writer") {
            &[
                "slot",
                "operation",
                "actor",
                "runner",
                "handoff_writer",
                "coordinator_authorized",
            ]
        } else {
            &["slot", "operation", "actor", "coordinator_authorized"]
        };
    exact_keys(payload, expected_fields, "recovery-started payload")?;
    let slot = string(&payload["slot"], "recovery-started payload.slot")?;
    validate_name(slot, "recovery-started payload.slot")?;
    let operation = string(&payload["operation"], "recovery-started payload.operation")?;
    validate_operation(operation, "recovery-started payload.operation")?;
    object(&payload["actor"], "recovery-started payload.actor")?;
    if let Some(runner) = payload.get("runner") {
        object(runner, "recovery-started payload.runner")?;
    }
    if payload
        .get("handoff_writer")
        .is_some_and(|writer| !writer.is_null())
    {
        object(
            &payload["handoff_writer"],
            "recovery-started payload.handoff_writer",
        )?;
    }
    if payload["coordinator_authorized"].as_bool().is_none() {
        return Err(ObserverError::invalid(
            "recovery-started payload.coordinator_authorized must be boolean",
        ));
    }
    let pending_journal = state.pending_operations.values().find(|pending| {
        pending.kind == PendingOperationKind::Journal
            && pending.slot == slot
            && pending.operation.as_deref() == Some(operation)
    });
    let generation = active_generation(state, slot)
        .or_else(|| pending_journal.and_then(|pending| pending.generation))
        .or_else(|| {
            (operation == "finish")
                .then(|| state.archived_generations.get(slot).copied())
                .flatten()
        });
    // `operation-completed` is appended before the journal is unlinked, so a
    // crash between the two leaves a completed journal that recovery loads
    // again. Bind the most recent such completion: an older one at another
    // path may since have been reused by a different identity. Only a journal
    // absent from the history is the legacy singleton.
    // The attempt inherits the bound journal's storage; the singleton default
    // has none, so nothing can later prove its storage owned.
    let (journal_path, storage) = pending_journal
        .and_then(|pending| Some((pending.journal_path.clone()?, pending.storage.clone())))
        .or_else(|| {
            state
                .completed_operations
                .iter()
                .filter(|(_, completed)| completed.is(slot, operation))
                .max_by_key(|(_, completed)| completed.sequence)
                .map(|(path, completed)| (path.clone(), completed.storage.clone()))
        })
        .unwrap_or_else(|| (format!("ACTIVE.{}.journal", event.machine), None));
    // Recovering the same journal again adds that attempt's storage to what
    // the open marker already requires.
    let suffix = recovery_suffix(operation, &journal_path);
    let storage = match state.pending_operations.get(&pending_key(
        PendingOperationKind::Recovery,
        slot,
        &suffix,
    )) {
        Some(open) => AttemptStorage::merge(open.storage.clone(), storage),
        None => storage,
    };
    insert_pending(
        state,
        &suffix,
        PendingOperation {
            slot: slot.to_owned(),
            generation,
            kind: PendingOperationKind::Recovery,
            operation: Some(operation.to_owned()),
            journal_path: Some(journal_path),
            journal_sha256: None,
            event_sha256: event.sha256.clone(),
            storage,
        },
    );
    clear_archived_removal_markers(state, slot);
    Ok(())
}

fn apply_retirement_attempted(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "retirement-attempted payload")?;
    exact_keys(
        payload,
        &[
            "slot",
            "generation",
            "sha256",
            "handoff_read_sequence",
            "reason",
        ],
        "retirement-attempted payload",
    )?;
    let slot = string(&payload["slot"], "retirement-attempted payload.slot")?;
    validate_name(slot, "retirement-attempted payload.slot")?;
    let generation = unsigned(
        &payload["generation"],
        "retirement-attempted payload.generation",
    )?;
    if generation == 0 || active_generation(state, slot) != Some(generation) {
        return Err(ObserverError::invalid(
            "retirement-attempted does not match an active slot generation",
        ));
    }
    let digest = string(&payload["sha256"], "retirement-attempted payload.sha256")?;
    if !is_sha256(digest)
        || unsigned(
            &payload["handoff_read_sequence"],
            "retirement-attempted payload.handoff_read_sequence",
        )? == 0
        || string(&payload["reason"], "retirement-attempted payload.reason")?
            .trim()
            .is_empty()
    {
        return Err(ObserverError::invalid(
            "retirement-attempted has invalid provenance fields",
        ));
    }
    insert_pending(
        state,
        &generation.to_string(),
        PendingOperation {
            slot: slot.to_owned(),
            generation: Some(generation),
            kind: PendingOperationKind::Retirement,
            operation: Some("finish".to_owned()),
            journal_path: None,
            journal_sha256: None,
            event_sha256: event.sha256.clone(),
            storage: None,
        },
    );
    Ok(())
}

fn apply_import(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    if state.active_revision.is_some() || state.archive_revision.is_some() {
        return Err(ObserverError::invalid(
            "event log imports state more than once",
        ));
    }
    let payload = object(&event.payload, "state-imported payload")?;
    exact_keys(
        payload,
        &["active", "archive", "holds"],
        "state-imported payload",
    )?;
    let imported_holds = array(&payload["holds"], "state-imported payload.holds")?;

    let active = object(&payload["active"], "imported active state")?;
    exact_keys(
        active,
        &["schema", "machine", "revision", "slots"],
        "imported active state",
    )?;
    validate_state_header(active, &event.machine, "imported active state")?;
    let slots = array(&active["slots"], "imported active state.slots")?;
    for record in slots {
        let validated = validate_active_record(record, &event.machine, "imported active record")?;
        let meta = validated.meta;
        let slot = meta.slot.clone();
        if meta.has_unique_agent() {
            if let Some(existing) = state.active_agents.get(&meta.agent) {
                return Err(ObserverError::invalid(format!(
                    "agent {} owns both active slots {existing} and {slot}",
                    meta.agent
                )));
            }
            state.active_agents.insert(meta.agent.clone(), slot.clone());
        }
        if state
            .active_records
            .insert(
                slot.clone(),
                ActiveEntry {
                    value: validated.normalized,
                    meta,
                },
            )
            .is_some()
        {
            return Err(ObserverError::invalid(format!(
                "duplicate active slot in imported state: {slot}"
            )));
        }
    }
    state.active_revision = Some(unsigned(
        &active["revision"],
        "imported active state.revision",
    )?);

    let archive = object(&payload["archive"], "imported archive state")?;
    exact_keys(
        archive,
        &["schema", "machine", "revision", "records"],
        "imported archive state",
    )?;
    validate_state_header(archive, &event.machine, "imported archive state")?;
    let records = array(&archive["records"], "imported archive state.records")?;
    for record in records {
        let meta = validate_archive_record(record, &event.machine, "imported archive record")?;
        insert_archive_record(record, meta, state)?;
    }
    state.archive_revision = Some(unsigned(
        &archive["revision"],
        "imported archive state.revision",
    )?);
    for (index, value) in imported_holds.iter().enumerate() {
        let label = format!("state-imported payload.holds[{index}]");
        let hold = object(value, &label)?;
        exact_keys(
            hold,
            &["schema", "machine", "slot", "held_at", "reason"],
            &label,
        )?;
        if unsigned(&hold["schema"], &format!("{label}.schema"))? != 1
            || string(&hold["machine"], &format!("{label}.machine"))? != event.machine
        {
            return Err(ObserverError::invalid(format!(
                "{label} has an invalid schema or machine"
            )));
        }
        let slot = string(&hold["slot"], &format!("{label}.slot"))?;
        validate_name(slot, &format!("{label}.slot"))?;
        let active = state.active_records.get(slot).ok_or_else(|| {
            ObserverError::invalid(format!("{label} does not name an active slot"))
        })?;
        let held_at = string(&hold["held_at"], &format!("{label}.held_at"))?;
        parse_timestamp(held_at, &format!("{label}.held_at"))?;
        let reason = string(&hold["reason"], &format!("{label}.reason"))?;
        if reason.trim().is_empty() || state.holds.contains_key(slot) {
            return Err(ObserverError::invalid(format!(
                "{label} has an empty reason or duplicates a slot"
            )));
        }
        state.holds.insert(
            slot.to_owned(),
            SlotHold {
                slot: slot.to_owned(),
                generation: active.meta.generation,
                held_at: held_at.to_owned(),
                reason: reason.to_owned(),
                event_sha256: event.sha256.clone(),
            },
        );
    }
    Ok(())
}

fn apply_hold(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "slot-held payload")?;
    exact_keys(
        payload,
        &["slot", "generation", "reason"],
        "slot-held payload",
    )?;
    let slot = string(&payload["slot"], "slot-held payload.slot")?;
    validate_name(slot, "slot-held payload.slot")?;
    let generation = unsigned(&payload["generation"], "slot-held payload.generation")?;
    let active = state.active_records.get(slot).ok_or_else(|| {
        ObserverError::invalid(format!("slot-held event names absent slot {slot}"))
    })?;
    let reason = string(&payload["reason"], "slot-held payload.reason")?;
    if generation == 0 || generation != active.meta.generation || reason.trim().is_empty() {
        return Err(ObserverError::invalid(format!(
            "slot-held event does not match active generation for {slot}"
        )));
    }
    if state.holds.contains_key(slot) {
        return Err(ObserverError::invalid(format!(
            "slot-held event duplicates active hold for {slot}"
        )));
    }
    state.holds.insert(
        slot.to_owned(),
        SlotHold {
            slot: slot.to_owned(),
            generation,
            held_at: event.recorded_at.clone(),
            reason: reason.to_owned(),
            event_sha256: event.sha256.clone(),
        },
    );
    Ok(())
}

fn apply_hold_release(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let payload = object(&event.payload, "slot-hold-released payload")?;
    exact_keys(
        payload,
        &["slot", "generation"],
        "slot-hold-released payload",
    )?;
    let slot = string(&payload["slot"], "slot-hold-released payload.slot")?;
    validate_name(slot, "slot-hold-released payload.slot")?;
    let generation = unsigned(
        &payload["generation"],
        "slot-hold-released payload.generation",
    )?;
    let active = state.active_records.get(slot).ok_or_else(|| {
        ObserverError::invalid(format!("slot-hold-released event names absent slot {slot}"))
    })?;
    if generation == 0 || generation != active.meta.generation {
        return Err(ObserverError::invalid(format!(
            "slot-hold-released event does not match active generation for {slot}"
        )));
    }
    let hold = state.holds.remove(slot).ok_or_else(|| {
        ObserverError::invalid(format!("slot-hold-released event has no hold for {slot}"))
    })?;
    if hold.generation != generation {
        return Err(ObserverError::invalid(format!(
            "slot-hold-released event does not match held generation for {slot}"
        )));
    }
    Ok(())
}

fn apply_active(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let current_revision = state
        .active_revision
        .ok_or_else(|| ObserverError::invalid("event records active state before its import"))?;
    let payload = object(&event.payload, "active-state-recorded payload")?;
    exact_keys(
        payload,
        &[
            "action",
            "slot",
            "previous_revision",
            "revision",
            "previous_record_sha256",
            "record",
            "evidence",
        ],
        "active-state-recorded payload",
    )?;
    let action = string(&payload["action"], "active-state-recorded action")?;
    if action.is_empty() {
        return Err(ObserverError::invalid(
            "active-state-recorded action must not be empty",
        ));
    }
    let slot = string(&payload["slot"], "active-state-recorded slot")?;
    validate_name(slot, "active-state-recorded slot")?;
    continued_revision(
        current_revision,
        &payload["previous_revision"],
        &payload["revision"],
        "active",
    )?;
    if !payload["evidence"].is_object() {
        return Err(ObserverError::invalid(
            "active-state-recorded evidence must be a JSON object",
        ));
    }

    let current = state.active_records.get(slot);
    let current_meta = current.map(|entry| entry.meta.clone());
    let expected_previous = current
        .map(|entry| canonical_sha256(&entry.value))
        .transpose()?;
    match (&payload["previous_record_sha256"], &expected_previous) {
        (Value::Null, None) => {}
        (Value::String(recorded), Some(expected)) if recorded == expected => {}
        _ => {
            return Err(ObserverError::invalid(format!(
                "active-state-recorded previous record does not match slot {slot}"
            )))
        }
    }

    if action == "absent-validation-row-recovered" {
        let current = current_meta.as_ref().ok_or_else(|| {
            ObserverError::invalid("absent-validation-row recovery must remove one active row")
        })?;
        if !payload["record"].is_null() {
            return Err(ObserverError::invalid(
                "absent-validation-row recovery must remove one active row",
            ));
        }
        let evidence = validation_recovery_evidence(
            &payload["evidence"],
            "active validation recovery evidence",
        )?;
        if Some(evidence.source_record_sha256.as_str()) != expected_previous.as_deref()
            || !evidence
                .archive_id
                .starts_with(&format!("{}:{slot}:{}:", event.machine, current.generation))
        {
            return Err(ObserverError::invalid(
                "active validation recovery evidence does not match its source record",
            ));
        }
    } else if action == "absent-agent-row-recovered" {
        let current = current_meta.as_ref().ok_or_else(|| {
            ObserverError::invalid("absent-agent-row recovery must remove one active agent row")
        })?;
        if current.slot_type != "agent" || !payload["record"].is_null() {
            return Err(ObserverError::invalid(
                "absent-agent-row recovery must remove one active agent row",
            ));
        }
        let evidence =
            agent_recovery_evidence(&payload["evidence"], "active agent recovery evidence")?;
        if Some(evidence.source_record_sha256.as_str()) != expected_previous.as_deref()
            || !evidence
                .archive_id
                .starts_with(&format!("{}:{slot}:{}:", event.machine, current.generation))
        {
            return Err(ObserverError::invalid(
                "active agent recovery evidence does not match its source record",
            ));
        }
    }

    let next = if payload["record"].is_null() {
        if current_meta.is_none() {
            return Err(ObserverError::invalid(format!(
                "active-state-recorded removes absent slot {slot}"
            )));
        }
        None
    } else {
        let validated = validate_active_record(
            &payload["record"],
            &event.machine,
            "active-state-recorded record",
        )?;
        let meta = validated.meta;
        if meta.slot != slot {
            return Err(ObserverError::invalid(format!(
                "active-state-recorded record does not match slot {slot}"
            )));
        }
        if meta.has_unique_agent()
            && state
                .active_agents
                .get(&meta.agent)
                .is_some_and(|existing| existing != slot)
        {
            return Err(ObserverError::invalid(format!(
                "agent {} already owns another active slot",
                meta.agent
            )));
        }
        Some(ActiveEntry {
            value: validated.normalized,
            meta,
        })
    };
    if let Some(hold) = state.holds.get(slot) {
        if next
            .as_ref()
            .is_none_or(|entry| entry.meta.generation != hold.generation)
        {
            return Err(ObserverError::invalid(format!(
                "active-state-recorded changes held generation for {slot}"
            )));
        }
    }
    let removed_generation = current_meta.as_ref().map(|meta| meta.generation);
    if let Some(current) = state.active_records.remove(slot) {
        if current.meta.has_unique_agent()
            && state
                .active_agents
                .get(&current.meta.agent)
                .map(String::as_str)
                == Some(slot)
        {
            state.active_agents.remove(&current.meta.agent);
        }
    }
    let removed_active = next.is_none();
    if let Some(next) = next {
        if next.meta.has_unique_agent() {
            state
                .active_agents
                .insert(next.meta.agent.clone(), slot.to_owned());
        }
        state.active_records.insert(slot.to_owned(), next);
    }
    state.active_revision = Some(current_revision + 1);
    if removed_active && removed_generation.is_some() {
        clear_archived_removal_markers(state, slot);
    }
    Ok(())
}

fn apply_archive(event: &Event, state: &mut State) -> Result<(), ObserverError> {
    let current_revision = state
        .archive_revision
        .ok_or_else(|| ObserverError::invalid("event records archive state before its import"))?;
    let payload = object(&event.payload, "archive-state-recorded payload")?;
    exact_keys(
        payload,
        &[
            "action",
            "slot",
            "previous_revision",
            "revision",
            "record",
            "evidence",
        ],
        "archive-state-recorded payload",
    )?;
    let action = string(&payload["action"], "archive-state-recorded action")?;
    if action.is_empty() {
        return Err(ObserverError::invalid(
            "archive-state-recorded action must not be empty",
        ));
    }
    let slot = string(&payload["slot"], "archive-state-recorded slot")?;
    validate_name(slot, "archive-state-recorded slot")?;
    continued_revision(
        current_revision,
        &payload["previous_revision"],
        &payload["revision"],
        "archive",
    )?;
    if !payload["evidence"].is_object() {
        return Err(ObserverError::invalid(
            "archive-state-recorded evidence must be a JSON object",
        ));
    }
    let meta = validate_archive_record(
        &payload["record"],
        &event.machine,
        "archive-state-recorded record",
    )?;
    if meta.slot != slot {
        return Err(ObserverError::invalid(format!(
            "archive-state-recorded record does not match slot {slot}"
        )));
    }
    if action == "absent-validation-row-recovered" {
        let evidence = validation_recovery_evidence(
            &payload["evidence"],
            "archive validation recovery evidence",
        )?;
        if meta.slot_type != "validate"
            || meta.physical_storage != "removed"
            || evidence.archive_id != meta.archive_id
        {
            return Err(ObserverError::invalid(
                "archive validation recovery evidence does not match its record",
            ));
        }
    } else if action == "absent-agent-row-recovered" {
        let evidence =
            agent_recovery_evidence(&payload["evidence"], "archive agent recovery evidence")?;
        if meta.slot_type != "agent"
            || meta.physical_storage != "removed"
            || evidence.archive_id != meta.archive_id
        {
            return Err(ObserverError::invalid(
                "archive agent recovery evidence does not match its record",
            ));
        }
    }
    insert_archive_record(&payload["record"], meta, state)?;
    state.archive_revision = Some(current_revision + 1);
    clear_archived_removal_markers(state, slot);
    Ok(())
}

fn insert_archive_record(
    record: &Value,
    meta: crate::schema::ArchiveRecordMeta,
    state: &mut State,
) -> Result<(), ObserverError> {
    if !state.archive_ids.insert(meta.archive_id.clone()) {
        return Err(ObserverError::invalid(format!(
            "duplicate archive_id in event log: {}",
            meta.archive_id
        )));
    }
    if state
        .archived_generations
        .insert(meta.slot.clone(), meta.generation)
        .is_some()
    {
        return Err(ObserverError::invalid(format!(
            "duplicate archived slot in event log: {}",
            meta.slot
        )));
    }
    state.archive_records.push(record.clone());
    Ok(())
}

fn validate_state_header(
    state: &Map<String, Value>,
    machine: &str,
    label: &str,
) -> Result<(), ObserverError> {
    if unsigned(&state["schema"], &format!("{label}.schema"))? != STATE_SCHEMA {
        return Err(ObserverError::invalid(format!(
            "unsupported state schema in {label}"
        )));
    }
    if string(&state["machine"], &format!("{label}.machine"))? != machine {
        return Err(ObserverError::invalid(format!(
            "machine mismatch in {label}"
        )));
    }
    unsigned(&state["revision"], &format!("{label}.revision"))?;
    Ok(())
}

struct RecoveryEvidence {
    archive_id: String,
    source_record_sha256: String,
}

fn validation_recovery_evidence(
    value: &Value,
    label: &str,
) -> Result<RecoveryEvidence, ObserverError> {
    let evidence = object(value, label)?;
    exact_keys(
        evidence,
        &["archive_id", "source_record_sha256", "validation_outcome"],
        label,
    )?;
    if string(
        &evidence["validation_outcome"],
        &format!("{label}.validation_outcome"),
    )? != "unknown"
    {
        return Err(ObserverError::invalid(format!(
            "{label}.validation_outcome must be unknown"
        )));
    }
    recovery_identity(evidence, label)
}

fn agent_recovery_evidence(value: &Value, label: &str) -> Result<RecoveryEvidence, ObserverError> {
    let evidence = object(value, label)?;
    exact_keys(
        evidence,
        &["archive_id", "source_record_sha256", "physical_storage"],
        label,
    )?;
    if string(
        &evidence["physical_storage"],
        &format!("{label}.physical_storage"),
    )? != "externally-absent"
    {
        return Err(ObserverError::invalid(format!(
            "{label}.physical_storage must be externally-absent"
        )));
    }
    recovery_identity(evidence, label)
}

fn recovery_identity(
    evidence: &Map<String, Value>,
    label: &str,
) -> Result<RecoveryEvidence, ObserverError> {
    let archive_id = string(&evidence["archive_id"], &format!("{label}.archive_id"))?;
    let source_record_sha256 = string(
        &evidence["source_record_sha256"],
        &format!("{label}.source_record_sha256"),
    )?;
    if !is_sha256(source_record_sha256) {
        return Err(ObserverError::invalid(format!(
            "{label}.source_record_sha256 is not a lowercase SHA-256 digest"
        )));
    }
    Ok(RecoveryEvidence {
        archive_id: archive_id.to_owned(),
        source_record_sha256: source_record_sha256.to_owned(),
    })
}

fn continued_revision(
    current: u64,
    previous: &Value,
    revision: &Value,
    label: &str,
) -> Result<(), ObserverError> {
    let recorded_previous = unsigned(previous, &format!("{label} previous_revision"))?;
    let recorded_revision = unsigned(revision, &format!("{label} revision"))?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| ObserverError::invalid(format!("{label} revision overflow")))?;
    if recorded_previous != current || recorded_revision != next {
        return Err(ObserverError::invalid(format!(
            "{label} revision does not continue the derived state"
        )));
    }
    Ok(())
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, ObserverError> {
    value
        .as_object()
        .ok_or_else(|| ObserverError::invalid(format!("{label} must be a JSON object")))
}

fn array<'a>(value: &'a Value, label: &str) -> Result<&'a [Value], ObserverError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| ObserverError::invalid(format!("{label} must be a JSON array")))
}

fn string<'a>(value: &'a Value, label: &str) -> Result<&'a str, ObserverError> {
    value
        .as_str()
        .ok_or_else(|| ObserverError::invalid(format!("{label} must be a string")))
}

fn unsigned(value: &Value, label: &str) -> Result<u64, ObserverError> {
    value
        .as_u64()
        .ok_or_else(|| ObserverError::invalid(format!("{label} must be a non-negative integer")))
}

/// The lifecycle authority's name grammar: 1-64 ASCII letters, digits, `.`,
/// `_`, or `-`, beginning with a letter or digit.
pub(crate) fn validate_name(value: &str, label: &str) -> Result<(), ObserverError> {
    let bytes = value.as_bytes();
    let first_is_valid = bytes.first().is_some_and(u8::is_ascii_alphanumeric);
    let rest_is_valid = bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if bytes.len() > 64 || !first_is_valid || !rest_is_valid {
        return Err(ObserverError::invalid(format!(
            "{label} is not a valid name: {value:?}"
        )));
    }
    Ok(())
}

pub(crate) fn canonical_payload(event: &Event) -> Result<String, ObserverError> {
    canonical_json(&event.payload)
}
