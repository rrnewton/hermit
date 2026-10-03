//! Original invocation admission and backend result observation share the
//! existing engine and native runtime. No Guest future owns early selection.
use super::*;
use crate::network_replay::original_connect::Admission;
use crate::resources::ExternalOpId;

pub(super) enum EpollCtlTurn {
    Observe,
    Retire,
}

impl GlobalState {
    pub(crate) fn replay_sendto_read_limit<T>(
        &self, tid: Tid, state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission, requested: usize,
    ) -> Result<usize, NetworkRpcError> {
        if self.shared_mm_attempts_active() {
            return self.shared_replay_sendto_read_limit(tid, state, read, requested);
        }
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner { thread: state.dettid, mm: state.mm_id };
        let runtime = self.network_runtime.as_ref().ok_or_else(|| NetworkRpcError::internal("Sendto Replay runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        scheduler.foreground_native_observation(owner, &root).map_err(|e| fail(&e))?;
        if !self.cfg.sequentialize_threads || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            || tid.as_raw() != root.association().process() || state.detpid != Some(owner.thread)
            || !root.matches_memory(&state.memory_metadata) || !root.matches_metadata(&state.file_metadata)
        {
            return Err(NetworkRpcError::internal("Sendto Replay lost exact Normal owner/MM"));
        }
        let mut metadata = state.file_metadata.lock().unwrap();
        let engine = self.network_engine.as_ref().ok_or_else(|| NetworkRpcError::internal("Sendto Replay engine absent"))?.lock().unwrap();
        let open_file = engine.validate_replay_transmit_read(owner, read, &state.file_metadata, &mut metadata).map_err(|e| fail(&e))?;
        engine.transmit_stream_read_limit(open_file, requested).map_err(|e| fail(&e))
    }

    async fn begin_foreground_original_send(
        &self, owner: NetworkStreamOwner,
        arguments: crate::network_replay::original_connect::Arguments,
    ) -> Result<(Admission, std::os::fd::OwnedFd), NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        if !self.cfg.sequentialize_threads {
            return Err(NetworkRpcError::internal("original Sendto requires strict foreground scheduling"));
        }
        let runtime = self.network_runtime.as_ref().ok_or_else(|| NetworkRpcError::internal("Sendto runtime absent"))?;
        let shared = self.network_engine.as_ref().ok_or_else(|| NetworkRpcError::internal("Sendto engine absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let actual = root.metadata().map_err(|e| fail(&e))?;
        let epoch = {
            let scheduler = self.sched.lock().unwrap();
            scheduler.foreground_native_observation(owner, &root).map_err(|e| fail(&e))?.epoch()
        };
        let task = runtime.prepare_native_capture_task(owner).map_err(|e| fail(&e))?;
        let joined = runtime.join_foreground_prefix(root.clone()).await.map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler.foreground_native_observation(owner, &root).map_err(|e| fail(&e))?;
        if grant.epoch() != epoch || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm) {
            return Err(NetworkRpcError::internal("Sendto entry changed original Normal grant/MM"));
        }
        let metadata = actual.lock().unwrap();
        let mut engine = shared.lock().unwrap();
        engine.validate_fd_metadata(owner, arguments.files, &actual, &metadata).map_err(|e| fail(&e))?;
        engine.validate_native_sendto(&arguments).map_err(|e| fail(&e))?;
        let admission = runtime.with_foreground_prefix(&joined, |prefix| {
            let admission = engine.begin_original_connect(owner, arguments).map_err(std::io::Error::other)?;
            let stamped = (|| {
                let mut attempt = engine.begin_native_entry_stamp(owner, admission.call)?;
                let _retained = attempt.retain_unsubmitted_recovery(&joined)?;
                engine.stamp_native_receive_entry(attempt, prefix, &grant, self.global_time.lock().unwrap().as_nanos())
            })();
            if let Err(error) = stamped {
                // This synchronous prefix borrow excludes all native workers
                // and Calls. No pin or provider submission has occurred.
                engine.abort_original_before_provider(owner, &admission).map_err(std::io::Error::other)?;
                return Err(std::io::Error::other(error));
            }
            Ok(admission)
        }).map_err(|e| fail(&e))?;
        Ok((admission, task))
    }

    pub(crate) fn publish_foreground_native_sent<T>(
        &self, tid: Tid, state: &crate::tool_local::ThreadState<T>, admission: &Admission,
    ) -> Result<(), NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner { thread: state.dettid, mm: state.mm_id };
        let runtime = self.network_runtime.as_ref().ok_or_else(|| NetworkRpcError::internal("Sendto runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler.foreground_native_observation(owner, &root).map_err(|e| fail(&e))?;
        if !self.cfg.sequentialize_threads || tid.as_raw() != root.association().process()
            || state.detpid != Some(owner.thread) || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        { return Err(NetworkRpcError::internal("Sendto publication changed actual root/MM")); }
        let mut engine = self.network_engine.as_ref().ok_or_else(|| NetworkRpcError::internal("Sendto engine absent"))?.lock().unwrap();
        runtime.publish_native_sent(&mut engine, admission, &grant, self.global_time.lock().unwrap().as_nanos()).map_err(|e| fail(&e))
    }

    /// Scheduler authority for the two sides of the existing local syscall
    /// continuation. It is neither native completion nor selected-file proof.
    pub(super) fn require_original_epoll_ctl_turn(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        phase: EpollCtlTurn,
    ) -> Result<(), NetworkRpcError> {
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed()
            || sched.thread_is_logically_killed(owner.thread)
            || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "epoll control lost its exact registered owner",
            ));
        }
        if self.cfg.sequentialize_threads {
            let allowed = match phase {
                EpollCtlTurn::Observe => sched.original_external_io_grant_matches(owner, operation),
                EpollCtlTurn::Retire => sched.ordinary_fd_observation(owner).is_ok(),
            };
            if !allowed {
                return Err(NetworkRpcError::internal(
                    "epoll control changed its observation/continuation turn",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn fail_original_connect(
        &self,
        tid: Tid,
        owner: NetworkStreamOwner,
        local: &crate::network_replay::original_connect::Local,
        detail: &str,
    ) {
        // Failure is not a physical observation. In particular it cannot mark
        // final_wait, release the table permit or manufacture a missing return.
        let process = self.sched.lock().unwrap().registered_process(owner.thread);
        tracing::error!(%tid, ?owner, ?local, %detail, "original Connect failed with retained custody");
        self.report_backend_failure(reverie::BackendFailure {
            pid: process.map_or(tid, |process| Tid::from_raw(process.as_raw())),
            tid,
            phase: "original Connect failed with retained custody before actual final wait",
        });
    }
    pub(super) async fn recv_original_connect(
        &self,
        owner: NetworkStreamOwner,
        request: NetworkRequest,
    ) -> GlobalResponse {
        let result = self.original_connect_request(owner, request).await;
        match result {
            Ok(reply) => GlobalResponse::Network(Ok(reply)),
            Err(error) => GlobalResponse::Network(Err(error)),
        }
    }
    async fn original_connect_request(
        &self,
        owner: NetworkStreamOwner,
        request: NetworkRequest,
    ) -> Result<NetworkReply, NetworkRpcError> {
        if let NetworkRequest::NativeBeginForegroundEpollCtl { arguments } = &request {
            return self
                .begin_foreground_epoll_ctl(owner, arguments.clone())
                .await;
        }
        if let NetworkRequest::NativeForegroundEpollCtlReturned { admission } = &request {
            self.check_foreground_epoll_return(owner, admission)?;
            return Ok(NetworkReply::Unit);
        }
        let fail = |error: NetworkReplayError| NetworkRpcError::internal(error.to_string());
        let physical = |error: std::io::Error| NetworkRpcError::internal(error.to_string());
        let runtime = self.network_runtime.as_ref();
        let publication = self
            .native_capture_recovery()
            .ok_or_else(|| NetworkRpcError::internal("original Connect requires shared engine"))?;
        if let NetworkRequest::SelectRecordedOriginalFile { admission }
        | NetworkRequest::CompleteRecordedOriginalFile { admission, .. }
        | NetworkRequest::CompleteRecordedReadInterruption { admission }
        | NetworkRequest::CompleteEmulatedRead { admission, .. } = &request
        {
            let sched = self.sched.lock().unwrap();
            if sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || self
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .get(&owner.thread)
                    .is_some_and(|mm| *mm != owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "recorded file delegate lost its exact owner",
                ));
            }
            let mut engine = publication.engine().lock().unwrap();
            match &request {
                NetworkRequest::SelectRecordedOriginalFile { .. } => engine
                    .select_recorded_original_file(owner, admission)
                    .map_err(fail)?,
                NetworkRequest::CompleteRecordedOriginalFile { returned, .. } => engine
                    .finish_recorded_original_file(owner, admission, *returned)
                    .map_err(fail)?,
                NetworkRequest::CompleteRecordedReadInterruption { .. } => engine
                    .finish_recorded_read_interruption(owner, admission)
                    .map_err(fail)?,
                NetworkRequest::CompleteEmulatedRead { returned, .. } => engine
                    .finish_emulated_read(owner, admission, *returned)
                    .map_err(fail)?,
                _ => unreachable!(),
            }
            let retired = engine.take_lifetime_retired_ports();
            drop(engine);
            drop(sched);
            self.release_lifetime_ports(retired);
            self.network_stream_changed.notify_waiters();
            return Ok(NetworkReply::Unit);
        }
        let emulated = matches!(&request, NetworkRequest::BeginEmulatedReadFromRead { .. });
        let begin = match &request {
            NetworkRequest::NativeBeginOriginalSocket { arguments, .. }
            | NetworkRequest::NativeBeginOriginalAllocator { arguments } => {
                Some((arguments, None, crate::OriginalFileExecution::Native))
            }
            NetworkRequest::NativeBeginOriginalConnect { arguments } => {
                Some((arguments, None, crate::OriginalFileExecution::Native))
            }
            NetworkRequest::BeginRecordedOriginalFile { arguments } => {
                Some((arguments, None, crate::OriginalFileExecution::Recorded))
            }
            NetworkRequest::BeginOriginalFileFromRead {
                arguments,
                read,
                source,
            } => Some((arguments, Some(read), *source)),
            NetworkRequest::BeginEmulatedReadFromRead { arguments, read } => {
                Some((arguments, Some(read), crate::OriginalFileExecution::Native))
            }
            NetworkRequest::NativeBeginOriginalExternalFromRead { arguments, read } => {
                Some((arguments, Some(read), crate::OriginalFileExecution::Native))
            }
            _ => None,
        };
        if let Some((arguments, read, source)) = begin {
            let recorded = source == crate::OriginalFileExecution::Recorded;
            if arguments.kind == crate::network_replay::original_connect::Kind::Sendto {
                if recorded || emulated || read.is_some() {
                    return Err(NetworkRpcError::internal("Sendto cannot borrow a recorded/external producer"));
                }
                let (admission, task) = self.begin_foreground_original_send(owner, arguments.clone()).await?;
                runtime.expect("Sendto entry required runtime")
                    .prepare_original_connect(owner, admission.clone(), task, publication).await.map_err(physical)?;
                return Ok(NetworkReply::OriginalConnectAdmission(admission));
            }
            if !recorded
                && !emulated
                && arguments.kind == crate::network_replay::original_connect::Kind::Connect
                && publication
                    .engine()
                    .lock()
                    .unwrap()
                    .native_receive_version()
            {
                let read = match &request {
                    NetworkRequest::NativeBeginOriginalExternalFromRead { read, .. } => {
                        read.clone()
                    }
                    _ => {
                        return Err(NetworkRpcError::internal(
                            "V4 Connect entry requires its actual selected reader",
                        ));
                    }
                };
                let (admission, task) = match self
                    .begin_original_native_entry_from_read(owner, arguments.clone(), read)
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(failure) => {
                        return Err(self
                            .cleanup_receive_admission_failure(failure)
                            .await
                            .into_rpc_error());
                    }
                };
                runtime
                    .expect("prepared native entry owns runtime")
                    .prepare_original_connect(owner, admission.clone(), task, publication)
                    .await
                    .map_err(physical)?;
                return Ok(NetworkReply::OriginalConnectAdmission(admission));
            }
            let (admission, task) = loop {
                let changed = self.network_stream_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let (attempt, failed) = {
                    let sched = self.sched.lock().unwrap();
                    if sched.backend_failed()
                        || sched.thread_is_logically_killed(owner.thread)
                        || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                        || self
                            .registered_exec_mms
                            .lock()
                            .unwrap()
                            .get(&owner.thread)
                            .is_some_and(|mm| *mm != owner.mm)
                    {
                        return Err(NetworkRpcError::internal(
                            "original Connect admission owner is no longer current",
                        ));
                    }
                    // Retain actual PIDFD_THREAD authority before task registry
                    // withdrawal can occur. Numeric task IDs never reach capture.
                    let task = if recorded || emulated {
                        None
                    } else {
                        Some(
                            runtime
                                .ok_or_else(|| {
                                    NetworkRpcError::internal(
                                        "original Connect requires authenticated runtime",
                                    )
                                })?
                                .prepare_native_capture_task(owner)
                                .map_err(physical)?,
                        )
                    };
                    if let NetworkRequest::NativeBeginOriginalExternalFromRead { read, .. } =
                        &request
                    {
                        let granted = if self.cfg.sequentialize_threads {
                            read.external_grant == Some(arguments.operation)
                                && sched.original_transfer_grant_matches(
                                    owner,
                                    arguments.operation,
                                    arguments.kind,
                                )
                        } else {
                            read.external_grant.is_none()
                        };
                        if !granted {
                            return Err(NetworkRpcError::internal(
                                "original transfer lost its actual scheduling grant",
                            ));
                        }
                    }
                    if arguments.kind == crate::network_replay::original_connect::Kind::Read
                        && self.cfg.sequentialize_threads
                    {
                        let read = read.ok_or_else(|| {
                            NetworkRpcError::internal(
                                "Read requires its exact classified admission",
                            )
                        })?;
                        let valid = match read.external_grant {
                            Some(operation) => {
                                operation == arguments.operation
                                    && sched.original_fd_grant_matches(owner, operation)
                            }
                            None => sched.ordinary_fd_observation(owner).is_ok(),
                        };
                        if !valid {
                            return Err(NetworkRpcError::internal(
                                "Read lost its actual foreground/external grant",
                            ));
                        }
                    }
                    if arguments.kind == crate::network_replay::original_connect::Kind::Openat
                        && self.cfg.sequentialize_threads
                        && !sched.original_external_io_grant_matches(owner, arguments.operation)
                    {
                        return Err(NetworkRpcError::internal(
                            "Openat lost its actual external-IO grant",
                        ));
                    }
                    if arguments.kind == crate::network_replay::original_connect::Kind::EpollCtl
                        && self.cfg.sequentialize_threads
                        && !sched.original_external_io_grant_matches(owner, arguments.operation)
                    {
                        return Err(NetworkRpcError::internal(
                            "epoll control lost its actual external-IO grant",
                        ));
                    }
                    let mut engine = publication.engine().lock().unwrap();
                    let attempt = if let Some(read) = read {
                        if emulated {
                            engine.begin_emulated_read_from_read(
                                owner,
                                arguments.clone(),
                                read.clone(),
                            )
                        } else if matches!(
                            &request,
                            NetworkRequest::NativeBeginOriginalExternalFromRead { .. }
                        ) {
                            engine.begin_original_external_from_read(
                                owner,
                                arguments.clone(),
                                read.clone(),
                            )
                        } else {
                            engine.begin_original_file_from_read(
                                owner,
                                arguments.clone(),
                                read.clone(),
                                source,
                            )
                        }
                    } else if matches!(
                        &request,
                        NetworkRequest::NativeBeginOriginalAllocator { .. }
                    ) {
                        engine.begin_original_allocator(owner, arguments.clone())
                    } else if let NetworkRequest::NativeBeginOriginalSocket { mutation, .. } =
                        &request
                    {
                        engine.begin_original_socket(owner, arguments.clone(), mutation.clone())
                    } else if arguments.kind
                        == crate::network_replay::original_connect::Kind::EpollCtl
                        && matches!(&request, NetworkRequest::NativeBeginOriginalConnect { .. })
                    {
                        engine.begin_original_epoll_ctl(owner, arguments.clone())
                    } else if recorded {
                        engine.begin_recorded_original_file(owner, arguments.clone())
                    } else {
                        engine.begin_original_connect(owner, arguments.clone())
                    }
                    .map(|admission| (admission, task));
                    (attempt, sched.backend_failure_waiter())
                };
                match attempt {
                    Ok(pair) => break pair,
                    Err(NetworkReplayError::StreamOperationBusy(_)) => {
                        // Background original calls can hold the same table
                        // in either scheduling mode. Wait for that admitted
                        // publication, not a host-ready selection or its final
                        // syscall return. All locks above have been released.
                        tokio::select! {_=changed=>{},_=failed=>return Err(NetworkRpcError::internal("backend failed during original Connect admission"))}
                    }
                    Err(error) => return Err(fail(error)),
                }
            };
            // The first poll starts the same retained NativeWorker before it
            // awaits anything; worker/Driver custody survives a lost RPC reply.
            if !recorded && !emulated {
                runtime
                    .expect("native task already required runtime")
                    .prepare_original_connect(owner, admission.clone(), task.unwrap(), publication)
                    .await
                    .map_err(physical)?;
            }
            return Ok(NetworkReply::OriginalConnectAdmission(admission));
        }
        let admission: &Admission = match &request {
            NetworkRequest::NativeSubmitOriginalConnect { admission }
            | NetworkRequest::NativeOriginalConnectOutcome { admission }
            | NetworkRequest::NativePublishOriginalSocket { admission, .. }
            | NetworkRequest::NativeObserveOriginalOpenat { admission }
            | NetworkRequest::NativePublishOriginalOpenat { admission }
            | NetworkRequest::NativePublishOriginalEpoll { admission }
            | NetworkRequest::NativeRetireOriginalConnect { admission }
            | NetworkRequest::NativeRetireInterruptedRead { admission } => admission,
            _ => unreachable!("original Connect dispatch family"),
        };
        if admission.arguments.kind == crate::network_replay::original_connect::Kind::EpollCtl {
            match &request {
                NetworkRequest::NativeOriginalConnectOutcome { .. } => self
                    .require_original_epoll_ctl_turn(
                        owner,
                        admission.arguments.operation,
                        EpollCtlTurn::Observe,
                    )?,
                NetworkRequest::NativeRetireOriginalConnect { .. } => self
                    .require_original_epoll_ctl_turn(
                        owner,
                        admission.arguments.operation,
                        EpollCtlTurn::Retire,
                    )?,
                _ => {}
            }
        }
        let runtime = runtime.ok_or_else(|| {
            NetworkRpcError::internal("original Connect requires authenticated runtime")
        })?;
        match request {
            NetworkRequest::NativeSubmitOriginalConnect { .. } => {
                let sched = self.sched.lock().unwrap();
                if sched.backend_failed()
                    || sched.thread_is_logically_killed(owner.thread)
                    || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                {
                    return Err(NetworkRpcError::internal(
                        "original Connect lost owner before invocation admission",
                    ));
                }
                if admission.arguments.kind == crate::network_replay::original_connect::Kind::Sendto {
                    let root = runtime.foreground_root(owner).map_err(physical)?;
                    let grant = sched.foreground_native_observation(owner, &root).map_err(|e| NetworkRpcError::internal(e.to_string()))?;
                    publication.engine().lock().unwrap().validate_native_foreground_call(admission.call, &grant, self.global_time.lock().unwrap().as_nanos()).map_err(fail)?;
                }
                publication
                    .engine()
                    .lock()
                    .unwrap()
                    .original_connect_invoked(owner, admission)
                    .map_err(fail)?;
                Ok(NetworkReply::Unit)
            }
            NetworkRequest::NativeOriginalConnectOutcome { .. } => runtime
                .original_connect_outcome(owner, admission, &publication)
                .await
                .map(|outcome| NetworkReply::OriginalConnectOutcome(Box::new(outcome)))
                .map_err(physical),
            NetworkRequest::NativeRetireInterruptedRead { .. } => {
                runtime
                    .retire_interrupted_read(owner, admission, &publication)
                    .await
                    .map_err(physical)?;
                Ok(NetworkReply::Unit)
            }
            NetworkRequest::NativePublishOriginalEpoll { .. } => self
                .publish_original_epoll(owner, admission)
                .await
                .map(NetworkReply::OriginalEpollInstallation),
            NetworkRequest::NativeObserveOriginalOpenat { .. } => {
                {
                    let sched = self.sched.lock().unwrap();
                    if sched.backend_failed()
                        || sched.thread_is_logically_killed(owner.thread)
                        || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                        || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                            != Some(&owner.mm)
                        || (self.cfg.sequentialize_threads
                            && !sched.original_external_io_grant_matches(
                                owner,
                                admission.arguments.operation,
                            ))
                    {
                        return Err(NetworkRpcError::internal(
                            "Openat observation lost its exact external owner",
                        ));
                    }
                }
                runtime
                    .observe_original_openat(owner, admission)
                    .await
                    .map_err(physical)?;
                Ok(NetworkReply::Unit)
            }
            NetworkRequest::NativePublishOriginalOpenat { .. } => self
                .publish_original_openat(owner, admission)
                .await
                .map(NetworkReply::OriginalOpenatInstallation),
            NetworkRequest::NativePublishOriginalSocket {
                stat,
                ref enrollment,
                ..
            } => self
                .publish_original_socket(owner, admission, stat, enrollment.clone())
                .await
                .map(NetworkReply::OriginalSocketInstallation),
            NetworkRequest::NativeRetireOriginalConnect { .. } => {
                runtime
                    .retire_original_connect(owner, admission, &publication)
                    .map_err(physical)?;
                runtime
                    .checkpoint_completed_original_history(owner, &publication)
                    .await
                    .map_err(physical)?;
                Ok(NetworkReply::Unit)
            }
            _ => unreachable!("original Connect dispatch family"),
        }
    }
    /// Actual final wait consumes the same backend-owned ThreadState. A new
    /// task reusing the numeric TID has no copy of this local admission/call.
    pub(crate) fn observe_original_connect_terminal<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &crate::tool_local::ThreadState<T>,
    ) {
        let Some(local) = &state.original_connect else {
            return;
        };
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let observed = (|| {
            if tid.as_raw() != owner.thread.as_raw()
                || local.arguments.operation
                    != crate::resources::ExternalOpId::new(owner.thread, state.stats.syscall_count)
            {
                return Err("original final wait changed exact backend task/invocation");
            }
            self.network_engine
                .as_ref()
                .ok_or("original terminal engine missing")?
                .lock()
                .unwrap()
                .original_connect_final_wait(owner, local)
                .map_err(|_| "original final wait changed retained call")
        })();
        // A positive terminal cleanup can never authorize trace publication.
        let phase = match observed {
            Ok(false) => return,
            Ok(true) => "original Connect task terminated before complete semantic capture",
            Err(phase) => phase,
        };
        self.report_backend_failure(reverie::BackendFailure {
            pid: Tid::from_raw(process.as_raw()),
            tid,
            phase,
        });
        self.network_stream_changed.notify_waiters();
    }
    /// The backend invokes this synchronously at a real syscall-return stop.
    /// A synthesized interrupted-injection result does not enter this hook.
    pub(crate) fn observe_original_connect<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &mut crate::tool_local::ThreadState<T>,
        nr: reverie::syscalls::Sysno,
        args: reverie::syscalls::SyscallArgs,
        event: reverie::InjectedSyscallEvent,
    ) {
        let Some(local) = state.original_connect.clone() else {
            return;
        };
        if nr != local.arguments.kind.syscall() {
            return;
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let raw = [
            args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
        ];
        let observed = (|| {
            if tid.as_raw() != owner.thread.as_raw()
                || !local.invoked
                || raw != local.raw_arguments
                || local.arguments.operation
                    != crate::resources::ExternalOpId::new(owner.thread, state.stats.syscall_count)
            {
                return Err("original Connect return mismatched actual caller/invocation");
            }
            let admission = local
                .admission
                .as_ref()
                .ok_or("original Connect return has no retained admission")?;
            if admission.arguments != local.arguments {
                return Err("original Connect local admission changed");
            }
            if event == reverie::InjectedSyscallEvent::Prepared {
                if local.arguments.kind == crate::network_replay::original_connect::Kind::Sendto {
                    let runtime = self.network_runtime.as_ref().ok_or("Sendto runtime absent")?;
                    let root = runtime.foreground_root(owner).map_err(|_| "Sendto root absent")?;
                    let sched = self.sched.lock().unwrap();
                    let grant = sched.foreground_native_observation(owner, &root).map_err(|_| "Sendto lost original Normal grant")?;
                    if !root.matches_memory(&state.memory_metadata) || !root.matches_metadata(&state.file_metadata)
                        || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
                        || raw[4] != 0 || raw[5] != 0
                    { return Err("Sendto Prepared changed actual metadata/destination"); }
                    self.network_engine.as_ref().ok_or("Sendto engine absent")?.lock().unwrap()
                        .validate_native_foreground_call(admission.call, &grant, self.global_time.lock().unwrap().as_nanos())
                        .map_err(|_| "Sendto Prepared changed entry frontier")?;
                }
                self.prepare_foreground_epoll_observation(owner, admission, state)
                    .map_err(|_| "foreground ctl changed actual grant/memory/FD preparation")?;
                if local.arguments.kind.allocator()
                    || local.arguments.kind
                        == crate::network_replay::original_connect::Kind::EpollCtl
                {
                    self.network_runtime
                        .as_ref()
                        .ok_or("Socket preparation has no runtime")?
                        .bind_original_installation_metadata(
                            owner,
                            admission,
                            state.file_metadata.clone(),
                        )
                        .map_err(|_| "Socket preparation changed retained metadata custody")?;
                }
                if local.arguments.kind == crate::network_replay::original_connect::Kind::EpollCtl {
                    let metadata = state
                        .file_metadata
                        .lock()
                        .map_err(|_| "epoll metadata mutex poisoned")?;
                    self.network_engine
                        .as_ref()
                        .ok_or("epoll Prepared engine missing")?
                        .lock()
                        .unwrap()
                        .original_epoll_ctl_metadata(
                            owner,
                            admission,
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(|_| "epoll Prepared changed actual metadata owner")?;
                }
                if local.arguments.kind == crate::network_replay::original_connect::Kind::Close {
                    self.network_runtime
                        .as_ref()
                        .ok_or("close preparation has no runtime")?
                        .bind_original_close_metadata(owner, admission, state.file_metadata.clone())
                        .map_err(|_| "close preparation changed retained metadata custody")?;
                }
                if matches!(
                    local.arguments.kind,
                    crate::network_replay::original_connect::Kind::File(_)
                        | crate::network_replay::original_connect::Kind::Read
                ) {
                    // Match the actual local table under uninterrupted admission,
                    // then validate the existing engine permit in metadata->engine
                    // order. There is no user-memory access or await in this join.
                    let observed = state
                        .file_metadata
                        .lock()
                        .map_err(|_| "file metadata mutex poisoned")?
                        .observe_original_file_metadata(admission)?;
                    {
                        let mut engine = self
                            .network_engine
                            .as_ref()
                            .ok_or("file preparation engine missing")?
                            .lock()
                            .unwrap();
                        engine
                            .validate_original_file_prepared(owner, admission)
                            .map_err(|_| "file preparation lost exact publication authority")?;
                        if local.arguments.kind
                            == crate::network_replay::original_connect::Kind::Read
                        {
                            engine
                                .original_read_metadata_prepared(owner, admission, &observed)
                                .map_err(|_| "read preparation changed actual metadata custody")?;
                        }
                    }
                    if state
                        .original_file_metadata
                        .as_ref()
                        .is_some_and(|prior| prior != &observed)
                    {
                        return Err("prepared file metadata changed retained invocation");
                    }
                    state.original_file_metadata = Some(observed);
                }
                return Ok(None);
            }
            if matches!(
                event,
                reverie::InjectedSyscallEvent::Entered
                    | reverie::InjectedSyscallEvent::InterruptedBeforeEntry
            ) {
                let mut engine = self
                    .network_engine
                    .as_ref()
                    .ok_or("Read observation engine missing")?
                    .lock()
                    .unwrap();
                if event == reverie::InjectedSyscallEvent::Entered
                    && admission.arguments.kind
                        != crate::network_replay::original_connect::Kind::Read
                {
                    engine
                        .original_non_read_entered(owner, admission)
                        .map_err(|_| "non-Read entry changed actual invocation custody")?;
                    return Ok(None);
                }
                if event == reverie::InjectedSyscallEvent::Entered {
                    engine.original_read_entered(owner, admission)
                } else {
                    engine.original_read_interrupted_before_entry(owner, admission)
                }
                .map_err(|_| "Read boundary changed actual invocation custody")?;
                return Ok(None);
            }
            let reverie::InjectedSyscallEvent::Returned(returned) = event else {
                return Err("original invocation unexpectedly reported child creation");
            };
            self.network_engine
                .as_ref()
                .ok_or("original Connect engine missing")?
                .lock()
                .unwrap()
                .original_connect_returned(owner, admission, returned)
                .map_err(|_| "original Connect raw return changed retained invocation")?;
            Ok(Some(returned))
        })();
        if observed.is_ok() && event == reverie::InjectedSyscallEvent::InterruptedBeforeEntry {
            state.original_connect.as_mut().unwrap().invoked = false;
        }
        if let Ok(Some(returned)) = observed {
            state.original_connect.as_mut().unwrap().returned = Some(returned);
        }
        if let Err(phase) = observed {
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase,
            });
        }
        self.network_stream_changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use reverie::ExitStatus;
    use reverie::GlobalTool;
    use reverie::InjectedSyscallEvent;
    use reverie::Tool;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::network_replay::original_connect::Arguments;
    use crate::network_replay::original_connect::Local;
    use crate::network_replay::original_connect::Pin;
    use crate::types::DetTid;
    use crate::types::DetTime;
    use crate::types::MmId;
    #[tokio::test]
    async fn original_backend_observer_accepts_only_exact_invoked_local_return() {
        for variant in 0..4 {
            let mut config = Config {
                sequentialize_threads: true,
                epoch_explicit: true,
                ..Config::default()
            };
            config.network_trace.policy = NetworkPolicy::Record;
            let state = GlobalState::initialize(&config, false);
            let thread = DetTid::from_raw(61);
            let owner = NetworkStreamOwner {
                thread,
                mm: MmId::initial(thread),
            };
            let tool: crate::Detcore = crate::Detcore::new(Tid::from_raw(61), &config);
            let mut local = tool.init_thread_state(Tid::from_raw(61), None);
            local.stats.syscall_count = 10;
            let admission = {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                engine.fd_table_fixture_enable();
                let files = engine.fd_publication_fixture_register(owner, None);
                let admission = engine
                    .begin_original_connect(
                        owner,
                        Arguments {
                            kind: crate::network_replay::original_connect::Kind::Connect,
                            operation: crate::resources::ExternalOpId::new(thread, 10),
                            files,
                            binding: None,
                            fd: 7,
                            address: 0x2000,
                            length: 16,
                            original_count: 0,
                        },
                    )
                    .unwrap();
                engine
                    .original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                engine
                    .original_connect_prepared(owner, &admission, Pin::Empty, 17)
                    .unwrap();
                engine.original_connect_invoked(owner, &admission).unwrap();
                admission
            };
            local.dettid = thread;
            local.mm_id = owner.mm;
            local.original_connect = Some(Local {
                arguments: admission.arguments.clone(),
                raw_arguments: [7, 0x2000, 16, 0, 0, 0],
                admission: Some(admission.clone()),
                invoked: variant != 1,
                returned: None,
            });
            // Until the actual hook arrives, absence stays unknown, with no
            // synthesized injection error installed as a native result.
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .original_connect_result(owner, &admission)
                    .unwrap(),
                None
            );
            let args = SyscallArgs::new(7, if variant == 2 { 0x3000 } else { 0x2000 }, 16, 0, 0, 0);
            state.observe_original_connect(
                Tid::from_raw(if variant == 3 { 62 } else { 61 }),
                thread,
                &mut local,
                Sysno::connect,
                args,
                InjectedSyscallEvent::Returned(-i64::from(libc::EBADF)),
            );
            let expected = (variant == 0).then_some(-i64::from(libc::EBADF));
            assert_eq!(local.original_connect.as_ref().unwrap().returned, expected);
            assert_eq!(
                state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .original_connect_result(owner, &admission)
                    .unwrap(),
                expected
            );
            assert_eq!(state.sched.lock().unwrap().backend_failed(), variant != 0);
        }
    }
    #[tokio::test]
    async fn original_consumed_rpc_before_delayed_begin_closes_admission_without_a_physical_receipt()
     {
        let mut config = Config {
            sequentialize_threads: false,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let state = GlobalState::initialize(&config, false);
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let arguments = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            let files = engine.fd_publication_fixture_register(owner, None);
            Arguments {
                kind: crate::network_replay::original_connect::Kind::Connect,
                operation: crate::resources::ExternalOpId::new(thread, 10),
                files,
                binding: None,
                fd: 7,
                address: 0x2000,
                length: 16,
                original_count: 0,
            }
        };
        let local = Local {
            arguments: arguments.clone(),
            raw_arguments: [7, 0x2000, 16, 0, 0, 0],
            admission: None,
            invoked: false,
            returned: None,
        };
        let reply = state
            .receive_rpc(
                Tid::from_raw(61),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::OriginalConnectOwnerGone(local),
                ),
            )
            .await;
        assert_eq!(
            reply,
            (None, GlobalResponse::OriginalConnectOwnerGone(true))
        );
        // There has been neither Begin admission nor the ordinary owner-gone
        // RPC. Its late Begin still cannot capture, arm, or own a table permit.
        assert!(
            matches!(state.network_engine.as_ref().unwrap().lock().unwrap()
            .begin_original_connect(owner,arguments),Err(NetworkReplayError::StreamOwnerGone(actual)) if actual==owner)
        );
    }

    #[tokio::test]
    async fn original_actual_consumed_rpc_handles_lost_begin_reply_before_ordinary_owner_gone() {
        let mut config = Config {
            sequentialize_threads: false,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let state = GlobalState::initialize(&config, false);
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let admission = {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            let files = engine.fd_publication_fixture_register(owner, None);
            engine
                .begin_original_connect(
                    owner,
                    Arguments {
                        kind: crate::network_replay::original_connect::Kind::Connect,
                        operation: crate::resources::ExternalOpId::new(thread, 10),
                        files,
                        binding: None,
                        fd: 7,
                        address: 0x2000,
                        length: 16,
                        original_count: 0,
                    },
                )
                .unwrap()
        };
        let local = Local {
            arguments: admission.arguments.clone(),
            raw_arguments: [7, 0x2000, 16, 0, 0, 0],
            admission: None,
            invoked: false,
            returned: None,
        };
        let reply = state
            .receive_rpc(
                Tid::from_raw(61),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::OriginalConnectOwnerGone(local),
                ),
            )
            .await;
        assert_eq!(
            reply,
            (None, GlobalResponse::OriginalConnectOwnerGone(true))
        );
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_eq!(
            engine
                .original_connect_cancellation(owner, &admission)
                .unwrap(),
            (true, true)
        );
        assert!(engine.original_connect_invoked(owner, &admission).is_err());
        // No provider submission has occurred: the capture worker can settle
        // only after known no-acquisition or actual pin close, not from the RPC.
        assert!(
            engine
                .original_connect_result(owner, &admission)
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn actual_backend_final_wait_marks_incomplete_connect_failed_without_a_return() {
        for (wrong_tid, lost_begin_reply) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut config = Config {
                sequentialize_threads: false,
                epoch_explicit: true,
                ..Config::default()
            };
            config.network_trace.policy = NetworkPolicy::Record;
            let state = GlobalState::initialize(&config, false);
            let thread = DetTid::from_raw(61);
            let owner = NetworkStreamOwner {
                thread,
                mm: MmId::initial(thread),
            };
            let tool: crate::Detcore = crate::Detcore::new(Tid::from_raw(61), &config);
            let mut local = tool.init_thread_state(Tid::from_raw(61), None);
            local.dettid = thread;
            local.mm_id = owner.mm;
            local.stats.syscall_count = 10;
            let admission = {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                engine.fd_table_fixture_enable();
                let files = engine.fd_publication_fixture_register(owner, None);
                let admission = engine
                    .begin_original_connect(
                        owner,
                        Arguments {
                            kind: crate::network_replay::original_connect::Kind::Connect,
                            operation: crate::resources::ExternalOpId::new(thread, 10),
                            files,
                            binding: None,
                            fd: 7,
                            address: 0x2000,
                            length: 16,
                            original_count: 0,
                        },
                    )
                    .unwrap();
                engine
                    .original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                engine
                    .original_connect_prepared(owner, &admission, Pin::Empty, 17)
                    .unwrap();
                if !lost_begin_reply {
                    engine.original_connect_invoked(owner, &admission).unwrap();
                }
                admission
            };
            local.original_connect = Some(Local {
                arguments: admission.arguments.clone(),
                raw_arguments: [7, 0x2000, 16, 0, 0, 0],
                admission: (!lost_begin_reply).then_some(admission.clone()),
                invoked: !lost_begin_reply,
                returned: None,
            });
            // The real failure RPC acknowledges sticky failure without holding
            // a server future open. It leaves native result and terminal
            // authority absent until the actual backend callback below.
            let request = state.receive_rpc(
                Tid::from_raw(61),
                (
                    DetTime::new(&config),
                    owner.mm,
                    GlobalRequest::Network(NetworkRequest::NativeOriginalConnectFailed {
                        local: local.original_connect.clone().unwrap(),
                        detail: "synthetic interruption / failed original collection".into(),
                    }),
                ),
            );
            assert_eq!(
                request.await,
                (None, GlobalResponse::Network(Ok(NetworkReply::Unit)))
            );
            assert!(state.sched.lock().unwrap().backend_failed());
            {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                assert!(
                    !engine
                        .original_connect_task_terminal(owner, &admission)
                        .unwrap()
                );
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                assert!(matches!(
                    engine.acquire_fd_publication(owner, admission.arguments.files),
                    Err(NetworkReplayError::StreamOperationBusy(_))
                ));
            }
            if lost_begin_reply {
                let mut changed = local.original_connect.clone().unwrap();
                changed.arguments.length += 1;
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                assert!(engine.original_connect_final_wait(owner, &changed).is_err());
                assert!(
                    !engine
                        .original_connect_task_terminal(owner, &admission)
                        .unwrap()
                );
            }
            tool.on_backend_thread_terminal(
                Tid::from_raw(if wrong_tid { 62 } else { 61 }),
                &state,
                &mut local,
                ExitStatus::Exited(0),
            );
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            assert_eq!(
                engine
                    .original_connect_task_terminal(owner, &admission)
                    .unwrap(),
                !wrong_tid
            );
            assert_eq!(
                engine.original_connect_result(owner, &admission).unwrap(),
                None
            );
            assert_eq!(local.original_connect.as_ref().unwrap().returned, None);
            assert!(state.sched.lock().unwrap().backend_failed());
            // Lost Begin reply means provider READY already exists but the
            // Guest never received Admission or entered inject. Only authentic
            // final wait selects dead-command retirement; consumed cancellation
            // alone must not route the reaped task through READY disarm.
            engine
                .original_connect_consumed(owner, local.original_connect.as_ref().unwrap())
                .unwrap();
            assert_eq!(
                engine
                    .original_connect_task_terminal(owner, &admission)
                    .unwrap(),
                !wrong_tid
            );
            if wrong_tid {
                assert!(
                    engine
                        .original_connect_dead_retired(owner, &admission, 17)
                        .is_err()
                );
            } else {
                assert!(
                    engine
                        .original_connect_dead_retired(owner, &admission, 18)
                        .is_err()
                );
                engine
                    .original_connect_dead_retired(owner, &admission, 17)
                    .unwrap();
                assert_eq!(
                    engine.original_connect_result(owner, &admission).unwrap(),
                    None
                );
                assert!(engine.finish_original_connect(owner, &admission).is_err());
                engine
                    .original_connect_pin_released(owner, &admission)
                    .unwrap();
                engine.finish_original_connect(owner, &admission).unwrap();
            }
        }
    }
}
#[cfg(test)]
mod recorded_file_adapter_tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use detcore_model::network_trace::NetworkTraceV2;
    use reverie::GlobalRPC;
    use reverie::Guest;
    use reverie::Never;
    use reverie::Stack;
    use reverie::TimerSchedule;
    use reverie::Tool;
    use reverie::syscalls::Addr;
    use reverie::syscalls::AddrMut;
    use reverie::syscalls::Fcntl;
    use reverie::syscalls::FcntlCmd;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallInfo;
    use serde::Deserialize;
    use serde::Serialize;

    use super::*;
    use crate::network_replay::NetworkFdPublicationBatch;
    use crate::network_replay::NetworkFdPublicationEntry;
    use crate::types::NetworkFdSlot;
    use crate::types::NetworkFdSlotReplacement;

    // Explicit semantic-delegate input. The separate CLI controls execute the
    // actual Recorder/Replayer stream; this component exercises its production
    // Detcore adapter and Global RPC ownership, with injection forbidden.
    #[derive(Debug, Default, Serialize, Deserialize)]
    struct RecordedFlags;
    #[reverie::tool]
    impl Tool for RecordedFlags {
        type GlobalState = GlobalState;
        type ThreadState = usize;
    }
    impl crate::RecordOrReplay for RecordedFlags {
        async fn invoke_original_read<G: Guest<Self>>(
            &self,
            _guest: &mut G,
            _call: reverie::syscalls::Read,
        ) -> Result<reverie::InjectedReadResult, Error> {
            panic!("the recorded F_GETFL fixture must not invoke a native Read")
        }
        fn original_file_execution(&self, _: Syscall) -> crate::OriginalFileExecution {
            crate::OriginalFileExecution::Recorded
        }
        async fn consume_recorded_original_file<G: Guest<Self>>(
            &self,
            guest: &mut G,
            call: Syscall,
        ) -> Result<i64, Error> {
                assert!(matches!(call,Syscall::Fcntl(c) if matches!(c.cmd(),FcntlCmd::F_GETFL)));
                *guest.thread_state_mut() += 1;
                Ok(0)
        }
    }
    struct NoStack;
    struct NoStackGuard;
    impl Drop for NoStackGuard {
        fn drop(&mut self) {}
    }
    impl Stack for NoStack {
        type StackGuard = NoStackGuard;
        fn size(&self) -> usize {
            panic!("unexpected stack")
        }
        fn capacity(&self) -> usize {
            panic!("unexpected stack")
        }
        fn push<'s, T>(&mut self, _: T) -> Addr<'s, T> {
            panic!("unexpected stack")
        }
        fn reserve<'s, T>(&mut self) -> AddrMut<'s, T> {
            panic!("unexpected stack")
        }
        fn commit(self) -> Result<NoStackGuard, Errno> {
            panic!("unexpected stack")
        }
    }
    struct AdapterGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<usize>,
        requests: Mutex<Vec<GlobalRequest>>,
        pause_after: Option<usize>,
        rpc_count: AtomicUsize,
    }
    #[reverie::tool]
    impl GlobalRPC<GlobalState> for AdapterGuest<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            assert!(
                matches!(
                    &request.2,
                    GlobalRequest::Network(
                        NetworkRequest::BeginRecordedOriginalFile { .. }
                            | NetworkRequest::SelectRecordedOriginalFile { .. }
                            | NetworkRequest::CompleteRecordedOriginalFile { .. }
                    )
                ),
                "F_GETFL introduced a scheduling/resource or native request: {:?}",
                request.2
            );
            self.requests.lock().unwrap().push(request.2.clone());
            let result = self
                .global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), request)
                .await;
            assert!(
                result.0.is_none(),
                "admission must not manufacture virtual time"
            );
            let count = self.rpc_count.fetch_add(1, Ordering::SeqCst) + 1;
            if self.pause_after == Some(count) {
                futures::future::pending::<()>().await;
            }
            result
        }
        fn config(&self) -> &Config {
            self.config
        }
    }
    #[reverie::tool]
    impl Guest<crate::Detcore<RecordedFlags>> for AdapterGuest<'_> {
        type Memory = LocalMemory;
        type Stack = NoStack;
        fn tid(&self) -> Tid {
            Tid::from_raw(self.thread.dettid.as_raw())
        }
        fn pid(&self) -> Tid {
            self.tid()
        }
        fn ppid(&self) -> Option<Tid> {
            None
        }
        fn memory(&self) -> LocalMemory {
            panic!("F_GETFL must not touch guest memory")
        }
        fn thread_state(&self) -> &crate::ThreadState<usize> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<usize> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected registers")
        }
        async fn stack(&mut self) -> NoStack {
            panic!("unexpected stack")
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemonization")
        }
        async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
            panic!("recorded value became native injection")
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("recorded value became tail injection")
        }
        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("unexpected clock")
        }
    }
    fn fixture(
        sequential: bool,
    ) -> (
        Config,
        GlobalState,
        crate::Detcore<RecordedFlags>,
        crate::ThreadState<usize>,
        OpenFileId,
    ) {
        let mut config = Config {
            sequentialize_threads: sequential,
            epoch_explicit: true,
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Replay;
        let trace = NetworkTraceV2 {
            epoch: config.epoch,
            channels: vec![],
            inputs: vec![],
            outputs: vec![],
        };
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        config.network_trace_input = Some(bytes);
        let state = GlobalState::initialize(&config, false);
        let tid = Tid::from_raw(71);
        let tool: crate::Detcore<RecordedFlags> = crate::Detcore::new(tid, &config);
        let mut local = tool.init_thread_state(tid, None);
        local.stats.syscall_count = 10;
        local
            .add_fd(
                7,
                nix::fcntl::OFlag::O_NONBLOCK,
                crate::fd::FdType::Socket,
                None,
            )
            .unwrap();
        let owner = NetworkStreamOwner {
            thread: local.dettid,
            mm: local.mm_id,
        };
        let binding = local.descriptor_binding(7).unwrap();
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(owner.thread, owner.thread, true);
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        // Explicit lower-layer initial logical slot input. This does not create
        // a native capability/census/command or assert this numeric FD is open.
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        assert!(!engine.fd_table_capability());
        let files = engine.fd_publication_fixture_register(owner, None);
        assert_eq!(files, binding.slot.files);
        let replacement = NetworkFdSlotReplacement {
            files,
            installation_generation: binding.generation,
            before: None,
            after: Some(NetworkFdSlot {
                binding,
                cloexec: false,
            }),
        };
        let effect = engine.fd_publication_fixture_effect(owner, replacement);
        let batch = NetworkFdPublicationBatch {
            files,
            sequence: 1,
            previous_generation: 0,
            through_generation: binding.generation,
            entries: vec![NetworkFdPublicationEntry {
                replacement,
                effect,
            }],
        };
        let permit = engine.acquire_fd_publication(owner, files).unwrap().permit;
        engine
            .publish_fd_publication(owner, permit, &batch)
            .unwrap();
        engine
            .acknowledge_fd_publication(owner, permit, &batch)
            .unwrap();
        drop(engine);
        // Pre-register the unchanged clock without advancing it, so the test
        // compares the adapter's own scheduling/time delta rather than setup.
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            local.thread_logical_time.as_nanos(),
            local.thread_logical_time.inherited_nanos(),
        );
        (config, state, tool, local, binding.open_file)
    }
    fn scheduling(state: &GlobalState) -> String {
        let scheduler = state.sched.lock().unwrap();
        format!(
            "{:?}",
            (
                &scheduler.turn,
                &scheduler.run_queue,
                &scheduler.next_turns,
                &scheduler.bg_action_pool,
                &scheduler.committed_time,
                &scheduler.blocked,
                &scheduler.per_thread_syscalls
            )
        )
    }
    #[tokio::test]
    async fn recorded_file_adapter_preserves_strict_and_no_seq_turns_time_and_exact_logical_flags()
    {
        for sequential in [true, false] {
            let (config, state, tool, thread, file) = fixture(sequential);
            let before_sched = scheduling(&state);
            let before_time = state.global_time.lock().unwrap().as_nanos();
            let mut guest = AdapterGuest {
                global: &state,
                config: &config,
                thread,
                requests: Mutex::new(vec![]),
                pause_after: None,
                rpc_count: AtomicUsize::new(0),
            };
            let before_local = guest.thread.thread_logical_time.as_nanos();
            let actual = tool
                .network_original_get_flags(
                    &mut guest,
                    Fcntl::new().with_fd(7).with_cmd(FcntlCmd::F_GETFL),
                )
                .await
                .unwrap();
            assert_eq!(actual, i64::from(libc::O_NONBLOCK));
            assert_eq!(*guest.thread.as_ref(), 1);
            assert_eq!(guest.thread.stats.syscall_count, 10);
            assert_eq!(guest.thread.thread_logical_time.as_nanos(), before_local);
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_time);
            assert_eq!(scheduling(&state), before_sched);
            assert_eq!(guest.rpc_count.load(Ordering::SeqCst), 3);
            assert!(guest.thread.original_connect.is_none());
            assert!(guest.thread.original_file_metadata.is_none());
            {
                let engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                assert_eq!(engine.native_capture_fixture_counts(file), (0, 0, 0, 0));
                assert!(!engine.fd_table_capability());
                assert!(
                    engine.finish_fd_mutations().is_err(),
                    "task ownership is distinct from operation cleanup"
                );
            }
            let owner = NetworkStreamOwner {
                thread: guest.thread.dettid,
                mm: guest.thread.mm_id,
            };
            let response = state
                .receive_rpc(
                    Tid::from_raw(owner.thread.as_raw()),
                    (
                        guest.thread.thread_logical_time.clone(),
                        owner.mm,
                        GlobalRequest::NetworkOwnerGone,
                    ),
                )
                .await;
            assert_eq!(response.1, GlobalResponse::NetworkOwnerGone);
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .finish_fd_mutations()
                .unwrap();
        }
    }
    #[tokio::test]
    async fn recorded_file_adapter_lost_begin_select_and_completion_replies_keep_consuming_custody()
    {
        for sequential in [true, false] {
            for after_rpc in 1..=3 {
                let (config, state, tool, thread, file) = fixture(sequential);
                let mut guest = AdapterGuest {
                    global: &state,
                    config: &config,
                    thread,
                    requests: Mutex::new(vec![]),
                    pause_after: Some(after_rpc),
                    rpc_count: AtomicUsize::new(0),
                };
                let mut future = Box::pin(tool.network_original_get_flags(
                    &mut guest,
                    Fcntl::new().with_fd(7).with_cmd(FcntlCmd::F_GETFL),
                ));
                let waker = futures::task::noop_waker();
                assert!(
                    future
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(&waker))
                        .is_pending()
                );
                drop(future);
                assert_eq!(guest.rpc_count.load(Ordering::SeqCst), after_rpc);
                let local = guest.thread.original_connect.clone().unwrap();
                assert!(!local.invoked && local.returned.is_none());
                assert_eq!(*guest.thread.as_ref(), usize::from(after_rpc == 3));
                let owner = NetworkStreamOwner {
                    thread: guest.thread.dettid,
                    mm: guest.thread.mm_id,
                };
                if after_rpc < 3 {
                    assert_eq!(
                        state
                            .network_engine
                            .as_ref()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .native_capture_fixture_counts(file),
                        (1, 1, usize::from(after_rpc == 1), 1)
                    );
                }
                let response = state
                    .receive_rpc(
                        Tid::from_raw(owner.thread.as_raw()),
                        (
                            guest.thread.thread_logical_time.clone(),
                            owner.mm,
                            GlobalRequest::OriginalConnectOwnerGone(local),
                        ),
                    )
                    .await;
                assert_eq!(response.1, GlobalResponse::OriginalConnectOwnerGone(true));
                assert_eq!(
                    state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .native_capture_fixture_counts(file),
                    (0, 0, 0, 0)
                );
                assert!(!state.sched.lock().unwrap().backend_failed());
            }
        }
    }
}
