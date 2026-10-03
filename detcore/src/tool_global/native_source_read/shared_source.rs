//! Selected Replay source custody for the separately versioned shared-MM
//! policy. Every publication retains its original grant, Call and joined
//! prefix; physical cohort staging is performed by the actual Guest backend.
use super::*;
use crate::network_runtime::SharedForegroundLineage;
use crate::network_runtime::shared_waits::JoinedSharedPrefix;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

pub(crate) struct SharedNativeSource {
    root: Arc<ForegroundRoot>,
    epoch: u64,
    call: crate::network_replay::NetworkStreamCallId,
    prefix: JoinedSharedPrefix,
    pub(super) address: usize,
    pub(super) length: usize,
    pub(super) interval: NativeSourceInterval,
}

impl GlobalState {
    /// The engine's closed policy is chosen before effects. A caller cannot
    /// turn a legacy refusal into shared authority with a flag or retry.
    pub(crate) fn shared_mm_attempts_active(&self) -> bool {
        self.network_engine
            .as_ref()
            .is_some_and(|engine| engine.lock().unwrap().uses_shared_mm_attempts())
    }

    fn with_shared_replay_read<T, R>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
        commit: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedMmForegroundObservation<'_>,
            OpenFileId,
            &SharedForegroundLineage<'_>,
        ) -> Result<R, NetworkRpcError>,
    ) -> Result<R, NetworkRpcError> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Replay source lost runtime"))?;
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(owner, lineage)
                        .map_err(internal)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    if self.cfg.network_trace.policy != NetworkPolicy::Replay
                        || read.publication.permit.files != grant.root().files()
                    {
                        return Err(internal(
                            "shared Replay source changed original policy/files",
                        ));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let mut metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| internal("shared Replay source lost engine"))?
                        .lock()
                        .unwrap();
                    if !engine.uses_shared_mm_attempts() {
                        return Err(internal("shared Replay source changed closed policy"));
                    }
                    let file = engine
                        .validate_replay_transmit_read(
                            owner,
                            read,
                            &state.file_metadata,
                            &mut metadata,
                        )
                        .map_err(internal)?;
                    commit(&mut engine, &grant, file, lineage)
                })())
            })
            .map_err(internal)?
    }

    pub(in crate::tool_global) fn shared_replay_sendto_read_limit<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
        requested: usize,
    ) -> Result<usize, NetworkRpcError> {
        self.with_shared_replay_read(tid, state, read, |engine, _, file, _| {
            engine
                .transmit_stream_read_limit(file, requested)
                .map_err(internal)
        })
    }

    pub(super) async fn prepare_shared_replay_transmit_source<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
        address: usize,
        length: usize,
    ) -> Result<PreparedNativeSource, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Replay source lost runtime"))?;
        let (root, epoch, transmitted) =
            self.with_shared_replay_read(tid, state, read, |engine, grant, file, _| {
                if engine
                    .transmit_stream_read_limit(file, length)
                    .map_err(internal)?
                    != length
                {
                    return Err(internal("shared Replay source changed selected prefix"));
                }
                Ok((
                    grant.root().clone(),
                    grant.epoch(),
                    engine.replay_transmit_offset(file).map_err(internal)?,
                ))
            })?;
        // No scheduler, census, metadata or engine lock spans this real join.
        let prefix = runtime
            .join_shared_foreground_prefix(
                root.clone(),
                self.network_engine
                    .as_ref()
                    .ok_or_else(|| internal("shared Replay source lost engine"))?,
                None,
            )
            .await
            .map_err(internal)?;
        let (call, interval) =
            self.with_shared_replay_read(tid, state, read, |engine, grant, file, lineage| {
                if grant.epoch() != epoch
                    || !Arc::ptr_eq(grant.root(), &root)
                    || engine.replay_transmit_offset(file).map_err(internal)? != transmitted
                    || engine
                        .transmit_stream_read_limit(file, length)
                        .map_err(internal)?
                        != length
                {
                    return Err(internal(
                        "shared Replay source crossed original grant/prefix",
                    ));
                }
                // The runtime reserves the original interval before transfer.
                // Every refusal leaves the selected read caller-owned. Success
                // transfers it once, with no fallible work after that transfer.
                runtime
                    .prepare_shared_replay_source(&prefix, lineage, engine, |engine, admission| {
                        engine
                            .begin_shared_replay_transmit(
                                read.clone(),
                                grant,
                                &prefix,
                                admission,
                                length,
                            )
                            .map_err(std::io::Error::other)
                    })
                    .map_err(internal)
            })?;
        Ok(PreparedNativeSource::Shared(SharedNativeSource {
            root,
            epoch,
            call: call.id,
            prefix,
            address,
            length,
            interval,
        }))
    }

    pub(super) fn finish_shared_replay_transmit_source<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        prepared: SharedNativeSource,
        bytes: Vec<u8>,
    ) -> Result<NetworkStreamTransmit, NetworkRpcError> {
        let owner = prepared.root.owner();
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Replay source lost runtime"))?;
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(owner, lineage)
                        .map_err(internal)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    if self.cfg.network_trace.policy != NetworkPolicy::Replay
                        || grant.epoch() != prepared.epoch
                        || !Arc::ptr_eq(grant.root(), &prepared.root)
                    {
                        return Err(internal(
                            "shared Replay source crossed original policy/grant/root",
                        ));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| internal("shared Replay source lost engine"))?
                        .lock()
                        .unwrap();
                    engine
                        .validate_fd_metadata(
                            owner,
                            prepared.root.files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(internal)?;
                    let outcome = runtime
                        .with_shared_source_interval(
                            &prepared.interval,
                            &mut engine,
                            prepared.call,
                            |engine| {
                                Ok(engine.complete_shared_replay_transmit(
                                    prepared.call,
                                    &grant,
                                    &prepared.prefix,
                                    &bytes,
                                ))
                            },
                        )
                        .map_err(internal)?
                        .map_err(|error| {
                            NetworkRpcError::from_engine(
                                NetworkPolicy::Replay,
                                NetworkFailurePhase::Transmit,
                                error,
                            )
                        })?;
                    self.release_lifetime_ports(engine.take_lifetime_retired_ports());
                    Ok(match outcome {
                        StreamTransmitOutcome::Accepted(count) => {
                            NetworkStreamTransmit::Accepted(count)
                        }
                        StreamTransmitOutcome::Error(errno) => NetworkStreamTransmit::Error(errno),
                    })
                })())
            })
            .map_err(internal)?
    }
}
