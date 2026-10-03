//! Real source/worker/lease borrowers for shared Record receive. The legacy
//! sole-root FullStoreCompletion and private Drain issuers are unchanged.
use super::*;
use crate::network_replay::NetworkStreamPhysicalEffect;
use crate::network_replay::shared_waits::PreparedSharedRecordReceivePublication;
use crate::network_replay::shared_waits::SharedRecordDrainSubmission;
use crate::network_replay::shared_waits::SharedRecordNoStorePlan;
use crate::network_replay::shared_waits::SharedRecordProbe;
use crate::network_replay::shared_waits::SharedRecordReceiveSource;
use crate::network_replay::shared_waits::SharedRecordStored;
use crate::network_runtime::native_peer::Observation;

pub(crate) struct ConfirmedSharedRecordReceive<'a> {
    origin: &'a Arc<SharedRecordProbe>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ConfirmedSharedRecordReceive<'_> {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        self.origin
    }
}
pub(crate) struct ConfirmedSharedRecordOutput<'a> {
    // Keep the exact source borrowed for the callback alongside its live guards.
    _source: &'a Arc<SharedRecordReceiveSource>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
#[derive(Debug)]
pub(crate) struct JoinedSharedDrain {
    submission: Arc<SharedRecordDrainSubmission>,
    observed: Observation,
    joined: JoinedNativeWorkerReceipt,
}
impl JoinedSharedDrain {
    pub(crate) fn submission(&self) -> &Arc<SharedRecordDrainSubmission> {
        &self.submission
    }
    pub(crate) fn observed(&self) -> &Observation {
        &self.observed
    }
}
pub(crate) struct ConfirmedSharedDrain<'a> {
    joined: &'a Arc<JoinedSharedDrain>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ConfirmedSharedDrain<'_> {
    pub(crate) fn joined(&self) -> &Arc<JoinedSharedDrain> {
        self.joined
    }
}

impl RuntimeShared {
    fn check_record_receive_effects(
        self: &Arc<Self>,
        source: &SharedRecordReceiveSource,
    ) -> std::io::Result<()> {
        for (at, effect) in source.effects().iter().enumerate() {
            if !Arc::ptr_eq(effect.step().origin(), source.origin())
                || effect.step().number() != at as u64 + 1
            {
                return Err(std::io::Error::other(
                    "Record output changed append-only original worker history",
                ));
            }
            effect.validate_runtime(self)?;
        }
        source.predecessor().joined_worker()?.validate_runtime(self)
    }
}
impl NetworkRuntimeResources {
    /// Pure source planning may inspect an eligible source only under the exact
    /// final confirmed scan, helper retirement and complete worker census.
    pub(crate) fn with_shared_record_receive_source<T>(
        &self,
        origin: &Arc<SharedRecordProbe>,
        engine: &mut NetworkReplayEngine,
        inspect: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordReceive<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let peers = engine
            .shared_record_probe_peers(origin)
            .map_err(std::io::Error::other)?;
        self.shared.check_shared_probe_peers(origin, &peers)?;
        let effects = engine
            .shared_record_probe_effects(origin)
            .map_err(std::io::Error::other)?;
        for (at, effect) in effects.iter().enumerate() {
            if !Arc::ptr_eq(effect.step().origin(), origin)
                || effect.step().number() != at as u64 + 1
            {
                return Err(std::io::Error::other(
                    "Record source changed worker history",
                ));
            }
            effect.validate_runtime(&self.shared)?;
        }
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, effects.len() as u64, None)?;
        let calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, true)?;
        calls.preflight_shared_probe_retirement(origin, effects)?;
        inspect(
            engine,
            &ConfirmedSharedRecordReceive {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )
    }
    pub(crate) fn with_shared_record_receive_output<T>(
        &self,
        origin: &Arc<SharedRecordProbe>,
        engine: &mut NetworkReplayEngine,
        reserve: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordReceive<'_>,
        ) -> std::io::Result<Arc<SharedRecordReceiveSource>>,
        store: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordOutput<'_>,
            &Arc<SharedRecordReceiveSource>,
            &Arc<NativeSourceInterval>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let peers = engine
            .shared_record_probe_peers(origin)
            .map_err(std::io::Error::other)?;
        self.shared.check_shared_probe_peers(origin, &peers)?;
        let effects = engine
            .shared_record_probe_effects(origin)
            .map_err(std::io::Error::other)?;
        for effect in effects {
            effect.validate_runtime(&self.shared)?;
        }
        let count = effects.len() as u64;
        let mut workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, count, None)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, true)?;
        calls.preflight_shared_probe_retirement(origin, effects)?;
        let source = reserve(
            engine,
            &ConfirmedSharedRecordReceive {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        if !Arc::ptr_eq(source.origin(), origin) {
            return Err(std::io::Error::other(
                "Record reservation replaced original probe",
            ));
        }
        // Reservation has no effect and leaves exact Call debt on any refusal.
        // The same Calls guard retains the prevalidated original lease here.
        calls.transfer_shared_record_delivery(&source)?;
        let generation = workers.submission_generation;
        let interval = Arc::new(self.shared.reserve_shared_record_receive_interval(
            &mut workers,
            generation,
            source.clone(),
            engine,
            &calls,
        )?);
        // A full store only releases the wrapper's interval owner. The local
        // Arc below remains until the actual backend callback returns. This
        // borrower never retires the native lease or launches Drain.
        let value = store(
            engine,
            &ConfirmedSharedRecordOutput {
                _source: &source,
                _workers: &workers,
                _calls: &calls,
            },
            &source,
            &interval,
        )?;
        if !engine.shared_record_store_ended(&source) {
            return Err(std::io::Error::other(
                "Record output callback did not retain and complete its full actual store",
            ));
        }
        Ok(value)
    }
    fn prepare_record_drain(
        &self,
        stored: SharedRecordStored,
    ) -> std::io::Result<Arc<SharedRecordDrainSubmission>> {
        let source = stored.source().clone();
        let engine = self
            .shared
            .native_streams
            .lock()
            .unwrap()
            .shared_probe_engine(source.origin())?;
        let submission = {
            let mut engine = engine.lock().unwrap();
            let peers = engine
                .shared_record_receive_peers(&source)
                .map_err(std::io::Error::other)?;
            self.shared
                .check_shared_probe_peers(source.origin(), &peers)?;
            self.shared.check_record_receive_effects(&source)?;
            let workers = self.shared.native_workers.lock().unwrap();
            self.shared.check_shared_probe_workers(
                source.origin(),
                &workers,
                source.effects().len() as u64,
                None,
            )?;
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .require_shared_record_delivery(&peers, &source)?;
            engine
                .prepare_shared_record_drain(stored)
                .map_err(std::io::Error::other)?
        };
        submission.claim()?;
        Ok(submission)
    }
    pub(crate) async fn execute_shared_record_drain(
        &self,
        stored: SharedRecordStored,
    ) -> std::io::Result<Arc<JoinedSharedDrain>> {
        #[cfg(test)]
        {
            let controlled = self.shared.record_receive_fixture.lock().unwrap().clone();
            if let Some(controlled) = controlled {
                controlled.begin_drain()?;
                return self.controlled_shared_record_drain(stored, false).await;
            }
        }
        let submission = self.prepare_record_drain(stored)?;
        let observed = self
            .shared
            .execute_shared_record_receive_drain(submission.clone())
            .await?;
        let completion = observed
            .helper_copy
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shared Drain lost canonical completion"))?;
        let joined = completion.joined_worker()?.clone();
        joined.validate_runtime(&self.shared)?;
        Ok(Arc::new(JoinedSharedDrain {
            submission,
            observed,
            joined,
        }))
    }
    pub(crate) fn with_shared_record_drain<T>(
        &self,
        joined: &Arc<JoinedSharedDrain>,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedDrain<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let source = joined.submission.source();
        joined.joined.validate_runtime(&self.shared)?;
        let peers = engine
            .validate_shared_record_drain(&joined.submission)
            .map_err(std::io::Error::other)?;
        self.shared
            .check_shared_probe_peers(source.origin(), &peers)?;
        self.shared.check_record_receive_effects(source)?;
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared.check_shared_probe_workers(
            source.origin(),
            &workers,
            source.effects().len() as u64 + 1,
            None,
        )?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_record_delivery(&peers, source)?;
        let effect = NetworkStreamPhysicalEffect::Drain {
            maximum: source.len(),
        };
        calls.preflight_confirmation(source.owner(), source.lease(), &effect, &joined.observed)?;
        let result = transition(
            engine,
            &ConfirmedSharedDrain {
                joined,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        calls
            .confirm(source.owner(), source.lease(), &effect, &joined.observed)
            .expect("exact actual Drain preflight held through engine confirmation");
        Ok(result)
    }
}

impl RuntimeShared {
    pub(in crate::network_runtime) fn arm_shared_record_drain(
        self: &Arc<Self>,
        submission: &Arc<SharedRecordDrainSubmission>,
        worker: &NativeWorkerHandle,
    ) -> std::io::Result<()> {
        let source = submission.source();
        let engine = self
            .native_streams
            .lock()
            .unwrap()
            .shared_probe_engine(source.origin())?;
        let engine = engine.lock().unwrap();
        let peers = engine
            .validate_shared_record_drain(submission)
            .map_err(std::io::Error::other)?;
        self.check_shared_probe_peers(source.origin(), &peers)?;
        self.check_record_receive_effects(source)?;
        let workers = self.native_workers.lock().unwrap();
        self.check_shared_probe_workers(
            source.origin(),
            &workers,
            source.effects().len() as u64 + 1,
            Some(worker),
        )?;
        self.native_streams
            .lock()
            .unwrap()
            .require_shared_record_delivery(&peers, source)
    }
}

impl NetworkRuntimeResources {
    pub(crate) fn publish_shared_record_receive(
        &self,
        prepared: &Arc<PreparedSharedRecordReceivePublication>,
        engine: &mut NetworkReplayEngine,
        grant: &crate::scheduler::ordinary_fd::SharedMmForegroundObservation<'_>,
        now: detcore_model::time::LogicalTime,
    ) -> std::io::Result<usize> {
        let joined = prepared.joined();
        let source = prepared.source();
        joined.joined.validate_runtime(&self.shared)?;
        self.shared.check_record_receive_effects(source)?;
        let peers = engine
            .shared_record_receive_peers(source)
            .map_err(std::io::Error::other)?;
        self.shared
            .check_shared_probe_peers(source.origin(), &peers)?;
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared.check_shared_probe_workers(
            source.origin(),
            &workers,
            source.effects().len() as u64 + 1,
            None,
        )?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_record_delivery(&peers, source)?;
        calls.preflight_shared_record_drain_retirement(source, joined.observed())?;
        let value = engine
            .publish_shared_record_receive(
                prepared,
                &ConfirmedSharedDrain {
                    joined,
                    _workers: &workers,
                    _calls: &calls,
                },
                grant,
                now,
            )
            .map_err(std::io::Error::other)?;
        calls
            .finish_lease(source.owner(), source.lease())
            .expect("exact confirmed shared Drain lease preflighted under same lock");
        Ok(value)
    }
    /// Caller validates actual original backend context inside this borrow.
    /// Neither a selected plan nor a skipped write certifies that context.
    pub(crate) fn with_shared_record_no_store<T>(
        &self,
        plan: &SharedRecordNoStorePlan,
        engine: &mut NetworkReplayEngine,
        commit: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordReceive<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let origin = plan.origin();
        let peers = engine
            .shared_record_probe_peers(origin)
            .map_err(std::io::Error::other)?;
        self.shared.check_shared_probe_peers(origin, &peers)?;
        let effects = engine
            .shared_record_probe_effects(origin)
            .map_err(std::io::Error::other)?;
        for effect in effects {
            effect.validate_runtime(&self.shared)?;
        }
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, effects.len() as u64, None)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, true)?;
        calls.preflight_shared_probe_retirement(origin, effects)?;
        let value = commit(
            engine,
            &ConfirmedSharedRecordReceive {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        if !engine.shared_record_no_store_committed(plan) {
            return Err(std::io::Error::other(
                "NoStore callback did not commit original source",
            ));
        }
        calls
            .finish_lease(origin.owner(), origin.lease())
            .expect("exact joined NoStore source lease preflighted under same lock");
        Ok(value)
    }
}

#[cfg(test)]
impl NetworkRuntimeResources {
    /// Real retained socket Consume, Pending, worker arm and actual JoinHandle.
    /// Only the provider copy5 geometry is supplied, as in the existing Peek
    /// fixture. This is not a native BPF or original guest writer certificate.
    pub(crate) async fn controlled_shared_record_drain(
        &self,
        stored: SharedRecordStored,
        corrupt_geometry: bool,
    ) -> std::io::Result<Arc<JoinedSharedDrain>> {
        let submission = self.prepare_record_drain(stored)?;
        let selected = submission.clone();
        let shared = self.shared.clone();
        let (permit, entered) = std::sync::mpsc::channel();
        let (worker, reply) =
            self.shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    entered.recv().map_err(std::io::Error::other)?;
                    let source = selected.source();
                    let work = shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .prepare_shared_record_drain(&selected)?;
                    let held = work.helper.as_ref().unwrap().clone();
                    let engine = shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .shared_probe_engine(source.origin())?;
                    engine
                        .lock()
                        .unwrap()
                        .bind_shared_record_drain_helper(&selected, held.binding())
                        .map_err(std::io::Error::other)?;
                    let before = u64::from(corrupt_geometry);
                    let actual = held.controlled_observation_at(work.perform(), 5, before, 0)?;
                    shared.native_streams.lock().unwrap().retain(
                        source.owner(),
                        source.lease(),
                        held.binding().effect(),
                        actual.clone(),
                    )?;
                    Ok(actual)
                })?;
        self.shared.arm_shared_record_drain(&submission, &worker)?;
        permit.send(()).map_err(std::io::Error::other)?;
        let observed = reply.await.map_err(std::io::Error::other)??;
        let joined = self.shared.join_native_worker_receipt(&worker).await?;
        let source = submission.source();
        crate::network_runtime::helper_receive::retain_joined_helper(
            &self.shared,
            source.owner(),
            source.lease(),
            &crate::network_replay::NetworkStreamPhysicalEffect::Drain {
                maximum: source.len(),
            },
            &observed,
            joined.clone(),
        )?;
        Ok(Arc::new(JoinedSharedDrain {
            submission,
            observed,
            joined,
        }))
    }
}
