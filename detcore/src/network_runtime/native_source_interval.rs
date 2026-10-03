//! Replay source-read exclusion under the existing native admission owner.
//! The backend independently supplies stopped-task/MM authority and retains
//! the keepalive through actual worker join, including callback cancellation.
use std::sync::Arc;
use std::sync::Weak;

use super::*;

#[derive(Debug)]
pub(super) struct Interval {
    runtime: Weak<RuntimeShared>,
    root: Arc<ForegroundRoot>,
    generation: u64,
    admission: SourceAdmission,
}
#[derive(Debug)]
enum SourceAdmission {
    Settled,
    Shared(crate::network_replay::shared_waits::SharedCallCensus),
}

#[derive(Debug)]
pub(crate) struct NativeSourceInterval {
    interval: Arc<Interval>,
}

impl NativeSourceInterval {
    pub(crate) fn keepalive(&self) -> Box<dyn Send + Sync> {
        Box::new(self.interval.clone())
    }
}

impl RuntimeShared {
    // Only the original JoinedNativePrefix issuer calls this while holding
    // the same admission mutex; a numeric generation cannot mint an interval.
    pub(super) fn reserve_source_interval(
        self: &Arc<Self>,
        owned: &mut NativeWorkers,
        root: Arc<ForegroundRoot>,
        generation: u64,
    ) -> std::io::Result<NativeSourceInterval> {
        if owned.closed
            || owned.copy_exclusion.is_some()
            || owned.source_read_active()
            || !owned.tasks.is_empty()
            || owned.submission_generation != generation
            || !root.is_current(root.owner())
        {
            return Err(std::io::Error::other(
                "source read changed its original joined admission",
            ));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        self.native_streams.lock().unwrap().settled()?;
        let interval = Arc::new(Interval {
            runtime: Arc::downgrade(self),
            root,
            generation,
            admission: SourceAdmission::Settled,
        });
        owned.source_read = Arc::downgrade(&interval);
        Ok(NativeSourceInterval { interval })
    }
}

impl NetworkRuntimeResources {
    /// Only a synchronous semantic commit may run here. Source IO and the
    /// backend join happen before this transaction, without these locks.
    pub(crate) fn with_source_interval<T>(
        &self,
        proof: &NativeSourceInterval,
        commit: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let owned = self.shared.native_workers.lock().unwrap();
        let interval = &proof.interval;
        if !matches!(interval.admission, SourceAdmission::Settled)
            || !interval.runtime.ptr_eq(&Arc::downgrade(&self.shared))
            || owned.closed
            || owned.copy_exclusion.is_some()
            || owned
                .source_read
                .upgrade()
                .is_none_or(|actual| !Arc::ptr_eq(&actual, interval))
            || !owned.tasks.is_empty()
            || owned.submission_generation != interval.generation
            || !interval.root.is_current(interval.root.owner())
        {
            return Err(std::io::Error::other(
                "source read lost its original runtime/root/worker interval",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.settled()?;
        let result = commit();
        drop(calls);
        drop(owned);
        result
    }
}

impl RuntimeShared {
    pub(super) fn reserve_shared_source_interval(
        self: &Arc<Self>,
        owned: &mut NativeWorkers,
        root: Arc<ForegroundRoot>,
        generation: u64,
        census: crate::network_replay::shared_waits::SharedCallCensus,
        calls: &native_peer::Calls,
    ) -> std::io::Result<NativeSourceInterval> {
        if owned.closed
            || owned.copy_exclusion.is_some()
            || owned.source_read_active()
            || !owned.tasks.is_empty()
            || owned.submission_generation != generation
            || !root.is_current(root.owner())
            || !root.has_shared_mm_history()
        {
            return Err(std::io::Error::other(
                "shared source changed joined native admission",
            ));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        calls.require_shared_quiescence(&census)?;
        let interval = Arc::new(Interval {
            runtime: Arc::downgrade(self),
            root,
            generation,
            admission: SourceAdmission::Shared(census),
        });
        owned.source_read = Arc::downgrade(&interval);
        Ok(NativeSourceInterval { interval })
    }
}
impl NetworkRuntimeResources {
    /// The selected TX Call is validated by its exact semantic consumer. Every
    /// other engine/native Call must still be the original suspended census;
    /// generic source commit cannot consume this distinct admission.
    pub(crate) fn with_shared_source_interval<T>(
        &self,
        proof: &NativeSourceInterval,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        selected: crate::network_replay::NetworkStreamCallId,
        commit: impl FnOnce(&mut crate::network_replay::NetworkReplayEngine) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let owned = self.shared.native_workers.lock().unwrap();
        let interval = &proof.interval;
        let SourceAdmission::Shared(original) = &interval.admission else {
            return Err(std::io::Error::other(
                "shared commit lacks original shared interval",
            ));
        };
        if !interval.runtime.ptr_eq(&Arc::downgrade(&self.shared))
            || owned.closed
            || owned.copy_exclusion.is_some()
            || owned
                .source_read
                .upgrade()
                .is_none_or(|actual| !Arc::ptr_eq(&actual, interval))
            || !owned.tasks.is_empty()
            || owned.submission_generation != interval.generation
            || !interval.root.is_current(interval.root.owner())
            || !interval.root.has_shared_mm_history()
        {
            return Err(std::io::Error::other(
                "shared source lost original runtime/root/worker interval",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        let current = engine
            .shared_call_census_excluding(None, Some(selected))
            .map_err(std::io::Error::other)?;
        if !original.same_rows(&current) {
            return Err(std::io::Error::other(
                "shared source changed suspended peer census",
            ));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_quiescence(&current)?;
        let result = commit(engine);
        drop(calls);
        drop(owned);
        result
    }
}

#[cfg(test)]
#[path = "native_source_interval/tests.rs"]
mod tests;
