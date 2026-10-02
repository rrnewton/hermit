//! Component controls: provider and backend EINPROGRESS are explicit premises;
//! the retained-pin state/peer queries, retirement, publisher and Replay engine
//! are real. These do not claim a native asynchronous Connect guest execution.

use super::*;

async fn completed_fixture(
    connected: bool,
    nonblocking: bool,
    returned: i64,
    observed: i64,
    change: ConnectCompletionChange,
) -> Fixture {
    let mut f = Fixture::new().await;
    if connected {
        f.connect();
    }
    if nonblocking {
        let flags = unsafe { libc::fcntl(f.client.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    f.client.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            },
            0
        );
    }
    // Actual connection state is established separately above. Preserve the
    // component's controlled provider/observer nature instead of labelling the
    // synthetic -115 as an actual kernel return from that blocking connect.
    f.returned = Some(returned);
    f.completed(change).unwrap();
    f.observe(InjectedSyscallEvent::Returned(observed), 0);
    assert!(!f.state.sched.lock().unwrap().backend_failed());
    assert_eq!(
        f.thread.original_connect.as_ref().unwrap().returned,
        Some(observed)
    );
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .controlled_connect_retirement_with_return(f.owner(), &f.admission, observed)
        .unwrap();
    f.close_pin();
    f.foreground().await;
    f
}

#[tokio::test]
async fn early_connect_preserves_einprogress_and_replays_distinct_completion() {
    let f = completed_fixture(
        true,
        true,
        -i64::from(libc::EINPROGRESS),
        -i64::from(libc::EINPROGRESS),
        ConnectCompletionChange::None,
    )
    .await;
    let result = f
        .state
        .publish_foreground_native_connected(f.tid, &f.thread, &f.admission);
    let trace = f.trace();
    let repeated = f
        .state
        .publish_foreground_native_connected(f.tid, &f.thread, &f.admission);
    let open_file = f.admission.arguments.binding.unwrap().open_file;
    f.finish();
    drop(f);
    result.unwrap();
    assert!(repeated.is_err());
    trace.validate().unwrap();
    assert_eq!(trace.inputs.len(), 2);
    assert_eq!(
        trace.inputs[0].event,
        NetworkInputKindV2::Connect(NetworkConnectionResultV2::Error(libc::EINPROGRESS))
    );
    assert_eq!(
        trace.inputs[1].event,
        NetworkInputKindV2::ConnectEstablished
    );
    assert_eq!(trace.inputs[0].release, trace.inputs[1].release);
    assert_eq!(trace.release_model.nodes().len(), 3);
    assert_eq!(
        trace.release_model.nodes()[2].kind,
        NetworkReleaseNodeKindV4::Progress {
            channel: trace.inputs[0].channel,
            milestone: NetworkProgressV4::Established {
                source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 1 },
            },
        }
    );
    let mut encoded = Vec::new();
    trace.write_framed(&mut encoded).unwrap();
    assert_eq!(
        NetworkTraceV4::read_framed(encoded.as_slice()).unwrap(),
        trace
    );

    let mut wrong_start = trace.clone();
    wrong_start.inputs[0].event = NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected);
    assert!(wrong_start.validate().is_err());
    let mut missing_completion = trace.clone();
    missing_completion.inputs[1].event = NetworkInputKindV2::Readiness(Default::default());
    assert!(missing_completion.validate().is_err());
    let mut duplicate_completion = trace.clone();
    duplicate_completion.inputs[0].event = NetworkInputKindV2::ConnectEstablished;
    assert!(duplicate_completion.validate().is_err());
    let legacy = detcore_model::network_trace::NetworkTraceV2 {
        epoch: trace.epoch,
        channels: trace.channels.clone(),
        inputs: trace
            .inputs
            .iter()
            .map(|input| detcore_model::network_trace::NetworkInputEventV2 {
                ordinal: input.ordinal,
                channel: input.channel,
                release: detcore_model::network_trace::NetworkReleaseV2 {
                    not_before_global_time: input.release.not_before_global_time,
                    after_transmitted_offset: 0,
                },
                event: input.event.clone(),
            })
            .collect(),
        outputs: vec![],
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
    let mut replay = NetworkReplayEngine::replay_native_receive(trace.clone()).unwrap();
    replay.bind(open_file, trace.inputs[0].channel).unwrap();
    assert!(replay.take_connection_outcome(open_file).unwrap().is_none());
    replay
        .release_eligible(trace.inputs[0].release.not_before_global_time)
        .unwrap();
    assert!(
        replay.finish().is_err(),
        "released completion cannot stand in for returned -115"
    );
    assert_eq!(
        replay.take_connection_outcome(open_file).unwrap(),
        Some(crate::network_replay::ConnectionOutcome::Connect(
            NetworkConnectionResultV2::Error(libc::EINPROGRESS)
        ))
    );
    assert!(replay.take_connection_outcome(open_file).unwrap().is_none());
    replay.finish().unwrap();
}

#[tokio::test]
async fn early_connect_refuses_pending_blocking_wrong_peer_and_other_return() {
    for (connected, nonblocking, returned, observed, change) in [
        (
            false,
            true,
            -i64::from(libc::EINPROGRESS),
            -i64::from(libc::EINPROGRESS),
            ConnectCompletionChange::None,
        ),
        (
            true,
            false,
            -i64::from(libc::EINPROGRESS),
            -i64::from(libc::EINPROGRESS),
            ConnectCompletionChange::None,
        ),
        (
            true,
            true,
            -i64::from(libc::EINPROGRESS),
            -i64::from(libc::EINPROGRESS),
            ConnectCompletionChange::Peer,
        ),
        (
            true,
            true,
            -i64::from(libc::ECONNREFUSED),
            -i64::from(libc::ECONNREFUSED),
            ConnectCompletionChange::None,
        ),
        (
            true,
            true,
            -i64::from(libc::EINPROGRESS),
            0,
            ConnectCompletionChange::None,
        ),
    ] {
        let f = completed_fixture(connected, nonblocking, returned, observed, change).await;
        let before = f.snapshot();
        let result = f
            .state
            .publish_foreground_native_connected(f.tid, &f.thread, &f.admission);
        let after = f.snapshot();
        let trace = f.trace();
        f.finish();
        drop(f);
        assert!(
            result.is_err(),
            "{connected}/{nonblocking}/{returned}/{change:?}"
        );
        assert_eq!(before, after);
        assert!(trace.inputs.is_empty());
        assert!(trace.release_model.nodes().is_empty());
    }
}
