// Copyright (c) Meta Platforms, Inc. and affiliates.
// Licensed under the BSD-style license in the LICENSE file.

//! The target callback acknowledges delivery before the original request can resume.

use super::*;

impl GlobalState {
    pub(super) async fn recv_child_exit_resources(
        &self,
        tid: DetTid,
        mm: MmId,
        parent: DetPid,
        original: Resources,
    ) -> GlobalResponse {
        let registered = {
            let mut state = self.sched.lock().unwrap();
            if state.thread_is_logically_killed(tid) || !state.rpc_incarnation_matches(tid, mm) {
                return GlobalResponse::ThreadExited;
            }
            let nextturn = state
                .next_turns
                .get(&tid)
                .expect("resource caller must be registered")
                .clone();
            let guest_time = self.global_time.lock().unwrap().threads_time(tid);
            match state.begin_child_exit_operation(tid, mm, parent, original.clone(), guest_time) {
                Ok(operation) => {
                    state.request_put(&nextturn.req, original, &self.global_time);
                    Ok((operation, nextturn.resp))
                }
                Err(failure) => Err(failure),
            }
        };
        let (operation, response) = match registered {
            Ok(value) => value,
            Err(failure) => failure.terminate(),
        };
        self.await_child_exit_resources(operation, response).await
    }

    pub(super) async fn await_child_exit_resources(
        &self,
        operation: Arc<child_exit::Operation>,
        response: Ivar<SchedResponse>,
    ) -> GlobalResponse {
        // This guard is outside every scheduler/operation lock scope. There is no
        // await while holding either mutex, including every early return below.
        let mut guard = child_exit::ReceiveGuard::new(Some(operation.clone()));
        let answer = response.get().await;
        let id = operation.id;
        let retired = {
            let state = self.sched.lock().unwrap();
            state.thread_is_logically_killed(id.tid)
                || !state.rpc_incarnation_matches(id.tid, id.mm)
                || !state.next_turns.contains_key(&id.tid)
        };
        if retired {
            operation.retire();
            // The receiver still owns an unreturned control, so its guard can settle
            // that attempt without asking a callback or exit hook to run.
            drop(guard);
            return GlobalResponse::ThreadExited;
        }
        if let SchedResponse::DeliverChildExit(command) = answer {
            let command = *command;
            let (failure, retired) = {
                let mut phase = operation.phase.lock().unwrap();
                match &mut *phase {
                    child_exit::Phase::Delivering(attempt)
                        if attempt.command == command
                            && attempt.phase == child_exit::AttemptPhase::NotReturned =>
                    {
                        attempt.phase = child_exit::AttemptPhase::InCallback;
                        (None, false)
                    }
                    child_exit::Phase::Delivering(attempt)
                        if attempt.phase == child_exit::AttemptPhase::TerminalAwaitingCallback =>
                    {
                        (None, true)
                    }
                    _ => (
                        Some(child_exit::FatalRecord::new(child_exit::Failure::protocol(
                            id,
                            command.delivery.id,
                            command.delivery.child,
                        ))),
                        false,
                    ),
                }
            };
            if let Some(failure) = failure {
                failure.terminate();
            }
            if retired {
                drop(guard);
                return GlobalResponse::ThreadExited;
            }
            guard.disarm();
            return GlobalResponse::DeliverChildExit(command);
        }
        let (finished, _) = self.finish_resource_response(
            Tid::from_raw(id.tid.as_raw()),
            operation.parent,
            operation.original.clone(),
            Some(id.mm),
            answer,
        );
        match finished {
            SchedulerRpcResult::ThreadExited => {
                operation.retire();
                drop(guard);
                GlobalResponse::ThreadExited
            }
            SchedulerRpcResult::Continue(status) => {
                let finish = { self.sched.lock().unwrap().finish_child_exit_operation(id) };
                if let Err(failure) = finish {
                    failure.terminate();
                }
                guard.disarm();
                GlobalResponse::RequestResources(status)
            }
        }
    }

    pub(super) fn resource_response_time(
        &self,
        tid: DetTid,
        mm: MmId,
        guest_time: LogicalTime,
        response: GlobalResponse,
    ) -> (Option<LogicalTime>, GlobalResponse) {
        if matches!(
            response,
            GlobalResponse::ThreadExited | GlobalResponse::DeliverChildExit(_)
        ) {
            return (None, response);
        }
        let state = self.sched.lock().unwrap();
        if state.thread_is_logically_killed(tid) || !state.rpc_incarnation_matches(tid, mm) {
            return (None, GlobalResponse::ThreadExited);
        }
        let scheduler_time = self.global_time.lock().unwrap().threads_time(tid);
        assert!(
            scheduler_time >= guest_time,
            "child control must not move thread time backward"
        );
        (
            (scheduler_time != guest_time).then_some(scheduler_time),
            response,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::Poll;
    use std::time::Duration;

    use futures::poll;

    use super::*;
    use crate::child_exit::AttemptPhase;
    use crate::child_exit::ControlResult;
    use crate::child_exit::Delivery;
    use crate::child_exit::Disposition;
    use crate::child_exit::NormalExit;
    use crate::child_exit::Outcome;
    use crate::child_exit::Phase;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
    use crate::scheduler::SkipTurn;
    use crate::scheduler::do_a_turn_blocking;

    fn state() -> (Config, GlobalState, DetTid, DetTid, Resources) {
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            backend_uses_virtual_signal_targets: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let child = DetTid::from_raw(18);
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.thread_tree.add_child(parent, parent, true);
            scheduler.thread_tree.add_child(parent, child, true);
            scheduler.next_turns.insert(
                parent,
                ThreadNextTurn {
                    dettid: parent,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                },
            );
            scheduler.priorities.insert(parent, DEFAULT_PRIORITY);
            scheduler.runqueue_push_back(parent);
            scheduler.logically_kill_thread(&child, &child, MmId::initial(child));
        }
        let mut original = Resources::new(parent);
        original.insert(
            ResourceID::WaitChild {
                parent,
                spec: ChildWaitSpec {
                    selector: ChildWaitSelector::Exact(child),
                    exit_class: ChildWaitExitClass::Sigchld,
                    owner: None,
                },
            },
            Permission::RW,
        );
        (config, state, parent, child, original)
    }

    fn due(state: &GlobalState, parent: DetTid, child: DetTid) {
        state.sched.lock().unwrap().child_exits.due.insert(
            1,
            Delivery {
                id: 1,
                child,
                child_mm: MmId::initial(child),
                parent,
                exit: NormalExit {
                    status: 37,
                    uid: 0,
                    user_ticks: 2,
                    system_ticks: 3,
                },
                deadline: LogicalTime::from_nanos(1),
            },
        );
    }

    #[tokio::test]
    async fn delivery_acknowledgement_preserves_wait_request_and_single_commit() {
        for event_first in [false, true] {
            for disposition in [
                Disposition::Ignored,
                Disposition::PendingBlocked,
                Disposition::PendingEligible,
            ] {
                let (config, state, parent, child, original) = state();
                let before_req = state.sched.lock().unwrap().next_turns[&parent].req.clone();
                let original_clock = DetTime::new(&config);
                let request = state.receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        original_clock.clone(),
                        MmId::initial(parent),
                        GlobalRequest::RequestResources(original.clone(), parent),
                    ),
                );
                let mut request = pin!(request);
                let last = Err(SkipTurn);
                let mut turn = pin!(do_a_turn_blocking(
                    state.sched.clone(),
                    state.global_time.clone(),
                    &last
                ));
                if event_first {
                    due(&state, parent, child);
                    assert!(poll!(turn.as_mut()).is_pending());
                }
                assert!(poll!(request.as_mut()).is_pending());
                if !event_first {
                    due(&state, parent, child);
                }
                assert!(poll!(turn.as_mut()).is_pending());
                let Poll::Ready((time, GlobalResponse::DeliverChildExit(command))) =
                    poll!(request.as_mut())
                else {
                    panic!("resource operation did not return the delivery control")
                };
                assert_eq!(time, None);
                assert_eq!(command.delivery.exit.status, 37);
                let operation =
                    state.sched.lock().unwrap().child_exits.operations[&command.operation].clone();
                assert_eq!(operation.original, original);
                let clock_at_control = state.global_time.lock().unwrap().as_nanos();
                for _ in 0..3 {
                    assert!(poll!(turn.as_mut()).is_pending());
                    assert_eq!(
                        state.sched.lock().unwrap().turn,
                        0,
                        "COMMIT crossed an unacknowledged control"
                    );
                    assert_eq!(
                        state.global_time.lock().unwrap().as_nanos(),
                        clock_at_control
                    );
                }
                // The acknowledgement's clock payload is deliberately unrelated. It must
                // neither enter clock accounting nor replace the original request's time.
                let mut poisoned_clock = original_clock;
                for _ in 0..100 {
                    poisoned_clock.add_syscall();
                }
                let acknowledgement = state.receive_rpc(
                    Tid::from_raw(parent.as_raw()),
                    (
                        poisoned_clock,
                        MmId::initial(parent),
                        GlobalRequest::AcknowledgeChildExit {
                            operation: command.operation,
                            delivery_id: command.delivery.id,
                            outcome: Outcome::Accepted {
                                disposition,
                                pending_generation: 9,
                                coalesced: true,
                            },
                        },
                    ),
                );
                let mut acknowledgement = pin!(acknowledgement);
                assert!(poll!(acknowledgement.as_mut()).is_pending());
                assert_eq!(
                    state.sched.lock().unwrap().next_turns[&parent].req,
                    before_req
                );
                assert_eq!(
                    state.global_time.lock().unwrap().as_nanos(),
                    clock_at_control
                );
                let resources = tokio::time::timeout(Duration::from_secs(1), turn)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(resources, original);
                let response = tokio::time::timeout(Duration::from_secs(1), acknowledgement)
                    .await
                    .unwrap();
                assert_eq!(
                    response.1,
                    GlobalResponse::RequestResources(ResumeStatus::Normal)
                );
                let scheduler = state.sched.lock().unwrap();
                assert_eq!(scheduler.turn, 1);
                assert_eq!(scheduler.child_exits.sequences[&parent], 1);
                assert!(scheduler.child_exits.operations.is_empty());
                assert!(scheduler.child_exits.current.is_empty());
                assert!(scheduler.child_exits.due.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn unconsumed_delivery_retirement_settles_without_callback_or_grant() {
        let (config, state, parent, child, original) = state();
        let request = state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(parent),
                GlobalRequest::RequestResources(original, parent),
            ),
        );
        let mut request = pin!(request);
        assert!(poll!(request.as_mut()).is_pending());
        due(&state, parent, child);
        let last = Err(SkipTurn);
        let mut turn = pin!(do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &last
        ));
        assert!(poll!(turn.as_mut()).is_pending());
        let operation = {
            let mut scheduler = state.sched.lock().unwrap();
            let id = scheduler.child_exits.current[&parent];
            let operation = scheduler.child_exits.operations[&id].clone();
            scheduler.logically_kill_thread(&parent, &parent, MmId::initial(parent));
            assert!(scheduler.outstanding_child_exit_completion().is_some());
            operation
        };
        assert_eq!(request.await, (None, GlobalResponse::ThreadExited));
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .outstanding_child_exit_completion()
                .is_none()
        );
        assert!(
            matches!(&*operation.phase.lock().unwrap(), Phase::Delivering(attempt)
            if attempt.phase == AttemptPhase::Settled && matches!(attempt.completion.try_read(), Some(ControlResult::TargetRetired)))
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), turn)
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(state.sched.lock().unwrap().turn, 0);
    }

    #[tokio::test]
    async fn in_flight_retirement_preserves_late_success_and_original_failure() {
        for outcome in [
            Outcome::Accepted {
                disposition: Disposition::PendingBlocked,
                pending_generation: 4,
                coalesced: false,
            },
            Outcome::RejectedBeforeCommit {
                kind: child_exit::ErrorKind::Unsupported,
                errno: libc::ENOSYS,
            },
            Outcome::RejectedBeforeCommit {
                kind: child_exit::ErrorKind::Backend,
                errno: libc::EIO,
            },
            Outcome::FailedAfterCommit {
                errno: libc::EPIPE,
                pending_generation: 5,
            },
        ] {
            let (config, state, parent, child, original) = state();
            let request = state.receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(parent),
                    GlobalRequest::RequestResources(original.clone(), parent),
                ),
            );
            let mut request = pin!(request);
            assert!(poll!(request.as_mut()).is_pending());
            due(&state, parent, child);
            let last = Err(SkipTurn);
            let mut turn = pin!(do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &last
            ));
            assert!(poll!(turn.as_mut()).is_pending());
            let (_, GlobalResponse::DeliverChildExit(command)) = request.await else {
                panic!("missing control")
            };
            let operation = {
                let mut scheduler = state.sched.lock().unwrap();
                let operation = scheduler.child_exits.operations[&command.operation].clone();
                scheduler.logically_kill_thread(&parent, &parent, MmId::initial(parent));
                assert!(scheduler.outstanding_child_exit_completion().is_some());
                operation
            };
            assert!(
                tokio::time::timeout(Duration::from_secs(1), turn)
                    .await
                    .unwrap()
                    .is_err()
            );
            let clock = state.global_time.lock().unwrap().as_nanos();
            if matches!(outcome, Outcome::Accepted { .. }) {
                let mut late_clock = DetTime::new(&config);
                late_clock.add_syscall();
                let response = state
                    .receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            late_clock,
                            MmId::initial(parent),
                            GlobalRequest::AcknowledgeChildExit {
                                operation: command.operation,
                                delivery_id: command.delivery.id,
                                outcome,
                            },
                        ),
                    )
                    .await;
                assert_eq!(response, (None, GlobalResponse::ThreadExited));
            } else {
                // Inspect the real transition before its nonreturning caller terminates.
                // Fatal subprocess controls separately exercise that exact termination path.
                let failure = state
                    .sched
                    .lock()
                    .unwrap()
                    .acknowledge_child_exit(command.operation, command.delivery.id, outcome)
                    .unwrap_err();
                assert_eq!(failure.failure.outcome, Some(outcome));
                assert_eq!(
                    failure.failure.exit_status(),
                    if matches!(
                        outcome,
                        Outcome::RejectedBeforeCommit {
                            kind: child_exit::ErrorKind::Unsupported,
                            ..
                        }
                    ) {
                        122
                    } else {
                        125
                    }
                );
                assert!(
                    matches!(&*operation.phase.lock().unwrap(), Phase::Failed(record)
                    if Arc::ptr_eq(record, &failure))
                );
            }
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
            assert_eq!(state.sched.lock().unwrap().turn, 0);
            assert!(
                state
                    .sched
                    .lock()
                    .unwrap()
                    .outstanding_child_exit_completion()
                    .is_none()
            );
            assert_eq!(operation.original, original);
        }
    }

    #[tokio::test]
    async fn child_exit_negative_acknowledgement_releases_both_waiters_without_grant() {
        for outcome in [
            Outcome::RejectedBeforeCommit {
                kind: child_exit::ErrorKind::Unsupported,
                errno: libc::ENOSYS,
            },
            Outcome::RejectedBeforeCommit {
                kind: child_exit::ErrorKind::Backend,
                errno: libc::EIO,
            },
            Outcome::FailedAfterCommit {
                errno: libc::EPIPE,
                pending_generation: 73,
            },
        ] {
            let (config, state, parent, child, original) = state();
            let request = state.receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(parent),
                    GlobalRequest::RequestResources(original.clone(), parent),
                ),
            );
            let mut request = pin!(request);
            assert!(poll!(request.as_mut()).is_pending());
            due(&state, parent, child);
            let last = Err(SkipTurn);
            let mut turn = pin!(do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &last,
            ));
            assert!(poll!(turn.as_mut()).is_pending());
            let (_, GlobalResponse::DeliverChildExit(command)) = request.await else {
                panic!("missing control")
            };
            let clock = state.global_time.lock().unwrap().as_nanos();
            let mut scheduler = state.sched.lock().unwrap();
            let operation = scheduler.child_exits.operations[&command.operation].clone();
            let (acknowledgement, completion) = {
                let phase = operation.phase.lock().unwrap();
                let Phase::Delivering(attempt) = &*phase else {
                    panic!("the live delivery attempt disappeared")
                };
                assert_eq!(attempt.phase, AttemptPhase::InCallback);
                (attempt.acknowledgement.clone(), attempt.completion.clone())
            };
            assert!(acknowledgement.try_read().is_none());
            assert!(completion.try_read().is_none());
            let original_request = scheduler.next_turns[&parent].req.clone();
            let original_response = scheduler.next_turns[&parent].resp.clone();
            let failure = scheduler
                .acknowledge_child_exit(command.operation, command.delivery.id, outcome)
                .unwrap_err();
            for waiter in [acknowledgement, completion] {
                assert!(
                    matches!(waiter.try_read(), Some(ControlResult::Failed(record))
                    if Arc::ptr_eq(&record, &failure))
                );
            }
            assert_eq!(failure.failure.outcome, Some(outcome));
            assert_eq!(operation.original, original);
            assert!(
                matches!(&*operation.phase.lock().unwrap(), Phase::Failed(record)
                if Arc::ptr_eq(record, &failure))
            );
            assert!(Arc::ptr_eq(
                scheduler.child_exits.fatal.as_ref().unwrap(),
                &failure
            ));
            assert_eq!(scheduler.next_turns[&parent].req, original_request);
            assert_eq!(scheduler.next_turns[&parent].resp, original_response);
            assert!(!scheduler.child_exits.current.contains_key(&parent));
            assert_eq!(scheduler.turn, 0);
            drop(scheduler);
            assert_eq!(state.global_time.lock().unwrap().as_nanos(), clock);
            // Polling the scheduler again would take the already-measured fatal
            // disposition. This control inspects its exact release state first.
        }
    }

    #[test]
    fn child_exit_actual_receive_drop_and_early_negative_rpc_are_bounded() {
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Instant;

        const CASE: &str = "HERMIT_TEST_CHILD_EXIT_TRANSPORT_CASE";
        const TEST: &str = "tool_global::child_exit_control::tests::child_exit_actual_receive_drop_and_early_negative_rpc_are_bounded";
        if let Ok(case) = std::env::var(CASE) {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(async {
                    let (config, state, parent, child, original) = state();
                    let mut request = Box::pin(state.receive_rpc(
                        Tid::from_raw(parent.as_raw()),
                        (
                            DetTime::new(&config),
                            MmId::initial(parent),
                            GlobalRequest::RequestResources(original.clone(), parent),
                        ),
                    ));
                    assert!(poll!(request.as_mut()).is_pending());
                    due(&state, parent, child);
                    let last = Err(SkipTurn);
                    let mut turn = Box::pin(do_a_turn_blocking(
                        state.sched.clone(),
                        state.global_time.clone(),
                        &last,
                    ));
                    assert!(poll!(turn.as_mut()).is_pending());
                    let operation = {
                        let scheduler = state.sched.lock().unwrap();
                        assert_eq!(scheduler.turn, 0);
                        let id = scheduler.child_exits.current[&parent];
                        scheduler.child_exits.operations[&id].clone()
                    };
                    assert_eq!(operation.original, original);
                    assert!(matches!(&*operation.phase.lock().unwrap(),
                        Phase::Delivering(attempt)
                        if attempt.phase == AttemptPhase::NotReturned));

                    if case.starts_with("receive-drop-") {
                        if case == "receive-drop-retired" {
                            let mut scheduler = state.sched.lock().unwrap();
                            scheduler.logically_kill_thread(
                                &parent,
                                &parent,
                                MmId::initial(parent),
                            );
                            assert!(scheduler.outstanding_child_exit_completion().is_some());
                        }
                        // Drop the actual receive_rpc future after control publication.
                        // Its guard is nested inside await_child_exit_resources, and no
                        // scheduler or operation mutex is held at this ownership boundary.
                        drop(request);
                        assert_eq!(case, "receive-drop-retired", "live abandonment returned");
                        assert!(matches!(&*operation.phase.lock().unwrap(),
                            Phase::Delivering(attempt)
                            if attempt.phase == AttemptPhase::Settled
                                && matches!(attempt.completion.try_read(), Some(ControlResult::TargetRetired))));
                        assert!(
                            tokio::time::timeout(Duration::from_secs(1), turn)
                                .await
                                .unwrap()
                                .is_err()
                        );
                        let scheduler = state.sched.lock().unwrap();
                        assert_eq!(scheduler.turn, 0);
                        assert!(scheduler.outstanding_child_exit_completion().is_none());
                        assert_eq!(operation.original, original);
                        eprintln!("CHILD_EXIT_RETIRED_RECEIVE_DROPPED: turn=0 callback=0");
                        return;
                    }

                    let (time, GlobalResponse::DeliverChildExit(command)) = request.await else {
                        panic!("the actual resource receiver did not return its control")
                    };
                    assert_eq!(time, None);
                    assert!(matches!(&*operation.phase.lock().unwrap(),
                        Phase::Delivering(attempt)
                        if attempt.phase == AttemptPhase::InCallback));
                    if case.starts_with("retired-") {
                        {
                            let mut scheduler = state.sched.lock().unwrap();
                            scheduler.logically_kill_thread(
                                &parent,
                                &parent,
                                MmId::initial(parent),
                            );
                            assert!(scheduler.outstanding_child_exit_completion().is_some());
                        }
                        assert!(
                            tokio::time::timeout(Duration::from_secs(1), turn)
                                .await
                                .unwrap()
                                .is_err()
                        );
                    }
                    let outcome = if case.ends_with("unsupported") {
                        Outcome::RejectedBeforeCommit {
                            kind: child_exit::ErrorKind::Unsupported,
                            errno: libc::ENOSYS,
                        }
                    } else if case.ends_with("backend") {
                        Outcome::RejectedBeforeCommit {
                            kind: child_exit::ErrorKind::Backend,
                            errno: libc::EIO,
                        }
                    } else {
                        assert!(case.ends_with("postcommit"));
                        Outcome::FailedAfterCommit {
                            errno: libc::EPIPE,
                            pending_generation: 73,
                        }
                    };
                    assert_eq!(state.sched.lock().unwrap().turn, 0);
                    assert_eq!(operation.original, original);
                    // This invokes the complete early handler, including its real
                    // nonreturning disposition, rather than calling the transition alone.
                    let mut poisoned_clock = DetTime::new(&config);
                    for _ in 0..100 {
                        poisoned_clock.add_syscall();
                    }
                    state
                        .receive_rpc(
                            Tid::from_raw(parent.as_raw()),
                            (
                                poisoned_clock,
                                MmId::initial(parent),
                                GlobalRequest::AcknowledgeChildExit {
                                    operation: command.operation,
                                    delivery_id: command.delivery.id,
                                    outcome,
                                },
                            ),
                        )
                        .await;
                    panic!("negative acknowledgement returned instead of failing the run");
                });
            return;
        }

        for (case, status, errno, stage) in [
            ("receive-drop-live", 125, libc::EPROTO, "ReceiveAbandoned"),
            ("receive-drop-retired", 0, 0, ""),
            ("live-unsupported", 122, libc::ENOSYS, "BeforeCommit"),
            ("retired-unsupported", 122, libc::ENOSYS, "BeforeCommit"),
            ("live-backend", 125, libc::EIO, "BeforeCommit"),
            ("retired-backend", 125, libc::EIO, "BeforeCommit"),
            ("live-postcommit", 125, libc::EPIPE, "AfterCommit"),
            ("retired-postcommit", 125, libc::EPIPE, "AfterCommit"),
        ] {
            let start = Instant::now();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(CASE, case)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let actual = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if start.elapsed() >= Duration::from_secs(2) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{case}: control termination waited for an acknowledgement or hook");
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let output = child.wait_with_output().unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(actual.code(), Some(status), "{case}: {stderr}");
            assert!(start.elapsed() < Duration::from_secs(1), "{case}: {stderr}");
            if status == 0 {
                assert!(stderr.contains("CHILD_EXIT_RETIRED_RECEIVE_DROPPED: turn=0 callback=0"));
                assert!(!stderr.contains("HERMIT_CHILD_EXIT_FAILURE"));
            } else {
                assert!(
                    stderr.contains(&format!("errno={errno} stage={stage}")),
                    "{case}: {stderr}"
                );
                assert!(
                    stderr.contains("operation=1 delivery=1 child=18"),
                    "{case}: {stderr}"
                );
                if case.ends_with("postcommit") {
                    assert!(
                        stderr.contains("pending_generation: 73"),
                        "{case}: {stderr}"
                    );
                }
            }
        }
    }
}
