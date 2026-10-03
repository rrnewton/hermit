//! Borrowed complete history for the actual initial-parent terminal callback.
use std::collections::BTreeSet;
use std::sync::Arc;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::native_birth_outcome::NativeTaskProjection;

pub(crate) struct SharedInitialTerminalHistory<'a> {
    _scheduler: &'a Scheduler,
    initial: Arc<NativeTaskProjection>,
    children: Vec<Arc<NativeTaskProjection>>,
}
impl SharedInitialTerminalHistory<'_> {
    pub(crate) fn initial(&self) -> &Arc<NativeTaskProjection> {
        &self.initial
    }
    pub(crate) fn children(&self) -> &[Arc<NativeTaskProjection>] {
        &self.children
    }
}
impl Scheduler {
    pub(crate) fn shared_initial_projection(
        &self,
        root: &Arc<ForegroundRoot>,
    ) -> std::io::Result<Arc<NativeTaskProjection>> {
        self.validate_native_initial_root(root.owner(), root)?;
        let entry = self
            .thread_tree
            .process_wait
            .get(&root.logical_process())
            .ok_or_else(|| std::io::Error::other("initial projection process missing"))?;
        let p = &entry.native_projections[0];
        if !p.matches_initial_root(root) {
            return Err(std::io::Error::other(
                "initial projection changed original root",
            ));
        }
        Ok(p.clone())
    }

    pub(crate) fn shared_initial_terminal_history<'a>(
        &'a self,
        owner: NetworkStreamOwner,
        root: &Arc<ForegroundRoot>,
        initial: &Arc<NativeTaskProjection>,
    ) -> std::io::Result<SharedInitialTerminalHistory<'a>> {
        let bad =
            || std::io::Error::other("initial final wait lacks complete retained child history");
        let entry = self
            .thread_tree
            .process_wait
            .get(&root.logical_process())
            .ok_or_else(bad)?;
        if root.owner() != owner
            || !initial.matches_initial_root(root)
            || self.thread_tree.root != Some(owner.thread)
            || self.thread_tree.process_wait.len() != 1
            || entry.reaped
            || entry.wait_parent.is_some()
            || entry.wait_owner != owner.thread
            || entry.native_birth_parent.is_some()
            || !entry.births.is_empty()
            || entry
                .historical_births
                .iter()
                .any(|birth| !birth.complete())
        {
            return Err(bad());
        }
        let mut seen = BTreeSet::new();
        let mut found = false;
        let mut children = Vec::new();
        for p in &entry.native_projections {
            let tid = p.thread();
            if !seen.insert(tid) || !p.same_process(initial) {
                return Err(bad());
            }
            if tid == owner.thread {
                if !Arc::ptr_eq(p, initial) {
                    return Err(bad());
                }
                found = true;
                continue;
            }
            if p.completed_child_for_initial_finalization(root).is_none()
                || self.physical_thread_pidfds.contains_key(&tid)
                || self.next_turns.contains_key(&tid)
                || !matches!(self.thread_status(tid), super::super::ThreadStatus::Gone)
                || self.pending_run_queue_removals.contains_key(&tid)
                || self.pending_run_queue_admissions.contains_key(&tid)
                || self.pending_cross_task_signals.contains_key(&tid)
                || self.network_capture_blockers.contains_key(&tid)
                || self.replay_connect.contains_key(&tid)
                || self.blocked.timed_out_futex_waiters.contains(&tid)
                || self.blocked.physical_child_ready.contains(&tid)
                || self.blocked.sigchld_deferred.contains(&tid)
                || self.blocked.sigchld_ready.contains(&tid)
                || self.blocked.zero_stream_waiters.contains_key(&tid)
            {
                return Err(bad());
            }
            children.push(p.clone());
        }
        if !found || seen != self.thread_tree.tree.keys().copied().collect() {
            return Err(bad());
        }
        Ok(SharedInitialTerminalHistory {
            _scheduler: self,
            initial: initial.clone(),
            children,
        })
    }
}

#[cfg(test)]
impl Scheduler {
    pub(crate) fn controlled_remove_shared_initial_history(&mut self, tid: crate::types::DetTid) {
        assert!(self.thread_tree.tree.remove(&tid).is_some());
    }
}
