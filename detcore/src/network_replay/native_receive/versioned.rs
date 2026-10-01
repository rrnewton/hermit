//! V4 journal and release predicates inside the existing shared engine. Payload
//! queues, FD lifetime, output comparison and readiness keep their existing
//! owner. No V2 release gate or V3 immutable copy unit is manufactured here.
use std::sync::Arc;

use detcore_model::network_trace::FreshSendTimeoutV1;
use detcore_model::network_trace::NetworkCreationModelV4;
use detcore_model::network_trace::NetworkEstablishmentV4;
use detcore_model::network_trace::NetworkInputEventV4;
use detcore_model::network_trace::NetworkProgressV4;
use detcore_model::network_trace::NetworkReceiveEntryCutV4;
use detcore_model::network_trace::NetworkReleaseModelV4;
use detcore_model::network_trace::NetworkReleaseNodeIdV4;
use detcore_model::network_trace::NetworkReleaseNodeKindV4;
use detcore_model::network_trace::NetworkReleaseNodeV4;
use detcore_model::network_trace::NetworkReleaseV4;
use detcore_model::network_trace::NetworkTraceV4;
use detcore_model::network_trace::NetworkTraceValidationErrorV4;
use detcore_model::network_trace::ReceiveTimeoutV3;

use super::*;

#[path = "replay_transmit.rs"]
mod replay_transmit;

#[derive(Debug)]
pub(in crate::network_replay) struct NativeState {
    pub(super) trace: NetworkTraceV4,
    replay: Option<NativeReplay>,
    fresh_send: BTreeMap<StreamSocketKeyV3, ReceiveTimeoutV3>,
    pub(super) poll_witnesses: Vec<NativePollWitness>,
    retirement_failure: Option<(NetworkChannelId, NetworkTraceValidationErrorV4)>,
    // Retained only by an actual pre-effect entry issuer. This is lifetime
    // provenance for terminal progress, never a substitute for a fresh grant.
    policy_root: Option<Arc<crate::network_runtime::ForegroundRoot>>,
    policy_failure: bool,
    // Exact original Close admissions, never Connect entries or journal nodes.
    close_policy: BTreeMap<NetworkStreamCallId, NativeClosePolicy>,
}
#[derive(Debug)]
struct NativeClosePolicy {
    root: Arc<crate::network_runtime::ForegroundRoot>,
    arguments: crate::network_replay::original_connect::Arguments,
    epoch: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NativePollWitness {
    pub(super) channel: NetworkChannelId,
    pub(super) stream_offset: u64,
    pub(super) minimum: usize,
}
#[derive(Debug)]
struct NativeReplay {
    released: Vec<bool>,
    connected: BTreeSet<NetworkChannelId>,
    consumed_eof: BTreeSet<u64>,
}
impl NativeState {
    pub(in crate::network_replay) fn mode(&self) -> NetworkEngineMode {
        if self.replay.is_some() {
            NetworkEngineMode::Replay
        } else {
            NetworkEngineMode::Record
        }
    }
    /// The fresh V4 recorder before any journal, send timeout or retirement.
    pub(in crate::network_replay) fn untouched_record(&self) -> bool {
        let t = &self.trace;
        self.replay.is_none()
            && self.fresh_send.is_empty()
            && self.poll_witnesses.is_empty()
            && self.retirement_failure.is_none()
            && self.policy_root.is_none()
            && !self.policy_failure
            && self.close_policy.is_empty()
            && t.channels.is_empty()
            && t.inputs.is_empty()
            && t.outputs.is_empty()
            && t.release_model.nodes().is_empty()
            && t.native_receive_observations.is_empty()
            && t.fresh_stream_profiles.is_empty()
            && t.channel_socket_classes.is_empty()
            && t.fresh_send_timeouts.is_empty()
    }
    pub(in crate::network_replay) fn trace(&self) -> &NetworkTraceV4 {
        &self.trace
    }
    /// Preserve the original first structural retirement error independently
    /// of the later policy check. Finalization reports actual outstanding
    /// custody before policy loss, but never replaces this earlier typed error.
    pub(in crate::network_replay) fn check_retirement_failure(
        &self,
    ) -> Result<(), NetworkReplayError> {
        match &self.retirement_failure {
            Some((channel, error)) => Err(NetworkReplayError::NativeRetirement {
                channel: *channel,
                error: error.clone(),
            }),
            None => Ok(()),
        }
    }
    fn check_retirement(&self) -> Result<(), NetworkReplayError> {
        self.check_retirement_failure()?;
        if self.policy_failure
            || (self.replay.is_none()
                && self
                    .policy_root
                    .as_ref()
                    .is_some_and(|root| !root.has_sole_initial_root_history()))
        {
            return Err(invalid(
                "V4 recorder lost its retained sole-initial-root policy",
            ));
        }
        Ok(())
    }
    pub(in crate::network_replay) fn record(
        &mut self,
    ) -> Result<&mut NetworkTraceV4, NetworkReplayError> {
        self.check_retirement()?;
        if self.replay.is_some() {
            return Err(NetworkReplayError::WrongMode);
        }
        Ok(&mut self.trace)
    }
    pub(in crate::network_replay) fn fresh_send(
        &self,
        key: StreamSocketKeyV3,
    ) -> Option<ReceiveTimeoutV3> {
        self.fresh_send.get(&key).copied()
    }
}

/// One owned attempt, consumed by the worker-admission callback even on refusal.
/// The exact Call retains its marker, so neither reissue nor cross-Call/engine
/// substitution can acquire a later cut.
#[derive(Debug)]
pub(crate) struct NativeEntryAttempt {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    marker: Arc<NativeEntryMarker>,
}

#[derive(Debug)]
pub(in crate::network_replay) struct NativeEntryMarker {
    spent: std::sync::atomic::AtomicBool,
    prefix: std::sync::OnceLock<crate::network_runtime::JoinedNativePrefix>,
}

/// Recovery correlation minted by the actual attempt before it is consumed.
/// Its original joined prefix cannot be replaced by a later worker join.
#[derive(Debug)]
pub(crate) struct UnsubmittedNativeEntry {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    marker: Arc<NativeEntryMarker>,
}
impl NativeEntryAttempt {
    pub(crate) fn retain_unsubmitted_recovery(
        &mut self,
        prefix: &crate::network_runtime::JoinedNativePrefix,
    ) -> Result<UnsubmittedNativeEntry, NetworkReplayError> {
        if prefix.root().owner() != self.owner || self.marker.prefix.set(prefix.clone()).is_err() {
            return Err(invalid(
                "entry recovery changed owner or original joined prefix",
            ));
        }
        Ok(UnsubmittedNativeEntry {
            owner: self.owner,
            call: self.call,
            marker: self.marker.clone(),
        })
    }
}
impl Drop for NativeEntryAttempt {
    fn drop(&mut self) {
        self.marker
            .spent
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
impl UnsubmittedNativeEntry {
    pub(crate) fn prefix(&self) -> &crate::network_runtime::JoinedNativePrefix {
        self.marker
            .prefix
            .get()
            .expect("actual recovery issuer retained its prefix")
    }
}

/// Retained only on the exact admitted Call, before capture/provider submission.
/// This cannot be reconstructed from serialized cut/frontier fields.
#[derive(Debug, Clone)]
pub(in crate::network_replay) struct NativeEntry {
    root: Arc<crate::network_runtime::ForegroundRoot>,
    kind: EntryKind,
    release: NetworkReleaseV4,
    used: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Foreground {
        epoch: u64,
    },
    Connect {
        operation: crate::resources::ExternalOpId,
    },
}

impl NetworkReplayEngine {
    pub(crate) fn check_receive_invocation_entry(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        epoch: u64,
        open_file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        let state = self.owned_stream_call(owner, call)?;
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("checked Read lacks original entry"))?;
        if !self.native_receive_version()
            || self.mode() != NetworkEngineMode::Record
            || state.open_file != Some(open_file)
            || state.phase != StreamCallPhase::Active
            || !state.physical_pin_required
            || state.abandoned
            || state.final_wait
            || entry.used
            || !Arc::ptr_eq(&entry.root, root)
            || !root.is_sole_initial_root(owner)
            || entry.kind != (EntryKind::Foreground { epoch })
        {
            return Err(invalid(
                "checked Read changed actual Call/root/initial entry",
            ));
        }
        Ok(())
    }

    /// A completed canonical empty attempt is the sole predecessor for rearm.
    /// The initial-entry marker remains spent for the entire Call lifetime.
    pub(crate) fn validate_native_receive_retry(
        &self,
        retry: &RecordReceiveRetry,
    ) -> Result<(), NetworkReplayError> {
        retry.check()?;
        self.check_native_retirement()?;
        if !self.native_receive_version() || self.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        let state = self.owned_stream_call(retry.owner(), retry.call())?;
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("receive retry lost consumed entry"))?;
        let open_file = state
            .open_file
            .ok_or_else(|| invalid("receive retry lost held OFD"))?;
        let channel = &self.channels[&self.bound_channel(open_file)?];
        let socket = self.stream_call_socket_state(retry.owner(), retry.call())?;
        if self.stream_calls.len() != 1
            || state.phase != StreamCallPhase::Active
            || !state.physical_pin_required
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.original.is_some()
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || !state.no_store_completed
            || state.native_entry_attempted.is_none()
            || state
                .record_no_store
                .as_ref()
                .is_none_or(|source| !Arc::ptr_eq(source, retry.source()))
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.replay_receive.is_some()
            || !self.shadow_probes.is_empty()
            || !self.socket_controls.is_empty()
            || !self.stream_operations.is_empty()
            || !self.stream_delivery.is_empty()
            || !self.shadow_deliveries.is_empty()
            || !entry.used
            || !Arc::ptr_eq(&entry.root, retry.root())
            || entry.kind
                != (EntryKind::Foreground {
                    epoch: retry.epoch(),
                })
            || !retry.root().is_sole_initial_root(retry.owner())
            || channel.local_read_shutdown
            || channel.peer_write_closed
            || !channel.inbound.is_empty()
            || socket.options.receive_low_water != 1
            || socket.options.receive_timeout != ReceiveTimeoutV3::Infinite
        {
            return Err(invalid(
                "receive retry changed its exact completed empty Call/root/entry",
            ));
        }
        Ok(())
    }

    pub(crate) fn stamp_native_receive_retry(
        &mut self,
        retry: &RecordReceiveRetry,
        admission: &crate::network_runtime::ReceiveRetryAdmission<'_>,
        checked: crate::tool_global::CheckedBlockingReadRetry<'_, '_>,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.validate_native_receive_retry(retry)?;
        let open_file = self.stream_call_open_file(retry.owner(), retry.call())?;
        if !checked.matches(retry, grant, open_file) {
            return Err(invalid(
                "receive retry lacks its exact checked blocking invocation",
            ));
        }
        if !admission.matches(retry)
            || !grant.admits_sole_initial_root(retry.root())
            || grant.owner() != retry.owner()
            || grant.epoch() <= retry.epoch()
            || now < retry.observed_at()
        {
            return Err(invalid(
                "receive retry lacks a new actual same-root foreground grant",
            ));
        }
        let EngineState::Native(native) = &self.mode else {
            unreachable!("validated Record");
        };
        if now < native.trace.epoch_global_time()? {
            return Err(NetworkTraceValidationError::ReleaseBeforeEpoch.into());
        }
        let cut = NetworkReceiveEntryCutV4(
            u64::try_from(native.trace.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?,
        );
        let prerequisites = native
            .trace
            .entry_frontier(cut)
            .map_err(|e| invalid(&e.to_string()))?;
        let entry = NativeEntry {
            root: retry.root().clone(),
            kind: EntryKind::Foreground {
                epoch: grant.epoch(),
            },
            release: NetworkReleaseV4 {
                not_before_global_time: now,
                receive_entry_cut: cut,
                prerequisites,
            },
            used: false,
        };
        // All fallible checks precede both owners' joint commit. The old initial
        // marker is deliberately untouched; this is not an initial-stamp reset.
        retry.mark_completed();
        let state = self.stream_calls.get_mut(&retry.call()).unwrap();
        state.native_entry = Some(entry);
        state.record_no_store = None;
        state.no_store_completed = false;
        Ok(())
    }

    pub(crate) fn native_receive_version(&self) -> bool {
        matches!(self.mode, EngineState::Native(_))
    }
    pub(crate) fn record_native_receive(epoch: DateTime<Utc>) -> Self {
        // Only the empty shared container is reused. There is no V2 history or
        // projected V2 release metadata alongside the authoritative V4 journal.
        let mut engine = Self::record_shadow(epoch);
        engine.mode = EngineState::Native(NativeState {
            trace: NetworkTraceV4 {
                epoch,
                channels: Vec::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                creation_model: NetworkCreationModelV4::OutboundAndDatagramV1,
                release_model: NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 {
                    nodes: Vec::new(),
                },
                native_receive_observations: Vec::new(),
                fresh_stream_profiles: Vec::new(),
                receive_environment: ReceiveEnvironmentV3::SingleRecorderNamespaceV1,
                channel_socket_classes: Vec::new(),
                fresh_send_timeouts: Vec::new(),
            },
            replay: None,
            fresh_send: BTreeMap::new(),
            poll_witnesses: Vec::new(),
            retirement_failure: None,
            policy_root: None,
            policy_failure: false,
            close_policy: BTreeMap::new(),
        });
        engine
    }

    pub(crate) fn replay_native_receive(trace: NetworkTraceV4) -> Result<Self, NetworkReplayError> {
        trace.validate().map_err(|e| invalid(&e.to_string()))?;
        // These are the current positive input producers. Other V4 model rows
        // need their actual consume/error/control issuer before this adapter
        // admits them; availability alone cannot stand in for guest delivery.
        if trace.inputs.iter().any(|input| {
            !matches!(
                input.event,
                NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected)
                    | NetworkInputKindV2::StreamBytes { .. }
                    | NetworkInputKindV2::PeerShutdown {
                        direction: NetworkShutdownV2::Write,
                        ..
                    }
            )
        }) {
            return Err(invalid(
                "V4 shared replay has no consumption issuer for this input kind",
            ));
        }
        let mut engine = Self::record_native_receive(trace.epoch);
        for definition in &trace.channels {
            engine
                .channels
                .insert(definition.id, ChannelState::new(definition));
        }
        for output in &trace.outputs {
            engine
                .channels
                .get_mut(&output.channel)
                .expect("validated channel")
                .append_expected_output(&output.event);
        }
        for state in engine.channels.values_mut() {
            state.refresh_readiness();
        }
        let fresh_send = trace
            .fresh_send_timeouts
            .iter()
            .map(|f| (f.key, f.timeout))
            .collect();
        engine.shadow = Some(ShadowReceiveState {
            environment: trace.receive_environment,
            namespace: None,
            profiles: trace
                .fresh_stream_profiles
                .iter()
                .cloned()
                .map(|p| (p.key, p))
                .collect(),
            channel_classes: trace
                .channel_socket_classes
                .iter()
                .map(|c| (c.channel, c.key))
                .collect(),
            sockets: BTreeMap::new(),
            units: Vec::new(),
            accepted: None,
        });
        let released = vec![false; trace.inputs.len()];
        engine.mode = EngineState::Native(NativeState {
            trace,
            replay: Some(NativeReplay {
                released,
                connected: BTreeSet::new(),
                consumed_eof: BTreeSet::new(),
            }),
            fresh_send,
            poll_witnesses: Vec::new(),
            retirement_failure: None,
            policy_root: None,
            policy_failure: false,
            close_policy: BTreeMap::new(),
        });
        Ok(engine)
    }

    pub(crate) fn into_native_recorded_trace(self) -> Result<NetworkTraceV4, NetworkReplayError> {
        self.check_stream_operations_finished()?;
        let EngineState::Native(mut native) = self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some() {
            return Err(NetworkReplayError::WrongMode);
        }
        native.check_retirement()?;
        if !native.trace.release_model.nodes().is_empty() && native.policy_root.is_none() {
            return Err(invalid("V4 trace lacks an actual sole-root entry issuer"));
        }
        if !native.poll_witnesses.is_empty() {
            return Err(invalid(
                "V4 recording retains readiness not backed by a published receive prefix",
            ));
        }
        let shadow = self.shadow.ok_or(NetworkReplayError::WrongMode)?;
        if !shadow.units.is_empty() || shadow.accepted.is_some() {
            return Err(invalid(
                "V4 cannot acquire V3 copy units or accepted-child release authority",
            ));
        }
        native.trace.fresh_stream_profiles = shadow.profiles.into_values().collect();
        native.trace.channel_socket_classes = shadow
            .channel_classes
            .into_iter()
            .map(|(channel, key)| ChannelSocketClassV3 { channel, key })
            .collect();
        native.trace.receive_environment = shadow.environment;
        native.trace.fresh_send_timeouts = native
            .fresh_send
            .into_iter()
            .map(|(key, timeout)| FreshSendTimeoutV1 { key, timeout })
            .collect();
        native
            .trace
            .validate()
            .map_err(|e| invalid(&e.to_string()))?;
        Ok(native.trace)
    }

    pub(in crate::network_replay) fn native_input_count(
        &self,
    ) -> Result<usize, NetworkReplayError> {
        match &self.mode {
            EngineState::Record(trace) => Ok(trace.inputs.len()),
            EngineState::Native(native) if native.replay.is_none() => {
                native.check_retirement()?;
                Ok(native.trace.inputs.len())
            }
            _ => Err(NetworkReplayError::WrongMode),
        }
    }

    /// Retire a poll-only probe. A positive readable observation is retained
    /// only as a run-local obligation: a later private receive must publish at
    /// least this low-water prefix at the same physical stream cut. No poll
    /// result itself becomes portable trace authority.
    pub(crate) fn finish_native_poll_probe(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        minimum: usize,
    ) -> Result<NetworkNativePollObservation, NetworkReplayError> {
        if !(1..=NETWORK_STREAM_CHUNK_LIMIT).contains(&minimum) {
            return Err(invalid(
                "V4 poll low-water exceeds its receive publication bound",
            ));
        }
        let probe = self.owned_shadow_probe(owner, lease)?.clone();
        self.native_transmit_entry(owner, probe.call)?;
        let state = self.owned_stream_call(owner, probe.call)?;
        let open_file = state.open_file.expect("owned stream call");
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.mode() != NetworkEngineMode::Record
            || probe.pending.is_some()
            || probe.peek.is_some()
            || probe.cursor_observed
            || probe.current_cursor != probe.original_cursor
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let revents = probe
            .poll
            .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?;
        let readable = revents & libc::POLLIN != 0;
        if readable != probe.queued.is_some() || probe.queued.is_some_and(|queued| queued < minimum)
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let queued = probe.queued;
        let terminal = revents & (libc::POLLERR | libc::POLLHUP | libc::POLLRDHUP);
        let witness = if readable && terminal == 0 {
            Some(NativePollWitness {
                channel: probe.channel,
                stream_offset: self
                    .channels
                    .get(&probe.channel)
                    .expect("probe pins channel")
                    .inbound_consumed,
                minimum,
            })
        } else {
            None
        };
        self.shadow_probes.remove(&lease);
        self.socket_controls.remove(&open_file);
        self.complete_deferred_retirement(open_file);
        if let Some(witness) = witness {
            let EngineState::Native(native) = &mut self.mode else {
                unreachable!();
            };
            native.poll_witnesses.push(witness);
        }
        Ok(NetworkNativePollObservation { revents, queued })
    }

    pub(in crate::network_replay) fn native_definitions_mut(
        &mut self,
    ) -> Result<&mut Vec<NetworkChannelV2>, NetworkReplayError> {
        match &mut self.mode {
            EngineState::Record(trace) => Ok(&mut trace.channels),
            EngineState::Native(native) => Ok(&mut native.record()?.channels),
            _ => Err(NetworkReplayError::WrongMode),
        }
    }

    /// Called only by the actual original Socket installation transaction after
    /// its retained held observation passed fresh_profile (including SNDTIMEO).
    /// A standalone profile registration cannot grant V4 send-timeout facts.
    pub(super) fn retain_native_fresh_send(&mut self, key: StreamSocketKeyV3) {
        if let EngineState::Native(native) = &mut self.mode
            && native.replay.is_none()
        {
            native
                .fresh_send
                .entry(key)
                .or_insert(ReceiveTimeoutV3::Infinite);
            self.shadow
                .as_mut()
                .expect("native shadow")
                .sockets
                .values_mut()
                .filter(|s| s.key == key)
                .for_each(|s| s.send_timeout = Some(ReceiveTimeoutV3::Infinite));
        }
    }

    /// Keep the first failed retirement visible even to void task-exit cleanup.
    /// Physical teardown still completes; no later recorder access can turn the
    /// retained semantic custody into a successful trace.
    pub(in crate::network_replay) fn check_native_retirement(
        &self,
    ) -> Result<(), NetworkReplayError> {
        match &self.mode {
            EngineState::Native(native) => native.check_retirement(),
            _ => Ok(()),
        }
    }

    /// Transfer the actual selected Close reader under the sole-root policy.
    /// The original-call issuer authenticates the reader/slot/source and owns
    /// cancellation. No channel, Connect entry, input or progress is required.
    pub(crate) fn begin_native_original_close_from_read(
        &mut self,
        arguments: crate::network_replay::original_connect::Arguments,
        read: NetworkFdReadAdmission,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        grant: &crate::scheduler::fd_read::NativeCaptureEntryObservation<'_>,
    ) -> Result<crate::network_replay::original_connect::Admission, NetworkReplayError> {
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let root = admission.root();
        if arguments.kind != crate::network_replay::original_connect::Kind::Close
            || arguments.operation != grant.operation()
            || read.external_grant != Some(grant.operation())
            || arguments.files != root.files()
            || !grant.admits_sole_initial_root(root)
        {
            return Err(invalid(
                "V4 original Close lacks its actual sole-root selected reader",
            ));
        }
        if native
            .policy_root
            .as_ref()
            .is_some_and(|old| !Arc::ptr_eq(old, root))
        {
            return Err(invalid("V4 original Close changed retained root authority"));
        }
        let call = self.begin_original_external_from_read(grant.owner(), arguments, read)?;
        // No fallible work follows transfer. Retain the exact source locally,
        // including in Replay: Replay closes real placeholders/loader files
        // but never gains Record progress or a fabricated recorded result.
        let active = &self.stream_calls;
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native
            .close_policy
            .retain(|call, _| active.contains_key(call));
        native.policy_root = Some(root.clone());
        native.close_policy.insert(
            call.call,
            NativeClosePolicy {
                root: root.clone(),
                arguments: call.arguments.clone(),
                epoch: grant.epoch(),
            },
        );
        Ok(call)
    }

    /// Recheck the same admission after preparation and before invocation.
    /// Cleanup/terminal paths deliberately do not require a live grant.
    pub(crate) fn validate_native_original_close_policy(
        &self,
        admission: &crate::network_replay::original_connect::Admission,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        grant: &crate::scheduler::fd_read::NativeCaptureEntryObservation<'_>,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        self.check_stream_owner(grant.owner())?;
        // Original Close owns a Native original Call even for a loader file or
        // EBADF (no open_file). Authenticate its retained owner, source and
        // exact arguments; the generic stream accessor excludes this family.
        let _ = self.original_connect_result(grant.owner(), admission)?;
        if self.stream_calls[&admission.call].abandoned {
            return Err(NetworkReplayError::UnresolvedStreamCall(admission.call));
        }
        let retained = native
            .close_policy
            .get(&admission.call)
            .ok_or_else(|| invalid("V4 original Close lost its pre-effect policy admission"))?;
        if retained.arguments != admission.arguments
            || retained.arguments.operation != grant.operation()
            || retained.epoch != grant.epoch()
            || !Arc::ptr_eq(&retained.root, root)
            || !grant.admits_sole_initial_root(root)
            || native
                .policy_root
                .as_ref()
                .is_none_or(|old| !Arc::ptr_eq(old, root))
        {
            return Err(invalid(
                "V4 original Close changed its retained Call/root/grant",
            ));
        }
        Ok(())
    }

    /// Admit a foreground descriptor close before its physical submission.
    /// A close of an unconnected socket needs no receive/Connect event. Its
    /// actual control, joined worker prefix and current sole-root borrow are
    /// sufficient policy provenance, retained through deferred Call-pin release.
    /// Original-syscall Close has a separate selected-call admission path; it
    /// must prove the same policy before submission rather than calling this
    /// after Linux has selected or removed the descriptor.
    pub(crate) fn submit_native_descriptor_close(
        &mut self,
        control: NetworkStreamLeaseId,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some() {
            return Err(NetworkReplayError::WrongMode);
        }
        let root = admission.root();
        if !grant.admits_sole_initial_root(root) {
            return Err(invalid(
                "V4 close lacks its borrowed sole-initial-root grant",
            ));
        }
        if native
            .policy_root
            .as_ref()
            .is_some_and(|previous| !Arc::ptr_eq(previous, root))
        {
            return Err(invalid(
                "V4 close changed its retained initial-root authority",
            ));
        }
        let held = self.owned_socket_control(grant.owner(), control)?;
        if !held.physical.can_release_unchanged()
            || held.physical.descriptor_released.is_some()
            || self.shadow_probes.contains_key(&control)
        {
            return Err(invalid(
                "V4 close policy admission requires an unsubmitted control",
            ));
        }
        self.bound_channel(held.open_file)?;
        // The ordinary descriptor state machine validates and retains the
        // exact pending CloseDescriptor. No syscall or fallible work follows
        // it in this engine-lock/worker-admission critical section.
        self.submit_descriptor_effect(
            grant.owner(),
            control,
            NetworkDescriptorEffect::CloseDescriptor,
        )?;
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!("validated recorder");
        };
        native.policy_root = Some(root.clone());
        Ok(())
    }

    /// Preflight the next progress node while the final channel binding still
    /// exists. Only an infallible append follows successful validation; callers
    /// then remove that binding in the same engine-lock critical section.
    pub(in crate::network_replay) fn retain_native_retirement(
        &mut self,
        channel: NetworkChannelId,
    ) -> Result<(), NetworkReplayError> {
        let EngineState::Native(native) = &mut self.mode else {
            return Ok(());
        };
        native.check_retirement()?;
        if native.replay.is_some() {
            return Ok(());
        }
        let plan = (|| {
            if native
                .trace
                .channels
                .iter()
                .filter(|c| c.id == channel)
                .count()
                != 1
            {
                return Err(NetworkTraceValidationErrorV4::InvalidReference);
            }
            let cut = NetworkReceiveEntryCutV4(
                u64::try_from(native.trace.release_model.nodes().len())
                    .map_err(|_| NetworkTraceValidationErrorV4::Overflow)?,
            );
            let prerequisites = native.trace.entry_frontier(cut)?;
            if native.trace.release_model.nodes().iter().any(|node| {
                matches!(node.kind, NetworkReleaseNodeKindV4::Progress {
                    channel: previous,
                    milestone: NetworkProgressV4::Retired,
                } if previous == channel)
            }) {
                return Err(NetworkTraceValidationErrorV4::EventAfterRetirement);
            }
            Ok(NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(cut.0),
                kind: NetworkReleaseNodeKindV4::Progress {
                    channel,
                    milestone: NetworkProgressV4::Retired,
                },
                prerequisites,
            })
        })();
        let node = match plan {
            Ok(node) => node,
            Err(error) => {
                native.retirement_failure = Some((channel, error.clone()));
                return Err(NetworkReplayError::NativeRetirement { channel, error });
            }
        };
        // Cleanup may outlive the final task, but cannot invent a policy root
        // from the final frontier or restore a premise lost to a sibling.
        if native
            .policy_root
            .as_ref()
            .is_none_or(|root| !root.has_sole_initial_root_history())
        {
            native.policy_failure = true;
            return Err(invalid(
                "V4 retirement lacks retained sole-initial-root history",
            ));
        }
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model;
        nodes.push(node);
        Ok(())
    }

    /// Latch the attempt before validating a possibly stale joined prefix. A
    /// failed attempt stays on this Call and cannot acquire a newer entry cut.
    pub(crate) fn begin_native_entry_stamp(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<NativeEntryAttempt, NetworkReplayError> {
        self.check_native_retirement()?;
        if !self.native_receive_version() || self.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        let state = self
            .stream_calls
            .get_mut(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner
            || state.native_entry_attempted.is_some()
            || state.abandoned
            || state.final_wait
        {
            return Err(invalid(
                "native receive entry attempt is one use on its actual Call",
            ));
        }
        let marker = Arc::new(NativeEntryMarker {
            spent: std::sync::atomic::AtomicBool::new(false),
            prefix: std::sync::OnceLock::new(),
        });
        state.native_entry_attempted = Some(marker.clone());
        Ok(NativeEntryAttempt {
            owner,
            call,
            marker,
        })
    }

    pub(crate) fn cancel_retained_unsubmitted_native_entry(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        original: &crate::network_runtime::JoinedNativePrefix,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
    ) -> Result<(), NetworkReplayError> {
        if !admission.is_original_prefix(original) {
            return Err(invalid("unsubmitted cleanup changed retained local prefix"));
        }
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let marker = state
            .native_entry_attempted
            .as_ref()
            .ok_or_else(|| invalid("unsubmitted cleanup lost its original attempt"))?
            .clone();
        if marker
            .prefix
            .get()
            .is_none_or(|p| !admission.is_original_prefix(p))
        {
            return Err(invalid(
                "unsubmitted cleanup cannot replace original prefix",
            ));
        }
        self.cancel_unsubmitted_native_entry(
            &UnsubmittedNativeEntry {
                owner,
                call,
                marker,
            },
            admission,
        )
    }

    /// This is a no-submission transaction, not a failed native capture or a
    /// physical retirement acknowledgement. The borrowed runtime admission
    /// holds actual worker and Calls absence through the complete mutation.
    pub(crate) fn cancel_unsubmitted_native_entry(
        &mut self,
        retained: &UnsubmittedNativeEntry,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
    ) -> Result<(), NetworkReplayError> {
        let owner = retained.owner;
        let call = retained.call;
        if !self.native_receive_version()
            || self.mode() != NetworkEngineMode::Record
            || !self.fd_table_capability()
            || !retained
                .marker
                .spent
                .load(std::sync::atomic::Ordering::Acquire)
            || !admission.is_original_prefix(retained.prefix())
        {
            return Err(invalid(
                "unsubmitted recovery lacks its spent actual entry prefix",
            ));
        }
        self.check_stream_owner(owner)?;
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner || state.abandoned || state.open_file.is_none() {
            return Err(invalid(
                "unsubmitted recovery changed its actual owner/Call",
            ));
        }
        if let Some(original) = &state.original {
            if !original.native_entry_cancellable() {
                return Err(invalid(
                    "unsubmitted Connect already owns a native invocation",
                ));
            }
        }
        if state
            .native_entry_attempted
            .as_ref()
            .is_none_or(|m| !Arc::ptr_eq(m, &retained.marker))
            || state.phase != StreamCallPhase::PinAcquireSubmitted
            || !state.physical_pin_required
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.native_entry.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.replay_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || self
                .zero_stream_waits
                .values()
                .any(|wait| wait.call == call)
        {
            return Err(invalid(
                "unsubmitted recovery changed exact Call or admitted effects",
            ));
        }
        let file = state.open_file.expect("owned stream Call has an OFD");
        let control = state
            .capture_control
            .ok_or_else(|| invalid("unsubmitted recovery lost descriptor control"))?;
        let permit = state
            .capture_publication
            .ok_or_else(|| invalid("unsubmitted recovery lost table permit"))?;
        let actual_metadata = self.fd_metadata(owner, permit.files)?;
        if !admission.root().matches_metadata(&actual_metadata) {
            return Err(invalid(
                "unsubmitted recovery changed actual engine/root metadata custody",
            ));
        }
        let held = self.owned_socket_control(owner, control)?;
        if held.open_file != file
            || !held.physical.can_release_unchanged()
            || self.shadow_probes.contains_key(&control)
            || self
                .stream_operations
                .values()
                .any(|op| op.open_file == file)
        {
            return Err(invalid(
                "unsubmitted recovery has a pending descriptor effect",
            ));
        }
        self.validate_publication_permit(owner, permit)?;
        let publication = &self.fd_publications[&permit.files];
        if publication.reader.is_some()
            || publication.pending.is_some()
            || publication.enrollment.is_some()
        {
            return Err(invalid(
                "unsubmitted recovery cannot erase table publication custody",
            ));
        }
        // All table/control validation precedes the only fallible mutation.
        // The lifetime operation checks its exact lease before changing it.
        self.release_registered_stream_call_lifetime(
            owner,
            call,
            file,
            lifetime::TransportResolution::CancellationAcknowledgedBeforeSubmission,
        )?;
        self.fd_publications.get_mut(&permit.files).unwrap().active = None;
        self.stream_calls.remove(&call);
        assert_eq!(self.socket_controls.remove(&file).unwrap().lease, control);
        self.complete_deferred_retirement(file);
        Ok(())
    }

    pub(crate) fn stamp_native_receive_entry(
        &mut self,
        attempt: NativeEntryAttempt,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        if !grant.admits_sole_initial_root(admission.root()) {
            return Err(invalid(
                "V4 entry lacks its borrowed sole-initial-root grant",
            ));
        }
        self.stamp_native_entry(
            grant.owner(),
            attempt,
            admission.root().clone(),
            EntryKind::Foreground {
                epoch: grant.epoch(),
            },
            now,
        )
    }

    pub(crate) fn stamp_native_connect_entry(
        &mut self,
        attempt: NativeEntryAttempt,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        grant: &crate::scheduler::fd_read::NativeCaptureEntryObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        if !grant.admits_sole_initial_root(admission.root()) {
            return Err(invalid(
                "V4 connect lacks its borrowed sole-initial-root grant",
            ));
        }
        self.stamp_native_entry(
            grant.owner(),
            attempt,
            admission.root().clone(),
            EntryKind::Connect {
                operation: grant.operation(),
            },
            now,
        )
    }

    fn stamp_native_entry(
        &mut self,
        owner: NetworkStreamOwner,
        attempt: NativeEntryAttempt,
        root: Arc<crate::network_runtime::ForegroundRoot>,
        kind: EntryKind,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let call = attempt.call;
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some() || attempt.owner != owner || !root.is_sole_initial_root(owner) {
            return Err(invalid("V4 entry requires the actual live recorder root"));
        }
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state
                .native_entry_attempted
                .as_ref()
                .is_none_or(|marker| !Arc::ptr_eq(marker, &attempt.marker))
            || state.native_entry.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || self.stream_operations.values().any(|op| op.owner == owner)
            || self.stream_calls.keys().any(|id| *id != call)
        {
            return Err(invalid(
                "V4 entry is late, repeated, or belongs to another admitted Call",
            ));
        }
        match (kind, &state.original) {
            (EntryKind::Connect { operation }, Some(original))
                if original.native_entry_unsubmitted(operation) => {}
            (EntryKind::Foreground { .. }, None)
                if state.phase == StreamCallPhase::PinAcquireSubmitted
                    && state.physical_pin_required => {}
            _ => {
                return Err(invalid(
                    "V4 entry follows physical admission or changed grant family",
                ));
            }
        }
        if now < native.trace.epoch_global_time()? {
            return Err(NetworkTraceValidationError::ReleaseBeforeEpoch.into());
        }
        let cut = NetworkReceiveEntryCutV4(
            u64::try_from(native.trace.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?,
        );
        let prerequisites = native
            .trace
            .entry_frontier(cut)
            .map_err(|e| invalid(&e.to_string()))?;
        if native
            .policy_root
            .as_ref()
            .is_some_and(|previous| !Arc::ptr_eq(previous, &root))
        {
            return Err(invalid(
                "V4 entry changed its retained initial-root authority",
            ));
        }
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!("validated recorder");
        };
        native.policy_root = Some(root.clone());
        self.stream_calls.get_mut(&call).unwrap().native_entry = Some(NativeEntry {
            root,
            kind,
            release: NetworkReleaseV4 {
                not_before_global_time: now,
                receive_entry_cut: cut,
                prerequisites,
            },
            used: false,
        });
        Ok(())
    }

    fn native_entry_release_for(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        kind: EntryKind,
        observed_at: LogicalTime,
    ) -> Result<NetworkReleaseV4, NetworkReplayError> {
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("V4 publication lacks actual entry"))?;
        if native.replay.is_some()
            || state.owner != owner
            || state.abandoned
            || state.final_wait
            || entry.used
            || !Arc::ptr_eq(&entry.root, root)
            || entry.kind != kind
            || !root.is_sole_initial_root(owner)
            || observed_at < entry.release.not_before_global_time
            // Publication must close the same outstanding producer interval.
            // Recomputing the old frontier alone cannot account for progress
            // appended after entry; that progress must never be omitted or
            // repaired by refreshing the cut at completion.
            || u64::try_from(native.trace.release_model.nodes().len()).ok()
                != Some(entry.release.receive_entry_cut.0)
            || native
                .trace
                .entry_frontier(entry.release.receive_entry_cut)
                .map_err(|e| invalid(&e.to_string()))?
                != entry.release.prerequisites
        {
            return Err(invalid(
                "V4 publication changed its one-use entry/root/grant/ledger",
            ));
        }
        let mut release = entry.release.clone();
        // Only paid continuous time is sampled at completion. The producer
        // frontier and cut remain the exact pre-effect entry values.
        release.not_before_global_time = observed_at;
        Ok(release)
    }
    pub(super) fn native_entry_release(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        epoch: u64,
        now: LogicalTime,
    ) -> Result<NetworkReleaseV4, NetworkReplayError> {
        self.native_entry_release_for(owner, call, root, EntryKind::Foreground { epoch }, now)
    }

    /// Recheck an existing foreground Call under the current scheduler borrow,
    /// including after a worker join. Retained entry provenance alone does not
    /// establish that the original Normal grant is still open.
    pub(crate) fn validate_native_foreground_call(
        &self,
        call: NetworkStreamCallId,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let state = self.owned_stream_call(grant.owner(), call)?;
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("V4 foreground Call lacks its original entry"))?;
        if !grant.admits_sole_initial_root(&entry.root) {
            return Err(invalid(
                "V4 foreground Call lacks its current sole-root borrow",
            ));
        }
        self.native_entry_release_for(
            grant.owner(),
            call,
            &entry.root,
            EntryKind::Foreground {
                epoch: grant.epoch(),
            },
            now,
        )?;
        Ok(())
    }

    /// Derive replay facts only from operations committed to the shared queues.
    /// Expected output bytes and input release flags are not producer evidence.
    fn native_completed(&self) -> Result<BTreeSet<NetworkReleaseNodeIdV4>, NetworkReplayError> {
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let replay = native
            .replay
            .as_ref()
            .ok_or(NetworkReplayError::WrongMode)?;
        let mut outputs: BTreeMap<NetworkChannelId, Vec<usize>> = BTreeMap::new();
        for (n, row) in native.trace.outputs.iter().enumerate() {
            outputs.entry(row.channel).or_default().push(n);
        }
        let mut consumed_outputs = BTreeSet::new();
        let mut datagrams = BTreeMap::new();
        for (channel, rows) in outputs {
            let consumed = rows
                .len()
                .checked_sub(self.channels[&channel].outbound.len())
                .ok_or_else(|| invalid("V4 shared output queue exceeds its trace"))?;
            let mut packets = 0u64;
            for n in &rows[..consumed] {
                consumed_outputs.insert(*n as u64);
                if matches!(
                    native.trace.outputs[*n].event,
                    NetworkOutputKindV2::Datagram(_) | NetworkOutputKindV2::DatagramExact(_)
                ) {
                    packets += 1;
                }
            }
            datagrams.insert(channel, packets);
        }
        let mut completed = BTreeSet::new();
        loop {
            let before = completed.len();
            for node in native.trace.release_model.nodes() {
                if completed.contains(&node.id)
                    || node.prerequisites.iter().any(|n| !completed.contains(n))
                {
                    continue;
                }
                let observed = match &node.kind {
                    NetworkReleaseNodeKindV4::Input { input_ordinal } => {
                        let n = *input_ordinal as usize;
                        let input = &native.trace.inputs[n];
                        replay.released[n] && match &input.event {
                            NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected) => replay.connected.contains(&input.channel),
                            NetworkInputKindV2::StreamBytes { stream_offset, bytes } =>
                                self.channels[&input.channel].inbound_consumed >= *stream_offset + bytes.len() as u64,
                            NetworkInputKindV2::PeerShutdown { direction: NetworkShutdownV2::Write, .. } =>
                                replay.consumed_eof.contains(input_ordinal),
                            _ => false,
                        }
                    }
                    NetworkReleaseNodeKindV4::Progress { channel, milestone } => match milestone {
                        NetworkProgressV4::Established { source: NetworkEstablishmentV4::ConnectedInput { input_ordinal } } =>
                            replay.connected.contains(channel) && completed.contains(&native.trace.release_model.nodes().iter()
                                .find(|n| matches!(n.kind, NetworkReleaseNodeKindV4::Input { input_ordinal: i } if i == *input_ordinal)).unwrap().id),
                        NetworkProgressV4::Established { source: NetworkEstablishmentV4::DatagramSetup } =>
                            self.reverse_bindings.contains_key(channel) || self.retired_channels.contains(channel),
                        NetworkProgressV4::StreamPrefix { exclusive_offset } => self.channels[channel].transmitted >= *exclusive_offset,
                        NetworkProgressV4::DatagramPrefix { completed: count } => datagrams.get(channel).copied().unwrap_or(0) >= *count,
                        NetworkProgressV4::LocalShutdown { output_ordinal } | NetworkProgressV4::OutputError { output_ordinal } =>
                            consumed_outputs.contains(output_ordinal),
                        NetworkProgressV4::Retired => self.retired_channels.contains(channel),
                    },
                };
                if observed {
                    completed.insert(node.id);
                }
            }
            if before == completed.len() {
                break;
            }
        }
        Ok(completed)
    }

    pub(in crate::network_replay) fn release_native_eligible(
        &mut self,
        now: LogicalTime,
    ) -> Result<BTreeSet<NetworkChannelId>, NetworkReplayError> {
        let completed = self.native_completed()?;
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        let replay = native.replay.as_mut().unwrap();
        let mut blocked = BTreeSet::new();
        let mut selected = Vec::new();
        for (n, input) in native.trace.inputs.iter().enumerate() {
            if replay.released[n] || blocked.contains(&input.channel) {
                continue;
            }
            if !input.release.is_eligible(now, &completed) {
                blocked.insert(input.channel);
                continue;
            }
            selected.push(n);
        }
        let mut ready = BTreeSet::new();
        // No fallible operation follows the first queue mutation. The frozen
        // set is independent of whether a released row was consumed this turn.
        for n in selected {
            let input = &native.trace.inputs[n];
            self.channels
                .get_mut(&input.channel)
                .unwrap()
                .release_at(input.ordinal, input.event.clone());
            replay.released[n] = true;
            ready.insert(input.channel);
        }
        Ok(ready)
    }

    pub(in crate::network_replay) fn next_native_release_time(
        &self,
    ) -> Result<Option<LogicalTime>, NetworkReplayError> {
        let completed = self.native_completed()?;
        let EngineState::Native(native) = &self.mode else {
            unreachable!();
        };
        let replay = native.replay.as_ref().unwrap();
        let mut seen = BTreeSet::new();
        Ok(native
            .trace
            .inputs
            .iter()
            .enumerate()
            .filter(|(n, input)| !replay.released[*n] && seen.insert(input.channel))
            .filter(|(_, input)| {
                input
                    .release
                    .prerequisites
                    .iter()
                    .all(|n| completed.contains(n))
            })
            .map(|(_, input)| input.release.not_before_global_time)
            .min())
    }

    pub(in crate::network_replay) fn native_connection_delivered(
        &mut self,
        channel: NetworkChannelId,
        outcome: &ConnectionOutcome,
    ) {
        if matches!(
            outcome,
            ConnectionOutcome::Connect(NetworkConnectionResultV2::Connected)
        ) && let EngineState::Native(native) = &mut self.mode
            && let Some(replay) = &mut native.replay
        {
            replay.connected.insert(channel);
        }
    }
    pub(in crate::network_replay) fn finish_native_replay(&self) -> Result<(), NetworkReplayError> {
        let completed = self.native_completed()?;
        let EngineState::Native(native) = &self.mode else {
            unreachable!();
        };
        let replay = native.replay.as_ref().unwrap();
        if replay.released.iter().any(|done| !done)
            || completed.len() != native.trace.release_model.nodes().len()
        {
            return Err(NetworkReplayError::UnconsumedTrace);
        }
        for (channel, state) in &self.channels {
            if !state.inbound.is_empty() || !state.outbound.is_empty() {
                return Err(NetworkReplayError::UnconsumedChannel(*channel));
            }
        }
        Ok(())
    }
}

impl NetworkReplayEngine {
    pub(super) fn consume_native_entry(&mut self, call: NetworkStreamCallId) {
        let entry = self
            .stream_calls
            .get_mut(&call)
            .unwrap()
            .native_entry
            .as_mut()
            .unwrap();
        assert!(!entry.used, "validated one-use native entry");
        entry.used = true;
    }

    /// Require the same pre-capture entry used by receive. A later transmit
    /// frontier cannot certify the original capture or a sibling's output.
    fn native_transmit_entry(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<NetworkReleaseV4, NetworkReplayError> {
        let state = self.owned_stream_call(owner, call)?;
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("V4 transmit lacks its pre-capture sole-root entry"))?;
        if !matches!(entry.kind, EntryKind::Foreground { .. }) {
            return Err(invalid("V4 transmit changed entry grant family"));
        }
        self.native_entry_release_for(
            owner,
            call,
            &entry.root,
            entry.kind,
            entry.release.not_before_global_time,
        )
    }

    /// Reserve one short OFD control for an immutable, helper-owned send.
    pub(crate) fn begin_native_transmit(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        bytes: Vec<u8>,
        flags: i32,
    ) -> Result<NetworkStreamLeaseId, NetworkReplayError> {
        if !(1..=512).contains(&bytes.len()) || flags != libc::MSG_NOSIGNAL {
            return Err(invalid(
                "V4 native transmit is not the bounded MSG_NOSIGNAL shape",
            ));
        }
        let state = self.owned_stream_call(owner, call)?;
        let open_file = state.open_file.expect("owned stream call");
        let channel = self.bound_channel(open_file)?;
        let definition = self
            .channel_definitions()
            .iter()
            .find(|definition| definition.id == channel)
            .ok_or(NetworkReplayError::UnknownChannel(channel))?;
        if definition.transport != NetworkTransportV2::Tcp
            || definition.role != NetworkEndpointRoleV2::OutboundClient
        {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.mode() != NetworkEngineMode::Record
            || !native.trace.release_model.nodes().iter().any(|node| {
                matches!(node.kind, NetworkReleaseNodeKindV4::Progress {
                    channel: established,
                    milestone: NetworkProgressV4::Established { .. },
                } if established == channel)
            })
        {
            return Err(NetworkReplayError::WrongMode);
        }
        native_stream_output_offset(&native.trace, channel)?;
        let entry = self.native_transmit_entry(owner, call)?;
        let entry_cut = entry.receive_entry_cut;
        let prerequisites = entry.prerequisites;
        let lease = self.begin_stream_call_control(owner, call)?;
        self.socket_controls
            .get_mut(&open_file)
            .expect("new transmit control")
            .physical
            .transmit_pending = Some(NativeTransmitPending {
            call,
            bytes,
            flags,
            entry_cut,
            prerequisites,
            submitted: false,
            timing_identity: None,
            timing_claimed: false,
            timing_normal_epoch: None,
            timing_receipt: None,
        });
        Ok(lease)
    }

    pub(in crate::network_replay) fn submit_native_transmit(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        bytes: Vec<u8>,
        flags: i32,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let open_file = control.open_file;
        let pending = control
            .physical
            .transmit_pending
            .as_ref()
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        if pending.submitted || pending.bytes != bytes || pending.flags != flags {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        self.native_transmit_entry(owner, pending.call)?;
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated transmit control")
            .physical
            .transmit_pending
            .as_mut()
            .expect("validated transmit")
            .submitted = true;
        Ok(())
    }

    pub(crate) fn confirm_native_transmit_if_pending(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed: &crate::network_runtime::native_peer::Observation,
    ) -> Option<Result<(), NetworkReplayError>> {
        let pending = self
            .socket_controls
            .values()
            .find(|control| control.lease == lease)
            .and_then(|control| control.physical.transmit_pending.clone())?;
        Some(self.confirm_native_transmit(owner, lease, pending, observed))
    }

    fn confirm_native_transmit(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        pending: NativeTransmitPending,
        observed: &crate::network_runtime::native_peer::Observation,
    ) -> Result<(), NetworkReplayError> {
        // Scheduler timing is not original-task return provenance, and an
        // output-only V4 row cannot carry entry/completion/handback authority.
        // Never silently fall back to the old writer after enrollment.
        if pending.timing_claimed {
            return Err(invalid(
                "timed original send requires a versioned attempt writer",
            ));
        }
        let control = self.owned_socket_control(owner, lease)?;
        if !pending.submitted {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let open_file = control.open_file;
        self.owned_stream_call(owner, pending.call)?;
        let entry = self.native_transmit_entry(owner, pending.call)?;
        if entry.receive_entry_cut != pending.entry_cut
            || entry.prerequisites != pending.prerequisites
        {
            return Err(invalid("V4 transmit replaced its original entry proof"));
        }
        let channel = self.bound_channel(open_file)?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.mode() != NetworkEngineMode::Record
            || u64::try_from(native.trace.release_model.nodes().len()).ok()
                != Some(pending.entry_cut.0)
            || native
                .trace
                .entry_frontier(pending.entry_cut)
                .map_err(|error| invalid(&error.to_string()))?
                != pending.prerequisites
        {
            return Err(invalid(
                "V4 native transmit changed its one-use entry frontier",
            ));
        }
        let stream_offset = native_stream_output_offset(&native.trace, channel)?;
        let output_ordinal =
            u64::try_from(native.trace.outputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let node_id = u64::try_from(native.trace.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let (event, milestone) = match &observed.confirmation {
            NetworkStreamPhysicalResult::Transmitted { count }
                if observed.raw_return == *count as i64
                    && observed.errno.is_none()
                    && observed.bytes.is_empty()
                    && (1..=pending.bytes.len()).contains(count) =>
            {
                let exclusive_offset = stream_offset
                    .checked_add(*count as u64)
                    .ok_or(NetworkReplayError::Overflow)?;
                (
                    NetworkOutputKindV2::StreamBytes {
                        stream_offset,
                        bytes: pending.bytes[..*count].to_vec(),
                    },
                    NetworkProgressV4::StreamPrefix { exclusive_offset },
                )
            }
            NetworkStreamPhysicalResult::Errno(errno)
                if observed.raw_return == -1
                    && observed.errno == Some(*errno)
                    && observed.bytes.is_empty()
                    && (1..=4095).contains(errno) =>
            {
                (
                    NetworkOutputKindV2::SocketError {
                        stream_offset,
                        errno: *errno,
                    },
                    NetworkProgressV4::OutputError { output_ordinal },
                )
            }
            _ => return Err(NetworkReplayError::UnresolvedStreamOperation(lease)),
        };
        let output = NetworkOutputEventV2 { channel, event };
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(node_id),
            kind: NetworkReleaseNodeKindV4::Progress { channel, milestone },
            prerequisites: pending.prerequisites.clone(),
        };
        let mut candidate = native.trace.clone();
        candidate.outputs.push(output.clone());
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut candidate.release_model;
        nodes.push(node.clone());
        let shadow = self.shadow.as_ref().ok_or(NetworkReplayError::WrongMode)?;
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
        candidate.fresh_send_timeouts = native
            .fresh_send
            .iter()
            .map(|(key, timeout)| FreshSendTimeoutV1 {
                key: *key,
                timeout: *timeout,
            })
            .collect();
        candidate
            .validate()
            .map_err(|error| invalid(&error.to_string()))?;

        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        native.trace.outputs.push(output);
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model;
        nodes.push(node);
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated transmit control")
            .physical
            .transmit_pending = None;
        self.consume_native_entry(pending.call);
        Ok(())
    }
}

fn native_stream_output_offset(
    trace: &NetworkTraceV4,
    channel: NetworkChannelId,
) -> Result<u64, NetworkReplayError> {
    let mut offset = 0u64;
    for output in trace
        .outputs
        .iter()
        .filter(|output| output.channel == channel)
    {
        let (at, count) = match &output.event {
            NetworkOutputKindV2::StreamBytes {
                stream_offset,
                bytes,
            }
            | NetworkOutputKindV2::StreamMessage {
                stream_offset,
                bytes,
                ..
            } => (*stream_offset, bytes.len()),
            NetworkOutputKindV2::SocketError { stream_offset, .. }
            | NetworkOutputKindV2::Shutdown { stream_offset, .. } => (*stream_offset, 0),
            _ => return Err(NetworkReplayError::TransportMismatch(channel)),
        };
        if at != offset {
            return Err(NetworkTraceValidationError::NonContiguousOutput.into());
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or(NetworkReplayError::Overflow)?;
    }
    Ok(offset)
}

impl NetworkReplayEngine {
    pub(crate) fn publish_native_connected(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &original_connect::Admission,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        completed: &crate::network_runtime::native_peer::CompletedNativeConnect<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let open_file = self.original_native_connected(owner, admission)?;
        let peer = completed
            .peer(owner, admission)
            .map_err(|e| invalid(&e.to_string()))?;
        let channel = self.bound_channel(open_file)?;
        let definition = self
            .channel_definitions()
            .iter()
            .find(|d| d.id == channel)
            .unwrap();
        if definition.transport != NetworkTransportV2::Tcp
            || definition.role != NetworkEndpointRoleV2::OutboundClient
            || definition.peer_address.as_ref() != Some(&peer)
        {
            return Err(invalid("V4 Connect channel changed actual original peer"));
        }
        let release = self.native_entry_release_for(
            owner,
            admission.call,
            root,
            EntryKind::Connect {
                operation: admission.arguments.operation,
            },
            now,
        )?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.mode() != NetworkEngineMode::Record
            || native.trace.inputs.iter().any(|i| i.channel == channel)
            || native.trace.release_model.nodes().iter().any(|node| {
                matches!(node.kind,
                NetworkReleaseNodeKindV4::Progress { channel: c, .. } if c == channel)
            })
        {
            return Err(invalid(
                "V4 Connect is one successful establishment per channel",
            ));
        }
        let ordinal =
            u64::try_from(native.trace.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let first = u64::try_from(native.trace.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let established = first.checked_add(1).ok_or(NetworkReplayError::Overflow)?;
        established
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let input = NetworkInputEventV4 {
            ordinal,
            channel,
            release: release.clone(),
            event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        };
        let input_node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(first),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
        let progress = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(established),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::Established {
                    source: NetworkEstablishmentV4::ConnectedInput {
                        input_ordinal: ordinal,
                    },
                },
            },
            prerequisites: vec![NetworkReleaseNodeIdV4(first)],
        };
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        native.trace.inputs.push(input);
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model;
        nodes.extend([input_node, progress]);
        self.consume_native_entry(admission.call);
        Ok(())
    }
}

#[cfg(test)]
#[path = "versioned/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "versioned/policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
impl NetworkReplayEngine {
    /// The prior Connected row and original socket origin are controlled
    /// premises. Entry/Store/Drain/publication use their real local issuers;
    /// this fixture is not native Connect or provider qualification.
    pub(crate) fn controlled_native_receive_pending(
        owner: NetworkStreamOwner,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
    ) -> (
        Self,
        NetworkStreamCallId,
        NetworkStreamLeaseId,
        NetworkStreamPhysicalEffect,
    ) {
        let (mut engine, call) = tests::unsubmitted_entry(owner);
        let ofd = engine.stream_calls[&call].open_file.unwrap();
        let channel = engine.bound_channel(ofd).unwrap();
        let binding = crate::types::FdSlotBinding {
            slot: crate::types::FdSlot {
                files: crate::types::FilesId::initial(owner.thread),
                fd: 17,
            },
            open_file: ofd,
            generation: 1,
        };
        let key = engine.shadow.as_ref().unwrap().sockets[&ofd].key;
        engine
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&ofd)
            .unwrap()
            .native = Some(NativeReceive {
            identity: FileIdentity::controlled_fixture(7, 19),
            binding,
            birth: call,
            physical_observed: Cut::ZERO,
        });
        engine.retain_native_fresh_send(key);
        let now = LogicalTime::from_nanos(1_790_000_000_000_000_000);
        let EngineState::Native(native) = &mut engine.mode else {
            unreachable!()
        };
        native.trace.inputs.push(NetworkInputEventV4 {
            ordinal: 0,
            channel,
            release: NetworkReleaseV4 {
                not_before_global_time: now,
                receive_entry_cut: NetworkReceiveEntryCutV4(0),
                prerequisites: vec![],
            },
            event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        });
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model;
        nodes.extend([
            NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(0),
                kind: NetworkReleaseNodeKindV4::Input { input_ordinal: 0 },
                prerequisites: vec![],
            },
            NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(1),
                kind: NetworkReleaseNodeKindV4::Progress {
                    channel,
                    milestone: NetworkProgressV4::Established {
                        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
                    },
                },
                prerequisites: vec![NetworkReleaseNodeIdV4(0)],
            },
        ]);
        let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
        engine
            .stamp_native_receive_entry(attempt, admission, grant, now)
            .unwrap();
        let control = engine.socket_controls[&ofd].lease;
        engine
            .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let lease = engine.begin_shadow_probe(owner, call, now).unwrap().lease;
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
    pub(crate) fn native_trace_fixture(&self) -> NetworkTraceV4 {
        let EngineState::Native(native) = &self.mode else {
            panic!("native engine")
        };
        let mut trace = native.trace.clone();
        let shadow = self.shadow.as_ref().unwrap();
        trace.fresh_stream_profiles = shadow.profiles.values().cloned().collect();
        trace.channel_socket_classes = shadow
            .channel_classes
            .iter()
            .map(|(channel, key)| ChannelSocketClassV3 {
                channel: *channel,
                key: *key,
            })
            .collect();
        trace.fresh_send_timeouts = native
            .fresh_send
            .iter()
            .map(|(key, timeout)| FreshSendTimeoutV1 {
                key: *key,
                timeout: *timeout,
            })
            .collect();
        trace
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Controlled original-socket creation premise for Connect publication
    /// controls. The OFD is the actual admitted logical binding; this helper
    /// supplies no entry, provider, backend-return or retirement authority.
    pub(crate) fn controlled_connect_socket_premise(&mut self, open_file: OpenFileId) {
        use detcore_model::network_trace::*;
        let profile = FreshStreamSocketProfileV3 {
            key: StreamSocketKeyV3 {
                transport: NetworkTransportV2::Tcp,
                domain: libc::AF_INET,
                socket_type: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            },
            normalization: LinuxReceiveNormalizationV3 {
                hz: LinuxReceiveHzV3::Hz1000,
                peek_offset_set_supported: true,
                system_rmem_max: 212_992,
                namespace_tcp_rmem_max: 6_291_456,
                minimum_receive_buffer: 2304,
            },
            initial: StreamSocketOptionsV3 {
                peek_offset: Some(-1),
                receive_low_water: 1,
                receive_timeout: ReceiveTimeoutV3::Infinite,
                receive_buffer: ReceiveBufferStateV3 {
                    bytes: 131_072,
                    user_locked: false,
                    tcp_scaling_ratio: 128,
                },
            },
        };
        self.register_stream_socket_profile(
            open_file,
            profile.key,
            NetworkStreamNamespace {
                device: 7,
                inode: 11,
            },
            Some(profile.clone()),
        )
        .unwrap();
        self.retain_native_fresh_send(profile.key);
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Read-only consumer/producer census for actual issuer+store controls.
    pub(crate) fn controlled_replay_delivery_state(
        &self,
        file: OpenFileId,
    ) -> (u64, Vec<Vec<u8>>, Vec<u64>) {
        let channel = self.bound_channel(file).unwrap();
        let queue = &self.channels[&channel];
        let bytes = queue
            .inbound
            .iter()
            .map(|event| match event {
                InboundOutcome::Stream {
                    bytes,
                    requires_message_io: false,
                    ..
                } => bytes.iter().copied().collect(),
                other => panic!("unexpected control in byte-only fixture: {other:?}"),
            })
            .collect();
        (
            queue.inbound_consumed,
            bytes,
            self.native_completed()
                .unwrap()
                .into_iter()
                .map(|node| node.0)
                .collect(),
        )
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Controlled malformed journal only; the RPC control must perform and
    /// acknowledge its actual native pin close through the production path.
    pub(crate) fn controlled_native_retirement_ledger_defect(&mut self) {
        let EngineState::Native(native) = &mut self.mode else {
            panic!("native recorder required")
        };
        assert!(native.replay.is_none());
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model;
        assert!(!nodes.is_empty());
        nodes[0].id = NetworkReleaseNodeIdV4(99);
    }
}

impl NetworkReplayEngine {
    /// The immutable EOF identity is distinct from readiness and release. Only
    /// the private no-store transaction below advances its consumed frontier.
    pub(super) fn native_replay_eof(
        &self,
        channel: NetworkChannelId,
        offset: u64,
    ) -> Result<Option<(u64, bool, bool)>, NetworkReplayError> {
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let replay = native
            .replay
            .as_ref()
            .ok_or(NetworkReplayError::WrongMode)?;
        let mut rows = native.trace.inputs.iter().filter(|row| {
            row.channel == channel
                && matches!(row.event, NetworkInputKindV2::PeerShutdown {
                stream_offset, direction: NetworkShutdownV2::Write } if stream_offset == offset)
        });
        let Some(row) = rows.next() else {
            return Ok(None);
        };
        if rows.next().is_some() {
            return Err(invalid("Replay EOF has ambiguous terminal identity"));
        }
        Ok(Some((
            row.ordinal,
            replay.released[row.ordinal as usize],
            replay.consumed_eof.contains(&row.ordinal),
        )))
    }
    pub(super) fn mark_native_replay_eof_consumed(&mut self, ordinal: u64) {
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native
            .replay
            .as_mut()
            .expect("prevalidated Replay")
            .consumed_eof
            .insert(ordinal);
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayNoStoreFixtureState {
    pub consumed: u64,
    pub bytes: Vec<Vec<u8>>,
    pub eof_offsets: Vec<u64>,
    pub peer_closed: bool,
    pub consume_epoch: u64,
    pub completed: Vec<u64>,
    pub consumed_eof: Vec<u64>,
    pub released: Vec<bool>,
}
#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn replay_no_store_fixture_state(
        &self,
        file: OpenFileId,
    ) -> ReplayNoStoreFixtureState {
        let channel = self.bound_channel(file).unwrap();
        let queue = &self.channels[&channel];
        let mut bytes = Vec::new();
        let mut eof_offsets = Vec::new();
        for row in &queue.inbound {
            match row {
                InboundOutcome::Stream {
                    bytes: data,
                    ancillary: None,
                    message_flags: 0,
                    requires_message_io: false,
                } => bytes.push(data.iter().copied().collect()),
                InboundOutcome::PeerShutdown {
                    stream_offset,
                    direction: NetworkShutdownV2::Write,
                } => eof_offsets.push(*stream_offset),
                other => panic!("unexpected outcome in exact Replay EOF fixture: {other:?}"),
            }
        }
        let EngineState::Native(native) = &self.mode else {
            panic!("actual V4 engine required")
        };
        let replay = native.replay.as_ref().unwrap();
        ReplayNoStoreFixtureState {
            consumed: queue.inbound_consumed,
            bytes,
            eof_offsets,
            peer_closed: queue.peer_write_closed,
            consume_epoch: self.shadow.as_ref().unwrap().sockets[&file].consume_epoch,
            completed: self
                .native_completed()
                .unwrap()
                .into_iter()
                .map(|id| id.0)
                .collect(),
            consumed_eof: replay.consumed_eof.iter().copied().collect(),
            released: replay.released.clone(),
        }
    }
    pub(crate) fn replay_no_store_fixture_call(
        &self,
        owner: NetworkStreamOwner,
    ) -> NetworkStreamCallId {
        assert_eq!(self.stream_calls.len(), 1);
        let (call, state) = self.stream_calls.iter().next().unwrap();
        assert_eq!(state.owner, owner);
        assert!(!state.physical_pin_required);
        *call
    }
}
