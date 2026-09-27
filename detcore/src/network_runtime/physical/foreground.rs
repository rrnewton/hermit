//! Positive initial-root lineage, retained in the existing physical Task.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;
use crate::memory::MemoryMetadata;
use crate::tool_local::FileMetadata;

/// Private census/EXEC provenance. Neither serde, a config bit nor a numeric
/// task/file identifier constructs this authority. Revocation is irreversible.
#[derive(Debug)]
pub(crate) struct ForegroundRoot {
    association: InitialTableAssociation,
    exec: ExecFilesReceipt,
    metadata: Weak<Mutex<FileMetadata>>,
    memory: Weak<Mutex<MemoryMetadata>>,
    revoked: AtomicBool,
}
impl ForegroundRoot {
    pub(crate) fn association(&self) -> &InitialTableAssociation {
        &self.association
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.association.owner()
    }
    pub(crate) fn files(&self) -> crate::types::FilesId {
        self.association.files
    }
    pub(crate) fn is_current(&self, owner: NetworkStreamOwner) -> bool {
        !self.revoked.load(Ordering::Acquire)
            && self.owner() == owner
            && self.exec.mm.for_exec(self.exec.process) == owner.mm
            && self.exec.new_files == self.files()
    }
    pub(crate) fn native_identity(&self) -> (u64, u64, u64, u64) {
        (
            self.association.provider,
            self.association.enrollment.task,
            self.association.enrollment.task_start,
            self.association.enrollment.table,
        )
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
}

impl<T> CustodyTasks<T> {
    pub(in crate::network_runtime) fn revoke_foreground_lineage(&mut self) {
        self.foreground_lineage_lost = true;
        for task in self.tasks.values() {
            if let Some(root) = &task.foreground_root {
                root.revoked.store(true, Ordering::Release);
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
        if self.foreground_lineage_lost || self.tasks.len() != 1 {
            return Ok(());
        }
        let task = self.task_mut(owner)?;
        if task.retired || task.native_birth.is_some() || task.process != task.thread {
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
            exec,
            metadata: Arc::downgrade(metadata),
            memory: Arc::downgrade(memory),
            revoked: AtomicBool::new(false),
        }));
        Ok(())
    }
    pub(in crate::network_runtime) fn foreground_root(
        &self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<Arc<ForegroundRoot>> {
        self.get(owner)?;
        if self.foreground_lineage_lost || self.tasks.len() != 1 {
            return Err(std::io::Error::other(
                "foreground ctl requires unchanged single-root lineage",
            ));
        }
        let task = &self.tasks[&owner.thread];
        let root = task
            .foreground_root
            .as_ref()
            .filter(|root| root.is_current(owner) && task.native_birth.is_none())
            .ok_or_else(|| {
                std::io::Error::other("foreground ctl lacks positive initial-root authority")
            })?;
        Ok(root.clone())
    }
}

// Explicit component premises using the existing census/semantic issuer. They
// do not represent a native receipt or authorize production via configuration.
#[cfg(test)]
fn controlled_tasks(
    thread: i32,
) -> (
    CustodyTasks<u64>,
    NetworkStreamOwner,
    Arc<Mutex<FileMetadata>>,
    Arc<Mutex<MemoryMetadata>>,
    InitialTableClaim,
) {
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
pub(crate) fn controlled_foreground_root(
    thread: i32,
) -> (
    Arc<ForegroundRoot>,
    Arc<Mutex<FileMetadata>>,
    Arc<Mutex<MemoryMetadata>>,
    InitialTableClaim,
) {
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
    fn foreground_root_cannot_survive_child_exec_final_wait_or_registration_reuse() {
        for transition in 0..4 {
            let (mut tasks, owner, metadata, memory, _) = controlled_tasks(61);
            tasks
                .bind_foreground_metadata(owner, &metadata, &memory)
                .unwrap();
            let root = tasks.foreground_root(owner).unwrap();
            match transition {
                0 => {
                    let child = NetworkStreamOwner {
                        thread: DetTid::from_raw(62),
                        mm: owner.mm,
                    };
                    tasks.register(child, 61, 62, || Ok(101)).unwrap();
                }
                1 => {
                    let next = NetworkStreamOwner {
                        mm: owner.mm.for_exec(owner.thread),
                        ..owner
                    };
                    tasks.register(next, 61, 61, || Ok(102)).unwrap();
                }
                2 => tasks.close_native_preparations(owner).unwrap(),
                3 => tasks.forget(owner),
                _ => unreachable!(),
            }
            assert!(!root.is_current(owner));
            assert!(tasks.foreground_root(owner).is_err());
        }
    }
}

#[cfg(test)]
pub(crate) fn controlled_foreground_runtime(
    thread: i32,
) -> (
    super::super::NetworkRuntimeResources,
    Arc<ForegroundRoot>,
    Arc<Mutex<FileMetadata>>,
    Arc<Mutex<MemoryMetadata>>,
    InitialTableClaim,
) {
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    let (mut tasks, owner, metadata, memory, claim) = controlled_tasks(thread);
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
        },
    );
    drop(actual);
    (runtime, root, metadata, memory, claim)
}
