//! Native memory provenance and narrow original ctl admission.
use reverie::InjectedSyscallEvent as Event;

use super::*;

impl GlobalState {
    pub(crate) fn revoke_original_foreground<T>(&self, state: &crate::tool_local::ThreadState<T>) {
        state
            .memory_metadata
            .lock()
            .unwrap()
            .invalidate_original_arena();
        if let Some(runtime) = &self.network_runtime {
            runtime.revoke_foreground_lineage();
        }
    }
    pub(crate) fn observe_original_memory<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &mut crate::tool_local::ThreadState<T>,
        nr: reverie::syscalls::Sysno,
        args: reverie::syscalls::SyscallArgs,
        event: Event,
    ) {
        let invalidating = crate::memory::invalidates_original_arena(nr);
        if !invalidating {
            return;
        }
        if !state
            .file_metadata
            .lock()
            .unwrap()
            .network_lifetime_tracking()
        {
            state
                .memory_metadata
                .lock()
                .unwrap()
                .invalidate_original_arena();
            return;
        }
        let Some(runtime) = &self.network_runtime else {
            state
                .memory_metadata
                .lock()
                .unwrap()
                .invalidate_original_arena();
            return;
        };
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let terminal_query = event == Event::Prepared
            && nr == reverie::syscalls::Sysno::ioctl
            && runtime.foreground_root(owner).is_ok_and(|root| {
                root.matches_metadata(&state.file_metadata)
                    && crate::memory::preserves_foreground_terminal_query(
                        nr,
                        args,
                        &root,
                        &state.file_metadata.lock().unwrap(),
                    )
            });
        if event == Event::Prepared
            && crate::memory::changes_foreground_lineage(nr)
            && !terminal_query
        {
            state
                .memory_metadata
                .lock()
                .unwrap()
                .invalidate_original_arena();
            runtime.revoke_foreground_lineage();
            return;
        }
        // Optional narrow capability: ordinary memory behavior still works
        // when this run has no positively established initial-root lineage.
        let Ok(root) = runtime.foreground_root(owner) else {
            state
                .memory_metadata
                .lock()
                .unwrap()
                .invalidate_original_arena();
            return;
        };
        let observed = (|| {
            let sched = self.sched.lock().unwrap();
            if tid.as_raw() != owner.thread.as_raw()
                || sched.backend_failed()
                || sched.thread_is_logically_killed(owner.thread)
                || !sched.rpc_incarnation_matches(owner.thread, owner.mm)
                || sched.registered_process(owner.thread) != Some(process)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
                || !root.matches_memory(&state.memory_metadata)
            {
                return Err("native mmap observation changed registered task/MM metadata");
            }
            let mut memory = state.memory_metadata.lock().unwrap();
            if terminal_query {
                memory.observe_original_memory_operation(&root, nr, args, event, true)
            } else {
                memory.observe_original_arena(&root, nr, args, event)
            }
        })();
        if let Err(detail) = observed {
            state
                .memory_metadata
                .lock()
                .unwrap()
                .invalidate_original_arena();
            runtime.revoke_foreground_lineage();
            tracing::error!(%tid, %detail, "native event-arena observation refused");
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase: "native event-arena observation changed exact original custody",
            });
        }
    }
}

impl GlobalState {
    pub(super) async fn begin_foreground_epoll_ctl(
        &self,
        owner: NetworkStreamOwner,
        arguments: crate::network_replay::original_connect::Arguments,
    ) -> Result<NetworkReply, NetworkRpcError> {
        let fail = |error: &dyn std::fmt::Display| NetworkRpcError::internal(error.to_string());
        if !self.cfg.sequentialize_threads {
            return Err(NetworkRpcError::internal(
                "foreground ctl requires the actual strict scheduler",
            ));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let actual = root.metadata().map_err(|e| fail(&e))?;
        let memory = root.memory().map_err(|e| fail(&e))?;
        let publication = self
            .native_capture_recovery()
            .ok_or_else(|| NetworkRpcError::internal("foreground engine absent"))?;
        {
            let sched = self.sched.lock().unwrap();
            sched
                .foreground_epoll_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            let local = actual.lock().unwrap();
            publication
                .engine()
                .lock()
                .unwrap()
                .foreground_epoll_preflight(owner, &arguments, &root, &actual, &local)
                .map_err(|e| fail(&e))?;
        }
        // Only proven retired/no-effect Calls can leave execution tails here.
        // Join those exact handles with no scheduler/metadata/engine guard.
        let joined = runtime
            .join_foreground_prefix(root.clone())
            .await
            .map_err(|e| fail(&e))?;
        let task = runtime
            .prepare_native_capture_task(owner)
            .map_err(|e| fail(&e))?;
        runtime
            .validate_foreground_prefix(&joined)
            .map_err(|e| fail(&e))?;
        let admission = {
            let sched = self.sched.lock().unwrap();
            let grant = sched
                .foreground_epoll_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            if self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm) {
                return Err(NetworkRpcError::internal(
                    "foreground ctl changed exact registered MM",
                ));
            }
            let local = actual.lock().unwrap();
            let memory = memory.lock().unwrap();
            publication
                .engine()
                .lock()
                .unwrap()
                .begin_foreground_epoll_ctl(
                    owner,
                    arguments, (&actual,
                    &local),
                    &memory,
                    joined,
                    grant.epoch(),
                )
                .map_err(|e| fail(&e))?
        };
        runtime
            .prepare_original_connect(owner, admission.clone(), task, publication)
            .await
            .map_err(|e| fail(&e))?;
        Ok(NetworkReply::OriginalConnectAdmission(admission))
    }
    pub(super) fn prepare_foreground_epoll_observation<T>(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
        state: &crate::tool_local::ThreadState<T>,
    ) -> Result<(), NetworkRpcError> {
        let Some(engine) = &self.network_engine else {
            return Ok(());
        };
        let root = engine
            .lock()
            .unwrap()
            .foreground_epoll_root(owner, admission)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        let Some(root) = root else { return Ok(()) };
        let sched = self.sched.lock().unwrap();
        let grant = sched
            .foreground_epoll_observation(owner, &root)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        if !root.matches_memory(&state.memory_metadata) {
            return Err(NetworkRpcError::internal(
                "foreground ctl changed actual memory Arc",
            ));
        }
        let local = state.file_metadata.lock().unwrap();
        let memory = state.memory_metadata.lock().unwrap();
        let mut engine = engine.lock().unwrap();
        engine
            .validate_fd_metadata(
                owner,
                admission.arguments.files,
                &state.file_metadata,
                &local,
            )
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        engine
            .prepare_foreground_epoll_ctl(owner, admission, &local, &memory, grant.epoch())
            .map_err(|e| NetworkRpcError::internal(e.to_string()))
    }
    pub(super) fn check_foreground_epoll_return(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> Result<(), NetworkRpcError> {
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground engine absent"))?;
        let root = engine
            .lock()
            .unwrap()
            .foreground_epoll_root(owner, admission)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?
            .ok_or_else(|| NetworkRpcError::internal("foreground capability absent"))?;
        let sched = self.sched.lock().unwrap();
        let grant = sched
            .foreground_epoll_observation(owner, &root)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        engine
            .lock()
            .unwrap()
            .foreground_epoll_returned(owner, admission, grant.epoch())
            .map_err(|e| NetworkRpcError::internal(e.to_string()))
    }
}
