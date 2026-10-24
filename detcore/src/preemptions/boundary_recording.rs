/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! File-format boundary between legacy editors and explicit slice records.
//! The scheduler consumes the complete stream; legacy editors must refuse it.
//! History-only replay and read-only inspection never rewrite that stream.

use std::path::Path;

use anyhow::Context;
use anyhow::bail;
use detcore_model::schedule::boundary::Schedule;
use serde::Deserialize;
use serde::Serialize;

use super::HistoryIdentity;
use super::PreemptionRecord;

/// The only format of a history that states the identity of its RCB
/// coordinates. Its sole top-level key replaces the required legacy
/// `per_thread`/`global` and the `slice_boundary_recording` marker, so a
/// reader built before it refuses the file instead of reading coordinates
/// in another unit.
const RCB_HISTORY_FORMAT: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityEnvelope<R> {
    rcb_history: IdentityBody<R>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityBody<R> {
    format: u32,
    identity: IdentityWire,
    /// The legacy record, or the `slice_boundary_recording` envelope.
    recording: R,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum IdentityWire {
    Known(String),
    Unqualified(UnqualifiedWire),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnqualifiedWire {
    unqualified: String,
}

impl From<&HistoryIdentity> for IdentityWire {
    fn from(identity: &HistoryIdentity) -> Self {
        match identity {
            HistoryIdentity::Known(metric) => Self::Known(metric.id().to_owned()),
            HistoryIdentity::Unqualified(reason) => Self::Unqualified(UnqualifiedWire {
                unqualified: reason.clone(),
            }),
        }
    }
}

impl TryFrom<IdentityWire> for HistoryIdentity {
    type Error = anyhow::Error;

    fn try_from(wire: IdentityWire) -> anyhow::Result<Self> {
        match wire {
            IdentityWire::Known(id) => reverie::pmu_config::RcbMetric::from_id(&id)
                .map(Self::Known)
                .ok_or_else(|| anyhow::anyhow!("unknown RCB coordinate identity {id:?}")),
            IdentityWire::Unqualified(UnqualifiedWire { unqualified }) => {
                Ok(Self::Unqualified(unqualified))
            }
        }
    }
}

/// Serializes `record` alone: in the `rcb_history` envelope if it states an
/// identity, otherwise in the legacy format.
pub(super) fn encode_record(record: &PreemptionRecord) -> serde_json::Result<String> {
    match record.identity() {
        None => serde_json::to_string(record),
        Some(identity) => serde_json::to_string(&IdentityEnvelope {
            rcb_history: IdentityBody {
                format: RCB_HISTORY_FORMAT,
                identity: identity.into(),
                recording: record,
            },
        }),
    }
}

/// Keep the preemption history and every existing run setting alongside the
/// complete boundary stream. No public access returns its legacy component:
/// doing so would let an editor silently discard the protocol and markers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "BoundaryWire", into = "BoundaryWire")]
pub struct BoundaryRecording {
    preemptions: PreemptionRecord,
    schedule: Schedule,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryWire {
    preemptions: PreemptionRecord,
    schedule: Schedule,
}

/// The wrapper is intentional. Old readers require top-level `per_thread`
/// and `global`; they therefore refuse this format even for zero markers.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    slice_boundary_recording: BoundaryRecording,
}

#[derive(Deserialize)]
struct MetadataProbe {
    // A present null must not look like an absent protocol marker.
    #[serde(default, deserialize_with = "present_boundary")]
    slice_boundary_recording: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_boundary")]
    rcb_history: Option<serde_json::Value>,
}

fn present_boundary<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

pub(super) fn read_settings<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read preemption record {}: {}", path.display(), e))?;
    settings_in(path, &bytes)
}

/// [`read_settings`] over `bytes` already read from `path`, which only names
/// them in errors.
pub(super) fn settings_in<T: serde::de::DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
) -> Result<T, String> {
    let probe: MetadataProbe = serde_json::from_slice(bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))?;
    if probe.slice_boundary_recording.is_none() && probe.rcb_history.is_none() {
        // Each old metadata reader still deserializes only its own field,
        // from the original bytes, preserving its legacy acceptance contract.
        return serde_json::from_slice(bytes)
            .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e));
    }
    let recording = Recording::decode(bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))?;
    serde_json::from_value(serde_json::to_value(recording.metadata()).map_err(|e| e.to_string())?)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))
}

impl BoundaryRecording {
    /// Combine a validated preemption history with the sole global record
    /// stream. The legacy history must not contain another global stream.
    pub fn new(preemptions: PreemptionRecord, schedule: Schedule) -> anyhow::Result<Self> {
        preemptions.validate().map_err(anyhow::Error::msg)?;
        if !preemptions.global.is_empty() {
            bail!("slice-boundary recording has a second, legacy global event stream");
        }
        Ok(Self {
            preemptions,
            schedule,
        })
    }

    /// A read-only copy of every recorded thread history. It contains no
    /// global stream and cannot flatten or rewrite the boundary protocol.
    pub fn thread_histories(
        &self,
    ) -> std::collections::BTreeMap<crate::types::DetTid, super::ThreadHistory> {
        self.preemptions.extract_all()
    }

    /// The complete stored stream, including boundary indices.
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }
}

impl TryFrom<BoundaryWire> for BoundaryRecording {
    type Error = anyhow::Error;

    fn try_from(wire: BoundaryWire) -> Result<Self, Self::Error> {
        Self::new(wire.preemptions, wire.schedule)
    }
}

impl From<BoundaryRecording> for BoundaryWire {
    fn from(recording: BoundaryRecording) -> Self {
        Self {
            preemptions: recording.preemptions,
            schedule: recording.schedule,
        }
    }
}

/// A recording whose format remains explicit through file transformations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recording {
    /// The unchanged legacy preemption/event representation.
    Legacy(PreemptionRecord),
    /// A versioned boundary stream and its associated preemption history.
    Boundaries(BoundaryRecording),
}

impl Recording {
    /// Require an explicit matching execution policy. The strict verifier must
    /// not silently fall back to an incomplete legacy stream, and ordinary
    /// recovery must not silently acquire strict EOF/desync semantics.
    pub fn require_strict_mode(&self, strict: bool) -> anyhow::Result<()> {
        match (self, strict) {
            (Self::Legacy(_), false) | (Self::Boundaries(_), true) => Ok(()),
            (Self::Legacy(_), true) => {
                anyhow::bail!("strict schedule boundaries require a complete boundary recording")
            }
            (Self::Boundaries(_), false) => {
                anyhow::bail!("complete boundary recording requires --strict-schedule-boundaries")
            }
        }
    }

    /// Validate and parse either format without a malformed-new-to-legacy
    /// fallback.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        // Inspect only to dispatch. Deserialize the ORIGINAL bytes again so
        // duplicate fields in the envelope are rejected, not overwritten by
        // Value. A malformed new recording never falls back to legacy.
        if value.get("rcb_history").is_some() {
            let envelope: IdentityEnvelope<Box<serde_json::value::RawValue>> =
                serde_json::from_slice(bytes)?;
            let body = envelope.rcb_history;
            if body.format != RCB_HISTORY_FORMAT {
                anyhow::bail!("unsupported rcb_history format {}", body.format);
            }
            let identity = HistoryIdentity::try_from(body.identity)?;
            let inner = body.recording.get().as_bytes();
            if serde_json::from_slice::<serde_json::Value>(inner)?
                .get("rcb_history")
                .is_some()
            {
                anyhow::bail!("nested rcb_history envelope");
            }
            let mut recording = Self::decode(inner)?;
            recording.metadata_mut().identity = Some(identity);
            return Ok(recording);
        }
        if value.get("slice_boundary_recording").is_some() {
            let envelope: Envelope = serde_json::from_slice(bytes)?;
            Ok(Self::Boundaries(envelope.slice_boundary_recording))
        } else {
            let legacy: PreemptionRecord = serde_json::from_slice(bytes)?;
            legacy.validate().map_err(anyhow::Error::msg)?;
            Ok(Self::Legacy(legacy))
        }
    }

    /// Read a complete recording, preserving errors as failures.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        Self::decode_from(
            path,
            &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )
    }

    /// Decode `bytes` already read from `path`, which names them in errors as
    /// [`Self::read`] does.
    pub fn decode_from(path: &Path, bytes: &[u8]) -> anyhow::Result<Self> {
        Self::decode(bytes).with_context(|| format!("reading schedule {}", path.display()))
    }

    /// Serialize the same format with the conventional terminal newline.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = match (self, self.metadata().identity()) {
            (Self::Legacy(record), _) => encode_record(record)?.into_bytes(),
            (Self::Boundaries(record), None) => serde_json::to_vec(&Envelope {
                slice_boundary_recording: record.clone(),
            })?,
            (Self::Boundaries(record), Some(identity)) => serde_json::to_vec(&IdentityEnvelope {
                rcb_history: IdentityBody {
                    format: RCB_HISTORY_FORMAT,
                    identity: identity.into(),
                    recording: Envelope {
                        slice_boundary_recording: record.clone(),
                    },
                },
            })?,
        };
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// The same recording, stating `identity` for its coordinates.
    pub fn with_identity(mut self, identity: Option<HistoryIdentity>) -> Self {
        self.metadata_mut().identity = identity;
        self
    }

    /// The identity of the recording's RCB coordinates; `None` for a legacy
    /// file, whose producer is unknown.
    pub fn identity(&self) -> Option<&HistoryIdentity> {
        self.metadata().identity()
    }

    /// Write the complete serialized recording, without changing its format.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        // Complete validation/serialization precedes opening the destination.
        let bytes = self.encode()?;
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    /// Give a legacy-only operation its input, or refuse a boundary format
    /// before that operation can discard its protocol or write an output.
    pub fn into_legacy(self, operation: &'static str) -> anyhow::Result<PreemptionRecord> {
        match self {
            Self::Legacy(record) => Ok(record),
            Self::Boundaries(_) => {
                Err(detcore_model::schedule::boundary::Error::UnsupportedEdit { operation }.into())
            }
        }
    }

    /// Remove only optional event timestamps, retaining boundary identities,
    /// RIP observations, preemption history and run settings.
    pub fn strip_times(&mut self) {
        match self {
            Self::Legacy(record) => {
                record.strip_event_times();
            }
            Self::Boundaries(record) => record.schedule.strip_times(),
        }
    }

    /// The recorded virtual-time epoch, if the recording states one.
    pub fn epoch(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata().epoch()
    }

    /// The recorded pipe-wake latency, preserving `Some(0)` versus off.
    pub fn pipe_wake_latency(&self) -> Option<u64> {
        self.metadata().pipe_wake_latency()
    }

    /// The recorded signal-copy setting, including the legacy unknown case.
    pub fn signal_state_copy(&self) -> Option<bool> {
        self.metadata().signal_state_copy()
    }

    fn metadata(&self) -> &PreemptionRecord {
        match self {
            Self::Legacy(record) => record,
            Self::Boundaries(record) => &record.preemptions,
        }
    }

    fn metadata_mut(&mut self) -> &mut PreemptionRecord {
        match self {
            Self::Legacy(record) => record,
            Self::Boundaries(record) => &mut record.preemptions,
        }
    }
}

/// Runtime access to recorded priorities/RCB history, retaining its complete
/// backing format. This reader cannot export a flattened legacy recording.
#[derive(Debug)]
pub(crate) struct HistoryReader(Recording);

impl HistoryReader {
    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        Recording::read(path).map(Self)
    }

    /// History-only replay cannot honor boundary records. Admission must not
    /// silently project a new-format recording into legacy priorities/RCBs.
    pub(crate) fn require_legacy_mode(&self) -> anyhow::Result<()> {
        self.0.require_strict_mode(false)
    }

    /// The recording this history reads, for admission.
    pub(crate) fn recording(&self) -> &Recording {
        &self.0
    }

    pub(crate) fn thread_initial_priority(
        &self,
        tid: &crate::types::DetTid,
    ) -> Option<crate::scheduler::runqueue::Priority> {
        self.0
            .metadata()
            .per_thread
            .get(tid)
            .map(|history| history.initial_priority())
    }

    pub(crate) fn extract_thread_record(
        &self,
        tid: &crate::types::DetTid,
    ) -> Option<super::ThreadHistory> {
        self.0.metadata().per_thread.get(tid).cloned()
    }

    /// The validated callback checkpoints of this history, per thread, in
    /// row order (`PreemptionRecord::validate`).
    pub(crate) fn callback_checkpoints(
        &self,
    ) -> &std::collections::BTreeMap<crate::types::DetTid, Vec<super::callback_history::Checkpoint>>
    {
        self.0.metadata().callback_checkpoints()
    }
}

impl std::fmt::Display for HistoryReader {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.encode().map_err(|_| std::fmt::Error)?;
        output.write_str(std::str::from_utf8(&bytes).map_err(|_| std::fmt::Error)?)
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::boundary::Boundary;
    use detcore_model::schedule::boundary::Cause;
    use detcore_model::schedule::boundary::Entry;
    use detcore_model::schedule::boundary::Identity;
    use detcore_model::schedule::boundary::Owner;
    use detcore_model::schedule::boundary::Phase;
    use detcore_model::schedule::boundary::Record;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::preemptions::PreemptionReader;
    use crate::preemptions::PreemptionWriter;
    use crate::preemptions::strip_times_from_events_file;
    use crate::types::DetTid;
    use crate::types::LogicalTime;
    use crate::types::SchedEvent;

    pub(super) fn sample(markers: bool) -> Recording {
        let metadata: PreemptionRecord = serde_json::from_str(
            &PreemptionWriter::new(None)
                .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
                .with_pipe_wake_latency(Some(0))
                .with_signal_state_copy(false)
                .into_string(),
        )
        .unwrap();
        let records = if markers {
            vec![Record::TimesliceEnd(Boundary {
                identity: Identity {
                    owner: Owner {
                        tid: DetTid::from_raw(3),
                        thread_generation: 0,
                        birth_tid: DetTid::from_raw(3),
                        birth_generation: 0,
                        address_space_creator: DetTid::from_raw(3),
                        address_space_generation: 0,
                    },
                    cause: Cause::SoftwareCheckin,
                    phase: Phase::Posthook,
                    entry: Entry::Syscall {
                        syscall: Sysno::write,
                        occurrence: 1,
                    },
                    committed_rcbs: 17,
                    checkin: 1,
                    slice: 1,
                },
                count: 1,
                start_rip: None,
                end_rip: Some(std::num::NonZeroUsize::new(0x1234).unwrap()),
                end_time: Some(LogicalTime::from_nanos(123)),
            })]
        } else {
            vec![]
        };
        Recording::Boundaries(
            BoundaryRecording::new(metadata, Schedule::new(records).unwrap()).unwrap(),
        )
    }

    #[test]
    fn actual_legacy_reader_rejects_even_an_empty_boundary_recording() {
        for markers in [false, true] {
            let recording = sample(markers);
            let bytes = recording.encode().unwrap();
            assert!(serde_json::from_slice::<PreemptionRecord>(&bytes).is_err());
            assert_eq!(Recording::decode(&bytes).unwrap(), recording);
            assert!(recording.into_legacy("minimize").is_err());
        }
    }

    #[test]
    fn file_strip_preserves_protocol_metadata_and_mandatory_identity() {
        let directory = tempfile::tempdir().unwrap();
        for markers in [false, true] {
            let input = directory.path().join(format!("{markers}.json"));
            let original = sample(markers);
            original.write(&input).unwrap();
            let original_bytes = std::fs::read(&input).unwrap();
            let output = strip_times_from_events_file(&input, None).unwrap();
            let actual = Recording::read(&output).unwrap();
            let mut expected = original.clone();
            expected.strip_times();
            assert_eq!(actual, expected);
            assert_eq!(std::fs::read(&input).unwrap(), original_bytes);
            assert_eq!(actual.epoch(), original.epoch());
            assert_eq!(actual.pipe_wake_latency(), Some(0));
            assert_eq!(actual.signal_state_copy(), Some(false));
            assert_eq!(
                super::super::read_recorded_epoch(&output).unwrap(),
                original.epoch()
            );
            assert_eq!(
                super::super::read_recorded_pipe_wake_latency(&output).unwrap(),
                Some(0)
            );
            assert_eq!(
                super::super::read_recorded_signal_state_copy(&output).unwrap(),
                Some(false)
            );
            assert!(PreemptionReader::try_new(&output).is_err());
            assert!(
                serde_json::from_slice::<PreemptionRecord>(&std::fs::read(output).unwrap())
                    .is_err()
            );
        }
    }

    #[test]
    fn unknown_or_broken_envelope_never_falls_back_to_valid_legacy_fields() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
        value["per_thread"] = serde_json::json!({});
        value["global"] = serde_json::json!([]);
        assert!(Recording::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value["slice_boundary_recording"] = serde_json::Value::Null;
        assert!(Recording::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        let body = String::from_utf8(sample(false).encode().unwrap()).unwrap();
        let duplicated = format!(
            "{{{}, {}}}",
            body.trim()[1..body.trim().len() - 1].to_owned(),
            body.trim()[1..body.trim().len() - 1].to_owned()
        );
        assert!(Recording::decode(duplicated.as_bytes()).is_err());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.json");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(super::super::read_recorded_epoch(&path).is_err());
        assert!(super::super::read_recorded_pipe_wake_latency(&path).is_err());
        assert!(super::super::read_recorded_signal_state_copy(&path).is_err());
    }

    #[test]
    fn boundary_history_reader_preserves_the_complete_format_and_original_thread_histories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.json");
        let tid = crate::types::DetTid::from_raw(3);
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_slice_boundaries()
            .unwrap();
        writer.register_thread(tid, crate::scheduler::runqueue::DEFAULT_PRIORITY);
        let expected_history = writer.inner.per_thread.get(&tid).cloned();
        writer.flush().unwrap();
        let before = std::fs::read(&path).unwrap();
        let history = HistoryReader::read(&path).unwrap();
        assert!(history.require_legacy_mode().is_err());
        assert_eq!(
            history.thread_initial_priority(&tid),
            Some(crate::scheduler::runqueue::DEFAULT_PRIORITY)
        );
        assert_eq!(history.extract_thread_record(&tid), expected_history);
        assert_eq!(history.to_string().as_bytes(), before);
        assert!(super::super::PreemptionReader::try_new(&path).is_err());
        let mut bad: serde_json::Value = serde_json::from_slice(&before).unwrap();
        bad["slice_boundary_recording"]["schedule"]["version"] = 999.into();
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(HistoryReader::read(&path).is_err());
        assert_ne!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_second_event_stream_is_not_accepted_as_metadata() {
        let metadata = PreemptionRecord::from_sched_events(
            vec![SchedEvent::branches(DetTid::from_raw(3), 1)],
            None,
        );
        assert!(BoundaryRecording::new(metadata, Schedule::new(vec![]).unwrap()).is_err());
    }

    #[test]
    fn legacy_bytes_and_editor_access_remain_legacy() {
        let legacy = PreemptionRecord::from_sched_events(
            vec![SchedEvent::branches(DetTid::from_raw(3), 2)],
            None,
        )
        .with_pipe_wake_latency(Some(7))
        .with_signal_state_copy(Some(true));
        let mut expected = serde_json::to_vec(&legacy).unwrap();
        expected.push(b'\n');
        let record = Recording::Legacy(legacy.clone());
        assert_eq!(record.encode().unwrap(), expected);
        assert_eq!(
            Recording::decode(&expected)
                .unwrap()
                .into_legacy("split")
                .unwrap(),
            legacy
        );
    }

    #[test]
    fn legacy_metadata_readers_keep_their_individual_field_contracts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.json");
        std::fs::write(&path, r#"{"epoch":null,"pipe_wake_latency":"invalid"}"#).unwrap();
        assert_eq!(super::super::read_recorded_epoch(&path), Ok(None));
        assert!(super::super::read_recorded_pipe_wake_latency(&path).is_err());
        assert_eq!(
            super::super::read_recorded_signal_state_copy(&path),
            Ok(None)
        );
        std::fs::write(&path, r#"{"epoch":null,"epoch":null}"#).unwrap();
        assert!(super::super::read_recorded_epoch(&path).is_err());
    }

    #[test]
    fn writer_keeps_events_markers_and_all_metadata_in_one_stored_stream() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.json");
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
            .with_pipe_wake_latency(Some(0))
            .with_signal_state_copy(false)
            .with_slice_boundaries()
            .unwrap();
        let tid = DetTid::from_raw(3);
        writer.register_thread(tid, crate::scheduler::runqueue::DEFAULT_PRIORITY);
        let metadata = writer.inner.clone();
        let Recording::Boundaries(sample) = sample(true) else {
            unreachable!()
        };
        let Record::TimesliceEnd(boundary) = sample.schedule.records()[0].clone() else {
            unreachable!()
        };
        let first = SchedEvent::branches(tid, 17);
        let last = SchedEvent::syscall(tid, Sysno::write, crate::types::SyscallPhase::Posthook);
        writer.insert_schedevent(first.clone());
        assert_eq!(writer.insert_slice_boundary(boundary.clone()).unwrap(), 1);
        writer.insert_schedevent(last.clone());
        writer.flush().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<PreemptionRecord>(&bytes).is_err());
        let Recording::Boundaries(actual) = Recording::decode(&bytes).unwrap() else {
            panic!("writer flattened the boundary protocol")
        };
        assert_eq!(actual.preemptions, metadata);
        assert_eq!(
            actual.schedule.records(),
            &[
                Record::Event(first),
                Record::TimesliceEnd(boundary),
                Record::Event(last),
            ]
        );
    }

    #[test]
    fn writer_refuses_invalid_order_before_overwriting_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retained.json");
        std::fs::write(&path, b"retained original").unwrap();
        let Recording::Boundaries(sample) = sample(true) else {
            unreachable!()
        };
        let Record::TimesliceEnd(boundary) = sample.schedule.records()[0].clone() else {
            unreachable!()
        };
        let mut writer = PreemptionWriter::new(Some(path.clone()))
            .with_slice_boundaries()
            .unwrap();
        writer.insert_slice_boundary(boundary.clone()).unwrap();
        writer.insert_slice_boundary(boundary.clone()).unwrap();
        assert!(writer.flush().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");

        let mut legacy = PreemptionWriter::new(Some(path.clone()));
        assert!(legacy.insert_slice_boundary(boundary).is_err());
        legacy.insert_schedevent(SchedEvent::branches(DetTid::from_raw(3), 1));
        assert!(legacy.with_slice_boundaries().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");
    }

    #[test]
    fn writer_default_still_emits_exact_legacy_bytes() {
        let mut writer = PreemptionWriter::new(None);
        let event = SchedEvent::branches(DetTid::from_raw(3), 7);
        writer.insert_schedevent(event.clone());
        assert_eq!(
            writer.into_string(),
            PreemptionRecord::from_sched_events(vec![event], None).to_string()
        );
    }

    #[test]
    fn zero_count_or_nonpositive_event_owner_is_rejected_on_new_format_read() {
        for (tid, count) in [(3, 0), (0, 1), (-1, 1)] {
            let event = SchedEvent::branches(DetTid::from_raw(tid), count);
            assert!(Schedule::new(vec![Record::Event(event.clone())]).is_err());
            let mut wire: serde_json::Value =
                serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
            let schedule = &mut wire["slice_boundary_recording"]["schedule"];
            schedule["record_count"] = 1.into();
            schedule["records"] = serde_json::json!([Record::Event(event)]);
            assert!(Recording::decode(&serde_json::to_vec(&wire).unwrap()).is_err());
        }
    }
}

#[cfg(test)]
mod explicit_mode_tests {
    use super::*;

    #[test]
    fn explicit_mode_admission_never_falls_back_or_changes_legacy_policy() {
        let legacy = Recording::Legacy(PreemptionRecord::default());
        let typed = Recording::Boundaries(
            BoundaryRecording::new(PreemptionRecord::default(), Schedule::new(vec![]).unwrap())
                .unwrap(),
        );
        assert!(legacy.require_strict_mode(false).is_ok());
        assert!(typed.require_strict_mode(true).is_ok());
        assert!(legacy.require_strict_mode(true).is_err());
        assert!(typed.require_strict_mode(false).is_err());
    }
    #[test]
    fn ordinary_history_admits_legacy_and_retains_read_error_panics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        std::fs::write(
            &path,
            Recording::Legacy(PreemptionRecord::default())
                .encode()
                .unwrap(),
        )
        .unwrap();
        assert!(
            HistoryReader::read(&path)
                .unwrap()
                .require_legacy_mode()
                .is_ok()
        );
        for contents in [None, Some("not a JSON recording")] {
            match contents {
                None => std::fs::remove_file(&path).unwrap(),
                Some(value) => std::fs::write(&path, value).unwrap(),
            }
            let cfg = crate::Config {
                replay_preemptions_from: Some(path.clone()),
                ..crate::Config::default()
            };
            assert!(std::panic::catch_unwind(|| crate::GlobalState::for_tests(&cfg)).is_err());
        }
    }
}

#[cfg(test)]
mod identity_tests {
    use super::tests::sample;
    use super::*;
    use crate::types::DetTid;
    use crate::types::SchedEvent;

    fn known() -> HistoryIdentity {
        HistoryIdentity::Known(reverie::pmu_config::RcbMetric::ConditionalBranchesRetiredUserV1)
    }

    /// The legacy reader of the base build: `PreemptionRecord`'s own serde
    /// shape, and the boundary dispatch that falls back to it.
    fn base_build_decode(bytes: &[u8]) -> anyhow::Result<()> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        if value.get("slice_boundary_recording").is_some() {
            serde_json::from_slice::<Envelope>(bytes)?;
        } else {
            serde_json::from_slice::<PreemptionRecord>(bytes)?;
        }
        Ok(())
    }

    #[test]
    fn an_identity_recording_round_trips_with_its_identity() {
        for identity in [known(), HistoryIdentity::Unqualified("claim-unset".into())] {
            let legacy = PreemptionRecord::from_sched_events(
                vec![SchedEvent::branches(DetTid::from_raw(3), 7)],
                Some(identity.clone()),
            )
            .with_pipe_wake_latency(Some(0));
            let encoded = legacy.to_string();
            assert!(
                encoded.starts_with("{\"rcb_history\":{\"format\":2,"),
                "{encoded}"
            );
            let decoded = Recording::decode(encoded.as_bytes()).unwrap();
            assert_eq!(decoded.identity(), Some(&identity));
            assert_eq!(decoded, Recording::Legacy(legacy.clone()));
            assert_eq!(
                decoded.encode().unwrap(),
                format!("{encoded}\n").into_bytes()
            );

            let Recording::Boundaries(mut boundaries) = sample(true) else {
                unreachable!()
            };
            boundaries.preemptions.identity = Some(identity.clone());
            let boundaries = Recording::Boundaries(boundaries);
            let bytes = boundaries.encode().unwrap();
            let again = Recording::decode(&bytes).unwrap();
            assert_eq!(again.identity(), Some(&identity));
            assert_eq!(again, boundaries);
            assert!(again.require_strict_mode(true).is_ok());
        }
    }

    #[test]
    fn the_base_builds_reader_refuses_every_identity_recording() {
        let empty = PreemptionRecord::from_sched_events(vec![], Some(known()));
        let Recording::Boundaries(mut boundaries) = sample(false) else {
            unreachable!()
        };
        boundaries.preemptions.identity = Some(known());
        for bytes in [
            empty.to_string().into_bytes(),
            Recording::Boundaries(boundaries).encode().unwrap(),
        ] {
            assert!(
                base_build_decode(&bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            assert!(Recording::decode(&bytes).is_ok());
        }
    }

    #[test]
    fn a_legacy_file_states_no_identity() {
        for recording in [
            Recording::Legacy(PreemptionRecord::from_sched_events(vec![], None)),
            sample(true),
        ] {
            let decoded = Recording::decode(&recording.encode().unwrap()).unwrap();
            assert_eq!(decoded.identity(), None);
        }
    }

    #[test]
    fn a_malformed_identity_recording_never_falls_back_to_legacy() {
        let good: serde_json::Value = serde_json::from_str(
            &PreemptionRecord::from_sched_events(vec![], Some(known())).to_string(),
        )
        .unwrap();
        let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut value = good.clone();
            f(&mut value);
            serde_json::to_vec(&value).unwrap()
        };
        for bytes in [
            mutate(&|v| v["rcb_history"]["format"] = 3.into()),
            mutate(&|v| v["rcb_history"]["identity"] = "rcb:cond-retired:user:v2".into()),
            mutate(&|v| v["rcb_history"]["identity"] = serde_json::json!({"unqualified": 1})),
            mutate(&|v| v["rcb_history"]["extra"] = 1.into()),
            mutate(&|v| v["per_thread"] = serde_json::json!({})),
            mutate(&|v| {
                let inner = v["rcb_history"].clone();
                v["rcb_history"]["recording"] = serde_json::json!({ "rcb_history": inner });
            }),
            mutate(&|v| v["rcb_history"]["recording"] = serde_json::json!({"global": []})),
        ] {
            assert!(
                Recording::decode(&bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
        }
        let text = PreemptionRecord::from_sched_events(vec![], Some(known())).to_string();
        let duplicated = text.replacen("\"format\":2,", "\"format\":2,\"format\":2,", 1);
        assert_ne!(duplicated, text);
        assert!(Recording::decode(duplicated.as_bytes()).is_err());
    }

    #[test]
    fn transformations_keep_the_identity() {
        let record = PreemptionRecord::from_sched_events(
            vec![SchedEvent::branches(DetTid::from_raw(3), 7)],
            Some(known()),
        );
        assert_eq!(record.clone_preemptions_only().identity(), Some(&known()));
        assert_eq!(record.normalize().identity(), Some(&known()));
        assert_eq!(
            record.with_latest_preempt_removed().identity(),
            Some(&known())
        );
        let mut recording = Recording::Legacy(record);
        recording.strip_times();
        let reread = Recording::decode(&recording.encode().unwrap()).unwrap();
        assert_eq!(reread.identity(), Some(&known()));
        assert_eq!(reread, recording);
    }

    /// A record with two priority rows of tid 7, the second ended by a write
    /// prehook checkpoint at the retained SPR record's coordinates, and
    /// RCB-bearing events, as a writer emits it with or without `identity`.
    fn with_rows_and_checkpoint(identity: Option<HistoryIdentity>) -> PreemptionRecord {
        use detcore_model::schedule::Op;
        use reverie::syscalls::Sysno;

        use crate::DEFAULT_PRIORITY;
        use crate::preemptions::PreemptionWriter;
        use crate::preemptions::callback_history::Origin;
        use crate::preemptions::callback_history::Site;
        use crate::slice_boundary::callbacks::Callback;
        use crate::types::LogicalTime;

        let tid = DetTid::from_raw(7);
        let mut writer = PreemptionWriter::new(None);
        if let Some(identity) = identity {
            writer = writer.with_identity(identity);
        }
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_schedevent(SchedEvent::branches(tid, 2_459_579));
        let first = LogicalTime::from_nanos(1_792_852_217_000_000_000);
        writer.insert_reprioritization(tid, first, 2_459_579, DEFAULT_PRIORITY, DEFAULT_PRIORITY);
        writer.insert_schedevent(SchedEvent::branches(tid, 8_299_750));
        // Progress counts events, so the write prehook's endpoint is 154
        // past the raw RCB count.
        writer.insert_schedevent(SchedEvent {
            op: Op::OtherInstructions,
            count: 154,
            ..SchedEvent::branches(tid, 0)
        });
        let second = LogicalTime::from_nanos(1_792_852_217_684_087_955);
        writer.insert_reprioritization(tid, second, 10_759_329, DEFAULT_PRIORITY, DEFAULT_PRIORITY);
        writer.insert_callback_checkpoint(
            tid,
            Origin {
                callback: Callback::Syscall(Sysno::write),
                site: Site::Prehook,
                progress: 10_759_483,
                last_op: Some(Op::OtherInstructions),
                time: second,
                rcbs: 10_759_329,
            },
        );
        Recording::decode(writer.into_string().as_bytes())
            .unwrap()
            .into_legacy("test")
            .unwrap()
    }

    /// Every transformation carries the identity and otherwise produces
    /// exactly what it produces from the same record without one: every
    /// per-thread row, callback checkpoint, event and RCB is unchanged.
    #[test]
    fn transformations_change_nothing_but_carry_the_identity() {
        let with = with_rows_and_checkpoint(Some(known()));
        let without = with_rows_and_checkpoint(None);
        let tid = DetTid::from_raw(7);
        assert_eq!(with.per_thread[&tid].prio_changes.len(), 2);
        assert_eq!(with.callback_checkpoints[&tid].len(), 1);
        assert_eq!(with.global.len(), 3);
        assert_eq!(with.identity(), Some(&known()));
        assert_eq!(with.clone().with_identity(None), without);
        type Transformation = fn(&PreemptionRecord) -> PreemptionRecord;
        let transformations: [(&str, Transformation); 3] = [
            (
                "clone_preemptions_only",
                PreemptionRecord::clone_preemptions_only,
            ),
            ("normalize", PreemptionRecord::normalize),
            (
                "with_latest_preempt_removed",
                PreemptionRecord::with_latest_preempt_removed,
            ),
        ];
        for (name, transform) in transformations {
            let carried = transform(&with);
            assert_eq!(carried.identity(), Some(&known()), "{name}");
            assert_eq!(carried.with_identity(None), transform(&without), "{name}");
        }
        let reloaded = |record: &PreemptionRecord| {
            let mut recording = Recording::Legacy(record.clone());
            recording.strip_times();
            Recording::decode(&recording.encode().unwrap()).unwrap()
        };
        let carried = reloaded(&with);
        assert_eq!(carried.identity(), Some(&known()));
        assert_eq!(carried.with_identity(None), reloaded(&without));
        let Recording::Boundaries(sampled) = sample(true) else {
            panic!("not a boundary recording")
        };
        let wrapped = |record: &PreemptionRecord| {
            let recording = Recording::Boundaries(
                BoundaryRecording::new(record.clone_preemptions_only(), sampled.schedule().clone())
                    .unwrap(),
            );
            Recording::decode(&recording.encode().unwrap()).unwrap()
        };
        let carried = wrapped(&with);
        assert_eq!(carried.identity(), Some(&known()));
        assert_eq!(carried.with_identity(None), wrapped(&without));
    }
}
