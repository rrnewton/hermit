//! Borrow existing execution and Call owners together. No serialized ID or
//! phase flag issues native retirement; fixed worker handles are actually joined.
use std::sync::Arc;

use super::*;
use crate::network_replay::NetworkReplayEngine;
use crate::network_replay::NetworkReplayError;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::shared_waits::SharedCallCensus;

#[derive(Debug, Clone)]
pub(crate) struct JoinedSharedPrefix {
    prefix: JoinedNativePrefix,
    census: SharedCallCensus,
    selected: Option<NetworkStreamCallId>,
}
impl JoinedSharedPrefix {
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        self.prefix.root()
    }
    pub(crate) fn same_prefix(&self, other: &Self) -> bool {
        self.prefix.same_prefix(&other.prefix)
            && self.selected == other.selected
            && self.census.same(&other.census)
    }
    pub(crate) fn matches_retained_peers(
        &self,
        engine: &NetworkReplayEngine,
        exclude: Option<NetworkStreamCallId>,
    ) -> Result<bool, NetworkReplayError> {
        Ok(self.selected.is_none()
            && self
                .census
                .same_rows(&engine.shared_call_census_excluding(None, exclude)?))
    }
}
pub(crate) struct SharedAttemptAdmission<'a> {
    prefix: &'a JoinedSharedPrefix,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl SharedAttemptAdmission<'_> {
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        self.prefix.prefix.root()
    }
    pub(crate) fn is_original_prefix(&self, prefix: &JoinedSharedPrefix) -> bool {
        self.prefix.same_prefix(prefix)
    }
    pub(crate) fn matches_peers(
        &self,
        engine: &NetworkReplayEngine,
        exclude: Option<NetworkStreamCallId>,
    ) -> Result<bool, NetworkReplayError> {
        if self.prefix.selected.is_some() {
            return Ok(false);
        }
        let actual = engine.shared_call_census_excluding(None, exclude)?;
        Ok(self.prefix.census.same_rows(&actual))
    }
    pub(crate) fn matches_selected(
        &self,
        engine: &NetworkReplayEngine,
        call: NetworkStreamCallId,
    ) -> Result<bool, NetworkReplayError> {
        Ok(self.prefix.selected == Some(call)
            && self
                .prefix
                .census
                .same(&engine.shared_call_census(Some(call))?))
    }
}
impl NetworkRuntimeResources {
    /// No scheduler, physical or engine mutex is retained across these awaits.
    /// After actual joins, compare the same original census and generation;
    /// never enroll new/replacement work into an earlier prefix.
    pub(crate) async fn join_shared_foreground_prefix(
        &self,
        root: Arc<ForegroundRoot>,
        engine: &Mutex<NetworkReplayEngine>,
        selected: Option<NetworkStreamCallId>,
    ) -> std::io::Result<JoinedSharedPrefix> {
        let (generation, workers, census) = {
            let engine = engine.lock().unwrap();
            let census = engine
                .shared_call_census(selected)
                .map_err(std::io::Error::other)?;
            let owned = self.shared.native_workers.lock().unwrap();
            if owned.closed
                || owned.copy_exclusion.is_some()
                || owned.source_read_active()
                || !root.is_current(root.owner())
                || !root.has_shared_mm_history()
                || census
                    .rows
                    .iter()
                    .any(|row| !row.root.same_shared_lineage(&root))
            {
                return Err(std::io::Error::other(
                    "shared prefix lost its live original admission",
                ));
            }
            if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
                return Err(std::io::Error::other(error.clone()));
            }
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .require_shared_quiescence(&census)?;
            (owned.submission_generation, owned.tasks.clone(), census)
        };
        for worker in workers {
            self.shared.join_native_worker(&worker).await?;
        }
        let prefix = JoinedSharedPrefix {
            prefix: JoinedNativePrefix {
                shared: Arc::downgrade(&self.shared),
                root,
                generation,
            },
            census,
            selected,
        };
        let mut engine = engine.lock().unwrap();
        self.with_shared_attempt_prefix(&prefix, &mut engine, |_, _| Ok(()))?;
        Ok(prefix)
    }

    /// Caller owns current scheduler→physical→metadata/engine custody. Native
    /// workers and Calls remain locked through the complete state transition.
    pub(crate) fn with_shared_attempt_prefix<T>(
        &self,
        prefix: &JoinedSharedPrefix,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedAttemptAdmission<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        self.with_shared_prefix_locks(prefix, engine, |engine, owned, calls| {
            transition(
                engine,
                &SharedAttemptAdmission {
                    prefix,
                    _workers: owned,
                    _calls: calls,
                },
            )
        })
    }

    /// Reserve a source interval while the same complete suspended census and
    /// physical lineage remain borrowed. A refused transfer drops only the
    /// known-unsubmitted reservation; it never disarms an already submitted Call.
    pub(crate) fn prepare_shared_replay_source<T>(
        &self,
        prefix: &JoinedSharedPrefix,
        lineage: &SharedForegroundLineage<'_>,
        engine: &mut NetworkReplayEngine,
        transfer: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedAttemptAdmission<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<(T, NativeSourceInterval)> {
        if prefix.selected.is_some()
            || !Arc::ptr_eq(lineage.root(), prefix.root())
            || prefix.census.rows.iter().any(|row| {
                !lineage
                    .members()
                    .any(|actual| Arc::ptr_eq(actual, &row.root))
            })
        {
            return Err(std::io::Error::other(
                "shared source changed complete original lineage",
            ));
        }
        self.with_shared_prefix_locks(prefix, engine, |engine, owned, calls| {
            let interval = self.shared.reserve_shared_source_interval(
                owned,
                prefix.root().clone(),
                prefix.prefix.generation,
                prefix.census.clone(),
                calls,
            )?;
            let value = transfer(
                engine,
                &SharedAttemptAdmission {
                    prefix,
                    _workers: owned,
                    _calls: calls,
                },
            )?;
            Ok((value, interval))
        })
    }

    fn with_shared_prefix_locks<T>(
        &self,
        prefix: &JoinedSharedPrefix,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &mut NativeWorkers,
            &native_peer::Calls,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let census = engine
            .shared_call_census(prefix.selected)
            .map_err(std::io::Error::other)?;
        let mut owned = self.shared.native_workers.lock().unwrap();
        if !prefix.prefix.shared.ptr_eq(&Arc::downgrade(&self.shared))
            || owned.closed
            || owned.copy_exclusion.is_some()
            || owned.source_read_active()
            || !owned.tasks.is_empty()
            || owned.submission_generation != prefix.prefix.generation
            || !prefix.census.same(&census)
            || !prefix.prefix.root.is_current(prefix.prefix.root.owner())
            || !prefix.prefix.root.has_shared_mm_history()
            || census
                .rows
                .iter()
                .any(|row| !row.root.same_shared_lineage(&prefix.prefix.root))
        {
            return Err(std::io::Error::other(
                "shared attempt changed its joined original prefix",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_quiescence(&census)?;
        transition(engine, &mut owned, &calls)
    }
}

#[cfg(test)]
impl NetworkRuntimeResources {
    pub(crate) fn controlled_shared_unknown_capture(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .capture_failed(owner, call, libc::EBADF)
            .unwrap();
    }
}
