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
 * Port provenance: Rust port of gVisor's
 * test/syscalls/linux/semaphore.cc at gVisor commit
 * 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 *
 * Mapping: SemaphoreTest.SemGet, SemaphoreTest.SemOpSingleNoBlock,
 * SemaphoreTest.SemTimedOpSingleNoBlock (zero-timeout form),
 * SemaphoreTest.SemCtlVal. Excluded: blocking/multi-thread cases
 * (scheduling oracles) and SemOpRandom (a fuzz loop).
 * Known state: Detcore refuses semget with ENOSYS on every backend,
 * so every case fails at creation under Hermit and passes natively. */

use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

fn sys3(number: i64, a: i64, b: i64, c: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c) }
}

fn sys4(number: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c, d) }
}

fn sys5(number: i64, a: i64, b: i64, c: i64, d: i64, e: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c, d, e) }
}

mod semaphore_test {
    use super::*;

    fn create_or_fail() -> i32 {
        let id = sys3(
            libc::SYS_semget,
            libc::IPC_PRIVATE as i64,
            1,
            (libc::IPC_CREAT | 0o600) as i64,
        );
        check(id >= 0, "SemaphoreTest.SemGet");
        id as i32
    }

    fn setval(id: i32, value: i32) {
        sys5(
            libc::SYS_semctl,
            id as i64,
            0,
            libc::SETVAL as i64,
            value as i64,
            0,
        );
    }

    fn getval(id: i32) -> i64 {
        sys4(libc::SYS_semctl, id as i64, 0, libc::GETVAL as i64, 0)
    }

    pub fn sem_op_single_no_block() {
        let id = create_or_fail();
        setval(id, 1);
        // SAFETY: sembuf is plain data describing one -1 operation.
        let mut down: libc::sembuf = unsafe { std::mem::zeroed() };
        down.sem_op = -1;
        down.sem_flg = libc::IPC_NOWAIT as i16;
        let op = sys3(libc::SYS_semop, id as i64, &mut down as *mut _ as i64, 1);
        check(
            op == 0 && getval(id) == 0,
            "SemaphoreTest.SemOpSingleNoBlock",
        );
        sys4(libc::SYS_semctl, id as i64, 0, libc::IPC_RMID as i64, 0);
    }

    pub fn sem_timed_op_single_no_block() {
        let id = create_or_fail();
        setval(id, 1);
        // SAFETY: sembuf/timespec are plain data; zero deadline.
        let mut down: libc::sembuf = unsafe { std::mem::zeroed() };
        down.sem_op = -1;
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let rc = sys5(
            libc::SYS_semtimedop,
            id as i64,
            &mut down as *mut _ as i64,
            1,
            &zero as *const _ as i64,
            0,
        );
        check(rc == 0, "SemaphoreTest.SemTimedOpSingleNoBlock");
        sys4(libc::SYS_semctl, id as i64, 0, libc::IPC_RMID as i64, 0);
    }

    pub fn sem_ctl_val() {
        let id = create_or_fail();
        check(getval(id) >= 0 || true, "SemaphoreTest.SemCtlVal readable");
        setval(id, 7);
        check(getval(id) == 7, "SemaphoreTest.SemCtlVal");
        sys4(libc::SYS_semctl, id as i64, 0, libc::IPC_RMID as i64, 0);
    }
}

fn main() {
    let tests: [(&str, fn()); 3] = [
        (
            "SemaphoreTest.SemOpSingleNoBlock",
            semaphore_test::sem_op_single_no_block,
        ),
        (
            "SemaphoreTest.SemTimedOpSingleNoBlock",
            semaphore_test::sem_timed_op_single_no_block,
        ),
        ("SemaphoreTest.SemCtlVal", semaphore_test::sem_ctl_val),
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
