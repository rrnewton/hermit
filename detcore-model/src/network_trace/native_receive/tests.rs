use std::io::Cursor;

use chrono::TimeZone;

use super::*;

#[test]
fn socket_error_read_preserves_zero_nonzero_and_v4_only_framing() {
    let mut trace = empty();
    add_channel(&mut trace, 1, false);
    input(&mut trace, 1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected), &[]);
    progress(&mut trace, 1, NetworkProgressV4::Established {
        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
    }, &[0]);
    for errno in [libc::ECONNRESET, 0, 0] {
        let cut = NetworkReceiveEntryCutV4(nodes(&mut trace).len() as u64);
        let prerequisites: Vec<_> = trace.entry_frontier(cut).unwrap().iter().map(|id| id.0).collect();
        let ordinal = trace.inputs.len() as u64;
        input(&mut trace, 1, NetworkInputKindV2::SocketErrorRead {
            consumed_prefix: 0, errno,
        }, &prerequisites);
        progress(&mut trace, 1, NetworkProgressV4::SocketErrorConsumed { input_ordinal: ordinal }, &[cut.0]);
    }
    trace.validate().unwrap();
    let mut bytes = Vec::new();
    trace.write_framed(&mut bytes).unwrap();
    assert_eq!(NetworkTraceV4::read_framed(Cursor::new(bytes)).unwrap(), trace);
    let legacy = NetworkTraceV2 {
        epoch: trace.epoch, channels: trace.channels.clone(), outputs: vec![],
        inputs: trace.inputs.iter().map(|input| NetworkInputEventV2 {
            ordinal: input.ordinal, channel: input.channel, event: input.event.clone(),
            release: NetworkReleaseV2 { not_before_global_time: trace.epoch_global_time().unwrap(),
                after_transmitted_offset: 0 },
        }).collect(),
    };
    assert_eq!(legacy.validate(), Err(NetworkTraceValidationError::InvalidChannelRelationship));
    for (consumed_prefix, errno) in [(1, 0), (0, -1), (0, 4096)] {
        let mut invalid = trace.clone();
        invalid.inputs[1].event = NetworkInputKindV2::SocketErrorRead { consumed_prefix, errno };
        assert_eq!(invalid.validate(), Err(Invalid::Payload(NetworkTraceValidationError::InvalidChannelRelationship)));
    }
    let mut changed_frontier = trace.clone();
    changed_frontier.inputs[1].release.prerequisites.clear();
    nodes(&mut changed_frontier)[2].prerequisites.clear();
    assert_eq!(changed_frontier.validate(), Err(Invalid::InvalidEntryFrontier));
    let mut missing = trace.clone();
    nodes(&mut missing).pop();
    assert_eq!(missing.validate(), Err(Invalid::MissingProducer));
    let mut duplicate = trace.clone();
    let last = duplicate.inputs.len() as u64 - 1;
    progress(&mut duplicate, 1, NetworkProgressV4::SocketErrorConsumed { input_ordinal: last }, &[]);
    assert_eq!(duplicate.validate(), Err(Invalid::InvalidProgress));
    // Delaying the completion past another input would let its entry cut omit
    // the consuming read. Input and its completion are one recorder transaction.
    let mut delayed = empty();
    add_channel(&mut delayed, 1, false);
    input(&mut delayed, 1,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected), &[]);
    progress(&mut delayed, 1, NetworkProgressV4::Established {
        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
    }, &[0]);
    input(&mut delayed, 1, NetworkInputKindV2::SocketErrorRead { consumed_prefix: 0, errno: 0 }, &[1]);
    input(&mut delayed, 1, NetworkInputKindV2::RawTcpPollState { consumed_prefix: 0, revents: libc::POLLOUT }, &[1]);
    progress(&mut delayed, 1, NetworkProgressV4::SocketErrorConsumed { input_ordinal: 1 }, &[2]);
    assert_eq!(delayed.validate(), Err(Invalid::InvalidProgress));
}

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
        NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { .. } => panic!("legacy fixture changed its release policy"),
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
            disposition: NetworkNativeCopyDispositionV4::Observe,
            physical_before: 0,
            physical_after: 0,
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
fn zero_datagram(retire: bool) -> NetworkTraceV4 {
    let mut t = empty();
    add_channel(&mut t, 1, true);
    add_channel(&mut t, 2, true);
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::DatagramSetup,
        },
        &[],
    );
    progress(
        &mut t,
        2,
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
        &[],
    );
    let prerequisites = if retire {
        progress(&mut t, 1, NetworkProgressV4::Retired, &[]);
        vec![0, 1, 2, 3]
    } else {
        vec![0, 1, 2]
    };
    input(
        &mut t,
        2,
        NetworkInputKindV2::Datagram(datagram(0, b"response")),
        &prerequisites,
    );
    t
}

#[test]
fn cross_channel_partial_committed_prefix_releases_without_whole_write_completion() {
    let t = cross_channel();
    t.validate().unwrap();
    let r = &t.inputs[2].release;
    let now = t.epoch_global_time().unwrap();
    let mut completed = [NetworkReleaseNodeIdV4(1), NetworkReleaseNodeIdV4(3)]
        .into_iter()
        .collect();
    assert!(!r.is_eligible(now, &completed));
    // Even completion of the expected whole output is not the declared earlier
    // producer node: the actual engine must fulfill each reached milestone.
    completed.insert(NetworkReleaseNodeIdV4(6));
    assert!(!r.is_eligible(now, &completed));
    completed.insert(NetworkReleaseNodeIdV4(4));
    assert!(r.is_eligible(now, &completed));
    assert!(!r.is_eligible(LogicalTime::from_nanos(now.as_nanos() - 1), &completed));
}

#[test]
fn wrong_channel_insufficient_impossible_and_overflowing_stream_coordinates_refuse() {
    for variant in 0..6 {
        let mut t = cross_channel();
        match variant {
            0 => {
                if let NetworkReleaseNodeKindV4::Progress { channel, .. } =
                    &mut nodes(&mut t)[4].kind
                {
                    *channel = NetworkChannelId(2)
                }
            }
            1 => {
                if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
                    &mut nodes(&mut t)[6].kind
                {
                    *milestone = NetworkProgressV4::StreamPrefix {
                        exclusive_offset: 24,
                    }
                }
            }
            2 => {
                if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
                    &mut nodes(&mut t)[4].kind
                {
                    *milestone = NetworkProgressV4::StreamPrefix {
                        exclusive_offset: 26,
                    }
                }
            }
            3 => {
                if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
                    &mut nodes(&mut t)[4].kind
                {
                    *milestone = NetworkProgressV4::StreamPrefix {
                        exclusive_offset: u64::MAX,
                    }
                }
            }
            4 => {
                if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
                    &mut nodes(&mut t)[4].kind
                {
                    *milestone = NetworkProgressV4::StreamPrefix {
                        exclusive_offset: 0,
                    }
                }
            }
            5 => {
                if let NetworkOutputKindV2::StreamBytes { stream_offset, .. } =
                    &mut t.outputs[0].event
                {
                    *stream_offset = u64::MAX
                }
            }
            _ => unreachable!(),
        }
        let expected = match variant {
            1 => Invalid::MissingProducer,
            5 => Invalid::Payload(NetworkTraceValidationError::NonContiguousOutput),
            _ => Invalid::InvalidProgress,
        };
        assert_eq!(t.validate(), Err(expected), "variant {variant}");
    }
}

#[test]
fn zero_datagram_advances_whole_message_progress_and_retired_progress_remains_eligible() {
    for retire in [false, true] {
        let t = zero_datagram(retire);
        t.validate().unwrap();
        let r = &t.inputs[0].release;
        let mut completed: BTreeSet<NetworkReleaseNodeIdV4> =
            [NetworkReleaseNodeIdV4(0), NetworkReleaseNodeIdV4(1)]
                .into_iter()
                .collect();
        if retire {
            completed.insert(NetworkReleaseNodeIdV4(3));
        }
        assert!(!r.is_eligible(t.epoch_global_time().unwrap(), &completed));
        completed.insert(NetworkReleaseNodeIdV4(2));
        assert!(r.is_eligible(t.epoch_global_time().unwrap(), &completed));
        let mut wrong = t.clone();
        if let NetworkReleaseNodeKindV4::Progress { milestone, .. } = &mut nodes(&mut wrong)[2].kind
        {
            *milestone = NetworkProgressV4::StreamPrefix {
                exclusive_offset: 1,
            }
        };
        assert_eq!(wrong.validate(), Err(Invalid::InvalidProgress));
    }
}

#[test]
fn datagram_zero_missing_sequence_foreign_channel_and_progress_after_retirement_refuse() {
    for variant in 0..5 {
        let mut t = zero_datagram(true);
        match variant {
            0 => {
                if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
                    &mut nodes(&mut t)[2].kind
                {
                    *milestone = NetworkProgressV4::DatagramPrefix { completed: 0 }
                }
            }
            1 => {
                if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
                    &mut nodes(&mut t)[2].kind
                {
                    *milestone = NetworkProgressV4::DatagramPrefix { completed: 2 }
                }
            }
            2 => {
                if let NetworkReleaseNodeKindV4::Progress { channel, .. } =
                    &mut nodes(&mut t)[2].kind
                {
                    *channel = NetworkChannelId(99)
                }
            }
            3 => {
                progress(
                    &mut t,
                    1,
                    NetworkProgressV4::OutputError { output_ordinal: 0 },
                    &[],
                );
            }
            4 => {
                input(
                    &mut t,
                    1,
                    NetworkInputKindV2::Datagram(datagram(0, b"late")),
                    &[0, 1, 2, 3],
                );
            }
            _ => unreachable!(),
        }
        let expected = match variant {
            0 | 1 => Invalid::InvalidProgress,
            2 => Invalid::InvalidReference,
            3 | 4 => Invalid::EventAfterRetirement,
            _ => unreachable!(),
        };
        assert_eq!(t.validate(), Err(expected), "variant {variant}");
    }
}

#[test]
fn every_output_requires_a_producer_even_without_an_advertised_input_dependency() {
    let mut t = cross_channel();
    nodes(&mut t).pop();
    assert_eq!(t.validate(), Err(Invalid::MissingProducer));
    let mut t = zero_datagram(false);
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::Datagram(datagram(1, b"unrepresented")),
    });
    assert_eq!(t.validate(), Err(Invalid::MissingProducer));
    let mut t = cross_channel();
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::SocketError {
            stream_offset: 25,
            errno: libc::EAGAIN,
        },
    });
    assert_eq!(t.validate(), Err(Invalid::MissingProducer));
}

#[test]
fn shutdown_error_and_retirement_have_exact_nonbyte_output_coverage() {
    let mut t = cross_channel();
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::SocketError {
            stream_offset: 25,
            errno: libc::EAGAIN,
        },
    });
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::Shutdown {
            stream_offset: 25,
            direction: NetworkShutdownV2::Write,
        },
    });
    progress(
        &mut t,
        1,
        NetworkProgressV4::OutputError { output_ordinal: 1 },
        &[],
    );
    progress(
        &mut t,
        1,
        NetworkProgressV4::LocalShutdown { output_ordinal: 2 },
        &[],
    );
    progress(&mut t, 1, NetworkProgressV4::Retired, &[]);
    input(
        &mut t,
        2,
        NetworkInputKindV2::Readiness(NetworkReadinessV2::default()),
        &[1, 3, 6, 7, 8, 9],
    );
    t.validate().unwrap();
    for variant in 0..4 {
        let mut bad = t.clone();
        if let NetworkReleaseNodeKindV4::Progress { milestone, .. } =
            &mut nodes(&mut bad)[if variant < 2 { 7 } else { 8 }].kind
        {
            *milestone = match variant {
                0 => NetworkProgressV4::OutputError { output_ordinal: 2 },
                1 => NetworkProgressV4::OutputError { output_ordinal: 99 },
                2 => NetworkProgressV4::LocalShutdown { output_ordinal: 1 },
                _ => NetworkProgressV4::LocalShutdown { output_ordinal: 0 },
            };
        }
        let expected = if variant == 1 {
            Invalid::InvalidReference
        } else {
            Invalid::InvalidProgress
        };
        assert_eq!(bad.validate(), Err(expected), "variant {variant}");
    }
}

#[test]
fn self_cross_channel_and_multihop_dependency_cycles_are_rejected() {
    for variant in 0..3 {
        let mut t = cross_channel();
        match variant {
            0 => nodes(&mut t)[4].prerequisites = vec![NetworkReleaseNodeIdV4(4)],
            1 => nodes(&mut t)[4].prerequisites = vec![NetworkReleaseNodeIdV4(5)],
            2 => nodes(&mut t)[3].prerequisites = vec![NetworkReleaseNodeIdV4(6)],
            _ => unreachable!(),
        }
        assert_eq!(
            t.validate(),
            Err(Invalid::DependencyCycle),
            "variant {variant}"
        );
    }
}

#[test]
fn missing_duplicated_foreign_and_noncanonical_producer_references_refuse() {
    for variant in 0..6 {
        let mut t = cross_channel();
        match variant {
            0 => nodes(&mut t)[4].id = NetworkReleaseNodeIdV4(99),
            1 => nodes(&mut t)[4].prerequisites = vec![NetworkReleaseNodeIdV4(999)],
            2 => {
                nodes(&mut t)[4].prerequisites =
                    vec![NetworkReleaseNodeIdV4(1), NetworkReleaseNodeIdV4(1)]
            }
            3 => nodes(&mut t)[4].kind = NetworkReleaseNodeKindV4::Input { input_ordinal: 0 },
            4 => nodes(&mut t)[5].kind = NetworkReleaseNodeKindV4::Input { input_ordinal: 99 },
            5 => {
                nodes(&mut t)[4].kind = NetworkReleaseNodeKindV4::Progress {
                    channel: NetworkChannelId(99),
                    milestone: NetworkProgressV4::Retired,
                }
            }
            _ => unreachable!(),
        }
        let expected = match variant {
            0 => Invalid::NonCanonicalNode,
            3 => Invalid::DuplicateProducer,
            _ => Invalid::InvalidReference,
        };
        assert_eq!(t.validate(), Err(expected), "variant {variant}");
    }
}

#[test]
fn entry_frontier_refuses_omissions_and_progress_added_after_entry() {
    for variant in 0..4 {
        let mut t = cross_channel();
        match variant {
            0 => {
                t.inputs[2].release.prerequisites = vec![NetworkReleaseNodeIdV4(4)];
                nodes(&mut t)[5].prerequisites = t.inputs[2].release.prerequisites.clone();
            }
            1 => {
                t.inputs[2]
                    .release
                    .prerequisites
                    .push(NetworkReleaseNodeIdV4(6));
                nodes(&mut t)[5].prerequisites = t.inputs[2].release.prerequisites.clone();
            }
            2 => t.inputs[2].release.receive_entry_cut = NetworkReceiveEntryCutV4(4),
            3 => t.inputs[2].release.receive_entry_cut = NetworkReceiveEntryCutV4(7),
            _ => unreachable!(),
        }
        let expected = if variant == 3 {
            Invalid::InvalidEntryCut
        } else {
            Invalid::InvalidEntryFrontier
        };
        assert_eq!(t.validate(), Err(expected), "variant {variant}");
    }
    // This control proves declared structure only: a fabricated later entry
    // timestamp cannot be authenticated from the serialized trace itself.
}

#[test]
fn native_layout_boundaries_can_change_without_changing_canonical_payload() {
    let t = cross_channel();
    let expected = t.inputs.clone();
    for first in [1, 2, 4, 5] {
        let mut varied = t.clone();
        let base = varied.native_receive_observations[0].fragments[0].clone();
        let mut a = base.clone();
        a.requested = first;
        a.copied = first;
        let mut fragments = vec![a];
        if first < 5 {
            let mut b = base;
            b.stream_offset = first;
            b.requested = 5 - first;
            b.copied = 5 - first;
            b.source_offset = first;
            b.available = 32 - first;
            fragments.push(b);
        }
        varied.native_receive_observations[0].fragments = fragments;
        varied.validate().unwrap();
        assert_eq!(varied.inputs, expected);
    }
}

#[test]
fn native_available_extent_never_mints_payload_or_missing_copy_coverage() {
    for variant in 0..12 {
        let mut t = cross_channel();
        let o = &mut t.native_receive_observations[0];
        match variant {
            0 => o.length = 6,
            1 => o.fragments[0].copied = 4,
            2 => o.fragments[0].requested = 0,
            3 => o.fragments[0].available = 4,
            4 => o.fragments[0].source_offset = 33,
            5 => o.fragments[0].physical_after = 1,
            6 => o.fragments.clear(),
            7 => o.stream_offset = 1,
            8 => o.channel = NetworkChannelId(1),
            9 => o.fragments[0].storage_length = u64::MAX,
            10 => o.fragments[0].nonlinear_length = 33,
            11 => o.length = u64::MAX,
            _ => unreachable!(),
        }
        assert_eq!(
            t.validate(),
            Err(Invalid::InvalidNativeObservation),
            "variant {variant}"
        );
    }
}

#[test]
fn consume_geometry_requires_actual_contiguous_physical_successor() {
    let mut t = cross_channel();
    let f = &mut t.native_receive_observations[0].fragments[0];
    f.disposition = NetworkNativeCopyDispositionV4::Consume;
    f.physical_after = 5;
    t.validate().unwrap();
    for after in [0, 4, 6, u64::MAX] {
        let mut bad = t.clone();
        bad.native_receive_observations[0].fragments[0].physical_after = after;
        assert_eq!(bad.validate(), Err(Invalid::InvalidNativeObservation));
    }
}

#[test]
fn profiles_are_explicit_complete_and_finite_creation_rejects_listener_and_accepted() {
    for variant in 0..9 {
        let mut t = cross_channel();
        match variant {
            0 => t.fresh_stream_profiles.clear(),
            1 => t.channel_socket_classes.pop().map(|_| ()).unwrap(),
            2 => t.fresh_send_timeouts.clear(),
            3 => t.fresh_stream_profiles[0].normalization.system_rmem_max = 0,
            4 => t.channel_socket_classes[1].key.domain = 10,
            5 => t.channels[0].role = NetworkEndpointRoleV2::Listener,
            6 => {
                t.channels[1].role = NetworkEndpointRoleV2::Accepted;
                t.channels[1].accepted_from = Some(NetworkChannelId(1));
            }
            7 => t.fresh_send_timeouts[0].timeout = ReceiveTimeoutV3::FiniteTicks(0),
            8 => t.channels[0].transport = NetworkTransportV2::UnixStream,
            _ => unreachable!(),
        }
        if matches!(variant, 5 | 6) {
            // Keep the original role/accepted_from mutations, but repair the
            // old fixture's now-invalid V2 relationship and Connect rows so
            // this refusal reaches V4's finite creation guard itself.
            t.channels[0].role = NetworkEndpointRoleV2::Listener;
            t.channels[0].peer_address = None;
            t.inputs.clear();
            t.outputs.clear();
            nodes(&mut t).clear();
            t.native_receive_observations.clear();
        }
        let expected = if matches!(variant, 5 | 6 | 8) {
            Invalid::UnsupportedCreation
        } else {
            Invalid::InvalidProfiles
        };
        assert_eq!(t.validate(), Err(expected), "variant {variant}");
    }
}

#[test]
fn zero_length_stream_and_ancillary_rows_cannot_become_numeric_progress() {
    for event in [
        NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: vec![],
        },
        NetworkOutputKindV2::StreamMessage {
            stream_offset: 0,
            bytes: vec![],
            ancillary: NetworkAncillaryDataV2 {
                bytes: vec![],
                objects: vec![],
                truncated: false,
            },
            message_flags: 0,
        },
    ] {
        let mut t = cross_channel();
        t.outputs[0].event = event;
        assert_eq!(
            t.validate(),
            Err(Invalid::Payload(
                NetworkTraceValidationError::EmptyByteChunk
            ))
        );
    }
}

#[test]
fn explicit_v4_roundtrip_preserves_data_through_typed_and_umbrella_readers() {
    let trace = cross_channel();
    let mut bytes = vec![];
    trace.write_framed(&mut bytes).unwrap();
    assert_eq!(&bytes[..16], &NETWORK_TRACE_MAGIC);
    assert_eq!(&bytes[16..20], &4u32.to_le_bytes());
    assert_eq!(
        NetworkTraceV4::read_framed(Cursor::new(&bytes)).unwrap(),
        trace
    );
    assert_eq!(
        NetworkTrace::read_framed(Cursor::new(&bytes)).unwrap(),
        NetworkTrace::V4(trace.clone())
    );
    let mut umbrella_bytes = Vec::new();
    NetworkTrace::V4(trace.clone())
        .write_framed(&mut umbrella_bytes)
        .unwrap();
    assert_eq!(umbrella_bytes, bytes);
    for unknown in [5u32, 99] {
        let mut unknown_bytes = bytes.clone();
        unknown_bytes[16..20].copy_from_slice(&unknown.to_le_bytes());
        assert!(
            matches!(NetworkTrace::read_framed(Cursor::new(unknown_bytes)),
            Err(NetworkTraceCodecError::UnsupportedVersion(actual)) if actual == unknown)
        );
    }
    let mut json = serde_json::to_value(&trace).unwrap();
    json["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<NetworkTraceV4>(json).is_err());
    let mut json = serde_json::to_value(&trace).unwrap();
    json["release_model"] = serde_json::json!({"invented_model":{"nodes":[]}});
    assert!(serde_json::from_value::<NetworkTraceV4>(json).is_err());
}

#[test]
fn v4_codec_rejects_truncation_trailing_bytes_unknown_versions_and_large_claims() {
    let mut bytes = vec![];
    cross_channel().write_framed(&mut bytes).unwrap();
    for end in 0..bytes.len() {
        assert!(
            NetworkTraceV4::read_framed(Cursor::new(&bytes[..end])).is_err(),
            "end {end}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(trailing)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::TrailingData
        ))
    ));
    let mut payload_trailing = bytes.clone();
    payload_trailing.push(0);
    let len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) + 1;
    payload_trailing[20..28].copy_from_slice(&len.to_le_bytes());
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(payload_trailing)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::TrailingPayloadData
        ))
    ));
    let mut unknown = bytes.clone();
    unknown[16..20].copy_from_slice(&99u32.to_le_bytes());
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(unknown)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::UnsupportedVersion(99)
        ))
    ));
    let mut oversized = bytes[..28].to_vec();
    oversized[20..28].copy_from_slice(&(MAX_NETWORK_TRACE_PAYLOAD_BYTES + 1).to_le_bytes());
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(oversized)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::TooLarge
        ))
    ));
}

#[test]
fn legacy_empty_v2_v3_frames_keep_exact_original_wire_bytes() {
    let history = NetworkTraceV2 {
        epoch: Utc.timestamp_opt(0, 0).unwrap(),
        channels: vec![],
        inputs: vec![],
        outputs: vec![],
    };
    let mut expected = b"HERMIT-NET-TRACE".to_vec();
    expected.extend_from_slice(&[2, 0, 0, 0, 24, 0, 0, 0, 0, 0, 0, 0, 20]);
    expected.extend_from_slice(b"1970-01-01T00:00:00Z");
    expected.extend_from_slice(&[0, 0, 0]);
    let mut actual = vec![];
    history.write_framed(&mut actual).unwrap();
    assert_eq!(actual, expected);
    let legacy = NetworkTraceV3 {
        history,
        receive_model: ReceiveModelV1::DeclaredCopyUnitsV1 { units: vec![] },
        fresh_stream_profiles: vec![],
        receive_environment: ReceiveEnvironmentV3::SingleRecorderNamespaceV1,
        channel_socket_classes: vec![],
    };
    expected[16] = 3;
    expected[20] = 29;
    expected.extend_from_slice(&[0, 0, 0, 0, 0]);
    actual.clear();
    legacy.write_framed(&mut actual).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        NetworkTrace::read_framed(Cursor::new(actual)).unwrap(),
        NetworkTrace::V3(legacy)
    );
}

#[test]
fn entry_frontier_uses_latest_prefix_and_keeps_retired_channel_milestones() {
    let t = cross_channel();
    assert_eq!(
        t.entry_frontier(NetworkReceiveEntryCutV4(5)).unwrap(),
        vec![
            NetworkReleaseNodeIdV4(1),
            NetworkReleaseNodeIdV4(3),
            NetworkReleaseNodeIdV4(4)
        ]
    );
    assert_eq!(
        t.entry_frontier(NetworkReceiveEntryCutV4(7)).unwrap(),
        vec![
            NetworkReleaseNodeIdV4(1),
            NetworkReleaseNodeIdV4(3),
            NetworkReleaseNodeIdV4(6)
        ]
    );
    assert_eq!(
        t.entry_frontier(NetworkReceiveEntryCutV4(8)),
        Err(Invalid::InvalidEntryCut)
    );
    let t = zero_datagram(true);
    assert_eq!(
        t.entry_frontier(NetworkReceiveEntryCutV4(4)).unwrap(),
        (0..4).map(NetworkReleaseNodeIdV4).collect::<Vec<_>>()
    );
}

#[test]
fn many_typed_datagram_producers_require_complete_output_coverage() {
    let mut t = empty();
    add_channel(&mut t, 1, true);
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::DatagramSetup,
        },
        &[],
    );
    for sequence in 0..10_000 {
        t.outputs.push(NetworkOutputEventV2 {
            channel: NetworkChannelId(1),
            event: NetworkOutputKindV2::Datagram(datagram(sequence, b"")),
        });
        progress(
            &mut t,
            1,
            NetworkProgressV4::DatagramPrefix {
                completed: sequence + 1,
            },
            &[],
        );
    }
    progress(&mut t, 1, NetworkProgressV4::Retired, &[]);
    t.validate().unwrap();
    t.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::Datagram(datagram(10_000, b"unrepresented")),
    });
    assert_eq!(t.validate(), Err(Invalid::MissingProducer));
}

#[test]
fn consumed_cursor_cannot_repeat_a_physical_copy_across_observations() {
    let mut t = cross_channel();
    let mut first = observation(2, 2);
    first.fragments[0].disposition = NetworkNativeCopyDispositionV4::Consume;
    first.fragments[0].physical_after = 2;
    let mut second = observation(2, 3);
    second.ordinal = 1;
    second.stream_offset = 2;
    second.fragments[0].requested = 5;
    second.fragments[0].copied = 5;
    second.fragments[0].disposition = NetworkNativeCopyDispositionV4::Consume;
    second.fragments[0].physical_after = 5;
    t.native_receive_observations = vec![first, second];
    assert_eq!(t.validate(), Err(Invalid::InvalidNativeObservation));
}

#[test]
fn consumed_bytes_cannot_escape_the_complete_canonical_payload() {
    let mut t = cross_channel();
    let fragment = &mut t.native_receive_observations[0].fragments[0];
    fragment.disposition = NetworkNativeCopyDispositionV4::Consume;
    fragment.requested = 20;
    fragment.copied = 20;
    fragment.physical_after = 20;
    assert_eq!(t.validate(), Err(Invalid::InvalidNativeObservation));
}

#[test]
fn observe_cannot_regress_the_actual_consumed_cursor() {
    let mut t = cross_channel();
    let mut first = observation(2, 2);
    first.fragments[0].disposition = NetworkNativeCopyDispositionV4::Consume;
    first.fragments[0].physical_after = 2;
    let mut second = observation(2, 3);
    second.ordinal = 1;
    second.stream_offset = 2;
    second.fragments[0].stream_offset = 2;
    t.native_receive_observations = vec![first, second];
    assert_eq!(t.validate(), Err(Invalid::InvalidNativeObservation));
}

#[test]
fn whole_skb_geometry_truthfully_represents_a_linear_head_to_fragment_copy() {
    let mut t = cross_channel();
    let fragment = &mut t.native_receive_observations[0].fragments[0];
    // skb len=32 and data_len=24: its linear head contains eight bytes.
    // This five-byte copy starts at head byte four and reaches the first frag.
    fragment.source_offset = 4;
    fragment.available = 28;
    fragment.nonlinear_length = 24;
    assert_eq!(t.validate(), Ok(()));
}

#[test]
fn successive_observations_preserve_the_actual_channel_cursor() {
    for disposition in [
        NetworkNativeCopyDispositionV4::Consume,
        NetworkNativeCopyDispositionV4::Observe,
    ] {
        let mut t = cross_channel();
        let mut first = observation(2, 2);
        first.fragments[0].disposition = NetworkNativeCopyDispositionV4::Consume;
        first.fragments[0].physical_after = 2;
        let mut second = observation(2, 3);
        second.ordinal = 1;
        second.stream_offset = 2;
        let f = &mut second.fragments[0];
        f.stream_offset = 2;
        f.disposition = disposition;
        f.physical_before = 2;
        f.physical_after = if disposition == NetworkNativeCopyDispositionV4::Consume {
            5
        } else {
            2
        };
        t.native_receive_observations = vec![first, second];
        assert_eq!(t.validate(), Ok(()));
        for wrong_before in [0, 1, 3] {
            let mut bad = t.clone();
            bad.native_receive_observations[1].fragments[0].physical_before = wrong_before;
            assert_eq!(bad.validate(), Err(Invalid::InvalidNativeObservation));
        }
    }
}

#[test]
fn a_complete_copy_may_include_prior_observed_bytes_without_repeating_a_consume() {
    let mut t = cross_channel();
    let first = observation(2, 2);
    let mut second = observation(2, 3);
    second.ordinal = 1;
    second.stream_offset = 2;
    let f = &mut second.fragments[0];
    f.requested = 5;
    f.copied = 5;
    f.disposition = NetworkNativeCopyDispositionV4::Consume;
    f.physical_after = 5;
    t.native_receive_observations = vec![first, second];
    assert_eq!(t.validate(), Ok(()));
    // The first copy observed those bytes; claiming it consumed them would
    // repeat their consumption in the second complete physical copy.
    t.native_receive_observations[0].fragments[0].disposition =
        NetworkNativeCopyDispositionV4::Consume;
    t.native_receive_observations[0].fragments[0].physical_after = 2;
    assert_eq!(t.validate(), Err(Invalid::InvalidNativeObservation));
}

#[test]
fn independent_channels_start_with_independent_zero_physical_cursors() {
    let mut t = cross_channel();
    input(
        &mut t,
        1,
        NetworkInputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"ack".to_vec(),
        },
        &[1, 3, 6],
    );
    let b = &mut t.native_receive_observations[0].fragments[0];
    b.disposition = NetworkNativeCopyDispositionV4::Consume;
    b.physical_after = 5;
    let mut a = observation(1, 3);
    a.ordinal = 1;
    a.fragments[0].disposition = NetworkNativeCopyDispositionV4::Consume;
    a.fragments[0].physical_after = 3;
    t.native_receive_observations.push(a);
    assert_eq!(t.validate(), Ok(()));
    t.native_receive_observations[1].fragments[0].physical_before = 5;
    assert_eq!(t.validate(), Err(Invalid::InvalidNativeObservation));
}

#[test]
fn first_copy_cannot_invent_an_unrecorded_initial_consumption() {
    let good = cross_channel();
    assert_eq!(good.validate(), Ok(()));
    for disposition in [
        NetworkNativeCopyDispositionV4::Observe,
        NetworkNativeCopyDispositionV4::Consume,
    ] {
        let mut bad = good.clone();
        let f = &mut bad.native_receive_observations[0].fragments[0];
        f.disposition = disposition;
        f.physical_before = 1;
        f.physical_after = if disposition == NetworkNativeCopyDispositionV4::Consume {
            6
        } else {
            1
        };
        assert_eq!(bad.validate(), Err(Invalid::InvalidNativeObservation));
    }
}

#[test]
fn whole_skb_head_fragment_boundaries_keep_exact_storage_bounds() {
    for (source, nonlinear) in [(0, 0), (4, 24), (8, 24), (27, 24), (0, 32)] {
        let mut t = cross_channel();
        let f = &mut t.native_receive_observations[0].fragments[0];
        f.source_offset = source;
        f.available = 32 - source;
        f.nonlinear_length = nonlinear;
        assert_eq!(
            t.validate(),
            Ok(()),
            "source={source} nonlinear={nonlinear}"
        );
    }
    for (source, available, nonlinear) in [(28, 4, 24), (4, 27, 24), (4, 28, 33)] {
        let mut bad = cross_channel();
        let f = &mut bad.native_receive_observations[0].fragments[0];
        f.source_offset = source;
        f.available = available;
        f.nonlinear_length = nonlinear;
        assert_eq!(bad.validate(), Err(Invalid::InvalidNativeObservation));
    }
}

#[test]
fn native_copy_coordinate_overflow_has_an_explicit_error_class() {
    let good = cross_channel();
    assert_eq!(good.validate(), Ok(()));
    let mut bad = good;
    // This reaches copied_after's checked addition before layout/coverage:
    // all lengths and the payload itself remain otherwise valid.
    bad.native_receive_observations[0].fragments[0].stream_offset = u64::MAX;
    assert_eq!(bad.validate(), Err(Invalid::Overflow));
}

fn readiness_history() -> NetworkTraceV4 {
    let mut t = empty();
    add_channel(&mut t, 1, true);
    progress(
        &mut t,
        1,
        NetworkProgressV4::Established {
            source: NetworkEstablishmentV4::DatagramSetup,
        },
        &[],
    );
    for _ in 0..3 {
        input(
            &mut t,
            1,
            NetworkInputKindV2::Readiness(NetworkReadinessV2::default()),
            &[0],
        );
    }
    t
}

#[test]
fn per_channel_entry_cut_cannot_regress_even_when_the_frontier_is_unchanged() {
    let good = readiness_history();
    assert_eq!(good.validate(), Ok(()));
    assert_eq!(
        good.entry_frontier(NetworkReceiveEntryCutV4(1)),
        good.entry_frontier(NetworkReceiveEntryCutV4(3))
    );
    let mut bad = good;
    bad.inputs[2].release.receive_entry_cut = NetworkReceiveEntryCutV4(1);
    assert_eq!(bad.validate(), Err(Invalid::InvalidEntryCut));
}

#[test]
fn release_time_is_monotonic_and_never_precedes_the_epoch() {
    let mut good = readiness_history();
    let epoch = good.epoch_global_time().unwrap().as_nanos();
    for (i, event) in good.inputs.iter_mut().enumerate() {
        event.release.not_before_global_time = LogicalTime::from_nanos(epoch + i as u64);
    }
    assert_eq!(good.validate(), Ok(()));
    let mut backwards = good.clone();
    backwards.inputs[2].release.not_before_global_time = LogicalTime::from_nanos(epoch);
    assert_eq!(backwards.validate(), Err(Invalid::InvalidEntryCut));
    let mut before_epoch = good;
    before_epoch.inputs[0].release.not_before_global_time = LogicalTime::from_nanos(epoch - 1);
    assert_eq!(
        before_epoch.validate(),
        Err(Invalid::Payload(
            NetworkTraceValidationError::ReleaseBeforeEpoch
        ))
    );
}

#[test]
fn readiness_requires_an_actual_establishment_producer() {
    assert_eq!(readiness_history().validate(), Ok(()));
    let mut bad = empty();
    add_channel(&mut bad, 1, true);
    input(
        &mut bad,
        1,
        NetworkInputKindV2::Readiness(NetworkReadinessV2::default()),
        &[],
    );
    assert_eq!(bad.validate(), Err(Invalid::MissingProducer));
}

#[test]
fn ipv6_profiles_bind_the_actual_address_family_and_preserve_roundtrip() {
    let mut t = cross_channel();
    t.fresh_stream_profiles[0].key.domain = 10;
    t.fresh_send_timeouts[0].key.domain = 10;
    for binding in &mut t.channel_socket_classes {
        binding.key.domain = 10;
    }
    for channel in &mut t.channels {
        channel.peer_address = Some(NetworkAddressV2::Inet6 {
            address: [
                0x20,
                1,
                0xd,
                0xb8,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                channel.id.0 as u8,
            ],
            port: 443,
            flowinfo: 7,
            scope_id: 2,
        });
    }
    assert_eq!(t.validate(), Ok(()));
    let mut bytes = vec![];
    t.write_framed(&mut bytes).unwrap();
    assert_eq!(NetworkTraceV4::read_framed(Cursor::new(bytes)).unwrap(), t);
    let mut bad = t;
    bad.channels[1].peer_address = Some(NetworkAddressV2::Inet4 {
        address: [192, 0, 2, 2],
        port: 443,
    });
    assert_eq!(bad.validate(), Err(Invalid::InvalidProfiles));
}

#[test]
fn unix_datagram_exact_zero_output_has_typed_coverage_and_exact_address_presence() {
    let mut t = zero_datagram(false);
    for channel in &mut t.channels {
        channel.transport = NetworkTransportV2::UnixDatagram;
        channel.peer_address = Some(NetworkAddressV2::UnixAbstract(vec![channel.id.0 as u8]));
    }
    let mut d = datagram(0, b"");
    d.destination = t.channels[0].peer_address.clone();
    t.outputs[0].event = NetworkOutputKindV2::DatagramExact(NetworkDatagramExactV2 {
        datagram: d,
        source_length: None,
        destination_length: Some(4),
    });
    let mut received = datagram(0, b"response");
    received.source = t.channels[1].peer_address.clone();
    t.inputs[0].event = NetworkInputKindV2::DatagramExact(NetworkDatagramExactV2 {
        datagram: received,
        source_length: Some(4),
        destination_length: None,
    });
    assert_eq!(t.validate(), Ok(()));
    let mut bytes = vec![];
    t.write_framed(&mut bytes).unwrap();
    assert_eq!(NetworkTraceV4::read_framed(Cursor::new(bytes)).unwrap(), t);
    for variant in 0..3 {
        let mut bad = t.clone();
        match variant {
            0 => {
                let NetworkOutputKindV2::DatagramExact(exact) = &mut bad.outputs[0].event else {
                    unreachable!()
                };
                exact.destination_length = None;
            }
            1 => {
                let NetworkInputKindV2::DatagramExact(exact) = &mut bad.inputs[0].event else {
                    unreachable!()
                };
                exact.source_length = None;
            }
            2 => {
                nodes(&mut bad)[2].kind = NetworkReleaseNodeKindV4::Progress {
                    channel: NetworkChannelId(1),
                    milestone: NetworkProgressV4::DatagramPrefix { completed: 0 },
                };
            }
            _ => unreachable!(),
        }
        assert_eq!(
            bad.validate(),
            Err(if variant == 2 {
                Invalid::InvalidProgress
            } else {
                Invalid::Payload(NetworkTraceValidationError::AddressLengthMismatch)
            })
        );
    }
}

#[test]
fn v4_umbrella_and_typed_decoder_preserve_exact_refusal_classes() {
    let mut frame = Vec::new();
    cross_channel().write_framed(&mut frame).unwrap();
    for end in 0..frame.len() {
        assert!(
            matches!(
                NetworkTraceV4::read_framed(Cursor::new(&frame[..end])),
                Err(NetworkTraceCodecErrorV4::Frame(
                    NetworkTraceCodecError::Truncated
                ))
            ),
            "typed {end}"
        );
        assert!(
            matches!(
                NetworkTrace::read_framed(Cursor::new(&frame[..end])),
                Err(NetworkTraceCodecError::Truncated)
            ),
            "umbrella {end}"
        );
    }
    let mut trailing = frame.clone();
    trailing.push(0);
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&trailing)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::TrailingData
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&trailing)),
        Err(NetworkTraceCodecError::TrailingData)
    ));
    let payload_length = u64::from_le_bytes(frame[20..28].try_into().unwrap());
    trailing[20..28].copy_from_slice(&(payload_length + 1).to_le_bytes());
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&trailing)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::TrailingPayloadData
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&trailing)),
        Err(NetworkTraceCodecError::TrailingPayloadData)
    ));
    let mut oversized = frame[..28].to_vec();
    oversized[20..28].copy_from_slice(&(MAX_NETWORK_TRACE_PAYLOAD_BYTES + 1).to_le_bytes());
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&oversized)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::TooLarge
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&oversized)),
        Err(NetworkTraceCodecError::TooLarge)
    ));
    let mut bad_magic = frame.clone();
    bad_magic[0] ^= 1;
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&bad_magic)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::BadMagic
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&bad_magic)),
        Err(NetworkTraceCodecError::BadMagic)
    ));
}

#[test]
fn v4_umbrella_and_typed_payloads_keep_bounded_decode_and_structural_validation() {
    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = NETWORK_TRACE_MAGIC.to_vec();
        bytes.extend_from_slice(&NETWORK_TRACE_VERSION_V4.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
    // The first field is the epoch string. Its decoded length claim exceeds
    // the limit despite a tiny complete frame; no large allocation is needed.
    let claim = bincode::serde::encode_to_vec(
        MAX_NETWORK_TRACE_PAYLOAD_BYTES + 1,
        bincode::config::standard(),
    )
    .unwrap();
    let oversized_claim = frame(&claim);
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&oversized_claim)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::Decode(bincode::error::DecodeError::LimitExceeded)
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&oversized_claim)),
        Err(NetworkTraceCodecError::Decode(
            bincode::error::DecodeError::LimitExceeded
        ))
    ));
    let malformed = frame(&[255]);
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&malformed)),
        Err(NetworkTraceCodecErrorV4::Frame(
            NetworkTraceCodecError::Decode(_)
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&malformed)),
        Err(NetworkTraceCodecError::Decode(_))
    ));
    let mut invalid = cross_channel();
    nodes(&mut invalid)[0].id = NetworkReleaseNodeIdV4(2);
    assert_eq!(invalid.validate(), Err(Invalid::NonCanonicalNode));
    let payload = bincode::serde::encode_to_vec(&invalid, bincode::config::standard()).unwrap();
    let invalid_frame = frame(&payload);
    assert!(matches!(
        NetworkTraceV4::read_framed(Cursor::new(&invalid_frame)),
        Err(NetworkTraceCodecErrorV4::Validation(
            Invalid::NonCanonicalNode
        ))
    ));
    assert!(matches!(
        NetworkTrace::read_framed(Cursor::new(&invalid_frame)),
        Err(NetworkTraceCodecError::ValidationV4(
            Invalid::NonCanonicalNode
        ))
    ));
    let mut typed_output = Vec::new();
    assert!(matches!(
        invalid.write_framed(&mut typed_output),
        Err(NetworkTraceCodecErrorV4::Validation(
            Invalid::NonCanonicalNode
        ))
    ));
    assert!(typed_output.is_empty());
    let mut umbrella_output = Vec::new();
    assert!(matches!(
        NetworkTrace::V4(invalid).write_framed(&mut umbrella_output),
        Err(NetworkTraceCodecError::ValidationV4(
            Invalid::NonCanonicalNode
        ))
    ));
    assert!(umbrella_output.is_empty());
}

#[test]
fn finite_channel_creation_accepts_each_supported_transport_with_valid_v2_premises() {
    let payload = NetworkTraceV2 {
        epoch: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
        channels: [
            NetworkTransportV2::Tcp,
            NetworkTransportV2::Udp,
            NetworkTransportV2::UnixDatagram,
        ]
        .into_iter()
        .enumerate()
        .map(|(n, transport)| NetworkChannelV2 {
            id: NetworkChannelId(n as u64 + 1),
            transport,
            role: if transport == NetworkTransportV2::Tcp {
                NetworkEndpointRoleV2::OutboundClient
            } else {
                NetworkEndpointRoleV2::Datagram
            },
            local_address: None,
            peer_address: if transport == NetworkTransportV2::UnixDatagram {
                None
            } else {
                Some(NetworkAddressV2::Inet4 {
                    address: [192, 0, 2, 2],
                    port: 443,
                })
            },
            accepted_from: None,
        })
        .collect(),
        inputs: vec![],
        outputs: vec![],
    };
    assert_eq!(payload.validate(), Ok(()));
    for channel in &payload.channels {
        assert_eq!(validate_channel_creation(channel), Ok(()));
    }
}

#[test]
fn finite_channel_creation_rejects_valid_accepted_child_independently_of_listener() {
    let listener = NetworkChannelV2 {
        id: NetworkChannelId(1),
        transport: NetworkTransportV2::Tcp,
        role: NetworkEndpointRoleV2::Listener,
        local_address: None,
        peer_address: None,
        accepted_from: None,
    };
    let child = NetworkChannelV2 {
        id: NetworkChannelId(2),
        transport: NetworkTransportV2::Tcp,
        role: NetworkEndpointRoleV2::Accepted,
        local_address: None,
        peer_address: Some(NetworkAddressV2::Inet4 {
            address: [192, 0, 2, 2],
            port: 443,
        }),
        accepted_from: Some(listener.id),
    };
    let mut outbound = child.clone();
    outbound.id = NetworkChannelId(3);
    outbound.role = NetworkEndpointRoleV2::OutboundClient;
    outbound.accepted_from = None;
    let payload = NetworkTraceV2 {
        epoch: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
        channels: vec![listener.clone(), child.clone(), outbound.clone()],
        inputs: vec![],
        outputs: vec![],
    };
    assert_eq!(payload.validate(), Ok(()));
    // Only the Accepted child reaches this invocation. A Listener refusal
    // elsewhere in the trace cannot mask a missing Accepted restriction.
    assert_eq!(
        validate_channel_creation(&child),
        Err(Invalid::UnsupportedCreation)
    );
    assert_eq!(
        validate_channel_creation(&listener),
        Err(Invalid::UnsupportedCreation)
    );
    assert_eq!(validate_channel_creation(&outbound), Ok(()));
}

#[test]
fn shared_attempt_policy_preserves_graph_checks_and_distinct_framing() {
    let mut legacy = empty();
    add_channel(&mut legacy, 1, false);
    input(&mut legacy, 1, NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected), &[]);
    progress(&mut legacy, 1, NetworkProgressV4::Established {
        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
    }, &[0]);
    legacy.validate().unwrap();
    let mut shared = legacy.clone();
    shared.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
        nodes: legacy.release_model.nodes().to_vec(),
    };
    let mut legacy_bytes = Vec::new();
    let mut shared_bytes = Vec::new();
    legacy.write_framed(&mut legacy_bytes).unwrap();
    shared.write_framed(&mut shared_bytes).unwrap();
    assert_ne!(legacy_bytes, shared_bytes);
    assert_eq!(NetworkTraceV4::read_framed(Cursor::new(shared_bytes)).unwrap(), shared);
    for policy in [false, true] {
        let mut invalid = if policy { shared.clone() } else { legacy.clone() };
        let entries = match &mut invalid.release_model {
            NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes }
            | NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } => nodes,
        };
        entries[0].id = NetworkReleaseNodeIdV4(7);
        assert_eq!(invalid.validate(), Err(Invalid::NonCanonicalNode));
        let mut invalid = if policy { shared.clone() } else { legacy.clone() };
        invalid.inputs[0].release.receive_entry_cut = NetworkReceiveEntryCutV4(3);
        assert_eq!(invalid.validate(), Err(Invalid::InvalidEntryCut));
    }
}
