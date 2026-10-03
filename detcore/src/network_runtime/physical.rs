//! Custody-purpose identities sharing the authenticated stopped-task description.
//! Possession never selects scheduler signal behavior.
mod foreground;
mod source_ioctl;
use std::collections::BTreeMap;

pub(crate) use foreground::ForegroundRoot;
pub(crate) use foreground::SharedForegroundLineage;
#[cfg(test)]
pub(crate) use foreground::controlled_foreground_root;
#[cfg(test)]
pub(crate) use foreground::controlled_foreground_runtime;
use serde::Deserialize;
use serde::Serialize;

use super::accepted_provider::FdEnrollment;
use super::accepted_provider::FdEvent;
use super::accepted_provider::Observation;
use super::accepted_provider::TableEnrollmentEffect;
use crate::network_replay::NetworkStreamOwner;
use crate::types::DetTid;
use crate::types::ExecFilesReceipt;
use crate::types::FilesId;
use crate::types::MmId;

/// Correlation only. Global RPC validates it against the retained task owner;
/// serializing this token conveys no table or descriptor authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialTableTicket {
    pub(super) registration: u64,
    pub(super) prepared_request: u64,
    pub(super) command: u64,
}
/// Cross-namespace task identity, constructed only from this custody entry's
/// retained completion of its exact preparation/collection request. The service
/// collected through the preparation's retained PIDFD_THREAD after checking its
/// pidfs identity and command; C checked that pidfd's TASK_STORAGE command.
/// Local owner/process numbers are not compared with initial-namespace BPF IDs.
/// This private value cannot be decoded from an RPC or built from caller fields.
#[derive(Debug, Clone)]
pub(super) struct CollectedEnrollment {
    owner: NetworkStreamOwner,
    process: i32,
    ticket: InitialTableTicket,
    files: FilesId,
    observation: Observation<TableEnrollmentEffect>,
}
impl CollectedEnrollment {
    pub(super) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    #[cfg(test)]
    pub(super) fn process(&self) -> i32 {
        self.process
    }
    pub(super) fn ticket(&self) -> InitialTableTicket {
        self.ticket
    }
    pub(super) fn observation(&self) -> &Observation<TableEnrollmentEffect> {
        &self.observation
    }
}
/// Initial physical provenance constructed only after the retained journal has
/// checked every occupied row. This is not a current table/slot capability and
/// does not classify inherited files as non-network or create semantic owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitialTableAssociation {
    owner: NetworkStreamOwner,
    process: i32,
    ticket: InitialTableTicket,
    provider: u64,
    files: FilesId,
    enrollment: FdEnrollment,
    slots: Vec<FdEvent>,
}
/// Exact initial issuance identity, retained on the existing native projection.
/// Only an authenticated complete census can issue these private fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InitialProjectionIdentity {
    owner: NetworkStreamOwner,
    process: i32,
    ticket: InitialTableTicket,
    provider: u64,
    task: u64,
    start: u64,
    table: u64,
}
impl InitialTableAssociation {
    // Complete model census used only by the real admission-transaction controls.
    // This is not a native provider receipt or a production constructor.
    #[cfg(test)]
    pub(crate) fn with_initial_fixture_fd(
        &self,
        mode: u32,
        flags: u32,
    ) -> (Self, InitialTableClaim) {
        use crate::types::FdSlot;
        use crate::types::FdSlotBinding;
        use crate::types::NetworkFdSlot;
        use crate::types::OpenFileId;
        let mut association = self.clone();
        association.slots = vec![
            super::accepted_provider_ffi::FdEvent {
                fd: 3,
                file: 37,
                mode,
                status_flags: flags,
                ..super::accepted_provider_ffi::FdEvent::default()
            }
            .into(),
        ];
        let mut raw: libc::stat = unsafe { std::mem::zeroed() };
        raw.st_mode = mode;
        let is_socket = crate::fd::FdType::from_initial_profile(mode, flags, 0, 0)
            == Some(crate::fd::FdType::Socket);
        let claim = InitialTableClaim {
            view: association.view(),
            metadata: vec![InitialFileStat {
                fd: 3,
                physical_file: 37,
                stat: raw.into(),
            }],
            slots: vec![NetworkFdSlot {
                binding: FdSlotBinding {
                    slot: FdSlot {
                        files: self.files,
                        fd: 3,
                    },
                    open_file: if is_socket {
                        OpenFileId::new_socket(self.owner.thread, 0)
                    } else {
                        OpenFileId::new(self.owner.thread, 0)
                    },
                    generation: 1,
                },
                cloexec: false,
            }],
            through_generation: 1,
            base_generation: 0,
            base_regular_sequence: 0,
            base_socket_sequence: 0,
        };
        association
            .check_claim(&claim)
            .expect("complete exact model census");
        (association, claim)
    }

    /// Validate the immutable original root identity before the semantic commit.
    /// The scheduler separately joins this identity to its retained live PIDFD.
    pub(crate) fn validate_root_identity(&self) -> std::io::Result<()> {
        self.root_projection_identity().map(|_| ())
    }

    pub(super) fn root_projection_identity(&self) -> std::io::Result<InitialProjectionIdentity> {
        let raw = &self.enrollment;
        if self.provider == 0
            || raw.task >> 32 == 0
            || raw.task >> 32 != u64::from(raw.task as u32)
            || raw.task_start == 0
            || self.ticket.registration == 0
            || self.ticket.prepared_request == 0
            || self.ticket.command == 0
            || raw.command != self.ticket.command
            || raw.registration != self.ticket.registration
            || raw.owner_mm != self.owner.mm.generation()
            || raw.table == 0
            || raw.ptrace_return != 0
        {
            return Err(std::io::Error::other(
                "initial projection lacks an exact native root leader census",
            ));
        }
        Ok(InitialProjectionIdentity {
            owner: self.owner,
            process: self.process,
            ticket: self.ticket,
            provider: self.provider,
            task: raw.task,
            start: raw.task_start,
            table: raw.table,
        })
    }
    pub(super) fn native_root(&self) -> std::io::Result<(u64, u64, u64)> {
        let identity = self.root_projection_identity()?;
        Ok((identity.provider, identity.task, identity.start))
    }

    pub(super) fn from_checked_journal(binding: &CollectedEnrollment, slots: Vec<FdEvent>) -> Self {
        Self {
            owner: binding.owner,
            process: binding.process,
            ticket: binding.ticket,
            provider: binding.observation.raw.command.identity.provider,
            files: binding.files,
            enrollment: binding.observation.raw.enrollment.clone(),
            slots,
        }
    }
    pub(crate) fn view(&self) -> InitialTableView {
        InitialTableView {
            owner: self.owner,
            files: self.files,
            registration: self.ticket.registration,
            table: self.enrollment.table,
            descriptors: self
                .slots
                .iter()
                .map(|slot| InitialDescriptor {
                    fd: slot.fd,
                    physical_file: slot.file,
                    cloexec: slot.returned == 1,
                    mode: slot.mode,
                    status_flags: slot.status_flags,
                    device_major: slot.device_major,
                    device_minor: slot.device_minor,
                })
                .collect(),
        }
    }
    pub(crate) fn process(&self) -> i32 {
        self.process
    }
    /// The view/claim are ordinary RPC data. Only this retained private census
    /// can attest their complete occupancy and physical-to-semantic alias join.
    pub(crate) fn check_claim(&self, claim: &InitialTableClaim) -> std::io::Result<()> {
        if claim.view != self.view() || claim.slots.len() != self.slots.len() {
            return Err(std::io::Error::other(
                "initial semantic claim changed physical census",
            ));
        }
        check_initial_stats(&claim.view, &claim.metadata)?;
        let mut physical_to_semantic = BTreeMap::new();
        let mut semantic_to_physical = BTreeMap::new();
        let mut generation = claim.base_generation;
        let mut regular = claim.base_regular_sequence;
        let mut socket = claim.base_socket_sequence;
        if claim.through_generation
            != generation
                .checked_add(self.slots.len() as u64)
                .ok_or_else(|| std::io::Error::other("initial generation exhausted"))?
        {
            return Err(std::io::Error::other(
                "initial census generation span changed",
            ));
        }
        for (physical, semantic) in self.slots.iter().zip(&claim.slots) {
            let b = semantic.binding;
            let is_socket = crate::fd::FdType::from_initial_profile(
                physical.mode,
                physical.status_flags,
                physical.device_major,
                physical.device_minor,
            ) == Some(crate::fd::FdType::Socket);
            let expected = match physical_to_semantic.get(&physical.file) {
                Some(previous) => *previous,
                None => {
                    let sequence = if is_socket { &mut socket } else { &mut regular };
                    if *sequence >= (1u64 << 63) {
                        return Err(std::io::Error::other("initial OFD sequence exhausted"));
                    }
                    let value = if is_socket {
                        crate::types::OpenFileId::new_socket(self.owner.thread, *sequence)
                    } else {
                        crate::types::OpenFileId::new(self.owner.thread, *sequence)
                    };
                    *sequence += 1;
                    value
                }
            };
            if b.open_file != expected
                || b.generation != generation + 1
                || b.slot.files != self.files
                || b.slot.fd != physical.fd
                || semantic.cloexec != (physical.returned == 1)
                || b.generation <= generation
                || b.generation > claim.through_generation
                || b.open_file.is_socket() != is_socket
                || physical_to_semantic
                    .insert(physical.file, b.open_file)
                    .is_some_and(|old| old != b.open_file)
                || semantic_to_physical
                    .insert(b.open_file, physical.file)
                    .is_some_and(|old| old != physical.file)
            {
                return Err(std::io::Error::other(
                    "initial semantic claim changed exact FD/OFD/profile relation",
                ));
            }
            generation = b.generation;
        }
        Ok(())
    }
    pub(super) fn native_provider(&self) -> u64 {
        self.provider
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn table(&self) -> u64 {
        self.enrollment.table
    }
    pub(crate) fn native_return(&self) -> i32 {
        self.enrollment.ptrace_return
    }
}
/// Private authenticated initial join, detached from the physical mutex before
/// the actual state-ready callback takes metadata and engine locks. Cloning it
/// does not acquire a native FD or make a serialized claim authoritative.
#[derive(Debug, Clone)]
pub(crate) struct InitialMetadataIdentity {
    association: InitialTableAssociation,
    claim: InitialTableClaim,
}
impl InitialMetadataIdentity {
    pub(crate) fn bind(
        &self,
        owner: NetworkStreamOwner,
        metadata: &mut crate::tool_local::FileMetadata,
    ) -> std::io::Result<()> {
        if self.association.owner != owner
            || metadata.files_id != self.association.files
            || !metadata.network_lifetime_tracking()
            || metadata.network_descriptor_slots() != self.claim.slots
        {
            return Err(std::io::Error::other(
                "ready metadata changed authenticated initial census",
            ));
        }
        let identities = super::original_installation::FileIdentity::from_initial_census(
            &self.association,
            &self.claim,
        )?;
        metadata
            .bind_native_population(&identities)
            .map_err(std::io::Error::other)
    }
}

/// One actual fstat obtained from a held file during the initial EXEC stop.
/// This RPC representation is correlation, not a constructor of physical
/// authority: the retained census authenticates the slot/OFD/profile, and the
/// exact full observation is included in the retained admission claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialFileStat {
    pub(crate) fd: i32,
    pub(crate) physical_file: u64,
    pub(crate) stat: crate::stat::DetStat,
}

/// Validate a complete first-slot-per-OFD stat population. All raw fields are
/// retained; only the fields independently observed by the census are compared.
/// Different OFDs may share an inode, and separate aliases must not invent
/// separate stat observations. This function cannot manufacture a capture.
pub(crate) fn check_initial_stats(
    view: &InitialTableView,
    stats: &[InitialFileStat],
) -> std::io::Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    let mut observations = stats.iter();
    for row in &view.descriptors {
        if seen.insert(row.physical_file) {
            let observed = observations
                .next()
                .ok_or_else(|| std::io::Error::other("initial stat population incomplete"))?;
            if observed.fd != row.fd
                || observed.physical_file != row.physical_file
                || observed.stat.mode != row.mode
                || libc::major(observed.stat.rdev) != row.device_major
                || libc::minor(observed.stat.rdev) != row.device_minor
            {
                return Err(std::io::Error::other(
                    "initial raw stat changed exact census slot/OFD/profile",
                ));
            }
        }
    }
    if observations.next().is_some() {
        return Err(std::io::Error::other(
            "initial stat population has extra observations",
        ));
    }
    Ok(())
}

/// Correlation data only. Serializing/deserializing a view or claim conveys no
/// authority; admission independently compares it with InitialTableAssociation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InitialDescriptor {
    pub(crate) fd: i32,
    pub(crate) physical_file: u64,
    pub(crate) cloexec: bool,
    pub(crate) mode: u32,
    pub(crate) status_flags: u32,
    pub(crate) device_major: u32,
    pub(crate) device_minor: u32,
}
/// Serialized initial-table correlation data from the retained physical census.
/// The private runtime association must authenticate this view before admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialTableView {
    pub(crate) owner: NetworkStreamOwner,
    pub(crate) files: FilesId,
    pub(crate) registration: u64,
    pub(crate) table: u64,
    pub(crate) descriptors: Vec<InitialDescriptor>,
}
/// Exact initial descriptor and metadata claim carried over the internal RPC.
/// Its private fields do not grant authority without the retained census match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialTableClaim {
    pub(crate) view: InitialTableView,
    pub(crate) metadata: Vec<InitialFileStat>,
    pub(crate) slots: Vec<crate::types::NetworkFdSlot>,
    pub(crate) through_generation: u64,
    pub(crate) base_generation: u64,
    pub(crate) base_regular_sequence: u64,
    pub(crate) base_socket_sequence: u64,
}

/// The ptrace startup callback intentionally ignores Error::Errno results.
/// Required initial metadata is admission, not a guest syscall result, so its
/// failure must remain a tool failure while retaining the original errno.
pub(crate) fn required_initial_metadata<T>(
    result: Result<T, reverie::syscalls::Errno>,
    fd: i32,
    physical_file: u64,
) -> Result<T, reverie::Error> {
    result.map_err(|errno| {
        reverie::Error::Tool(anyhow::Error::new(errno).context(format!(
            "required initial fstat failed for fd {fd}, physical OFD {physical_file}: {errno}"
        )))
    })
}

/// Every acquired auxiliary file has a retained observation and close outcome.
/// A missing/failed close is not silently repaired by repeating the capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InitialFileCapture {
    pub fd: i32,
    pub physical_file: u64,
    pub capture: super::accepted_provider::CallStatus,
    pub metadata: Result<(crate::stat::DetStat, i32, Option<i32>), String>,
    pub release: Option<super::accepted_provider::CallStatus>,
}
impl InitialFileCapture {
    fn checked_stat(&self, row: &InitialDescriptor) -> std::io::Result<InitialFileStat> {
        if self.fd != row.fd
            || self.physical_file != row.physical_file
            || self.capture.returned < 0
            || self.capture.errno.is_some()
            || self
                .release
                .as_ref()
                .is_none_or(|r| r.returned != 0 || r.errno.is_some())
        {
            return Err(std::io::Error::other(
                "initial metadata lacks exact capture/auxiliary close",
            ));
        }
        let (stat, flags, domain) = self
            .metadata
            .as_ref()
            .map_err(|error| std::io::Error::other(error.clone()))?;
        if matches!(*domain, Some(libc::AF_INET | libc::AF_INET6)) {
            return Err(std::io::Error::other(
                "inherited Internet socket refused before guest entry",
            ));
        }
        if *flags as u32 != row.status_flags
            || (stat.mode & libc::S_IFMT == libc::S_IFSOCK) != domain.is_some()
        {
            return Err(std::io::Error::other(
                "initial held-file profile changed census",
            ));
        }
        Ok(InitialFileStat {
            fd: self.fd,
            physical_file: self.physical_file,
            stat: *stat,
        })
    }
}

#[derive(Debug)]
struct Enrollment {
    registration: u64,
    preparation: Option<Result<u64, String>>,
    ticket: Option<InitialTableTicket>,
    native_read: Option<bool>,
    collection: Option<Result<u64, String>>,
    raw: Option<Result<Observation<TableEnrollmentEffect>, String>>,
    association: Option<InitialTableAssociation>,
    metadata_started: bool,
    metadata: Option<Result<Vec<InitialFileCapture>, String>>,
    semantic: Option<(InitialTableClaim, Result<(), String>)>,
    settled: bool,
}

impl Enrollment {
    fn unresolved(&self) -> bool {
        !self.settled || (self.metadata_started && self.metadata.is_none())
    }
}

// Owned work detached only from the short physical mutex, not from the
// original run/enrollment. The existing native-worker owner retains execution.
pub(super) struct InitialMetadataWork<T> {
    pub registration: u64,
    pub process: i32,
    pub task: T,
    pub association: InitialTableAssociation,
}
pub(super) enum InitialMetadataRequest<T> {
    Ready(Vec<InitialFileStat>),
    Work(Box<InitialMetadataWork<T>>),
}

type ForegroundMetadata = (
    std::sync::Weak<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    std::sync::Weak<std::sync::Mutex<crate::memory::MemoryMetadata>>,
);

#[derive(Debug)]
struct Task<T> {
    mm: MmId,
    process: i32,
    thread: i32,
    handle: T,
    initial_exec: Option<ExecFilesReceipt>,
    retired: bool,
    enrollment: Option<Enrollment>,
    native_birth: Option<super::native_birth::NativeBirthAdmission>,
    foreground_metadata: Option<ForegroundMetadata>,
    foreground_root: Option<std::sync::Arc<ForegroundRoot>>,
}
#[derive(Debug)]
pub(super) struct CustodyTasks<T> {
    tasks: BTreeMap<DetTid, Task<T>>,
    next_registration: u64,
    foreground_lineage_lost: bool,
    // Loss of the narrow V4 premise does not revoke live shared-task roots.
    sole_initial_root_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shared_mm_lineage_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    first_task: Option<DetTid>,
}
impl<T> Default for CustodyTasks<T> {
    fn default() -> Self {
        Self {
            tasks: BTreeMap::new(),
            next_registration: 0,
            foreground_lineage_lost: false,
            sole_initial_root_lost: Default::default(),
            shared_mm_lineage_lost: Default::default(),
            first_task: None,
        }
    }
}
impl<T> CustodyTasks<T> {
    /// The opener executes only after identity checks and before publication.
    /// Its failure leaves any previously registered incarnation untouched.
    pub(super) fn register(
        &mut self,
        owner: NetworkStreamOwner,
        process: i32,
        thread: i32,
        open: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<()> {
        if process <= 0 || thread <= 0 || thread != owner.thread.as_raw() {
            return Err(std::io::Error::other(
                "ptrace custody task identity mismatch",
            ));
        }
        if let Some(old) = self.tasks.get(&owner.thread) {
            if old.retired
                || (old.mm != owner.mm
                    && old.enrollment.as_ref().is_some_and(Enrollment::unresolved))
            {
                return Err(std::io::Error::other(
                    "unresolved physical enrollment cannot change incarnation",
                ));
            }
            if old.mm == owner.mm {
                return if old.process == process && old.thread == thread {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "custody identity changed within one MM",
                    ))
                };
            }
        }
        let handle = open()?;
        if self.tasks.get(&owner.thread).is_some_and(|old| {
            old.initial_exec.is_some() || old.foreground_root.is_some() || old.native_birth.is_some()
        }) {
            self.shared_mm_lineage_lost.store(true, std::sync::atomic::Ordering::Release);
        }
        if self.first_task.is_some_and(|first| first != owner.thread)
            || (self.first_task.is_some()
                && self
                    .tasks
                    .get(&owner.thread)
                    .is_none_or(|old| old.initial_exec.is_some() || old.foreground_root.is_some()))
        {
            self.sole_initial_root_lost
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(old) = self.tasks.get(&owner.thread)
            && let Some(root) = &old.foreground_root
        {
            root.revoke();
        }
        self.first_task.get_or_insert(owner.thread);
        self.tasks.insert(
            owner.thread,
            Task {
                mm: owner.mm,
                process,
                thread,
                handle,
                initial_exec: None,
                retired: false,
                enrollment: None,
                native_birth: None,
                foreground_metadata: None,
                foreground_root: None,
            },
        );
        Ok(())
    }
    /// Bind only the existing globally consumed exec preparation. The caller
    /// also owns the actual initial EXEC stop and the scheduler's retained pin;
    /// a FilesId carried in an RPC claim cannot create this relation.
    pub(super) fn bind_initial_exec(
        &mut self,
        owner: NetworkStreamOwner,
        receipt: ExecFilesReceipt,
    ) -> std::io::Result<()> {
        let task = self.task_mut(owner)?;
        if task.initial_exec == Some(receipt) {
            return Ok(());
        }
        if receipt.caller != owner.thread
            || receipt.process.as_raw() != task.process
            || task.process != task.thread
            || receipt.mm.for_exec(receipt.process) != owner.mm
            || receipt.old_files == receipt.new_files
            || task.native_birth.is_some()
            || task.enrollment.is_some()
            || task.initial_exec.is_some_and(|old| old != receipt)
        {
            return Err(std::io::Error::other(
                "initial exec changed retained root/table transition",
            ));
        }
        task.initial_exec = Some(receipt);
        Ok(())
    }

    pub(super) fn initial_exec(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Option<ExecFilesReceipt>> {
        self.get(owner)?;
        Ok(self.tasks[&owner.thread].initial_exec)
    }

    pub(super) fn get(&self, owner: NetworkStreamOwner) -> std::io::Result<&T> {
        self.tasks
            .get(&owner.thread)
            .filter(|task| task.mm == owner.mm && !task.retired)
            .map(|task| &task.handle)
            .ok_or_else(|| std::io::Error::other("unregistered custody task/MM"))
    }
    pub(super) fn installation_identity(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<(u64, u64, u64, u64)> {
        self.get(owner)?;
        if let Some(birth) = &self.tasks[&owner.thread].native_birth {
            let raw = birth.raw();
            return Ok((
                raw.provider,
                raw.child_task,
                raw.child_start,
                raw.child_table,
            ));
        }
        let association = self.initial_association(owner)?;
        let (provider, task, start) = association.native_root()?;
        Ok((provider, task, start, association.table()))
    }

    pub(super) fn native_table(&self, owner: NetworkStreamOwner) -> std::io::Result<u64> {
        self.get(owner)?;
        let task = &self.tasks[&owner.thread];
        if let Some(birth) = &task.native_birth {
            return Ok(birth.table());
        }
        self.initial_association(owner)
            .map(InitialTableAssociation::table)
    }
    pub(super) fn native_birth(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<super::native_birth::NativeBirthAdmission> {
        self.get(owner)?;
        self.tasks[&owner.thread]
            .native_birth
            .clone()
            .ok_or_else(|| std::io::Error::other("native child has no retained birth admission"))
    }
    pub(super) fn bind_native_child(
        &mut self,
        birth: &super::native_birth::NativeBirthAdmission,
    ) -> std::io::Result<()> {
        let raw = birth.raw();
        if raw.shared_mm != 1 || raw.shared_files != 1 || raw.same_thread_group != 1 {
            self.shared_mm_lineage_lost.store(true, std::sync::atomic::Ordering::Release);
        }
        if birth.terminal() {
            return Err(std::io::Error::other(
                "terminal child cannot gain live custody",
            ));
        }
        let task = self.task_mut(birth.child_owner())?;
        if task.process != birth.child_process().as_raw()
            || task.enrollment.is_some()
            || task.native_birth.as_ref().is_some_and(|old| old != birth)
        {
            return Err(std::io::Error::other(
                "native child changed its retained task/table association",
            ));
        }
        task.native_birth = Some(birth.clone());
        if let Some((metadata, memory)) = &task.foreground_metadata
            && let (Some(metadata), Some(memory)) = (metadata.upgrade(), memory.upgrade())
        {
            // Re-enter after releasing the task borrow; this issues the child
            // root only from the now-retained authenticated birth.
            let metadata = metadata.clone();
            let memory = memory.clone();
            let _ = task;
            return self.bind_foreground_metadata(birth.child_owner(), &metadata, &memory);
        }
        Ok(())
    }

    fn task_mut(&mut self, owner: NetworkStreamOwner) -> std::io::Result<&mut Task<T>> {
        self.tasks
            .get_mut(&owner.thread)
            .filter(|t| t.mm == owner.mm && !t.retired)
            .ok_or_else(|| std::io::Error::other("unregistered custody task/MM"))
    }
    fn enrollment(&mut self, owner: NetworkStreamOwner) -> std::io::Result<&mut Enrollment> {
        self.task_mut(owner)?
            .enrollment
            .as_mut()
            .ok_or_else(|| std::io::Error::other("task enrollment not submitted"))
    }
    pub(super) fn begin_initial(&mut self, owner: NetworkStreamOwner) -> std::io::Result<u64> {
        if let Some(e) = &self.task_mut(owner)?.enrollment {
            return Ok(e.registration);
        }
        let id = self
            .next_registration
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("table registration identity exhausted"))?;
        self.next_registration = id;
        self.task_mut(owner)?.enrollment = Some(Enrollment {
            registration: id,
            preparation: None,
            ticket: None,
            native_read: None,
            collection: None,
            raw: None,
            association: None,
            metadata_started: false,
            metadata: None,
            semantic: None,
            settled: false,
        });
        Ok(id)
    }
    pub(super) fn preparation(
        &mut self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Option<Result<u64, String>>> {
        Ok(self.enrollment(owner)?.preparation.clone())
    }
    pub(super) fn retain_preparation(
        &mut self,
        owner: NetworkStreamOwner,
        result: Result<u64, String>,
    ) -> std::io::Result<()> {
        let e = self.enrollment(owner)?;
        if let Some(old) = &e.preparation
            && old != &result
        {
            return Err(std::io::Error::other("enrollment preparation changed"));
        }
        e.preparation = Some(result);
        Ok(())
    }
    pub(super) fn prepared(
        &mut self,
        owner: NetworkStreamOwner,
        sequence: u64,
        command: u64,
    ) -> std::io::Result<InitialTableTicket> {
        let e = self.enrollment(owner)?;
        if command == 0 || e.preparation != Some(Ok(sequence)) {
            return Err(std::io::Error::other(
                "enrollment command/preparation mismatch",
            ));
        }
        let ticket = InitialTableTicket {
            registration: e.registration,
            prepared_request: sequence,
            command,
        };
        if e.ticket.is_some_and(|old| old != ticket) {
            return Err(std::io::Error::other("enrollment command replaced"));
        }
        e.ticket = Some(ticket);
        Ok(ticket)
    }
    pub(super) fn native_read(
        &mut self,
        owner: NetworkStreamOwner,
        ticket: InitialTableTicket,
        success: bool,
    ) -> std::io::Result<()> {
        let e = self.enrollment(owner)?;
        if e.ticket != Some(ticket) || e.native_read.is_some_and(|old| old != success) {
            return Err(std::io::Error::other("enrollment native result changed"));
        }
        e.native_read = Some(success);
        Ok(())
    }
    pub(super) fn collection(
        &mut self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Option<Result<u64, String>>> {
        Ok(self.enrollment(owner)?.collection.clone())
    }
    pub(super) fn retain_collection(
        &mut self,
        owner: NetworkStreamOwner,
        result: Result<u64, String>,
    ) -> std::io::Result<()> {
        let e = self.enrollment(owner)?;
        if let Some(old) = &e.collection
            && old != &result
        {
            return Err(std::io::Error::other("enrollment collection changed"));
        }
        e.collection = Some(result);
        Ok(())
    }
    pub(super) fn retain_raw(
        &mut self,
        owner: NetworkStreamOwner,
        result: Result<Observation<TableEnrollmentEffect>, String>,
    ) -> std::io::Result<()> {
        let e = self.enrollment(owner)?;
        if let Some(old) = &e.raw
            && old != &result
        {
            return Err(std::io::Error::other("enrollment raw completion changed"));
        }
        e.raw = Some(result);
        Ok(())
    }
    /// The only constructor of the local-owner/kernel-task relation. No caller
    /// supplied raw record or numeric kernel PID can create this proof. Keep the
    /// original task handle alive in this entry, including after an unknown RPC.
    pub(super) fn collected_binding(
        &self,
        owner: NetworkStreamOwner,
        ticket: InitialTableTicket,
        sequence: u64,
    ) -> std::io::Result<CollectedEnrollment> {
        self.get(owner)?;
        let task = &self.tasks[&owner.thread];
        let e = task
            .enrollment
            .as_ref()
            .ok_or_else(|| std::io::Error::other("task enrollment not submitted"))?;
        let observation = e
            .raw
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .ok_or_else(|| std::io::Error::other("enrollment has no retained completion"))?;
        let r = &observation.raw.command;
        let census = &observation.raw.enrollment;
        if e.ticket != Some(ticket)
            || e.preparation != Some(Ok(ticket.prepared_request))
            || e.collection != Some(Ok(sequence))
            || sequence == 0
            || e.native_read.is_none()
            || observation.status.returned != 0
            || observation.status.errno.is_some()
            || r.command != ticket.command
            || r.operation != 6
            || r.phase != 1
            || r.task >> 32 == 0
            || r.task as u32 == 0
            || r.start_boottime == 0
            || census.command != r.command
            || census.registration != e.registration
            || census.owner_mm != owner.mm.generation()
            || census.task != r.task
            || census.task_start != r.start_boottime
        {
            return Err(std::io::Error::other(
                "enrollment completion is not bound to the retained task/command/MM",
            ));
        }
        let files = match task.initial_exec {
            Some(receipt) => receipt.new_files,
            None if owner.mm == MmId::initial(owner.thread) => FilesId::initial(owner.thread),
            None => {
                return Err(std::io::Error::other(
                    "post-exec census lacks its consumed table receipt",
                ));
            }
        };
        Ok(CollectedEnrollment {
            owner,
            process: task.process,
            ticket,
            files,
            observation: observation.clone(),
        })
    }
    pub(super) fn complete(
        &mut self,
        owner: NetworkStreamOwner,
        association: InitialTableAssociation,
    ) -> std::io::Result<()> {
        let e = self.enrollment(owner)?;
        let retained = e.raw.as_ref().and_then(|r| r.as_ref().ok());
        if association.owner != owner
            || Some(association.ticket) != e.ticket
            || e.native_read.is_none()
            || !matches!(e.collection, Some(Ok(_)))
            || retained.is_none_or(|raw| {
                raw.status.returned != 0
                    || raw.raw.enrollment != association.enrollment
                    || raw.raw.command.identity.provider != association.provider
            })
        {
            return Err(std::io::Error::other(
                "enrollment completion changed retained task/command/observation",
            ));
        }
        // A valid negative native receipt is settled evidence, never admission.
        e.settled = true;
        if e.native_read != Some(true) || association.native_return() != 0 {
            return Err(std::io::Error::other("original GETREGSET failed"));
        }
        if e.association
            .as_ref()
            .is_some_and(|old| old != &association)
        {
            return Err(std::io::Error::other("enrollment association replaced"));
        }
        e.association = Some(association);
        Ok(())
    }
    pub(super) fn initial_association(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<&InitialTableAssociation> {
        self.get(owner)?;
        self.tasks[&owner.thread]
            .enrollment
            .as_ref()
            .and_then(|e| e.association.as_ref())
            .ok_or_else(|| std::io::Error::other("initial table census not admitted"))
    }
    pub(super) fn initial_metadata_identity(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Option<InitialMetadataIdentity>> {
        self.get(owner)?;
        let Some(enrollment) = &self.tasks[&owner.thread].enrollment else {
            return Ok(None);
        };
        let association = enrollment
            .association
            .as_ref()
            .ok_or_else(|| std::io::Error::other("initial identity has no checked census"))?;
        let (claim, result) = enrollment
            .semantic
            .as_ref()
            .ok_or_else(|| std::io::Error::other("initial identity lacks semantic admission"))?;
        if result.is_err() || association.owner != owner {
            return Err(std::io::Error::other(
                "initial identity has no successful exact admission",
            ));
        }
        association.check_claim(claim)?;
        Ok(Some(InitialMetadataIdentity {
            association: association.clone(),
            claim: claim.clone(),
        }))
    }
    /// Mark the one capture and snapshot exact custody before submitting its
    /// existing run-owned worker. No callback retry can create a second job.
    pub(super) fn begin_initial_metadata<U>(
        &mut self,
        owner: NetworkStreamOwner,
        duplicate: impl FnOnce(&T) -> std::io::Result<U>,
    ) -> std::io::Result<InitialMetadataRequest<U>> {
        let task = self.task_mut(owner)?;
        let e = task
            .enrollment
            .as_mut()
            .ok_or_else(|| std::io::Error::other("initial metadata has no enrollment"))?;
        let association = e
            .association
            .as_ref()
            .ok_or_else(|| std::io::Error::other("initial metadata has no checked census"))?;
        if task.initial_exec.is_none() {
            return Err(std::io::Error::other(
                "initial metadata is outside authenticated initial EXEC",
            ));
        }
        if let Some(result) = &e.metadata {
            return Self::checked_metadata(association, result).map(InitialMetadataRequest::Ready);
        }
        if e.metadata_started {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "original initial metadata worker still owns its result",
            ));
        }
        e.metadata_started = true;
        let pin = match duplicate(&task.handle) {
            Ok(pin) => pin,
            Err(error) => {
                e.metadata = Some(Err(error.to_string()));
                return Err(error);
            }
        };
        Ok(InitialMetadataRequest::Work(Box::new(
            InitialMetadataWork {
                registration: e.registration,
                process: task.process,
                task: pin,
                association: association.clone(),
            },
        )))
    }
    /// Completion is allowed after owner exit only on this still-retained
    /// registration/MM/association. Retention precedes the dispensable reply.
    pub(super) fn retain_initial_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        registration: u64,
        association: &InitialTableAssociation,
        result: Result<Vec<InitialFileCapture>, String>,
    ) -> std::io::Result<()> {
        let task = self
            .tasks
            .get_mut(&owner.thread)
            .filter(|t| t.mm == owner.mm)
            .ok_or_else(|| std::io::Error::other("initial metadata lost original task/MM"))?;
        let e = task
            .enrollment
            .as_mut()
            .ok_or_else(|| std::io::Error::other("initial metadata lost original enrollment"))?;
        if e.registration != registration
            || e.association.as_ref() != Some(association)
            || !e.metadata_started
            || e.metadata.is_some()
        {
            return Err(std::io::Error::other(
                "initial metadata completion changed original capture",
            ));
        }
        e.metadata = Some(result);
        Ok(())
    }
    pub(super) fn initial_metadata(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Vec<InitialFileStat>> {
        self.get(owner)?;
        let e = self.tasks[&owner.thread]
            .enrollment
            .as_ref()
            .ok_or_else(|| std::io::Error::other("initial metadata has no enrollment"))?;
        Self::checked_metadata(
            e.association
                .as_ref()
                .ok_or_else(|| std::io::Error::other("initial metadata has no checked census"))?,
            e.metadata.as_ref().ok_or_else(|| {
                std::io::Error::other("initial metadata worker has not completed")
            })?,
        )
    }
    // Keep the original state controls on the same begin/retain/read protocol;
    // production never invokes the blocking observer under this mutex.
    #[cfg(test)]
    pub(super) fn observe_initial_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        capture: impl FnOnce(&T, i32, &InitialTableView) -> std::io::Result<Vec<InitialFileCapture>>,
    ) -> std::io::Result<Vec<InitialFileStat>>
    where
        T: Clone,
    {
        match self.begin_initial_metadata(owner, |pin| Ok(pin.clone()))? {
            InitialMetadataRequest::Ready(metadata) => Ok(metadata),
            InitialMetadataRequest::Work(work) => {
                let result = capture(&work.task, work.process, &work.association.view())
                    .map_err(|error| error.to_string());
                self.retain_initial_metadata(owner, work.registration, &work.association, result)?;
                self.initial_metadata(owner)
            }
        }
    }
    fn checked_metadata(
        association: &InitialTableAssociation,
        captured: &Result<Vec<InitialFileCapture>, String>,
    ) -> std::io::Result<Vec<InitialFileStat>> {
        let captured = captured
            .as_ref()
            .map_err(|error| std::io::Error::other(error.clone()))?;
        let view = association.view();
        let mut seen = std::collections::BTreeSet::new();
        let mut captures = captured.iter();
        let mut result = Vec::new();
        for row in &view.descriptors {
            if seen.insert(row.physical_file) {
                let capture = captures
                    .next()
                    .ok_or_else(|| std::io::Error::other("initial metadata capture incomplete"))?;
                result.push(capture.checked_stat(row)?);
            }
        }
        if captures.next().is_some() {
            return Err(std::io::Error::other(
                "initial metadata contains extra captures",
            ));
        }
        check_initial_stats(&view, &result)?;
        Ok(result)
    }

    /// A response lost after the ledger commit recovers this exact result.
    /// The callback must revalidate current scheduler custody on every attempt,
    /// including retries. A retained result forbids a second ledger mutation.
    /// Both the native description and association stay borrowed until return.
    pub(super) fn admit_semantics(
        &mut self,
        owner: NetworkStreamOwner,
        claim: InitialTableClaim,
        admit: impl FnOnce(
            &InitialTableAssociation,
            &InitialTableClaim,
            &T,
            Option<&Result<(), String>>,
        ) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let task = self.task_mut(owner)?;
        let e = task
            .enrollment
            .as_mut()
            .ok_or_else(|| std::io::Error::other("task enrollment not submitted"))?;
        let association = e
            .association
            .as_ref()
            .ok_or_else(|| std::io::Error::other("semantic admission has no retained census"))?;
        association.check_claim(&claim)?;
        if task.initial_exec.is_some() {
            let captured = e.metadata.as_ref().ok_or_else(|| {
                std::io::Error::other("initial admission lacks held-file metadata")
            })?;
            if Self::checked_metadata(association, captured)? != claim.metadata {
                return Err(std::io::Error::other(
                    "initial claim changed retained full metadata",
                ));
            }
        }
        if let Some((previous, result)) = &e.semantic {
            return if previous == &claim {
                admit(association, &claim, &task.handle, Some(result))?;
                result.clone().map_err(std::io::Error::other)
            } else {
                Err(std::io::Error::other(
                    "initial semantic admission changed claim",
                ))
            };
        }
        let result = admit(association, &claim, &task.handle, None).map_err(|e| e.to_string());
        e.semantic = Some((claim, result.clone()));
        result.map_err(std::io::Error::other)
    }
    pub(super) fn enrollments_settled(&self) -> bool {
        self.tasks
            .values()
            .all(|t| t.enrollment.as_ref().is_none_or(|e| !e.unresolved()))
    }
    /// Close command admission on the existing authenticated custody entry.
    /// Absence alone conveys no terminal fact; the caller separately owns the
    /// backend final-wait event and marks only existing controller requests.
    pub(super) fn close_native_preparations(
        &mut self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<()> {
        if let Some(task) = self.tasks.get_mut(&owner.thread) {
            if task.mm != owner.mm {
                self.shared_mm_lineage_lost.store(true, std::sync::atomic::Ordering::Release);
                return Err(std::io::Error::other("terminal task changed custody MM"));
            }
            if task.foreground_root.is_none() {
                self.shared_mm_lineage_lost.store(true, std::sync::atomic::Ordering::Release);
            }
            if let Some(root) = &task.foreground_root {
                root.revoke();
            }
            task.retired = true;
        }
        Ok(())
    }
    pub(super) fn forget(&mut self, owner: NetworkStreamOwner) {
        if self
            .tasks
            .get(&owner.thread)
            .is_some_and(|task| task.mm == owner.mm)
        {
            if self.tasks[&owner.thread].foreground_root.is_none() {
                self.shared_mm_lineage_lost.store(true, std::sync::atomic::Ordering::Release);
            }
            if let Some(root) = &self.tasks[&owner.thread].foreground_root {
                root.revoke();
            }
            if self.tasks[&owner.thread]
                .enrollment
                .as_ref()
                .is_some_and(Enrollment::unresolved)
            {
                self.tasks.get_mut(&owner.thread).unwrap().retired = true;
            } else {
                self.tasks.remove(&owner.thread);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn owner(thread: i32) -> NetworkStreamOwner {
        let thread = DetTid::from_raw(thread);
        NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        }
    }
    fn initial_exec_fixture() -> (CustodyTasks<u32>, NetworkStreamOwner, InitialTableClaim) {
        initial_exec_fixture_with(7)
    }
    fn initial_exec_fixture_with<T>(
        handle: T,
    ) -> (CustodyTasks<T>, NetworkStreamOwner, InitialTableClaim) {
        let old = owner(41);
        let own = NetworkStreamOwner {
            mm: old.mm.for_exec(old.thread),
            ..old
        };
        let receipt = ExecFilesReceipt {
            caller: old.thread,
            process: old.thread,
            mm: old.mm,
            old_files: FilesId::initial(old.thread),
            new_files: crate::types::FilesIdAllocator::default().allocate_exec(old.thread),
        };
        let mut tasks = CustodyTasks::default();
        tasks.register(own, 41, 41, || Ok(handle)).unwrap();
        tasks.bind_initial_exec(own, receipt).unwrap();
        tasks.begin_initial(own).unwrap();
        let (mut association, _) = initial_root_fixture(own, 41);
        association.files = receipt.new_files;
        let (association, claim) =
            association.with_initial_fixture_fd(libc::S_IFREG | 0o600, libc::O_RDWR as u32);
        tasks
            .tasks
            .get_mut(&own.thread)
            .unwrap()
            .enrollment
            .as_mut()
            .unwrap()
            .association = Some(association);
        (tasks, own, claim)
    }
    fn captured(view: &InitialTableView, stat: crate::stat::DetStat) -> Vec<InitialFileCapture> {
        vec![InitialFileCapture {
            fd: view.descriptors[0].fd,
            physical_file: view.descriptors[0].physical_file,
            capture: super::super::accepted_provider::CallStatus {
                operation: "controlled retained file".into(),
                returned: 71,
                errno: None,
            },
            metadata: Ok((stat, libc::O_RDWR, None)),
            release: Some(super::super::accepted_provider::CallStatus {
                operation: "controlled close".into(),
                returned: 0,
                errno: None,
            }),
        }]
    }
    fn metadata_worker_fixture() -> (
        super::super::NetworkRuntimeResources,
        NetworkStreamOwner,
        InitialTableClaim,
    ) {
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) };
        assert!(raw >= 0);
        let (mut tasks, owner, claim) =
            initial_exec_fixture_with(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        // Existing checked-census fixture: model the already-retired provider
        // command so only the new pending metadata prevents registry removal.
        tasks
            .tasks
            .get_mut(&owner.thread)
            .unwrap()
            .enrollment
            .as_mut()
            .unwrap()
            .settled = true;
        let (runtime, _) = super::super::tests::fixture(89);
        *runtime.shared.physical.lock().unwrap() = tasks;
        (runtime, owner, claim)
    }

    async fn blocked_metadata_worker(deadline_failure: bool) {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use std::time::Instant;
        let (runtime, owner, claim) = metadata_worker_fixture();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let captures = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&captures);
        let stat = claim.metadata[0].stat;
        let mut waiter = Box::pin(runtime.observe_initial_metadata_with(
            owner,
            move |_, _, view| {
                count.fetch_add(1, Ordering::SeqCst);
                entered.send(()).unwrap();
                proceed.recv_timeout(Duration::from_secs(1)).unwrap();
                Ok(captured(view, stat))
            },
        ));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        drop(waiter);
        // A stalled filesystem operation holds no controller/physical lock.
        // Cancellation retains its original worker and cannot begin another.
        {
            let mut tasks = runtime
                .shared
                .physical
                .try_lock()
                .expect("blocked metadata held physical mutex");
            assert!(!tasks.enrollments_settled());
            assert!(
                tasks
                    .begin_initial_metadata(owner, |_| -> std::io::Result<()> {
                        panic!("duplicate callback captured another task")
                    })
                    .is_err()
            );
            if deadline_failure {
                tasks.forget(owner);
                assert!(tasks.get(owner).is_err());
                assert!(
                    tasks.tasks.contains_key(&owner.thread),
                    "pending enrollment was erased"
                );
                let changed = NetworkStreamOwner {
                    mm: owner.mm.for_exec(owner.thread),
                    ..owner
                };
                assert!(
                    tasks
                        .register(changed, 41, 41, || panic!("retired pending MM replaced"))
                        .is_err()
                );
            }
        }
        assert_eq!(runtime.shared.native_workers.lock().unwrap().tasks.len(), 1);
        let worker = runtime.shared.native_workers.lock().unwrap().tasks[0].clone();
        if deadline_failure {
            let original = Instant::now() + Duration::from_millis(10);
            tokio::time::timeout(
                Duration::from_millis(100),
                runtime.shared.finish_native_workers(original),
            )
            .await
            .expect("blocked metadata prevented bounded terminal progress")
            .unwrap_err();
            assert!(worker.deadline_failure.lock().unwrap().is_some());
            release.send(()).unwrap();
            assert!(runtime.shared.join_native_worker(&worker).await.is_err());
            assert!(
                runtime
                    .shared
                    .finish_native_workers(original)
                    .await
                    .is_err(),
                "late completion erased deadline failure"
            );
            let tasks = runtime.shared.physical.lock().unwrap();
            let entry = tasks.tasks[&owner.thread].enrollment.as_ref().unwrap();
            assert_eq!(
                entry.metadata.as_ref().unwrap().as_ref().unwrap(),
                &captured(&claim.view, stat)
            );
            assert!(tasks.tasks[&owner.thread].retired);
            assert!(
                tasks.initial_metadata(owner).is_err(),
                "dead owner gained live admission"
            );
        } else {
            release.send(()).unwrap();
            runtime
                .shared
                .finish_native_workers(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap();
            assert_eq!(
                runtime
                    .shared
                    .physical
                    .lock()
                    .unwrap()
                    .initial_metadata(owner)
                    .unwrap(),
                claim.metadata
            );
            assert!(
                matches!(runtime.shared.physical.lock().unwrap().begin_initial_metadata(
                owner, |_| -> std::io::Result<()> { panic!("lost reply repeated capture") }
            ).unwrap(), InitialMetadataRequest::Ready(metadata) if metadata == claim.metadata)
            );
            assert!(
                runtime
                    .shared
                    .native_workers
                    .lock()
                    .unwrap()
                    .tasks
                    .is_empty()
            );
        }
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert!(
            worker.completion.lock().await.task.is_none(),
            "original worker join was not consumed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_metadata_canceled_waiter_keeps_result_and_physical_control_responsive() {
        blocked_metadata_worker(false).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn initial_metadata_deadline_retains_registration_and_actual_late_completion() {
        blocked_metadata_worker(true).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn initial_metadata_closed_worker_admission_retains_failure_without_native_retry() {
        let (runtime, owner, _) = metadata_worker_fixture();
        runtime
            .shared
            .finish_native_workers(std::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
        for _ in 0..2 {
            assert!(
                runtime
                    .observe_initial_metadata_with(owner, |_, _, _| panic!(
                        "terminal cutoff admitted capture"
                    ))
                    .await
                    .is_err()
            );
        }
        let tasks = runtime.shared.physical.lock().unwrap();
        let entry = tasks.tasks[&owner.thread].enrollment.as_ref().unwrap();
        assert!(entry.metadata_started);
        assert!(entry.metadata.as_ref().unwrap().is_err());
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn initial_exec_binding_refuses_missing_changed_mm_and_unreserved_table() {
        let old = owner(41);
        let own = NetworkStreamOwner {
            mm: old.mm.for_exec(old.thread),
            ..old
        };
        let receipt = ExecFilesReceipt {
            caller: old.thread,
            process: old.thread,
            mm: old.mm,
            old_files: FilesId::initial(old.thread),
            new_files: crate::types::FilesIdAllocator::default().allocate_exec(old.thread),
        };
        for variant in 0..4 {
            let mut tasks = CustodyTasks::default();
            tasks.register(own, 41, 41, || Ok(7)).unwrap();
            let mut wrong = receipt;
            match variant {
                0 => wrong.mm = own.mm,
                1 => wrong.caller = DetTid::from_raw(42),
                2 => wrong.process = DetTid::from_raw(42),
                3 => wrong.new_files = wrong.old_files,
                _ => unreachable!(),
            }
            assert!(tasks.bind_initial_exec(own, wrong).is_err());
            assert!(tasks.tasks[&own.thread].initial_exec.is_none());
            tasks.bind_initial_exec(own, receipt).unwrap();
            tasks.begin_initial(own).unwrap();
            tasks.bind_initial_exec(own, receipt).unwrap(); // lost-reply recovery preserves exact receipt
            assert!(tasks.bind_initial_exec(own, wrong).is_err());
            assert!(
                tasks
                    .observe_initial_metadata(old, |_, _, _| panic!("wrong MM captured metadata"))
                    .is_err()
            );
        }
    }

    #[test]
    fn initial_exec_metadata_retains_exact_full_stat_and_consumed_table_receipt() {
        let (mut tasks, own, mut claim) = initial_exec_fixture();
        let stat = claim.metadata[0].stat;
        assert_ne!(claim.view.files, FilesId::initial(own.thread));
        claim.metadata = tasks
            .observe_initial_metadata(own, |pin, process, view| {
                assert_eq!((*pin, process), (7, 41));
                Ok(captured(view, stat))
            })
            .unwrap();
        assert_eq!(
            tasks
                .observe_initial_metadata(own, |_, _, _| panic!(
                    "lost reply repeated native capture"
                ))
                .unwrap(),
            claim.metadata
        );
        let mut forged = claim.clone();
        forged.metadata[0].stat.inode += 1;
        assert!(
            tasks
                .admit_semantics(own, forged, |_, _, _, _| panic!("forged stat admitted"))
                .is_err()
        );
        let mut wrong_table = claim.clone();
        wrong_table.slots[0].binding.slot.files = FilesId::initial(own.thread);
        assert!(
            tasks
                .admit_semantics(own, wrong_table, |_, _, _, _| panic!("old table admitted"))
                .is_err()
        );
        tasks
            .admit_semantics(own, claim.clone(), |_, _, pin, old| {
                assert_eq!(*pin, 7);
                assert!(old.is_none());
                Ok(())
            })
            .unwrap();
        tasks
            .admit_semantics(own, claim, |_, _, _, old| {
                assert_eq!(old, Some(&Ok(())));
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn initial_exec_metadata_missing_close_or_failed_capture_never_retries_or_admits() {
        for variant in 0..4 {
            let (mut tasks, own, claim) = initial_exec_fixture();
            let stat = claim.metadata[0].stat;
            assert!(
                tasks
                    .observe_initial_metadata(own, |_, _, view| {
                        let mut rows = captured(view, stat);
                        match variant {
                            0 => rows[0].release = None,
                            1 => rows[0].release.as_mut().unwrap().errno = Some(libc::EIO),
                            2 => rows[0].metadata = Err("actual fstat failure".into()),
                            3 => {
                                rows[0].capture.returned = -1;
                                rows[0].capture.errno = Some(libc::EBADF);
                            }
                            _ => unreachable!(),
                        }
                        Ok(rows)
                    })
                    .is_err()
            );
            assert!(
                tasks
                    .observe_initial_metadata(own, |_, _, _| panic!("failed capture retried"))
                    .is_err()
            );
            assert!(
                tasks
                    .admit_semantics(own, claim, |_, _, _, _| panic!(
                        "incomplete observer admitted"
                    ))
                    .is_err()
            );
        }
    }
    #[test]
    fn initial_held_domain_refuses_inet_even_on_standard_or_aliased_descriptor() {
        let (tasks, own, _) = initial_exec_fixture();
        let mut row = tasks.initial_association(own).unwrap().view().descriptors[0].clone();
        row.fd = 0;
        row.mode = libc::S_IFSOCK | 0o600;
        let mut raw: libc::stat = unsafe { std::mem::zeroed() };
        raw.st_mode = row.mode;
        let view = InitialTableView {
            owner: own,
            files: FilesId::initial(own.thread),
            registration: 1,
            table: 1,
            descriptors: vec![row.clone()],
        };
        let mut observed = captured(&view, raw.into()).remove(0);
        for domain in [libc::AF_INET, libc::AF_INET6] {
            observed.metadata = Ok((raw.into(), libc::O_RDWR, Some(domain)));
            assert!(observed.checked_stat(&row).is_err());
            let mut alias = row.clone();
            alias.fd = 99;
            observed.fd = 99;
            assert!(observed.checked_stat(&alias).is_err());
            observed.fd = 0;
        }
        observed.metadata = Ok((raw.into(), libc::O_RDWR, Some(libc::AF_UNIX)));
        assert!(observed.checked_stat(&row).is_ok());
    }

    #[test]
    fn actual_terminal_fence_rejects_late_preparation_without_reopening_identity() {
        let mut tasks = CustodyTasks::default();
        let own = owner(41);
        tasks.register(own, 41, 41, || Ok(7)).unwrap();
        let wrong = NetworkStreamOwner {
            mm: own.mm.for_exec(own.thread),
            ..own
        };
        assert!(tasks.close_native_preparations(wrong).is_err());
        assert_eq!(*tasks.get(own).unwrap(), 7);
        tasks.close_native_preparations(own).unwrap();
        assert!(tasks.get(own).is_err());
        assert!(
            tasks
                .register(own, 41, 41, || panic!("terminal registration reopened pin"))
                .is_err()
        );
        tasks.close_native_preparations(own).unwrap();
        tasks.forget(own);
        assert!(tasks.tasks.is_empty());
    }
    #[test]
    fn wrong_task_and_failed_open_never_replace_authority() {
        let mut tasks = CustodyTasks::default();
        let old = owner(7);
        assert!(tasks.register(old, 7, 8, || Ok(99)).is_err());
        tasks.register(old, 7, 7, || Ok(1)).unwrap();
        tasks
            .register(old, 7, 7, || {
                panic!("idempotent registration reopened task")
            })
            .unwrap();
        assert!(tasks.register(old, 8, 7, || Ok(2)).is_err());
        let new = NetworkStreamOwner {
            mm: old.mm.for_exec(old.thread),
            ..old
        };
        assert!(
            tasks
                .register(new, 7, 7, || Err(std::io::Error::other("pidfd failure")))
                .is_err()
        );
        assert_eq!(*tasks.get(old).unwrap(), 1);
        assert!(tasks.get(new).is_err());
        tasks.register(new, 7, 7, || Ok(3)).unwrap();
        tasks.forget(old);
        assert_eq!(*tasks.get(new).unwrap(), 3);
        tasks.forget(new);
        assert!(tasks.get(new).is_err());
    }
}

// Synthetic complete empty census for transaction controls. This does not
// claim a kernel observation; native qualification must use the real producer.
#[cfg(test)]
pub(crate) fn initial_root_fixture(
    owner: NetworkStreamOwner,
    process: i32,
) -> (InitialTableAssociation, InitialTableClaim) {
    use super::accepted_provider_ffi as ffi;
    let ticket = InitialTableTicket {
        registration: 1,
        prepared_request: 17,
        command: 23,
    };
    let enrollment = ffi::FdEnrollment {
        command: ticket.command,
        registration: ticket.registration,
        owner_mm: owner.mm.generation(),
        task: (5001u64 << 32) | 5001,
        task_start: 29,
        table: 31,
        begin: 1,
        end: 2,
        ..ffi::FdEnrollment::default()
    };
    let binding = CollectedEnrollment {
        owner,
        process,
        ticket,
        files: FilesId::initial(owner.thread),
        observation: Observation {
            status: super::accepted_provider::CallStatus {
                operation: "controlled initial census".into(),
                returned: 0,
                errno: None,
            },
            raw: super::accepted_provider::TableEnrollmentEffect {
                command: ffi::CommandResult {
                    command: ticket.command,
                    operation: 6,
                    task: enrollment.task,
                    start_boottime: enrollment.task_start,
                    phase: 1,
                    identity: ffi::Identity {
                        provider: 3,
                        ..ffi::Identity::default()
                    },
                    ..ffi::CommandResult::default()
                }
                .into(),
                enrollment: enrollment.into(),
            },
        },
    };
    let association = InitialTableAssociation::from_checked_journal(&binding, vec![]);
    let claim = InitialTableClaim {
        view: association.view(),
        metadata: vec![],
        slots: vec![],
        through_generation: 0,
        base_generation: 0,
        base_regular_sequence: 0,
        base_socket_sequence: 0,
    };
    (association, claim)
}

#[cfg(test)]
pub(crate) fn changed_initial_root_fixture(
    source: &InitialTableAssociation,
    field: &str,
) -> InitialTableAssociation {
    let mut changed = source.clone();
    match field {
        "MM" => changed.owner.mm = changed.owner.mm.for_exec(changed.owner.thread),
        "process" => changed.process += 1,
        "registration" => {
            changed.ticket.registration += 1;
            changed.enrollment.registration += 1;
        }
        "request" => changed.ticket.prepared_request += 1,
        "command" => {
            changed.ticket.command += 1;
            changed.enrollment.command += 1;
        }
        "provider" => changed.provider += 1,
        "start" => changed.enrollment.task_start += 1,
        "task" => changed.enrollment.task += (1u64 << 32) | 1,
        "table" => changed.enrollment.table += 1,
        "nonleader" => changed.enrollment.task += 1,
        "missing start" => changed.enrollment.task_start = 0,
        "missing provider" => changed.provider = 0,
        _ => panic!("unknown controlled initial-census mutation"),
    }
    changed
}

#[cfg(test)]
mod initial_admission_tests {
    use super::*;
    fn fixture() -> (CustodyTasks<u64>, NetworkStreamOwner, InitialTableClaim) {
        let thread = DetTid::from_raw(41);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let (association, claim) = initial_root_fixture(owner, 41);
        let mut tasks = CustodyTasks::default();
        tasks.register(owner, 41, 41, || Ok(99)).unwrap();
        tasks.begin_initial(owner).unwrap();
        let entry = tasks.enrollment(owner).unwrap();
        entry.association = Some(association);
        entry.settled = true;
        (tasks, owner, claim)
    }
    #[test]
    fn initial_admission_retry_revalidates_custody_without_second_commit() {
        let (mut tasks, owner, claim) = fixture();
        let mut commits = 0;
        tasks
            .admit_semantics(owner, claim.clone(), |_, _, pin, previous| {
                assert_eq!(*pin, 99);
                assert!(previous.is_none());
                commits += 1;
                Ok(())
            })
            .unwrap();
        tasks
            .admit_semantics(owner, claim.clone(), |_, _, pin, previous| {
                assert_eq!(*pin, 99);
                assert_eq!(previous, Some(&Ok(())));
                Ok(())
            })
            .unwrap();
        let retained = format!("{tasks:?}");
        assert!(
            tasks
                .admit_semantics(owner, claim, |_, _, _, previous| {
                    assert_eq!(previous, Some(&Ok(())));
                    Err(std::io::Error::other(
                        "controlled stale scheduler registration",
                    ))
                })
                .is_err()
        );
        assert_eq!(format!("{tasks:?}"), retained);
        assert_eq!(commits, 1);
    }
    #[test]
    fn initial_admission_failure_cannot_be_relabelled_or_replace_custody() {
        let (mut tasks, owner, claim) = fixture();
        assert!(
            tasks
                .admit_semantics(owner, claim.clone(), |_, _, pin, previous| {
                    assert_eq!(*pin, 99);
                    assert!(previous.is_none());
                    Err(std::io::Error::other("original census refusal"))
                })
                .is_err()
        );
        let retained = format!("{tasks:?}");
        let error = tasks
            .admit_semantics(owner, claim, |_, _, _, previous| {
                assert_eq!(previous, Some(&Err("original census refusal".into())));
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "original census refusal");
        assert_eq!(format!("{tasks:?}"), retained);
        assert_eq!(*tasks.get(owner).unwrap(), 99);
    }
    #[test]
    fn initial_admission_changed_claim_refuses_before_transaction_callback() {
        let (mut tasks, owner, claim) = fixture();
        tasks
            .admit_semantics(owner, claim.clone(), |_, _, _, _| Ok(()))
            .unwrap();
        let retained = format!("{tasks:?}");
        let mut changed = claim;
        changed.base_socket_sequence += 1;
        assert!(
            tasks
                .admit_semantics(owner, changed, |_, _, _, _| panic!(
                    "changed claim committed"
                ))
                .is_err()
        );
        assert_eq!(format!("{tasks:?}"), retained);
    }
}

#[cfg(test)]
mod initial_native_identity_tests {
    use reverie::syscalls::OFlag;

    use super::*;
    use crate::tool_local::FileMetadata;
    fn fixture() -> (NetworkStreamOwner, InitialMetadataIdentity, FileMetadata) {
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let (association, _) = initial_root_fixture(owner, 61);
        let (association, claim) = association.with_initial_fixture_fd(libc::S_IFREG | 0o600, 0);
        let empty = FileMetadata::empty_network_fixture(thread);
        let (metadata, replacement) = empty
            .prepare_original_installation_typed(
                thread,
                3,
                OFlag::empty(),
                crate::fd::FdType::Regular,
                None,
            )
            .unwrap();
        assert_eq!(Some(claim.slots[0]), replacement.after);
        (
            owner,
            InitialMetadataIdentity { association, claim },
            metadata,
        )
    }
    #[test]
    fn initial_native_identity_comes_from_retained_census_and_survives_slot_removal() {
        let (owner, identity, mut metadata) = fixture();
        let binding = identity.claim.slots[0].binding;
        assert_eq!(metadata.native_binding_identity(binding), None);
        identity.bind(owner, &mut metadata).unwrap();
        let observed = metadata.native_binding_identity(binding).unwrap();
        assert!(observed.matches(3, 37));
        identity.bind(owner, &mut metadata).unwrap();
        assert!(metadata.remove_descriptor_binding(binding));
        assert_eq!(metadata.native_binding_identity(binding), None);
        assert!(observed.matches(3, 37)); // proof scalar owns no descriptor
        assert!(identity.bind(owner, &mut metadata).is_err());
    }
    #[test]
    fn initial_native_identity_rejects_changed_correlation_without_annotating_metadata() {
        let (owner, identity, metadata) = fixture();
        for case in 0..7 {
            let mut changed = identity.clone();
            let mut candidate = metadata.clone();
            let binding = changed.claim.slots[0].binding;
            let mut presented_owner = owner;
            match case {
                0 => changed.claim.view.descriptors[0].physical_file += 1,
                1 => changed.claim.slots[0].binding.generation += 1,
                2 => {
                    changed.claim.slots[0].binding.open_file =
                        crate::types::OpenFileId::new(owner.thread, 9)
                }
                3 => changed.association.provider = 0,
                4 => changed.claim.slots.clear(),
                5 => presented_owner.mm = owner.mm.for_exec(owner.thread),
                6 => {
                    assert!(candidate.remove_descriptor_binding(binding));
                }
                _ => unreachable!(),
            }
            assert!(
                changed.bind(presented_owner, &mut candidate).is_err(),
                "case {case}"
            );
            assert_eq!(
                candidate.native_binding_identity(binding),
                None,
                "case {case}"
            );
        }
    }
}
