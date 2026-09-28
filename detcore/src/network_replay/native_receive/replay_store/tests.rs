use chrono::TimeZone;
use chrono::Utc;
use detcore_model::futex::MmId;
use detcore_model::network_trace::*;

use super::*;

fn profile() -> FreshStreamSocketProfileV3 {
    FreshStreamSocketProfileV3 {
        key: StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: 2,
            socket_type: 1,
            protocol: 6,
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
    }
}
fn empty() -> NetworkTraceV4 {
    NetworkTraceV4 {
        epoch: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
        channels: vec![],
        inputs: vec![],
        outputs: vec![],
        creation_model: NetworkCreationModelV4::OutboundAndDatagramV1,
        release_model: NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes: vec![] },
        native_receive_observations: vec![],
        fresh_stream_profiles: vec![],
        receive_environment: ReceiveEnvironmentV3::SingleRecorderNamespaceV1,
        channel_socket_classes: vec![],
        fresh_send_timeouts: vec![],
    }
}
fn nodes(t: &mut NetworkTraceV4) -> &mut Vec<NetworkReleaseNodeV4> {
    match &mut t.release_model {
        NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } => nodes,
    }
}
fn add_channel(t: &mut NetworkTraceV4, id: u64, datagram: bool) {
    t.channels.push(NetworkChannelV2 {
        id: NetworkChannelId(id),
        transport: if datagram {
            NetworkTransportV2::Udp
        } else {
            NetworkTransportV2::Tcp
        },
        role: if datagram {
            NetworkEndpointRoleV2::Datagram
        } else {
            NetworkEndpointRoleV2::OutboundClient
        },
        local_address: None,
        peer_address: Some(NetworkAddressV2::Inet4 {
            address: [192, 0, 2, id as u8],
            port: 443,
        }),
        accepted_from: None,
    });
    if !datagram {
        if t.fresh_stream_profiles.is_empty() {
            t.fresh_stream_profiles.push(profile());
            t.fresh_send_timeouts.push(FreshSendTimeoutV1 {
                key: profile().key,
                timeout: ReceiveTimeoutV3::Infinite,
            });
        }
        t.channel_socket_classes.push(ChannelSocketClassV3 {
            channel: NetworkChannelId(id),
            key: profile().key,
        });
    }
}
fn progress(
    t: &mut NetworkTraceV4,
    channel: u64,
    milestone: NetworkProgressV4,
    dependencies: &[u64],
) -> u64 {
    let n = nodes(t).len() as u64;
    nodes(t).push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(n),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel: NetworkChannelId(channel),
            milestone,
        },
        prerequisites: dependencies
            .iter()
            .copied()
            .map(NetworkReleaseNodeIdV4)
            .collect(),
    });
    n
}
fn input(
    t: &mut NetworkTraceV4,
    channel: u64,
    event: NetworkInputKindV2,
    dependencies: &[u64],
) -> u64 {
    let ordinal = t.inputs.len() as u64;
    let n = nodes(t).len() as u64;
    let prerequisites: Vec<_> = dependencies
        .iter()
        .copied()
        .map(NetworkReleaseNodeIdV4)
        .collect();
    t.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: NetworkChannelId(channel),
        event,
        release: NetworkReleaseV4 {
            not_before_global_time: t.epoch_global_time().unwrap(),
            receive_entry_cut: NetworkReceiveEntryCutV4(n),
            prerequisites: prerequisites.clone(),
        },
    });
    nodes(t).push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(n),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    n
}
fn observation(channel: u64, length: u64) -> NetworkNativeReceiveObservationV4 {
    NetworkNativeReceiveObservationV4 {
        ordinal: 0,
        channel: NetworkChannelId(channel),
        stream_offset: 0,
        length,
        fragments: vec![NetworkNativeCopyFragmentV4 {
            stream_offset: 0,
            requested: length,
            copied: length,
            available: 32,
            source_offset: 0,
            storage_length: 32,
            nonlinear_length: 0,
            disposition: NetworkNativeCopyDispositionV4::Consume,
            physical_before: 0,
            physical_after: length,
        }],
    }
}

fn fixture() -> (NetworkReplayEngine, NetworkStreamOwner, NetworkStreamCallId) {
    fixture_with_eof(false)
}

fn fixture_with_eof(eof: bool) -> (NetworkReplayEngine, NetworkStreamOwner, NetworkStreamCallId) {
    let mut trace = empty();
    add_channel(&mut trace, 1, false);
    input(
        &mut trace,
        1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[],
    );
    progress(
        &mut trace,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[],
    );
    input(
        &mut trace,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"ab".to_vec(),
        },
        &[1],
    );
    input(
        &mut trace,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 2,
            bytes: b"cdefgh".to_vec(),
        },
        &[1],
    );
    trace.native_receive_observations.push(observation(1, 8));
    if eof {
        input(
            &mut trace,
            1,
            NetworkInputKindV2::PeerShutdown {
                stream_offset: 8,
                direction: NetworkShutdownV2::Write,
            },
            &[1],
        );
    }
    trace.validate().unwrap();
    let thread = crate::types::DetTid::from_raw(7);
    let owner = NetworkStreamOwner {
        thread,
        mm: MmId::initial(thread),
    };
    let (engine, call) = NetworkReplayEngine::controlled_replay_store_call(owner, trace);
    (engine, owner, call)
}

#[test]
fn replay_store_selects_across_trace_fragments_without_native_receipts() {
    let (mut engine, owner, call) = fixture();
    let source = engine
        .reserve_replay_store_source(owner, call, 5)
        .unwrap()
        .unwrap();
    assert_eq!(source.bytes(), b"abcde");
    assert_eq!(engine.check_replay_store_source(&source).unwrap(), call);
    assert!(engine.stream_calls[&call].helper_copy.is_none());
    assert!(engine.stream_calls[&call].native_receive.is_empty());
    assert!(!engine.stream_calls[&call].physical_pin_required);
    let before = format!("{engine:?}");
    assert!(
        engine
            .finish_stream_chunk(
                owner,
                source.lease(),
                NetworkStreamChunkDisposition::Consumed
            )
            .is_err()
    );
    assert!(engine.begin_stream_call_release(owner, call).is_err());
    assert!(engine.reserve_replay_store_source(owner, call, 5).is_err());
    assert_eq!(format!("{engine:?}"), before);
}

#[test]
fn replay_store_refuses_unqualified_profiles_before_selection_or_reservation() {
    // These are model-admission controls, not Linux SO_RCVLOWAT/timeout
    // qualification. In particular, queued EOF must not turn an unsupported
    // partial-receive policy into a successful or indefinitely waiting call.
    for (low_water, timeout) in [
        (9, ReceiveTimeoutV3::Infinite),
        (1, ReceiveTimeoutV3::FiniteTicks(5_000)),
        (9, ReceiveTimeoutV3::FiniteTicks(5_000)),
    ] {
        for eof in [false, true] {
            for nonblocking in [false, true] {
                for maximum in [1, 16] {
                    let (mut engine, owner, call) = fixture_with_eof(eof);
                    let open_file = engine.stream_calls[&call].open_file.unwrap();
                    let options = &mut engine
                        .shadow
                        .as_mut()
                        .unwrap()
                        .sockets
                        .get_mut(&open_file)
                        .unwrap()
                        .options;
                    options.receive_low_water = low_water;
                    options.receive_timeout = timeout;
                    let before = format!("{engine:?}");
                    let error = engine
                        .plan_replay_receive(owner, call, maximum, nonblocking)
                        .expect_err(
                            "unqualified profile must refuse before selecting bytes or wait",
                        );
                    assert!(
                        error
                            .to_string()
                            .contains("qualified low-water/timeout profile")
                    );
                    assert_eq!(format!("{engine:?}"), before);
                    let error = engine
                        .reserve_replay_store_source(owner, call, maximum)
                        .expect_err("direct reservation must enforce the same profile");
                    assert!(
                        error
                            .to_string()
                            .contains("qualified low-water/timeout profile")
                    );
                    assert_eq!(format!("{engine:?}"), before);
                }
            }
        }
    }
}

#[test]
fn replay_store_rechecks_profile_after_selection_without_consuming() {
    for timeout in [false, true] {
        let (mut engine, owner, call) = fixture_with_eof(true);
        let open_file = engine.stream_calls[&call].open_file.unwrap();
        let ReplayReceivePlan::Bytes(plan) =
            engine.plan_replay_receive(owner, call, 16, false).unwrap()
        else {
            panic!("qualified profile must select the complete prefix before EOF");
        };
        assert_eq!(plan.bytes, b"abcdefgh");
        let options = &mut engine
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&open_file)
            .unwrap()
            .options;
        if timeout {
            options.receive_timeout = ReceiveTimeoutV3::FiniteTicks(5_000);
        } else {
            options.receive_low_water = 9;
        }
        let before = format!("{engine:?}");
        let error = engine.reserve_replay_planned_store(&plan).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("qualified low-water/timeout profile")
        );
        assert_eq!(format!("{engine:?}"), before);
    }
}

#[test]
fn replay_store_refuses_foreign_or_changed_source_without_consuming() {
    for variant in 0..6 {
        let (mut engine, owner, call) = fixture();
        let source = engine
            .reserve_replay_store_source(owner, call, 8)
            .unwrap()
            .unwrap();
        match variant {
            0 => engine.stream_calls.get_mut(&call).unwrap().abandoned = true,
            1 => engine.stream_calls.get_mut(&call).unwrap().final_wait = true,
            2 => {
                engine
                    .channels
                    .get_mut(&source.channel)
                    .unwrap()
                    .inbound_consumed += 1
            }
            3 => {
                engine
                    .stream_operations
                    .get_mut(&source.lease)
                    .unwrap()
                    .abandoned = true
            }
            4 => {
                engine
                    .shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&source.open_file)
                    .unwrap()
                    .consume_epoch += 1
            }
            5 => {
                let (other, _, _) = fixture();
                engine = other;
            }
            _ => unreachable!(),
        }
        let before = format!("{engine:?}");
        assert!(
            engine.check_replay_store_source(&source).is_err(),
            "variant {variant}"
        );
        assert_eq!(format!("{engine:?}"), before);
    }
}

#[test]
fn replay_store_empty_and_unsupported_bounds_cannot_reserve_or_finish() {
    for maximum in [0, 513, usize::MAX] {
        let (mut engine, owner, call) = fixture();
        let before = format!("{engine:?}");
        assert!(
            engine
                .reserve_replay_store_source(owner, call, maximum)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    let (mut engine, owner, call) = fixture();
    let channel = engine.stream_calls[&call]
        .open_file
        .and_then(|file| engine.channel_for(file))
        .unwrap();
    engine.channels.get_mut(&channel).unwrap().inbound.clear();
    let before = format!("{engine:?}");
    assert!(
        engine
            .reserve_replay_store_source(owner, call, 4)
            .unwrap()
            .is_none()
    );
    assert_eq!(format!("{engine:?}"), before);
}

pub(super) fn two_row_trace() -> NetworkTraceV4 {
    let mut trace = empty();
    add_channel(&mut trace, 1, false);
    input(
        &mut trace,
        1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[],
    );
    progress(
        &mut trace,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[],
    );
    input(
        &mut trace,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"ab".to_vec(),
        },
        &[1],
    );
    input(
        &mut trace,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 2,
            bytes: b"cdefgh".to_vec(),
        },
        &[1],
    );
    trace.native_receive_observations.push(observation(1, 8));
    trace.validate().unwrap();
    trace
}
