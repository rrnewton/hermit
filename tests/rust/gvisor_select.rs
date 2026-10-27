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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/select.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26),
 * from dev-hermit/experiments/gvisor. libc + googletest matchers only.
 * Calls go through SYS_select (the plain number glibc bypasses with
 * pselect6).
 *
 * Mapping: SelectTest.NegativeNfds, SelectTest.ClosedFds,
 * SelectTest.ZeroTimeout, SelectTest.IgnoreBitsAboveNfds.
 * Excluded: NullFds and NoTimeout (blocking-sleep timing oracles),
 * InterruptedBySignal (interruption timing), SetrlimitCallNOFILE
 * (raises RLIMIT_NOFILE).
 */

use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn zero_tv() -> libc::timeval {
    libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    }
}

fn select_raw(nfds: i32, read: *mut libc::fd_set, tv: *mut libc::timeval) -> i64 {
    // SAFETY: select with caller-supplied (or null) sets and timeout.
    unsafe {
        libc::syscall(
            libc::SYS_select,
            nfds,
            read,
            std::ptr::null_mut::<libc::fd_set>(),
            std::ptr::null_mut::<libc::fd_set>(),
            tv,
        )
    }
}

fn pipe() -> (i32, i32) {
    let mut fds = [0i32; 2];
    // SAFETY: pipe into a two-element array.
    check(unsafe { libc::pipe(fds.as_mut_ptr()) } == 0, "pipe");
    (fds[0], fds[1])
}

fn fd_set_of(fd: i32) -> libc::fd_set {
    // SAFETY: fd_set is plain data; zero then set one valid fd.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        set
    }
}

mod select_test {
    use super::*;

    pub fn negative_nfds() {
        let mut tv = zero_tv();
        let rc = select_raw(-1, std::ptr::null_mut(), &mut tv);
        check(
            rc == -1 && errno() == libc::EINVAL,
            "SelectTest.NegativeNfds",
        );
    }

    pub fn closed_fds() {
        let (read_end, write_end) = pipe();
        let mut set = fd_set_of(read_end);
        // SAFETY: close of a live fd.
        unsafe { libc::close(read_end) };
        let mut tv = zero_tv();
        let rc = select_raw(read_end + 1, &mut set, &mut tv);
        check(rc == -1 && errno() == libc::EBADF, "SelectTest.ClosedFds");
        unsafe { libc::close(write_end) };
    }

    pub fn zero_timeout() {
        let (read_end, write_end) = pipe();
        let mut set = fd_set_of(read_end);
        let mut tv = zero_tv();
        let not_ready = select_raw(read_end + 1, &mut set, &mut tv);
        // SAFETY: write one byte to a live pipe.
        check(
            unsafe { libc::write(write_end, b"x".as_ptr() as *const libc::c_void, 1) } == 1,
            "write",
        );
        let mut set2 = fd_set_of(read_end);
        let mut tv2 = zero_tv();
        let ready = select_raw(read_end + 1, &mut set2, &mut tv2);
        // SAFETY: FD_ISSET on an fd_set select updated.
        let isset = unsafe { libc::FD_ISSET(read_end, &set2) };
        check(
            not_ready == 0 && ready == 1 && isset,
            "SelectTest.ZeroTimeout",
        );
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
    }

    pub fn ignore_bits_above_nfds() {
        let (read_end, write_end) = pipe();
        // SAFETY: write one byte to a live pipe.
        unsafe { libc::write(write_end, b"x".as_ptr() as *const libc::c_void, 1) };
        let mut set = fd_set_of(read_end);
        // SAFETY: setting a bit above nfds in a valid fd_set.
        unsafe {
            if read_end + 2 < libc::FD_SETSIZE as i32 {
                libc::FD_SET(read_end + 2, &mut set);
            }
        }
        let mut tv = zero_tv();
        let rc = select_raw(read_end + 1, &mut set, &mut tv);
        // SAFETY: FD_ISSET on an fd_set select updated.
        let isset = unsafe { libc::FD_ISSET(read_end, &set) };
        check(rc == 1 && isset, "SelectTest.IgnoreBitsAboveNfds");
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
    }
}

fn main() {
    let tests: [(&str, fn()); 4] = [
        ("SelectTest.NegativeNfds", select_test::negative_nfds),
        ("SelectTest.ClosedFds", select_test::closed_fds),
        ("SelectTest.ZeroTimeout", select_test::zero_timeout),
        (
            "SelectTest.IgnoreBitsAboveNfds",
            select_test::ignore_bits_above_nfds,
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
