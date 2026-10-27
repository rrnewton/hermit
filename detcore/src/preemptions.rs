/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A datatype to abstract a record of thread preemptions, as generated during chaos mode execution.

pub mod boundary_recording;
pub mod callback_history;
mod record_serde;

use std::collections::BTreeMap;
use std::fs::File;
use std::iter::FromIterator;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use detcore_model::collections::ReplayCursor;
use detcore_model::host_input::HostInputSet;
use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use crate::resources::ChaosEpochTransition;
pub use crate::resources::InstructionOffset;
use crate::scheduler::Priority;
use crate::scheduler::runqueue::DEFAULT_PRIORITY;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::runqueue::is_ordinary_priority;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::SchedEvent;

/// The newest preemption-record format this Hermit writes and reads
/// (`PreemptionRecord::format_version`). One strictly monotone counter serves
/// every change that adds to the record: format N means the record holds
/// every feature numbered N or lower. Format 1 has no version field: every
/// record written before the field existed, and every record since that holds
/// nothing a format-1 reader would misread. Format 2 adds the instruction
/// offsets of the schedule trace's events ([`INSTRUCTION_OFFSET_FORMAT`]).
/// A reader reads every format up to its own, and refuses a newer one
/// ([`RecordFormatRefusal`]). (Provisional: renumbered to the next free
/// number if another format-bumping change lands first.)
pub const PREEMPTION_RECORD_FORMAT: u32 = 2;

/// The format of a record written by a Hermit that records
/// `--chaos-instruction-offset` decisions in its schedule trace
/// (`SchedEvent::instruction_offset`) as well as in its rows
/// (`ThreadHistory::preemption_instruction_offsets`). A record without any
/// is written as before, with no version.
pub const INSTRUCTION_OFFSET_FORMAT: u32 = 2;

/// A preemption record of a format newer than this Hermit reads
/// ([`PREEMPTION_RECORD_FORMAT`]): a policy refusal, not a malformed file, so
/// a replay of one exits with the policy-refusal status rather than a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordFormatRefusal {
    /// The format the record states.
    pub format: u32,
}

impl std::fmt::Display for RecordFormatRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the preemption record is format {}, written by a newer Hermit; this Hermit reads \
             formats up to {PREEMPTION_RECORD_FORMAT}. Replay it with the Hermit that recorded \
             it, or a newer one",
            self.format
        )
    }
}

impl std::error::Error for RecordFormatRefusal {}

/// Refuse a record of format `format` if this Hermit does not read it. A
/// record stating format 0 is malformed, not refused
/// (`PreemptionRecord::validate`).
pub fn check_record_format(format: u32) -> Result<(), RecordFormatRefusal> {
    if format > PREEMPTION_RECORD_FORMAT {
        Err(RecordFormatRefusal { format })
    } else {
        Ok(())
    }
}

/// A record of all the preemptions and other scheduling events that occur during execution.
#[derive(PartialEq, Default, Debug, Eq, Clone, Hash)]
pub struct PreemptionRecord {
    // TODO(T106838933): switch the keys from DetTid to Pedigree.
    /// A sorted list of end-of-timeslice preemption times for each thread.
    per_thread: BTreeMap<DetTid, ThreadHistory>,
    global: Vec<SchedEvent>,
    /// The virtual-time epoch of the run that produced this record.
    ///
    /// Every `LogicalTime` above is an absolute virtual instant, i.e. an offset
    /// from this epoch, so the record only describes a run whose clock starts
    /// here. A replay started from any other epoch would see every recorded
    /// timeslice end shifted by the difference. `None` for records written
    /// before the epoch was stored, and for records synthesized in memory.
    epoch: Option<DateTime<Utc>>,
    /// The `--pipe-wake-latency` of the run that produced this record
    /// (<https://github.com/rrnewton/hermit/issues/4380>). A hand-off moves
    /// turns and their charged time, so a replay under another value cannot
    /// reproduce the run, and is refused. `None` when the option was off,
    /// which writes the record exactly as before the field existed, and for
    /// records written before it. `Some(0)` is distinct from `None`.
    pipe_wake_latency: Option<u64>,
    /// Whether Detcore's signal-state copy was on in the run that produced
    /// this record (`Config::signal_state_copy`, A5 step 2d). With it on,
    /// some signal decisions read the copy, and ptrace reports each handler
    /// return, which can move a preemption in the cases docs/SIGNAL_STATE_CHECK.md
    /// lists, so a replay under the other setting is refused. `None` for
    /// records written before the field existed, and for records synthesized
    /// in memory, which are replayed under either setting.
    signal_state_copy: Option<bool>,
    /// Whether the run that produced this record charged preemptions at most
    /// the time their slice ran (`--cap-preemption-charge`). Turn charges
    /// move timers and timeouts, so a replay under the other setting cannot
    /// reproduce the run, and is refused. Off writes the record exactly as
    /// before the field existed.
    cap_preemption_charge: bool,
    /// The `--max-timeslice` the run that produced this record drew under
    /// `--chaos-timeslice-draw=run` (`Config::chaos_timeslice`), in virtual
    /// nanoseconds. A replay of the record runs under it (`hermit run` takes
    /// it unless the replay gives another `--max-timeslice`, which is
    /// refused). `None`, which writes nothing, for a run that drew none and
    /// for records written before the field existed.
    chaos_timeslice: Option<u64>,
    /// How the run that produced this record numbered its guest's process
    /// and thread IDs (`Config::guest_ids`), when no PID namespace of its own
    /// did (`--no-namespace`). Every thread above is keyed by those IDs, so a
    /// replay under another numbering is refused. `None` for a run its PID
    /// namespace numbered, which writes the record exactly as before the
    /// field existed, and for records written before it.
    guest_ids: Option<detcore_model::pid::GuestIds>,
    /// Set in a record written by a preemption replay that left a change point
    /// (and its slowdown-epoch transitions) out of it because it would not
    /// have come after the thread's last one: the global event count where it
    /// first did. A record's rows must increase (`Self::validate`), so such a
    /// change point cannot be written, and the record no longer holds the
    /// priority the thread had at that instant: it is not an exact record of
    /// the run, and faithful replay refuses it. That happens to a replay that left the
    /// record it replays, and to a replay of an edited record
    /// (`--replay-edited-preemptions`), which keeps a slice that ends at once.
    /// Absence preserves legacy admission; it does not prove historical
    /// completeness. Newly marked JSON uses a mandatory envelope that old
    /// readers reject, and marked non-human serialization is refused.
    replay_departed_at: Option<u64>,
    /// Actual callback expirations associated with existing priority-history rows.
    /// Absent in old/edited recordings; never inferred from time or branch counts.
    callback_checkpoints: BTreeMap<DetTid, Vec<callback_history::Checkpoint>>,
    /// The `--rng-seed` of the run that produced this record.
    ///
    /// It decides what the guest reads as randomness, so the recorded schedule
    /// only describes a guest that read the same values: `--seed` sets it too, so
    /// a replay at another `--seed` would otherwise hand the guest other values.
    /// `None` for records written before the field existed, and for records
    /// synthesized in memory.
    rng_seed: Option<u64>,
    /// Whether the run that produced this record charged a run of
    /// consecutive expired timed waits one scheduler turn
    /// (`Scheduler::expired_wait_runs_charged_once`): a run under
    /// `--expired-wait-charge=once`, by default one outside race search.
    /// `false`, which writes the record exactly as before the field existed
    /// and is what a record without it means, is the rule of every run
    /// before it and of `each` runs: each expired wait is charged. A replay
    /// that leaves `--expired-wait-charge` unset charges as its record says,
    /// so that a chaos recording replayed without `--chaos` (or a plain one
    /// replayed with it) keeps its recorded virtual times.
    expired_wait_runs_charged_once: bool,
    /// The host files the recorded run's guest opened
    /// (`detcore_model::host_input::HostInputSet`), so that a replay can
    /// report one that changed since (`HERMIT_HOST_INPUT_CHANGED_SINCE_RECORDING`).
    /// Information only: a replay never refuses or alters its schedule for
    /// it. `None` for records written before the field existed, and for
    /// records synthesized in memory.
    host_inputs: Option<HostInputSet>,
    /// The store-exclusives that failed in the run that produced this record
    /// (aarch64, `STREX_FAIL_SPEC`). Each retried a load/store exclusive loop
    /// and so added retired branches the run's preemption points count, so a
    /// replay of this record may diverge; a replay warns at its start when it
    /// is nonzero. Zero, and absent, for a run that counted none, for other
    /// architectures, and for records written before the field existed.
    store_exclusive_failures: HostObservedCount,
    /// The record's format (`PREEMPTION_RECORD_FORMAT`): `None` for format 1.
    /// The writer states format 2 once it records an instruction-offset
    /// decision, and an edit keeps the format of the record it edits: format
    /// 2 says the schedule trace holds every offset decision, which offsets
    /// in a thread's rows alone (a record written before schedule traces held
    /// them) do not.
    format_version: Option<u32>,
}

/// A count a record's run observed on its host rather than decided: it is
/// kept in the record but compares and hashes as equal whatever its value,
/// so that records of two runs of one schedule stay equal.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct HostObservedCount(u64);

impl HostObservedCount {
    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl PartialEq for HostObservedCount {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for HostObservedCount {}

impl std::hash::Hash for HostObservedCount {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

impl std::fmt::Display for PreemptionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = serde_json::to_string(&self).unwrap();
        write!(f, "{}", str)
    }
}

impl PreemptionRecord {
    /// TODO: This is not ideal because PreemptionRecord shouldn't be doing double duty as an eventlog.
    pub fn from_sched_events(events: Vec<SchedEvent>) -> Self {
        let mut record = Self {
            per_thread: Default::default(),
            global: events,
            epoch: None,
            pipe_wake_latency: None,
            signal_state_copy: None,
            cap_preemption_charge: false,
            chaos_timeslice: None,
            guest_ids: None,
            replay_departed_at: None,
            callback_checkpoints: BTreeMap::new(),
            rng_seed: None,
            expired_wait_runs_charged_once: false,
            host_inputs: None,
            store_exclusive_failures: HostObservedCount::default(),
            format_version: None,
        };
        // A schedule spliced, cut or continued from a recording keeps the
        // offsets of the events it took, and says so: only a format-2
        // recording has events with offsets.
        record.adopt_event_offsets_format();
        record
    }

    /// Whether the run this record came from charged a run of expired timed
    /// waits one turn (`false` for each wait charged, and for records that
    /// do not say).
    pub fn expired_wait_runs_charged_once(&self) -> bool {
        self.expired_wait_runs_charged_once
    }

    /// The same record, stating how the run it came from charged runs of
    /// expired timed waits.
    pub fn with_expired_wait_runs_charged_once(mut self, once: bool) -> Self {
        self.expired_wait_runs_charged_once = once;
        self
    }

    /// The record a replay of only the first `len` scheduled events of this
    /// one follows: those events, and the callback checkpoints they reach.
    ///
    /// A checkpoint names the callback that ended one of a thread's slices
    /// and the thread's progress there (the sum of its events' counts, as
    /// replay counts it, [`callback_history::Progress`]). The checkpoints of
    /// threads the prefix runs, up to the progress each thread reaches in it,
    /// end the prefix's slices where the recording did, so a replay of the
    /// prefix keeps the software-expiry turns they mark. A schedule replayed
    /// without them has no record of those turns, and its virtual time drifts
    /// from the recording's. The scheduler runs freely after the prefix, so
    /// later checkpoints are dropped, and so are those of threads the prefix
    /// does not run, progress-0 ones included.
    ///
    /// Progress alone places a checkpoint, so one taken at exactly the
    /// progress a thread reaches at the prefix end is kept: it ended the
    /// thread's last slice in the prefix. Another one at the same progress,
    /// from a later callback of the same handler after other threads ran past
    /// the prefix, is kept too, and the replay applies it only when its
    /// recorded time and branch count are reached.
    ///
    /// The thread that runs the prefix's last event is the exception while
    /// another event follows: its checkpoints at the progress that event
    /// reaches end its slice at the prefix's own end, which decides what runs
    /// next, so they belong to the event after the prefix and are dropped. A
    /// replay of the prefix then lets that thread run on, as it does past any
    /// event the prefix leaves out. A prefix of the whole schedule keeps the
    /// record's checkpoints.
    ///
    /// The per-thread histories stay whole, for the rows the checkpoints
    /// name; a schedule replay reads only the events and the checkpoints.
    pub fn schedule_prefix(&self, len: usize) -> PreemptionRecord {
        let PreemptionRecord {
            per_thread,
            global,
            epoch,
            pipe_wake_latency,
            signal_state_copy,
            cap_preemption_charge,
            chaos_timeslice,
            guest_ids,
            callback_checkpoints,
            rng_seed,
            expired_wait_runs_charged_once,
            host_inputs,
            store_exclusive_failures: _,
            replay_departed_at,
        } = self;
        let followed = len < global.len();
        let global = global[..len.min(global.len())].to_vec();
        let mut reached = BTreeMap::<DetTid, callback_history::Progress>::new();
        for event in &global {
            reached
                .entry(event.dettid)
                .or_default()
                .observe_event(event);
        }
        // The thread whose slice is still running at the prefix's end: the
        // last event's, when that event advances it and another one follows.
        let running = global
            .last()
            .filter(|last| followed && last.count > 0)
            .map(|last| last.dettid);
        let callback_checkpoints = callback_checkpoints
            .iter()
            .filter_map(|(tid, points)| {
                let reached = reached.get(tid)?.count;
                let within = |progress: u64| {
                    if running == Some(*tid) {
                        progress < reached
                    } else {
                        progress <= reached
                    }
                };
                let kept: Vec<_> = points
                    .iter()
                    .filter(|point| within(point.origin.progress))
                    .cloned()
                    .collect();
                (!kept.is_empty()).then_some((*tid, kept))
            })
            .collect();
        PreemptionRecord {
            per_thread: per_thread.clone(),
            global,
            epoch: *epoch,
            pipe_wake_latency: *pipe_wake_latency,
            signal_state_copy: *signal_state_copy,
            cap_preemption_charge: *cap_preemption_charge,
            chaos_timeslice: *chaos_timeslice,
            guest_ids: *guest_ids,
            callback_checkpoints,
            rng_seed: *rng_seed,
            expired_wait_runs_charged_once: *expired_wait_runs_charged_once,
            host_inputs: host_inputs.clone(),
            // A count of this run's host, not part of the preemptions.
            store_exclusive_failures: HostObservedCount::default(),
            // A prefix of an inexact record is inexact too.
            replay_departed_at: *replay_departed_at,
        }
    }

    /// The same record with `events` as its schedule, or `None` if its
    /// callback checkpoints do not fit those events (see [`Self::validate`]).
    /// For a schedule that replayed this record's events and went on past
    /// them: its own recording keeps no checkpoints, since a replay records
    /// none, but the replayed part ended its slices where this record did.
    pub fn with_schedule(&self, events: Vec<SchedEvent>) -> Option<PreemptionRecord> {
        let mut record = self.clone();
        record.global = events;
        record.validate().is_ok().then_some(record)
    }

    /// The `--max-timeslice` the run that produced this record drew under
    /// `--chaos-timeslice-draw=run`, if it drew one.
    pub fn chaos_timeslice(&self) -> Option<u64> {
        self.chaos_timeslice
    }

    /// The same record, stating the timeslice its run drew.
    pub fn with_chaos_timeslice(mut self, timeslice: Option<u64>) -> Self {
        self.chaos_timeslice = timeslice;
        self
    }

    /// The same record, stating the `--pipe-wake-latency` of the trials it
    /// came from. Every record analyze, minimize or bisect writes from trials
    /// carries it, so that a replay under another value is refused.
    pub fn with_pipe_wake_latency(mut self, latency: Option<u64>) -> Self {
        self.pipe_wake_latency = latency;
        self
    }

    /// The `--pipe-wake-latency` this record was produced under (`None` for
    /// off).
    pub fn pipe_wake_latency(&self) -> Option<u64> {
        self.pipe_wake_latency
    }

    /// The virtual-time epoch this record was produced under, if it was stored.
    pub fn epoch(&self) -> Option<DateTime<Utc>> {
        self.epoch
    }

    /// Whether the signal-state copy was on in the run this record came
    /// from, if the record says.
    pub fn signal_state_copy(&self) -> Option<bool> {
        self.signal_state_copy
    }

    /// The host files the recorded run's guest opened, if the record says.
    pub fn host_inputs(&self) -> Option<&HostInputSet> {
        self.host_inputs.as_ref()
    }

    /// The store-exclusives that failed in the run this record came from
    /// (zero where none were counted).
    pub fn store_exclusive_failures(&self) -> u64 {
        self.store_exclusive_failures.0
    }

    /// A retained source event count where a replay omitted a change point.
    /// After transformations it need not index this record's global events.
    pub fn replay_departed_at(&self) -> Option<u64> {
        self.replay_departed_at
    }

    /// Refuse a known omission before replay or analysis can certify it.
    /// Unmarked legacy records retain their existing admission policy; their
    /// absence of this field is not proof of historical completeness.
    pub fn require_faithful_replay(&self) -> anyhow::Result<()> {
        if let Some(event) = self.replay_departed_at {
            anyhow::bail!(
                "HERMIT_INCOMPLETE_REPLAY_RECORD: replay_departed_at={event}: this record omits \
                 a replay change point and cannot certify faithful replay. Re-record a complete \
                 execution; --replay-edited-preemptions permits only non-verifying history exploration."
            );
        }
        Ok(())
    }

    /// Retain an omission observed in a source record, never clearing one.
    /// When sources differ, the smaller count is only a retained source
    /// observation: it is neither a common causal order nor an event index
    /// in a transformed output. Admission depends only on its presence.
    pub fn retain_replay_departure(mut self, source: Option<u64>) -> Self {
        self.replay_departed_at = match (self.replay_departed_at, source) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self
    }

    /// The same record, stating whether the copy was on in the trials it
    /// came from (`None` for unknown). Every record analyze, minimize or
    /// bisect writes from trials carries it, as it carries the pipe-wake
    /// latency.
    pub fn with_signal_state_copy(mut self, on: Option<bool>) -> Self {
        self.signal_state_copy = on;
        self
    }

    /// The same record, stating whether the trials it came from capped
    /// preemption charges, as it states the pipe-wake latency.
    pub fn with_cap_preemption_charge(mut self, on: bool) -> Self {
        self.cap_preemption_charge = on;
        self
    }

    /// Whether this record was produced under `--cap-preemption-charge`.
    pub fn cap_preemption_charge(&self) -> bool {
        self.cap_preemption_charge
    }

    /// How the run this record came from numbered its guest, if no PID
    /// namespace of its own did.
    pub fn guest_ids(&self) -> Option<detcore_model::pid::GuestIds> {
        self.guest_ids
    }

    /// The same record, retaining the guest numbering of schedules used to
    /// construct it. Namespace numbering keeps the legacy omitted field.
    pub fn with_guest_ids(mut self, ids: detcore_model::pid::GuestIds) -> Self {
        self.guest_ids = (ids != detcore_model::pid::GuestIds::Namespace).then_some(ids);
        self
    }

    /// Make a copy of everything in the `PreemptionRecord` in a public format.
    pub fn extract_all(&self) -> BTreeMap<DetTid, ThreadHistory> {
        self.per_thread.clone()
    }

    /// Inverse of from_vecs.
    pub fn as_vecs(&self) -> BTreeMap<DetTid, Vec<(LogicalTime, Priority)>> {
        let mut bt = BTreeMap::new();
        for (tid, th) in &self.per_thread {
            bt.insert(*tid, th.as_vec());
        }
        bt
    }
    /// TODO: provide a better way to create an empty PreemptionRecord but with entries for a given set of tids.
    pub fn strip_contents(mut self) -> Self {
        self.callback_checkpoints.clear();
        for history in self.per_thread.values_mut() {
            history.prio_changes = Vec::new();
            history.preemption_rcbs = Vec::new();
            history.preemption_instruction_offsets = Vec::new();
            history.chaos_epochs = Vec::new();
            history.final_prio = 1000;
        }
        self.global = Vec::new();
        self
    }

    /// Iterates over each sched event and applies function event -> *event yielding resulting iterator as new events
    /// This effectively moves content of global allowing to split or delete events based on the function provided
    pub fn split_map<F, R>(&mut self, splitter: F)
    where
        R: IntoIterator<Item = SchedEvent>,
        F: Fn(SchedEvent, &ReplayCursor<SchedEvent>) -> R,
    {
        self.callback_checkpoints.clear();
        let mut result = Vec::new();
        let global = std::mem::take(&mut self.global);
        let mut cursor = ReplayCursor::from_iter(global);

        while let Some(event) = cursor.next() {
            for new_event in splitter(event, &cursor) {
                result.push(new_event);
            }
        }
        self.global = result;
    }

    /// Iterate over the schedevents.
    pub fn schedevents_iter_mut(&mut self) -> std::slice::IterMut<'_, SchedEvent> {
        self.callback_checkpoints.clear();
        self.global.iter_mut()
    }

    /// Leave only the per-thread preemption records, not the global schedule.
    pub fn preemptions_only(&mut self) {
        self.callback_checkpoints.clear();
        self.global.clear();
    }

    /// Make a copy just of the preemptions (cheap) rather than the whole schedule trace (expensive)
    pub fn clone_preemptions_only(&self) -> Self {
        PreemptionRecord {
            per_thread: self.per_thread.clone(),
            global: Vec::new(),
            epoch: self.epoch,
            pipe_wake_latency: self.pipe_wake_latency,
            signal_state_copy: self.signal_state_copy,
            cap_preemption_charge: self.cap_preemption_charge,
            chaos_timeslice: self.chaos_timeslice,
            guest_ids: self.guest_ids,
            // A record derived from an inexact one is inexact too.
            replay_departed_at: self.replay_departed_at,
            callback_checkpoints: BTreeMap::new(),
            rng_seed: self.rng_seed,
            expired_wait_runs_charged_once: self.expired_wait_runs_charged_once,
            host_inputs: self.host_inputs.clone(),
            // A count of this run's host, not part of the preemptions.
            store_exclusive_failures: HostObservedCount::default(),
            format_version: self.format_version,
        }
    }

    /// Read-only access to the raw event schedule.
    pub fn schedevents(&self) -> &Vec<SchedEvent> {
        &self.global
    }

    /// Returns true if the schedule trace (SchedEvent) record is nonempty.
    pub fn contains_schedevents(&self) -> bool {
        !self.global.is_empty()
    }

    /// Convert from a flat vector representation (Time,Priority_AFTER_Time) into the internal representation.
    /// The very first timeslice should have a zero timestamp, but it is ignored if nonzero.
    pub fn from_vecs(bt: &BTreeMap<DetTid, Vec<(LogicalTime, Priority)>>) -> Self {
        let mut bt2 = BTreeMap::new();
        for (tid, vec) in bt {
            let th = if vec.is_empty() {
                ThreadHistory {
                    final_prio: DEFAULT_PRIORITY,
                    prio_changes: Vec::new(),
                    preemption_rcbs: Vec::new(),
                    preemption_instruction_offsets: Vec::new(),
                    chaos_epochs: Vec::new(),
                }
            } else {
                // We could insist on this invariant, but it's a little more flexible not to:
                // assert!(vec.get(0).unwrap().per_thread.is_zero());
                let (_final_end, final_prio) = vec.last().unwrap();
                let final_prio = *final_prio;

                let mut prio_changes = Vec::new();
                let mut it = vec.iter().peekable();
                while let Some((_this_ns, this_p)) = it.next() {
                    if let Some((next_ns, _next_p)) = it.peek() {
                        prio_changes.push((*next_ns, *this_p));
                    } else {
                        // Don't need the last one, because we've already used it's
                        // timestamp, and retrieved final_prio.
                        break;
                    }
                }
                assert!(final_prio >= FIRST_PRIORITY);
                assert!(final_prio <= LAST_PRIORITY);
                ThreadHistory {
                    final_prio,
                    prio_changes,
                    preemption_rcbs: Vec::new(),
                    preemption_instruction_offsets: Vec::new(),
                    chaos_epochs: Vec::new(),
                }
            };
            bt2.insert(*tid, th);
        }
        PreemptionRecord {
            per_thread: bt2,
            global: Vec::new(),
            epoch: None,
            pipe_wake_latency: None,
            signal_state_copy: None,
            cap_preemption_charge: false,
            chaos_timeslice: None,
            guest_ids: None,
            replay_departed_at: None,
            callback_checkpoints: BTreeMap::new(),
            rng_seed: None,
            expired_wait_runs_charged_once: false,
            host_inputs: None,
            store_exclusive_failures: HostObservedCount::default(),
            format_version: None,
        }
    }

    /// [`Self::from_vecs`], for rows chosen from `source`'s, as minimization
    /// chooses them. When `source` carries instruction offsets, each row
    /// also takes the exact branch target and instruction offset of
    /// `source`'s row that ended at the same time: a row replayed by logical
    /// time alone ends at a branch, not some steps past one, so it could not
    /// reproduce `source`. A record without offsets is rebuilt by time alone,
    /// exactly as `from_vecs` rebuilds it.
    ///
    /// Each row is found by the time it ended, which is unique within a
    /// thread's history (`validate`). A row `source` does not have is an
    /// error.
    pub fn from_vecs_of(
        bt: &BTreeMap<DetTid, Vec<(LogicalTime, Priority)>>,
        source: &PreemptionRecord,
    ) -> Result<Self, String> {
        let mut record = Self::from_vecs(bt);
        // Rebuilt from `source`'s rows, it is of `source`'s format.
        record.format_version = source.format_version;
        if !source.carries_instruction_offsets() {
            return Ok(record);
        }
        for (tid, history) in &mut record.per_thread {
            let Some(original) = source.per_thread.get(tid) else {
                continue;
            };
            if original.preemption_rcbs.is_empty() {
                continue;
            }
            let rows: BTreeMap<LogicalTime, (u64, Option<InstructionOffset>)> = original
                .prio_changes
                .iter()
                .enumerate()
                .map(|(row, (time, _))| {
                    let offset = original
                        .preemption_instruction_offsets
                        .get(row)
                        .copied()
                        .flatten();
                    (*time, (original.preemption_rcbs[row], offset))
                })
                .collect();
            let mut rcbs = Vec::with_capacity(history.prio_changes.len());
            let mut offsets = Vec::with_capacity(history.prio_changes.len());
            for (time, _) in &history.prio_changes {
                let (row_rcbs, offset) = rows.get(time).copied().ok_or_else(|| {
                    format!("thread {tid} has no row that ended at {time} to take its branch target from")
                })?;
                rcbs.push(row_rcbs);
                offsets.push(offset);
            }
            history.preemption_rcbs = rcbs;
            history.preemption_instruction_offsets = if offsets.iter().any(Option::is_some) {
                offsets
            } else {
                Vec::new()
            };
        }
        Ok(record)
    }

    pub(crate) fn callback_checkpoints(
        &self,
    ) -> &BTreeMap<DetTid, Vec<callback_history::Checkpoint>> {
        &self.callback_checkpoints
    }

    /// The record's format (`PREEMPTION_RECORD_FORMAT`), 1 when it states
    /// none.
    pub fn format_version(&self) -> u32 {
        self.format_version.unwrap_or(1)
    }

    /// Refuse a record of a format newer than this Hermit reads.
    pub fn check_format(&self) -> Result<(), RecordFormatRefusal> {
        check_record_format(self.format_version())
    }

    /// Whether the record carries any `--chaos-instruction-offset` decision,
    /// in a thread's rows or in its schedule trace.
    pub fn carries_instruction_offsets(&self) -> bool {
        self.per_thread
            .values()
            .any(ThreadHistory::has_instruction_offsets)
            || self.schedule_carries_instruction_offsets()
    }

    /// Whether any event of the schedule trace carries an instruction offset.
    pub fn schedule_carries_instruction_offsets(&self) -> bool {
        self.global
            .iter()
            .any(|event| event.instruction_offset.is_some())
    }

    /// State format 2 when the schedule trace carries an instruction offset:
    /// only a format-2 recording has such events, so a record built from
    /// them (a schedule analyze splices, cuts or continues) is of that format
    /// too. Offsets in a thread's rows alone never raise the format: a record
    /// written before schedule traces held them has them, and its schedule
    /// trace does not ([`RecordedOffsets::RowsOnly`]).
    fn adopt_event_offsets_format(&mut self) {
        if self.schedule_carries_instruction_offsets() {
            self.raise_format(INSTRUCTION_OFFSET_FORMAT);
        }
    }

    /// State at least `format`: a record holds every feature up to the
    /// format it states, so a feature only ever raises it, and never lowers
    /// what another feature stated.
    fn raise_format(&mut self, format: u32) {
        self.format_version = Some(
            self.format_version
                .map_or(format, |stated| stated.max(format)),
        );
    }

    /// Timestamp stripping preserves the event/progress relation, unlike an editor.
    pub(crate) fn strip_event_times(&mut self) {
        for event in &mut self.global {
            event.end_time = None;
        }
    }

    /// Extracts the inner global events.
    pub fn into_global(self) -> Vec<SchedEvent> {
        self.global
    }

    /// Save to disk.
    pub fn write_to_disk(&self, path: &Path) -> Result<(), String> {
        let mut str: String = self.to_string();
        str.push('\n');
        match File::create(path) {
            Ok(mut file) => match std::io::Write::write_all(&mut file, str.as_bytes()) {
                Ok(_) => Ok(()),
                Err(err) => Err(format!(
                    "Failed to write preemption record to file {:?}, error: {}",
                    path, err
                )),
            },
            Err(err) => Err(format!(
                "Failed to create file for preemption record {:?}, error: {}",
                path, err
            )),
        }
    }

    /// Perform internal invariant checks on the PreemptionRecord and return an error if
    /// it is not well formed.
    pub fn validate(&self) -> Result<(), String> {
        self.check_format().map_err(|refusal| refusal.to_string())?;
        if self.format_version == Some(0) {
            return Err("the preemption record states format 0, which does not exist".to_owned());
        }
        for (index, event) in self.global.iter().enumerate() {
            let Some(offset) = event.instruction_offset else {
                continue;
            };
            if self.format_version() < INSTRUCTION_OFFSET_FORMAT {
                return Err(format!(
                    "schedule event #{index} of thread {} records an instruction offset, which a \
                     format-{} record cannot hold (offsets need format \
                     {INSTRUCTION_OFFSET_FORMAT})",
                    event.dettid,
                    self.format_version()
                ));
            }
            if event.op != crate::types::Op::Branch {
                return Err(format!(
                    "schedule event #{index} of thread {} records an instruction offset on a \
                     {:?} event; only a batch of branches has one",
                    event.dettid, event.op
                ));
            }
            if offset.instructions == 0 {
                return Err(format!(
                    "schedule event #{index} of thread {} has an instruction offset of zero steps",
                    event.dettid
                ));
            }
        }
        self.validate_callback_checkpoints()?;
        for (tid, history) in &self.per_thread {
            if !is_ordinary_priority(history.final_prio) {
                return Err(format!(
                    "final priority for thread {} invalid: {}",
                    tid, history.final_prio
                ));
            }
            {
                let mut time_last = None;
                for (count, (ns, prio)) in history.prio_changes.iter().enumerate() {
                    if let Some(last) = time_last {
                        if !is_ordinary_priority(*prio) {
                            return Err(format!(
                                "preemption priority #{} for thread {} invalid: {}",
                                count, tid, prio
                            ));
                        }
                        if *ns <= last {
                            return Err(format!(
                                "thread {}'s change point {} at {} is not after change point {} \
                                 at {} (counting from 1): a thread's change points must be \
                                 strictly increasing in time",
                                tid,
                                count + 1,
                                ns,
                                count,
                                last
                            ));
                        }
                    }
                    time_last = Some(*ns);
                }
            }
            if !history.preemption_rcbs.is_empty()
                && history.preemption_rcbs.len() != history.prio_changes.len()
            {
                return Err(format!(
                    "thread {} has {} preemption times but {} RCB targets",
                    tid,
                    history.prio_changes.len(),
                    history.preemption_rcbs.len()
                ));
            }
            if !history.preemption_instruction_offsets.is_empty()
                && history.preemption_instruction_offsets.len() != history.prio_changes.len()
            {
                return Err(format!(
                    "thread {} has {} preemption times but {} instruction offsets",
                    tid,
                    history.prio_changes.len(),
                    history.preemption_instruction_offsets.len()
                ));
            }
            if !history.preemption_instruction_offsets.is_empty()
                && history.preemption_rcbs.is_empty()
            {
                return Err(format!(
                    "thread {} has instruction offsets but no RCB targets",
                    tid
                ));
            }
            if history
                .preemption_instruction_offsets
                .iter()
                .flatten()
                .any(|offset| offset.instructions == 0)
            {
                return Err(format!(
                    "thread {} has an instruction offset of zero steps",
                    tid
                ));
            }
            // A row records an offset only once its slice reached the target
            // (`ThreadState::take_ended_slice_offset`). There is no upper
            // bound: an `rt_sigreturn` among the steps cancels the event, and
            // the slice then ends at its next callback.
            if history
                .preemption_instruction_offsets
                .iter()
                .zip(&history.preemption_rcbs)
                .any(|(offset, rcbs)| offset.is_some_and(|offset| *rcbs < offset.rcb_target))
            {
                return Err(format!(
                    "thread {} has an instruction offset whose branch target its row's end precedes",
                    tid
                ));
            }
            if history
                .preemption_rcbs
                .windows(2)
                .any(|pair| pair[1] < pair[0])
            {
                return Err(format!(
                    "preemption RCB targets failed to increase for thread {}",
                    tid
                ));
            }
            let mut transition_last = None;
            let mut epoch_last = None;
            for transition in &history.chaos_epochs {
                if transition.factor.as_f64() <= 0.0 {
                    return Err(format!(
                        "chaos epoch factor for thread {} must be positive",
                        tid
                    ));
                }
                if transition_last.is_some_and(|last| transition.logical_time <= last) {
                    return Err(format!(
                        "chaos epoch transition times failed to increase for thread {}",
                        tid
                    ));
                }
                if epoch_last.is_some_and(|last| transition.epoch <= last) {
                    return Err(format!(
                        "chaos epoch numbers failed to increase for thread {}",
                        tid
                    ));
                }
                transition_last = Some(transition.logical_time);
                epoch_last = Some(transition.epoch);
            }
        }
        Ok(())
    }

    fn validate_callback_checkpoints(&self) -> Result<(), String> {
        let mut endpoints: BTreeMap<DetTid, BTreeMap<u64, crate::types::Op>> = BTreeMap::new();
        let mut progress = BTreeMap::<DetTid, u64>::new();
        for event in self.global.iter().filter(|event| {
            event.count > 0 && self.callback_checkpoints.contains_key(&event.dettid)
        }) {
            let count = progress.entry(event.dettid).or_default();
            *count = count
                .checked_add(u64::from(event.count))
                .ok_or("callback progress overflow")?;
            endpoints
                .entry(event.dettid)
                .or_default()
                .insert(*count, event.op);
        }
        for (tid, checkpoints) in &self.callback_checkpoints {
            let history = self
                .per_thread
                .get(tid)
                .ok_or("callback history has no thread")?;
            let mut previous = None;
            for checkpoint in checkpoints {
                let row = checkpoint.history_index;
                if previous.is_some_and(|(last_row, last_progress)| {
                    row <= last_row || checkpoint.origin.progress < last_progress
                }) || history.prio_changes.get(row).map(|x| x.0) != Some(checkpoint.origin.time)
                    || history.preemption_rcbs.get(row).copied() != Some(checkpoint.origin.rcbs)
                {
                    return Err(format!("invalid callback history row for {tid}: {row}"));
                }
                previous = Some((row, checkpoint.origin.progress));
                let found = if checkpoint.origin.progress == 0 {
                    checkpoint.origin.last_op.is_none()
                } else {
                    endpoints
                        .get(tid)
                        .and_then(|points| points.get(&checkpoint.origin.progress))
                        == checkpoint.origin.last_op.as_ref()
                        && checkpoint.origin.last_op.is_some()
                };
                if !found {
                    return Err(format!(
                        "callback history has no event endpoint for {tid}: {row}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Normalize priorities for readibility.
    pub fn normalize(&self) -> PreemptionRecord {
        let mut clone = self.clone();

        let mut priomap: BTreeMap<Priority, Priority> = BTreeMap::new();
        for history in clone.per_thread.values_mut() {
            let _ = priomap.insert(history.final_prio, 0);
            for (_ns, prio) in &history.prio_changes {
                let _ = priomap.insert(*prio, 0);
            }
        }

        for (cur_prio, val) in (DEFAULT_PRIORITY..).zip(priomap.values_mut()) {
            assert!(cur_prio <= LAST_PRIORITY);
            *val = cur_prio;
        }
        for history in clone.per_thread.values_mut() {
            history.final_prio = *priomap.get(&history.final_prio).unwrap();
            for (_ns, prio) in &mut history.prio_changes {
                *prio = *priomap.get(prio).unwrap();
            }
        }

        // One more pass to remove anything that just has default priority its whole lifetime.
        let mut finalmap = BTreeMap::new();
        for (tid, history) in clone.per_thread.into_iter() {
            if history.final_prio != DEFAULT_PRIORITY
                || !history.prio_changes.is_empty()
                || !history.chaos_epochs.is_empty()
            {
                assert!(finalmap.insert(tid, history).is_none());
            }
        }
        clone.per_thread = finalmap;
        clone
    }

    /// Remove whichever is the latest preemption across all thread histories. If
    /// there are no priority changes in any thread history, the entire last thread
    /// (highest thread number) is completely removed.
    /// Note this function assumes the last priority change in the list has the latest
    /// preempt time.
    pub fn with_latest_preempt_removed(&self) -> PreemptionRecord {
        let mut preempts_latest_prio_changes: Vec<(DetTid, LogicalTime)> = self
            .per_thread
            .clone()
            .into_iter()
            .map(|(tid, th)| {
                (
                    tid,
                    th.prio_changes
                        .last() // Assume last prio change in the list has the latest preempt time
                        .map_or(LogicalTime::ZERO, |prio_change| prio_change.0),
                )
            })
            .collect();
        preempts_latest_prio_changes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let tid_with_preempt_to_drop = preempts_latest_prio_changes
            .into_iter()
            .map(|(tid, _th)| tid)
            .next();

        let mut clone = self.clone();
        clone.callback_checkpoints.clear();
        if let Some(tid) = tid_with_preempt_to_drop {
            let thread_history = &mut clone.per_thread.get_mut(&tid).unwrap();
            if !thread_history.prio_changes.is_empty() {
                // There were prio changes for at least one of the threads, remove that
                // preemption as the one to drop
                let removed_prio = thread_history.prio_changes.pop().unwrap().1;
                if !thread_history.preemption_rcbs.is_empty() {
                    thread_history.preemption_rcbs.pop();
                }
                if !thread_history.preemption_instruction_offsets.is_empty() {
                    thread_history.preemption_instruction_offsets.pop();
                }
                thread_history.final_prio = removed_prio;
            } else {
                // There were no prio changes for any threads, so just remove the last
                // thread completely as the drop
                clone.per_thread.pop_last();
            }
        }
        clone
    }
}

/// The record of priorities and preemptions for a particular thread.
#[derive(
    PartialEq, // Silly protection from rustfmt disagreements.
    Debug,
    Eq,
    Clone,
    Hash,
    Deserialize,
)]
pub struct ThreadHistory {
    /// The priority of the last timeslice, after the last preemption.
    /// (This is also the initial priority if there are no preemptions.)
    /// Plain nochaos/default-heuristic raw replay retains this logical policy
    /// value; its temporary foreground/deferred dispatch bands are not changes
    /// to the recorded policy.
    pub final_prio: Priority,

    /// The preemption points and priority just BEFORE the preemption occurred.  It's
    /// private, and thus you must go through the iterator, as this will change in the
    /// future.
    prio_changes: Vec<(LogicalTime, Priority)>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact per-thread PMU RCB target for each matching priority change. Empty
    /// in older JSON artifacts, which fall back to logical-time replay.
    // `ThreadHistory` crosses a non-self-describing bincode RPC; never skip it.
    #[serde(default)]
    preemption_rcbs: Vec<u64>,

    /// Where each matching timeslice's timer was armed, for a timeslice that
    /// `--chaos-instruction-offset` delivered past its branch target (`None`
    /// for the others). Empty when no timeslice had one, as in every record
    /// written without the option and by older versions; a written record
    /// then omits it (see `Serialize`).
    #[serde(default)]
    preemption_instruction_offsets: Vec<Option<InstructionOffset>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Exact per-thread slowdown transitions. Empty artifacts from older Hermit
    /// versions remain wire-compatible when read through Serde's default.
    // `ThreadHistory` also crosses Reverie's non-self-describing bincode RPC,
    // so this field must never be skipped when it is empty.
    #[serde(default)]
    chaos_epochs: Vec<ChaosEpochTransition>,
}

/// Written by hand only to leave out empty instruction offsets from a written
/// (human-readable) record, so that a record made without
/// `--chaos-instruction-offset` is byte for byte what it was before the
/// option existed. `ThreadHistory` also crosses Detcore's RPC, whose encoding
/// is not self-describing and needs every field, in declaration order.
impl Serialize for ThreadHistory {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let omit_offsets =
            serializer.is_human_readable() && self.preemption_instruction_offsets.is_empty();
        let mut fields =
            serializer.serialize_struct("ThreadHistory", if omit_offsets { 4 } else { 5 })?;
        fields.serialize_field("final_prio", &self.final_prio)?;
        fields.serialize_field("prio_changes", &self.prio_changes)?;
        fields.serialize_field("preemption_rcbs", &self.preemption_rcbs)?;
        if omit_offsets {
            fields.skip_field("preemption_instruction_offsets")?;
        } else {
            fields.serialize_field(
                "preemption_instruction_offsets",
                &self.preemption_instruction_offsets,
            )?;
        }
        fields.serialize_field("chaos_epochs", &self.chaos_epochs)?;
        fields.end()
    }
}

impl ThreadHistory {
    /// An empty thread history, with default priority only.
    pub fn new() -> Self {
        ThreadHistory {
            final_prio: DEFAULT_PRIORITY,
            prio_changes: Vec::new(),
            preemption_rcbs: Vec::new(),
            preemption_instruction_offsets: Vec::new(),
            chaos_epochs: Vec::new(),
        }
    }

    /// Turns the thread history into an iterator that produces a stream of preemption events.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> ThreadHistoryIterator {
        ThreadHistoryIterator {
            full_history: self,
            ix: 0,
            chaos_epoch_ix: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_chaos_epochs(mut self, chaos_epochs: Vec<ChaosEpochTransition>) -> Self {
        self.chaos_epochs = chaos_epochs;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_prio_changes(mut self, prio_changes: Vec<(LogicalTime, Priority)>) -> Self {
        self.prio_changes = prio_changes;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_preemption_rcbs(mut self, preemption_rcbs: Vec<u64>) -> Self {
        self.preemption_rcbs = preemption_rcbs;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_preemption_instruction_offsets(
        mut self,
        offsets: Vec<Option<InstructionOffset>>,
    ) -> Self {
        self.preemption_instruction_offsets = offsets;
        self
    }

    /// Whether any timeslice of this history was delivered past its branch
    /// target (`--chaos-instruction-offset`).
    pub fn has_instruction_offsets(&self) -> bool {
        self.preemption_instruction_offsets
            .iter()
            .any(Option::is_some)
    }

    /// Flatten the time series into a uniform representation.
    /// This is a `(Time, Priority_AFTER_Time)` representation.
    /// The very first timeslice will have a zero timestamp.
    // Note that this is different than the internal representation which groups together
    // `Priority_UNTIL_Time, Time`, i.e. shifted by one.
    pub fn as_vec(&self) -> Vec<(LogicalTime, Priority)> {
        let mut vec = Vec::new();
        let mut time0 = LogicalTime::from_nanos(0);

        for (time1, prio) in &self.prio_changes {
            vec.push((time0, *prio));
            time0 = *time1;
        }
        vec.push((time0, self.final_prio));
        vec
    }

    /// Return the priority of the initial timeslice after the thread starts running.
    pub fn initial_priority(&self) -> Priority {
        if let Some((_ns, pr)) = self.prio_changes.first() {
            *pr
        } else {
            self.final_prio
        }
    }
}

impl Default for ThreadHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// An iterator over the preemption history of a thread.  Does not include the final
/// timeslice, which does not have an ending time.
// TODO: in the future this will perform "lazy IO" and pull from disk in batches.
#[derive(PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadHistoryIterator {
    /// For now we store the full history in memory:
    full_history: ThreadHistory,
    /// Index that tracks our position.
    ix: usize,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[serde(default)]
    chaos_epoch_ix: usize,
}

impl ThreadHistoryIterator {
    /// Return the priority of the initial slice after the thread starts running.
    pub fn initial_priority(&self) -> Priority {
        self.full_history.initial_priority()
    }

    /// Return the priority of the last slice after the last preemption.
    pub fn final_priority(&self) -> Priority {
        self.full_history.final_prio
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Consume recorded epoch transitions up to `current_time`, returning a
    /// transition only when the active recorded factor changed.
    pub fn advance_chaos_epoch(
        &mut self,
        current_time: LogicalTime,
    ) -> Option<ChaosEpochTransition> {
        let mut changed = None;
        while let Some(transition) = self.full_history.chaos_epochs.get(self.chaos_epoch_ix)
            && transition.logical_time <= current_time
        {
            changed = Some(*transition);
            self.chaos_epoch_ix += 1;
        }
        changed
    }

    /// Whether this artifact carries recorded slowdown epochs.
    pub fn has_chaos_epochs(&self) -> bool {
        !self.full_history.chaos_epochs.is_empty()
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Return the next recorded logical boundary and its exact RCB target when
    /// the artifact was produced by a version that records one.
    pub fn next_with_rcbs(&mut self) -> Option<(LogicalTime, Priority, Option<u64>)> {
        self.next_with_offset()
            .map(|(time, priority, rcbs, _)| (time, priority, rcbs))
    }

    /// [`Self::next_with_rcbs`], with where the recorded timeslice's timer
    /// was armed if `--chaos-instruction-offset` delivered it past its branch
    /// target.
    pub fn next_with_offset(
        &mut self,
    ) -> Option<(
        LogicalTime,
        Priority,
        Option<u64>,
        Option<InstructionOffset>,
    )> {
        let ix = self.ix;
        self.next().map(|(time, priority)| {
            (
                time,
                priority,
                self.full_history.preemption_rcbs.get(ix).copied(),
                self.full_history
                    .preemption_instruction_offsets
                    .get(ix)
                    .copied()
                    .flatten(),
            )
        })
    }

    /// How many recorded rows have been installed as slice ends. The row the
    /// current slice ends is the last of them: `next_with_rcbs` advances past
    /// a row as it installs it, before that row's priority request is filed.
    pub fn installed_rows(&self) -> usize {
        self.ix
    }
}

impl Iterator for ThreadHistoryIterator {
    type Item = (LogicalTime, Priority);

    fn next(&mut self) -> Option<Self::Item> {
        let vec = &self.full_history.prio_changes;
        if vec.len() > self.ix {
            let elt = vec[self.ix];
            self.ix += 1;
            Some(elt)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::Op;
    use pretty_assertions::assert_eq;
    use test_case::test_case;

    use super::*;
    use crate::types::RcbTimeMultiplier;

    /// A record written by a replay that left a change point out of it says
    /// so, with the global event count where it first did; records derived
    /// from it keep the mark; a record with nothing left out has none.
    #[test]
    fn marked_record_serialization_requires_an_envelope() {
        let ordinary = PreemptionWriter::new(None).into_string();
        assert_eq!(ordinary, "{\"per_thread\":{},\"global\":[]}");
        let mut writer = PreemptionWriter::new(None);
        writer.mark_replay_departed(7);
        let text = writer.into_string();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            value.get("per_thread").is_none(),
            "old readers must not accept {text}"
        );
        assert!(
            value.get("global").is_none(),
            "old readers must not accept {text}"
        );
        assert_eq!(value["incomplete_preemption_record"]["version"], 1);
        assert_eq!(
            value["incomplete_preemption_record"]["replay_departed_at"],
            7
        );
        let back: PreemptionRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back.replay_departed_at(), Some(7));
    }

    #[test]
    fn marked_records_refuse_binary_serialization_before_writing() {
        let mut writer = PreemptionWriter::new(None);
        writer.mark_replay_departed(7);
        let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        let mut bytes = Vec::new();
        let result =
            bincode::serde::encode_into_std_write(&record, &mut bytes, bincode::config::legacy());
        assert!(
            result.is_err(),
            "a marked record has no unversioned binary encoding"
        );
        assert!(bytes.is_empty(), "refuse before writing a partial record");
    }

    #[test]
    fn a_departed_replays_record_says_it_is_not_exact() {
        let mut writer = PreemptionWriter::new(None);
        writer.mark_replay_departed(7);
        writer.mark_replay_departed(9);
        let written = writer.into_string();
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.replay_departed_at(), Some(7), "the first departure");
        assert_eq!(back.clone_preemptions_only().replay_departed_at(), Some(7));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(read_recorded_replay_departure(&path), Ok(Some(7)));
        let exact = PreemptionWriter::new(None).into_string();
        assert!(!exact.contains("replay_departed_at"), "{exact}");
        std::fs::write(&path, &exact).unwrap();
        assert_eq!(read_recorded_replay_departure(&path), Ok(None));
    }

    /// `--pipe-wake-latency` rides in a preemption record as the epoch does:
    /// omitted when off, kept through JSON and through every record a run or
    /// an analysis writes, and compared exactly, so that `Some(0)` is not off
    /// (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn the_pipe_wake_latency_round_trips_and_is_compared_exactly() {
        let off = PreemptionRecord::from_sched_events(Vec::new());
        assert!(
            !serde_json::to_string(&off)
                .unwrap()
                .contains("pipe_wake_latency")
        );
        let dir = tempfile::tempdir().unwrap();
        for latency in [Some(0), Some(3_000_000)] {
            let record =
                PreemptionRecord::from_sched_events(Vec::new()).with_pipe_wake_latency(latency);
            let json = serde_json::to_string(&record).unwrap();
            let back: PreemptionRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(back.pipe_wake_latency(), latency);
            assert_eq!(record.clone_preemptions_only().pipe_wake_latency(), latency);
            let minimized =
                PreemptionRecord::from_vecs(&BTreeMap::new()).with_pipe_wake_latency(latency);
            assert_eq!(minimized.pipe_wake_latency(), latency);
            let path = dir.path().join("record.json");
            std::fs::write(&path, &json).unwrap();
            assert_eq!(read_recorded_pipe_wake_latency(&path), Ok(latency));
            let written = PreemptionWriter::new(None)
                .with_pipe_wake_latency(latency)
                .into_string();
            assert!(
                written.contains(&format!("\"pipe_wake_latency\":{}", latency.unwrap())),
                "{written}"
            );
        }
        for (recorded, replay, refused) in [
            (None, None, false),
            (Some(0), Some(0), false),
            (Some(0), None, true),
            (None, Some(0), true),
            (Some(0), Some(3_000_000), true),
        ] {
            assert_eq!(
                pipe_wake_latency_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay:?}"
            );
        }
    }

    /// How a run charged runs of expired timed waits rides in a preemption
    /// record only when it charged them once (`--expired-wait-charge=once`,
    /// by default outside race search), so an `each` record is written
    /// exactly as before; a record that does not say means every expired
    /// wait was charged.
    #[test]
    fn the_expired_wait_charge_rule_round_trips() {
        let unsaid = PreemptionRecord::from_sched_events(Vec::new());
        assert!(!unsaid.expired_wait_runs_charged_once());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        for once in [false, true] {
            let written = PreemptionWriter::new(None)
                .with_expired_wait_runs_charged_once(once)
                .into_string();
            assert_eq!(
                written.contains("expired_wait_runs_charged_once"),
                once,
                "{written}"
            );
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.expired_wait_runs_charged_once(), once);
            assert_eq!(
                back.clone_preemptions_only()
                    .expired_wait_runs_charged_once(),
                once
            );
            std::fs::write(&path, &written).unwrap();
            assert_eq!(
                read_recorded_expired_wait_runs_charged_once(&path),
                Ok(once)
            );
        }
        std::fs::write(&path, serde_json::to_string(&unsaid).unwrap()).unwrap();
        assert_eq!(
            read_recorded_expired_wait_runs_charged_once(&path),
            Ok(false)
        );
    }

    /// The guest numbering of a run without a PID namespace rides in its
    /// record, and a replay that numbers otherwise is refused; a namespace
    /// run's record is written as before and reads back as namespace-numbered.
    #[test]
    fn the_guest_numbering_round_trips_and_is_compared() {
        use detcore_model::pid::GuestIds;
        let chosen = |base, refuse_host_ids| GuestIds::Chosen {
            base,
            refuse_host_ids,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        let namespace = PreemptionWriter::new(None)
            .with_guest_ids(GuestIds::Namespace)
            .into_string();
        assert!(!namespace.contains("guest_ids"), "{namespace}");
        std::fs::write(&path, &namespace).unwrap();
        assert_eq!(read_recorded_guest_ids(&path), Ok(GuestIds::Namespace));
        for ids in [GuestIds::Host, chosen(4000, false)] {
            let written = PreemptionWriter::new(None)
                .with_guest_ids(ids)
                .into_string();
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.guest_ids(), Some(ids));
            assert_eq!(back.clone_preemptions_only().guest_ids(), Some(ids));
            std::fs::write(&path, &written).unwrap();
            assert_eq!(read_recorded_guest_ids(&path), Ok(ids));
        }
        for (recorded, replay, refused) in [
            (GuestIds::Namespace, GuestIds::Namespace, false),
            (chosen(4000, false), chosen(4000, true), false),
            (chosen(4000, false), chosen(4001, false), true),
            (chosen(4000, false), GuestIds::Namespace, true),
            (GuestIds::Namespace, chosen(4000, true), true),
            (GuestIds::Host, GuestIds::Host, false),
            (GuestIds::Host, GuestIds::Namespace, true),
        ] {
            assert_eq!(
                guest_ids_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay:?}"
            );
        }
    }

    /// The signal-state copy setting rides in a preemption record as the
    /// pipe-wake latency does, and a replay under the other setting is
    /// refused; a record that does not say (older, or synthesized) is not.
    /// `--chaos-timeslice-draw=run` draws a pure function of the scheduler
    /// seed, inside the range, spread evenly over its decades (log-uniform),
    /// and a record stores the draw only when there is one.
    #[test]
    fn a_drawn_chaos_timeslice_is_seeded_log_uniform_and_recorded() {
        let range: detcore_model::config::TimesliceRange = "1000..1000000000".parse().unwrap();
        let mut decades = [0u32; 6];
        for seed in 0..3000 {
            let drawn = draw_chaos_timeslice(range, seed).get();
            assert_eq!(draw_chaos_timeslice(range, seed).get(), drawn);
            assert!((1_000..=1_000_000_000).contains(&drawn), "{drawn}");
            decades[((drawn as f64).log10() as usize - 3).min(5)] += 1;
        }
        // 500 expected in each decade.
        assert!(
            decades.iter().all(|count| (400..=600).contains(count)),
            "{decades:?}"
        );
        assert_ne!(
            draw_chaos_timeslice(range, 1),
            draw_chaos_timeslice(range, 2)
        );
        let point = "4321..4321".parse().unwrap();
        assert_eq!(draw_chaos_timeslice(point, 9).get(), 4321);

        let unsaid = PreemptionWriter::new(None).into_string();
        assert!(!unsaid.contains("chaos_timeslice"), "{unsaid}");
        let written = PreemptionWriter::new(None)
            .with_chaos_timeslice(std::num::NonZeroU64::new(4321))
            .into_string();
        assert!(written.contains("\"chaos_timeslice\":4321"), "{written}");
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.chaos_timeslice(), Some(4321));
        assert_eq!(back.clone_preemptions_only().chaos_timeslice(), Some(4321));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(read_recorded_chaos_timeslice(&path), Ok(Some(4321)));
        std::fs::write(&path, &unsaid).unwrap();
        assert_eq!(read_recorded_chaos_timeslice(&path), Ok(None));
    }

    #[test]
    fn the_signal_state_copy_setting_round_trips_and_is_compared() {
        let unsaid = PreemptionRecord::from_sched_events(Vec::new());
        assert_eq!(unsaid.signal_state_copy(), None);
        assert!(
            !serde_json::to_string(&unsaid)
                .unwrap()
                .contains("signal_state_copy")
        );
        let dir = tempfile::tempdir().unwrap();
        for on in [true, false] {
            let written = PreemptionWriter::new(None)
                .with_signal_state_copy(on)
                .into_string();
            assert!(
                written.contains(&format!("\"signal_state_copy\":{on}")),
                "{written}"
            );
            let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
            assert_eq!(back.signal_state_copy(), Some(on));
            assert_eq!(back.clone_preemptions_only().signal_state_copy(), Some(on));
            let path = dir.path().join("record.json");
            std::fs::write(&path, &written).unwrap();
            assert_eq!(read_recorded_signal_state_copy(&path), Ok(Some(on)));
        }
        for (recorded, replay, refused) in [
            (None, true, false),
            (None, false, false),
            (Some(true), true, false),
            (Some(false), false, false),
            (Some(true), false, true),
            (Some(false), true, true),
        ] {
            assert_eq!(
                signal_state_copy_mismatch("--replay-schedule-from=x", recorded, replay).is_some(),
                refused,
                "{recorded:?} replayed with {replay}"
            );
        }
    }

    /// `--cap-preemption-charge` round-trips through the writer, the record
    /// and the settings reader; off writes the record as before; a replay
    /// under the other setting is refused.
    #[test]
    fn the_cap_preemption_charge_setting_round_trips_and_is_compared() {
        let off = PreemptionWriter::new(None).into_string();
        assert!(!off.contains("cap_preemption_charge"), "{off}");
        let written = PreemptionWriter::new(None)
            .with_cap_preemption_charge(true)
            .into_string();
        assert!(
            written.contains("\"cap_preemption_charge\":true"),
            "{written}"
        );
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert!(back.cap_preemption_charge());
        assert!(back.clone_preemptions_only().cap_preemption_charge());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        std::fs::write(&path, &written).unwrap();
        assert_eq!(read_recorded_cap_preemption_charge(&path), Ok(true));
        std::fs::write(&path, &off).unwrap();
        assert_eq!(read_recorded_cap_preemption_charge(&path), Ok(false));
        for (recorded, replay, refused) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
            (false, true, true),
        ] {
            assert_eq!(
                cap_preemption_charge_mismatch("--replay-schedule-from=x", recorded, replay)
                    .is_some(),
                refused,
                "{recorded} replayed with {replay}"
            );
        }
    }

    /// The run's failed store-exclusives ride in a record so that a replay
    /// can label a divergence: written only when nonzero, so a record of a
    /// run without failures is byte-for-byte what it was before the field,
    /// and read as zero from records that lack it. The count is the host's,
    /// so it never makes two records unequal, and a copy of the preemptions
    /// alone does not carry it.
    #[test]
    fn store_exclusive_failures_round_trip_and_legacy_records_have_none() {
        let none = PreemptionWriter::new(None).with_signal_state_copy(true);
        let legacy = none.into_string();
        assert!(!legacy.contains("store_exclusive_failures"), "{legacy}");
        let back: PreemptionRecord = serde_json::from_str(&legacy).unwrap();
        assert_eq!(back.store_exclusive_failures(), 0);

        let mut zero = PreemptionWriter::new(None).with_signal_state_copy(true);
        zero.record_store_exclusive_failures(0);
        assert_eq!(zero.into_string(), legacy);

        let mut writer = PreemptionWriter::new(None).with_signal_state_copy(true);
        writer.record_store_exclusive_failures(20);
        let written = writer.into_string();
        assert!(
            written.contains("\"store_exclusive_failures\":20"),
            "{written}"
        );
        let back: PreemptionRecord = serde_json::from_str(&written).unwrap();
        assert_eq!(back.store_exclusive_failures(), 20);
        assert_eq!(back.to_string(), written);
        assert_eq!(back.clone_preemptions_only().store_exclusive_failures(), 0);

        let without: PreemptionRecord = serde_json::from_str(&legacy).unwrap();
        assert_eq!(back, without);
        let hash = |record: &PreemptionRecord| {
            use std::hash::Hash;
            use std::hash::Hasher;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            record.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&back), hash(&without));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn chaos_epoch_transitions_round_trip_and_replay_exact_factors() {
        let tid = DetTid::from_raw(2);
        let first = ChaosEpochTransition {
            logical_time: LogicalTime::from_nanos(100),
            epoch: 0,
            factor: RcbTimeMultiplier::from_f64(2.5),
        };
        let second = ChaosEpochTransition {
            logical_time: LogicalTime::from_nanos(500),
            epoch: 1,
            factor: RcbTimeMultiplier::from_f64(0.75),
        };

        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_chaos_epoch(tid, first);
        writer.insert_chaos_epoch(tid, second);
        let encoded = writer.into_string();
        assert!(encoded.contains("chaos_epochs"));

        let decoded: PreemptionRecord = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        let mut history = decoded.extract_all().remove(&tid).unwrap().into_iter();
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(99)),
            None
        );
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(100)),
            Some(first)
        );
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(499)),
            None
        );
        assert_eq!(
            history.advance_chaos_epoch(LogicalTime::from_nanos(500)),
            Some(second)
        );
        assert_eq!(history.advance_chaos_epoch(LogicalTime::MAX), None);
    }

    /// Instruction offsets stamp the record format 2 and round-trip through
    /// JSON. A record without them is written exactly as before, with
    /// neither the field nor a version. An edit keeps the format of the
    /// record it edits, whatever it drops, and a schedule rebuilt from events
    /// with offsets is format 2. An offset that a record's format cannot
    /// hold, on an event other than a batch of branches, or of zero steps, is
    /// refused.
    #[test]
    fn schedule_offsets_version_the_record_and_ride_its_edits() {
        let tid = DetTid::from_raw(3);
        let offset = InstructionOffset {
            rcb_target: 18,
            instructions: 4,
        };
        let branch = |offset: Option<InstructionOffset>| {
            SchedEvent::branches(tid, 20).with_instruction_offset(offset)
        };
        let mut plain = PreemptionWriter::new(None);
        plain.insert_schedevent(branch(None));
        assert_eq!(
            plain.into_string(),
            r#"{"per_thread":{},"global":[{"dettid":3,"op":"Branch","count":20,"start_rip":null,"end_rip":null,"end_time":null}]}"#
        );
        let unedited = PreemptionRecord::from_sched_events(vec![branch(None)]);
        assert_eq!(unedited.format_version, None);
        assert_eq!(unedited.recorded_offsets(), RecordedOffsets::None);

        let mut writer = PreemptionWriter::new(None);
        writer.insert_schedevent(branch(None));
        writer.insert_schedevent(branch(Some(offset)));
        let json = writer.into_string();
        assert!(
            json.ends_with(
                r#""instruction_offset":{"rcb_target":18,"instructions":4}}],"format_version":2}"#
            ),
            "{json}"
        );
        let record: PreemptionRecord = serde_json::from_str(&json).unwrap();
        record.validate().unwrap();
        assert_eq!(record.format_version(), INSTRUCTION_OFFSET_FORMAT);
        assert_eq!(record.recorded_offsets(), RecordedOffsets::Recorded);
        assert_eq!(record.to_string(), json);

        // A schedule rebuilt from its events (a trial, a splice, a final
        // schedule), split, or stripped of times keeps the offset and the
        // format.
        let rebuilt = PreemptionRecord::from_sched_events(record.clone().into_global());
        assert_eq!(rebuilt.format_version, Some(INSTRUCTION_OFFSET_FORMAT));
        assert_eq!(rebuilt.schedevents()[1].instruction_offset, Some(offset));
        let mut split = record.clone();
        split.split_map(|event, _| vec![event]);
        assert_eq!(split, record);
        let mut stripped = record.clone();
        stripped.strip_event_times();
        assert_eq!(stripped.format_version, Some(INSTRUCTION_OFFSET_FORMAT));
        // An edit that drops the schedule, or its offsets, keeps the format:
        // the record still comes from a Hermit that traces every offset.
        assert_eq!(
            record.clone_preemptions_only().format_version,
            Some(INSTRUCTION_OFFSET_FORMAT)
        );
        let mut preemptions = record.clone();
        preemptions.preemptions_only();
        assert_eq!(preemptions.format_version, Some(INSTRUCTION_OFFSET_FORMAT));
        assert_eq!(preemptions.recorded_offsets(), RecordedOffsets::None);
        let mut dropped = record.clone();
        dropped.split_map(|event, _| vec![event.with_instruction_offset(None)]);
        assert_eq!(dropped.format_version, Some(INSTRUCTION_OFFSET_FORMAT));
        dropped.validate().unwrap();

        // What a record's format cannot hold, or no batch of branches has.
        let mut below = record.clone();
        below.format_version = None;
        assert!(below.validate().unwrap_err().contains("cannot hold"));
        below.format_version = Some(1);
        assert!(
            below
                .validate()
                .unwrap_err()
                .contains("offsets need format 2")
        );
        let mut zero_format = record.clone();
        zero_format.format_version = Some(0);
        assert!(zero_format.validate().unwrap_err().contains("format 0"));
        let mut on_syscall = record.clone();
        on_syscall.global[1].op = Op::OtherInstructions;
        assert!(
            on_syscall
                .validate()
                .unwrap_err()
                .contains("only a batch of branches")
        );
        let mut zero_steps = record;
        zero_steps.global[1].instruction_offset = Some(InstructionOffset {
            rcb_target: 18,
            instructions: 0,
        });
        assert!(zero_steps.validate().unwrap_err().contains("zero steps"));
    }

    /// Every format a record can state, read from the file as a replay reads
    /// it: formats 1 (absent or explicit) and 2 are read; 3 and 5, newer than
    /// this Hermit, are refused by name with `RecordFormatRefusal`, kept typed
    /// through `Recording::decode` and `read_record_format`, for a legacy
    /// record and a slice-boundary one alike; 0 is malformed.
    #[test]
    fn a_record_is_read_only_in_a_format_this_hermit_reads() {
        let directory = tempfile::tempdir().unwrap();
        let tid = DetTid::from_raw(3);
        let legacy = |format: Option<u32>| {
            let mut writer = PreemptionWriter::new(None);
            writer.insert_schedevent(SchedEvent::branches(tid, 20));
            let mut json: serde_json::Value = serde_json::from_str(&writer.into_string()).unwrap();
            if let Some(format) = format {
                json["format_version"] = format.into();
            }
            json.to_string()
        };
        let boundary = |format: Option<u32>| {
            let mut json: serde_json::Value =
                serde_json::from_slice(&boundary_recording::tests::sample(false).encode().unwrap())
                    .unwrap();
            if let Some(format) = format {
                json["slice_boundary_recording"]["preemptions"]["format_version"] = format.into();
            }
            json.to_string()
        };
        for (name, make) in [
            ("legacy", &legacy as &dyn Fn(Option<u32>) -> String),
            ("boundary", &boundary),
        ] {
            for format in [None, Some(1), Some(2), Some(3), Some(5), Some(0)] {
                let bytes = make(format);
                let path = directory.path().join(format!("{name}-{format:?}.json"));
                std::fs::write(&path, &bytes).unwrap();
                let decoded = boundary_recording::Recording::decode(bytes.as_bytes());
                let stated = read_record_format(&path);
                match format {
                    None | Some(1) | Some(2) => {
                        decoded.unwrap_or_else(|e| panic!("{name} {format:?}: {e:#}"));
                        assert_eq!(stated, Ok(format.unwrap_or(1)), "{name}");
                        assert_eq!(check_record_format(format.unwrap_or(1)), Ok(()));
                    }
                    Some(0) => {
                        let error = decoded.unwrap_err();
                        assert!(
                            error.downcast_ref::<RecordFormatRefusal>().is_none(),
                            "{name}: {error:#}"
                        );
                        assert!(
                            format!("{error:#}").contains("format 0"),
                            "{name}: {error:#}"
                        );
                    }
                    Some(refused) => {
                        let error = decoded.unwrap_err();
                        assert_eq!(
                            error.downcast_ref::<RecordFormatRefusal>(),
                            Some(&RecordFormatRefusal { format: refused }),
                            "{name}: {error:#}"
                        );
                        assert_eq!(stated, Ok(refused), "{name}");
                        let message = RecordFormatRefusal { format: refused }.to_string();
                        assert!(
                            message.contains(&format!(
                                "is format {refused}, written by a newer Hermit; this Hermit \
                                 reads formats up to 2"
                            )),
                            "{message}"
                        );
                    }
                }
            }
        }
    }

    /// A record written before schedule traces held offsets has them in its
    /// rows alone, and no format (`RecordedOffsets::RowsOnly`). No edit
    /// raises its format, so no edit makes its schedule trace replayable as
    /// one that holds the offsets.
    #[test]
    fn an_edit_of_a_rows_only_record_keeps_it_rows_only() {
        let (three, four) = (DetTid::from_raw(3), DetTid::from_raw(4));
        let at = |ns| LogicalTime::from_nanos(ns);
        let offset = InstructionOffset {
            rcb_target: 18,
            instructions: 4,
        };
        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(three, DEFAULT_PRIORITY);
        writer.register_thread(four, DEFAULT_PRIORITY);
        writer.insert_reprioritization(three, at(10), 10, DEFAULT_PRIORITY, 5, None);
        writer.insert_reprioritization(three, at(20), 20, 5, 6, Some(offset));
        writer.insert_reprioritization(four, at(15), 15, DEFAULT_PRIORITY, 8, None);
        writer.insert_schedevent(SchedEvent::branches(three, 20));
        writer.insert_schedevent(SchedEvent::branches(four, 15));
        let mut legacy: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        // As written before the schedule trace held offsets: no format.
        legacy.format_version = None;
        legacy.validate().unwrap();
        assert_eq!(legacy.recorded_offsets(), RecordedOffsets::RowsOnly);

        let rows_only = |edited: &PreemptionRecord, what: &str| {
            assert_eq!(edited.format_version, None, "{what}");
            assert_eq!(
                edited.recorded_offsets(),
                RecordedOffsets::RowsOnly,
                "{what}"
            );
        };
        let mut split = legacy.clone();
        split.split_map(|event, _| vec![event.clone(), event]);
        rows_only(&split, "split_map");
        let mut events = legacy.clone();
        events
            .schedevents_iter_mut()
            .for_each(|event| event.count += 1);
        rows_only(&events, "schedevents_iter_mut");
        let mut preemptions = legacy.clone();
        preemptions.preemptions_only();
        rows_only(&preemptions, "preemptions_only");
        rows_only(&legacy.clone_preemptions_only(), "clone_preemptions_only");
        rows_only(
            &PreemptionRecord::from_vecs_of(&legacy.as_vecs(), &legacy).unwrap(),
            "from_vecs_of",
        );
        let mut stripped = legacy.clone();
        stripped.strip_event_times();
        rows_only(&stripped, "strip_event_times");
        // Rows knocked out: the record is still of no format, and its
        // remaining rows carry no offset.
        let removed = legacy.with_latest_preempt_removed();
        assert_eq!(removed.format_version, None);
        let emptied = legacy.clone().strip_contents();
        assert_eq!(emptied.format_version, None);
        assert_eq!(emptied.recorded_offsets(), RecordedOffsets::None);
        // A schedule rebuilt from its events has no offset to say it holds.
        let rebuilt = PreemptionRecord::from_sched_events(legacy.clone().into_global());
        assert_eq!(rebuilt.format_version, None);
        assert_eq!(rebuilt.recorded_offsets(), RecordedOffsets::None);
    }

    /// Minimization rebuilds rows from times and priorities alone
    /// (`from_vecs`), which replay by time and end at a branch. From a record
    /// with instruction offsets, `from_vecs_of` gives each kept row its exact
    /// branch target and offset back, so a minimization trial still stops
    /// where the record did; from one without, it rebuilds as `from_vecs`.
    #[test]
    fn minimized_rows_keep_their_branch_targets_and_offsets() {
        let (three, four) = (DetTid::from_raw(3), DetTid::from_raw(4));
        let at = |ns| LogicalTime::from_nanos(ns);
        let offset = InstructionOffset {
            rcb_target: 18,
            instructions: 4,
        };
        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(three, DEFAULT_PRIORITY);
        writer.register_thread(four, DEFAULT_PRIORITY);
        writer.insert_reprioritization(three, at(10), 10, DEFAULT_PRIORITY, 5, None);
        writer.insert_reprioritization(three, at(20), 20, 5, 6, Some(offset));
        writer.insert_reprioritization(three, at(30), 30, 6, 7, None);
        writer.insert_reprioritization(four, at(15), 15, DEFAULT_PRIORITY, 8, None);
        let source: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        assert_eq!(source.format_version, Some(INSTRUCTION_OFFSET_FORMAT));

        // Knock out thread 3's first row, as a minimization round does.
        let mut vecs = source.as_vecs();
        vecs.get_mut(&three).unwrap().remove(1);
        let kept = PreemptionRecord::from_vecs_of(&vecs, &source).unwrap();
        kept.validate().unwrap();
        assert_eq!(kept.format_version, Some(INSTRUCTION_OFFSET_FORMAT));
        let mut rows = kept.per_thread[&three].clone().into_iter();
        assert_eq!(
            rows.next_with_offset(),
            Some((at(20), DEFAULT_PRIORITY, Some(20), Some(offset)))
        );
        assert_eq!(rows.next_with_offset(), Some((at(30), 6, Some(30), None)));
        assert_eq!(kept.per_thread[&four].preemption_rcbs, vec![15]);
        // The offset's row knocked out: the rows keep their branch targets,
        // and the record keeps its source's format.
        let mut vecs = source.as_vecs();
        vecs.get_mut(&three).unwrap().remove(2);
        let without_offset = PreemptionRecord::from_vecs_of(&vecs, &source).unwrap();
        assert!(!without_offset.carries_instruction_offsets());
        assert_eq!(
            without_offset.format_version,
            Some(INSTRUCTION_OFFSET_FORMAT)
        );
        assert_eq!(
            without_offset.per_thread[&three].preemption_rcbs,
            vec![10, 30]
        );
        // A row the source does not have.
        let mut vecs = source.as_vecs();
        vecs.get_mut(&three).unwrap().push((at(40), 9));
        assert!(PreemptionRecord::from_vecs_of(&vecs, &source).is_err());

        // A source without offsets is rebuilt by time alone, as before.
        let mut plain = PreemptionWriter::new(None);
        plain.register_thread(three, DEFAULT_PRIORITY);
        plain.insert_reprioritization(three, at(10), 10, DEFAULT_PRIORITY, 5, None);
        let plain: PreemptionRecord = serde_json::from_str(&plain.into_string()).unwrap();
        assert_eq!(
            PreemptionRecord::from_vecs_of(&plain.as_vecs(), &plain).unwrap(),
            PreemptionRecord::from_vecs(&plain.as_vecs())
        );
    }

    /// The instruction offsets ride in a thread's history beside its RCB
    /// targets: empty until a timeslice has one (so a record written without
    /// `--chaos-instruction-offset` is the same as before), then one entry per
    /// row, kept in step when a row is dropped or the contents stripped,
    /// through JSON, and checked by `validate`.
    #[test]
    fn instruction_offsets_ride_beside_their_rows() {
        let tid = DetTid::from_raw(3);
        let at = |ns| LogicalTime::from_nanos(ns);
        let mut off = PreemptionWriter::new(None);
        off.register_thread(tid, DEFAULT_PRIORITY);
        off.insert_reprioritization(tid, at(10), 1, DEFAULT_PRIORITY, 5, None);
        let off = off.into_string();
        assert!(!off.contains("preemption_instruction_offsets"), "{off}");

        let offset = InstructionOffset {
            rcb_target: 18,
            instructions: 4,
        };
        let mut writer = PreemptionWriter::new(None);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(tid, at(10), 1, DEFAULT_PRIORITY, 5, None);
        writer.insert_reprioritization(tid, at(20), 20, 5, 6, Some(offset));
        writer.insert_reprioritization(tid, at(30), 30, 6, 7, None);
        let record: PreemptionRecord = serde_json::from_str(&writer.into_string()).unwrap();
        record.validate().unwrap();
        let history = &record.per_thread[&tid];
        assert_eq!(
            history.preemption_instruction_offsets,
            vec![None, Some(offset), None]
        );
        assert!(history.has_instruction_offsets());
        // The RPC's encoding keeps every field, empty or not.
        for history in [history.clone(), ThreadHistory::new()] {
            let config = bincode::config::standard();
            let encoded = bincode::serde::encode_to_vec(&history, config).unwrap();
            let (decoded, _): (ThreadHistory, usize) =
                bincode::serde::decode_from_slice(&encoded, config).unwrap();
            assert_eq!(decoded, history);
        }
        let mut rows = history.clone().into_iter();
        assert_eq!(
            rows.next_with_offset(),
            Some((at(10), DEFAULT_PRIORITY, Some(1), None))
        );
        assert_eq!(
            rows.next_with_offset(),
            Some((at(20), 5, Some(20), Some(offset)))
        );
        assert_eq!(rows.next_with_rcbs(), Some((at(30), 6, Some(30))));

        let dropped = record.with_latest_preempt_removed();
        dropped.validate().unwrap();
        assert_eq!(
            dropped.per_thread[&tid].preemption_instruction_offsets,
            vec![None, Some(offset)]
        );
        let stripped = record.clone().strip_contents();
        assert!(!stripped.per_thread[&tid].has_instruction_offsets());
        stripped.validate().unwrap();

        let mut short = record.clone();
        short
            .per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_instruction_offsets
            .pop();
        assert!(
            short
                .validate()
                .unwrap_err()
                .contains("instruction offsets")
        );
        let mut zero = record.clone();
        zero.per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_instruction_offsets[1] = Some(InstructionOffset {
            rcb_target: 18,
            instructions: 0,
        });
        assert!(zero.validate().unwrap_err().contains("zero steps"));
        // The row ends at branch 20: an offset aimed past it cannot be its.
        let mut early = record.clone();
        early
            .per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_instruction_offsets[1] = Some(InstructionOffset {
            rcb_target: 21,
            instructions: 4,
        });
        assert!(early.validate().unwrap_err().contains("precedes"));
        let mut without_rcbs = record.clone();
        without_rcbs
            .per_thread
            .get_mut(&tid)
            .unwrap()
            .preemption_rcbs
            .clear();
        assert!(
            without_rcbs
                .validate()
                .unwrap_err()
                .contains("no RCB targets")
        );

        let dir = tempfile::tempdir().unwrap();
        let (with, without) = (
            dir.path().join("with.json"),
            dir.path().join("without.json"),
        );
        std::fs::write(&with, record.to_string()).unwrap();
        std::fs::write(&without, &off).unwrap();
        assert_eq!(read_recorded_offsets(&with), Ok(RecordedOffsets::Recorded));
        assert_eq!(read_recorded_offsets(&without), Ok(RecordedOffsets::None));
        // Rows with offsets in a record that states no format: one written
        // before the schedule trace held offsets.
        let mut rows_only = record.clone();
        rows_only.format_version = None;
        std::fs::write(&with, rows_only.to_string()).unwrap();
        assert_eq!(read_recorded_offsets(&with), Ok(RecordedOffsets::RowsOnly));
        assert_eq!(rows_only.recorded_offsets(), RecordedOffsets::RowsOnly);
        rows_only.validate().unwrap();
    }

    /// <https://github.com/rrnewton/hermit/issues/3411>: the epoch a record was
    /// made under survives serialization, the preemptions-only copy, and the
    /// non-panicking CLI reader; records written before it was stored read as
    /// `None`, and a malformed file is an error rather than a panic.
    #[test]
    fn recorded_epoch_round_trips_and_legacy_records_have_none() {
        let epoch: DateTime<Utc> = "2000-12-31T23:59:59.123456789Z".parse().unwrap();
        let tid = DetTid::from_raw(3);
        let mut writer = PreemptionWriter::new(None).with_epoch(epoch);
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(
            tid,
            LogicalTime::from_nanos(978_307_199_223_456_789),
            7,
            DEFAULT_PRIORITY,
            5,
            None,
        );
        let encoded = writer.into_string();

        let decoded: PreemptionRecord = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.epoch(), Some(epoch));
        assert_eq!(decoded.clone_preemptions_only().epoch(), Some(epoch));

        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current.json");
        std::fs::write(&current, &encoded).unwrap();
        assert_eq!(read_recorded_epoch(&current), Ok(Some(epoch)));

        let mut legacy_value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        legacy_value
            .as_object_mut()
            .unwrap()
            .remove("epoch")
            .unwrap();
        let legacy = directory.path().join("legacy.json");
        std::fs::write(&legacy, legacy_value.to_string()).unwrap();
        assert_eq!(read_recorded_epoch(&legacy), Ok(None));
        let legacy_record: PreemptionRecord = serde_json::from_value(legacy_value.clone()).unwrap();
        assert_eq!(legacy_record.epoch(), None);
        assert!(
            !serde_json::to_string(&legacy_record)
                .unwrap()
                .contains("\"epoch\"")
        );

        let malformed = directory.path().join("malformed.json");
        std::fs::write(&malformed, "{\"epoch\": 7}").unwrap();
        assert!(read_recorded_epoch(&malformed).is_err());
        assert!(read_recorded_epoch(&directory.path().join("absent.json")).is_err());
    }

    #[test]
    fn print_preemptionrecord() {
        let (file, path) = tempfile::NamedTempFile::new().unwrap().keep().unwrap();
        drop(file);
        let mut pw = PreemptionWriter::new(Some(path.clone()));
        let tid1 = DetTid::from_raw(2);
        let tid2 = DetTid::from_raw(4);
        pw.register_thread(tid1, 1000);
        pw.register_thread(tid2, 1000);

        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(3), 3, 1000, 3, None);
        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(30), 30, 3, 30, None);
        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(300), 300, 30, 300, None);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(2), 2, 1000, 2, None);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(20), 20, 2, 20, None);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(200), 200, 20, 200, None);

        // First, round trip through a pretty-printed string
        // ------------------------------------------------
        let str: String = serde_json::to_string_pretty(&pw.inner).unwrap();
        eprintln!("{}", str);
        let pr2: PreemptionRecord = serde_json::from_str(&str).unwrap();
        eprintln!("Round trip {:?}", pr2);
        assert_eq!(pw.inner, pr2);

        // Second, actually write it out to disk
        // -------------------------------------
        pw.flush().unwrap();
        let reader = PreemptionReader::new(&path);
        let th1 = reader.extract_thread_record(&tid1).unwrap();
        let th2 = reader.extract_thread_record(&tid2).unwrap();
        assert_eq!(th1.final_prio, 300);
        assert_eq!(th2.final_prio, 200);

        let mut exact = th1.clone().into_iter();
        assert_eq!(
            exact.next_with_rcbs(),
            Some((LogicalTime::from_nanos(3), 1000, Some(3)))
        );
        assert_eq!(
            exact.next_with_rcbs(),
            Some((LogicalTime::from_nanos(30), 3, Some(30)))
        );

        let it1 = th1.into_iter();
        let it2 = th2.into_iter();

        assert_eq!(it1.initial_priority(), 1000);
        assert_eq!(it2.initial_priority(), 1000);

        let v1: Vec<(LogicalTime, Priority)> = it1.collect();
        let v2: Vec<(LogicalTime, Priority)> = it2.collect();
        assert_eq!(
            v1,
            vec![
                (LogicalTime::from_nanos(3), 1000),
                (LogicalTime::from_nanos(30), 3),
                (LogicalTime::from_nanos(300), 30)
            ]
        );
        assert_eq!(
            v2,
            vec![
                (LogicalTime::from_nanos(2), 1000),
                (LogicalTime::from_nanos(20), 2),
                (LogicalTime::from_nanos(200), 20)
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn round_trip_vec_representations() {
        let str = r#"{"per_thread":{"2":{"final_prio":1716,"prio_changes":[[946684799000013020,7301],[946684799000034020,9081],[946684799000041600,9238],[946684799000054790,865],
[946684799000057440,751],[946684799000061970,275],[946684799000062730,5135],[946684799000069530,6339],
[946684799000082850,1123],[946684799000101140,7875],[946684799000140625,4203],[946684799000171780,8611],
[946684799000183550,6306],[946684799000184440,7958],[946684799000195750,8919],[946684799000226150,69],
[946684799000236380,5915],[946684799000278180,3514],[946684799000320050,30],[946684799000334630,4629],
[946684799000344650,2926],[946684799000355020,710],[946684799000365030,3513],[946684799000386350,4881],
[946684799000396360,4852],[946684799000406840,4935],[946684799000426980,6672],[946684799000437970,7727],
[946684799000452410,7017],[946684799000462430,1572],[946684799000546210,6395],[946684799000548120,3726],
[946684799000562700,846],[946684799000583090,7838],[946684799000603310,8291],[946684799000655180,210],
[946684799000666230,4576],[946684799000680910,4974],[946684799000723020,9160],[946684799000776780,439],
[946684799000777080,6791],[946684799000787220,3015],[946684799000809090,7489],[946684799000840870,7165],
[946684799000852855,4326],[946684799000854355,358],[946684799000866575,4448],[946684799000903415,6848],
[946684799000913455,1899],[946684799000923630,2117],[946684799000963980,7705],[946684799001007090,8683],
[946684799001016950,3317],[946684799001017980,5261],[946684799001027540,3478],[946684799001029990,6474],
[946684799001053545,4823],[946684799001068395,4508],[946684799001073095,194],[946684799001121035,5944],
[946684799001171285,8408],[946684799001171295,4493],[946684799001192845,3481]]}},"global":[]}"#;
        let pr: PreemptionRecord = serde_json::from_str(str).unwrap();
        let vecs = pr.as_vecs();
        let pr2 = PreemptionRecord::from_vecs(&vecs);
        assert_eq!(pr, pr2);
    }

    #[test]
    fn normalize_preemption_record() {
        let str = r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"7":{"final_prio":9722,"prio_changes":[]},
"9":{"final_prio":9982,"prio_changes":[[946684799006227400,7839]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#;
        let pr: PreemptionRecord = serde_json::from_str(str).unwrap();
        let pr2 = pr.normalize();
        pr2.validate().unwrap();
        let bmap = pr2.as_vecs();
        // Normalization may remove the useless default-prio entry:
        if let Some(x) = bmap.get(&DetTid::from_raw(3)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
        if let Some(x) = bmap.get(&DetTid::from_raw(5)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
        assert_eq!(
            bmap.get(&DetTid::from_raw(7)).unwrap(),
            &vec![(LogicalTime::from_nanos(0), 1002)]
        );
        assert_eq!(
            bmap.get(&DetTid::from_raw(9)).unwrap(),
            &vec![
                (LogicalTime::from_nanos(0), 1001),
                (LogicalTime::from_nanos(946684799006227400), 1003)
            ]
        );
        if let Some(x) = bmap.get(&DetTid::from_raw(11)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
    }

    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1002,"prio_changes":[[946684799006227410,1000],[946684799006227415,1001]]},
"9":{"final_prio":1002,"prio_changes":[[946684799006227400,1000],[946684799006227405,1001]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1001,"prio_changes":[[946684799006227410,1000]]},
"9":{"final_prio":1002,"prio_changes":[[946684799006227400,1000],[946684799006227405,1001]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes latest priority change and coalesces into final priority"
    )]
    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1001,"prio_changes":[[946684799006227400,1000]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes only priority change and coalesces into final priority"
    )]
    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes entire history for last tid if no non-default priority changes"
    )]
    fn with_latest_preempt_removed(pr_json: &str, expected_pr_json: &str) {
        let pr: PreemptionRecord = serde_json::from_str(pr_json).unwrap();
        let expected_pr: PreemptionRecord = serde_json::from_str(expected_pr_json).unwrap();

        let pr_with_latest_removed = pr.with_latest_preempt_removed();

        pr_with_latest_removed.validate().unwrap();
        self::assert_eq!(pr_with_latest_removed, expected_pr);
    }

    /// Prefix trials must keep the recording's replay settings and host
    /// inputs even when none, or only some, of its events are replayed.
    #[test]
    fn a_schedule_prefix_keeps_the_recorded_replay_settings_and_host_inputs() {
        use detcore_model::host_input::HostFileIdentity;
        use detcore_model::host_input::HostInput;

        let tid = DetTid::from_raw(3);
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 10),
            SchedEvent::branches(tid, 20),
        ])
        .with_cap_preemption_charge(true)
        .with_expired_wait_runs_charged_once(true);
        record.rng_seed = Some(1234567);
        record.host_inputs = Some(HostInputSet {
            digest: "ab".repeat(32),
            files: vec![HostInput {
                path: "/recorded/input".to_owned(),
                identity: HostFileIdentity {
                    dev: 2,
                    ino: 3,
                    size: 4,
                    mtime_sec: 5,
                    mtime_nsec: 6,
                },
                content: Some("cd".repeat(32)),
            }],
        });
        for len in [0, 1, 2, 9] {
            let prefix = record.schedule_prefix(len);
            assert_eq!(prefix.global, record.global[..len.min(record.global.len())]);
            assert_eq!(
                (
                    prefix.cap_preemption_charge,
                    prefix.rng_seed,
                    prefix.expired_wait_runs_charged_once,
                    &prefix.host_inputs,
                ),
                (true, Some(1234567), true, &record.host_inputs),
                "prefix of {len} events"
            );
        }
    }

    /// A replay of a schedule's first events keeps the callback checkpoints
    /// of the slices those events end, for the threads they run, and no later
    /// ones: the slice end at the prefix's own end belongs to the event after
    /// it. The result still validates, and lends its checkpoints to a
    /// schedule that replayed it and went on.
    #[test]
    fn a_schedule_prefix_keeps_the_checkpoints_of_the_slices_its_events_end() {
        use callback_history::Checkpoint;
        use callback_history::Origin;
        use callback_history::Site;

        use crate::slice_boundary::callbacks::Callback;
        let (main, other, late) = (
            DetTid::from_raw(3),
            DetTid::from_raw(5),
            DetTid::from_raw(7),
        );
        // Progress per thread: main 10, 11 (after the OtherInstructions), 21;
        // other 4; late 6.
        let mut record = PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(main, 10),
            SchedEvent::branches(other, 4),
            SchedEvent {
                op: Op::OtherInstructions,
                ..SchedEvent::branches(main, 1)
            },
            SchedEvent::branches(main, 10),
            SchedEvent::branches(late, 6),
        ]);
        let mut point = |tid: DetTid, row: usize, progress: u64, last_op: Op| {
            let time = LogicalTime::from_nanos(100 * (row as u64 + 1));
            let history = record.per_thread.entry(tid).or_default();
            history.prio_changes.push((time, 1000));
            history.preemption_rcbs.push(progress);
            Checkpoint {
                history_index: row,
                origin: Origin {
                    callback: Callback::PmuTimer,
                    site: Site::Prehook,
                    progress,
                    last_op: Some(last_op),
                    time,
                    rcbs: progress,
                },
            }
        };
        let main_points = vec![
            point(main, 0, 10, Op::Branch),
            point(main, 1, 11, Op::OtherInstructions),
            point(main, 2, 21, Op::Branch),
            // A later callback at the same progress, after the schedule ends.
            point(main, 3, 21, Op::Branch),
        ];
        let other_points = vec![point(other, 0, 4, Op::Branch)];
        let late_points = vec![point(late, 0, 6, Op::Branch)];
        record
            .callback_checkpoints
            .insert(main, main_points.clone());
        record
            .callback_checkpoints
            .insert(other, other_points.clone());
        record
            .callback_checkpoints
            .insert(late, late_points.clone());
        record.validate().unwrap();

        let kept = |len: usize| {
            let prefix = record.schedule_prefix(len);
            prefix.validate().unwrap();
            assert_eq!(prefix.global, record.global[..len]);
            prefix.callback_checkpoints
        };
        assert!(kept(0).is_empty());
        // Each prefix of 1 to 4 events stops where its last event's slice
        // ends (main's at 10, other's at 4, main's at 11, main's two at 21),
        // so it drops those checkpoints and keeps the earlier ones.
        assert!(kept(1).is_empty());
        assert_eq!(kept(2), BTreeMap::from([(main, main_points[..1].to_vec())]));
        assert_eq!(
            kept(3),
            BTreeMap::from([
                (main, main_points[..1].to_vec()),
                (other, other_points.clone()),
            ])
        );
        assert_eq!(
            kept(4),
            BTreeMap::from([
                (main, main_points[..2].to_vec()),
                (other, other_points.clone()),
            ])
        );
        // The whole schedule has no event after it, nor has a longer prefix.
        assert_eq!(kept(5), record.callback_checkpoints);
        assert_eq!(
            record.schedule_prefix(9).callback_checkpoints,
            record.callback_checkpoints
        );

        // A schedule that replayed the prefix and went on takes the prefix's
        // checkpoints; one whose events they do not fit does not.
        let prefix = record.schedule_prefix(3);
        let mut continued = record.global[..3].to_vec();
        continued.push(SchedEvent::branches(other, 9));
        assert_eq!(
            prefix
                .with_schedule(continued.clone())
                .map(|record| (record.global, record.callback_checkpoints)),
            Some((continued, prefix.callback_checkpoints.clone()))
        );
        assert!(prefix.with_schedule(Vec::new()).is_none());
    }

    #[test]
    fn callback_checkpoint_metadata_validates_and_only_time_stripping_preserves_it() {
        use callback_history::Checkpoint;
        use callback_history::Origin;
        use callback_history::Site;
        use reverie::syscalls::Sysno;

        use crate::slice_boundary::callbacks::Callback;
        let tid = DetTid::from_raw(3);
        let mut record = PreemptionRecord::from_sched_events(vec![SchedEvent::branches(tid, 10)]);
        let time = LogicalTime::from_nanos(100);
        let mut history = ThreadHistory::new();
        history.prio_changes.push((time, 1000));
        history.preemption_rcbs.push(10);
        record.per_thread.insert(tid, history);
        let point = Checkpoint {
            history_index: 0,
            origin: Origin {
                callback: Callback::Syscall(Sysno::close),
                site: Site::Posthook,
                progress: 10,
                last_op: Some(Op::Branch),
                time,
                rcbs: 10,
            },
        };
        record.callback_checkpoints.insert(tid, vec![point.clone()]);
        record.validate().unwrap();
        let mut stripped = record.clone();
        stripped.strip_event_times();
        assert_eq!(stripped.callback_checkpoints, record.callback_checkpoints);
        stripped.validate().unwrap();
        assert_eq!(
            record.normalize().callback_checkpoints,
            record.callback_checkpoints
        );
        for edited in [
            record.clone().strip_contents(),
            record.clone_preemptions_only(),
            record.with_latest_preempt_removed(),
        ] {
            assert!(edited.callback_checkpoints.is_empty());
        }
        let mut edited = record.clone();
        edited.split_map(|event, _| [event]);
        assert!(edited.callback_checkpoints.is_empty());
        let mut edited = record.clone();
        let _ = edited.schedevents_iter_mut();
        assert!(edited.callback_checkpoints.is_empty());
        let mut edited = record.clone();
        edited.preemptions_only();
        assert!(edited.callback_checkpoints.is_empty());
        for field in ["history_index", "progress", "time", "rcbs"] {
            let mut bad = record.clone();
            let point = &mut bad.callback_checkpoints.get_mut(&tid).unwrap()[0];
            match field {
                "history_index" => point.history_index = 1,
                "progress" => point.origin.progress = 9,
                "time" => point.origin.time = LogicalTime::from_nanos(101),
                "rcbs" => point.origin.rcbs = 11,
                _ => unreachable!(),
            }
            assert!(bad.validate().is_err(), "{field}");
        }
        let mut bad = record.clone();
        bad.callback_checkpoints.get_mut(&tid).unwrap().push(point);
        assert!(bad.validate().is_err());
        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: PreemptionRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, record);
        let empty = PreemptionRecord::from_sched_events(Vec::new());
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"per_thread":{},"global":[]}"#
        );
        let old: PreemptionRecord =
            serde_json::from_str(r#"{"per_thread":{},"global":[]}"#).unwrap();
        assert_eq!(old, empty);
        let bad = encoded.replace("Posthook", "UnknownHook");
        assert!(serde_json::from_str::<PreemptionRecord>(&bad).is_err());
    }

    #[test]
    fn test_split_map() {
        let mut pr: PreemptionRecord = serde_json::from_str(
            r#"
            {
                "per_thread" : {
                },
                "global" : [
                    {
                        "dettid": 3,
                        "op": "OtherInstructions",
                        "count": 1,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000000000
                    },
                    {
                        "dettid": 3,
                        "op": "Branch",
                        "count": 311,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000003110
                    },
                    {
                        "dettid": 3,
                        "op": "OtherInstructions",
                        "count": 1,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000003110
                    }
                ]
            }
        "#,
        )
        .unwrap();
        let original = pr.global.clone();
        pr.split_map(|e, _| vec![e]); // shouldn't change contents with this
        assert_eq!(original, pr.global);

        pr.split_map(|e, _| match &e {
            // this will split event with Op::Branch into two
            SchedEvent { op: Op::Branch, .. } => vec![e.clone(), e],
            _ => vec![e],
        });

        assert_eq!(
            pr.global.iter().map(|e| e.op).collect::<Vec<_>>(),
            vec![
                Op::OtherInstructions,
                Op::Branch,
                Op::Branch,
                Op::OtherInstructions
            ]
        );

        pr.split_map(|_, _| vec![]); // this will remove all entries
        assert_eq!(pr.global, Vec::new());
    }
}

// Reader and Writer implementations:
// =============================================================================

/// A writer for a stream of preemption events.
#[derive(Debug)]
pub struct PreemptionWriter {
    inner: PreemptionRecord,
    boundary_records: Option<Vec<detcore_model::schedule::boundary::Record>>,
    dest: Option<PathBuf>,
    flushed: bool,
}

impl PreemptionWriter {
    /// A new, empty record of preemptions, with an optional location on disk that it will be
    /// written to.  If no path is supplied, the results will accumulate in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        PreemptionWriter {
            inner: Default::default(),
            boundary_records: None,
            dest: path,
            flushed: false,
        }
    }

    /// Select the explicit boundary protocol before recording any global
    /// event. Metadata stays in the legacy history, with exactly one global
    /// stream in the new envelope. Existing writers remain legacy by default.
    pub fn with_slice_boundaries(mut self) -> Result<Self, String> {
        if !self.inner.global.is_empty() || self.boundary_records.is_some() {
            self.flushed = true;
            return Err("slice-boundary protocol must be selected before recording events".into());
        }
        self.boundary_records = Some(Vec::new());
        Ok(self)
    }

    /// Whether this writer must retain the complete versioned boundary stream.
    pub fn records_slice_boundaries(&self) -> bool {
        self.boundary_records.is_some()
    }

    /// The next complete stored index, before accepting an annotation. The
    /// controller validates index/stack requests before consuming its cursor.
    pub(crate) fn next_boundary_index(&self) -> Option<usize> {
        self.boundary_records.as_ref().map(Vec::len)
    }

    /// Store the virtual-time epoch the recorded logical times are measured
    /// from, so a replay can start its clock at the same instant.
    pub fn with_epoch(mut self, epoch: DateTime<Utc>) -> Self {
        self.inner.epoch = Some(epoch);
        self
    }

    /// Store the run's `--pipe-wake-latency` (`None` for off, which leaves
    /// the record as it was before the field existed).
    pub fn with_pipe_wake_latency(mut self, latency: Option<u64>) -> Self {
        self.inner.pipe_wake_latency = latency;
        self
    }

    /// Store whether the run kept the signal-state copy
    /// (`Config::signal_state_copy`).
    pub fn with_signal_state_copy(mut self, on: bool) -> Self {
        self.inner.signal_state_copy = Some(on);
        self
    }

    /// Store the seed of the guest's randomness, so a replay can hand the guest
    /// the same values.
    pub fn with_rng_seed(mut self, rng_seed: u64) -> Self {
        self.inner.rng_seed = Some(rng_seed);
        self
    }

    /// Store whether the run charged a run of expired timed waits one turn
    /// (`Scheduler::expired_wait_runs_charged_once`; `false` leaves the record
    /// as it was before the field existed).
    pub fn with_expired_wait_runs_charged_once(mut self, once: bool) -> Self {
        self.inner.expired_wait_runs_charged_once = once;
        self
    }

    /// Store whether the run capped preemption charges
    /// (`--cap-preemption-charge`; off leaves the record as it was before
    /// the field existed).
    pub fn with_cap_preemption_charge(mut self, on: bool) -> Self {
        self.inner.cap_preemption_charge = on;
        self
    }

    /// Store the host files the run's guest opened, once the run has ended
    /// and before the record is flushed.
    pub fn set_host_inputs(&mut self, inputs: HostInputSet) {
        self.inner.host_inputs = Some(inputs);
    }

    /// Store the store-exclusives that failed in the run (aarch64), counted
    /// once every thread has exited.
    pub fn record_store_exclusive_failures(&mut self, failures: u64) {
        self.inner.store_exclusive_failures = HostObservedCount(failures);
    }

    /// Store the `--max-timeslice` the run drew under `--chaos-timeslice-draw=run`
    /// (`Config::chaos_timeslice`; `None` writes nothing).
    pub fn with_chaos_timeslice(mut self, timeslice: Option<std::num::NonZeroU64>) -> Self {
        self.inner.chaos_timeslice = timeslice.map(std::num::NonZeroU64::get);
        self
    }

    /// Store how the run numbered its guest (`Config::guest_ids`), unless its
    /// PID namespace did, which leaves the record as it was before the field
    /// existed.
    pub fn with_guest_ids(mut self, ids: detcore_model::pid::GuestIds) -> Self {
        self.set_guest_ids(ids);
        self
    }

    /// [`Self::with_guest_ids`] on a writer in use: a run whose guest got a
    /// host ID after all states `GuestIds::Host`.
    pub fn set_guest_ids(&mut self, ids: detcore_model::pid::GuestIds) {
        self.inner.guest_ids = (ids != detcore_model::pid::GuestIds::Namespace).then_some(ids);
    }

    /// Does the record have zero entries?
    pub fn is_empty(&self) -> bool {
        self.inner.per_thread.is_empty()
    }

    /// Total number of elements in the record, across all threads.
    pub fn len(&self) -> usize {
        let mut count = 0;
        for v in self.inner.per_thread.values() {
            count += v.prio_changes.len() + v.chaos_epochs.len();
        }
        count
    }

    /// Register a thread with its default/final priority.
    pub fn register_thread(&mut self, tid: DetTid, prio: Priority) {
        if self
            .inner
            .per_thread
            .insert(
                tid,
                ThreadHistory {
                    final_prio: prio,
                    prio_changes: Vec::new(),
                    preemption_rcbs: Vec::new(),
                    preemption_instruction_offsets: Vec::new(),
                    chaos_epochs: Vec::new(),
                },
            )
            .is_some()
        {
            panic!(
                "PreemptionRecord: error, cannot re-register thread id already registered: {}",
                tid
            )
        }
    }

    /// Insert a new preemption point for the given thread.
    /// It must monotonically increase in time.
    ///
    /// For now it (redundantly) takes both the priority of the timeslice just finished,
    /// and the next one coming up.  This serves as a sanity check.
    ///
    /// `offset` says where the timeslice's timer was armed when
    /// `--chaos-instruction-offset` delivered it past its branch target.
    pub fn insert_reprioritization(
        &mut self,
        tid: DetTid,
        time: LogicalTime,
        rcbs: u64,
        prior_prio: Priority,
        next_prio: Priority,
        offset: Option<InstructionOffset>,
    ) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot insert a preemption before registering thread {}",
                tid
            )
        });
        assert_eq!(history.final_prio, prior_prio);
        // --chaos-instruction-offset refuses --strict-schedule-boundaries,
        // and a slice-boundary recording holding one is malformed
        // (`BoundaryRecording::new`).
        assert!(
            offset.is_none() || self.boundary_records.is_none(),
            "slice-boundary protocol refuses instruction offsets"
        );

        if let Some((last, _prio)) = history.prio_changes.last() {
            assert!(&time > last);
        }
        // The offsets stay empty until a timeslice has one, so a record
        // written without the option is the same as before it existed; the
        // first one fills in `None` for the rows before it.
        let offsets = &mut history.preemption_instruction_offsets;
        if offset.is_some() || !offsets.is_empty() {
            offsets.resize(history.prio_changes.len(), None);
            offsets.push(offset);
        }
        history.prio_changes.push((time, prior_prio));
        history.preemption_rcbs.push(rcbs);
        history.final_prio = next_prio;
        if offset.is_some() {
            self.inner.raise_format(INSTRUCTION_OFFSET_FORMAT);
        }
    }

    /// Mark this record as written by a preemption replay that left a change
    /// point out of it, first at global event count `event`
    /// ([`PreemptionRecord::replay_departed_at`]). Warns once, naming the
    /// file, so nobody replays it as an exact record.
    pub fn mark_replay_departed(&mut self, event: u64) {
        if self.inner.replay_departed_at.is_some() {
            return;
        }
        self.inner.replay_departed_at = Some(event);
        if let Some(dest) = &self.dest {
            tracing::warn!(
                "the preemption record {} retains an omitted replay change point observed at source \
                 event {}; replay_departed_at preserves that provenance and faithful replay refuses it",
                dest.display(),
                event
            );
        }
    }

    /// The time of the last change point recorded for `tid`, if any. A new one
    /// must be strictly later ([`Self::insert_reprioritization`]).
    pub fn last_reprioritization_time(&self, tid: DetTid) -> Option<LogicalTime> {
        self.inner
            .per_thread
            .get(&tid)
            .and_then(|history| history.prio_changes.last())
            .map(|(time, _)| *time)
    }

    /// How many change points are recorded for `tid`.
    pub(crate) fn reprioritization_count(&self, tid: DetTid) -> usize {
        self.inner
            .per_thread
            .get(&tid)
            .map_or(0, |history| history.prio_changes.len())
    }

    /// Attach only an observed callback expiry to the row the original helper just wrote.
    pub(crate) fn insert_callback_checkpoint(
        &mut self,
        tid: DetTid,
        origin: callback_history::Origin,
    ) {
        let history = &self.inner.per_thread[&tid];
        let history_index = history
            .prio_changes
            .len()
            .checked_sub(1)
            .expect("callback follows priority row");
        assert_eq!(history.prio_changes[history_index].0, origin.time);
        assert_eq!(history.preemption_rcbs[history_index], origin.rcbs);
        self.inner
            .callback_checkpoints
            .entry(tid)
            .or_default()
            .push(callback_history::Checkpoint {
                history_index,
                origin,
            });
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Record a deterministic slowdown epoch transition for replay.
    pub fn insert_chaos_epoch(&mut self, tid: DetTid, transition: ChaosEpochTransition) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot insert a chaos epoch before registering thread {}",
                tid
            )
        });
        if let Some(last) = history.chaos_epochs.last() {
            assert!(transition.logical_time > last.logical_time);
            assert!(transition.epoch > last.epoch);
        }
        history.chaos_epochs.push(transition);
    }

    /// Add a SchedEvent to the global log of thread behavior.
    pub fn insert_schedevent(&mut self, ev: SchedEvent) {
        if let Some(records) = &mut self.boundary_records {
            assert!(
                ev.count > 0,
                "slice-boundary protocol refuses zero-count events"
            );
            // --chaos-instruction-offset refuses --strict-schedule-boundaries,
            // and a slice-boundary recording holding one is malformed
            // (`BoundaryRecording::new`).
            assert!(
                ev.instruction_offset.is_none(),
                "slice-boundary protocol refuses instruction offsets"
            );
            records.push(detcore_model::schedule::boundary::Record::Event(ev));
            return;
        }
        if ev.count > 0 {
            if ev.instruction_offset.is_some() {
                self.inner.raise_format(INSTRUCTION_OFFSET_FORMAT);
            }
            self.inner.global.push(ev)
            // TODO, aggregation: possibly check if the last event was the SAME, and combine them,
            // increasing the count.
        } else {
            trace!("NOT recording scheduled event with zero count!");
        }
    }

    /// Append the observation of an existing slice-end request at its exact
    /// stream position. Refuse use with a legacy writer rather than silently
    /// dropping the boundary. Full stream ordering is checked before output.
    pub fn insert_slice_boundary(
        &mut self,
        boundary: detcore_model::schedule::boundary::Boundary,
    ) -> Result<usize, String> {
        let records = self
            .boundary_records
            .as_mut()
            .ok_or_else(|| "cannot append a slice boundary to a legacy recording".to_string())?;
        let index = records.len();
        boundary.validate_shape(index).map_err(|e| e.to_string())?;
        records.push(detcore_model::schedule::boundary::Record::TimesliceEnd(
            boundary,
        ));
        Ok(index)
    }

    /// Current recorded priority of a genuinely registered thread. During plain
    /// raw-schedule replay this is logical state, not a dispatch-control band.
    pub(crate) fn current_priority(&self, tid: DetTid) -> Priority {
        self.inner
            .per_thread
            .get(&tid)
            .unwrap_or_else(|| {
                panic!(
                    "PreemptionRecord: Cannot read current priority before registering thread {}",
                    tid
                )
            })
            .final_prio
    }

    /// Set the priority of the current thread after a change.
    pub fn set_current(&mut self, tid: DetTid, new_prio: Priority) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot set current priority before registering thread {}",
                tid
            )
        });
        history.final_prio = new_prio;
    }

    /// Abort writing to disk and instead gather the output thusfar into a string.
    pub fn into_string(mut self) -> String {
        self.flushed = true;
        self.serialized_string()
            .expect("invalid preemption recording")
    }

    /// Finish any IO necessary to flush the record to disk.
    /// This will also happen automatically on drop.
    pub fn flush(mut self) -> Result<(), String> {
        self.flushed = true;
        self.write_to_disk()
    }

    fn write_to_disk(&mut self) -> Result<(), String> {
        if let Some(path) = &self.dest {
            if self.boundary_records.is_none() {
                return self.inner.write_to_disk(path);
            }
            // Validate the complete stream before opening an existing file.
            let mut bytes = self.serialized_string()?.into_bytes();
            bytes.push(b'\n');
            std::fs::write(path, bytes)
                .map_err(|e| format!("Failed to write boundary recording {}: {e}", path.display()))
        } else {
            Err(
                "Cannot write_to_disk because this PreemptionWriter was created without a backing file.".to_string()
            )
        }
    }

    fn serialized_string(&self) -> Result<String, String> {
        let Some(records) = &self.boundary_records else {
            return Ok(self.inner.to_string());
        };
        let schedule = detcore_model::schedule::boundary::Schedule::new(records.clone())
            .map_err(|e| e.to_string())?;
        let recording = boundary_recording::Recording::Boundaries(
            boundary_recording::BoundaryRecording::new(self.inner.clone(), schedule)
                .map_err(|e| e.to_string())?,
        );
        let mut bytes = recording.encode().map_err(|e| e.to_string())?;
        assert_eq!(bytes.pop(), Some(b'\n'));
        String::from_utf8(bytes).map_err(|e| e.to_string())
    }
}

impl Drop for PreemptionWriter {
    fn drop(&mut self) {
        if !self.flushed
            && self.dest.is_some()
            && let Err(e) = self.write_to_disk()
        {
            panic!("Error while dropping PreemptionWriter: {}", e);
        }
    }
}

/// Read only whether a preemption record was written by a replay that left a
/// change point out of it ([`PreemptionRecord::replay_departed_at`]).
pub fn read_recorded_replay_departure(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct DepartureOnly {
        #[serde(default)]
        replay_departed_at: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<DepartureOnly>(path)?.replay_departed_at)
}

/// A reader for a stream of preemption events.
#[derive(Debug)]
pub struct PreemptionReader {
    inner: PreemptionRecord,
}

// Deprecated: use schedevents()
/// Read a full trace from disk.  Panic if it doesn't load.
pub fn read_trace(path: &Path) -> Vec<SchedEvent> {
    let pr = read_preemption_record(path);
    pr.global
}

// TODO: we should implement streaming and not read this all at once.
fn read_preemption_record(path: &Path) -> PreemptionRecord {
    let string = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Error reading file {:?}:\n {}", path, e));
    let pr: PreemptionRecord = serde_json::from_str(&string).unwrap_or_else(|e| {
        panic!(
            "Error parsing PreemptionRecord from JSON: {}\nJSON contents:\n{}",
            e, string
        )
    });
    if let Err(e) = pr.validate() {
        panic!(
            "Invalid PreemptionRecord when loading from path {}. Error:\n {}",
            path.display(),
            e
        );
    }
    pr
}

/// Read only the virtual-time epoch stored in a preemption record file.
///
/// Unlike [`PreemptionReader::new`], this reports an unreadable or malformed
/// file as an error instead of panicking, because the CLI consults it before
/// any guest starts. `Ok(None)` means the record predates stored epochs.
pub fn read_recorded_epoch(path: &Path) -> Result<Option<DateTime<Utc>>, String> {
    #[derive(Deserialize)]
    struct EpochOnly {
        #[serde(default)]
        epoch: Option<DateTime<Utc>>,
    }
    Ok(boundary_recording::read_settings::<EpochOnly>(path)?.epoch)
}

/// Read only the `--rng-seed` stored in a preemption record file, as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` means the record predates
/// the field.
pub fn read_recorded_rng_seed(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct RngSeedOnly {
        #[serde(default)]
        rng_seed: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<RngSeedOnly>(path)?.rng_seed)
}

/// What a preemption record holds of `--chaos-instruction-offset`'s
/// decisions, as a replay or an analysis must treat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedOffsets {
    /// None: the record replays as any record does.
    None,
    /// Format 2: in its rows, and in its schedule trace at every batch of
    /// branches such a timer ended. Both replays can arm them.
    Recorded,
    /// In its rows alone, in a record without a format version (written
    /// before the schedule trace held offsets): a preemption replay can arm
    /// them, but its schedule trace stops each such slice at a branch.
    RowsOnly,
    /// In its schedule trace, in a record whose format cannot hold them: a
    /// malformed record, which loading refuses (`PreemptionRecord::validate`).
    Malformed,
}

impl RecordedOffsets {
    /// Classify a record's offsets from what it holds.
    fn classify(format: u32, rows: bool, schedule: bool) -> Self {
        if format >= INSTRUCTION_OFFSET_FORMAT {
            if rows || schedule {
                Self::Recorded
            } else {
                Self::None
            }
        } else if schedule {
            Self::Malformed
        } else if rows {
            Self::RowsOnly
        } else {
            Self::None
        }
    }
}

impl PreemptionRecord {
    /// What this record holds of `--chaos-instruction-offset`'s decisions.
    pub fn recorded_offsets(&self) -> RecordedOffsets {
        RecordedOffsets::classify(
            self.format_version(),
            self.per_thread
                .values()
                .any(ThreadHistory::has_instruction_offsets),
            self.schedule_carries_instruction_offsets(),
        )
    }
}

/// Refuse `path` if it is a preemption record of a format this Hermit does
/// not read, keeping the refusal typed ([`RecordFormatRefusal`]) in the
/// error's chain so that a tool can exit with the policy status. For a tool
/// that reads a record's settings (`read_recorded_*`) before decoding it: a
/// legacy record's settings are read without its format. An unreadable or
/// unparseable file is left to the reader that needs it.
pub fn refuse_unread_record_format(path: &Path) -> anyhow::Result<()> {
    let Ok(format) = read_record_format(path) else {
        return Ok(());
    };
    check_record_format(format).map_err(|refusal| {
        anyhow::Error::new(refusal).context(format!("preemption record {}", path.display()))
    })
}

/// The format a preemption record file states (1 when it states none), in
/// a legacy record or a slice-boundary one, read without decoding the rest
/// of the record, so that a reader can refuse a format it does not read
/// before anything else. An unreadable or malformed file is an error, not a
/// panic, because the CLI consults it before any guest starts.
pub fn read_record_format(path: &Path) -> Result<u32, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read preemption record {}: {}", path.display(), e))?;
    boundary_recording::probe_format(&bytes)
        .map_err(|e| format!("cannot parse preemption record {}: {}", path.display(), e))
}

/// What a preemption record file holds of `--chaos-instruction-offset`'s
/// decisions ([`RecordedOffsets`]), read as [`read_record_format`] reads its
/// format. A replay of offsets needs the precise single steps that only the
/// ptrace backend takes, and a schedule replay needs them in the schedule
/// trace.
pub fn read_recorded_offsets(path: &Path) -> Result<RecordedOffsets, String> {
    #[derive(Deserialize)]
    struct OffsetsOnly {
        #[serde(default)]
        preemption_instruction_offsets: Vec<Option<InstructionOffset>>,
    }
    #[derive(Deserialize)]
    struct EventOffsetOnly {
        #[serde(default)]
        instruction_offset: Option<InstructionOffset>,
    }
    #[derive(Deserialize)]
    struct OffsetsAndFormat {
        #[serde(default)]
        per_thread: BTreeMap<DetTid, OffsetsOnly>,
        #[serde(default)]
        global: Vec<EventOffsetOnly>,
        #[serde(default)]
        format_version: Option<u32>,
    }
    let record = boundary_recording::read_settings::<OffsetsAndFormat>(path)?;
    let rows = record.per_thread.values().any(|history| {
        history
            .preemption_instruction_offsets
            .iter()
            .any(Option::is_some)
    });
    let schedule = record
        .global
        .iter()
        .any(|event| event.instruction_offset.is_some());
    Ok(RecordedOffsets::classify(
        record.format_version.unwrap_or(1),
        rows,
        schedule,
    ))
}

/// Read only the `--pipe-wake-latency` stored in a preemption record file, as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` means the record was
/// made with the option off, or before the field existed.
pub fn read_recorded_pipe_wake_latency(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct LatencyOnly {
        #[serde(default)]
        pipe_wake_latency: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<LatencyOnly>(path)?.pipe_wake_latency)
}

/// The refusal of a replay whose `--pipe-wake-latency` differs from the one a
/// record stores, naming the record by `source` (an option and a path), or
/// `None` when they agree. Absent and off are the same; `Some(0)` is not off.
pub fn pipe_wake_latency_mismatch(
    source: &str,
    recorded: Option<u64>,
    replay: Option<u64>,
) -> Option<String> {
    let show = |latency: Option<u64>| match latency {
        Some(ns) => format!("--pipe-wake-latency={ns}"),
        None => "no --pipe-wake-latency".to_owned(),
    };
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. A hand-off moves \
             turns and the time they charge, so the replay cannot reproduce the run. Replay \
             with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

/// Read only whether the run that wrote a preemption record file capped
/// preemption charges, as [`read_recorded_epoch`] reads the epoch. A record
/// without the field was made with the option off.
pub fn read_recorded_cap_preemption_charge(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct CapOnly {
        #[serde(default)]
        cap_preemption_charge: bool,
    }
    Ok(boundary_recording::read_settings::<CapOnly>(path)?.cap_preemption_charge)
}

/// The refusal of a replay whose `--cap-preemption-charge` differs from the
/// one a record stores, naming the record by `source` (an option and a
/// path), or `None` when they agree.
pub fn cap_preemption_charge_mismatch(
    source: &str,
    recorded: bool,
    replay: bool,
) -> Option<String> {
    let show = |on: bool| {
        if on {
            "--cap-preemption-charge"
        } else {
            "no --cap-preemption-charge"
        }
    };
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. The option changes \
             what each preemption charges the clock, so timers and timeouts move and the replay \
             cannot reproduce the run. Replay with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

/// Whether the run that wrote a preemption record file ran under
/// `--chaos-per-thread-slowdown`: a thread's rows hold the slowdown epochs
/// it recorded (`ThreadHistory::chaos_epochs`). Read as
/// [`read_recorded_epoch`] reads the epoch.
pub fn read_recorded_chaos_slowdown(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct EpochsOnly {
        #[serde(default)]
        chaos_epochs: Vec<serde::de::IgnoredAny>,
    }
    #[derive(Deserialize)]
    struct Histories {
        #[serde(default)]
        per_thread: BTreeMap<DetTid, EpochsOnly>,
    }
    Ok(boundary_recording::read_settings::<Histories>(path)?
        .per_thread
        .values()
        .any(|history| !history.chaos_epochs.is_empty()))
}

/// Read only whether the signal-state copy was on in the run that wrote a
/// preemption record file, as [`read_recorded_epoch`] reads the epoch.
/// `Ok(None)` for a record written before the field existed.
pub fn read_recorded_signal_state_copy(path: &Path) -> Result<Option<bool>, String> {
    #[derive(Deserialize)]
    struct CopyOnly {
        #[serde(default)]
        signal_state_copy: Option<bool>,
    }
    Ok(boundary_recording::read_settings::<CopyOnly>(path)?.signal_state_copy)
}

/// Read only whether a preemption record file carries callback checkpoints
/// (`Config::annotated_priority_replay`), as [`read_recorded_epoch`] reads
/// the epoch. A record without the field, older or made without chaos
/// recording, carries none. Detcore validates the whole record, checkpoints
/// included, when it loads it for the run.
pub fn read_recorded_callback_checkpoints(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct CheckpointsOnly {
        #[serde(default)]
        callback_checkpoints: BTreeMap<String, Vec<serde_json::Value>>,
    }
    Ok(boundary_recording::read_settings::<CheckpointsOnly>(path)?
        .callback_checkpoints
        .values()
        .any(|points| !points.is_empty()))
}

/// Detcore's own refusal of a run prepared to replay a callback-annotated
/// priority history (`Config::annotated_priority_replay`) on an effective
/// configuration that cannot honor its row ends
/// (`Config::annotated_priority_replay_unqualified`), checked before the
/// scheduler or any history exists. Hosts refuse earlier, by name; this
/// backstop holds for every configuration that reaches Detcore, whoever
/// prepared it. `None` for an unannotated run or a qualified one.
pub fn annotated_replay_mode_refusal(cfg: &crate::Config) -> Option<String> {
    if !cfg.annotated_priority_replay {
        return None;
    }
    cfg.annotated_priority_replay_unqualified().map(|setting| {
        format!(
            "HERMIT_CALLBACK_CHECKPOINT_UNQUALIFIED_MODE: the run was prepared to replay a \
             callback-annotated priority history, but its configuration {setting}; no thread \
             has started"
        )
    })
}

/// Read only whether the run that wrote a preemption record file charged a
/// run of expired timed waits one turn, as [`read_recorded_epoch`] reads the
/// epoch. `Ok(false)` for a record that does not say: one written before the
/// field existed, or by a run under the `each` rule (by default race search),
/// both of which charged every wait.
pub fn read_recorded_expired_wait_runs_charged_once(path: &Path) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct ChargeOnly {
        #[serde(default)]
        expired_wait_runs_charged_once: bool,
    }
    Ok(boundary_recording::read_settings::<ChargeOnly>(path)?.expired_wait_runs_charged_once)
}

/// The refusal of a replay whose explicit `--expired-wait-charge` differs from
/// the rule a record states (`recorded_once`, as
/// [`read_recorded_expired_wait_runs_charged_once`] reads it), naming the
/// record by `source` (an option and a path), or `None` when they agree or the
/// replay leaves the option unset, and so takes the record's rule. For a
/// search (`hermit analyze`, `hermit bisect`), which runs every trial under
/// one rule, the remedy names the recordings instead.
pub fn expired_wait_charge_mismatch(
    source: &str,
    recorded_once: bool,
    replay: Option<detcore_model::config::ExpiredWaitCharge>,
    search: bool,
) -> Option<String> {
    let replay = replay?;
    let recorded = detcore_model::config::ExpiredWaitCharge::from_charged_once(recorded_once);
    (recorded != replay).then(|| {
        let remedy = if search {
            format!(
                "A search replays every trial under one rule, its target's unless its run \
                 arguments give one. Re-record this one under that rule \
                 (`hermit run --expired-wait-charge={} --record-preemptions-to=...`), or, if \
                 it is the target, pass --expired-wait-charge={} in the run arguments.",
                replay.as_str(),
                recorded.as_str()
            )
        } else {
            format!(
                "Replay with --expired-wait-charge={}, or leave the option out to take the \
                 record's.",
                recorded.as_str()
            )
        };
        format!(
            "{source} was recorded with --expired-wait-charge={} and would be replayed with \
             --expired-wait-charge={}. The rule decides how much virtual time expired timed \
             waits cost, and so which thread runs next, so the replay cannot reproduce the run. \
             {remedy}",
            recorded.as_str(),
            replay.as_str(),
        )
    })
}

/// Read only the timeslice a preemption record file states its run drew under
/// `--chaos-timeslice-draw=run` (`PreemptionRecord::chaos_timeslice`), as
/// [`read_recorded_epoch`] reads the epoch. `Ok(None)` for a record without
/// one.
pub fn read_recorded_chaos_timeslice(path: &Path) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct TimesliceOnly {
        #[serde(default)]
        chaos_timeslice: Option<u64>,
    }
    Ok(boundary_recording::read_settings::<TimesliceOnly>(path)?.chaos_timeslice)
}

/// The `--max-timeslice` a chaos run with `--chaos-timeslice-draw=run` draws:
/// log-uniform over `range`, both bounds included, as a pure function of the
/// scheduler seed. A stream of its own (`CHAOS_TIMESLICE_SALT`) keeps it
/// independent of the seed's other draws, so adding the option changes no
/// other random choice of the run.
pub fn draw_chaos_timeslice(
    range: detcore_model::config::TimesliceRange,
    sched_seed: u64,
) -> std::num::NonZeroU64 {
    use rand::RngExt as _;
    use rand::SeedableRng as _;
    const CHAOS_TIMESLICE_SALT: u64 = 0x7469_6d65_736c_6963; // "timeslic"
    let mut prng = rand_pcg::Pcg64Mcg::seed_from_u64(sched_seed ^ CHAOS_TIMESLICE_SALT);
    let drawn = range.log_uniform(prng.random::<f64>()).round() as u64;
    std::num::NonZeroU64::new(drawn.clamp(range.min.get(), range.max.get()))
        .expect("the range starts at one RCB")
}

/// The refusal of a replay whose signal-state copy setting differs from the
/// one a record stores, naming the record by `source` (an option and a path),
/// or `None` when they agree or the record does not say.
pub fn signal_state_copy_mismatch(
    source: &str,
    recorded: Option<bool>,
    replay: bool,
) -> Option<String> {
    let show = |on: bool| {
        if on {
            "the signal-state copy on"
        } else {
            "the signal-state copy off (--no-signal-state-copy)"
        }
    };
    let recorded = recorded?;
    (recorded != replay).then(|| {
        format!(
            "{source} was recorded with {} and would be replayed with {}. The copy decides \
             some signal questions and adds a stop at each handler return, which can move a \
             preemption, so the replay cannot be relied on to reproduce the run. Replay with {}.",
            show(recorded),
            show(replay),
            show(recorded),
        )
    })
}

/// How the run that wrote the record at `path` numbered its guest, as the
/// record stores it: `GuestIds::Namespace` for a record that does not say,
/// which only a run its PID namespace numbered, or one before the field
/// existed, writes.
pub fn read_recorded_guest_ids(path: &Path) -> Result<detcore_model::pid::GuestIds, String> {
    #[derive(Deserialize)]
    struct IdsOnly {
        #[serde(default)]
        guest_ids: Option<detcore_model::pid::GuestIds>,
    }
    Ok(boundary_recording::read_settings::<IdsOnly>(path)?
        .guest_ids
        .unwrap_or_default())
}

/// The refusal of a replay whose guest would be numbered otherwise than the
/// run that a record (named by `source`, an option and a path) came from,
/// or `None` when they number it alike (`GuestIds::numbers_like`).
pub fn guest_ids_mismatch(
    source: &str,
    recorded: detcore_model::pid::GuestIds,
    replay: detcore_model::pid::GuestIds,
) -> Option<String> {
    (!recorded.numbers_like(&replay)).then(|| {
        format!(
            "{source} was recorded with guest process IDs {recorded}, and this replay's would be \
             {replay}. The record names every thread by its ID, so the replay cannot follow it."
        )
    })
}

// TODO: eventually this should do streaming IO, and abstract it behind this interface.
impl PreemptionReader {
    /// Fallible legacy access for editors. A boundary recording cannot be
    /// flattened into a legacy event vector, even if it has no markers.
    pub fn try_new(path: &Path) -> anyhow::Result<Self> {
        let inner =
            boundary_recording::Recording::read(path)?.into_legacy("legacy preemption reader")?;
        Ok(Self { inner })
    }

    /// Access the stored `PreemptionRecord`, and eagerly or lazily load its data from disk.
    pub fn new(path: &Path) -> Self {
        let pr = read_preemption_record(path);
        PreemptionReader { inner: pr }
    }

    /// Gets the inner `PreemptionRecord`.
    pub fn into_inner(self) -> PreemptionRecord {
        self.inner
    }

    /// Extract a copy of all the entries for a particular thread.  Note that this returns None if
    /// there is NO record for the thread (not registered).  Otherwise, it returns a copy of the
    /// `ThreadHistory`, which may be read into memory or not.
    pub fn extract_thread_record(&self, tid: &DetTid) -> Option<ThreadHistory> {
        self.inner.per_thread.get(tid).cloned()
    }

    /// Return the initial priority for a thread, if it is recorded in the record.
    pub fn thread_initial_priority(&self, tid: &DetTid) -> Option<Priority> {
        self.inner.per_thread.get(tid).map(|x| x.initial_priority())
    }

    /// Return all the threads in this record.
    pub fn all_threads(&self) -> Vec<DetTid> {
        // TODO(T110956298): have this return an iterator.
        self.inner.per_thread.keys().copied().collect()
    }

    /// Load the full record of preemptions into memory.
    pub fn load_all(&self) -> PreemptionRecord {
        self.inner.clone()
    }

    /// Size in number of preemptions, across all threads.
    pub fn size(&self) -> usize {
        let mut sum = 0;
        for th in self.inner.per_thread.values() {
            sum += th.prio_changes.len()
        }
        sum
    }
}

/// Take a file containing a PreemptionRecord and strip the times from all the events.
/// Optionally take a destination path, otherwise create a destination file in the same directory
/// with our own naming convention.
pub fn strip_times_from_events_file(
    sched_path: &Path,
    dest: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let new_path = dest.unwrap_or_else(|| sched_path.with_extension("notimes"));
    let mut recording = boundary_recording::Recording::read(sched_path)?;
    recording.strip_times();
    recording.write(&new_path)?;
    Ok(new_path)
}
