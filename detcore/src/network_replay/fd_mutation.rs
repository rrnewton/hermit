//! Physical descriptor mutation admission. The ordinary constructor supplies no
//! capability until all backend table mutations have a proven interception
//! boundary. A publication cursor alone is never that capability.

use reverie::syscalls::CloneFlags;

use super::*;
use crate::types::DetPid;
use crate::types::ExecFilesReceipt;
use crate::types::FdSlotBinding;
use crate::types::NetworkFdSlot;
use crate::types::NetworkFdSlotReplacement;

/// Proof supplied by a backend with complete descriptor-table mutation coverage.
/// No production constructor exists in this incomplete vertical slice.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NetworkFdTableCapability(());

/// Capability seam for the real backend. No backend has closed the complete
/// mutation inventory yet; there is no flag which bypasses that requirement.
pub(crate) fn backend_fd_table_capability(
    _cfg: &crate::Config,
) -> Option<NetworkFdTableCapability> {
    None
}

#[cfg(test)]
impl NetworkFdTableCapability {
    pub(crate) fn controlled_fixture() -> Self {
        Self(())
    }
}

/// Kernel operation for the currently reviewed socket/alias vertical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkFdMutationKind {
    /// A single fresh socket; success returns its new descriptor.
    Socket,
    /// Kernel clone, with the existing exact clone-family flags.
    Clone {
        /// Original flags, including CLONE_FILES and CLONE_THREAD.
        flags: CloneFlags,
    },
    /// Exact run-global preparation, authenticated again at the RPC boundary.
    Exec {
        /// Existing allocator/MM receipt; not a new success detector.
        receipt: ExecFilesReceipt,
    },
    /// Dup/F_DUPFD allocates an alias at the returned descriptor.
    Alias {
        /// Original numeric source argument; never sufficient OFD authority.
        source_fd: i32,
        /// Exact current source installation, or None for an invalid FD.
        source: Option<FdSlotBinding>,
        /// Original syscall family, never inferred from its result.
        kind: NetworkFdInstallKind,
        /// Destination FD_CLOEXEC selected by the syscall.
        cloexec: bool,
        /// Explicit dup2/dup3 destination, or None for allocating forms.
        destination: Option<i32>,
        /// Exact destination network installation, if occupied.
        replaced: Option<NetworkFdSlot>,
    },
}

/// One short table/OFD admission and physical submission identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkFdMutationAdmission {
    /// Existing publication authority; it is not a second table mutex.
    pub publication: NetworkFdPublicationAdmission,
    /// Exact operation which this admission may submit.
    pub kind: NetworkFdMutationKind,
    /// Exact short OFD controls, acquired atomically with the table permit.
    pub controls: Vec<(OpenFileId, NetworkStreamLeaseId)>,
}

/// Result of atomic admission; recovery never masquerades as an inactive mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkFdMutationBegin {
    /// The normal backend has not supplied complete mutation coverage.
    Dormant,
    /// Recover/acknowledge the prior exact publication before another admission.
    Recover,
    /// A legal competing mutation changed the captured binding before admission.
    Refresh,
    /// Both the exact table and affected OFDs are exclusively admitted.
    Admitted(NetworkFdMutationAdmission),
}

#[derive(Debug, Clone)]
struct FdMutationState {
    admission: NetworkFdMutationAdmission,
    submitted: bool,
    kernel_result: Option<Result<i64, i32>>,
    installation_confirmed: bool,
    clone_child: Option<(TaskOwner, DetPid)>,
}

#[derive(Debug, Default)]
pub(super) struct FdLifecycleState {
    capability: Option<NetworkFdTableCapability>,
    mutations: BTreeMap<NetworkStreamLeaseId, FdMutationState>,
    retired_ports: BTreeSet<OpenFileId>,
}

fn protocol(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.to_owned())
}

impl NetworkReplayEngine {
    pub(crate) fn fd_table_capability(&self) -> bool {
        self.fd_lifecycle.capability.is_some()
    }

    /// Backend-only admission. The controlled fixture constructor is test-only;
    /// ordinary Record/Replay construction cannot opt into incomplete coverage.
    pub(crate) fn install_fd_table_capability(&mut self, capability: NetworkFdTableCapability) {
        assert!(self.fd_lifecycle.capability.is_none());
        assert!(self.fd_lifecycle.mutations.is_empty());
        self.fd_lifecycle.capability = Some(capability);
    }

    /// Called at authenticated initial scheduler registration, never from an
    /// arbitrary local snapshot or a reload that should consume an exec receipt.
    pub(crate) fn register_initial_fd_table(
        &mut self,
        owner: NetworkStreamOwner,
        process: DetPid,
    ) -> Result<bool, NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(false);
        }
        self.check_stream_owner(owner)?;
        self.lifetime
            .register(
                TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                },
                process,
                FilesId::initial(owner.thread),
            )
            .map_err(|e| protocol(&e.to_string()))?;
        Ok(true)
    }

    fn validate_fd_mutation_kind(
        &self,
        owner: NetworkStreamOwner,
        files: FilesId,
        kind: &NetworkFdMutationKind,
    ) -> Result<Option<Vec<OpenFileId>>, NetworkReplayError> {
        let task = self.publication_owner(owner, files)?;
        match kind {
            NetworkFdMutationKind::Socket | NetworkFdMutationKind::Clone { .. } => {
                Ok(Some(Vec::new()))
            }
            NetworkFdMutationKind::Exec { receipt } => {
                if receipt.caller != owner.thread
                    || receipt.mm != owner.mm
                    || receipt.old_files != files
                {
                    return Err(protocol("exec mutation receipt changed owner/table"));
                }
                Ok(Some(Vec::new()))
            }
            NetworkFdMutationKind::Alias {
                source_fd,
                source,
                kind,
                destination,
                replaced,
                ..
            } => {
                if !matches!(
                    kind,
                    NetworkFdInstallKind::Dup
                        | NetworkFdInstallKind::Dup2
                        | NetworkFdInstallKind::Dup3
                        | NetworkFdInstallKind::FcntlDup
                ) || matches!(
                    kind,
                    NetworkFdInstallKind::Dup2 | NetworkFdInstallKind::Dup3
                ) != destination.is_some()
                    || source.is_some_and(|s| s.slot.files != files || s.slot.fd != *source_fd)
                {
                    return Err(protocol("mutation source/family mismatch"));
                }
                let actual = self.lifetime.descriptor_binding(task, *source_fd).ok();
                if actual != *source {
                    return Ok(None);
                }
                if let Some(fd) = destination {
                    let actual = self.lifetime.descriptor_binding(task, *fd).ok();
                    if actual != replaced.map(|s| s.binding) {
                        return Ok(None);
                    }
                    if replaced.is_some_and(|s| s.binding.slot.fd != *fd) {
                        return Err(protocol("mutation destination mismatch"));
                    }
                } else if replaced.is_some() {
                    return Err(protocol("allocating duplicate has a fixed destination"));
                }
                Ok(Some(
                    source
                        .map(|s| s.open_file)
                        .into_iter()
                        .chain(replaced.map(|s| s.binding.open_file))
                        .collect(),
                ))
            }
        }
    }

    /// Reserve the table and every affected OFD in one engine transaction.
    /// Failure leaves no held permit/control; burned lease IDs are never reused.
    pub fn begin_fd_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        files: FilesId,
        kind: NetworkFdMutationKind,
    ) -> Result<NetworkFdMutationBegin, NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(NetworkFdMutationBegin::Dormant);
        }
        let Some(controls) = self.validate_fd_mutation_kind(owner, files, &kind)? else {
            return Ok(NetworkFdMutationBegin::Refresh);
        };
        let publication = self.acquire_fd_publication(owner, files)?;
        // Recovery must be published/ACKed by the caller before a new operation.
        // Return the real recovery admission rather than silently skipping it.
        if publication.recovery.is_some() {
            // No physical operation/control was submitted. Return this temporary
            // logical admission while retaining the exact recovery payload.
            self.fd_publications.get_mut(&files).unwrap().active = None;
            return Ok(NetworkFdMutationBegin::Recover);
        }
        let controls = match self.begin_socket_controls(owner, controls) {
            Ok(controls) => controls,
            Err(primary) => {
                self.release_empty_fd_publication(owner, publication.permit)
                    .expect("unmodified newly admitted empty publication");
                return Err(primary);
            }
        };
        // Physical control identity and semantic ownership are different. Add
        // all pins to a candidate ledger before committing any of them.
        let mut next_lifetime = self.lifetime.clone();
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        let retained = controls.iter().try_for_each(|(open_file, lease)| {
            let binding = next_lifetime.binding_for_open_file(task, *open_file)?;
            next_lifetime.retain_binding(task, binding, descriptor_mutation_pin(owner, *lease))
        });
        if let Err(primary) = retained {
            for (_, lease) in controls {
                self.finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
                    .expect("new unsubmitted controls have no physical effects");
            }
            self.release_empty_fd_publication(owner, publication.permit)
                .expect("new unsubmitted publication has no pending prefix");
            return Err(protocol(&primary.to_string()));
        }
        self.lifetime = next_lifetime;
        let admission = NetworkFdMutationAdmission {
            publication,
            kind,
            controls,
        };
        let lease = admission.publication.permit.lease;
        assert!(
            self.fd_lifecycle
                .mutations
                .insert(
                    lease,
                    FdMutationState {
                        admission: admission.clone(),
                        submitted: false,
                        kernel_result: None,
                        installation_confirmed: false,
                        clone_child: None,
                    }
                )
                .is_none()
        );
        Ok(NetworkFdMutationBegin::Admitted(admission))
    }

    fn fd_mutation(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<&FdMutationState, NetworkReplayError> {
        let completed_clone = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .is_some_and(|s| s.admission.publication.permit == permit && s.clone_child.is_some());
        if completed_clone {
            self.check_stream_owner(owner)?;
            if permit.owner != owner {
                return Err(protocol("clone result sender changed"));
            }
        } else {
            self.validate_publication_permit(owner, permit)?;
        }
        self.fd_lifecycle
            .mutations
            .get(&permit.lease)
            .filter(|s| s.admission.publication.permit == permit)
            .ok_or_else(|| protocol("unknown physical table mutation"))
    }

    /// Persist the pending effect before the adapter injects the real syscall.
    pub fn submit_fd_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        if state.submitted {
            return Err(protocol("duplicate physical submission"));
        }
        if let NetworkFdMutationKind::Clone { flags } = state.admission.kind {
            self.lifetime
                .prepare_clone(
                    clone_ticket(permit),
                    flags.contains(CloneFlags::CLONE_FILES),
                )
                .map_err(|error| protocol(&error.to_string()))?;
        }
        if let NetworkFdMutationKind::Exec { receipt } =
            self.fd_mutation(owner, permit)?.admission.kind
        {
            self.lifetime
                .prepare_exec(receipt)
                .map_err(|error| protocol(&error.to_string()))?;
        }
        self.fd_lifecycle
            .mutations
            .get_mut(&permit.lease)
            .unwrap()
            .submitted = true;
        Ok(())
    }

    /// Capture the actual kernel result before fstat/profile/publication awaits.
    /// Handler cancellation without this acknowledgement leaves a pending effect.
    pub fn confirm_fd_mutation_result(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
        result: Result<i64, i32>,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        if !state.submitted
            || state.kernel_result.is_some()
            || result
                .as_ref()
                .is_err_and(|errno| !(1..=4095).contains(errno))
            || result
                .as_ref()
                .is_ok_and(|fd| i32::try_from(*fd).is_err() || *fd < 0)
        {
            return Err(protocol("invalid or duplicate kernel mutation result"));
        }
        if let (
            NetworkFdMutationKind::Alias {
                destination: Some(fd),
                ..
            },
            Ok(actual),
        ) = (&state.admission.kind, result)
            && actual != i64::from(*fd)
        {
            return Err(protocol("replacement returned a different descriptor"));
        }
        if matches!(state.admission.kind, NetworkFdMutationKind::Exec { .. }) && result.is_ok() {
            return Err(protocol(
                "exec success requires authenticated backend lifecycle event",
            ));
        }
        if let Some((child, _)) = state.clone_child {
            if result != Ok(i64::from(child.tid.as_raw())) {
                return Err(protocol("parent clone result contradicts registered child"));
            }
            self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
            return Ok(());
        }
        self.fd_lifecycle
            .mutations
            .get_mut(&permit.lease)
            .unwrap()
            .kernel_result = Some(result);
        Ok(())
    }

    /// Associate metadata only with the matching already-confirmed kernel result.
    pub fn confirm_fd_installation(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
        change: NetworkFdSlotReplacement,
    ) -> Result<NetworkFdEffectAssociation, NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        let Some(Ok(fd)) = state.kernel_result else {
            return Err(protocol("installation has no successful kernel result"));
        };
        let after = change
            .after
            .ok_or_else(|| protocol("socket/alias installation missing"))?;
        if state.installation_confirmed
            || change.files != permit.files
            || after.binding.slot.files != permit.files
            || i64::from(after.binding.slot.fd) != fd
            || after.binding.generation != change.installation_generation
        {
            return Err(protocol("installation does not match confirmed result"));
        }
        let (kind, source) = match &state.admission.kind {
            NetworkFdMutationKind::Clone { .. } | NetworkFdMutationKind::Exec { .. } => {
                return Err(protocol(
                    "process lifecycle is not a descriptor installation",
                ));
            }
            NetworkFdMutationKind::Socket => {
                if change.before.is_some() || !after.binding.open_file.is_socket() {
                    return Err(protocol("fresh socket replaced an occupied tracked slot"));
                }
                (NetworkFdInstallKind::Socket, SlotInstallationSource::Fresh)
            }
            NetworkFdMutationKind::Alias {
                source,
                kind,
                cloexec,
                destination,
                replaced,
                ..
            } => {
                let source =
                    source.ok_or_else(|| protocol("successful alias has no source binding"))?;
                if after.binding.open_file != source.open_file
                    || after.cloexec != *cloexec
                    || change.before != *replaced
                    || destination.is_some_and(|dst| dst == source.slot.fd)
                {
                    return Err(protocol("alias result changed its captured source/target"));
                }
                (*kind, SlotInstallationSource::Alias(source))
            }
        };
        let effect = NetworkFdEffectAssociation {
            owner,
            lease: permit.lease,
            kind,
            result_index: 0,
            returned_fd: fd as i32,
        };
        if self.fd_installations.contains_key(&permit.lease) {
            return Err(protocol("physical installation receipt already exists"));
        }
        self.fd_installations.insert(
            permit.lease,
            ConfirmedFdInstallation {
                owner,
                files: permit.files,
                kind,
                returned_fds: vec![fd as i32],
                installations: vec![change],
                open_files: vec![Some(after.binding.open_file)],
                sources: vec![source],
            },
        );
        self.fd_lifecycle
            .mutations
            .get_mut(&permit.lease)
            .unwrap()
            .installation_confirmed = true;
        Ok(effect)
    }
}

/// Requests issued by real descriptor handlers at the physical boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkFdMutationRequest {
    /// Query the authenticated table's admitted capability.
    Tracking {
        /// Exact local descriptor-table identity.
        files: FilesId,
    },
    /// Atomically acquire table and OFD admission.
    Begin {
        /// Exact table.
        files: FilesId,
        /// Original syscall and pre-call bindings.
        kind: NetworkFdMutationKind,
    },
    /// Latch before syscall injection.
    Submit {
        /// Exact acquired permit.
        permit: NetworkFdPublicationPermit,
    },
    /// Capture the observed physical syscall result before further awaits.
    KernelResult {
        /// Exact submitted permit.
        permit: NetworkFdPublicationPermit,
        /// Actual kernel result/errno.
        result: Result<i64, i32>,
    },
    /// Bind one local installation to the matching physical result.
    Installation {
        /// Exact submitted permit.
        permit: NetworkFdPublicationPermit,
        /// Exact new/superseded incarnation.
        change: NetworkFdSlotReplacement,
    },
    /// Complete an error or a same-FD dup2, neither of which installs a slot.
    Unchanged {
        /// Exact submitted permit.
        permit: NetworkFdPublicationPermit,
    },
}

/// Typed response; an internal receipt never implies guest syscall success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkFdMutationReply {
    /// This exact table has complete mutation authority, or remains dormant.
    Tracking(bool),
    /// Atomic admission/recovery result.
    Begin(NetworkFdMutationBegin),
    /// Exact physical-to-metadata association.
    Installation(NetworkFdEffectAssociation),
    /// This protocol step completed; preserve the separate kernel result.
    Unit,
}

impl NetworkReplayEngine {
    pub(crate) fn recv_fd_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        request: NetworkFdMutationRequest,
    ) -> Result<NetworkFdMutationReply, NetworkReplayError> {
        use NetworkFdMutationReply as P;
        use NetworkFdMutationRequest as Q;
        match request {
            Q::Tracking { files } => {
                if self.fd_table_capability() {
                    self.publication_owner(owner, files)?;
                }
                Ok(P::Tracking(self.fd_table_capability()))
            }
            Q::Begin { files, kind } => self.begin_fd_mutation(owner, files, kind).map(P::Begin),
            Q::Submit { permit } => self.submit_fd_mutation(owner, permit).map(|()| P::Unit),
            Q::KernelResult { permit, result } => self
                .confirm_fd_mutation_result(owner, permit, result)
                .map(|()| P::Unit),
            Q::Installation { permit, change } => self
                .confirm_fd_installation(owner, permit, change)
                .map(P::Installation),
            Q::Unchanged { permit } => self
                .finish_unchanged_fd_mutation(owner, permit)
                .map(|()| P::Unit),
        }
    }

    fn validate_unchanged_fd_mutation(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        if state.installation_confirmed {
            return Err(protocol("confirmed installation cannot be discarded"));
        }
        let no_change = match (&state.admission.kind, state.kernel_result) {
            (_, Some(Err(_))) => true,
            (
                NetworkFdMutationKind::Alias {
                    source,
                    kind: NetworkFdInstallKind::Dup2,
                    destination: Some(dst),
                    ..
                },
                Some(Ok(actual)),
            ) => source.is_some_and(|s| *dst == s.slot.fd) && actual == i64::from(*dst),
            _ => false,
        };
        if !no_change {
            return Err(protocol(
                "unchanged finish has unresolved or successful installation",
            ));
        }
        self.validate_fd_control_release(
            owner,
            &state.admission.controls,
            lifetime::TransportResolution::CompletedAndRecorded,
        )?;
        Ok(())
    }

    /// Release admission only after an observed error or same-FD dup2 result.
    pub fn finish_unchanged_fd_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        self.validate_unchanged_fd_mutation(owner, permit)?;
        let is_clone = matches!(
            self.fd_mutation(owner, permit)?.admission.kind,
            NetworkFdMutationKind::Clone { .. }
        );
        if is_clone {
            let retired = self
                .lifetime
                .cancel_clone(clone_ticket(permit))
                .map_err(|error| protocol(&error.to_string()))?;
            self.retire_lifetime_open_files(retired);
        }
        if let NetworkFdMutationKind::Exec { receipt } =
            self.fd_mutation(owner, permit)?.admission.kind
        {
            let retired = self
                .lifetime
                .cancel_exec(receipt)
                .map_err(|error| protocol(&error.to_string()))?;
            self.retire_lifetime_open_files(retired);
        }
        let controls = self.fd_mutation(owner, permit)?.admission.controls.clone();
        // All checks precede mutation. Preserve exact physical errors separately;
        // this merely releases known-effect-free alias allocation controls.
        self.finish_fd_controls(
            owner,
            &controls,
            lifetime::TransportResolution::CompletedAndRecorded,
        )?;
        self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
        self.release_empty_fd_publication(owner, permit)
    }

    pub(super) fn validate_fd_mutation_publication(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let Some(state) = self.fd_lifecycle.mutations.get(&permit.lease) else {
            return Ok(());
        };
        self.fd_mutation(owner, permit)?;
        if !state.installation_confirmed || state.kernel_result.is_none_or(|result| result.is_err())
        {
            return Err(protocol(
                "publication cannot clear unresolved physical mutation",
            ));
        }
        self.validate_fd_control_release(
            owner,
            &state.admission.controls,
            lifetime::TransportResolution::CompletedAndRecorded,
        )?;
        Ok(())
    }

    pub(super) fn complete_fd_mutation_publication(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        self.validate_fd_mutation_publication(owner, permit)?;
        if let Some(state) = self.fd_lifecycle.mutations.remove(&permit.lease) {
            self.finish_fd_controls(
                owner,
                &state.admission.controls,
                lifetime::TransportResolution::CompletedAndRecorded,
            )?;
        }
        Ok(())
    }

    pub(super) fn finish_fd_mutations(&self) -> Result<(), NetworkReplayError> {
        if let Some((&lease, _)) = self.fd_lifecycle.mutations.first_key_value() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        if self.fd_table_capability() {
            self.lifetime
                .finish()
                .map_err(|error| protocol(&error.to_string()))?;
        }
        Ok(())
    }
}

fn clone_ticket(permit: NetworkFdPublicationPermit) -> lifetime::CloneTicket {
    lifetime::CloneTicket {
        owner: TaskOwner {
            tid: permit.owner.thread,
            mm: permit.owner.mm,
        },
        files: permit.files,
        operation: crate::resources::ExternalOpId::new(permit.owner.thread, permit.lease.0),
    }
}

impl NetworkReplayEngine {
    /// Existing backend child registration is the positive physical child event.
    /// Consume the one pre-clone reservation before the child enters the run queue.
    pub(crate) fn register_cloned_fd_table(
        &mut self,
        parent: NetworkStreamOwner,
        child: NetworkStreamOwner,
        process: DetPid,
        flags: CloneFlags,
    ) -> Result<bool, NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(false);
        }
        let matches: Vec<_> = self
            .fd_lifecycle
            .mutations
            .iter()
            .filter_map(|(&lease, state)| {
                (state.admission.publication.permit.owner == parent
                    && state.admission.kind == NetworkFdMutationKind::Clone { flags })
                .then_some(lease)
            })
            .collect();
        if matches.len() != 1 {
            return Err(protocol("child lacks one exact prepared clone"));
        }
        let lease = matches[0];
        let state = &self.fd_lifecycle.mutations[&lease];
        let permit = state.admission.publication.permit;
        self.validate_publication_permit(parent, permit)?;
        if !state.submitted
            || state.clone_child.is_some()
            || state
                .kernel_result
                .is_some_and(|r| r != Ok(i64::from(child.thread.as_raw())))
            || child.mm
                != crate::types::MmId::for_clone(
                    parent.mm,
                    child.thread,
                    flags.contains(CloneFlags::CLONE_VM),
                )
        {
            return Err(protocol("child contradicts submitted clone identity"));
        }
        let parent_returned = state.kernel_result.is_some();
        let child_task = TaskOwner {
            tid: child.thread,
            mm: child.mm,
        };
        self.lifetime
            .commit_clone(clone_ticket(permit), child_task, process)
            .map_err(|error| protocol(&error.to_string()))?;
        // The child can now mutate, even while vfork keeps the parent in kernel.
        self.release_empty_fd_publication(parent, permit)?;
        if parent_returned {
            self.fd_lifecycle.mutations.remove(&lease).unwrap();
        } else {
            self.fd_lifecycle
                .mutations
                .get_mut(&lease)
                .unwrap()
                .clone_child = Some((child_task, process));
        }
        Ok(true)
    }

    pub(super) fn physical_fd_mutation_pending(&self, permit: NetworkFdPublicationPermit) -> bool {
        self.fd_lifecycle
            .mutations
            .get(&permit.lease)
            .is_some_and(|state| {
                state.admission.publication.permit == permit
                    && state.submitted
                    && state.clone_child.is_none()
            })
    }
}

impl NetworkReplayEngine {
    /// Only the existing authenticated exec-success paths may call this method.
    /// The old table admission still excludes physical mutations when Linux
    /// performs its unshare; the retained receipt determines the new FilesId.
    pub(crate) fn commit_exec_fd_table(
        &mut self,
        receipt: ExecFilesReceipt,
        event: &crate::scheduler::ExecReconnect,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(());
        }
        let owner = NetworkStreamOwner {
            thread: receipt.caller,
            mm: receipt.mm,
        };
        let matches: Vec<_> = self
            .fd_lifecycle
            .mutations
            .values()
            .filter(|state| state.admission.kind == NetworkFdMutationKind::Exec { receipt })
            .collect();
        if matches.len() != 1 {
            return Err(protocol(
                "exec success lacks one exact admitted preparation",
            ));
        }
        let state = matches[0];
        let permit = state.admission.publication.permit;
        self.validate_publication_permit(owner, permit)?;
        if !state.submitted
            || state.kernel_result.is_some()
            || state.installation_confirmed
            || !state.admission.controls.is_empty()
        {
            return Err(protocol("exec event contradicts physical admission"));
        }
        let retired = self
            .lifetime
            .commit_exec(receipt, event)
            .map_err(|error| protocol(&error.to_string()))?;
        // Success replaced the old owner, so do not re-authorize using its now
        // dead MM. Release only the exact permit validated before the commit.
        let publication = self.fd_publications.get_mut(&permit.files).unwrap();
        assert_eq!(publication.active, Some(permit));
        assert!(publication.pending.is_none());
        publication.active = None;
        self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
        self.retire_lifetime_open_files(retired);
        self.prune_dead_fd_publication(receipt.old_files);
        Ok(())
    }

    pub(super) fn retire_lifetime_open_files(
        &mut self,
        retired: impl IntoIterator<Item = OpenFileId>,
    ) {
        for open_file in retired {
            self.fd_lifecycle.retired_ports.insert(open_file);
            self.retire_open_file(open_file);
        }
    }

    /// Global controller drains this set in the same synchronous transaction,
    /// before an RPC reply can be lost. Call pins delay this semantic retirement.
    pub(crate) fn take_lifetime_retired_ports(&mut self) -> BTreeSet<OpenFileId> {
        std::mem::take(&mut self.fd_lifecycle.retired_ports)
    }
}

fn descriptor_mutation_pin(
    owner: NetworkStreamOwner,
    lease: NetworkStreamLeaseId,
) -> lifetime::LeaseId {
    lifetime::LeaseId {
        operation: crate::resources::ExternalOpId::new(owner.thread, lease.0),
        mm: owner.mm,
        kind: lifetime::LeaseKind::DescriptorMutation,
        ordinal: 0,
    }
}

impl NetworkReplayEngine {
    fn validate_fd_control_release(
        &self,
        owner: NetworkStreamOwner,
        controls: &[(OpenFileId, NetworkStreamLeaseId)],
        resolution: lifetime::TransportResolution,
    ) -> Result<(NetworkLifetime, BTreeSet<OpenFileId>), NetworkReplayError> {
        let mut next = self.lifetime.clone();
        let mut retired = BTreeSet::new();
        for (open_file, lease) in controls {
            let control = self.owned_socket_control(owner, *lease)?;
            if control.open_file != *open_file
                || self.shadow_probes.contains_key(lease)
                || !control.physical.can_release_unchanged()
            {
                return Err(NetworkReplayError::UnresolvedStreamOperation(*lease));
            }
            retired.extend(
                next.acknowledge_transport(
                    descriptor_mutation_pin(owner, *lease),
                    *open_file,
                    resolution,
                )
                .map_err(|error| protocol(&error.to_string()))?,
            );
        }
        Ok((next, retired))
    }

    fn finish_fd_controls(
        &mut self,
        owner: NetworkStreamOwner,
        controls: &[(OpenFileId, NetworkStreamLeaseId)],
        resolution: lifetime::TransportResolution,
    ) -> Result<(), NetworkReplayError> {
        let (next, retired) = self.validate_fd_control_release(owner, controls, resolution)?;
        for (_, lease) in controls {
            self.finish_socket_control(owner, *lease, NetworkSocketControlFinish::Unchanged)?;
        }
        self.lifetime = next;
        self.retire_lifetime_open_files(retired);
        Ok(())
    }

    fn cancel_unsubmitted_fd_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        if state.submitted
            || state.kernel_result.is_some()
            || state.installation_confirmed
            || state.clone_child.is_some()
        {
            return Err(protocol(
                "cancellation cannot erase a submitted descriptor effect",
            ));
        }
        let controls = state.admission.controls.clone();
        self.finish_fd_controls(
            owner,
            &controls,
            lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission,
        )?;
        self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
        self.release_empty_fd_publication(owner, permit)
    }
}

fn stream_call_pin(owner: NetworkStreamOwner, call: NetworkStreamCallId) -> lifetime::LeaseId {
    lifetime::LeaseId {
        operation: crate::resources::ExternalOpId::new(owner.thread, call.0),
        mm: owner.mm,
        kind: lifetime::LeaseKind::StreamCall,
        ordinal: 0,
    }
}

impl NetworkReplayEngine {
    pub(super) fn retain_stream_call_lifetime(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        open_file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(());
        }
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        let binding = self
            .lifetime
            .binding_for_open_file(task, open_file)
            .map_err(|error| protocol(&error.to_string()))?;
        self.lifetime
            .retain_binding(task, binding, stream_call_pin(owner, call))
            .map_err(|error| protocol(&error.to_string()))
    }

    pub(super) fn release_stream_call_lifetime(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        open_file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(());
        }
        let retired = self
            .lifetime
            .acknowledge_transport(
                stream_call_pin(owner, call),
                open_file,
                lifetime::TransportResolution::CompletedAndRecorded,
            )
            .map_err(|error| protocol(&error.to_string()))?;
        self.retire_lifetime_open_files(retired);
        Ok(())
    }

    /// Actual backend task consumption closes a table owner even if local Arcs
    /// linger. A stale MM cannot detach a replacement, and operation refs stay.
    pub(crate) fn retire_fd_table_owner(&mut self, owner: NetworkStreamOwner) {
        if !self.fd_table_capability() {
            return;
        }
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        let Ok(files) = self.lifetime.task_files(task) else {
            return;
        };
        // An admission whose Submit was never processed cannot have reached
        // Guest::inject: the handler waits for that acknowledgement first.
        // A lost Submit reply has submitted=true and deliberately fails this guard.
        let unsubmitted: Vec<_> = self
            .fd_lifecycle
            .mutations
            .values()
            .filter(|state| state.admission.publication.permit.owner == owner && !state.submitted)
            .map(|state| state.admission.publication.permit)
            .collect();
        for permit in unsubmitted {
            self.cancel_unsubmitted_fd_mutation(owner, permit)
                .expect("unsubmitted admission has no kernel effect or publication");
        }
        // A confirmed allocation error or same-FD dup2 has no installation.
        // Perform its existing exact cleanup while the task still owns the table.
        // Unknown/positive unpublished effects do not satisfy this predicate.
        let known_unchanged: Vec<_> = self
            .fd_lifecycle
            .mutations
            .values()
            .filter(|state| state.admission.publication.permit.owner == owner)
            .map(|state| state.admission.publication.permit)
            .filter(|permit| self.validate_unchanged_fd_mutation(owner, *permit).is_ok())
            .collect();
        for permit in known_unchanged {
            self.finish_unchanged_fd_mutation(owner, permit)
                .expect("known unchanged physical mutation prevalidated during exact owner exit");
        }
        // Registered child is a positive kernel effect. A dead parent's missing
        // return value does not undo the child's table or leave an unknown clone.
        self.fd_lifecycle.mutations.retain(|_, state| {
            state.admission.publication.permit.owner != owner || state.clone_child.is_none()
        });
        let retired = self.lifetime.exit(task).expect("exact admitted task owner");
        self.retire_lifetime_open_files(retired);
        self.prune_dead_fd_publication(files);
    }

    pub(super) fn prune_dead_fd_publication(&mut self, files: FilesId) {
        if self.lifetime.table_exists(files) {
            return;
        }
        let unknown = self
            .fd_publications
            .get(&files)
            .and_then(|state| state.active)
            .is_some_and(|permit| self.physical_fd_mutation_pending(permit));
        if unknown {
            return;
        }
        self.lifetime
            .prune_dead_table_publication(files)
            .expect("last actual table owner and copy/share reservations are gone");
        self.fd_publications.remove(&files);
        self.fd_publication_history
            .retain(|(table, _), _| *table != files);
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn fd_table_fixture_enable(&mut self) {
        self.install_fd_table_capability(NetworkFdTableCapability::controlled_fixture());
    }
    pub(crate) fn fd_table_fixture_files(&self, owner: NetworkStreamOwner) -> Option<FilesId> {
        self.lifetime
            .task_files(TaskOwner {
                tid: owner.thread,
                mm: owner.mm,
            })
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::types::DetTid;
    use crate::types::FdSlot;
    use crate::types::MmId;

    fn setup() -> (NetworkReplayEngine, NetworkStreamOwner, FilesId) {
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let files = FilesId::initial(thread);
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.install_fd_table_capability(NetworkFdTableCapability::controlled_fixture());
        assert!(engine.register_initial_fd_table(owner, thread).unwrap());
        (engine, owner, files)
    }
    fn admitted(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        files: FilesId,
        kind: NetworkFdMutationKind,
    ) -> NetworkFdMutationAdmission {
        match engine.begin_fd_mutation(owner, files, kind).unwrap() {
            NetworkFdMutationBegin::Admitted(admission) => admission,
            other => panic!("expected admission, got {other:?}"),
        }
    }
    fn slot(
        owner: NetworkStreamOwner,
        fd: i32,
        generation: u64,
        open_file: OpenFileId,
    ) -> NetworkFdSlot {
        NetworkFdSlot {
            binding: FdSlotBinding {
                slot: FdSlot {
                    files: FilesId::initial(owner.thread),
                    fd,
                },
                generation,
                open_file,
            },
            cloexec: false,
        }
    }
    fn commit_install(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &NetworkFdMutationAdmission,
        before: Option<NetworkFdSlot>,
        after: NetworkFdSlot,
    ) {
        let permit = admission.publication.permit;
        let change = NetworkFdSlotReplacement {
            files: permit.files,
            installation_generation: after.binding.generation,
            before,
            after: Some(after),
        };
        let effect = engine
            .confirm_fd_installation(owner, permit, change)
            .unwrap();
        let batch = NetworkFdPublicationBatch {
            files: permit.files,
            sequence: admission.publication.acknowledged_sequence + 1,
            previous_generation: admission.publication.acknowledged_generation,
            through_generation: after.binding.generation,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        };
        assert_eq!(
            engine
                .publish_fd_publication(owner, permit, &batch)
                .unwrap(),
            batch
        );
        engine
            .acknowledge_fd_publication(owner, permit, &batch)
            .unwrap();
    }
    fn socket(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        files: FilesId,
        fd: i32,
        generation: u64,
    ) -> NetworkFdSlot {
        let admission = admitted(engine, owner, files, NetworkFdMutationKind::Socket);
        let permit = admission.publication.permit;
        engine.submit_fd_mutation(owner, permit).unwrap();
        engine
            .confirm_fd_mutation_result(owner, permit, Ok(fd.into()))
            .unwrap();
        let after = slot(
            owner,
            fd,
            generation,
            OpenFileId::new_socket(owner.thread, generation),
        );
        commit_install(engine, owner, &admission, None, after);
        after
    }
    fn alias(
        source: NetworkFdSlot,
        kind: NetworkFdInstallKind,
        destination: Option<i32>,
        replaced: Option<NetworkFdSlot>,
    ) -> NetworkFdMutationKind {
        NetworkFdMutationKind::Alias {
            source_fd: source.binding.slot.fd,
            source: Some(source.binding),
            kind,
            cloexec: false,
            destination,
            replaced,
        }
    }

    #[test]
    fn ordinary_backend_has_no_partial_mutation_capability() {
        assert!(backend_fd_table_capability(&crate::Config::default()).is_none());
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(71),
            mm: MmId::initial(DetTid::from_raw(71)),
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        let before = format!("{engine:?}");
        assert!(
            !engine
                .register_initial_fd_table(owner, owner.thread)
                .unwrap()
        );
        assert_eq!(
            engine
                .begin_fd_mutation(
                    owner,
                    FilesId::initial(owner.thread),
                    NetworkFdMutationKind::Socket
                )
                .unwrap(),
            NetworkFdMutationBegin::Dormant
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    #[test]
    fn physical_result_precedes_installation_and_wrong_fd_preserves_state() {
        let (mut engine, owner, files) = setup();
        let a = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        let p = a.publication.permit;
        assert!(engine.confirm_fd_mutation_result(owner, p, Ok(7)).is_err());
        engine.submit_fd_mutation(owner, p).unwrap();
        assert!(engine.finish_unchanged_fd_mutation(owner, p).is_err());
        engine.confirm_fd_mutation_result(owner, p, Ok(7)).unwrap();
        let wrong = slot(owner, 8, 1, OpenFileId::new_socket(owner.thread, 1));
        let change = NetworkFdSlotReplacement {
            files,
            installation_generation: 1,
            before: None,
            after: Some(wrong),
        };
        let before = format!("{engine:?}");
        assert!(engine.confirm_fd_installation(owner, p, change).is_err());
        assert_eq!(format!("{engine:?}"), before);
        let after = slot(owner, 7, 1, wrong.binding.open_file);
        commit_install(&mut engine, owner, &a, None, after);
        assert_eq!(
            engine
                .lifetime
                .descriptor_binding(
                    TaskOwner {
                        tid: owner.thread,
                        mm: owner.mm
                    },
                    7
                )
                .unwrap(),
            after.binding
        );
        assert!(engine.fd_lifecycle.mutations.is_empty());
        assert!(engine.fd_installations.is_empty());
    }
    #[test]
    fn failed_kernel_allocation_releases_exact_permit_without_installation() {
        let (mut engine, owner, files) = setup();
        let a = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        engine
            .confirm_fd_mutation_result(owner, p, Err(libc::EMFILE))
            .unwrap();
        assert!(engine.confirm_fd_mutation_result(owner, p, Ok(7)).is_err());
        engine.finish_unchanged_fd_mutation(owner, p).unwrap();
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (0, 0));
        assert!(engine.fd_installations.is_empty());
        let next = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        assert_ne!(next.publication.permit.lease, p.lease);
    }
    #[test]
    fn actual_alias_result_preserves_ofd_and_adds_exact_slot() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let a = admitted(
            &mut engine,
            owner,
            files,
            alias(source, NetworkFdInstallKind::Dup, None, None),
        );
        assert_eq!(a.controls.len(), 1);
        engine
            .submit_fd_mutation(owner, a.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, a.publication.permit, Ok(8))
            .unwrap();
        let after = slot(owner, 8, 2, source.binding.open_file);
        commit_install(&mut engine, owner, &a, None, after);
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 2);
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (2, 2));
        assert!(engine.socket_controls.is_empty());
    }
    #[test]
    fn same_fd_dup2_does_not_allocate_or_change_flags() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let a = admitted(
            &mut engine,
            owner,
            files,
            alias(source, NetworkFdInstallKind::Dup2, Some(7), Some(source)),
        );
        assert_eq!(
            a.controls.len(),
            1,
            "source and destination share one atomic control"
        );
        engine
            .submit_fd_mutation(owner, a.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, a.publication.permit, Ok(7))
            .unwrap();
        engine
            .finish_unchanged_fd_mutation(owner, a.publication.permit)
            .unwrap();
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (1, 1));
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
        assert!(engine.socket_controls.is_empty());
    }
    #[test]
    fn stale_alias_binding_refreshes_without_holding_any_authority() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let mut stale = source;
        stale.binding.generation += 1;
        let before = format!("{engine:?}");
        assert_eq!(
            engine
                .begin_fd_mutation(
                    owner,
                    files,
                    alias(stale, NetworkFdInstallKind::Dup, None, None)
                )
                .unwrap(),
            NetworkFdMutationBegin::Refresh
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    #[test]
    fn busy_second_ofd_leaves_neither_first_control_nor_table_gate() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let destination = socket(&mut engine, owner, files, 8, 2);
        let other = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: owner.mm,
        };
        let held = engine
            .begin_socket_controls(other, vec![destination.binding.open_file])
            .unwrap();
        assert!(matches!(
            engine.begin_fd_mutation(
                owner,
                files,
                alias(
                    source,
                    NetworkFdInstallKind::Dup2,
                    Some(8),
                    Some(destination)
                )
            ),
            Err(NetworkReplayError::StreamOperationBusy(_))
        ));
        assert_eq!(engine.socket_controls.len(), 1);
        assert!(
            !engine
                .socket_controls
                .contains_key(&source.binding.open_file)
        );
        assert!(engine.fd_publications[&files].active.is_none());
        assert!(engine.fd_lifecycle.mutations.is_empty());
        engine
            .finish_socket_control(other, held[0].1, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        assert!(matches!(
            engine.begin_fd_mutation(
                owner,
                files,
                alias(
                    source,
                    NetworkFdInstallKind::Dup2,
                    Some(8),
                    Some(destination)
                )
            ),
            Ok(NetworkFdMutationBegin::Admitted(_))
        ));
    }
    #[test]
    fn stale_owner_and_cancellation_cannot_ack_unknown_kernel_effect() {
        let (mut engine, owner, files) = setup();
        let a = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        let stale = NetworkStreamOwner {
            thread: owner.thread,
            mm: MmId::initial(DetTid::from_raw(91)),
        };
        assert!(
            engine
                .confirm_fd_mutation_result(stale, p, Err(libc::EINTR))
                .is_err()
        );
        assert!(engine.finish_unchanged_fd_mutation(owner, p).is_err());
        engine.stream_owner_gone(owner);
        assert!(engine.finish_fd_mutations().is_err());
        assert!(engine.fd_lifecycle.mutations.contains_key(&p.lease));
        assert!(
            engine.fd_lifecycle.mutations[&p.lease]
                .kernel_result
                .is_none()
        );
    }

    #[test]
    fn failed_clone_cancels_only_after_kernel_error_without_child_or_generation() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Clone {
                flags: CloneFlags::empty(),
            },
        );
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        assert_eq!(
            engine
                .lifetime
                .counts(source.binding.open_file)
                .clone_reservations,
            1
        );
        assert!(engine.finish_unchanged_fd_mutation(owner, p).is_err());
        engine
            .confirm_fd_mutation_result(owner, p, Err(libc::EAGAIN))
            .unwrap();
        engine.finish_unchanged_fd_mutation(owner, p).unwrap();
        assert_eq!(
            engine
                .lifetime
                .counts(source.binding.open_file)
                .clone_reservations,
            0
        );
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (1, 1));
        assert!(engine.fd_publications[&files].active.is_none());
    }

    #[test]
    fn copied_child_registration_preserves_slots_ofd_and_cursor_before_exit() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let flags = CloneFlags::empty();
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Clone { flags },
        );
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        let thread = DetTid::from_raw(62);
        let child = NetworkStreamOwner {
            thread,
            mm: MmId::for_clone(owner.mm, thread, false),
        };
        engine
            .confirm_fd_mutation_result(owner, p, Ok(i64::from(thread.as_raw())))
            .unwrap();
        engine
            .register_cloned_fd_table(owner, child, thread, flags)
            .unwrap();
        let binding = engine
            .lifetime
            .descriptor_binding(
                TaskOwner {
                    tid: thread,
                    mm: child.mm,
                },
                7,
            )
            .unwrap();
        assert_eq!(binding.slot.files, FilesId::forked(thread));
        assert_eq!(binding.open_file, source.binding.open_file);
        assert_eq!(binding.generation, 1);
        assert_eq!(engine.fd_publication_fixture_cursor(child), (0, 1));
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 2);
        engine.retire_fd_table_owner(owner);
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
        assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
        engine.retire_fd_table_owner(child);
        assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
    }

    #[test]
    fn vfork_child_handoff_releases_table_before_parent_kernel_return() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let flags = CloneFlags::CLONE_FILES | CloneFlags::CLONE_VM | CloneFlags::CLONE_VFORK;
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Clone { flags },
        );
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        let child = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: owner.mm,
        };
        engine
            .register_cloned_fd_table(owner, child, child.thread, flags)
            .unwrap();
        assert_eq!(
            engine
                .lifetime
                .task_files(TaskOwner {
                    tid: child.thread,
                    mm: child.mm
                })
                .unwrap(),
            files
        );
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
        assert!(engine.fd_publications[&files].active.is_none());
        let next = admitted(&mut engine, child, files, NetworkFdMutationKind::Socket);
        engine
            .submit_fd_mutation(child, next.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(child, next.publication.permit, Err(libc::EMFILE))
            .unwrap();
        engine
            .finish_unchanged_fd_mutation(child, next.publication.permit)
            .unwrap();
        assert!(
            engine.fd_lifecycle.mutations[&p.lease]
                .kernel_result
                .is_none()
        );
        engine
            .confirm_fd_mutation_result(owner, p, Ok(i64::from(child.thread.as_raw())))
            .unwrap();
        assert!(!engine.fd_lifecycle.mutations.contains_key(&p.lease));
        engine.retire_fd_table_owner(owner);
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
        engine.retire_fd_table_owner(child);
        assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
    }

    #[test]
    fn clone_registration_rejects_changed_flags_or_mm_without_consuming_snapshot() {
        let (mut engine, owner, files) = setup();
        socket(&mut engine, owner, files, 7, 1);
        let flags = CloneFlags::empty();
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Clone { flags },
        );
        engine
            .submit_fd_mutation(owner, a.publication.permit)
            .unwrap();
        let child = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: owner.mm,
        };
        let before = format!("{engine:?}");
        assert!(
            engine
                .register_cloned_fd_table(owner, child, child.thread, flags)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let child = NetworkStreamOwner {
            mm: MmId::for_clone(owner.mm, child.thread, false),
            ..child
        };
        assert!(
            engine
                .register_cloned_fd_table(owner, child, child.thread, CloneFlags::CLONE_FILES)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .register_cloned_fd_table(owner, child, child.thread, flags)
            .unwrap();
        assert!(
            engine
                .confirm_fd_mutation_result(owner, a.publication.permit, Err(libc::EAGAIN))
                .is_err()
        );
        assert!(
            engine
                .fd_lifecycle
                .mutations
                .contains_key(&a.publication.permit.lease)
        );
    }

    #[test]
    fn owner_exit_keeps_unknown_or_known_unpublished_physical_table_gate() {
        for result in [None, Some(Ok(7))] {
            let (mut engine, owner, files) = setup();
            let sibling = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                mm: owner.mm,
            };
            engine.fd_publication_fixture_register(sibling, Some(owner));
            let a = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
            let p = a.publication.permit;
            engine.submit_fd_mutation(owner, p).unwrap();
            if let Some(result) = result {
                engine.confirm_fd_mutation_result(owner, p, result).unwrap();
            }
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert_eq!(engine.fd_publications[&files].active, Some(p));
            assert!(
                engine
                    .begin_fd_mutation(sibling, files, NetworkFdMutationKind::Socket)
                    .is_err()
            );
            assert!(engine.finish_fd_mutations().is_err());
        }
    }

    #[test]
    fn published_physical_result_recovers_after_owner_exit_without_repeating_effect() {
        let (mut engine, owner, files) = setup();
        let sibling = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: owner.mm,
        };
        engine.fd_publication_fixture_register(sibling, Some(owner));
        let a = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        engine.confirm_fd_mutation_result(owner, p, Ok(7)).unwrap();
        let after = slot(owner, 7, 1, OpenFileId::new_socket(owner.thread, 1));
        let change = NetworkFdSlotReplacement {
            files,
            installation_generation: 1,
            before: None,
            after: Some(after),
        };
        let effect = engine.confirm_fd_installation(owner, p, change).unwrap();
        let batch = NetworkFdPublicationBatch {
            files,
            sequence: 1,
            previous_generation: 0,
            through_generation: 1,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        };
        engine.publish_fd_publication(owner, p, &batch).unwrap();
        assert!(!engine.fd_lifecycle.mutations.contains_key(&p.lease));
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        let recovery = engine.acquire_fd_publication(sibling, files).unwrap();
        assert_eq!(recovery.recovery, Some(batch.clone()));
        assert_eq!(
            engine
                .publish_fd_publication(sibling, recovery.permit, &batch)
                .unwrap(),
            batch
        );
        engine
            .acknowledge_fd_publication(sibling, recovery.permit, &batch)
            .unwrap();
        assert!(engine.fd_installations.is_empty());
        assert_eq!(engine.lifetime.counts(after.binding.open_file).slots, 1);
        assert!(engine.fd_publications[&files].active.is_none());
    }

    #[test]
    fn stale_mm_exit_cannot_detach_live_table_or_aliases() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let stale = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        let before = format!("{engine:?}");
        engine.retire_fd_table_owner(stale);
        assert_eq!(format!("{engine:?}"), before);
        engine.retire_fd_table_owner(owner);
        assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
    }

    #[test]
    fn final_table_exit_preserves_semantic_active_call_pin_without_false_completion() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let controls = engine
            .begin_socket_controls(owner, vec![source.binding.open_file])
            .unwrap();
        let call = engine.begin_stream_call(owner, controls[0].1).unwrap();
        engine
            .confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(owner, controls[0].1, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        assert_eq!(
            engine.lifetime.counts(source.binding.open_file).transports,
            1
        );
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 0);
        assert_eq!(
            engine.lifetime.counts(source.binding.open_file).transports,
            1
        );
        assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
        assert!(engine.lifetime.finish().is_err());
    }
    #[test]
    fn known_failed_owner_exit_releases_admission_for_shared_survivor() {
        for errno in [libc::EMFILE, libc::EBADF, libc::EINTR] {
            let (mut engine, owner, files) = setup();
            let sibling = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                mm: owner.mm,
            };
            engine.fd_publication_fixture_register(sibling, Some(owner));
            let source = socket(&mut engine, owner, files, 7, 1);
            let a = admitted(
                &mut engine,
                owner,
                files,
                alias(source, NetworkFdInstallKind::Dup, None, None),
            );
            let p = a.publication.permit;
            engine.submit_fd_mutation(owner, p).unwrap();
            engine
                .confirm_fd_mutation_result(owner, p, Err(errno))
                .unwrap();
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert!(!engine.fd_lifecycle.mutations.contains_key(&p.lease));
            assert!(engine.fd_publications[&files].active.is_none());
            assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
            assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
            assert!(matches!(
                engine
                    .begin_fd_mutation(sibling, files, NetworkFdMutationKind::Socket)
                    .unwrap(),
                NetworkFdMutationBegin::Admitted(_)
            ));
        }
    }

    #[test]
    fn last_owner_exit_prunes_completed_lost_ack_but_not_unknown_effect() {
        let (mut engine, owner, files) = setup();
        let a = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        let p = a.publication.permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        engine.confirm_fd_mutation_result(owner, p, Ok(7)).unwrap();
        let after = slot(owner, 7, 1, OpenFileId::new_socket(owner.thread, 1));
        let change = NetworkFdSlotReplacement {
            files,
            installation_generation: 1,
            before: None,
            after: Some(after),
        };
        let effect = engine.confirm_fd_installation(owner, p, change).unwrap();
        let batch = NetworkFdPublicationBatch {
            files,
            sequence: 1,
            previous_generation: 0,
            through_generation: 1,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        };
        engine.publish_fd_publication(owner, p, &batch).unwrap();
        assert_eq!(engine.fd_publication_history.len(), 1);
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        assert!(!engine.fd_publications.contains_key(&files));
        assert!(engine.fd_publication_history.is_empty());
        assert!(engine.fd_installations.is_empty());
        assert_eq!(
            engine.take_lifetime_retired_ports(),
            BTreeSet::from([after.binding.open_file])
        );
        assert!(
            engine
                .lifetime
                .register(
                    TaskOwner {
                        tid: owner.thread,
                        mm: owner.mm
                    },
                    owner.thread,
                    files
                )
                .is_err()
        );
        engine.finish_fd_mutations().unwrap();
        assert!(engine.publish_fd_publication(owner, p, &batch).is_err());

        let (mut engine, owner, files) = setup();
        let p = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket)
            .publication
            .permit;
        engine.submit_fd_mutation(owner, p).unwrap();
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        assert_eq!(engine.fd_publications[&files].active, Some(p));
        assert!(engine.fd_lifecycle.mutations.contains_key(&p.lease));
        assert!(engine.finish_fd_mutations().is_err());
    }

    fn exec_receipt(
        owner: NetworkStreamOwner,
        files: FilesId,
        generation: u64,
    ) -> ExecFilesReceipt {
        ExecFilesReceipt {
            caller: owner.thread,
            process: owner.thread,
            mm: owner.mm,
            old_files: files,
            new_files: {
                let mut allocator = crate::types::FilesIdAllocator::default();
                (0..generation)
                    .map(|_| allocator.allocate_exec(owner.thread))
                    .last()
                    .unwrap()
            },
        }
    }
    fn exec_event(receipt: ExecFilesReceipt) -> crate::scheduler::ExecReconnect {
        crate::scheduler::ExecReconnect {
            caller: receipt.caller,
            new_leader: receipt.process,
            detpid: receipt.process,
            pre_exec_mm: receipt.mm,
            post_exec_mm: receipt.mm.for_exec(receipt.process),
            child_tid_addr: 0,
            reconnect_priority: None,
        }
    }

    #[test]
    fn failed_exec_preserves_exact_bindings_and_burns_canceled_allocation() {
        for errno in [libc::ENOENT, libc::EACCES] {
            let (mut engine, owner, files) = setup();
            let source = socket(&mut engine, owner, files, 7, 1);
            let receipt = exec_receipt(owner, files, 10);
            let a = admitted(
                &mut engine,
                owner,
                files,
                NetworkFdMutationKind::Exec { receipt },
            );
            let p = a.publication.permit;
            engine.submit_fd_mutation(owner, p).unwrap();
            assert_eq!(
                engine
                    .lifetime
                    .counts(source.binding.open_file)
                    .exec_reservations,
                1
            );
            engine
                .confirm_fd_mutation_result(owner, p, Err(errno))
                .unwrap();
            engine.finish_unchanged_fd_mutation(owner, p).unwrap();
            assert_eq!(
                engine
                    .lifetime
                    .counts(source.binding.open_file)
                    .exec_reservations,
                0
            );
            assert_eq!(
                engine
                    .lifetime
                    .descriptor_binding(
                        TaskOwner {
                            tid: owner.thread,
                            mm: owner.mm
                        },
                        7
                    )
                    .unwrap(),
                source.binding
            );
            assert_eq!(
                engine
                    .lifetime
                    .task_files(TaskOwner {
                        tid: owner.thread,
                        mm: owner.mm
                    })
                    .unwrap(),
                files
            );
            assert!(engine.take_lifetime_retired_ports().is_empty());
            let a = admitted(
                &mut engine,
                owner,
                files,
                NetworkFdMutationKind::Exec { receipt },
            );
            assert!(
                engine
                    .submit_fd_mutation(owner, a.publication.permit)
                    .is_err()
            );
        }
    }

    #[test]
    fn successful_exec_uses_one_receipt_and_preserves_external_shared_alias() {
        let (mut engine, owner, files) = setup();
        let keep = socket(&mut engine, owner, files, 7, 1);
        let p = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
        engine
            .submit_fd_mutation(owner, p.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, p.publication.permit, Ok(8))
            .unwrap();
        let mut close = slot(owner, 8, 2, OpenFileId::new_socket(owner.thread, 2));
        close.cloexec = true;
        commit_install(&mut engine, owner, &p, None, close);
        let child = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: MmId::initial(DetTid::from_raw(62)),
        };
        let clone = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Clone {
                flags: CloneFlags::CLONE_FILES,
            },
        );
        engine
            .submit_fd_mutation(owner, clone.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, clone.publication.permit, Ok(62))
            .unwrap();
        engine
            .register_cloned_fd_table(owner, child, child.thread, CloneFlags::CLONE_FILES)
            .unwrap();
        let receipt = exec_receipt(owner, files, 10);
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Exec { receipt },
        );
        engine
            .submit_fd_mutation(owner, a.publication.permit)
            .unwrap();
        assert!(
            engine
                .confirm_fd_mutation_result(owner, a.publication.permit, Ok(0))
                .is_err()
        );
        engine
            .commit_exec_fd_table(receipt, &exec_event(receipt))
            .unwrap();
        let new = TaskOwner {
            tid: owner.thread,
            mm: receipt.mm.for_exec(receipt.process),
        };
        assert_eq!(engine.lifetime.task_files(new).unwrap(), receipt.new_files);
        assert_eq!(
            engine
                .lifetime
                .descriptor_binding(new, 7)
                .unwrap()
                .open_file,
            keep.binding.open_file
        );
        assert!(engine.lifetime.descriptor_binding(new, 8).is_err());
        assert_eq!(engine.lifetime.counts(close.binding.open_file).slots, 1);
        assert_eq!(engine.lifetime.counts(keep.binding.open_file).slots, 2);
        assert!(engine.take_lifetime_retired_ports().is_empty());
        assert!(
            engine
                .commit_exec_fd_table(receipt, &exec_event(receipt))
                .is_err()
        );
        engine.retire_fd_table_owner(owner); // stale pre-exec MM has no effect
        assert_eq!(engine.lifetime.task_files(new).unwrap(), receipt.new_files);
        engine.retire_fd_table_owner(child);
        assert_eq!(
            engine.take_lifetime_retired_ports(),
            BTreeSet::from([close.binding.open_file])
        );
        assert_eq!(engine.lifetime.counts(keep.binding.open_file).slots, 1);
    }

    #[test]
    fn changed_exec_event_and_old_receipt_cannot_commit_current_preparation() {
        let (mut engine, owner, files) = setup();
        socket(&mut engine, owner, files, 7, 1);
        let old = exec_receipt(owner, files, 10);
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Exec { receipt: old },
        );
        engine
            .submit_fd_mutation(owner, a.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, a.publication.permit, Err(libc::ENOENT))
            .unwrap();
        engine
            .finish_unchanged_fd_mutation(owner, a.publication.permit)
            .unwrap();
        let current = exec_receipt(owner, files, 11);
        let a = admitted(
            &mut engine,
            owner,
            files,
            NetworkFdMutationKind::Exec { receipt: current },
        );
        engine
            .submit_fd_mutation(owner, a.publication.permit)
            .unwrap();
        let before = format!("{engine:?}");
        assert!(engine.commit_exec_fd_table(old, &exec_event(old)).is_err());
        let mut bad = exec_event(current);
        bad.post_exec_mm = old.mm;
        assert!(engine.commit_exec_fd_table(current, &bad).is_err());
        assert_eq!(format!("{engine:?}"), before);
        engine
            .commit_exec_fd_table(current, &exec_event(current))
            .unwrap();
    }
    #[test]
    fn pre_submit_owner_exit_cancels_only_the_unsubmitted_admission() {
        for which in 0..4 {
            let (mut engine, owner, files) = setup();
            let source = socket(&mut engine, owner, files, 7, 1);
            let kind = match which {
                0 => NetworkFdMutationKind::Socket,
                1 => alias(source, NetworkFdInstallKind::Dup, None, None),
                2 => NetworkFdMutationKind::Clone {
                    flags: CloneFlags::CLONE_FILES,
                },
                _ => NetworkFdMutationKind::Exec {
                    receipt: exec_receipt(owner, files, 10),
                },
            };
            let p = admitted(&mut engine, owner, files, kind).publication.permit;
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                usize::from(which == 1)
            );
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert!(!engine.fd_lifecycle.mutations.contains_key(&p.lease));
            assert!(!engine.fd_publications.contains_key(&files));
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                0
            );
            assert_eq!(
                engine.take_lifetime_retired_ports(),
                BTreeSet::from([source.binding.open_file])
            );
            engine.finish_fd_mutations().unwrap();
        }
    }

    #[test]
    fn submitted_alias_owner_exit_keeps_exact_semantic_ref_and_unknown_effect() {
        for result in [None, Some(Ok(8))] {
            let (mut engine, owner, files) = setup();
            let source = socket(&mut engine, owner, files, 7, 1);
            let a = admitted(
                &mut engine,
                owner,
                files,
                alias(source, NetworkFdInstallKind::Dup, None, None),
            );
            let p = a.publication.permit;
            engine.submit_fd_mutation(owner, p).unwrap();
            if let Some(result) = result {
                engine.confirm_fd_mutation_result(owner, p, result).unwrap();
            }
            assert!(engine.cancel_unsubmitted_fd_mutation(owner, p).is_err());
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 0);
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                1
            );
            assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
            assert!(engine.take_lifetime_retired_ports().is_empty());
            assert_eq!(engine.fd_publications[&files].active, Some(p));
            assert!(engine.finish_fd_mutations().is_err());
        }
    }

    #[test]
    fn known_error_and_same_fd_dup2_release_pins_without_replacing_binding() {
        for result in [Err(libc::EBADF), Ok(7)] {
            let (mut engine, owner, files) = setup();
            let source = socket(&mut engine, owner, files, 7, 1);
            let a = admitted(
                &mut engine,
                owner,
                files,
                alias(source, NetworkFdInstallKind::Dup2, Some(7), Some(source)),
            );
            let p = a.publication.permit;
            assert_eq!(
                a.controls.len(),
                1,
                "same OFD appears only once in atomic control set"
            );
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                1
            );
            engine.submit_fd_mutation(owner, p).unwrap();
            engine.confirm_fd_mutation_result(owner, p, result).unwrap();
            engine.finish_unchanged_fd_mutation(owner, p).unwrap();
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                0
            );
            assert_eq!(
                engine
                    .lifetime
                    .descriptor_binding(
                        TaskOwner {
                            tid: owner.thread,
                            mm: owner.mm
                        },
                        7
                    )
                    .unwrap(),
                source.binding
            );
            assert!(engine.take_lifetime_retired_ports().is_empty());
            engine.retire_fd_table_owner(owner);
            assert_eq!(
                engine.take_lifetime_retired_ports(),
                BTreeSet::from([source.binding.open_file])
            );
        }
    }

    #[test]
    fn replacement_publishes_retirement_before_ack_and_leaves_source_aliases() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let target = socket(&mut engine, owner, files, 8, 2);
        let a = admitted(
            &mut engine,
            owner,
            files,
            alias(source, NetworkFdInstallKind::Dup2, Some(8), Some(target)),
        );
        let p = a.publication.permit;
        assert_eq!(a.controls.len(), 2);
        for slot in [source, target] {
            assert_eq!(engine.lifetime.counts(slot.binding.open_file).transports, 1);
        }
        engine.submit_fd_mutation(owner, p).unwrap();
        engine.confirm_fd_mutation_result(owner, p, Ok(8)).unwrap();
        let after = slot(owner, 8, 3, source.binding.open_file);
        let change = NetworkFdSlotReplacement {
            files,
            installation_generation: 3,
            before: Some(target),
            after: Some(after),
        };
        let effect = engine.confirm_fd_installation(owner, p, change).unwrap();
        let batch = NetworkFdPublicationBatch {
            files,
            sequence: 3,
            previous_generation: 2,
            through_generation: 3,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        };
        engine.publish_fd_publication(owner, p, &batch).unwrap();
        assert_eq!(engine.fd_publications[&files].pending, Some(batch.clone()));
        assert_eq!(
            engine.take_lifetime_retired_ports(),
            BTreeSet::from([target.binding.open_file])
        );
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 2);
        assert_eq!(
            engine.lifetime.counts(source.binding.open_file).transports,
            0
        );
        assert_eq!(
            engine.lifetime.counts(target.binding.open_file).transports,
            0
        );
        assert!(engine.fd_publication_fixture_is_retired(target.binding.open_file));
        engine.acknowledge_fd_publication(owner, p, &batch).unwrap();
        assert!(engine.take_lifetime_retired_ports().is_empty());
    }
}
