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
 * test/syscalls/linux/timerfd.cc at gVisor commit
 * 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. Only libc (already a workspace
 * dependency) is used.
 *
 * Per-test mapping: the original is TEST_P(TimerfdTest, X) instantiated
 * for CLOCK_MONOTONIC and CLOCK_BOOTTIME (25 case instances with the
 * three ClockRealtime tests). Each ported case keeps its name and is run
 * for both clocks, as timerfd_test::x_monotonic / x_boottime:
 *   TimerfdTest.IsInitiallyStopped        -> is_initially_stopped_*
 *   TimerfdTest.SetToPastExpiresEventually-> set_to_past_expires_eventually_*
 *   TimerfdTest.IllegalPread              -> illegal_pread_*
 *   TimerfdTest.IllegalPwrite             -> illegal_pwrite_*
 *   TimerfdTest.IllegalWrite              -> illegal_write_*
 * Excluded, with reasons (native runtimes measured where applicable):
 *   TimerfdTest.SingleShot (x2) — sleeps 1 s plus 500 ms slack per
 *     clock; a timing/expiry oracle (native ~1.5 s per instance).
 *   TimerfdTest.Periodic (x2) — 3 periods at 1 s plus slack (native
 *     ~3.5 s per instance).
 *   TimerfdTest.BlockingRead (x2) — asserts a read blocks a full 3 s.
 *   TimerfdTest.NonblockingRead (x2) — 5 s delay plus slack (~5.5 s).
 *   TimerfdTest.BlockingPoll_SetTimeResetsExpirations (x2) — 3 s poll.
 *   TimerfdTest.SetAbsoluteTime (x2) — 3 s delay plus slack.
 *   TimerfdTest.IllegalSeek (x2) — gVisor-only: SKIP_IF(
 *     !IsRunningOnGvisor()) with TODO b/298787679; Linux returns 0
 *     from lseek on a timerfd, gVisor expects ESPIPE.
 *   TimerfdClockRealtimeTest.ClockRealtime — 1 s realtime expiry
 *     counted against the host clock.
 *   TimerfdClockRealtimeTest.ClockAbsoluteRealtime — same, absolute.
 *   TimerfdClockRealtimeTest.ClockAbsoluteRealtimeCancelOnSet —
 *     gVisor-only (the original itself skips it on native Linux).
 * The owner may ask for stripped (shortened-delay) versions of the
 * timing cases separately; nothing here shortens an asserted delay.
 */

use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

/// Minimal stand-ins for gVisor's TimerfdCreate / ReadFd helpers
/// (ported helpers, not copied code).
struct Fd(OwnedFd);

impl Fd {
    fn raw(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }
}

fn timerfd_create(clock: i32, flags: i32) -> Fd {
    // SAFETY: timerfd_create with a clock id and flag bits; checked.
    let raw = unsafe { libc::timerfd_create(clock, flags) };
    assert!(
        raw >= 0,
        "timerfd_create failed: {}",
        std::io::Error::last_os_error()
    );
    Fd(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn zero_spec() -> libc::itimerspec {
    // SAFETY: itimerspec is plain data; zeroing is its disarmed value.
    unsafe { std::mem::zeroed() }
}

fn gettime(fd: &Fd) -> libc::itimerspec {
    let mut spec = zero_spec();
    // SAFETY: timerfd_gettime on a live fd with a valid out-spec.
    let rc = unsafe { libc::timerfd_gettime(fd.raw(), &mut spec) };
    check(rc == 0, "timerfd_gettime succeeds");
    spec
}

fn settime(fd: &Fd, flags: i32, spec: &libc::itimerspec) {
    // SAFETY: timerfd_settime on a live fd with a valid spec.
    let rc = unsafe { libc::timerfd_settime(fd.raw(), flags, spec, std::ptr::null_mut()) };
    check(rc == 0, "timerfd_settime succeeds");
}

fn read_u64(fd: &Fd) -> u64 {
    let mut value: u64 = 0;
    // SAFETY: read on a live fd into an 8-byte buffer.
    let rc = unsafe { libc::read(fd.raw(), &mut value as *mut u64 as *mut libc::c_void, 8) };
    check(rc == 8, "timerfd read returns 8 bytes");
    value
}

mod timerfd_test {
    use super::*;

    pub fn is_initially_stopped(clock: i32) {
        let tfd = timerfd_create(clock, 0);
        let spec = gettime(&tfd);
        check(
            spec.it_value.tv_sec == 0 && spec.it_value.tv_nsec == 0,
            "initially stopped",
        );
    }

    pub fn set_to_past_expires_eventually(clock: i32) {
        let tfd = timerfd_create(clock, 0);
        let mut spec = zero_spec();
        spec.it_value.tv_nsec = 1;
        settime(&tfd, libc::TFD_TIMER_ABSTIME, &spec);
        check(read_u64(&tfd) == 1, "past absolute expiry reads 1");
    }

    pub fn illegal_pread(clock: i32) {
        let tfd = timerfd_create(clock, 0);
        let mut value: i32 = 0;
        // SAFETY: pread on a live fd with a valid buffer.
        let rc =
            unsafe { libc::pread(tfd.raw(), &mut value as *mut i32 as *mut libc::c_void, 4, 0) };
        check(rc == -1 && errno() == libc::ESPIPE, "pread ESPIPE");
    }

    pub fn illegal_pwrite(clock: i32) {
        let tfd = timerfd_create(clock, 0);
        // SAFETY: pwrite on a live fd with a valid buffer.
        let rc = unsafe { libc::pwrite(tfd.raw(), b"x".as_ptr() as *const libc::c_void, 1, 0) };
        check(rc == -1 && errno() == libc::ESPIPE, "pwrite ESPIPE");
    }

    pub fn illegal_write(clock: i32) {
        let tfd = timerfd_create(clock, libc::TFD_NONBLOCK);
        let value: u64 = 0;
        // SAFETY: write on a live fd with a valid buffer.
        let rc = unsafe { libc::write(tfd.raw(), &value as *const u64 as *const libc::c_void, 8) };
        check(rc == -1 && errno() == libc::EINVAL, "write EINVAL");
    }
}

fn main() {
    let clocks: [(&str, i32); 2] = [
        ("CLOCK_MONOTONIC", libc::CLOCK_MONOTONIC),
        ("CLOCK_BOOTTIME", libc::CLOCK_BOOTTIME),
    ];
    type ClockCase = (&'static str, fn(i32));
    let cases: [ClockCase; 5] = [
        ("IsInitiallyStopped", timerfd_test::is_initially_stopped),
        (
            "SetToPastExpiresEventually",
            timerfd_test::set_to_past_expires_eventually,
        ),
        ("IllegalPread", timerfd_test::illegal_pread),
        ("IllegalPwrite", timerfd_test::illegal_pwrite),
        ("IllegalWrite", timerfd_test::illegal_write),
    ];
    let mut failures = 0;
    for (clock_name, clock) in clocks {
        for (case, test) in cases {
            let name = format!("TimerfdTest.{case}/{clock_name}");
            match std::panic::catch_unwind(AssertUnwindSafe(|| test(clock))) {
                Ok(()) => println!("{name}: PASS"),
                Err(_) => {
                    println!("{name}: FAIL");
                    failures += 1;
                }
            }
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
        }
    }
    println!("failures={failures}");
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
