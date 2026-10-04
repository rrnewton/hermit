//! Close eligibility is private history of one actual original Socket. Neither
//! a recorded public profile nor a duplicate descriptor can manufacture it.
use std::sync::Arc;

use super::*;
use crate::network_runtime::original_installation::FileIdentity;
use crate::network_runtime::original_installation::Installation;
use crate::network_runtime::original_installation::Source;
use crate::network_runtime::socket_birth_policy::Completed;
use crate::network_runtime::socket_birth_policy::decline_diagnostic;

#[derive(Debug)]
pub(in crate::network_replay) struct FiniteCloseBirth {
    file: OpenFileId,
    identity: FileIdentity,
    call: NetworkStreamCallId,
    policy: Arc<Completed>,
}
impl FiniteCloseBirth {
    pub(in crate::network_replay) fn matches_initial_root(
        &self,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> bool {
        self.policy.matches_initial_root(root)
    }
}
#[derive(Debug, Clone)]
pub(super) struct Provenance {
    birth: Arc<FiniteCloseBirth>,
    revoked: bool,
}
fn invalid(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.into())
}
impl NetworkReplayEngine {
    pub(in crate::network_replay) fn finite_close_birth_for_read(
        &self,
        owner: NetworkStreamOwner,
        read: &NetworkFdReadAdmission,
    ) -> Result<Option<Arc<FiniteCloseBirth>>, NetworkReplayError> {
        self.validate_fd_read(owner, read)?;
        let Some(binding) = read.binding else {
            decline_diagnostic(format_args!(
                "phase=close-birth fd={} reason=no-binding",
                read.fd
            ));
            return Ok(None);
        };
        let Some(socket) = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&binding.open_file))
        else {
            decline_diagnostic(format_args!(
                "phase=close-birth fd={} ofd={:?} reason=no-socket",
                read.fd, binding.open_file
            ));
            return Ok(None);
        };
        let Some(provenance) = &socket.finite_close else {
            decline_diagnostic(format_args!(
                "phase=close-birth fd={} ofd={:?} reason=no-birth",
                read.fd, binding.open_file
            ));
            return Ok(None);
        };
        if provenance.revoked {
            decline_diagnostic(format_args!(
                "phase=close-birth fd={} ofd={:?} birth_call={} reason=revoked",
                read.fd,
                binding.open_file,
                provenance.birth.call.native_command_call()
            ));
            return Ok(None);
        }
        self.validate_finite_close_birth(binding.open_file, &provenance.birth)?;
        decline_diagnostic(format_args!(
            "phase=close-birth fd={} ofd={:?} birth_call={} reason=present",
            read.fd,
            binding.open_file,
            provenance.birth.call.native_command_call()
        ));
        Ok(Some(provenance.birth.clone()))
    }
    pub(in crate::network_replay) fn validate_finite_close_birth(
        &self,
        open_file: OpenFileId,
        birth: &Arc<FiniteCloseBirth>,
    ) -> Result<(), NetworkReplayError> {
        if !self.uses_shared_mm_attempts() || birth.file != open_file {
            return Err(invalid("finite Close changed shared socket incarnation"));
        }
        let retained = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&open_file))
            .and_then(|s| s.finite_close.as_ref())
            .ok_or_else(|| invalid("finite Close lacks original birth"))?;
        if retained.revoked
            || !Arc::ptr_eq(&retained.birth, birth)
            || retained.birth.identity != birth.identity
            || retained.birth.call != birth.call
        {
            return Err(invalid("finite Close birth was revoked or replaced"));
        }
        Ok(())
    }
    /// Validate actual existing exclusion first; no new control is acquired.
    /// A failed/unknown setter cannot undo this pre-effect history transition.
    pub(crate) fn observe_finite_close_option_attempt(
        &mut self,
        owner: NetworkStreamOwner,
        control: NetworkStreamLeaseId,
        level: i32,
        option: i32,
    ) -> Result<(), NetworkReplayError> {
        let held = self.owned_socket_control(owner, control)?;
        if !held.physical.can_release_unchanged() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(control));
        }
        let file = held.open_file;
        let safe = level == libc::SOL_SOCKET
            && matches!(
                option,
                libc::SO_RCVTIMEO | libc::SO_SNDTIMEO | libc::SO_RCVLOWAT
            );
        if !safe
            && let Some(provenance) = self
                .shadow
                .as_mut()
                .and_then(|s| s.sockets.get_mut(&file))
                .and_then(|s| s.finite_close.as_mut())
        {
            provenance.revoked = true;
            decline_diagnostic(format_args!(
                "phase=birth-revoked ofd={:?} birth_call={} level={} option={}",
                file,
                provenance.birth.call.native_command_call(),
                level,
                option
            ));
        }
        Ok(())
    }
    pub(super) fn validate_finite_close_installation(
        &self,
        binding: crate::types::FdSlotBinding,
        receipt: &Installation,
    ) -> Result<(), NetworkReplayError> {
        let Some(policy) = receipt.finite_close_birth() else {
            return Ok(());
        };
        let Source::Socket(call) = receipt.source() else {
            return Err(invalid("Close birth is not an original Socket"));
        };
        if !self.uses_shared_mm_attempts()
            || !policy.validates(receipt.original_owner(), call)
            || binding.slot.fd != receipt.fd()
            || binding.slot.files != receipt.files()
            || !binding.open_file.is_socket()
        {
            return Err(invalid("Close birth changed actual installation"));
        }
        if let Some(prior) = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&binding.open_file))
            .and_then(|s| s.finite_close.as_ref())
            && (prior.birth.identity != receipt.file_identity()
                || prior.birth.call != call
                || !prior.birth.policy.same(policy))
        {
            return Err(invalid(
                "Close birth cannot replace retained original incarnation",
            ));
        }
        Ok(())
    }
    /// Called only after the above preflight and actual common enrollment.
    /// Repeated publication never clears the existing sticky state.
    pub(super) fn retain_finite_close_installation(
        &mut self,
        binding: crate::types::FdSlotBinding,
        receipt: &Installation,
    ) {
        if self.uses_shared_mm_attempts()
            && let Source::Socket(call) = receipt.source()
        {
            decline_diagnostic(format_args!(
                "phase=birth-enroll call={} fd={} ofd={:?} proof={}",
                call.native_command_call(),
                binding.slot.fd,
                binding.open_file,
                receipt.finite_close_birth().is_some()
            ));
        }
        let Some(policy) = receipt.finite_close_birth() else {
            return;
        };
        let Source::Socket(call) = receipt.source() else {
            unreachable!("checked Socket birth");
        };
        let socket = self
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&binding.open_file)
            .expect("original Socket was enrolled");
        if socket.finite_close.is_none() {
            socket.finite_close = Some(Provenance {
                birth: Arc::new(FiniteCloseBirth {
                    file: binding.open_file,
                    identity: receipt.file_identity(),
                    call,
                    policy: policy.clone(),
                }),
                revoked: false,
            });
        }
    }
}

#[cfg(test)]
#[path = "finite_close/tests.rs"]
mod tests;
