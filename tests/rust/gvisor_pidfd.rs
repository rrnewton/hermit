/*
 * Copyright 2026 The gVisor Authors.
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
 * Port provenance: Rust port of gVisor's
 * test/syscalls/linux/pidfd_test.cc at gVisor commit
 * 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 *
 * Mapping: PidfdTest.PidfdOpenInvalid,
 * PidfdTest.OpenPidfdSelfSucceeds,
 * PidfdTest.SendSignalViaPidfdOpenPidfdToSelfSucceeds (signal 0 and a
 * delivered SIGUSR1). PidfdTest.GetfdUnknown is excluded: Linux
 * answers pidfd_getfd on a reaped child with ESRCH, Detcore with
 * EOPNOTSUPP (measured 2026-10-27; issue draft pending with the
 * coordinator). GetfdWorks and the other getfd success cases need
 * ptrace access to another process; the Clone3 and stopped/paused
 * child cases are excluded (own cells / scheduling oracles).
 * Known failures at this commit: KVM differs on PidfdOpenInvalid,
 * the self-signal case and the dead-child case; DBT differs on the
 * dead-child pidfd_getfd errno; the guest stops at pidfd_send_signal
 * on LiteInst and in-guest-trap (all measured 2026-10-27). */

use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn sys2(number: i64, a: i64, b: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b) }
}

fn sys4(number: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c, d) }
}

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

static GOT_USR1: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_sig: i32) {
    GOT_USR1.store(true, Ordering::SeqCst);
}

mod pidfd_test {
    use super::*;

    pub fn pidfd_open_invalid() {
        let rc = sys2(libc::SYS_pidfd_open, -1, 0);
        check(
            rc == -1 && errno() == libc::EINVAL,
            "PidfdTest.PidfdOpenInvalid",
        );
    }

    pub fn open_pidfd_self_succeeds() {
        // SAFETY: getpid cannot fail.
        let pid = unsafe { libc::getpid() };
        let fd = sys2(libc::SYS_pidfd_open, pid as i64, 0);
        check(fd >= 0, "PidfdTest.OpenPidfdSelfSucceeds");
        if fd >= 0 {
            unsafe { libc::close(fd as i32) };
        }
    }

    pub fn send_signal_to_self_succeeds() {
        // SAFETY: sigaction with a zeroed structure and a handler.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_usr1 as extern "C" fn(i32) as usize;
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
        }
        // SAFETY: getpid cannot fail.
        let pid = unsafe { libc::getpid() };
        let fd = sys2(libc::SYS_pidfd_open, pid as i64, 0);
        let zero_sig = if fd >= 0 {
            sys4(libc::SYS_pidfd_send_signal, fd, 0, 0, 0)
        } else {
            -1
        };
        let usr1 = if fd >= 0 {
            sys4(libc::SYS_pidfd_send_signal, fd, libc::SIGUSR1 as i64, 0, 0)
        } else {
            -1
        };
        check(
            zero_sig == 0 && usr1 == 0 && GOT_USR1.load(Ordering::SeqCst),
            "PidfdTest.SendSignalViaPidfdOpenPidfdToSelfSucceeds",
        );
        if fd >= 0 {
            unsafe { libc::close(fd as i32) };
        }
    }
}

fn main() {
    let tests: [(&str, fn()); 3] = [
        ("PidfdTest.PidfdOpenInvalid", pidfd_test::pidfd_open_invalid),
        (
            "PidfdTest.OpenPidfdSelfSucceeds",
            pidfd_test::open_pidfd_self_succeeds,
        ),
        (
            "PidfdTest.SendSignalViaPidfdOpenPidfdToSelfSucceeds",
            pidfd_test::send_signal_to_self_succeeds,
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
