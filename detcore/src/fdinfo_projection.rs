/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Read-only target metadata for fdinfo. Acquisition and scheduler admission
//! remain the caller's responsibility. This module does not issue syscalls,
//! normalize a raw snapshot, classify a parent, or mint an unmanaged cookie.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;

use crate::fd::FdType;
use crate::resources::ResourceID;
use crate::types::DetTid;
use crate::types::FilesId;
use crate::types::MmId;
use crate::types::OpenFileId;
use crate::types::RawFd;
use crate::types::RawInode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoRawObject {
    pub inode: RawInode,
    pub mode: u32,
    pub mount_id: u64,
}

impl FdinfoRawObject {
    fn same_object(self, other: Self) -> bool {
        self.inode == other.inode
            && self.mount_id == other.mount_id
            && (self.mode & libc::S_IFMT) == (other.mode & libc::S_IFMT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoOfdProjection {
    pub open_file: OpenFileId,
    pub status_flags: i32,
    pub resource: Option<ResourceID>,
    pub ty: FdType,
    pub object: Option<FdinfoRawObject>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoSlotProjection {
    pub open_file: OpenFileId,
    pub cloexec: bool,
}

/// Setter events, including same-valued local assignments. A mask carries the
/// actual setter's field semantics; a stale entire flags word is not an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoOfdDelta {
    Seed(FdinfoOfdProjection),
    StatusFlags {
        open_file: OpenFileId,
        mask: i32,
        value: i32,
    },
    Resource {
        open_file: OpenFileId,
        resource: Option<ResourceID>,
    },
    Type {
        open_file: OpenFileId,
        ty: FdType,
    },
    Object {
        open_file: OpenFileId,
        object: Option<FdinfoRawObject>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoSlotDelta {
    Install {
        files: FilesId,
        fd: RawFd,
        slot: FdinfoSlotProjection,
    },
    Remove {
        files: FilesId,
        fd: RawFd,
        expected: OpenFileId,
    },
    Cloexec {
        files: FilesId,
        fd: RawFd,
        expected: OpenFileId,
        cloexec: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoTableOrigin {
    Initial,
    Fork { source: FilesId },
    Exec { source: FilesId },
}

/// Only a genuinely new table may consume this snapshot. A fork snapshot is
/// from the authenticated clone boundary, not the parent's later current table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoTableSeed {
    pub files: FilesId,
    pub origin: FdinfoTableOrigin,
    pub slots: Vec<(RawFd, FdinfoSlotProjection)>,
    pub ofds: Vec<FdinfoOfdProjection>,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct FdinfoIncarnation {
    pub tid: DetTid,
    pub mm: MmId,
}

/// Supplied only after the existing lifecycle/MM admission accepted the task.
/// None binds an existing shared table; Some permits one new table seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FdinfoFilesAdmission {
    pub files: FilesId,
    pub new_origin: Option<FdinfoTableOrigin>,
}

/// Not a wire type. An RPC wrapper cannot manufacture admission by decoding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FdinfoAdmission {
    owner: u64,
    incarnation: FdinfoIncarnation,
}

/// Issued only before an authenticated exec's held Table commit. The owner
/// keeps this non-wire, one-use permit until that exact replacement supplies
/// a positive raw task observation; no old subject follows a numeric PID.
#[derive(Debug)]
pub(crate) struct FdinfoDeferredExecIdentity {
    admission: FdinfoAdmission,
    predecessor: FdinfoIncarnation,
    files: FdinfoFilesAdmission,
    lifetime: Option<FdinfoTaskLifetime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct FdinfoProcView {
    pub procfs_root: RawInode,
    pub pid_namespace: RawInode,
    pub mount_namespace: RawInode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum FdinfoProcAliasKind {
    Process { pid: i32 },
    Task { pid: i32, tid: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct FdinfoProcAlias {
    pub view: FdinfoProcView,
    pub kind: FdinfoProcAliasKind,
    pub directory: RawInode,
}

/// A birth anchor and raw kernel start field authenticated against a retained
/// task-lifetime reference. Equal task numbers or different alias inodes do not
/// establish this identity. If acquisition cannot distinguish reuse, it must
/// leave the subject unbound. Local owner generations prevent a released or
/// retired subject from redirecting to a newly admitted task; they do not supply
/// the caller's missing kernel lifetime proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct FdinfoTaskLifetime {
    pub birth_anchor: RawInode,
    pub start_ticks: u64,
}

/// Captured when the proc file is opened. It binds a task, not that task's old
/// descriptor table. Exec may change the current FilesId without changing this
/// held task lifetime; PID reuse must never redirect this handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoBoundSubject {
    alias: FdinfoProcAlias,
    lifetime: FdinfoTaskLifetime,
    owner: u64,
}

/// Private ownership of an authenticated pending fork/exec seed. The matching
/// admitted Table event adopts it; canceled lifecycle paths release it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FdinfoPendingSeed {
    owner: u64,
    files: FilesId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoProcViewCoverage {
    Partial,
    /// Real proc-view acquisition and complete lifecycle/alias export are both
    /// required. This is not inferred from the modeled task count. Logical KVM
    /// IDs, pending newborn/exec transitions and partial views cannot supply it.
    Complete {
        members: Vec<FdinfoTaskLifetime>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoProjectionDelta {
    Table(FdinfoTableSeed),
    Ofd(FdinfoOfdDelta),
    Slot(FdinfoSlotDelta),
    BindTask {
        lifetime: FdinfoTaskLifetime,
        previous: Option<FdinfoIncarnation>,
    },
    BindAlias {
        alias: FdinfoProcAlias,
        lifetime: FdinfoTaskLifetime,
    },
    InvalidateViews,
    View {
        view: FdinfoProcView,
        coverage: FdinfoProcViewCoverage,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoProjectionBatch {
    pub sequence: u64,
    pub deltas: Vec<FdinfoProjectionDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FdinfoProjectionError {
    InvalidAdmission,
    StaleAdmission,
    Sequence,
    ExistingTable,
    TableOrigin,
    InvalidSlot,
    UnknownOfd,
    ObjectConflict,
    InvalidFlags,
    ForeignMutation,
    SlotChanged,
    TaskChanged,
    AliasChanged,
    IncompleteView,
    Overflow,
    StaleOwner,
}

/// These are projection gaps, not replacement Linux errno values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoProjectionGap {
    RejectedUpdate,
    UnboundAlias,
    RetiredTask,
    UnknownTask,
    PartialView,
    PartialTable,
    MissingSlot,
    MissingObject,
    ObjectChanged,
    MissingLogicalMetadata,
    UnsupportedMountView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoSubjectClass {
    Managed {
        incarnation: FdinfoIncarnation,
        files: FilesId,
    },
    /// Only a complete actual view/export supports this classification. It
    /// grants no flags, stdio ResourceID, cookie, normalizer or parent authority.
    OutsideToolInCompleteView,
}

/// Owned acquisition evidence for recording with the successful raw snapshot.
/// Rendering/replay consumes these values without re-querying a live target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoCapturedTarget {
    pub subject: FdinfoBoundSubject,
    pub incarnation: FdinfoIncarnation,
    pub files: FilesId,
    pub slot: FdinfoSlotProjection,
    pub ofd: FdinfoOfdProjection,
    pub observed_object: FdinfoRawObject,
    pub revision: u64,
}

/// Owned logical evidence for a separately authenticated current actor.
/// It has no physical proc alias, task start field, or namespace authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoCapturedCurrentTarget {
    pub incarnation: FdinfoIncarnation,
    pub files: FilesId,
    pub source_fd: RawFd,
    pub source: OpenFileId,
    pub target_fd: RawFd,
    pub slot: FdinfoSlotProjection,
    pub ofd: FdinfoOfdProjection,
    pub observed_object: FdinfoRawObject,
    pub revision: u64,
}

/// Issued only through the integration's actual registered actor/MM gate.
/// Keeping this separate cannot turn an absent proc lifetime into a subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FdinfoAdmittedSlotCapture {
    admission: FdinfoAdmission,
    files: FilesId,
    source_fd: RawFd,
    source_slot: FdinfoSlotProjection,
    target_fd: RawFd,
    target_slot: FdinfoSlotProjection,
    ofd: FdinfoOfdProjection,
    anchor: Option<FdinfoRawObject>,
    revision: u64,
}

impl FdinfoCapturedCurrentTarget {
    pub(crate) fn fdinfo_flags(&self) -> i32 {
        self.ofd.status_flags
            | if self.slot.cloexec {
                libc::O_CLOEXEC
            } else {
                0
            }
    }
}

/// Positive current-slot authority captured before acquiring a raw target
/// object. Not a wire/admission type and not a retained guest descriptor or
/// cache lease. The issuing cache and its serialized turn remain the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FdinfoCurrentSlotCapture {
    subject: FdinfoBoundSubject,
    incarnation: FdinfoIncarnation,
    files: FilesId,
    fd: RawFd,
    slot: FdinfoSlotProjection,
    ofd: FdinfoOfdProjection,
    anchor: Option<FdinfoRawObject>,
    revision: u64,
}

impl FdinfoCapturedTarget {
    pub(crate) fn fdinfo_flags(&self) -> i32 {
        self.ofd.status_flags
            | if self.slot.cloexec {
                libc::O_CLOEXEC
            } else {
                0
            }
    }
}

#[derive(Debug, Clone)]
struct IncarnationState {
    incarnation: FdinfoIncarnation,
    files: FdinfoFilesAdmission,
    predecessor: Option<FdinfoIncarnation>,
    last_sequence: u64,
    lifetime: Option<FdinfoTaskLifetime>,
    aliases: HashSet<FdinfoProcAlias>,
    views: HashSet<FdinfoProcView>,
    pending: Option<FilesId>,
}

#[derive(Debug)]
struct TableState {
    seeded: bool,
    slots: BTreeMap<RawFd, FdinfoSlotProjection>,
    /// Counts current slots, not admitted senders. Setter admission is local to
    /// the sender's current table even when a pending child holds the OFD.
    open_files: HashMap<OpenFileId, usize>,
    complete: bool,
    /// Active admitted senders and authenticated pending source snapshots.
    owners: usize,
}

impl TableState {
    fn reserved() -> Self {
        Self {
            seeded: false,
            slots: BTreeMap::new(),
            open_files: HashMap::new(),
            complete: false,
            owners: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct OfdState {
    projection: FdinfoOfdProjection,
    anchor: Option<FdinfoRawObject>,
    /// Current slots across all live tables plus one hold per pending snapshot
    /// that references this OFD. Captured targets own their values separately.
    references: usize,
}

#[derive(Debug, Clone)]
struct TaskState {
    current: Option<FdinfoIncarnation>,
    files: Option<FilesId>,
    generation: u64,
    live: bool,
    open_subjects: usize,
    aliases: HashSet<FdinfoProcAlias>,
}

#[derive(Debug, Clone)]
struct AliasState {
    lifetime: FdinfoTaskLifetime,
    owners: BTreeSet<u64>,
}

#[derive(Debug, Clone)]
struct ViewState {
    coverage: FdinfoProcViewCoverage,
    owner: u64,
    epoch: u64,
}

#[derive(Debug)]
struct PendingSeedState {
    handle: FdinfoPendingSeed,
    target: FdinfoIncarnation,
    parent: FdinfoIncarnation,
    source: FilesId,
    seed: FdinfoTableSeed,
    held_ofds: BTreeSet<OpenFileId>,
}

#[derive(Debug)]
struct OpenSubjectState {
    subject: FdinfoBoundSubject,
    generation: u64,
}

/// Undo is local to the actual changed keys. No ordinary slot/OFD event clones
/// a table, an unrelated object, the cache, or a history of retired owners.
#[derive(Debug)]
enum Undo {
    Ofd(OpenFileId, Option<OfdState>),
    Slot(FilesId, RawFd, Option<FdinfoSlotProjection>),
    TableOfd(FilesId, OpenFileId, Option<usize>),
    TableMetadata(FilesId, bool, bool),
    TableOwners(FilesId, usize),
    Task(FdinfoTaskLifetime, Option<TaskState>),
    SenderLifetime(u64, Option<FdinfoTaskLifetime>),
    Alias(FdinfoProcAlias, Option<AliasState>),
    SenderAlias(u64, FdinfoProcAlias, bool),
    View(FdinfoProcView, Option<ViewState>),
    SenderView(u64, FdinfoProcView, bool),
}

#[derive(Debug, Default)]
struct Transaction {
    undo: Vec<Undo>,
    ofds_to_collect: BTreeSet<OpenFileId>,
    adopted_seeds: Vec<FilesId>,
}

#[derive(Debug)]
pub(crate) struct FdinfoProjectionCache {
    valid: bool,
    revision: u64,
    next_owner: u64,
    view_epoch: u64,
    /// Only currently admitted senders. The private increasing token rejects a
    /// stale sender even after all of its old maps have been reclaimed.
    incarnations: BTreeMap<DetTid, u64>,
    senders: HashMap<u64, IncarnationState>,
    tables: HashMap<FilesId, TableState>,
    ofds: BTreeMap<OpenFileId, OfdState>,
    tasks: HashMap<FdinfoTaskLifetime, TaskState>,
    aliases: HashMap<FdinfoProcAlias, AliasState>,
    views: HashMap<FdinfoProcView, ViewState>,
    pending: HashMap<FilesId, PendingSeedState>,
    open_subjects: BTreeMap<u64, OpenSubjectState>,
}

impl Default for FdinfoProjectionCache {
    fn default() -> Self {
        Self {
            valid: true,
            revision: 0,
            next_owner: 0,
            view_epoch: 0,
            incarnations: BTreeMap::new(),
            senders: HashMap::new(),
            tables: HashMap::new(),
            ofds: BTreeMap::new(),
            tasks: HashMap::new(),
            aliases: HashMap::new(),
            views: HashMap::new(),
            pending: HashMap::new(),
            open_subjects: BTreeMap::new(),
        }
    }
}

impl FdinfoProjectionCache {
    /// The existing scheduler/lifecycle/MM admission must already have accepted
    /// this task. Fork/exec seeds must have been held at the actual cut, before
    /// their source loses slots or exits; a late child snapshot is not a hold.
    pub(crate) fn register_after_admission(
        &mut self,
        incarnation: FdinfoIncarnation,
        files: FdinfoFilesAdmission,
    ) -> Result<FdinfoAdmission, FdinfoProjectionError> {
        self.validate_registration(incarnation, files, None)?;
        let owner = self
            .next_owner
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        let epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        if let Some(table) = self.tables.get(&files.files) {
            table
                .owners
                .checked_add(1)
                .ok_or(FdinfoProjectionError::Overflow)?;
        }
        self.next_owner = owner;
        self.view_epoch = epoch;
        if let Some(table) = self.tables.get_mut(&files.files) {
            table.owners += 1;
        } else {
            self.tables.insert(files.files, TableState::reserved());
        }
        self.incarnations.insert(incarnation.tid, owner);
        self.senders.insert(
            owner,
            IncarnationState {
                incarnation,
                files,
                predecessor: None,
                last_sequence: 0,
                lifetime: None,
                aliases: HashSet::new(),
                views: HashSet::new(),
                pending: files
                    .new_origin
                    .filter(|o| !matches!(o, FdinfoTableOrigin::Initial))
                    .map(|_| files.files),
            },
        );
        Ok(FdinfoAdmission { owner, incarnation })
    }

    /// Call only after successful exec/reconnect admission. The corresponding
    /// authentic new-table snapshot is held before the old table is released.
    /// Validation precedes these local changes; no global candidate is copied.
    pub(crate) fn replace_after_exec(
        &mut self,
        previous: FdinfoAdmission,
        incarnation: FdinfoIncarnation,
        files: FdinfoFilesAdmission,
    ) -> Result<FdinfoAdmission, FdinfoProjectionError> {
        let old = self.sender(previous)?;
        if incarnation.mm != old.incarnation.mm.for_exec(incarnation.tid)
            || files.new_origin
                != Some(FdinfoTableOrigin::Exec {
                    source: old.files.files,
                })
        {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        self.validate_registration(incarnation, files, Some(previous.owner))?;
        let owner = self
            .next_owner
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        let epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        let old = self
            .senders
            .remove(&previous.owner)
            .expect("validated sender");
        self.incarnations.remove(&old.incarnation.tid);
        self.next_owner = owner;
        self.view_epoch = epoch;
        self.tables.insert(files.files, TableState::reserved());
        self.incarnations.insert(incarnation.tid, owner);
        // Keep the exact task's alias joins across exec. Foreign publication
        // owned solely by the old incarnation can retire; open-subject owners
        // retain their task binding independently of a publication alias.
        let mut aliases = HashSet::new();
        for alias in old.aliases {
            let retain = self
                .aliases
                .get(&alias)
                .is_some_and(|a| Some(a.lifetime) == old.lifetime);
            if retain {
                let state = self.aliases.get_mut(&alias).expect("existing alias");
                state.owners.remove(&previous.owner);
                state.owners.insert(owner);
                aliases.insert(alias);
            } else {
                self.release_alias_owner(previous.owner, alias);
            }
        }
        for view in old.views {
            if self
                .views
                .get(&view)
                .is_some_and(|v| v.owner == previous.owner)
            {
                self.views.remove(&view);
            }
        }
        self.senders.insert(
            owner,
            IncarnationState {
                incarnation,
                files,
                predecessor: Some(old.incarnation),
                last_sequence: 0,
                lifetime: old.lifetime,
                aliases,
                views: HashSet::new(),
                pending: Some(files.files),
            },
        );
        self.release_table_owner(old.files.files);
        Ok(FdinfoAdmission { owner, incarnation })
    }

    /// Refresh a raw task binding only on an already authenticated exec
    /// replacement. A changed proc-directory observation supplies a new
    /// projection lifetime; it cannot redirect an older opened proc subject.
    pub(crate) fn refresh_exec_task_binding(
        &mut self,
        admission: FdinfoAdmission,
        observed: FdinfoTaskLifetime,
    ) -> Result<(), FdinfoProjectionError> {
        let sender = self.sender(admission)?;
        let predecessor = sender
            .predecessor
            .ok_or(FdinfoProjectionError::InvalidAdmission)?;
        if sender.pending.is_none()
            || sender.incarnation.mm != predecessor.mm.for_exec(sender.incarnation.tid)
        {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        self.refresh_exec_task_observation(admission, predecessor, observed)
    }

    pub(crate) fn defer_exec_task_binding(
        &self,
        admission: FdinfoAdmission,
    ) -> Result<FdinfoDeferredExecIdentity, FdinfoProjectionError> {
        let sender = self.sender(admission)?;
        let predecessor = sender
            .predecessor
            .ok_or(FdinfoProjectionError::InvalidAdmission)?;
        if sender.pending != Some(sender.files.files)
            || sender.last_sequence != 0
            || sender.incarnation.mm != predecessor.mm.for_exec(sender.incarnation.tid)
        {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        Ok(FdinfoDeferredExecIdentity {
            admission,
            predecessor,
            files: sender.files,
            lifetime: sender.lifetime,
        })
    }

    pub(crate) fn refresh_deferred_exec_task_binding(
        &mut self,
        admission: FdinfoAdmission,
        permit: FdinfoDeferredExecIdentity,
        observed: FdinfoTaskLifetime,
    ) -> Result<Option<FdinfoIncarnation>, FdinfoProjectionError> {
        let sender = self.sender(admission)?;
        if admission != permit.admission
            || sender.predecessor != Some(permit.predecessor)
            || sender.files != permit.files
            || sender.lifetime != permit.lifetime
            || sender.pending.is_some()
            || sender.last_sequence == 0
        {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        self.refresh_exec_task_observation(admission, permit.predecessor, observed)?;
        Ok(self
            .admitted_task_lifetime(admission)?
            .map(|_| permit.predecessor))
    }

    fn refresh_exec_task_observation(
        &mut self,
        admission: FdinfoAdmission,
        predecessor: FdinfoIncarnation,
        observed: FdinfoTaskLifetime,
    ) -> Result<(), FdinfoProjectionError> {
        let prior = self.sender(admission)?.lifetime;
        if let Some(prior) = prior
            && self
                .tasks
                .get(&prior)
                .is_none_or(|task| !task.live || task.current != Some(predecessor))
        {
            return Err(FdinfoProjectionError::TaskChanged);
        }
        if prior == Some(observed) {
            return Ok(());
        }
        // Existing publication under this raw lifetime is not a new exec
        // observation and cannot be adopted by object/PID equality.
        if self.tasks.contains_key(&observed) {
            return Err(FdinfoProjectionError::TaskChanged);
        }
        let epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        if let Some(prior) = prior {
            let aliases = {
                let task = self.tasks.get_mut(&prior).expect("validated former task");
                // This retires the sampled projection binding, not the kernel
                // task or its successful reads. Held old subjects stay owned
                // but unavailable instead of following a guessed new alias.
                task.live = false;
                task.current = None;
                task.files = None;
                std::mem::take(&mut task.aliases)
            };
            for alias in aliases {
                self.remove_alias(alias);
            }
            self.collect_task(prior);
        }
        self.senders
            .get_mut(&admission.owner)
            .expect("validated replacement sender")
            .lifetime = None;
        self.view_epoch = epoch;
        Ok(())
    }

    pub(crate) fn retire_after_admission(
        &mut self,
        admission: FdinfoAdmission,
    ) -> Result<(), FdinfoProjectionError> {
        self.sender(admission)?;
        let epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        let old = self
            .senders
            .remove(&admission.owner)
            .expect("validated sender");
        self.incarnations.remove(&old.incarnation.tid);
        self.view_epoch = epoch;
        if let Some(lifetime) = old.lifetime {
            if let Some(task) = self.tasks.get_mut(&lifetime) {
                task.live = false;
                task.current = None;
                task.files = None;
                // Positive lifecycle retirement invalidates all published aliases
                // for this lifetime, including another sender's copy.
                let aliases = std::mem::take(&mut task.aliases);
                for alias in aliases {
                    self.remove_alias(alias);
                }
            }
            self.collect_task(lifetime);
        }
        for alias in old.aliases {
            self.release_alias_owner(admission.owner, alias);
        }
        for view in old.views {
            if self
                .views
                .get(&view)
                .is_some_and(|v| v.owner == admission.owner)
            {
                self.views.remove(&view);
            }
        }
        if let Some(files) = old.pending {
            self.release_pending(files);
        }
        self.release_table_owner(old.files.files);
        Ok(())
    }

    /// Retain the real fork/exec snapshot before the source can close/reuse its
    /// slots. The caller has flushed prior mutations and authenticated this
    /// lifecycle cut. Unknown source members never become a complete census.
    pub(crate) fn reserve_pending_seed(
        &mut self,
        parent: FdinfoAdmission,
        target: FdinfoIncarnation,
        seed: FdinfoTableSeed,
    ) -> Result<FdinfoPendingSeed, FdinfoProjectionError> {
        let actor = self.sender(parent)?;
        if target.tid.as_raw() <= 0
            || self.pending.contains_key(&seed.files)
            || self.tables.contains_key(&seed.files)
        {
            return Err(FdinfoProjectionError::ExistingTable);
        }
        let source = match seed.origin {
            FdinfoTableOrigin::Fork { source }
                if source == actor.files.files
                    && seed.files == FilesId::forked(target.tid)
                    && target.tid != actor.incarnation.tid =>
            {
                source
            }
            FdinfoTableOrigin::Exec { source }
                if source == actor.files.files
                    && seed.files == source.for_exec(actor.incarnation.tid)
                    && target.mm == actor.incarnation.mm.for_exec(target.tid) =>
            {
                source
            }
            _ => return Err(FdinfoProjectionError::TableOrigin),
        };
        let parent_incarnation = actor.incarnation;
        let slots = self.validate_seed(&seed)?;
        let table = self
            .table(source)
            .ok_or(FdinfoProjectionError::TableOrigin)?;
        let mut inherited = 0;
        for (fd, slot) in &table.slots {
            if matches!(seed.origin, FdinfoTableOrigin::Exec { .. }) && slot.cloexec {
                continue;
            }
            inherited += 1;
            if slots.get(fd) != Some(slot) {
                return Err(FdinfoProjectionError::SlotChanged);
            }
        }
        if table.complete && inherited != slots.len() {
            return Err(FdinfoProjectionError::SlotChanged);
        }
        let owner = self
            .next_owner
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        let epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        let handle = FdinfoPendingSeed {
            owner,
            files: seed.files,
        };
        let held_ofds: BTreeSet<_> = slots.values().map(|s| s.open_file).collect();
        let mut tx = Transaction::default();
        let result = (|| {
            for projection in &seed.ofds {
                self.seed_ofd(projection.clone(), &mut tx)?;
            }
            for open_file in &held_ofds {
                self.change_ofd_reference(*open_file, true, &mut tx)?;
            }
            let table = self.tables.get_mut(&source).expect("validated source");
            let owners = table
                .owners
                .checked_add(1)
                .ok_or(FdinfoProjectionError::Overflow)?;
            tx.undo.push(Undo::TableOwners(source, table.owners));
            table.owners = owners;
            Ok(())
        })();
        if let Err(error) = result {
            self.rollback(tx.undo);
            return Err(error);
        }
        self.next_owner = owner;
        self.view_epoch = epoch;
        self.pending.insert(
            seed.files,
            PendingSeedState {
                handle,
                target,
                parent: parent_incarnation,
                source,
                seed,
                held_ofds,
            },
        );
        self.collect_ofds(tx.ofds_to_collect);
        Ok(handle)
    }

    /// Only a canceled, not-yet-admitted lifecycle may release this reservation.
    /// A live child's matching Table event releases it at successful commit.
    pub(crate) fn cancel_pending_seed(
        &mut self,
        handle: FdinfoPendingSeed,
    ) -> Result<(), FdinfoProjectionError> {
        let pending = self
            .pending
            .get(&handle.files)
            .filter(|p| p.handle == handle)
            .ok_or(FdinfoProjectionError::StaleOwner)?;
        if self
            .senders
            .values()
            .any(|s| s.pending == Some(handle.files))
        {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        let files = pending.handle.files;
        self.view_epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        self.release_pending(files);
        Ok(())
    }

    /// Exclusive synchronous application hides intermediate state. Rollback is
    /// proportional to actual deltas/touched entries; pruning happens only after
    /// commit. A rejected admitted mutation still invalidates proof authority.
    pub(crate) fn apply(
        &mut self,
        admission: FdinfoAdmission,
        batch: FdinfoProjectionBatch,
    ) -> Result<(), FdinfoProjectionError> {
        let sender = self.sender(admission)?;
        if sender.last_sequence.checked_add(1) != Some(batch.sequence) {
            self.valid = false;
            return Err(FdinfoProjectionError::Sequence);
        }
        let Some(revision) = self.revision.checked_add(1) else {
            self.valid = false;
            return Err(FdinfoProjectionError::Overflow);
        };
        let old_next_owner = self.next_owner;
        let old_epoch = self.view_epoch;
        let mut tx = Transaction::default();
        for delta in batch.deltas {
            if let Err(error) = self.apply_one(admission, delta, &mut tx) {
                self.rollback(tx.undo);
                self.next_owner = old_next_owner;
                self.view_epoch = old_epoch;
                self.valid = false;
                return Err(error);
            }
        }
        self.senders
            .get_mut(&admission.owner)
            .expect("validated sender")
            .last_sequence = batch.sequence;
        self.revision = revision;
        for files in tx.adopted_seeds {
            self.senders
                .get_mut(&admission.owner)
                .expect("validated sender")
                .pending = None;
            self.release_pending(files);
        }
        self.collect_ofds(tx.ofds_to_collect);
        Ok(())
    }

    /// Acquire one owner for an actual opened proc OFD. Dup/shared aliases of the
    /// same opened proc file share this owner; release at its last real close.
    /// No inference is made from a process number or process/task alias inode.
    pub(crate) fn bind_open_subject(
        &mut self,
        alias: FdinfoProcAlias,
        observed_lifetime: FdinfoTaskLifetime,
    ) -> Result<FdinfoBoundSubject, FdinfoProjectionGap> {
        if !self.valid {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let state = self
            .aliases
            .get(&alias)
            .filter(|a| a.lifetime == observed_lifetime)
            .ok_or(FdinfoProjectionGap::UnboundAlias)?;
        let task = self
            .tasks
            .get(&state.lifetime)
            .ok_or(FdinfoProjectionGap::UnboundAlias)?;
        if !task.live {
            return Err(FdinfoProjectionGap::RetiredTask);
        }
        let owner = self
            .next_owner
            .checked_add(1)
            .ok_or(FdinfoProjectionGap::RejectedUpdate)?;
        let references = task
            .open_subjects
            .checked_add(1)
            .ok_or(FdinfoProjectionGap::RejectedUpdate)?;
        let generation = task.generation;
        let subject = FdinfoBoundSubject {
            alias,
            lifetime: observed_lifetime,
            owner,
        };
        self.next_owner = owner;
        self.tasks
            .get_mut(&observed_lifetime)
            .expect("validated task")
            .open_subjects = references;
        self.open_subjects.insert(
            owner,
            OpenSubjectState {
                subject,
                generation,
            },
        );
        Ok(subject)
    }

    /// A positive exported alias, not a numeric registry miss. Integration
    /// must independently match the opened kernel file to this alias and its
    /// raw lifetime observation before acquiring an owner below.
    pub(crate) fn registered_alias_lifetime(
        &self,
        alias: FdinfoProcAlias,
    ) -> Result<FdinfoTaskLifetime, FdinfoProjectionGap> {
        if !self.valid {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let alias = self
            .aliases
            .get(&alias)
            .ok_or(FdinfoProjectionGap::UnboundAlias)?;
        let task = self
            .tasks
            .get(&alias.lifetime)
            .ok_or(FdinfoProjectionGap::UnboundAlias)?;
        if !task.live {
            return Err(FdinfoProjectionGap::RetiredTask);
        }
        Ok(alias.lifetime)
    }

    pub(crate) fn admitted_contains_ofd(
        &self,
        admission: FdinfoAdmission,
        open_file: OpenFileId,
    ) -> Result<bool, FdinfoProjectionError> {
        let sender = self.sender(admission)?;
        let table = self
            .table(sender.files.files)
            .ok_or(FdinfoProjectionError::TableOrigin)?;
        Ok(table.open_files.contains_key(&open_file))
    }

    /// Capture known source and target slots of the already admitted actor.
    /// Integration must separately prove actual backend registration, the
    /// source description, and the ordinary target object in that same table.
    /// This never classifies or binds a physical proc subject.
    pub(crate) fn capture_admitted_slot_token(
        &self,
        admission: FdinfoAdmission,
        files: FilesId,
        source_fd: RawFd,
        source: OpenFileId,
        target_fd: RawFd,
    ) -> Result<FdinfoAdmittedSlotCapture, FdinfoProjectionGap> {
        if !self.valid {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let sender = self
            .sender(admission)
            .map_err(|_| FdinfoProjectionGap::RetiredTask)?;
        if sender.files.files != files {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let table = self.table(files).ok_or(FdinfoProjectionGap::PartialTable)?;
        let missing = if table.complete {
            FdinfoProjectionGap::MissingSlot
        } else {
            FdinfoProjectionGap::PartialTable
        };
        let source_slot = *table.slots.get(&source_fd).ok_or_else(|| missing.clone())?;
        if source_slot.open_file != source {
            return Err(FdinfoProjectionGap::MissingSlot);
        }
        let target_slot = *table.slots.get(&target_fd).ok_or(missing)?;
        let ofd = self
            .ofds
            .get(&target_slot.open_file)
            .ok_or(FdinfoProjectionGap::MissingObject)?;
        Ok(FdinfoAdmittedSlotCapture {
            admission,
            files,
            source_fd,
            source_slot,
            target_fd,
            target_slot,
            ofd: ofd.projection.clone(),
            anchor: ofd.anchor,
            revision: self.revision,
        })
    }

    /// Complete the one-use current-actor token after the same fresh rendered
    /// snapshot and ordinary object have been authenticated by integration.
    pub(crate) fn capture_admitted_slot_object(
        &mut self,
        token: FdinfoAdmittedSlotCapture,
        observed_object: FdinfoRawObject,
    ) -> Result<FdinfoCapturedCurrentTarget, FdinfoProjectionGap> {
        let current = self.capture_admitted_slot_token(
            token.admission,
            token.files,
            token.source_fd,
            token.source_slot.open_file,
            token.target_fd,
        )?;
        if current != token {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let (ofd, revision) = self.commit_slot_object(
            current.target_slot,
            current.anchor,
            &current.ofd,
            observed_object,
        )?;
        Ok(FdinfoCapturedCurrentTarget {
            incarnation: current.admission.incarnation,
            files: current.files,
            source_fd: current.source_fd,
            source: current.source_slot.open_file,
            target_fd: current.target_fd,
            slot: current.target_slot,
            ofd,
            observed_object,
            revision,
        })
    }

    pub(crate) fn release_open_subject(
        &mut self,
        subject: FdinfoBoundSubject,
    ) -> Result<(), FdinfoProjectionError> {
        self.open_subjects
            .get(&subject.owner)
            .filter(|s| s.subject == subject)
            .ok_or(FdinfoProjectionError::StaleOwner)?;
        self.open_subjects.remove(&subject.owner);
        let task = self
            .tasks
            .get_mut(&subject.lifetime)
            .expect("retained task owner");
        task.open_subjects -= 1;
        self.collect_task(subject.lifetime);
        Ok(())
    }

    pub(crate) fn classify(
        &self,
        subject: FdinfoBoundSubject,
    ) -> Result<FdinfoSubjectClass, FdinfoProjectionGap> {
        if !self.valid {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let opened = self
            .open_subjects
            .get(&subject.owner)
            .filter(|s| s.subject == subject)
            .ok_or(FdinfoProjectionGap::RetiredTask)?;
        let task = self
            .tasks
            .get(&subject.lifetime)
            .filter(|t| t.generation == opened.generation)
            .ok_or(FdinfoProjectionGap::RetiredTask)?;
        if !task.live {
            return Err(FdinfoProjectionGap::RetiredTask);
        }
        if let (Some(incarnation), Some(files)) = (task.current, task.files) {
            let live = self
                .incarnations
                .get(&incarnation.tid)
                .and_then(|id| self.senders.get(id))
                .is_some_and(|s| {
                    s.incarnation == incarnation && s.lifetime == Some(subject.lifetime)
                });
            if !live {
                return Err(FdinfoProjectionGap::RetiredTask);
            }
            return Ok(FdinfoSubjectClass::Managed { incarnation, files });
        }
        let current_alias = self
            .aliases
            .get(&subject.alias)
            .is_some_and(|a| a.lifetime == subject.lifetime);
        let coverage = self
            .views
            .get(&subject.alias.view)
            .filter(|v| v.epoch == self.view_epoch)
            .map(|v| &v.coverage);
        match coverage {
            Some(FdinfoProcViewCoverage::Complete { members })
                if current_alias && members.contains(&subject.lifetime) =>
            {
                Ok(FdinfoSubjectClass::OutsideToolInCompleteView)
            }
            Some(FdinfoProcViewCoverage::Complete { .. }) => Err(FdinfoProjectionGap::UnknownTask),
            _ => Err(FdinfoProjectionGap::PartialView),
        }
    }

    #[cfg(test)]
    pub(crate) fn capture(
        &self,
        subject: FdinfoBoundSubject,
        fd: RawFd,
        observed_object: FdinfoRawObject,
    ) -> Result<FdinfoCapturedTarget, FdinfoProjectionGap> {
        let FdinfoSubjectClass::Managed { incarnation, files } = self.classify(subject)? else {
            return Err(FdinfoProjectionGap::UnknownTask);
        };
        let table = self.table(files).ok_or(FdinfoProjectionGap::PartialTable)?;
        // A known admitted slot is positive authority regardless of unrelated
        // enumeration gaps. Completeness qualifies only an unlisted slot.
        let slot = *table.slots.get(&fd).ok_or(if table.complete {
            FdinfoProjectionGap::MissingSlot
        } else {
            FdinfoProjectionGap::PartialTable
        })?;
        let ofd = &self
            .ofds
            .get(&slot.open_file)
            .ok_or(FdinfoProjectionGap::MissingObject)?
            .projection;
        let object = ofd.object.ok_or(FdinfoProjectionGap::MissingObject)?;
        if !object.same_object(observed_object) {
            return Err(FdinfoProjectionGap::ObjectChanged);
        }
        Ok(FdinfoCapturedTarget {
            subject,
            incarnation,
            files,
            slot,
            ofd: ofd.clone(),
            observed_object,
            revision: self.revision,
        })
    }

    /// Caller must already authenticate the opened proc subject and hold this
    /// cache's serialized turn. A known slot needs no complete-table claim.
    /// Its exact OFD may still lack the first raw-object acquisition anchor.
    pub(crate) fn capture_current_slot_token(
        &self,
        subject: FdinfoBoundSubject,
        fd: RawFd,
    ) -> Result<FdinfoCurrentSlotCapture, FdinfoProjectionGap> {
        let FdinfoSubjectClass::Managed { incarnation, files } = self.classify(subject)? else {
            return Err(FdinfoProjectionGap::UnknownTask);
        };
        let table = self.table(files).ok_or(FdinfoProjectionGap::PartialTable)?;
        let slot = *table.slots.get(&fd).ok_or(if table.complete {
            FdinfoProjectionGap::MissingSlot
        } else {
            FdinfoProjectionGap::PartialTable
        })?;
        let ofd = self
            .ofds
            .get(&slot.open_file)
            .ok_or(FdinfoProjectionGap::MissingObject)?;
        Ok(FdinfoCurrentSlotCapture {
            subject,
            incarnation,
            files,
            fd,
            slot,
            ofd: ofd.projection.clone(),
            anchor: ofd.anchor,
            revision: self.revision,
        })
    }

    /// Complete only after a successful same-target raw Statx/fdinfo snapshot
    /// has been authenticated by integration. No later path lookup is issued.
    /// This is a cache fact/gap, never a replacement Linux return/errno.
    pub(crate) fn capture_current_slot_object(
        &mut self,
        token: FdinfoCurrentSlotCapture,
        observed_object: FdinfoRawObject,
    ) -> Result<FdinfoCapturedTarget, FdinfoProjectionGap> {
        let current = self.capture_current_slot_token(token.subject, token.fd)?;
        // Exact subject/incarnation/table/slot/OFD facts must still match.
        // Revision also rejects remove/reinstall or same-valued setters that
        // restore equal bytes. Any intervening ordered batch is conservative
        // unavailability; integration must keep this issuing serialized turn.
        if current != token {
            return Err(FdinfoProjectionGap::RejectedUpdate);
        }
        let (projection, revision) =
            self.commit_slot_object(current.slot, current.anchor, &current.ofd, observed_object)?;
        Ok(FdinfoCapturedTarget {
            subject: current.subject,
            incarnation: current.incarnation,
            files: current.files,
            slot: current.slot,
            ofd: projection,
            observed_object,
            revision,
        })
    }

    /// Both authority domains use the same immutable ordinary-object anchors.
    /// No source ownership, target slot, flag or resource is filled here.
    fn commit_slot_object(
        &mut self,
        slot: FdinfoSlotProjection,
        anchor: Option<FdinfoRawObject>,
        projection: &FdinfoOfdProjection,
        observed_object: FdinfoRawObject,
    ) -> Result<(FdinfoOfdProjection, u64), FdinfoProjectionGap> {
        for object in [anchor, projection.object].into_iter().flatten() {
            if !object.same_object(observed_object) {
                return Err(FdinfoProjectionGap::ObjectChanged);
            }
        }
        let fill = anchor.is_none() || projection.object.is_none();
        let revision = if fill {
            self.revision
                .checked_add(1)
                .ok_or(FdinfoProjectionGap::RejectedUpdate)?
        } else {
            self.revision
        };
        // All possible rejections precede the only touched mutation. Keep the
        // immutable anchor across any Object(None) delta; no global clone,
        // historical hold, slot rewrite or logical metadata replacement.
        let ofd = self
            .ofds
            .get_mut(&slot.open_file)
            .expect("validated current OFD");
        if ofd.anchor.is_none() {
            ofd.anchor = Some(observed_object);
        }
        if ofd.projection.object.is_none() {
            ofd.projection.object = Some(observed_object);
        }
        let projection = ofd.projection.clone();
        self.revision = revision;
        Ok((projection, revision))
    }

    pub(crate) fn contains_ofd(&self, open_file: OpenFileId) -> bool {
        self.ofds.contains_key(&open_file)
    }

    pub(crate) fn invalidate_authority(&mut self) {
        self.valid = false;
    }

    pub(crate) fn admitted_task_lifetime(
        &self,
        admission: FdinfoAdmission,
    ) -> Result<Option<FdinfoTaskLifetime>, FdinfoProjectionError> {
        Ok(self.sender(admission)?.lifetime)
    }

    /// An owned current projection after the admitted exec transaction. It
    /// uses canonical OFD facts rather than stale mutable seed fields.
    pub(crate) fn admitted_table_snapshot(
        &self,
        admission: FdinfoAdmission,
    ) -> Result<FdinfoTableSeed, FdinfoProjectionError> {
        let sender = self.sender(admission)?;
        let table = self
            .table(sender.files.files)
            .ok_or(FdinfoProjectionError::TableOrigin)?;
        let mut ofds = BTreeMap::new();
        for slot in table.slots.values() {
            let ofd = self
                .ofds
                .get(&slot.open_file)
                .ok_or(FdinfoProjectionError::UnknownOfd)?;
            ofds.entry(slot.open_file)
                .or_insert_with(|| ofd.projection.clone());
        }
        Ok(FdinfoTableSeed {
            files: sender.files.files,
            origin: sender
                .files
                .new_origin
                .unwrap_or(FdinfoTableOrigin::Initial),
            slots: table.slots.iter().map(|(&fd, &slot)| (fd, slot)).collect(),
            ofds: ofds.into_values().collect(),
            complete: table.complete,
        })
    }

    fn sender(
        &self,
        admission: FdinfoAdmission,
    ) -> Result<&IncarnationState, FdinfoProjectionError> {
        self.senders
            .get(&admission.owner)
            .filter(|s| s.incarnation == admission.incarnation)
            .ok_or(FdinfoProjectionError::StaleAdmission)
    }

    fn table(&self, files: FilesId) -> Option<&TableState> {
        self.tables.get(&files).filter(|t| t.seeded)
    }

    fn validate_registration(
        &self,
        incarnation: FdinfoIncarnation,
        files: FdinfoFilesAdmission,
        replacing: Option<u64>,
    ) -> Result<(), FdinfoProjectionError> {
        if incarnation.tid.as_raw() <= 0
            || self
                .incarnations
                .get(&incarnation.tid)
                .is_some_and(|id| Some(*id) != replacing)
        {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        match files.new_origin {
            None if replacing.is_none() && self.table(files.files).is_some() => Ok(()),
            Some(FdinfoTableOrigin::Initial)
                if replacing.is_none()
                    && files.files == FilesId::initial(incarnation.tid)
                    && !self.tables.contains_key(&files.files)
                    && !self.pending.contains_key(&files.files) =>
            {
                Ok(())
            }
            Some(origin)
                if !matches!(origin, FdinfoTableOrigin::Initial)
                    && !self.tables.contains_key(&files.files) =>
            {
                let pending = self
                    .pending
                    .get(&files.files)
                    .ok_or(FdinfoProjectionError::TableOrigin)?;
                if pending.target != incarnation || pending.seed.origin != origin {
                    return Err(FdinfoProjectionError::TableOrigin);
                }
                if let Some(owner) = replacing
                    && self
                        .senders
                        .get(&owner)
                        .is_none_or(|s| s.incarnation != pending.parent)
                {
                    return Err(FdinfoProjectionError::TableOrigin);
                }
                Ok(())
            }
            _ => Err(FdinfoProjectionError::TableOrigin),
        }
    }

    fn next_id(&mut self) -> Result<u64, FdinfoProjectionError> {
        self.next_owner = self
            .next_owner
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        Ok(self.next_owner)
    }

    fn invalidate_views(&mut self) -> Result<(), FdinfoProjectionError> {
        self.view_epoch = self
            .view_epoch
            .checked_add(1)
            .ok_or(FdinfoProjectionError::Overflow)?;
        Ok(())
    }

    fn validate_seed(
        &self,
        seed: &FdinfoTableSeed,
    ) -> Result<BTreeMap<RawFd, FdinfoSlotProjection>, FdinfoProjectionError> {
        let mut ofds = BTreeSet::new();
        for ofd in &seed.ofds {
            if !ofds.insert(ofd.open_file) {
                return Err(FdinfoProjectionError::UnknownOfd);
            }
            if ofd.status_flags & libc::O_CLOEXEC != 0 {
                return Err(FdinfoProjectionError::InvalidFlags);
            }
            if let Some(current) = self.ofds.get(&ofd.open_file)
                && let (Some(anchor), Some(incoming)) = (current.anchor, ofd.object)
                && !anchor.same_object(incoming)
            {
                return Err(FdinfoProjectionError::ObjectConflict);
            }
        }
        let mut slots = BTreeMap::new();
        for (fd, slot) in &seed.slots {
            if *fd < 0 || slots.contains_key(fd) {
                return Err(FdinfoProjectionError::InvalidSlot);
            }
            if !self.ofds.contains_key(&slot.open_file) && !ofds.contains(&slot.open_file) {
                return Err(FdinfoProjectionError::UnknownOfd);
            }
            if matches!(seed.origin, FdinfoTableOrigin::Exec { .. }) && slot.cloexec {
                return Err(FdinfoProjectionError::InvalidSlot);
            }
            slots.insert(*fd, *slot);
        }
        Ok(slots)
    }

    fn seed_ofd(
        &mut self,
        projection: FdinfoOfdProjection,
        tx: &mut Transaction,
    ) -> Result<(), FdinfoProjectionError> {
        if projection.status_flags & libc::O_CLOEXEC != 0 {
            return Err(FdinfoProjectionError::InvalidFlags);
        }
        if let Some(current) = self.ofds.get(&projection.open_file) {
            if let (Some(anchor), Some(incoming)) = (current.anchor, projection.object)
                && !anchor.same_object(incoming)
            {
                return Err(FdinfoProjectionError::ObjectConflict);
            }
            if current.projection.object.is_none() && projection.object.is_some() {
                tx.undo
                    .push(Undo::Ofd(projection.open_file, Some(current.clone())));
                let current = self
                    .ofds
                    .get_mut(&projection.open_file)
                    .expect("existing OFD");
                current.projection.object = projection.object;
                current.anchor = current.anchor.or(projection.object);
            }
        } else {
            tx.undo.push(Undo::Ofd(projection.open_file, None));
            tx.ofds_to_collect.insert(projection.open_file);
            self.ofds.insert(
                projection.open_file,
                OfdState {
                    anchor: projection.object,
                    projection,
                    references: 0,
                },
            );
        }
        Ok(())
    }

    fn change_ofd_reference(
        &mut self,
        open_file: OpenFileId,
        add: bool,
        tx: &mut Transaction,
    ) -> Result<(), FdinfoProjectionError> {
        let current = self
            .ofds
            .get(&open_file)
            .ok_or(FdinfoProjectionError::UnknownOfd)?;
        let references = if add {
            current.references.checked_add(1)
        } else {
            current.references.checked_sub(1)
        }
        .ok_or(FdinfoProjectionError::Overflow)?;
        tx.undo.push(Undo::Ofd(open_file, Some(current.clone())));
        self.ofds
            .get_mut(&open_file)
            .expect("existing OFD")
            .references = references;
        tx.ofds_to_collect.insert(open_file);
        Ok(())
    }

    fn change_table_ofd(
        &mut self,
        files: FilesId,
        open_file: OpenFileId,
        add: bool,
        tx: &mut Transaction,
    ) -> Result<(), FdinfoProjectionError> {
        let table = self
            .tables
            .get_mut(&files)
            .ok_or(FdinfoProjectionError::TableOrigin)?;
        let previous = table.open_files.get(&open_file).copied();
        let count = if add {
            previous.unwrap_or(0).checked_add(1)
        } else {
            previous.and_then(|n| n.checked_sub(1))
        }
        .ok_or(FdinfoProjectionError::Overflow)?;
        tx.undo.push(Undo::TableOfd(files, open_file, previous));
        if count == 0 {
            table.open_files.remove(&open_file);
        } else {
            table.open_files.insert(open_file, count);
        }
        self.change_ofd_reference(open_file, add, tx)
    }

    fn install_slot(
        &mut self,
        files: FilesId,
        fd: RawFd,
        slot: Option<FdinfoSlotProjection>,
        tx: &mut Transaction,
    ) -> Result<(), FdinfoProjectionError> {
        let previous = self
            .tables
            .get(&files)
            .ok_or(FdinfoProjectionError::TableOrigin)?
            .slots
            .get(&fd)
            .copied();
        tx.undo.push(Undo::Slot(files, fd, previous));
        if let Some(old) = previous {
            self.change_table_ofd(files, old.open_file, false, tx)?;
        }
        if let Some(new) = slot {
            self.change_table_ofd(files, new.open_file, true, tx)?;
        }
        let table = self.tables.get_mut(&files).expect("existing table");
        if let Some(new) = slot {
            table.slots.insert(fd, new);
        } else {
            table.slots.remove(&fd);
        }
        Ok(())
    }

    fn apply_one(
        &mut self,
        admission: FdinfoAdmission,
        delta: FdinfoProjectionDelta,
        tx: &mut Transaction,
    ) -> Result<(), FdinfoProjectionError> {
        let actor = self.sender(admission)?;
        let incarnation = actor.incarnation;
        let files = actor.files;
        let predecessor = actor.predecessor;
        match delta {
            FdinfoProjectionDelta::Table(seed) => {
                let table = self
                    .tables
                    .get(&seed.files)
                    .ok_or(FdinfoProjectionError::TableOrigin)?;
                if table.seeded {
                    return Err(FdinfoProjectionError::ExistingTable);
                }
                if files.files != seed.files || files.new_origin != Some(seed.origin) {
                    return Err(FdinfoProjectionError::TableOrigin);
                }
                let slots = self.validate_seed(&seed)?;
                match seed.origin {
                    FdinfoTableOrigin::Initial
                        if seed.files == FilesId::initial(incarnation.tid) => {}
                    FdinfoTableOrigin::Fork { .. } | FdinfoTableOrigin::Exec { .. } => {
                        let pending = self
                            .pending
                            .get(&seed.files)
                            .ok_or(FdinfoProjectionError::TableOrigin)?;
                        if pending.target != incarnation || pending.seed != seed {
                            return Err(FdinfoProjectionError::TableOrigin);
                        }
                        tx.adopted_seeds.push(seed.files);
                    }
                    _ => return Err(FdinfoProjectionError::TableOrigin),
                }
                for projection in seed.ofds {
                    self.seed_ofd(projection, tx)?;
                }
                let table = self.tables.get_mut(&seed.files).expect("reserved table");
                tx.undo.push(Undo::TableMetadata(
                    seed.files,
                    table.seeded,
                    table.complete,
                ));
                table.seeded = true;
                table.complete = seed.complete;
                for (fd, slot) in slots {
                    self.install_slot(seed.files, fd, Some(slot), tx)?;
                }
            }
            FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Seed(projection)) => {
                self.seed_ofd(projection, tx)?
            }
            FdinfoProjectionDelta::Ofd(delta) => {
                let open_file = match &delta {
                    FdinfoOfdDelta::StatusFlags { open_file, .. }
                    | FdinfoOfdDelta::Resource { open_file, .. }
                    | FdinfoOfdDelta::Type { open_file, .. }
                    | FdinfoOfdDelta::Object { open_file, .. } => *open_file,
                    FdinfoOfdDelta::Seed(_) => unreachable!(),
                };
                if !self
                    .table(files.files)
                    .is_some_and(|t| t.open_files.contains_key(&open_file))
                {
                    return Err(FdinfoProjectionError::ForeignMutation);
                }
                let ofd = self
                    .ofds
                    .get_mut(&open_file)
                    .ok_or(FdinfoProjectionError::UnknownOfd)?;
                tx.undo.push(Undo::Ofd(open_file, Some(ofd.clone())));
                match delta {
                    FdinfoOfdDelta::StatusFlags { mask, value, .. } => {
                        let mutable = libc::O_APPEND
                            | libc::O_ASYNC
                            | libc::O_DIRECT
                            | libc::O_NOATIME
                            | libc::O_NONBLOCK;
                        if mask == 0 || mask & !mutable != 0 {
                            return Err(FdinfoProjectionError::InvalidFlags);
                        }
                        ofd.projection.status_flags =
                            (ofd.projection.status_flags & !mask) | (value & mask);
                    }
                    FdinfoOfdDelta::Resource { resource, .. } => ofd.projection.resource = resource,
                    FdinfoOfdDelta::Type { ty, .. } => ofd.projection.ty = ty,
                    FdinfoOfdDelta::Object { object, .. } => {
                        if let (Some(anchor), Some(incoming)) = (ofd.anchor, object)
                            && !anchor.same_object(incoming)
                        {
                            return Err(FdinfoProjectionError::ObjectConflict);
                        }
                        ofd.anchor = ofd.anchor.or(object);
                        ofd.projection.object = object;
                    }
                    FdinfoOfdDelta::Seed(_) => unreachable!(),
                }
            }
            FdinfoProjectionDelta::Slot(delta) => {
                let (target_files, fd) = match &delta {
                    FdinfoSlotDelta::Install { files, fd, .. }
                    | FdinfoSlotDelta::Remove { files, fd, .. }
                    | FdinfoSlotDelta::Cloexec { files, fd, .. } => (*files, *fd),
                };
                if target_files != files.files {
                    return Err(FdinfoProjectionError::ForeignMutation);
                }
                if fd < 0 {
                    return Err(FdinfoProjectionError::InvalidSlot);
                }
                self.table(target_files)
                    .ok_or(FdinfoProjectionError::TableOrigin)?;
                match delta {
                    FdinfoSlotDelta::Install { slot, .. } => {
                        if !self.ofds.contains_key(&slot.open_file) {
                            return Err(FdinfoProjectionError::UnknownOfd);
                        }
                        self.install_slot(target_files, fd, Some(slot), tx)?;
                    }
                    FdinfoSlotDelta::Remove { expected, .. } => {
                        if !self.table(target_files).is_some_and(|t| {
                            t.slots.get(&fd).is_some_and(|s| s.open_file == expected)
                        }) {
                            return Err(FdinfoProjectionError::SlotChanged);
                        }
                        self.install_slot(target_files, fd, None, tx)?;
                    }
                    FdinfoSlotDelta::Cloexec {
                        expected, cloexec, ..
                    } => {
                        let table = self.tables.get_mut(&target_files).expect("existing table");
                        let old = *table
                            .slots
                            .get(&fd)
                            .ok_or(FdinfoProjectionError::SlotChanged)?;
                        if old.open_file != expected {
                            return Err(FdinfoProjectionError::SlotChanged);
                        }
                        tx.undo.push(Undo::Slot(target_files, fd, Some(old)));
                        table.slots.get_mut(&fd).expect("existing slot").cloexec = cloexec;
                    }
                }
            }
            FdinfoProjectionDelta::BindTask { lifetime, previous } => {
                self.table(files.files)
                    .ok_or(FdinfoProjectionError::TableOrigin)?;
                let old_lifetime = self
                    .senders
                    .get(&admission.owner)
                    .expect("validated sender")
                    .lifetime;
                if old_lifetime.is_some_and(|l| l != lifetime) {
                    return Err(FdinfoProjectionError::TaskChanged);
                }
                let old = self.tasks.get(&lifetime);
                if let Some(task) = old {
                    if !task.live || previous != task.current {
                        return Err(FdinfoProjectionError::TaskChanged);
                    }
                    if let Some(current) = task.current
                        && current != incarnation
                        && predecessor != Some(current)
                    {
                        return Err(FdinfoProjectionError::TaskChanged);
                    }
                } else if previous.is_some() {
                    return Err(FdinfoProjectionError::TaskChanged);
                }
                tx.undo.push(Undo::Task(lifetime, old.cloned()));
                if old.is_none() {
                    let generation = self.next_id()?;
                    self.tasks.insert(
                        lifetime,
                        TaskState {
                            current: None,
                            files: None,
                            generation,
                            live: true,
                            open_subjects: 0,
                            aliases: HashSet::new(),
                        },
                    );
                }
                let task = self.tasks.get_mut(&lifetime).expect("bound task");
                task.current = Some(incarnation);
                task.files = Some(files.files);
                tx.undo
                    .push(Undo::SenderLifetime(admission.owner, old_lifetime));
                self.senders
                    .get_mut(&admission.owner)
                    .expect("validated sender")
                    .lifetime = Some(lifetime);
                self.invalidate_views()?;
            }
            FdinfoProjectionDelta::BindAlias { alias, lifetime } => {
                let valid = match alias.kind {
                    FdinfoProcAliasKind::Process { pid } => pid > 0,
                    FdinfoProcAliasKind::Task { pid, tid } => pid > 0 && tid > 0,
                };
                if !valid {
                    return Err(FdinfoProjectionError::AliasChanged);
                }
                let old = self.aliases.get(&alias);
                if old.is_some_and(|a| a.lifetime != lifetime) {
                    return Err(FdinfoProjectionError::AliasChanged);
                }
                if self.tasks.get(&lifetime).is_some_and(|t| !t.live) {
                    return Err(FdinfoProjectionError::AliasChanged);
                }
                tx.undo.push(Undo::Alias(alias, old.cloned()));
                let old_task = self.tasks.get(&lifetime);
                tx.undo.push(Undo::Task(lifetime, old_task.cloned()));
                if old_task.is_none() {
                    let generation = self.next_id()?;
                    self.tasks.insert(
                        lifetime,
                        TaskState {
                            current: None,
                            files: None,
                            generation,
                            live: true,
                            open_subjects: 0,
                            aliases: HashSet::new(),
                        },
                    );
                }
                self.tasks
                    .get_mut(&lifetime)
                    .expect("bound lifetime")
                    .aliases
                    .insert(alias);
                self.aliases
                    .entry(alias)
                    .or_insert_with(|| AliasState {
                        lifetime,
                        owners: BTreeSet::new(),
                    })
                    .owners
                    .insert(admission.owner);
                let sender = self
                    .senders
                    .get_mut(&admission.owner)
                    .expect("validated sender");
                tx.undo.push(Undo::SenderAlias(
                    admission.owner,
                    alias,
                    sender.aliases.contains(&alias),
                ));
                sender.aliases.insert(alias);
                self.invalidate_views()?;
            }
            FdinfoProjectionDelta::InvalidateViews => self.invalidate_views()?,
            FdinfoProjectionDelta::View { view, coverage } => {
                if let FdinfoProcViewCoverage::Complete { members } = &coverage {
                    if self.pending.keys().any(|f| !tx.adopted_seeds.contains(f)) {
                        return Err(FdinfoProjectionError::IncompleteView);
                    }
                    let mut seen = HashSet::new();
                    for member in members {
                        if !seen.insert(*member)
                            || !self
                                .tasks
                                .get(member)
                                .is_some_and(|t| t.live && t.aliases.iter().any(|a| a.view == view))
                        {
                            return Err(FdinfoProjectionError::IncompleteView);
                        }
                    }
                    for sender in self.senders.values() {
                        let task = sender
                            .lifetime
                            .and_then(|l| self.tasks.get(&l))
                            .ok_or(FdinfoProjectionError::IncompleteView)?;
                        if !task.live
                            || task.current != Some(sender.incarnation)
                            || !seen.contains(&sender.lifetime.expect("bound lifetime"))
                            || !task.aliases.iter().any(|a| a.view == view)
                        {
                            return Err(FdinfoProjectionError::IncompleteView);
                        }
                    }
                    for (alias, state) in &self.aliases {
                        if alias.view == view && !seen.contains(&state.lifetime) {
                            return Err(FdinfoProjectionError::IncompleteView);
                        }
                    }
                }
                let old = self.views.get(&view);
                tx.undo.push(Undo::View(view, old.cloned()));
                if let Some(old) = old
                    && let Some(sender) = self.senders.get_mut(&old.owner)
                {
                    tx.undo.push(Undo::SenderView(
                        old.owner,
                        view,
                        sender.views.contains(&view),
                    ));
                    sender.views.remove(&view);
                }
                self.views.insert(
                    view,
                    ViewState {
                        coverage,
                        owner: admission.owner,
                        epoch: self.view_epoch,
                    },
                );
                let sender = self
                    .senders
                    .get_mut(&admission.owner)
                    .expect("validated sender");
                tx.undo.push(Undo::SenderView(
                    admission.owner,
                    view,
                    sender.views.contains(&view),
                ));
                sender.views.insert(view);
            }
        }
        Ok(())
    }

    fn rollback(&mut self, undo: Vec<Undo>) {
        for item in undo.into_iter().rev() {
            match item {
                Undo::Ofd(id, old) => {
                    if let Some(state) = old {
                        self.ofds.insert(id, state);
                    } else {
                        self.ofds.remove(&id);
                    }
                }
                Undo::Slot(files, fd, old) => {
                    let table = self
                        .tables
                        .get_mut(&files)
                        .expect("touched table retained until commit");
                    if let Some(slot) = old {
                        table.slots.insert(fd, slot);
                    } else {
                        table.slots.remove(&fd);
                    }
                }
                Undo::TableOfd(files, ofd, old) => {
                    let table = self
                        .tables
                        .get_mut(&files)
                        .expect("touched table retained until commit");
                    if let Some(count) = old {
                        table.open_files.insert(ofd, count);
                    } else {
                        table.open_files.remove(&ofd);
                    }
                }
                Undo::TableMetadata(files, seeded, complete) => {
                    let table = self.tables.get_mut(&files).expect("reserved table");
                    table.seeded = seeded;
                    table.complete = complete;
                }
                Undo::TableOwners(files, owners) => {
                    self.tables.get_mut(&files).expect("retained table").owners = owners
                }
                Undo::Task(lifetime, old) => {
                    if let Some(state) = old {
                        self.tasks.insert(lifetime, state);
                    } else {
                        self.tasks.remove(&lifetime);
                    }
                }
                Undo::SenderLifetime(owner, old) => {
                    self.senders
                        .get_mut(&owner)
                        .expect("sender retained until commit")
                        .lifetime = old
                }
                Undo::Alias(alias, old) => {
                    if let Some(state) = old {
                        self.aliases.insert(alias, state);
                    } else {
                        self.aliases.remove(&alias);
                    }
                }
                Undo::SenderAlias(owner, alias, old) => {
                    let sender = self
                        .senders
                        .get_mut(&owner)
                        .expect("sender retained until commit");
                    if old {
                        sender.aliases.insert(alias);
                    } else {
                        sender.aliases.remove(&alias);
                    }
                }
                Undo::View(view, old) => {
                    if let Some(state) = old {
                        self.views.insert(view, state);
                    } else {
                        self.views.remove(&view);
                    }
                }
                Undo::SenderView(owner, view, old) => {
                    let sender = self
                        .senders
                        .get_mut(&owner)
                        .expect("sender retained until commit");
                    if old {
                        sender.views.insert(view);
                    } else {
                        sender.views.remove(&view);
                    }
                }
            }
        }
    }

    fn collect_ofds(&mut self, candidates: BTreeSet<OpenFileId>) {
        for open_file in candidates {
            if self.ofds.get(&open_file).is_some_and(|o| o.references == 0) {
                self.ofds.remove(&open_file);
            }
        }
    }

    fn release_table_owner(&mut self, files: FilesId) {
        let table = self.tables.get_mut(&files).expect("retained table owner");
        table.owners -= 1;
        if table.owners != 0 {
            return;
        }
        let table = self.tables.remove(&files).expect("unreferenced table");
        for slot in table.slots.into_values() {
            let ofd = self.ofds.get_mut(&slot.open_file).expect("live slot OFD");
            ofd.references -= 1;
            if ofd.references == 0 {
                self.ofds.remove(&slot.open_file);
            }
        }
    }

    fn release_pending(&mut self, files: FilesId) {
        let pending = self.pending.remove(&files).expect("retained pending seed");
        for open_file in pending.held_ofds {
            let ofd = self.ofds.get_mut(&open_file).expect("pending snapshot OFD");
            ofd.references -= 1;
            if ofd.references == 0 {
                self.ofds.remove(&open_file);
            }
        }
        self.release_table_owner(pending.source);
    }

    fn release_alias_owner(&mut self, owner: u64, alias: FdinfoProcAlias) {
        if let Some(state) = self.aliases.get_mut(&alias) {
            state.owners.remove(&owner);
            if state.owners.is_empty() {
                self.remove_alias(alias);
            }
        }
    }

    fn remove_alias(&mut self, alias: FdinfoProcAlias) {
        if let Some(state) = self.aliases.remove(&alias) {
            for owner in state.owners {
                if let Some(sender) = self.senders.get_mut(&owner) {
                    sender.aliases.remove(&alias);
                }
            }
            if let Some(task) = self.tasks.get_mut(&state.lifetime) {
                task.aliases.remove(&alias);
            }
            self.collect_task(state.lifetime);
        }
    }

    fn collect_task(&mut self, lifetime: FdinfoTaskLifetime) {
        if self.tasks.get(&lifetime).is_some_and(|t| {
            t.open_subjects == 0 && t.aliases.is_empty() && (!t.live || t.current.is_none())
        }) {
            self.tasks.remove(&lifetime);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        cache: FdinfoProjectionCache,
        admitted: FdinfoAdmission,
        incarnation: FdinfoIncarnation,
        files: FilesId,
        ofd: OpenFileId,
        object: FdinfoRawObject,
        lifetime: FdinfoTaskLifetime,
        alias: FdinfoProcAlias,
    }

    fn batch(sequence: u64, deltas: Vec<FdinfoProjectionDelta>) -> FdinfoProjectionBatch {
        FdinfoProjectionBatch { sequence, deltas }
    }

    fn fixture() -> Fixture {
        let tid = DetTid::from_raw(3);
        let incarnation = FdinfoIncarnation {
            tid,
            mm: MmId::initial(tid),
        };
        let files = FilesId::initial(tid);
        let ofd = OpenFileId::new(tid, 1);
        let object = FdinfoRawObject {
            inode: RawInode::new(33, 256),
            mode: libc::S_IFDIR | 0o700,
            mount_id: 7,
        };
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 101),
            start_ticks: 23,
        };
        let view = FdinfoProcView {
            procfs_root: RawInode::new(9, 1),
            pid_namespace: RawInode::new(4, 10),
            mount_namespace: RawInode::new(4, 11),
        };
        let alias = FdinfoProcAlias {
            view,
            kind: FdinfoProcAliasKind::Process { pid: 5 },
            directory: lifetime.birth_anchor,
        };
        let mut cache = FdinfoProjectionCache::default();
        let admitted = cache
            .register_after_admission(
                incarnation,
                FdinfoFilesAdmission {
                    files,
                    new_origin: Some(FdinfoTableOrigin::Initial),
                },
            )
            .unwrap();
        cache
            .apply(
                admitted,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(FdinfoTableSeed {
                            files,
                            origin: FdinfoTableOrigin::Initial,
                            slots: vec![(
                                7,
                                FdinfoSlotProjection {
                                    open_file: ofd,
                                    cloexec: false,
                                },
                            )],
                            ofds: vec![FdinfoOfdProjection {
                                open_file: ofd,
                                status_flags: libc::O_RDONLY,
                                resource: Some(ResourceID::Device(
                                    crate::resources::Device::ContainerStdout,
                                )),
                                ty: FdType::Regular,
                                object: Some(object),
                            }],
                            complete: true,
                        }),
                        FdinfoProjectionDelta::BindTask {
                            lifetime,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias { alias, lifetime },
                        FdinfoProjectionDelta::View {
                            view,
                            coverage: FdinfoProcViewCoverage::Complete {
                                members: vec![lifetime],
                            },
                        },
                    ],
                ),
            )
            .unwrap();
        Fixture {
            cache,
            admitted,
            incarnation,
            files,
            ofd,
            object,
            lifetime,
            alias,
        }
    }

    fn capture(f: &mut Fixture) -> FdinfoCapturedTarget {
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let result = f.cache.capture(subject, 7, f.object).unwrap();
        f.cache.release_open_subject(subject).unwrap();
        result
    }

    struct CurrentFixture {
        cache: FdinfoProjectionCache,
        admission: FdinfoAdmission,
        files: FilesId,
        source: OpenFileId,
        target: OpenFileId,
        object: FdinfoRawObject,
    }

    fn current_fixture() -> CurrentFixture {
        let tid = DetTid::from_raw(3);
        let files = FilesId::initial(tid);
        let source = OpenFileId::new(tid, 1);
        let target = OpenFileId::new(tid, 2);
        let object = FdinfoRawObject {
            inode: RawInode::new(33, 256),
            mode: libc::S_IFREG | 0o600,
            mount_id: 7,
        };
        let mut cache = FdinfoProjectionCache::default();
        let admission = cache
            .register_after_admission(
                FdinfoIncarnation {
                    tid,
                    mm: MmId::initial(tid),
                },
                FdinfoFilesAdmission {
                    files,
                    new_origin: Some(FdinfoTableOrigin::Initial),
                },
            )
            .unwrap();
        cache
            .apply(
                admission,
                batch(
                    1,
                    vec![FdinfoProjectionDelta::Table(FdinfoTableSeed {
                        files,
                        origin: FdinfoTableOrigin::Initial,
                        complete: false,
                        slots: vec![
                            (
                                7,
                                FdinfoSlotProjection {
                                    open_file: source,
                                    cloexec: false,
                                },
                            ),
                            (
                                0,
                                FdinfoSlotProjection {
                                    open_file: target,
                                    cloexec: true,
                                },
                            ),
                        ],
                        ofds: vec![
                            FdinfoOfdProjection {
                                open_file: source,
                                status_flags: libc::O_RDONLY,
                                resource: Some(ResourceID::Device(
                                    crate::resources::Device::ContainerStdout,
                                )),
                                ty: FdType::Regular,
                                object: None,
                            },
                            FdinfoOfdProjection {
                                open_file: target,
                                status_flags: libc::O_RDWR | libc::O_NONBLOCK,
                                resource: None,
                                ty: FdType::Regular,
                                object: None,
                            },
                        ],
                    })],
                ),
            )
            .unwrap();
        CurrentFixture {
            cache,
            admission,
            files,
            source,
            target,
            object,
        }
    }

    #[test]
    fn current_actor_known_slot_keeps_physical_alias_and_target_resource_domains_separate() {
        let CurrentFixture {
            mut cache,
            admission,
            files,
            source,
            target,
            object,
        } = current_fixture();
        assert!(cache.tasks.is_empty() && cache.aliases.is_empty());
        assert_eq!(cache.senders[&admission.owner].lifetime, None);
        assert_eq!(
            cache.capture_admitted_slot_token(admission, files, 7, source, 8),
            Err(FdinfoProjectionGap::PartialTable)
        );
        assert_eq!(
            cache.capture_admitted_slot_token(admission, files, 0, source, 0),
            Err(FdinfoProjectionGap::MissingSlot)
        );
        let token = cache
            .capture_admitted_slot_token(admission, files, 7, source, 0)
            .unwrap();
        let captured = cache.capture_admitted_slot_object(token, object).unwrap();
        assert_eq!(captured.source, source);
        assert_eq!(captured.slot.open_file, target);
        assert_eq!(captured.ofd.resource, None);
        assert_eq!(
            captured.fdinfo_flags(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC
        );
        assert_eq!(captured.observed_object, object);
        // Current ordinary-object evidence neither invents a physical task
        // lifetime nor borrows the source's stdout ResourceID for target 0.
        assert!(cache.tasks.is_empty() && cache.aliases.is_empty());
    }

    #[test]
    fn current_actor_capture_rejects_source_and_target_revision_changes_before_object_commit() {
        for fd in [7, 0] {
            let CurrentFixture {
                mut cache,
                admission,
                files,
                source,
                target,
                object,
            } = current_fixture();
            let token = cache
                .capture_admitted_slot_token(admission, files, 7, source, 0)
                .unwrap();
            let slot = token.source_slot;
            let (expected, cloexec) = if fd == 7 {
                (source, slot.cloexec)
            } else {
                (target, token.target_slot.cloexec)
            };
            // An equal-valued setter is still an ordered mutation and must
            // not make a held token look continuously unchanged.
            cache
                .apply(
                    admission,
                    batch(
                        2,
                        vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Cloexec {
                            files,
                            fd,
                            expected,
                            cloexec,
                        })],
                    ),
                )
                .unwrap();
            assert_eq!(
                cache.capture_admitted_slot_object(token, object),
                Err(FdinfoProjectionGap::RejectedUpdate)
            );
            assert_eq!(cache.ofds[&target].projection.object, None);
            assert_eq!(cache.ofds[&target].anchor, None);
        }
        let CurrentFixture {
            mut cache,
            admission,
            files,
            source,
            target,
            object,
        } = current_fixture();
        let token = cache
            .capture_admitted_slot_token(admission, files, 7, source, 0)
            .unwrap();
        cache
            .apply(
                admission,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                        files,
                        fd: 7,
                        expected: source,
                    })],
                ),
            )
            .unwrap();
        assert!(cache.capture_admitted_slot_object(token, object).is_err());
        assert_eq!(cache.ofds[&target].projection.object, None);
    }

    #[test]
    fn fork_seed_preserves_canonical_ofd_and_same_valued_setter_is_an_event() {
        let mut f = fixture();
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Cloexec {
                        files: f.files,
                        fd: 7,
                        expected: f.ofd,
                        cloexec: true,
                    })],
                ),
            )
            .unwrap();
        let stale = capture(&mut f).ofd;
        let child = DetTid::from_raw(4);
        let child_incarnation = FdinfoIncarnation {
            tid: child,
            mm: MmId::initial(child),
        };
        let child_files = FilesId::forked(child);
        let seed = FdinfoTableSeed {
            files: child_files,
            origin: FdinfoTableOrigin::Fork { source: f.files },
            slots: vec![(
                7,
                FdinfoSlotProjection {
                    open_file: f.ofd,
                    cloexec: true,
                },
            )],
            ofds: vec![stale],
            complete: true,
        };
        f.cache
            .reserve_pending_seed(f.admitted, child_incarnation, seed.clone())
            .unwrap();
        // The held child snapshot is older than these canonical shared-OFD and
        // parent-slot changes, yet remains an authentic clone-time snapshot.
        f.cache
            .apply(
                f.admitted,
                batch(
                    3,
                    vec![
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::StatusFlags {
                            open_file: f.ofd,
                            mask: libc::O_NONBLOCK,
                            value: libc::O_NONBLOCK,
                        }),
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Resource {
                            open_file: f.ofd,
                            resource: Some(ResourceID::Device(
                                crate::resources::Device::ContainerStderr,
                            )),
                        }),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Cloexec {
                            files: f.files,
                            fd: 7,
                            expected: f.ofd,
                            cloexec: false,
                        }),
                    ],
                ),
            )
            .unwrap();
        let child_admitted = f
            .cache
            .register_after_admission(
                child_incarnation,
                FdinfoFilesAdmission {
                    files: child_files,
                    new_origin: Some(seed.origin),
                },
            )
            .unwrap();
        f.cache
            .apply(
                child_admitted,
                batch(1, vec![FdinfoProjectionDelta::Table(seed)]),
            )
            .unwrap();
        assert_eq!(
            capture(&mut f).ofd.status_flags & libc::O_NONBLOCK,
            libc::O_NONBLOCK
        );
        assert_eq!(
            capture(&mut f).ofd.resource,
            Some(ResourceID::Device(
                crate::resources::Device::ContainerStderr
            ))
        );
        // The child locally believed NONBLOCK was clear. Its successful clear
        // remains an ordered field event rather than a stale whole-OFD copy.
        f.cache
            .apply(
                child_admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::StatusFlags {
                        open_file: f.ofd,
                        mask: libc::O_NONBLOCK,
                        value: 0,
                    })],
                ),
            )
            .unwrap();
        assert_eq!(capture(&mut f).ofd.status_flags & libc::O_NONBLOCK, 0);
        assert!(f.cache.table(child_files).unwrap().slots[&7].cloexec);
        assert!(!capture(&mut f).slot.cloexec);
    }

    #[test]
    fn shared_table_seed_and_late_error_cannot_replace_or_partially_mutate_state() {
        let mut f = fixture();
        let old = capture(&mut f);
        let invalid = batch(
            2,
            vec![
                FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Resource {
                    open_file: f.ofd,
                    resource: None,
                }),
                FdinfoProjectionDelta::Table(FdinfoTableSeed {
                    files: f.files,
                    origin: FdinfoTableOrigin::Initial,
                    slots: vec![],
                    ofds: vec![],
                    complete: true,
                }),
            ],
        );
        assert_eq!(
            f.cache.apply(f.admitted, invalid),
            Err(FdinfoProjectionError::ExistingTable)
        );
        assert_eq!(f.cache.ofds[&f.ofd].projection, old.ofd);
        assert_eq!(
            f.cache.bind_open_subject(f.alias, f.lifetime),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
        f.cache.apply(f.admitted, batch(2, vec![])).unwrap();
        assert_eq!(
            f.cache.apply(f.admitted, batch(2, vec![])),
            Err(FdinfoProjectionError::Sequence)
        );
    }

    #[test]
    fn process_and_task_aliases_require_an_explicit_join() {
        let mut f = fixture();
        let task_alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Task { pid: 5, tid: 5 },
            directory: RawInode::new(9, 202),
            ..f.alias
        };
        assert_eq!(
            f.cache.bind_open_subject(task_alias, f.lifetime),
            Err(FdinfoProjectionGap::UnboundAlias)
        );
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::BindAlias {
                        alias: task_alias,
                        lifetime: f.lifetime,
                    }],
                ),
            )
            .unwrap();
        let task_subject = f.cache.bind_open_subject(task_alias, f.lifetime).unwrap();
        let via_task = f.cache.capture(task_subject, 7, f.object).unwrap();
        assert_eq!(via_task.files, capture(&mut f).files);
        assert_eq!(via_task.ofd, capture(&mut f).ofd);
    }

    #[test]
    fn bound_subject_follows_exec_but_rejects_old_sender_and_preserves_saved_capture() {
        let mut f = fixture();
        let bound = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let saved = f.cache.capture(bound, 7, f.object).unwrap();
        let next = FdinfoIncarnation {
            tid: f.incarnation.tid,
            mm: f.incarnation.mm.for_exec(f.incarnation.tid),
        };
        let next_files = f.files.for_exec(next.tid);
        let exec_seed = FdinfoTableSeed {
            files: next_files,
            origin: FdinfoTableOrigin::Exec { source: f.files },
            slots: vec![(
                7,
                FdinfoSlotProjection {
                    open_file: f.ofd,
                    cloexec: false,
                },
            )],
            ofds: vec![],
            complete: true,
        };
        f.cache
            .reserve_pending_seed(f.admitted, next, exec_seed.clone())
            .unwrap();
        let next_admitted = f
            .cache
            .replace_after_exec(
                f.admitted,
                next,
                FdinfoFilesAdmission {
                    files: next_files,
                    new_origin: Some(FdinfoTableOrigin::Exec { source: f.files }),
                },
            )
            .unwrap();
        assert_eq!(
            f.cache.apply(f.admitted, batch(2, vec![])),
            Err(FdinfoProjectionError::StaleAdmission)
        );
        f.cache
            .apply(
                next_admitted,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(exec_seed),
                        FdinfoProjectionDelta::BindTask {
                            lifetime: f.lifetime,
                            previous: Some(f.incarnation),
                        },
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::StatusFlags {
                            open_file: f.ofd,
                            mask: libc::O_NONBLOCK,
                            value: libc::O_NONBLOCK,
                        }),
                    ],
                ),
            )
            .unwrap();
        assert!(f.cache.table(f.files).is_none());
        assert_eq!(f.cache.senders.len(), 1);
        assert_eq!(
            f.cache.capture(bound, 7, f.object).unwrap().files,
            next_files
        );
        assert_eq!(saved.ofd.status_flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn pid_reuse_does_not_redirect_an_open_proc_subject() {
        let mut f = fixture();
        let old = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        f.cache.retire_after_admission(f.admitted).unwrap();
        let tid = DetTid::from_raw(8);
        let inc = FdinfoIncarnation {
            tid,
            mm: MmId::initial(tid),
        };
        let new_files = FilesId::initial(tid);
        let new_life = FdinfoTaskLifetime {
            start_ticks: f.lifetime.start_ticks + 1,
            ..f.lifetime
        };
        let admission = f
            .cache
            .register_after_admission(
                inc,
                FdinfoFilesAdmission {
                    files: new_files,
                    new_origin: Some(FdinfoTableOrigin::Initial),
                },
            )
            .unwrap();
        f.cache
            .apply(
                admission,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(FdinfoTableSeed {
                            files: new_files,
                            origin: FdinfoTableOrigin::Initial,
                            slots: vec![],
                            ofds: vec![],
                            complete: true,
                        }),
                        FdinfoProjectionDelta::BindTask {
                            lifetime: new_life,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias {
                            alias: f.alias,
                            lifetime: new_life,
                        },
                    ],
                ),
            )
            .unwrap();
        assert_eq!(f.cache.classify(old), Err(FdinfoProjectionGap::RetiredTask));
        assert_ne!(f.cache.bind_open_subject(f.alias, new_life).unwrap(), old);
    }

    #[test]
    fn partial_or_unexported_task_cannot_authorize_an_outside_tool_miss() {
        let mut f = fixture();
        let outside = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 303),
            start_ticks: 5,
        };
        let alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Process { pid: 1 },
            directory: outside.birth_anchor,
            ..f.alias
        };
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::BindAlias {
                        alias,
                        lifetime: outside,
                    }],
                ),
            )
            .unwrap();
        let subject = f.cache.bind_open_subject(alias, outside).unwrap();
        assert_eq!(
            f.cache.classify(subject),
            Err(FdinfoProjectionGap::PartialView)
        );
        f.cache
            .apply(
                f.admitted,
                batch(
                    3,
                    vec![FdinfoProjectionDelta::View {
                        view: alias.view,
                        coverage: FdinfoProcViewCoverage::Complete {
                            members: vec![f.lifetime, outside],
                        },
                    }],
                ),
            )
            .unwrap();
        assert_eq!(
            f.cache.classify(subject),
            Ok(FdinfoSubjectClass::OutsideToolInCompleteView)
        );
        // This classification supplies no FD metadata or normalization policy.
        assert_eq!(
            f.cache.capture(subject, 7, f.object),
            Err(FdinfoProjectionGap::UnknownTask)
        );
        let child = DetTid::from_raw(9);
        f.cache
            .register_after_admission(
                FdinfoIncarnation {
                    tid: child,
                    mm: MmId::initial(child),
                },
                FdinfoFilesAdmission {
                    files: f.files,
                    new_origin: None,
                },
            )
            .unwrap();
        assert_eq!(
            f.cache.classify(subject),
            Err(FdinfoProjectionGap::PartialView)
        );
        assert_eq!(
            f.cache.apply(
                f.admitted,
                batch(
                    4,
                    vec![FdinfoProjectionDelta::View {
                        view: alias.view,
                        coverage: FdinfoProcViewCoverage::Complete {
                            members: vec![f.lifetime, outside]
                        }
                    }]
                )
            ),
            Err(FdinfoProjectionError::IncompleteView)
        );
    }

    #[test]
    fn slot_reuse_selects_current_ofd_without_reader_number_or_old_object_fallback() {
        let mut f = fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let new_ofd = OpenFileId::new(f.incarnation.tid, 2);
        let new_object = FdinfoRawObject {
            inode: RawInode::new(33, 999),
            ..f.object
        };
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                            files: f.files,
                            fd: 7,
                            expected: f.ofd,
                        }),
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Seed(FdinfoOfdProjection {
                            open_file: new_ofd,
                            status_flags: libc::O_RDONLY,
                            resource: None,
                            ty: FdType::Regular,
                            object: Some(new_object),
                        })),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Install {
                            files: f.files,
                            fd: 7,
                            slot: FdinfoSlotProjection {
                                open_file: new_ofd,
                                cloexec: false,
                            },
                        }),
                    ],
                ),
            )
            .unwrap();
        assert_eq!(
            f.cache.capture(subject, 7, f.object),
            Err(FdinfoProjectionGap::ObjectChanged)
        );
        assert_eq!(
            f.cache.capture(subject, 3, new_object),
            Err(FdinfoProjectionGap::MissingSlot)
        );
        assert_eq!(
            f.cache
                .capture(subject, 7, new_object)
                .unwrap()
                .ofd
                .open_file,
            new_ofd
        );
    }

    #[test]
    fn slot_cloexec_and_masked_status_changes_do_not_overwrite_each_other() {
        let mut f = fixture();
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Install {
                            files: f.files,
                            fd: 8,
                            slot: FdinfoSlotProjection {
                                open_file: f.ofd,
                                cloexec: false,
                            },
                        }),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Cloexec {
                            files: f.files,
                            fd: 7,
                            expected: f.ofd,
                            cloexec: true,
                        }),
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::StatusFlags {
                            open_file: f.ofd,
                            mask: libc::O_APPEND,
                            value: libc::O_APPEND,
                        }),
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::StatusFlags {
                            open_file: f.ofd,
                            mask: libc::O_NONBLOCK,
                            value: libc::O_NONBLOCK,
                        }),
                    ],
                ),
            )
            .unwrap();
        assert_eq!(
            capture(&mut f).fdinfo_flags(),
            libc::O_APPEND | libc::O_NONBLOCK | libc::O_CLOEXEC
        );
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let other = f.cache.capture(subject, 8, f.object).unwrap();
        assert_eq!(other.fdinfo_flags(), libc::O_APPEND | libc::O_NONBLOCK);
    }

    #[test]
    fn cleared_object_authority_does_not_allow_a_new_physical_identity_under_same_ofd() {
        let mut f = fixture();
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Object {
                        open_file: f.ofd,
                        object: None,
                    })],
                ),
            )
            .unwrap();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        assert_eq!(
            f.cache.capture(subject, 7, f.object),
            Err(FdinfoProjectionGap::MissingObject)
        );
        let other = FdinfoRawObject {
            inode: RawInode::new(33, 1000),
            ..f.object
        };
        assert_eq!(
            f.cache.apply(
                f.admitted,
                batch(
                    3,
                    vec![FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Object {
                        open_file: f.ofd,
                        object: Some(other),
                    })]
                )
            ),
            Err(FdinfoProjectionError::ObjectConflict)
        );
    }

    #[test]
    fn duplicate_seed_slots_and_unknown_ofds_reject_whole_batch() {
        let mut f = fixture();
        let child = DetTid::from_raw(4);
        let files = FilesId::forked(child);
        let incarnation = FdinfoIncarnation {
            tid: child,
            mm: MmId::initial(child),
        };
        let slot = FdinfoSlotProjection {
            open_file: f.ofd,
            cloexec: false,
        };
        let seed = FdinfoTableSeed {
            files,
            origin: FdinfoTableOrigin::Fork { source: f.files },
            slots: vec![(7, slot)],
            ofds: vec![],
            complete: true,
        };
        f.cache
            .reserve_pending_seed(f.admitted, incarnation, seed.clone())
            .unwrap();
        let admitted = f
            .cache
            .register_after_admission(
                incarnation,
                FdinfoFilesAdmission {
                    files,
                    new_origin: Some(seed.origin),
                },
            )
            .unwrap();
        let mut duplicate = seed.clone();
        duplicate.slots.push((7, slot));
        assert_eq!(
            f.cache.apply(
                admitted,
                batch(1, vec![FdinfoProjectionDelta::Table(duplicate)])
            ),
            Err(FdinfoProjectionError::InvalidSlot)
        );
        assert!(f.cache.table(files).is_none());
        let mut unknown = seed;
        unknown.slots[0].1.open_file = OpenFileId::new(child, 99);
        assert_eq!(
            f.cache.apply(
                admitted,
                batch(1, vec![FdinfoProjectionDelta::Table(unknown)])
            ),
            Err(FdinfoProjectionError::UnknownOfd)
        );
        assert!(f.cache.table(files).is_none());
    }

    #[test]
    fn captured_projection_roundtrip_is_owned_evidence_not_a_live_target_query() {
        let mut f = fixture();
        let captured = capture(&mut f);
        let bytes = bincode::serde::encode_to_vec(&captured, bincode::config::standard()).unwrap();
        f.cache.retire_after_admission(f.admitted).unwrap();
        let (recorded, consumed): (FdinfoCapturedTarget, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(recorded, captured);
        assert_eq!(
            recorded.ofd.resource,
            Some(ResourceID::Device(
                crate::resources::Device::ContainerStdout
            ))
        );
        assert_eq!(recorded.ofd.open_file, f.ofd);
    }

    #[test]
    fn stale_lifetime_or_unrelated_admission_cannot_rebind_a_known_task() {
        let mut f = fixture();
        let other_lifetime = FdinfoTaskLifetime {
            start_ticks: f.lifetime.start_ticks + 1,
            ..f.lifetime
        };
        assert_eq!(
            f.cache.bind_open_subject(f.alias, other_lifetime),
            Err(FdinfoProjectionGap::UnboundAlias)
        );
        let child = DetTid::from_raw(9);
        let admission = f
            .cache
            .register_after_admission(
                FdinfoIncarnation {
                    tid: child,
                    mm: MmId::initial(child),
                },
                FdinfoFilesAdmission {
                    files: f.files,
                    new_origin: None,
                },
            )
            .unwrap();
        assert_eq!(
            f.cache.apply(
                admission,
                batch(
                    1,
                    vec![FdinfoProjectionDelta::BindTask {
                        lifetime: f.lifetime,
                        previous: Some(f.incarnation),
                    }]
                )
            ),
            Err(FdinfoProjectionError::TaskChanged)
        );
        assert_eq!(f.cache.tasks[&f.lifetime].current, Some(f.incarnation));
        assert_eq!(
            f.cache.bind_open_subject(f.alias, f.lifetime),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
    }

    #[test]
    fn authenticated_positive_slot_is_usable_but_partial_table_miss_is_unknown() {
        let mut f = fixture();
        let child = DetTid::from_raw(9);
        let files = FilesId::forked(child);
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 404),
            start_ticks: 31,
        };
        let alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Task { pid: 5, tid: 9 },
            directory: lifetime.birth_anchor,
            ..f.alias
        };
        let incarnation = FdinfoIncarnation {
            tid: child,
            mm: MmId::initial(child),
        };
        let seed = FdinfoTableSeed {
            files,
            origin: FdinfoTableOrigin::Fork { source: f.files },
            slots: vec![(
                7,
                FdinfoSlotProjection {
                    open_file: f.ofd,
                    cloexec: false,
                },
            )],
            ofds: vec![],
            complete: false,
        };
        f.cache
            .reserve_pending_seed(f.admitted, incarnation, seed.clone())
            .unwrap();
        let admission = f
            .cache
            .register_after_admission(
                incarnation,
                FdinfoFilesAdmission {
                    files,
                    new_origin: Some(seed.origin),
                },
            )
            .unwrap();
        f.cache
            .apply(
                admission,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(seed),
                        FdinfoProjectionDelta::BindTask {
                            lifetime,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias { alias, lifetime },
                    ],
                ),
            )
            .unwrap();
        let subject = f.cache.bind_open_subject(alias, lifetime).unwrap();
        assert!(matches!(
            f.cache.classify(subject),
            Ok(FdinfoSubjectClass::Managed { .. })
        ));
        assert_eq!(
            f.cache.capture(subject, 7, f.object).unwrap().ofd.open_file,
            f.ofd
        );
        assert_eq!(
            f.cache.capture(subject, 3, f.object),
            Err(FdinfoProjectionGap::PartialTable)
        );
    }

    #[test]
    fn repeated_open_close_reclaims_ofds_and_retired_owner_state() {
        let mut f = fixture();
        let saved = capture(&mut f);
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                        files: f.files,
                        fd: 7,
                        expected: f.ofd,
                    })],
                ),
            )
            .unwrap();
        assert!(f.cache.ofds.is_empty());
        for turn in 0..32 {
            let open_file = OpenFileId::new(f.incarnation.tid, turn + 2);
            let object = FdinfoRawObject {
                inode: RawInode::new(33, turn + 1000),
                ..f.object
            };
            f.cache
                .apply(
                    f.admitted,
                    batch(
                        3 + 2 * turn,
                        vec![
                            FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Seed(FdinfoOfdProjection {
                                open_file,
                                status_flags: libc::O_RDONLY,
                                resource: None,
                                ty: FdType::Regular,
                                object: Some(object),
                            })),
                            FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Install {
                                files: f.files,
                                fd: 7,
                                slot: FdinfoSlotProjection {
                                    open_file,
                                    cloexec: false,
                                },
                            }),
                        ],
                    ),
                )
                .unwrap();
            assert_eq!(
                f.cache.capture(subject, 7, object).unwrap().ofd.open_file,
                open_file
            );
            f.cache
                .apply(
                    f.admitted,
                    batch(
                        4 + 2 * turn,
                        vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                            files: f.files,
                            fd: 7,
                            expected: open_file,
                        })],
                    ),
                )
                .unwrap();
            assert!(f.cache.ofds.is_empty());
            assert!(f.cache.table(f.files).unwrap().slots.is_empty());
        }
        f.cache.retire_after_admission(f.admitted).unwrap();
        assert!(f.cache.tables.is_empty());
        assert!(f.cache.senders.is_empty());
        assert!(f.cache.incarnations.is_empty());
        assert!(f.cache.aliases.is_empty());
        assert!(f.cache.views.is_empty());
        assert_eq!(f.cache.tasks.len(), 1); // Only the actual open subject holds it.
        assert_eq!(
            f.cache.classify(subject),
            Err(FdinfoProjectionGap::RetiredTask)
        );
        f.cache.release_open_subject(subject).unwrap();
        assert!(f.cache.tasks.is_empty());
        assert!(f.cache.open_subjects.is_empty());
        assert_eq!(
            f.cache.release_open_subject(subject),
            Err(FdinfoProjectionError::StaleOwner)
        );
        // Recording owns old facts independently of all current cache owners.
        let bytes = bincode::serde::encode_to_vec(&saved, bincode::config::standard()).unwrap();
        let (recorded, consumed): (FdinfoCapturedTarget, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(recorded, saved);
        assert_eq!(recorded.ofd.open_file, f.ofd);
    }

    #[test]
    fn pending_fork_seed_survives_parent_close_and_retirement_without_stale_replacement() {
        let mut f = fixture();
        let child = DetTid::from_raw(4);
        let incarnation = FdinfoIncarnation {
            tid: child,
            mm: MmId::initial(child),
        };
        let files = FilesId::forked(child);
        let seed = FdinfoTableSeed {
            files,
            origin: FdinfoTableOrigin::Fork { source: f.files },
            slots: vec![(
                7,
                FdinfoSlotProjection {
                    open_file: f.ofd,
                    cloexec: false,
                },
            )],
            ofds: vec![],
            complete: true,
        };
        f.cache
            .reserve_pending_seed(f.admitted, incarnation, seed.clone())
            .unwrap();
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                        files: f.files,
                        fd: 7,
                        expected: f.ofd,
                    })],
                ),
            )
            .unwrap();
        f.cache.retire_after_admission(f.admitted).unwrap();
        assert!(f.cache.senders.is_empty());
        assert_eq!(f.cache.ofds[&f.ofd].references, 1);
        let admitted = f
            .cache
            .register_after_admission(
                incarnation,
                FdinfoFilesAdmission {
                    files,
                    new_origin: Some(seed.origin),
                },
            )
            .unwrap();
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 202),
            start_ticks: 30,
        };
        let alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Process { pid: 6 },
            directory: lifetime.birth_anchor,
            ..f.alias
        };
        f.cache
            .apply(
                admitted,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(seed),
                        FdinfoProjectionDelta::BindTask {
                            lifetime,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias { alias, lifetime },
                    ],
                ),
            )
            .unwrap();
        assert!(f.cache.pending.is_empty());
        assert!(f.cache.table(f.files).is_none());
        assert_eq!(f.cache.ofds[&f.ofd].references, 1); // Only the real child slot.
        let subject = f.cache.bind_open_subject(alias, lifetime).unwrap();
        assert_eq!(
            f.cache.capture(subject, 7, f.object).unwrap().ofd.open_file,
            f.ofd
        );
        f.cache.retire_after_admission(admitted).unwrap();
        assert!(f.cache.tables.is_empty());
        assert!(f.cache.ofds.is_empty());
        f.cache.release_open_subject(subject).unwrap();
        assert!(f.cache.tasks.is_empty());
    }

    #[test]
    fn cancel_pending_and_last_shared_table_owner_release_only_unreferenced_metadata() {
        let mut f = fixture();
        let child = DetTid::from_raw(4);
        let incarnation = FdinfoIncarnation {
            tid: child,
            mm: MmId::initial(child),
        };
        let files = FilesId::forked(child);
        let seed = FdinfoTableSeed {
            files,
            origin: FdinfoTableOrigin::Fork { source: f.files },
            slots: vec![(
                7,
                FdinfoSlotProjection {
                    open_file: f.ofd,
                    cloexec: false,
                },
            )],
            ofds: vec![],
            complete: true,
        };
        let pending = f
            .cache
            .reserve_pending_seed(f.admitted, incarnation, seed)
            .unwrap();
        let shared_tid = DetTid::from_raw(5);
        let shared = f
            .cache
            .register_after_admission(
                FdinfoIncarnation {
                    tid: shared_tid,
                    mm: MmId::initial(shared_tid),
                },
                FdinfoFilesAdmission {
                    files: f.files,
                    new_origin: None,
                },
            )
            .unwrap();
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 202),
            start_ticks: 30,
        };
        let alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Task { pid: 5, tid: 6 },
            directory: lifetime.birth_anchor,
            ..f.alias
        };
        f.cache
            .apply(
                shared,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::BindTask {
                            lifetime,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias { alias, lifetime },
                    ],
                ),
            )
            .unwrap();
        f.cache.cancel_pending_seed(pending).unwrap();
        assert_eq!(
            f.cache.cancel_pending_seed(pending),
            Err(FdinfoProjectionError::StaleOwner)
        );
        f.cache.retire_after_admission(f.admitted).unwrap();
        let subject = f.cache.bind_open_subject(alias, lifetime).unwrap();
        assert_eq!(
            f.cache.capture(subject, 7, f.object).unwrap().ofd.resource,
            Some(ResourceID::Device(
                crate::resources::Device::ContainerStdout
            ))
        );
        assert_eq!(f.cache.ofds[&f.ofd].references, 1);
        f.cache.retire_after_admission(shared).unwrap();
        assert!(f.cache.tables.is_empty());
        assert!(f.cache.ofds.is_empty());
        f.cache.release_open_subject(subject).unwrap();
        assert!(f.cache.tasks.is_empty());
    }

    #[test]
    fn late_failed_batch_restores_touched_slots_references_and_aliases_atomically() {
        let mut f = fixture();
        let old = capture(&mut f);
        let ofd = OpenFileId::new(f.incarnation.tid, 2);
        let object = FdinfoRawObject {
            inode: RawInode::new(33, 999),
            ..f.object
        };
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 202),
            start_ticks: 30,
        };
        let alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Process { pid: 6 },
            directory: lifetime.birth_anchor,
            ..f.alias
        };
        let old_next = f.cache.next_owner;
        assert_eq!(
            f.cache.apply(
                f.admitted,
                batch(
                    2,
                    vec![
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Resource {
                            open_file: f.ofd,
                            resource: None
                        }),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                            files: f.files,
                            fd: 7,
                            expected: f.ofd
                        }),
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Seed(FdinfoOfdProjection {
                            open_file: ofd,
                            status_flags: libc::O_RDONLY,
                            resource: None,
                            ty: FdType::Regular,
                            object: Some(object)
                        })),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Install {
                            files: f.files,
                            fd: 8,
                            slot: FdinfoSlotProjection {
                                open_file: ofd,
                                cloexec: false
                            }
                        }),
                        FdinfoProjectionDelta::BindAlias { alias, lifetime },
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                            files: f.files,
                            fd: 99,
                            expected: ofd
                        }),
                    ]
                )
            ),
            Err(FdinfoProjectionError::SlotChanged)
        );
        assert_eq!(f.cache.table(f.files).unwrap().slots.len(), 1);
        assert_eq!(f.cache.table(f.files).unwrap().slots[&7], old.slot);
        assert_eq!(f.cache.table(f.files).unwrap().open_files[&f.ofd], 1);
        assert_eq!(f.cache.ofds.len(), 1);
        assert_eq!(f.cache.ofds[&f.ofd].projection, old.ofd);
        assert_eq!(f.cache.ofds[&f.ofd].references, 1);
        assert!(!f.cache.aliases.contains_key(&alias));
        assert!(!f.cache.tasks.contains_key(&lifetime));
        assert_eq!(f.cache.next_owner, old_next);
        assert_eq!(f.cache.sender(f.admitted).unwrap().last_sequence, 1);
        assert_eq!(
            f.cache.bind_open_subject(f.alias, f.lifetime),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
    }

    #[test]
    fn nonleader_exec_keeps_former_actor_files_id_with_authenticated_leader_mm() {
        let mut f = fixture();
        let former = DetTid::from_raw(4);
        let worker = FdinfoIncarnation {
            tid: former,
            mm: f.incarnation.mm,
        };
        let worker_admitted = f
            .cache
            .register_after_admission(
                worker,
                FdinfoFilesAdmission {
                    files: f.files,
                    new_origin: None,
                },
            )
            .unwrap();
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: RawInode::new(9, 202),
            start_ticks: 30,
        };
        let alias = FdinfoProcAlias {
            kind: FdinfoProcAliasKind::Task { pid: 5, tid: 6 },
            directory: lifetime.birth_anchor,
            ..f.alias
        };
        f.cache
            .apply(
                worker_admitted,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::BindTask {
                            lifetime,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias { alias, lifetime },
                    ],
                ),
            )
            .unwrap();
        let bound = f.cache.bind_open_subject(alias, lifetime).unwrap();
        // c69 for_exec receives the former caller before Linux transfers it
        // to the process leader TID. MmId instead advances under that leader.
        let next = FdinfoIncarnation {
            tid: f.incarnation.tid,
            mm: worker.mm.for_exec(f.incarnation.tid),
        };
        let actual_files = f.files.for_exec(former);
        let seed = FdinfoTableSeed {
            files: actual_files,
            origin: FdinfoTableOrigin::Exec { source: f.files },
            slots: vec![(
                7,
                FdinfoSlotProjection {
                    open_file: f.ofd,
                    cloexec: false,
                },
            )],
            ofds: vec![],
            complete: true,
        };
        assert_ne!(actual_files, f.files.for_exec(next.tid));
        let mut rekeyed = seed.clone();
        rekeyed.files = f.files.for_exec(next.tid);
        assert_eq!(
            f.cache.reserve_pending_seed(worker_admitted, next, rekeyed),
            Err(FdinfoProjectionError::TableOrigin)
        );
        let wrong_mm = FdinfoIncarnation {
            tid: next.tid,
            mm: worker.mm.for_exec(former),
        };
        assert_eq!(
            f.cache
                .reserve_pending_seed(worker_admitted, wrong_mm, seed.clone()),
            Err(FdinfoProjectionError::TableOrigin)
        );
        f.cache
            .reserve_pending_seed(worker_admitted, next, seed.clone())
            .unwrap();
        // Actual sibling teardown precedes authenticated nonleader takeover;
        // the model neither assigns that lifecycle nor changes its ordering.
        f.cache.retire_after_admission(f.admitted).unwrap();
        let replacement = f
            .cache
            .replace_after_exec(
                worker_admitted,
                next,
                FdinfoFilesAdmission {
                    files: actual_files,
                    new_origin: Some(seed.origin),
                },
            )
            .unwrap();
        f.cache
            .apply(
                replacement,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(seed),
                        FdinfoProjectionDelta::BindTask {
                            lifetime,
                            previous: Some(worker),
                        },
                    ],
                ),
            )
            .unwrap();
        let acquired = f.cache.capture(bound, 7, f.object).unwrap();
        assert_eq!(acquired.files, actual_files);
        assert_eq!(acquired.incarnation, next);
        assert_eq!(acquired.ofd.open_file, f.ofd);
        assert_eq!(
            acquired.ofd.resource,
            Some(ResourceID::Device(
                crate::resources::Device::ContainerStdout
            ))
        );
        assert_eq!(
            f.cache.apply(worker_admitted, batch(2, vec![])),
            Err(FdinfoProjectionError::StaleAdmission)
        );
        f.cache.release_open_subject(bound).unwrap();
    }

    // Genuine new-table seed with no acquired raw object; do not erase an
    // existing canonical anchor to manufacture the first-observation case.
    fn unanchored_fixture() -> Fixture {
        let mut f = fixture();
        let mut projection = f.cache.ofds[&f.ofd].projection.clone();
        projection.object = None;
        f.cache = FdinfoProjectionCache::default();
        f.admitted = f
            .cache
            .register_after_admission(
                f.incarnation,
                FdinfoFilesAdmission {
                    files: f.files,
                    new_origin: Some(FdinfoTableOrigin::Initial),
                },
            )
            .unwrap();
        f.cache
            .apply(
                f.admitted,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(FdinfoTableSeed {
                            files: f.files,
                            origin: FdinfoTableOrigin::Initial,
                            slots: vec![(
                                7,
                                FdinfoSlotProjection {
                                    open_file: f.ofd,
                                    cloexec: false,
                                },
                            )],
                            ofds: vec![projection],
                            complete: false,
                        }),
                        FdinfoProjectionDelta::BindTask {
                            lifetime: f.lifetime,
                            previous: None,
                        },
                        FdinfoProjectionDelta::BindAlias {
                            alias: f.alias,
                            lifetime: f.lifetime,
                        },
                    ],
                ),
            )
            .unwrap();
        f
    }

    #[test]
    fn positive_partial_slot_captures_first_anchor_and_output_outlives_pruning() {
        let mut f = unanchored_fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        assert_eq!(
            f.cache.capture(subject, 7, f.object),
            Err(FdinfoProjectionGap::MissingObject)
        );
        assert_eq!(
            f.cache.capture_current_slot_token(subject, 8),
            Err(FdinfoProjectionGap::PartialTable)
        );
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        assert_eq!(token.ofd.object, None);
        let old_flags = token.ofd.status_flags;
        let old_resource = token.ofd.resource.clone();
        let old_slot = token.slot;
        let revision = f.cache.revision;
        let captured = f
            .cache
            .capture_current_slot_object(token.clone(), f.object)
            .unwrap();
        assert_eq!(captured.ofd.object, Some(f.object));
        assert_eq!(captured.ofd.open_file, f.ofd);
        assert_eq!(captured.ofd.status_flags, old_flags);
        assert_eq!(captured.ofd.resource, old_resource);
        assert_eq!(captured.slot, old_slot);
        assert_eq!(captured.revision, revision + 1);
        assert_eq!(f.cache.ofds[&f.ofd].anchor, Some(f.object));
        assert_eq!(
            f.cache.capture_current_slot_object(token, f.object),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
        assert_eq!(f.cache.capture(subject, 7, f.object).unwrap(), captured);
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                        files: f.files,
                        fd: 7,
                        expected: f.ofd,
                    })],
                ),
            )
            .unwrap();
        assert!(f.cache.ofds.is_empty());
        assert_eq!(captured.ofd.object, Some(f.object)); // Owned recorded facts.
        f.cache.release_open_subject(subject).unwrap();
        f.cache.retire_after_admission(f.admitted).unwrap();
        assert!(f.cache.tables.is_empty());
    }

    #[test]
    fn current_slot_token_rejects_replacement_same_bytes_and_current_table_change() {
        let mut f = unanchored_fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        let replacement = OpenFileId::new(f.incarnation.tid, 2);
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![
                        FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Seed(FdinfoOfdProjection {
                            open_file: replacement,
                            status_flags: libc::O_RDONLY,
                            resource: None,
                            ty: FdType::Regular,
                            object: None,
                        })),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Install {
                            files: f.files,
                            fd: 7,
                            slot: FdinfoSlotProjection {
                                open_file: replacement,
                                cloexec: false,
                            },
                        }),
                    ],
                ),
            )
            .unwrap();
        // Even equal dev/ino is not equality of open descriptions/slots.
        assert_eq!(
            f.cache.capture_current_slot_object(token, f.object),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
        assert_eq!(f.cache.ofds[&replacement].anchor, None);
        let current = f.cache.capture_current_slot_token(subject, 7).unwrap();
        assert_eq!(
            f.cache
                .capture_current_slot_object(current, f.object)
                .unwrap()
                .ofd
                .open_file,
            replacement
        );
        f.cache.release_open_subject(subject).unwrap();

        let mut f = unanchored_fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Remove {
                            files: f.files,
                            fd: 7,
                            expected: f.ofd,
                        }),
                        FdinfoProjectionDelta::Slot(FdinfoSlotDelta::Install {
                            files: f.files,
                            fd: 7,
                            slot: FdinfoSlotProjection {
                                open_file: f.ofd,
                                cloexec: false,
                            },
                        }),
                    ],
                ),
            )
            .unwrap();
        assert_eq!(f.cache.table(f.files).unwrap().slots[&7], token.slot);
        assert_eq!(
            f.cache.capture_current_slot_object(token, f.object),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
        assert_eq!(f.cache.ofds[&f.ofd].anchor, None);
        f.cache.release_open_subject(subject).unwrap();

        let mut f = unanchored_fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        let next = FdinfoIncarnation {
            tid: f.incarnation.tid,
            mm: f.incarnation.mm.for_exec(f.incarnation.tid),
        };
        let files = f.files.for_exec(f.incarnation.tid);
        let seed = FdinfoTableSeed {
            files,
            origin: FdinfoTableOrigin::Exec { source: f.files },
            slots: vec![(7, token.slot)],
            ofds: vec![],
            complete: false,
        };
        f.cache
            .reserve_pending_seed(f.admitted, next, seed.clone())
            .unwrap();
        let replacement = f
            .cache
            .replace_after_exec(
                f.admitted,
                next,
                FdinfoFilesAdmission {
                    files,
                    new_origin: Some(seed.origin),
                },
            )
            .unwrap();
        f.cache
            .apply(
                replacement,
                batch(
                    1,
                    vec![
                        FdinfoProjectionDelta::Table(seed),
                        FdinfoProjectionDelta::BindTask {
                            lifetime: f.lifetime,
                            previous: Some(f.incarnation),
                        },
                    ],
                ),
            )
            .unwrap();
        assert_eq!(
            f.cache.capture_current_slot_object(token, f.object),
            Err(FdinfoProjectionGap::RejectedUpdate)
        );
        assert_eq!(
            f.cache
                .capture_current_slot_token(subject, 7)
                .unwrap()
                .files,
            files
        );
        assert_eq!(f.cache.ofds[&f.ofd].anchor, None);
        f.cache.release_open_subject(subject).unwrap();
    }

    #[test]
    fn current_slot_anchor_requires_exact_raw_object_and_never_replaces_logical_facts() {
        let mut f = fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        let projection = f.cache.ofds[&f.ofd].projection.clone();
        let revision = f.cache.revision;
        for changed in [
            FdinfoRawObject {
                inode: RawInode::new(34, 256),
                ..f.object
            },
            FdinfoRawObject {
                inode: RawInode::new(33, 257),
                ..f.object
            },
            FdinfoRawObject {
                mode: libc::S_IFREG | 0o700,
                ..f.object
            },
            FdinfoRawObject {
                mount_id: f.object.mount_id + 1,
                ..f.object
            },
        ] {
            assert_eq!(
                f.cache.capture_current_slot_object(token.clone(), changed),
                Err(FdinfoProjectionGap::ObjectChanged)
            );
            assert_eq!(f.cache.ofds[&f.ofd].projection, projection);
            assert_eq!(f.cache.ofds[&f.ofd].anchor, Some(f.object));
            assert_eq!(f.cache.revision, revision);
            assert!(f.cache.valid);
        }
        let observed = FdinfoRawObject {
            mode: libc::S_IFDIR | 0o500,
            ..f.object
        };
        let captured = f
            .cache
            .capture_current_slot_object(token, observed)
            .unwrap();
        assert_eq!(captured.observed_object, observed);
        assert_eq!(captured.ofd, projection);
        assert_eq!(captured.revision, revision);
        // Object(None) does not erase immutable first-object identity.
        f.cache
            .apply(
                f.admitted,
                batch(
                    2,
                    vec![FdinfoProjectionDelta::Ofd(FdinfoOfdDelta::Object {
                        open_file: f.ofd,
                        object: None,
                    })],
                ),
            )
            .unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        let conflict = FdinfoRawObject {
            inode: RawInode::new(33, 257),
            ..f.object
        };
        assert_eq!(
            f.cache.capture_current_slot_object(token.clone(), conflict),
            Err(FdinfoProjectionGap::ObjectChanged)
        );
        assert_eq!(f.cache.ofds[&f.ofd].projection.object, None);
        let restored = f
            .cache
            .capture_current_slot_object(token, f.object)
            .unwrap();
        assert_eq!(restored.ofd.object, Some(f.object));
        assert_eq!(restored.ofd.resource, projection.resource);
        assert_eq!(restored.ofd.status_flags, projection.status_flags);
        assert_eq!(restored.slot.cloexec, captured.slot.cloexec);
        f.cache.release_open_subject(subject).unwrap();
    }

    #[test]
    fn current_slot_token_cannot_revive_retired_target_or_released_proc_subject() {
        let mut f = unanchored_fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        f.cache.retire_after_admission(f.admitted).unwrap();
        assert_eq!(
            f.cache.capture_current_slot_object(token, f.object),
            Err(FdinfoProjectionGap::RetiredTask)
        );
        assert!(f.cache.tables.is_empty());
        assert!(f.cache.ofds.is_empty());
        f.cache.release_open_subject(subject).unwrap();
        assert!(f.cache.tasks.is_empty());

        let mut f = unanchored_fixture();
        let subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
        let token = f.cache.capture_current_slot_token(subject, 7).unwrap();
        f.cache.release_open_subject(subject).unwrap();
        assert_eq!(
            f.cache.capture_current_slot_object(token, f.object),
            Err(FdinfoProjectionGap::RetiredTask)
        );
        assert_eq!(f.cache.ofds[&f.ofd].anchor, None);
        assert_eq!(f.cache.ofds[&f.ofd].projection.object, None);
    }

    #[test]
    fn authenticated_exec_refreshes_aliases_without_redirecting_old_lifetimes() {
        for deferred in [false, true] {
            for changed_lifetime in [false, true] {
                let mut f = fixture();
                let old_subject = f.cache.bind_open_subject(f.alias, f.lifetime).unwrap();
                let leader = DetTid::from_raw(2);
                let replacement = FdinfoIncarnation {
                    tid: leader,
                    mm: f.incarnation.mm.for_exec(leader),
                };
                // Nonleader exec retains the former actor's genuine FilesId,
                // while the admitted replacement uses the process leader.
                let files = f.files.for_exec(f.incarnation.tid);
                let seed = FdinfoTableSeed {
                    files,
                    origin: FdinfoTableOrigin::Exec { source: f.files },
                    slots: vec![(
                        7,
                        FdinfoSlotProjection {
                            open_file: f.ofd,
                            cloexec: false,
                        },
                    )],
                    ofds: vec![],
                    complete: true,
                };
                f.cache
                    .reserve_pending_seed(f.admitted, replacement, seed.clone())
                    .unwrap();
                let admission = f
                    .cache
                    .replace_after_exec(
                        f.admitted,
                        replacement,
                        FdinfoFilesAdmission {
                            files,
                            new_origin: Some(seed.origin),
                        },
                    )
                    .unwrap();
                let observed = if changed_lifetime {
                    FdinfoTaskLifetime {
                        birth_anchor: RawInode::new(9, 202),
                        start_ticks: 31,
                    }
                } else {
                    f.lifetime
                };
                let previous = if deferred {
                    let permit = f.cache.defer_exec_task_binding(admission).unwrap();
                    f.cache
                        .apply(
                            admission,
                            batch(1, vec![FdinfoProjectionDelta::Table(seed.clone())]),
                        )
                        .unwrap();
                    assert_eq!(
                        f.cache.classify(old_subject),
                        Err(FdinfoProjectionGap::RetiredTask)
                    );
                    assert!(f.cache.defer_exec_task_binding(admission).is_err());
                    f.cache
                        .refresh_deferred_exec_task_binding(admission, permit, observed)
                        .unwrap()
                } else {
                    f.cache
                        .refresh_exec_task_binding(admission, observed)
                        .unwrap();
                    f.cache
                        .admitted_task_lifetime(admission)
                        .unwrap()
                        .map(|_| f.incarnation)
                };
                assert_eq!(previous, (!changed_lifetime).then_some(f.incarnation));
                let fresh_alias = FdinfoProcAlias {
                    kind: FdinfoProcAliasKind::Task { pid: 2, tid: 2 },
                    directory: observed.birth_anchor,
                    ..f.alias
                };
                let mut deltas = if deferred {
                    Vec::new()
                } else {
                    vec![FdinfoProjectionDelta::Table(seed)]
                };
                deltas.extend([
                    FdinfoProjectionDelta::BindTask {
                        lifetime: observed,
                        previous,
                    },
                    FdinfoProjectionDelta::BindAlias {
                        alias: fresh_alias,
                        lifetime: observed,
                    },
                ]);
                f.cache
                    .apply(admission, batch(if deferred { 2 } else { 1 }, deltas))
                    .unwrap();
                let fresh_subject = f.cache.bind_open_subject(fresh_alias, observed).unwrap();
                let projected = f.cache.capture(fresh_subject, 7, f.object).unwrap();
                assert_eq!(projected.incarnation, replacement);
                assert_eq!(projected.files, files);
                assert_eq!(projected.ofd.open_file, f.ofd);
                assert_eq!(
                    projected.ofd.resource,
                    Some(ResourceID::Device(
                        crate::resources::Device::ContainerStdout
                    ))
                );
                if changed_lifetime {
                    assert_eq!(
                        f.cache.classify(old_subject),
                        Err(FdinfoProjectionGap::RetiredTask)
                    );
                } else {
                    assert_eq!(
                        f.cache.capture(old_subject, 7, f.object).unwrap().files,
                        files
                    );
                }
                f.cache.release_open_subject(fresh_subject).unwrap();
                f.cache.release_open_subject(old_subject).unwrap();
            }
        }
    }
}
