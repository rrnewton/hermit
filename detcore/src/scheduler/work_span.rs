/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Work and span of the schedule Detcore ran (study branch, opt-in with
//! `HERMIT_WORK_SPAN=<report path>`; minibeads dh-mlmtd).
//!
//! Turns commit one at a time (`Scheduler::unblock_guest`), so commit order is
//! a topological order of every edge below and no graph is stored. Each
//! committed turn of thread `T` ends a node whose weight is `T`'s own guest
//! CPU time since its previous commit (user plus system, the quantity the exit
//! order reads; clock jumps to deadlines are excluded). The node's commit
//! happens at
//!
//! ```text
//! commit(T) = max(previous commit(T) + weight, finish of every other predecessor)
//! ```
//!
//! work is the sum of the weights, and span the largest commit time.
//!
//! Edges come from typed scheduler state only, never from log text:
//!
//! - program order: a thread's consecutive commits;
//! - spawn: the creator's commit time at the clone, to the child
//!   (`work_span_note_spawn`, where the thread tree learns of the child);
//! - exit to reap: an `Exit` commit, to the parent's next commit of a child
//!   wait (`ResourceID::WaitChild` or a `Resources::polls_children` retry),
//!   which reaps the oldest (smallest `DetPid`) exited child, as Linux's
//!   `do_wait` does;
//! - resource conflicts: the request's resource, for the variants that name
//!   guest-visible state (`conflict_key`, an exhaustive match);
//! - waiting: only in the upper bound, below.
//!
//! Detcore's resources do not name pipes, futex words or the files a
//! background call reads, so a thread that waits on another can lack the edge
//! that would order it. Two spans bound the true one:
//!
//! - `span_lo` uses only the edges above, so it can only be too short.
//! - `span_hi` adds, for a thread that waited (its request blocked or polled,
//!   or its call ran outside the schedule), an edge from every commit made
//!   while it waited: whatever it waited for is among them. Sleeps, exits
//!   held by the exit order, yields and other scheduler-only waits are not
//!   waits on another thread (`waits_on_others`).
//!
//! The scheduler-only `ResourceID`s (the global poller token, the futex token,
//! exit permits, vfork barriers, background-call tokens, priority points) are
//! excluded from conflicts by variant: they serialize the schedule without
//! carrying data.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Serialize;

use super::Scheduler;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;

/// Where to write the report, from `HERMIT_WORK_SPAN`; `None` when off.
pub(crate) fn work_span_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("HERMIT_WORK_SPAN")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    })
    .as_ref()
}

/// A time in both bounds: `lo` without the waiting edges, `hi` with them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Bounds {
    pub(crate) lo: u64,
    pub(crate) hi: u64,
}

impl Bounds {
    fn max(self, other: Bounds) -> Bounds {
        Bounds {
            lo: self.lo.max(other.lo),
            hi: self.hi.max(other.hi),
        }
    }

    fn plus(self, weight: u64) -> Bounds {
        Bounds {
            lo: self.lo + weight,
            hi: self.hi + weight,
        }
    }
}

/// The kind of predecessor that fixed a commit's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EdgeKind {
    /// The thread's own previous commit plus its work.
    Program,
    /// The creator's clone commit.
    Spawn,
    /// A child's exit, reaped by this wait.
    Reap,
    /// The last conflicting access to the request's resource.
    Resource,
    /// A commit made while the thread waited (upper bound only).
    Waited,
}

/// Accesses to one resource.
#[derive(Clone, Copy, Debug, Default)]
struct Access {
    /// The last write's commit time.
    write: Bounds,
    /// The largest read commit time since that write.
    reads: Bounds,
}

/// Scheduler-side state of the work/span report.
#[derive(Debug, Default)]
pub struct WorkSpanState {
    pub(super) enabled: bool,
    /// Each thread's guest CPU time carried with its latest request.
    cpu_now: HashMap<DetTid, u64>,
    /// Each thread's guest CPU time at its latest commit (or first request).
    cpu_committed: HashMap<DetTid, u64>,
    /// Each thread's latest commit time.
    finish: HashMap<DetTid, Bounds>,
    /// A child thread's start: its creator's latest commit when it was created.
    spawned: HashMap<DetTid, Bounds>,
    /// Threads waiting on others, with the largest commit time since they
    /// began to wait.
    waiting: HashMap<DetTid, u64>,
    resources: HashMap<ResourceID, Access>,
    /// Per parent process, its exited and not yet reaped children with their
    /// exit commit times.
    exited: HashMap<DetPid, BTreeMap<DetPid, Bounds>>,
    work: u64,
    span: Bounds,
    commits: u64,
    /// How often each kind of edge fixed a commit's time, per bound.
    binding_lo: BTreeMap<EdgeKind, u64>,
    binding_hi: BTreeMap<EdgeKind, u64>,
}

impl WorkSpanState {
    /// Empty state; on when `HERMIT_WORK_SPAN` names a report path.
    pub(super) fn new() -> Self {
        WorkSpanState {
            enabled: work_span_path().is_some(),
            ..Default::default()
        }
    }
}

/// The report written at the end of the run.
#[derive(Debug, Serialize)]
pub(crate) struct WorkSpanReport {
    pub(crate) work_ns: u64,
    pub(crate) span_lo_ns: u64,
    pub(crate) span_hi_ns: u64,
    /// work / span_hi: the parallelism the schedule surely has.
    pub(crate) parallelism_lo: f64,
    /// work / span_lo: the parallelism it has at most.
    pub(crate) parallelism_hi: f64,
    pub(crate) commits: u64,
    pub(crate) threads: usize,
    pub(crate) binding_lo: BTreeMap<EdgeKind, u64>,
    pub(crate) binding_hi: BTreeMap<EdgeKind, u64>,
}

/// The key a resource conflicts under, or `None` for a scheduler-only
/// resource. Exhaustive, so that a new variant has to be classified.
fn conflict_key(resource: &ResourceID) -> Option<&ResourceID> {
    match resource {
        ResourceID::FileContents(_)
        | ResourceID::FileMetadata(_)
        | ResourceID::DirectoryContents(_)
        | ResourceID::MemAddrSpace(_)
        | ResourceID::Path(_)
        | ResourceID::PathsTransitive(_)
        | ResourceID::Device(_) => Some(resource),
        ResourceID::Exit { .. }
        | ResourceID::ParentContinue { .. }
        | ResourceID::SleepUntil(_)
        | ResourceID::InternalIOPolling
        | ResourceID::TraceReplay
        | ResourceID::FutexWait
        | ResourceID::BlockingExternalIO(_)
        | ResourceID::BlockingVfork(_)
        | ResourceID::VforkFailed(_)
        | ResourceID::BlockedExternalContinue(_)
        | ResourceID::PriorityChangePoint(..)
        | ResourceID::WaitChild { .. }
        | ResourceID::WaitPhysicalChild(_)
        | ResourceID::InboundSignal(_)
        | ResourceID::WaitidSignals(_)
        | ResourceID::SchedYield
        | ResourceID::HappensBeforeCheckpoint(_)
        | ResourceID::HappensBeforeSyscallCheckpoint { .. }
        | ResourceID::HappensBeforePosthookInterrupted { .. }
        | ResourceID::BlockingRtSigsuspend(_)
        | ResourceID::InGuestFatalAlarm => None,
    }
}

/// Whether a request that waits for `resource` waits on another thread.
/// Exhaustive for the same reason as `conflict_key`.
fn waits_on_others(resource: &ResourceID) -> bool {
    match resource {
        ResourceID::FileContents(_)
        | ResourceID::FileMetadata(_)
        | ResourceID::DirectoryContents(_)
        | ResourceID::MemAddrSpace(_)
        | ResourceID::Path(_)
        | ResourceID::PathsTransitive(_)
        | ResourceID::Device(_)
        | ResourceID::ParentContinue { .. }
        | ResourceID::InternalIOPolling
        | ResourceID::FutexWait
        | ResourceID::BlockingExternalIO(_)
        | ResourceID::BlockingVfork(_)
        | ResourceID::BlockedExternalContinue(_)
        | ResourceID::WaitChild { .. }
        | ResourceID::WaitPhysicalChild(_)
        | ResourceID::InboundSignal(_)
        | ResourceID::WaitidSignals(_)
        | ResourceID::BlockingRtSigsuspend(_) => true,
        ResourceID::Exit { .. }
        | ResourceID::SleepUntil(_)
        | ResourceID::TraceReplay
        | ResourceID::VforkFailed(_)
        | ResourceID::PriorityChangePoint(..)
        | ResourceID::SchedYield
        | ResourceID::HappensBeforeCheckpoint(_)
        | ResourceID::HappensBeforeSyscallCheckpoint { .. }
        | ResourceID::HappensBeforePosthookInterrupted { .. }
        | ResourceID::InGuestFatalAlarm => false,
    }
}

/// Whether a granted `resource` sends the call outside the schedule.
fn goes_outside(resource: &ResourceID) -> bool {
    matches!(
        resource,
        ResourceID::BlockingExternalIO(_)
            | ResourceID::BlockingVfork(_)
            | ResourceID::BlockingRtSigsuspend(_)
    )
}

/// Note `kind` as the binding predecessor if `candidate` exceeds `best`.
fn bind(best: &mut (u64, EdgeKind), candidate: u64, kind: EdgeKind) {
    if candidate > best.0 {
        *best = (candidate, kind);
    }
}

impl Scheduler {
    /// Record the guest CPU time `tid` carried with its latest request.
    pub(crate) fn work_span_note_cpu(&mut self, tid: DetTid, cpu: u64) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        state.cpu_committed.entry(tid).or_insert(cpu);
        state.cpu_now.insert(tid, cpu);
    }

    /// `child` was created by `creator`, in the creator's turn.
    pub(crate) fn work_span_note_spawn(&mut self, creator: DetTid, child: DetTid) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        let start = state.finish.get(&creator).copied().unwrap_or_default();
        state.spawned.insert(child, start);
        state.finish.remove(&child);
        state.cpu_committed.remove(&child);
        state.waiting.remove(&child);
    }

    /// `tid`'s request for `resource` waits (it blocked or polls).
    pub(super) fn work_span_note_wait(&mut self, tid: DetTid, resource: Option<&ResourceID>) {
        let state = &mut self.work_span;
        if state.enabled && resource.is_none_or(waits_on_others) {
            state.waiting.entry(tid).or_insert(0);
        }
    }

    /// `tid`'s turn commits with request `request`.
    pub(super) fn work_span_commit(&mut self, tid: DetTid, request: Option<&Resources>) {
        if !self.work_span.enabled {
            return;
        }
        // The process this commit's exit ends, and that process's parent.
        let ends = request
            .and_then(|request| {
                request
                    .resources
                    .keys()
                    .find_map(|resource| match resource {
                        ResourceID::Exit { group, .. } => {
                            self.exit_order_ending_process(tid, *group)
                        }
                        _ => None,
                    })
            })
            .and_then(|process| Some((process, self.thread_tree.parent_process(&process)?)));
        let reaps_for = request.and_then(|request| {
            request.polls_children.or_else(|| {
                request
                    .resources
                    .keys()
                    .find_map(|resource| match resource {
                        ResourceID::WaitChild { parent, .. } => Some(*parent),
                        _ => None,
                    })
            })
        });
        let state = &mut self.work_span;
        let cpu = state.cpu_now.get(&tid).copied().unwrap_or(0);
        let weight = cpu.saturating_sub(*state.cpu_committed.entry(tid).or_insert(cpu));
        state.cpu_committed.insert(tid, cpu);

        let (previous, first_kind) = match state.finish.get(&tid) {
            Some(previous) => (*previous, EdgeKind::Program),
            None => (
                state.spawned.remove(&tid).unwrap_or_default(),
                EdgeKind::Spawn,
            ),
        };
        let ready = previous.plus(weight);
        let mut lo = (ready.lo, first_kind);
        let mut hi = (ready.hi, first_kind);

        if let Some(waited) = state.waiting.remove(&tid) {
            bind(&mut hi, waited, EdgeKind::Waited);
        }
        if let Some(parent) = reaps_for
            && let Some(children) = state.exited.get_mut(&parent)
            && let Some((_, exit)) = children.pop_first()
        {
            bind(&mut lo, exit.lo, EdgeKind::Reap);
            bind(&mut hi, exit.hi, EdgeKind::Reap);
        }
        let conflicts: Vec<(&ResourceID, &Permission)> = request
            .map(|request| {
                request
                    .resources
                    .iter()
                    .filter_map(|(resource, permission)| {
                        conflict_key(resource).map(|key| (key, permission))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (key, permission) in &conflicts {
            if let Some(access) = state.resources.get(*key) {
                let before = match permission {
                    Permission::R => access.write,
                    Permission::W | Permission::RW => access.write.max(access.reads),
                };
                bind(&mut lo, before.lo, EdgeKind::Resource);
                bind(&mut hi, before.hi, EdgeKind::Resource);
            }
        }

        let commit = Bounds { lo: lo.0, hi: hi.0 };
        *state.binding_lo.entry(lo.1).or_default() += 1;
        *state.binding_hi.entry(hi.1).or_default() += 1;
        for (key, permission) in conflicts {
            let access = state.resources.entry(key.clone()).or_default();
            match permission {
                Permission::R => access.reads = access.reads.max(commit),
                Permission::W | Permission::RW => {
                    access.write = commit;
                    access.reads = Bounds::default();
                }
            }
        }
        state.finish.insert(tid, commit);
        state.work += weight;
        state.span = state.span.max(commit);
        state.commits += 1;
        for waited in state.waiting.values_mut() {
            *waited = (*waited).max(commit.hi);
        }
        if request.is_some_and(|request| request.resources.keys().any(goes_outside)) {
            state.waiting.insert(tid, 0);
        }
        if let Some((process, parent)) = ends {
            state
                .exited
                .entry(parent)
                .or_default()
                .insert(process, commit);
        }
    }

    /// The report, from the commits so far.
    pub(crate) fn work_span_report(&self) -> Option<WorkSpanReport> {
        let state = &self.work_span;
        if !state.enabled {
            return None;
        }
        let ratio = |span: u64| {
            if span == 0 {
                0.0
            } else {
                state.work as f64 / span as f64
            }
        };
        Some(WorkSpanReport {
            work_ns: state.work,
            span_lo_ns: state.span.lo,
            span_hi_ns: state.span.hi,
            parallelism_lo: ratio(state.span.hi),
            parallelism_hi: ratio(state.span.lo),
            commits: state.commits,
            threads: state.cpu_committed.len(),
            binding_lo: state.binding_lo.clone(),
            binding_hi: state.binding_hi.clone(),
        })
    }

    /// Write the report to `HERMIT_WORK_SPAN`, if on.
    pub(crate) fn write_work_span_report(&self) {
        let (Some(report), Some(path)) = (self.work_span_report(), work_span_path()) else {
            return;
        };
        let json = serde_json::to_string_pretty(&report).expect("the report serializes");
        if let Err(err) = std::fs::write(path, json + "\n") {
            tracing::warn!(
                "[work-span] cannot write the report to {}: {err}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::resources::Device;
    use crate::types::LogicalTime;
    use crate::types::MmId;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    /// Process 1 with children 2 and 3, the report on.
    fn family() -> Scheduler {
        let mut sched = Scheduler::new(&Config::default());
        sched.work_span.enabled = true;
        for child in [1, 2, 3] {
            sched.thread_tree.add_child_with_wait_metadata(
                tid(1),
                tid(child),
                true,
                false,
                libc::SIGCHLD,
            );
        }
        sched
    }

    fn request(thread: i32, resource: Option<(ResourceID, Permission)>) -> Resources {
        let mut request = Resources::new(tid(thread));
        if let Some((resource, permission)) = resource {
            request.insert(resource, permission);
        }
        request
    }

    /// `thread` has used `cpu` in all, and its turn commits with `resource`.
    fn commit(
        sched: &mut Scheduler,
        thread: i32,
        cpu: u64,
        resource: Option<(ResourceID, Permission)>,
    ) {
        sched.work_span_note_cpu(tid(thread), cpu);
        sched.work_span_commit(tid(thread), Some(&request(thread, resource)));
    }

    fn exit(thread: i32) -> Option<(ResourceID, Permission)> {
        Some((
            ResourceID::Exit {
                group: true,
                process: DetPid::from_raw(thread),
                mm: MmId::initial(tid(thread)),
            },
            Permission::W,
        ))
    }

    fn report(sched: &Scheduler) -> (u64, u64, u64) {
        let report = sched.work_span_report().unwrap();
        (report.work_ns, report.span_lo_ns, report.span_hi_ns)
    }

    /// `thread`'s latest commit time, (lower bound, upper bound).
    fn finish(sched: &Scheduler, thread: i32) -> (u64, u64) {
        let bounds = sched.work_span.finish[&tid(thread)];
        (bounds.lo, bounds.hi)
    }

    /// Two children forked at the same point run in parallel: the span is the
    /// parent's work, then the longer child, then the parent's work after it
    /// reaps both; the work is the sum.
    #[test]
    fn forked_children_run_in_parallel_and_their_reaps_join() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 1, 100, None); // the clone of 2 and 3
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for child in [2, 3] {
            commit(&mut sched, child, 0, None);
        }
        commit(&mut sched, 2, 1_000, None);
        commit(&mut sched, 2, 1_000, exit(2));
        commit(&mut sched, 3, 300, None);
        commit(&mut sched, 3, 300, exit(3));
        assert_eq!(finish(&sched, 2), (1_100, 1_100));
        assert_eq!(finish(&sched, 3), (400, 400));
        let mut wait = request(1, None);
        wait.polls_children = Some(DetPid::from_raw(1));
        for cpu in [110, 120] {
            sched.work_span_note_cpu(tid(1), cpu);
            sched.work_span_commit(tid(1), Some(&wait));
        }
        commit(&mut sched, 1, 170, None);
        // The parent reaps 2 at 1,100 (its own 10 units since the clone run
        // in parallel), then 3 (already exited) after 10 more units, then
        // works 50 more: 1,160. Work: 170 + 1,000 + 300.
        assert_eq!(finish(&sched, 1), (1_160, 1_160));
        assert_eq!(report(&sched), (1_470, 1_160, 1_160));
    }

    /// A wait reaps one exited child: the oldest, as Linux's `do_wait` does.
    #[test]
    fn a_wait_reaps_the_oldest_exited_child_only() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 500, exit(3));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 50, exit(2));
        let mut wait = request(1, None);
        wait.polls_children = Some(DetPid::from_raw(1));
        sched.work_span_commit(tid(1), Some(&wait));
        assert_eq!(finish(&sched, 1), (50, 50), "reaped 2, not 3");
        sched.work_span_commit(tid(1), Some(&wait));
        assert_eq!(finish(&sched, 1), (500, 500), "then 3");
        sched.work_span_commit(tid(1), Some(&wait));
        assert_eq!(finish(&sched, 1), (500, 500), "nothing left to reap");
    }

    /// A read follows the last write to the same resource; reads do not
    /// order each other; a write follows the reads.
    #[test]
    fn conflicting_accesses_are_ordered() {
        let mut sched = family();
        let out = || ResourceID::Device(Device::ContainerStdout);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for thread in [1, 2, 3] {
            commit(&mut sched, thread, 0, None);
        }
        commit(&mut sched, 2, 400, Some((out(), Permission::W)));
        commit(&mut sched, 3, 500, Some((out(), Permission::R)));
        commit(&mut sched, 1, 20, Some((out(), Permission::R)));
        assert_eq!(
            finish(&sched, 1),
            (400, 400),
            "after the write, not the read"
        );
        commit(&mut sched, 2, 401, Some((out(), Permission::W)));
        assert_eq!(finish(&sched, 2), (500, 500), "after both reads");
        commit(
            &mut sched,
            1,
            30,
            Some((ResourceID::FutexWait, Permission::RW)),
        );
        assert_eq!(
            finish(&sched, 1),
            (410, 410),
            "a scheduler token orders nothing"
        );
    }

    /// The upper bound orders a thread that waited after every commit made
    /// while it waited; the lower bound does not, and a sleep waits on no one.
    #[test]
    fn a_waiting_thread_follows_what_ran_while_it_waited_in_the_upper_bound() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        sched.work_span_note_wait(tid(1), Some(&ResourceID::FutexWait));
        commit(&mut sched, 2, 900, None);
        commit(
            &mut sched,
            1,
            5,
            Some((ResourceID::FutexWait, Permission::RW)),
        );
        assert_eq!(finish(&sched, 1), (5, 900));

        sched.work_span_note_wait(
            tid(1),
            Some(&ResourceID::SleepUntil(LogicalTime::from_nanos(1))),
        );
        commit(&mut sched, 2, 2_000, None);
        commit(&mut sched, 1, 10, None);
        assert_eq!(finish(&sched, 1), (10, 905), "a sleep waits on no one");
        assert_eq!(report(&sched), (2_010, 2_000, 2_000));
    }

    /// Off by default: nothing is tracked and there is no report.
    #[test]
    fn off_by_default() {
        let mut sched = Scheduler::new(&Config::default());
        assert!(!sched.work_span.enabled);
        sched.work_span_note_cpu(tid(1), 10);
        sched.work_span_commit(tid(1), None);
        assert!(sched.work_span.cpu_now.is_empty());
        assert!(sched.work_span_report().is_none());
    }
}
