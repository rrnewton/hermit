//! The original shared Record entry survives its one physical pin acquisition.
//! This is Call custody, never a replacement for the selected Normal grant.
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;
use crate::network_runtime::JoinedNativeWorkerReceipt;
use crate::network_runtime::original_installation::FileIdentity;
use crate::network_runtime::shared_waits::ConfirmedSharedCapture;
use crate::network_runtime::shared_waits::JoinedSharedPrefix;

#[derive(Debug)]
pub(crate) struct SharedCaptureOrigin {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    binding: crate::types::FdSlotBinding,
    identity: FileIdentity,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    epoch: u64,
    entry: NetworkReleaseV4,
    prefix: JoinedSharedPrefix,
    submitted: AtomicBool,
    joined: OnceLock<(JoinedNativeWorkerReceipt, NetworkStreamPinOutcome)>,
}
impl SharedCaptureOrigin {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.call
    }
    pub(crate) fn fd(&self) -> i32 {
        self.binding.slot.fd
    }
    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity
    }
    pub(crate) fn root(&self) -> &Arc<crate::network_runtime::ForegroundRoot> {
        &self.root
    }
    pub(crate) fn prefix(&self) -> &JoinedSharedPrefix {
        &self.prefix
    }
    pub(crate) fn claim_submission(&self) -> std::io::Result<()> {
        self.submitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| std::io::Error::other("shared capture was already submitted"))
    }
    pub(crate) fn retain_join(
        &self,
        joined: JoinedNativeWorkerReceipt,
        outcome: NetworkStreamPinOutcome,
    ) -> std::io::Result<()> {
        self.joined
            .set((joined, outcome))
            .map_err(|_| std::io::Error::other("shared capture replaced its actual join"))
    }
    pub(crate) fn joined(&self) -> Option<&(JoinedNativeWorkerReceipt, NetworkStreamPinOutcome)> {
        self.joined.get()
    }
}

/// Non-Clone. The same origin remains on the Call if this callback disappears.
#[derive(Debug)]
pub(crate) struct SharedCaptureSubmission(Arc<SharedCaptureOrigin>);
impl SharedCaptureSubmission {
    pub(crate) fn into_origin(self) -> Arc<SharedCaptureOrigin> {
        self.0
    }
}

impl NetworkReplayEngine {
    pub(crate) fn prepare_shared_record_capture(
        &mut self,
        call: NetworkStreamCallId,
        identity: FileIdentity,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
    ) -> Result<SharedCaptureSubmission, NetworkReplayError> {
        let state = self.shared_wait_capture_pending_state(grant.owner(), call)?;
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            unreachable!()
        };
        let AttemptPhase::Active {
            ordinal: 0,
            epoch,
            entry: AttemptEntry::Record(entry),
            completion: None,
        } = &wait.phase
        else {
            return Err(invalid("shared capture lacks its original Record entry"));
        };
        if wait.capture.is_some()
            || *epoch != grant.epoch()
            || !Arc::ptr_eq(&wait.root, grant.root())
            || !Arc::ptr_eq(admission.root(), grant.root())
            || !admission.matches_peers(self, Some(call))?
            || !self.shared_census_matches_grant(None, Some(call), grant)?
            || self
                .shadow
                .as_ref()
                .and_then(|s| s.sockets.get(&wait.binding.open_file))
                .and_then(|s| s.native.as_ref())
                .is_none_or(|n| n.identity != identity)
        {
            return Err(invalid(
                "shared capture changed original grant/file/peer census",
            ));
        }
        self.preflight_shared_capture_settlement(grant.owner(), call)?;
        let origin = Arc::new(SharedCaptureOrigin {
            owner: grant.owner(),
            call,
            binding: wait.binding,
            identity,
            root: wait.root.clone(),
            epoch: *epoch,
            entry: entry.clone(),
            prefix: admission.retained_capture_prefix(),
            submitted: AtomicBool::new(false),
            joined: OnceLock::new(),
        });
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.capture = Some(origin.clone());
        Ok(SharedCaptureSubmission(origin))
    }

    /// Read-only exact original capture and all suspended peers. The runtime
    /// separately joins the real native Calls and worker population.
    pub(crate) fn validate_shared_record_capture(
        &self,
        origin: &Arc<SharedCaptureOrigin>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        let state = self.shared_wait_capture_pending_state(origin.owner, origin.call)?;
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            unreachable!()
        };
        if wait
            .capture
            .as_ref()
            .is_none_or(|held| !Arc::ptr_eq(held, origin))
            || wait.binding != origin.binding
            || !Arc::ptr_eq(&wait.root, &origin.root)
            || !matches!(&wait.phase, AttemptPhase::Active { ordinal: 0, epoch, entry: AttemptEntry::Record(entry), completion: None }
                if *epoch == origin.epoch && *entry == origin.entry)
            || !origin
                .prefix
                .matches_retained_peers(self, Some(origin.call))?
            || self
                .shadow
                .as_ref()
                .and_then(|s| s.sockets.get(&origin.binding.open_file))
                .and_then(|s| s.native.as_ref())
                .is_none_or(|n| n.identity != origin.identity)
        {
            return Err(invalid("shared capture lost its retained original entry"));
        }
        self.preflight_shared_capture_settlement(origin.owner, origin.call)?;
        self.shared_call_census_excluding(None, Some(origin.call))
    }

    fn preflight_shared_capture_settlement(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        if !self.fd_table_capability() {
            return Err(invalid("shared capture lost actual table capability"));
        }
        let state = self.shared_wait_capture_pending_state(owner, call)?;
        let file = state.open_file.expect("checked original Call");
        self.validate_stream_call_lifetime(owner, call, file)?;
        let permit = state
            .capture_publication
            .expect("checked capture publication");
        self.validate_publication_permit(owner, permit)?;
        let publication = &self.fd_publications[&permit.files];
        let lease = state.capture_control.expect("checked capture control");
        let control = self.owned_socket_control(owner, lease)?;
        if publication.reader.is_some()
            || publication.pending.is_some()
            || publication.enrollment.is_some()
            || control.open_file != file
            || !control.physical.can_release_unchanged()
            || self.shadow_probes.contains_key(&lease)
            || self.socket_controls.len() != 1
        {
            return Err(invalid(
                "shared capture cannot settle original publication/control",
            ));
        }
        Ok(())
    }

    pub(in crate::network_replay) fn shared_wait_pin_confirmation_state(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        outcome: NetworkStreamPinOutcome,
    ) -> Result<&StreamCallState, NetworkReplayError> {
        let state = self.shared_wait_capture_pending_state(owner, call)?;
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            unreachable!()
        };
        if wait.capture.as_ref().is_none_or(|origin| {
            !origin.submitted.load(Ordering::Acquire)
                || origin
                    .joined
                    .get()
                    .is_none_or(|(_, actual)| *actual != outcome)
        }) {
            return Err(invalid(
                "shared capture lacks its actual submitted worker join",
            ));
        }
        Ok(state)
    }

    /// Every recoverable check precedes publication/control mutation. This
    /// accepts only the runtime's exact borrowed successful acquisition.
    pub(crate) fn complete_shared_record_capture(
        &mut self,
        confirmed: &ConfirmedSharedCapture<'_>,
        grant: &SharedMmForegroundObservation<'_>,
    ) -> Result<NetworkStreamCall, NetworkReplayError> {
        let origin = confirmed.origin();
        self.validate_shared_record_capture(origin)?;
        if grant.owner() != origin.owner
            || grant.epoch() != origin.epoch
            || !Arc::ptr_eq(grant.root(), &origin.root)
            || !self.shared_census_matches_grant(None, Some(origin.call), grant)?
        {
            return Err(invalid(
                "shared capture completion crossed its original Normal grant",
            ));
        }
        self.shared_wait_pin_confirmation_state(
            origin.owner,
            origin.call,
            NetworkStreamPinOutcome::Acquired,
        )?;
        let control = self.stream_calls[&origin.call].capture_control.unwrap();
        self.confirm_stream_call_pin(origin.owner, origin.call, NetworkStreamPinOutcome::Acquired)
            .expect("exact shared capture publication prevalidated under the same engine lock");
        self.finish_socket_control(origin.owner, control, NetworkSocketControlFinish::Unchanged)
            .expect("exact shared capture control prevalidated under the same engine lock");
        Ok(NetworkStreamCall {
            id: origin.call,
            open_file: origin.binding.open_file,
            physical_pin_required: true,
        })
    }
}
