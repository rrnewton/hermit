//! Physical descriptor mutation admission. Config and publication cursors grant
//! no capability. The owned stopped-root census can activate the intercepted
//! single-root Record operation set; its entry gate refuses all unjoined shapes.

use reverie::syscalls::CloneFlags;

use super::*;
use crate::types::DetPid;
use crate::types::ExecFilesReceipt;
use crate::types::FdSlotBinding;
use crate::types::NetworkFdSlot;
use crate::types::NetworkFdSlotReplacement;

/// Run-private admission for the intercepted descriptor-table operation set.
/// Production issuance consumes the retained stopped-root census atomically;
/// unsupported shapes are refused at syscall entry before a physical effect.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NetworkFdTableCapability(());

/// Config cannot issue run-private FD authority. Production admission is the
/// retained initial-census transaction, never this deserializable configuration.
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
    /// Original Openat, admitted for publication only after its real return.
    Openat,
    /// Original epoll_create/epoll_create1 after the exact native return.
    EpollCreate,
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
    native_birth: Option<crate::network_runtime::native_birth::NativeBirthAdmission>,
}

#[derive(Debug, Default)]
pub(super) struct FdLifecycleState {
    capability: Option<NetworkFdTableCapability>,
    mutations: BTreeMap<NetworkStreamLeaseId, FdMutationState>,
    pub(super) retired_ports: BTreeSet<OpenFileId>,
}

fn protocol(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.to_owned())
}

impl NetworkFdPublicationPermit {
    /// Correlation for the existing private provider command; possession of the
    /// number is not authority without this full retained permit.
    pub(crate) fn native_command_call(self) -> u64 {
        self.lease.0
    }
}

impl NetworkReplayEngine {
    pub(super) fn validate_original_socket_mutation(
        &self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdMutationAdmission,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, admission.publication.permit)?;
        if !self.fd_table_capability()
            || state.admission != *admission
            || !state.submitted
            || state.kernel_result.is_some()
            || state.installation_confirmed
            || admission.kind != NetworkFdMutationKind::Socket
            || !admission.controls.is_empty()
        {
            return Err(protocol(
                "Socket Call must consume its exact submitted allocator permit",
            ));
        }
        Ok(())
    }

    /// Only the existing original Call's positive pre-submission/disarm path
    /// calls this after it has proved the actual invocation never entered.
    pub(super) fn discard_uninvoked_socket_mutation(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        if state.admission.kind != NetworkFdMutationKind::Socket
            || !state.submitted
            || state.kernel_result.is_some()
            || state.installation_confirmed
            || !state.admission.controls.is_empty()
        {
            return Err(protocol(
                "uninvoked Socket changed its submitted allocator custody",
            ));
        }
        self.fd_lifecycle.mutations.remove(&permit.lease);
        Ok(())
    }

    /// Called only after the existing original Call authenticates its actual
    /// completion. A blocking allocator owns no table permit during execution.
    pub(super) fn admit_completed_original_allocation(
        &mut self,
        owner: NetworkStreamOwner,
        files: FilesId,
        kind: super::original_connect::Kind,
        returned: i64,
    ) -> Result<NetworkFdMutationAdmission, NetworkReplayError> {
        let kind = match kind {
            super::original_connect::Kind::Socket => NetworkFdMutationKind::Socket,
            super::original_connect::Kind::Openat => NetworkFdMutationKind::Openat,
            super::original_connect::Kind::EpollCreate { .. } => NetworkFdMutationKind::EpollCreate,
            _ => {
                return Err(protocol(
                    "non-allocator requested completed allocation admission",
                ));
            }
        };
        if !self.fd_table_capability() || !(-4095..=i64::from(i32::MAX)).contains(&returned) {
            return Err(protocol(
                "completed allocator lost capability or actual result",
            ));
        }
        let publication = self.acquire_fd_publication(owner, files)?;
        let admission = NetworkFdMutationAdmission {
            publication,
            kind,
            controls: Vec::new(),
        };
        let lease = admission.publication.permit.lease;
        // Same retained mutation slot as every other FD publisher. `submitted`
        // refers to the already-completed original Call, never a second syscall.
        assert!(
            self.fd_lifecycle
                .mutations
                .insert(
                    lease,
                    FdMutationState {
                        admission: admission.clone(),
                        submitted: true,
                        kernel_result: Some(if returned < 0 {
                            Err((-returned) as i32)
                        } else {
                            Ok(returned)
                        }),
                        installation_confirmed: false,
                        clone_child: None,
                        native_birth: None,
                    }
                )
                .is_none()
        );
        Ok(admission)
    }

    /// Physical confirmation is consumed by the atomic lifetime publication.
    /// Recovery must name exactly one side of that transition: either the
    /// original unconsumed confirmation, or the identical pending/history pair.
    /// Absence of a confirmation by itself grants no recovery authority.
    fn terminal_allocator_prefix_applied(
        &self,
        prior: &NetworkFdMutationAdmission,
        recovery: Option<&NetworkFdPublicationBatch>,
    ) -> Result<bool, NetworkReplayError> {
        let permit = prior.publication.permit;
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("terminal allocator lost its publication state"))?;
        let confirmation = self.fd_installations.contains_key(&permit.lease);
        let Some(batch) = recovery else {
            if confirmation || publication.enrollment.is_some() || publication.pending.is_some() {
                return Err(protocol(
                    "unprepared terminal allocator has retained installation effects",
                ));
            }
            return Ok(false);
        };
        let plan = publication
            .enrollment
            .as_ref()
            .ok_or_else(|| protocol("terminal allocator recovery lost its exact enrollment"))?;
        if plan.permit != permit || plan.batch() != batch {
            return Err(protocol(
                "terminal allocator recovery changed its original enrollment prefix",
            ));
        }
        let history = self
            .fd_publication_history
            .get(&(permit.files, batch.sequence));
        match (confirmation, publication.pending.as_ref(), history) {
            (true, None, None) if !plan.completed => Ok(false),
            (false, Some(pending), Some(history)) if pending == batch && history == batch => {
                Ok(true)
            }
            _ => Err(protocol(
                "terminal allocator confirmation does not match its exact publication phase",
            )),
        }
    }

    /// Transfer only an already-completed allocator's unconfirmed publication
    /// lease. No syscall or physical effect is discarded/reissued. The original
    /// Call retains the previous admission as well as the exact raw result.
    pub(super) fn transfer_terminal_allocator_mutation(
        &mut self,
        prior: &NetworkFdMutationAdmission,
        publisher: NetworkStreamOwner,
        returned: i64,
        recovery: Option<NetworkFdPublicationBatch>,
    ) -> Result<NetworkFdMutationAdmission, NetworkReplayError> {
        let permit = prior.publication.permit;
        let task = self.publication_owner(publisher, permit.files)?;
        let state = self.fd_lifecycle.mutations.get(&permit.lease);
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("terminal allocator lost its publication state"))?;
        let expected = if returned < 0 {
            Err((-returned) as i32)
        } else {
            Ok(returned)
        };
        let recovering = recovery.is_some();
        let applied = self.terminal_allocator_prefix_applied(prior, recovery.as_ref())?;
        let cursor = self
            .lifetime
            .publication_cursor(task)
            .map_err(|e| protocol(&e.to_string()))?;
        let mutation_valid = state.map_or_else(
            || {
                publication
                    .enrollment
                    .as_ref()
                    .is_some_and(|plan| recovering && plan.completed)
            },
            |state| {
                state.admission == *prior
                    && state.submitted
                    && state.kernel_result == Some(expected)
                    && state.installation_confirmed == recovering
                    && state.clone_child.is_none()
                    && state.native_birth.is_none()
            },
        );
        if !self.gone_stream_owners.contains(&permit.owner)
            || !mutation_valid
            || !prior.controls.is_empty()
            || !matches!(
                prior.kind,
                NetworkFdMutationKind::Socket
                    | NetworkFdMutationKind::Openat
                    | NetworkFdMutationKind::EpollCreate
            )
            || prior.publication.recovery.is_some()
            || publication.reader.is_some()
            || publication.active.is_some_and(|active| active != permit)
            || (publication.active.is_none()
                && !publication
                    .enrollment
                    .as_ref()
                    .is_some_and(|plan| recovering && plan.completed))
            || (recovering != publication.enrollment.is_some())
            || publication
                .pending
                .as_ref()
                .is_some_and(|batch| recovery.as_ref() != Some(batch))
            || cursor
                != if applied {
                    let batch = recovery
                        .as_ref()
                        .expect("applied recovery has an exact prefix");
                    (batch.sequence, batch.through_generation)
                } else {
                    (
                        prior.publication.acknowledged_sequence,
                        prior.publication.acknowledged_generation,
                    )
                }
        {
            return Err(protocol(
                "terminal allocator changed its exact retained publication prefix",
            ));
        }
        let mut transferred = prior.clone();
        transferred.publication.permit.owner = publisher;
        transferred.publication.acknowledged_sequence = cursor.0;
        transferred.publication.acknowledged_generation = cursor.1;
        transferred.publication.recovery = recovery;
        if let Some(state) = self.fd_lifecycle.mutations.get_mut(&permit.lease) {
            state.admission = transferred.clone();
        }
        self.fd_publications.get_mut(&permit.files).unwrap().active =
            Some(transferred.publication.permit);
        Ok(transferred)
    }

    /// Complete a known historical Install->Remove only after the last real
    /// table owner and pending shares have gone. Its admission/result are still
    /// retained by the original Call until the normal retirement join.
    pub(super) fn retire_terminal_allocator_mutation(
        &mut self,
        prior: &NetworkFdMutationAdmission,
        returned: i64,
        recovery: Option<&NetworkFdPublicationBatch>,
    ) -> Result<(), NetworkReplayError> {
        let permit = prior.publication.permit;
        let state = self.fd_lifecycle.mutations.get(&permit.lease);
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("terminal allocator lost its retained publication"))?;
        let recovering = recovery.is_some();
        self.terminal_allocator_prefix_applied(prior, recovery)?;
        let mutation_valid = state.map_or_else(
            || {
                publication
                    .enrollment
                    .as_ref()
                    .is_some_and(|plan| recovering && plan.completed)
            },
            |state| {
                state.admission == *prior
                    && state.submitted
                    && state.kernel_result == Some(Ok(returned))
                    && state.installation_confirmed == recovering
                    && state.clone_child.is_none()
                    && state.native_birth.is_none()
            },
        );
        if self.lifetime.table_exists(permit.files)
            || !self.gone_stream_owners.contains(&permit.owner)
            || !mutation_valid
            || !prior.controls.is_empty()
            || !matches!(
                prior.kind,
                NetworkFdMutationKind::Socket
                    | NetworkFdMutationKind::Openat
                    | NetworkFdMutationKind::EpollCreate
            )
            || prior.publication.recovery.is_some()
            || publication.reader.is_some()
            || publication.active.is_some_and(|active| active != permit)
            || (publication.active.is_none()
                && !publication
                    .enrollment
                    .as_ref()
                    .is_some_and(|plan| recovering && plan.completed))
            || recovering != publication.enrollment.is_some()
            || publication
                .pending
                .as_ref()
                .is_some_and(|batch| recovery != Some(batch))
        {
            return Err(protocol(
                "terminal allocator cannot discard a live or unmatched publication",
            ));
        }
        let mut lifetime = self.lifetime.clone();
        lifetime
            .prune_dead_table_publication(permit.files)
            .map_err(|error| protocol(&error.to_string()))?;
        // The caller transfers the exact prior admission/enrollment and fresh
        // physical receipt into the original Call before its final removal.
        self.fd_lifecycle.mutations.remove(&permit.lease);
        self.fd_installations.remove(&permit.lease);
        self.fd_publications.remove(&permit.files);
        self.fd_publication_history
            .retain(|(files, _), _| *files != permit.files);
        self.lifetime = lifetime;
        Ok(())
    }

    pub(super) fn retire_terminal_failed_allocator_mutation(
        &mut self,
        prior: &NetworkFdMutationAdmission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        let permit = prior.publication.permit;
        if !(-4095..0).contains(&returned)
            || !self.gone_stream_owners.contains(&permit.owner)
            || !prior.controls.is_empty()
            || prior.publication.recovery.is_some()
            || !matches!(
                prior.kind,
                NetworkFdMutationKind::Socket
                    | NetworkFdMutationKind::Openat
                    | NetworkFdMutationKind::EpollCreate
            )
            || self.fd_installations.contains_key(&permit.lease)
        {
            return Err(protocol(
                "terminal negative allocator changed known no-effect custody",
            ));
        }
        let publication = self.fd_publications.get(&permit.files);
        if let Some(state) = self.fd_lifecycle.mutations.get(&permit.lease) {
            if state.admission != *prior
                || !state.submitted
                || state.kernel_result != Some(Err((-returned) as i32))
                || state.installation_confirmed
                || state.clone_child.is_some()
                || state.native_birth.is_some()
                || publication.is_none_or(|p| {
                    p.active != Some(permit)
                        || p.pending.is_some()
                        || p.enrollment.is_some()
                        || p.reader.is_some()
                })
            {
                return Err(protocol(
                    "terminal negative allocator still has unmatched mutation effects",
                ));
            }
            self.fd_lifecycle.mutations.remove(&permit.lease);
            self.fd_publications.get_mut(&permit.files).unwrap().active = None;
            self.prune_dead_fd_publication(permit.files);
        } else if publication.is_some_and(|p| {
            p.active == Some(permit)
                || p.enrollment
                    .as_ref()
                    .is_some_and(|plan| plan.permit == permit)
        }) {
            return Err(protocol(
                "negative allocator cleanup lost mutation before publication retirement",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_original_socket_publication(
        &self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdMutationAdmission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        let permit = admission.publication.permit;
        self.validate_publication_permit(owner, permit)?;
        if !matches!(
            admission.kind,
            NetworkFdMutationKind::Socket
                | NetworkFdMutationKind::Openat
                | NetworkFdMutationKind::EpollCreate
        ) || !admission.controls.is_empty()
            || !(-4095..=i64::from(i32::MAX)).contains(&returned)
        {
            return Err(protocol("Socket publication changed its original mutation"));
        }
        let expected = if returned < 0 {
            Err((-returned) as i32)
        } else {
            Ok(returned)
        };
        if let Some(state) = self.fd_lifecycle.mutations.get(&permit.lease) {
            if state.admission != *admission
                || !state.submitted
                || state.kernel_result.is_some_and(|prior| prior != expected)
                || (state.installation_confirmed && state.kernel_result != Some(expected))
            {
                return Err(protocol(
                    "Socket publication changed its retained native result",
                ));
            }
        } else {
            // Publication may have consumed the mutation before local ACK.
            // Only its exact completed enrollment/pending batch closes that gap.
            self.validate_completed_socket_publication(admission, returned)?;
        }
        Ok(())
    }

    pub(super) fn retain_original_socket_publication_result(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdMutationAdmission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        self.validate_original_socket_publication(owner, admission, returned)?;
        if self
            .fd_lifecycle
            .mutations
            .get(&admission.publication.permit.lease)
            .is_some_and(|state| state.kernel_result.is_none())
        {
            self.confirm_fd_mutation_result(
                owner,
                admission.publication.permit,
                if returned < 0 {
                    Err((-returned) as i32)
                } else {
                    Ok(returned)
                },
            )?;
        }
        Ok(())
    }

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

    /// Empty tables exist only in controlled legacy component fixtures. The
    /// production path must consume the private initial census below.
    #[cfg(test)]
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

    /// Called only inside the retained runtime + stopped-root scheduler commit.
    /// This grants the bounded original-operation route whose dispatch rejects
    /// unsupported descriptor mutations before native entry. Config and the
    /// serializable view/claim cannot manufacture the retained association.
    pub(crate) fn admit_initial_record_census(
        &mut self,
        association: &crate::network_runtime::InitialTableAssociation,
        claim: &crate::network_runtime::InitialTableClaim,
        process: DetPid,
    ) -> Result<(), NetworkReplayError> {
        if self.fd_table_capability()
            || !self.fd_lifecycle.mutations.is_empty()
            || !self.fd_publications.is_empty()
            || !self.fd_installations.is_empty()
            || !self.fd_publication_history.is_empty()
        {
            return Err(protocol(
                "initial capability was already issued or publication started",
            ));
        }
        self.check_initial_accepted_record()?;
        association
            .validate_root_identity()
            .map_err(|e| protocol(&e.to_string()))?;
        association
            .check_claim(claim)
            .map_err(|e| protocol(&e.to_string()))?;
        // An inherited socket has no enrolled stream/profile in the untouched
        // Record engine. Its exact census must not mint partial socket authority.
        // O_PATH is classified by the same existing physical profile predicate.
        if claim.view.descriptors.iter().any(|row| {
            crate::fd::FdType::from_initial_profile(
                row.mode,
                row.status_flags,
                row.device_major,
                row.device_minor,
            ) == Some(crate::fd::FdType::Socket)
        }) {
            return Err(protocol(
                "initial Record socket profile enrollment is not implemented",
            ));
        }
        let owner = association.owner();
        self.check_stream_owner(owner)?;
        self.lifetime
            .register_census(
                TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                },
                process,
                claim.view.files,
                &claim.slots,
                claim.through_generation,
            )
            .map_err(|e| protocol(&e.to_string()))?;
        // register_census validates into a candidate before replacing the ledger.
        // Nothing fallible or asynchronous follows its commit under this lock.
        self.fd_lifecycle.capability = Some(NetworkFdTableCapability(()));
        self.activate_initial_accepted_record();
        Ok(())
    }

    /// Replay authenticates the live initial descriptor table at the same
    /// stopped-root/scheduler commit as Record.  The serialized network trace
    /// is not authority for a host task, MM, files_struct, or descriptor slot;
    /// those facts come only from the retained association and census claim.
    /// Unlike Record, Replay already owns its decoded channel/accepted model,
    /// so admission must not apply the untouched-Record predicate or mutate
    /// that model.
    pub(crate) fn admit_initial_replay_census(
        &mut self,
        association: &crate::network_runtime::InitialTableAssociation,
        claim: &crate::network_runtime::InitialTableClaim,
        process: DetPid,
    ) -> Result<(), NetworkReplayError> {
        if self.mode() != NetworkEngineMode::Replay {
            return Err(NetworkReplayError::WrongMode);
        }
        if self.fd_table_capability()
            || !self.fd_lifecycle.mutations.is_empty()
            || !self.fd_publications.is_empty()
            || !self.fd_installations.is_empty()
            || !self.fd_publication_history.is_empty()
        {
            return Err(protocol(
                "initial Replay capability was already issued or publication started",
            ));
        }
        association
            .validate_root_identity()
            .map_err(|e| protocol(&e.to_string()))?;
        association
            .check_claim(claim)
            .map_err(|e| protocol(&e.to_string()))?;
        // No trace version currently serializes an inherited-socket identity
        // join.  Matching only the numeric slot would mint authority, so keep
        // the same fail-closed boundary as Record.
        if claim.view.descriptors.iter().any(|row| {
            crate::fd::FdType::from_initial_profile(
                row.mode,
                row.status_flags,
                row.device_major,
                row.device_minor,
            ) == Some(crate::fd::FdType::Socket)
        }) {
            return Err(protocol(
                "initial Replay socket profile enrollment is not implemented",
            ));
        }
        let owner = association.owner();
        self.check_stream_owner(owner)?;
        self.lifetime
            .register_census(
                TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                },
                process,
                claim.view.files,
                &claim.slots,
                claim.through_generation,
            )
            .map_err(|e| protocol(&e.to_string()))?;
        self.fd_lifecycle.capability = Some(NetworkFdTableCapability(()));
        Ok(())
    }

    pub(crate) fn register_initial_census(
        &mut self,
        association: &crate::network_runtime::InitialTableAssociation,
        claim: &crate::network_runtime::InitialTableClaim,
        process: DetPid,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Err(protocol("initial census cannot grant backend capability"));
        }
        association
            .check_claim(claim)
            .map_err(|e| protocol(&e.to_string()))?;
        let owner = association.owner();
        self.check_stream_owner(owner)?;
        self.lifetime
            .register_census(
                TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                },
                process,
                claim.view.files,
                &claim.slots,
                claim.through_generation,
            )
            .map_err(|e| protocol(&e.to_string()))
    }

    fn validate_fd_mutation_kind(
        &self,
        owner: NetworkStreamOwner,
        files: FilesId,
        kind: &NetworkFdMutationKind,
    ) -> Result<Option<Vec<OpenFileId>>, NetworkReplayError> {
        let task = self.publication_owner(owner, files)?;
        match kind {
            NetworkFdMutationKind::Socket
            | NetworkFdMutationKind::Openat
            | NetworkFdMutationKind::EpollCreate
            | NetworkFdMutationKind::Clone { .. } => Ok(Some(Vec::new())),
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
        if matches!(
            kind,
            NetworkFdMutationKind::Openat | NetworkFdMutationKind::EpollCreate
        ) {
            return Err(protocol(
                "Openat cannot acquire a publication permit before native completion",
            ));
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
        let controls = match self.begin_descriptor_controls(owner, controls) {
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
                        native_birth: None,
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

    /// Switch the already submitted clone escrow to an unresolved native
    /// outcome before arming its provider command. The same permit remains
    /// exclusive through actual child admission, even after creator exit.
    pub(crate) fn prepare_native_birth_escrow(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(owner, permit)?;
        if !state.submitted
            || state.kernel_result.is_some()
            || state.clone_child.is_some()
            || !matches!(state.admission.kind, NetworkFdMutationKind::Clone { .. })
        {
            return Err(protocol(
                "native birth does not name a submitted original clone",
            ));
        }
        self.lifetime
            .defer_clone_choice(clone_ticket(permit))
            .map_err(|e| protocol(&e.to_string()))
    }

    /// The only native outcome selector takes private runtime authority from
    /// the retained creator command and backend child event, not RPC fields.
    pub(crate) fn admit_native_birth(
        &mut self,
        birth: &crate::network_runtime::native_birth::NativeBirthAdmission,
    ) -> Result<(), NetworkReplayError> {
        let permit = birth.permit();
        let state = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .filter(|s| {
                s.admission.publication.permit == permit
                    && s.submitted
                    && s.clone_child.is_none()
                    && s.kernel_result.is_none_or(|result| result.is_ok())
            })
            .ok_or_else(|| protocol("native birth lost original clone escrow"))?;
        if state.admission.kind
            != (NetworkFdMutationKind::Clone {
                flags: birth.flags(),
            })
        {
            return Err(protocol(
                "native birth changed immutable original clone request",
            ));
        }
        if let Some(old) = &state.native_birth {
            return if old == birth {
                Ok(())
            } else {
                Err(protocol("native birth outcome changed"))
            };
        }
        self.lifetime
            .resolve_clone_choice(clone_ticket(permit), birth.shared_files())
            .map_err(|e| protocol(&e.to_string()))?;
        // Request equality still governs cancellation and errno. Successful
        // inheritance consumes this separately authenticated actual outcome.
        self.fd_lifecycle
            .mutations
            .get_mut(&permit.lease)
            .unwrap()
            .native_birth = Some(birth.clone());
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
                return Err(protocol("parent clone result contradicts observed child"));
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
            NetworkFdMutationKind::EpollCreate => {
                if change.before.is_some() || after.binding.open_file.is_socket() {
                    return Err(protocol(
                        "original epoll replaced a tracked slot or socket class",
                    ));
                }
                (
                    NetworkFdInstallKind::EpollCreate,
                    SlotInstallationSource::Fresh,
                )
            }
            NetworkFdMutationKind::Openat => {
                if change.before.is_some() {
                    return Err(protocol(
                        "original Openat replaced an occupied tracked slot",
                    ));
                }
                (NetworkFdInstallKind::Openat, SlotInstallationSource::Fresh)
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
                original_creation: None,
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

    pub(crate) fn finish_fd_mutations(&self) -> Result<(), NetworkReplayError> {
        if let Some(plan) = self
            .fd_publications
            .values()
            .find_map(|state| state.enrollment.as_ref())
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(
                plan.permit.lease,
            ));
        }
        if let Some((&lease, _)) = self.fd_lifecycle.mutations.first_key_value() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        // Logical Replay owns registered tasks and operation leases even when
        // it has no native descriptor-table capability.
        if self.fd_table_capability() || self.mode() == NetworkEngineMode::Replay {
            self.lifetime
                .finish()
                .map_err(|error| protocol(&error.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum InheritedCloneDisposition {
    Admit,
    RetireBeforeStart,
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

    /// Read-only fence before disarming an actually uninvoked provider command.
    /// The existing mutation and publication remain held across that await.
    pub(crate) fn validate_uninvoked_clone_admission(
        &self,
        admission: &NetworkFdMutationAdmission,
    ) -> Result<(), NetworkReplayError> {
        let permit = admission.publication.permit;
        let state = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .ok_or_else(|| protocol("uninvoked clone admission is not retained"))?;
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("uninvoked clone lost table publication"))?;
        if !matches!(admission.kind, NetworkFdMutationKind::Clone { .. })
            || state.admission != *admission
            || !admission.controls.is_empty()
            || state.kernel_result.is_some()
            || (state.clone_child.is_some() || state.native_birth.is_some())
            || state.installation_confirmed
            || publication.active != Some(permit)
            || publication.pending.is_some()
        {
            return Err(protocol("uninvoked clone changed exact pending custody"));
        }
        Ok(())
    }

    /// Settle the exact returned clone admission retained before its Submit RPC.
    /// The consuming callback supplies non-invocation, never parent death alone.
    pub(crate) fn cancel_uninvoked_clone_admission(
        &mut self,
        admission: &NetworkFdMutationAdmission,
    ) -> Result<(), NetworkReplayError> {
        let permit = admission.publication.permit;
        let NetworkFdMutationKind::Clone { flags } = admission.kind else {
            return Err(protocol("uninvoked clone receipt has another operation"));
        };
        let state = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .ok_or_else(|| protocol("uninvoked clone admission is not retained"))?;
        if state.admission != *admission || !admission.controls.is_empty() {
            return Err(protocol("uninvoked clone admission changed exact custody"));
        }
        if !state.submitted {
            return self.cancel_unsubmitted_fd_mutation(permit.owner, permit);
        }
        self.cancel_uninvoked_cloned_fd_table(permit, flags)
    }

    /// Only the consuming backend ThreadState's exact uninvoked marker may call
    /// this path. Parent death alone cannot settle a submitted native clone.
    pub(crate) fn cancel_uninvoked_cloned_fd_table(
        &mut self,
        permit: NetworkFdPublicationPermit,
        flags: CloneFlags,
    ) -> Result<(), NetworkReplayError> {
        let state = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .ok_or_else(|| protocol("uninvoked clone lost its retained mutation"))?;
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("uninvoked clone lost its table admission"))?;
        if state.admission.publication.permit != permit
            || state.admission.kind != (NetworkFdMutationKind::Clone { flags })
            || !state.submitted
            || state.kernel_result.is_some()
            || (state.clone_child.is_some() || state.native_birth.is_some())
            || state.installation_confirmed
            || !state.admission.controls.is_empty()
            || publication.active != Some(permit)
            || publication.pending.is_some()
        {
            return Err(protocol(
                "uninvoked clone contradicts retained physical state",
            ));
        }
        let retired = self
            .lifetime
            .cancel_clone(clone_ticket(permit))
            .map_err(|error| protocol(&error.to_string()))?;
        self.retire_lifetime_open_files(retired);
        self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
        self.fd_publications.get_mut(&permit.files).unwrap().active = None;
        self.prune_dead_fd_publication(permit.files);
        Ok(())
    }

    /// Settle one exact known failed clone at normal return or actual terminal
    /// observation. The caller owns the original birth reservation and native
    /// errno. A missing mutation or different result remains an error.
    pub(crate) fn settle_failed_cloned_fd_table(
        &mut self,
        permit: NetworkFdPublicationPermit,
        flags: CloneFlags,
        errno: i32,
    ) -> Result<(), NetworkReplayError> {
        let state = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .ok_or_else(|| protocol("failed clone lost retained mutation"))?;
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("failed clone lost table admission"))?;
        if !(1..=4095).contains(&errno)
            || state.admission.publication.permit != permit
            || state.admission.kind != (NetworkFdMutationKind::Clone { flags })
            || !state.submitted
            || (state.clone_child.is_some() || state.native_birth.is_some())
            || state.installation_confirmed
            || !state.admission.controls.is_empty()
            || state
                .kernel_result
                .is_some_and(|result| result != Err(errno))
            || publication.active != Some(permit)
            || publication.pending.is_some()
        {
            return Err(protocol("known failed clone contradicts exact custody"));
        }
        let retired = self
            .lifetime
            .cancel_clone(clone_ticket(permit))
            .map_err(|error| protocol(&error.to_string()))?;
        self.retire_lifetime_open_files(retired);
        self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
        self.fd_publications.get_mut(&permit.files).unwrap().active = None;
        self.prune_dead_fd_publication(permit.files);
        Ok(())
    }

    pub(crate) fn validate_child_birth_permit(
        &self,
        parent: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
        flags: CloneFlags,
    ) -> Result<(), NetworkReplayError> {
        let state = self.fd_mutation(parent, permit)?;
        if !state.submitted
            || state.kernel_result.is_some()
            || state.clone_child.is_some()
            || state.admission.kind != (NetworkFdMutationKind::Clone { flags })
        {
            return Err(protocol("birth preparation lacks exact submitted clone"));
        }
        Ok(())
    }

    /// Consume the exact clone permit inherited by a real backend child. Unlike
    /// ordinary parent publication, the submitting task may already be retired.
    /// The caller authenticates the child callback and inherited birth metadata;
    /// this method authenticates the still-retained physical submission and the
    /// original table reservation. It never recovers a permit by scanning a PID.
    pub(crate) fn register_inherited_cloned_fd_table(
        &mut self,
        permit: NetworkFdPublicationPermit,
        child: NetworkStreamOwner,
        process: DetPid,
        flags: CloneFlags,
    ) -> Result<(), NetworkReplayError> {
        self.complete_inherited_clone(
            permit,
            child,
            process,
            flags,
            InheritedCloneDisposition::Admit,
        )
    }

    /// Called only after the exact inherited birth and native NewChild were
    /// matched at the backend's actual final wait, before any child startup.
    pub(crate) fn retire_prestart_cloned_fd_table(
        &mut self,
        permit: NetworkFdPublicationPermit,
        child: NetworkStreamOwner,
        process: DetPid,
        flags: CloneFlags,
    ) -> Result<(), NetworkReplayError> {
        self.complete_inherited_clone(
            permit,
            child,
            process,
            flags,
            InheritedCloneDisposition::RetireBeforeStart,
        )
    }

    fn complete_inherited_clone(
        &mut self,
        permit: NetworkFdPublicationPermit,
        child: NetworkStreamOwner,
        process: DetPid,
        flags: CloneFlags,
        disposition: InheritedCloneDisposition,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Err(protocol("inherited clone cannot enable table capability"));
        }
        self.check_stream_owner(child)?;
        let state = self
            .fd_lifecycle
            .mutations
            .get(&permit.lease)
            .ok_or_else(|| protocol("inherited clone has no retained submission"))?;
        let publication = self
            .fd_publications
            .get(&permit.files)
            .ok_or_else(|| protocol("inherited clone lost its table publication"))?;
        let actual_matches = match &state.native_birth {
            Some(actual) => {
                actual.child_owner() == child
                    && actual.child_process() == process
                    && actual.terminal()
                        == matches!(disposition, InheritedCloneDisposition::RetireBeforeStart)
                    && actual.actual_flags() == flags
            }
            None => state.admission.kind == (NetworkFdMutationKind::Clone { flags }),
        };
        if state.admission.publication.permit != permit
            || !actual_matches
            || !state.submitted
            || state.clone_child.is_some()
            || state.installation_confirmed
            || !state.admission.controls.is_empty()
            || publication.active != Some(permit)
            || publication.pending.is_some()
            || child.thread == permit.owner.thread
            || child.mm
                != crate::types::MmId::for_clone(
                    permit.owner.mm,
                    child.thread,
                    flags.contains(CloneFlags::CLONE_VM),
                )
            || state
                .kernel_result
                .is_some_and(|result| result != Ok(i64::from(child.thread.as_raw())))
        {
            return Err(protocol(
                "inherited child contradicts exact clone submission",
            ));
        }
        let parent_returned = state.kernel_result.is_some();
        let parent_retired = self
            .lifetime
            .task_files(TaskOwner {
                tid: permit.owner.thread,
                mm: permit.owner.mm,
            })
            .is_err();
        let child_task = TaskOwner {
            tid: child.thread,
            mm: child.mm,
        };
        if matches!(disposition, InheritedCloneDisposition::RetireBeforeStart) {
            let retired = self
                .lifetime
                .retire_prestart_clone(clone_ticket(permit), child_task)
                .map_err(|error| protocol(&error.to_string()))?;
            self.retire_lifetime_open_files(retired);
        } else {
            self.lifetime
                .commit_clone(clone_ticket(permit), child_task, process)
                .map_err(|error| protocol(&error.to_string()))?;
        }
        // All table/lease/pending checks precede the lifetime commit. This is
        // the one child-consumption path; generic publication still requires
        // the live submitting owner and is deliberately unchanged.
        self.fd_publications.get_mut(&permit.files).unwrap().active = None;
        if parent_returned || parent_retired {
            self.fd_lifecycle.mutations.remove(&permit.lease).unwrap();
        } else {
            self.fd_lifecycle
                .mutations
                .get_mut(&permit.lease)
                .unwrap()
                .clone_child = Some((child_task, process));
        }
        self.prune_dead_fd_publication(permit.files);
        Ok(())
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
        exact: Option<FdSlotBinding>,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Ok(());
        }
        self.retain_registered_stream_call_lifetime(owner, call, open_file, exact)
    }
    // Logical Replay owns the same registered table/slot and lease without a
    // corresponding physical provider slot. Its caller has separately admitted
    // that exact logical publication; this helper never issues native authority.
    pub(super) fn retain_registered_stream_call_lifetime(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        open_file: OpenFileId,
        exact: Option<FdSlotBinding>,
    ) -> Result<(), NetworkReplayError> {
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        let binding = match exact {
            Some(binding) if binding.open_file == open_file => binding,
            Some(_) => return Err(protocol("stream call binding changed OFD")),
            None => self
                .lifetime
                .binding_for_open_file(task, open_file)
                .map_err(|error| protocol(&error.to_string()))?,
        };
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
        self.release_registered_stream_call_lifetime(
            owner,
            call,
            open_file,
            lifetime::TransportResolution::CompletedAndRecorded,
        )
    }
    pub(super) fn release_registered_stream_call_lifetime(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        open_file: OpenFileId,
        resolution: lifetime::TransportResolution,
    ) -> Result<(), NetworkReplayError> {
        let retired = self
            .lifetime
            .acknowledge_transport(stream_call_pin(owner, call), open_file, resolution)
            .map_err(|error| protocol(&error.to_string()))?;
        self.retire_lifetime_open_files(retired);
        Ok(())
    }

    /// Actual backend task consumption closes a table owner even if local Arcs
    /// linger. A stale MM cannot detach a replacement, and operation refs stay.
    pub(crate) fn retire_fd_table_owner(&mut self, owner: NetworkStreamOwner) {
        // A recorded Call may retain an explicitly registered logical table
        // without physical descriptor coverage. Exact TaskOwner lookup below,
        // not Replay mode alone, authorizes this consuming ledger transition.
        if !self.fd_table_capability() && self.mode() != NetworkEngineMode::Replay {
            return;
        }
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        let Ok(files) = self.lifetime.task_files(task) else {
            return;
        };
        // No physical submission exists while the exact read is still retained
        // by its table permit. Transferred reads are owned by Calls instead.
        self.consume_logical_fd_reads(owner);
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
        // An observed child is a positive kernel effect, including a child whose
        // prestart final wait settled its escrow. Parent death cannot undo it.
        self.fd_lifecycle.mutations.retain(|_, state| {
            state.admission.publication.permit.owner != owner || state.clone_child.is_none()
        });
        let retired = self.lifetime.exit(task).expect("exact admitted task owner");
        self.retire_lifetime_open_files(retired);
        self.prune_dead_fd_publication(files);
    }

    pub(super) fn prune_dead_fd_publication(&mut self, files: FilesId) {
        if self.lifetime.table_exists(files)
            || self
                .fd_publications
                .get(&files)
                .is_some_and(|state| state.enrollment.is_some())
        {
            return;
        }
        let unknown = self
            .fd_publications
            .get(&files)
            .and_then(|state| state.active)
            .is_some_and(|permit| {
                self.physical_fd_mutation_pending(permit)
                    || self.native_stream_capture_pending(permit)
            });
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
    pub(crate) fn native_capture_fixture_cancel_unsubmitted(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        self.cancel_unsubmitted_fd_mutation(owner, permit)
    }

    pub(crate) fn native_capture_fixture_counts(
        &self,
        open_file: OpenFileId,
    ) -> (usize, usize, usize, usize) {
        (
            self.stream_calls.len(),
            self.socket_controls.len(),
            self.fd_publications
                .values()
                .filter(|state| state.active.is_some())
                .count(),
            self.lifetime.counts(open_file).transports,
        )
    }
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

    fn recording_finalizers()
    -> [fn(NetworkReplayEngine) -> Result<NetworkTrace, NetworkReplayError>; 2] {
        [
            |engine| engine.into_recorded_trace().map(NetworkTrace::V2),
            NetworkReplayEngine::into_recorded_versioned_trace,
        ]
    }

    #[test]
    fn recording_finalizers_reject_pure_fd_mutation_until_result_consumption_and_owner_retirement()
    {
        for finalize in recording_finalizers() {
            for consumed in [false, true] {
                let (mut engine, owner, files) = setup();
                let EngineState::Record(expected) = &engine.mode else {
                    unreachable!("setup constructs a recorder");
                };
                let expected = expected.clone();
                let admission = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
                let permit = admission.publication.permit;
                engine.submit_fd_mutation(owner, permit).unwrap();
                assert!(admission.controls.is_empty());
                assert!(engine.stream_calls.is_empty());
                assert!(engine.socket_controls.is_empty());
                assert!(engine.stream_operations.is_empty());
                assert!(engine.zero_stream_waits.is_empty());
                if consumed {
                    // Explicit component kernel-error input uses the existing
                    // confirmation/consumption path, never an inferred close.
                    engine
                        .confirm_fd_mutation_result(owner, permit, Err(libc::EMFILE))
                        .unwrap();
                    engine.finish_unchanged_fd_mutation(owner, permit).unwrap();
                    assert!(engine.fd_lifecycle.mutations.is_empty());
                    engine.retire_fd_table_owner(owner);
                    engine.lifetime.finish().unwrap();
                    assert_eq!(finalize(engine).unwrap(), NetworkTrace::V2(expected));
                } else {
                    // With no StreamCall/control/operation, only the exact
                    // pending descriptor mutation can produce this receipt.
                    assert!(matches!(
                        finalize(engine),
                        Err(NetworkReplayError::UnresolvedStreamOperation(actual))
                            if actual == permit.lease
                    ));
                }
            }
        }
    }

    #[test]
    fn recording_finalizers_reject_registered_owner_until_exact_retirement() {
        for finalize in recording_finalizers() {
            for retired in [false, true] {
                let (mut engine, owner, _) = setup();
                let EngineState::Record(expected) = &engine.mode else {
                    unreachable!("setup constructs a recorder");
                };
                let expected = expected.clone();
                assert!(engine.fd_lifecycle.mutations.is_empty());
                assert!(engine.stream_calls.is_empty());
                assert!(engine.socket_controls.is_empty());
                assert!(engine.stream_operations.is_empty());
                assert!(engine.zero_stream_waits.is_empty());
                if retired {
                    engine.retire_fd_table_owner(owner);
                    engine.lifetime.finish().unwrap();
                    assert_eq!(finalize(engine).unwrap(), NetworkTrace::V2(expected));
                } else {
                    assert!(matches!(
                        finalize(engine),
                        Err(NetworkReplayError::FdPublicationProtocol(message))
                            if message == lifetime::LifetimeError::OutstandingOwners.to_string()
                    ));
                }
            }
        }
    }

    #[test]
    fn native_capture_rejects_fixed_source_and_stale_generation_before_call() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let other = socket(&mut engine, owner, files, 8, 2);
        let control = engine
            .begin_socket_controls(owner, vec![source.binding.open_file])
            .unwrap()[0]
            .1;
        let next_call = engine.next_stream_call;
        let mut stale = source.binding;
        stale.generation += 1;
        for invalid in [stale, other.binding] {
            assert!(
                engine
                    .begin_native_stream_call(owner, control, invalid)
                    .is_err()
            );
            assert_eq!(engine.next_stream_call, next_call);
            assert!(engine.stream_calls.is_empty());
            assert!(engine.fd_publications[&files].active.is_none());
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                0
            );
        }
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }

    #[test]
    fn native_capture_rejects_replaced_slot_even_while_old_ofd_alias_survives() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let replacement = socket(&mut engine, owner, files, 8, 2);
        let duplicate = admitted(
            &mut engine,
            owner,
            files,
            alias(source, NetworkFdInstallKind::Dup, None, None),
        );
        engine
            .submit_fd_mutation(owner, duplicate.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, duplicate.publication.permit, Ok(9))
            .unwrap();
        commit_install(
            &mut engine,
            owner,
            &duplicate,
            None,
            slot(owner, 9, 3, source.binding.open_file),
        );
        let replace = admitted(
            &mut engine,
            owner,
            files,
            alias(
                replacement,
                NetworkFdInstallKind::Dup2,
                Some(7),
                Some(source),
            ),
        );
        engine
            .submit_fd_mutation(owner, replace.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, replace.publication.permit, Ok(7))
            .unwrap();
        commit_install(
            &mut engine,
            owner,
            &replace,
            Some(source),
            slot(owner, 7, 4, replacement.binding.open_file),
        );
        let control = engine
            .begin_socket_controls(owner, vec![source.binding.open_file])
            .unwrap()[0]
            .1;
        let next_call = engine.next_stream_call;
        assert!(
            engine
                .begin_native_stream_call(owner, control, source.binding)
                .is_err()
        );
        assert_eq!(engine.next_stream_call, next_call);
        assert!(engine.stream_calls.is_empty());
        assert!(engine.fd_publications[&files].active.is_none());
        assert_eq!(engine.lifetime.counts(source.binding.open_file).slots, 1);
        assert_eq!(
            engine.lifetime.counts(source.binding.open_file).transports,
            0
        );
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }

    #[test]
    fn native_capture_holds_table_until_known_pin_outcome() {
        for outcome in [
            NetworkStreamPinOutcome::Acquired,
            NetworkStreamPinOutcome::Failed(libc::EPERM),
        ] {
            let (mut engine, owner, files) = setup();
            let source = socket(&mut engine, owner, files, 7, 1);
            let control = engine
                .begin_socket_controls(owner, vec![source.binding.open_file])
                .unwrap()[0]
                .1;
            let call = engine
                .begin_native_stream_call(owner, control, source.binding)
                .unwrap();
            let permit = engine.stream_calls[&call.id].capture_publication.unwrap();
            assert!(call.physical_pin_required);
            assert!(engine.native_stream_capture_pending(permit));
            for kind in [
                NetworkFdMutationKind::Socket,
                NetworkFdMutationKind::Clone {
                    flags: CloneFlags::CLONE_FILES,
                },
            ] {
                assert!(matches!(engine.begin_fd_mutation(owner, files, kind),
                    Err(NetworkReplayError::StreamOperationBusy(lease)) if lease == permit.lease));
            }
            assert_eq!(engine.fd_publications[&files].active, Some(permit));
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                1
            );
            engine
                .confirm_stream_call_pin(owner, call.id, outcome)
                .unwrap();
            assert!(!engine.native_stream_capture_pending(permit));
            assert!(engine.fd_publications[&files].active.is_none());
            engine
                .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
                .unwrap();
            if outcome == NetworkStreamPinOutcome::Acquired {
                assert_eq!(
                    engine.lifetime.counts(source.binding.open_file).transports,
                    1
                );
                engine.begin_stream_call_release(owner, call.id).unwrap();
                engine.finish_stream_call_release(owner, call.id).unwrap();
            }
            assert!(engine.stream_calls.is_empty());
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                0
            );
            let next = admitted(&mut engine, owner, files, NetworkFdMutationKind::Socket);
            engine
                .cancel_unsubmitted_fd_mutation(owner, next.publication.permit)
                .unwrap();
        }
    }

    #[test]
    fn native_capture_unknown_owner_exit_keeps_exact_table_and_pin_custody() {
        for shared in [false, true] {
            let (mut engine, owner, files) = setup();
            let sibling = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                mm: owner.mm,
            };
            if shared {
                engine.fd_publication_fixture_register(sibling, Some(owner));
            }
            let source = socket(&mut engine, owner, files, 7, 1);
            let control = engine
                .begin_socket_controls(owner, vec![source.binding.open_file])
                .unwrap()[0]
                .1;
            let call = engine
                .begin_native_stream_call(owner, control, source.binding)
                .unwrap();
            let permit = engine.stream_calls[&call.id].capture_publication.unwrap();
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert!(engine.native_stream_capture_pending(permit));
            assert_eq!(engine.fd_publications[&files].active, Some(permit));
            assert_eq!(
                engine.lifetime.counts(source.binding.open_file).transports,
                1
            );
            assert!(engine.stream_calls[&call.id].abandoned);
            assert!(engine.lifetime.finish().is_err());
            if shared {
                assert!(
                    matches!(engine.begin_fd_mutation(sibling, files, NetworkFdMutationKind::Socket),
                    Err(NetworkReplayError::StreamOperationBusy(lease)) if lease == permit.lease)
                );
            }
        }
    }

    #[test]
    fn native_capture_never_infers_backend_capability_from_registered_slot() {
        let (mut engine, owner, files) = setup();
        let source = socket(&mut engine, owner, files, 7, 1);
        let control = engine
            .begin_socket_controls(owner, vec![source.binding.open_file])
            .unwrap()[0]
            .1;
        engine.fd_lifecycle.capability = None;
        let next_call = engine.next_stream_call;
        assert!(
            engine
                .begin_native_stream_call(owner, control, source.binding)
                .is_err()
        );
        assert_eq!(engine.next_stream_call, next_call);
        assert!(engine.stream_calls.is_empty());
        assert!(engine.fd_publications[&files].active.is_none());
        assert_eq!(
            engine.lifetime.counts(source.binding.open_file).transports,
            0
        );
    }

    #[test]
    fn regular_stdout_alias_and_mixed_replacement_use_shared_mutation_owner() {
        let (mut engine, owner, files) = setup();
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        let regular = slot(owner, 1, 1, OpenFileId::new(owner.thread, 0));
        engine
            .lifetime
            .publish_created_slot(task, regular, None)
            .unwrap();
        let socket = socket(&mut engine, owner, files, 4, 2);
        assert!(matches!(
            engine.begin_socket_controls(owner, vec![regular.binding.open_file]),
            Err(NetworkReplayError::NonSocketOpenFile(_))
        ));
        let duplicate = admitted(
            &mut engine,
            owner,
            files,
            alias(regular, NetworkFdInstallKind::Dup, None, None),
        );
        engine
            .submit_fd_mutation(owner, duplicate.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, duplicate.publication.permit, Ok(7))
            .unwrap();
        let alias_slot = slot(owner, 7, 3, regular.binding.open_file);
        commit_install(&mut engine, owner, &duplicate, None, alias_slot);
        assert_eq!(
            engine.lifetime.descriptor_binding(task, 7).unwrap(),
            alias_slot.binding
        );
        let replace = admitted(
            &mut engine,
            owner,
            files,
            alias(regular, NetworkFdInstallKind::Dup2, Some(4), Some(socket)),
        );
        assert_eq!(replace.controls.len(), 2);
        engine
            .submit_fd_mutation(owner, replace.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, replace.publication.permit, Ok(4))
            .unwrap();
        let replacement = slot(owner, 4, 4, regular.binding.open_file);
        commit_install(&mut engine, owner, &replace, Some(socket), replacement);
        assert_eq!(
            engine.lifetime.descriptor_binding(task, 4).unwrap(),
            replacement.binding
        );
        assert!(engine.lifetime.is_retired(socket.binding.open_file));
        assert_eq!(engine.lifetime.counts(regular.binding.open_file).slots, 3);
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
    #[test]
    fn inherited_clone_consumes_retained_shared_or_copied_table_after_parent_exit() {
        for shared in [false, true] {
            for parent_result in [false, true] {
                let (mut engine, parent, files) = setup();
                let source = socket(&mut engine, parent, files, 7, 1);
                let flags = if shared {
                    CloneFlags::CLONE_FILES
                } else {
                    CloneFlags::empty()
                };
                let child = NetworkStreamOwner {
                    thread: DetTid::from_raw(62),
                    mm: MmId::for_clone(parent.mm, DetTid::from_raw(62), false),
                };
                let admission = admitted(
                    &mut engine,
                    parent,
                    files,
                    NetworkFdMutationKind::Clone { flags },
                );
                let permit = admission.publication.permit;
                engine.submit_fd_mutation(parent, permit).unwrap();
                if parent_result {
                    engine
                        .confirm_fd_mutation_result(parent, permit, Ok(62))
                        .unwrap();
                }
                engine.retire_fd_table_owner(parent);
                engine.stream_owner_gone(parent);
                assert_eq!(engine.fd_publications[&files].active, Some(permit));
                assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
                assert!(engine.release_empty_fd_publication(parent, permit).is_err());
                engine
                    .register_inherited_cloned_fd_table(permit, child, child.thread, flags)
                    .unwrap();
                let expected_files = if shared {
                    files
                } else {
                    FilesId::forked(child.thread)
                };
                assert_eq!(engine.fd_table_fixture_files(child), Some(expected_files));
                let binding = engine
                    .lifetime
                    .descriptor_binding(
                        TaskOwner {
                            tid: child.thread,
                            mm: child.mm,
                        },
                        7,
                    )
                    .unwrap();
                assert_eq!(binding.open_file, source.binding.open_file);
                assert_eq!(binding.generation, source.binding.generation);
                assert!(!engine.fd_lifecycle.mutations.contains_key(&permit.lease));
                let after = format!("{engine:?}");
                assert!(
                    engine
                        .register_inherited_cloned_fd_table(permit, child, child.thread, flags)
                        .is_err()
                );
                assert_eq!(format!("{engine:?}"), after);
                engine.retire_fd_table_owner(child);
                assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn inherited_clone_rejects_wrong_owner_lease_table_flags_and_child_mm_without_effect() {
        let (mut engine, parent, files) = setup();
        let source = socket(&mut engine, parent, files, 7, 1);
        let flags = CloneFlags::empty();
        let child = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: MmId::for_clone(parent.mm, DetTid::from_raw(62), false),
        };
        let admission = admitted(
            &mut engine,
            parent,
            files,
            NetworkFdMutationKind::Clone { flags },
        );
        let permit = admission.publication.permit;
        engine.submit_fd_mutation(parent, permit).unwrap();
        engine.retire_fd_table_owner(parent);
        engine.stream_owner_gone(parent);
        let before = format!("{engine:?}");
        let mut wrong = permit;
        wrong.owner.mm = parent.mm.for_exec(parent.thread);
        let mut wrong_lease = permit;
        wrong_lease.lease = NetworkStreamLeaseId(permit.lease.0 + 1);
        let mut wrong_table = permit;
        wrong_table.files = FilesId::forked(child.thread);
        for candidate in [wrong, wrong_lease, wrong_table] {
            assert!(
                engine
                    .register_inherited_cloned_fd_table(candidate, child, child.thread, flags)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
        }
        assert!(
            engine
                .register_inherited_cloned_fd_table(
                    permit,
                    child,
                    child.thread,
                    CloneFlags::CLONE_FILES
                )
                .is_err()
        );
        assert!(
            engine
                .register_inherited_cloned_fd_table(
                    permit,
                    NetworkStreamOwner {
                        mm: parent.mm,
                        ..child
                    },
                    child.thread,
                    flags
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
        engine
            .register_inherited_cloned_fd_table(permit, child, child.thread, flags)
            .unwrap();
    }

    #[test]
    fn inherited_clone_cannot_invent_submission_or_override_native_failure() {
        for result in [None, Some(Err(libc::EAGAIN)), Some(Ok(63))] {
            let (mut engine, parent, files) = setup();
            let flags = CloneFlags::empty();
            let child = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                mm: MmId::for_clone(parent.mm, DetTid::from_raw(62), false),
            };
            let admission = admitted(
                &mut engine,
                parent,
                files,
                NetworkFdMutationKind::Clone { flags },
            );
            let permit = admission.publication.permit;
            if let Some(result) = result {
                engine.submit_fd_mutation(parent, permit).unwrap();
                engine
                    .confirm_fd_mutation_result(parent, permit, result)
                    .unwrap();
            }
            let before = format!("{engine:?}");
            assert!(
                engine
                    .register_inherited_cloned_fd_table(permit, child, child.thread, flags)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
        }
    }
    #[test]
    fn inherited_clone_uninvoked_consumption_releases_retired_parent_table_custody() {
        for shared in [false, true] {
            let (mut engine, parent, files) = setup();
            let source = socket(&mut engine, parent, files, 7, 1);
            let flags = if shared {
                CloneFlags::CLONE_FILES
            } else {
                CloneFlags::empty()
            };
            let admission = admitted(
                &mut engine,
                parent,
                files,
                NetworkFdMutationKind::Clone { flags },
            );
            let permit = admission.publication.permit;
            engine.submit_fd_mutation(parent, permit).unwrap();
            engine.retire_fd_table_owner(parent);
            engine.stream_owner_gone(parent);
            assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
            let before = format!("{engine:?}");
            let mut wrong = permit;
            wrong.owner.mm = parent.mm.for_exec(parent.thread);
            assert!(
                engine
                    .cancel_uninvoked_cloned_fd_table(wrong, flags)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
            engine
                .cancel_uninvoked_cloned_fd_table(permit, flags)
                .unwrap();
            assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
            engine.finish_fd_mutations().unwrap();
            let after = format!("{engine:?}");
            assert!(
                engine
                    .cancel_uninvoked_cloned_fd_table(permit, flags)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), after);
        }
    }

    #[test]
    fn prestart_terminal_clone_retires_shared_or_copied_escrow_without_child_admission() {
        for flags in [CloneFlags::empty(), CloneFlags::CLONE_FILES] {
            for parent_gone in [false, true] {
                let (mut engine, parent, files) = setup();
                let source = socket(&mut engine, parent, files, 7, 1);
                let child = NetworkStreamOwner {
                    thread: DetTid::from_raw(62),
                    mm: MmId::for_clone(parent.mm, DetTid::from_raw(62), false),
                };
                let a = admitted(
                    &mut engine,
                    parent,
                    files,
                    NetworkFdMutationKind::Clone { flags },
                );
                let permit = a.publication.permit;
                engine.submit_fd_mutation(parent, permit).unwrap();
                if parent_gone {
                    engine.retire_fd_table_owner(parent);
                    engine.stream_owner_gone(parent);
                }
                assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
                assert_eq!(
                    engine
                        .lifetime
                        .counts(source.binding.open_file)
                        .clone_reservations,
                    usize::from(!flags.contains(CloneFlags::CLONE_FILES))
                );
                assert_eq!(engine.fd_publications[&files].active, Some(permit));
                engine
                    .retire_prestart_cloned_fd_table(permit, child, child.thread, flags)
                    .unwrap();
                assert_eq!(engine.fd_table_fixture_files(child), None);
                assert_eq!(
                    engine
                        .lifetime
                        .counts(source.binding.open_file)
                        .clone_reservations,
                    0
                );
                assert_eq!(
                    engine.fd_publication_fixture_is_retired(source.binding.open_file),
                    parent_gone
                );
                let after = format!("{engine:?}");
                assert!(
                    engine
                        .retire_prestart_cloned_fd_table(permit, child, child.thread, flags)
                        .is_err()
                );
                assert_eq!(format!("{engine:?}"), after);
                if !parent_gone {
                    assert!(
                        engine
                            .confirm_fd_mutation_result(parent, permit, Ok(63))
                            .is_err()
                    );
                    assert_eq!(format!("{engine:?}"), after);
                    engine
                        .confirm_fd_mutation_result(parent, permit, Ok(62))
                        .unwrap();
                    engine.retire_fd_table_owner(parent);
                    engine.stream_owner_gone(parent);
                }
                assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn prestart_terminal_clone_rejects_changed_custody_without_releasing_anything() {
        for wrong in ["permit", "MM", "flags", "native-error"] {
            let (mut engine, parent, files) = setup();
            let source = socket(&mut engine, parent, files, 7, 1);
            let a = admitted(
                &mut engine,
                parent,
                files,
                NetworkFdMutationKind::Clone {
                    flags: CloneFlags::empty(),
                },
            );
            let mut permit = a.publication.permit;
            engine.submit_fd_mutation(parent, permit).unwrap();
            let mut child = NetworkStreamOwner {
                thread: DetTid::from_raw(62),
                mm: MmId::for_clone(parent.mm, DetTid::from_raw(62), false),
            };
            let mut flags = CloneFlags::empty();
            if wrong == "permit" {
                permit.owner.mm = parent.mm.for_exec(parent.thread);
            }
            if wrong == "MM" {
                child.mm = child.mm.for_exec(child.thread);
            }
            if wrong == "flags" {
                flags = CloneFlags::CLONE_FILES;
            }
            if wrong == "native-error" {
                engine
                    .confirm_fd_mutation_result(parent, permit, Err(libc::EAGAIN))
                    .unwrap();
            }
            let before = format!("{engine:?}");
            assert!(
                engine
                    .retire_prestart_cloned_fd_table(permit, child, child.thread, flags)
                    .is_err(),
                "{wrong}"
            );
            assert_eq!(format!("{engine:?}"), before, "{wrong}");
            assert_eq!(
                engine
                    .lifetime
                    .counts(source.binding.open_file)
                    .clone_reservations,
                1
            );
            assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
        }
    }

    #[test]
    fn observed_failed_clone_releases_exact_shared_or_copied_reservation_once() {
        for flags in [CloneFlags::empty(), CloneFlags::CLONE_FILES] {
            let (mut engine, owner, files) = setup();
            let source = socket(&mut engine, owner, files, 7, 1);
            let a = admitted(
                &mut engine,
                owner,
                files,
                NetworkFdMutationKind::Clone { flags },
            );
            let permit = a.publication.permit;
            engine.submit_fd_mutation(owner, permit).unwrap();
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert!(!engine.fd_publication_fixture_is_retired(source.binding.open_file));
            assert!(engine.fd_lifecycle.mutations.contains_key(&permit.lease));
            engine
                .settle_failed_cloned_fd_table(permit, flags, libc::EAGAIN)
                .unwrap();
            assert!(!engine.fd_lifecycle.mutations.contains_key(&permit.lease));
            assert!(engine.fd_publication_fixture_is_retired(source.binding.open_file));
            assert!(
                engine
                    .settle_failed_cloned_fd_table(permit, flags, libc::EAGAIN)
                    .is_err()
            );
            engine.finish_fd_mutations().unwrap();
        }
    }

    #[test]
    fn observed_failed_clone_rejects_changed_permit_and_contradictory_outcome() {
        for wrong in [
            "lease",
            "flags",
            "errno",
            "native_success",
            "different_errno",
        ] {
            let (mut engine, owner, files) = setup();
            let flags = CloneFlags::empty();
            let a = admitted(
                &mut engine,
                owner,
                files,
                NetworkFdMutationKind::Clone { flags },
            );
            let permit = a.publication.permit;
            engine.submit_fd_mutation(owner, permit).unwrap();
            let mut actual = permit;
            let mut actual_flags = flags;
            let mut errno = libc::EAGAIN;
            match wrong {
                "lease" => actual.lease.0 += 1,
                "flags" => actual_flags = CloneFlags::CLONE_FILES,
                "errno" => errno = 0,
                "native_success" => engine
                    .confirm_fd_mutation_result(owner, permit, Ok(62))
                    .unwrap(),
                "different_errno" => engine
                    .confirm_fd_mutation_result(owner, permit, Err(libc::EINVAL))
                    .unwrap(),
                _ => unreachable!(),
            }
            assert!(
                engine
                    .settle_failed_cloned_fd_table(actual, actual_flags, errno)
                    .is_err(),
                "{wrong}"
            );
            assert!(engine.fd_lifecycle.mutations.contains_key(&permit.lease));
            assert_eq!(engine.fd_publications[&files].active, Some(permit));
        }
    }
}

#[cfg(test)]
mod native_actual_fd_tests {
    use chrono::TimeZone;

    use super::*;
    use crate::types::DetTid;
    use crate::types::MmId;
    fn prepared(
        actual: CloneFlags,
        terminal: bool,
    ) -> (
        NetworkReplayEngine,
        NetworkFdMutationAdmission,
        crate::network_runtime::native_birth::NativeBirthAdmission,
    ) {
        let thread = DetTid::from_raw(41);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.fd_table_fixture_enable();
        engine.register_initial_fd_table(owner, thread).unwrap();
        let NetworkFdMutationBegin::Admitted(admission) = engine
            .begin_fd_mutation(
                owner,
                FilesId::initial(thread),
                NetworkFdMutationKind::Clone {
                    flags: CloneFlags::empty(),
                },
            )
            .unwrap()
        else {
            panic!("original clone admission");
        };
        let permit = admission.publication.permit;
        engine.submit_fd_mutation(owner, permit).unwrap();
        engine.prepare_native_birth_escrow(owner, permit).unwrap();
        let proof = crate::network_runtime::native_birth::synthetic_admission_for_permit(
            permit, actual, terminal,
        );
        (engine, admission, proof)
    }
    #[test]
    fn actual_files_choice_consumes_same_escrow_without_changing_request() {
        for actual in [
            CloneFlags::empty(),
            CloneFlags::CLONE_FILES | CloneFlags::CLONE_VM,
        ] {
            let (mut engine, original, proof) = prepared(actual, false);
            let permit = original.publication.permit;
            engine.admit_native_birth(&proof).unwrap();
            engine.admit_native_birth(&proof).unwrap();
            assert_eq!(
                engine.fd_lifecycle.mutations[&permit.lease].admission,
                original
            );
            assert!(engine.cancel_uninvoked_clone_admission(&original).is_err());
            engine
                .register_inherited_cloned_fd_table(
                    permit,
                    proof.child_owner(),
                    proof.child_process(),
                    actual,
                )
                .unwrap();
            let expected = if actual.contains(CloneFlags::CLONE_FILES) {
                permit.files
            } else {
                FilesId::forked(proof.child_owner().thread)
            };
            assert_eq!(
                engine.fd_table_fixture_files(proof.child_owner()),
                Some(expected)
            );
            assert!(
                engine
                    .confirm_fd_mutation_result(permit.owner, permit, Ok(999))
                    .is_err()
            );
            engine
                .confirm_fd_mutation_result(
                    permit.owner,
                    permit,
                    Ok(i64::from(proof.child_owner().thread.as_raw())),
                )
                .unwrap();
        }
    }
    #[test]
    fn prestart_actual_files_retirement_does_not_register_live_child() {
        for actual in [CloneFlags::empty(), CloneFlags::CLONE_FILES] {
            let (mut engine, original, proof) = prepared(actual, true);
            let permit = original.publication.permit;
            engine.admit_native_birth(&proof).unwrap();
            assert!(
                engine
                    .settle_failed_cloned_fd_table(permit, CloneFlags::empty(), libc::ECHILD)
                    .is_err()
            );
            engine
                .retire_prestart_cloned_fd_table(
                    permit,
                    proof.child_owner(),
                    proof.child_process(),
                    actual,
                )
                .unwrap();
            assert_eq!(engine.fd_table_fixture_files(proof.child_owner()), None);
            assert_eq!(
                engine.fd_lifecycle.mutations[&permit.lease].admission,
                original
            );
            engine
                .confirm_fd_mutation_result(
                    permit.owner,
                    permit,
                    Ok(i64::from(proof.child_owner().thread.as_raw())),
                )
                .unwrap();
        }
    }
}

impl NetworkReplayEngine {
    pub(super) fn foreground_fd_mutations_settled(&self) -> bool {
        self.fd_lifecycle.mutations.is_empty()
    }
}
