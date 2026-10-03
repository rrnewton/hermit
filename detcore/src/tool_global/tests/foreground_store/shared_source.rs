//! Actual Global preparation/completion with a shared task census. Provider
//! birth replies, physical descriptor stand-ins and already joined source bytes
//! are explicit component premises; no backend stop/native read is claimed.
use detcore_model::network_trace::*;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::tool_global::native_source_read::PreparedNativeSource;

struct SharedFixture {
    f: ReplayIssuerFixture,
    // The retained typed birth and controlled responder outlive every attempt.
    _birth: Box<dyn std::any::Any>,
}

async fn selected() -> SharedFixture {
    let mut legacy = NetworkReplayEngine::controlled_replay_two_row_trace();
    let channel = legacy.channels[0].id;
    legacy.outputs.push(NetworkOutputEventV2 {
        channel,
        event: NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"abc".to_vec(),
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut legacy.release_model
    else {
        panic!("legacy fixture changed policy")
    };
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
    legacy.validate().unwrap();
    let mut trace = legacy.clone();
    trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
        nodes: legacy.release_model.nodes().to_vec(),
    };
    trace.validate().unwrap();
    let mut bytes = Vec::new();
    NetworkTrace::V4(legacy).write_framed(&mut bytes).unwrap();
    let mut config = Config {
        sequentialize_threads: true,
        epoch_explicit: true,
        epoch: trace.epoch,
        network_trace_input: Some(bytes),
        ..Config::default()
    };
    config.network_trace.policy = NetworkPolicy::Replay;
    let mut state = GlobalState::initialize(&config, false);
    // The production selector is intentionally unchanged. This separately
    // named constructor is selected before census, FD publication or effects;
    // no recorded history is relabelled and no old validator is bypassed.
    *state.network_engine.as_ref().unwrap().lock().unwrap() =
        NetworkReplayEngine::replay_shared_mm_attempts(trace.clone()).unwrap();
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let tid = Tid::from_raw(raw);
    let birth = ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, claim| {
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(root);
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        engine.fd_table_fixture_enable();
        engine
            .register_initial_census(root.association(), claim, root.owner().thread)
            .unwrap();
    })
    .await;
    let root = birth.parent.clone();
    let owner = root.owner();
    assert!(!root.is_sole_initial_root(owner));
    assert!(!birth.child.is_sole_initial_root(birth.child.owner()));
    state
        .sched
        .lock()
        .unwrap()
        .controlled_shared_birth_census(&root, &birth.child, &birth._birth);
    let metadata = birth.metadata.clone();
    let memory = birth.memory.clone();
    let (runtime, retained_birth) = birth.into_runtime_and_retention();
    state.network_runtime = Some(runtime);
    let tool: Detcore = Detcore::new(tid, &config);
    let mut thread = tool.init_thread_state(tid, None);
    thread.dettid = owner.thread;
    thread.mm_id = owner.mm;
    thread.detpid = Some(root.logical_process());
    thread.file_metadata = metadata;
    thread.memory_metadata = memory;
    state
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    tool.on_thread_state_ready(tid, &state, &thread).unwrap();
    state.global_time.lock().unwrap().update_global_time(
        owner.thread,
        thread.thread_logical_time.as_nanos(),
        thread.thread_logical_time.inherited_nanos(),
    );
    let pages = Pages::new();
    let mut guest = owned_read_guest(&config, &state, thread);
    let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Socket).await;
    {
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        engine
            .register_stream_socket(
                binding.open_file,
                trace.fresh_stream_profiles[0].key,
                crate::network_replay::NetworkStreamNamespace {
                    device: 7,
                    inode: 11,
                },
                None,
            )
            .unwrap();
        engine.bind(binding.open_file, channel).unwrap();
        let now = trace.epoch_global_time().unwrap();
        engine.release_eligible(now).unwrap();
        assert_eq!(
            engine.take_connection_outcome(binding.open_file).unwrap(),
            Some(crate::network_replay::ConnectionOutcome::Connect(
                NetworkConnectionResultV2::Connected
            ))
        );
        engine.release_eligible(now).unwrap();
    }
    grant_owned_read_foreground(&state, &mut guest).await;
    let reply = super::super::super::network_request(
        &mut guest,
        NetworkRequest::BeginOrdinaryFdRead {
            files: binding.slot.files,
            fd: binding.slot.fd,
        },
    )
    .await
    .unwrap();
    let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) = reply
    else {
        panic!("actual acknowledged reader refused")
    };
    assert_eq!(read.binding, Some(binding));
    assert!(read.control.is_some());
    let thread = guest.thread;
    {
        let scheduler = state.sched.lock().unwrap();
        state
            .network_runtime
            .as_ref()
            .unwrap()
            .with_shared_foreground_lineage(owner, |lineage| {
                assert_eq!(lineage.members().count(), 2);
                assert!(
                    scheduler
                        .foreground_native_observation(owner, &root)
                        .is_err()
                );
                scheduler.shared_mm_foreground_observation(owner, lineage)?;
                Ok(())
            })
            .unwrap();
    }
    SharedFixture {
        f: ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read: *read,
        },
        _birth: retained_birth,
    }
}

async fn prepare(f: &ReplayIssuerFixture) -> PreparedNativeSource {
    assert_eq!(
        f.state
            .replay_sendto_read_limit(f.tid, &f.thread, &f.read, 4)
            .unwrap(),
        3
    );
    let prepared = f
        .state
        .prepare_replay_transmit_source(
            f.tid,
            &f.thread,
            &f.read,
            (f.pages.at(128) as usize, 3, libc::MSG_NOSIGNAL as u32),
        )
        .await
        .unwrap();
    assert!(prepared.transferred_read());
    assert_eq!(prepared.address(), f.pages.at(128) as usize);
    prepared
}

fn assert_unconsumed_call(f: &ReplayIssuerFixture) {
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        engine.replay_transmit_offset(f.binding.open_file).unwrap(),
        0
    );
    assert!(
        engine
            .finish_fd_read(f.root.owner(), f.read.clone())
            .is_err()
    );
    assert!(engine.transmit_stream(f.binding.open_file, b"abc").is_err());
    assert!(engine.finish().is_err());
}

#[tokio::test]
async fn shared_source_global_transfers_exact_read_and_completes_without_another_turn() {
    let fixture = selected().await;
    let f = &fixture.f;
    let turn = f.state.sched.lock().unwrap().turn;
    let clock = f.state.global_time.lock().unwrap().as_nanos();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let prepared = prepare(f).await;
    assert_unconsumed_call(f);
    assert!(f.state.sched.try_lock().is_ok());
    assert!(f.thread.file_metadata.try_lock().is_ok());
    assert!(f.thread.memory_metadata.try_lock().is_ok());
    assert!(f.state.network_engine.as_ref().unwrap().try_lock().is_ok());
    assert!(runtime.with_foreground_prefix(&prefix, |_| Ok(())).is_err());
    let retention = prepared.retention();
    assert!(matches!(
        f.state
            .finish_replay_transmit_source(f.tid, &f.thread, prepared, b"abc".to_vec())
            .unwrap(),
        NetworkStreamTransmit::Accepted(3)
    ));
    assert!(runtime.with_foreground_prefix(&prefix, |_| Ok(())).is_err());
    drop(retention);
    runtime.with_foreground_prefix(&prefix, |_| Ok(())).unwrap();
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(
        engine
            .finish_fd_read(f.root.owner(), f.read.clone())
            .is_err()
    );
    assert_eq!(
        engine.replay_transmit_offset(f.binding.open_file).unwrap(),
        3
    );
    assert_eq!(f.state.sched.lock().unwrap().turn, turn);
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), clock);
}

#[tokio::test]
async fn shared_source_global_pretransfer_refusals_keep_the_original_reader() {
    for variant in 0..4 {
        let fixture = selected().await;
        let f = &fixture.f;
        let (tid, address, length) = match variant {
            0 => (f.tid, f.pages.at(128) as usize, 0),
            1 => (f.tid, f.pages.at(128) as usize, 4),
            2 => (
                Tid::from_raw(f.tid.as_raw() + 1),
                f.pages.at(128) as usize,
                3,
            ),
            _ => (f.tid, usize::MAX, 3),
        };
        assert!(
            f.state
                .prepare_replay_transmit_source(
                    tid,
                    &f.thread,
                    &f.read,
                    (address, length, libc::MSG_NOSIGNAL as u32)
                )
                .await
                .is_err(),
            "variant {variant}"
        );
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        engine
            .validate_fd_read_grant(f.root.owner(), &f.read)
            .unwrap();
        assert_eq!(
            engine.replay_transmit_offset(f.binding.open_file).unwrap(),
            0
        );
        engine
            .finish_fd_read(f.root.owner(), f.read.clone())
            .unwrap();
    }
}

#[tokio::test]
async fn shared_source_global_changed_grant_root_metadata_or_bytes_retains_call_and_frontier() {
    for variant in 0..7 {
        let mut fixture = selected().await;
        let f = &mut fixture.f;
        let prepared = prepare(f).await;
        let mut bytes = b"abc".to_vec();
        let mut tid = f.tid;
        match variant {
            0 => f
                .state
                .sched
                .lock()
                .unwrap()
                .controlled_shared_foreground_grant(&f.root),
            1 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage(),
            2 => tid = Tid::from_raw(tid.as_raw() + 1),
            3 => {
                f.thread.memory_metadata =
                    Arc::new(Mutex::new(crate::memory::MemoryMetadata::default()))
            }
            4 => {
                f.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .remove(&f.root.owner().thread);
            }
            5 => bytes = b"bad".to_vec(),
            _ => bytes = b"ab".to_vec(),
        }
        assert!(
            f.state
                .finish_replay_transmit_source(tid, &f.thread, prepared, bytes)
                .is_err(),
            "variant {variant}"
        );
        assert_unconsumed_call(f);
    }
}
