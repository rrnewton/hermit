/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Work and span of the schedule Detcore ran, opt-in with
//! `--work-span-report=<path>` (`Config::work_span_report`), computed from
//! typed scheduler events, never from log text. The report is written when
//! the run ends normally (the scheduler finds nothing left to run); a run
//! ended by a refusal or a fatal error writes none.
//!
//! Turns commit one at a time (`Scheduler::unblock_guest`), so commit order is
//! a topological order of every edge below and no graph is stored. Each
//! committed turn of thread `T` ends a node whose weight is `T`'s own guest
//! CPU time since its previous commit (user plus system; clock jumps to
//! deadlines are excluded). Every request samples that clock; a child's first
//! request carries the clock it inherited, which becomes its baseline, so
//! inherited time is not counted again. The node's commit happens at
//!
//! ```text
//! commit(T) = max(previous commit(T) + weight, every other predecessor)
//! ```
//!
//! Work is the sum of the weights, and span the largest commit time. Two spans
//! are kept:
//!
//! - `span_lo` takes only edges that are certainly real, so it can only be too
//!   short;
//! - `span_hi` adds conservative edges wherever a dependency may be hidden, so
//!   it is an upper bound of the dependencies the report observes, unless the
//!   report lists an `UpperBoundCaveat`. Every report lists what neither span
//!   observes (`Unobserved`: data through regular files or shared mappings
//!   that nothing else orders, and devices).
//!
//! Edges in both bounds, all from typed state:
//!
//! - program order: a thread's consecutive commits;
//! - spawn: the creator's commit, to the child (`work_span_note_spawn`);
//! - exit to reap: a child's `Exit` commit, to the thread whose wait reaped
//!   exactly that child (`GlobalRequest::ConsumeChildWait`), from the reap on;
//! - resource conflicts: the request's resource, for the `ResourceID` variants
//!   that name guest-visible state (`conflict_key`, an exhaustive match);
//! - pipes: a read of an anonymous pipe, from every write that supplied its
//!   bytes, the pipe counted as a byte stream (`ChannelEvent`).
//!
//! An edge met inside a segment (a pipe read, a reap) holds from that point of
//! the thread's CPU time on (`Segment`), so a write the thread makes later in
//! the same segment carries it too.
//!
//! Edges in the upper bound only:
//!
//! - waiting: a thread whose request waited (it blocked, polled, was parked
//!   outside the run queue, or its call ran outside the schedule) follows
//!   every commit made while it waited (`wait_kind`, an exhaustive match;
//!   sleeps, exit permits, yields and other scheduler-only waits take none).
//!   A polled call is spared that edge when its result explains it: a pipe
//!   read by its supplying writes, a wait by its reap, a call the tool saw end
//!   interrupted and whose `SIGCHLD` delivery is the next commit by the latest
//!   possible cause of a `SIGCHLD` (a child exit, or a signal the guest sent),
//!   and an attempt that found nothing and moved nothing, then polled again
//!   (`WorkSpanPayload::repolls`, `Resources::previous_attempt_made_progress`);
//! - untracked data: a read whose sender the report cannot name (a socket, a
//!   named FIFO, an anonymous inode, a pipe's end of file, a pipe it can no
//!   longer follow) follows every other thread as far as it has run
//!   (`work_span_untracked`);
//! - shared memory: a thread follows the other threads of its process as far
//!   as they have run, since their shared memory shows no event.
//!
//! Pipe events are gathered for this report alone, by the tool, at each
//! call's exit (`gather`): the descriptor's `/proc` link names it, an
//! anonymous pipe by its host (device, inode), a key within this run only. A
//! thread's events ride on its next grant-awaiting request (and an exec's
//! preparation) in a `GlobalRequest::WithWorkSpan` envelope, so they reach
//! the scheduler before another thread can commit. A pipe the report can no
//! longer follow (a read past every write seen, an uncounted call on it, a
//! reader that died mid-turn) is poisoned: its reads take the untracked edge
//! instead of a pairing that could be wrong.
//!
//! **Off, it costs nothing measurable.** Every entry point returns at once
//! unless `Config::work_span_report` is set: the tool stats no `/proc` link
//! and gathers no event (`gather`), a request is sent as it always was, with
//! no envelope (`payload_for`, `flush_for`; `Resources` itself is unchanged),
//! and the scheduler keeps no state, clones no request and samples no clock.
//! On a backend whose tool runs inside the guest, the tool gathers nothing
//! even with the option on (`gathers`), and the report says its upper span is
//! not maintained (`UpperBoundCaveat::InGuestTool`).
//!
//! **Memory is bounded.** Per live thread, a handful of map entries, freed
//! when the scheduler retires the thread (`work_span_retire_thread`) and
//! moved when a non-leader execs (`work_span_transfer_thread`); in the tool,
//! at most `MAX_PENDING_EVENTS` buffered events, past which pipe edges stop.
//! Per live parent process, its latest child exit and the exits of its
//! zombies, at most `MAX_EXITED_PER_PARENT`, freed with the parent. Per
//! resource, one `Access` (32 bytes) plus its key, at most `MAX_RESOURCES`;
//! past that the table is cleared and its latest time kept as a floor that a
//! resource seen again keeps as its write, in the upper bound, until a real
//! write. Per pipe, two counters and its unread writes (24 bytes each): at
//! most `MAX_CHANNELS` pipes and `MAX_UNREAD_WRITES` writes in all; past
//! either, pipe edges stop for the rest of the run (`pipe_tracking_stopped`)
//! and reads take the untracked edge. Each cap keeps both bounds sound.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
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
    /// The writes that supplied the bytes a pipe read returned.
    Pipe,
    /// The latest possible cause of a `SIGCHLD`: the parent's latest child
    /// exit, or the latest signal the guest sent (upper bound only).
    ChildSignal,
    /// Everything that ran, for data from a channel the report cannot follow
    /// (upper bound only).
    Untracked,
    /// The other threads of the process, which share its memory (upper bound
    /// only).
    SharedMemory,
}

/// What a `ChannelEvent` records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ChannelEventKind {
    /// Bytes written to an anonymous pipe.
    Write,
    /// Bytes read from an anonymous pipe, or its end of file (no bytes).
    Read,
    /// Bytes moved through an anonymous pipe by a call the report does not
    /// count (`splice`, `tee`, `vmsplice`, `sendfile`): the pipe's stream can
    /// no longer be followed, and the call may have read.
    Untracked,
    /// Data received from a channel the report does not follow: a socket, a
    /// named FIFO, or an anonymous inode (an eventfd, a signalfd). Its sender
    /// is unknown, so the upper bound orders it after everything that ran.
    UntrackedRead,
    /// A signal the guest sent (`kill` and its kin): a possible cause of a
    /// signal another thread receives.
    SignalSent,
}

/// What the report makes of a descriptor (`gather`'s `classify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DescriptorClass {
    /// An anonymous pipe, by its host (device, inode).
    Pipe((u64, u64)),
    /// A channel whose data's sender the report cannot name.
    Untracked,
}

/// A pipe or signal event, gathered by the tool for this report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChannelEvent {
    /// The host pipe's (device, inode): a key within this run only.
    pub(crate) channel: (u64, u64),
    pub(crate) kind: ChannelEventKind,
    /// Bytes moved; 0 for an end of file.
    pub(crate) bytes: u64,
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
    /// The stream can no longer be followed: a read went past every write
    /// seen, an untracked call moved bytes, or a reader died mid-turn. Its
    /// reads are unmatched from then on.
    poisoned: bool,
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
    /// A child wait the scheduler parks (`ResourceID::WaitChild`).
    ChildWait,
}

/// Why a report's upper span is not an upper bound. Each is listed in the
/// report, which then says the bound is not maintained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpperBoundCaveat {
    /// The backend's tool runs inside the guest and gathers nothing, so data
    /// read from pipes, sockets and other channels has no edge.
    InGuestTool,
}

/// Dependencies neither span observes, whatever the run; every report lists
/// them. Builds order a file's producer and consumer through the process tree
/// (a wait, then a spawn), which both spans do observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Unobserved {
    /// Data passed through a regular file between processes that nothing
    /// else orders.
    RegularFiles,
    /// Memory shared between processes (`MAP_SHARED`, System V segments).
    SharedMappings,
    /// Data passed through a device (a terminal, a pty).
    Devices,
}

/// Every report's `Unobserved` list.
const UNOBSERVED: [Unobserved; 3] = [
    Unobserved::RegularFiles,
    Unobserved::SharedMappings,
    Unobserved::Devices,
];

/// Predecessors met at events inside a thread's current segment: from guest
/// CPU time `cpu` on, the thread's time is at least `cpu + offset`. Only the
/// largest offset per bound is kept, which is all a later time needs.
#[derive(Clone, Copy, Debug, Default)]
struct Segment {
    lo: Option<(i128, EdgeKind)>,
    hi: Option<(i128, EdgeKind)>,
}

/// `offset` applied at guest CPU time `cpu`.
fn shifted(cpu: u64, offset: i128) -> u64 {
    u64::try_from((i128::from(cpu) + offset).max(0)).unwrap_or(u64::MAX)
}

/// Most exited, unreaped children kept per parent; past it the oldest goes,
/// which only drops a reap edge (the reaping wait keeps its waiting edge).
const MAX_EXITED_PER_PARENT: usize = 4096;

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
    /// Per thread, the predecessors met inside its current segment (pipe
    /// reads, reaps, untracked data), which its next commit takes.
    segment: HashMap<DetTid, Segment>,
    /// The latest signal the guest sent, in the upper bound.
    signal_sent: Option<u64>,
    /// Upper-bound edges taken for data the report could not follow.
    fallback_edges: u64,
    /// Why the upper span is not an upper bound, if anything.
    caveats: BTreeSet<UpperBoundCaveat>,
    /// Threads whose latest polled call was a read a pipe edge explains.
    poll_explained: HashSet<DetTid>,
    /// Per thread, the waiting edge of a commit that ended a polled or parked
    /// wait, and its kind, applied at the thread's next commit unless the
    /// call's result explains the wait (a supplying write, a reap).
    deferred_poll: HashMap<DetTid, (u64, WaitKind)>,
    /// Threads whose latest commit was their own exit: everything they
    /// gathered went with that request.
    exited_threads: HashSet<DetTid>,
    /// Per parent process, its latest child exit.
    latest_child_exit: HashMap<DetPid, Bounds>,
    /// Pipe reads matched to their write, and reads that could not be.
    pipe_reads_matched: u64,
    pipe_reads_unmatched: u64,
    /// Channels whose stream can no longer be followed.
    channels_poisoned: u64,
    /// Per thread, the channels it has read, which its death mid-turn could
    /// leave with reads the report never saw.
    read_channels: HashMap<DetTid, HashSet<(u64, u64)>>,
}

impl WorkSpanState {
    /// Empty state; on when `--work-span-report` names a path.
    pub(super) fn new(cfg: &Config) -> Self {
        let mut caveats = BTreeSet::new();
        if cfg.backend.tool_shares_guest_descriptor_table {
            caveats.insert(UpperBoundCaveat::InGuestTool);
        }
        WorkSpanState {
            enabled: cfg.work_span_report.is_some(),
            report: cfg.work_span_report.clone(),
            caveats,
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
    /// Channels whose stream could no longer be followed (`Channel::poisoned`).
    pub(crate) channels_poisoned: u64,
    /// Upper-bound edges taken for data the report could not follow.
    pub(crate) fallback_edges: u64,
    /// Whether `span_hi_ns` is an upper bound of the observed dependencies:
    /// false when `upper_bound_caveats` lists a reason.
    pub(crate) upper_bound_maintained: bool,
    pub(crate) upper_bound_caveats: Vec<UpperBoundCaveat>,
    /// Dependencies neither span observes.
    pub(crate) unobserved: Vec<Unobserved>,
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
        ResourceID::WaitChild { .. } | ResourceID::WaitPhysicalChild(_) => {
            Some(WaitKind::ChildWait)
        }
        ResourceID::Exit { .. }
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

/// Most pipe events a thread buffers between two requests; past it the
/// thread's next request reports an overflow and pipe edges stop.
pub(crate) const MAX_PENDING_EVENTS: usize = 4096;

/// The tool's per-thread state for the report (`ThreadState::work_span`),
/// untouched unless the tool gathers (`gathers`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSpanThread {
    /// The pipe events since the thread's previous grant-awaiting request.
    pub(crate) events: Vec<ChannelEvent>,
    /// The current call polled (made an `InternalIOPolling` request), so its
    /// result is the polled one.
    pub(crate) polled: bool,
    /// The latest polled call ended interrupted: `EINTR` or a restart errno.
    pub(crate) interrupted: bool,
    /// Events were dropped past `MAX_PENDING_EVENTS`.
    pub(crate) overflow: bool,
}

/// Whether the tool gathers for the report: the flag is set and the tool runs
/// outside the guest. A tool inside the guest's process (an in-guest backend)
/// would allocate in it, so there it gathers nothing and sends no envelope.
pub(crate) fn gathers(cfg: &Config) -> bool {
    cfg.work_span_report.is_some() && !cfg.backend.tool_shares_guest_descriptor_table
}

/// The device number of anonymous pipes (pipefs), from a pipe the tool makes
/// once. Named FIFOs live on other devices; their streams can outlive a
/// reader and restart, so the report follows anonymous pipes only.
pub(crate) fn pipefs_device() -> Option<u64> {
    static DEVICE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *DEVICE.get_or_init(|| {
        use std::os::unix::fs::MetadataExt;
        let (read, _write) = nix::unistd::pipe().ok()?;
        std::fs::File::from(read)
            .metadata()
            .ok()
            .map(|meta| meta.dev())
    })
}

/// Gather what a call's result gives the report into `thread`: an anonymous
/// pipe's read or write that moved bytes (or read its end of file), data
/// received from a channel the report does not follow, an untracked call on a
/// pipe, a signal the guest sent, and whether a polled call ended interrupted.
/// `classify` names a descriptor (a `/proc` stat); `None` means a regular
/// file, a directory or a device, which the report does not observe. `cpu`
/// reads the thread's guest CPU time. Neither runs unless the report is on.
pub(crate) fn gather(
    on: bool,
    call: &reverie::syscalls::Syscall,
    res: &Result<i64, reverie::Error>,
    thread: &mut WorkSpanThread,
    classify: impl Fn(std::os::unix::io::RawFd) -> Option<DescriptorClass>,
    cpu: impl Fn() -> u64,
) {
    use reverie::syscalls::Errno;
    use reverie::syscalls::Syscall;
    if !on {
        return;
    }
    let polled = std::mem::take(&mut thread.polled);
    if polled {
        thread.interrupted = matches!(
            res,
            Err(reverie::Error::Errno(
                Errno::EINTR
                    | Errno::ERESTARTSYS
                    | Errno::ERESTARTNOINTR
                    | Errno::ERESTARTNOHAND
                    | Errno::ERESTART_RESTARTBLOCK
            ))
        );
    }
    let Ok(moved) = *res else {
        return;
    };
    let push = |thread: &mut WorkSpanThread, channel, kind, bytes| {
        if thread.events.len() >= MAX_PENDING_EVENTS {
            thread.overflow = true;
        } else {
            thread.events.push(ChannelEvent {
                channel,
                kind,
                bytes,
                polled,
                cpu: cpu(),
            });
        }
    };
    if matches!(
        call,
        Syscall::Kill(_)
            | Syscall::Tkill(_)
            | Syscall::Tgkill(_)
            | Syscall::RtSigqueueinfo(_)
            | Syscall::RtTgsigqueueinfo(_)
            | Syscall::Other(reverie::syscalls::Sysno::pidfd_send_signal, _)
    ) {
        push(thread, (0, 0), ChannelEventKind::SignalSent, 0);
        return;
    }
    if let Some((fd, readies)) = crate::pipe_wake::progress_of(call) {
        let read = readies == crate::pipe_wake::Wants::Space;
        match classify(fd) {
            Some(DescriptorClass::Pipe(channel)) if moved > 0 || read => {
                let kind = if read {
                    ChannelEventKind::Read
                } else {
                    ChannelEventKind::Write
                };
                push(thread, channel, kind, moved as u64);
            }
            Some(DescriptorClass::Untracked) if read && moved > 0 => {
                push(
                    thread,
                    (0, 0),
                    ChannelEventKind::UntrackedRead,
                    moved as u64,
                );
            }
            _ => {}
        }
        return;
    }
    let received = match call {
        Syscall::Recvfrom(call) => Some(call.fd()),
        Syscall::Recvmsg(call) => Some(call.sockfd()),
        Syscall::Recvmmsg(call) => Some(call.fd()),
        Syscall::Accept(call) => Some(call.sockfd()),
        Syscall::Accept4(call) => Some(call.sockfd()),
        _ => None,
    };
    if let Some(fd) = received {
        if moved >= 0 && classify(fd).is_some() {
            push(
                thread,
                (0, 0),
                ChannelEventKind::UntrackedRead,
                moved as u64,
            );
        }
        return;
    }
    let untracked: &[i32] = match call {
        Syscall::Splice(call) => &[call.fd_in(), call.fd_out()],
        Syscall::Tee(call) => &[call.fd_in(), call.fd_out()],
        Syscall::Vmsplice(call) => &[call.fd()],
        Syscall::Sendfile(call) => &[call.in_fd(), call.out_fd()],
        _ => &[],
    };
    if moved > 0 {
        for fd in untracked {
            match classify(*fd) {
                Some(DescriptorClass::Pipe(channel)) => {
                    push(thread, channel, ChannelEventKind::Untracked, 0);
                }
                Some(DescriptorClass::Untracked) => {
                    push(thread, (0, 0), ChannelEventKind::UntrackedRead, 0);
                }
                None => {}
            }
        }
    }
}

/// What a grant-awaiting request carries for the report, in a
/// `GlobalRequest::WithWorkSpan` envelope sent only when the tool gathers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSpanPayload {
    /// A poll the current call makes again, because its previous polled
    /// attempt found nothing ready (resource requests only).
    pub(crate) repolls: bool,
    /// The latest polled call ended interrupted (resource requests only).
    pub(crate) interrupted: bool,
    /// The thread dropped events past `MAX_PENDING_EVENTS`.
    pub(crate) overflow: bool,
    /// The pipe events the thread gathered since its previous such request.
    pub(crate) channel_events: Vec<ChannelEvent>,
}

/// The envelope's payload for a resource request, or `None` when the tool
/// does not gather, when it touches nothing and the request goes as it always
/// did. On, it takes the gathered events and flags, and marks a poll
/// (`polls`) that the current call makes again.
pub(crate) fn payload_for(
    on: bool,
    polls: bool,
    thread: &mut WorkSpanThread,
) -> Option<WorkSpanPayload> {
    if !on {
        return None;
    }
    let mut repolls = false;
    if polls {
        repolls = thread.polled;
        thread.polled = true;
    }
    Some(WorkSpanPayload {
        repolls,
        interrupted: std::mem::take(&mut thread.interrupted),
        overflow: std::mem::take(&mut thread.overflow),
        channel_events: std::mem::take(&mut thread.events),
    })
}

/// The envelope's payload for another grant-awaiting request: the pending
/// events, so that every event reaches the scheduler before another thread can
/// commit. `None` when there are none or the tool does not gather.
pub(crate) fn flush_for(on: bool, thread: &mut WorkSpanThread) -> Option<WorkSpanPayload> {
    if !on || (thread.events.is_empty() && !thread.overflow) {
        return None;
    }
    Some(WorkSpanPayload {
        overflow: std::mem::take(&mut thread.overflow),
        channel_events: std::mem::take(&mut thread.events),
        ..WorkSpanPayload::default()
    })
}

/// A pending resource request's facts from its envelope, kept until it
/// commits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RequestFacts {
    repolls: bool,
    interrupted: bool,
}

/// Note `kind` as the binding predecessor if `candidate` exceeds `best`.
fn bind(best: &mut (u64, EdgeKind), candidate: u64, kind: EdgeKind) {
    if candidate > best.0 {
        *best = (candidate, kind);
    }
}

impl Scheduler {
    /// A request from `tid` arrives, carrying guest CPU time `cpu` and, from a
    /// tool that gathers, its envelope's payload. Every request samples the
    /// thread's clock: its first, `StartNewThread` for a child, sets the
    /// baseline, which for a child is the creator's clock it inherited, so
    /// that inherited time is never counted as the child's work.
    /// `resource_request` says whether it is a resource request, whose
    /// envelope facts (`RequestFacts`) belong to its commit.
    pub(crate) fn work_span_note_arrival(
        &mut self,
        tid: DetTid,
        cpu: u64,
        resource_request: bool,
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
        if resource_request {
            let facts = payload.map_or_else(RequestFacts::default, |payload| RequestFacts {
                repolls: payload.repolls,
                interrupted: payload.interrupted,
            });
            if facts == RequestFacts::default() {
                state.request_facts.remove(&tid);
            } else {
                state.request_facts.insert(tid, facts);
            }
        }
        if let Some(payload) = payload {
            if payload.overflow {
                Self::work_span_stop_pipes(state);
            }
            self.work_span_note_events(tid, &payload.channel_events);
        }
    }

    /// Past a memory bound: stop following pipes for the rest of the run.
    /// Their reads then take the conservative edge (`work_span_untracked`).
    fn work_span_stop_pipes(state: &mut WorkSpanState) {
        if state.pipe_tracking_stopped.is_none() {
            state.pipe_tracking_stopped = Some(state.commits);
        }
        state.channels = HashMap::new();
        state.unread_writes = 0;
    }

    /// Stop following `channel`'s stream (`Channel::poisoned`).
    fn work_span_poison(state: &mut WorkSpanState, channel: (u64, u64)) {
        let entry = state.channels.entry(channel).or_default();
        if !entry.poisoned {
            entry.poisoned = true;
            state.channels_poisoned += 1;
            state.unread_writes -= entry.writes.len();
            entry.writes = VecDeque::new();
        }
    }

    /// `tid`'s time at its guest CPU time `cpu`, inside its current segment:
    /// its latest commit (or its start) plus its work since, or later where a
    /// predecessor met earlier in the segment says so.
    fn work_span_time_at(state: &WorkSpanState, tid: DetTid, cpu: u64) -> Bounds {
        let base = state
            .finish
            .get(&tid)
            .or_else(|| state.spawned.get(&tid))
            .copied()
            .unwrap_or_default();
        let committed = state.cpu_committed.get(&tid).copied().unwrap_or(cpu);
        let mut at = base.plus(cpu.saturating_sub(committed));
        if let Some(segment) = state.segment.get(&tid) {
            if let Some((offset, _)) = segment.lo {
                at.lo = at.lo.max(shifted(cpu, offset));
            }
            if let Some((offset, _)) = segment.hi {
                at.hi = at.hi.max(shifted(cpu, offset));
            }
        }
        at
    }

    /// `tid`'s time at its latest sample, in the upper bound: where its
    /// current segment has reached.
    fn work_span_ready_hi(state: &WorkSpanState, tid: DetTid) -> u64 {
        let cpu = state
            .cpu_now
            .get(&tid)
            .or_else(|| state.cpu_committed.get(&tid))
            .copied()
            .unwrap_or(0);
        Self::work_span_time_at(state, tid, cpu).hi
    }

    /// From its guest CPU time `cpu` on, `tid`'s segment follows `pred`: in
    /// both bounds, or (`lo` `None`) in the upper bound only.
    fn work_span_anchor(
        state: &mut WorkSpanState,
        tid: DetTid,
        cpu: u64,
        lo: Option<u64>,
        hi: u64,
        kind: EdgeKind,
    ) {
        let segment = state.segment.entry(tid).or_default();
        let raise = |slot: &mut Option<(i128, EdgeKind)>, at: u64| {
            let offset = i128::from(at) - i128::from(cpu);
            if slot.is_none_or(|(current, _)| offset > current) {
                *slot = Some((offset, kind));
            }
        };
        if let Some(lo) = lo {
            raise(&mut segment.lo, lo);
        }
        raise(&mut segment.hi, hi);
    }

    /// Data `tid` received at guest CPU time `cpu` from a channel the report
    /// cannot follow: in the upper bound, it follows everything that ran,
    /// every other thread's segment as far as it has reached.
    fn work_span_untracked(state: &mut WorkSpanState, tid: DetTid, cpu: u64) {
        let everything = state
            .cpu_committed
            .keys()
            .filter(|thread| **thread != tid)
            .map(|thread| Self::work_span_ready_hi(state, *thread))
            .max()
            .unwrap_or(0);
        state.fallback_edges += 1;
        Self::work_span_anchor(state, tid, cpu, None, everything, EdgeKind::Untracked);
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
        state.segment.remove(&child);
    }

    /// The events `tid` gathered since its previous grant-awaiting request.
    /// Each is timed inside the thread's current segment
    /// (`work_span_time_at`), so a write made after a read the thread waited
    /// on carries that read's predecessor. A write enters its pipe's stream; a
    /// read follows every write that supplied its bytes. A thread's events
    /// arrive with its next grant-awaiting request, before the scheduler can
    /// let another thread commit; a write that arrives late anyway (made
    /// outside the schedule, in a background call) is found when a read goes
    /// past every write seen, which poisons the pipe rather than pairing the
    /// read wrongly. A read that cannot be matched takes the conservative
    /// edge.
    fn work_span_note_events(&mut self, tid: DetTid, events: &[ChannelEvent]) {
        for event in events {
            let state = &mut self.work_span;
            let stopped = state.pipe_tracking_stopped.is_some();
            if !stopped
                && ((matches!(event.kind, ChannelEventKind::Write | ChannelEventKind::Read)
                    && !state.channels.contains_key(&event.channel)
                    && state.channels.len() >= MAX_CHANNELS)
                    || (event.kind == ChannelEventKind::Write
                        && state.unread_writes >= MAX_UNREAD_WRITES))
            {
                Self::work_span_stop_pipes(state);
            }
            let stopped = state.pipe_tracking_stopped.is_some();
            match event.kind {
                ChannelEventKind::SignalSent => {
                    let at = Self::work_span_time_at(state, tid, event.cpu).hi;
                    state.signal_sent = Some(state.signal_sent.unwrap_or(0).max(at));
                }
                ChannelEventKind::UntrackedRead => {
                    Self::work_span_untracked(state, tid, event.cpu);
                }
                ChannelEventKind::Untracked => {
                    if !stopped {
                        Self::work_span_poison(state, event.channel);
                    }
                    Self::work_span_untracked(state, tid, event.cpu);
                }
                ChannelEventKind::Write => {
                    if stopped {
                        continue;
                    }
                    let at = Self::work_span_time_at(state, tid, event.cpu);
                    let channel = state.channels.entry(event.channel).or_default();
                    if channel.poisoned {
                        continue;
                    }
                    channel.written += event.bytes;
                    channel.writes.push_back((channel.written, at));
                    state.unread_writes += 1;
                }
                ChannelEventKind::Read => {
                    if stopped {
                        state.pipe_reads_unmatched += 1;
                        Self::work_span_untracked(state, tid, event.cpu);
                        continue;
                    }
                    state
                        .read_channels
                        .entry(tid)
                        .or_default()
                        .insert(event.channel);
                    if event.bytes == 0 {
                        // An end of file: every write end closed, by closes
                        // the report does not gather.
                        Self::work_span_untracked(state, tid, event.cpu);
                        continue;
                    }
                    let channel = state.channels.entry(event.channel).or_default();
                    let first = channel.read;
                    let last = first + event.bytes;
                    channel.read = last;
                    if channel.poisoned || last > channel.written {
                        state.pipe_reads_unmatched += 1;
                        Self::work_span_poison(state, event.channel);
                        Self::work_span_untracked(state, tid, event.cpu);
                        continue;
                    }
                    // Every write ending past `first` supplied bytes of this
                    // read: those ending before `last` wholly, and the one
                    // reaching `last`.
                    let mut supplied: Option<Bounds> = None;
                    let mut consumed = 0;
                    while let Some((end, written)) = channel.writes.front().copied() {
                        supplied = Some(supplied.unwrap_or(written).max(written));
                        if end <= last {
                            channel.writes.pop_front();
                            consumed += 1;
                        }
                        if end >= last {
                            break;
                        }
                    }
                    state.unread_writes -= consumed;
                    let supplied = supplied.expect("the writes seen reach `last`");
                    Self::work_span_anchor(
                        state,
                        tid,
                        event.cpu,
                        Some(supplied.lo),
                        supplied.hi,
                        EdgeKind::Pipe,
                    );
                    if event.polled {
                        state.poll_explained.insert(tid);
                    }
                    state.pipe_reads_matched += 1;
                }
            }
        }
    }

    /// `reaper`'s wait reaped `child` of `parent` (`GlobalRequest::ConsumeChildWait`),
    /// with the reaper's guest CPU time `cpu` at the reap: from there on, it
    /// follows the child's exit.
    pub(crate) fn work_span_note_reap(
        &mut self,
        reaper: DetTid,
        cpu: u64,
        parent: DetPid,
        child: DetPid,
    ) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        if let Some(exit) = state
            .exited
            .get_mut(&parent)
            .and_then(|children| children.remove(&child))
        {
            Self::work_span_anchor(state, reaper, cpu, Some(exit.lo), exit.hi, EdgeKind::Reap);
            state.poll_explained.insert(reaper);
        }
    }

    /// `child` of `parent` was reaped, by a wait or automatically
    /// (`Scheduler::consume_child_wait`): its exit is forgotten.
    pub(super) fn work_span_forget_exit(&mut self, parent: DetPid, child: DetPid) {
        if let Some(children) = self.work_span.exited.get_mut(&parent) {
            children.remove(&child);
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

    /// The latest possible cause of a `SIGCHLD` to `process`, in the upper
    /// bound: its latest child exit, or the latest signal the guest sent
    /// (which covers a `kill` of it and a child stopped by a signal). `None`
    /// when neither has happened.
    fn work_span_sigchld_cause(state: &WorkSpanState, process: Option<DetPid>) -> Option<u64> {
        let exit = process
            .and_then(|process| state.latest_child_exit.get(&process))
            .map(|exit| exit.hi);
        match (exit, state.signal_sent) {
            (None, None) => None,
            (exit, sent) => Some(exit.unwrap_or(0).max(sent.unwrap_or(0))),
        }
    }

    /// `tid`'s turn commits with request `request`.
    pub(super) fn work_span_commit(&mut self, tid: DetTid, request: Option<&Resources>) {
        if !self.work_span.enabled {
            return;
        }
        // The process this commit's exit ends, that process's parent, and the
        // parent's children whose exits still wait to be reaped.
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
                let waitable: HashSet<DetPid> = self
                    .thread_tree
                    .unreaped_children_of(parent)
                    .into_iter()
                    .filter(|child| {
                        self.logically_exited_processes.contains(child)
                            && !self.autoreaped_exits.contains(child)
                    })
                    .collect();
                Some((process, parent, waitable))
            });
        let facts = self
            .work_span
            .request_facts
            .remove(&tid)
            .unwrap_or_default();
        let made_progress = request.is_some_and(|request| request.previous_attempt_made_progress);
        let process = self.thread_tree.process_of(tid);
        // The other threads of the process, which share its memory.
        let siblings: Vec<DetTid> = process
            .map(|process| {
                self.thread_tree
                    .my_thread_group(&process)
                    .into_iter()
                    .filter(|other| *other != tid)
                    .collect()
            })
            .unwrap_or_default();
        // The other threads whose requests wait outside the run queue: parked
        // or blocked (a futex waiter, a blocked resource, a parked child
        // wait). Whatever this commit does may be what they wait for.
        let blocked: Vec<(DetTid, WaitKind)> = self
            .next_turns
            .iter()
            .filter(|(other, _)| **other != tid && !self.run_queue.contains_tid(**other))
            .filter_map(|(other, turn)| {
                let kind = turn
                    .req
                    .try_read_with(|request| request.as_ref().ok().and_then(wait_kind))
                    .flatten()?;
                Some((*other, kind))
            })
            .collect();
        let state = &mut self.work_span;
        let committed = match state.cpu_committed.get(&tid) {
            Some(committed) => *committed,
            None => {
                state.threads += 1;
                state.cpu_now.get(&tid).copied().unwrap_or(0)
            }
        };
        // No sample since the baseline (a child's first turn): no work yet.
        let cpu = state.cpu_now.get(&tid).copied().unwrap_or(committed);
        let weight = cpu.saturating_sub(committed);
        let shared_memory = siblings
            .iter()
            .map(|sibling| Self::work_span_ready_hi(state, *sibling))
            .max();
        let segment = state.segment.remove(&tid).unwrap_or_default();
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
        if let Some((offset, kind)) = segment.lo {
            bind(&mut lo, shifted(cpu, offset), kind);
        }
        if let Some((offset, kind)) = segment.hi {
            bind(&mut hi, shifted(cpu, offset), kind);
        }
        if let Some(shared) = shared_memory {
            bind(&mut hi, shared, EdgeKind::SharedMemory);
        }

        // Which kind of wait a `Waited` binding of this commit came from.
        let mut waited_kind = None;
        let explained = state.poll_explained.remove(&tid);
        // A polled call the tool saw end interrupted, followed by this
        // commit's `SIGCHLD` delivery: a child's exit or a guest's signal
        // ended it.
        let delivers_sigchld = request.is_some_and(|request| {
            request.resources.keys().any(|resource| {
                matches!(resource, ResourceID::InboundSignal(signal) if signal.raw() == libc::SIGCHLD)
            })
        });
        let sigchld_cause = Self::work_span_sigchld_cause(state, process);
        // A poll the call makes again after an attempt that found nothing
        // ready and moved nothing observed no one.
        let fruitless_repoll = facts.repolls && !made_progress;
        if let Some((deferred, kind)) = state.deferred_poll.remove(&tid)
            && !explained
            && !fruitless_repoll
        {
            match sigchld_cause.filter(|_| facts.interrupted && delivers_sigchld) {
                Some(cause) => bind(&mut hi, cause, EdgeKind::ChildSignal),
                None if deferred + weight > hi.0 => {
                    hi = (deferred + weight, EdgeKind::Waited);
                    waited_kind = Some(kind);
                }
                None => {}
            }
        }
        match state.waiting.remove(&tid) {
            Some((time, kind @ (WaitKind::IoPoll | WaitKind::ChildWait))) => {
                // The polled or woken call runs after this commit; its result
                // decides.
                state.deferred_poll.insert(tid, (time, kind));
            }
            Some((time, WaitKind::ChildSignal)) => match sigchld_cause {
                Some(cause) => bind(&mut hi, cause, EdgeKind::ChildSignal),
                None if time > hi.0 => {
                    hi = (time, EdgeKind::Waited);
                    waited_kind = Some(WaitKind::ChildSignal);
                }
                None => {}
            },
            Some((time, kind)) if time > hi.0 => {
                hi = (time, EdgeKind::Waited);
                waited_kind = Some(kind);
            }
            Some(_) | None => {}
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
            // A resource seen again after a clear may have a forgotten
            // writer: until a write supersedes it, the upper bound keeps the
            // floor as its write.
            let floor = state.resource_floor.hi;
            let access = state.resources.entry(key.clone()).or_insert(Access {
                write: Bounds { lo: 0, hi: floor },
                reads: Bounds::default(),
            });
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
        for (other, kind) in blocked {
            state.waiting.entry(other).or_insert((0, kind));
        }
        for (waited, _) in state.waiting.values_mut() {
            *waited = (*waited).max(commit.hi);
        }
        if request.is_some_and(|request| request.resources.keys().any(goes_outside)) {
            state.waiting.insert(tid, (0, WaitKind::Outside));
        }
        if request.is_some_and(|request| {
            request
                .resources
                .keys()
                .any(|resource| matches!(resource, ResourceID::Exit { .. }))
        }) {
            state.exited_threads.insert(tid);
        } else {
            state.exited_threads.remove(&tid);
        }
        if let Some((process, parent, waitable)) = ends {
            let exited = state.exited.entry(parent).or_default();
            // Children reaped some other way (automatically, or by a
            // subreaper) go.
            exited.retain(|child, _| waitable.contains(child));
            exited.insert(process, commit);
            while exited.len() > MAX_EXITED_PER_PARENT {
                exited.pop_first();
            }
            let latest = state.latest_child_exit.entry(parent).or_default();
            *latest = latest.max(commit);
        }
    }

    /// The scheduler retires `tid` (`Scheduler::logically_kill_thread`): it
    /// commits no more, so its state goes, and with its process's last thread,
    /// the process's state as a parent. A thread retired mid-turn
    /// (`mid_turn`: no request pending, so it was running or in a background
    /// call) may have read pipes since its last request, reads the report will
    /// never see; the channels it has read are poisoned. A thread retired with
    /// a request pending sent its events with that request, as did one whose
    /// last commit was its own exit.
    pub(super) fn work_span_retire_thread(
        &mut self,
        tid: DetTid,
        process: DetPid,
        last_of_process: bool,
        mid_turn: bool,
    ) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        let exited = state.exited_threads.remove(&tid);
        if let Some(channels) = state.read_channels.remove(&tid)
            && mid_turn
            && !exited
        {
            for channel in channels {
                Self::work_span_poison(state, channel);
            }
        }
        state.segment.remove(&tid);
        state.cpu_now.remove(&tid);
        state.cpu_committed.remove(&tid);
        state.finish.remove(&tid);
        state.spawned.remove(&tid);
        state.waiting.remove(&tid);
        state.poll_explained.remove(&tid);
        state.deferred_poll.remove(&tid);
        state.request_facts.remove(&tid);
        if last_of_process {
            state.exited.remove(&process);
            state.latest_child_exit.remove(&process);
        }
    }

    /// A non-leader `caller` exec'd and survives as the process's `leader`
    /// (`Scheduler::reconnect_exec`): its timeline goes on under the leader's
    /// tid.
    pub(super) fn work_span_transfer_thread(&mut self, caller: DetTid, leader: DetTid) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        fn rekey<V>(map: &mut HashMap<DetTid, V>, caller: DetTid, leader: DetTid) {
            map.remove(&leader);
            if let Some(value) = map.remove(&caller) {
                map.insert(leader, value);
            }
        }
        rekey(&mut state.cpu_now, caller, leader);
        rekey(&mut state.cpu_committed, caller, leader);
        rekey(&mut state.finish, caller, leader);
        rekey(&mut state.spawned, caller, leader);
        rekey(&mut state.waiting, caller, leader);
        rekey(&mut state.segment, caller, leader);
        rekey(&mut state.deferred_poll, caller, leader);
        state.exited_threads.remove(&leader);
        rekey(&mut state.request_facts, caller, leader);
        rekey(&mut state.read_channels, caller, leader);
        state.poll_explained.remove(&leader);
        if state.poll_explained.remove(&caller) {
            state.poll_explained.insert(leader);
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
            channels_poisoned: state.channels_poisoned,
            fallback_edges: state.fallback_edges,
            upper_bound_maintained: state.caveats.is_empty(),
            upper_bound_caveats: state.caveats.iter().copied().collect(),
            unobserved: UNOBSERVED.to_vec(),
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
    use reverie::syscalls::Errno;
    use reverie::syscalls::Syscall;

    use super::*;
    use crate::config::Config;
    use crate::resources::Device;
    use crate::types::FutexID;
    use crate::types::LogicalTime;
    use crate::types::MmId;
    use crate::types::SigWrapper;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    fn pid(raw: i32) -> DetPid {
        DetPid::from_raw(raw)
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
            register(&mut sched, child);
        }
        sched
    }

    fn register(sched: &mut Scheduler, thread: i32) {
        sched.next_turns.insert(
            tid(thread),
            super::super::ThreadNextTurn {
                dettid: tid(thread),
                child_tid_addr: 0,
                req: crate::ivar::Ivar::new(),
                resp: crate::ivar::Ivar::new(),
                protocol: Default::default(),
            },
        );
    }

    fn request(thread: i32, resource: Option<(ResourceID, Permission)>) -> Resources {
        let mut request = Resources::new(tid(thread));
        if let Some((resource, permission)) = resource {
            request.insert(resource, permission);
        }
        request
    }

    /// A resource request from `thread`, which has used `cpu` in all, with
    /// envelope `payload`.
    fn arrive(sched: &mut Scheduler, thread: i32, cpu: u64, payload: Option<&WorkSpanPayload>) {
        sched.work_span_note_arrival(tid(thread), cpu, true, payload);
    }

    /// `thread` has used `cpu` in all, and its turn commits with `resource`.
    /// An exit leaves a zombie, as the scheduler's logical exit does.
    fn commit(
        sched: &mut Scheduler,
        thread: i32,
        cpu: u64,
        resource: Option<(ResourceID, Permission)>,
    ) {
        let exits = matches!(resource, Some((ResourceID::Exit { .. }, _)));
        arrive(sched, thread, cpu, None);
        sched.work_span_commit(tid(thread), Some(&request(thread, resource)));
        if exits {
            sched.logically_exited_processes.insert(pid(thread));
        }
    }

    fn events(events: Vec<ChannelEvent>) -> WorkSpanPayload {
        WorkSpanPayload {
            channel_events: events,
            ..WorkSpanPayload::default()
        }
    }

    fn event(kind: ChannelEventKind, bytes: u64, polled: bool, cpu: u64) -> ChannelEvent {
        ChannelEvent {
            channel: (7, 42),
            kind,
            bytes,
            polled,
            cpu,
        }
    }

    use ChannelEventKind::Read;
    use ChannelEventKind::SignalSent;
    use ChannelEventKind::Untracked;
    use ChannelEventKind::UntrackedRead;
    use ChannelEventKind::Write;

    fn exit(thread: i32) -> Option<(ResourceID, Permission)> {
        Some((
            ResourceID::Exit {
                group: true,
                process: pid(thread),
                mm: MmId::initial(tid(thread)),
            },
            Permission::W,
        ))
    }

    fn poll(thread: i32) -> Resources {
        request(thread, Some((ResourceID::InternalIOPolling, Permission::W)))
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
        // The parent's wait reaps 2 at its 112, then 3 at its 125.
        commit(&mut sched, 1, 110, None);
        sched.work_span_note_reap(tid(1), 112, pid(1), pid(2));
        commit(&mut sched, 1, 120, None);
        assert_eq!(finish(&sched, 1), (1_108, 1_108), "2's exit, then 8 more");
        sched.work_span_note_reap(tid(1), 125, pid(1), pid(3));
        commit(&mut sched, 1, 170, None);
        assert_eq!(finish(&sched, 1), (1_158, 1_158));
        assert_eq!(report(&sched), (1_470, 1_158, 1_158));
    }

    /// A wait follows the child it reaped, whichever that is, and a wait that
    /// reaped nothing follows no exit (the review's `waitpid(X)` case).
    #[test]
    fn a_wait_follows_the_child_it_reaped() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 100, exit(2)); // Y exits at 100
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 1_000, exit(3)); // X exits at 1,000
        sched.work_span_note_reap(tid(1), 0, pid(1), pid(3));
        commit(&mut sched, 1, 500, None);
        assert_eq!(finish(&sched, 1), (1_500, 1_500), "X's exit, then 500");
        commit(&mut sched, 1, 600, None);
        assert_eq!(finish(&sched, 1), (1_600, 1_600), "Y is not reaped");
        assert!(sched.work_span.exited[&pid(1)].contains_key(&pid(2)));
        sched.consume_child_wait(pid(1), pid(2));
        assert!(!sched.work_span.exited[&pid(1)].contains_key(&pid(2)));
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

    /// A futex waiter parked by the scheduler (`sleep_futex_waiter`, never
    /// through step 4) follows, in the upper bound, every commit made while it
    /// was parked: A works 1,000 and wakes B, which works 1,000 more.
    #[test]
    fn a_parked_futex_waiter_follows_what_ran_while_it_slept() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let futex = FutexID::private(MmId::initial(tid(1)), 0x40);
        sched.sleep_futex_waiter(&tid(1), futex, None, u32::MAX, None);
        commit(&mut sched, 2, 1_000, None);
        let wait = request(1, Some((ResourceID::FutexWait, Permission::R)));
        arrive(&mut sched, 1, 0, None);
        sched.work_span_commit(tid(1), Some(&wait));
        commit(&mut sched, 1, 1_000, None);
        assert_eq!(finish(&sched, 1), (1_000, 2_000));
        assert_eq!(sched.work_span.binding_waited[&WaitKind::Futex], 1);
    }

    /// A sleep and other scheduler-only waits order nothing.
    #[test]
    fn a_sleep_waits_on_no_one() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let sleep = ResourceID::SleepUntil(LogicalTime::from_nanos(1));
        sched.work_span_note_wait(tid(1), &request(1, Some((sleep, Permission::R))));
        commit(&mut sched, 2, 2_000, None);
        commit(&mut sched, 1, 10, None);
        assert_eq!(finish(&sched, 1), (10, 10));
    }

    /// A child's clock starts at its creator's: that inherited time, which its
    /// first request (`StartNewThread`) carries, is not the child's work.
    #[test]
    fn a_childs_inherited_clock_is_not_its_work() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 1, 5_000, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_arrival(tid(2), 5_000, false, None);
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        commit(&mut sched, 2, 5_050, None);
        assert_eq!(
            report(&sched).0,
            5_050,
            "the parent's 5,000 and the child's 50"
        );
        assert_eq!(finish(&sched, 2), (5_050, 5_050));
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
        let writes = events(vec![
            event(Write, 3, false, 300),
            event(Write, 2, false, 700),
        ]);
        arrive(&mut sched, 2, 1_000, Some(&writes));
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        // Thread 1 reads 4 bytes at its 10 (the 4th byte is the second
        // write's, at 700), then works to 50 before its request.
        arrive(
            &mut sched,
            1,
            50,
            Some(&events(vec![event(Read, 4, false, 10)])),
        );
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(finish(&sched, 1), (740, 740));
        assert_eq!(sched.work_span.pipe_reads_matched, 1);
    }

    /// A read past every write seen (a write that arrives late, or one the
    /// report never saw) poisons the pipe and takes the conservative edge,
    /// as every later read of it does: no wrong pairing can lower the upper
    /// bound or invent a lower-bound edge. A call the report does not count
    /// poisons the pipe too.
    #[test]
    fn a_stream_the_report_cannot_follow_is_poisoned() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 900, None);
        arrive(
            &mut sched,
            1,
            10,
            Some(&events(vec![event(Read, 4, false, 5)])),
        );
        assert!(sched.work_span.channels[&(7, 42)].poisoned);
        // The late write, then a polled read of new data: still unmatched.
        arrive(
            &mut sched,
            2,
            920,
            Some(&events(vec![event(Write, 6, false, 910)])),
        );
        arrive(
            &mut sched,
            1,
            30,
            Some(&events(vec![event(Read, 2, true, 25)])),
        );
        assert!(sched.work_span.poll_explained.is_empty());
        assert_eq!(sched.work_span.pipe_reads_unmatched, 2);
        assert_eq!(sched.work_span.channels_poisoned, 1);
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        // No lower-bound edge; in the upper bound, everything that ran
        // (thread 2 at 920) plus the 5 units since the later read.
        assert_eq!(finish(&sched, 1), (30, 925));

        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        let untracked = events(vec![
            event(Write, 3, false, 1),
            event(Untracked, 0, false, 2),
        ]);
        arrive(&mut sched, 1, 5, Some(&untracked));
        arrive(
            &mut sched,
            1,
            6,
            Some(&events(vec![event(Read, 3, true, 6)])),
        );
        assert!(sched.work_span.poll_explained.is_empty());
        assert_eq!(sched.work_span.unread_writes, 0);
    }

    /// A polled read explained by a pipe edge takes no waiting edge in the
    /// upper bound; a polled call that is not one keeps it; an attempt that
    /// found nothing and polls again takes none.
    #[test]
    fn an_explained_poll_drops_its_waiting_edge() {
        for explained in [true, false] {
            let mut sched = family();
            sched.work_span_note_spawn(tid(1), tid(2));
            sched.work_span_note_spawn(tid(1), tid(3));
            for thread in [1, 2, 3] {
                commit(&mut sched, thread, 0, None);
            }
            sched.work_span_note_wait(tid(1), &poll(1));
            let write = events(vec![event(Write, 1, false, 100)]);
            arrive(&mut sched, 2, 100, explained.then_some(&write));
            sched.work_span_commit(tid(2), Some(&request(2, None)));
            commit(&mut sched, 3, 5_000, None);
            // The polled retry's grant, then the read's result on the next request.
            arrive(&mut sched, 1, 0, None);
            sched.work_span_commit(tid(1), Some(&poll(1)));
            let read = events(vec![event(Read, 1, true, 1)]);
            arrive(&mut sched, 1, 20, explained.then_some(&read));
            sched.work_span_commit(tid(1), Some(&request(1, None)));
            let expected = if explained { (119, 119) } else { (20, 5_020) };
            assert_eq!(finish(&sched, 1), expected, "explained: {explained}");
        }

        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        sched.work_span_note_wait(tid(1), &poll(1));
        commit(&mut sched, 2, 5_000, None);
        arrive(&mut sched, 1, 0, None);
        sched.work_span_commit(tid(1), Some(&poll(1)));
        let again = WorkSpanPayload {
            repolls: true,
            ..WorkSpanPayload::default()
        };
        arrive(&mut sched, 1, 20, Some(&again));
        sched.work_span_commit(tid(1), Some(&poll(1)));
        assert_eq!(finish(&sched, 1), (20, 20));
    }

    /// A polled wait for a child is explained by the child it reaped.
    #[test]
    fn a_polled_wait_is_explained_by_its_reap() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 3, 0, None);
        sched.work_span_note_wait(tid(1), &poll(1));
        commit(&mut sched, 2, 100, exit(2));
        commit(&mut sched, 3, 5_000, None);
        arrive(&mut sched, 1, 0, None);
        sched.work_span_commit(tid(1), Some(&poll(1)));
        sched.work_span_note_reap(tid(1), 1, pid(1), pid(2));
        commit(&mut sched, 1, 2, None);
        assert_eq!(
            finish(&sched, 1),
            (101, 101),
            "child 3's work does not order it"
        );
        assert!(sched.work_span.binding_waited.is_empty());
    }

    /// A polled call follows the latest possible cause of a `SIGCHLD` when
    /// the tool saw it end interrupted and its `SIGCHLD` delivery is the next
    /// commit: the latest child exit, or a later signal the guest sent. It
    /// keeps its waiting edge otherwise, and when no cause has happened.
    #[test]
    fn an_interrupted_poll_follows_the_latest_sigchld_cause() {
        let delivery = request(
            1,
            Some((
                ResourceID::InboundSignal(SigWrapper(libc::SIGCHLD)),
                Permission::RW,
            )),
        );
        // (interrupted, child exit, guest kill at) -> expected upper bound
        for (interrupted, exits, kill, expected) in [
            (false, true, None, 9_000),
            (true, true, None, 300),
            (true, true, Some(700), 700),
            (true, false, Some(700), 700),
            (true, false, None, 9_000),
        ] {
            let mut sched = family();
            sched.work_span_note_spawn(tid(1), tid(2));
            sched.work_span_note_spawn(tid(1), tid(3));
            for thread in [1, 2, 3] {
                commit(&mut sched, thread, 0, None);
            }
            if exits {
                commit(&mut sched, 2, 300, exit(2));
            }
            if let Some(at) = kill {
                let sent = events(vec![event(SignalSent, 0, false, at)]);
                arrive(&mut sched, 3, at, Some(&sent));
                sched.work_span_commit(tid(3), Some(&request(3, None)));
            }
            sched.work_span_note_wait(tid(1), &poll(1));
            commit(&mut sched, 3, 9_000, None);
            arrive(&mut sched, 1, 0, None);
            sched.work_span_commit(tid(1), Some(&poll(1)));
            let facts = WorkSpanPayload {
                interrupted,
                ..WorkSpanPayload::default()
            };
            arrive(&mut sched, 1, 0, Some(&facts));
            sched.work_span_commit(tid(1), Some(&delivery));
            assert_eq!(
                finish(&sched, 1),
                (0, expected),
                "interrupted {interrupted}, exit {exits}, kill {kill:?}"
            );
        }
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
        arrive(&mut sched, 1, 0, None);
        sched.work_span_commit(tid(1), Some(&delivery));
        assert_eq!(finish(&sched, 1), (0, 300));
    }

    /// A non-leader that execs goes on as the leader with its own timeline.
    #[test]
    fn an_exec_keeps_the_callers_timeline() {
        let mut sched = family();
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 500, None);
        register(&mut sched, 7);
        sched.work_span_transfer_thread(tid(2), tid(7));
        assert!(!sched.work_span.finish.contains_key(&tid(2)));
        commit(&mut sched, 7, 520, None);
        assert_eq!(finish(&sched, 7), (520, 520));
    }

    /// Off, the tool gathers nothing: it neither stats a descriptor nor reads
    /// the clock, whatever the call. On, a pipe read or write gives an event,
    /// data from an untracked channel gives one, an untracked call on a pipe
    /// gives one, a signal sent gives one, an interrupted polled call is
    /// marked, and past `MAX_PENDING_EVENTS` the overflow is marked instead.
    #[test]
    fn off_the_tool_never_stats_or_gathers() {
        let read = Syscall::Read(reverie::syscalls::Read::new().with_fd(3).with_len(8));
        let write = Syscall::Write(reverie::syscalls::Write::new().with_fd(4).with_len(8));
        let socket_read = Syscall::Read(reverie::syscalls::Read::new().with_fd(6).with_len(8));
        let file_read = Syscall::Read(reverie::syscalls::Read::new().with_fd(5).with_len(8));
        let splice = Syscall::Splice(
            reverie::syscalls::Splice::new()
                .with_fd_in(3)
                .with_fd_out(5),
        );
        let kill = Syscall::Kill(reverie::syscalls::Kill::new());
        let mut thread = WorkSpanThread {
            polled: true,
            ..WorkSpanThread::default()
        };
        for (call, res) in [
            (&read, Ok(8)),
            (&read, Ok(0)),
            (&write, Ok(8)),
            (&splice, Ok(8)),
            (&kill, Ok(0)),
            (&read, Err(reverie::Error::Errno(Errno::EINTR))),
        ] {
            gather(
                false,
                call,
                &res,
                &mut thread,
                |_| panic!("stat with the report off"),
                || panic!("clock read with the report off"),
            );
        }
        assert_eq!(
            thread,
            WorkSpanThread {
                polled: true,
                ..WorkSpanThread::default()
            },
            "untouched"
        );

        // 3 and 4 are pipes, 6 a socket, 5 a regular file.
        let classify = |fd: i32| match fd {
            3 | 4 => Some(DescriptorClass::Pipe((9, fd as u64))),
            6 => Some(DescriptorClass::Untracked),
            _ => None,
        };
        gather(true, &read, &Ok(8), &mut thread, classify, || 77);
        gather(true, &write, &Ok(0), &mut thread, classify, || 78);
        gather(true, &read, &Ok(0), &mut thread, classify, || 79);
        gather(true, &socket_read, &Ok(8), &mut thread, classify, || 80);
        gather(true, &file_read, &Ok(8), &mut thread, classify, || 81);
        gather(true, &splice, &Ok(8), &mut thread, classify, || 82);
        gather(true, &kill, &Ok(0), &mut thread, classify, || 83);
        let kinds: Vec<_> = thread
            .events
            .iter()
            .map(|e| (e.kind, e.bytes, e.polled))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (Read, 8, true),
                (Read, 0, false),
                (UntrackedRead, 8, false),
                (Untracked, 0, false),
                (SignalSent, 0, false),
            ],
            "a write that moved nothing and a regular file are no event"
        );
        thread.polled = true;
        gather(
            true,
            &read,
            &Err(reverie::Error::Errno(Errno::ERESTARTSYS)),
            &mut thread,
            classify,
            || 84,
        );
        assert!(thread.interrupted && !thread.polled);

        let mut full = WorkSpanThread::default();
        for _ in 0..=MAX_PENDING_EVENTS {
            gather(true, &read, &Ok(1), &mut full, classify, || 1);
        }
        assert_eq!(full.events.len(), MAX_PENDING_EVENTS);
        assert!(full.overflow);
    }

    /// Off, there is no envelope and nothing is taken, so the request goes as
    /// it always did (`Resources` is unchanged). On, the envelope carries the
    /// events and flags and marks a repeated poll; another grant-awaiting
    /// request carries only pending events; and the envelope survives the
    /// RPC's own encoding (bincode, legacy configuration, as reverie's debug
    /// validation encodes every request).
    #[test]
    fn off_a_request_goes_without_an_envelope() {
        let pending = || WorkSpanThread {
            events: vec![event(Write, 1, false, 5)],
            polled: false,
            interrupted: true,
            overflow: false,
        };
        let mut thread = pending();
        assert_eq!(payload_for(false, true, &mut thread), None);
        assert_eq!(flush_for(false, &mut thread), None);
        assert_eq!(thread, pending(), "untouched");

        let first = payload_for(true, true, &mut thread).unwrap();
        assert_eq!(
            first,
            WorkSpanPayload {
                repolls: false,
                interrupted: true,
                overflow: false,
                channel_events: vec![event(Write, 1, false, 5)],
            }
        );
        assert!(thread.events.is_empty() && thread.polled && !thread.interrupted);
        assert!(payload_for(true, true, &mut thread).unwrap().repolls);

        let mut thread = pending();
        let flushed = flush_for(true, &mut thread).unwrap();
        assert_eq!(flushed.channel_events.len(), 1);
        assert!(
            !flushed.interrupted && thread.interrupted,
            "flags wait for a resource request"
        );
        assert_eq!(flush_for(true, &mut thread), None, "nothing pending");

        use crate::tool_global::GlobalRequest;
        let wrapped = GlobalRequest::WithWorkSpan(
            Box::new(first),
            Box::new(GlobalRequest::RequestResources(poll(1), pid(1))),
        );
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&wrapped, config).unwrap();
        let (decoded, _): (GlobalRequest, usize) =
            bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, wrapped);
    }

    /// A retired thread's state goes, and with its process's last thread, the
    /// process's state as a parent. A thread retired mid-turn poisons the
    /// channels it read; one retired with a request pending does not.
    #[test]
    fn retirement_frees_a_threads_state() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 10, exit(2));
        assert!(!sched.work_span.exited.is_empty());
        sched.work_span_retire_thread(tid(2), pid(2), true, false);
        sched.work_span_retire_thread(tid(1), pid(1), true, false);
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
        arrive(&mut sched, 2, 50, None);
        assert!(sched.work_span.cpu_now.is_empty());

        for mid_turn in [false, true] {
            let mut sched = family();
            commit(&mut sched, 1, 0, None);
            arrive(
                &mut sched,
                1,
                1,
                Some(&events(vec![event(Write, 2, false, 1)])),
            );
            arrive(
                &mut sched,
                3,
                1,
                Some(&events(vec![event(Read, 1, false, 1)])),
            );
            sched.work_span_retire_thread(tid(3), pid(3), true, mid_turn);
            assert_eq!(sched.work_span.channels[&(7, 42)].poisoned, mid_turn);
        }
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
        // An independent second reader also follows the forgotten write.
        sched.work_span_note_spawn(tid(9), tid(3));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 5, Some((path(0), Permission::R)));
        commit(&mut sched, 3, 1_005, None);
        assert_eq!(finish(&sched, 3), (1_005, 2_000));
    }

    /// Past `MAX_UNREAD_WRITES`, or when a thread's buffer overflowed, pipe
    /// edges stop for the rest of the run and their memory goes; reads then
    /// take the conservative edge, so the upper bound stays an upper bound.
    #[test]
    fn pipe_tracking_is_bounded() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let writes: Vec<ChannelEvent> = (0..=MAX_UNREAD_WRITES as u64)
            .map(|n| event(Write, 1, false, n))
            .collect();
        arrive(&mut sched, 2, 1_000, Some(&events(writes)));
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        assert!(sched.work_span.pipe_tracking_stopped.is_some());
        assert!(sched.work_span.channels.is_empty());
        // Thread 1 reads immediately (no wait) and works 1,000.
        arrive(
            &mut sched,
            1,
            1_001,
            Some(&events(vec![event(Read, 1, false, 1)])),
        );
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(finish(&sched, 1), (1_001, 2_000), "after thread 2's 1,000");
        assert_eq!(sched.work_span.pipe_reads_unmatched, 1);

        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        let overflowed = WorkSpanPayload {
            overflow: true,
            ..WorkSpanPayload::default()
        };
        arrive(&mut sched, 1, 1, Some(&overflowed));
        assert!(sched.work_span.pipe_tracking_stopped.is_some());
    }

    /// A read that takes bytes from several writes follows all of them, the
    /// latest-finishing first in time even when it supplied the earliest
    /// bytes.
    #[test]
    fn a_read_follows_every_write_that_supplied_it() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for thread in [1, 2, 3] {
            commit(&mut sched, thread, 0, None);
        }
        // Thread 2 computes 1,000 and writes; thread 3 then writes at once.
        arrive(
            &mut sched,
            2,
            1_000,
            Some(&events(vec![event(Write, 2, false, 1_000)])),
        );
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        arrive(
            &mut sched,
            3,
            1,
            Some(&events(vec![event(Write, 2, false, 1)])),
        );
        sched.work_span_commit(tid(3), Some(&request(3, None)));
        // Thread 1 reads all 4 bytes at once, then computes 1,000.
        arrive(
            &mut sched,
            1,
            1_000,
            Some(&events(vec![event(Read, 4, false, 0)])),
        );
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(finish(&sched, 1), (2_000, 2_000));
    }

    /// An event's time includes what the thread met earlier in the same
    /// segment: a writer that read from a slow producer passes the producer's
    /// time on to its own reader.
    #[test]
    fn an_events_time_includes_its_segments_predecessors() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for thread in [1, 2, 3] {
            commit(&mut sched, thread, 0, None);
        }
        let producer = (7, 1);
        let relay = (7, 2);
        let on = |channel, kind, bytes, cpu| ChannelEvent {
            channel,
            kind,
            bytes,
            polled: false,
            cpu,
        };
        arrive(
            &mut sched,
            2,
            1_000,
            Some(&events(vec![on(producer, Write, 1, 1_000)])),
        );
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        // Thread 3 reads it at its 1 and passes a byte on at its 2.
        let relayed = events(vec![on(producer, Read, 1, 1), on(relay, Write, 1, 2)]);
        arrive(&mut sched, 3, 2, Some(&relayed));
        sched.work_span_commit(tid(3), Some(&request(3, None)));
        arrive(
            &mut sched,
            1,
            1_000,
            Some(&events(vec![on(relay, Read, 1, 0)])),
        );
        sched.work_span_commit(tid(1), Some(&request(1, None)));
        assert_eq!(
            finish(&sched, 1),
            (2_001, 2_001),
            "producer, relay, then 1,000"
        );
    }

    /// Data from a channel the report cannot follow (a socket), and a pipe's
    /// end of file, follow everything that ran, in the upper bound only.
    #[test]
    fn untracked_data_follows_everything_in_the_upper_bound() {
        for kind in [UntrackedRead, Read] {
            let mut sched = family();
            sched.work_span_note_spawn(tid(1), tid(2));
            commit(&mut sched, 1, 0, None);
            commit(&mut sched, 2, 0, None);
            commit(&mut sched, 2, 1_000, None);
            arrive(
                &mut sched,
                1,
                1_000,
                Some(&events(vec![event(kind, 0, false, 0)])),
            );
            sched.work_span_commit(tid(1), Some(&request(1, None)));
            assert_eq!(finish(&sched, 1), (1_000, 2_000), "{kind:?}");
            assert_eq!(sched.work_span.fallback_edges, 1);
        }
    }

    /// A poll the call makes again follows no one only when its attempt
    /// moved nothing: one that wrote part of its data saw room a reader made.
    #[test]
    fn a_repoll_after_progress_keeps_its_waiting_edge() {
        for made_progress in [false, true] {
            let mut sched = family();
            sched.work_span_note_spawn(tid(1), tid(2));
            commit(&mut sched, 1, 0, None);
            commit(&mut sched, 2, 0, None);
            sched.work_span_note_wait(tid(1), &poll(1));
            commit(&mut sched, 2, 5_000, None);
            arrive(&mut sched, 1, 0, None);
            sched.work_span_commit(tid(1), Some(&poll(1)));
            let again = WorkSpanPayload {
                repolls: true,
                ..WorkSpanPayload::default()
            };
            let mut retry = poll(1);
            retry.previous_attempt_made_progress = made_progress;
            arrive(&mut sched, 1, 20, Some(&again));
            sched.work_span_commit(tid(1), Some(&retry));
            let expected = if made_progress { (20, 5_020) } else { (20, 20) };
            assert_eq!(finish(&sched, 1), expected, "progress {made_progress}");
        }
    }

    /// Threads of one process share its memory, which no event shows: in the
    /// upper bound each follows its siblings as far as they have run.
    #[test]
    fn threads_of_a_process_follow_each_other_in_the_upper_bound() {
        let mut sched = family();
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(1), tid(8), false, false, 0);
        register(&mut sched, 8);
        sched.work_span_note_spawn(tid(1), tid(8));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 8, 0, None);
        commit(&mut sched, 8, 700, None);
        commit(&mut sched, 1, 10, None);
        assert_eq!(finish(&sched, 1), (10, 700));
    }

    /// The exits a parent can still reap are the zombies the scheduler holds:
    /// one reaped automatically is forgotten when the next child exits.
    #[test]
    fn automatically_reaped_exits_are_forgotten() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 10, exit(2));
        sched.logically_exited_processes.insert(pid(2));
        sched.autoreaped_exits.insert(pid(2));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 10, exit(3));
        let kept: Vec<DetPid> = sched.work_span.exited[&pid(1)].keys().copied().collect();
        assert_eq!(kept, vec![pid(3)]);
    }

    /// On a backend whose tool runs inside the guest, the report says its
    /// upper span is not an upper bound, and why; every report lists what
    /// neither span observes.
    #[test]
    fn the_report_states_what_its_upper_bound_covers() {
        let sched = family();
        let report = sched.work_span_report().unwrap();
        assert!(report.upper_bound_maintained && report.upper_bound_caveats.is_empty());
        assert_eq!(report.unobserved, UNOBSERVED.to_vec());
        let mut cfg = Config {
            work_span_report: Some(PathBuf::from("/nonexistent/report.json")),
            ..Config::default()
        };
        cfg.backend.tool_shares_guest_descriptor_table = true;
        let in_guest = Scheduler::new(&cfg).work_span_report().unwrap();
        assert!(!in_guest.upper_bound_maintained);
        assert_eq!(
            in_guest.upper_bound_caveats,
            vec![UpperBoundCaveat::InGuestTool]
        );
        assert!(!gathers(&cfg), "an in-guest tool gathers nothing");
    }

    /// A wait the scheduler parks (`ResourceID::WaitChild`) and wakes is
    /// explained by the child it then reaps; without a reap it keeps its
    /// waiting edge.
    #[test]
    fn a_parked_child_wait_is_explained_by_its_reap() {
        for reaps in [true, false] {
            let mut sched = family();
            commit(&mut sched, 1, 0, None);
            sched.work_span_note_spawn(tid(1), tid(2));
            sched.work_span_note_spawn(tid(1), tid(3));
            commit(&mut sched, 2, 0, None);
            commit(&mut sched, 3, 0, None);
            let parked = request(
                1,
                Some((
                    ResourceID::WaitChild {
                        parent: pid(1),
                        spec: crate::types::ChildWaitSpec {
                            selector: crate::types::ChildWaitSelector::Any,
                            owner: None,
                            exit_class: crate::types::ChildWaitExitClass::Sigchld,
                        },
                    },
                    Permission::R,
                )),
            );
            sched.work_span_note_wait(tid(1), &parked);
            commit(&mut sched, 2, 100, exit(2));
            commit(&mut sched, 3, 5_000, None);
            arrive(&mut sched, 1, 0, None);
            sched.work_span_commit(tid(1), Some(&parked));
            if reaps {
                sched.work_span_note_reap(tid(1), 1, pid(1), pid(2));
            }
            commit(&mut sched, 1, 2, None);
            let expected = if reaps { (101, 101) } else { (2, 5_002) };
            assert_eq!(finish(&sched, 1), expected, "reaps {reaps}");
        }
    }

    /// A thread whose last commit was its exit sent everything it gathered
    /// with that request: retiring it poisons nothing.
    #[test]
    fn an_exited_reader_poisons_nothing() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 3, 0, None);
        arrive(
            &mut sched,
            1,
            1,
            Some(&events(vec![event(Write, 2, false, 1)])),
        );
        arrive(
            &mut sched,
            3,
            1,
            Some(&events(vec![event(Read, 1, false, 1)])),
        );
        sched.work_span_commit(tid(3), Some(&request(3, exit(3))));
        sched.work_span_retire_thread(tid(3), pid(3), true, true);
        assert!(!sched.work_span.channels[&(7, 42)].poisoned);
    }

    /// Off by default: nothing is tracked and there is no report.
    #[test]
    fn off_by_default() {
        let mut sched = Scheduler::new(&Config::default());
        assert!(!sched.work_span.enabled);
        sched.work_span_note_arrival(tid(1), 10, true, None);
        sched.work_span_commit(tid(1), None);
        assert!(sched.work_span.cpu_now.is_empty());
        assert!(sched.work_span_report().is_none());
    }
}
