//! Pending-send authority. These values are not trace data or RPC messages.

use std::sync::Arc;

#[cfg(test)]
use super::NetworkReplayEngine;
#[cfg(test)]
use super::NetworkReplayError;
use super::NetworkStreamCallId;
use super::NetworkStreamLeaseId;
use super::NetworkStreamOwner;
#[cfg(test)]
use crate::scheduler::send_handback::SendHandbackReceipt;

/// Issued only from the engine's existing retained pending Call/control.
/// This proves neither a physical send nor an original-task return.
#[derive(Debug)]
pub(crate) struct PendingSendTiming {
    // Keep the complete pending identity and its original drop order even while
    // only the controlled timing consumer reads the non-owner fields.
    pub(super) _identity: Arc<()>,
    pub(super) owner: NetworkStreamOwner,
    pub(super) _call: NetworkStreamCallId,
    pub(super) _lease: NetworkStreamLeaseId,
    pub(super) _normal_epoch: u64,
}

impl PendingSendTiming {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    #[cfg(test)]
    pub(crate) fn normal_epoch(&self) -> u64 {
        self._normal_epoch
    }
    #[cfg(test)]
    pub(crate) fn same_pending(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self._identity, &other._identity)
            && self.owner == other.owner
            && self._call == other._call
            && self._lease == other._lease
            && self._normal_epoch == other._normal_epoch
    }

    #[cfg(test)]
    pub(crate) fn fixture(owner: NetworkStreamOwner, call: u64) -> Self {
        Self {
            _identity: Arc::new(()),
            owner,
            _call: NetworkStreamCallId::controlled_fixture(call),
            _lease: NetworkStreamLeaseId::controlled_fixture(call),
            _normal_epoch: 1,
        }
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Claim timing enrollment once, before submission. Caller must enroll
    /// while holding the scheduler -> engine locks and the same Normal gate.
    pub(crate) fn take_pending_send_timing(
        &mut self,
        lease: NetworkStreamLeaseId,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: crate::types::LogicalTime,
    ) -> Result<PendingSendTiming, NetworkReplayError> {
        let owner = grant.owner();
        let control = self.owned_socket_control(owner, lease)?;
        let open_file = control.open_file;
        let pending = control
            .physical
            .transmit_pending
            .as_ref()
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        if pending.submitted || pending._timing_claimed {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        // The pending source entry must be from this exact retained sole-root
        // Normal epoch, not merely another later grant for the same numeric TID.
        self.validate_native_foreground_call(pending.call, grant, now)?;
        let pending = self
            .socket_controls
            .get_mut(&open_file)
            .unwrap()
            .physical
            .transmit_pending
            .as_mut()
            .unwrap();
        pending._timing_claimed = true;
        pending._timing_normal_epoch = Some(grant.epoch());
        let identity = Arc::new(());
        pending._timing_identity = Some(identity.clone());
        Ok(PendingSendTiming {
            _identity: identity,
            owner,
            _call: pending.call,
            _lease: lease,
            _normal_epoch: grant.epoch(),
        })
    }

    /// Retain scheduler evidence once; do NOT treat it as physical-return
    /// authority or write it into legacy V4 output rows. The latter writer
    /// explicitly refuses an enrolled attempt until a versioned writer exists.
    pub(crate) fn accept_send_handback(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        receipt: SendHandbackReceipt,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let open_file = control.open_file;
        let pending = control
            .physical
            .transmit_pending
            .as_ref()
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        let expected = PendingSendTiming {
            _identity: pending
                ._timing_identity
                .clone()
                .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?,
            owner,
            _call: pending.call,
            _lease: lease,
            _normal_epoch: pending
                ._timing_normal_epoch
                .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?,
        };
        if !pending._timing_claimed
            || !pending.submitted
            || pending._timing_receipt.is_some()
            || !receipt.matches(&expected)
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        self.socket_controls
            .get_mut(&open_file)
            .unwrap()
            .physical
            .transmit_pending
            .as_mut()
            .unwrap()
            ._timing_receipt = Some(Arc::new(receipt));
        Ok(())
    }
}
