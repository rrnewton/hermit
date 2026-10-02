/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
*/

//! Detcore tool global state, and centralized methods corresponding to the centralized portion of
//! the Detcore tool.

mod foreground_epoll;
mod foreground_store;
mod original_connect;
pub(crate) use foreground_store::CheckedBlockingReadRetry;
pub(crate) use foreground_store::CheckedReadInvocation;
pub(crate) use foreground_store::CheckedReadRange;
#[cfg(test)]
pub(crate) use foreground_store::ReceiveRetryFailure;
pub(crate) use foreground_store::SavedReceivePolicy;
mod original_installation;
mod parked;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::btree_map::Entry;
use std::fmt::Debug;
use std::fs;
use std::fs::File;
use std::io::Cursor;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering::SeqCst;
use std::task::Poll;
use std::time::SystemTime;

use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use detcore_model::network_trace::FreshStreamSocketProfileV3;
use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::NetworkAncillaryDataV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkInputEventV2;
use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkOutputEventV2;
use detcore_model::network_trace::NetworkOutputKindV2;
use detcore_model::network_trace::NetworkPolicy;
use detcore_model::network_trace::NetworkReadinessV2;
use detcore_model::network_trace::NetworkReleaseV2;
use detcore_model::network_trace::NetworkShutdownV2;
use detcore_model::network_trace::NetworkTrace;
use detcore_model::network_trace::StreamSocketKeyV3;
use detcore_model::procfs::mount_ids_are_ordered_subset;
use detcore_model::summary::RunSummary;
use detcore_model::summary::TimesliceStats;
use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
pub(crate) use parked::parked_wait_request;
pub(crate) use parked::polled_read_request;
pub(crate) use parked::signal_dequeued;
use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::config::Config;
use crate::consts::ROOT_DETPID;
use crate::ivar::Ivar;
use crate::network_failure::NetworkFailurePhase;
use crate::network_failure::NetworkPolicyRefusal;
use crate::network_failure::NetworkRpcError;
use crate::network_replay::ConnectionOutcome;
use crate::network_replay::NetworkChannelBinding;
pub use crate::network_replay::NetworkIngressObservation;
use crate::network_replay::NetworkReceiveOptions;
use crate::network_replay::NetworkReplayEngine;
use crate::network_replay::NetworkReplayError;
use crate::network_replay::NetworkShadowProbe;
use crate::network_replay::NetworkSocketControl;
use crate::network_replay::NetworkSocketControlFinish;
use crate::network_replay::NetworkStreamCall;
use crate::network_replay::NetworkStreamCallId;
pub use crate::network_replay::NetworkStreamChunk;
pub use crate::network_replay::NetworkStreamChunkDisposition;
pub use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamNamespace;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::NetworkStreamPhysicalEffect;
use crate::network_replay::NetworkStreamPhysicalResult;
use crate::network_replay::NetworkStreamPinOutcome;
pub use crate::network_replay::NetworkStreamQueueStatus;
use crate::network_replay::NetworkStreamSocketOption;
use crate::network_replay::NetworkStreamSocketState;
use crate::network_replay::NetworkZeroStreamReceive;
use crate::network_replay::NetworkZeroStreamWaitId;
use crate::network_replay::StreamReceiveOutcome;
use crate::network_replay::StreamTransmitOutcome;
use crate::network_replay::replay_from_reader_with_expected_epoch;
use crate::preemptions::PreemptionReader;
use crate::preemptions::ThreadHistory;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ChaosEpochTransition;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::AdmitIntent;
use crate::scheduler::AdmitSide;
use crate::scheduler::ConsumeResult;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::scheduler::ExecReconnect;
use crate::scheduler::MaybePrintStack;
use crate::scheduler::Priority;
use crate::scheduler::SchedResponse;
use crate::scheduler::SchedValue;
use crate::scheduler::Scheduler;
use crate::scheduler::ThreadNextTurn;
use crate::scheduler::entropy_to_priority;
use crate::scheduler::parked::*;
use crate::scheduler::real_timer::DequeueAck;
use crate::scheduler::real_timer::ItimerSnapshot;
use crate::scheduler::real_timer::TimerFailure;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::runqueue::REPLAY_DEFERRED_PRIORITY;
use crate::scheduler::runqueue::REPLAY_FOREGROUND_PRIORITY;
use crate::scheduler::runqueue::is_ordinary_priority;
use crate::scheduler::sched_loop;
use crate::scheduler::sched_loop_external;
use crate::tool_local::Detcore;
use crate::tool_local::ExecFdBlockingOverrides;
use crate::tool_local::RobustListWake;
use crate::types::*;

pub(crate) async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(|context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[derive(Debug)]
struct InodePool {
    // TODO(T87258449): merge these two maps:
    inodes: HashMap<RawInode, DetInode>,
    detinodes_info: HashMap<DetInode, DetInodeInfo>,
    /// Counter backing the minted [`DetInode`]s. Deliberately a plain integer:
    /// it is the *source* of deterministic inodes, not one itself, and typing
    /// it `RawInode` previously blurred that distinction.
    next_inode: u64,
}

/// Everything we know (globally) about a DetInode.
#[derive(Debug)]
struct DetInodeInfo {
    raw: RawInode,
    mtime: LogicalTime,
}

/// Everything the global scheduler needs to register a new child thread. A
/// normal clone is registered by the parent; a `CLONE_VFORK` child registers
/// itself (with `parent_is_kernel_blocked` set) because its parent is blocked
/// inside the kernel until the child execs or exits.
struct ChildRegistration {
    parent_dettid: DetTid,
    parent_detpid: DetPid,
    child_dettid: DetTid,
    child_tid_addr: usize,
    flags: Option<CloneFlags>,
    exit_signal: libc::c_int,
    physical_ids: Option<(i32, i32)>,
    maybe_priority: Option<Priority>,
    parent_is_kernel_blocked: bool,
    inherited_birth: Option<crate::scheduler::NoSeqChildBirth>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingExecState {
    receipt: ExecFilesReceipt,
    fd_blocking: ExecFdBlockingOverrides,
}

/// Separate terminal cleanup outcomes; neither replaces the backend failure.
pub struct BackendFailureCleanup {
    /// Natural scheduler completion, retaining a task panic or cancellation.
    pub scheduler: Result<(), tokio::task::JoinError>,
    /// The requested partial recording's write result; no destination is success.
    pub preemption_recording: Result<(), String>,
}

#[derive(Clone, Copy)]
struct RpcIncarnation {
    dettid: DetTid,
    mm: MmId,
}

impl Default for InodePool {
    fn default() -> Self {
        InodePool::new()
    }
}

impl InodePool {
    fn new() -> Self {
        InodePool {
            inodes: HashMap::new(),
            detinodes_info: HashMap::new(),
            next_inode: 1,
        }
    }

    // Allocate the next deterministic inode.  This takes the raw-inode and
    // can return an existing mapping or extend the mapping by creating a
    // new deterministic inode. The returned inode is strictly increasing
    // to avoid inode re-use issue in some filesystem like ext4.
    fn add_inode(&mut self, raw_inode: RawInode, mtime: LogicalTime) -> (DetInode, LogicalTime) {
        match self.inodes.get(&raw_inode) {
            None => {
                // THE determinization boundary: the single place a host inode
                // is deliberately mapped to a deterministic one. The value is
                // minted from a monotonic counter, never derived from the host
                // inode's bits.
                let new = DetInode::mint(self.next_inode);
                self.next_inode += 1;
                assert!(self.inodes.insert(raw_inode, new).is_none());
                let prev = self.detinodes_info.insert(
                    new,
                    DetInodeInfo {
                        raw: raw_inode,
                        mtime,
                    },
                );
                assert!(prev.is_none()); // Should not have been previously used.
                (new, mtime)
            }
            Some(dino) => {
                let info = self
                    .detinodes_info
                    .get(dino)
                    .expect("Internal invariant broken, det_ino missing entry");
                (*dino, info.mtime)
            }
        }
    }

    // remove a det inode
    fn remove_inode(&mut self, det_inode: DetInode) {
        if let Some(info) = self.detinodes_info.remove(&det_inode) {
            self.inodes.remove(&info.raw);
        }
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1056): Deterministic remapping of device numbers (st_dev).
/// Deterministic remapping of device numbers (`st_dev`).
///
/// The kernel assigns anonymous block-device numbers to filesystems without a
/// backing block device (procfs, sysfs, tmpfs, devpts) from a global,
/// host-wide counter (`get_anon_bdev`). The raw `st_dev` a guest observes for
/// such a filesystem therefore drifts between otherwise-identical runs — and
/// even between the two runs of `--verify`, because the first run's mounts are
/// still live when the second run mounts fresh copies, so the second run's
/// procfs gets a different anonymous device number. That leaked host state into
/// a guest-visible `stat`/`statx` field.
///
/// We replace each distinct raw device number with a strictly-increasing
/// synthetic id assigned in first-observation order. Both `stat`/`statx` and a
/// virtualized mountinfo snapshot use this pool. A mountinfo read intentionally
/// pre-populates it in that snapshot's row order, and later metadata syscalls
/// reuse those assignments.
///
/// This guarantees identity consistency within one Detcore run. It is not an
/// unconditional cross-machine guarantee: hosts with different filesystem
/// layouts can expose different device equivalence classes or first-observation
/// order. The remapping still preserves distinctness and equality within the
/// run, so `find -xdev`, `du -x`, and `(st_dev, st_ino)` checks behave
/// consistently with the mountinfo device column.
#[derive(Debug)]
struct DevicePool {
    devices: HashMap<u64, u64>,
    next_device: u64,
}

/// Run-global identities for fdinfo mount IDs that are not present in the
/// namespace's mountinfo table.
///
/// Linux gives pseudo filesystems such as pipefs, sockfs, anon_inodefs, nsfs,
/// and pidfs their own mount IDs without listing those mounts in
/// `/proc/*/mountinfo`. The raw numbers are host-assigned. Preserve equality
/// and distinctness by keying on the raw mount ID. IDs present in mountinfo are
/// assigned in canonical row/parent order; unlisted IDs are assigned afterward
/// in deterministic guest observation order.
#[derive(Debug)]
enum MountIdPool {
    Uninitialized,
    Invalid,
    Ready {
        mount_ids: BTreeMap<u64, u64>,
        mountinfo_order: Vec<u64>,
        allow_visible_subsets: bool,
        unlisted_order: Vec<u64>,
        next_mount_id: u64,
    },
}

/// The raw identity order observed by the producer and required to reconstruct
/// the same guest-visible mount IDs during replay.
pub struct MountIdentityProvenance {
    pub mountinfo_order: Vec<u64>,
    pub unlisted_order: Vec<u64>,
}

impl MountIdPool {
    fn from_config(mount_ids: &[u64], captured: bool, unlisted_ids: &[u64]) -> Self {
        if !captured {
            if !mount_ids.is_empty() || !unlisted_ids.is_empty() {
                return Self::Invalid;
            }
            return Self::Uninitialized;
        }
        Self::from_orders(mount_ids, unlisted_ids, true).unwrap_or(Self::Invalid)
    }

    fn from_orders(
        mount_ids: &[u64],
        unlisted_ids: &[u64],
        allow_visible_subsets: bool,
    ) -> Option<Self> {
        let mut seen = BTreeSet::new();
        if !mount_ids.iter().all(|raw| seen.insert(*raw))
            || !unlisted_ids
                .iter()
                .all(|raw| *raw != 0 && seen.insert(*raw))
        {
            return None;
        }

        let mut mappings = BTreeMap::new();
        for (index, raw) in mount_ids.iter().chain(unlisted_ids).enumerate() {
            mappings.insert(*raw, u64::try_from(index).ok()?.checked_add(1)?);
        }
        let next_mount_id = u64::try_from(mappings.len()).ok()?.checked_add(1)?;
        Some(Self::Ready {
            mount_ids: mappings,
            mountinfo_order: mount_ids.to_vec(),
            allow_visible_subsets,
            unlisted_order: unlisted_ids.to_vec(),
            next_mount_id,
        })
    }

    fn validate_mountinfo_order(&mut self, mountinfo_order: &[u64]) -> bool {
        if matches!(self, Self::Uninitialized) {
            *self = Self::from_orders(mountinfo_order, &[], false).unwrap_or(Self::Invalid);
        }
        let Self::Ready {
            mountinfo_order: expected,
            allow_visible_subsets,
            ..
        } = self
        else {
            return false;
        };
        if *allow_visible_subsets {
            mount_ids_are_ordered_subset(mountinfo_order, expected)
        } else {
            mountinfo_order == expected
        }
    }

    fn determinize(&mut self, raw_mount_id: u64, mountinfo_order: Option<&[u64]>) -> Option<u64> {
        // Linux uses zero for anonymous objects such as memfd. It is one
        // equivalence class regardless of Detcore's descriptor classification.
        if raw_mount_id == 0 {
            return Some(0);
        }
        if let Some(order) = mountinfo_order {
            if !self.validate_mountinfo_order(order) {
                return None;
            }
        } else if matches!(self, Self::Uninitialized) {
            return None;
        }
        let Self::Ready {
            mount_ids,
            unlisted_order,
            next_mount_id,
            ..
        } = self
        else {
            return None;
        };

        if let Some(virtual_mount_id) = mount_ids.get(&raw_mount_id) {
            return Some(*virtual_mount_id);
        }
        let virtual_mount_id = *next_mount_id;
        *next_mount_id = next_mount_id.checked_add(1)?;
        mount_ids.insert(raw_mount_id, virtual_mount_id);
        unlisted_order.push(raw_mount_id);
        Some(virtual_mount_id)
    }

    fn provenance(&self) -> Result<Option<MountIdentityProvenance>, &'static str> {
        match self {
            Self::Uninitialized => Ok(None),
            Self::Invalid => Err("mount identity provenance is invalid"),
            Self::Ready {
                mountinfo_order,
                unlisted_order,
                ..
            } => Ok(Some(MountIdentityProvenance {
                mountinfo_order: mountinfo_order.clone(),
                unlisted_order: unlisted_order.clone(),
            })),
        }
    }
}

impl Default for DevicePool {
    fn default() -> Self {
        DevicePool::new()
    }
}

impl DevicePool {
    fn new() -> Self {
        // Start at 1 so no file reports st_dev == 0, which some tools treat as
        // "no device".
        DevicePool {
            devices: HashMap::new(),
            next_device: 1,
        }
    }

    /// Return the deterministic device id for `raw_device`, allocating a new one
    /// (in first-observation order) the first time a raw device is seen.
    fn determinize(&mut self, raw_device: u64) -> u64 {
        match self.devices.get(&raw_device) {
            Some(dev) => *dev,
            None => {
                let new = self.next_device;
                self.next_device += 1;
                self.devices.insert(raw_device, new);
                new
            }
        }
    }
}

fn initialize_network_engine(
    cfg: &Config,
) -> Result<Option<Arc<Mutex<NetworkReplayEngine>>>, String> {
    let engine = match cfg.network_trace.policy {
        NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => return Ok(None),
        NetworkPolicy::Record => {
            if !cfg.epoch_explicit {
                return Err(
                    "network record epoch was not resolved before engine startup".to_owned(),
                );
            }
            // Production Record uses the V4 native-receive journal. The V3
            // recorder stays constructible, and its host-stream capture stays
            // refused: it has no current-layout or copy authority.
            NetworkReplayEngine::record_native_receive(cfg.epoch)
        }
        NetworkPolicy::Replay => {
            if !cfg.epoch_explicit {
                return Err(
                    "network replay epoch was not resolved before engine startup".to_owned(),
                );
            }
            let bytes = cfg.network_trace_input.as_ref().ok_or_else(|| {
                "network replay policy omitted verified host trace bytes".to_owned()
            })?;
            replay_from_reader_with_expected_epoch(Cursor::new(bytes), cfg.epoch)
                .map_err(|error| format!("cannot initialize network replay: {error}"))?
        }
    };
    Ok(Some(Arc::new(Mutex::new(engine))))
}

#[derive(Debug, Default)]
struct NetworkRecordProgress {
    inbound_stream: u64,
    outbound_stream: u64,
}

/// Global state associated with the detcore tool.
///
/// This is a singleton, and the one object of this type lives inside a central
/// address space, generally the "tracer" in a Reverie backend.
#[derive(Debug)]
pub struct GlobalState {
    sched: Arc<Mutex<Scheduler>>,

    /// One run-global network engine shared by every guest task and scheduler wait.
    network_engine: Option<Arc<Mutex<NetworkReplayEngine>>>,

    /// Actual owned startup resources; never reconstructed from Config or an FD number.
    network_runtime: Option<crate::network_runtime::NetworkRuntimeResources>,

    /// Stream offsets are run-global so competing aliases cannot assign them
    /// according to thread-local syscall order.
    network_record_progress: Mutex<BTreeMap<OpenFileId, NetworkRecordProgress>>,

    inodes: Arc<Mutex<InodePool>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping state.
    devices: Arc<Mutex<DevicePool>>,

    /// Shared fdinfo mount-ID equivalence classes for this Detcore run.
    mount_ids: Mutex<MountIdPool>,

    // next port to use if input port is 0
    next_port: AtomicU16,

    // used ports
    used_ports: Arc<Mutex<HashSet<u16>>>,

    // Unsupported syscall names observed across every process in this run.
    unsupported_syscalls: Mutex<BTreeSet<String>>,

    // Optional append-only sink shared by DBT fork descendants.
    unsupported_syscall_report_fd: Option<Mutex<File>>,

    // Open file description to bound port.
    open_file_to_port: Arc<Mutex<HashMap<OpenFileId, u16>>>,

    port_start_range: AtomicU16,
    port_end_range: AtomicU16,

    // False initially after fork, and true when we begin executing the guest binary.
    past_first_execve: AtomicBool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
    /// Pre-exec identity and descriptor state awaiting a SaBRe exec reload.
    // TODO-HUMAN-REVIEW(PR-1173): Review SaBRe exec incarnation fencing.
    pending_exec_states: Mutex<BTreeMap<DetPid, PendingExecState>>,

    /// One checked, never-rewound allocator for every exec in this run.
    exec_files_allocator: Mutex<FilesIdAllocator>,

    /// Address spaces derived at accepted clone/root registration and updated
    /// only by authenticated successful exec. The PrepareExec payload is not
    /// its own authority for a pre-exec address space.
    registered_exec_mms: Arc<Mutex<BTreeMap<DetTid, MmId>>>,

    /// Contending sibling execs wait without retaining any scheduler lock.
    exec_preparation_changed: Arc<tokio::sync::Notify>,
    network_stream_changed: Arc<tokio::sync::Notify>,

    /// The same reservation returned before injection, awaiting post-exec
    /// delivery after a backend reentered thread-start.
    post_exec_files: Mutex<BTreeMap<DetTid, ExecFilesReceipt>>,

    /// Descriptor state retained after the one-shot scheduler transition is consumed.
    post_exec_fd_blocking: Mutex<BTreeMap<DetTid, ExecFdBlockingOverrides>>,

    sched_handle: Option<tokio::task::JoinHandle<()>>,

    /// Global time is a *volatile* vector clock of individual thread progress. Each
    /// thread can independently update its own progress, even (potentially) asynchronously.
    ///
    /// LockOrdering: this lock can be acquired while holding the sched lock (but not vice
    /// versa).
    //
    // TODO: it would be more future-proof to provide a non-blocking way to retrieve a
    // (nondeterministic) monotonic lower bound on global time.
    global_time: Arc<Mutex<GlobalTime>>,

    /// Just cache the config so we can access it from everywhere.
    cfg: Config,

    /// Storage for the preemption record read from `replay_preemptions_from`.
    preemptions_to_replay: Option<PreemptionReader>,

    /// The start is when we construct the global state.  Close enough.
    realtime_start: SystemTime,
}

impl Default for GlobalState {
    fn default() -> Self {
        // TODO(T77816673): eventually we want to remove this requirement.
        // In the meantime... just don't call this.
        panic!("Detcore GlobalState Default impl should not be called");
    }
}

impl Drop for GlobalState {
    fn drop(&mut self) {
        // TODO-HUMAN-REVIEW(PR-643): Review shutdown-time aggregate warning delivery.
        if let Some(message) =
            format_unsupported_syscall_warning(&self.unsupported_syscalls.lock().unwrap())
        {
            warn!("{}", message);
        }
        info!("detcore shut down, destroying global state");
    }
}

impl GlobalState {
    /// Check one real local-only zero-time poll while the existing reader
    /// protects its exact slot. No new source, effect, or scheduling grant is
    /// issued; callers recheck after the native probe before using its bits.
    pub(crate) fn validate_local_pair_poll<T>(
        &self,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<(), reverie::Error> {
        let refuse = |message: &str| reverie::Error::Tool(anyhow::anyhow!(message.to_owned()));
        let owner = NetworkStreamOwner { thread: state.dettid, mm: state.mm_id };
        let sched = self.sched.lock().unwrap();
        let root = self.network_runtime.as_ref()
            .ok_or_else(|| refuse("local pair poll lacks original runtime"))?
            .foreground_root(owner).map_err(|e| refuse(&e.to_string()))?;
        sched.foreground_native_observation(owner, &root).map_err(|e| refuse(&e.to_string()))?;
        if !root.matches_metadata(&state.file_metadata) || root.files() != read.publication.permit.files {
            return Err(refuse("local pair poll changed original table"));
        }
        let mut local = state.file_metadata.lock().unwrap();
        if !local.observe_read_descriptor(read)?.is_some_and(|fd| fd.is_local_socket_pair()) {
            return Err(refuse("local pair poll changed exact local endpoint"));
        }
        let engine = self.network_engine.as_ref().ok_or_else(|| refuse("local pair poll lost engine"))?.lock().unwrap();
        engine.validate_fd_metadata(owner, root.files(), &state.file_metadata, &local)
            .map_err(|e| refuse(&e.to_string()))?;
        engine.validate_fd_read_grant(owner, read).map_err(|e| refuse(&e.to_string()))
    }

    /// Inspect the version and mode of this actual shared engine. Config policy
    /// alone cannot select a native receive path or create another engine.
    pub(crate) fn native_receive_mode(&self) -> Option<crate::network_replay::NetworkEngineMode> {
        let engine = self.network_engine.as_ref()?.lock().unwrap();
        engine.native_receive_version().then(|| engine.mode())
    }

    #[cfg(test)]
    pub(crate) fn native_record_view_fixture(cfg: &Config) -> Self {
        let state = Self::initialize(cfg, false);
        assert_eq!(
            state.native_receive_mode(),
            Some(crate::network_replay::NetworkEngineMode::Record)
        );
        state
    }

    async fn recv_enroll_accepted_listener(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        fd: i32,
    ) -> GlobalResponse {
        let Some(runtime) = &self.network_runtime else {
            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                "listener provider runtime absent",
            )));
        };
        let prepared = {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return GlobalResponse::ThreadExited;
            }
            (|| -> Result<_, NetworkRpcError> {
                let engine = self
                    .network_engine
                    .as_ref()
                    .ok_or_else(|| NetworkRpcError::internal("listener engine absent"))?
                    .lock()
                    .unwrap();
                let (open_file, state) = engine
                    .accepted_listener_enrollment_target(owner, call)
                    .map_err(|error| NetworkRpcError::internal(error.to_string()))?;
                if engine.accepted_listener_enrolled(open_file) {
                    return Ok(None);
                }
                runtime
                    .capture_accepted_listener(owner, call, open_file, fd)
                    .map_err(|error| NetworkRpcError::internal(error.to_string()))?;
                Ok(Some((open_file, state, sched.backend_failure_waiter())))
            })()
        };
        let (open_file, state, backend_failed) = match prepared {
            Ok(Some(value)) => value,
            Ok(None) => return GlobalResponse::Network(Ok(NetworkReply::Unit)),
            Err(error) => return GlobalResponse::Network(Err(error)),
        };
        let evidence = tokio::select! {
            evidence=runtime.enroll_accepted_listener(owner,open_file,&state)=>evidence,
            _=backend_failed=>return GlobalResponse::ThreadExited,
        };
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed()
            || sched.thread_is_logically_killed(owner.thread)
            || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return GlobalResponse::ThreadExited;
        }
        let outcome = evidence
            .map_err(|error| NetworkRpcError::internal(error.to_string()))
            .and_then(|evidence| {
                self.network_engine
                    .as_ref()
                    .ok_or_else(|| NetworkRpcError::internal("listener engine disappeared"))?
                    .lock()
                    .unwrap()
                    .confirm_provider_listener_enrollment(owner, call, evidence)
                    .map_err(|error| NetworkRpcError::internal(error.to_string()))
            });
        GlobalResponse::Network(outcome.map(|()| NetworkReply::Unit))
    }

    async fn recv_prepare_accepted_effect(
        &self,
        owner: NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        fd: i32,
        flags: i32,
    ) -> GlobalResponse {
        let Some(runtime) = &self.network_runtime else {
            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                "accepted effect runtime absent",
            )));
        };
        let (listener, physical, backend_failed) = {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return GlobalResponse::ThreadExited;
            }
            let target = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("accepted effect engine absent"))
                .and_then(|engine| {
                    engine
                        .lock()
                        .unwrap()
                        .accepted_provider_effect_target(owner, lease)
                        .map_err(|error| NetworkRpcError::internal(error.to_string()))
                });
            let (listener, physical) = match target {
                Ok(value) => value,
                Err(error) => return GlobalResponse::Network(Err(error)),
            };
            (listener, physical, sched.backend_failure_waiter())
        };
        let metadata = (|| -> Result<_, NetworkRpcError> {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "accepted preparation lost exact registered owner",
                ));
            }
            let engine = self.network_engine.as_ref().unwrap();
            let (files, actual) = engine
                .lock()
                .unwrap()
                .original_installation_metadata(owner)
                .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
            // Original metadata custody precedes provider arming. The actual
            // association is rechecked in the required metadata→engine order.
            {
                let local = actual.lock().unwrap();
                engine
                    .lock()
                    .unwrap()
                    .validate_fd_metadata(owner, files, &actual, &local)
                    .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
            }
            runtime
                .bind_accepted_installation_metadata(owner, lease, files, actual)
                .map_err(|e| NetworkRpcError::internal(e.to_string()))
        })();
        if let Err(error) = metadata {
            return GlobalResponse::Network(Err(error));
        }
        let result = tokio::select! {
            result=runtime.prepare_accepted_effect(owner,lease,listener,physical,fd,flags)=>result,
            _=backend_failed=>return GlobalResponse::ThreadExited,
        };
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed()
            || sched.thread_is_logically_killed(owner.thread)
            || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return GlobalResponse::ThreadExited;
        }
        GlobalResponse::Network(
            result
                .map(|()| NetworkReply::Unit)
                .map_err(|error| NetworkRpcError::internal(error.to_string())),
        )
    }

    async fn recv_collect_accepted_effect(
        &self,
        owner: NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> GlobalResponse {
        let Some(runtime) = &self.network_runtime else {
            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                "accepted recovery runtime absent",
            )));
        };
        // Authenticate the original submitted lease, even when scheduler/task
        // publication permission was revoked. This only retains effect evidence.
        let admission = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("accepted recovery engine absent"))
            .and_then(|engine| {
                engine
                    .lock()
                    .unwrap()
                    .accepted_capture_recovery_call(owner, lease)
                    .map_err(|error| NetworkRpcError::internal(error.to_string()))
            });
        if let Err(error) = admission {
            return GlobalResponse::Network(Err(error));
        }
        GlobalResponse::Network(
            runtime
                .collect_accepted_effect(owner, lease)
                .await
                .map(|()| NetworkReply::Unit)
                .map_err(|error| NetworkRpcError::internal(error.to_string())),
        )
    }

    async fn recv_resolve_accepted_provider(
        &self,
        owner: NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> GlobalResponse {
        let Some(runtime) = &self.network_runtime else {
            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                "accepted match runtime absent",
            )));
        };
        let backend_failed = {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return GlobalResponse::ThreadExited;
            }
            let result = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("accepted match engine absent"))
                .and_then(|engine| {
                    engine
                        .lock()
                        .unwrap()
                        .accepted_capture_call(owner, lease)
                        .map_err(|error| NetworkRpcError::internal(error.to_string()))
                });
            if let Err(error) = result {
                return GlobalResponse::Network(Err(error));
            }
            sched.backend_failure_waiter()
        };
        match runtime.accepted_captured_result(owner, lease) {
            Ok(Err(_)) => {
                let published = self
                    .publish_accepted_original_no_installation(owner, lease)
                    .await;
                return GlobalResponse::Network(
                    published.map(|()| NetworkReply::AcceptedNoInstallation),
                );
            }
            Ok(Ok(_)) => {}
            Err(error) => {
                return GlobalResponse::Network(Err(NetworkRpcError::internal(error.to_string())));
            }
        }
        let matched = tokio::select! {
            result=runtime.resolve_accepted_pin(owner,lease)=>result,
            _=backend_failed=>return GlobalResponse::ThreadExited,
        };
        let matched = match matched {
            Ok(matched) => matched,
            Err(error) => {
                return GlobalResponse::Network(Err(NetworkRpcError::internal(error.to_string())));
            }
        };
        // Successful kernel accept already proves this child became queued.
        // Observer fexit may trail queue publication. Reobserve that pending
        // phase; never convert it into no-connection or an installed slot.
        loop {
            let backend_failed = {
                let sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(owner.thread)
                    || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                    || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                {
                    return GlobalResponse::ThreadExited;
                }
                let engine = self.network_engine.as_ref().unwrap().lock().unwrap();
                if let Err(error) = engine.accepted_capture_call(owner, lease) {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        error.to_string(),
                    )));
                }
                match engine.provider_child_published(matched) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        return GlobalResponse::Network(Err(NetworkRpcError::internal(
                            error.to_string(),
                        )));
                    }
                }
                sched.backend_failure_waiter()
            };
            let next = tokio::select! {
                next=runtime.next_accepted_creation(owner)=>next,
                _=backend_failed=>return GlobalResponse::ThreadExited,
            };
            let publication = match next {
                Ok(Some(value)) => value,
                Ok(None) => {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        "accepted observation completed without a queued or terminal receipt",
                    )));
                }
                Err(error) => {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        error.to_string(),
                    )));
                }
            };
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return GlobalResponse::ThreadExited;
            }
            let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
            if let Err(error) = engine.accepted_capture_call(owner, lease).and_then(|_| {
                engine.confirm_provider_child_creation(
                    &publication.evidence,
                    self.global_time.lock().unwrap().as_nanos(),
                )
            }) {
                return GlobalResponse::Network(Err(NetworkRpcError::internal(error.to_string())));
            }
            // No await separates publication and exact runtime cursor ACK.
            if let Err(error) = publication.acknowledge() {
                return GlobalResponse::Network(Err(NetworkRpcError::internal(error.to_string())));
            }
            self.network_stream_changed.notify_waiters();
        }
        let published = self
            .publish_accepted_original_installation(owner, lease)
            .await;
        GlobalResponse::Network(published.map(NetworkReply::AcceptedInstallation))
    }

    fn recv_capture_accepted_return(
        &self,
        owner: NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        result: Result<i32, i32>,
    ) -> GlobalResponse {
        let sched = self.sched.lock().unwrap();
        let mut may_publish = !sched.backend_failed()
            && !sched.thread_is_logically_killed(owner.thread)
            && sched.rpc_incarnation_matches(owner.thread, owner.mm)
            && self.registered_exec_mms.lock().unwrap().get(&owner.thread) == Some(&owner.mm);
        let outcome = (|| -> Result<NetworkReply, NetworkRpcError> {
            let runtime = self.network_runtime.as_ref().ok_or_else(|| {
                NetworkRpcError::internal("accept capture has no authenticated runtime")
            })?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("accept capture has no engine"))?;
            let engine = engine.lock().unwrap();
            // Exact lease+original owner/MM is recovery authority even when a
            // later scheduler transition has revoked semantic publication.
            engine
                .accepted_capture_recovery_call(owner, lease)
                .map_err(|error| NetworkRpcError::internal(error.to_string()))?;
            may_publish &= !engine
                .accepted_capture_owner_retired(owner, lease)
                .map_err(|error| NetworkRpcError::internal(error.to_string()))?;
            let may_acquire = may_publish && engine.accepted_capture_call(owner, lease).is_ok();
            runtime
                .capture_accept_return(owner, lease, result, may_acquire)
                .map_err(|error| NetworkRpcError::internal(error.to_string()))?;
            Ok(NetworkReply::Unit)
        })();
        if may_publish {
            GlobalResponse::Network(outcome)
        } else {
            GlobalResponse::ThreadExited
        }
    }

    /// Ordinary RPC mutation must linearize before terminal publication under
    /// the same mutex as scheduler grants. A losing callback stays pending for
    /// the backend's failure subscription to drop; no normal reply is invented.
    /// Consuming exit RPCs retain their existing validation/accounting path.
    async fn lock_rpc_scheduler(
        &self,
        consuming_cleanup: bool,
    ) -> std::sync::MutexGuard<'_, Scheduler> {
        std::future::poll_fn(|_| {
            let sched = self.sched.lock().unwrap();
            if !consuming_cleanup && sched.backend_failed() {
                Poll::Pending
            } else {
                Poll::Ready(sched)
            }
        })
        .await
    }

    async fn recv_prepare_exec(
        &self,
        caller: DetTid,
        process: DetPid,
        request_mm: MmId,
        mm: MmId,
        old_files: FilesId,
        fd_blocking: ExecFdBlockingOverrides,
    ) -> GlobalResponse {
        loop {
            let changed = self.exec_preparation_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                // This direct lock lets a waiter observe terminal failure;
                // lock_rpc_scheduler(false) intentionally stays pending there.
                let sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(caller)
                    || sched.registered_process(caller) != Some(process)
                    || request_mm != mm
                    || !sched.rpc_incarnation_matches(caller, mm)
                    || self.registered_exec_mms.lock().unwrap().get(&caller) != Some(&mm)
                {
                    return GlobalResponse::ThreadExited;
                }
                let mut pending = self.pending_exec_states.lock().unwrap();
                if let Some(active) = pending.get(&process) {
                    assert_ne!(
                        active.receipt.caller, caller,
                        "one caller cannot prepare exec twice without completing its attempt"
                    );
                } else {
                    let new_files = self
                        .exec_files_allocator
                        .lock()
                        .unwrap()
                        .allocate_exec(caller);
                    let receipt = ExecFilesReceipt {
                        caller,
                        process,
                        mm,
                        old_files,
                        new_files,
                    };
                    trace!(
                        "[detcore, dtid {}] preparing exec with table receipt {:?}",
                        caller, receipt
                    );
                    pending.insert(
                        process,
                        PendingExecState {
                            receipt,
                            fd_blocking,
                        },
                    );
                    return GlobalResponse::PrepareExec(receipt);
                }
            }
            // The active caller can cancel or complete without acquiring a
            // lock held by this waiter. A wake never itself admits the caller.
            changed.await;
        }
    }

    /// Return the producer-observed mount identity order after a run.
    ///
    /// The first vector is the exact mountinfo row/parent order. The second is
    /// the first-observation order of raw fdinfo IDs absent from that table.
    pub fn mount_identity_provenance(
        &self,
    ) -> Result<Option<MountIdentityProvenance>, &'static str> {
        self.mount_ids.lock().unwrap().provenance()
    }

    fn initialize(cfg: &Config, spawn_scheduler: bool) -> Self {
        // Replay is decoded and fully validated before the first guest task can
        // start.  There is no live-network fallback for a missing or malformed
        // trace.
        let network_engine = initialize_network_engine(cfg).unwrap_or_else(|error| {
            panic!("network capture/replay initialization failed: {error}")
        });
        let mut scheduler = Scheduler::new(cfg);
        scheduler.set_network_engine(network_engine.clone());
        let network_stream_changed = scheduler.fd_read_notification();
        let sched = Arc::new(Mutex::new(scheduler));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(cfg)));
        let handle = if cfg.sequentialize_threads && spawn_scheduler {
            // Announce before spawning, not from inside the spawned task. The
            // task's first poll is unordered with respect to the rest of this
            // bootstrap, so emitting there raced the root thread's seeding
            // lines and produced a nondeterministic INFO stream. Emitting here
            // sequences it: `Scheduler::new`'s SCHEDRAND line above, then this,
            // then the root `ThreadState::new` seeding lines.
            info!("[scheduler] daemon task starting up, waiting for guest thread start..");
            Some(tokio::spawn(sched_loop(sched.clone(), global_time.clone())))
        } else {
            None
        };

        let preemptions_to_replay: Option<PreemptionReader> = cfg
            .replay_preemptions_from
            .as_ref()
            .map(|path| PreemptionReader::new(path));
        let range = Self::read_port_range();

        let unsupported_syscall_report_fd = cfg.unsupported_syscall_report_fd.and_then(|fd| {
            // This writer is internal controller state. In an in-process DBT
            // runtime it must not leak into the next guest image across exec
            // (hence F_DUPFD_CLOEXEC), and it must not perturb the descriptor
            // namespace the *current* guest observes. The backend places the
            // report fd itself high, out of the guest's working range (e.g. 199
            // for the DBT backend). Duplicating with a min hint of `fd` keeps
            // this private copy up in that same reserved band instead of
            // grabbing the lowest free descriptor (fd 3), which would shift
            // every fd the guest subsequently opens and diverge from the golden
            // ptrace reference (where this fd is unset and no dup happens).
            let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, fd) };
            if duplicate == -1 {
                warn!(
                    "failed to duplicate unsupported-syscall report fd {fd}: {}",
                    std::io::Error::last_os_error()
                );
                None
            } else {
                // SAFETY: dup returned a new owned descriptor.
                Some(Mutex::new(unsafe { File::from_raw_fd(duplicate) }))
            }
        });

        Self {
            sched,
            network_engine,
            network_runtime: None,
            network_record_progress: Mutex::new(BTreeMap::new()),
            next_port: AtomicU16::new(range[0]),
            used_ports: Arc::new(Mutex::new(HashSet::new())),
            unsupported_syscalls: Mutex::new(BTreeSet::new()),
            unsupported_syscall_report_fd,
            port_start_range: AtomicU16::new(range[0]),
            port_end_range: AtomicU16::new(range[1]),
            open_file_to_port: Arc::new(Mutex::new(HashMap::new())),
            past_first_execve: AtomicBool::new(false),
            pending_exec_states: Mutex::new(BTreeMap::new()),
            exec_files_allocator: Mutex::new(FilesIdAllocator::default()),
            registered_exec_mms: Arc::new(Mutex::new(BTreeMap::new())),
            exec_preparation_changed: Arc::new(tokio::sync::Notify::new()),
            network_stream_changed,
            post_exec_files: Mutex::new(BTreeMap::new()),
            post_exec_fd_blocking: Mutex::new(BTreeMap::new()),
            inodes: Arc::new(Mutex::new(InodePool::new())),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping state.
            devices: Arc::new(Mutex::new(DevicePool::new())),
            mount_ids: Mutex::new(MountIdPool::from_config(
                &cfg.mountinfo_mount_ids,
                cfg.mountinfo_mount_ids_captured,
                &cfg.fdinfo_unlisted_mount_ids,
            )),
            sched_handle: handle,
            cfg: cfg.clone(),
            realtime_start: SystemTime::now(),
            global_time,
            preemptions_to_replay,
        }
    }

    /// Initializes global state whose sequential scheduler is driven by an
    /// external backend executor.
    pub fn init_for_external_scheduler(cfg: &Config) -> Self {
        assert!(
            cfg.sequentialize_threads,
            "an external scheduler is only meaningful when threads are sequentialized"
        );
        Self::initialize(cfg, false)
    }

    /// Runs the sequential scheduler on a backend-owned executor.
    pub async fn run_external_scheduler(&self, observer: Arc<dyn Fn(&'static str) + Send + Sync>) {
        // Emitted at the call site for the same reason as the spawned path in
        // `initialize`, so both ways of starting the daemon place this line at
        // a deterministic point in the caller's program order.
        info!("[scheduler] daemon task starting up, waiting for guest thread start..");
        sched_loop_external(self.sched.clone(), self.global_time.clone(), observer).await;
    }

    /// Reports that a backend supervisor received a process's final kernel exit status.
    ///
    /// This only records a barrier observation when the backend advertises physical-exit
    /// reporting; it is therefore a no-op for ptrace, DBT, KVM, and LiteInst execution. The
    /// exact process's barrier is released at this physical-waitability boundary.
    pub fn complete_physical_process_exit(&self, raw_pid: i32) {
        let detpid = DetPid::from_raw(raw_pid);
        self.pending_exec_states.lock().unwrap().remove(&detpid);
        self.post_exec_files.lock().unwrap().remove(&detpid);
        self.post_exec_fd_blocking.lock().unwrap().remove(&detpid);
        self.exec_preparation_changed.notify_waiters();
        if self
            .sched
            .lock()
            .unwrap()
            .complete_physical_process_exit(detpid)
        {
            trace!(
                "[detcore, dpid {}] backend completed final physical process exit",
                detpid
            );
        }
    }

    /// Releases all physical-process-exit barriers after a backend supervisor has drained every
    /// tracee and no guest thread can race another lifecycle event.
    pub fn release_all_physical_process_exits(&self) {
        self.pending_exec_states.lock().unwrap().clear();
        self.post_exec_files.lock().unwrap().clear();
        self.post_exec_fd_blocking.lock().unwrap().clear();
        self.exec_preparation_changed.notify_waiters();
        let released = self
            .sched
            .lock()
            .unwrap()
            .release_all_physical_process_exits();
        if released != 0 {
            trace!("released {released} final physical process-exit barrier(s)");
        }
    }

    /// Unrecoverable fatal erorr. Bring things to a close cleanly, but as quickly as
    /// possible.
    pub fn force_shutdown_with_error(&self) {
        let start = std::time::Instant::now();
        let sched = loop {
            if start.elapsed().as_millis() > 1000 {
                eprintln!(
                    "Could not acquire scheduler lock during forced shutdown (timeout)... proceeding anyway."
                );
                return;
            }
            match self.sched.try_lock() {
                Ok(guard) => {
                    break guard;
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::yield_now();
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "Could not acquire scheduler lock during forced shutdown ({})... proceeding anyway.",
                        e
                    );
                    return;
                }
            }
        };
        info!("Scheduler state at exit:\n{}", sched.full_summary());
    }

    /// Inspect unfinished work without acknowledging effects, consuming receipts,
    /// removing the engine, or publishing a trace. This reports the first pending
    /// operation (if any), before the ordinary unconsumed-trace checks.
    fn network_refusal_state_diagnostic(&self) -> String {
        match &self.network_engine {
            None => "network engine unavailable".to_owned(),
            Some(engine) => match engine.try_lock() {
                Ok(engine) => format!("read-only completion check: {:?}", engine.finish()),
                Err(error) => format!("network state unavailable: {error}"),
            },
        }
    }

    fn shutdown_for_network_refusal(&self, refusal: &NetworkPolicyRefusal) -> ! {
        // The explicit controller capability proves exiting this process ends
        // the owned PID namespace. Do not await a guest RPC/turn or run normal
        // finalization: unresolved effects remain unresolved and nothing is
        // published as a successful recording. Synchronous stderr preserves
        // the typed reason when process::exit bypasses tracing guard drops.
        let _ = writeln!(
            crate::util::RetryingStderr,
            "hermit: network replay policy refusal ({:?}): {refusal}\nhermit: network state not finalized or published; {}",
            refusal.reason(),
            self.network_refusal_state_diagnostic(),
        );
        // Existing diagnostic lock acquisition is bounded to one second.
        self.force_shutdown_with_error();
        exit_owned_controller(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-744): Review explicit abnormal-backend scheduler cancellation.
    /// Cancels the internally spawned scheduler task after a backend guest exits abnormally.
    ///
    /// External-scheduler states do not own a task and are left unchanged.
    pub async fn cancel_internal_scheduler(&mut self) {
        if let Some(handle) = self.sched_handle.take() {
            handle.abort();
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => panic!("cancelled scheduler task panicked: {error}"),
            }
        }
    }

    /// Consume failed-run state after the scheduler has naturally finished.
    ///
    /// A failed run has no successful run summary. Preserve the scheduler's
    /// join error and any requested partial preemption recording's write error
    /// for the caller, without allowing either to replace the backend failure.
    pub async fn clean_up_after_backend_failure(mut self) -> BackendFailureCleanup {
        let scheduler = if let Some(handle) = self.sched_handle.take() {
            handle.await
        } else {
            Ok(())
        };
        // A scheduler panic can poison this mutex. Its JoinError is returned
        // below; recovering only to finish output must not replace that error.
        let writer = self
            .sched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .preemption_writer
            .take();
        let preemption_recording = if self.cfg.record_preemptions_to.is_some() {
            writer.map_or(Ok(()), |writer| writer.flush())
        } else {
            // The writer's destination is fixed from this same configuration.
            // In-memory-only recordings have no Drop write to finish.
            drop(writer);
            Ok(())
        };
        BackendFailureCleanup {
            scheduler,
            preemption_recording,
        }
    }

    /// Shut down anything running, in particular wait on the scheduler.
    ///
    /// This is basically the destructor for the global state, but is here rather than in the
    /// Drop instance so that it can be async, and is more explicitly sequenced in the program.
    ///
    /// Print a summary of the execution, typically called when it is complete.
    ///
    /// If the boolean argument is true, print to stderr, otherwise only print the summary
    /// to the log.
    pub async fn clean_up(
        mut self,
        to_stderr: bool,
        print_summary_to_json_file: &Option<PathBuf>,
    ) -> anyhow::Result<()> {
        if let Some(handle) = self.sched_handle.take() {
            debug!("Global state cleanup, confirming scheduler has shut down...");
            handle.await.expect("Global scheduler clean shutdown");
            debug!("Global state cleanup, continuing...");
        }
        if let Some(runtime) = &self.network_runtime {
            // Backend wait and scheduler join precede this cleanup. Collection
            // and final read retirement must finish before trace publication;
            // a lost final guest RPC cannot turn an unapplied receipt into success.
            runtime.finish_accepted_after_backend().await?;
        }
        let pending_births = self
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .pending_no_seq_birth_count();
        let pending_group = self
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .pending_process_group_change();
        if pending_births != 0 || pending_group {
            anyhow::bail!(
                "backend completion retains {pending_births} unresolved child birth(s) and {} group transition(s)",
                usize::from(pending_group)
            );
        }
        self.finalize_network_trace()?;
        let banner =
            "  ------------------------------ hermit run report ------------------------------";
        let recording_destination = self.cfg.record_preemptions_to.clone();
        let (mut summary, info_reprio_descrip) = self.into_run_summary_for_log().unwrap();

        // Print machine-readable summary:
        if let Some(path) = print_summary_to_json_file {
            let json = serde_json::to_string_pretty(&summary).unwrap();
            fs::write(path, json + "\n").unwrap();
        }

        // Print human-readable summary:
        if to_stderr {
            // In this case, print summary irrespective of logging level.
            // TODO: output summary in machine-readable, JSON form.
            //
            // NOT `eprint!`: a guest that set O_NONBLOCK on the inherited fd 2
            // makes `write_all` fail with EAGAIN here, which panics the print
            // macro and loses both the summary and the panic message. See
            // `crate::util::RetryingStderr`.
            {
                use std::io::Write;
                let _ = write!(crate::util::RetryingStderr, "{}\n{}", banner, summary);
            }
        } else {
            // Separate out the nondeterministic bits and print them at debug level:
            let rt = summary.realtime_elapsed.take();
            log_run_summary(
                banner,
                &summary,
                info_reprio_descrip.as_deref(),
                recording_destination.as_deref(),
            );
            if let Some(x) = rt {
                debug!("Nondeterministic realtime elapsed: {:?}", x);
            }
        }
        Ok(())
    }

    /// Finish the shared engine and durably close its sidecar before a caller
    /// publishes successful run metadata.  Replay refuses unconsumed input or
    /// output; record validates the complete V2 trace before writing it.
    pub fn finalize_network_trace(&mut self) -> anyhow::Result<()> {
        let Some(engine) = self.network_engine.take() else {
            return Ok(());
        };
        let scheduler_engine = self
            .sched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_network_engine();
        if let Some(scheduler_engine) = &scheduler_engine
            && !Arc::ptr_eq(&engine, scheduler_engine)
        {
            bail!("scheduler and RPC paths used different network engines");
        }
        drop(scheduler_engine);
        let engine = Arc::try_unwrap(engine)
            .map_err(|_| anyhow::anyhow!("network engine still has live users at finalization"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("network engine mutex was poisoned"))?;
        match self.cfg.network_trace.policy {
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => {
                bail!("non-engine network policy unexpectedly owned an engine")
            }
            NetworkPolicy::Replay => engine.finish().map_err(|error| {
                NetworkRpcError::from_engine(
                    self.cfg.network_trace.policy,
                    NetworkFailurePhase::Completion,
                    error,
                )
                .into_error()
            }),
            NetworkPolicy::Record => {
                let trace = engine
                    .into_recorded_versioned_trace()
                    .map_err(|error| anyhow::anyhow!("invalid recorded network trace: {error}"))?;
                let no_channels = match &trace {
                    NetworkTrace::V1(trace) => trace.channels.is_empty(),
                    NetworkTrace::V2(trace) => trace.channels.is_empty(),
                    NetworkTrace::V3(trace) => trace.history.channels.is_empty(),
                    NetworkTrace::V4(trace) => trace.channels.is_empty(),
                };
                if no_channels && !self.cfg.recordreplay_modes {
                    bail!("network recording captured no external channels");
                }
                let inherited = self.cfg.network_trace_output_fd.ok_or_else(|| {
                    anyhow::anyhow!("network record policy omitted its reserved host output")
                })?;
                let duplicate = unsafe { libc::fcntl(inherited, libc::F_DUPFD_CLOEXEC, inherited) };
                if duplicate < 0 {
                    bail!(
                        "cannot duplicate reserved network trace output: {}",
                        std::io::Error::last_os_error()
                    );
                }
                // SAFETY: fcntl returned a fresh owned descriptor.
                let mut file = unsafe { File::from_raw_fd(duplicate) };
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                trace.write_framed(&mut file).map_err(|error| {
                    anyhow::anyhow!("cannot encode reserved network trace output: {error}")
                })?;
                file.sync_all()?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    fn into_run_summary(self) -> anyhow::Result<RunSummary> {
        self.into_run_summary_for_log().map(|(summary, _)| summary)
    }

    fn into_run_summary_for_log(self) -> anyhow::Result<(RunSummary, Option<String>)> {
        // First, the scheduler can generate part of the summary (and flush once).
        let (mut summary, info_reprio_descrip) = {
            let mut sched = self.sched.lock().unwrap();
            sched.generate_partial_run_summary_for_log(self.cfg.record_preemptions_to.as_ref())?
        };
        // Second, we fill in the rest based on global state.
        //
        // Real time report:
        // N.B.: We don't have a job-level exit hook atm (T76248597), so we use the
        // CURRENT time -- that we are calling summarize -- as the end time:
        summary.realtime_elapsed = Some(self.realtime_start.elapsed()?);

        if self.cfg.virtualize_time {
            let final_time = self.global_time.lock().unwrap();
            let final_time_ns = final_time.as_nanos();
            let nanos = self
                .cfg
                .epoch
                .timestamp_nanos_opt()
                .expect("epoch cannot be represented in a timestamp with nanosecond precision")
                as u64;
            let epoch_ns = LogicalTime::from_nanos(nanos);
            summary.virttime_final = final_time_ns.as_nanos();
            summary.virttime_elapsed = if final_time_ns.as_nanos() >= epoch_ns.as_nanos() {
                (final_time_ns - epoch_ns).as_nanos()
            } else {
                bail!(
                    "Internal invariant violated! Global time is before epoch start {}",
                    epoch_ns
                );
            }
        }

        Ok((summary, info_reprio_descrip))
    }
}

fn log_run_summary(
    banner: &str,
    summary: &RunSummary,
    info_reprio_descrip: Option<&str>,
    recording_destination: Option<&std::path::Path>,
) {
    info!("\n{}\n{}", banner, summary.info(info_reprio_descrip));
    debug!(
        replayed_events = summary.schedevent_replayed,
        ?recording_destination,
        "Run recording/replay bookkeeping"
    );
}

#[reverie::global_tool]
impl GlobalTool for GlobalState {
    type Config = Config;

    /// A request asks the scheduler to perform an RPC, which includes multiple kinds of
    /// actions, and, most importantly, permission to acquire resources and run the guest thread.
    ///
    /// Irrespective of which method we execute, we can "tick" our local component of the
    /// global time in the process.
    type Request = (DetTime, MmId, GlobalRequest);

    /// Response from the global portion of the Detcore instrumentation tool.
    /// The exact form of the response depends on which method was executed.
    ///
    /// Irrespective of which method was called, the global handling may have consumed
    /// logical time, in which case the scheduler can send a new thread-local time back to
    /// the caller.  Unfortunately, information is lost as this is collapsed to a flat
    /// scalar instead of a rich `DetTime`.
    type Response = (Option<LogicalTime>, GlobalResponse);

    /// Called once during startup.
    async fn init_global_state(cfg: &Config) -> GlobalState {
        let runtime = crate::network_runtime::take_network_runtime_resources()
            .expect("one scoped runtime resource belongs to exactly one global initializer");
        let mut state = GlobalState::initialize(cfg, true);
        state.network_runtime = runtime;
        state
    }

    fn install_backend_signal_control(
        &self,
        control: Option<reverie::BackendSignalControl>,
    ) -> Result<reverie::BackendSignalControlMode, reverie::Error> {
        self.sched.lock().unwrap().install_signal_control(control)
    }

    fn authorize_backend_signal_boundary(
        &self,
        task: reverie::SignalTaskIdentity,
    ) -> Result<Option<reverie::SignalDeliveryPermit>, reverie::Error> {
        self.sched.lock().unwrap().authorize_signal_boundary(task)
    }

    async fn on_backend_signal_boundary(
        &self,
        receipt: reverie::SignalBoundaryReceipt,
    ) -> Result<(), reverie::Error> {
        self.sched.lock().unwrap().consume_signal_boundary(receipt)
    }

    fn report_backend_failure(&self, event: reverie::BackendFailure) {
        let (wake, deferred) = {
            let mut sched = self.sched.lock().unwrap();
            (
                sched.report_backend_failure(event),
                sched.take_signal_failure_wakes(),
            )
        };
        self.exec_preparation_changed.notify_waiters();
        self.network_stream_changed.notify_waiters();
        for wake in deferred {
            let _ = wake.send(());
        }
        if let Some(wake) = wake {
            // No waiter can begin consuming cleanup until the scheduler has
            // closed its selected transaction under the grant/commit mutex.
            let _ = wake.send(());
        }
    }

    async fn wait_for_backend_failure(&self) {
        let wake = self.sched.lock().unwrap().backend_failure_waiter();
        wake.await
            .expect("GlobalState owns the failure sender until publication");
    }

    async fn on_backend_child_wait_event(
        &self,
        event: reverie::BackendChildWaitEvent,
    ) -> Result<(), reverie::Error> {
        // KVM polls this future through its first suspension before exposing
        // waitability. The helper's first poll performs the complete
        // generation-bound publication under the scheduler mutex, then
        // self-wakes and returns Pending; its second poll consumes the typed
        // result and releases the stage-2 control barrier.
        crate::scheduler::signal_control::ChildExitPublicationFuture::new(self.sched.clone(), event)
            .await
    }

    async fn receive_rpc(&self, from: Tid, gr: Self::Request) -> Self::Response {
        type R = GlobalResponse;
        let dtid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME
        let (guest_time, request_mm, request) = gr;
        let time_from_guest = guest_time.as_nanos();
        let mut completed_group_change = None;
        if let GlobalRequest::PrepareNetworkNativeBirth(permit, syscall) = &request {
            let owner = NetworkStreamOwner {
                thread: dtid,
                mm: request_mm,
            };
            let preparation = (|| {
                let sched = self.sched.lock().unwrap();
                if owner != permit.owner
                    || sched.backend_failed()
                    || sched.thread_is_logically_killed(dtid)
                    || !sched.thread_was_registered(dtid)
                    || !sched.rpc_incarnation_matches(dtid, request_mm)
                    || self.registered_exec_mms.lock().unwrap().get(&dtid) != Some(&request_mm)
                {
                    return Err(anyhow::anyhow!(
                        "native clone preparation lost original owner"
                    ));
                }
                self.network_engine
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("native clone lost engine"))?
                    .lock()
                    .unwrap()
                    .prepare_native_birth_escrow(owner, *permit)?;
                self.network_runtime
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("native clone lost runtime"))
            })();
            let outcome = match preparation {
                Ok(runtime) => runtime
                    .prepare_native_birth(*permit, *syscall)
                    .await
                    .map_err(|e| e.to_string()),
                Err(error) => Err(error.to_string()),
            };
            return (None, R::NetworkNativeBirthPrepared(outcome));
        }
        if let GlobalRequest::CollectNetworkNativeBirth(permit, returned) = &request {
            let outcome = if permit.owner
                != (NetworkStreamOwner {
                    thread: dtid,
                    mm: request_mm,
                }) {
                Err("native clone completion changed owner".into())
            } else if let Some(runtime) = &self.network_runtime {
                runtime
                    .collect_native_birth(*permit, *returned)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                Err("native clone completion lost runtime".into())
            };
            return (None, R::NetworkNativeBirthCollected(outcome));
        }
        if let GlobalRequest::PrepareNoSeqBirth {
            process,
            syscall_count,
            flags,
            child_tid_addr,
            exit_signal,
            priority_entropy,
            fd_permit,
        } = &request
        {
            loop {
                let changed = self.exec_preparation_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut sched = self.sched.lock().unwrap();
                    let owner = NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    };
                    let valid = !sched.backend_failed()
                        && (!self.cfg.sequentialize_threads
                            || (fd_permit.is_some() && self.network_runtime.is_some()))
                        && !sched.thread_is_logically_killed(dtid)
                        && sched.thread_was_registered(dtid)
                        && sched.rpc_incarnation_matches(dtid, request_mm)
                        && self.registered_exec_mms.lock().unwrap().get(&dtid) == Some(&request_mm);
                    if !valid {
                        return (None, R::PrepareNoSeqBirth(None));
                    }
                    let valid_permit = match &self.network_engine {
                        Some(engine) => {
                            let engine = engine.lock().unwrap();
                            if engine.fd_table_capability() {
                                fd_permit.is_some_and(|permit| {
                                    engine
                                        .validate_child_birth_permit(owner, permit, *flags)
                                        .is_ok()
                                })
                            } else {
                                fd_permit.is_none()
                            }
                        }
                        None => fd_permit.is_none(),
                    };
                    if !valid_permit {
                        return (None, R::PrepareNoSeqBirth(None));
                    }
                    if !sched.thread_tree.no_seq_birth_admission_busy() {
                        let birth = sched.thread_tree.prepare_no_seq_birth(
                            owner,
                            *process,
                            crate::resources::ExternalOpId::new(dtid, *syscall_count), (*flags,
                            *child_tid_addr,
                            *exit_signal),
                            *priority_entropy,
                            *fd_permit,
                        );
                        return (None, R::PrepareNoSeqBirth(birth));
                    }
                }
                changed.await;
            }
        }
        if let GlobalRequest::SubmitNoSeqBirth(birth) = &request {
            let mut sched = self.lock_rpc_scheduler(false).await;
            let valid = (!self.cfg.sequentialize_threads
                || (birth.fd_permit().is_some() && self.network_runtime.is_some()))
                && birth.parent()
                    == (NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    })
                && !sched.thread_is_logically_killed(dtid)
                && sched.rpc_incarnation_matches(dtid, request_mm)
                && self.registered_exec_mms.lock().unwrap().get(&dtid) == Some(&request_mm);
            return (
                None,
                R::PrepareNoSeqBirth(if valid {
                    sched.thread_tree.submit_no_seq_birth(birth)
                } else {
                    None
                }),
            );
        }
        if let GlobalRequest::NoSeqBirthOwnerGone(uninvoked, uninvoked_fd_clone) = &request {
            // Consuming cleanup bypasses the backend-failure wait, exactly as
            // lock_rpc_scheduler(true). Retain its inputs before any await.
            let (runtime, cleanup) = 'retained_cleanup: {
                let mut sched = self.sched.lock().unwrap();
                let owner = NetworkStreamOwner {
                    thread: dtid,
                    mm: request_mm,
                };
                if (self.cfg.sequentialize_threads && uninvoked_fd_clone.is_none())
                    || uninvoked.as_ref().is_some_and(|marker| {
                        !sched
                            .thread_tree
                            .validate_uninvoked_wait_call(owner, marker)
                    })
                {
                    return (None, R::NoSeqBirthOwnerGone(false));
                }
                let birth_clone = uninvoked
                    .as_ref()
                    .and_then(|marker| marker.clone_submission());
                if birth_clone.is_some_and(|(permit, flags)| {
                    uninvoked_fd_clone.as_ref().is_none_or(|admission| {
                        admission.publication.permit != permit
                            || admission.kind
                                != (crate::network_replay::NetworkFdMutationKind::Clone { flags })
                    })
                }) || uninvoked_fd_clone
                    .as_ref()
                    .is_some_and(|admission| admission.publication.permit.owner != owner)
                {
                    return (None, R::NoSeqBirthOwnerGone(false));
                }
                if let Some(admission) = uninvoked_fd_clone {
                    let Some(engine) = &self.network_engine else {
                        return (None, R::NoSeqBirthOwnerGone(false));
                    };
                    if engine
                        .lock()
                        .unwrap()
                        .validate_uninvoked_clone_admission(admission)
                        .is_err()
                    {
                        return (None, R::NoSeqBirthOwnerGone(false));
                    }
                    if let Some(runtime) = &self.network_runtime {
                        use crate::network_runtime::native_birth::NativeBirthCleanupRequest;
                        let retained = runtime.retain_native_birth_cleanup(
                            NativeBirthCleanupRequest::Uninvoked {
                                admission: Box::new(admission.clone()),
                                marker: uninvoked.clone(),
                            },
                            self.native_birth_cleanup_recovery()
                                .expect("validated cleanup has network engine"),
                        );
                        let cleanup = match retained {
                            Ok(cleanup) => cleanup,
                            Err(_) => return (None, R::NoSeqBirthOwnerGone(false)),
                        };
                        if let Some(cleanup) = cleanup {
                            // The real marker and exact admission now belong to the
                            // existing preparation, including a lost Prepare reply.
                            break 'retained_cleanup (runtime, cleanup);
                        }
                    }
                    let retired = {
                        let mut engine = engine.lock().unwrap();
                        if engine.cancel_uninvoked_clone_admission(admission).is_err() {
                            return (None, R::NoSeqBirthOwnerGone(false));
                        }
                        engine.take_lifetime_retired_ports()
                    };
                    self.release_lifetime_ports(retired);
                }
                let retired = sched
                    .thread_tree
                    .retire_no_seq_wait_owner(owner, uninvoked.as_ref());
                drop(sched);
                self.exec_preparation_changed.notify_waiters();
                self.network_stream_changed.notify_waiters();
                if retired
                    && let Some(admission) = uninvoked_fd_clone
                    && let Some(runtime) = &self.network_runtime
                    && runtime
                        .native_birth_semantics_consumed(admission.publication.permit, None, true)
                        .is_err()
                    {
                        self.report_backend_failure(reverie::BackendFailure {
                            pid: from,
                            tid: from,
                            phase: "uninvoked birth semantic retirement lost command custody",
                        });
                        return (None, R::NoSeqBirthOwnerGone(false));
                    }
                return (None, R::NoSeqBirthOwnerGone(retired));
            };
            let retired = runtime.wait_native_birth_cleanup(&cleanup).await.is_ok();
            if !retired {
                self.report_backend_failure(reverie::BackendFailure {
                    pid: from,
                    tid: from,
                    phase: "known-uninvoked native birth cleanup failed",
                });
            }
            return (None, R::NoSeqBirthOwnerGone(retired));
        }
        if let GlobalRequest::CancelNoSeqBirth(birth, errno) = &request {
            let cleanup = 'retained_cleanup: {
                let mut sched = self.lock_rpc_scheduler(true).await;
                let valid = (!self.cfg.sequentialize_threads
                    || (birth.fd_permit().is_some() && self.network_runtime.is_some()))
                    && birth.parent()
                        == (NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        })
                    && (1..=4095).contains(errno);
                if valid && sched.thread_tree.failed_birth_matches(birth, *errno) {
                    match self.retain_failed_native_birth(birth, *errno) {
                        Ok(Some(cleanup)) => break 'retained_cleanup cleanup,
                        Err(_) => return (None, R::CancelNoSeqBirth(false)),
                        Ok(None) => {}
                    }
                }
                let canceled = valid && self.settle_failed_no_seq_birth(&mut sched, birth, *errno);
                drop(sched);
                if canceled {
                    self.exec_preparation_changed.notify_waiters();
                    self.network_stream_changed.notify_waiters();
                }
                return (None, R::CancelNoSeqBirth(canceled));
            };
            let canceled = self
                .network_runtime
                .as_ref()
                .unwrap()
                .wait_native_birth_cleanup(&cleanup)
                .await
                .is_ok();
            if !canceled {
                self.report_backend_failure(reverie::BackendFailure {
                    pid: from,
                    tid: from,
                    phase: "known native errno cleanup failed",
                });
            }
            return (None, R::CancelNoSeqBirth(canceled));
        }
        if let GlobalRequest::JoinNoSeqBirth(birth, child) = &request {
            loop {
                let changed = self.exec_preparation_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let parent_continuation = {
                    let mut sched = self.sched.lock().unwrap();
                    if (self.cfg.sequentialize_threads && birth.fd_permit().is_none())
                        || sched.backend_failed()
                        || birth.parent()
                            != (NetworkStreamOwner {
                                thread: dtid,
                                mm: request_mm,
                            })
                        || sched.thread_is_logically_killed(dtid)
                        || !sched.rpc_incarnation_matches(dtid, request_mm)
                        || self.registered_exec_mms.lock().unwrap().get(&dtid) != Some(&request_mm)
                    {
                        return (None, R::JoinNoSeqBirth(false));
                    }
                    let native = if birth.fd_permit().is_some() && self.network_runtime.is_some() {
                        let mut rebound = birth.clone();
                        if sched.thread_tree.rebind_native_birth(&mut rebound).is_err() {
                            return (None, R::JoinNoSeqBirth(false));
                        }
                        match rebound.construction() {
                            Ok(Some(outcome)) => Some(outcome),
                            _ => return (None, R::JoinNoSeqBirth(false)),
                        }
                    } else {
                        None
                    };
                    if let Some(joined) = sched.thread_tree.join_no_seq_birth(birth, *child) {
                        let parent_continues = joined
                            && self.cfg.sequentialize_threads
                            && native.as_ref().is_some_and(|outcome| {
                                !outcome.admission().terminal()
                                    && !outcome.flags().contains(CloneFlags::CLONE_VFORK)
                                    && !(self.cfg.backend_serializes_fork_children
                                        && !outcome.flags().contains(CloneFlags::CLONE_THREAD))
                            });
                        if !parent_continues {
                            return (None, R::JoinNoSeqBirth(joined));
                        }
                        Some(native)
                    } else {
                        None
                    }
                };
                if let Some(_native_outcome) = parent_continuation {
                    // Child publication and parent scheduling are separate
                    // consumers. This RPC is the original parent caller.
                    let mut resources = Resources::new(birth.process());
                    resources.insert(
                        ResourceID::ParentContinue {
                            parent: dtid,
                            child: *child,
                        },
                        Permission::W,
                    );
                    let result = self
                        .recv_grant_resources(
                            from,
                            birth.process(),
                            resources,
                            Some(request_mm),
                            RpcOrigin::ParentContinue,
                        )
                        .await
                        .0;
                    return (
                        None,
                        R::JoinNoSeqBirth(matches!(result, SchedulerRpcResult::Continue(_))),
                    );
                }
                changed.await;
            }
        }
        if let GlobalRequest::PrepareProcessGroupChange(kind, sequence) = &request {
            loop {
                let changed = self.exec_preparation_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut sched = self.sched.lock().unwrap();
                    let owner = NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    };
                    if self.cfg.sequentialize_threads
                        || sched.backend_failed()
                        || sched.thread_is_logically_killed(dtid)
                        || !sched.thread_was_registered(dtid)
                        || !sched.rpc_incarnation_matches(dtid, request_mm)
                        || self.registered_exec_mms.lock().unwrap().get(&dtid) != Some(&request_mm)
                        || matches!(kind, crate::scheduler::ProcessGroupChangeKind::Session { process } if sched.registered_process(dtid) != Some(*process))
                    {
                        return (None, R::ProcessGroupChange(None));
                    }
                    if !sched.thread_tree.process_group_admission_busy() {
                        let change = sched.thread_tree.prepare_process_group_change(
                            owner,
                            crate::resources::ExternalOpId::new(dtid, *sequence),
                            *kind,
                        );
                        return (None, R::ProcessGroupChange(change));
                    }
                }
                changed.await;
            }
        }
        if let GlobalRequest::SubmitProcessGroupChange(change) = &request {
            let mut sched = self.sched.lock().unwrap();
            let valid = !self.cfg.sequentialize_threads
                && !sched.backend_failed()
                && change.owner()
                    == (NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    })
                && !sched.thread_is_logically_killed(dtid)
                && sched.rpc_incarnation_matches(dtid, request_mm)
                && self.registered_exec_mms.lock().unwrap().get(&dtid) == Some(&request_mm);
            return (
                None,
                R::ProcessGroupChange(if valid {
                    sched.thread_tree.submit_process_group_change(change)
                } else {
                    None
                }),
            );
        }
        if let GlobalRequest::CompleteProcessGroupChange(change, result) = &request {
            let mut sched = self.sched.lock().unwrap();
            // Exact retained completion remains useful if the sender retired;
            // no current numeric-PID admission or guest success is reconstructed.
            let valid = !self.cfg.sequentialize_threads
                && change.owner()
                    == (NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    });
            let completed = valid
                && sched
                    .thread_tree
                    .complete_process_group_change(change, *result);
            drop(sched);
            if completed {
                self.exec_preparation_changed.notify_waiters();
            }
            if result.is_err() {
                // The original handler returned native errno before its mirror
                // RPC. Preserve that no-accounting path after settling custody.
                return (None, R::CompleteProcessGroupChange(completed));
            }
            // Keep the original post-syscall RPC clock/accounting epilogue for
            // the live caller. Known physical completion is already retained
            // if ordinary admission observes owner retirement meanwhile.
            completed_group_change = Some(completed);
        }
        if let GlobalRequest::Network(NetworkRequest::CaptureAcceptedReturn {
            lease,
            kernel_result,
        }) = &request
        {
            // Ptrace's GlobalRPC forwards inline. This branch contains no await:
            // its first poll latches the return and actual pin before yielding.
            return (
                None,
                self.recv_capture_accepted_return(
                    NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    },
                    *lease,
                    *kernel_result,
                ),
            );
        }
        if let GlobalRequest::Network(NetworkRequest::CollectAcceptedEffect { lease }) = &request {
            return (
                None,
                self.recv_collect_accepted_effect(
                    NetworkStreamOwner {
                        thread: dtid,
                        mm: request_mm,
                    },
                    *lease,
                )
                .await,
            );
        }
        if let GlobalRequest::Network(NetworkRequest::NativeOriginalConnectFailed {
            local,
            detail,
        }) = &request
        {
            // This failure-only path must precede ordinary RPC admission: that
            // admission parks after backend failure. ACK publication here;
            // only the owning Guest future stays pending until ptrace's existing
            // failure arm finally waits for its exact task.
            self.fail_original_connect(
                from,
                NetworkStreamOwner {
                    thread: dtid,
                    mm: request_mm,
                },
                local,
                detail,
            );
            return (None, R::Network(Ok(NetworkReply::Unit)));
        }
        if let GlobalRequest::OriginalConnectOwnerGone(local) = &request {
            let owner = NetworkStreamOwner {
                thread: dtid,
                mm: request_mm,
            };
            let settled = self.network_engine.as_ref().is_some_and(|engine| {
                let mut engine = engine.lock().unwrap();
                let settled = engine.original_connect_consumed(owner, local).is_ok();
                self.release_lifetime_ports(engine.take_lifetime_retired_ports());
                settled
            });
            self.network_stream_changed.notify_waiters();
            return (None, R::OriginalConnectOwnerGone(settled));
        }
        if matches!(request, GlobalRequest::NetworkOwnerGone) {
            self.recv_network_owner_gone(NetworkStreamOwner {
                thread: dtid,
                mm: request_mm,
            });
            return (None, R::NetworkOwnerGone);
        }
        if let GlobalRequest::SignalDequeued {
            detpid,
            identity,
            dequeue,
        } = &request
        {
            return self
                .recv_signal_dequeued(dtid, request_mm, guest_time, *detpid, *identity, *dequeue)
                .await;
        }

        let is_deregister = matches!(&request, GlobalRequest::DeregisterThread(_));
        let consuming_cleanup =
            is_deregister || matches!(&request, GlobalRequest::RobustListWakes(_));

        let (exec_reconnect, is_exec_caller_after_local_mm_swap) = {
            let pending = self.pending_exec_states.lock().unwrap();
            let reconnect = match &request {
                GlobalRequest::CreateChildThread(child, process, _, None, _, _, _)
                    if *child == dtid && *child == *process =>
                {
                    pending.get(process).cloned()
                }
                _ => None,
            };
            let is_exec_caller_after_local_mm_swap = pending.values().any(|state| {
                state.receipt.caller == dtid
                    && state.receipt.mm.for_exec(state.receipt.process) == request_mm
            });
            (reconnect, is_exec_caller_after_local_mm_swap)
        };

        // Tombstones reject raw Linux TID reuse except for the kernel-defined leader-TID takeover
        // recorded by a successful non-leader exec. Hold the scheduler admission lock through
        // clock accounting so logical teardown cannot linearize between the two.
        let mut tombstoned_deregistration = None;
        {
            let sched = self.lock_rpc_scheduler(consuming_cleanup).await;
            if exec_reconnect.is_none()
                && !is_exec_caller_after_local_mm_swap
                && (!sched.rpc_incarnation_matches(dtid, request_mm)
                    || self
                        .registered_exec_mms
                        .lock()
                        .unwrap()
                        .get(&dtid)
                        .is_some_and(|registered| *registered != request_mm))
            {
                debug!(
                    "[detcore, dtid {}] rejecting {:?} RPC from retired exec incarnation {:?}",
                    dtid, request, request_mm,
                );
                return if is_deregister {
                    (None, R::DeregisterThread(()))
                } else {
                    (None, R::ThreadExited)
                };
            }
            if let GlobalRequest::DeregisterThread(owner) = &request {
                assert_eq!(
                    owner.dettid, dtid,
                    "deregistration must belong to its sender"
                );
                assert_eq!(owner.mm, request_mm, "deregistration must retain its MmId");
                // NoSeq has no scheduler tombstone by default. Its consuming
                // callback still accounts exactly once, before any late clock
                // can replace the final value or create another admission.
                if !self.cfg.sequentialize_threads && sched.deregistration_was_accounted(dtid) {
                    return (None, R::DeregisterThread(()));
                }
                // DBT can reject StartNewThread before parent registration.
                // Its tombstone still needs the existing final accounting path.
                if !sched.thread_was_registered(dtid) && !sched.thread_is_logically_killed(dtid) {
                    assert!(
                        sched.backend_failed() || !owner.thread_start_entered,
                        "a started thread must have a scheduler registration before deregistration"
                    );
                    // The backend still consumes this constructed ThreadState,
                    // but no guest start/registration happened. Acknowledge its
                    // cleanup without creating a clock, tree entry or admission.
                    return (None, R::DeregisterThread(()));
                }
            }
            let child = match &request {
                GlobalRequest::CreateChildThread(child, ..)
                | GlobalRequest::CreateVforkChildThread(_, _, child, ..) => Some(*child),
                GlobalRequest::CreateNoSeqChildThread(birth, ..) => birth.child(),
                _ => None,
            };
            if sched.thread_is_logically_killed(dtid) && exec_reconnect.is_none() {
                trace!(
                    "[detcore, dtid {}] rejecting RPC after permanent logical-thread removal",
                    dtid
                );
                if let GlobalRequest::DeregisterThread(deregistration) = &request {
                    tombstoned_deregistration = Some(deregistration.clone());
                } else {
                    return (None, R::ThreadExited);
                }
            }
            if child.is_some_and(|child| sched.thread_is_logically_killed(child))
                && exec_reconnect.is_none()
            {
                trace!(
                    "[detcore, dtid {}] rejecting registration that reuses a tombstoned child TID",
                    dtid
                );
                return (None, R::ThreadExited);
            }

            let is_thread_reconnect = matches!(
                &request,
                GlobalRequest::StartNewThread(child_dettid, ..) if *child_dettid == dtid
            ) && self.global_time.lock().unwrap().contains_thread(dtid);

            // This portion of the time updates "asynchronously", and we can tick it on every rpc:
            // TODO: eventually the vector clock should be in shared memory, and
            // the local clocks should update truly asynchrously.  Therefore it
            // SHOULD be safe to always push through this update on any rpc.
            if tombstoned_deregistration.is_none()
                && exec_reconnect.is_none()
                && !is_thread_reconnect
            {
                self.global_time.lock().unwrap().update_global_time(
                    dtid,
                    time_from_guest,
                    guest_time.inherited_nanos(),
                );
            }
        }
        if let Some(deregistration) = tombstoned_deregistration {
            self.recv_deregister_thread(from, deregistration).await;
            return (None, R::DeregisterThread(()));
        }

        // RPC boilerplate. (Hard to generate systematically now though, because of the
        // time payload piggy-backing on each rpc. Maybe eventually once ticking a
        // threads' own clock happens through shared memory.)
        #[allow(clippy::unit_arg)]
        let resp = match request {
            GlobalRequest::SignalDequeued { .. } => {
                unreachable!("consuming path handled before ordinary cancellation")
            }
            GlobalRequest::ParkedRequest(rs, pid, capability) => {
                let (response, _) = self
                    .recv_resources_with_origin(
                        from,
                        pid,
                        rs,
                        Some(request_mm),
                        RpcOrigin::DirectRequestResources,
                        capability,
                    )
                    .await;
                match response {
                    SchedulerRpcResult::Continue(r) => R::ParkedRequest(r),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::ResumeParkedRequest {
                ticket,
                current_site,
            } => {
                self.recv_resume_parked(from, request_mm, ticket, current_site)
                    .await
            }
            GlobalRequest::FinishParkedObservation {
                wait,
                lease,
                site,
                finish,
            } => {
                let ack = Ivar::new();
                let intent = ControlIntent::Finish {
                    wait,
                    lease,
                    site,
                    finish,
                    ack: ack.clone(),
                };
                let posted = self
                    .sched
                    .lock()
                    .unwrap()
                    .post_control(dtid, request_mm, intent);
                if let Err(error) = posted {
                    self.sched.lock().unwrap().fail_parked(dtid, error);
                    return (None, R::ThreadExited);
                }
                R::FinishParkedObservation(ack.await)
            }
            GlobalRequest::ParkedProtocolFailure(error) => {
                self.sched.lock().unwrap().fail_parked(dtid, error);
                return (None, R::ThreadExited);
            }

            GlobalRequest::RequestResources(rs, pid) => {
                let (response, _endtime) = self
                    .recv_request_resources(from, pid, rs, Some(request_mm))
                    .await;
                match response {
                    SchedulerRpcResult::Continue(response) => R::RequestResources(response),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::ReleaseResources(rs) => {
                R::ReleaseResources(self.recv_release_resources(from, rs).await)
            }
            GlobalRequest::ReleaseAllResources => {
                R::ReleaseAllResources(self.recv_release_all_resources(from).await)
            }
            // TODO-HUMAN-REVIEW(PR-643): Review run-wide unsupported-syscall aggregation.
            GlobalRequest::ReportUnsupportedSyscall(name) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let inserted = self
                    .unsupported_syscalls
                    .lock()
                    .unwrap()
                    .insert(name.clone());
                if inserted
                    && let Some(report) = &self.unsupported_syscall_report_fd
                    && let Err(error) = writeln!(report.lock().unwrap(), "{name}")
                {
                    warn!("failed to append unsupported-syscall report: {error}");
                }
                R::ReportUnsupportedSyscall(())
            }
            GlobalRequest::PrepareExec(process, mm, old_files, fd_blocking) => {
                let result = self
                    .recv_prepare_exec(dtid, process, request_mm, mm, old_files, fd_blocking)
                    .await;
                if result == R::ThreadExited {
                    return (None, result);
                }
                result
            }
            GlobalRequest::CancelExec(receipt) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let mut pending = self.pending_exec_states.lock().unwrap();
                if receipt.caller == dtid
                    && sched.registered_process(dtid) == Some(receipt.process)
                    && request_mm == receipt.mm
                    && pending
                        .get(&receipt.process)
                        .is_some_and(|state| state.receipt == receipt)
                {
                    pending.remove(&receipt.process);
                    self.exec_preparation_changed.notify_waiters();
                }
                R::CancelExec(())
            }
            GlobalRequest::UpdateExecFdBlocking(receipt, fd_blocking) => {
                let sched = self.lock_rpc_scheduler(false).await;
                let mut pending = self.pending_exec_states.lock().unwrap();
                let updated = receipt.caller == dtid
                    && sched.registered_process(dtid) == Some(receipt.process)
                    && request_mm == receipt.mm
                    && pending
                        .get(&receipt.process)
                        .is_some_and(|state| state.receipt == receipt);
                if updated {
                    pending
                        .get_mut(&receipt.process)
                        .expect("checked preparation")
                        .fd_blocking = fd_blocking;
                }
                R::UpdateExecFdBlocking(updated)
            }
            GlobalRequest::MarkPastFirstExecve(signal_identity) => {
                let mut sched = self.lock_rpc_scheduler(false).await;
                let pid = sched.registered_process(dtid);
                let mut consumed_files = None;
                if self.cfg.kvm_shared_dequeue_timers {
                    let result = (|| {
                        let identity = signal_identity.ok_or(ProtocolFailure::Identity)?;
                        let pid = pid.ok_or(ProtocolFailure::Identity)?;
                        let mut pending = self.pending_exec_states.lock().unwrap();
                        if let Some(prepared) = pending.get(&pid) {
                            if prepared.receipt.caller != dtid || prepared.receipt.process != pid {
                                return Err(ProtocolFailure::Identity);
                            }
                            sched.complete_signal_exec(
                                pid,
                                dtid,
                                prepared.receipt.mm,
                                request_mm,
                                identity,
                            )?;
                            consumed_files =
                                Some(pending.remove(&pid).expect("checked preparation").receipt);
                        } else {
                            sched
                                .real_timers
                                .validate_task(pid, dtid, request_mm, identity)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        sched.fail_parked(dtid, error);
                        self.exec_preparation_changed.notify_waiters();
                        return (None, R::ThreadExited);
                    }
                } else if let Some(pid) = pid {
                    let mut pending = self.pending_exec_states.lock().unwrap();
                    if let Some(prepared) = pending.get(&pid) {
                        if prepared.receipt.caller != dtid
                            || prepared.receipt.process != pid
                            || prepared.receipt.mm.for_exec(pid) != request_mm
                        {
                            return (None, R::ThreadExited);
                        }
                        consumed_files =
                            Some(pending.remove(&pid).expect("checked preparation").receipt);
                    }
                }
                if let Some(files) = consumed_files {
                    sched.rebind_ordinary_fd_exec(
                        NetworkStreamOwner {
                            thread: files.caller,
                            mm: files.mm,
                        },
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                    );
                    self.commit_network_exec_files(
                        files,
                        &ExecReconnect {
                            caller: files.caller,
                            new_leader: dtid,
                            detpid: files.process,
                            pre_exec_mm: files.mm,
                            post_exec_mm: request_mm,
                            child_tid_addr: 0,
                            reconnect_priority: None,
                        },
                    );
                    let mut mms = self.registered_exec_mms.lock().unwrap();
                    let retired_owners: Vec<_> = mms
                        .iter()
                        .filter(|(tid, mm)| {
                            sched.registered_process(**tid) == Some(files.process)
                                && (**tid != dtid || **mm != request_mm)
                        })
                        .map(|(tid, mm)| NetworkStreamOwner {
                            thread: *tid,
                            mm: *mm,
                        })
                        .collect();
                    self.abandon_network_owners(retired_owners.into_iter().chain(std::iter::once(
                        NetworkStreamOwner {
                            thread: files.caller,
                            mm: files.mm,
                        },
                    )));
                    mms.retain(|tid, _| sched.registered_process(*tid) != Some(files.process));
                    mms.insert(dtid, request_mm);
                    self.exec_preparation_changed.notify_waiters();
                    assert!(
                        !self.post_exec_files.lock().unwrap().contains_key(&dtid),
                        "exec cannot complete through two lifecycle paths"
                    );
                    consumed_files = Some(files);
                } else {
                    consumed_files = self.post_exec_files.lock().unwrap().remove(&dtid);
                }
                // The actual initial-EXEC consumer must use the replacement
                // table reserved by this same successful exec. Retain its
                // handoff on the existing exec lifecycle entry until physical
                // registration binds it; serialized claims cannot choose it.
                if let Some(files) = consumed_files
                    && sched.thread_tree.is_root(dtid)
                    && self
                        .network_runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.has_initial_table_provider())
                    && self
                        .network_engine
                        .as_ref()
                        .is_some_and(|engine| !engine.lock().unwrap().fd_table_capability())
                {
                    self.post_exec_files.lock().unwrap().insert(dtid, files);
                }
                self.past_first_execve.store(true, SeqCst);
                let overrides = self
                    .post_exec_fd_blocking
                    .lock()
                    .unwrap()
                    .remove(&dtid)
                    .unwrap_or_default();
                trace!(
                    "[detcore, dtid {}] restoring logically blocking descriptors after exec: {:?}",
                    dtid, overrides
                );
                R::MarkPastFirstExecve(overrides, consumed_files)
            }
            // Requested by the parent thread:
            GlobalRequest::CreateChildThread(
                dettid,
                parent_detpid,
                ctid,
                flags,
                exit_signal,
                physical_ids,
                priority,
            ) => {
                if let Some(prepared) = &exec_reconnect {
                    let mut sched = self.lock_rpc_scheduler(false).await;
                    let (pending, post_exec_mm) = {
                        let mut states = self.pending_exec_states.lock().unwrap();
                        let Some(pending) = states.remove(&parent_detpid) else {
                            return (None, R::ThreadExited);
                        };
                        assert_eq!(&pending, prepared);
                        let post_exec_mm = pending.receipt.mm.for_exec(pending.receipt.process);
                        (pending, post_exec_mm)
                    };
                    assert_eq!(pending.receipt.process, parent_detpid);
                    if let Some((physical_pid, physical_tid)) = physical_ids
                        && let Err(open_error) = sched.register_physical_thread(
                            dettid,
                            post_exec_mm,
                            physical_pid,
                            physical_tid,
                        )
                    {
                        error!(
                            "[detcore, dtid {}] failed to register post-exec host process {} thread {}: {}",
                            dettid, physical_pid, physical_tid, open_error,
                        );
                        return (None, R::ThreadExited);
                    }
                    let event = ExecReconnect {
                        caller: pending.receipt.caller,
                        new_leader: dettid,
                        detpid: parent_detpid,
                        pre_exec_mm: pending.receipt.mm,
                        post_exec_mm,
                        child_tid_addr: ctid,
                        reconnect_priority: priority,
                    };
                    self.commit_network_exec_files(pending.receipt, &event);
                    let retired = sched.reconnect_after_exec(event);
                    sched.rebind_ordinary_fd_exec(
                        NetworkStreamOwner {
                            thread: pending.receipt.caller,
                            mm: pending.receipt.mm,
                        },
                        NetworkStreamOwner {
                            thread: dettid,
                            mm: post_exec_mm,
                        },
                    );
                    if pending.receipt.caller != dettid {
                        self.global_time
                            .lock()
                            .unwrap()
                            .reassign_thread(pending.receipt.caller, dettid);
                    }
                    if !pending.fd_blocking.is_empty() {
                        self.post_exec_fd_blocking
                            .lock()
                            .unwrap()
                            .insert(dettid, pending.fd_blocking);
                    }
                    debug!(
                        "[detcore, dtid {}] reconciled successful exec from caller {}; retired prior identities {:?}",
                        dtid, pending.receipt.caller, retired
                    );
                    let mut mms = self.registered_exec_mms.lock().unwrap();
                    let retired_owners: Vec<_> = retired
                        .iter()
                        .filter_map(|tid| {
                            mms.get(tid).map(|mm| NetworkStreamOwner {
                                thread: *tid,
                                mm: *mm,
                            })
                        })
                        .collect();
                    self.abandon_network_owners(retired_owners.into_iter().chain(std::iter::once(
                        NetworkStreamOwner {
                            thread: pending.receipt.caller,
                            mm: pending.receipt.mm,
                        },
                    )));
                    for retired in &retired {
                        mms.remove(retired);
                    }
                    mms.insert(dettid, post_exec_mm);
                    drop(mms);
                    self.post_exec_files
                        .lock()
                        .unwrap()
                        .insert(dettid, pending.receipt);
                    self.exec_preparation_changed.notify_waiters();
                    R::CreateChildThread(Some((post_exec_mm, pending.receipt)))
                } else {
                    match self
                        .recv_create_child_thread(
                            from,
                            request_mm,
                            ChildRegistration {
                                parent_dettid: DetTid::from_raw(from.into()),
                                parent_detpid,
                                child_dettid: dettid,
                                child_tid_addr: ctid,
                                flags,
                                exit_signal,
                                physical_ids,
                                maybe_priority: priority,
                                parent_is_kernel_blocked: false,
                                inherited_birth: None,
                            },
                        )
                        .await
                    {
                        SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                        SchedulerRpcResult::ThreadExited => R::ThreadExited,
                    }
                }
            }
            GlobalRequest::CreateNoSeqChildThread(mut birth, physical_ids, priority) => {
                let native = if birth.fd_permit().is_some() && self.network_runtime.is_some() {
                    if self
                        .sched
                        .lock()
                        .unwrap()
                        .thread_tree
                        .rebind_native_birth(&mut birth)
                        .is_err()
                    {
                        return (None, R::ThreadExited);
                    }
                    match birth.construction() {
                        Ok(Some(outcome)) => Some(outcome),
                        _ => return (None, R::ThreadExited),
                    }
                } else {
                    None
                };
                let flags = native
                    .as_ref()
                    .map_or(birth.flags(), |outcome| outcome.flags());
                let ctid = native
                    .as_ref()
                    .map_or(birth.child_tid_addr(), |outcome| outcome.clear_child_tid());
                let exit_signal = native
                    .as_ref()
                    .map_or(birth.exit_signal(), |outcome| outcome.exit_signal());
                if (self.cfg.sequentialize_threads && native.is_none())
                    || birth.child() != Some(dtid)
                    || request_mm
                        != MmId::for_clone(
                            birth.parent().mm,
                            dtid,
                            flags.contains(CloneFlags::CLONE_VM),
                        )
                {
                    return (None, R::ThreadExited);
                }
                match self
                    .recv_create_child_thread(
                        from,
                        request_mm,
                        ChildRegistration {
                            parent_dettid: birth.parent().thread,
                            parent_detpid: birth.process(),
                            child_dettid: dtid,
                            child_tid_addr: ctid,
                            flags: Some(flags),
                            exit_signal,
                            physical_ids,
                            maybe_priority: priority,
                            parent_is_kernel_blocked: native.as_ref().is_some_and(|outcome| {
                                outcome.flags().contains(CloneFlags::CLONE_VFORK)
                                    || (self.cfg.backend_serializes_fork_children
                                        && !outcome.flags().contains(CloneFlags::CLONE_THREAD))
                            }),
                            inherited_birth: Some(birth),
                        },
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // Requested by the vfork child on behalf of its kernel-blocked parent:
            GlobalRequest::CreateVforkChildThread(
                parent_dettid,
                parent_detpid,
                child_dettid,
                ctid,
                flags,
                exit_signal,
                priority,
            ) => match self
                .recv_create_child_thread(
                    from,
                    request_mm,
                    ChildRegistration {
                        parent_dettid,
                        parent_detpid,
                        child_dettid,
                        child_tid_addr: ctid,
                        flags: Some(flags),
                        exit_signal,
                        physical_ids: None,
                        maybe_priority: priority,
                        parent_is_kernel_blocked: true,
                        inherited_birth: None,
                    },
                )
                .await
            {
                SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                SchedulerRpcResult::ThreadExited => R::ThreadExited,
            },
            // Requested by the child thread itself:
            GlobalRequest::StartNewThread(dettid, detpid, physical_ids, signal_identity) => {
                match self
                    .recv_start_new_thread(
                        from,
                        dettid,
                        detpid,
                        request_mm,
                        physical_ids,
                        signal_identity,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(history) => R::StartNewThread(history),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::DeregisterThread(deregistration) => {
                R::DeregisterThread(self.recv_deregister_thread(from, deregistration).await)
            }
            GlobalRequest::SetChildTidAddress(address) => {
                let updated = self
                    .lock_rpc_scheduler(false)
                    .await
                    .set_child_tid_address(dtid, address);
                if updated {
                    R::SetChildTidAddress(())
                } else {
                    R::ThreadExited
                }
            }
            GlobalRequest::FutexAction(dettid, action, futexid, init_read, mask) => R::FutexAction(
                self.recv_futex_action(
                    RpcIncarnation {
                        dettid,
                        mm: request_mm,
                    },
                    action,
                    futexid,
                    init_read,
                    mask,
                )
                .await,
            ),
            GlobalRequest::RobustListWakes(wakes) => {
                R::RobustListWakes(self.recv_robust_list_wakes(wakes))
            }
            GlobalRequest::DeterminizeInode(ino) => {
                R::DeterminizeInode(self.recv_determinize_inode(from, ino).await)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
            GlobalRequest::DeterminizeDevice(dev) => {
                R::DeterminizeDevice(self.recv_determinize_device(from, dev).await)
            }
            GlobalRequest::DeterminizeMountId(raw_mount_id, fallback_order) => {
                R::DeterminizeMountId(
                    self.recv_determinize_mount_id(from, raw_mount_id, fallback_order.as_deref())
                        .await,
                )
            }
            GlobalRequest::ValidateMountIdOrder(mountinfo_order) => R::ValidateMountIdOrder(
                self.recv_validate_mount_id_order(from, &mountinfo_order)
                    .await,
            ),
            GlobalRequest::UnlinkInode(d_ino) => {
                R::UnlinkInode(self.recv_unlink_inode(from, d_ino).await)
            }
            GlobalRequest::TouchFile(ino) => R::TouchFile(self.recv_touch_file(from, ino).await),
            GlobalRequest::GlobalTimeLowerBound => {
                let ns = self.global_time.lock().unwrap().as_nanos();
                R::GlobalTimeLowerBound(ns)
            }
            GlobalRequest::Network(request) => match request {
                request @ (NetworkRequest::BeginRecordedOriginalFile { .. }
                | NetworkRequest::BeginOriginalFileFromRead { .. }
                | NetworkRequest::BeginEmulatedReadFromRead { .. }
                | NetworkRequest::CompleteEmulatedRead { .. }
                | NetworkRequest::SelectRecordedOriginalFile { .. }
                | NetworkRequest::CompleteRecordedOriginalFile { .. }
                | NetworkRequest::CompleteRecordedReadInterruption { .. }
                | NetworkRequest::NativeBeginOriginalConnect { .. }
                | NetworkRequest::NativeBeginForegroundEpollCtl { .. }
                | NetworkRequest::NativeForegroundEpollCtlReturned { .. }
                | NetworkRequest::NativeBeginOriginalSocket { .. }
                | NetworkRequest::NativeBeginOriginalAllocator { .. }
                | NetworkRequest::NativePublishOriginalSocket { .. }
                | NetworkRequest::NativeObserveOriginalOpenat { .. }
                | NetworkRequest::NativePublishOriginalOpenat { .. }
                | NetworkRequest::NativePublishOriginalEpoll { .. }
                | NetworkRequest::NativeBeginOriginalExternalFromRead { .. }
                | NetworkRequest::NativeSubmitOriginalConnect { .. }
                | NetworkRequest::NativeOriginalConnectOutcome { .. }
                | NetworkRequest::NativeOriginalConnectFailed { .. }
                | NetworkRequest::NativeRetireOriginalConnect { .. }
                | NetworkRequest::NativeRetireInterruptedRead { .. }) => {
                    self.recv_original_connect(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        request,
                    )
                    .await
                }
                request @ (NetworkRequest::NativeBeginStreamCall { .. }
                | NetworkRequest::NativeStreamEffect { .. }
                | NetworkRequest::NativeReleaseStreamCall { .. }) => {
                    self.recv_native_stream_operation(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        request,
                    )
                    .await
                }
                NetworkRequest::PrepareAcceptedEffect { lease, fd, flags } => {
                    self.recv_prepare_accepted_effect(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        lease,
                        fd,
                        flags,
                    )
                    .await
                }
                NetworkRequest::CollectAcceptedEffect { .. } => {
                    unreachable!("effect recovery uses early dispatch")
                }
                NetworkRequest::ResolveAcceptedProvider { lease } => {
                    self.recv_resolve_accepted_provider(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        lease,
                    )
                    .await
                }

                NetworkRequest::EnrollAcceptedListener { call, fd } => {
                    self.recv_enroll_accepted_listener(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        call,
                        fd,
                    )
                    .await
                }

                request @ (NetworkRequest::BeginStreamIngress { .. }
                | NetworkRequest::CompleteStreamIngress { .. }
                | NetworkRequest::StreamQueueStatus { .. }
                | NetworkRequest::ReadStreamChunkView { .. }
                | NetworkRequest::FinishStreamChunk { .. }
                | NetworkRequest::ShadowMode
                | NetworkRequest::AcceptedMode
                | NetworkRequest::RegisterAcceptedFreshSend { .. }
                | NetworkRequest::BeginAcceptedSocket { .. }
                | NetworkRequest::SubmitAcceptedSocket { .. }
                | NetworkRequest::CaptureAcceptedReturn { .. }
                | NetworkRequest::CancelAcceptedSocket { .. }
                | NetworkRequest::CompleteAcceptedSocket { .. }
                | NetworkRequest::AcceptedEndpoint { .. }
                | NetworkRequest::RegisterStreamSocket { .. }
                | NetworkRequest::StreamSocketState { .. }
                | NetworkRequest::StreamCallSocketState { .. }
                | NetworkRequest::BeginSocketControl { .. }
                | NetworkRequest::BeginSocketControls { .. }
                | NetworkRequest::BeginFdRead { .. }
                | NetworkRequest::BeginOrdinaryFdRead { .. }
                | NetworkRequest::FinishFdRead { .. }
                | NetworkRequest::FinishSocketControl { .. }
                | NetworkRequest::BeginStreamCall { .. }
                | NetworkRequest::ConfirmStreamCallPin { .. }
                | NetworkRequest::BeginStreamCallRelease { .. }
                | NetworkRequest::FinishStreamCallRelease { .. }
                | NetworkRequest::StreamCallQueueStatus { .. }
                | NetworkRequest::BeginShadowProbe { .. }
                | NetworkRequest::SubmitStreamPhysical { .. }
                | NetworkRequest::ConfirmStreamPhysical { .. }
                | NetworkRequest::CompleteShadowProbe { .. }
                | NetworkRequest::ReserveStreamCallChunk { .. }
                | NetworkRequest::ZeroStreamReceive { .. }
                | NetworkRequest::BeginRecordDrain { .. }
                | NetworkRequest::FinishRecordDrain { .. }
                | NetworkRequest::BeginZeroStreamWait { .. }
                | NetworkRequest::FinishZeroStreamWait { .. }
                | NetworkRequest::InspectZeroStreamWait { .. }
                | NetworkRequest::CancelZeroStreamWait { .. }
                | NetworkRequest::PreviewSocketOption { .. }
                | NetworkRequest::FdPublication(..)
                | NetworkRequest::FdMutation(..)) => {
                    self.recv_stream_operation(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        request,
                    )
                    .await
                }
                request => R::Network(self.recv_network_request(request)),
            },
            GlobalRequest::PrepareNetworkNativeBirth(_, _)
            | GlobalRequest::CollectNetworkNativeBirth(_, _)
            | GlobalRequest::PrepareNoSeqBirth { .. }
            | GlobalRequest::SubmitNoSeqBirth(_)
            | GlobalRequest::CancelNoSeqBirth(_, _)
            | GlobalRequest::NoSeqBirthOwnerGone(_, _)
            | GlobalRequest::JoinNoSeqBirth(_, _)
            | GlobalRequest::PrepareProcessGroupChange(_, _)
            | GlobalRequest::SubmitProcessGroupChange(_)
            | GlobalRequest::OriginalConnectOwnerGone(_)
            | GlobalRequest::NetworkOwnerGone => {
                unreachable!("consuming path handled before admission")
            }
            GlobalRequest::RegisterNetworkPhysicalTask {
                process,
                thread,
                initial_exec,
            } => {
                let response = self
                    .recv_register_network_physical_task(
                        NetworkStreamOwner {
                            thread: dtid,
                            mm: request_mm,
                        },
                        process,
                        thread,
                        initial_exec,
                    )
                    .await;
                // Terminal birth recovery must not enter the ordinary epilogue,
                // whose admission lock intentionally parks after backend failure.
                if response == R::ThreadExited {
                    return (None, response);
                }
                response
            }
            GlobalRequest::CompleteNetworkInitialTable {
                ticket,
                register_read_succeeded,
            } => {
                let owner = NetworkStreamOwner {
                    thread: dtid,
                    mm: request_mm,
                };
                let result = match &self.network_runtime {
                    Some(runtime) => async {
                        let association = runtime
                            .collect_initial_table(owner, ticket, register_read_succeeded)
                            .await?;
                        if self.network_engine.is_none() {
                            return Ok(None);
                        }
                        let metadata = runtime.observe_initial_metadata(owner).await?;
                        Ok(Some((association.view(), metadata)))
                    }
                    .await
                    .map_err(|e: std::io::Error| e.to_string()),
                    None => Err("initial table collection has no runtime owner".into()),
                };
                R::NetworkInitialTableCollected(result)
            }
            GlobalRequest::AdmitNetworkInitialTable(claim) => {
                let owner = NetworkStreamOwner {
                    thread: dtid,
                    mm: request_mm,
                };
                let result = match (&self.network_runtime, &self.network_engine) {
                    (Some(runtime), Some(engine)) => {
                        // Shared order is scheduler -> engine -> physical custody.
                        // Recheck the exact live MM even on response recovery;
                        // no await or fallible attachment follows the commit.
                        let mut sched = self.sched.lock().unwrap();
                        if self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                            != Some(&owner.mm)
                        {
                            Err("initial projection changed registered exec MM".into())
                        } else {
                            let mut engine = engine.lock().unwrap();
                            runtime
                                .admit_initial_table(
                                    owner,
                                    claim,
                                    |association, claim, pin, previous| {
                                        sched.admit_initial_native_root(
                                            association,
                                            pin,
                                            previous,
                                            |process| {
                                                if engine.fd_table_capability() {
                                                    engine.register_initial_census(
                                                        association,
                                                        claim,
                                                        process,
                                                    )
                                                } else if engine.mode()
                                                    == crate::network_replay::NetworkEngineMode::Replay
                                                {
                                                    engine.admit_initial_replay_census(
                                                        association,
                                                        claim,
                                                        process,
                                                    )
                                                } else {
                                                    engine.admit_initial_record_census(
                                                        association,
                                                        claim,
                                                        process,
                                                    )
                                                }
                                                .map_err(std::io::Error::other)
                                            },
                                        )
                                    },
                                )
                                .map_err(|e| e.to_string())
                        }
                    }
                    _ => Err("initial semantic admission has no retained runtime/engine".into()),
                };
                if result.is_ok() {
                    self.post_exec_files.lock().unwrap().remove(&owner.thread);
                }
                R::NetworkInitialTableAdmitted(result)
            }
            GlobalRequest::TraceSchedEvent(ev, detpid, command_bootstrap) => {
                match self
                    .recv_trace_schedevent(ev, detpid, request_mm, command_bootstrap)
                    .await
                {
                    SchedulerRpcResult::Continue(response) => R::TraceSchedEvent(response),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            // TODO-HUMAN-REVIEW(#869)
            GlobalRequest::RegisterAlarm(dpid, dtid, duration, interval, sig) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                match self
                    .recv_register_alarm(
                        dpid,
                        RpcIncarnation {
                            dettid: dtid,
                            mm: request_mm,
                        },
                        now,
                        duration,
                        interval,
                        sig,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(remaining) => R::RegisterAlarm(remaining),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
            GlobalRequest::AlarmRemaining(dpid) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                let mut sched = self.lock_rpc_scheduler(false).await;
                match sched.itimer_snapshot(dpid, now) {
                    Ok(snapshot) => R::AlarmRemaining(snapshot),
                    Err(error) => {
                        sched.fail_parked(dtid, ProtocolFailure::Timer(error));
                        R::ThreadExited
                    }
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#869)
            GlobalRequest::RegisterPosixTimer(dpid, dtid, timer_id, deadline, interval, sig) => {
                match self
                    .recv_register_posix_timer(
                        dpid,
                        RpcIncarnation {
                            dettid: dtid,
                            mm: request_mm,
                        },
                        timer_id,
                        deadline,
                        interval,
                        sig,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(()) => R::RegisterPosixTimer(()),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            GlobalRequest::ResolveKillTargets(dpid) => R::ResolveKillTargets(
                self.lock_rpc_scheduler(false)
                    .await
                    .process_signal_targets(dpid),
            ),
            GlobalRequest::NotifySignalPending(dettid, SigWrapper(signal), target_process) => {
                let mut scheduler = self.lock_rpc_scheduler(false).await;
                scheduler.notify_signal_pending(dettid, SigWrapper(signal));
                if signal == libc::SIGKILL
                    && let Some(detpid) = target_process
                {
                    scheduler.note_process_sigkill(dettid, detpid);
                }
                R::NotifySignalPending(())
            }
            GlobalRequest::ThreadIsLive(dtid) => {
                R::ThreadIsLive(self.lock_rpc_scheduler(false).await.thread_is_live(dtid))
            }
            GlobalRequest::ExactChildWaitState(parent, child) => R::ExactChildWaitState(
                self.lock_rpc_scheduler(false)
                    .await
                    .exact_child_wait_state(parent, child),
            ),
            GlobalRequest::ReadyChildWait(parent, selector) => {
                let sched = self.lock_rpc_scheduler(false).await;
                R::ReadyChildWait((
                    sched.ready_child_wait(parent, selector),
                    sched.has_child_wait_target(parent, selector),
                ))
            }
            GlobalRequest::ConsumeChildWait(parent, child) => {
                let consumed = self
                    .lock_rpc_scheduler(false)
                    .await
                    .consume_child_wait(parent, child);
                if consumed {
                    self.exec_preparation_changed.notify_waiters();
                }
                R::ConsumeChildWait(consumed)
            }
            GlobalRequest::CompleteProcessGroupChange(_, _) => R::CompleteProcessGroupChange(
                completed_group_change.expect("native group completion retained before accounting"),
            ),
            GlobalRequest::ProcessGroup(process) => R::ProcessGroup(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .process_group(process),
            ),
            GlobalRequest::SetProcessGroup(process, group) => R::SetProcessGroup(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .set_process_group(process, group),
            ),
            GlobalRequest::CreateSession(process) => R::CreateSession(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .create_session(process),
            ),
            GlobalRequest::UnrecoverableShutdown => {
                self.force_shutdown_with_error();
                R::UnrecoverableShutdown(())
            }
            GlobalRequest::RequestPort(open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut mut_used_ports = self.used_ports.lock().unwrap();
                self.update_port_range();
                let total_available =
                    self.port_end_range.load(SeqCst) - self.port_start_range.load(SeqCst);
                let mut index = 0;
                while (*mut_used_ports).contains(&self.next_port.load(SeqCst))
                    && index < total_available
                {
                    self.next_port.fetch_add(1, SeqCst);
                    if self.next_port.load(SeqCst) > self.port_end_range.load(SeqCst) {
                        self.next_port
                            .store(self.port_start_range.load(SeqCst), SeqCst);
                    }
                    index += 1;
                }
                if index == total_available {
                    R::PortFull
                } else {
                    (*mut_used_ports).insert(self.next_port.load(SeqCst));
                    let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                    open_file_to_port.insert(open_file_id, self.next_port.load(SeqCst));
                    R::RequestPort(self.next_port.load(SeqCst))
                }
            }
            GlobalRequest::AddUsedPort(port, open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut used_ports = self.used_ports.lock().unwrap();
                used_ports.insert(port);
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                open_file_to_port.insert(open_file_id, port);
                R::AddUsedPort
            }
            GlobalRequest::ReleasePort(open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut used_ports = self.used_ports.lock().unwrap();
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                let port = open_file_to_port.remove(&open_file_id);
                if let Some(port) = port {
                    used_ports.remove(&port);
                }
                R::ReleasePort(port)
            }
        };

        // Awaited scheduler operations may have raced logical teardown. Never return their
        // operation-specific response after the sender acquired a permanent tombstone.
        let sender_became_terminal =
            if is_deregister || exec_reconnect.is_some() || is_exec_caller_after_local_mm_swap {
                false
            } else {
                let sched = self.lock_rpc_scheduler(consuming_cleanup).await;
                sched.thread_is_logically_killed(dtid)
                    || !sched.rpc_incarnation_matches(dtid, request_mm)
            };
        if resp == R::ThreadExited || sender_became_terminal {
            return (None, R::ThreadExited);
        }

        // The handler locks are released and the existing late-incarnation
        // revalidation above has admitted this response. A Tool-error
        // unwind can leave classic ptrace awaiting parked sibling tasks; an
        // owned controller must terminate before returning to the guest handler.
        // Direct library and plugin configurations retain ordinary typed errors.
        if let R::Network(Err(error)) = &resp
            && let Some(refusal) = error.refusal_for_owned_controller(
                self.cfg.network_trace.policy,
                self.cfg.controller_can_exit_on_network_refusal,
            )
        {
            self.shutdown_for_network_refusal(refusal);
        }

        let time_from_sched = self.global_time.lock().unwrap().threads_time(dtid);
        let time_update = match time_from_sched.cmp(&time_from_guest) {
            Ordering::Equal => None,
            Ordering::Less => {
                panic!(
                    "internal error: thread time should never go down, only monotonically up: time in sched {}, thread local time was {}",
                    time_from_sched, time_from_guest
                )
            }
            Ordering::Greater => Some(time_from_sched),
        };
        (time_update, resp)
    }
}

impl GlobalState {
    /// Actual backend preconstruction callback, including creator-final-before-
    /// construction. Parent liveness is not required; the exact submitted
    /// clone escrow and controller Inbox are the retained operation owners.
    pub(crate) async fn admit_native_child<T>(
        &self,
        creator: Tid,
        child: Tid,
        parent: &mut crate::tool_local::ThreadState<T>,
        pin: std::os::fd::BorrowedFd<'_>,
        terminal: bool,
    ) -> Result<(), Error> {
        let permit = parent
            .pending_fd_clone
            .ok_or_else(|| Error::Tool(anyhow::anyhow!("native child lost clone permit")))?;
        let flags = parent.clone_flags.ok_or_else(|| {
            Error::Tool(anyhow::anyhow!("native child lost construction metadata"))
        })?;
        let owner = NetworkStreamOwner {
            thread: parent.dettid,
            mm: parent.mm_id,
        };
        if creator.as_raw() != owner.thread.as_raw()
            || permit.owner != owner
            || parent.uninvoked_fd_clone.is_some()
            || parent.uninvoked_wait_call.is_some()
            || parent.pending_no_seq_birth.as_ref().is_some_and(|b| {
                b.parent() != owner || b.fd_permit() != Some(permit) || b.flags() != flags
            })
        {
            return Err(Error::Tool(anyhow::anyhow!(
                "backend birth changed retained original invocation"
            )));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| Error::Tool(anyhow::anyhow!("native child lost runtime")))?;
        let proof = runtime
            .observe_native_birth(
                permit,
                DetTid::from_raw(child.as_raw()),
                parent.detpid.ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!("native creator process missing"))
                })?,
                pin,
                terminal,
                flags,
            )
            .await
            .map_err(|e| Error::Tool(anyhow::anyhow!(e)))?;
        // Rebind only the original retained reservation. No numeric parent or
        // serialized RPC field can issue a construction outcome. Lock order is
        // scheduler -> engine; no await occurs after this shared mutation.
        let sched = self.sched.lock().unwrap();
        let birth = parent.pending_no_seq_birth.as_mut().ok_or_else(|| {
            Error::Tool(anyhow::anyhow!("native child lost original birth owner"))
        })?;
        sched
            .thread_tree
            .rebind_native_birth(birth)
            .map_err(|e| Error::Tool(anyhow::anyhow!(e)))?;
        let outcome = birth
            .native_owner
            .as_ref()
            .unwrap()
            .attach(proof.clone())
            .map_err(|e| Error::Tool(anyhow::anyhow!(e)))?;
        self.network_engine
            .as_ref()
            .ok_or_else(|| Error::Tool(anyhow::anyhow!("native child lost engine")))?
            .lock()
            .unwrap()
            .admit_native_birth(&proof)
            .map_err(|e| Error::Tool(anyhow::anyhow!(e)))?;
        // Any failure retains the same immutable outcome and escrow. Identical
        // retries are idempotent; a conflicting second outcome is rejected.
        runtime
            .retain_native_child(&proof, pin)
            .map_err(|e| Error::Tool(anyhow::anyhow!(e)))?;
        parent.native_child_outcome = Some(outcome);
        parent.native_birth_required = true;
        Ok(())
    }

    fn native_birth_cleanup_recovery(
        &self,
    ) -> Option<crate::network_runtime::native_birth::NativeBirthRecovery> {
        let ports = self.used_ports.clone();
        let mappings = self.open_file_to_port.clone();
        Some(
            crate::network_runtime::native_birth::NativeBirthRecovery::new(
                self.sched.clone(),
                self.network_engine.as_ref()?.clone(),
                self.exec_preparation_changed.clone(),
                self.network_stream_changed.clone(),
                move |retired| {
                    let mut ports = ports.lock().unwrap();
                    let mut mappings = mappings.lock().unwrap();
                    for open_file in retired {
                        if let Some(port) = mappings.remove(&open_file) {
                            ports.remove(&port);
                        }
                    }
                },
            ),
        )
    }
    fn retain_failed_native_birth(
        &self,
        birth: &crate::scheduler::NoSeqChildBirth,
        errno: i32,
    ) -> std::io::Result<Option<Arc<crate::network_runtime::native_birth::NativeBirthCleanup>>>
    {
        let Some(runtime) = &self.network_runtime else {
            return Ok(None);
        };
        if birth.fd_permit().is_none() {
            return Ok(None);
        }
        let recovery = self
            .native_birth_cleanup_recovery()
            .ok_or_else(|| std::io::Error::other("native errno cleanup lost network engine"))?;
        runtime.retain_native_birth_cleanup(
            crate::network_runtime::native_birth::NativeBirthCleanupRequest::Failed {
                birth: birth.clone(),
                errno,
            },
            recovery,
        )
    }

    fn settle_failed_no_seq_birth(
        &self,
        sched: &mut Scheduler,
        birth: &crate::scheduler::NoSeqChildBirth,
        errno: i32,
    ) -> bool {
        if !sched.thread_tree.failed_birth_matches(birth, errno) {
            return false;
        }
        if let Some(permit) = birth.fd_permit() {
            let Some(engine) = &self.network_engine else {
                return false;
            };
            let mut engine = engine.lock().unwrap();
            if engine
                .settle_failed_cloned_fd_table(permit, birth.flags(), errno)
                .is_err()
            {
                return false;
            }
            self.release_lifetime_ports(engine.take_lifetime_retired_ports());
        }
        assert!(
            sched.thread_tree.cancel_no_seq_birth(birth),
            "validated known clone failure lost its exact birth reservation"
        );
        if let Some(permit) = birth.fd_permit()
            && let Some(runtime) = &self.network_runtime
            && runtime
                .native_birth_semantics_consumed(permit, None, false)
                .is_err()
        {
            return false;
        }
        true
    }

    pub(crate) fn observe_no_seq_operation<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &crate::tool_local::ThreadState<T>,
        nr: reverie::syscalls::Sysno,
        args: reverie::syscalls::SyscallArgs,
        event: reverie::InjectedSyscallEvent,
    ) {
        if self.cfg.sequentialize_threads && !state.native_birth_required {
            return;
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let outcome = if tid.as_raw() != owner.thread.as_raw() {
            Err("native observer callback mismatched its retained local task")
        } else {
            self.sched
                .lock()
                .unwrap()
                .thread_tree
                .observe_no_seq_operation(
                    owner,
                    process,
                    crate::resources::ExternalOpId::new(owner.thread, state.stats.syscall_count),
                    state.pending_no_seq_birth.as_ref(), (nr,
                    args),
                    event,
                )
        };
        if let Err(phase) = outcome {
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase,
            });
        }
    }

    /// Called only by the backend's distinct actual-final-wait observation.
    /// Consuming ThreadState/on_exit_thread by itself is not physical death.
    pub(crate) fn settle_no_seq_terminal<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &crate::tool_local::ThreadState<T>,
    ) {
        if let Some(runtime) = &self.network_runtime {
            let owner = NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            };
            if tid.as_raw() != owner.thread.as_raw()
                || runtime.native_birth_creator_terminal(owner).is_err()
            {
                self.report_backend_failure(reverie::BackendFailure {
                    pid: Tid::from_raw(process.as_raw()),
                    tid,
                    phase: "actual final wait could not close retained birth preparation",
                });
                return;
            }
        }
        if self.cfg.sequentialize_threads && !state.native_birth_required {
            return;
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let outcome = (|| {
            if tid.as_raw() != owner.thread.as_raw() {
                return Err("terminal callback mismatched its retained local task");
            }
            let mut sched = self.sched.lock().unwrap();
            if state.native_birth_required && self.cfg.sequentialize_threads {
                // Actual final wait retires this original owner, while unknown
                // submitted births remain attached for their late child.
                sched.thread_tree.retire_no_seq_wait_owner(owner, None);
            }
            sched
                .thread_tree
                .settle_terminal_group_operation(owner, state.uninvoked_wait_call.as_ref())?;
            if let Some(birth) = state.pending_no_seq_birth.as_ref()
                && birth.child() == Some(owner.thread)
                && !state.thread_start_entered
            {
                if state.pending_fd_clone != birth.fd_permit()
                    || sched.thread_was_registered(owner.thread)
                    || sched.next_turns.contains_key(&owner.thread)
                    || self
                        .global_time
                        .lock()
                        .unwrap()
                        .contains_thread(owner.thread)
                    || self
                        .registered_exec_mms
                        .lock()
                        .unwrap()
                        .contains_key(&owner.thread)
                    || !sched
                        .thread_tree
                        .check_prestart_terminal_birth(birth, owner)
                {
                    return Err("prestart terminal child lost its exact native birth");
                }
                if let Some(permit) = birth.fd_permit() {
                    let Some(engine) = &self.network_engine else {
                        return Err("prestart terminal child lost its FD engine");
                    };
                    let mut engine = engine.lock().unwrap();
                    let native = state
                        .native_construction()
                        .map_err(|_| "prestart native child lost authenticated rebind")?;
                    let flags = native
                        .as_ref()
                        .map_or(birth.flags(), |outcome| outcome.flags());
                    let process = native.as_ref().map_or_else(
                        || {
                            if flags.contains(CloneFlags::CLONE_THREAD) {
                                birth.process()
                            } else {
                                owner.thread
                            }
                        },
                        |outcome| outcome.process(),
                    );
                    engine
                        .retire_prestart_cloned_fd_table(permit, owner, process, flags)
                        .map_err(|_| "prestart final wait could not settle exact clone custody")?;
                    self.release_lifetime_ports(engine.take_lifetime_retired_ports());
                }
                assert!(
                    sched.complete_prestart_child_exit(birth, owner),
                    "validated prestart terminal birth changed under scheduler lock"
                );
                if let Some(permit) = birth.fd_permit()
                    && let Some(runtime) = &self.network_runtime
                {
                    runtime
                        .native_birth_semantics_consumed(permit, Some((owner.thread, true)), false)
                        .map_err(|_| "prestart semantic retirement lost exact command custody")?;
                }
                return Ok(());
            }
            if let Some(birth) = state.pending_no_seq_birth.as_ref()
                && let Some(errno) = sched.thread_tree.terminal_birth_outcome(
                    owner,
                    birth,
                    state.uninvoked_wait_call.as_ref(),
                )?
            {
                if !sched.thread_tree.failed_birth_matches(birth, errno) {
                    return Err("native failed clone changed exact terminal birth");
                }
                match self
                    .retain_failed_native_birth(birth, errno)
                    .map_err(|_| "native failed clone could not retain cleanup ownership")?
                {
                    Some(_) => {} // Existing driver retains actual errno and common cleanup.
                    None => {
                        if !self.settle_failed_no_seq_birth(&mut sched, birth, errno) {
                            return Err(
                                "native failed clone could not settle its exact retained FD custody",
                            );
                        }
                    }
                }
            }
            Ok(())
        })();
        self.exec_preparation_changed.notify_waiters();
        self.network_stream_changed.notify_waiters();
        if let Err(phase) = outcome {
            // Do not clear unknown gates or pretend a dead creator had no child.
            // Existing failure subscribers enter consuming teardown; pending
            // physical/lifetime obligations remain visible to final cleanup.
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase,
            });
        }
    }

    async fn recv_resources_with_origin(
        &self,
        from: Tid,
        detpid: DetPid,
        rs: Resources,
        request_mm: Option<MmId>,
        rpc: RpcOrigin,
        capability: ControlCapability,
    ) -> (SchedulerRpcResult<ResourceReply>, Option<LogicalTime>) {
        let dettid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME

        let resp2 = {
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
            {
                return (SchedulerRpcResult::ThreadExited, None);
            }
            let Some(nextturn) = sched.next_turns.get(&dettid).cloned() else {
                panic!(
                    "Detcore internal error: no entry for dettid {} in next_turns during resource request.",
                    dettid
                );
            };
            trace!(
                "[detcore, dtid {}] ResourceRequest, filling request into {}",
                &dettid, &nextturn.req
            );
            if let Some(mm) = request_mm
                && let Err(error) = sched.install_resource_origin(
                    dettid,
                    ResourceOrigin {
                        rpc,
                        mm,
                        control: capability,
                    },
                )
            {
                sched.fail_parked(dettid, error);
                return (SchedulerRpcResult::ThreadExited, None);
            }
            sched.request_put(&nextturn.req, rs.clone(), &self.global_time);
            nextturn.resp
        };
        trace!(
            "[detcore, dtid {}] waiting on {} for resources: {:?}",
            dettid, &resp2, rs
        );
        let answer = resp2.get().await; // Block on the scheduler allowing our guest to proceed.
        self.finish_resource_response(from, detpid, rs, request_mm, answer)
            .await
    }

    async fn finish_resource_response(
        &self,
        from: Tid,
        detpid: DetPid,
        rs: Resources,
        request_mm: Option<MmId>,
        answer: SchedResponse,
    ) -> (SchedulerRpcResult<ResourceReply>, Option<LogicalTime>) {
        let dettid = DetTid::from_raw(from.as_raw());
        let request_became_stale = {
            let sched = self.lock_rpc_scheduler(false).await;
            sched.thread_is_logically_killed(dettid)
                || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
        };
        if request_became_stale {
            // `logically_kill_thread` wakes an already-pending request with a
            // signal response.  Treat that wake-up as terminal: otherwise a
            // caller that ignores `ResumeStatus::Signaled` can inject the
            // original syscall after the thread was logically removed.
            // TODO-HUMAN-REVIEW(PR-1023): Review pending SaBRe resource-request cancellation.
            trace!(
                "[detcore, dtid {}] terminating pending request after logical removal",
                dettid
            );
            return (SchedulerRpcResult::ThreadExited, None);
        }
        // A control spends only this response transport, not the resource
        // operation. It must precede exit-group or normal grant side effects.
        if let SchedResponse::ObserveSignal(control) = answer {
            return (
                SchedulerRpcResult::Continue(ResourceReply::ObserveSignal(control)),
                None,
            );
        }
        if let Some((true, process, mm)) = rs.exit_identity() {
            info!(
                "Scheduler authorized an exit-group scenario, from dettid {} / detpid {}",
                dettid, detpid
            );
            // Before allowing an `exit_group` to physically proceed, we
            // deregister the other threads in the thread group to reflect the
            // fact that they will not receive further logical turns.
            //
            // We trust the kernel to physically kill them irrespective of what they're
            // blocked on, including us having blocked them in the `futex_waiters` list.
            {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(dettid)
                    || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
                {
                    return (SchedulerRpcResult::ThreadExited, None);
                }
                for tid in sched.thread_tree.my_thread_group(&dettid) {
                    // We don't need to do anything extra for our own thread. That can use the
                    // same mechanics as a normal exit:
                    if tid != dettid {
                        sched.logically_kill_thread(&tid, &process, mm);
                    }
                }
            }
        }

        match answer {
            SchedResponse::ObserveSignal(_) => {
                self.sched
                    .lock()
                    .unwrap()
                    .fail_parked(dettid, ProtocolFailure::UnexpectedControl);
                (SchedulerRpcResult::ThreadExited, None)
            }
            SchedResponse::GoFdRead(value, read) => {
                // The engine still owns this exact untransferred reader. A lost
                // response is cleaned by owner-gone; it is not a second lookup.
                let owner = crate::network_replay::NetworkStreamOwner {
                    thread: dettid,
                    mm: match request_mm {
                        Some(mm) => mm,
                        None => {
                            self.sched
                                .lock()
                                .unwrap()
                                .fail_parked(dettid, ProtocolFailure::Identity);
                            return (SchedulerRpcResult::ThreadExited, None);
                        }
                    },
                };
                let valid = {
                    let sched = self.sched.lock().unwrap();
                    if sched.backend_failed()
                        || sched.thread_is_logically_killed(dettid)
                        || !sched.rpc_incarnation_matches(dettid, owner.mm)
                    {
                        return (SchedulerRpcResult::ThreadExited, None);
                    }
                    rs.fd_read.is_some_and(|intent| {
                        intent.owner == owner
                            && intent.files == read.publication.permit.files
                            && intent.fd == read.fd
                            && Some(intent.operation) == read.external_grant
                            && sched.original_fd_grant_matches(owner, intent.operation)
                    }) && self.network_engine.as_ref().is_some_and(|engine| {
                        engine
                            .lock()
                            .unwrap()
                            .validate_fd_read_grant(owner, &read)
                            .is_ok()
                    })
                };
                if !valid {
                    self.sched
                        .lock()
                        .unwrap()
                        .fail_parked(dettid, ProtocolFailure::Identity);
                    return (SchedulerRpcResult::ThreadExited, None);
                }
                let time = match value {
                    Some(SchedValue::Value(time)) => Some(LogicalTime::from_nanos(time)),
                    Some(SchedValue::TimeOut) | None => None,
                };
                (
                    SchedulerRpcResult::Continue(ResourceReply::ReadGrant {
                        status: ResumeStatus::Normal,
                        read,
                    }),
                    time,
                )
            }
            // In this context, SchedValue
            SchedResponse::Go(Some(schedval)) => {
                trace!(
                    "[dtid {}] resources granted, resuming normally: {:?}",
                    dettid, rs
                );

                let endtime_update = match schedval {
                    // Only syscalls timeout, and they don't need to update guest timeslice end.
                    SchedValue::TimeOut => None,
                    SchedValue::Value(timeslice) => Some(LogicalTime::from_nanos(timeslice)),
                };
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::Normal)),
                    endtime_update,
                )
            }
            SchedResponse::Go(None) => {
                trace!(
                    "[dtid {}] resources granted but no timeslice specified",
                    dettid,
                );
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::Normal)),
                    None,
                )
            }
            SchedResponse::Signaled(signal) => {
                trace!(
                    "[dtid {}] resources granted but interrupted by signal",
                    dettid,
                );
                (
                    SchedulerRpcResult::Continue(ResourceReply::Grant(ResumeStatus::Signaled(
                        signal,
                    ))),
                    None,
                )
            }
        }
    }

    async fn recv_release_resources(&self, from: Tid, rs: Resources) {
        // TODO(T78627377): add real resource-locking when we enable backgrounding actions.
        trace!("[detcore] Resources released to pid {}: {:?}", from, rs);
    }

    async fn recv_release_all_resources(&self, from: Tid) {
        // TODO(T78627377): add real resource-locking when we enable backgrounding actions.
        trace!("[detcore] All resources held by pid {} released", from);
    }

    /// Global portion of parent-forking-child protocol.  Called by the parent
    /// thread for an ordinary clone, or by the child itself for a vfork whose
    /// parent is blocked inside the kernel (`parent_is_kernel_blocked`).
    async fn recv_create_child_thread(
        &self,
        rpc_sender: Tid,
        request_mm: MmId,
        registration: ChildRegistration,
    ) -> SchedulerRpcResult<()> {
        let ChildRegistration {
            parent_dettid,
            parent_detpid,
            child_dettid,
            child_tid_addr: ctid,
            flags,
            exit_signal,
            physical_ids,
            maybe_priority,
            parent_is_kernel_blocked,
            inherited_birth,
        } = registration;
        let parent_mm = inherited_birth
            .as_ref()
            .map_or(request_mm, |birth| birth.parent().mm);
        let initial_priority = if let Some(pr) = &self.preemptions_to_replay {
            assert!(maybe_priority.is_none());
            let prio = pr
                .thread_initial_priority(&child_dettid)
                .unwrap_or_else(|| {
                    warn!(
                        "Child thread {} not found in preemption history to replay",
                        child_dettid
                    );
                    DEFAULT_PRIORITY
                });
            if !is_ordinary_priority(prio) {
                panic!(
                    "Read a bad initial_prority from file: {}\nFull file: {}",
                    prio,
                    pr.load_all(),
                );
            }
            prio
        } else {
            let prio = maybe_priority.expect(
                "create_child_thread must take an initial priority unless replaying preemptions",
            );
            if !is_ordinary_priority(prio) {
                panic!(
                    "recv_create_child_thread received a bad prority argument : {}",
                    prio,
                );
            }
            prio
        };

        let native_self_registration = inherited_birth
            .as_ref()
            .is_some_and(|birth| birth.native_required);
        let mut native_child_consumed = None;
        {
            let mut sched = self.lock_rpc_scheduler(false).await;
            let sender = DetTid::from_raw(rpc_sender.into());
            if sched.thread_is_logically_killed(sender)
                || !sched.rpc_incarnation_matches(sender, request_mm)
                || sched.thread_is_logically_killed(child_dettid)
            {
                return SchedulerRpcResult::ThreadExited;
            }

            if let Some(birth) = &inherited_birth
                && (sender != child_dettid
                    || (self.cfg.sequentialize_threads && !birth.native_required)
                    || !sched.thread_tree.check_no_seq_birth(birth, child_dettid))
                {
                    return SchedulerRpcResult::ThreadExited;
                }

            let native = inherited_birth
                .as_ref()
                .map(|birth| birth.construction())
                .transpose()
                .expect("authenticated birth rebind must precede registration")
                .flatten();
            if let Some(outcome) = &native {
                if outcome.admission().terminal() {
                    return SchedulerRpcResult::ThreadExited;
                }
                assert_eq!(Some(outcome.flags()), flags);
                assert_eq!(outcome.clear_child_tid(), ctid);
                assert_eq!(outcome.exit_signal(), exit_signal);
                assert_eq!(outcome.child().thread, child_dettid);
            }
            if parent_is_kernel_blocked && self.cfg.sequentialize_threads {
                sched.complete_vfork_registration(parent_dettid, child_dettid);
            }

            // Don't fill in the request, as the child will do it:
            let _entry = sched
                .next_turns
                .entry(child_dettid)
                .or_insert_with(|| ThreadNextTurn {
                    dettid: child_dettid,
                    child_tid_addr: ctid,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                });

            {
                let is_group_leader = if let Some(f) = flags {
                    !f.contains(CloneFlags::CLONE_THREAD)
                } else {
                    true // root thread
                };
                if let Some(birth) = &inherited_birth {
                    assert!(sched.thread_tree.consume_no_seq_birth(birth, child_dettid));
                } else {
                    sched.thread_tree.add_child_with_wait_metadata(
                        parent_dettid,
                        child_dettid,
                        is_group_leader,
                        flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_PARENT)),
                        exit_signal,
                    );
                }
            }

            if let Some((physical_pid, physical_tid)) = physical_ids {
                let child_is_thread =
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_THREAD));
                let child_detpid = if child_is_thread {
                    parent_detpid
                } else {
                    child_dettid
                };
                let child_mm = MmId::for_clone(
                    parent_mm,
                    child_dettid,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_VM)),
                );
                if let Err(open_error) = sched.register_physical_thread(
                    child_dettid,
                    child_mm,
                    physical_pid,
                    physical_tid,
                ) {
                    error!(
                        "[detcore, dtid {}] cannot bind host process {} thread {} during child registration: {}",
                        child_dettid, physical_pid, physical_tid, open_error,
                    );
                    sched.logically_kill_thread(&child_dettid, &child_detpid, child_mm);
                    self.exec_preparation_changed.notify_waiters();
                    return SchedulerRpcResult::ThreadExited;
                }
            }

            let child_mm = if flags.is_none() {
                request_mm
            } else {
                MmId::for_clone(
                    parent_mm,
                    child_dettid,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_VM)),
                )
            };
            let previous_mm = self
                .registered_exec_mms
                .lock()
                .unwrap()
                .insert(child_dettid, child_mm);
            assert!(
                previous_mm.is_none_or(|previous| previous == child_mm),
                "child registration cannot change an admitted address space"
            );

            if let Some(engine) = &self.network_engine {
                let parent_mm = flags.map(|_| {
                    inherited_birth.as_ref().map_or_else(
                        || {
                            *self
                                .registered_exec_mms
                                .lock()
                                .unwrap()
                                .get(&parent_dettid)
                                .expect("authenticated clone parent MM")
                        },
                        |birth| birth.parent().mm,
                    )
                });
                let mut engine = engine.lock().unwrap();
                if engine.fd_table_capability() {
                    let child = NetworkStreamOwner {
                        thread: child_dettid,
                        mm: child_mm,
                    };
                    if let Some(flags) = flags {
                        let parent_mm = parent_mm.expect("clone branch");
                        let process = if flags.contains(CloneFlags::CLONE_THREAD) {
                            parent_detpid
                        } else {
                            child_dettid
                        };
                        if let Some(birth) = &inherited_birth {
                            engine
                                .register_inherited_cloned_fd_table(
                                    birth
                                        .fd_permit()
                                        .expect("active birth retained its submitted permit"),
                                    child,
                                    process,
                                    flags,
                                )
                                .expect("backend child consumes its exact retained clone permit");
                        } else {
                            engine
                                .register_cloned_fd_table(
                                    NetworkStreamOwner {
                                        thread: parent_dettid,
                                        mm: parent_mm,
                                    },
                                    child,
                                    process,
                                    flags,
                                )
                                .expect(
                                    "authenticated child consumes exact pre-clone table receipt",
                                );
                        }
                    }
                    // Capture the retained birth before publishing this child.
                    // No live-task lookup is allowed after registration can wake it.
                    if flags.is_some()
                        && let Some(runtime) = &self.network_runtime
                    {
                        native_child_consumed = Some(runtime.native_child_birth(child));
                    }
                    // Root occupancy is admitted only after the authenticated
                    // stopped-task census, never as a synthesized empty table.
                    self.network_stream_changed.notify_waiters();
                }
            }

            // Record this thread in deterministic creation order so a
            // happens-before anchor addressed by `spawn_ordinal` resolves to it.
            sched.hb_note_spawn(child_dettid);

            if self.cfg.replay_schedule_from.is_none() {
                // Give the thread an initial priority
                let old_prio = sched.priorities.insert(child_dettid, initial_priority);
                assert!(old_prio.is_none());
            } else {
                // In replay mode, the context switch point will already have initialized the priority.
                // UNLESS this is the root thread, in which case we need to fill it in:
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    sched.priorities.entry(child_dettid)
                {
                    assert_eq!(parent_detpid, ROOT_DETPID);
                    entry.insert(initial_priority);
                }
            }

            if let Some(pr) = &mut sched.preemption_writer {
                pr.register_thread(child_dettid, initial_priority);
            }

            // Describe *how* the admission side is chosen, but do not resolve it
            // (and in particular do not draw the post-fork PRNG) here: this
            // handler runs on whichever backend worker fielded the RPC, so on an
            // asynchronous backend (e.g. DBT, where the child self-registers
            // outside a scheduler turn) resolving the side now would consume the
            // PRNG draw in host RPC order. `admit_to_run_queue` resolves the
            // intent at the step2 drain -- which under ptrace is post-commit, in
            // schedule order, unchanged. Read its "What this does and does not
            // make deterministic" section before relying on that word: the drain
            // is a deterministic *point* and resolution within one drain is
            // `DetTid`-ordered, but on an asynchronous backend which drain a
            // given admission lands in is not itself schedule-determined unless
            // that admission is anchored (ordinary clone is, via the parent's
            // `ParentContinue`; vfork is, via `step2a`'s barrier). When threads
            // are not sequentialized, or the
            // parent is already kernel-blocked (vfork), the child takes the tail
            // and no PRNG is consumed.
            let intent = if self.cfg.sequentialize_threads && !parent_is_kernel_blocked {
                AdmitIntent::PostFork(self.cfg.runs_post_fork)
            } else {
                AdmitIntent::Fixed(AdmitSide::Back)
            };
            sched.admit_to_run_queue(child_dettid, intent);
            debug!(
                "[detcore] CreateChildThread with dtid {}: admit child via {:?}.",
                child_dettid, intent,
            );
            sched.started_up.try_put(());
        }
        // Publish complete birth before ParentContinue can await child progress.
        self.exec_preparation_changed.notify_waiters();
        if let Some(proof) = native_child_consumed
            && let Some(runtime) = &self.network_runtime
            && proof
                .and_then(|proof| runtime.native_child_semantics_consumed(&proof))
                .is_err()
            {
                self.report_backend_failure(reverie::BackendFailure {
                    pid: rpc_sender,
                    tid: rpc_sender,
                    phase: "registered child lost exact native command retirement",
                });
                return SchedulerRpcResult::ThreadExited;
            }
        // The child queue position above determines which equal-priority side
        // gets the first turn when the parent requests ParentContinue.
        // A vfork parent is already blocked by the kernel and is not in the run
        // queue, so it must not issue a ParentContinue request here.
        if self.cfg.sequentialize_threads && !parent_is_kernel_blocked && !native_self_registration
        {
            let mut rs = Resources::new(parent_detpid);
            rs.insert(
                ResourceID::ParentContinue {
                    parent: parent_dettid,
                    child: child_dettid,
                },
                Permission::W,
            );
            if matches!(
                self.recv_grant_resources(
                    rpc_sender,
                    parent_detpid,
                    rs,
                    Some(request_mm),
                    RpcOrigin::ParentContinue
                )
                .await
                .0,
                SchedulerRpcResult::ThreadExited
            ) {
                return SchedulerRpcResult::ThreadExited;
            }
        }
        SchedulerRpcResult::Continue(())
    }

    // A backend may deliver the child's startup callback before the parent's
    // CreateChildThread RPC. Missing birth is a wait, not terminal cancellation.
    // This also covers NoSeq without creating a scheduler turn or retrying the
    // RPC's clock accounting. Historical tree membership with no active MM is
    // retired identity, so it must never wait for (or accept) raw TID reuse.
    async fn recv_register_network_physical_task(
        &self,
        owner: NetworkStreamOwner,
        process: i32,
        thread: i32,
        initial_exec: bool,
    ) -> GlobalResponse {
        if thread != owner.thread.as_raw() {
            return GlobalResponse::ThreadExited;
        }
        loop {
            let changed = self.exec_preparation_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let admission = {
                // Unlike lock_rpc_scheduler(false), this lock permits an
                // already pending birth waiter to observe backend failure.
                let mut sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(owner.thread)
                    || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                {
                    return GlobalResponse::ThreadExited;
                }
                let registered_mm = self
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .get(&owner.thread)
                    .copied();
                match (sched.registered_process(owner.thread), registered_mm) {
                    (None, None) => None,
                    (Some(pid), Some(mm)) if pid.as_raw() == process && mm == owner.mm => {
                        let initial = sched.thread_tree.is_root(owner.thread);
                        let pin = if self.network_runtime.is_some() {
                            match sched.register_stopped_ptrace_thread(owner, process, thread) {
                                Ok(pin) => Some(pin),
                                Err(error) => {
                                    return GlobalResponse::RegisterNetworkPhysicalTask(Err(
                                        error.to_string()
                                    ));
                                }
                            }
                        } else {
                            None
                        };
                        Some((initial, pin))
                    }
                    _ => return GlobalResponse::ThreadExited,
                }
            };
            // The lexical guard scope has ended before this asynchronous native
            // request; no scheduler/MM MutexGuard enters the Send RPC future.
            if let Some((initial, pin)) = admission {
                let Some(runtime) = &self.network_runtime else {
                    return GlobalResponse::RegisterNetworkPhysicalTask(Ok(false));
                };
                if initial_exec && !initial {
                    return GlobalResponse::RegisterNetworkPhysicalTask(Err(
                        "initial EXEC census does not name the original root".into(),
                    ));
                }
                let registered = runtime
                    .register_ptrace_task(
                        pin.expect("authenticated runtime admission retained its task"),
                    )
                    .and_then(|()| {
                        if initial {
                            if initial_exec && runtime.has_initial_table_provider() {
                                let receipt = self
                                    .post_exec_files
                                    .lock()
                                    .unwrap()
                                    .get(&owner.thread)
                                    .copied()
                                    .ok_or_else(|| {
                                        std::io::Error::other(
                                            "initial census lacks completed logical exec handoff",
                                        )
                                    })?;
                                runtime.bind_initial_exec(owner, receipt)?;
                            }
                            runtime.register_guard_initial(owner)
                        } else {
                            Ok(())
                        }
                    });
                if let Err(error) = registered {
                    return GlobalResponse::RegisterNetworkPhysicalTask(Err(error.to_string()));
                }
                if initial && initial_exec {
                    return match runtime.prepare_initial_table(owner).await {
                        Ok(Some(ticket)) => GlobalResponse::NetworkInitialTablePrepared(ticket),
                        Ok(None) => GlobalResponse::RegisterNetworkPhysicalTask(Ok(true)),
                        Err(error) => {
                            GlobalResponse::RegisterNetworkPhysicalTask(Err(error.to_string()))
                        }
                    };
                }
                return GlobalResponse::RegisterNetworkPhysicalTask(Ok(true));
            }
            changed.await;
        }
    }

    /// Called by the child thread upon startup.
    /// Returns a thread-preemption history for the new guest thread (if --replay-preemptions-from
    /// is used).
    async fn recv_start_new_thread(
        &self,
        from: Tid,
        dettid: DetTid,
        detpid: DetPid,
        request_mm: MmId,
        physical_ids: Option<(i32, i32)>,
        signal_identity: Option<reverie::SignalTaskIdentity>,
    ) -> SchedulerRpcResult<Option<ThreadHistory>> {
        let mut tries: u64 = 0;
        // TODO: eliminate this loop. Could instead signal with an ivar.
        let response_ivar = loop {
            yield_once().await;
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
            {
                return SchedulerRpcResult::ThreadExited;
            }
            if self.cfg.backend_requires_thread_directed_process_signals && physical_ids.is_none() {
                error!(
                    "[detcore, dtid {}] backend requires a host thread ID at StartNewThread",
                    dettid,
                );
                sched.logically_kill_thread(&dettid, &detpid, request_mm);
                return SchedulerRpcResult::ThreadExited;
            }
            // The resources that must be held for the fresh thread to run:
            let rsrcs = {
                let mut s = HashMap::new();
                s.insert(ResourceID::MemAddrSpace(detpid), Permission::RW); // TODO(T78055411): track mem aliasing.
                Resources {
                    tid: dettid,
                    resources: s,
                    poll_attempt: 0,
                    fyi: String::new(),
                    signal_interrupt_errno: None,
                    fd_read: None,
                }
            };
            let nextturn = match sched.next_turns.entry(dettid) {
                Entry::Vacant(_entry) => {
                    // CreateChildThread on the parent hasn't run yet.

                    // TODO: We could try to populate the entry since we get here
                    // first, but currently we lack the information right here to
                    // populate the child_tid_addr field.
                    if tries == 0 {
                        trace!(
                            "[detcore, dtid {}] thread showed up early, no queue entry yet.  Waiting...",
                            dettid
                        );
                    }
                    tries += 1;
                    continue;
                }
                Entry::Occupied(entry) => {
                    trace!(
                        "[detcore, dtid {}] handling StartNewThread rpc.  Found next_turns entry (after {} tries)",
                        from, tries
                    );
                    entry.get().clone()
                }
            };
            if let Some((physical_pid, physical_tid)) = physical_ids
                && let Err(open_error) =
                    sched.register_physical_thread(dettid, request_mm, physical_pid, physical_tid)
            {
                error!(
                    "[detcore, dtid {}] cannot bind host process {} thread {} for exact signal delivery: {}",
                    dettid, physical_pid, physical_tid, open_error,
                );
                sched.logically_kill_thread(&dettid, &detpid, request_mm);
                return SchedulerRpcResult::ThreadExited;
            }
            if self.cfg.kvm_shared_dequeue_timers {
                let binding = signal_identity
                    .ok_or(TimerFailure::Identity)
                    .and_then(|identity| {
                        if from.as_raw() != dettid.as_raw()
                            || sched.registered_process(dettid) != Some(detpid)
                        {
                            return Err(TimerFailure::Identity);
                        }
                        sched.real_timers.bind(detpid, dettid, request_mm, identity)
                    });
                if let Err(error) = binding {
                    sched.fail_parked(dettid, ProtocolFailure::Timer(error));
                    return SchedulerRpcResult::ThreadExited;
                }
            }
            if let Err(error) = sched.install_resource_origin(
                dettid,
                ResourceOrigin {
                    rpc: RpcOrigin::ThreadStart,
                    mm: request_mm,
                    control: ControlCapability::None,
                },
            ) {
                sched.fail_parked(dettid, error);
                return SchedulerRpcResult::ThreadExited;
            }
            sched.request_put(&nextturn.req, rsrcs, &self.global_time);
            break nextturn.resp;
        };
        debug!(
            "[detcore, dtid {}] New thread will now wait for response on {}...",
            &dettid, &response_ivar
        );
        let answer = response_ivar.get().await;
        if matches!(
            answer,
            SchedResponse::ObserveSignal(_) | SchedResponse::GoFdRead(..)
        ) {
            self.sched
                .lock()
                .unwrap()
                .fail_parked(dettid, ProtocolFailure::UnexpectedControl);
            return SchedulerRpcResult::ThreadExited;
        }
        let request_became_stale = {
            let sched = self.lock_rpc_scheduler(false).await;
            sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
        };
        if request_became_stale {
            return SchedulerRpcResult::ThreadExited;
        }
        info!(
            "[detcore, dtid {}] New thread given go-ahead to proceed via {}",
            &dettid, &response_ivar
        );
        if let Some(pr) = &self.preemptions_to_replay {
            let (history, old_prio) = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(dettid)
                    || !sched.rpc_incarnation_matches(dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                let history = pr.extract_thread_record(&dettid).unwrap_or_else(|| {
                    warn!(
                        "Replaying preemptions, but no record found for thread {}",
                        dettid
                    );
                    ThreadHistory::new()
                });
                let old_prio = sched.priorities.insert(dettid, history.initial_priority());
                (history, old_prio)
            };
            debug!(
                "[replay-preemption] Enqueing new thread at priority {:?} (changed from {:?})",
                history.initial_priority(),
                old_prio,
            );
            SchedulerRpcResult::Continue(Some(history))
        } else {
            SchedulerRpcResult::Continue(None)
        }
    }

    /// Warning: this happens completely asynchronously, whenever the guest exit hook fires.
    /// Its timing is not coordinated by the scheduler.
    async fn recv_deregister_thread(&self, _from: Tid, deregistration: ThreadDeregistration) {
        let ThreadDeregistration {
            dettid,
            detpid,
            mm,
            thread_start_entered: _,
            timeslice_stats,
            syscall_count,
            chaos_epochs,
        } = deregistration;
        // A fatal signal can tear down the caller after its local state has advanced to the
        // candidate exec image but before the successful reconnect (or failed-exec cancel).
        // Retire that one-shot preparation before scheduler-incarnation admission rejects the
        // candidate image's final cleanup RPC.
        let mut pending = self.pending_exec_states.lock().unwrap();
        let abandoned_exec = pending.get(&detpid).is_some_and(|state| {
            state.receipt.caller == dettid
                && (mm == state.receipt.mm
                    || mm == state.receipt.mm.for_exec(state.receipt.process))
        });
        if abandoned_exec {
            pending.remove(&detpid);
        }
        drop(pending);
        self.exec_preparation_changed.notify_waiters();

        // NoSeq retains the same incarnation checks and final accounting;
        // only its scheduler queue retirement differs below.
        let mut sched = self.sched.lock().unwrap();
        let registered_mm = self
            .registered_exec_mms
            .lock()
            .unwrap()
            .get(&dettid)
            .copied();
        if !sched.rpc_incarnation_matches(dettid, mm)
            || (!abandoned_exec && registered_mm.is_some_and(|current| current != mm))
        {
            debug!(
                "[detcore, dtid {}] ignoring deregistration from retired exec incarnation {:?}",
                dettid, mm,
            );
            return;
        }
        self.post_exec_fd_blocking.lock().unwrap().remove(&dettid);
        if !sched.note_deregistration_accounted(dettid) {
            trace!(
                "[detcore, dtid {}] acknowledging already-accounted deregistration",
                dettid
            );
            return;
        }
        if let Some(writer) = &mut sched.preemption_writer {
            for transition in chaos_epochs {
                writer.insert_chaos_epoch(dettid, transition);
            }
        }
        sched.record_timeslice_stats(dettid, timeslice_stats);
        sched.record_syscall_count(dettid, syscall_count);
        if !self.cfg.sequentialize_threads {
            sched.consume_no_seq_thread(dettid, detpid, mm);
        } else if !sched.thread_is_logically_killed(dettid) {
            sched.logically_kill_thread(&dettid, &detpid, mm);
        }
        self.registered_exec_mms.lock().unwrap().remove(&dettid);
        self.post_exec_files.lock().unwrap().remove(&dettid);
        drop(sched);
        self.exec_preparation_changed.notify_waiters();
        trace!(
            "[detcore, dtid {}] thread deregistered, removed from sched structures.",
            dettid
        );
    }

    async fn recv_futex_action(
        &self,
        caller: RpcIncarnation,
        action: FutexAction,
        futexid: FutexID,
        init_read: i32,
        mask: u32,
    ) -> Option<SchedValue> {
        let RpcIncarnation { dettid, mm } = caller;
        trace!("[detcore, dtid {}] Futex action: {:?}", &dettid, action);
        let response_iv = {
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, mm)
            {
                return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
            }
            let Some(resp_iv) = sched
                .next_turns
                .get(&dettid)
                .map(|nextturn| nextturn.resp.clone())
            else {
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-845): Review late RPCs from exit-group siblings.
                trace!(
                    "[detcore, dtid {}] ignoring futex action after logical thread removal",
                    dettid
                );
                return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
            };
            match action {
                FutexAction::WaitRequest(maybe_timeout) => {
                    if sched.child_tid_was_cleared(futexid, init_read) {
                        trace!(
                            "[detcore, dtid {}] late wait on cleared child-TID futex {:?}",
                            dettid, futexid
                        );
                        return Some(SchedValue::Value(0));
                    }
                    if let Err(error) = sched.install_resource_origin(
                        dettid,
                        ResourceOrigin {
                            rpc: RpcOrigin::FutexAction,
                            mm,
                            control: ControlCapability::None,
                        },
                    ) {
                        sched.fail_parked(dettid, error);
                        return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
                    }
                    sched.sleep_futex_waiter(&dettid, futexid, maybe_timeout, mask);
                    // block on ivar, below
                }
                FutexAction::WaitFinished => {
                    return None;
                }
                FutexAction::WakeRequest(num_threads) => {
                    let num = sched.wake_futex_waiters(dettid, futexid, num_threads, mask);
                    return Some(SchedValue::Value(num));
                }
                FutexAction::WakeFinished(_num_threads) => {
                    return None;
                }
            }
            // Blocking on the FUTEX_WAIT here, remove ourselves:
            assert!(sched.run_queue.remove_tid(dettid));
            resp_iv
        };
        // Wait for wake+scheduler response.
        match response_iv.get().await {
            SchedResponse::Go(answer) => {
                trace!(
                    "[detcore, dtid {}] Unblocked from futex_wait! ({})",
                    &dettid, &response_iv
                );
                answer
            }
            SchedResponse::Signaled(_) => Some(SchedValue::Value(nix::errno::Errno::EINTR as u64)),
            SchedResponse::ObserveSignal(_) | SchedResponse::GoFdRead(..) => {
                self.sched
                    .lock()
                    .unwrap()
                    .fail_parked(dettid, ProtocolFailure::UnexpectedControl);
                self.wait_for_backend_failure().await;
                futures::future::pending().await
            }
        }
    }

    fn recv_robust_list_wakes(&self, wakes: Vec<(DetTid, FutexID)>) -> Vec<u64> {
        let mut sched = self.sched.lock().unwrap();
        sched.wake_futex_waiters_after_exit(&wakes)
    }

    async fn recv_determinize_inode(&self, from: Tid, ino: RawInode) -> (DetInode, LogicalTime) {
        let _sched = self.lock_rpc_scheduler(false).await;
        // Here we establish a policy that when we first see a file its mtime is epoch.
        let nanos = self
            .cfg
            .epoch
            .timestamp_nanos_opt()
            .expect("epoch cannot be represented in a timestamp with nanosecond precision")
            as u64;
        let (dino, ns) = self
            .inodes
            .lock()
            .unwrap()
            .add_inode(ino, LogicalTime::from_nanos(nanos));
        trace!(
            "[detcore, dtid {}] resolved (raw) inode {:?} to {:?}, mtime {}",
            from, ino, dino, ns
        );
        (dino, ns)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    async fn recv_determinize_device(&self, from: Tid, raw_device: u64) -> u64 {
        let _sched = self.lock_rpc_scheduler(false).await;
        let det_device = self.devices.lock().unwrap().determinize(raw_device);
        trace!(
            "[detcore, dtid {}] resolved (raw) device {} to {}",
            from, raw_device, det_device
        );
        det_device
    }

    async fn recv_determinize_mount_id(
        &self,
        from: Tid,
        raw_mount_id: u64,
        mountinfo_order: Option<&[u64]>,
    ) -> Option<u64> {
        let _sched = self.lock_rpc_scheduler(false).await;
        let virtual_mount_id = self
            .mount_ids
            .lock()
            .unwrap()
            .determinize(raw_mount_id, mountinfo_order);
        trace!(
            "[detcore, dtid {}] resolved fdinfo mount ID {} to {:?}",
            from, raw_mount_id, virtual_mount_id
        );
        virtual_mount_id
    }

    async fn recv_validate_mount_id_order(&self, from: Tid, mountinfo_order: &[u64]) -> bool {
        let _sched = self.lock_rpc_scheduler(false).await;
        let valid = self
            .mount_ids
            .lock()
            .unwrap()
            .validate_mountinfo_order(mountinfo_order);
        trace!(
            "[detcore, dtid {}] validated mountinfo identity order: {}",
            from, valid
        );
        valid
    }

    async fn recv_unlink_inode(&self, from: Tid, d_ino: DetInode) {
        let _sched = self.lock_rpc_scheduler(false).await;
        trace!("[detcore, dtid {}] unlink (det) inode {:?}", from, d_ino);
        self.inodes.lock().unwrap().remove_inode(d_ino);
    }

    async fn recv_touch_file(&self, from: Tid, ino: RawInode) {
        let _sched = self.lock_rpc_scheduler(false).await;
        let mtime = if self.cfg.virtualize_time {
            self.global_time.lock().unwrap().as_nanos()
        } else {
            // In this scenario, virtualize_metadata is set and virtualize_time isn't.
            // We virtualize initial mtimes, but update using realtime.
            let dt: DateTime<Utc> = Utc::now();
            let nanos = dt.timestamp_nanos_opt().expect(
                "current time cannot be represented in a timestamp with nanosecond precision",
            ) as u64;
            LogicalTime::from_nanos(nanos)
        };
        trace!(
            "[dtid {}] bumping mtime on file (rawinode {:?}) to {}",
            from, ino, mtime,
        );
        let mut mg = self.inodes.lock().unwrap();
        let dino =
            if let Some(d) = mg.inodes.get(&ino) {
                *d
            } else {
                // Otherwise we haven't seen this inode yet (e.g. because there hasnt been a
                // stat on it), so we just-in-time add it.
                let nanos =
                    self.cfg.epoch.timestamp_nanos_opt().expect(
                        "epoch cannot be represented in a timestamp with nanosecond precision",
                    ) as u64;
                let (d, _) = mg.add_inode(ino, LogicalTime::from_nanos(nanos));
                d
            };
        let info = mg
            .detinodes_info
            .get_mut(&dino)
            // TODO(T87258449): remove this `expect`:
            .expect("Invariant violation: det inode missing from map.");
        info.mtime = mtime;
    }

    async fn recv_trace_schedevent(
        &self,
        ev: SchedEvent,
        detpid: DetPid,
        request_mm: MmId,
        command_bootstrap: bool,
    ) -> SchedulerRpcResult<TraceSchedEventResponse> {
        let ev = {
            let sched = self.lock_rpc_scheduler(false).await;
            if !sched.rpc_incarnation_matches(ev.dettid, request_mm) {
                return SchedulerRpcResult::ThreadExited;
            }
            // TODO(T124316762): debug address randomization in the tracer and get rid of this hack:
            let ev = {
                if self.past_first_execve.load(SeqCst) {
                    ev
                } else {
                    info!(
                        "Warning: erasing rip of pre-execve sched event! {:?}",
                        SchedEventForLog {
                            event: &ev,
                            command_bootstrap
                        }
                    );
                    SchedEvent {
                        end_rip: None,
                        start_rip: None,
                        ..ev
                    }
                }
            };
            // Future trace_schedevent calls will retain their rip values.
            if ev.op == Op::Syscall(Sysno::execve, SyscallPhase::Prehook) {
                self.past_first_execve.store(true, SeqCst);
            }
            ev
        };

        // Yield this guest thread if needed to follow schedule.
        let result = if self.cfg.replay_schedule_from.is_some() {
            let (consumed, print_stack2) = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(ev.dettid)
                    || !sched.rpc_incarnation_matches(ev.dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                let consumed = sched.consume_schedevent(&ev);
                let print_stack2 = if self.cfg.record_preemptions {
                    sched.record_event(&ev)
                } else {
                    None
                };
                (consumed, print_stack2)
            };
            let ConsumeResult {
                keep_running,
                print_stack,
                event_ix: _,
                timeslice_remaining: mut end_of_timeslice,
            } = consumed;
            trace!(
                "keep_running :{}, end_of_timeslice: {:?}",
                keep_running, end_of_timeslice
            );

            if !keep_running {
                trace!(
                    "[detcore, dtid {}] Thread yielding to follow replay schedule",
                    &ev.dettid,
                );
                let tid = reverie::Tid::from(ev.dettid.as_raw()); // TODO(T78538674): virtualize pid/tid:
                let mut rsrcs = Resources::new(ev.dettid);
                rsrcs.insert(ResourceID::TraceReplay, Permission::RW);
                let (response, timeslice) = self
                    .recv_grant_resources(
                        tid,
                        detpid,
                        rsrcs,
                        Some(request_mm),
                        RpcOrigin::TraceSchedEvent,
                    )
                    .await;
                if response == SchedulerRpcResult::ThreadExited {
                    return SchedulerRpcResult::ThreadExited;
                }
                end_of_timeslice = timeslice;
                trace!(
                    "[detcore, dtid {}] Thread reactivated after yielding for replay schedule",
                    &ev.dettid,
                );
            }

            TraceSchedEventResponse {
                print_stack_strace: print_stack.or(print_stack2),
                timeslice: end_of_timeslice,
            }
        } else {
            let print_stack_strace = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(ev.dettid)
                    || !sched.rpc_incarnation_matches(ev.dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                if self.cfg.record_preemptions {
                    sched.record_event(&ev)
                } else {
                    None
                }
            };
            TraceSchedEventResponse {
                print_stack_strace,
                timeslice: None,
            }
        };

        if result.print_stack_strace.is_some()
            && let Some(sig) = &self.cfg.stacktrace_signal
        {
            let _sched = self.lock_rpc_scheduler(false).await;
            trace!(
                "[dtid {}] signaling thread with {} at the point of stack trace printing.",
                ev.dettid, sig.0
            );
            let tid = Pid::from_raw(ev.dettid.as_raw());
            // TODO(T78538674): virtualize pid/tid:
            // We send a raw signal here and let the guest pick it up WHENEVER it resumes.
            // We don't use the "signal_guest" method because we don't necessarily respect that
            // protocol here.
            // Alarm/timer signals are guest-chosen and may be realtime, which
            // `nix` cannot name; fall through to the raw syscall for those.
            match sig.signal() {
                Some(named) => signal::kill(tid, named).unwrap(),
                None => {
                    // SAFETY: `tid` is a live thread this scheduler owns and
                    // `sig.raw()` is a signal number the guest already supplied.
                    let rc = unsafe { libc::kill(tid.as_raw(), sig.raw()) };
                    assert_eq!(rc, 0, "raw kill of signal {} failed", sig.raw());
                }
            }
        }

        SchedulerRpcResult::Continue(result)
    }

    // Ephemeral port range is in file /proc/sys/net/ipv4/ip_local_port_range"
    // This function reads from the file and returns the range
    // Start of range is at index 0, end of range is at index 1.
    fn read_port_range() -> Vec<u16> {
        let contents = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .expect("File should be present");
        let range: Vec<u16> = contents
            .split_whitespace()
            .filter_map(|number| number.parse().ok())
            .collect();
        range
    }

    // Reflect ephemeral port range updated outside of the tracer program internally.
    fn update_port_range(&self) {
        let range = Self::read_port_range();
        self.port_start_range.store(range[0], SeqCst);
        self.port_end_range.store(range[1], SeqCst);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Register an alarm (delayed signal delivery) with the global scheduler.
    async fn recv_register_alarm(
        &self,
        detpid: DetPid,
        caller: RpcIncarnation,
        now: LogicalTime,
        duration: LogicalTime,
        interval: LogicalTime,
        sig: SigWrapper,
    ) -> SchedulerRpcResult<(LogicalTime, LogicalTime)> {
        let RpcIncarnation { dettid, mm } = caller;
        let mut sched = self.lock_rpc_scheduler(false).await;
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        match sched.replace_real_timer(detpid, dettid, now, duration, interval, alarm_signal(sig)) {
            Ok(old) => SchedulerRpcResult::Continue(old),
            Err(error) => {
                sched.fail_parked(dettid, ProtocolFailure::Timer(error));
                SchedulerRpcResult::ThreadExited
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Register, re-arm, or disarm a POSIX timer in the global scheduler.
    async fn recv_register_posix_timer(
        &self,
        detpid: DetPid,
        caller: RpcIncarnation,
        timer_id: i32,
        deadline: Option<LogicalTime>,
        interval: LogicalTime,
        sig: SigWrapper,
    ) -> SchedulerRpcResult<()> {
        let RpcIncarnation { dettid, mm } = caller;
        let mut sched = self.lock_rpc_scheduler(false).await;
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        sched.register_posix_timer(
            detpid,
            dettid,
            timer_id,
            deadline,
            interval,
            alarm_signal(sig),
        );
        SchedulerRpcResult::Continue(())
    }

    /// The shared engine owns admission and semantic publication. Blocking
    /// workers own the original handle and raw completion independently of this
    /// callback future, with no scheduler/engine locks spanning physical work.
    async fn recv_native_stream_operation(
        &self,
        owner: NetworkStreamOwner,
        request: NetworkRequest,
    ) -> GlobalResponse {
        let Some(runtime) = &self.network_runtime else {
            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                "native stream lacks authenticated runtime",
            )));
        };
        let (call, task, identity) = loop {
            let changed = self.network_stream_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (prepared, backend_failed, native_task, native_identity) = {
                let sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(owner.thread)
                    || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                    || self
                        .registered_exec_mms
                        .lock()
                        .unwrap()
                        .get(&owner.thread)
                        .is_some_and(|registered| *registered != owner.mm)
                {
                    return GlobalResponse::ThreadExited;
                }
                let Some(engine) = &self.network_engine else {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        "native stream without shared engine",
                    )));
                };
                // The scheduler guard precedes metadata, which precedes the
                // engine. Lookup alone grants nothing; revalidate the exact
                // Arc, binding and reader before transferring its permit.
                let metadata = if let NetworkRequest::NativeBeginStreamCall { read } = &request {
                    match engine
                        .lock()
                        .unwrap()
                        .fd_metadata(owner, read.publication.permit.files)
                    {
                        Ok(actual) => Some(actual),
                        Err(error) => {
                            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                                error.to_string(),
                            )));
                        }
                    }
                } else {
                    None
                };
                let metadata_guard = metadata.as_ref().map(|actual| actual.lock().unwrap());
                let mut engine = engine.lock().unwrap();
                let mut native_task = None;
                let mut native_identity = None;
                let prepared = match &request {
                    NetworkRequest::NativeBeginStreamCall { read } => {
                        // Clone the registered task authority before admitting
                        // work that can outlive its task/RPC registry entry.
                        native_task = Some(match runtime.prepare_native_capture_task(owner) {
                            Ok(task) => task,
                            Err(error) => {
                                return GlobalResponse::Network(Err(NetworkRpcError::internal(
                                    error.to_string(),
                                )));
                            }
                        });
                        native_identity = match engine.native_stream_capture_identity(
                            owner,
                            read,
                            metadata.as_ref().expect("capture metadata"),
                            metadata_guard.as_deref().expect("capture metadata guard"),
                        ) {
                            Ok(identity) => identity,
                            Err(error) => {
                                return GlobalResponse::Network(Err(NetworkRpcError::internal(
                                    error.to_string(),
                                )));
                            }
                        };
                        engine
                            .begin_native_stream_call_from_read(owner, read.clone())
                            .map(Some)
                    }
                    NetworkRequest::NativeStreamEffect { lease, effect } => engine
                        .submit_retained_stream_physical(owner, *lease, effect.clone())
                        .map(|()| None),
                    NetworkRequest::NativeReleaseStreamCall { call } => engine
                        .begin_stream_call_release(owner, *call)
                        .map(|()| None),
                    _ => unreachable!("native dispatch family"),
                };
                (
                    prepared,
                    sched.backend_failure_waiter(),
                    native_task,
                    native_identity,
                )
            };
            match prepared {
                Ok(call) => break (call, native_task, native_identity),
                Err(NetworkReplayError::StreamOperationBusy(_))
                    if matches!(request, NetworkRequest::NativeBeginStreamCall { .. })
                        && !self.cfg.sequentialize_threads =>
                {
                    // The callback may overlap an independently progressing
                    // table mutation in NoSeq. Recheck task/MM, exact slot and
                    // permit after every wake; no lock spans this await.
                    tokio::select! {
                        _ = changed => {},
                        _ = backend_failed => return GlobalResponse::ThreadExited,
                    }
                }
                Err(error) => {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        error.to_string(),
                    )));
                }
            }
        };
        if let Some(call) = call.filter(|call| !call.physical_pin_required) {
            return GlobalResponse::Network(Ok(NetworkReply::StreamCall(call)));
        }
        enum Completion {
            Pin(NetworkStreamPinOutcome),
            Effect(crate::network_runtime::native_peer::Observation),
            Released,
        }
        let observed = match &request {
            NetworkRequest::NativeBeginStreamCall { read } => runtime
                .capture_native_stream(
                    owner,
                    call.expect("admitted call").id,
                    read.binding.expect("validated capture binding").slot.fd,
                    task.expect("task authority retained before capture admission"),
                    identity.expect("physical capture identity retained with reader"),
                    self.native_capture_recovery()
                        .expect("admitted shared engine"),
                )
                .await
                .map(Completion::Pin),
            NetworkRequest::NativeStreamEffect { lease, effect } => runtime
                .execute_native_stream(owner, *lease, effect.clone())
                .await
                .map(Completion::Effect),
            NetworkRequest::NativeReleaseStreamCall { call } => runtime
                .release_native_stream(owner, *call)
                .await
                .map(|()| Completion::Released),
            _ => unreachable!("native dispatch family"),
        };
        let observed = match observed {
            Ok(observed) => observed,
            Err(error) => {
                return GlobalResponse::Network(Err(NetworkRpcError::internal(error.to_string())));
            }
        };
        // A known late physical result is retained, even when publication has
        // since been revoked by task exit, exec, or backend failure.
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed()
            || sched.thread_is_logically_killed(owner.thread)
            || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
            || self
                .registered_exec_mms
                .lock()
                .unwrap()
                .get(&owner.thread)
                .is_some_and(|registered| *registered != owner.mm)
        {
            return GlobalResponse::ThreadExited;
        }
        let mut engine = self
            .network_engine
            .as_ref()
            .expect("admitted engine")
            .lock()
            .unwrap();
        let result = (|| -> Result<NetworkReply, NetworkRpcError> {
            let protocol = |error: NetworkReplayError| NetworkRpcError::internal(error.to_string());
            let physical = |error: std::io::Error| NetworkRpcError::internal(error.to_string());
            match (&request, observed) {
                (NetworkRequest::NativeBeginStreamCall { read }, Completion::Pin(outcome)) => {
                    let call = call.expect("admitted call");
                    engine
                        .confirm_stream_call_pin(owner, call.id, outcome)
                        .map_err(protocol)?;
                    if let NetworkStreamPinOutcome::Failed(errno) = outcome {
                        engine
                            .finish_socket_control(
                                owner,
                                read.control.expect("validated capture control"),
                                NetworkSocketControlFinish::Unchanged,
                            )
                            .map_err(protocol)?;
                        runtime
                            .finish_native_capture_failure(owner, call.id, errno)
                            .map_err(physical)?;
                        Ok(NetworkReply::NativeStreamPinFailed(errno))
                    } else {
                        Ok(NetworkReply::StreamCall(call))
                    }
                }
                (
                    NetworkRequest::NativeStreamEffect { lease, effect },
                    Completion::Effect(observed),
                ) => {
                    // A serde value cannot authenticate controller-local copy
                    // custody. Validate the exact retained Pending before any
                    // engine counter/lease mutation, then keep the independent
                    // engine semantic-join guard below.
                    runtime
                        .preflight_native_stream(owner, *lease, effect, &observed)
                        .map_err(physical)?;
                    engine
                        .confirm_retained_stream_physical(owner, *lease, &observed)
                        .map_err(protocol)?;
                    runtime
                        .confirm_native_stream(owner, *lease, effect, &observed)
                        .map_err(physical)?;
                    Ok(NetworkReply::NativeStreamObservation(observed))
                }
                (NetworkRequest::NativeReleaseStreamCall { call }, Completion::Released) => {
                    let completed = engine
                        .complete_stream_call_release(owner, *call)
                        .map_err(protocol)?;
                    let (released_owner, released_call) = completed.identity();
                    let cleanup =
                        runtime.finish_native_stream_release(released_owner, released_call);
                    match (completed.into_result(), cleanup) {
                        (Ok(()), Ok(())) => Ok(NetworkReply::Unit),
                        (Err(primary), Ok(())) => Err(protocol(primary)),
                        (Ok(()), Err(cleanup)) => Err(physical(cleanup)),
                        (Err(primary), Err(cleanup)) => Err(NetworkRpcError::internal(format!(
                            "{primary}; native release acknowledgement: {cleanup}"
                        ))),
                    }
                }
                _ => unreachable!("completion follows submitted operation"),
            }
        })();
        self.release_lifetime_ports(engine.take_lifetime_retired_ports());
        self.network_stream_changed.notify_waiters();
        GlobalResponse::Network(result)
    }

    /// Associate the real backend-owned metadata after successful local startup
    /// or exec. This never registers a task/table or issues FD capability.
    pub(crate) fn observe_ready_fd_metadata<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
    ) -> Result<(), reverie::Error> {
        let Some(engine) = &self.network_engine else {
            return Ok(());
        };
        let fail = |message: &str| reverie::Error::Tool(anyhow::anyhow!(message.to_owned()));
        // Disabled capability remains inert even before local metadata access.
        // This brief guard is dropped before taking scheduler or metadata.
        if !engine.lock().unwrap().fd_table_capability() {
            return Ok(());
        }
        // Take an immutable private census join before scheduler/metadata.
        // Physical admission itself takes physical -> scheduler elsewhere.
        let initial = self
            .network_runtime
            .as_ref()
            .map(|runtime| {
                runtime.initial_metadata_identity(NetworkStreamOwner {
                    thread: state.dettid,
                    mm: state.mm_id,
                })
            })
            .transpose()
            .map_err(|error| fail(&error.to_string()))?
            .flatten();
        // Established joint order: scheduler -> metadata -> engine. No lock
        // survives an await and the callback never requests a scheduler turn.
        let sched = self.sched.lock().unwrap();
        let current = tid.as_raw() == state.dettid.as_raw()
            && !sched.backend_failed()
            && !sched.thread_is_logically_killed(state.dettid)
            && sched.rpc_incarnation_matches(state.dettid, state.mm_id)
            && self.registered_exec_mms.lock().unwrap().get(&state.dettid) == Some(&state.mm_id);
        let mut metadata = state
            .file_metadata
            .lock()
            .map_err(|_| fail("ready metadata mutex poisoned"))?;
        let mut engine = engine.lock().unwrap();
        if !engine.fd_table_capability() {
            return Ok(());
        }
        if !current || !metadata.network_lifetime_tracking() {
            return Err(fail("ready metadata lost current enrolled task/MM"));
        }
        engine
            .associate_authenticated_fd_metadata(
                NetworkStreamOwner {
                    thread: state.dettid,
                    mm: state.mm_id,
                },
                &state.file_metadata,
                &mut metadata,
                initial.as_ref(),
            )
            .map_err(|error| fail(&error.to_string()))?;
        drop(engine);
        drop(metadata);
        drop(sched);
        if let Some(runtime) = &self.network_runtime {
            runtime
                .bind_foreground_metadata(
                    NetworkStreamOwner {
                        thread: state.dettid,
                        mm: state.mm_id,
                    },
                    &state.file_metadata,
                    &state.memory_metadata,
                )
                .map_err(|error| fail(&error.to_string()))?;
        }
        Ok(())
    }

    /// Complete or acquire one runtime stream receipt. In sequential mode the
    /// adapter enters this only within its granted foreground turn; it never
    /// holds an ingress receipt across a blocking physical wait or a scheduler
    /// resource request. Waiting on Notify here would otherwise deadlock the
    /// owner of that turn. Nonsequential owners make independent progress.
    async fn recv_stream_operation(
        &self,
        owner: NetworkStreamOwner,
        request: NetworkRequest,
    ) -> GlobalResponse {
        let acquiring = matches!(
            request,
            NetworkRequest::BeginStreamIngress { .. }
                | NetworkRequest::BeginSocketControl { .. }
                | NetworkRequest::BeginSocketControls { .. }
                | NetworkRequest::BeginFdRead { .. }
                | NetworkRequest::BeginOrdinaryFdRead { .. }
                | NetworkRequest::BeginShadowProbe { .. }
                | NetworkRequest::ReserveStreamCallChunk { .. }
                | NetworkRequest::ZeroStreamReceive { .. }
                | NetworkRequest::FdPublication(
                    crate::network_replay::NetworkFdPublicationRequest::Acquire { .. }
                )
                | NetworkRequest::FdMutation(
                    crate::network_replay::NetworkFdMutationRequest::Begin { .. }
                )
        );
        loop {
            let changed = self.network_stream_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (result, backend_failed, prior_selection_can_progress) = {
                // Same admission order as the scheduler: scheduler, engine.
                // No lock survives an await, and a notification never admits
                // a stale owner without another complete condition check.
                let sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(owner.thread)
                    || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                    || self
                        .registered_exec_mms
                        .lock()
                        .unwrap()
                        .get(&owner.thread)
                        .is_some_and(|registered| *registered != owner.mm)
                {
                    return GlobalResponse::ThreadExited;
                }
                // Receipt bytes alone are not ownership authority. Recheck the
                // existing registered process/MM preparation on each admission
                // attempt, including after a NoSeq waiter was notified.
                if let NetworkRequest::FdMutation(
                    crate::network_replay::NetworkFdMutationRequest::Begin {
                        kind: crate::network_replay::NetworkFdMutationKind::Exec { receipt },
                        ..
                    },
                ) = &request
                    && (sched.registered_process(owner.thread) != Some(receipt.process)
                        || receipt.caller != owner.thread
                        || receipt.mm != owner.mm
                        || !self
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .get(&receipt.process)
                            .is_some_and(|pending| pending.receipt == *receipt))
                    {
                        return GlobalResponse::Network(Err(NetworkRpcError::internal(
                            "exec mutation lacks the current authenticated preparation",
                        )));
                    }
                // The borrowed proof stays inside this scheduler guard through
                // metadata -> engine admission. NoSeq deliberately has no turn.
                let ordinary = if matches!(&request, NetworkRequest::BeginOrdinaryFdRead { .. })
                    && self.cfg.sequentialize_threads
                {
                    match sched.ordinary_fd_observation(owner) {
                        Ok(proof) => Some(proof),
                        Err(error) => {
                            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                                format!(
                                    "ordinary reader lost its real foreground grant: {error:?}"
                                ),
                            )));
                        }
                    }
                } else {
                    None
                };
                let backend_failed = sched.backend_failure_waiter();
                let Some(engine) = self.network_engine.as_ref() else {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        "stream operation without a network engine",
                    )));
                };
                // A pipe holds this existing short table permit across its
                // original syscall/copyout. Do not enable the single-root
                // adapter after any sibling or replacement has been admitted.
                let pipe_files = match &request {
                    NetworkRequest::FdMutation(
                        crate::network_replay::NetworkFdMutationRequest::Begin {
                            files,
                            kind: crate::network_replay::NetworkFdMutationKind::PipePair { .. }
                                | crate::network_replay::NetworkFdMutationKind::SocketPair { .. },
                        },
                    ) => Some(*files),
                    NetworkRequest::FdMutation(
                        crate::network_replay::NetworkFdMutationRequest::PipeResult {
                            permit, ..
                        }
                        | crate::network_replay::NetworkFdMutationRequest::PipeInstallation {
                            permit,
                            ..
                        },
                    ) => Some(permit.files),
                    _ => None,
                };
                if let Some(files) = pipe_files {
                    let checked = self
                        .network_runtime
                        .as_ref()
                        .ok_or_else(|| std::io::Error::other("pipe pair lacks native runtime"))
                        .and_then(|runtime| runtime.foreground_root(owner))
                        .and_then(|root| {
                            if !self.cfg.sequentialize_threads || root.files() != files {
                                return Err(std::io::Error::other(
                                    "pipe pair changed sole-root table",
                                ));
                            }
                            sched
                                .foreground_native_observation(owner, &root)
                                .map(|_| ())
                        });
                    if let Err(error) = checked {
                        return GlobalResponse::Network(Err(NetworkRpcError::internal(
                            error.to_string(),
                        )));
                    }
                }
                // Obtain only an Arc under engine, then drop that guard before
                // taking metadata. Revalidate below after reacquiring engine:
                // lookup/upgrade alone never authorizes a reader.
                let metadata_files = match &request {
                    NetworkRequest::BeginFdRead { files, .. }
                    | NetworkRequest::BeginOrdinaryFdRead { files, .. } => Some(*files),
                    NetworkRequest::FdMutation(
                        crate::network_replay::NetworkFdMutationRequest::Installation {
                            permit,
                            ..
                        }
                        | crate::network_replay::NetworkFdMutationRequest::PipeResult {
                            permit, ..
                        }
                        | crate::network_replay::NetworkFdMutationRequest::PipeInstallation {
                            permit,
                            ..
                        },
                    ) => Some(permit.files),
                    _ => None,
                };
                let metadata = if let Some(files) = metadata_files {
                    match engine.lock().unwrap().fd_metadata(owner, files) {
                        Ok(metadata) => Some(metadata),
                        Err(error) => {
                            return GlobalResponse::Network(Err(NetworkRpcError::internal(
                                error.to_string(),
                            )));
                        }
                    }
                } else {
                    None
                };
                let metadata_guard = match metadata.as_ref().map(|metadata| metadata.lock()) {
                    Some(Ok(guard)) => Some(guard),
                    Some(Err(_)) => {
                        return GlobalResponse::Network(Err(NetworkRpcError::internal(
                            "reader metadata mutex was poisoned",
                        )));
                    }
                    None => None,
                };
                let Ok(mut engine) = engine.lock() else {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        "network engine mutex was poisoned",
                    )));
                };
                if let Some(files) = metadata_files
                    && let Err(error) = engine.validate_fd_metadata(
                        owner,
                        files,
                        metadata.as_ref().expect("reader metadata"),
                        metadata_guard.as_deref().expect("reader metadata guard"),
                    ) {
                        return GlobalResponse::Network(Err(NetworkRpcError::internal(
                            error.to_string(),
                        )));
                    }
                let pipe_metadata = match &request {
                    NetworkRequest::FdMutation(
                        crate::network_replay::NetworkFdMutationRequest::PipeResult { fds, .. },
                    ) => metadata_guard
                        .as_deref()
                        .expect("pipe metadata")
                        .validate_fresh_pipe_fds(*fds),
                    NetworkRequest::FdMutation(
                        crate::network_replay::NetworkFdMutationRequest::PipeInstallation {
                            permit,
                            changes,
                            ..
                        },
                    ) => engine.validate_fd_pair_metadata(owner, *permit,
                        metadata_guard.as_deref().expect("pair metadata"), changes)
                        .map_err(|e| reverie::Error::Tool(anyhow::anyhow!(e.to_string()))),
                    _ => Ok(()),
                };
                if let Err(error) = pipe_metadata {
                    return GlobalResponse::Network(Err(NetworkRpcError::internal(
                        error.to_string(),
                    )));
                }
                let observed_at = self.global_time.lock().unwrap().as_nanos();
                // Shared child creation becomes eligible before an inheritable
                // guest mutation can commit at this logical cut. No accepter is
                // selected by this release and unrelated channels never gate it.
                if engine.accepted_mode()
                    && engine.mode() == crate::network_replay::NetworkEngineMode::Replay
                    && let Err(error) = engine.release_eligible(observed_at) {
                        return GlobalResponse::Network(Err(NetworkRpcError::internal(
                            error.to_string(),
                        )));
                    }
                let result = match &request {
                    NetworkRequest::FdMutation(request) => engine
                        .recv_fd_mutation(owner, request.clone())
                        .and_then(|reply| {
                            if let crate::network_replay::NetworkFdMutationRequest::Installation {
                                change,
                                ..
                            } = request
                                && let Some(slot) = change.after
                            {
                                engine.note_epoll_published_metadata(
                                    owner,
                                    metadata.as_ref().expect("installation metadata"),
                                    metadata_guard
                                        .as_deref()
                                        .expect("installation metadata guard"),
                                    slot.binding,
                                )?;
                            }
                            if let crate::network_replay::NetworkFdMutationRequest::PipeInstallation {
                                changes, ..
                            } = request {
                                for change in changes.iter() {
                                    engine.note_epoll_published_metadata(
                                        owner,
                                        metadata.as_ref().expect("pipe metadata"),
                                        metadata_guard.as_deref().expect("pipe metadata guard"),
                                        change.after.expect("confirmed complete pipe pair").binding,
                                    )?;
                                }
                            }
                            if let crate::network_replay::NetworkFdMutationRequest::Unchanged {
                                permit,
                            } = request
                                && let Some(runtime) = &self.network_runtime
                            {
                                runtime
                                    .native_birth_semantics_consumed(*permit, None, false)
                                    .map_err(|error| {
                                        NetworkReplayError::FdPublicationProtocol(error.to_string())
                                    })?;
                            }
                            Ok(NetworkReply::FdMutation(reply))
                        }),
                    NetworkRequest::FdPublication(request) => {
                        use crate::network_replay::NetworkFdPublicationReply as P;
                        use crate::network_replay::NetworkFdPublicationRequest as Q;
                        match request {
                            Q::Acquire { files } => engine
                                .acquire_fd_publication(owner, *files)
                                .map(P::Admitted),
                            Q::Publish { permit, batch } => engine
                                .publish_fd_publication(owner, *permit, batch)
                                .map(P::Published),
                            Q::Acknowledge { permit, batch } => engine
                                .acknowledge_fd_publication(owner, *permit, batch)
                                .map(|()| P::Released),
                            Q::ReleaseEmpty { permit } => engine
                                .release_empty_fd_publication(owner, *permit)
                                .map(|()| P::Released),
                        }
                        .map(NetworkReply::FdPublication)
                    }
                    NetworkRequest::BeginStreamIngress { open_file } => engine
                        .begin_stream_ingress(owner, *open_file, observed_at)
                        .map(NetworkReply::IngressLease),
                    NetworkRequest::CompleteStreamIngress { lease, observation } => engine
                        .complete_stream_ingress(owner, *lease, observed_at, observation.clone())
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::StreamQueueStatus { open_file } => engine
                        .stream_queue_status(*open_file)
                        .map(NetworkReply::StreamQueueStatus),
                    NetworkRequest::ReadStreamChunkView {
                        lease,
                        offset,
                        maximum,
                    } => engine
                        .read_stream_chunk_view(owner, *lease, *offset, *maximum)
                        .map(NetworkReply::StreamChunkView),
                    NetworkRequest::FinishStreamChunk { lease, disposition } => engine
                        .finish_stream_chunk(owner, *lease, *disposition)
                        .and_then(|()| {
                            if let Some(runtime) = &self.network_runtime {
                                runtime.finish_native_stream_lease(owner, *lease).map_err(
                                    |_| NetworkReplayError::UnresolvedStreamOperation(*lease),
                                )?;
                            }
                            Ok(NetworkReply::Unit)
                        }),

                    NetworkRequest::ShadowMode => {
                        Ok(NetworkReply::ShadowMode(engine.shadow_mode()))
                    }
                    NetworkRequest::AcceptedMode => {
                        Ok(NetworkReply::AcceptedMode(engine.accepted_mode()))
                    }
                    NetworkRequest::RegisterAcceptedFreshSend { key, observed } => engine
                        .register_accepted_fresh_send(*key, *observed)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::BeginAcceptedSocket { call } => engine
                        .begin_accepted_socket(owner, *call)
                        .map(NetworkReply::AcceptedChild),
                    NetworkRequest::SubmitAcceptedSocket { lease } => engine
                        .submit_accepted_socket(owner, *lease)
                        .and_then(|()| {
                            if let Some(runtime) = &self.network_runtime {
                                runtime
                                    .submit_accept(
                                        owner,
                                        *lease,
                                        engine.accepted_capture_call(owner, *lease)?,
                                    )
                                    .map_err(|_| NetworkReplayError::UnresolvedAccept(*lease))?;
                            }
                            Ok(())
                        })
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::ResolveAcceptedProvider { .. }
                    | NetworkRequest::PrepareAcceptedEffect { .. }
                    | NetworkRequest::CollectAcceptedEffect { .. }
                    | NetworkRequest::EnrollAcceptedListener { .. } => {
                        unreachable!("listener enrollment uses authenticated asynchronous dispatch")
                    }
                    NetworkRequest::CaptureAcceptedReturn { .. } => {
                        unreachable!("synchronous capture uses early dispatch")
                    }
                    NetworkRequest::CancelAcceptedSocket { lease } => engine
                        .cancel_accepted_socket(owner, *lease)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::CompleteAcceptedSocket {
                        lease,
                        kernel_result,
                        installed_open_file,
                    } => engine
                        .complete_accepted_socket(
                            owner,
                            *lease,
                            *kernel_result,
                            *installed_open_file,
                            observed_at,
                        )
                        .map(NetworkReply::AcceptedCompletion),
                    NetworkRequest::AcceptedEndpoint { open_file, peer } => engine
                        .accepted_endpoint(*open_file, *peer)
                        .map(NetworkReply::AcceptedEndpoint),
                    NetworkRequest::RegisterStreamSocket {
                        open_file,
                        key,
                        namespace,
                        observed_profile,
                    } => engine
                        .register_stream_socket(
                            *open_file,
                            *key,
                            *namespace,
                            observed_profile.clone(),
                        )
                        .map(|state| NetworkReply::StreamSocketState(Some(state))),
                    NetworkRequest::StreamSocketState { open_file } => engine
                        .stream_socket_state(*open_file)
                        .map(NetworkReply::StreamSocketState),
                    NetworkRequest::StreamCallSocketState { call } => engine
                        .stream_call_socket_state(owner, *call)
                        .map(|state| NetworkReply::StreamSocketState(Some(state))),
                    NetworkRequest::BeginFdRead { files, fd }
                    | NetworkRequest::BeginOrdinaryFdRead { files, fd } => engine
                        .begin_fd_read(owner, *files, *fd)
                        .map(NetworkReply::FdRead),
                    NetworkRequest::FinishFdRead { admission } => engine
                        .finish_fd_read(owner, admission.clone())
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::BeginSocketControl { open_file } => engine
                        .begin_socket_controls(owner, vec![*open_file])
                        .and_then(|controls| engine.socket_control_view(owner, controls[0].1))
                        .map(NetworkReply::SocketControl),
                    NetworkRequest::BeginSocketControls { open_files } => engine
                        .begin_socket_controls(owner, open_files.clone())
                        .and_then(|controls| {
                            controls
                                .into_iter()
                                .map(|(ofd, lease)| {
                                    engine
                                        .socket_control_view(owner, lease)
                                        .map(|view| (ofd, view))
                                })
                                .collect()
                        })
                        .map(NetworkReply::SocketControls),
                    NetworkRequest::FinishSocketControl { lease, disposition } => engine
                        .finish_socket_control(owner, *lease, *disposition)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::BeginStreamCall { control_lease } => engine
                        .begin_stream_call(owner, *control_lease)
                        .map(NetworkReply::StreamCall),
                    NetworkRequest::NativeBeginStreamCall { .. }
                    | NetworkRequest::NativeStreamEffect { .. }
                    | NetworkRequest::NativeReleaseStreamCall { .. } => {
                        unreachable!("owned native execution uses dedicated dispatch")
                    }
                    NetworkRequest::ConfirmStreamCallPin { id, outcome } => engine
                        .confirm_stream_call_pin(owner, *id, *outcome)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::BeginStreamCallRelease { id } => engine
                        .begin_stream_call_release(owner, *id)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::FinishStreamCallRelease { id } => engine
                        .finish_stream_call_release(owner, *id)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::StreamCallQueueStatus { call } => engine
                        .stream_call_queue_status(owner, *call)
                        .map(NetworkReply::StreamQueueStatus),
                    NetworkRequest::BeginShadowProbe { call } => engine
                        .begin_shadow_probe(owner, *call, observed_at)
                        .and_then(|probe| {
                            if let Some(runtime) = &self.network_runtime {
                                runtime
                                    .bind_native_stream_lease(owner, *call, probe.lease)
                                    .map_err(|_| {
                                        NetworkReplayError::UnresolvedStreamOperation(probe.lease)
                                    })?;
                            }
                            Ok(NetworkReply::ShadowProbe(probe))
                        }),
                    NetworkRequest::SubmitStreamPhysical { lease, effect } => engine
                        .submit_stream_physical(owner, *lease, effect.clone())
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::ConfirmStreamPhysical { lease, result } => engine
                        .confirm_stream_physical(owner, *lease, result.clone())
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::CompleteShadowProbe { lease, bytes, eof } => {
                        (|| {
                            if let Some(runtime) = &self.network_runtime {
                                runtime
                                    .check_native_probe_bytes(owner, *lease, bytes)
                                    .map_err(|_| {
                                        NetworkReplayError::UnresolvedStreamOperation(*lease)
                                    })?;
                            }
                            engine.complete_shadow_probe(
                                owner,
                                *lease,
                                observed_at,
                                bytes.clone(),
                                *eof,
                            )?;
                            if let Some(runtime) = &self.network_runtime {
                                runtime.finish_native_stream_lease(owner, *lease).map_err(
                                    |_| NetworkReplayError::UnresolvedStreamOperation(*lease),
                                )?;
                            }
                            Ok(NetworkReply::Unit)
                        })()
                    }
                    NetworkRequest::ReserveStreamCallChunk {
                        call,
                        maximum,
                        peek_offset,
                    } => engine
                        .reserve_stream_call_chunk(owner, *call, *maximum, *peek_offset)
                        .and_then(|chunk| {
                            if let NetworkStreamChunk::Reserved { lease, .. } = &chunk
                                && let Some(runtime) = &self.network_runtime {
                                    runtime
                                        .bind_native_stream_lease(owner, *call, *lease)
                                        .map_err(|_| {
                                            NetworkReplayError::UnresolvedStreamOperation(*lease)
                                        })?;
                                }
                            Ok(NetworkReply::StreamChunk(chunk))
                        }),
                    NetworkRequest::ZeroStreamReceive { call, peek_offset } => engine
                        .prepare_zero_stream_receive(owner, *call, *peek_offset)
                        .map(NetworkReply::ZeroStreamReceive),
                    NetworkRequest::BeginRecordDrain { lease } => engine
                        .begin_record_drain(owner, *lease)
                        .map(|()| NetworkReply::Unit),
                    NetworkRequest::FinishRecordDrain { lease } => {
                        engine.finish_record_drain(owner, *lease).and_then(|()| {
                            if let Some(runtime) = &self.network_runtime {
                                runtime.finish_native_stream_lease(owner, *lease).map_err(
                                    |_| NetworkReplayError::UnresolvedStreamOperation(*lease),
                                )?;
                            }
                            Ok(NetworkReply::Unit)
                        })
                    }
                    NetworkRequest::BeginZeroStreamWait {
                        call,
                        record_operation,
                    } => engine
                        .begin_zero_stream_wait(owner, *call, *record_operation)
                        .map(NetworkReply::ZeroStreamWait),
                    NetworkRequest::FinishZeroStreamWait { id } => engine
                        .finish_zero_stream_wait(owner, *id)
                        .map(NetworkReply::ZeroStreamWaitEntered),
                    NetworkRequest::InspectZeroStreamWait { id } => engine
                        .zero_stream_wait_entered(owner, *id)
                        .map(NetworkReply::ZeroStreamWaitEntered),
                    NetworkRequest::CancelZeroStreamWait { id } => engine
                        .cancel_zero_stream_wait(owner, *id)
                        .map(|()| NetworkReply::Unit),

                    NetworkRequest::PreviewSocketOption { lease, option } => engine
                        .preview_socket_option(owner, *lease, option)
                        .map(NetworkReply::SocketOptionResult),
                    _ => unreachable!("only stream receipt operations enter this helper"),
                };
                // Failed admission and the holder's independently runnable
                // original selection share this engine cut (the F1 invariant).
                let prior_selection_can_progress = ordinary.is_some()
                    && matches!(result, Err(NetworkReplayError::StreamOperationBusy(_)))
                    && match &request {
                        NetworkRequest::BeginOrdinaryFdRead { files, fd } => engine
                            .fd_read_pending_external_selection(owner, *files, *fd)
                            .is_some_and(|(holder, operation)| {
                                holder != owner
                                    && sched.original_fd_grant_matches(holder, operation)
                            }),
                        _ => false,
                    };
                self.release_lifetime_ports(engine.take_lifetime_retired_ports());
                (result, backend_failed, prior_selection_can_progress)
            };
            match result {
                Err(NetworkReplayError::StreamOperationBusy(_))
                    if acquiring
                        && (!self.cfg.sequentialize_threads || prior_selection_can_progress) =>
                {
                    tokio::select! {
                        _ = changed => {},
                        _ = backend_failed => return GlobalResponse::ThreadExited,
                    }
                }
                result => {
                    // A successful finish can unblock either kind of waiter.
                    // No notification promises readiness or proves no effects.
                    if matches!(
                        result,
                        Ok(NetworkReply::Unit
                            | NetworkReply::FdMutation(
                                crate::network_replay::NetworkFdMutationReply::Unit
                            )
                            | NetworkReply::FdPublication(
                                crate::network_replay::NetworkFdPublicationReply::Released
                            ))
                    ) {
                        self.network_stream_changed.notify_waiters();
                    }
                    return GlobalResponse::Network(result.map_err(|error| {
                        NetworkRpcError::from_engine(
                            self.cfg.network_trace.policy,
                            NetworkFailurePhase::Other,
                            error,
                        )
                    }));
                }
            }
        }
    }

    fn release_lifetime_ports(&self, retired: impl IntoIterator<Item = OpenFileId>) {
        let mut ports = self.used_ports.lock().unwrap();
        let mut mappings = self.open_file_to_port.lock().unwrap();
        for open_file in retired {
            if let Some(port) = mappings.remove(&open_file) {
                ports.remove(&port);
            }
        }
    }

    fn commit_network_exec_files(&self, receipt: ExecFilesReceipt, event: &ExecReconnect) {
        if let Some(engine) = &self.network_engine {
            let mut engine = engine.lock().unwrap();
            engine
                .commit_exec_fd_table(receipt, event)
                .expect("authenticated backend exec event must match admitted FD transition");
            self.release_lifetime_ports(engine.take_lifetime_retired_ports());
        }
    }

    /// Actual backend final wait, using its still-owned ThreadState. Logical
    /// owner-gone/exec cleanup never enters this authority-producing callback.
    pub(crate) fn observe_native_stream_terminal<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &crate::tool_local::ThreadState<T>,
    ) {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let phase = if tid.as_raw() != owner.thread.as_raw() {
            Some("native stream final wait changed exact backend task")
        } else if self
            .network_engine
            .as_ref()
            .is_some_and(|engine| engine.lock().unwrap().native_stream_final_wait(owner))
        {
            Some("native stream task terminated before complete semantic capture")
        } else {
            None
        };
        if let Some(phase) = phase {
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase,
            });
            self.network_stream_changed.notify_waiters();
        }
    }

    fn native_capture_recovery(&self) -> Option<crate::network_runtime::NativeCaptureRecovery> {
        let ports = self.used_ports.clone();
        let mappings = self.open_file_to_port.clone();
        let engine = self.network_engine.as_ref()?.clone();
        let terminal_engine = engine.clone();
        let scheduler = self.sched.clone();
        let current_mms = self.registered_exec_mms.clone();
        let time = self.global_time.clone();
        let virtualize_metadata = self.cfg.virtualize_metadata;
        Some(crate::network_runtime::NativeCaptureRecovery::new(
            engine,
            self.network_stream_changed.clone(),
            move |retired| {
                let mut ports = ports.lock().unwrap();
                let mut mappings = mappings.lock().unwrap();
                for open_file in retired {
                    if let Some(port) = mappings.remove(&open_file) {
                        ports.remove(&port);
                    }
                }
            },
        ).with_terminal_allocator(self.cfg.network_trace.policy == NetworkPolicy::Record,
            move |publisher, admission, receipt, profile| {
                let protocol = |message: &str| NetworkReplayError::FdPublicationProtocol(message.into());
                let sched = scheduler.lock().unwrap();
                if sched.thread_is_logically_killed(publisher.thread)
                    || !sched.rpc_incarnation_matches(publisher.thread, publisher.mm)
                    || current_mms.lock().unwrap().get(&publisher.thread) != Some(&publisher.mm) {
                    return Err(protocol("terminal allocator publisher is no longer current"));
                }
                let actual = receipt.metadata();
                let mut metadata = actual.lock().unwrap();
                let mut engine = terminal_engine.lock().unwrap();
                let original = receipt.original_owner();
                let (mutation, _) = engine.original_allocator_publication(original, admission)?;
                if mutation.publication.permit.owner != publisher {
                    return Err(protocol("terminal allocator changed current publication owner"));
                }
                engine.validate_fd_metadata(publisher, mutation.publication.permit.files, &actual, &metadata)?;
                if mutation.publication.recovery.is_some() {
                    let binding = engine.recover_terminal_allocator_publication(original, admission,
                        publisher, &mutation, receipt, (&actual, &mut metadata))?;
                    engine.original_allocator_publication_finished(original, admission,
                        mutation.publication.permit)?;
                    return Ok(binding);
                }
                let bound = receipt.for_terminal_publication(mutation.publication.permit)
                    .map_err(|error| protocol(&error.to_string()))?;
                engine.confirm_original_allocator_publication_result(original, admission)?;
                let now = time.lock().unwrap().as_nanos();
                let stat = virtualize_metadata.then_some(profile.stat).flatten();
                let binding = if matches!(bound.source(), crate::network_runtime::original_installation::Source::EpollCreate(_)) {
                    if profile.fresh.is_some() || profile.stat.is_some()
                        || profile.opened != Some(crate::network_replay::original_installation::OpenatEnrollment {
                            kind: crate::fd::FdType::Epoll,
                            status_flags: bound.epoll_profile().map_err(|error| protocol(&error.to_string()))?.status_flags,
                        }) {
                        return Err(protocol("terminal epoll publication changed original profile"));
                    }
                    engine.publish_original_epoll_installation(publisher, &mutation.publication,
                        &bound, &actual, &mut metadata, now)?
                } else if let Some(opened) = profile.opened {
                    engine.publish_original_openat_installation(publisher, &mutation.publication,
                        &bound, (&actual, &mut metadata), (opened, stat), now)?
                } else {
                    if let Some(fresh) = &profile.fresh
                        && engine.accepted_mode() {
                            engine.register_accepted_fresh_send(fresh.key,
                                fresh.observed_profile.as_ref().map(|_| detcore_model::network_trace::ReceiveTimeoutV3::Infinite))?;
                        }
                    engine.publish_original_installation(publisher, &mutation.publication,
                        &bound, (&actual, &mut metadata), (crate::network_replay::original_installation::socket_installation_flags(
                            admission.arguments.address as u32 as i32), stat, profile.fresh), now)?
                };
                engine.original_allocator_publication_finished(original, admission,
                    mutation.publication.permit)?;
                Ok(binding)
            }))
    }

    fn abandon_network_owners(&self, owners: impl IntoIterator<Item = NetworkStreamOwner>) {
        let owners: Vec<_> = owners.into_iter().collect();
        if let Some(engine) = &self.network_engine {
            let mut engine = engine.lock().unwrap();
            for owner in &owners {
                engine.retire_fd_table_owner(*owner);
                engine.stream_owner_gone(*owner);
            }
            self.release_lifetime_ports(engine.take_lifetime_retired_ports());
        }
        // Completion-before-exit is recovered here; exit-before-completion is
        // recovered by the original worker. Both claim the same engine phase.
        if let (Some(runtime), Some(recovery)) =
            (&self.network_runtime, self.native_capture_recovery())
        {
            for owner in owners {
                if let Err(error) = runtime.queue_native_capture_recovery(owner, recovery.clone()) {
                    // Worker admission also retains a terminal runtime failure.
                    warn!("native capture retirement submission failed: {}", error);
                }
            }
        }
        self.network_stream_changed.notify_waiters();
    }

    /// Consuming cleanup runs before ordinary RPC tombstone/clock admission.
    /// The backend supplies the sender identity; a stale MmId marks only stale
    /// receipts. A pending candidate-MM cleanup also identifies its exact
    /// prepared caller, never every task sharing the old address space.
    fn recv_network_owner_gone(&self, owner: NetworkStreamOwner) {
        if let Some(runtime) = &self.network_runtime {
            runtime.forget_task(owner);
        }
        let previous = self
            .pending_exec_states
            .lock()
            .unwrap()
            .values()
            .find(|pending| {
                pending.receipt.caller == owner.thread
                    && pending.receipt.mm.for_exec(pending.receipt.process) == owner.mm
            })
            .map(|pending| NetworkStreamOwner {
                thread: owner.thread,
                mm: pending.receipt.mm,
            });
        self.abandon_network_owners(std::iter::once(owner).chain(previous));
    }

    fn recv_network_request(
        &self,
        request: NetworkRequest,
    ) -> Result<NetworkReply, NetworkRpcError> {
        let phase = match &request {
            NetworkRequest::EnsureChannel { .. } => NetworkFailurePhase::Binding,
            NetworkRequest::TransmitStream { .. } => NetworkFailurePhase::Transmit,
            NetworkRequest::Shutdown(..) => NetworkFailurePhase::Shutdown,
            _ => NetworkFailurePhase::Other,
        };
        let engine = self.network_engine.as_ref().ok_or_else(|| {
            NetworkRpcError::internal("network engine request under deny/unsafe-live policy")
        })?;
        let mut engine = engine
            .lock()
            .map_err(|_| NetworkRpcError::internal("network engine mutex was poisoned"))?;
        let result = match request {
            NetworkRequest::BeginStreamIngress { .. }
            | NetworkRequest::CompleteStreamIngress { .. }
            | NetworkRequest::StreamQueueStatus { .. }
            | NetworkRequest::ReadStreamChunkView { .. }
            | NetworkRequest::FinishStreamChunk { .. }
            | NetworkRequest::ShadowMode
            | NetworkRequest::AcceptedMode
            | NetworkRequest::RegisterAcceptedFreshSend { .. }
            | NetworkRequest::BeginAcceptedSocket { .. }
            | NetworkRequest::SubmitAcceptedSocket { .. }
            | NetworkRequest::ResolveAcceptedProvider { .. }
            | NetworkRequest::PrepareAcceptedEffect { .. }
            | NetworkRequest::CollectAcceptedEffect { .. }
            | NetworkRequest::EnrollAcceptedListener { .. }
            | NetworkRequest::CaptureAcceptedReturn { .. }
            | NetworkRequest::CancelAcceptedSocket { .. }
            | NetworkRequest::CompleteAcceptedSocket { .. }
            | NetworkRequest::AcceptedEndpoint { .. }
            | NetworkRequest::RegisterStreamSocket { .. }
            | NetworkRequest::StreamSocketState { .. }
            | NetworkRequest::StreamCallSocketState { .. }
            | NetworkRequest::BeginSocketControl { .. }
            | NetworkRequest::BeginSocketControls { .. }
            | NetworkRequest::BeginFdRead { .. }
            | NetworkRequest::BeginOrdinaryFdRead { .. }
            | NetworkRequest::FinishFdRead { .. }
            | NetworkRequest::FinishSocketControl { .. }
            | NetworkRequest::BeginStreamCall { .. }
            | NetworkRequest::NativeBeginStreamCall { .. }
            | NetworkRequest::BeginRecordedOriginalFile { .. }
            | NetworkRequest::BeginOriginalFileFromRead { .. }
            | NetworkRequest::BeginEmulatedReadFromRead { .. }
            | NetworkRequest::CompleteEmulatedRead { .. }
            | NetworkRequest::SelectRecordedOriginalFile { .. }
            | NetworkRequest::CompleteRecordedOriginalFile { .. }
            | NetworkRequest::CompleteRecordedReadInterruption { .. }
            | NetworkRequest::NativeBeginOriginalConnect { .. }
            | NetworkRequest::NativeBeginForegroundEpollCtl { .. }
            | NetworkRequest::NativeForegroundEpollCtlReturned { .. }
            | NetworkRequest::NativeBeginOriginalSocket { .. }
            | NetworkRequest::NativeBeginOriginalAllocator { .. }
            | NetworkRequest::NativePublishOriginalSocket { .. }
            | NetworkRequest::NativeObserveOriginalOpenat { .. }
            | NetworkRequest::NativePublishOriginalOpenat { .. }
            | NetworkRequest::NativePublishOriginalEpoll { .. }
            | NetworkRequest::NativeBeginOriginalExternalFromRead { .. }
            | NetworkRequest::NativeSubmitOriginalConnect { .. }
            | NetworkRequest::NativeOriginalConnectOutcome { .. }
            | NetworkRequest::NativeOriginalConnectFailed { .. }
            | NetworkRequest::NativeRetireOriginalConnect { .. }
            | NetworkRequest::NativeRetireInterruptedRead { .. }
            | NetworkRequest::NativeStreamEffect { .. }
            | NetworkRequest::NativeReleaseStreamCall { .. }
            | NetworkRequest::ConfirmStreamCallPin { .. }
            | NetworkRequest::BeginStreamCallRelease { .. }
            | NetworkRequest::FinishStreamCallRelease { .. }
            | NetworkRequest::StreamCallQueueStatus { .. }
            | NetworkRequest::BeginShadowProbe { .. }
            | NetworkRequest::SubmitStreamPhysical { .. }
            | NetworkRequest::ConfirmStreamPhysical { .. }
            | NetworkRequest::CompleteShadowProbe { .. }
            | NetworkRequest::ReserveStreamCallChunk { .. }
            | NetworkRequest::ZeroStreamReceive { .. }
            | NetworkRequest::BeginRecordDrain { .. }
            | NetworkRequest::FinishRecordDrain { .. }
            | NetworkRequest::BeginZeroStreamWait { .. }
            | NetworkRequest::FinishZeroStreamWait { .. }
            | NetworkRequest::InspectZeroStreamWait { .. }
            | NetworkRequest::CancelZeroStreamWait { .. }
            | NetworkRequest::PreviewSocketOption { .. }
            | NetworkRequest::FdPublication(..)
            | NetworkRequest::FdMutation(..) => {
                unreachable!("stream receipt RPC uses authenticated async path")
            }
            NetworkRequest::RecordInput(input) => {
                engine.record_input(input).map(|()| NetworkReply::Unit)
            }
            NetworkRequest::EnsureChannel { open_file, binding } => engine
                .ensure_channel(open_file, binding)
                .map(|channel| NetworkReply::Channel(Some(channel))),
            NetworkRequest::CaptureStreamInput {
                open_file,
                observed_at,
                input,
            } => (|| -> Result<NetworkReply, NetworkReplayError> {
                let channel = engine
                    .channel_for(open_file)
                    .ok_or(NetworkReplayError::UnboundOpenFile(open_file))?;
                let mut progress = self.network_record_progress.lock().unwrap();
                let progress = progress.entry(open_file).or_default();
                let event = match input {
                    NetworkCapturedStreamInput::Bytes(bytes) => NetworkInputKindV2::StreamBytes {
                        stream_offset: progress.inbound_stream,
                        bytes,
                    },
                    NetworkCapturedStreamInput::EndOfFile => NetworkInputKindV2::PeerShutdown {
                        stream_offset: progress.inbound_stream,
                        direction: NetworkShutdownV2::Write,
                    },
                    NetworkCapturedStreamInput::Error(errno) => NetworkInputKindV2::SocketError {
                        stream_offset: progress.inbound_stream,
                        errno,
                    },
                    NetworkCapturedStreamInput::Connect(result) => {
                        NetworkInputKindV2::Connect(result)
                    }
                };
                let byte_count = match &event {
                    NetworkInputKindV2::StreamBytes { bytes, .. } => bytes.len() as u64,
                    _ => 0,
                };
                engine.record_input(NetworkInputEventV2 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: observed_at,
                        after_transmitted_offset: progress.outbound_stream,
                    },
                    event,
                })?;
                progress.inbound_stream = progress
                    .inbound_stream
                    .checked_add(byte_count)
                    .ok_or(NetworkReplayError::Overflow)?;
                Ok(NetworkReply::Unit)
            })(),
            NetworkRequest::CaptureStreamOutput { open_file, output } => {
                (|| -> Result<NetworkReply, NetworkReplayError> {
                    let channel = engine
                        .channel_for(open_file)
                        .ok_or(NetworkReplayError::UnboundOpenFile(open_file))?;
                    let mut progress = self.network_record_progress.lock().unwrap();
                    let progress = progress.entry(open_file).or_default();
                    let (event, byte_count) = match output {
                        NetworkCapturedStreamOutput::Bytes(bytes) => {
                            let byte_count = bytes.len() as u64;
                            (
                                NetworkOutputKindV2::StreamBytes {
                                    stream_offset: progress.outbound_stream,
                                    bytes,
                                },
                                byte_count,
                            )
                        }
                        NetworkCapturedStreamOutput::Error(errno) => (
                            NetworkOutputKindV2::SocketError {
                                stream_offset: progress.outbound_stream,
                                errno,
                            },
                            0,
                        ),
                        NetworkCapturedStreamOutput::Shutdown(direction) => (
                            NetworkOutputKindV2::Shutdown {
                                stream_offset: progress.outbound_stream,
                                direction,
                            },
                            0,
                        ),
                    };
                    engine.record_output(NetworkOutputEventV2 { channel, event })?;
                    progress.outbound_stream = progress
                        .outbound_stream
                        .checked_add(byte_count)
                        .ok_or(NetworkReplayError::Overflow)?;
                    Ok(NetworkReply::Unit)
                })()
            }
            NetworkRequest::CaptureReadiness {
                open_file,
                observed_at,
                readiness,
            } => (|| -> Result<NetworkReply, NetworkReplayError> {
                let channel = engine
                    .channel_for(open_file)
                    .ok_or(NetworkReplayError::UnboundOpenFile(open_file))?;
                let outbound_stream = self
                    .network_record_progress
                    .lock()
                    .unwrap()
                    .get(&open_file)
                    .map_or(0, |progress| progress.outbound_stream);
                engine.record_input(NetworkInputEventV2 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: observed_at,
                        after_transmitted_offset: outbound_stream,
                    },
                    event: NetworkInputKindV2::Readiness(readiness),
                })?;
                Ok(NetworkReply::Unit)
            })(),
            NetworkRequest::Retire(open_file) => {
                Ok(NetworkReply::Channel(engine.retire_open_file(open_file)))
            }
            NetworkRequest::ReleaseEligible(now) => engine
                .release_eligible(now)
                .map(|channels| NetworkReply::ReadyChannels(channels.into_iter().collect())),
            NetworkRequest::ReceiveStream {
                open_file,
                maximum,
                nonblocking,
                flags,
                receive_low_water,
            } => engine
                .receive_stream_with_options(
                    open_file,
                    NetworkReceiveOptions {
                        maximum,
                        nonblocking,
                        flags,
                        receive_low_water,
                    },
                )
                .map(|outcome| {
                    NetworkReply::StreamReceive(match outcome {
                        StreamReceiveOutcome::Bytes(bytes) => NetworkStreamReceive::Bytes(bytes),
                        StreamReceiveOutcome::EndOfFile => NetworkStreamReceive::EndOfFile,
                        StreamReceiveOutcome::Error(errno) => NetworkStreamReceive::Error(errno),
                        StreamReceiveOutcome::WouldBlock => NetworkStreamReceive::WouldBlock,
                        StreamReceiveOutcome::Pending => NetworkStreamReceive::Pending,
                    })
                }),
            NetworkRequest::TransmitStream { open_file, bytes } => {
                engine.transmit_stream(open_file, &bytes).map(|outcome| {
                    NetworkReply::StreamTransmit(match outcome {
                        StreamTransmitOutcome::Accepted(count) => {
                            NetworkStreamTransmit::Accepted(count)
                        }
                        StreamTransmitOutcome::Error(errno) => NetworkStreamTransmit::Error(errno),
                    })
                })
            }
            NetworkRequest::Shutdown(open_file, direction) => engine
                .shutdown(open_file, direction)
                .map(|()| NetworkReply::Unit),
            NetworkRequest::PreflightSocketShutdown => engine
                .preflight_socket_shutdown()
                .map(|()| NetworkReply::Unit),
            NetworkRequest::TakeConnectionOutcome(open_file) => {
                (|| -> Result<NetworkReply, NetworkReplayError> {
                    let outcome = engine.take_connection_outcome(open_file)?;
                    let outcome = match outcome {
                        Some(ConnectionOutcome::Connect(result)) => {
                            Some(NetworkConnection::Connect(result))
                        }
                        Some(ConnectionOutcome::Accept {
                            accepted,
                            peer,
                            ancillary,
                        }) => Some(NetworkConnection::Accept {
                            accepted,
                            peer,
                            ancillary,
                        }),
                        None => match engine.receive_stream_with_options(
                            open_file,
                            NetworkReceiveOptions {
                                maximum: 0,
                                nonblocking: true,
                                flags: 0,
                                receive_low_water: 1,
                            },
                        )? {
                            StreamReceiveOutcome::Error(errno) => {
                                Some(NetworkConnection::Error(errno))
                            }
                            StreamReceiveOutcome::WouldBlock
                            | StreamReceiveOutcome::Pending
                            | StreamReceiveOutcome::Bytes(_)
                            | StreamReceiveOutcome::EndOfFile => None,
                        },
                    };
                    Ok(NetworkReply::Connection(outcome))
                })()
            }
            NetworkRequest::Readiness(open_file) => {
                engine.readiness(open_file).map(NetworkReply::Readiness)
            }
            NetworkRequest::ChannelFor(open_file) => {
                Ok(NetworkReply::Channel(engine.channel_for(open_file)))
            }
        };
        result.map_err(|error| {
            NetworkRpcError::from_engine(self.cfg.network_trace.policy, phase, error)
        })
    }
}

/// Identity and final accounting for an asynchronous scheduler deregistration.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadDeregistration {
    pub(crate) dettid: DetTid,
    pub(crate) detpid: DetPid,
    pub(crate) mm: MmId,
    /// Carried by the consuming ThreadState owner, independently of detpid's
    /// delayed initialization and the parent's scheduler registration RPC.
    pub(crate) thread_start_entered: bool,
    pub(crate) timeslice_stats: TimesliceStats,
    pub(crate) syscall_count: u64,
    pub(crate) chaos_epochs: Vec<ChaosEpochTransition>,
}

/// The one normalized request vocabulary used by all engine-owned socket and
/// readiness syscalls. Private operation admission carries run-local task/call
/// custody; those identities are never encoded into the portable network trace.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkRequest {
    /// Authenticated descriptor installation prefix protocol.
    FdPublication(crate::network_replay::NetworkFdPublicationRequest),
    /// Exact physical descriptor mutation protocol.
    FdMutation(crate::network_replay::NetworkFdMutationRequest),
    /// Append one legacy journal-only observation after its guest syscall.
    RecordInput(NetworkInputEventV2),
    /// Atomically assign offsets and append a live stream observation.
    CaptureStreamInput {
        /// Stable socket open-file description.
        open_file: OpenFileId,
        /// Continuous logical time at host observation.
        observed_at: LogicalTime,
        /// Normalized input result.
        input: NetworkCapturedStreamInput,
    },
    /// Atomically assign offsets and append guest stream output.
    CaptureStreamOutput {
        /// Stable socket open-file description.
        open_file: OpenFileId,
        /// Normalized output result.
        output: NetworkCapturedStreamOutput,
    },
    /// Append one host readiness observation with engine-owned release gates.
    CaptureReadiness {
        /// Stable socket open-file description.
        open_file: OpenFileId,
        /// Continuous logical time at host observation.
        observed_at: LogicalTime,
        /// Complete level-readiness snapshot, including clear transitions.
        readiness: NetworkReadinessV2,
    },
    /// Retire the binding after the final descriptor alias closes.
    Retire(OpenFileId),
    /// Release replay input eligible at this exact logical time.
    ReleaseEligible(LogicalTime),
    /// Consume available stream input without matching a caller identity.
    ReceiveStream {
        /// Stable socket open-file description.
        open_file: OpenFileId,
        /// Guest buffer capacity.
        maximum: usize,
        /// Whether absence returns `WouldBlock` instead of `Pending`.
        nonblocking: bool,
        /// Linux receive flags relevant to data movement.
        flags: i32,
        /// Effective `SO_RCVLOWAT` value.
        receive_low_water: usize,
    },
    /// Validate and advance outbound stream progress.
    TransmitStream {
        /// Stable socket open-file description.
        open_file: OpenFileId,
        /// Guest-provided bytes in stream order.
        bytes: Vec<u8>,
    },
    /// Validate an outbound half/full-close transition.
    Shutdown(OpenFileId, NetworkShutdownV2),
    /// Consume one released connect or accept result.
    TakeConnectionOutcome(OpenFileId),
    /// Query modeled readiness without consuming availability.
    Readiness(OpenFileId),
    /// Resolve the trace channel currently bound to an open file.
    ChannelFor(OpenFileId),
    /// Allocate or match a channel by endpoint facts, never by caller-derived
    /// trace ID. Existing bindings undergo the same metadata checks.
    EnsureChannel {
        /// Stable OFD shared by all descriptor aliases.
        open_file: OpenFileId,
        /// Exact endpoint facts, known local constraints and accept selection.
        binding: NetworkChannelBinding,
    },
    /// Latch possible physical receive effects before kernel submission.
    BeginStreamIngress {
        /// Stable OFD shared by all descriptor aliases.
        open_file: OpenFileId,
    },
    /// Publish a known scratch result before guest delivery.
    CompleteStreamIngress {
        /// Receipt returned before the corresponding kernel submission.
        lease: NetworkStreamLeaseId,
        /// Known result recovered from that exact physical operation.
        observation: NetworkIngressObservation,
    },
    /// Contiguous payload and terminal conditions for threshold-aware waits.
    StreamQueueStatus {
        /// Stable OFD whose payload and terminal conditions are inspected.
        open_file: OpenFileId,
    },
    /// Fetch a bounded view while retaining the entire selected copy unit.
    ReadStreamChunkView {
        /// Existing immutable selection owned by this authenticated caller.
        lease: NetworkStreamLeaseId,
        /// Offset within the selected unit, not within the whole stream.
        offset: usize,
        /// View capacity, which must not exceed 512 bytes.
        maximum: usize,
    },
    /// Resolve the whole selected copy unit without inferring consumption from writes.
    FinishStreamChunk {
        /// Exact selection receipt being acknowledged once.
        lease: NetworkStreamLeaseId,
        /// Known whole-selection outcome, distinct from guest bytes written.
        disposition: NetworkStreamChunkDisposition,
    },
    /// Query explicit V3 enrollment without upgrading legacy traces.
    ShadowMode,
    /// Register actual fresh socket options before any guest mutation.
    RegisterStreamSocket {
        /// Exact stable socket open-file identity.
        open_file: OpenFileId,
        /// Exact fresh kernel socket class.
        key: StreamSocketKeyV3,
        /// Authenticated run-local namespace identity.
        namespace: NetworkStreamNamespace,
        /// Record observation; Replay must supply None.
        observed_profile: Option<FreshStreamSocketProfileV3>,
    },
    /// Inspect enrolled state without consulting a Replay placeholder.
    StreamSocketState {
        /// Exact stable socket open-file identity.
        open_file: OpenFileId,
    },
    /// Inspect the socket through an admitted call after final descriptor close.
    StreamCallSocketState {
        /// Exact active-call reference owned by this task/MM.
        call: NetworkStreamCallId,
    },
    /// Acquire short exact-OFD exclusion and return its current facts.
    BeginSocketControl {
        /// Exact stable socket open-file identity.
        open_file: OpenFileId,
    },
    /// Acquire the unique sorted set atomically, with none retained on contention.
    BeginSocketControls {
        /// Stable OFD identities acquired as one sorted set.
        open_files: Vec<OpenFileId>,
    },
    /// Finish one short control after physical/lifetime reconciliation.
    FinishSocketControl {
        /// Exact owned operation receipt.
        lease: NetworkStreamLeaseId,
        /// Known completion, distinct from future destruction.
        disposition: NetworkSocketControlFinish,
    },
    /// Allocate an active-call reference before physical host pin acquisition.
    BeginStreamCall {
        /// Existing short exclusion receipt.
        control_lease: NetworkStreamLeaseId,
    },
    /// The same Call/table membership for an operation consumed from the
    /// Replayer's existing event stream. This submits no native provider work.
    BeginRecordedOriginalFile {
        /// Exact logical table and operation operands for the original Call admission.
        arguments: crate::network_replay::original_connect::Arguments,
    },
    /// The owning ThreadState has copied its logical metadata under that permit.
    SelectRecordedOriginalFile {
        /// Existing logical Call whose admitted metadata snapshot has been retained.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Exact semantic Return consumed at the existing delegate boundary.
    CompleteRecordedOriginalFile {
        /// Same logical admission whose recorded result is being consumed.
        admission: crate::network_replay::original_connect::Admission,
        /// Semantic return from the recorded event stream, not a native completion receipt.
        returned: i64,
    },
    /// Consume an explicit recorded Read interruption without a native result.
    CompleteRecordedReadInterruption {
        /// Same selected logical Read and owner retained during consumption.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Retain an original allocator without excluding other table operations
    /// while the kernel may block. Publication admission follows actual return.
    NativeBeginOriginalAllocator {
        /// Exact Socket or Openat operands, not a returned numeric FD.
        arguments: crate::network_replay::original_connect::Arguments,
    },
    /// Finish the same Call's potentially blocking held-file observation while
    /// its owner still holds the actual ordinary external-IO grant.
    NativeObserveOriginalOpenat {
        /// Original actual completion; no numeric descriptor recapture claim.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Publish the actual Openat effect using its retained held-file observation.
    NativePublishOriginalOpenat {
        /// Same original Call and actual native completion.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Publish exact original epoll creation through the existing allocator transaction.
    NativePublishOriginalEpoll {
        /// Same original Call and actual native completion.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Transfer the existing submitted allocator admission into the original Call.
    NativeBeginOriginalSocket {
        /// Exact Socket operands and original invocation identity.
        arguments: crate::network_replay::original_connect::Arguments,
        /// Existing allocator custody, not a second admission or executor.
        mutation: crate::network_replay::NetworkFdMutationAdmission,
    },
    /// Publish the actual original installation from retained provider/Call facts.
    NativePublishOriginalSocket {
        /// Exact original Call, already completed by backend and provider.
        admission: crate::network_replay::original_connect::Admission,
        /// Exact retained held-file fstat, checked again by the shared publisher.
        stat: Option<crate::stat::DetStat>,
        /// Profile derived from that same held observation without guest buffers.
        enrollment: Option<crate::network_replay::original_installation::FreshStreamEnrollment>,
    },
    /// Narrow positive capability; no production syscall gate selects this path.
    NativeBeginForegroundEpollCtl {
        /// Exact original ctl operands; permission is issued from private state.
        arguments: crate::network_replay::original_connect::Arguments,
    },
    /// Authenticate the actual native return before leaving the foreground.
    NativeForegroundEpollCtlReturned {
        /// The same Call, retaining its private capability and native result.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Capture and arm one exact original invocation under continuous table custody.
    NativeBeginOriginalConnect {
        /// Original operation and descriptor-table snapshot to authenticate.
        arguments: crate::network_replay::original_connect::Arguments,
    },
    /// Consume the reader admitted at this original external scheduling grant.
    /// No physical operation has occurred and no table permit is reacquired.
    NativeBeginOriginalExternalFromRead {
        /// Exact original Connect/Close operands and selected logical binding.
        arguments: crate::network_replay::original_connect::Arguments,
        /// Existing unsubmitted reader, transferred into the same Call.
        read: crate::network_replay::NetworkFdReadAdmission,
    },
    /// Admission to enter the original kernel syscall; never a completion receipt.
    NativeSubmitOriginalConnect {
        /// Exact engine-issued call and admitted original arguments.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Return the retained actual kernel address/result after physical retirement.
    NativeOriginalConnectOutcome {
        /// Exact call whose retained physical outcome is requested.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Fail the run while retaining the owning Guest future for actual final wait.
    /// This grants neither a native result nor physical retirement authority.
    NativeOriginalConnectFailed {
        /// Existing Guest custody, including invocation and return observations.
        local: crate::network_replay::original_connect::Local,
        /// Failure diagnostic retained while actual final wait remains owned.
        detail: String,
    },
    /// Acknowledge semantic consumption and retire the existing Call.
    NativeRetireOriginalConnect {
        /// Exact call whose semantic outcome has been consumed.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Wait for positive provider cancellation/ACK after an actual backend
    /// pre-entry interruption, then retire the existing Call without a result.
    NativeRetireInterruptedRead {
        /// Exact interrupted Call; this request itself is not no-entry proof.
        admission: crate::network_replay::original_connect::Admission,
    },
    /// Consume an existing logical reader into the one original file Call.
    BeginOriginalFileFromRead {
        /// Exact original operation and its admitted descriptor binding.
        arguments: crate::network_replay::original_connect::Arguments,
        /// Existing unsubmitted table/OFD read custody.
        read: crate::network_replay::NetworkFdReadAdmission,
        /// Required result producer; this declaration supplies no receipt.
        source: crate::OriginalFileExecution,
    },
    /// Retain one already classified Detcore-modeled Read in the shared Call.
    BeginEmulatedReadFromRead {
        /// Exact original Read operands and current admitted binding.
        arguments: crate::network_replay::original_connect::Arguments,
        /// Consumed short publication/descriptor admission.
        read: crate::network_replay::NetworkFdReadAdmission,
    },
    /// Complete Detcore's modeled Read without supplying a native receipt.
    CompleteEmulatedRead {
        /// Exact logical Call returned by the consuming handoff.
        admission: crate::network_replay::original_connect::Admission,
        /// Actual modeled copy/result, distinct from recorded Return.
        returned: i64,
    },
    /// Observe an FD in an existing ordinary foreground turn. This requests
    /// no resource and carries no native selection or syscall result.
    BeginOrdinaryFdRead {
        /// Exact table associated with the current task/MM and metadata Arc.
        files: FilesId,
        /// Original numeric descriptor, including invalid operands.
        fd: i32,
    },
    /// Admit one current FD binding under the existing table/OFD authorities.
    BeginFdRead {
        /// Exact descriptor table owned by the sender.
        files: detcore_model::fd::FilesId,
        /// Original numeric lookup operand.
        fd: i32,
    },
    /// Release a logical admission which has not entered physical capture.
    FinishFdRead {
        /// Exact untransferred reader authority.
        admission: crate::network_replay::NetworkFdReadAdmission,
    },
    /// Consume the admitted FD/OFD authority directly into the existing Call.
    NativeBeginStreamCall {
        /// Exact current binding/control/publication; validated before capture.
        read: crate::network_replay::NetworkFdReadAdmission,
    },
    /// Submit, execute, retain and confirm one owned native queue effect.
    NativeStreamEffect {
        /// Engine-produced lease already joined to its original call.
        lease: NetworkStreamLeaseId,
        /// Existing shared physical effect vocabulary.
        effect: NetworkStreamPhysicalEffect,
    },
    /// Join actual native close to the existing semantic call lifetime receipt.
    NativeReleaseStreamCall {
        /// Exact owned call, not a descriptor lookup.
        call: NetworkStreamCallId,
    },
    /// Publish the exact physical pin acquisition outcome.
    ConfirmStreamCallPin {
        /// Exact owned call or wait identity.
        id: NetworkStreamCallId,
        /// Matched physical pin acquisition result.
        outcome: NetworkStreamPinOutcome,
    },
    /// Latch a physical host pin release before performing it.
    BeginStreamCallRelease {
        /// Exact owned call or wait identity.
        id: NetworkStreamCallId,
    },
    /// Confirm actual host pin release; Drop is not confirmation.
    FinishStreamCallRelease {
        /// Exact owned call or wait identity.
        id: NetworkStreamCallId,
    },
    /// Inspect queues through a still-active call.
    StreamCallQueueStatus {
        /// Exact active-call reference owned by this task/MM.
        call: NetworkStreamCallId,
    },
    /// Acquire the nonconsuming physical shadow observation receipt.
    BeginShadowProbe {
        /// Exact active-call reference owned by this task/MM.
        call: NetworkStreamCallId,
    },
    /// Latch the next exact physical operation before executing it.
    SubmitStreamPhysical {
        /// Exact owned operation receipt.
        lease: NetworkStreamLeaseId,
        /// Next physical operation, latched before execution.
        effect: NetworkStreamPhysicalEffect,
    },
    /// Confirm only the submitted physical operation's actual result.
    ConfirmStreamPhysical {
        /// Exact owned operation receipt.
        lease: NetworkStreamLeaseId,
        /// Actual result of the submitted operation.
        result: NetworkStreamPhysicalResult,
    },
    /// Atomically publish the observed suffix at the paid completion boundary.
    CompleteShadowProbe {
        /// Exact owned operation receipt.
        lease: NetworkStreamLeaseId,
        /// New nonconsumed suffix from the validated observation.
        bytes: Vec<u8>,
        /// Terminal claim requiring matched physical evidence.
        eof: bool,
    },
    /// Select one whole declared copy unit through an active call.
    ReserveStreamCallChunk {
        /// Exact active-call reference owned by this task/MM.
        call: NetworkStreamCallId,
        /// Remaining guest length; individual views remain bounded.
        maximum: usize,
        /// Logical nonconsuming offset within queued input.
        peek_offset: usize,
    },
    /// Resolve zero receive availability/error atomically without a byte receipt.
    ZeroStreamReceive {
        /// Exact active-call reference owned by this task/MM.
        call: NetworkStreamCallId,
        /// Logical nonconsuming offset within queued input.
        peek_offset: usize,
    },
    /// Begin exact physical drain only after full selected-unit guest copy.
    BeginRecordDrain {
        /// Exact owned operation receipt.
        lease: NetworkStreamLeaseId,
    },
    /// Commit the selected unit only after its exact drain has been confirmed.
    FinishRecordDrain {
        /// Exact owned operation receipt.
        lease: NetworkStreamLeaseId,
    },
    /// Associate the zero receive with a transient scheduler wait receipt.
    BeginZeroStreamWait {
        /// Exact active-call reference owned by this task/MM.
        call: NetworkStreamCallId,
        /// Exact Record background operation; absent in Replay.
        record_operation: Option<crate::resources::ExternalOpId>,
    },
    /// Read and consume the scheduler's logical entry receipt.
    FinishZeroStreamWait {
        /// Exact owned call or wait identity.
        id: NetworkZeroStreamWaitId,
    },

    /// Explicit appended receive-model variant, never inferred from a listener.
    AcceptedMode,
    /// Pin and enroll the exact admitted listener before its first physical listen.
    EnrollAcceptedListener {
        /// Authenticated active listener reference.
        call: NetworkStreamCallId,
        /// Original guest descriptor, used only at the stopped ptrace boundary.
        fd: i32,
    },
    /// Fresh send-timeout observation before any mutation; no old V3 default.
    RegisterAcceptedFreshSend {
        /// Exact socket class.
        key: StreamSocketKeyV3,
        /// Record's actual fresh observation; absent in Replay.
        observed: Option<detcore_model::network_trace::ReceiveTimeoutV3>,
    },
    /// Reserve an eligible shared child while retaining the listener call.
    BeginAcceptedSocket {
        /// Authenticated active listener reference.
        call: NetworkStreamCallId,
    },
    /// Latch possible physical descriptor allocation before kernel injection.
    SubmitAcceptedSocket {
        /// Exact owned accept receipt.
        lease: crate::network_replay::NetworkAcceptLeaseId,
    },
    /// Arm the provider's exact original physical accept before kernel entry.
    PrepareAcceptedEffect {
        /// Existing submitted accept receipt authenticated by owner and MM.
        lease: crate::network_replay::NetworkAcceptLeaseId,
        /// Original numeric listener argument, checked at the kernel boundary.
        fd: i32,
        /// Original accept4 flags; never inferred from a typed bit mask.
        flags: i32,
    },
    /// Retain the original completed/partial kernel effect, including owner exit.
    CollectAcceptedEffect {
        /// Original submitted receipt, retained even after owner abandonment.
        lease: crate::network_replay::NetworkAcceptLeaseId,
    },
    /// Synchronously latch the actual return and acquire controller custody before continuation.
    CaptureAcceptedReturn {
        /// Previously submitted receipt, qualified by RPC owner and MM.
        lease: crate::network_replay::NetworkAcceptLeaseId,
        /// Original physical return; errors do not imply absence of dequeue.
        kernel_result: Result<i32, i32>,
    },
    /// Match an already captured accepted pin to its provider creation occurrence.
    ResolveAcceptedProvider {
        /// Exact submitted return/custody receipt, authenticated by owner/MM.
        lease: crate::network_replay::NetworkAcceptLeaseId,
    },
    /// Cancel only before physical submission, without consuming a connection.
    CancelAcceptedSocket {
        /// Exact owned accept receipt.
        lease: crate::network_replay::NetworkAcceptLeaseId,
    },
    /// Compare the kernel return with already-confirmed installation authority.
    CompleteAcceptedSocket {
        /// Exact owned accept receipt.
        lease: crate::network_replay::NetworkAcceptLeaseId,
        /// Actual returned FD or positive errno; this is not a slot certificate.
        kernel_result: Result<i32, i32>,
        /// Local metadata claim, compared with the service's exact fact before mutation.
        installed_open_file: Option<OpenFileId>,
    },
    /// Obtain a bound accepted endpoint without placeholder host queries.
    AcceptedEndpoint {
        /// Exact enrolled open file.
        open_file: OpenFileId,
        /// Peer when true, bound local endpoint otherwise.
        peer: bool,
    },
    /// Read-only normalized setter result under exact control exclusion.
    PreviewSocketOption {
        /// Exact owned short control.
        lease: NetworkStreamLeaseId,
        /// Raw guest option after ABI validation.
        option: NetworkStreamSocketOption,
    },
    /// Inspect actual wait entry while retaining its arrival-generation receipt.
    InspectZeroStreamWait {
        /// Exact wait owned by the authenticated active call.
        id: NetworkZeroStreamWaitId,
    },
    /// Resolve a prepared wait only if the scheduler never entered it.
    CancelZeroStreamWait {
        /// Exact wait owned by the authenticated active call.
        id: NetworkZeroStreamWaitId,
    },
    /// Check actual engine support before either enrolled or legacy Shutdown.
    /// This read-only check grants no native effect or versioned completion.
    PreflightSocketShutdown,
}

/// Live stream input normalized before entering the capture engine.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkCapturedStreamInput {
    /// Newly observed bytes.
    Bytes(Vec<u8>),
    /// Peer write-side shutdown.
    EndOfFile,
    /// Positive Linux errno.
    Error(i32),
    /// Completion of an outbound connect attempt.
    Connect(NetworkConnectionResultV2),
}

/// Guest stream output normalized before entering the capture engine.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkCapturedStreamOutput {
    /// Successfully transmitted bytes.
    Bytes(Vec<u8>),
    /// Positive Linux errno.
    Error(i32),
    /// Local shutdown transition.
    Shutdown(NetworkShutdownV2),
}

/// Serializable result of one replayed stream receive attempt.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkStreamReceive {
    /// Bytes removed from currently available stream input.
    Bytes(Vec<u8>),
    /// Peer write shutdown after all preceding bytes were consumed.
    EndOfFile,
    /// Exact recorded Linux errno.
    Error(i32),
    /// A nonblocking attempt has no current availability.
    WouldBlock,
    /// A blocking attempt must register a scheduler wait.
    Pending,
}

/// Serializable result of validating one stream transmit.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkStreamTransmit {
    /// Byte count accepted before an error or trace boundary.
    Accepted(usize),
    /// Exact recorded Linux errno at current outbound progress.
    Error(i32),
}

/// Serializable released connection-control observation.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkConnection {
    /// Completion of an outbound connect.
    Connect(NetworkConnectionResultV2),
    /// Completion of an accept with a new trace channel.
    Accept {
        /// Trace identity of the accepted socket.
        accepted: NetworkChannelId,
        /// Peer address returned to guest memory.
        peer: Option<NetworkAddressV2>,
        /// Creation-time ancillary relocation metadata.
        ancillary: Option<NetworkAncillaryDataV2>,
    },
    /// Exact recorded error from an accept attempt.
    Error(i32),
}

/// Response vocabulary for the normalized network engine RPC.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum NetworkReply {
    /// Shared publication's installed generation, or an actual Socket error.
    OriginalSocketInstallation(Option<crate::types::FdSlotBinding>),
    /// Shared publication's epoll generation, or its actual original error.
    OriginalEpollInstallation(Option<crate::types::FdSlotBinding>),
    /// Shared original Openat publication, including actual errno or installed generation.
    OriginalOpenatInstallation(crate::network_runtime::original_installation::OpenatPublication),
    /// Exact global descriptor installation publication receipt.
    FdPublication(crate::network_replay::NetworkFdPublicationReply),
    /// Exact physical descriptor mutation response.
    FdMutation(crate::network_replay::NetworkFdMutationReply),
    /// Mutation completed without a value.
    Unit,
    /// Optional channel from lookup or retirement.
    Channel(Option<NetworkChannelId>),
    /// Channels whose replay observations became eligible.
    ReadyChannels(Vec<NetworkChannelId>),
    /// Stream receive outcome.
    StreamReceive(NetworkStreamReceive),
    /// Stream transmit outcome.
    StreamTransmit(NetworkStreamTransmit),
    /// Optional connect or accept observation.
    Connection(Option<NetworkConnection>),
    /// Current modeled readiness bits.
    Readiness(NetworkReadinessV2),
    /// Durable physical receive receipt.
    IngressLease(NetworkStreamLeaseId),
    /// Current queue facts, including reservation availability.
    StreamQueueStatus(NetworkStreamQueueStatus),
    /// Bounded delivery receipt or no currently available outcome.
    StreamChunk(NetworkStreamChunk),
    /// At most the bounded view limit from an existing reservation.
    StreamChunkView(Vec<u8>),
    /// Whether this engine was explicitly created/decoded as V3.
    ShadowMode(bool),
    /// Actual profile state, with None only for an unenrolled OFD.
    StreamSocketState(Option<NetworkStreamSocketState>),
    /// One short control with its exact admission snapshot.
    SocketControl(NetworkSocketControl),
    /// Current table/OFD read authority, or exact prior-prefix recovery.
    FdRead(crate::network_replay::NetworkFdReadBegin),
    /// Atomic sorted set of controls and their admission snapshots.
    SocketControls(Vec<(OpenFileId, NetworkSocketControl)>),
    /// The active syscall reference and its physical pin requirement.
    StreamCall(NetworkStreamCall),
    /// Existing Call admission for the actual original Connect invocation.
    OriginalConnectAdmission(crate::network_replay::original_connect::Admission),
    /// Retained final observation, authenticated against the backend raw return.
    OriginalConnectOutcome(Box<crate::network_runtime::original_connect::Outcome>),
    /// Known acquisition failure; no physical pin or semantic call survives.
    NativeStreamPinFailed(i32),
    /// Retained raw result and exact bytes from an original-OFD operation.
    NativeStreamObservation(crate::network_runtime::native_peer::Observation),
    /// Persistent nonconsuming observation receipt.
    ShadowProbe(NetworkShadowProbe),
    /// Atomic zero-receive outcome.
    ZeroStreamReceive(NetworkZeroStreamReceive),
    /// Transient scheduler wait identity.
    ZeroStreamWait(NetworkZeroStreamWaitId),
    /// Whether the scheduler admitted the logical wait.
    ZeroStreamWaitEntered(bool),

    /// Whether explicit accepted inheritance is declared.
    AcceptedMode(bool),
    /// An eligible exact child reservation, or no current connection.
    AcceptedChild(Option<crate::network_replay::NetworkAcceptReservation>),
    /// Reconciled accepted FD/OFD/channel; None for known no-connection effects.
    AcceptedCompletion(Option<crate::network_replay::NetworkAcceptedCompletion>),
    /// Exact local generation joined to the original accepted installation.
    AcceptedInstallation(crate::types::FdSlotBinding),
    /// The private original consumer has already confirmed the negative fact.
    AcceptedNoInstallation,
    /// Exact modeled endpoint; None for an unmanaged socket.
    AcceptedEndpoint(Option<NetworkAddressV2>),
    /// Normalized setter result, without committing a mutation.
    SocketOptionResult(Result<(), i32>),
}

/// Messages to the global object.
///
/// This is public only so it can be used in the `GlobalTool` trait.
/// It should NOT be used by any client outside of this file.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub enum GlobalRequest {
    /// Lock the resources
    /// Also contains the `DetPid` of the process containing the thread requesting resources.
    RequestResources(Resources, DetPid),
    ParkedRequest(Resources, DetPid, ControlCapability),
    ResumeParkedRequest {
        ticket: ResumeTicket,
        current_site: reverie::CallbackSignalSite,
    },
    FinishParkedObservation {
        wait: ContinuationId,
        lease: reverie::ParkedObservationLease,
        site: reverie::CallbackSignalSite,
        finish: ObservationFinish,
    },
    ParkedProtocolFailure(ProtocolFailure),
    SignalDequeued {
        detpid: DetPid,
        identity: reverie::SignalTaskIdentity,
        dequeue: reverie::SignalDequeue,
    },
    /// Release the locks
    ReleaseResources(Resources),
    /// For convenience, release all the resources held by the current TID.
    ReleaseAllResources,

    // TODO-HUMAN-REVIEW(PR-643): Review this new Detcore global RPC request.
    /// Add a syscall to the run-wide unsupported-use summary.
    ReportUnsupportedSyscall(String),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
    /// Save the caller, address-space identity, and logically blocking descriptors before a
    /// backend reloads its tool across exec.
    PrepareExec(DetPid, MmId, FilesId, ExecFdBlockingOverrides),

    /// Clear the saved transition after an exec attempt returns with an error.
    CancelExec(ExecFilesReceipt),

    /// Mark the initial image transition complete for backends that begin post-exec.
    MarkPastFirstExecve(Option<reverie::SignalTaskIdentity>),

    /// The parent is adding a child-thread to the round-robin pool.  Contains the dettid
    /// of the new child and it's starting scheduler priority IF it is available to the caller.
    /// The only scenario where the Priority will be missing is when we're replaying preemptions.
    /// In that case it is the global state that holds the information regarding the new thread's
    /// initial priority.
    CreateChildThread(
        DetTid,
        DetPid,
        usize,
        Option<CloneFlags>,
        libc::c_int,
        Option<(i32, i32)>,
        Option<Priority>,
    ),

    /// Retain existing wait metadata before a NoSeq physical clone.
    PrepareNoSeqBirth {
        process: DetPid,
        syscall_count: u64,
        flags: CloneFlags,
        child_tid_addr: usize,
        exit_signal: libc::c_int,
        priority_entropy: Option<u64>,
        fd_permit: Option<crate::network_replay::NetworkFdPublicationPermit>,
    },
    /// Persist submission before entering the kernel clone.
    SubmitNoSeqBirth(crate::scheduler::NoSeqChildBirth),
    /// Exact task consumption settles preparations/completed births; unknown submissions remain.
    NoSeqBirthOwnerGone(
        Option<crate::scheduler::UninvokedWaitCall>,
        Option<crate::network_replay::NetworkFdMutationAdmission>,
    ),
    /// A successful surviving parent joins the exact child registration before guest resume.
    JoinNoSeqBirth(crate::scheduler::NoSeqChildBirth, DetTid),
    /// Shared registry admission before a NoSeq native setpgid/setsid.
    PrepareProcessGroupChange(crate::scheduler::ProcessGroupChangeKind, u64),
    /// Acknowledge physical submission without changing native syscall arguments.
    SubmitProcessGroupChange(crate::scheduler::ProcessGroupChange),
    /// Record the actual native result before releasing group/birth admission.
    CompleteProcessGroupChange(crate::scheduler::ProcessGroupChange, Result<i64, i32>),
    /// Known native clone error; parent disappearance is not this event.
    CancelNoSeqBirth(crate::scheduler::NoSeqChildBirth, i32),
    /// Actual backend-bound child, independent of a surviving parent callback.
    CreateNoSeqChildThread(
        crate::scheduler::NoSeqChildBirth,
        Option<(i32, i32)>,
        Option<Priority>,
    ),

    /// A vfork child registering itself while its parent is blocked inside the
    /// kernel. Contains the (real) parent dettid and detpid, the child dettid,
    /// the child TID address, the clone flags, and the starting priority (absent
    /// only when replaying preemptions).
    CreateVforkChildThread(
        DetTid,
        DetPid,
        DetTid,
        usize,
        CloneFlags,
        libc::c_int,
        Option<Priority>,
    ),

    /// New thread is alive and waiting to run its first instruction.  Contains the dettid
    /// and detpid of the new child.
    StartNewThread(
        DetTid,
        DetPid,
        Option<(i32, i32)>,
        Option<reverie::SignalTaskIdentity>,
    ),

    /// Remove a thread from scheduler data structures, guaranteeing that it will
    /// consume no further turns. Carries its final timeslice distribution and any
    /// chaos-epoch transitions not yet flushed by a priority-change commit.
    DeregisterThread(ThreadDeregistration),

    /// Replace the address cleared and woken when the calling thread exits.
    /// A zero address disables the exit-time store and wake.
    SetChildTidAddress(usize),

    /// Notify scheduler before/after futex action.
    /// The last two arguments are the initial contents of the memory word, and the mask.
    FutexAction(DetTid, FutexAction, FutexID, i32, u32),

    /// Translate nondeterministic to deterministic inode.
    DeterminizeInode(RawInode),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    /// Translate a nondeterministic (host-assigned) device number to a
    /// deterministic one.
    DeterminizeDevice(u64),

    /// Translate a host-assigned fdinfo mount ID to a run-local identity. The
    /// fallback order is populated only by low-level callers without captured
    /// namespace provenance.
    DeterminizeMountId(u64, Option<Vec<u64>>),

    /// Seed or validate the exact mountinfo row/parent identity order.
    ValidateMountIdOrder(Vec<u64>),

    /// unlink an inode
    UnlinkInode(DetInode),

    /// Bump mtime
    TouchFile(RawInode),

    /// Retrieve global time.
    GlobalTimeLowerBound,

    /// Run one normalized operation against the shared network engine.
    Network(NetworkRequest),

    /// Record scheduling event in a total order.
    // Logging provenance only; never serialized into a schedule artifact.
    TraceSchedEvent(SchedEvent, DetPid, bool),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Basically performs an alarm syscall, takes a logical duration.
    RegisterAlarm(DetPid, DetTid, LogicalTime, LogicalTime, SigWrapper),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Register, re-arm, or disarm one POSIX timer.
    RegisterPosixTimer(
        DetPid,
        DetTid,
        i32,
        Option<LogicalTime>,
        LogicalTime,
        SigWrapper,
    ),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
    /// Return the logical time remaining on a process's one-shot alarm.
    AlarmRemaining(DetPid),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Deterministically select a logically exited matching child.
    ReadyChildWait(DetPid, ChildWaitSpec),
    /// Retire a consumed terminal child wait status.
    ConsumeChildWait(DetPid, DetPid),
    /// Query scheduler-owned process-group membership.
    ProcessGroup(DetPid),
    /// Apply a successful setpgid transition.
    SetProcessGroup(DetPid, DetPid),
    /// Apply a successful setsid transition.
    CreateSession(DetPid),
    /// Query live threads before translating process-directed signal delivery.
    ResolveKillTargets(DetPid),
    /// A successful kill(2) queued a physical signal for this sole target.
    NotifySignalPending(DetTid, SigWrapper, Option<DetPid>),
    /// Liveness of one tid, leader or not; see [`thread_is_live`].
    ThreadIsLive(DetTid),
    /// Scheduler-owned lifecycle state for a direct child process.
    ExactChildWaitState(DetPid, DetPid),

    /// The container is shutting down.  Exit the scheduler "thread".
    UnrecoverableShutdown,

    // Request a port for an open file description.
    RequestPort(OpenFileId),

    // Add a port to the used-port list for an open file description.
    AddUsedPort(u16, OpenFileId),

    // Release the port when the last alias of its open file description closes.
    ReleasePort(OpenFileId),

    /// Deliver robust-futex wakes collected before exit after the backend has
    /// confirmed that Linux's physical task cleanup completed.
    RobustListWakes(Vec<(DetTid, FutexID)>),

    /// Publish the descriptor snapshot taken after exact exec admission.
    UpdateExecFdBlocking(ExecFilesReceipt, ExecFdBlockingOverrides),
    /// Actual consumed ThreadState, including exact known-uninvoked custody.
    OriginalConnectOwnerGone(crate::network_replay::original_connect::Local),
    /// Backend-owned exit cleanup, independent of cfgseq scheduler registration.
    NetworkOwnerGone,
    /// Register actual ptrace task custody independently of scheduler signal identity.
    RegisterNetworkPhysicalTask {
        process: i32,
        thread: i32,
        initial_exec: bool,
    },
    CompleteNetworkInitialTable {
        ticket: crate::network_runtime::InitialTableTicket,
        register_read_succeeded: bool,
    },
    AdmitNetworkInitialTable(crate::network_runtime::InitialTableClaim),
    /// Arm exactly the already submitted clone permit before native invocation.
    PrepareNetworkNativeBirth(crate::network_replay::NetworkFdPublicationPermit, i32),
    /// Compare the actual original native return with the retained provider.
    CollectNetworkNativeBirth(
        crate::network_replay::NetworkFdPublicationPermit,
        Result<i64, i32>,
    ),
}

/// Responses from the global object
#[allow(missing_docs, clippy::unit_arg)]
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum GlobalResponse {
    /// The scheduler permanently removed this raw TID. Guest-side RPC handling retires
    /// this task without cancelling a live group-exit issuer or resuming the operation.
    ThreadExited,
    RequestResources(ResumeStatus),
    ParkedRequest(ResourceReply),
    ResumeParkedRequest(ResourceReply),
    FinishParkedObservation(Result<FinishAck, ProtocolFailure>),
    SignalDequeued {
        ack: Result<DequeueAck, TimerFailure>,
        terminal: bool,
    },
    ReleaseResources(()),
    ReleaseAllResources(()),
    // TODO-HUMAN-REVIEW(PR-643): Review this new Detcore global RPC response.
    ReportUnsupportedSyscall(()),
    PrepareExec(ExecFilesReceipt),
    CancelExec(()),
    MarkPastFirstExecve(ExecFdBlockingOverrides, Option<ExecFilesReceipt>),
    CreateChildThread(Option<(MmId, ExecFilesReceipt)>),
    PrepareNoSeqBirth(Option<crate::scheduler::NoSeqChildBirth>),
    NetworkNativeBirthPrepared(Result<(), String>),
    NetworkNativeBirthCollected(Result<(), String>),
    CancelNoSeqBirth(bool),
    NoSeqBirthOwnerGone(bool),
    JoinNoSeqBirth(bool),
    ProcessGroupChange(Option<crate::scheduler::ProcessGroupChange>),
    CompleteProcessGroupChange(bool),
    /// Includes optional preemption points for the new thread.
    StartNewThread(Option<ThreadHistory>),
    DeregisterThread(()),
    SetChildTidAddress(()),
    FutexAction(Option<SchedValue>),
    /// Return the mtime as well:
    DeterminizeInode((DetInode, LogicalTime)),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    DeterminizeDevice(u64),
    DeterminizeMountId(Option<u64>),
    ValidateMountIdOrder(bool),
    UnlinkInode(()),
    TouchFile(()),
    GlobalTimeLowerBound(LogicalTime),
    Network(Result<NetworkReply, NetworkRpcError>),
    TraceSchedEvent(TraceSchedEventResponse),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    RegisterAlarm((LogicalTime, LogicalTime)),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    RegisterPosixTimer(()),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
    ReadyChildWait((Option<DetPid>, bool)),
    ConsumeChildWait(bool),
    ProcessGroup(Option<DetPid>),
    SetProcessGroup(bool),
    CreateSession(bool),
    AlarmRemaining(ItimerSnapshot),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    ResolveKillTargets(Vec<DetTid>),
    NotifySignalPending(()),
    ThreadIsLive(bool),
    ExactChildWaitState(ExactChildWaitState),
    // TODO: use void_send_rpc, and remove this bogus response:
    UnrecoverableShutdown(()),

    RequestPort(u16),
    AddUsedPort,
    ReleasePort(Option<u16>),
    PortFull,
    RobustListWakes(Vec<u64>),
    UpdateExecFdBlocking(bool),
    /// Exact owner was marked terminal without acknowledging possible effects.
    OriginalConnectOwnerGone(bool),
    NetworkOwnerGone,
    RegisterNetworkPhysicalTask(Result<bool, String>),
    NetworkInitialTablePrepared(crate::network_runtime::InitialTableTicket),
    NetworkInitialTableCollected(
        Result<
            Option<(
                crate::network_runtime::InitialTableView,
                Vec<crate::network_runtime::InitialFileStat>,
            )>,
            String,
        >,
    ),
    NetworkInitialTableAdmitted(Result<(), String>),
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the shared warning formatter API.
/// Formats one deterministic warning for a set of unsupported syscall names.
pub fn format_unsupported_syscall_warning(syscalls: &BTreeSet<String>) -> Option<String> {
    if syscalls.is_empty() {
        None
    } else {
        Some(format!(
            "syscalls {} used but not yet supported",
            syscalls.iter().cloned().collect::<Vec<_>>().join(",")
        ))
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
/// Notifies the coordinator that `guest` is about to `execve`, recording the
/// pre-exec address space `mm` and any file-descriptor blocking overrides. A
/// backend that handles `execve` outside Detcore's syscall handler must call
/// this before the native syscall so the next image reconnects to the existing
/// scheduler identity and logical clock.
pub async fn prepare_exec<G, T>(
    guest: &mut G,
    mm: MmId,
    fd_blocking: ExecFdBlockingOverrides,
) -> ExecFilesReceipt
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    assert!(
        guest.thread_state().pending_exec_files.is_none(),
        "exec already prepared locally"
    );
    let old_files = guest.thread_state().file_metadata.lock().unwrap().files_id;
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::PrepareExec(detpid, mm, old_files, fd_blocking),
    )
    .await;
    let GlobalResponse::PrepareExec(receipt) = response else {
        unreachable!()
    };
    assert_eq!(
        (receipt.process, receipt.mm, receipt.old_files),
        (detpid, mm, old_files)
    );
    guest.thread_state_mut().pending_exec_files = Some(receipt);
    receipt
}

pub(crate) async fn update_exec_fd_blocking<G, T>(
    guest: &mut G,
    receipt: ExecFilesReceipt,
    overrides: ExecFdBlockingOverrides,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert_eq!(guest.thread_state().pending_exec_files, Some(receipt));
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::UpdateExecFdBlocking(receipt, overrides),
    )
    .await;
    assert_eq!(
        response,
        GlobalResponse::UpdateExecFdBlocking(true),
        "exec override publication lost its exact reservation"
    );
}

pub async fn cancel_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let receipt = guest
        .thread_state()
        .pending_exec_files
        .expect("exec cancellation must name its preparation");
    let (_, response) = send_and_update_time(guest, GlobalRequest::CancelExec(receipt)).await;
    assert_eq!(response, GlobalResponse::CancelExec(()));
    assert_eq!(
        guest.thread_state_mut().pending_exec_files.take(),
        Some(receipt)
    );
}

pub async fn mark_past_first_execve<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let signal_identity = guest
        .config()
        .kvm_shared_dequeue_timers
        .then(|| guest.signal_task_identity())
        .flatten();
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::MarkPastFirstExecve(signal_identity)).await;
    let overrides = match response {
        GlobalResponse::MarkPastFirstExecve(overrides, files) => {
            if let Some(files) = files {
                guest.thread_state_mut().finish_exec_files(files);
            }
            overrides
        }
        _ => unreachable!(),
    };
    if guest.config().kvm_shared_dequeue_timers {
        guest.thread_state_mut().signal_task_identity = signal_identity;
    }
    if !overrides.is_empty() {
        let dettid = guest.thread_state().dettid;
        let metadata = Arc::clone(&guest.thread_state().file_metadata);
        metadata
            .lock()
            .unwrap()
            .apply_exec_blocking_overrides(dettid, overrides);
    }
}

// TODO-HUMAN-REVIEW(PR-643): Review the guest-to-global unsupported-syscall report path.
pub async fn report_unsupported_syscall<G, T>(guest: &mut G, sysno: Sysno)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::ReportUnsupportedSyscall(sysno.to_string()),
    )
    .await;
    assert_eq!(response, GlobalResponse::ReportUnsupportedSyscall(()));
}

/// Mirrors a successful `set_tid_address(2)` into scheduler-owned exit state.
pub(crate) async fn set_child_tid_address<G, T>(guest: &mut G, address: usize)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::SetChildTidAddress(address)).await;
    assert_eq!(response, GlobalResponse::SetChildTidAddress(()));
}

pub async fn send_and_update_time<G, T>(
    guest: &mut G,
    request: GlobalRequest,
) -> (Option<LogicalTime>, GlobalResponse)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mytime = guest.thread_state().thread_logical_time.clone();
    let mm = guest.thread_state().mm_id;
    let resp = guest.send_rpc((mytime, mm, request)).await;
    if resp.1 == GlobalResponse::ThreadExited {
        let dettid = guest.thread_state().dettid;
        trace!(
            "[detcore, dtid {}] exiting after terminal scheduler cancellation",
            dettid
        );
        // This exact task must never resume, but a sibling may still own the
        // winning exit-group commit. Retire only this task; explicit backend
        // cancellation could cancel that issuer or lose its later status.
        guest.retire_current_thread().await
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review applying the coordinator clock after exec reload.
    if let Some(time) = resp.0 {
        guest
            .thread_state_mut()
            .thread_logical_time
            .advance_to(time);
    }
    resp
}

/// Register actual callback task identity even when cfgseq is disabled. An
/// ordinary run with no owned startup runtime returns false and opens no pidfd.
pub(crate) async fn register_network_physical_task<G, T>(
    guest: &mut G,
    initial_exec: bool,
) -> Result<bool, reverie::Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let before_capture = if initial_exec {
        Some(
            guest
                .thread_state()
                .file_metadata
                .lock()
                .unwrap()
                .initial_census_fence()?,
        )
    } else {
        None
    };
    let request = GlobalRequest::RegisterNetworkPhysicalTask {
        process: guest.pid().as_raw(),
        thread: guest.tid().as_raw(),
        initial_exec,
    };
    match send_and_update_time(guest, request).await.1 {
        GlobalResponse::RegisterNetworkPhysicalTask(result) => {
            result.map_err(|message| reverie::Error::Tool(anyhow::anyhow!(message)))
        }
        GlobalResponse::NetworkInitialTablePrepared(ticket) => {
            // Guest::regs returns the register value directly. The ptrace
            // backend performs the original GETREGSET and aborts this task on
            // any read/output error; an abort never reaches this completion
            // RPC. Its retained prepared command therefore remains unresolved.
            let _original_registers: libc::user_regs_struct = guest.regs().await;
            let collected = send_and_update_time(
                guest,
                GlobalRequest::CompleteNetworkInitialTable {
                    ticket,
                    register_read_succeeded: true,
                },
            )
            .await
            .1;
            match collected {
                GlobalResponse::NetworkInitialTableCollected(result) => {
                    let view =
                        result.map_err(|message| reverie::Error::Tool(anyhow::anyhow!(message)))?;
                    if let Some((view, metadata)) = view {
                        if !initial_exec {
                            return Err(reverie::Error::Tool(anyhow::anyhow!(
                                "initial census reply arrived outside the initial EXEC callback"
                            )));
                        }
                        let state = guest.thread_state();
                        let expected_owner = NetworkStreamOwner {
                            thread: state.dettid,
                            mm: state.mm_id,
                        };
                        let table = state.file_metadata.clone();
                        let (candidate, claim, fence) = table
                            .lock()
                            .unwrap()
                            .prepare_initial_census(expected_owner, view, metadata)?;
                        if before_capture.as_ref() != Some(&fence) {
                            return Err(reverie::Error::Tool(anyhow::anyhow!(
                                "initial metadata changed during native stat capture"
                            )));
                        }
                        // A lost response retains this exact full stat/census claim.
                        // A retry with different observations is refused by custody.
                        let response = send_and_update_time(
                            guest,
                            GlobalRequest::AdmitNetworkInitialTable(claim),
                        )
                        .await
                        .1;
                        match response {
                            GlobalResponse::NetworkInitialTableAdmitted(result) => {
                                result.map_err(|message| {
                                    reverie::Error::Tool(anyhow::anyhow!(message))
                                })?;
                                table
                                    .lock()
                                    .unwrap()
                                    .commit_initial_census(candidate, fence)?;
                            }
                            _ => {
                                return Err(reverie::Error::Tool(anyhow::anyhow!(
                                    "unexpected initial semantic admission"
                                )));
                            }
                        }
                    }
                    Ok(true)
                }
                _ => Err(reverie::Error::Tool(anyhow::anyhow!(
                    "unexpected initial census completion"
                ))),
            }
        }
        _ => Err(reverie::Error::Tool(anyhow::anyhow!(
            "unexpected custody registration reply"
        ))),
    }
}

/// Send one engine-owned network operation through the single global path.
/// An unexpected response is an internal protocol failure, never permission to
/// fall back to a live syscall.
pub async fn network_request<G, T>(
    guest: &mut G,
    request: NetworkRequest,
) -> Result<NetworkReply, NetworkRpcError>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::Network(request))
        .await
        .1
    {
        GlobalResponse::Network(result) => result,
        response => Err(NetworkRpcError::internal(format!(
            "network engine RPC returned unexpected response {response:?}"
        ))),
    }
}

/// When the thread resumes after a potentially-blocking scheduler request, is it a normal
/// continuation of execution, or is it because the thread will now execute a signal handler.
/// If the latter, that interrupts logically blocking syscalls that were in progress.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum ResumeStatus {
    Normal,
    Signaled(Option<Vec<SigWrapper>>),
}

/// Internal result of a scheduler operation. Terminal results become
/// [`GlobalResponse::ThreadExited`] before returning to the guest-side RPC helper.
#[derive(PartialEq, Debug, Eq, Clone)]
enum SchedulerRpcResult<T> {
    Continue(T),
    ThreadExited,
}

/// Global method RPC to request to control a resource.
///
/// Blocking: future returns only when resources are fully acquired.
pub async fn resource_request<G, T>(guest: &mut G, r: Resources) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().sequentialize_threads {
        if let Some(lease) = guest.signal_observation_lease() {
            if let Some(site) = guest.parked_signal_site() {
                return parked::capable_resource_request(
                    guest,
                    r,
                    ControlCapability::PublishOnly { lease, site },
                )
                .await;
            }
            parked::terminate_protocol(guest, ProtocolFailure::Identity).await;
        }
        let dettid = guest.thread_state().dettid;
        let detpid = guest.thread_state().detpid.expect("detpid unset");
        trace!(
            "[detcore, dtid {}] BLOCKING on resource_request rpc... {:?}",
            &dettid, r
        );
        let resp =
            send_and_update_time(guest, GlobalRequest::RequestResources(r.clone(), detpid)).await;
        match resp.1 {
            GlobalResponse::RequestResources(x) => {
                trace!(
                    "[detcore, dtid {}] UNBLOCKED, acquired resources: {:?}",
                    &dettid, r
                );
                x
            }
            _ => unreachable!(),
        }
    } else {
        ResumeStatus::Normal
    }
}

/// Carry the same resource request through its existing response transport,
/// returning the reader fixed by the selected grant. This does not add a turn.
pub(crate) async fn fd_read_resource_request<G, T>(
    guest: &mut G,
    resources: Resources,
) -> ResourceReply
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let capability = if let Some(lease) = guest.signal_observation_lease() {
        match guest.parked_signal_site() {
            Some(site) => ControlCapability::PublishOnly { lease, site },
            None => parked::terminate_protocol(guest, ProtocolFailure::Identity).await,
        }
    } else {
        ControlCapability::None
    };
    parked::capable_resource_reply(guest, resources, capability).await
}

/// Global method RPC to release all held resources.
///
/// Nonblocking: future may return immediately before the central global object has
/// processed the resource release.
pub async fn resource_release_all<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().sequentialize_threads {
        let resp = send_and_update_time(guest, GlobalRequest::ReleaseAllResources).await;
        match resp.1 {
            GlobalResponse::ReleaseAllResources(x) => x,
            _ => unreachable!(),
        }
    }
}

/// Global method RPC to allow a new thread to begin execution, called from the child thread.
///
/// Blocking: future returns only when the thread execution is truly ready to proceed.
///
/// Returns: a history of the thread preemptions, for it to play back when --replay-preemptions-from
/// is used.
pub async fn thread_start_request<G, T>(
    cfg: &Config,
    guest: &mut G,
    detpid: DetPid,
) -> Option<ThreadHistory>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    if cfg.sequentialize_threads {
        trace!("[detcore, dtid {}] new thread BLOCKING on rpc...", &dettid);
        let physical_ids = guest
            .thread_state()
            .physical_tid
            .map(|tid| (guest.pid().as_raw(), tid));
        let signal_identity = guest.signal_task_identity();
        guest.thread_state_mut().signal_task_identity = signal_identity;
        let resp = send_and_update_time(
            guest,
            GlobalRequest::StartNewThread(dettid, detpid, physical_ids, signal_identity),
        )
        .await;
        match resp.1 {
            GlobalResponse::StartNewThread(preempts) => {
                trace!("[detcore, dtid {}] new thread UNBLOCKED (post-rpc)", dettid);
                preempts
            }
            _ => unreachable!(),
        }
    } else {
        None
    }
}

/// Keep only the clone pointer that requests an exit-time clear and wake.
/// SETTID alone requests a birth-time store and must not register an exit wake.
pub(crate) fn child_tid_clear_address(flags: CloneFlags, address: usize) -> usize {
    if flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
        address
    } else {
        0
    }
}

/// Global method RPC for the parent to add a child-thread to the round-robin pool.
///
/// Nonblocking: future returning does not guarantee anything about the central scheduler,
/// except that it will eventually give a slot to the child.  Then the protocol is that
/// child will subsequently make a `thread_start_request` to gate the start of its execution.
pub async fn create_child_thread<G, T>(
    guest: &mut G,
    child_dettid: DetTid,
    ctid: usize,
    flags: Option<CloneFlags>,
    exit_signal: libc::c_int,
    physical_ids: Option<(i32, i32)>,
) -> Option<(MmId, ExecFilesReceipt)>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Random (or replayed) starting priority in chaos mode, constant priority otherwise.
    let starting_priority = if guest.config().replay_preemptions_from.is_some() {
        // In preemption replay mode, the initial priority is set on the other
        // side of the rpc, in recv_create_child_thread.
        None
    } else if guest.config().replay_schedule_from.is_some() {
        // FIXME!  Find a cleaner way to make the root thread start off high-priority:
        if child_dettid <= DetTid::from_raw(3) {
            Some(REPLAY_FOREGROUND_PRIORITY)
        } else {
            Some(REPLAY_DEFERRED_PRIORITY)
        }
    } else if guest.config().chaos {
        let entropy = guest
            .thread_state_mut()
            .chaos_prng_next_u64("child_priority");
        if guest.config().chaos_target_races {
            // Targeted chaos: bias a freshly created child to an extreme priority
            // so it either runs before the parent resumes or strictly after it,
            // instead of landing at a uniformly random priority. This maximizes
            // parent/child ordering divergence to surface fork/exec races.
            // Reproducible under `--fuzz-seed`/`--sched-seed`.
            if entropy.is_multiple_of(2) {
                Some(FIRST_PRIORITY)
            } else {
                Some(LAST_PRIORITY)
            }
        } else {
            Some(entropy_to_priority(entropy))
        }
    } else {
        Some(DEFAULT_PRIORITY)
    };

    let detpid = guest.thread_state().detpid.expect("detpid unset");

    let resp = send_and_update_time(
        guest,
        GlobalRequest::CreateChildThread(
            child_dettid,
            detpid,
            ctid,
            flags,
            exit_signal,
            physical_ids,
            starting_priority,
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::CreateChildThread(x) => x,
        _ => unreachable!(),
    }
}

pub(crate) async fn prepare_network_native_birth<G, T>(
    guest: &mut G,
    permit: crate::network_replay::NetworkFdPublicationPermit,
    syscall: i32,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(
        guest,
        GlobalRequest::PrepareNetworkNativeBirth(permit, syscall),
    )
    .await
    .1
    {
        GlobalResponse::NetworkNativeBirthPrepared(result) => {
            result.map_err(|e| Error::Tool(anyhow::anyhow!(e)))
        }
        _ => Err(Error::Tool(anyhow::anyhow!(
            "native clone preparation response changed"
        ))),
    }
}
pub(crate) async fn collect_network_native_birth<G, T>(
    guest: &mut G,
    permit: crate::network_replay::NetworkFdPublicationPermit,
    returned: Result<i64, Errno>,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(
        guest,
        GlobalRequest::CollectNetworkNativeBirth(permit, returned.map_err(Errno::into_raw)),
    )
    .await
    .1
    {
        GlobalResponse::NetworkNativeBirthCollected(result) => {
            result.map_err(|e| Error::Tool(anyhow::anyhow!(e)))
        }
        _ => Err(Error::Tool(anyhow::anyhow!(
            "native clone collection response changed"
        ))),
    }
}

pub(crate) async fn prepare_no_seq_child_birth<G, T>(
    guest: &mut G,
    flags: CloneFlags,
    child_tid_addr: usize,
    exit_signal: libc::c_int,
    priority_entropy: Option<u64>,
    fd_permit: Option<crate::network_replay::NetworkFdPublicationPermit>,
) -> Result<crate::scheduler::NoSeqChildBirth, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let state = guest.thread_state();
    let request = GlobalRequest::PrepareNoSeqBirth {
        process: state.detpid.expect("clone parent process"),
        syscall_count: state.stats.syscall_count,
        flags,
        child_tid_addr,
        exit_signal,
        priority_entropy,
        fd_permit,
    };
    let GlobalResponse::PrepareNoSeqBirth(Some(birth)) =
        send_and_update_time(guest, request).await.1
    else {
        return Err(Error::Tool(anyhow::anyhow!(
            "NoSeq clone preparation lost authenticated parent state"
        )));
    };
    assert!(guest.thread_state().uninvoked_wait_call.is_none());
    guest.thread_state_mut().uninvoked_wait_call =
        Some(crate::scheduler::UninvokedWaitCall::birth(birth.clone()));
    match send_and_update_time(guest, GlobalRequest::SubmitNoSeqBirth(birth))
        .await
        .1
    {
        GlobalResponse::PrepareNoSeqBirth(Some(submitted)) => Ok(submitted),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "NoSeq clone submission was not acknowledged"
        ))),
    }
}

pub(crate) async fn join_no_seq_child_birth<G, T>(
    guest: &mut G,
    birth: crate::scheduler::NoSeqChildBirth,
    child: DetTid,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(guest, GlobalRequest::JoinNoSeqBirth(birth, child))
        .await
        .1
    {
        GlobalResponse::JoinNoSeqBirth(true) => Ok(()),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "successful native clone lost exact child-registration join"
        ))),
    }
}

pub(crate) async fn prepare_process_group_change<G, T>(
    guest: &mut G,
    kind: crate::scheduler::ProcessGroupChangeKind,
) -> Result<Option<crate::scheduler::ProcessGroupChange>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().sequentialize_threads {
        return Ok(None);
    }
    let sequence = guest.thread_state().stats.syscall_count;
    let GlobalResponse::ProcessGroupChange(Some(prepared)) = send_and_update_time(
        guest,
        GlobalRequest::PrepareProcessGroupChange(kind, sequence),
    )
    .await
    .1
    else {
        return Err(Error::Tool(anyhow::anyhow!(
            "group transition lost authenticated registry admission"
        )));
    };
    assert!(guest.thread_state().uninvoked_wait_call.is_none());
    guest.thread_state_mut().uninvoked_wait_call =
        Some(crate::scheduler::UninvokedWaitCall::group(prepared.clone()));
    match send_and_update_time(guest, GlobalRequest::SubmitProcessGroupChange(prepared))
        .await
        .1
    {
        GlobalResponse::ProcessGroupChange(Some(submitted)) => Ok(Some(submitted)),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "group transition submission was not acknowledged"
        ))),
    }
}

pub(crate) async fn complete_process_group_change<G, T>(
    guest: &mut G,
    change: crate::scheduler::ProcessGroupChange,
    result: Result<i64, Errno>,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(
        guest,
        GlobalRequest::CompleteProcessGroupChange(change, result.map_err(Errno::into_raw)),
    )
    .await
    .1
    {
        GlobalResponse::CompleteProcessGroupChange(true) => Ok(()),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "group transition could not retain native result {result:?}"
        ))),
    }
}

pub(crate) async fn cancel_no_seq_child_birth<G, T>(
    guest: &mut G,
    birth: crate::scheduler::NoSeqChildBirth,
    errno: Errno,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match send_and_update_time(
        guest,
        GlobalRequest::CancelNoSeqBirth(birth, errno.into_raw()),
    )
    .await
    .1
    {
        GlobalResponse::CancelNoSeqBirth(true) => Ok(()),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "native clone failure contradicted retained birth: {errno}"
        ))),
    }
}

pub(crate) async fn create_no_seq_child_thread<G, T>(
    guest: &mut G,
    birth: crate::scheduler::NoSeqChildBirth,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let child = guest.thread_state().dettid;
    let native = guest
        .thread_state()
        .native_construction()
        .map_err(|e| Error::Tool(anyhow::anyhow!(e)))?;
    if (guest.config().sequentialize_threads && native.is_none())
        || birth.child() != Some(child)
        || birth.parent().thread == child
    {
        return Err(Error::Tool(anyhow::anyhow!(
            "invalid backend-bound NoSeq child"
        )));
    }
    let priority = if guest.config().replay_preemptions_from.is_some() {
        None
    } else if guest.config().replay_schedule_from.is_some() {
        Some(if child <= DetTid::from_raw(3) {
            REPLAY_FOREGROUND_PRIORITY
        } else {
            REPLAY_DEFERRED_PRIORITY
        })
    } else if guest.config().chaos {
        let entropy = birth
            .priority_entropy()
            .expect("NoSeq child inherited parent entropy");
        Some(if guest.config().chaos_target_races {
            if entropy.is_multiple_of(2) {
                FIRST_PRIORITY
            } else {
                LAST_PRIORITY
            }
        } else {
            entropy_to_priority(entropy)
        })
    } else {
        Some(DEFAULT_PRIORITY)
    };
    let physical_ids = guest
        .config()
        .backend_requires_thread_directed_process_signals
        .then(|| {
            (
                guest.pid().as_raw(),
                guest
                    .thread_state()
                    .physical_tid
                    .expect("backend supplied physical thread"),
            )
        });
    match send_and_update_time(
        guest,
        GlobalRequest::CreateNoSeqChildThread(birth, physical_ids, priority),
    )
    .await
    .1
    {
        GlobalResponse::CreateChildThread(None) => Ok(()),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "NoSeq child birth was not admitted"
        ))),
    }
}

/// Register a vfork child while its parent is blocked inside `clone(2)`.
///
/// Unlike an ordinary clone, the parent cannot perform this registration
/// because the kernel has blocked it until the child execs or exits. The child
/// therefore registers itself, carrying the inherited parent identity, flags,
/// and starting priority. The starting priority is derived the same way as an
/// ordinary clone so that chaos and replay scheduling stay deterministic.
pub async fn create_vfork_child_thread<G, T>(
    guest: &mut G,
    child_dettid: DetTid,
    vfork: crate::tool_local::PendingVfork,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let starting_priority = if guest.config().replay_preemptions_from.is_some() {
        None
    } else if guest.config().replay_schedule_from.is_some() {
        Some(if child_dettid <= DetTid::from_raw(3) {
            REPLAY_FOREGROUND_PRIORITY
        } else {
            REPLAY_DEFERRED_PRIORITY
        })
    } else if guest.config().chaos {
        Some(entropy_to_priority(vfork.child_priority_entropy.expect(
            "vfork child priority entropy missing in chaos mode",
        )))
    } else {
        // POSIX vfork suspends the parent until the child execs or _exits. Give
        // the vfork child a strictly higher priority (lower number) than the
        // parent's DEFAULT_PRIORITY so the deterministic scheduler always runs
        // the child first, rather than round-robining the parent and child at
        // equal priority (which leaves fork/exec ordering nondeterministic).
        Some(DEFAULT_PRIORITY - 1)
    };

    let resp = send_and_update_time(
        guest,
        GlobalRequest::CreateVforkChildThread(
            vfork.parent_dettid,
            vfork.parent_detpid,
            child_dettid,
            vfork.child_tid_addr,
            vfork.flags,
            vfork.exit_signal,
            starting_priority,
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::CreateChildThread(_) => (),
        _ => unreachable!(),
    }
}

/// Consume this exact backend-owned task incarnation independently of scheduler
/// registration. Drop/cancellation is not a zero-effect acknowledgement.
pub(crate) async fn network_owner_gone<R>(cfg: &Config, thread_time: DetTime, mm: MmId, backend: &R)
where
    R: GlobalRPC<GlobalState>,
{
    if matches!(
        cfg.network_trace.policy,
        NetworkPolicy::Record | NetworkPolicy::Replay
    ) {
        let response = backend
            .send_rpc((thread_time, mm, GlobalRequest::NetworkOwnerGone))
            .await;
        assert_eq!(response, (None, GlobalResponse::NetworkOwnerGone));
    }
}

pub(crate) async fn original_connect_owner_gone<R>(
    thread_time: DetTime,
    mm: MmId,
    backend: &R,
    local: Option<crate::network_replay::original_connect::Local>,
) where
    R: GlobalRPC<GlobalState>,
{
    if let Some(local) = local {
        let reply = backend
            .send_rpc((
                thread_time,
                mm,
                GlobalRequest::OriginalConnectOwnerGone(local),
            ))
            .await;
        assert_eq!(
            reply,
            (None, GlobalResponse::OriginalConnectOwnerGone(true)),
            "consumed original Connect marker cannot discard unresolved custody"
        );
    }
}

/// Consume the task's scheduler registration and acknowledge final accounting.
/// NoSeq owns registrations too, even though it has no scheduling daemon.
pub(crate) async fn deregister_thread<R>(
    threads_time: DetTime,
    _cfg: &Config,
    reverie: &R,
    thread: ThreadDeregistration,
) where
    // Note, this is called from a context where we DON'T have a full, operable `Guest`.
    R: GlobalRPC<GlobalState>,
{
    let mm = thread.mm;
    let resp = reverie
        .send_rpc((threads_time, mm, GlobalRequest::DeregisterThread(thread)))
        .await;
    // We can't update the thread time here. But it's dead anyway!
    match resp.1 {
        GlobalResponse::DeregisterThread(x) => x,
        _ => unreachable!(),
    }
}

/// Consume pre-invocation custody before other exit cleanup can retire it.
pub(crate) async fn settle_no_seq_preparations<R: GlobalRPC<GlobalState>>(
    threads_time: DetTime,
    cfg: &Config,
    reverie: &R,
    mm: MmId,
    uninvoked: Option<crate::scheduler::UninvokedWaitCall>,
    uninvoked_fd_clone: Option<crate::network_replay::NetworkFdMutationAdmission>,
) {
    if !cfg.sequentialize_threads || uninvoked.is_some() || uninvoked_fd_clone.is_some() {
        let reply = reverie
            .send_rpc((
                threads_time,
                mm,
                GlobalRequest::NoSeqBirthOwnerGone(uninvoked, uninvoked_fd_clone),
            ))
            .await;
        assert_eq!(reply, (None, GlobalResponse::NoSeqBirthOwnerGone(true)));
    }
}

/// Account this physical-exit callback's own clock before it joins a staged
/// robust-list batch. The empty wake request has no scheduler effects, but its
/// ordinary RPC header is admitted and accounted before it is acknowledged.
/// A retired incarnation must not contribute to the completed-exit barrier.
pub(crate) async fn acknowledge_robust_list_exit_time<R>(
    threads_time: DetTime,
    reverie: &R,
    mm: MmId,
) -> bool
where
    R: GlobalRPC<GlobalState>,
{
    let response = reverie
        .send_rpc((threads_time, mm, GlobalRequest::RobustListWakes(Vec::new())))
        .await;
    match response.1 {
        GlobalResponse::RobustListWakes(counts) => {
            assert!(
                counts.is_empty(),
                "an empty exit-clock acknowledgement woke a waiter"
            );
            true
        }
        GlobalResponse::ThreadExited => false,
        _ => unreachable!(),
    }
}

/// Deliver owner-death wakes from an exit callback that no longer has guest
/// memory access. The callback runs only after ptrace has observed physical
/// exit, so Linux's atomic robust-list word update is already complete.
pub(crate) async fn robust_list_wakes_after_exit<R>(
    threads_time: DetTime,
    reverie: &R,
    mm: MmId,
    wakes: Vec<(DetTid, RobustListWake)>,
) -> Vec<u64>
where
    R: GlobalRPC<GlobalState>,
{
    if wakes.is_empty() {
        return Vec::new();
    }
    let response = reverie
        .send_rpc((
            threads_time,
            mm,
            GlobalRequest::RobustListWakes(
                wakes
                    .into_iter()
                    .map(|(owner, wake)| (owner, wake.futex))
                    .collect(),
            ),
        ))
        .await;
    match response.1 {
        GlobalResponse::RobustListWakes(counts) => counts,
        _ => unreachable!(),
    }
}

/// Which actions we can take before/after a futex system call.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum FutexAction {
    /// Check in before a FUTEX_WAIT, including an optional timeout.
    WaitRequest(Option<LogicalTime>),
    /// Check in after a FUTEX_WAIT
    WaitFinished,
    /// Check in before a FUTEX_WAKE, parameterized by the number of threads woken.
    WakeRequest(i32),
    /// Check in after a FUTEX_WAKE, parameterized by the number of threads woken.
    WakeFinished(i32),
}

/// Ask scheduler for permission to proceed before/after futex operation.
/// Returns true if the operation completed normally, and false if it timed out.
pub async fn futex_action<G, T>(
    guest: &mut G,
    futex_action: FutexAction,
    futexid: &FutexID,
    init_read: i32,
    mask: u32,
) -> Option<SchedValue>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(guest.config().sequentialize_threads);
    let dettid = guest.thread_state().dettid;
    let req = GlobalRequest::FutexAction(dettid, futex_action, *futexid, init_read, mask);
    trace!(
        "BLOCKING on futex_action: sending request to scheduler: {:?}",
        req
    );
    // Update local time from potentially blocking operation:
    let resp = send_and_update_time(guest, req.clone()).await;
    match resp.1 {
        GlobalResponse::FutexAction(answer) => {
            trace!("UNBLOCKING after futex_action. Request was: {:?}", req);
            answer
        }
        _ => unreachable!(),
    }
}

/// track a (possibly new) inode, by returning a deterministic inode.
/// Also return the logical mtime for the inode, though this is only
/// used if `virtualize_metadata` is set.
pub async fn determinize_inode<G, T>(guest: &mut G, inode: RawInode) -> (DetInode, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DeterminizeInode(inode)).await;
    match resp.1 {
        GlobalResponse::DeterminizeInode(x) => x,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
/// Translate a host-assigned device number (`st_dev`) to a deterministic one.
pub async fn determinize_device<G, T>(guest: &mut G, raw_device: u64) -> u64
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DeterminizeDevice(raw_device)).await;
    match resp.1 {
        GlobalResponse::DeterminizeDevice(x) => x,
        _ => unreachable!(),
    }
}

/// Translate an observed fdinfo mount ID to the shared run-local identity.
pub async fn determinize_mount_id<G, T>(
    guest: &mut G,
    raw_mount_id: u64,
    mountinfo_order: Option<Vec<u64>>,
) -> Option<u64>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(
        guest,
        GlobalRequest::DeterminizeMountId(raw_mount_id, mountinfo_order),
    )
    .await;
    match resp.1 {
        GlobalResponse::DeterminizeMountId(value) => value,
        _ => unreachable!(),
    }
}

/// Seed or validate the run-global mount-ID pool against one mountinfo snapshot.
pub async fn validate_mountinfo_identity_order<G, T>(
    guest: &mut G,
    mountinfo_order: Vec<u64>,
) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp =
        send_and_update_time(guest, GlobalRequest::ValidateMountIdOrder(mountinfo_order)).await;
    match resp.1 {
        GlobalResponse::ValidateMountIdOrder(valid) => valid,
        _ => unreachable!(),
    }
}

/// unlink a detfd, i.e. When `unlink` a file
#[allow(unused)]
pub async fn unlink_inode<G, T>(guest: &mut G, d_ino: DetInode)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::UnlinkInode(d_ino)).await;
    match resp.1 {
        GlobalResponse::UnlinkInode(x) => x,
        _ => unreachable!(),
    }
}

/// Update the modification time for a file, using its inode.
/// This will set the mtime to a coherent global-time value.
pub async fn touch_file<G, T>(guest: &mut G, inode: RawInode)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::TouchFile(inode)).await;
    match resp.1 {
        GlobalResponse::TouchFile(x) => x,
        _ => unreachable!(),
    }
}

/// Read the global clock, or at least a deterministic lower bound on it.
pub async fn global_time_lower_bound<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::GlobalTimeLowerBound).await;
    match resp.1 {
        GlobalResponse::GlobalTimeLowerBound(x) => x,
        _ => unreachable!(),
    }
}

/// Take a time observation from the current thread. This extra indirection
/// helps abstract over whether or not we need to use local or global
/// information for this.
pub async fn thread_observe_time<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    global_time_lower_bound(guest).await
}

/// Writes a structured json backtrace to a given file
fn write_backtrace<G, T>(guest: &mut G, m_path: Option<&PathBuf>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(backtrace) = guest.backtrace() {
        if let Some(path) = m_path {
            let file = File::create(path).expect("Failed to open preemption stacktrace log file");
            serde_json::to_writer(file, &backtrace.force_pretty()).unwrap();
        } else {
            eprintln!("{}", backtrace.force_pretty());
        }
    } else {
        warn!("Could not read backtrace!");
    }
}

/// Additional instructions to a guest after shed events is consumed by global tool
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct TraceSchedEventResponse {
    print_stack_strace: MaybePrintStack,
    timeslice: Option<LogicalTime>,
}

struct SchedEventForLog<'a> {
    event: &'a SchedEvent,
    command_bootstrap: bool,
}

struct CommandBootstrapInstructionPointer(NonZeroUsize);

impl std::fmt::Debug for CommandBootstrapInstructionPointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::logdiff::host_addr(self.0.get()))
    }
}

impl std::fmt::Debug for SchedEventForLog<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.command_bootstrap {
            return std::fmt::Debug::fmt(self.event, f);
        }
        f.debug_struct("SchedEvent")
            .field("dettid", &self.event.dettid)
            .field("op", &self.event.op)
            .field("count", &self.event.count)
            .field(
                "start_rip",
                &self.event.start_rip.map(CommandBootstrapInstructionPointer),
            )
            .field(
                "end_rip",
                &self.event.end_rip.map(CommandBootstrapInstructionPointer),
            )
            .field("end_time", &self.event.end_time)
            .finish()
    }
}

/// Record an event in the schedule trace, OR check the event on replay.
/// This also prints the backtrace of the schedevent, if indicated.
///
/// Arguments:
/// - tag_end_rip: read the current guest registers to fill in the `end_rip` on the event with the
///   current instruction pointer.
pub async fn trace_schedevent<G, T>(guest: &mut G, ev: SchedEvent, tag_end_rip: bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(guest.config().sequentialize_threads);

    // trace_schedevent is called AFTER the event is complete, and the rip is resting just after it.
    let ev = if tag_end_rip {
        let end_rip = if let Some(r) = ev.end_rip {
            r
        } else {
            let regs = guest.regs().await;
            NonZeroUsize::new(regs.rip.try_into().unwrap()).unwrap()
        };
        SchedEvent {
            end_rip: Some(end_rip),
            ..ev
        }
    } else {
        ev
    };

    if tracing::enabled!(tracing::Level::TRACE)
        && let Some(rip) = ev.end_rip
    {
        let rip_addr = AddrMut::<u16>::from_raw(rip.into()).unwrap();
        // These bytes are diagnostic, not part of the recorded event. A guest
        // may legitimately make its memory unreadable (PR_SET_DUMPABLE), so
        // preserve that observation without unwinding the live run owner.
        let rip_contents = guest.memory().read_value(rip_addr);
        trace!(
            "Tracing sched event, after which rip is {}, next two instruction bytes {:?}",
            rip, rip_contents
        );
    }

    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let command_bootstrap = guest.is_command_bootstrap();
    let resp = send_and_update_time(
        guest,
        GlobalRequest::TraceSchedEvent(ev, detpid, command_bootstrap),
    )
    .await;

    trace!("trace_schedevent result: {:?}", resp);
    match resp {
        (
            _,
            GlobalResponse::TraceSchedEvent(TraceSchedEventResponse {
                print_stack_strace,
                timeslice,
            }),
        ) => {
            if let Some(m_path) = print_stack_strace {
                trace!("[trace_schedevent] writing stacktrace via Reverie...");
                write_backtrace(guest, m_path.as_ref());
            }

            if let Some(timeslice) = timeslice
                && guest.thread_state().past_global_first_execve
            {
                let end_of_timeslice =
                    guest.thread_state().thread_logical_time.as_nanos() + timeslice;
                trace!(
                    "[detcore][dettid {}] setting end_of_timeslice to {:?} as instructed by replayer",
                    guest.thread_state().dettid,
                    end_of_timeslice
                );
                guest.thread_state_mut().end_of_timeslice = Some(end_of_timeslice);
                if guest.config().max_timeslice.is_some() {
                    guest.thread_state_mut().max_timeslice_end = Some(end_of_timeslice);
                }
            }
        }
        _ => {
            unreachable!()
        }
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
// TODO-HUMAN-REVIEW(#869)
/// Register an alarm (delayed signal delivery) with the global scheduler.
/// Returns the logical duration remaining until any previously scheduled alarm.
pub async fn register_alarm<G, T>(
    guest: &mut G,
    duration: LogicalTime,
    interval: LogicalTime,
    sig: Signal,
) -> (LogicalTime, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterAlarm(detpid, dettid, duration, interval, SigWrapper::from(sig)),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterAlarm(x) => x,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-841): Review logical alarm query API.
/// Return the logical duration remaining on the process's one-shot alarm.
pub async fn alarm_remaining<G, T>(guest: &mut G) -> ItimerSnapshot
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(guest, GlobalRequest::AlarmRemaining(detpid)).await;
    match resp.1 {
        GlobalResponse::AlarmRemaining(remaining) => remaining,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#869)
/// Register, re-arm, or disarm a POSIX timer with the global scheduler.
pub async fn register_posix_timer<G, T>(
    guest: &mut G,
    timer_id: i32,
    deadline: Option<LogicalTime>,
    interval: LogicalTime,
    sig: Signal,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterPosixTimer(
            detpid,
            dettid,
            timer_id,
            deadline,
            interval,
            SigWrapper::from(sig),
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterPosixTimer(()) => {}
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
/// Return the scheduler's live threads for a positive process ID.
/// Does a live thread with this tid exist, leader or not?
///
/// Distinct from [`resolve_kill_targets`], which models `kill(2)` and therefore
/// only recognises thread-group leaders. Syscalls that resolve a task through
/// `find_task_by_vpid` -- `sched_setattr` among them -- must use this instead,
/// or a non-leader thread reports ESRCH while it is plainly running.
pub async fn thread_is_live<G, T>(guest: &mut G, dettid: DetTid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ThreadIsLive(dettid)).await;
    match response.1 {
        GlobalResponse::ThreadIsLive(live) => live,
        _ => unreachable!(),
    }
}

/// Return scheduler-owned lifecycle state for an exact child-process wait.
pub async fn exact_child_wait_state<G, T>(guest: &mut G, child: DetPid) -> ExactChildWaitState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::ExactChildWaitState(parent, child)).await;
    match response.1 {
        GlobalResponse::ExactChildWaitState(state) => state,
        _ => unreachable!(),
    }
}

/// Wait without requesting a scheduler turn for a backend's physical-exit report.
pub async fn await_exact_child_physical_exit<G, T>(
    guest: &mut G,
    child: DetPid,
) -> ExactChildWaitState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut state = exact_child_wait_state(guest, child).await;
    if matches!(
        state,
        ExactChildWaitState::PhysicalExitPending | ExactChildWaitState::PhysicallyExited
    ) {
        let dettid = guest.thread_state().dettid;
        let mut resources = Resources::new(dettid);
        resources.insert(ResourceID::WaitPhysicalChild(child), Permission::R);
        resources.fyi("wait-child-physical-exit");
        let _ = resource_request(guest, resources).await;
        state = exact_child_wait_state(guest, child).await;
    }
    state
}

/// Park until an exact or any-child process wait has a logical exit to reap.
pub async fn wait_for_child_lifecycle<G, T>(guest: &mut G, spec: ChildWaitSpec) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let mut resources = Resources::new(dettid);
    resources.insert(ResourceID::WaitChild { parent, spec }, Permission::R);
    resources.fyi("wait-child-lifecycle");
    resource_request(guest, resources).await
}

pub async fn ready_child_wait<G, T>(guest: &mut G, spec: ChildWaitSpec) -> (Option<DetPid>, bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response = send_and_update_time(guest, GlobalRequest::ReadyChildWait(parent, spec)).await;
    match response.1 {
        GlobalResponse::ReadyChildWait(snapshot) => snapshot,
        _ => unreachable!(),
    }
}

pub async fn process_group<G, T>(guest: &mut G, process: DetPid) -> Option<DetPid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ProcessGroup(process)).await;
    match response.1 {
        GlobalResponse::ProcessGroup(group) => group,
        _ => unreachable!(),
    }
}

pub async fn set_process_group<G, T>(guest: &mut G, process: DetPid, group: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::SetProcessGroup(process, group)).await;
    match response.1 {
        GlobalResponse::SetProcessGroup(updated) => updated,
        _ => unreachable!(),
    }
}

pub async fn create_session<G, T>(guest: &mut G, process: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::CreateSession(process)).await;
    match response.1 {
        GlobalResponse::CreateSession(updated) => updated,
        _ => unreachable!(),
    }
}

pub async fn consume_child_wait<G, T>(guest: &mut G, child: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::ConsumeChildWait(parent, child)).await;
    match response.1 {
        GlobalResponse::ConsumeChildWait(consumed) => consumed,
        _ => unreachable!(),
    }
}

pub async fn resolve_kill_targets<G, T>(guest: &mut G, detpid: DetPid) -> Vec<DetTid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ResolveKillTargets(detpid)).await;
    match response.1 {
        GlobalResponse::ResolveKillTargets(targets) => targets,
        _ => unreachable!(),
    }
}

/// Tell the scheduler that a successful kill(2) left a physical
/// signal pending for a sole, known target.
/// The `nix` signal a timer will raise.
///
/// Timer registration still models a named signal: `setitimer`/`timer_create`
/// reach here with one, and the timer wheel stores it. Widening that is a
/// separate axis from the notification defect this change fixes, so the
/// conversion is asserted here rather than silently widened — a realtime timer
/// signal would be a NEW capability, not a regression of an existing one.
fn alarm_signal(sig: SigWrapper) -> Signal {
    sig.signal().unwrap_or_else(|| {
        panic!(
            "timer registration received unnameable signal {}",
            sig.raw()
        )
    })
}

pub async fn notify_signal_pending<G, T>(
    guest: &mut G,
    dettid: DetTid,
    signal: SigWrapper,
    target_process: Option<DetPid>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::NotifySignalPending(dettid, signal, target_process),
    )
    .await;
    match response.1 {
        GlobalResponse::NotifySignalPending(()) => {}
        _ => unreachable!(),
    }
}

/// Signal an unrecoverable error that exits the entire container.
/// Such exits are not determinizable (see "quasi-determinism").
///
/// ⚠️ `status` IS NOT A DEFAULTABLE PARAMETER, AND THAT IS THE POINT. This
/// function has four callers and they do not all mean the same thing: three are
/// fail-closed policy refusals (`HERMIT_POLICY_REFUSAL_EXIT`) and one is an
/// operator interrupt (`HERMIT_SIGINT_DEATH_EXIT`). While the status was baked in
/// here, every caller inherited whatever the last edit chose, so the SIGINT path
/// reported "hermit refused this run" for a run hermit did not refuse. Making it
/// an argument turns the meaning into a visible claim at each call site, which is
/// the same rule `scripts/check-exit-status-class.rs` enforces on the test side:
/// a status you can read is worth less than a status that says which channel
/// produced it.
///
/// Adding a caller means choosing; there is deliberately no default to fall into.
pub async fn unrecoverable_shutdown<G, T>(guest: &G, status: i32) -> !
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if cfg!(debug_assertions) {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let mm = guest.thread_state().mm_id;
        // TODO: void_send_rpc
        let _ = guest
            .send_rpc((mytime, mm, GlobalRequest::UnrecoverableShutdown))
            .await;
    }

    exit_owned_controller(status)
}

/// Shared terminal boundary for existing unrecoverable shutdown and an
/// explicitly owned controller's typed network refusal. No cleanup success is
/// claimed: exiting the PID namespace init kills its remaining guest tasks.
pub(crate) fn exit_owned_controller(status: i32) -> ! {
    // In this scenario a backtrace doesn't really help us.
    //
    // ⚠️ THE STATUS IS THE ONLY THING THAT CROSSES THIS BOUNDARY, SO IT HAS TO
    // CARRY THE MEANING. This is a DELIBERATE, policy-driven shutdown: the run
    // hit an operation it cannot service under a fail-closed configuration and
    // hermit chose to stop. The parent sees only a container child that exited,
    // and its classifier treats an exit it cannot account for as unchosen — so
    // exiting 1 here made `classify_container_result` report
    // `class=container-child-exit` and 125, i.e. "hermit broke", for a shutdown
    // that worked exactly as designed. `HERMIT_POLICY_REFUSAL_EXIT` is the
    // agreed value for "hermit refused"; see its doc for why it is not 1.
    //
    // ⚠️ AND WHICH VALUE IS THE CALLER'S TO SAY, NOT THIS FUNCTION'S. Baking one
    // in here is what made an operator's Ctrl-C report as a policy refusal: the
    // condition differs per caller and only the caller knows it.
    //
    // The RPC above is `cfg!(debug_assertions)`-only, so it cannot be the
    // signal — in a release build the parent would learn nothing.
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    #[test]
    fn schedule_event_host_markers_require_command_bootstrap_provenance() {
        let event = SchedEvent::branches(DetTid::from_raw(3), 223)
            .with_end_rip(std::num::NonZeroUsize::new(0x1234).unwrap())
            .with_time(LogicalTime::from_nanos(2230));
        let original = serde_json::to_string(&event).unwrap();
        let plain = format!(
            "{:?}",
            super::SchedEventForLog {
                event: &event,
                command_bootstrap: false
            }
        );
        assert_eq!(plain, format!("{event:?}"));
        let marked = format!(
            "{:?}",
            super::SchedEventForLog {
                event: &event,
                command_bootstrap: true
            }
        );
        assert_eq!(
            marked,
            "SchedEvent { dettid: DetPid(3), op: Branch, count: 223, start_rip: None, end_rip: Some(<hostaddr 0x1234>), end_time: Some(LogicalTime(2230)) }"
        );
        assert_eq!(serde_json::to_string(&event).unwrap(), original);
    }

    #[test]
    fn summary_preemption_views_keep_counts_full_report_and_single_flush() {
        let (_config, state, tid, _) = cancellation_test_state();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording");
        let mut writer = crate::preemptions::PreemptionWriter::new(Some(path.clone()));
        writer.register_thread(tid, DEFAULT_PRIORITY);
        writer.insert_reprioritization(tid, LogicalTime::from_nanos(100), 10, DEFAULT_PRIORITY, 20);
        let mut scheduler = state.sched.lock().unwrap();
        scheduler.preemption_writer = Some(writer);
        let (summary, info_description) = scheduler
            .generate_partial_run_summary_for_log(Some(&path))
            .unwrap();
        assert!(scheduler.preemption_writer.is_none());
        let recorded = std::fs::read(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
        assert_eq!(
            parsed["per_thread"][tid.to_string()]["prio_changes"],
            serde_json::json!([[100, DEFAULT_PRIORITY]])
        );
        let count_line = "Record of 1 preemption and reprioritization events:\n";
        assert_eq!(info_description.as_deref(), Some(count_line));
        assert_eq!(
            summary.reprio_descrip.as_deref(),
            Some(format!("{count_line}  (Writing to file {path:?})\n").as_str())
        );
        let full = summary.to_string();
        let json = serde_json::to_vec(&summary).unwrap();
        let info = summary.info(info_description.as_deref()).to_string();
        assert!(info.contains(count_line));
        assert!(!info.contains("Writing to file"));
        assert!(full.contains(&format!("Writing to file {path:?}")));
        // Formatting and another empty summary do not flush the consumed writer again.
        let _ = scheduler.generate_partial_run_summary(None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), recorded);
        assert_eq!(summary.to_string(), full);
        assert_eq!(serde_json::to_vec(&summary).unwrap(), json);
    }

    #[test]
    fn run_summary_info_keeps_semantics_and_debug_retains_bookkeeping() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<Mutex<Vec<(tracing::Level, String)>>>);
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                write!(self.0, "{}={value:?};", field.name()).unwrap();
            }
        }
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = Visitor(String::new());
                event.record(&mut visitor);
                self.0
                    .lock()
                    .unwrap()
                    .push((*event.metadata().level(), visitor.0));
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let summary = super::RunSummary {
            sched_turns: 4,
            schedevent_recorded: 11,
            schedevent_replayed: 11,
            schedevent_desynced: 2,
            desync_descrip: Some("two real desyncs\n".into()),
            num_processes: 1,
            num_threads: 1,
            threads_descrip: "[3]".into(),
            syscalls: Some(3),
            virttime_elapsed: 2_512_380,
            virttime_final: 2_512_380,
            timeslice_stats: TimesliceStats {
                count: 1,
                sum_ns: 512_380,
                min_ns: 512_380,
                max_ns: 512_380,
            },
            ..Default::default()
        };
        let description = "Record of 7 preemption and reprioritization events:\n";
        let captured = Capture(Default::default());
        tracing::subscriber::with_default(captured.clone(), || {
            super::log_run_summary(
                "report",
                &summary,
                Some(description),
                Some(std::path::Path::new("/host/recording")),
            );
        });
        let events = captured.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, tracing::Level::INFO);
        assert_eq!(events[1].0, tracing::Level::DEBUG);
        let info = &events[0].1;
        for text in [
            "1 group leaders of 1 thread(s)",
            "3 syscalls",
            "4 turns, recorded 11 events (2 desynced)",
            "two real desyncs",
            "Record of 7 preemption",
            "2_512_380ns",
            "min=512380ns max=512380ns mean=512380ns count=1",
        ] {
            assert!(info.contains(text), "missing semantic field {text}: {info}");
        }
        assert!(!info.contains("/host/recording"));
        assert!(!info.contains("replayed"));
        assert!(events[1].1.contains("replayed_events=11"));
        assert!(events[1].1.contains("/host/recording"));
        let baseline = summary.info(Some(description)).to_string();
        let mutations: [fn(&mut super::RunSummary); 8] = [
            |s| s.sched_turns += 1,
            |s| s.schedevent_recorded += 1,
            |s| s.schedevent_desynced += 1,
            |s| s.num_threads += 1,
            |s| s.syscalls = Some(4),
            |s| s.virttime_elapsed += 1,
            |s| s.timeslice_stats.count += 1,
            |s| s.threads_descrip.push_str(",4"),
        ];
        for mutate in mutations {
            let mut changed = summary.clone();
            mutate(&mut changed);
            assert_ne!(changed.info(Some(description)).to_string(), baseline);
        }
        assert_ne!(
            summary
                .info(Some(
                    "Record of 8 preemption and reprioritization events:\n"
                ))
                .to_string(),
            baseline
        );
    }

    mod backend_failure_tests;
    mod epoll_ctl_scheduling;
    mod foreground_epoll;
    mod foreground_store;
    mod native_connected;
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::task::Poll;
    use std::time::Duration;

    use detcore_model::fd::OpenFileId;
    use detcore_model::network_trace::NetworkAddressV2;
    use detcore_model::network_trace::NetworkChannelId;
    use detcore_model::network_trace::NetworkChannelV2;
    use detcore_model::network_trace::NetworkInputEventV2;
    use detcore_model::network_trace::NetworkInputKindV2;
    use detcore_model::network_trace::NetworkOutputEventV2;
    use detcore_model::network_trace::NetworkOutputKindV2;
    use detcore_model::network_trace::NetworkPolicy;
    use detcore_model::network_trace::NetworkReleaseV2;
    use detcore_model::network_trace::NetworkShutdownV2;
    use nix::sys::signal::Signal;
    use reverie::ExitStatus;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Guest;
    use reverie::Tid;
    use reverie::syscalls::CloneFlags;

    use super::ControlCapability;
    use super::FutexAction;
    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use super::MountIdPool;
    use super::NetworkChannelBinding;
    use super::NetworkIngressObservation;
    use super::NetworkReplayEngine;
    use super::NetworkReply;
    use super::NetworkRequest;
    use super::NetworkRpcError;
    use super::NetworkSocketControlFinish;
    use super::NetworkStreamLeaseId;
    use super::NetworkStreamNamespace;
    use super::NetworkStreamOwner;
    use super::NetworkStreamPhysicalEffect;
    use super::NetworkStreamPhysicalResult;
    use super::NetworkStreamQueueStatus;
    use super::NetworkStreamSocketOption;
    use super::NetworkStreamTransmit;
    use super::PendingExecState;
    use super::ResourceReply;
    use super::ResumeStatus;
    use super::RpcIncarnation;
    use super::SchedulerRpcResult;
    use super::SigWrapper;
    use super::ThreadDeregistration;
    use super::TimesliceStats;
    use super::format_unsupported_syscall_warning;
    use crate::Detcore;
    use crate::config::Config;
    use crate::config::RunsPostFork;
    use crate::ivar::Ivar;
    use crate::preemptions::PreemptionRecord;
    use crate::resources::ExternalOpId;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
    use crate::resources::Resources;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::SchedRequest;
    use crate::scheduler::SchedResponse;
    use crate::scheduler::SchedValue;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_local::ExecFdBlockingOverrides;
    use crate::types::DetPid;
    use crate::types::DetTid;
    use crate::types::DetTime;
    use crate::types::ExecFilesReceipt;
    use crate::types::FilesId;
    use crate::types::FilesIdAllocator;
    use crate::types::FutexID;
    use crate::types::LogicalTime;
    use crate::types::MmId;
    use crate::types::Op;
    use crate::types::SchedEvent;

    #[test]
    fn fdinfo_mount_ids_preserve_raw_equivalence_and_distinctness() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30], true, &[]);
        assert_eq!(pool.determinize(20, None), Some(2));

        // Unlisted nsfs, anon_inodefs, and pidfs IDs are distinct even when
        // their descriptors have the same broad Detcore FdType.
        assert_eq!(pool.determinize(700, None), Some(4));
        assert_eq!(pool.determinize(701, None), Some(5));
        assert_eq!(pool.determinize(702, None), Some(6));
        assert_eq!(pool.determinize(701, None), Some(5));
    }

    #[test]
    fn fdinfo_mount_ids_seed_from_low_level_snapshot_and_refuse_drift() {
        let mut pool = MountIdPool::from_config(&[], false, &[]);
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30])), Some(2));
        assert_eq!(pool.determinize(700, Some(&[10, 20, 30])), Some(4));
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30])), Some(2));
        assert_eq!(pool.determinize(20, Some(&[10, 99, 30])), None);
        assert_eq!(pool.determinize(20, Some(&[10, 20])), None);
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30, 40])), None);
    }

    #[test]
    fn mountinfo_reads_seed_once_and_refuse_later_table_changes() {
        let mut pool = MountIdPool::from_config(&[], false, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 20, 30]));
        assert!(pool.validate_mountinfo_order(&[10, 20, 30]));
        assert!(!pool.validate_mountinfo_order(&[10, 99, 30]));

        let mut empty = MountIdPool::from_config(&[], false, &[]);
        assert!(empty.validate_mountinfo_order(&[]));
        assert!(!empty.validate_mountinfo_order(&[10]));
    }

    #[test]
    fn configured_mountinfo_order_accepts_only_ordered_known_subsets() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 30, 40]));
        assert_eq!(pool.determinize(30, None), Some(3));
        assert!(pool.validate_mountinfo_order(&[20, 40]));
        assert!(!pool.validate_mountinfo_order(&[30, 20]));
        assert!(!pool.validate_mountinfo_order(&[10, 10]));
    }

    #[test]
    fn configured_mountinfo_order_refuses_new_namespace_ids() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 30, 40]));
        assert!(
            !pool.validate_mountinfo_order(&[10, 99, 40]),
            "a namespace-local mount ID absent from captured provenance must fail closed"
        );
    }

    #[test]
    fn fdinfo_mount_ids_refuse_malformed_configured_provenance() {
        let mut pool = MountIdPool::from_config(&[10, 10], true, &[]);
        assert_eq!(pool.determinize(10, None), None);
    }

    #[test]
    fn raw_zero_is_one_mount_identity_without_descriptor_type_partitioning() {
        let mut pool = MountIdPool::from_config(&[10, 0], true, &[]);
        // The pool API intentionally accepts no descriptor type: memfd and
        // every other Linux descriptor reporting raw mnt_id 0 share this one
        // equivalence class, even when mountinfo uses zero as an outside parent
        // ID for its root row.
        assert_eq!(pool.determinize(0, None), Some(0));
        assert_eq!(pool.determinize(20, None), Some(3));
        assert_eq!(pool.determinize(0, None), Some(0));
    }

    #[test]
    fn recorded_unlisted_mount_order_rebuilds_the_same_mapping() {
        let mut recording = MountIdPool::from_config(&[], false, &[]);
        assert!(recording.validate_mountinfo_order(&[10, 20]));
        assert_eq!(recording.determinize(700, None), Some(3));
        assert_eq!(recording.determinize(701, None), Some(4));
        let provenance = recording.provenance().unwrap().unwrap();

        let mut replay = MountIdPool::from_config(
            &provenance.mountinfo_order,
            true,
            &provenance.unlisted_order,
        );
        assert_eq!(replay.determinize(701, None), Some(4));
        assert_eq!(replay.determinize(700, None), Some(3));
    }

    fn network_refusal_state(with_input: bool, with_output: bool) -> (GlobalState, OpenFileId) {
        use detcore_model::network_trace::NetworkEndpointRoleV2;
        use detcore_model::network_trace::NetworkTraceV2;
        use detcore_model::network_trace::NetworkTransportV2;
        let mut config = Config {
            sequentialize_threads: false,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Replay;
        let channel = NetworkChannelId(1);
        let peer = NetworkAddressV2::Inet4 {
            address: [192, 0, 2, 1],
            port: 443,
        };
        let mut trace = NetworkTraceV2 {
            epoch: config.epoch,
            channels: vec![NetworkChannelV2 {
                id: channel,
                transport: NetworkTransportV2::Tcp,
                role: NetworkEndpointRoleV2::OutboundClient,
                local_address: None,
                peer_address: Some(peer.clone()),
                accepted_from: None,
            }],
            inputs: vec![],
            outputs: vec![],
        };
        if with_input {
            trace.inputs.push(NetworkInputEventV2 {
                ordinal: 0,
                channel,
                release: NetworkReleaseV2 {
                    not_before_global_time: trace.epoch_global_time().unwrap(),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"input".to_vec(),
                },
            });
        }
        if with_output {
            trace.outputs.push(NetworkOutputEventV2 {
                channel,
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"expected".to_vec(),
                },
            });
        }
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        config.network_trace_input = Some(bytes);
        let state = GlobalState::initialize(&config, false);
        let ofd = OpenFileId::new_socket(DetTid::from_raw(1), 0);
        assert_eq!(
            state.recv_network_request(NetworkRequest::EnsureChannel {
                open_file: ofd,
                binding: NetworkChannelBinding {
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::OutboundClient,
                    peer_address: Some(peer),
                    requested_local_constraint: None,
                    observed_local_address: None,
                    accepted_from: None,
                    selected_channel: None,
                },
            }),
            Ok(NetworkReply::Channel(Some(channel)))
        );
        (state, ofd)
    }

    #[tokio::test]
    async fn network_refusal_diagnostic_preserves_pending_effects_and_output() {
        use crate::network_replay::NetworkReplayError;
        let (config, state) = stream_rpc_state(false);
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(1),
            mm: MmId::initial(DetTid::from_raw(1)),
        };
        let ofd = OpenFileId::new_socket(owner.thread, 0);
        stream_rpc_bind(&state, &config, owner, ofd).await;
        let lease = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginStreamIngress { open_file: ofd },
            )
            .await,
        );
        let engine = state.network_engine.as_ref().unwrap();
        let expected =
            format!("read-only completion check: Err(UnresolvedStreamOperation({lease:?}))");
        assert_eq!(state.network_refusal_state_diagnostic(), expected);
        assert_eq!(state.network_refusal_state_diagnostic(), expected);
        assert!(
            matches!(engine.lock().unwrap().finish(), Err(NetworkReplayError::UnresolvedStreamOperation(actual)) if actual == lease)
        );
        {
            let _held = engine.lock().unwrap();
            assert!(
                state
                    .network_refusal_state_diagnostic()
                    .starts_with("network state unavailable:")
            );
        }
        assert_eq!(state.network_refusal_state_diagnostic(), expected);

        let (state, ofd) = network_refusal_state(false, true);
        let expected = "read-only completion check: Err(UnconsumedChannel(NetworkChannelId(1)))";
        assert_eq!(state.network_refusal_state_diagnostic(), expected);
        assert_eq!(state.network_refusal_state_diagnostic(), expected);
        assert_eq!(
            state.recv_network_request(NetworkRequest::TransmitStream {
                open_file: ofd,
                bytes: b"expected".to_vec(),
            }),
            Ok(NetworkReply::StreamTransmit(
                NetworkStreamTransmit::Accepted(8)
            ))
        );
        assert_eq!(
            state.network_refusal_state_diagnostic(),
            "read-only completion check: Ok(())"
        );
    }

    #[tokio::test]
    async fn network_dispatch_mismatch_is_typed_but_unknown_ofd_remains_internal() {
        use crate::network_failure::NetworkRefusalReason;
        let (state, ofd) = network_refusal_state(false, true);
        let error = state
            .recv_network_request(NetworkRequest::TransmitStream {
                open_file: ofd,
                bytes: b"different".to_vec(),
            })
            .unwrap_err();
        let GlobalResponse::Network(Err(NetworkRpcError::Refusal(refusal))) =
            serde_json::from_slice(
                &serde_json::to_vec(&GlobalResponse::Network(Err(error))).unwrap(),
            )
            .unwrap()
        else {
            panic!(
                "actual outbound comparison must preserve refusal through response serialization"
            );
        };
        assert_eq!(refusal.reason(), NetworkRefusalReason::OutboundMismatch);
        let unknown = OpenFileId::new_socket(DetTid::from_raw(1), 99);
        assert!(matches!(
            state.recv_network_request(NetworkRequest::TransmitStream {
                open_file: unknown,
                bytes: b"expected".to_vec(),
            }),
            Err(NetworkRpcError::Internal(_))
        ));
        // A rejected transmit did not consume the expected bytes.
        assert_eq!(
            state.recv_network_request(NetworkRequest::TransmitStream {
                open_file: ofd,
                bytes: b"expected".to_vec(),
            }),
            Ok(NetworkReply::StreamTransmit(
                NetworkStreamTransmit::Accepted(8)
            ))
        );
    }

    #[tokio::test]
    async fn network_cleanup_refuses_unconsumed_trace_without_publishing_summary() {
        use crate::network_failure::NetworkPolicyRefusal;
        use crate::network_failure::NetworkRefusalReason;
        for (with_input, expected) in [
            (true, NetworkRefusalReason::UnconsumedTrace),
            (false, NetworkRefusalReason::UnconsumedChannel),
        ] {
            let (state, _) = network_refusal_state(with_input, !with_input);
            let directory = tempfile::tempdir().unwrap();
            let summary = directory.path().join("summary.json");
            let error = state
                .clean_up(false, &Some(summary.clone()))
                .await
                .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<NetworkPolicyRefusal>()
                    .unwrap()
                    .reason(),
                expected
            );
            assert!(
                !summary.exists(),
                "refused completion must not publish a successful summary"
            );
        }
    }

    #[tokio::test]
    async fn network_cleanup_accepts_consumed_output_and_rejects_internal_ownership() {
        use crate::network_failure::NetworkPolicyRefusal;
        let (state, ofd) = network_refusal_state(false, true);
        assert_eq!(
            state.recv_network_request(NetworkRequest::TransmitStream {
                open_file: ofd,
                bytes: b"expected".to_vec(),
            }),
            Ok(NetworkReply::StreamTransmit(
                NetworkStreamTransmit::Accepted(8)
            ))
        );
        state.clean_up(false, &None).await.unwrap();

        let (state, _) = network_refusal_state(false, false);
        let outstanding = state.network_engine.as_ref().unwrap().clone();
        let error = state.clean_up(false, &None).await.unwrap_err();
        assert!(error.downcast_ref::<NetworkPolicyRefusal>().is_none());
        assert!(error.to_string().contains("live users at finalization"));
        drop(outstanding);

        let (_, state) = stream_rpc_state(false);
        let error = state.clean_up(false, &None).await.unwrap_err();
        assert!(error.downcast_ref::<NetworkPolicyRefusal>().is_none());
        assert!(error.to_string().contains("captured no external channels"));
    }

    #[tokio::test]
    async fn fd_publication_global_gate_waits_for_exact_owner_cleanup_and_revalidates() {
        use crate::network_replay::NetworkFdPublicationReply as P;
        use crate::network_replay::NetworkFdPublicationRequest as Q;
        let (config, state) = stream_rpc_state(false);
        let first = NetworkStreamOwner {
            thread: DetTid::from_raw(41),
            mm: MmId::initial(DetTid::from_raw(41)),
        };
        let sibling = NetworkStreamOwner {
            thread: DetTid::from_raw(42),
            mm: first.mm,
        };
        let files = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            let files = engine.fd_publication_fixture_register(first, None);
            assert_eq!(
                engine.fd_publication_fixture_register(sibling, Some(first)),
                files
            );
            files
        };
        let first_permit = match stream_rpc(
            &state,
            &config,
            first,
            NetworkRequest::FdPublication(Q::Acquire { files }),
        )
        .await
        .unwrap()
        {
            NetworkReply::FdPublication(P::Admitted(value)) => value.permit,
            reply => panic!("unexpected reply {reply:?}"),
        };
        let mut blocked = std::pin::pin!(stream_rpc(
            &state,
            &config,
            sibling,
            NetworkRequest::FdPublication(Q::Acquire { files })
        ));
        assert!(matches!(futures::poll!(blocked.as_mut()), Poll::Pending));
        // A stale incarnation's cleanup must neither release this permit nor
        // make the awakened contender runnable through an unchecked path.
        let stale = NetworkStreamOwner {
            thread: first.thread,
            mm: first.mm.for_exec(first.thread),
        };
        state.abandon_network_owners([stale]);
        assert!(matches!(futures::poll!(blocked.as_mut()), Poll::Pending));
        state.abandon_network_owners([first]);
        let successor = match blocked.await.unwrap() {
            NetworkReply::FdPublication(P::Admitted(value)) => value.permit,
            reply => panic!("unexpected successor reply {reply:?}"),
        };
        assert_ne!(successor.lease, first_permit.lease);
        assert_eq!(successor.owner, sibling);
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::FdPublication(Q::ReleaseEmpty { permit: successor })
            )
            .await,
            Ok(NetworkReply::FdPublication(P::Released))
        );
        assert!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .release_empty_fd_publication(sibling, first_permit)
                .is_err()
        );
    }

    async fn native_cleanup_common_fixture() -> (
        Config,
        GlobalState,
        NetworkStreamOwner,
        crate::network_replay::NetworkFdMutationAdmission,
        crate::scheduler::NoSeqChildBirth,
        crate::scheduler::UninvokedWaitCall,
    ) {
        use crate::network_replay::NetworkFdMutationBegin;
        use crate::network_replay::NetworkFdMutationKind;
        let (config, state, owner, files) = fd_lifecycle_exec_fixture();
        let admission = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            let NetworkFdMutationBegin::Admitted(admission) = engine
                .begin_fd_mutation(
                    owner,
                    files,
                    NetworkFdMutationKind::Clone {
                        flags: CloneFlags::empty(),
                    },
                )
                .unwrap()
            else {
                panic!("clone admission")
            };
            let admission = *admission;
            engine
                .submit_fd_mutation(owner, admission.publication.permit)
                .unwrap();
            engine
                .prepare_native_birth_escrow(owner, admission.publication.permit)
                .unwrap();
            admission
        };
        let reply = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::PrepareNoSeqBirth {
                        process: owner.thread,
                        syscall_count: 1,
                        flags: CloneFlags::empty(),
                        child_tid_addr: 0,
                        exit_signal: libc::SIGCHLD,
                        priority_entropy: None,
                        fd_permit: Some(admission.publication.permit),
                    },
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = reply.1 else {
            panic!("common birth preparation")
        };
        let marker = crate::scheduler::UninvokedWaitCall::birth(prepared.clone());
        let reply = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::SubmitNoSeqBirth(prepared),
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = reply.1 else {
            panic!("common birth submission")
        };
        (config, state, owner, admission, birth, marker)
    }

    #[tokio::test]
    async fn actual_global_uninvoked_cleanup_survives_future_drop_at_each_physical_await() {
        use crate::network_runtime::native_birth::NativeBirthCleanupPeer;
        for boundary in [
            "prepare-reply",
            "cancel-dispatch",
            "cancel-reply",
            "retirement-ack",
        ] {
            let (config, mut state, owner, admission, _birth, marker) =
                native_cleanup_common_fixture().await;
            let engine_owners = Arc::strong_count(state.network_engine.as_ref().unwrap());
            let (runtime, mut peer) =
                NativeBirthCleanupPeer::new(admission.publication.permit, false);
            state.network_runtime = Some(runtime);
            if boundary != "prepare-reply" {
                peer.prepare_reply().await;
            }
            let mut future = Box::pin(state.receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::NoSeqBirthOwnerGone(
                        Some(marker.clone()),
                        Some(admission.clone()),
                    ),
                ),
            ));
            assert!(futures::poll!(future.as_mut()).is_pending(), "{boundary}");
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_no_seq_birth_count(),
                1
            );
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .validate_uninvoked_clone_admission(&admission)
                .unwrap();
            assert_eq!(
                Arc::strong_count(state.network_engine.as_ref().unwrap()),
                engine_owners + 1
            );
            if boundary == "prepare-reply" {
                drop(future);
                peer.prepare_reply().await;
                peer.receive_finish().await;
                peer.finish_reply(false);
                peer.receive_retirement().await;
            } else {
                peer.receive_finish().await;
                if boundary == "cancel-dispatch" {
                    drop(future);
                    peer.finish_reply(false);
                    peer.receive_retirement().await;
                } else if boundary == "cancel-reply" {
                    // Actual reply is retained while the real common consumer
                    // waits for its usual scheduler mutex. Dropping Global
                    // cannot drop the driver's validated continuation.
                    let (acquired_tx, acquired_rx) = std::sync::mpsc::sync_channel(0);
                    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
                    let scheduler = state.sched.clone();
                    let holder = std::thread::spawn(move || {
                        let _scheduler = scheduler.lock().unwrap();
                        acquired_tx.send(()).unwrap();
                        // A dropped sender also releases the lock on test unwind.
                        let _ = release_rx.recv();
                    });
                    acquired_rx.recv().unwrap();
                    peer.finish_reply(false);
                    peer.wait_reply_retained().await;
                    drop(future);
                    release_tx.send(()).unwrap();
                    holder.join().unwrap();
                    peer.receive_retirement().await;
                } else {
                    peer.finish_reply(false);
                    peer.receive_retirement().await;
                    drop(future);
                }
            }
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_no_seq_birth_count(),
                0
            );
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .validate_uninvoked_wait_call(owner, &marker)
            );
            // Cancelling the clone retires the operation, not its original task.
            assert!(matches!(
                state.network_engine.as_ref().unwrap().lock().unwrap().finish_fd_mutations(),
                Err(crate::network_replay::NetworkReplayError::FdPublicationProtocol(message))
                    if message == "network OFD lifetime protocol: OutstandingOwners"
            ));
            let reply = state
                .receive_rpc(
                    Tid::from_raw(owner.thread.as_raw()),
                    (
                        DetTime::new(&config),
                        owner.mm,
                        GlobalRequest::NetworkOwnerGone,
                    ),
                )
                .await;
            assert_eq!(reply, (None, GlobalResponse::NetworkOwnerGone));
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .finish_fd_mutations()
                .unwrap();
            // Semantic completion alone cannot drop provider/callback custody.
            assert_eq!(
                Arc::strong_count(state.network_engine.as_ref().unwrap()),
                engine_owners + 1
            );
            peer.retirement_reply().await;
            assert_eq!(
                Arc::strong_count(state.network_engine.as_ref().unwrap()),
                engine_owners
            );
            peer.stop(true);
        }
    }

    #[tokio::test]
    async fn actual_global_uninvoked_cleanup_rejects_changed_admission_before_cancel() {
        use crate::network_runtime::native_birth::NativeBirthCleanupPeer;
        let (config, mut state, owner, admission, _birth, marker) =
            native_cleanup_common_fixture().await;
        let (runtime, mut peer) = NativeBirthCleanupPeer::new(admission.publication.permit, false);
        state.network_runtime = Some(runtime);
        peer.prepare_reply().await;
        let mut wrong = admission.clone();
        wrong.publication.permit.owner.mm = owner.mm.for_exec(owner.thread);
        let bad = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::NoSeqBirthOwnerGone(Some(marker.clone()), Some(wrong)),
                ),
            )
            .await;
        assert_eq!(bad.1, GlobalResponse::NoSeqBirthOwnerGone(false));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_no_seq_birth_count(),
            1
        );
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .validate_uninvoked_clone_admission(&admission)
            .unwrap();
        let mut good = Box::pin(state.receive_rpc(
            Tid::from_raw(owner.thread.as_raw()),
            (
                DetTime::new(&config),
                owner.mm,
                GlobalRequest::NoSeqBirthOwnerGone(Some(marker), Some(admission)),
            ),
        ));
        assert!(futures::poll!(good.as_mut()).is_pending());
        peer.receive_finish().await;
        peer.finish_reply(false);
        peer.receive_retirement().await;
        assert_eq!(good.await.1, GlobalResponse::NoSeqBirthOwnerGone(true));
        peer.retirement_reply().await;
        peer.stop(true);
    }

    #[tokio::test]
    async fn actual_final_wait_native_errno_owns_same_collect_and_cleanup_after_waiter_loss() {
        use reverie::Tool;
        use reverie::syscalls::SyscallInfo;

        use crate::network_runtime::native_birth::NativeBirthCleanupPeer;
        for queued in [false, true] {
            for wrong_errno in [false, true] {
                let (config, mut state, owner, admission, birth, _marker) =
                    native_cleanup_common_fixture().await;
                let (runtime, mut peer) =
                    NativeBirthCleanupPeer::new(admission.publication.permit, true);
                state.network_runtime = Some(runtime);
                peer.prepare_reply().await;
                let tool: Detcore = Detcore::new(Tid::from_raw(owner.thread.as_raw()), &config);
                let mut local = tool.init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
                local.detpid = Some(owner.thread);
                local.stats.syscall_count = 1;
                local.pending_no_seq_birth = Some(birth);
                local.pending_fd_clone = Some(admission.publication.permit);
                local.clone_flags = Some(CloneFlags::empty());
                local.thread_start_entered = true;
                let (nr, args) = reverie::syscalls::Fork::new().into_parts();
                tool.on_injected_syscall_observed(
                    Tid::from_raw(owner.thread.as_raw()),
                    &state,
                    &mut local,
                    nr,
                    args,
                    reverie::InjectedSyscallEvent::Returned(-i64::from(libc::EINVAL)),
                );
                assert!(!state.sched.lock().unwrap().backend_failed());
                if queued {
                    let mut abandoned = Box::pin(state.receive_rpc(
                        Tid::from_raw(owner.thread.as_raw()),
                        (
                            DetTime::new(&config),
                            owner.mm,
                            GlobalRequest::CollectNetworkNativeBirth(
                                admission.publication.permit,
                                Err(libc::EINVAL),
                            ),
                        ),
                    ));
                    assert!(futures::poll!(abandoned.as_mut()).is_pending());
                    drop(abandoned);
                }
                // With no Collect, final wait must submit the original one. With
                // a queued Collect and lost Global borrower, it must recover that
                // same request. Both retain the real backend errno for cleanup.
                tool.on_backend_thread_terminal(
                    Tid::from_raw(owner.thread.as_raw()),
                    &state,
                    &mut local,
                    reverie::ExitStatus::Signaled(reverie::Signal::SIGKILL, false),
                );
                assert!(!state.sched.lock().unwrap().backend_failed());
                assert_eq!(
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .thread_tree
                        .pending_no_seq_birth_count(),
                    1
                );
                assert!(
                    state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .finish_fd_mutations()
                        .is_err()
                );
                // The actual final-wait observer precedes ordinary task consumption.
                // Retire that task in both branches; failed physical cleanup must
                // remain unresolved independently of this fixture owner.
                let reply = state
                    .receive_rpc(
                        Tid::from_raw(owner.thread.as_raw()),
                        (
                            DetTime::new(&config),
                            owner.mm,
                            GlobalRequest::NetworkOwnerGone,
                        ),
                    )
                    .await;
                assert_eq!(reply, (None, GlobalResponse::NetworkOwnerGone));
                assert_eq!(
                    state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .fd_table_fixture_files(owner),
                    None
                );
                peer.receive_finish().await;
                peer.finish_reply(wrong_errno);
                if wrong_errno {
                    peer.wait_failure().await;
                    assert_eq!(
                        state
                            .sched
                            .lock()
                            .unwrap()
                            .thread_tree
                            .pending_no_seq_birth_count(),
                        1
                    );
                    assert!(
                        state
                            .network_engine
                            .as_ref()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .finish_fd_mutations()
                            .is_err()
                    );
                    assert!(matches!(
                        state.network_engine.as_ref().unwrap().lock().unwrap().finish_fd_mutations(),
                        Err(crate::network_replay::NetworkReplayError::UnresolvedStreamOperation(lease))
                            if lease == admission.publication.permit.lease
                    ));
                    peer.stop(false); // Explicit failed execution/custody, no finite-retirement claim.
                } else {
                    peer.receive_retirement().await;
                    assert_eq!(
                        state
                            .sched
                            .lock()
                            .unwrap()
                            .thread_tree
                            .pending_no_seq_birth_count(),
                        0
                    );
                    state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .finish_fd_mutations()
                        .unwrap();
                    peer.retirement_reply().await;
                    peer.stop(true);
                }
            }
        }
    }

    fn fd_lifecycle_exec_fixture() -> (Config, GlobalState, NetworkStreamOwner, FilesId) {
        let (config, state) = stream_rpc_state(false);
        let tid = DetTid::from_raw(61);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(tid, tid, true);
        install_test_registration(&state, tid, Ivar::new());
        let owner = NetworkStreamOwner {
            thread: tid,
            mm: MmId::initial(tid),
        };
        let files = FilesId::initial(tid);
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            assert!(engine.register_initial_fd_table(owner, tid).unwrap());
        }
        (config, state, owner, files)
    }
    fn custody_birth_fixture(sequentialize: bool) -> (Config, GlobalState, DetTid, MmId) {
        let config = Config {
            sequentialize_threads: sequentialize,
            runs_post_fork: RunsPostFork::Parent,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(61);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        install_test_registration(&state, parent, Ivar::new());
        state.global_time.lock().unwrap().update_global_time(
            parent,
            DetTime::new(&config).as_nanos(),
            LogicalTime::ZERO,
        );
        (config, state, parent, MmId::initial(parent))
    }

    struct CustodyWakeCount(std::sync::atomic::AtomicUsize);
    impl futures::task::ArcWake for CustodyWakeCount {
        fn wake_by_ref(this: &std::sync::Arc<Self>) {
            this.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn custody_test_waker() -> (std::sync::Arc<CustodyWakeCount>, std::task::Waker) {
        let count = std::sync::Arc::new(CustodyWakeCount(std::sync::atomic::AtomicUsize::new(0)));
        let waker = futures::task::waker(std::sync::Arc::clone(&count));
        (count, waker)
    }

    #[tokio::test]
    async fn no_seq_consuming_exit_retires_registration_and_accounts_once() {
        use reverie::Tool;
        let (config, state, owner, mm) = custody_birth_fixture(false);
        let tool: Detcore = Detcore::new(Tid::from_raw(owner.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(owner.as_raw()), None);
        thread.detpid = Some(owner);
        thread.thread_start_entered = true;
        thread.stats.syscall_count = 17;
        thread.stats.timeslice_stats.record(7);
        thread.stats.timeslice_start_ns = Some(thread.thread_logical_time.as_nanos());
        thread.thread_logical_time.add_syscall_with_cost(37);
        let final_time = thread.thread_logical_time.clone();
        let receipt = ExecFilesReceipt {
            caller: owner,
            process: owner,
            mm,
            old_files: FilesId::initial(owner),
            new_files: state
                .exec_files_allocator
                .lock()
                .unwrap()
                .allocate_exec(owner),
        };
        state.post_exec_files.lock().unwrap().insert(owner, receipt);
        state
            .post_exec_fd_blocking
            .lock()
            .unwrap()
            .insert(owner, Default::default());
        let request = state.sched.lock().unwrap().next_turns[&owner].req.clone();
        let committed = state.sched.lock().unwrap().committed_time;
        let rpc = NetworkExitRpc {
            state: &state,
            sender: owner,
        };
        tool.on_exit_thread(
            Tid::from_raw(owner.as_raw()),
            &rpc,
            thread,
            ExitStatus::Exited(0),
        )
        .await
        .unwrap();
        {
            let scheduler = state.sched.lock().unwrap();
            assert!(scheduler.next_turns.is_empty());
            assert!(scheduler.run_queue.is_empty());
            assert!(!scheduler.priorities.contains_key(&owner));
            assert!(scheduler.deregistration_was_accounted(owner));
            assert_eq!(scheduler.per_thread_syscalls.get(&owner), Some(&17));
            assert!(matches!(
                request.try_read(),
                Some(Err(crate::scheduler::ThreadExited))
            ));
            assert_eq!(scheduler.committed_time, committed);
            scheduler.assert_native_clear_tid_idle();
        }
        assert!(
            !state
                .registered_exec_mms
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(!state.post_exec_files.lock().unwrap().contains_key(&owner));
        assert!(
            !state
                .post_exec_fd_blocking
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        let clocks = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
        let final_stats = state.sched.lock().unwrap().per_thread_timeslice[&owner];
        assert_eq!(final_stats.count, 2);
        assert_eq!(final_stats.sum_ns, 44);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(owner),
            final_time.as_nanos()
        );
        let mut late_time = final_time;
        late_time.add_syscall_with_cost(99);
        let duplicate = ThreadDeregistration {
            dettid: owner,
            detpid: owner,
            mm,
            thread_start_entered: true,
            timeslice_stats: TimesliceStats::default(),
            syscall_count: 99,
            chaos_epochs: vec![],
        };
        assert_eq!(
            rpc.send_rpc((late_time, mm, GlobalRequest::DeregisterThread(duplicate)))
                .await,
            (None, GlobalResponse::DeregisterThread(()))
        );
        assert_eq!(
            serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
            clocks
        );
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&owner),
            Some(&17)
        );
        assert_eq!(
            state.sched.lock().unwrap().per_thread_timeslice.get(&owner),
            Some(&final_stats)
        );
    }

    #[tokio::test]
    async fn no_seq_stale_deregistration_keeps_current_exec_registration_and_clock() {
        let (config, state, owner, old_mm) = custody_birth_fixture(false);
        let current_mm = old_mm.for_exec(owner);
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(owner, current_mm);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner, current_mm);
        let clocks = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
        let mut stale_time = DetTime::new(&config);
        stale_time.add_syscall_with_cost(99);
        let response = state
            .receive_rpc(
                Tid::from_raw(owner.as_raw()),
                (
                    stale_time,
                    old_mm,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid: owner,
                        detpid: owner,
                        mm: old_mm,
                        thread_start_entered: true,
                        timeslice_stats: TimesliceStats::default(),
                        syscall_count: 99,
                        chaos_epochs: vec![],
                    }),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        assert_eq!(
            serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
            clocks
        );
        assert_eq!(
            state.registered_exec_mms.lock().unwrap().get(&owner),
            Some(&current_mm)
        );
        let scheduler = state.sched.lock().unwrap();
        assert!(scheduler.next_turns.contains_key(&owner));
        assert!(scheduler.run_queue.contains_tid(owner));
        assert!(!scheduler.deregistration_was_accounted(owner));
        assert!(!scheduler.per_thread_syscalls.contains_key(&owner));
    }

    #[tokio::test]
    async fn no_seq_unknown_prestart_deregistration_acknowledges_without_clock_or_admission() {
        let (config, state, parent, _) = custody_birth_fixture(false);
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let mm = MmId::initial(child);
        let clocks = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
        let mut final_time = DetTime::new(&config);
        final_time.add_syscall_with_cost(99);
        let rpc = NetworkExitRpc {
            state: &state,
            sender: child,
        };
        super::deregister_thread(
            final_time,
            &config,
            &rpc,
            ThreadDeregistration {
                dettid: child,
                detpid: child,
                mm,
                thread_start_entered: false,
                timeslice_stats: TimesliceStats::default(),
                syscall_count: 0,
                chaos_epochs: vec![],
            },
        )
        .await;
        assert_eq!(
            serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
            clocks
        );
        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_was_registered(child));
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(!scheduler.run_queue.contains_tid(child));
        assert!(!scheduler.deregistration_was_accounted(child));
        assert!(!scheduler.per_thread_syscalls.contains_key(&child));
        assert!(
            !state
                .registered_exec_mms
                .lock()
                .unwrap()
                .contains_key(&child)
        );
        assert!(scheduler.next_turns.contains_key(&parent));
    }

    #[tokio::test]
    async fn custody_birth_rpc_waits_for_actual_parent_publication_in_both_modes() {
        use std::future::Future;
        for sequentialize in [false, true] {
            for same_process in [false, true] {
                let (config, state, parent, parent_mm) = custody_birth_fixture(sequentialize);
                let child = DetTid::from_raw(62);
                let flags = if same_process {
                    CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM
                } else {
                    CloneFlags::empty()
                };
                let mm = MmId::for_clone(parent_mm, child, same_process);
                let process = if same_process { parent } else { child };
                let mut waiting = Box::pin(state.receive_rpc(
                    Tid::from_raw(child.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::RegisterNetworkPhysicalTask {
                            process: process.as_raw(),
                            thread: child.as_raw(),
                            initial_exec: false,
                        },
                    ),
                ));
                let (wakes, waker) = custody_test_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                let committed = state.sched.lock().unwrap().committed_time;
                assert!(waiting.as_mut().poll(&mut cx).is_pending());
                let child_time = state.global_time.lock().unwrap().threads_time(child);
                assert!(waiting.as_mut().poll(&mut cx).is_pending());
                assert_eq!(
                    state.global_time.lock().unwrap().threads_time(child),
                    child_time
                );
                assert_eq!(state.sched.lock().unwrap().committed_time, committed);
                assert!(!state.sched.lock().unwrap().next_turns.contains_key(&child));
                assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
                let mut publication = Box::pin(state.receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        parent_mm,
                        GlobalRequest::CreateChildThread(
                            child,
                            parent,
                            0,
                            Some(flags),
                            if same_process { 0 } else { libc::SIGCHLD },
                            None,
                            Some(DEFAULT_PRIORITY),
                        ),
                    ),
                ));
                let parent_result = futures::poll!(&mut publication);
                assert_eq!(parent_result.is_pending(), sequentialize);
                assert!(
                    wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
                    "actual birth must wake the registered waiter"
                );
                assert_eq!(
                    waiting.await.1,
                    GlobalResponse::RegisterNetworkPhysicalTask(Ok(false))
                );
                // Birth admission neither schedules this child nor opens a pidfd
                // when the run has no physical-runtime capability.
                assert!(
                    state.sched.lock().unwrap().next_turns[&child]
                        .req
                        .try_read()
                        .is_none()
                );
                assert_eq!(state.sched.lock().unwrap().committed_time, committed);
                assert!(
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .physical_thread_identity(child)
                        .is_none()
                );
                if sequentialize {
                    let turn = crate::scheduler::do_a_turn_blocking(
                        state.sched.clone(),
                        state.global_time.clone(),
                        &Err(crate::scheduler::SkipTurn),
                    )
                    .await
                    .unwrap();
                    assert_eq!(turn.tid, parent);
                    assert_eq!(publication.await.1, GlobalResponse::CreateChildThread(None));
                } else {
                    assert!(matches!(
                        parent_result,
                        Poll::Ready((_, GlobalResponse::CreateChildThread(None)))
                    ));
                }
            }
        }
    }

    #[tokio::test]
    async fn custody_birth_rpc_wakes_on_actual_physical_registration_failure() {
        use std::future::Future;
        for sequentialize in [false, true] {
            let (config, state, parent, mm) = custody_birth_fixture(sequentialize);
            let child = DetTid::from_raw(62);
            let mut waiting = Box::pin(state.receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::RegisterNetworkPhysicalTask {
                        process: parent.as_raw(),
                        thread: child.as_raw(),
                        initial_exec: false,
                    },
                ),
            ));
            let (wakes, waker) = custody_test_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(
                state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            mm,
                            GlobalRequest::CreateChildThread(
                                child,
                                parent,
                                0,
                                Some(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM),
                                0,
                                Some((i32::MAX, i32::MAX)),
                                Some(DEFAULT_PRIORITY)
                            ),
                        )
                    )
                    .await
                    .1,
                GlobalResponse::ThreadExited
            );
            assert!(
                wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
                "actual terminal publication must wake the sleeping future"
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                assert_eq!(waiting.await.1, GlobalResponse::ThreadExited);
            })
            .await
            .expect("failed physical birth must wake its waiter");
            assert!(state.sched.lock().unwrap().thread_was_registered(child));
            assert!(
                !state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .contains_key(&child)
            );
        }
    }

    #[tokio::test]
    async fn custody_birth_rpc_wakes_on_backend_failure_without_admitting_child() {
        use std::future::Future;
        for sequentialize in [false, true] {
            let (config, state, parent, mm) = custody_birth_fixture(sequentialize);
            let child = DetTid::from_raw(62);
            let mut waiting = Box::pin(state.receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::RegisterNetworkPhysicalTask {
                        process: parent.as_raw(),
                        thread: child.as_raw(),
                        initial_exec: false,
                    },
                ),
            ));
            let (wakes, waker) = custody_test_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            state.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(parent.as_raw()),
                tid: Tid::from_raw(child.as_raw()),
                phase: "pending custody birth",
            });
            assert!(
                wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
                "actual terminal publication must wake the sleeping future"
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                assert_eq!(waiting.await.1, GlobalResponse::ThreadExited);
            })
            .await
            .expect("terminal backend publication must wake its waiter");
            assert!(!state.sched.lock().unwrap().thread_was_registered(child));
        }
    }

    #[tokio::test]
    async fn custody_birth_rpc_rejects_retired_ptrace_owner_without_waiting_for_tid_reuse() {
        let (config, state, parent, mm) = custody_birth_fixture(true);
        assert!(!config.cancel_killed_thread_rpcs);
        state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid: parent,
                        detpid: parent,
                        mm,
                        thread_start_entered: true,
                        timeslice_stats: TimesliceStats::default(),
                        syscall_count: 0,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert!(state.sched.lock().unwrap().thread_was_registered(parent));
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(parent)
        );
        assert!(
            !state
                .registered_exec_mms
                .lock()
                .unwrap()
                .contains_key(&parent)
        );
        let mut registration = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::RegisterNetworkPhysicalTask {
                    process: parent.as_raw(),
                    thread: parent.as_raw(),
                    initial_exec: false,
                },
            ),
        ));
        assert!(matches!(
            futures::poll!(&mut registration),
            Poll::Ready((_, GlobalResponse::ThreadExited))
        ));
    }

    #[tokio::test]
    async fn custody_task_registration_checks_sender_process_mm_without_cfgseq() {
        let (config, state, owner, _) = fd_lifecycle_exec_fixture();
        assert!(!config.sequentialize_threads);
        let call = |mm, process, thread| {
            state.receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::RegisterNetworkPhysicalTask {
                        process,
                        thread,
                        initial_exec: false,
                    },
                ),
            )
        };
        // No runtime capability means no host pidfd opens, even for an otherwise
        // authenticated task. An ordinary library run remains inactive.
        assert_eq!(
            call(owner.mm, owner.thread.as_raw(), owner.thread.as_raw())
                .await
                .1,
            GlobalResponse::RegisterNetworkPhysicalTask(Ok(false))
        );
        assert_eq!(
            call(owner.mm, owner.thread.as_raw() + 1, owner.thread.as_raw())
                .await
                .1,
            GlobalResponse::ThreadExited
        );
        assert_eq!(
            call(owner.mm, owner.thread.as_raw(), owner.thread.as_raw() + 1)
                .await
                .1,
            GlobalResponse::ThreadExited
        );
        assert_eq!(
            call(
                owner.mm.for_exec(owner.thread),
                owner.thread.as_raw(),
                owner.thread.as_raw()
            )
            .await
            .1,
            GlobalResponse::ThreadExited
        );
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .physical_thread_identity(owner.thread)
                .is_none()
        );
    }

    async fn fd_lifecycle_exec_admit(
        state: &GlobalState,
        config: &Config,
        owner: NetworkStreamOwner,
        receipt: ExecFilesReceipt,
    ) -> crate::network_replay::NetworkFdMutationAdmission {
        use crate::network_replay::NetworkFdMutationBegin as B;
        use crate::network_replay::NetworkFdMutationKind;
        use crate::network_replay::NetworkFdMutationReply as P;
        use crate::network_replay::NetworkFdMutationRequest as Q;
        let reply = stream_rpc(
            state,
            config,
            owner,
            NetworkRequest::FdMutation(Q::Begin {
                files: receipt.old_files,
                kind: NetworkFdMutationKind::Exec { receipt },
            }),
        )
        .await
        .unwrap();
        let NetworkReply::FdMutation(P::Begin(B::Admitted(a))) = reply else {
            panic!("missing admission {reply:?}");
        };
        assert_eq!(
            stream_rpc(
                state,
                config,
                owner,
                NetworkRequest::FdMutation(Q::Submit {
                    permit: a.publication.permit,
                })
            )
            .await
            .unwrap(),
            NetworkReply::FdMutation(P::Unit)
        );
        *a
    }

    #[tokio::test]
    async fn fd_lifecycle_exec_rejects_canceled_receipt_before_engine_mutation() {
        use crate::network_replay::NetworkFdMutationKind;
        use crate::network_replay::NetworkFdMutationRequest as Q;
        let (config, state, owner, files) = fd_lifecycle_exec_fixture();
        let old = prepare_test_rpc(&state, &config, owner.thread, owner.thread).await;
        cancel_test_rpc(&state, &config, old).await;
        let current = prepare_test_rpc(&state, &config, owner.thread, owner.thread).await;
        let before = format!(
            "{:?}",
            state.network_engine.as_ref().unwrap().lock().unwrap()
        );
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FdMutation(Q::Begin {
                    files,
                    kind: NetworkFdMutationKind::Exec { receipt: old },
                })
            )
            .await,
            Err(NetworkRpcError::Internal(_))
        ));
        assert_eq!(
            format!(
                "{:?}",
                state.network_engine.as_ref().unwrap().lock().unwrap()
            ),
            before
        );
        assert_eq!(
            state.pending_exec_states.lock().unwrap()[&owner.thread].receipt,
            current
        );
        fd_lifecycle_exec_admit(&state, &config, owner, current).await;
    }

    #[tokio::test]
    async fn fd_lifecycle_exec_mark_callback_commits_same_table_receipt() {
        let (config, state, owner, files) = fd_lifecycle_exec_fixture();
        let receipt = prepare_test_rpc(&state, &config, owner.thread, owner.thread).await;
        fd_lifecycle_exec_admit(&state, &config, owner, receipt).await;
        let new_mm = owner.mm.for_exec(owner.thread);
        let (_, reply) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    new_mm,
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert_eq!(
            reply,
            GlobalResponse::MarkPastFirstExecve(Default::default(), Some(receipt))
        );
        let engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_eq!(
            engine.fd_table_fixture_files(NetworkStreamOwner {
                mm: new_mm,
                ..owner
            }),
            Some(receipt.new_files)
        );
        assert_eq!(engine.fd_table_fixture_files(owner), None);
        assert_ne!(files, receipt.new_files);
    }

    #[tokio::test]
    async fn fd_lifecycle_exec_reconnect_callback_then_mark_consumes_same_receipt_once() {
        let (config, state, owner, _) = fd_lifecycle_exec_fixture();
        let receipt = prepare_test_rpc(&state, &config, owner.thread, owner.thread).await;
        fd_lifecycle_exec_admit(&state, &config, owner, receipt).await;
        let new_mm = owner.mm.for_exec(owner.thread);
        let (_, reply) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    new_mm,
                    GlobalRequest::CreateChildThread(
                        owner.thread,
                        owner.thread,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        None,
                    ),
                ),
            )
            .await;
        assert_eq!(
            reply,
            GlobalResponse::CreateChildThread(Some((new_mm, receipt)))
        );
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_table_fixture_files(NetworkStreamOwner {
                    mm: new_mm,
                    ..owner
                }),
            Some(receipt.new_files)
        );
        let (_, reply) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    new_mm,
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert_eq!(
            reply,
            GlobalResponse::MarkPastFirstExecve(Default::default(), Some(receipt))
        );
        assert!(state.post_exec_files.lock().unwrap().is_empty());
        let (_, reply) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    new_mm,
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert_eq!(
            reply,
            GlobalResponse::MarkPastFirstExecve(Default::default(), None)
        );
    }

    fn stream_rpc_state(sequential: bool) -> (Config, GlobalState) {
        let mut config = Config {
            sequentialize_threads: sequential,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let state = GlobalState::initialize(&config, false);
        // These existing tests exercise V2 BeginStreamIngress, not V3 Socket
        // enrollment. Keep their explicit legacy engine and every assertion.
        // Replace only the pristine fixture contents; the scheduler and RPC
        // continue to share the original single installed engine allocation.
        *state.network_engine.as_ref().unwrap().lock().unwrap() =
            NetworkReplayEngine::record(config.epoch);
        (config, state)
    }

    async fn stream_rpc(
        state: &GlobalState,
        config: &Config,
        owner: NetworkStreamOwner,
        request: NetworkRequest,
    ) -> Result<NetworkReply, NetworkRpcError> {
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(config),
                    owner.mm,
                    GlobalRequest::Network(request),
                ),
            )
            .await;
        match response {
            GlobalResponse::Network(reply) => reply,
            reply => panic!("unexpected RPC {reply:?}"),
        }
    }

    async fn stream_rpc_bind(
        state: &GlobalState,
        config: &Config,
        owner: NetworkStreamOwner,
        ofd: OpenFileId,
    ) {
        let binding = NetworkChannelBinding {
            transport: detcore_model::network_trace::NetworkTransportV2::Tcp,
            role: detcore_model::network_trace::NetworkEndpointRoleV2::OutboundClient,
            peer_address: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 1],
                port: 443,
            }),
            requested_local_constraint: None,
            observed_local_address: None,
            accepted_from: None,
            selected_channel: None,
        };
        assert!(matches!(
            stream_rpc(
                state,
                config,
                owner,
                NetworkRequest::EnsureChannel {
                    open_file: ofd,
                    binding
                }
            )
            .await,
            Ok(NetworkReply::Channel(Some(_)))
        ));
    }

    fn ingress_receipt(reply: Result<NetworkReply, NetworkRpcError>) -> NetworkStreamLeaseId {
        match reply {
            Ok(NetworkReply::IngressLease(receipt)) => receipt,
            other => panic!("expected receipt {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_rpc_contention_rechecks_after_publication_without_holding_locks() {
        let (config, state) = stream_rpc_state(false);
        let first = NetworkStreamOwner {
            thread: DetTid::from_raw(41),
            mm: MmId::initial(DetTid::from_raw(41)),
        };
        let second = NetworkStreamOwner {
            thread: DetTid::from_raw(42),
            mm: first.mm,
        };
        let ofd = OpenFileId::new_socket(first.thread, 0);
        stream_rpc_bind(&state, &config, first, ofd).await;
        let a = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                first,
                NetworkRequest::BeginStreamIngress { open_file: ofd },
            )
            .await,
        );
        let mut blocked = std::pin::pin!(stream_rpc(
            &state,
            &config,
            second,
            NetworkRequest::BeginStreamIngress { open_file: ofd }
        ));
        assert!(matches!(futures::poll!(blocked.as_mut()), Poll::Pending));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                first,
                NetworkRequest::CompleteStreamIngress {
                    lease: a,
                    observation: NetworkIngressObservation::Bytes(b"abc".to_vec()),
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        let b = ingress_receipt(blocked.await);
        assert_ne!(a, b);
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                second,
                NetworkRequest::CompleteStreamIngress {
                    lease: b,
                    observation: NetworkIngressObservation::NoArrival,
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                second,
                NetworkRequest::StreamQueueStatus { open_file: ofd }
            )
            .await,
            Ok(NetworkReply::StreamQueueStatus(NetworkStreamQueueStatus {
                queued_bytes: 3,
                ingress_busy: false,
                ..
            }))
        ));
    }

    struct NetworkExitRpc<'a> {
        state: &'a GlobalState,
        sender: DetTid,
    }
    #[reverie::tool]
    impl GlobalRPC<GlobalState> for NetworkExitRpc<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.state
                .receive_rpc(Tid::from_raw(self.sender.as_raw()), request)
                .await
        }
        fn config(&self) -> &Config {
            &self.state.cfg
        }
    }

    #[tokio::test]
    async fn blocked_stream_acquire_rechecks_registered_mm_after_notify_without_scheduler_mm() {
        let (config, state) = stream_rpc_state(false);
        let first = NetworkStreamOwner {
            thread: DetTid::from_raw(45),
            mm: MmId::initial(DetTid::from_raw(45)),
        };
        let old = NetworkStreamOwner {
            thread: DetTid::from_raw(46),
            mm: first.mm,
        };
        let new = NetworkStreamOwner {
            thread: old.thread,
            mm: old.mm.for_exec(old.thread),
        };
        let ofd = OpenFileId::new_socket(first.thread, 0);
        stream_rpc_bind(&state, &config, first, ofd).await;
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(old.thread, old.mm);
        let receipt = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                first,
                NetworkRequest::BeginStreamIngress { open_file: ofd },
            )
            .await,
        );
        let mut stale = std::pin::pin!(state.receive_rpc(
            Tid::from_raw(old.thread.as_raw()),
            (
                DetTime::new(&config),
                old.mm,
                GlobalRequest::Network(NetworkRequest::BeginStreamIngress { open_file: ofd })
            )
        ));
        assert!(matches!(futures::poll!(stale.as_mut()), Poll::Pending));
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .rpc_incarnation_matches(old.thread, old.mm)
        );
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(old.thread, new.mm);
        state.network_stream_changed.notify_waiters();
        assert_eq!(stale.await, (None, GlobalResponse::ThreadExited));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                first,
                NetworkRequest::CompleteStreamIngress {
                    lease: receipt,
                    observation: NetworkIngressObservation::NoArrival,
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        let fresh = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                new,
                NetworkRequest::BeginStreamIngress { open_file: ofd },
            )
            .await,
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                new,
                NetworkRequest::CompleteStreamIngress {
                    lease: fresh,
                    observation: NetworkIngressObservation::NoArrival,
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
    }

    #[tokio::test]
    async fn actual_nonsequential_thread_exit_wakes_waiter_but_retains_unresolved_effects() {
        use reverie::Tool;
        let (config, state) = stream_rpc_state(false);
        let first = DetTid::from_raw(51);
        let tool: Detcore = Detcore::new(Tid::from_raw(first.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(first.as_raw()), None);
        thread.detpid = Some(first);
        let owner = NetworkStreamOwner {
            thread: first,
            mm: thread.mm_id,
        };
        let other = NetworkStreamOwner {
            thread: DetTid::from_raw(52),
            mm: owner.mm,
        };
        let ofd = OpenFileId::new_socket(first, 0);
        stream_rpc_bind(&state, &config, owner, ofd).await;
        let receipt = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginStreamIngress { open_file: ofd },
            )
            .await,
        );
        let mut waiting = std::pin::pin!(stream_rpc(
            &state,
            &config,
            other,
            NetworkRequest::BeginStreamIngress { open_file: ofd }
        ));
        assert!(matches!(futures::poll!(waiting.as_mut()), Poll::Pending));
        let rpc = NetworkExitRpc {
            state: &state,
            sender: first,
        };
        tool.on_exit_thread(
            Tid::from_raw(first.as_raw()),
            &rpc,
            thread,
            ExitStatus::Exited(0),
        )
        .await
        .unwrap();
        let NetworkRpcError::Internal(message) = waiting.await.unwrap_err() else {
            panic!("unresolved physical effect must remain an internal failure");
        };
        assert!(message.contains("UnresolvedStreamOperation"));
        assert!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::CompleteStreamIngress {
                    lease: receipt,
                    observation: NetworkIngressObservation::NoArrival,
                }
            )
            .await
            .is_err()
        );
        assert!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .stream_queue_status(ofd)
                .is_err()
        );
    }

    #[tokio::test]
    async fn stale_owner_gone_rpc_cannot_abandon_a_new_incarnation_receipt() {
        let (config, state) = stream_rpc_state(false);
        let tid = DetTid::from_raw(61);
        let old = MmId::initial(tid);
        let owner = NetworkStreamOwner {
            thread: tid,
            mm: old.for_exec(tid),
        };
        let ofd = OpenFileId::new_socket(tid, 0);
        stream_rpc_bind(&state, &config, owner, ofd).await;
        let receipt = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginStreamIngress { open_file: ofd },
            )
            .await,
        );
        assert_eq!(
            state
                .receive_rpc(
                    Tid::from_raw(tid.as_raw()),
                    (DetTime::new(&config), old, GlobalRequest::NetworkOwnerGone)
                )
                .await,
            (None, GlobalResponse::NetworkOwnerGone)
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::CompleteStreamIngress {
                    lease: receipt,
                    observation: NetworkIngressObservation::NoArrival,
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
    }

    #[tokio::test]
    async fn successful_exec_marks_real_siblings_but_preserves_same_mm_other_process_receipt() {
        let (config, state) = stream_rpc_state(true);
        let leader = DetTid::from_raw(71);
        let sibling = DetTid::from_raw(72);
        let survivor = DetTid::from_raw(73);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(leader, leader, true);
            sched.thread_tree.add_child(leader, sibling, false);
            sched.thread_tree.add_child(leader, survivor, true);
        }
        for tid in [leader, sibling, survivor] {
            install_test_registration(&state, tid, Ivar::new());
        }
        let old_mm = MmId::initial(leader);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(survivor, old_mm);
        let sibling_owner = NetworkStreamOwner {
            thread: sibling,
            mm: old_mm,
        };
        let survivor_owner = NetworkStreamOwner {
            thread: survivor,
            mm: old_mm,
        };
        let sibling_fd = OpenFileId::new_socket(sibling, 0);
        let survivor_fd = OpenFileId::new_socket(survivor, 0);
        stream_rpc_bind(&state, &config, sibling_owner, sibling_fd).await;
        stream_rpc_bind(&state, &config, survivor_owner, survivor_fd).await;
        let _sibling_receipt = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                sibling_owner,
                NetworkRequest::BeginStreamIngress {
                    open_file: sibling_fd,
                },
            )
            .await,
        );
        let survivor_receipt = ingress_receipt(
            stream_rpc(
                &state,
                &config,
                survivor_owner,
                NetworkRequest::BeginStreamIngress {
                    open_file: survivor_fd,
                },
            )
            .await,
        );
        let prepared = prepare_test_rpc(&state, &config, leader, leader).await;
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    DetTime::new(&config),
                    old_mm.for_exec(leader),
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert!(
            matches!(response, GlobalResponse::MarkPastFirstExecve(_, Some(receipt)) if receipt == prepared)
        );
        assert!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .stream_queue_status(sibling_fd)
                .is_err()
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                survivor_owner,
                NetworkRequest::CompleteStreamIngress {
                    lease: survivor_receipt,
                    observation: NetworkIngressObservation::NoArrival,
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
    }

    #[test]
    fn production_record_engine_is_v4_native_receive_and_replay_is_not_upgraded() {
        let mut config = Config {
            sequentialize_threads: false,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let state = GlobalState::initialize(&config, false);
        assert_eq!(
            state.native_receive_mode(),
            Some(crate::network_replay::NetworkEngineMode::Record)
        );
        let engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        // Untouched: the census has not yet issued FD-table or accept authority.
        assert!(!engine.fd_table_capability());
        assert!(!engine.accepted_mode());
        drop(engine);
        // The V3 recorder remains constructible and never reports native receive.
        assert!(!NetworkReplayEngine::record_shadow(config.epoch).native_receive_version());
    }

    fn shadow_rpc_state() -> (Config, GlobalState) {
        let mut config = Config {
            sequentialize_threads: false,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let state = GlobalState::initialize(&config, false);
        // These are V3 component controls. Production Record now starts V4, so
        // select the V3 recorder explicitly rather than by policy default.
        *state.network_engine.as_ref().unwrap().lock().unwrap() =
            NetworkReplayEngine::record_shadow(config.epoch);
        assert!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .shadow_mode()
        );
        (config, state)
    }
    async fn shadow_rpc_register(
        state: &GlobalState,
        config: &Config,
        owner: NetworkStreamOwner,
        ofd: OpenFileId,
    ) {
        use detcore_model::network_trace::*;
        let profile = FreshStreamSocketProfileV3 {
            key: StreamSocketKeyV3 {
                transport: NetworkTransportV2::Tcp,
                domain: libc::AF_INET,
                socket_type: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            },
            normalization: LinuxReceiveNormalizationV3 {
                hz: LinuxReceiveHzV3::Hz1000,
                peek_offset_set_supported: true,
                system_rmem_max: 20_971_520,
                namespace_tcp_rmem_max: 6_291_456,
                minimum_receive_buffer: 2304,
            },
            initial: StreamSocketOptionsV3 {
                peek_offset: Some(-1),
                receive_low_water: 1,
                receive_timeout: ReceiveTimeoutV3::Infinite,
                receive_buffer: ReceiveBufferStateV3 {
                    bytes: 262_144,
                    user_locked: false,
                    tcp_scaling_ratio: 128,
                },
            },
        };
        assert!(matches!(
            stream_rpc(
                state,
                config,
                owner,
                NetworkRequest::RegisterStreamSocket {
                    open_file: ofd,
                    key: profile.key,
                    namespace: NetworkStreamNamespace {
                        device: 4,
                        inode: 100
                    },
                    observed_profile: Some(profile)
                }
            )
            .await,
            Ok(NetworkReply::StreamSocketState(Some(_)))
        ));
    }
    fn socket_control_receipt(
        reply: Result<NetworkReply, NetworkRpcError>,
    ) -> NetworkStreamLeaseId {
        match reply {
            Ok(NetworkReply::SocketControl(control)) => control.lease,
            other => panic!("expected control: {other:?}"),
        }
    }
    #[tokio::test]
    async fn v3_production_rpc_requires_enrollment_and_applies_exact_timeout_state() {
        use detcore_model::network_trace::ReceiveTimeoutV3;
        let (config, state) = shadow_rpc_state();
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(301),
            mm: MmId::initial(DetTid::from_raw(301)),
        };
        let ofd = OpenFileId::new_socket(owner.thread, 0);
        let binding = NetworkChannelBinding {
            transport: detcore_model::network_trace::NetworkTransportV2::Tcp,
            role: detcore_model::network_trace::NetworkEndpointRoleV2::OutboundClient,
            peer_address: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 1],
                port: 443,
            }),
            requested_local_constraint: None,
            observed_local_address: None,
            accepted_from: None,
            selected_channel: None,
        };
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::EnsureChannel {
                    open_file: ofd,
                    binding
                }
            )
            .await,
            Err(NetworkRpcError::Internal { .. })
        ));
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .channel_for(ofd),
            None
        );
        shadow_rpc_register(&state, &config, owner, ofd).await;
        stream_rpc_bind(&state, &config, owner, ofd).await;
        let lease = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        for (seconds, microseconds, result) in [(-1, 0, Ok(())), (-1, -1, Err(libc::EDOM))] {
            let option = NetworkStreamSocketOption::ReceiveTimeout {
                seconds,
                microseconds,
            };
            assert_eq!(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::PreviewSocketOption {
                        lease,
                        option: option.clone()
                    }
                )
                .await,
                Ok(NetworkReply::SocketOptionResult(result))
            );
            assert_eq!(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::SubmitStreamPhysical {
                        lease,
                        effect: NetworkStreamPhysicalEffect::SetSocketOption { option }
                    }
                )
                .await,
                Ok(NetworkReply::Unit)
            );
            assert_eq!(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::ConfirmStreamPhysical {
                        lease,
                        result: NetworkStreamPhysicalResult::SocketOption { result }
                    }
                )
                .await,
                Ok(NetworkReply::Unit)
            );
            let NetworkReply::StreamSocketState(Some(current)) = stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::StreamSocketState { open_file: ofd },
            )
            .await
            .unwrap() else {
                panic!("missing options")
            };
            assert_eq!(
                current.options.receive_timeout,
                ReceiveTimeoutV3::FiniteTicks(0)
            );
            assert_eq!(
                current
                    .options
                    .receive_timeout
                    .exposed_timeval(current.normalization.hz),
                (0, 0)
            );
        }
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FinishSocketControl {
                    lease,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
    }
    #[tokio::test]
    async fn v3_control_contention_wakes_and_rechecks_registered_mm() {
        let (config, state) = shadow_rpc_state();
        let first = NetworkStreamOwner {
            thread: DetTid::from_raw(302),
            mm: MmId::initial(DetTid::from_raw(302)),
        };
        let second = NetworkStreamOwner {
            thread: DetTid::from_raw(303),
            mm: first.mm,
        };
        let ofd = OpenFileId::new_socket(first.thread, 0);
        shadow_rpc_register(&state, &config, first, ofd).await;
        let lease = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                first,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        let mut waiter = std::pin::pin!(stream_rpc(
            &state,
            &config,
            second,
            NetworkRequest::BeginSocketControl { open_file: ofd }
        ));
        assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                first,
                NetworkRequest::FinishSocketControl {
                    lease,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        let acquired = socket_control_receipt(waiter.await);
        assert_ne!(lease, acquired);
        let old = NetworkStreamOwner {
            thread: DetTid::from_raw(304),
            mm: first.mm,
        };
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(old.thread, old.mm);
        let mut stale = std::pin::pin!(state.receive_rpc(
            Tid::from_raw(old.thread.as_raw()),
            (
                DetTime::new(&config),
                old.mm,
                GlobalRequest::Network(NetworkRequest::BeginSocketControl { open_file: ofd })
            )
        ));
        assert!(matches!(futures::poll!(stale.as_mut()), Poll::Pending));
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(old.thread, old.mm.for_exec(old.thread));
        state.network_stream_changed.notify_waiters();
        assert_eq!(stale.await, (None, GlobalResponse::ThreadExited));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                second,
                NetworkRequest::FinishSocketControl {
                    lease: acquired,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
    }
    #[tokio::test]
    async fn v3_owner_exit_wakes_contender_without_erasing_pending_option() {
        let (config, state) = shadow_rpc_state();
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(305),
            mm: MmId::initial(DetTid::from_raw(305)),
        };
        let other = NetworkStreamOwner {
            thread: DetTid::from_raw(306),
            mm: owner.mm,
        };
        let ofd = OpenFileId::new_socket(owner.thread, 0);
        shadow_rpc_register(&state, &config, owner, ofd).await;
        let lease = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::SubmitStreamPhysical {
                    lease,
                    effect: NetworkStreamPhysicalEffect::SetSocketOption {
                        option: NetworkStreamSocketOption::ReceiveLowWater(3)
                    }
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        let mut waiter = std::pin::pin!(stream_rpc(
            &state,
            &config,
            other,
            NetworkRequest::BeginSocketControl { open_file: ofd }
        ));
        assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::NetworkOwnerGone,
                ),
            )
            .await;
        assert!(matches!(response, GlobalResponse::NetworkOwnerGone));
        assert!(matches!(
            waiter.await,
            Err(NetworkRpcError::Internal { .. })
        ));
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::ConfirmStreamPhysical {
                    lease,
                    result: NetworkStreamPhysicalResult::SocketOption { result: Ok(()) }
                }
            )
            .await,
            Err(NetworkRpcError::Internal { .. })
        ));
        assert!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .finish()
                .is_err()
        );
    }

    async fn zero_rpc_call(
        state: &GlobalState,
        config: &Config,
        owner: NetworkStreamOwner,
    ) -> crate::network_replay::NetworkStreamCallId {
        let ofd = OpenFileId::new_socket(owner.thread, 0);
        shadow_rpc_register(state, config, owner, ofd).await;
        stream_rpc_bind(state, config, owner, ofd).await;
        let control = socket_control_receipt(
            stream_rpc(
                state,
                config,
                owner,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        let NetworkReply::StreamCall(call) = stream_rpc(
            state,
            config,
            owner,
            NetworkRequest::BeginStreamCall {
                control_lease: control,
            },
        )
        .await
        .unwrap() else {
            panic!("missing call")
        };
        assert!(call.physical_pin_required);
        assert_eq!(
            stream_rpc(
                state,
                config,
                owner,
                NetworkRequest::ConfirmStreamCallPin {
                    id: call.id,
                    outcome: crate::network_replay::NetworkStreamPinOutcome::Acquired
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert_eq!(
            stream_rpc(
                state,
                config,
                owner,
                NetworkRequest::FinishSocketControl {
                    lease: control,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        call.id
    }

    #[tokio::test]
    async fn v3_zero_receive_short_contention_wakes_after_shutdown_and_refuses_stale_mm() {
        use crate::network_replay::NetworkStreamPhysicalEffect;
        use crate::network_replay::NetworkStreamPhysicalResult;
        use crate::network_replay::NetworkZeroStreamReceive;
        let (config, state) = shadow_rpc_state();
        assert!(!config.sequentialize_threads);
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(311),
            mm: MmId::initial(DetTid::from_raw(311)),
        };
        let sibling = NetworkStreamOwner {
            thread: DetTid::from_raw(312),
            mm: owner.mm,
        };
        let call = zero_rpc_call(&state, &config, owner).await;
        let ofd = OpenFileId::new_socket(owner.thread, 0);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        let lease = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        let mut waiter = std::pin::pin!(stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::ZeroStreamReceive {
                call,
                peek_offset: 8
            }
        ));
        assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::SubmitStreamPhysical {
                    lease,
                    effect: NetworkStreamPhysicalEffect::Shutdown {
                        direction: NetworkShutdownV2::Read
                    }
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::ConfirmStreamPhysical {
                    lease,
                    result: NetworkStreamPhysicalResult::Shutdown { result: Ok(()) }
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        // A notification does not bypass the still-held short OFD control.
        assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::FinishSocketControl {
                    lease,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert_eq!(
            waiter.await,
            Ok(NetworkReply::ZeroStreamReceive(
                NetworkZeroStreamReceive::EndOfFile
            ))
        );
        let lease = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        let mut stale = std::pin::pin!(state.receive_rpc(
            Tid::from_raw(owner.thread.as_raw()),
            (
                DetTime::new(&config),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::ZeroStreamReceive {
                    call,
                    peek_offset: 8
                })
            )
        ));
        assert!(matches!(futures::poll!(stale.as_mut()), Poll::Pending));
        let before = format!(
            "{:?}",
            state.network_engine.as_ref().unwrap().lock().unwrap()
        );
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm.for_exec(owner.thread));
        state.network_stream_changed.notify_waiters();
        assert_eq!(stale.await, (None, GlobalResponse::ThreadExited));
        assert_eq!(
            format!(
                "{:?}",
                state.network_engine.as_ref().unwrap().lock().unwrap()
            ),
            before
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::FinishSocketControl {
                    lease,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
    }

    #[tokio::test]
    async fn v3_zero_receive_rpc_requires_authenticated_resolution_before_pin_release() {
        use detcore_model::network_trace::NetworkTrace;

        use crate::network_replay::NetworkZeroStreamReceive;
        let (config, state) = shadow_rpc_state();
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(307),
            mm: MmId::initial(DetTid::from_raw(307)),
        };
        let call = zero_rpc_call(&state, &config, owner).await;
        let NetworkReply::ZeroStreamReceive(NetworkZeroStreamReceive::Waiting(id)) = stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::ZeroStreamReceive {
                call,
                peek_offset: 8,
            },
        )
        .await
        .unwrap() else {
            panic!("missing atomic receipt")
        };
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::InspectZeroStreamWait { id }
            )
            .await,
            Ok(NetworkReply::ZeroStreamWaitEntered(false))
        );
        assert!(
            matches!(stream_rpc(&state, &config, owner, NetworkRequest::BeginStreamCallRelease { id: call }).await, Err(NetworkRpcError::Internal(message)) if message.contains("UnresolvedZeroStreamWait"))
        );
        let stale = NetworkStreamOwner {
            thread: owner.thread,
            mm: owner.mm.for_exec(owner.thread),
        };
        let before = format!(
            "{:?}",
            state.network_engine.as_ref().unwrap().lock().unwrap()
        );
        assert!(
            stream_rpc(
                &state,
                &config,
                stale,
                NetworkRequest::CancelZeroStreamWait { id }
            )
            .await
            .is_err()
        );
        assert_eq!(
            format!(
                "{:?}",
                state.network_engine.as_ref().unwrap().lock().unwrap()
            ),
            before
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::CancelZeroStreamWait { id }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginStreamCallRelease { id: call }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FinishStreamCallRelease { id: call }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::InspectZeroStreamWait { id }
            )
            .await
            .is_err()
        );
        // Exercise the Record finalizer, not Replay-only engine.finish().
        let mut state = state;
        let mut output = tempfile::tempfile().unwrap();
        state.cfg.network_trace_output_fd = Some(std::os::fd::AsRawFd::as_raw_fd(&output));
        state.finalize_network_trace().unwrap();
        std::io::Seek::rewind(&mut output).unwrap();
        assert!(matches!(
            NetworkTrace::read_framed(&mut output).unwrap(),
            NetworkTrace::V3(_)
        ));
    }

    #[tokio::test]
    async fn v3_zero_receive_rpc_owner_exit_preserves_unresolved_receipt_and_rejects_ack() {
        use crate::network_replay::NetworkReplayError;
        use crate::network_replay::NetworkZeroStreamReceive;
        let (config, state) = shadow_rpc_state();
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(308),
            mm: MmId::initial(DetTid::from_raw(308)),
        };
        let call = zero_rpc_call(&state, &config, owner).await;
        let NetworkReply::ZeroStreamReceive(NetworkZeroStreamReceive::Waiting(id)) = stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::ZeroStreamReceive {
                call,
                peek_offset: 0,
            },
        )
        .await
        .unwrap() else {
            panic!("missing atomic receipt")
        };
        let operation = ExternalOpId::new(owner.thread, 19);
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginZeroStreamWait {
                    call,
                    record_operation: Some(operation)
                }
            )
            .await,
            Ok(NetworkReply::ZeroStreamWait(id))
        );
        // Scheduler entry has its own tests; dispatch must not fabricate it.
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::InspectZeroStreamWait { id }
            )
            .await,
            Ok(NetworkReply::ZeroStreamWaitEntered(false))
        );
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::NetworkOwnerGone,
                ),
            )
            .await;
        assert!(matches!(response, GlobalResponse::NetworkOwnerGone));
        assert!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FinishZeroStreamWait { id }
            )
            .await
            .is_err()
        );
        assert!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::CancelZeroStreamWait { id }
            )
            .await
            .is_err()
        );
        assert!(
            matches!(state.network_engine.as_ref().unwrap().lock().unwrap().finish(), Err(NetworkReplayError::UnresolvedZeroStreamWait(actual)) if actual == id)
        );
    }

    fn cancellation_test_state() -> (Config, GlobalState, DetTid, DetPid) {
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        (config, state, dettid, detpid)
    }

    fn install_test_registration(state: &GlobalState, dettid: DetTid, request: Ivar<SchedRequest>) {
        let mut scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_is_logically_killed(dettid));
        scheduler.next_turns.insert(
            dettid,
            ThreadNextTurn {
                dettid,
                child_tid_addr: 0,
                req: request,
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        scheduler.priorities.insert(dettid, DEFAULT_PRIORITY);
        scheduler.runqueue_push_back(dettid);
        let process = scheduler
            .registered_process(dettid)
            .expect("test task has a process");
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .entry(dettid)
            .or_insert(MmId::initial(process));
    }

    // Exercise the real external registration method and global RPC without a
    // backend or a guest process. Any unexpected guest operation fails the test.
    struct ExternalRegistrationGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<()>,
        requests: Mutex<Vec<GlobalRequest>>,
        pause_rpc: Option<&'static str>,
    }

    struct ExternalRegistrationStack;
    struct ExternalRegistrationStackGuard;

    impl Drop for ExternalRegistrationStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for ExternalRegistrationStack {
        type StackGuard = ExternalRegistrationStackGuard;

        fn size(&self) -> usize {
            panic!("external registration must not use a guest stack")
        }

        fn capacity(&self) -> usize {
            panic!("external registration must not use a guest stack")
        }

        fn push<'stack, T>(&mut self, _value: T) -> reverie::syscalls::Addr<'stack, T> {
            panic!("external registration must not use a guest stack")
        }

        fn reserve<'stack, T>(&mut self) -> reverie::syscalls::AddrMut<'stack, T> {
            panic!("external registration must not use a guest stack")
        }

        fn commit(self) -> Result<Self::StackGuard, reverie::syscalls::Errno> {
            panic!("external registration must not use a guest stack")
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ExternalRegistrationGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests.lock().unwrap().push(message.2.clone());
            let submit = matches!(
                &message.2,
                GlobalRequest::Network(NetworkRequest::FdMutation(
                    crate::network_replay::NetworkFdMutationRequest::Submit { .. }
                ))
            );
            let prepare_birth = matches!(&message.2, GlobalRequest::PrepareNoSeqBirth { .. });
            if submit && self.pause_rpc == Some("before-fd-submit") {
                std::future::pending::<()>().await;
            }
            let response = self
                .global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
                .await;
            if (submit && self.pause_rpc == Some("after-fd-submit"))
                || (prepare_birth && self.pause_rpc == Some("after-birth-prepare"))
            {
                std::future::pending::<()>().await;
            }
            response
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for ExternalRegistrationGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;

        fn tid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<reverie::Pid> {
            None
        }

        fn memory(&self) -> Self::Memory {
            panic!("external registration must not access guest memory")
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("external registration must not read guest registers")
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("external registration must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("external registration must not daemonize")
        }

        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            panic!("external registration must not inject a syscall")
        }

        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> reverie::Never {
            panic!("external registration unexpectedly retired its live parent")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn set_timer_precise(
            &mut self,
            _schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("external registration must not read a host clock")
        }
    }

    // Real scheduler/Global/owned metadata boundary. The existing fixture
    // issuer is component input only; Config still cannot activate native FD
    // capability, and this Guest panics on physical injection or memory access.
    fn selected_external_fixture() -> (Config, GlobalState, Detcore, crate::ThreadState<()>) {
        use reverie::Tool;
        let (config, state) = stream_rpc_state(true);
        let tid = Tid::from_raw(181);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.stats.syscall_count = 17;
        *thread.file_metadata.lock().unwrap() =
            crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid);
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, owner.thread, true);
        install_test_registration(&state, owner.thread, Ivar::new());
        // Match delayed process initialization in handle_thread_start before
        // this fixture enters the real parked RPC.
        assert_eq!(thread.detpid, None);
        thread.detpid = state.sched.lock().unwrap().registered_process(owner.thread);
        assert_eq!(thread.detpid, Some(owner.thread));
        assert_eq!(
            state.registered_exec_mms.lock().unwrap().get(&owner.thread),
            Some(&owner.mm)
        );
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            engine.fd_publication_fixture_register(owner, None);
        }
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        (config, state, tool, thread)
    }

    fn selected_external_request(thread: &crate::ThreadState<()>, fd: i32) -> Resources {
        let operation = ExternalOpId::new(thread.dettid, thread.stats.syscall_count);
        let mut request = Resources::new(thread.dettid);
        request.insert(
            ResourceID::BlockingNetworkCapture(operation),
            Permission::RW,
        );
        request.fyi("close");
        request.fd_read = Some(crate::scheduler::fd_read::FdReadIntent {
            owner: NetworkStreamOwner {
                thread: thread.dettid,
                mm: thread.mm_id,
            },
            files: thread.file_metadata.lock().unwrap().files_id,
            fd,
            operation,
        });
        request
    }

    async fn finish_external_component_grant(
        state: &GlobalState,
        selected: (DetTid, Ivar<SchedRequest>, Ivar<SchedResponse>),
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
    ) {
        let before = state.sched.lock().unwrap().turn;
        let request = selected.1.clone();
        let response = selected.2.clone();
        let result = crate::scheduler::finish_selected_turn(
            state.sched.clone(),
            state.global_time.clone(),
            selected.0,
            selected.1,
            selected.2,
        )
        .await;
        // SkipTurn is a unit control-flow type. The exact normal external
        // branch grants in step4 and then returns this value, bypassing step5.
        // Inspect all distinguishing state, not merely Result::is_err().
        assert!(matches!(result, Err(crate::scheduler::SkipTurn)));
        let sched = state.sched.lock().unwrap();
        assert!(!sched.backend_failed());
        assert_eq!(sched.fd_read_grant_diagnostic(), None);
        assert_eq!(sched.turn, before + 1);
        assert!(!sched.run_queue.contains_tid(owner.thread));
        assert_eq!(
            sched.blocked.external_io_blockers.get(&owner.thread),
            Some(&operation)
        );
        assert!(sched.original_external_grant_matches(owner, operation));
        assert_ne!(sched.next_turns[&owner.thread].req, request);
        assert_ne!(sched.next_turns[&owner.thread].resp, response);
        assert!(sched.next_turns[&owner.thread].req.try_read().is_none());
        assert!(
            matches!(response.try_read(), Some(SchedResponse::GoFdRead(_, read))
            if read.publication.permit.owner == owner && read.external_grant == Some(operation))
        );
    }

    #[tokio::test]
    async fn selected_external_reader_revalidates_queued_slot_and_keeps_original_grant() {
        use reverie::Tool;
        let (config, state, tool, thread) = selected_external_fixture();
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        guest
            .thread
            .add_fd(
                7,
                nix::fcntl::OFlag::empty(),
                crate::fd::FdType::Socket,
                None,
            )
            .unwrap();
        let original = guest.thread.descriptor_binding(7).unwrap();
        {
            let mut metadata = guest.thread.file_metadata.lock().unwrap();
            let replacement = metadata.pending_network_installations()[0];
            let effect = state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_publication_fixture_effect(owner, replacement);
            metadata
                .associate_network_installation(replacement.installation_generation, effect)
                .unwrap();
        }
        tool.publish_network_fd_installations(&mut guest)
            .await
            .unwrap();
        guest.requests.lock().unwrap().clear();
        let request = selected_external_request(&guest.thread, 7);
        let encoded = bincode::serde::encode_to_vec(&request, bincode::config::legacy()).unwrap();
        let (decoded, consumed) =
            bincode::serde::decode_from_slice::<Resources, _>(&encoded, bincode::config::legacy())
                .unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, request);
        let metadata = guest.thread.file_metadata.clone();
        let mut peer = tool.init_thread_state(Tid::from_raw(182), None);
        peer.file_metadata = metadata.clone();
        peer.mm_id = owner.mm;
        peer.detpid = Some(owner.thread);
        let peer_owner = NetworkStreamOwner {
            thread: peer.dettid,
            mm: peer.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, peer_owner.thread, false);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(peer_owner.thread, peer_owner.mm);
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_publication_fixture_register(peer_owner, Some(owner)),
            original.slot.files
        );
        tool.on_thread_state_ready(Tid::from_raw(182), &state, &peer)
            .unwrap();
        state.global_time.lock().unwrap().update_global_time(
            peer_owner.thread,
            peer.thread_logical_time.as_nanos(),
            peer.thread_logical_time.inherited_nanos(),
        );
        let mut peer_guest = RetirementGuest {
            global: &state,
            config: &config,
            thread: peer,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let before_turn = state.sched.lock().unwrap().turn;
        let before_clock = state.global_time.lock().unwrap().as_nanos();
        let before_local = guest.thread.thread_logical_time.as_nanos();
        let reply = {
            let mut pending =
                std::pin::pin!(super::fd_read_resource_request(&mut guest, request.clone()));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(original.open_file),
                (0, 0, 0, 0)
            );
            // Explicit admitted replacement input; use the maintained local
            // installation and publication adapter, not a cloned binding map.
            peer_guest
                .thread
                .add_fd(
                    7,
                    nix::fcntl::OFlag::O_NONBLOCK,
                    crate::fd::FdType::Socket,
                    None,
                )
                .unwrap();
            {
                let mut table = metadata.lock().unwrap();
                let replacement = table.pending_network_installations()[0];
                assert_eq!(replacement.before.unwrap().binding, original);
                let effect = state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .fd_publication_fixture_effect(peer_owner, replacement);
                table
                    .associate_network_installation(replacement.installation_generation, effect)
                    .unwrap();
            }
            tool.publish_network_fd_installations(&mut peer_guest)
                .await
                .unwrap();
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert_eq!(selected.0, owner.thread);
            let queued = selected.1.try_read().unwrap().unwrap();
            assert_eq!(queued, request);
            assert_eq!(queued.resources.len(), 1);
            assert_eq!(queued.fyi, "close");
            finish_external_component_grant(
                &state,
                selected,
                owner,
                request.fd_read.unwrap().operation,
            )
            .await;
            pending.await
        };
        let ResourceReply::ReadGrant {
            status: ResumeStatus::Normal,
            read,
        } = reply
        else {
            panic!("missing original selected grant")
        };
        let read = *read;
        let current = metadata.lock().unwrap().descriptor_binding(7).unwrap();
        assert_ne!(current, original);
        assert_eq!(read.binding, Some(current));
        assert_eq!(
            metadata
                .lock()
                .unwrap()
                .observe_fd_read(&read)
                .unwrap()
                .nonblocking,
            Some(true)
        );
        assert_eq!(state.sched.lock().unwrap().turn, before_turn + 1);
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .original_external_grant_matches(owner, request.fd_read.unwrap().operation)
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .run_queue
                .contains_tid(owner.thread)
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_clock);
        assert_eq!(guest.thread.thread_logical_time.as_nanos(), before_local);
        assert_eq!(guest.thread.stats.syscall_count, 17);
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![GlobalRequest::ParkedRequest(
                request,
                owner.thread,
                ControlCapability::None
            )]
        );
        assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
        assert!(!guest.retired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            super::network_request(&mut guest, NetworkRequest::FinishFdRead { admission: read })
                .await,
            Ok(NetworkReply::Unit)
        );
    }

    #[tokio::test]
    async fn selected_external_lost_grant_and_lost_call_transfer_have_distinct_consuming_cleanup() {
        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Kind;
        use crate::network_replay::original_connect::Local;
        for transfer in [false, true] {
            let (config, state, _tool, mut thread) = selected_external_fixture();
            let owner = NetworkStreamOwner {
                thread: thread.dettid,
                mm: thread.mm_id,
            };
            let request = selected_external_request(&thread, -1);
            let arguments = Arguments {
                kind: Kind::Close,
                operation: request.fd_read.unwrap().operation,
                files: request.fd_read.unwrap().files,
                binding: None,
                fd: -1,
                address: 0,
                length: 0,
                original_count: 0,
            };
            thread.original_connect = Some(Local {
                arguments: arguments.clone(),
                raw_arguments: [usize::MAX, 0, 0, 0, 0, 0],
                admission: None,
                invoked: false,
                returned: None,
            });
            let mut guest = RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(vec![]),
                retired: std::sync::atomic::AtomicBool::new(false),
            };
            let read = {
                let mut pending =
                    std::pin::pin!(super::fd_read_resource_request(&mut guest, request.clone()));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
                let response = selected.2.clone();
                finish_external_component_grant(
                    &state,
                    selected,
                    owner,
                    request.fd_read.unwrap().operation,
                )
                .await;
                let SchedResponse::GoFdRead(_, read) = response.try_read().unwrap() else {
                    panic!("missing grant")
                };
                let read = *read;
                // Observer reads the durable response, but the Guest future is
                // dropped here without polling or receiving that response.
                read
            };
            assert_eq!(read.publication.permit.files, arguments.files);
            assert_eq!(read.binding, arguments.binding);
            // Component Call transfer only: no native preparation, result or
            // backend observation is constructed by this control.
            let admission = transfer.then(|| {
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .begin_original_external_from_read(owner, arguments, read.clone())
                    .unwrap()
            });
            let before_turn = state.sched.lock().unwrap().turn;
            let local = guest.thread.original_connect.take().unwrap();
            assert!(local.admission.is_none() && !local.invoked && local.returned.is_none());
            let (_, consumed) = state
                .receive_rpc(
                    Tid::from_raw(owner.thread.as_raw()),
                    (
                        DetTime::new(&config),
                        owner.mm,
                        GlobalRequest::OriginalConnectOwnerGone(local),
                    ),
                )
                .await;
            assert_eq!(consumed, GlobalResponse::OriginalConnectOwnerGone(true));
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            assert!(engine.finish_fd_read(owner, read.clone()).is_err());
            if let Some(admission) = admission {
                assert_eq!(
                    engine
                        .original_connect_cancellation(owner, &admission)
                        .unwrap(),
                    (true, true)
                );
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                assert_eq!(
                    engine.fd_read_pending_external_selection(
                        owner,
                        read.publication.permit.files,
                        -1
                    ),
                    Some((owner, request.fd_read.unwrap().operation))
                );
            } else {
                assert_eq!(
                    engine.fd_read_pending_external_selection(
                        owner,
                        read.publication.permit.files,
                        -1
                    ),
                    None
                );
            }
            drop(engine);
            assert_eq!(state.sched.lock().unwrap().turn, before_turn);
            assert!(!state.sched.lock().unwrap().backend_failed());
        }
    }

    #[tokio::test]
    async fn selected_external_reader_waits_for_exact_prior_selection_not_final_return() {
        use reverie::Tool;

        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Kind;
        let (config, state, tool, thread) = selected_external_fixture();
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        let metadata = thread.file_metadata.clone();
        let request = selected_external_request(&thread, -1);
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let first = {
            let mut pending =
                std::pin::pin!(super::fd_read_resource_request(&mut guest, request.clone()));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            finish_external_component_grant(
                &state,
                selected,
                owner,
                request.fd_read.unwrap().operation,
            )
            .await;
            pending.await
        };
        let ResourceReply::ReadGrant { read, .. } = first else {
            panic!("missing first grant")
        };
        let read = *read;
        let admission = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            let admission = engine
                .begin_original_external_from_read(
                    owner,
                    Arguments {
                        kind: Kind::Close,
                        operation: request.fd_read.unwrap().operation,
                        files: read.publication.permit.files,
                        binding: None,
                        fd: -1,
                        address: 0,
                        length: 0,
                        original_count: 0,
                    },
                    read,
                )
                .unwrap();
            // Provider phases and the selection below are explicit component
            // inputs, not native evidence. No raw completion or final ACK is
            // supplied; the test must progress while both remain unknown.
            engine
                .original_connect_provider_submitted(owner, &admission)
                .unwrap();
            engine
                .original_call_prepared(owner, &admission, None, 41)
                .unwrap();
            engine.original_connect_invoked(owner, &admission).unwrap();
            admission
        };
        let mut peer = tool.init_thread_state(Tid::from_raw(183), None);
        peer.file_metadata = metadata.clone();
        peer.mm_id = owner.mm;
        peer.detpid = Some(owner.thread);
        let peer_owner = NetworkStreamOwner {
            thread: peer.dettid,
            mm: peer.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, peer_owner.thread, false);
        install_test_registration(&state, peer_owner.thread, Ivar::new());
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(peer_owner.thread, peer_owner.mm);
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_publication_fixture_register(peer_owner, Some(owner)),
            admission.arguments.files
        );
        state.global_time.lock().unwrap().update_global_time(
            peer_owner.thread,
            peer.thread_logical_time.as_nanos(),
            peer.thread_logical_time.inherited_nanos(),
        );
        tool.on_thread_state_ready(Tid::from_raw(183), &state, &peer)
            .unwrap();
        let peer_request = selected_external_request(&peer, -1);
        let peer_operation = peer_request.fd_read.unwrap().operation;
        let mut peer_guest = RetirementGuest {
            global: &state,
            config: &config,
            thread: peer,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let second = {
            let mut pending = std::pin::pin!(super::fd_read_resource_request(
                &mut peer_guest,
                peer_request
            ));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert_eq!(selected.0, peer_owner.thread);
            let before_turn = state.sched.lock().unwrap().turn;
            let before_time = state.global_time.lock().unwrap().as_nanos();
            let mut turn = std::pin::pin!(finish_external_component_grant(
                &state,
                selected,
                peer_owner,
                peer_operation
            ));
            assert!(futures::poll!(turn.as_mut()).is_pending());
            state.network_stream_changed.notify_waiters();
            assert!(
                futures::poll!(turn.as_mut()).is_pending(),
                "wakeup is not selection authority"
            );
            assert_eq!(state.sched.lock().unwrap().turn, before_turn);
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_time);
            {
                let mut table = metadata.lock().unwrap();
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                let selected = crate::network_runtime::OriginalSelection {
                    command: 41,
                    call: admission.call.native_command_call(),
                    owner_mm: owner.mm.generation(),
                    provider: 1,
                    task: 181,
                    task_start: 19,
                    table: 3,
                    file: 0,
                    requested_fd: -1,
                    ready: 1,
                    user_address: 0,
                    fdput_flags: 0,
                    address_length: 0,
                    original_count: 0,
                };
                engine
                    .publish_original_close_selection(owner, &admission, &selected, &mut table)
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                assert!(engine.finish_original_connect(owner, &admission).is_err());
            }
            state.network_stream_changed.notify_waiters();
            turn.await; // helper proves the exact normal step4 background transition
            assert_eq!(state.sched.lock().unwrap().turn, before_turn + 1);
            pending.await
        };
        let ResourceReply::ReadGrant { read, .. } = second else {
            panic!("missing peer grant")
        };
        let read = *read;
        assert_eq!(read.publication.permit.owner, peer_owner);
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .original_connect_result(owner, &admission)
                .unwrap(),
            None
        );
        assert_eq!(
            super::network_request(
                &mut peer_guest,
                NetworkRequest::FinishFdRead { admission: read }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert!(!state.sched.lock().unwrap().backend_failed());
    }

    #[tokio::test]
    async fn selected_external_reader_publication_between_busy_cut_and_wait_keeps_original_grant() {
        use reverie::Tool;

        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Kind;
        let (config, state, tool, thread) = selected_external_fixture();
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        let metadata = thread.file_metadata.clone();
        let request = selected_external_request(&thread, -1);
        let mut guest = RetirementGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let first = {
            let mut pending =
                std::pin::pin!(super::fd_read_resource_request(&mut guest, request.clone()));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            finish_external_component_grant(
                &state,
                selected,
                owner,
                request.fd_read.unwrap().operation,
            )
            .await;
            pending.await
        };
        let ResourceReply::ReadGrant { read, .. } = first else {
            panic!("missing first grant")
        };
        let read = *read;
        let admission = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            let admission = engine
                .begin_original_external_from_read(
                    owner,
                    Arguments {
                        kind: Kind::Close,
                        operation: request.fd_read.unwrap().operation,
                        files: read.publication.permit.files,
                        binding: None,
                        fd: -1,
                        address: 0,
                        length: 0,
                        original_count: 0,
                    },
                    read,
                )
                .unwrap();
            // Provider phases and the selection below are explicit component
            // inputs, not native evidence. No raw completion or final ACK is
            // supplied; the test must progress while both remain unknown.
            engine
                .original_connect_provider_submitted(owner, &admission)
                .unwrap();
            engine
                .original_call_prepared(owner, &admission, None, 41)
                .unwrap();
            engine.original_connect_invoked(owner, &admission).unwrap();
            admission
        };
        let mut peer = tool.init_thread_state(Tid::from_raw(183), None);
        peer.file_metadata = metadata.clone();
        peer.mm_id = owner.mm;
        peer.detpid = Some(owner.thread);
        let peer_owner = NetworkStreamOwner {
            thread: peer.dettid,
            mm: peer.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, peer_owner.thread, false);
        install_test_registration(&state, peer_owner.thread, Ivar::new());
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(peer_owner.thread, peer_owner.mm);
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_publication_fixture_register(peer_owner, Some(owner)),
            admission.arguments.files
        );
        state.global_time.lock().unwrap().update_global_time(
            peer_owner.thread,
            peer.thread_logical_time.as_nanos(),
            peer.thread_logical_time.inherited_nanos(),
        );
        tool.on_thread_state_ready(Tid::from_raw(183), &state, &peer)
            .unwrap();
        let peer_request = selected_external_request(&peer, -1);
        let peer_operation = peer_request.fd_read.unwrap().operation;
        let mut peer_guest = RetirementGuest {
            global: &state,
            config: &config,
            thread: peer,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
        };
        let second = {
            let mut pending = std::pin::pin!(super::fd_read_resource_request(
                &mut peer_guest,
                peer_request.clone()
            ));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert_eq!(selected.0, peer_owner.thread);
            let before_turn = state.sched.lock().unwrap().turn;
            let before_time = state.global_time.lock().unwrap().as_nanos();
            let cut_reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cut_seen = cut_reached.clone();
            let cut_engine = state.network_engine.as_ref().unwrap().clone();
            let cut_metadata = metadata.clone();
            let cut_changed = state.network_stream_changed.clone();
            let cut_admission = admission.clone();
            // The actual selected loop registers Notify before admission, then
            // invokes this once after that function drops metadata/engine and
            // before matching its decision. Frozen v1's second lookup follows
            // this exact cut and therefore sees no former holder.
            state.sched.lock().unwrap().set_fd_read_test_cut(move || {
                let mut table = cut_metadata.lock().unwrap();
                let mut engine = cut_engine.lock().unwrap();
                assert_eq!(
                    engine.fd_read_pending_external_selection(
                        peer_owner,
                        cut_admission.arguments.files,
                        -1
                    ),
                    Some((owner, cut_admission.arguments.operation))
                );
                let selection = crate::network_runtime::OriginalSelection {
                    command: 41,
                    call: cut_admission.call.native_command_call(),
                    owner_mm: owner.mm.generation(),
                    provider: 1,
                    task: 181,
                    task_start: 19,
                    table: 3,
                    file: 0,
                    requested_fd: -1,
                    ready: 1,
                    user_address: 0,
                    fdput_flags: 0,
                    address_length: 0,
                    original_count: 0,
                };
                engine
                    .publish_original_close_selection(owner, &cut_admission, &selection, &mut table)
                    .unwrap();
                assert_eq!(
                    engine
                        .original_connect_result(owner, &cut_admission)
                        .unwrap(),
                    None
                );
                assert!(
                    engine
                        .finish_original_connect(owner, &cut_admission)
                        .is_err()
                );
                assert_eq!(
                    engine.fd_read_pending_external_selection(
                        peer_owner,
                        cut_admission.arguments.files,
                        -1
                    ),
                    None
                );
                drop(engine);
                drop(table);
                cut_seen.store(true, std::sync::atomic::Ordering::SeqCst);
                cut_changed.notify_waiters();
            });
            finish_external_component_grant(&state, selected, peer_owner, peer_operation).await;
            assert!(cut_reached.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(state.sched.lock().unwrap().turn, before_turn + 1);
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_time);
            pending.await
        };
        let ResourceReply::ReadGrant { read, .. } = second else {
            panic!("missing peer grant")
        };
        let read = *read;
        assert_eq!(read.publication.permit.owner, peer_owner);
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .original_connect_result(owner, &admission)
                .unwrap(),
            None
        );
        assert_eq!(
            super::network_request(
                &mut peer_guest,
                NetworkRequest::FinishFdRead { admission: read }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert!(!state.sched.lock().unwrap().backend_failed());
    }

    // Uses the real owned ThreadState, FileMetadata publication adapter and
    // Global RPC. Initial installation association is explicit component input;
    // no backend capability is exposed through Config and no pin is fabricated.
    #[tokio::test]
    async fn fd_reader_actual_adapter_preserves_turn_time_and_one_metadata_observation() {
        use reverie::Tool;
        for sequential in [true, false] {
            let (config, state) = stream_rpc_state(sequential);
            let tid = Tid::from_raw(81);
            let tool: Detcore = Detcore::new(tid, &config);
            let mut thread = tool.init_thread_state(tid, None);
            thread.stats.syscall_count = 17;
            *thread.file_metadata.lock().unwrap() =
                crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid);
            thread
                .add_fd(
                    7,
                    nix::fcntl::OFlag::O_NONBLOCK,
                    crate::fd::FdType::Socket,
                    None,
                )
                .unwrap();
            let owner = NetworkStreamOwner {
                thread: thread.dettid,
                mm: thread.mm_id,
            };
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .add_child(owner.thread, owner.thread, true);
            state
                .registered_exec_mms
                .lock()
                .unwrap()
                .insert(owner.thread, owner.mm);
            let binding = thread.descriptor_binding(7).unwrap();
            {
                let mut table = thread.file_metadata.lock().unwrap();
                let replacement = table.pending_network_installations()[0];
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                engine.fd_table_fixture_enable();
                assert_eq!(
                    engine.fd_publication_fixture_register(owner, None),
                    binding.slot.files
                );
                let effect = engine.fd_publication_fixture_effect(owner, replacement);
                table
                    .associate_network_installation(replacement.installation_generation, effect)
                    .unwrap();
            }
            state.global_time.lock().unwrap().update_global_time(
                owner.thread,
                thread.thread_logical_time.as_nanos(),
                thread.thread_logical_time.inherited_nanos(),
            );
            let schedule = || {
                let sched = state.sched.lock().unwrap();
                format!(
                    "{:?}",
                    (
                        &sched.turn,
                        &sched.run_queue,
                        &sched.next_turns,
                        &sched.bg_action_pool,
                        &sched.committed_time,
                        &sched.blocked,
                        &sched.per_thread_syscalls
                    )
                )
            };
            // Explicit component lifecycle delivery with this actual owned state.
            tool.on_thread_state_ready(tid, &state, &thread).unwrap();
            let before_schedule = schedule();
            let before_global = state.global_time.lock().unwrap().as_nanos();
            let before_local = thread.thread_logical_time.as_nanos();
            let mut guest = RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(vec![]),
                retired: std::sync::atomic::AtomicBool::new(false),
            };
            let read = tool.begin_network_fd_read(&mut guest, 7).await.unwrap();
            assert_eq!(read.binding, Some(binding));
            let observed = guest
                .thread
                .file_metadata
                .lock()
                .unwrap()
                .observe_fd_read(&read)
                .unwrap();
            assert_eq!(observed.binding, Some(binding));
            assert_eq!(observed.socket, Some(binding.open_file));
            assert_eq!(observed.nonblocking, Some(true));
            assert!(
                guest
                    .thread
                    .file_metadata
                    .lock()
                    .unwrap()
                    .pending_network_installations()
                    .is_empty()
            );
            assert_eq!(schedule(), before_schedule);
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_global);
            assert_eq!(guest.thread.thread_logical_time.as_nanos(), before_local);
            assert_eq!(guest.thread.stats.syscall_count, 17);
            assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(binding.open_file),
                (0, 1, 1, 0)
            );
            assert_eq!(
                super::network_request(
                    &mut guest,
                    NetworkRequest::FinishFdRead { admission: read }
                )
                .await,
                Ok(NetworkReply::Unit)
            );
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(binding.open_file),
                (0, 0, 0, 0)
            );
            assert!(
                guest
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| matches!(
                        request,
                        GlobalRequest::Network(
                            NetworkRequest::FdPublication(_)
                                | NetworkRequest::BeginFdRead { .. }
                                | NetworkRequest::FinishFdRead { .. }
                        )
                    ))
            );
            assert!(!guest.retired.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[test]
    fn ready_metadata_callback_is_inert_without_authority_and_preserves_exact_object() {
        use reverie::Tool;
        let (config, state) = stream_rpc_state(false);
        let tid = Tid::from_raw(89);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        let engine = state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        tool.on_thread_state_ready(Tid::from_raw(90), &state, &thread)
            .unwrap();
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        assert!(!engine.lock().unwrap().fd_table_capability());
        *thread.file_metadata.lock().unwrap() =
            crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid);
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, owner.thread, true);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        let files = {
            let mut engine = engine.lock().unwrap();
            engine.fd_table_fixture_enable();
            engine.fd_publication_fixture_register(owner, None)
        };
        let count = Arc::strong_count(&thread.file_metadata);
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        assert_eq!(Arc::strong_count(&thread.file_metadata), count);
        let current = engine.lock().unwrap().fd_metadata(owner, files).unwrap();
        assert!(Arc::ptr_eq(&current, &thread.file_metadata));
        drop(current);
        let before = format!("{:?}", engine.lock().unwrap());
        assert!(
            tool.on_thread_state_ready(Tid::from_raw(90), &state, &thread)
                .is_err()
        );
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        // Replacing only the Arc is not an exec/table receipt.
        thread.file_metadata = Arc::new(Mutex::new(
            crate::tool_local::FileMetadata::empty_network_fixture(owner.thread),
        ));
        assert!(tool.on_thread_state_ready(tid, &state, &thread).is_err());
        assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
        assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
        assert!(!state.sched.lock().unwrap().backend_failed());
    }

    #[tokio::test]
    async fn fd_reader_no_seq_wait_rechecks_owner_and_lost_reply_is_consumed_before_table_retirement()
     {
        let (config, state) = stream_rpc_state(false);
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(83),
            mm: MmId::initial(DetTid::from_raw(83)),
        };
        let peer = NetworkStreamOwner {
            thread: DetTid::from_raw(84),
            mm: owner.mm,
        };
        let files = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            let files = engine.fd_publication_fixture_register(owner, None);
            assert_eq!(
                engine.fd_publication_fixture_register(peer, Some(owner)),
                files
            );
            files
        };
        // Explicit component metadata association; this lower-layer setup is
        // not a backend state-ready or physical selection receipt.
        let metadata = Arc::new(Mutex::new(
            crate::tool_local::FileMetadata::empty_network_fixture(owner.thread),
        ));
        {
            let local = metadata.lock().unwrap();
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine
                .associate_fd_metadata(owner, &metadata, &local)
                .unwrap();
            engine
                .associate_fd_metadata(peer, &metadata, &local)
                .unwrap();
        }
        // The response is intentionally not delivered to an adapter. The real
        // Global owner nevertheless retains exactly its untransferred read.
        let reply = stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::BeginFdRead { files, fd: -1 },
        )
        .await
        .unwrap();
        let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) = reply
        else {
            panic!("reader was not admitted")
        };
        let read = *read;
        assert!(read.binding.is_none() && read.control.is_none());
        let mut pending = std::pin::pin!(stream_rpc(
            &state,
            &config,
            peer,
            NetworkRequest::BeginFdRead { files, fd: -1 }
        ));
        assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
        state.abandon_network_owners([NetworkStreamOwner {
            thread: owner.thread,
            mm: owner.mm.for_exec(owner.thread),
        }]);
        assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
        state.abandon_network_owners([owner]);
        let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(next)) =
            pending.await.unwrap()
        else {
            panic!("successor was not admitted")
        };
        let next = *next;
        assert_eq!(next.publication.permit.owner, peer);
        assert_ne!(next.publication.permit.lease, read.publication.permit.lease);
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                peer,
                NetworkRequest::FinishFdRead { admission: next }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert!(!state.sched.lock().unwrap().backend_failed());
    }

    // This Guest calls the actual Global RPC and dispatcher callee with local
    // guest memory. The lower-layer table issuer is explicit component input;
    // production capability remains None and every physical injection panics.
    struct OwnedReadGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<()>,
        requests: Mutex<Vec<GlobalRequest>>,
        retired: std::sync::atomic::AtomicBool,
        pause_selected: Option<(&'a tokio::sync::Notify, &'a tokio::sync::Notify)>,
        // Opt-in in-process global view, as production local dispatch sees it.
        expose_local_global: bool,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for OwnedReadGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests.lock().unwrap().push(message.2.clone());
            let selected = matches!(
                &message.2,
                GlobalRequest::Network(NetworkRequest::BeginEmulatedReadFromRead { .. })
            );
            let reply = self
                .global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
                .await;
            if selected && let Some((arrived, resume)) = self.pause_selected {
                arrived.notify_one();
                resume.notified().await;
            }
            reply
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for OwnedReadGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;

        fn tid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<reverie::Pid> {
            None
        }

        fn local_global_state(&self) -> Option<&GlobalState> {
            self.expose_local_global.then_some(self.global)
        }

        fn memory(&self) -> Self::Memory {
            reverie::syscalls::LocalMemory::new()
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("external registration must not read guest registers")
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("external registration must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("external registration must not daemonize")
        }

        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            panic!("external registration must not inject a syscall")
        }

        async fn retire_current_thread(&mut self) -> reverie::Never {
            self.retired
                .store(true, std::sync::atomic::Ordering::SeqCst);
            futures::future::pending().await
        }

        async fn cancel_current_thread(&mut self) -> reverie::Never {
            panic!("scheduler retirement must not request group cancellation")
        }

        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> reverie::Never {
            panic!("external registration unexpectedly retired its live parent")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn set_timer_precise(
            &mut self,
            _schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("external registration must not read a host clock")
        }
    }

    fn owned_read_fixture(
        sequential: bool,
    ) -> (Config, GlobalState, Detcore, crate::ThreadState<()>) {
        use reverie::Tool;
        let (config, state) = stream_rpc_state(sequential);
        let tid = Tid::from_raw(181);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.stats.syscall_count = 17;
        *thread.file_metadata.lock().unwrap() =
            crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid);
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, owner.thread, true);
        install_test_registration(&state, owner.thread, Ivar::new());
        // Match delayed process initialization in handle_thread_start before
        // this fixture enters the real parked RPC.
        assert_eq!(thread.detpid, None);
        thread.detpid = state.sched.lock().unwrap().registered_process(owner.thread);
        assert_eq!(thread.detpid, Some(owner.thread));
        assert_eq!(
            state.registered_exec_mms.lock().unwrap().get(&owner.thread),
            Some(&owner.mm)
        );
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            engine.fd_publication_fixture_register(owner, None);
        }
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        (config, state, tool, thread)
    }

    fn owned_read_guest<'a>(
        config: &'a Config,
        state: &'a GlobalState,
        thread: crate::ThreadState<()>,
    ) -> OwnedReadGuest<'a> {
        OwnedReadGuest {
            config,
            global: state,
            thread,
            requests: Mutex::new(vec![]),
            retired: std::sync::atomic::AtomicBool::new(false),
            pause_selected: None,
            expose_local_global: false,
        }
    }

    async fn publish_owned_read_fd(
        tool: &Detcore,
        guest: &mut OwnedReadGuest<'_>,
        ty: crate::fd::FdType,
    ) -> crate::types::FdSlotBinding {
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        guest
            .thread
            .add_fd(7, nix::fcntl::OFlag::empty(), ty, None)
            .unwrap();
        {
            let mut metadata = guest.thread.file_metadata.lock().unwrap();
            let replacement = metadata.pending_network_installations()[0];
            let effect = guest
                .global
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_publication_fixture_effect(owner, replacement);
            metadata
                .associate_network_installation(replacement.installation_generation, effect)
                .unwrap();
        }
        tool.publish_network_fd_installations(guest).await.unwrap();
        guest.thread.descriptor_binding(7).unwrap()
    }

    fn legacy_replay_alarm_fixture(
        input: Option<(u64, NetworkInputKindV2)>,
    ) -> (
        Config,
        GlobalState,
        Detcore,
        crate::ThreadState<()>,
        NetworkChannelId,
    ) {
        use detcore_model::network_trace::NetworkChannelV2;
        use detcore_model::network_trace::NetworkEndpointRoleV2;
        use detcore_model::network_trace::NetworkInputEventV2;
        use detcore_model::network_trace::NetworkReleaseV2;
        use detcore_model::network_trace::NetworkTraceV2;
        use detcore_model::network_trace::NetworkTransportV2;
        use reverie::Tool;

        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Replay;
        let channel = NetworkChannelId(1);
        let epoch = LogicalTime::from_nanos(
            config
                .epoch
                .timestamp_nanos_opt()
                .expect("fixture epoch must fit") as u64,
        );
        let inputs = input
            .into_iter()
            .enumerate()
            .map(|(ordinal, (after_epoch_ns, event))| NetworkInputEventV2 {
                ordinal: ordinal as u64,
                channel,
                release: NetworkReleaseV2 {
                    not_before_global_time: epoch + LogicalTime::from_nanos(after_epoch_ns),
                    after_transmitted_offset: 0,
                },
                event,
            })
            .collect();
        let trace = NetworkTraceV2 {
            epoch: config.epoch,
            channels: vec![NetworkChannelV2 {
                id: channel,
                transport: NetworkTransportV2::Tcp,
                role: NetworkEndpointRoleV2::OutboundClient,
                local_address: None,
                peer_address: Some(NetworkAddressV2::Inet4 {
                    address: [192, 0, 2, 1],
                    port: 443,
                }),
                accepted_from: None,
            }],
            inputs,
            outputs: Vec::new(),
        };
        let mut trace_bytes = Vec::new();
        trace.write_framed(&mut trace_bytes).unwrap();
        config.network_trace_input = Some(trace_bytes);
        let state = GlobalState::initialize(&config, false);
        state
            .sched
            .lock()
            .unwrap()
            .enable_controlled_signal_delivery();
        let tid = Tid::from_raw(181);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.stats.syscall_count = 17;
        *thread.file_metadata.lock().unwrap() =
            crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid);
        let owner = NetworkStreamOwner {
            thread: thread.dettid,
            mm: thread.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, owner.thread, true);
        install_test_registration(&state, owner.thread, Ivar::new());
        thread.detpid = state.sched.lock().unwrap().registered_process(owner.thread);
        assert_eq!(thread.detpid, Some(owner.thread));
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            engine.fd_publication_fixture_register(owner, None);
        }
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        (config, state, tool, thread, channel)
    }

    async fn legacy_replay_bind_socket(
        state: &GlobalState,
        tool: &Detcore,
        guest: &mut OwnedReadGuest<'_>,
        channel: NetworkChannelId,
    ) -> OpenFileId {
        let binding = publish_owned_read_fd(tool, guest, crate::fd::FdType::Socket).await;
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .bind(binding.open_file, channel)
            .unwrap();
        binding.open_file
    }

    async fn drive_legacy_wait_callback(state: &GlobalState) -> Resources {
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await;
        assert!(
            first.is_err(),
            "the network wait must park before its alarm"
        );
        let mut prior = first;
        let mut resumed = None;
        for _ in 0..3 {
            prior = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &prior,
            )
            .await;
            if let Ok(turn) = &prior {
                resumed = Some(turn.clone());
                break;
            }
        }
        resumed.expect("the scheduler must resume the waiting syscall")
    }

    async fn drive_legacy_alarm_callback(state: &GlobalState, owner: NetworkStreamOwner) {
        let resumed = drive_legacy_wait_callback(state).await;
        assert_eq!(resumed.tid, owner.thread);
        assert!(
            resumed
                .resources
                .contains_key(&ResourceID::InboundSignal(SigWrapper::from(
                    reverie::Signal::SIGALRM
                )))
        );
    }

    fn arm_legacy_alarm(state: &GlobalState, owner: NetworkStreamOwner, after: LogicalTime) {
        let now = state.global_time.lock().unwrap().as_nanos();
        state.sched.lock().unwrap().register_alarm(
            owner.thread,
            owner.thread,
            now,
            after,
            LogicalTime::ZERO,
            reverie::Signal::SIGALRM,
        );
    }

    #[tokio::test]
    async fn legacy_replay_poll_caught_alarm_completes_the_waiting_syscall() {
        let (config, state, tool, thread, channel) = legacy_replay_alarm_fixture(None);
        let mut guest = owned_read_guest(&config, &state, thread);
        legacy_replay_bind_socket(&state, &tool, &mut guest, channel).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        arm_legacy_alarm(&state, owner, LogicalTime::from_nanos(10));
        let mut pollfd = libc::pollfd {
            fd: 7,
            events: libc::POLLIN,
            revents: 0,
        };
        let call = reverie::syscalls::Poll::new()
            .with_fds(reverie::syscalls::AddrMut::from_ptr(
                (&mut pollfd as *mut libc::pollfd).cast(),
            ))
            .with_nfds(1)
            .with_timeout(-1);
        let mut pending = std::pin::pin!(tool.handle_network_io(&mut guest, call.into()));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        drive_legacy_alarm_callback(&state, owner).await;
        let completed = tokio::time::timeout(std::time::Duration::from_millis(100), pending)
            .await
            .expect("caught alarm must complete legacy poll");
        assert!(matches!(
            completed,
            Err(reverie::Error::Errno(errno)) if errno == reverie::syscalls::Errno::EINTR
        ));
        assert_eq!(pollfd.revents, 0);
    }

    #[tokio::test]
    async fn legacy_replay_select_caught_alarm_completes_the_waiting_syscall() {
        let (config, state, tool, thread, channel) = legacy_replay_alarm_fixture(None);
        let mut guest = owned_read_guest(&config, &state, thread);
        legacy_replay_bind_socket(&state, &tool, &mut guest, channel).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        arm_legacy_alarm(&state, owner, LogicalTime::from_nanos(10));
        let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe { libc::FD_SET(7, &mut readfds) };
        let call = reverie::syscalls::Select::new()
            .with_nfds(8)
            .with_readfds(reverie::syscalls::AddrMut::from_ptr(std::ptr::addr_of_mut!(readfds)))
            .with_writefds(None)
            .with_exceptfds(None)
            .with_timeout(None);
        let mut pending = std::pin::pin!(tool.handle_network_io(&mut guest, call.into()));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        drive_legacy_alarm_callback(&state, owner).await;
        let completed = tokio::time::timeout(std::time::Duration::from_millis(100), pending)
            .await
            .expect("caught alarm must complete legacy select");
        assert!(matches!(
            completed,
            Err(reverie::Error::Errno(errno)) if errno == reverie::syscalls::Errno::EINTR
        ));
    }

    #[tokio::test]
    async fn legacy_replay_recv_caught_alarm_preserves_restart_errno() {
        let (config, state, tool, thread, channel) = legacy_replay_alarm_fixture(None);
        let mut guest = owned_read_guest(&config, &state, thread);
        legacy_replay_bind_socket(&state, &tool, &mut guest, channel).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        arm_legacy_alarm(&state, owner, LogicalTime::from_nanos(10));
        let mut byte = 0u8;
        let call = reverie::syscalls::Recvfrom::new()
            .with_fd(7)
            .with_buf(reverie::syscalls::AddrMut::from_ptr(std::ptr::addr_of_mut!(byte)))
            .with_len(1)
            .with_flags(0);
        let mut pending = std::pin::pin!(tool.handle_network_io(&mut guest, call.into()));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        drive_legacy_alarm_callback(&state, owner).await;
        let completed = tokio::time::timeout(std::time::Duration::from_millis(100), pending)
            .await
            .expect("caught alarm must complete legacy recv");
        assert!(matches!(
            completed,
            Err(reverie::Error::Errno(errno)) if errno == reverie::syscalls::Errno::ERESTARTSYS
        ));
        assert_eq!(byte, 0);
    }

    #[tokio::test]
    async fn legacy_replay_readiness_and_partial_bytes_precede_later_alarm() {
        for (event, expected) in [
            (
                NetworkInputKindV2::Readiness(detcore_model::network_trace::NetworkReadinessV2 {
                    readable: true,
                    ..Default::default()
                }),
                None,
            ),
            (
                NetworkInputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"x".to_vec(),
                },
                Some(b'x'),
            ),
        ] {
            let (config, state, tool, thread, channel) =
                legacy_replay_alarm_fixture(Some((0, event)));
            let mut guest = owned_read_guest(&config, &state, thread);
            legacy_replay_bind_socket(&state, &tool, &mut guest, channel).await;
            let owner = NetworkStreamOwner {
                thread: guest.thread.dettid,
                mm: guest.thread.mm_id,
            };
            arm_legacy_alarm(&state, owner, LogicalTime::from_nanos(1_000));
            if let Some(expected) = expected {
                let mut bytes = [0u8; 2];
                let call = reverie::syscalls::Recvfrom::new()
                    .with_fd(7)
                    .with_buf(reverie::syscalls::AddrMut::from_ptr(bytes.as_mut_ptr()))
                    .with_len(bytes.len())
                    .with_flags(0);
                assert_eq!(
                    tool.handle_network_io(&mut guest, call.into())
                        .await
                        .unwrap(),
                    1
                );
                assert_eq!(bytes, [expected, 0]);
            } else {
                let mut pollfd = libc::pollfd {
                    fd: 7,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let call = reverie::syscalls::Poll::new()
                    .with_fds(reverie::syscalls::AddrMut::from_ptr(
                        (&mut pollfd as *mut libc::pollfd).cast(),
                    ))
                    .with_nfds(1)
                    .with_timeout(-1);
                assert_eq!(
                    tool.handle_network_io(&mut guest, call.into())
                        .await
                        .unwrap(),
                    1
                );
                assert_eq!(pollfd.revents, libc::POLLIN);
            }
            assert_eq!(state.sched.lock().unwrap().turn, 0);
        }
    }

    #[tokio::test]
    async fn legacy_replay_finite_poll_timeout_precedes_later_alarm() {
        let (config, state, tool, thread, channel) = legacy_replay_alarm_fixture(None);
        let mut guest = owned_read_guest(&config, &state, thread);
        legacy_replay_bind_socket(&state, &tool, &mut guest, channel).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        arm_legacy_alarm(&state, owner, LogicalTime::from_nanos(2_000_000));
        let mut pollfd = libc::pollfd {
            fd: 7,
            events: libc::POLLIN,
            revents: 0,
        };
        let call = reverie::syscalls::Poll::new()
            .with_fds(reverie::syscalls::AddrMut::from_ptr(
                (&mut pollfd as *mut libc::pollfd).cast(),
            ))
            .with_nfds(1)
            .with_timeout(1);
        let mut pending = std::pin::pin!(tool.handle_network_io(&mut guest, call.into()));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let resumed = drive_legacy_wait_callback(&state).await;
        assert_eq!(resumed.tid, owner.thread);
        assert!(!resumed.resources.keys().any(|resource| matches!(
            resource,
            ResourceID::InboundSignal(_) | ResourceID::WaitidSignals(_)
        )));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(100), pending)
                .await
                .expect("finite timeout must complete legacy poll")
                .unwrap(),
            0
        );
        assert_eq!(pollfd.revents, 0);
    }

    async fn grant_owned_read_foreground(state: &GlobalState, guest: &mut OwnedReadGuest<'_>) {
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let request = Resources::new(owner.thread);
        let before = state.sched.lock().unwrap().turn;
        {
            let mut pending = std::pin::pin!(super::resource_request(guest, request.clone()));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert_eq!(selected.0, owner.thread);
            assert_eq!(
                selected
                    .1
                    .try_read()
                    .expect("selected request is filled")
                    .expect("selected request has not exited"),
                request
            );
            let committed = crate::scheduler::finish_selected_turn(
                state.sched.clone(),
                state.global_time.clone(),
                selected.0,
                selected.1,
                selected.2,
            )
            .await
            .unwrap();
            assert_eq!(committed, request);
            assert_eq!(pending.await, ResumeStatus::Normal);
        }
        let sched = state.sched.lock().unwrap();
        assert_eq!(sched.turn, before + 1);
        let proof = sched.ordinary_fd_observation(owner).unwrap();
        assert_eq!(proof.owner(), owner);
        assert_eq!(
            proof.resume(),
            crate::scheduler::ordinary_fd::OrdinaryFdResume::Normal
        );
    }

    #[tokio::test]
    async fn owned_read_rng_uses_real_current_grant_without_an_added_turn() {
        use reverie::Tool;
        for sequential in [true, false] {
            let (config, state, tool, thread) = owned_read_fixture(sequential);
            let mut guest = owned_read_guest(&config, &state, thread);
            let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Rng).await;
            if sequential {
                grant_owned_read_foreground(&state, &mut guest).await;
            }
            guest.requests.lock().unwrap().clear();
            let turn = state.sched.lock().unwrap().turn;
            let clock = state.global_time.lock().unwrap().as_nanos();
            let local = guest.thread.thread_logical_time.as_nanos();
            let count = guest.thread.stats.syscall_count;
            let mut bytes = [0_u8; 24];
            let call = reverie::syscalls::Read::new()
                .with_fd(7)
                .with_buf(reverie::syscalls::AddrMut::from_raw(
                    bytes.as_mut_ptr() as usize
                ))
                .with_len(24);
            assert_eq!(tool.handle_owned_read(&mut guest, call).await.unwrap(), 24);
            assert_eq!(
                guest
                    .thread
                    .with_detfd(7, |fd| fd.random_device_offset())
                    .unwrap(),
                24
            );
            assert!(guest.thread.original_connect.is_none());
            assert!(guest.thread.original_file_metadata.is_none());
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(binding.open_file),
                (0, 0, 0, 0)
            );
            assert_eq!(state.sched.lock().unwrap().turn, turn);
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
            assert_eq!(guest.thread.thread_logical_time.as_nanos(), local);
            assert_eq!(guest.thread.stats.syscall_count, count);
            {
                let requests = guest.requests.lock().unwrap();
                assert!(requests.iter().all(|r| !matches!(
                    r,
                    GlobalRequest::RequestResources(..) | GlobalRequest::ParkedRequest(..)
                )));
                assert_eq!(
                    requests
                        .iter()
                        .filter(|r| matches!(
                            r,
                            GlobalRequest::Network(NetworkRequest::BeginEmulatedReadFromRead { .. })
                        ))
                        .count(),
                    1
                );
                assert_eq!(
                    requests
                        .iter()
                        .filter(|r| matches!(
                            r,
                            GlobalRequest::Network(NetworkRequest::CompleteEmulatedRead {
                                returned: 24,
                                ..
                            })
                        ))
                        .count(),
                    1
                );
            }
            // Compare the actual legacy data mover at offset zero, not a
            // separately implemented random stream or expected native receipt.
            let mut legacy = owned_read_guest(
                &config,
                &state,
                tool.init_thread_state(Tid::from_raw(189), None),
            );
            legacy
                .thread
                .add_fd(7, nix::fcntl::OFlag::empty(), crate::fd::FdType::Rng, None)
                .unwrap();
            let mut expected = [0_u8; 24];
            assert_eq!(
                tool.handle_read(
                    &mut legacy,
                    call.with_buf(reverie::syscalls::AddrMut::from_raw(
                        expected.as_mut_ptr() as usize
                    ))
                )
                .await
                .unwrap(),
                24
            );
            assert_eq!(bytes, expected);
            assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
        }
    }

    #[tokio::test]
    async fn owned_read_rejects_a_registered_but_ungranted_foreground() {
        let (config, state, tool, thread) = owned_read_fixture(true);
        let mut guest = owned_read_guest(&config, &state, thread);
        let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Rng).await;
        let turn = state.sched.lock().unwrap().turn;
        let clock = state.global_time.lock().unwrap().as_nanos();
        let mut bytes = [0xa5_u8; 8];
        let error = tool
            .handle_owned_read(
                &mut guest,
                reverie::syscalls::Read::new()
                    .with_fd(7)
                    .with_buf(reverie::syscalls::AddrMut::from_raw(
                        bytes.as_mut_ptr() as usize
                    ))
                    .with_len(8),
            )
            .await
            .unwrap_err();
        let reverie::Error::Tool(error) = error else {
            panic!("expected exact grant protocol failure")
        };
        assert_eq!(
            error.to_string(),
            "scalar Read ownership: ordinary reader lost its real foreground grant: Phase"
        );
        assert_eq!(bytes, [0xa5; 8]);
        assert_eq!(
            guest
                .thread
                .with_detfd(7, |fd| fd.random_device_offset())
                .unwrap(),
            0
        );
        assert!(guest.thread.original_connect.is_none());
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(state.sched.lock().unwrap().turn, turn);
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
        assert!(!state.sched.lock().unwrap().backend_failed());
    }

    #[tokio::test]
    async fn owned_read_modeled_copy_keeps_old_description_after_noseq_slot_reuse() {
        use reverie::Tool;
        let (config, state, tool, thread) = owned_read_fixture(false);
        let mut guest = owned_read_guest(&config, &state, thread);
        let old = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Rng).await;
        let selected = guest.thread.with_detfd(7, |fd| fd.clone()).unwrap();
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let mut peer = tool.init_thread_state(Tid::from_raw(182), None);
        peer.file_metadata = guest.thread.file_metadata.clone();
        peer.mm_id = owner.mm;
        peer.detpid = Some(owner.thread);
        let peer_owner = NetworkStreamOwner {
            thread: peer.dettid,
            mm: peer.mm_id,
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, peer_owner.thread, false);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(peer_owner.thread, peer_owner.mm);
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .fd_publication_fixture_register(peer_owner, Some(owner));
        tool.on_thread_state_ready(Tid::from_raw(182), &state, &peer)
            .unwrap();
        state.global_time.lock().unwrap().update_global_time(
            peer_owner.thread,
            peer.thread_logical_time.as_nanos(),
            peer.thread_logical_time.inherited_nanos(),
        );
        let mut peer = owned_read_guest(&config, &state, peer);
        let arrived = tokio::sync::Notify::new();
        let resume = tokio::sync::Notify::new();
        guest.pause_selected = Some((&arrived, &resume));
        let mut bytes = [0_u8; 16];
        let replacement;
        {
            let mut pending = std::pin::pin!(
                tool.handle_owned_read(
                    &mut guest,
                    reverie::syscalls::Read::new()
                        .with_fd(7)
                        .with_len(16)
                        .with_buf(reverie::syscalls::AddrMut::from_raw(
                            bytes.as_mut_ptr() as usize
                        ))
                )
            );
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert!(futures::poll!(std::pin::pin!(arrived.notified())).is_ready());
            // The actual modeled Call owns the old OFD; neither table nor short
            // descriptor exclusion spans the paused guest-memory operation.
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(old.open_file),
                (1, 0, 0, 1)
            );
            replacement = publish_owned_read_fd(&tool, &mut peer, crate::fd::FdType::Rng).await;
            assert_ne!(replacement, old);
            assert_eq!(selected.random_device_offset(), 0);
            resume.notify_one();
            assert_eq!(pending.await.unwrap(), 16);
        }
        assert_eq!(selected.random_device_offset(), 16);
        assert_eq!(guest.thread.descriptor_binding(7).unwrap(), replacement);
        assert_eq!(
            guest
                .thread
                .with_detfd(7, |fd| fd.random_device_offset())
                .unwrap(),
            0
        );
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_capture_fixture_counts(old.open_file),
            (0, 0, 0, 0)
        );
        assert!(guest.thread.original_connect.is_none());
        assert!(!state.sched.lock().unwrap().backend_failed());
        assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
    }

    #[tokio::test]
    async fn owned_read_replacement_rebinds_only_the_required_outer_resource() {
        use reverie::Tool;
        let first = ResourceID::Device(crate::resources::Device::ContainerStdin);
        let other = ResourceID::Device(crate::resources::Device::ContainerStderr);
        for replacement_resource in [Some(first.clone()), Some(other.clone()), None] {
            let (config, state, tool, thread) = owned_read_fixture(true);
            let mut guest = owned_read_guest(&config, &state, thread);
            let old = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Rng).await;
            guest
                .thread
                .with_detfd(7, |fd| *fd = fd.clone().with_resource(first.clone()))
                .unwrap();
            let old_description = guest.thread.with_detfd(7, |fd| fd.clone()).unwrap();
            grant_owned_read_foreground(&state, &mut guest).await;
            guest.requests.lock().unwrap().clear();
            let owner = NetworkStreamOwner {
                thread: guest.thread.dettid,
                mm: guest.thread.mm_id,
            };
            let mut peer = tool.init_thread_state(Tid::from_raw(182), None);
            peer.file_metadata = guest.thread.file_metadata.clone();
            peer.mm_id = owner.mm;
            peer.detpid = Some(owner.thread);
            let peer_owner = NetworkStreamOwner {
                thread: peer.dettid,
                mm: peer.mm_id,
            };
            state.sched.lock().unwrap().thread_tree.add_child(
                owner.thread,
                peer_owner.thread,
                false,
            );
            state
                .registered_exec_mms
                .lock()
                .unwrap()
                .insert(peer_owner.thread, peer_owner.mm);
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_publication_fixture_register(peer_owner, Some(owner));
            tool.on_thread_state_ready(Tid::from_raw(182), &state, &peer)
                .unwrap();
            state.global_time.lock().unwrap().update_global_time(
                peer_owner.thread,
                peer.thread_logical_time.as_nanos(),
                peer.thread_logical_time.inherited_nanos(),
            );
            let mut peer = owned_read_guest(&config, &state, peer);
            let turn = state.sched.lock().unwrap().turn;
            let clock = state.global_time.lock().unwrap().as_nanos();
            let mut bytes = [0_u8; 8];
            let mut expected = Resources::new(owner.thread);
            expected.insert(first.clone(), Permission::R);
            let mut expected_requests = vec![expected.clone()];
            let replacement;
            {
                let mut pending = std::pin::pin!(
                    tool.handle_owned_read(
                        &mut guest,
                        reverie::syscalls::Read::new()
                            .with_fd(7)
                            .with_len(8)
                            .with_buf(reverie::syscalls::AddrMut::from_raw(
                                bytes.as_mut_ptr() as usize
                            ))
                    )
                );
                assert!(futures::poll!(pending.as_mut()).is_pending());
                assert_eq!(
                    state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .native_capture_fixture_counts(old.open_file),
                    (0, 0, 0, 0)
                );
                replacement = publish_owned_read_fd(&tool, &mut peer, crate::fd::FdType::Rng).await;
                peer.thread
                    .with_detfd(7, |fd| {
                        *fd = fd.clone().with_resource(replacement_resource.clone())
                    })
                    .unwrap();
                let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
                assert_eq!(
                    selected
                        .1
                        .try_read()
                        .expect("selected request is filled")
                        .expect("selected request has not exited"),
                    expected
                );
                assert_eq!(
                    crate::scheduler::finish_selected_turn(
                        state.sched.clone(),
                        state.global_time.clone(),
                        selected.0,
                        selected.1,
                        selected.2
                    )
                    .await
                    .unwrap(),
                    expected
                );
                if replacement_resource.as_ref() == Some(&other) {
                    assert!(futures::poll!(pending.as_mut()).is_pending());
                    let mut expected = Resources::new(owner.thread);
                    expected.insert(other.clone(), Permission::R);
                    expected_requests.push(expected.clone());
                    let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
                    assert_eq!(
                        selected
                            .1
                            .try_read()
                            .expect("selected request is filled")
                            .expect("selected request has not exited"),
                        expected
                    );
                    assert_eq!(
                        crate::scheduler::finish_selected_turn(
                            state.sched.clone(),
                            state.global_time.clone(),
                            selected.0,
                            selected.1,
                            selected.2
                        )
                        .await
                        .unwrap(),
                        expected
                    );
                }
                assert_eq!(pending.await.unwrap(), 8);
            }
            let requests = guest.requests.lock().unwrap();
            let actual: Vec<_> = requests
                .iter()
                .filter_map(|request| match request {
                    GlobalRequest::RequestResources(resources, _) => Some(resources.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(actual, expected_requests);
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| matches!(request, GlobalRequest::ReleaseAllResources))
                    .count(),
                usize::from(replacement_resource.as_ref() != Some(&first))
            );
            assert_eq!(
                state.sched.lock().unwrap().turn,
                turn + expected_requests.len() as u64
            );
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
            assert_eq!(old_description.random_device_offset(), 0);
            assert_ne!(old, replacement);
            assert_eq!(guest.thread.descriptor_binding(7).unwrap(), replacement);
            assert_eq!(
                guest
                    .thread
                    .with_detfd(7, |fd| fd.random_device_offset())
                    .unwrap(),
                8
            );
            assert!(guest.thread.original_connect.is_none());
            assert!(!state.sched.lock().unwrap().backend_failed());
        }
    }

    #[tokio::test]
    async fn owned_read_external_intent_keeps_the_existing_blocking_io_resource() {
        let (config, state, tool, thread) = owned_read_fixture(true);
        let mut guest = owned_read_guest(&config, &state, thread);
        let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Pipe).await;
        grant_owned_read_foreground(&state, &mut guest).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let operation = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count);
        let mut request = selected_external_request(&guest.thread, 7);
        request.resources.clear();
        request.insert(ResourceID::BlockingExternalIO(operation), Permission::RW);
        request.fyi = "read".to_owned();
        let before = state.sched.lock().unwrap().turn;
        let clock = state.global_time.lock().unwrap().as_nanos();
        let reply = {
            let mut pending =
                std::pin::pin!(super::fd_read_resource_request(&mut guest, request.clone()));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert_eq!(
                selected
                    .1
                    .try_read()
                    .expect("selected request is filled")
                    .expect("selected request has not exited"),
                request
            );
            assert!(matches!(
                crate::scheduler::finish_selected_turn(
                    state.sched.clone(),
                    state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await,
                Err(crate::scheduler::SkipTurn)
            ));
            pending.await
        };
        let ResourceReply::ReadGrant {
            status: ResumeStatus::Normal,
            read,
        } = reply
        else {
            panic!("missing actual external Read grant")
        };
        let read = *read;
        assert_eq!(read.binding, Some(binding));
        assert_eq!(read.external_grant, Some(operation));
        {
            let sched = state.sched.lock().unwrap();
            assert_eq!(sched.turn, before + 1);
            assert!(sched.original_fd_grant_matches(owner, operation));
            assert!(!sched.original_external_grant_matches(owner, operation));
            assert_eq!(
                sched.blocked.external_io_blockers.get(&owner.thread),
                Some(&operation)
            );
            assert!(!sched.run_queue.contains_tid(owner.thread));
            assert!(!sched.backend_failed());
        }
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
        assert!(matches!(
            super::network_request(&mut guest, NetworkRequest::FinishFdRead { admission: read })
                .await
                .unwrap(),
            NetworkReply::Unit
        ));
        assert_eq!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_capture_fixture_counts(binding.open_file),
            (0, 0, 0, 0)
        );
    }

    // Exercise the production turn adapters against real GlobalState RPC and
    // scheduler grant/harvest code. No native result is claimed by this fixture.
    #[tokio::test]
    async fn original_openat_turn_adapter_rejoins_without_capture_clock_or_invented_grant() {
        let (config, state, tool, thread) = owned_read_fixture(true);
        let mut guest = owned_read_guest(&config, &state, thread);
        grant_owned_read_foreground(&state, &mut guest).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let operation = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count);
        let turn = state.sched.lock().unwrap().turn;
        let clock = state.global_time.lock().unwrap().as_nanos();
        guest.requests.lock().unwrap().clear();
        {
            let mut pending =
                std::pin::pin!(tool.begin_original_openat_wait(&mut guest, operation));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .original_external_io_grant_matches(owner, operation)
            );
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            let requested = selected.1.try_read().unwrap().unwrap();
            assert_eq!(requested.resources.len(), 1);
            assert!(
                requested
                    .resources
                    .contains_key(&ResourceID::BlockingExternalIO(operation))
            );
            assert!(requested.fd_read.is_none());
            assert!(matches!(
                crate::scheduler::finish_selected_turn(
                    state.sched.clone(),
                    state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await,
                Err(crate::scheduler::SkipTurn)
            ));
            pending.await.unwrap();
        }
        {
            let sched = state.sched.lock().unwrap();
            assert_eq!(sched.turn, turn + 1);
            assert!(sched.original_external_io_grant_matches(owner, operation));
            assert!(!sched.original_external_grant_matches(owner, operation));
            assert!(!sched.original_external_io_grant_matches(
                owner,
                ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count + 1)
            ));
            assert!(!sched.ordinary_fd_observation(owner).is_ok());
            assert!(!sched.run_queue.contains_tid(owner.thread));
            assert!(!sched.backend_failed());
        }
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
        {
            let mut continuation =
                std::pin::pin!(tool.finish_original_openat_wait(&mut guest, operation));
            assert!(futures::poll!(continuation.as_mut()).is_pending());
            {
                let mut sched = state.sched.lock().unwrap();
                let request = sched.next_turns[&owner.thread]
                    .req
                    .try_read()
                    .unwrap()
                    .unwrap();
                assert_eq!(request.resources.len(), 1);
                assert!(
                    request
                        .resources
                        .contains_key(&ResourceID::BlockedExternalContinue(operation))
                );
                assert!(sched.original_external_io_grant_matches(owner, operation));
                assert!(sched.harvest_external_io_for_test().is_ok());
                assert!(!sched.original_external_io_grant_matches(owner, operation));
            }
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert!(
                crate::scheduler::finish_selected_turn(
                    state.sched.clone(),
                    state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await
                .is_ok()
            );
            continuation.await;
        }
        let sched = state.sched.lock().unwrap();
        assert_eq!(sched.turn, turn + 2);
        assert!(sched.ordinary_fd_observation(owner).is_ok());
        assert!(sched.blocked.external_io_blockers.is_empty());
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
        assert!(
            guest.thread.original_connect.is_none(),
            "this component did not claim native effects"
        );
    }

    #[tokio::test]
    async fn original_openat_noseq_adapter_adds_no_resource_or_capture_authority() {
        let (config, state, tool, thread) = owned_read_fixture(false);
        let mut guest = owned_read_guest(&config, &state, thread);
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let operation = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count);
        let turn = state.sched.lock().unwrap().turn;
        let clock = state.global_time.lock().unwrap().as_nanos();
        guest.requests.lock().unwrap().clear();
        tool.begin_original_openat_wait(&mut guest, operation)
            .await
            .unwrap();
        tool.finish_original_openat_wait(&mut guest, operation)
            .await;
        assert!(guest.requests.lock().unwrap().is_empty());
        assert_eq!(state.sched.lock().unwrap().turn, turn);
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .original_external_io_grant_matches(owner, operation)
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
    }

    #[tokio::test]
    async fn original_openat_external_authority_refuses_a_real_network_capture_grant() {
        let (config, state, tool, thread) = owned_read_fixture(true);
        let mut guest = owned_read_guest(&config, &state, thread);
        grant_owned_read_foreground(&state, &mut guest).await;
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let operation = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count);
        let mut resources = Resources::new(owner.thread);
        resources.insert(
            ResourceID::BlockingNetworkCapture(operation),
            Permission::RW,
        );
        {
            let mut pending = std::pin::pin!(super::resource_request(&mut guest, resources));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert!(matches!(
                crate::scheduler::finish_selected_turn(
                    state.sched.clone(),
                    state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await,
                Err(crate::scheduler::SkipTurn)
            ));
            assert_eq!(pending.await, ResumeStatus::Normal);
        }
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .original_external_grant_matches(owner, operation)
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .original_external_io_grant_matches(owner, operation)
        );
        {
            let mut continuation =
                std::pin::pin!(tool.finish_original_openat_wait(&mut guest, operation));
            assert!(futures::poll!(continuation.as_mut()).is_pending());
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .harvest_external_io_for_test()
                    .is_ok()
            );
            let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
            assert!(
                crate::scheduler::finish_selected_turn(
                    state.sched.clone(),
                    state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2
                )
                .await
                .is_ok()
            );
            continuation.await;
        }
    }

    struct RetirementGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<()>,
        requests: Mutex<Vec<GlobalRequest>>,
        retired: std::sync::atomic::AtomicBool,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for RetirementGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests.lock().unwrap().push(message.2.clone());
            self.global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
                .await
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for RetirementGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;

        fn tid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<reverie::Pid> {
            None
        }

        fn memory(&self) -> Self::Memory {
            panic!("external registration must not access guest memory")
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("external registration must not read guest registers")
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("external registration must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("external registration must not daemonize")
        }

        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            panic!("external registration must not inject a syscall")
        }

        async fn retire_current_thread(&mut self) -> reverie::Never {
            self.retired
                .store(true, std::sync::atomic::Ordering::SeqCst);
            futures::future::pending().await
        }

        async fn cancel_current_thread(&mut self) -> reverie::Never {
            panic!("scheduler retirement must not request group cancellation")
        }

        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> reverie::Never {
            panic!("external registration unexpectedly retired its live parent")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn set_timer_precise(
            &mut self,
            _schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("external registration must not read a host clock")
        }
    }

    #[tokio::test]
    async fn thread_exited_uses_natural_retirement_for_killed_stale_and_failed_rpcs() {
        use std::sync::atomic::Ordering;

        use reverie::Tool;

        for cause in ["killed", "stale image", "backend failure", "live"] {
            let (config, state, tid, pid) = cancellation_test_state();
            install_test_registration(&state, tid, Ivar::new());
            let tool: Detcore = Detcore::new(Tid::from_raw(tid.as_raw()), &config);
            let mut thread = tool.init_thread_state(Tid::from_raw(tid.as_raw()), None);
            thread.detpid = Some(pid);
            thread.thread_logical_time.add_syscall_with_cost(37);
            match cause {
                "killed" => {
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .logically_kill_thread(&tid, &pid, thread.mm_id)
                }
                "stale image" => {
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .install_test_exec_incarnation(tid, thread.mm_id);
                    thread.mm_id = thread.mm_id.for_exec(pid);
                }
                "backend failure" => state.report_backend_failure(reverie::BackendFailure {
                    pid: Tid::from_raw(pid.as_raw()),
                    tid: Tid::from_raw(tid.as_raw()),
                    phase: "natural retirement control",
                }),
                "live" => {}
                _ => unreachable!(),
            }
            let mut guest = RetirementGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(Vec::new()),
                retired: std::sync::atomic::AtomicBool::new(false),
            };
            let before = guest.thread.thread_logical_time.as_nanos();
            {
                let mut call = std::pin::pin!(super::send_and_update_time(
                    &mut guest,
                    GlobalRequest::GlobalTimeLowerBound
                ));
                if cause == "live" {
                    assert!(matches!(
                        futures::poll!(call.as_mut()),
                        std::task::Poll::Ready((_, GlobalResponse::GlobalTimeLowerBound(_)))
                    ));
                } else {
                    assert!(futures::poll!(call.as_mut()).is_pending(), "{cause}");
                }
            }
            // A terminal backend failure closes ordinary RPC admission before
            // any ThreadExited response. The backend failure subscription owns
            // cancellation of that pending callback; it must not retire as success.
            assert_eq!(
                guest.retired.load(Ordering::SeqCst),
                matches!(cause, "killed" | "stale image"),
                "{cause}"
            );
            assert_eq!(guest.requests.lock().unwrap().len(), 1);
            assert_eq!(guest.thread.thread_logical_time.as_nanos(), before);
            if cause == "backend failure" {
                assert!(state.sched.lock().unwrap().backend_failed());
                assert!(
                    futures::poll!(std::pin::pin!(state.wait_for_backend_failure())).is_ready()
                );
            }
        }
    }

    async fn check_external_child_tid_registration(
        flags: CloneFlags,
        supplied_address: usize,
        expected_address: usize,
    ) {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            // This control observes registration before the child starts; use
            // the supported parent-first order for the one turn it drives.
            runs_post_fork: crate::RunsPostFork::Parent,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let parent_pid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        install_test_registration(&state, parent, Ivar::new());
        let tool = Detcore::new(reverie::Pid::from_raw(parent.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        thread.detpid = Some(parent_pid);
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            pause_rpc: None,
        };
        let child = DetTid::from_raw(18);
        let exit_signal = if flags.contains(CloneFlags::CLONE_THREAD) {
            0
        } else {
            libc::SIGCHLD
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut registration = std::pin::pin!(tool.register_external_child(
                &mut guest,
                Tid::from_raw(child.as_raw()),
                supplied_address,
                flags,
                exit_signal,
                None,
            ));
            assert!(futures::poll!(registration.as_mut()).is_pending());
            // The production RPC parks the parent on ParentContinue. Drive
            // that actual scheduler turn instead of pre-filling a response or
            // disabling sequentialization to make registration return.
            let committed = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &Err(crate::scheduler::SkipTurn),
            )
            .await
            .expect("the parent continuation must commit");
            assert_eq!(committed.tid, parent);
            assert_eq!(
                committed.resources,
                std::collections::HashMap::from([(
                    crate::resources::ResourceID::ParentContinue { parent, child },
                    crate::resources::Permission::W,
                )]),
            );
            registration.await;
        })
        .await
        .expect("external child registration must return without running a guest");

        assert_eq!(guest.thread.clone_flags, None);
        assert_eq!(guest.thread.dettid, parent);
        let mut scheduler = state.sched.lock().unwrap();
        assert_eq!(scheduler.next_turns.len(), 2);
        assert!(scheduler.next_turns.contains_key(&parent));
        assert_eq!(
            scheduler.next_turns[&child].child_tid_addr,
            expected_address
        );
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![GlobalRequest::CreateChildThread(
                child,
                parent_pid,
                expected_address,
                Some(flags),
                exit_signal,
                None,
                Some(DEFAULT_PRIORITY),
            )],
            "external registration must send one correctly gated real RPC"
        );
        let child_pid = if flags.contains(CloneFlags::CLONE_THREAD) {
            parent_pid
        } else {
            child
        };
        let child_mm = MmId::for_clone(
            MmId::initial(parent_pid),
            child,
            flags.contains(CloneFlags::CLONE_VM),
        );
        scheduler.logically_kill_thread(&child, &child_pid, child_mm);
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(scheduler.next_turns.contains_key(&parent));
        assert_eq!(
            scheduler.child_tid_was_cleared(
                FutexID::private(child_mm, supplied_address),
                child.as_raw(),
            ),
            expected_address != 0,
            "exit must use only the registered child-TID address"
        );
        assert!(!scheduler.child_tid_was_cleared(FutexID::private(child_mm, 0), child.as_raw(),));
        assert!(!scheduler.child_tid_was_cleared(
            FutexID::private(child_mm, supplied_address),
            parent.as_raw(),
        ));
    }

    #[tokio::test]
    async fn external_registration_without_child_cleartid_disables_exit_wake() {
        for kind in [
            CloneFlags::empty(),
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND,
        ] {
            for registration in [CloneFlags::empty(), CloneFlags::CLONE_CHILD_SETTID] {
                check_external_child_tid_registration(kind | registration, 0x1234, 0).await;
            }
        }
    }

    #[tokio::test]
    async fn external_registration_with_child_cleartid_preserves_exact_exit_wake() {
        for kind in [
            CloneFlags::empty(),
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND,
        ] {
            for registration in [
                CloneFlags::CLONE_CHILD_CLEARTID,
                CloneFlags::CLONE_CHILD_CLEARTID | CloneFlags::CLONE_CHILD_SETTID,
            ] {
                check_external_child_tid_registration(kind | registration, 0x1234, 0x1234).await;
                check_external_child_tid_registration(kind | registration, 0, 0).await;
            }
        }
    }

    #[tokio::test]
    async fn set_child_tid_address_rpc_updates_and_resets_the_registration() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .get_mut(&dettid)
            .unwrap()
            .child_tid_addr = 0x1234;

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::SetChildTidAddress(0),
                ),
            )
            .await;

        assert_eq!(response.1, GlobalResponse::SetChildTidAddress(()));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .next_turns
                .get(&dettid)
                .unwrap()
                .child_tid_addr,
            0
        );
    }

    async fn child_start_clock_trajectory(
        start_before_selection: bool,
        child_first: bool,
    ) -> (Vec<LogicalTime>, Vec<DetTid>, bool) {
        let config = Config {
            sequentialize_threads: true,
            runs_post_fork: if child_first {
                RunsPostFork::Child
            } else {
                RunsPostFork::Parent
            },
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let parent_pid = parent;
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let parent_mm = MmId::initial(parent_pid);
        let child_mm = MmId::for_clone(parent_mm, child, false);
        install_test_registration(&state, parent, Ivar::new());
        let epoch = DetTime::new(&config).as_nanos();
        let mut parent_clock = DetTime::new(&config);
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(1_001));
        let child_clock = parent_clock.clone_for_child();

        // Polling the real parent RPC publishes both admission and
        // ParentContinue before it waits, just as the clone handler does.
        let mut registration = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                parent_clock.clone(),
                parent_mm,
                GlobalRequest::CreateChildThread(
                    child,
                    parent_pid,
                    0,
                    Some(CloneFlags::empty()),
                    libc::SIGCHLD,
                    None,
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        ));
        assert!(futures::poll!(&mut registration).is_pending());
        let child_request = state.sched.lock().unwrap().next_turns[&child].req.clone();
        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(child.as_raw()),
            (
                child_clock.clone(),
                child_mm,
                GlobalRequest::StartNewThread(child, child, None, None),
            ),
        ));
        if start_before_selection {
            // recv_start_new_thread yields once before filling its request.
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }

        let skipped = Err(crate::scheduler::SkipTurn);
        let mut turn = Box::pin(crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &skipped,
        ));
        if !start_before_selection && child_first {
            assert!(futures::poll!(&mut turn).is_pending());
            assert_eq!(child_request.to_string(), "<ivar HasWaiter>");
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }
        let first = turn.await.expect("first post-fork turn must commit");
        if !start_before_selection && !child_first {
            assert!(child_request.try_read().is_none());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }
        let child_resource = ResourceID::MemAddrSpace(child);
        let parent_resource = ResourceID::ParentContinue { parent, child };
        let (first_tid, first_resource, first_permission, second_resource, second_permission) =
            if child_first {
                (
                    child,
                    &child_resource,
                    Permission::RW,
                    &parent_resource,
                    Permission::W,
                )
            } else {
                (
                    parent,
                    &parent_resource,
                    Permission::W,
                    &child_resource,
                    Permission::RW,
                )
            };
        assert_eq!(first.tid, first_tid);
        assert_eq!(first.resources.len(), 1);
        assert_eq!(first.resources.get(first_resource), Some(&first_permission));
        if child_first {
            assert_eq!(
                startup.as_mut().await,
                (None, GlobalResponse::StartNewThread(None))
            );
        } else {
            assert_eq!(
                registration.as_mut().await,
                (None, GlobalResponse::CreateChildThread(None))
            );
        }
        let first_time = state.sched.lock().unwrap().committed_time;
        assert_eq!(first_time, epoch + LogicalTime::from_nanos(1_001));
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(child),
            child_clock.as_nanos()
        );

        // The selected thread's first new nanosecond and the ordinary scheduler
        // increment must both remain observable on the other thread's turn.
        let (mut first_clock, first_mm) = if child_first {
            (child_clock, child_mm)
        } else {
            (parent_clock, parent_mm)
        };
        first_clock.advance_to(first_clock.as_nanos() + LogicalTime::from_nanos(1));
        let mut resources = Resources::new(first_tid);
        resources.insert(ResourceID::MemAddrSpace(first_tid), Permission::RW);
        let mut work = Box::pin(state.receive_rpc(
            Tid::from_raw(first_tid.as_raw()),
            (
                first_clock,
                first_mm,
                GlobalRequest::RequestResources(resources, first_tid),
            ),
        ));
        assert!(futures::poll!(&mut work).is_pending());
        let second = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Ok(first),
        )
        .await
        .expect("other thread's continuation must commit");
        assert_eq!(second.resources.len(), 1);
        assert_eq!(
            second.resources.get(second_resource),
            Some(&second_permission)
        );
        if child_first {
            assert_eq!(
                registration.await,
                (None, GlobalResponse::CreateChildThread(None))
            );
        } else {
            assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
        }
        let mut scheduler = state.sched.lock().unwrap();
        let next_time = scheduler.committed_time;
        assert_eq!(next_time, epoch + LogicalTime::from_nanos(501_002));
        let queue = scheduler.run_queue.tids().copied().collect();
        let next_random = scheduler.child_runs_first_post_fork(RunsPostFork::Random);
        (vec![first_time, next_time], queue, next_random)
    }

    #[tokio::test]
    async fn child_start_clock_is_independent_of_first_rpc_arrival() {
        for child_first in [true, false] {
            assert_eq!(
                child_start_clock_trajectory(true, child_first).await,
                child_start_clock_trajectory(false, child_first).await,
            );
        }
    }

    #[tokio::test]
    async fn vfork_registration_does_not_charge_inherited_work_before_startup() {
        let (config, state, parent, parent_pid) = cancellation_test_state();
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let mm = MmId::initial(parent_pid);
        install_test_registration(&state, parent, Ivar::new());
        let mut parent_clock = DetTime::new(&config);
        let epoch = parent_clock.as_nanos();
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(1_001));
        let child_clock = parent_clock.clone_for_child();
        let mut resources = Resources::new(parent);
        resources.insert(
            ResourceID::BlockingVfork(ExternalOpId::new(parent, 1)),
            Permission::RW,
        );
        let mut blocking = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                parent_clock,
                mm,
                GlobalRequest::RequestResources(resources, parent_pid),
            ),
        ));
        assert!(futures::poll!(&mut blocking).is_pending());
        let background = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await;
        assert!(background.is_err());
        assert_eq!(
            blocking.await,
            (None, GlobalResponse::RequestResources(ResumeStatus::Normal))
        );
        let before_child = state.global_time.lock().unwrap().as_nanos();
        assert_eq!(before_child, epoch + LogicalTime::from_nanos(1_001));

        // Unlike ordinary clone, this first RPC is sent by the child itself.
        let created = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_clock.clone(),
                    mm,
                    GlobalRequest::CreateVforkChildThread(
                        parent,
                        parent_pid,
                        child,
                        0,
                        CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
                        libc::SIGCHLD,
                        Some(DEFAULT_PRIORITY - 1),
                    ),
                ),
            )
            .await;
        assert_eq!(created, (None, GlobalResponse::CreateChildThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_child);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(child),
            child_clock.as_nanos()
        );

        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(child.as_raw()),
            (
                child_clock,
                mm,
                GlobalRequest::StartNewThread(child, child, None, None),
            ),
        ));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(futures::poll!(&mut startup).is_pending());
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &background,
        )
        .await
        .expect("vfork child must receive its first turn");
        assert_eq!(first.tid, child);
        assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_child);
    }

    #[tokio::test]
    async fn first_nonstartup_rpc_counts_only_new_child_work() {
        let config = Config {
            sequentialize_threads: false,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let child = DetTid::from_raw(18);
        let mut parent_clock = DetTime::new(&config);
        let epoch = parent_clock.as_nanos();
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(101));
        let mut child_clock = parent_clock.clone_for_child();
        let _ = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    parent_clock,
                    MmId::initial(parent),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        child_clock.advance_to(child_clock.as_nanos() + LogicalTime::from_nanos(1));
        let observed = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_clock,
                    MmId::initial(child),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(
            observed,
            (
                None,
                GlobalResponse::GlobalTimeLowerBound(epoch + LogicalTime::from_nanos(102))
            )
        );
    }

    fn exec_siblings() -> (Config, GlobalState, DetTid, DetTid) {
        let (config, state, leader, _) = cancellation_test_state();
        let sibling = DetTid::from_raw(leader.as_raw() + 1);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, sibling, false);
        install_test_registration(&state, sibling, Ivar::new());
        (config, state, leader, sibling)
    }

    async fn prepare_test_rpc(
        state: &GlobalState,
        config: &Config,
        tid: DetTid,
        pid: DetPid,
    ) -> ExecFilesReceipt {
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(tid.as_raw()),
                (
                    DetTime::new(config),
                    MmId::initial(pid),
                    GlobalRequest::PrepareExec(
                        pid,
                        MmId::initial(pid),
                        FilesId::initial(tid),
                        Default::default(),
                    ),
                ),
            )
            .await;
        let GlobalResponse::PrepareExec(receipt) = response else {
            panic!("unexpected preparation {response:?}")
        };
        receipt
    }

    async fn cancel_test_rpc(state: &GlobalState, config: &Config, receipt: ExecFilesReceipt) {
        assert_eq!(
            state
                .receive_rpc(
                    Tid::from_raw(receipt.caller.as_raw()),
                    (
                        DetTime::new(config),
                        receipt.mm,
                        GlobalRequest::CancelExec(receipt),
                    )
                )
                .await,
            (None, GlobalResponse::CancelExec(()))
        );
    }

    #[tokio::test]
    #[should_panic(expected = "one caller cannot prepare exec twice")]
    async fn duplicate_exec_from_one_caller_is_an_internal_protocol_error() {
        let (config, state, leader, _) = exec_siblings();
        prepare_test_rpc(&state, &config, leader, leader).await;
        prepare_test_rpc(&state, &config, leader, leader).await;
    }

    #[tokio::test]
    async fn exec_admission_rejects_unregistered_sender_process_and_mm_before_allocation() {
        let (_, state, leader, sibling) = exec_siblings();
        let mm = MmId::initial(leader);
        let wrong_mm = mm.for_exec(leader);
        for (sender, process, envelope, asserted) in [
            (DetTid::from_raw(99), leader, mm, mm),
            (sibling, sibling, mm, mm),
            (leader, leader, wrong_mm, mm),
            (leader, leader, wrong_mm, wrong_mm),
        ] {
            assert_eq!(
                state
                    .recv_prepare_exec(
                        sender,
                        process,
                        envelope,
                        asserted,
                        FilesId::initial(sender),
                        Default::default()
                    )
                    .await,
                GlobalResponse::ThreadExited
            );
            assert!(state.pending_exec_states.lock().unwrap().is_empty());
        }
        let expected = ExecFilesReceipt {
            caller: leader,
            process: leader,
            mm,
            old_files: FilesId::initial(leader),
            new_files: FilesIdAllocator::default().allocate_exec(leader),
        };
        assert_eq!(
            state
                .recv_prepare_exec(
                    leader,
                    leader,
                    mm,
                    mm,
                    expected.old_files,
                    Default::default()
                )
                .await,
            GlobalResponse::PrepareExec(expected)
        );
    }

    #[tokio::test]
    async fn sibling_exec_waits_without_locks_and_stale_cancel_cannot_release_retry() {
        let (config, state, leader, sibling) = exec_siblings();
        let first = prepare_test_rpc(&state, &config, leader, leader).await;
        let waiting = prepare_test_rpc(&state, &config, sibling, leader);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        assert!(state.sched.try_lock().is_ok());
        assert!(state.pending_exec_states.try_lock().is_ok());
        // A notification without any state transition must not admit a sibling.
        state.exec_preparation_changed.notify_waiters();
        assert!(futures::poll!(&mut waiting).is_pending());
        cancel_test_rpc(&state, &config, first).await;
        let second = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancellation must wake the registered sibling");
        assert_ne!(first.new_files, second.new_files);
        assert_eq!(
            state.pending_exec_states.lock().unwrap()[&leader].receipt,
            second
        );
        cancel_test_rpc(&state, &config, first).await;
        assert_eq!(
            state.pending_exec_states.lock().unwrap()[&leader].receipt,
            second
        );
        cancel_test_rpc(&state, &config, second).await;
        let third = prepare_test_rpc(&state, &config, leader, leader).await;
        let mut expected = FilesIdAllocator::default();
        assert_eq!(first.new_files, expected.allocate_exec(leader));
        assert_eq!(second.new_files, expected.allocate_exec(sibling));
        assert_eq!(third.new_files, expected.allocate_exec(leader));
    }

    #[tokio::test]
    async fn canceled_exec_override_update_cannot_mutate_the_new_attempt() {
        let (config, state, leader, _) = exec_siblings();
        let first = prepare_test_rpc(&state, &config, leader, leader).await;
        cancel_test_rpc(&state, &config, first).await;
        let second = prepare_test_rpc(&state, &config, leader, leader).await;
        for (receipt, overrides, accepted) in [
            (second, BTreeSet::from([7]), true),
            (first, BTreeSet::from([99]), false),
        ] {
            assert_eq!(
                state
                    .receive_rpc(
                        Tid::from_raw(leader.as_raw()),
                        (
                            DetTime::new(&config),
                            receipt.mm,
                            GlobalRequest::UpdateExecFdBlocking(receipt, overrides),
                        )
                    )
                    .await,
                (None, GlobalResponse::UpdateExecFdBlocking(accepted))
            );
        }
        let pending = state.pending_exec_states.lock().unwrap();
        assert_eq!(pending[&leader].receipt, second);
        assert_eq!(pending[&leader].fd_blocking, BTreeSet::from([7]));
    }

    #[tokio::test]
    async fn successful_exec_wakes_and_revalidates_waiting_sibling() {
        let (config, state, leader, sibling) = exec_siblings();
        let receipt = prepare_test_rpc(&state, &config, leader, leader).await;
        let waiting = state.recv_prepare_exec(
            sibling,
            leader,
            receipt.mm,
            receipt.mm,
            FilesId::initial(sibling),
            Default::default(),
        );
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        let completed = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    DetTime::new(&config),
                    receipt.mm.for_exec(leader),
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert_eq!(
            completed,
            (
                None,
                GlobalResponse::MarkPastFirstExecve(Default::default(), Some(receipt))
            )
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap(),
            GlobalResponse::ThreadExited
        );
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(
            state.registered_exec_mms.lock().unwrap().get(&leader),
            Some(&receipt.mm.for_exec(leader))
        );
        assert!(
            !state
                .registered_exec_mms
                .lock()
                .unwrap()
                .contains_key(&sibling)
        );
    }

    #[tokio::test]
    async fn stale_deregistration_after_retained_exec_keeps_current_table_receipt() {
        let (config, state, leader, _) = exec_siblings();
        let first = prepare_test_rpc(&state, &config, leader, leader).await;
        let current_mm = first.mm.for_exec(leader);
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    DetTime::new(&config),
                    current_mm,
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert_eq!(
            response,
            GlobalResponse::MarkPastFirstExecve(Default::default(), Some(first))
        );
        let response = state
            .recv_prepare_exec(
                leader,
                leader,
                current_mm,
                current_mm,
                first.new_files,
                Default::default(),
            )
            .await;
        let GlobalResponse::PrepareExec(second) = response else {
            panic!("{response:?}")
        };
        let stale_time = state.global_time.lock().unwrap().as_nanos();
        let mut advanced_stale_clock = DetTime::new(&config);
        advanced_stale_clock.advance_to(stale_time + LogicalTime::from_nanos(99));
        assert_eq!(
            state
                .receive_rpc(
                    Tid::from_raw(leader.as_raw()),
                    (
                        advanced_stale_clock,
                        first.mm,
                        GlobalRequest::ReportUnsupportedSyscall("stale-image-effect".into()),
                    )
                )
                .await,
            (None, GlobalResponse::ThreadExited)
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), stale_time);
        assert!(
            !state
                .unsupported_syscalls
                .lock()
                .unwrap()
                .contains("stale-image-effect")
        );
        state
            .recv_deregister_thread(
                Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    dettid: leader,
                    detpid: leader,
                    mm: first.mm,
                    thread_start_entered: true,
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert_eq!(
            state.registered_exec_mms.lock().unwrap().get(&leader),
            Some(&current_mm)
        );
        assert_eq!(
            state.pending_exec_states.lock().unwrap()[&leader].receipt,
            second
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(leader)
        );
    }

    #[tokio::test]
    async fn exec_owner_exit_and_backend_failure_release_registered_waiters() {
        for backend_failure in [false, true] {
            let (config, state, leader, sibling) = exec_siblings();
            let receipt = prepare_test_rpc(&state, &config, sibling, leader).await;
            let waiting = state.recv_prepare_exec(
                leader,
                leader,
                receipt.mm,
                receipt.mm,
                FilesId::initial(leader),
                Default::default(),
            );
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
            if backend_failure {
                state.report_backend_failure(reverie::BackendFailure {
                    pid: Tid::from_raw(leader.as_raw()),
                    tid: Tid::from_raw(sibling.as_raw()),
                    phase: "exec contention test",
                });
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(1), waiting)
                        .await
                        .unwrap(),
                    GlobalResponse::ThreadExited
                );
            } else {
                state
                    .recv_deregister_thread(
                        Tid::from_raw(sibling.as_raw()),
                        ThreadDeregistration {
                            dettid: sibling,
                            detpid: leader,
                            mm: receipt.mm,
                            thread_start_entered: true,
                            timeslice_stats: TimesliceStats::default(),
                            syscall_count: 0,
                            chaos_epochs: Vec::new(),
                        },
                    )
                    .await;
                let response = tokio::time::timeout(Duration::from_secs(1), waiting)
                    .await
                    .unwrap();
                let GlobalResponse::PrepareExec(next) = response else {
                    panic!("live sibling must remain eligible: {response:?}")
                };
                assert_eq!(next.caller, leader);
                assert_ne!(next.new_files, receipt.new_files);
            }
        }
    }

    #[tokio::test]
    async fn exec_reconnect_retains_inherited_work_accounting_across_local_reload() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let ancestor = DetTid::from_raw(leader.as_raw() - 1);
        let worker = DetTid::from_raw(leader.as_raw() + 1);
        let old_mm = MmId::initial(detpid);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, worker, false);
        install_test_registration(&state, worker, Ivar::new());

        let mut ancestor_clock = DetTime::new(&config);
        let epoch = ancestor_clock.as_nanos();
        ancestor_clock.advance_to(epoch + LogicalTime::from_nanos(1_000));
        let mut leader_clock = ancestor_clock.clone_for_child();
        leader_clock.advance_to(epoch + LogicalTime::from_nanos(1_100));
        let mut worker_clock = leader_clock.clone_for_child();
        worker_clock.advance_to(epoch + LogicalTime::from_nanos(1_350));
        for (tid, clock) in [
            (ancestor, ancestor_clock),
            (leader, leader_clock),
            (worker, worker_clock.clone()),
        ] {
            let _ = state
                .receive_rpc(
                    Tid::from_raw(tid.as_raw()),
                    (clock, old_mm, GlobalRequest::GlobalTimeLowerBound),
                )
                .await;
        }
        let total = state.global_time.lock().unwrap().as_nanos();
        assert_eq!(total, epoch + LogicalTime::from_nanos(1_350));
        let mut expected_allocator = FilesIdAllocator::default();
        let first_files = ExecFilesReceipt {
            caller: worker,
            process: detpid,
            mm: old_mm,
            old_files: FilesId::initial(worker),
            new_files: expected_allocator.allocate_exec(worker),
        };
        // A failed exec cancels its pending transfer without changing either
        // inherited component. The next successful attempt must use the same
        // clocks, not charge either component's inherited work again.
        let first_prepared = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(
                        detpid,
                        old_mm,
                        FilesId::initial(worker),
                        Default::default(),
                    ),
                ),
            )
            .await;
        assert_eq!(
            first_prepared,
            (None, GlobalResponse::PrepareExec(first_files))
        );
        let cancelled = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::CancelExec(first_files),
                ),
            )
            .await;
        assert_eq!(cancelled, (None, GlobalResponse::CancelExec(())));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(worker),
            worker_clock.as_nanos()
        );

        let prepared = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(
                        detpid,
                        old_mm,
                        FilesId::initial(worker),
                        Default::default(),
                    ),
                ),
            )
            .await;
        let expected_files = ExecFilesReceipt {
            new_files: expected_allocator.allocate_exec(worker),
            ..first_files
        };
        assert_eq!(
            prepared,
            (None, GlobalResponse::PrepareExec(expected_files))
        );

        let mut fresh = DetTime::new(&config);
        let recreated = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    fresh.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            recreated,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::CreateChildThread(Some((old_mm.for_exec(detpid), expected_files)))
            )
        );
        // A delayed request from the destroyed image must be rejected before
        // its absolute clock or its inherited metadata reaches accounting.
        let stale = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::RequestResources(Resources::new(leader), detpid),
                ),
            )
            .await;
        assert_eq!(stale, (None, GlobalResponse::ThreadExited));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);

        fresh.advance_to(recreated.0.unwrap());
        assert_eq!(fresh.inherited_nanos(), LogicalTime::ZERO);
        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(leader.as_raw()),
            (
                fresh.clone(),
                old_mm.for_exec(detpid),
                GlobalRequest::StartNewThread(leader, detpid, None, None),
            ),
        ));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(futures::poll!(&mut startup).is_pending());
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .expect("replacement leader must run");
        assert_eq!(first.tid, leader);
        assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);

        fresh.advance_to(fresh.as_nanos() + LogicalTime::from_nanos(1));
        let observed = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    fresh.clone(),
                    old_mm.for_exec(detpid),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(
            observed,
            (
                None,
                GlobalResponse::GlobalTimeLowerBound(total + LogicalTime::from_nanos(1))
            )
        );
        let global = state.global_time.lock().unwrap();
        assert_eq!(global.threads_time(leader), fresh.as_nanos());
        assert!(!global.contains_thread(worker));
    }

    #[test]
    fn live_registration_without_next_turn_is_not_terminal() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state.sched.lock().unwrap().next_turns.remove(&dettid);

        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(dettid),
            "transient next-turn absence must not imply logical death"
        );

        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(dettid),
            "explicit logical death must install a permanent TID tombstone"
        );
    }

    #[tokio::test]
    async fn exec_reconnect_retires_siblings_and_reuses_live_scheduler_and_clock_state() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        install_test_registration(&state, dettid, Ivar::new());
        let sibling = DetTid::from_raw(dettid.as_raw() + 1);
        let sibling_request = Ivar::new();
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, sibling, false);
        install_test_registration(&state, sibling, sibling_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.next_turns.get_mut(&dettid).unwrap().resp =
                Ivar::full(SchedResponse::Go(None));
            scheduler
                .next_turns
                .get_mut(&sibling)
                .unwrap()
                .child_tid_addr = 0x1234;
        }
        let mut existing_time = DetTime::new(&config);
        existing_time.add_syscall();
        existing_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            existing_time.as_nanos(),
            LogicalTime::ZERO,
        );
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let fresh_local_time = DetTime::new(&config);
        let physical_pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let physical_ids = Some((physical_pid, physical_tid));
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                receipt: ExecFilesReceipt {
                    caller: dettid,
                    process: detpid,
                    mm: old_mm,
                    old_files: FilesId::initial(dettid),
                    new_files: state
                        .exec_files_allocator
                        .lock()
                        .unwrap()
                        .allocate_exec(dettid),
                },
                fd_blocking: Default::default(),
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(dettid, old_mm);
        let in_flight_exec_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    existing_time.clone(),
                    old_mm.for_exec(detpid),
                    GlobalRequest::ReportUnsupportedSyscall("exec-in-flight".to_owned()),
                ),
            )
            .await;
        assert_eq!(
            in_flight_exec_response.1,
            GlobalResponse::ReportUnsupportedSyscall(())
        );

        let expected_files = state.pending_exec_states.lock().unwrap()[&detpid].receipt;
        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::CreateChildThread(
                        dettid,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        physical_ids,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response,
            (
                Some(thread_before),
                GlobalResponse::CreateChildThread(Some((old_mm.for_exec(detpid), expected_files)))
            )
        );
        assert_eq!(
            state.sched.lock().unwrap().physical_thread_identity(dettid),
            Some((old_mm.for_exec(detpid), physical_pid, physical_tid)),
            "post-exec host identity must be installed by CreateChildThread before admission"
        );

        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid, physical_ids, None),
                ),
            )
            .await;
        assert_eq!(
            start_response,
            (Some(thread_before), GlobalResponse::StartNewThread(None))
        );

        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_is_logically_killed(dettid));
        assert!(scheduler.thread_is_logically_killed(sibling));
        assert_eq!(scheduler.next_turns.len(), 1);
        assert!(matches!(sibling_request.try_read(), Some(Err(_))));
        assert!(
            scheduler.child_tid_was_cleared(FutexID::private(old_mm, 0x1234), sibling.as_raw())
        );
        drop(scheduler);
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn nonleader_exec_rebinds_caller_to_leader_and_preserves_its_clock() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let worker = DetTid::from_raw(leader.as_raw() + 1);
        let sibling = DetTid::from_raw(leader.as_raw() + 2);
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        let leader_request = Ivar::new();
        let worker_request = Ivar::new();
        let sibling_request = Ivar::new();
        install_test_registration(&state, leader, leader_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.thread_tree.add_child(leader, worker, false);
            scheduler.thread_tree.add_child(leader, sibling, false);
        }
        install_test_registration(&state, worker, worker_request.clone());
        install_test_registration(&state, sibling, sibling_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler
                .next_turns
                .get_mut(&sibling)
                .unwrap()
                .child_tid_addr = 0x5678;
            scheduler
                .timeslices
                .insert(leader, Some(LogicalTime::from_nanos(99)));
            scheduler.install_test_vfork_barrier(leader, sibling);
        }

        let mut leader_clock = DetTime::new(&config);
        leader_clock.add_syscall();
        let mut worker_clock = DetTime::new(&config);
        worker_clock.add_syscall();
        worker_clock.add_syscall();
        worker_clock.add_syscall();
        {
            let mut global_time = state.global_time.lock().unwrap();
            global_time.update_global_time(leader, leader_clock.as_nanos(), LogicalTime::ZERO);
            global_time.update_global_time(worker, worker_clock.as_nanos(), LogicalTime::ZERO);
        }
        let total_before = state.global_time.lock().unwrap().as_nanos();
        let fd_blocking: ExecFdBlockingOverrides = [42].into_iter().collect();
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                receipt: ExecFilesReceipt {
                    caller: worker,
                    process: detpid,
                    mm: old_mm,
                    old_files: FilesId::initial(worker),
                    new_files: state
                        .exec_files_allocator
                        .lock()
                        .unwrap()
                        .allocate_exec(worker),
                },
                fd_blocking: fd_blocking.clone(),
            },
        );
        let fresh_local_time = DetTime::new(&config);

        let expected_files = state.pending_exec_states.lock().unwrap()[&detpid].receipt;
        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::CreateChildThread(Some((old_mm.for_exec(detpid), expected_files)))
            )
        );
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(
            state.post_exec_fd_blocking.lock().unwrap().get(&leader),
            Some(&fd_blocking)
        );

        let late_old_request = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    leader_clock.clone(),
                    old_mm,
                    GlobalRequest::RequestResources(Resources::new(leader), detpid),
                ),
            )
            .await;
        assert_eq!(late_old_request, (None, GlobalResponse::ThreadExited));
        let admitted_before_fence = state
            .recv_request_resources(
                reverie::Tid::from_raw(leader.as_raw()),
                detpid,
                Resources::new(leader),
                Some(old_mm),
            )
            .await;
        assert_eq!(
            admitted_before_fence,
            (SchedulerRpcResult::ThreadExited, None)
        );
        let late_old_deregister = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    leader_clock.clone(),
                    old_mm,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid: leader,
                        detpid,
                        mm: old_mm,
                        timeslice_stats: TimesliceStats::default(),
                        syscall_count: 0,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(
            late_old_deregister,
            (None, GlobalResponse::DeregisterThread(()))
        );
        let duplicate_create = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(duplicate_create, (None, GlobalResponse::ThreadExited));

        {
            let scheduler = state.sched.lock().unwrap();
            assert!(!scheduler.thread_is_logically_killed(leader));
            assert!(scheduler.thread_is_logically_killed(worker));
            assert!(scheduler.thread_is_logically_killed(sibling));
            assert_eq!(scheduler.next_turns.len(), 1);
            assert!(scheduler.next_turns.contains_key(&leader));
            assert!(!scheduler.timeslices.contains_key(&leader));
            assert!(!scheduler.vfork_barrier_mentions(leader));
            assert!(!scheduler.vfork_barrier_mentions(sibling));
            assert!(matches!(leader_request.try_read(), Some(Err(_))));
            assert!(matches!(worker_request.try_read(), Some(Err(_))));
            assert!(matches!(sibling_request.try_read(), Some(Err(_))));
            assert!(
                scheduler
                    .child_tid_was_cleared(FutexID::private(old_mm, 0x5678), sibling.as_raw(),)
            );
        }

        // Drive the real daemon through step2 rather than manually pre-filling
        // the replacement's response. This drains the destroyed leader's
        // physical removal and the same-raw-TID replacement admission before
        // StartNewThread supplies the fresh image's first request.
        let turn_sched = state.sched.clone();
        let turn_time = state.global_time.clone();
        let turn = tokio::spawn(async move {
            let last: Result<Resources, crate::scheduler::SkipTurn> =
                Err(crate::scheduler::SkipTurn);
            crate::scheduler::do_a_turn_blocking(turn_sched, turn_time, &last).await
        });
        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(leader, detpid, None, None),
                ),
            )
            .await;
        assert!(
            turn.await
                .expect("replacement scheduler turn panicked")
                .is_ok(),
            "replacement leader did not survive the first step2 drain"
        );
        assert_eq!(
            start_response,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::StartNewThread(None)
            )
        );
        {
            let scheduler = state.sched.lock().unwrap();
            assert_eq!(
                scheduler
                    .run_queue
                    .tids()
                    .filter(|dettid| **dettid == leader)
                    .count(),
                1
            );
            assert!(!scheduler.run_queue.contains_tid(worker));
            assert!(!scheduler.run_queue.contains_tid(sibling));
        }
        {
            let global_time = state.global_time.lock().unwrap();
            assert_eq!(global_time.as_nanos(), total_before);
            assert_eq!(global_time.threads_time(leader), worker_clock.as_nanos());
            assert!(!global_time.contains_thread(worker));
        }

        let mark_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    worker_clock,
                    old_mm.for_exec(detpid),
                    GlobalRequest::MarkPastFirstExecve(None),
                ),
            )
            .await;
        assert_eq!(
            mark_response.1,
            GlobalResponse::MarkPastFirstExecve(fd_blocking, Some(expected_files))
        );
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_exec_clears_prepared_state_without_retiring_siblings() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let sibling = DetTid::from_raw(leader.as_raw() + 1);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, sibling, false);
        install_test_registration(&state, sibling, Ivar::new());
        let clock = DetTime::new(&config);

        let prepared = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock.clone(),
                    MmId::initial(leader),
                    GlobalRequest::PrepareExec(
                        detpid,
                        MmId::initial(detpid),
                        FilesId::initial(leader),
                        Default::default(),
                    ),
                ),
            )
            .await;
        let expected_files = ExecFilesReceipt {
            caller: leader,
            process: detpid,
            mm: MmId::initial(detpid),
            old_files: FilesId::initial(leader),
            new_files: FilesIdAllocator::default().allocate_exec(leader),
        };
        assert_eq!(prepared.1, GlobalResponse::PrepareExec(expected_files));
        assert!(
            state
                .pending_exec_states
                .lock()
                .unwrap()
                .contains_key(&detpid)
        );

        let cancelled = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock,
                    MmId::initial(leader),
                    GlobalRequest::CancelExec(expected_files),
                ),
            )
            .await;
        assert_eq!(cancelled.1, GlobalResponse::CancelExec(()));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        {
            let scheduler = state.sched.lock().unwrap();
            assert!(!scheduler.thread_is_logically_killed(leader));
            assert!(!scheduler.thread_is_logically_killed(sibling));
            assert_eq!(scheduler.next_turns.len(), 2);
        }

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                receipt: ExecFilesReceipt {
                    caller: leader,
                    process: detpid,
                    mm: MmId::initial(detpid),
                    old_files: FilesId::initial(leader),
                    new_files: state
                        .exec_files_allocator
                        .lock()
                        .unwrap()
                        .allocate_exec(leader),
                },
                fd_blocking: Default::default(),
            },
        );
        state
            .post_exec_fd_blocking
            .lock()
            .unwrap()
            .insert(leader, [42].into_iter().collect());
        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                receipt: ExecFilesReceipt {
                    caller: leader,
                    process: detpid,
                    mm: MmId::initial(detpid),
                    old_files: FilesId::initial(leader),
                    new_files: state
                        .exec_files_allocator
                        .lock()
                        .unwrap()
                        .allocate_exec(leader),
                },
                fd_blocking: Default::default(),
            },
        );
        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid).for_exec(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                receipt: ExecFilesReceipt {
                    caller: leader,
                    process: detpid,
                    mm: MmId::initial(detpid),
                    old_files: FilesId::initial(leader),
                    new_files: state
                        .exec_files_allocator
                        .lock()
                        .unwrap()
                        .allocate_exec(leader),
                },
                fd_blocking: Default::default(),
            },
        );
        state
            .post_exec_fd_blocking
            .lock()
            .unwrap()
            .insert(leader, [42].into_iter().collect());
        state.complete_physical_process_exit(detpid.as_raw());
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
    }

    #[test]
    fn unsupported_syscall_report_duplicate_is_close_on_exec() {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 initialized both descriptors and transfers ownership.
        let _reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let config = Config {
            unsupported_syscall_report_fd: Some(writer.as_raw_fd()),
            ..Config::default()
        };

        let state = GlobalState::initialize(&config, false);
        let duplicate = state
            .unsupported_syscall_report_fd
            .as_ref()
            .expect("report writer should be duplicated")
            .lock()
            .unwrap();
        let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping test.
    #[test]
    fn device_pool_remaps_deterministically() {
        use super::DevicePool;

        // Raw device numbers a guest might observe; the procfs/tmpfs ones
        // (large anon-bdev values) are exactly what drifts between runs.
        let raw_root = 0x20; // e.g. a real block device
        let raw_proc_run1 = 3_145_792; // anon bdev in run 1
        let raw_proc_run2 = 3_145_788; // same procfs, different number in run 2

        // Run 1: observe root then proc.
        let mut pool1 = DevicePool::new();
        let root1 = pool1.determinize(raw_root);
        let proc1 = pool1.determinize(raw_proc_run1);
        // Run 2: same observation order, different raw proc number.
        let mut pool2 = DevicePool::new();
        let root2 = pool2.determinize(raw_root);
        let proc2 = pool2.determinize(raw_proc_run2);

        // The synthetic ids depend only on first-observation order, so they are
        // identical across the two runs despite the raw proc number differing.
        assert_eq!(root1, root2);
        assert_eq!(proc1, proc2);

        // Distinct raw devices get distinct ids; ids start at 1 (never 0).
        assert_ne!(root1, proc1);
        assert_eq!(root1, 1);
        assert_eq!(proc1, 2);

        // Re-observing a raw device is stable within a run.
        assert_eq!(pool1.determinize(raw_root), root1);
        assert_eq!(pool1.determinize(raw_proc_run1), proc1);
    }

    #[test]
    fn mountinfo_prepopulation_is_reused_by_later_stat_observations() {
        use super::DevicePool;

        let mountinfo_devices = [libc::makedev(8, 1), libc::makedev(0, 44)];
        let mut pool = DevicePool::new();
        let rendered = mountinfo_devices
            .into_iter()
            .map(|raw| pool.determinize(raw))
            .collect::<Vec<_>>();

        assert_eq!(rendered, [1, 2]);
        assert_eq!(pool.determinize(libc::makedev(0, 44)), rendered[1]);
        assert_eq!(pool.determinize(libc::makedev(8, 1)), rendered[0]);
        assert_eq!(pool.determinize(libc::makedev(259, 7)), 3);
    }

    #[tokio::test]
    async fn late_futex_rpc_after_thread_removal_returns_eintr() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        let response = state
            .recv_futex_action(
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                FutexAction::WaitRequest(None),
                FutexID::private(MmId::initial(detpid), 0x1000),
                0,
                u32::MAX,
            )
            .await;

        assert!(matches!(
            response,
            Some(SchedValue::Value(value)) if value == nix::errno::Errno::EINTR as u64
        ));
    }

    #[tokio::test]
    async fn late_child_tid_wait_after_exit_returns_spurious_wake() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let detpid = DetPid::from_raw(17);
        let child = DetTid::from_raw(18);
        let futex = FutexID::private(MmId::initial(detpid), 0x1000);
        state.sched.lock().unwrap().next_turns.insert(
            detpid,
            ThreadNextTurn {
                dettid: detpid,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .wake_futex_child_cleartid(futex, child);
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .child_tid_was_cleared(futex, child.as_raw())
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .child_tid_was_cleared(futex, child.as_raw() + 1)
        );

        let response = state
            .recv_futex_action(
                RpcIncarnation {
                    dettid: detpid,
                    mm: MmId::initial(detpid),
                },
                FutexAction::WaitRequest(None),
                futex,
                child.as_raw(),
                u32::MAX,
            )
            .await;

        assert!(matches!(response, Some(SchedValue::Value(0))));
        assert!(state.sched.lock().unwrap().blocked.futex_waiters.is_empty());
    }

    #[tokio::test]
    async fn late_resource_request_after_logical_kill_is_cancelled() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let mut current_time = DetTime::new(&config);
        current_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            current_time.as_nanos(),
            LogicalTime::ZERO,
        );
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let mut late_time = current_time;
        late_time.add_syscall();

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::RequestResources(Resources::new(dettid), detpid),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        assert!(!state.sched.lock().unwrap().next_turns.contains_key(&dettid));
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn duplicate_deregistration_is_acknowledged_without_clock_or_scheduler_mutation() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let mut current_time = DetTime::new(&config);
        current_time.add_syscall();
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            current_time.as_nanos(),
            LogicalTime::ZERO,
        );
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let mut late_time = current_time;
        late_time.add_syscall();
        let mut final_stats = TimesliceStats::default();
        final_stats.record(7);

        let first_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        syscall_count: 17,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(first_response, (None, GlobalResponse::DeregisterThread(())));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .per_thread_timeslice
                .get(&dettid),
            Some(&final_stats)
        );
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&dettid),
            Some(&17)
        );

        late_time.add_syscall();
        let duplicate_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        syscall_count: 99,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(
            duplicate_response,
            (None, GlobalResponse::DeregisterThread(()))
        );
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .per_thread_timeslice
                .get(&dettid),
            Some(&final_stats)
        );
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&dettid),
            Some(&17),
            "a duplicate deregistration must not double-count or replace final accounting"
        );
        let summary = state
            .sched
            .lock()
            .unwrap()
            .generate_partial_run_summary(None)
            .unwrap();
        assert_eq!(summary.syscalls, Some(17));
        assert!(!state.sched.lock().unwrap().next_turns.contains_key(&dettid));
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn child_registration_fails_closed_for_a_tombstoned_tid() {
        let (config, state, parent, detpid) = cancellation_test_state();
        install_test_registration(&state, parent, Ivar::new());
        let child = DetTid::from_raw(18);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, child, false);
        install_test_registration(&state, child, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&child, &detpid, MmId::initial(detpid));

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(parent),
                    GlobalRequest::CreateChildThread(
                        child,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        let scheduler = state.sched.lock().unwrap();
        assert!(scheduler.thread_is_logically_killed(child));
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(!scheduler.priorities.contains_key(&child));
        assert!(!state.global_time.lock().unwrap().contains_thread(parent));
    }

    #[tokio::test]
    async fn pending_resource_request_woken_by_logical_kill_is_terminal() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());

        let request = state.recv_request_resources(
            reverie::Tid::from_raw(dettid.as_raw()),
            detpid,
            Resources::new(dettid),
            None,
        );
        let kill_after_request = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_request);
        assert_eq!(response, (SchedulerRpcResult::ThreadExited, None));
    }

    #[tokio::test]
    async fn trace_replay_yield_propagates_terminal_scheduler_cancellation() {
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        let next_tid = DetTid::from_raw(18);
        let event = SchedEvent {
            dettid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: Some(LogicalTime::from_nanos(1)),
        };
        let next_event = SchedEvent {
            dettid: next_tid,
            end_time: Some(LogicalTime::from_nanos(2)),
            ..event.clone()
        };
        let trace_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            trace_file.path(),
            PreemptionRecord::from_sched_events(vec![event.clone(), next_event]).to_string(),
        )
        .unwrap();
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            replay_schedule_from: Some(trace_file.path().to_path_buf()),
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, next_tid, false);
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());
        install_test_registration(&state, next_tid, Ivar::new());

        let replay = state.recv_trace_schedevent(event, detpid, MmId::initial(detpid), false);
        let kill_after_replay_yield = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };
        let (response, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(replay, kill_after_replay_yield)
        })
        .await
        .expect("trace replay cancellation did not terminate the pending scheduler RPC");

        assert_eq!(response, SchedulerRpcResult::ThreadExited);
    }

    #[tokio::test]
    async fn pending_start_request_woken_by_logical_kill_is_terminal() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());
        let request = state.receive_rpc(
            reverie::Tid::from_raw(dettid.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(dettid),
                GlobalRequest::StartNewThread(dettid, detpid, None, None),
            ),
        );
        let kill_after_request = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_request);
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        assert!(!state.sched.lock().unwrap().priorities.contains_key(&dettid));
    }

    #[tokio::test]
    async fn required_physical_thread_id_missing_is_terminal() {
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            backend_requires_thread_directed_process_signals: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        install_test_registration(&state, dettid, Ivar::new());

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid, None, None),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.next_turns.contains_key(&dettid));
    }

    #[tokio::test]
    async fn tombstoned_timer_registration_cannot_mutate_scheduler_state() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let now = LogicalTime::from_nanos(100);

        let alarm = state
            .recv_register_alarm(
                detpid,
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                now,
                LogicalTime::from_nanos(10),
                LogicalTime::ZERO,
                SigWrapper::from(Signal::SIGALRM),
            )
            .await;
        assert_eq!(alarm, SchedulerRpcResult::ThreadExited);

        let posix = state
            .recv_register_posix_timer(
                detpid,
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                1,
                Some(now + LogicalTime::from_nanos(10)),
                LogicalTime::ZERO,
                SigWrapper::from(Signal::SIGALRM),
            )
            .await;
        assert_eq!(posix, SchedulerRpcResult::ThreadExited);
        assert!(state.sched.lock().unwrap().blocked.timed_waiters.is_empty());
    }

    #[tokio::test]
    async fn pending_futex_request_woken_by_logical_kill_is_terminal() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let request = state.receive_rpc(
            reverie::Tid::from_raw(dettid.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(dettid),
                GlobalRequest::FutexAction(
                    dettid,
                    FutexAction::WaitRequest(None),
                    FutexID::private(MmId::initial(detpid), 0x1000),
                    0,
                    u32::MAX,
                ),
            ),
        );
        let kill_after_wait = async {
            while state.sched.lock().unwrap().blocked.futex_waiters.is_empty() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(request, kill_after_wait)
        })
        .await
        .expect("futex teardown did not wake the blocked RPC");
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
    }

    #[tokio::test]
    async fn parent_continue_propagates_terminal_scheduler_cancellation() {
        let (config, state, parent, detpid) = cancellation_test_state();
        let parent_request = Ivar::new();
        install_test_registration(&state, parent, parent_request.clone());
        let child = DetTid::from_raw(18);
        let physical_pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let request = state.receive_rpc(
            reverie::Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(parent),
                GlobalRequest::CreateChildThread(
                    child,
                    detpid,
                    0,
                    None,
                    libc::SIGCHLD,
                    Some((physical_pid, physical_tid)),
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        );
        let kill_after_parent_parks = async {
            while parent_request.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            let mut scheduler = state.sched.lock().unwrap();
            let (registered_mm, registered_pid, registered_tid) = scheduler
                .physical_thread_identity(child)
                .expect("parent registration must install the child pidfd before continuing");
            assert_eq!(
                registered_mm,
                MmId::for_clone(MmId::initial(parent), child, false)
            );
            assert_eq!(
                (registered_pid, registered_tid),
                (physical_pid, physical_tid)
            );
            scheduler.logically_kill_thread(&parent, &detpid, MmId::initial(detpid));
        };

        let (response, ()) = tokio::join!(request, kill_after_parent_parks);
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
    }

    #[test]
    fn unsupported_syscall_warning_is_sorted_and_aggregated() {
        let syscalls = BTreeSet::from([
            "vmsplice".to_owned(),
            "getppid".to_owned(),
            "getppid".to_owned(),
        ]);

        assert_eq!(
            format_unsupported_syscall_warning(&syscalls).as_deref(),
            Some("syscalls getppid,vmsplice used but not yet supported")
        );
        assert_eq!(format_unsupported_syscall_warning(&BTreeSet::new()), None);
    }

    #[tokio::test]
    async fn abnormal_cleanup_cancels_an_unstarted_scheduler() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut state = GlobalState::initialize(&config, true);
        state.cancel_internal_scheduler().await;
        let summary_path = None;
        let cleanup = state.clean_up(false, &summary_path);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), cleanup)
                .await
                .is_ok_and(|result| result.is_ok()),
            "cleanup waited for a scheduler whose guest never registered"
        );
    }

    #[tokio::test]
    async fn abnormal_cleanup_cancels_a_registered_scheduler() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut state = GlobalState::initialize(&config, true);
        let dettid = DetTid::from_raw(1);
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.priorities.insert(dettid, DEFAULT_PRIORITY);
            scheduler.next_turns.insert(
                dettid,
                ThreadNextTurn {
                    dettid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            scheduler.runqueue_push_back(dettid);
            scheduler.started_up.put(());
        }
        tokio::task::yield_now().await;

        state.cancel_internal_scheduler().await;
        let summary_path = None;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                state.clean_up(false, &summary_path),
            )
            .await
            .is_ok_and(|result| result.is_ok()),
            "cleanup waited after cancelling a registered scheduler"
        );
    }

    /// A deterministic inode must be minted from the monotonic counter, never
    /// derived from the host inode's bits. This is the behavioural half of the
    /// guarantee whose static half is `DetInode` being a newtype: even for a
    /// large, realistic host inode the det value stays small and dense, so a
    /// leaked host inode is distinguishable from a genuine det one.
    #[test]
    fn det_inodes_are_minted_not_passed_through() {
        use crate::types::DetInode;

        let mut pool = super::InodePool::new();
        let t = LogicalTime::from_nanos(0);

        let host_a = 221_742_951; // the value observed leaking into FileContents
        let host_b = 998_877_665;
        let (a, _) = pool.add_inode(host_a, t);
        let (b, _) = pool.add_inode(host_b, t);

        assert_ne!(a.as_raw(), host_a, "det inode must not be the host inode");
        assert_ne!(b.as_raw(), host_b, "det inode must not be the host inode");
        assert_eq!(a, DetInode::mint(1), "minting starts at 1");
        assert_eq!(b, DetInode::mint(2), "minting is monotonic");

        // Re-determinizing the same host inode is stable, not a fresh mint.
        let (a_again, _) = pool.add_inode(host_a, t);
        assert_eq!(a, a_again, "mapping must be stable per host inode");
    }
    async fn accepted_capture_rpc_fixture() -> (
        Config,
        GlobalState,
        NetworkStreamOwner,
        crate::network_replay::NetworkAcceptLeaseId,
    ) {
        use detcore_model::network_trace::*;

        use crate::network_replay::accepted::AcceptedBackendCapability;
        let (config, mut state) = shadow_rpc_state();
        let thread = DetTid::from_raw(491);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(thread, thread, true);
        let ofd = OpenFileId::new_socket(thread, 0);
        *state.network_engine.as_ref().unwrap().lock().unwrap() =
            NetworkReplayEngine::record_shadow_accepted(
                config.epoch,
                AcceptedBackendCapability::controlled_fixture(),
            );
        let key = StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: libc::AF_INET,
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        };
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::RegisterAcceptedFreshSend {
                    key,
                    observed: Some(ReceiveTimeoutV3::Infinite)
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        shadow_rpc_register(&state, &config, owner, ofd).await;
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::EnsureChannel {
                    open_file: ofd,
                    binding: NetworkChannelBinding {
                        transport: NetworkTransportV2::Tcp,
                        role: NetworkEndpointRoleV2::Listener,
                        peer_address: None,
                        requested_local_constraint: None,
                        observed_local_address: None,
                        accepted_from: None,
                        selected_channel: None,
                    }
                }
            )
            .await,
            Ok(NetworkReply::Channel(Some(_)))
        ));
        let control = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginSocketControl { open_file: ofd },
            )
            .await,
        );
        let Ok(NetworkReply::StreamCall(call)) = stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::BeginStreamCall {
                control_lease: control,
            },
        )
        .await
        else {
            panic!("missing listener call")
        };
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::ConfirmStreamCallPin {
                    id: call.id,
                    outcome: crate::network_replay::NetworkStreamPinOutcome::Acquired
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FinishSocketControl {
                    lease: control,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        let Ok(NetworkReply::AcceptedChild(Some(reservation))) = stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::BeginAcceptedSocket { call: call.id },
        )
        .await
        else {
            panic!("missing accept receipt")
        };
        assert!(reservation.child.is_none());
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::SubmitAcceptedSocket {
                    lease: reservation.lease
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        );
        // Model state uses the actual admission RPCs. Only physical capture is
        // replaced: this resource has no task fd, so any attempted acquisition
        // fails before a host syscall rather than contacting an arbitrary task.
        state.network_runtime = Some(
            crate::network_runtime::NetworkRuntimeResources::accepted_custody_fixture(
                owner,
                reservation.lease,
                call.id,
            ),
        );
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(thread, owner.mm);
        (config, state, owner, reservation.lease)
    }

    #[tokio::test]
    async fn accepted_late_capture_rpc_retains_original_return_after_owner_exit_or_terminal_transition()
     {
        for cause in ["owner gone", "killed", "new mm", "backend failure"] {
            let (config, state, owner, lease) = accepted_capture_rpc_fixture().await;
            match cause {
                "owner gone" => assert_eq!(
                    state
                        .receive_rpc(
                            Tid::from_raw(owner.thread.as_raw()),
                            (
                                DetTime::new(&config),
                                owner.mm,
                                GlobalRequest::NetworkOwnerGone
                            )
                        )
                        .await,
                    (None, GlobalResponse::NetworkOwnerGone)
                ),
                "killed" => state.sched.lock().unwrap().logically_kill_thread(
                    &owner.thread,
                    &owner.thread,
                    owner.mm,
                ),
                "new mm" => {
                    state
                        .registered_exec_mms
                        .lock()
                        .unwrap()
                        .insert(owner.thread, owner.mm.for_exec(owner.thread));
                }
                "backend failure" => state.report_backend_failure(reverie::BackendFailure {
                    pid: Tid::from_raw(owner.thread.as_raw()),
                    tid: Tid::from_raw(owner.thread.as_raw()),
                    phase: "accepted late return control",
                }),
                _ => unreachable!(),
            }
            let mut capture = Box::pin(state.receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::Network(NetworkRequest::CaptureAcceptedReturn {
                        lease,
                        kernel_result: Ok(9),
                    }),
                ),
            ));
            let result = futures::poll!(capture.as_mut());
            assert!(
                matches!(result, Poll::Ready(_)),
                "late capture suspended: {cause}"
            );
            assert_eq!(
                state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .accepted_recovery_result(owner, lease)
                    .unwrap(),
                Some(Ok(9)),
                "lost exact late result: {cause}"
            );
            assert!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .finish()
                    .is_err(),
                "terminal receipt fabricated completion: {cause}"
            );
        }
    }

    #[tokio::test]
    async fn accepted_terminal_capture_drives_collection_without_following_guest_rpc() {
        for backend_failed in [false, true] {
            let (config, mut state, owner, lease) = accepted_capture_rpc_fixture().await;
            let (mut retained, mut peer) = state
                .network_runtime
                .as_mut()
                .unwrap()
                .accepted_collection_transport_fixture(owner, lease);
            if backend_failed {
                state.report_backend_failure(reverie::BackendFailure {
                    pid: Tid::from_raw(owner.thread.as_raw()),
                    tid: Tid::from_raw(owner.thread.as_raw()),
                    phase: "accepted final caller collection control",
                });
            } else {
                assert_eq!(
                    state
                        .receive_rpc(
                            Tid::from_raw(owner.thread.as_raw()),
                            (
                                DetTime::new(&config),
                                owner.mm,
                                GlobalRequest::NetworkOwnerGone
                            )
                        )
                        .await,
                    (None, GlobalResponse::NetworkOwnerGone)
                );
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            // A wrong lease cannot enqueue any command or overwrite the real
            // operation, even when the terminal reply hides its diagnostic.
            let wrong = crate::network_replay::NetworkAcceptLeaseId(lease.0 + 1);
            let _ = state
                .receive_rpc(
                    Tid::from_raw(owner.thread.as_raw()),
                    (
                        DetTime::new(&config),
                        owner.mm,
                        GlobalRequest::Network(NetworkRequest::CaptureAcceptedReturn {
                            lease: wrong,
                            kernel_result: Ok(8),
                        }),
                    ),
                )
                .await;
            assert!(peer.no_other_request());
            assert_eq!(
                state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .accepted_recovery_result(owner, lease)
                    .unwrap(),
                None
            );

            let mut capture = Box::pin(state.receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::Network(NetworkRequest::CaptureAcceptedReturn {
                        lease,
                        kernel_result: Ok(8),
                    }),
                ),
            ));
            assert!(matches!(
                futures::poll!(capture.as_mut()),
                Poll::Ready((None, GlobalResponse::ThreadExited))
            ));
            drop(capture);
            // No CollectAcceptedEffect RPC and no sibling task follows that
            // terminal response. Actual native transport must still advance.
            peer.reply_to_collection(owner, lease, deadline);
            loop {
                if state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .accepted_collection_effect_status(owner, lease)
                    .unwrap()
                    == Some((-1, 8))
                {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(peer.no_other_request());
            assert!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .finish()
                    .is_err()
            );
            let error = state.clean_up(false, &None).await.unwrap_err();
            assert!(
                error.to_string().contains("partial receipt retained"),
                "actual cleanup must refuse the retained provider failure before trace publication: {error}"
            );
            retained.stop_collection_fixture(deadline);
        }
    }

    #[tokio::test]
    async fn accepted_late_capture_rpc_rejects_wrong_lease_owner_without_erasing_rightful_result() {
        let (config, state, owner, lease) = accepted_capture_rpc_fixture().await;
        let wrong = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    wrong.mm,
                    GlobalRequest::Network(NetworkRequest::CaptureAcceptedReturn {
                        lease,
                        kernel_result: Ok(6),
                    }),
                ),
            )
            .await;
        assert!(matches!(
            response,
            (None, GlobalResponse::ThreadExited) | (None, GlobalResponse::Network(Err(_)))
        ));
        assert_eq!(
            state
                .network_runtime
                .as_ref()
                .unwrap()
                .accepted_recovery_result(owner, lease)
                .unwrap(),
            None
        );
        let response = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::Network(NetworkRequest::CaptureAcceptedReturn {
                        lease,
                        kernel_result: Err(libc::EFAULT),
                    }),
                ),
            )
            .await;
        assert!(matches!(
            response,
            (None, GlobalResponse::Network(Ok(NetworkReply::Unit)))
        ));
        assert_eq!(
            state
                .network_runtime
                .as_ref()
                .unwrap()
                .accepted_recovery_result(owner, lease)
                .unwrap(),
            Some(Err(libc::EFAULT))
        );
        assert!(
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .finish()
                .is_err()
        );
    }

    #[tokio::test]
    async fn accepted_listener_rpc_requires_current_owner_before_any_physical_capture() {
        let (config, state, owner, lease) = accepted_capture_rpc_fixture().await;
        let call = state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .accepted_capture_call(owner, lease)
            .unwrap();
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm.for_exec(owner.thread));
        let response = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::Network(NetworkRequest::EnrollAcceptedListener { call, fd: 4 }),
                ),
            )
            .await;
        assert!(matches!(response.1, GlobalResponse::ThreadExited));
        let engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        let ofd = engine.stream_call_open_file(owner, call).unwrap();
        assert!(!engine.accepted_listener_enrolled(ofd));
    }

    #[tokio::test]
    async fn accepted_listener_rpc_missing_physical_task_never_publishes_enrollment() {
        let (config, state, owner, lease) = accepted_capture_rpc_fixture().await;
        let call = state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .accepted_capture_call(owner, lease)
            .unwrap();
        // The actual runtime fixture has no task pidfd. The shared capture path
        // must stop at its authority lookup, before pidfd_getfd or transport IO.
        let response = state
            .receive_rpc(
                Tid::from_raw(owner.thread.as_raw()),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::Network(NetworkRequest::EnrollAcceptedListener { call, fd: 4 }),
                ),
            )
            .await;
        assert!(matches!(response.1, GlobalResponse::Network(Err(_))));
        let engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        let ofd = engine.stream_call_open_file(owner, call).unwrap();
        assert!(!engine.accepted_listener_enrolled(ofd));
    }

    #[tokio::test]
    async fn accepted_rpc_enrolls_exact_child_and_initializes_explicit_replay_variant() {
        use detcore_model::network_trace::NetworkTransportV2;
        use detcore_model::network_trace::ReceiveTimeoutV3;
        use detcore_model::network_trace::StreamSocketKeyV3;

        use crate::network_replay::NetworkStreamPinOutcome;
        use crate::network_replay::accepted::AcceptedBackendCapability;
        use crate::network_replay::accepted::AcceptedInstallationFact;
        use crate::network_replay::accepted::AcceptedPhysicalIdentity;
        use crate::network_replay::accepted::ChildCreationCertificate;
        let (mut config, state) = shadow_rpc_state();
        let thread = DetTid::from_raw(391);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let listener = OpenFileId::new_socket(thread, 0);
        assert!(matches!(
            stream_rpc(&state, &config, owner, NetworkRequest::AcceptedMode).await,
            Ok(NetworkReply::AcceptedMode(false))
        ));
        let cap = AcceptedBackendCapability::controlled_fixture();
        *state.network_engine.as_ref().unwrap().lock().unwrap() =
            NetworkReplayEngine::record_shadow_accepted(
                config.epoch,
                AcceptedBackendCapability::controlled_fixture(),
            );
        let key = StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: libc::AF_INET,
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        };
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::RegisterAcceptedFreshSend {
                    key,
                    observed: Some(ReceiveTimeoutV3::Infinite)
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        shadow_rpc_register(&state, &config, owner, listener).await;
        let local = NetworkAddressV2::Inet4 {
            address: [127, 0, 0, 1],
            port: 12345,
        };
        let peer = NetworkAddressV2::Inet4 {
            address: [127, 0, 0, 1],
            port: 54321,
        };
        let binding = NetworkChannelBinding {
            transport: NetworkTransportV2::Tcp,
            role: detcore_model::network_trace::NetworkEndpointRoleV2::Listener,
            peer_address: None,
            requested_local_constraint: None,
            observed_local_address: Some(local.clone()),
            accepted_from: None,
            selected_channel: None,
        };
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::EnsureChannel {
                    open_file: listener,
                    binding
                }
            )
            .await,
            Ok(NetworkReply::Channel(Some(_)))
        ));
        for option in [
            NetworkStreamSocketOption::ReceiveLowWater(3),
            NetworkStreamSocketOption::SendTimeout {
                seconds: 2,
                microseconds: 0,
            },
        ] {
            let lease = socket_control_receipt(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::BeginSocketControl {
                        open_file: listener,
                    },
                )
                .await,
            );
            assert!(matches!(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::SubmitStreamPhysical {
                        lease,
                        effect: NetworkStreamPhysicalEffect::SetSocketOption { option }
                    }
                )
                .await,
                Ok(NetworkReply::Unit)
            ));
            assert!(matches!(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::ConfirmStreamPhysical {
                        lease,
                        result: NetworkStreamPhysicalResult::SocketOption { result: Ok(()) }
                    }
                )
                .await,
                Ok(NetworkReply::Unit)
            ));
            assert!(matches!(
                stream_rpc(
                    &state,
                    &config,
                    owner,
                    NetworkRequest::FinishSocketControl {
                        lease,
                        disposition: NetworkSocketControlFinish::Unchanged
                    }
                )
                .await,
                Ok(NetworkReply::Unit)
            ));
        }
        let physical = AcceptedPhysicalIdentity {
            provider: 1,
            object: 2,
            namespace: 3,
        };
        let child = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine
                .enroll_accepted_listener(
                    listener,
                    AcceptedPhysicalIdentity {
                        object: 1,
                        ..physical
                    },
                    &cap,
                )
                .unwrap();
            let inherited = engine.stream_socket_state(listener).unwrap().unwrap();
            engine
                .observe_child_creation(
                    listener,
                    ChildCreationCertificate {
                        sequence: 1,
                        listener: AcceptedPhysicalIdentity {
                            object: 1,
                            ..physical
                        },
                        child: physical,
                        listener_generation: inherited.option_generation,
                        inherited,
                        local: local.clone(),
                        peer: peer.clone(),
                    },
                    state.global_time.lock().unwrap().as_nanos(),
                    &cap,
                )
                .unwrap()
        };
        let control = socket_control_receipt(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginSocketControl {
                    open_file: listener,
                },
            )
            .await,
        );
        let call = match stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::BeginStreamCall {
                control_lease: control,
            },
        )
        .await
        {
            Ok(NetworkReply::StreamCall(call)) => call,
            other => panic!("{other:?}"),
        };
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::ConfirmStreamCallPin {
                    id: call.id,
                    outcome: NetworkStreamPinOutcome::Acquired
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FinishSocketControl {
                    lease: control,
                    disposition: NetworkSocketControlFinish::Unchanged
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        let receipt = match stream_rpc(
            &state,
            &config,
            owner,
            NetworkRequest::BeginAcceptedSocket { call: call.id },
        )
        .await
        {
            Ok(NetworkReply::AcceptedChild(Some(receipt))) => receipt,
            other => panic!("{other:?}"),
        };
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::SubmitAcceptedSocket {
                    lease: receipt.lease
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        let installed = OpenFileId::new_socket(thread, 1);
        // The controlled fixture seeds the service's matched fact. No production
        // installation authority is inferred from this test or an RPC payload.
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .confirm_accepted_installation(
                owner,
                receipt.lease,
                AcceptedInstallationFact::Installed {
                    child,
                    fd: 17,
                    open_file: installed,
                    slot_generation: 1,
                    physical,
                },
                &cap,
            )
            .unwrap();
        assert!(
            matches!(stream_rpc(&state,&config,owner,NetworkRequest::CompleteAcceptedSocket {lease:receipt.lease,kernel_result:Ok(17),installed_open_file:Some(installed)}).await,Ok(NetworkReply::AcceptedCompletion(Some(done))) if done.fd==17 && done.open_file==installed)
        );
        assert!(
            matches!(stream_rpc(&state,&config,owner,NetworkRequest::StreamSocketState {open_file:installed}).await,Ok(NetworkReply::StreamSocketState(Some(socket))) if socket.options.receive_low_water==3 && socket.send_timeout==Some(ReceiveTimeoutV3::FiniteTicks(2000)))
        );
        assert!(
            matches!(stream_rpc(&state,&config,owner,NetworkRequest::AcceptedEndpoint {open_file:installed,peer:true}).await,Ok(NetworkReply::AcceptedEndpoint(Some(address))) if address==peer)
        );
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::BeginStreamCallRelease { id: call.id }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        assert!(matches!(
            stream_rpc(
                &state,
                &config,
                owner,
                NetworkRequest::FinishStreamCallRelease { id: call.id }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        let engine = std::mem::replace(
            &mut *state.network_engine.as_ref().unwrap().lock().unwrap(),
            NetworkReplayEngine::record(config.epoch),
        );
        let trace = engine.into_recorded_versioned_trace().unwrap();
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        config.network_trace.policy = NetworkPolicy::Replay;
        config.network_trace_input = Some(bytes);
        let replay = GlobalState::initialize(&config, false);
        assert!(matches!(
            stream_rpc(&replay, &config, owner, NetworkRequest::AcceptedMode).await,
            Ok(NetworkReply::AcceptedMode(true))
        ));
        assert!(matches!(
            stream_rpc(
                &replay,
                &config,
                owner,
                NetworkRequest::RegisterAcceptedFreshSend {
                    key,
                    observed: None
                }
            )
            .await,
            Ok(NetworkReply::Unit)
        ));
        assert!(
            matches!(stream_rpc(&replay,&config,owner,NetworkRequest::RegisterStreamSocket {open_file:listener,key,
            namespace:NetworkStreamNamespace {device:4,inode:100},observed_profile:None}).await,Ok(NetworkReply::StreamSocketState(Some(socket))) if socket.send_timeout==Some(ReceiveTimeoutV3::Infinite))
        );
    }
    #[tokio::test]
    async fn accepted_rpc_rejects_stale_mm_before_mode_or_custody_mutation() {
        let (config, state) = shadow_rpc_state();
        let thread = DetTid::from_raw(392);
        let old = MmId::initial(thread);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(thread, old.for_exec(thread));
        let before = format!(
            "{:?}",
            state.network_engine.as_ref().unwrap().lock().unwrap()
        );
        let (_, response) = state
            .receive_rpc(
                Tid::from_raw(thread.as_raw()),
                (
                    DetTime::new(&config),
                    old,
                    GlobalRequest::Network(NetworkRequest::AcceptedMode),
                ),
            )
            .await;
        assert!(matches!(response, GlobalResponse::ThreadExited));
        assert_eq!(
            format!(
                "{:?}",
                state.network_engine.as_ref().unwrap().lock().unwrap()
            ),
            before
        );
    }
    #[tokio::test]
    async fn no_seq_backend_inherited_birth_consumes_permit_after_parent_exit_and_reap() {
        use reverie::Tool;

        use crate::network_replay::NetworkFdMutationBegin;
        use crate::network_replay::NetworkFdMutationKind;
        for shared in [false, true] {
            let (config, state) = stream_rpc_state(false);
            let grand = DetTid::from_raw(60);
            let parent = DetTid::from_raw(61);
            let child = DetTid::from_raw(62);
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(grand, grand, true);
                sched.thread_tree.add_child(grand, parent, true);
            }
            install_test_registration(&state, parent, Ivar::new());
            let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
            let mut parent_state = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
            parent_state.detpid = Some(parent);
            parent_state.thread_start_entered = true;
            let parent_mm = parent_state.mm_id;
            let owner = NetworkStreamOwner {
                thread: parent,
                mm: parent_mm,
            };
            let flags = if shared {
                CloneFlags::CLONE_FILES
            } else {
                CloneFlags::empty()
            };
            let permit = {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                engine.fd_table_fixture_enable();
                engine.register_initial_fd_table(owner, parent).unwrap();
                let files = engine.fd_table_fixture_files(owner).unwrap();
                let NetworkFdMutationBegin::Admitted(admission) = engine
                    .begin_fd_mutation(owner, files, NetworkFdMutationKind::Clone { flags })
                    .unwrap()
                else {
                    panic!("clone admitted");
                };
                let admission = *admission;
                engine
                    .submit_fd_mutation(owner, admission.publication.permit)
                    .unwrap();
                admission.publication.permit
            };
            let response = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        parent_mm,
                        GlobalRequest::PrepareNoSeqBirth {
                            process: parent,
                            syscall_count: 9,
                            flags,
                            child_tid_addr: 0x1234,
                            exit_signal: libc::SIGCHLD,
                            priority_entropy: None,
                            fd_permit: Some(permit),
                        },
                    ),
                )
                .await;
            assert!(
                response.0.is_none(),
                "custody prepare cannot advance virtual time"
            );
            let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = response.1 else {
                panic!("prepared birth");
            };
            let submitted = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        parent_mm,
                        GlobalRequest::SubmitNoSeqBirth(birth),
                    ),
                )
                .await;
            let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = submitted.1 else {
                panic!("submitted birth");
            };
            parent_state.clone_flags = Some(flags);
            parent_state.pending_fd_clone = Some(permit);
            parent_state.pending_no_seq_birth = Some(birth.clone());
            let mut child_state = tool.init_thread_state(
                Tid::from_raw(child.as_raw()),
                Some((Tid::from_raw(parent.as_raw()), &parent_state)),
            );
            let inherited = child_state.pending_no_seq_birth.take().unwrap();
            assert_eq!(inherited.fd_permit(), Some(permit));
            assert_eq!(child_state.pending_fd_clone, Some(permit));
            // Real existing teardown methods; no missing-birth tombstone and no
            // backend-failure fabrication. This is a unit boundary, not a
            // native-process or end-to-end guest execution claim.
            state.recv_network_owner_gone(owner);
            {
                let mut sched = state.sched.lock().unwrap();
                sched.logically_kill_thread(&parent, &parent, parent_mm);
                assert!(sched.consume_child_wait(grand, parent));
            }
            state.registered_exec_mms.lock().unwrap().remove(&parent);
            let result = state
                .receive_rpc(
                    Tid::from_raw(child.as_raw()),
                    (
                        child_state.thread_logical_time.clone(),
                        child_state.mm_id,
                        GlobalRequest::CreateNoSeqChildThread(
                            inherited.clone(),
                            None,
                            Some(DEFAULT_PRIORITY),
                        ),
                    ),
                )
                .await;
            assert_eq!(result.1, GlobalResponse::CreateChildThread(None));
            assert!(state.sched.lock().unwrap().thread_was_registered(child));
            assert!(!state.sched.lock().unwrap().backend_failed());
            assert_eq!(
                state.sched.lock().unwrap().next_turns[&child].child_tid_addr,
                0x1234
            );
            assert_eq!(
                state.registered_exec_mms.lock().unwrap().get(&child),
                Some(&child_state.mm_id)
            );
            let child_owner = NetworkStreamOwner {
                thread: child,
                mm: child_state.mm_id,
            };
            let files = state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .fd_table_fixture_files(child_owner)
                .unwrap();
            assert_eq!(
                files,
                if shared {
                    permit.files
                } else {
                    FilesId::forked(child)
                }
            );
            let duplicate = state
                .receive_rpc(
                    Tid::from_raw(child.as_raw()),
                    (
                        child_state.thread_logical_time.clone(),
                        child_state.mm_id,
                        GlobalRequest::CreateNoSeqChildThread(
                            inherited,
                            None,
                            Some(DEFAULT_PRIORITY),
                        ),
                    ),
                )
                .await;
            assert_eq!(duplicate.1, GlobalResponse::ThreadExited);
            assert!(state.sched.lock().unwrap().thread_was_registered(child));
        }
    }
    #[tokio::test]
    async fn no_seq_birth_unknown_submission_refuses_terminal_success() {
        for submitted in [false, true] {
            let (config, state, parent, mm) = custody_birth_fixture(false);
            let response = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::PrepareNoSeqBirth {
                            process: parent,
                            syscall_count: 1,
                            flags: CloneFlags::empty(),
                            child_tid_addr: 0,
                            exit_signal: libc::SIGCHLD,
                            priority_entropy: None,
                            fd_permit: None,
                        },
                    ),
                )
                .await;
            let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = response.1 else {
                panic!("prepared");
            };
            if submitted {
                let response = state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            mm,
                            GlobalRequest::SubmitNoSeqBirth(birth),
                        ),
                    )
                    .await;
                assert!(matches!(
                    response.1,
                    GlobalResponse::PrepareNoSeqBirth(Some(_))
                ));
            }
            let error = state.clean_up(false, &None).await.unwrap_err();
            assert!(
                error.to_string().contains("1 unresolved child birth(s)"),
                "{error:#}"
            );
        }
    }

    #[tokio::test]
    async fn no_seq_birth_requires_bound_callback_and_original_mm() {
        use reverie::Tool;
        let (config, state, parent, parent_mm) = custody_birth_fixture(false);
        let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
        let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        local.detpid = Some(parent);
        let flags = CloneFlags::empty();
        let request = GlobalRequest::PrepareNoSeqBirth {
            process: parent,
            syscall_count: 1,
            flags,
            child_tid_addr: 0,
            exit_signal: libc::SIGCHLD,
            priority_entropy: None,
            fd_permit: None,
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (DetTime::new(&config), parent_mm, request),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = response.1 else {
            panic!("prepared");
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    parent_mm,
                    GlobalRequest::SubmitNoSeqBirth(prepared),
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = response.1 else {
            panic!("submitted");
        };
        let child = DetTid::from_raw(62);
        let child_mm = MmId::for_clone(parent_mm, child, false);
        // Unbound parent copy is not child authority.
        let unbound = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    DetTime::new(&config),
                    child_mm,
                    GlobalRequest::CreateNoSeqChildThread(
                        birth.clone(),
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(unbound.1, GlobalResponse::ThreadExited);
        local.clone_flags = Some(flags);
        local.pending_no_seq_birth = Some(birth.clone());
        let mut inherited = tool.init_thread_state(
            Tid::from_raw(child.as_raw()),
            Some((Tid::from_raw(parent.as_raw()), &local)),
        );
        let bound = inherited.pending_no_seq_birth.take().unwrap();
        for (sender, mm) in [
            (child, child_mm.for_exec(child)),
            (DetTid::from_raw(63), child_mm),
        ] {
            let rejected = state
                .receive_rpc(
                    Tid::from_raw(sender.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::CreateNoSeqChildThread(
                            bound.clone(),
                            None,
                            Some(DEFAULT_PRIORITY),
                        ),
                    ),
                )
                .await;
            assert_eq!(rejected.1, GlobalResponse::ThreadExited);
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_no_seq_birth_count(),
                1
            );
            assert!(!state.sched.lock().unwrap().thread_was_registered(child));
        }
        let accepted = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    DetTime::new(&config),
                    child_mm,
                    GlobalRequest::CreateNoSeqChildThread(bound, None, Some(DEFAULT_PRIORITY)),
                ),
            )
            .await;
        assert_eq!(accepted.1, GlobalResponse::CreateChildThread(None));
        let joined = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    parent_mm,
                    GlobalRequest::JoinNoSeqBirth(birth, child),
                ),
            )
            .await;
        assert_eq!(joined.1, GlobalResponse::JoinNoSeqBirth(true));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_no_seq_birth_count(),
            0
        );
    }
    #[tokio::test]
    async fn no_seq_parent_return_and_group_change_wait_for_real_child_registration() {
        use std::future::Future;

        use reverie::Tool;
        let (config, state) = stream_rpc_state(false);
        let grand = DetTid::from_raw(60);
        let parent = DetTid::from_raw(61);
        let mm = MmId::initial(parent);
        let child = DetTid::from_raw(62);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(grand, grand, true);
            sched.thread_tree.add_child(grand, parent, true);
        }
        install_test_registration(&state, parent, Ivar::new());
        let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
        let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        local.detpid = Some(parent);
        local.clone_flags = Some(CloneFlags::empty());
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::PrepareNoSeqBirth {
                        process: parent,
                        syscall_count: 1,
                        flags: CloneFlags::empty(),
                        child_tid_addr: 0,
                        exit_signal: libc::SIGCHLD,
                        priority_entropy: None,
                        fd_permit: None,
                    },
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = response.1 else {
            panic!("prepared");
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::SubmitNoSeqBirth(prepared),
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = response.1 else {
            panic!("submitted");
        };
        local.pending_no_seq_birth = Some(birth.clone());
        // This is the actual backend Tool birth call, before common registration.
        // It represents a physically created child, not a pre-clone snapshot.
        let mut child_state = tool.init_thread_state(
            Tid::from_raw(child.as_raw()),
            Some((Tid::from_raw(parent.as_raw()), &local)),
        );
        let inherited = child_state.pending_no_seq_birth.take().unwrap();
        let mut parent_return = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::JoinNoSeqBirth(birth.clone(), child),
            ),
        ));
        let mut later_setsid = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::PrepareProcessGroupChange(
                    crate::scheduler::ProcessGroupChangeKind::Session { process: parent },
                    2,
                ),
            ),
        ));
        let (wakes, waker) = custody_test_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(parent_return.as_mut().poll(&mut cx).is_pending());
        assert!(later_setsid.as_mut().poll(&mut cx).is_pending());
        assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let child_before = state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .process_group(parent)
            .unwrap();
        let registered = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_state.thread_logical_time.clone(),
                    child_state.mm_id,
                    GlobalRequest::CreateNoSeqChildThread(inherited, None, Some(DEFAULT_PRIORITY)),
                ),
            )
            .await;
        assert_eq!(registered.1, GlobalResponse::CreateChildThread(None));
        assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert_eq!(parent_return.await.1, GlobalResponse::JoinNoSeqBirth(true));
        let GlobalResponse::ProcessGroupChange(Some(prepared)) = later_setsid.await.1 else {
            panic!("group admitted after birth");
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::SubmitProcessGroupChange(prepared),
                ),
            )
            .await;
        let GlobalResponse::ProcessGroupChange(Some(submitted)) = response.1 else {
            panic!("group submitted");
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::CompleteProcessGroupChange(
                        submitted,
                        Ok(i64::from(parent.as_raw())),
                    ),
                ),
            )
            .await;
        assert_eq!(response.1, GlobalResponse::CompleteProcessGroupChange(true));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .process_group(parent),
            Some(parent)
        );
        assert_ne!(child_before, parent);
        assert_eq!(
            state.sched.lock().unwrap().thread_tree.process_group(child),
            Some(child_before)
        );
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_no_seq_birth_count(),
            0
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_process_group_change()
        );
        let duplicate = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::JoinNoSeqBirth(birth, child),
                ),
            )
            .await;
        assert_eq!(duplicate.1, GlobalResponse::JoinNoSeqBirth(false));
    }

    #[tokio::test]
    async fn no_seq_known_clone_failure_wakes_waiting_group_admission() {
        use std::future::Future;
        let (config, state, parent, mm) = custody_birth_fixture(false);
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::PrepareNoSeqBirth {
                        process: parent,
                        syscall_count: 1,
                        flags: CloneFlags::empty(),
                        child_tid_addr: 0,
                        exit_signal: libc::SIGCHLD,
                        priority_entropy: None,
                        fd_permit: None,
                    },
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = response.1 else {
            panic!("prepared");
        };
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::SubmitNoSeqBirth(prepared),
                ),
            )
            .await;
        let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = response.1 else {
            panic!("submitted");
        };
        let mut group = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::PrepareProcessGroupChange(
                    crate::scheduler::ProcessGroupChangeKind::Set {
                        process: parent,
                        group: parent,
                    },
                    2,
                ),
            ),
        ));
        let (wakes, waker) = custody_test_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(group.as_mut().poll(&mut cx).is_pending());
        let response = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::CancelNoSeqBirth(birth, libc::EAGAIN),
                ),
            )
            .await;
        assert_eq!(response.1, GlobalResponse::CancelNoSeqBirth(true));
        assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0);
        let GlobalResponse::ProcessGroupChange(Some(_)) = group.await.1 else {
            panic!("known native clone error must unblock group admission");
        };
        // The exact callback consumption cancels this unsubmitted preparation.
        state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::NoSeqBirthOwnerGone(None, None),
                ),
            )
            .await;
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_process_group_change()
        );
    }
    #[tokio::test]
    async fn no_seq_group_native_error_keeps_clock_path_and_unknown_refuses_cleanup() {
        for known_error in [false, true] {
            let (config, state, parent, mm) = custody_birth_fixture(false);
            let response = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::PrepareProcessGroupChange(
                            crate::scheduler::ProcessGroupChangeKind::Session { process: parent },
                            1,
                        ),
                    ),
                )
                .await;
            let GlobalResponse::ProcessGroupChange(Some(prepared)) = response.1 else {
                panic!("prepared");
            };
            let response = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::SubmitProcessGroupChange(prepared),
                    ),
                )
                .await;
            let GlobalResponse::ProcessGroupChange(Some(submitted)) = response.1 else {
                panic!("submitted");
            };
            if known_error {
                let before = state.global_time.lock().unwrap().threads_time(parent);
                let committed = state.sched.lock().unwrap().committed_time;
                let response = state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            mm,
                            GlobalRequest::CompleteProcessGroupChange(submitted, Err(libc::EPERM)),
                        ),
                    )
                    .await;
                assert_eq!(
                    response,
                    (None, GlobalResponse::CompleteProcessGroupChange(true))
                );
                assert_eq!(
                    state.global_time.lock().unwrap().threads_time(parent),
                    before
                );
                assert_eq!(state.sched.lock().unwrap().committed_time, committed);
                assert!(
                    !state
                        .sched
                        .lock()
                        .unwrap()
                        .thread_tree
                        .pending_process_group_change()
                );
            } else {
                state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            mm,
                            GlobalRequest::NoSeqBirthOwnerGone(None, None),
                        ),
                    )
                    .await;
                assert!(
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .thread_tree
                        .pending_process_group_change()
                );
                let error = state.clean_up(false, &None).await.unwrap_err();
                assert!(
                    error.to_string().contains("1 group transition(s)"),
                    "{error:#}"
                );
            }
        }
    }
    #[tokio::test]
    async fn no_seq_actual_consumed_state_settles_only_exact_uninvoked_submission() {
        use reverie::Tool;
        for group in [false, true] {
            for invoked in [false, true] {
                let (config, state, parent, mm) = custody_birth_fixture(false);
                let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
                let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
                local.detpid = Some(parent);
                local.thread_start_entered = true;
                let marker = if group {
                    let response = state
                        .receive_rpc(
                            Tid::from_raw(parent.as_raw()),
                            (
                                DetTime::new(&config),
                                mm,
                                GlobalRequest::PrepareProcessGroupChange(
                                    crate::scheduler::ProcessGroupChangeKind::Session {
                                        process: parent,
                                    },
                                    1,
                                ),
                            ),
                        )
                        .await;
                    let GlobalResponse::ProcessGroupChange(Some(prepared)) = response.1 else {
                        panic!("prepared");
                    };
                    let marker = crate::scheduler::UninvokedWaitCall::group(prepared.clone());
                    let response = state
                        .receive_rpc(
                            Tid::from_raw(parent.as_raw()),
                            (
                                DetTime::new(&config),
                                mm,
                                GlobalRequest::SubmitProcessGroupChange(prepared),
                            ),
                        )
                        .await;
                    assert!(matches!(
                        response.1,
                        GlobalResponse::ProcessGroupChange(Some(_))
                    ));
                    marker
                } else {
                    let response = state
                        .receive_rpc(
                            Tid::from_raw(parent.as_raw()),
                            (
                                DetTime::new(&config),
                                mm,
                                GlobalRequest::PrepareNoSeqBirth {
                                    process: parent,
                                    syscall_count: 1,
                                    flags: CloneFlags::empty(),
                                    child_tid_addr: 0,
                                    exit_signal: libc::SIGCHLD,
                                    priority_entropy: None,
                                    fd_permit: None,
                                },
                            ),
                        )
                        .await;
                    let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = response.1 else {
                        panic!("prepared");
                    };
                    let marker = crate::scheduler::UninvokedWaitCall::birth(prepared.clone());
                    let response = state
                        .receive_rpc(
                            Tid::from_raw(parent.as_raw()),
                            (
                                DetTime::new(&config),
                                mm,
                                GlobalRequest::SubmitNoSeqBirth(prepared),
                            ),
                        )
                        .await;
                    assert!(matches!(
                        response.1,
                        GlobalResponse::PrepareNoSeqBirth(Some(_))
                    ));
                    marker
                };
                // Marker remains even if the acknowledged Submit reply is lost.
                // Only the actual invocation boundary removes it in production.
                local.uninvoked_wait_call = (!invoked).then_some(marker.clone());
                let wrong = state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            mm.for_exec(parent),
                            GlobalRequest::NoSeqBirthOwnerGone(Some(marker), None),
                        ),
                    )
                    .await;
                assert_eq!(wrong.1, GlobalResponse::NoSeqBirthOwnerGone(false));
                let rpc = NetworkExitRpc {
                    state: &state,
                    sender: parent,
                };
                tool.on_exit_thread(
                    Tid::from_raw(parent.as_raw()),
                    &rpc,
                    local,
                    reverie::ExitStatus::Exited(0),
                )
                .await
                .unwrap();
                let sched = state.sched.lock().unwrap();
                assert_eq!(
                    sched.thread_tree.pending_process_group_change(),
                    group && invoked
                );
                assert_eq!(
                    sched.thread_tree.pending_no_seq_birth_count(),
                    usize::from(!group && invoked)
                );
                assert_eq!(sched.thread_tree.process_group_admission_busy(), invoked);
            }
        }
    }
    #[tokio::test]
    async fn no_seq_fd_admission_survives_submit_and_birth_prepare_cancellation() {
        use reverie::Tool;

        use crate::network_replay::NetworkFdMutationKind;
        for pause in [
            "before-fd-submit",
            "after-fd-submit",
            "group-held",
            "after-birth-prepare",
        ] {
            let (config, state, owner, _files) = fd_lifecycle_exec_fixture();
            let tool: Detcore = Detcore::new(Tid::from_raw(owner.thread.as_raw()), &config);
            let mut local = tool.init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
            local.detpid = Some(owner.thread);
            local.thread_start_entered = true;
            local.file_metadata = std::sync::Arc::new(Mutex::new(
                crate::tool_local::FileMetadata::empty_network_fixture(owner.thread),
            ));
            let other = DetTid::from_raw(71);
            let other_mm = MmId::initial(other);
            if pause == "group-held" {
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .add_child(owner.thread, other, true);
                install_test_registration(&state, other, Ivar::new());
                let reply = state
                    .receive_rpc(
                        Tid::from_raw(other.as_raw()),
                        (
                            DetTime::new(&config),
                            other_mm,
                            GlobalRequest::PrepareProcessGroupChange(
                                crate::scheduler::ProcessGroupChangeKind::Session {
                                    process: other,
                                },
                                1,
                            ),
                        ),
                    )
                    .await;
                assert!(matches!(
                    reply.1,
                    GlobalResponse::ProcessGroupChange(Some(_))
                ));
            }
            let mut guest = ExternalRegistrationGuest {
                global: &state,
                config: &config,
                thread: local,
                requests: Mutex::new(Vec::new()),
                pause_rpc: Some(pause),
            };
            {
                let pending = async {
                    let admission = tool
                        .begin_network_fd_mutation(
                            &mut guest,
                            NetworkFdMutationKind::Clone {
                                flags: CloneFlags::empty(),
                            },
                        )
                        .await
                        .unwrap()
                        .unwrap();
                    super::prepare_no_seq_child_birth(
                        &mut guest,
                        CloneFlags::empty(),
                        0,
                        libc::SIGCHLD,
                        None,
                        Some(admission.publication.permit),
                    )
                    .await
                    .unwrap();
                };
                let mut pending = std::pin::pin!(pending);
                assert!(
                    futures::poll!(pending.as_mut()).is_pending(),
                    "pause {pause}"
                );
            }
            let admission = guest
                .thread
                .uninvoked_fd_clone
                .clone()
                .expect("actual begin stores admission before Submit may yield");
            assert!(
                guest.thread.uninvoked_wait_call.is_none(),
                "birth preparation has not returned at any selected cancellation boundary"
            );
            assert!(guest.thread.pending_fd_clone.is_none());
            assert!(
                guest
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| matches!(
                        request,
                        GlobalRequest::Network(NetworkRequest::FdMutation(
                            crate::network_replay::NetworkFdMutationRequest::Submit { .. }
                        ))
                    ))
            );
            let rpc = NetworkExitRpc {
                state: &state,
                sender: owner.thread,
            };
            tool.on_exit_thread(
                Tid::from_raw(owner.thread.as_raw()),
                &rpc,
                guest.thread,
                reverie::ExitStatus::Exited(0),
            )
            .await
            .unwrap();
            {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                assert!(
                    engine.cancel_uninvoked_clone_admission(&admission).is_err(),
                    "exact admission already consumed"
                );
                engine.finish_fd_mutations().unwrap();
            }
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_no_seq_birth_count(),
                0
            );
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_process_group_change(),
                pause == "group-held"
            );
            if pause == "group-held" {
                let reply = state
                    .receive_rpc(
                        Tid::from_raw(other.as_raw()),
                        (
                            DetTime::new(&config),
                            other_mm,
                            GlobalRequest::NoSeqBirthOwnerGone(None, None),
                        ),
                    )
                    .await;
                assert_eq!(reply.1, GlobalResponse::NoSeqBirthOwnerGone(true));
                assert!(
                    !state
                        .sched
                        .lock()
                        .unwrap()
                        .thread_tree
                        .pending_process_group_change()
                );
            }
        }
    }

    #[tokio::test]
    async fn no_seq_uninvoked_clone_release_wakes_fd_waiter_after_prior_owner_notification() {
        use std::future::Future;

        use crate::network_replay::NetworkFdMutationBegin;
        use crate::network_replay::NetworkFdMutationKind;
        use crate::network_replay::NetworkFdPublicationReply as P;
        use crate::network_replay::NetworkFdPublicationRequest as Q;
        let (config, state) = stream_rpc_state(false);
        let first = NetworkStreamOwner {
            thread: DetTid::from_raw(41),
            mm: MmId::initial(DetTid::from_raw(41)),
        };
        let sibling = NetworkStreamOwner {
            thread: DetTid::from_raw(42),
            mm: first.mm,
        };
        let (files, admission) = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            let files = engine.fd_publication_fixture_register(first, None);
            assert_eq!(
                engine.fd_publication_fixture_register(sibling, Some(first)),
                files
            );
            engine.fd_table_fixture_enable();
            let NetworkFdMutationBegin::Admitted(admission) = engine
                .begin_fd_mutation(
                    first,
                    files,
                    NetworkFdMutationKind::Clone {
                        flags: CloneFlags::CLONE_FILES,
                    },
                )
                .unwrap()
            else {
                panic!("admitted");
            };
            let admission = *admission;
            engine
                .submit_fd_mutation(first, admission.publication.permit)
                .unwrap();
            (files, admission)
        };
        // The old owner-gone wake happens while the submitted permit is retained.
        state.recv_network_owner_gone(first);
        let mut waiter = std::pin::pin!(stream_rpc(
            &state,
            &config,
            sibling,
            NetworkRequest::FdPublication(Q::Acquire { files })
        ));
        let (wakes, waker) = custody_test_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        wakes.0.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut wrong = admission.clone();
        wrong.publication.permit.owner.mm = first.mm.for_exec(first.thread);
        let bad = state
            .receive_rpc(
                Tid::from_raw(first.thread.as_raw()),
                (
                    DetTime::new(&config),
                    first.mm,
                    GlobalRequest::NoSeqBirthOwnerGone(None, Some(wrong)),
                ),
            )
            .await;
        assert_eq!(bad.1, GlobalResponse::NoSeqBirthOwnerGone(false));
        assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        let good = state
            .receive_rpc(
                Tid::from_raw(first.thread.as_raw()),
                (
                    DetTime::new(&config),
                    first.mm,
                    GlobalRequest::NoSeqBirthOwnerGone(None, Some(admission.clone())),
                ),
            )
            .await;
        assert_eq!(good.1, GlobalResponse::NoSeqBirthOwnerGone(true));
        assert!(
            wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "release must wake the already enrolled network waiter"
        );
        let successor = match waiter.await.unwrap() {
            NetworkReply::FdPublication(P::Admitted(a)) => a.permit,
            other => panic!("{other:?}"),
        };
        assert_eq!(successor.owner, sibling);
        assert_ne!(successor.lease, admission.publication.permit.lease);
        assert_eq!(
            stream_rpc(
                &state,
                &config,
                sibling,
                NetworkRequest::FdPublication(Q::ReleaseEmpty { permit: successor })
            )
            .await,
            Ok(NetworkReply::FdPublication(P::Released))
        );
    }
    async fn submitted_observed_group_fixture(
        state: &GlobalState,
        config: &Config,
        parent: DetTid,
        mm: MmId,
    ) -> crate::scheduler::ProcessGroupChange {
        let prepared = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(config),
                    mm,
                    GlobalRequest::PrepareProcessGroupChange(
                        crate::scheduler::ProcessGroupChangeKind::Session { process: parent },
                        1,
                    ),
                ),
            )
            .await
            .1;
        let GlobalResponse::ProcessGroupChange(Some(prepared)) = prepared else {
            panic!("prepare");
        };
        let submitted = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(config),
                    mm,
                    GlobalRequest::SubmitProcessGroupChange(prepared),
                ),
            )
            .await
            .1;
        let GlobalResponse::ProcessGroupChange(Some(submitted)) = submitted else {
            panic!("submit");
        };
        submitted
    }

    #[tokio::test]
    async fn actual_observer_known_group_result_settles_before_consuming_exit() {
        use reverie::InjectedSyscallEvent;
        use reverie::Tool;
        use reverie::syscalls::SyscallInfo;
        for success in [false, true] {
            let (config, state, parent, mm) = custody_birth_fixture(false);
            let inherited_group = DetTid::from_raw(60);
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .set_process_group(parent, inherited_group)
            );
            let submitted = submitted_observed_group_fixture(&state, &config, parent, mm).await;
            let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
            let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
            local.detpid = Some(parent);
            local.stats.syscall_count = 1;
            let (nr, args) = reverie::syscalls::Setsid::new().into_parts();
            let raw = if success {
                i64::from(parent.as_raw())
            } else {
                -i64::from(libc::EPERM)
            };
            tool.on_injected_syscall_observed(
                Tid::from_raw(parent.as_raw()),
                &state,
                &mut local,
                nr,
                args,
                InjectedSyscallEvent::Returned(raw),
            );
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_process_group_change()
            );
            assert!(!state.sched.lock().unwrap().backend_failed());
            // This is the distinct native final-wait hook, not on_exit_thread.
            tool.on_backend_thread_terminal(
                Tid::from_raw(parent.as_raw()),
                &state,
                &mut local,
                ExitStatus::Exited(0),
            );
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_process_group_change()
            );
            assert_eq!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .process_group(parent),
                Some(if success { parent } else { inherited_group })
            );
            assert!(!state.sched.lock().unwrap().backend_failed());
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .complete_process_group_change(
                        &submitted,
                        if success { Ok(raw) } else { Err(libc::EPERM) }
                    )
            );
        }
    }

    #[tokio::test]
    async fn actual_terminal_unknown_group_fails_and_wakes_without_discarding_gate() {
        use std::future::Future;

        use reverie::Tool;
        let (config, state, parent, mm) = custody_birth_fixture(false);
        let _submitted = submitted_observed_group_fixture(&state, &config, parent, mm).await;
        let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
        let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        local.detpid = Some(parent);
        local.stats.syscall_count = 1;
        let mut waiter = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                mm,
                GlobalRequest::PrepareProcessGroupChange(
                    crate::scheduler::ProcessGroupChangeKind::Session { process: parent },
                    2,
                ),
            ),
        ));
        let (wakes, waker) = custody_test_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        let network = state.network_stream_changed.notified();
        tokio::pin!(network);
        network.as_mut().enable();
        assert!(network.as_mut().poll(&mut cx).is_pending());
        let before = wakes.0.load(std::sync::atomic::Ordering::SeqCst);
        tool.on_backend_thread_terminal(
            Tid::from_raw(parent.as_raw()),
            &state,
            &mut local,
            ExitStatus::Signaled(reverie::Signal::SIGKILL, false),
        );
        assert!(state.sched.lock().unwrap().backend_failed());
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_process_group_change()
        );
        assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) > before);
        assert!(network.as_mut().poll(&mut cx).is_ready());
        assert_eq!(waiter.await.1, GlobalResponse::ProcessGroupChange(None));
    }

    #[tokio::test]
    async fn actual_observer_rejects_wrong_mm_arguments_and_duplicate_result() {
        use reverie::InjectedSyscallEvent;
        use reverie::Tool;
        use reverie::syscalls::SyscallInfo;
        for wrong in ["MM", "arguments", "duplicate"] {
            let (config, state, parent, mm) = custody_birth_fixture(false);
            let _submitted = submitted_observed_group_fixture(&state, &config, parent, mm).await;
            let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
            let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
            local.detpid = Some(parent);
            local.stats.syscall_count = 1;
            let (mut nr, mut args) = reverie::syscalls::Setsid::new().into_parts();
            if wrong == "MM" {
                local.mm_id = mm.for_exec(parent);
            }
            if wrong == "arguments" {
                (nr, args) = reverie::syscalls::Setpgid::new()
                    .with_pid(parent.as_raw())
                    .with_pgid(99)
                    .into_parts();
            }
            tool.on_injected_syscall_observed(
                Tid::from_raw(parent.as_raw()),
                &state,
                &mut local,
                nr,
                args,
                InjectedSyscallEvent::Returned(i64::from(parent.as_raw())),
            );
            if wrong == "duplicate" {
                assert!(!state.sched.lock().unwrap().backend_failed());
                tool.on_injected_syscall_observed(
                    Tid::from_raw(parent.as_raw()),
                    &state,
                    &mut local,
                    nr,
                    args,
                    InjectedSyscallEvent::Returned(i64::from(parent.as_raw())),
                );
            }
            assert!(state.sched.lock().unwrap().backend_failed(), "{wrong}");
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .pending_process_group_change()
            );
        }
    }

    async fn observed_prestart_child_fixture(
        config: &Config,
        state: &GlobalState,
        parent: DetTid,
        flags: CloneFlags,
        native_child: bool,
    ) -> (
        crate::scheduler::NoSeqChildBirth,
        Detcore,
        crate::ThreadState<()>,
    ) {
        use reverie::InjectedSyscallEvent;
        use reverie::Tool;
        use reverie::syscalls::SyscallInfo;
        let mm = MmId::initial(parent);
        let parent_tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), config);
        let mut local = parent_tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        local.detpid = Some(parent);
        local.stats.syscall_count = 1;
        local.clone_flags = Some(flags);
        let prepared = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(config),
                    mm,
                    GlobalRequest::PrepareNoSeqBirth {
                        process: parent,
                        syscall_count: 1,
                        flags,
                        child_tid_addr: 0,
                        exit_signal: libc::SIGCHLD,
                        priority_entropy: None,
                        fd_permit: None,
                    },
                ),
            )
            .await
            .1;
        let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = prepared else {
            panic!("prepare");
        };
        let submitted = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(config),
                    mm,
                    GlobalRequest::SubmitNoSeqBirth(prepared),
                ),
            )
            .await
            .1;
        let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = submitted else {
            panic!("submit");
        };
        local.pending_no_seq_birth = Some(birth.clone());
        let child = DetTid::from_raw(62);
        if native_child {
            let (nr, args) = reverie::syscalls::Clone::new()
                .with_flags(
                    nix::sched::CloneFlags::from_bits(flags.bits().try_into().unwrap()).unwrap(),
                )
                .into_parts();
            parent_tool.on_injected_syscall_observed(
                Tid::from_raw(parent.as_raw()),
                state,
                &mut local,
                nr,
                args,
                InjectedSyscallEvent::ChildCreated(Tid::from_raw(child.as_raw())),
            );
        }
        let process = if flags.contains(CloneFlags::CLONE_THREAD) {
            parent
        } else {
            child
        };
        let tool: Detcore = Detcore::new(Tid::from_raw(process.as_raw()), config);
        let child_local = tool.init_thread_state(
            Tid::from_raw(child.as_raw()),
            Some((Tid::from_raw(parent.as_raw()), &local)),
        );
        (birth, tool, child_local)
    }

    #[tokio::test]
    async fn actual_terminal_prestart_child_preserves_wait_without_scheduler_admission() {
        use std::future::Future;

        use reverie::Tool;

        use crate::types::ChildWaitExitClass;
        use crate::types::ChildWaitSelector;
        use crate::types::ChildWaitSpec;
        use crate::types::ExactChildWaitState;
        for flags in [
            CloneFlags::empty(),
            CloneFlags::CLONE_FILES,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_VFORK,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_FILES | CloneFlags::CLONE_THREAD,
        ] {
            let (config, state, parent, mm) = custody_birth_fixture(false);
            let (birth, tool, mut child_local) =
                observed_prestart_child_fixture(&config, &state, parent, flags, true).await;
            let child = child_local.dettid;
            let clocks = serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap();
            let committed = state.sched.lock().unwrap().committed_time;
            assert!(!child_local.thread_start_entered);
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .process_group_admission_busy()
            );
            let mut join = Box::pin(state.receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    mm,
                    GlobalRequest::JoinNoSeqBirth(birth.clone(), child),
                ),
            ));
            let (wakes, waker) = custody_test_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(join.as_mut().poll(&mut cx).is_pending());
            let before = wakes.0.load(std::sync::atomic::Ordering::SeqCst);
            let status = ExitStatus::Signaled(reverie::Signal::SIGKILL, false);
            tool.on_backend_thread_terminal(
                Tid::from_raw(child.as_raw()),
                &state,
                &mut child_local,
                status,
            );
            assert!(!state.sched.lock().unwrap().backend_failed());
            assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) > before);
            assert_eq!(join.await.1, GlobalResponse::JoinNoSeqBirth(true));
            assert_eq!(
                state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            mm,
                            GlobalRequest::JoinNoSeqBirth(birth, child),
                        )
                    )
                    .await
                    .1,
                GlobalResponse::JoinNoSeqBirth(false)
            );
            // Exercise the real Detcore consuming callback after the native
            // terminal hook; a mock Observer cannot prove this cleanup path.
            let rpc = NetworkExitRpc {
                state: &state,
                sender: child,
            };
            tool.on_exit_thread(Tid::from_raw(child.as_raw()), &rpc, child_local, status)
                .await
                .unwrap();
            let mut sched = state.sched.lock().unwrap();
            assert!(!sched.thread_was_registered(child));
            assert!(!sched.next_turns.contains_key(&child));
            assert!(!sched.priorities.contains_key(&child));
            assert!(!sched.run_queue.contains_tid(child));
            assert_eq!(sched.committed_time, committed);
            assert_eq!(sched.thread_tree.pending_no_seq_birth_count(), 0);
            assert!(!sched.thread_tree.process_group_admission_busy());
            assert!(sched.process_signal_targets(child).is_empty());
            if !flags.contains(CloneFlags::CLONE_THREAD) {
                assert_eq!(
                    sched.exact_child_wait_state(parent, child),
                    ExactChildWaitState::PhysicallyExited
                );
                let spec = ChildWaitSpec {
                    selector: ChildWaitSelector::Exact(child),
                    owner: Some(parent),
                    exit_class: ChildWaitExitClass::Sigchld,
                };
                assert_eq!(sched.ready_child_wait(parent, spec), Some(child));
                assert!(sched.has_child_wait_target(parent, spec));
                assert_eq!(sched.thread_tree.process_group(child), Some(parent));
                assert!(sched.consume_child_wait(parent, child));
                assert!(!sched.consume_child_wait(parent, child));
                assert!(!sched.has_child_wait_target(parent, spec));
            } else {
                assert_eq!(
                    sched.exact_child_wait_state(parent, child),
                    ExactChildWaitState::Unknown
                );
            }
            drop(sched);
            assert!(!state.global_time.lock().unwrap().contains_thread(child));
            assert_eq!(
                serde_json::to_value(&*state.global_time.lock().unwrap()).unwrap(),
                clocks
            );
        }
    }

    #[tokio::test]
    async fn actual_terminal_prestart_rejects_missing_native_wrong_mm_or_started_child() {
        use reverie::Tool;
        for wrong in ["native", "MM", "started"] {
            let (config, state, parent, _) = custody_birth_fixture(false);
            let (_, tool, mut child) = observed_prestart_child_fixture(
                &config,
                &state,
                parent,
                CloneFlags::empty(),
                wrong != "native",
            )
            .await;
            if wrong == "MM" {
                child.mm_id = child.mm_id.for_exec(child.dettid);
            }
            if wrong == "started" {
                child.thread_start_entered = true;
            }
            let tid = child.dettid;
            tool.on_backend_thread_terminal(
                Tid::from_raw(tid.as_raw()),
                &state,
                &mut child,
                ExitStatus::Exited(0),
            );
            let sched = state.sched.lock().unwrap();
            assert!(sched.backend_failed(), "{wrong}");
            assert_eq!(sched.thread_tree.pending_no_seq_birth_count(), 1, "{wrong}");
            assert!(sched.thread_tree.process_group_admission_busy(), "{wrong}");
            assert!(!sched.thread_was_registered(tid));
            assert!(!state.global_time.lock().unwrap().contains_thread(tid));
        }
    }

    #[tokio::test]
    async fn actual_terminal_prestart_birth_survives_parent_consumption_and_refuses_repeat() {
        use reverie::Tool;
        let (config, state, parent, mm) = custody_birth_fixture(false);
        let (_, tool, mut child) =
            observed_prestart_child_fixture(&config, &state, parent, CloneFlags::empty(), true)
                .await;
        assert_eq!(
            state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::NoSeqBirthOwnerGone(None, None),
                    )
                )
                .await
                .1,
            GlobalResponse::NoSeqBirthOwnerGone(true)
        );
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_no_seq_birth_count(),
            1
        );
        let tid = child.dettid;
        tool.on_backend_thread_terminal(
            Tid::from_raw(tid.as_raw()),
            &state,
            &mut child,
            ExitStatus::Exited(0),
        );
        assert!(!state.sched.lock().unwrap().backend_failed());
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .pending_no_seq_birth_count(),
            0
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_tree
                .process_group_admission_busy()
        );
        tool.on_backend_thread_terminal(
            Tid::from_raw(tid.as_raw()),
            &state,
            &mut child,
            ExitStatus::Exited(0),
        );
        assert!(
            state.sched.lock().unwrap().backend_failed(),
            "a duplicate final-wait hook cannot consume twice"
        );
    }

    #[tokio::test]
    async fn observed_vfork_child_releases_group_gate_before_parent_native_return() {
        use reverie::InjectedSyscallEvent;
        use reverie::Tool;
        use reverie::syscalls::SyscallInfo;
        for flags in [
            CloneFlags::empty(),
            CloneFlags::CLONE_FILES,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_VFORK,
        ] {
            let (config, state, parent, mm) = custody_birth_fixture(false);
            let child = DetTid::from_raw(62);
            let prepared = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::PrepareNoSeqBirth {
                            process: parent,
                            syscall_count: 1,
                            flags,
                            child_tid_addr: 0,
                            exit_signal: libc::SIGCHLD,
                            priority_entropy: None,
                            fd_permit: None,
                        },
                    ),
                )
                .await
                .1;
            let GlobalResponse::PrepareNoSeqBirth(Some(prepared)) = prepared else {
                panic!("prepare");
            };
            let submitted = state
                .receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        DetTime::new(&config),
                        mm,
                        GlobalRequest::SubmitNoSeqBirth(prepared),
                    ),
                )
                .await
                .1;
            let GlobalResponse::PrepareNoSeqBirth(Some(birth)) = submitted else {
                panic!("submit");
            };
            let tool: Detcore = Detcore::new(Tid::from_raw(parent.as_raw()), &config);
            let mut local = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
            local.detpid = Some(parent);
            local.stats.syscall_count = 1;
            local.clone_flags = Some(flags);
            local.pending_no_seq_birth = Some(birth.clone());
            let (nr, args) = reverie::syscalls::Clone::new()
                .with_flags(
                    nix::sched::CloneFlags::from_bits(flags.bits().try_into().unwrap()).unwrap(),
                )
                .into_parts();
            tool.on_injected_syscall_observed(
                Tid::from_raw(parent.as_raw()),
                &state,
                &mut local,
                nr,
                args,
                InjectedSyscallEvent::ChildCreated(Tid::from_raw(child.as_raw())),
            );
            let mut child_local = tool.init_thread_state(
                Tid::from_raw(child.as_raw()),
                Some((Tid::from_raw(parent.as_raw()), &local)),
            );
            let bound = child_local.pending_no_seq_birth.take().unwrap();
            let registered = state
                .receive_rpc(
                    Tid::from_raw(child.as_raw()),
                    (
                        DetTime::new(&config),
                        child_local.mm_id,
                        GlobalRequest::CreateNoSeqChildThread(bound, None, Some(DEFAULT_PRIORITY)),
                    ),
                )
                .await
                .1;
            assert_eq!(registered, GlobalResponse::CreateChildThread(None));
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .process_group_admission_busy()
            );
            assert!(!state.sched.lock().unwrap().backend_failed());
            // Parent Join/ordinary scalar return has not occurred. A real vfork
            // child must already be able to enter its own group transition.
            let group = state
                .receive_rpc(
                    Tid::from_raw(child.as_raw()),
                    (
                        DetTime::new(&config),
                        child_local.mm_id,
                        GlobalRequest::PrepareProcessGroupChange(
                            crate::scheduler::ProcessGroupChangeKind::Session { process: child },
                            1,
                        ),
                    ),
                )
                .await
                .1;
            assert!(matches!(group, GlobalResponse::ProcessGroupChange(Some(_))));
        }
    }
}

#[cfg(test)]
mod robust_exit_clock_tests {
    use std::sync::Mutex;
    use std::task::Poll;

    use nix::sys::signal::Signal;
    use reverie::ExitStatus;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Tid;
    use reverie::Tool;

    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use crate::Detcore;
    use crate::ThreadState;
    use crate::config::Config;
    use crate::ivar::Ivar;
    use crate::resources::Resources;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::SkipTurn;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_local::RobustListExit;
    use crate::tool_local::RobustListWake;
    use crate::types::*;

    #[derive(Debug, PartialEq)]
    struct WakeObservation {
        wakes: Vec<(DetTid, FutexID)>,
        counts: Vec<u64>,
        clocks: serde_json::Value,
        turn: u64,
    }

    #[derive(Debug)]
    struct RpcObservation {
        sender: DetTid,
        kind: &'static str,
        accepted: bool,
        clocks: serde_json::Value,
        queued: Vec<DetTid>,
        waiters: usize,
        turn: u64,
    }

    // This is only a sender-bound transport, as in Reverie's WrappedFrom.
    // Replies, clock accounting, wakes and deregistration come from GlobalState.
    struct ExitRpc<'a> {
        state: &'a GlobalState,
        sender: DetTid,
        observations: &'a Mutex<Vec<WakeObservation>>,
        rpc_observations: &'a Mutex<Vec<RpcObservation>>,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ExitRpc<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let kind = match &request.2 {
                GlobalRequest::RobustListWakes(wakes) if wakes.is_empty() => "empty-wake",
                GlobalRequest::RobustListWakes(_) => "wake",
                GlobalRequest::DeregisterThread(_) => "deregister",
                _ => panic!("unexpected exit RPC: {:?}", request.2),
            };
            let wakes = match &request.2 {
                GlobalRequest::RobustListWakes(wakes) if !wakes.is_empty() => Some(wakes.clone()),
                _ => None,
            };
            let response = self
                .state
                .receive_rpc(Tid::from_raw(self.sender.as_raw()), request)
                .await;
            let clocks = serde_json::to_value(&*self.state.global_time.lock().unwrap()).unwrap();
            {
                let sched = self.state.sched.lock().unwrap();
                self.rpc_observations.lock().unwrap().push(RpcObservation {
                    sender: self.sender,
                    kind,
                    accepted: !matches!(response.1, GlobalResponse::ThreadExited),
                    clocks: clocks.clone(),
                    queued: sched.run_queue.tids().copied().collect(),
                    waiters: sched
                        .blocked
                        .futex_waiters
                        .values()
                        .map(Vec::len)
                        .sum::<usize>(),
                    turn: sched.turn,
                });
            }
            if let Some(wakes) = wakes {
                let GlobalResponse::RobustListWakes(counts) = &response.1 else {
                    panic!("a complete admitted exit batch was refused: {response:?}");
                };
                let clocks =
                    serde_json::to_value(&*self.state.global_time.lock().unwrap()).unwrap();
                let turn = self.state.sched.lock().unwrap().turn;
                self.observations.lock().unwrap().push(WakeObservation {
                    wakes,
                    counts: counts.clone(),
                    clocks,
                    turn,
                });
            }
            response
        }

        fn config(&self) -> &Config {
            &self.state.cfg
        }
    }

    struct Fixture {
        state: GlobalState,
        tool: Detcore,
        owners: [ThreadState<()>; 2],
        initial_clocks: [DetTime; 2],
        waiters: [DetTid; 2],
        peer: DetTid,
        futexes: [FutexID; 2],
        observations: Mutex<Vec<WakeObservation>>,
        rpc_observations: Mutex<Vec<RpcObservation>>,
    }

    impl Fixture {
        fn new(
            reason: RobustListExit,
            equal_clocks: bool,
            empty_owner: Option<usize>,
            cancel_killed_thread_rpcs: bool,
        ) -> Self {
            let config = Config {
                sequentialize_threads: true,
                cancel_killed_thread_rpcs,
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let parent = DetTid::from_raw(1);
            let leader = DetTid::from_raw(17);
            let worker = DetTid::from_raw(18);
            let waiters = [DetTid::from_raw(21), DetTid::from_raw(23)];
            let peer = DetTid::from_raw(25);
            let mm = MmId::initial(leader);
            let mut first = ThreadState::new(leader, &config, ());
            first.detpid = Some(leader);
            let mut second = first.clone();
            second.dettid = worker;
            let mut inherited = DetTime::new(&config);
            inherited.add_syscall_with_cost(1_000);
            let first_initial = inherited.clone_for_child();
            inherited.add_syscall_with_cost(370);
            let second_initial = inherited.clone_for_child();
            first.thread_logical_time = first_initial.clone();
            second.thread_logical_time = second_initial.clone();
            first
                .thread_logical_time
                .add_syscall_with_cost(if equal_clocks { 407 } else { 37 });
            second.thread_logical_time.add_syscall_with_cost(37);
            first.record_robust_list_head(Some(0x404100));
            second.record_robust_list_head(Some(0x404200));
            let object = SharedMemoryObjectId::Anonymous {
                origin: MmId::initial(parent),
                sequence: 1,
            };
            let futexes = [FutexID::shared(object, 0), FutexID::shared(object, 8)];
            first.stage_robust_list_wakes(
                reason,
                vec![
                    (
                        worker,
                        if empty_owner == Some(1) {
                            Vec::new()
                        } else {
                            vec![RobustListWake { futex: futexes[1] }]
                        },
                    ),
                    (
                        leader,
                        if empty_owner == Some(0) {
                            Vec::new()
                        } else {
                            vec![RobustListWake { futex: futexes[0] }]
                        },
                    ),
                ],
            );
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(parent, parent, true);
                sched.thread_tree.add_child(parent, leader, true);
                sched.thread_tree.add_child(leader, worker, false);
                for tid in [waiters[0], waiters[1], peer] {
                    sched.thread_tree.add_child(parent, tid, true);
                }
                for tid in [leader, worker, waiters[0], waiters[1], peer] {
                    sched.priorities.insert(tid, DEFAULT_PRIORITY);
                    sched.next_turns.insert(
                        tid,
                        ThreadNextTurn {
                            dettid: tid,
                            child_tid_addr: 0,
                            req: Ivar::new(),
                            resp: Ivar::new(),
                            protocol: Default::default(),
                        },
                    );
                    sched.install_test_exec_incarnation(
                        tid,
                        if tid == leader || tid == worker {
                            mm
                        } else {
                            MmId::initial(tid)
                        },
                    );
                }
                // Both owners have returned from their last guest turn and
                // retain its empty next request until their exit callbacks.
                // The independent peer is ready, but cannot pass quiescence.
                sched.runqueue_push_back(leader);
                sched.runqueue_push_back(worker);
                sched.runqueue_push_back(peer);
                sched.next_turns[&peer].req.put(Ok(Resources::new(peer)));
                for (waiter, futex) in waiters.into_iter().zip(futexes) {
                    sched.sleep_futex_waiter(&waiter, futex, None, u32::MAX);
                }
            }
            {
                let mut time = state.global_time.lock().unwrap();
                for (tid, clock) in [(leader, &first_initial), (worker, &second_initial)] {
                    time.update_global_time(tid, clock.as_nanos(), clock.inherited_nanos());
                }
            }
            let tool = Detcore::new(Tid::from_raw(leader.as_raw()), &config);
            Self {
                state,
                tool,
                owners: [first, second],
                initial_clocks: [first_initial, second_initial],
                waiters,
                peer,
                futexes,
                observations: Mutex::new(Vec::new()),
                rpc_observations: Mutex::new(Vec::new()),
            }
        }

        fn assert_clocks(&self, completed: &[usize]) -> serde_json::Value {
            let time = self.state.global_time.lock().unwrap();
            let snapshot = serde_json::to_value(&*time).unwrap();
            let epoch = DetTime::new(&self.state.cfg).as_nanos();
            let mut expected = epoch;
            for (index, owner) in self.owners.iter().enumerate() {
                let clock = if completed.contains(&index) {
                    &owner.thread_logical_time
                } else {
                    &self.initial_clocks[index]
                };
                assert_eq!(time.threads_time(owner.dettid), clock.as_nanos());
                assert_eq!(
                    snapshot["inherited_time"][owner.dettid.as_raw().to_string()],
                    serde_json::to_value(clock.inherited_nanos()).unwrap()
                );
                expected = expected + (clock.as_nanos() - epoch - clock.inherited_nanos());
            }
            assert_eq!(
                time.as_nanos(),
                expected,
                "only each owner's own uninherited work contributes"
            );
            snapshot
        }

        async fn exit(&self, index: usize, status: ExitStatus) {
            self.exit_thread(self.owners[index].clone(), status).await;
        }

        async fn exit_thread(&self, thread: ThreadState<()>, status: ExitStatus) {
            let rpc = ExitRpc {
                state: &self.state,
                sender: thread.dettid,
                observations: &self.observations,
                rpc_observations: &self.rpc_observations,
            };
            let exit =
                self.tool
                    .on_exit_thread(Tid::from_raw(rpc.sender.as_raw()), &rpc, thread, status);
            let mut exit = std::pin::pin!(exit);
            // On the CLI's current-thread ptrace route, Ready on the first
            // poll excludes a new queued-observer window inside this callback.
            // Multi-thread embeddings still admit concurrent scheduler reads;
            // the RPC observations separately check the actual global actions.
            assert!(
                matches!(futures::poll!(exit.as_mut()), Poll::Ready(Ok(()))),
                "exit callback yielded before its accounting and cleanup completed"
            );
        }

        async fn nonmember_exit(&self, raw_tid: i32) {
            let mut thread = self.owners[0].clone();
            thread.dettid = DetTid::from_raw(raw_tid);
            thread.thread_logical_time = DetTime::new(&self.state.cfg);
            let tid = thread.dettid;
            {
                let mut sched = self.state.sched.lock().unwrap();
                sched
                    .thread_tree
                    .add_child(self.owners[0].dettid, tid, false);
                sched.priorities.insert(tid, DEFAULT_PRIORITY);
                sched.next_turns.insert(
                    tid,
                    ThreadNextTurn {
                        dettid: tid,
                        child_tid_addr: 0,
                        req: Ivar::new(),
                        resp: Ivar::new(),
                        protocol: Default::default(),
                    },
                );
                sched.install_test_exec_incarnation(tid, thread.mm_id);
                sched.runqueue_push_back(tid);
            }
            let before = self.rpc_observations.lock().unwrap().len();
            self.exit_thread(thread, ExitStatus::Exited(0)).await;
            let observations = self.rpc_observations.lock().unwrap();
            assert_eq!(
                observations.len(),
                before + 1,
                "a nonmember sent an acknowledgement or wake RPC"
            );
            assert_eq!(observations[before].sender, tid);
            assert_eq!(observations[before].kind, "deregister");
            assert!(observations[before].accepted);
        }
    }

    #[tokio::test]
    async fn robust_exit_acknowledgements_preserve_global_action_order_and_eligibility() {
        for order in [[0, 1], [1, 0]] {
            for empty_owner in [0, 1] {
                let f = Fixture::new(RobustListExit::ExitGroup, false, Some(empty_owner), false);
                f.nonmember_exit(30).await;
                assert!(f.observations.lock().unwrap().is_empty());
                let queue_before_first: Vec<_> = f
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .tids()
                    .copied()
                    .collect();
                let first_start = f.rpc_observations.lock().unwrap().len();
                f.exit(order[0], ExitStatus::Exited(0)).await;
                let first_clocks = f.assert_clocks(&[order[0]]);
                {
                    let observations = f.rpc_observations.lock().unwrap();
                    let actions = &observations[first_start..];
                    assert_eq!(
                        actions.iter().map(|a| a.kind).collect::<Vec<_>>(),
                        ["empty-wake", "deregister"]
                    );
                    assert!(actions.iter().all(|a| a.accepted
                        && a.sender == f.owners[order[0]].dettid
                        && a.clocks == first_clocks
                        && a.waiters == 2
                        && a.turn == 0));
                    assert_eq!(
                        actions[0].queued, queue_before_first,
                        "empty acknowledgement changed scheduler eligibility"
                    );
                }
                f.nonmember_exit(31).await;
                f.assert_clocks(&[order[0]]);
                assert!(
                    f.observations.lock().unwrap().is_empty(),
                    "a nonmember completed the physical-exit barrier"
                );
                let queue_before_last: Vec<_> = f
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .tids()
                    .copied()
                    .collect();
                let last_start = f.rpc_observations.lock().unwrap().len();
                f.exit(order[1], ExitStatus::Exited(0)).await;
                let final_clocks = f.assert_clocks(&[0, 1]);
                {
                    let observations = f.rpc_observations.lock().unwrap();
                    let actions = &observations[last_start..];
                    assert_eq!(
                        actions.iter().map(|a| a.kind).collect::<Vec<_>>(),
                        ["empty-wake", "wake", "deregister"]
                    );
                    assert!(actions.iter().all(|a| a.accepted
                        && a.sender == f.owners[order[1]].dettid
                        && a.clocks == final_clocks
                        && a.turn == 0));
                    assert_eq!(actions[0].waiters, 2);
                    assert_eq!(actions[1].waiters, 1);
                    assert_eq!(actions[0].queued, queue_before_last);
                    assert_eq!(
                        actions[1].queued, queue_before_last,
                        "wake bypassed deferred admission"
                    );
                }
                f.nonmember_exit(32).await;
                f.assert_clocks(&[0, 1]);
                assert_eq!(f.observations.lock().unwrap().len(), 1);
                let observations = f.rpc_observations.lock().unwrap();
                for owner in &f.owners {
                    assert_eq!(
                        observations
                            .iter()
                            .filter(|a| a.sender == owner.dettid && a.kind == "empty-wake")
                            .count(),
                        1,
                        "each unique matching owner, including an empty-wake owner, must acknowledge before the batch clears"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn robust_exit_callbacks_preserve_owner_clocks_across_arrival_orders() {
        for reason in [
            RobustListExit::ExitGroup,
            RobustListExit::Signal(libc::SIGTERM),
        ] {
            for equal_clocks in [false, true] {
                for empty_owner in [None, Some(0), Some(1)] {
                    let mut results = Vec::new();
                    for order in [[0, 1], [1, 0]] {
                        let f = Fixture::new(reason, equal_clocks, empty_owner, false);
                        let status = match reason {
                            RobustListExit::ExitGroup => ExitStatus::Exited(0),
                            RobustListExit::Signal(_) => {
                                ExitStatus::Signaled(Signal::SIGTERM, false)
                            }
                        };
                        f.exit(order[0], status).await;
                        f.assert_clocks(&[order[0]]);
                        assert!(f.observations.lock().unwrap().is_empty());
                        {
                            let sched = f.state.sched.lock().unwrap();
                            assert_eq!(
                                sched
                                    .blocked
                                    .futex_waiters
                                    .values()
                                    .map(Vec::len)
                                    .sum::<usize>(),
                                2
                            );
                            assert_eq!(sched.turn, 0);
                        }
                        // An actually runnable peer must still wait for the
                        // remaining owner's outstanding next request.
                        {
                            let last = Err(SkipTurn);
                            let turn = crate::scheduler::do_a_turn_blocking(
                                f.state.sched.clone(),
                                f.state.global_time.clone(),
                                &last,
                            );
                            let mut turn = std::pin::pin!(turn);
                            assert!(matches!(futures::poll!(turn.as_mut()), Poll::Pending));
                        }
                        f.exit(order[0], status).await;
                        f.assert_clocks(&[order[0]]);
                        assert!(
                            f.observations.lock().unwrap().is_empty(),
                            "duplicate physical exit released an incomplete group"
                        );
                        f.exit(order[1], status).await;
                        let clocks = f.assert_clocks(&[0, 1]);
                        {
                            let observations = f.observations.lock().unwrap();
                            assert_eq!(observations.len(), 1);
                            let expected: Vec<_> = (0..2)
                                .filter(|i| Some(*i) != empty_owner)
                                .map(|i| (f.owners[i].dettid, f.futexes[i]))
                                .collect();
                            assert_eq!(observations[0].wakes, expected);
                            assert_eq!(observations[0].counts, vec![1; expected.len()]);
                            assert_eq!(
                                observations[0].clocks, clocks,
                                "all owner clocks must be accounted before wake admission"
                            );
                            assert_eq!(observations[0].turn, 0);
                        }
                        f.exit(order[1], status).await;
                        assert_eq!(f.assert_clocks(&[0, 1]), clocks);
                        assert_eq!(
                            f.observations.lock().unwrap().len(),
                            1,
                            "repeated cleanup emitted a second batch"
                        );
                        {
                            let sched = f.state.sched.lock().unwrap();
                            assert!(!sched.run_queue.contains_tid(f.waiters[0]));
                            assert!(!sched.run_queue.contains_tid(f.waiters[1]));
                        }
                        // Drive the real step1/step2 drain and one peer turn;
                        // no test-only scheduler implementation or reply shim.
                        let result = crate::scheduler::do_a_turn_blocking(
                            f.state.sched.clone(),
                            f.state.global_time.clone(),
                            &Err(SkipTurn),
                        )
                        .await;
                        assert!(result.is_ok());
                        let sched = f.state.sched.lock().unwrap();
                        assert_eq!(sched.turn, 1);
                        let queued: Vec<_> = sched.run_queue.tids().copied().collect();
                        for (i, waiter) in f.waiters.iter().enumerate() {
                            assert_eq!(queued.contains(waiter), Some(i) != empty_owner);
                        }
                        assert!(queued.contains(&f.peer));
                        results.push((
                            clocks,
                            queued,
                            std::mem::take(&mut *f.observations.lock().unwrap()),
                        ));
                    }
                    assert_eq!(
                        results[0], results[1],
                        "callback order changed final clocks, typed wake observations or the actual scheduler drain"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn robust_exit_callbacks_keep_rejected_or_mismatched_batches_incomplete() {
        for rejection in ["old-mm", "tombstone", "wrong-signal", "normal-exit"] {
            let f = Fixture::new(
                RobustListExit::Signal(libc::SIGTERM),
                false,
                None,
                rejection == "tombstone",
            );
            let rejected = f.owners[0].dettid;
            let mm = f.owners[0].mm_id;
            f.exit(1, ExitStatus::Signaled(Signal::SIGTERM, false))
                .await;
            let replacement_request = Ivar::new();
            if rejection == "old-mm" {
                let mut sched = f.state.sched.lock().unwrap();
                sched.install_test_exec_incarnation(rejected, mm.for_exec(rejected));
                sched.next_turns.insert(
                    rejected,
                    ThreadNextTurn {
                        dettid: rejected,
                        child_tid_addr: 0,
                        req: replacement_request.clone(),
                        resp: Ivar::new(),
                        protocol: Default::default(),
                    },
                );
                let mut replacement_time = f.owners[0].thread_logical_time.clone();
                replacement_time.add_syscall_with_cost(500);
                f.state.global_time.lock().unwrap().update_global_time(
                    rejected,
                    replacement_time.as_nanos(),
                    replacement_time.inherited_nanos(),
                );
            } else if rejection == "tombstone" {
                // Use the real cancelling backend gate; keep its matching Mm.
                let mut sched = f.state.sched.lock().unwrap();
                sched.logically_kill_thread(&rejected, &rejected, mm);
            }
            let before = serde_json::to_value(&*f.state.global_time.lock().unwrap()).unwrap();
            let status = match rejection {
                "wrong-signal" => ExitStatus::Signaled(Signal::SIGKILL, false),
                "normal-exit" => ExitStatus::Exited(0),
                _ => ExitStatus::Signaled(Signal::SIGTERM, false),
            };
            f.exit(0, status).await;
            assert!(
                f.observations.lock().unwrap().is_empty(),
                "{rejection} released a group"
            );
            if rejection == "old-mm" || rejection == "tombstone" {
                assert_eq!(
                    serde_json::to_value(&*f.state.global_time.lock().unwrap()).unwrap(),
                    before,
                    "rejected acknowledgement changed clock state"
                );
            }
            let sched = f.state.sched.lock().unwrap();
            assert_eq!(
                sched
                    .blocked
                    .futex_waiters
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
                2,
                "rejected group lost a real waiter"
            );
            assert!(
                f.waiters
                    .iter()
                    .all(|tid| !sched.run_queue.contains_tid(*tid))
            );
            if rejection == "old-mm" {
                assert!(sched.rpc_incarnation_matches(rejected, mm.for_exec(rejected)));
                assert_eq!(
                    sched.next_turns[&rejected].req, replacement_request,
                    "old cleanup destroyed replacement registration"
                );
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "Attempted to update tid 17 time")]
    async fn robust_exit_clock_ack_still_refuses_a_backwards_owner_sample() {
        let f = Fixture::new(RobustListExit::ExitGroup, false, None, false);
        let owner = &f.owners[0];
        let mut later = owner.thread_logical_time.clone();
        later.add_syscall_with_cost(1);
        f.state.global_time.lock().unwrap().update_global_time(
            owner.dettid,
            later.as_nanos(),
            later.inherited_nanos(),
        );
        f.exit(0, ExitStatus::Exited(0)).await;
    }

    #[tokio::test]
    async fn backend_failure_preserves_consuming_robust_exit_clock_accounting() {
        for order in [[0, 1], [1, 0]] {
            let f = Fixture::new(RobustListExit::ExitGroup, false, None, false);
            let selected = {
                let mut sched = f.state.sched.lock().unwrap();
                sched.next_turns.get_mut(&f.peer).unwrap().req = Ivar::new();
                sched.select_test_turn().unwrap()
            };
            let responses = {
                let sched = f.state.sched.lock().unwrap();
                f.waiters.map(|tid| sched.next_turns[&tid].resp.clone())
            };
            let mut turn = std::pin::pin!(crate::scheduler::finish_selected_turn(
                f.state.sched.clone(),
                f.state.global_time.clone(),
                selected.0,
                selected.1,
                selected.2,
            ));
            assert!(futures::poll!(turn.as_mut()).is_pending());
            f.state.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(17),
                tid: Tid::from_raw(18),
                phase: "native robust cleanup control",
            });
            for index in order {
                f.exit(index, ExitStatus::Exited(0)).await;
            }
            assert!(matches!(futures::poll!(turn.as_mut()), Poll::Ready(Err(_))));
            f.assert_clocks(&[0, 1]);
            let observations = f.observations.lock().unwrap();
            assert_eq!(observations.len(), 1, "one complete batch");
            assert_eq!(observations[0].counts, vec![1, 1]);
            assert!(
                responses
                    .iter()
                    .all(|response| response.try_read().is_none())
            );
            let mut sched = f.state.sched.lock().unwrap();
            assert_eq!(sched.turn, 0);
            for owner in &f.owners {
                assert!(!sched.next_turns.contains_key(&owner.dettid));
                assert!(!sched.note_deregistration_accounted(owner.dettid));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod native_prestart_tests;

#[cfg(test)]
pub(crate) mod native_clear_tid_tests;
