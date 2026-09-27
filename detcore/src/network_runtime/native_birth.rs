//! Original clone admission from the existing command, journal and backend
//! pending-child owner. No RPC field or historical row alone grants admission.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;

use reverie::syscalls::CloneFlags;

use super::NetworkRuntimeResources;
use super::accepted_controller::Effect;
use super::accepted_provider::NativeBirth;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::fd_journal::Transition;
use crate::network_replay::NetworkFdPublicationPermit;
use crate::network_replay::NetworkStreamOwner;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::MmId;

/// Constructed only by the consuming preconstruction path after its retained
/// request and complete journal have been checked. This is NOT a current-slot
/// capability and cannot be serialized or manufactured through Global RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeBirthAdmission {
    permit: NetworkFdPublicationPermit,
    child: DetTid,
    process: DetPid,
    flags: CloneFlags,
    terminal: bool,
    raw: NativeBirth,
}
impl NativeBirthAdmission {
    pub(crate) fn raw(&self) -> &NativeBirth {
        &self.raw
    }
    pub(crate) fn permit(&self) -> NetworkFdPublicationPermit {
        self.permit
    }
    pub(crate) fn flags(&self) -> CloneFlags {
        self.flags
    }
    pub(crate) fn actual_flags(&self) -> CloneFlags {
        CloneFlags::from_bits_retain(self.raw.kernel_flags)
    }
    pub(crate) fn shared_files(&self) -> bool {
        self.raw.shared_files == 1
    }
    pub(super) fn table(&self) -> u64 {
        self.raw.child_table
    }
    pub(crate) fn terminal(&self) -> bool {
        self.terminal
    }
    pub(crate) fn child_process(&self) -> DetPid {
        self.process
    }
    pub(crate) fn child_owner(&self) -> NetworkStreamOwner {
        NetworkStreamOwner {
            thread: self.child,
            mm: MmId::for_clone(self.permit.owner.mm, self.child, self.raw.shared_mm == 1),
        }
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn check_birth(
    raw: &NativeBirth,
    permit: NetworkFdPublicationPermit,
    command: u64,
    table: u64,
    flags: CloneFlags,
    syscall: i32,
) -> io::Result<()> {
    if raw.command != command
        || raw.call != permit.native_command_call()
        || raw.owner_mm != permit.owner.mm.generation()
        || raw.provider == 0
        || raw.creator_task == 0
        || raw.creator_start == 0
        || raw.creator_table != table
        || raw.child_task == 0
        || raw.child_start == 0
        || raw.child_task == raw.creator_task
        || raw.child_table == 0
        || raw.parent_task == 0
        || raw.parent_start == 0
        || raw.ready != 1
        || raw.problem != 0
        || (raw.kernel_flags & 0x200000 == 0 && raw.clear_child_tid != 0)
        || raw.shared_mm > 1
        || raw.shared_files > 1
        || raw.same_thread_group > 1
        || raw.shared_mm != u32::from(raw.kernel_flags & 0x100 != 0)
        || raw.shared_files != u32::from(raw.kernel_flags & 0x400 != 0)
        || raw.same_thread_group != u32::from(raw.kernel_flags & 0x10000 != 0)
        || raw.same_thread_group != u32::from(raw.creator_task >> 32 == raw.child_task >> 32)
        || !(-1..=64).contains(&raw.exit_signal)
        || !(0..=64).contains(&raw.requested_exit_signal)
    {
        return Err(invalid(
            "native birth changed exact command/task/physical inheritance",
        ));
    }
    // Until every common clone3 consumer can take actual kernel facts, a
    // contradiction is an explicit failed execution, not speculative Tool
    // construction. This does not claim support of a valid raced clone3 call.
    let expected_flags = match syscall {
        435 => flags.bits(), // clone3 exit_signal is a separate kernel-copied field.
        56 | 57 | 58 => flags.bits() & !0xff, // legacy family embeds CSIGNAL.
        _ => return Err(invalid("birth names another syscall shape")),
    };
    if raw.kernel_flags != expected_flags {
        return Err(invalid(
            "kernel clone flags differ from retained Tool construction metadata",
        ));
    }
    if raw.shared_files == 1 {
        if raw.child_table != raw.creator_table || raw.copy_begin != 0 || raw.copy_end != 0 {
            return Err(invalid("shared birth contains a copied-table claim"));
        }
    } else if raw.child_table == raw.creator_table
        || raw.copy_begin == 0
        || raw.copy_end <= raw.copy_begin
    {
        return Err(invalid("copied birth lacks complete original table census"));
    }
    if raw.kernel_flags & 0x1000 != 0 {
        if raw.pidfd_install_begin == 0
            || raw.pidfd_install_end <= raw.pidfd_install_begin
            || raw.pidfd_file == 0
            || raw.pidfd_fd < 0
        {
            return Err(invalid("CLONE_PIDFD lacks original installation"));
        }
    } else if raw.pidfd_install_begin != 0
        || raw.pidfd_install_end != 0
        || raw.pidfd_file != 0
        || raw.pidfd_fd != -1
    {
        return Err(invalid("non-PIDFD birth claims an installation"));
    }
    Ok(())
}
/// Actual common cleanup inputs, retained on the original provider preparation.
/// These are not consumption facts and cannot authorize a child or native result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeBirthCleanupRequest {
    Uninvoked {
        admission: crate::network_replay::NetworkFdMutationAdmission,
        marker: Option<crate::scheduler::UninvokedWaitCall>,
    },
    Failed {
        birth: crate::scheduler::NoSeqChildBirth,
        errno: i32,
    },
}
impl NativeBirthCleanupRequest {
    pub(super) fn permit(&self) -> NetworkFdPublicationPermit {
        match self {
            Self::Uninvoked { admission, .. } => admission.publication.permit,
            Self::Failed { birth, .. } => birth
                .fd_permit()
                .expect("retained native failure has clone permit"),
        }
    }
    pub(super) fn completion(&self) -> super::accepted_controller::BirthCompletion {
        match self {
            Self::Uninvoked { .. } => super::accepted_controller::BirthCompletion::Uninvoked,
            Self::Failed { errno, .. } => {
                super::accepted_controller::BirthCompletion::Returned(Err(*errno))
            }
        }
    }
}

/// Handles to the existing common owners, not a second cleanup queue or task.
/// The driver invokes the original engine/scheduler consumers under their usual
/// lock order after positively validating this request's physical completion.
pub(crate) struct NativeBirthRecovery {
    sched: std::sync::Arc<std::sync::Mutex<crate::scheduler::Scheduler>>,
    engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
    exec_changed: std::sync::Arc<tokio::sync::Notify>,
    network_changed: std::sync::Arc<tokio::sync::Notify>,
    retire_ports: Box<dyn Fn(Vec<detcore_model::fd::OpenFileId>) + Send + Sync>,
}
impl NativeBirthRecovery {
    pub(crate) fn new(
        sched: std::sync::Arc<std::sync::Mutex<crate::scheduler::Scheduler>>,
        engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
        exec_changed: std::sync::Arc<tokio::sync::Notify>,
        network_changed: std::sync::Arc<tokio::sync::Notify>,
        retire_ports: impl Fn(Vec<detcore_model::fd::OpenFileId>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            sched,
            engine,
            exec_changed,
            network_changed,
            retire_ports: Box::new(retire_ports),
        }
    }

    fn consume(
        &self,
        request: &NativeBirthCleanupRequest,
    ) -> io::Result<super::accepted_controller::BirthSemantic> {
        use super::accepted_controller::BirthSemantic;
        let mut sched = self.sched.lock().unwrap();
        let mut engine = self.engine.lock().unwrap();
        let semantic = match request {
            NativeBirthCleanupRequest::Uninvoked { admission, marker } => {
                let owner = admission.publication.permit.owner;
                if marker
                    .as_ref()
                    .is_some_and(|m| !sched.thread_tree.validate_uninvoked_wait_call(owner, m))
                {
                    return Err(invalid(
                        "retained uninvoked marker changed before actual cleanup",
                    ));
                }
                engine
                    .validate_uninvoked_clone_admission(admission)
                    .map_err(|e| invalid(&e.to_string()))?;
                engine
                    .cancel_uninvoked_clone_admission(admission)
                    .map_err(|e| invalid(&e.to_string()))?;
                if !sched
                    .thread_tree
                    .retire_no_seq_wait_owner(owner, marker.as_ref())
                {
                    return Err(invalid("validated uninvoked birth lost scheduler cleanup"));
                }
                BirthSemantic::Uninvoked
            }
            NativeBirthCleanupRequest::Failed { birth, errno } => {
                if !sched.thread_tree.failed_birth_matches(birth, *errno) {
                    return Err(invalid(
                        "retained native errno changed exact birth reservation",
                    ));
                }
                engine
                    .settle_failed_cloned_fd_table(request.permit(), birth.flags(), *errno)
                    .map_err(|e| invalid(&e.to_string()))?;
                if !sched.thread_tree.cancel_no_seq_birth(birth) {
                    return Err(invalid("validated native errno lost scheduler cleanup"));
                }
                BirthSemantic::Failed
            }
        };
        let retired = engine.take_lifetime_retired_ports();
        drop(engine);
        drop(sched);
        (self.retire_ports)(retired.into_iter().collect());
        self.exec_changed.notify_waiters();
        self.network_changed.notify_waiters();
        Ok(semantic)
    }
}

/// This continuation lives inside the existing PrepareNativeBirth entry. The
/// result lock serializes driver/finalization callbacks; it is never held across
/// a Global await. Failed/partially consumed cleanup keeps its original inputs.
pub(crate) struct NativeBirthCleanup {
    pub(super) request: NativeBirthCleanupRequest,
    recovery: NativeBirthRecovery,
    outcome: std::sync::Mutex<Option<Result<(), String>>>,
}
impl std::fmt::Debug for NativeBirthCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBirthCleanup")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}
impl NativeBirthCleanup {
    pub(super) fn new(request: NativeBirthCleanupRequest, recovery: NativeBirthRecovery) -> Self {
        Self {
            request,
            recovery,
            outcome: std::sync::Mutex::new(None),
        }
    }
    pub(super) fn result(&self) -> Option<Result<(), String>> {
        self.outcome.lock().unwrap().clone()
    }
    pub(super) fn progress(
        &self,
        controller: &super::accepted_controller::Controller,
    ) -> io::Result<()> {
        let mut outcome = self.outcome.lock().unwrap();
        if let Some(result) = outcome.as_ref() {
            return result.clone().map_err(io::Error::other);
        }
        let result: io::Result<bool> = (|| {
            if !controller.poll_native_birth_cleanup(&self.request)? {
                return Ok(false);
            }
            let semantic = self.recovery.consume(&self.request)?;
            controller.native_birth_semantics_consumed(self.request.permit(), semantic)?;
            Ok(true)
        })();
        match result {
            Ok(false) => Ok(()),
            Ok(true) => {
                *outcome = Some(Ok(()));
                controller.notify_birth_cleanup();
                Ok(())
            }
            Err(error) => {
                *outcome = Some(Err(error.to_string()));
                controller.notify_birth_cleanup();
                Err(error)
            }
        }
    }
}

impl NetworkRuntimeResources {
    /// Called synchronously while the common scheduler validates the actual
    /// marker/errno. Ownership transfers before any physical operation awaits.
    pub(crate) fn retain_native_birth_cleanup(
        &self,
        request: NativeBirthCleanupRequest,
        recovery: NativeBirthRecovery,
    ) -> io::Result<Option<std::sync::Arc<NativeBirthCleanup>>> {
        if self.shared.endpoint.is_none() {
            return Ok(None);
        }
        self.accepted_controller()?
            .retain_native_birth_cleanup(request, recovery)
    }
    pub(crate) async fn wait_native_birth_cleanup(
        &self,
        cleanup: &NativeBirthCleanup,
    ) -> io::Result<()> {
        self.accepted_controller()?
            .wait_native_birth_cleanup(cleanup)
            .await
    }

    /// The caller is the distinct authenticated actual-final-wait observer.
    /// This mutex is also held through Controller::prepare before its first
    /// await, so a lost preparation reply cannot escape terminal ownership.
    pub(crate) fn native_birth_creator_terminal(
        &self,
        owner: NetworkStreamOwner,
    ) -> io::Result<()> {
        // Guard-only resources cannot have issued an accepted command. Do not
        // create a controller or turn ordinary provider-absent exit into failure.
        if self.shared.endpoint.is_none() {
            return Ok(());
        }
        let mut tasks = self.shared.physical.lock().unwrap();
        tasks.close_native_preparations(owner)?;
        let retained = self.shared.controller.lock().unwrap();
        match retained.as_ref() {
            Some(Ok(controller)) => controller.native_birth_creator_terminal(owner),
            Some(Err(error)) => Err(invalid(error)),
            None => Ok(()), // physical admission is closed before a future prepare
        }
    }
    pub(crate) async fn prepare_native_birth(
        &self,
        permit: NetworkFdPublicationPermit,
        syscall: i32,
    ) -> io::Result<()> {
        let controller = self.accepted_controller()?;
        let sequence = {
            let tasks = self.shared.physical.lock().unwrap();
            let table = tasks.native_table(permit.owner)?;
            controller.prepare(
                Effect::PrepareNativeBirth(permit.lease),
                permit.owner,
                &Request::PrepareNativeBirth {
                    call: permit.native_command_call(),
                    mm: permit.owner.mm.generation(),
                    table,
                    syscall,
                },
                || Ok(vec![tasks.get(permit.owner)?.as_fd().try_clone_to_owned()?]),
            )?
        };
        controller.response(sequence).await?;
        controller.native_birth_preparation(permit)?;
        Ok(())
    }

    /// Only Detcore's actual backend NewChild callback invokes this method.
    /// The backend owns the original creator state and child generation across
    /// every await; terminal means its actual final wait, never mere absence.
    pub(crate) async fn observe_native_birth(
        &self,
        permit: NetworkFdPublicationPermit,
        child: DetTid,
        creator_process: DetPid,
        pin: BorrowedFd<'_>,
        terminal: bool,
        flags: CloneFlags,
    ) -> io::Result<NativeBirthAdmission> {
        if child == permit.owner.thread || child.as_raw() <= 0 {
            return Err(invalid("native child identity invalid"));
        }
        let controller = self.accepted_controller()?;
        let (prepared_request, command, table, syscall) =
            controller.native_birth_preparation(permit)?;
        let sequence = controller.prepare(
            Effect::ObserveNativeBirth(permit.lease),
            permit.owner,
            &Request::ObserveNativeBirth {
                call: permit.native_command_call(),
                command,
                prepared_request,
                child: child.as_raw(),
                terminal,
            },
            || Ok(vec![pin.try_clone_to_owned()?]),
        )?;
        let Reply::NativeBirth(observation) = controller.response(sequence).await? else {
            return Err(invalid("native child observation changed response kind"));
        };
        if observation.status.returned != 0 || observation.status.errno.is_some() {
            return Err(invalid("provider could not bind exact native child"));
        }
        let raw = observation.raw;
        check_birth(&raw, permit, command, table, flags, syscall)?;
        let endpoint = raw.copy_end.max(raw.pidfd_install_end);
        if endpoint != 0 {
            let mut journal = self.shared.fd_journal.lock().await;
            journal.through(&controller, permit.owner, endpoint).await?;
            if raw.copy_end != 0 {
                let Some(Transition::Copy { begin, end, .. }) =
                    journal.history().transition(raw.copy_end)?
                else {
                    return Err(invalid(
                        "native birth copy is not a complete COPY transition",
                    ));
                };
                if begin.sequence != raw.copy_begin
                    || begin.table != raw.creator_table
                    || end.table != raw.child_table
                    || begin.task != raw.creator_task
                    || begin.task_start != raw.creator_start
                    || end.returned < 0
                {
                    return Err(invalid(
                        "native birth changed original complete copy interval",
                    ));
                }
            }
            if raw.pidfd_install_end != 0 {
                let Some(Transition::Install { begin, end }) =
                    journal.history().transition(raw.pidfd_install_end)?
                else {
                    return Err(invalid(
                        "native birth PIDFD is not a complete INSTALL transition",
                    ));
                };
                if begin.sequence != raw.pidfd_install_begin
                    || end.table != raw.creator_table
                    || end.file != raw.pidfd_file
                    || end.fd != raw.pidfd_fd
                    || begin.task != raw.creator_task
                    || begin.task_start != raw.creator_start
                {
                    return Err(invalid("native birth changed original PIDFD installation"));
                }
            }
        }
        let process = if raw.same_thread_group == 1 {
            creator_process
        } else {
            child
        };
        Ok(NativeBirthAdmission {
            permit,
            child,
            process,
            flags,
            terminal,
            raw,
        })
    }

    pub(crate) fn retain_native_child(
        &self,
        birth: &NativeBirthAdmission,
        pin: BorrowedFd<'_>,
    ) -> io::Result<()> {
        if birth.terminal {
            return Ok(());
        }
        let owner = birth.child_owner();
        let mut tasks = self.shared.physical.lock().unwrap();
        tasks.register(owner, birth.process.as_raw(), birth.child.as_raw(), || {
            pin.try_clone_to_owned()
        })?;
        tasks.bind_native_child(birth)
    }

    pub(crate) async fn collect_native_birth(
        &self,
        permit: NetworkFdPublicationPermit,
        returned: Result<i64, i32>,
    ) -> io::Result<()> {
        let controller = self.accepted_controller()?;
        let (prepared_request, command, _, _) = controller.native_birth_preparation(permit)?;
        let sequence = controller.prepare(
            Effect::CollectNativeBirth(permit.lease),
            permit.owner,
            &Request::CollectNativeBirth {
                call: permit.native_command_call(),
                command,
                prepared_request,
            },
            || Ok(vec![]),
        )?;
        let Reply::NativeBirthEffect(observation) = controller.response(sequence).await? else {
            return Err(invalid("native clone completion changed response kind"));
        };
        let native = match returned {
            Ok(value) => value,
            Err(errno) => -i64::from(errno),
        };
        if observation.status.returned != 0
            || observation.status.errno.is_some()
            || observation.raw.command.command != command
            || observation.raw.command.operation != 8
            || i64::from(observation.raw.command.returned) != native
        {
            return Err(invalid(
                "provider completion differs from original native clone result",
            ));
        }
        controller.native_birth_completion_consumed(
            permit,
            super::accepted_controller::BirthCompletion::Returned(returned),
        )
    }

    /// Called synchronously after the common engine/scheduler consumed the
    /// exact clone escrow. Failed calls leave request/PIDFD custody retained.
    pub(crate) fn native_birth_semantics_consumed(
        &self,
        permit: NetworkFdPublicationPermit,
        child: Option<(crate::types::DetTid, bool)>,
        uninvoked: bool,
    ) -> io::Result<()> {
        let controller = self.accepted_controller()?;
        // Ordinary non-network mutations have no provider command. This does
        // not convert an unresolved existing preparation into absence.
        if !controller.has_native_birth_preparation(permit)? {
            return if child.is_none() {
                Ok(())
            } else {
                Err(invalid("consumed native child lost original preparation"))
            };
        }
        let semantic = match (child, uninvoked) {
            (Some((child, terminal)), false) => super::accepted_controller::BirthSemantic::Child {
                child: child.as_raw(),
                terminal,
            },
            (None, false) => super::accepted_controller::BirthSemantic::Failed,
            (None, true) => super::accepted_controller::BirthSemantic::Uninvoked,
            _ => return Err(invalid("uninvoked birth cannot consume a child")),
        };
        controller.native_birth_semantics_consumed(permit, semantic)
    }
    pub(crate) fn native_child_birth(
        &self,
        child: NetworkStreamOwner,
    ) -> io::Result<NativeBirthAdmission> {
        self.shared.physical.lock().unwrap().native_birth(child)
    }
    pub(crate) fn native_child_semantics_consumed(
        &self,
        proof: &NativeBirthAdmission,
    ) -> io::Result<()> {
        self.native_birth_semantics_consumed(
            proof.permit(),
            Some((proof.child_owner().thread, false)),
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (NetworkFdPublicationPermit, NativeBirth) {
        let thread = DetTid::from_raw(41);
        let permit = NetworkFdPublicationPermit {
            owner: NetworkStreamOwner {
                thread,
                mm: MmId::initial(thread),
            },
            files: crate::types::FilesId::initial(thread),
            lease: serde_json::from_str("17").unwrap(),
        };
        // These are initial-namespace kernel IDs, intentionally unlike local41.
        let raw = super::super::accepted_provider_ffi::NativeBirth {
            command: 7,
            call: 17,
            owner_mm: permit.owner.mm.generation(),
            provider: 3,
            creator_task: (5001u64 << 32) | 5001,
            creator_start: 29,
            creator_table: 47,
            child_task: (5012u64 << 32) | 5012,
            child_start: 31,
            child_table: 53,
            parent_task: (5001u64 << 32) | 5001,
            parent_start: 29,
            copy_begin: 59,
            copy_end: 61,
            ready: 1,
            exit_signal: 17,
            requested_exit_signal: 17,
            pidfd_fd: -1,
            ..Default::default()
        }
        .into();
        (permit, raw)
    }
    #[test]
    fn birth_validation_binds_ticket_and_physical_inheritance_across_pid_namespaces() {
        let (permit, raw) = fixture();
        assert_ne!(raw.creator_task as u32, permit.owner.thread.as_raw() as u32);
        check_birth(&raw, permit, 7, 47, CloneFlags::empty(), 435).unwrap();
        let mut wrong = raw.clone();
        wrong.command += 1;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        wrong = raw.clone();
        wrong.owner_mm += 1;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        wrong = raw.clone();
        wrong.creator_start = 0;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        wrong = raw.clone();
        wrong.child_task = wrong.creator_task;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        wrong = raw.clone();
        wrong.copy_end = 0;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        wrong = raw.clone();
        wrong.ready = 0;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        wrong = raw;
        wrong.problem = 1;
        assert!(check_birth(&wrong, permit, 7, 47, CloneFlags::empty(), 435).is_err());
    }
    #[test]
    fn actual_kernel_shared_files_cannot_be_replaced_by_a_clone3_pre_read() {
        let (permit, mut raw) = fixture();
        raw.kernel_flags = CloneFlags::CLONE_FILES.bits();
        raw.shared_files = 1;
        raw.child_table = 47;
        raw.copy_begin = 0;
        raw.copy_end = 0;
        check_birth(&raw, permit, 7, 47, CloneFlags::CLONE_FILES, 435).unwrap();
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        raw.shared_mm = 1;
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::CLONE_FILES, 435).is_err());
        raw.shared_mm = 0;
        raw.child_table = 53;
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::CLONE_FILES, 435).is_err());
    }
    #[test]
    fn clone3_newtime_is_preserved_while_classic_signal_byte_is_normalized() {
        let (permit, mut raw) = fixture();
        raw.kernel_flags = CloneFlags::CLONE_NEWTIME.bits();
        check_birth(&raw, permit, 7, 47, CloneFlags::CLONE_NEWTIME, 435).unwrap();
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::empty(), 435).is_err());
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::CLONE_NEWTIME, 56).is_err());
        raw.kernel_flags = 0;
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::CLONE_NEWTIME, 435).is_err());
        for syscall in [56, 57] {
            check_birth(&raw, permit, 7, 47, CloneFlags::SIGCHLD, syscall).unwrap();
        }
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::SIGCHLD, 435).is_err());
        assert!(check_birth(&raw, permit, 7, 47, CloneFlags::empty(), 60).is_err());
    }
}

/// Transport fixture with the actual run-owned native driver. It supplies only
/// provider replies/PIDFD stand-ins; Global cleanup and its common consumers
/// run unchanged. No fixture inserts a semantic consumption marker.
#[cfg(test)]
pub(crate) struct NativeBirthCleanupPeer {
    runtime: super::NetworkRuntimeOwner,
    controller: std::sync::Arc<super::accepted_controller::Controller>,
    peer: super::accepted_transport::AcceptedSession,
    permit: NetworkFdPublicationPermit,
    rows: Vec<(super::accepted_transport::Envelope, Vec<u8>, usize)>,
}
#[cfg(test)]
impl NativeBirthCleanupPeer {
    pub(crate) fn new(
        permit: NetworkFdPublicationPermit,
        failed: bool,
    ) -> (NetworkRuntimeResources, Self) {
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
        let (runtime, resources) = unsafe {
            NetworkRuntimeResources::from_authenticated_startup(
                OwnedFd::from_raw_fd(pair[0]),
                [7; 16],
                super::ProviderWireFormat::Abi7Copy4,
            )
        };
        let peer = super::accepted_transport::AcceptedSession::new(
            unsafe { OwnedFd::from_raw_fd(pair[1]) },
            [7; 16],
        )
        .unwrap();
        let (owner, observed, completed, mut rows) =
            super::accepted_transport::native_birth_test_group(
                1,
                permit.native_command_call(),
                if failed { 1 } else { 2 },
            );
        assert_eq!(owner, permit.owner);
        assert_eq!(observed, None);
        assert_eq!(completed, 2);
        let mut request: Request = serde_json::from_slice(&rows[0].0.body).unwrap();
        let Request::PrepareNativeBirth { syscall, .. } = &mut request else {
            unreachable!()
        };
        *syscall = 57; // Same fork invocation used by the actual backend observer control.
        rows[0].0.body = serde_json::to_vec(&request).unwrap();
        let controller = resources.accepted_controller().unwrap();
        let pin = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            controller
                .prepare(
                    Effect::PrepareNativeBirth(permit.lease),
                    owner,
                    &request,
                    || Ok(vec![pin.as_fd().try_clone_to_owned()?])
                )
                .unwrap(),
            1
        );
        (
            resources,
            Self {
                runtime,
                controller,
                peer,
                permit,
                rows,
            },
        )
    }
    async fn receive(&mut self) -> (u64, Request) {
        use super::accepted_transport::Received;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(Received::Request(sequence)) = self.peer.try_receive().unwrap() {
                    let (envelope, _, _) = self.peer.retained_request(sequence).unwrap();
                    return (sequence, serde_json::from_slice(&envelope.body).unwrap());
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("bounded native driver transport request")
    }
    pub(crate) async fn prepare_reply(&mut self) {
        let (sequence, request) = self.receive().await;
        assert_eq!(sequence, 1);
        assert_eq!(serde_json::to_vec(&request).unwrap(), self.rows[0].0.body);
        self.peer
            .dispatch(sequence, |_, _| Ok(self.rows[0].1.clone()))
            .unwrap();
        assert!(self.peer.try_reply(sequence).unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while self
                .controller
                .retained_response(sequence)
                .unwrap()
                .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    pub(crate) async fn receive_finish(&mut self) {
        let (sequence, request) = self.receive().await;
        assert_eq!(sequence, 2);
        assert_eq!(serde_json::to_vec(&request).unwrap(), self.rows[1].0.body);
    }
    pub(crate) fn finish_reply(&mut self, wrong_errno: bool) {
        let mut reply: Reply = serde_json::from_slice(&self.rows[1].1).unwrap();
        if wrong_errno {
            let Reply::NativeBirthEffect(value) = &mut reply else {
                panic!("not errno collection")
            };
            value.raw.command.returned = -libc::ENOMEM;
        }
        self.peer
            .dispatch(2, |_, _| {
                serde_json::to_vec(&reply).map_err(io::Error::other)
            })
            .unwrap();
        if matches!(reply, Reply::NativeBirthEffect(_)) {
            self.peer
                .acknowledge_command_completion(2, |_, _| {
                    Ok(serde_json::to_vec(&serde_json::json!({
                "Observed":{"operation":"ap_ack_command","returned":0,"errno":null}}))
                    .unwrap())
                })
                .unwrap();
        }
        assert!(self.peer.try_reply(2).unwrap());
    }
    pub(crate) async fn wait_reply_retained(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while self.controller.retained_response(2).unwrap().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    pub(crate) async fn receive_retirement(&mut self) {
        let (sequence, request) = self.receive().await;
        assert_eq!(sequence, 3);
        assert!(
            matches!(request,Request::RetireNativeBirth {call,prepared:1,observed:None,completed:2}
            if call==self.permit.native_command_call())
        );
        assert!(!self.controller.quiescent().unwrap());
    }
    pub(crate) async fn retirement_reply(&mut self) {
        self.peer
            .retire_incoming_native_birth(
                self.permit.owner,
                self.permit.native_command_call(),
                1,
                None,
                2,
            )
            .unwrap();
        self.peer
            .dispatch(3, |_, _| {
                serde_json::to_vec(&Reply::Retired).map_err(io::Error::other)
            })
            .unwrap();
        assert!(self.peer.try_reply(3).unwrap());
        self.peer.retire_sent_original_ack(3).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !self.controller.quiescent().unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(self.peer.terminal_custody().retained_rights, 0);
    }
    pub(crate) async fn wait_failure(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while self.controller.quiescent().is_ok() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    pub(crate) fn stop(&mut self, success: bool) {
        let mut driver = self.runtime.shared.driver.lock().unwrap();
        let result = driver
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .stop_and_join(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert_eq!(result.is_ok(), success);
    }
}
#[cfg(test)]
impl Drop for NativeBirthCleanupPeer {
    fn drop(&mut self) {
        // Panic paths must not leave this test's native thread running.
        if let Some(Ok(driver)) = self.runtime.shared.driver.lock().unwrap().as_mut() {
            let _ =
                driver.stop_and_join(std::time::Instant::now() + std::time::Duration::from_secs(2));
        }
    }
}

// Synthetic authority exists only in unit-test builds. It deliberately cannot
// qualify the kernel issuer or remove the production requested-flags refusal.
#[cfg(test)]
pub(crate) fn synthetic_admission_for_common_consumer(
    requested: CloneFlags,
    actual: CloneFlags,
    clear_child_tid: u64,
    exit_signal: i32,
    terminal: bool,
) -> NativeBirthAdmission {
    let parent = DetTid::from_raw(41);
    let child = DetTid::from_raw(42);
    let permit = NetworkFdPublicationPermit {
        owner: NetworkStreamOwner {
            thread: parent,
            mm: MmId::initial(parent),
        },
        files: crate::types::FilesId::initial(parent),
        lease: serde_json::from_str("17").unwrap(),
    };
    let shared_mm = u32::from(actual.contains(CloneFlags::CLONE_VM));
    let shared_files = u32::from(actual.contains(CloneFlags::CLONE_FILES));
    let same_thread_group = u32::from(actual.contains(CloneFlags::CLONE_THREAD));
    let raw = super::accepted_provider_ffi::NativeBirth {
        command: 7,
        call: 17,
        owner_mm: permit.owner.mm.generation(),
        provider: 3,
        creator_task: (5001u64 << 32) | 5001,
        creator_start: 29,
        creator_table: 47,
        child_task: ((if same_thread_group == 1 {
            5001u64
        } else {
            5002u64
        }) << 32)
            | 5002,
        child_start: 31,
        child_table: if shared_files == 1 { 47 } else { 53 },
        parent_task: (5001u64 << 32) | 5001,
        parent_start: 29,
        copy_begin: if shared_files == 1 { 0 } else { 59 },
        copy_end: if shared_files == 1 { 0 } else { 61 },
        kernel_flags: actual.bits(),
        ready: 1,
        shared_mm,
        shared_files,
        same_thread_group,
        exit_signal,
        requested_exit_signal: 17,
        pidfd_fd: -1,
        clear_child_tid,
        ..Default::default()
    }
    .into();
    NativeBirthAdmission {
        permit,
        child,
        process: if same_thread_group == 1 {
            parent
        } else {
            child
        },
        flags: requested,
        terminal,
        raw,
    }
}

#[cfg(test)]
mod actual_clear_tid_tests {
    use super::*;
    #[test]
    fn clear_tid_zero_is_legal_with_cleartid_and_required_without_it() {
        for pointer in [0, 0x1234_5678] {
            let proof = synthetic_admission_for_common_consumer(
                CloneFlags::CLONE_CHILD_CLEARTID,
                CloneFlags::CLONE_CHILD_CLEARTID,
                pointer,
                17,
                false,
            );
            check_birth(proof.raw(), proof.permit(), 7, 47, proof.flags(), 435).unwrap();
        }
        let proof = synthetic_admission_for_common_consumer(
            CloneFlags::CLONE_CHILD_SETTID,
            CloneFlags::CLONE_CHILD_SETTID,
            0,
            17,
            false,
        );
        check_birth(proof.raw(), proof.permit(), 7, 47, proof.flags(), 435).unwrap();
        let mut wrong = proof.raw().clone();
        wrong.clear_child_tid = 0x1234;
        assert!(check_birth(&wrong, proof.permit(), 7, 47, proof.flags(), 435).is_err());
    }
    #[test]
    fn scalar_extension_keeps_requested_flag_mismatch_refusal() {
        let proof = synthetic_admission_for_common_consumer(
            CloneFlags::empty(),
            CloneFlags::CLONE_VM,
            0,
            17,
            false,
        );
        assert!(check_birth(proof.raw(), proof.permit(), 7, 47, proof.flags(), 435).is_err());
    }
}

#[cfg(test)]
pub(crate) fn synthetic_admission_for_permit(
    permit: NetworkFdPublicationPermit,
    actual: CloneFlags,
    terminal: bool,
) -> NativeBirthAdmission {
    let mut proof =
        synthetic_admission_for_common_consumer(CloneFlags::empty(), actual, 0, 17, terminal);
    assert_eq!(permit.owner.thread, DetTid::from_raw(41));
    proof.permit = permit;
    proof.raw.call = permit.native_command_call();
    proof.raw.owner_mm = permit.owner.mm.generation();
    proof
}
