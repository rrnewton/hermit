//! The initial census, provider replies and PIDFD stand-ins are controlled
//! premises. The original Tool observer, retained birth and state-ready paths
//! are real; this is not native clone/Sendto qualification.
use std::cell::RefCell;

use reverie::Tool;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::*;
use crate::network_replay::NetworkFdMutationBegin;
use crate::network_replay::NetworkFdMutationKind;
use crate::network_runtime::ForegroundRoot;

fn flags() -> CloneFlags {
    CloneFlags::CLONE_VM
        | CloneFlags::CLONE_FILES
        | CloneFlags::CLONE_SIGHAND
        | CloneFlags::CLONE_THREAD
}
fn args(nr: Sysno) -> SyscallArgs {
    if nr == Sysno::clone3 {
        return SyscallArgs {
            arg0: 0x7000,
            arg1: 88,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        };
    }
    SyscallArgs {
        arg0: flags().bits() as usize,
        arg1: 0x8000,
        arg2: 0,
        arg3: 0,
        arg4: 0,
        arg5: 0,
    }
}
fn entry(nr: Sysno) -> (i32, [usize; 6]) {
    let a = args(nr);
    (nr as i32, [a.arg0, a.arg1, a.arg2, a.arg3, a.arg4, a.arg5])
}

async fn observer_case(nr: Sysno, mutation: Option<u8>) {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let tid = Tid::from_raw(raw);
    let mut cfg = crate::config::Config {
        sequentialize_threads: true,
        epoch_explicit: true,
        epoch: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
        ..Default::default()
    };
    cfg.network_trace.policy = detcore_model::network_trace::NetworkPolicy::Record;
    let state = RefCell::new(GlobalState::initialize(&cfg, false));
    *state
        .borrow()
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap() = if mutation == Some(8) {
        NetworkReplayEngine::record_native_receive(cfg.epoch)
    } else {
        NetworkReplayEngine::record_shared_mm_attempts(cfg.epoch)
    };
    let tool: crate::Detcore = crate::Detcore::new(tid, &cfg);
    let thread = RefCell::new(tool.init_thread_state(tid, None));
    let requested_flags = if mutation == Some(5) {
        flags() & !CloneFlags::CLONE_VM
    } else {
        flags()
    };
    let f = ForegroundRoot::controlled_shared_birth_with_observer(
        raw,
        (mutation != Some(9)).then_some(nr as i32),
        |root, claim| {
            let state = state.borrow();
            let owner = root.owner();
            state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(root);
            state
                .registered_exec_mms
                .lock()
                .unwrap()
                .insert(owner.thread, owner.mm);
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine.fd_table_fixture_enable();
            engine
                .register_initial_census(root.association(), claim, owner.thread)
                .unwrap();
            let NetworkFdMutationBegin::Admitted(admission) = engine
                .begin_fd_mutation(
                    owner,
                    root.files(),
                    NetworkFdMutationKind::Clone {
                        flags: requested_flags,
                    },
                )
                .unwrap()
            else {
                panic!("actual clone reservation missing")
            };
            let permit = admission.publication.permit;
            engine.submit_fd_mutation(owner, permit).unwrap();
            engine.prepare_native_birth_escrow(owner, permit).unwrap();
            drop(engine);
            let mut scheduler = state.sched.lock().unwrap();
            let prepared = scheduler
                .thread_tree
                .prepare_no_seq_birth(
                    owner,
                    root.logical_process(),
                    crate::resources::ExternalOpId::new(owner.thread, 157),
                    (requested_flags, 0, 0),
                    None,
                    Some(permit),
                )
                .unwrap();
            let submitted = scheduler
                .thread_tree
                .submit_no_seq_birth(&prepared)
                .unwrap();
            let mut thread = thread.borrow_mut();
            thread.dettid = owner.thread;
            thread.detpid = Some(root.logical_process());
            thread.mm_id = owner.mm;
            thread.thread_start_entered = true;
            thread.file_metadata = root.metadata().unwrap();
            thread.memory_metadata = root.memory().unwrap();
            thread.pending_no_seq_birth = Some(submitted);
            thread.pending_fd_clone = Some(permit);
            thread.clone_flags = Some(requested_flags);
            thread.stats.syscall_count = 157;
            thread.native_birth_required = true;
            Some(permit)
        },
        |runtime, root, permit| {
            let mut state = state.borrow_mut();
            state.network_runtime = Some(runtime);
            if mutation == Some(9) {
                assert!(
                    state.retain_shared_birth_entry(permit, entry(nr)).is_err(),
                    "missing actual provider preparation must refuse"
                );
            } else if mutation != Some(0) {
                state.retain_shared_birth_entry(permit, entry(nr)).unwrap();
            }
            let mut thread = thread.borrow_mut();
            // A callback cannot supply its own expected tuple. Failed validation
            // must not consume the exact retained entry used by the real callback.
            if mutation.is_none() {
                let mut changed = args(nr);
                changed.arg1 += 16;
                assert!(
                    state
                        .observe_shared_birth_entry(
                            tid,
                            root.logical_process(),
                            &thread,
                            nr,
                            changed
                        )
                        .is_err()
                );
                assert!(root.is_sole_initial_root(root.owner()));
            }
            let mmap = SyscallArgs {
                arg0: 0,
                arg1: 4096,
                arg2: 3,
                arg3: 0x22,
                arg4: usize::MAX,
                arg5: 0,
            };
            {
                let mut memory = thread.memory_metadata.lock().unwrap();
                memory
                    .observe_original_arena(root, Sysno::mmap, mmap, Event::Prepared)
                    .unwrap();
                memory
                    .observe_original_arena(root, Sysno::mmap, mmap, Event::Returned(0x10000))
                    .unwrap();
                assert!(memory.original_event_arena(root.owner(), 0x10000).is_ok());
            }
            if let Some(case) = mutation {
                let mut observed = args(nr);
                let mut number = nr;
                match case {
                    0 | 5 | 8 | 9 => {}
                    1 => observed.arg1 += 16,
                    2 => state
                        .sched
                        .lock()
                        .unwrap()
                        .controlled_shared_foreground_grant(root),
                    3 => thread.pending_no_seq_birth = None,
                    4 => thread.mm_id = thread.mm_id.for_exec(root.logical_process()),
                    6 | 10 | 11 => tool.on_injected_syscall_observed(
                        tid,
                        &state,
                        &mut thread,
                        nr,
                        args(nr),
                        Event::Prepared,
                    ),
                    7 => number = Sysno::execve,
                    _ => unreachable!(),
                }
                if case == 10 || case == 11 {
                    let event = if case == 10 {
                        Event::InterruptedBeforeEntry
                    } else {
                        Event::Returned(-i64::from(libc::EINTR))
                    };
                    tool.on_injected_syscall_observed(
                        tid,
                        &state,
                        &mut thread,
                        nr,
                        args(nr),
                        event,
                    );
                    assert!(!state.sched.lock().unwrap().backend_failed());
                    assert_eq!(
                        state
                            .sched
                            .lock()
                            .unwrap()
                            .thread_tree
                            .pending_no_seq_birth_count(),
                        1
                    );
                    assert!(!root.is_sole_initial_root(root.owner()));
                    let scheduler = state.sched.lock().unwrap();
                    assert!(
                        state
                            .network_runtime
                            .as_ref()
                            .unwrap()
                            .with_shared_foreground_lineage(root.owner(), |lineage| {
                                scheduler
                                    .shared_mm_foreground_observation(root.owner(), lineage)
                                    .map(|_| ())
                            })
                            .is_err()
                    );
                } else {
                    tool.on_injected_syscall_observed(
                        tid,
                        &state,
                        &mut thread,
                        number,
                        observed,
                        Event::Prepared,
                    );
                    assert!(
                        !root.is_current(root.owner()),
                        "case {case} preserved an invalid parent"
                    );
                    assert!(
                        !root.has_shared_mm_history(),
                        "case {case} failed sticky revocation"
                    );
                }
                assert!(
                    thread
                        .memory_metadata
                        .lock()
                        .unwrap()
                        .original_event_arena(root.owner(), 0x10000)
                        .is_err()
                );
                return None; // No ObserveNativeBirth or state-ready success is claimed.
            }
            tool.on_injected_syscall_observed(
                tid,
                &state,
                &mut thread,
                nr,
                args(nr),
                Event::Prepared,
            );
            assert!(
                root.is_current(root.owner()),
                "real Prepared revoked admitted shared parent"
            );
            assert!(root.has_shared_mm_history());
            assert!(!root.is_sole_initial_root(root.owner()));
            assert!(
                thread
                    .memory_metadata
                    .lock()
                    .unwrap()
                    .original_event_arena(root.owner(), 0x10000)
                    .is_err()
            );
            assert!(
                state
                    .observe_shared_birth_entry(tid, root.logical_process(), &thread, nr, args(nr))
                    .is_err(),
                "duplicate Prepared reused entry"
            );
            let scheduler = state.sched.lock().unwrap();
            assert!(
                state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .with_shared_foreground_lineage(root.owner(), |lineage| {
                        scheduler
                            .shared_mm_foreground_observation(root.owner(), lineage)
                            .map(|_| ())
                    })
                    .is_err(),
                "unresolved birth must still block shared operations"
            );
            drop(scheduler);
            Some(state.network_runtime.take().unwrap())
        },
    )
    .await;
    if mutation.is_some() {
        assert!(f.is_none());
        return;
    }
    let f = f.unwrap();
    let parent = f.parent.clone();
    let child = f.child.clone();
    let admission = f._birth.clone();
    let (runtime, _retained) = f.into_runtime_and_retention();
    let mut state = state.into_inner();
    state.network_runtime = Some(runtime);
    let mut parent_thread = thread.into_inner();
    let submitted = parent_thread.pending_no_seq_birth.as_mut().unwrap();
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .rebind_native_birth(submitted)
        .unwrap();
    let outcome = submitted
        .native_owner
        .as_ref()
        .unwrap()
        .attach(admission.clone())
        .unwrap();
    parent_thread.native_child_outcome = Some(outcome);
    tool.on_injected_syscall_observed(
        tid,
        &state,
        &mut parent_thread,
        nr,
        args(nr),
        Event::ChildCreated(Tid::from_raw(child.owner().thread.as_raw())),
    );
    assert!(!state.sched.lock().unwrap().backend_failed());
    let mut child_thread = tool.init_thread_state(
        Tid::from_raw(child.owner().thread.as_raw()),
        Some((tid, &parent_thread)),
    );
    child_thread.detpid = Some(parent.logical_process());
    child_thread.thread_start_entered = true;
    {
        let inherited = child_thread.pending_no_seq_birth.as_ref().unwrap();
        let mut scheduler = state.sched.lock().unwrap();
        assert!(
            scheduler
                .thread_tree
                .consume_no_seq_birth(inherited, child.owner().thread)
        );
        scheduler.controlled_shared_birth_ready_projection(parent.owner(), &child);
    }
    {
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        engine.admit_native_birth(&admission).unwrap();
        engine
            .register_inherited_cloned_fd_table(
                admission.permit(),
                child.owner(),
                parent.logical_process(),
                flags(),
            )
            .unwrap();
    }
    state
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(child.owner().thread, child.owner().mm);
    assert!(
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .fd_metadata(child.owner(), child.files())
            .is_err(),
        "inherited physical lineage must not mark the child reader ready"
    );
    tool.on_injected_syscall_observed(
        tid,
        &state,
        &mut parent_thread,
        nr,
        args(nr),
        Event::ChildSyscallReturned {
            child: Tid::from_raw(child.owner().thread.as_raw()),
            raw: i64::from(child.owner().thread.as_raw()),
        },
    );
    assert!(!state.sched.lock().unwrap().backend_failed());
    assert_eq!(
        state.sched.lock().unwrap().thread_tree.join_no_seq_birth(
            parent_thread.pending_no_seq_birth.as_ref().unwrap(),
            child.owner().thread
        ),
        Some(true)
    );
    let scheduler = state.sched.lock().unwrap();
    state
        .network_runtime
        .as_ref()
        .unwrap()
        .with_shared_foreground_lineage(parent.owner(), |lineage| {
            assert_eq!(lineage.members().count(), 2);
            scheduler
                .shared_mm_foreground_observation(parent.owner(), lineage)
                .map(|_| ())
        })
        .unwrap();
    drop(scheduler);
    // The parent's complete physical census above precedes the actual child
    // state-ready callback, exactly as with a parent-first scheduler choice.
    tool.on_thread_state_ready(
        Tid::from_raw(child.owner().thread.as_raw()),
        &state,
        &child_thread,
    )
    .unwrap();
    assert!(
        state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .fd_metadata(child.owner(), child.files())
            .is_ok(),
        "only the actual state-ready callback grants child reader permission"
    );
    assert!(!parent.is_sole_initial_root(parent.owner()));
    // An unbound second invocation remains a sticky full revocation.
    tool.on_injected_syscall_observed(
        tid,
        &state,
        &mut parent_thread,
        Sysno::clone,
        args(nr),
        Event::Prepared,
    );
    assert!(!parent.has_shared_mm_history());
    assert!(!child.is_current(child.owner()));
}

#[tokio::test]
async fn shared_birth_actual_prepared_observer_clone() {
    observer_case(Sysno::clone, None).await;
}
#[tokio::test]
async fn shared_birth_actual_prepared_observer_clone3() {
    observer_case(Sysno::clone3, None).await;
}
#[tokio::test]
async fn shared_birth_actual_tool_refusals_and_unsettled_observations() {
    for case in 0..12 {
        observer_case(Sysno::clone3, Some(case)).await;
    }
}
