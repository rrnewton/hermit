//! One same-Call physical successor of an exact whole foreground store.
//! This does not publish a receive, consume semantic bytes or issue a return.
use std::sync::Arc;

use super::*;
use crate::network_runtime::HelperCopyBinding;
use crate::network_runtime::HelperCopyCompletion;
use crate::network_runtime::native_peer::Observation;

#[derive(Debug, Clone)]
pub(in crate::network_replay) struct PrivateDrain {
    pub(super) full: FullStoreCompletion,
    pub(super) before: Cut,
    pub(super) binding: Option<Arc<HelperCopyBinding>>,
    // Keep the actual raw result before validating its geometry or bytes.
    pub(super) observed: Option<Observation>,
    pub(super) physically_joined: bool,
    pub(super) matched: bool,
    pub(super) publication: Option<Arc<PreparedPrivatePublication>>,
}

impl NetworkReplayEngine {
    /// Latch the one-use successor on the existing Call before any worker can
    /// submit. No caller-provided byte count or cloned handle issues a retry.
    pub(crate) fn begin_private_drain(
        &mut self,
        full: &FullStoreCompletion,
    ) -> Result<NetworkStreamPhysicalEffect, NetworkReplayError> {
        let store = full.store();
        store.record_completion()?;
        self.check_foreground_store(store)?;
        let state = self.owned_stream_call(store.owner(), store.call())?;
        if !full.has_ended_full_store()
            || !store.root().is_current(store.owner())
            || store.source_offset() != 0
            || state.private_drain.is_some()
        {
            return Err(invalid(
                "private Drain lacks exact ended whole-store authority",
            ));
        }
        let before = state
            .private_receive
            .as_ref()
            .expect("validated source")
            .cut;
        let effect = NetworkStreamPhysicalEffect::Drain {
            maximum: store.length(),
        };
        self.stream_calls
            .get_mut(&store.call())
            .unwrap()
            .private_drain = Some(PrivateDrain {
            full: full.clone(),
            before,
            binding: None,
            observed: None,
            physically_joined: false,
            matched: false,
            publication: None,
        });
        let delivery = self.shadow_deliveries.get_mut(&store.lease()).unwrap();
        delivery.drain_started = true;
        delivery.pending = Some(effect.clone());
        Ok(effect)
    }

    /// Only the actual successor Pending can issue this Binding. Its immutable
    /// predecessor joins the positively retired old probe and same held file.
    pub(crate) fn bind_private_drain_helper(
        &mut self,
        full: &FullStoreCompletion,
        binding: Arc<HelperCopyBinding>,
    ) -> Result<(), NetworkReplayError> {
        let store = full.store();
        let record_source = store.record_completion()?;
        let state = self.owned_stream_call(store.owner(), store.call())?;
        let delivery = self.owned_shadow_delivery(store.owner(), store.lease())?;
        let drain = state
            .private_drain
            .as_ref()
            .ok_or_else(|| invalid("private Drain was not latched"))?;
        if state.phase != StreamCallPhase::Active
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || !full.has_ended_full_store()
            || !store.root().is_current(store.owner())
            || drain.full != *full
            || drain.binding.is_some()
            || drain.observed.is_some()
            || state
                .foreground_store
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, store))
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, record_source.binding()))
            || binding.owner() != store.owner()
            || binding.call() != store.call()
            || binding.lease() != store.lease()
            || !binding.succeeds(record_source)
            || binding.effect()
                != &(NetworkStreamPhysicalEffect::Drain {
                    maximum: store.length(),
                })
            || delivery.pending.as_ref() != Some(binding.effect())
            || !delivery.drain_started
            || delivery.drained != 0
            || delivery.private_offset != Some(0)
            || delivery.selected_len != store.length()
            || self.stream_operations.get(&store.lease()).is_none_or(|operation| {
                operation.owner != store.owner() || operation.abandoned
                    || self.stream_delivery.get(&operation.open_file) != Some(&store.lease())
                    || self.channels[&operation.channel].inbound_consumed != drain.before.bytes
                    || !matches!(&operation.kind,
                        StreamOperationKind::Delivery { at_offset, peek_offset: 0, selection_len, .. }
                        if *at_offset == drain.before.bytes && *selection_len == store.length())
            })
        {
            return Err(invalid(
                "private Drain changed its exact same-file successor",
            ));
        }
        self.check_private_receive_cut(
            store.owner(),
            store.call(),
            state.private_receive.as_ref().unwrap(),
        )?;
        let state = self.stream_calls.get_mut(&store.call()).unwrap();
        state.private_drain.as_mut().unwrap().binding = Some(binding.clone());
        state.helper_copy = Some(binding);
        Ok(())
    }

    /// Runtime exact-Pending preflight precedes this call. The raw completion
    /// stays retained on all later refusals, even where geometry cannot join.
    pub(super) fn confirm_private_drain(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed: &Observation,
        completion: &HelperCopyCompletion,
    ) -> Result<(), NetworkReplayError> {
        let delivery = self.owned_shadow_delivery(owner, lease)?;
        let call = delivery.call;
        let state = self.owned_stream_call(owner, call)?;
        let drain = state
            .private_drain
            .as_ref()
            .ok_or_else(|| invalid("private Drain lacks retained store"))?;
        let store = drain.full.store().clone();
        let record_source = store.record_completion()?;
        if drain.observed.is_some()
            || store.owner() != owner
            || store.lease() != lease
            || drain
                .binding
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
            || observed.helper_copy.as_ref() != Some(completion)
            || delivery.pending.as_ref() != Some(completion.binding().effect())
        {
            return Err(invalid(
                "private Drain completion changed exact retained Pending",
            ));
        }
        let before = drain.before;
        self.stream_calls
            .get_mut(&call)
            .unwrap()
            .private_drain
            .as_mut()
            .unwrap()
            .observed = Some(observed.clone());
        completion
            .joined_worker()
            .map_err(|e| invalid(&e.to_string()))?;
        let capture = completion.capture();
        if capture.manifest.present != 1
            || capture.manifest.summary.version != 5
            || completion.attempts().len() != capture.units.len()
            || observed.bytes != capture.committed
        {
            return Err(invalid(
                "private Drain lacks canonical copy5 result coverage",
            ));
        }
        let returned = if let Some(errno) = observed.errno {
            if errno <= 0
                || observed.raw_return != -1
                || capture.manifest.returned != -i64::from(errno)
                || observed.confirmation != NetworkStreamPhysicalResult::Errno(errno)
                || !capture.committed.is_empty()
            {
                return Err(invalid("private Drain changed actual negative result"));
            }
            0
        } else {
            let n = usize::try_from(observed.raw_return)
                .map_err(|_| invalid("private Drain has invalid raw return"))?;
            if capture.manifest.returned != observed.raw_return
                || n != capture.committed.len()
                || n > store.length()
                || observed.confirmation
                    != (NetworkStreamPhysicalResult::Drained {
                        bytes: capture.committed.clone(),
                    })
            {
                return Err(invalid("private Drain changed actual successful result"));
            }
            n
        };
        let mut cursor = before;
        let mut copied = 0u64;
        for (index, (attempt, unit)) in completion.attempts().iter().zip(&capture.units).enumerate()
        {
            let geometry = unit
                .observation
                .ok_or_else(|| invalid("private Drain lacks Consume geometry"))?;
            let native = &unit.native;
            if !completion.binding().owns_attempt(attempt)
                || attempt.operation() != 21
                || attempt.ordinal() != index
                || attempt.unit() != unit
                || native.disposition != CONSUME
                || !matches!(native.returned, 0 | -14)
                || native.offset != copied
                || geometry.begin.before != cursor.bytes
                || geometry.begin.start != cursor.bytes
                || geometry.begin.order != cursor.order
            {
                return Err(invalid(
                    "private Drain changed actual contiguous Consume chain",
                ));
            }
            if native.returned == 0 {
                if native.copied == 0
                    || native.copied != native.requested
                    || cursor.bytes.checked_add(native.copied) != Some(geometry.after)
                    || cursor.order.checked_add(1) != Some(native.order)
                {
                    return Err(invalid(
                        "private Drain successful unit changed actual consumption",
                    ));
                }
                copied = copied
                    .checked_add(native.copied)
                    .ok_or(NetworkReplayError::Overflow)?;
                cursor = Cut {
                    bytes: geometry.after,
                    order: native.order,
                };
            } else if native.copied >= native.requested
                || geometry.after != cursor.bytes
                || native.order != cursor.order
            {
                return Err(invalid(
                    "private Drain failed unit changed its retained nonconsuming cut",
                ));
            }
        }
        if usize::try_from(copied).ok() != Some(returned) {
            return Err(invalid(
                "private Drain count is not its actual Consume prefix",
            ));
        }
        // This authenticates original file identity and current predecessor and
        // commits only physical history. Even a short/mismatching result keeps
        // the known real prefix; it never authorizes retry or semantic release.
        self.retain_native_receive_attempts(owner, call, completion.attempts())?;
        let matched = observed.errno.is_none()
            && returned == store.length()
            && capture.committed == record_source.capture().committed[..store.length()];
        let drain = self
            .stream_calls
            .get_mut(&call)
            .unwrap()
            .private_drain
            .as_mut()
            .unwrap();
        drain.physically_joined = true;
        drain.matched = matched;
        if !matched {
            return Err(invalid(
                "private Drain did not reconcile the exact whole stored prefix",
            ));
        }
        let delivery = self.shadow_deliveries.get_mut(&lease).unwrap();
        delivery.drained = returned;
        delivery.pending = None;
        Ok(())
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn private_drain_fixture_state(
        &self,
        call: NetworkStreamCallId,
    ) -> (String, (u64, u64), (bool, bool, bool), usize) {
        let state = &self.stream_calls[&call];
        let file = state.open_file.unwrap();
        let shadow = self.shadow.as_ref().unwrap();
        let socket = &shadow.sockets[&file];
        let cut = socket.native.as_ref().unwrap().physical_observed;
        let drain = state.private_drain.as_ref();
        (
            format!(
                "{:?}/{:?}/{:?}/{:?}",
                self.channels, self.mode, socket.profile, shadow.units
            ),
            (cut.bytes, cut.order),
            (
                drain.is_some_and(|d| d.observed.is_some()),
                drain.is_some_and(|d| d.physically_joined),
                drain.is_some_and(|d| d.matched),
            ),
            state.native_receive.len(),
        )
    }
}
