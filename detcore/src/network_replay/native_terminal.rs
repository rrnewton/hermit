//! Failed-run physical retirement for an already-owned ordinary stream Call.
//! Final wait is distinct from owner-gone. Closing its pin does not acknowledge
//! any receive, copy, zero wait, option or unknown physical effect.

use super::*;

/// Issued only from the exact retained Call after authentic backend final wait.
/// This private, unserialized authority cannot admit a capture or a new effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admission {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
}
impl Admission {
    pub(crate) fn owner(self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn call(self) -> NetworkStreamCallId {
        self.call
    }
}

impl NetworkReplayEngine {
    /// Called synchronously with the final-wait callback's owned ThreadState.
    /// The tombstone also fences a delayed admission whose reply never arrived.
    pub(crate) fn native_stream_final_wait(&mut self, owner: NetworkStreamOwner) -> bool {
        self.gone_stream_owners.insert(owner);
        let mut incomplete = false;
        for call in self.stream_calls.values_mut().filter(|call| {
            call.owner == owner
                && call.original.is_none()
                && call.physical_pin_required
                && call.capture_control.is_none()
                && call.capture_publication.is_none()
        }) {
            call.final_wait = true;
            call.abandoned = true;
            incomplete = true;
        }
        incomplete
    }

    pub(crate) fn terminal_stream_admission(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<Option<Admission>, NetworkReplayError> {
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner || state.original.is_some() || !state.physical_pin_required {
            return Err(NetworkReplayError::UnresolvedStreamCall(call));
        }
        if !state.final_wait {
            return Ok(None);
        }
        // Capture-pending cleanup has its own existing owner and table permit.
        // Accepted provider custody must retire through its existing join first.
        if state.capture_control.is_some() || state.capture_publication.is_some() {
            return Ok(None);
        }
        self.check_accept_call_release(call)?;
        if !matches!(
            state.phase,
            StreamCallPhase::Active
                | StreamCallPhase::PinReleaseSubmitted
                | StreamCallPhase::TerminalPinReleased
        ) {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        Ok(Some(Admission { owner, call }))
    }

    /// Consume exact physical evidence once. All semantic obligations and their
    /// diagnostics remain in this same Call/operation state, so finish is RED.
    pub(crate) fn retain_terminal_stream_release(
        &mut self,
        admission: Admission,
        evidence: crate::network_runtime::native_peer::TerminalEvidence,
    ) -> Result<(), NetworkReplayError> {
        if self.terminal_stream_admission(admission.owner, admission.call)? != Some(admission)
            || !evidence.matches(admission)
        {
            return Err(NetworkReplayError::UnresolvedStreamCall(admission.call));
        }
        let state = &self.stream_calls[&admission.call];
        if let Some(previous) = &state.terminal_evidence {
            return if previous == &evidence {
                Ok(())
            } else {
                Err(NetworkReplayError::UnresolvedStreamCall(admission.call))
            };
        }
        let open_file = state
            .open_file
            .ok_or(NetworkReplayError::UnresolvedStreamCall(admission.call))?;
        // CompletedAndRecorded here settles only the actual closed StreamCall
        // pin. The exact incomplete effects remain in evidence and their engine
        // operations; this is not a successful transport/receive acknowledgement.
        self.release_stream_call_lifetime(admission.owner, admission.call, open_file)?;
        let state = self.stream_calls.get_mut(&admission.call).unwrap();
        state.phase = StreamCallPhase::TerminalPinReleased;
        state.terminal_evidence = Some(evidence);
        // Do not remove the Call, probe, delivery, control or zero-wait. Their
        // incomplete result must still reject successful trace publication.
        self.complete_deferred_retirement(open_file);
        Ok(())
    }
}
