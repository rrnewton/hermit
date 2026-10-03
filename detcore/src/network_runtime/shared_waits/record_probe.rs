//! Native shared-probe evidence is issued by the original worker's actual join.
//! A direct PollState has no provider transport ACK; a helper Peek retains its
//! own exact copy5 retirement, Binding and JoinHandle receipt.
use super::*;
use crate::network_replay::shared_waits::PreparedSharedEffect;
use crate::network_replay::shared_waits::SharedEffectIdentity;
use crate::network_replay::shared_waits::SharedRecordProbe;
use crate::network_runtime::native_peer::Observation;

#[derive(Debug)]
pub(crate) struct JoinedSharedEffect {
    step: Arc<SharedEffectIdentity>,
    observed: Observation,
    joined: JoinedNativeWorkerReceipt,
}
impl JoinedSharedEffect {
    pub(crate) fn step(&self) -> &Arc<SharedEffectIdentity> {
        &self.step
    }
    pub(crate) fn observed(&self) -> &Observation {
        &self.observed
    }
    pub(super) fn validate_runtime(&self, runtime: &Arc<RuntimeShared>) -> std::io::Result<()> { self.joined.validate_runtime(runtime) }
}
pub(crate) struct ConfirmedSharedEffect<'a> {
    joined: &'a Arc<JoinedSharedEffect>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ConfirmedSharedEffect<'_> {
    pub(crate) fn joined(&self) -> &Arc<JoinedSharedEffect> {
        self.joined
    }
}
pub(crate) struct ConfirmedSharedRecordPending<'a> {
    origin: &'a Arc<SharedRecordProbe>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ConfirmedSharedRecordPending<'_> {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        self.origin
    }
}

/// A full actual scan with settled native effect/worker custody. This says
/// nothing about readiness, timeout, syscall completion or lease retirement.
pub(crate) struct ConfirmedSharedRecordPoll<'a> {
    origin: &'a Arc<SharedRecordProbe>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ConfirmedSharedRecordPoll<'_> {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        self.origin
    }
}

impl RuntimeShared {
    pub(super) fn check_shared_probe_workers(
        self: &Arc<Self>,
        origin: &SharedRecordProbe,
        workers: &NativeWorkers,
        effects: u64,
        running: Option<&NativeWorkerHandle>,
    ) -> std::io::Result<()> {
        let prefix = origin.prefix();
        let expected = prefix
            .prefix
            .generation
            .checked_add(effects)
            .ok_or_else(|| std::io::Error::other("shared probe worker generation overflow"))?;
        if !prefix.prefix.shared.ptr_eq(&Arc::downgrade(self))
            || prefix.selected != Some(origin.call())
            || workers.submission_generation != expected
            || workers.closed
            || workers.copy_exclusion.is_some()
            || workers.source_read_active()
            || !origin.root().is_current(origin.owner())
            || !origin.root().has_shared_mm_history()
        {
            return Err(std::io::Error::other(
                "shared probe changed original runtime prefix",
            ));
        }
        let exact = match running {
            Some(handle) => workers.tasks.len() == 1 && Arc::ptr_eq(&workers.tasks[0], handle),
            None => workers.tasks.is_empty(),
        };
        if !exact {
            return Err(std::io::Error::other(
                "shared probe has extra or unjoined native workers",
            ));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        Ok(())
    }

    pub(super) fn check_shared_probe_peers(
        &self,
        origin: &SharedRecordProbe,
        peers: &SharedCallCensus,
    ) -> std::io::Result<()> {
        let mut original = origin.prefix().census.clone();
        original.rows.retain(|r| r.call != origin.call());
        if !original.same_rows(peers) {
            return Err(std::io::Error::other(
                "shared probe changed original suspended peer census",
            ));
        }
        Ok(())
    }

    pub(in crate::network_runtime) fn shared_effect_engine(
        &self,
        step: &SharedEffectIdentity,
    ) -> std::io::Result<Arc<Mutex<NetworkReplayEngine>>> {
        self.native_streams
            .lock()
            .unwrap()
            .shared_probe_engine(step.origin())
    }

    fn claim_shared_effect(
        self: &Arc<Self>,
        step: &Arc<SharedEffectIdentity>,
    ) -> std::io::Result<()> {
        let engine = self.shared_effect_engine(step)?;
        let engine = engine.lock().unwrap();
        let peers = engine
            .validate_shared_record_effect(step)
            .map_err(std::io::Error::other)?;
        self.check_shared_probe_peers(step.origin(), &peers)?;
        let workers = self.native_workers.lock().unwrap();
        self.check_shared_probe_workers(step.origin(), &workers, step.number() - 1, None)?;
        self.native_streams
            .lock()
            .unwrap()
            .require_shared_probe(&peers, step.origin(), true)?;
        step.claim()
    }

    /// The worker is retained but gated. Validation and permit publication run
    /// before its first native effect; a different worker cannot replace it.
    pub(in crate::network_runtime) fn arm_shared_effect(
        self: &Arc<Self>,
        step: &Arc<SharedEffectIdentity>,
        worker: &NativeWorkerHandle,
    ) -> std::io::Result<()> {
        let engine = self.shared_effect_engine(step)?;
        let engine = engine.lock().unwrap();
        let peers = engine
            .validate_shared_record_effect(step)
            .map_err(std::io::Error::other)?;
        self.check_shared_probe_peers(step.origin(), &peers)?;
        let workers = self.native_workers.lock().unwrap();
        self.check_shared_probe_workers(step.origin(), &workers, step.number(), Some(worker))?;
        self.native_streams
            .lock()
            .unwrap()
            .require_shared_probe(&peers, step.origin(), true)
    }
}

impl NetworkRuntimeResources {
    /// Called immediately after the engine acquired the exact existing control.
    /// Failure retains that engine origin; it is never retried as unsubmitted.
    pub(crate) fn bind_shared_record_probe(
        &self,
        origin: &Arc<SharedRecordProbe>,
        engine: &NetworkReplayEngine,
    ) -> std::io::Result<()> {
        let peers = engine
            .shared_record_probe_peers(origin)
            .map_err(std::io::Error::other)?;
        self.shared.check_shared_probe_peers(origin, &peers)?;
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, 0, None)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, false)?;
        calls.bind_lease(origin.owner(), origin.call(), origin.lease())
    }

    pub(crate) async fn execute_shared_record_effect(
        &self,
        prepared: PreparedSharedEffect,
    ) -> std::io::Result<Arc<JoinedSharedEffect>> {
        let step = prepared.into_identity();
        #[cfg(test)]
        if matches!(
            step.effect(),
            crate::network_replay::NetworkStreamPhysicalEffect::Peek { .. }
        ) {
            let controlled = self.shared.record_receive_fixture.lock().unwrap().clone();
            if let Some(controlled) = controlled {
                controlled.begin_peek()?;
                return self.controlled_shared_record_effect_identity(step).await;
            }
        }
        self.shared.claim_shared_effect(&step)?;
        let owner = step.origin().owner();
        let lease = step.origin().lease();
        let (observed, joined) = if matches!(
            step.effect(),
            crate::network_replay::NetworkStreamPhysicalEffect::Peek { .. }
        ) {
            let observed = self
                .shared
                .execute_shared_helper_receive(step.clone())
                .await?;
            let joined = observed
                .helper_copy
                .as_ref()
                .ok_or_else(|| std::io::Error::other("shared Peek lost helper receipt"))?
                .joined_worker()?
                .clone();
            (observed, joined)
        } else {
            let (permit, entered) = std::sync::mpsc::channel();
            let shared = self.shared.clone();
            let effect = step.effect().clone();
            let (worker, reply) = self.shared.start_native_worker(
                tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?,
                move || {
                    entered
                        .recv()
                        .map_err(|_| std::io::Error::other("shared control was never armed"))?;
                    let work = shared.native_streams.lock().unwrap().prepare(
                        owner,
                        lease,
                        effect.clone(),
                    )?;
                    let result = work.perform_control()?;
                    shared.native_streams.lock().unwrap().retain(
                        owner,
                        lease,
                        &effect,
                        result.clone(),
                    )?;
                    Ok(result)
                },
            )?;
            self.shared.arm_shared_effect(&step, &worker)?;
            permit
                .send(())
                .map_err(|_| std::io::Error::other("shared control lost gated worker"))?;
            let result = reply.await;
            let joined = self.shared.join_native_worker_receipt(&worker).await?;
            (result.map_err(std::io::Error::other)??, joined)
        };
        joined.validate_runtime(&self.shared)?;
        Ok(Arc::new(JoinedSharedEffect {
            step,
            observed,
            joined,
        }))
    }

    /// Exact Pending/result and actual worker remain borrowed while the engine
    /// validates source/history. Positive preflight makes physical confirmation
    /// infallible under these same locks; failure keeps both old owners.
    pub(crate) fn with_shared_record_effect<T>(
        &self,
        joined: &Arc<JoinedSharedEffect>,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedEffect<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let step = joined.step();
        let origin = step.origin();
        joined.joined.validate_runtime(&self.shared)?;
        let peers = engine
            .validate_shared_record_effect(step)
            .map_err(std::io::Error::other)?;
        self.shared.check_shared_probe_peers(origin, &peers)?;
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, step.number(), None)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, true)?;
        calls.preflight_confirmation(
            origin.owner(),
            origin.lease(),
            step.effect(),
            joined.observed(),
        )?;
        let value = transition(
            engine,
            &ConfirmedSharedEffect {
                joined,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        calls
            .confirm(
                origin.owner(),
                origin.lease(),
                step.effect(),
                joined.observed(),
            )
            .expect("same-lock exact native result preflight before engine confirmation");
        Ok(value)
    }

    /// Publish the actual scan while its exact settled native owners remain
    /// borrowed. In particular, a ready result keeps its original pin/lease for
    /// the separate guest-output transaction; this borrower never retires it.
    pub(crate) fn with_shared_record_poll_publication<T>(
        &self,
        origin: &Arc<SharedRecordProbe>,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordPoll<'_>,
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
                    "shared probe changed append-only native effect history",
                ));
            }
            effect.joined.validate_runtime(&self.shared)?;
        }
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, effects.len() as u64, None)?;
        let calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, true)?;
        calls.preflight_shared_probe_retirement(origin, effects)?;
        let value = transition(
            engine,
            &ConfirmedSharedRecordPoll {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        Ok(value)
    }

    pub(crate) fn with_shared_record_pending<T>(
        &self,
        origin: &Arc<SharedRecordProbe>,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordPending<'_>,
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
                    "shared probe changed append-only native effect history",
                ));
            }
            effect.joined.validate_runtime(&self.shared)?;
        }
        let workers = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_shared_probe_workers(origin, &workers, effects.len() as u64, None)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_probe(&peers, origin, true)?;
        calls.preflight_shared_probe_retirement(origin, effects)?;
        let value = transition(
            engine,
            &ConfirmedSharedRecordPending {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        calls
            .finish_lease(origin.owner(), origin.lease())
            .expect("exact confirmed native lease preflight held across shared completion");
        Ok(value)
    }
}

#[cfg(test)]
impl NetworkRuntimeResources {
    /// Real selected native effect, Pending retention and JoinHandle. Only the
    /// copy5 provider rows are a controlled premise. The same submission/arm,
    /// file binding, engine and runtime validators run as in production.
    pub(crate) async fn controlled_shared_record_effect(
        &self,
        prepared: PreparedSharedEffect,
    ) -> std::io::Result<Arc<JoinedSharedEffect>> {
        self.controlled_shared_record_effect_identity(prepared.into_identity())
            .await
    }
    async fn controlled_shared_record_effect_identity(
        &self,
        step: Arc<crate::network_replay::shared_waits::SharedEffectIdentity>,
    ) -> std::io::Result<Arc<JoinedSharedEffect>> {
        self.shared.claim_shared_effect(&step)?;
        let (permit, entered) = std::sync::mpsc::channel();
        let shared = self.shared.clone();
        let selected = step.clone();
        let (worker, reply) =
            self.shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    entered
                        .recv()
                        .map_err(|_| std::io::Error::other("controlled shared effect not armed"))?;
                    let owner = selected.origin().owner();
                    let lease = selected.origin().lease();
                    let work = shared.native_streams.lock().unwrap().prepare(
                        owner,
                        lease,
                        selected.effect().clone(),
                    )?;
                    let held = work.helper.clone();
                    if let Some(held) = &held {
                        if !held.binding().matches_file(selected.origin().identity()) {
                            return Err(std::io::Error::other(
                                "controlled helper changed original file",
                            ));
                        }
                        shared
                            .shared_effect_engine(&selected)?
                            .lock()
                            .unwrap()
                            .bind_shared_helper_copy(held.binding(), &selected)
                            .map_err(std::io::Error::other)?;
                    }
                    let actual = if let Some(held) = held {
                        held.controlled_observation(work.perform(), 5)?
                    } else {
                        work.perform_control()?
                    };
                    shared.native_streams.lock().unwrap().retain(
                        owner,
                        lease,
                        selected.effect(),
                        actual.clone(),
                    )?;
                    Ok(actual)
                })?;
        self.shared.arm_shared_effect(&step, &worker)?;
        permit
            .send(())
            .map_err(|_| std::io::Error::other("controlled shared worker lost permit"))?;
        let actual = reply.await.map_err(std::io::Error::other)??;
        let joined = self.shared.join_native_worker_receipt(&worker).await?;
        if actual.helper_copy.is_some() {
            crate::network_runtime::helper_receive::retain_joined_helper(
                &self.shared,
                step.origin().owner(),
                step.origin().lease(),
                step.effect(),
                &actual,
                joined.clone(),
            )?;
        }
        Ok(Arc::new(JoinedSharedEffect {
            step,
            observed: actual,
            joined,
        }))
    }
}

impl NetworkRuntimeResources {
    /// The complete original Replay prefix is held continuously from reserve
    /// through actual store/retention/commit. The interval outlives this borrow
    /// only when retained actual failure custody requires it.
    pub(crate) fn with_shared_replay_poll_output<T>(
        &self,
        prefix: &JoinedSharedPrefix,
        lineage: &SharedForegroundLineage<'_>,
        engine: &mut NetworkReplayEngine,
        call: NetworkStreamCallId,
        reserve: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedAttemptAdmission<'_>,
        ) -> std::io::Result<
            Arc<crate::network_replay::shared_waits::SharedPollSource>,
        >,
        store: impl FnOnce(
            &mut NetworkReplayEngine,
            &Arc<crate::network_replay::shared_waits::SharedPollSource>,
            &Arc<NativeSourceInterval>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        if prefix.selected != Some(call)
            || !Arc::ptr_eq(lineage.root(), prefix.root())
            || prefix.census.rows.iter().any(|row| {
                !lineage
                    .members()
                    .any(|actual| Arc::ptr_eq(actual, &row.root))
            })
            || prefix
                .census
                .rows
                .iter()
                .find(|r| r.call == call)
                .is_none_or(|r| r.native.is_some() || r.owner != prefix.root().owner())
            || engine.mode() != crate::network_replay::NetworkEngineMode::Replay
        {
            return Err(std::io::Error::other(
                "Poll output changed selected original Replay lineage",
            ));
        }
        self.with_shared_prefix_locks(prefix, engine, |engine, workers, calls| {
            let source = reserve(
                engine,
                &SharedAttemptAdmission {
                    prefix,
                    _workers: workers,
                    _calls: calls,
                },
            )?;
            if source.call() != call
                || !Arc::ptr_eq(source.root(), prefix.root())
                || source.record_publication().is_some()
            {
                return Err(std::io::Error::other(
                    "Poll reservation changed original Replay source",
                ));
            }
            let interval = Arc::new(self.shared.reserve_shared_poll_interval(
                workers,
                prefix.prefix.generation,
                source.clone(),
                engine,
                calls,
            )?);
            store(engine, &source, &interval)
        })
    }

    /// A ready Record scan legitimately still owns its confirmed PollState
    /// lease. This exact borrower admits only that lease, never empty-map or
    /// generic settled permission. Failed actual stores retain lease+interval.
    pub(crate) fn with_shared_record_poll_output<T>(
        &self,
        publication: &Arc<crate::network_replay::shared_waits::SharedRecordPollPublication>,
        engine: &mut NetworkReplayEngine,
        reserve: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordPoll<'_>,
        ) -> std::io::Result<
            Arc<crate::network_replay::shared_waits::SharedPollSource>,
        >,
        store: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedRecordPoll<'_>,
            &Arc<crate::network_replay::shared_waits::SharedPollSource>,
            &Arc<NativeSourceInterval>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let origin = publication.source().origin();
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
                    "Poll output changed append-only worker history",
                ));
            }
            effect.joined.validate_runtime(&self.shared)?;
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
            &ConfirmedSharedRecordPoll {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )?;
        if source
            .record_publication()
            .is_none_or(|p| !Arc::ptr_eq(p, publication))
        {
            return Err(std::io::Error::other(
                "Poll output changed actual publication",
            ));
        }
        let generation = workers.submission_generation;
        let interval = Arc::new(self.shared.reserve_shared_poll_interval(
            &mut workers,
            generation,
            source.clone(),
            engine,
            &calls,
        )?);
        let value = store(
            engine,
            &ConfirmedSharedRecordPoll {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
            &source,
            &interval,
        )?;
        if !engine.shared_record_poll_output_committed(&source) {
            return Err(std::io::Error::other(
                "Poll output closure lacks exact full semantic commit",
            ));
        }
        calls
            .finish_lease(origin.owner(), origin.lease())
            .expect("actual Poll lease retirement preflight held across exact full store");
        Ok(value)
    }
}
