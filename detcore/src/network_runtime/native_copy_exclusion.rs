//! Bounded local guest stores exclude new native submissions at their actual
//! admission mutex. The engine separately owns selected bytes and store outcome.
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use super::*;
use crate::network_replay::ForegroundStoreSource;

const ACTIVE: u8 = 0;
const FINISHED: u8 = 1;
const ABANDONED_AFTER_BACKEND: u8 = 2;

#[derive(Debug)]
pub(super) struct Interval {
    runtime: Weak<RuntimeShared>,
    root: Arc<ForegroundRoot>,
    source: ForegroundStoreSource,
    submission_generation: u64,
    phase: AtomicU8,
}
impl Interval {
    pub(super) fn active(&self) -> bool {
        self.phase.load(Ordering::Acquire) == ACTIVE
    }
}

/// An opaque exclusion interval, not a mapping or semantic copy grant. Dropping
/// a callback never releases it: the existing NativeWorkers owner retains it.
#[derive(Debug, Clone)]
pub(crate) struct NativeCopyExclusion {
    interval: Arc<Interval>,
}

impl NativeCopyExclusion {
    pub(crate) fn matches_source(
        &self,
        root: &Arc<ForegroundRoot>,
        source: &ForegroundStoreSource,
    ) -> bool {
        Arc::ptr_eq(&self.interval.root, root) && self.interval.source.same(source)
    }
}

impl NetworkRuntimeResources {
    /// Cleanup joins are never entry authority and cannot refresh a failed
    /// retry prefix. They only expose this actual Call's retained close status.
    pub(crate) async fn join_receive_retry_release(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<()> {
        let (generation, workers) = {
            let owned = self.shared.native_workers.lock().unwrap();
            if owned.closed || owned.copy_exclusion.is_some() {
                return Err(std::io::Error::other(
                    "retry cleanup lacks open native admission",
                ));
            }
            (owned.submission_generation, owned.tasks.clone())
        };
        for worker in workers {
            self.shared.join_native_worker(&worker).await?;
        }
        let owned = self.shared.native_workers.lock().unwrap();
        if owned.closed
            || owned.copy_exclusion.is_some()
            || !owned.tasks.is_empty()
            || owned.submission_generation != generation
        {
            return Err(std::io::Error::other(
                "retry cleanup changed its actually joined workers",
            ));
        }
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .receive_retry_release_known(owner, call)?;
        Ok(())
    }
    pub(crate) fn receive_retry_release_known(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<bool> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .receive_retry_release_known(owner, call)
    }

    #[cfg(test)]
    pub(crate) async fn controlled_join_retry_workers(&self) {
        let workers = self.shared.native_workers.lock().unwrap().tasks.clone();
        for worker in workers {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                self.shared.join_native_worker(&worker),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }

    #[cfg(test)]
    pub(crate) async fn controlled_foreground_store_worker(&self) -> std::sync::mpsc::Sender<()> {
        let (release, wait) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (_worker, reply) = self
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                entered.send(()).unwrap();
                wait.recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap();
                Ok(())
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        drop(reply);
        release
    }

    #[cfg(test)]
    pub(crate) fn retain_foreground_store_fixture_publication(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        engine: Arc<Mutex<crate::network_replay::NetworkReplayEngine>>,
    ) {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .retain_capture_publication(
                owner,
                call,
                NativeCaptureRecovery::new(engine, Arc::new(tokio::sync::Notify::new()), |_| {}),
            )
            .unwrap();
    }

    #[cfg(test)]
    pub(crate) async fn exclude_native_for_copy(
        &self,
        root: Arc<ForegroundRoot>,
        completion: HelperCopyCompletion,
    ) -> std::io::Result<NativeCopyExclusion> {
        self.exclude_native_for_store(root, ForegroundStoreSource::Record(completion))
            .await
    }

    pub(crate) async fn exclude_native_for_store(
        &self,
        root: Arc<ForegroundRoot>,
        source: ForegroundStoreSource,
    ) -> std::io::Result<NativeCopyExclusion> {
        let owner = source.owner();
        // Obtain the actual registered root before acquiring native_workers.
        // Revocation is irreversible and is checked again at atomic admission.
        let actual_root = self.foreground_root(owner)?;
        if !Arc::ptr_eq(&root, &actual_root) {
            return Err(std::io::Error::other(
                "copy exclusion changed its actual foreground root",
            ));
        }
        if let ForegroundStoreSource::Record(completion) = &source {
            completion.joined_worker()?.validate_runtime(&self.shared)?;
        }
        let (generation, workers) = {
            let owned = self.shared.native_workers.lock().unwrap();
            self.shared
                .check_copy_exclusion_preparation(&owned, &root, &source)?;
            (owned.submission_generation, owned.tasks.clone())
        };
        // Actual retained handles include destructors after dispensable replies.
        // There is no engine, native admission, or Calls lock across this await.
        for worker in workers {
            self.shared.join_native_worker(&worker).await?;
        }
        let mut owned = self.shared.native_workers.lock().unwrap();
        self.shared
            .check_copy_exclusion_preparation(&owned, &root, &source)?;
        if owned.submission_generation != generation || !owned.tasks.is_empty() {
            return Err(std::io::Error::other(
                "copy exclusion changed its actually joined native prefix",
            ));
        }
        let interval = Arc::new(Interval {
            runtime: Arc::downgrade(&self.shared),
            root,
            source,
            submission_generation: generation,
            phase: AtomicU8::new(ACTIVE),
        });
        // This is the same mutex as spawn and actual JoinHandle insertion.
        owned.copy_exclusion = Some(interval.clone());
        Ok(NativeCopyExclusion { interval })
    }

    pub(crate) fn validate_native_copy_exclusion(
        &self,
        proof: &NativeCopyExclusion,
    ) -> std::io::Result<()> {
        let owned = self.shared.native_workers.lock().unwrap();
        self.shared.check_active_copy_exclusion(&owned, proof)
    }

    /// The caller must first retain the exact synchronous store outcome in its
    /// existing engine Call. This method certifies only end of exclusion, never
    /// a successful store, stream consumption, or permission for a later copy.
    pub(crate) fn finish_native_copy_exclusion(
        &self,
        proof: &NativeCopyExclusion,
    ) -> std::io::Result<()> {
        let mut owned = self.shared.native_workers.lock().unwrap();
        self.shared.check_active_copy_exclusion(&owned, proof)?;
        proof.interval.phase.store(FINISHED, Ordering::Release);
        owned.copy_exclusion = None;
        Ok(())
    }

    /// Replay has no Drain successor. Keep actual native admission and Calls
    /// locked through its semantic commit, so ending the store interval cannot
    /// hide an intervening native submission or newly retained Call.
    pub(crate) fn with_ended_replay_copy<T>(
        &self,
        proof: &NativeCopyExclusion,
        source: &ForegroundStoreSource,
        commit: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let owned = self.shared.native_workers.lock().unwrap();
        let interval = &proof.interval;
        if !matches!(source, ForegroundStoreSource::Replay(_))
            || !interval.source.same(source)
            || !interval.runtime.ptr_eq(&Arc::downgrade(&self.shared))
            || interval.phase.load(Ordering::Acquire) != FINISHED
            || owned.closed
            || owned.copy_exclusion.is_some()
            || !owned.tasks.is_empty()
            || owned.submission_generation != interval.submission_generation
            || !interval.root.is_current(source.owner())
        {
            return Err(std::io::Error::other(
                "Replay commit changed its ended store or native submission prefix",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.settled()?;
        // The caller already holds scheduler/MM/engine. This callback is only
        // the prevalidated queue transaction: no await, syscall or submission.
        let result = commit();
        drop(calls);
        result
    }
}

impl RuntimeShared {
    fn check_copy_exclusion_preparation(
        &self,
        owned: &NativeWorkers,
        root: &Arc<ForegroundRoot>,
        source: &ForegroundStoreSource,
    ) -> std::io::Result<()> {
        if owned.closed || owned.copy_exclusion.is_some() || owned.source_read_active() || !root.is_current(source.owner()) {
            return Err(std::io::Error::other(
                "copy exclusion lacks open exact-root native admission",
            ));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        self.check_copy_source_quiescence(source)
    }

    fn check_active_copy_exclusion(
        self: &Arc<Self>,
        owned: &NativeWorkers,
        proof: &NativeCopyExclusion,
    ) -> std::io::Result<()> {
        let interval = &proof.interval;
        if !interval.runtime.ptr_eq(&Arc::downgrade(self))
            || owned.closed
            || !interval.active()
            || owned
                .copy_exclusion
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, interval))
            || owned.submission_generation != interval.submission_generation
            || !owned.tasks.is_empty()
            || !interval.root.is_current(interval.source.owner())
        {
            return Err(std::io::Error::other(
                "native copy exclusion is stale or changed its actual owner",
            ));
        }
        if let ForegroundStoreSource::Record(completion) = &interval.source {
            completion.joined_worker()?.validate_runtime(self)?;
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        self.check_copy_source_quiescence(&interval.source)
    }

    fn check_copy_source_quiescence(&self, source: &ForegroundStoreSource) -> std::io::Result<()> {
        let calls = self.native_streams.lock().unwrap();
        match source {
            ForegroundStoreSource::Record(completion) => calls.require_copy_quiescence(completion),
            // Replay has no native helper or capture Call. Requiring the actual
            // owner empty is stronger than selecting an allegedly harmless row.
            ForegroundStoreSource::Replay(_) => calls.settled(),
        }
    }

    // Only finish_native_after_backend calls this. The synchronous local copy
    // runs inside the actual backend-owned handler and cannot continue after
    // that future/LocalSet has ended. This is not callable by an ordinary RPC.
    pub(super) fn abandon_copy_exclusion_after_backend(&self) {
        let mut owned = self.native_workers.lock().unwrap();
        owned.closed = true;
        if let Some(copy) = owned.copy_exclusion.as_ref().filter(|copy| copy.active()) {
            copy.phase.store(ABANDONED_AFTER_BACKEND, Ordering::Release);
            self.native_terminal_failure
                .lock()
                .unwrap()
                .get_or_insert_with(|| {
                    "backend ended with an unresolved guest-copy interval; possible stores retained"
                        .into()
                });
            // Keep the exact root, source Completion and failed interval in the
            // existing admission owner while typed physical cleanup proceeds.
        }
    }
}

#[cfg(test)]
#[path = "native_copy_exclusion/tests.rs"]
mod tests;

#[derive(Debug, Clone)]
pub(super) struct NoStoreJoinOrigin {
    runtime: Weak<RuntimeShared>,
    root: Arc<ForegroundRoot>,
    submission_generation: u64,
}
impl NoStoreJoinOrigin {
    pub(super) fn same(&self, other: &Self) -> bool {
        self.runtime.ptr_eq(&other.runtime)
            && Arc::ptr_eq(&self.root, &other.root)
            && self.submission_generation == other.submission_generation
    }
}

/// A joined prefix for one live canonical no-store Call, distinct from both
/// Calls::settled and an interval that permits guest writes. Not serializable.
#[derive(Debug)]
pub(crate) struct JoinedNoStorePrefix {
    origin: NoStoreJoinOrigin,
    source: Arc<crate::network_replay::RecordNoStore>,
}
impl NetworkRuntimeResources {
    pub(crate) async fn join_record_no_store(
        &self,
        root: Arc<ForegroundRoot>,
        source: Arc<crate::network_replay::RecordNoStore>,
    ) -> std::io::Result<JoinedNoStorePrefix> {
        let completion = source.completion();
        if !Arc::ptr_eq(&root, &self.foreground_root(completion.binding().owner())?) {
            return Err(std::io::Error::other(
                "no-store changed actual registered root",
            ));
        }
        completion.joined_worker()?.validate_runtime(&self.shared)?;
        let (generation, workers) = {
            let owned = self.shared.native_workers.lock().unwrap();
            self.shared.check_no_store_prefix(&owned, &root, &source)?;
            let origin = NoStoreJoinOrigin {
                runtime: Arc::downgrade(&self.shared),
                root: root.clone(),
                submission_generation: owned.submission_generation,
            };
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .retain_no_store_join(&source, &origin)?;
            (owned.submission_generation, owned.tasks.clone())
        };
        for worker in workers {
            self.shared.join_native_worker(&worker).await?;
        }
        let owned = self.shared.native_workers.lock().unwrap();
        self.shared.check_no_store_prefix(&owned, &root, &source)?;
        if owned.submission_generation != generation || !owned.tasks.is_empty() {
            return Err(std::io::Error::other(
                "no-store changed its actually joined native prefix",
            ));
        }
        Ok(JoinedNoStorePrefix {
            source,
            origin: NoStoreJoinOrigin {
                runtime: Arc::downgrade(&self.shared),
                root,
                submission_generation: generation,
            },
        })
    }

    pub(crate) fn complete_record_no_store(
        &self,
        joined: JoinedNoStorePrefix,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        epoch: u64,
        now: detcore_model::time::LogicalTime,
    ) -> std::io::Result<crate::network_replay::CompletedNoStore> {
        let owned = self.shared.native_workers.lock().unwrap();
        if !joined.origin.runtime.ptr_eq(&Arc::downgrade(&self.shared))
            || owned.submission_generation != joined.origin.submission_generation
            || !owned.tasks.is_empty()
        {
            return Err(std::io::Error::other(
                "no-store commit changed the joined native submission prefix",
            ));
        }
        self.shared
            .check_no_store_prefix(&owned, &joined.origin.root, &joined.source)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        let confirmed = calls.confirmed_no_store(&joined.source, &joined.origin)?;
        let result = engine
            .commit_record_no_store(&joined.source, &confirmed, &joined.origin.root, epoch, now)
            .map_err(std::io::Error::other)?;
        // The private typed commit succeeded while both owners and admission
        // stayed locked. No arbitrary engine Err acknowledges physical work.
        calls.retire_completed_no_store(&joined.source);
        Ok(result)
    }
}
impl RuntimeShared {
    fn check_no_store_prefix(
        &self,
        owned: &NativeWorkers,
        root: &Arc<ForegroundRoot>,
        source: &Arc<crate::network_replay::RecordNoStore>,
    ) -> std::io::Result<()> {
        if owned.closed
            || owned.copy_exclusion.is_some()
            || !root.is_current(source.completion().binding().owner())
        {
            return Err(std::io::Error::other(
                "no-store requires exact open native admission",
            ));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        self.native_streams
            .lock()
            .unwrap()
            .require_copy_quiescence(source.completion())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReceiveRetryOrigin {
    runtime: Weak<RuntimeShared>,
    root: Arc<ForegroundRoot>,
    submission_generation: u64,
}
impl ReceiveRetryOrigin {
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.runtime.ptr_eq(&other.runtime)
            && Arc::ptr_eq(&self.root, &other.root)
            && self.submission_generation == other.submission_generation
    }
}

/// Actual joined workers for the one completed-empty but still-pinned Call.
#[derive(Debug)]
pub(crate) struct JoinedReceiveRetryPrefix {
    origin: ReceiveRetryOrigin,
    source: Arc<crate::network_replay::RecordNoStore>,
}

#[cfg(test)]
pub(crate) struct ControlledRetryPinWorker {
    shared: Arc<RuntimeShared>,
    worker: super::NativeWorkerHandle,
}
#[cfg(test)]
impl ControlledRetryPinWorker {
    pub(crate) async fn join(self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.shared.join_native_worker(&self.worker),
        )
        .await
        .unwrap()
        .unwrap();
    }
}
#[cfg(test)]
impl NetworkRuntimeResources {
    pub(crate) fn controlled_retry_pin_count(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> usize {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .retry_fixture_pin_count(owner, call)
    }

    pub(crate) async fn controlled_retry_pin_worker(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> (std::sync::mpsc::Sender<()>, ControlledRetryPinWorker) {
        let original = self
            .shared
            .native_streams
            .lock()
            .unwrap()
            .retry_fixture_pin(owner, call);
        let (release, wait) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (worker, reply) = self
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                entered.send(()).unwrap();
                wait.recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap();
                drop(original);
                Ok(())
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        drop(reply);
        (
            release,
            ControlledRetryPinWorker {
                shared: self.shared.clone(),
                worker,
            },
        )
    }
}

/// Private construction holds both worker admission and the physical Calls
/// ledger through the engine commit. Neither numeric Call nor errno issues it.
pub(crate) struct ReceiveRetryAdmission<'a> {
    root: &'a Arc<ForegroundRoot>,
    source: &'a Arc<crate::network_replay::RecordNoStore>,
    _calls: &'a super::native_peer::Calls,
}
impl ReceiveRetryAdmission<'_> {
    pub(crate) fn matches(&self, retry: &crate::network_replay::RecordReceiveRetry) -> bool {
        Arc::ptr_eq(self.root, retry.root()) && Arc::ptr_eq(self.source, retry.source())
    }
}

impl NetworkRuntimeResources {
    pub(crate) async fn join_receive_retry_prefix(
        &self,
        retry: &crate::network_replay::RecordReceiveRetry,
    ) -> std::io::Result<JoinedReceiveRetryPrefix> {
        let work = async {
            retry.check().map_err(std::io::Error::other)?;
            if !Arc::ptr_eq(retry.root(), &self.foreground_root(retry.owner())?) {
                return Err(std::io::Error::other(
                    "receive retry changed actual registered root",
                ));
            }
            retry
                .source()
                .completion()
                .joined_worker()?
                .validate_runtime(&self.shared)?;
            let (origin, workers) = {
                let owned = self.shared.native_workers.lock().unwrap();
                let origin = ReceiveRetryOrigin {
                    runtime: Arc::downgrade(&self.shared),
                    root: retry.root().clone(),
                    submission_generation: owned.submission_generation,
                };
                retry
                    .retain_origin(origin.clone())
                    .map_err(std::io::Error::other)?;
                self.shared.check_receive_retry_prefix(&owned, retry)?;
                (origin, owned.tasks.clone())
            };
            for worker in workers {
                self.shared.join_native_worker(&worker).await?;
            }
            let owned = self.shared.native_workers.lock().unwrap();
            self.shared.check_receive_retry_prefix(&owned, retry)?;
            if owned.submission_generation != origin.submission_generation
                || !owned.tasks.is_empty()
            {
                return Err(std::io::Error::other(
                    "receive retry changed its actually joined native prefix",
                ));
            }
            Ok(JoinedReceiveRetryPrefix {
                origin,
                source: retry.source().clone(),
            })
        }
        .await;
        work.map_err(|error: std::io::Error| std::io::Error::other(retry.fail(error)))
    }

    pub(crate) fn with_receive_retry_prefix<T>(
        &self,
        joined: &JoinedReceiveRetryPrefix,
        retry: &crate::network_replay::RecordReceiveRetry,
        commit: impl FnOnce(&ReceiveRetryAdmission<'_>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let work = || {
            retry.check().map_err(std::io::Error::other)?;
            let owned = self.shared.native_workers.lock().unwrap();
            if !joined.origin.runtime.ptr_eq(&Arc::downgrade(&self.shared))
                || !Arc::ptr_eq(&joined.source, retry.source())
                || !Arc::ptr_eq(&joined.origin.root, retry.root())
                || !retry.matches_origin(&joined.origin)
                || owned.submission_generation != joined.origin.submission_generation
                || !owned.tasks.is_empty()
            {
                return Err(std::io::Error::other(
                    "receive retry changed its retained joined prefix",
                ));
            }
            self.shared.check_receive_retry_prefix(&owned, retry)?;
            let calls = self.shared.native_streams.lock().unwrap();
            calls.require_receive_retry_quiescence(retry.source())?;
            commit(&ReceiveRetryAdmission {
                root: &joined.origin.root,
                source: &joined.source,
                _calls: &calls,
            })
        };
        work().map_err(|error: std::io::Error| std::io::Error::other(retry.fail(error)))
    }
}
impl RuntimeShared {
    fn check_receive_retry_prefix(
        &self,
        owned: &NativeWorkers,
        retry: &crate::network_replay::RecordReceiveRetry,
    ) -> std::io::Result<()> {
        retry.check().map_err(std::io::Error::other)?;
        if owned.closed || owned.copy_exclusion.is_some() || !retry.root().is_current(retry.owner())
        {
            return Err(std::io::Error::other(
                "receive retry requires exact open native admission",
            ));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        self.native_streams
            .lock()
            .unwrap()
            .require_receive_retry_quiescence(retry.source())
    }
}
