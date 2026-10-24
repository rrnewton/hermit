/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::ops::Add;

use detcore_model::collections::ReplayCursor;
use detcore_model::pid::DetTid;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::SyscallPhase;
use detcore_model::time::LogicalTime;
use tracing::debug;
use tracing::info;
use tracing::trace;

const LOOKAHEAD_WINDOW: usize = 10;

/// Replayer
#[derive(Debug)]
pub struct Replayer {
    /// A cursor that holds our place in the global total order of events being replayed.
    pub cursor: ReplayCursor<SchedEvent>,
    /// Keep track of how many events we have observed during replay.  The current value is the
    /// event number of the NEXT event to replay.
    pub traced_event_count: u64,

    /// The number of events we've popped from the recording.
    pub events_popped: u64,

    /// Desync events observed per thread.  Counts different kinds of desyncs.
    pub desync_counts: BTreeMap<DetTid, DesyncStats>,
    /// A cached copy of the same (immutable) field in Config.
    pub replay_exhausted_panic: bool,
    /// A cached copy of the same (immutable) field in Config.
    pub die_on_desync: bool,

    /// How many events the replayed schedule has.
    schedule_len: u64,
    /// The schedule position of the event the last observed event consumed,
    /// if it consumed one.
    last_consumed: Option<u64>,
    /// The traced index of the first event observed after the schedule ran
    /// out.
    exhausted_from: Option<u64>,
}

/// The count of desyncs for a particular thread.
#[derive(Debug, PartialEq, Eq, Default, Copy, Clone)]
pub struct DesyncStats {
    /// Not inclusive of hard desyncs.  You must add soft+hard together to get total desyncs.
    pub soft: u64,
    pub hard: u64,
    /// The very worst kind, where we need to context switch, but we don't have a match on the
    /// current event. These are a subset of the hard desyncs.
    pub at_context_switch: u64,
    /// How many times do we find a match again, after a mismatch, specifically new obserserved
    /// events that were not expected.
    pub resync_insertions: u64,
    /// How many times do we find a match again, after a mismatch, specifically expected events that
    /// were not observed.
    pub resync_deletions: u64,
    /// How many times did we fast-forward to resync.
    pub fast_forwards: u64,

    /// A bit of statefulness: true when the last event processed was a desync.
    pub last_was_desync: bool,
}

impl Add for DesyncStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        DesyncStats {
            soft: self.soft + rhs.soft,
            hard: self.hard + rhs.hard,
            at_context_switch: self.at_context_switch + rhs.at_context_switch,
            resync_insertions: self.resync_insertions + rhs.resync_insertions,
            resync_deletions: self.resync_deletions + rhs.resync_deletions,
            fast_forwards: self.fast_forwards + rhs.fast_forwards,

            // Not commutative!
            last_was_desync: rhs.last_was_desync,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    FatalDesync,
    ReplayExausted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayAction {
    Continue(Option<LogicalTime>),
    Stop(StopReason),
    /// The boolean indicates if the context switch should be immediate (or wait till the current
    /// thread blocks itself and naturally yields control).
    ContextSwitch(bool, DetTid, Option<LogicalTime>),
}

fn compare_desync(observed: &SchedEvent, expected: &SchedEvent) -> String {
    if !is_desync(observed, expected) {
        "SYNCED".to_string()
    } else if observed.op != expected.op {
        "FULL-OP-DESYNC".to_string()
    } else {
        let mut msg = "DESYNC".to_string();
        if (expected.start_rip.is_some() && observed.start_rip != expected.start_rip)
            || (expected.end_rip.is_some() && observed.end_rip != expected.end_rip)
        {
            msg = "RIP-".to_owned() + &msg;
        }
        if observed.end_time != expected.end_time {
            msg = "TIME-".to_owned() + &msg;
        }
        if observed.op == Op::Branch
            && expected.op == Op::Branch
            && observed.count != expected.count
        {
            msg = "RCB-".to_owned() + &msg;
        }
        msg
    }
}

/// Do the events describe the same range of instructions, based on all the information present.
/// end_time is ignored because it depends on the global context, not just these events.
pub fn events_match(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    !is_desync(observed, expected)
}

fn is_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    if (observed.dettid, observed.op, observed.count)
        != (expected.dettid, expected.op, expected.count)
    {
        return true;
    }
    // It is ok for the observed event to contain information not present in the replay trace.
    if expected.end_time.is_some() && expected.end_time != observed.end_time {
        return true;
    }

    if expected.start_rip.is_some() && observed.start_rip != expected.start_rip {
        return true;
    }

    if expected.end_rip.is_some() && observed.end_rip != expected.end_rip {
        return true;
    }

    false
}

/// The fields that make `observed` a hard desync from `expected` (see
/// [`is_hard_desync`]), by name, so that a fatal desync says what differed.
/// A schedule recorded on KVM before KVM reported the guest's own return
/// address shows `end_rip` here: its syscall events hold the address of
/// the backend's trampoline.
fn hard_desync_fields(observed: &SchedEvent, expected: &SchedEvent) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if observed.dettid != expected.dettid {
        fields.push("dettid");
    }
    if observed.op != expected.op {
        fields.push("op");
    }
    if observed.start_rip != expected.start_rip {
        fields.push("start_rip");
    }
    if observed.end_rip != expected.end_rip {
        fields.push("end_rip");
    }
    fields
}

fn is_hard_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    let mut strip1 = observed.clone();
    let mut strip2 = expected.clone();
    strip1.end_time = None;
    strip2.end_time = None;
    strip1.count = 0;
    strip2.count = 0;
    // As in `is_desync`, the observed event may carry information the trace
    // lacks: a recorded timer preemption's branch event has no end RIP while the
    // replayed one does, and that is the same event, not a different one.
    if expected.start_rip.is_none() {
        strip1.start_rip = None;
    }
    if expected.end_rip.is_none() {
        strip1.end_rip = None;
    }
    strip1 != strip2
}

impl Replayer {
    /// Allocate a new replayer.
    pub fn new(i: impl IntoIterator<Item = SchedEvent>) -> Replayer {
        let cursor = ReplayCursor::from_iter(i);
        Replayer {
            schedule_len: cursor.len() as u64,
            last_consumed: None,
            exhausted_from: None,
            cursor,
            desync_counts: BTreeMap::new(),
            die_on_desync: false,
            replay_exhausted_panic: false,
            traced_event_count: 0,
            events_popped: 0,
        }
    }

    /// Whether the replay must stop the thread of `next`, a branch event, at
    /// its branch count with a PMU timer, and if so the count, relative to the
    /// thread's clock.
    ///
    /// A timer preemption records its branches alone, with the RIP the PMU
    /// stopped at: the stop comes right after a branch, so the recording gives
    /// it no OtherInstructions event (since 832ca40b7; before it, the stop had
    /// one). Only then does the replay need the timer, to stop the thread
    /// where the recording did, even when the thread is picked again and its
    /// next check-in follows after no further branch. The fragments
    /// split-branches and `hermit analyze` cut a branch event into carry no
    /// RIP, but are followed directly by more branches or by another thread's
    /// event, which marks them as stops too.
    ///
    /// Branches without a RIP followed by the thread's OtherInstructions
    /// reached a check-in after them: a syscall, a trapped instruction, a
    /// signal or an exec. The thread reaches that check-in on its own, and a
    /// context switch the schedule puts after it happens there. A stop on the
    /// branch count would land on the instructions between the last branch
    /// and the check-in and end the slice there, with a scheduler turn the
    /// recording does not have.
    fn resolve_time_to_run(&self, next: &SchedEvent) -> Option<LogicalTime> {
        let SchedEvent {
            op: Op::Branch,
            count,
            dettid,
            end_rip,
            ..
        } = next
        else {
            return None;
        };
        let timer_preemption = end_rip.is_some()
            || self
                .cursor
                .peek_nth(1)
                .is_some_and(|following| following.op == Op::Branch || following.dettid != *dettid);
        timer_preemption.then(|| {
            trace!("branch count to stop at before the next event: {}", count);
            LogicalTime::from_rcbs(u64::from(*count))
        })
    }

    fn pop_next(&mut self) -> Option<SchedEvent> {
        self.events_popped += 1;
        self.last_consumed = Some(self.schedule_len - self.cursor.len() as u64);
        self.cursor.next()
    }

    /// The schedule position of the event just observed (at traced index
    /// `traced_ix`), for `--stacktrace-event`: the position of the recorded
    /// event it consumed, or, once the schedule has run out, the positions
    /// that continue past its end (the first event after the end is position
    /// `schedule_len`). None for an observed event the replay did not match.
    ///
    /// Positions follow the recording rather than the observed count, so a
    /// replay that observed an extra event earlier (a split timeslice, say)
    /// still prints the stack of the recorded event it was asked for.
    pub(super) fn stacktrace_position(&self, traced_ix: u64) -> Option<u64> {
        if let Some(position) = self.last_consumed {
            return Some(position);
        }
        self.exhausted_from
            .map(|first| self.schedule_len + (traced_ix - first))
    }

    fn peek_next(&self) -> Option<SchedEvent> {
        self.cursor.peek().cloned()
    }

    /// The next task and its relative duration, without consuming a replay
    /// event. A successful exec can replace that task's scheduler registration
    /// after its prehook already selected it as the replay foreground task.
    pub(super) fn next_resume(&self) -> Option<(DetTid, Option<LogicalTime>)> {
        self.cursor
            .peek()
            .map(|next| (next.dettid, self.resolve_time_to_run(next)))
    }

    /// Whether the next replayed event is `tid` retrying a blocked syscall: either
    /// another unsuccessful attempt or its completion.
    pub(super) fn next_is_retry_by(&self, tid: DetTid) -> bool {
        self.cursor.peek().is_some_and(|next| {
            next.dettid == tid
                && matches!(
                    next.op,
                    Op::Syscall(_, SyscallPhase::Polling | SyscallPhase::Posthook)
                )
        })
    }

    /// Whether the next replayed event is `tid`'s unsuccessful attempt at a
    /// blocked syscall: the recording retried it and failed again.
    pub(super) fn next_is_failed_attempt_by(&self, tid: DetTid) -> bool {
        self.cursor.peek().is_some_and(|next| {
            next.dettid == tid && matches!(next.op, Op::Syscall(_, SyscallPhase::Polling))
        })
    }

    /// Whether the recorded schedule still has an event to replay. Past its
    /// end the run goes on unguided (`ReplayAction::Continue`).
    pub(super) fn has_events_left(&self) -> bool {
        self.cursor.peek().is_some()
    }

    fn next_replay_matches(&self, observed: &SchedEvent) -> bool {
        if let Some(expected) = self.cursor.peek() {
            !is_hard_desync(observed, expected)
        } else {
            false
        }
    }

    pub fn observe_event(&mut self, observed: &SchedEvent) -> ReplayAction {
        let mytid = observed.dettid;
        let current_ix = self.traced_event_count;
        self.traced_event_count += 1;
        self.last_consumed = None;
        if self.cursor.is_empty() && self.exhausted_from.is_none() {
            self.exhausted_from = Some(current_ix);
        }

        if let Some(expected) = self.peek_next() {
            debug!(
                "[detcore, dtid {}] {}: Ran event #{} {}, current replay event: {}",
                mytid,
                compare_desync(observed, &expected),
                current_ix,
                observed,
                expected,
            );
            if let Some(nxt) = self.cursor.peek_nth(1) {
                trace!("[detcore, dtid {}] NEXT event to replay {}", mytid, nxt);
            }
            if observed.dettid != expected.dettid {
                tracing::warn!(
                    "[tracereplay] expected to be running thread {}, but running {} at event #{}",
                    expected.dettid,
                    observed.dettid,
                    current_ix
                );
            }

            let is_desync = is_desync(observed, &expected);
            let mut in_hard_desync_mode = false;
            if is_desync {
                let counts = self.desync_counts.entry(mytid).or_default();
                if is_hard_desync(observed, &expected) {
                    if self.die_on_desync {
                        eprintln!(
                            "Replay mode desynchronized from trace, bailing out: event #{} differs \
                             in {} (observed {}, recorded {})",
                            current_ix,
                            hard_desync_fields(observed, &expected).join(", "),
                            observed,
                            expected,
                        );
                        return ReplayAction::Stop(StopReason::FatalDesync);
                    }
                    counts.hard += 1;
                    in_hard_desync_mode = true;
                } else {
                    // TODO: we need to handle a mismatch in counts by doing a partial-pop on the
                    // replayer state.
                    counts.soft += 1;
                }
            }
            self.counts_next_round(mytid, is_desync);

            if is_desync && let Some(ix) = self.check_fast_forward(observed) {
                debug_assert!(ix > 0);
                // This counts DELETIONS (missing events from observed stream):
                let counts = self.desync_counts.entry(mytid).or_default();
                counts.resync_deletions += 1;
                in_hard_desync_mode = false;
                // is_desync = false; // Logically correct, but... unused_assignments
            }

            if !in_hard_desync_mode {
                // Whether naturally or by fast forward, we should end up in this state before we pop:
                if !self.next_replay_matches(observed) {
                    tracing::warn!(
                        "Expected match before pop, observed: {}\n Cursor at: {}",
                        observed,
                        self.cursor.display_first_n(10)
                    );
                }
                let _ = self.pop_next();
            }

            let action = self.check_context_switch(observed);
            if let ReplayAction::ContextSwitch(_, next_tid, _) = action
                && in_hard_desync_mode
            {
                tracing::warn!(
                    "[dtid {}] context switch to {}, but in a desynced state at this event (#{}) in --replay-schedule-from",
                    &mytid,
                    next_tid,
                    current_ix
                );
                let counts = self.desync_counts.entry(mytid).or_default();
                counts.at_context_switch += 1;
            }
            action
        } else if self.replay_exhausted_panic {
            eprintln!(
                "[detcore, dtid {}] Replay trace ran out, stopping at unknown event {:?}",
                mytid, observed
            );
            ReplayAction::Stop(StopReason::ReplayExausted)
        } else {
            info!(
                "[detcore, dtid {}] Replay trace ran out, unknown event {:?}",
                &mytid, observed
            );
            ReplayAction::Continue(None)
        }
    }

    /// Logically advance the counting machinery to the next round.
    fn counts_next_round(&mut self, mytid: DetTid, is_desync: bool) {
        let counts = self.desync_counts.entry(mytid).or_default();
        if is_desync {
            // Set it for the next round:
            counts.last_was_desync = true;
        } else {
            if counts.last_was_desync {
                // This counts INSERTION events (new events in observed stream), because we
                // previously desynced and left unmatched events on the replay cursor, which are
                // now matched.
                counts.resync_insertions += 1;
            }
            counts.last_was_desync = false;
        }
    }

    /// Return how many to drop to have observed equal to the head of the replay tape,
    /// i.e. jump past missing or mismatched events on the replay tape.
    fn check_fast_forward(&mut self, observed: &SchedEvent) -> Option<usize> {
        let print_win = |sz: usize| -> String {
            let mut acc = String::new();
            for i in 0..sz {
                if let Some(ev) = self.cursor.peek_nth(i) {
                    acc.push_str(&format!(" {}", ev));
                }
            }
            acc
        };

        if let Some(ix) = self.cursor.prefix_contains(LOOKAHEAD_WINDOW, |expected| {
            events_match(observed, expected)
        }) {
            let counts = self.desync_counts.entry(observed.dettid).or_default();
            counts.fast_forwards += 1;
            info!(
                "FAST FORWARD by dropping {} events from replay to resync: {}",
                ix,
                print_win(ix)
            );
            // Drop the corresponding number of of missed/mismatched events so that the observed
            // matches the head of the replay tape (which is to be popped).
            self.cursor.drop(ix);
            Some(ix)
        } else {
            trace!(
                "No fast-forward possible for {} in window{}",
                observed,
                print_win(LOOKAHEAD_WINDOW)
            );
            None
        }
    }

    fn check_context_switch(&mut self, observed: &SchedEvent) -> ReplayAction {
        // We run after the pop of any matched action, so the next action in replay represents the
        // FUTURE not the PRESENT/PAST:
        if let Some(next_ev) = self.cursor.peek() {
            let mytid = observed.dettid;
            let time = observed.end_time.expect("timestamps required for now");
            let is_prehook = matches!(observed.op, Op::Syscall(_, SyscallPhase::Prehook));

            let next_tid = next_ev.dettid;
            // Checking for an optional timeslice in case of RCB
            let timeslice = self.resolve_time_to_run(next_ev);

            if next_tid != observed.dettid {
                if is_prehook {
                    info!(
                        "[detcore, dtid {}] CONTEXT SWITCH to {} after this syscall blocks.  Reprioritizing at time {}",
                        &mytid, next_tid, time
                    );
                } else {
                    info!(
                        "[detcore, dtid {}] CONTEXT SWITCH to {} after the last event retired.  Reprioritizing at time {}",
                        &mytid, next_tid, time
                    );
                }

                // The *downgrading* of the current thread will be handled by the caller if the
                // context switch is *now*.  If the last traced event on this thread is instead
                // a prehook, well we don't deschedule the current thread quite yet.  Rather, we
                // let the thread plow ahead, and actually block on the syscall, which will have
                // the effect of descheduling the therad anyway.  After that, the priorities
                // be set so as to make sure the correct thread (next_tid) runs.
                //
                // Note: for *natural* schedules that are recorded from hermit, the only reason
                // to switch away from the thread after the prehook is if it blocked. But a
                // synthetic schedule can choose to do this arbitrarily, and we need to assign
                // semantics to that on replay.  If we have threads A&B, and events "A_prehook,
                // B_*, A_posthook", then does the effect of A's syscall happen before or after
                // B's logic?
                ReplayAction::ContextSwitch(!is_prehook, next_tid, timeslice)
            } else {
                // We're still running the next event.
                ReplayAction::Continue(timeslice)
            }
        } else {
            ReplayAction::Continue(None)
        }
    }
}

#[cfg(test)]
mod builder {
    use detcore_model::pid::DetTid;
    use detcore_model::schedule::InstructionPointer;
    use detcore_model::schedule::Op;
    use detcore_model::schedule::SchedEvent;
    use detcore_model::schedule::SyscallPhase;
    use detcore_model::time::DetTime;
    use reverie::syscalls::Sysno;

    pub struct SchedTestBuilder {
        time: DetTime,
    }

    impl SchedTestBuilder {
        pub fn syscall_pre(&mut self, sys_no: Sysno, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_rip: InstructionPointer::new(end_rip),
                end_time: Some(self.time.as_nanos()),
                op: Op::Syscall(sys_no, SyscallPhase::Prehook),
            }
        }

        pub fn syscall_post(&mut self, sys_no: Sysno, end_rip: usize, tid: DetTid) -> SchedEvent {
            self.time.add_syscall();
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_rip: InstructionPointer::new(end_rip),
                end_time: Some(self.time.as_nanos()),
                op: Op::Syscall(sys_no, SyscallPhase::Posthook),
            }
        }

        pub fn other(&mut self, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_time: Some(self.time.as_nanos()),
                op: Op::OtherInstructions,
                end_rip: InstructionPointer::new(end_rip),
            }
        }

        pub fn branch(&mut self, count: u32, tid: DetTid) -> SchedEvent {
            self.time.add_rcbs(count as u64);
            SchedEvent {
                count,
                dettid: tid,
                end_rip: None,
                end_time: Some(self.time.as_nanos()),
                op: Op::Branch,
                start_rip: None,
            }
        }

        pub fn new() -> Self {
            Self {
                time: Default::default(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::InstructionPointer;
    use pretty_assertions::assert_eq;
    use reverie::syscalls::Sysno;

    use super::*;

    const SOME_RIP: usize = 1234566;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    fn strip_times(vec: Vec<SchedEvent>) -> Vec<SchedEvent> {
        vec.iter()
            .map(|se| SchedEvent {
                end_time: Some(LogicalTime::from_nanos(0)),
                ..*se
            })
            .collect()
    }

    /// A fatal desync names the fields that differed: an event that matches
    /// except for its `end_rip` (a schedule whose syscall events hold a
    /// different return address) names exactly `end_rip`.
    #[test]
    fn a_hard_desync_names_the_fields_that_differ() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.other(SOME_RIP, tid(3));
        let observed = SchedEvent {
            end_rip: std::num::NonZeroUsize::new(SOME_RIP + 2),
            ..recorded
        };
        assert!(is_hard_desync(&observed, &recorded));
        assert_eq!(hard_desync_fields(&observed, &recorded), vec!["end_rip"]);
        let other_thread = SchedEvent {
            dettid: tid(4),
            ..observed
        };
        assert_eq!(
            hard_desync_fields(&other_thread, &recorded),
            vec!["dettid", "end_rip"]
        );
        assert!(hard_desync_fields(&recorded, &recorded).is_empty());
    }

    /// Stack-trace positions follow the recorded schedule: an extra observed
    /// event neither takes a position nor shifts the later ones, and positions
    /// continue past the end of the schedule.
    #[test]
    fn stacktrace_positions_follow_the_schedule_and_continue_past_its_end() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.syscall_pre(Sysno::read, SOME_RIP, tid(3)),
            b.syscall_post(Sysno::read, SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::write, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());

        let _ = replayer.observe_event(&schedule[0]);
        assert_eq!(replayer.stacktrace_position(0), Some(0));
        // An event the schedule does not have, such as a split timeslice.
        let extra = b.branch(2, tid(3));
        let _ = replayer.observe_event(&extra);
        assert_eq!(replayer.stacktrace_position(1), None);
        let _ = replayer.observe_event(&schedule[1]);
        assert_eq!(replayer.stacktrace_position(2), Some(1));
        let _ = replayer.observe_event(&schedule[2]);
        assert_eq!(replayer.stacktrace_position(3), Some(2));

        // Past the end, positions continue from the schedule's length.
        let after = b.syscall_post(Sysno::write, SOME_RIP, tid(3));
        let _ = replayer.observe_event(&after);
        assert_eq!(replayer.stacktrace_position(4), Some(3));
        let _ = replayer.observe_event(&b.branch(9, tid(5)));
        assert_eq!(replayer.stacktrace_position(5), Some(4));
    }

    #[test]
    fn an_observed_rip_the_trace_lacks_is_not_a_hard_desync() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = SchedEvent {
            end_rip: None,
            ..b.branch(12203, tid(59))
        };
        let replayed = SchedEvent {
            end_rip: Some(InstructionPointer::new(0x7fff_f7c9_6c68).unwrap()),
            end_time: Some(LogicalTime::from_nanos(150)),
            ..recorded.clone()
        };
        assert!(
            !is_hard_desync(&replayed, &recorded),
            "the trace leaves the RIP open"
        );
        assert!(is_desync(&replayed, &recorded), "the time still differs");

        let other_rip = SchedEvent {
            end_rip: Some(InstructionPointer::new(0x1000).unwrap()),
            ..recorded.clone()
        };
        let recorded_with_rip = SchedEvent {
            end_rip: Some(InstructionPointer::new(0x2000).unwrap()),
            ..recorded
        };
        assert!(
            is_hard_desync(&other_rip, &recorded_with_rip),
            "a RIP the trace names must match"
        );
    }

    #[test]
    fn a_blocked_syscall_retry_is_recognized_only_for_its_thread() {
        let mut b = builder::SchedTestBuilder::new();
        let pre = b.syscall_pre(Sysno::read, SOME_RIP, tid(3));
        let polling = SchedEvent {
            op: Op::Syscall(Sysno::read, SyscallPhase::Polling),
            ..b.syscall_pre(Sysno::read, SOME_RIP, tid(3))
        };
        let schedule = vec![
            pre,
            polling,
            b.syscall_post(Sysno::read, SOME_RIP, tid(3)),
            b.branch(5, tid(5)),
        ];
        let mut replayer = Replayer::new(schedule.clone());

        let _ = replayer.observe_event(&schedule[0]);
        assert!(
            replayer.next_is_retry_by(tid(3)),
            "thread 3 polls its read next"
        );
        assert!(
            !replayer.next_is_retry_by(tid(5)),
            "thread 5 has nothing to retry"
        );
        let _ = replayer.observe_event(&schedule[1]);
        assert!(
            replayer.next_is_retry_by(tid(3)),
            "thread 3 completes its read next"
        );
        let _ = replayer.observe_event(&schedule[2]);
        assert!(!replayer.next_is_retry_by(tid(3)), "the read is done");
        assert!(
            !replayer.next_is_retry_by(tid(5)),
            "a branch is not a retry"
        );
    }

    #[test]
    fn test_continue_() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![b.branch(10, tid(3))];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(None)
        );
    }

    /// A timer preemption the thread runs on from (its branches followed by
    /// more of its branches) is replayed with a stop on the branch count.
    #[test]
    fn test_respect_branch_count() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)), //current instruction
            b.branch(123, tid(3)),     // next to respect branches
            b.branch(1, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    /// A timer preemption that switched threads (its branches followed by
    /// another thread's event) is replayed with a stop on the branch count.
    #[test]
    fn test_respect_branch_context_switch() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)), //current instruction
            b.branch(123, tid(3)),     // next to respect branches
            b.syscall_post(Sysno::exit, SOME_RIP, tid(5)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    /// A timer stop the recording gave its RIP keeps its stop when the thread
    /// was picked again and reached its next check-in after no further
    /// branch: its OtherInstructions follows directly.
    #[test]
    fn a_timer_stop_with_its_rip_keeps_its_stop_before_the_threads_check_in() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)),
            SchedEvent {
                end_rip: detcore_model::schedule::InstructionPointer::new(SOME_RIP),
                ..b.branch(123, tid(3))
            },
            b.other(SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::mmap, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    /// Branches the recording followed with the thread's OtherInstructions
    /// reached a check-in, which the replayed thread reaches on its own: no
    /// stop on their count, whether the schedule switches threads after the
    /// check-in or the thread runs on.
    #[test]
    fn branches_before_a_recorded_check_in_get_no_stop() {
        for switches in [true, false] {
            let mut b = builder::SchedTestBuilder::new();
            let mut schedule = vec![
                b.other(SOME_RIP, tid(3)),
                b.branch(123, tid(3)),
                b.other(SOME_RIP, tid(3)),
            ];
            schedule.push(if switches {
                b.syscall_post(Sysno::exit, SOME_RIP, tid(5))
            } else {
                b.branch(1, tid(3))
            });
            let mut replayer = Replayer::new(schedule.clone());
            assert_eq!(
                replayer.observe_event(&schedule[0]),
                ReplayAction::Continue(None)
            );
        }
    }

    #[test]
    fn test_context_switch_pre_hook() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(5)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(true, tid(3), None)
        );
    }

    #[test]
    fn test_context_switch_post_hook() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(5)),
            b.syscall_post(Sysno::exit, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(true, tid(3), None)
        );
    }

    #[test]
    fn test_fast_forward() {
        let mut b = builder::SchedTestBuilder::new();

        // Strip times so we don't have a time mismatch in the branch 123 events:
        let observed = strip_times(vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ]);
        let expected = strip_times(vec![
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ]);
        let mut replayer = Replayer::new(expected);
        let action1 = replayer.observe_event(&observed[0]);
        {
            let counts = replayer.desync_counts.get(&tid(3)).unwrap();
            assert_eq!(
                *counts,
                DesyncStats {
                    soft: 0,
                    hard: 1,
                    at_context_switch: 0,
                    resync_insertions: 0,
                    resync_deletions: 0,
                    fast_forwards: 0,
                    last_was_desync: true,
                }
            );
        }
        assert_eq!(action1, ReplayAction::Continue(None));

        let action2 = replayer.observe_event(&observed[1]);
        {
            let counts = replayer.desync_counts.get(&tid(3)).unwrap();
            assert_eq!(
                *counts,
                DesyncStats {
                    soft: 0,
                    hard: 1,
                    at_context_switch: 0,
                    resync_insertions: 1,
                    resync_deletions: 0,
                    fast_forwards: 0,
                    last_was_desync: false,
                }
            );
        }
        assert_eq!(action2, ReplayAction::Continue(None));
    }
}
