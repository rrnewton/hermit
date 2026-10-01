use chrono::TimeZone;
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
/// Two independently established channels; a prefix of A's expected write
/// precedes B's reply, and A's remaining bytes explicitly follow its delivery.
fn cross_channel() -> NetworkTraceV4 {
    let mut t = empty();
    add_channel(&mut t, 1, false);
    add_channel(&mut t, 2, false);
    input(
        &mut t,
        1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[],
    );
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[],
    );
    input(
        &mut t,
        2,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[1],
    );
    progress(
        &mut t,
        2,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 1 },
        },
        &[],
    );
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"0123456789012345678901234".to_vec(),
        },
    });
    progress(
        &mut t,
        1,
        NetworkProgressV4::StreamPrefix {
            exclusive_offset: 12,
        },
        &[],
    );
    input(
        &mut t,
        2,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"reply".to_vec(),
        },
        &[1, 3, 4],
    );
    progress(
        &mut t,
        1,
        NetworkProgressV4::StreamPrefix {
            exclusive_offset: 25,
        },
        &[5],
    );
    t.native_receive_observations.push(observation(2, 5));
    t
}
fn datagram(sequence: u64, bytes: &[u8]) -> NetworkDatagramV2 {
    NetworkDatagramV2 {
        sequence,
        bytes: bytes.to_vec(),
        source: None,
        destination: None,
        ancillary: None,
        message_flags: 0,
    }
}

fn file(id: u64) -> OpenFileId {
    OpenFileId::new_socket(crate::types::DetTid::from_raw(61), id)
}
fn bind(engine: &mut NetworkReplayEngine, id: u64) -> OpenFileId {
    let ofd = file(id);
    if engine.channels[&NetworkChannelId(id)].transport == NetworkTransportV2::Tcp {
        engine
            .register_stream_socket(
                ofd,
                profile().key,
                NetworkStreamNamespace {
                    device: 7,
                    inode: 11,
                },
                None,
            )
            .unwrap();
    }
    engine.bind(ofd, NetworkChannelId(id)).unwrap();
    ofd
}
fn connected(engine: &mut NetworkReplayEngine, ofd: OpenFileId, now: LogicalTime) {
    engine.release_eligible(now).unwrap();
    assert_eq!(
        engine.take_connection_outcome(ofd).unwrap(),
        Some(ConnectionOutcome::Connect(
            NetworkConnectionResultV2::Connected
        ))
    );
}
fn one_stream() -> NetworkTraceV4 {
    let mut t = empty();
    add_channel(&mut t, 1, false);
    input(
        &mut t,
        1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[],
    );
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[0],
    );
    input(
        &mut t,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"ab".to_vec(),
        },
        &[1],
    );
    input(
        &mut t,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 2,
            bytes: b"cd".to_vec(),
        },
        &[1],
    );
    t.native_receive_observations.push(observation(1, 4));
    t.validate().unwrap();
    t
}
#[test]
fn native_replay_availability_is_not_delivery_or_establishment() {
    let t = one_stream();
    let now = t.epoch_global_time().unwrap();
    let mut e = NetworkReplayEngine::replay_native_receive(t).unwrap();
    let ofd = bind(&mut e, 1);
    assert_eq!(
        e.release_eligible(now).unwrap(),
        BTreeSet::from([NetworkChannelId(1)])
    );
    assert!(e.native_completed().unwrap().is_empty());
    assert!(e.release_eligible(now).unwrap().is_empty());
    assert_eq!(
        e.receive_stream(ofd, 4, false).unwrap(),
        StreamReceiveOutcome::Pending
    );
    assert!(e.finish().is_err());
    connected(&mut e, ofd, now);
    e.release_eligible(now).unwrap();
    assert_eq!(
        e.native_completed().unwrap(),
        BTreeSet::from([NetworkReleaseNodeIdV4(0), NetworkReleaseNodeIdV4(1)])
    );
    assert_eq!(
        e.receive_stream(ofd, 4, false).unwrap(),
        StreamReceiveOutcome::Bytes(b"abcd".to_vec())
    );
    assert_eq!(e.native_completed().unwrap().len(), 4);
    e.finish().unwrap();
}
#[test]
fn native_replay_changed_read_partition_preserves_bytes_and_time_floor() {
    for sizes in [vec![4], vec![1, 1, 2], vec![3, 1]] {
        let mut t = one_stream();
        let now = t.epoch_global_time().unwrap();
        let later = LogicalTime::from_nanos(now.as_nanos() + 37);
        for row in &mut t.inputs[1..] {
            row.release.not_before_global_time = later;
        }
        let mut e = NetworkReplayEngine::replay_native_receive(t).unwrap();
        let ofd = bind(&mut e, 1);
        connected(&mut e, ofd, now);
        assert!(e.release_eligible(now).unwrap().is_empty());
        assert_eq!(e.next_release_time().unwrap(), Some(later));
        assert_eq!(
            e.receive_stream(ofd, 4, true).unwrap(),
            StreamReceiveOutcome::WouldBlock
        );
        e.release_eligible(LogicalTime::from_nanos(later.as_nanos() + 211))
            .unwrap();
        let mut actual = Vec::new();
        for size in sizes {
            let StreamReceiveOutcome::Bytes(bytes) = e.receive_stream(ofd, size, false).unwrap()
            else {
                panic!("plain bytes")
            };
            actual.extend(bytes);
        }
        assert_eq!(actual, b"abcd");
        e.finish().unwrap();
    }
}
#[test]
fn native_replay_cross_channel_prefix_requires_actual_matching_output() {
    let t = cross_channel();
    t.validate().unwrap();
    let now = t.epoch_global_time().unwrap();
    let mut e = NetworkReplayEngine::replay_native_receive(t).unwrap();
    let a = bind(&mut e, 1);
    let b = bind(&mut e, 2);
    connected(&mut e, a, now);
    connected(&mut e, b, now);
    assert!(e.release_eligible(now).unwrap().is_empty());
    let before = format!("{e:?}");
    assert!(matches!(
        e.transmit_stream(a, b"wrong"),
        Err(NetworkReplayError::OutboundMismatch { .. })
    ));
    assert_eq!(format!("{e:?}"), before);
    assert_eq!(
        e.transmit_stream(a, b"01234567890").unwrap(),
        StreamTransmitOutcome::Accepted(11)
    );
    assert!(e.release_eligible(now).unwrap().is_empty());
    assert_eq!(
        e.transmit_stream(a, b"1").unwrap(),
        StreamTransmitOutcome::Accepted(1)
    );
    assert_eq!(
        e.release_eligible(now).unwrap(),
        BTreeSet::from([NetworkChannelId(2)])
    );
    assert_eq!(
        e.receive_stream(b, 5, false).unwrap(),
        StreamReceiveOutcome::Bytes(b"reply".to_vec())
    );
    assert!(e.finish().is_err());
    assert_eq!(
        e.transmit_stream(a, b"2345678901234").unwrap(),
        StreamTransmitOutcome::Accepted(13)
    );
    e.finish().unwrap();
}
#[test]
fn native_replay_zero_datagram_advances_whole_packet_without_byte_progress() {
    let mut t = empty();
    add_channel(&mut t, 1, true);
    add_channel(&mut t, 2, false);
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::DatagramSetup,
        },
        &[],
    );
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::Datagram(datagram(0, b"")),
    });
    progress(
        &mut t,
        1,
        NetworkProgressV4::DatagramPrefix { completed: 1 },
        &[0],
    );
    input(
        &mut t,
        2,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[0, 1],
    );
    progress(
        &mut t,
        2,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[2],
    );
    t.validate().unwrap();
    let now = t.epoch_global_time().unwrap();
    let mut e = NetworkReplayEngine::replay_native_receive(t).unwrap();
    let a = bind(&mut e, 1);
    let b = bind(&mut e, 2);
    assert!(e.release_eligible(now).unwrap().is_empty());
    let before = format!("{e:?}");
    assert!(e.transmit_datagram(a, &datagram(0, b"x")).is_err());
    assert_eq!(format!("{e:?}"), before);
    e.transmit_datagram(a, &datagram(0, b"")).unwrap();
    assert_eq!(e.channels[&NetworkChannelId(1)].transmitted, 0);
    connected(&mut e, b, now);
    e.finish().unwrap();
}
#[test]
fn native_replay_retirement_preserves_producer_facts_after_binding_removal() {
    let mut t = empty();
    add_channel(&mut t, 1, true);
    add_channel(&mut t, 2, false);
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::DatagramSetup,
        },
        &[],
    );
    progress(&mut t, 1, NetworkProgressV4::Retired, &[0]);
    input(
        &mut t,
        2,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[0, 1],
    );
    progress(
        &mut t,
        2,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[2],
    );
    t.validate().unwrap();
    let now = t.epoch_global_time().unwrap();
    let mut e = NetworkReplayEngine::replay_native_receive(t).unwrap();
    let a = bind(&mut e, 1);
    let b = bind(&mut e, 2);
    assert!(e.release_eligible(now).unwrap().is_empty());
    assert_eq!(e.retire_open_file(a), Some(NetworkChannelId(1)));
    assert_eq!(e.channel_for(a), None);
    connected(&mut e, b, now);
    e.finish().unwrap();
}
#[test]
fn native_recorder_has_no_generic_profile_or_legacy_capture_exemption() {
    let mut e = NetworkReplayEngine::record_native_receive(empty().epoch);
    let p = profile();
    let before = format!("{e:?}");
    assert!(matches!(
        e.register_stream_socket(
            file(1),
            p.key,
            NetworkStreamNamespace {
                device: 7,
                inode: 11
            },
            Some(p)
        ),
        Err(NetworkReplayError::FdPublicationProtocol(_))
    ));
    assert_eq!(format!("{e:?}"), before);
    let time = e.trace_epoch().timestamp_nanos_opt().unwrap() as u64;
    assert!(matches!(
        e.record_input(NetworkInputEventV2 {
            ordinal: 0,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV2 {
                not_before_global_time: LogicalTime::from_nanos(time),
                after_transmitted_offset: 0
            },
            event: NetworkInputKindV2::StreamBytes {
                stream_offset: 0,
                bytes: b"x".to_vec()
            }
        }),
        Err(NetworkReplayError::WrongMode)
    ));
    assert!(matches!(
        e.record_output(NetworkOutputEventV2 {
            channel: NetworkChannelId(1),
            event: NetworkOutputKindV2::StreamBytes {
                stream_offset: 0,
                bytes: b"x".to_vec()
            }
        }),
        Err(NetworkReplayError::WrongMode)
    ));
    assert_eq!(format!("{e:?}"), before);
    assert_eq!(e.into_native_recorded_trace().unwrap(), empty());
}

pub(super) fn unsubmitted_entry(
    owner: NetworkStreamOwner,
) -> (NetworkReplayEngine, NetworkStreamCallId) {
    // Controlled installed profile only; the entry itself must be issued through
    // the actual scheduler and worker-admission borrow below.
    let mut e = NetworkReplayEngine::record_native_receive(empty().epoch);
    let ofd = OpenFileId::new_socket(owner.thread, 0);
    let p = profile();
    e.register_stream_socket_profile(
        ofd,
        p.key,
        NetworkStreamNamespace {
            device: 7,
            inode: 11,
        },
        Some(p),
    )
    .unwrap();
    e.ensure_channel(
        ofd,
        NetworkChannelBinding {
            transport: NetworkTransportV2::Tcp,
            role: NetworkEndpointRoleV2::OutboundClient,
            peer_address: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 1],
                port: 443,
            }),
            requested_local_constraint: None,
            observed_local_address: None,
            accepted_from: None,
            selected_channel: None,
        },
    )
    .unwrap();
    let control = e.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
    let call = e.begin_stream_call(owner, control).unwrap().id;
    (e, call)
}
#[tokio::test]
async fn native_entry_requires_actual_join_borrow_and_same_call_one_use() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let grant = scheduler
        .foreground_native_observation(owner, &root)
        .unwrap();
    let (mut e, call) = unsubmitted_entry(owner);
    let now = empty().epoch_global_time().unwrap();
    let attempt = e.begin_native_entry_stamp(owner, call).unwrap();
    runtime
        .with_foreground_prefix(&prefix, |admission| {
            e.stamp_native_receive_entry(attempt, admission, &grant, now)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .unwrap();
    let release = e
        .native_entry_release(owner, call, &root, grant.epoch(), now)
        .unwrap();
    assert_eq!(release.receive_entry_cut, NetworkReceiveEntryCutV4(0));
    assert!(release.prerequisites.is_empty());
    assert!(e.begin_native_entry_stamp(owner, call).is_err());
    assert!(
        e.native_entry_release(owner, call, &root, grant.epoch() + 1, now)
            .is_err()
    );
    e.consume_native_entry(call);
    assert!(
        e.native_entry_release(owner, call, &root, grant.epoch(), now)
            .is_err()
    );
    assert!(matches!(e.finish(),Err(NetworkReplayError::UnresolvedStreamCall(id)) if id==call));
}
#[tokio::test]
async fn native_entry_never_refreshes_cut_after_later_progress_or_abandonment() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let grant = scheduler
        .foreground_native_observation(owner, &root)
        .unwrap();
    let (mut e, call) = unsubmitted_entry(owner);
    // A valid pre-existing ledger is a controlled premise of this entry test.
    // Production issuers of its committed facts are qualified separately.
    let mut prior = empty();
    add_channel(&mut prior, 1, false);
    input(
        &mut prior,
        1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        &[],
    );
    progress(
        &mut prior,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
        },
        &[0],
    );
    prior.validate().unwrap();
    let EngineState::Native(native) = &mut e.mode else {
        unreachable!()
    };
    native.trace = prior;
    let now = empty().epoch_global_time().unwrap();
    let attempt = e.begin_native_entry_stamp(owner, call).unwrap();
    runtime
        .with_foreground_prefix(&prefix, |admission| {
            e.stamp_native_receive_entry(attempt, admission, &grant, now)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .unwrap();
    let before = e
        .native_entry_release(owner, call, &root, grant.epoch(), now)
        .unwrap();
    assert_eq!(before.receive_entry_cut, NetworkReceiveEntryCutV4(2));
    assert_eq!(before.prerequisites, vec![NetworkReleaseNodeIdV4(1)]);
    let later = LogicalTime::from_nanos(now.as_nanos() + 37);
    let after = e
        .native_entry_release(owner, call, &root, grant.epoch(), later)
        .unwrap();
    assert_eq!(after.receive_entry_cut, before.receive_entry_cut);
    assert_eq!(after.prerequisites, before.prerequisites);
    assert_eq!(after.not_before_global_time, later);
    let EngineState::Native(native) = &mut e.mode else {
        unreachable!()
    };
    progress(&mut native.trace, 1, NetworkProgressV4::Retired, &[1]);
    native.trace.validate().unwrap();
    assert!(
        e.native_entry_release(owner, call, &root, grant.epoch(), later)
            .is_err(),
        "later producer progress cannot be omitted from the entry interval"
    );
    assert_eq!(
        e.stream_calls[&call].native_entry.as_ref().unwrap().release,
        before,
        "a refusal must preserve the original cut/frontier/time instead of refreshing it"
    );
    e.stream_owner_gone(owner);
    assert!(
        e.native_entry_release(owner, call, &root, grant.epoch(), later)
            .is_err()
    );
    assert!(e.stream_calls.contains_key(&call));
    assert!(e.begin_native_entry_stamp(owner, call).is_err());
}
#[tokio::test]
async fn native_entry_rejects_same_number_from_different_actual_call() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let grant = scheduler
        .foreground_native_observation(owner, &root)
        .unwrap();
    let (mut first, a) = unsubmitted_entry(owner);
    let (mut second, b) = unsubmitted_entry(owner);
    assert_eq!(a, b);
    let wrong = first.begin_native_entry_stamp(owner, a).unwrap();
    let original = second.begin_native_entry_stamp(owner, b).unwrap();
    let now = empty().epoch_global_time().unwrap();
    let before = format!("{second:?}");
    assert!(
        runtime
            .with_foreground_prefix(&prefix, |admission| second
                .stamp_native_receive_entry(wrong, admission, &grant, now)
                .map_err(|e| std::io::Error::other(e.to_string())))
            .is_err()
    );
    assert_eq!(format!("{second:?}"), before);
    assert!(first.stream_calls[&a].native_entry.is_none());
    assert!(first.begin_native_entry_stamp(owner, a).is_err());
    runtime
        .with_foreground_prefix(&prefix, |admission| {
            second
                .stamp_native_receive_entry(original, admission, &grant, now)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .unwrap();
    assert!(second.stream_calls[&b].native_entry.is_some());
}
#[tokio::test]
async fn native_entry_failed_worker_admission_spends_attempt_before_fresh_join() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (runtime, prefix) = crate::network_runtime::controlled_joined_prefix(root.clone()).await;
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let grant = scheduler
        .foreground_native_observation(owner, &root)
        .unwrap();
    let (mut e, call) = unsubmitted_entry(owner);
    let now = empty().epoch_global_time().unwrap();
    let attempt = e.begin_native_entry_stamp(owner, call).unwrap();
    let release = runtime.controlled_foreground_store_worker().await;
    let entered = std::cell::Cell::new(false);
    let before = e.foreground_store_semantic_fixture_state();
    let failure = runtime
        .with_foreground_prefix(&prefix, |admission| {
            entered.set(true);
            e.stamp_native_receive_entry(attempt, admission, &grant, now)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("actually joined native prefix")
    );
    assert!(!entered.get());
    assert!(e.stream_calls[&call].native_entry.is_none());
    release.send(()).unwrap();
    let fresh = runtime.join_foreground_prefix(root.clone()).await.unwrap();
    runtime.with_foreground_prefix(&fresh, |_| Ok(())).unwrap();
    assert!(e.begin_native_entry_stamp(owner, call).is_err());
    assert!(
        e.native_entry_release(owner, call, &root, grant.epoch(), now)
            .is_err()
    );
    assert_eq!(e.foreground_store_semantic_fixture_state(), before);
    assert!(matches!(e.finish(),Err(NetworkReplayError::UnresolvedStreamCall(id)) if id==call));
}
#[test]
fn native_replay_unimplemented_eof_control_has_no_implicit_consumer() {
    let mut trace = one_stream();
    input(
        &mut trace,
        1,
        NetworkInputKindV2::PeerShutdown {
            stream_offset: 4,
            direction: NetworkShutdownV2::Write,
        },
        &[1],
    );
    trace.validate().unwrap();
    let now = trace.epoch_global_time().unwrap();
    let terminal = NetworkReleaseNodeIdV4(trace.release_model.nodes().len() as u64 - 1);
    let mut engine = NetworkReplayEngine::replay_native_receive(trace)
        .expect("EOF trace requires its actual typed consumption issuer");
    let ofd = bind(&mut engine, 1);
    connected(&mut engine, ofd, now);
    engine.release_eligible(now).unwrap();
    assert!(
        !engine.native_completed().unwrap().contains(&terminal),
        "releasing an EOF behind buffered bytes is not consuming it"
    );
    assert_eq!(engine.channels[&NetworkChannelId(1)].inbound_consumed, 0);
    assert_eq!(engine.channels[&NetworkChannelId(1)].inbound.len(), 3);
    assert!(matches!(
        engine.channels[&NetworkChannelId(1)].inbound.back(),
        Some(InboundOutcome::PeerShutdown {
            stream_offset: 4,
            direction: NetworkShutdownV2::Write
        })
    ));
    engine
        .release_eligible(LogicalTime::from_nanos(now.as_nanos() + 1_000_000))
        .unwrap();
    assert!(!engine.native_completed().unwrap().contains(&terminal));
    assert!(engine.finish().is_err());
}

// These controls use real Linux socket/close/dup effects and the shared engine's
// descriptor protocol. Socket profiles and any prior Connect rows are explicit
// controlled premises; these are not provider or Guest Record E2E evidence.
fn retirement_socket(connected: bool) -> (std::os::fd::OwnedFd, Option<std::net::TcpStream>) {
    if connected {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (client.into(), Some(peer))
    } else {
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket: {}", std::io::Error::last_os_error());
        (unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }, None)
    }
}

pub(super) fn retirement_binding(e: &mut NetworkReplayEngine, ofd: OpenFileId) -> NetworkChannelId {
    e.controlled_connect_socket_premise(ofd);
    e.ensure_channel(
        ofd,
        NetworkChannelBinding {
            transport: NetworkTransportV2::Tcp,
            role: NetworkEndpointRoleV2::OutboundClient,
            peer_address: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 1],
                port: 443,
            }),
            requested_local_constraint: None,
            observed_local_address: None,
            accepted_from: None,
            selected_channel: None,
        },
    )
    .unwrap()
}

fn retirement_fixture(
    connected: bool,
) -> (
    NetworkReplayEngine,
    NetworkStreamOwner,
    OpenFileId,
    NetworkChannelId,
    std::os::fd::OwnedFd,
    Option<std::net::TcpStream>,
) {
    let thread = crate::types::DetTid::from_raw(61);
    let owner = NetworkStreamOwner {
        thread,
        mm: detcore_model::futex::MmId::initial(thread),
    };
    retirement_fixture_for_owner(connected, owner)
}

pub(super) fn retirement_fixture_for_owner(
    connected: bool,
    owner: NetworkStreamOwner,
) -> (
    NetworkReplayEngine,
    NetworkStreamOwner,
    OpenFileId,
    NetworkChannelId,
    std::os::fd::OwnedFd,
    Option<std::net::TcpStream>,
) {
    let (socket, peer) = retirement_socket(connected);
    let ofd = file(1);
    let mut e = NetworkReplayEngine::record_native_receive(empty().epoch);
    let channel = retirement_binding(&mut e, ofd);
    if connected {
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        input(
            &mut native.trace,
            channel.0,
            NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
            &[],
        );
        progress(
            &mut native.trace,
            channel.0,
            NetworkProgressV4::Established {
                source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
            },
            &[0],
        );
    }
    e.native_trace_fixture().validate().unwrap();
    (e, owner, ofd, channel, socket, peer)
}

pub(super) fn close_retirement_socket(socket: std::os::fd::OwnedFd) {
    use std::os::fd::IntoRawFd;
    let raw = socket.into_raw_fd();
    assert_eq!(unsafe { libc::close(raw) }, 0);
    assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
}

fn retirement_close(
    e: &mut NetworkReplayEngine,
    owner: NetworkStreamOwner,
    ofd: OpenFileId,
    socket: std::os::fd::OwnedFd,
) -> Result<(), NetworkReplayError> {
    let control = e.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
    e.submit_descriptor_effect(owner, control, NetworkDescriptorEffect::CloseDescriptor)
        .unwrap();
    close_retirement_socket(socket);
    e.confirm_descriptor_effect(owner, control, Ok(())).unwrap();
    e.finish_socket_control(
        owner,
        control,
        NetworkSocketControlFinish::Closed { last_alias: true },
    )
}

#[tokio::test]
async fn native_retirement_real_unconnected_close_needs_no_establishment() {
    let policy = super::policy_tests::ClosePolicyFixture::new().await;
    let (mut e, _owner, ofd, channel, socket, _peer) =
        retirement_fixture_for_owner(false, policy.owner());
    policy.close(&mut e, ofd, socket).unwrap();
    assert_eq!(e.channel_for(ofd), None);
    assert!(!e.reverse_bindings.contains_key(&channel));
    assert!(e.retired_channels.contains(&channel));
    e.check_stream_operations_finished().unwrap();
    let trace = e.into_native_recorded_trace().unwrap();
    assert!(trace.inputs.is_empty());
    assert_eq!(
        trace.release_model.nodes(),
        &[NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(0),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::Retired
            },
            prerequisites: vec![],
        }]
    );
    trace.validate().unwrap();
}

#[tokio::test]
async fn native_retirement_real_established_close_appends_once() {
    let policy = super::policy_tests::ClosePolicyFixture::new().await;
    let (mut e, _owner, ofd, channel, socket, _peer) =
        retirement_fixture_for_owner(true, policy.owner());
    policy.close(&mut e, ofd, socket).unwrap();
    assert_eq!(e.channel_for(ofd), None);
    assert_eq!(e.retire_open_file(ofd), None);
    assert_eq!(e.retire_open_file(ofd), None);
    let trace = e.into_native_recorded_trace().unwrap();
    assert_eq!(trace.release_model.nodes().len(), 3);
    assert_eq!(
        trace.release_model.nodes()[2],
        NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(2),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::Retired
            },
            prerequisites: vec![NetworkReleaseNodeIdV4(1)],
        }
    );
    trace.validate().unwrap();
}

#[tokio::test]
async fn native_retirement_real_close_waits_for_actual_call_pin_release() {
    let policy = super::policy_tests::ClosePolicyFixture::new().await;
    let (mut e, owner, ofd, channel, socket, _peer) =
        retirement_fixture_for_owner(true, policy.owner());
    let control = e.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
    let call = e.begin_stream_call(owner, control).unwrap().id;
    let raw_pin = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    assert!(raw_pin >= 0);
    let pin = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_pin) };
    e.confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
        .unwrap();
    e.finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
        .unwrap();
    policy.close(&mut e, ofd, socket).unwrap();
    assert_eq!(e.channel_for(ofd), Some(channel));
    assert!(!e.retired_channels.contains(&channel));
    assert_eq!(e.native_trace_fixture().release_model.nodes().len(), 2);
    assert!(unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFD) } >= 0);
    assert!(matches!(e.finish(), Err(NetworkReplayError::UnresolvedStreamCall(id)) if id == call));
    e.begin_stream_call_release(owner, call).unwrap();
    close_retirement_socket(pin);
    e.finish_stream_call_release(owner, call).unwrap();
    assert_eq!(e.channel_for(ofd), None);
    let trace = e.into_native_recorded_trace().unwrap();
    assert_eq!(trace.release_model.nodes().len(), 3);
    trace.validate().unwrap();
}

#[test]
fn native_retirement_real_close_malformed_ledger_preserves_custody_without_panic() {
    let (mut e, owner, ofd, channel, socket, _peer) = retirement_fixture(true);
    let EngineState::Native(native) = &mut e.mode else {
        unreachable!()
    };
    nodes(&mut native.trace)[0].id = NetworkReleaseNodeIdV4(99);
    let before = e.native_trace_fixture();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        retirement_close(&mut e, owner, ofd, socket)
    }));
    let error = outcome
        .expect("actual close must not panic on invalid retirement ledger")
        .expect_err("invalid retirement ledger must refuse publication");
    assert_eq!(
        format!("{error:?}"),
        format!("NativeRetirement {{ channel: {channel:?}, error: NonCanonicalNode }}")
    );
    assert_eq!(e.native_trace_fixture(), before);
    assert_eq!(e.channel_for(ofd), Some(channel));
    assert_eq!(e.reverse_bindings.get(&channel), Some(&ofd));
    assert!(!e.retired_channels.contains(&channel));
    assert!(e.retired_open_files.contains(&ofd));
    assert!(e.socket_controls.is_empty());
    assert_eq!(e.retire_open_file(ofd), None);
    assert_eq!(
        format!("{:?}", e.finish().unwrap_err()),
        format!("{error:?}")
    );
    assert_eq!(
        format!("{:?}", e.into_native_recorded_trace().unwrap_err()),
        format!("{error:?}")
    );
}

#[test]
fn native_retirement_first_error_survives_later_physical_cleanup_and_all_record_accessors() {
    for accessor in 0..2 {
        let (mut e, owner, ofd, channel, socket, _peer) = retirement_fixture(true);
        let (second, _second_peer) = retirement_socket(false);
        let second_file = file(2);
        let second_channel = retirement_binding(&mut e, second_file);
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        nodes(&mut native.trace)[0].id = NetworkReleaseNodeIdV4(99);
        let first = retirement_close(&mut e, owner, ofd, socket).unwrap_err();
        let first = format!("{first:?}");
        assert_eq!(
            first,
            format!("NativeRetirement {{ channel: {channel:?}, error: NonCanonicalNode }}")
        );
        // Controlled repair of the original malformed byte cannot clear the
        // retained failure or let a later retirement replace its first cause.
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        nodes(&mut native.trace)[0].id = NetworkReleaseNodeIdV4(0);
        let before = e.native_trace_fixture();
        let later = retirement_close(&mut e, owner, second_file, second).unwrap_err();
        assert_eq!(format!("{later:?}"), first);
        assert!(e.socket_controls.is_empty());
        assert_eq!(e.channel_for(ofd), Some(channel));
        assert_eq!(e.channel_for(second_file), Some(second_channel));
        assert!(e.retired_channels.is_empty());
        assert_eq!(e.retire_open_file(ofd), None);
        assert_eq!(e.retire_open_file(second_file), None);
        assert_eq!(e.native_trace_fixture(), before);
        assert_eq!(format!("{:?}", e.finish().unwrap_err()), first);
        assert_eq!(format!("{:?}", e.native_input_count().unwrap_err()), first);
        assert_eq!(
            format!("{:?}", e.native_definitions_mut().unwrap_err()),
            first
        );
        assert_eq!(
            format!(
                "{:?}",
                e.begin_native_entry_stamp(owner, NetworkStreamCallId(999))
                    .unwrap_err()
            ),
            first
        );
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        assert_eq!(format!("{:?}", native.record().unwrap_err()), first);
        let final_error = if accessor == 0 {
            e.into_native_recorded_trace().unwrap_err()
        } else {
            e.into_recorded_versioned_trace().unwrap_err()
        };
        assert_eq!(format!("{final_error:?}"), first);
    }
}

#[test]
fn native_retirement_refuses_duplicate_progress_and_missing_channel_without_commit() {
    for duplicate in [true, false] {
        let (mut e, owner, ofd, channel, socket, _peer) = retirement_fixture(true);
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        if duplicate {
            progress(
                &mut native.trace,
                channel.0,
                NetworkProgressV4::Retired,
                &[1],
            );
        } else {
            native.trace.channels.clear();
        }
        let before = e.native_trace_fixture();
        let error = retirement_close(&mut e, owner, ofd, socket).unwrap_err();
        let kind = if duplicate {
            "EventAfterRetirement"
        } else {
            "InvalidReference"
        };
        assert_eq!(
            format!("{error:?}"),
            format!("NativeRetirement {{ channel: {channel:?}, error: {kind} }}")
        );
        assert_eq!(e.native_trace_fixture(), before);
        assert_eq!(e.channel_for(ofd), Some(channel));
        assert_eq!(e.reverse_bindings.get(&channel), Some(&ofd));
        assert!(!e.retired_channels.contains(&channel));
        assert!(e.socket_controls.is_empty());
        assert_eq!(
            format!("{:?}", e.finish().unwrap_err()),
            format!("{error:?}")
        );
    }
}

#[test]
fn native_retirement_failure_refuses_call_transfer_but_releases_actual_fd_reader() {
    use crate::types::FdSlot;
    use crate::types::FdSlotBinding;
    use crate::types::NetworkFdSlot;
    use crate::types::NetworkFdSlotReplacement;
    let (mut e, owner, ofd, channel, socket, _peer) = retirement_fixture(true);
    let (second, _second_peer) = retirement_socket(false);
    let second_file = file(2);
    retirement_binding(&mut e, second_file);
    // Explicit component table/installation premise. The reader acquisition,
    // refused transfer and reader cleanup below use the actual engine methods.
    e.fd_table_fixture_enable();
    let files = e.fd_publication_fixture_register(owner, None);
    let binding = FdSlotBinding {
        slot: FdSlot {
            files,
            fd: second.as_raw_fd(),
        },
        generation: 1,
        open_file: second_file,
    };
    let replacement = NetworkFdSlotReplacement {
        files,
        installation_generation: 1,
        before: None,
        after: Some(NetworkFdSlot {
            binding,
            cloexec: true,
        }),
    };
    let effect = e.fd_publication_fixture_effect(owner, replacement);
    let permit = e.acquire_fd_publication(owner, files).unwrap().permit;
    let batch = NetworkFdPublicationBatch {
        files,
        sequence: 1,
        previous_generation: 0,
        through_generation: 1,
        entries: vec![NetworkFdPublicationEntry {
            replacement,
            effect,
        }],
    };
    e.publish_fd_publication(owner, permit, &batch).unwrap();
    e.acknowledge_fd_publication(owner, permit, &batch).unwrap();
    let EngineState::Native(native) = &mut e.mode else {
        unreachable!()
    };
    nodes(&mut native.trace)[0].id = NetworkReleaseNodeIdV4(99);
    let error = retirement_close(&mut e, owner, ofd, socket).unwrap_err();
    let error = format!("{error:?}");
    assert_eq!(
        error,
        format!("NativeRetirement {{ channel: {channel:?}, error: NonCanonicalNode }}")
    );
    let NetworkFdReadBegin::Admitted(read) =
        e.begin_fd_read(owner, files, binding.slot.fd).unwrap()
    else {
        panic!("reader needs no recovery")
    };
    assert_eq!(read.binding, Some(binding));
    let before = format!("{e:?}");
    assert_eq!(
        format!(
            "{:?}",
            e.begin_native_stream_call_from_read(owner, read.clone())
                .unwrap_err()
        ),
        error
    );
    assert_eq!(
        format!("{e:?}"),
        before,
        "refusal must not transfer or create custody"
    );
    assert_eq!(e.fd_publications[&files].reader.as_ref(), Some(&read));
    assert!(e.stream_calls.is_empty());
    e.finish_fd_read(owner, read).unwrap();
    assert!(e.fd_publications[&files].reader.is_none());
    assert!(e.fd_publications[&files].active.is_none());
    assert!(e.socket_controls.is_empty());
    assert!(e.stream_calls.is_empty());
    close_retirement_socket(second);
    e.retire_fd_table_owner(owner);
    e.stream_owner_gone(owner);
    assert!(e.fd_publications.is_empty());
    assert_eq!(format!("{:?}", e.finish().unwrap_err()), error);
    assert_eq!(
        format!("{:?}", e.into_recorded_versioned_trace().unwrap_err()),
        error
    );
}
