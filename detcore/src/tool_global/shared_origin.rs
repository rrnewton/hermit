//! Actual initial-parent callback joins. No logical cleanup or empty-map proof.
use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::NetworkRuntimeResources;
use crate::network_runtime::native_birth_outcome::NativeTaskProjection;

impl GlobalState {
    fn retained_shared_initial(
        &self,
        owner: NetworkStreamOwner,
        process: DetPid,
    ) -> std::io::Result<(Arc<ForegroundRoot>, Arc<NativeTaskProjection>)> {
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| std::io::Error::other("initial terminal lacks engine"))?
            .lock()
            .unwrap();
        let (root, projection) = engine.shared_initial_origin().ok_or_else(|| {
            std::io::Error::other("initial terminal lacks actual pre-effect origin")
        })?;
        if root.owner() != owner
            || root.logical_process() != process
            || !projection.matches_initial_root(&root)
        {
            return Err(std::io::Error::other(
                "initial terminal changed exact origin",
            ));
        }
        Ok((root, projection))
    }
    pub(super) fn settle_shared_initial_terminal(
        &self,
        scheduler: &crate::scheduler::Scheduler,
        runtime: &NetworkRuntimeResources,
        owner: NetworkStreamOwner,
        process: DetPid,
    ) -> std::io::Result<()> {
        let (root, projection) = self.retained_shared_initial(owner, process)?;
        let history = scheduler.shared_initial_terminal_history(owner, &root, &projection)?;
        runtime.native_shared_initial_terminal(owner, &root, &history)
    }
    pub(super) fn finish_shared_initial_observers(
        &self,
        scheduler: &crate::scheduler::Scheduler,
        runtime: &NetworkRuntimeResources,
        owner: NetworkStreamOwner,
        process: DetPid,
    ) -> std::io::Result<()> {
        let (root, projection) = self.retained_shared_initial(owner, process)?;
        // Recheck complete retained history after the two existing observers.
        // No new fact is made here; the same original callback issued it above.
        let _history = scheduler.shared_initial_terminal_history(owner, &root, &projection)?;
        runtime.finish_shared_terminal_observations(owner, &projection)
    }
}
