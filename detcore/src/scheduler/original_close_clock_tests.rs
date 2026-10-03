//! Actual scheduler resource/clock transitions for original Close and Connect.
//! This does not replace native FD selection or physical-close qualification.
use super::*;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Kind;

fn fixture(replay: bool) -> (Scheduler, GlobalTime, NetworkStreamOwner, ExternalOpId) {
    let config = Config::default();
    let mut scheduler = Scheduler::new(&config);
    let record = NetworkReplayEngine::record(config.epoch);
    let engine = if replay {
        let trace = record.into_recorded_versioned_trace().unwrap();
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        NetworkReplayEngine::replay_from_reader(std::io::Cursor::new(bytes)).unwrap()
    } else {
        record
    };
    scheduler.set_network_engine(Some(Arc::new(Mutex::new(engine))));
    let thread = DetTid::from_raw(903);
    let owner = NetworkStreamOwner {
        thread,
        mm: MmId::initial(thread),
    };
    scheduler.install_test_exec_incarnation(thread, owner.mm);
    scheduler.priorities.insert(thread, DEFAULT_PRIORITY);
    scheduler.next_turns.insert(
        thread,
        ThreadNextTurn {
            dettid: thread,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    (
        scheduler,
        GlobalTime::new(&config),
        owner,
        ExternalOpId::new(thread, 9),
    )
}

fn grant(
    scheduler: &mut Scheduler,
    owner: NetworkStreamOwner,
    operation: ExternalOpId,
    kind: Kind,
) {
    // Use the production dispatcher classification, then the actual selected
    // scheduler grant. Merely constructing a resource must grant no authority.
    let resource = crate::syscalls::original_external_resource(kind, operation).unwrap();
    let mut request = Resources::new(owner.thread);
    request.insert(resource.clone(), Permission::RW);
    let next = &scheduler.next_turns[&owner.thread];
    next.req.put(Ok(request));
    let response = next.resp.clone();
    scheduler.runqueue_push_back(owner.thread);
    assert!(!scheduler.original_transfer_grant_matches(owner, operation, kind));
    assert_eq!(scheduler.run_queue.tentative_pop_next(), Some(owner.thread));
    assert!(!scheduler.original_transfer_grant_matches(owner, operation, kind));
    assert!(matches!(
        scheduler
            .block_for_one_resource(owner.thread, &resource, &Permission::RW, None, &response,),
        Err(SkipTurn)
    ));
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    assert!(!scheduler.run_queue.contains_tid(owner.thread));
    assert_eq!(
        scheduler.blocked.external_io_blockers.get(&owner.thread),
        Some(&operation)
    );
    assert!(scheduler.original_transfer_grant_matches(owner, operation, kind));
    assert_eq!(scheduler.turn, 1);
}

#[test]
fn original_close_wait_preserves_record_replay_time_and_continuation() {
    for replay in [false, true] {
        let (mut scheduler, mut time, owner, operation) = fixture(replay);
        let initial = time.as_nanos();
        grant(&mut scheduler, owner, operation, Kind::Close);
        let start = Instant::now();
        scheduler.sample_network_capture_clock(&mut time, start);
        scheduler.sample_network_capture_clock(&mut time, start + Duration::from_secs(30));
        assert_eq!(time.as_nanos(), initial, "replay={replay}");
        assert!(scheduler.network_capture_blockers.is_empty());
        assert!(scheduler.network_capture_idle_since.is_none());

        let mut continuation = Resources::new(owner.thread);
        continuation.insert(
            ResourceID::BlockedExternalContinue(operation),
            Permission::RW,
        );
        scheduler.next_turns[&owner.thread]
            .req
            .put(Ok(continuation.clone()));
        assert!(scheduler.step2c_process_io_blockers().is_ok());
        assert!(scheduler.run_queue.contains_tid(owner.thread));
        assert!(scheduler.blocked.external_io_blockers.is_empty());
        assert!(!scheduler.original_transfer_grant_matches(owner, operation, Kind::Close));
        scheduler.sample_network_capture_clock(&mut time, start + Duration::from_secs(60));
        assert_eq!(time.as_nanos(), initial);
        // The original continuation still pays its ordinary scheduler quantum.
        let expected = GlobalTime::new(&Config::default()).add_scheduler_time();
        let global = Mutex::new(time);
        scheduler.bump_global_time(&global, &Ok(continuation));
        assert_eq!(global.lock().unwrap().as_nanos(), expected);
        assert!(expected > initial);
    }
}

#[test]
fn original_connect_retains_record_only_capture_time() {
    for replay in [false, true] {
        let (mut scheduler, mut time, owner, operation) = fixture(replay);
        let initial = time.as_nanos();
        grant(&mut scheduler, owner, operation, Kind::Connect);
        let start = Instant::now();
        scheduler.sample_network_capture_clock(&mut time, start);
        scheduler.sample_network_capture_clock(&mut time, start + Duration::from_millis(37));
        let delta = if replay { 0 } else { 37_000_000 };
        assert_eq!(time.as_nanos(), initial + LogicalTime::from_nanos(delta));
        assert!(scheduler.network_capture_idle_since.is_some());
        // This exercises the clock policy, not authorization for a native
        // Connect in Replay; the syscall's Record-only route is unchanged.
    }
}

#[test]
fn original_close_timer_uses_ordinary_io_without_fabricating_completion() {
    for replay in [false, true] {
        for inbound_signal in [false, true] {
            let (mut scheduler, time, owner, operation) = fixture(replay);
            let global = Arc::new(Mutex::new(time));
            let initial = global.lock().unwrap().as_nanos();
            grant(&mut scheduler, owner, operation, Kind::Close);
            scheduler.register_alarm(
                owner.thread,
                owner.thread,
                initial,
                LogicalTime::from_secs(8),
                LogicalTime::ZERO,
                Signal::SIGALRM,
            );
            assert!(scheduler.step2d_handle_empty_queue(&global).is_err());
            let deadline = initial + LogicalTime::from_secs(8);
            assert_eq!(global.lock().unwrap().as_nanos(), deadline);
            assert!(scheduler.blocked.timed_waiters.is_empty());
            assert_eq!(scheduler.host_signal_attempts, 1);
            // This fixture has no host process group. The genuine timer
            // dispatch path runs, but it does not send a physical host signal.
            assert!(scheduler.original_transfer_grant_matches(owner, operation, Kind::Close));
            assert!(scheduler.next_turns[&owner.thread].req.try_read().is_none());
            assert!(!scheduler.run_queue.contains_tid(owner.thread));
            scheduler.wake_signaled_guest(owner.thread, Signal::SIGALRM);
            assert!(scheduler.next_turns[&owner.thread].req.try_read().is_none());
            assert!(!scheduler.run_queue.contains_tid(owner.thread));

            // Only the real backend request can release the physical wait.
            // A timer notification is not a synthetic Close result or errno.
            let mut request = Resources::new(owner.thread);
            request.insert(
                if inbound_signal {
                    ResourceID::InboundSignal(SigWrapper::from(Signal::SIGALRM))
                } else {
                    ResourceID::BlockedExternalContinue(operation)
                },
                Permission::RW,
            );
            scheduler.next_turns[&owner.thread].req.put(Ok(request));
            assert!(scheduler.step2c_process_io_blockers().is_ok());
            assert!(scheduler.run_queue.contains_tid(owner.thread));
            assert!(scheduler.blocked.external_io_blockers.is_empty());
            assert!(!scheduler.original_transfer_grant_matches(owner, operation, Kind::Close));
            let start = Instant::now();
            scheduler.sample_network_capture_clock(&mut global.lock().unwrap(), start);
            scheduler.sample_network_capture_clock(
                &mut global.lock().unwrap(),
                start + Duration::from_secs(30),
            );
            assert_eq!(global.lock().unwrap().as_nanos(), deadline);
            assert!(scheduler.network_capture_idle_since.is_none());
        }
    }
}

#[test]
fn original_transfer_refuses_cross_class_stale_and_ungranted_operations() {
    for kind in [Kind::Close, Kind::Connect] {
        let (mut scheduler, _, owner, operation) = fixture(false);
        grant(&mut scheduler, owner, operation, kind);
        let other = if kind == Kind::Close {
            Kind::Connect
        } else {
            Kind::Close
        };
        assert!(!scheduler.original_transfer_grant_matches(owner, operation, other));
        assert!(!scheduler.original_transfer_grant_matches(owner, operation, Kind::Openat));
        assert!(!scheduler.original_transfer_grant_matches(
            owner,
            ExternalOpId::new(owner.thread, operation.sequence + 1),
            kind,
        ));
        let changed = DetTid::from_raw(904);
        assert!(!scheduler.original_transfer_grant_matches(
            NetworkStreamOwner {
                thread: changed,
                mm: owner.mm
            },
            operation,
            kind,
        ));
        assert!(!scheduler.original_transfer_grant_matches(
            NetworkStreamOwner {
                thread: owner.thread,
                mm: MmId::initial(changed)
            },
            operation,
            kind,
        ));
        scheduler.remove_blocking_entries(&owner.thread);
        assert!(!scheduler.original_transfer_grant_matches(owner, operation, kind));
    }
}

#[test]
fn unsupported_original_kind_cannot_obtain_external_resource() {
    let (_, _, _, operation) = fixture(false);
    for kind in [Kind::Openat, Kind::Read, Kind::EpollCtl] {
        assert!(crate::syscalls::original_external_resource(kind, operation).is_err());
    }
}
