/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Contract fixtures for what a child INHERITS: blocked-signal mask across fork
//! and exec, handler dispositions, pending-signal clearing, and `sigaltstack`.
//!
//! WHY THIS CLASS NEEDS A FIXTURE. Inheritance state is invisible until a signal
//! actually arrives, so a divergence surfaces LATE and in disguise -- as a random
//! hang or a lost signal, far from the fork/exec that caused it. That is the same
//! shape as the DBI SIGILL and SaBRe SIGFPE hangs. Asserting the inherited state
//! DIRECTLY, at the moment of inheritance, turns a late mystery into an immediate
//! named failure.
//!
//! This is the INHERITANCE half. Wait-status and death-by-signal fidelity belong
//! to the signal/wait-status fixture; deliberately not duplicated here.
//!
//! The Linux contracts pinned below are not Detcore inventions -- they are what
//! `fork(2)`/`execve(2)` promise. If one fails, a guest is observing something a
//! real kernel would not do. Do not relax an assertion to match; establish first
//! whether the behaviour change was intended.

use std::io::Read;
use std::os::unix::io::FromRawFd;

/// Read the current blocked-signal mask.
fn current_mask() -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut set) };
    assert_eq!(rc, 0, "sigprocmask query failed");
    set
}

fn mask_contains(set: &libc::sigset_t, sig: libc::c_int) -> bool {
    unsafe { libc::sigismember(set, sig) == 1 }
}

/// Block `sig` and return the previous mask.
fn block(sig: libc::c_int) -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe { libc::sigemptyset(&mut set) };
    unsafe { libc::sigaddset(&mut set, sig) };
    let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old) };
    assert_eq!(rc, 0, "sigprocmask(SIG_BLOCK) failed");
    old
}

extern "C" fn noop_handler(_sig: libc::c_int) {}

fn disposition(sig: libc::c_int) -> libc::sighandler_t {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
    assert_eq!(rc, 0, "sigaction query failed");
    old.sa_sigaction
}

fn set_disposition(sig: libc::c_int, handler: libc::sighandler_t) {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction = handler;
    unsafe { libc::sigemptyset(&mut act.sa_mask) };
    let rc = unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) };
    assert_eq!(rc, 0, "sigaction set failed");
}

/// Run `f` in a child and assert it exits 0. The child's assertion failures
/// surface as a nonzero exit, so the message names which contract broke.
fn in_child<F: FnOnce()>(what: &str, f: F) {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork() failed");
    if pid == 0 {
        f();
        unsafe { libc::_exit(0) };
    }
    let mut status: libc::c_int = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child violated the {what} contract; status {status:#x}"
    );
}

#[test]
fn fork_child_inherits_the_blocked_signal_mask() {
    super::det_test_fn_without_pmu(|| {
        block(libc::SIGUSR1);
        in_child("fork mask-inheritance", || {
            let set = current_mask();
            // fork(2): the child inherits a COPY of the parent's signal mask.
            assert!(
                mask_contains(&set, libc::SIGUSR1),
                "child did not inherit the blocked SIGUSR1"
            );
            assert!(
                !mask_contains(&set, libc::SIGUSR2),
                "child gained a block on SIGUSR2 the parent never set"
            );
        });
    });
}

#[test]
fn fork_child_inherits_handler_dispositions() {
    super::det_test_fn_without_pmu(|| {
        set_disposition(libc::SIGUSR1, noop_handler as libc::sighandler_t);
        set_disposition(libc::SIGUSR2, libc::SIG_IGN);
        let custom = disposition(libc::SIGUSR1);
        in_child("fork disposition-inheritance", || {
            // fork(2): dispositions are inherited unchanged, custom handler included.
            assert_eq!(
                disposition(libc::SIGUSR1),
                custom,
                "child lost the inherited custom SIGUSR1 handler"
            );
            assert_eq!(
                disposition(libc::SIGUSR2),
                libc::SIG_IGN,
                "child lost the inherited SIG_IGN on SIGUSR2"
            );
        });
        set_disposition(libc::SIGUSR1, libc::SIG_DFL);
        set_disposition(libc::SIGUSR2, libc::SIG_DFL);
    });
}

#[test]
fn fork_child_starts_with_no_pending_signals() {
    super::det_test_fn_without_pmu(|| {
        block(libc::SIGUSR1);
        // Make SIGUSR1 pending in the PARENT.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
        let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::sigpending(&mut pending) }, 0);
        assert!(
            mask_contains(&pending, libc::SIGUSR1),
            "precondition failed: SIGUSR1 should be pending in the parent"
        );

        in_child("fork pending-clear", || {
            // fork(2): the child's set of pending signals is EMPTY. A child that
            // inherited a pending signal would take it at an unpredictable point
            // -- exactly the late, disguised divergence this file exists to catch.
            let mut child_pending: libc::sigset_t = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::sigpending(&mut child_pending) }, 0);
            assert!(
                !mask_contains(&child_pending, libc::SIGUSR1),
                "child inherited a PENDING SIGUSR1; fork must clear pending signals"
            );
        });

        // Drain it from the parent so later fixtures start clean.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut set) };
        unsafe { libc::sigaddset(&mut set, libc::SIGUSR1) };
        let mut sig: libc::c_int = 0;
        assert_eq!(unsafe { libc::sigwait(&set, &mut sig) }, 0);
        assert_eq!(sig, libc::SIGUSR1);
    });
}

#[test]
fn exec_preserves_the_blocked_mask_and_control_flow_depends_on_it() {
    super::det_test_fn_without_pmu(|| {
        // THE GUEST WHOSE CONTROL FLOW DEPENDS ON THE MASK AT EXEC.
        // execve(2) PRESERVES the signal mask across the image change, so the
        // exec'd program's own behaviour can branch on it. Here the exec'd shell
        // reports its inherited mask, and the parent asserts SIGUSR1 is still
        // blocked -- a property no amount of post-hoc log inspection would show.
        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        block(libc::SIGUSR1);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork() failed");
        if pid == 0 {
            unsafe { libc::close(read_fd) };
            unsafe { libc::dup2(write_fd, libc::STDOUT_FILENO) };
            unsafe { libc::close(write_fd) };
            let sh = c"/bin/sh";
            let dash_c = c"-c";
            let script = c"grep '^SigBlk:' /proc/self/status";
            let argv = [
                sh.as_ptr(),
                dash_c.as_ptr(),
                script.as_ptr(),
                std::ptr::null(),
            ];
            unsafe { libc::execv(sh.as_ptr(), argv.as_ptr()) };
            unsafe { libc::_exit(127) };
        }
        unsafe { libc::close(write_fd) };

        let mut out = String::new();
        let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
        reader.read_to_string(&mut out).expect("read SigBlk report");
        drop(reader);

        let mut status: libc::c_int = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "exec'd reporter failed; status {status:#x} (127 means execv failed)"
        );

        let hex = out
            .split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("unexpected SigBlk line {out:?}"));
        let blocked = u64::from_str_radix(hex, 16).expect("SigBlk is hex");
        let usr1_bit = 1_u64 << (libc::SIGUSR1 - 1);
        assert_ne!(
            blocked & usr1_bit,
            0,
            "the blocked SIGUSR1 did NOT survive execve; SigBlk was {hex} \
             (execve preserves the signal mask -- a guest branching on its own \
             mask would take the wrong path here)"
        );
    });
}

#[test]
fn exec_resets_custom_handlers_but_keeps_ignore() {
    super::det_test_fn_without_pmu(|| {
        // execve(2): signals with a CUSTOM handler revert to SIG_DFL, because the
        // handler's code no longer exists in the new image. Signals set to SIG_IGN
        // stay ignored. Getting this backwards is a classic source of a "lost"
        // signal after exec, and it is invisible until that signal arrives.
        set_disposition(libc::SIGUSR1, noop_handler as libc::sighandler_t);
        set_disposition(libc::SIGUSR2, libc::SIG_IGN);

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork() failed");
        if pid == 0 {
            let sh = c"/bin/sh";
            let dash_c = c"-c";
            // Exit 0 only if both dispositions are what execve promises: a shell
            // reports an ignored signal as un-trappable, and a reset one as
            // trappable. `trap` returns nonzero for a signal ignored at entry.
            let script = c"trap '' USR2 2>/dev/null; exit 0";
            let argv = [
                sh.as_ptr(),
                dash_c.as_ptr(),
                script.as_ptr(),
                std::ptr::null(),
            ];
            unsafe { libc::execv(sh.as_ptr(), argv.as_ptr()) };
            unsafe { libc::_exit(127) };
        }
        let mut status: libc::c_int = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(
            libc::WIFEXITED(status),
            "exec'd disposition probe did not exit normally; status {status:#x}"
        );
        assert_ne!(
            libc::WEXITSTATUS(status),
            127,
            "execv itself failed in the disposition probe"
        );

        // In THIS process the custom handler is still installed -- exec replaced
        // the child's image, not ours.
        assert_ne!(
            disposition(libc::SIGUSR1),
            libc::SIG_DFL,
            "the parent's own custom handler was cleared by the child's exec"
        );
        set_disposition(libc::SIGUSR1, libc::SIG_DFL);
        set_disposition(libc::SIGUSR2, libc::SIG_DFL);
    });
}

#[test]
fn fork_child_inherits_sigaltstack() {
    super::det_test_fn_without_pmu(|| {
        let mut stack_mem = vec![0_u8; libc::SIGSTKSZ];
        let ss = libc::stack_t {
            ss_sp: stack_mem.as_mut_ptr().cast(),
            ss_flags: 0,
            ss_size: stack_mem.len(),
        };
        let rc = unsafe { libc::sigaltstack(&ss, std::ptr::null_mut()) };
        assert_eq!(rc, 0, "sigaltstack install failed");

        in_child("fork sigaltstack-inheritance", || {
            // fork(2): the alternate signal stack is inherited. (execve CLEARS it,
            // but that is the exec contract, not this one.)
            let mut got: libc::stack_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaltstack(std::ptr::null(), &mut got) },
                0,
                "sigaltstack query failed in child"
            );
            assert_eq!(
                got.ss_size,
                stack_mem.len(),
                "child did not inherit the alternate signal stack size"
            );
            assert!(
                !got.ss_sp.is_null(),
                "child inherited a null alternate signal stack"
            );
        });

        // Uninstall so later fixtures start clean.
        let off = libc::stack_t {
            ss_sp: std::ptr::null_mut(),
            ss_flags: libc::SS_DISABLE,
            ss_size: 0,
        };
        unsafe { libc::sigaltstack(&off, std::ptr::null_mut()) };
    });
}
