//! V4 nonconsuming readiness snapshots, never V3 Peek or expected-output state.
use detcore_model::network_trace::tcp_poll_row_mask;
use detcore_model::network_trace::valid_tcp_poll_mask;

use super::*;

#[cfg(test)]
impl NetworkReplayEngine {
    /// Establish only the controlled startup connection for an already-bound
    /// fixture channel. This creates no entry, grant, pin or physical result.
    pub(crate) fn controlled_poll_connected_channel(
        &mut self,
        file: OpenFileId,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let channel = self.bound_channel(file)?;
        let EngineState::Native(native) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some()
            || native.trace.channels.len() != 1
            || native.trace.channels[0].id != channel
            || !native.trace.inputs.is_empty()
            || !native.trace.outputs.is_empty()
            || !native.trace.release_model.nodes().is_empty()
            || now < native.trace.epoch_global_time()?
        {
            return Err(invalid(
                "controlled poll connection changed startup channel",
            ));
        }
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
            &mut native.trace.release_model else { panic!("legacy fixture changed its release policy"); };
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
        Ok(())
    }
}

/// A released immutable shared trace input, never application-time controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SharedPollSnapshot {
    pub(super) ordinal: u64,
    pub(super) consumed_prefix: u64,
    pub(super) observed_at: LogicalTime,
    pub(super) revents: i16,
    pub(super) control_generation: u64,
    pub(super) receive_low_water: u32,
}

#[derive(Debug, Default)]
pub(super) struct PollSnapshots {
    latest: BTreeMap<NetworkChannelId, (u64, LogicalTime, i16)>,
    shared_latest: BTreeMap<NetworkChannelId, SharedPollSnapshot>,
    applied: BTreeSet<u64>,
}

impl PollSnapshots {
    pub(super) fn apply(
        &mut self,
        ordinal: u64,
        channel: NetworkChannelId,
        consumed_prefix: u64,
        observed_at: LogicalTime,
        revents: i16,
    ) {
        self.latest
            .insert(channel, (consumed_prefix, observed_at, revents));
        self.applied.insert(ordinal);
    }

    pub(super) fn completed(&self, ordinal: u64) -> bool {
        self.applied.contains(&ordinal)
    }

    fn observation_at(
        &self,
        channel: NetworkChannelId,
        consumed: u64,
    ) -> Result<(LogicalTime, i16), NetworkReplayError> {
        match self.latest.get(&channel) {
            Some(&(cut, time, mask)) if cut == consumed => Ok((time, mask)),
            _ => Err(invalid(
                "V4 poll has no eligible observation at its consumed-byte cut",
            )),
        }
    }

    pub(super) fn apply_shared(
        &mut self,
        ordinal: u64,
        channel: NetworkChannelId,
        snapshot: SharedPollSnapshot,
    ) {
        self.shared_latest.insert(channel, snapshot);
        self.applied.insert(ordinal);
    }

    pub(super) fn shared_observation_at(
        &self,
        channel: NetworkChannelId,
        consumed: u64,
        control_generation: u64,
        receive_low_water: u32,
    ) -> Result<Option<SharedPollSnapshot>, NetworkReplayError> {
        let Some(sample) = self.shared_latest.get(&channel) else {
            return Ok(None);
        };
        if sample.consumed_prefix != consumed {
            return Ok(None);
        }
        if sample.control_generation != control_generation
            || sample.receive_low_water != receive_low_water
        {
            return Err(invalid(
                "shared Poll observation has stale recorded controls",
            ));
        }
        Ok(Some(*sample))
    }

    #[cfg(test)]
    fn at(&self, channel: NetworkChannelId, consumed: u64) -> Result<i16, NetworkReplayError> {
        self.observation_at(channel, consumed).map(|(_, mask)| mask)
    }
}

impl NetworkReplayEngine {
    /// Retain the completed initial full-state observation before admitting
    /// exactly one bounded requested-mask wait on the same original Call.
    /// Preparation neither publishes a snapshot nor creates a new grant.
    pub(crate) fn prepare_native_poll_wait(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
        request: (i16, u64),
    ) -> Result<i16, NetworkReplayError> {
        let (events, timeout_ns) = request;
        if self.mode() != NetworkEngineMode::Record || !self.native_receive_version() {
            return Err(NetworkReplayError::WrongMode);
        }
        self.require_no_helper_copy_for_lease(owner, lease)?;
        let probe = self.owned_shadow_probe(owner, lease)?;
        self.validate_native_foreground_call(call, grant, now)?;
        let initial = probe
            .poll
            .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?;
        if probe.call != call
            || grant.owner() != owner
            || probe.pending.is_some()
            || probe.peek.is_some()
            || probe.queued.is_some()
            || probe.cursor_observed
            || probe.current_cursor != probe.original_cursor
            || probe.poll_wait.is_some()
            || now < probe.began
            || !valid_tcp_poll_mask(initial)
            || !valid_tcp_poll_mask(events)
            || tcp_poll_row_mask(initial, events) != 0
            || !(1..1_000_000_000).contains(&timeout_ns)
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        now.as_nanos()
            .checked_add(timeout_ns)
            .ok_or(NetworkReplayError::Overflow)?;
        self.shadow_probes
            .get_mut(&lease)
            .expect("validated probe")
            .poll_wait = Some(NativePollWaitState {
            initial,
            observed_at: now,
            events,
            timeout_ns,
            returned: None,
        });
        Ok(initial)
    }

    /// Only a completed read-only initial PollState can be ordinarily aborted.
    /// A prepared/started wait remains under the existing terminal owner even
    /// if some later completion is known. This is not the Peek cursor abort.
    pub(crate) fn check_native_poll_abort(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
    ) -> Result<(), NetworkReplayError> {
        if self.mode() != NetworkEngineMode::Record || !self.native_receive_version() {
            return Err(NetworkReplayError::WrongMode);
        }
        self.require_no_helper_copy_for_lease(owner, lease)?;
        let probe = self.owned_shadow_probe(owner, lease)?;
        self.stream_call_open_file(owner, call)?;
        let state = self.owned_stream_call(owner, call)?;
        if probe.call != call
            || probe.pending.is_some()
            || probe.peek.is_some()
            || probe.queued.is_some()
            || probe.cursor_observed
            || probe.current_cursor != probe.original_cursor
            || probe.poll_wait.is_some()
            || probe.poll.is_none_or(|mask| !valid_tcp_poll_mask(mask))
            || state.native_entry.as_ref().is_none_or(|entry| entry.used)
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        Ok(())
    }

    /// The Global caller keeps the engine lock across this preflight, actual
    /// runtime confirmed-lease removal, and commit. Spending the entry prevents
    /// reusing the aborted attempt; no observation or elapsed time is published.
    pub(crate) fn abort_native_poll_known(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
    ) -> Result<(), NetworkReplayError> {
        self.check_native_poll_abort(owner, call, lease)?;
        let file = self.stream_call_open_file(owner, call)?;
        self.consume_native_entry(call);
        self.shadow_probes.remove(&lease);
        self.socket_controls.remove(&file);
        self.complete_deferred_retirement(file);
        Ok(())
    }

    /// The existing physical Pending and actual foreground entry have already
    /// joined. Publish the whole raw mask, including zero, as one transaction;
    /// a completed wait publishes both samples at the unchanged entry frontier.
    pub(crate) fn publish_native_poll(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
    ) -> Result<i16, NetworkReplayError> {
        self.require_sole_initial_release_policy()?;
        let probe = self.owned_shadow_probe(owner, lease)?.clone();
        self.validate_native_foreground_call(probe.call, grant, now)?;
        if probe.call != call
            || grant.owner() != owner
            || probe.pending.is_some()
            || probe.peek.is_some()
            || probe.queued.is_some()
            || probe.cursor_observed
            || probe.current_cursor != probe.original_cursor
            || now < probe.began
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let revents = probe
            .poll
            .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?;
        if !valid_tcp_poll_mask(revents) {
            return Err(invalid("V4 poll returned unsupported raw mask"));
        }
        let state = self.owned_stream_call(owner, probe.call)?;
        let file = state.open_file.expect("owned stream Call");
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("poll lost entry"))?;
        let release =
            self.native_entry_release(owner, probe.call, &entry.root, grant.epoch(), now)?;
        let mut observations = Vec::with_capacity(2);
        if let Some(wait) = &probe.poll_wait {
            let returned = wait
                .returned
                .ok_or(NetworkReplayError::UnresolvedStreamOperation(lease))?;
            let deadline = wait
                .observed_at
                .as_nanos()
                .checked_add(wait.timeout_ns)
                .ok_or(NetworkReplayError::Overflow)?;
            if wait.observed_at < probe.began
                || now <= wait.observed_at
                || !valid_tcp_poll_mask(wait.initial)
                || !valid_tcp_poll_mask(wait.events)
                || !valid_tcp_poll_mask(returned)
                || tcp_poll_row_mask(wait.initial, wait.events) != 0
                || tcp_poll_row_mask(returned, wait.events) != returned
                || !(1..1_000_000_000).contains(&wait.timeout_ns)
                // Real readiness at the final scan wins, including after a
                // zero wait result. A vanished nonzero wake is not a timeout.
                || (tcp_poll_row_mask(revents, wait.events) == 0
                    && (returned != 0 || now.as_nanos() < deadline))
            {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
            let initial_release = self.native_entry_release(
                owner,
                probe.call,
                &entry.root,
                grant.epoch(),
                wait.observed_at,
            )?;
            observations.push((initial_release, wait.initial));
        }
        observations.push((release, revents));
        let consumed_prefix = self.channels[&probe.channel].inbound_consumed;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let mut candidate = native.trace.clone();
        let mut additions = Vec::with_capacity(observations.len());
        for (release, observed) in observations {
            let ordinal =
                u64::try_from(candidate.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
            let node = u64::try_from(candidate.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?;
            let input = NetworkInputEventV4 {
                ordinal,
                channel: probe.channel,
                release: release.clone(),
                event: NetworkInputKindV2::RawTcpPollState {
                    consumed_prefix,
                    revents: observed,
                },
            };
            candidate.inputs.push(input.clone());
            let node = NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(node),
                kind: NetworkReleaseNodeKindV4::Input {
                    input_ordinal: ordinal,
                },
                prerequisites: release.prerequisites,
            };
            let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
                &mut candidate.release_model else { return Err(invalid("legacy V4 publisher requires sole-initial-root policy")); };
            nodes.push(node.clone());
            additions.push((input, node));
        }
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
        candidate.validate().map_err(|e| invalid(&e.to_string()))?;
        // Nothing fallible follows semantic publication.
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model else { return Err(invalid("legacy V4 publisher requires sole-initial-root policy")); };
        for (input, node) in additions {
            native.trace.inputs.push(input);
            nodes.push(node);
        }
        self.consume_native_entry(probe.call);
        self.shadow_probes.remove(&lease);
        self.socket_controls.remove(&file);
        self.complete_deferred_retirement(file);
        Ok(revents)
    }

    /// Repeated polls do not consume a snapshot. Input-node completion records
    /// application to model state, independently of how many callers inspect it.
    pub(crate) fn replay_native_poll(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        now: LogicalTime,
    ) -> Result<i16, NetworkReplayError> {
        self.replay_native_poll_observation(owner, call, now)
            .map(|(_, mask)| mask)
    }

    /// Retain the input's actual release time, not the caller's current time.
    /// An old eligible zero is not evidence that a later wait deadline elapsed.
    pub(crate) fn replay_native_poll_observation(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        now: LogicalTime,
    ) -> Result<(LogicalTime, i16), NetworkReplayError> {
        let file = self.stream_call_open_file(owner, call)?;
        let channel = self.bound_channel(file)?;
        if self.mode() != NetworkEngineMode::Replay || !self.native_receive_version() {
            return Err(NetworkReplayError::WrongMode);
        }
        // Releasing a readiness input can satisfy the next input's prerequisite.
        // Each successful iteration releases at least one previously unseen row.
        loop {
            if self.release_native_eligible(now)?.is_empty() {
                break;
            }
        }
        let EngineState::Native(native) = &self.mode else {
            unreachable!()
        };
        native
            .replay
            .as_ref()
            .unwrap()
            .poll
            .observation_at(channel, self.channels[&channel].inbound_consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordFixture {
        engine: NetworkReplayEngine,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        probe: NetworkShadowProbe,
        now: LogicalTime,
        scheduler: crate::scheduler::Scheduler,
        root: Arc<crate::network_runtime::ForegroundRoot>,
        runtime: crate::network_runtime::NetworkRuntimeResources,
        prefix: crate::network_runtime::JoinedNativePrefix,
        // The original root holds Weak references; retain both ledgers through
        // every assertion instead of manufacturing a fresh root at publication.
        _metadata: Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
        _memory: Arc<std::sync::Mutex<crate::memory::MemoryMetadata>>,
    }

    async fn record_fixture() -> RecordFixture {
        // Provider, established channel and pin completion are explicit
        // component premises. The entry is the existing actual scheduler/root
        // borrow; the native poll syscall is tested separately in native_peer.
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
        let owner = root.owner();
        let (runtime, prefix) =
            crate::network_runtime::controlled_joined_prefix(root.clone()).await;
        let (mut engine, call) = super::super::tests::unsubmitted_entry(owner);
        let file = engine.stream_calls[&call].open_file.unwrap();
        let control = engine.socket_controls[&file].lease;
        let channel = engine.bound_channel(file).unwrap();
        let key = engine.shadow.as_ref().unwrap().sockets[&file].key;
        engine.retain_native_fresh_send(key);
        let EngineState::Native(native) = &mut engine.mode else {
            unreachable!()
        };
        let now = native.trace.epoch_global_time().unwrap();
        let config = crate::config::Config {
            epoch: native.trace.epoch,
            ..Default::default()
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
            &mut native.trace.release_model else { panic!("legacy fixture changed its release policy"); };
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
        let mut scheduler = crate::scheduler::Scheduler::new(&config);
        scheduler.controlled_foreground_store_grant(&root);
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .unwrap();
        let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |borrow| {
                engine
                    .stamp_native_receive_entry(attempt, borrow, &grant, now)
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        engine
            .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let probe = engine.begin_shadow_probe(owner, call, now).unwrap();
        RecordFixture {
            engine,
            owner,
            call,
            probe,
            now,
            scheduler,
            root,
            runtime,
            prefix,
            _metadata,
            _memory,
        }
    }

    #[tokio::test]
    async fn raw_poll_record_publishes_only_completed_same_call_at_original_entry() {
        let RecordFixture {
            mut engine,
            owner,
            call,
            probe,
            now,
            scheduler,
            root,
            runtime,
            prefix,
            _metadata,
            _memory,
        } = record_fixture().await;
        let file = engine.stream_calls[&call].open_file.unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .publish_native_poll(owner, call, probe.lease, &grant, now)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::PollState,
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .confirm_shadow_probe_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalResult::PollState {
                        revents: libc::POLLNVAL
                    }
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PollState { revents: 0 },
            )
            .unwrap();
        let before = format!("{engine:?}");
        let foreign = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        assert!(
            engine
                .publish_native_poll(foreign, call, probe.lease, &grant, now)
                .is_err()
        );
        assert!(
            engine
                .publish_native_poll(
                    owner,
                    call,
                    probe.lease,
                    &grant,
                    LogicalTime::from_nanos(now.as_nanos() - 1)
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine
                .publish_native_poll(owner, call, probe.lease, &grant, now)
                .unwrap(),
            0
        );
        let trace = engine.native_trace_fixture();
        trace.validate().unwrap();
        assert_eq!(
            trace.inputs[1].event,
            NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 0,
                revents: 0
            }
        );
        assert_eq!(
            trace.inputs[1].release.receive_entry_cut,
            NetworkReceiveEntryCutV4(2)
        );
        assert_eq!(trace.inputs[1].release.not_before_global_time, now);
        assert_eq!(
            trace.inputs[1].release.prerequisites,
            [NetworkReleaseNodeIdV4(1)]
        );
        assert!(
            engine
                .publish_native_poll(owner, call, probe.lease, &grant, now)
                .is_err()
        );
        assert_eq!(engine.native_trace_fixture(), trace);
        engine.begin_stream_call_release(owner, call).unwrap();
        engine.finish_stream_call_release(owner, call).unwrap();

        // A second controlled completed observation at the same byte cut and
        // time must fail candidate validation before any semantic mutation.
        // This is a component premise, not a second physical poll receipt.
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, control).unwrap().id;
        let attempt = engine.begin_native_entry_stamp(owner, call).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |borrow| {
                engine
                    .stamp_native_receive_entry(attempt, borrow, &grant, now)
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        engine
            .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let probe = engine.begin_shadow_probe(owner, call, now).unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::PollState,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PollState { revents: 0 },
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.publish_native_poll(owner, call, probe.lease, &grant, now),
            Err(NetworkReplayError::FdPublicationProtocol(message))
                if message == NetworkTraceValidationErrorV4::InvalidNativeObservation.to_string()
        ));
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(engine.native_trace_fixture(), trace);

        // Adding a later final sample cannot repair the ambiguous initial
        // sample. Candidate validation must reject the whole pair atomically.
        engine
            .prepare_native_poll_wait(owner, call, probe.lease, &grant, now, (libc::POLLIN, 100))
            .unwrap();
        complete_wait(&mut engine, owner, probe.lease, (libc::POLLIN, 100), 0);
        complete_poll(&mut engine, owner, probe.lease, 0);
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.publish_native_poll(
                owner, call, probe.lease, &grant,
                LogicalTime::from_nanos(now.as_nanos() + 100),
            ),
            Err(NetworkReplayError::FdPublicationProtocol(message))
                if message == NetworkTraceValidationErrorV4::InvalidNativeObservation.to_string()
        ));
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(engine.native_trace_fixture(), trace);
    }

    fn complete_poll(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        revents: i16,
    ) {
        engine
            .submit_shadow_probe_physical(owner, lease, NetworkStreamPhysicalEffect::PollState)
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                lease,
                NetworkStreamPhysicalResult::PollState { revents },
            )
            .unwrap();
    }

    fn complete_wait(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        request: (i16, u64),
        revents: i16,
    ) {
        let (events, timeout_ns) = request;
        engine
            .submit_shadow_probe_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::PollWait { events, timeout_ns },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                lease,
                NetworkStreamPhysicalResult::PollWait { revents },
            )
            .unwrap();
    }

    #[tokio::test]
    async fn raw_poll_wait_preparation_requires_exact_completed_initial_and_original_grant() {
        let mut f = record_fixture().await;
        let grant = f
            .scheduler
            .foreground_native_observation(f.owner, &f.root)
            .unwrap();
        let request = (libc::POLLIN, 100);
        let before = format!("{:?}", f.engine);
        assert!(
            f.engine
                .prepare_native_poll_wait(f.owner, f.call, f.probe.lease, &grant, f.now, request,)
                .is_err()
        );
        assert!(
            f.engine
                .submit_shadow_probe_physical(
                    f.owner,
                    f.probe.lease,
                    NetworkStreamPhysicalEffect::PollWait {
                        events: request.0,
                        timeout_ns: request.1
                    },
                )
                .is_err(),
            "no unprepared physical wait"
        );
        assert_eq!(format!("{:?}", f.engine), before);

        complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
        let before = format!("{:?}", f.engine);
        for bad in [
            (libc::POLLIN, 0),
            (libc::POLLIN, 1_000_000_000),
            (libc::POLLNVAL, 100),
            (i16::MIN, 100),
            (libc::POLLOUT, 100), // initial requested projection already ready
        ] {
            assert!(
                f.engine
                    .prepare_native_poll_wait(f.owner, f.call, f.probe.lease, &grant, f.now, bad,)
                    .is_err()
            );
            assert_eq!(format!("{:?}", f.engine), before);
        }
        let wrong_owner = NetworkStreamOwner {
            mm: f.owner.mm.for_exec(f.owner.thread),
            ..f.owner
        };
        assert!(
            f.engine
                .prepare_native_poll_wait(
                    wrong_owner,
                    f.call,
                    f.probe.lease,
                    &grant,
                    f.now,
                    request,
                )
                .is_err()
        );
        assert!(
            f.engine
                .prepare_native_poll_wait(
                    f.owner,
                    NetworkStreamCallId(f.call.0 + 1),
                    f.probe.lease,
                    &grant,
                    f.now,
                    request,
                )
                .is_err()
        );
        for time in [
            LogicalTime::from_nanos(f.now.as_nanos() - 1),
            LogicalTime::MAX,
        ] {
            assert!(f.engine.prepare_native_poll_wait(
                f.owner, f.call, f.probe.lease, &grant, time, request,
            ).is_err());
        }
        assert_eq!(format!("{:?}", f.engine), before);
        let original = f.engine.shadow_probes[&f.probe.lease].clone();
        for case in 0..4 {
            let probe = f.engine.shadow_probes.get_mut(&f.probe.lease).unwrap();
            match case {
                0 => probe.pending = Some(NetworkStreamPhysicalEffect::QueuedBytes),
                1 => probe.peek = Some(Ok(0)),
                2 => probe.queued = Some(0),
                3 => probe.current_cursor = Some(2),
                _ => unreachable!(),
            }
            let changed = format!("{:?}", f.engine);
            assert!(
                f.engine
                    .prepare_native_poll_wait(
                        f.owner,
                        f.call,
                        f.probe.lease,
                        &grant,
                        f.now,
                        request,
                    )
                    .is_err(),
                "unresolved probe phase {case}"
            );
            assert_eq!(format!("{:?}", f.engine), changed);
            f.engine
                .shadow_probes
                .insert(f.probe.lease, original.clone());
        }
        assert_eq!(format!("{:?}", f.engine), before);
        assert_eq!(
            f.engine
                .prepare_native_poll_wait(f.owner, f.call, f.probe.lease, &grant, f.now, request,)
                .unwrap(),
            libc::POLLOUT
        );
        let prepared = format!("{:?}", f.engine);
        assert!(
            f.engine
                .prepare_native_poll_wait(f.owner, f.call, f.probe.lease, &grant, f.now, request,)
                .is_err(),
            "one wait per original probe"
        );
        assert_eq!(format!("{:?}", f.engine), prepared);
    }

    #[tokio::test]
    async fn raw_poll_wait_publishes_initial_and_final_atomically_without_refreshing_frontier() {
        // All physical completions here are explicit component premises;
        // native ppoll/held-OFD behavior is covered by native_peer's tests.
        for (returned, final_mask, elapsed) in [
            (libc::POLLIN, libc::POLLIN | libc::POLLOUT, 23),
            (0, libc::POLLOUT, 100),
            (0, libc::POLLIN | libc::POLLOUT, 113),
        ] {
            let mut f = record_fixture().await;
            let grant = f
                .scheduler
                .foreground_native_observation(f.owner, &f.root)
                .unwrap();
            let initial_time = LogicalTime::from_nanos(f.now.as_nanos() + 7);
            let final_time = LogicalTime::from_nanos(initial_time.as_nanos() + elapsed);
            complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
            let original = f.engine.native_trace_fixture();
            f.engine
                .prepare_native_poll_wait(
                    f.owner,
                    f.call,
                    f.probe.lease,
                    &grant,
                    initial_time,
                    (libc::POLLIN, 100),
                )
                .unwrap();
            assert_eq!(
                f.engine.native_trace_fixture(),
                original,
                "preparation publishes nothing"
            );
            complete_wait(
                &mut f.engine,
                f.owner,
                f.probe.lease,
                (libc::POLLIN, 100),
                returned,
            );
            complete_poll(&mut f.engine, f.owner, f.probe.lease, final_mask);
            assert_eq!(
                f.engine
                    .publish_native_poll(f.owner, f.call, f.probe.lease, &grant, final_time,)
                    .unwrap(),
                final_mask
            );
            let trace = f.engine.native_trace_fixture();
            trace.validate().unwrap();
            assert_eq!(trace.inputs.len(), original.inputs.len() + 2);
            assert_eq!(
                trace.release_model.nodes().len(),
                original.release_model.nodes().len() + 2
            );
            for (input, (time, mask)) in trace.inputs[original.inputs.len()..]
                .iter()
                .zip([(initial_time, libc::POLLOUT), (final_time, final_mask)])
            {
                assert_eq!(input.release.not_before_global_time, time);
                assert_eq!(input.release.receive_entry_cut, NetworkReceiveEntryCutV4(2));
                assert_eq!(input.release.prerequisites, [NetworkReleaseNodeIdV4(1)]);
                assert_eq!(
                    input.event,
                    NetworkInputKindV2::RawTcpPollState {
                        consumed_prefix: 0,
                        revents: mask,
                    }
                );
            }
            assert!(
                f.engine
                    .publish_native_poll(f.owner, f.call, f.probe.lease, &grant, final_time,)
                    .is_err()
            );
            assert_eq!(f.engine.native_trace_fixture(), trace);
            f.engine.begin_stream_call_release(f.owner, f.call).unwrap();
            f.engine
                .finish_stream_call_release(f.owner, f.call)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn raw_poll_wait_refuses_omitted_wait_missing_final_time_and_disappeared_wake() {
        let mut f = record_fixture().await;
        let grant = f
            .scheduler
            .foreground_native_observation(f.owner, &f.root)
            .unwrap();
        complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
        f.engine
            .prepare_native_poll_wait(
                f.owner,
                f.call,
                f.probe.lease,
                &grant,
                f.now,
                (libc::POLLIN, 100),
            )
            .unwrap();
        let later = LogicalTime::from_nanos(f.now.as_nanos() + 101);
        let before = format!("{:?}", f.engine);
        assert!(
            f.engine
                .publish_native_poll(f.owner, f.call, f.probe.lease, &grant, later)
                .is_err()
        );
        assert_eq!(
            format!("{:?}", f.engine),
            before,
            "no omitted-wait publication"
        );
        complete_wait(
            &mut f.engine,
            f.owner,
            f.probe.lease,
            (libc::POLLIN, 100),
            0,
        );
        let before = format!("{:?}", f.engine);
        assert!(
            f.engine
                .publish_native_poll(f.owner, f.call, f.probe.lease, &grant, later)
                .is_err()
        );
        assert_eq!(
            format!("{:?}", f.engine),
            before,
            "wait is not the final full-state probe"
        );
        complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
        let before = format!("{:?}", f.engine);
        for time in [f.now, LogicalTime::from_nanos(f.now.as_nanos() + 99)] {
            assert!(
                f.engine
                    .publish_native_poll(f.owner, f.call, f.probe.lease, &grant, time)
                    .is_err()
            );
            assert_eq!(
                format!("{:?}", f.engine),
                before,
                "no tick or early timeout"
            );
        }
        assert!(
            f.engine
                .publish_native_poll(
                    f.owner,
                    NetworkStreamCallId(f.call.0 + 1),
                    f.probe.lease,
                    &grant,
                    later,
                )
                .is_err()
        );
        assert_eq!(format!("{:?}", f.engine), before);

        let mut f = record_fixture().await;
        let grant = f
            .scheduler
            .foreground_native_observation(f.owner, &f.root)
            .unwrap();
        complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
        f.engine
            .prepare_native_poll_wait(
                f.owner,
                f.call,
                f.probe.lease,
                &grant,
                f.now,
                (libc::POLLIN, 100),
            )
            .unwrap();
        complete_wait(
            &mut f.engine,
            f.owner,
            f.probe.lease,
            (libc::POLLIN, 100),
            libc::POLLIN,
        );
        complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
        let before = format!("{:?}", f.engine);
        assert!(
            f.engine
                .publish_native_poll(
                    f.owner,
                    f.call,
                    f.probe.lease,
                    &grant,
                    LogicalTime::from_nanos(f.now.as_nanos() + 101),
                )
                .is_err(),
            "a vanished nonzero wake cannot become a timeout"
        );
        assert_eq!(format!("{:?}", f.engine), before);
    }

    #[tokio::test]
    async fn raw_poll_wait_cannot_prepare_or_publish_under_replacement_normal_epoch() {
        for prepare_before_replacement in [false, true] {
            let mut f = record_fixture().await;
            complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
            let original_epoch = {
                let grant = f
                    .scheduler
                    .foreground_native_observation(f.owner, &f.root)
                    .unwrap();
                if prepare_before_replacement {
                    f.engine
                        .prepare_native_poll_wait(
                            f.owner,
                            f.call,
                            f.probe.lease,
                            &grant,
                            f.now,
                            (libc::POLLIN, 100),
                        )
                        .unwrap();
                    complete_wait(
                        &mut f.engine,
                        f.owner,
                        f.probe.lease,
                        (libc::POLLIN, 100),
                        0,
                    );
                    complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLIN);
                }
                grant.epoch()
            };
            f.scheduler.controlled_foreground_store_grant(&f.root);
            let grant = f
                .scheduler
                .foreground_native_observation(f.owner, &f.root)
                .unwrap();
            assert!(grant.epoch() > original_epoch);
            let before = format!("{:?}", f.engine);
            assert!(
                f.engine
                    .prepare_native_poll_wait(
                        f.owner,
                        f.call,
                        f.probe.lease,
                        &grant,
                        f.now,
                        (libc::POLLIN, 100),
                    )
                    .is_err()
            );
            assert!(
                f.engine
                    .publish_native_poll(
                        f.owner,
                        f.call,
                        f.probe.lease,
                        &grant,
                        LogicalTime::from_nanos(f.now.as_nanos() + 101),
                    )
                    .is_err()
            );
            assert_eq!(format!("{:?}", f.engine), before);
        }
    }

    #[tokio::test]
    async fn raw_poll_abort_known_spends_only_pre_wait_entry_without_publication() {
        let mut f = record_fixture().await;
        complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
        let trace = f.engine.native_trace_fixture();
        let before = format!("{:?}", f.engine);
        f.engine
            .check_native_poll_abort(f.owner, f.call, f.probe.lease)
            .unwrap();
        assert_eq!(format!("{:?}", f.engine), before);
        // The generic cursor/Peek cleanup is deliberately still inapplicable.
        assert!(
            f.engine
                .abort_shadow_probe_known(f.owner, f.probe.lease)
                .is_err()
        );
        assert_eq!(format!("{:?}", f.engine), before);
        f.engine
            .abort_native_poll_known(f.owner, f.call, f.probe.lease)
            .unwrap();
        assert!(
            f.engine.stream_calls[&f.call]
                .native_entry
                .as_ref()
                .unwrap()
                .used
        );
        assert!(!f.engine.shadow_probes.contains_key(&f.probe.lease));
        let file = f.engine.stream_call_open_file(f.owner, f.call).unwrap();
        assert!(!f.engine.socket_controls.contains_key(&file));
        assert_eq!(f.engine.native_trace_fixture(), trace);
        assert!(
            f.engine
                .abort_native_poll_known(f.owner, f.call, f.probe.lease)
                .is_err()
        );
        f.engine.begin_stream_call_release(f.owner, f.call).unwrap();
        f.engine
            .finish_stream_call_release(f.owner, f.call)
            .unwrap();
        assert!(!f.engine.stream_calls.contains_key(&f.call));
        assert_eq!(f.engine.native_trace_fixture(), trace);
    }

    #[tokio::test]
    async fn raw_poll_abort_refuses_unknown_mixed_and_every_prepared_wait_state() {
        for variant in 0..15 {
            let mut f = record_fixture().await;
            let mut owner = f.owner;
            let mut call = f.call;
            if variant != 0 && variant != 1 {
                complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
            }
            match variant {
                0 => {} // No completed initial observation.
                1 => f
                    .engine
                    .submit_shadow_probe_physical(
                        f.owner,
                        f.probe.lease,
                        NetworkStreamPhysicalEffect::PollState,
                    )
                    .unwrap(),
                2 => {
                    f.engine
                        .shadow_probes
                        .get_mut(&f.probe.lease)
                        .unwrap()
                        .cursor_observed = true
                }
                3 => f.engine.shadow_probes.get_mut(&f.probe.lease).unwrap().peek = Some(Ok(0)),
                4 => {
                    f.engine
                        .shadow_probes
                        .get_mut(&f.probe.lease)
                        .unwrap()
                        .queued = Some(0)
                }
                5 => {
                    f.engine
                        .shadow_probes
                        .get_mut(&f.probe.lease)
                        .unwrap()
                        .current_cursor = Some(0)
                }
                6 => {
                    f.engine.shadow_probes.get_mut(&f.probe.lease).unwrap().poll =
                        Some(libc::POLLNVAL)
                }
                7..=10 => {
                    let grant = f
                        .scheduler
                        .foreground_native_observation(f.owner, &f.root)
                        .unwrap();
                    f.engine
                        .prepare_native_poll_wait(
                            f.owner,
                            f.call,
                            f.probe.lease,
                            &grant,
                            f.now,
                            (libc::POLLIN, 100),
                        )
                        .unwrap();
                    if variant == 8 {
                        f.engine
                            .submit_shadow_probe_physical(
                                f.owner,
                                f.probe.lease,
                                NetworkStreamPhysicalEffect::PollWait {
                                    events: libc::POLLIN,
                                    timeout_ns: 100,
                                },
                            )
                            .unwrap();
                    } else if variant >= 9 {
                        complete_wait(
                            &mut f.engine,
                            f.owner,
                            f.probe.lease,
                            (libc::POLLIN, 100),
                            0,
                        );
                        if variant == 10 {
                            complete_poll(&mut f.engine, f.owner, f.probe.lease, libc::POLLOUT);
                        }
                    }
                }
                11 => call = NetworkStreamCallId(f.call.0 + 1),
                12 => owner.mm = owner.mm.for_exec(owner.thread),
                13 => f.engine.consume_native_entry(f.call),
                14 => {
                    f.engine.stream_calls.get_mut(&f.call).unwrap().phase =
                        StreamCallPhase::PinReleaseSubmitted
                }
                _ => unreachable!(),
            }
            let before = format!("{:?}", f.engine);
            assert!(
                f.engine
                    .check_native_poll_abort(owner, call, f.probe.lease)
                    .is_err(),
                "case {variant}"
            );
            assert!(
                f.engine
                    .abort_native_poll_known(owner, call, f.probe.lease)
                    .is_err(),
                "case {variant}"
            );
            assert_eq!(format!("{:?}", f.engine), before, "case {variant}");
        }
    }

    fn append(trace: &mut NetworkTraceV4, cut: u64, mask: i16, now: LogicalTime) {
        let ordinal = trace.inputs.len() as u64;
        let index = trace.release_model.nodes().len() as u64;
        let entry_cut = NetworkReceiveEntryCutV4(index);
        let prerequisites = trace.entry_frontier(entry_cut).unwrap();
        trace.inputs.push(NetworkInputEventV4 {
            ordinal,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV4 {
                not_before_global_time: now,
                receive_entry_cut: entry_cut,
                prerequisites: prerequisites.clone(),
            },
            event: NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: cut,
                revents: mask,
            },
        });
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(index),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites,
        });
    }

    fn bound(trace: NetworkTraceV4) -> (NetworkReplayEngine, OpenFileId) {
        let key = trace.fresh_stream_profiles[0].key;
        let mut engine = NetworkReplayEngine::replay_native_receive(trace).unwrap();
        let file = OpenFileId::new_socket(crate::types::DetTid::from_raw(61), 0);
        engine
            .register_stream_socket(
                file,
                key,
                NetworkStreamNamespace {
                    device: 7,
                    inode: 11,
                },
                None,
            )
            .unwrap();
        engine.bind(file, NetworkChannelId(1)).unwrap();
        (engine, file)
    }

    fn mask(engine: &NetworkReplayEngine, cut: u64) -> Result<i16, NetworkReplayError> {
        let EngineState::Native(native) = &engine.mode else {
            unreachable!()
        };
        native
            .replay
            .as_ref()
            .unwrap()
            .poll
            .at(NetworkChannelId(1), cut)
    }

    #[test]
    fn raw_poll_replay_observation_keeps_release_time_not_caller_now_or_poll_count() {
        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let now = trace.epoch_global_time().unwrap();
        let initial = LogicalTime::from_nanos(now.as_nanos() + 7);
        let final_time = LogicalTime::from_nanos(initial.as_nanos() + 100);
        append(&mut trace, 0, 0, initial);
        append(&mut trace, 0, libc::POLLIN, final_time);
        let (mut engine, file) = bound(trace);
        engine.release_eligible(now).unwrap();
        engine.take_connection_outcome(file).unwrap().unwrap();
        let thread = crate::types::DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, control).unwrap().id;
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        assert!(
            engine
                .replay_native_poll_observation(owner, call, now)
                .is_err()
        );
        for time in [initial, LogicalTime::from_nanos(final_time.as_nanos() - 1)] {
            assert_eq!(
                engine
                    .replay_native_poll_observation(owner, call, time)
                    .unwrap(),
                (initial, 0)
            );
            assert_eq!(engine.replay_native_poll(owner, call, time).unwrap(), 0);
            assert_eq!(engine.next_native_release_time().unwrap(), Some(final_time));
        }
        assert_eq!(
            engine
                .replay_native_poll_observation(owner, call, final_time)
                .unwrap(),
            (final_time, libc::POLLIN)
        );
        assert_eq!(
            engine
                .replay_native_poll_observation(
                    owner,
                    call,
                    LogicalTime::from_nanos(final_time.as_nanos() + 10),
                )
                .unwrap(),
            (final_time, libc::POLLIN)
        );
        engine.begin_stream_call_release(owner, call).unwrap();
        engine.finish_stream_call_release(owner, call).unwrap();
    }

    #[test]
    fn raw_poll_replay_respects_time_frontier_and_zero_without_future_output_readiness() {
        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let now = trace.epoch_global_time().unwrap();
        let later = LogicalTime::from_nanos(now.as_nanos() + 37);
        let cleared = LogicalTime::from_nanos(later.as_nanos() + 41);
        append(&mut trace, 0, libc::POLLPRI | libc::POLLRDHUP, later);
        append(&mut trace, 0, 0, cleared);
        trace.validate().unwrap();
        let (mut engine, file) = bound(trace);
        engine.release_eligible(later).unwrap();
        assert!(
            mask(&engine, 0).is_err(),
            "unconsumed Connect cannot establish frontier"
        );
        engine.take_connection_outcome(file).unwrap().unwrap();
        engine.release_eligible(now).unwrap();
        assert!(mask(&engine, 0).is_err(), "future time is still required");
        engine.release_eligible(later).unwrap();
        assert_eq!(mask(&engine, 0).unwrap(), libc::POLLPRI | libc::POLLRDHUP);
        engine.release_eligible(later).unwrap();
        assert_eq!(mask(&engine, 0).unwrap(), libc::POLLPRI | libc::POLLRDHUP);
        engine.release_eligible(cleared).unwrap();
        assert_eq!(
            mask(&engine, 0).unwrap(),
            0,
            "later ordinal replaces, not ORs"
        );
        // Explicit model-only expected-output premise: this is not transmission.
        let channel = engine.channels.get_mut(&NetworkChannelId(1)).unwrap();
        channel.append_expected_output(&NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"future".to_vec(),
        });
        channel.refresh_readiness();
        assert_eq!(mask(&engine, 0).unwrap(), 0);
        assert!(
            engine
                .native_completed()
                .unwrap()
                .contains(&NetworkReleaseNodeIdV4(5))
        );
    }

    #[test]
    fn raw_poll_rejects_equal_time_same_cut_including_identical_masks() {
        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let now = trace.epoch_global_time().unwrap();
        append(&mut trace, 0, libc::POLLPRI, now);
        trace.validate().unwrap();
        for revents in [0, libc::POLLPRI] {
            let mut ambiguous = trace.clone();
            append(&mut ambiguous, 0, revents, now);
            assert_eq!(
                ambiguous.validate(),
                Err(NetworkTraceValidationErrorV4::InvalidNativeObservation)
            );
            assert!(matches!(
                NetworkReplayEngine::replay_native_receive(ambiguous),
                Err(NetworkReplayError::FdPublicationProtocol(message))
                    if message == NetworkTraceValidationErrorV4::InvalidNativeObservation.to_string()
            ));
        }
        // Actual byte consumption is a distinct eligibility cut, not a poll
        // invocation counter or an invented increment of logical time.
        append(&mut trace, 8, 0, now);
        trace.validate().unwrap();
        let (mut engine, file) = bound(trace);
        engine.release_eligible(now).unwrap();
        engine.take_connection_outcome(file).unwrap().unwrap();
        engine.release_eligible(now).unwrap();
        assert_eq!(mask(&engine, 0).unwrap(), libc::POLLPRI);
        assert_eq!(
            engine.receive_stream(file, 8, false).unwrap(),
            StreamReceiveOutcome::Bytes(b"abcdefgh".to_vec())
        );
        engine.release_eligible(now).unwrap();
        assert_eq!(mask(&engine, 8).unwrap(), 0);
    }

    #[test]
    fn raw_poll_release_requires_consumed_bytes_and_rejects_stale_cut() {
        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let now = trace.epoch_global_time().unwrap();
        append(&mut trace, 8, libc::POLLIN | libc::POLLRDHUP, now);
        let (mut engine, file) = bound(trace);
        engine.release_eligible(now).unwrap();
        engine.take_connection_outcome(file).unwrap().unwrap();
        engine.release_eligible(now).unwrap();
        assert!(mask(&engine, 0).is_err());
        assert_eq!(
            engine.next_native_release_time().unwrap(),
            None,
            "a future byte cut is not a due timer"
        );
        assert_eq!(
            engine.receive_stream(file, 8, false).unwrap(),
            StreamReceiveOutcome::Bytes(b"abcdefgh".to_vec())
        );
        engine.release_eligible(now).unwrap();
        assert_eq!(mask(&engine, 8).unwrap(), libc::POLLIN | libc::POLLRDHUP);
        assert!(mask(&engine, 0).is_err());

        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let later = LogicalTime::from_nanos(now.as_nanos() + 37);
        append(&mut trace, 0, libc::POLLIN, later);
        let (mut engine, file) = bound(trace);
        engine.release_eligible(now).unwrap();
        engine.take_connection_outcome(file).unwrap().unwrap();
        engine.release_eligible(now).unwrap();
        assert_eq!(
            engine.receive_stream(file, 8, false).unwrap(),
            StreamReceiveOutcome::Bytes(b"abcdefgh".to_vec())
        );
        let before = format!("{engine:?}");
        assert!(engine.release_eligible(later).is_err());
        assert_eq!(
            format!("{engine:?}"),
            before,
            "stale readiness cannot partially publish"
        );
    }

    #[test]
    fn raw_poll_codec_keeps_v4_and_rejects_legacy_invalid_mask_or_cut() {
        let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
        let now = trace.epoch_global_time().unwrap();
        append(&mut trace, 0, libc::POLLHUP | libc::POLLERR, now);
        trace.validate().unwrap();
        let mut bytes = Vec::new();
        NetworkTrace::V4(trace.clone())
            .write_framed(&mut bytes)
            .unwrap();
        assert_eq!(
            NetworkTrace::read_framed(bytes.as_slice()).unwrap(),
            NetworkTrace::V4(trace.clone())
        );
        let legacy = NetworkTraceV2 {
            epoch: trace.epoch,
            channels: trace.channels.clone(),
            outputs: trace.outputs.clone(),
            inputs: trace
                .inputs
                .iter()
                .map(|input| NetworkInputEventV2 {
                    ordinal: input.ordinal,
                    channel: input.channel,
                    event: input.event.clone(),
                    release: NetworkReleaseV2 {
                        not_before_global_time: now,
                        after_transmitted_offset: 0,
                    },
                })
                .collect(),
        };
        assert!(legacy.validate().is_err());
        let legacy_v3 = detcore_model::network_trace::NetworkTraceV3 {
            history: legacy,
            receive_model: detcore_model::network_trace::ReceiveModelV1::DeclaredCopyUnitsV1 {
                units: vec![],
            },
            fresh_stream_profiles: trace.fresh_stream_profiles.clone(),
            receive_environment: trace.receive_environment,
            channel_socket_classes: trace.channel_socket_classes.clone(),
        };
        assert!(legacy_v3.validate().is_err());
        for event in [
            NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 9,
                revents: 0,
            },
            NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 0,
                revents: libc::POLLNVAL,
            },
        ] {
            let mut bad = trace.clone();
            bad.inputs.last_mut().unwrap().event = event;
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn raw_poll_replaces_zero_preserves_order_and_never_counts_poll_calls() {
        let channel = NetworkChannelId(1);
        let mut state = PollSnapshots::default();
        assert!(state.at(channel, 0).is_err());
        assert!(!state.completed(7));
        let first = LogicalTime::from_nanos(71);
        let second = LogicalTime::from_nanos(83);
        let third = LogicalTime::from_nanos(97);
        state.apply(7, channel, 0, first, libc::POLLIN | libc::POLLPRI);
        assert_eq!(state.at(channel, 0).unwrap(), libc::POLLIN | libc::POLLPRI);
        assert_eq!(state.at(channel, 0).unwrap(), libc::POLLIN | libc::POLLPRI);
        assert_eq!(
            state.observation_at(channel, 0).unwrap(),
            (first, libc::POLLIN | libc::POLLPRI)
        );
        assert!(state.completed(7));
        state.apply(8, channel, 0, second, 0);
        assert_eq!(state.at(channel, 0).unwrap(), 0);
        assert_eq!(state.observation_at(channel, 0).unwrap(), (second, 0));
        assert!(state.completed(7) && state.completed(8));
        assert!(state.at(channel, 1).is_err());
        state.apply(9, channel, 1, third, libc::POLLIN | libc::POLLRDHUP);
        assert!(state.at(channel, 0).is_err());
        assert_eq!(
            state.at(channel, 1).unwrap(),
            libc::POLLIN | libc::POLLRDHUP
        );
        assert!(state.at(NetworkChannelId(2), 1).is_err());
    }
}

#[cfg(test)]
#[path = "raw_poll/shared_provenance_tests.rs"]
mod shared_provenance_tests;

impl NetworkReplayEngine {
    /// The full immutable shared observation, including its original control
    /// provenance. A missing current cut is not a synthetic zero mask.
    pub(super) fn shared_poll_snapshot(
        &self, file: OpenFileId,
    ) -> Result<Option<SharedPollSnapshot>, NetworkReplayError> {
        let channel = self.bound_channel(file)?;
        let EngineState::Native(native) = &self.mode else { return Err(NetworkReplayError::WrongMode); };
        let replay = native.replay.as_ref().ok_or(NetworkReplayError::WrongMode)?;
        let queue = &self.channels[&channel];
        let low_water = self.shadow.as_ref().and_then(|s| s.sockets.get(&file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(file))?.options.receive_low_water;
        replay.poll.shared_observation_at(channel, queue.inbound_consumed,
            queue.local_control_generation, low_water)
    }
}
