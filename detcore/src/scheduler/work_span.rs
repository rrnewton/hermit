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
//! - pipes: a read of a pipe or FIFO, from the write that supplied its last
//!   byte (`ChannelEvent`, counted through the channel as a byte stream);
//!
//! A child wait needs no other edge: what it waits for is the exit it reaps.
//! In the upper bound, a polled call explained by its result takes no waiting
//! edge: a read by the write that supplied it, a write that waited for room by
//! the channel's latest read, and a call a `SIGCHLD` ended (its delivery is
//! the next commit) by the parent's latest child exit. A `SIGCHLD` delivery is
//! likewise ordered after the parent's latest child exit instead of after
//! everything. An end-of-file read keeps its waiting edge: the closes that
//! give it are not gathered. A polled attempt that found nothing ready and
//! polls again (`Resources::repolls`) observed no one and takes no edge.
//!
//! Detcore's resources do not name pipes, futex words or the files a
//! background call reads. Pipe events are gathered for this report alone: at
//! a read or write's exit the tool stats the descriptor's `/proc` link, and a
//! FIFO's host (device, inode) names the channel (host numbers, used only as
//! keys). The events ride on the thread's next resource request. A thread
//! that waits on another can still lack the edge that would order it. Two spans bound the true one:
//!
//! - `span_lo` uses only the edges above, so it can only be too short.
//! - `span_hi` adds, for a thread that waited (its request blocked or polled,
//!   or its call ran outside the schedule), an edge from every commit made
//!   while it waited: whatever it waited for is among them. Child waits,
//!   sleeps, exits held by the exit order, yields and other scheduler-only
//!   waits take no such edge (`wait_kind`). The report counts which kind of
//!   wait fixed each such commit (`binding_waited`).
//!
//! The scheduler-only `ResourceID`s (the global poller token, the futex token,
//! exit permits, vfork barriers, background-call tokens, priority points) are
//! excluded from conflicts by variant: they serialize the schedule without
//! carrying data.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;
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
    /// The write that supplied the last byte a pipe read returned.
    Pipe,
    /// The parent's latest child exit, for a `SIGCHLD` delivery (upper bound
    /// only).
    ChildSignal,
}

/// A read or write that moved bytes through a pipe or FIFO, or a read that
/// returned its end of file, gathered by the tool for this report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChannelEvent {
    /// The host channel's (device, inode): a key within this run only.
    pub(crate) channel: (u64, u64),
    /// Bytes moved; 0 for an end of file.
    pub(crate) bytes: u64,
    /// A read, rather than a write.
    pub(crate) read: bool,
    /// The result of a call that polled for it (`InternalIOPolling`).
    pub(crate) polled: bool,
    /// The thread's guest CPU time at the call's exit.
    pub(crate) cpu: u64,
}

/// One channel's byte stream.
#[derive(Debug, Default)]
struct Channel {
    /// Bytes written and read so far.
    written: u64,
    read: u64,
    /// Writes not yet wholly read: the stream offset each ends at, and when.
    writes: VecDeque<(u64, Bounds)>,
    /// The latest read's time.
    last_read: Option<Bounds>,
}

/// What a thread that waited waited for (`wait_kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitKind {
    /// A polled descriptor (`ResourceID::InternalIOPolling` without a child
    /// wait): a pipe, FIFO, socket or other descriptor read or written.
    IoPoll,
    /// A futex wait.
    Futex,
    /// A call that ran outside the schedule (a background call, a vfork
    /// parent, an `rt_sigsuspend`).
    Outside,
    /// A signal to deliver, or a `waitid` waiting for one.
    Signal,
    /// A `SIGCHLD` to deliver.
    ChildSignal,
    /// Guest-visible state another thread holds (`conflict_key`).
    Resource,
    /// A vfork parent waiting for its child to exec or exit.
    VforkParent,
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
    /// began to wait and what they wait for.
    waiting: HashMap<DetTid, (u64, WaitKind)>,
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
    /// Of the upper bound's `Waited` bindings, how many each kind of wait made.
    binding_waited: BTreeMap<WaitKind, u64>,
    channels: HashMap<(u64, u64), Channel>,
    /// Per thread, the pipe edge its next commit takes (the supplying write's
    /// time plus the thread's work since its read).
    pipe_pred: HashMap<DetTid, Bounds>,
    /// Per thread, the upper bound's edge for a polled write that waited for
    /// room: the channel's latest read, plus the thread's work since.
    space_pred: HashMap<DetTid, u64>,
    /// Threads whose latest polled call was a read a pipe edge explains.
    poll_explained: HashSet<DetTid>,
    /// Per thread, the waiting edge of a polled commit, applied at its next
    /// commit unless the polled call turns out to be an explained read.
    deferred_poll: HashMap<DetTid, u64>,
    /// Per parent process, its latest child exit.
    latest_child_exit: HashMap<DetPid, Bounds>,
    /// Pipe reads matched to their write, and reads that could not be.
    pipe_reads_matched: u64,
    pipe_reads_unmatched: u64,
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
    pub(crate) binding_waited: BTreeMap<WaitKind, u64>,
    pub(crate) pipe_reads_matched: u64,
    pub(crate) pipe_reads_unmatched: u64,
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

/// What a request that waits (blocked or polled) waits for, or `None` when it
/// takes no waiting edge: a child wait, whose dependency is the exit it reaps
/// (the reap edge), and scheduler-only waits such as a sleep, a held exit or
/// a yield. Exhaustive, so that a new variant has to be classified.
fn wait_kind(request: &Resources) -> Option<WaitKind> {
    if request.polls_children.is_some() {
        return None;
    }
    let resource = request.resources.keys().next()?;
    match resource {
        ResourceID::FileContents(_)
        | ResourceID::FileMetadata(_)
        | ResourceID::DirectoryContents(_)
        | ResourceID::MemAddrSpace(_)
        | ResourceID::Path(_)
        | ResourceID::PathsTransitive(_)
        | ResourceID::Device(_) => Some(WaitKind::Resource),
        ResourceID::InternalIOPolling => Some(WaitKind::IoPoll),
        ResourceID::FutexWait => Some(WaitKind::Futex),
        ResourceID::BlockingExternalIO(_)
        | ResourceID::BlockingVfork(_)
        | ResourceID::BlockedExternalContinue(_)
        | ResourceID::BlockingRtSigsuspend(_) => Some(WaitKind::Outside),
        ResourceID::InboundSignal(signal) if signal.raw() == libc::SIGCHLD => {
            Some(WaitKind::ChildSignal)
        }
        ResourceID::InboundSignal(_) | ResourceID::WaitidSignals(_) => Some(WaitKind::Signal),
        ResourceID::ParentContinue { .. } => Some(WaitKind::VforkParent),
        ResourceID::WaitChild { .. }
        | ResourceID::WaitPhysicalChild(_)
        | ResourceID::Exit { .. }
        | ResourceID::SleepUntil(_)
        | ResourceID::TraceReplay
        | ResourceID::VforkFailed(_)
        | ResourceID::PriorityChangePoint(..)
        | ResourceID::SchedYield
        | ResourceID::HappensBeforeCheckpoint(_)
        | ResourceID::HappensBeforeSyscallCheckpoint { .. }
        | ResourceID::HappensBeforePosthookInterrupted { .. }
        | ResourceID::InGuestFatalAlarm => None,
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

    /// The pipe events `tid` gathered since its previous request, which
    /// carries guest CPU time `cpu`. Each is timed within the thread's current
    /// segment; a write enters its channel's stream, and a read takes the
    /// edge from the write that supplied its last byte for the thread's next
    /// commit. Called as the request arrives, before any later commit: a
    /// writer's events arrive with its next request, before the scheduler can
    /// let a reader run.
    pub(crate) fn work_span_note_events(&mut self, tid: DetTid, cpu: u64, events: &[ChannelEvent]) {
        let state = &mut self.work_span;
        if !state.enabled || events.is_empty() {
            return;
        }
        let base = state
            .finish
            .get(&tid)
            .or_else(|| state.spawned.get(&tid))
            .copied()
            .unwrap_or_default();
        let committed = state.cpu_committed.get(&tid).copied().unwrap_or(cpu);
        for event in events {
            let at = base.plus(event.cpu.saturating_sub(committed));
            let channel = state.channels.entry(event.channel).or_default();
            if !event.read {
                channel.written += event.bytes;
                channel.writes.push_back((channel.written, at));
                // A polled write waited for room, which a read made: no later
                // than the latest read seen, all of which came before.
                if event.polled
                    && let Some(read) = channel.last_read
                {
                    let after = read.hi + cpu.saturating_sub(event.cpu);
                    let pred = state.space_pred.entry(tid).or_default();
                    *pred = (*pred).max(after);
                    state.poll_explained.insert(tid);
                }
                continue;
            }
            if event.bytes > 0 {
                channel.last_read = Some(channel.last_read.unwrap_or_default().max(at));
            }
            if event.bytes == 0 {
                // An end of file: the closes that gave it are not gathered.
                continue;
            }
            let last = channel.read + event.bytes;
            channel.read = last;
            if last > channel.written {
                // A write this report did not see: start the stream again.
                channel.written = last;
                channel.writes.clear();
                state.pipe_reads_unmatched += 1;
                continue;
            }
            while channel.writes.front().is_some_and(|(end, _)| *end < last) {
                channel.writes.pop_front();
            }
            let (end, written) = *channel
                .writes
                .front()
                .expect("a write ends at or past `last`");
            if end == last {
                channel.writes.pop_front();
            }
            let after = written.plus(cpu.saturating_sub(event.cpu));
            let pred = state.pipe_pred.entry(tid).or_default();
            *pred = pred.max(after);
            if event.polled {
                state.poll_explained.insert(tid);
            }
            state.pipe_reads_matched += 1;
        }
    }

    /// `tid`'s request `request` waits (it blocked or polls).
    pub(super) fn work_span_note_wait(&mut self, tid: DetTid, request: &Resources) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        if let Some(kind) = wait_kind(request) {
            state.waiting.entry(tid).or_insert((0, kind));
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
        let process = self.thread_tree.process_of(tid);
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

        // Which kind of wait a `Waited` binding of this commit came from.
        let mut waited_kind = None;
        let explained = state.poll_explained.remove(&tid);
        // A polled call ended by a `SIGCHLD`, whose delivery is this commit:
        // a child's exit readied it.
        let child_signal = request.is_some_and(|request| {
            request.resources.keys().any(|resource| {
                matches!(resource, ResourceID::InboundSignal(signal) if signal.raw() == libc::SIGCHLD)
            })
        });
        // A poll the call makes again: its previous attempt, granted at the
        // previous commit, found nothing ready, so observed no one.
        let repolls = request.is_some_and(|request| request.repolls);
        if let Some(deferred) = state.deferred_poll.remove(&tid)
            && !explained
            && !repolls
        {
            if child_signal {
                if let Some(exit) = process.and_then(|p| state.latest_child_exit.get(&p)) {
                    bind(&mut hi, exit.hi, EdgeKind::ChildSignal);
                }
            } else if deferred + weight > hi.0 {
                hi = (deferred + weight, EdgeKind::Waited);
                waited_kind = Some(WaitKind::IoPoll);
            }
        }
        if let Some(pred) = state.space_pred.remove(&tid) {
            bind(&mut hi, pred, EdgeKind::Pipe);
        }
        if let Some(pred) = state.pipe_pred.remove(&tid) {
            bind(&mut lo, pred.lo, EdgeKind::Pipe);
            bind(&mut hi, pred.hi, EdgeKind::Pipe);
        }
        match state.waiting.remove(&tid) {
            Some((time, WaitKind::IoPoll)) => {
                // The polled call runs after this commit; its result decides.
                state.deferred_poll.insert(tid, time);
            }
            Some((_, WaitKind::ChildSignal)) => {
                if let Some(exit) = process.and_then(|p| state.latest_child_exit.get(&p)) {
                    bind(&mut hi, exit.hi, EdgeKind::ChildSignal);
                }
            }
            Some((time, kind)) if time > hi.0 => {
                hi = (time, EdgeKind::Waited);
                waited_kind = Some(kind);
            }
            Some(_) | None => {}
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
        if hi.1 == EdgeKind::Waited
            && let Some(kind) = waited_kind
        {
            *state.binding_waited.entry(kind).or_default() += 1;
        }
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
        for (waited, _) in state.waiting.values_mut() {
            *waited = (*waited).max(commit.hi);
        }
        if request.is_some_and(|request| request.resources.keys().any(goes_outside)) {
            state.waiting.insert(tid, (0, WaitKind::Outside));
        }
        if let Some((process, parent)) = ends {
            state
                .exited
                .entry(parent)
                .or_default()
                .insert(process, commit);
            let latest = state.latest_child_exit.entry(parent).or_default();
            *latest = latest.max(commit);
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
            binding_waited: state.binding_waited.clone(),
            pipe_reads_matched: state.pipe_reads_matched,
            pipe_reads_unmatched: state.pipe_reads_unmatched,
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
    use crate::types::SigWrapper;

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
        sched.work_span_note_wait(
            tid(1),
            &request(1, Some((ResourceID::FutexWait, Permission::RW))),
        );
        commit(&mut sched, 2, 900, None);
        commit(
            &mut sched,
            1,
            5,
            Some((ResourceID::FutexWait, Permission::RW)),
        );
        assert_eq!(finish(&sched, 1), (5, 900));

        let sleep = ResourceID::SleepUntil(LogicalTime::from_nanos(1));
        sched.work_span_note_wait(tid(1), &request(1, Some((sleep, Permission::R))));
        commit(&mut sched, 2, 2_000, None);
        commit(&mut sched, 1, 10, None);
        assert_eq!(finish(&sched, 1), (10, 905), "a sleep waits on no one");
        assert_eq!(report(&sched), (2_010, 2_000, 2_000));
    }

    /// A child wait is satisfied by the exit it reaps: what else ran while it
    /// waited does not order it, in either bound.
    #[test]
    fn a_child_wait_takes_only_its_reap_edge() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        let mut wait = request(1, Some((ResourceID::InternalIOPolling, Permission::W)));
        wait.polls_children = Some(DetPid::from_raw(1));
        sched.work_span_note_wait(tid(1), &wait);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 100, exit(2));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 5_000, None);
        sched.work_span_commit(tid(1), Some(&wait));
        assert_eq!(
            finish(&sched, 1),
            (100, 100),
            "child 3's work does not order it"
        );
        assert!(sched.work_span.binding_waited.is_empty());
    }

    fn event(bytes: u64, read: bool, polled: bool, cpu: u64) -> ChannelEvent {
        ChannelEvent {
            channel: (7, 42),
            bytes,
            read,
            polled,
            cpu,
        }
    }

    /// A pipe read takes the edge from the write that supplied its last
    /// byte, timed within both threads' segments.
    #[test]
    fn a_pipe_read_follows_the_write_that_supplied_it() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        // Thread 2 writes 3 bytes at 300 and 2 more at 700, then requests at 1,000.
        sched.work_span_note_cpu(tid(2), 1_000);
        sched.work_span_note_events(
            tid(2),
            1_000,
            &[event(3, false, false, 300), event(2, false, false, 700)],
        );
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        // Thread 1 reads 4 bytes at its 10 (the 4th byte is the second write's,
        // at 700), then works to 50 before its request.
        sched.work_span_note_cpu(tid(1), 50);
        sched.work_span_note_events(tid(1), 50, &[event(4, true, false, 10)]);
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(finish(&sched, 1), (740, 740));
        assert_eq!(sched.work_span.pipe_reads_matched, 1);
        // A read past every write seen is not matched, and takes no edge.
        sched.work_span_note_cpu(tid(1), 60);
        sched.work_span_note_events(tid(1), 60, &[event(9, true, false, 55)]);
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(finish(&sched, 1), (750, 750));
        assert_eq!(sched.work_span.pipe_reads_unmatched, 1);
    }

    /// A polled read explained by a pipe edge takes no waiting edge in the
    /// upper bound; a polled call that is not one keeps it.
    #[test]
    fn an_explained_poll_drops_its_waiting_edge() {
        let poll = || request(1, Some((ResourceID::InternalIOPolling, Permission::W)));
        for explained in [true, false] {
            let mut sched = family();
            sched.work_span_note_spawn(tid(1), tid(2));
            sched.work_span_note_spawn(tid(1), tid(3));
            for thread in [1, 2, 3] {
                commit(&mut sched, thread, 0, None);
            }
            sched.work_span_note_wait(tid(1), &poll());
            sched.work_span_note_cpu(tid(2), 100);
            if explained {
                sched.work_span_note_events(tid(2), 100, &[event(1, false, false, 100)]);
            }
            sched.work_span_commit(tid(2), Some(&request(2, None)));
            commit(&mut sched, 3, 5_000, None);
            // The polled retry's grant, then the read's result on the next request.
            sched.work_span_commit(tid(1), Some(&poll()));
            sched.work_span_note_cpu(tid(1), 20);
            if explained {
                sched.work_span_note_events(tid(1), 20, &[event(1, true, true, 1)]);
            }
            sched.work_span_commit(tid(1), Some(&request(1, None)));
            let expected = if explained { (119, 119) } else { (20, 5_020) };
            assert_eq!(finish(&sched, 1), expected, "explained: {explained}");
        }

        // An attempt that found nothing and polls again takes no edge.
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        sched.work_span_note_wait(tid(1), &poll());
        commit(&mut sched, 2, 5_000, None);
        sched.work_span_commit(tid(1), Some(&poll()));
        let mut again = poll();
        again.repolls = true;
        sched.work_span_note_cpu(tid(1), 20);
        sched.work_span_commit(tid(1), Some(&again));
        assert_eq!(finish(&sched, 1), (20, 20));
    }

    /// A polled write that waited for room follows the channel's latest
    /// read in the upper bound, not everything that ran meanwhile; a polled
    /// call that a `SIGCHLD` ended follows the latest child exit.
    #[test]
    fn a_full_pipe_write_and_an_interrupted_poll_are_explained() {
        let poll = |thread| request(thread, Some((ResourceID::InternalIOPolling, Permission::W)));
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for thread in [1, 2, 3] {
            commit(&mut sched, thread, 0, None);
        }
        // Thread 2 waits to write into a full pipe; thread 1 reads at 200.
        sched.work_span_note_wait(tid(2), &poll(2));
        sched.work_span_note_events(tid(1), 0, &[event(4, false, false, 0)]);
        sched.work_span_note_cpu(tid(1), 200);
        sched.work_span_note_events(tid(1), 200, &[event(4, true, false, 200)]);
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        commit(&mut sched, 3, 5_000, None);
        sched.work_span_commit(tid(2), Some(&poll(2)));
        sched.work_span_note_cpu(tid(2), 30);
        sched.work_span_note_events(tid(2), 30, &[event(1, false, true, 10)]);
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        assert_eq!(finish(&sched, 2), (30, 220));

        // Thread 3 exits at 5,100; thread 1's poll is ended by the SIGCHLD.
        commit(&mut sched, 3, 5_100, exit(3));
        sched.work_span_note_wait(tid(1), &poll(1));
        commit(&mut sched, 2, 9_000, None);
        sched.work_span_commit(tid(1), Some(&poll(1)));
        let delivery = request(
            1,
            Some((
                ResourceID::InboundSignal(SigWrapper(libc::SIGCHLD)),
                Permission::RW,
            )),
        );
        sched.work_span_commit(tid(1), Some(&delivery));
        assert_eq!(finish(&sched, 1), (200, 5_100), "not 9,000");
    }

    /// A `SIGCHLD` delivery is ordered after the parent's latest child exit
    /// in the upper bound, not after everything that ran meanwhile.
    #[test]
    fn a_sigchld_delivery_follows_the_latest_child_exit() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        let delivery = request(
            1,
            Some((
                ResourceID::InboundSignal(SigWrapper(libc::SIGCHLD)),
                Permission::RW,
            )),
        );
        sched.work_span_note_wait(tid(1), &delivery);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 300, exit(2));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 9_000, None);
        sched.work_span_commit(tid(1), Some(&delivery));
        assert_eq!(finish(&sched, 1), (0, 300));
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
