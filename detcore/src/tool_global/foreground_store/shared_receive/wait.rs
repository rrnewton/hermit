//! Original Call/target/deadline persist through actual shared Receive waits.
use super::*;
use crate::network_replay::shared_waits::SharedRecordProbe;

impl GlobalState {
    pub(crate) async fn resume_shared_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
    ) -> Result<(), NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared receive continuation lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared receive continuation lost engine"))?;
        let epoch =
            self.with_shared_receive_wait_context(tid, state, invocation, |_, grant, _| {
                Ok(grant.epoch())
            })?;
        let prefix = runtime
            .join_shared_foreground_prefix(
                invocation.policy.root.clone(),
                engine,
                Some(invocation.call.id),
            )
            .await
            .map_err(internal)?;
        self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
            if grant.epoch() != epoch {
                return Err(internal("shared receive resume crossed continuation grant"));
            }
            let now = self.global_time.lock().unwrap().as_nanos();
            runtime
                .with_shared_attempt_prefix(&prefix, engine, |engine, admission| {
                    engine
                        .resume_shared_wait(invocation.call.id, grant, admission, now)
                        .map(|_| ())
                        .map_err(std::io::Error::other)
                })
                .map_err(internal)
        })
    }

    pub(crate) async fn suspend_shared_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
        origin: Option<&Arc<SharedRecordProbe>>,
    ) -> Result<Vec<crate::resources::NetworkWaitKind>, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared receive suspension lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared receive suspension lost engine"))?;
        let (epoch, completed) =
            self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
                let now = self.global_time.lock().unwrap().as_nanos();
                let completed = match origin {
                    Some(origin) => runtime
                        .with_shared_record_pending(origin, engine, |engine, proof| {
                            engine
                                .complete_shared_record_pending(proof, grant, now)
                                .map_err(std::io::Error::other)
                        })
                        .map_err(internal)?,
                    None => engine
                        .complete_shared_replay_wait_observation(invocation.call.id, grant, now)
                        .map_err(internal)?,
                };
                Ok((grant.epoch(), completed))
            })?;
        // Both engine completion and native lease retirement are already
        // positive. Join the resulting prefix before moving Active→Suspended.
        let prefix = runtime
            .join_shared_foreground_prefix(
                invocation.policy.root.clone(),
                engine,
                Some(invocation.call.id),
            )
            .await
            .map_err(internal)?;
        self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
            if grant.epoch() != epoch {
                return Err(internal("shared receive suspension crossed original grant"));
            }
            runtime
                .with_shared_attempt_prefix(&prefix, engine, |engine, admission| {
                    engine
                        .suspend_shared_wait(completed, grant, admission)
                        .map_err(std::io::Error::other)?;
                    engine
                        .shared_wait_interests(grant.owner(), invocation.call.id)
                        .map_err(std::io::Error::other)
                })
                .map_err(internal)
        })
    }
}

impl GlobalState {
    pub(super) fn with_shared_receive_wait_context<T, R>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
        action: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedMmForegroundObservation<'_>,
            &SharedForegroundLineage<'_>,
        ) -> Result<R, NetworkRpcError>,
    ) -> Result<R, NetworkRpcError> {
        let policy = &invocation.policy;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared receive lost runtime"))?;
        let mode = match self.cfg.network_trace.policy {
            NetworkPolicy::Record => crate::network_replay::NetworkEngineMode::Record,
            NetworkPolicy::Replay => crate::network_replay::NetworkEngineMode::Replay,
            _ => return Err(internal("shared Receive wait changed closed policy")),
        };
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(policy.owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(policy.owner, lineage)
                        .map_err(internal)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    if !policy.matches_shared(
                        grant.owner(),
                        invocation.call.id,
                        invocation.call.open_file,
                    ) || !Arc::ptr_eq(policy.root(), grant.root())
                    {
                        return Err(internal("shared receive lost original task/policy/root"));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| internal("shared receive lost engine"))?
                        .lock()
                        .unwrap();
                    if !engine.uses_shared_mm_attempts() || engine.mode() != mode {
                        return Err(internal("shared receive changed engine policy"));
                    }
                    engine
                        .validate_fd_metadata(
                            policy.owner,
                            policy.root.files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(internal)?;
                    action(&mut engine, &grant, lineage)
                })())
            })
            .map_err(internal)?
    }
}
