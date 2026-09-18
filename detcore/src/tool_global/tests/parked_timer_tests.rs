/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// These tests exercise the actual RPC receiver and daemon. The signal receipt
// is a controlled input, not evidence that a backend hook or guest ran.
use reverie::CallbackSignalSite;
use reverie::PendingDomain;
use reverie::PreparedSignalToken;
use reverie::ProcessAlarmSignalDisposition;
use reverie::ProcessAlarmSignalOutcome;
use reverie::ProcessAlarmSignalReceipt;
use reverie::SignalConsumer;
use reverie::SignalDequeue;
use reverie::SignalEvent;
use reverie::SignalProcessId;
use reverie::SignalTarget;
use reverie::SignalTaskIdentity;
use reverie::Tool;

use super::*;
use crate::scheduler::SkipTurn;
use crate::scheduler::do_a_turn_blocking;
use crate::scheduler::parked::*;
use crate::scheduler::real_timer::DequeueAck;
use crate::scheduler::real_timer::TimerFailure;
use crate::types::DetTime;
use crate::types::GlobalTime;

struct Fixture {
    state: GlobalState,
    tid: DetTid,
    mm: MmId,
    clock: DetTime,
    identity: SignalTaskIdentity,
    site: CallbackSignalSite,
}
impl Fixture {
    async fn started() -> Self {
        let config = Config {
            backend_is_kvm: true,
            kvm_shared_dequeue_timers: true,
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: false,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let tid = DetTid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(tid, tid, true);
        install_test_registration(&state, tid, Ivar::new());
        let tool: Detcore = Detcore::new(Tid::from_raw(17), &config);
        let thread = tool.init_thread_state(Tid::from_raw(17), None);
        let identity = SignalTaskIdentity {
            process: SignalProcessId {
                tgid: Tid::from_raw(17),
                generation: 1,
            },
            tid: Tid::from_raw(17),
            task_generation: 1,
        };
        let site = CallbackSignalSite {
            process: identity.process,
            tid: identity.tid,
            task_generation: 1,
            callback_nonce: 1,
            boundary_nonce: 1,
        };
        let fixture = Self {
            state,
            tid,
            mm: thread.mm_id,
            clock: thread.thread_logical_time,
            identity,
            site,
        };
        let mut start = Box::pin(fixture.rpc(GlobalRequest::StartNewThread(
            tid,
            tid,
            None,
            Some(identity),
        )));
        assert!(futures::poll!(start.as_mut()).is_pending());
        assert!(futures::poll!(start.as_mut()).is_pending());
        let binding = fixture
            .state
            .sched
            .lock()
            .unwrap()
            .real_timers
            .validate_task(tid, tid, fixture.mm, identity);
        assert!(binding.is_ok(), "startup must bind before the first grant");
        let committed = do_a_turn_blocking(
            fixture.state.sched.clone(),
            fixture.state.global_time.clone(),
            &Err(SkipTurn),
        )
        .await
        .unwrap();
        assert!(
            committed
                .resources
                .contains_key(&ResourceID::MemAddrSpace(tid))
        );
        assert!(matches!(start.await, GlobalResponse::StartNewThread(None)));
        fixture
    }
    async fn rpc(&self, request: GlobalRequest) -> GlobalResponse {
        self.state
            .receive_rpc(
                Tid::from_raw(self.tid.as_raw()),
                (self.clock.clone(), self.mm, request),
            )
            .await
            .1
    }
    async fn prepare_exec(&self) {
        assert_eq!(
            self.rpc(GlobalRequest::PrepareExec(
                self.tid,
                self.mm,
                Default::default()
            ))
            .await,
            GlobalResponse::PrepareExec(())
        );
    }
    async fn finish_exec(&mut self) {
        self.mm = self.mm.for_exec(self.tid);
        assert!(matches!(
            self.rpc(GlobalRequest::MarkPastFirstExecve(Some(self.identity)))
                .await,
            GlobalResponse::MarkPastFirstExecve(_)
        ));
    }
    fn now(&self) -> LogicalTime {
        self.state.global_time.lock().unwrap().as_nanos()
    }
    fn resources(&self, resource: ResourceID) -> Resources {
        let mut result = Resources::new(self.tid);
        result.insert(resource, Permission::RW);
        result
    }
    fn effect(&self, sequence: u64) -> SignalDequeue {
        let mut info = [0; 128];
        info[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
        SignalDequeue {
            process: self.identity.process,
            sequence,
            consumer: SignalConsumer::ReturnToUser,
            domain: PendingDomain::Process,
            event: SignalEvent::new(
                libc::SIGALRM,
                info,
                SignalTarget::Process {
                    pid: self.identity.process.tgid,
                },
            )
            .unwrap(),
        }
    }
}

#[tokio::test]
async fn captured_write_rpc_restores_original_request_before_one_real_grant() {
    let mut fixture = Fixture::started().await;
    fixture.prepare_exec().await;
    fixture.finish_exec().await;
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::RegisterAlarm(
                fixture.tid,
                fixture.tid,
                LogicalTime::from_nanos(1),
                LogicalTime::ZERO,
                SigWrapper::from(Signal::SIGALRM),
            ))
            .await,
        GlobalResponse::RegisterAlarm(_)
    ));
    let request_time = fixture.now() + LogicalTime::from_nanos(100);
    fixture.clock.advance_to(request_time);
    let mut resources = Resources::new(fixture.tid);
    resources.insert(
        ResourceID::Device(crate::resources::Device::ContainerStdout),
        Permission::W,
    );
    let mut original = Box::pin(fixture.rpc(GlobalRequest::ParkedRequest(
        resources.clone(),
        fixture.tid,
        ControlCapability::CapturedWrite { site: fixture.site },
    )));
    assert!(futures::poll!(original.as_mut()).is_pending());
    let (request, response, turns) = {
        let scheduler = fixture.state.sched.lock().unwrap();
        let turn = &scheduler.next_turns[&fixture.tid];
        (turn.req.clone(), turn.resp.clone(), scheduler.turn)
    };
    let sched = fixture.state.sched.clone();
    let global_time = fixture.state.global_time.clone();
    let skipped = Err(SkipTurn);
    let mut daemon = Box::pin(do_a_turn_blocking(sched, global_time, &skipped));
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    let GlobalResponse::ParkedRequest(ResourceReply::PublishAlarm(control)) = original.await else {
        panic!("due alarm must consume only the original response");
    };
    assert_eq!(fixture.state.sched.lock().unwrap().turn, turns);
    assert!(fixture.now() >= request_time);
    // The caller can report newer time while acknowledging the publication.
    // Control transport must retain that observation, without inventing a turn.
    let ack_time = fixture.now() + LogicalTime::from_nanos(37);
    fixture.clock.advance_to(ack_time);
    let mut publication = Box::pin(fixture.rpc(GlobalRequest::AlarmPublicationAck(
        *control,
        ProcessAlarmSignalOutcome::Accepted(ProcessAlarmSignalReceipt {
            blocked: false,
            disposition: ProcessAlarmSignalDisposition::Caught,
            pending_generation: 1,
            coalesced: false,
        }),
    )));
    assert!(futures::poll!(publication.as_mut()).is_pending());
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    let GlobalResponse::AlarmPublicationAck(Ok(PublicationActivation::AwaitResume(ticket))) =
        publication.await
    else {
        panic!("an unblocked captured write still resumes without observation");
    };
    assert!(fixture.now() >= ack_time);
    assert_eq!(fixture.state.sched.lock().unwrap().turn, turns);
    let mut resumed = Box::pin(fixture.rpc(GlobalRequest::ResumeParkedRequest {
        ticket,
        current_site: fixture.site,
    }));
    assert!(futures::poll!(resumed.as_mut()).is_pending());
    assert_eq!(daemon.await.unwrap(), resources);
    assert_eq!(request.try_read().unwrap().unwrap(), resources);
    assert_eq!(
        resumed.await,
        GlobalResponse::ResumeParkedRequest(ResourceReply::Grant(ResumeStatus::Normal))
    );
    assert_eq!(fixture.state.sched.lock().unwrap().turn, turns + 1);
    assert!(matches!(
        response.try_read(),
        Some(SchedResponse::PublishAlarm(_))
    ));
    assert!(fixture.now() >= ack_time);
}

#[tokio::test]
async fn parked_rpc_caught_return_charges_the_last_real_hook_only_after_callback_parks() {
    let mut fixture = Fixture::started().await;
    fixture.prepare_exec().await;
    fixture.finish_exec().await;
    let interval = LogicalTime::from_nanos(10_000_000);
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::RegisterAlarm(
                fixture.tid,
                fixture.tid,
                interval,
                interval,
                SigWrapper::from(Signal::SIGALRM)
            ))
            .await,
        GlobalResponse::RegisterAlarm((_, _))
    ));
    let deadline = fixture.now() + LogicalTime::from_nanos(100_000_000);
    let mut original = Box::pin(fixture.rpc(GlobalRequest::ParkedRequest(
        fixture.resources(ResourceID::SleepUntil(deadline)),
        fixture.tid,
        ControlCapability::ParkedWait {
            policy: ParkedWaitPolicy::NanosleepNoHandlerRestart {
                absolute_deadline: deadline,
            },
            site: fixture.site,
        },
    )));
    assert!(futures::poll!(original.as_mut()).is_pending());
    let skipped = Err(SkipTurn);
    assert!(
        do_a_turn_blocking(
            fixture.state.sched.clone(),
            fixture.state.global_time.clone(),
            &skipped
        )
        .await
        .is_err()
    );
    let mut expiry = Box::pin(do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &skipped,
    ));
    assert!(futures::poll!(expiry.as_mut()).is_pending());
    let GlobalResponse::ParkedRequest(ResourceReply::PublishAlarm(control)) = original.await else {
        panic!("original RPC must receive publication")
    };
    let mut publication = Box::pin(fixture.rpc(GlobalRequest::AlarmPublicationAck(
        *control,
        ProcessAlarmSignalOutcome::Accepted(ProcessAlarmSignalReceipt {
            blocked: false,
            disposition: ProcessAlarmSignalDisposition::Caught,
            pending_generation: 1,
            coalesced: false,
        }),
    )));
    assert!(futures::poll!(publication.as_mut()).is_pending());
    assert!(matches!(
        futures::poll!(expiry.as_mut()),
        std::task::Poll::Ready(Err(_))
    ));
    let GlobalResponse::AlarmPublicationAck(Ok(PublicationActivation::Observe { wait, lease })) =
        publication.await
    else {
        panic!("daemon must admit observation")
    };
    assert_eq!(
        fixture
            .rpc(GlobalRequest::SignalDequeued {
                detpid: fixture.tid,
                identity: fixture.identity,
                dequeue: fixture.effect(1)
            })
            .await,
        GlobalResponse::SignalDequeued {
            ack: Ok(DequeueAck::Applied { sequence: 1 }),
            terminal: false
        }
    );
    let mut hook = Box::pin(fixture.rpc(GlobalRequest::ParkedRequest(
        fixture.resources(ResourceID::InboundSignal(SigWrapper::from(Signal::SIGALRM))),
        fixture.tid,
        ControlCapability::PublishOnly {
            lease,
            site: fixture.site,
        },
    )));
    assert!(futures::poll!(hook.as_mut()).is_pending());
    let hook_turn = do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &skipped,
    )
    .await
    .unwrap();
    assert!(matches!(
        hook.await,
        GlobalResponse::ParkedRequest(ResourceReply::Grant(_))
    ));
    let before = fixture.now();
    let turns = fixture.state.sched.lock().unwrap().turn;
    let last_hook = Ok(hook_turn);
    let mut daemon = Box::pin(do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &last_hook,
    ));
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    let mut finish = Box::pin(fixture.rpc(GlobalRequest::FinishParkedObservation {
        wait,
        lease,
        site: fixture.site,
        finish: ObservationFinish::InterruptForCaught {
            selection: PreparedSignalToken {
                site: fixture.site,
                selection_nonce: 1,
            },
        },
    }));
    assert!(futures::poll!(finish.as_mut()).is_pending());
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    assert_eq!(
        finish.await,
        GlobalResponse::FinishParkedObservation(Ok(FinishAck::Interrupted))
    );
    for _ in 0..3 {
        assert!(futures::poll!(daemon.as_mut()).is_pending());
        assert_eq!(fixture.now(), before, "caught return is still executing");
        assert_eq!(fixture.state.sched.lock().unwrap().turn, turns);
    }
    let mut posthook = Box::pin(fixture.rpc(GlobalRequest::RequestResources(
        fixture.resources(ResourceID::MemAddrSpace(fixture.tid)),
        fixture.tid,
    )));
    assert!(futures::poll!(posthook.as_mut()).is_pending());
    assert!(matches!(
        futures::poll!(daemon.as_mut()),
        std::task::Poll::Ready(Ok(_))
    ));
    let mut model = GlobalTime::new(&fixture.state.cfg);
    let initial = model.as_nanos();
    let one_turn = model.add_scheduler_time() - initial;
    assert_eq!(
        fixture.now(),
        before + one_turn,
        "one real hook turn is charged once"
    );
    assert!(matches!(
        posthook.await,
        GlobalResponse::RequestResources(_)
    ));
    assert_eq!(
        fixture.state.sched.lock().unwrap().turn,
        turns + 1,
        "caught handoff did not commit the abandoned sleep"
    );
}

#[tokio::test]
async fn post_exec_rpc_preserves_timer_and_dequeue_sequence_with_new_task_identity() {
    let mut fixture = Fixture::started().await;
    let before_mm = fixture.mm;
    let before_identity = fixture.identity;
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::SignalDequeued {
                detpid: fixture.tid,
                identity: fixture.identity,
                dequeue: fixture.effect(1)
            })
            .await,
        GlobalResponse::SignalDequeued {
            ack: Ok(DequeueAck::Applied { sequence: 1 }),
            terminal: false
        }
    ));
    let interval = LogicalTime::from_nanos(100_000_000);
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::RegisterAlarm(
                fixture.tid,
                fixture.tid,
                interval,
                interval,
                SigWrapper::from(Signal::SIGALRM)
            ))
            .await,
        GlobalResponse::RegisterAlarm(_)
    ));
    let before = fixture
        .state
        .sched
        .lock()
        .unwrap()
        .itimer_snapshot(fixture.tid, fixture.now())
        .unwrap();
    fixture.prepare_exec().await;
    fixture.identity.task_generation += 1;
    fixture.finish_exec().await;
    {
        let scheduler = fixture.state.sched.lock().unwrap();
        assert!(
            scheduler
                .real_timers
                .validate_task(fixture.tid, fixture.tid, fixture.mm, fixture.identity)
                .is_ok()
        );
        assert_eq!(
            scheduler.real_timers.validate_task(
                fixture.tid,
                fixture.tid,
                before_mm,
                before_identity
            ),
            Err(TimerFailure::Identity)
        );
        assert_eq!(
            scheduler
                .itimer_snapshot(fixture.tid, fixture.now())
                .unwrap(),
            before
        );
    }
    assert!(fixture.state.pending_exec_states.lock().unwrap().is_empty());
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::SignalDequeued {
                detpid: fixture.tid,
                identity: fixture.identity,
                dequeue: fixture.effect(2)
            })
            .await,
        GlobalResponse::SignalDequeued {
            ack: Ok(DequeueAck::Applied { sequence: 2 }),
            terminal: false
        }
    ));
    assert_eq!(
        fixture
            .state
            .sched
            .lock()
            .unwrap()
            .itimer_snapshot(fixture.tid, fixture.now())
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn post_exec_rpc_refuses_unprepared_foreign_and_stale_lifetimes() {
    for case in 0..3 {
        let mut fixture = Fixture::started().await;
        let before_mm = fixture.mm;
        let before_identity = fixture.identity;
        match case {
            0 => fixture.mm = fixture.mm.for_exec(fixture.tid),
            1 => {
                fixture.prepare_exec().await;
                fixture.mm = fixture.mm.for_exec(fixture.tid);
                fixture.identity.process.generation += 1;
            }
            2 => fixture.identity.task_generation += 1,
            _ => unreachable!(),
        }
        assert_eq!(
            fixture
                .rpc(GlobalRequest::MarkPastFirstExecve(Some(fixture.identity)))
                .await,
            GlobalResponse::ThreadExited
        );
        let scheduler = fixture.state.sched.lock().unwrap();
        assert!(scheduler.backend_failed());
        assert!(
            scheduler
                .real_timers
                .validate_task(fixture.tid, fixture.tid, before_mm, before_identity)
                .is_ok()
        );
    }
}

#[tokio::test]
async fn failed_exec_cancellation_retains_the_original_signal_binding() {
    let mut fixture = Fixture::started().await;
    fixture.prepare_exec().await;
    assert_eq!(
        fixture.rpc(GlobalRequest::CancelExec(fixture.tid)).await,
        GlobalResponse::CancelExec(())
    );
    assert!(
        fixture
            .state
            .sched
            .lock()
            .unwrap()
            .real_timers
            .validate_task(fixture.tid, fixture.tid, fixture.mm, fixture.identity)
            .is_ok()
    );
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::SignalDequeued {
                detpid: fixture.tid,
                identity: fixture.identity,
                dequeue: fixture.effect(1)
            })
            .await,
        GlobalResponse::SignalDequeued {
            ack: Ok(DequeueAck::Applied { sequence: 1 }),
            terminal: false
        }
    ));
    fixture.mm = fixture.mm.for_exec(fixture.tid);
    assert_eq!(
        fixture
            .rpc(GlobalRequest::MarkPastFirstExecve(Some(fixture.identity)))
            .await,
        GlobalResponse::ThreadExited
    );
}

#[tokio::test]
async fn dequeue_acknowledges_without_a_turn_while_sibling_requests_are_unfilled() {
    let fixture = Fixture::started().await;
    let sibling = DetTid::from_raw(18);
    let identity = SignalTaskIdentity {
        process: fixture.identity.process,
        tid: Tid::from_raw(sibling.as_raw()),
        task_generation: 2,
    };
    let clock = fixture.clock.clone_for_child();
    // Controlled already-admitted sibling. Startup admission has its own
    // actual RPC control; this test isolates the consuming notification path.
    fixture
        .state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(fixture.tid, sibling, false);
    install_test_registration(&fixture.state, sibling, Ivar::new());
    fixture
        .state
        .sched
        .lock()
        .unwrap()
        .real_timers
        .bind(fixture.tid, sibling, fixture.mm, identity)
        .unwrap();
    fixture
        .state
        .global_time
        .lock()
        .unwrap()
        .update_global_time(sibling, clock.as_nanos(), clock.inherited_nanos());
    let time = fixture.now();
    let turn = fixture.state.sched.lock().unwrap().turn;
    let skipped = Err(SkipTurn);
    let mut daemon = Box::pin(do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &skipped,
    ));
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    for (sequence, from, identity, clock) in [
        (1, fixture.tid, fixture.identity, fixture.clock.clone()),
        (2, sibling, identity, clock),
    ] {
        let mut notification = Box::pin(fixture.state.receive_rpc(
            Tid::from_raw(from.as_raw()),
            (
                clock,
                fixture.mm,
                GlobalRequest::SignalDequeued {
                    detpid: fixture.tid,
                    identity,
                    dequeue: fixture.effect(sequence),
                },
            ),
        ));
        assert_eq!(
            futures::poll!(notification.as_mut()),
            std::task::Poll::Ready((
                None,
                GlobalResponse::SignalDequeued {
                    ack: Ok(DequeueAck::Applied { sequence }),
                    terminal: false
                }
            ))
        );
        assert!(futures::poll!(daemon.as_mut()).is_pending());
        assert_eq!(fixture.now(), time);
        let scheduler = fixture.state.sched.lock().unwrap();
        assert_eq!(scheduler.turn, turn);
        for tid in [fixture.tid, sibling] {
            assert!(scheduler.next_turns[&tid].req.try_read().is_none());
            assert!(scheduler.next_turns[&tid].resp.try_read().is_none());
        }
    }
}

// Actual RPC/daemon controls. Backend receipts are controlled inputs: these
// tests prove scheduler boundaries, not the backend's callback/frame ordering.
async fn newer_dequeue_clock_reaches_next_deadline(caught: bool) {
    let mut fixture = Fixture::started().await;
    fixture.prepare_exec().await;
    fixture.finish_exec().await;
    let period = LogicalTime::from_nanos(100);
    assert!(matches!(
        fixture
            .rpc(GlobalRequest::RegisterAlarm(
                fixture.tid,
                fixture.tid,
                LogicalTime::from_nanos(1),
                period,
                SigWrapper::from(Signal::SIGALRM),
            ))
            .await,
        GlobalResponse::RegisterAlarm(_)
    ));
    fixture.clock.add_syscall_with_cost(100);
    let mut write = Resources::new(fixture.tid);
    write.insert(
        ResourceID::Device(crate::resources::Device::ContainerStdout),
        Permission::W,
    );
    let sleep_until = fixture.now() + LogicalTime::from_nanos(10_000_000);
    let (resources, capability) = if caught {
        (
            fixture.resources(ResourceID::SleepUntil(sleep_until)),
            ControlCapability::ParkedWait {
                policy: ParkedWaitPolicy::NanosleepNoHandlerRestart {
                    absolute_deadline: sleep_until,
                },
                site: fixture.site,
            },
        )
    } else {
        (
            write.clone(),
            ControlCapability::CapturedWrite { site: fixture.site },
        )
    };
    let initial_turn = fixture.state.sched.lock().unwrap().turn;
    let mut original = Box::pin(fixture.rpc(GlobalRequest::ParkedRequest(
        resources,
        fixture.tid,
        capability,
    )));
    assert!(futures::poll!(original.as_mut()).is_pending());
    let skipped = Err(SkipTurn);
    let mut daemon = Box::pin(do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &skipped,
    ));
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    let GlobalResponse::ParkedRequest(ResourceReply::PublishAlarm(first)) = original.await else {
        panic!("the first deadline must precede an original request grant");
    };
    let mut publication = Box::pin(fixture.rpc(GlobalRequest::AlarmPublicationAck(
        *first,
        ProcessAlarmSignalOutcome::Accepted(ProcessAlarmSignalReceipt {
            // The no-hook case keeps SIGALRM masked through Write return;
            // signalfd then consumes it instead of return-to-user delivery.
            blocked: !caught,
            disposition: ProcessAlarmSignalDisposition::Caught,
            pending_generation: 1,
            coalesced: false,
        }),
    )));
    assert!(futures::poll!(publication.as_mut()).is_pending());
    assert!(futures::poll!(daemon.as_mut()).is_pending());
    let GlobalResponse::AlarmPublicationAck(Ok(activation)) = publication.await else {
        panic!("publication must be acknowledged");
    };
    let mut model = GlobalTime::new(&fixture.state.cfg);
    let model_start = model.as_nanos();
    let charge = model.add_scheduler_time() - model_start;
    assert!(charge > period);
    let mut caught_observation = None;
    let last_grant = match activation {
        PublicationActivation::Observe { wait, lease } => {
            assert!(caught);
            let before = fixture.now();
            fixture.clock.add_syscall_with_cost(37);
            assert_eq!(
                fixture
                    .rpc(GlobalRequest::SignalDequeued {
                        detpid: fixture.tid,
                        identity: fixture.identity,
                        dequeue: fixture.effect(1),
                    })
                    .await,
                GlobalResponse::SignalDequeued {
                    ack: Ok(DequeueAck::Applied { sequence: 1 }),
                    terminal: false,
                }
            );
            assert_eq!(fixture.now(), before + LogicalTime::from_nanos(37));
            assert_eq!(fixture.state.sched.lock().unwrap().turn, initial_turn);
            let mut hook = Box::pin(fixture.rpc(GlobalRequest::ParkedRequest(
                fixture.resources(ResourceID::InboundSignal(SigWrapper::from(Signal::SIGALRM))),
                fixture.tid,
                ControlCapability::PublishOnly {
                    lease,
                    site: fixture.site,
                },
            )));
            assert!(futures::poll!(hook.as_mut()).is_pending());
            let granted = daemon
                .await
                .expect("the real signal hook must receive its grant");
            assert!(matches!(
                hook.await,
                GlobalResponse::ParkedRequest(ResourceReply::Grant(_))
            ));
            // The refresh observes the newer dequeue clock; it must not charge
            // a fictitious preceding turn merely because transport was awaited.
            assert_eq!(fixture.now(), before + LogicalTime::from_nanos(37));
            assert_eq!(
                fixture.state.sched.lock().unwrap().committed_time,
                fixture.now()
            );
            assert_eq!(fixture.state.sched.lock().unwrap().turn, initial_turn + 1);
            caught_observation = Some((wait, lease));
            granted
        }
        PublicationActivation::AwaitResume(ticket) => {
            assert!(!caught);
            let before = fixture.now();
            let mut resumed = Box::pin(fixture.rpc(GlobalRequest::ResumeParkedRequest {
                ticket,
                current_site: fixture.site,
            }));
            assert!(futures::poll!(resumed.as_mut()).is_pending());
            let written = daemon
                .await
                .expect("captured publication must restore its real Write");
            assert_eq!(written, write);
            assert!(matches!(
                resumed.await,
                GlobalResponse::ResumeParkedRequest(ResourceReply::Grant(_))
            ));
            assert_eq!(fixture.now(), before);
            assert_eq!(fixture.state.sched.lock().unwrap().turn, initial_turn + 1);
            // A normal polling read grant precedes a controlled signalfd
            // removal. SignalFd has no structured signal hook; it still owes
            // the real read turn's charge before another request can run.
            let mut read = Box::pin(fixture.rpc(GlobalRequest::RequestResources(
                fixture.resources(ResourceID::InternalIOPolling),
                fixture.tid,
            )));
            assert!(futures::poll!(read.as_mut()).is_pending());
            let granted = do_a_turn_blocking(
                fixture.state.sched.clone(),
                fixture.state.global_time.clone(),
                &Ok(written),
            )
            .await
            .expect("the consuming read must receive a real grant");
            assert!(matches!(read.await, GlobalResponse::RequestResources(_)));
            assert_eq!(fixture.now(), before + charge);
            let mut effect = fixture.effect(1);
            effect.consumer = SignalConsumer::SignalFd;
            fixture.clock.add_syscall_with_cost(37);
            assert_eq!(
                fixture
                    .rpc(GlobalRequest::SignalDequeued {
                        detpid: fixture.tid,
                        identity: fixture.identity,
                        dequeue: effect,
                    })
                    .await,
                GlobalResponse::SignalDequeued {
                    ack: Ok(DequeueAck::Applied { sequence: 1 }),
                    terminal: false,
                }
            );
            assert_eq!(fixture.now(), before + charge + LogicalTime::from_nanos(37));
            assert_eq!(fixture.state.sched.lock().unwrap().turn, initial_turn + 2);
            granted
        }
    };
    let before = fixture.now();
    let before_turn = fixture.state.sched.lock().unwrap().turn;
    let deadline = fixture
        .state
        .sched
        .lock()
        .unwrap()
        .blocked
        .timed_waiters
        .next_deadline()
        .unwrap();
    assert!(before < deadline && deadline <= before + period);
    let last_turn = Ok(last_grant);
    let mut next = Box::pin(do_a_turn_blocking(
        fixture.state.sched.clone(),
        fixture.state.global_time.clone(),
        &last_turn,
    ));
    assert!(futures::poll!(next.as_mut()).is_pending());
    assert_eq!(
        fixture.now(),
        before,
        "the current callback has not parked again"
    );
    if let Some((wait, lease)) = caught_observation {
        let mut finish = Box::pin(fixture.rpc(GlobalRequest::FinishParkedObservation {
            wait,
            lease,
            site: fixture.site,
            finish: ObservationFinish::InterruptForCaught {
                selection: PreparedSignalToken {
                    site: fixture.site,
                    selection_nonce: 1,
                },
            },
        }));
        assert!(futures::poll!(finish.as_mut()).is_pending());
        assert!(futures::poll!(next.as_mut()).is_pending());
        assert_eq!(
            finish.await,
            GlobalResponse::FinishParkedObservation(Ok(FinishAck::Interrupted))
        );
        assert_eq!(
            fixture.now(),
            before,
            "caught return still owns the empty gate"
        );
    }
    // A fresh callback after the actual grant contributes newer guest time.
    // This fixture supplies the receipt/site: backend frame ordering is
    // independently exercised by the real guest and backend controls.
    fixture.clock.add_syscall_with_cost(11);
    fixture.site.callback_nonce += 1;
    fixture.site.boundary_nonce += 1;
    let mut original = Box::pin(fixture.rpc(GlobalRequest::ParkedRequest(
        write,
        fixture.tid,
        ControlCapability::CapturedWrite { site: fixture.site },
    )));
    assert!(futures::poll!(original.as_mut()).is_pending());
    assert_eq!(fixture.now(), before + LogicalTime::from_nanos(11));
    assert!(futures::poll!(next.as_mut()).is_pending());
    let GlobalResponse::ParkedRequest(ResourceReply::PublishAlarm(second)) = original.await else {
        panic!("the adjacent periodic deadline must precede the next guest grant");
    };
    assert_eq!(second.expiry.life, first.expiry.life);
    assert_eq!(second.expiry.arm, first.expiry.arm);
    assert_eq!(second.expiry.ordinal, first.expiry.ordinal + 1);
    assert_eq!(fixture.now(), before + LogicalTime::from_nanos(11) + charge);
    let scheduler = fixture.state.sched.lock().unwrap();
    assert_eq!(scheduler.committed_time, fixture.now());
    assert_eq!(
        scheduler.turn, before_turn,
        "publication is not another COMMIT"
    );
    assert!(scheduler.blocked.timed_waiters.next_deadline().is_none());
}

#[tokio::test]
async fn newer_caught_dequeue_time_expires_adjacent_timer_before_the_next_grant() {
    newer_dequeue_clock_reaches_next_deadline(true).await;
}

#[tokio::test]
async fn newer_signalfd_dequeue_time_expires_adjacent_timer_before_the_next_grant() {
    newer_dequeue_clock_reaches_next_deadline(false).await;
}
