//! Pending-send authority. These values are not trace data or RPC messages.

use std::sync::Arc;

use super::NetworkReplayEngine;
use super::NetworkReplayError;
use super::NetworkStreamCallId;
use super::NetworkStreamLeaseId;
use super::NetworkStreamOwner;
use crate::scheduler::send_handback::SendHandbackReceipt;

/// Issued only from the engine's existing retained pending Call/control.
/// This proves neither a physical send nor an original-task return.
#[derive(Debug)]
pub(crate) struct PendingSendTiming {
    pub(super) identity: Arc<()>,
    pub(super) owner: NetworkStreamOwner,
    pub(super) call: NetworkStreamCallId,
    pub(super) lease: NetworkStreamLeaseId,
    pub(super) normal_epoch: u64,
}

impl PendingSendTiming {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.call
    }
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
    pub(crate) fn normal_epoch(&self) -> u64 {
        self.normal_epoch
    }
    pub(crate) fn same_pending(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
            && self.owner == other.owner
            && self.call == other.call
            && self.lease == other.lease
            && self.normal_epoch == other.normal_epoch
    }

    #[cfg(test)]
    pub(crate) fn fixture(owner: NetworkStreamOwner, call: u64) -> Self {
        Self {
            identity: Arc::new(()),
            owner,
            call: NetworkStreamCallId::controlled_fixture(call),
            lease: NetworkStreamLeaseId::controlled_fixture(call),
            normal_epoch: 1,
        }
    }
}

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
        if pending.submitted || pending.timing_claimed {
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
        pending.timing_claimed = true;
        pending.timing_normal_epoch = Some(grant.epoch());
        let identity = Arc::new(());
        pending.timing_identity = Some(identity.clone());
        Ok(PendingSendTiming {
            identity,
            owner,
            call: pending.call,
            lease,
            normal_epoch: grant.epoch(),
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
            identity: pending
                .timing_identity
                .clone()
                .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?,
            owner,
            call: pending.call,
            lease,
            normal_epoch: pending
                .timing_normal_epoch
                .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?,
        };
        if !pending.timing_claimed
            || !pending.submitted
            || pending.timing_receipt.is_some()
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
            .timing_receipt = Some(Arc::new(receipt));
        Ok(())
    }
}
