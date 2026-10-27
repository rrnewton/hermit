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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/mq.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26),
 * from dev-hermit/experiments/gvisor. libc + googletest matchers only.
 *
 * Mapping: MqTest.Open, MqTest.CreateAlreadyExists (EEXIST),
 * MqTest.NoQueueExists (ENOENT), MqTest.ReadEmpty (timed receive with
 * a zero deadline), and one timed send/receive pair in the same
 * spirit. Queue names carry no leading slash (glibc strips it before
 * the syscall) and are unlinked on every path.
 * Excluded: Mount cases (privilege), OpenAsFile/UnlinkAsFile (procfs
 * spellings), ChangeIpcNamespace (namespace privilege), access-mode
 * cases (caller credentials the container root does not reproduce).
 * Known state: Detcore refuses mq_open/mq_unlink with ENOSYS on every
 * backend (measured 2026-10-27), so every case fails under Hermit and
 * passes on native Linux. */

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

fn sys5(number: i64, a: i64, b: i64, c: i64, d: i64, e: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c, d, e) }
}

const NAME: &str = "gvisor-mq-batch2";

fn cname() -> std::ffi::CString {
    std::ffi::CString::new(NAME).unwrap()
}

fn open_queue(excl: bool) -> i64 {
    // SAFETY: mq_attr is plain data describing a small queue.
    let mut attr: libc::mq_attr = unsafe { std::mem::zeroed() };
    attr.mq_maxmsg = 4;
    attr.mq_msgsize = 32;
    let flags = libc::O_CREAT | libc::O_RDWR | if excl { libc::O_EXCL } else { 0 };
    sys5(
        libc::SYS_mq_open,
        cname().as_ptr() as i64,
        flags as i64,
        0o600,
        &attr as *const _ as i64,
        0,
    )
}

mod mq_test {
    use super::*;

    pub fn open() {
        sys2(libc::SYS_mq_unlink, cname().as_ptr() as i64, 0);
        let queue = open_queue(true);
        check(queue >= 0, "MqTest.Open");
        if queue >= 0 {
            // SAFETY: close of a live descriptor.
            unsafe { libc::close(queue as i32) };
        }
        sys2(libc::SYS_mq_unlink, cname().as_ptr() as i64, 0);
    }

    pub fn create_already_exists() {
        sys2(libc::SYS_mq_unlink, cname().as_ptr() as i64, 0);
        let queue = open_queue(true);
        let again = open_queue(true);
        check(
            queue >= 0 && again == -1 && errno() == libc::EEXIST,
            "MqTest.CreateAlreadyExists",
        );
        if queue >= 0 {
            unsafe { libc::close(queue as i32) };
        }
        sys2(libc::SYS_mq_unlink, cname().as_ptr() as i64, 0);
    }

    pub fn no_queue_exists() {
        let missing = std::ffi::CString::new("gvisor-mq-batch2-missing").unwrap();
        let rc = sys4(
            libc::SYS_mq_open,
            missing.as_ptr() as i64,
            libc::O_RDWR as i64,
            0,
            0,
        );
        check(rc == -1 && errno() == libc::ENOENT, "MqTest.NoQueueExists");
    }

    pub fn timed_send_receive_and_read_empty() {
        sys2(libc::SYS_mq_unlink, cname().as_ptr() as i64, 0);
        let queue = open_queue(true);
        check(queue >= 0, "MqTest.TimedSendReceive open");
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let sent = sys5(
            libc::SYS_mq_timedsend,
            queue,
            b"x".as_ptr() as i64,
            1,
            1,
            &zero as *const _ as i64,
        );
        let mut byte = [0u8; 32];
        let mut prio: u32 = 0;
        let got = if sent == 0 {
            sys5(
                libc::SYS_mq_timedreceive,
                queue,
                byte.as_mut_ptr() as i64,
                32,
                &mut prio as *mut _ as i64,
                &zero as *const _ as i64,
            )
        } else {
            -1
        };
        check(
            sent == 0 && got == 1 && byte[0] == b'x',
            "MqTest.TimedSendReceive",
        );
        let mut buf = [0u8; 32];
        let empty = sys5(
            libc::SYS_mq_timedreceive,
            queue,
            buf.as_mut_ptr() as i64,
            32,
            &mut prio as *mut _ as i64,
            &zero as *const _ as i64,
        );
        check(
            empty == -1 && (errno() == libc::EAGAIN || errno() == libc::ETIMEDOUT),
            "MqTest.ReadEmpty",
        );
        if queue >= 0 {
            unsafe { libc::close(queue as i32) };
        }
        check(
            sys2(libc::SYS_mq_unlink, cname().as_ptr() as i64, 0) == 0,
            "MqTest.Unlink",
        );
    }
}

fn main() {
    let tests: [(&str, fn()); 4] = [
        ("MqTest.Open", mq_test::open),
        ("MqTest.CreateAlreadyExists", mq_test::create_already_exists),
        ("MqTest.NoQueueExists", mq_test::no_queue_exists),
        (
            "MqTest.TimedSendReceive",
            mq_test::timed_send_receive_and_read_empty,
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
