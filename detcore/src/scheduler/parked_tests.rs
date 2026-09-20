/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use reverie::BackendChildWaitEvent;
use reverie::BackendChildWaitState;
use reverie::BackendSignalControl;
use reverie::ChildExitCompletion;
use reverie::ChildExitPublication;
use reverie::ChildExitPublicationEffect;
use reverie::ChildExitPublicationResult;
use reverie::ExitStatus;
use reverie::ProcessSignalControl;
use reverie::ProcessSignalPublication;
use reverie::ProcessSignalPublicationResult;
use reverie::SignalBoundaryOutcome;
use reverie::SignalBoundaryReceipt;
use reverie::SignalDeliveryPermit;
use reverie::SignalProcessId;
use reverie::SignalRecipient;
use reverie::SignalTaskIdentity;

use super::parked::*;
use super::*;

#[derive(Default)]
struct Backend {
    child_effect: Mutex<Option<ChildExitPublicationEffect>>,
    child_failures: Mutex<Vec<ChildExitPublication>>,
    child_publication_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    child_publications: Mutex<Vec<ChildExitCompletion>>,
    fail_child_publication: std::sync::atomic::AtomicBool,
    fail_publication: std::sync::atomic::AtomicBool,
    fail_recipients: Mutex<Option<SignalProcessId>>,
    fail_reservation: std::sync::atomic::AtomicBool,
    failure_processes: Mutex<Vec<SignalProcessId>>,
    failure_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    recipients: Mutex<Vec<SignalRecipient>>,
    publications: Mutex<Vec<(SignalProcessId, reverie::SignalEvent)>>,
    reject_child_publication: std::sync::atomic::AtomicBool,
    permits: Mutex<Vec<SignalDeliveryPermit>>,
}
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").finish_non_exhaustive()
    }
}
impl ProcessSignalControl for Backend {
    fn publish_alarm(
        &self,
        process: SignalProcessId,
        event: reverie::SignalEvent,
    ) -> ProcessSignalPublicationResult {
        self.publications.lock().unwrap().push((process, event));
        let receipt = ProcessSignalPublication {
            process,
            pending_generation: 0,
            coalesced: false,
            disposition: reverie::ProcessAlarmSignalDisposition::Caught,
        };
        if self
            .fail_publication
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            ProcessSignalPublicationResult::FailedAfterCommit {
                receipt,
                errno: reverie::Errno::EBADF,
            }
        } else {
            ProcessSignalPublicationResult::Committed(receipt)
        }
    }
    fn publish_child_exit(&self, completion: ChildExitCompletion) -> ChildExitPublicationResult {
        if let Some(probe) = self.child_publication_probe.lock().unwrap().as_ref() {
            probe();
        }
        self.child_publications.lock().unwrap().push(completion);
        if self
            .reject_child_publication
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return ChildExitPublicationResult::RejectedBeforeCommit(reverie::Errno::EBADF);
        }
        let receipt = ChildExitPublication {
            completion,
            pending_generation: 11,
            effect: self
                .child_effect
                .lock()
                .unwrap()
                .unwrap_or(ChildExitPublicationEffect::Queued),
        };
        if self
            .fail_child_publication
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            ChildExitPublicationResult::FailedAfterCommit {
                receipt,
                errno: reverie::Errno::EBADF,
            }
        } else {
            ChildExitPublicationResult::Committed(receipt)
        }
    }
    fn signal_recipients(
        &self,
        process: SignalProcessId,
        _: i32,
    ) -> Result<Vec<SignalRecipient>, reverie::Errno> {
        if *self.fail_recipients.lock().unwrap() == Some(process) {
            return Err(reverie::Errno::EBADF);
        }
        Ok(self
            .recipients
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.task.process == process)
            .copied()
            .collect())
    }
    fn alarm_recipients(
        &self,
        process: SignalProcessId,
    ) -> Result<Vec<SignalRecipient>, reverie::Errno> {
        if *self.fail_recipients.lock().unwrap() == Some(process) {
            return Err(reverie::Errno::EBADF);
        }
        Ok(self
            .recipients
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.task.process == process)
            .copied()
            .collect())
    }
    fn reserve_delivery(&self, permit: SignalDeliveryPermit) -> Result<(), reverie::Errno> {
        if self
            .fail_reservation
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(reverie::Errno::EBADF);
        }
        self.permits.lock().unwrap().push(permit);
        Ok(())
    }
    fn release_delivery(&self, permit: SignalDeliveryPermit) -> Result<(), reverie::Errno> {
        self.permits.lock().unwrap().retain(|p| *p != permit);
        Ok(())
    }
    fn finish_publication_failure(&self, process: SignalProcessId) -> Result<(), reverie::Errno> {
        self.failure_processes.lock().unwrap().push(process);
        if let Some(probe) = self.failure_probe.lock().unwrap().as_ref() {
            probe();
        }
        Ok(())
    }
    fn finish_child_exit_publication_failure(
        &self,
        receipt: ChildExitPublication,
    ) -> Result<(), reverie::Errno> {
        self.child_failures.lock().unwrap().push(receipt);
        if let Some(probe) = self.failure_probe.lock().unwrap().as_ref() {
            probe();
        }
        Ok(())
    }
}
fn at(n: u64) -> LogicalTime {
    LogicalTime::from_nanos(n)
}
fn task(pid: i32, tid: i32) -> SignalTaskIdentity {
    SignalTaskIdentity {
        process: SignalProcessId {
            tgid: reverie::Pid::from_raw(pid),
            generation: 1,
        },
        tid: reverie::Pid::from_raw(tid),
        task_generation: tid as u64,
    }
}
fn fixture() -> (Scheduler, Arc<Backend>) {
    let mut s = Scheduler::new(&Config {
        backend_is_kvm: true,
        kvm_shared_dequeue_timers: true,
        cancel_killed_thread_rpcs: true,
        ..Config::default()
    });
    let backend = Arc::new(Backend::default());
    s.install_signal_control(Some(BackendSignalControl {
        process: backend.clone(),
    }))
    .unwrap();
    (s, backend)
}
fn add(s: &mut Scheduler, pid: i32, tid: i32) -> (DetTid, MmId, reverie::CallbackSignalSite) {
    add_with_mm(s, pid, tid, MmId::initial(DetTid::from_raw(pid)))
}
fn add_with_mm(
    s: &mut Scheduler,
    pid: i32,
    tid: i32,
    mm: MmId,
) -> (DetTid, MmId, reverie::CallbackSignalSite) {
    let (pid, tid) = (DetTid::from_raw(pid), DetTid::from_raw(tid));
    s.thread_tree.add_child(pid, tid, pid == tid);
    s.priorities.insert(tid, DEFAULT_PRIORITY);
    s.next_turns.insert(
        tid,
        ThreadNextTurn {
            dettid: tid,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    let identity = task(pid.as_raw(), tid.as_raw());
    s.real_timers.bind(pid, tid, mm, identity).unwrap();
    (
        tid,
        mm,
        reverie::CallbackSignalSite {
            process: identity.process,
            tid: identity.tid,
            task_generation: identity.task_generation,
            callback_nonce: 1,
            boundary_nonce: 1,
        },
    )
}

fn add_process_child(
    s: &mut Scheduler,
    parent: i32,
    child: i32,
) -> (DetTid, MmId, reverie::CallbackSignalSite) {
    let parent = DetTid::from_raw(parent);
    let child = DetTid::from_raw(child);
    let mm = MmId::initial(child);
    s.thread_tree.add_child(parent, child, true);
    s.priorities.insert(child, DEFAULT_PRIORITY);
    s.next_turns.insert(
        child,
        ThreadNextTurn {
            dettid: child,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    let identity = task(child.as_raw(), child.as_raw());
    s.real_timers.bind(child, child, mm, identity).unwrap();
    (
        child,
        mm,
        reverie::CallbackSignalSite {
            process: identity.process,
            tid: identity.tid,
            task_generation: identity.task_generation,
            callback_nonce: 1,
            boundary_nonce: 1,
        },
    )
}

fn child_exit_event(
    parent: SignalProcessId,
    child: SignalProcessId,
    status: ExitStatus,
    waitable: bool,
) -> BackendChildWaitEvent {
    BackendChildWaitEvent {
        parent,
        child,
        state: BackendChildWaitState::Exited {
            status,
            waitable,
            uid: 1000,
            user_ticks: 13,
            system_ticks: 17,
        },
    }
}

fn poll_child_exit(
    future: &mut Pin<Box<super::signal_control::ChildExitPublicationFuture>>,
) -> Poll<Result<(), reverie::Error>> {
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    Future::poll(future.as_mut(), &mut context)
}
fn sleep(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    deadline: u64,
) -> Ivar<SchedResponse> {
    let mut r = Resources::new(tid);
    r.insert(ResourceID::SleepUntil(at(deadline)), Permission::RW);
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::ParkedWait {
                policy: ParkedWaitPolicy::NanosleepNoHandlerRestart {
                    absolute_deadline: at(deadline),
                },
                site,
            },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(r));
    let response = turn.resp.clone();
    s.blocked.timed_waiters.insert(at(deadline), tid);
    response
}
fn selected(response: &Ivar<SchedResponse>) -> AlarmControl {
    match response.try_read().expect("observation transport") {
        SchedResponse::ObserveSignal(c) => *c,
        other => panic!("{other:?}"),
    }
}

#[test]
fn real_expiry_publishes_without_any_borrowed_callback() {
    let (mut s, b) = fixture();
    let (leader, _, _) = add(&mut s, 100, 100);
    let (worker, _, _) = add(&mut s, 100, 101);
    // An arming worker does not own the process timer's later lifetime.
    s.replace_real_timer(leader, worker, at(0), at(10), at(7), Signal::SIGALRM)
        .unwrap();
    s.real_timers.retire_task(leader, worker);
    s.committed_time = at(10);
    s.step2b_process_timed();
    assert!(!s.backend_failed());
    assert_eq!(s.host_signal_attempts, 0);
    let p = b.publications.lock().unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].0, task(100, 100).process);
    assert_eq!(
        i32::from_ne_bytes(p[0].1.siginfo()[8..12].try_into().unwrap()),
        libc::SI_KERNEL
    );
    assert_eq!(
        s.real_timers.snapshot(leader, at(100)).unwrap().remaining,
        at(0)
    );
    assert!(s.blocked.timed_waiters.is_empty());
}

#[test]
fn masked_leader_and_unrelated_process_do_not_prevent_worker_sleep_selection() {
    let (mut s, b) = fixture();
    let (leader, mm, ls) = add(&mut s, 100, 100);
    let (worker, _, ws) = add(&mut s, 100, 101);
    add(&mut s, 200, 200);
    let lr = sleep(&mut s, leader, mm, ls, 100);
    let wr = sleep(&mut s, worker, mm, ws, 100);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 101),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    assert!(lr.try_read().is_none());
    let c = selected(&wr);
    assert_eq!(c.permit.task, task(100, 101));
    assert_eq!(
        s.blocked.timed_waiters.thread_deadline(leader),
        Some(at(100))
    );
    assert_eq!(s.blocked.timed_waiters.thread_deadline(worker), None);
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
    // A repeated maintenance pass cannot select a second recipient for the same pending signal.
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    assert_eq!(b.permits.lock().unwrap().len(), 1);
}

#[test]
fn no_handler_reenrolls_original_sleep_and_late_timeout_wins() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let response = sleep(&mut s, tid, mm, site, 100);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(t) => t,
        other => panic!("{other:?}"),
    };
    assert!(b.permits.lock().unwrap().is_empty());
    s.committed_time = at(150);
    let resumed = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site,
            response: resumed.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(s.blocked.timed_waiters.thread_deadline(tid), Some(at(100)));
    b.recipients.lock().unwrap().clear();
    s.step2b_process_timed();
    assert_eq!(s.blocked.timed_waiters.thread_deadline(tid), None);
    assert!(resumed.try_read().is_none());
    assert!(s.run_queue.contains_tid(tid));
    assert_eq!(
        s.next_turns[&tid]
            .req
            .try_read()
            .unwrap()
            .unwrap()
            .resources
            .keys()
            .next(),
        Some(&ResourceID::SleepUntil(at(100)))
    );
}

#[test]
fn natural_due_sleep_is_not_rewritten_as_interrupted() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let response = sleep(&mut s, tid, mm, site, 10);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.step2b_process_timed();
    s.select_parked_alarm().unwrap();
    assert!(response.try_read().is_none());
    assert!(b.permits.lock().unwrap().is_empty());
    assert!(s.run_queue.contains_tid(tid));
}

#[test]
fn return_permit_requires_real_grant_and_receipt_is_exactly_once() {
    let (mut s, b) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    let (peer, _, _) = add(&mut s, 100, 101);
    s.parked.running = Some(tid);
    assert_eq!(s.authorize_signal_boundary(task(100, 101)).unwrap(), None);
    let permit = s
        .authorize_signal_boundary(task(100, 100))
        .unwrap()
        .unwrap();
    assert_eq!(permit.site, None);
    assert_eq!(b.permits.lock().unwrap().len(), 1);
    let receipt = SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Caught,
    };
    s.consume_signal_boundary(receipt).unwrap();
    s.consume_signal_boundary(receipt).unwrap();
    assert!(
        s.consume_signal_boundary(SignalBoundaryReceipt {
            outcome: SignalBoundaryOutcome::NoHandler,
            ..receipt
        })
        .is_err()
    );
    assert!(s.next_turns.contains_key(&peer));
}

#[test]
fn committed_failure_reenters_only_after_scheduler_terminal_and_unlock() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    let (mut s, b) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    s.replace_real_timer(tid, tid, at(0), at(10), at(0), Signal::SIGALRM)
        .unwrap();
    b.fail_publication.store(true, Ordering::Relaxed);
    let s = Arc::new(Mutex::new(s));
    let weak = Arc::downgrade(&s);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    *b.failure_probe.lock().unwrap() = Some(Box::new(move || {
        let scheduler = weak.upgrade().unwrap();
        let scheduler = scheduler
            .try_lock()
            .expect("publisher must not retain scheduler lock");
        assert!(
            scheduler.backend_failed(),
            "terminal transition precedes backend notification"
        );
        counted.fetch_add(1, Ordering::Relaxed);
    }));
    {
        let mut scheduler = s.lock().unwrap();
        scheduler.committed_time = at(10);
        scheduler.step2b_process_timed();
        assert!(scheduler.backend_failed());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(scheduler.host_signal_attempts, 0);
    }
    super::signal_control::flush_signal_failures(&s);
    super::signal_control::flush_signal_failures(&s);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(b.publications.lock().unwrap().len(), 1);
}

fn polled_read(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    resource: ResourceID,
    attempt: u32,
) -> Ivar<SchedResponse> {
    let mut r = Resources::new(tid);
    r.insert(resource, Permission::W);
    r.poll_attempt = attempt;
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::PolledRead { site },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(r));
    let response = turn.resp.clone();
    s.run_queue.push_poller(tid, DEFAULT_PRIORITY, attempt);
    response
}

#[test]
fn polled_read_no_handler_restores_exact_request_and_queue_order() {
    assert_polled_read_restoration(false);
}

#[test]
fn promoted_polled_read_restores_exact_ready_request_and_queue_order() {
    assert_polled_read_restoration(true);
}

fn assert_polled_read_restoration(promote: bool) {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let (peer, _, _) = add(&mut s, 200, 200);
    let response = polled_read(&mut s, tid, mm, site, ResourceID::InternalIOPolling, 3);
    if promote {
        let original = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        let before_turn = s.turn;
        let before_time = s.committed_time;
        let (selected, selected_request, selected_response) = s.step3_peek().unwrap();
        assert_eq!(selected, tid);
        assert_eq!(selected_request, s.next_turns[&tid].req);
        assert_eq!(selected_response, response);
        assert!(
            s.step4_resource_block(tid, &original, &selected_response)
                .is_err()
        );
        assert_eq!(s.turn, before_turn + 1);
        assert_eq!(s.committed_time, before_time);
        assert!(!s.run_queue.tentative_pop_in_progress());
        assert_ne!(s.next_turns[&tid].req, selected_request);
        assert!(response.try_read().is_none());
    }
    let request = s.next_turns[&tid].req.clone();
    s.run_queue.push_poller(peer, DEFAULT_PRIORITY, 3);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    assert_ne!(s.next_turns[&tid].req, request);
    assert!(s.next_turns[&tid].req.try_read().is_none());
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let ticket = match ack.try_read().unwrap().unwrap() {
        FinishAck::AwaitResume(ticket) => ticket,
        other => panic!("{other:?}"),
    };
    assert!(b.permits.lock().unwrap().is_empty());
    let resumed = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site,
            response: resumed.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert!(!s.backend_failed());
    assert_eq!(s.next_turns[&tid].req, request);
    assert_ne!(s.next_turns[&tid].resp, response);
    assert_eq!(s.next_turns[&tid].resp, resumed);
    assert!(resumed.try_read().is_none());
    let restored = request.try_read().unwrap().unwrap();
    assert_eq!(restored.poll_attempt, if promote { 0 } else { 3 });
    assert_eq!(restored.resources.len(), 1);
    assert_eq!(
        restored.resources.get(&ResourceID::InternalIOPolling),
        Some(&Permission::W)
    );
    assert!(s.blocked.timed_waiters.thread_deadline(tid).is_none());
    assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(peer));
    s.run_queue.undo_tentative_pop();
    // The old single-use response is never changed into a normal grant.
    assert_eq!(selected(&response), c);
    if promote {
        // A second actual observation after response registration still owns
        // the same ready request. Resume must not discard promotion provenance.
        s.run_queue.push_back(tid, DEFAULT_PRIORITY);
        s.select_parked_alarm().unwrap();
        assert_eq!(selected(&resumed).site, site);
    }
}

#[test]
fn polled_read_rejects_wrong_resource_first_attempt_and_foreign_identity() {
    for (resource, attempt) in [
        (ResourceID::SleepUntil(at(100)), 1),
        (ResourceID::InternalIOPolling, 0),
    ] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let response = polled_read(&mut s, tid, mm, site, resource, attempt);
        let request = s.next_turns[&tid].req.clone();
        let queue = format!("{:?}", s.run_queue);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        assert_eq!(
            s.select_parked_alarm(),
            Err(SelectionFailure {
                pid: DetPid::from_raw(100),
                tid: Some(tid),
                failure: ProtocolFailure::Unsupported,
            })
        );
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(format!("{:?}", s.run_queue), queue);
        assert!(response.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
    for which in 0..3 {
        let (mut s, _) = fixture();
        let (tid, mm, mut site) = add(&mut s, 100, 100);
        let bad_mm = MmId::initial(DetTid::from_raw(999));
        if which == 0 {
            site.process.generation += 1;
        }
        if which == 1 {
            site.task_generation += 1;
        }
        assert!(
            s.install_resource_origin(
                tid,
                ResourceOrigin {
                    rpc: RpcOrigin::DirectRequestResources,
                    mm: if which == 2 { bad_mm } else { mm },
                    control: ControlCapability::PolledRead { site },
                }
            )
            .is_err()
        );
        assert!(s.next_turns[&tid].protocol.origin.is_none());
    }
}

#[test]
fn polled_read_resume_rejects_stale_or_duplicate_transport() {
    for mutation in 0..4 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let response = polled_read(&mut s, tid, mm, site, ResourceID::InternalIOPolling, 1);
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        s.select_parked_alarm().unwrap();
        let c = selected(&response);
        let ack = Ivar::new();
        s.post_control(
            tid,
            mm,
            ControlIntent::Finish {
                wait: c.continuation,
                lease: c.lease,
                site,
                finish: ObservationFinish::ResumeSameWait,
                ack: ack.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        let mut ticket = match ack.try_read().unwrap().unwrap() {
            FinishAck::AwaitResume(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        let mut returned_site = site;
        if mutation == 0 {
            ticket.next_epoch += 1;
        }
        if mutation == 1 {
            ticket.nonce += 1;
        }
        if mutation == 2 {
            returned_site.callback_nonce += 1;
        }
        let resumed = Ivar::new();
        s.post_control(
            tid,
            mm,
            ControlIntent::Resume {
                ticket,
                site: returned_site,
                response: resumed.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        if mutation == 3 {
            assert!(!s.backend_failed());
            s.post_control(
                tid,
                mm,
                ControlIntent::Resume {
                    ticket,
                    site,
                    response: Ivar::new(),
                },
            )
            .unwrap();
            s.drain_control_intents();
        }
        assert!(s.backend_failed(), "mutation {mutation}");
        assert!(resumed.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
}

#[test]
fn promoted_polled_read_refuses_foreign_request_response_epoch_and_origin() {
    for mutation in 0..4 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        polled_read(&mut s, tid, mm, site, ResourceID::InternalIOPolling, 2);
        let original = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        s.upgrade_polled_to_runnable(tid, &original);
        let turn = s.next_turns.get_mut(&tid).unwrap();
        match mutation {
            0 => turn.req = Ivar::full(turn.req.try_read().unwrap()),
            1 => turn.resp = Ivar::new(),
            2 => turn.protocol.epoch += 1,
            3 => {
                turn.protocol.origin.as_mut().unwrap().rpc = RpcOrigin::ResumeParkedRequest {
                    continuation: ContinuationId {
                        dettid: tid,
                        nonce: 999,
                    },
                    cycle: 1,
                }
            }
            _ => unreachable!(),
        }
        let req = turn.req.clone();
        let resp = turn.resp.clone();
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        assert_eq!(
            s.select_parked_alarm(),
            Err(SelectionFailure {
                pid: DetPid::from_raw(100),
                tid: Some(tid),
                failure: ProtocolFailure::Unsupported,
            })
        );
        assert_eq!(s.next_turns[&tid].req, req);
        assert!(resp.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
}

#[test]
fn terminal_boundary_retires_exact_scope_before_pending_rpc_and_preserves_duplicates() {
    for group in [false, true] {
        let (mut s, _) = fixture();
        let (tid, mm, _) = add(&mut s, 100, 100);
        let (peer, _, _) = add(&mut s, 100, 101);
        // A different process sharing this mm is not a member of the group.
        let (other, _, _) = add_with_mm(&mut s, 200, 200, mm);
        let worker_req = s.next_turns[&peer].req.clone();
        let worker_resp = s.next_turns[&peer].resp.clone();
        let leader_req = s.next_turns[&tid].req.clone();
        let mut resources = Resources::new(peer);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.poll_attempt = 1;
        worker_req.put(Ok(resources));
        s.run_queue.push_back(tid, DEFAULT_PRIORITY);
        s.run_queue.push_back(peer, DEFAULT_PRIORITY);
        s.run_queue.push_back(other, DEFAULT_PRIORITY);
        s.parked.running = Some(tid);
        let permit = s
            .authorize_signal_boundary(task(100, 100))
            .unwrap()
            .unwrap();
        let receipt = SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group,
                wait_status: 14,
            },
        };
        assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
        s.consume_signal_boundary(receipt).unwrap();
        assert!(!s.next_turns.contains_key(&tid));
        assert!(matches!(leader_req.try_read(), Some(Err(ThreadExited))));
        assert_eq!(s.next_turns.contains_key(&peer), !group);
        assert_eq!(
            matches!(worker_resp.try_read(), Some(SchedResponse::Signaled(None))),
            group
        );
        assert!(s.next_turns.contains_key(&other));
        assert_eq!(
            s.real_timers
                .task_identity(DetTid::from_raw(100), peer)
                .is_some(),
            !group
        );
        // Retired timer identities cannot invalidate an exact receipt replay.
        s.consume_signal_boundary(receipt).unwrap();
        for altered in [
            SignalBoundaryOutcome::Terminated {
                group: !group,
                wait_status: 14,
            },
            SignalBoundaryOutcome::Terminated {
                group,
                wait_status: 15,
            },
        ] {
            assert!(
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    outcome: altered,
                    ..receipt
                })
                .is_err()
            );
        }
        assert_eq!(s.parked.completed[&tid], receipt);
        assert!(s.run_queue.tentative_pop_in_progress());
        s.run_queue.undo_tentative_pop();
    }
}

#[test]
fn committed_exit_boundary_fences_later_turns_until_exact_terminal_receipt() {
    for group in [false, true] {
        let (mut s, backend) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        let (peer, _, _) = add(&mut s, 100, 101);
        s.run_queue.push_back(leader, DEFAULT_PRIORITY);
        s.run_queue.push_back(peer, DEFAULT_PRIORITY);

        s.reserve_exit_boundary(leader, DetPid::from_raw(100), mm, group)
            .unwrap();
        let fence = s.parked.exit_fences[&leader];
        assert_eq!(s.parked.permits[&leader], fence.permit);
        assert_eq!(backend.permits.lock().unwrap().as_slice(), &[fence.permit]);
        assert!(s.control_barrier());

        let wrong = SignalBoundaryReceipt {
            permit: fence.permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: !group,
                wait_status: 7 << 8,
            },
        };
        assert!(s.consume_signal_boundary(wrong).is_err());
        assert_eq!(s.parked.exit_fences[&leader], fence);
        assert!(s.control_barrier());

        let receipt = SignalBoundaryReceipt {
            outcome: SignalBoundaryOutcome::Terminated {
                group,
                wait_status: 7 << 8,
            },
            ..wrong
        };
        s.consume_signal_boundary(receipt).unwrap();
        assert!(!s.parked.exit_fences.contains_key(&leader));
        assert!(!s.parked.permits.contains_key(&leader));
        assert!(!s.control_barrier());
        assert!(!s.next_turns.contains_key(&leader));
        assert_eq!(s.next_turns.contains_key(&peer), !group);
        assert!(s.control_waiter().try_read().is_some());
        s.consume_signal_boundary(receipt).unwrap();
    }
}

#[test]
fn child_exit_publication_is_two_poll_and_retains_shadow_for_wnowait() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);

    assert_eq!(
        s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true),
        Ok(super::signal_control::ExitReserveMode::Controlled)
    );
    let fence = s.parked.exit_fences[&child];
    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(s.parked.child_exit_reservations.contains_key(&child_key));
    assert!(s.control_barrier(), "stages 1 and 2 form one barrier");

    let receipt = SignalBoundaryReceipt {
        permit: fence.permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 7 << 8,
        },
    };
    s.consume_signal_boundary(receipt).unwrap();
    assert!(!s.parked.exit_fences.contains_key(&child));
    assert!(matches!(
        s.parked.child_exit_reservations[&child_key].phase,
        ChildExitReservationPhase::AwaitingPublication {
            status: ExitStatus::Exited(7)
        }
    ));
    assert!(s.control_barrier(), "stage 2 survives stage-1 receipt");

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(7),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let admission_locked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_scheduler = Arc::downgrade(&scheduler);
    let probe_admission_locked = admission_locked.clone();
    *backend.child_publication_probe.lock().unwrap() = Some(Box::new(move || {
        let scheduler = probe_scheduler.upgrade().unwrap();
        assert!(
            scheduler.try_lock().is_err(),
            "publication must remain inside scheduler admission"
        );
        probe_admission_locked.store(true, std::sync::atomic::Ordering::Relaxed);
    }));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(admission_locked.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    {
        let s = scheduler.lock().unwrap();
        assert!(matches!(
            s.parked.child_exit_reservations[&child_key].phase,
            ChildExitReservationPhase::Published { .. }
        ));
        assert!(s.control_barrier());
    }
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    {
        let mut s = scheduler.lock().unwrap();
        assert!(!s.control_barrier());
        assert_eq!(
            s.parked.completed_child_exits[&child_key].completion,
            event.child_exit_completion().unwrap()
        );

        let spec = crate::types::ChildWaitSpec {
            selector: crate::types::ChildWaitSelector::Exact(DetPid::from_raw(200)),
            owner: None,
            exit_class: crate::types::ChildWaitExitClass::Sigchld,
        };
        assert_eq!(
            s.ready_child_wait(DetPid::from_raw(100), spec),
            Some(DetPid::from_raw(200))
        );
        assert_eq!(
            s.ready_child_wait(DetPid::from_raw(100), spec),
            Some(DetPid::from_raw(200)),
            "WNOWAIT requires the scheduler shadow to remain repeatedly observable"
        );
        assert!(s.consume_child_wait(DetPid::from_raw(100), DetPid::from_raw(200)));
        assert_eq!(s.ready_child_wait(DetPid::from_raw(100), spec), None);
    }

    // The exact duplicate is terminal and does not republish.
    let mut duplicate = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut duplicate), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut duplicate),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);

    // Neither a reused generation nor contradictory status can inherit the
    // terminal receipt.
    for stale in [
        BackendChildWaitEvent {
            child: SignalProcessId {
                generation: event.child.generation + 1,
                ..event.child
            },
            ..event
        },
        child_exit_event(event.parent, event.child, ExitStatus::Exited(8), true),
    ] {
        let mut stale = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
            scheduler.clone(),
            stale,
        ));
        assert!(matches!(poll_child_exit(&mut stale), Poll::Ready(Err(_))));
    }
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert!(scheduler.lock().unwrap().backend_failed());
}

#[test]
fn nonwaitable_child_auto_reaps_shadow_regardless_of_signal_effect() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    *backend.child_effect.lock().unwrap() = Some(ChildExitPublicationEffect::Queued);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 9 << 8,
        },
    })
    .unwrap();

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(9),
        false,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    let s = scheduler.lock().unwrap();
    assert!(!s.control_barrier());
    assert!(
        !s.logically_exited_processes
            .contains(&DetPid::from_raw(200))
    );
    assert_eq!(s.thread_tree.parent_process(&DetPid::from_raw(200)), None);
    assert_eq!(
        backend.child_publications.lock().unwrap()[0],
        event.child_exit_completion().unwrap()
    );
}

#[test]
fn post_commit_shadow_cleanup_failure_is_terminal_before_barrier_release() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 11 << 8,
        },
    })
    .unwrap();
    s.thread_tree.process_parent.remove(&DetPid::from_raw(200));

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(11),
        false,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Err(_))
    ));
    let s = scheduler.lock().unwrap();
    assert!(s.backend_failed());
    assert!(!s.control_barrier());
}

#[test]
fn final_process_classification_uses_live_direct_parent_not_transitive_root() {
    let (mut s, _) = fixture();
    let (root, root_mm, _) = add(&mut s, 100, 100);
    let _ = add_process_child(&mut s, 100, 200);
    let (grandchild, grandchild_mm, _) = add_process_child(&mut s, 200, 300);

    s.reserve_exit_boundary(root, DetPid::from_raw(100), root_mm, true)
        .unwrap();
    let root_permit = s.parked.exit_fences[&root].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: root_permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 0,
        },
    })
    .unwrap();
    assert!(matches!(
        s.parked.terminal_processes[&ProcessGeneration::from_backend(task(100, 100).process)].class,
        FinalProcessClass::Root
    ));

    s.reserve_exit_boundary(grandchild, DetPid::from_raw(300), grandchild_mm, true)
        .unwrap();
    let reservation =
        s.parked.child_exit_reservations[&ProcessGeneration::from_backend(task(300, 300).process)];
    assert_eq!(reservation.parent, task(200, 200).process);
    assert!(matches!(
        reservation.phase,
        ChildExitReservationPhase::AwaitingTerminalStatus
    ));
}

#[test]
fn direct_parent_terminal_child_is_classified_and_auto_reaped_without_callback() {
    let (mut s, backend) = fixture();
    let (root, root_mm, _) = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);

    s.reserve_exit_boundary(root, DetPid::from_raw(100), root_mm, true)
        .unwrap();
    let root_key = ProcessGeneration::from_backend(task(100, 100).process);
    assert!(
        !s.parked.child_exit_reservations.contains_key(&root_key),
        "a root process has no parent callback stage"
    );
    let root_permit = s.parked.exit_fences[&root].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: root_permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 4 << 8,
        },
    })
    .unwrap();
    assert!(matches!(
        s.parked.terminal_processes[&root_key].class,
        FinalProcessClass::Root
    ));
    assert!(!s.control_barrier());
    assert!(backend.child_publications.lock().unwrap().is_empty());

    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(
        !s.parked.child_exit_reservations.contains_key(&child_key),
        "a terminal direct parent causes backend teardown auto-reap"
    );
    let child_permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: child_permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 5 << 8,
        },
    })
    .unwrap();
    assert!(matches!(
        s.parked.terminal_processes[&child_key].class,
        FinalProcessClass::DirectParentTerminal { parent }
            if parent == task(100, 100).process
    ));
    assert!(
        !s.logically_exited_processes
            .contains(&DetPid::from_raw(200))
    );
    assert_eq!(s.thread_tree.parent_process(&DetPid::from_raw(200)), None);
    assert!(!s.control_barrier());
    assert!(backend.child_publications.lock().unwrap().is_empty());
}

#[test]
fn fatal_signal_final_transition_installs_stage_two_before_stage_one_retires() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    s.parked.running = Some(child);
    let permit = s
        .authorize_signal_boundary(task(200, 200))
        .unwrap()
        .unwrap();
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: libc::SIGKILL,
        },
    })
    .unwrap();

    let child_key = ProcessGeneration::from_backend(task(200, 200).process);
    assert!(!s.parked.permits.contains_key(&child));
    assert!(matches!(
        s.parked.child_exit_reservations[&child_key].phase,
        ChildExitReservationPhase::AwaitingPublication { status }
            if status == ExitStatus::from_raw(libc::SIGKILL)
    ));
    assert!(s.control_barrier());
}

#[test]
fn failed_after_commit_is_forwarded_unlocked_exactly_once_and_is_terminal() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 6 << 8,
        },
    })
    .unwrap();
    backend
        .fail_child_publication
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let scheduler = Arc::new(Mutex::new(s));
    let unlocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_scheduler = Arc::downgrade(&scheduler);
    let probe_unlocked = unlocked.clone();
    *backend.failure_probe.lock().unwrap() = Some(Box::new(move || {
        let scheduler = probe_scheduler.upgrade().unwrap();
        let mut scheduler = scheduler
            .try_lock()
            .expect("failure forwarding must run outside the scheduler lock");
        assert!(
            scheduler.backend_failed(),
            "terminal state must precede outside-lock failure forwarding"
        );
        probe_unlocked.store(true, std::sync::atomic::Ordering::Relaxed);
        let wake = scheduler.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(200),
            tid: reverie::Pid::from_raw(200),
            phase: "test child publication failure",
        });
        assert!(wake.is_none(), "failure must already be linearized");
        drop(scheduler);
        if let Some(wake) = wake {
            let _ = wake.send(());
        }
    }));

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(6),
        true,
    );
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    assert!(unlocked.load(std::sync::atomic::Ordering::Relaxed));
    assert!(scheduler.lock().unwrap().backend_failed());
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert_eq!(backend.child_failures.lock().unwrap().len(), 1);

    let mut duplicate = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut duplicate), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut duplicate),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert_eq!(backend.child_failures.lock().unwrap().len(), 1);
}

#[test]
fn rejected_child_publication_is_terminal_before_stage_two_wakes() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 10 << 8,
        },
    })
    .unwrap();
    backend
        .reject_child_publication
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(10),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    {
        let s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        assert!(s.control_barrier());
    }
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Err(_))
    ));
    let s = scheduler.lock().unwrap();
    assert!(s.backend_failed());
    assert!(!s.control_barrier());
    assert!(matches!(
        s.parked.completed_child_exits[&ProcessGeneration::from_backend(task(200, 200).process)]
            .result,
        ChildExitPublicationResult::RejectedBeforeCommit(reverie::Errno::EBADF)
    ));
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
    assert!(backend.child_failures.lock().unwrap().is_empty());
}

#[test]
fn child_failure_does_not_steal_pending_alarm_failure_wake() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 12 << 8,
        },
    })
    .unwrap();

    let alarm_process = task(100, 100).process;
    let alarm_waiter = s.backend_failure_waiter();
    let alarm_wake = s
        .report_backend_failure(reverie::BackendFailure {
            pid: alarm_process.tgid,
            tid: reverie::Pid::from_raw(100),
            phase: "retained alarm publication failure",
        })
        .unwrap();
    s.parked.failures.push(alarm_process);
    s.parked.failure_wakes.push(alarm_wake);
    backend
        .reject_child_publication
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let event = child_exit_event(
        alarm_process,
        task(200, 200).process,
        ExitStatus::Exited(12),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Err(_))
    ));
    assert!(alarm_waiter.clone().now_or_never().is_none());
    assert!(backend.failure_processes.lock().unwrap().is_empty());

    super::signal_control::flush_signal_failures(&scheduler);
    assert!(matches!(alarm_waiter.now_or_never(), Some(Ok(()))));
    assert_eq!(
        backend.failure_processes.lock().unwrap().as_slice(),
        &[alarm_process]
    );
}

#[test]
fn child_callback_drop_does_not_steal_pending_alarm_failure_wake() {
    use futures::FutureExt;

    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 13 << 8,
        },
    })
    .unwrap();
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(13),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));

    let alarm_process = task(100, 100).process;
    let alarm_waiter = {
        let mut s = scheduler.lock().unwrap();
        let alarm_waiter = s.backend_failure_waiter();
        let alarm_wake = s
            .report_backend_failure(reverie::BackendFailure {
                pid: alarm_process.tgid,
                tid: reverie::Pid::from_raw(100),
                phase: "retained alarm publication failure",
            })
            .unwrap();
        s.parked.failures.push(alarm_process);
        s.parked.failure_wakes.push(alarm_wake);
        alarm_waiter
    };

    drop(publication);
    assert!(alarm_waiter.clone().now_or_never().is_none());
    assert!(backend.failure_processes.lock().unwrap().is_empty());
    assert!(!scheduler.lock().unwrap().control_barrier());

    super::signal_control::flush_signal_failures(&scheduler);
    assert!(matches!(alarm_waiter.now_or_never(), Some(Ok(()))));
    assert_eq!(
        backend.failure_processes.lock().unwrap().as_slice(),
        &[alarm_process]
    );
}

#[test]
fn dropping_admitted_child_publication_cleans_barrier_and_fails_run() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 3 << 8,
        },
    })
    .unwrap();
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::Exited(3),
        true,
    );
    let scheduler = Arc::new(Mutex::new(s));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        scheduler.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(scheduler.lock().unwrap().control_barrier());
    drop(publication);
    let s = scheduler.lock().unwrap();
    assert!(!s.control_barrier());
    assert!(s.backend_failed());
    assert_eq!(backend.child_publications.lock().unwrap().len(), 1);
}

#[test]
fn controlled_exit_mode_never_enqueues_legacy_timed_sigchld() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    let response = Ivar::new();
    assert!(
        s.block_for_one_resource(
            child,
            &ResourceID::Exit {
                group: true,
                process: DetPid::from_raw(200),
                mm: child_mm,
            },
            &Permission::RW,
            None,
            &response,
        )
        .is_ok()
    );
    assert!(s.parked.exit_fences.contains_key(&child));
    assert!(s.blocked.timed_waiters.is_empty());

    let mut uncontrolled = Scheduler::new(&Config::default());
    assert_eq!(
        uncontrolled.reserve_exit_boundary(
            DetTid::from_raw(1),
            DetPid::from_raw(1),
            MmId::initial(DetTid::from_raw(1)),
            true,
        ),
        Ok(super::signal_control::ExitReserveMode::Uncontrolled)
    );
}

#[test]
fn exit_boundary_reservation_failure_has_no_local_effect() {
    let (mut s, backend) = fixture();
    let (tid, mm, _) = add(&mut s, 100, 100);
    backend
        .fail_reservation
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        s.reserve_exit_boundary(tid, DetPid::from_raw(100), mm, true),
        Err(ProtocolFailure::Identity)
    );
    assert!(s.parked.permits.is_empty());
    assert!(s.parked.exit_fences.is_empty());
    assert!(!s.control_barrier());
}

#[test]
fn selected_exit_reservation_failure_closes_tentative_pop_exactly_once() {
    let (mut s, backend) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.run_queue.push_back(child, DEFAULT_PRIORITY);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(child));
    backend
        .fail_reservation
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let response = Ivar::new();
    assert!(
        s.block_for_one_resource(
            child,
            &ResourceID::Exit {
                group: true,
                process: DetPid::from_raw(200),
                mm: child_mm,
            },
            &Permission::RW,
            None,
            &response,
        )
        .is_err()
    );
    assert!(s.backend_failed());
    assert!(!s.run_queue.tentative_pop_in_progress());
    assert!(s.run_queue.contains_tid(child));
    assert!(s.parked.exit_fences.is_empty());
    assert!(s.parked.child_exit_reservations.is_empty());
}

#[test]
fn terminal_boundary_refuses_stale_lifetime_and_preserves_shared_failure_authority() {
    for failed in [false, true] {
        let (mut s, _) = fixture();
        let (tid, _, _) = add(&mut s, 100, 100);
        let (peer, _, _) = add(&mut s, 100, 101);
        s.parked.running = Some(tid);
        let permit = s
            .authorize_signal_boundary(task(100, 100))
            .unwrap()
            .unwrap();
        let receipt = SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: 14,
            },
        };
        let mut stale = receipt;
        stale.permit.task.task_generation += 1;
        assert!(s.consume_signal_boundary(stale).is_err());
        assert!(s.next_turns.contains_key(&tid));
        assert!(s.next_turns.contains_key(&peer));
        let response = s.next_turns[&peer].resp.clone();
        s.next_turns[&peer].req.put(Ok(Resources::new(peer)));
        let failure_wake = failed.then(|| {
            s.report_backend_failure(reverie::BackendFailure {
                pid: reverie::Pid::from_raw(100),
                tid: reverie::Pid::from_raw(101),
                phase: "original failure",
            })
        });
        s.consume_signal_boundary(receipt).unwrap();
        assert_eq!(s.backend_failed(), failed);
        assert_eq!(
            matches!(response.try_read(), Some(SchedResponse::Signaled(None))),
            !failed
        );
        drop(failure_wake);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TimedTurnObservation {
    selected: DetTid,
    queued: Vec<DetTid>,
    deadlines: Vec<Option<LogicalTime>>,
    turn: u64,
    committed: LogicalTime,
    global: LogicalTime,
}

fn ordinary_sleep(s: &mut Scheduler, tid: DetTid, deadline: LogicalTime) {
    let mut request = Resources::new(tid);
    request.insert(ResourceID::SleepUntil(deadline), Permission::RW);
    s.next_turns[&tid].req.put(Ok(request));
    s.blocked.timed_waiters.insert(deadline, tid);
}

#[test]
fn timed_maintenance_preserves_reference_selection_and_clock() {
    use futures::FutureExt;

    let observe = |controlled| {
        let (mut s, backend) = fixture();
        s.kvm_shared_dequeue_timers = controlled;
        let mut expected_time = GlobalTime::new(&Config::default());
        let start = expected_time.as_nanos();
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let (a, _, _) = add(&mut s, 100, 100);
        let (b, _, _) = add(&mut s, 100, 101);
        let (r, _, _) = add(&mut s, 100, 102);
        // Two expired ordinary sleeps share a deadline and priority with R.
        // No process alarm, parked observation or host readiness is involved.
        ordinary_sleep(&mut s, a, start);
        ordinary_sleep(&mut s, b, start);
        s.next_turns[&r].req.put(Ok(Resources::new(r)));
        s.runqueue_push_back(r);
        let scheduler = Arc::new(Mutex::new(s));
        let mut last = Ok(Resources::new(r));
        let mut observations = Vec::new();
        for selected in [a, b] {
            let expected_clock = expected_time.add_scheduler_time();
            let result = do_a_turn_blocking(scheduler.clone(), global.clone(), &last)
                .now_or_never()
                .expect("all three requests were quiescent")
                .expect("an expired ordinary sleep must commit");
            let s = scheduler.lock().unwrap();
            let observation = TimedTurnObservation {
                selected: result.tid,
                queued: s.run_queue.tids().copied().collect(),
                deadlines: [a, b]
                    .map(|tid| s.blocked.timed_waiters.thread_deadline(tid))
                    .to_vec(),
                turn: s.turn,
                committed: s.committed_time,
                global: global.lock().unwrap().as_nanos(),
            };
            assert_eq!(observation.selected, selected, "{observation:?}");
            assert_eq!(observation.committed, expected_clock);
            assert_eq!(observation.global, expected_clock);
            assert_eq!(observation.turn, observations.len() as u64 + 1);
            if selected == a {
                assert_eq!(observation.queued, [r, a]);
                assert_eq!(observation.deadlines, [None, Some(start)]);
            } else {
                assert_eq!(observation.queued, [r, a, b]);
                assert_eq!(observation.deadlines, [None, None]);
            }
            assert!(!s.run_queue.tentative_pop_in_progress());
            assert!(!s.backend_failed());
            assert!(backend.publications.lock().unwrap().is_empty());
            assert!(backend.permits.lock().unwrap().is_empty());
            // Model only the next quiescent request, without adding guest work.
            s.next_turns[&selected]
                .req
                .put(Ok(Resources::new(selected)));
            observations.push(observation);
            last = Ok(result);
        }
        observations
    };
    assert_eq!(observe(false), observe(true));
}

#[test]
fn timed_maintenance_budget_survives_alarm_refresh_and_empty_queue() {
    use futures::FutureExt;

    // The first two cases differ only in whether initial maintenance really
    // popped an event. The last case spends its event on the alarm itself and
    // exercises the separate empty-queue wake after a hook crosses the deadline.
    for (initial_due_sleep, empty_queue) in [(true, false), (false, false), (false, true)] {
        let (mut s, backend) = fixture();
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let start = global.lock().unwrap().as_nanos();
        let (waiter, mm, site) = add(&mut s, 100, 100);
        let deadline = start + at(if empty_queue { 100 } else { 1_000 });
        let response = sleep(&mut s, waiter, mm, site, deadline.as_nanos());
        s.replace_real_timer(waiter, waiter, start, at(1), at(0), Signal::SIGALRM)
            .unwrap();
        global
            .lock()
            .unwrap()
            .add_extra_time(std::time::Duration::from_nanos(1));
        if !empty_queue {
            // A prior maintenance pass committed this pending alarm. The turn
            // under test therefore starts with either one or zero due sleeps.
            s.committed_time = start + at(1);
            s.step2b_process_timed();
        }
        backend.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        let mut sleepers = None;
        if !empty_queue {
            let (a, _, _) = add(&mut s, 100, 101);
            let (b, _, _) = add(&mut s, 100, 102);
            let (r, _, _) = add(&mut s, 100, 103);
            ordinary_sleep(
                &mut s,
                a,
                start + at(if initial_due_sleep { 1 } else { 200 }),
            );
            ordinary_sleep(&mut s, b, start + at(200));
            s.next_turns[&r].req.put(Ok(Resources::new(r)));
            s.runqueue_push_back(r);
            sleepers = Some((a, b, r));
        }
        let scheduler = Arc::new(Mutex::new(s));
        let last = Err(SkipTurn);
        let mut turn = Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
        assert!(turn.as_mut().now_or_never().is_none());
        let control = selected(&response);
        let ack = Ivar::new();
        {
            let mut s = scheduler.lock().unwrap();
            assert_eq!(s.turn, 0, "observation must not commit a guest turn");
            assert_eq!(s.committed_time, start + at(1));
            assert_eq!(backend.publications.lock().unwrap().len(), 1);
            if let Some((a, b, _)) = sleepers {
                assert_eq!(
                    s.blocked.timed_waiters.thread_deadline(a).is_none(),
                    initial_due_sleep
                );
                assert_eq!(
                    s.blocked.timed_waiters.thread_deadline(b),
                    Some(start + at(200))
                );
            }
            s.post_control(
                waiter,
                mm,
                ControlIntent::Finish {
                    wait: control.continuation,
                    lease: control.lease,
                    site,
                    finish: ObservationFinish::ResumeSameWait,
                    ack: ack.clone(),
                },
            )
            .unwrap();
        }
        // The daemon processes the real control intent, then waits for exact
        // resume registration. No host sleeps or product test hooks are needed.
        assert!(turn.as_mut().now_or_never().is_none());
        let ticket = match ack.try_read().unwrap().unwrap() {
            FinishAck::AwaitResume(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        assert!(backend.permits.lock().unwrap().is_empty());
        global
            .lock()
            .unwrap()
            .add_extra_time(std::time::Duration::from_nanos(299));
        backend.recipients.lock().unwrap().clear();
        let resumed = Ivar::new();
        scheduler
            .lock()
            .unwrap()
            .post_control(
                waiter,
                mm,
                ControlIntent::Resume {
                    ticket,
                    site,
                    response: resumed.clone(),
                },
            )
            .unwrap();
        let result = turn
            .as_mut()
            .now_or_never()
            .expect("resume restored a filled request");
        let mut s = scheduler.lock().unwrap();
        assert!(!s.backend_failed());
        assert!(!s.run_queue.tentative_pop_in_progress());
        assert_eq!(s.committed_time, start + at(300));
        assert_eq!(global.lock().unwrap().as_nanos(), start + at(300));
        if let Some((a, b, r)) = sleepers {
            assert_eq!(result.unwrap().tid, a);
            assert_eq!(s.turn, 1);
            assert_eq!(s.run_queue.tids().copied().collect::<Vec<_>>(), [r, a]);
            assert_eq!(
                s.blocked.timed_waiters.thread_deadline(b),
                Some(start + at(200))
            );
            assert_eq!(
                s.blocked.timed_waiters.thread_deadline(waiter),
                Some(deadline)
            );
            assert!(resumed.try_read().is_none());
        } else {
            assert!(result.is_err(), "empty-queue wake retains SkipTurn");
            assert_eq!(s.turn, 0);
            assert_eq!(s.run_queue.tids().copied().collect::<Vec<_>>(), [waiter]);
            assert!(s.blocked.timed_waiters.is_empty());
            assert!(resumed.try_read().is_none(), "wake must not grant early");
            drop(s);
            let granted = do_a_turn_blocking(scheduler.clone(), global.clone(), &Err(SkipTurn))
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(granted.tid, waiter);
            s = scheduler.lock().unwrap();
            assert_eq!(s.turn, 1);
            assert_eq!(s.committed_time, start + at(300));
            assert_eq!(global.lock().unwrap().as_nanos(), start + at(300));
            assert!(matches!(resumed.try_read(), Some(SchedResponse::Go(_))));
        }
    }
}

#[test]
fn parked_selection_failure_preserves_process_and_optional_task_identity() {
    use std::sync::atomic::Ordering;

    use futures::FutureExt;

    for task_failure in [false, true] {
        for unrelated_running in [false, true] {
            let (mut s, backend) = fixture();
            let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
            let start = global.lock().unwrap().as_nanos();
            let (leader, _, _) = add(&mut s, 100, 100);
            let (worker, mm, site) = add(&mut s, 100, 101);
            let (foreign, _, _) = add(&mut s, 200, 200);
            for tid in [leader, foreign] {
                s.next_turns[&tid].req.put(Ok(Resources::new(tid)));
                s.runqueue_push_back(tid);
            }
            let response = sleep(&mut s, worker, mm, site, (start + at(1_000)).as_nanos());
            s.replace_real_timer(leader, worker, start, at(500), at(0), Signal::SIGALRM)
                .unwrap();
            s.replace_real_timer(foreign, foreign, start, at(700), at(0), Signal::SIGALRM)
                .unwrap();
            s.parked.running = unrelated_running.then_some(foreign);
            if task_failure {
                backend.recipients.lock().unwrap().push(SignalRecipient {
                    task: task(100, 101),
                });
                backend.fail_reservation.store(true, Ordering::Relaxed);
            } else {
                *backend.fail_recipients.lock().unwrap() = Some(task(100, 100).process);
            }
            let failure_waiter = s.backend_failure_waiter();
            let scheduler = Arc::new(Mutex::new(s));
            let result = do_a_turn_blocking(scheduler.clone(), global.clone(), &Err(SkipTurn))
                .now_or_never()
                .expect("filled requests reach the injected selection failure");
            assert!(result.is_err());
            assert!(
                failure_waiter.now_or_never().unwrap().is_ok(),
                "failure wake is published after unlocking"
            );
            let mut s = scheduler.lock().unwrap();
            let expected = BackendFailureLocation {
                pid: reverie::Pid::from_raw(100),
                tid: task_failure.then_some(reverie::Tid::from_raw(101)),
                phase: if task_failure {
                    "KVM parked signal observation"
                } else {
                    "KVM parked signal process selection"
                },
            };
            assert_eq!(s.backend_failure, Some(expected));
            assert_eq!(s.parked.failure, Some(ProtocolFailure::Identity));
            assert!(s.backend_failed());
            assert!(s.real_timers.snapshot(leader, start).is_err());
            assert_eq!(
                s.real_timers.snapshot(foreign, start).unwrap().remaining,
                at(700)
            );
            assert_eq!(
                s.blocked.timed_waiters.next_deadline(),
                Some(start + at(700))
            );
            assert_eq!(s.turn, 0);
            assert_eq!(s.committed_time, start);
            assert_eq!(global.lock().unwrap().as_nanos(), start);
            assert!(
                response.try_read().is_none(),
                "failed reservation cannot admit an observation"
            );
            assert!(backend.permits.lock().unwrap().is_empty());
            assert!(backend.publications.lock().unwrap().is_empty());
            assert!(s.parked.failure_wakes.is_empty());
            assert!(!s.run_queue.tentative_pop_in_progress());
            assert!(
                s.report_backend_failure(reverie::BackendFailure {
                    pid: reverie::Pid::from_raw(200),
                    tid: reverie::Tid::from_raw(200),
                    phase: "later independent failure",
                })
                .is_none()
            );
            assert_eq!(
                s.backend_failure,
                Some(expected),
                "first failure remains authoritative"
            );
        }
    }
}
