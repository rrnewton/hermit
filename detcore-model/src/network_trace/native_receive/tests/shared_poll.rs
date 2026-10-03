use super::*;

fn poll_trace(shared: bool) -> NetworkTraceV4 {
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
        &[0],
    );
    input(
        &mut trace,
        1,
        NetworkInputKindV2::RawTcpPollState {
            consumed_prefix: 0,
            revents: 0,
        },
        &[1],
    );
    if shared {
        trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
            nodes: trace.release_model.nodes().to_vec(),
        };
        trace.inputs[1].event = sample(0, 0, 0, 1);
    }
    trace
}
fn sample(
    consumed_prefix: u64,
    revents: i16,
    control_generation: u64,
    receive_low_water: u32,
) -> NetworkInputKindV2 {
    NetworkInputKindV2::SharedRawTcpPollState {
        consumed_prefix,
        revents,
        control_generation,
        receive_low_water,
    }
}

#[test]
fn shared_poll_provenance_is_immutable_and_policy_specific() {
    let shared = poll_trace(true);
    shared.validate().unwrap();
    let mut bytes = Vec::new();
    shared.write_framed(&mut bytes).unwrap();
    assert_eq!(
        NetworkTraceV4::read_framed(Cursor::new(bytes)).unwrap(),
        shared
    );
    let mut sole = poll_trace(false);
    sole.validate().unwrap();
    sole.inputs[1].event = shared.inputs[1].event.clone();
    assert_eq!(sole.validate(), Err(Invalid::InvalidNativeObservation));
    let mut missing = shared.clone();
    missing.inputs[1].event = NetworkInputKindV2::RawTcpPollState {
        consumed_prefix: 0,
        revents: 0,
    };
    assert_eq!(missing.validate(), Err(Invalid::InvalidNativeObservation));
    for low in [0, i32::MAX as u32 + 1, u32::MAX] {
        let mut bad = shared.clone();
        bad.inputs[1].event = sample(0, 0, 0, low);
        assert_eq!(bad.validate(), Err(Invalid::InvalidNativeObservation));
    }
    for generation in [0, 1, u64::MAX] {
        let mut valid = shared.clone();
        valid.inputs[1].event =
            sample(0, libc::POLLIN | libc::POLLERR, generation, i32::MAX as u32);
        valid.validate().unwrap();
    }
    for event in [sample(1, 0, 0, 1), sample(0, libc::POLLNVAL, 0, 1)] {
        let mut bad = shared.clone();
        bad.inputs[1].event = event;
        assert_eq!(
            bad.validate(),
            Err(Invalid::Payload(
                NetworkTraceValidationError::InvalidChannelRelationship
            ))
        );
    }
    let mut changed = shared.clone();
    changed.inputs[1].release.prerequisites.clear();
    let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } = &mut changed.release_model
    else {
        unreachable!()
    };
    nodes[2].prerequisites.clear();
    assert_eq!(changed.validate(), Err(Invalid::InvalidEntryFrontier));
}

#[test]
fn shared_poll_preserves_old_discriminants_and_legacy_refusal() {
    for (event, expected) in [
        (
            NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 0,
                revents: 0,
            },
            vec![10, 0, 0],
        ),
        (
            NetworkInputKindV2::SocketErrorRead {
                consumed_prefix: 0,
                errno: 0,
            },
            vec![11, 0, 0],
        ),
    ] {
        let bytes = bincode::serde::encode_to_vec(&event, bincode::config::standard()).unwrap();
        assert_eq!(bytes, expected);
        let (decoded, used): (NetworkInputKindV2, _) =
            bincode::serde::decode_from_slice(&expected, bincode::config::standard()).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(used, expected.len());
    }
    let shared = poll_trace(true);
    let legacy = NetworkTraceV2 {
        epoch: shared.epoch,
        channels: shared.channels.clone(),
        outputs: vec![],
        inputs: shared
            .inputs
            .iter()
            .map(|input| NetworkInputEventV2 {
                ordinal: input.ordinal,
                channel: input.channel,
                event: input.event.clone(),
                release: NetworkReleaseV2 {
                    not_before_global_time: shared.epoch_global_time().unwrap(),
                    after_transmitted_offset: 0,
                },
            })
            .collect(),
    };
    assert_eq!(
        legacy.validate(),
        Err(NetworkTraceValidationError::InvalidChannelRelationship)
    );
    let v3 = NetworkTraceV3 {
        history: legacy,
        receive_model: ReceiveModelV1::DeclaredCopyUnitsV1 { units: vec![] },
        fresh_stream_profiles: shared.fresh_stream_profiles.clone(),
        receive_environment: shared.receive_environment,
        channel_socket_classes: shared.channel_socket_classes.clone(),
    };
    assert!(v3.validate().is_err());
    let mut json = serde_json::to_value(&shared.inputs[1].event).unwrap();
    json["shared_raw_tcp_poll_state"]
        .as_object_mut()
        .unwrap()
        .remove("control_generation");
    assert!(serde_json::from_value::<NetworkInputKindV2>(json).is_err());
}

#[test]
fn shared_poll_control_change_does_not_separate_equal_time_observations() {
    for (generation, low_water, mask) in [(0, 1, 0), (1, 1, 0), (0, 3, libc::POLLIN)] {
        let mut trace = poll_trace(true);
        let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
        let prerequisites = trace.entry_frontier(cut).unwrap();
        trace.inputs.push(NetworkInputEventV4 {
            ordinal: 2,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV4 {
                not_before_global_time: trace.epoch_global_time().unwrap(),
                receive_entry_cut: cut,
                prerequisites: prerequisites.clone(),
            },
            event: sample(0, mask, generation, low_water),
        });
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut trace.release_model
        else {
            unreachable!()
        };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(cut.0),
            kind: NetworkReleaseNodeKindV4::Input { input_ordinal: 2 },
            prerequisites,
        });
        assert_eq!(trace.validate(), Err(Invalid::InvalidNativeObservation));
        trace.inputs[2].release.not_before_global_time =
            LogicalTime::from_nanos(trace.epoch_global_time().unwrap().as_nanos() + 1);
        trace.validate().unwrap();
    }
}
