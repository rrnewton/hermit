/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native tests of the actual Detcore consumer. No synthetic backend event.
//! Select individually through the reviewed ordinary native-test command owner.
use std::cell::RefCell;
use std::time::Duration;
use std::time::Instant;

use reverie::ExitStatus;
use reverie::InjectedSyscallEvent;
use reverie_ptrace::testing::NewbornExitStop;
use reverie_ptrace::testing::kill_newborn_process_at_exit_stop;
use reverie_ptrace::testing::test_fn_with_config;

use super::*;
use crate::config::RunsPostFork;
use crate::tool_local::ThreadState;

#[derive(Default)]
struct Observed {
    parent: Option<DetTid>,
    child: Option<DetTid>,
    stop: Option<NewbornExitStop>,
    native_births: usize,
    terminal_before_start: usize,
    settlements: usize,
    consuming_exits: usize,
}

thread_local! {
    // The backend's LocalSet executes each Tool callback on this same tracer
    // thread. No other test or guest run is enrolled by a process-wide flag.
    static ACTIVE: RefCell<Option<Arc<Mutex<Observed>>>> = const { RefCell::new(None) };
}

struct Active;
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}
fn active() -> Option<Arc<Mutex<Observed>>> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

pub(crate) fn native_birth<T>(
    tid: Tid,
    global: &GlobalState,
    state: &ThreadState<T>,
    nr: Sysno,
    event: InjectedSyscallEvent,
) {
    let Some(observed) = active() else {
        return;
    };
    let InjectedSyscallEvent::ChildCreated(child) = event else {
        return;
    };
    assert_eq!(nr, Sysno::clone);
    assert!(!global.cfg.sequentialize_threads);
    let birth = state
        .pending_no_seq_birth
        .as_ref()
        .expect("real clone wrapper submitted birth");
    assert_eq!(
        birth.child(),
        None,
        "parent form is bound by actual child Tool inheritance later"
    );
    assert!(!global.sched.lock().unwrap().backend_failed());
    assert_eq!(
        global
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .pending_no_seq_birth_count(),
        1
    );
    assert_eq!(state.dettid.as_raw(), tid.as_raw());
    assert!(
        birth.fd_permit().is_none(),
        "this fixture does not enable unqualified provider custody"
    );
    {
        let mut log = observed.lock().unwrap();
        assert_eq!(log.native_births, 0);
        log.native_births += 1;
        log.parent = Some(state.dettid);
        log.child = Some(DetTid::from_raw(child.as_raw()));
    }
    // This is the actual synchronous callback, after native ChildCreated was
    // retained by Detcore and before backend child dispatch. The creator's
    // NewChild stop is still owned. No PID lookup from guest data is used.
    let stop = unsafe { kill_newborn_process_at_exit_stop(child, Duration::from_secs(1)) }
        .expect("real held newborn must reach its notifier EXIT stop");
    assert_eq!(stop.child(), child);
    observed.lock().unwrap().stop = Some(stop);
}

pub(crate) struct BeforeTerminal {
    clocks: serde_json::Value,
    committed: LogicalTime,
}

fn assert_never_admitted(global: &GlobalState, child: DetTid) {
    let sched = global.sched.lock().unwrap();
    assert!(!sched.backend_failed());
    assert!(!sched.thread_was_registered(child));
    assert!(!sched.next_turns.contains_key(&child));
    assert!(!sched.priorities.contains_key(&child));
    assert!(!sched.run_queue.contains_tid(child));
    assert!(!global.global_time.lock().unwrap().contains_thread(child));
    assert!(
        !global
            .registered_exec_mms
            .lock()
            .unwrap()
            .contains_key(&child)
    );
}

pub(crate) fn before_terminal<T>(
    tid: Tid,
    global: &GlobalState,
    state: &ThreadState<T>,
    status: ExitStatus,
) -> Option<BeforeTerminal> {
    let observed = active()?;
    let mut log = observed.lock().unwrap();
    if log.child != Some(state.dettid) {
        return None;
    }
    assert_eq!(tid.as_raw(), state.dettid.as_raw());
    assert_eq!(
        status,
        ExitStatus::Signaled(reverie::Signal::SIGKILL, false)
    );
    assert!(
        !state.thread_start_entered,
        "positive prestart census is mandatory"
    );
    let birth = state
        .pending_no_seq_birth
        .as_ref()
        .expect("actual backend inherited this birth");
    assert_eq!(birth.child(), Some(state.dettid));
    assert_eq!(birth.parent().thread, log.parent.unwrap());
    assert_eq!(state.pending_fd_clone, birth.fd_permit());
    assert_eq!(log.terminal_before_start, 0);
    assert!(
        log.stop.is_some(),
        "actual EXIT-stop observation preceded callback"
    );
    assert_never_admitted(global, state.dettid);
    log.terminal_before_start += 1;
    Some(BeforeTerminal {
        clocks: serde_json::to_value(&*global.global_time.lock().unwrap()).unwrap(),
        committed: global.sched.lock().unwrap().committed_time,
    })
}

pub(crate) fn after_terminal<T>(
    tid: Tid,
    global: &GlobalState,
    state: &ThreadState<T>,
    before: Option<BeforeTerminal>,
) {
    let Some(before) = before else {
        return;
    };
    assert_eq!(tid.as_raw(), state.dettid.as_raw());
    assert_never_admitted(global, state.dettid);
    assert_eq!(
        serde_json::to_value(&*global.global_time.lock().unwrap()).unwrap(),
        before.clocks
    );
    let mut sched = global.sched.lock().unwrap();
    assert_eq!(sched.committed_time, before.committed);
    assert!(!sched.thread_tree.process_group_admission_busy());
    assert_eq!(
        sched.exact_child_wait_state(
            active().unwrap().lock().unwrap().parent.unwrap(),
            state.dettid
        ),
        ExactChildWaitState::PhysicallyExited
    );
    drop(sched);
    let observed = active().unwrap();
    let mut log = observed.lock().unwrap();
    assert_eq!(log.settlements, 0);
    log.settlements += 1;
}

pub(crate) fn before_consuming_exit<T>(
    tid: Tid,
    state: &ThreadState<T>,
    status: ExitStatus,
) -> bool {
    let Some(observed) = active() else {
        return false;
    };
    let log = observed.lock().unwrap();
    if log.child != Some(state.dettid) {
        return false;
    }
    assert_eq!(tid.as_raw(), state.dettid.as_raw());
    assert!(!state.thread_start_entered);
    assert_eq!(
        status,
        ExitStatus::Signaled(reverie::Signal::SIGKILL, false)
    );
    assert_eq!(log.terminal_before_start, 1);
    assert_eq!(log.settlements, 1);
    true
}

pub(crate) fn after_consuming_exit(selected: bool) {
    if !selected {
        return;
    }
    let observed = active().unwrap();
    let mut log = observed.lock().unwrap();
    assert_eq!(log.consuming_exits, 0);
    log.consuming_exits += 1;
}

fn run_case(share_files: bool) {
    let observed = Arc::new(Mutex::new(Observed::default()));
    ACTIVE.with(|slot| {
        assert!(slot.borrow_mut().replace(Arc::clone(&observed)).is_none());
    });
    let _active = Active;
    let start = Instant::now();
    let config = Config {
        sequentialize_threads: false,
        runs_post_fork: RunsPostFork::Parent,
        ..Config::default()
    };
    assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
    let (output, global) = test_fn_with_config::<crate::Detcore, _>(
        move || unsafe {
            let flags = libc::SIGCHLD | if share_files { libc::CLONE_FILES } else { 0 };
            let child = libc::syscall(libc::SYS_clone, flags, 0usize, 0usize, 0usize, 0usize);
            assert!(
                child >= 0,
                "original native clone failed: {}",
                std::io::Error::last_os_error()
            );
            if child == 0 {
                libc::_exit(99);
            } // The child must never execute a guest instruction.
            let mut status = 0;
            assert_eq!(libc::waitpid(child as i32, &mut status, 0), child as i32);
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        },
        config,
        true,
    )
    .expect("real ptrace-to-Detcore prestart fixture");
    assert_eq!(output.status, ExitStatus::Exited(0));
    let log = observed.lock().unwrap();
    assert_eq!(log.native_births, 1);
    assert_eq!(log.terminal_before_start, 1);
    assert_eq!(log.settlements, 1);
    assert_eq!(log.consuming_exits, 1);
    let child = log.child.unwrap();
    assert_never_admitted(&global, child);
    let mut sched = global.sched.lock().unwrap();
    assert_eq!(sched.thread_tree.pending_no_seq_birth_count(), 0);
    assert!(!sched.thread_tree.process_group_admission_busy());
    assert_eq!(
        sched.exact_child_wait_state(log.parent.unwrap(), child),
        ExactChildWaitState::Unknown
    );
    drop(sched);
    assert!(
        log.stop
            .as_ref()
            .unwrap()
            .worker_drained(Duration::from_secs(1)),
        "original notifier worker must be gone"
    );
    println!(
        "native-prestart child={} births=1 final-SIGKILL=1 start-entered=0 settlements=1 consumed-exits=1 registered=0 shared-files={} elapsed={:?}",
        child,
        share_files,
        start.elapsed()
    );
}

#[test]
fn native_copied_files_child_dies_before_detcore_thread_start() {
    run_case(false);
}
#[test]
fn native_shared_files_child_dies_before_detcore_thread_start() {
    run_case(true);
}
