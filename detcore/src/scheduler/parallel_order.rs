/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Study-branch scheduling experiments for process parallelism, both off by
//! default and enabled only by environment variables. Each changes the serial
//! schedule, deterministically, so that the logical execution has the
//! parallelism a build has natively. See the dev-hermit survey
//! `ai_docs/2026-10-18-detcore-parallelism-survey.md`.
//!
//! `HERMIT_PARALLEL_VFORK_REJOIN=1`: a vfork parent rejoins the run queue,
//! ahead of other work, at the step-2 pass in which its barrier ends: after
//! its child's release edge (successful exec or `Exit` grant) the barrier
//! already waits, committing no turn, for the parent's continuation. Without
//! the flag the continuation stays a background completion that is readmitted
//! only when no other deterministic work is runnable, so `make -jN` runs its
//! jobs one after another.
//!
//! `HERMIT_PARALLEL_EXIT_ORDER=1`: a child process's exit is granted in order
//! of a per-process virtual completion clock, with the process ID as the
//! tie-break, instead of in the order the serial schedule happens to finish
//! the children. A process's clock is the parent's clock when the process was
//! first seen plus the work of its threads since then (deterministic logical
//! CPU time: retired branches, trapped instructions and syscall costs, without
//! the jumps a thread's clock takes when it is advanced to a deadline), or the
//! clock of a child whose exit was granted, if larger (a Lamport merge at
//! reap).
//! An exit is held while a sibling process (same wait parent) whose lower
//! bound is smaller still has a thread that can run; the lower bound of a
//! process is the largest clock in its live subtree. Held exits are released
//! at the step-2 pass in which no such sibling remains. An exit is also held
//! while its parent has an exited child it has not reaped yet and the parent
//! is queued to run, so a parent reaps its children one at a time, in that
//! order, as it would when they finish at different times; and while the
//! parent is doing work (queued other than as a backed-off poller) and its
//! clock is behind the exit's, so it starts its next job first. When an exit is
//! granted, a wait parent that polls `wait4`/`waitid` is moved to the front of
//! the run queue at the next step 2, so it reaps before the next sibling exit
//! commits. Every input is committed scheduler state, so the order is the
//! same in every run.

use std::sync::OnceLock;

use super::AdmitIntent;
use super::AdmitSide;
use super::Scheduler;
use crate::types::DetPid;
use crate::types::DetTid;

/// The environment variable enabling the deterministic vfork rejoin.
pub const VFORK_REJOIN_ENV: &str = "HERMIT_PARALLEL_VFORK_REJOIN";
/// The environment variable enabling the virtual-completion exit order.
pub const EXIT_ORDER_ENV: &str = "HERMIT_PARALLEL_EXIT_ORDER";

fn env_on(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v == "1")
}

/// Whether the deterministic vfork rejoin is on.
pub fn vfork_rejoin() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| env_on(VFORK_REJOIN_ENV))
}

/// Whether the virtual-completion exit order is on.
pub fn exit_order() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| env_on(EXIT_ORDER_ENV))
}

/// Exit with a clear message if the exit order is enabled under `hermit
/// record` or `hermit replay`. A recording of GNU hello `make -j8` with it
/// replayed to a point where a logged readmission never completed (zlib
/// passed), so its inputs are not yet known to be reproduced by replay.
/// Refusing beats a divergent replay.
pub fn refuse_unsupported_modes(recordreplay_modes: bool) {
    // Experiment only: lets the step-2a replay readmission be measured.
    if env_on("HERMIT_PARALLEL_EXIT_ORDER_ALLOW_RR") {
        return;
    }
    if recordreplay_modes && exit_order() {
        eprintln!(
            "hermit: {EXIT_ORDER_ENV}=1 is not supported under record or replay (study-branch experiment)"
        );
        std::process::exit(2);
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
    /// Wait parents of processes whose exit was just granted; their polled
    /// `wait4`/`waitid` is handed the next turn at the next step 2.
    wake_parents: std::collections::BTreeSet<DetPid>,
}

impl Scheduler {
    /// Record the logical CPU time `tid` carried with its latest request.
    pub(crate) fn exit_order_note_work(&mut self, tid: DetTid, cpu: u64) {
        if !exit_order() {
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
                            // which happens once no other work can run.
                            || self.blocked.sigchld_deferred.contains(&thread))
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
    /// virtual time.
    fn exit_order_must_wait(&self, tid: DetTid, group: bool) -> bool {
        let Some(process) = self.exit_order_ending_process(tid, group) else {
            return false;
        };
        let Some(parent) = self.thread_tree.parent_process(&process) else {
            return false;
        };
        // A vfork child's exit releases its blocked parent, and step 3 must
        // select a registered vfork child: never hold it.
        if self
            .vfork_barriers
            .values()
            .any(|child| *child == Some(process))
        {
            return false;
        }
        let key = (self.exit_order_clock(process), process);
        let siblings: Vec<DetPid> = self
            .thread_tree
            .unreaped_children_of(parent)
            .into_iter()
            .filter(|sibling| *sibling != process)
            .collect();
        // One reapable child at a time: while the parent has a sibling whose
        // exit was granted and that it has not reaped yet, and the parent is
        // queued to run (so it will get to its wait), wait for that reap.
        let parent_queued = self.thread_tree.contains(&parent)
            && self
                .thread_tree
                .my_thread_group(&parent)
                .into_iter()
                .any(|thread| self.run_queue.contains_tid(thread));
        if parent_queued
            && siblings
                .iter()
                .any(|sibling| self.process_exit_granted(*sibling))
        {
            return true;
        }
        // The parent is doing work (queued, not as a backed-off poller) and
        // has not reached this exit's virtual time: in a parallel run it
        // would act first (start its next job) and only then observe the exit.
        let parent_working = self.thread_tree.contains(&parent)
            && self
                .thread_tree
                .my_thread_group(&parent)
                .into_iter()
                .any(|thread| self.run_queue.contains_non_polling_tid(thread));
        if parent_working && self.exit_order_clock(parent) < key.0 {
            return true;
        }
        siblings
            .into_iter()
            .filter(|sibling| !self.process_exit_granted(*sibling))
            .any(|sibling| {
                (self.exit_order_lower_bound(sibling, 0), sibling) < key
                    && self.exit_order_subtree_runnable(sibling, 0)
            })
    }

    /// Called at an `Exit` request's step 4: hold it if the order requires.
    /// Returns whether the exit is now held.
    pub(super) fn exit_order_hold(&mut self, tid: DetTid, group: bool) -> bool {
        if !exit_order() || !self.exit_order_must_wait(tid, group) {
            if exit_order()
                && let Some(process) = self.exit_order_ending_process(tid, group)
            {
                crate::parallelism_probe::note(serde_json::json!({
                    "event": "exit_order", "action": "grant", "turn": self.turn,
                    "pid": process.as_raw(), "key": self.exit_order_clock(process),
                }));
            }
            return false;
        }
        if let Some(process) = self.exit_order_ending_process(tid, group) {
            crate::parallelism_probe::note(serde_json::json!({
                "event": "exit_order", "action": "hold", "turn": self.turn,
                "pid": process.as_raw(), "key": self.exit_order_clock(process),
            }));
        }
        self.exit_order.held.insert(tid);
        self.parked.exits_awaiting_permit.insert(tid);
        true
    }

    /// Called when an exit that ends `process` is granted: fold its clock into
    /// its parent's (the reap edge).
    pub(super) fn exit_order_note_granted(&mut self, process: DetPid) {
        if !exit_order() {
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
                let polls_wait = request
                    .resources
                    .contains_key(&crate::resources::ResourceID::InternalIOPolling)
                    && (request.fyi.contains("wait4") || request.fyi.contains("waitid"));
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
        if !exit_order() {
            return false;
        }
        // Moving a parent applies now; it does not need another pass.
        self.exit_order_wake_parents();
        if self.exit_order.held.is_empty() {
            return false;
        }
        let held: Vec<DetTid> = self.exit_order.held.iter().copied().collect();
        let mut released = false;
        for tid in held {
            if !self.next_turns.contains_key(&tid)
                || !self.parked.exits_awaiting_permit.contains(&tid)
            {
                // Retired, or re-admitted by another path.
                self.exit_order.held.remove(&tid);
                continue;
            }
            let group = self
                .next_turns
                .get(&tid)
                .and_then(|turn| turn.req.try_read())
                .and_then(|req| req.ok())
                .and_then(|rs| rs.exit_identity())
                .is_some_and(|(group, _, _)| group);
            if !self.exit_order_must_wait(tid, group) {
                self.exit_order.held.remove(&tid);
                self.parked.exits_awaiting_permit.remove(&tid);
                self.admit_to_run_queue(tid, AdmitIntent::Fixed(AdmitSide::Front));
                released = true;
            }
        }
        released
    }

    /// Step 2a: readmit a vfork parent, ahead of other work, as its barrier
    /// ends at its continuation.
    pub(super) fn vfork_rejoin_readmit(&mut self, parent: DetTid) {
        if !vfork_rejoin() {
            return;
        }
        let Some(op_id) = self.blocked.external_io_blockers.get(&parent).copied() else {
            return;
        };
        // Replay readmits the parent where the recording logged it, which is
        // here: record readmits at step 2a, before anything later in the pass
        // reads the run queue. The exit order's release does (whether the
        // parent is queued or working decides a hold), so readmitting it later
        // in the pass, at step 2c, let replay release and re-hold an exit the
        // recording kept held, a turn number the recording never spent, and
        // every logged readmission after it fell one turn early. Any other
        // entry stays with step 2c, which refuses one that does not match.
        if let Some(rejoins) = self.rejoin_replay.as_mut() {
            let point = (self.turn, self.empty_queue_wakes);
            let due_here = rejoins.front().is_some_and(|next| {
                next.point() == point && next.calls == [(parent, op_id.sequence)]
            });
            if !due_here {
                return;
            }
            rejoins.pop_front();
            self.rejoin_stalled_since = None;
        } else {
            // Record logs the readmission like any other, so replay follows it.
            self.log_rejoin(&[(parent, op_id, false)], &[parent]);
        }
        self.blocked.external_io_blockers.remove(&parent);
        self.blocked.replay_log_served.remove(&parent);
        let _ = self.blocked.out_of_scheduler_masks.remove(&parent);
        // The message `step2c_process_io_blockers` logs for a readmission,
        // which is what replay logs when it follows the rejoin log here.
        tracing::info!(
            target: "detcore::scheduler",
            "[step2] Reschedule formerly backgrounded dtid {:?}",
            parent
        );
        self.readmit_ready_thread(parent);
    }
}
