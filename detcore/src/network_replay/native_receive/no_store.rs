//! Joined no-unit native receives are not copy geometry or a successful Store.
//! The exact Call retains their canonical source until physical pin retirement.
use std::sync::Arc;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::HelperCopyCompletion;
use crate::network_runtime::native_peer::ConfirmedNoStore;
use crate::network_runtime::native_peer::Observation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoStoreReturn {
    Eof,
    WouldBlock,
}

#[derive(Debug)]
pub(crate) struct RecordNoStore {
    observed: Observation,
    outcome: NoStoreReturn,
    retry: std::sync::Mutex<RecordRetryState>,
    retry_origin: std::sync::OnceLock<crate::network_runtime::ReceiveRetryOrigin>,
}
#[derive(Debug, Default)]
struct RecordRetryState {
    attempted: bool,
    completed: bool,
    failure: Option<String>,
}
impl RecordNoStore {
    pub(crate) fn completion(&self) -> &HelperCopyCompletion {
        self.observed
            .helper_copy
            .as_ref()
            .expect("checked canonical no-store source")
    }
    pub(crate) fn observed(&self) -> &Observation {
        &self.observed
    }
    fn checked(observed: &Observation) -> Result<Self, NetworkReplayError> {
        let completion = observed
            .helper_copy
            .as_ref()
            .ok_or_else(|| invalid("no-store lost its canonical completion"))?;
        completion
            .joined_worker()
            .map_err(|e| invalid(&e.to_string()))?;
        let NetworkStreamPhysicalEffect::Peek { maximum } = completion.binding().effect() else {
            return Err(invalid("no-store requires exact Peek"));
        };
        let capture = completion.capture();
        let m = &capture.manifest;
        if m.present != 1
            || m.summary.version != 5
            || m.summary.protocol_complete != 1
            || m.summary.protocol_returned as i64 != m.returned
            || m.summary.initial_count != *maximum as u64
            || m.summary.final_count != m.summary.initial_count
            || m.summary.records != 0
            || m.summary.attempts != 0
            || m.summary.copied != 0
            || !capture.records.is_empty()
            || !capture.units.is_empty()
            || !capture.committed.is_empty()
            || !completion.attempts().is_empty()
            || !observed.bytes.is_empty()
        {
            return Err(invalid(
                "no-store lacks exact complete zero-unit copy5 manifest",
            ));
        }
        let outcome = match (
            m.returned,
            observed.raw_return,
            observed.errno,
            &observed.confirmation,
        ) {
            (0, 0, None, NetworkStreamPhysicalResult::Peeked { count: 0 }) => NoStoreReturn::Eof,
            (v, -1, Some(libc::EAGAIN), NetworkStreamPhysicalResult::Errno(libc::EAGAIN))
                if v == -i64::from(libc::EAGAIN) =>
            {
                NoStoreReturn::WouldBlock
            }
            _ => return Err(invalid("no-store changed its actual native result")),
        };
        Ok(Self {
            observed: observed.clone(),
            outcome,
            retry: std::sync::Mutex::new(RecordRetryState::default()),
            retry_origin: std::sync::OnceLock::new(),
        })
    }
}

/// Local one-use result of the joint engine/runtime transaction. A source or a
/// numeric zero cannot manufacture this result. It grants no memory access.
#[derive(Debug)]
pub(crate) struct CompletedNoStore {
    outcome: NoStoreReturn,
    record_empty: Option<CompletedRecordEmptyAttempt>,
    replay_timed_out: bool,
}
impl CompletedNoStore {
    #[cfg(test)]
    pub(crate) fn into_outcome(self) -> NoStoreReturn {
        self.outcome
    }
    /// Classify without consuming: a blocking EAGAIN keeps this exact token
    /// for the one-use same-Call retry after the caller's real wait.
    pub(crate) fn outcome(&self) -> NoStoreReturn {
        self.outcome
    }
    pub(crate) fn replay_timed_out(&self) -> bool {
        self.replay_timed_out
    }
    pub(crate) fn into_record_empty(
        self,
    ) -> Result<CompletedRecordEmptyAttempt, NetworkReplayError> {
        self.record_empty
            .ok_or_else(|| invalid("retry requires a committed canonical Record EAGAIN"))
    }
}

/// Only the successful joint canonical EAGAIN commit creates this local token.
/// It owns no fd reader and cannot reopen a Call after physical retirement.
#[derive(Debug)]
pub(crate) struct CompletedRecordEmptyAttempt {
    source: Arc<RecordNoStore>,
    root: Arc<ForegroundRoot>,
    epoch: u64,
    observed_at: LogicalTime,
}
impl CompletedRecordEmptyAttempt {
    pub(crate) fn begin(self) -> Result<RecordReceiveRetry, NetworkReplayError> {
        {
            let mut state = self.source.retry.lock().unwrap();
            if let Some(first) = &state.failure {
                return Err(invalid(first));
            }
            if state.attempted {
                return Err(invalid(state.failure.get_or_insert_with(|| {
                    "completed EAGAIN retry entitlement is one use".into()
                })));
            }
            state.attempted = true;
        }
        Ok(RecordReceiveRetry { completed: self })
    }
}

/// Dropping an in-progress issuer retains its first failure on the old source.
/// No cancelled future can acquire a newer prefix or clear unknown native work.
#[derive(Debug)]
pub(crate) struct RecordReceiveRetry {
    completed: CompletedRecordEmptyAttempt,
}
impl RecordReceiveRetry {
    pub(crate) fn source(&self) -> &Arc<RecordNoStore> {
        &self.completed.source
    }
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        &self.completed.root
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.completed.epoch
    }
    pub(crate) fn observed_at(&self) -> LogicalTime {
        self.completed.observed_at
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.source().completion().binding().owner()
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.source().completion().binding().call()
    }
    pub(crate) fn check(&self) -> Result<(), NetworkReplayError> {
        let state = self.source().retry.lock().unwrap();
        if let Some(first) = &state.failure {
            return Err(invalid(first));
        }
        if !state.attempted || state.completed {
            return Err(invalid("completed EAGAIN retry attempt is spent"));
        }
        Ok(())
    }
    pub(crate) fn fail(&self, error: impl std::fmt::Display) -> NetworkReplayError {
        let mut state = self.source().retry.lock().unwrap();
        invalid(state.failure.get_or_insert_with(|| error.to_string()))
    }
    pub(crate) fn retain_origin(
        &self,
        origin: crate::network_runtime::ReceiveRetryOrigin,
    ) -> Result<(), NetworkReplayError> {
        self.check()?;
        let first = self.source().retry_origin.get_or_init(|| origin.clone());
        if !first.same(&origin) {
            return Err(
                self.fail("receive retry cannot replace its first native submission prefix")
            );
        }
        Ok(())
    }
    pub(crate) fn matches_origin(
        &self,
        origin: &crate::network_runtime::ReceiveRetryOrigin,
    ) -> bool {
        self.source()
            .retry_origin
            .get()
            .is_some_and(|first| first.same(origin))
    }
    pub(in crate::network_replay) fn mark_completed(&self) {
        let mut state = self.source().retry.lock().unwrap();
        assert!(state.attempted && !state.completed && state.failure.is_none());
        state.completed = true;
    }
}
impl Drop for RecordReceiveRetry {
    fn drop(&mut self) {
        let mut state = self.source().retry.lock().unwrap();
        if !state.completed {
            state
                .failure
                .get_or_insert_with(|| "receive retry issuer ended before joint admission".into());
        }
    }
}

#[cfg(test)]
impl CompletedNoStore {
    /// Deliberate stale-token adversary; production completion is not Clone.
    pub(crate) fn duplicate_retry_fixture(&self) -> Self {
        Self {
            outcome: self.outcome,
            replay_timed_out: self.replay_timed_out,
            record_empty: self
                .record_empty
                .as_ref()
                .map(|empty| CompletedRecordEmptyAttempt {
                    source: empty.source.clone(),
                    root: empty.root.clone(),
                    epoch: empty.epoch,
                    observed_at: empty.observed_at,
                }),
        }
    }
}

impl NetworkReplayEngine {
    pub(super) fn confirm_record_no_store(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed: &Observation,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        if !matches!(&self.mode, EngineState::Native(n) if n.mode() == NetworkEngineMode::Record) {
            return Err(NetworkReplayError::WrongMode);
        }
        let probe = self.owned_shadow_probe(owner, lease)?;
        let state = self.owned_stream_call(owner, probe.call)?;
        let completion = observed
            .helper_copy
            .as_ref()
            .ok_or_else(|| invalid("no-store lost helper proof"))?;
        let binding = completion.binding();
        if !matches!(
            probe.pending,
            Some(NetworkStreamPhysicalEffect::Peek { .. })
        ) || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || probe.peek.is_some()
            || binding.owner() != owner
            || binding.call() != probe.call
            || binding.lease() != lease
            || Some(binding.effect()) != probe.pending.as_ref()
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|held| !Arc::ptr_eq(held, binding))
        {
            return Err(invalid("no-store changed its exact pending Call/effect"));
        }
        let source = Arc::new(RecordNoStore::checked(observed)?);
        let peek = match source.outcome {
            NoStoreReturn::Eof => Ok(0),
            NoStoreReturn::WouldBlock => Err(libc::EAGAIN),
        };
        let call = probe.call;
        self.stream_calls.get_mut(&call).unwrap().record_no_store = Some(source);
        let probe = self.shadow_probes.get_mut(&lease).unwrap();
        probe.peek = Some(peek);
        probe.pending = None;
        Ok(())
    }

    pub(crate) fn record_no_store_source(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
    ) -> Result<Arc<RecordNoStore>, NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, lease)?;
        let state = self.owned_stream_call(owner, call)?;
        if probe.pending.is_some() || !probe.cursor_restored() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        if probe.call != call || state.no_store_completed {
            return Err(invalid("no-store source is stale or already completed"));
        }
        state
            .record_no_store
            .clone()
            .ok_or_else(|| invalid("no-store source absent"))
    }

    pub(crate) fn commit_record_no_store(
        &mut self,
        source: &Arc<RecordNoStore>,
        confirmed: &ConfirmedNoStore<'_>,
        root: &Arc<ForegroundRoot>,
        epoch: u64,
        now: LogicalTime,
    ) -> Result<CompletedNoStore, NetworkReplayError> {
        use detcore_model::network_trace::FreshSendTimeoutV1;
        use detcore_model::network_trace::NetworkInputEventV4;
        use detcore_model::network_trace::NetworkProgressV4;
        use detcore_model::network_trace::NetworkReleaseModelV4;
        use detcore_model::network_trace::NetworkReleaseNodeIdV4;
        use detcore_model::network_trace::NetworkReleaseNodeKindV4;
        use detcore_model::network_trace::NetworkReleaseNodeV4;
        let binding = source.completion().binding();
        let owner = binding.owner();
        let call = binding.call();
        let lease = binding.lease();
        let state = self.owned_stream_call(owner, call)?;
        let probe = self.owned_shadow_probe(owner, lease)?;
        let open_file = self.stream_call_open_file(owner, call)?;
        let shadow = self.shadow.as_ref().ok_or(NetworkReplayError::WrongMode)?;
        let socket = shadow
            .sockets
            .get(&open_file)
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?;
        let origin = socket
            .native
            .as_ref()
            .ok_or_else(|| invalid("no-store lacks original installed origin"))?;
        let channel = self
            .channels
            .get(&probe.channel)
            .ok_or(NetworkReplayError::UnknownChannel(probe.channel))?;
        let frontier = channel.published_ingress.unwrap_or_default();
        if state.phase != StreamCallPhase::Active
            || !state.physical_pin_required
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.original.is_some()
            || state.no_store_completed
            || state
                .record_no_store
                .as_ref()
                .is_none_or(|s| !Arc::ptr_eq(s, source))
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, binding))
            || state.private_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.replay_receive.is_some()
            || self.stream_calls.len() != 1
            || probe.call != call
            || probe.pending.is_some()
            || !probe.cursor_restored()
            || probe.retained_prefix != 0
            || probe.captured_through != frontier.stream_offset
            || self.bound_channel(open_file)? != probe.channel
            || self.shadow_probes.len() != 1
            || self.socket_controls.len() != 1
            || !self.stream_operations.is_empty()
            || self.stream_delivery.contains_key(&open_file)
            || !self.shadow_deliveries.is_empty()
            || state.native_receive.iter().any(|a| !a.joined)
            || channel.transport.is_datagram()
            || self.stream_role(probe.channel)? == NetworkEndpointRoleV2::Listener
            || channel.local_read_shutdown
            || !channel.inbound.is_empty()
            || origin.physical_observed.bytes != channel.inbound_consumed
            || frontier.stream_offset != channel.inbound_consumed
            || (source.outcome == NoStoreReturn::WouldBlock
                && !self.receive_profile_qualified(owner, call)?)
            || !confirmed.matches(source, origin.identity, probe.original_cursor)
            || probe.peek
                != Some(match source.outcome {
                    NoStoreReturn::Eof => Ok(0),
                    NoStoreReturn::WouldBlock => Err(libc::EAGAIN),
                })
        {
            return Err(invalid(
                "no-store changed actual source/Call/Pending/cursor/physical frontier",
            ));
        }
        let release = self.native_entry_release(owner, call, root, epoch, now)?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let trace = native.trace();
        let payload_end = trace
            .native_receive_observations
            .iter()
            .rev()
            .find(|o| o.channel == probe.channel)
            .map(|o| {
                o.stream_offset
                    .checked_add(o.length)
                    .ok_or(NetworkReplayError::Overflow)
            })
            .transpose()?
            .unwrap_or(0);
        let terminal_rows = trace
            .inputs
            .iter()
            .filter(|i| {
                i.channel == probe.channel
                    && matches!(i.event, NetworkInputKindV2::PeerShutdown { .. })
            })
            .collect::<Vec<_>>();
        if native.mode() != NetworkEngineMode::Record || payload_end != origin.physical_observed.bytes
            || trace.inputs.iter().rev().find(|i| i.channel == probe.channel)
                .is_none_or(|i| i.release.not_before_global_time > now)
            || !trace.release_model.nodes().iter().any(|n| matches!(n.kind,
                NetworkReleaseNodeKindV4::Progress { channel, milestone: NetworkProgressV4::Established { .. } }
                if channel == probe.channel))
            || frontier.terminal != channel.peer_write_closed
            || if frontier.terminal { terminal_rows.len() != 1 || !matches!(terminal_rows[0].event,
                NetworkInputKindV2::PeerShutdown { stream_offset, direction: NetworkShutdownV2::Write }
                if stream_offset == frontier.stream_offset) } else { !terminal_rows.is_empty() }
            || (source.outcome == NoStoreReturn::WouldBlock && frontier.terminal) {
            return Err(invalid("no-store changed immutable payload/terminal/release frontier"));
        }
        // Validate the complete candidate before changing either owner's state.
        // Zero units contribute no physical Cut/order or native observation.
        let mut candidate = trace.clone();
        candidate.fresh_stream_profiles = shadow.profiles.values().cloned().collect();
        candidate.channel_socket_classes = shadow
            .channel_classes
            .iter()
            .map(|(channel, key)| ChannelSocketClassV3 {
                channel: *channel,
                key: *key,
            })
            .collect();
        candidate.receive_environment = shadow.environment;
        candidate.fresh_send_timeouts = shadow
            .profiles
            .keys()
            .filter_map(|key| {
                native
                    .fresh_send(*key)
                    .map(|timeout| FreshSendTimeoutV1 { key: *key, timeout })
            })
            .collect();
        let append = source.outcome == NoStoreReturn::Eof && !frontier.terminal;
        let ordinal =
            u64::try_from(candidate.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let input = NetworkInputEventV4 {
            ordinal,
            channel: probe.channel,
            release: release.clone(),
            event: NetworkInputKindV2::PeerShutdown {
                stream_offset: frontier.stream_offset,
                direction: NetworkShutdownV2::Write,
            },
        };
        let id = u64::try_from(candidate.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        id.checked_add(1).ok_or(NetworkReplayError::Overflow)?;
        let producer = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(id),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
        if append {
            candidate.inputs.push(input.clone());
            let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
                &mut candidate.release_model else { panic!("legacy fixture changed its release policy"); };
            nodes.push(producer.clone());
        }
        candidate.validate().map_err(|e| invalid(&e.to_string()))?;
        let channel_id = probe.channel;
        // No error or await after the first mutation. Calls/native admission
        // are held by ConfirmedNoStore's private issuer through lease removal.
        if append {
            let EngineState::Native(native) = &mut self.mode else {
                unreachable!()
            };
            let trace = native
                .record()
                .expect("validated native recorder and first-error fence");
            trace.inputs.push(input);
            let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
                &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
            nodes.push(producer);
            let channel = self.channels.get_mut(&channel_id).unwrap();
            channel.published_ingress = Some(PublishedIngress {
                terminal: true,
                ..frontier
            });
            channel.peer_write_closed = true;
            channel.receive_input_generation = Some(ordinal);
            channel.refresh_readiness();
        }
        self.consume_native_entry(call);
        self.shadow_probes.remove(&lease);
        self.socket_controls.remove(&open_file);
        let state = self.stream_calls.get_mut(&call).unwrap();
        state.helper_copy = None;
        state.no_store_completed = true;
        Ok(CompletedNoStore {
            outcome: source.outcome,
            replay_timed_out: false,
            record_empty: (source.outcome == NoStoreReturn::WouldBlock).then(|| {
                CompletedRecordEmptyAttempt {
                    source: source.clone(),
                    root: root.clone(),
                    epoch,
                    observed_at: now,
                }
            }),
        })
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn no_store_fixture_frontier(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> (u64, u64, u64, u64, bool, bool, usize, usize) {
        let ofd = self.stream_call_open_file(owner, call).unwrap();
        let socket = &self.shadow.as_ref().unwrap().sockets[&ofd];
        let native = socket.native.as_ref().unwrap();
        let channel = &self.channels[&self.bound_channel(ofd).unwrap()];
        let frontier = channel.published_ingress.unwrap_or_default();
        (
            native.physical_observed.bytes,
            native.physical_observed.order,
            channel.inbound_consumed,
            frontier.stream_offset,
            frontier.terminal,
            channel.peer_write_closed,
            self.shadow_probes.len(),
            self.socket_controls.len(),
        )
    }
    pub(crate) fn change_no_store_fixture(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        variant: usize,
    ) {
        let ofd = self.stream_call_open_file(owner, call).unwrap();
        let channel = self.bound_channel(ofd).unwrap();
        match variant {
            0 => {
                assert!(
                    self.stream_calls
                        .get_mut(&call)
                        .unwrap()
                        .native_entry
                        .take()
                        .is_some()
                );
            }
            1 => {
                let p = self.shadow_probes.get_mut(&lease).unwrap();
                assert!(p.cursor_restored());
                p.current_cursor = Some(7);
            }
            2 => {
                self.shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&ofd)
                    .unwrap()
                    .native
                    .as_mut()
                    .unwrap()
                    .physical_observed
                    .bytes += 1;
            }
            3 => {
                self.channels.get_mut(&channel).unwrap().inbound_consumed += 1;
            }
            4 => {
                self.channels.get_mut(&channel).unwrap().published_ingress =
                    Some(PublishedIngress {
                        stream_offset: 1,
                        ..Default::default()
                    });
            }
            5 => {
                self.shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&ofd)
                    .unwrap()
                    .options
                    .receive_low_water = 2;
            }
            6 => {
                self.channels.get_mut(&channel).unwrap().local_read_shutdown = true;
            }
            7 => {
                self.controlled_native_retirement_ledger_defect();
            }
            _ => panic!("unknown explicit no-store fixture defect"),
        }
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Controlled local option projection after the fixture performed and read
    /// back native setsockopt on the same actual held original OFD.
    pub(crate) fn no_store_fixture_set_cursor(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        value: i32,
    ) {
        let ofd = self.stream_call_open_file(owner, call).unwrap();
        assert!(self.shadow_probes.is_empty());
        assert!(self.stream_calls[&call].no_store_completed);
        self.shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&ofd)
            .unwrap()
            .options
            .peek_offset = Some(value);
    }
}

/// Exhaustive local receive result. NoStore is issued only after its actual
/// joint commit; Wait carries no Store, exclusion, source plan, or memory grant.
#[derive(Debug)]
pub(crate) enum ReceiveSelection<P> {
    Bytes(P),
    NoStore(CompletedNoStore),
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayNoStoreKind {
    Eof { ordinal: u64, repeated: bool },
    WouldBlock,
    TimedOut,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReplayNoStorePlan {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    maximum: usize,
    nonblocking: bool,
    open_file: OpenFileId,
    channel: NetworkChannelId,
    before: u64,
    generation: Option<u64>,
    consume_epoch: u64,
    cursor: Option<i32>,
    kind: ReplayNoStoreKind,
    policy: Option<Arc<crate::tool_global::SavedReceivePolicy>>,
}

impl NetworkReplayEngine {
    pub(super) fn plan_replay_no_store(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        maximum: usize,
        nonblocking: bool,
        now: Option<LogicalTime>,
    ) -> Result<super::replay_store::ReplayReceivePlan, NetworkReplayError> {
        use super::replay_store::ReplayReceivePlan;
        let (open_file, channel) = self.check_replay_receive_call(owner, call, maximum)?;
        let queue = &self.channels[&channel];
        let socket = self.stream_call_socket_state(owner, call)?;
        let policy = self.saved_receive_policy(owner, call)?;
        if policy
            .as_ref()
            .is_some_and(|saved| saved.nonblocking() != nonblocking)
        {
            return Err(invalid(
                "Replay empty selection changed original nonblocking flag",
            ));
        }
        if queue.local_read_shutdown || queue.transport.is_datagram() {
            return Err(invalid(
                "Replay no-store requires an open stream receive side",
            ));
        }
        let kind = match queue.inbound.front() {
            Some(InboundOutcome::PeerShutdown {
                stream_offset,
                direction: NetworkShutdownV2::Write,
            }) => {
                if queue.peer_write_closed || *stream_offset != queue.inbound_consumed {
                    return Err(invalid("Replay EOF changed its exact byte frontier"));
                }
                let Some((ordinal, true, false)) =
                    self.native_replay_eof(channel, *stream_offset)?
                else {
                    return Err(invalid("Replay EOF lacks its released unconsumed input"));
                };
                ReplayNoStoreKind::Eof {
                    ordinal,
                    repeated: false,
                }
            }
            None if queue.peer_write_closed => {
                let Some((ordinal, true, true)) =
                    self.native_replay_eof(channel, queue.inbound_consumed)?
                else {
                    return Err(invalid(
                        "Replay repeated EOF lacks its consumed terminal input",
                    ));
                };
                ReplayNoStoreKind::Eof {
                    ordinal,
                    repeated: true,
                }
            }
            None if nonblocking => ReplayNoStoreKind::WouldBlock,
            None if policy.as_ref().is_some_and(|saved| {
                now.is_some_and(|now| now >= saved.started() && saved.expired(now))
            }) =>
            {
                ReplayNoStoreKind::TimedOut
            }
            None => return Ok(ReplayReceivePlan::Wait),
            _ => {
                return Err(invalid(
                    "Replay no-store cannot consume a byte/error/other control",
                ));
            }
        };
        Ok(ReplayReceivePlan::NoStore(ReplayNoStorePlan {
            owner,
            call,
            maximum,
            nonblocking,
            open_file,
            channel,
            before: queue.inbound_consumed,
            generation: queue.receive_input_generation,
            consume_epoch: socket.consume_epoch,
            cursor: socket.options.peek_offset,
            kind,
            policy,
        }))
    }

    /// Called only inside the local GlobalState's same-engine, same-root
    /// native-prefix transaction. The private plan never escapes that operation
    /// across a Guest continuation and carries no copy authority.
    pub(crate) fn commit_replay_no_store(
        &mut self,
        plan: ReplayNoStorePlan,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        root: &Arc<ForegroundRoot>,
        now: LogicalTime,
    ) -> Result<CompletedNoStore, NetworkReplayError> {
        if !Arc::ptr_eq(admission.root(), root) || !root.is_current(plan.owner) {
            return Err(invalid(
                "Replay no-store lost exact held native admission/root",
            ));
        }
        let super::replay_store::ReplayReceivePlan::NoStore(current) = self.plan_replay_no_store(
            plan.owner,
            plan.call,
            plan.maximum,
            plan.nonblocking,
            Some(now),
        )?
        else {
            return Err(invalid("Replay no-store selection changed before commit"));
        };
        if current != plan {
            return Err(invalid(
                "Replay no-store changed its exact Call/queue/input/cursor frontier",
            ));
        }
        let first_eof = matches!(
            plan.kind,
            ReplayNoStoreKind::Eof {
                repeated: false,
                ..
            }
        );
        let epoch = if first_eof {
            plan.consume_epoch
                .checked_add(1)
                .ok_or(NetworkReplayError::Overflow)?
        } else {
            plan.consume_epoch
        };
        let outcome = match plan.kind {
            ReplayNoStoreKind::Eof { .. } => NoStoreReturn::Eof,
            ReplayNoStoreKind::WouldBlock | ReplayNoStoreKind::TimedOut => {
                NoStoreReturn::WouldBlock
            }
        };
        // All fallible validation precedes the first semantic mutation. Empty
        // attempts/repeated EOF do not manufacture queue or epoch progress.
        if let ReplayNoStoreKind::Eof {
            ordinal,
            repeated: false,
        } = plan.kind
        {
            self.mark_native_replay_eof_consumed(ordinal);
            let queue = self.channels.get_mut(&plan.channel).unwrap();
            queue.inbound.pop_front();
            queue.peer_write_closed = true;
            queue.refresh_readiness();
            self.shadow
                .as_mut()
                .unwrap()
                .sockets
                .get_mut(&plan.open_file)
                .unwrap()
                .consume_epoch = epoch;
        }
        self.stream_calls
            .get_mut(&plan.call)
            .unwrap()
            .no_store_completed = true;
        Ok(CompletedNoStore {
            outcome,
            record_empty: None,
            replay_timed_out: matches!(plan.kind, ReplayNoStoreKind::TimedOut),
        })
    }

    pub(crate) fn record_no_store_available(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        probe: NetworkStreamLeaseId,
    ) -> Result<bool, NetworkReplayError> {
        let state = self.owned_stream_call(owner, call)?;
        if state.record_no_store.is_none() {
            return Ok(false);
        }
        self.record_no_store_source(owner, call, probe)?;
        Ok(true)
    }
}
