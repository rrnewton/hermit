//! One consumer for original Socket and accepted-child installations.
//! Physical facts stay in the existing Call/Accept owner; this module only
//! joins them to the existing metadata journal and lifetime publisher.

use std::sync::Arc;
use std::sync::Mutex;

use nix::fcntl::OFlag;

use super::*;
use crate::network_runtime::original_installation::Installation;
use crate::network_runtime::original_installation::Source;
use crate::stat::DetStat;
use crate::tool_local::FileMetadata;
use crate::types::FdSlotBinding;

fn protocol(error: impl std::fmt::Display) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(error.to_string())
}

/// Issued only after the private Original allocator receipt and profile match.
/// It is not a serialized kernel-return value or a new installation registry.
#[derive(Debug, Clone)]
pub(super) struct OriginalCreationAuthority(TaskOwner);
impl OriginalCreationAuthority {
    #[cfg(test)]
    pub(super) fn controlled_fixture(owner: NetworkStreamOwner) -> Self {
        Self(TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        })
    }
    pub(super) fn owner(&self) -> TaskOwner {
        self.0
    }
}

/// Actual fresh Socket profile captured under its existing installation permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshStreamEnrollment {
    /// Exact protocol class checked against original creation operands.
    pub(crate) key: StreamSocketKeyV3,
    /// Captured backend network namespace.
    pub(crate) namespace: NetworkStreamNamespace,
    /// Actual Record profile, or recorded-profile selection in Replay.
    pub(crate) observed_profile: Option<FreshStreamSocketProfileV3>,
}

/// Linux socket files are read/write; creation type bits are not access modes.
/// CLOEXEC remains descriptor-local and NONBLOCK remains OFD status.
pub(crate) fn socket_installation_flags(creation_flags: i32) -> OFlag {
    OFlag::O_RDWR
        | OFlag::from_bits_truncate(creation_flags & (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK))
}

/// The allocation-only IPv6 capability check used by curl. This is not UDP
/// transport admission; even other legal creation flags remain outside this
/// initial operation profile.
pub(crate) fn is_udp6_capability_probe(domain: i32, socket_type: i32, protocol: i32) -> bool {
    domain == libc::AF_INET6 && socket_type == libc::SOCK_DGRAM && protocol == libc::IPPROTO_IP
}

/// Kernel-observed file class/status joined to an original Openat installation.
/// The runtime authenticates its held file before the synchronous publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenatEnrollment {
    pub(crate) kind: crate::fd::FdType,
    pub(crate) status_flags: i32,
}

#[derive(Debug, Clone)]
enum EnrollmentKind {
    Socket(Option<FreshStreamEnrollment>),
    Openat(OpenatEnrollment),
    EpollCreate(OpenatEnrollment),
    Accepted {
        lease: NetworkAcceptLeaseId,
        now: LogicalTime,
    },
}

/// Retained inside the existing FdPublicationState, before local publication.
/// The exact batch is the recovery identity; completed enrollment is never
/// replayed over subsequent guest option mutations.
#[derive(Debug, Clone)]
pub(super) struct Enrollment {
    pub(super) permit: NetworkFdPublicationPermit,
    owner: NetworkStreamOwner,
    binding: FdSlotBinding,
    admission: NetworkFdPublicationAdmission,
    receipt: Installation,
    flags: OFlag,
    stat: Option<DetStat>,
    batch: NetworkFdPublicationBatch,
    kind: EnrollmentKind,
    pub(super) completed: bool,
}

impl Enrollment {
    pub(super) fn batch(&self) -> &NetworkFdPublicationBatch {
        &self.batch
    }
}

// Group the metadata identity with its existing held borrow. These aliases do
// not mint publication authority or change the caller's lock lifetime.
type InstallationMetadata<'a> = (&'a Arc<Mutex<FileMetadata>>, &'a mut FileMetadata);
type SocketInstallationProfile = (OFlag, Option<DetStat>, Option<FreshStreamEnrollment>);
type OpenatInstallationProfile = (OpenatEnrollment, Option<DetStat>);
type AllocatorInstallationProfile = (
    OFlag,
    Option<DetStat>,
    Option<FreshStreamEnrollment>,
    Option<OpenatEnrollment>,
);

impl NetworkReplayEngine {
    pub(crate) fn original_installation_metadata(
        &self,
        owner: NetworkStreamOwner,
    ) -> Result<(FilesId, Arc<Mutex<FileMetadata>>), NetworkReplayError> {
        let files = self
            .lifetime
            .task_files(TaskOwner {
                tid: owner.thread,
                mm: owner.mm,
            })
            .map_err(protocol)?;
        Ok((files, self.fd_metadata(owner, files)?))
    }

    /// Caller holds the actual metadata mutex before the engine mutex. No
    /// physical syscall, guest copy, await, or numeric-FD query occurs here.
    pub(crate) fn publish_original_installation(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdPublicationAdmission,
        receipt: &Installation,
        metadata: InstallationMetadata<'_>,
        profile: SocketInstallationProfile,
        now: LogicalTime,
    ) -> Result<FdSlotBinding, NetworkReplayError> {
        let (actual, metadata) = metadata;
        let (flags, stat, fresh) = profile;
        let (binding, batch) = self.prepare_original_installation_publication(
            owner,
            admission,
            receipt,
            (actual, metadata),
            (flags, stat, fresh),
            now,
        )?;
        self.publish_fd_publication(owner, admission.permit, &batch)?;
        metadata.publication_acknowledge(&batch).map_err(protocol)?;
        self.acknowledge_fd_publication(owner, admission.permit, &batch)?;
        metadata
            .publication_server_acknowledge(&batch)
            .map_err(protocol)?;
        Ok(binding)
    }

    // The same production preparation is exposed only within this engine;
    // generic recovery consumes its retained plan and exact metadata batch.
    pub(super) fn prepare_original_installation_publication(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdPublicationAdmission,
        receipt: &Installation,
        metadata: InstallationMetadata<'_>,
        profile: SocketInstallationProfile,
        now: LogicalTime,
    ) -> Result<(FdSlotBinding, NetworkFdPublicationBatch), NetworkReplayError> {
        let (actual, metadata) = metadata;
        let (flags, stat, fresh) = profile;
        self.prepare_allocator_installation_publication(
            owner,
            admission,
            receipt,
            (actual, metadata),
            (flags, stat, fresh, None),
            now,
        )
    }

    pub(crate) fn publish_original_openat_installation(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdPublicationAdmission,
        receipt: &Installation,
        metadata: InstallationMetadata<'_>,
        profile: OpenatInstallationProfile,
        now: LogicalTime,
    ) -> Result<FdSlotBinding, NetworkReplayError> {
        let (actual, metadata) = metadata;
        let (opened, stat) = profile;
        let Source::Openat(call) = receipt.source() else {
            return Err(protocol(
                "Openat publication changed original installation source",
            ));
        };
        let args =
            self.validate_original_allocator_installation(owner, call, admission.permit, receipt)?;
        let flags = OFlag::from_bits_retain(opened.status_flags | (args.length & libc::O_CLOEXEC));
        let (binding, batch) = self.prepare_allocator_installation_publication(
            owner,
            admission,
            receipt,
            (actual, metadata),
            (flags, stat, None, Some(opened)),
            now,
        )?;
        self.publish_fd_publication(owner, admission.permit, &batch)?;
        metadata.publication_acknowledge(&batch).map_err(protocol)?;
        self.acknowledge_fd_publication(owner, admission.permit, &batch)?;
        metadata
            .publication_server_acknowledge(&batch)
            .map_err(protocol)?;
        Ok(binding)
    }

    /// Epoll's class and both flag domains come from the original fd_install
    /// receipt. No current numeric descriptor or input flag is its certificate.
    pub(crate) fn publish_original_epoll_installation(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdPublicationAdmission,
        receipt: &Installation,
        actual: &Arc<Mutex<FileMetadata>>,
        metadata: &mut FileMetadata,
        now: LogicalTime,
    ) -> Result<FdSlotBinding, NetworkReplayError> {
        let Source::EpollCreate(call) = receipt.source() else {
            return Err(protocol(
                "epoll publication changed original installation source",
            ));
        };
        let args =
            self.validate_original_allocator_installation(owner, call, admission.permit, receipt)?;
        if !matches!(args.kind, super::original_connect::Kind::EpollCreate { .. }) {
            return Err(protocol(
                "epoll publication changed selected original creator",
            ));
        }
        let profile = receipt.epoll_profile().map_err(protocol)?;
        let flags = OFlag::from_bits_retain(
            profile.status_flags
                | if profile.descriptor_flags & libc::FD_CLOEXEC != 0 {
                    libc::O_CLOEXEC
                } else {
                    0
                },
        );
        let opened = OpenatEnrollment {
            kind: crate::fd::FdType::Epoll,
            status_flags: profile.status_flags,
        };
        let (binding, batch) = self.prepare_allocator_installation_publication(
            owner,
            admission,
            receipt,
            (actual, metadata),
            (flags, None, None, Some(opened)),
            now,
        )?;
        self.publish_fd_publication(owner, admission.permit, &batch)?;
        metadata.publication_acknowledge(&batch).map_err(protocol)?;
        self.acknowledge_fd_publication(owner, admission.permit, &batch)?;
        metadata
            .publication_server_acknowledge(&batch)
            .map_err(protocol)?;
        Ok(binding)
    }

    fn prepare_allocator_installation_publication(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdPublicationAdmission,
        receipt: &Installation,
        metadata: InstallationMetadata<'_>,
        profile: AllocatorInstallationProfile,
        now: LogicalTime,
    ) -> Result<(FdSlotBinding, NetworkFdPublicationBatch), NetworkReplayError> {
        let (actual, metadata) = metadata;
        let (flags, stat, fresh, opened) = profile;
        let permit = admission.permit;
        self.validate_publication_permit(owner, permit)?;
        let flags = if matches!(receipt.source(), Source::Openat(_) | Source::EpollCreate(_)) {
            flags
        } else {
            socket_installation_flags(flags.bits())
        };
        if matches!(receipt.source(), Source::Openat(_) | Source::EpollCreate(_))
            != opened.is_some()
            || (opened.is_some() && fresh.is_some())
        {
            return Err(protocol("allocation profile changed its original family"));
        }
        self.validate_fd_metadata(owner, permit.files, actual, metadata)?;
        receipt
            .validate_publication(owner, permit, actual, metadata)
            .map_err(protocol)?;
        if admission.recovery.is_some() {
            return Err(protocol(
                "original installation must settle its exact prior publication first",
            ));
        }
        if let Some(plan) = self.fd_publications[&permit.files].enrollment.clone() {
            let profile_matches = match &plan.kind {
                EnrollmentKind::Socket(profile) => *profile == fresh && opened.is_none(),
                EnrollmentKind::Openat(profile) | EnrollmentKind::EpollCreate(profile) => {
                    Some(*profile) == opened && fresh.is_none()
                }
                EnrollmentKind::Accepted { .. } => fresh.is_none(),
            };
            if plan.owner != owner
                || plan.permit != permit
                || plan.admission != *admission
                || !receipt.resumes(&plan.receipt)
                || !profile_matches
                || plan.flags != flags
                || plan.stat != stat
                || metadata.descriptor_binding(receipt.fd()).ok()
                    != (!plan.receipt.removed_before_publication()).then_some(plan.binding)
            {
                return Err(protocol(
                    "original installation retry changed retained publication inputs",
                ));
            }
            let recovery = NetworkFdPublicationAdmission {
                recovery: Some(plan.batch.clone()),
                ..admission.clone()
            };
            if metadata.publication_snapshot(&recovery).map_err(protocol)? != plan.batch {
                return Err(protocol(
                    "original installation retry changed its exact local batch",
                ));
            }
            return Ok((plan.binding, plan.batch));
        }
        let (mut candidate, change) = metadata
            .prepare_original_installation_typed(
                receipt.original_owner().thread,
                receipt.fd(),
                flags,
                opened.map_or(crate::fd::FdType::Socket, |p| p.kind),
                stat,
            )
            .map_err(protocol)?;
        let binding = change
            .after
            .expect("fresh candidate contains its installed slot")
            .binding;
        candidate
            .bind_native_installation(binding, receipt.file_identity())
            .map_err(protocol)?;
        if receipt.removed_before_publication() {
            candidate
                .remove_native_installation(binding, receipt.file_identity())
                .map_err(protocol)?;
        }
        if self.fd_publications[&permit.files].enrollment.is_some() {
            return Err(protocol(
                "original installation already retains an enrollment plan",
            ));
        }
        // Derive whether a fresh profile is required from the exact admitted
        // original arguments. An omitted profile cannot select a legacy path.
        let kind = match receipt.source() {
            Source::Socket(call) => {
                let args =
                    self.validate_original_socket_installation(owner, call, permit, receipt)?;
                let tcp = matches!(args.fd, libc::AF_INET | libc::AF_INET6)
                    && (args.address as u32 as i32) & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                        == libc::SOCK_STREAM
                    && matches!(args.length, 0 | libc::IPPROTO_TCP);
                let expected = StreamSocketKeyV3 {
                    transport: NetworkTransportV2::Tcp,
                    domain: args.fd,
                    socket_type: libc::SOCK_STREAM,
                    protocol: libc::IPPROTO_TCP,
                };
                // A fully reconciled removal leaves no live alias for socket
                // admission. Missing metadata is allowed only at that proved
                // historical retirement, never for a published live slot.
                if (self.shadow_mode() && tcp && !receipt.removed_before_publication())
                    && fresh.is_none()
                    || (!self.shadow_mode() || !tcp) && fresh.is_some()
                    || fresh
                        .as_ref()
                        .is_some_and(|profile| profile.key != expected)
                {
                    return Err(protocol(
                        "original Socket enrollment differs from its actual creation tuple",
                    ));
                }
                if !receipt.removed_before_publication()
                    && is_udp6_capability_probe(args.fd, args.address as u32 as i32, args.length)
                {
                    candidate.restrict_network_capability_probe(binding).map_err(protocol)?;
                }
                EnrollmentKind::Socket(fresh)
            }
            Source::Openat(call) => {
                let args =
                    self.validate_original_allocator_installation(owner, call, permit, receipt)?;
                let opened =
                    opened.ok_or_else(|| protocol("Openat lost its exact held file profile"))?;
                if args.kind != super::original_connect::Kind::Openat
                    || opened.status_flags & libc::O_CLOEXEC != 0
                    || flags.bits() != (opened.status_flags | (args.length & libc::O_CLOEXEC))
                {
                    return Err(protocol(
                        "Openat profile changed status or descriptor flags",
                    ));
                }
                EnrollmentKind::Openat(opened)
            }
            Source::EpollCreate(call) => {
                let args =
                    self.validate_original_allocator_installation(owner, call, permit, receipt)?;
                let profile = receipt.epoll_profile().map_err(protocol)?;
                let expected = OpenatEnrollment {
                    kind: crate::fd::FdType::Epoll,
                    status_flags: profile.status_flags,
                };
                let descriptor = if profile.descriptor_flags & libc::FD_CLOEXEC != 0 {
                    libc::O_CLOEXEC
                } else {
                    0
                };
                if !matches!(args.kind, super::original_connect::Kind::EpollCreate { .. })
                    || opened != Some(expected)
                    || flags.bits() != (profile.status_flags | descriptor)
                    || stat.is_some()
                {
                    return Err(protocol(
                        "epoll publication changed its original file/descriptor profile",
                    ));
                }
                EnrollmentKind::EpollCreate(expected)
            }
            Source::Accepted { lease, .. } => {
                if fresh.is_some() {
                    return Err(protocol(
                        "accepted child cannot replace inherited state with a fresh profile",
                    ));
                }
                EnrollmentKind::Accepted { lease, now }
            }
        };
        // Construct the private candidate and every fallible local snapshot
        // before retaining engine confirmation. This association is not exposed
        // until the original effect below authenticates the exact replacement.
        let effect = NetworkFdEffectAssociation {
            owner,
            lease: permit.lease,
            kind: match receipt.source() {
                Source::Socket(_) => NetworkFdInstallKind::Socket,
                Source::Openat(_) => NetworkFdInstallKind::Openat,
                Source::EpollCreate(_) => NetworkFdInstallKind::EpollCreate,
                Source::Accepted { .. } => NetworkFdInstallKind::Accept,
            },
            result_index: 0,
            returned_fd: receipt.fd(),
        };
        candidate
            .associate_network_installation(change.installation_generation, effect)
            .map_err(protocol)?;
        let batch = candidate
            .publication_snapshot(admission)
            .map_err(protocol)?;
        // The private receipt describes this actual installation even if its
        // slot was removed before semantic publication. Retain that fact in
        // each already-owned unresolved pair before metadata can discard it.
        self.note_epoll_native_binding(binding, receipt.file_identity())?;
        match receipt.source() {
            Source::Socket(call) | Source::Openat(call) | Source::EpollCreate(call) => {
                self.validate_original_allocator_installation(owner, call, permit, receipt)?;
                let confirmed = self.confirm_fd_installation(owner, permit, change)?;
                assert_eq!(
                    confirmed, effect,
                    "same checked original Socket association"
                );
                if matches!(receipt.source(), Source::Socket(_) | Source::EpollCreate(_)) {
                    self.fd_installations
                        .get_mut(&permit.lease)
                        .unwrap()
                        .original_creation = Some(OriginalCreationAuthority(TaskOwner {
                        tid: receipt.original_owner().thread,
                        mm: receipt.original_owner().mm,
                    }));
                }
            }
            Source::Accepted { lease, child } => {
                if self.fd_installations.contains_key(&permit.lease) {
                    return Err(protocol(
                        "accepted installation reused a publication association",
                    ));
                }
                self.confirm_original_accepted_installation(owner, lease, child, binding)?;
                self.fd_installations.insert(
                    permit.lease,
                    ConfirmedFdInstallation {
                        owner,
                        files: permit.files,
                        kind: effect.kind,
                        returned_fds: vec![receipt.fd()],
                        installations: vec![change],
                        open_files: vec![Some(binding.open_file)],
                        sources: vec![SlotInstallationSource::Fresh],
                        original_creation: None,
                    },
                );
            }
        }
        // Once confirmation has been retained, preserve its exact local batch
        // before any fallible global step. Existing recovery owns that batch;
        // a failure cannot authorize repeating Socket/Accept or replacing it.
        self.fd_publications
            .get_mut(&permit.files)
            .unwrap()
            .enrollment = Some(Enrollment {
            permit,
            owner,
            binding,
            admission: admission.clone(),
            receipt: receipt.clone(),
            flags,
            stat,
            batch: batch.clone(),
            kind,
            completed: false,
        });
        *metadata = candidate;
        Ok((binding, batch))
    }
    pub(super) fn terminal_allocator_recovery_batch(
        &self,
        prior: &NetworkFdMutationAdmission,
        receipt: &Installation,
        actual: &Arc<Mutex<FileMetadata>>,
        metadata: &FileMetadata,
    ) -> Result<Option<NetworkFdPublicationBatch>, NetworkReplayError> {
        let permit = prior.publication.permit;
        let Some(plan) = self
            .fd_publications
            .get(&permit.files)
            .and_then(|state| state.enrollment.as_ref())
        else {
            return Ok(None);
        };
        if plan.permit != permit
            || plan.admission != prior.publication
            || plan.owner != permit.owner
            || !receipt.extends_original(&plan.receipt)
            || !Arc::ptr_eq(actual, &plan.receipt.metadata())
            || metadata.descriptor_binding(receipt.fd()).ok()
                != (!plan.receipt.removed_before_publication()).then_some(plan.binding)
            || !matches!(
                plan.receipt.source(),
                Source::Socket(_) | Source::Openat(_) | Source::EpollCreate(_)
            )
        {
            return Err(protocol(
                "terminal recovery changed the original immutable enrollment",
            ));
        }
        let recovery = NetworkFdPublicationAdmission {
            recovery: Some(plan.batch.clone()),
            ..prior.publication.clone()
        };
        let mut candidate = metadata.clone();
        if candidate
            .publication_snapshot(&recovery)
            .map_err(protocol)?
            != plan.batch
        {
            return Err(protocol(
                "terminal recovery changed the exact local publication prefix",
            ));
        }
        Ok(Some(plan.batch.clone()))
    }

    pub(super) fn terminal_allocator_dead_enrollment(
        &self,
        prior: &NetworkFdMutationAdmission,
        receipt: &Installation,
        actual: &Arc<Mutex<FileMetadata>>,
        metadata: &FileMetadata,
    ) -> Result<Option<Enrollment>, NetworkReplayError> {
        if self.lifetime.table_exists(receipt.files()) || !receipt.removed_before_publication() {
            return Err(protocol(
                "terminal enrollment still has a live table or installation",
            ));
        }
        let Some(_) = self.terminal_allocator_recovery_batch(prior, receipt, actual, metadata)?
        else {
            return Ok(None);
        };
        let plan = self.fd_publications[&receipt.files()]
            .enrollment
            .as_ref()
            .unwrap();
        if self.lifetime.counts(plan.binding.open_file) != lifetime::OwnerCounts::default()
            || self.has_stream_references(plan.binding.open_file)
        {
            return Err(protocol(
                "terminal enrollment still has live shared references",
            ));
        }
        Ok(Some(plan.clone()))
    }

    pub(super) fn validate_terminal_allocator_recovery(
        &self,
        mutation: &NetworkFdMutationAdmission,
    ) -> Result<(), NetworkReplayError> {
        let permit = mutation.publication.permit;
        self.validate_publication_permit(permit.owner, permit)?;
        let plan = self
            .fd_publications
            .get(&permit.files)
            .and_then(|state| state.enrollment.as_ref())
            .ok_or_else(|| protocol("terminal recovery lost immutable enrollment"))?;
        if mutation.publication.recovery.as_ref() != Some(&plan.batch)
            || plan.permit.lease != permit.lease
            || plan.permit.files != permit.files
            || !matches!(
                plan.receipt.source(),
                Source::Socket(_) | Source::Openat(_) | Source::EpollCreate(_)
            )
        {
            return Err(protocol(
                "terminal recovery changed its batch or allocator lease",
            ));
        }
        Ok(())
    }

    /// Replay the exact retained prefix through the existing publisher. A later
    /// proved removal is a separate ordered effect and never rewrites that batch.
    pub(crate) fn recover_terminal_allocator_publication(
        &mut self,
        original: NetworkStreamOwner,
        admission: &super::original_connect::Admission,
        publisher: NetworkStreamOwner,
        mutation: &NetworkFdMutationAdmission,
        receipt: &Installation,
        metadata: InstallationMetadata<'_>,
    ) -> Result<FdSlotBinding, NetworkReplayError> {
        let (actual, metadata) = metadata;
        self.validate_terminal_allocator_recovery(mutation)?;
        let permit = mutation.publication.permit;
        self.validate_fd_metadata(publisher, permit.files, actual, metadata)?;
        receipt
            .validate_terminal(original, actual, metadata)
            .map_err(protocol)?;
        let plan = self.fd_publications[&permit.files]
            .enrollment
            .clone()
            .unwrap();
        if !receipt.extends_original(&plan.receipt)
            || mutation.publication.recovery.as_ref() != Some(&plan.batch)
            || metadata
                .publication_snapshot(&mutation.publication)
                .map_err(protocol)?
                != plan.batch
        {
            return Err(protocol(
                "terminal recovery changed original effect or local batch",
            ));
        }
        let later_removal =
            receipt.removed_before_publication() && !plan.receipt.removed_before_publication();
        let mut after_removal = metadata.clone();
        if later_removal {
            after_removal
                .remove_native_installation(plan.binding, receipt.file_identity())
                .map_err(protocol)?;
        }
        // The original plan's source/creator/profile remain unchanged even
        // though a current CLONE_FILES owner carries recovery authority.
        self.publish_fd_publication(publisher, permit, &plan.batch)?;
        if later_removal {
            let mut lifetime = self.lifetime.clone();
            let retired = lifetime
                .close_retained_binding(plan.binding)
                .map_err(protocol)?;
            if retired.iter().any(|file| self.has_stream_references(*file)) {
                return Err(protocol(
                    "terminal ordered removal still has semantic operation references",
                ));
            }
            *metadata = after_removal;
            self.lifetime = lifetime;
            self.retire_lifetime_open_files(retired);
        }
        self.retain_terminal_allocator_enrollment(original, admission, plan.clone())?;
        metadata
            .publication_acknowledge(&plan.batch)
            .map_err(protocol)?;
        self.acknowledge_fd_publication(publisher, permit, &plan.batch)?;
        metadata
            .publication_server_acknowledge(&plan.batch)
            .map_err(protocol)?;
        Ok(plan.binding)
    }

    pub(super) fn validate_completed_socket_publication(
        &self,
        mutation: &NetworkFdMutationAdmission,
        returned: i64,
    ) -> Result<(), NetworkReplayError> {
        let permit = mutation.publication.permit;
        let state = &self.fd_publications[&permit.files];
        let plan = state
            .enrollment
            .as_ref()
            .ok_or_else(|| protocol("Socket lost mutation without retained enrollment"))?;
        if !plan.completed
            || plan.permit != permit
            || plan.admission != mutation.publication
            || plan.owner != permit.owner
            || !matches!(
                (mutation.kind.clone(), plan.receipt.source()),
                (NetworkFdMutationKind::Socket, Source::Socket(_))
                    | (NetworkFdMutationKind::Openat, Source::Openat(_))
                    | (NetworkFdMutationKind::EpollCreate, Source::EpollCreate(_))
            )
            || i64::from(plan.binding.slot.fd) != returned
            || state.pending.as_ref() != Some(&plan.batch)
            || self
                .fd_publication_history
                .get(&(permit.files, plan.batch.sequence))
                != Some(&plan.batch)
        {
            return Err(protocol(
                "Socket mutation retirement lacks its exact pending publication",
            ));
        }
        Ok(())
    }

    /// Called by initial publication, pending replay, and ACK before any gate
    /// release. It never performs a syscall, guest copy or await.
    pub(super) fn settle_fd_enrollment(
        &mut self,
        permit: NetworkFdPublicationPermit,
        batch: &NetworkFdPublicationBatch,
    ) -> Result<(), NetworkReplayError> {
        let Some(plan) = self
            .fd_publications
            .get(&permit.files)
            .and_then(|state| state.enrollment.clone())
        else {
            return Ok(());
        };
        if plan.batch != *batch
            || plan.permit.files != permit.files
            || plan.owner != plan.permit.owner
            || !batch.entries.iter().any(|entry| {
                entry.effect.owner == plan.owner
                    && entry.effect.lease == plan.permit.lease
                    && entry
                        .replacement
                        .after
                        .is_some_and(|slot| slot.binding == plan.binding)
            })
        {
            return Err(protocol("publication changed retained enrollment identity"));
        }
        if plan.completed {
            return Ok(());
        }
        match plan.kind {
            EnrollmentKind::Socket(Some(fresh)) => {
                self.register_original_receive_socket(plan.binding, &plan.receipt, fresh)?;
            }
            EnrollmentKind::Socket(None)
            | EnrollmentKind::Openat(_)
            | EnrollmentKind::EpollCreate(_) => {}
            EnrollmentKind::Accepted { lease, now } => {
                let done = self
                    .complete_accepted_socket(
                        plan.owner,
                        lease,
                        Ok(plan.binding.slot.fd),
                        Some(plan.binding.open_file),
                        now,
                    )?
                    .ok_or_else(|| protocol("installed accepted child produced no completion"))?;
                if done.fd != plan.binding.slot.fd || done.open_file != plan.binding.open_file {
                    return Err(protocol(
                        "accepted enrollment changed its installed binding",
                    ));
                }
            }
        }
        // The physical removal is committed under this same engine transaction
        // after enrollment and before permit release. The creator retains its
        // historical installed binding even though no live alias remains.
        if plan.receipt.removed_before_publication() {
            let mut next = self.lifetime.clone();
            let retired = next
                .close_retained_binding(plan.binding)
                .map_err(protocol)?;
            if retired.iter().any(|ofd| self.has_stream_references(*ofd)) {
                return Err(protocol(
                    "journal removal still has unowned semantic references",
                ));
            }
            self.lifetime = next;
            self.retire_lifetime_open_files(retired);
        }
        self.fd_publications
            .get_mut(&permit.files)
            .unwrap()
            .enrollment
            .as_mut()
            .unwrap()
            .completed = true;
        Ok(())
    }
}

/// Uses the actual installation consumer and both ACKs with explicit controlled
/// provider rows; it does not represent native BPF execution.
#[cfg(test)]
pub(super) fn controlled_receive_origin() -> (
    NetworkReplayEngine,
    NetworkStreamOwner,
    Arc<Mutex<FileMetadata>>,
    FdSlotBinding,
) {
    tests::receive_origin_fixture()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::super::original_connect::Admission;
    use super::super::original_connect::Arguments;
    use super::super::original_connect::Kind;
    use super::*;
    use crate::network_runtime::original_installation::installation_fixture;
    use crate::resources::ExternalOpId;
    use crate::types::DetTid;
    use crate::types::MmId;

    fn setup() -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        Arc<Mutex<FileMetadata>>,
        NetworkFdMutationAdmission,
        Admission,
    ) {
        setup_socket(libc::AF_INET, libc::SOCK_STREAM, 0)
    }
    fn setup_socket(domain: i32, socket_type: i32, protocol: i32) -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        Arc<Mutex<FileMetadata>>,
        NetworkFdMutationAdmission,
        Admission,
    ) {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.fd_table_fixture_enable();
        let files = engine.fd_publication_fixture_register(owner, None);
        let actual = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        let NetworkFdMutationBegin::Admitted(mutation) = engine
            .begin_fd_mutation(owner, files, NetworkFdMutationKind::Socket)
            .unwrap()
        else {
            panic!("expected existing mutation admission")
        };
        let mutation = *mutation;
        engine
            .submit_fd_mutation(owner, mutation.publication.permit)
            .unwrap();
        let args = Arguments {
            kind: Kind::Socket,
            operation: ExternalOpId::new(thread, 10),
            files,
            binding: None,
            fd: domain,
            address: u64::from(socket_type as u32),
            length: protocol,
            original_count: 0,
        };
        assert!(engine.begin_original_connect(owner, args.clone()).is_err());
        let admission = engine
            .begin_original_socket(owner, args, mutation.clone())
            .unwrap();
        (engine, owner, actual, mutation, admission)
    }
    fn complete(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &Admission,
        raw: i64,
    ) {
        engine
            .original_connect_provider_submitted(owner, admission)
            .unwrap();
        engine
            .original_call_prepared(owner, admission, None, 71)
            .unwrap();
        engine.original_connect_invoked(owner, admission).unwrap();
        engine
            .original_connect_selected(
                owner,
                admission,
                71,
                (7, 31, 101, 13, if raw >= 0 { 19 } else { 0 }),
            )
            .unwrap();
        assert!(
            engine
                .original_socket_publication(owner, admission)
                .is_err()
        );
        engine
            .original_connect_returned(owner, admission, raw)
            .unwrap();
        engine
            .original_connect_provider_retired(owner, admission, raw)
            .unwrap();
        engine
            .original_connect_pin_released(owner, admission)
            .unwrap();
    }
    pub(super) fn receive_origin_fixture() -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        Arc<Mutex<FileMetadata>>,
        FdSlotBinding,
    ) {
        let (mut engine, owner, actual, mutation, admission) = setup();
        engine.shadow =
            NetworkReplayEngine::record_shadow(Utc.timestamp_opt(1_790_000_000, 0).unwrap()).shadow;
        let profile = super::super::tests::test_fresh_profile(libc::AF_INET);
        let fresh = FreshStreamEnrollment {
            key: profile.key,
            namespace: super::super::tests::test_socket_namespace(),
            observed_profile: Some(profile),
        };
        complete(&mut engine, owner, &admission, 17);
        let receipt = installation_fixture(
            owner,
            actual.clone(),
            mutation.publication.permit,
            Source::Socket(admission.call),
            71,
            17,
            false,
        );
        engine
            .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(17))
            .unwrap();
        let binding = engine
            .publish_original_installation(
                owner,
                &mutation.publication,
                &receipt,
                (&actual, &mut actual.lock().unwrap()),
                (OFlag::O_CLOEXEC, None, Some(fresh)),
                LogicalTime::ZERO,
            )
            .unwrap();
        engine
            .original_socket_publication_finished(owner, &admission, mutation.publication.permit)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert!(engine.fd_publication_history.is_empty());
        assert!(engine.fd_installations.is_empty());
        (engine, owner, actual, binding)
    }

    #[test]
    fn socket_actual_consumer_publishes_one_generation_and_same_call_ack() {
        let (mut engine, owner, actual, mutation, admission) = setup();
        complete(&mut engine, owner, &admission, 17);
        assert_eq!(
            engine
                .original_socket_publication(owner, &admission)
                .unwrap(),
            (mutation.clone(), 17)
        );
        let receipt = installation_fixture(
            owner,
            actual.clone(),
            mutation.publication.permit,
            Source::Socket(admission.call),
            71,
            17,
            false,
        );
        engine
            .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(17))
            .unwrap();
        let binding = engine
            .publish_original_installation(
                owner,
                &mutation.publication,
                &receipt,
                (&actual, &mut actual.lock().unwrap()),
                (OFlag::O_CLOEXEC, None, None),
                LogicalTime::ZERO,
            )
            .unwrap();
        assert_eq!(binding.slot.fd, 17);
        assert_eq!(
            actual.lock().unwrap().descriptor_binding(17).unwrap(),
            binding
        );
        engine
            .original_socket_publication_finished(owner, &admission, mutation.publication.permit)
            .unwrap();
        assert!(
            engine
                .publish_original_installation(
                    owner,
                    &mutation.publication,
                    &receipt,
                    (&actual, &mut actual.lock().unwrap()),
                    (OFlag::O_CLOEXEC, None, None),
                    LogicalTime::ZERO,
                )
                .is_err()
        );
        assert_eq!(
            actual.lock().unwrap().descriptor_binding(17).unwrap(),
            binding
        );
        engine.finish_original_connect(owner, &admission).unwrap();
        assert!(engine.finish_fd_mutations().is_err());
        assert!(
            engine
                .acquire_fd_publication(owner, binding.slot.files)
                .is_ok()
        );
        let permit = engine.fd_publications[&binding.slot.files].active.unwrap();
        engine.release_empty_fd_publication(owner, permit).unwrap();
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        engine.finish_fd_mutations().unwrap();
    }
    #[test]
    fn original_creation_issuer_requires_private_receipt_and_survives_real_consumer_ack() {
        for original in [false, true] {
            let (mut engine, owner, actual, mutation, admission) = setup();
            complete(&mut engine, owner, &admission, 17);
            engine
                .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(17))
                .unwrap();
            let binding = if original {
                let receipt = installation_fixture(
                    owner,
                    actual.clone(),
                    mutation.publication.permit,
                    Source::Socket(admission.call),
                    71,
                    17,
                    false,
                );
                engine
                    .publish_original_installation(
                        owner,
                        &mutation.publication,
                        &receipt,
                        (&actual, &mut actual.lock().unwrap()),
                        (OFlag::O_CLOEXEC, None, None),
                        LogicalTime::ZERO,
                    )
                    .unwrap()
            } else {
                let (candidate, change) = actual
                    .lock()
                    .unwrap()
                    .prepare_original_installation_typed(
                        owner.thread,
                        17,
                        OFlag::O_CLOEXEC,
                        crate::fd::FdType::Socket,
                        None,
                    )
                    .unwrap();
                let effect = engine
                    .confirm_fd_installation(owner, mutation.publication.permit, change)
                    .unwrap();
                let batch = NetworkFdPublicationBatch {
                    files: change.files,
                    sequence: 1,
                    previous_generation: 0,
                    through_generation: 1,
                    entries: vec![NetworkFdPublicationEntry {
                        replacement: change,
                        effect,
                    }],
                };
                *actual.lock().unwrap() = candidate;
                engine
                    .publish_fd_publication(owner, mutation.publication.permit, &batch)
                    .unwrap();
                engine
                    .acknowledge_fd_publication(owner, mutation.publication.permit, &batch)
                    .unwrap();
                change.after.unwrap().binding
            };
            let task = TaskOwner {
                tid: owner.thread,
                mm: owner.mm,
            };
            assert_eq!(
                engine.lifetime.has_original_creation(
                    task,
                    binding,
                    lifetime::OriginalCreationKind::Socket
                ),
                original
            );
            assert!(engine.fd_installations.is_empty());
            assert!(engine.fd_publication_history.is_empty());
            assert_eq!(engine.lifetime.pending_publication_payloads_for_test(), 0);
            engine.lifetime.close_binding(task, binding).unwrap();
            assert!(!engine.lifetime.has_original_creation(
                task,
                binding,
                lifetime::OriginalCreationKind::Socket
            ));
        }
    }

    #[test]
    fn socket_consumer_refuses_arc_command_and_endpoint_interference_before_local_change() {
        for bad in 0..3 {
            let (mut engine, owner, actual, mutation, admission) = setup();
            complete(&mut engine, owner, &admission, 17);
            let wrong = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(
                owner.thread,
            )));
            let receipt = installation_fixture(
                owner,
                if bad == 0 { wrong } else { actual.clone() },
                mutation.publication.permit,
                Source::Socket(admission.call),
                if bad == 1 { 72 } else { 71 },
                17,
                bad == 2,
            );
            engine
                .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(17))
                .unwrap();
            assert!(
                engine
                    .publish_original_installation(
                        owner,
                        &mutation.publication,
                        &receipt,
                        (&actual, &mut actual.lock().unwrap()),
                        (OFlag::empty(), None, None),
                        LogicalTime::ZERO,
                    )
                    .is_err()
            );
            assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
            assert!(engine.finish_original_connect(owner, &admission).is_err());
        }
    }
    #[test]
    fn socket_known_uninvoked_cleanup_releases_both_existing_owners_without_errno() {
        let (mut engine, owner, actual, _, admission) = setup();
        engine
            .abort_original_before_provider(owner, &admission)
            .unwrap();
        assert!(engine.finish_fd_mutations().is_err());
        assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
        assert!(
            engine
                .acquire_fd_publication(owner, admission.arguments.files)
                .is_ok()
        );
        let permit = engine.fd_publications[&admission.arguments.files]
            .active
            .unwrap();
        engine.release_empty_fd_publication(owner, permit).unwrap();
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        engine.finish_fd_mutations().unwrap();
    }
    #[test]
    fn socket_actual_negative_result_releases_no_installation_only_after_same_call_completion() {
        let (mut engine, owner, actual, mutation, admission) = setup();
        assert!(
            engine
                .finish_unchanged_fd_mutation(owner, mutation.publication.permit)
                .is_err()
        );
        complete(&mut engine, owner, &admission, -i64::from(libc::EMFILE));
        let (observed, raw) = engine
            .original_socket_publication(owner, &admission)
            .unwrap();
        assert_eq!(observed, mutation);
        engine
            .confirm_fd_mutation_result(owner, mutation.publication.permit, Err((-raw) as i32))
            .unwrap();
        engine
            .finish_unchanged_fd_mutation(owner, mutation.publication.permit)
            .unwrap();
        engine
            .original_socket_publication_finished(owner, &admission, mutation.publication.permit)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert!(engine.finish_fd_mutations().is_err());
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        engine.finish_fd_mutations().unwrap();
        assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
    }

    #[test]
    fn udp6_capability_probe_original_publication_restricts_exact_live_binding() {
        let (mut engine, owner, actual, mutation, admission) =
            setup_socket(libc::AF_INET6, libc::SOCK_DGRAM, 0);
        complete(&mut engine, owner, &admission, 17);
        let receipt = installation_fixture(owner, actual.clone(), mutation.publication.permit,
            Source::Socket(admission.call), 71, 17, false);
        engine.confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(17)).unwrap();
        let binding = engine.publish_original_installation(owner, &mutation.publication, &receipt,
            (&actual, &mut actual.lock().unwrap()), (OFlag::O_RDWR, None, None), LogicalTime::ZERO).unwrap();
        assert!(actual.lock().unwrap().has_network_capability_probe());
        assert!(engine.stream_socket_state(binding.open_file).unwrap().is_none());
        let mut wrong = binding;
        wrong.generation += 1;
        let before = format!("{:?}", actual.lock().unwrap());
        assert!(actual.lock().unwrap().restrict_network_capability_probe(wrong).is_err());
        assert_eq!(format!("{:?}", actual.lock().unwrap()), before);
        engine.original_socket_publication_finished(owner, &admission, mutation.publication.permit).unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        engine.finish_fd_mutations().unwrap();
        assert!(actual.lock().unwrap().has_network_capability_probe());
        assert_eq!(actual.lock().unwrap().descriptor_binding(17).unwrap(), binding);
    }

    #[test]
    fn udp6_capability_probe_original_errors_keep_errno_and_leave_no_installation() {
        for errno in [libc::EAFNOSUPPORT, libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EPERM] {
            let (mut engine, owner, actual, mutation, admission) =
                setup_socket(libc::AF_INET6, libc::SOCK_DGRAM, 0);
            complete(&mut engine, owner, &admission, -i64::from(errno));
            let (observed, raw) = engine.original_socket_publication(owner, &admission).unwrap();
            assert_eq!(observed, mutation);
            assert_eq!(raw, -i64::from(errno));
            engine.confirm_fd_mutation_result(owner, mutation.publication.permit, Err(errno)).unwrap();
            engine.finish_unchanged_fd_mutation(owner, mutation.publication.permit).unwrap();
            engine.original_socket_publication_finished(owner, &admission, mutation.publication.permit).unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            engine.finish_fd_mutations().unwrap();
            assert!(!actual.lock().unwrap().has_network_capability_probe());
            assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
        }
    }
    #[test]
    fn socket_publication_result_retry_uses_same_original_call_without_generic_duplicate_allowance()
    {
        for returned in [17, -i64::from(libc::EMFILE)] {
            let (mut engine, owner, actual, mutation, admission) = setup();
            assert!(
                engine
                    .confirm_original_socket_publication_result(owner, &admission)
                    .is_err()
            );
            complete(&mut engine, owner, &admission, returned);
            engine
                .confirm_original_socket_publication_result(owner, &admission)
                .unwrap();
            engine
                .confirm_original_socket_publication_result(owner, &admission)
                .unwrap();
            assert_eq!(
                engine
                    .original_socket_publication(owner, &admission)
                    .unwrap(),
                (mutation.clone(), returned)
            );
            let result = if returned < 0 {
                Err((-returned) as i32)
            } else {
                Ok(returned)
            };
            assert!(
                engine
                    .confirm_fd_mutation_result(owner, mutation.publication.permit, result)
                    .is_err()
            );
            let mut changed = admission.clone();
            changed.arguments.length = libc::IPPROTO_UDP;
            assert!(
                engine
                    .confirm_original_socket_publication_result(owner, &changed)
                    .is_err()
            );
            assert!(
                engine
                    .confirm_original_socket_publication_result(
                        NetworkStreamOwner {
                            thread: crate::types::DetTid::from_raw(32),
                            ..owner
                        },
                        &admission
                    )
                    .is_err()
            );
            assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
            assert!(engine.finish_original_connect(owner, &admission).is_err());
        }
    }

    #[test]
    fn original_install_then_remove_is_published_before_shared_peer_admission() {
        for retired in [false, true] {
            let (mut engine, owner, actual, mutation, admission) = setup();
            let peer = NetworkStreamOwner {
                thread: crate::types::DetTid::from_raw(32),
                ..owner
            };
            let files = engine.fd_publication_fixture_register(peer, Some(owner));
            engine
                .associate_fd_metadata(peer, &actual, &actual.lock().unwrap())
                .unwrap();
            complete(&mut engine, owner, &admission, 17);
            let receipt =
                crate::network_runtime::original_installation::removed_installation_fixture(
                    owner,
                    actual.clone(),
                    mutation.publication.permit,
                    Source::Socket(admission.call),
                    71,
                    17,
                    retired,
                );
            engine
                .confirm_original_socket_publication_result(owner, &admission)
                .unwrap();
            let binding = engine
                .publish_original_installation(
                    owner,
                    &mutation.publication,
                    &receipt,
                    (&actual, &mut actual.lock().unwrap()),
                    (OFlag::O_CLOEXEC, None, None),
                    LogicalTime::ZERO,
                )
                .unwrap();
            assert_eq!(
                binding.slot.fd, 17,
                "creator retains the actual successful historical installation"
            );
            assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
            assert!(
                engine
                    .lifetime
                    .binding_in_retained_table(files, 17)
                    .is_none()
            );
            // The creator has not received its reply or retired its Call yet.
            let NetworkFdReadBegin::Admitted(read) = engine.begin_fd_read(peer, files, 17).unwrap()
            else {
                panic!("same-table peer must progress after committed removal")
            };
            let read = *read;
            assert_eq!(read.binding, None);
            engine.finish_fd_read(peer, read).unwrap();
            engine
                .original_socket_publication_finished(
                    owner,
                    &admission,
                    mutation.publication.permit,
                )
                .unwrap();
            engine.finish_original_connect(owner, &admission).unwrap();
            assert!(
                engine
                    .publish_original_installation(
                        owner,
                        &mutation.publication,
                        &receipt,
                        (&actual, &mut actual.lock().unwrap()),
                        (OFlag::O_CLOEXEC, None, None),
                        LogicalTime::ZERO,
                    )
                    .is_err()
            );
            assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
        }
    }

    #[test]
    fn removed_original_installation_recovers_exact_prefix_without_resurrecting_slot() {
        for cut in 0..3 {
            let (mut engine, owner, actual, mutation, admission) = setup();
            complete(&mut engine, owner, &admission, 17);
            let receipt =
                crate::network_runtime::original_installation::removed_installation_fixture(
                    owner,
                    actual.clone(),
                    mutation.publication.permit,
                    Source::Socket(admission.call),
                    71,
                    17,
                    true,
                );
            engine
                .confirm_original_socket_publication_result(owner, &admission)
                .unwrap();
            let (binding, batch) = engine
                .prepare_original_installation_publication(
                    owner,
                    &mutation.publication,
                    &receipt,
                    (&actual, &mut actual.lock().unwrap()),
                    (OFlag::empty(), None, None),
                    LogicalTime::ZERO,
                )
                .unwrap();
            if cut >= 1 {
                engine
                    .publish_fd_publication(owner, mutation.publication.permit, &batch)
                    .unwrap();
            }
            if cut >= 2 {
                actual
                    .lock()
                    .unwrap()
                    .publication_acknowledge(&batch)
                    .unwrap();
            }
            let observed = engine
                .publish_original_installation(
                    owner,
                    &mutation.publication,
                    &receipt,
                    (&actual, &mut actual.lock().unwrap()),
                    (OFlag::empty(), None, None),
                    LogicalTime::ZERO,
                )
                .unwrap();
            assert_eq!(observed, binding);
            assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
            assert!(
                engine
                    .lifetime
                    .binding_in_retained_table(binding.slot.files, 17)
                    .is_none()
            );
            assert!(engine.fd_publications[&binding.slot.files].active.is_none());
            assert!(
                engine.fd_publications[&binding.slot.files]
                    .enrollment
                    .is_none()
            );
        }
    }

    fn unexcluded_allocator(
        kind: Kind,
    ) -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        NetworkStreamOwner,
        Arc<Mutex<FileMetadata>>,
        Admission,
        Admission,
    ) {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let peer = NetworkStreamOwner {
            thread: DetTid::from_raw(32),
            ..owner
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.fd_table_fixture_enable();
        let files = engine.fd_publication_fixture_register(owner, None);
        assert_eq!(
            engine.fd_publication_fixture_register(peer, Some(owner)),
            files
        );
        let actual = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        engine
            .associate_fd_metadata(peer, &actual, &actual.lock().unwrap())
            .unwrap();
        let args = |who: NetworkStreamOwner| Arguments {
            kind,
            operation: ExternalOpId::new(who.thread, 10),
            files,
            binding: None,
            fd: match kind {
                Kind::Openat => libc::AT_FDCWD,
                Kind::EpollCreate { legacy: true } => 1,
                Kind::EpollCreate { legacy: false } => 0,
                _ => libc::AF_UNIX,
            },
            address: match kind {
                Kind::Openat => 0x1000,
                Kind::EpollCreate { .. } => kind.syscall() as u64,
                _ => libc::SOCK_STREAM as u64,
            },
            length: if kind == Kind::Openat {
                libc::O_RDWR | libc::O_CLOEXEC
            } else {
                0
            },
            original_count: if kind == Kind::Openat { 0o640 } else { 0 },
        };
        let first = engine.begin_original_allocator(owner, args(owner)).unwrap();
        // The second same-table allocator is admitted while the first original
        // call has no result. The old pre-call Socket-style permit deadlocked
        // a FIFO read-open waiting for this writer-open.
        let second = engine.begin_original_allocator(peer, args(peer)).unwrap();
        assert!(engine.fd_publications[&files].active.is_none());
        assert!(
            engine
                .original_allocator_publication_admission(owner, &first)
                .is_err()
        );
        assert!(
            engine
                .original_allocator_publication_admission(peer, &second)
                .is_err()
        );
        (engine, owner, peer, actual, first, second)
    }

    fn complete_allocator(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &Admission,
        command: u64,
        raw: i64,
    ) {
        engine
            .original_connect_provider_submitted(owner, admission)
            .unwrap();
        engine
            .original_call_prepared(owner, admission, None, command)
            .unwrap();
        engine.original_connect_invoked(owner, admission).unwrap();
        engine
            .original_connect_selected(
                owner,
                admission,
                command,
                (
                    7,
                    owner.thread.as_raw() as u64,
                    101,
                    13,
                    if raw >= 0 { 19 } else { 0 },
                ),
            )
            .unwrap();
        // Backend return alone cannot acquire publication authority until the
        // same command's physical collection/ACK and pin owner have completed.
        engine
            .original_connect_returned(owner, admission, raw)
            .unwrap();
        assert!(
            engine
                .original_allocator_publication_admission(owner, admission)
                .is_err()
        );
        engine
            .original_connect_provider_retired(owner, admission, raw)
            .unwrap();
        engine
            .original_connect_pin_released(owner, admission)
            .unwrap();
    }

    #[test]
    fn allocators_serialize_publication_after_both_original_completions() {
        for kind in [Kind::Socket, Kind::Openat] {
            let (mut engine, owner, peer, actual, first, second) = unexcluded_allocator(kind);
            complete_allocator(&mut engine, owner, &first, 71, -i64::from(libc::ENOENT));
            complete_allocator(&mut engine, peer, &second, 72, -i64::from(libc::EACCES));
            let a = engine
                .original_allocator_publication_admission(owner, &first)
                .unwrap();
            assert_eq!(
                engine
                    .original_allocator_publication_admission(owner, &first)
                    .unwrap(),
                a
            );
            assert!(matches!(
                engine.original_allocator_publication_admission(peer, &second),
                Err(NetworkReplayError::StreamOperationBusy(_))
            ));
            assert!(actual.lock().unwrap().network_descriptor_slots().is_empty());
            engine
                .confirm_original_allocator_publication_result(owner, &first)
                .unwrap();
            engine
                .finish_unchanged_fd_mutation(owner, a.publication.permit)
                .unwrap();
            engine
                .original_allocator_publication_finished(owner, &first, a.publication.permit)
                .unwrap();
            engine.finish_original_connect(owner, &first).unwrap();
            let b = engine
                .original_allocator_publication_admission(peer, &second)
                .unwrap();
            assert_ne!(a.publication.permit, b.publication.permit);
            engine
                .confirm_original_allocator_publication_result(peer, &second)
                .unwrap();
            engine
                .finish_unchanged_fd_mutation(peer, b.publication.permit)
                .unwrap();
            engine
                .original_allocator_publication_finished(peer, &second, b.publication.permit)
                .unwrap();
            engine.finish_original_connect(peer, &second).unwrap();
            assert!(actual.lock().unwrap().network_descriptor_slots().is_empty());
            engine.retire_fd_table_owner(peer);
            engine.stream_owner_gone(peer);
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            engine.finish_fd_mutations().unwrap();
        }
    }

    #[test]
    fn uninvoked_openat_releases_call_without_inventing_a_table_permit_or_errno() {
        let (mut engine, owner, peer, _, first, second) = unexcluded_allocator(Kind::Openat);
        engine
            .abort_original_before_provider(owner, &first)
            .unwrap();
        assert!(
            engine.fd_publications[&first.arguments.files]
                .active
                .is_none()
        );
        assert!(
            engine
                .original_allocator_publication_admission(owner, &first)
                .is_err()
        );
        engine
            .abort_original_before_provider(peer, &second)
            .unwrap();
        engine.retire_fd_table_owner(peer);
        engine.stream_owner_gone(peer);
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
        engine.finish_fd_mutations().unwrap();
    }

    #[test]
    fn openat_publication_preserves_actual_file_class_flags_and_same_call_generation() {
        for (kind, flags) in [
            (crate::fd::FdType::Regular, libc::O_RDONLY),
            (crate::fd::FdType::Regular, libc::O_WRONLY | libc::O_APPEND),
            (crate::fd::FdType::Regular, libc::O_PATH),
            (crate::fd::FdType::Pipe, libc::O_RDWR | libc::O_NONBLOCK),
        ] {
            for removed in [false, true] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(Kind::Openat);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let mutation = engine
                    .original_allocator_publication_admission(owner, &admission)
                    .unwrap();
                let receipt = if removed {
                    crate::network_runtime::original_installation::removed_installation_fixture(
                        owner,
                        actual.clone(),
                        mutation.publication.permit,
                        Source::Openat(admission.call),
                        71,
                        17,
                        true,
                    )
                } else {
                    installation_fixture(
                        owner,
                        actual.clone(),
                        mutation.publication.permit,
                        Source::Openat(admission.call),
                        71,
                        17,
                        false,
                    )
                };
                engine
                    .confirm_original_allocator_publication_result(owner, &admission)
                    .unwrap();
                let binding = engine
                    .publish_original_openat_installation(
                        owner,
                        &mutation.publication,
                        &receipt,
                        (&actual, &mut actual.lock().unwrap()),
                        (
                            OpenatEnrollment {
                                kind,
                                status_flags: flags,
                            },
                            None,
                        ),
                        LogicalTime::ZERO,
                    )
                    .unwrap();
                assert_eq!(binding.slot.fd, 17);
                if removed {
                    assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
                    assert!(
                        engine
                            .lifetime
                            .binding_in_retained_table(binding.slot.files, 17)
                            .is_none()
                    );
                } else {
                    let metadata = actual.lock().unwrap();
                    assert_eq!(metadata.descriptor_binding(17).unwrap(), binding);
                    let fd = metadata.file_handles.get(&17).unwrap();
                    assert_eq!(fd.ty(), kind);
                    assert_eq!(fd.status_flags(), flags);
                    assert!(fd.is_cloexec());
                }
                engine
                    .original_allocator_publication_finished(
                        owner,
                        &admission,
                        mutation.publication.permit,
                    )
                    .unwrap();
                engine.finish_original_connect(owner, &admission).unwrap();
                assert!(
                    engine
                        .publish_original_openat_installation(
                            owner,
                            &mutation.publication,
                            &receipt,
                            (&actual, &mut actual.lock().unwrap()),
                            (
                                OpenatEnrollment {
                                    kind,
                                    status_flags: flags
                                },
                                None
                            ),
                            LogicalTime::ZERO,
                        )
                        .is_err()
                );
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                engine.retire_fd_table_owner(owner);
                engine.stream_owner_gone(owner);
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn completed_allocator_consumption_keeps_unpublished_effect_owned() {
        // This is the currently unresolved terminal publication seam, not a
        // successful cancellation control. It proves that the bounded positive
        // slice does not discard the actual result or pretend it was published.
        for kind in [Kind::Socket, Kind::Openat] {
            let (mut engine, owner, peer, _, admission, unused) = unexcluded_allocator(kind);
            engine
                .abort_original_before_provider(peer, &unused)
                .unwrap();
            complete_allocator(&mut engine, owner, &admission, 71, 17);
            let local = super::super::original_connect::Local {
                arguments: admission.arguments.clone(),
                raw_arguments: [0; 6],
                admission: Some(admission.clone()),
                invoked: true,
                returned: Some(17),
            };
            engine.original_connect_consumed(owner, &local).unwrap();
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                Some(17)
            );
            assert_eq!(
                engine
                    .original_connect_cancellation(owner, &admission)
                    .unwrap(),
                (false, true)
            );
            assert!(
                engine
                    .original_allocator_publication_admission(owner, &admission)
                    .is_err()
            );
            assert!(engine.finish_original_connect(owner, &admission).is_err());
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                Some(17)
            );
            assert!(
                engine.fd_publications[&admission.arguments.files]
                    .active
                    .is_none()
            );
            assert!(engine.finish_fd_mutations().is_err());
        }
    }

    fn consume_allocator(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &Admission,
        final_wait: bool,
    ) {
        let local = super::super::original_connect::Local {
            arguments: admission.arguments.clone(),
            raw_arguments: [0; 6],
            admission: Some(admission.clone()),
            invoked: true,
            returned: Some(17),
        };
        if final_wait {
            engine.original_connect_final_wait(owner, &local).unwrap();
        } else {
            engine.original_connect_consumed(owner, &local).unwrap();
        }
        engine.retire_fd_table_owner(owner);
        engine.stream_owner_gone(owner);
    }

    #[test]
    fn terminal_epoll_publishes_exact_original_through_surviving_shared_table() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::epoll_profile_fixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for kind in [
            Kind::EpollCreate { legacy: true },
            Kind::EpollCreate { legacy: false },
        ] {
            for final_wait in [false, true] {
                for disposition in [
                    TerminalFixture::Live,
                    TerminalFixture::NonfinalPut,
                    TerminalFixture::ExecKeep,
                    TerminalFixture::Close,
                    TerminalFixture::ExecRemove,
                ] {
                    let (mut engine, owner, peer, actual, admission, unused) =
                        unexcluded_allocator(kind);
                    engine
                        .abort_original_before_provider(peer, &unused)
                        .unwrap();
                    complete_allocator(&mut engine, owner, &admission, 71, 17);
                    let source = Source::EpollCreate(admission.call);
                    let descriptor_flags = if matches!(disposition, TerminalFixture::ExecKeep) {
                        0
                    } else {
                        libc::FD_CLOEXEC
                    };
                    let receipt = epoll_profile_fixture(
                        terminal_installation_fixture(
                            owner,
                            actual.clone(),
                            source,
                            71,
                            17,
                            disposition,
                        ),
                        &admission,
                        descriptor_flags,
                    );
                    consume_allocator(&mut engine, owner, &admission, final_wait);
                    assert_eq!(
                        engine
                            .terminal_allocator_successor(owner, &admission)
                            .unwrap(),
                        Some(peer)
                    );
                    assert!(
                        engine
                            .original_allocator_publication_admission(owner, &admission)
                            .is_err()
                    );
                    let (mutation, bound) = engine
                        .begin_terminal_allocator_publication(
                            owner,
                            &admission,
                            peer,
                            &receipt,
                            &actual,
                            &actual.lock().unwrap(),
                        )
                        .unwrap();
                    assert_eq!(mutation.publication.permit.owner, peer);
                    assert_eq!(bound.original_owner(), owner);
                    assert_eq!(
                        engine.original_connect_result(owner, &admission).unwrap(),
                        Some(17)
                    );
                    engine
                        .confirm_original_allocator_publication_result(owner, &admission)
                        .unwrap();
                    let binding = engine
                        .publish_original_epoll_installation(
                            peer,
                            &mutation.publication,
                            &bound,
                            &actual,
                            &mut actual.lock().unwrap(),
                            LogicalTime::ZERO,
                        )
                        .unwrap();
                    if !receipt.removed_before_publication() {
                        let metadata = actual.lock().unwrap();
                        let fd = metadata.file_handles.get(&17).unwrap();
                        assert_eq!(fd.ty(), crate::fd::FdType::Epoll);
                        assert_eq!(fd.is_cloexec(), descriptor_flags != 0);
                    }
                    assert_eq!(
                        actual.lock().unwrap().descriptor_binding(17).ok(),
                        (!receipt.removed_before_publication()).then_some(binding)
                    );
                    engine
                        .original_allocator_publication_finished(
                            owner,
                            &admission,
                            mutation.publication.permit,
                        )
                        .unwrap();
                    engine.finish_original_connect(owner, &admission).unwrap();
                    assert!(
                        engine
                            .terminal_allocator_successor(owner, &admission)
                            .is_err()
                    );
                    engine.retire_fd_table_owner(peer);
                    engine.stream_owner_gone(peer);
                    engine.finish_fd_mutations().unwrap();
                }
            }
        }
    }

    #[test]
    fn terminal_epoll_requires_actual_removal_when_no_table_survives() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::epoll_profile_fixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for kind in [
            Kind::EpollCreate { legacy: true },
            Kind::EpollCreate { legacy: false },
        ] {
            for final_wait in [false, true] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(kind);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let source = Source::EpollCreate(admission.call);
                let live = epoll_profile_fixture(
                    terminal_installation_fixture(
                        owner,
                        actual.clone(),
                        source,
                        71,
                        17,
                        TerminalFixture::Live,
                    ),
                    &admission,
                    libc::FD_CLOEXEC,
                );
                let removed = epoll_profile_fixture(
                    terminal_installation_fixture(
                        owner,
                        actual.clone(),
                        source,
                        71,
                        17,
                        TerminalFixture::FinalPut,
                    ),
                    &admission,
                    libc::FD_CLOEXEC,
                );
                consume_allocator(&mut engine, owner, &admission, final_wait);
                // A surviving peer forbids classifying this as an absent table.
                assert!(
                    engine
                        .reconcile_terminal_removed_allocator(
                            owner,
                            &admission,
                            &removed,
                            &actual,
                            &actual.lock().unwrap()
                        )
                        .is_err()
                );
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                assert_eq!(
                    engine
                        .terminal_allocator_successor(owner, &admission)
                        .unwrap(),
                    None
                );
                assert!(
                    engine
                        .reconcile_terminal_removed_allocator(
                            owner,
                            &admission,
                            &live,
                            &actual,
                            &actual.lock().unwrap()
                        )
                        .is_err()
                );
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                assert!(engine.finish_original_connect(owner, &admission).is_err());
                engine
                    .reconcile_terminal_removed_allocator(
                        owner,
                        &admission,
                        &removed,
                        &actual,
                        &actual.lock().unwrap(),
                    )
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                engine.finish_original_connect(owner, &admission).unwrap();
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn terminal_completed_allocator_publishes_through_exact_surviving_shared_table() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for kind in [Kind::Socket, Kind::Openat] {
            for final_wait in [false, true] {
                for disposition in [
                    TerminalFixture::Live,
                    TerminalFixture::NonfinalPut,
                    TerminalFixture::ExecKeep,
                    TerminalFixture::Close,
                    TerminalFixture::ExecRemove,
                ] {
                    let (mut engine, owner, peer, actual, admission, unused) =
                        unexcluded_allocator(kind);
                    engine
                        .abort_original_before_provider(peer, &unused)
                        .unwrap();
                    complete_allocator(&mut engine, owner, &admission, 71, 17);
                    let source = if kind == Kind::Socket {
                        Source::Socket(admission.call)
                    } else {
                        Source::Openat(admission.call)
                    };
                    let receipt = terminal_installation_fixture(
                        owner,
                        actual.clone(),
                        source,
                        71,
                        17,
                        disposition,
                    );
                    consume_allocator(&mut engine, owner, &admission, final_wait);
                    assert_eq!(
                        engine
                            .terminal_allocator_successor(owner, &admission)
                            .unwrap(),
                        Some(peer)
                    );
                    assert!(
                        engine
                            .original_allocator_publication_admission(owner, &admission)
                            .is_err()
                    );
                    let (mutation, bound) = engine
                        .begin_terminal_allocator_publication(
                            owner,
                            &admission,
                            peer,
                            &receipt,
                            &actual,
                            &actual.lock().unwrap(),
                        )
                        .unwrap();
                    assert_eq!(mutation.publication.permit.owner, peer);
                    assert_eq!(bound.original_owner(), owner);
                    assert_eq!(
                        engine.original_connect_result(owner, &admission).unwrap(),
                        Some(17)
                    );
                    engine
                        .confirm_original_allocator_publication_result(owner, &admission)
                        .unwrap();
                    let binding = if kind == Kind::Openat {
                        engine
                            .publish_original_openat_installation(
                                peer,
                                &mutation.publication,
                                &bound,
                                (&actual, &mut actual.lock().unwrap()),
                                (
                                    OpenatEnrollment {
                                        kind: crate::fd::FdType::Regular,
                                        status_flags: libc::O_RDWR,
                                    },
                                    None,
                                ),
                                LogicalTime::ZERO,
                            )
                            .unwrap()
                    } else {
                        engine
                            .publish_original_installation(
                                peer,
                                &mutation.publication,
                                &bound,
                                (&actual, &mut actual.lock().unwrap()),
                                (OFlag::O_RDWR, None, None),
                                LogicalTime::ZERO,
                            )
                            .unwrap()
                    };
                    assert_eq!(
                        actual.lock().unwrap().descriptor_binding(17).ok(),
                        (!receipt.removed_before_publication()).then_some(binding)
                    );
                    engine
                        .original_allocator_publication_finished(
                            owner,
                            &admission,
                            mutation.publication.permit,
                        )
                        .unwrap();
                    engine.finish_original_connect(owner, &admission).unwrap();
                    assert!(
                        engine
                            .terminal_allocator_successor(owner, &admission)
                            .is_err()
                    );
                    engine.retire_fd_table_owner(peer);
                    engine.stream_owner_gone(peer);
                    engine.finish_fd_mutations().unwrap();
                }
            }
        }
    }

    #[test]
    fn terminal_removed_allocator_requires_actual_removal_when_no_table_survives() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for kind in [Kind::Socket, Kind::Openat] {
            for final_wait in [false, true] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(kind);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let source = if kind == Kind::Socket {
                    Source::Socket(admission.call)
                } else {
                    Source::Openat(admission.call)
                };
                let live = terminal_installation_fixture(
                    owner,
                    actual.clone(),
                    source,
                    71,
                    17,
                    TerminalFixture::Live,
                );
                let removed = terminal_installation_fixture(
                    owner,
                    actual.clone(),
                    source,
                    71,
                    17,
                    TerminalFixture::FinalPut,
                );
                consume_allocator(&mut engine, owner, &admission, final_wait);
                // A surviving peer forbids classifying this as an absent table.
                assert!(
                    engine
                        .reconcile_terminal_removed_allocator(
                            owner,
                            &admission,
                            &removed,
                            &actual,
                            &actual.lock().unwrap()
                        )
                        .is_err()
                );
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                assert_eq!(
                    engine
                        .terminal_allocator_successor(owner, &admission)
                        .unwrap(),
                    None
                );
                assert!(
                    engine
                        .reconcile_terminal_removed_allocator(
                            owner,
                            &admission,
                            &live,
                            &actual,
                            &actual.lock().unwrap()
                        )
                        .is_err()
                );
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                assert!(engine.finish_original_connect(owner, &admission).is_err());
                engine
                    .reconcile_terminal_removed_allocator(
                        owner,
                        &admission,
                        &removed,
                        &actual,
                        &actual.lock().unwrap(),
                    )
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                engine.finish_original_connect(owner, &admission).unwrap();
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn terminal_allocator_rejects_reused_tid_foreign_metadata_and_wrong_result_without_releasing_call()
     {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        let (mut engine, owner, peer, actual, admission, unused) =
            unexcluded_allocator(Kind::Openat);
        engine
            .abort_original_before_provider(peer, &unused)
            .unwrap();
        complete_allocator(&mut engine, owner, &admission, 71, 17);
        let receipt = terminal_installation_fixture(
            owner,
            actual.clone(),
            Source::Openat(admission.call),
            71,
            17,
            TerminalFixture::Live,
        );
        let wrong = terminal_installation_fixture(
            owner,
            actual.clone(),
            Source::Openat(admission.call),
            71,
            18,
            TerminalFixture::Live,
        );
        consume_allocator(&mut engine, owner, &admission, false);
        let replacement = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        let replacement_files =
            detcore_model::fd::FilesIdAllocator::default().allocate_exec(owner.thread);
        engine
            .lifetime
            .register(
                TaskOwner {
                    tid: replacement.thread,
                    mm: replacement.mm,
                },
                replacement.thread,
                replacement_files,
            )
            .unwrap();
        let foreign = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(peer.thread)));
        assert!(
            engine
                .begin_terminal_allocator_publication(
                    owner,
                    &admission,
                    replacement,
                    &receipt,
                    &actual,
                    &actual.lock().unwrap()
                )
                .is_err()
        );
        assert!(
            engine
                .begin_terminal_allocator_publication(
                    owner,
                    &admission,
                    peer,
                    &receipt,
                    &foreign,
                    &foreign.lock().unwrap()
                )
                .is_err()
        );
        assert!(
            engine
                .begin_terminal_allocator_publication(
                    owner,
                    &admission,
                    peer,
                    &wrong,
                    &actual,
                    &actual.lock().unwrap()
                )
                .is_err()
        );
        assert!(
            engine.fd_publications[&admission.arguments.files]
                .active
                .is_none()
        );
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            Some(17)
        );
        assert!(engine.finish_original_connect(owner, &admission).is_err());
    }

    #[test]
    fn terminal_allocator_transfers_same_unpublished_lease_without_reissuing_original() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for kind in [Kind::Socket, Kind::Openat] {
            for survives in [false, true] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(kind);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let prior = engine
                    .original_allocator_publication_admission(owner, &admission)
                    .unwrap();
                let source = if kind == Kind::Socket {
                    Source::Socket(admission.call)
                } else {
                    Source::Openat(admission.call)
                };
                let receipt = terminal_installation_fixture(
                    owner,
                    actual.clone(),
                    source,
                    71,
                    17,
                    if survives {
                        TerminalFixture::NonfinalPut
                    } else {
                        TerminalFixture::FinalPut
                    },
                );
                consume_allocator(&mut engine, owner, &admission, false);
                if survives {
                    let (mutation, bound) = engine
                        .begin_terminal_allocator_publication(
                            owner,
                            &admission,
                            peer,
                            &receipt,
                            &actual,
                            &actual.lock().unwrap(),
                        )
                        .unwrap();
                    assert_eq!(
                        mutation.publication.permit.lease,
                        prior.publication.permit.lease
                    );
                    assert_eq!(mutation.publication.permit.owner, peer);
                    assert_eq!(bound.original_owner(), owner);
                    engine
                        .confirm_original_allocator_publication_result(owner, &admission)
                        .unwrap();
                    if kind == Kind::Openat {
                        engine
                            .publish_original_openat_installation(
                                peer,
                                &mutation.publication,
                                &bound,
                                (&actual, &mut actual.lock().unwrap()),
                                (
                                    OpenatEnrollment {
                                        kind: crate::fd::FdType::Regular,
                                        status_flags: libc::O_RDWR,
                                    },
                                    None,
                                ),
                                LogicalTime::ZERO,
                            )
                            .unwrap();
                    } else {
                        engine
                            .publish_original_installation(
                                peer,
                                &mutation.publication,
                                &bound,
                                (&actual, &mut actual.lock().unwrap()),
                                (OFlag::O_RDWR, None, None),
                                LogicalTime::ZERO,
                            )
                            .unwrap();
                    }
                    engine
                        .original_allocator_publication_finished(
                            owner,
                            &admission,
                            mutation.publication.permit,
                        )
                        .unwrap();
                } else {
                    engine.retire_fd_table_owner(peer);
                    engine.stream_owner_gone(peer);
                    engine
                        .reconcile_terminal_removed_allocator(
                            owner,
                            &admission,
                            &receipt,
                            &actual,
                            &actual.lock().unwrap(),
                        )
                        .unwrap();
                }
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                engine.finish_original_connect(owner, &admission).unwrap();
                if survives {
                    engine.retire_fd_table_owner(peer);
                    engine.stream_owner_gone(peer);
                }
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn terminal_allocator_sys_exit_proof_does_not_fabricate_backend_return_or_errno() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        use crate::network_runtime::original_installation::terminal_no_installation_fixture;
        use crate::network_runtime::original_installation::terminal_result_fixture;
        for kind in [Kind::Socket, Kind::Openat] {
            for returned in [17, -libc::ENOENT] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(kind);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                engine
                    .original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                engine
                    .original_call_prepared(owner, &admission, None, 71)
                    .unwrap();
                engine.original_connect_invoked(owner, &admission).unwrap();
                let local = super::super::original_connect::Local {
                    arguments: admission.arguments.clone(),
                    raw_arguments: [0; 6],
                    admission: Some(admission.clone()),
                    invoked: true,
                    returned: None,
                };
                engine.original_connect_final_wait(owner, &local).unwrap();
                assert!(
                    engine
                        .original_connect_dead_retired(owner, &admission, 71)
                        .is_err()
                );
                let result = terminal_result_fixture(owner, &admission, 71, returned);
                let mut incomplete = result.clone();
                incomplete.complete = 0;
                assert!(
                    engine
                        .original_terminal_allocator_completed(owner, &admission, &incomplete)
                        .is_err()
                );
                let mut foreign = result.clone();
                foreign.selection.owner_mm += 1;
                assert!(
                    engine
                        .original_terminal_allocator_completed(owner, &admission, &foreign)
                        .is_err()
                );
                engine
                    .original_terminal_allocator_completed(owner, &admission, &result)
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                engine
                    .original_connect_dead_retired(owner, &admission, 71)
                    .unwrap();
                engine
                    .original_connect_pin_released(owner, &admission)
                    .unwrap();
                engine.retire_fd_table_owner(owner);
                engine.stream_owner_gone(owner);
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                if returned >= 0 {
                    let source = if kind == Kind::Socket {
                        Source::Socket(admission.call)
                    } else {
                        Source::Openat(admission.call)
                    };
                    let receipt = terminal_installation_fixture(
                        owner,
                        actual.clone(),
                        source,
                        71,
                        returned,
                        TerminalFixture::FinalPut,
                    );
                    engine
                        .reconcile_terminal_removed_allocator(
                            owner,
                            &admission,
                            &receipt,
                            &actual,
                            &actual.lock().unwrap(),
                        )
                        .unwrap();
                } else {
                    let wrong = terminal_no_installation_fixture(
                        owner,
                        actual.clone(),
                        &admission,
                        71,
                        -i64::from(libc::EBADF),
                    );
                    assert!(
                        engine
                            .reconcile_terminal_failed_allocator(owner, &admission, &wrong)
                            .is_err()
                    );
                    let receipt = terminal_no_installation_fixture(
                        owner,
                        actual.clone(),
                        &admission,
                        71,
                        i64::from(returned),
                    );
                    engine
                        .reconcile_terminal_failed_allocator(owner, &admission, &receipt)
                        .unwrap();
                }
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                engine.finish_original_connect(owner, &admission).unwrap();
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn terminal_allocator_recovers_exact_prior_prefix_before_ordered_later_removal() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for applied in [false, true] {
            for removed in [false, true] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(Kind::Socket);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let prior = engine
                    .original_allocator_publication_admission(owner, &admission)
                    .unwrap();
                engine
                    .confirm_original_allocator_publication_result(owner, &admission)
                    .unwrap();
                let receipt = installation_fixture(
                    owner,
                    actual.clone(),
                    prior.publication.permit,
                    Source::Socket(admission.call),
                    71,
                    17,
                    false,
                );
                let (binding, batch) = engine
                    .prepare_original_installation_publication(
                        owner,
                        &prior.publication,
                        &receipt,
                        (&actual, &mut actual.lock().unwrap()),
                        (OFlag::O_RDWR, None, None),
                        LogicalTime::ZERO,
                    )
                    .unwrap();
                if applied {
                    engine
                        .publish_fd_publication(owner, prior.publication.permit, &batch)
                        .unwrap();
                }
                consume_allocator(&mut engine, owner, &admission, false);
                let terminal = terminal_installation_fixture(
                    owner,
                    actual.clone(),
                    Source::Socket(admission.call),
                    71,
                    17,
                    if removed {
                        TerminalFixture::Close
                    } else {
                        TerminalFixture::NonfinalPut
                    },
                );
                let (mutation, _) = engine
                    .begin_terminal_allocator_publication(
                        owner,
                        &admission,
                        peer,
                        &terminal,
                        &actual,
                        &actual.lock().unwrap(),
                    )
                    .unwrap();
                assert_eq!(mutation.publication.recovery.as_ref(), Some(&batch));
                assert_eq!(
                    mutation.publication.permit.lease,
                    prior.publication.permit.lease
                );
                let recovered = engine
                    .recover_terminal_allocator_publication(
                        owner,
                        &admission,
                        peer,
                        &mutation,
                        &terminal,
                        (&actual, &mut actual.lock().unwrap()),
                    )
                    .unwrap();
                assert_eq!(recovered, binding);
                assert_eq!(
                    actual.lock().unwrap().descriptor_binding(17).ok(),
                    (!removed).then_some(binding)
                );
                engine
                    .original_allocator_publication_finished(
                        owner,
                        &admission,
                        mutation.publication.permit,
                    )
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                engine.finish_original_connect(owner, &admission).unwrap();
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn terminal_allocator_rejects_changed_original_confirmation_or_applied_prefix() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for applied in [false, true] {
            for corrupt in 0..4 {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(Kind::Socket);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let prior = engine
                    .original_allocator_publication_admission(owner, &admission)
                    .unwrap();
                engine
                    .confirm_original_allocator_publication_result(owner, &admission)
                    .unwrap();
                let receipt = installation_fixture(
                    owner,
                    actual.clone(),
                    prior.publication.permit,
                    Source::Socket(admission.call),
                    71,
                    17,
                    false,
                );
                let (binding, batch) = engine
                    .prepare_original_installation_publication(
                        owner,
                        &prior.publication,
                        &receipt,
                        (&actual, &mut actual.lock().unwrap()),
                        (OFlag::O_RDWR, None, None),
                        LogicalTime::ZERO,
                    )
                    .unwrap();
                let permit = prior.publication.permit;
                let confirmation = engine.fd_installations[&permit.lease].clone();
                if applied {
                    engine
                        .publish_fd_publication(owner, permit, &batch)
                        .unwrap();
                }
                consume_allocator(&mut engine, owner, &admission, false);
                let terminal = terminal_installation_fixture(
                    owner,
                    actual.clone(),
                    Source::Socket(admission.call),
                    71,
                    17,
                    TerminalFixture::NonfinalPut,
                );
                match (applied, corrupt) {
                    (true, 0) => {
                        engine
                            .fd_publication_history
                            .remove(&(permit.files, batch.sequence));
                    }
                    (false, 0) => {
                        engine
                            .fd_publication_history
                            .insert((permit.files, batch.sequence), batch.clone());
                    }
                    (true, 1) => {
                        engine
                            .fd_publications
                            .get_mut(&permit.files)
                            .unwrap()
                            .pending = None;
                    }
                    (false, 1) => {
                        engine
                            .fd_publications
                            .get_mut(&permit.files)
                            .unwrap()
                            .pending = Some(batch.clone());
                    }
                    (true, 2) => {
                        engine.fd_installations.insert(permit.lease, confirmation);
                    }
                    (false, 2) => {
                        engine.fd_installations.remove(&permit.lease);
                    }
                    (_, 3) => {
                        engine
                            .fd_publications
                            .get_mut(&permit.files)
                            .unwrap()
                            .enrollment
                            .as_mut()
                            .unwrap()
                            .completed = !applied;
                    }
                    _ => unreachable!(),
                }
                let active = engine.fd_publications[&permit.files].active;
                assert!(
                    engine
                        .begin_terminal_allocator_publication(
                            owner,
                            &admission,
                            peer,
                            &terminal,
                            &actual,
                            &actual.lock().unwrap()
                        )
                        .is_err(),
                    "applied={applied} corrupt={corrupt}"
                );
                assert_eq!(engine.fd_publications[&permit.files].active, active);
                assert_eq!(
                    actual.lock().unwrap().descriptor_binding(17).unwrap(),
                    binding
                );
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(17)
                );
                assert!(engine.finish_original_connect(owner, &admission).is_err());
            }
        }
    }

    #[test]
    fn terminal_allocator_retains_partial_enrollment_until_exact_dead_table_removal() {
        use crate::network_runtime::original_installation::TerminalFixture;
        use crate::network_runtime::original_installation::terminal_installation_fixture;
        for applied in [false, true] {
            let (mut engine, owner, peer, actual, admission, unused) =
                unexcluded_allocator(Kind::Socket);
            engine
                .abort_original_before_provider(peer, &unused)
                .unwrap();
            complete_allocator(&mut engine, owner, &admission, 71, 17);
            let prior = engine
                .original_allocator_publication_admission(owner, &admission)
                .unwrap();
            engine
                .confirm_original_allocator_publication_result(owner, &admission)
                .unwrap();
            let receipt = installation_fixture(
                owner,
                actual.clone(),
                prior.publication.permit,
                Source::Socket(admission.call),
                71,
                17,
                false,
            );
            let (_, batch) = engine
                .prepare_original_installation_publication(
                    owner,
                    &prior.publication,
                    &receipt,
                    (&actual, &mut actual.lock().unwrap()),
                    (OFlag::O_RDWR, None, None),
                    LogicalTime::ZERO,
                )
                .unwrap();
            if applied {
                engine
                    .publish_fd_publication(owner, prior.publication.permit, &batch)
                    .unwrap();
            }
            consume_allocator(&mut engine, owner, &admission, false);
            let removed = terminal_installation_fixture(
                owner,
                actual.clone(),
                Source::Socket(admission.call),
                71,
                17,
                TerminalFixture::FinalPut,
            );
            assert!(
                engine
                    .reconcile_terminal_removed_allocator(
                        owner,
                        &admission,
                        &removed,
                        &actual,
                        &actual.lock().unwrap()
                    )
                    .is_err()
            );
            assert!(
                engine.fd_publications[&admission.arguments.files]
                    .enrollment
                    .is_some()
            );
            engine.retire_fd_table_owner(peer);
            engine.stream_owner_gone(peer);
            engine
                .reconcile_terminal_removed_allocator(
                    owner,
                    &admission,
                    &removed,
                    &actual,
                    &actual.lock().unwrap(),
                )
                .unwrap();
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                Some(17)
            );
            engine.finish_original_connect(owner, &admission).unwrap();
            engine.finish_fd_mutations().unwrap();
        }
    }

    #[test]
    fn terminal_failed_allocator_preserves_actual_errno_through_old_permit_cleanup() {
        use crate::network_runtime::original_installation::terminal_no_installation_fixture;
        for kind in [Kind::Socket, Kind::Openat] {
            for admitted in [false, true] {
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(kind);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                let returned = -i64::from(libc::EMFILE);
                complete_allocator(&mut engine, owner, &admission, 71, returned);
                if admitted {
                    engine
                        .original_allocator_publication_admission(owner, &admission)
                        .unwrap();
                }
                let local = super::super::original_connect::Local {
                    arguments: admission.arguments.clone(),
                    raw_arguments: [0; 6],
                    admission: Some(admission.clone()),
                    invoked: true,
                    returned: Some(returned),
                };
                engine.original_connect_consumed(owner, &local).unwrap();
                engine.retire_fd_table_owner(owner);
                engine.stream_owner_gone(owner);
                let receipt =
                    terminal_no_installation_fixture(owner, actual, &admission, 71, returned);
                engine
                    .reconcile_terminal_failed_allocator(owner, &admission, &receipt)
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    Some(returned)
                );
                engine.finish_original_connect(owner, &admission).unwrap();
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                engine.finish_fd_mutations().unwrap();
            }
        }
    }

    #[test]
    fn epoll_publication_requires_original_profile_and_keeps_actual_descriptor_flags() {
        use crate::network_runtime::original_installation::epoll_profile_fixture;
        for legacy in [false, true] {
            for descriptor_flags in [0, libc::FD_CLOEXEC] {
                let kind = Kind::EpollCreate { legacy };
                let (mut engine, owner, peer, actual, admission, unused) =
                    unexcluded_allocator(kind);
                engine
                    .abort_original_before_provider(peer, &unused)
                    .unwrap();
                complete_allocator(&mut engine, owner, &admission, 71, 17);
                let mutation = engine
                    .original_allocator_publication_admission(owner, &admission)
                    .unwrap();
                engine
                    .confirm_original_allocator_publication_result(owner, &admission)
                    .unwrap();
                let bare = installation_fixture(
                    owner,
                    actual.clone(),
                    mutation.publication.permit,
                    Source::EpollCreate(admission.call),
                    71,
                    17,
                    false,
                );
                // Creator identity alone cannot supply the observed descriptor bit.
                assert!(
                    engine
                        .publish_original_epoll_installation(
                            owner,
                            &mutation.publication,
                            &bare,
                            &actual,
                            &mut actual.lock().unwrap(),
                            LogicalTime::ZERO
                        )
                        .is_err()
                );
                assert!(actual.lock().unwrap().descriptor_binding(17).is_err());
                let receipt = epoll_profile_fixture(bare, &admission, descriptor_flags);
                let binding = engine
                    .publish_original_epoll_installation(
                        owner,
                        &mutation.publication,
                        &receipt,
                        &actual,
                        &mut actual.lock().unwrap(),
                        LogicalTime::ZERO,
                    )
                    .unwrap();
                let task = TaskOwner {
                    tid: owner.thread,
                    mm: owner.mm,
                };
                assert!(engine.lifetime.has_original_creation(
                    task,
                    binding,
                    lifetime::OriginalCreationKind::Epoll
                ));
                assert!(!engine.lifetime.has_original_creation(
                    task,
                    binding,
                    lifetime::OriginalCreationKind::Socket
                ));
                assert!(engine.fd_publication_history.is_empty());
                assert_eq!(engine.lifetime.pending_publication_payloads_for_test(), 0);
                {
                    let metadata = actual.lock().unwrap();
                    assert_eq!(metadata.descriptor_binding(17).unwrap(), binding);
                    let fd = metadata.file_handles.get(&17).unwrap();
                    assert_eq!(fd.ty(), crate::fd::FdType::Epoll);
                    assert_eq!(fd.status_flags(), libc::O_RDWR);
                    // Input flags are zero (or legacy size1), but actual CLOEXEC
                    // still comes from the allocating table's receipt.
                    assert_eq!(fd.is_cloexec(), descriptor_flags == libc::FD_CLOEXEC);
                }
                engine
                    .original_allocator_publication_finished(
                        owner,
                        &admission,
                        mutation.publication.permit,
                    )
                    .unwrap();
                engine.finish_original_connect(owner, &admission).unwrap();
                engine.retire_fd_table_owner(peer);
                engine.stream_owner_gone(peer);
                engine.retire_fd_table_owner(owner);
                engine.stream_owner_gone(owner);
                engine.finish_fd_mutations().unwrap();
            }
        }
    }
}
