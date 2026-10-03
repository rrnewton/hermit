//! Local joins for a trace-completed, physically offline Connect.
//! AUTONOMOUS-BOT-IMPLEMENTED
//! TODO-HUMAN-REVIEW(PR-3464): https://github.com/rrnewton/hermit/pull/3464
use super::*;
use crate::network_replay::NetworkFdReadAdmission;
use crate::resources::ExternalOpId;
use crate::types::OpenFileId;

impl GlobalState {
    pub(crate) fn begin_replay_connect<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        operation: ExternalOpId,
        read: NetworkFdReadAdmission,
        open_file: OpenFileId,
    ) -> Result<NetworkStreamCall, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| fail(&"Replay Connect runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let mut scheduler = self.sched.lock().unwrap();
        scheduler
            .check_replay_connect_start(owner, operation)
            .map_err(|e| fail(&e))?;
        // This proves only the selected external grant and unchanged root. It
        // does not claim a native Connect entry, selection, or return in Replay.
        scheduler
            .native_capture_entry_observation(owner, operation, &root)
            .map_err(|e| fail(&e))?;
        if !self.cfg.sequentialize_threads
            || self.cfg.network_trace.policy != NetworkPolicy::Replay
            || tid.as_raw() != root.association().process()
            || state.detpid != Some(owner.thread)
            || operation != ExternalOpId::new(owner.thread, state.stats.syscall_count)
            || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(fail(
                &"Replay Connect changed actual selected task/MM/operation",
            ));
        }
        let _memory = state.memory_metadata.lock().unwrap();
        let metadata = state.file_metadata.lock().unwrap();
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| fail(&"Replay Connect engine absent"))?
            .lock()
            .unwrap();
        if !engine.native_receive_version()
            || engine.mode() != crate::network_replay::NetworkEngineMode::Replay
        {
            return Err(fail(&"Replay Connect requires the existing V4 engine"));
        }
        engine
            .validate_fd_metadata(
                owner,
                read.publication.permit.files,
                &state.file_metadata,
                &metadata,
            )
            .map_err(|e| fail(&e))?;
        if engine.uses_shared_mm_attempts() {
            let initial = scheduler.shared_initial_projection(&root).map_err(|e| fail(&e))?;
            let grant = scheduler.native_capture_entry_observation(owner, operation, &root)
                .map_err(|e| fail(&e))?;
            engine.bind_shared_initial_replay_origin(root.clone(), initial, &grant)
                .map_err(|e| fail(&e))?;
        }
        let call = engine
            .begin_replay_connect(owner, operation, read, open_file)
            .map_err(|e| fail(&e))?;
        // A failure after transfer leaves the engine Call unresolved; it never
        // falls back to generic input consumption or reconstructs the reader.
        scheduler
            .enroll_replay_connect(owner, operation, call.id)
            .map_err(|e| fail(&e))?;
        Ok(call)
    }

    pub(crate) fn complete_replay_connect<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        operation: ExternalOpId,
        call: NetworkStreamCallId,
    ) -> Result<NetworkConnectionResultV2, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| fail(&"Replay Connect runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let mut scheduler = self.sched.lock().unwrap();
        scheduler
            .check_replay_connect_completion(owner, operation, call)
            .map_err(|e| fail(&e))?;
        scheduler
            .foreground_native_observation(owner, &root)
            .map_err(|e| fail(&e))?;
        if !self.cfg.sequentialize_threads
            || self.cfg.network_trace.policy != NetworkPolicy::Replay
            || tid.as_raw() != root.association().process()
            || state.detpid != Some(owner.thread)
            || operation != ExternalOpId::new(owner.thread, state.stats.syscall_count)
            || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(fail(
                &"Replay Connect changed actual completion task/MM/operation",
            ));
        }
        let _memory = state.memory_metadata.lock().unwrap();
        let _metadata = state.file_metadata.lock().unwrap();
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| fail(&"Replay Connect engine absent"))?
            .lock()
            .unwrap();
        let now = self.global_time.lock().unwrap().as_nanos();
        if now != scheduler.committed_time {
            return Err(fail(
                &"Replay Connect completion has unpublished logical time",
            ));
        }
        let result = engine
            .complete_replay_connect(owner, operation, call, now)
            .map_err(|e| fail(&e))?;
        scheduler.finish_replay_connect(owner);
        // The logical Call can be the final owner after an FD replacement.
        // Drain the lifetime's retired-port effects in this same transaction.
        self.release_lifetime_ports(engine.take_lifetime_retired_ports());
        drop(engine);
        drop(_metadata);
        drop(_memory);
        drop(scheduler);
        self.network_stream_changed.notify_waiters();
        Ok(result)
    }
}
