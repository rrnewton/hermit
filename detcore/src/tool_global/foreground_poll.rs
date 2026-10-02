//! Local poll entry borrows the actual Normal grant and existing FD-reader custody.
//! It issues no guest-memory read/copy authority and creates no scheduler request.
use super::foreground_store::ReceiveAdmissionCustody;
use super::foreground_store::ReceiveAdmissionFailure;
use super::foreground_store::ReceiveAdmissionStage;
use super::foreground_store::RetainedReceiveAdmission;
use super::*;

impl GlobalState {
    pub(crate) async fn begin_native_poll_call<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<crate::network_replay::NetworkStreamCall, Box<ReceiveAdmissionFailure>> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let mut cleanup_diagnostic = None;
        let prepared = async {
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            if !self.cfg.sequentialize_threads {
                return Err(NetworkRpcError::internal(
                    "private receive requires strict single-root poll admission",
                ));
            }
            let owner = NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            };
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("private entry runtime absent"))?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("private entry engine absent"))?;
            let recovery = self
                .native_capture_recovery()
                .ok_or_else(|| NetworkRpcError::internal("private entry lost recovery owner"))?;
            let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
            let check_local = || {
                if tid.as_raw() != root.association().process()
                    || state.detpid != Some(owner.thread)
                    || !root.matches_memory(&state.memory_metadata)
                    || !root.matches_metadata(&state.file_metadata)
                    || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                {
                    return Err(NetworkRpcError::internal(
                        "private entry changed actual local task/MM metadata",
                    ));
                }
                Ok(())
            };
            let epoch = {
                let scheduler = self.sched.lock().unwrap();
                let grant = scheduler
                    .foreground_native_observation(owner, &root)
                    .map_err(|e| fail(&e))?;
                check_local()?;
                grant.epoch()
            };
            let joined = runtime
                .join_foreground_prefix(root.clone())
                .await
                .map_err(|e| fail(&e))?;
            let task = runtime
                .prepare_native_capture_task(owner)
                .map_err(|e| fail(&e))?;
            let binding = read
                .binding
                .ok_or_else(|| NetworkRpcError::internal("private receive lost admitted socket"))?;
            let control = read.control.ok_or_else(|| {
                NetworkRpcError::internal("private receive lost admitted control")
            })?;
            let (call, identity) = {
                let scheduler = self.sched.lock().unwrap();
                let grant = scheduler
                    .foreground_native_observation(owner, &root)
                    .map_err(|e| fail(&e))?;
                check_local()?;
                if grant.epoch() != epoch {
                    return Err(NetworkRpcError::internal(
                        "private entry crossed foreground grant",
                    ));
                }
                let _memory = state.memory_metadata.lock().unwrap();
                let metadata = state.file_metadata.lock().unwrap();
                let mut engine = engine.lock().unwrap();
                if !engine.native_receive_version()
                    || engine.mode() != crate::network_replay::NetworkEngineMode::Record
                {
                    return Err(NetworkRpcError::internal(
                        "private native entry requires explicit V4 recorder",
                    ));
                }
                let identity = engine
                    .native_stream_capture_identity(owner, &read, &state.file_metadata, &metadata)
                    .map_err(|e| fail(&e))?
                    .ok_or_else(|| {
                        NetworkRpcError::internal("private entry lacks original file identity")
                    })?;
                let call = runtime
                    .with_foreground_prefix(&joined, |prefix| {
                        // No native admission can interleave with transfer/stamp/abort.
                        let call = engine
                            .begin_native_stream_call_from_read(owner, read.clone())
                            .map_err(std::io::Error::other)?;
                        custody = ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                            call: call.id,
                            stage: ReceiveAdmissionStage::Unsubmitted(joined.clone()),
                        });
                        let mut attempt = engine
                            .begin_native_entry_stamp(owner, call.id)
                            .map_err(std::io::Error::other)?;
                        let unsubmitted = attempt
                            .retain_unsubmitted_recovery(&joined)
                            .map_err(std::io::Error::other)?;
                        let now = self.global_time.lock().unwrap().as_nanos();
                        if let Err(primary) =
                            engine.stamp_native_receive_entry(attempt, prefix, &grant, now)
                        {
                            match engine.cancel_unsubmitted_native_entry(&unsubmitted, prefix) {
                                Ok(()) => custody = ReceiveAdmissionCustody::Released,
                                Err(secondary) => cleanup_diagnostic = Some(fail(&secondary)),
                            }
                            return Err(std::io::Error::other(primary));
                        }
                        Ok(call)
                    })
                    .map_err(|e| fail(&e))?;
                (call, identity)
            };
            Ok((
                root, epoch, call, control, binding, task, identity, recovery,
            ))
        }
        .await;
        let (root, epoch, call, control, binding, task, identity, recovery) = match prepared {
            Ok(prepared) => prepared,
            Err(primary) => {
                if matches!(custody, ReceiveAdmissionCustody::Released) {
                    self.finish_local_receive_release();
                }
                let mut failure = self.receive_admission_failure(owner, primary, custody);
                failure.cleanup = cleanup_diagnostic;
                return Err(
                    if matches!(failure.custody, ReceiveAdmissionCustody::RetainedCall(_)) {
                        self.cleanup_receive_admission_failure(failure).await
                    } else {
                        failure
                    },
                );
            }
        };
        let runtime = self.network_runtime.as_ref().expect("prepared runtime");
        let captured = runtime
            .capture_native_stream(owner, call.id, binding.slot.fd, task, identity, recovery)
            .await;
        match captured {
            Ok(captured) => {
                self.complete_private_receive_capture(
                    tid,
                    state,
                    &root,
                    epoch,
                    (call, control),
                    captured,
                )
                .await
            }
            Err(primary) => Err(self
                .fail_private_receive_capture(
                    owner,
                    call,
                    NetworkRpcError::internal(primary.to_string()),
                )
                .await),
        }
    }
    pub(crate) fn begin_replay_poll_call<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<crate::network_replay::NetworkStreamCall, Box<ReceiveAdmissionFailure>> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let result = (|| -> Result<_, NetworkRpcError> {
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            if !self.cfg.sequentialize_threads {
                return Err(NetworkRpcError::internal(
                    "Replay receive requires strict single-root poll admission",
                ));
            }
            let owner = NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            };
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("Replay local runtime absent"))?;
            let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
            let scheduler = self.sched.lock().unwrap();
            scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            if tid.as_raw() != root.association().process()
                || state.detpid != Some(owner.thread)
                || !root.matches_memory(&state.memory_metadata)
                || !root.matches_metadata(&state.file_metadata)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "Replay entry changed actual local task/MM metadata",
                ));
            }
            let _memory = state.memory_metadata.lock().unwrap();
            let metadata = state.file_metadata.lock().unwrap();
            let mut engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("Replay engine absent"))?
                .lock()
                .unwrap();
            if !engine.native_receive_version()
                || engine.mode() != crate::network_replay::NetworkEngineMode::Replay
            {
                return Err(NetworkRpcError::internal(
                    "offline entry requires the shared V4 Replay engine",
                ));
            }
            engine
                .validate_fd_metadata(
                    owner,
                    read.binding
                        .ok_or_else(|| {
                            NetworkRpcError::internal("Replay entry requires its admitted socket")
                        })?
                        .slot
                        .files,
                    &state.file_metadata,
                    &metadata,
                )
                .map_err(|e| fail(&e))?;
            let control = read
                .control
                .ok_or_else(|| NetworkRpcError::internal("Replay entry lost admitted control"))?;
            let call = engine
                .begin_native_stream_call_from_read(owner, read)
                .map_err(|e| fail(&e))?;
            custody = ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                call: call.id,
                stage: ReceiveAdmissionStage::Replay(control),
            });
            engine
                .finish_socket_control(
                    owner,
                    control,
                    crate::network_replay::NetworkSocketControlFinish::Unchanged,
                )
                .map_err(|e| fail(&e))?;
            Ok(call)
        })();
        match result {
            Ok(call) => {
                self.finish_local_receive_release();
                Ok(call)
            }
            Err(primary) => Err(self.receive_admission_failure(owner, primary, custody)),
        }
    }

    pub(crate) fn native_poll_enabled(&self) -> bool {
        self.network_engine
            .as_ref()
            .is_some_and(|engine| engine.lock().unwrap().native_receive_version())
    }

    pub(crate) fn finish_foreground_poll<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        call: crate::network_replay::NetworkStreamCallId,
        lease: Option<NetworkStreamLeaseId>,
    ) -> Result<i16, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        if !self.cfg.sequentialize_threads {
            return Err(NetworkRpcError::internal(
                "poll requires strict single-root admission",
            ));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("poll runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .map_err(|e| fail(&e))?;
        if tid.as_raw() != root.association().process()
            || state.detpid != Some(owner.thread)
            || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "poll changed actual local task/MM metadata",
            ));
        }
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("poll engine absent"))?
            .lock()
            .unwrap();
        let now = self.global_time.lock().unwrap().as_nanos();
        match lease {
            Some(lease) => {
                let raw = engine
                    .publish_native_poll(owner, call, lease, &grant, now)
                    .map_err(|e| fail(&e))?;
                runtime
                    .finish_native_stream_lease(owner, lease)
                    .map_err(|e| fail(&e))?;
                Ok(raw)
            }
            None => engine
                .replay_native_poll(owner, call, now)
                .map_err(|e| fail(&e)),
        }
    }
}
