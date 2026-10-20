/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The virtual-completion exit order (study branch, plan step 3): off by
//! default, enabled by `HERMIT_PARALLEL_EXIT_ORDER=1`. It changes the serial
//! schedule, deterministically, so that `make -jN` collects its children in
//! the order they would finish in a parallel run. See the dev-hermit proposal
//! `ai_docs/2026-10-19-parallelism-steps-2-3-proposal.md` (section "Step 3
//! liveness") for the rules and the proof that every held exit is released.
//!
//! A child process's exit is granted in order of a per-process virtual
//! completion clock, with the process ID as the tie-break. A process's clock
//! is the parent's clock when the process was first seen plus the work of its
//! threads since then (deterministic logical CPU time: retired branches,
//! trapped instructions and syscall costs, without the jumps a thread's clock
//! takes when it is advanced to a deadline), or the clock of a child whose
//! exit was granted, if larger (a Lamport merge at reap). The hold rules are
//! `hold_reason`'s. When an exit is granted, a wait parent that polls
//! `wait4`/`waitid` (`Resources::polls_children`, a typed marker) is moved to
//! the front of the run queue at the next step 2, so it reaps before the next
//! sibling exit commits.
//!
//! Every input is committed scheduler state, so the decisions are the same in
//! every run. Under `hermit record` each hold and release is logged
//! (`exit_order_log`), and `hermit replay` follows the log, refusing a logged
//! point it has passed, as it does for readmissions (`rejoin_log`).

use std::collections::VecDeque;
use std::sync::OnceLock;

use super::AdmitIntent;
use super::AdmitSide;
use super::Scheduler;
use super::exit_order_log;
use super::exit_order_log::ExitOrderEntry;
use super::exit_order_log::ExitOrderEvent;
use super::exit_order_log::ExitOrderWriter;
use super::rejoin_log::RejoinRefusal;
use crate::config::Config;
use crate::types::DetPid;
use crate::types::DetTid;

/// The environment variable enabling the virtual-completion exit order.
pub const EXIT_ORDER_ENV: &str = "HERMIT_PARALLEL_EXIT_ORDER";

fn env_on(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v == "1")
}

/// Whether the virtual-completion exit order is on.
pub fn exit_order() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| env_on(EXIT_ORDER_ENV))
}

/// The exit-order log of a run: written under `hermit record`, followed under
/// `hermit replay`. A replay refuses to start when the recording was made with
/// the exit order and this run is not, or the other way round: the schedules
/// would part at the first held exit.
pub(super) fn open_log(
    cfg: &Config,
) -> (Option<ExitOrderWriter>, Option<VecDeque<ExitOrderEntry>>) {
    let Some(dir) = cfg.replay_data.as_ref().filter(|_| cfg.recordreplay_modes) else {
        return (None, None);
    };
    let refuse =
        |reason: String| -> ! { super::exit_on_scheduler_refusal(&RejoinRefusal { reason }) };
    if !cfg.replaying {
        if !exit_order() {
            return (None, None);
        }
        let writer = ExitOrderWriter::create(dir).unwrap_or_else(|err| {
            panic!(
                "cannot create the exit-order log in {}: {err}",
                dir.display()
            )
        });
        return (Some(writer), None);
    }
    match exit_order_log::read(dir) {
        Ok(Some(entries)) if exit_order() => (None, Some(entries)),
        Ok(None) if !exit_order() => (None, None),
        Ok(Some(_)) => refuse(format!(
            "the recording was made with {EXIT_ORDER_ENV}=1 and this replay runs without it"
        )),
        Ok(None) => refuse(format!(
            "this replay runs with {EXIT_ORDER_ENV}=1 and the recording was made without it"
        )),
        Err(err) => refuse(format!(
            "the recording's exit-order log cannot be read ({err})"
        )),
    }
}

/// Recursion bound for subtree walks; process trees in builds are shallow.
const MAX_DEPTH: u32 = 64;

/// Scheduler-side state of the exit-order experiment.
#[derive(Debug, Default)]
pub struct ExitOrderState {
    /// Each thread's logical CPU time when first seen.
    work_baseline: std::collections::HashMap<DetTid, u64>,
    /// Each thread's logical CPU time since it was first seen.
    work: std::collections::HashMap<DetTid, u64>,
    /// Each process's clock inherited from its parent when first seen.
    inherited: std::collections::HashMap<DetPid, u64>,
    /// The largest clock among each process's children whose exits were
    /// granted.
    reaped: std::collections::HashMap<DetPid, u64>,
    /// Threads whose exit this experiment holds (also parked in
    /// `parked.exits_awaiting_permit`, which gives them the treatment of an
    /// exiting thread elsewhere in the scheduler).
    held: std::collections::BTreeSet<DetTid>,
    /// The key each held exit was held with. It does not move while the exit
    /// is held: the other threads of an `exit_group` keep running until it is
    /// granted, where Linux would already have killed them, and their work
    /// must not raise the key the hold rules compare against (assumption A3
    /// of the liveness proof).
    held_keys: std::collections::HashMap<DetTid, (u64, DetPid)>,
    /// Wait parents of processes whose exit was just granted; their polled
    /// `wait4`/`waitid` is handed the next turn at the next step 2.
    wake_parents: std::collections::BTreeSet<DetPid>,
    /// Whether the exit order is on for this run (`exit_order()`).
    pub(super) enabled: bool,
    /// `hermit record`: where each hold and release is logged.
    pub(super) log_writer: Option<ExitOrderWriter>,
    /// `hermit replay`: the recording's holds and releases still to apply.
    pub(super) log_replay: Option<VecDeque<ExitOrderEntry>>,
}

impl ExitOrderState {
    /// Empty state, with the run's exit-order log (`open_log`).
    pub(super) fn new(cfg: &Config) -> Self {
        let (log_writer, log_replay) = open_log(cfg);
        ExitOrderState {
            enabled: exit_order(),
            log_writer,
            log_replay,
            ..Default::default()
        }
    }
}

/// Why the virtual-completion exit order holds an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HoldReason {
    /// H1: the parent is queued and has a sibling's granted exit to reap
    /// first (one reapable child at a time).
    ReapFirst,
    /// H2: the parent is doing work and has not reached this exit's virtual
    /// time: in a parallel run it would act first.
    ParentAhead,
    /// H3: a sibling with a smaller lower bound can still run and may finish
    /// first.
    EarlierSibling,
}

/// The facts an exit-order hold is decided from (see
/// `Scheduler::exit_order_hold_inputs`).
#[derive(Clone, Copy, Debug)]
pub(super) struct HoldInputs {
    /// The exiting process's virtual clock and ID, which orders exits.
    pub key: (u64, DetPid),
    /// Whether the process is a vfork child its parent waits on.
    pub vfork_child: bool,
    /// The parent's virtual clock.
    pub parent_clock: u64,
    /// Whether a thread of the parent is queued to run.
    pub parent_queued: bool,
    /// Whether a thread of the parent is queued other than as a backed-off
    /// poller.
    pub parent_working: bool,
    /// Whether a sibling's exit was granted and the parent has not reaped it.
    pub granted_unreaped_sibling: bool,
    /// Whether a sibling whose exit is not granted has a smaller
    /// (lower bound, ID) than `key` and a runnable live subtree.
    pub earlier_runnable_sibling: bool,
}

/// Whether, and why, an exit with `inputs` is held.
///
/// Liveness (the steps 2-3 proposal, "Step 3 liveness"): every reason waits
/// for another party that can still act and whose virtual clock is below the
/// exit's, and lapses once that clock reaches it. A held exit's own clock
/// does not move, every committed turn raises its thread's clock, and a new
/// process starts at its creator's clock, so the parties' budget below the
/// key is finite and every held exit is released.
pub(super) fn hold_reason(inputs: &HoldInputs) -> Option<HoldReason> {
    if inputs.vfork_child {
        return None;
    }
    let parent_behind = inputs.parent_clock < inputs.key.0;
    // The clock guard bounds H1: a parent that keeps working without reaping
    // has, past this exit's virtual time, both exits to reap in a parallel
    // run too.
    if inputs.parent_queued && inputs.granted_unreaped_sibling && parent_behind {
        return Some(HoldReason::ReapFirst);
    }
    if inputs.parent_working && parent_behind {
        return Some(HoldReason::ParentAhead);
    }
    if inputs.earlier_runnable_sibling {
        return Some(HoldReason::EarlierSibling);
    }
    None
}

impl Scheduler {
    /// Record the logical CPU time `tid` carried with its latest request.
    pub(crate) fn exit_order_note_work(&mut self, tid: DetTid, cpu: u64) {
        if !self.exit_order.enabled {
            return;
        }
        let baseline = *self.exit_order.work_baseline.entry(tid).or_insert(cpu);
        self.exit_order
            .work
            .insert(tid, cpu.saturating_sub(baseline));
        if let Some(process) = self.thread_tree.process_of(tid)
            && !self.exit_order.inherited.contains_key(&process)
        {
            let inherited = self
                .thread_tree
                .parent_process(&process)
                .map_or(0, |parent| self.exit_order_clock(parent));
            self.exit_order.inherited.insert(process, inherited);
        }
    }

    /// The effective virtual clock of `process`.
    fn exit_order_clock(&self, process: DetPid) -> u64 {
        let state = &self.exit_order;
        let mut work = 0;
        if self.thread_tree.contains(&process) {
            for thread in self.thread_tree.my_thread_group(&process) {
                work += state.work.get(&thread).copied().unwrap_or(0);
            }
        }
        let own = state.inherited.get(&process).copied().unwrap_or(0) + work;
        own.max(state.reaped.get(&process).copied().unwrap_or(0))
    }

    /// A lower bound on the clock `process` will exit with: the largest clock
    /// in its live subtree.
    fn exit_order_lower_bound(&self, process: DetPid, depth: u32) -> u64 {
        let mut bound = self.exit_order_clock(process);
        if depth < MAX_DEPTH {
            for child in self.thread_tree.unreaped_children_of(process) {
                if !self.process_exit_granted(child) {
                    bound = bound.max(self.exit_order_lower_bound(child, depth + 1));
                }
            }
        }
        bound
    }

    /// Whether some thread in `process`'s live subtree is queued to run.
    fn exit_order_subtree_runnable(&self, process: DetPid, depth: u32) -> bool {
        if self.thread_tree.contains(&process)
            && self
                .thread_tree
                .my_thread_group(&process)
                .into_iter()
                .any(|thread| {
                    !self.exit_order.held.contains(&thread)
                        && (self.run_queue.contains_non_polling_tid(thread)
                            // Parked until its deferred SIGCHLD is admitted,
                            // which happens once no other work can run: it
                            // counts only then, when it is next to run. While
                            // other work runs its clock does not move, so
                            // counting it would make a held exit wait on
                            // that other work's end (liveness, condition H3).
                            || (self.blocked.sigchld_deferred.contains(&thread)
                                && self.run_queue.only_pollers_or_yielders()))
                })
        {
            return true;
        }
        depth < MAX_DEPTH
            && self
                .thread_tree
                .unreaped_children_of(process)
                .into_iter()
                .any(|child| {
                    !self.process_exit_granted(child)
                        && self.exit_order_subtree_runnable(child, depth + 1)
                })
    }

    /// The process `tid`'s exit would end, if it ends one: an `exit_group`,
    /// or the exit of its process's last live thread.
    fn exit_order_ending_process(&self, tid: DetTid, group: bool) -> Option<DetPid> {
        let process = self.registered_process(tid)?;
        let last = group
            || !self
                .thread_tree
                .my_thread_group(&process)
                .into_iter()
                .any(|other| other != tid && self.next_turns.contains_key(&other));
        last.then_some(process)
    }

    /// Whether `tid`'s exit must wait for a sibling that may finish earlier in
    /// virtual time ([`hold_reason`]).
    fn exit_order_must_wait(&self, tid: DetTid, group: bool) -> bool {
        self.exit_order_hold_inputs(tid, group)
            .is_some_and(|inputs| hold_reason(&inputs).is_some())
    }

    /// The facts [`hold_reason`] decides from, for `tid`'s exit; `None` when
    /// the exit ends no process or the process has no parent.
    fn exit_order_hold_inputs(&self, tid: DetTid, group: bool) -> Option<HoldInputs> {
        let process = self.exit_order_ending_process(tid, group)?;
        let parent = self.thread_tree.parent_process(&process)?;
        let key = self
            .exit_order
            .held_keys
            .get(&tid)
            .copied()
            .unwrap_or_else(|| (self.exit_order_clock(process), process));
        let siblings: Vec<DetPid> = self
            .thread_tree
            .unreaped_children_of(parent)
            .into_iter()
            .filter(|sibling| *sibling != process)
            .collect();
        let parent_threads = if self.thread_tree.contains(&parent) {
            self.thread_tree.my_thread_group(&parent)
        } else {
            Vec::new()
        };
        Some(HoldInputs {
            key,
            // A vfork child's exit releases its blocked parent, and step 3
            // must select a registered vfork child.
            vfork_child: self
                .vfork_barriers
                .values()
                .any(|child| *child == Some(process)),
            parent_clock: self.exit_order_clock(parent),
            parent_queued: parent_threads
                .iter()
                .any(|thread| self.run_queue.contains_tid(*thread)),
            parent_working: parent_threads
                .iter()
                .any(|thread| self.run_queue.contains_non_polling_tid(*thread)),
            granted_unreaped_sibling: siblings
                .iter()
                .any(|sibling| self.process_exit_granted(*sibling)),
            earlier_runnable_sibling: siblings
                .into_iter()
                .filter(|sibling| !self.process_exit_granted(*sibling))
                .any(|sibling| {
                    (self.exit_order_lower_bound(sibling, 0), sibling) < key
                        && self.exit_order_subtree_runnable(sibling, 0)
                }),
        })
    }

    /// Called at an `Exit` request's step 4: hold it if the order requires.
    /// Returns whether the exit is now held. Replay holds exactly the exits
    /// the recording held at this point.
    pub(super) fn exit_order_hold(&mut self, tid: DetTid, group: bool) -> bool {
        if !self.exit_order.enabled {
            return false;
        }
        let hold = match self.exit_order_logged(ExitOrderEvent::Hold) {
            Some(logged) => logged.contains(&tid),
            None => self.exit_order_must_wait(tid, group),
        };
        if !hold {
            return false;
        }
        self.exit_order_log(ExitOrderEvent::Hold, tid);
        self.exit_order_note_held(tid, group);
        self.parked.exits_awaiting_permit.insert(tid);
        true
    }

    /// Replay: pop and return the threads the recording logged for `event` at
    /// this point, or `None` when not replaying. A logged point this run has
    /// already passed refuses the run.
    fn exit_order_logged(&mut self, event: ExitOrderEvent) -> Option<Vec<DetTid>> {
        let point = (self.turn, self.empty_queue_wakes);
        let entries = self.exit_order.log_replay.as_mut()?;
        if let Some(front) = entries.front()
            && front.point() < point
        {
            let reason = format!(
                "the recording's exit order {:?}ed thread {} at turn {} after {} timer wakes, \
                 and replay reached turn {} after {} first",
                front.event,
                front.tid.as_raw(),
                front.turn,
                front.wakes,
                self.turn,
                self.empty_queue_wakes
            );
            if self.rejoin_refusal.is_none() {
                tracing::error!("[exit order] {reason}");
                self.rejoin_refusal = Some(RejoinRefusal { reason });
            }
            return Some(Vec::new());
        }
        let mut due = Vec::new();
        while let Some(front) = entries.front()
            && front.point() == point
            && front.event == event
        {
            due.push(front.tid);
            entries.pop_front();
        }
        Some(due)
    }

    /// Record: log `event` for `tid` at this point.
    fn exit_order_log(&mut self, event: ExitOrderEvent, tid: DetTid) {
        let entry = ExitOrderEntry {
            turn: self.turn,
            wakes: self.empty_queue_wakes,
            event,
            tid,
        };
        if let Some(writer) = self.exit_order.log_writer.as_mut() {
            writer
                .append(entry)
                .unwrap_or_else(|err| panic!("cannot write the exit-order log: {err}"));
        }
    }

    /// Mark `tid`'s exit held, keeping the key it is held with.
    fn exit_order_note_held(&mut self, tid: DetTid, group: bool) {
        if let Some(process) = self.exit_order_ending_process(tid, group) {
            let key = (self.exit_order_clock(process), process);
            self.exit_order.held_keys.entry(tid).or_insert(key);
        }
        self.exit_order.held.insert(tid);
    }

    /// Called when an exit that ends `process` is granted: fold its clock into
    /// its parent's (the reap edge).
    pub(super) fn exit_order_note_granted(&mut self, process: DetPid) {
        if !self.exit_order.enabled {
            return;
        }
        let clock = self.exit_order_clock(process);
        if let Some(parent) = self.thread_tree.parent_process(&process) {
            let reaped = self.exit_order.reaped.entry(parent).or_insert(0);
            *reaped = (*reaped).max(clock);
            self.exit_order.wake_parents.insert(parent);
        }
    }

    /// Step 2: hand the turn to a wait parent polling `wait4`/`waitid` for a
    /// child whose exit was granted, so it reaps that child before another
    /// sibling's exit commits, as it would when the jobs run in parallel.
    /// Returns whether any parent was moved.
    fn exit_order_wake_parents(&mut self) -> bool {
        let parents = std::mem::take(&mut self.exit_order.wake_parents);
        let mut moved = false;
        for parent in parents {
            if !self.thread_tree.contains(&parent) {
                continue;
            }
            for thread in self.thread_tree.my_thread_group(&parent) {
                let Some(Ok(request)) = self
                    .next_turns
                    .get(&thread)
                    .and_then(|turn| turn.req.try_read())
                else {
                    continue;
                };
                let polls_wait = request.polls_children == Some(parent);
                if !polls_wait || !self.run_queue.contains_tid(thread) {
                    continue;
                }
                let _ = self.run_queue.restore_poller_priority(thread);
                if request.poll_attempt > 0 {
                    self.upgrade_polled_to_runnable(thread, &request);
                }
                if self.run_queue.remove_tid(thread) {
                    self.runqueue_push_front(thread);
                    tracing::info!(
                        "[step2] exit order: handing the turn to dettid {}, which polls for its children",
                        thread
                    );
                    moved = true;
                }
            }
        }
        moved
    }

    /// Step 2: release held exits that no longer have to wait, in key order.
    /// Returns whether any was released; the admissions apply at the next
    /// pass's drain, so the caller skips this pass.
    pub(super) fn exit_order_release(&mut self) -> bool {
        if !self.exit_order.enabled {
            return false;
        }
        // Moving a parent applies now; it does not need another pass.
        self.exit_order_wake_parents();
        if self.exit_order.held.is_empty() {
            return false;
        }
        let logged = self.exit_order_logged(ExitOrderEvent::Release);
        let held: Vec<DetTid> = self.exit_order.held.iter().copied().collect();
        let mut released = false;
        for tid in held {
            if !self.next_turns.contains_key(&tid)
                || !self.parked.exits_awaiting_permit.contains(&tid)
            {
                // Retired, or re-admitted by another path.
                self.exit_order.held.remove(&tid);
                self.exit_order.held_keys.remove(&tid);
                continue;
            }
            let group = self
                .next_turns
                .get(&tid)
                .and_then(|turn| turn.req.try_read())
                .and_then(|req| req.ok())
                .and_then(|rs| rs.exit_identity())
                .is_some_and(|(group, _, _)| group);
            let release = match &logged {
                Some(logged) => logged.contains(&tid),
                None => !self.exit_order_must_wait(tid, group),
            };
            if release {
                self.exit_order_log(ExitOrderEvent::Release, tid);
                self.exit_order.held.remove(&tid);
                self.exit_order.held_keys.remove(&tid);
                self.parked.exits_awaiting_permit.remove(&tid);
                self.admit_to_run_queue(tid, AdmitIntent::Fixed(AdmitSide::Front));
                released = true;
            }
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn pid(raw: i32) -> DetPid {
        DetPid::from_raw(raw)
    }

    fn idle(key: u64) -> HoldInputs {
        HoldInputs {
            key: (key, pid(3)),
            vfork_child: false,
            parent_clock: 0,
            parent_queued: false,
            parent_working: false,
            granted_unreaped_sibling: false,
            earlier_runnable_sibling: false,
        }
    }

    /// Each reason holds on its own, and nothing holds when no other party
    /// can act: an idle system releases every held exit (no deadlock).
    #[test]
    fn each_reason_holds_alone_and_idle_holds_nothing() {
        assert_eq!(hold_reason(&idle(100)), None);
        let reap_first = HoldInputs {
            parent_queued: true,
            granted_unreaped_sibling: true,
            ..idle(100)
        };
        assert_eq!(hold_reason(&reap_first), Some(HoldReason::ReapFirst));
        let parent_ahead = HoldInputs {
            parent_queued: true,
            parent_working: true,
            ..idle(100)
        };
        assert_eq!(hold_reason(&parent_ahead), Some(HoldReason::ParentAhead));
        let earlier = HoldInputs {
            earlier_runnable_sibling: true,
            ..idle(100)
        };
        assert_eq!(hold_reason(&earlier), Some(HoldReason::EarlierSibling));
        // A backed-off polling parent is queued but not working.
        let polling = HoldInputs {
            parent_queued: true,
            ..idle(100)
        };
        assert_eq!(hold_reason(&polling), None);
    }

    /// A vfork child's exit releases its parent: never held.
    #[test]
    fn a_vfork_child_is_never_held() {
        let all = HoldInputs {
            vfork_child: true,
            parent_queued: true,
            parent_working: true,
            granted_unreaped_sibling: true,
            earlier_runnable_sibling: true,
            ..idle(100)
        };
        assert_eq!(hold_reason(&all), None);
    }

    /// H1 and H2 lapse once the parent's clock reaches the exit's: a parent
    /// that keeps working without reaping does not hold the exit for ever.
    #[test]
    fn the_parent_rules_lapse_at_the_exits_virtual_time() {
        let working = HoldInputs {
            parent_queued: true,
            parent_working: true,
            granted_unreaped_sibling: true,
            parent_clock: 99,
            ..idle(100)
        };
        assert_eq!(hold_reason(&working), Some(HoldReason::ReapFirst));
        let caught_up = HoldInputs {
            parent_clock: 100,
            ..working
        };
        assert_eq!(hold_reason(&caught_up), None);
    }

    /// The bound: a parent working from clock c with at least delta per
    /// committed turn releases an exit with clock k after ceil((k - c) / delta)
    /// turns.
    #[test]
    fn a_working_parent_releases_within_the_bound() {
        let (key, start, delta) = (1_000u64, 130u64, 7u64);
        let bound = (key - start).div_ceil(delta);
        let mut clock = start;
        let mut turns = 0;
        while hold_reason(&HoldInputs {
            parent_queued: true,
            parent_working: true,
            parent_clock: clock,
            ..idle(key)
        })
        .is_some()
        {
            clock += delta;
            turns += 1;
            assert!(turns <= bound, "held past the bound");
        }
        assert_eq!(turns, bound);
    }

    /// H3's termination with a sibling subtree that keeps spawning: each turn
    /// raises its process's clock by delta, a child starts at its creator's
    /// clock, a blocked process wakes only when another commits a turn. The
    /// exit is held while the subtree's largest clock is below the key and
    /// some process can run; it is always released (the multiset of budgets
    /// below the key strictly decreases).
    #[test]
    fn a_spawning_sibling_subtree_always_releases() {
        let mut seed: u64 = 0x5eed;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        for trial in 0..200 {
            let key = 50 + (trial % 50) as u64;
            // (clock, runnable)
            let mut subtree: Vec<(u64, bool)> = vec![(0, true)];
            let mut steps = 0u64;
            loop {
                let largest = subtree.iter().map(|(c, _)| *c).max().unwrap_or(u64::MAX);
                let runnable: Vec<usize> = (0..subtree.len()).filter(|&i| subtree[i].1).collect();
                let held = hold_reason(&HoldInputs {
                    earlier_runnable_sibling: largest < key && !runnable.is_empty(),
                    ..idle(key)
                })
                .is_some();
                if !held {
                    assert!(largest >= key || runnable.is_empty());
                    break;
                }
                let actor = runnable[(next() as usize) % runnable.len()];
                subtree[actor].0 += 1 + next() % 3;
                for other in subtree.iter_mut() {
                    if !other.1 && next() % 4 == 0 {
                        other.1 = true;
                    }
                }
                match next() % 6 {
                    0 => {
                        let clock = subtree[actor].0;
                        subtree.push((clock, true));
                    }
                    1 => subtree[actor].1 = false,
                    2 if subtree.len() > 1 => {
                        subtree.remove(actor);
                    }
                    _ => {}
                }
                steps += 1;
                assert!(steps < 1_000_000, "trial {trial}: no release");
            }
        }
    }

    /// A scheduler with process 1 and its children 2 and 3; 3 is exiting
    /// with clock 1,000, the others at 0.
    fn family() -> Scheduler {
        let mut sched = Scheduler::new(&Config::default());
        let tid = DetTid::from_raw;
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(1), tid(1), true, false, libc::SIGCHLD);
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(1), tid(2), true, false, libc::SIGCHLD);
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(1), tid(3), true, false, libc::SIGCHLD);
        sched.exit_order.work.insert(tid(3), 1_000);
        for raw in [1, 2, 3, 9] {
            sched
                .priorities
                .insert(tid(raw), super::super::DEFAULT_PRIORITY);
        }
        sched
    }

    fn reason(sched: &Scheduler) -> Option<HoldReason> {
        let inputs = sched
            .exit_order_hold_inputs(DetTid::from_raw(3), true)
            .expect("3 ends a process with a parent");
        hold_reason(&inputs)
    }

    /// Nothing queued: the exit is not held, whatever its key.
    #[test]
    fn an_idle_scheduler_holds_no_exit() {
        assert_eq!(reason(&family()), None);
    }

    /// A queued sibling with a smaller lower bound holds the exit; the hold
    /// lapses when its clock passes the exit's.
    #[test]
    fn a_runnable_earlier_sibling_holds_until_its_clock_passes() {
        let mut sched = family();
        sched.runqueue_push_back(DetTid::from_raw(2));
        assert_eq!(reason(&sched), Some(HoldReason::EarlierSibling));
        sched.exit_order.work.insert(DetTid::from_raw(2), 1_001);
        assert_eq!(reason(&sched), None);
    }

    /// A sibling parked for its deferred SIGCHLD counts as runnable only when
    /// nothing else is queued, when it is next to run; while other work runs
    /// it does not hold the exit.
    #[test]
    fn a_deferred_sibling_holds_only_when_it_is_next() {
        let mut sched = family();
        sched.blocked.sigchld_deferred.insert(DetTid::from_raw(2));
        assert_eq!(reason(&sched), Some(HoldReason::EarlierSibling));
        sched.runqueue_push_back(DetTid::from_raw(9));
        assert_eq!(reason(&sched), None);
    }

    /// Rule change 3 (assumption A3 of the liveness proof): an `exit_group`
    /// held while another thread of its process still runs keeps the key it
    /// was held with, so that thread's later work does not move it, and the
    /// hold still lapses once the parent's clock reaches that key.
    #[test]
    fn a_held_exit_group_keeps_its_key_while_another_thread_runs() {
        let mut sched = family();
        let tid = DetTid::from_raw;
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(3), tid(4), false, false, libc::SIGCHLD);
        sched
            .priorities
            .insert(tid(4), super::super::DEFAULT_PRIORITY);
        sched.runqueue_push_back(tid(1));
        sched.exit_order.work.insert(tid(1), 500);
        sched.exit_order_note_held(tid(3), true);
        let key = sched.exit_order_hold_inputs(tid(3), true).unwrap().key;
        assert_eq!(key, (1_000, pid(3)));
        sched.exit_order.work.insert(tid(4), 5_000);
        assert_eq!(sched.exit_order_hold_inputs(tid(3), true).unwrap().key, key);
        assert_eq!(reason(&sched), Some(HoldReason::ParentAhead));
        sched.exit_order.work.insert(tid(1), key.0);
        assert_eq!(reason(&sched), None);
    }

    /// A queued parent with a granted sibling to reap holds the exit only
    /// while its clock is below the exit's.
    #[test]
    fn a_parent_with_a_child_to_reap_holds_until_its_virtual_time() {
        let mut sched = family();
        sched.grant_exit_for_test(pid(2));
        sched.runqueue_push_back(DetTid::from_raw(1));
        assert_eq!(reason(&sched), Some(HoldReason::ReapFirst));
        sched.exit_order.work.insert(DetTid::from_raw(1), 1_000);
        assert_eq!(reason(&sched), None);
    }

    /// `family()` with the exit order on, thread 3 registered at its exit, and
    /// parent 1 queued doing work behind 3's clock (rule H2 holds 3's exit).
    fn holding_family() -> Scheduler {
        let mut sched = family();
        sched.exit_order.enabled = true;
        let tid = DetTid::from_raw;
        sched.next_turns.insert(
            tid(3),
            super::super::ThreadNextTurn {
                dettid: tid(3),
                child_tid_addr: 0,
                req: crate::ivar::Ivar::new(),
                resp: crate::ivar::Ivar::new(),
                protocol: Default::default(),
            },
        );
        sched.runqueue_push_back(tid(1));
        sched.exit_order.work.insert(tid(1), 500);
        sched
    }

    fn entry(turn: u64, event: ExitOrderEvent, tid: i32) -> ExitOrderEntry {
        ExitOrderEntry {
            turn,
            wakes: 0,
            event,
            tid: DetTid::from_raw(tid),
        }
    }

    /// Record logs each hold and each release at the point it happens.
    #[test]
    fn record_logs_each_hold_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let mut sched = holding_family();
        sched.exit_order.log_writer = Some(ExitOrderWriter::create(dir.path()).unwrap());
        sched.turn = 7;
        assert!(sched.exit_order_hold(DetTid::from_raw(3), true));
        sched.turn = 9;
        assert!(!sched.exit_order_release(), "parent 1 is still behind");
        sched.exit_order.work.insert(DetTid::from_raw(1), 1_000);
        sched.turn = 11;
        assert!(sched.exit_order_release());
        assert_eq!(
            exit_order_log::read(dir.path()).unwrap(),
            Some(VecDeque::from([
                entry(7, ExitOrderEvent::Hold, 3),
                entry(11, ExitOrderEvent::Release, 3),
            ]))
        );
    }

    /// Replay holds and releases exactly what the recording logged at each
    /// point, whatever it would decide itself: here its own inputs say "no
    /// hold" at turn 7 and "keep holding" at turn 11.
    #[test]
    fn replay_follows_the_logged_holds_and_releases() {
        let mut sched = holding_family();
        sched.exit_order.work.insert(DetTid::from_raw(1), 5_000);
        sched.exit_order.log_replay = Some(VecDeque::from([
            entry(7, ExitOrderEvent::Hold, 3),
            entry(11, ExitOrderEvent::Release, 3),
        ]));
        sched.turn = 7;
        assert!(
            sched.exit_order_hold(DetTid::from_raw(3), true),
            "held as logged"
        );
        sched.exit_order.work.insert(DetTid::from_raw(1), 0);
        sched.turn = 9;
        assert!(!sched.exit_order_release(), "nothing logged at turn 9");
        sched.turn = 11;
        assert!(sched.exit_order_release(), "released as logged");
        assert!(sched.exit_order.log_replay.as_ref().unwrap().is_empty());
        assert!(sched.rejoin_refusal.is_none());
    }

    /// Replay refuses a logged hold it has passed instead of diverging.
    #[test]
    fn replay_refuses_a_passed_logged_hold() {
        let mut sched = holding_family();
        sched.exit_order.log_replay = Some(VecDeque::from([entry(7, ExitOrderEvent::Hold, 3)]));
        sched.turn = 8;
        assert!(!sched.exit_order_hold(DetTid::from_raw(3), true));
        assert!(sched.rejoin_refusal.is_some());
    }

    /// The parent handed the turn after a child's exit is granted is found by
    /// the typed `polls_children` marker, not by the request's `fyi` text: a
    /// request that only says "wait4" is not moved.
    #[test]
    fn a_polling_parent_is_found_by_its_typed_marker() {
        let tid = DetTid::from_raw;
        for marked in [true, false] {
            let mut sched = family();
            sched.exit_order.enabled = true;
            let mut request = crate::resources::Resources::new(tid(1));
            request.insert(
                crate::resources::ResourceID::InternalIOPolling,
                crate::resources::Permission::W,
            );
            request.fyi("wait4");
            if marked {
                request.polls_children = Some(pid(1));
            }
            sched.next_turns.insert(
                tid(1),
                super::super::ThreadNextTurn {
                    dettid: tid(1),
                    child_tid_addr: 0,
                    req: crate::ivar::Ivar::full(Ok(request)),
                    resp: crate::ivar::Ivar::new(),
                    protocol: Default::default(),
                },
            );
            sched.next_turns.insert(
                tid(9),
                super::super::ThreadNextTurn {
                    dettid: tid(9),
                    child_tid_addr: 0,
                    req: crate::ivar::Ivar::new(),
                    resp: crate::ivar::Ivar::new(),
                    protocol: Default::default(),
                },
            );
            sched.runqueue_push_back(tid(9));
            sched.runqueue_push_back(tid(1));
            sched.exit_order_note_granted(pid(2));
            sched.exit_order_release();
            let (selected, _, _) = sched.step3_peek().expect("two threads are queued");
            sched.run_queue.undo_tentative_pop();
            assert_eq!(selected == tid(1), marked, "marked {marked}");
        }
    }
}
