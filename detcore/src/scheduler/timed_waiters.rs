/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use nix::sys::signal::Signal;

use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::OpenFileId;

/// Encapsulate the set of threads that are waiting for a specific time in the future.
///
/// It's possible (but unlikely) that multiple threads are waiting for the same
/// nanosecond, and this structure must break that symmetry.
#[derive(Debug, Clone, Default)]
pub struct TimedEvents {
    // Inner btreeset is *always* non-empty:
    map: BTreeMap<LogicalTime, BTreeSet<TimedEvent>>,

    // Keep one alarm(2)/setitimer(2) event per process and one event per POSIX timer id.
    signal_timers: BTreeMap<SignalTimerId, SignalTimerState>,
    // One pending expiry event per virtual timerfd, keyed by the open file
    // description, never by a descriptor number that close and dup can reuse.
    timerfd_timers: BTreeMap<OpenFileId, TimerFdTimerState>,
    // KVM real timers recur only after an actual shared SIGALRM dequeue.
    kvm_real_deadlines: BTreeMap<DetPid, LogicalTime>,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#869)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SignalTimerId {
    Alarm(DetPid),
    Posix(DetPid, i32),
    /// A deterministic child-exit `SIGCHLD`, synthesized at the child's
    /// scheduler-ordered `Exit` grant (`t_exit`) rather than delivered by the
    /// host-async kernel signal. `child` is the exiting process (a unique
    /// coalescing key so multiple reaped children never collide); `parent` is
    /// the process that receives the signal. Unlike `Alarm`/`Posix`, a
    /// `ChildExit` event is one-shot and is never re-armed or cancelled, so it
    /// is inserted directly into the timed `map` and bypasses the
    /// `signal_timers` re-arm bookkeeping (see `insert_child_exit`).
    ChildExit {
        child: DetPid,
        parent: DetPid,
    },
}

impl SignalTimerId {
    /// The process the timed signal is delivered to. For `ChildExit` this is the
    /// *parent* (the reaper), not the exiting child.
    pub(super) fn process(self) -> DetPid {
        match self {
            Self::Alarm(pid) | Self::Posix(pid, _) => pid,
            Self::ChildExit { parent, .. } => parent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SignalTimerState {
    deadline: LogicalTime,
    interval: LogicalTime,
}

/// Scheduler bookkeeping for one armed virtual timerfd. `owner` is the process
/// that last armed it; its exit removes the fast-forward target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerFdTimerState {
    owner: DetPid,
    deadline: LogicalTime,
    interval: LogicalTime,
}

/// An event that occurs at a particular time in the execution, typically at an offset in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimedEvent {
    // An upcoming timer signal, destined for a process with a preferred target thread.
    SignalEvt(SignalTimerId, DetTid, Signal),

    /// A timed event on a particular thread (sleep, timeout, etc)
    ThreadEvt(DetTid),

    /// A virtual timerfd expiry for one open file description. Declared last so the
    /// canonical same-deadline pop order is SignalEvt, ThreadEvt, TimerFdExpiry.
    /// Popping wakes no thread: guest-visible readiness is a pure function of
    /// virtual time computed by detcore; this event exists so the deadline is
    /// a fast-forward target and periodic re-arm has a scheduler owner.
    TimerFdExpiry(OpenFileId),
}

impl fmt::Display for TimedEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TimedEvent::ThreadEvt(dt) => write!(f, "ThreadEvt({})", dt),
            TimedEvent::SignalEvt(id, dt, sig) => {
                write!(f, "SignalEvt({:?},{},{})", id, dt, sig)
            }
            TimedEvent::TimerFdExpiry(id) => write!(f, "TimerFdExpiry({:?})", id),
        }
    }
}

impl TimedEvents {
    pub fn insert(&mut self, ns: LogicalTime, dt: DetTid) {
        let set = self.map.entry(ns).or_default();
        if !set.insert(TimedEvent::ThreadEvt(dt)) {
            panic!(
                "TimedEvents::insert should not take a DetTid which is *already* in the set: {}",
                dt
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    fn insert_signal_timer(
        &mut self,
        id: SignalTimerId,
        ns: LogicalTime,
        dt: DetTid,
        sig: Signal,
        interval: LogicalTime,
    ) -> Option<SignalTimerState> {
        let old = self.signal_timers.insert(
            id,
            SignalTimerState {
                deadline: ns,
                interval,
            },
        );
        self.clear_old_signal_timer(id, old);

        let set = self.map.entry(ns).or_default();
        let evt = TimedEvent::SignalEvt(id, dt, sig);
        if !set.insert(evt) {
            panic!(
                "TimedEvents::insert_signal_timer should not insert an event which is already in the set: {}",
                evt
            );
        }
        old
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    // Return the last alarm state for this pid, if any.
    pub fn insert_alarm(
        &mut self,
        ns: LogicalTime,
        dp: DetPid,
        dt: DetTid,
        sig: Signal,
        interval: LogicalTime,
    ) -> Option<(LogicalTime, LogicalTime)> {
        self.insert_signal_timer(SignalTimerId::Alarm(dp), ns, dt, sig, interval)
            .map(|state| (state.deadline, state.interval))
    }

    /// Replace a process-owned KVM deadline without entering legacy recurrence.
    /// Publication is process-owned, and receiver selection is a separate phase;
    /// the task that armed the timer does not own its later delivery.
    pub fn insert_kvm_real_deadline(&mut self, deadline: LogicalTime, pid: DetPid) {
        assert!(!self.signal_timers.contains_key(&SignalTimerId::Alarm(pid)));
        self.remove_kvm_real_deadline(pid);
        self.kvm_real_deadlines.insert(pid, deadline);
        self.map
            .entry(deadline)
            .or_default()
            .insert(TimedEvent::SignalEvt(
                SignalTimerId::Alarm(pid),
                pid,
                Signal::SIGALRM,
            ));
    }

    pub fn remove_kvm_real_deadline(&mut self, pid: DetPid) {
        let old = self
            .kvm_real_deadlines
            .remove(&pid)
            .map(|deadline| SignalTimerState {
                deadline,
                interval: LogicalTime::ZERO,
            });
        self.clear_old_signal_timer(SignalTimerId::Alarm(pid), old);
    }

    pub fn alarm_state(&self, pid: DetPid) -> Option<(LogicalTime, LogicalTime)> {
        self.signal_timers
            .get(&SignalTimerId::Alarm(pid))
            .map(|s| (s.deadline, s.interval))
    }

    pub fn thread_deadline(&self, tid: DetTid) -> Option<LogicalTime> {
        self.map.iter().find_map(|(deadline, events)| {
            events
                .contains(&TimedEvent::ThreadEvt(tid))
                .then_some(*deadline)
        })
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    fn clear_old_signal_timer(&mut self, id: SignalTimerId, old: Option<SignalTimerState>) {
        if let Some(state) = old {
            // The `map` entry may already be gone if the alarm fired (was
            // popped by `pop_if_before`) before being cleared. Clearing an
            // already-fired alarm is a no-op rather than an invariant break.
            let Some(set) = self.map.get_mut(&state.deadline) else {
                return;
            };

            // Could use a drain_filter here, but it is nightly only:
            let mut to_remove = None;
            for evt in set.iter() {
                if matches!(evt, TimedEvent::SignalEvt(evt_id, _, _) if *evt_id == id) {
                    assert!(to_remove.is_none());
                    to_remove = Some(*evt);
                }
            }
            if let Some(evt) = to_remove {
                assert!(set.remove(&evt));
            }

            // Preserve the invariant that `map` never holds an empty set, which
            // `is_empty()` and `iter()` rely on.
            if set.is_empty() {
                self.map.remove(&state.deadline);
            }
        }
    }

    // Return the time of any previous alarm on this process.
    pub fn remove_alarm(&mut self, dp: DetPid) -> Option<(LogicalTime, LogicalTime)> {
        self.remove_signal_timer(SignalTimerId::Alarm(dp))
            .map(|state| (state.deadline, state.interval))
    }

    /// Register a one-shot, deterministic child-exit `SIGCHLD` to be delivered to
    /// `parent` (via thread `parent_tid`) at logical time `ns` (the child's
    /// `Exit` grant time plus a tick). Inserted directly into the timed `map`,
    /// deliberately bypassing the `signal_timers` re-arm/cancel bookkeeping used
    /// by `alarm`/`setitimer`/POSIX timers: a child exit fires exactly once and
    /// is never re-armed or replaced, and its key (`ChildExit{child,parent}`) is
    /// unique per exiting child, so it cannot collide with a concurrent
    /// `Alarm`/`Posix` timer on the same process. If an identical event is
    /// already queued at `ns` (the same child reported twice), the insert is a
    /// no-op — the redundant delivery is coalesced, matching Linux `SIGCHLD`.
    pub fn insert_child_exit(
        &mut self,
        ns: LogicalTime,
        child: DetPid,
        parent: DetPid,
        parent_tid: DetTid,
    ) {
        let evt = TimedEvent::SignalEvt(
            SignalTimerId::ChildExit { child, parent },
            parent_tid,
            Signal::SIGCHLD,
        );
        // BTreeSet::insert returns false on a duplicate; coalescing it is
        // intentional (see doc comment) rather than a panic.
        self.map.entry(ns).or_default().insert(evt);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn insert_posix_timer(
        &mut self,
        ns: LogicalTime,
        dp: DetPid,
        dt: DetTid,
        timer_id: i32,
        sig: Signal,
        interval: LogicalTime,
    ) {
        self.insert_signal_timer(SignalTimerId::Posix(dp, timer_id), ns, dt, sig, interval);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn remove_posix_timer(&mut self, dp: DetPid, timer_id: i32) {
        self.remove_signal_timer(SignalTimerId::Posix(dp, timer_id));
    }

    /// Arm or re-arm a virtual timerfd. Returns the replaced state, mirroring
    /// the signal-timer bookkeeping so re-arm/disarm never leaves a stale event.
    pub fn insert_timerfd(
        &mut self,
        ns: LogicalTime,
        owner: DetPid,
        id: OpenFileId,
        interval: LogicalTime,
    ) -> Option<(LogicalTime, LogicalTime)> {
        let old = self.timerfd_timers.insert(
            id,
            TimerFdTimerState {
                owner,
                deadline: ns,
                interval,
            },
        );
        self.clear_old_timerfd(id, old);
        self.map
            .entry(ns)
            .or_default()
            .insert(TimedEvent::TimerFdExpiry(id));
        old.map(|state| (state.deadline, state.interval))
    }

    /// Disarm a virtual timerfd, or drop it when its open file is released.
    pub fn remove_timerfd(&mut self, id: OpenFileId) -> Option<(LogicalTime, LogicalTime)> {
        let old = self.timerfd_timers.remove(&id);
        self.clear_old_timerfd(id, old);
        old.map(|state| (state.deadline, state.interval))
    }

    fn clear_old_timerfd(&mut self, id: OpenFileId, old: Option<TimerFdTimerState>) {
        if let Some(state) = old {
            let Some(set) = self.map.get_mut(&state.deadline) else {
                return;
            };
            if set.remove(&TimedEvent::TimerFdExpiry(id)) && set.is_empty() {
                self.map.remove(&state.deadline);
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    fn remove_signal_timer(&mut self, id: SignalTimerId) -> Option<SignalTimerState> {
        let old = self.signal_timers.remove(&id);
        self.clear_old_signal_timer(id, old);
        old
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review non-mutating logical alarm lookup.
    pub fn alarm_time(&self, dp: DetPid) -> Option<LogicalTime> {
        self.signal_timers
            .get(&SignalTimerId::Alarm(dp))
            .map(|state| state.deadline)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn remove_process_timers(&mut self, dp: DetPid) {
        self.remove_kvm_real_deadline(dp);
        let ids: Vec<_> = self
            .signal_timers
            .keys()
            .copied()
            .filter(|id| match id {
                SignalTimerId::Alarm(pid) | SignalTimerId::Posix(pid, _) => *pid == dp,
                // `ChildExit` events are never stored in `signal_timers`, so this
                // arm is unreachable in practice; it exists only for exhaustiveness.
                SignalTimerId::ChildExit { .. } => false,
            })
            .collect();
        for id in ids {
            self.remove_signal_timer(id);
        }
        let timerfd_ids: Vec<_> = self
            .timerfd_timers
            .iter()
            .filter(|(_, state)| state.owner == dp)
            .map(|(id, _)| *id)
            .collect();
        for id in timerfd_ids {
            self.remove_timerfd(id);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return the next event if its target time of occurrence is before the supplied time.
    /// Being a "pop", this destructively removes the entry.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn pop_if_before(
        &mut self,
        current_time: LogicalTime,
    ) -> Option<(LogicalTime, TimedEvent)> {
        let (time_ns, evt) = if let Some(mut entry) = self.map.first_entry() {
            let time_ns = *entry.key();
            if time_ns <= current_time {
                let set = entry.get_mut();
                let evt = set.pop_first().expect("inner set cannot be empty");
                if set.is_empty() {
                    entry.remove();
                }
                Some((time_ns, evt))
            } else {
                None
            }
        } else {
            None
        }?;

        if let TimedEvent::SignalEvt(SignalTimerId::Alarm(pid), _, _) = evt
            && self.kvm_real_deadlines.get(&pid) == Some(&time_ns)
        {
            self.kvm_real_deadlines.remove(&pid);
            return Some((time_ns, evt));
        }
        if let TimedEvent::SignalEvt(id, _, _) = evt
            && self
                .signal_timers
                .get(&id)
                .is_some_and(|state| state.deadline == time_ns)
        {
            let state = self.signal_timers.get_mut(&id).unwrap();
            if state.interval == LogicalTime::ZERO {
                self.signal_timers.remove(&id);
            } else {
                state.deadline = time_ns + state.interval;
                let next_deadline = state.deadline;
                self.map.entry(next_deadline).or_default().insert(evt);
            }
        }
        if let TimedEvent::TimerFdExpiry(key) = evt
            && self
                .timerfd_timers
                .get(&key)
                .is_some_and(|state| state.deadline == time_ns)
        {
            let interval = self.timerfd_timers[&key].interval;
            if interval == LogicalTime::ZERO {
                self.timerfd_timers.remove(&key);
            } else {
                // Coalesced re-arm: jump to the first deadline after
                // current_time in one step, however many intervals elapsed.
                // An indefinite current_time is the unconditional `pop()`
                // sentinel, not a real instant: the caller has just advanced
                // the clock to `time_ns`, so re-arm exactly one interval
                // (evaluating the coalesce arithmetic at MAX overflows u64).
                let mut next = time_ns + interval;
                if next <= current_time && !current_time.is_indefinite() {
                    let missed = (current_time.as_nanos() - next.as_nanos()) / interval.as_nanos();
                    next = LogicalTime::from_nanos(
                        (next.as_nanos() as u128
                            + (missed as u128 + 1) * interval.as_nanos() as u128)
                            .min(u64::MAX as u128) as u64,
                    );
                }
                self.timerfd_timers.get_mut(&key).unwrap().deadline = next;
                self.map.entry(next).or_default().insert(evt);
            }
        }
        Some((time_ns, evt))
    }

    /// Pop the next event unconditionally, if available.
    pub fn pop(&mut self) -> Option<(LogicalTime, TimedEvent)> {
        self.pop_if_before(LogicalTime::MAX)
    }

    /// The target time of the earliest pending event, without removing it.
    ///
    /// Callers that decide whether virtual time may be fast-forwarded need to
    /// inspect the deadline *before* committing to popping it; a
    /// [`LogicalTime::is_indefinite`] deadline is not a real deadline.
    pub fn next_deadline(&self) -> Option<LogicalTime> {
        self.map.first_key_value().map(|(time_ns, _)| *time_ns)
    }

    /// Are there no timed events waiting?
    pub fn is_empty(&self) -> bool {
        // Here we rely on the invariant that there are no entries with empty sets on the RHS:
        self.map.is_empty()
    }

    /// Remove a specific thread from the set of those waiting on time to elapse.
    pub fn remove(&mut self, dettid: DetTid) {
        let mut to_remove: Option<LogicalTime> = None;
        let mut already_removed = false;
        for (time_key, set) in self.map.iter_mut() {
            let removed = set.remove(&TimedEvent::ThreadEvt(dettid));
            if removed {
                if already_removed {
                    panic!(
                        "invariant violation: multiple entries for dtid {} in TimedEvents",
                        dettid
                    );
                } else {
                    already_removed = true;
                }
            }
            // Cannot allow empty sets to remain:
            if set.is_empty() {
                to_remove = Some(*time_key);
            }
        }
        if let Some(time) = to_remove {
            let _ = self.map.remove(&time);
        }
    }

    /// Iterate over the entries in the TimedEvents collection
    pub fn iter(&self) -> impl Iterator<Item = (LogicalTime, TimedEvent)> + '_ {
        self.map
            .iter()
            .flat_map(|(key, set)| set.iter().map(|dtid| (*key, *dtid)))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn pid(n: i32) -> DetPid {
        DetPid::from_raw(n)
    }
    fn tid(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }
    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }
    fn tfd(sequence: u64) -> OpenFileId {
        OpenFileId::new(tid(100), sequence)
    }

    /// `next_deadline` must report the earliest pending deadline without
    /// consuming it: the scheduler inspects it to decide whether fast-forwarding
    /// virtual time is legitimate at all, and an indefinite front entry must stay
    /// parked rather than be popped.
    #[test]
    fn next_deadline_peeks_without_removing() {
        let mut ev = TimedEvents::default();
        assert_eq!(ev.next_deadline(), None);

        ev.insert(LogicalTime::INDEFINITE, tid(101));
        assert_eq!(ev.next_deadline(), Some(LogicalTime::INDEFINITE));
        assert!(ev.next_deadline().expect("nonempty").is_indefinite());

        // A real deadline sorts ahead of the indefinite sentinel.
        ev.insert(at(1000), tid(100));
        assert_eq!(ev.next_deadline(), Some(at(1000)));
        assert!(!ev.next_deadline().expect("nonempty").is_indefinite());

        // Peeking is non-destructive: both entries are still queued.
        assert_eq!(ev.iter().count(), 2);
        assert_eq!(ev.pop(), Some((at(1000), TimedEvent::ThreadEvt(tid(100)))));
        assert_eq!(ev.next_deadline(), Some(LogicalTime::INDEFINITE));
    }

    /// Regression: an alarm that fires (is popped) must clear its timer
    /// bookkeeping so a subsequent alarm for the same process does not panic in
    /// `clear_old_alarm`. This reproduces the openssl-speed crash, where a
    /// SIGALRM fires and then the next timing round arms another alarm.
    #[test]
    fn reregister_after_fire_does_not_panic() {
        let mut ev = TimedEvents::default();
        let p = pid(100);

        assert_eq!(
            ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            None
        );

        // The alarm fires: the scheduler pops the due event.
        assert_eq!(
            ev.pop(),
            Some((
                at(1000),
                TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM),
            ))
        );
        assert!(ev.is_empty());

        // Arming a new alarm must see no stale previous alarm (the old one has
        // already fired) and must not panic.
        assert_eq!(
            ev.insert_alarm(at(2000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            None
        );
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn removing_alarm_preserves_other_process_at_same_deadline() {
        let mut ev = TimedEvents::default();
        let first_pid = pid(100);
        let second_pid = pid(200);
        let deadline = at(1_000);

        assert_eq!(
            ev.insert_alarm(
                deadline,
                first_pid,
                tid(101),
                Signal::SIGALRM,
                LogicalTime::ZERO,
            ),
            None
        );
        assert_eq!(
            ev.insert_alarm(
                deadline,
                second_pid,
                tid(201),
                Signal::SIGALRM,
                LogicalTime::ZERO,
            ),
            None
        );

        assert_eq!(
            ev.remove_alarm(first_pid),
            Some((deadline, LogicalTime::ZERO))
        );
        assert_eq!(
            ev.iter().collect::<Vec<_>>(),
            vec![(
                deadline,
                TimedEvent::SignalEvt(SignalTimerId::Alarm(second_pid), tid(201), Signal::SIGALRM,)
            )]
        );
        assert_eq!(
            ev.remove_alarm(second_pid),
            Some((deadline, LogicalTime::ZERO))
        );
        assert!(ev.is_empty());
    }

    /// Cancelling (`alarm(0)`) after a fire must be a no-op, not a panic.
    #[test]
    fn cancel_after_fire_does_not_panic() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO);
        let _ = ev.pop(); // fire
        assert_eq!(ev.remove_alarm(p), None);
        assert!(ev.is_empty());
    }

    /// Replacing a still-pending alarm reports the old target time and must not
    /// leave an empty set behind in `map` (which would break `is_empty()`).
    #[test]
    fn replace_pending_alarm_reports_old_and_leaves_no_empty_sets() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        assert_eq!(
            ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            None
        );
        assert_eq!(
            ev.insert_alarm(at(2000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            Some((at(1000), LogicalTime::ZERO))
        );
        // Only the replacement remains; the emptied 1000ns slot is gone.
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev.pop(),
            Some((
                at(2000),
                TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM),
            ))
        );
        assert!(ev.is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    #[test]
    fn periodic_alarm_rearms_at_its_interval() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        let event = TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM);
        ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, at(250));

        assert_eq!(ev.pop_if_before(at(1000)), Some((at(1000), event)));
        assert_eq!(ev.pop_if_before(at(1249)), None);
        assert_eq!(ev.pop_if_before(at(1250)), Some((at(1250), event)));
        assert_eq!(ev.remove_alarm(p), Some((at(1500), at(250))));
        assert!(ev.is_empty());
    }

    /// A deterministic child-exit `SIGCHLD` must coexist with an `alarm(2)` on the
    /// *same* process at the *same* deadline without hitting the
    /// `insert_signal_timer` "already in set" panic (the collision that motivated
    /// the distinct `ChildExit` key), and must be delivered to the parent.
    #[test]
    fn child_exit_coexists_with_process_alarm_at_same_deadline() {
        let mut ev = TimedEvents::default();
        let parent = pid(100);
        let child = pid(200);
        let deadline = at(1_000);

        // Parent has an armed alarm...
        ev.insert_alarm(
            deadline,
            parent,
            tid(100),
            Signal::SIGALRM,
            LogicalTime::ZERO,
        );
        // ...and simultaneously reaps a child at the same logical time.
        ev.insert_child_exit(deadline, child, parent, tid(100));

        // Both are queued (no panic); popping yields the SIGALRM and the SIGCHLD.
        let first = ev.pop().expect("first event");
        let second = ev.pop().expect("second event");
        let popped = [first.1, second.1];
        assert!(popped.contains(&TimedEvent::SignalEvt(
            SignalTimerId::Alarm(parent),
            tid(100),
            Signal::SIGALRM
        )));
        assert!(popped.contains(&TimedEvent::SignalEvt(
            SignalTimerId::ChildExit { child, parent },
            tid(100),
            Signal::SIGCHLD
        )));
        assert!(ev.is_empty());

        // The alarm bookkeeping is untouched by the child-exit event: re-arming
        // sees no stale state (the fired alarm cleared itself) and does not panic.
        assert_eq!(
            ev.insert_alarm(
                at(2_000),
                parent,
                tid(100),
                Signal::SIGALRM,
                LogicalTime::ZERO
            ),
            None
        );
    }

    /// Two reports of the same child exit at the same deadline coalesce to a
    /// single `SIGCHLD`, matching Linux non-RT signal semantics; distinct
    /// children produce distinct events.
    #[test]
    fn child_exit_coalesces_duplicate_and_keeps_distinct_children() {
        let mut ev = TimedEvents::default();
        let parent = pid(100);
        let deadline = at(1_000);

        ev.insert_child_exit(deadline, pid(200), parent, tid(100));
        ev.insert_child_exit(deadline, pid(200), parent, tid(100)); // duplicate: coalesced
        ev.insert_child_exit(deadline, pid(201), parent, tid(100)); // distinct child

        assert_eq!(ev.iter().count(), 2);
        assert!(ev.iter().any(|(_, e)| e
            == TimedEvent::SignalEvt(
                SignalTimerId::ChildExit {
                    child: pid(200),
                    parent
                },
                tid(100),
                Signal::SIGCHLD
            )));
        assert!(ev.iter().any(|(_, e)| e
            == TimedEvent::SignalEvt(
                SignalTimerId::ChildExit {
                    child: pid(201),
                    parent
                },
                tid(100),
                Signal::SIGCHLD
            )));
    }

    /// Same-deadline canonical order: SignalEvt, ThreadEvt, TimerFdExpiry.
    #[test]
    fn timerfd_sorts_after_signal_and_thread_at_same_deadline() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_timerfd(at(500), p, tfd(9), LogicalTime::ZERO);
        ev.insert(at(500), tid(100));
        ev.insert_alarm(at(500), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO);
        assert_eq!(
            ev.pop(),
            Some((
                at(500),
                TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM)
            ))
        );
        assert_eq!(ev.pop(), Some((at(500), TimedEvent::ThreadEvt(tid(100)))));
        assert_eq!(ev.pop(), Some((at(500), TimedEvent::TimerFdExpiry(tfd(9)))));
        assert!(ev.is_empty());
    }

    /// Re-arm replaces the pending event; disarm and process cleanup remove it.
    #[test]
    fn timerfd_rearm_disarm_and_process_cleanup() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_timerfd(at(500), p, tfd(9), LogicalTime::ZERO);
        assert_eq!(
            ev.insert_timerfd(at(900), p, tfd(9), LogicalTime::ZERO),
            Some((at(500), LogicalTime::ZERO))
        );
        assert_eq!(ev.next_deadline(), Some(at(900)));
        assert_eq!(
            ev.remove_timerfd(tfd(9)),
            Some((at(900), LogicalTime::ZERO))
        );
        assert!(ev.is_empty());
        ev.insert_timerfd(at(700), p, tfd(9), LogicalTime::ZERO);
        ev.remove_process_timers(p);
        assert!(ev.is_empty());
    }

    /// Periodic re-arm coalesces: popping far past multiple intervals inserts
    /// exactly one future event at the first deadline after `now`.
    #[test]
    fn timerfd_periodic_rearm_coalesces() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_timerfd(at(100), p, tfd(9), at(50));
        let (t, evt) = ev.pop_if_before(at(430)).expect("pops");
        assert_eq!((t, evt), (at(100), TimedEvent::TimerFdExpiry(tfd(9))));
        // Next deadline is 450 (first > 430), not 150/200/.../400.
        assert_eq!(ev.next_deadline(), Some(at(450)));
        assert_eq!(ev.iter().count(), 1);
    }

    /// Unconditional `pop()` (the empty-queue fast-forward path) on a periodic
    /// timerfd re-arms exactly one interval. Regression: `pop()` is
    /// `pop_if_before(LogicalTime::MAX)`, and the coalesced re-arm used to
    /// evaluate its missed-interval arithmetic at `MAX`, overflowing u64 and
    /// panicking the scheduler — a periodic timerfd plus a sleeping thread
    /// (empty run queue) hung the guest.
    #[test]
    fn timerfd_periodic_pop_unconditional_rearms_one_interval() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_timerfd(at(100), p, tfd(9), at(50));
        assert_eq!(ev.pop(), Some((at(100), TimedEvent::TimerFdExpiry(tfd(9)))));
        assert_eq!(ev.next_deadline(), Some(at(150)));
        assert_eq!(ev.iter().count(), 1);
        assert_eq!(ev.pop(), Some((at(150), TimedEvent::TimerFdExpiry(tfd(9)))));
        assert_eq!(ev.next_deadline(), Some(at(200)));
    }

    /// Keying by open file description: two timerfds of one process that
    /// happen to reuse a descriptor number stay distinct, and removing one
    /// (its open file was released) never disturbs the other or another
    /// process's timer.
    #[test]
    fn timerfd_keyed_by_open_file_not_descriptor_number() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        let q = pid(200);
        ev.insert_timerfd(at(500), p, tfd(1), LogicalTime::ZERO);
        ev.insert_timerfd(at(600), p, tfd(2), LogicalTime::ZERO);
        ev.insert_timerfd(at(700), q, tfd(3), at(10));
        assert_eq!(
            ev.remove_timerfd(tfd(1)),
            Some((at(500), LogicalTime::ZERO))
        );
        assert_eq!(ev.next_deadline(), Some(at(600)));
        assert_eq!(ev.remove_timerfd(tfd(1)), None);
        ev.remove_process_timers(p);
        assert_eq!(ev.next_deadline(), Some(at(700)));
        assert_eq!(ev.iter().count(), 1);
        ev.remove_process_timers(q);
        assert!(ev.is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    #[test]
    fn posix_timer_does_not_replace_process_alarm() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO);
        ev.insert_posix_timer(at(500), p, tid(100), 7, Signal::SIGUSR1, LogicalTime::ZERO);

        assert_eq!(
            ev.pop(),
            Some((
                at(500),
                TimedEvent::SignalEvt(SignalTimerId::Posix(p, 7), tid(100), Signal::SIGUSR1,),
            ))
        );
        assert_eq!(ev.remove_alarm(p), Some((at(1000), LogicalTime::ZERO)));
        assert!(ev.is_empty());
    }
}
