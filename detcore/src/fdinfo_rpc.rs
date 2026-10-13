/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Admission and publication for the read-only fdinfo projection. The existing
//! scheduler transaction supplies admission; these facts never grant a turn.

use std::collections::HashMap;

use reverie::SignalTaskIdentity;
use serde::Deserialize;
use serde::Serialize;

use crate::fdinfo_projection::*;
use crate::syscalls::fdinfo_acquisition::ProcFdinfoOpenSubjectRelation;
use crate::syscalls::fdinfo_acquisition::ProcIdentityAcquisition;
use crate::syscalls::fdinfo_acquisition::ProcIdentityFact;
use crate::syscalls::fdinfo_acquisition::ProcRawAliasName;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::FilesId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoOriginalParent {
    pub alias: FdinfoProcAlias,
    pub lifetime: FdinfoTaskLifetime,
    pub mount_namespace: crate::types::RawInode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoOwnIdentity {
    pub lifetime: FdinfoTaskLifetime,
    pub aliases: Vec<FdinfoProcAlias>,
    pub original_parent_candidate: Option<FdinfoOriginalParent>,
}

fn observed<T>(fact: &ProcIdentityFact<T>) -> Option<&T> {
    match fact {
        ProcIdentityFact::Observed(value) => Some(value),
        _ => None,
    }
}

impl FdinfoOwnIdentity {
    /// Only the represented task's own raw proc aliases are exported. The
    /// process alias belongs to its leader, not any coincident logical DetTid.
    pub(crate) fn from_acquisition(raw: &ProcIdentityAcquisition) -> Option<Self> {
        if !raw.cleanup_complete() || observed(&raw.procfs_type) != Some(&0x9fa0) {
            return None;
        }
        // Application getters may be virtualized by the backend. Resolve raw
        // names in the held proc view from its canonical self links instead;
        // directory/stat/lifetime and namespace checks below remain authority.
        let (proc_pid, proc_tid) = crate::syscalls::fdinfo_acquisition::proc_view_self_ids(
            &raw.process_link_bytes,
            &raw.task_link_bytes,
        );
        let pid = *observed(&proc_pid)?;
        let tid = *observed(&proc_tid)?;
        if pid <= 0
            || tid <= 0
            || observed(&raw.process_alias) != Some(&ProcRawAliasName::Process { pid })
            || observed(&raw.task_alias) != Some(&ProcRawAliasName::Task { pid, tid })
        {
            return None;
        }
        let root = *observed(&raw.procfs_root)?;
        let task_dir = *observed(&raw.task.directory)?;
        let task_stat = *observed(&raw.task.stat)?;
        let mounted = *observed(&raw.mounted_pid_namespace)?;
        let pid_ns = *observed(&raw.task.pid_namespace)?;
        let mount_ns = *observed(&raw.task.mount_namespace)?;
        if root.mode & libc::S_IFMT != libc::S_IFDIR
            || task_dir.mode & libc::S_IFMT != libc::S_IFDIR
            || task_dir.mount_id != root.mount_id
            || root.mount_id.is_none()
            || task_stat.pid != tid
            || mounted.inode != pid_ns.inode
            || pid_ns.mode & libc::S_IFMT != libc::S_IFREG
            || mount_ns.mode & libc::S_IFMT != libc::S_IFREG
        {
            return None;
        }
        let view = FdinfoProcView {
            procfs_root: root.inode,
            pid_namespace: mounted.inode,
            mount_namespace: mount_ns.inode,
        };
        let lifetime = FdinfoTaskLifetime {
            birth_anchor: task_dir.inode,
            start_ticks: task_stat.start_ticks,
        };
        let mut aliases = vec![FdinfoProcAlias {
            view,
            kind: FdinfoProcAliasKind::Task { pid, tid },
            directory: task_dir.inode,
        }];
        if pid == tid {
            let process_dir = *observed(&raw.process.directory)?;
            let process_stat = *observed(&raw.process.stat)?;
            if process_dir.mode & libc::S_IFMT != libc::S_IFDIR
                || process_dir.mount_id != root.mount_id
                || process_stat != task_stat
                || observed(&raw.process.pid_namespace)?.inode != mounted.inode
                || observed(&raw.process.mount_namespace)?.inode != mount_ns.inode
            {
                return None;
            }
            aliases.push(FdinfoProcAlias {
                view,
                kind: FdinfoProcAliasKind::Process { pid },
                directory: process_dir.inode,
            });
        }
        let original_parent_candidate =
            raw.observed_initial_parent_relation()
                .and_then(|(_, parent)| {
                    let dir = *observed(&raw.original_parent_candidate.directory)?;
                    let mount_namespace =
                        observed(&raw.original_parent_candidate.mount_namespace)?.inode;
                    Some(FdinfoOriginalParent {
                        alias: FdinfoProcAlias {
                            view,
                            kind: FdinfoProcAliasKind::Process { pid: parent.pid },
                            directory: dir.inode,
                        },
                        lifetime: FdinfoTaskLifetime {
                            birth_anchor: dir.inode,
                            start_ticks: parent.start_ticks,
                        },
                        mount_namespace,
                    })
                });
        Some(Self {
            lifetime,
            aliases,
            original_parent_candidate,
        })
    }
}

/// An exact table snapshot retained at init_thread_state's successful clone
/// cut. The parent's existing registration request supplies the admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoForkSeed {
    pub child: FdinfoIncarnation,
    pub table: FdinfoTableSeed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoAllocationCursors {
    pub regular: u64,
    pub socket: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoExecSeed {
    pub table: FdinfoTableSeed,
    pub cursors: FdinfoAllocationCursors,
}

/// Projection-only reply to a successfully authenticated loader reconnect.
/// These keys never replace FileMetadata's actual identities or rendered
/// cookie identities. Inherited slots retain their genuine logical OFD ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdinfoReentryBinding {
    pub(crate) incarnation: FdinfoIncarnation,
    pub(crate) table: FdinfoTableSeed,
    pub(crate) cursors: FdinfoAllocationCursors,
    pub(crate) logical_ids: Vec<(crate::types::OpenFileId, crate::types::OpenFileId)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoInheritedSlot {
    pub ofd: FdinfoOfdProjection,
    pub logical: crate::types::OpenFileId,
    pub cloexec: bool,
}

/// Piggybacked on the original request, inside the unchanged time/MM tuple.
/// Unwrapping is not admission and must not apply this payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdinfoPublication {
    pub(crate) files: FilesId,
    pub(crate) origin: Option<FdinfoTableOrigin>,
    pub(crate) batch: FdinfoProjectionBatch,
    pub(crate) forks: Vec<FdinfoForkSeed>,
    pub(crate) logical_ids: Vec<(crate::types::OpenFileId, crate::types::OpenFileId)>,
    pub(crate) exec: Option<FdinfoExecSeed>,
    pub(crate) own_identity: Option<FdinfoOwnIdentity>,
}

#[derive(Debug)]
struct PendingExecProjection {
    actor: FdinfoIncarnation,
    target: FdinfoIncarnation,
    table: FdinfoTableSeed,
    cursors: FdinfoAllocationCursors,
    hold: FdinfoPendingSeed,
}

/// An internal observation/control request on the existing admitted RPC path.
/// It never grants a scheduler turn or carries a deserialized model token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdinfoControl {
    pub(crate) operation: FdinfoControlOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoControlOperation {
    BindOpened {
        source: crate::types::OpenFileId,
        relation: ProcFdinfoOpenSubjectRelation,
    },
    BeginCapture {
        source: crate::types::OpenFileId,
        target_fd: crate::types::RawFd,
    },
    FinishCapture {
        capture: u64,
        object: FdinfoRawObject,
        relation: Option<ProcFdinfoOpenSubjectRelation>,
    },
    BeginCurrent {
        files: FilesId,
        source_fd: crate::types::RawFd,
        source: crate::types::OpenFileId,
        target_fd: crate::types::RawFd,
        actor: SignalTaskIdentity,
    },
    FinishCurrent {
        capture: u64,
        object: FdinfoRawObject,
        actor: SignalTaskIdentity,
    },
    CancelCapture {
        capture: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdinfoControlReply {
    pub(crate) result: Result<FdinfoControlValue, FdinfoProjectionGap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoControlValue {
    Bound,
    Capture(u64),
    Captured(FdinfoCapturedAuthority),
    CapturedCurrent(FdinfoCapturedCurrentAuthority),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FdinfoCapturedCurrentAuthority {
    pub target: Box<FdinfoCapturedCurrentTarget>,
    pub logical_open_file: crate::types::OpenFileId,
}

/// Supplied only from the scheduler's existing registered actor/MM lookup.
/// This is not a wire type and a decoded request cannot mint it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FdinfoRegisteredActor {
    incarnation: FdinfoIncarnation,
    identity: SignalTaskIdentity,
}

impl FdinfoRegisteredActor {
    pub(crate) fn after_registered_lookup(
        incarnation: FdinfoIncarnation,
        identity: SignalTaskIdentity,
    ) -> Self {
        Self {
            incarnation,
            identity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FdinfoCapturedAuthority {
    Managed {
        target: Box<FdinfoCapturedTarget>,
        logical_open_file: crate::types::OpenFileId,
    },
    /// Positive original launch parent only. Logical flags are its successful
    /// kernel snapshot's own flags; no managed stdio or OFD label is borrowed.
    OriginalParent,
}

#[derive(Debug, Clone)]
enum OpenedSubject {
    Managed(FdinfoBoundSubject),
    OriginalParent(Box<ProcFdinfoOpenSubjectRelation>),
}

#[derive(Debug)]
enum CurrentCaptureKind {
    Managed(FdinfoCurrentSlotCapture),
    OriginalParent(ProcFdinfoOpenSubjectRelation),
    Represented {
        token: FdinfoAdmittedSlotCapture,
        actor: SignalTaskIdentity,
    },
}

#[derive(Debug)]
struct CurrentCapture {
    owner: FdinfoIncarnation,
    source: crate::types::OpenFileId,
    kind: CurrentCaptureKind,
}

impl ProcFdinfoOpenSubjectRelation {
    pub(crate) fn projection_alias(self) -> FdinfoProcAlias {
        let kind = match self.alias {
            ProcRawAliasName::Process { pid } => FdinfoProcAliasKind::Process { pid },
            ProcRawAliasName::Task { pid, tid } => FdinfoProcAliasKind::Task { pid, tid },
        };
        FdinfoProcAlias {
            view: FdinfoProcView {
                procfs_root: self.procfs_root.inode,
                pid_namespace: self.mounted_pid_namespace.inode,
                mount_namespace: self.caller_mount_namespace.inode,
            },
            kind,
            directory: self.target_directory.inode,
        }
    }
}

/// All ownership here is bounded by actual admitted tasks and pending
/// lifecycle transactions. Captured fdinfo values own their metadata.
#[derive(Debug, Default)]
pub(crate) struct FdinfoRpcState {
    pub cache: FdinfoProjectionCache,
    admitted: HashMap<DetTid, (FdinfoIncarnation, FdinfoAdmission)>,
    pending_children: HashMap<FdinfoIncarnation, FdinfoPendingSeed>,
    staged_children: HashMap<FdinfoIncarnation, (FdinfoIncarnation, FdinfoForkSeed)>,
    pending_execs: HashMap<DetPid, PendingExecProjection>,
    deferred_exec_identities: HashMap<FdinfoIncarnation, FdinfoDeferredExecIdentity>,
    /// Only private keys differing from the genuine logical ID need an entry.
    /// This is bounded by canonical live/pending OFDs, not allocation history.
    logical_ids: HashMap<crate::types::OpenFileId, crate::types::OpenFileId>,
    /// One owner per actual proc OFD; dup/fork aliases share it. Collection is
    /// driven by canonical live OFD ownership, including held fork/exec cuts.
    opened_subjects: HashMap<crate::types::OpenFileId, OpenedSubject>,
    captures: HashMap<u64, CurrentCapture>,
    next_capture: u64,
    original_root: Option<FdinfoIncarnation>,
    original_parent: Option<(FdinfoIncarnation, FdinfoOriginalParent)>,
}

impl FdinfoRpcState {
    /// Close the one-shot original-root window at its authenticated
    /// registration, even without an instruction context or positive parent
    /// fact. A later raw candidate may follow reparenting and is not admitted.
    pub(crate) fn register_original_root(
        &mut self,
        incarnation: FdinfoIncarnation,
        own: Option<&FdinfoOwnIdentity>,
    ) {
        if self.original_root.is_some() {
            return;
        }
        self.original_root = Some(incarnation);
        if let Some(parent) = own.and_then(|own| own.original_parent_candidate.as_ref()) {
            self.original_parent = Some((incarnation, parent.clone()));
        }
    }
    pub(crate) fn control_admitted(
        &mut self,
        incarnation: FdinfoIncarnation,
        control: FdinfoControl,
    ) -> FdinfoControlReply {
        FdinfoControlReply {
            result: self.control(incarnation, control.operation, None),
        }
    }

    pub(crate) fn control_current_admitted(
        &mut self,
        registered: FdinfoRegisteredActor,
        control: FdinfoControl,
    ) -> FdinfoControlReply {
        FdinfoControlReply {
            result: self.control(registered.incarnation, control.operation, Some(registered)),
        }
    }

    fn authentic_original_parent(&self, relation: ProcFdinfoOpenSubjectRelation) -> bool {
        let Some((_, parent)) = &self.original_parent else {
            return false;
        };
        let alias = relation.projection_alias();
        let pid = match alias.kind {
            FdinfoProcAliasKind::Process { pid } | FdinfoProcAliasKind::Task { pid, .. } => pid,
        };
        let Some((process_dir, start_ticks)) = relation.process_incarnation else {
            return false;
        };
        parent.alias.kind == (FdinfoProcAliasKind::Process { pid })
            && parent.alias.view == alias.view
            && parent.lifetime.birth_anchor == process_dir.inode
            && parent.lifetime.start_ticks == start_ticks
            && relation.target_pid_namespace.inode == parent.alias.view.pid_namespace
            && relation.target_mount_namespace.inode == relation.caller_mount_namespace.inode
    }

    fn control(
        &mut self,
        incarnation: FdinfoIncarnation,
        control: FdinfoControlOperation,
        registered: Option<FdinfoRegisteredActor>,
    ) -> Result<FdinfoControlValue, FdinfoProjectionGap> {
        let admission = self
            .admission(incarnation)
            .map_err(|_| FdinfoProjectionGap::RejectedUpdate)?;
        let owns_source = |cache: &FdinfoProjectionCache, source| {
            cache
                .admitted_contains_ofd(admission, source)
                .map_err(|_| FdinfoProjectionGap::RejectedUpdate)
                .and_then(|owned| owned.then_some(()).ok_or(FdinfoProjectionGap::MissingSlot))
        };
        match control {
            FdinfoControlOperation::BindOpened { source, relation } => {
                owns_source(&self.cache, source)?;
                if self.opened_subjects.contains_key(&source) {
                    return Err(FdinfoProjectionGap::RejectedUpdate);
                }
                let alias = relation.projection_alias();
                let subject = if let Ok(lifetime) = self.cache.registered_alias_lifetime(alias) {
                    if lifetime.start_ticks != relation.target_start_ticks {
                        return Err(FdinfoProjectionGap::RetiredTask);
                    }
                    OpenedSubject::Managed(self.cache.bind_open_subject(alias, lifetime)?)
                } else if self.authentic_original_parent(relation) {
                    OpenedSubject::OriginalParent(Box::new(relation))
                } else {
                    return Err(FdinfoProjectionGap::UnboundAlias);
                };
                self.opened_subjects.insert(source, subject);
                Ok(FdinfoControlValue::Bound)
            }
            FdinfoControlOperation::BeginCapture { source, target_fd } => {
                owns_source(&self.cache, source)?;
                let subject = self
                    .opened_subjects
                    .get(&source)
                    .ok_or(FdinfoProjectionGap::UnboundAlias)?;
                let kind = match subject {
                    OpenedSubject::Managed(subject) => CurrentCaptureKind::Managed(
                        self.cache.capture_current_slot_token(*subject, target_fd)?,
                    ),
                    OpenedSubject::OriginalParent(relation) if relation.target_fd == target_fd => {
                        CurrentCaptureKind::OriginalParent(**relation)
                    }
                    OpenedSubject::OriginalParent(_) => {
                        return Err(FdinfoProjectionGap::MissingSlot);
                    }
                };
                self.captures
                    .retain(|_, capture| capture.owner != incarnation);
                let key = self
                    .next_capture
                    .checked_add(1)
                    .ok_or(FdinfoProjectionGap::RejectedUpdate)?;
                self.next_capture = key;
                self.captures.insert(
                    key,
                    CurrentCapture {
                        owner: incarnation,
                        source,
                        kind,
                    },
                );
                Ok(FdinfoControlValue::Capture(key))
            }
            FdinfoControlOperation::FinishCapture {
                capture,
                object,
                relation,
            } => {
                let current = self
                    .captures
                    .get(&capture)
                    .filter(|capture| capture.owner == incarnation)
                    .ok_or(FdinfoProjectionGap::RejectedUpdate)?;
                owns_source(&self.cache, current.source)?;
                let current = self
                    .captures
                    .remove(&capture)
                    .expect("validated current capture");
                let authority = match current.kind {
                    CurrentCaptureKind::Managed(token) => {
                        let target = self.cache.capture_current_slot_object(token, object)?;
                        let logical_open_file = self
                            .logical_ids
                            .get(&target.ofd.open_file)
                            .copied()
                            .unwrap_or(target.ofd.open_file);
                        FdinfoCapturedAuthority::Managed {
                            target: Box::new(target),
                            logical_open_file,
                        }
                    }
                    CurrentCaptureKind::OriginalParent(opened) => {
                        if relation != Some(opened) || !self.authentic_original_parent(opened) {
                            return Err(FdinfoProjectionGap::UnboundAlias);
                        }
                        FdinfoCapturedAuthority::OriginalParent
                    }
                    CurrentCaptureKind::Represented { .. } => {
                        return Err(FdinfoProjectionGap::RejectedUpdate);
                    }
                };
                Ok(FdinfoControlValue::Captured(authority))
            }
            FdinfoControlOperation::BeginCurrent {
                files,
                source_fd,
                source,
                target_fd,
                actor,
            } => {
                let registered = registered.ok_or(FdinfoProjectionGap::RejectedUpdate)?;
                if registered.identity != actor {
                    return Err(FdinfoProjectionGap::RejectedUpdate);
                }
                owns_source(&self.cache, source)?;
                let token = self
                    .cache
                    .capture_admitted_slot_token(admission, files, source_fd, source, target_fd)?;
                self.captures
                    .retain(|_, capture| capture.owner != incarnation);
                let key = self
                    .next_capture
                    .checked_add(1)
                    .ok_or(FdinfoProjectionGap::RejectedUpdate)?;
                self.next_capture = key;
                self.captures.insert(
                    key,
                    CurrentCapture {
                        owner: incarnation,
                        source,
                        kind: CurrentCaptureKind::Represented { token, actor },
                    },
                );
                Ok(FdinfoControlValue::Capture(key))
            }
            FdinfoControlOperation::FinishCurrent {
                capture,
                object,
                actor,
            } => {
                let registered = registered.ok_or(FdinfoProjectionGap::RejectedUpdate)?;
                if registered.identity != actor {
                    return Err(FdinfoProjectionGap::RejectedUpdate);
                }
                let current = self
                    .captures
                    .get(&capture)
                    .filter(|capture| capture.owner == incarnation)
                    .ok_or(FdinfoProjectionGap::RejectedUpdate)?;
                owns_source(&self.cache, current.source)?;
                if !matches!(&current.kind,
                    CurrentCaptureKind::Represented { actor: issuing, .. } if *issuing == actor)
                {
                    return Err(FdinfoProjectionGap::RejectedUpdate);
                }
                let current = self
                    .captures
                    .remove(&capture)
                    .expect("validated current capture");
                let CurrentCaptureKind::Represented { token, .. } = current.kind else {
                    unreachable!("validated represented capture");
                };
                let target = self.cache.capture_admitted_slot_object(token, object)?;
                let logical_open_file = self
                    .logical_ids
                    .get(&target.ofd.open_file)
                    .copied()
                    .unwrap_or(target.ofd.open_file);
                Ok(FdinfoControlValue::CapturedCurrent(
                    FdinfoCapturedCurrentAuthority {
                        target: Box::new(target),
                        logical_open_file,
                    },
                ))
            }
            FdinfoControlOperation::CancelCapture { capture } => {
                if !self
                    .captures
                    .get(&capture)
                    .is_some_and(|capture| capture.owner == incarnation)
                {
                    return Err(FdinfoProjectionGap::RejectedUpdate);
                }
                self.captures.remove(&capture);
                Ok(FdinfoControlValue::Cancelled)
            }
        }
    }

    pub(crate) fn has_pending_exec(&self, target: crate::types::DetPid) -> bool {
        self.pending_execs.contains_key(&target)
    }

    /// Called while the scheduler's existing incarnation admission is held.
    pub(crate) fn publish_admitted(
        &mut self,
        incarnation: FdinfoIncarnation,
        publication: &FdinfoPublication,
    ) -> Result<(), FdinfoProjectionError> {
        // Validate the ancillary facts before applying the ordered journal.
        // A rejected wrapper must not leave a usable partially published view.
        let mut labels = HashMap::new();
        for &(key, logical) in &publication.logical_ids {
            if self
                .logical_ids
                .get(&key)
                .is_some_and(|known| *known != logical)
                || labels
                    .insert(key, logical)
                    .is_some_and(|known| known != logical)
            {
                return Err(FdinfoProjectionError::ObjectConflict);
            }
        }
        let mut children = std::collections::HashSet::new();
        for seed in &publication.forks {
            if self.staged_children.contains_key(&seed.child) || !children.insert(seed.child) {
                return Err(FdinfoProjectionError::InvalidAdmission);
            }
        }
        let admission = match self.admitted.get(&incarnation.tid).copied() {
            Some((current, admission)) if current == incarnation => admission,
            Some(_) => return Err(FdinfoProjectionError::StaleAdmission),
            None => {
                let admission = self.cache.register_after_admission(
                    incarnation,
                    FdinfoFilesAdmission {
                        files: publication.files,
                        new_origin: publication.origin,
                    },
                )?;
                self.admitted
                    .insert(incarnation.tid, (incarnation, admission));
                admission
            }
        };
        let mut batch = publication.batch.clone();
        if let Some(own) = &publication.own_identity {
            if let Some(permit) = self.deferred_exec_identities.remove(&incarnation) {
                let previous = self.cache.refresh_deferred_exec_task_binding(
                    admission,
                    permit,
                    own.lifetime,
                )?;
                batch.deltas.push(FdinfoProjectionDelta::BindTask {
                    lifetime: own.lifetime,
                    previous,
                });
            } else {
                match self.cache.admitted_task_lifetime(admission)? {
                    None => batch.deltas.push(FdinfoProjectionDelta::BindTask {
                        lifetime: own.lifetime,
                        previous: None,
                    }),
                    Some(lifetime) if lifetime == own.lifetime => {}
                    Some(_) => return Err(FdinfoProjectionError::TaskChanged),
                }
            }
            for &alias in &own.aliases {
                batch.deltas.push(FdinfoProjectionDelta::BindAlias {
                    alias,
                    lifetime: own.lifetime,
                });
            }
        }
        self.cache.apply(admission, batch)?;
        for &(key, logical) in &publication.logical_ids {
            self.logical_ids.insert(key, logical);
        }
        for seed in &publication.forks {
            self.staged_children
                .insert(seed.child, (incarnation, seed.clone()));
        }
        // The matching Table commit consumed the private reservation. Removing
        // this bookkeeping entry does not cancel that committed ownership.
        if publication
            .origin
            .is_some_and(|origin| matches!(origin, FdinfoTableOrigin::Fork { .. }))
        {
            self.pending_children.remove(&incarnation);
        }
        self.prune_logical_ids();
        Ok(())
    }

    /// The successful parent's admitted CreateChildThread transaction, before
    /// ParentContinue, authenticates the target identity and the source table.
    pub(crate) fn reserve_child_admitted(
        &mut self,
        parent: FdinfoIncarnation,
        child: FdinfoIncarnation,
        seed: FdinfoForkSeed,
    ) -> Result<(), FdinfoProjectionError> {
        if seed.child != child || self.pending_children.contains_key(&child) {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        let admission = self.admission(parent)?;
        let hold = self
            .cache
            .reserve_pending_seed(admission, child, seed.table)?;
        self.pending_children.insert(child, hold);
        Ok(())
    }

    pub(crate) fn reserve_registered_child(
        &mut self,
        parent: FdinfoIncarnation,
        child: FdinfoIncarnation,
    ) -> Result<(), FdinfoProjectionError> {
        let Some((source, seed)) = self.staged_children.remove(&child) else {
            // CLONE_FILES needs no new table. A missing copied-table seed is
            // not reconstructed from a late child snapshot.
            return Ok(());
        };
        if source != parent {
            return Err(FdinfoProjectionError::StaleAdmission);
        }
        self.reserve_child_admitted(parent, child, seed)
    }

    /// Runs in the original PrepareExec transaction under the old MM. The
    /// genuine FilesId is created by the former caller, even for nonleader
    /// exec; the future MM/TID belong to the surviving process leader.
    pub(crate) fn prepare_exec_admitted(
        &mut self,
        actor: FdinfoIncarnation,
        process: DetPid,
        seed: FdinfoExecSeed,
    ) -> Result<(), FdinfoProjectionError> {
        if self.pending_execs.contains_key(&process) {
            return Err(FdinfoProjectionError::InvalidAdmission);
        }
        let target = FdinfoIncarnation {
            tid: process,
            mm: actor.mm.for_exec(process),
        };
        let admission = self.admission(actor)?;
        let table = seed.table;
        let hold = self
            .cache
            .reserve_pending_seed(admission, target, table.clone())?;
        self.pending_execs.insert(
            process,
            PendingExecProjection {
                actor,
                target,
                table,
                cursors: seed.cursors,
                hold,
            },
        );
        Ok(())
    }

    /// Failed exec keeps the original image and cancels only its exact hold.
    pub(crate) fn cancel_exec_admitted(
        &mut self,
        actor: FdinfoIncarnation,
        process: DetPid,
    ) -> Result<(), FdinfoProjectionError> {
        let Some(pending) = self.pending_execs.get(&process) else {
            return Ok(());
        };
        if pending.actor != actor {
            return Err(FdinfoProjectionError::StaleAdmission);
        }
        let pending = self
            .pending_execs
            .remove(&process)
            .expect("checked pending exec");
        self.cache.cancel_pending_seed(pending.hold)
    }

    /// Called only inside the existing authenticated successful exec edge.
    /// Seed the retained clone-cut table before releasing the former owner.
    pub(crate) fn adopt_exec_admitted(
        &mut self,
        former: FdinfoIncarnation,
        target: FdinfoIncarnation,
        own_identity: Option<&FdinfoOwnIdentity>,
    ) -> Result<FdinfoReentryBinding, FdinfoProjectionError> {
        let pending = self
            .pending_execs
            .get(&target.tid)
            .ok_or(FdinfoProjectionError::TableOrigin)?;
        if pending.actor != former || pending.target != target {
            return Err(FdinfoProjectionError::StaleAdmission);
        }
        let previous = self.admission(former)?;
        let table = pending.table.clone();
        let cursors = pending.cursors;
        let admission = self.cache.replace_after_exec(
            previous,
            target,
            FdinfoFilesAdmission {
                files: table.files,
                new_origin: Some(table.origin),
            },
        )?;
        if let Some(own) = own_identity {
            // The authenticated loader reconnect supplies this recorded raw
            // identity independently of its newly constructed local table.
            // Never apply that fresh Initial seed to the retained exec table.
            self.cache
                .refresh_exec_task_binding(admission, own.lifetime)?;
        }
        let deferred = if own_identity.is_none() {
            Some(self.cache.defer_exec_task_binding(admission)?)
        } else {
            None
        };
        let mut deltas = vec![FdinfoProjectionDelta::Table(table)];
        if let Some(own) = own_identity {
            deltas.push(FdinfoProjectionDelta::BindTask {
                lifetime: own.lifetime,
                previous: self
                    .cache
                    .admitted_task_lifetime(admission)?
                    .map(|_| former),
            });
            deltas.extend(
                own.aliases
                    .iter()
                    .map(|&alias| FdinfoProjectionDelta::BindAlias {
                        alias,
                        lifetime: own.lifetime,
                    }),
            );
        }
        self.cache.apply(
            admission,
            FdinfoProjectionBatch {
                sequence: 1,
                deltas,
            },
        )?;
        self.pending_execs.remove(&target.tid);
        self.deferred_exec_identities.remove(&former);
        if let Some(permit) = deferred {
            self.deferred_exec_identities.insert(target, permit);
        }
        self.admitted.remove(&former.tid);
        self.admitted.insert(target.tid, (target, admission));
        let table = self.cache.admitted_table_snapshot(admission)?;
        let logical_ids = table
            .ofds
            .iter()
            .filter_map(|ofd| {
                self.logical_ids
                    .get(&ofd.open_file)
                    .map(|&logical| (ofd.open_file, logical))
            })
            .collect();
        self.prune_logical_ids();
        Ok(FdinfoReentryBinding {
            incarnation: target,
            table,
            cursors,
            logical_ids,
        })
    }

    /// Actual lifecycle retirement, never inferred from a missing proc alias.
    pub(crate) fn retire_admitted(
        &mut self,
        incarnation: FdinfoIncarnation,
    ) -> Result<(), FdinfoProjectionError> {
        self.deferred_exec_identities.remove(&incarnation);
        self.captures
            .retain(|_, capture| capture.owner != incarnation);
        self.staged_children
            .retain(|_, (source, _)| *source != incarnation);
        if let Some(hold) = self.pending_children.remove(&incarnation) {
            self.cache.cancel_pending_seed(hold)?;
        }
        let processes: Vec<_> = self
            .pending_execs
            .iter()
            .filter_map(|(&process, pending)| (pending.actor == incarnation).then_some(process))
            .collect();
        for process in processes {
            self.cancel_exec_admitted(incarnation, process)?;
        }
        let Some((current, admission)) = self.admitted.get(&incarnation.tid).copied() else {
            return Ok(());
        };
        if current != incarnation {
            return Err(FdinfoProjectionError::StaleAdmission);
        }
        self.cache.retire_after_admission(admission)?;
        self.admitted.remove(&incarnation.tid);
        self.prune_logical_ids();
        Ok(())
    }

    fn prune_logical_ids(&mut self) {
        self.logical_ids
            .retain(|key, _| self.cache.contains_ofd(*key));
        let retired: Vec<_> = self
            .opened_subjects
            .keys()
            .copied()
            .filter(|key| !self.cache.contains_ofd(*key))
            .collect();
        for key in retired {
            let subject = self
                .opened_subjects
                .remove(&key)
                .expect("collected proc OFD");
            if let OpenedSubject::Managed(subject) = subject
                && self.cache.release_open_subject(subject).is_err()
            {
                self.cache.invalidate_authority();
            }
            self.captures.retain(|_, capture| capture.source != key);
        }
    }

    fn admission(
        &self,
        incarnation: FdinfoIncarnation,
    ) -> Result<FdinfoAdmission, FdinfoProjectionError> {
        self.admitted
            .get(&incarnation.tid)
            .filter(|(current, _)| *current == incarnation)
            .map(|(_, admitted)| *admitted)
            .ok_or(FdinfoProjectionError::StaleAdmission)
    }
}
