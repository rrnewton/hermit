//! Private canonical PEEK bytes on the existing Call and delivery ownership.
//! No byte view here is a Linux rollback unit, environment release or store grant.
use super::*;
use crate::network_runtime::HelperCopyCompletion;
use crate::network_runtime::native_peer::Observation;
use crate::network_runtime::original_read_copy::OBSERVE;

#[derive(Clone, Debug)]
pub(in crate::network_replay) struct PrivateSource {
    pub(super) completion: HelperCopyCompletion,
    pub(super) cut: Cut,
    pub(super) length: usize,
}
impl PrivateSource {
    fn checked(completion: &HelperCopyCompletion) -> Result<Self, NetworkReplayError> {
        completion
            .joined_worker()
            .map_err(|error| invalid(&error.to_string()))?;
        let capture = completion.capture();
        let returned = usize::try_from(capture.manifest.returned)
            .map_err(|_| invalid("private receive requires actual successful PEEK bytes"))?;
        if capture.manifest.present != 1
            || capture.manifest.summary.version != 5
            || returned == 0
            || returned != capture.committed.len()
            || completion.attempts().len() != capture.units.len()
            || capture.units.is_empty()
        {
            return Err(invalid(
                "private receive requires complete canonical copy5 PEEK coverage",
            ));
        }
        let mut cut = None;
        let mut copied = 0u64;
        for (index, (attempt, unit)) in completion.attempts().iter().zip(&capture.units).enumerate()
        {
            let begin = unit
                .observation
                .ok_or_else(|| invalid("private receive lacks actual geometry"))?
                .begin;
            let next_cut = Cut {
                bytes: begin.before,
                order: begin.order,
            };
            if !completion.binding().owns_attempt(attempt)
                || attempt.operation() != 22
                || attempt.ordinal() != index
                || attempt.unit() != unit
                || unit.native.disposition != OBSERVE
                || unit.native.returned != 0
                || unit.native.offset != copied
                || unit.native.copied != unit.native.requested
                || begin.before.checked_add(copied) != Some(begin.start)
                || cut.is_some_and(|cut| cut != next_cut)
            {
                return Err(invalid(
                    "private PEEK changed its exact canonical traversal or cut",
                ));
            }
            cut = Some(next_cut);
            copied = copied
                .checked_add(unit.native.copied)
                .ok_or(NetworkReplayError::Overflow)?;
        }
        if usize::try_from(copied).ok() != Some(returned) {
            return Err(invalid(
                "private PEEK cannot infer bytes from available extent",
            ));
        }
        Ok(Self {
            completion: completion.clone(),
            cut: cut.expect("nonempty capture"),
            length: returned,
        })
    }
    fn check_overlap(&self, other: &Self) -> Result<(), NetworkReplayError> {
        let end = self
            .cut
            .bytes
            .checked_add(self.length as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let other_end = other
            .cut
            .bytes
            .checked_add(other.length as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let first = self.cut.bytes.max(other.cut.bytes);
        let last = end.min(other_end);
        if first < last {
            let a = usize::try_from(first - self.cut.bytes)
                .map_err(|_| NetworkReplayError::Overflow)?;
            let b = usize::try_from(first - other.cut.bytes)
                .map_err(|_| NetworkReplayError::Overflow)?;
            let n = usize::try_from(last - first).map_err(|_| NetworkReplayError::Overflow)?;
            if self.completion.capture().committed[a..a + n]
                != other.completion.capture().committed[b..b + n]
            {
                return Err(invalid(
                    "native observations disagree on immutable stream bytes",
                ));
            }
        }
        Ok(())
    }
    fn view(&self, offset: usize, maximum: usize) -> Result<Vec<u8>, NetworkReplayError> {
        if maximum > NETWORK_STREAM_CHUNK_LIMIT {
            return Err(NetworkReplayError::StreamChunkTooLarge(maximum));
        }
        if offset > self.length {
            return Err(invalid("private view left its actual returned bytes"));
        }
        let end = offset
            .checked_add(maximum.min(self.length - offset))
            .ok_or(NetworkReplayError::Overflow)?;
        Ok(self.completion.capture().committed[offset..end].to_vec())
    }
}

impl NetworkReplayEngine {
    /// The caller first validates the controller's retained Pending/result.
    /// Unlike the plain serialized API this branch requires canonical custody
    /// and an actual joined worker; it never discharges the semantic fence.
    pub(crate) fn confirm_retained_stream_physical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed: &Observation,
    ) -> Result<(), NetworkReplayError> {
        if let Some(completion) = &observed.helper_copy {
            if matches!(
                completion.binding().effect(),
                NetworkStreamPhysicalEffect::Drain { .. }
            ) {
                return self.confirm_private_drain(owner, lease, observed, completion);
            }
            if matches!(&self.mode, EngineState::Native(n) if n.mode() == NetworkEngineMode::Record)
                && completion.capture().manifest.returned <= 0
            {
                return self.confirm_record_no_store(owner, lease, observed);
            }
            return self.confirm_private_peek(owner, lease, observed, completion);
        }
        if self
            .shadow_probes
            .get(&lease)
            .and_then(|probe| self.stream_calls.get(&probe.call))
            .is_some_and(|call| (call.private_receive.is_some() || call.record_no_store.is_some()))
        {
            return self.confirm_private_cursor_restore(owner, lease, observed);
        }
        // Missing/serde-stripped helper proof reaches the unchanged guard.
        self.confirm_stream_physical(owner, lease, observed.confirmation.clone())
    }

    fn confirm_private_peek(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed: &Observation,
        completion: &HelperCopyCompletion,
    ) -> Result<(), NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, lease)?;
        let state = self.owned_stream_call(owner, probe.call)?;
        let binding = completion.binding();
        let Some(NetworkStreamPhysicalEffect::Peek { maximum }) = &probe.pending else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        if self.mode() != NetworkEngineMode::Record
            || (state.private_receive.is_some() || state.record_no_store.is_some())
            || probe.peek.is_some()
            || binding.owner() != owner
            || binding.call() != probe.call
            || binding.lease() != lease
            || Some(binding.effect()) != probe.pending.as_ref()
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|held| !std::sync::Arc::ptr_eq(held, binding))
        {
            return Err(invalid(
                "private PEEK changed its exact pending Call and effect",
            ));
        }
        let source = PrivateSource::checked(completion)?;
        if source.length < probe.retained_prefix
            || source.length > *maximum
            || observed.errno.is_some()
            || observed.raw_return != source.length as i64
            || observed.confirmation
                != (NetworkStreamPhysicalResult::Peeked {
                    count: source.length,
                })
            || observed.bytes != completion.capture().committed[probe.retained_prefix..]
        {
            return Err(invalid(
                "private PEEK changed its actual returned result or suffix",
            ));
        }
        let call = probe.call;
        self.check_private_receive_cut(owner, call, &source)?;
        self.check_private_published_prefix(probe, &source)?;
        // All source/result checks precede the physical history transaction.
        // Once that transaction succeeds, only infallible local commits follow.
        self.retain_native_receive_attempts(owner, call, completion.attempts())?;
        self.stream_calls.get_mut(&call).unwrap().private_receive = Some(source);
        let probe = self.shadow_probes.get_mut(&lease).unwrap();
        probe.peek = Some(Ok(completion.capture().committed.len()));
        probe.pending = None;
        Ok(())
    }

    fn check_private_published_prefix(
        &self,
        probe: &ShadowProbeState,
        source: &PrivateSource,
    ) -> Result<(), NetworkReplayError> {
        let channel = self
            .channels
            .get(&probe.channel)
            .expect("probe pins channel");
        if channel.inbound_consumed != source.cut.bytes {
            return Err(invalid(
                "private PEEK changed the retained semantic prefix origin",
            ));
        }
        let mut compared = 0usize;
        for input in &channel.inbound {
            if compared == probe.retained_prefix {
                break;
            }
            let InboundOutcome::Stream {
                bytes,
                requires_message_io: false,
                ..
            } = input
            else {
                return Err(invalid(
                    "private PEEK cannot skip a control or ancillary prefix",
                ));
            };
            let length = bytes.len().min(probe.retained_prefix - compared);
            if !bytes
                .iter()
                .take(length)
                .copied()
                .eq(
                    source.completion.capture().committed[compared..compared + length]
                        .iter()
                        .copied(),
                )
            {
                return Err(invalid(
                    "private PEEK disagrees with already published stream bytes",
                ));
            }
            compared += length;
        }
        if compared != probe.retained_prefix {
            return Err(invalid(
                "private PEEK retained prefix is missing from the semantic queue",
            ));
        }
        Ok(())
    }

    pub(super) fn check_private_receive_cut(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        source: &PrivateSource,
    ) -> Result<(), NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        let origin = self
            .shadow
            .as_ref()
            .and_then(|shadow| shadow.sockets.get(&open_file))
            .and_then(|socket| socket.native.as_ref())
            .ok_or_else(|| invalid("private PEEK lacks original installed origin"))?;
        if source.cut != origin.physical_observed {
            return Err(invalid("private PEEK is not at the joined physical cut"));
        }
        for prior in self
            .stream_calls
            .values()
            .filter(|state| state.open_file == Some(open_file))
            .filter_map(|state| state.private_receive.as_ref())
        {
            source.check_overlap(prior)?;
        }
        // A known future/missing native predecessor is not a current layout.
        // Absence here still is not native-submission exclusion: the future
        // foreground memory grant must join/exclude actual worker execution.
        if self
            .stream_calls
            .values()
            .filter(|state| state.open_file == Some(open_file))
            .any(|state| state.native_receive.iter().any(|attempt| !attempt.joined))
        {
            return Err(invalid(
                "private PEEK has unresolved native physical history",
            ));
        }
        Ok(())
    }

    /// Only the native adapter may restore the already retained original
    /// SO_PEEK_OFF. No new Peek/Poll/FIONREAD/Drain is admitted by this method.
    pub(crate) fn submit_retained_stream_physical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: NetworkStreamPhysicalEffect,
    ) -> Result<(), NetworkReplayError> {
        let private = self
            .shadow_probes
            .get(&lease)
            .and_then(|probe| self.stream_calls.get(&probe.call))
            .is_some_and(|call| (call.private_receive.is_some() || call.record_no_store.is_some()));
        if !private {
            return self.submit_stream_physical(owner, lease, effect);
        }
        let probe = self.owned_shadow_probe(owner, lease)?;
        let NetworkStreamPhysicalEffect::SetPeekOffset { value } = effect else {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        };
        if probe.pending.is_some()
            || probe.peek.is_none()
            || !probe.cursor_observed
            || value < 0
            || probe.original_cursor != Some(value)
            || probe.current_cursor != Some(-1)
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        self.shadow_probes.get_mut(&lease).unwrap().pending = Some(effect);
        Ok(())
    }

    fn confirm_private_cursor_restore(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed: &Observation,
    ) -> Result<(), NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, lease)?;
        let Some(NetworkStreamPhysicalEffect::SetPeekOffset { value }) = probe.pending else {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        };
        if value < 0
            || probe.original_cursor != Some(value)
            || probe.current_cursor != Some(-1)
            || observed.raw_return != 0
            || observed.errno.is_some()
            || !observed.bytes.is_empty()
            || observed.confirmation != NetworkStreamPhysicalResult::Unit
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let probe = self.shadow_probes.get_mut(&lease).unwrap();
        probe.current_cursor = Some(value);
        probe.pending = None;
        Ok(())
    }

    /// Retain a private view using the existing delivery ownership. This API is
    /// internal and does not publish bytes, readiness, trace or a syscall return.
    pub(crate) fn reserve_private_receive_span(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        probe_lease: NetworkStreamLeaseId,
        maximum: usize,
        peek_offset: usize,
    ) -> Result<NetworkStreamChunk, NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, probe_lease)?;
        if probe.call != call || probe.pending.is_some() || !probe.cursor_restored() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(probe_lease));
        }
        let state = self.owned_stream_call(owner, call)?;
        let source = state
            .private_receive
            .as_ref()
            .ok_or_else(|| invalid("private selection lacks retained canonical source"))?;
        self.check_private_receive_cut(owner, call, source)?;
        let open_file = self.stream_call_open_file(owner, call)?;
        let channel = probe.channel;
        let socket = self.stream_call_socket_state(owner, call)?;
        let queue = self.channels.get(&channel).expect("probe pins channel");
        if maximum == 0
            || peek_offset >= source.length
            || self.stream_delivery.contains_key(&open_file)
            || queue.inbound_consumed != source.cut.bytes
        {
            return Err(invalid(
                "private selection lacks an exact unconsumed physical prefix",
            ));
        }
        let selected_len = maximum.min(source.length - peek_offset);
        let outcome = NetworkStreamChunkOutcome::Bytes(
            source.view(peek_offset, selected_len.min(NETWORK_STREAM_CHUNK_LIMIT))?,
        );
        let next_consume_epoch = socket
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let at_offset = source.cut.bytes;
        let lease = self.allocate_stream_lease()?;
        // No fallible step after allocation or removal of short probe exclusion.
        self.stream_operations.insert(
            lease,
            StreamOperation {
                owner,
                open_file,
                channel,
                abandoned: false,
                kind: StreamOperationKind::Delivery {
                    at_offset,
                    peek_offset,
                    selection_len: selected_len,
                    outcome: outcome.clone(),
                },
            },
        );
        self.stream_delivery.insert(open_file, lease);
        self.shadow_deliveries.insert(
            lease,
            ShadowDeliveryState {
                call,
                private_offset: Some(peek_offset),
                selected_len,
                cursor_before: socket.options.peek_offset,
                next_consume_epoch,
                drain_started: false,
                drained: 0,
                peek_cursor_confirmed: false,
                pending: None,
            },
        );
        self.shadow_probes.remove(&probe_lease);
        self.socket_controls.remove(&open_file);
        Ok(NetworkStreamChunk::Reserved {
            lease,
            selection_len: selected_len,
            outcome,
        })
    }

    pub(in crate::network_replay) fn read_private_receive_view(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        offset: usize,
        maximum: usize,
    ) -> Result<Vec<u8>, NetworkReplayError> {
        if maximum > NETWORK_STREAM_CHUNK_LIMIT {
            return Err(NetworkReplayError::StreamChunkTooLarge(maximum));
        }
        let delivery = self.owned_shadow_delivery(owner, lease)?;
        let start = delivery
            .private_offset
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        let source = self
            .owned_stream_call(owner, delivery.call)?
            .private_receive
            .as_ref()
            .ok_or_else(|| invalid("private delivery lost canonical source"))?;
        if offset > delivery.selected_len {
            return Err(NetworkReplayError::StreamDeliveryChanged(lease));
        }
        // Historical immutable bytes remain private; this accessor makes no
        // assertion that current Linux rollback geometry is unchanged.
        source.view(
            start
                .checked_add(offset)
                .ok_or(NetworkReplayError::Overflow)?,
            maximum.min(delivery.selected_len - offset),
        )
    }

    /// Root's future store grant retains this exact target after the runtime
    /// removes Pending. No numeric lookup can reconstruct this opaque proof.
    pub(crate) fn private_receive_completion(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<&HelperCopyCompletion, NetworkReplayError> {
        let call = if let Some(probe) = self.shadow_probes.get(&lease) {
            self.owned_shadow_probe(owner, lease)?;
            probe.call
        } else {
            self.owned_shadow_delivery(owner, lease)?.call
        };
        self.owned_stream_call(owner, call)?
            .private_receive
            .as_ref()
            .map(|source| &source.completion)
            .ok_or_else(|| invalid("private source has no canonical completion"))
    }
}

#[cfg(test)]
#[path = "private_peek_tests.rs"]
mod tests;
