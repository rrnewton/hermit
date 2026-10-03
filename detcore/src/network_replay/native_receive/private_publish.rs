//! Preparation for semantic publication of a reconciled private receive.
//! No preparation here is a trace release, Linux copy unit or guest return.
use std::ops::Range;
use std::sync::Arc;

use super::*;
use crate::network_runtime::native_peer::ConfirmedPrivateDrain;
use crate::network_runtime::native_peer::Observation;

#[derive(Debug, PartialEq, Eq)]
struct Fragment {
    length: usize,
    whole: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PrefixPlan {
    fragments: Vec<Fragment>,
    private_suffix: Range<usize>,
    consumed_after: u64,
    published_after: u64,
    readiness_after: NetworkReadinessV2,
}

/// Compare every touched plain fragment before planning any mutation. The
/// source remains owned by its canonical Capture; this stores only coordinates.
fn plan_prefix(
    channel: &ChannelState,
    source: &[u8],
    before: u64,
) -> Result<PrefixPlan, NetworkReplayError> {
    if source.is_empty()
        || channel.transport.is_datagram()
        || channel.inbound_consumed != before
        || channel.local_read_shutdown
    {
        return Err(invalid(
            "private publication changed its semantic stream origin",
        ));
    }
    let frontier = channel.published_ingress.unwrap_or_default();
    let published = frontier
        .stream_offset
        .checked_sub(before)
        .ok_or_else(|| invalid("private publication regressed its published frontier"))?;
    let consumed_after = before
        .checked_add(u64::try_from(source.len()).map_err(|_| NetworkReplayError::Overflow)?)
        .ok_or(NetworkReplayError::Overflow)?;
    let prefix = usize::try_from(published.min(source.len() as u64))
        .map_err(|_| NetworkReplayError::Overflow)?;
    let mut compared = 0;
    let mut removed = 0;
    let mut fragments = Vec::new();
    for input in &channel.inbound {
        if compared == prefix {
            break;
        }
        let InboundOutcome::Stream {
            bytes,
            ancillary: None,
            message_flags: 0,
            requires_message_io: false,
        } = input
        else {
            return Err(invalid(
                "private publication crosses a control or ancillary boundary",
            ));
        };
        if bytes.is_empty() {
            return Err(invalid(
                "private publication crosses an empty stream message",
            ));
        }
        let count = bytes.len().min(prefix - compared);
        if !bytes
            .iter()
            .take(count)
            .eq(source[compared..compared + count].iter())
        {
            return Err(invalid(
                "private publication disagrees with a published stream fragment",
            ));
        }
        let whole = count == bytes.len();
        fragments.push(Fragment {
            length: count,
            whole,
        });
        removed += usize::from(whole);
        compared += count;
    }
    if compared != prefix {
        return Err(invalid(
            "private publication lacks its complete published prefix",
        ));
    }
    if prefix < source.len()
        && (frontier.terminal || channel.peer_write_closed || removed != channel.inbound.len())
    {
        return Err(invalid(
            "private suffix cannot cross a retained control or terminal boundary",
        ));
    }
    // A private suffix is consumed by this same whole store. It never becomes
    // an extra readable queue item or changes the remaining front fragment.
    Ok(PrefixPlan {
        fragments,
        private_suffix: prefix..source.len(),
        consumed_after,
        published_after: frontier.stream_offset.max(consumed_after),
        readiness_after: channel.computed_readiness_with_front(channel.inbound.get(removed)),
    })
}

/// Constructible only from this engine's exact retained reconciliation. The
/// current journal count is a stale-plan fence, never an ordinal reservation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PrivatePublicationPlan {
    full: FullStoreCompletion,
    observed: Observation,
    open_file: OpenFileId,
    channel: NetworkChannelId,
    physical_after: Cut,
    prior_ingress: Option<PublishedIngress>,
    prior_input_count: u64,
    prior_receive_generation: Option<u64>,
    prior_control_generation: u64,
    next_consume_epoch: u64,
    cursor_after: Option<i32>,
    prefix: PrefixPlan,
}
impl PrivatePublicationPlan {
    pub(crate) fn full(&self) -> &FullStoreCompletion {
        &self.full
    }
    pub(crate) fn observed(&self) -> &Observation {
        &self.observed
    }
}

/// Retained on the same PrivateDrain. Cloning its Arc cannot publish anything;
/// the future versioned publisher must revalidate this exact live preparation.
#[derive(Debug)]
pub(crate) struct PreparedPrivatePublication {
    plan: PrivatePublicationPlan,
}

impl NetworkReplayEngine {
    pub(crate) fn plan_private_receive_publication(
        &self,
        full: &FullStoreCompletion,
    ) -> Result<PrivatePublicationPlan, NetworkReplayError> {
        self.plan_private_receive_publication_inner(full, None)
    }

    fn plan_private_receive_publication_inner(
        &self,
        full: &FullStoreCompletion,
        attached: Option<&Arc<PreparedPrivatePublication>>,
    ) -> Result<PrivatePublicationPlan, NetworkReplayError> {
        let store = full.store();
        let record_source = store.record_completion()?;
        let owner = store.owner();
        let call = store.call();
        let lease = store.lease();
        let state = self.owned_stream_call(owner, call)?;
        let delivery = self.owned_shadow_delivery(owner, lease)?;
        let operation = self.owned_stream_operation(owner, lease)?;
        let source = state
            .private_receive
            .as_ref()
            .ok_or_else(|| invalid("private publication lost its canonical source"))?;
        let drain = state
            .private_drain
            .as_ref()
            .ok_or_else(|| invalid("private publication lacks actual Drain"))?;
        let observed = drain
            .observed
            .as_ref()
            .ok_or_else(|| invalid("private publication lacks actual Drain result"))?;
        let completion = observed
            .helper_copy
            .as_ref()
            .ok_or_else(|| invalid("private publication lost Drain custody"))?;
        let open_file = self.stream_call_open_file(owner, call)?;
        let socket = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&open_file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?;
        let native = socket
            .native
            .as_ref()
            .ok_or_else(|| invalid("private publication lost its original file origin"))?;
        let channel = self
            .channels
            .get(&operation.channel)
            .ok_or_else(|| invalid("private publication lost its channel"))?;
        let input_count = self.native_input_count()?;
        let next_consume_epoch = socket
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let StreamOperationKind::Delivery {
            at_offset,
            peek_offset,
            selection_len,
            outcome,
        } = &operation.kind
        else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        if state.phase != StreamCallPhase::Active
            || !state.physical_pin_required
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || operation.abandoned
            || operation.open_file != open_file
            || self.bound_channel(open_file)? != operation.channel
            || self.stream_delivery.get(&open_file) != Some(&lease)
            || !full.has_ended_full_store()
            || !store.root().is_current(owner)
            || state
                .foreground_store
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, store))
            || source.completion != *record_source
            || source.cut != drain.before
            || source.length < store.length()
            || store.source_offset() != 0
            || !(1..=512).contains(&store.length())
            || drain.full != *full
            || !drain.physically_joined
            || !drain.matched
            || match (attached, &drain.publication) {
                (None, None) => false,
                (Some(expected), Some(actual)) => !Arc::ptr_eq(expected, actual),
                _ => true,
            }
            || drain
                .binding
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
            || !completion.binding().succeeds(record_source)
            || completion.binding().owner() != owner
            || completion.binding().call() != call
            || completion.binding().lease() != lease
            || completion.binding().effect()
                != &(NetworkStreamPhysicalEffect::Drain {
                    maximum: store.length(),
                })
            || observed.errno.is_some()
            || observed.raw_return != store.length() as i64
            || observed.bytes != source.completion.capture().committed[..store.length()]
            || completion.capture().committed != observed.bytes
            || observed.confirmation
                != (NetworkStreamPhysicalResult::Drained {
                    bytes: observed.bytes.clone(),
                })
            || delivery.call != call
            || delivery.private_offset != Some(0)
            || delivery.selected_len != store.length()
            || !delivery.drain_started
            || delivery.drained != store.length()
            || delivery.pending.is_some()
            || delivery.peek_cursor_confirmed
            || delivery.next_consume_epoch != next_consume_epoch
            || *at_offset != source.cut.bytes
            || *peek_offset != 0
            || *selection_len != store.length()
            || !matches!(outcome, NetworkStreamChunkOutcome::Bytes(bytes)
                if bytes == &source.completion.capture().committed[..store.length()])
        {
            return Err(invalid(
                "private publication changed its exact source/store/Drain selection",
            ));
        }
        completion
            .joined_worker()
            .map_err(|e| invalid(&e.to_string()))?;
        let after_bytes = source
            .cut
            .bytes
            .checked_add(store.length() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let last = completion
            .attempts()
            .last()
            .ok_or_else(|| invalid("private publication lost the actual Consume chain"))?;
        let physical_after = Cut {
            bytes: after_bytes,
            order: last.unit().native.order,
        };
        if native.physical_observed != physical_after
            || physical_after.order <= source.cut.order
            || native.binding.open_file != open_file
            || completion.attempts().iter().any(|r| {
                !native
                    .identity
                    .matches(r.selection().provider, r.selection().file)
            })
            || self
                .stream_calls
                .values()
                .filter(|s| s.open_file == Some(open_file))
                .any(|s| s.native_receive.iter().any(|a| !a.joined))
            || completion.attempts().iter().any(|receipt| {
                !state
                    .native_receive
                    .iter()
                    .any(|retained| retained.joined && retained.receipt.same(receipt))
            })
        {
            return Err(invalid(
                "private publication changed its actual physical-after cut",
            ));
        }
        let prefix = plan_prefix(
            channel,
            &source.completion.capture().committed[..store.length()],
            source.cut.bytes,
        )?;
        let prior_input_count =
            u64::try_from(input_count).map_err(|_| NetworkReplayError::Overflow)?;
        Ok(PrivatePublicationPlan {
            full: full.clone(),
            observed: observed.clone(),
            open_file,
            channel: operation.channel,
            physical_after,
            prior_ingress: channel.published_ingress,
            prior_input_count,
            prior_receive_generation: channel.receive_input_generation,
            prior_control_generation: channel.local_control_generation,
            next_consume_epoch,
            cursor_after: delivery.consumed_cursor(),
            prefix,
        })
    }

    /// The borrowed join is minted only while the actual Calls mutex is held.
    /// All checks precede the sole mutation; no runtime lease is retired yet.
    pub(crate) fn attach_private_receive_publication(
        &mut self,
        plan: PrivatePublicationPlan,
        confirmed: &ConfirmedPrivateDrain<'_>,
    ) -> Result<Arc<PreparedPrivatePublication>, NetworkReplayError> {
        if !confirmed.matches(&plan.full, &plan.observed)
            || self.plan_private_receive_publication(&plan.full)? != plan
        {
            return Err(invalid(
                "private publication lost its confirmed Pending or live plan",
            ));
        }
        let call = plan.full.store().call();
        let prepared = Arc::new(PreparedPrivatePublication { plan });
        self.stream_calls
            .get_mut(&call)
            .unwrap()
            .private_drain
            .as_mut()
            .unwrap()
            .publication = Some(prepared.clone());
        Ok(prepared)
    }
}

#[cfg(test)]
#[path = "private_publish/tests.rs"]
mod tests;

impl PreparedPrivatePublication {
    pub(crate) fn full(&self) -> &FullStoreCompletion {
        &self.plan.full
    }
    pub(crate) fn observed(&self) -> &Observation {
        &self.plan.observed
    }
}

impl NetworkReplayEngine {
    /// Commit only under the actual confirmed Pending borrow. The runtime
    /// removes that exact lease after this infallible semantic commit while its
    /// Calls guard is still held. Actual pin close remains a separate operation.
    pub(crate) fn publish_private_native_receive(
        &mut self,
        prepared: &Arc<PreparedPrivatePublication>,
        confirmed: &ConfirmedPrivateDrain<'_>,
        now: LogicalTime,
    ) -> Result<usize, NetworkReplayError> {
        use detcore_model::network_trace::NetworkInputEventV4;
        use detcore_model::network_trace::NetworkNativeCopyDispositionV4;
        use detcore_model::network_trace::NetworkNativeCopyFragmentV4;
        use detcore_model::network_trace::NetworkNativeReceiveObservationV4;
        use detcore_model::network_trace::NetworkReleaseModelV4;
        use detcore_model::network_trace::NetworkReleaseNodeIdV4;
        use detcore_model::network_trace::NetworkReleaseNodeKindV4;
        use detcore_model::network_trace::NetworkReleaseNodeV4;
        let plan = &prepared.plan;
        if !confirmed.matches(&plan.full, &plan.observed)
            || self.plan_private_receive_publication_inner(&plan.full, Some(prepared))? != *plan
        {
            return Err(invalid(
                "V4 publication lost its exact prepared source/store/Drain/Pending",
            ));
        }
        let store = plan.full.store();
        let release = self.native_entry_release(
            store.owner(),
            store.call(),
            store.root(),
            store.epoch(),
            now,
        )?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        let trace = native.trace();
        let suffix = &plan.prefix.private_suffix;
        // The current V4 producer publishes at actual whole-store completion.
        // It has no separate availability-only producer. E's general prefix
        // planner stays intact; this publisher cannot relabel an already
        // delivered Input node as merely available payload.
        if suffix.start != 0
            || suffix.end != store.length()
            || trace
                .inputs
                .iter()
                .rev()
                .find(|i| i.channel == plan.channel)
                .is_none_or(|last| last.release.not_before_global_time > now)
            || !trace.release_model.nodes().iter().any(|node| {
                matches!(&node.kind,
                NetworkReleaseNodeKindV4::Progress { channel, milestone:
                    detcore_model::network_trace::NetworkProgressV4::Established { .. } }
                if *channel == plan.channel)
            })
        {
            return Err(invalid(
                "V4 positive receive lacks its established unpublished whole prefix",
            ));
        }
        let source = self.stream_calls[&store.call()]
            .private_receive
            .as_ref()
            .unwrap();
        let offset = source.cut.bytes;
        if trace
            .native_receive_observations
            .iter()
            .rev()
            .find(|o| o.channel == plan.channel)
            .map_or(0, |o| o.stream_offset + o.length)
            != offset
        {
            return Err(invalid(
                "V4 source publication changed its immutable payload frontier",
            ));
        }
        let ordinal =
            u64::try_from(trace.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let node_id = u64::try_from(trace.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        node_id.checked_add(1).ok_or(NetworkReplayError::Overflow)?;
        let observation_ordinal = u64::try_from(trace.native_receive_observations.len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        // Describe the actual Consume chain, once. The larger private Peek is
        // retained on Call as source evidence; it cannot hide Drain progress
        // behind an Observe row or issue consumed bytes numerically.
        let drain_completion = plan
            .observed
            .helper_copy
            .as_ref()
            .expect("validated exact confirmed Drain");
        let fragments = drain_completion
            .attempts()
            .iter()
            .map(|attempt| {
                let unit = attempt.unit();
                let observation = unit.observation.expect("canonical source requires copy5");
                let begin = observation.begin;
                NetworkNativeCopyFragmentV4 {
                    stream_offset: begin.start,
                    requested: unit.native.requested,
                    copied: unit.native.copied,
                    available: begin.available,
                    source_offset: begin.source_offset,
                    storage_length: begin.skb_length,
                    nonlinear_length: begin.nonlinear,
                    disposition: NetworkNativeCopyDispositionV4::Consume,
                    physical_before: begin.before,
                    physical_after: observation.after,
                }
            })
            .collect();
        let input = NetworkInputEventV4 {
            ordinal,
            channel: plan.channel,
            release: release.clone(),
            event: NetworkInputKindV2::StreamBytes {
                stream_offset: offset,
                bytes: source.completion.capture().committed[..store.length()].to_vec(),
            },
        };
        let producer = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(node_id),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
        let observation = NetworkNativeReceiveObservationV4 {
            ordinal: observation_ordinal,
            channel: plan.channel,
            stream_offset: offset,
            length: store.length() as u64,
            fragments,
        };
        // No error or await follows the first append. Source bytes were already
        // stored once and actual Drain matched. No guest memory is touched here.
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        let trace = native.record().expect("validated recorder");
        trace.inputs.push(input);
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes.push(producer);
        trace.native_receive_observations.push(observation);
        let channel = self.channels.get_mut(&plan.channel).unwrap();
        // The selected prefix is entirely private, hence there is no queue item
        // to enqueue and consume. Other shared queue state remains unchanged.
        channel.inbound_consumed = plan.prefix.consumed_after;
        channel.published_ingress = Some(PublishedIngress {
            stream_offset: plan.prefix.published_after,
            last_release: None,
            terminal: false,
        });
        channel.receive_input_generation = Some(ordinal);
        channel.readiness = plan.prefix.readiness_after;
        self.commit_shadow_delivery_finish(
            store.lease(),
            plan.open_file,
            NetworkStreamChunkDisposition::Consumed,
            true,
        );
        self.stream_delivery.remove(&plan.open_file);
        self.stream_operations.remove(&store.lease());
        self.consume_native_entry(store.call());
        self.stream_calls
            .get_mut(&store.call())
            .unwrap()
            .helper_copy = None;
        // Store, Peek, Drain and the prepared plan remain retained on Call until
        // the actual pin-close transaction retires that same Call.
        Ok(store.length())
    }
}
