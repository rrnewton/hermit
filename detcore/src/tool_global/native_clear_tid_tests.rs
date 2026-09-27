/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Real ptrace-to-Detcore NoSeq clear-TID controls. No manufactured wake or exit.
//! Execute each selector under the existing bounded native-test command owner.
use std::cell::RefCell;
use std::sync::atomic::AtomicI32;
use std::time::Duration;
use std::time::Instant;

use reverie::ExitStatus;
use reverie::InjectedSyscallEvent;
use reverie::syscalls::SyscallArgs;
use reverie_ptrace::testing::test_fn_with_config;

use super::*;
use crate::config::RunsPostFork;
use crate::tool_local::ThreadState;

const WAIT_SECONDS: u64 = 1;
const STAGING: i32 = 77;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Case {
    Original,
    Replacement,
    Nearby,
}

// Linux's syscall-exit stop precedes signal/restart processing. A raw -516
// therefore retains this wait; only a later actual return can complete it.
fn restart_block_raw() -> i64 {
    -(reverie::syscalls::Errno::ERESTART_RESTARTBLOCK.into_raw() as i64)
}

#[derive(Debug, Default, Eq, PartialEq)]
enum ParentWaitPhase {
    #[default]
    Initial,
    Restart {
        owner: DetTid,
        expected: i64,
    },
    Complete(i64),
}

#[derive(Debug, Default)]
struct ParentWaitReceipt {
    phase: ParentWaitPhase,
    restart_returns: usize,
    restart_continuations: usize,
}

impl ParentWaitReceipt {
    fn initial(
        &mut self,
        owner: DetTid,
        raw: i64,
        expected: i64,
    ) -> Result<Option<i64>, &'static str> {
        if self.phase != ParentWaitPhase::Initial {
            return Err("duplicate initial wait return");
        }
        if raw == restart_block_raw() {
            self.phase = ParentWaitPhase::Restart { owner, expected };
            self.restart_returns += 1;
            Ok(None)
        } else if raw == expected {
            self.phase = ParentWaitPhase::Complete(raw);
            Ok(Some(raw))
        } else {
            Err("initial wait did not produce its required result")
        }
    }

    fn restarted(&mut self, caller: DetTid, raw: i64) -> Result<Option<i64>, &'static str> {
        let ParentWaitPhase::Restart { owner, expected } = self.phase else {
            return Err("restart without an outstanding wait");
        };
        if caller != owner {
            return Err("restart belongs to a different parent");
        }
        if raw == restart_block_raw() {
            // Each real restart can itself be interrupted. Retain the same
            // operation and deadline; this observer never reissues a syscall.
            self.restart_returns += 1;
            self.restart_continuations += 1;
            Ok(None)
        } else if raw == expected {
            self.phase = ParentWaitPhase::Complete(raw);
            self.restart_continuations += 1;
            Ok(Some(raw))
        } else {
            Err("restart did not produce the required final result")
        }
    }

    fn completed(&self) -> Result<i64, &'static str> {
        match self.phase {
            ParentWaitPhase::Complete(raw)
                if self.restart_returns == self.restart_continuations =>
            {
                Ok(raw)
            }
            _ => Err("native wait has no consumed final return"),
        }
    }
}

struct Observed {
    case: Case,
    parent: Option<DetTid>,
    child: Option<DetTid>,
    born: Option<Instant>,
    initial: usize,
    replacement: Option<usize>,
    staging: Option<usize>,
    destination: Option<usize>,
    requeues: usize,
    parent_wait: Option<i64>,
    parent_wait_receipt: ParentWaitReceipt,
    parent_reaped: usize,
    clone_returned: usize,
    child_terminal: usize,
    parent_terminal: usize,
}

thread_local! {
    // The backend's LocalSet executes these callbacks on the same tracer
    // thread. No other test is enrolled through a process-wide switch.
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

pub(crate) fn observe<T>(
    tid: Tid,
    global: &GlobalState,
    state: &ThreadState<T>,
    nr: Sysno,
    args: SyscallArgs,
    event: InjectedSyscallEvent,
) {
    let Some(observed) = active() else {
        return;
    };
    let mut log = observed.lock().unwrap();
    assert!(!global.cfg.sequentialize_threads);
    assert_eq!(tid.as_raw(), state.dettid.as_raw());
    if let InjectedSyscallEvent::ChildCreated(child) = event {
        assert_eq!(nr, Sysno::clone);
        assert!(log.child.is_none(), "exactly one real native child");
        assert_eq!(
            args.arg0,
            (libc::SIGCHLD | libc::CLONE_VM | libc::CLONE_CHILD_SETTID | libc::CLONE_CHILD_CLEARTID)
                as usize
        );
        // x86-64 scalar clone's fourth argument, not a mutable clone3 pre-read.
        assert_ne!(args.arg3, 0);
        log.initial = args.arg3;
        log.parent = Some(state.dettid);
        log.child = Some(DetTid::from_raw(child.as_raw()));
        log.born = Some(Instant::now());
        return;
    }
    if let InjectedSyscallEvent::ChildSyscallReturned { child, raw } = event {
        assert_eq!(Some(state.dettid), log.parent);
        assert_eq!(nr, Sysno::clone);
        assert_eq!(Some(DetTid::from_raw(child.as_raw())), log.child);
        assert_eq!(raw, i64::from(child.as_raw()));
        assert_eq!(
            log.clone_returned, 0,
            "parent exit boundary must be observed once"
        );
        log.clone_returned += 1;
        return;
    }
    let InjectedSyscallEvent::Returned(raw) = event else {
        return;
    };
    if nr == Sysno::restart_syscall {
        let completed = log
            .parent_wait_receipt
            .restarted(state.dettid, raw)
            .expect("actual restart must consume this parent's retained wait");
        if let Some(completed) = completed {
            assert!(log.parent_wait.replace(completed).is_none());
        }
        return;
    }
    if Some(state.dettid) == log.child {
        if nr == Sysno::set_tid_address {
            assert_eq!(log.case, Case::Replacement);
            assert_eq!(raw, tid.as_raw() as i64);
            assert_eq!(args.arg0, log.initial + std::mem::size_of::<i32>());
            assert!(log.replacement.replace(args.arg0).is_none());
        } else if nr == Sysno::futex {
            // No explicit wake is allowed: zero wakees, at most one requeue.
            assert_eq!(args.arg1, libc::FUTEX_CMP_REQUEUE as usize);
            assert_eq!(args.arg2, 0);
            assert_eq!(args.arg3, 1);
            assert_eq!(args.arg5, STAGING as usize);
            assert_ne!(args.arg0, args.arg4);
            assert!(
                matches!(raw, 0 | 1),
                "unexpected native requeue result {raw}"
            );
            if let Some(staging) = log.staging {
                assert_eq!(staging, args.arg0);
            }
            if let Some(destination) = log.destination {
                assert_eq!(destination, args.arg4);
            }
            log.staging = Some(args.arg0);
            log.destination = Some(args.arg4);
            match log.case {
                Case::Original => assert_eq!(args.arg4, log.initial),
                Case::Replacement => assert_eq!(Some(args.arg4), log.replacement),
                Case::Nearby => assert_eq!(args.arg4, log.initial + 2 * std::mem::size_of::<i32>()),
            }
            if raw == 1 {
                assert_eq!(log.requeues, 0, "queued waiter must transfer only once");
                log.requeues += 1;
            }
        }
    } else if Some(state.dettid) == log.parent && nr == Sysno::clone {
        panic!("untyped clone scalar is not the authenticated parent completion");
    } else if Some(state.dettid) == log.parent && nr == Sysno::wait4 {
        assert_eq!(args.arg0, log.child.unwrap().as_raw() as usize);
        assert!(raw == 0 || raw == log.child.unwrap().as_raw() as i64);
        if raw != 0 {
            assert_eq!(log.parent_reaped, 0);
            log.parent_reaped += 1;
        }
    } else if Some(state.dettid) == log.parent && nr == Sysno::futex {
        assert_eq!(args.arg1, libc::FUTEX_WAIT as usize);
        assert_eq!(args.arg2, STAGING as usize);
        assert_eq!(Some(args.arg0), log.staging);
        assert_eq!(log.requeues, 1, "require actual queued-waiter proof");
        let expected = if log.case == Case::Nearby {
            -(libc::ETIMEDOUT as i64)
        } else {
            0
        };
        let completed = log
            .parent_wait_receipt
            .initial(state.dettid, raw, expected)
            .expect("native wait must complete or retain its actual restart");
        if let Some(raw) = completed {
            assert_eq!(raw, expected);
            assert!(log.parent_wait.replace(raw).is_none());
        }
    }
}

pub(crate) fn terminal<T>(
    tid: Tid,
    global: &GlobalState,
    state: &ThreadState<T>,
    status: ExitStatus,
) {
    let Some(observed) = active() else {
        return;
    };
    let mut log = observed.lock().unwrap();
    if Some(state.dettid) == log.child {
        assert_eq!(tid.as_raw(), state.dettid.as_raw());
        assert!(state.thread_start_entered);
        assert_eq!(status, ExitStatus::Exited(0));
        assert_eq!(log.requeues, 1);
        assert_eq!(log.child_terminal, 0);
        if log.case == Case::Nearby {
            // Birth precedes the parent's FUTEX_WAIT entry. A real final wait
            // before birth+timeout precedes the earliest possible timeout,
            // even if the return callback is delayed.
            assert!(
                log.born.unwrap().elapsed() < Duration::from_secs(WAIT_SECONDS),
                "child must actually terminate before the negative wait can time out"
            );
        }
        log.child_terminal += 1;
        assert!(!global.sched.lock().unwrap().backend_failed());
    } else if Some(state.dettid) == log.parent {
        assert_eq!(status, ExitStatus::Exited(0));
        assert_eq!(log.child_terminal, 1);
        assert_eq!(log.parent_terminal, 0);
        log.parent_terminal += 1;
    }
}

#[repr(C)]
struct Shared {
    initial: AtomicI32,
    replacement: AtomicI32,
    nearby: AtomicI32,
    staging: AtomicI32,
    requeued: AtomicI32,
    case: Case,
}

// The callback performs no allocation and never unwinds across the C ABI.
extern "C" fn child_main(arg: *mut libc::c_void) -> libc::c_int {
    unsafe {
        let shared = &*(arg as *const Shared);
        let tid = libc::syscall(libc::SYS_gettid) as i32;
        if tid <= 0 || shared.initial.load(SeqCst) != tid {
            return 41;
        }
        shared.replacement.store(tid, SeqCst);
        shared.nearby.store(tid, SeqCst);
        let target = match shared.case {
            Case::Original => shared.initial.as_ptr(),
            Case::Replacement => {
                if libc::syscall(libc::SYS_set_tid_address, shared.replacement.as_ptr())
                    != tid as libc::c_long
                {
                    return 42;
                }
                shared.replacement.as_ptr()
            }
            Case::Nearby => shared.nearby.as_ptr(),
        };
        for _ in 0..1024 {
            // Linux clear_child_tid uses FUTEX_WAKE without PRIVATE_FLAG.
            // Wait and requeue use the same shared key in this shared MM.
            let result = libc::syscall(
                libc::SYS_futex,
                shared.staging.as_ptr(),
                libc::FUTEX_CMP_REQUEUE,
                0usize,
                1usize,
                target,
                STAGING,
            );
            if result == 1 {
                shared.requeued.store(1, SeqCst);
                return 0; // libc's clone trampoline executes actual SYS_exit.
            }
            if result != 0 {
                return 43;
            }
            if libc::syscall(libc::SYS_sched_yield) != 0 {
                return 44;
            }
        }
        45 // Exhausting the bounded handshake is a test failure.
    }
}

fn run_case(case: Case) {
    let observed = Arc::new(Mutex::new(Observed {
        case,
        parent: None,
        child: None,
        born: None,
        initial: 0,
        replacement: None,
        staging: None,
        destination: None,
        requeues: 0,
        parent_wait: None,
        parent_wait_receipt: ParentWaitReceipt::default(),
        parent_reaped: 0,
        clone_returned: 0,
        child_terminal: 0,
        parent_terminal: 0,
    }));
    ACTIVE.with(|slot| {
        assert!(slot.borrow_mut().replace(Arc::clone(&observed)).is_none());
    });
    let _active = Active;
    let config = Config {
        sequentialize_threads: false,
        runs_post_fork: RunsPostFork::Parent,
        ..Config::default()
    };
    assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
    let (output, global) = test_fn_with_config::<crate::Detcore, _>(
        move || unsafe {
            let shared = Box::new(Shared {
                initial: AtomicI32::new(-71),
                replacement: AtomicI32::new(-72),
                nearby: AtomicI32::new(-73),
                staging: AtomicI32::new(STAGING),
                requeued: AtomicI32::new(0),
                case,
            });
            // Separate aligned 128KiB child stack. Both allocations stay owned
            // until native waitpid has actually reaped this exact child.
            let mut stack = vec![0u128; 8192];
            let top = stack.as_mut_ptr().add(stack.len()).cast::<libc::c_void>();
            let flags = libc::SIGCHLD
                | libc::CLONE_VM
                | libc::CLONE_CHILD_SETTID
                | libc::CLONE_CHILD_CLEARTID;
            let child = libc::clone(
                child_main,
                top,
                flags,
                (&*shared as *const Shared)
                    .cast_mut()
                    .cast::<libc::c_void>(),
                std::ptr::null_mut::<i32>(),
                std::ptr::null_mut::<libc::c_void>(),
                shared.initial.as_ptr(),
            );
            assert!(
                child > 0,
                "native clone: {}",
                std::io::Error::last_os_error()
            );
            let timeout = libc::timespec {
                tv_sec: WAIT_SECONDS as _,
                tv_nsec: 0,
            };
            let result = libc::syscall(
                libc::SYS_futex,
                shared.staging.as_ptr(),
                libc::FUTEX_WAIT,
                STAGING,
                &timeout as *const libc::timespec,
                std::ptr::null_mut::<i32>(),
                0usize,
            );
            let errno = if result < 0 {
                *libc::__errno_location()
            } else {
                0
            };
            if case == Case::Nearby {
                assert_eq!(result, -1);
                assert_eq!(errno, libc::ETIMEDOUT);
            } else {
                assert_eq!(result, 0, "native wait errno={errno}");
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            assert_eq!(shared.requeued.load(SeqCst), 1);
            assert_eq!(
                shared.initial.load(SeqCst),
                if case == Case::Replacement { child } else { 0 }
            );
            assert_eq!(
                shared.replacement.load(SeqCst),
                if case == Case::Replacement { 0 } else { child }
            );
            assert_eq!(shared.nearby.load(SeqCst), child);
            assert_eq!(shared.staging.load(SeqCst), STAGING);
            // Stack and words can now drop: native waitpid succeeded.
        },
        config,
        true,
    )
    .expect("real ptrace-to-Detcore clear-TID fixture");
    assert_eq!(output.status, ExitStatus::Exited(0));
    let log = observed.lock().unwrap();
    assert!(log.child.is_some());
    assert_eq!(log.requeues, 1);
    assert_eq!(
        log.parent_wait,
        Some(if case == Case::Nearby {
            -(libc::ETIMEDOUT as i64)
        } else {
            0
        })
    );
    assert_eq!(log.child_terminal, 1);
    assert_eq!(log.parent_terminal, 1);
    assert_eq!(log.clone_returned, 1);
    assert_eq!(log.parent_reaped, 1);
    assert_eq!(
        log.parent_wait_receipt
            .completed()
            .expect("every observed restart must finish"),
        log.parent_wait.unwrap()
    );
    assert_eq!(log.replacement.is_some(), case == Case::Replacement);
    let sched = global.sched.lock().unwrap();
    assert!(!sched.backend_failed());
    assert!(sched.next_turns.is_empty());
    sched.assert_native_clear_tid_idle();
    assert_eq!(sched.thread_tree.pending_no_seq_birth_count(), 0);
    assert!(!sched.thread_tree.process_group_admission_busy());
    println!(
        "native-clear-tid-return case={case:?} parent={} restarts={} continuations={} final={}",
        log.parent.unwrap(),
        log.parent_wait_receipt.restart_returns,
        log.parent_wait_receipt.restart_continuations,
        log.parent_wait.unwrap()
    );
    println!(
        "native-clear-tid case={case:?} child={} queued=1 child-final=1 parent-final=1 modeled-clears=0",
        log.child.unwrap()
    );
}

#[test]
fn native_clear_tid_wakes_requeued_waiter() {
    run_case(Case::Original);
}
#[test]
fn native_set_tid_address_replaces_clear_word() {
    run_case(Case::Replacement);
}
#[test]
fn native_clear_tid_does_not_wake_nearby_word() {
    run_case(Case::Nearby);
}

#[test]
fn native_wait_observer_accepts_direct_terminal_timeout() {
    let mut receipt = ParentWaitReceipt::default();
    let expected = -(libc::ETIMEDOUT as i64);
    assert_eq!(
        receipt.initial(DetTid::from_raw(7), expected, expected),
        Ok(Some(expected))
    );
    assert_eq!(receipt.completed(), Ok(expected));
    assert_eq!(
        (receipt.restart_returns, receipt.restart_continuations),
        (0, 0)
    );
}

#[test]
fn native_wait_observer_retains_repeated_restart_receipts_until_final_timeout() {
    let owner = DetTid::from_raw(7);
    let expected = -(libc::ETIMEDOUT as i64);
    let mut receipt = ParentWaitReceipt::default();
    assert_eq!(
        receipt.initial(owner, restart_block_raw(), expected),
        Ok(None)
    );
    for _ in 0..3 {
        assert_eq!(receipt.restarted(owner, restart_block_raw()), Ok(None));
        assert_eq!(
            receipt.completed(),
            Err("native wait has no consumed final return")
        );
    }
    assert_eq!(
        (receipt.restart_returns, receipt.restart_continuations),
        (4, 3)
    );
    assert_eq!(receipt.restarted(owner, expected), Ok(Some(expected)));
    assert_eq!(receipt.completed(), Ok(expected));
    assert_eq!(
        (receipt.restart_returns, receipt.restart_continuations),
        (4, 4)
    );
}

#[test]
fn native_wait_observer_rejects_missing_restart_completion() {
    let mut receipt = ParentWaitReceipt::default();
    assert_eq!(
        receipt.initial(
            DetTid::from_raw(7),
            restart_block_raw(),
            -(libc::ETIMEDOUT as i64)
        ),
        Ok(None)
    );
    assert_eq!(
        receipt.completed(),
        Err("native wait has no consumed final return")
    );
    assert_eq!(
        (receipt.restart_returns, receipt.restart_continuations),
        (1, 0)
    );
}

#[test]
fn native_wait_observer_rejects_wrong_parent_without_consuming_restart() {
    let owner = DetTid::from_raw(7);
    let expected = -(libc::ETIMEDOUT as i64);
    let mut receipt = ParentWaitReceipt::default();
    assert_eq!(
        receipt.initial(owner, restart_block_raw(), expected),
        Ok(None)
    );
    assert_eq!(
        receipt.restarted(DetTid::from_raw(8), expected),
        Err("restart belongs to a different parent")
    );
    assert_eq!(
        receipt.completed(),
        Err("native wait has no consumed final return")
    );
    assert_eq!(
        (receipt.restart_returns, receipt.restart_continuations),
        (1, 0)
    );
    assert_eq!(receipt.restarted(owner, expected), Ok(Some(expected)));
}

#[test]
fn native_wait_observer_rejects_wrong_final_result_without_consuming_restart() {
    let owner = DetTid::from_raw(7);
    let expected = -(libc::ETIMEDOUT as i64);
    let mut receipt = ParentWaitReceipt::default();
    assert_eq!(
        receipt.initial(owner, restart_block_raw(), expected),
        Ok(None)
    );
    for wrong in [0, -(libc::EINTR as i64), -1] {
        assert_eq!(
            receipt.restarted(owner, wrong),
            Err("restart did not produce the required final result")
        );
        assert_eq!(
            receipt.completed(),
            Err("native wait has no consumed final return")
        );
        assert_eq!(
            (receipt.restart_returns, receipt.restart_continuations),
            (1, 0)
        );
    }
    assert_eq!(receipt.restarted(owner, expected), Ok(Some(expected)));
}

#[test]
fn native_wait_observer_rejects_unrelated_and_duplicate_completion() {
    let owner = DetTid::from_raw(7);
    let expected = -(libc::ETIMEDOUT as i64);
    let mut receipt = ParentWaitReceipt::default();
    assert_eq!(
        receipt.restarted(owner, expected),
        Err("restart without an outstanding wait")
    );
    assert_eq!(
        receipt.initial(owner, expected, expected),
        Ok(Some(expected))
    );
    assert_eq!(
        receipt.initial(owner, expected, expected),
        Err("duplicate initial wait return")
    );
    assert_eq!(
        receipt.restarted(owner, expected),
        Err("restart without an outstanding wait")
    );
    assert_eq!(receipt.completed(), Ok(expected));
}
