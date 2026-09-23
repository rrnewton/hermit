/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Pure ownership ledger for network OFDs. No host handles or `Arc` counts are
//! guest ownership. The coordinator must authenticate lifecycle events and
//! serialize successful slot mutations at their existing kernel/RPC boundary.
//!
//! Returned retirements are irreversible: every table slot, pending exec
//! reservation and explicit operation/transfer lease is gone. Exiting a task
//! does NOT acknowledge cancellation of its transport or guest-copy operation.
//! Lease release says nothing about stream consumption: TCP copy faults consume
//! zero bytes of the current chunk, whereas a UDP receive error consumes the
//! whole message. The engine commits those semantics separately.
//!
//! This module is staged independently of the adapters. Until their lifecycle
//! hooks publish every owner transition, this ledger does not repair live exec,
//! close, process exit or in-flight operation behavior.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;

use crate::resources::ExternalOpId;
use crate::scheduler::ExecReconnect;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::ExecFilesReceipt;
use crate::types::FdSlot;
use crate::types::FdSlotBinding;
use crate::types::FilesId;
use crate::types::MmId;
use crate::types::NetworkFdSlot;
use crate::types::NetworkFdSlotReplacement;
use crate::types::OpenFileId;
use crate::types::RawFd;

/// Exact task incarnation used by the scheduler's exec and exit protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TaskOwner {
    pub tid: DetTid,
    pub mm: MmId,
}

/// A tracked slot, including its slot-local close-on-exec bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NetworkSlot {
    pub open_file: OpenFileId,
    pub cloexec: bool,
}

/// Pins are distinct from descriptor aliases, and from one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LeaseKind {
    Transfer,
    Transport,
    /// Run-global stream-call allocator; distinct from syscall operation IDs.
    StreamCall,
    /// Run-global descriptor mutation allocator; distinct from stream calls.
    DescriptorMutation,
    Delivery,
}

/// An exact operation pin. `ordinal` distinguishes multiple SCM_RIGHTS objects
/// or concurrent stages of the same syscall; it is not a channel-match key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LeaseId {
    pub operation: ExternalOpId,
    pub mm: MmId,
    pub kind: LeaseKind,
    pub ordinal: u32,
}

/// Cancellation of a handler future is not evidence about an injected syscall.
/// Only these explicit physical facts can retire a transport pin. Unknown
/// effects retain the pin and prevent successful ledger finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportResolution {
    CompletedAndRecorded,
    CancellationAcknowledgedBeforeSubmission,
    UnknownEffects,
}

/// Exactly the global PrepareExec allocation, not a second ledger-local nonce.
type ExecTicket = ExecFilesReceipt;

fn exec_owner(ticket: ExecTicket) -> TaskOwner {
    TaskOwner {
        tid: ticket.caller,
        mm: ticket.mm,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifetimeError {
    TaskAlreadyRegistered(DetTid),
    StaleTask(TaskOwner),
    TableAlreadyUsed(FilesId),
    InvalidDescriptor(RawFd),
    SlotOccupied(RawFd),
    SlotIdentity(RawFd),
    SlotGeneration(FdSlotBinding),
    OpenFileAlreadyUsed(OpenFileId),
    LeaseAlreadyUsed(LeaseId),
    LeaseIdentity(LeaseId),
    ExecAlreadyPrepared(DetPid),
    ExecIdentity,
    TransportAcknowledgementRequired(LeaseId),
    UnresolvedTransport(LeaseId),
    OutstandingOwners,
    PublicationIdentity { files: FilesId, sequence: u64 },
    CloneIdentity,
}

impl fmt::Display for LifetimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network OFD lifetime protocol: {self:?}")
    }
}

impl std::error::Error for LifetimeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskBinding {
    owner: TaskOwner,
    process: DetPid,
    files: FilesId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Table {
    owners: HashSet<TaskOwner>,
    slots: BTreeMap<RawFd, NetworkSlot>,
    slot_generations: BTreeMap<RawFd, u64>,
    last_slot_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedExec {
    ticket: ExecTicket,
    // A provisional snapshot pins its objects but is not an active guest table.
    // The adapter must take this at the serialized exec-unshare boundary.
    slots: BTreeMap<RawFd, NetworkSlot>,
    slot_generations: BTreeMap<RawFd, u64>,
    last_slot_generation: u64,
}

/// Exact pre-physical table reservation. Operation is allocated by the same
/// run-global monotonic mutation allocator as descriptor installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CloneTicket {
    pub owner: TaskOwner,
    pub files: FilesId,
    pub operation: ExternalOpId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedClone {
    ticket: CloneTicket,
    shared: bool,
    table: Table,
}

/// Source authorization is supplied by the engine's matched physical-effect
/// receipt, not trusted from an arbitrary caller's journal payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlotInstallationSource {
    Fresh,
    Alias(FdSlotBinding),
    Transfer(LeaseId),
    NonNetwork,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotPublicationEntry {
    pub replacement: NetworkFdSlotReplacement,
    pub source: SlotInstallationSource,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotPublicationBatch {
    pub files: FilesId,
    pub sequence: u64,
    pub previous_generation: u64,
    pub through_generation: u64,
    pub entries: Vec<SlotPublicationEntry>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlotPublicationResult {
    Applied { retired: BTreeSet<OpenFileId> },
    // Exact duplicate is acknowledged, but irreversible retirement is not emitted twice.
    AlreadyApplied,
}

/// Counts semantic owners, never implementation references.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OwnerCounts {
    pub slots: usize,
    pub exec_reservations: usize,
    pub clone_reservations: usize,
    pub transfers: usize,
    pub transports: usize,
    pub deliveries: usize,
}

impl OwnerCounts {
    fn is_zero(self) -> bool {
        self == Self::default()
    }
}

/// The coordinator owns one ledger for the run. Hash maps are lookup-only;
/// every observable retirement set is canonically ordered by OpenFileId.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NetworkLifetime {
    tasks: BTreeMap<DetTid, TaskBinding>,
    tables: HashMap<FilesId, Table>,
    used_tables: HashSet<FilesId>,
    live: BTreeSet<OpenFileId>,
    retired: BTreeSet<OpenFileId>,
    leases: HashMap<LeaseId, OpenFileId>,
    used_leases: HashSet<LeaseId>,
    pending_exec: BTreeMap<DetPid, PreparedExec>,
    pending_clones: BTreeMap<ExternalOpId, PreparedClone>,
    used_clones: HashSet<ExternalOpId>,
    used_exec_allocations: HashSet<FilesId>,
    // Exact installations superseded by an authenticated mutation. An old
    // close completion may acknowledge its own removal without touching reuse.
    removed_slots: HashSet<FdSlotBinding>,
    publication_sequences: HashMap<FilesId, u64>,
    published_batches: HashMap<(FilesId, u64), SlotPublicationBatch>,
}

impl NetworkLifetime {
    /// Apply one exact ordered prefix under the global engine mutex. The caller
    /// must also hold the table's shared publication owner through local ACK.
    /// Failed validation changes nothing, including object and lease ownership.
    pub fn publish_installation_batch(
        &mut self,
        owner: TaskOwner,
        batch: &SlotPublicationBatch,
    ) -> Result<SlotPublicationResult, LifetimeError> {
        let files = self.task(owner)?.files;
        let error = || LifetimeError::PublicationIdentity {
            files: batch.files,
            sequence: batch.sequence,
        };
        if files != batch.files || batch.sequence == 0 {
            return Err(error());
        }
        if let Some(previous) = self.published_batches.get(&(files, batch.sequence)) {
            return if previous == batch {
                Ok(SlotPublicationResult::AlreadyApplied)
            } else {
                Err(error())
            };
        }
        let expected_sequence = self
            .publication_sequences
            .get(&files)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(error)?;
        if batch.sequence != expected_sequence
            || batch.previous_generation != self.tables[&files].last_slot_generation
            || batch.through_generation <= batch.previous_generation
        {
            return Err(error());
        }
        let mut candidate = self.clone();
        let mut generation = batch.previous_generation;
        for entry in &batch.entries {
            let change = entry.replacement;
            if change.files != files
                || change.installation_generation <= generation
                || change.installation_generation > batch.through_generation
                || (change.before.is_none() && change.after.is_none())
            {
                return Err(error());
            }
            let slot = change.after.or(change.before).ok_or_else(error)?;
            let fd = slot.binding.slot.fd;
            if fd < 0 {
                return Err(LifetimeError::InvalidDescriptor(fd));
            }
            for value in [change.before, change.after].into_iter().flatten() {
                if value.binding.slot != (FdSlot { files, fd }) || value.binding.generation == 0 {
                    return Err(error());
                }
            }
            if let Some(after) = change.after {
                if after.binding.generation != change.installation_generation {
                    return Err(error());
                }
            }
            if let Some(before) = change.before {
                if before.binding.generation >= change.installation_generation {
                    return Err(error());
                }
            }
            let actual = candidate.binding_in_table(files, fd);
            let expected = change.before.map(|value| value.binding);
            let removed_exact = actual.is_none()
                && expected.is_some_and(|binding| candidate.removed_slots.contains(&binding));
            if actual != expected && !removed_exact {
                return Err(LifetimeError::SlotIdentity(fd));
            }
            if actual.is_some()
                && candidate.tables[&files].slots[&fd].cloexec != change.before.unwrap().cloexec
            {
                return Err(LifetimeError::SlotIdentity(fd));
            }
            match (&entry.source, change.after) {
                (SlotInstallationSource::Fresh, Some(after)) => {
                    let object = after.binding.open_file;
                    if candidate.live.contains(&object) || candidate.retired.contains(&object) {
                        return Err(LifetimeError::OpenFileAlreadyUsed(object));
                    }
                    candidate.live.insert(object);
                }
                (SlotInstallationSource::Alias(source), Some(after)) => {
                    candidate.validate_binding(owner, *source)?;
                    if source.open_file != after.binding.open_file {
                        return Err(LifetimeError::SlotIdentity(source.slot.fd));
                    }
                }
                (SlotInstallationSource::Transfer(lease), Some(after)) => {
                    candidate.check_lease(*lease, after.binding.open_file)?;
                    if lease.kind != LeaseKind::Transfer {
                        return Err(LifetimeError::LeaseIdentity(*lease));
                    }
                    // Receiver entitlement must already match the queued delivery
                    // receipt in the engine; possession of a lease name is insufficient.
                    candidate.leases.remove(lease);
                }
                (SlotInstallationSource::NonNetwork, None) if change.before.is_some() => {}
                _ => return Err(error()),
            }
            candidate.note_removed(files, fd);
            let table = candidate.tables.get_mut(&files).expect("registered table");
            table.slots.remove(&fd);
            table.slot_generations.remove(&fd);
            // Advance even when a regular-file replacement has no network after-slot.
            table.last_slot_generation = change.installation_generation;
            if let Some(after) = change.after {
                candidate.install_binding(after);
            }
            generation = change.installation_generation;
        }
        // Non-network-only installations between recorded network transitions
        // still consume installation generations in the local table snapshot.
        candidate
            .tables
            .get_mut(&files)
            .unwrap()
            .last_slot_generation = batch.through_generation;
        candidate
            .publication_sequences
            .insert(files, batch.sequence);
        candidate
            .published_batches
            .insert((files, batch.sequence), batch.clone());
        let retired = candidate.collect_retired();
        *self = candidate;
        Ok(SlotPublicationResult::Applied { retired })
    }

    fn task(&self, owner: TaskOwner) -> Result<&TaskBinding, LifetimeError> {
        self.tasks
            .get(&owner.tid)
            .filter(|binding| binding.owner == owner)
            .ok_or(LifetimeError::StaleTask(owner))
    }

    fn slot(
        &self,
        owner: TaskOwner,
        fd: RawFd,
        expected: OpenFileId,
    ) -> Result<(FilesId, NetworkSlot), LifetimeError> {
        let files = self.task(owner)?.files;
        let slot = self.tables[&files]
            .slots
            .get(&fd)
            .filter(|slot| slot.open_file == expected)
            .copied()
            .ok_or(LifetimeError::SlotIdentity(fd))?;
        Ok((files, slot))
    }

    fn new_task(&self, owner: TaskOwner) -> Result<(), LifetimeError> {
        if self.tasks.contains_key(&owner.tid) {
            return Err(LifetimeError::TaskAlreadyRegistered(owner.tid));
        }
        Ok(())
    }

    fn new_table(&self, files: FilesId) -> Result<(), LifetimeError> {
        if self.used_tables.contains(&files)
            || self
                .pending_exec
                .values()
                .any(|p| p.ticket.new_files == files)
        {
            return Err(LifetimeError::TableAlreadyUsed(files));
        }
        Ok(())
    }

    /// Pin the exact copied table or shared-table identity before kernel clone.
    pub fn prepare_clone(
        &mut self,
        ticket: CloneTicket,
        shared: bool,
    ) -> Result<(), LifetimeError> {
        let task = self.task(ticket.owner)?;
        if task.files != ticket.files
            || ticket.operation.tid != ticket.owner.tid
            || self.used_clones.contains(&ticket.operation)
            || self
                .pending_clones
                .values()
                .any(|p| p.ticket.owner == ticket.owner)
        {
            return Err(LifetimeError::CloneIdentity);
        }
        let table = self.tables[&task.files].clone();
        self.used_clones.insert(ticket.operation);
        assert!(
            self.pending_clones
                .insert(
                    ticket.operation,
                    PreparedClone {
                        ticket,
                        shared,
                        table
                    }
                )
                .is_none()
        );
        Ok(())
    }

    fn prepared_clone(&self, ticket: CloneTicket) -> Result<&PreparedClone, LifetimeError> {
        self.pending_clones
            .get(&ticket.operation)
            .filter(|p| p.ticket == ticket)
            .ok_or(LifetimeError::CloneIdentity)
    }

    /// Known kernel error only. A parent/handler disappearing is not this proof.
    pub fn cancel_clone(
        &mut self,
        ticket: CloneTicket,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let shared = self.prepared_clone(ticket)?.shared;
        self.pending_clones.remove(&ticket.operation).unwrap();
        if shared
            && self
                .tables
                .get(&ticket.files)
                .is_some_and(|t| t.owners.is_empty())
        {
            self.tables.remove(&ticket.files);
        }
        Ok(self.collect_retired())
    }

    /// Actual authenticated child registration consumes the pre-clone snapshot.
    /// It is valid before parent return, as required for vfork progress.
    pub fn commit_clone(
        &mut self,
        ticket: CloneTicket,
        child: TaskOwner,
        process: DetPid,
    ) -> Result<FilesId, LifetimeError> {
        let pending = self.prepared_clone(ticket)?;
        self.new_task(child)?;
        let files = if pending.shared {
            ticket.files
        } else {
            FilesId::forked(child.tid)
        };
        if pending.shared {
            if !self.tables.contains_key(&files) {
                return Err(LifetimeError::CloneIdentity);
            }
        } else {
            self.new_table(files)?;
        }
        let pending = self.pending_clones.remove(&ticket.operation).unwrap();
        if pending.shared {
            assert!(self.tables.get_mut(&files).unwrap().owners.insert(child));
            assert!(
                self.tasks
                    .insert(
                        child.tid,
                        TaskBinding {
                            owner: child,
                            process,
                            files
                        }
                    )
                    .is_none()
            );
        } else {
            self.insert_table(
                child,
                process,
                files,
                pending.table.slots,
                pending.table.slot_generations,
                pending.table.last_slot_generation,
            );
        }
        Ok(files)
    }

    /// Publish one actual initial table. Inherited slots are installed through
    /// explicit open/transfer operations; this does not discover host fds.
    pub fn register(
        &mut self,
        owner: TaskOwner,
        process: DetPid,
        files: FilesId,
    ) -> Result<(), LifetimeError> {
        self.new_task(owner)?;
        self.new_table(files)?;
        self.insert_table(owner, process, files, BTreeMap::new(), BTreeMap::new(), 0);
        Ok(())
    }

    fn insert_table(
        &mut self,
        owner: TaskOwner,
        process: DetPid,
        files: FilesId,
        slots: BTreeMap<RawFd, NetworkSlot>,
        slot_generations: BTreeMap<RawFd, u64>,
        last_slot_generation: u64,
    ) {
        assert!(self.used_tables.insert(files));
        assert!(
            self.tables
                .insert(
                    files,
                    Table {
                        owners: HashSet::from([owner]),
                        slots,
                        slot_generations,
                        last_slot_generation,
                    }
                )
                .is_none()
        );
        assert!(
            self.tasks
                .insert(
                    owner.tid,
                    TaskBinding {
                        owner,
                        process,
                        files
                    }
                )
                .is_none()
        );
    }

    /// CLONE_FILES adds a task owner, not another copy of the table's slots.
    pub fn share_table(
        &mut self,
        parent: TaskOwner,
        child: TaskOwner,
        process: DetPid,
    ) -> Result<(), LifetimeError> {
        let files = self.task(parent)?.files;
        self.new_task(child)?;
        assert!(self.tables.get_mut(&files).unwrap().owners.insert(child));
        self.tasks.insert(
            child.tid,
            TaskBinding {
                owner: child,
                process,
                files,
            },
        );
        Ok(())
    }

    /// Fork copies slots for a new task while preserving OFD identity.
    pub fn copy_table(
        &mut self,
        parent: TaskOwner,
        child: TaskOwner,
        process: DetPid,
        files: FilesId,
    ) -> Result<(), LifetimeError> {
        let old = self.task(parent)?.files;
        self.new_task(child)?;
        self.new_table(files)?;
        let table = &self.tables[&old];
        let slots = table.slots.clone();
        let generations = table.slot_generations.clone();
        let last = table.last_slot_generation;
        self.insert_table(child, process, files, slots, generations, last);
        Ok(())
    }

    /// Publish a fresh OFD only after successful kernel creation.
    pub fn open(
        &mut self,
        owner: TaskOwner,
        fd: RawFd,
        slot: NetworkSlot,
    ) -> Result<(), LifetimeError> {
        let files = self.task(owner)?.files;
        if fd < 0 {
            return Err(LifetimeError::InvalidDescriptor(fd));
        }
        if self.tables[&files].slots.contains_key(&fd) {
            return Err(LifetimeError::SlotOccupied(fd));
        }
        if self.live.contains(&slot.open_file) || self.retired.contains(&slot.open_file) {
            return Err(LifetimeError::OpenFileAlreadyUsed(slot.open_file));
        }
        self.live.insert(slot.open_file);
        self.install_generated(files, fd, slot);
        Ok(())
    }

    pub fn set_cloexec(
        &mut self,
        owner: TaskOwner,
        fd: RawFd,
        expected: OpenFileId,
        cloexec: bool,
    ) -> Result<(), LifetimeError> {
        let (files, _) = self.slot(owner, fd, expected)?;
        self.tables
            .get_mut(&files)
            .unwrap()
            .slots
            .get_mut(&fd)
            .unwrap()
            .cloexec = cloexec;
        Ok(())
    }

    /// A successful dup2(fd, fd) is a no-op, including FD_CLOEXEC. Callers must
    /// not publish failed dup/dup3 syscalls as successful mutations.
    pub fn duplicate(
        &mut self,
        owner: TaskOwner,
        oldfd: RawFd,
        expected: OpenFileId,
        newfd: RawFd,
        cloexec: bool,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let (files, old) = self.slot(owner, oldfd, expected)?;
        if newfd < 0 {
            return Err(LifetimeError::InvalidDescriptor(newfd));
        }
        if oldfd == newfd {
            return Ok(BTreeSet::new());
        }
        self.note_removed(files, newfd);
        self.install_generated(
            files,
            newfd,
            NetworkSlot {
                open_file: old.open_file,
                cloexec,
            },
        );
        Ok(self.collect_retired())
    }

    pub fn close(
        &mut self,
        owner: TaskOwner,
        fd: RawFd,
        expected: OpenFileId,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let (files, _) = self.slot(owner, fd, expected)?;
        self.note_removed(files, fd);
        let table = self.tables.get_mut(&files).unwrap();
        table.slots.remove(&fd);
        table.slot_generations.remove(&fd);
        Ok(self.collect_retired())
    }

    fn binding_in_table(&self, files: FilesId, fd: RawFd) -> Option<FdSlotBinding> {
        let table = self.tables.get(&files)?;
        let slot = table.slots.get(&fd)?;
        Some(FdSlotBinding {
            slot: FdSlot { files, fd },
            generation: *table
                .slot_generations
                .get(&fd)
                .expect("registered slot generation"),
            open_file: slot.open_file,
        })
    }

    pub fn descriptor_binding(
        &self,
        owner: TaskOwner,
        fd: RawFd,
    ) -> Result<FdSlotBinding, LifetimeError> {
        let files = self.task(owner)?.files;
        self.binding_in_table(files, fd)
            .ok_or(LifetimeError::SlotIdentity(fd))
    }

    fn validate_binding(
        &self,
        owner: TaskOwner,
        binding: FdSlotBinding,
    ) -> Result<(), LifetimeError> {
        let files = self.task(owner)?.files;
        if files != binding.slot.files
            || self.binding_in_table(files, binding.slot.fd) != Some(binding)
        {
            return Err(LifetimeError::SlotIdentity(binding.slot.fd));
        }
        Ok(())
    }

    fn note_removed(&mut self, files: FilesId, fd: RawFd) {
        if let Some(binding) = self.binding_in_table(files, fd) {
            self.removed_slots.insert(binding);
        }
    }

    // Existing transition entry points also allocate incarnations, so tests of
    // clone/exec/transfer exercise the same table state as authenticated adapters.
    fn install_generated(&mut self, files: FilesId, fd: RawFd, slot: NetworkSlot) {
        let table = self.tables.get_mut(&files).expect("registered table");
        let generation = table
            .last_slot_generation
            .checked_add(1)
            .expect("network descriptor slot generation exhausted");
        table.last_slot_generation = generation;
        table.slot_generations.insert(fd, generation);
        table.slots.insert(fd, slot);
    }

    fn validate_install(
        &self,
        owner: TaskOwner,
        slot: NetworkFdSlot,
        replaced: Option<FdSlotBinding>,
    ) -> Result<(), LifetimeError> {
        let files = self.task(owner)?.files;
        let binding = slot.binding;
        if binding.slot.fd < 0 {
            return Err(LifetimeError::InvalidDescriptor(binding.slot.fd));
        }
        if files != binding.slot.files
            || binding.generation == 0
            || binding.generation <= self.tables[&files].last_slot_generation
        {
            return Err(LifetimeError::SlotGeneration(binding));
        }
        if self.binding_in_table(files, binding.slot.fd) != replaced {
            return Err(LifetimeError::SlotIdentity(binding.slot.fd));
        }
        Ok(())
    }

    fn install_binding(&mut self, slot: NetworkFdSlot) {
        let binding = slot.binding;
        self.note_removed(binding.slot.files, binding.slot.fd);
        let table = self
            .tables
            .get_mut(&binding.slot.files)
            .expect("registered table");
        table.last_slot_generation = binding.generation;
        table
            .slot_generations
            .insert(binding.slot.fd, binding.generation);
        table.slots.insert(
            binding.slot.fd,
            NetworkSlot {
                open_file: binding.open_file,
                cloexec: slot.cloexec,
            },
        );
    }

    /// Publish an actual successful socket creation under an authenticated
    /// table mutation receipt. A stale modeled slot may have been removed by a
    /// still-pending close; replacement names that exact incarnation.
    pub fn publish_created_slot(
        &mut self,
        owner: TaskOwner,
        slot: NetworkFdSlot,
        replaced: Option<FdSlotBinding>,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        self.validate_install(owner, slot, replaced)?;
        let object = slot.binding.open_file;
        if self.live.contains(&object) || self.retired.contains(&object) {
            return Err(LifetimeError::OpenFileAlreadyUsed(object));
        }
        self.live.insert(object);
        self.install_binding(slot);
        Ok(self.collect_retired())
    }

    /// Publish successful dup/F_DUPFD while the short atomic source/target
    /// admission remains held. Failed replacement must never reach this call.
    pub fn publish_duplicated_slot(
        &mut self,
        owner: TaskOwner,
        source: FdSlotBinding,
        destination: NetworkFdSlot,
        replaced: Option<FdSlotBinding>,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        self.validate_binding(owner, source)?;
        if source.slot == destination.binding.slot {
            if destination.binding != source
                || replaced != Some(source)
                || destination.cloexec
                    != self.tables[&source.slot.files].slots[&source.slot.fd].cloexec
            {
                return Err(LifetimeError::SlotIdentity(source.slot.fd));
            }
            return Ok(BTreeSet::new());
        }
        self.validate_install(owner, destination, replaced)?;
        if source.open_file != destination.binding.open_file {
            return Err(LifetimeError::SlotIdentity(source.slot.fd));
        }
        self.install_binding(destination);
        Ok(self.collect_retired())
    }

    /// Reconcile an authenticated descriptor-removal result. A completed older
    /// operation may name an already-superseded installation, never its numeric
    /// replacement. Operation receipts still enforce exactly-once completion.
    pub fn close_binding(
        &mut self,
        owner: TaskOwner,
        binding: FdSlotBinding,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        if self.task(owner)?.files != binding.slot.files {
            return Err(LifetimeError::SlotIdentity(binding.slot.fd));
        }
        if self.binding_in_table(binding.slot.files, binding.slot.fd) != Some(binding) {
            if self.removed_slots.contains(&binding) {
                return Ok(BTreeSet::new());
            }
            return Err(LifetimeError::SlotIdentity(binding.slot.fd));
        }
        self.close(owner, binding.slot.fd, binding.open_file)
    }

    pub fn set_cloexec_binding(
        &mut self,
        owner: TaskOwner,
        binding: FdSlotBinding,
        cloexec: bool,
    ) -> Result<(), LifetimeError> {
        self.validate_binding(owner, binding)?;
        self.set_cloexec(owner, binding.slot.fd, binding.open_file, cloexec)
    }

    /// Exact registered task/table authority, independent of host metadata Arcs.
    /// The engine calls this only after the matching local ACK is authenticated.
    /// Sequence/generation high-water remains; completed full payloads do not.
    pub fn acknowledge_publication_batch(
        &mut self,
        owner: TaskOwner,
        sequence: u64,
        generation: u64,
    ) -> Result<(), LifetimeError> {
        let files = self.task(owner)?.files;
        let current = self.publication_cursor(owner)?;
        if current != (sequence, generation)
            || self
                .published_batches
                .get(&(files, sequence))
                .is_none_or(|batch| batch.through_generation != generation)
        {
            return Err(LifetimeError::PublicationIdentity { files, sequence });
        }
        self.published_batches
            .retain(|(table, completed), _| *table != files || *completed > sequence);
        Ok(())
    }

    #[cfg(test)]
    pub fn pending_publication_payloads_for_test(&self) -> usize {
        self.published_batches.len()
    }

    pub fn publication_cursor(&self, owner: TaskOwner) -> Result<(u64, u64), LifetimeError> {
        let files = self.task(owner)?.files;
        Ok((
            self.publication_sequences.get(&files).copied().unwrap_or(0),
            self.tables[&files].last_slot_generation,
        ))
    }

    pub fn task_files(&self, owner: TaskOwner) -> Result<FilesId, LifetimeError> {
        Ok(self.task(owner)?.files)
    }

    /// Retain the specific installation admitted before a physical operation.
    pub fn retain_binding(
        &mut self,
        owner: TaskOwner,
        binding: FdSlotBinding,
        lease: LeaseId,
    ) -> Result<(), LifetimeError> {
        self.validate_binding(owner, binding)?;
        self.retain_slot(owner, binding.slot.fd, binding.open_file, lease)
    }

    fn new_lease(&self, owner: TaskOwner, lease: LeaseId) -> Result<(), LifetimeError> {
        self.task(owner)?;
        if lease.operation.tid != owner.tid || lease.mm != owner.mm {
            return Err(LifetimeError::LeaseIdentity(lease));
        }
        if self.used_leases.contains(&lease) {
            return Err(LifetimeError::LeaseAlreadyUsed(lease));
        }
        Ok(())
    }

    pub fn retain_slot(
        &mut self,
        owner: TaskOwner,
        fd: RawFd,
        expected: OpenFileId,
        lease: LeaseId,
    ) -> Result<(), LifetimeError> {
        self.slot(owner, fd, expected)?;
        self.new_lease(owner, lease)?;
        self.used_leases.insert(lease);
        self.leases.insert(lease, expected);
        Ok(())
    }

    /// Resolve only this authenticated task's live table, in numeric slot order.
    pub fn binding_for_open_file(
        &self,
        owner: TaskOwner,
        object: OpenFileId,
    ) -> Result<FdSlotBinding, LifetimeError> {
        let files = self.task(owner)?.files;
        let fd = self.tables[&files]
            .slots
            .iter()
            .find_map(|(&fd, slot)| (slot.open_file == object).then_some(fd))
            .ok_or(LifetimeError::SlotIdentity(-1))?;
        self.descriptor_binding(owner, fd)
    }

    /// Retain an exact queued/captured object for a receiving task or a new
    /// operation stage; numeric fd reuse cannot substitute another object.
    pub fn retain_lease(
        &mut self,
        source: LeaseId,
        expected: OpenFileId,
        owner: TaskOwner,
        lease: LeaseId,
    ) -> Result<(), LifetimeError> {
        self.check_lease(source, expected)?;
        self.new_lease(owner, lease)?;
        self.used_leases.insert(lease);
        self.leases.insert(lease, expected);
        Ok(())
    }

    fn check_lease(&self, lease: LeaseId, expected: OpenFileId) -> Result<(), LifetimeError> {
        if self.leases.get(&lease) != Some(&expected) {
            return Err(LifetimeError::LeaseIdentity(lease));
        }
        Ok(())
    }

    /// Install an SCM_RIGHTS/captured object and consume its transfer lease
    /// atomically. Transport/delivery completion is a separate operation.
    pub fn install_transfer(
        &mut self,
        owner: TaskOwner,
        fd: RawFd,
        expected: OpenFileId,
        lease: LeaseId,
        cloexec: bool,
    ) -> Result<(), LifetimeError> {
        let files = self.task(owner)?.files;
        self.check_lease(lease, expected)?;
        if lease.kind != LeaseKind::Transfer {
            return Err(LifetimeError::LeaseIdentity(lease));
        }
        if fd < 0 {
            return Err(LifetimeError::InvalidDescriptor(fd));
        }
        if self.tables[&files].slots.contains_key(&fd) {
            return Err(LifetimeError::SlotOccupied(fd));
        }
        self.install_generated(
            files,
            fd,
            NetworkSlot {
                open_file: expected,
                cloexec,
            },
        );
        self.leases.remove(&lease);
        Ok(())
    }

    /// Requires actual completion/cancellation acknowledgement. It deliberately
    /// works after the initiating task exits, and never consumes network data.
    pub fn release_lease(
        &mut self,
        lease: LeaseId,
        expected: OpenFileId,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        self.check_lease(lease, expected)?;
        if matches!(
            lease.kind,
            LeaseKind::Transport | LeaseKind::StreamCall | LeaseKind::DescriptorMutation
        ) {
            return Err(LifetimeError::TransportAcknowledgementRequired(lease));
        }
        self.leases.remove(&lease);
        Ok(self.collect_retired())
    }

    /// Called only by the actual transport completion/cancellation boundary.
    /// A dropped callback or unresolved kernel outcome remains a live owner.
    pub fn acknowledge_transport(
        &mut self,
        lease: LeaseId,
        expected: OpenFileId,
        resolution: TransportResolution,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        self.check_lease(lease, expected)?;
        if !matches!(
            lease.kind,
            LeaseKind::Transport | LeaseKind::StreamCall | LeaseKind::DescriptorMutation
        ) {
            return Err(LifetimeError::LeaseIdentity(lease));
        }
        if resolution == TransportResolution::UnknownEffects {
            return Err(LifetimeError::UnresolvedTransport(lease));
        }
        self.leases.remove(&lease);
        Ok(self.collect_retired())
    }

    /// No implicit sweep of operation leases at run completion. In particular,
    /// a handler cancelled with unknown physical effects cannot become success.
    pub fn finish(&self) -> Result<(), LifetimeError> {
        if self.tasks.is_empty()
            && self.pending_exec.is_empty()
            && self.pending_clones.is_empty()
            && self.leases.is_empty()
            && self.live.is_empty()
        {
            Ok(())
        } else {
            Err(LifetimeError::OutstandingOwners)
        }
    }

    /// No active owner, slot, binding or flag changes on preparation. The
    /// provisional table reserves its exact objects until completion/cancel.
    pub fn prepare_exec(&mut self, ticket: ExecTicket) -> Result<ExecTicket, LifetimeError> {
        let task = self.task(exec_owner(ticket))?.clone();
        if task.process != ticket.process || task.files != ticket.old_files {
            return Err(LifetimeError::ExecIdentity);
        }
        if self.pending_exec.contains_key(&task.process) {
            return Err(LifetimeError::ExecAlreadyPrepared(task.process));
        }
        self.new_table(ticket.new_files)?;
        if self.used_exec_allocations.contains(&ticket.new_files) {
            return Err(LifetimeError::TableAlreadyUsed(ticket.new_files));
        }
        let table = &self.tables[&task.files];
        let slots = table.slots.clone();
        let slot_generations = table.slot_generations.clone();
        let last_slot_generation = table.last_slot_generation;
        self.used_exec_allocations.insert(ticket.new_files);
        self.pending_exec.insert(
            task.process,
            PreparedExec {
                ticket,
                slots,
                slot_generations,
                last_slot_generation,
            },
        );
        Ok(ticket)
    }

    fn prepared(&self, ticket: ExecTicket) -> Result<&PreparedExec, LifetimeError> {
        self.task(exec_owner(ticket))?;
        self.pending_exec
            .get(&ticket.process)
            .filter(|pending| pending.ticket == ticket)
            .ok_or(LifetimeError::ExecIdentity)
    }

    /// Cancellation changes no active slots. A close by another real table
    /// owner during preparation can make reservation release the final owner.
    pub fn cancel_exec(
        &mut self,
        ticket: ExecTicket,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        self.prepared(ticket)?;
        self.pending_exec.remove(&ticket.process);
        Ok(self.collect_retired())
    }

    /// Consume the existing authenticated successful-exec identity, including
    /// non-leader-to-leader TID reassignment. This is not a success detector.
    pub fn commit_exec(
        &mut self,
        ticket: ExecTicket,
        reconnect: &ExecReconnect,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let prepared = self.prepared(ticket)?;
        if reconnect.caller != ticket.caller
            || reconnect.detpid != ticket.process
            || reconnect.new_leader != ticket.process
            || reconnect.pre_exec_mm != ticket.mm
            || reconnect.post_exec_mm != ticket.mm.for_exec(ticket.process)
            || self.task(exec_owner(ticket))?.files != ticket.old_files
            || self.used_tables.contains(&ticket.new_files)
            || self
                .tasks
                .get(&reconnect.new_leader)
                .is_some_and(|task| task.process != ticket.process)
        {
            return Err(LifetimeError::ExecIdentity);
        }
        let survivors = prepared
            .slots
            .iter()
            .filter_map(|(&fd, &slot)| (!slot.cloexec).then_some((fd, slot)))
            .collect();
        let survivor_generations = prepared
            .slot_generations
            .iter()
            .filter_map(|(&fd, &generation)| {
                (!prepared.slots[&fd].cloexec).then_some((fd, generation))
            })
            .collect();
        let last_slot_generation = prepared.last_slot_generation;
        let removed: Vec<_> = self
            .tasks
            .values()
            .filter(|task| task.process == ticket.process)
            .map(|task| task.owner)
            .collect();
        // All validation precedes mutation. Retirement is collected only after
        // the surviving table is installed, never in the intermediate state.
        for owner in removed {
            self.detach(owner);
        }
        self.pending_exec.remove(&ticket.process);
        self.insert_table(
            TaskOwner {
                tid: reconnect.new_leader,
                mm: reconnect.post_exec_mm,
            },
            ticket.process,
            ticket.new_files,
            survivors,
            survivor_generations,
            last_slot_generation,
        );
        Ok(self.collect_retired())
    }

    fn detach(&mut self, owner: TaskOwner) {
        let task = self.tasks.remove(&owner.tid).expect("validated task");
        assert_eq!(task.owner, owner);
        let table = self.tables.get_mut(&task.files).unwrap();
        assert!(table.owners.remove(&owner));
        if table.owners.is_empty()
            && !self
                .pending_clones
                .values()
                .any(|pending| pending.shared && pending.ticket.files == task.files)
        {
            self.tables.remove(&task.files);
        }
    }

    /// Actual exit detaches one owner even when host metadata snapshots linger.
    /// In-flight leases survive until their distinct cancellation/completion.
    pub fn exit(&mut self, owner: TaskOwner) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let process = self.task(owner)?.process;
        self.detach(owner);
        if self
            .pending_exec
            .get(&process)
            .is_some_and(|pending| exec_owner(pending.ticket) == owner)
        {
            self.pending_exec.remove(&process);
        }
        Ok(self.collect_retired())
    }

    pub fn table_exists(&self, files: FilesId) -> bool {
        self.tables.contains_key(&files)
    }

    /// A dead, never-reusable table has no successor which needs its completed
    /// publication payload. Unknown physical operations remain engine-owned.
    pub fn prune_dead_table_publication(&mut self, files: FilesId) -> Result<(), LifetimeError> {
        if self.tables.contains_key(&files) {
            return Err(LifetimeError::TableAlreadyUsed(files));
        }
        self.published_batches
            .retain(|(table, _), _| *table != files);
        self.publication_sequences.remove(&files);
        self.removed_slots
            .retain(|binding| binding.slot.files != files);
        Ok(())
    }

    pub fn counts(&self, open_file: OpenFileId) -> OwnerCounts {
        let slots = self
            .tables
            .values()
            .flat_map(|table| table.slots.values())
            .filter(|slot| slot.open_file == open_file)
            .count();
        let exec_reservations = self
            .pending_exec
            .values()
            .flat_map(|pending| pending.slots.values())
            .filter(|slot| slot.open_file == open_file)
            .count();
        let clone_reservations = self
            .pending_clones
            .values()
            .filter(|pending| !pending.shared)
            .flat_map(|pending| pending.table.slots.values())
            .filter(|slot| slot.open_file == open_file)
            .count();
        let mut counts = OwnerCounts {
            slots,
            exec_reservations,
            clone_reservations,
            ..OwnerCounts::default()
        };
        for (lease, &object) in &self.leases {
            if object == open_file {
                match lease.kind {
                    LeaseKind::Transfer => counts.transfers += 1,
                    LeaseKind::Transport
                    | LeaseKind::StreamCall
                    | LeaseKind::DescriptorMutation => counts.transports += 1,
                    LeaseKind::Delivery => counts.deliveries += 1,
                }
            }
        }
        counts
    }

    pub fn is_retired(&self, open_file: OpenFileId) -> bool {
        self.retired.contains(&open_file)
    }

    fn collect_retired(&mut self) -> BTreeSet<OpenFileId> {
        let retired: BTreeSet<_> = self
            .live
            .iter()
            .copied()
            .filter(|id| self.counts(*id).is_zero())
            .collect();
        for id in &retired {
            assert!(self.live.remove(id));
            assert!(self.retired.insert(*id));
        }
        retired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(owner: TaskOwner, fd: RawFd, generation: u64, sequence: u64) -> NetworkFdSlot {
        NetworkFdSlot {
            binding: FdSlotBinding {
                slot: FdSlot {
                    files: FilesId::initial(owner.tid),
                    fd,
                },
                generation,
                open_file: object(sequence),
            },
            cloexec: false,
        }
    }

    #[test]
    fn authenticated_install_and_delayed_close_preserve_same_object_aba() {
        let (mut state, owner) = setup();
        let original = installed(owner, 3, 4, 0);
        state.publish_created_slot(owner, original, None).unwrap();
        let alias = installed(owner, 4, 7, 0);
        state
            .publish_duplicated_slot(owner, original.binding, alias, None)
            .unwrap();
        let replacement = installed(owner, 3, 9, 0);
        state
            .publish_duplicated_slot(owner, alias.binding, replacement, Some(original.binding))
            .unwrap();
        assert!(
            state
                .close_binding(owner, original.binding)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            state.descriptor_binding(owner, 3).unwrap(),
            replacement.binding
        );
        assert_eq!(state.counts(object(0)).slots, 2);
        let before = state.clone();
        assert!(matches!(
            state.set_cloexec_binding(owner, original.binding, true),
            Err(LifetimeError::SlotIdentity(3))
        ));
        assert_eq!(state, before);
    }

    #[test]
    fn malformed_or_reused_installation_receipt_changes_nothing() {
        let (mut state, owner) = setup();
        let original = installed(owner, 3, 4, 0);
        state.publish_created_slot(owner, original, None).unwrap();
        for candidate in [installed(owner, 4, 0, 1), installed(owner, 4, 4, 1)] {
            let before = state.clone();
            assert!(matches!(
                state.publish_created_slot(owner, candidate, None),
                Err(LifetimeError::SlotGeneration(_))
            ));
            assert_eq!(state, before);
        }
        let replacement = installed(owner, 3, 5, 1);
        let before = state.clone();
        assert_eq!(
            state.publish_created_slot(owner, replacement, None),
            Err(LifetimeError::SlotIdentity(3))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn fresh_reuse_keeps_an_active_transport_and_never_removes_replacement() {
        let (mut state, owner) = setup();
        let original = installed(owner, 3, 4, 0);
        state.publish_created_slot(owner, original, None).unwrap();
        let pin = lease(owner, LeaseKind::Transport, 0);
        state.retain_slot(owner, 3, object(0), pin).unwrap();
        let replacement = installed(owner, 3, 6, 1);
        assert!(
            state
                .publish_created_slot(owner, replacement, Some(original.binding))
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .close_binding(owner, original.binding)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.counts(object(0)).slots, 0);
        assert_eq!(state.counts(object(0)).transports, 1);
        assert_eq!(
            state.descriptor_binding(owner, 3).unwrap(),
            replacement.binding
        );
        assert_eq!(
            state
                .acknowledge_transport(pin, object(0), TransportResolution::CompletedAndRecorded)
                .unwrap(),
            BTreeSet::from([object(0)])
        );
        assert!(!state.is_retired(object(1)));
    }

    fn creation(slot: NetworkFdSlot) -> SlotPublicationEntry {
        SlotPublicationEntry {
            replacement: NetworkFdSlotReplacement {
                files: slot.binding.slot.files,
                installation_generation: slot.binding.generation,
                before: None,
                after: Some(slot),
            },
            source: SlotInstallationSource::Fresh,
        }
    }
    fn batch(
        owner: TaskOwner,
        sequence: u64,
        previous: u64,
        through: u64,
        entries: Vec<SlotPublicationEntry>,
    ) -> SlotPublicationBatch {
        SlotPublicationBatch {
            files: FilesId::initial(owner.tid),
            sequence,
            previous_generation: previous,
            through_generation: through,
            entries,
        }
    }
    #[test]
    fn duplicate_prefix_from_shared_owner_is_exactly_acknowledged_once() {
        let (mut state, owner) = setup();
        let sibling = task(11);
        state.share_table(owner, sibling, owner.tid).unwrap();
        let input = batch(owner, 1, 0, 4, vec![creation(installed(owner, 3, 4, 0))]);
        assert_eq!(
            state.publish_installation_batch(owner, &input),
            Ok(SlotPublicationResult::Applied {
                retired: BTreeSet::new()
            })
        );
        let before = state.clone();
        assert_eq!(
            state.publish_installation_batch(sibling, &input),
            Ok(SlotPublicationResult::AlreadyApplied)
        );
        assert_eq!(state, before);
        let mut mixed = input.clone();
        mixed.entries[0].replacement.after.as_mut().unwrap().cloexec = true;
        assert!(matches!(
            state.publish_installation_batch(sibling, &mixed),
            Err(LifetimeError::PublicationIdentity { .. })
        ));
        assert_eq!(state, before);
    }
    #[test]
    fn late_invalid_entry_does_not_publish_earlier_valid_installation() {
        let (mut state, owner) = setup();
        let before = state.clone();
        let input = batch(
            owner,
            1,
            0,
            7,
            vec![
                creation(installed(owner, 3, 4, 0)),
                creation(installed(owner, 4, 7, 0)),
            ],
        );
        assert_eq!(
            state.publish_installation_batch(owner, &input),
            Err(LifetimeError::OpenFileAlreadyUsed(object(0)))
        );
        assert_eq!(state, before);
    }
    #[test]
    fn ordered_prefixes_reject_reordering_without_skipping_or_partial_apply() {
        let (mut state, owner) = setup();
        let first = batch(owner, 1, 0, 4, vec![creation(installed(owner, 3, 4, 0))]);
        let second = batch(owner, 2, 4, 7, vec![creation(installed(owner, 4, 7, 1))]);
        let before = state.clone();
        assert!(state.publish_installation_batch(owner, &second).is_err());
        assert_eq!(state, before);
        state.publish_installation_batch(owner, &first).unwrap();
        state.publish_installation_batch(owner, &second).unwrap();
        let after = state.clone();
        assert_eq!(
            state.publish_installation_batch(owner, &first),
            Ok(SlotPublicationResult::AlreadyApplied)
        );
        assert_eq!(state, after);
    }
    #[test]
    fn regular_replacement_advances_high_water_and_retires_once() {
        let (mut state, owner) = setup();
        let initial = installed(owner, 3, 4, 0);
        state
            .publish_installation_batch(owner, &batch(owner, 1, 0, 4, vec![creation(initial)]))
            .unwrap();
        let removal = SlotPublicationEntry {
            replacement: NetworkFdSlotReplacement {
                files: initial.binding.slot.files,
                installation_generation: 8,
                before: Some(initial),
                after: None,
            },
            source: SlotInstallationSource::NonNetwork,
        };
        let input = batch(owner, 2, 4, 8, vec![removal]);
        assert_eq!(
            state.publish_installation_batch(owner, &input),
            Ok(SlotPublicationResult::Applied {
                retired: BTreeSet::from([object(0)])
            })
        );
        assert_eq!(state.tables[&input.files].last_slot_generation, 8);
        assert_eq!(
            state.publish_installation_batch(owner, &input),
            Ok(SlotPublicationResult::AlreadyApplied)
        );
        let before = state.clone();
        assert!(
            state
                .publish_installation_batch(
                    owner,
                    &batch(owner, 3, 8, 9, vec![creation(installed(owner, 3, 8, 1))])
                )
                .is_err()
        );
        assert_eq!(state, before);
        state
            .publish_installation_batch(
                owner,
                &batch(owner, 3, 8, 9, vec![creation(installed(owner, 3, 9, 1))]),
            )
            .unwrap();
        assert_eq!(
            state.descriptor_binding(owner, 3).unwrap().open_file,
            object(1)
        );
    }
    #[test]
    fn batch_retains_transport_owner_through_last_slot_replacement() {
        let (mut state, owner) = setup();
        let first = installed(owner, 3, 4, 0);
        state
            .publish_installation_batch(owner, &batch(owner, 1, 0, 4, vec![creation(first)]))
            .unwrap();
        let pin = lease(owner, LeaseKind::Transport, 0);
        state.retain_binding(owner, first.binding, pin).unwrap();
        let change = SlotPublicationEntry {
            replacement: NetworkFdSlotReplacement {
                files: first.binding.slot.files,
                installation_generation: 7,
                before: Some(first),
                after: Some(installed(owner, 3, 7, 1)),
            },
            source: SlotInstallationSource::Fresh,
        };
        assert_eq!(
            state.publish_installation_batch(owner, &batch(owner, 2, 4, 7, vec![change])),
            Ok(SlotPublicationResult::Applied {
                retired: BTreeSet::new()
            })
        );
        assert_eq!(state.counts(object(0)).slots, 0);
        assert_eq!(state.counts(object(0)).transports, 1);
        assert!(!state.is_retired(object(0)));
    }
    #[test]
    fn stale_incarnation_cannot_replay_a_known_batch_receipt() {
        let (mut state, owner) = setup();
        let input = batch(owner, 1, 0, 4, vec![creation(installed(owner, 3, 4, 0))]);
        state.publish_installation_batch(owner, &input).unwrap();
        let stale = TaskOwner {
            mm: owner.mm.for_exec(owner.tid),
            ..owner
        };
        let before = state.clone();
        assert_eq!(
            state.publish_installation_batch(stale, &input),
            Err(LifetimeError::StaleTask(stale))
        );
        assert_eq!(state, before);
    }

    fn task(tid: i32) -> TaskOwner {
        TaskOwner {
            tid: DetTid::from_raw(tid),
            mm: MmId::initial(DetTid::from_raw(tid)),
        }
    }

    fn object(sequence: u64) -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(10), sequence)
    }

    fn slot(sequence: u64, cloexec: bool) -> NetworkSlot {
        NetworkSlot {
            open_file: object(sequence),
            cloexec,
        }
    }

    fn setup() -> (NetworkLifetime, TaskOwner) {
        let owner = task(10);
        let mut state = NetworkLifetime::default();
        state
            .register(owner, owner.tid, FilesId::initial(owner.tid))
            .unwrap();
        (state, owner)
    }

    fn lease(owner: TaskOwner, kind: LeaseKind, ordinal: u32) -> LeaseId {
        LeaseId {
            operation: ExternalOpId::new(owner.tid, 7),
            mm: owner.mm,
            kind,
            ordinal,
        }
    }

    fn prepare(
        state: &mut NetworkLifetime,
        allocator: &mut crate::types::FilesIdAllocator,
        owner: TaskOwner,
    ) -> Result<ExecTicket, LifetimeError> {
        let task = state.task(owner)?.clone();
        state.prepare_exec(ExecFilesReceipt {
            caller: owner.tid,
            process: task.process,
            mm: owner.mm,
            old_files: task.files,
            new_files: allocator.allocate_exec(owner.tid),
        })
    }

    fn success(ticket: ExecTicket) -> ExecReconnect {
        ExecReconnect {
            caller: ticket.caller,
            new_leader: ticket.process,
            detpid: ticket.process,
            pre_exec_mm: ticket.mm,
            post_exec_mm: ticket.mm.for_exec(ticket.process),
            child_tid_addr: 0,
            reconnect_priority: None,
        }
    }

    fn active_slots(state: &NetworkLifetime, owner: TaskOwner) -> BTreeMap<RawFd, NetworkSlot> {
        let files = state.task(owner).unwrap().files;
        state.tables[&files].slots.clone()
    }

    #[test]
    fn dup_replacement_and_fd_reuse_retire_only_the_old_last_object() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, true)).unwrap();
        state.open(owner, 4, slot(1, false)).unwrap();
        assert_eq!(
            state.duplicate(owner, 3, object(0), 4, false).unwrap(),
            BTreeSet::from([object(1)])
        );
        assert!(
            state
                .duplicate(owner, 3, object(0), 3, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            active_slots(&state, owner)[&3].cloexec,
            "dup2(fd,fd) preserves FD_CLOEXEC"
        );
        assert!(state.close(owner, 3, object(0)).unwrap().is_empty());
        state.open(owner, 3, slot(2, false)).unwrap();
        let before = state.clone();
        assert_eq!(
            state.close(owner, 3, object(0)),
            Err(LifetimeError::SlotIdentity(3))
        );
        assert_eq!(state, before);
        assert_eq!(
            state.close(owner, 4, object(0)).unwrap(),
            BTreeSet::from([object(0)])
        );
        assert_eq!(state.counts(object(2)).slots, 1);
        assert_eq!(
            state.open(owner, 4, slot(0, false)),
            Err(LifetimeError::OpenFileAlreadyUsed(object(0)))
        );
    }

    #[test]
    fn shared_table_is_one_set_of_slots_and_fork_adds_aliases() {
        let (mut state, owner) = setup();
        let sibling = TaskOwner {
            tid: DetTid::from_raw(11),
            mm: owner.mm,
        };
        let child = task(20);
        state.open(owner, 3, slot(0, false)).unwrap();
        state.share_table(owner, sibling, owner.tid).unwrap();
        assert_eq!(state.counts(object(0)).slots, 1);
        state
            .copy_table(owner, child, child.tid, FilesId::forked(child.tid))
            .unwrap();
        assert_eq!(state.counts(object(0)).slots, 2);
        let host_snapshot = state.tables[&FilesId::initial(owner.tid)].clone();
        assert!(state.exit(owner).unwrap().is_empty());
        assert!(state.exit(sibling).unwrap().is_empty());
        assert_eq!(state.counts(object(0)).slots, 1);
        assert_eq!(
            host_snapshot.slots[&3].open_file,
            object(0),
            "host snapshots do not keep guest ownership alive"
        );
        assert_eq!(state.exit(child).unwrap(), BTreeSet::from([object(0)]));
        assert_eq!(state.finish(), Ok(()));
    }

    #[test]
    fn cloexec_is_per_slot_and_mutations_require_the_exact_open_file() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, false)).unwrap();
        state.duplicate(owner, 3, object(0), 4, false).unwrap();
        state.set_cloexec(owner, 3, object(0), true).unwrap();
        assert!(active_slots(&state, owner)[&3].cloexec);
        assert!(!active_slots(&state, owner)[&4].cloexec);
        let before = state.clone();
        assert_eq!(
            state.set_cloexec(owner, 3, object(1), false),
            Err(LifetimeError::SlotIdentity(3))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn failed_exec_preserves_active_slots_flags_and_all_aliases() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, true)).unwrap();
        state.duplicate(owner, 3, object(0), 4, false).unwrap();
        let original = active_slots(&state, owner);
        let ticket = prepare(&mut state, &mut allocator, owner).unwrap();
        assert_eq!(active_slots(&state, owner), original);
        assert_eq!(
            state.counts(object(0)),
            OwnerCounts {
                slots: 2,
                exec_reservations: 2,
                ..OwnerCounts::default()
            }
        );
        assert!(state.cancel_exec(ticket).unwrap().is_empty());
        assert_eq!(active_slots(&state, owner), original);
        assert_eq!(
            state.counts(object(0)),
            OwnerCounts {
                slots: 2,
                ..OwnerCounts::default()
            }
        );
        assert!(!state.is_retired(object(0)));
    }

    #[test]
    fn successful_exec_is_once_only_and_stale_exit_cannot_release_new_image() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, true)).unwrap();
        state.open(owner, 4, slot(1, false)).unwrap();
        let ticket = prepare(&mut state, &mut allocator, owner).unwrap();
        let event = success(ticket);
        assert_eq!(
            state.commit_exec(ticket, &event).unwrap(),
            BTreeSet::from([object(0)])
        );
        let replacement = TaskOwner {
            tid: owner.tid,
            mm: event.post_exec_mm,
        };
        assert_eq!(
            active_slots(&state, replacement),
            BTreeMap::from([(4, slot(1, false))])
        );
        let before = state.clone();
        assert_eq!(state.exit(owner), Err(LifetimeError::StaleTask(owner)));
        assert_eq!(
            state.commit_exec(ticket, &event),
            Err(LifetimeError::StaleTask(owner))
        );
        assert_eq!(state, before);
        assert_eq!(
            state.exit(replacement).unwrap(),
            BTreeSet::from([object(1)])
        );
    }

    #[test]
    fn exec_unshares_from_other_process_but_keeps_its_cloexec_alias() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, owner) = setup();
        let outside = task(20);
        state.open(owner, 3, slot(0, true)).unwrap();
        state.open(owner, 4, slot(1, false)).unwrap();
        state.share_table(owner, outside, outside.tid).unwrap();
        let ticket = prepare(&mut state, &mut allocator, owner).unwrap();
        let event = success(ticket);
        assert!(state.commit_exec(ticket, &event).unwrap().is_empty());
        assert_eq!(active_slots(&state, outside)[&3], slot(0, true));
        assert_eq!(state.counts(object(1)).slots, 2);
        assert_eq!(
            state.close(outside, 3, object(0)).unwrap(),
            BTreeSet::from([object(0)])
        );
    }

    #[test]
    fn nonleader_exec_drops_same_process_sibling_tables_atomically() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, leader) = setup();
        let caller = TaskOwner {
            tid: DetTid::from_raw(11),
            mm: leader.mm,
        };
        state.open(leader, 3, slot(0, true)).unwrap();
        state
            .copy_table(leader, caller, leader.tid, FilesId::forked(caller.tid))
            .unwrap();
        state.open(caller, 4, slot(1, false)).unwrap();
        let ticket = prepare(&mut state, &mut allocator, caller).unwrap();
        let event = success(ticket);
        assert_eq!(
            state.commit_exec(ticket, &event).unwrap(),
            BTreeSet::from([object(0)])
        );
        let replacement = TaskOwner {
            tid: leader.tid,
            mm: event.post_exec_mm,
        };
        assert_eq!(
            active_slots(&state, replacement),
            BTreeMap::from([(4, slot(1, false))])
        );
        assert_eq!(state.tasks.len(), 1);
        let before = state.clone();
        assert_eq!(state.exit(caller), Err(LifetimeError::StaleTask(caller)));
        assert_eq!(state.exit(leader), Err(LifetimeError::StaleTask(leader)));
        assert_eq!(state, before);
    }

    #[test]
    fn repeated_exec_after_nonleader_reassignment_keeps_table_incarnations_distinct() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, leader) = setup();
        let caller = TaskOwner {
            tid: DetTid::from_raw(11),
            mm: leader.mm,
        };
        state.open(leader, 3, slot(0, false)).unwrap();
        state.share_table(leader, caller, leader.tid).unwrap();
        let first = prepare(&mut state, &mut allocator, caller).unwrap();
        let first_event = success(first);
        assert!(state.commit_exec(first, &first_event).unwrap().is_empty());
        let replacement = TaskOwner {
            tid: leader.tid,
            mm: first_event.post_exec_mm,
        };
        let second = prepare(&mut state, &mut allocator, replacement).unwrap();
        assert_ne!(first.old_files, first.new_files);
        assert_ne!(second.new_files, first.old_files);
        assert_ne!(second.new_files, first.new_files);
        let second_event = success(second);
        assert!(state.commit_exec(second, &second_event).unwrap().is_empty());
        let current = TaskOwner {
            tid: leader.tid,
            mm: second_event.post_exec_mm,
        };
        assert_eq!(
            active_slots(&state, current),
            BTreeMap::from([(3, slot(0, false))])
        );
        let before = state.clone();
        assert_eq!(state.exit(leader), Err(LifetimeError::StaleTask(leader)));
        assert_eq!(state.exit(caller), Err(LifetimeError::StaleTask(caller)));
        assert_eq!(
            state.exit(replacement),
            Err(LifetimeError::StaleTask(replacement))
        );
        assert_eq!(state, before);
        assert_eq!(state.exit(current).unwrap(), BTreeSet::from([object(0)]));
        assert_eq!(state.finish(), Ok(()));
    }

    #[test]
    fn after_unshare_exec_reservation_survives_other_table_owner_close() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        for commit in [false, true] {
            let (mut state, owner) = setup();
            let outside = task(20);
            state.open(owner, 3, slot(0, false)).unwrap();
            state.share_table(owner, outside, outside.tid).unwrap();
            // Model the actual exec-unshare boundary here. A PrepareExec RPC
            // alone is NOT proof that a concurrent table close happens after
            // kernel unshare; production enrollment must supply that exclusion.
            let ticket = prepare(&mut state, &mut allocator, owner).unwrap();
            assert!(state.close(outside, 3, object(0)).unwrap().is_empty());
            assert_eq!(
                state.counts(object(0)),
                OwnerCounts {
                    exec_reservations: 1,
                    ..OwnerCounts::default()
                }
            );
            if commit {
                let event = success(ticket);
                assert!(state.commit_exec(ticket, &event).unwrap().is_empty());
                assert_eq!(state.counts(object(0)).slots, 1);
            } else {
                assert_eq!(
                    state.cancel_exec(ticket).unwrap(),
                    BTreeSet::from([object(0)])
                );
                assert!(
                    active_slots(&state, owner).is_empty(),
                    "cancel does not undo an independent real close"
                );
            }
        }
    }

    #[test]
    fn cancelled_attempt_cannot_cancel_or_commit_a_new_attempt() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, owner) = setup();
        let old = prepare(&mut state, &mut allocator, owner).unwrap();
        state.cancel_exec(old).unwrap();
        let new = prepare(&mut state, &mut allocator, owner).unwrap();
        assert_ne!(old, new);
        let before = state.clone();
        assert_eq!(state.cancel_exec(old), Err(LifetimeError::ExecIdentity));
        assert_eq!(
            state.commit_exec(old, &success(old)),
            Err(LifetimeError::ExecIdentity)
        );
        assert_eq!(state, before);
        state.cancel_exec(new).unwrap();
    }

    #[test]
    fn wrong_authenticated_exec_fields_leave_every_owner_unchanged() {
        let mut allocator = crate::types::FilesIdAllocator::default();
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, true)).unwrap();
        let ticket = prepare(&mut state, &mut allocator, owner).unwrap();
        for variant in 0..5 {
            let mut event = success(ticket);
            match variant {
                0 => event.caller = DetTid::from_raw(99),
                1 => event.detpid = DetPid::from_raw(99),
                2 => event.new_leader = DetTid::from_raw(99),
                3 => event.pre_exec_mm = owner.mm.for_exec(owner.tid),
                _ => event.post_exec_mm = owner.mm,
            }
            let before = state.clone();
            assert_eq!(
                state.commit_exec(ticket, &event),
                Err(LifetimeError::ExecIdentity)
            );
            assert_eq!(state, before);
        }
    }

    #[test]
    fn exit_and_last_close_do_not_acknowledge_physical_transport_cancel() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, false)).unwrap();
        let transfer = lease(owner, LeaseKind::Transfer, 0);
        let transport = lease(owner, LeaseKind::Transport, 0);
        let delivery = lease(owner, LeaseKind::Delivery, 0);
        for pin in [transfer, transport, delivery] {
            state.retain_slot(owner, 3, object(0), pin).unwrap();
        }
        assert!(state.exit(owner).unwrap().is_empty());
        assert_eq!(
            state.counts(object(0)),
            OwnerCounts {
                transfers: 1,
                transports: 1,
                deliveries: 1,
                ..OwnerCounts::default()
            }
        );
        assert!(state.release_lease(transfer, object(0)).unwrap().is_empty());
        assert!(state.release_lease(delivery, object(0)).unwrap().is_empty());
        let before = state.clone();
        assert_eq!(
            state.release_lease(transport, object(0)),
            Err(LifetimeError::TransportAcknowledgementRequired(transport))
        );
        assert_eq!(
            state.acknowledge_transport(transport, object(0), TransportResolution::UnknownEffects),
            Err(LifetimeError::UnresolvedTransport(transport))
        );
        assert_eq!(state, before);
        assert_eq!(state.finish(), Err(LifetimeError::OutstandingOwners));
        assert_eq!(
            state
                .acknowledge_transport(
                    transport,
                    object(0),
                    TransportResolution::CompletedAndRecorded
                )
                .unwrap(),
            BTreeSet::from([object(0)])
        );
        assert_eq!(state.finish(), Ok(()));
    }

    #[test]
    fn acknowledged_unsubmitted_transport_releases_only_its_exact_pin() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, false)).unwrap();
        let pin = lease(owner, LeaseKind::Transport, 0);
        state.retain_slot(owner, 3, object(0), pin).unwrap();
        state.close(owner, 3, object(0)).unwrap();
        state.open(owner, 3, slot(1, false)).unwrap();
        let before = state.clone();
        assert_eq!(
            state.acknowledge_transport(
                pin,
                object(1),
                TransportResolution::CancellationAcknowledgedBeforeSubmission
            ),
            Err(LifetimeError::LeaseIdentity(pin))
        );
        assert_eq!(state, before);
        assert_eq!(
            state
                .acknowledge_transport(
                    pin,
                    object(0),
                    TransportResolution::CancellationAcknowledgedBeforeSubmission
                )
                .unwrap(),
            BTreeSet::from([object(0)])
        );
        assert_eq!(state.counts(object(1)).slots, 1);
        assert_eq!(
            state.retain_slot(owner, 3, object(1), pin),
            Err(LifetimeError::LeaseAlreadyUsed(pin))
        );
    }

    #[test]
    fn queued_transfer_survives_sender_exit_and_installs_atomically() {
        let (mut state, sender) = setup();
        let receiver = task(20);
        state
            .register(receiver, receiver.tid, FilesId::initial(receiver.tid))
            .unwrap();
        state.open(sender, 3, slot(0, false)).unwrap();
        let queued = lease(sender, LeaseKind::Transfer, 0);
        state.retain_slot(sender, 3, object(0), queued).unwrap();
        assert!(state.exit(sender).unwrap().is_empty());
        let received = lease(receiver, LeaseKind::Transfer, 0);
        state
            .retain_lease(queued, object(0), receiver, received)
            .unwrap();
        assert!(state.release_lease(queued, object(0)).unwrap().is_empty());
        state
            .install_transfer(receiver, 8, object(0), received, true)
            .unwrap();
        assert_eq!(
            state.counts(object(0)),
            OwnerCounts {
                slots: 1,
                ..OwnerCounts::default()
            }
        );
        assert_eq!(active_slots(&state, receiver)[&8], slot(0, true));
        let before = state.clone();
        assert_eq!(
            state.install_transfer(receiver, 9, object(0), received, true),
            Err(LifetimeError::LeaseIdentity(received))
        );
        assert_eq!(state, before);
        assert_eq!(
            state.close(receiver, 8, object(0)).unwrap(),
            BTreeSet::from([object(0)])
        );
    }

    #[test]
    fn occupied_transfer_destination_does_not_drop_the_object_pin() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, false)).unwrap();
        state.open(owner, 4, slot(1, false)).unwrap();
        let pin = lease(owner, LeaseKind::Transfer, 0);
        state.retain_slot(owner, 3, object(0), pin).unwrap();
        state.close(owner, 3, object(0)).unwrap();
        let before = state.clone();
        assert_eq!(
            state.install_transfer(owner, 4, object(0), pin, false),
            Err(LifetimeError::SlotOccupied(4))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn operation_pins_require_exact_tid_mm_and_generation() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0, false)).unwrap();
        let valid = lease(owner, LeaseKind::Transport, 0);
        for invalid in [
            LeaseId {
                operation: ExternalOpId::new(DetTid::from_raw(99), 7),
                ..valid
            },
            LeaseId {
                mm: owner.mm.for_exec(owner.tid),
                ..valid
            },
        ] {
            let before = state.clone();
            assert_eq!(
                state.retain_slot(owner, 3, object(0), invalid),
                Err(LifetimeError::LeaseIdentity(invalid))
            );
            assert_eq!(state, before);
        }
        state.retain_slot(owner, 3, object(0), valid).unwrap();
        let wrong_sequence = LeaseId {
            operation: ExternalOpId::new(owner.tid, 8),
            ..valid
        };
        let before = state.clone();
        assert_eq!(
            state.acknowledge_transport(
                wrong_sequence,
                object(0),
                TransportResolution::CompletedAndRecorded,
            ),
            Err(LifetimeError::LeaseIdentity(wrong_sequence))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn canceled_global_allocation_cannot_be_reused() {
        let (mut state, owner) = setup();
        let mut allocator = crate::types::FilesIdAllocator::default();
        let ticket = prepare(&mut state, &mut allocator, owner).unwrap();
        state.cancel_exec(ticket).unwrap();
        let before = state.clone();
        assert_eq!(
            state.prepare_exec(ticket),
            Err(LifetimeError::TableAlreadyUsed(ticket.new_files))
        );
        assert_eq!(state, before);
    }
}
