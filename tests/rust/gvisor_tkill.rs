/*
 * Copyright 2018 The gVisor Authors.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * Port provenance: one-to-one Rust port of gVisor's
 * test/syscalls/linux/tkill.cc at gVisor commit
 * 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. Only libc (already a workspace
 * dependency) is used.
 *
 * Per-test mapping (all three cases ported, same names, same assertions):
 *   TkillTest.InvalidTID            -> tkill_test::invalid_tid
 *   TkillTest.ValidTID              -> tkill_test::valid_tid
 *   TkillTest.ValidTIDAndRealSignal -> tkill_test::valid_tid_and_real_signal
 * Excluded: none. Note: ValidTIDAndRealSignal kills the guest on
 * Hermit's ptrace backend at this commit (the SIGRTMAX handler does
 * not run; the process exits as if by the default disposition):
 * https://github.com/rrnewton/hermit/issues/4851
 * On native Linux all three cases pass.
 */

use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use googletest::assert_that;
use googletest::matchers::eq;

/// The original's EAGAIN-retrying tkill wrapper (b/25434735: tkill may
/// return EAGAIN for RT signals).
fn tkill(tid: i32, sig: i32) -> std::io::Result<()> {
    loop {
        // SAFETY: tkill takes two scalars; the result is checked.
        let rc = unsafe { libc::syscall(libc::SYS_tkill, tid, sig) };
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EAGAIN) {
            return Err(error);
        }
    }
}

fn gettid() -> i32 {
    // SAFETY: gettid takes no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

static HANDLER_RAN: AtomicBool = AtomicBool::new(false);

extern "C" fn sig_handler(sig: i32, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    // Mirrors the original's TEST_CHECKs and records that they held, so
    // the case can assert after tkill returns instead of dying inside
    // the handler.
    let ok = sig == libc::SIGRTMAX()
        && !info.is_null()
        // SAFETY: the kernel hands the handler a valid siginfo_t.
        && unsafe { (*info).si_pid() } == unsafe { libc::getpid() }
        && unsafe { (*info).si_uid() } == unsafe { libc::getuid() }
        && unsafe { (*info).si_code } == libc::SI_TKILL;
    if ok {
        HANDLER_RAN.store(true, Ordering::SeqCst);
    }
}

mod tkill_test {
    use super::*;

    pub fn invalid_tid() {
        let neg = tkill(-1, 0);
        check(
            neg.as_ref().err().and_then(|e| e.raw_os_error()) == Some(libc::EINVAL),
            "tkill(-1, 0) is EINVAL",
        );
        let zero = tkill(0, 0);
        check(
            zero.as_ref().err().and_then(|e| e.raw_os_error()) == Some(libc::EINVAL),
            "tkill(0, 0) is EINVAL",
        );
    }

    pub fn valid_tid() {
        check(tkill(gettid(), 0).is_ok(), "tkill(gettid(), 0) succeeds");
    }

    pub fn valid_tid_and_real_signal() {
        // SAFETY: sigaction/sigprocmask with valid, zeroed structures.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction =
                sig_handler as extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) as usize;
            libc::sigfillset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_SIGINFO;
            check(
                libc::sigaction(libc::SIGRTMAX(), &sa, std::ptr::null_mut()) == 0,
                "sigaction",
            );
            let mut unblock: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblock);
            libc::sigaddset(&mut unblock, libc::SIGRTMAX());
            check(
                libc::sigprocmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut()) == 0,
                "sigprocmask unblocks SIGRTMAX",
            );
        }
        check(
            tkill(gettid(), libc::SIGRTMAX()).is_ok(),
            "tkill SIGRTMAX succeeds",
        );
        check(
            HANDLER_RAN.load(Ordering::SeqCst),
            "handler ran with SI_TKILL siginfo",
        );
    }
}

fn main() {
    let tests: [(&str, fn()); 3] = [
        ("TkillTest.InvalidTID", tkill_test::invalid_tid),
        ("TkillTest.ValidTID", tkill_test::valid_tid),
        (
            "TkillTest.ValidTIDAndRealSignal",
            tkill_test::valid_tid_and_real_signal,
        ),
    ];
    let mut failures = 0;
    for (name, test) in tests {
        match std::panic::catch_unwind(AssertUnwindSafe(test)) {
            Ok(()) => println!("{name}: PASS"),
            Err(_) => {
                println!("{name}: FAIL");
                failures += 1;
            }
        }
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    println!("failures={failures}");
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
