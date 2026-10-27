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
 * Port provenance: this is a one-to-one Rust port of the gVisor syscall
 * acceptance tests in test/syscalls/linux/eventfd.cc at gVisor commit
 * 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26), from the checkout
 * at dev-hermit/experiments/gvisor. Dependencies: libc and googletest
 * (matchers only), both already used by this workspace; no gVisor code,
 * submodule, or build is involved. googletest is used through its
 * assert_that!/eq-style matchers inside this guest's own runner (the
 * libtest harness is not used, so the printed output stays the
 * deterministic "Case: PASS" stream Hermit cells compare).
 *
 * Per-test mapping (original -> here, same name, same assertions):
 *   EventfdTest.Nonblock       -> eventfd_test::nonblock
 *   EventfdTest.BlockingWrite  -> eventfd_test::blocking_write
 *   EventfdTest.SmallWrite     -> eventfd_test::small_write
 *   EventfdTest.SmallRead      -> eventfd_test::small_read
 *   EventfdTest.IllegalPread   -> eventfd_test::illegal_pread
 *   EventfdTest.IllegalPwrite  -> eventfd_test::illegal_pwrite
 *   EventfdTest.BigWriteFails  -> eventfd_test::big_write_fails
 *   EventfdTest.BigRead        -> eventfd_test::big_read
 * Excluded, with reasons:
 *   EventfdTest.IllegalSeek — gVisor-only: the original is
 *     SKIP_IF(!IsRunningOnGvisor()) with TODO b/298787679 (fails on
 *     6.0+ kernels): Linux returns 0 from lseek on an eventfd, gVisor
 *     expects ESPIPE. Porting it would assert a non-Linux result.
 *   EventfdTest.NotifyNonZero — long-running: a 5 s sleep thread
 *     against a 10 s epoll timeout (measured native runtime ~5 s).
 *     The owner may ask for a stripped version separately.
 *   EventfdTest.SpliceReturnsEINVAL — gVisor-only marker
 *     (SKIP_IF(!IsRunningOnGvisor())); splice has its own Hermit
 *     cells, so this file keeps to the eventfd numbers.
 * Descriptor creation mirrors the original's NewEventFD (eventfd2
 * through libc), so the plain eventfd number is not what this port
 * exercises; the coverage it adds is the eventfd area's behaviour.
 */

use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

/// Minimal stand-in for gVisor's FileDescriptor (ported helper).
struct Fd(File);

impl Fd {
    fn raw(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }
    fn read_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.0.read_exact(&mut buf).expect("read u64");
        u64::from_ne_bytes(buf)
    }
    fn write_u64(&mut self, value: u64) {
        self.0.write_all(&value.to_ne_bytes()).expect("write u64");
    }
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
}

fn new_event_fd(count: u32, flags: i32) -> Fd {
    // SAFETY: eventfd2 with a plain count and flag bits; checked.
    let raw = unsafe { libc::syscall(libc::SYS_eventfd2, count, flags) };
    assert!(
        raw >= 0,
        "eventfd2 failed: {}",
        std::io::Error::last_os_error()
    );
    Fd(unsafe { File::from_raw_fd(raw as i32) })
}

/// SyscallFailsWithErrno, as a googletest assertion on the raw errno.
fn expect_errno<T>(result: std::io::Result<T>, errno: i32, what: &str) {
    match result {
        Ok(_) => panic!("{what}: expected errno {errno}, got success"),
        Err(error) => assert_that!(error.raw_os_error(), eq(Some(errno)), "{what}"),
    }
}

mod eventfd_test {
    use super::*;

    pub fn nonblock() {
        let mut efd = new_event_fd(0, libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE);
        let mut buf = [0u8; 8];
        expect_errno(efd.read(&mut buf), libc::EAGAIN, "empty nonblock read");
        efd.write_u64(1);
        assert_that!(efd.read_u64(), eq(1));
        expect_errno(efd.read(&mut buf), libc::EAGAIN, "drained nonblock read");
    }

    pub fn blocking_write() {
        let efd = new_event_fd(0, libc::EFD_SEMAPHORE);
        let raw = efd.raw();
        let reader = std::thread::spawn(move || {
            for _ in 0..3 {
                let mut buf = [0u8; 8];
                // SAFETY: raw names the same eventfd, kept open by the
                // parent until join below.
                let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, 8) };
                assert_that!(n, eq(8));
            }
        });
        let mut efd = efd;
        for _ in 0..3 {
            efd.write_u64(1);
        }
        reader.join().expect("reader thread");
    }

    pub fn small_write() {
        let mut efd = new_event_fd(0, libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE);
        let value: u64 = 16;
        expect_errno(
            efd.write(&value.to_ne_bytes()[..4]),
            libc::EINVAL,
            "4-byte write",
        );
    }

    pub fn small_read() {
        let mut efd = new_event_fd(0, libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE);
        efd.write_u64(1);
        let mut buf = [0u8; 8];
        expect_errno(efd.read(&mut buf[..4]), libc::EINVAL, "4-byte read");
    }

    pub fn illegal_pread() {
        let efd = new_event_fd(0, 0);
        let mut value: i32 = 0;
        // SAFETY: pread on a live fd with a valid buffer.
        let rc =
            unsafe { libc::pread(efd.raw(), &mut value as *mut i32 as *mut libc::c_void, 4, 0) };
        assert_that!(rc, eq(-1));
        assert_that!(
            std::io::Error::last_os_error().raw_os_error(),
            eq(Some(libc::ESPIPE))
        );
    }

    pub fn illegal_pwrite() {
        let efd = new_event_fd(0, 0);
        // SAFETY: pwrite on a live fd with a valid buffer.
        let rc = unsafe { libc::pwrite(efd.raw(), b"x".as_ptr() as *const libc::c_void, 1, 0) };
        assert_that!(rc, eq(-1));
        assert_that!(
            std::io::Error::last_os_error().raw_os_error(),
            eq(Some(libc::ESPIPE))
        );
    }

    pub fn big_write_fails() {
        // The host kernel here is 7.1, so the 6.9+ behaviour the
        // original gates on applies without a version probe.
        let mut efd = new_event_fd(0, libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE);
        let mut big = [0u64; 16];
        big[0] = 16;
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(big.as_ptr() as *const u8, std::mem::size_of_val(&big))
        };
        expect_errno(efd.write(bytes), libc::EINVAL, "128-byte write");
    }

    pub fn big_read() {
        let mut efd = new_event_fd(0, libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE);
        efd.write_u64(1);
        let mut big = [0u64; 16];
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(big.as_mut_ptr() as *mut u8, std::mem::size_of_val(&big))
        };
        // The original asserts the read succeeds and big[0] is the
        // counter; an eventfd read returns one 8-byte counter, not the
        // whole buffer.
        let n = efd.read(bytes).expect("big read");
        assert_that!(n, eq(8));
        assert_that!(big[0], eq(1));
    }
}

fn main() {
    let tests: [(&str, fn()); 8] = [
        ("EventfdTest.Nonblock", eventfd_test::nonblock),
        ("EventfdTest.BlockingWrite", eventfd_test::blocking_write),
        ("EventfdTest.SmallWrite", eventfd_test::small_write),
        ("EventfdTest.SmallRead", eventfd_test::small_read),
        ("EventfdTest.IllegalPread", eventfd_test::illegal_pread),
        ("EventfdTest.IllegalPwrite", eventfd_test::illegal_pwrite),
        ("EventfdTest.BigWriteFails", eventfd_test::big_write_fails),
        ("EventfdTest.BigRead", eventfd_test::big_read),
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
