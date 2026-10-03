//! The actual held backend writer is the only production outcome issuer.
use super::*;

/// No Clone and no numeric constructor. The exact runtime interval stays owned
/// on the original Call with every actual failed or successful store outcome.
#[derive(Debug)]
pub(crate) struct SharedPollStoreAttempt {
    source: Arc<crate::network_replay::shared_waits::SharedPollSource>,
    outcome: reverie::syscalls::NativeUserStoreOutcome,
    interval: Mutex<Option<Arc<crate::network_runtime::NativeSourceInterval>>>,
}
impl SharedPollStoreAttempt {
    pub(crate) fn source(&self) -> &Arc<crate::network_replay::shared_waits::SharedPollSource> {
        &self.source
    }
    pub(crate) fn outcome(&self) -> &reverie::syscalls::NativeUserStoreOutcome {
        &self.outcome
    }
    /// The engine calls this only after exact full output and source settlement
    /// commit. The actual Global borrower still retains the identical interval
    /// through the end of its synchronous held-backend callback. Failed output
    /// never reaches this operation and cannot reopen worker admission.
    pub(crate) fn release_interval_after_commit(&self) {
        self.interval.lock().unwrap().take();
    }

    /// Supplied outcome with an actual runtime interval tests ownership only.
    #[cfg(test)]
    pub(crate) fn controlled_with_interval(
        source: Arc<crate::network_replay::shared_waits::SharedPollSource>,
        outcome: reverie::syscalls::NativeUserStoreOutcome,
        interval: Arc<crate::network_runtime::NativeSourceInterval>,
    ) -> Self {
        Self {
            source,
            outcome,
            interval: Mutex::new(Some(interval)),
        }
    }
}

impl GlobalState {
    pub(super) fn with_shared_poll_context<T, R>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedPollInvocation,
        action: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedMmForegroundObservation<'_>,
            &SharedForegroundLineage<'_>,
        ) -> Result<R, NetworkRpcError>,
    ) -> Result<R, NetworkRpcError> {
        let custody = &invocation.captured.custody;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll lost runtime"))?;
        let mode = match self.cfg.network_trace.policy {
            NetworkPolicy::Record => crate::network_replay::NetworkEngineMode::Record,
            NetworkPolicy::Replay => crate::network_replay::NetworkEngineMode::Replay,
            _ => return Err(internal("shared Poll changed closed Record/Replay policy")),
        };
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(custody.owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(custody.owner, lineage)
                        .map_err(internal)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    if !Arc::ptr_eq(&custody.root, grant.root())
                        || state.dettid != custody.owner.thread
                        || state.mm_id != custody.owner.mm
                    {
                        return Err(internal("shared Poll changed original task/MM/root"));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| internal("shared Poll lost engine"))?
                        .lock()
                        .unwrap();
                    if !engine.uses_shared_mm_attempts() || engine.mode() != mode {
                        return Err(internal("shared Poll changed engine profile"));
                    }
                    engine
                        .validate_fd_metadata(
                            custody.owner,
                            custody.root.files(),
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

pub(crate) struct PreparedSharedPollAttempt {
    epoch: u64,
    publication: Option<Arc<crate::network_replay::shared_waits::SharedRecordPollPublication>>,
    prefix: Option<crate::network_runtime::shared_waits::JoinedSharedPrefix>,
}

impl GlobalState {
    /// Select and write only inside the actual original/restored backend hold.
    /// No native reservation spans an await; Pending creates no output source.
    pub(crate) async fn prepare_shared_poll_attempt<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        invocation: &SharedPollInvocation,
    ) -> Result<PreparedSharedPollAttempt, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll output lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Poll output lost engine"))?;
        let epoch = self.with_shared_poll_context(
            guest.tid(),
            guest.thread_state(),
            invocation,
            |_, grant, _| Ok(grant.epoch()),
        )?;
        let (publication, prefix) = match self.cfg.network_trace.policy {
            NetworkPolicy::Record => (
                Some(
                    self.observe_shared_record_poll(guest.tid(), guest.thread_state(), invocation)
                        .await?,
                ),
                None,
            ),
            NetworkPolicy::Replay => (
                None,
                Some(
                    runtime
                        .join_shared_foreground_prefix(
                            invocation.captured.custody.root.clone(),
                            engine,
                            Some(invocation.call.id),
                        )
                        .await
                        .map_err(internal)?,
                ),
            ),
            _ => return Err(internal("shared Poll output changed closed policy")),
        };
        Ok(PreparedSharedPollAttempt {
            epoch,
            publication,
            prefix,
        })
    }

    pub(crate) fn store_shared_poll_attempt<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        invocation: &SharedPollInvocation,
        prepared: &PreparedSharedPollAttempt,
    ) -> Result<Option<i64>, NetworkRpcError> {
        use crate::network_replay::shared_waits::SharedPollDecision;
        let PreparedSharedPollAttempt {
            epoch,
            publication,
            prefix,
        } = prepared;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll output lost runtime"))?;
        let stored = guest
            .with_followed_poll_store(invocation.original(), |writer| {
                self.with_shared_poll_context(
                    guest.tid(),
                    guest.thread_state(),
                    invocation,
                    |engine, grant, lineage| {
                        if grant.epoch() != *epoch || writer.input() != invocation.captured.input {
                            return Err(internal("held Poll changed grant or original row"));
                        }
                        writer
                            .validate_context()
                            .map_err(|e| internal(format!("held Poll original context: {e:?}")))?;
                        let now = self.global_time.lock().unwrap().as_nanos();
                        let decision = match &publication {
                            Some(publication) => runtime
                                .with_shared_record_poll_publication(
                                    publication.source().origin(),
                                    engine,
                                    |engine, proof| {
                                        engine
                                            .plan_shared_record_poll(publication, grant, proof, now)
                                            .map_err(std::io::Error::other)
                                    },
                                )
                                .map_err(internal)?,
                            None => runtime
                                .with_shared_attempt_prefix(
                                    prefix.as_ref().expect("Replay retains original prefix"),
                                    engine,
                                    |engine, admission| {
                                        engine
                                            .plan_shared_replay_poll(
                                                invocation.call.id,
                                                grant,
                                                admission,
                                                now,
                                            )
                                            .map_err(std::io::Error::other)
                                    },
                                )
                                .map_err(internal)?,
                        };
                        let SharedPollDecision::Store(plan) = decision else {
                            return Ok(None);
                        };
                        let result = match &publication {
                            Some(publication) => runtime.with_shared_record_poll_output(
                                publication,
                                engine,
                                |engine, proof| {
                                    engine
                                        .reserve_shared_record_poll(plan, grant, proof, now)
                                        .map_err(std::io::Error::other)
                                },
                                |engine, proof, source, interval| {
                                    Self::retain_actual_shared_poll_store(
                                        engine, grant, invocation, writer, source, interval, now,
                                    )?;
                                    engine
                                        .complete_shared_record_poll_store(
                                            source, grant, proof, now,
                                        )
                                        .map_err(std::io::Error::other)
                                },
                            ),
                            None => runtime.with_shared_replay_poll_output(
                                prefix.as_ref().expect("Replay retains original prefix"),
                                lineage,
                                engine,
                                invocation.call.id,
                                |engine, admission| {
                                    engine
                                        .reserve_shared_replay_poll(plan, grant, admission, now)
                                        .map_err(std::io::Error::other)
                                },
                                |engine, source, interval| {
                                    Self::retain_actual_shared_poll_store(
                                        engine, grant, invocation, writer, source, interval, now,
                                    )?;
                                    engine
                                        .complete_shared_replay_poll_store(source, grant, now)
                                        .map_err(std::io::Error::other)
                                },
                            ),
                        }
                        .map_err(internal)?;
                        Ok(Some(result))
                    },
                )
            })
            .map_err(|e| internal(format!("shared Poll held writer refused: {e:?}")))??;
        if stored.is_some() {
            self.finish_local_receive_release();
        }
        Ok(stored)
    }

    pub(crate) async fn suspend_prepared_shared_poll<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        invocation: &SharedPollInvocation,
        prepared: PreparedSharedPollAttempt,
    ) -> Result<Vec<crate::resources::NetworkWaitKind>, NetworkRpcError> {
        self.suspend_shared_poll(
            guest.tid(),
            guest.thread_state(),
            invocation,
            prepared.publication.as_ref(),
        )
        .await
    }

    fn retain_actual_shared_poll_store(
        engine: &mut NetworkReplayEngine,
        grant: &SharedMmForegroundObservation<'_>,
        invocation: &SharedPollInvocation,
        writer: &mut dyn reverie::syscalls::FollowedPollStore,
        source: &Arc<crate::network_replay::shared_waits::SharedPollSource>,
        interval: &Arc<crate::network_runtime::NativeSourceInterval>,
        now: LogicalTime,
    ) -> std::io::Result<()> {
        let expected = invocation.captured.input;
        if source.call() != invocation.call.id
            || source.owner() != invocation.captured.custody.owner
            || source.raw() != invocation.captured.custody.raw
            || !Arc::ptr_eq(source.root(), &invocation.captured.custody.root)
            || source.input() != (expected.fd, expected.events, expected.timeout_millis)
        {
            return Err(std::io::Error::other(
                "Poll reservation changed original invocation",
            ));
        }
        engine
            .with_shared_poll_store_retention(source, grant, now, |retainer| {
                let outcome = writer.store_revents(source.revents());
                // The retainer exclusively borrows the original output. Its append
                // is unconditional and has no structural Call lookup after effect.
                retainer.retain(SharedPollStoreAttempt {
                    source: source.clone(),
                    outcome,
                    interval: Mutex::new(Some(interval.clone())),
                })
            })
            .map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)
    }
}
