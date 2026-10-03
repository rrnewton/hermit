//! Helper receipts belong to the existing physical StreamCall before effects.
//! This module provides custody only. There is no semantic discharge until the
//! native byte frontier, foreground memory and release joins are implemented.
use std::sync::Arc;

use super::*;
use crate::network_runtime::HelperCopyBinding;

impl NetworkReplayEngine {
    /// The private binding is issued by native_peer from its original held pin.
    /// Read immutable metadata only: never lock helper/custody under the engine.
    pub(crate) fn bind_helper_copy(
        &mut self,
        binding: Arc<HelperCopyBinding>,
    ) -> Result<(), NetworkReplayError> {
        let owner = binding.owner();
        let call = binding.call();
        let lease = binding.lease();
        let state = self.owned_stream_call(owner, call)?;
        if self.mode() != NetworkEngineMode::Record
            || !self.shadow_mode()
            || !state.physical_pin_required
            || state.phase != StreamCallPhase::Active
            || state.helper_copy.is_some()
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let (actual_call, pending) = if self.shadow_probes.contains_key(&lease) {
            let probe = self.owned_shadow_probe(owner, lease)?;
            if !matches!(binding.effect(), NetworkStreamPhysicalEffect::Peek { .. }) {
                return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
            }
            (probe.call, probe.pending.as_ref())
        } else {
            let delivery = self.owned_shadow_delivery(owner, lease)?;
            if !matches!(binding.effect(), NetworkStreamPhysicalEffect::Drain { .. }) {
                return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
            }
            (delivery.call, delivery.pending.as_ref())
        };
        if actual_call != call || pending != Some(binding.effect()) {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        }
        self.stream_calls
            .get_mut(&call)
            .expect("validated active Call")
            .helper_copy = Some(binding);
        Ok(())
    }

    pub(super) fn require_no_helper_copy(
        &self,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        if let Some(binding) = self
            .stream_calls
            .get(&call)
            .and_then(|state| state.helper_copy.as_ref())
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(
                binding.lease(),
            ));
        }
        Ok(())
    }

    pub(super) fn require_no_helper_copy_for_lease(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<(), NetworkReplayError> {
        // A transferred private delivery has a new lease. Its unresolved raw
        // owner remains on the same Call; the old probe lease is not an escape.
        let call = self
            .shadow_deliveries
            .get(&lease)
            .map(|delivery| delivery.call)
            .or_else(|| self.shadow_probes.get(&lease).map(|probe| probe.call));
        if let Some(call) = call {
            if self
                .stream_calls
                .get(&call)
                .is_some_and(|state| state.owner != owner)
            {
                return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
            }
            self.require_no_helper_copy(call)?;
        }
        for state in self.stream_calls.values() {
            if let Some(binding) = state
                .helper_copy
                .as_ref()
                .filter(|binding| binding.lease() == lease)
            {
                if binding.owner() != owner {
                    return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
                }
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn controlled_pending_helper() -> (
        Self,
        NetworkStreamOwner,
        NetworkStreamCallId,
        NetworkStreamLeaseId,
        NetworkStreamPhysicalEffect,
    ) {
        let (mut engine, _, owner, call) = super::tests::shadow_probe_fixture();
        let probe = engine
            .begin_shadow_probe(
                owner,
                call,
                LogicalTime::from_nanos(1_790_000_000_000_000_001),
            )
            .unwrap();
        engine
            .submit_stream_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::ReadPeekOffset,
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PeekOffset(-1),
            )
            .unwrap();
        let effect = NetworkStreamPhysicalEffect::Peek { maximum: 1024 };
        engine
            .submit_stream_physical(owner, probe.lease, effect.clone())
            .unwrap();
        (engine, owner, call, probe.lease, effect)
    }
}

/// Component-only logical owner variant. The real supplied runtime separately
/// owns its PIDFD/Root and held helper FD; this creates no production authority.
#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn controlled_pending_helper_for_owner(
        owner: NetworkStreamOwner,
    ) -> (
        Self,
        NetworkStreamCallId,
        NetworkStreamLeaseId,
        NetworkStreamPhysicalEffect,
    ) {
        use chrono::TimeZone;
        let mut engine = Self::record_shadow(chrono::Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        let file = OpenFileId::new_socket(owner.thread, 0);
        let profile = super::tests::test_fresh_profile(libc::AF_INET);
        engine
            .register_stream_socket(
                file,
                profile.key,
                super::tests::test_socket_namespace(),
                Some(profile),
            )
            .unwrap();
        engine
            .ensure_channel(
                file,
                NetworkChannelBinding {
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::OutboundClient,
                    peer_address: Some(NetworkAddressV2::Inet4 {
                        address: [192, 0, 2, 1],
                        port: 443,
                    }),
                    requested_local_constraint: None,
                    observed_local_address: None,
                    accepted_from: None,
                    selected_channel: None,
                },
            )
            .unwrap();
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, control).unwrap().id;
        engine
            .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let lease = engine
            .begin_shadow_probe(
                owner,
                call,
                LogicalTime::from_nanos(1_790_000_000_000_000_001),
            )
            .unwrap()
            .lease;
        engine
            .submit_stream_physical(owner, lease, NetworkStreamPhysicalEffect::ReadPeekOffset)
            .unwrap();
        engine
            .confirm_stream_physical(owner, lease, NetworkStreamPhysicalResult::PeekOffset(-1))
            .unwrap();
        let effect = NetworkStreamPhysicalEffect::Peek { maximum: 1024 };
        engine
            .submit_stream_physical(owner, lease, effect.clone())
            .unwrap();
        (engine, call, lease, effect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_engine_requires_exact_live_call_owner_lease_and_submitted_effect_before_binding() {
        for variant in 0..7 {
            let (mut engine, owner, call, lease, effect) =
                NetworkReplayEngine::controlled_pending_helper();
            let mut changed_owner = owner;
            let mut changed_call = call;
            let mut changed_lease = lease;
            let mut changed_effect = effect.clone();
            match variant {
                0 => changed_owner.thread = crate::types::DetTid::from_raw(99),
                1 => changed_owner.mm = owner.mm.for_exec(owner.thread),
                2 => changed_call = NetworkStreamCallId::controlled_fixture(999),
                3 => changed_lease = serde_json::from_value(serde_json::json!(999)).unwrap(),
                4 => changed_effect = NetworkStreamPhysicalEffect::Peek { maximum: 2048 },
                5 => changed_effect = NetworkStreamPhysicalEffect::Drain { maximum: 3 },
                6 => engine.shadow_probes.get_mut(&lease).unwrap().pending = None,
                _ => unreachable!(),
            }
            let before = format!("{engine:?}");
            let binding = HelperCopyBinding::controlled_fixture(
                changed_owner,
                changed_call,
                changed_lease,
                changed_effect,
            );
            assert!(
                engine.bind_helper_copy(binding).is_err(),
                "variant {variant}"
            );
            assert_eq!(format!("{engine:?}"), before);
            assert!(engine.stream_calls[&call].helper_copy.is_none());
        }
    }
    #[test]
    fn helper_engine_keeps_unjoined_custody_before_any_confirm_publish_or_retirement() {
        let (mut engine, owner, call, lease, effect) =
            NetworkReplayEngine::controlled_pending_helper();
        let binding = HelperCopyBinding::controlled_fixture(owner, call, lease, effect.clone());
        let weak = Arc::downgrade(&binding);
        engine.bind_helper_copy(binding.clone()).unwrap();
        let before = format!("{engine:?}");
        assert!(engine.bind_helper_copy(binding.clone()).is_err());
        assert!(
            engine
                .bind_helper_copy(HelperCopyBinding::controlled_fixture(
                    owner,
                    call,
                    lease,
                    effect.clone()
                ))
                .is_err()
        );
        assert!(
            engine
                .confirm_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalResult::Peeked { count: 3 }
                )
                .is_err()
        );
        assert!(
            engine
                .confirm_shadow_probe_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalResult::Peeked { count: 3 }
                )
                .is_err()
        );
        assert!(engine.submit_stream_physical(owner, lease, effect).is_err());
        assert!(
            engine
                .complete_shadow_probe(
                    owner,
                    lease,
                    LogicalTime::from_nanos(1_790_000_000_000_000_002),
                    b"abc".to_vec(),
                    false
                )
                .is_err()
        );
        assert!(engine.begin_stream_call_release(owner, call).is_err());
        assert!(engine.finish_stream_call_release(owner, call).is_err());
        assert_eq!(format!("{engine:?}"), before);
        drop(binding);
        assert!(weak.upgrade().is_some());
        assert!(engine.check_stream_operations_finished().is_err());
        assert!(engine.native_stream_final_wait(owner));
        assert!(
            engine
                .terminal_stream_admission(owner, call)
                .unwrap()
                .is_some()
        );
        assert!(weak.upgrade().is_some());
        assert!(engine.check_stream_operations_finished().is_err());
    }
    #[test]
    fn helper_drain_binding_keeps_consumption_and_selection_unchanged_until_real_join() {
        let (mut engine, owner, call, probe, _) = NetworkReplayEngine::controlled_pending_helper();
        // Controlled existing-model setup; this is not native layout evidence.
        engine
            .confirm_stream_physical(
                owner,
                probe,
                NetworkStreamPhysicalResult::Peeked { count: 3 },
            )
            .unwrap();
        engine
            .submit_stream_physical(owner, probe, NetworkStreamPhysicalEffect::PollState)
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                probe,
                NetworkStreamPhysicalResult::PollState {
                    revents: libc::POLLIN,
                },
            )
            .unwrap();
        engine
            .submit_stream_physical(owner, probe, NetworkStreamPhysicalEffect::QueuedBytes)
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                probe,
                NetworkStreamPhysicalResult::QueuedBytes { count: 3 },
            )
            .unwrap();
        engine
            .complete_shadow_probe(
                owner,
                probe,
                LogicalTime::from_nanos(1_790_000_000_000_000_002),
                b"abc".to_vec(),
                false,
            )
            .unwrap();
        let NetworkStreamChunk::Reserved { lease, .. } =
            engine.reserve_stream_call_chunk(owner, call, 3, 0).unwrap()
        else {
            panic!("controlled published bytes require a delivery")
        };
        engine.begin_record_drain(owner, lease).unwrap();
        let effect = NetworkStreamPhysicalEffect::Drain { maximum: 3 };
        engine
            .submit_stream_physical(owner, lease, effect.clone())
            .unwrap();
        engine
            .bind_helper_copy(HelperCopyBinding::controlled_fixture(
                owner, call, lease, effect,
            ))
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .confirm_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalResult::Drained {
                        bytes: b"abc".to_vec()
                    }
                )
                .is_err()
        );
        assert!(engine.finish_record_drain(owner, lease).is_err());
        assert!(
            engine
                .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
                .is_err()
        );
        assert_eq!(engine.shadow_deliveries[&lease].drained, 0);
        assert_eq!(format!("{engine:?}"), before);
        assert!(
            matches!(engine.finish(),Err(NetworkReplayError::UnresolvedStreamCall(id)) if id==call)
        );
    }
}

impl NetworkReplayEngine {
    /// Explicit shared-Record adapter. The legacy binder above remains strict.
    pub(crate) fn bind_shared_helper_copy(
        &mut self,
        binding: Arc<HelperCopyBinding>,
        step: &Arc<shared_waits::SharedEffectIdentity>,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shared_record_effect(step)?;
        let origin = step.origin();
        let state = self
            .stream_calls
            .get(&origin.call())
            .ok_or(NetworkReplayError::UnknownStreamCall(origin.call()))?;
        if state.helper_copy.is_some()
            || binding.owner() != origin.owner()
            || binding.call() != origin.call()
            || binding.lease() != origin.lease()
            || binding.effect() != step.effect()
            || !matches!(step.effect(), NetworkStreamPhysicalEffect::Peek { .. })
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(
                origin.lease(),
            ));
        }
        self.stream_calls
            .get_mut(&origin.call())
            .unwrap()
            .helper_copy = Some(binding);
        Ok(())
    }
}
