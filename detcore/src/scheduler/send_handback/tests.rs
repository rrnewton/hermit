//! Components invoke the production scheduler issuers. Pending Call setup is
//! deliberately modeled; these tests do not execute or prove a native send.
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use super::*;
use crate::config::Config;
use crate::ivar::Ivar;
use crate::network_replay::NetworkStreamOwner;
use crate::resources::Permission;
use crate::resources::Resources;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::scheduler::SchedResponse;
use crate::scheduler::SkipTurn;
use crate::scheduler::ThreadNextTurn;
use crate::scheduler::parked::ControlCapability;
use crate::scheduler::parked::ResourceOrigin;
use crate::scheduler::parked::RpcOrigin;
use crate::types::GlobalTime;
use crate::types::MmId;

struct Fixture {
    scheduler: Scheduler,
    global: Mutex<GlobalTime>,
    owner: NetworkStreamOwner,
    operation: ExternalOpId,
    handle: SendTimingHandle,
    initial: LogicalTime,
}

fn request(f: &mut Fixture, resource: ResourceID) -> Ivar<SchedResponse> {
    let next = f.scheduler.next_turns.get_mut(&f.owner.thread).unwrap();
    next.protocol.origin = Some(ResourceOrigin {
        rpc: RpcOrigin::DirectRequestResources,
        mm: f.owner.mm,
        control: ControlCapability::None,
    });
    let mut resources = Resources::new(f.owner.thread);
    resources.insert(resource, Permission::RW);
    next.req.put(Ok(resources));
    next.resp.clone()
}

fn fixture() -> Fixture {
    let config = Config::default();
    let global = Mutex::new(GlobalTime::new(&config));
    let initial = global.lock().unwrap().as_nanos();
    let mut scheduler = Scheduler::new(&config);
    // Real committed-time publication without a turn/time charge.
    scheduler.bump_global_time(&global, &Err(SkipTurn));
    let tid = DetTid::from_raw(101);
    let owner = NetworkStreamOwner {
        thread: tid,
        mm: MmId::initial(tid),
    };
    scheduler.priorities.insert(tid, DEFAULT_PRIORITY);
    scheduler.next_turns.insert(
        tid,
        ThreadNextTurn {
            dettid: tid,
            child_tid_addr: 0,
            req: Ivar::full(Ok(Resources::new(tid))),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    scheduler.next_turns.get_mut(&tid).unwrap().protocol.origin = Some(ResourceOrigin {
        rpc: RpcOrigin::DirectRequestResources,
        mm: owner.mm,
        control: ControlCapability::None,
    });
    scheduler.runqueue_push_back(tid);
    let response = scheduler.next_turns[&tid].resp.clone();
    scheduler.unblock_guest(tid, &response).unwrap();
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    let operation = ExternalOpId::new(tid, 19);
    let before = (scheduler.turn, scheduler.committed_time);
    let handle = scheduler
        .enroll_send_timing(PendingSendTiming::fixture(owner, 7), operation)
        .unwrap();
    assert_eq!(before, (scheduler.turn, scheduler.committed_time));
    Fixture {
        scheduler,
        global,
        owner,
        operation,
        handle,
        initial,
    }
}

fn enter(f: &mut Fixture) {
    let resource = ResourceID::BlockingNetworkCapture(f.operation);
    let response = request(f, resource.clone());
    assert_eq!(
        f.scheduler.run_queue.tentative_pop_next(),
        Some(f.owner.thread)
    );
    assert!(
        f.scheduler
            .block_for_one_resource(f.owner.thread, &resource, &Permission::RW, None, &response)
            .is_err()
    );
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    assert_eq!(
        f.scheduler.network_capture_blockers[&f.owner.thread],
        f.operation
    );
}

fn complete(f: &mut Fixture) -> Ivar<SchedResponse> {
    let response = request(f, ResourceID::BlockedExternalContinue(f.operation));
    f.scheduler.step2c_process_io_blockers().unwrap();
    response
}

fn publish_tail(f: &mut Fixture, start: Instant) {
    f.scheduler.sample_network_capture_clock(
        &mut f.global.lock().unwrap(),
        start + Duration::from_millis(7),
    );
    assert!(f.scheduler.network_capture_idle_since.is_none());
    f.scheduler.bump_global_time(&f.global, &Err(SkipTurn));
}

#[test]
fn actual_external_pair_issues_exact_separate_clocks_and_consumes_once() {
    let mut f = fixture();
    assert_eq!(f.scheduler.turn, 1);
    enter(&mut f);
    let start = Instant::now();
    f.scheduler
        .sample_network_capture_clock(&mut f.global.lock().unwrap(), start);
    f.scheduler.sample_network_capture_clock(
        &mut f.global.lock().unwrap(),
        start + Duration::from_millis(3),
    );
    let response = complete(&mut f);
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Phase
    );
    // 3ms sampled but not published: completion must not borrow it from RPC,
    // GlobalTime, packet bytes, or a later receive. Final idle tail reaches 7ms.
    publish_tail(&mut f, start);
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    let receipt = f.scheduler.take_send_handback(&f.handle).unwrap();
    assert_eq!(receipt.operation(), f.operation);
    assert_eq!(
        receipt.stamps(),
        [
            SendTimingStamp {
                committed_time: f.initial,
                turn: 1,
                epoch: 1,
                publication: 1
            },
            SendTimingStamp {
                committed_time: f.initial,
                turn: 2,
                epoch: 2,
                publication: 1
            },
            SendTimingStamp {
                committed_time: f.initial,
                turn: 2,
                epoch: 2,
                publication: 1
            },
            SendTimingStamp {
                committed_time: f.initial + LogicalTime::from_millis(7),
                turn: 3,
                epoch: 3,
                publication: 2
            },
        ]
    );
    assert!(!receipt.matches(&PendingSendTiming::fixture(f.owner, 7)));
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Missing
    );
}

#[test]
fn missing_harvest_cannot_be_relabelled_completion() {
    let mut f = fixture();
    enter(&mut f);
    let response = request(
        &mut f,
        ResourceID::BlockedExternalContinue(ExternalOpId::new(DetTid::from_raw(101), 19)),
    );
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Phase
    );
}

#[test]
fn changed_operation_cannot_get_entry_evidence() {
    let mut f = fixture();
    let resource = ResourceID::BlockingNetworkCapture(ExternalOpId::new(f.owner.thread, 20));
    let response = request(&mut f, resource);
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Identity
    );
}

#[test]
fn duplicate_harvest_is_sticky_refusal() {
    let mut f = fixture();
    enter(&mut f);
    let response = complete(&mut f);
    f.scheduler
        .observe_send_completion(f.owner.thread, f.operation);
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Phase
    );
}

#[test]
fn unpublished_idle_tail_cannot_authorize_handback() {
    let mut f = fixture();
    enter(&mut f);
    f.scheduler
        .sample_network_capture_clock(&mut f.global.lock().unwrap(), Instant::now());
    let response = complete(&mut f);
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::UnpublishedIdleTail
    );
}

#[test]
fn due_timer_cannot_be_skipped_by_completed_send() {
    let mut f = fixture();
    enter(&mut f);
    let response = complete(&mut f);
    f.scheduler
        .blocked
        .timed_waiters
        .insert(f.initial, DetTid::from_raw(102));
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::DueTimer
    );
    assert_eq!(
        f.scheduler.blocked.timed_waiters.next_deadline(),
        Some(f.initial)
    );
}

#[test]
fn signal_response_remains_real_but_cannot_mint_send_handback() {
    let mut f = fixture();
    enter(&mut f);
    let response = request(
        &mut f,
        ResourceID::InboundSignal(crate::types::SigWrapper(libc::SIGUSR1)),
    );
    f.scheduler.step2c_process_io_blockers().unwrap();
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert!(matches!(
        response.try_read(),
        Some(SchedResponse::Signaled(_))
    ));
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::UnsupportedSignalOrRestart
    );
}

#[test]
fn cancellation_retains_refusal_instead_of_success() {
    let mut f = fixture();
    enter(&mut f);
    f.scheduler.remove_blocking_entries(&f.owner.thread);
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Cancelled
    );
}

#[test]
fn nonparticipant_external_grant_is_unchanged() {
    let mut f = fixture();
    f.scheduler.send_timing.attempts.clear();
    enter(&mut f);
    let response = complete(&mut f);
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    assert_eq!(f.scheduler.turn, 3);
    assert_eq!(f.scheduler.committed_time, f.initial);
    assert!(f.scheduler.send_timing.attempts.is_empty());
}

#[test]
fn same_call_numbers_do_not_replace_retained_pending_identity() {
    let mut f = fixture();
    assert_eq!(
        f.scheduler
            .enroll_send_timing(PendingSendTiming::fixture(f.owner, 7), f.operation)
            .unwrap_err(),
        SendTimingError::Phase
    );
    let forged = SendTimingHandle {
        identity: Arc::new(()),
        owner: f.owner,
    };
    assert_eq!(
        f.scheduler.take_send_handback(&forged).unwrap_err(),
        SendTimingError::Identity
    );
}

#[test]
fn unpublished_clock_assignment_cannot_mint_evidence() {
    let mut f = fixture();
    f.scheduler.committed_time = f.scheduler.committed_time + LogicalTime::from_nanos(1);
    enter(&mut f);
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Phase
    );
}

#[test]
fn replaced_continuation_epoch_cannot_get_handback() {
    let mut f = fixture();
    enter(&mut f);
    let response = complete(&mut f);
    f.scheduler
        .next_turns
        .get_mut(&f.owner.thread)
        .unwrap()
        .protocol
        .epoch += 1;
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Phase
    );
}

#[test]
fn exhausted_publication_does_not_restart_its_identity() {
    let mut f = fixture();
    f.scheduler.send_timing.published = Some((f.initial, u64::MAX));
    f.scheduler.bump_global_time(&f.global, &Err(SkipTurn));
    assert!(f.scheduler.send_timing.publication_exhausted);
    assert!(f.scheduler.send_timing.published.is_none());
    f.scheduler.bump_global_time(&f.global, &Err(SkipTurn));
    assert!(f.scheduler.send_timing.published.is_none());
    assert_eq!(
        f.scheduler.take_send_handback(&f.handle).unwrap_err(),
        SendTimingError::Phase
    );
}

#[test]
fn source_entry_epoch_cannot_be_rebound_to_a_later_normal_grant() {
    let mut f = fixture();
    f.scheduler.send_timing.attempts.clear();
    let response = request(
        &mut f,
        ResourceID::BlockedExternalContinue(ExternalOpId::new(DetTid::from_raw(101), 19)),
    );
    f.scheduler
        .unblock_guest(f.owner.thread, &response)
        .unwrap();
    let before = (f.scheduler.turn, f.scheduler.committed_time);
    assert_eq!(
        f.scheduler
            .enroll_send_timing(PendingSendTiming::fixture(f.owner, 7), f.operation)
            .unwrap_err(),
        SendTimingError::Identity
    );
    assert_eq!(before, (f.scheduler.turn, f.scheduler.committed_time));
}
