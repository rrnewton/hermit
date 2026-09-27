//! Real scheduler/Global turn transport and existing paired-history join.
//! Native rows below are explicit component premises, never kernel evidence.
use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::controlled_epoll_metadata_fixture;
use crate::network_runtime::original_epoll_ctl::HistoricalPair;
use crate::tool_global::original_connect::EpollCtlTurn;
use crate::tool_local::FileMetadata;

async fn background_local(
    state: &GlobalState,
    tool: &Detcore,
    guest: &mut OwnedReadGuest<'_>,
    operation: ExternalOpId,
    epoll: bool,
) {
    let mut pending = std::pin::pin!(async {
        if epoll {
            tool.begin_original_epoll_ctl_wait(guest, operation).await
        } else {
            tool.begin_original_openat_wait(guest, operation).await
        }
    });
    assert!(futures::poll!(pending.as_mut()).is_pending());
    let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
    let request = selected.1.try_read().unwrap().unwrap();
    assert_eq!(request.resources.len(), 1);
    assert!(
        request
            .resources
            .contains_key(&ResourceID::BlockingExternalIO(operation))
    );
    assert!(request.fd_read.is_none());
    assert!(matches!(
        crate::scheduler::finish_selected_turn(
            state.sched.clone(),
            state.global_time.clone(),
            selected.0,
            selected.1,
            selected.2
        )
        .await,
        Err(crate::scheduler::SkipTurn)
    ));
    pending.await.unwrap();
}
async fn grant_ready(state: &GlobalState, expected: DetTid) {
    let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
    assert_eq!(selected.0, expected);
    assert!(
        crate::scheduler::finish_selected_turn(
            state.sched.clone(),
            state.global_time.clone(),
            selected.0,
            selected.1,
            selected.2
        )
        .await
        .is_ok()
    );
}
#[tokio::test]
async fn ctl_local_turn_grants_are_non_capture_and_enforce_observe_then_retire() {
    for sequential in [true, false] {
        let (config, state, tool, thread) = owned_read_fixture(sequential);
        let mut guest = owned_read_guest(&config, &state, thread);
        let owner = NetworkStreamOwner {
            thread: guest.thread.dettid,
            mm: guest.thread.mm_id,
        };
        let operation = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count);
        if sequential {
            grant_owned_read_foreground(&state, &mut guest).await;
        }
        let clock = state.global_time.lock().unwrap().as_nanos();
        let turn = state.sched.lock().unwrap().turn;
        guest.requests.lock().unwrap().clear();
        if sequential {
            assert!(
                state
                    .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Observe)
                    .is_err()
            );
            state
                .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Retire)
                .unwrap();
            background_local(&state, &tool, &mut guest, operation, true).await;
        } else {
            tool.begin_original_epoll_ctl_wait(&mut guest, operation)
                .await
                .unwrap();
        }
        state
            .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Observe)
            .unwrap();
        if sequential {
            assert!(
                state
                    .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Retire)
                    .is_err()
            );
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .original_external_grant_matches(owner, operation)
            );
            assert!(
                !state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .contains_tid(owner.thread)
            );
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
            let mut pending =
                std::pin::pin!(tool.finish_original_epoll_ctl_wait(&mut guest, operation));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .harvest_external_io_for_test()
                    .is_ok()
            );
            grant_ready(&state, owner.thread).await;
            pending.await;
        } else {
            tool.finish_original_epoll_ctl_wait(&mut guest, operation)
                .await;
        }
        state
            .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Retire)
            .unwrap();
        assert_eq!(
            state.sched.lock().unwrap().turn,
            turn + if sequential { 2 } else { 0 }
        );
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
        if sequential {
            assert!(
                state
                    .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Observe)
                    .is_err()
            );
            assert_eq!(guest.requests.lock().unwrap().len(), 2);
        } else {
            assert!(guest.requests.lock().unwrap().is_empty());
        }
        let stale = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        assert!(
            state
                .require_original_epoll_ctl_turn(stale, operation, EpollCtlTurn::Observe)
                .is_err()
        );
        assert!(
            state
                .require_original_epoll_ctl_turn(stale, operation, EpollCtlTurn::Retire)
                .is_err()
        );
        assert!(
            guest.thread.original_connect.is_none(),
            "turn fixture issues no Call/native effect"
        );
    }
}
#[tokio::test]
async fn ctl_local_turn_rejects_network_capture_and_wrong_operation() {
    let (config, state, tool, thread) = owned_read_fixture(true);
    let mut guest = owned_read_guest(&config, &state, thread);
    grant_owned_read_foreground(&state, &mut guest).await;
    let owner = NetworkStreamOwner {
        thread: guest.thread.dettid,
        mm: guest.thread.mm_id,
    };
    let operation = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count);
    let mut request = Resources::new(owner.thread);
    request.insert(
        ResourceID::BlockingNetworkCapture(operation),
        Permission::RW,
    );
    {
        let mut pending = std::pin::pin!(crate::tool_global::resource_request(&mut guest, request));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
        assert!(matches!(
            crate::scheduler::finish_selected_turn(
                state.sched.clone(),
                state.global_time.clone(),
                selected.0,
                selected.1,
                selected.2
            )
            .await,
            Err(crate::scheduler::SkipTurn)
        ));
        assert_eq!(pending.await, ResumeStatus::Normal);
    }
    assert!(
        state
            .sched
            .lock()
            .unwrap()
            .original_external_grant_matches(owner, operation)
    );
    assert!(
        state
            .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Observe)
            .is_err()
    );
    assert!(
        state
            .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Retire)
            .is_err()
    );
    // Pay this actually issued control grant before using a fresh local one.
    {
        let mut pending =
            std::pin::pin!(tool.finish_original_epoll_ctl_wait(&mut guest, operation));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        state
            .sched
            .lock()
            .unwrap()
            .harvest_external_io_for_test()
            .unwrap();
        grant_ready(&state, owner.thread).await;
        pending.await;
    }
    let next = ExternalOpId::new(owner.thread, guest.thread.stats.syscall_count + 1);
    background_local(&state, &tool, &mut guest, next, true).await;
    assert!(
        state
            .require_original_epoll_ctl_turn(owner, operation, EpollCtlTurn::Observe)
            .is_err()
    );
    state
        .require_original_epoll_ctl_turn(owner, next, EpollCtlTurn::Observe)
        .unwrap();
    {
        let mut pending = std::pin::pin!(tool.finish_original_epoll_ctl_wait(&mut guest, next));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        state
            .sched
            .lock()
            .unwrap()
            .harvest_external_io_for_test()
            .unwrap();
        grant_ready(&state, owner.thread).await;
        pending.await;
    }
}

struct PairFixture {
    config: Config,
    state: GlobalState,
    tool: Detcore,
    ctl: crate::ThreadState<()>,
    allocator: crate::ThreadState<()>,
    owner: NetworkStreamOwner,
    peer: NetworkStreamOwner,
    admission: Admission,
    metadata: Arc<Mutex<FileMetadata>>,
    history: HistoricalPair,
    candidate: FileMetadata,
    change: detcore_model::fd::NetworkFdSlotReplacement,
}
fn pair_fixture() -> PairFixture {
    use reverie::Tool;
    let (config, state) = stream_rpc_state(true);
    let (mut engine, owner, admission, metadata, candidate, change) =
        controlled_epoll_metadata_fixture(DetTid::from_raw(62));
    let tool = Detcore::new(Tid::from_raw(owner.thread.as_raw()), &config);
    let mut ctl = tool.init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
    ctl.file_metadata = metadata.clone();
    ctl.stats.syscall_count = 10; // The existing controlled admission uses this operation.
    assert_eq!(
        admission.arguments.operation,
        ExternalOpId::new(owner.thread, 10)
    );
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(owner.thread, owner.thread, true);
    install_test_registration(&state, owner.thread, Ivar::new());
    ctl.detpid = Some(owner.thread);
    let peer = NetworkStreamOwner {
        thread: DetTid::from_raw(62),
        mm: owner.mm,
    };
    let mut allocator = tool.init_thread_state(Tid::from_raw(peer.thread.as_raw()), None);
    allocator.file_metadata = metadata.clone();
    allocator.mm_id = owner.mm;
    allocator.detpid = Some(owner.thread);
    allocator.stats.syscall_count = 20;
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(owner.thread, peer.thread, false);
    state
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(peer.thread, peer.mm);
    assert_eq!(
        engine.fd_publication_fixture_register(peer, Some(owner)),
        admission.arguments.files
    );
    engine
        .associate_fd_metadata(peer, &metadata, &metadata.lock().unwrap())
        .unwrap();
    // A same-table replacement has already occurred physically in the supplied
    // historical pair. Its semantic publication is still pending. The old OFD
    // stays logically retained by the existing Call selection reservation.
    let (history, _) = crate::network_runtime::original_epoll_ctl::controlled_history_fixture(
        owner,
        metadata.clone(),
        &admission,
        [31, 37],
    )
    .unwrap();
    engine
        .original_connect_returned(owner, &admission, 0)
        .unwrap();
    assert!(
        !engine
            .original_epoll_ctl_selected(owner, &admission, &history)
            .unwrap()
    );
    // The unresolved ctl pair does not own the table exclusion needed by the
    // publisher. This probe is logical component authority, not fd_install.
    let permit = engine
        .acquire_fd_publication(peer, admission.arguments.files)
        .unwrap()
        .permit;
    engine.release_empty_fd_publication(peer, permit).unwrap();
    *state.network_engine.as_ref().unwrap().lock().unwrap() = engine;
    for thread in [&ctl, &allocator] {
        state.global_time.lock().unwrap().update_global_time(
            thread.dettid,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
    }
    PairFixture {
        config,
        state,
        tool,
        ctl,
        allocator,
        owner,
        peer,
        admission,
        metadata,
        history,
        candidate,
        change,
    }
}

#[tokio::test]
async fn ctl_old_early_rejoin_blocks_allocator_continuation() {
    let f = pair_fixture();
    let mut ctl = owned_read_guest(&f.config, &f.state, f.ctl);
    let mut allocator = owned_read_guest(&f.config, &f.state, f.allocator);
    grant_owned_read_foreground(&f.state, &mut ctl).await;
    background_local(
        &f.state,
        &f.tool,
        &mut ctl,
        f.admission.arguments.operation,
        true,
    )
    .await;
    install_test_registration(&f.state, f.peer.thread, Ivar::new());
    grant_owned_read_foreground(&f.state, &mut allocator).await;
    let allocator_op = ExternalOpId::new(f.peer.thread, allocator.thread.stats.syscall_count);
    background_local(&f.state, &f.tool, &mut allocator, allocator_op, false).await;
    // Reproduce the old faulty order deliberately through real turn transport.
    {
        let mut continuation = std::pin::pin!(
            f.tool
                .finish_original_epoll_ctl_wait(&mut ctl, f.admission.arguments.operation)
        );
        assert!(futures::poll!(continuation.as_mut()).is_pending());
        f.state
            .sched
            .lock()
            .unwrap()
            .harvest_external_io_for_test()
            .unwrap();
        grant_ready(&f.state, f.owner.thread).await;
        continuation.await;
    }
    assert!(
        f.state
            .require_original_epoll_ctl_turn(
                f.owner,
                f.admission.arguments.operation,
                EpollCtlTurn::Observe
            )
            .is_err()
    );
    let mut waiting = std::pin::pin!(
        f.tool
            .finish_original_openat_wait(&mut allocator, allocator_op)
    );
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    {
        let mut scheduler = f.state.sched.lock().unwrap();
        assert!(scheduler.ordinary_fd_observation(f.owner).is_ok());
        scheduler.harvest_external_io_for_test().unwrap();
        assert!(scheduler.original_external_io_grant_matches(f.peer, allocator_op));
        assert!(!scheduler.run_queue.contains_tid(f.peer.thread));
    }
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    assert!(
        !f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_epoll_ctl_selected(f.owner, &f.admission, &f.history)
            .unwrap()
    );
    // Both are component objects; no actor or native registration is abandoned.
}

#[tokio::test]
async fn ctl_pending_history_allows_allocator_foreground_publication() {
    let f = pair_fixture();
    let mut ctl = owned_read_guest(&f.config, &f.state, f.ctl);
    let mut allocator = owned_read_guest(&f.config, &f.state, f.allocator);
    grant_owned_read_foreground(&f.state, &mut ctl).await;
    background_local(
        &f.state,
        &f.tool,
        &mut ctl,
        f.admission.arguments.operation,
        true,
    )
    .await;
    install_test_registration(&f.state, f.peer.thread, Ivar::new());
    grant_owned_read_foreground(&f.state, &mut allocator).await;
    let allocator_op = ExternalOpId::new(f.peer.thread, allocator.thread.stats.syscall_count);
    background_local(&f.state, &f.tool, &mut allocator, allocator_op, false).await;
    assert!(
        !f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_epoll_ctl_selected(f.owner, &f.admission, &f.history)
            .unwrap()
    );
    // Ctl keeps waiting for history outside the ordinary queue. The sibling's
    // actual continuation is now harvestable while that join remains false.
    {
        let mut continuation = std::pin::pin!(
            f.tool
                .finish_original_openat_wait(&mut allocator, allocator_op)
        );
        assert!(futures::poll!(continuation.as_mut()).is_pending());
        f.state
            .sched
            .lock()
            .unwrap()
            .harvest_external_io_for_test()
            .unwrap();
        grant_ready(&f.state, f.peer.thread).await;
        continuation.await;
    }
    f.state
        .require_original_epoll_ctl_turn(
            f.owner,
            f.admission.arguments.operation,
            EpollCtlTurn::Observe,
        )
        .unwrap();
    assert!(
        f.state
            .sched
            .lock()
            .unwrap()
            .ordinary_fd_observation(f.peer)
            .is_ok()
    );
    {
        let mut metadata = f.metadata.lock().unwrap();
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        engine
            .validate_fd_metadata(f.peer, f.admission.arguments.files, &f.metadata, &metadata)
            .unwrap();
        let permit = engine
            .acquire_fd_publication(f.peer, f.admission.arguments.files)
            .unwrap()
            .permit;
        let effect = engine.fd_publication_fixture_effect(f.peer, f.change);
        let batch = crate::network_replay::NetworkFdPublicationBatch {
            files: f.admission.arguments.files,
            sequence: 2,
            previous_generation: 1,
            through_generation: 2,
            entries: vec![crate::network_replay::NetworkFdPublicationEntry {
                replacement: f.change,
                effect,
            }],
        };
        engine
            .publish_fd_publication(f.peer, permit, &batch)
            .unwrap();
        engine
            .acknowledge_fd_publication(f.peer, permit, &batch)
            .unwrap();
        *metadata = f.candidate;
        // Publication of the exact private annotation is still required even
        // though the generation now exists. No current numeric fd lookup.
        assert!(
            !engine
                .original_epoll_ctl_selected(f.owner, &f.admission, &f.history)
                .unwrap()
        );
        engine
            .note_epoll_published_metadata(
                f.peer,
                &f.metadata,
                &metadata,
                f.change.after.unwrap().binding,
            )
            .unwrap();
        assert!(
            engine
                .original_epoll_ctl_selected(f.owner, &f.admission, &f.history)
                .unwrap()
        );
        assert_eq!(
            engine
                .original_connect_result(f.owner, &f.admission)
                .unwrap(),
            Some(0)
        );
    }
    // Only now is the continuation posted. The allocator's remaining ordinary
    // work keeps its priority; this test does not bypass foreground scheduling.
    let mut continuation = std::pin::pin!(
        f.tool
            .finish_original_epoll_ctl_wait(&mut ctl, f.admission.arguments.operation)
    );
    assert!(futures::poll!(continuation.as_mut()).is_pending());
    let scheduler = f.state.sched.lock().unwrap();
    assert!(scheduler.original_external_io_grant_matches(f.owner, f.admission.arguments.operation));
    assert!(scheduler.ordinary_fd_observation(f.peer).is_ok());
    assert!(!scheduler.run_queue.contains_tid(f.owner.thread));
}
