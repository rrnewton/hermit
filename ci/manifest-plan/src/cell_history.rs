//! Explicit, append-only cell history outside Git.
//!
//! This module does not activate a producer, project a scorecard, or reap data.
//! A submission is a caller-declared byte census, not an independently inferred
//! proof that every planned test ran. Finalized validation evidence is separate.
//! Historical archives remain queryable even when their overlap cannot safely be
//! projected. Original bytes are always retained alongside the interpreted view.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::ledger::HistoryRow;
use crate::runner::CellResult;
use crate::stress_series::SeriesRow;

pub const BATCH_SCHEMA: &str = "cell-history-batch/v1";
pub const SNAPSHOT_SCHEMA: &str = "cell-history-snapshot/v1";
const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const PARTITION_EVENTS: usize = 8192;
const PARTITION_BYTES: usize = 16 * 1024 * 1024;
pub const STREAM_SCHEMA: &str = "cell-history-stream/v1";

pub type Result<T> = std::result::Result<T, String>;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 240
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// JSON's ordinary Value decoder silently overwrites duplicate object keys.
/// Reject them recursively before any schema-specific interpretation.
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<UniqueValue>()? {
                    values.push(v.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueValue>()? {
                    if values.insert(key.clone(), value.0).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON key {key:?}"
                        )));
                    }
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let value = serde_json::from_slice::<UniqueValue>(bytes).map_err(|e| e.to_string())?;
    serde_json::from_value(value.0).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub sha256: String,
    pub bytes: u64,
}

impl Blob {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: digest(bytes),
            bytes: bytes.len() as u64,
        }
    }
    fn verify(&self, bytes: &[u8]) -> Result<()> {
        if !hex(&self.sha256, 64) || self.bytes > MAX_BLOB_BYTES || *self != Self::of(bytes) {
            return Err("input differs from the declared byte census".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputOrigin {
    /// Explicit source-directory member. This is not a full effective env.
    ProducerFile,
    /// The commit/path/blob relation is checked with replacement objects and
    /// lazy network fetching disabled. Repository is a caller-supplied local path.
    Git {
        repository: String,
        commit: String,
        path: String,
        blob: String,
    },
    /// A legacy pending/retained source, preserved with an honest unavailable
    /// Git publication anchor. It never acquires a fabricated run identity.
    RetainedFile { original_path: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
    pub content: Blob,
    pub origin: InputOrigin,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawRow {
    pub input: String,
    /// One-based physical JSONL line number, not a synthesized event ID.
    pub line: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventBinding {
    pub event_id: String,
    /// Includes every outer repetition represented by a compressed series row.
    pub rows: Vec<RawRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnprojectedRow {
    pub row: RawRow,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizedValidation {
    pub row: String,
    pub plan: String,
    pub cells: String,
    pub tests: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchKind {
    Current {
        run_id: String,
        hermit_sha: String,
        /// Exactly "overrides_only"; current framework env maps do not record
        /// the inherited process environment. Keep absence explicit.
        environment_scope: String,
        series: String,
        raw: Vec<String>,
        bindings: Vec<EventBinding>,
        unprojected: Vec<UnprojectedRow>,
        /// Null preserves unqualified/partial evidence. It is never a pass.
        finalized: Option<Box<FinalizedValidation>>,
    },
    LegacySeries {
        series: Vec<String>,
    },
    /// An exact original cells.json blob with per-cell query access. There is
    /// deliberately no guessed event-ID mapping or aggregate-to-run conversion.
    LegacyCells {
        archive: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub schema: String,
    /// Human-readable producer/import identity, not an inferred run identity.
    pub identity: String,
    pub inputs: Vec<Input>,
    pub data: BatchKind,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedCells {
    pub batch: String,
    pub input: Input,
    pub cells: BTreeMap<String, Value>,
    pub projection: String,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchReceipt {
    pub id: String,
    pub identity: String,
    pub manifest: Blob,
    pub inputs: Vec<Input>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema: String,
    pub events: Vec<Value>,
    pub event_count: u64,
    pub events_sha256: String,
    pub batches: Vec<BatchReceipt>,
    pub archives: Vec<ArchivedCells>,
    pub incomplete: Vec<String>,
    /// Existing shared projection checks; exclusions do not erase history.
    pub event_projection_refusals: BTreeMap<String, String>,
    pub unqualified_current_batches: Vec<String>,
}

impl Snapshot {
    /// A sealed caller census cannot establish actual producer completeness.
    /// Current-history qualification requires the later producer-proof cutover;
    /// archive availability likewise does not resolve legacy overlap.
    pub fn require_projection_ready(&self) -> Result<()> {
        if !self.incomplete.is_empty()
            || !self.archives.is_empty()
            || !self.unqualified_current_batches.is_empty()
            || !self.event_projection_refusals.is_empty()
        {
            return Err("history is available, but incomplete staging, unresolved legacy coverage, excluded historical evidence, or missing current producer proof prevents projection".into());
        }
        Ok(())
    }
}

struct Validated {
    events: BTreeMap<String, Value>,
    archives: Vec<ArchivedCells>,
    unqualified: bool,
}

fn lines(bytes: &[u8]) -> Result<Vec<(u64, Value)>> {
    std::str::from_utf8(bytes)
        .map_err(|e| e.to_string())?
        .lines()
        .enumerate()
        .filter(|(_, s)| !s.trim().is_empty())
        .map(|(n, s)| decode(s.as_bytes()).map(|v| (n as u64 + 1, v)))
        .collect()
}

fn take_input<'a>(
    name: &str,
    inputs: &'a BTreeMap<String, Vec<u8>>,
    used: &mut BTreeSet<String>,
) -> Result<&'a [u8]> {
    if !used.insert(name.to_owned()) {
        return Err(format!("input {name:?} has multiple roles"));
    }
    inputs
        .get(name)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("missing declared input {name:?}"))
}

fn add_series(bytes: &[u8], current: bool, events: &mut BTreeMap<String, Value>) -> Result<()> {
    for (_, value) in lines(bytes)? {
        let row: SeriesRow = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        if current {
            row.validate_for_write()?;
        } else {
            row.validate_for_read()?;
        }
        match events.get(&row.event_id) {
            Some(previous) if previous != &value => {
                return Err(format!("conflicting event {}", row.event_id));
            }
            Some(_) if current => return Err(format!("duplicate current event {}", row.event_id)),
            _ => {
                events.insert(row.event_id, value);
            }
        }
    }
    Ok(())
}

fn raw_cell(row: &CellResult) -> Result<String> {
    let backend = row
        .backend
        .as_deref()
        .or_else(|| (row.mode == "naked").then_some("native"))
        .ok_or("raw cell omitted backend")?;
    Ok(format!("{}/{}/{}", row.test, row.mode, backend))
}

fn require_raw(row: &CellResult, run_id: &str, head: &str) -> Result<()> {
    if row.schema != 4 || row.run_id != run_id || row.hermit_sha != head || row.attempt == 0 {
        return Err("raw source/run/attempt differs from its producer declaration".into());
    }
    row.validate_timeout_policy()?;
    row.require_current_classification()?;
    row.require_cpu_observations()?;
    let mut attempts = BTreeSet::new();
    for attempt in &row.attempts {
        if attempt.index.is_empty() || !attempts.insert(&attempt.index) {
            return Err("raw row has an empty or repeated inner attempt identity".into());
        }
    }
    Ok(())
}

impl Batch {
    fn validate(&self, id: &str, inputs: &BTreeMap<String, Vec<u8>>) -> Result<Validated> {
        if self.schema != BATCH_SCHEMA
            || self.identity.trim().is_empty()
            || self.inputs.len() > MAX_ENTRIES
        {
            return Err("unknown batch schema, empty identity, or excessive input census".into());
        }
        let mut names = BTreeSet::new();
        for input in &self.inputs {
            if !component(&input.name) || !names.insert(input.name.clone()) {
                return Err("input names must be unique plain filename components".into());
            }
            input
                .content
                .verify(inputs.get(&input.name).ok_or("declared input missing")?)?;
        }
        if names != inputs.keys().cloned().collect() {
            return Err("undeclared input in census".into());
        }
        let mut used = BTreeSet::new();
        let mut result = Validated {
            events: BTreeMap::new(),
            archives: Vec::new(),
            unqualified: false,
        };
        match &self.data {
            BatchKind::LegacySeries { series } => {
                if series.is_empty() {
                    return Err("legacy series import is empty".into());
                }
                if self
                    .inputs
                    .iter()
                    .any(|i| i.origin == InputOrigin::ProducerFile)
                {
                    return Err(
                        "legacy import must explicitly name its retained or Git source".into(),
                    );
                }
                for name in series {
                    add_series(
                        take_input(name, inputs, &mut used)?,
                        false,
                        &mut result.events,
                    )?;
                }
            }
            BatchKind::LegacyCells { archive } => {
                let bytes = take_input(archive, inputs, &mut used)?;
                let value: Value = decode(bytes)?;
                let mut cells = BTreeMap::new();
                match value.get("cells") {
                    Some(Value::Object(keyed)) => {
                        cells.extend(
                            keyed
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone())),
                        );
                    }
                    Some(Value::Array(rows)) => {
                        for row in rows {
                            let fields = ["test", "mode", "backend"].map(|field| {
                                row.get(field)
                                    .and_then(Value::as_str)
                                    .filter(|v| !v.is_empty())
                                    .ok_or(
                                        "archive cell omitted an exact test/mode/backend identity",
                                    )
                            });
                            let [test, mode, backend] = fields;
                            let key = format!("{}/{}/{}", test?, mode?, backend?);
                            if cells.insert(key, row.clone()).is_some() {
                                return Err("archive repeats a cell identity".into());
                            }
                        }
                    }
                    _ => return Err("archive has no supported cells population".into()),
                }
                let input = self
                    .inputs
                    .iter()
                    .find(|i| &i.name == archive)
                    .ok_or("archive input missing")?
                    .clone();
                if !matches!(input.origin, InputOrigin::Git { .. }) {
                    return Err("legacy cells require exact Git commit/path/blob provenance".into());
                }
                result.archives.push(ArchivedCells { batch: id.to_owned(), input, cells, projection: "unresolved legacy coverage; original cells remain available, no fabricated event/run IDs".into() });
            }
            BatchKind::Current {
                run_id,
                hermit_sha,
                environment_scope,
                series,
                raw,
                bindings,
                unprojected,
                finalized,
            } => {
                // Even a separately authenticated finalized population does not
                // bind superseded raw inputs. Stage1 never upgrades the caller's
                // declaration into actual current producer completeness.
                result.unqualified = true;
                if run_id.trim().is_empty()
                    || !hex(hermit_sha, 40)
                    || environment_scope != "overrides_only"
                {
                    return Err("current batch requires exact run/head and overrides_only environment provenance".into());
                }
                if self
                    .inputs
                    .iter()
                    .any(|i| i.origin != InputOrigin::ProducerFile)
                {
                    return Err(
                        "current batch inputs must come from the declared producer census".into(),
                    );
                }
                add_series(
                    take_input(series, inputs, &mut used)?,
                    true,
                    &mut result.events,
                )?;
                let mut raw_rows = BTreeMap::new();
                let mut raw_identities = BTreeSet::new();
                for name in raw {
                    for (line, value) in lines(take_input(name, inputs, &mut used)?)? {
                        let row: CellResult = serde_json::from_value(value)
                            .map_err(|e| format!("raw {name}:{line}: {e}"))?;
                        require_raw(&row, run_id, hermit_sha)?;
                        if !raw_identities.insert((raw_cell(&row)?, row.run_index, row.attempt)) {
                            return Err(
                                "raw census repeats a cell/repetition/outer-attempt identity"
                                    .into(),
                            );
                        }
                        raw_rows.insert(
                            RawRow {
                                input: name.clone(),
                                line,
                            },
                            row,
                        );
                    }
                }
                let mut covered = BTreeSet::new();
                let mut linked_events = BTreeSet::new();
                for binding in bindings {
                    if !linked_events.insert(&binding.event_id) {
                        return Err("repeated event binding".into());
                    }
                    let value = result
                        .events
                        .get(&binding.event_id)
                        .ok_or("binding names an absent event")?;
                    let event: SeriesRow =
                        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    if event.run_id != *run_id
                        || event.series.tree != *hermit_sha
                        || event.series.num_runs != binding.rows.len() as u64
                    {
                        return Err(
                            "event source/run/compressed population differs from raw binding"
                                .into(),
                        );
                    }
                    let mut repetitions = BTreeSet::new();
                    for reference in &binding.rows {
                        if !covered.insert(reference.clone()) {
                            return Err("raw row represented more than once".into());
                        }
                        let row = raw_rows
                            .get(reference)
                            .ok_or("event names absent raw row")?;
                        let repetition = row.run_index.unwrap_or(row.attempt);
                        if raw_cell(row)? != event.series.cell
                            || row.result != event.series.result
                            || row.failure_class != event.series.failure_class
                            || row.source_tree_dirty != event.series.source_tree_dirty
                            || row.machine_shortname != event.host
                            || !repetitions.insert(repetition)
                            || event.series.kernel_version.as_deref() != Some(&row.kernel_version)
                            || event.series.host_capabilities.as_ref()
                                != Some(&row.host_capabilities)
                            || repetition < event.series.run_index
                            || repetition
                                > event
                                    .series
                                    .last_run_index
                                    .unwrap_or(event.series.run_index)
                            || event
                                .series
                                .attempt
                                .is_some_and(|attempt| attempt != row.attempt)
                        {
                            return Err("series/raw identity, classification, host or compressed span mismatch".into());
                        }
                    }
                }
                if linked_events.len() != result.events.len() {
                    return Err("event omitted its raw binding".into());
                }
                for excluded in unprojected {
                    if excluded.reason.trim().is_empty()
                        || !raw_rows.contains_key(&excluded.row)
                        || !covered.insert(excluded.row.clone())
                    {
                        return Err("invalid unprojected raw disposition".into());
                    }
                }
                if covered != raw_rows.keys().cloned().collect() {
                    return Err("raw census contains omitted/superseded rows".into());
                }
                if let Some(proof) = finalized {
                    let row: HistoryRow = decode(take_input(&proof.row, inputs, &mut used)?)?;
                    if row.run_id.as_deref() != Some(run_id)
                        || row.commit.as_deref() != Some(hermit_sha)
                        || row.finished_at.as_deref().is_none_or(str::is_empty)
                    {
                        return Err("finalized row source/run/finish differs".into());
                    }
                    let verified = row
                        .verify_schema10_artifact_bytes(
                            take_input(&proof.plan, inputs, &mut used)?,
                            take_input(&proof.cells, inputs, &mut used)?,
                            take_input(&proof.tests, inputs, &mut used)?,
                        )?
                        .ok_or("finalized evidence is not schema10")?;
                    let raw_population: BTreeSet<_> = raw_rows
                        .values()
                        .map(|r| crate::ledger::CellIdentity {
                            lane: r.lane.clone(),
                            category: r.category.clone(),
                            test: r.test.clone(),
                            mode: r.mode.clone(),
                            backend: r.backend.clone().unwrap_or_else(|| "native".into()),
                        })
                        .collect();
                    let recorded_population: BTreeSet<_> = verified
                        .cell_results
                        .cells
                        .iter()
                        .map(|r| crate::ledger::CellIdentity {
                            lane: r.lane.clone(),
                            category: r.category.clone(),
                            test: r.test.clone(),
                            mode: r.mode.clone(),
                            backend: r.backend.clone(),
                        })
                        .collect();
                    if raw_population != recorded_population {
                        return Err("raw population differs from finalized recorded cells (unexecuted planned cells remain unexecuted)".into());
                    }
                    if raw_rows.is_empty()
                        && (verified.cell_results.selected_count != 0
                            || verified.cell_results.recorded_count != 0
                            || !verified.missing_cells.is_empty())
                    {
                        return Err(
                            "empty current census is not a verified zero-selected population"
                                .into(),
                        );
                    }
                } else {
                    if raw_rows.is_empty() {
                        return Err(
                            "empty current input requires finalized zero-selection proof".into(),
                        );
                    }
                    result.unqualified = true;
                }
            }
        }
        if used != names {
            return Err("declared input has no retained evidence role".into());
        }
        Ok(result)
    }
}

/// All store operations stay relative to held no-follow directory descriptors.
/// A replaced path cannot redirect a later write into somebody else's directory.
struct Dir(File);

// flock belongs to the open file description, which an unrelated fork can
// inherit. Closing only our descriptor need not release it. Normal outcomes
// therefore explicitly unlock in the acquiring process before acknowledgement.
struct HistoryLock {
    file: Option<File>,
    owner: libc::pid_t,
}

impl HistoryLock {
    fn finish<T>(mut self, result: Result<T>) -> Result<T> {
        let release = self.release();
        match (result, release) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(release)) => Err(format!("{error}; additionally {release}")),
        }
    }

    fn release(&mut self) -> Result<()> {
        if unsafe { libc::getpid() } != self.owner {
            return Err("inherited history lock cannot be released by another process".into());
        }
        let file = self.file.take().ok_or("history lock already released")?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(io_error("release history lock"));
        }
        Ok(())
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        // Panic/unwind fallback only: all ordinary success AND error outcomes
        // use finish so an unlock failure is reported. A child dropping its
        // inherited guard must never unlock the parent's active critical section.
        // Abrupt owner death with an inherited descriptor is not solved here.
        if unsafe { libc::getpid() } != self.owner {
            return;
        }
        if let Some(file) = &self.file {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn c_name(name: &str) -> Result<std::ffi::CString> {
    if !component(name) {
        return Err(format!("invalid store filename {name:?}"));
    }
    std::ffi::CString::new(name).map_err(|e| e.to_string())
}

fn io_error(operation: &str) -> String {
    format!("{operation}: {}", std::io::Error::last_os_error())
}

impl Dir {
    fn open(path: &Path, create: bool) -> Result<Self> {
        if !path.is_absolute() {
            return Err("store/input path must be absolute".into());
        }
        if create {
            // The caller supplies an existing, durable parent. Creating or
            // syncing arbitrary ancestors would claim ownership of unrelated
            // mounts (including autofs) and cannot establish their durability.
            let parent = path.parent().ok_or("store path must name a leaf")?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("store path must name a UTF-8 leaf")?;
            let name = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
            let parent = Self::open(parent, false)
                .map_err(|e| format!("store parent must already exist and be readable: {e}"))?;
            return parent.child_c(&name, true);
        }
        let mut dir = Self(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open("/")
                .map_err(|e| e.to_string())?,
        );
        for part in path.components() {
            match part {
                Component::RootDir => (),
                Component::Normal(name) => {
                    let name = name.to_str().ok_or("non-UTF8 directory component")?;
                    // Directory names may contain spaces; only traversal/NUL is
                    // forbidden. Store-controlled leaf names are stricter.
                    let name = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
                    dir = dir.child_c(&name, false)?;
                }
                _ => return Err("directory path contains traversal".into()),
            }
        }
        Ok(dir)
    }

    fn child_c(&self, name: &std::ffi::CStr, create: bool) -> Result<Self> {
        if create {
            // SAFETY: name is NUL-terminated and both operations are relative
            // to this live directory descriptor, with symlinks refused below.
            let rc = unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o700) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io_error("mkdirat"));
            }
        }
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io_error("open directory"));
        }
        let child = Self(unsafe { File::from_raw_fd(fd) });
        if create {
            // EEXIST may be the visible result of an interrupted mkdir. Complete
            // both durability obligations before relying on this path component.
            child.sync()?;
            self.sync()?;
        }
        Ok(child)
    }

    fn child(&self, name: &str, create: bool) -> Result<Self> {
        self.child_c(&c_name(name)?, create)
    }
    fn sync(&self) -> Result<()> {
        self.0
            .sync_all()
            .map_err(|e| format!("directory fsync: {e}"))
    }

    fn names(&self) -> Result<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
            .map_err(|e| e.to_string())?
        {
            if names.len() >= MAX_ENTRIES {
                return Err("directory exceeds the explicit entry bound".into());
            }
            let name = entry
                .map_err(|e| e.to_string())?
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF8 store entry")?;
            c_name(&name)?;
            names.insert(name);
        }
        Ok(names)
    }

    /// Sorted, complete directory traversal with bounded memory. A page scans
    /// the directory but retains only its next smallest names. The cursor is
    /// lexical, not an unstable readdir offset. The caller holds the store lock.
    fn for_each_name(&self, visit: impl FnMut(&str) -> Result<()>) -> Result<DirectoryFrontier> {
        self.for_each_name_paged(MAX_ENTRIES, visit)
    }

    fn for_each_name_paged(
        &self,
        page_size: usize,
        mut visit: impl FnMut(&str) -> Result<()>,
    ) -> Result<DirectoryFrontier> {
        if page_size == 0 || page_size > MAX_ENTRIES {
            return Err("invalid directory traversal page size".into());
        }
        let mut cursor = String::new();
        let mut count = 0_u64;
        let mut hasher = Sha256::new();
        loop {
            let mut page = BTreeSet::new();
            for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
                .map_err(|e| e.to_string())?
            {
                let name = entry
                    .map_err(|e| e.to_string())?
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF8 store entry")?;
                c_name(&name)?;
                if name > cursor {
                    page.insert(name);
                    if page.len() > page_size {
                        page.pop_last();
                    }
                }
            }
            if page.is_empty() {
                break;
            }
            for name in &page {
                visit(name)?;
                count = count.checked_add(1).ok_or("directory count overflow")?;
                hasher.update(name.as_bytes());
                hasher.update([0]);
            }
            cursor = page.pop_last().ok_or("directory page lost its cursor")?;
        }
        Ok(DirectoryFrontier {
            count,
            sha256: format!("{:x}", hasher.finalize()),
        })
    }

    fn file(&self, name: &str, create: bool) -> Result<File> {
        let name = c_name(name)?;
        let flags = if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        };
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io_error("open file"));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("input must be a regular single-link file".into());
        }
        Ok(file)
    }

    fn read(&self, name: &str) -> Result<Vec<u8>> {
        self.read_held(name, &mut self.file(name, false)?)
    }

    fn read_held(&self, name: &str, file: &mut File) -> Result<Vec<u8>> {
        file.rewind().map_err(|e| e.to_string())?;
        let before = file.metadata().map_err(|e| e.to_string())?;
        if before.len() > MAX_BLOB_BYTES {
            return Err("input exceeds byte bound".into());
        }
        let mut bytes = Vec::new();
        (&mut *file)
            .take(MAX_BLOB_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let after = file.metadata().map_err(|e| e.to_string())?;
        let named = self
            .file(name, false)?
            .metadata()
            .map_err(|e| e.to_string())?;
        if stamp(&before) != stamp(&after)
            || stamp(&after) != stamp(&named)
            || after.len() != bytes.len() as u64
        {
            return Err("input changed during read".into());
        }
        Ok(bytes)
    }

    fn write_once(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.write_once_with_checkpoint(name, bytes, || Ok(()))
    }

    fn write_once_with_checkpoint(
        &self,
        name: &str,
        bytes: &[u8],
        before_sync: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let exists = self.names()?.contains(name);
        let mut file = self.file(name, !exists)?;
        if exists {
            if self.read_held(name, &mut file)? != bytes {
                return Err(format!("existing immutable input {name:?} differs"));
            }
        } else {
            file.write_all(bytes).map_err(|e| e.to_string())?;
        }
        // Matching cached bytes are not a durability receipt. The preceding
        // writer may have stopped after write_all but before this file fsync.
        before_sync()?;
        file.sync_all()
            .map_err(|e| format!("immutable file fsync: {e}"))?;
        self.sync()?;
        if self.read_held(name, &mut file)? != bytes {
            return Err("immutable write readback differs".into());
        }
        Ok(())
    }

    fn same_path(&self, path: &Path) -> Result<()> {
        let expected = self.0.metadata().map_err(|e| e.to_string())?;
        let actual = Self::open(path, false)?
            .0
            .metadata()
            .map_err(|e| e.to_string())?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            return Err("directory name moved during operation".into());
        }
        Ok(())
    }

    fn lock(&self, create: bool, exclusive: bool) -> Result<HistoryLock> {
        if create {
            self.write_once("lock", b"cell-history/v1\n")?;
        }
        let file = self.file("lock", false)?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        if unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) } != 0 {
            return Err(io_error(
                "history is busy; retry the same immutable request after its writer finishes",
            ));
        }
        Ok(HistoryLock {
            file: Some(file),
            owner: unsafe { libc::getpid() },
        })
    }

    fn with_lock<T>(
        &self,
        create: bool,
        exclusive: bool,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let lock = self.lock(create, exclusive)?;
        lock.finish(operation())
    }
}

fn stamp(meta: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

fn git_output(repository: &str, args: &[&str], limit: u64) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repository)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("read Git source: {e}"))?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or("Git stdout missing")?
        .take(limit + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > limit {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    read.map_err(|e| e.to_string())?;
    if !status.success() || bytes.len() as u64 > limit {
        return Err("Git source unavailable or exceeds byte bound".into());
    }
    Ok(bytes)
}

fn source_inputs(batch: &Batch, directory: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let source = Dir::open(directory, false)?;
    let expected: BTreeSet<_> = batch
        .inputs
        .iter()
        .filter(|i| !matches!(i.origin, InputOrigin::Git { .. }))
        .map(|i| i.name.clone())
        .collect();
    if source.names()? != expected {
        return Err("source directory is missing or contains undeclared files".into());
    }
    let mut inputs = BTreeMap::new();
    let mut total = 0_u64;
    for input in &batch.inputs {
        if !component(&input.name) || input.content.bytes > MAX_BLOB_BYTES {
            return Err("invalid input name/size".into());
        }
        let bytes = match &input.origin {
            InputOrigin::Git {
                repository,
                commit,
                path,
                blob,
            } => {
                if !Path::new(repository).is_absolute()
                    || !hex(commit, 40)
                    || !hex(blob, 40)
                    || Path::new(path).is_absolute()
                    || Path::new(path)
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                    || path.is_empty()
                {
                    return Err("invalid exact Git anchor".into());
                }
                if git_output(repository, &["cat-file", "-t", commit], 128)? != b"commit\n" {
                    return Err("Git anchor is not a commit".into());
                }
                let object = format!("{commit}:{path}");
                if git_output(repository, &["rev-parse", "--verify", &object], 128)?
                    != format!("{blob}\n").as_bytes()
                {
                    return Err("Git commit/path does not name the declared blob".into());
                }
                if git_output(repository, &["cat-file", "-t", blob], 128)? != b"blob\n" {
                    return Err("Git archive source is not a blob".into());
                }
                git_output(repository, &["cat-file", "blob", blob], input.content.bytes)?
            }
            InputOrigin::RetainedFile { original_path } => {
                if original_path.trim().is_empty() {
                    return Err("retained legacy source omitted original path".into());
                }
                source.read(&input.name)?
            }
            InputOrigin::ProducerFile => source.read(&input.name)?,
        };
        input.content.verify(&bytes)?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("input size overflow")?;
        if total > MAX_TOTAL_BYTES {
            return Err("batch exceeds total byte bound".into());
        }
        if inputs.insert(input.name.clone(), bytes).is_some() {
            return Err("duplicate input name".into());
        }
    }
    if source.names()? != expected {
        return Err("source census moved while capturing".into());
    }
    source.same_path(directory)?;
    Ok(inputs)
}

fn merge_events(
    target: &mut BTreeMap<String, Value>,
    incoming: BTreeMap<String, Value>,
) -> Result<()> {
    for (id, value) in incoming {
        if let Some(old) = target.get(&id) {
            if old != &value {
                return Err(format!("conflicting body for existing event {id:?}"));
            }
        } else {
            target.insert(id, value);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct DirectoryFrontier {
    count: u64,
    sha256: String,
}

struct StoredBatch {
    receipt: BatchReceipt,
    validated: Validated,
    inputs: BTreeMap<String, Vec<u8>>,
}

fn load_batch(id: &str, dir: &Dir) -> Result<StoredBatch> {
    if !hex(id, 64) {
        return Err("unexpected committed batch name".into());
    }
    let manifest = dir.read("manifest.json")?;
    if digest(&manifest) != id {
        return Err("committed manifest identity differs".into());
    }
    let batch: Batch = decode(&manifest)?;
    let mut inputs = BTreeMap::new();
    let mut expected = BTreeSet::from(["manifest.json".to_owned()]);
    let mut total = 0_u64;
    for input in &batch.inputs {
        let name = format!("{}.blob", input.content.sha256);
        expected.insert(name.clone());
        let bytes = dir.read(&name)?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("batch size overflow")?;
        if total > MAX_TOTAL_BYTES {
            return Err("batch exceeds explicit total byte bound".into());
        }
        if inputs.insert(input.name.clone(), bytes).is_some() {
            return Err("duplicate input name".into());
        }
    }
    if dir.names()? != expected {
        return Err("committed batch contains undeclared files".into());
    }
    let validated = batch.validate(id, &inputs)?;
    Ok(StoredBatch {
        receipt: BatchReceipt {
            id: id.into(),
            identity: batch.identity,
            manifest: Blob::of(&manifest),
            inputs: batch.inputs,
        },
        validated,
        inputs,
    })
}

fn visit_batches(
    root: &Dir,
    mut visit: impl FnMut(StoredBatch) -> Result<()>,
) -> Result<DirectoryFrontier> {
    if root.names()? != BTreeSet::from(["lock".into(), "batches".into(), "incomplete".into()]) {
        return Err("store contains missing or unrecognized top-level state".into());
    }
    let batches = root.child("batches", false)?;
    batches.for_each_name(|id| visit(load_batch(id, &batches.child(id, false)?)?))
}

fn snapshot_locked(root: &Dir) -> Result<Snapshot> {
    if root.names()? != BTreeSet::from(["lock".into(), "batches".into(), "incomplete".into()]) {
        return Err("store contains missing or unrecognized top-level state".into());
    }
    let mut events = BTreeMap::new();
    let mut receipts = Vec::new();
    let mut archives = Vec::new();
    let mut unqualified = Vec::new();
    let mut current_event_ids = BTreeSet::new();
    let mut total = 0_u64;
    let frontier = visit_batches(root, |batch| {
        for input in &batch.receipt.inputs {
            total = total
                .checked_add(input.content.bytes)
                .ok_or("snapshot size overflow")?;
            if total > MAX_TOTAL_BYTES {
                return Err("in-memory snapshot exceeds its byte bound; use the complete streaming interface".into());
            }
        }
        if batch.validated.unqualified {
            current_event_ids.extend(batch.validated.events.keys().cloned());
            unqualified.push(batch.receipt.id.clone());
        }
        merge_events(&mut events, batch.validated.events)?;
        archives.extend(batch.validated.archives);
        receipts.push(batch.receipt);
        Ok(())
    })?;
    if frontier != root.child("batches", false)?.for_each_name(|_| Ok(()))? {
        return Err("committed batch set changed during snapshot".into());
    }
    let mut refusals = BTreeMap::new();
    for (id, value) in &events {
        let row: SeriesRow = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        if let Err(reason) = row.validate_for_projection() {
            refusals.insert(id.clone(), reason);
        }
    }
    for id in current_event_ids {
        refusals.insert(id,"current raw input is retained with caller-census consistency only; actual producer completeness has not been established".into());
    }
    let events: Vec<_> = events.into_values().collect();
    let event_bytes = serde_json::to_vec(&events).map_err(|e| e.to_string())?;
    Ok(Snapshot {
        schema: SNAPSHOT_SCHEMA.into(),
        event_count: events.len() as u64,
        events_sha256: digest(&event_bytes),
        events,
        batches: receipts,
        archives,
        incomplete: root
            .child("incomplete", false)?
            .names()?
            .into_iter()
            .collect(),
        event_projection_refusals: refusals,
        unqualified_current_batches: unqualified,
    })
}

/// Read only: an absent store is unavailable, never an empty history. Locks are
/// existing descriptors; reads never create directories, fetch, append or reap.
pub fn snapshot(path: &Path) -> Result<Snapshot> {
    let root = Dir::open(path, false)?;
    root.with_lock(false, false, || {
        let result = snapshot_locked(&root)?;
        root.same_path(path)?;
        Ok(result)
    })
}

/// Verify a caller-retained snapshot against the complete current frontier.
/// This is a read-only consistency check, not a lease: a later append requires
/// a fresh snapshot. It never upgrades current batches or legacy exclusions.
/// Unknown fields/versions and changes to any retained attachment, archive,
/// disposition or batch identity refuse, even if the event digest still agrees.
pub fn verify_snapshot(path: &Path, bytes: &[u8]) -> Result<Snapshot> {
    let expected: Snapshot = decode(bytes)?;
    if expected.schema != SNAPSHOT_SCHEMA {
        return Err("unsupported cell-history snapshot schema".into());
    }
    let current = snapshot(path)?;
    if current != expected {
        return Err("snapshot differs from the complete current store frontier".into());
    }
    Ok(current)
}

/// Complete history is a stream, not a whole-history allocation. A consumer must
/// observe the terminal seal and successful command/callback completion. Earlier
/// records alone are unqualified partial output.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamRecord {
    Start {
        schema: String,
        cell: Option<String>,
    },
    Batch {
        receipt: BatchReceipt,
        current_unqualified: bool,
    },
    ArchiveCell {
        batch: String,
        input: Input,
        cell: String,
        value: Value,
        projection: String,
    },
    Event {
        event: Value,
        projection_refusal: Option<String>,
    },
    Incomplete {
        identity: String,
    },
    Seal {
        receipt: StreamSeal,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSeal {
    pub schema: String,
    pub cell: Option<String>,
    pub body_records: u64,
    pub sha256: String,
    pub frontier_sha256: String,
    pub source_batches: u64,
    pub source_events: u64,
    /// Original archived cell rows, not reconciled or additive event evidence.
    pub source_archive_cells: u64,
    pub emitted_events: u64,
    pub emitted_archive_cells: u64,
    pub incomplete_batches: u64,
    pub projection_ready: bool,
    pub partition_scans: u64,
    pub partitions: u64,
    /// Actual repeated input bytes validated, exposing the rescan cost.
    pub validated_input_bytes: u64,
}

struct StreamWriter<F> {
    emit: F,
    hasher: Sha256,
    seal: StreamSeal,
}

impl<F: FnMut(&StreamRecord) -> Result<()>> StreamWriter<F> {
    fn record(&mut self, record: StreamRecord) -> Result<()> {
        let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        if bytes.len() as u64 >= MAX_BLOB_BYTES {
            return Err("stream record exceeds per-object byte bound".into());
        }
        self.hasher.update(&bytes);
        self.hasher.update(b"\n");
        self.seal.body_records = self
            .seal
            .body_records
            .checked_add(1)
            .ok_or("stream count overflow")?;
        (self.emit)(&record)
    }

    fn observed_batch(&mut self, batch: &StoredBatch) -> Result<()> {
        for input in &batch.receipt.inputs {
            self.seal.validated_input_bytes = self
                .seal
                .validated_input_bytes
                .checked_add(input.content.bytes)
                .ok_or("scan byte count overflow")?;
        }
        Ok(())
    }

    fn events(&mut self, root: &Dir, prefix: &str) -> Result<()> {
        self.seal.partition_scans = self
            .seal
            .partition_scans
            .checked_add(1)
            .ok_or("partition count overflow")?;
        let mut events: BTreeMap<(String, String), (Value, Option<String>)> = BTreeMap::new();
        let mut bytes = 0_usize;
        let mut split = false;
        let batches = root.child("batches", false)?;
        batches.for_each_name(|id| {
            let dir = batches.child(id, false)?;
            // Archives were fully validated/emitted in the initial pass and are
            // fully reverified before the seal. They contain no event IDs.
            let manifest = dir.read("manifest.json")?;
            if !hex(id, 64) || digest(&manifest) != id { return Err("committed manifest identity differs".into()); }
            let declaration: Batch = decode(&manifest)?;
            if matches!(declaration.data, BatchKind::LegacyCells { .. }) { return Ok(()); }
            let batch = load_batch(id, &dir)?;
            self.observed_batch(&batch)?;
            if split { return Ok(()); }
            for (id, event) in batch.validated.events {
                let key = digest(id.as_bytes());
                if !key.starts_with(prefix) { continue; }
                let row: SeriesRow = serde_json::from_value(event.clone()).map_err(|e| e.to_string())?;
                let refusal = if batch.validated.unqualified {
                    Some("current raw input is retained with caller-census consistency only; actual producer completeness has not been established".into())
                } else { row.validate_for_projection().err() };
                let entry_key = (key, id);
                if let Some((prior, prior_refusal)) = events.get_mut(&entry_key) {
                    if prior != &event { return Err(format!("conflicting body for existing event {:?}", entry_key.1)); }
                    if refusal.is_some() { *prior_refusal = refusal; }
                } else {
                    bytes = bytes.checked_add(serde_json::to_vec(&event).map_err(|e| e.to_string())?.len()).ok_or("partition byte count overflow")?;
                    events.insert(entry_key, (event, refusal));
                }
                // A single event retains the existing per-object limit. It is
                // never split, truncated or rewritten to meet a page target.
                if events.len() > PARTITION_EVENTS || (bytes > PARTITION_BYTES && events.len() > 1) {
                    split = true;
                    events.clear();
                    break;
                }
            }
            Ok(())
        })?;
        if split {
            if prefix.len() >= 64 {
                return Err("digest-collision partition exceeds its bounded representation".into());
            }
            for nibble in b"0123456789abcdef" {
                self.events(root, &format!("{prefix}{}", char::from(*nibble)))?;
            }
            return Ok(());
        }
        if !events.is_empty() {
            self.seal.partitions += 1;
        }
        for (_, (event, refusal)) in events {
            self.seal.source_events = self
                .seal
                .source_events
                .checked_add(1)
                .ok_or("event count overflow")?;
            if refusal.is_some() {
                self.seal.projection_ready = false;
            }
            let selected = self.seal.cell.as_deref().is_none_or(|cell| {
                event
                    .get("series")
                    .and_then(|s| s.get("cell"))
                    .and_then(Value::as_str)
                    == Some(cell)
            });
            if selected {
                self.seal.emitted_events += 1;
                self.record(StreamRecord::Event {
                    event,
                    projection_refusal: refusal,
                })?;
            }
        }
        Ok(())
    }
}

/// Pure complete traversal with bounded per-batch and recursive partition
/// memory. Every prefix is visited internally; no prefix can stand in for the
/// complete history. An optional cell filter is carried in both start and seal.
/// Repeated scans are explicit in the seal. No query cache or reaper is created.
pub fn stream_snapshot(
    path: &Path,
    cell: Option<&str>,
    emit: impl FnMut(&StreamRecord) -> Result<()>,
) -> Result<StreamSeal> {
    if cell.is_some_and(str::is_empty) {
        return Err("empty cell filter".into());
    }
    let root = Dir::open(path, false)?;
    let mut writer = StreamWriter {
        emit,
        hasher: Sha256::new(),
        seal: StreamSeal {
            schema: STREAM_SCHEMA.into(),
            cell: cell.map(str::to_owned),
            body_records: 0,
            sha256: String::new(),
            frontier_sha256: String::new(),
            source_batches: 0,
            source_events: 0,
            source_archive_cells: 0,
            emitted_events: 0,
            emitted_archive_cells: 0,
            incomplete_batches: 0,
            projection_ready: true,
            partition_scans: 0,
            partitions: 0,
            validated_input_bytes: 0,
        },
    };
    root.with_lock(false, false, || {
        writer.record(StreamRecord::Start {
            schema: STREAM_SCHEMA.into(),
            cell: cell.map(str::to_owned),
        })?;
        let frontier = visit_batches(&root, |batch| {
            writer.observed_batch(&batch)?;
            if batch.validated.unqualified {
                writer.seal.projection_ready = false;
            }
            writer.record(StreamRecord::Batch {
                receipt: batch.receipt,
                current_unqualified: batch.validated.unqualified,
            })?;
            for archive in batch.validated.archives {
                writer.seal.projection_ready = false;
                for (key, value) in archive.cells {
                    writer.seal.source_archive_cells += 1;
                    if cell.is_none_or(|filter| filter == key) {
                        writer.seal.emitted_archive_cells += 1;
                        writer.record(StreamRecord::ArchiveCell {
                            batch: archive.batch.clone(),
                            input: archive.input.clone(),
                            cell: key,
                            value,
                            projection: archive.projection.clone(),
                        })?;
                    }
                }
            }
            Ok(())
        })?;
        let incomplete = root.child("incomplete", false)?;
        let pending = incomplete.for_each_name(|name| {
            if !hex(name, 64) {
                return Err("unexpected incomplete batch name".into());
            }
            writer.seal.projection_ready = false;
            writer.record(StreamRecord::Incomplete {
                identity: name.into(),
            })
        })?;
        writer.events(&root, "")?;
        let after = visit_batches(&root, |batch| writer.observed_batch(&batch))?;
        if after != frontier || incomplete.for_each_name(|_| Ok(()))? != pending {
            return Err("complete history frontier moved during streaming snapshot".into());
        }
        root.same_path(path)?;
        writer.seal.source_batches = frontier.count;
        writer.seal.incomplete_batches = pending.count;
        writer.seal.frontier_sha256 = digest(
            format!(
                "{}:{}:{}:{}",
                frontier.count, frontier.sha256, pending.count, pending.sha256
            )
            .as_bytes(),
        );
        Ok(())
    })?;
    // This seal describes the complete frontier captured under the lock. A
    // concurrent append after release belongs to a newer snapshot; no further
    // store reads contribute to this seal. Unlock errors never emit a seal.
    writer.seal.sha256 = format!("{:x}", writer.hasher.finalize());
    (writer.emit)(&StreamRecord::Seal {
        receipt: writer.seal.clone(),
    })?;
    Ok(writer.seal)
}

/// Compare a retained stream to fresh complete authority without loading it in
/// memory. File digest, strict records, explicit filter, terminal seal, EOF and
/// original file generation are all required. A truncated copy cannot qualify.
pub fn verify_stream_file(store: &Path, path: &Path, expected_sha256: &str) -> Result<StreamSeal> {
    if !hex(expected_sha256, 64) {
        return Err("invalid retained stream digest".into());
    }
    let parent = path.parent().ok_or("stream has no parent")?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("stream has no UTF8 filename")?;
    let dir = Dir::open(parent, false)?;
    let file = dir.file(name, false)?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut read_record = || -> Result<Vec<u8>> {
        let mut line = Vec::new();
        (&mut reader)
            .take(MAX_BLOB_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if line.is_empty() || line.len() as u64 > MAX_BLOB_BYTES {
            return Err("retained stream truncated or record exceeds byte bound".into());
        }
        hasher.update(&line);
        Ok(line)
    };
    let first = read_record()?;
    let StreamRecord::Start { schema, cell } = decode(&first)? else {
        return Err("retained stream omitted its start record".into());
    };
    if schema != STREAM_SCHEMA {
        return Err("unsupported history stream schema".into());
    }
    let mut pending = Some(first);
    let seal = stream_snapshot(store, cell.as_deref(), |record| {
        let actual = if let Some(first) = pending.take() {
            first
        } else {
            read_record()?
        };
        let mut expected = serde_json::to_vec(record).map_err(|e| e.to_string())?;
        expected.push(b'\n');
        if actual != expected {
            return Err("retained stream differs from complete current authority".into());
        }
        Ok(())
    })?;
    let mut extra = [0];
    if reader.read(&mut extra).map_err(|e| e.to_string())? != 0 {
        return Err("retained stream has trailing records after its seal".into());
    }
    if format!("{:x}", hasher.finalize()) != expected_sha256 {
        return Err("retained stream digest differs".into());
    }
    let after = reader.get_ref().metadata().map_err(|e| e.to_string())?;
    let named = dir
        .file(name, false)?
        .metadata()
        .map_err(|e| e.to_string())?;
    if stamp(&before) != stamp(&after) || stamp(&after) != stamp(&named) {
        return Err("retained stream file moved during verification".into());
    }
    dir.same_path(parent)?;
    Ok(seal)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamExport {
    pub receipt: StreamSeal,
    pub sha256: String,
    pub bytes: u64,
}

/// Explicitly retain a complete stream in a new caller-owned file. This is not
/// an implicit reader cache. Failure leaves partial output unacknowledged.
pub fn export_stream_file(store: &Path, path: &Path, cell: Option<&str>) -> Result<StreamExport> {
    let parent = path.parent().ok_or("stream destination has no parent")?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("stream destination has no UTF8 filename")?;
    let dir = Dir::open(parent, false)?;
    let mut output = std::io::BufWriter::new(dir.file(name, true)?);
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let receipt = stream_snapshot(store, cell, |record| {
        let mut line = serde_json::to_vec(record).map_err(|e| e.to_string())?;
        line.push(b'\n');
        output.write_all(&line).map_err(|e| e.to_string())?;
        hasher.update(&line);
        bytes = bytes
            .checked_add(line.len() as u64)
            .ok_or("stream size overflow")?;
        Ok(())
    })?;
    output.flush().map_err(|e| e.to_string())?;
    output.get_ref().sync_all().map_err(|e| e.to_string())?;
    dir.sync()?;
    let expected = output.get_ref().metadata().map_err(|e| e.to_string())?;
    let mut retained = dir.file(name, false)?;
    let before = retained.metadata().map_err(|e| e.to_string())?;
    let sha256 = format!("{:x}", hasher.finalize());
    let mut check = Sha256::new();
    let mut block = [0_u8; 64 * 1024];
    loop {
        let count = retained.read(&mut block).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        check.update(&block[..count]);
    }
    let after = retained.metadata().map_err(|e| e.to_string())?;
    let named = dir
        .file(name, false)?
        .metadata()
        .map_err(|e| e.to_string())?;
    if stamp(&expected) != stamp(&before)
        || stamp(&before) != stamp(&after)
        || stamp(&after) != stamp(&named)
        || bytes != after.len()
        || format!("{:x}", check.finalize()) != sha256
    {
        return Err("retained stream moved or differs from durable output".into());
    }
    dir.same_path(parent)?;
    Ok(StreamExport {
        receipt,
        sha256,
        bytes,
    })
}

#[derive(Clone, Copy, Debug)]
enum CommitStage {
    StagingCreated,
    BeforeInputSync,
    InputsDurable,
    BeforeManifestSync,
    ManifestDurable,
    Renamed,
    BeforePublicationSync,
    Published,
}

fn append_with_checkpoint(
    path: &Path,
    request: &[u8],
    source: &Path,
    mut checkpoint: impl FnMut(CommitStage) -> Result<()>,
) -> Result<BatchReceipt> {
    let batch: Batch = decode(request)?;
    let id = digest(request);
    // Validate all supplied bytes and identities before creating any store state.
    let inputs = source_inputs(&batch, source)?;
    let validated = batch.validate(&id, &inputs)?;
    let root = Dir::open(path, true)?;
    let initial_names = root.names()?;
    if !initial_names.is_subset(&BTreeSet::from([
        "lock".into(),
        "batches".into(),
        "incomplete".into(),
    ])) {
        return Err("explicit store destination contains unrelated state".into());
    }
    root.with_lock(true, true, || {
        let committed = root.child("batches", true)?;
        let incomplete = root.child("incomplete", true)?;
        let mut existing_receipt = None;
        visit_batches(&root, |old| {
            if old.receipt.identity == batch.identity && old.receipt.id != id {
                return Err(
                    "producer/import identity already committed a different byte census".into(),
                );
            }
            for (event_id, value) in &validated.events {
                if old
                    .validated
                    .events
                    .get(event_id)
                    .is_some_and(|prior| prior != value)
                {
                    return Err(format!("conflicting body for existing event {event_id:?}"));
                }
            }
            if old.receipt.id == id {
                existing_receipt = Some(old.receipt);
            }
            Ok(())
        })?;
        if existing_receipt.is_some() {
            let dir = committed.child(&id, false)?;
            if dir.read("manifest.json")? != request {
                return Err("existing request identity collision".into());
            }
            // Visibility can precede the prior writer's final syncs. Replay must
            // complete data and namespace durability even for a committed name.
            for input in &batch.inputs {
                dir.write_once_with_checkpoint(
                    &format!("{}.blob", input.content.sha256),
                    inputs.get(&input.name).ok_or("validated input missing")?,
                    || checkpoint(CommitStage::BeforeInputSync),
                )?;
            }
            dir.write_once_with_checkpoint("manifest.json", request, || {
                checkpoint(CommitStage::BeforeManifestSync)
            })?;
        } else {
            let stage = incomplete.child(&id, true)?;
            checkpoint(CommitStage::StagingCreated)?;
            let mut expected = BTreeSet::from(["manifest.json".to_owned()]);
            for input in &batch.inputs {
                let name = format!("{}.blob", input.content.sha256);
                expected.insert(name.clone());
                stage.write_once_with_checkpoint(
                    &name,
                    inputs.get(&input.name).ok_or("validated input missing")?,
                    || checkpoint(CommitStage::BeforeInputSync),
                )?;
            }
            checkpoint(CommitStage::InputsDurable)?;
            stage.write_once_with_checkpoint("manifest.json", request, || {
                checkpoint(CommitStage::BeforeManifestSync)
            })?;
            if stage.names()? != expected {
                return Err(
                    "incomplete request contains unexpected files; preserve for inspection".into(),
                );
            }
            stage.sync()?;
            checkpoint(CommitStage::ManifestDurable)?;
            // Re-read the entire declared source before publication. A changed
            // producer census cannot be certified by hashing its surviving subset.
            if source_inputs(&batch, source)? != inputs {
                return Err("source moved before durable publication".into());
            }
            root.same_path(path)?;
            let name = c_name(&id)?;
            let rc = unsafe {
                libc::renameat2(
                    incomplete.0.as_raw_fd(),
                    name.as_ptr(),
                    committed.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if rc != 0 {
                return Err(io_error("publish immutable batch without replacement"));
            }
            checkpoint(CommitStage::Renamed)?;
        }
        // Repeating a rename's visible result must also complete both parent syncs.
        // A sync error propagates before the receipt, including on identical replay.
        checkpoint(CommitStage::BeforePublicationSync)?;
        committed.sync()?;
        incomplete.sync()?;
        root.sync()?;
        checkpoint(CommitStage::Published)?;
        let readback = load_batch(&id, &committed.child(&id, false)?)?;
        root.same_path(path)?;
        Ok(readback.receipt)
    })
}

/// Acknowledgement follows fsync of inputs, manifest and both rename parents,
/// then strict readback. Failure leaves evidence in place; no path is removed.
pub fn append(path: &Path, request: &[u8], source: &Path) -> Result<BatchReceipt> {
    append_with_checkpoint(path, request, source, |_| Ok(()))
}

/// Read a sealed request without following source symlinks or accepting a
/// changing generation. The CLI additionally checks the caller's expected hash.
pub fn read_request(path: &Path) -> Result<Vec<u8>> {
    let parent = path.parent().ok_or("request has no parent")?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("request has no UTF8 filename")?;
    let dir = Dir::open(parent, false)?;
    let bytes = dir.read(name)?;
    dir.same_path(parent)?;
    Ok(bytes)
}

/// Return exact retained bytes, including archives and superseded raw attempts.
pub fn read_input(path: &Path, batch_id: &str, name: &str) -> Result<Vec<u8>> {
    if !hex(batch_id, 64) || !component(name) {
        return Err("invalid batch/input identity".into());
    }
    let root = Dir::open(path, false)?;
    root.with_lock(false, false, || {
        let batch = load_batch(
            batch_id,
            &root.child("batches", false)?.child(batch_id, false)?,
        )?;
        let bytes = batch
            .inputs
            .get(name)
            .ok_or("retained input identity not found")?
            .clone();
        root.same_path(path)?;
        Ok(bytes)
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    struct Fixture {
        root: PathBuf,
        inputs: PathBuf,
        store: PathBuf,
        request: Vec<u8>,
    }
    impl Fixture {
        fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "hermit-cell-history-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            let inputs = root.join("inputs");
            std::fs::create_dir(&inputs).unwrap();
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema":"stress-series/v1","event_id":"old-event","event_type":"series.observation",
                "emitted_at":"2026-09-20T00:00:00Z","team":"hermit","host":"fixture","producer":"validate","run_id":"old-run",
                "series":{"cell":"fixture/test/naked/native","tree":"a".repeat(40),"outcome":"passed","run_index":1,"num_runs":1}
            })).unwrap();
            std::fs::write(inputs.join("series.jsonl"), &bytes).unwrap();
            let batch = Batch {
                schema: BATCH_SCHEMA.into(),
                identity: "historical-import".into(),
                inputs: vec![Input {
                    name: "series.jsonl".into(),
                    content: Blob::of(&bytes),
                    origin: InputOrigin::RetainedFile {
                        original_path: "old/retained.jsonl".into(),
                    },
                }],
                data: BatchKind::LegacySeries {
                    series: vec!["series.jsonl".into()],
                },
            };
            Self {
                store: root.join("store"),
                root,
                inputs,
                request: serde_json::to_vec(&batch).unwrap(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    // Model an unrelated fork while a store operation holds its lock. The
    // child uses only async-signal-safe calls and retains inherited descriptors
    // until the parent explicitly releases and reaps it.
    struct HeldFork {
        pid: libc::pid_t,
        release: File,
    }

    impl HeldFork {
        fn new() -> Self {
            let mut pipe = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                unsafe {
                    libc::close(pipe[1]);
                    let mut byte = 0_u8;
                    let read = libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(if read == 1 { 0 } else { 2 });
                }
            }
            unsafe { libc::close(pipe[0]) };
            Self {
                pid,
                release: unsafe { File::from_raw_fd(pipe[1]) },
            }
        }

        fn finish(mut self) {
            let released = self.release.write_all(b"x");
            drop(self.release);
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(self.pid, &mut status, 0) }, self.pid);
            assert_eq!(status, 0, "held descriptor child failed");
            released.unwrap();
        }
    }

    #[test]
    fn durable_transitions_leave_visible_evidence_and_replay_exactly_once() {
        for wanted in [
            CommitStage::StagingCreated,
            CommitStage::BeforeInputSync,
            CommitStage::InputsDurable,
            CommitStage::BeforeManifestSync,
            CommitStage::ManifestDurable,
            CommitStage::Renamed,
            CommitStage::BeforePublicationSync,
            CommitStage::Published,
        ] {
            let f = Fixture::new();
            if matches!(wanted, CommitStage::StagingCreated) {
                // Visible mkdir without an fsync is a modeled interruption,
                // not a physical power-loss experiment.
                std::fs::create_dir(&f.store).unwrap();
            }
            let error = append_with_checkpoint(&f.store, &f.request, &f.inputs, |stage| {
                if std::mem::discriminant(&stage) == std::mem::discriminant(&wanted) {
                    Err("injected interruption before acknowledgement".into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(error.contains("interruption"));
            let interrupted = snapshot(&f.store).unwrap();
            if matches!(
                wanted,
                CommitStage::Renamed | CommitStage::BeforePublicationSync | CommitStage::Published
            ) {
                assert_eq!(interrupted.event_count, 1);
                assert!(interrupted.incomplete.is_empty());
            } else {
                assert_eq!(interrupted.event_count, 0);
                assert_eq!(interrupted.incomplete, vec![digest(&f.request)]);
            }
            let mut recovery = Vec::new();
            let receipt = append_with_checkpoint(&f.store, &f.request, &f.inputs, |stage| {
                recovery.push(stage);
                Ok(())
            })
            .unwrap();
            for required in [
                CommitStage::BeforeInputSync,
                CommitStage::BeforeManifestSync,
                CommitStage::BeforePublicationSync,
                CommitStage::Published,
            ] {
                assert!(
                    recovery
                        .iter()
                        .any(|stage| std::mem::discriminant(stage)
                            == std::mem::discriminant(&required)),
                    "recovery skipped {required:?}"
                );
            }
            assert_eq!(receipt.id, digest(&f.request));
            let after = snapshot(&f.store).unwrap();
            assert_eq!(after.event_count, 1);
            assert_eq!(after.batches.len(), 1);
            assert!(after.incomplete.is_empty());
            // Retained v1 stays available but does not gain modern host facts.
            assert!(after.event_projection_refusals["old-event"].contains("machine_shortname"));
            assert_eq!(
                read_input(&f.store, &receipt.id, "series.jsonl").unwrap(),
                std::fs::read(f.inputs.join("series.jsonl")).unwrap()
            );
            // Inject errors at each missing durability obligation on an
            // already-visible receipt. No error may become an acknowledgement.
            for failed in [
                CommitStage::BeforeInputSync,
                CommitStage::BeforeManifestSync,
                CommitStage::BeforePublicationSync,
            ] {
                let error = append_with_checkpoint(&f.store, &f.request, &f.inputs, |stage| {
                    if std::mem::discriminant(&stage) == std::mem::discriminant(&failed) {
                        Err("injected sync-boundary error".into())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
                assert!(error.contains("sync-boundary error"), "{failed:?}: {error}");
                assert_eq!(snapshot(&f.store).unwrap().event_count, 1);
                assert_eq!(
                    append(&f.store, &f.request, &f.inputs).unwrap().id,
                    receipt.id
                );
            }
        }
    }

    #[test]
    fn lock_release_is_owner_scoped_across_inherited_descriptors() {
        for fail_operation in [false, true] {
            let f = Fixture::new();
            let mut held = None;
            let original = append_with_checkpoint(&f.store, &f.request, &f.inputs, |stage| {
                if matches!(stage, CommitStage::Published) {
                    held = Some(HeldFork::new());
                    if fail_operation {
                        return Err("operation failed after publication".into());
                    }
                }
                Ok(())
            });
            let while_inherited = append(&f.store, &f.request, &f.inputs);
            // Reap before any assertion. No held child is used as a test retry.
            held.expect("publication checkpoint must execute").finish();
            let after_reap = append(&f.store, &f.request, &f.inputs);
            if fail_operation {
                assert_eq!(original.unwrap_err(), "operation failed after publication");
            } else {
                assert_eq!(original.unwrap().id, digest(&f.request));
            }
            assert_eq!(after_reap.unwrap().id, digest(&f.request));
            assert!(
                while_inherited.is_ok(),
                "completed parent operation retained its lock in an unrelated fork: {while_inherited:?}"
            );
            assert_eq!(while_inherited.unwrap().id, digest(&f.request));
        }
        let f = Fixture::new();
        append(&f.store, &f.request, &f.inputs).unwrap();
        let root = Dir::open(&f.store, false).unwrap();
        let lock = root.lock(false, true).unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // A fork copied this guard. Exercise its actual Drop implementation
            // using only getpid/flock/close, then _exit without Rust teardown.
            unsafe {
                drop(std::ptr::read(&lock));
                libc::_exit(0);
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(status, 0);
        let competitor = root.lock(false, true);
        assert!(
            competitor.is_err(),
            "inherited child released the parent's active lock"
        );
        let error = competitor.err().unwrap();
        assert!(error.contains("history is busy"), "{error}");
        lock.finish(Ok(())).unwrap();
        root.lock(false, true).unwrap().finish(Ok(())).unwrap();
    }

    #[test]
    fn stream_seal_keeps_its_locked_frontier_after_release() {
        let f = Fixture::new();
        append(&f.store, &f.request, &f.inputs).unwrap();
        let mut later = Fixture::new();
        let input = later.inputs.join("series.jsonl");
        let mut event: Value = serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
        event["event_id"] = Value::String("later-event".into());
        let bytes = serde_json::to_vec(&event).unwrap();
        std::fs::write(&input, &bytes).unwrap();
        let mut batch: Batch = decode(&later.request).unwrap();
        batch.identity = "later-import".into();
        batch.inputs[0].content = Blob::of(&bytes);
        later.request = serde_json::to_vec(&batch).unwrap();
        let mut seal_seen = false;
        let seal = stream_snapshot(&f.store, None, |record| {
            if let StreamRecord::Seal { receipt } = record {
                assert_eq!(receipt.source_batches, 1);
                assert_eq!(receipt.source_events, 1);
                // The complete old snapshot has released its lock. A new
                // append may now commit, without altering the captured seal.
                append(&f.store, &later.request, &later.inputs)?;
                seal_seen = true;
            }
            Ok(())
        })
        .unwrap();
        assert!(seal_seen);
        assert_eq!(seal.source_batches, 1);
        assert_eq!(seal.source_events, 1);
        let newer = snapshot(&f.store).unwrap();
        assert_eq!(newer.batches.len(), 2);
        assert_eq!(newer.event_count, 2);
    }

    #[test]
    fn source_movement_and_changed_request_identity_never_acknowledge_loss() {
        let f = Fixture::new();
        let original = std::fs::read(f.inputs.join("series.jsonl")).unwrap();
        let error = append_with_checkpoint(&f.store, &f.request, &f.inputs, |stage| {
            if matches!(stage, CommitStage::InputsDurable) {
                std::fs::write(f.inputs.join("series.jsonl"), b"").unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("byte census"));
        let after = snapshot(&f.store).unwrap();
        assert_eq!(after.event_count, 0);
        assert_eq!(after.incomplete.len(), 1);
        std::fs::write(f.inputs.join("series.jsonl"), &original).unwrap();
        append(&f.store, &f.request, &f.inputs).unwrap();
        let before = serde_json::to_value(snapshot(&f.store).unwrap()).unwrap();
        let mut request: Batch = decode(&f.request).unwrap();
        request.inputs[0].origin = InputOrigin::RetainedFile {
            original_path: "rewritten-origin".into(),
        };
        assert!(
            append(&f.store, &serde_json::to_vec(&request).unwrap(), &f.inputs)
                .unwrap_err()
                .contains("different byte census")
        );
        assert_eq!(
            serde_json::to_value(snapshot(&f.store).unwrap()).unwrap(),
            before
        );

        let moved = Fixture::new();
        let old_store = moved.root.join("moved-store");
        let error = append_with_checkpoint(&moved.store, &moved.request, &moved.inputs, |stage| {
            if matches!(stage, CommitStage::InputsDurable) {
                std::fs::rename(&moved.store, &old_store).unwrap();
                std::fs::create_dir(&moved.store).unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("directory name moved"), "{error}");
        assert!(snapshot(&moved.store).is_err());
        let preserved = snapshot(&old_store).unwrap();
        assert_eq!(preserved.event_count, 0);
        assert_eq!(preserved.incomplete, vec![digest(&moved.request)]);
    }

    #[test]
    fn schema_refuses_duplicate_keys_unknown_fields_and_unrelated_store() {
        assert!(
            decode::<Value>(br#"{"nested":{"x":1,"x":2}}"#)
                .unwrap_err()
                .contains("duplicate")
        );
        let f = Fixture::new();
        let mut request: Value = decode(&f.request).unwrap();
        request["pretend_complete"] = true.into();
        assert!(decode::<Batch>(&serde_json::to_vec(&request).unwrap()).is_err());
        assert!(!f.store.exists());
        let missing_parent = f.root.join("missing-parent");
        let nested_store = missing_parent.join("store");
        let error = append(&nested_store, &f.request, &f.inputs).unwrap_err();
        assert!(error.contains("store parent must already exist"), "{error}");
        assert!(!missing_parent.exists());
        std::fs::create_dir(&missing_parent).unwrap();
        append(&nested_store, &f.request, &f.inputs).unwrap();
        assert_eq!(snapshot(&nested_store).unwrap().event_count, 1);
        std::fs::create_dir(&f.store).unwrap();
        std::fs::write(f.store.join("foreign"), b"untouched").unwrap();
        assert!(
            append(&f.store, &f.request, &f.inputs)
                .unwrap_err()
                .contains("unrelated")
        );
        assert_eq!(
            std::fs::read(f.store.join("foreign")).unwrap(),
            b"untouched"
        );
        assert_eq!(std::fs::read_dir(&f.store).unwrap().count(), 1);
    }
    #[test]
    fn streaming_frontier_refuses_insertion_and_retains_every_directory_page() {
        let f = Fixture::new();
        let pages = f.root.join("pages");
        std::fs::create_dir(&pages).unwrap();
        for name in ["h", "g", "f", "e", "d", "c", "b", "a"] {
            std::fs::write(pages.join(name), b"preserved").unwrap();
        }
        let dir = Dir::open(&pages, false).unwrap();
        let mut visited = Vec::new();
        // Test-only smaller DIRECTORY page, calling the production iterator.
        // Event population/byte bounds are unchanged.
        let paged = dir
            .for_each_name_paged(3, |name| {
                visited.push(name.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, ["a", "b", "c", "d", "e", "f", "g", "h"]);
        assert_eq!(paged.count, 8);
        assert_eq!(paged, dir.for_each_name(|_| Ok(())).unwrap());
        let initial = dir
            .for_each_name_paged(3, |name| {
                if name == "a" {
                    std::fs::write(pages.join("0"), b"inserted behind cursor").unwrap();
                }
                Ok(())
            })
            .unwrap();
        assert_ne!(initial, dir.for_each_name(|_| Ok(())).unwrap());

        append(&f.store, &f.request, &f.inputs).unwrap();
        let mut sealed = false;
        let error = stream_snapshot(&f.store, None, |record| {
            if matches!(record, StreamRecord::Event { .. }) {
                std::fs::create_dir(f.store.join("incomplete").join("a".repeat(64))).unwrap();
            }
            sealed |= matches!(record, StreamRecord::Seal { .. });
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("frontier moved"), "{error}");
        assert!(!sealed);
        assert_eq!(
            read_input(&f.store, &digest(&f.request), "series.jsonl").unwrap(),
            std::fs::read(f.inputs.join("series.jsonl")).unwrap()
        );
    }
}
