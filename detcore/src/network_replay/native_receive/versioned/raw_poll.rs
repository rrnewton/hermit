//! V4 nonconsuming readiness snapshots, never V3 Peek or expected-output state.
use detcore_model::network_trace::valid_tcp_poll_mask;

use super::*;

#[derive(Debug, Default)]
pub(super) struct PollSnapshots {
    latest: BTreeMap<NetworkChannelId, (u64, i16)>,
    applied: BTreeSet<u64>,
}

impl PollSnapshots {
    pub(super) fn apply(
        &mut self,
        ordinal: u64,
        channel: NetworkChannelId,
        consumed_prefix: u64,
        revents: i16,
    ) {
        self.latest.insert(channel, (consumed_prefix, revents));
        self.applied.insert(ordinal);
    }

    pub(super) fn completed(&self, ordinal: u64) -> bool {
        self.applied.contains(&ordinal)
    }

    fn at(&self, channel: NetworkChannelId, consumed: u64) -> Result<i16, NetworkReplayError> {
        match self.latest.get(&channel) {
            Some(&(cut, mask)) if cut == consumed => Ok(mask),
            _ => Err(invalid(
                "V4 poll has no eligible observation at its consumed-byte cut",
            )),
        }
    }
}

impl NetworkReplayEngine {
    /// The existing physical Pending and actual foreground entry have already
    /// joined. Publish the whole raw mask, including zero, as one transaction.
    pub(crate) fn publish_native_poll(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
    ) -> Result<i16, NetworkReplayError> {
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
        let consumed_prefix = self.channels[&probe.channel].inbound_consumed;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let ordinal =
            u64::try_from(native.trace.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let node = u64::try_from(native.trace.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let mut candidate = native.trace.clone();
        let input = NetworkInputEventV4 {
            ordinal,
            channel: probe.channel,
            release: release.clone(),
            event: NetworkInputKindV2::RawTcpPollState {
                consumed_prefix,
                revents,
            },
        };
        candidate.inputs.push(input.clone());
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut candidate.release_model;
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(node),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
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
        candidate.validate().map_err(|e| invalid(&e.to_string()))?;
        // Nothing fallible follows semantic publication.
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native.trace.inputs.push(input);
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model;
        nodes.push(node);
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
            .at(channel, self.channels[&channel].inbound_consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn raw_poll_record_publishes_only_completed_same_call_at_original_entry() {
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
            &mut trace.release_model;
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
        state.apply(7, channel, 0, libc::POLLIN | libc::POLLPRI);
        assert_eq!(state.at(channel, 0).unwrap(), libc::POLLIN | libc::POLLPRI);
        assert_eq!(state.at(channel, 0).unwrap(), libc::POLLIN | libc::POLLPRI);
        assert!(state.completed(7));
        state.apply(8, channel, 0, 0);
        assert_eq!(state.at(channel, 0).unwrap(), 0);
        assert!(state.completed(7) && state.completed(8));
        assert!(state.at(channel, 1).is_err());
        state.apply(9, channel, 1, libc::POLLIN | libc::POLLRDHUP);
        assert!(state.at(channel, 0).is_err());
        assert_eq!(
            state.at(channel, 1).unwrap(),
            libc::POLLIN | libc::POLLRDHUP
        );
        assert!(state.at(NetworkChannelId(2), 1).is_err());
    }
}
