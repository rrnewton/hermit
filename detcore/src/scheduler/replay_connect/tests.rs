//! Controlled valid V4 trace/table premises; production selected reader,
//! external start, maintenance, requeue and continuation grants are exercised.
//! This does not substitute for native callback or full Global-root evidence.
use super::*;
use crate::network_replay::original_connect::Kind;
use crate::network_replay::replay_connect::fixture;
use crate::types::FdSlotBinding;

struct Fixture {
    scheduler: Scheduler,
    engine: Arc<Mutex<NetworkReplayEngine>>,
    owner: NetworkStreamOwner,
    binding: FdSlotBinding,
    _metadata: Arc<Mutex<crate::tool_local::FileMetadata>>,
    time: Arc<Mutex<GlobalTime>>,
    start: LogicalTime,
    release: LogicalTime,
    operation: ExternalOpId,
    call: NetworkStreamCallId,
}

fn origin(mm: MmId) -> parked::ResourceOrigin {
    parked::ResourceOrigin {
        rpc: parked::RpcOrigin::DirectRequestResources,
        mm,
        control: parked::ControlCapability::None,
    }
}

fn register(scheduler: &mut Scheduler, tid: DetTid) {
    scheduler.install_test_exec_incarnation(tid, MmId::initial(tid));
    scheduler.priorities.insert(tid, DEFAULT_PRIORITY);
    scheduler.next_turns.insert(
        tid,
        ThreadNextTurn {
            dettid: tid,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
}

fn start(asynchronous: bool, delay: u64) -> Fixture {
    let config = Config::default();
    let time = GlobalTime::new(&config);
    let start = time.as_nanos();
    let release = start + LogicalTime::from_nanos(delay);
    let f = fixture(release, asynchronous);
    let engine = Arc::new(Mutex::new(f.engine));
    let mut scheduler = Scheduler::new(&config);
    scheduler.committed_time = start;
    scheduler.set_network_engine(Some(engine.clone()));
    let owner = f.owner;
    register(&mut scheduler, owner.thread);
    let operation = ExternalOpId::new(owner.thread, 383);
    let resource = crate::syscalls::original_external_resource(Kind::Connect, operation).unwrap();
    let mut resources = Resources::new(owner.thread);
    resources.insert(resource, Permission::RW);
    resources.fyi("connect");
    resources.fd_read = Some(fd_read::FdReadIntent {
        owner,
        files: f.binding.slot.files,
        fd: f.binding.slot.fd,
        operation,
    });
    let turn = scheduler.next_turns.get_mut(&owner.thread).unwrap();
    turn.protocol.origin = Some(origin(owner.mm));
    turn.req.put(Ok(resources.clone()));
    scheduler.runqueue_push_back(owner.thread);
    assert!(
        scheduler
            .check_replay_connect_start(owner, operation)
            .is_err()
    );
    let (tid, request, response) = scheduler.step3_peek().unwrap();
    assert_eq!(tid, owner.thread);
    let fd_read::SelectedFdRead::Ready(Some(read)) = scheduler
        .try_selected_fd_read(tid, &request, &response, &resources)
        .unwrap()
    else {
        panic!("actual selected reader must grant exact bound socket");
    };
    scheduler.next_turns.get_mut(&tid).unwrap().protocol.fd_read = Some(*read);
    assert!(
        scheduler
            .step4_resource_block(tid, &resources, &response)
            .is_err()
    );
    let Some(SchedResponse::GoFdRead(_, read)) = response.try_read() else {
        panic!("actual external start must carry its selected reader");
    };
    assert_eq!(scheduler.turn, 1);
    assert!(!scheduler.run_queue.contains_tid(tid));
    assert!(scheduler.blocked.network_waiters.is_empty());
    scheduler
        .check_replay_connect_start(owner, operation)
        .unwrap();
    let call = engine
        .lock()
        .unwrap()
        .begin_replay_connect(owner, operation, *read, f.binding.open_file)
        .unwrap()
        .id;
    scheduler
        .enroll_replay_connect(owner, operation, call)
        .unwrap();
    Fixture {
        scheduler,
        engine,
        owner,
        binding: f.binding,
        _metadata: f.metadata,
        time: Arc::new(Mutex::new(time)),
        start,
        release,
        operation,
        call,
    }
}

fn post(f: &mut Fixture) {
    let mut continuation = Resources::new(f.owner.thread);
    continuation.insert(
        ResourceID::BlockedExternalContinue(f.operation),
        Permission::RW,
    );
    continuation.fyi("connect");
    let turn = f.scheduler.next_turns.get_mut(&f.owner.thread).unwrap();
    turn.protocol.origin = Some(origin(f.owner.mm));
    turn.req.put(Ok(continuation));
}

fn maintain(f: &mut Fixture) {
    let _ = f.scheduler.step2_process_blocked(&f.time);
    f.scheduler.bump_global_time(&f.time, &Err(SkipTurn));
}

fn grant(f: &mut Fixture) {
    let (tid, request, response) = f
        .scheduler
        .step3_peek()
        .expect("eligible actual continuation enters selection");
    assert_eq!(tid, f.owner.thread);
    let resources = request.try_read().unwrap().unwrap();
    assert_eq!(
        resources.resources,
        [(
            ResourceID::BlockedExternalContinue(f.operation),
            Permission::RW
        )]
        .into_iter()
        .collect()
    );
    f.scheduler
        .step4_resource_block(tid, &resources, &response)
        .unwrap();
    f.scheduler
        .step5_guest_unblock(tid, &resources, &response)
        .unwrap();
    f.scheduler.step6_reenquue(tid, false);
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    f.scheduler
        .check_replay_connect_completion(f.owner, f.operation, f.call)
        .unwrap();
}

#[test]
fn replay_connect_selected_pair_preserves_exact_result_turns_and_capture_time() {
    for asynchronous in [false, true] {
        for delay in [0, 1, 1_392_909] {
            // Keep the synthetic host clock in its own fixture: later actual
            // maintenance samples Instant::now and must remain monotonic.
            let mut clock_probe = start(asynchronous, delay);
            let host = Instant::now();
            clock_probe
                .scheduler
                .sample_network_capture_clock(&mut clock_probe.time.lock().unwrap(), host);
            clock_probe.scheduler.sample_network_capture_clock(
                &mut clock_probe.time.lock().unwrap(),
                host + Duration::from_secs(60),
            );
            assert_eq!(
                clock_probe.time.lock().unwrap().as_nanos(),
                clock_probe.start,
                "Replay cannot import host delay"
            );
            let mut f = start(asynchronous, delay);
            assert!(
                f.scheduler
                    .check_replay_connect_completion(f.owner, f.operation, f.call)
                    .is_err()
            );
            post(&mut f);
            for _ in 0..4 {
                maintain(&mut f);
            }
            assert_eq!(f.scheduler.committed_time, f.release);
            assert_eq!(
                f.scheduler.turn, 1,
                "maintenance does not commit another turn"
            );
            assert!(f.scheduler.blocked.network_waiters.is_empty());
            assert!(
                f.engine
                    .lock()
                    .unwrap()
                    .take_connection_outcome(f.binding.open_file)
                    .is_err()
            );
            grant(&mut f);
            assert_eq!(f.scheduler.turn, 2);
            let expected = if asynchronous {
                detcore_model::network_trace::NetworkConnectionResultV2::Error(libc::EINPROGRESS)
            } else {
                detcore_model::network_trace::NetworkConnectionResultV2::Connected
            };
            assert_eq!(
                f.engine
                    .lock()
                    .unwrap()
                    .complete_replay_connect(f.owner, f.operation, f.call, f.release)
                    .unwrap(),
                expected
            );
            f.scheduler.finish_replay_connect(f.owner);
            assert!(
                f.scheduler
                    .check_replay_connect_completion(f.owner, f.operation, f.call)
                    .is_err()
            );
            assert!(f.scheduler.blocked.external_io_blockers.is_empty());
            assert!(f.scheduler.network_capture_blockers.is_empty());
        }
    }
}

fn timer(f: &mut Fixture, deadline: LogicalTime) -> DetTid {
    let tid = DetTid::from_raw(41);
    register(&mut f.scheduler, tid);
    let mut request = Resources::new(tid);
    request.insert(ResourceID::SleepUntil(deadline), Permission::R);
    f.scheduler.next_turns[&tid].req.put(Ok(request));
    f.scheduler.blocked.timed_waiters.insert(deadline, tid);
    tid
}

#[test]
fn replay_connect_missing_or_already_ready_continuation_does_not_jump_to_timer() {
    for ready in [false, true] {
        let mut f = start(false, if ready { 0 } else { 10 });
        let deadline = f.start + LogicalTime::from_nanos(100);
        timer(&mut f, deadline);
        if ready {
            post(&mut f);
            f.engine.lock().unwrap().release_eligible(f.start).unwrap();
        }
        f.scheduler.advance_replay_connect(&f.time).unwrap();
        assert_eq!(f.time.lock().unwrap().as_nanos(), f.start);
        assert_eq!(f.scheduler.turn, 1);
        assert!(f.scheduler.run_queue.is_empty());
        assert!(f.scheduler.terminal_deadlock.is_none());
    }
}

#[test]
fn replay_connect_timer_before_equal_and_after_release_preserves_maintenance_order() {
    for delta in [9, 10, 11] {
        let mut f = start(true, 10);
        let deadline = f.start + LogicalTime::from_nanos(delta);
        let waiter = timer(&mut f, deadline);
        post(&mut f);
        maintain(&mut f);
        assert_eq!(f.scheduler.committed_time, deadline.min(f.release));
        maintain(&mut f);
        if delta <= 10 {
            assert!(f.scheduler.run_queue.contains_tid(waiter));
            assert!(
                !f.scheduler.run_queue.contains_tid(f.owner.thread),
                "ordinary timer wake precedes external harvesting, including equal time"
            );
            assert_eq!(
                f.scheduler.blocked.external_io_blockers[&f.owner.thread],
                f.operation
            );
        } else {
            assert!(!f.scheduler.run_queue.contains_tid(waiter));
            assert!(f.scheduler.run_queue.contains_tid(f.owner.thread));
            assert_eq!(f.scheduler.committed_time, f.release);
        }
    }
}

#[test]
fn replay_connect_runnable_work_precedes_completion_but_pollers_do_not_starve_it() {
    for poller in [false, true] {
        let mut f = start(false, 0);
        let peer = DetTid::from_raw(42);
        register(&mut f.scheduler, peer);
        if poller {
            f.scheduler.priorities.insert(peer, LAST_PRIORITY);
        }
        f.scheduler.next_turns[&peer]
            .req
            .put(Ok(Resources::new(peer)));
        f.scheduler.runqueue_push_back(peer);
        post(&mut f);
        f.scheduler.step2c_process_io_blockers().unwrap();
        assert_eq!(f.scheduler.run_queue.contains_tid(f.owner.thread), poller);
        assert!(f.scheduler.run_queue.contains_tid(peer));
        assert_eq!(f.scheduler.committed_time, f.start);
    }
}

#[test]
fn replay_connect_rejects_signal_foreign_operation_mm_and_replaced_transport() {
    for change in 0..4 {
        let mut f = start(false, 0);
        if change < 2 {
            let mut request = Resources::new(f.owner.thread);
            request.insert(
                if change == 0 {
                    ResourceID::InboundSignal(SigWrapper::from(Signal::SIGUSR1))
                } else {
                    ResourceID::BlockedExternalContinue(ExternalOpId::new(f.owner.thread, 384))
                },
                Permission::RW,
            );
            let turn = f.scheduler.next_turns.get_mut(&f.owner.thread).unwrap();
            turn.protocol.origin = Some(origin(f.owner.mm));
            turn.req.put(Ok(request));
        } else {
            post(&mut f);
            let turn = f.scheduler.next_turns.get_mut(&f.owner.thread).unwrap();
            if change == 2 {
                turn.protocol.origin = Some(origin(f.owner.mm.for_exec(f.owner.thread)));
            } else {
                turn.req = Ivar::full(turn.req.try_read().unwrap());
            }
        }
        assert!(f.scheduler.step2c_process_io_blockers().is_err());
        assert!(f.scheduler.terminal_deadlock.is_some());
        assert_eq!(f.scheduler.turn, 1);
        assert!(!f.scheduler.run_queue.contains_tid(f.owner.thread));
        assert!(f.engine.lock().unwrap().finish().is_err());
    }
}

#[test]
fn replay_connect_cancellation_keeps_claim_unconsumed_before_and_after_eligibility() {
    for eligible in [false, true] {
        let mut f = start(true, 10);
        post(&mut f);
        if eligible {
            for _ in 0..3 {
                maintain(&mut f);
            }
        }
        f.scheduler.remove_blocking_entries(&f.owner.thread);
        f.engine.lock().unwrap().stream_owner_gone(f.owner);
        assert!(f.scheduler.replay_connect.is_empty());
        assert!(
            f.engine
                .lock()
                .unwrap()
                .complete_replay_connect(f.owner, f.operation, f.call, f.release)
                .is_err()
        );
        assert!(f.engine.lock().unwrap().finish().is_err());
    }
}

#[test]
fn replay_connect_unresolved_real_external_blocker_is_not_fabricated_from_trace_time() {
    let mut f = start(true, 10);
    post(&mut f);
    let other = DetTid::from_raw(43);
    register(&mut f.scheduler, other);
    let operation = ExternalOpId::new(other, 9);
    f.scheduler
        .blocked
        .external_io_blockers
        .insert(other, operation);
    assert!(f.scheduler.advance_replay_connect(&f.time).is_err());
    assert!(
        f.scheduler
            .terminal_deadlock
            .as_ref()
            .unwrap()
            .contains("mixed with unresolved native IO")
    );
    assert_eq!(f.time.lock().unwrap().as_nanos(), f.start);
    assert!(f.engine.lock().unwrap().finish().is_err());
    assert_eq!(f.scheduler.blocked.external_io_blockers[&other], operation);
    assert!(f.scheduler.next_turns[&other].req.try_read().is_none());
}

#[test]
fn replay_connect_physical_exit_and_terminal_failure_barriers_do_not_advance_time() {
    for failure in [false, true] {
        let mut f = start(true, 10);
        post(&mut f);
        let child = DetPid::from_raw(44);
        if failure {
            f.scheduler.terminal_deadlock = Some("retained prior refusal".to_owned());
        } else {
            f.scheduler.pending_physical_process_exits.insert(child);
        }
        let _ = f.scheduler.advance_replay_connect(&f.time);
        assert_eq!(f.time.lock().unwrap().as_nanos(), f.start);
        if !failure {
            assert!(f.scheduler.complete_physical_process_exit(child));
            assert!(f.scheduler.advance_replay_connect(&f.time).is_err());
            assert_eq!(f.time.lock().unwrap().as_nanos(), f.release);
        }
    }
}

#[test]
fn replay_connect_deferred_signal_becomes_runnable_before_idle_time_advance() {
    let mut f = start(true, 10);
    post(&mut f);
    let deadline = f.start + LogicalTime::from_nanos(100);
    timer(&mut f, deadline);
    let peer = DetTid::from_raw(45);
    register(&mut f.scheduler, peer);
    let mut signal = Resources::new(peer);
    signal.insert(
        ResourceID::InboundSignal(SigWrapper::from(Signal::SIGCHLD)),
        Permission::RW,
    );
    f.scheduler.next_turns[&peer].req.put(Ok(signal));
    f.scheduler.blocked.sigchld_deferred.insert(peer);
    f.scheduler.recordreplay_modes = false;
    let _ = f.scheduler.step2_process_blocked(&f.time);
    assert!(f.scheduler.run_queue.contains_tid(peer));
    assert!(f.scheduler.blocked.sigchld_ready.contains(&peer));
    assert!(!f.scheduler.run_queue.contains_tid(f.owner.thread));
    assert_eq!(f.time.lock().unwrap().as_nanos(), f.start);
}
