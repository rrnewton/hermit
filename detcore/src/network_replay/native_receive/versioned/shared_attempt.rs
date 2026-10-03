//! Closed shared-MM policy and one actual selected Replay source attempt.
//! The Call owns the claim; the runtime owns worker/physical exclusion. Neither
//! this policy tag nor a logical phase certifies a native syscall completion.
use super::*;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

#[cfg(test)]
#[path = "shared_attempt/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "shared_attempt/executable_tests.rs"]
mod executable_tests;

#[derive(Debug, Clone)]
pub(in crate::network_replay) struct TransmitAttempt {
    root: Arc<crate::network_runtime::ForegroundRoot>,
    epoch: u64,
    prefix: crate::network_runtime::shared_waits::JoinedSharedPrefix,
    binding: crate::types::FdSlotBinding,
    transmitted: u64,
    length: usize,
    executable: Option<Arc<crate::network_runtime::executable_capture::ExecutableCapture>>,
}

#[derive(Debug, Clone)]
pub(in crate::network_replay) enum SharedAttempt {
    RecordTransmit(Arc<super::shared_send::SharedRecordSend>),
    Transmit(TransmitAttempt),
    Wait(Box<super::shared_waits::SharedWait>),
}
impl SharedAttempt {
    fn transmit(&self) -> Option<&TransmitAttempt> {
        match self {
            Self::Transmit(attempt) => Some(attempt),
            Self::Wait(_) | Self::RecordTransmit(_) => None,
        }
    }
}

impl NetworkReplayEngine {
    pub(crate) fn record_shared_mm_attempts(epoch: DateTime<Utc>) -> Self {
        let mut engine = Self::record_native_receive(epoch);
        let EngineState::Native(native) = &mut engine.mode else {
            unreachable!()
        };
        assert!(native.untouched_record());
        native.trace.release_model =
            NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes: Vec::new() };
        engine
    }

    pub(crate) fn replay_shared_mm_attempts(
        trace: NetworkTraceV4,
    ) -> Result<Self, NetworkReplayError> {
        if !matches!(
            trace.release_model,
            NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { .. }
        ) {
            return Err(invalid(
                "shared V4 replay requires serialized shared-MM attempt policy",
            ));
        }
        Self::replay_native_receive_inner(trace)
    }

    pub(crate) fn uses_shared_mm_attempts(&self) -> bool {
        matches!(&self.mode, EngineState::Native(native)
            if matches!(native.trace.release_model, NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { .. }))
    }

    pub(super) fn require_sole_initial_release_policy(&self) -> Result<(), NetworkReplayError> {
        match &self.mode {
            EngineState::Native(native)
                if matches!(
                    native.trace.release_model,
                    NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { .. }
                ) =>
            {
                Ok(())
            }
            EngineState::Native(_) => Err(invalid(
                "legacy V4 issuer requires sole-initial-root policy",
            )),
            _ => Err(NetworkReplayError::WrongMode),
        }
    }

    pub(in crate::network_replay) fn check_shared_attempt_unclaimed(
        &self,
        file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        if let Some((call, _)) = self
            .stream_calls
            .iter()
            .find(|(_, state)| state.open_file == Some(file) && state.shared_attempt.is_some())
        {
            return Err(NetworkReplayError::UnresolvedStreamCall(*call));
        }
        Ok(())
    }

    /// All refusal paths precede the reader transfer. The caller's runtime
    /// transaction has already reserved an interval and holds the exact complete
    /// physical lineage plus worker/Calls admission through this mutation.
    pub(crate) fn begin_shared_replay_transmit(
        &mut self,
        read: NetworkFdReadAdmission,
        grant: &SharedMmForegroundObservation<'_>,
        prefix: &crate::network_runtime::shared_waits::JoinedSharedPrefix,
        admission: &crate::network_runtime::shared_waits::SharedAttemptAdmission<'_>,
        length: usize,
    ) -> Result<NetworkStreamCall, NetworkReplayError> {
        let owner = grant.owner();
        self.check_native_retirement()?;
        if !self.uses_shared_mm_attempts() || self.mode() != NetworkEngineMode::Replay {
            return Err(NetworkReplayError::WrongMode);
        }
        self.validate_fd_read(owner, &read)?;
        let binding = read
            .binding
            .ok_or_else(|| invalid("shared source lacks selected descriptor"))?;
        if !(1..=512).contains(&length)
            || read.external_grant.is_some()
            || !admission.matches_peers(self, None)?
            || !self.shared_census_matches_grant(None, None, grant)?
            || self.stream_calls.values().any(|state| state.owner == owner)
            || !self.stream_operations.is_empty()
            || !self.shadow_probes.is_empty()
            || !Arc::ptr_eq(grant.root(), prefix.root())
            || !Arc::ptr_eq(grant.root(), admission.root())
            || !admission.is_original_prefix(prefix)
            || binding.slot.files != grant.root().files()
            || !grant.root().has_shared_mm_history()
            || self.transmit_stream_read_limit(binding.open_file, length)? != length
        {
            return Err(invalid(
                "shared source changed selected prefix/lineage or owns another active attempt",
            ));
        }
        let channel = self.bound_channel(binding.open_file)?;
        let EngineState::Native(native) = &self.mode else {
            unreachable!()
        };
        let completed = self.native_completed()?;
        if !native.trace.channels.iter().any(|definition| {
            definition.id == channel
                && definition.transport == NetworkTransportV2::Tcp
                && definition.role == NetworkEndpointRoleV2::OutboundClient
        }) || !native.trace.release_model.nodes().iter().any(|node| {
            matches!(node.kind, NetworkReleaseNodeKindV4::Progress {
                    channel: established, milestone: NetworkProgressV4::Established { .. },
                } if established == channel)
                && completed.contains(&node.id)
        }) {
            return Err(invalid(
                "shared source lacks completed outbound TCP establishment",
            ));
        }
        let transmitted = self.replay_transmit_offset(binding.open_file)?;
        let control = read
            .control
            .ok_or_else(|| invalid("shared source lost selected control"))?;
        let attempt = TransmitAttempt {
            root: grant.root().clone(),
            epoch: grant.epoch(),
            prefix: prefix.clone(),
            binding,
            transmitted,
            length,
            executable: None,
        };
        let call = self.begin_native_stream_call_from_read(owner, read)?;
        assert!(!call.physical_pin_required);
        self.finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .expect("prevalidated logical reader cannot acquire an effect during transfer");
        self.stream_calls
            .get_mut(&call.id)
            .expect("transferred Call")
            .shared_attempt = Some(SharedAttempt::Transmit(attempt));
        Ok(call)
    }

    pub(crate) fn validate_shared_replay_transmit(
        &self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        prefix: &crate::network_runtime::shared_waits::JoinedSharedPrefix,
    ) -> Result<OpenFileId, NetworkReplayError> {
        let owner = grant.owner();
        self.check_stream_owner(owner)?;
        self.check_native_retirement()?;
        if !self.uses_shared_mm_attempts()
            || self.mode() != NetworkEngineMode::Replay
            || !self.fd_table_capability()
        {
            return Err(invalid("shared source lost replay/table policy"));
        }
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let attempt = state
            .shared_attempt
            .as_ref()
            .and_then(SharedAttempt::transmit)
            .ok_or(NetworkReplayError::StreamCallPhaseMismatch(call))?;
        if state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.phase != StreamCallPhase::Active
            || state.physical_pin_required
            || state.terminal_evidence.is_some()
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || state.original.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || state.replay_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.receive_policy.is_some()
            || state.replay_connect.is_some()
            || state.open_file != Some(attempt.binding.open_file)
            || attempt.epoch != grant.epoch()
            || !Arc::ptr_eq(&attempt.root, grant.root())
            || !attempt.prefix.same_prefix(prefix)
            || !attempt.root.is_current(owner)
            || !attempt.root.has_shared_mm_history()
            || !prefix.matches_retained_peers(self, Some(call))?
            || !self.shared_census_matches_grant(None, Some(call), grant)?
            || !self.stream_operations.is_empty()
            || !self.socket_controls.is_empty()
            || !self.shadow_probes.is_empty()
            || self
                .zero_stream_waits
                .values()
                .any(|wait| wait.call == call)
            || self.replay_transmit_offset(attempt.binding.open_file)? != attempt.transmitted
            || self.transmit_stream_read_limit(attempt.binding.open_file, attempt.length)?
                != attempt.length
        {
            return Err(invalid(
                "shared source changed original Call/grant/prefix or retains other custody",
            ));
        }
        self.validate_stream_call_lifetime(owner, call, attempt.binding.open_file)?;
        Ok(attempt.binding.open_file)
    }

    pub(crate) fn attach_shared_executable_capture(
        &mut self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        prefix: &crate::network_runtime::shared_waits::JoinedSharedPrefix,
        address: usize,
        capture: Arc<crate::network_runtime::executable_capture::ExecutableCapture>,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shared_replay_transmit(call, grant, prefix)?;
        let Some(SharedAttempt::Transmit(attempt)) = self
            .stream_calls
            .get_mut(&call)
            .and_then(|state| state.shared_attempt.as_mut())
        else {
            unreachable!()
        };
        if attempt.executable.is_some()
            || !capture.matches(&attempt.root, call, address, attempt.length)
        {
            return Err(invalid("executable capture changed original source Call"));
        }
        attempt.executable = Some(capture);
        Ok(())
    }

    /// Only the actual Global source consumer calls this inside the original
    /// with_source_interval transaction after the backend's true worker join.
    /// On mismatch/error the claim stays retained; no generic cancellation may
    /// erase it. Global drains lifetime-retired ports before replying.
    pub(crate) fn complete_shared_replay_transmit(
        &mut self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        prefix: &crate::network_runtime::shared_waits::JoinedSharedPrefix,
        bytes: &[u8],
    ) -> Result<StreamTransmitOutcome, NetworkReplayError> {
        let file = self.validate_shared_replay_transmit(call, grant, prefix)?;
        if bytes.len()
            != self.stream_calls[&call]
                .shared_attempt
                .as_ref()
                .and_then(SharedAttempt::transmit)
                .unwrap()
                .length
        {
            return Err(invalid("shared source changed exact selected length"));
        }
        if let Some(capture) = self.stream_calls[&call]
            .shared_attempt
            .as_ref()
            .and_then(SharedAttempt::transmit)
            .and_then(|a| a.executable.as_ref())
        {
            capture
                .require_joined()
                .map_err(|error| invalid(&error.to_string()))?;
        }
        let result = self.transmit_stream_inner(file, bytes)?;
        self.release_stream_call_lifetime(grant.owner(), call, file)
            .expect("same-lock shared transmit validated exact lifetime lease before consumption");
        self.stream_calls.remove(&call);
        self.complete_deferred_retirement(file);
        self.check_native_retirement()?;
        Ok(result)
    }
}
