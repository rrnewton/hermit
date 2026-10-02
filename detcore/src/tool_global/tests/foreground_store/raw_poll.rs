//! Real local reader/Normal issuer controls. Root/provider premises are the
//! existing controlled fixture, not ptrace or an end-to-end poll qualification.
use detcore_model::network_trace::*;

use super::*;

#[tokio::test]
async fn raw_poll_guest_dispatch_preserves_duplicate_alias_masks_and_closes_call() {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let now = trace.epoch_global_time().unwrap();
    let ordinal = trace.inputs.len() as u64;
    let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    trace.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: trace.channels[0].id,
        release: NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: prerequisites.clone(),
        },
        event: NetworkInputKindV2::RawTcpPollState {
            consumed_prefix: 0,
            revents: libc::POLLIN
                | libc::POLLRDNORM
                | libc::POLLOUT
                | libc::POLLWRNORM
                | libc::POLLPRI
                | libc::POLLRDHUP,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model;
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(cut.0),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    let (f, _) = ReplayIssuerFixture::new_trace(false, trace, true).await;
    let engine = f.state.network_engine.as_ref().unwrap();
    // Release only the setup reader. The actual dispatcher must acquire and
    // close its own readers and Call; no issuer shortcut is used below.
    engine
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read)
        .unwrap();
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = owned_read_guest(&f.config, &f.state, f.thread);
    guest.expose_local_global = true;
    let fd = f.binding.slot.fd;
    let alias = fd + 1;
    // Controlled successful dup2 premise, not a fresh socket creation. The
    // existing mutation validates the source and publishes an Alias effect.
    let crate::network_replay::NetworkFdMutationBegin::Admitted(admission) = engine
        .lock()
        .unwrap()
        .begin_fd_mutation(
            f.root.owner(),
            f.binding.slot.files,
            crate::network_replay::NetworkFdMutationKind::Alias {
                source_fd: fd,
                source: Some(f.binding),
                kind: crate::network_replay::NetworkFdInstallKind::Dup2,
                cloexec: false,
                destination: Some(alias),
                replaced: None,
            },
        )
        .unwrap()
    else {
        panic!("alias fixture requires its actual mutation admission")
    };
    {
        let mut engine = engine.lock().unwrap();
        engine
            .submit_fd_mutation(f.root.owner(), admission.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(
                f.root.owner(),
                admission.publication.permit,
                Ok(i64::from(alias)),
            )
            .unwrap();
    }
    assert_eq!(
        guest
            .thread
            .dup_fd(fd, alias, nix::fcntl::OFlag::empty())
            .unwrap(),
        None
    );
    tool.complete_network_fd_installation(&mut guest, Some(&admission), alias)
        .await
        .unwrap();
    assert_eq!(
        guest.thread.descriptor_binding(alias).unwrap().open_file,
        f.binding.open_file
    );
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before_delivery = engine
        .lock()
        .unwrap()
        .controlled_replay_delivery_state(f.binding.open_file);
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_turn = f.state.sched.lock().unwrap().turn;
    let inputs = [
        (fd, libc::POLLIN),
        (fd, libc::POLLPRI),
        (alias, libc::POLLRDNORM | libc::POLLWRNORM),
        (fd, libc::POLLRDHUP),
        (alias, 0),
        (-1, libc::POLLIN),
    ];
    let mut rows = inputs.map(|(fd, events)| libc::pollfd {
        fd,
        events,
        revents: -1,
    });
    let call = reverie::syscalls::Poll::new()
        .with_fds(reverie::syscalls::AddrMut::from_ptr(
            rows.as_mut_ptr().cast(),
        ))
        .with_nfds(rows.len() as libc::nfds_t)
        .with_timeout(0);
    // OwnedReadGuest permits typed local memory but panics on every physical
    // injection, timer and stack use. Removing native_poll_single_scan's
    // dispatch hits the unchanged exceptional-mask refusal, not this result.
    let result = tool.handle_network_io(&mut guest, call.into()).await;
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert!(matches!(result, Ok(4)), "actual Poll handler: {result:?}");
    assert_eq!(
        rows.map(|row| (row.fd, row.events, row.revents)),
        [
            (fd, libc::POLLIN, libc::POLLIN),
            (fd, libc::POLLPRI, libc::POLLPRI),
            (
                alias,
                libc::POLLRDNORM | libc::POLLWRNORM,
                libc::POLLRDNORM | libc::POLLWRNORM
            ),
            (fd, libc::POLLRDHUP, libc::POLLRDHUP),
            (alias, 0, 0),
            (-1, libc::POLLIN, 0),
        ]
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        before_delivery
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn raw_poll_replay_issuer_uses_actual_reader_without_physical_capture() {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let now = trace.epoch_global_time().unwrap();
    let ordinal = trace.inputs.len() as u64;
    let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    trace.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: trace.channels[0].id,
        release: NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: prerequisites.clone(),
        },
        event: NetworkInputKindV2::RawTcpPollState {
            consumed_prefix: 0,
            revents: libc::POLLPRI | libc::POLLRDHUP,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model;
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(cut.0),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    let (f, _) = ReplayIssuerFixture::new_trace(false, trace, true).await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let engine = f.state.network_engine.as_ref().unwrap();
    let call = f
        .state
        .begin_replay_poll_call(f.tid, &f.thread, f.read.clone())
        .unwrap();
    assert!(!call.physical_pin_required);
    assert_eq!(call.open_file, f.binding.open_file);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (1, 0, 0, 1)
    );
    for _ in 0..2 {
        assert_eq!(
            f.state
                .finish_foreground_poll(f.tid, &f.thread, call.id, None)
                .unwrap(),
            libc::POLLPRI | libc::POLLRDHUP
        );
    }
    let before = format!("{:?}", engine.lock().unwrap());
    assert!(
        f.state
            .begin_replay_poll_call(f.tid, &f.thread, f.read.clone())
            .is_err()
    );
    assert!(
        f.state
            .finish_foreground_poll(Tid::from_raw(f.tid.as_raw() + 1), &f.thread, call.id, None)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, call.id)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, call.id)
        .unwrap();
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn raw_poll_issuer_refuses_foreign_task_mm_metadata_and_reader() {
    for variant in 0..8 {
        let mut f = ReplayIssuerFixture::new().await;
        let owner = f.root.owner();
        let mut tid = f.tid;
        let mut read = f.read.clone();
        match variant {
            0 => tid = Tid::from_raw(f.tid.as_raw() + 1),
            1 => f.thread.mm_id = f.thread.mm_id.for_exec(owner.thread),
            2 => {
                let copied = f.thread.file_metadata.lock().unwrap().clone();
                f.thread.file_metadata = Arc::new(Mutex::new(copied));
            }
            3 => {
                f.thread.memory_metadata =
                    Arc::new(Mutex::new(crate::memory::MemoryMetadata::default()))
            }
            4 => read.binding = None,
            5 => read.control = None,
            6 => f.state.cfg.sequentialize_threads = false,
            7 => f.state.sched = Arc::new(Mutex::new(crate::scheduler::Scheduler::new(&f.config))),
            _ => unreachable!(),
        }
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        assert!(
            f.state
                .begin_replay_poll_call(tid, &f.thread, read)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(
            format!("{:?}", engine.lock().unwrap()),
            before,
            "variant {variant}"
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 1, 1, 0)
        );
        engine
            .lock()
            .unwrap()
            .finish_fd_read(owner, f.read)
            .unwrap();
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn raw_poll_record_entry_failure_releases_reader_before_any_capture() {
    let f = ReplayIssuerFixture::new_record().await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let mut older = f.config.clone();
    older.epoch -= chrono::Duration::seconds(1);
    *f.state.global_time.lock().unwrap() = crate::types::GlobalTime::new(&older);
    let failure = f
        .state
        .begin_native_poll_call(f.tid, &f.thread, f.read.clone())
        .await
        .unwrap_err();
    let NetworkRpcError::Internal(message) = failure.primary().clone() else {
        panic!("entry refusal")
    };
    assert_eq!(
        message,
        NetworkReplayError::from(NetworkTraceValidationError::ReleaseBeforeEpoch).to_string()
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}
