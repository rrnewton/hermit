//! Real local issuer and queue commit; the supplied bytes model an already
//! joined backend reply. No source-stop/native-memory/BPF success is claimed.
use detcore_model::network_trace::*;

use super::*;
use crate::network_replay::StreamTransmitOutcome;

async fn selected() -> ReplayIssuerFixture {
    selected_outputs(false).await
}

async fn selected_outputs(successor: bool) -> ReplayIssuerFixture {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let channel = trace.channels[0].id;
    trace.outputs.push(NetworkOutputEventV2 {
        channel,
        event: NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"abc".to_vec(),
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(nodes.len() as u64),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel,
            milestone: NetworkProgressV4::StreamPrefix {
                exclusive_offset: 3,
            },
        },
        prerequisites: vec![NetworkReleaseNodeIdV4(1)],
    });
    if successor {
        trace.outputs.push(NetworkOutputEventV2 {
            channel,
            event: NetworkOutputKindV2::StreamBytes {
                stream_offset: 3,
                bytes: b"abc".to_vec(),
            },
        });
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(nodes.len() as u64),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::StreamPrefix {
                    exclusive_offset: 6,
                },
            },
            prerequisites: vec![NetworkReleaseNodeIdV4(1)],
        });
    }
    trace.validate().unwrap();
    ReplayIssuerFixture::new_trace(false, trace, true).await.0
}

async fn prepare(
    f: &ReplayIssuerFixture,
) -> crate::tool_global::native_source_read::PreparedNativeSource {
    let limit = f
        .state
        .replay_sendto_read_limit(f.tid, &f.thread, &f.read, 4)
        .unwrap();
    assert_eq!(limit, 3);
    f.state
        .prepare_replay_transmit_source(
            f.tid,
            &f.thread,
            &f.read,
            (f.pages.at(128) as usize, limit, libc::MSG_NOSIGNAL as u32),
        )
        .await
        .unwrap()
}

fn finish_reader(f: &ReplayIssuerFixture) {
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
}

#[tokio::test]
async fn replay_source_issuer_commits_only_selected_prefix_on_original_turn() {
    let f = selected().await;
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let turn = f.state.sched.lock().unwrap().turn;
    let clock = f.state.global_time.lock().unwrap().as_nanos();
    let prepared = prepare(&f).await;
    assert_eq!(prepared.length(), 3);
    assert!(f.state.sched.try_lock().is_ok());
    assert!(f.thread.memory_metadata.try_lock().is_ok());
    assert!(f.thread.file_metadata.try_lock().is_ok());
    assert!(f.state.network_engine.as_ref().unwrap().try_lock().is_ok());
    assert!(runtime.with_foreground_prefix(&prefix, |_| Ok(())).is_err());
    let result = f
        .state
        .finish_replay_transmit_source(f.tid, &f.thread, prepared, b"abc".to_vec());
    finish_reader(&f);
    assert!(matches!(
        result.unwrap(),
        NetworkStreamTransmit::Accepted(3)
    ));
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .transmit_stream_read_limit(f.binding.open_file, 4)
            .is_err()
    );
    assert_eq!(f.state.sched.lock().unwrap().turn, turn);
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), clock);
    runtime.with_foreground_prefix(&prefix, |_| Ok(())).unwrap();
}

#[tokio::test]
async fn replay_source_issuer_changed_grant_or_identity_refuses_without_consumption() {
    for variant in 0..5 {
        let mut f = selected().await;
        let prepared = prepare(&f).await;
        let mut tid = f.tid;
        match variant {
            0 => f
                .state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(&f.root),
            1 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage(),
            2 => {
                tid = Tid::from_raw(tid.as_raw() + 1);
            }
            3 => {
                f.thread.memory_metadata =
                    Arc::new(Mutex::new(crate::memory::MemoryMetadata::default()));
            }
            _ => {
                f.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .remove(&f.root.owner().thread);
            }
        }
        let result =
            f.state
                .finish_replay_transmit_source(tid, &f.thread, prepared, b"abc".to_vec());
        finish_reader(&f);
        assert!(result.is_err(), "variant {variant}");
        assert_eq!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .transmit_stream_read_limit(f.binding.open_file, 4)
                .unwrap(),
            3
        );
    }
}

#[tokio::test]
async fn replay_source_issuer_short_long_or_wrong_bytes_leave_prefix_unconsumed() {
    for bytes in [vec![], b"ab".to_vec(), b"abc!".to_vec(), b"bad".to_vec()] {
        let f = selected().await;
        let prepared = prepare(&f).await;
        let result = f
            .state
            .finish_replay_transmit_source(f.tid, &f.thread, prepared, bytes);
        finish_reader(&f);
        assert!(result.is_err());
        assert_eq!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .transmit_stream_read_limit(f.binding.open_file, 4)
                .unwrap(),
            3
        );
    }
}

#[tokio::test]
async fn replay_source_issuer_equal_length_successor_is_not_original_selection() {
    let f = selected_outputs(true).await;
    let prepared = prepare(&f).await;
    let engine = f.state.network_engine.as_ref().unwrap();
    // Deliberate component mutation while the source is outstanding. The
    // successor has the same bytes and length; only its stream frontier differs.
    assert!(matches!(
        engine
            .lock()
            .unwrap()
            .transmit_stream(f.binding.open_file, b"abc")
            .unwrap(),
        StreamTransmitOutcome::Accepted(3)
    ));
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .transmit_stream_read_limit(f.binding.open_file, 4)
            .unwrap(),
        3
    );
    let result = f
        .state
        .finish_replay_transmit_source(f.tid, &f.thread, prepared, b"abc".to_vec());
    finish_reader(&f);
    assert!(result.is_err());
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .replay_transmit_offset(f.binding.open_file)
            .unwrap(),
        3
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .transmit_stream_read_limit(f.binding.open_file, 4)
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn replay_source_retention_blocks_existing_copy_exclusion_until_retirement() {
    let f = ReplayIssuerFixture::new().await;
    let owner = f.root.owner();
    let call = f
        .state
        .begin_replay_receive_call(f.tid, &f.thread, f.read.clone(), f.pages.at(128), 3)
        .unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let source = engine
        .lock()
        .unwrap()
        .reserve_replay_store_source(owner, call.id, 3)
        .unwrap()
        .unwrap();
    let source = crate::network_replay::ForegroundStoreSource::Replay(source);
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    // This is the runtime admission component, not a claim that one Tool
    // simultaneously acquired source and destination semantic admissions.
    let interval = runtime.reserve_replay_source_interval(&prefix).unwrap();
    let retained_backend = interval.keepalive();
    let (release, gate) = std::sync::mpsc::channel();
    // An actual join controls retirement of this component's retained owner;
    // this is not a fabricated stopped-task or Reverie source-read receipt.
    let worker = std::thread::spawn(move || gate.recv_timeout(std::time::Duration::from_secs(1)));
    drop(interval);
    let blocked = runtime
        .exclude_native_for_store(f.root.clone(), source.clone())
        .await;
    if let Ok(copy) = &blocked {
        runtime.finish_native_copy_exclusion(copy).unwrap();
    }
    let released = release.send(());
    let joined = worker.join();
    drop(retained_backend);
    let copy = runtime
        .exclude_native_for_store(f.root.clone(), source)
        .await
        .unwrap();
    runtime.finish_native_copy_exclusion(&copy).unwrap();
    assert!(released.is_ok());
    assert!(matches!(joined, Ok(Ok(()))));
    assert!(blocked.is_err());
    assert!(runtime.validate_native_copy_exclusion(&copy).is_err());
    // No guest store was attempted; keep the logical source unconsumed.
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file)
            .0,
        0
    );
}
