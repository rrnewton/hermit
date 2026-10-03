//! Positive initial-root lineage, retained in the existing physical Task.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;
use crate::memory::MemoryMetadata;
use crate::tool_local::FileMetadata;

#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod source_ioctl_fixture;

/// Private census/EXEC provenance. Neither serde, a config bit nor a numeric
/// task/file identifier constructs this authority. Revocation is irreversible.
#[derive(Debug)]
pub(crate) struct ForegroundRoot {
    association: InitialTableAssociation,
    owner: NetworkStreamOwner,
    // Retain the original logical process identity and field/drop order even
    // though the current physical consumer uses the process below.
    _logical_process: crate::types::DetPid,
    process: i32,
    thread: i32,
    native_identity: (u64, u64, u64, u64),
    initial_exec: Option<ExecFilesReceipt>,
    parent: Option<Arc<ForegroundRoot>>,
    metadata: Weak<Mutex<FileMetadata>>,
    memory: Weak<Mutex<MemoryMetadata>>,
    revoked: AtomicBool,
    sole_initial_root_lost: Arc<AtomicBool>,
}
impl ForegroundRoot {
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "retained shared-birth guard fixture; no active caller or qualification claim"
    )]
    pub(crate) async fn controlled_shared_birth_fixture(
        thread: i32,
    ) -> policy_tests::SharedBirthFixture {
        policy_tests::SharedBirthFixture::new(thread).await
    }
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "retained pre-close shared-birth guard fixture; no active caller or qualification claim"
    )]
    pub(crate) async fn controlled_shared_birth_after_close_setup(
        thread: i32,
        before: impl FnOnce(&Arc<Self>, &InitialTableClaim),
    ) -> policy_tests::SharedBirthFixture {
        policy_tests::SharedBirthFixture::new_after_setup(thread, before).await
    }
    #[cfg(test)]
    pub(crate) async fn controlled_shared_birth_after_entry(
        thread: i32,
        before: impl FnOnce(
            &crate::network_runtime::NetworkRuntimeResources,
            &Arc<Self>,
            &crate::network_runtime::JoinedNativePrefix,
        ),
    ) -> policy_tests::SharedBirthFixture {
        policy_tests::SharedBirthFixture::new_after_entry(thread, before).await
    }
    /// Controlled completed census plus an owned local accepted endpoint. No
    /// provider command is submitted; terminal retirement must still traverse
    /// the production accepted-runtime branch, not the guard-only early return.
    #[cfg(test)]
    pub(crate) fn controlled_terminal_runtime(
        thread: i32,
    ) -> (ControlledRuntimeFixture, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let (mut runtime, root, metadata, memory, claim) = controlled_foreground_runtime(thread);
        let shared = Arc::get_mut(&mut runtime.shared).unwrap();
        assert!(shared.endpoint.is_none());
        shared.endpoint = Some(unsafe { OwnedFd::from_raw_fd(pair[0]) });
        shared.copy_wire = Some(super::super::ProviderWireFormat::Abi9Copy5);
        ((runtime, root, metadata, memory, claim), unsafe {
            OwnedFd::from_raw_fd(pair[1])
        })
    }
    pub(in crate::network_runtime) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    pub(crate) fn association(&self) -> &InitialTableAssociation {
        &self.association
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn logical_process(&self) -> crate::types::DetPid {
        self._logical_process
    }
    pub(crate) fn process(&self) -> i32 {
        self.process
    }
    pub(crate) fn thread(&self) -> i32 {
        self.thread
    }
    pub(crate) fn files(&self) -> crate::types::FilesId {
        self.association.files
    }
    pub(crate) fn is_current(&self, owner: NetworkStreamOwner) -> bool {
        !self.revoked.load(Ordering::Acquire)
            && self.owner() == owner
            && self.initial_exec.is_none_or(|exec| {
                exec.mm.for_exec(exec.process) == owner.mm && exec.new_files == self.files()
            })
            && self
                .parent
                .as_ref()
                .is_none_or(|parent| parent.files() == self.files())
    }
    pub(crate) fn native_identity(&self) -> (u64, u64, u64, u64) {
        self.native_identity
    }
    /// Retained initial census/exec provenance with no subsequent physical
    /// sibling or replacement. Terminal cleanup may inspect this history even
    /// after this task exits; it cannot authorize a new guest operation.
    pub(crate) fn has_sole_initial_root_history(&self) -> bool {
        self.initial_exec.is_some()
            && self.parent.is_none()
            && self.owner == self.association.owner
            && !self.sole_initial_root_lost.load(Ordering::Acquire)
    }
    pub(crate) fn is_sole_initial_root(&self, owner: NetworkStreamOwner) -> bool {
        self.is_current(owner) && self.has_sole_initial_root_history()
    }
    pub(crate) fn metadata(&self) -> std::io::Result<Arc<Mutex<FileMetadata>>> {
        if !self.is_current(self.owner()) {
            return Err(std::io::Error::other("foreground root revoked"));
        }
        self.metadata
            .upgrade()
            .ok_or_else(|| std::io::Error::other("foreground root lost actual FD metadata"))
    }
    pub(crate) fn memory(&self) -> std::io::Result<Arc<Mutex<MemoryMetadata>>> {
        if !self.is_current(self.owner()) {
            return Err(std::io::Error::other("foreground root revoked"));
        }
        self.memory
            .upgrade()
            .ok_or_else(|| std::io::Error::other("foreground root lost actual MM metadata"))
    }
    pub(crate) fn matches_metadata(&self, actual: &Arc<Mutex<FileMetadata>>) -> bool {
        self.metadata.ptr_eq(&Arc::downgrade(actual))
    }
    pub(crate) fn matches_memory(&self, actual: &Arc<Mutex<MemoryMetadata>>) -> bool {
        self.memory.ptr_eq(&Arc::downgrade(actual))
    }
    /// Exact in-process proof that two current task roots name the same native
    /// address space and the same non-serialized memory ledger.  This permits a
    /// CLONE_VM sibling to use a mapping observed from its creator without
    /// treating equal numeric MM IDs or independently reconstructed metadata as
    /// authority.
    #[cfg(test)]
    pub(crate) fn same_memory_authority(&self, other: &ForegroundRoot) -> bool {
        self.is_current(self.owner())
            && other.is_current(other.owner())
            && self.owner().mm == other.owner().mm
            && self.process == other.process
            && self.memory.ptr_eq(&other.memory)
    }
}

impl<T> CustodyTasks<T> {
    pub(in crate::network_runtime) fn revoke_foreground_lineage(&mut self) {
        self.foreground_lineage_lost = true;
        self.sole_initial_root_lost.store(true, Ordering::Release);
        for task in self.tasks.values() {
            if let Some(root) = &task.foreground_root {
                root.revoke();
            }
        }
    }
    /// Called from the real state-ready observation after the same FD metadata
    /// was authenticated. Does not issue for a noninitial or partial census.
    pub(in crate::network_runtime) fn bind_foreground_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        metadata: &Arc<Mutex<FileMetadata>>,
        memory: &Arc<Mutex<MemoryMetadata>>,
    ) -> std::io::Result<()> {
        if self.foreground_lineage_lost {
            return Ok(());
        }
        let sole_initial_root_lost = self.sole_initial_root_lost.clone();
        self.task_mut(owner)?.foreground_metadata =
            Some((Arc::downgrade(metadata), Arc::downgrade(memory)));
        let (retired, birth, process, thread, old_root) = {
            let task = self.tasks.get(&owner.thread).expect("validated task");
            (
                task.retired,
                task.native_birth.clone(),
                task.process,
                task.thread,
                task.foreground_root.clone(),
            )
        };
        if retired {
            return Ok(());
        }
        if let Some(birth) = birth {
            let raw = birth.raw();
            if birth.child_owner() != owner
                || raw.shared_mm != 1
                || raw.shared_files != 1
                || raw.same_thread_group != 1
            {
                return Ok(());
            }
            let parent = self
                .tasks
                .get(&birth.permit().owner.thread)
                .and_then(|task| task.foreground_root.clone())
                .ok_or_else(|| {
                    std::io::Error::other("foreground child lost its authenticated creator root")
                })?;
            if parent.files() != birth.permit().files
                || !parent.matches_metadata(metadata)
                || !parent.matches_memory(memory)
                || process != parent.process()
                || thread != owner.thread.as_raw()
            {
                return Err(std::io::Error::other(
                    "foreground child changed shared MM/files or physical task identity",
                ));
            }
            if let Some(old) = &old_root {
                if !old.is_current(owner)
                    || !old.metadata.ptr_eq(&Arc::downgrade(metadata))
                    || !old.memory.ptr_eq(&Arc::downgrade(memory))
                {
                    return Err(std::io::Error::other(
                        "foreground child changed its state-ready Arc",
                    ));
                }
                return Ok(());
            }
            self.tasks.get_mut(&owner.thread).unwrap().foreground_root =
                Some(Arc::new(ForegroundRoot {
                    association: parent.association.clone(),
                    owner,
                    _logical_process: birth.child_process(),
                    process,
                    thread,
                    native_identity: (
                        raw.provider,
                        raw.child_task,
                        raw.child_start,
                        raw.child_table,
                    ),
                    initial_exec: None,
                    parent: Some(parent),
                    metadata: Arc::downgrade(metadata),
                    memory: Arc::downgrade(memory),
                    revoked: AtomicBool::new(false),
                    sole_initial_root_lost,
                }));
            return Ok(());
        }
        let task = self.task_mut(owner)?;
        if task.process != task.thread {
            return Ok(());
        }
        let Some(exec) = task.initial_exec else {
            return Ok(());
        };
        let Some(e) = &task.enrollment else {
            return Ok(());
        };
        let Some(association) = &e.association else {
            return Ok(());
        };
        let Some((claim, Ok(()))) = &e.semantic else {
            return Ok(());
        };
        if !e.settled
            || e.unresolved()
            || association.owner != owner
            || association.files != exec.new_files
            || association.enrollment.references != 1
            || association.enrollment.expected_table != 0
            || association.enrollment.mode != 1
        {
            return Err(std::io::Error::other(
                "foreground root lacks completed sole-table census",
            ));
        }
        association.validate_root_identity()?;
        association.check_claim(claim)?;
        if let Some(old) = &task.foreground_root {
            if !old.is_current(owner)
                || !old.metadata.ptr_eq(&Arc::downgrade(metadata))
                || !old.memory.ptr_eq(&Arc::downgrade(memory))
            {
                return Err(std::io::Error::other(
                    "foreground root changed its state-ready Arc",
                ));
            }
            return Ok(());
        }
        task.foreground_root = Some(Arc::new(ForegroundRoot {
            association: association.clone(),
            owner,
            _logical_process: owner.thread,
            process: task.process,
            thread: task.thread,
            native_identity: (
                association.provider,
                association.enrollment.task,
                association.enrollment.task_start,
                association.enrollment.table,
            ),
            initial_exec: Some(exec),
            parent: None,
            metadata: Arc::downgrade(metadata),
            memory: Arc::downgrade(memory),
            revoked: AtomicBool::new(false),
            sole_initial_root_lost,
        }));
        Ok(())
    }
    pub(in crate::network_runtime) fn foreground_root(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Arc<ForegroundRoot>> {
        self.get(owner)?;
        if self.foreground_lineage_lost {
            return Err(std::io::Error::other(
                "foreground ctl lineage was revoked by an unsupported physical owner",
            ));
        }
        let task = &self.tasks[&owner.thread];
        let root = task
            .foreground_root
            .as_ref()
            .filter(|root| root.is_current(owner))
            .ok_or_else(|| {
                std::io::Error::other("foreground ctl lacks positive initial-root authority")
            })?;
        Ok(root.clone())
    }
}

// Explicit component premises using the existing census/semantic issuer. They
// do not represent a native receipt or authorize production via configuration.
#[cfg(test)]
type ControlledTaskFixture = (
    CustodyTasks<u64>,
    NetworkStreamOwner,
    Arc<Mutex<FileMetadata>>,
    Arc<Mutex<MemoryMetadata>>,
    InitialTableClaim,
);
#[cfg(test)]
type ControlledRootFixture = (
    Arc<ForegroundRoot>,
    Arc<Mutex<FileMetadata>>,
    Arc<Mutex<MemoryMetadata>>,
    InitialTableClaim,
);
#[cfg(test)]
type ControlledRuntimeFixture = (
    super::super::NetworkRuntimeResources,
    Arc<ForegroundRoot>,
    Arc<Mutex<FileMetadata>>,
    Arc<Mutex<MemoryMetadata>>,
    InitialTableClaim,
);

#[cfg(test)]
fn controlled_tasks(thread: i32) -> ControlledTaskFixture {
    let thread = DetTid::from_raw(thread);
    let before = MmId::initial(thread);
    let owner = NetworkStreamOwner {
        thread,
        mm: before.for_exec(thread),
    };
    let files = crate::types::FilesIdAllocator::default().allocate_exec(thread);
    let exec = ExecFilesReceipt {
        caller: thread,
        process: thread,
        mm: before,
        old_files: FilesId::initial(thread),
        new_files: files,
    };
    let mut tasks = CustodyTasks::default();
    tasks
        .register(owner, thread.as_raw(), thread.as_raw(), || Ok(99))
        .unwrap();
    tasks.bind_initial_exec(owner, exec).unwrap();
    tasks.begin_initial(owner).unwrap();
    let (mut association, mut claim) = super::initial_root_fixture(owner, thread.as_raw());
    association.files = files;
    association.enrollment.references = 1;
    association.enrollment.mode = 1;
    claim.view = association.view();
    let enrollment = tasks.enrollment(owner).unwrap();
    enrollment.association = Some(association);
    enrollment.settled = true;
    tasks
        .observe_initial_metadata(owner, |_, _, _| Ok(Vec::new()))
        .unwrap();
    tasks
        .admit_semantics(owner, claim.clone(), |_, _, _, _| Ok(()))
        .unwrap();
    let mut metadata = FileMetadata::empty_network_fixture(thread);
    metadata.files_id = files;
    (
        tasks,
        owner,
        Arc::new(Mutex::new(metadata)),
        Arc::new(Mutex::new(MemoryMetadata::new())),
        claim,
    )
}
#[cfg(test)]
pub(crate) fn controlled_foreground_root(thread: i32) -> ControlledRootFixture {
    let (mut tasks, owner, metadata, memory, claim) = controlled_tasks(thread);
    tasks
        .bind_foreground_metadata(owner, &metadata, &memory)
        .unwrap();
    (
        tasks.foreground_root(owner).unwrap(),
        metadata,
        memory,
        claim,
    )
}

#[cfg(test)]
pub(crate) fn controlled_foreground_runtime(thread: i32) -> ControlledRuntimeFixture {
    runtime_from_controlled_tasks(thread, controlled_tasks(thread))
}

#[cfg(test)]
fn runtime_from_controlled_tasks(
    thread: i32,
    fixture: ControlledTaskFixture,
) -> ControlledRuntimeFixture {
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    let (mut tasks, owner, metadata, memory, claim) = fixture;
    tasks
        .bind_foreground_metadata(owner, &metadata, &memory)
        .unwrap();
    let root = tasks.foreground_root(owner).unwrap();
    let (runtime, _) = super::super::tests::fixture(94);
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, thread, libc::O_EXCL) };
    assert!(
        raw >= 0,
        "controlled current-process PIDFD_THREAD: {}",
        std::io::Error::last_os_error()
    );
    let task = tasks.tasks.remove(&owner.thread).unwrap();
    let mut actual = runtime.shared.physical.lock().unwrap();
    actual.first_task = tasks.first_task;
    actual.next_registration = tasks.next_registration;
    actual.sole_initial_root_lost = tasks.sole_initial_root_lost.clone();
    actual.tasks.insert(
        owner.thread,
        Task {
            mm: task.mm,
            process: task.process,
            thread: task.thread,
            handle: unsafe { OwnedFd::from_raw_fd(raw as i32) },
            initial_exec: task.initial_exec,
            foreground_root: task.foreground_root,
            retired: task.retired,
            enrollment: task.enrollment,
            native_birth: task.native_birth,
            foreground_metadata: task.foreground_metadata,
        },
    );
    drop(actual);
    (runtime, root, metadata, memory, claim)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreground_root_requires_actual_completed_exec_census_and_same_metadata_arcs() {
        for mutation in 0..7 {
            let (mut tasks, owner, metadata, memory, _) = controlled_tasks(61);
            let task = tasks.tasks.get_mut(&owner.thread).unwrap();
            match mutation {
                0 => task.initial_exec = None,
                1 => task.enrollment.as_mut().unwrap().settled = false,
                2 => task.enrollment.as_mut().unwrap().semantic = None,
                3 => {
                    task.enrollment
                        .as_mut()
                        .unwrap()
                        .semantic
                        .as_mut()
                        .unwrap()
                        .1 = Err("failed admission".into())
                }
                4 => {
                    task.enrollment
                        .as_mut()
                        .unwrap()
                        .association
                        .as_mut()
                        .unwrap()
                        .enrollment
                        .references = 2
                }
                5 => {
                    task.enrollment
                        .as_mut()
                        .unwrap()
                        .association
                        .as_mut()
                        .unwrap()
                        .enrollment
                        .mode = 0
                }
                6 => {
                    task.enrollment
                        .as_mut()
                        .unwrap()
                        .association
                        .as_mut()
                        .unwrap()
                        .enrollment
                        .expected_table = 7
                }
                _ => unreachable!(),
            }
            let _ = tasks.bind_foreground_metadata(owner, &metadata, &memory);
            assert!(
                tasks.foreground_root(owner).is_err(),
                "premise {mutation} issued authority"
            );
        }
        let (mut tasks, owner, metadata, memory, _) = controlled_tasks(61);
        tasks
            .bind_foreground_metadata(owner, &metadata, &memory)
            .unwrap();
        let root = tasks.foreground_root(owner).unwrap();
        assert!(root.is_current(owner));
        tasks
            .bind_foreground_metadata(owner, &metadata, &memory)
            .unwrap();
        let copied = Arc::new(Mutex::new(memory.lock().unwrap().clone()));
        assert!(
            tasks
                .bind_foreground_metadata(owner, &metadata, &copied)
                .is_err()
        );
        assert!(Arc::ptr_eq(&root, &tasks.foreground_root(owner).unwrap()));
    }
    #[test]
    fn foreground_root_keeps_live_parent_but_loses_sole_policy_after_child_registration() {
        let (mut tasks, owner, metadata, memory, _) = controlled_tasks(61);
        tasks
            .bind_foreground_metadata(owner, &metadata, &memory)
            .unwrap();
        let root = tasks.foreground_root(owner).unwrap();
        assert!(root.is_sole_initial_root(owner));
        let child = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: owner.mm,
        };
        tasks.register(child, 61, 62, || Ok(101)).unwrap();
        assert!(root.is_current(owner));
        assert!(Arc::ptr_eq(&root, &tasks.foreground_root(owner).unwrap()));
        assert!(!root.is_sole_initial_root(owner));
        assert!(
            tasks.foreground_root(child).is_err(),
            "registration is not birth authority"
        );
        tasks.forget(child);
        assert!(
            !root.is_sole_initial_root(owner),
            "forget cannot erase physical history"
        );
    }
}
