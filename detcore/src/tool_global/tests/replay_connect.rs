//! Real caller/Global/scheduler joins over controlled initial-census and trace
//! premises. OwnedReadGuest forbids every native injection; this does not claim
//! native transport-query, backend callback or complete manifest qualification.
use reverie::Tool;

use super::*;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkStreamNamespace;
use crate::resources::ExternalOpId;
use crate::scheduler::parked::ResourceReply;
use crate::types::FdSlotBinding;

struct Fixture {
    config: Config,
    state: GlobalState,
    tool: Detcore,
    thread: crate::ThreadState<()>,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    binding: FdSlotBinding,
    release: LogicalTime,
}

impl Fixture {
    async fn new(asynchronous: bool) -> Self {
        let release = LogicalTime::from_nanos(1_392_909);
        let trace = crate::network_replay::replay_connect::fixture(release, asynchronous)
            .engine
            .native_trace_fixture();
        let mut bytes = Vec::new();
        detcore_model::network_trace::NetworkTrace::V4(trace.clone())
            .write_framed(&mut bytes)
            .unwrap();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            epoch: trace.epoch,
            network_trace_input: Some(bytes),
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Replay;
        let tid = Tid::from_raw(unsafe { libc::syscall(libc::SYS_gettid) } as i32);
        let (runtime, root, metadata, memory, claim) =
            crate::network_runtime::controlled_foreground_runtime(tid.as_raw());
        let owner = root.owner();
        let mut state = GlobalState::initialize(&config, false);
        state.network_runtime = Some(runtime);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.detpid = Some(owner.thread);
        thread.mm_id = owner.mm;
        thread.stats.syscall_count = 383;
        thread.file_metadata = metadata;
        thread.memory_metadata = memory;
        state
            .registered_exec_mms
            .lock()
            .unwrap()
            .insert(owner.thread, owner.mm);
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(&root);
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            engine
                .register_initial_census(root.association(), &claim, owner.thread)
                .unwrap();
        }
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
        let mut guest = owned_read_guest(&config, &state, thread);
        let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Socket).await;
        tool.initialize_network_fd_tracking(&mut guest)
            .await
            .unwrap();
        assert!(tool.network_fd_tracking_active(&guest));
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine
                .register_stream_socket(
                    binding.open_file,
                    trace.fresh_stream_profiles[0].key,
                    NetworkStreamNamespace {
                        device: 7,
                        inode: 11,
                    },
                    None,
                )
                .unwrap();
            engine
                .bind(binding.open_file, trace.channels[0].id)
                .unwrap();
        }
        grant_owned_read_foreground(&state, &mut guest).await;
        let thread = guest.thread;
        Self {
            config,
            state,
            tool,
            thread,
            root,
            binding,
            release,
        }
    }
}

async fn select_start(state: &GlobalState, owner: NetworkStreamOwner, binding: FdSlotBinding) {
    let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
    assert_eq!(selected.0, owner.thread);
    let request = selected.1.try_read().unwrap().unwrap();
    assert_eq!(
        request.resources,
        [(
            ResourceID::BlockingNetworkCapture(ExternalOpId::new(owner.thread, 383)),
            Permission::RW
        )]
        .into_iter()
        .collect()
    );
    let intent = request.fd_read.as_ref().unwrap();
    assert_eq!(
        (intent.owner, intent.files, intent.fd),
        (owner, binding.slot.files, binding.slot.fd)
    );
    assert!(
        crate::scheduler::finish_selected_turn(
            state.sched.clone(),
            state.global_time.clone(),
            selected.0,
            selected.1,
            selected.2
        )
        .await
        .is_err()
    );
}

async fn select_continuation(state: &GlobalState, owner: NetworkStreamOwner) {
    let mut completed = None;
    // Actual scheduler step1/time publication, timer/signal maintenance and
    // continuation selection; a missing callback may not pass vacuously.
    for _ in 0..4 {
        let result = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await;
        if let Ok(resources) = result {
            completed = Some(resources);
            break;
        }
    }
    let request = completed.expect("exact trace continuation must commit");
    assert_eq!(
        request.resources,
        [(
            ResourceID::BlockedExternalContinue(ExternalOpId::new(owner.thread, 383)),
            Permission::RW
        )]
        .into_iter()
        .collect()
    );
    assert!(request.fd_read.is_none());
}

#[tokio::test]
async fn replay_connect_actual_caller_global_pair_consumes_once_without_native_injection() {
    for asynchronous in [false, true] {
        let q = Fixture::new(asynchronous).await;
        let owner = q.root.owner();
        let mut guest = owned_read_guest(&q.config, &q.state, q.thread);
        guest.expose_local_global = true;
        let before_turn = q.state.sched.lock().unwrap().turn;
        let result;
        {
            let call = reverie::syscalls::Connect::new().with_fd(q.binding.slot.fd);
            let mut pending = std::pin::pin!(q.tool.replay_connect_fixture_dispatch(
                &mut guest,
                call,
                q.binding.open_file
            ));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            select_start(&q.state, owner, q.binding).await;
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert_eq!(
                q.state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(q.binding.open_file),
                (1, 0, 0, 1)
            );
            select_continuation(&q.state, owner).await;
            result = pending.await;
        }
        if asynchronous {
            assert!(matches!(
                result,
                Err(reverie::Error::Errno(reverie::Errno::EINPROGRESS))
            ));
        } else {
            assert_eq!(result.unwrap(), 0);
        }
        let scheduler = q.state.sched.lock().unwrap();
        assert_eq!(scheduler.turn, before_turn + 2);
        assert_eq!(scheduler.committed_time, q.release);
        assert!(!scheduler.backend_failed());
        drop(scheduler);
        assert_eq!(q.state.global_time.lock().unwrap().as_nanos(), q.release);
        let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_eq!(
            engine.native_capture_fixture_counts(q.binding.open_file),
            (0, 0, 0, 0)
        );
        assert!(
            engine
                .take_connection_outcome(q.binding.open_file)
                .unwrap()
                .is_none()
        );
        assert!(engine.take_lifetime_retired_ports().is_empty());
        let requested = guest
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| match request {
                GlobalRequest::RequestResources(resources, _)
                | GlobalRequest::ParkedRequest(resources, _, _) => Some(resources.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(requested.len(), 2);
        assert!(requested[0].fd_read.is_some());
        assert!(requested[1].fd_read.is_none());
    }
}

async fn selected_read(q: &Fixture, guest: &mut OwnedReadGuest<'_>) -> NetworkFdReadAdmission {
    let owner = q.root.owner();
    let operation = ExternalOpId::new(owner.thread, 383);
    let mut resources = Resources::new(owner.thread);
    resources.insert(
        ResourceID::BlockingNetworkCapture(operation),
        Permission::RW,
    );
    resources.fd_read = Some(crate::scheduler::fd_read::FdReadIntent {
        owner,
        files: q.binding.slot.files,
        fd: q.binding.slot.fd,
        operation,
    });
    let mut pending = std::pin::pin!(super::super::fd_read_resource_request(guest, resources));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    select_start(&q.state, owner, q.binding).await;
    let ResourceReply::ReadGrant {
        status: ResumeStatus::Normal,
        read,
    } = pending.await
    else {
        panic!("real selected FD read must grant Normal");
    };
    *read
}

#[tokio::test]
async fn replay_connect_global_rejects_wrong_actual_identity_before_transfer() {
    for mutation in 0..5 {
        let mut q = Fixture::new(false).await;
        let owner = q.root.owner();
        // Move out without replacing any registered identity.
        let thread = q
            .tool
            .init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
        let mut guest = owned_read_guest(
            &q.config,
            &q.state,
            std::mem::replace(&mut q.thread, thread),
        );
        let read = selected_read(&q, &mut guest).await;
        let mut tid = Tid::from_raw(q.root.association().process());
        match mutation {
            0 => tid = Tid::from_raw(tid.as_raw() + 1),
            1 => guest.thread.stats.syscall_count += 1,
            2 => guest.thread.mm_id = crate::types::MmId::initial(DetTid::from_raw(999)),
            3 => {
                guest.thread.file_metadata = Arc::new(Mutex::new(
                    crate::tool_local::FileMetadata::empty_network_fixture(owner.thread),
                ))
            }
            4 => {
                q.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .remove(&owner.thread);
            }
            _ => unreachable!(),
        }
        assert!(
            q.state
                .begin_replay_connect(
                    tid,
                    &guest.thread,
                    ExternalOpId::new(owner.thread, 383),
                    read.clone(),
                    q.binding.open_file
                )
                .is_err()
        );
        let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_eq!(
            engine.native_capture_fixture_counts(q.binding.open_file),
            (0, 1, 1, 0)
        );
        // Refusal has not transferred or invented retirement of the exact read.
        engine.finish_fd_read(owner, read).unwrap();
        assert_eq!(
            engine.native_capture_fixture_counts(q.binding.open_file),
            (0, 0, 0, 0)
        );
        engine.release_eligible(q.release).unwrap();
        assert!(
            engine
                .take_connection_outcome(q.binding.open_file)
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn replay_connect_caller_cancellation_keeps_transferred_claim_unresolved() {
    let q = Fixture::new(false).await;
    let owner = q.root.owner();
    let mut guest = owned_read_guest(&q.config, &q.state, q.thread);
    guest.expose_local_global = true;
    {
        let mut pending = std::pin::pin!(q.tool.replay_connect_fixture_dispatch(
            &mut guest,
            reverie::syscalls::Connect::new().with_fd(q.binding.slot.fd),
            q.binding.open_file
        ));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        select_start(&q.state, owner, q.binding).await;
        assert!(futures::poll!(pending.as_mut()).is_pending());
    }
    q.state.abandon_network_owners([owner]);
    let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        engine.native_capture_fixture_counts(q.binding.open_file),
        (1, 0, 0, 1)
    );
    assert!(engine.take_connection_outcome(q.binding.open_file).is_err());
    assert!(engine.finish().is_err());
}

#[tokio::test]
async fn replay_connect_global_rejected_completion_keeps_exact_granted_claim() {
    let mut q = Fixture::new(true).await;
    let owner = q.root.owner();
    let operation = ExternalOpId::new(owner.thread, 383);
    let thread = q
        .tool
        .init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
    let mut guest = owned_read_guest(
        &q.config,
        &q.state,
        std::mem::replace(&mut q.thread, thread),
    );
    let read = selected_read(&q, &mut guest).await;
    let tid = Tid::from_raw(q.root.association().process());
    let call = q
        .state
        .begin_replay_connect(tid, &guest.thread, operation, read, q.binding.open_file)
        .unwrap();
    {
        let mut resources = Resources::new(owner.thread);
        resources.insert(
            ResourceID::BlockedExternalContinue(operation),
            Permission::RW,
        );
        let mut pending = std::pin::pin!(super::super::resource_request(&mut guest, resources));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        select_continuation(&q.state, owner).await;
        assert_eq!(pending.await, ResumeStatus::Normal);
    }
    for (bad_tid, bad_operation) in [
        (Tid::from_raw(tid.as_raw() + 1), operation),
        (tid, ExternalOpId::new(owner.thread, 384)),
    ] {
        assert!(
            q.state
                .complete_replay_connect(bad_tid, &guest.thread, bad_operation, call.id)
                .is_err()
        );
        let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
        assert!(
            engine
                .replay_connect_status(owner, operation, call.id, q.release)
                .unwrap()
                .ready
        );
        assert!(engine.take_connection_outcome(q.binding.open_file).is_err());
        assert_eq!(
            engine.native_capture_fixture_counts(q.binding.open_file),
            (1, 0, 0, 1)
        );
    }
    assert_eq!(
        q.state
            .complete_replay_connect(tid, &guest.thread, operation, call.id)
            .unwrap(),
        detcore_model::network_trace::NetworkConnectionResultV2::Error(libc::EINPROGRESS)
    );
    assert!(
        q.state
            .complete_replay_connect(tid, &guest.thread, operation, call.id)
            .is_err()
    );
}

#[tokio::test]
async fn replay_connect_global_completion_drains_only_last_call_owned_port() {
    let mut q = Fixture::new(false).await;
    let owner = q.root.owner();
    let operation = ExternalOpId::new(owner.thread, 383);
    let thread = q
        .tool
        .init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
    let mut guest = owned_read_guest(
        &q.config,
        &q.state,
        std::mem::replace(&mut q.thread, thread),
    );
    let read = selected_read(&q, &mut guest).await;
    let tid = Tid::from_raw(q.root.association().process());
    let call = q
        .state
        .begin_replay_connect(tid, &guest.thread, operation, read, q.binding.open_file)
        .unwrap();
    let unrelated = OpenFileId::new_socket(owner.thread, 9999);
    q.state.used_ports.lock().unwrap().extend([12345, 12346]);
    q.state
        .open_file_to_port
        .lock()
        .unwrap()
        .extend([(q.binding.open_file, 12345), (unrelated, 12346)]);
    // Controlled exact Close in the lifetime table, not native concurrent
    // single-root execution. The admitted Call must remain the last owner.
    crate::network_replay::replay_connect::close_selected_binding_fixture(
        &mut q.state.network_engine.as_ref().unwrap().lock().unwrap(),
        owner,
        q.binding,
        &guest.thread.file_metadata,
    );
    assert_eq!(
        q.state
            .open_file_to_port
            .lock()
            .unwrap()
            .get(&q.binding.open_file),
        Some(&12345)
    );
    assert!(
        q.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .take_lifetime_retired_ports()
            .is_empty()
    );
    {
        let mut resources = Resources::new(owner.thread);
        resources.insert(
            ResourceID::BlockedExternalContinue(operation),
            Permission::RW,
        );
        let mut pending = std::pin::pin!(super::super::resource_request(&mut guest, resources));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        select_continuation(&q.state, owner).await;
        assert_eq!(pending.await, ResumeStatus::Normal);
    }
    assert_eq!(
        q.state
            .complete_replay_connect(tid, &guest.thread, operation, call.id)
            .unwrap(),
        detcore_model::network_trace::NetworkConnectionResultV2::Connected
    );
    assert_eq!(
        *q.state.used_ports.lock().unwrap(),
        [12346].into_iter().collect()
    );
    assert_eq!(
        *q.state.open_file_to_port.lock().unwrap(),
        [(unrelated, 12346)].into_iter().collect()
    );
    let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(engine.take_lifetime_retired_ports().is_empty());
    assert_eq!(
        engine.native_capture_fixture_counts(q.binding.open_file),
        (0, 0, 0, 0)
    );
}
