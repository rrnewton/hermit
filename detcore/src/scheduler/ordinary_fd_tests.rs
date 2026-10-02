/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
*/

// Reuse the maintained scheduler/signal fixture, actual request/grant/parked
// transitions and its backend contract double. No native signal or Read claim.
use super::*;
use crate::network_replay::NetworkStreamOwner;
use crate::scheduler::ordinary_fd::OrdinaryFdResume;

fn post(s: &mut Scheduler, owner: NetworkStreamOwner, request: Resources) -> Ivar<SchedResponse> {
    s.install_resource_origin(
        owner.thread,
        ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm: owner.mm,
            control: ControlCapability::None,
        },
    )
    .unwrap();
    let next = &s.next_turns[&owner.thread];
    let req = next.req.clone();
    let resp = next.resp.clone();
    s.request_put(
        &req,
        request,
        &Arc::new(Mutex::new(GlobalTime::new(&Config::default()))),
    );
    if !s.run_queue.contains_tid(owner.thread) {
        s.runqueue_push_back(owner.thread);
    }
    resp
}

fn grant(s: &mut Scheduler, owner: NetworkStreamOwner) {
    let (tid, request, response) = s.step3_peek().expect("actual selected request");
    assert_eq!(tid, owner.thread);
    let request = request.try_read().unwrap().unwrap();
    assert!(s.step4_resource_block(tid, &request, &response).is_ok());
    assert!(s.step5_guest_unblock(tid, &request, &response).is_ok());
    s.step6_reenquue(tid, false);
}

fn foreground() -> (Scheduler, Arc<Backend>, NetworkStreamOwner) {
    let (mut s, backend) = fixture();
    let (tid, mm, _) = add(&mut s, 100, 100);
    let owner = NetworkStreamOwner { thread: tid, mm };
    let response = post(&mut s, owner, Resources::new(tid));
    grant(&mut s, owner);
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    (s, backend, owner)
}

#[test]
fn ordinary_fd_authority_needs_actual_grant_and_ends_at_real_request() {
    let (mut s, _) = fixture();
    let (tid, mm, _) = add(&mut s, 100, 100);
    let owner = NetworkStreamOwner { thread: tid, mm };
    s.runqueue_push_back(tid);
    s.parked.running = Some(tid); // Deliberate last-grantee-only negative.
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    let response = post(&mut s, owner, Resources::new(tid));
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    let before = (s.turn, s.committed_time);
    grant(&mut s, owner);
    assert_eq!((s.turn, s.committed_time), (before.0 + 1, before.1));
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    let gate = (
        s.next_turns[&tid].req.clone(),
        s.next_turns[&tid].resp.clone(),
    );
    let epoch = s.next_turns[&tid].protocol.epoch;
    let mut without_private_authority = s.next_turns[&tid].protocol.clone();
    without_private_authority.foreground_fd = None;
    assert_eq!(
        format!("{:?}", s.next_turns[&tid].protocol),
        format!("{:?}", without_private_authority)
    );
    for _ in 0..2 {
        let proof = s.ordinary_fd_observation(owner).unwrap();
        assert_eq!(proof.owner(), owner);
        assert_eq!(proof.epoch(), epoch);
        assert_eq!(proof.resume(), OrdinaryFdResume::Normal);
    }
    assert_eq!((s.turn, s.committed_time), (before.0 + 1, before.1));
    assert_eq!(s.next_turns[&tid].req, gate.0);
    assert_eq!(s.next_turns[&tid].resp, gate.1);
    assert_eq!(s.are_all_quiesced(), Some(gate.0.clone()));
    post(&mut s, owner, Resources::new(tid));
    assert_eq!(s.parked.running, Some(tid));
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
}

#[test]
fn ordinary_fd_authority_poll_and_priority_replacements_do_not_grant() {
    for polling in [true, false] {
        let (mut s, _, owner) = foreground();
        let mut request = Resources::new(owner.thread);
        if polling {
            request.insert(ResourceID::InternalIOPolling, Permission::W);
            request.poll_attempt = 1;
            request.fyi("read");
        } else {
            request.insert(
                ResourceID::PriorityChangePoint(DEFAULT_PRIORITY, s.committed_time, 0, vec![]),
                Permission::W,
            );
        }
        let response = post(&mut s, owner, request);
        let (tid, request, _) = s.step3_peek().unwrap();
        let queued = request.try_read().unwrap().unwrap();
        let before = (s.turn, s.committed_time);
        assert!(matches!(
            s.step4_resource_block(tid, &queued, &response),
            Err(SkipTurn)
        ));
        assert_eq!((s.turn, s.committed_time), (before.0 + 1, before.1));
        assert_ne!(s.next_turns[&tid].req, request);
        assert!(response.try_read().is_none());
        assert_eq!(
            s.ordinary_fd_observation(owner).unwrap_err(),
            ProtocolFailure::Phase
        );
        let replacement = s.next_turns[&tid].req.try_read().unwrap().unwrap();
        assert_eq!(replacement.poll_attempt, 0);
        if polling {
            assert_eq!(replacement.fyi, "read");
        } else {
            assert!(replacement.resources.is_empty());
        }
        grant(&mut s, owner);
        assert_eq!(
            s.ordinary_fd_observation(owner).unwrap().resume(),
            OrdinaryFdResume::Normal
        );
        assert_eq!((s.turn, s.committed_time), (before.0 + 2, before.1));
    }
}

#[test]
fn ordinary_fd_authority_external_go_keeps_original_blockers_without_authority() {
    for kind in 0..4 {
        let (mut s, _, owner) = foreground();
        let operation = crate::resources::ExternalOpId::new(owner.thread, 7);
        let resource = match kind {
            0 => ResourceID::BlockingExternalIO(operation),
            1 => ResourceID::BlockingNetworkCapture(operation),
            2 => ResourceID::BlockingVfork(operation),
            _ => ResourceID::BlockingRtSigsuspend(operation),
        };
        let mut request = Resources::new(owner.thread);
        request.insert(resource, Permission::RW);
        let response = post(&mut s, owner, request);
        let (tid, queued, _) = s.step3_peek().unwrap();
        let before = (s.turn, s.committed_time);
        assert!(matches!(
            s.step4_resource_block(tid, &queued.try_read().unwrap().unwrap(), &response),
            Err(SkipTurn)
        ));
        assert_eq!((s.turn, s.committed_time), (before.0 + 1, before.1));
        assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
        assert!(!s.run_queue.contains_tid(tid));
        if kind == 3 {
            assert_eq!(s.blocked.rt_sigsuspend_blockers.get(&tid), Some(&operation));
        } else {
            assert_eq!(s.blocked.external_io_blockers.get(&tid), Some(&operation));
        }
        assert_eq!(
            s.ordinary_fd_observation(owner).unwrap_err(),
            ProtocolFailure::Phase
        );
    }
}

#[test]
fn ordinary_fd_authority_late_request_cannot_override_signal_but_handler_keeps_gate() {
    let (mut s, _, owner) = foreground();
    let mut request = Resources::new(owner.thread);
    request.insert(
        ResourceID::SleepUntil(s.committed_time + at(100)),
        Permission::RW,
    );
    let response = post(&mut s, owner, request);
    let (tid, queued, _) = s.step3_peek().unwrap();
    assert!(matches!(
        s.step4_resource_block(tid, &queued.try_read().unwrap().unwrap(), &response),
        Err(SkipTurn)
    ));
    s.wake_signaled_guest(tid, Signal::SIGUSR1);
    let signal_request = s.next_turns[&tid].req.clone();
    let exact_signal = signal_request.try_read().unwrap().unwrap();
    s.request_put(
        &signal_request,
        Resources::new(tid),
        &Arc::new(Mutex::new(GlobalTime::new(&Config::default()))),
    );
    assert_eq!(signal_request.try_read().unwrap().unwrap(), exact_signal);
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    let before = (s.turn, s.committed_time);
    grant(&mut s, owner);
    assert!(
        matches!(response.try_read(), Some(SchedResponse::Signaled(Some(signals)))
        if signals == vec![SigWrapper::from(Signal::SIGUSR1)])
    );
    let proof = s.ordinary_fd_observation(owner).unwrap();
    assert_eq!(proof.resume(), OrdinaryFdResume::SignalResume);
    assert_eq!(proof.owner(), owner);
    assert_eq!((s.turn, s.committed_time), (before.0 + 1, before.1));
}

#[test]
fn ordinary_fd_authority_actual_caught_handback_and_receipt_preserve_same_gate() {
    let (mut s, backend) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let owner = NetworkStreamOwner { thread: tid, mm };
    let response = sleep(&mut s, tid, mm, site, 100);
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    let observation = selected(&response);
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    let before = (s.turn, s.committed_time);
    let ack = Ivar::new();
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: observation.continuation,
            lease: observation.lease,
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
    assert_eq!(ack.try_read().unwrap(), Ok(FinishAck::Interrupted));
    assert!(matches!(
        s.next_turns[&tid].protocol.owner,
        NextTurnOwner::ReturningCaught { .. }
    ));
    let epoch = s.ordinary_fd_observation(owner).unwrap().epoch();
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap().resume(),
        OrdinaryFdResume::ReturningCaught
    );
    assert_eq!((s.turn, s.committed_time), before);
    s.consume_signal_boundary(SignalBoundaryReceipt {
        permit: observation.permit,
        outcome: SignalBoundaryOutcome::Caught,
    })
    .unwrap();
    assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
    assert_eq!(s.ordinary_fd_observation(owner).unwrap().epoch(), epoch);
    assert_eq!((s.turn, s.committed_time), before);
    post(&mut s, owner, Resources::new(tid));
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
}

#[test]
fn ordinary_fd_authority_resume_same_wait_never_inherits_observation_gate() {
    let (mut s, backend) = fixture();
    let (tid, mm, site) = add(&mut s, 100, 100);
    let owner = NetworkStreamOwner { thread: tid, mm };
    let response = sleep(&mut s, tid, mm, site, 100);
    backend.recipients.lock().unwrap().push(SignalRecipient {
        task: task(100, 100),
    });
    s.committed_time = at(10);
    s.select_parked_alarm().unwrap();
    let observation = selected(&response);
    let ack = Ivar::new();
    let before = (s.turn, s.committed_time);
    s.post_control(
        tid,
        mm,
        ControlIntent::Finish {
            wait: observation.continuation,
            lease: observation.lease,
            site,
            finish: ObservationFinish::ResumeSameWait,
            ack: ack.clone(),
        },
    )
    .unwrap();
    s.drain_control_intents();
    let Ok(FinishAck::AwaitResume(ticket)) = ack.try_read().unwrap() else {
        panic!("resume authority")
    };
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
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
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    assert_eq!(s.blocked.timed_waiters.thread_deadline(tid), Some(at(100)));
    assert!(resumed.try_read().is_none());
    assert_eq!((s.turn, s.committed_time), before);
}

#[test]
fn ordinary_fd_authority_exact_same_thread_exec_handback_preserves_gate() {
    let (mut s, _, before) = foreground();
    let pid = before.thread;
    let after = NetworkStreamOwner {
        thread: pid,
        mm: before.mm.for_exec(pid),
    };
    assert_eq!(
        s.ordinary_fd_observation(after).unwrap_err(),
        ProtocolFailure::Identity
    );
    let unchanged = (
        s.turn,
        s.committed_time,
        s.next_turns[&pid].req.clone(),
        s.next_turns[&pid].resp.clone(),
    );
    // Failed exec never reaches the authenticated successful handback.
    assert_eq!(s.ordinary_fd_observation(before).unwrap().owner(), before);
    s.reconnect_after_exec(ExecReconnect {
        caller: pid,
        new_leader: pid,
        detpid: pid,
        pre_exec_mm: before.mm,
        post_exec_mm: after.mm,
        child_tid_addr: 0,
        reconnect_priority: None,
    });
    assert_eq!(
        s.ordinary_fd_observation(before).unwrap_err(),
        ProtocolFailure::Identity
    );
    assert_eq!(
        s.ordinary_fd_observation(after).unwrap_err(),
        ProtocolFailure::Identity
    );
    s.rebind_ordinary_fd_exec(before, after);
    assert_eq!(s.ordinary_fd_observation(after).unwrap().owner(), after);
    assert_eq!(
        (
            s.turn,
            s.committed_time,
            s.next_turns[&pid].req.clone(),
            s.next_turns[&pid].resp.clone()
        ),
        unchanged
    );
    assert_eq!(
        s.ordinary_fd_observation(before).unwrap_err(),
        ProtocolFailure::Identity
    );
    s.logically_kill_thread(&pid, &pid, after.mm);
    assert_eq!(
        s.ordinary_fd_observation(after).unwrap_err(),
        ProtocolFailure::Identity
    );
}

#[test]
fn ordinary_fd_authority_new_registration_and_wrong_exec_cannot_inherit() {
    let (mut s, _, owner) = foreground();
    let (peer, peer_mm, _) = add(&mut s, 100, 101);
    let peer_owner = NetworkStreamOwner {
        thread: peer,
        mm: peer_mm,
    };
    s.runqueue_push_back(peer);
    s.rebind_ordinary_fd_exec(owner, peer_owner);
    assert_eq!(
        s.ordinary_fd_observation(peer_owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    assert_eq!(s.ordinary_fd_observation(owner).unwrap().owner(), owner);
    // A response withdrawal/replacement cannot reuse a previous grant either.
    s.next_turns.get_mut(&owner.thread).unwrap().resp = Ivar::new();
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
}

#[test]
fn ordinary_fd_authority_nonleader_exec_replacement_does_not_inherit_old_gate() {
    let (mut s, _, leader) = foreground();
    let mut sleeping = Resources::new(leader.thread);
    sleeping.insert(
        ResourceID::SleepUntil(s.committed_time + at(100)),
        Permission::RW,
    );
    let response = post(&mut s, leader, sleeping);
    let (tid, request, _) = s.step3_peek().unwrap();
    assert!(matches!(
        s.step4_resource_block(tid, &request.try_read().unwrap().unwrap(), &response),
        Err(SkipTurn)
    ));
    let (worker, mm, _) = add(&mut s, 100, 101);
    let worker_owner = NetworkStreamOwner { thread: worker, mm };
    post(&mut s, worker_owner, Resources::new(worker));
    grant(&mut s, worker_owner);
    assert_eq!(
        s.ordinary_fd_observation(worker_owner).unwrap().owner(),
        worker_owner
    );
    let before = (s.turn, s.committed_time);
    let after = NetworkStreamOwner {
        thread: leader.thread,
        mm: mm.for_exec(leader.thread),
    };
    s.reconnect_after_exec(ExecReconnect {
        caller: worker,
        new_leader: leader.thread,
        detpid: leader.thread,
        pre_exec_mm: mm,
        post_exec_mm: after.mm,
        child_tid_addr: 0,
        reconnect_priority: Some(DEFAULT_PRIORITY),
    });
    s.rebind_ordinary_fd_exec(worker_owner, after);
    assert_eq!(
        s.ordinary_fd_observation(after).unwrap_err(),
        ProtocolFailure::Phase
    );
    assert_eq!(
        s.ordinary_fd_observation(worker_owner).unwrap_err(),
        ProtocolFailure::Identity
    );
    assert_eq!((s.turn, s.committed_time), before);
}

#[test]
fn ordinary_fd_authority_clear_without_grant_and_terminal_failure_refuse() {
    let (mut s, _, owner) = foreground();
    let before = (s.turn, s.committed_time);
    s.clear_nextturn(owner.thread).unwrap();
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    assert_eq!((s.turn, s.committed_time), before);
    post(&mut s, owner, Resources::new(owner.thread));
    grant(&mut s, owner);
    assert!(s.ordinary_fd_observation(owner).is_ok());
    let before = (s.turn, s.committed_time);
    s.report_backend_failure_location(BackendFailureLocation {
        pid: reverie::Pid::from_raw(100),
        tid: Some(reverie::Tid::from_raw(100)),
        phase: "ordinary-fd-authority-test",
    });
    assert_eq!(
        s.ordinary_fd_observation(owner).unwrap_err(),
        ProtocolFailure::Phase
    );
    assert_eq!((s.turn, s.committed_time), before);
}

#[test]
fn foreground_epoll_requires_actual_normal_grant_and_unchanged_initial_projection() {
    let raw = std::process::id() as i32;
    let (root, _metadata, _memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (mut s, _) = fixture();
    add_with_mm(&mut s, raw, raw, owner.mm);
    s.register_physical_thread(owner.thread, owner.mm, raw, raw)
        .unwrap();
    let pin = s.physical_thread_pidfds[&owner.thread]
        .3
        .try_clone()
        .unwrap();
    s.admit_initial_native_root(root.association(), &pin, None, |_| Ok(()))
        .unwrap();
    assert!(s.foreground_epoll_observation(owner, &root).is_err());
    let response = post(&mut s, owner, Resources::new(owner.thread));
    grant(&mut s, owner);
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    let epoch = s
        .foreground_epoll_observation(owner, &root)
        .unwrap()
        .epoch();
    let before = (s.turn, s.committed_time);
    assert_eq!(
        s.foreground_epoll_observation(owner, &root)
            .unwrap()
            .epoch(),
        epoch
    );
    assert_eq!((s.turn, s.committed_time), before);
    s.record_ordinary_fd_grant(owner, OrdinaryFdResume::SignalResume);
    assert!(s.foreground_epoll_observation(owner, &root).is_err());
    s.record_ordinary_fd_grant(owner, OrdinaryFdResume::Normal);
    s.thread_tree
        .process_wait
        .get_mut(&owner.thread)
        .unwrap()
        .birth_sequences
        .insert((owner.thread, owner.mm), 1);
    // Birth history disqualifies V4's sole-root policy, not the unchanged
    // generic task projection. The separate projection controls cover epoll.
    assert!(s.foreground_native_observation(owner, &root).is_err());
    s.thread_tree
        .process_wait
        .get_mut(&owner.thread)
        .unwrap()
        .birth_sequences
        .clear();
    post(&mut s, owner, Resources::new(owner.thread));
    assert!(s.foreground_epoll_observation(owner, &root).is_err());
}

#[test]
fn foreground_epoll_refuses_missing_or_changed_current_task_projection() {
    use crate::network_runtime::native_birth_outcome::NativeTaskProjection;

    // These are controlled census mutations, not observed kernel births. Each
    // case retains the actual grant/registration and changes only its projection.
    for field in ["missing", "provider", "task", "start"] {
        let (mut s, root, _metadata, _memory) = native_capture_entry_fixture();
        let owner = root.owner();
        let response = post(&mut s, owner, Resources::new(owner.thread));
        grant(&mut s, owner);
        assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
        let epoch = s
            .foreground_epoll_observation(owner, &root)
            .unwrap()
            .epoch();
        assert!(s.foreground_native_observation(owner, &root).is_ok());
        let before = (s.turn, s.committed_time);
        let saved = s
            .thread_tree
            .process_wait
            .get_mut(&owner.thread)
            .unwrap()
            .native_projections
            .pop()
            .unwrap();
        assert!(
            s.thread_tree.process_wait[&owner.thread]
                .native_projections
                .is_empty()
        );
        if field != "missing" {
            let changed = crate::network_runtime::changed_initial_root_fixture(
                root.association(),
                field,
            );
            let registration = crate::scheduler::InitialRootRegistration {
                owner,
                process: s.registered_process(owner.thread).unwrap(),
                raw_process: root.process(),
                _pin: &s.physical_thread_pidfds[&owner.thread].3,
            };
            let changed =
                NativeTaskProjection::from_initial_root(&changed, &registration).unwrap();
            assert_eq!(changed.thread(), owner.thread);
            assert!(!changed.matches_foreground_identity(owner, root.native_identity()));
            s.thread_tree
                .process_wait
                .get_mut(&owner.thread)
                .unwrap()
                .native_projections
                .push(changed);
        }
        assert_eq!(s.ordinary_fd_observation(owner).unwrap().epoch(), epoch);
        assert_eq!(
            s.foreground_epoll_observation(owner, &root)
                .unwrap_err()
                .to_string(),
            "native observation lacks an unchanged task projection",
            "projection {field}",
        );
        assert_eq!((s.turn, s.committed_time), before);
        let projections = &mut s
            .thread_tree
            .process_wait
            .get_mut(&owner.thread)
            .unwrap()
            .native_projections;
        projections.clear();
        projections.push(saved);
        assert_eq!(
            s.foreground_epoll_observation(owner, &root)
                .unwrap()
                .epoch(),
            epoch
        );
        assert!(s.foreground_native_observation(owner, &root).is_ok());
        assert_eq!((s.turn, s.committed_time), before);
    }
}

#[test]
fn foreground_epoll_birth_bookkeeping_never_lends_v4_sole_root_authority() {
    let (mut s, root, _metadata, _memory) = native_capture_entry_fixture();
    let owner = root.owner();
    let response = post(&mut s, owner, Resources::new(owner.thread));
    grant(&mut s, owner);
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    let epoch = s
        .foreground_epoll_observation(owner, &root)
        .unwrap()
        .epoch();
    assert!(
        s.foreground_native_observation(owner, &root)
            .unwrap()
            .admits_sole_initial_root(&root)
    );
    let before = (s.turn, s.committed_time);
    s.thread_tree
        .process_wait
        .get_mut(&owner.thread)
        .unwrap()
        .birth_sequences
        .insert((owner.thread, owner.mm), 1);
    let generic = s.foreground_epoll_observation(owner, &root).unwrap();
    assert_eq!(generic.epoch(), epoch);
    assert!(!generic.admits_sole_initial_root(&root));
    assert_eq!(
        s.foreground_native_observation(owner, &root)
            .unwrap_err()
            .to_string(),
        "native observation lacks unchanged sole initial root",
    );
    assert_eq!((s.turn, s.committed_time), before);
}

type NativeCaptureFixture = (
    Scheduler,
    Arc<crate::network_runtime::ForegroundRoot>,
    Arc<Mutex<crate::tool_local::FileMetadata>>,
    Arc<Mutex<crate::memory::MemoryMetadata>>,
);

fn native_capture_entry_fixture() -> NativeCaptureFixture {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, metadata, memory, _) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let (mut s, _) = fixture();
    add_with_mm(&mut s, raw, raw, owner.mm);
    s.register_physical_thread(owner.thread, owner.mm, raw, raw)
        .unwrap();
    let pin = s.physical_thread_pidfds[&owner.thread]
        .3
        .try_clone()
        .unwrap();
    s.admit_initial_native_root(root.association(), &pin, None, |_| Ok(()))
        .unwrap();
    (s, root, metadata, memory)
}
fn native_capture_entry_grant(
    s: &mut Scheduler,
    root: &crate::network_runtime::ForegroundRoot,
    network: bool,
) -> ExternalOpId {
    let owner = root.owner();
    let operation = ExternalOpId::new(owner.thread, 71);
    let mut resources = Resources::new(owner.thread);
    resources.insert(
        if network {
            ResourceID::BlockingNetworkCapture(operation)
        } else {
            ResourceID::BlockingExternalIO(operation)
        },
        Permission::RW,
    );
    let response = post(s, owner, resources);
    assert!(
        s.native_capture_entry_observation(owner, operation, root)
            .is_err(),
        "queued request is not a selected grant"
    );
    let (tid, req, _) = s.step3_peek().unwrap();
    let selected = req.try_read().unwrap().unwrap();
    assert!(
        s.native_capture_entry_observation(owner, operation, root)
            .is_err(),
        "tentative selection is not a grant"
    );
    assert!(matches!(
        s.step4_resource_block(tid, &selected, &response),
        Err(SkipTurn)
    ));
    assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
    operation
}
#[test]
fn native_capture_entry_requires_actual_network_grant_and_exact_operation() {
    let (mut s, root, _metadata, _memory) = native_capture_entry_fixture();
    let owner = root.owner();
    let operation = native_capture_entry_grant(&mut s, &root, true);
    let before = (s.turn, s.committed_time);
    let proof = s
        .native_capture_entry_observation(owner, operation, &root)
        .unwrap();
    assert_eq!(proof.owner(), owner);
    assert_eq!(proof.operation(), operation);
    assert!(
        s.foreground_native_observation(owner, &root).is_err(),
        "external grant is not foreground authority"
    );
    assert!(
        s.native_capture_entry_observation(
            owner,
            ExternalOpId::new(owner.thread, operation.sequence + 1),
            &root
        )
        .is_err()
    );
    assert_eq!((s.turn, s.committed_time), before);
    let (mut ordinary, root, _metadata, _memory) = native_capture_entry_fixture();
    let operation = native_capture_entry_grant(&mut ordinary, &root, false);
    assert!(ordinary.original_external_io_grant_matches(root.owner(), operation));
    assert!(
        ordinary
            .native_capture_entry_observation(root.owner(), operation, &root)
            .is_err()
    );
}
#[test]
fn native_capture_entry_refuses_cancellation_kill_exec_and_changed_root() {
    for variant in 0..5 {
        let (mut s, root, _metadata, _memory) = native_capture_entry_fixture();
        let owner = root.owner();
        let operation = native_capture_entry_grant(&mut s, &root, true);
        assert!(
            s.native_capture_entry_observation(owner, operation, &root)
                .is_ok()
        );
        match variant {
            0 => {
                post(&mut s, owner, Resources::new(owner.thread));
            }
            1 => s.logically_kill_thread(&owner.thread, &owner.thread, owner.mm),
            2 => {
                let after = owner.mm.for_exec(owner.thread);
                s.reconnect_after_exec(ExecReconnect {
                    caller: owner.thread,
                    new_leader: owner.thread,
                    detpid: owner.thread,
                    pre_exec_mm: owner.mm,
                    post_exec_mm: after,
                    child_tid_addr: 0,
                    reconnect_priority: None,
                });
            }
            3 => {
                s.thread_tree
                    .process_wait
                    .get_mut(&owner.thread)
                    .unwrap()
                    .birth_sequences
                    .insert((owner.thread, owner.mm), 1);
            }
            4 => {
                s.report_backend_failure_location(BackendFailureLocation {
                    pid: reverie::Pid::from_raw(100),
                    tid: Some(reverie::Tid::from_raw(100)),
                    phase: "native-entry-refusal",
                });
            }
            _ => unreachable!(),
        }
        assert!(
            s.native_capture_entry_observation(owner, operation, &root)
                .is_err(),
            "variant {variant}"
        );
    }
}
