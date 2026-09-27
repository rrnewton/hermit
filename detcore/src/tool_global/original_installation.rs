//! Original allocation facts use the same metadata/publication transaction.
use super::*;
use crate::network_replay::NetworkAcceptLeaseId;
use crate::network_replay::NetworkFdPublicationAdmission;
use crate::network_runtime::original_installation::Installation;
use crate::types::FdSlotBinding;

fn internal(error: impl std::fmt::Display) -> NetworkRpcError {
    NetworkRpcError::internal(error.to_string())
}

impl GlobalState {
    fn installation_owner_current(&self, owner: NetworkStreamOwner) -> Result<(), NetworkRpcError> {
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed()
            || sched.thread_is_logically_killed(owner.thread)
            || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(internal("original installation owner is no longer current"));
        }
        Ok(())
    }

    /// Shared synchronous commit. Neither consumer looks up the returned
    /// numeric FD after an await or supplies its own OpenFileId certificate.
    fn publish_original_installation(
        &self,
        owner: NetworkStreamOwner,
        admission: &NetworkFdPublicationAdmission,
        installation: &Installation,
        flags: nix::fcntl::OFlag,
        stat: Option<crate::stat::DetStat>,
    ) -> Result<FdSlotBinding, NetworkRpcError> {
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed()
            || sched.thread_is_logically_killed(owner.thread)
            || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(internal(
                "original installation lost owner before atomic commit",
            ));
        }
        let actual = installation.metadata();
        let mut metadata = actual.lock().unwrap();
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("original installation engine missing"))?
            .lock()
            .unwrap();
        let result = engine.publish_original_installation(
            owner,
            admission,
            installation,
            &actual,
            &mut metadata,
            flags,
            stat,
            None,
            self.global_time.lock().unwrap().as_nanos(),
        );
        self.release_lifetime_ports(engine.take_lifetime_retired_ports());
        let binding = result.map_err(internal)?;
        drop(engine);
        drop(metadata);
        drop(sched);
        self.network_stream_changed.notify_waiters();
        Ok(binding)
    }

    async fn original_allocator_publication_admission(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> Result<crate::network_replay::NetworkFdMutationAdmission, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("allocator runtime missing"))?;
        let (files, actual) = runtime
            .original_allocator_metadata(owner, admission)
            .map_err(internal)?;
        loop {
            let changed = self.network_stream_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (attempt, failed) = {
                let sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(owner.thread)
                    || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                    || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                {
                    return Err(internal("allocator publication lost its registered owner"));
                }
                let metadata = actual.lock().unwrap();
                let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
                engine
                    .validate_fd_metadata(owner, files, &actual, &metadata)
                    .map_err(internal)?;
                // This validates the original actual completion before acquiring
                // and retaining the existing table permit in the same Call.
                let result = engine.original_allocator_publication_admission(owner, admission);
                (result, sched.backend_failure_waiter())
            };
            match attempt {
                Ok(admitted) => return Ok(admitted),
                Err(NetworkReplayError::StreamOperationBusy(_)) => {
                    tokio::select! { _=changed=>{}, _=failed=>return Err(internal("backend failed during allocator publication")) }
                }
                Err(error) => return Err(internal(error)),
            }
        }
    }

    pub(super) async fn publish_original_socket(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
        stat: Option<crate::stat::DetStat>,
        enrollment: Option<crate::network_replay::original_installation::FreshStreamEnrollment>,
    ) -> Result<Option<FdSlotBinding>, NetworkRpcError> {
        self.installation_owner_current(owner)?;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("Socket runtime missing"))?;
        if let Some(binding) = runtime
            .original_socket_installed(owner, admission)
            .map_err(internal)?
        {
            return Ok(binding);
        }
        let mutation = self
            .original_allocator_publication_admission(owner, admission)
            .await?;
        let (same, returned) = self
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_socket_publication(owner, admission)
            .map_err(internal)?;
        if same != mutation {
            return Err(internal("Socket changed completed publication admission"));
        }
        let receipt = runtime
            .original_socket_installation(owner, admission, mutation.publication.permit)
            .await
            .map_err(internal)?;
        let observed = runtime
            .original_socket_observation(owner, admission)
            .map_err(internal)?;
        match (&receipt, &observed) {
            (None, None) if stat.is_none() && enrollment.is_none() => {}
            (Some(receipt), None)
                if receipt.removed_before_publication()
                    && stat.is_none()
                    && enrollment.is_none() => {}
            (Some(receipt), Some(observed)) => {
                if !receipt.matches_observed_file(observed.provider, observed.file)
                    || observed.metadata.status_flags
                        != (libc::O_RDWR
                            | (admission.arguments.address as u32 as i32 & libc::SOCK_NONBLOCK))
                    || stat
                        != self
                            .cfg
                            .virtualize_metadata
                            .then_some(observed.metadata.stat)
                {
                    return Err(internal(
                        "Socket metadata changed its exact held original file",
                    ));
                }
                if let Some(fresh) = &enrollment {
                    if fresh.namespace.inode != observed.namespace {
                        return Err(internal("Socket enrollment changed observed namespace"));
                    }
                    match (&fresh.observed_profile, self.cfg.network_trace.policy) {
                        (Some(profile), NetworkPolicy::Record)
                            if *profile
                                == observed
                                    .fresh_profile(fresh.key, profile.normalization.clone())
                                    .map_err(internal)? => {}
                        (None, NetworkPolicy::Replay) => {}
                        _ => return Err(internal("Socket enrollment changed held fresh profile")),
                    }
                }
            }
            _ => {
                return Err(internal(
                    "live Socket publication lacks its exact held metadata",
                ));
            }
        }
        // A fresh cut and complete original result precede this synchronized
        // consumption. Exact owner registration cannot disappear inside commit.
        let binding = {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(internal("Socket publication lost exact registered owner"));
            }
            let actual = receipt.as_ref().map(Installation::metadata);
            let mut local = actual.as_ref().map(|actual| actual.lock().unwrap());
            let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
            let (current, raw) = engine
                .original_socket_publication(owner, admission)
                .map_err(internal)?;
            if current != mutation || raw != returned {
                return Err(internal("Socket publication changed retained result"));
            }
            engine
                .confirm_original_socket_publication_result(owner, admission)
                .map_err(internal)?;
            let binding = if let Some(receipt) = &receipt {
                let result = engine.publish_original_installation(
                    owner,
                    &mutation.publication,
                    receipt,
                    actual.as_ref().unwrap(),
                    local.as_mut().unwrap(),
                    crate::network_replay::original_installation::socket_installation_flags(
                        admission.arguments.address as u32 as i32,
                    ),
                    stat,
                    enrollment,
                    self.global_time.lock().unwrap().as_nanos(),
                );
                self.release_lifetime_ports(engine.take_lifetime_retired_ports());
                Some(result.map_err(internal)?)
            } else {
                engine
                    .finish_unchanged_fd_mutation(owner, mutation.publication.permit)
                    .map_err(internal)?;
                None
            };
            engine
                .original_socket_publication_finished(owner, admission, mutation.publication.permit)
                .map_err(internal)?;
            runtime
                .retain_original_socket_installed(owner, admission, binding)
                .map_err(internal)?;
            binding
        };
        self.network_stream_changed.notify_waiters();
        Ok(binding)
    }

    pub(super) async fn publish_original_epoll(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> Result<Option<FdSlotBinding>, NetworkRpcError> {
        use crate::network_replay::original_connect::Kind;
        if !matches!(admission.arguments.kind, Kind::EpollCreate { .. }) {
            return Err(internal(
                "epoll publication changed original allocator kind",
            ));
        }
        self.installation_owner_current(owner)?;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("epoll runtime missing"))?;
        if let Some(binding) = runtime
            .original_socket_installed(owner, admission)
            .map_err(internal)?
        {
            return Ok(binding);
        }
        let mutation = self
            .original_allocator_publication_admission(owner, admission)
            .await?;
        let (same, returned) = self
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_allocator_publication(owner, admission)
            .map_err(internal)?;
        if same != mutation {
            return Err(internal("epoll changed completed publication admission"));
        }
        let receipt = runtime
            .original_allocator_installation(owner, admission, mutation.publication.permit)
            .await
            .map_err(internal)?;
        let binding = {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(internal("epoll publication lost exact registered owner"));
            }
            let actual = receipt.as_ref().map(Installation::metadata);
            let mut local = actual.as_ref().map(|actual| actual.lock().unwrap());
            let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
            let (current, raw) = engine
                .original_allocator_publication(owner, admission)
                .map_err(internal)?;
            if current != mutation || raw != returned {
                return Err(internal("epoll changed actual original result"));
            }
            engine
                .confirm_original_allocator_publication_result(owner, admission)
                .map_err(internal)?;
            let binding = if let Some(receipt) = &receipt {
                Some(
                    engine
                        .publish_original_epoll_installation(
                            owner,
                            &mutation.publication,
                            receipt,
                            actual.as_ref().unwrap(),
                            local.as_mut().unwrap(),
                            self.global_time.lock().unwrap().as_nanos(),
                        )
                        .map_err(internal)?,
                )
            } else {
                engine
                    .finish_unchanged_fd_mutation(owner, mutation.publication.permit)
                    .map_err(internal)?;
                None
            };
            engine
                .original_allocator_publication_finished(
                    owner,
                    admission,
                    mutation.publication.permit,
                )
                .map_err(internal)?;
            runtime
                .retain_original_socket_installed(owner, admission, binding)
                .map_err(internal)?;
            binding
        };
        self.network_stream_changed.notify_waiters();
        Ok(binding)
    }

    pub(super) async fn publish_original_openat(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> Result<crate::network_runtime::original_installation::OpenatPublication, NetworkRpcError>
    {
        use crate::network_replay::original_installation::OpenatEnrollment;
        use crate::network_runtime::original_installation::OpenatPublication;
        self.installation_owner_current(owner)?;
        if self.cfg.sequentialize_threads {
            self.sched
                .lock()
                .unwrap()
                .ordinary_fd_observation(owner)
                .map_err(|error| {
                    internal(format!(
                        "Openat publication lacks foreground continuation: {error:?}"
                    ))
                })?;
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("Openat runtime missing"))?;
        if let Some(published) = runtime
            .original_openat_published(owner, admission)
            .map_err(internal)?
        {
            return Ok(published);
        }
        // The existing worker completed op23, getattr and explicit close before
        // the guest paid its continuation. Foreground publication can only read
        // that exact retained result; it must never start or wait for a helper.
        let observed = runtime
            .original_openat_observed(owner, admission)
            .map_err(internal)?;
        // Actual generation equality above does not require a table permit.
        // Acquire it only after potentially blocking filesystem work has ended.
        let mutation = self
            .original_allocator_publication_admission(owner, admission)
            .await?;
        let (same, returned) = self
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_allocator_publication(owner, admission)
            .map_err(internal)?;
        if same != mutation {
            return Err(internal("Openat changed completed publication admission"));
        }
        let receipt = runtime
            .original_allocator_installation(owner, admission, mutation.publication.permit)
            .await
            .map_err(internal)?;
        let opened = match (&receipt, &observed) {
            (None, None) => None,
            (Some(receipt), None) if receipt.removed_before_publication() => {
                let profile = receipt.allocated_profile().map_err(internal)?;
                Some(OpenatEnrollment {
                    kind: profile.kind().map_err(internal)?,
                    status_flags: profile.status_flags,
                })
            }
            (Some(receipt), Some(observed))
                if receipt.matches_observed_file(observed.provider, observed.file) =>
            {
                Some(OpenatEnrollment {
                    kind: observed.kind().map_err(internal)?,
                    status_flags: observed.status_flags,
                })
            }
            _ => {
                return Err(internal(
                    "Openat live publication lacks its exact original held file",
                ));
            }
        };
        let live = receipt
            .as_ref()
            .is_some_and(|r| !r.removed_before_publication());
        let annotation_stat = live.then(|| observed.as_ref().map(|o| o.stat)).flatten();
        let resolved_path = live
            .then(|| observed.as_ref().and_then(|o| o.resolved_path.clone()))
            .flatten();
        let published = {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(internal("Openat publication lost exact registered owner"));
            }
            let actual = receipt.as_ref().map(Installation::metadata);
            let mut local = actual.as_ref().map(|actual| actual.lock().unwrap());
            let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
            let (current, raw) = engine
                .original_allocator_publication(owner, admission)
                .map_err(internal)?;
            if current != mutation || raw != returned {
                return Err(internal("Openat publication changed actual result"));
            }
            engine
                .confirm_original_allocator_publication_result(owner, admission)
                .map_err(internal)?;
            let binding = if let Some(receipt) = &receipt {
                let result = engine.publish_original_openat_installation(
                    owner,
                    &mutation.publication,
                    receipt,
                    actual.as_ref().unwrap(),
                    local.as_mut().unwrap(),
                    opened.unwrap(),
                    self.cfg
                        .virtualize_metadata
                        .then_some(annotation_stat)
                        .flatten(),
                    self.global_time.lock().unwrap().as_nanos(),
                );
                self.release_lifetime_ports(engine.take_lifetime_retired_ports());
                Some(result.map_err(internal)?)
            } else {
                engine
                    .finish_unchanged_fd_mutation(owner, mutation.publication.permit)
                    .map_err(internal)?;
                None
            };
            engine
                .original_allocator_publication_finished(
                    owner,
                    admission,
                    mutation.publication.permit,
                )
                .map_err(internal)?;
            let published = OpenatPublication {
                binding,
                live,
                stat: annotation_stat,
                resolved_path,
            };
            runtime
                .retain_original_openat_published(owner, admission, published.clone())
                .map_err(internal)?;
            published
        };
        self.network_stream_changed.notify_waiters();
        Ok(published)
    }

    async fn accepted_publication_admission(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<NetworkFdPublicationAdmission, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("accepted runtime missing"))?;
        let (files, actual) = runtime
            .accepted_installation_metadata(owner, lease)
            .map_err(internal)?;
        // Only after the real accept and its complete child/command matching.
        // A blocking accept never owns this table permit across its queue wait.
        let admission = if let Some(admission) = runtime
            .accepted_installation_admission(owner, lease)
            .map_err(internal)?
        {
            admission
        } else {
            loop {
                let changed = self.network_stream_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let (attempt, failed) = {
                    let sched = self.sched.lock().unwrap();
                    if sched.backend_failed()
                        || sched.thread_is_logically_killed(owner.thread)
                        || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                        || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                            != Some(&owner.mm)
                    {
                        return Err(internal("accepted publication lost its registered owner"));
                    }
                    let metadata = actual.lock().unwrap();
                    let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
                    engine
                        .accepted_capture_call(owner, lease)
                        .map_err(internal)?;
                    engine
                        .validate_fd_metadata(owner, files, &actual, &metadata)
                        .map_err(internal)?;
                    let admitted = engine.acquire_fd_publication(owner, files);
                    if let Ok(admission) = &admitted {
                        // No suspension separates acquisition from retained Accept
                        // custody. A lost callback resumes this same cut/permit.
                        runtime
                            .retain_accepted_installation_admission(owner, lease, admission)
                            .map_err(internal)?;
                    }
                    (admitted, sched.backend_failure_waiter())
                };
                match attempt {
                    Ok(admission) => break admission,
                    Err(NetworkReplayError::StreamOperationBusy(_)) => {
                        tokio::select! { _=changed=>{}, _=failed=>return Err(internal("backend failed during accepted publication")) }
                    }
                    Err(error) => return Err(internal(error)),
                }
            }
        };
        Ok(admission)
    }

    pub(super) async fn publish_accepted_original_no_installation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<(), NetworkRpcError> {
        self.installation_owner_current(owner)?;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("accepted runtime missing"))?;
        if runtime
            .accepted_no_installation_published(owner, lease)
            .map_err(internal)?
        {
            return Ok(());
        }
        let admission = self.accepted_publication_admission(owner, lease).await?;
        let receipt = runtime
            .accepted_original_no_installation(owner, lease, admission.permit)
            .await
            .map_err(internal)?;
        let (_, actual) = runtime
            .accepted_installation_metadata(owner, lease)
            .map_err(internal)?;
        {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(internal(
                    "negative accept lost its registered owner before commit",
                ));
            }
            let metadata = actual.lock().unwrap();
            let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
            engine
                .validate_fd_metadata(owner, admission.permit.files, &actual, &metadata)
                .map_err(internal)?;
            engine
                .confirm_original_accepted_no_installation(
                    owner,
                    lease,
                    admission.permit,
                    &actual,
                    &receipt,
                )
                .map_err(internal)?;
            runtime
                .retain_accepted_no_installation_published(owner, lease, &receipt)
                .map_err(internal)?;
        }
        self.network_stream_changed.notify_waiters();
        Ok(())
    }

    pub(super) async fn publish_accepted_original_installation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<FdSlotBinding, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("accepted runtime missing"))?;
        if let Some(binding) = runtime
            .accepted_installed_binding(owner, lease)
            .map_err(internal)?
        {
            self.installation_owner_current(owner)?;
            return Ok(binding);
        }
        let admission = self.accepted_publication_admission(owner, lease).await?;
        let installation = runtime
            .accepted_original_installation(owner, lease, admission.permit)
            .await
            .map_err(internal)?;
        let flags = runtime
            .accepted_installation_flags(owner, lease)
            .map_err(internal)?;
        let stat = if self.cfg.virtualize_metadata {
            Some(
                runtime
                    .accepted_installation_stat(owner, lease)
                    .map_err(internal)?,
            )
        } else {
            None
        };
        let binding =
            self.publish_original_installation(owner, &admission, &installation, flags, stat)?;
        runtime
            .retain_accepted_installed_binding(owner, lease, binding)
            .map_err(internal)?;
        Ok(binding)
    }
}
