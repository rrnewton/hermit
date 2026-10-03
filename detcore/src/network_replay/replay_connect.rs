//! One trace-authorized Connect continuation on an existing logical Call.
//! The scheduler authenticates the selected entry/continuation. These methods
//! neither submit a native syscall nor turn readiness into result delivery.

use super::*;
use crate::resources::ExternalOpId;

#[derive(Debug, Clone)]
pub(super) struct Claim {
    operation: ExternalOpId,
    channel: NetworkChannelId,
    ordinal: u64,
}

/// A read-only view of this Call's exact input, never a consumption receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplayConnectStatus {
    pub(crate) ready: bool,
    pub(crate) next_release: Option<LogicalTime>,
}

/// Issued from private V4 state, including actual completed producer nodes.
#[derive(Debug)]
pub(super) struct NativeInput {
    pub(super) ordinal: u64,
    pub(super) result: NetworkConnectionResultV2,
    pub(super) release: LogicalTime,
    pub(super) prerequisites_complete: bool,
    pub(super) released: bool,
    pub(super) delivered: bool,
}

fn protocol(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.into())
}

impl NetworkReplayEngine {
    pub(super) fn check_replay_connect_unclaimed(
        &self,
        open_file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        if let Some((id, _)) = self
            .stream_calls
            .iter()
            .find(|(_, state)| state.open_file == Some(open_file) && state.replay_connect.is_some())
        {
            return Err(NetworkReplayError::UnresolvedStreamCall(*id));
        }
        Ok(())
    }

    /// Transfer the actual selected reader into the existing logical Call.
    /// Every fallible claim predicate is checked before transferring custody.
    pub(crate) fn begin_replay_connect(
        &mut self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        read: NetworkFdReadAdmission,
        expected_open_file: OpenFileId,
    ) -> Result<NetworkStreamCall, NetworkReplayError> {
        self.check_native_retirement()?;
        self.validate_fd_read(owner, &read)?;
        let binding = read
            .binding
            .ok_or_else(|| protocol("Replay Connect has no selected socket"))?;
        if operation.tid != owner.thread
            || read.external_grant != Some(operation)
            || binding.open_file != expected_open_file
        {
            return Err(protocol("Replay Connect changed selected operation or OFD"));
        }
        self.check_replay_connect_unclaimed(expected_open_file)?;
        if self.stream_calls.values().any(|state| {
            state.open_file == Some(expected_open_file)
                || state
                    .replay_connect
                    .as_ref()
                    .is_some_and(|claim| claim.operation == operation)
        }) {
            return Err(protocol("Replay Connect already has Call custody"));
        }
        let channel = self.bound_channel(expected_open_file)?;
        let input = self.native_replay_connect_snapshot(channel)?;
        if input.delivered {
            return Err(protocol("Replay Connect outcome was already delivered"));
        }
        if input.released {
            self.check_replay_connect_front(channel, &input.result)?;
        }
        let control = read
            .control
            .ok_or_else(|| protocol("Replay Connect lost selected control"))?;
        // validate_fd_read already proves the exact unchanged control, no probe
        // and no submitted effect. Logical transfer releases the table permit.
        let call = self.begin_native_stream_call_from_read(owner, read)?;
        assert!(!call.physical_pin_required);
        self.finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .expect(
                "prevalidated unchanged Replay control cannot acquire an effect during transfer",
            );
        self.stream_calls
            .get_mut(&call.id)
            .expect("new logical Call")
            .replay_connect = Some(Claim {
            operation,
            channel,
            ordinal: input.ordinal,
        });
        Ok(call)
    }

    fn owned_replay_connect(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        call: NetworkStreamCallId,
    ) -> Result<(&StreamCallState, &Claim, NativeInput), NetworkReplayError> {
        self.check_stream_owner(owner)?;
        self.check_native_retirement()?;
        if !self.fd_table_capability() {
            return Err(protocol("Replay Connect lost complete FD-table authority"));
        }
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner || operation.tid != owner.thread {
            return Err(NetworkReplayError::StreamCallOwnerMismatch(call));
        }
        let claim = state
            .replay_connect
            .as_ref()
            .ok_or(NetworkReplayError::StreamCallPhaseMismatch(call))?;
        if claim.operation != operation
            || state.phase != StreamCallPhase::Active
            || state.physical_pin_required
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || state.original.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || state.replay_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.receive_policy.is_some()
        {
            return Err(NetworkReplayError::UnresolvedStreamCall(call));
        }
        let open_file = state
            .open_file
            .ok_or(NetworkReplayError::StreamCallPhaseMismatch(call))?;
        if self.bound_channel(open_file)? != claim.channel {
            return Err(protocol("Replay Connect changed its retained channel"));
        }
        let input = self.native_replay_connect_snapshot(claim.channel)?;
        if input.ordinal != claim.ordinal || input.delivered {
            return Err(protocol(
                "Replay Connect changed or consumed its claimed input",
            ));
        }
        Ok((state, claim, input))
    }

    fn check_replay_connect_front(
        &self,
        channel: NetworkChannelId,
        expected: &NetworkConnectionResultV2,
    ) -> Result<(), NetworkReplayError> {
        match self.runtime_channel(channel)?.inbound.front() {
            Some(InboundOutcome::Control(ConnectionOutcome::Connect(actual)))
                if actual == expected =>
            {
                Ok(())
            }
            _ => Err(protocol(
                "Replay Connect released input is not its exact front outcome",
            )),
        }
    }

    /// Maintenance may inspect the claim repeatedly, but cannot consume it.
    /// The global minimum release or another channel's readiness grants nothing.
    pub(crate) fn replay_connect_status(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        call: NetworkStreamCallId,
        now: LogicalTime,
    ) -> Result<ReplayConnectStatus, NetworkReplayError> {
        let (_, claim, input) = self.owned_replay_connect(owner, operation, call)?;
        if input.released {
            self.check_replay_connect_front(claim.channel, &input.result)?;
            if !input.prerequisites_complete || now < input.release {
                return Err(protocol(
                    "Replay Connect release precedes its actual frontier",
                ));
            }
        }
        Ok(ReplayConnectStatus {
            ready: input.released,
            next_release: (!input.released && input.prerequisites_complete)
                .then_some(input.release),
        })
    }

    /// Called only after Global authenticates this operation's selected Normal
    /// continuation. Consume the exact input and retire the logical reference
    /// in one engine critical section; no guest progress or native work occurs.
    pub(crate) fn complete_replay_connect(
        &mut self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        call: NetworkStreamCallId,
        now: LogicalTime,
    ) -> Result<NetworkConnectionResultV2, NetworkReplayError> {
        let (state, claim, input) = self.owned_replay_connect(owner, operation, call)?;
        let open_file = state.open_file.expect("validated logical Call");
        let channel = claim.channel;
        if !input.released || !input.prerequisites_complete || now < input.release {
            return Err(protocol(
                "Replay Connect continuation precedes its input release",
            ));
        }
        self.check_replay_connect_front(channel, &input.result)?;
        if self.socket_controls.contains_key(&open_file)
            || self
                .stream_operations
                .values()
                .any(|op| op.open_file == open_file)
            || self
                .zero_stream_waits
                .values()
                .any(|wait| wait.call == call)
        {
            return Err(NetworkReplayError::UnresolvedStreamCall(call));
        }
        // Lifetime acknowledgement performs all checks before its first
        // mutation. The Call still pins channel retirement during this step.
        self.release_stream_call_lifetime(owner, call, open_file)?;
        let Some(InboundOutcome::Control(outcome)) =
            self.channels.get_mut(&channel).unwrap().inbound.pop_front()
        else {
            unreachable!("exact front prevalidated under exclusive engine ownership")
        };
        self.channels.get_mut(&channel).unwrap().refresh_readiness();
        self.native_connection_delivered(channel, &outcome);
        self.stream_calls.remove(&call);
        self.complete_deferred_retirement(open_file);
        self.check_native_retirement()?;
        Ok(input.result)
    }
}

#[cfg(test)]
pub(crate) struct Fixture {
    pub(crate) engine: NetworkReplayEngine,
    pub(crate) owner: NetworkStreamOwner,
    pub(crate) binding: crate::types::FdSlotBinding,
    pub(crate) metadata: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
}

/// Controlled trace, table, namespace and selected-effect premises. This does
/// not execute native Connect or claim provider/foreground qualification.
#[cfg(test)]
fn fixture_trace(
    release: LogicalTime,
    asynchronous: bool,
) -> detcore_model::network_trace::NetworkTraceV4 {
    use chrono::TimeZone;
    use detcore_model::network_trace::*;
    let key = StreamSocketKeyV3 {
        transport: NetworkTransportV2::Tcp,
        domain: 2,
        socket_type: 1,
        protocol: 6,
    };
    let profile = FreshStreamSocketProfileV3 {
        key,
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
    let channel = NetworkChannelId(1);
    let mut inputs = vec![NetworkInputEventV4 {
        ordinal: 0,
        channel,
        release: NetworkReleaseV4 {
            not_before_global_time: release,
            receive_entry_cut: NetworkReceiveEntryCutV4(0),
            prerequisites: vec![],
        },
        event: NetworkInputKindV2::Connect(if asynchronous {
            NetworkConnectionResultV2::Error(libc::EINPROGRESS)
        } else {
            NetworkConnectionResultV2::Connected
        }),
    }];
    if asynchronous {
        inputs.push(NetworkInputEventV4 {
            ordinal: 1,
            event: NetworkInputKindV2::ConnectEstablished,
            ..inputs[0].clone()
        });
    }
    let mut nodes: Vec<_> = inputs
        .iter()
        .map(|input| NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(input.ordinal),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: input.ordinal,
            },
            prerequisites: vec![],
        })
        .collect();
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(inputs.len() as u64),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel,
            milestone: NetworkProgressV4::Established {
                source: NetworkEstablishmentV4::ConnectedInput {
                    input_ordinal: u64::from(asynchronous),
                },
            },
        },
        prerequisites: (0..inputs.len() as u64)
            .map(NetworkReleaseNodeIdV4)
            .collect(),
    });
    let trace = NetworkTraceV4 {
        epoch: Utc.timestamp_opt(0, 0).unwrap(),
        channels: vec![NetworkChannelV2 {
            id: channel,
            transport: NetworkTransportV2::Tcp,
            role: NetworkEndpointRoleV2::OutboundClient,
            local_address: None,
            peer_address: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 1],
                port: 443,
            }),
            accepted_from: None,
        }],
        inputs,
        outputs: vec![],
        creation_model: NetworkCreationModelV4::OutboundAndDatagramV1,
        release_model: NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes },
        native_receive_observations: vec![],
        fresh_stream_profiles: vec![profile],
        receive_environment: ReceiveEnvironmentV3::SingleRecorderNamespaceV1,
        channel_socket_classes: vec![ChannelSocketClassV3 { channel, key }],
        fresh_send_timeouts: vec![FreshSendTimeoutV1 {
            key,
            timeout: ReceiveTimeoutV3::Infinite,
        }],
    };
    trace.validate().unwrap();
    trace
}

#[cfg(test)]
pub(crate) fn fixture(release: LogicalTime, asynchronous: bool) -> Fixture {
    fixture_with_trace(fixture_trace(release, asynchronous))
}

/// Exact Close is a controlled component premise, not native backend evidence.
/// Retain the selected Call while exercising its final-OFD retirement later.
#[cfg(test)]
pub(crate) fn close_selected_binding_fixture(
    engine: &mut NetworkReplayEngine,
    owner: NetworkStreamOwner,
    binding: crate::types::FdSlotBinding,
    metadata: &std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
) {
    assert!(engine.stream_calls.values().any(|state| {
        state.owner == owner
            && state.open_file == Some(binding.open_file)
            && state.replay_connect.is_some()
    }));
    let retired = engine
        .lifetime
        .close_binding(
            TaskOwner {
                tid: owner.thread,
                mm: owner.mm,
            },
            binding,
        )
        .unwrap();
    assert!(retired.is_empty(), "Call still owns the original OFD");
    assert!(metadata.lock().unwrap().remove_descriptor_binding(binding));
}

#[cfg(test)]
fn fixture_with_trace(trace: detcore_model::network_trace::NetworkTraceV4) -> Fixture {
    use std::sync::Arc;
    use std::sync::Mutex;

    use crate::tool_local::FileMetadata;
    use crate::types::DetTid;
    use crate::types::MmId;
    let key = trace.fresh_stream_profiles[0].key;
    let channel = NetworkChannelId(1);
    let mut engine = NetworkReplayEngine::replay_native_receive(trace).unwrap();
    let thread = DetTid::from_raw(3);
    let owner = NetworkStreamOwner {
        thread,
        mm: MmId::initial(thread),
    };
    engine.fd_table_fixture_enable();
    let files = engine.fd_publication_fixture_register(owner, None);
    let metadata = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
    engine
        .associate_fd_metadata(owner, &metadata, &metadata.lock().unwrap())
        .unwrap();
    let (mut candidate, replacement) = metadata
        .lock()
        .unwrap()
        .prepare_original_installation(thread, 5, nix::fcntl::OFlag::O_RDWR, None)
        .unwrap();
    let binding = replacement.after.unwrap().binding;
    let publication = engine.acquire_fd_publication(owner, files).unwrap();
    let effect = engine.fd_publication_fixture_effect(owner, replacement);
    candidate
        .associate_network_installation(replacement.installation_generation, effect)
        .unwrap();
    let batch = candidate.publication_snapshot(&publication).unwrap();
    *metadata.lock().unwrap() = candidate;
    engine
        .publish_fd_publication(owner, publication.permit, &batch)
        .unwrap();
    metadata
        .lock()
        .unwrap()
        .publication_acknowledge(&batch)
        .unwrap();
    engine
        .acknowledge_fd_publication(owner, publication.permit, &batch)
        .unwrap();
    metadata
        .lock()
        .unwrap()
        .publication_server_acknowledge(&batch)
        .unwrap();
    engine
        .register_stream_socket(
            binding.open_file,
            key,
            NetworkStreamNamespace {
                device: 1,
                inode: 1,
            },
            None,
        )
        .unwrap();
    engine.bind(binding.open_file, channel).unwrap();
    Fixture {
        engine,
        owner,
        binding,
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(f: &mut Fixture, operation: ExternalOpId) -> NetworkFdReadAdmission {
        let NetworkFdReadBegin::Admitted(read) = f
            .engine
            .begin_fd_read(f.owner, f.binding.slot.files, f.binding.slot.fd)
            .unwrap()
        else {
            panic!("fixture publication was acknowledged");
        };
        // The scheduler issues this annotation in production; component tests
        // explicitly substitute that selection, not a native return.
        f.engine
            .bind_fd_read_external_grant(f.owner, *read, operation)
            .unwrap()
    }
    fn begin(f: &mut Fixture) -> (ExternalOpId, NetworkStreamCall) {
        let operation = ExternalOpId::new(f.owner.thread, 383);
        let read = selected(f, operation);
        let call = f
            .engine
            .begin_replay_connect(f.owner, operation, read, f.binding.open_file)
            .unwrap();
        (operation, call)
    }
    fn unchanged_error<T: std::fmt::Debug>(
        f: &mut Fixture,
        action: impl FnOnce(&mut Fixture) -> Result<T, NetworkReplayError>,
    ) {
        let before = format!("{:?}", f.engine);
        assert!(action(f).is_err());
        assert_eq!(format!("{:?}", f.engine), before);
    }

    #[test]
    fn replay_connect_consumes_only_after_exact_release_for_both_results() {
        for asynchronous in [false, true] {
            let release = LogicalTime::from_nanos(100);
            let mut f = fixture(release, asynchronous);
            let (operation, call) = begin(&mut f);
            assert!(!call.physical_pin_required);
            assert!(f.engine.socket_controls.is_empty());
            assert!(
                f.engine.fd_publications[&f.binding.slot.files]
                    .active
                    .is_none()
            );
            assert_eq!(f.engine.lifetime.counts(f.binding.open_file).transports, 1);
            let expected_pending = ReplayConnectStatus {
                ready: false,
                next_release: Some(release),
            };
            assert_eq!(
                f.engine
                    .replay_connect_status(f.owner, operation, call.id, release)
                    .unwrap(),
                expected_pending
            );
            f.engine
                .release_eligible(LogicalTime::from_nanos(99))
                .unwrap();
            unchanged_error(&mut f, |f| {
                f.engine.complete_replay_connect(
                    f.owner,
                    operation,
                    call.id,
                    LogicalTime::from_nanos(99),
                )
            });
            f.engine.release_eligible(release).unwrap();
            unchanged_error(&mut f, |f| {
                f.engine.replay_connect_status(
                    f.owner,
                    operation,
                    call.id,
                    LogicalTime::from_nanos(99),
                )
            });
            unchanged_error(&mut f, |f| {
                f.engine.complete_replay_connect(
                    f.owner,
                    operation,
                    call.id,
                    LogicalTime::from_nanos(99),
                )
            });
            assert_eq!(
                f.engine
                    .replay_connect_status(f.owner, operation, call.id, release)
                    .unwrap(),
                ReplayConnectStatus {
                    ready: true,
                    next_release: None
                }
            );
            assert!(
                !f.engine
                    .native_replay_connect_snapshot(NetworkChannelId(1))
                    .unwrap()
                    .delivered
            );
            let expected = if asynchronous {
                NetworkConnectionResultV2::Error(libc::EINPROGRESS)
            } else {
                NetworkConnectionResultV2::Connected
            };
            assert_eq!(
                f.engine
                    .complete_replay_connect(f.owner, operation, call.id, release)
                    .unwrap(),
                expected
            );
            assert!(f.engine.stream_calls.is_empty());
            assert_eq!(f.engine.lifetime.counts(f.binding.open_file).transports, 0);
            assert!(
                f.engine
                    .native_replay_connect_snapshot(NetworkChannelId(1))
                    .unwrap()
                    .delivered
            );
            unchanged_error(&mut f, |f| {
                f.engine
                    .complete_replay_connect(f.owner, operation, call.id, release)
            });
            let read = selected(&mut f, operation);
            unchanged_error(&mut f, |f| {
                f.engine
                    .begin_replay_connect(f.owner, operation, read.clone(), f.binding.open_file)
            });
            f.engine.finish_fd_read(f.owner, read).unwrap();
        }
    }

    #[test]
    fn replay_connect_can_claim_an_already_released_but_undelivered_outcome() {
        for asynchronous in [false, true] {
            let now = LogicalTime::from_nanos(10);
            let mut f = fixture(now, asynchronous);
            f.engine.release_eligible(now).unwrap();
            let (operation, call) = begin(&mut f);
            assert!(
                f.engine
                    .replay_connect_status(f.owner, operation, call.id, now)
                    .unwrap()
                    .ready
            );
            f.engine
                .complete_replay_connect(f.owner, operation, call.id, now)
                .unwrap();
        }
    }

    #[test]
    fn replay_connect_refuses_foreign_and_stale_admissions_without_transfer() {
        let mut f = fixture(LogicalTime::from_nanos(10), false);
        let operation = ExternalOpId::new(f.owner.thread, 383);
        let read = selected(&mut f, operation);
        let mut variants = vec![read.clone(); 4];
        variants[0].external_grant = None;
        variants[1].external_grant = Some(ExternalOpId::new(f.owner.thread, 384));
        variants[2].binding.as_mut().unwrap().generation += 1;
        variants[3].fd += 1;
        for bad in variants {
            unchanged_error(&mut f, |f| {
                f.engine
                    .begin_replay_connect(f.owner, operation, bad, f.binding.open_file)
            });
        }
        let wrong_owner = NetworkStreamOwner {
            mm: f.owner.mm.for_exec(f.owner.thread),
            ..f.owner
        };
        unchanged_error(&mut f, |f| {
            f.engine
                .begin_replay_connect(wrong_owner, operation, read.clone(), f.binding.open_file)
        });
        let wrong_ofd = OpenFileId::new_socket(f.owner.thread, 99);
        unchanged_error(&mut f, |f| {
            f.engine
                .begin_replay_connect(f.owner, operation, read.clone(), wrong_ofd)
        });
        let call = f
            .engine
            .begin_replay_connect(f.owner, operation, read.clone(), f.binding.open_file)
            .unwrap();
        unchanged_error(&mut f, |f| {
            f.engine
                .begin_replay_connect(f.owner, operation, read, f.binding.open_file)
        });
        for (owner, op, id) in [
            (wrong_owner, operation, call.id),
            (f.owner, ExternalOpId::new(f.owner.thread, 384), call.id),
            (f.owner, operation, NetworkStreamCallId(call.id.0 + 1)),
        ] {
            unchanged_error(&mut f, |f| {
                f.engine
                    .complete_replay_connect(owner, op, id, LogicalTime::from_nanos(10))
            });
        }
    }

    #[test]
    fn replay_connect_generic_consumers_and_call_release_cannot_steal_claim() {
        let now = LogicalTime::from_nanos(10);
        let mut f = fixture(now, true);
        let (operation, call) = begin(&mut f);
        f.engine.release_eligible(now).unwrap();
        unchanged_error(&mut f, |f| {
            f.engine.take_connection_outcome(f.binding.open_file)
        });
        unchanged_error(&mut f, |f| {
            f.engine.receive_stream(f.binding.open_file, 0, true)
        });
        unchanged_error(&mut f, |f| {
            f.engine
                .reserve_stream_chunk(f.owner, f.binding.open_file, 10, 0)
        });
        unchanged_error(&mut f, |f| {
            f.engine.transmit_stream(f.binding.open_file, b"x")
        });
        unchanged_error(&mut f, |f| f.engine.stream_call_open_file(f.owner, call.id));
        unchanged_error(&mut f, |f| {
            f.engine.begin_stream_call_release(f.owner, call.id)
        });
        unchanged_error(&mut f, |f| {
            f.engine.finish_stream_call_release(f.owner, call.id)
        });
        unchanged_error(&mut f, |f| {
            f.engine
                .cancel_replay_receive_admission(f.owner, call.id, NetworkStreamLeaseId(999))
        });
        assert!(f.engine.finish().is_err());
        f.engine
            .complete_replay_connect(f.owner, operation, call.id, now)
            .unwrap();
    }

    #[test]
    fn replay_connect_owner_loss_preserves_undelivered_claim_at_every_wait_cut() {
        for released in [false, true] {
            let now = LogicalTime::from_nanos(10);
            let mut f = fixture(now, true);
            let (operation, call) = begin(&mut f);
            if released {
                f.engine.release_eligible(now).unwrap();
            }
            f.engine.stream_owner_gone(f.owner);
            unchanged_error(&mut f, |f| {
                f.engine
                    .complete_replay_connect(f.owner, operation, call.id, now)
            });
            assert!(f.engine.stream_calls.contains_key(&call.id));
            assert!(
                !f.engine
                    .native_replay_connect_snapshot(NetworkChannelId(1))
                    .unwrap()
                    .delivered
            );
            assert!(f.engine.finish().is_err());
        }
    }

    #[test]
    fn replay_connect_requires_actual_prerequisite_consumption_not_another_channel_release() {
        use detcore_model::network_trace::*;
        let now = LogicalTime::from_nanos(100);
        let mut trace = fixture_trace(now, false);
        let first = NetworkChannelId(2);
        let mut preceding = trace.channels[0].clone();
        preceding.id = first;
        trace.channels.push(preceding);
        trace.channel_socket_classes.push(ChannelSocketClassV3 {
            channel: first,
            key: trace.fresh_stream_profiles[0].key,
        });
        trace.inputs[0].channel = first;
        trace.inputs.push(NetworkInputEventV4 {
            ordinal: 1,
            channel: NetworkChannelId(1),
            event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
            release: NetworkReleaseV4 {
                not_before_global_time: now,
                receive_entry_cut: NetworkReceiveEntryCutV4(2),
                prerequisites: vec![NetworkReleaseNodeIdV4(1)],
            },
        });
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
        let NetworkReleaseNodeKindV4::Progress { channel, .. } = &mut nodes[1].kind else {
            unreachable!()
        };
        *channel = first;
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(2),
            kind: NetworkReleaseNodeKindV4::Input { input_ordinal: 1 },
            prerequisites: vec![NetworkReleaseNodeIdV4(1)],
        });
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(3),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel: NetworkChannelId(1),
                milestone: NetworkProgressV4::Established {
                    source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 1 },
                },
            },
            prerequisites: vec![NetworkReleaseNodeIdV4(2)],
        });
        trace.validate().unwrap();
        let mut f = fixture_with_trace(trace);
        let other_file = OpenFileId::new_socket(f.owner.thread, 999);
        f.engine.bind(other_file, first).unwrap();
        let (operation, call) = begin(&mut f);
        f.engine.release_eligible(now).unwrap();
        assert!(f.engine.readiness(other_file).unwrap().readable);
        assert_eq!(
            f.engine
                .replay_connect_status(f.owner, operation, call.id, now)
                .unwrap(),
            ReplayConnectStatus {
                ready: false,
                next_release: None
            }
        );
        unchanged_error(&mut f, |f| {
            f.engine
                .complete_replay_connect(f.owner, operation, call.id, now)
        });
        assert!(matches!(
            f.engine.take_connection_outcome(other_file).unwrap(),
            Some(ConnectionOutcome::Connect(
                NetworkConnectionResultV2::Connected
            ))
        ));
        assert_eq!(
            f.engine
                .replay_connect_status(f.owner, operation, call.id, now)
                .unwrap(),
            ReplayConnectStatus {
                ready: false,
                next_release: Some(now)
            }
        );
        f.engine.release_eligible(now).unwrap();
        f.engine
            .complete_replay_connect(f.owner, operation, call.id, now)
            .unwrap();
    }

    #[test]
    fn replay_connect_released_claim_rejects_wrong_phase_ordinal_channel_front_and_lost_capability()
    {
        for defect in 0..5 {
            let now = LogicalTime::from_nanos(10);
            let mut f = fixture(now, true);
            let (operation, call) = begin(&mut f);
            f.engine.release_eligible(now).unwrap();
            match defect {
                0 => {
                    f.engine.stream_calls.get_mut(&call.id).unwrap().phase =
                        StreamCallPhase::PinReleaseSubmitted
                }
                1 => {
                    f.engine
                        .stream_calls
                        .get_mut(&call.id)
                        .unwrap()
                        .replay_connect
                        .as_mut()
                        .unwrap()
                        .ordinal += 1
                }
                2 => {
                    f.engine
                        .stream_calls
                        .get_mut(&call.id)
                        .unwrap()
                        .replay_connect
                        .as_mut()
                        .unwrap()
                        .channel = NetworkChannelId(99)
                }
                3 => {
                    *f.engine
                        .channels
                        .get_mut(&NetworkChannelId(1))
                        .unwrap()
                        .inbound
                        .front_mut()
                        .unwrap() = InboundOutcome::Control(ConnectionOutcome::Connect(
                        NetworkConnectionResultV2::Connected,
                    ))
                }
                4 => f.engine.fd_lifecycle = Default::default(),
                _ => unreachable!(),
            }
            unchanged_error(&mut f, |f| {
                f.engine
                    .replay_connect_status(f.owner, operation, call.id, now)
            });
            unchanged_error(&mut f, |f| {
                f.engine
                    .complete_replay_connect(f.owner, operation, call.id, now)
            });
            assert!(f.engine.stream_calls.contains_key(&call.id));
            assert!(
                !f.engine
                    .native_replay_connect_snapshot(NetworkChannelId(1))
                    .unwrap()
                    .delivered
            );
        }
    }

    #[test]
    fn replay_connect_selected_call_keeps_original_ofd_when_numeric_fd_is_replaced() {
        let now = LogicalTime::from_nanos(10);
        let mut f = fixture(now, true);
        let (operation, call) = begin(&mut f);
        let original = f.binding;
        // Exact Close is an explicit component premise, not native evidence.
        // The existing lifetime preserves the Call while removing the slot.
        let retired = f
            .engine
            .lifetime
            .close_binding(
                TaskOwner {
                    tid: f.owner.thread,
                    mm: f.owner.mm,
                },
                original,
            )
            .unwrap();
        assert!(retired.is_empty(), "Call still owns the original OFD");
        assert!(
            f.metadata
                .lock()
                .unwrap()
                .remove_descriptor_binding(original)
        );
        // Fresh publication/ACK then exercises the existing real metadata and
        // lifetime APIs, not mutation of the claimed Call's identity.
        let (mut candidate, replacement) = f
            .metadata
            .lock()
            .unwrap()
            .prepare_original_installation(
                f.owner.thread,
                original.slot.fd,
                nix::fcntl::OFlag::O_RDWR,
                None,
            )
            .unwrap();
        let new_binding = replacement.after.unwrap().binding;
        assert_ne!(new_binding.open_file, original.open_file);
        let publication = f
            .engine
            .acquire_fd_publication(f.owner, original.slot.files)
            .unwrap();
        let effect = f.engine.fd_publication_fixture_effect(f.owner, replacement);
        candidate
            .associate_network_installation(replacement.installation_generation, effect)
            .unwrap();
        let batch = candidate.publication_snapshot(&publication).unwrap();
        *f.metadata.lock().unwrap() = candidate;
        f.engine
            .publish_fd_publication(f.owner, publication.permit, &batch)
            .unwrap();
        f.metadata
            .lock()
            .unwrap()
            .publication_acknowledge(&batch)
            .unwrap();
        f.engine
            .acknowledge_fd_publication(f.owner, publication.permit, &batch)
            .unwrap();
        f.metadata
            .lock()
            .unwrap()
            .publication_server_acknowledge(&batch)
            .unwrap();
        assert_eq!(
            f.engine.stream_calls[&call.id].open_file,
            Some(original.open_file)
        );
        assert_eq!(f.engine.lifetime.counts(original.open_file).transports, 1);
        assert_eq!(f.engine.lifetime.counts(original.open_file).slots, 0);
        assert_eq!(
            f.engine
                .lifetime
                .descriptor_binding(
                    TaskOwner {
                        tid: f.owner.thread,
                        mm: f.owner.mm
                    },
                    original.slot.fd
                )
                .unwrap(),
            new_binding
        );
        f.engine.release_eligible(now).unwrap();
        f.engine
            .complete_replay_connect(f.owner, operation, call.id, now)
            .unwrap();
        assert_eq!(f.engine.lifetime.counts(original.open_file).transports, 0);
        assert_eq!(
            f.engine
                .lifetime
                .descriptor_binding(
                    TaskOwner {
                        tid: f.owner.thread,
                        mm: f.owner.mm
                    },
                    original.slot.fd
                )
                .unwrap(),
            new_binding
        );
        assert_eq!(f.engine.channel_for(new_binding.open_file), None);
    }

    #[test]
    fn replay_connect_missing_duplicate_or_unsupported_trace_never_becomes_a_claim() {
        use detcore_model::network_trace::*;
        let now = LogicalTime::from_nanos(10);
        let mut missing = fixture_trace(now, false);
        missing.inputs.clear();
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut missing.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes.clear();
        missing.validate().unwrap();
        let mut f = fixture_with_trace(missing);
        let operation = ExternalOpId::new(f.owner.thread, 383);
        let read = selected(&mut f, operation);
        unchanged_error(&mut f, |f| {
            f.engine
                .begin_replay_connect(f.owner, operation, read.clone(), f.binding.open_file)
        });
        f.engine.finish_fd_read(f.owner, read).unwrap();
        let mut duplicate = fixture_trace(now, false);
        let mut second = duplicate.inputs[0].clone();
        second.ordinal = 1;
        duplicate.inputs.push(second);
        assert!(duplicate.validate().is_err());
        assert!(NetworkReplayEngine::replay_native_receive(duplicate).is_err());
        let mut unsupported = fixture_trace(now, false);
        unsupported.inputs[0].event =
            NetworkInputKindV2::Connect(NetworkConnectionResultV2::Error(libc::ECONNREFUSED));
        assert!(NetworkReplayEngine::replay_native_receive(unsupported).is_err());
    }

    #[test]
    fn replay_connect_claim_blocks_direct_socket_error_before_any_release() {
        let now = LogicalTime::from_nanos(10);
        let mut f = fixture(now, true);
        let (_, call) = begin(&mut f);
        let control = f
            .engine
            .begin_socket_controls(f.owner, vec![f.binding.open_file])
            .unwrap()[0]
            .1;
        assert!(
            !f.engine
                .native_replay_connect_snapshot(NetworkChannelId(1))
                .unwrap()
                .released
        );
        unchanged_error(&mut f, |f| {
            f.engine.take_replay_socket_error(f.owner, control, now)
        });
        unchanged_error(&mut f, |f| {
            f.engine
                .replay_native_poll_observation(f.owner, call.id, now)
        });
        assert!(
            !f.engine
                .native_replay_connect_snapshot(NetworkChannelId(1))
                .unwrap()
                .released
        );
        f.engine
            .finish_socket_control(f.owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }
}
