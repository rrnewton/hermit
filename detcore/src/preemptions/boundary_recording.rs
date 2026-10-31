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

use super::PreemptionRecord;

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

/// Whether a record is in the slice-boundary format. The probe builds none of
/// the record: it skips every value, so reading the format of a schedule of
/// millions of events does not cost a JSON tree of it, several times the
/// size of its text.
#[derive(Deserialize)]
struct FormatProbe {
    // A present null must not look like an absent protocol marker.
    #[serde(default, deserialize_with = "marker_present")]
    slice_boundary_recording: bool,
    #[serde(default, deserialize_with = "marker_present")]
    incomplete_preemption_record: bool,
}

fn marker_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
}

impl FormatProbe {
    fn is_boundary_recording(bytes: &[u8]) -> serde_json::Result<bool> {
        serde_json::from_slice::<Self>(bytes).map(|probe| probe.slice_boundary_recording)
    }
}

/// A `format_version` as [`probe_format`] reads it: a format number, or
/// `None` for anything else, which the record's ordinary decoding reports.
struct StatedFormat(Option<u32>);

impl<'de> Deserialize<'de> for StatedFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self(
            value.as_u64().and_then(|format| u32::try_from(format).ok()),
        ))
    }
}

/// The envelope field, present (even as null) or absent, so that a null
/// envelope is not taken for a legacy record.
fn present_envelope<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The format a preemption record states (`PreemptionRecord::format_version`,
/// 1 when absent), read from its bytes without decoding the record: at the
/// top level of a legacy record, and only in the preemptions of a
/// slice-boundary one (a field beside the envelope is the envelope decoder's
/// to refuse). A reader checks it first, so that a record of a format it does
/// not read is refused as such ([`super::RecordFormatRefusal`]) before anything
/// that format could change is parsed or applied. The probe is typed, so a
/// duplicated `format_version` or `preemptions` is an error here, as in the
/// strict decoders, never a format; every other field is skipped unread. A
/// format that is not a number, or a null envelope, reads as 1, and an
/// envelope that is not an object is an error, each left to the record's
/// ordinary decoding to report. A record marked incomplete
/// (`PreemptionRecord::replay_departed_at`) is written inside an
/// `incomplete_preemption_record` envelope, as a legacy record or as a
/// slice-boundary recording's preemptions, and states its format only in that
/// envelope's record.
pub(super) fn probe_format(bytes: &[u8]) -> Result<u32, serde_json::Error> {
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        format_version: Option<StatedFormat>,
    }
    #[derive(Deserialize)]
    struct Marked {
        #[serde(default)]
        record: Option<Payload>,
    }
    #[derive(Deserialize)]
    struct Preemptions {
        #[serde(default)]
        format_version: Option<StatedFormat>,
        #[serde(default, deserialize_with = "present_envelope")]
        incomplete_preemption_record: Option<Option<Marked>>,
    }
    impl Preemptions {
        // A field beside the incomplete-record envelope is the decoder's to
        // refuse, as one beside the slice-boundary envelope is.
        fn stated(self) -> Option<StatedFormat> {
            match self.incomplete_preemption_record {
                Some(marked) => marked
                    .and_then(|marked| marked.record)
                    .and_then(|record| record.format_version),
                None => self.format_version,
            }
        }
    }
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        preemptions: Option<Preemptions>,
    }
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        format_version: Option<StatedFormat>,
        #[serde(default, deserialize_with = "present_envelope")]
        incomplete_preemption_record: Option<Option<Marked>>,
        #[serde(default, deserialize_with = "present_envelope")]
        slice_boundary_recording: Option<Option<Envelope>>,
    }
    let probe: Probe = serde_json::from_slice(bytes)?;
    let stated = match probe.slice_boundary_recording {
        Some(envelope) => envelope
            .and_then(|envelope| envelope.preemptions)
            .and_then(Preemptions::stated),
        None => Preemptions {
            format_version: probe.format_version,
            incomplete_preemption_record: probe.incomplete_preemption_record,
        }
        .stated(),
    };
    Ok(stated.and_then(|format| format.0).unwrap_or(1))
}

pub(super) fn read_settings<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read preemption record {}: {}", path.display(), e))?;
    let format: FormatProbe = serde_json::from_slice(&bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))?;
    if !format.slice_boundary_recording && !format.incomplete_preemption_record {
        // Each old metadata reader still deserializes only its own field,
        // from the original bytes, preserving its legacy acceptance contract.
        return serde_json::from_slice(&bytes)
            .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e));
    }
    let recording = Recording::decode(&bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))?;
    serde_json::from_value(
        super::record_serde::settings_value(recording.metadata()).map_err(|e| e.to_string())?,
    )
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
        // --chaos-instruction-offset refuses --strict-schedule-boundaries, so
        // no slice-boundary recording holds an offset decision, in a thread's
        // rows or in its events.
        if let Some((tid, _)) = preemptions
            .per_thread
            .iter()
            .find(|(_, history)| history.has_instruction_offsets())
        {
            bail!(
                "slice-boundary recording thread {tid}'s rows carry instruction offsets, which a \
                 slice-boundary recording cannot hold; the record is malformed"
            );
        }
        if let Some(index) = schedule.records().iter().position(|record| {
            matches!(
                record,
                detcore_model::schedule::boundary::Record::Event(event)
                    if event.instruction_offset.is_some()
            )
        }) {
            bail!(
                "slice-boundary recording record {index} carries an instruction offset, which a \
                 slice-boundary recording cannot hold; the record is malformed"
            );
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
        wire.preemptions.check_format()?;
        Self::new(wire.preemptions, wire.schedule)
    }
}

/// Exit with the policy-refusal status when `result` failed because the
/// record is of a format this Hermit does not read
/// ([`super::RecordFormatRefusal`]); pass any other result through, so the
/// caller keeps its own handling of unreadable or malformed files.
pub(crate) fn exit_on_record_format_refusal<T>(result: anyhow::Result<T>) -> anyhow::Result<T> {
    if let Err(error) = &result
        && error
            .chain()
            .any(|cause| cause.downcast_ref::<super::RecordFormatRefusal>().is_some())
    {
        crate::scheduler::exit_on_scheduler_refusal(&format!("{error:#}"));
    }
    result
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
    /// Refuse a known omitted change point in either stored format.
    pub fn require_faithful_replay(&self) -> anyhow::Result<()> {
        self.metadata().require_faithful_replay()
    }

    /// A retained source observation of an omitted replay change point.
    pub fn replay_departed_at(&self) -> Option<u64> {
        self.metadata().replay_departed_at()
    }

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
        // A format this Hermit does not read is refused before anything else
        // is parsed, kept typed so that a replay exits with the policy status
        // (`exit_on_record_format_refusal`), for both forms.
        // A record the probe cannot read (malformed JSON, a duplicated field,
        // an envelope that is not an object) is left to the decoding below,
        // which reports it.
        if let Ok(format) = probe_format(bytes) {
            super::check_record_format(format)?;
        }
        // Inspect only to dispatch. Deserialize the ORIGINAL bytes again so
        // duplicate fields in the envelope are rejected. A malformed new
        // recording never falls back to legacy.
        if FormatProbe::is_boundary_recording(bytes)? {
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
        Self::decode(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)
            .with_context(|| format!("reading schedule {}", path.display()))
    }

    /// Serialize the same format with the conventional terminal newline.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = match self {
            Self::Legacy(record) => serde_json::to_vec(record)?,
            Self::Boundaries(record) => serde_json::to_vec(&Envelope {
                slice_boundary_recording: record.clone(),
            })?,
        };
        bytes.push(b'\n');
        Ok(bytes)
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

    /// The host files the recorded run's guest opened, if the recording says.
    pub fn host_inputs(&self) -> Option<&detcore_model::host_input::HostInputSet> {
        self.metadata().host_inputs()
    }

    fn metadata(&self) -> &PreemptionRecord {
        match self {
            Self::Legacy(record) => record,
            Self::Boundaries(record) => &record.preemptions,
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

    /// The host files the recorded run's guest opened, if the recording says.
    pub(crate) fn host_inputs(&self) -> Option<&detcore_model::host_input::HostInputSet> {
        self.0.host_inputs()
    }

    /// History-only replay cannot honor boundary records. Admission must not
    /// silently project a new-format recording into legacy priorities/RCBs.
    pub(crate) fn require_legacy_mode(&self) -> anyhow::Result<()> {
        self.0.require_strict_mode(false)
    }

    /// Only explicit edited history exploration may consume known omissions.
    pub(crate) fn require_replay_mode(&self, edited: bool) -> anyhow::Result<()> {
        self.require_legacy_mode()?;
        if !edited {
            self.0.require_faithful_replay()?;
        }
        Ok(())
    }

    pub(crate) fn replay_departed_at(&self) -> Option<u64> {
        self.0.replay_departed_at()
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

    /// The store-exclusives that failed in the recorded run.
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn store_exclusive_failures(&self) -> u64 {
        self.0.metadata().store_exclusive_failures()
    }
}

impl std::fmt::Display for HistoryReader {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.encode().map_err(|_| std::fmt::Error)?;
        output.write_str(std::str::from_utf8(&bytes).map_err(|_| std::fmt::Error)?)
    }
}

#[cfg(test)]
pub(super) mod tests {
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

    pub(crate) fn sample(markers: bool) -> Recording {
        let metadata: PreemptionRecord = serde_json::from_str(
            &PreemptionWriter::new(None)
                .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
                .with_pipe_wake_latency(Some(0))
                .with_signal_state_copy(false)
                .with_expired_wait_runs_charged_once(true)
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

    /// Strict slice boundaries refuse --chaos-instruction-offset, so a
    /// slice-boundary recording whose events hold an offset is malformed:
    /// refused when built and when read, as an error rather than a format
    /// refusal. The same recording without the offset is accepted.
    #[test]
    fn a_boundary_recording_holding_an_instruction_offset_is_malformed() {
        let metadata = || -> PreemptionRecord {
            serde_json::from_str(
                &PreemptionWriter::new(None)
                    .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
                    .with_pipe_wake_latency(Some(0))
                    .with_signal_state_copy(false)
                    .into_string(),
            )
            .unwrap()
        };
        let offset = crate::resources::InstructionOffset {
            rcb_target: 40,
            instructions: 5,
        };
        let plain = SchedEvent::branches(DetTid::from_raw(3), 3);
        let with = plain.clone().with_instruction_offset(Some(offset));
        let error = BoundaryRecording::new(
            metadata(),
            Schedule::new(vec![Record::Event(with.clone())]).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");

        let text = String::from_utf8(
            Recording::Boundaries(
                BoundaryRecording::new(
                    metadata(),
                    Schedule::new(vec![Record::Event(plain.clone())]).unwrap(),
                )
                .unwrap(),
            )
            .encode()
            .unwrap(),
        )
        .unwrap();
        Recording::decode(text.as_bytes()).unwrap();
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(text.contains(&plain_json), "{text}");
        let spliced = text.replacen(&plain_json, &serde_json::to_string(&with).unwrap(), 1);
        let error = Recording::decode(spliced.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        assert!(
            !error
                .chain()
                .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>()),
            "{error:#}"
        );

        // An offset in a thread's rows (a slice whose timer stopped it at a
        // syscall or trap leaves no offset on an event): refused when built
        // and when read, and the boundary writer refuses to record one.
        let tid = DetTid::from_raw(3);
        let priority = crate::scheduler::runqueue::DEFAULT_PRIORITY;
        let at = LogicalTime::from_nanos;
        let mut writer = PreemptionWriter::new(None)
            .with_epoch("2001-02-03T04:05:06Z".parse().unwrap())
            .with_pipe_wake_latency(Some(0))
            .with_signal_state_copy(false);
        writer.register_thread(tid, priority);
        writer.insert_reprioritization(tid, at(100), 41, priority, 5, Some(offset));
        let rows: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        let error =
            BoundaryRecording::new(rows.clone(), Schedule::new(vec![]).unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        let mut envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        envelope["slice_boundary_recording"]["preemptions"] = serde_json::to_value(&rows).unwrap();
        let error = Recording::decode(envelope.to_string().as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        assert!(
            !error
                .chain()
                .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>()),
            "{error:#}"
        );
        let boundary_writer = std::panic::catch_unwind(|| {
            let mut writer = PreemptionWriter::new(None).with_slice_boundaries().unwrap();
            writer.register_thread(tid, priority);
            writer.insert_reprioritization(tid, at(100), 41, priority, 5, Some(offset));
        });
        assert!(boundary_writer.is_err());
    }

    /// A slice-boundary recording states its format inside its envelope
    /// alone: a `format_version` beside the envelope is the envelope
    /// decoder's to refuse, not a format refusal, and a null or malformed
    /// envelope reaches ordinary decoding. A legacy record states it at the
    /// top level.
    #[test]
    fn an_envelope_states_its_format_only_inside_it() {
        let refused = |bytes: &[u8]| {
            Recording::decode(bytes).err().is_some_and(|error| {
                error
                    .chain()
                    .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>())
            })
        };
        let envelope: serde_json::Value =
            serde_json::from_slice(&sample(false).encode().unwrap()).unwrap();
        // Beside the envelope: read as 1, refused by the envelope decoder.
        let mut beside = envelope.clone();
        beside["format_version"] = serde_json::json!(99);
        let beside = beside.to_string();
        assert_eq!(probe_format(beside.as_bytes()).unwrap(), 1);
        let error = Recording::decode(beside.as_bytes()).unwrap_err();
        assert!(!refused(beside.as_bytes()), "{error:#}");
        // Inside it: refused by format.
        let mut inside = envelope.clone();
        inside["slice_boundary_recording"]["preemptions"]["format_version"] = serde_json::json!(99);
        let inside = inside.to_string();
        assert_eq!(probe_format(inside.as_bytes()).unwrap(), 99);
        assert!(refused(inside.as_bytes()));
        // A null or malformed envelope: ordinary decoding reports it, never
        // a format refusal (the probe reads 1, or cannot read it at all).
        for broken in [
            serde_json::json!({"slice_boundary_recording": null, "format_version": 99}),
            serde_json::json!({"slice_boundary_recording": 7, "format_version": 99}),
            serde_json::json!({"slice_boundary_recording": {"preemptions": {"format_version": "x"}}}),
        ] {
            let broken = broken.to_string();
            assert_eq!(probe_format(broken.as_bytes()).unwrap_or(1), 1, "{broken}");
            assert!(Recording::decode(broken.as_bytes()).is_err(), "{broken}");
            assert!(!refused(broken.as_bytes()), "{broken}");
        }
        // A duplicated `format_version` (top level or in the envelope's
        // preemptions) or `preemptions`, in either order: an error of the
        // strict decoders, never a format refusal.
        let preemptions = |format: u32| {
            format!(r#""preemptions":{{"per_thread":{{}},"global":[],"format_version":{format}}}"#)
        };
        let mut duplicated = Vec::new();
        for (first, second) in [(99, 1), (1, 99)] {
            let (a, b) = (
                format!(r#""format_version":{first}"#),
                format!(r#""format_version":{second}"#),
            );
            duplicated.push(format!(r#"{{"per_thread":{{}},"global":[],{a},{b}}}"#));
            duplicated.push(format!(
                r#"{{"slice_boundary_recording":{{"preemptions":{{"per_thread":{{}},"global":[],{a},{b}}},"schedule":[]}}}}"#
            ));
            duplicated.push(format!(
                r#"{{"slice_boundary_recording":{{{},{},"schedule":[]}}}}"#,
                preemptions(first),
                preemptions(second)
            ));
        }
        for record in duplicated {
            assert!(probe_format(record.as_bytes()).is_err(), "{record}");
            assert!(Recording::decode(record.as_bytes()).is_err(), "{record}");
            assert!(!refused(record.as_bytes()), "{record}");
        }
        // A legacy record: the top level.
        let legacy = serde_json::json!({"per_thread": {}, "global": [], "format_version": 99});
        assert_eq!(probe_format(legacy.to_string().as_bytes()).unwrap(), 99);
        assert!(refused(legacy.to_string().as_bytes()));
    }

    /// A record marked incomplete states its format inside main's
    /// `incomplete_preemption_record` envelope, as a legacy record and as a
    /// slice-boundary recording's preemptions. The probe reads it there, so a
    /// newer format is refused as such, typed, before anything else is
    /// decoded; a format this Hermit reads decodes as before.
    #[test]
    fn a_marked_record_states_its_format_inside_its_envelope() {
        let refused = |bytes: &[u8]| {
            Recording::decode(bytes).err().is_some_and(|error| {
                error
                    .chain()
                    .any(|cause| cause.is::<crate::preemptions::RecordFormatRefusal>())
            })
        };
        let marked = |format: u32| {
            serde_json::json!({"incomplete_preemption_record": {
                "version": 1,
                "replay_departed_at": 7,
                "record": {"per_thread": {}, "global": [], "format_version": format},
            }})
        };
        let legacy = marked(99).to_string();
        assert_eq!(probe_format(legacy.as_bytes()).unwrap(), 99);
        assert!(refused(legacy.as_bytes()));
        let boundary = serde_json::json!({"slice_boundary_recording": {
            "preemptions": marked(99),
            "schedule": [],
        }})
        .to_string();
        assert_eq!(probe_format(boundary.as_bytes()).unwrap(), 99);
        assert!(refused(boundary.as_bytes()));
        let current = marked(1).to_string();
        assert_eq!(probe_format(current.as_bytes()).unwrap(), 1);
        let Recording::Legacy(record) = Recording::decode(current.as_bytes()).unwrap() else {
            panic!("a marked legacy record decodes as one");
        };
        assert_eq!(record.replay_departed_at(), Some(7));
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
            assert_eq!(
                super::super::read_recorded_expired_wait_runs_charged_once(&output),
                Ok(true)
            );
            assert!(PreemptionReader::try_new(&output).is_err());
            assert!(
                serde_json::from_slice::<PreemptionRecord>(&std::fs::read(output).unwrap())
                    .is_err()
            );
        }
    }

    /// The format is the presence of the top-level marker, null included,
    /// read without building any value.
    #[test]
    fn the_format_is_the_presence_of_the_top_level_marker() {
        let probe = |text: &str| FormatProbe::is_boundary_recording(text.as_bytes()).unwrap();
        assert!(!probe(r#"{"per_thread": {}, "global": []}"#));
        assert!(probe(r#"{"slice_boundary_recording": {"preemptions": 1}}"#));
        assert!(probe(r#"{"slice_boundary_recording": null}"#));
        assert!(!probe(
            r#"{"per_thread": {"3": {"slice_boundary_recording": 1}}, "global": []}"#
        ));
        assert!(
            FormatProbe::is_boundary_recording(
                br#"{"slice_boundary_recording": null, "slice_boundary_recording": null}"#
            )
            .is_err()
        );
        assert!(FormatProbe::is_boundary_recording(b"{\"global\": [").is_err());
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
        let metadata =
            PreemptionRecord::from_sched_events(vec![SchedEvent::branches(DetTid::from_raw(3), 1)]);
        assert!(BoundaryRecording::new(metadata, Schedule::new(vec![]).unwrap()).is_err());
    }

    #[test]
    fn legacy_bytes_and_editor_access_remain_legacy() {
        let legacy =
            PreemptionRecord::from_sched_events(vec![SchedEvent::branches(DetTid::from_raw(3), 2)])
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
            PreemptionRecord::from_sched_events(vec![event]).to_string()
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
