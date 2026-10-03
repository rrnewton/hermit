//! Input-only shared interval: no selected Call may be excluded on completion.
use super::*;

impl RuntimeShared {
    pub(in crate::network_runtime) fn reserve_shared_poll_input_interval(
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
                "Poll input lost joined native admission",
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
            admission: SourceAdmission::SharedPollInput(census),
        });
        owned.source_read = Arc::downgrade(&interval);
        Ok(NativeSourceInterval { interval })
    }
}

impl NetworkRuntimeResources {
    /// Caller holds current scheduler, physical lineage, metadata and engine.
    /// Validate the complete unchanged census after the real backend join.
    pub(crate) fn with_shared_poll_input_interval<T>(
        &self,
        proof: &NativeSourceInterval,
        lineage: &SharedForegroundLineage<'_>,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        observed: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let interval = &proof.interval;
        let SourceAdmission::SharedPollInput(original) = &interval.admission else {
            return Err(std::io::Error::other(
                "Poll input requires its own interval",
            ));
        };
        let owned = self.shared.native_workers.lock().unwrap();
        if !Arc::ptr_eq(&interval.root, lineage.root())
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
            || !interval.root.has_shared_mm_history()
            || original.rows.iter().any(|row| {
                !lineage
                    .members()
                    .any(|member| Arc::ptr_eq(member, &row.root))
            })
        {
            return Err(std::io::Error::other(
                "Poll input changed original interval/lineage",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        let current = engine
            .shared_call_census(None)
            .map_err(std::io::Error::other)?;
        if !original.same(&current) {
            return Err(std::io::Error::other(
                "Poll input changed complete original census",
            ));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_quiescence(&current)?;
        let result = observed();
        drop(calls);
        drop(owned);
        result
    }
}
