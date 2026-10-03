//! Valid immutable trace/model controls. These do not claim a native Poll scan.
use super::*;

fn trace() -> NetworkTraceV4 {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    trace.inputs.truncate(1);
    trace.native_receive_observations.clear();
    trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
        nodes: trace.release_model.nodes()[..2].to_vec(),
    };
    let epoch = trace.epoch_global_time().unwrap();
    for (low_water, mask, offset) in [(1, libc::POLLIN, 10), (3, 0, 20)] {
        let ordinal = trace.inputs.len() as u64;
        let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
        let prerequisites = trace.entry_frontier(cut).unwrap();
        trace.inputs.push(NetworkInputEventV4 {
            ordinal,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV4 {
                not_before_global_time: LogicalTime::from_nanos(epoch.as_nanos() + offset),
                receive_entry_cut: cut,
                prerequisites: prerequisites.clone(),
            },
            event: NetworkInputKindV2::SharedRawTcpPollState {
                consumed_prefix: 0,
                revents: mask,
                control_generation: 0,
                receive_low_water: low_water,
            },
        });
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut trace.release_model
        else {
            unreachable!()
        };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(cut.0),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites,
        });
    }
    trace.validate().unwrap();
    trace
}

#[test]
fn shared_poll_due_before_control_change_cannot_adopt_application_time_low_water() {
    let trace = trace();
    let epoch = trace.epoch_global_time().unwrap();
    let first = trace.inputs[1].release.not_before_global_time;
    let later = trace.inputs[2].release.not_before_global_time;
    let key = trace.fresh_stream_profiles[0].key;
    let mut engine = NetworkReplayEngine::replay_shared_mm_attempts(trace.clone()).unwrap();
    let thread = crate::types::DetTid::from_raw(61);
    let owner = NetworkStreamOwner {
        thread,
        mm: crate::types::MmId::initial(thread),
    };
    let file = OpenFileId::new_socket(thread, 0);
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
    engine.release_eligible(epoch).unwrap();
    engine.take_connection_outcome(file).unwrap().unwrap();
    assert_eq!(engine.next_native_release_time().unwrap(), Some(first));
    // first is already due at the caller's time, but has not been applied.
    // A real model socket-control transaction changes LOWAT before release.
    let now = LogicalTime::from_nanos(first.as_nanos() + 1);
    let lease = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
    engine
        .submit_stream_physical(
            owner,
            lease,
            NetworkStreamPhysicalEffect::SetSocketOption {
                option: NetworkStreamSocketOption::ReceiveLowWater(3),
            },
        )
        .unwrap();
    engine
        .confirm_stream_physical(
            owner,
            lease,
            NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
        )
        .unwrap();
    engine
        .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
        .unwrap();
    assert_eq!(
        engine.channels[&NetworkChannelId(1)].local_control_generation,
        0,
        "LOWAT does not increment this separate control generation"
    );
    assert_eq!(
        engine.shadow.as_ref().unwrap().sockets[&file]
            .options
            .receive_low_water,
        3
    );
    engine.release_eligible(now).unwrap();
    assert!(
        matches!(engine.shared_poll_sample(file), Err(NetworkReplayError::FdPublicationProtocol(message))
        if message == "shared Poll observation has stale recorded controls")
    );
    let EngineState::Native(native) = &engine.mode else {
        unreachable!()
    };
    let held = native.replay.as_ref().unwrap().poll.shared_latest[&NetworkChannelId(1)];
    assert_eq!(
        (
            held.ordinal,
            held.observed_at,
            held.receive_low_water,
            held.revents
        ),
        (1, first, 1, libc::POLLIN)
    );
    assert!(
        native
            .replay
            .as_ref()
            .unwrap()
            .poll
            .observation_at(NetworkChannelId(1), 0)
            .is_err(),
        "shared release cannot populate a legacy snapshot fallback"
    );
    engine.release_eligible(later).unwrap();
    assert_eq!(engine.shared_poll_sample(file).unwrap(), Some((later, 0)));
    assert_eq!(
        engine.native_trace_fixture(),
        trace,
        "release cannot rewrite recorded provenance"
    );
    assert!(
        engine
            .native_completed()
            .unwrap()
            .contains(&NetworkReleaseNodeIdV4(3))
    );
}

#[test]
fn shared_poll_snapshot_checks_generation_low_water_and_cut_independently() {
    let channel = NetworkChannelId(1);
    let mut state = PollSnapshots::default();
    let at = LogicalTime::from_nanos(7);
    state.apply(4, channel, 0, at, libc::POLLIN);
    assert_eq!(state.shared_observation_at(channel, 0, 0, 1).unwrap(), None);
    let sample = SharedPollSnapshot {
        ordinal: 5,
        consumed_prefix: 0,
        observed_at: at,
        revents: 0,
        control_generation: 0,
        receive_low_water: 1,
    };
    state.apply_shared(5, channel, sample);
    assert_eq!(
        state.shared_observation_at(channel, 0, 0, 1).unwrap(),
        Some(sample)
    );
    assert!(state.shared_observation_at(channel, 0, 1, 1).is_err());
    assert!(state.shared_observation_at(channel, 0, 0, 3).is_err());
    assert_eq!(state.shared_observation_at(channel, 1, 0, 1).unwrap(), None);
    assert_eq!(
        state
            .shared_observation_at(NetworkChannelId(2), 0, 0, 1)
            .unwrap(),
        None
    );
    assert_eq!(
        state.observation_at(channel, 0).unwrap(),
        (at, libc::POLLIN)
    );
    assert!(state.completed(4) && state.completed(5));
}
