//! FD admission at an existing selected request, before its actual grant.
//!
//! A queued intent owns no table or OFD. The engine's existing reader owns the
//! resulting permit; the scheduler response only transports that exact token.
use detcore_model::fd::FilesId;

use super::*;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkFdReadBegin;
use crate::network_replay::NetworkReplayError;
use crate::network_replay::NetworkStreamOwner;

/// Serializable lookup intent; neither a selected file nor a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdReadIntent {
    /// Exact current task and address-space incarnation.
    pub owner: NetworkStreamOwner,
    /// Current descriptor table; exec replacement must not adopt this request.
    pub files: FilesId,
    /// Original numeric descriptor, including invalid operands.
    pub fd: i32,
    /// Original external scheduling operation requested by this caller.
    pub operation: ExternalOpId,
}

impl FdReadIntent {
    fn matches_resources(self, resources: &Resources) -> bool {
        if resources.tid != self.owner.thread || resources.resources.len() > 1 {
            return false;
        }
        resources.resources.len() == 1
            && [
                ResourceID::BlockingNetworkCapture(self.operation),
                ResourceID::BlockingExternalIO(self.operation),
            ]
            .iter()
            .any(|resource| resources.resources.get(resource) == Some(&Permission::RW))
    }
}

/// Admission and a wait decision are made at the same engine-state cut.
/// The wait variant is not a table token or a physical result.
pub(crate) enum SelectedFdRead {
    /// No intent, or the exact newly admitted reader for this selected grant.
    Ready(Option<Box<NetworkFdReadAdmission>>),
    /// The failed lookup and an independently runnable exact holder agreed atomically.
    AwaitPriorSelection,
}

impl Scheduler {
    #[cfg(test)]
    pub(crate) fn fd_read_grant_diagnostic(&self) -> Option<&str> {
        self.terminal_deadlock.as_deref()
    }

    /// Revalidate the actual selected transport before touching metadata. This
    /// is not authority for an arbitrary RPC or a former response's new turn.
    pub(super) fn try_selected_fd_read(
        &mut self,
        owner: DetTid,
        request: &Ivar<SchedRequest>,
        response: &Ivar<SchedResponse>,
        resources: &Resources,
    ) -> Result<SelectedFdRead, NetworkReplayError> {
        let failure = |message: &str| NetworkReplayError::FdPublicationProtocol(message.into());
        let Some(intent) = resources.fd_read else {
            return Ok(SelectedFdRead::Ready(None));
        };
        let turn = self
            .next_turns
            .get(&owner)
            .ok_or_else(|| failure("selected FD reader lost its request owner"))?;
        if self.backend_failed()
            || self.thread_is_logically_killed(owner)
            || !self.rpc_incarnation_matches(owner, intent.owner.mm)
            || owner != intent.owner.thread
            || !intent.matches_resources(resources)
            || &turn.req != request
            || &turn.resp != response
            || turn
                .req
                .try_read()
                .is_none_or(|value| !matches!(value, Ok(actual) if actual == *resources))
            || turn
                .protocol
                .origin
                .as_ref()
                .is_none_or(|origin| origin.mm != intent.owner.mm)
            || turn.protocol.fd_read.is_some()
        {
            return Err(failure("FD reader differs from the selected live request"));
        }
        // This first external caller posts a nonpolling request. Do not turn
        // a deprioritization or signal-only response into lookup authority;
        // general restarted reader requests need their separate owned join.
        if resources.poll_attempt != 0 || !self.inbound_signals(owner).is_empty() {
            return Ok(SelectedFdRead::Ready(None));
        }
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| failure("selected FD reader has no network engine"))?;
        let actual = engine
            .lock()
            .unwrap()
            .fd_metadata(intent.owner, intent.files)?;
        // Never take metadata while retaining the engine lock. Association is
        // checked again after taking metadata, under the established lock order.
        let metadata = actual.lock().unwrap();
        let mut engine = engine.lock().unwrap();
        engine.validate_fd_metadata(intent.owner, intent.files, &actual, &metadata)?;
        let read = match engine.begin_fd_read(intent.owner, intent.files, intent.fd) {
            Ok(NetworkFdReadBegin::Admitted(read)) => read,
            Ok(NetworkFdReadBegin::Recover) => {
                return Err(failure(
                    "selected reader needs its original publication recovery",
                ));
            }
            Err(error @ NetworkReplayError::StreamOperationBusy(_)) => {
                // Keep the failed admission and its exact holder in this same
                // engine interval. The Driver can clear the holder as soon as
                // this guard drops; that is progress, not a protocol failure.
                let progress = engine
                    .fd_read_pending_external_selection(intent.owner, intent.files, intent.fd)
                    .is_some_and(|(holder, operation)| {
                        holder != intent.owner && self.original_fd_grant_matches(holder, operation)
                    });
                return if progress {
                    Ok(SelectedFdRead::AwaitPriorSelection)
                } else {
                    Err(error)
                };
            }
            Err(error) => return Err(error),
        };
        let read = engine.bind_fd_read_external_grant(intent.owner, *read, intent.operation)?;
        Ok(SelectedFdRead::Ready(Some(Box::new(read))))
    }

    /// Exact already-granted operation, distinct from a queued request or a
    /// host-ready provider command. Used by the consuming Global transfer too.
    pub(crate) fn original_external_grant_matches(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
    ) -> bool {
        self.rpc_incarnation_matches(owner.thread, owner.mm)
            && self.network_capture_blockers.get(&owner.thread) == Some(&operation)
            && self.blocked.external_io_blockers.get(&owner.thread) == Some(&operation)
    }

    /// Exact ordinary BlockingExternalIO grant. Filesystem helper latency
    /// must not enroll in, or borrow authority from, the network capture clock.
    pub(crate) fn original_external_io_grant_matches(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
    ) -> bool {
        self.rpc_incarnation_matches(owner.thread, owner.mm)
            && self.blocked.external_io_blockers.get(&owner.thread) == Some(&operation)
            && !self.network_capture_blockers.contains_key(&owner.thread)
    }

    /// Match the original operation's exact time-source classification too.
    /// Close cannot borrow a capture grant, nor Connect an ordinary IO grant.
    pub(crate) fn original_transfer_grant_matches(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        kind: crate::network_replay::original_connect::Kind,
    ) -> bool {
        use crate::network_replay::original_connect::Kind;
        match kind {
            Kind::Close => self.original_external_io_grant_matches(owner, operation),
            Kind::Connect => self.original_external_grant_matches(owner, operation),
            _ => false,
        }
    }

    /// Actual selected external resource, including scalar Read's existing
    /// BlockingExternalIO path. The token itself was issued before step4.
    pub(crate) fn original_fd_grant_matches(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
    ) -> bool {
        self.rpc_incarnation_matches(owner.thread, owner.mm)
            && self.blocked.external_io_blockers.get(&owner.thread) == Some(&operation)
            && self
                .network_capture_blockers
                .get(&owner.thread)
                .is_none_or(|actual| *actual == operation)
    }

    #[cfg(test)]
    pub(crate) fn harvest_external_io_for_test(&mut self) -> Result<(), super::SkipTurn> {
        self.step2c_process_io_blockers()
    }

    #[cfg(test)]
    pub(crate) fn set_fd_read_test_cut(&mut self, action: impl FnOnce() + Send + 'static) {
        assert!(self.fd_read_test_cut.0.is_none());
        self.fd_read_test_cut.0 = Some(Box::new(action));
    }
}

// One-shot component interleaving at the actual post-admission cut. This is
// absent from production builds and carries no authority or scheduling state.
#[cfg(test)]
#[derive(Default)]
pub(super) struct TestCutHook(Option<Box<dyn FnOnce() + Send>>);
#[cfg(test)]
impl std::fmt::Debug for TestCutHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TestCutHook")
            .field(&self.0.is_some())
            .finish()
    }
}
#[cfg(test)]
impl TestCutHook {
    pub(super) fn run(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}

/// Exact selected network-capture grant, borrowed under the scheduler guard.
/// This is distinct from foreground execution and ordinary external file IO.
#[derive(Debug)]
pub(crate) struct NativeCaptureEntryObservation<'a> {
    owner: NetworkStreamOwner,
    operation: &'a ExternalOpId,
    root: &'a crate::network_runtime::ForegroundRoot,
    epoch: u64,
}
impl NativeCaptureEntryObservation<'_> {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn operation(&self) -> ExternalOpId {
        *self.operation
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn admits_sole_initial_root(
        &self,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> bool {
        std::ptr::eq(self.root, root) && root.is_sole_initial_root(self.owner)
    }
}
impl Scheduler {
    pub(crate) fn native_capture_entry_observation<'a>(
        &'a self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        root: &'a crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<NativeCaptureEntryObservation<'a>> {
        let bad = || std::io::Error::other("native entry lacks its selected network-capture grant");
        if self.backend_failed()
            || self.thread_is_logically_killed(owner.thread)
            || !self.original_external_grant_matches(owner, operation)
            || self.run_queue.contains_tid(owner.thread)
            || self.pending_run_queue_removals.contains_key(&owner.thread)
            || self
                .pending_run_queue_admissions
                .contains_key(&owner.thread)
            || self
                .next_turns
                .get(&owner.thread)
                .is_none_or(|turn| turn.req.try_read().is_some() || turn.resp.try_read().is_some())
        {
            return Err(bad());
        }
        self.validate_native_initial_root(owner, root)?;
        Ok(NativeCaptureEntryObservation {
            owner,
            root,
            epoch: self.next_turns[&owner.thread].protocol.epoch,
            operation: self
                .network_capture_blockers
                .get(&owner.thread)
                .ok_or_else(bad)?,
        })
    }
}
