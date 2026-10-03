//! Existing actual Call/controller owners, with a distinct shared-policy consumer.
use super::*;
use crate::scheduler::ordinary_fd::shared_initial::SharedInitialTerminalHistory;

impl super::super::NetworkRuntimeResources {
    pub(crate) fn publish_shared_initial_connected(
        &self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        root: &std::sync::Arc<super::super::ForegroundRoot>,
        now: detcore_model::time::LogicalTime,
    ) -> io::Result<()> {
        let calls = self.shared.native_streams.lock().unwrap();
        let call = calls
            .calls
            .get(&admission.call)
            .ok_or_else(|| io::Error::other("shared Connect lost actual completed native Call"))?;
        engine
            .publish_shared_initial_connected(
                owner,
                admission,
                root,
                &CompletedNativeConnect { call },
                now,
            )
            .map_err(io::Error::other)
    }

    /// Called only from the actual initial final-wait hook with scheduler held.
    pub(crate) fn native_shared_initial_terminal(
        &self,
        owner: NetworkStreamOwner,
        root: &std::sync::Arc<super::super::ForegroundRoot>,
        history: &SharedInitialTerminalHistory<'_>,
    ) -> io::Result<()> {
        if self.shared.endpoint.is_none() {
            return Err(io::Error::other(
                "initial final wait has no accepted endpoint",
            ));
        }
        let mut tasks = self.shared.physical.lock().unwrap();
        let root = tasks.prepare_shared_initial_final_wait(owner, root, history)?;
        match self.shared.controller.lock().unwrap().as_ref() {
            Some(Ok(controller)) => controller.native_birth_creator_terminal(owner)?,
            Some(Err(error)) => return Err(io::Error::other(error.to_string())),
            None => {}
        }
        tasks.retain_shared_initial_final_wait(owner, root, history)
    }
}
