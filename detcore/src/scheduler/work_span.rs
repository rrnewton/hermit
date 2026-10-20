/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Work and span of the schedule Detcore ran, opt-in with
//! `--work-span-report=<path>` (`Config::work_span_report`), computed from
//! typed scheduler events, never from log text.
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
//!   wait (`ResourceID::WaitChild`, or a `wait4`/`waitid` retry, which the
//!   tool marks with `WorkSpanPayload::polls_children`),
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
//! polls again (`WorkSpanPayload::repolls`) observed no one and takes no edge.
//!
//! Detcore's resources do not name pipes, futex words or the files a
//! background call reads. Pipe events are gathered for this report alone: at
//! a read or write's exit the tool stats the descriptor's `/proc` link, and a
//! FIFO's host (device, inode) names the channel (host numbers, used only as
//! keys). The events ride on the thread's next resource request, in a
//! `GlobalRequest::WithWorkSpan` envelope. A thread
//! that waits on another can still lack the edge that would order it. Two spans bound the true one:
//!
//! - `span_lo` uses only the edges above, so it can only be too short.
//! - `span_hi` adds, for a thread that waited (its request blocked or polled,
//!   or its call ran outside the schedule), an edge from every commit made
//!   while it waited: whatever it waited for is among them. Child waits,
//!   sleeps, exit permits, yields and other scheduler-only waits take no such
//!   edge (`wait_kind`). The report counts which kind of
//!   wait fixed each such commit (`binding_waited`).
//!
//! The scheduler-only `ResourceID`s (the global poller token, the futex token,
//! exit permits, vfork barriers, background-call tokens, priority points) are
//! excluded from conflicts by variant: they serialize the schedule without
//! carrying data.
//!
//! **Off, it costs nothing measurable.** Every entry point returns at once
//! unless `Config::work_span_report` is set: the tool stats no `/proc` link
//! and gathers no event (`channel_event_for`), a request is sent as it always
//! was, with no envelope (`payload_for`; `Resources` itself is unchanged), and
//! the scheduler keeps no state and clones no request.
//!
//! **Memory is bounded.** Per live thread, a handful of map entries, freed
//! when the scheduler retires the thread (`work_span_retire_thread`). Per live
//! parent process, its latest child exit and its exited children not yet
//! reaped, pruned against the thread tree's unreaped children and freed with
//! the parent. Per resource, one `Access` (two `Bounds`, 32 bytes) plus its
//! key, at most `MAX_RESOURCES`; past that the table is cleared and its latest
//! time kept as a floor that every later first access takes in the upper
//! bound, so both bounds stay bounds. Per pipe or FIFO, two counters, its
//! latest read and its writes not yet read (24 bytes each): at most
//! `MAX_CHANNELS` channels and `MAX_UNREAD_WRITES` writes in all; past either,
//! pipe edges stop for the rest of the run (`pipe_tracking_stopped`): reads
//! are then unmatched and polls keep their waiting edge, so both bounds stay
//! bounds.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::Scheduler;
use crate::config::Config;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;

/// Most resources with a remembered access; past it the table is cleared
/// into `WorkSpanState::resource_floor`.
const MAX_RESOURCES: usize = 4096;
/// Most pipes and FIFOs tracked, and most writes not yet read across them;
/// past either, pipe edges stop.
const MAX_CHANNELS: usize = 65_536;
const MAX_UNREAD_WRITES: usize = 65_536;

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
    /// Where to write the report (`Config::work_span_report`).
    report: Option<PathBuf>,
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
    /// Each thread's pending request's envelope facts, until it commits.
    request_facts: HashMap<DetTid, RequestFacts>,
    /// The latest access time of resources cleared from `resources`, which a
    /// first access takes in the upper bound.
    resource_floor: Bounds,
    resource_evictions: u64,
    /// Per parent process, its exited and not yet reaped children with their
    /// exit commit times.
    exited: HashMap<DetPid, BTreeMap<DetPid, Bounds>>,
    work: u64,
    span: Bounds,
    commits: u64,
    /// Threads seen in all, retired ones included.
    threads: u64,
    /// How often each kind of edge fixed a commit's time, per bound.
    binding_lo: BTreeMap<EdgeKind, u64>,
    binding_hi: BTreeMap<EdgeKind, u64>,
    /// Of the upper bound's `Waited` bindings, how many each kind of wait made.
    binding_waited: BTreeMap<WaitKind, u64>,
    channels: HashMap<(u64, u64), Channel>,
    /// Writes not yet read, across all channels.
    unread_writes: usize,
    /// Pipe edges stopped at this commit count, past a memory bound.
    pipe_tracking_stopped: Option<u64>,
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
    /// Empty state; on when `--work-span-report` names a path.
    pub(super) fn new(cfg: &Config) -> Self {
        WorkSpanState {
            enabled: cfg.work_span_report.is_some(),
            report: cfg.work_span_report.clone(),
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
    pub(crate) threads: u64,
    pub(crate) binding_lo: BTreeMap<EdgeKind, u64>,
    pub(crate) binding_hi: BTreeMap<EdgeKind, u64>,
    pub(crate) binding_waited: BTreeMap<WaitKind, u64>,
    pub(crate) pipe_reads_matched: u64,
    pub(crate) pipe_reads_unmatched: u64,
    /// Times the resource table was cleared past `MAX_RESOURCES`.
    pub(crate) resource_evictions: u64,
    /// The commit count at which pipe edges stopped, past a memory bound.
    pub(crate) pipe_tracking_stopped: Option<u64>,
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
fn wait_kind(request: &Resources, polls_children: bool) -> Option<WaitKind> {
    if polls_children {
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

/// The pipe event a call's result gives, or `None`. `locate` names the
/// channel behind a descriptor (a `/proc` stat) and `cpu` reads the thread's
/// guest CPU time; neither runs unless the report is on and the call is a
/// read or write that moved bytes or read an end of file
/// (`crate::pipe_wake::progress_of`).
pub(crate) fn channel_event_for(
    on: bool,
    call: &reverie::syscalls::Syscall,
    res: &Result<i64, reverie::Error>,
    polled: bool,
    locate: impl FnOnce(std::os::unix::io::RawFd) -> Option<(u64, u64)>,
    cpu: impl FnOnce() -> u64,
) -> Option<ChannelEvent> {
    if !on {
        return None;
    }
    let (Ok(moved), Some((fd, readies))) = (res, crate::pipe_wake::progress_of(call)) else {
        return None;
    };
    let read = readies == crate::pipe_wake::Wants::Space;
    if *moved < 0 || (*moved == 0 && !read) {
        return None;
    }
    let channel = locate(fd)?;
    Some(ChannelEvent {
        channel,
        bytes: *moved as u64,
        read,
        polled,
        cpu: cpu(),
    })
}

/// What a resource request carries for the report, in a
/// `GlobalRequest::WithWorkSpan` envelope sent only with the report on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSpanPayload {
    /// The process whose children the request polls for: a `wait4` or
    /// `waitid` retry.
    pub(crate) polls_children: Option<DetPid>,
    /// A poll the current call makes again, because its previous polled
    /// attempt found nothing ready.
    pub(crate) repolls: bool,
    /// The pipe events the thread gathered since its previous request.
    pub(crate) channel_events: Vec<ChannelEvent>,
}

/// The envelope's payload for resource request `request`, or `None` with the
/// report off, when it touches nothing and the request goes as it always did.
/// On, it takes the gathered events and the call's child-wait marker, and
/// marks a poll the current call makes again.
pub(crate) fn payload_for(
    on: bool,
    request: &Resources,
    events: &mut Vec<ChannelEvent>,
    polled: &mut bool,
    polls_children: &mut Option<DetPid>,
) -> Option<WorkSpanPayload> {
    if !on {
        return None;
    }
    let mut repolls = false;
    if request
        .resources
        .contains_key(&ResourceID::InternalIOPolling)
    {
        repolls = *polled;
        *polled = true;
    }
    Some(WorkSpanPayload {
        polls_children: polls_children.take(),
        repolls,
        channel_events: std::mem::take(events),
    })
}

/// A pending request's facts from its envelope, kept until it commits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RequestFacts {
    polls_children: Option<DetPid>,
    repolls: bool,
}

/// Note `kind` as the binding predecessor if `candidate` exceeds `best`.
fn bind(best: &mut (u64, EdgeKind), candidate: u64, kind: EdgeKind) {
    if candidate > best.0 {
        *best = (candidate, kind);
    }
}

impl Scheduler {
    /// A resource request from `tid` arrives, carrying guest CPU time `cpu`
    /// and, from a tool with the report on, its envelope's payload.
    pub(crate) fn work_span_note_request(
        &mut self,
        tid: DetTid,
        cpu: u64,
        payload: Option<&WorkSpanPayload>,
    ) {
        // A request from a thread the scheduler already retired keeps no state.
        if !self.work_span.enabled || !self.next_turns.contains_key(&tid) {
            return;
        }
        let state = &mut self.work_span;
        if !state.cpu_committed.contains_key(&tid) {
            state.threads += 1;
            state.cpu_committed.insert(tid, cpu);
        }
        state.cpu_now.insert(tid, cpu);
        let facts = payload.map_or_else(RequestFacts::default, |payload| RequestFacts {
            polls_children: payload.polls_children,
            repolls: payload.repolls,
        });
        if facts == RequestFacts::default() {
            state.request_facts.remove(&tid);
        } else {
            state.request_facts.insert(tid, facts);
        }
        if let Some(payload) = payload {
            self.work_span_note_events(tid, cpu, &payload.channel_events);
        }
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
    fn work_span_note_events(&mut self, tid: DetTid, cpu: u64, events: &[ChannelEvent]) {
        if !self.work_span.enabled || events.is_empty() || !self.next_turns.contains_key(&tid) {
            return;
        }
        let commits = self.work_span.commits;
        let state = &mut self.work_span;
        if state.pipe_tracking_stopped.is_some() {
            state.pipe_reads_unmatched +=
                events.iter().filter(|e| e.read && e.bytes > 0).count() as u64;
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
            if (!state.channels.contains_key(&event.channel)
                && state.channels.len() >= MAX_CHANNELS)
                || (!event.read && state.unread_writes >= MAX_UNREAD_WRITES)
            {
                // Past a memory bound: stop pipe edges for the rest of the run.
                state.pipe_tracking_stopped = Some(commits);
                state.channels = HashMap::new();
                state.unread_writes = 0;
                return;
            }
            let at = base.plus(event.cpu.saturating_sub(committed));
            let channel = state.channels.entry(event.channel).or_default();
            if !event.read {
                channel.written += event.bytes;
                channel.writes.push_back((channel.written, at));
                state.unread_writes += 1;
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
            if event.bytes == 0 {
                // An end of file: the closes that gave it are not gathered.
                continue;
            }
            channel.last_read = Some(channel.last_read.unwrap_or_default().max(at));
            let last = channel.read + event.bytes;
            channel.read = last;
            if last > channel.written {
                // A write this report did not see: start the stream again.
                channel.written = last;
                state.unread_writes -= channel.writes.len();
                channel.writes.clear();
                state.pipe_reads_unmatched += 1;
                continue;
            }
            while channel.writes.front().is_some_and(|(end, _)| *end < last) {
                channel.writes.pop_front();
                state.unread_writes -= 1;
            }
            let (end, written) = *channel
                .writes
                .front()
                .expect("a write ends at or past `last`");
            if end == last {
                channel.writes.pop_front();
                state.unread_writes -= 1;
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
        let polls_children = state
            .request_facts
            .get(&tid)
            .is_some_and(|facts| facts.polls_children.is_some());
        if let Some(kind) = wait_kind(request, polls_children) {
            state.waiting.entry(tid).or_insert((0, kind));
        }
    }

    /// `tid`'s turn commits with request `request`.
    pub(super) fn work_span_commit(&mut self, tid: DetTid, request: Option<&Resources>) {
        if !self.work_span.enabled {
            return;
        }
        // The process this commit's exit ends, that process's parent, and the
        // parent's children not yet reaped.
        let ends = request
            .and_then(|request| {
                request
                    .resources
                    .keys()
                    .find_map(|resource| match resource {
                        ResourceID::Exit { group, .. } => {
                            self.work_span_ending_process(tid, *group)
                        }
                        _ => None,
                    })
            })
            .and_then(|process| {
                let parent = self.thread_tree.parent_process(&process)?;
                Some((
                    process,
                    parent,
                    self.thread_tree.unreaped_children_of(parent),
                ))
            });
        let facts = self
            .work_span
            .request_facts
            .remove(&tid)
            .unwrap_or_default();
        let reaps_for = request.and_then(|request| {
            facts.polls_children.or_else(|| {
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
        let committed = match state.cpu_committed.get(&tid) {
            Some(committed) => *committed,
            None => {
                state.threads += 1;
                cpu
            }
        };
        let weight = cpu.saturating_sub(committed);
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
        let repolls = facts.repolls;
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
            match state.resources.get(*key) {
                Some(access) => {
                    let before = match permission {
                        Permission::R => access.write,
                        Permission::W | Permission::RW => access.write.max(access.reads),
                    };
                    bind(&mut lo, before.lo, EdgeKind::Resource);
                    bind(&mut hi, before.hi, EdgeKind::Resource);
                }
                // Perhaps cleared past `MAX_RESOURCES`: the upper bound
                // takes the cleared table's latest time.
                None => bind(&mut hi, state.resource_floor.hi, EdgeKind::Resource),
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
            if !state.resources.contains_key(key) && state.resources.len() >= MAX_RESOURCES {
                for access in state.resources.values() {
                    state.resource_floor = state.resource_floor.max(access.write).max(access.reads);
                }
                state.resources = HashMap::new();
                state.resource_evictions += 1;
            }
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
        if let Some((process, parent, unreaped)) = ends {
            let exited = state.exited.entry(parent).or_default();
            // Children reaped some other way (an ignored `SIGCHLD`) go.
            exited.retain(|child, _| unreaped.contains(child));
            exited.insert(process, commit);
            let latest = state.latest_child_exit.entry(parent).or_default();
            *latest = latest.max(commit);
        }
    }

    /// The scheduler retires `tid` (`Scheduler::logically_kill_thread`): it
    /// commits no more, so its state goes, and with its process's last thread,
    /// the process's state as a parent.
    pub(super) fn work_span_retire_thread(
        &mut self,
        tid: DetTid,
        process: DetPid,
        last_of_process: bool,
    ) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        state.cpu_now.remove(&tid);
        state.cpu_committed.remove(&tid);
        state.finish.remove(&tid);
        state.spawned.remove(&tid);
        state.waiting.remove(&tid);
        state.pipe_pred.remove(&tid);
        state.space_pred.remove(&tid);
        state.poll_explained.remove(&tid);
        state.deferred_poll.remove(&tid);
        state.request_facts.remove(&tid);
        if last_of_process {
            state.exited.remove(&process);
            state.latest_child_exit.remove(&process);
        }
    }

    /// The process `tid`'s exit ends, if it ends one: an `exit_group`, or the
    /// exit of its process's last live thread.
    fn work_span_ending_process(&self, tid: DetTid, group: bool) -> Option<DetPid> {
        let process = self.registered_process(tid)?;
        let last = group
            || !self
                .thread_tree
                .my_thread_group(&process)
                .into_iter()
                .any(|other| other != tid && self.next_turns.contains_key(&other));
        last.then_some(process)
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
            threads: state.threads,
            binding_lo: state.binding_lo.clone(),
            binding_hi: state.binding_hi.clone(),
            binding_waited: state.binding_waited.clone(),
            pipe_reads_matched: state.pipe_reads_matched,
            pipe_reads_unmatched: state.pipe_reads_unmatched,
            resource_evictions: state.resource_evictions,
            pipe_tracking_stopped: state.pipe_tracking_stopped,
        })
    }

    /// Write the report to `--work-span-report`'s path, if set.
    pub(crate) fn write_work_span_report(&self) {
        let (Some(report), Some(path)) = (self.work_span_report(), self.work_span.report.as_ref())
        else {
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
        let mut sched = Scheduler::new(&Config {
            work_span_report: Some(PathBuf::from("/nonexistent/report.json")),
            ..Config::default()
        });
        assert!(sched.work_span.enabled);
        for child in [1, 2, 3] {
            sched.thread_tree.add_child_with_wait_metadata(
                tid(1),
                tid(child),
                true,
                false,
                libc::SIGCHLD,
            );
            sched.next_turns.insert(
                tid(child),
                super::super::ThreadNextTurn {
                    dettid: tid(child),
                    child_tid_addr: 0,
                    req: crate::ivar::Ivar::new(),
                    resp: crate::ivar::Ivar::new(),
                    protocol: Default::default(),
                },
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
        sched.work_span_note_request(tid(thread), cpu, None);
        sched.work_span_commit(tid(thread), Some(&request(thread, resource)));
    }

    /// Thread 1's request polls for process 1's children, with `cpu` used.
    fn child_wait(sched: &mut Scheduler, cpu: u64) {
        let payload = WorkSpanPayload {
            polls_children: Some(DetPid::from_raw(1)),
            ..WorkSpanPayload::default()
        };
        sched.work_span_note_request(tid(1), cpu, Some(&payload));
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
        let wait = request(1, None);
        for cpu in [110, 120] {
            child_wait(&mut sched, cpu);
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
        let wait = request(1, None);
        child_wait(&mut sched, 0);
        sched.work_span_commit(tid(1), Some(&wait));
        assert_eq!(finish(&sched, 1), (50, 50), "reaped 2, not 3");
        child_wait(&mut sched, 0);
        sched.work_span_commit(tid(1), Some(&wait));
        assert_eq!(finish(&sched, 1), (500, 500), "then 3");
        child_wait(&mut sched, 0);
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
        let wait = request(1, Some((ResourceID::InternalIOPolling, Permission::W)));
        child_wait(&mut sched, 0);
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
        sched.work_span_note_request(tid(2), 1_000, None);
        sched.work_span_note_events(
            tid(2),
            1_000,
            &[event(3, false, false, 300), event(2, false, false, 700)],
        );
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        // Thread 1 reads 4 bytes at its 10 (the 4th byte is the second write's,
        // at 700), then works to 50 before its request.
        sched.work_span_note_request(tid(1), 50, None);
        sched.work_span_note_events(tid(1), 50, &[event(4, true, false, 10)]);
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(finish(&sched, 1), (740, 740));
        assert_eq!(sched.work_span.pipe_reads_matched, 1);
        // A read past every write seen is not matched, and takes no edge.
        sched.work_span_note_request(tid(1), 60, None);
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
            sched.work_span_note_request(tid(2), 100, None);
            if explained {
                sched.work_span_note_events(tid(2), 100, &[event(1, false, false, 100)]);
            }
            sched.work_span_commit(tid(2), Some(&request(2, None)));
            commit(&mut sched, 3, 5_000, None);
            // The polled retry's grant, then the read's result on the next request.
            sched.work_span_commit(tid(1), Some(&poll()));
            sched.work_span_note_request(tid(1), 20, None);
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
        let repolled = WorkSpanPayload {
            repolls: true,
            ..WorkSpanPayload::default()
        };
        sched.work_span_note_request(tid(1), 20, Some(&repolled));
        sched.work_span_commit(tid(1), Some(&poll()));
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
        sched.work_span_note_request(tid(1), 200, None);
        sched.work_span_note_events(tid(1), 200, &[event(4, true, false, 200)]);
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        commit(&mut sched, 3, 5_000, None);
        sched.work_span_commit(tid(2), Some(&poll(2)));
        sched.work_span_note_request(tid(2), 30, None);
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

    /// Off, the tool gathers nothing: it neither stats a descriptor nor reads
    /// the clock, whatever the call. On, a pipe read or write gives an event.
    #[test]
    fn off_the_tool_never_stats_or_gathers() {
        use reverie::syscalls::Syscall;
        let read = Syscall::Read(reverie::syscalls::Read::new().with_fd(3).with_len(8));
        let write = Syscall::Write(reverie::syscalls::Write::new().with_fd(4).with_len(8));
        for (call, res) in [(&read, Ok(8)), (&read, Ok(0)), (&write, Ok(8))] {
            assert_eq!(
                channel_event_for(
                    false,
                    call,
                    &res,
                    true,
                    |_| panic!("stat with the report off"),
                    || panic!("clock read with the report off"),
                ),
                None
            );
        }
        let event = channel_event_for(true, &read, &Ok(8), true, |fd| Some((9, fd as u64)), || 77);
        assert_eq!(
            event,
            Some(ChannelEvent {
                channel: (9, 3),
                bytes: 8,
                read: true,
                polled: true,
                cpu: 77,
            })
        );
        assert_eq!(
            channel_event_for(true, &write, &Ok(0), false, |_| Some((9, 4)), || 1),
            None,
            "a write that moved nothing is no event"
        );
        assert_eq!(
            channel_event_for(true, &read, &Ok(8), false, |_| None, || 1),
            None,
            "not a pipe or FIFO"
        );
    }

    /// Off, there is no envelope and nothing is taken, so the request goes as
    /// it always did (`Resources` is unchanged). On, the envelope carries the
    /// events, the child-wait marker and a repeated poll's mark, and survives
    /// the RPC's own encoding (bincode, legacy configuration, as reverie's
    /// debug validation encodes every request).
    #[test]
    fn off_a_request_goes_without_an_envelope() {
        let poll = request(1, Some((ResourceID::InternalIOPolling, Permission::W)));
        let mut events = vec![event(1, false, false, 5)];
        let mut polled = true;
        let mut children = Some(DetPid::from_raw(1));
        assert_eq!(
            payload_for(false, &poll, &mut events, &mut polled, &mut children),
            None
        );
        assert_eq!(
            (events.len(), polled, children),
            (1, true, Some(DetPid::from_raw(1)))
        );

        polled = false;
        let first = payload_for(true, &poll, &mut events, &mut polled, &mut children).unwrap();
        assert_eq!(
            first,
            WorkSpanPayload {
                polls_children: Some(DetPid::from_raw(1)),
                repolls: false,
                channel_events: vec![event(1, false, false, 5)],
            }
        );
        assert!(events.is_empty() && polled && children.is_none());
        let again = payload_for(true, &poll, &mut events, &mut polled, &mut children).unwrap();
        assert!(again.repolls, "the same call polls again");

        use crate::tool_global::GlobalRequest;
        let wrapped = GlobalRequest::WithWorkSpan(
            Box::new(first),
            Box::new(GlobalRequest::RequestResources(poll, DetPid::from_raw(1))),
        );
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&wrapped, config).unwrap();
        let (decoded, _): (GlobalRequest, usize) =
            bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, wrapped);
    }

    /// A retired thread's state goes, and with its process's last thread, the
    /// process's state as a parent.
    #[test]
    fn retirement_frees_a_threads_state() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 10, exit(2));
        assert!(!sched.work_span.exited.is_empty());
        sched.work_span_retire_thread(tid(2), DetPid::from_raw(2), true);
        sched.work_span_retire_thread(tid(1), DetPid::from_raw(1), true);
        assert_eq!(
            sched.work_span_report().unwrap().threads,
            2,
            "retired threads still count"
        );
        let state = &sched.work_span;
        assert!(state.finish.is_empty() && state.cpu_now.is_empty());
        assert!(state.cpu_committed.is_empty() && state.spawned.is_empty());
        assert!(state.exited.is_empty() && state.latest_child_exit.is_empty());
        // A request from a retired thread keeps no state.
        sched.next_turns.remove(&tid(2));
        sched.work_span_note_request(tid(2), 50, None);
        assert!(sched.work_span.cpu_now.is_empty());
    }

    /// Past `MAX_RESOURCES` the table is cleared: the lower bound loses those
    /// edges, and the upper bound orders every later first access after the
    /// cleared table's latest time, so it stays an upper bound.
    #[test]
    fn the_resource_table_is_bounded_and_stays_sound() {
        let path = |n: usize| ResourceID::Path(PathBuf::from(format!("/p{n}")));
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 1, 1_000, Some((path(0), Permission::W)));
        for n in 1..=MAX_RESOURCES {
            commit(&mut sched, 1, 1_000, Some((path(n), Permission::W)));
        }
        assert_eq!(sched.work_span.resource_evictions, 1);
        assert!(sched.work_span.resources.len() <= 1);
        // Thread 2 reads /p0, which thread 1 wrote at 1,000 before the clear.
        commit(&mut sched, 2, 7, Some((path(0), Permission::R)));
        assert_eq!(
            finish(&sched, 2),
            (7, 1_000),
            "the floor, in the upper bound only"
        );
    }

    /// Past `MAX_UNREAD_WRITES`, pipe edges stop for the rest of the run and
    /// their memory goes; reads are then unmatched.
    #[test]
    fn pipe_tracking_is_bounded() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        let writes: Vec<ChannelEvent> = (0..=MAX_UNREAD_WRITES as u64)
            .map(|n| event(1, false, false, n))
            .collect();
        sched.work_span_note_request(tid(1), MAX_UNREAD_WRITES as u64 + 1, None);
        sched.work_span_note_events(tid(1), MAX_UNREAD_WRITES as u64 + 1, &writes);
        assert!(sched.work_span.pipe_tracking_stopped.is_some());
        assert!(sched.work_span.channels.is_empty());
        sched.work_span_note_events(
            tid(1),
            MAX_UNREAD_WRITES as u64 + 1,
            &[event(1, true, true, 0)],
        );
        assert!(sched.work_span.pipe_pred.is_empty() && sched.work_span.poll_explained.is_empty());
        assert_eq!(sched.work_span.pipe_reads_unmatched, 1);
    }

    /// Off by default: nothing is tracked and there is no report.
    #[test]
    fn off_by_default() {
        let mut sched = Scheduler::new(&Config::default());
        assert!(!sched.work_span.enabled);
        sched.work_span_note_request(tid(1), 10, None);
        sched.work_span_commit(tid(1), None);
        assert!(sched.work_span.cpu_now.is_empty());
        assert!(sched.work_span_report().is_none());
    }
}
