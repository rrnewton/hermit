//! The original parent Task survives both consuming cleanup and actual final wait.
use std::sync::Arc;

use super::*;
use crate::network_runtime::native_birth_outcome::NativeTaskProjection;
use crate::scheduler::ordinary_fd::shared_initial::SharedInitialTerminalHistory;

impl<T> CustodyTasks<T> {
    pub(super) fn forget_shared_initial(
        &mut self,
        owner: NetworkStreamOwner,
    ) -> std::io::Result<()> {
        let task = self
            .tasks
            .get_mut(&owner.thread)
            .ok_or_else(|| std::io::Error::other("initial cleanup lost physical owner"))?;
        let root = task
            .foreground_root
            .as_ref()
            .ok_or_else(|| std::io::Error::other("initial cleanup lost original root"))?;
        if task.mm != owner.mm
            || root.owner() != owner
            || root.is_shared_child()
            || !root.has_shared_mm_history()
        {
            return Err(std::io::Error::other(
                "initial cleanup changed original custody",
            ));
        }
        root.revoke();
        task.retired = true;
        task.shared_cleanup_requested = true;
        self.retire_completed_shared_child(owner)
    }

    pub(in crate::network_runtime) fn prepare_shared_initial_final_wait(
        &mut self,
        owner: NetworkStreamOwner,
        expected: &Arc<ForegroundRoot>,
        history: &SharedInitialTerminalHistory<'_>,
    ) -> std::io::Result<Arc<ForegroundRoot>> {
        let bad = || std::io::Error::other("initial final wait lost original physical custody");
        if self.foreground_lineage_lost
            || !history.initial().matches_initial_root(expected)
            || expected.owner() != owner
            || expected.is_shared_child()
        {
            return Err(bad());
        }
        if let Some(task) = self.tasks.get(&owner.thread) {
            if task.mm != owner.mm
                || task.process != expected.process()
                || task.thread != expected.thread()
                || task.native_birth.is_some()
                || task.initial_exec.is_none()
                || task
                    .foreground_root
                    .as_ref()
                    .is_none_or(|r| !Arc::ptr_eq(r, expected))
            {
                return Err(bad());
            }
        } else if !history.initial().completed_initial_final_wait(expected) {
            // An exact repeat may inspect the prior positive fact. No absent
            // entry can create the first terminal issuer.
            return Err(bad());
        }
        for child in history.children() {
            let root = child
                .completed_child_for_initial_finalization(expected)
                .ok_or_else(bad)?;
            if self.tasks.get(&child.thread()).is_some_and(|task| {
                !task.retired
                    || task.mm != root.owner().mm
                    || task
                        .shared_terminal
                        .as_ref()
                        .is_none_or(|p| !Arc::ptr_eq(p, child))
                    || task
                        .foreground_root
                        .as_ref()
                        .is_none_or(|r| !Arc::ptr_eq(r, root))
                    || task.enrollment.as_ref().is_some_and(|e| e.unresolved())
            }) {
                return Err(bad());
            }
        }
        // Reject extra physical registrations not present in the fixed history.
        if self.tasks.keys().any(|tid| {
            *tid != owner.thread && !history.children().iter().any(|p| p.thread() == *tid)
        }) {
            return Err(bad());
        }
        self.close_native_preparations(owner)?;
        Ok(expected.clone())
    }

    pub(in crate::network_runtime) fn retain_shared_initial_final_wait(
        &mut self,
        owner: NetworkStreamOwner,
        root: Arc<ForegroundRoot>,
        history: &SharedInitialTerminalHistory<'_>,
    ) -> std::io::Result<()> {
        let projection = history.initial();
        if self.tasks.get(&owner.thread).is_some_and(|task| {
            task.shared_terminal
                .as_ref()
                .is_some_and(|old| !Arc::ptr_eq(old, projection))
        }) {
            return Err(std::io::Error::other(
                "initial final wait replaced original projection",
            ));
        }
        projection.retain_initial_final_wait(owner, root, history.children())?;
        if let Some(task) = self.tasks.get_mut(&owner.thread) {
            task.shared_terminal = Some(projection.clone());
        }
        Ok(())
    }

    pub(super) fn finish_shared_initial_observations(
        &mut self,
        owner: NetworkStreamOwner,
        projection: &Arc<NativeTaskProjection>,
    ) -> std::io::Result<()> {
        let root = projection.final_wait_root(owner)?;
        if !projection.matches_initial_root(root) {
            return Err(std::io::Error::other("wrong initial observer root"));
        }
        if let Some(task) = self.tasks.get(&owner.thread) {
            if task.mm != owner.mm
                || !task.retired
                || task
                    .foreground_root
                    .as_ref()
                    .is_none_or(|r| !Arc::ptr_eq(r, root))
                || task
                    .shared_terminal
                    .as_ref()
                    .is_none_or(|p| !Arc::ptr_eq(p, projection))
            {
                return Err(std::io::Error::other(
                    "initial observers lost physical owner",
                ));
            }
        } else if !projection.completed_initial_final_wait(root) {
            return Err(std::io::Error::other(
                "initial observer absence is not completion",
            ));
        }
        projection.finish_final_observations(owner, root)?;
        self.retire_completed_shared_child(owner)
    }
}
