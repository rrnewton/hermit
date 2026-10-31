/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Carry recorded jumps of global time into the trial schedules that analyze
//! and bisect splice from recorded ones.
//!
//! A record lists the jumps its run made (`PreemptionRecord::time_skips`),
//! each at a position in its event stream, and a replay of the record makes
//! exactly those jumps. A trial schedule that analyze or bisect builds from a
//! recorded schedule's events carries none of them by itself, so its replay
//! would make no jump where the recording did and diverge there. This module
//! remembers every recorded schedule, and gives a trial
//! schedule the jumps of the one recording whose events it repeats furthest:
//! one recording's history, never a mix of several, which could disagree.
//!
//! Only a trial whose events are a prefix of that recording's gets a list. A
//! trial that goes on past where its events and the recording's part (a
//! bisection's splice, an edited or continued schedule) gets none, and is
//! written in a format that lists no jumps, so its replay makes them as a
//! replay of an older record does (`TimeSkipMode::Legacy`). No recording lists
//! the jumps of its events past that point, and a replay that makes only listed
//! jumps makes none there, not even where a run would compute one: a FIFO open
//! that finds nothing else can run, with a timer pending, moves global time to
//! that timer in a legacy replay and stays where it is in a driven one.

use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;

use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::TimeSkip;
use detcore::types::SchedEvent;

/// Every recorded schedule seen so far: the hash of each prefix of its events,
/// and its jumps in stream order, none for a schedule that made none.
#[derive(Debug, Default)]
pub struct TimeSkipSources {
    sources: Vec<Source>,
    /// The oldest format among the recordings remembered (`None` for format
    /// 1), once one is (`trial_format`).
    oldest_format: Option<Option<u32>>,
}

#[derive(Debug, PartialEq)]
struct Source {
    /// Entry `i` hashes the recording's first `i` events (`prefix_hashes`).
    prefix_hashes: Vec<u64>,
    skips: Vec<TimeSkip>,
}

/// One [`TimeSkipSources`] that every copy of the analyze options shares.
pub type SharedTimeSkipSources = Arc<Mutex<TimeSkipSources>>;

/// The hash of each prefix of `events`, from the empty one on: entry `i`
/// covers `events[..i]`. An event's timestamp is left out, because analyze and
/// bisect drop timestamps from the schedules they search.
fn prefix_hashes(events: &[SchedEvent]) -> Vec<u64> {
    let mut hash = 0u64;
    std::iter::once(hash)
        .chain(events.iter().map(move |event| {
            let mut hasher = DefaultHasher::new();
            hash.hash(&mut hasher);
            (
                event.dettid,
                event.op,
                event.count,
                event.start_rip,
                event.end_rip,
            )
                .hash(&mut hasher);
            hash = hasher.finish();
            hash
        }))
        .collect()
}

/// How many leading events two schedules share, from their prefix hashes:
/// the hash of a prefix covers every event before it, so prefixes match up to
/// some length and differ from there on.
fn shared_events(left: &[u64], right: &[u64]) -> usize {
    let shared = left.len().min(right.len());
    // The empty prefixes always match; find the last matching entry.
    let (mut matching, mut differing) = (0, shared);
    while matching + 1 < differing {
        let middle = (matching + differing) / 2;
        if left[middle] == right[middle] {
            matching = middle;
        } else {
            differing = middle;
        }
    }
    if shared > 0 && left[shared - 1] == right[shared - 1] {
        shared - 1
    } else {
        matching
    }
}

impl TimeSkipSources {
    /// Remember `record`. One that made no jump counts as much as one that
    /// did: a trial that repeats it furthest makes no jump either.
    pub fn remember(&mut self, record: &PreemptionRecord) {
        // `None` (format 1) orders before every numbered format.
        let format = record.format_version();
        self.oldest_format = Some(
            self.oldest_format
                .map_or(format, |oldest| oldest.min(format)),
        );
        let source = Source {
            prefix_hashes: prefix_hashes(record.schedevents()),
            skips: record.time_skips().unwrap_or_default().to_vec(),
        };
        if !self.sources.contains(&source) {
            self.sources.push(source);
        }
    }

    /// The format a trial schedule spliced from the remembered recordings is
    /// written in: the oldest of theirs, since its events follow those
    /// recordings' rules, or the newest format when none was read.
    pub fn trial_format(&self) -> Option<u32> {
        self.oldest_format
            .unwrap_or(Some(detcore::preemptions::PREEMPTION_RECORD_FORMAT))
    }

    /// The jumps a replay of `events` must make, when a recording lists them
    /// all: those of the remembered recording whose events `events` repeats
    /// furthest, in stream order, when `events` is a prefix of that
    /// recording's events. The run that recorded a jump was in the state those
    /// events lead to when it jumped, so a replay of the same events is too.
    /// One recording's jumps, validated when it was read, are never mixed with
    /// another's, which could disagree. Ties go to the recording remembered
    /// first.
    ///
    /// `None` when `events` goes on past every remembered recording's events,
    /// or when none was remembered: no recording lists the jumps of the rest,
    /// so the trial must not claim to list them (see the module's doc).
    pub fn carried(&self, events: &[SchedEvent]) -> Option<Vec<TimeSkip>> {
        if self.sources.is_empty() {
            return None;
        }
        let hashes = prefix_hashes(events);
        let mut best: Option<(usize, &Source)> = None;
        for source in &self.sources {
            let shared = shared_events(&source.prefix_hashes, &hashes);
            if best.is_none_or(|(furthest, _)| shared > furthest) {
                best = Some((shared, source));
            }
        }
        let (shared, source) = best?;
        if shared < events.len() {
            return None;
        }
        Some(
            source
                .skips
                .iter()
                .take_while(|skip| usize::try_from(skip.event).is_ok_and(|event| event <= shared))
                .copied()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use detcore::preemptions::TimeSkipReason;
    use detcore::types::DetTid;
    use detcore::types::LogicalTime;
    use detcore::types::SyscallPhase;
    use reverie::syscalls::Sysno;

    use super::*;

    fn events(tids: &[i32]) -> Vec<SchedEvent> {
        tids.iter()
            .map(|&tid| {
                SchedEvent::syscall(DetTid::from_raw(tid), Sysno::poll, SyscallPhase::Posthook)
            })
            .collect()
    }

    fn skip(event: u64, target: u64) -> TimeSkip {
        TimeSkip {
            event,
            target: LogicalTime::from_nanos(target),
            reason: TimeSkipReason::IdleGuestPollers,
        }
    }

    /// A trial that repeats a recorded schedule's prefix carries the jumps
    /// that prefix contains, at their positions. Test adapted: a trial spliced
    /// past where its events and the recording's part used to carry the
    /// prefix's jumps too, and now carries no list, since none lists the
    /// jumps past that point.
    #[test]
    fn a_spliced_prefix_carries_the_jumps_inside_it() {
        let recorded = events(&[3, 3, 5, 3, 5, 5]);
        let record = PreemptionRecord::from_sched_events(recorded.clone())
            .with_time_skips(vec![skip(2, 1_000), skip(5, 2_000)]);
        let mut sources = TimeSkipSources::default();
        sources.remember(&record);

        // The whole schedule carries both.
        assert_eq!(
            sources.carried(&recorded),
            Some(vec![skip(2, 1_000), skip(5, 2_000)])
        );
        // A prefix of three events carries the first jump, and not the second,
        // which lies past it.
        assert_eq!(sources.carried(&recorded[..3]), Some(vec![skip(2, 1_000)]));
        // A jump exactly at the prefix's end is inside it.
        assert_eq!(sources.carried(&recorded[..2]), Some(vec![skip(2, 1_000)]));
        // Spliced after event 3, the trial goes on past where its events and
        // the recording's part: it carries no list.
        let mut spliced = recorded[..3].to_vec();
        spliced.extend(events(&[5, 3, 3]));
        assert_eq!(sources.carried(&spliced), None);
        // So does a trial that differs from the first event on.
        assert_eq!(sources.carried(&events(&[5, 3, 5])), None);
    }

    /// Timestamps, which analyze and bisect strip, do not stop a prefix from
    /// matching.
    #[test]
    fn a_prefix_matches_without_its_timestamps() {
        let mut recorded = events(&[3, 5, 3]);
        for (index, event) in recorded.iter_mut().enumerate() {
            event.end_time = Some(LogicalTime::from_nanos(index as u64 + 1));
        }
        let record = PreemptionRecord::from_sched_events(recorded.clone())
            .with_time_skips(vec![skip(1, 1_000)]);
        let mut sources = TimeSkipSources::default();
        sources.remember(&record);
        let mut stripped = recorded;
        for event in &mut stripped {
            event.end_time = None;
        }
        assert_eq!(sources.carried(&stripped), Some(vec![skip(1, 1_000)]));
    }

    /// A record without jumps, and a jump past the end of its events, add
    /// nothing.
    #[test]
    fn unplaceable_jumps_are_left_out() {
        let recorded = events(&[3, 5]);
        let mut sources = TimeSkipSources::default();
        sources.remember(
            &PreemptionRecord::from_sched_events(recorded.clone())
                .with_time_skips(vec![skip(9, 1_000)]),
        );
        assert_eq!(sources.carried(&recorded), Some(Vec::new()));
    }

    /// Two recordings that share their first events but jumped differently
    /// inside them are never mixed: a trial carries the jumps of the one whose
    /// events it repeats furthest, and the result validates (Codex review,
    /// finding 5).
    #[test]
    fn recordings_that_disagree_are_not_mixed() {
        let first = events(&[3, 5, 7]);
        let second = events(&[3, 5, 9]);
        let mut sources = TimeSkipSources::default();
        sources.remember(
            &PreemptionRecord::from_sched_events(first.clone())
                .with_time_skips(vec![skip(1, 2_000)]),
        );
        sources.remember(
            &PreemptionRecord::from_sched_events(second.clone())
                .with_time_skips(vec![skip(2, 1_000)]),
        );
        let carried = sources.carried(&first).unwrap();
        assert_eq!(carried, vec![skip(1, 2_000)]);
        assert!(
            PreemptionRecord::from_sched_events(first)
                .with_time_skips(carried)
                .validate()
                .is_ok()
        );
        assert_eq!(sources.carried(&second), Some(vec![skip(2, 1_000)]));
    }

    /// A recording that made no jump is a source like any other: a trial that
    /// repeats it further than a recording that jumped makes no jump (Codex
    /// review, round 2).
    #[test]
    fn a_closer_recording_without_jumps_wins() {
        let jumped = events(&[3, 5, 7]);
        let plain = events(&[3, 5, 9]);
        let mut sources = TimeSkipSources::default();
        sources.remember(
            &PreemptionRecord::from_sched_events(jumped.clone())
                .with_time_skips(vec![skip(1, 2_000)]),
        );
        sources.remember(&PreemptionRecord::from_sched_events(plain.clone()));
        assert_eq!(sources.carried(&plain), Some(Vec::new()));
        assert_eq!(sources.carried(&jumped), Some(vec![skip(1, 2_000)]));
    }

    /// A schedule that goes on past every recording's events carries no list:
    /// one continued past a recording's end, and any schedule when no
    /// recording was remembered.
    #[test]
    fn a_schedule_past_every_recording_carries_no_list() {
        let recorded = events(&[3, 5]);
        let mut sources = TimeSkipSources::default();
        assert_eq!(sources.carried(&recorded), None);
        sources.remember(
            &PreemptionRecord::from_sched_events(recorded.clone())
                .with_time_skips(vec![skip(1, 1_000)]),
        );
        assert_eq!(sources.carried(&recorded), Some(vec![skip(1, 1_000)]));
        let mut continued = recorded;
        continued.extend(events(&[3]));
        assert_eq!(sources.carried(&continued), None);
    }

    /// A trial is written in the oldest format among the recordings read, the
    /// newest when there are none (Claude review, round 3).
    #[test]
    fn a_trial_takes_the_oldest_format_read() {
        let mut sources = TimeSkipSources::default();
        assert_eq!(
            sources.trial_format(),
            Some(detcore::preemptions::PREEMPTION_RECORD_FORMAT)
        );
        sources.remember(&PreemptionRecord::from_sched_events(events(&[3])).with_current_format());
        assert_eq!(
            sources.trial_format(),
            Some(detcore::preemptions::PREEMPTION_RECORD_FORMAT)
        );
        sources.remember(&PreemptionRecord::from_sched_events(events(&[5])));
        assert_eq!(sources.trial_format(), None);
    }
}
