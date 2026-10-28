/*
 * Copyright 2019 The gVisor Authors.
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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/signalfd.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 * The descriptor is created through SYS_signalfd (the plain number the
 * coverage report found unexercised).
 *
 * Mapping: SignalfdTest.Basic, SignalfdTest.MaskWorks (with SetMask's
 * live-descriptor replacement), Signalfd.Cloexec (the plain number has
 * no flags, so this asserts a fresh descriptor is born without
 * FD_CLOEXEC), SignalfdTest.Nonblock (poll with a zero timeout).
 * Excluded: Blocking and ThreadGroup (blocking reads / scheduling),
 * Ppoll (own cells), KillStillKills (asserts process death),
 * RtQueueDeliversPayload (sigqueueinfo, a different number).
 * Known failure: on DBT the mask replacement on a live descriptor
 * returns EPERM (MaskWorks):
 * https://github.com/rrnewton/hermit/issues/4850
 */

use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

fn signalfd(fd: i32, mask: &libc::sigset_t) -> i64 {
    // SAFETY: signalfd with a valid sigset pointer and its size (8).
    unsafe { libc::syscall(libc::SYS_signalfd, fd, mask, 8) }
}

fn mask_of(signals: &[i32]) -> libc::sigset_t {
    // SAFETY: sigset operations on a zeroed set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &sig in signals {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

fn read_signo(fd: i32) -> i32 {
    let mut info = [0u8; 128];
    // SAFETY: read into a 128-byte signalfd_siginfo buffer.
    let n = unsafe { libc::read(fd, info.as_mut_ptr() as *mut libc::c_void, 128) };
    if n != 128 {
        return -1;
    }
    i32::from_ne_bytes(info[0..4].try_into().unwrap())
}

mod signalfd_test {
    use super::*;

    fn blocked_usr1() -> libc::sigset_t {
        let mask = mask_of(&[libc::SIGUSR1]);
        // SAFETY: sigprocmask with a valid set.
        check(
            unsafe { libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) } == 0,
            "block",
        );
        mask
    }

    pub fn basic() {
        let mask = blocked_usr1();
        let fd = signalfd(-1, &mask);
        // SAFETY: kill self with a scalar signal number.
        unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
        check(
            fd >= 0 && read_signo(fd as i32) == libc::SIGUSR1,
            "SignalfdTest.Basic",
        );
        if fd >= 0 {
            unsafe { libc::close(fd as i32) };
        }
    }

    pub fn mask_works() {
        let mask = blocked_usr1();
        let fd = signalfd(-1, &mask);
        let both = mask_of(&[libc::SIGUSR1, libc::SIGUSR2]);
        // SAFETY: sigprocmask with a valid set.
        unsafe { libc::sigprocmask(libc::SIG_BLOCK, &both, std::ptr::null_mut()) };
        let replaced = signalfd(fd as i32, &both);
        // SAFETY: kill self with a scalar signal number.
        unsafe { libc::kill(libc::getpid(), libc::SIGUSR2) };
        check(
            replaced == fd && read_signo(fd as i32) == libc::SIGUSR2,
            "SignalfdTest.MaskWorks",
        );
        if fd >= 0 {
            unsafe { libc::close(fd as i32) };
        }
    }

    pub fn cloexec() {
        let mask = blocked_usr1();
        let fd = signalfd(-1, &mask);
        // SAFETY: fcntl F_GETFD on a live descriptor.
        let flags = unsafe { libc::fcntl(fd as i32, libc::F_GETFD) };
        check(
            fd >= 0 && (flags & libc::FD_CLOEXEC) == 0,
            "Signalfd.Cloexec",
        );
        if fd >= 0 {
            unsafe { libc::close(fd as i32) };
        }
    }

    pub fn nonblock() {
        let mask = blocked_usr1();
        let fd = signalfd(-1, &mask);
        // SAFETY: pollfd is plain data; poll with a zero timeout.
        let mut pfd: libc::pollfd = unsafe { std::mem::zeroed() };
        pfd.fd = fd as i32;
        pfd.events = libc::POLLIN;
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        check(fd >= 0 && rc == 0, "SignalfdTest.Nonblock");
        if fd >= 0 {
            unsafe { libc::close(fd as i32) };
        }
    }
}

fn main() {
    let tests: [(&str, fn()); 4] = [
        ("SignalfdTest.Basic", signalfd_test::basic),
        ("SignalfdTest.MaskWorks", signalfd_test::mask_works),
        ("Signalfd.Cloexec", signalfd_test::cloexec),
        ("SignalfdTest.Nonblock", signalfd_test::nonblock),
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
