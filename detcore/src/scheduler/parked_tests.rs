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
use reverie::BackendSignalControlMode;
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
use super::real_timer::TimerFailure;
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
    fixture_with_control(true)
}
/// A scheduler for a backend that reports the KVM capabilities. With
/// `installed`, it holds a fake installed process signal control, as a
/// sequentialized run on that backend does; without, the backend offered
/// nothing and the run is not controlled.
fn fixture_with_control(installed: bool) -> (Scheduler, Arc<Backend>) {
    let mut s = Scheduler::new(&Config {
        sequentialize_threads: true,
        ..Config::default().with_backend(|backend| {
            // The five behaviours the old `backend_is_kvm` identity flag selected.
            backend.provides_process_signal_control = true;
            backend.emulates_child_waits = true;
            backend.refuses_nonleader_exec_with_enosys = true;
            backend.failed_gettimeofday_may_store_host_time = false;
            backend.user_address_limit = Some(reverie::X86_64_FOUR_LEVEL_USER_ADDRESS_LIMIT);
            backend.needs_killed_thread_rpc_cancellation = true;
        })
    });
    let backend = Arc::new(Backend::default());
    if installed {
        assert_eq!(
            s.install_signal_control(
                Some(BackendSignalControl {
                    process: backend.clone(),
                }),
                true,
            )
            .unwrap(),
            BackendSignalControlMode::ToolControlled
        );
    }
    assert_eq!(s.signal_control_installed(), installed);
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
    s.note_thread_registration(tid, mm);
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
    s.note_thread_registration(child, mm);
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
        assert_eq!(s.control_barrier(), group);
        if group {
            s.consume_process_retirement(reverie::BackendProcessRetirement {
                process: task(100, 100).process,
                status: ExitStatus::Exited(7),
            })
            .unwrap();
        }
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
        let retirement = reverie::BackendProcessRetirement {
            process: event.child,
            status: ExitStatus::Exited(7),
        };
        s.consume_process_retirement(retirement).unwrap();
        s.consume_process_retirement(retirement).unwrap();
        assert!(s.parked.process_retirements.is_empty());
        assert!(!s.control_barrier());
        assert_eq!(backend.child_publications.lock().unwrap().len(), 1);

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
    assert!(s.control_barrier());
    s.consume_process_retirement(reverie::BackendProcessRetirement {
        process: task(100, 100).process,
        status: ExitStatus::SUCCESS,
    })
    .unwrap();

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
    assert!(s.control_barrier());
    s.consume_process_retirement(reverie::BackendProcessRetirement {
        process: task(100, 100).process,
        status: ExitStatus::Exited(4),
    })
    .unwrap();
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
    assert!(s.control_barrier());
    s.consume_process_retirement(reverie::BackendProcessRetirement {
        process: task(200, 200).process,
        status: ExitStatus::Exited(5),
    })
    .unwrap();
    assert!(!s.control_barrier());
    assert!(backend.child_publications.lock().unwrap().is_empty());
}

#[test]
fn process_retirement_without_controlled_terminal_preserves_root_and_orphan_state() {
    for terminal_parent in [false, true] {
        for tasks_retired in [false, true] {
            let (mut s, backend) = fixture();
            let (root, root_mm, _) = add(&mut s, 100, 100);
            let (leader, mm) = if terminal_parent {
                let (child, mm, _) = add_process_child(&mut s, 100, 200);
                s.reserve_exit_boundary(root, root, root_mm, true).unwrap();
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit: s.parked.exit_fences[&root].permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: true,
                        wait_status: 0,
                    },
                })
                .unwrap();
                s.consume_process_retirement(reverie::BackendProcessRetirement {
                    process: task(100, 100).process,
                    status: ExitStatus::SUCCESS,
                })
                .unwrap();
                (child, mm)
            } else {
                (root, root_mm)
            };
            let (worker, _, _) = add(&mut s, leader.as_raw(), leader.as_raw() + 1);
            if tasks_retired {
                // A previously exited leader can join a final faulting worker
                // after that worker's consuming hook retired the last task.
                s.reserve_exit_boundary(leader, leader, mm, false).unwrap();
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit: s.parked.exit_fences[&leader].permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: false,
                        wait_status: 7 << 8,
                    },
                })
                .unwrap();
                assert!(s.thread_is_logically_killed(leader));
                assert!(!s.thread_is_logically_killed(worker));
                s.logically_kill_thread(&worker, &leader, mm);
                assert!(s.real_timers.process_identity(leader).is_err());
            }
            let (other, other_mm, _) = add(&mut s, 900, 900);
            s.reserve_exit_boundary(other, other, other_mm, true)
                .unwrap();
            let foreign_fence = s.parked.exit_fences[&other];
            s.drain_control_intents();
            let waiter = s.control_waiter();
            assert!(waiter.try_read().is_none());
            let tasks_before: Vec<_> = s.next_turns.keys().copied().collect();
            let killed_before = s.logically_killed_threads.clone();
            let terminal_before = s.parked.terminal_processes.clone();
            let pending_before = s.parked.process_retirements.clone();
            let mut completed = s.parked.completed_process_retirements.clone();
            let event = reverie::BackendProcessRetirement {
                process: task(leader.as_raw(), leader.as_raw()).process,
                status: ExitStatus::from_raw(libc::SIGSEGV),
            };
            assert!(
                !s.parked
                    .terminal_processes
                    .contains_key(&ProcessGeneration::from_backend(event.process))
            );
            s.consume_process_retirement(event).unwrap();
            completed.insert(ProcessGeneration::from_backend(event.process), event);
            assert_eq!(s.parked.completed_process_retirements, completed);
            assert_eq!(s.parked.terminal_processes, terminal_before);
            assert_eq!(s.parked.process_retirements, pending_before);
            assert_eq!(
                s.next_turns.keys().copied().collect::<Vec<_>>(),
                tasks_before
            );
            assert_eq!(s.logically_killed_threads, killed_before);
            assert_eq!(s.parked.exit_fences[&other], foreign_fence);
            assert!(s.control_barrier());
            assert!(waiter.try_read().is_none());
            assert!(backend.child_publications.lock().unwrap().is_empty());
            assert!(!s.backend_failed());
            assert_eq!(s.turn, 0);
        }
    }
}

#[test]
fn process_retirement_without_controlled_terminal_preserves_duplicate_status_guard() {
    let (mut s, _) = fixture();
    let (leader, mm, _) = add(&mut s, 100, 100);
    let event = reverie::BackendProcessRetirement {
        process: task(100, 100).process,
        status: ExitStatus::from_raw(libc::SIGSEGV),
    };
    s.consume_process_retirement(event).unwrap();
    s.logically_kill_thread(&leader, &leader, mm);
    s.consume_process_retirement(event).unwrap();
    assert!(!s.backend_failed());
    let completed = s.parked.completed_process_retirements.clone();
    assert!(
        s.consume_process_retirement(reverie::BackendProcessRetirement {
            status: ExitStatus::from_raw(libc::SIGILL),
            ..event
        })
        .is_err()
    );
    assert!(s.backend_failed());
    assert_eq!(s.parked.completed_process_retirements, completed);
    assert!(s.parked.terminal_processes.is_empty());
    assert_eq!(s.turn, 0);
}

#[test]
fn process_retirement_without_controlled_terminal_rejects_unknown_and_reused_generation() {
    for defect in ["unknown", "generation", "reused generation"] {
        let (mut s, _) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        let mut identity = task(100, 100);
        let mut event = reverie::BackendProcessRetirement {
            process: identity.process,
            status: ExitStatus::from_raw(libc::SIGSEGV),
        };
        match defect {
            "unknown" => event.process.tgid = reverie::Pid::from_raw(999),
            "generation" => event.process.generation += 1,
            "reused generation" => {
                s.logically_kill_thread(&leader, &leader, mm);
                identity.process.generation += 1;
                identity.task_generation += 1;
                s.real_timers.bind(leader, leader, mm, identity).unwrap();
            }
            _ => unreachable!(),
        }
        let (other, other_mm, _) = add(&mut s, 900, 900);
        s.reserve_exit_boundary(other, other, other_mm, true)
            .unwrap();
        let fence = s.parked.exit_fences[&other];
        assert!(s.consume_process_retirement(event).is_err(), "{defect}");
        assert!(s.backend_failed(), "{defect}");
        assert_eq!(s.parked.exit_fences[&other], fence, "{defect}");
        assert!(s.control_barrier(), "{defect}");
        assert!(s.parked.completed_process_retirements.is_empty());
        assert!(s.parked.terminal_processes.is_empty());
        assert_eq!(s.turn, 0);
    }
}

#[test]
fn process_retirement_without_controlled_terminal_rejects_pending_process_obligations() {
    for obligation in ["permit", "exit fence", "child publication", "retirement"] {
        let (mut s, _) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        let event = reverie::BackendProcessRetirement {
            process: task(100, 100).process,
            status: ExitStatus::from_raw(libc::SIGSEGV),
        };
        let key = ProcessGeneration::from_backend(event.process);
        match obligation {
            "permit" => {
                // Ordinary signal delivery owns a permit without an exit fence.
                s.parked.running = Some(leader);
                s.authorize_signal_boundary(task(100, 100))
                    .unwrap()
                    .unwrap();
                assert!(s.parked.exit_fences.is_empty());
            }
            "exit fence" => {
                s.reserve_exit_boundary(leader, leader, mm, true).unwrap();
            }
            "child publication" => {
                s.parked.child_exit_reservations.insert(
                    key,
                    ChildExitReservation {
                        child: event.process,
                        parent: task(50, 50).process,
                        phase: ChildExitReservationPhase::AwaitingTerminalStatus,
                    },
                );
            }
            "retirement" => {
                s.parked.process_retirements.insert(key, event);
            }
            _ => unreachable!(),
        }
        let permits = s.parked.permits.clone();
        let fences = s.parked.exit_fences.clone();
        let children = s.parked.child_exit_reservations.clone();
        let retirements = s.parked.process_retirements.clone();
        let barrier = s.control_barrier();
        assert!(s.consume_process_retirement(event).is_err(), "{obligation}");
        assert!(s.backend_failed(), "{obligation}");
        assert_eq!(s.parked.permits, permits);
        assert_eq!(s.parked.exit_fences, fences);
        assert_eq!(s.parked.child_exit_reservations, children);
        assert_eq!(s.parked.process_retirements, retirements);
        assert_eq!(s.control_barrier(), barrier);
        assert!(s.parked.completed_process_retirements.is_empty());
        assert!(s.parked.terminal_processes.is_empty());
        assert_eq!(s.turn, 0);
    }
}

#[test]
fn process_retirement_cannot_replace_live_parent_child_publication() {
    let (mut s, _) = fixture();
    let _ = add(&mut s, 100, 100);
    let (child, mm, _) = add_process_child(&mut s, 100, 200);
    s.reserve_exit_boundary(child, child, mm, true).unwrap();
    let permit = s.parked.exit_fences[&child].permit;
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit,
        outcome: SignalBoundaryOutcome::Terminated {
            group: true,
            wait_status: 7 << 8,
        },
    })
    .unwrap();
    let key = ProcessGeneration::from_backend(task(200, 200).process);
    let reservation = s.parked.child_exit_reservations[&key];
    assert!(
        s.consume_process_retirement(reverie::BackendProcessRetirement {
            process: task(200, 200).process,
            status: ExitStatus::Exited(7),
        })
        .is_err()
    );
    assert!(s.backend_failed());
    assert!(s.control_barrier());
    assert_eq!(s.parked.child_exit_reservations[&key], reservation);
    assert!(s.parked.completed_process_retirements.is_empty());
}

#[test]
fn process_retirement_fences_root_and_orphan_group_exit_after_cancelling_peer_rpc() {
    use futures::FutureExt;

    for terminal_parent in [false, true] {
        for fatal in [false, true] {
            for worker_issuer in [false, true] {
                let (mut s, backend) = fixture();
                let (root, root_mm, _) = add(&mut s, 100, 100);
                let (leader, mm) = if terminal_parent {
                    let (child, mm, _) = add_process_child(&mut s, 100, 200);
                    s.reserve_exit_boundary(root, root, root_mm, true).unwrap();
                    let permit = s.parked.exit_fences[&root].permit;
                    s.consume_signal_boundary(SignalBoundaryReceipt {
                        permit,
                        outcome: SignalBoundaryOutcome::Terminated {
                            group: true,
                            wait_status: 0,
                        },
                    })
                    .unwrap();
                    s.consume_process_retirement(reverie::BackendProcessRetirement {
                        process: task(100, 100).process,
                        status: ExitStatus::SUCCESS,
                    })
                    .unwrap();
                    (child, mm)
                } else {
                    (root, root_mm)
                };
                let pid = leader.as_raw();
                let (worker, _, _) = add(&mut s, pid, pid + 1);
                let (issuer, peer) = if worker_issuer {
                    (worker, leader)
                } else {
                    (leader, worker)
                };
                let peer_response = s.next_turns[&peer].resp.clone();
                let mut polling = Resources::new(peer);
                polling.insert(ResourceID::InternalIOPolling, Permission::W);
                polling.poll_attempt = 1;
                s.next_turns[&peer].req.put(Ok(polling));
                let (other, _, _) = add(&mut s, 900, 900);
                s.next_turns[&other].req.put(Ok(Resources::new(other)));
                for tid in [issuer, peer, other] {
                    s.run_queue.push_back(tid, DEFAULT_PRIORITY);
                }

                let permit = if fatal {
                    s.parked.running = Some(issuer);
                    s.authorize_signal_boundary(task(pid, issuer.as_raw()))
                        .unwrap()
                        .unwrap()
                } else {
                    s.reserve_exit_boundary(issuer, leader, mm, true).unwrap();
                    s.parked.exit_fences[&issuer].permit
                };
                let status = if fatal {
                    ExitStatus::from_raw(libc::SIGTERM)
                } else {
                    ExitStatus::Exited(7)
                };
                s.consume_signal_boundary(SignalBoundaryReceipt {
                    permit,
                    outcome: SignalBoundaryOutcome::Terminated {
                        group: true,
                        wait_status: if fatal { libc::SIGTERM } else { 7 << 8 },
                    },
                })
                .unwrap();
                assert!(matches!(
                    peer_response.try_read(),
                    Some(SchedResponse::Signaled(None))
                ));
                assert!(s.thread_is_logically_killed(issuer));
                assert!(s.thread_is_logically_killed(peer));
                assert!(s.control_barrier());
                assert!(s.parked.exit_fences.is_empty());
                assert!(s.parked.child_exit_reservations.is_empty());
                assert!(backend.child_publications.lock().unwrap().is_empty());

                let scheduler = Arc::new(Mutex::new(s));
                let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
                let before = global.lock().unwrap().as_nanos();
                let last = Err(SkipTurn);
                let mut next =
                    Box::pin(do_a_turn_blocking(scheduler.clone(), global.clone(), &last));
                assert!(next.as_mut().now_or_never().is_none());
                assert_eq!(scheduler.lock().unwrap().turn, 0);
                assert_eq!(global.lock().unwrap().as_nanos(), before);
                let retirement = reverie::BackendProcessRetirement {
                    process: task(pid, pid).process,
                    status,
                };
                {
                    let mut s = scheduler.lock().unwrap();
                    s.consume_process_retirement(retirement).unwrap();
                    s.consume_process_retirement(retirement).unwrap();
                    assert!(!s.control_barrier());
                    assert!(!s.backend_failed());
                }
                let resumed = next
                    .as_mut()
                    .now_or_never()
                    .expect("retirement must release the ready unrelated task")
                    .unwrap();
                assert_eq!(resumed.tid, other);
                assert_eq!(scheduler.lock().unwrap().turn, 1);
            }
        }
    }
}

#[test]
fn process_retirement_rejects_early_stale_and_conflicting_receipts_before_release() {
    use futures::FutureExt;

    for defect in ["early", "generation", "status", "unknown", "duplicate"] {
        let (mut s, _) = fixture();
        let (leader, mm, _) = add(&mut s, 100, 100);
        s.reserve_exit_boundary(leader, leader, mm, true).unwrap();
        let permit = s.parked.exit_fences[&leader].permit;
        let exact = reverie::BackendProcessRetirement {
            process: task(100, 100).process,
            status: ExitStatus::Exited(7),
        };
        if defect != "early" {
            s.consume_signal_boundary(SignalBoundaryReceipt {
                permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group: true,
                    wait_status: 7 << 8,
                },
            })
            .unwrap();
        }
        if defect == "duplicate" {
            s.consume_process_retirement(exact).unwrap();
        }
        let mut wrong = exact;
        match defect {
            "generation" => wrong.process.generation += 1,
            "status" | "duplicate" => wrong.status = ExitStatus::Exited(8),
            "unknown" => wrong.process.tgid = reverie::Pid::from_raw(999),
            "early" => {}
            _ => unreachable!(),
        }
        let pending_before = s.parked.process_retirements.clone();
        let failure = s.backend_failure_waiter();
        assert!(s.consume_process_retirement(wrong).is_err(), "{defect}");
        assert!(s.backend_failed(), "{defect}");
        assert_eq!(s.parked.process_retirements, pending_before, "{defect}");
        assert_eq!(s.control_barrier(), defect != "duplicate", "{defect}");
        assert!(failure.clone().now_or_never().is_none());
        let wakes = s.take_signal_failure_wakes();
        assert_eq!(wakes.len(), 1);
        let scheduler = Arc::new(Mutex::new(s));
        for wake in wakes {
            let _ = wake.send(());
        }
        assert!(failure.now_or_never().unwrap().is_ok());
        // Terminal failure abandons the wait, without turning an incomplete
        // retirement into a successful scheduling continuation.
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        assert!(
            do_a_turn_blocking(scheduler.clone(), global, &Err(SkipTurn))
                .now_or_never()
                .unwrap()
                .is_err()
        );
        assert_eq!(scheduler.lock().unwrap().turn, 0);
    }
}

#[test]
fn process_retirement_keeps_empty_run_alive_until_cleanup_or_failure() {
    use futures::FutureExt;

    for fail in [false, true] {
        for group in [false, true] {
            let (mut s, _) = fixture();
            let (leader, mm, _) = add(&mut s, 100, 100);
            s.started_up.put(());
            s.reserve_exit_boundary(leader, leader, mm, group).unwrap();
            let permit = s.parked.exit_fences[&leader].permit;
            s.consume_signal_boundary(SignalBoundaryReceipt {
                permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group,
                    wait_status: 0,
                },
            })
            .unwrap();
            assert!(s.run_queue.is_empty());
            assert!(s.blocked.is_empty());
            let scheduler = Arc::new(Mutex::new(s));
            let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = events.clone();
            let mut daemon = Box::pin(sched_loop_external(
                scheduler.clone(),
                global,
                Arc::new(move |event| observed.lock().unwrap().push(event)),
            ));
            assert!(daemon.as_mut().now_or_never().is_none());
            assert!(
                !events
                    .lock()
                    .unwrap()
                    .contains(&"run queue empty; scheduler completed")
            );
            if fail {
                let wake = scheduler
                    .lock()
                    .unwrap()
                    .report_backend_failure(reverie::BackendFailure {
                        pid: reverie::Pid::from_raw(100),
                        tid: reverie::Pid::from_raw(100),
                        phase: "injected process cleanup failure",
                    })
                    .unwrap();
                let _ = wake.send(());
            } else {
                scheduler
                    .lock()
                    .unwrap()
                    .consume_process_retirement(reverie::BackendProcessRetirement {
                        process: task(100, 100).process,
                        status: ExitStatus::SUCCESS,
                    })
                    .unwrap();
            }
            assert!(daemon.as_mut().now_or_never().is_some());
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .contains(&"run queue empty; scheduler completed"),
                !fail
            );
            assert_eq!(scheduler.lock().unwrap().control_barrier(), fail);
        }
    }
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

    // A nonreturning self-SIGKILL has no ordinary signal-delivery boundary.
    // Its syscall handler therefore pre-reserves the same controlled Exit
    // fence as exit_group. Exercise that distinct producer path through the
    // real two-poll child publication rather than fabricating a callback pass.
    let (mut reserved, backend) = fixture();
    let _ = add(&mut reserved, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut reserved, 100, 200);
    reserved
        .reserve_exit_boundary(child, DetPid::from_raw(200), child_mm, true)
        .unwrap();
    let permit = reserved.parked.exit_fences[&child].permit;
    reserved
        .consume_signal_boundary(SignalBoundaryReceipt {
            permit,
            outcome: SignalBoundaryOutcome::Terminated {
                group: true,
                wait_status: libc::SIGKILL,
            },
        })
        .unwrap();
    let event = child_exit_event(
        task(100, 100).process,
        task(200, 200).process,
        ExitStatus::from_raw(libc::SIGKILL),
        true,
    );
    let reserved = Arc::new(Mutex::new(reserved));
    let mut publication = Box::pin(super::signal_control::ChildExitPublicationFuture::new(
        reserved.clone(),
        event,
    ));
    assert!(matches!(poll_child_exit(&mut publication), Poll::Pending));
    assert!(matches!(
        poll_child_exit(&mut publication),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(
        backend.child_publications.lock().unwrap().as_slice(),
        &[event.child_exit_completion().unwrap()]
    );
    assert!(!reserved.lock().unwrap().control_barrier());
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

    // The default/ptrace path has no backend control. Exercise the complete
    // Exit resource arm, not only its reservation helper, so the adjacent
    // controlled assertion cannot hide loss of the legacy timed SIGCHLD.
    let mut uncontrolled = Scheduler::new(&Config::default());
    let _ = add(&mut uncontrolled, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut uncontrolled, 100, 200);
    let parent = DetPid::from_raw(100);
    let child_process = DetPid::from_raw(200);
    let deadline = uncontrolled.committed_time + LogicalTime::from_nanos(1);
    assert!(
        uncontrolled
            .block_for_one_resource(
                child,
                &ResourceID::Exit {
                    group: true,
                    process: child_process,
                    mm: child_mm,
                },
                &Permission::RW,
                None,
                &Ivar::new(),
            )
            .is_ok()
    );
    assert!(uncontrolled.parked.exit_fences.is_empty());
    assert_eq!(
        uncontrolled
            .blocked
            .timed_waiters
            .iter()
            .collect::<Vec<_>>(),
        vec![(
            deadline,
            TimedEvent::SignalEvt(
                timed_waiters::SignalTimerId::ChildExit {
                    child: child_process,
                    parent,
                },
                parent,
                Signal::SIGCHLD,
            ),
        )]
    );
}

/// A scheduler for a backend whose supervisor reaps every guest process and
/// reports that physical exit (SaBRe's capability profile).
fn physical_exit_reporting_scheduler() -> Scheduler {
    Scheduler::new(&Config::default().with_backend(|backend| {
        backend.reports_physical_process_exits = true;
        backend.signal_interrupts_external_syscalls = true;
    }))
}

/// Grant `child`'s `exit_group` through the complete `Exit` resource arm, then
/// remove its only thread as the backend does before the real exit. The
/// child's physical exit is left pending, as it is until the supervisor reaps.
fn grant_child_exit_and_remove_thread(s: &mut Scheduler, child: DetTid, child_mm: MmId) {
    let process = DetPid::from_raw(child.as_raw());
    assert!(
        s.block_for_one_resource(
            child,
            &ResourceID::Exit {
                group: true,
                process,
                mm: child_mm,
            },
            &Permission::RW,
            None,
            &Ivar::new(),
        )
        .is_ok()
    );
    s.logically_kill_thread(&child, &process, child_mm);
}

/// Park `tid` in the run queue on an internal I/O poll, the request a signal
/// wakeup replaces with `InboundSignal`.
fn park_on_internal_poll(s: &mut Scheduler, tid: DetTid) {
    let mut polling = Resources::new(tid);
    polling.insert(ResourceID::InternalIOPolling, Permission::W);
    s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(polling));
    s.runqueue_push_back(tid);
}

fn requests_inbound_sigchld(s: &Scheduler, tid: DetTid) -> bool {
    s.next_turns
        .get(&tid)
        .and_then(|turn| turn.req.try_read())
        .is_some_and(|request| {
            request.is_ok_and(|resources| {
                resources
                    .resources
                    .contains_key(&ResourceID::InboundSignal(SigWrapper::from(
                        Signal::SIGCHLD,
                    )))
            })
        })
}

#[test]
fn reporting_backend_child_exit_wakes_the_parent_once_after_the_reap_without_a_signal() {
    let mut s = physical_exit_reporting_scheduler();
    let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let (parent, _, _) = add(&mut s, 100, 100);
    let (sibling, _, _) = add(&mut s, 300, 300);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    let child_process = DetPid::from_raw(200);
    park_on_internal_poll(&mut s, parent);
    park_on_internal_poll(&mut s, sibling);

    grant_child_exit_and_remove_thread(&mut s, child, child_mm);
    // The grant schedules no SIGCHLD of its own: the reap sends the real one.
    assert!(s.blocked.timed_waiters.is_empty());
    assert_eq!(
        s.child_exit_signals_owed.get(&child_process),
        Some(&DetPid::from_raw(100))
    );
    assert!(s.pending_physical_process_exits.contains(&child_process));

    // Must not: until the supervisor has reaped the child (and so the kernel
    // has generated the parent's SIGCHLD), no thread may run and the parent is
    // not woken, however many passes the scheduler makes.
    for _ in 0..3 {
        assert!(
            s.step2_process_blocked(&global_time).is_err(),
            "a runnable thread must not pass an unreaped child's owed wakeup"
        );
    }
    assert_eq!(s.owed_child_exit_wakeups, 0);
    assert!(!s.blocked.sigchld_ready.contains(&parent));
    assert!(!requests_inbound_sigchld(&s, parent));
    assert_eq!(
        s.run_queue.tids().copied().collect::<Vec<_>>(),
        vec![parent, sibling]
    );

    // Must wake: the first pass after the reap wakes the parent exactly once
    // and sends no host signal.
    assert!(s.complete_physical_process_exit(child_process));
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1);
    assert_eq!(s.host_signal_attempts, 0);
    assert!(s.child_exit_signals_owed.is_empty());
    // The parent's poll is replaced by the inbound signal. Requeuing it clears
    // its blocking entries, `sigchld_ready` included, as the timer path did.
    assert!(requests_inbound_sigchld(&s, parent));
    assert_eq!(
        s.run_queue.tids().copied().collect::<Vec<_>>(),
        vec![sibling, parent]
    );

    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1, "one child exit, one wakeup");
    assert_eq!(s.host_signal_attempts, 0);
}

#[test]
fn reporting_backend_child_exit_readmits_a_deferred_parent() {
    let mut s = physical_exit_reporting_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    s.blocked.sigchld_deferred.insert(DetPid::from_raw(100));

    grant_child_exit_and_remove_thread(&mut s, child, child_mm);
    assert!(s.step2_drain_prefix().is_err());
    assert!(s.blocked.sigchld_deferred.contains(&DetPid::from_raw(100)));

    // `release_all_physical_process_exits` is the supervisor's final drain;
    // it releases the hold exactly as an individual completion does.
    assert_eq!(s.release_all_physical_process_exits(), 1);
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1);
    assert_eq!(s.host_signal_attempts, 0);
    assert!(!s.blocked.sigchld_deferred.contains(&DetPid::from_raw(100)));
    assert!(s.blocked.sigchld_ready.contains(&DetPid::from_raw(100)));
    assert!(s.run_queue.contains_tid(parent));
}

#[test]
fn reporting_backend_child_exit_owes_nothing_to_a_parent_that_exited() {
    let mut s = physical_exit_reporting_scheduler();
    let (parent, parent_mm, _) = add(&mut s, 100, 100);
    let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
    let child_process = DetPid::from_raw(200);

    grant_child_exit_and_remove_thread(&mut s, child, child_mm);
    assert!(s.child_exit_signals_owed.contains_key(&child_process));
    s.logically_kill_thread(&parent, &DetPid::from_raw(100), parent_mm);
    assert!(s.child_exit_signals_owed.is_empty());

    // With no one owed a wakeup, the child's pending exit does not hold the
    // schedule, and its completion wakes nobody.
    assert!(s.step2_drain_prefix().is_ok());
    assert!(s.complete_physical_process_exit(child_process));
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 0);
}

#[test]
fn reporting_backend_exit_of_a_parent_keeps_the_wakeup_it_owes_its_own_parent() {
    let mut s = physical_exit_reporting_scheduler();
    let (grandparent, _, _) = add(&mut s, 100, 100);
    let (parent, parent_mm, _) = add_process_child(&mut s, 100, 200);
    let (child, child_mm, _) = add_process_child(&mut s, 200, 300);
    park_on_internal_poll(&mut s, grandparent);

    grant_child_exit_and_remove_thread(&mut s, child, child_mm);
    grant_child_exit_and_remove_thread(&mut s, parent, parent_mm);
    // The middle process's exit drops what is owed TO it, not what it owes.
    assert_eq!(
        s.child_exit_signals_owed.iter().collect::<Vec<_>>(),
        vec![(&DetPid::from_raw(200), &DetPid::from_raw(100))]
    );
    // The grandchild's pending exit no longer holds anything; the middle
    // process's does.
    assert!(s.step2_drain_prefix().is_err());
    assert!(s.complete_physical_process_exit(DetPid::from_raw(300)));
    assert!(s.step2_drain_prefix().is_err());
    assert!(s.complete_physical_process_exit(DetPid::from_raw(200)));
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1);
    assert!(requests_inbound_sigchld(&s, grandparent));
}

/// The waking set holding exactly `signals`, in the kernel mask layout.
fn rt_sigsuspend_wakers(signals: &[libc::c_int]) -> RtSigsuspendWakers {
    RtSigsuspendWakers(
        signals
            .iter()
            .fold(0, |set, signal| set | (1_u64 << (signal - 1))),
    )
}

/// Queue `waiter` with its `rt_sigsuspend` request, carrying `wakers`, as the
/// guest posts it before the scheduler grants it.
fn post_rt_sigsuspend_request(
    s: &mut Scheduler,
    waiter: DetTid,
    wakers: RtSigsuspendWakers,
) -> ResourceID {
    post_rt_sigsuspend_request_with_pending(s, waiter, wakers, RtSigsuspendWakers::default())
}

/// As `post_rt_sigsuspend_request`, for a call that found `pending_wakers`
/// (a subset of `wakers`) already pending in the kernel when it classified
/// its temporary mask. The request is published through `request_put`, as
/// the guest's RPC is, so the publication-time facts it records are present.
fn post_rt_sigsuspend_request_with_pending(
    s: &mut Scheduler,
    waiter: DetTid,
    wakers: RtSigsuspendWakers,
    pending_wakers: RtSigsuspendWakers,
) -> ResourceID {
    let rid =
        ResourceID::BlockingRtSigsuspend(ExternalOpId::new(waiter, 7), wakers, pending_wakers);
    let mut request = Resources::new(waiter);
    request.insert(rid.clone(), Permission::RW);
    let req = Ivar::new();
    s.next_turns.get_mut(&waiter).unwrap().req = req.clone();
    let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    s.request_put(&req, request, &global_time);
    s.runqueue_push_back(waiter);
    rid
}

/// Grant the posted `rt_sigsuspend` through the complete resource arm, which
/// records the waking set and leaves the thread executing the real call.
fn grant_posted_rt_sigsuspend(s: &mut Scheduler, waiter: DetTid, rid: &ResourceID) -> ExternalOpId {
    let ResourceID::BlockingRtSigsuspend(op_id, ..) = rid else {
        panic!("not an rt_sigsuspend request: {rid:?}");
    };
    assert_eq!(s.run_queue.tentative_pop_next(), Some(waiter));
    assert!(
        s.block_for_one_resource(waiter, rid, &Permission::RW, None, &Ivar::new())
            .is_err()
    );
    assert_eq!(s.blocked.rt_sigsuspend_blockers.get(&waiter), Some(op_id));
    *op_id
}

fn grant_rt_sigsuspend(
    s: &mut Scheduler,
    waiter: DetTid,
    wakers: RtSigsuspendWakers,
) -> ExternalOpId {
    let rid = post_rt_sigsuspend_request(s, waiter, wakers);
    grant_posted_rt_sigsuspend(s, waiter, &rid)
}

/// The interrupted real call returns and its continuation reaches the
/// scheduler.
fn post_rt_sigsuspend_continuation(s: &mut Scheduler, waiter: DetTid, op_id: ExternalOpId) {
    let mut request = Resources::new(waiter);
    request.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
    s.next_turns.get_mut(&waiter).unwrap().req = Ivar::full(Ok(request));
}

/// Must wake: a signal certain to end a SaBRe `rt_sigsuspend` has been sent,
/// so until its continuation arrives the thread is about to run, not
/// deadlocked. Before the fix, `step2d` judged it deadlocked on the first pass.
#[test]
fn signaled_rt_sigsuspend_awaits_its_continuation_instead_of_a_deadlock_verdict() {
    let mut s = physical_exit_reporting_scheduler();
    let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let initial_time = global_time.lock().unwrap().as_nanos();
    let (waiter, _, _) = add(&mut s, 100, 100);
    let op_id = grant_rt_sigsuspend(&mut s, waiter, rt_sigsuspend_wakers(&[libc::SIGCHLD]));

    s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
    for _ in 0..3 {
        assert!(s.step2_process_blocked(&global_time).is_err());
        assert!(
            s.take_terminal_deadlock().is_none(),
            "a thread whose rt_sigsuspend was just interrupted is not deadlocked"
        );
    }
    assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);

    // The first pass after the continuation arrives harvests it and settles
    // the wake.
    post_rt_sigsuspend_continuation(&mut s, waiter, op_id);
    assert!(s.step2_process_blocked(&global_time).is_ok());
    assert!(s.run_queue.contains_tid(waiter));
    assert!(s.blocked.rt_sigsuspend_blockers.is_empty());
    assert!(s.blocked.rt_sigsuspend_wakers.is_empty());
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(s.take_terminal_deadlock().is_none());
    assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);
}

/// Must wake: with another thread asleep, the same in-flight continuation must
/// not let the scheduler advance virtual time to the sleeper's deadline. On
/// which side of the jump the woken thread ran used to depend on host timing.
#[test]
fn signaled_rt_sigsuspend_holds_back_a_timer_fast_forward() {
    let mut s = physical_exit_reporting_scheduler();
    let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let initial_time = global_time.lock().unwrap().as_nanos();
    let (waiter, _, _) = add(&mut s, 100, 100);
    let (sleeper, _, _) = add(&mut s, 300, 300);
    let op_id = grant_rt_sigsuspend(&mut s, waiter, rt_sigsuspend_wakers(&[libc::SIGUSR1]));
    s.blocked
        .timed_waiters
        .insert(initial_time + LogicalTime::from_nanos(1_000), sleeper);

    s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGUSR1));
    for _ in 0..3 {
        assert!(s.step2_process_blocked(&global_time).is_err());
    }
    assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);
    assert!(!s.run_queue.contains_tid(sleeper));

    post_rt_sigsuspend_continuation(&mut s, waiter, op_id);
    assert!(s.step2_process_blocked(&global_time).is_ok());
    assert!(s.run_queue.contains_tid(waiter));
    assert!(!s.run_queue.contains_tid(sleeper));
    assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);
}

/// Must wake: a signal sent while the request still waits for its grant is
/// held pending by the backend, so the real call returns at once; the wake it
/// owes survives the grant.
#[test]
fn rt_sigsuspend_wake_sent_before_the_grant_stays_owed_after_it() {
    let mut s = physical_exit_reporting_scheduler();
    let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
    let (waiter, _, _) = add(&mut s, 100, 100);
    let rid = post_rt_sigsuspend_request(&mut s, waiter, rt_sigsuspend_wakers(&[libc::SIGCHLD]));

    s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
    let op_id = grant_posted_rt_sigsuspend(&mut s, waiter, &rid);
    assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));

    assert!(s.step2_process_blocked(&global_time).is_err());
    assert!(s.take_terminal_deadlock().is_none());
    post_rt_sigsuspend_continuation(&mut s, waiter, op_id);
    assert!(s.step2_process_blocked(&global_time).is_ok());
    assert!(s.run_queue.contains_tid(waiter));
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
}

/// Must not: when the signal is not certain to end the call, nothing is owed
/// and the existing verdict stands: the scheduler does not wait.
#[test]
fn rt_sigsuspend_wake_is_owed_only_when_the_signal_certainly_ends_the_call() {
    let sigchld_only = rt_sigsuspend_wakers(&[libc::SIGCHLD]);
    // (case, backend lets a signal interrupt the real call, waking set,
    //  a second live thread in the waiter's process)
    let cases = [
        (
            "signal outside the waking set (masked, SIG_DFL or SIG_IGN)",
            true,
            rt_sigsuspend_wakers(&[libc::SIGUSR1]),
            false,
        ),
        (
            "empty waking set",
            true,
            RtSigsuspendWakers::default(),
            false,
        ),
        (
            "backend whose signals do not interrupt the real call",
            false,
            sigchld_only,
            false,
        ),
        (
            "another live thread could take the signal or change its action",
            true,
            sigchld_only,
            true,
        ),
    ];
    for (case, interrupts, wakers, second_thread) in cases {
        let mut s = Scheduler::new(&Config::default().with_backend(|backend| {
            backend.reports_physical_process_exits = interrupts;
            backend.signal_interrupts_external_syscalls = interrupts;
        }));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let (waiter, _, _) = add(&mut s, 100, 100);
        if second_thread {
            add(&mut s, 100, 101);
        }
        grant_rt_sigsuspend(&mut s, waiter, wakers);

        s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGCHLD));
        assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
        assert!(s.step2c_process_io_blockers().is_ok(), "{case}");
        if !second_thread {
            assert!(s.step2_process_blocked(&global_time).is_err(), "{case}");
            let report = s
                .take_terminal_deadlock()
                .unwrap_or_else(|| panic!("{case}: the deadlock verdict must stand"));
            assert!(
                report.contains("thread(s) waiting in rt_sigsuspend with no possible signal"),
                "{case}: unexpected report:\n{report}"
            );
        }
    }
}

/// Must not: a thread the scheduler cannot place in a process, and a thread
/// parked on something other than `rt_sigsuspend`, are owed nothing; a wake
/// owed to a thread that is then killed does not outlive it.
#[test]
fn rt_sigsuspend_wake_is_owed_only_to_a_known_rt_sigsuspend_thread() {
    let mut s = physical_exit_reporting_scheduler();
    let stranger = DetTid::from_raw(400);
    s.priorities.insert(stranger, DEFAULT_PRIORITY);
    s.next_turns.insert(
        stranger,
        ThreadNextTurn {
            dettid: stranger,
            child_tid_addr: 0,
            req: Ivar::new(),
            resp: Ivar::new(),
            protocol: Default::default(),
        },
    );
    grant_rt_sigsuspend(&mut s, stranger, rt_sigsuspend_wakers(&[libc::SIGCHLD]));
    s.notify_signal_pending(stranger, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());

    let (waiter, waiter_mm, _) = add(&mut s, 100, 100);
    grant_rt_sigsuspend(&mut s, waiter, rt_sigsuspend_wakers(&[libc::SIGCHLD]));
    s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
    s.logically_kill_thread(&waiter, &DetPid::from_raw(100), waiter_mm);
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(!s.blocked.rt_sigsuspend_wakers.contains_key(&waiter));

    let (poller, _, _) = add(&mut s, 300, 300);
    park_on_internal_poll(&mut s, poller);
    s.notify_signal_pending(poller, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
}

/// A child's exit owes its parent's `rt_sigsuspend` a wake only for the
/// child's real exit signal: the kernel sends that signal, not always SIGCHLD,
/// and none at all for an exit signal of 0.
#[test]
fn child_exit_owes_an_rt_sigsuspend_wake_only_for_its_exit_signal() {
    for (exit_signal, owed) in [(libc::SIGCHLD, true), (libc::SIGUSR1, false), (0, false)] {
        let mut s = physical_exit_reporting_scheduler();
        let (parent, _, _) = add(&mut s, 100, 100);
        let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
        let child_process = DetPid::from_raw(200);
        s.thread_tree
            .process_wait
            .get_mut(&child_process)
            .unwrap()
            .exit_signal = exit_signal;
        grant_rt_sigsuspend(&mut s, parent, rt_sigsuspend_wakers(&[libc::SIGCHLD]));

        grant_child_exit_and_remove_thread(&mut s, child, child_mm);
        assert!(s.complete_physical_process_exit(child_process));
        // An owed wake holds the pass until the waiter's continuation arrives.
        assert_eq!(
            s.step2_drain_prefix().is_err(),
            owed,
            "exit signal {exit_signal}"
        );
        assert_eq!(s.owed_child_exit_wakeups, 1);
        assert_eq!(
            s.blocked.rt_sigsuspend_wake_owed.contains(&parent),
            owed,
            "exit signal {exit_signal}"
        );
    }
}

/// The wake owed for a child's exit follows the signal Linux actually sends,
/// after exec changes it. A clone child created with SIGUSR1 sends SIGCHLD once
/// it has exec'd (`de_thread`) or once its parent has exec'd since creating it
/// (`do_notify_parent`). Must wake: a parent waiting for SIGCHLD. Must not: a
/// parent waiting only for SIGUSR1, which the kernel no longer sends; owing
/// that wake would leave the waiter unwoken and hang the scheduler.
#[test]
fn child_exit_wake_follows_the_signal_after_exec() {
    for (child_execs, parent_execs, wakers, owed) in [
        (true, false, libc::SIGCHLD, true),
        (true, false, libc::SIGUSR1, false),
        (false, true, libc::SIGCHLD, true),
        (false, true, libc::SIGUSR1, false),
        (false, false, libc::SIGUSR1, true),
        (false, false, libc::SIGCHLD, false),
    ] {
        let case = format!("child exec {child_execs}, parent exec {parent_execs}, waker {wakers}");
        let mut s = physical_exit_reporting_scheduler();
        let (parent, _, _) = add(&mut s, 100, 100);
        let (child, child_mm, _) = add_process_child(&mut s, 100, 200);
        let child_process = DetPid::from_raw(200);
        s.thread_tree
            .process_wait
            .get_mut(&child_process)
            .unwrap()
            .exit_signal = libc::SIGUSR1;
        if child_execs {
            s.thread_tree.note_successful_exec(child_process);
        }
        if parent_execs {
            s.thread_tree.note_successful_exec(DetPid::from_raw(100));
        }
        grant_rt_sigsuspend(&mut s, parent, rt_sigsuspend_wakers(&[wakers]));

        grant_child_exit_and_remove_thread(&mut s, child, child_mm);
        assert!(s.complete_physical_process_exit(child_process), "{case}");
        // An owed wake holds the pass until the waiter's continuation arrives.
        assert_eq!(s.step2_drain_prefix().is_err(), owed, "{case}");
        assert_eq!(s.owed_child_exit_wakeups, 1, "{case}");
        assert_eq!(
            s.blocked.rt_sigsuspend_wake_owed.contains(&parent),
            owed,
            "{case}"
        );
    }
}

/// Make `tid` ordinary runnable work at its default priority.
fn queue_ordinary_work(s: &mut Scheduler, tid: DetTid) {
    s.next_turns.get_mut(&tid).unwrap().req = Ivar::full(Ok(Resources::new(tid)));
    s.runqueue_push_back(tid);
}

/// Let `waiter`'s owed `rt_sigsuspend` wake settle under one of the two host
/// timings: the interrupted call's continuation reaches the scheduler before
/// its next pass, or only after several passes. Until it arrives, every pass
/// must hold: no queued thread may be selected, no due timer taken, and
/// virtual time must stand still. Returns the run queue after the first pass
/// that goes through.
fn settle_owed_rt_sigsuspend_wake(
    s: &mut Scheduler,
    global_time: &Arc<Mutex<GlobalTime>>,
    waiter: DetTid,
    op_id: ExternalOpId,
    continuation_first: bool,
) -> Vec<DetTid> {
    let time = global_time.lock().unwrap().as_nanos();
    if !continuation_first {
        let queued: Vec<DetTid> = s.run_queue.tids().copied().collect();
        let timers = s.blocked.timed_waiters.len();
        for _ in 0..3 {
            assert!(
                s.step2_process_blocked(global_time).is_err(),
                "no turn may run while an owed wake's signal can still merge with another"
            );
            assert!(s.take_terminal_deadlock().is_none());
        }
        assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
        assert_eq!(s.run_queue.tids().copied().collect::<Vec<_>>(), queued);
        assert_eq!(s.blocked.timed_waiters.len(), timers);
        assert_eq!(global_time.lock().unwrap().as_nanos(), time);
    }
    post_rt_sigsuspend_continuation(s, waiter, op_id);
    assert!(s.step2_process_blocked(global_time).is_ok());
    assert!(s.take_terminal_deadlock().is_none());
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(!s.blocked.rt_sigsuspend_blockers.contains_key(&waiter));
    assert!(!s.blocked.rt_sigsuspend_wakers.contains_key(&waiter));
    assert_eq!(global_time.lock().unwrap().as_nanos(), time);
    s.run_queue.tids().copied().collect()
}

/// Must wake, at one point on every run: a sole-thread parent suspended for a
/// caught SIGCHLD, a first child that has exited and been reaped, and a second
/// child that is runnable. Until the parent's interrupted call has posted its
/// continuation, the first child's SIGCHLD may still be pending for it, and a
/// second SIGCHLD sent then merges with it or not depending on host timing:
/// two handler runs on one run, one on another. So no turn may run, which
/// keeps the second child from exiting, and no due timer may be taken. Both
/// arrival orders of the continuation end in the same run queue, with the
/// parent first. Before the fix the first pass let the second child run.
#[test]
fn owed_rt_sigsuspend_wake_holds_every_turn_until_its_continuation() {
    let mut outcomes = Vec::new();
    for continuation_first in [false, true] {
        let mut s = physical_exit_reporting_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let (parent, _, _) = add(&mut s, 100, 100);
        let (first, first_mm, _) = add_process_child(&mut s, 100, 200);
        let (second, _, _) = add_process_child(&mut s, 100, 300);
        let (sleeper, _, _) = add(&mut s, 400, 400);
        let op_id = grant_rt_sigsuspend(&mut s, parent, rt_sigsuspend_wakers(&[libc::SIGCHLD]));
        queue_ordinary_work(&mut s, second);
        let due = s.committed_time;
        s.blocked.timed_waiters.insert(due, sleeper);

        grant_child_exit_and_remove_thread(&mut s, first, first_mm);
        assert!(s.complete_physical_process_exit(DetPid::from_raw(200)));
        let queue =
            settle_owed_rt_sigsuspend_wake(&mut s, &global_time, parent, op_id, continuation_first);
        assert_eq!(s.owed_child_exit_wakeups, 1);
        assert_eq!(s.host_signal_attempts, 0);
        assert!(s.blocked.timed_waiters.thread_deadline(sleeper).is_none());
        assert_eq!(queue, vec![parent, sleeper, second]);
        outcomes.push(queue);
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

/// Must wake, at one point on every run, for each of two child exits that
/// reach a parent through two successive suspends. This is the shape that
/// matters when SIGCHLD is blocked outside the suspends: between them the
/// signal stays pending in the kernel, so a second child's SIGCHLD that
/// arrives before the first was taken would merge with it and leave nothing
/// for the second suspend, which would then sleep on one run and return at
/// once on another. The scheduler never sees the original mask; it keeps the
/// two exits apart by holding every turn until each owed continuation has
/// posted. The second exit, sent while the second suspend still waits for its
/// grant, stays pending because the waking set was blocked before that
/// request was published; it is owed across the grant and held the same way.
#[test]
fn successive_owed_rt_sigsuspend_wakes_each_hold_until_their_continuation() {
    let mut outcomes = Vec::new();
    for continuation_first in [false, true] {
        let mut s = physical_exit_reporting_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let sigchld = rt_sigsuspend_wakers(&[libc::SIGCHLD]);
        let (parent, _, _) = add(&mut s, 100, 100);
        let (first, first_mm, _) = add_process_child(&mut s, 100, 200);
        let (second, second_mm, _) = add_process_child(&mut s, 100, 300);
        let (third, _, _) = add_process_child(&mut s, 100, 400);
        let (sleeper, _, _) = add(&mut s, 500, 500);

        // First suspend; the first child exits and is reaped.
        let first_op = grant_rt_sigsuspend(&mut s, parent, sigchld);
        grant_child_exit_and_remove_thread(&mut s, first, first_mm);
        assert!(s.complete_physical_process_exit(DetPid::from_raw(200)));
        let queue = settle_owed_rt_sigsuspend_wake(
            &mut s,
            &global_time,
            parent,
            first_op,
            continuation_first,
        );
        assert_eq!(queue, vec![parent]);

        // The parent's continuation turn runs, and it suspends again.
        assert!(s.run_queue.remove_tid(parent));
        let second_request = post_rt_sigsuspend_request(&mut s, parent, sigchld);

        // The second child exits before that suspend is granted. Its wake is
        // owed to the pending request, which holds no turn yet: nothing can
        // take the signal from the kernel until the real call runs.
        grant_child_exit_and_remove_thread(&mut s, second, second_mm);
        assert!(s.complete_physical_process_exit(DetPid::from_raw(300)));
        assert!(s.step2_process_blocked(&global_time).is_ok());
        assert_eq!(s.owed_child_exit_wakeups, 2);
        assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&parent));

        // Granting the suspend keeps the wake owed, and from then on every
        // turn and timer waits for its continuation.
        let second_op = grant_posted_rt_sigsuspend(&mut s, parent, &second_request);
        queue_ordinary_work(&mut s, third);
        let due = s.committed_time;
        s.blocked.timed_waiters.insert(due, sleeper);
        let queue = settle_owed_rt_sigsuspend_wake(
            &mut s,
            &global_time,
            parent,
            second_op,
            continuation_first,
        );
        assert_eq!(s.owed_child_exit_wakeups, 2);
        assert_eq!(s.host_signal_attempts, 0);
        assert_eq!(queue, vec![parent, sleeper, third]);
        outcomes.push(queue);
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

/// A parent `P` with queued work, a child `Q` suspended for a caught SIGCHLD,
/// and Q's own child `A`, plus a sleeper due now. `Q` is the only thread of
/// its process. Returns (P, Q, Q's suspend, A, A's mm, sleeper).
fn killed_waiter_fixture(
    s: &mut Scheduler,
) -> (DetTid, DetTid, ExternalOpId, DetTid, MmId, DetTid) {
    let (parent, _, _) = add(s, 100, 100);
    let (waiter, _, _) = add_process_child(s, 100, 200);
    let (grandchild, grandchild_mm, _) = add_process_child(s, 200, 300);
    let (sleeper, _, _) = add(s, 400, 400);
    let op_id = grant_rt_sigsuspend(s, waiter, rt_sigsuspend_wakers(&[libc::SIGCHLD]));
    queue_ordinary_work(s, parent);
    let due = s.committed_time;
    s.blocked.timed_waiters.insert(due, sleeper);
    (parent, waiter, op_id, grandchild, grandchild_mm, sleeper)
}

/// Must not hang, and must decide the same way on every run: a SaBRe child
/// suspended for SIGCHLD is killed by a death the scheduler sees no guest
/// `SIGKILL` edge for (an external kill; the edge itself, for a positive PID
/// or a process group, is covered by the `sigkill_edge_*` and
/// `process_group_sigkill_*` tests), and the child's own child then
/// exits. SaBRe runs no exit hook for a thread killed outright, so the dead
/// waiter stays registered and the grandchild's exit owes it a wake it will
/// never take: before the fix the scheduler held every turn for a continuation
/// that could not come, forever. The supervisor's report of the death releases the
/// hold, whether it reaches the scheduler before the wake is owed (no hold at
/// all) or only after several held passes. Both orders end in the same run
/// queue, with the surviving parent's work and the sleeper admitted, and the
/// dead waiter keeping its blocking entries.
#[test]
fn killed_rt_sigsuspend_waiter_releases_the_hold_in_either_report_order() {
    let mut outcomes = Vec::new();
    for report_first in [true, false] {
        let mut s = physical_exit_reporting_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let time = global_time.lock().unwrap().as_nanos();
        let (parent, waiter, op_id, grandchild, grandchild_mm, sleeper) =
            killed_waiter_fixture(&mut s);
        // A death with no guest SIGKILL edge: nothing retires the waiter
        // logically (the thread-directed hook is DBT's alone).
        s.note_process_sigkill(waiter, DetPid::from_raw(200));
        assert!(s.next_turns.contains_key(&waiter));

        grant_child_exit_and_remove_thread(&mut s, grandchild, grandchild_mm);
        assert!(s.complete_physical_process_exit(DetPid::from_raw(300)));
        if report_first {
            s.note_physical_thread_exit(waiter);
        } else {
            let queued: Vec<DetTid> = s.run_queue.tids().copied().collect();
            for _ in 0..3 {
                assert!(s.step2_process_blocked(&global_time).is_err());
                assert!(s.take_terminal_deadlock().is_none());
            }
            assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
            assert_eq!(s.run_queue.tids().copied().collect::<Vec<_>>(), queued);
            assert!(s.blocked.timed_waiters.thread_deadline(sleeper).is_some());
            s.note_physical_thread_exit(waiter);
        }
        assert!(s.step2_process_blocked(&global_time).is_ok());
        assert!(s.take_terminal_deadlock().is_none());
        assert_eq!(s.owed_child_exit_wakeups, 1);
        assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
        assert_eq!(s.blocked.rt_sigsuspend_blockers.get(&waiter), Some(&op_id));
        assert!(s.blocked.rt_sigsuspend_wakers.contains_key(&waiter));
        assert_eq!(global_time.lock().unwrap().as_nanos(), time);
        let queue: Vec<DetTid> = s.run_queue.tids().copied().collect();
        assert!(queue.contains(&parent) && queue.contains(&sleeper));
        assert!(!queue.contains(&waiter));
        outcomes.push(queue);
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

/// Must wake: a waiter whose continuation was already posted when it died
/// (with no guest `SIGKILL` edge, as above) has taken the owed signal, so the
/// death report leaves its wake alone and the first pass harvests the
/// continuation exactly as if no report had come.
#[test]
fn killed_rt_sigsuspend_waiter_keeps_a_wake_whose_continuation_was_posted() {
    let mut outcomes = Vec::new();
    for reported in [false, true] {
        let mut s = physical_exit_reporting_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let (parent, waiter, op_id, grandchild, grandchild_mm, sleeper) =
            killed_waiter_fixture(&mut s);
        grant_child_exit_and_remove_thread(&mut s, grandchild, grandchild_mm);
        assert!(s.complete_physical_process_exit(DetPid::from_raw(300)));
        assert!(s.step2_process_blocked(&global_time).is_err());
        assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));

        post_rt_sigsuspend_continuation(&mut s, waiter, op_id);
        if reported {
            s.note_physical_thread_exit(waiter);
            assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
        }
        assert!(s.step2_process_blocked(&global_time).is_ok());
        assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
        assert!(!s.blocked.rt_sigsuspend_blockers.contains_key(&waiter));
        let queue: Vec<DetTid> = s.run_queue.tids().copied().collect();
        assert_eq!(queue, vec![waiter, sleeper, parent]);
        outcomes.push(queue);
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

/// Must not: once the supervisor has reported a thread dead, no wake is owed
/// to it, whether by a signal, across the grant of a request it posted before
/// dying, or by a waker that request found pending. A report for a thread the
/// scheduler no longer holds changes nothing, and a raw TID registered again
/// (logically retired, or reused by a new thread) may be owed a wake again.
#[test]
fn rt_sigsuspend_wake_is_never_owed_to_a_thread_reported_dead() {
    let sigchld = rt_sigsuspend_wakers(&[libc::SIGCHLD]);

    // Reported dead while in the real call: a later signal owes nothing.
    let mut s = physical_exit_reporting_scheduler();
    let (waiter, _, _) = add(&mut s, 100, 100);
    grant_rt_sigsuspend(&mut s, waiter, sigchld);
    s.note_physical_thread_exit(waiter);
    s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(s.step2_drain_prefix().is_ok());

    // Owed to a request still awaiting its grant, then reported dead: the
    // grant owes nothing.
    let mut s = physical_exit_reporting_scheduler();
    let (waiter, _, _) = add(&mut s, 100, 100);
    let rid = post_rt_sigsuspend_request(&mut s, waiter, sigchld);
    s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGCHLD));
    assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
    s.note_physical_thread_exit(waiter);
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    grant_posted_rt_sigsuspend(&mut s, waiter, &rid);
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(s.step2_drain_prefix().is_ok());

    // A request with a pending waker, granted after the report.
    let mut s = physical_exit_reporting_scheduler();
    let (waiter, _, _) = add(&mut s, 100, 100);
    let rid = post_rt_sigsuspend_request_with_pending(&mut s, waiter, sigchld, sigchld);
    s.note_physical_thread_exit(waiter);
    grant_posted_rt_sigsuspend(&mut s, waiter, &rid);
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(s.step2_drain_prefix().is_ok());

    // A report for an unregistered TID is ignored, and registering a thread
    // under a reported TID makes it eligible again.
    let mut s = physical_exit_reporting_scheduler();
    let stray = DetTid::from_raw(900);
    s.note_physical_thread_exit(stray);
    assert!(s.may_owe_rt_sigsuspend_wake(stray));
    let (waiter, waiter_mm, _) = add(&mut s, 100, 100);
    s.note_physical_thread_exit(waiter);
    assert!(!s.may_owe_rt_sigsuspend_wake(waiter));
    s.forget_physical_thread_exit(waiter);
    assert!(s.may_owe_rt_sigsuspend_wake(waiter));
    s.note_physical_thread_exit(waiter);
    s.logically_kill_thread(&waiter, &DetPid::from_raw(100), waiter_mm);
    assert!(s.may_owe_rt_sigsuspend_wake(waiter));
}

/// Must wake, at one point on every run: a sole-thread waiter whose
/// `rt_sigsuspend` found a waker already pending, and a sleeper whose deadline
/// is in the future. The real call takes the pending signal the moment it
/// installs its temporary mask, so its return is certain, and until its
/// continuation is posted no turn may run and virtual time must not advance
/// to the sleeper's deadline. Before the fix this call waited as external IO,
/// whose continuation arrives at a host-chosen pass, and the empty-queue step
/// fast-forwarded to the sleeper's deadline first on some runs and not on
/// others. Both continuation arrival orders end in the same state.
#[test]
fn rt_sigsuspend_with_a_pending_waker_holds_turns_and_timers_until_its_continuation() {
    let mut outcomes = Vec::new();
    for continuation_first in [false, true] {
        let mut s = physical_exit_reporting_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let time = global_time.lock().unwrap().as_nanos();
        let sigchld = rt_sigsuspend_wakers(&[libc::SIGCHLD]);
        let (waiter, _, _) = add(&mut s, 100, 100);
        let (sleeper, _, _) = add(&mut s, 300, 300);
        s.blocked
            .timed_waiters
            .insert(time + LogicalTime::from_nanos(1_000), sleeper);
        let rid = post_rt_sigsuspend_request_with_pending(&mut s, waiter, sigchld, sigchld);
        let op_id = grant_posted_rt_sigsuspend(&mut s, waiter, &rid);
        assert!(s.blocked.rt_sigsuspend_wake_owed.contains(&waiter));
        assert!(!s.blocked.external_io_blockers.contains_key(&waiter));

        let queue =
            settle_owed_rt_sigsuspend_wake(&mut s, &global_time, waiter, op_id, continuation_first);
        assert_eq!(queue, vec![waiter]);
        assert!(s.blocked.timed_waiters.thread_deadline(sleeper).is_some());
        assert_eq!(global_time.lock().unwrap().as_nanos(), time);
        assert_eq!(s.host_signal_attempts, 0);
        outcomes.push(queue);
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

/// Must not: a pending waker makes the return certain only for the only live
/// thread of its process, on a backend whose signals interrupt the real call.
/// With another live thread, which may take the signal first, the call waits
/// as external IO as before; with no pending waker nothing is owed at the
/// grant.
#[test]
fn rt_sigsuspend_with_a_pending_waker_owes_nothing_when_its_return_is_not_certain() {
    let sigchld = rt_sigsuspend_wakers(&[libc::SIGCHLD]);

    let mut s = physical_exit_reporting_scheduler();
    let (waiter, _, _) = add(&mut s, 100, 100);
    add(&mut s, 100, 101);
    let rid = post_rt_sigsuspend_request_with_pending(&mut s, waiter, sigchld, sigchld);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(waiter));
    assert!(
        s.block_for_one_resource(waiter, &rid, &Permission::RW, None, &Ivar::new())
            .is_err()
    );
    assert_eq!(
        s.blocked.external_io_blockers.get(&waiter),
        Some(&ExternalOpId::new(waiter, 7))
    );
    assert!(!s.blocked.rt_sigsuspend_blockers.contains_key(&waiter));
    assert!(!s.blocked.rt_sigsuspend_wakers.contains_key(&waiter));
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());

    let mut s = physical_exit_reporting_scheduler();
    let (waiter, _, _) = add(&mut s, 100, 100);
    grant_rt_sigsuspend(&mut s, waiter, sigchld);
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
    assert!(s.step2_drain_prefix().is_ok());

    let mut s = Scheduler::new(&Config::default());
    let (waiter, _, _) = add(&mut s, 100, 100);
    let rid = post_rt_sigsuspend_request_with_pending(&mut s, waiter, sigchld, sigchld);
    grant_posted_rt_sigsuspend(&mut s, waiter, &rid);
    assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty());
}

/// Must not owe a wake: a sibling may take a process-pending waker (for
/// example with a zero-timeout `rt_sigtimedwait`) and then exit between the
/// waiter's publication of its `rt_sigsuspend` and the grant. The waiter is
/// then the only live thread at the grant, but the signal it saw pending is
/// gone, so its real call sleeps; owing the wake would hold every turn and
/// timer forever. Only a waiter that was already alone when it published may
/// be owed the pending waker. Before the fix the grant tested membership only
/// at the grant and owed the wake in both cases.
#[test]
fn rt_sigsuspend_pending_waker_is_owed_only_when_sole_at_publication() {
    let sigchld = rt_sigsuspend_wakers(&[libc::SIGCHLD]);
    for sibling_exits_before_publication in [false, true] {
        let case = format!("sibling exits before publication: {sibling_exits_before_publication}");
        let mut s = physical_exit_reporting_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let time = global_time.lock().unwrap().as_nanos();
        let (waiter, waiter_mm, _) = add(&mut s, 100, 100);
        let (sibling, _, _) = add(&mut s, 100, 101);
        let (worker, _, _) = add(&mut s, 300, 300);
        let (sleeper, _, _) = add(&mut s, 400, 400);
        let waiter_process = DetPid::from_raw(100);
        if sibling_exits_before_publication {
            s.logically_kill_thread(&sibling, &waiter_process, waiter_mm);
        }
        let rid = post_rt_sigsuspend_request_with_pending(&mut s, waiter, sigchld, sigchld);
        let ResourceID::BlockingRtSigsuspend(op_id, ..) = rid else {
            unreachable!()
        };
        assert_eq!(
            s.blocked.rt_sigsuspend_sole_pending.get(&waiter),
            sibling_exits_before_publication.then_some(&op_id),
            "{case}"
        );
        if !sibling_exits_before_publication {
            // The sibling takes the pending SIGCHLD and exits.
            s.logically_kill_thread(&sibling, &waiter_process, waiter_mm);
        }
        queue_ordinary_work(&mut s, worker);
        let due = s.committed_time;
        s.blocked.timed_waiters.insert(due, sleeper);

        assert_eq!(s.run_queue.tentative_pop_next(), Some(waiter), "{case}");
        assert!(
            s.block_for_one_resource(waiter, &rid, &Permission::RW, None, &Ivar::new())
                .is_err(),
            "{case}"
        );
        assert!(s.blocked.rt_sigsuspend_sole_pending.is_empty(), "{case}");
        if sibling_exits_before_publication {
            assert_eq!(
                s.blocked.rt_sigsuspend_blockers.get(&waiter),
                Some(&op_id),
                "{case}"
            );
            assert!(
                s.blocked.rt_sigsuspend_wake_owed.contains(&waiter),
                "{case}"
            );
            let queue = settle_owed_rt_sigsuspend_wake(&mut s, &global_time, waiter, op_id, false);
            assert_eq!(queue, vec![waiter, sleeper, worker], "{case}");
        } else {
            assert_eq!(
                s.blocked.external_io_blockers.get(&waiter),
                Some(&op_id),
                "{case}"
            );
            assert!(
                !s.blocked.rt_sigsuspend_blockers.contains_key(&waiter),
                "{case}"
            );
            assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
            assert!(s.step2_process_blocked(&global_time).is_ok(), "{case}");
            assert!(s.take_terminal_deadlock().is_none(), "{case}");
            assert_eq!(
                s.run_queue.tids().copied().collect::<Vec<_>>(),
                vec![sleeper, worker],
                "{case}"
            );
        }
        assert_eq!(global_time.lock().unwrap().as_nanos(), time, "{case}");
    }
}

/// A scheduler with SaBRe's whole capability profile: the supervisor reports
/// physical exits, a signal interrupts the real blocking call, and a killed
/// thread is tombstoned with its posted request cancelled.
fn sabre_scheduler() -> Scheduler {
    Scheduler::new(&Config::default().with_backend(|backend| {
        backend.reports_physical_process_exits = true;
        backend.signal_interrupts_external_syscalls = true;
        backend.needs_killed_thread_rpc_cancellation = true;
    }))
}

/// When the supervisor's report of a killed process's death reaches the
/// scheduler, relative to the killer's `SIGKILL` edge. The kernel kills the
/// target inside the killer's `kill` call, and the killer notifies the
/// scheduler only after that call returns, so the report can come first.
#[derive(Clone, Copy, Debug)]
enum DeathReport {
    BeforeEdge,
    AfterEdge,
    AfterHeldPasses,
}

/// The supervisor reports the death of `leader`, the only thread of its
/// process: first the thread, then the process's final exit. Returns whether
/// that released the process's physical-exit barrier.
fn report_process_death(s: &mut Scheduler, leader: DetTid) -> bool {
    s.note_physical_thread_exit(leader);
    s.complete_physical_process_exit(DetPid::from_raw(leader.as_raw()))
}

/// Must not hang or diverge: a SaBRe child `Q` suspended for a caught SIGCHLD
/// is killed by its parent `P` with `SIGKILL`, and `Q`'s own child `A` exits
/// afterwards. In one variant `Q`'s real call was first interrupted by a signal
/// outside its waking set (SaBRe's central handler catches even a default or
/// ignored one), so `Q` had posted its continuation, which waits behind `P`'s
/// work. The supervisor's report of `Q`'s death is host-timed: before the
/// edge, right after it, or after several held passes. In every order the
/// edge retires `Q` exactly as an exit-group would: its posted request is
/// cancelled and never granted, no wake is owed to it, and its parent gets one
/// SIGCHLD wakeup once the reap is reported. All orders end in the same run
/// queue and the same first turn, which belongs to a survivor. Before the fix
/// nothing retired `Q`: `A`'s exit owed it a wake whenever `A` was reported
/// before `Q`'s death, and the posted continuation of the dead thread was then
/// granted ahead of `P`'s work on some runs and not on others.
#[test]
fn sigkill_edge_retires_a_suspended_child_identically_in_every_report_order() {
    for continuation_posted in [true, false] {
        let mut outcomes = Vec::new();
        for order in [
            DeathReport::BeforeEdge,
            DeathReport::AfterEdge,
            DeathReport::AfterHeldPasses,
        ] {
            let case = format!("continuation posted: {continuation_posted}, report {order:?}");
            let mut s = sabre_scheduler();
            let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
            let time = global_time.lock().unwrap().as_nanos();
            let (parent, waiter, op_id, grandchild, grandchild_mm, sleeper) =
                killed_waiter_fixture(&mut s);
            let waiter_process = DetPid::from_raw(200);
            if continuation_posted {
                s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGWINCH));
                assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
                post_rt_sigsuspend_continuation(&mut s, waiter, op_id);
            }
            let response = s.next_turns[&waiter].resp.clone();
            if matches!(order, DeathReport::BeforeEdge) {
                assert!(!report_process_death(&mut s, waiter), "{case}");
            }

            s.note_sigkill_sent(parent, waiter, Some(waiter_process));
            assert!(!s.next_turns.contains_key(&waiter), "{case}");
            assert!(s.thread_is_logically_killed(waiter), "{case}");
            if continuation_posted {
                assert!(
                    matches!(response.try_read(), Some(SchedResponse::Signaled(None))),
                    "{case}"
                );
            }
            assert!(
                !s.blocked.rt_sigsuspend_blockers.contains_key(&waiter),
                "{case}"
            );
            assert!(
                !s.blocked.rt_sigsuspend_wakers.contains_key(&waiter),
                "{case}"
            );
            assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
            assert!(
                s.logically_exited_processes.contains(&waiter_process),
                "{case}"
            );
            assert_eq!(
                s.child_exit_signals_owed.get(&waiter_process),
                Some(&DetPid::from_raw(100)),
                "{case}"
            );

            // A exits after the edge and is reaped at once. Its parent is
            // dead, so no wake can be owed to Q's suspend.
            grant_child_exit_and_remove_thread(&mut s, grandchild, grandchild_mm);
            assert!(
                s.complete_physical_process_exit(DetPid::from_raw(300)),
                "{case}"
            );
            assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
            match order {
                DeathReport::BeforeEdge => {}
                DeathReport::AfterEdge => {
                    assert!(report_process_death(&mut s, waiter), "{case}");
                }
                DeathReport::AfterHeldPasses => {
                    // Until the reap is reported, P's SIGCHLD may not have been
                    // sent yet, so no turn or timer may run.
                    assert!(s.step2_process_blocked(&global_time).is_err(), "{case}");
                    let queued: Vec<DetTid> = s.run_queue.tids().copied().collect();
                    assert!(!queued.contains(&waiter), "{case}");
                    for _ in 0..2 {
                        assert!(s.step2_process_blocked(&global_time).is_err(), "{case}");
                        assert!(s.take_terminal_deadlock().is_none(), "{case}");
                    }
                    assert_eq!(
                        s.run_queue.tids().copied().collect::<Vec<_>>(),
                        queued,
                        "{case}"
                    );
                    assert!(
                        s.blocked.timed_waiters.thread_deadline(sleeper).is_some(),
                        "{case}"
                    );
                    assert_eq!(global_time.lock().unwrap().as_nanos(), time, "{case}");
                    assert!(report_process_death(&mut s, waiter), "{case}");
                }
            }
            assert!(
                s.completed_physical_process_exits.contains(&waiter_process),
                "{case}"
            );
            assert!(s.step2_process_blocked(&global_time).is_ok(), "{case}");
            assert!(s.take_terminal_deadlock().is_none(), "{case}");
            assert!(s.child_exit_signals_owed.is_empty(), "{case}");
            assert_eq!(global_time.lock().unwrap().as_nanos(), time, "{case}");
            let queue: Vec<DetTid> = s.run_queue.tids().copied().collect();
            assert!(
                queue.contains(&parent) && queue.contains(&sleeper) && !queue.contains(&waiter),
                "{case}: {queue:?}"
            );
            let (first, _, _) = s.select_test_turn().expect("a survivor is runnable");
            assert_ne!(first, waiter, "{case}");
            outcomes.push((queue, first, s.owed_child_exit_wakeups));
        }
        assert!(
            outcomes.iter().all(|outcome| *outcome == outcomes[0]),
            "continuation posted: {continuation_posted}: {outcomes:?}"
        );
    }
}

/// The `SIGKILL` edge retires only what it can prove dead, and only once. A
/// kill sent to the sender's own process kills the sender too and is left to
/// the existing paths; a backend without SaBRe's whole profile keeps its own
/// handling; a second kill of a retired process finds nothing to do; and a
/// live thread without a recorded address space fails the run closed rather
/// than being retired with a guessed one.
#[test]
fn sigkill_edge_retires_only_another_live_process_once() {
    let child_process = DetPid::from_raw(200);

    let mut s = sabre_scheduler();
    add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    let (child_sibling, _, _) = add(&mut s, 200, 201);
    s.note_sigkill_sent(child_sibling, child, Some(child_process));
    assert!(s.next_turns.contains_key(&child) && s.next_turns.contains_key(&child_sibling));
    assert!(!s.thread_is_logically_killed(child));
    assert!(s.child_exit_signals_owed.is_empty());

    for mut s in [
        physical_exit_reporting_scheduler(),
        Scheduler::new(&Config::default().with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        })),
    ] {
        let (parent, _, _) = add(&mut s, 100, 100);
        let (child, _, _) = add_process_child(&mut s, 100, 200);
        s.note_sigkill_sent(parent, child, Some(child_process));
        assert!(s.next_turns.contains_key(&child));
        assert!(!s.logically_exited_processes.contains(&child_process));
        assert!(s.child_exit_signals_owed.is_empty());
    }

    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    queue_ordinary_work(&mut s, parent);
    s.note_sigkill_sent(parent, child, Some(child_process));
    assert!(s.thread_is_logically_killed(child));
    assert!(s.pending_physical_process_exits.contains(&child_process));
    assert!(s.complete_physical_process_exit(child_process));
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1);
    s.note_sigkill_sent(parent, child, Some(child_process));
    assert!(s.child_exit_signals_owed.is_empty());
    assert!(!s.pending_physical_process_exits.contains(&child_process));
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1);
    assert!(s.take_terminal_deadlock().is_none());

    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    s.registered_mms.remove(&child);
    s.note_sigkill_sent(parent, child, Some(child_process));
    assert!(
        s.take_terminal_deadlock()
            .is_some_and(|message| message.contains("address-space identity"))
    );
    assert!(s.next_turns.contains_key(&child));
    assert!(!s.thread_is_logically_killed(child));
    assert!(s.child_exit_signals_owed.is_empty());
}

/// Must not leave a thread behind: a `SIGKILL` sent with `tgkill` to one
/// worker thread kills its whole process. The edge resolves the process from
/// the thread and retires every live thread of it, cancelling the worker's
/// posted request so that it is never granted. Until the supervisor reports
/// the reap no turn runs; then the parent gets its one wakeup.
#[test]
fn sigkill_edge_retires_every_thread_of_the_target_process() {
    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (leader, _, _) = add_process_child(&mut s, 100, 200);
    let (worker, _, _) = add(&mut s, 200, 201);
    let child_process = DetPid::from_raw(200);
    queue_ordinary_work(&mut s, parent);
    queue_ordinary_work(&mut s, worker);
    let worker_response = s.next_turns[&worker].resp.clone();

    s.note_sigkill_sent(parent, worker, None);
    for tid in [leader, worker] {
        assert!(!s.next_turns.contains_key(&tid));
        assert!(s.thread_is_logically_killed(tid));
    }
    assert!(matches!(
        worker_response.try_read(),
        Some(SchedResponse::Signaled(None))
    ));
    assert!(s.pending_physical_process_exits.contains(&child_process));
    assert_eq!(
        s.child_exit_signals_owed.get(&child_process),
        Some(&DetPid::from_raw(100))
    );
    assert!(s.step2_drain_prefix().is_err());
    assert!(!s.run_queue.tids().any(|tid| *tid == worker));
    assert!(s.complete_physical_process_exit(child_process));
    assert!(s.step2_drain_prefix().is_ok());
    assert!(s.take_terminal_deadlock().is_none());
    assert_eq!(s.owed_child_exit_wakeups, 1);
    assert_eq!(
        s.run_queue.tids().copied().collect::<Vec<_>>(),
        vec![parent]
    );
}

/// The `SIGKILL` edge completes only once the parent's one real `SIGCHLD` is
/// queued. The kernel queues it when the supervisor reaps the child, before
/// the supervisor reports the reap, so an edge that owes the parent that
/// signal while the report is outstanding returns a receiver that resolves at
/// exactly that report: not at an unrelated report, and once. Teardown, which
/// releases every physical-exit barrier, releases a waiting edge too, so no
/// sender is left waiting for a report that will never come.
#[test]
fn sigkill_edge_waits_for_the_reap_report_that_queues_the_parents_sigchld() {
    let child_process = DetPid::from_raw(200);

    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    queue_ordinary_work(&mut s, parent);
    let mut reported = s
        .note_sigkill_sent(parent, child, Some(child_process))
        .expect("an owed SIGCHLD makes the edge wait for the reap report");
    assert!(matches!(reported.try_recv(), Ok(None)));
    assert!(s.step2_drain_prefix().is_err());
    assert!(!s.complete_physical_process_exit(DetPid::from_raw(999)));
    assert!(matches!(reported.try_recv(), Ok(None)));
    assert!(s.complete_physical_process_exit(child_process));
    assert!(matches!(reported.try_recv(), Ok(Some(()))));
    assert!(s.sigkill_exit_waiters.is_empty());
    assert!(!s.complete_physical_process_exit(child_process));
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 1);

    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    let mut reported = s
        .note_sigkill_sent(parent, child, Some(child_process))
        .expect("an owed SIGCHLD makes the edge wait for the reap report");
    assert_eq!(s.release_all_physical_process_exits(), 1);
    assert!(matches!(reported.try_recv(), Ok(Some(()))));
    assert!(s.sigkill_exit_waiters.is_empty());
}

/// The edge waits only when it owes a `SIGCHLD` whose reap is unreported. A
/// report that reached Detcore before the edge has already queued the signal,
/// so the edge completes the exit and returns at once; a kill of the sender's
/// own process and a backend without SaBRe's whole profile retire nothing and
/// never wait.
#[test]
fn sigkill_edge_does_not_wait_without_an_unreported_owed_sigchld() {
    let child_process = DetPid::from_raw(200);

    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    assert!(!s.complete_physical_process_exit(child_process));
    assert!(
        s.note_sigkill_sent(parent, child, Some(child_process))
            .is_none()
    );
    assert_eq!(
        s.test_process_exit_state(child_process),
        (false, true, Some(DetPid::from_raw(100)))
    );
    assert!(s.sigkill_exit_waiters.is_empty());

    let mut s = sabre_scheduler();
    add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    let (child_sibling, _, _) = add(&mut s, 200, 201);
    assert!(
        s.note_sigkill_sent(child_sibling, child, Some(child_process))
            .is_none()
    );
    assert!(s.sigkill_exit_waiters.is_empty());

    for mut s in [
        physical_exit_reporting_scheduler(),
        Scheduler::new(&Config::default().with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        })),
    ] {
        let (parent, _, _) = add(&mut s, 100, 100);
        let (child, _, _) = add_process_child(&mut s, 100, 200);
        assert!(
            s.note_sigkill_sent(parent, child, Some(child_process))
                .is_none()
        );
        assert!(s.sigkill_exit_waiters.is_empty());
    }
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
        let (mut s, backend) = fixture_with_control(controlled);
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

// ChildWait uses the actual scheduler request/observation transitions below.
// These state tests do not stand in for backend signal frames or guest copyout.
fn child_wait(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    site: reverie::CallbackSignalSite,
    spec: ChildWaitSpec,
    blocked: bool,
) -> Ivar<SchedResponse> {
    let parent = s.registered_process(tid).unwrap();
    let mut resources = Resources::new(tid);
    resources.insert(ResourceID::WaitChild { parent, spec }, Permission::R);
    resources.fyi("wait-child-lifecycle");
    s.install_resource_origin(
        tid,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm,
            control: ControlCapability::ChildWait { site },
        },
    )
    .unwrap();
    let turn = &s.next_turns[&tid];
    turn.req.put(Ok(resources));
    let response = turn.resp.clone();
    if blocked {
        assert!(
            s.blocked
                .child_waiters
                .insert(tid, (parent, spec))
                .is_none()
        );
    } else {
        s.run_queue.push_back(tid, DEFAULT_PRIORITY);
    }
    response
}

fn child_spec() -> ChildWaitSpec {
    ChildWaitSpec {
        selector: ChildWaitSelector::ProcessGroup(DetPid::from_raw(100)),
        owner: Some(DetTid::from_raw(100)),
        exit_class: ChildWaitExitClass::Sigchld,
    }
}

fn resume_child_observation(
    s: &mut Scheduler,
    tid: DetTid,
    mm: MmId,
    c: AlarmControl,
) -> Ivar<SchedResponse> {
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site: c.site,
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
    let response = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Resume {
            ticket,
            site: c.site,
            response: response.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert!(!s.backend_failed());
    response
}

#[test]
fn blocked_child_wait_observation_restores_exact_membership() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let spec = child_spec();
    let response = child_wait(&mut s, tid, mm, site, spec, true);
    let request = s.next_turns[&tid].req.clone();
    let resources = request.try_read().unwrap().unwrap();
    let before_turn = s.turn;
    let before_time = s.committed_time;
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    assert!(!s.blocked.child_waiters.contains_key(&tid));
    assert!(s.run_queue.contains_tid(tid));
    assert!(s.next_turns[&tid].req.try_read().is_none());
    let resumed = resume_child_observation(&mut s, tid, mm, c);
    assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
    assert!(!s.run_queue.contains_tid(tid));
    assert_eq!(s.next_turns[&tid].req, request);
    assert_eq!(request.try_read().unwrap().unwrap(), resources);
    assert_eq!(s.next_turns[&tid].resp, resumed);
    assert!(resumed.try_read().is_none());
    assert_eq!(s.turn, before_turn);
    assert_eq!(s.committed_time, before_time);
    assert!(b.permits.lock().unwrap().is_empty());
    // Repeated ignored/suppressed observations retain the same logical wait.
    s.select_parked_alarm().unwrap();
    let second = selected(&resumed);
    assert_eq!(second.continuation, c.continuation);
    assert_ne!(second.lease, c.lease);
    assert_eq!(selected(&response), c);
}

#[test]
fn queued_child_wait_observation_restores_original_queue_order() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let (peer, _, _) = add(&mut s, 300, 300);
    let spec = child_spec();
    let response = child_wait(&mut s, tid, mm, site, spec, false);
    let request = s.next_turns[&tid].req.clone();
    s.run_queue.push_back(peer, DEFAULT_PRIORITY);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    let resumed = resume_child_observation(&mut s, tid, mm, c);
    assert_eq!(s.next_turns[&tid].req, request);
    assert!(resumed.try_read().is_none());
    assert!(!s.blocked.child_waiters.contains_key(&tid));
    assert_eq!(s.run_queue.tentative_pop_next(), Some(tid));
    assert_eq!(s.run_queue.commit_tentative_pop(), tid);
    assert_eq!(s.run_queue.tentative_pop_next(), Some(peer));
    s.run_queue.undo_tentative_pop();
}

#[test]
fn child_wait_ready_and_no_child_precede_alarm_selection() {
    for blocked in [false, true] {
        for change in 0..4 {
            let (mut s, b) = fixture();
            let (tid, mm, site) = add(&mut s, 100, 100);
            let (child, _, _) = add_process_child(&mut s, 100, 200);
            let spec = child_spec();
            let response = child_wait(&mut s, tid, mm, site, spec, blocked);
            let request = s.next_turns[&tid].req.clone();
            match change {
                0 => {
                    s.logically_exited_processes.insert(child);
                }
                1 => {
                    s.logically_exited_processes.insert(child);
                    assert!(s.consume_child_wait(tid, child));
                }
                2 => {
                    assert!(
                        s.thread_tree
                            .set_process_group(child, DetPid::from_raw(999))
                    );
                }
                3 => {
                    s.thread_tree
                        .process_wait
                        .get_mut(&child)
                        .unwrap()
                        .wait_owner = DetTid::from_raw(101);
                }
                _ => unreachable!(),
            }
            // Consumption wakes a blocked waiter through a deferred admission.
            // Run the same prefix as the daemon before alarm selection.
            let pending_wake = blocked && change == 1;
            assert_eq!(
                s.pending_run_queue_admissions.get(&tid).copied(),
                pending_wake.then_some(AdmitIntent::Fixed(AdmitSide::Back))
            );
            assert_eq!(s.run_queue.contains_tid(tid), !blocked);
            assert_eq!(
                s.blocked.child_waiters.contains_key(&tid),
                blocked && !pending_wake
            );
            let before_turn = s.turn;
            let before_time = s.committed_time;
            s.step2_drain_prefix().unwrap();
            assert!(s.pending_run_queue_admissions.is_empty());
            assert_eq!(s.run_queue.contains_tid(tid), !blocked || pending_wake);
            assert_eq!(
                s.blocked.child_waiters.contains_key(&tid),
                blocked && !pending_wake
            );
            assert_eq!(s.turn, before_turn);
            assert_eq!(s.committed_time, before_time);
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, 100),
            });
            s.select_parked_alarm().unwrap();
            assert!(
                response.try_read().is_none(),
                "blocked {blocked}, change {change}"
            );
            assert!(b.permits.lock().unwrap().is_empty());
            assert_eq!(s.next_turns[&tid].req, request);
            assert!(s.run_queue.contains_tid(tid));
            assert!(!s.blocked.child_waiters.contains_key(&tid));
            assert_eq!(
                s.ready_child_wait(tid, spec),
                (change == 0).then_some(child)
            );
            assert_eq!(s.has_child_wait_target(tid, spec), change == 0);
            let original = request.try_read().unwrap().unwrap();
            assert_eq!(s.step3_peek().unwrap().0, tid);
            assert!(s.step4_resource_block(tid, &original, &response).is_ok());
            s.run_queue.undo_tentative_pop();
        }
    }
}

#[test]
fn child_wait_resume_rechecks_ready_consumed_group_and_owner() {
    for change in 0..5 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        let (child, _, _) = add_process_child(&mut s, 100, 200);
        let spec = child_spec();
        let response = child_wait(&mut s, tid, mm, site, spec, true);
        let request = s.next_turns[&tid].req.clone();
        let resources = request.try_read().unwrap().unwrap();
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        s.select_parked_alarm().unwrap();
        let c = selected(&response);
        // Model changes while the observation owns an empty execution gate;
        // wake_child_waiters cannot find the removed blocked membership.
        match change {
            0 => {}
            1 => {
                s.logically_exited_processes.insert(child);
                s.wake_child_waiters(tid, child);
            }
            2 => {
                s.logically_exited_processes.insert(child);
                assert!(s.consume_child_wait(tid, child));
            }
            3 => {
                assert!(
                    s.thread_tree
                        .set_process_group(child, DetPid::from_raw(999))
                );
            }
            4 => {
                s.thread_tree
                    .process_wait
                    .get_mut(&child)
                    .unwrap()
                    .wait_owner = DetTid::from_raw(101);
            }
            _ => unreachable!(),
        }
        let resumed = resume_child_observation(&mut s, tid, mm, c);
        assert_eq!(
            s.blocked.child_waiters.get(&tid).copied(),
            (change == 0).then_some((tid, spec))
        );
        assert_eq!(s.run_queue.contains_tid(tid), change != 0);
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(request.try_read().unwrap().unwrap(), resources);
        assert!(resumed.try_read().is_none());
        if change != 0 {
            s.select_parked_alarm().unwrap();
            assert!(resumed.try_read().is_none());
            assert!(b.permits.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn child_wait_rejects_malformed_capability_or_membership_before_reservation() {
    for fault in 0..9 {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        add_process_child(&mut s, 100, 200);
        let spec = child_spec();
        let response = child_wait(&mut s, tid, mm, site, spec, true);
        let mut resources = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        match fault {
            0 => {
                resources.tid = DetTid::from_raw(101);
            }
            1 => {
                resources.poll_attempt = 1;
            }
            2 => {
                resources.insert(ResourceID::InternalIOPolling, Permission::W);
            }
            3 => {
                resources.resources.clear();
                resources.insert(ResourceID::WaitChild { parent: tid, spec }, Permission::W);
            }
            4 => {
                s.run_queue.push_back(tid, DEFAULT_PRIORITY);
            }
            5 => {
                s.blocked.child_waiters.get_mut(&tid).unwrap().1.owner = None;
            }
            6..=8 => {
                let mut wrong = spec;
                let parent = if fault == 6 {
                    DetPid::from_raw(999)
                } else {
                    tid
                };
                if fault == 7 {
                    wrong.owner = Some(DetTid::from_raw(101));
                }
                if fault == 8 {
                    wrong.exit_class = ChildWaitExitClass::Clone;
                }
                resources.resources.clear();
                resources.insert(
                    ResourceID::WaitChild {
                        parent,
                        spec: wrong,
                    },
                    Permission::R,
                );
            }
            _ => unreachable!(),
        }
        let request = Ivar::new();
        request.put(Ok(resources));
        s.next_turns.get_mut(&tid).unwrap().req = request.clone();
        let queue = format!("{:?}", s.run_queue);
        let membership = s.blocked.child_waiters.clone();
        b.recipients.lock().unwrap().push(SignalRecipient {
            task: task(100, 100),
        });
        assert_eq!(
            s.select_parked_alarm(),
            Err(SelectionFailure {
                pid: tid,
                tid: Some(tid),
                failure: if fault < 4 {
                    ProtocolFailure::Unsupported
                } else if fault < 6 {
                    ProtocolFailure::Phase
                } else {
                    ProtocolFailure::Identity
                },
            })
        );
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(s.blocked.child_waiters, membership);
        assert_eq!(format!("{:?}", s.run_queue), queue);
        assert!(response.try_read().is_none());
        assert!(b.permits.lock().unwrap().is_empty());
    }
}

#[test]
fn child_wait_reservation_failure_preserves_blocked_request() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let spec = child_spec();
    let response = child_wait(&mut s, tid, mm, site, spec, true);
    let request = s.next_turns[&tid].req.clone();
    b.fail_reservation
        .store(true, std::sync::atomic::Ordering::Relaxed);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    assert_eq!(
        s.select_parked_alarm(),
        Err(SelectionFailure {
            pid: tid,
            tid: Some(tid),
            failure: ProtocolFailure::Identity,
        })
    );
    assert_eq!(s.next_turns[&tid].req, request);
    assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
    assert!(!s.run_queue.contains_tid(tid));
    assert!(response.try_read().is_none());
    assert!(b.permits.lock().unwrap().is_empty());
}

#[test]
fn child_wait_caught_selection_wins_later_exit_and_keeps_return_gate() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let (child, _, _) = add_process_child(&mut s, 100, 200);
    let response = child_wait(&mut s, tid, mm, site, child_spec(), true);
    b.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.select_parked_alarm().unwrap();
    let c = selected(&response);
    // A nested caught-signal Tool hook publishes readiness after selection.
    // It must not turn the already-selected interruption into a child reap.
    s.logically_exited_processes.insert(child);
    s.wake_child_waiters(tid, child);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: c.continuation,
            lease: c.lease,
            site,
            finish: ObservationFinish::InterruptForCaught {
                selection: reverie::PreparedSignalToken {
                    site,
                    selection_nonce: 1,
                },
            },
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    assert_eq!(ack.try_read(), Some(Ok(FinishAck::Interrupted)));
    assert_eq!(s.ready_child_wait(tid, child_spec()), Some(child));
    assert_eq!(
        s.next_turns[&tid].protocol.owner,
        NextTurnOwner::ReturningCaught {
            completed_wait: c.continuation
        }
    );
    assert!(s.next_turns[&tid].req.try_read().is_none());
    assert!(s.next_turns[&tid].resp.try_read().is_none());
    assert!(s.run_queue.contains_tid(tid));
    assert!(!s.blocked.child_waiters.contains_key(&tid));
    assert_eq!(s.parked.permits.get(&tid), Some(&c.permit));
    s.select_parked_alarm().unwrap();
    assert_eq!(b.permits.lock().unwrap().as_slice(), &[c.permit]);
    let receipt = SignalBoundaryReceipt {
        permit: c.permit,
        outcome: SignalBoundaryOutcome::Caught,
    };
    s.consume_signal_boundary(receipt).unwrap();
    s.consume_signal_boundary(receipt).unwrap();
    assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
    assert!(!s.parked.permits.contains_key(&tid));
    assert_eq!(s.ready_child_wait(tid, child_spec()), Some(child));
}

#[test]
fn child_wait_cancellation_and_terminal_receipt_cannot_restore_membership() {
    for observed in [false, true] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        add_process_child(&mut s, 100, 200);
        let response = child_wait(&mut s, tid, mm, site, child_spec(), true);
        if observed {
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
                    finish: ObservationFinish::Terminate {
                        selection: reverie::PreparedSignalToken {
                            site,
                            selection_nonce: 2,
                        },
                    },
                    ack: ack.clone(),
                },
            )
            .unwrap();
            s.drain_control_intents();
            assert_eq!(ack.try_read(), Some(Ok(FinishAck::Terminate)));
            assert_eq!(s.parked.permits.get(&tid), Some(&c.permit));
            let receipt = SignalBoundaryReceipt {
                permit: c.permit,
                outcome: SignalBoundaryOutcome::Terminated {
                    group: false,
                    wait_status: 14,
                },
            };
            s.consume_signal_boundary(receipt).unwrap();
            s.consume_signal_boundary(receipt).unwrap();
        } else {
            s.logically_kill_thread(&tid, &tid, mm);
            assert!(matches!(
                response.try_read(),
                Some(SchedResponse::Signaled(None))
            ));
        }
        assert!(!s.next_turns.contains_key(&tid));
        assert!(!s.blocked.child_waiters.contains_key(&tid));
        // Logical retirement is immediate; physical queue removal is deferred.
        assert_eq!(
            s.pending_run_queue_removals.get(&tid),
            Some(&RemovalDisposition::Retire)
        );
        assert_eq!(s.run_queue.contains_tid(tid), observed);
        assert!(!s.parked.permits.contains_key(&tid));
        assert!(s.parked.requests.keys().all(|wait| wait.dettid != tid));
        assert!(s.thread_is_logically_killed(tid));
        let before_turn = s.turn;
        let before_time = s.committed_time;
        s.step2_drain_prefix().unwrap();
        assert!(s.pending_run_queue_removals.is_empty());
        assert!(s.pending_run_queue_admissions.is_empty());
        assert_eq!(s.turn, before_turn);
        assert_eq!(s.committed_time, before_time);
        assert!(!s.next_turns.contains_key(&tid));
        assert!(!s.blocked.child_waiters.contains_key(&tid));
        assert!(!s.run_queue.contains_tid(tid));
        assert!(!s.parked.permits.contains_key(&tid));
        assert!(s.parked.requests.keys().all(|wait| wait.dettid != tid));
        assert!(s.thread_is_logically_killed(tid));
    }
}

#[test]
fn child_wait_cross_signal_drain_retires_capability_before_due_alarm() {
    // Blocked, queued, resumed after ignored observation, and pending child
    // admission are all real positions at the prefix drain (before admissions).
    for position in 0..4 {
        for alarm_masked in [false, true] {
            let (mut s, b) = fixture();
            let (tid, mm, site) = add(&mut s, 100, 100);
            let (child, _, _) = add_process_child(&mut s, 100, 200);
            let spec = child_spec();
            let mut response = child_wait(&mut s, tid, mm, site, spec, position != 1);
            if position == 2 {
                b.recipients.lock().unwrap().push(SignalRecipient {
                    task: task(100, 100),
                });
                s.select_parked_alarm().unwrap();
                response = resume_child_observation(&mut s, tid, mm, selected(&response));
                assert_eq!(s.parked.requests.len(), 1);
                b.recipients.lock().unwrap().clear();
            }
            if position == 3 {
                s.logically_exited_processes.insert(child);
                s.wake_child_waiters(tid, child);
                assert!(s.pending_run_queue_admissions.contains_key(&tid));
            }
            let original = s.next_turns[&tid].req.clone();
            s.replace_real_timer(tid, tid, at(0), at(10), at(0), Signal::SIGALRM)
                .unwrap();
            for signal in [
                Signal::SIGUSR2,
                Signal::SIGURG,
                Signal::SIGALRM,
                Signal::SIGUSR1,
                Signal::SIGUSR2,
            ] {
                s.notify_signal_pending(tid, SigWrapper::from(signal));
            }
            let before_turn = s.turn;
            s.step2_drain_prefix().unwrap();
            assert!(!s.backend_failed());
            assert!(s.pending_cross_task_signals.is_empty());
            assert!(s.pending_run_queue_admissions.is_empty());
            assert!(!s.blocked.child_waiters.contains_key(&tid));
            assert!(s.run_queue.contains_tid(tid));
            assert_ne!(s.next_turns[&tid].req, original);
            assert_eq!(
                s.next_turns[&tid].protocol.origin.unwrap().control,
                ControlCapability::None
            );
            assert!(s.parked.requests.is_empty());
            assert!(response.try_read().is_none());
            assert_eq!(s.turn, before_turn);
            // A second notification merges with the ordinary legacy batch.
            s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGWINCH));
            s.step2_drain_prefix().unwrap();
            let signals = vec![
                Signal::SIGUSR1,
                Signal::SIGUSR2,
                Signal::SIGALRM,
                Signal::SIGURG,
                Signal::SIGWINCH,
            ]
            .into_iter()
            .map(SigWrapper::from)
            .collect::<Vec<_>>();
            let resources = s.next_turns[&tid].req.try_read().unwrap().unwrap();
            assert_eq!(resources.resources.len(), 1);
            assert_eq!(
                resources
                    .resources
                    .get(&ResourceID::WaitidSignals(signals.clone())),
                Some(&Permission::W)
            );
            // The shared alarm is published after the signal prefix and before
            // selection, exactly where the stale ChildWait origin used to fail.
            s.committed_time = at(10);
            assert!(s.step2b_process_timed());
            assert_eq!(b.publications.lock().unwrap().len(), 1);
            if !alarm_masked {
                b.recipients.lock().unwrap().push(SignalRecipient {
                    task: task(100, 100),
                });
            }
            s.select_parked_alarm().unwrap();
            assert!(b.permits.lock().unwrap().is_empty());
            assert!(response.try_read().is_none());
            let (selected, _, selected_response) = s.step3_peek().unwrap();
            assert_eq!(selected, tid);
            assert!(
                s.step4_resource_block(tid, &resources, &selected_response)
                    .is_ok()
            );
            assert!(
                s.step5_guest_unblock(tid, &resources, &selected_response)
                    .is_ok()
            );
            assert!(
                matches!(response.try_read(), Some(SchedResponse::Signaled(Some(actual))) if actual == signals)
            );
            // This is the ordinary grant/return-boundary route, never an
            // unowned observation or an execution gate silently discarded.
            let permit = s
                .authorize_signal_boundary(task(100, 100))
                .unwrap()
                .unwrap();
            assert_eq!(permit.site, None);
            assert_eq!(s.parked.permits.get(&tid), Some(&permit));
            s.run_queue.undo_tentative_pop();
        }
    }
}

#[test]
fn child_wait_repark_after_legacy_signal_keeps_shared_alarm_eligible() {
    for alarm_masked in [false, true] {
        let (mut s, b) = fixture();
        let (tid, mm, site) = add(&mut s, 100, 100);
        add_process_child(&mut s, 100, 200);
        let spec = child_spec();
        let response = child_wait(&mut s, tid, mm, site, spec, true);
        s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGURG));
        s.step2_drain_prefix().unwrap();
        let resources = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        let (_, _, selected_response) = s.step3_peek().unwrap();
        assert!(
            s.step4_resource_block(tid, &resources, &selected_response)
                .is_ok()
        );
        assert!(
            s.step5_guest_unblock(tid, &resources, &selected_response)
                .is_ok()
        );
        assert!(
            matches!(response.try_read(), Some(SchedResponse::Signaled(Some(signals))) if signals == vec![SigWrapper::from(Signal::SIGURG)])
        );
        s.step6_reenquue(tid, false);
        // The actual helper rechecks an ignored/blocked legacy signal and asks
        // for the same ChildWait again. Drive its normal request/park here.
        s.install_resource_origin(
            tid,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm,
                control: ControlCapability::ChildWait { site },
            },
        )
        .unwrap();
        let mut resources = Resources::new(tid);
        resources.insert(ResourceID::WaitChild { parent: tid, spec }, Permission::R);
        resources.fyi("wait-child-lifecycle");
        s.next_turns[&tid].req.put(Ok(resources.clone()));
        let new_response = s.next_turns[&tid].resp.clone();
        assert_eq!(s.step3_peek().unwrap().0, tid);
        assert!(
            s.step4_resource_block(tid, &resources, &new_response)
                .is_err()
        );
        assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
        if !alarm_masked {
            b.recipients.lock().unwrap().push(SignalRecipient {
                task: task(100, 100),
            });
        }
        s.select_parked_alarm().unwrap();
        if alarm_masked {
            assert!(new_response.try_read().is_none());
            assert!(b.permits.lock().unwrap().is_empty());
            assert_eq!(s.blocked.child_waiters.get(&tid), Some(&(tid, spec)));
        } else {
            assert_eq!(selected(&new_response).site, site);
            assert!(!s.blocked.child_waiters.contains_key(&tid));
            assert!(s.run_queue.contains_tid(tid));
        }
    }
}

#[test]
fn child_wait_cross_signal_rejects_conflicting_ownership_before_rewrite() {
    let (mut s, b) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    add_process_child(&mut s, 100, 200);
    let response = child_wait(&mut s, tid, mm, site, child_spec(), true);
    let request = s.next_turns[&tid].req.clone();
    // A pending admission cannot also own a still-blocked waiter. Keep the
    // malformed-state guard rather than accepting any origin mismatch.
    s.admit_to_run_queue(tid, AdmitIntent::Fixed(AdmitSide::Back));
    s.notify_signal_pending(tid, SigWrapper::from(Signal::SIGUSR1));
    s.drain_pending_cross_task_signals();
    assert!(s.backend_failed());
    assert_eq!(s.next_turns[&tid].req, request);
    assert_eq!(
        s.next_turns[&tid].protocol.origin.unwrap().control,
        ControlCapability::ChildWait { site }
    );
    assert!(response.try_read().is_none());
    assert!(b.permits.lock().unwrap().is_empty());
}

/// A scheduler for `backend` with sequentialized threads set as given.
fn scheduler_for(backend: BackendCapabilities, sequentialize_threads: bool) -> Scheduler {
    Scheduler::new(&Config {
        sequentialize_threads,
        backend,
        ..Config::default()
    })
}

/// The decision is the backend's offer and Detcore's own sequentialization:
/// an offer in a sequentialized run is installed, and no offer, or a run that
/// is not sequentialized, leaves the run unchanged. An offer that contradicts
/// the backend's reported capabilities is refused rather than resolved either
/// way, because Reverie bounds dequeue observation by those capabilities.
/// Refusing a missing control is reverie-kvm's job, not this one.
#[test]
fn signal_control_is_installed_when_offered_in_a_sequentialized_run() {
    for (name, backend) in [
        ("PTRACE", BackendCapabilities::PTRACE),
        ("E9PATCH", BackendCapabilities::E9PATCH),
        ("LITEINST_IN_GUEST", BackendCapabilities::LITEINST_IN_GUEST),
        ("SABRE", BackendCapabilities::SABRE),
        ("DBT", BackendCapabilities::DBT),
        ("KVM", BackendCapabilities::KVM),
    ] {
        // Only KVM reports the capability, so it is the only backend whose
        // offer can be coherent.
        assert_eq!(
            backend.provides_process_signal_control,
            name == "KVM",
            "{name}"
        );
        for sequentialize in [false, true] {
            for offered in [false, true] {
                let mut s = scheduler_for(backend, sequentialize);
                let control = offered.then(|| BackendSignalControl {
                    process: Arc::new(Backend::default()),
                });
                let result = s.install_signal_control(control, sequentialize);
                let case = format!("{name} sequentialize={sequentialize} offered={offered}");
                let installs = offered && sequentialize;
                match (installs, backend.provides_process_signal_control) {
                    (true, true) => {
                        assert_eq!(
                            result.unwrap(),
                            BackendSignalControlMode::ToolControlled,
                            "{case}"
                        );
                    }
                    (true, false) => {
                        assert!(
                            matches!(
                                result,
                                Err(reverie::Error::Errno(reverie::syscalls::Errno::EINVAL))
                            ),
                            "{case}: {result:?}"
                        );
                    }
                    (false, _) => {
                        assert_eq!(
                            result.unwrap(),
                            BackendSignalControlMode::Unchanged,
                            "{case}"
                        );
                    }
                }
                assert_eq!(
                    s.signal_control_installed(),
                    installs && backend.provides_process_signal_control,
                    "{case}"
                );
            }
        }
    }
}

/// The installation is decided once, before the first turn: a second offer,
/// or an offer after a guest thread registered or a turn ran, is refused and
/// leaves the earlier answer in place.
#[test]
fn signal_control_cannot_be_installed_after_the_run_started() {
    let offer = || {
        Some(BackendSignalControl {
            process: Arc::new(Backend::default()),
        })
    };
    let einval = |result: Result<BackendSignalControlMode, reverie::Error>| {
        matches!(
            result,
            Err(reverie::Error::Errno(reverie::syscalls::Errno::EINVAL))
        )
    };

    let (mut s, _) = fixture();
    assert!(einval(s.install_signal_control(offer(), true)));
    assert!(s.signal_control_installed());

    type StartTheRun = fn(&mut Scheduler);
    let late: [(&str, StartTheRun); 3] = [
        ("a pending turn", |s| {
            add(s, 100, 100);
        }),
        ("a completed turn", |s| s.turn = 1),
        ("a registered guest thread", |s| {
            s.started_up.try_put(());
        }),
    ];
    for (name, start) in late {
        let mut s = scheduler_for(BackendCapabilities::KVM, true);
        start(&mut s);
        assert!(einval(s.install_signal_control(offer(), true)), "{name}");
        assert!(!s.signal_control_installed(), "{name}");
    }
}

/// A run on a backend without process signal control never enters the
/// controlled turn loop, even when it is sequentialized and a control is
/// offered: the offer is refused and the turn takes the ordinary path. KVM,
/// the one backend with the capability, is the positive control.
#[test]
fn backends_without_signal_control_never_enter_the_controlled_loop() {
    use futures::FutureExt;

    for (name, backend, controlled) in [
        ("PTRACE", BackendCapabilities::PTRACE, false),
        ("E9PATCH", BackendCapabilities::E9PATCH, false),
        (
            "LITEINST_IN_GUEST",
            BackendCapabilities::LITEINST_IN_GUEST,
            false,
        ),
        ("SABRE", BackendCapabilities::SABRE, false),
        ("DBT", BackendCapabilities::DBT, false),
        ("KVM", BackendCapabilities::KVM, true),
    ] {
        let mut s = scheduler_for(backend, true);
        let result = s.install_signal_control(
            Some(BackendSignalControl {
                process: Arc::new(Backend::default()),
            }),
            true,
        );
        if controlled {
            assert_eq!(
                result.unwrap(),
                BackendSignalControlMode::ToolControlled,
                "{name}"
            );
        } else {
            // The incoherent offer is refused; the run goes on uncontrolled.
            assert!(
                matches!(
                    result,
                    Err(reverie::Error::Errno(reverie::syscalls::Errno::EINVAL))
                ),
                "{name}: {result:?}"
            );
        }
        assert_eq!(s.signal_control_installed(), controlled, "{name}");
        let (r, _, _) = add(&mut s, 100, 100);
        s.next_turns[&r].req.put(Ok(Resources::new(r)));
        s.runqueue_push_back(r);
        let scheduler = Arc::new(Mutex::new(s));
        let global = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let result = do_a_turn_blocking(scheduler.clone(), global, &Ok(Resources::new(r)))
            .now_or_never()
            .expect("a quiescent request must not block the turn")
            .expect("the only runnable thread must commit");
        assert_eq!(result.tid, r, "{name}");
        let s = scheduler.lock().unwrap();
        assert_eq!(s.turn, 1, "{name}");
        assert_eq!(s.controlled_turn_entries, u64::from(controlled), "{name}");
    }
}

/// Where a real timer goes is decided by the installed control alone. Without
/// one, Detcore's alarm model serves it, whatever the backend's capabilities.
/// A backend with process signal control never runs a sequentialized guest
/// without the control: reverie-kvm refuses that run before any guest code,
/// because Detcore reports the need through
/// `reverie::Tool::requires_signal_control`. So a capable scheduler without a
/// control is reached only in this test, and it gets no refusal of its own.
/// An installed control drives SIGALRM and refuses any other timer signal.
#[test]
fn real_timers_follow_the_installed_control() {
    let pid = DetTid::from_raw(100);

    // No installed control: Detcore's own alarm model, with or without the
    // capability, sequentialized or not.
    for (name, mut s) in [
        ("KVM capability, no control", fixture_with_control(false).0),
        ("PTRACE", scheduler_for(BackendCapabilities::PTRACE, false)),
        (
            "PTRACE sequentialized",
            scheduler_for(BackendCapabilities::PTRACE, true),
        ),
    ] {
        assert!(!s.signal_control_installed(), "{name}");
        assert_eq!(
            s.replace_real_timer(pid, pid, at(0), at(10), at(3), Signal::SIGALRM),
            Ok((at(0), at(0))),
            "{name}"
        );
        assert_eq!(
            s.blocked.timed_waiters.alarm_state(pid),
            Some((at(10), at(3))),
            "{name}"
        );
        assert_eq!(
            s.itimer_snapshot(pid, at(4)),
            Ok(super::real_timer::ItimerSnapshot {
                remaining: at(6),
                interval: at(3),
            }),
            "{name}"
        );
    }

    // An installed control: the process's real timer, SIGALRM only.
    let (mut s, _) = fixture();
    let (tid, _, _) = add(&mut s, 100, 100);
    assert_eq!(
        s.replace_real_timer(pid, tid, at(0), at(10), at(3), Signal::SIGVTALRM),
        Err(TimerFailure::Unsupported)
    );
    assert_eq!(
        s.replace_real_timer(pid, tid, at(0), at(10), at(3), Signal::SIGALRM),
        Ok((at(0), at(0)))
    );
    assert_eq!(s.blocked.timed_waiters.alarm_state(pid), None);
    assert_eq!(
        s.itimer_snapshot(pid, at(4)),
        Ok(super::real_timer::ItimerSnapshot {
            remaining: at(6),
            interval: at(3),
        })
    );
}

/// Must not hang or diverge: R5-F2 of the review of this series. The SaBRe
/// child `Q` leads its own process group and is suspended for a caught
/// SIGCHLD; its child `A` leads a different group. A SIGWINCH outside `Q`'s
/// waking set interrupted the real call, so `Q` has posted its continuation,
/// which waits behind `P`'s work. `P` then kills `Q` with
/// `kill(-Q, SIGKILL)`, the process-group spelling the SaBRe backend forwards.
/// The supervisor's report of `Q`'s death is host-timed: before the edge,
/// right after it, or after several held passes, and `A` exits after the edge.
/// In every order the group edge retires `Q` exactly as a positive-PID
/// `SIGKILL` does: its posted request is cancelled and never granted, no wake
/// is owed to it when `A` exits, `A` (in another group) lives on until its own
/// exit, and `P` gets one SIGCHLD wakeup once the reap is reported. All orders
/// end in the same run queue and the same first turn, which belongs to a
/// survivor. Before the fix the group kill sent no notification, so nothing
/// retired `Q`; when `A`'s exit was seen before `Q`'s death report, `Q` was
/// owed a wake and its dead continuation was admitted ahead of `P`'s work.
#[test]
fn process_group_sigkill_edge_retires_a_suspended_member_identically_in_every_report_order() {
    let mut outcomes = Vec::new();
    for order in [
        DeathReport::BeforeEdge,
        DeathReport::AfterEdge,
        DeathReport::AfterHeldPasses,
    ] {
        let case = format!("report {order:?}");
        let mut s = sabre_scheduler();
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&Config::default())));
        let time = global_time.lock().unwrap().as_nanos();
        let (parent, waiter, op_id, grandchild, grandchild_mm, sleeper) =
            killed_waiter_fixture(&mut s);
        let waiter_process = DetPid::from_raw(200);
        let grandchild_process = DetPid::from_raw(300);
        assert!(
            s.thread_tree
                .set_process_group(waiter_process, waiter_process)
        );
        assert!(
            s.thread_tree
                .set_process_group(grandchild_process, grandchild_process)
        );
        s.notify_signal_pending(waiter, SigWrapper::from(Signal::SIGWINCH));
        assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
        post_rt_sigsuspend_continuation(&mut s, waiter, op_id);
        let response = s.next_turns[&waiter].resp.clone();
        if matches!(order, DeathReport::BeforeEdge) {
            assert!(!report_process_death(&mut s, waiter), "{case}");
        }

        let reports = s.note_process_group_sigkill_sent(parent, waiter_process);
        assert_eq!(
            reports.len(),
            usize::from(!matches!(order, DeathReport::BeforeEdge)),
            "{case}"
        );
        assert!(!s.next_turns.contains_key(&waiter), "{case}");
        assert!(s.thread_is_logically_killed(waiter), "{case}");
        assert!(
            matches!(response.try_read(), Some(SchedResponse::Signaled(None))),
            "{case}"
        );
        assert!(
            !s.blocked.rt_sigsuspend_blockers.contains_key(&waiter),
            "{case}"
        );
        assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
        assert!(
            s.logically_exited_processes.contains(&waiter_process),
            "{case}"
        );
        assert_eq!(
            s.child_exit_signals_owed.get(&waiter_process),
            Some(&DetPid::from_raw(100)),
            "{case}"
        );
        // A leads another group, so the kill did not reach it.
        assert!(s.next_turns.contains_key(&grandchild), "{case}");
        assert!(!s.thread_is_logically_killed(grandchild), "{case}");

        grant_child_exit_and_remove_thread(&mut s, grandchild, grandchild_mm);
        assert!(
            s.complete_physical_process_exit(grandchild_process),
            "{case}"
        );
        assert!(s.blocked.rt_sigsuspend_wake_owed.is_empty(), "{case}");
        match order {
            DeathReport::BeforeEdge => {}
            DeathReport::AfterEdge => {
                assert!(report_process_death(&mut s, waiter), "{case}");
            }
            DeathReport::AfterHeldPasses => {
                assert!(s.step2_process_blocked(&global_time).is_err(), "{case}");
                let queued: Vec<DetTid> = s.run_queue.tids().copied().collect();
                assert!(!queued.contains(&waiter), "{case}");
                for _ in 0..2 {
                    assert!(s.step2_process_blocked(&global_time).is_err(), "{case}");
                    assert!(s.take_terminal_deadlock().is_none(), "{case}");
                }
                assert_eq!(
                    s.run_queue.tids().copied().collect::<Vec<_>>(),
                    queued,
                    "{case}"
                );
                assert_eq!(global_time.lock().unwrap().as_nanos(), time, "{case}");
                assert!(report_process_death(&mut s, waiter), "{case}");
            }
        }
        for mut report in reports {
            assert!(matches!(report.try_recv(), Ok(Some(()))), "{case}");
        }
        assert!(s.step2_process_blocked(&global_time).is_ok(), "{case}");
        assert!(s.take_terminal_deadlock().is_none(), "{case}");
        assert!(s.child_exit_signals_owed.is_empty(), "{case}");
        assert_eq!(global_time.lock().unwrap().as_nanos(), time, "{case}");
        let queue: Vec<DetTid> = s.run_queue.tids().copied().collect();
        assert!(
            queue.contains(&parent) && queue.contains(&sleeper) && !queue.contains(&waiter),
            "{case}: {queue:?}"
        );
        let (first, _, _) = s.select_test_turn().expect("a survivor is runnable");
        assert_ne!(first, waiter, "{case}");
        outcomes.push((queue, first, s.owed_child_exit_wakeups));
    }
    assert!(
        outcomes.iter().all(|outcome| *outcome == outcomes[0]),
        "{outcomes:?}"
    );
}

/// A process-group `SIGKILL` retires exactly the live processes Detcore
/// records in that group, in `DetPid` order, each once: not a process of
/// another group, and not the sender's own process, which the kill reaches
/// too but which is left to the existing paths as a self-kill is. The edge
/// waits for the reap report of every retired process whose parent is owed
/// its exit signal. A group with no members, and a backend without SaBRe's
/// whole profile, retire nothing.
#[test]
fn process_group_sigkill_edge_retires_every_member_but_the_senders_own_process() {
    let group = DetPid::from_raw(200);
    let mut s = sabre_scheduler();
    let (parent, _, _) = add(&mut s, 100, 100);
    let (first, _, _) = add_process_child(&mut s, 100, 200);
    let (second, _, _) = add_process_child(&mut s, 100, 300);
    let (other, _, _) = add_process_child(&mut s, 100, 400);
    assert!(
        s.thread_tree
            .set_process_group(DetPid::from_raw(200), group)
    );
    assert!(
        s.thread_tree
            .set_process_group(DetPid::from_raw(300), group)
    );
    queue_ordinary_work(&mut s, parent);
    queue_ordinary_work(&mut s, other);

    assert!(
        s.note_process_group_sigkill_sent(parent, DetPid::from_raw(999))
            .is_empty()
    );
    let mut reports = s.note_process_group_sigkill_sent(parent, group);
    assert_eq!(reports.len(), 2);
    for tid in [first, second] {
        assert!(!s.next_turns.contains_key(&tid));
        assert!(s.thread_is_logically_killed(tid));
    }
    assert!(s.next_turns.contains_key(&other) && s.next_turns.contains_key(&parent));
    assert!(matches!(reports[0].try_recv(), Ok(None)));
    assert!(matches!(reports[1].try_recv(), Ok(None)));
    assert!(s.complete_physical_process_exit(DetPid::from_raw(200)));
    assert!(matches!(reports[0].try_recv(), Ok(Some(()))));
    assert!(matches!(reports[1].try_recv(), Ok(None)));
    assert!(s.complete_physical_process_exit(DetPid::from_raw(300)));
    assert!(matches!(reports[1].try_recv(), Ok(Some(()))));
    assert!(s.note_process_group_sigkill_sent(parent, group).is_empty());
    assert!(s.step2_drain_prefix().is_ok());
    assert_eq!(s.owed_child_exit_wakeups, 2);

    // The sender's own group: its own process is skipped, `other` is retired.
    let own_group = DetPid::from_raw(100);
    let reports = s.note_process_group_sigkill_sent(parent, own_group);
    assert_eq!(reports.len(), 1);
    assert!(s.thread_is_logically_killed(other));
    assert!(s.next_turns.contains_key(&parent));
    assert!(!s.thread_is_logically_killed(parent));
    assert!(s.take_terminal_deadlock().is_none());

    for mut s in [
        physical_exit_reporting_scheduler(),
        Scheduler::new(&Config::default().with_backend(|backend| {
            backend.needs_killed_thread_rpc_cancellation = true;
        })),
    ] {
        let (parent, _, _) = add(&mut s, 100, 100);
        let (child, _, _) = add_process_child(&mut s, 100, 200);
        assert!(
            s.thread_tree
                .set_process_group(DetPid::from_raw(200), group)
        );
        assert!(s.note_process_group_sigkill_sent(parent, group).is_empty());
        assert!(s.next_turns.contains_key(&child));
        assert!(s.child_exit_signals_owed.is_empty());
    }
}
