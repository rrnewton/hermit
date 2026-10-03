//! Actual shared Record source, held output, successor Drain and publication.
use super::*;
use crate::network_replay::shared_waits::SharedNoStoreResult;
use crate::network_replay::shared_waits::SharedProbeProgress;
use crate::network_replay::shared_waits::SharedRecordProbe;
use crate::network_replay::shared_waits::SharedRecordReceivePlan;
use crate::network_replay::shared_waits::SharedRecordReceiveSource;
use crate::network_replay::shared_waits::SharedRecordStored;

pub(crate) struct PreparedSharedRecordReceive {
    epoch: u64,
    origin: Arc<SharedRecordProbe>,
    pending: bool,
}
impl PreparedSharedRecordReceive {
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        &self.origin
    }
}

pub(crate) enum SharedRecordReceiveEffect {
    Stored(SharedRecordStored),
    NoStore(SharedNoStoreResult),
}

/// Only the actual held writer below issues production outcomes. The original
/// Call retains every effect before any subsequent fallible check.
#[derive(Debug)]
pub(crate) struct SharedRecordStoreAttempt {
    source: Arc<SharedRecordReceiveSource>,
    outcome: reverie::syscalls::NativeUserStoreOutcome,
    interval: Mutex<Option<Arc<NativeSourceInterval>>>,
}
impl SharedRecordStoreAttempt {
    pub(crate) fn source(&self) -> &Arc<SharedRecordReceiveSource> {
        &self.source
    }
    pub(crate) fn outcome(&self) -> &reverie::syscalls::NativeUserStoreOutcome {
        &self.outcome
    }
    pub(crate) fn release_interval_after_store(&self) {
        self.interval.lock().unwrap().take();
    }
    #[cfg(test)]
    pub(crate) fn controlled_with_interval(
        source: Arc<SharedRecordReceiveSource>,
        outcome: reverie::syscalls::NativeUserStoreOutcome,
        interval: Arc<NativeSourceInterval>,
    ) -> Self {
        Self {
            source,
            outcome,
            interval: Mutex::new(Some(interval)),
        }
    }
}

impl GlobalState {
    pub(crate) async fn prepare_shared_record_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
    ) -> Result<PreparedSharedRecordReceive, NetworkRpcError> {
        if self.cfg.network_trace.policy != NetworkPolicy::Record {
            return Err(internal("shared Record receive changed closed policy"));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Record receive lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Record receive lost engine"))?;
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
        let origin =
            self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
                if grant.epoch() != epoch {
                    return Err(internal("shared Receive probe crossed original Normal"));
                }
                let now = self.global_time.lock().unwrap().as_nanos();
                let origin = runtime
                    .with_shared_attempt_prefix(&prefix, engine, |engine, admission| {
                        engine
                            .begin_shared_record_probe(invocation.call.id, grant, admission, now)
                            .map_err(std::io::Error::other)
                    })
                    .map_err(internal)?;
                runtime
                    .bind_shared_record_probe(&origin, engine)
                    .map_err(internal)?;
                Ok(origin)
            })?;
        loop {
            let next = self.with_shared_receive_wait_context(
                tid,
                state,
                invocation,
                |engine, grant, _| {
                    if grant.epoch() != epoch {
                        return Err(internal(
                            "shared Receive observation crossed original Normal",
                        ));
                    }
                    let now = self.global_time.lock().unwrap().as_nanos();
                    match engine
                        .shared_record_probe_progress(&origin, now)
                        .map_err(internal)?
                    {
                        SharedProbeProgress::Need(effect) => engine
                            .prepare_shared_record_effect(&origin, grant, effect, now)
                            .map(Some)
                            .map_err(internal),
                        SharedProbeProgress::PendingCandidate
                        | SharedProbeProgress::EligibleSource => Ok(None),
                        SharedProbeProgress::Refused(reason) => Err(internal(reason)),
                    }
                },
            )?;
            let Some(next) = next else {
                break;
            };
            let actual = runtime
                .execute_shared_record_effect(next)
                .await
                .map_err(internal)?;
            self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
                if grant.epoch() != epoch {
                    return Err(internal("shared Receive result crossed original Normal"));
                }
                let now = self.global_time.lock().unwrap().as_nanos();
                runtime
                    .with_shared_record_effect(&actual, engine, |engine, proof| {
                        engine
                            .confirm_shared_record_effect(proof, grant, now)
                            .map(|_| ())
                            .map_err(std::io::Error::other)
                    })
                    .map_err(internal)
            })?;
        }
        let pending =
            self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
                if grant.epoch() != epoch {
                    return Err(internal(
                        "shared Receive preparation crossed original Normal",
                    ));
                }
                match engine
                    .shared_record_probe_progress(
                        &origin,
                        self.global_time.lock().unwrap().as_nanos(),
                    )
                    .map_err(internal)?
                {
                    SharedProbeProgress::PendingCandidate => Ok(true),
                    SharedProbeProgress::EligibleSource => Ok(false),
                    SharedProbeProgress::Need(_) => {
                        Err(internal("shared Receive preparation lost completed source"))
                    }
                    SharedProbeProgress::Refused(reason) => Err(internal(reason)),
                }
            })?;
        Ok(PreparedSharedRecordReceive {
            epoch,
            origin,
            pending,
        })
    }

    pub(crate) fn store_shared_record_receive<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        invocation: &SharedReceiveInvocation,
        prepared: PreparedSharedRecordReceive,
        restored: bool,
    ) -> Result<SharedRecordReceiveEffect, NetworkRpcError> {
        if prepared.pending || self.cfg.network_trace.policy != NetworkPolicy::Record {
            return Err(internal(
                "shared Receive output requires completed eligible Record source",
            ));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Record output lost runtime"))?;
        let original =
            reverie::syscalls::Syscall::from_raw(invocation.policy.raw.0, invocation.policy.raw.1);
        let action = |writer: &mut dyn reverie::syscalls::FollowedStore| {
            self.with_shared_receive_wait_context(
                guest.tid(),
                guest.thread_state(),
                invocation,
                |engine, grant, _| {
                    if grant.epoch() != prepared.epoch {
                        return Err(internal("shared Record output crossed original Normal"));
                    }
                    writer
                        .validate_context()
                        .map_err(|e| internal(format!("shared Record output context: {e:?}")))?;
                    let now = self.global_time.lock().unwrap().as_nanos();
                    let plan = runtime
                        .with_shared_record_receive_source(
                            &prepared.origin,
                            engine,
                            |engine, proof| {
                                engine
                                    .plan_shared_record_receive(&prepared.origin, grant, proof, now)
                                    .map_err(std::io::Error::other)
                            },
                        )
                        .map_err(internal)?;
                    match plan {
                        SharedRecordReceivePlan::Bytes(plan) => runtime
                            .with_shared_record_receive_output(
                                &prepared.origin,
                                engine,
                                |engine, proof| {
                                    engine
                                        .reserve_shared_record_receive(plan, grant, proof, now)
                                        .map_err(std::io::Error::other)
                                },
                                |engine, _proof, source, interval| {
                                    if source.call() != invocation.call.id
                                        || source.open_file() != invocation.call.open_file
                                        || source.raw() != invocation.policy.raw
                                        || source.epoch() != prepared.epoch
                                        || !Arc::ptr_eq(source.root(), invocation.policy.root())
                                        || !Arc::ptr_eq(source.policy(), &invocation.policy)
                                    {
                                        return Err(std::io::Error::other(
                                            "shared Record store changed original invocation",
                                        ));
                                    }
                                    engine
                                        .with_shared_record_store_retention(
                                            source,
                                            grant,
                                            now,
                                            |retainer| {
                                                let outcome = writer.store(source.bytes());
                                                retainer.retain(SharedRecordStoreAttempt {
                                                    source: source.clone(),
                                                    outcome,
                                                    interval: Mutex::new(Some(interval.clone())),
                                                })
                                            },
                                        )
                                        .map_err(std::io::Error::other)?
                                        .map_err(std::io::Error::other)?;
                                    engine
                                        .complete_shared_record_store(source, grant, now)
                                        .map(SharedRecordReceiveEffect::Stored)
                                        .map_err(std::io::Error::other)
                                },
                            )
                            .map_err(internal),
                        SharedRecordReceivePlan::NoStore(plan) => runtime
                            .with_shared_record_no_store(&plan, engine, |engine, proof| {
                                writer.validate_context().map_err(|e| {
                                    std::io::Error::other(format!(
                                        "shared Record no-store context: {e:?}"
                                    ))
                                })?;
                                engine
                                    .commit_shared_record_no_store(&plan, proof, grant, now)
                                    .map(SharedRecordReceiveEffect::NoStore)
                                    .map_err(std::io::Error::other)
                            })
                            .map_err(internal),
                    }
                },
            )
        };
        let result = if restored {
            guest.with_restored_followed_store(original, action)
        } else {
            guest.with_followed_store(original, action)
        }
        .map_err(|e| internal(format!("shared Record held output refused: {e:?}")))?;
        // No interval owner may cross into the Drain await. Failed effects stay
        // on the original Call; successful wrappers alone release their owner.
        drop(prepared);
        if matches!(&result, Ok(SharedRecordReceiveEffect::NoStore(_))) {
            self.finish_local_receive_release();
        }
        result
    }

    pub(crate) async fn drain_shared_record_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
        stored: SharedRecordStored,
    ) -> Result<usize, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Record Drain lost runtime"))?;
        let source = stored.source().clone();
        let epoch = source.epoch();
        self.with_shared_receive_wait_context(tid, state, invocation, |_, grant, _| {
            if self.cfg.network_trace.policy != NetworkPolicy::Record
                || grant.epoch() != epoch
                || source.call() != invocation.call.id
                || source.open_file() != invocation.call.open_file
                || !Arc::ptr_eq(source.policy(), &invocation.policy)
                || source.raw() != invocation.policy.raw
            {
                return Err(internal(
                    "shared Record Drain changed store epoch or original invocation",
                ));
            }
            Ok(())
        })?;
        let actual = runtime
            .execute_shared_record_drain(stored)
            .await
            .map_err(internal)?;
        let count =
            self.with_shared_receive_wait_context(tid, state, invocation, |engine, grant, _| {
                if grant.epoch() != epoch {
                    return Err(internal("shared Record publication crossed store Normal"));
                }
                let now = self.global_time.lock().unwrap().as_nanos();
                let prepared = runtime
                    .with_shared_record_drain(&actual, engine, |engine, proof| {
                        engine
                            .confirm_shared_record_drain(proof, grant, now)
                            .map_err(std::io::Error::other)
                    })
                    .map_err(internal)?;
                runtime
                    .publish_shared_record_receive(&prepared, engine, grant, now)
                    .map_err(internal)
            })?;
        self.finish_local_receive_release();
        Ok(count)
    }
}
