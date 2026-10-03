//! Actual full-mask Record observation; no timer or copied argument is a source.
use super::*;
use crate::network_replay::NetworkStreamPhysicalEffect;
use crate::network_replay::shared_waits::SharedRecordPollPublication;

impl GlobalState {
    pub(super) async fn observe_shared_record_poll<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedPollInvocation,
    ) -> Result<Arc<SharedRecordPollPublication>, NetworkRpcError> {
        if self.cfg.network_trace.policy != NetworkPolicy::Record {
            return Err(internal("actual shared Poll scan requires Record"));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll observation lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Poll observation lost engine"))?;
        let epoch =
            self.with_shared_poll_context(tid, state, invocation, |_, grant, _| Ok(grant.epoch()))?;
        let prefix = runtime
            .join_shared_foreground_prefix(
                invocation.captured.custody.root.clone(),
                engine,
                Some(invocation.call.id),
            )
            .await
            .map_err(internal)?;
        let origin =
            self.with_shared_poll_context(tid, state, invocation, |engine, grant, _| {
                if grant.epoch() != epoch {
                    return Err(internal("shared Poll probe crossed original grant"));
                }
                let now = self.global_time.lock().unwrap().as_nanos();
                let origin = runtime
                    .with_shared_attempt_prefix(&prefix, engine, |engine, admission| {
                        engine
                            .begin_shared_record_probe(invocation.call.id, grant, admission, now)
                            .map_err(std::io::Error::other)
                    })
                    .map_err(internal)?;
                // The prefix borrower has released worker/Call locks. The same
                // engine transaction binds its exact newly acquired native lease.
                runtime
                    .bind_shared_record_probe(&origin, engine)
                    .map_err(internal)?;
                Ok(origin)
            })?;
        let prepared =
            self.with_shared_poll_context(tid, state, invocation, |engine, grant, _| {
                if grant.epoch() != epoch {
                    return Err(internal("shared Poll scan crossed original grant"));
                }
                let now = self.global_time.lock().unwrap().as_nanos();
                engine
                    .prepare_shared_record_effect(
                        &origin,
                        grant,
                        NetworkStreamPhysicalEffect::PollState,
                        now,
                    )
                    .map_err(internal)
            })?;
        let actual = runtime
            .execute_shared_record_effect(prepared)
            .await
            .map_err(internal)?;
        self.with_shared_poll_context(tid, state, invocation, |engine, grant, _| {
            if grant.epoch() != epoch {
                return Err(internal("shared Poll result crossed original grant"));
            }
            let now = self.global_time.lock().unwrap().as_nanos();
            runtime
                .with_shared_record_effect(&actual, engine, |engine, proof| {
                    engine
                        .confirm_shared_record_effect(proof, grant, now)
                        .map(|_| ())
                        .map_err(std::io::Error::other)
                })
                .map_err(internal)?;
            let source = engine
                .shared_record_poll_source(&origin, grant, now)
                .map_err(internal)?;
            runtime
                .with_shared_record_poll_publication(&origin, engine, |engine, proof| {
                    engine
                        .publish_shared_record_poll(source, proof, grant, now)
                        .map_err(std::io::Error::other)
                })
                .map_err(internal)
        })
    }

    pub(crate) async fn resume_shared_poll<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedPollInvocation,
    ) -> Result<(), NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll continuation lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Poll continuation lost engine"))?;
        let epoch =
            self.with_shared_poll_context(tid, state, invocation, |_, grant, _| Ok(grant.epoch()))?;
        let prefix = runtime
            .join_shared_foreground_prefix(
                invocation.captured.custody.root.clone(),
                engine,
                Some(invocation.call.id),
            )
            .await
            .map_err(internal)?;
        self.with_shared_poll_context(tid, state, invocation, |engine, grant, _| {
            if grant.epoch() != epoch {
                return Err(internal("shared Poll resume crossed continuation grant"));
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

    pub(super) async fn suspend_shared_poll<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedPollInvocation,
        publication: Option<&Arc<SharedRecordPollPublication>>,
    ) -> Result<Vec<crate::resources::NetworkWaitKind>, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll suspension lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Poll suspension lost engine"))?;
        let (epoch, completed) =
            self.with_shared_poll_context(tid, state, invocation, |engine, grant, _| {
                let now = self.global_time.lock().unwrap().as_nanos();
                let completed = match publication {
                    Some(publication) => runtime
                        .with_shared_record_pending(
                            publication.source().origin(),
                            engine,
                            |engine, proof| {
                                engine
                                    .complete_shared_record_pending(proof, grant, now)
                                    .map_err(std::io::Error::other)
                            },
                        )
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
                invocation.captured.custody.root.clone(),
                engine,
                Some(invocation.call.id),
            )
            .await
            .map_err(internal)?;
        self.with_shared_poll_context(tid, state, invocation, |engine, grant, _| {
            if grant.epoch() != epoch {
                return Err(internal("shared Poll suspension crossed original grant"));
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
