//! An immutable selection from this engine's existing Replay queue. It grants
//! neither guest stores nor consumption until the common foreground transaction.
use std::sync::Arc;

use super::*;

/// Immutable queue selection. It neither reserves a Delivery nor permits memory
/// access, and no wait may retain one of these plans.
#[derive(Debug)]
pub(crate) enum ReplayReceivePlan {
    Bytes(ReplayBytesPlan),
    NoStore(super::no_store::ReplayNoStorePlan),
    Wait,
}
#[derive(Debug)]
pub(crate) struct ReplayBytesPlan {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    maximum: usize,
    bytes: Vec<u8>,
}
impl ReplayBytesPlan {
    pub(crate) fn length(&self) -> usize {
        self.bytes.len()
    }
}

#[derive(Debug)]
pub(crate) struct ReplayStoreSource {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    open_file: OpenFileId,
    channel: NetworkChannelId,
    before: u64,
    bytes: Vec<u8>,
    consume_epoch: u64,
    cursor: Option<i32>,
}
impl ReplayStoreSource {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
}

fn plain_prefix(channel: &ChannelState, maximum: usize) -> Result<Vec<u8>, NetworkReplayError> {
    if channel.transport.is_datagram()
        || channel.local_read_shutdown
        || maximum == 0
        || maximum > NETWORK_STREAM_CHUNK_LIMIT
    {
        return Err(invalid(
            "Replay store requires a bounded open stream prefix",
        ));
    }
    let mut selected = Vec::with_capacity(maximum);
    for item in &channel.inbound {
        if selected.len() == maximum {
            break;
        }
        let InboundOutcome::Stream {
            bytes,
            ancillary: None,
            message_flags: 0,
            requires_message_io: false,
        } = item
        else {
            // EOF ends a positive prefix; a head EOF uses a distinct typed
            // no-store plan. Other controls/errors retain their exact refusal.
            if matches!(
                item,
                InboundOutcome::PeerShutdown {
                    direction: NetworkShutdownV2::Write,
                    ..
                }
            ) {
                break;
            }
            if selected.is_empty() {
                return Err(invalid("Replay store selected a non-byte outcome"));
            }
            break;
        };
        if bytes.is_empty() {
            return Err(invalid(
                "Replay store cannot invent progress for an empty message",
            ));
        }
        selected.extend(bytes.iter().take(maximum - selected.len()).copied());
    }
    Ok(selected)
}

impl NetworkReplayEngine {
    pub(super) fn check_replay_receive_call(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        maximum: usize,
    ) -> Result<(OpenFileId, NetworkChannelId), NetworkReplayError> {
        self.check_native_retirement()?;
        let state = self.owned_stream_call(owner, call)?;
        if (!self.native_receive_version() || self.mode() != NetworkEngineMode::Replay)
            || state.phase != StreamCallPhase::Active
            || state.physical_pin_required
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || state.helper_copy.is_some()
            || state.private_receive.is_some()
            || state.private_drain.is_some()
            || state.foreground_store.is_some()
            || state.replay_receive.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.no_store_completed
            || state.record_no_store.is_some()
            || state.original.is_some()
            || state.abandoned
        {
            return Err(invalid(
                "Replay source requires its untouched logical V4 Call",
            ));
        }
        let open_file = state.open_file.expect("owned stream Call");
        self.check_socket_control_available(open_file)?;
        self.check_unreserved_stream_delivery(open_file)?;
        let channel = self.bound_channel(open_file)?;
        if self.stream_role(channel)? == NetworkEndpointRoleV2::Listener {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        // General low-water/timeout handling needs a partial-completion
        // transaction for EOF, error, deadline and signal, not just a readiness
        // threshold. Until that is qualified, refuse before selecting or
        // reserving a source (including direct reservation and revalidation).
        let socket = self.stream_call_socket_state(owner, call)?;
        if socket.options.receive_low_water != 1
            || socket.options.receive_timeout
                != detcore_model::network_trace::ReceiveTimeoutV3::Infinite
        {
            return Err(invalid(
                "Replay scalar store requires its qualified low-water/timeout profile",
            ));
        }
        if maximum == 0 || maximum > NETWORK_STREAM_CHUNK_LIMIT {
            return Err(invalid("Replay selection requires bounded scalar capacity"));
        }
        Ok((open_file, channel))
    }

    pub(crate) fn plan_replay_receive(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        maximum: usize,
        nonblocking: bool,
    ) -> Result<ReplayReceivePlan, NetworkReplayError> {
        let (_, channel) = self.check_replay_receive_call(owner, call, maximum)?;
        let bytes = plain_prefix(&self.channels[&channel], maximum)?;
        if bytes.is_empty() {
            self.plan_replay_no_store(owner, call, maximum, nonblocking)
        } else {
            Ok(ReplayReceivePlan::Bytes(ReplayBytesPlan {
                owner,
                call,
                maximum,
                bytes,
            }))
        }
    }

    /// Only a still-identical immutable positive selection reserves Delivery.
    /// The caller validates the selected destination span before this mutation.
    pub(crate) fn reserve_replay_planned_store(
        &mut self,
        plan: &ReplayBytesPlan,
    ) -> Result<Arc<ReplayStoreSource>, NetworkReplayError> {
        let owner = plan.owner;
        let call = plan.call;
        let maximum = plan.maximum;
        let (open_file, channel) = self.check_replay_receive_call(owner, call, maximum)?;
        let socket = self.stream_call_socket_state(owner, call)?;
        let queue = &self.channels[&channel];
        let bytes = plain_prefix(queue, maximum)?;
        if bytes.is_empty() || bytes != plan.bytes {
            return Err(invalid(
                "Replay positive selection changed before reservation",
            ));
        }
        let before = queue.inbound_consumed;
        before
            .checked_add(bytes.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        socket
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let lease = self.allocate_stream_lease()?;
        let source = Arc::new(ReplayStoreSource {
            owner,
            call,
            lease,
            open_file,
            channel,
            before,
            bytes,
            consume_epoch: socket.consume_epoch,
            cursor: socket.options.peek_offset,
        });
        self.stream_operations.insert(
            lease,
            StreamOperation {
                owner,
                open_file,
                channel,
                abandoned: false,
                kind: StreamOperationKind::Delivery {
                    at_offset: before,
                    peek_offset: 0,
                    selection_len: source.bytes.len(),
                    outcome: NetworkStreamChunkOutcome::Bytes(source.bytes.clone()),
                },
            },
        );
        self.stream_delivery.insert(open_file, lease);
        self.stream_calls.get_mut(&call).unwrap().replay_receive = Some(source.clone());
        Ok(source)
    }

    // Compatibility only for the unchanged existing focused engine controls.
    // Product callers use the exhaustive plan and exact prevalidated bytes.
    #[cfg(test)]
    pub(crate) fn reserve_replay_store_source(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        maximum: usize,
    ) -> Result<Option<Arc<ReplayStoreSource>>, NetworkReplayError> {
        match self.plan_replay_receive(owner, call, maximum, false)? {
            ReplayReceivePlan::Bytes(plan) => self.reserve_replay_planned_store(&plan).map(Some),
            ReplayReceivePlan::Wait => Ok(None),
            ReplayReceivePlan::NoStore(_) => Err(invalid(
                "positive fixture selected a typed no-store outcome",
            )),
        }
    }

    pub(super) fn check_replay_store_source(
        &self,
        source: &Arc<ReplayStoreSource>,
    ) -> Result<NetworkStreamCallId, NetworkReplayError> {
        let state = self.owned_stream_call(source.owner, source.call)?;
        let operation = self.owned_stream_operation(source.owner, source.lease)?;
        let socket = self.stream_call_socket_state(source.owner, source.call)?;
        if (!self.native_receive_version() || self.mode() != NetworkEngineMode::Replay)
            || state.phase != StreamCallPhase::Active
            || state.physical_pin_required
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.open_file != Some(source.open_file)
            || state
                .replay_receive
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, source))
            || state.helper_copy.is_some()
            || state.private_receive.is_some()
            || state.private_drain.is_some()
            || operation.open_file != source.open_file
            || operation.channel != source.channel
            || operation.abandoned
            || self.stream_delivery.get(&source.open_file) != Some(&source.lease)
            || socket.consume_epoch != source.consume_epoch
            || socket.options.peek_offset != source.cursor
            || self.channels[&source.channel].inbound_consumed != source.before
            || self.shadow_deliveries.contains_key(&source.lease)
            || !matches!(&operation.kind, StreamOperationKind::Delivery { at_offset, peek_offset: 0,
                selection_len, outcome: NetworkStreamChunkOutcome::Bytes(bytes) }
                if *at_offset == source.before && *selection_len == source.bytes.len() && bytes == &source.bytes)
            || plain_prefix(&self.channels[&source.channel], source.bytes.len())? != source.bytes
        {
            return Err(invalid(
                "Replay store changed its exact retained queue source",
            ));
        }
        Ok(source.call)
    }

    pub(super) fn replay_store_selection(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<Arc<ReplayStoreSource>, NetworkReplayError> {
        let source = self
            .stream_calls
            .values()
            .find_map(|state| {
                state
                    .replay_receive
                    .as_ref()
                    .filter(|source| source.lease == lease)
            })
            .ok_or_else(|| invalid("Replay store lacks its same-Call source"))?;
        if source.owner != owner {
            return Err(NetworkReplayError::StreamLeaseOwnerMismatch(lease));
        }
        self.check_replay_store_source(source)?;
        Ok(source.clone())
    }

    /// The only consuming issuer for a retained Replay store. Every queue and
    /// identity check precedes mutation; a partial/failed/unknown native write
    /// leaves the lease, source and raw store outcome owned and cannot retry.
    pub(crate) fn commit_replay_foreground_store(
        &mut self,
        full: &FullStoreCompletion,
    ) -> Result<usize, NetworkReplayError> {
        let store = full.store();
        let foreground_store::ForegroundStoreSource::Replay(source) = store.source() else {
            return Err(invalid(
                "Replay consumption cannot use a Record helper completion",
            ));
        };
        self.check_foreground_store(store)?;
        if !full.has_ended_full_store()
            || store.source_offset() != 0
            || store.length() != source.bytes.len()
        {
            return Err(invalid(
                "Replay consumption lacks the exact ended whole store",
            ));
        }
        let after = source
            .before
            .checked_add(source.bytes.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let epoch = source
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let cursor = source.cursor.map(|value| {
            if value >= 0 {
                value.wrapping_sub(source.bytes.len() as i32).max(0)
            } else {
                value
            }
        });
        let queue = self.channels.get_mut(&source.channel).unwrap();
        let mut remaining = source.bytes.len();
        while remaining != 0 {
            let InboundOutcome::Stream { bytes, .. } = queue.inbound.front_mut().unwrap() else {
                unreachable!()
            };
            let count = remaining.min(bytes.len());
            bytes.drain(..count);
            remaining -= count;
            if bytes.is_empty() {
                queue.inbound.pop_front();
            }
        }
        queue.inbound_consumed = after;
        queue.refresh_readiness();
        let socket = self
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&source.open_file)
            .unwrap();
        socket.consume_epoch = epoch;
        socket.options.peek_offset = cursor;
        self.stream_delivery.remove(&source.open_file);
        self.stream_operations.remove(&source.lease);
        self.stream_calls
            .get_mut(&source.call)
            .unwrap()
            .replay_receive = None;
        self.complete_deferred_retirement(source.open_file);
        Ok(source.bytes.len())
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Controlled logical installation premise only; source and store still
    /// use the real shared Replay queue/Call/lease methods.
    pub(crate) fn controlled_replay_store_call(
        owner: NetworkStreamOwner,
        trace: detcore_model::network_trace::NetworkTraceV4,
    ) -> (Self, NetworkStreamCallId) {
        let now = trace
            .inputs
            .iter()
            .map(|i| i.release.not_before_global_time)
            .max()
            .unwrap_or(trace.epoch_global_time().unwrap());
        let channel = trace.channels[0].id;
        let profile = trace.fresh_stream_profiles[0].clone();
        let mut engine = Self::replay_native_receive(trace).unwrap();
        let file = OpenFileId::new_socket(owner.thread, 9);
        engine
            .register_stream_socket(
                file,
                profile.key,
                NetworkStreamNamespace {
                    device: 7,
                    inode: 11,
                },
                None,
            )
            .unwrap();
        engine.bind(file, channel).unwrap();
        engine.release_eligible(now).unwrap();
        assert_eq!(
            engine.take_connection_outcome(file).unwrap(),
            Some(ConnectionOutcome::Connect(
                NetworkConnectionResultV2::Connected
            ))
        );
        engine.release_eligible(now).unwrap();
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, control).unwrap();
        assert!(!call.physical_pin_required);
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        (engine, call.id)
    }
}

#[cfg(test)]
#[path = "replay_store/tests.rs"]
mod tests;

#[cfg(test)]
impl NetworkReplayEngine {
    /// Valid model premise only; no Call, reader, control, or store is issued.
    pub(crate) fn controlled_replay_two_row_trace() -> detcore_model::network_trace::NetworkTraceV4
    {
        tests::two_row_trace()
    }
}
