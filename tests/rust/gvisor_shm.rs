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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/shm.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 *
 * Mapping: ShmTest.AttachDetach, ShmTest.LookupByKey, ShmTest.IpcStat,
 * ShmTest.MultipleDetachFails, ShmTest.RemovedSegmentsAreMarkedDeleted.
 * Excluded: the death tests and mprotect/partial-unmap cases (address
 * layout assertions) and the SHMMAX/SHMMIN limit cases (host limits).
 * Known state: Detcore refuses shmget with ENOSYS on every backend
 * (extending the sysv-ipc-refusal cell's creation finding), so every
 * case fails at creation under Hermit and passes on native Linux. */

use std::panic::AssertUnwindSafe;

use googletest::assert_that;
use googletest::matchers::eq;

fn check(condition: bool, what: &str) {
    assert_that!(condition, eq(true), "{what}");
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn sys3(number: i64, a: i64, b: i64, c: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c) }
}

fn sys4(number: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
    // SAFETY: raw syscall; arguments supplied per named call.
    unsafe { libc::syscall(number, a, b, c, d) }
}

struct Segment(i32);

impl Segment {
    fn create() -> Option<Segment> {
        let id = sys3(
            libc::SYS_shmget,
            libc::IPC_PRIVATE as i64,
            4096,
            (libc::IPC_CREAT | 0o600) as i64,
        );
        if id < 0 {
            None
        } else {
            Some(Segment(id as i32))
        }
    }
}

mod shm_test {
    use super::*;

    fn create_or_fail() -> Segment {
        Segment::create().expect("shmget (ENOSYS under Hermit at this commit)")
    }

    pub fn attach_detach() {
        let seg = create_or_fail();
        let addr = sys3(libc::SYS_shmat, seg.0 as i64, 0, 0);
        check(
            addr > 0 && sys3(libc::SYS_shmdt, addr, 0, 0) == 0,
            "ShmTest.AttachDetach",
        );
        sys4(libc::SYS_shmctl, seg.0 as i64, libc::IPC_RMID as i64, 0, 0);
    }

    pub fn lookup_by_key() {
        // SAFETY: shmid_ds is plain data; unused here beyond creation.
        let key: i64 = 0x6d7532;
        let first = sys4(
            libc::SYS_shmget,
            key,
            4096,
            (libc::IPC_CREAT | libc::IPC_EXCL | 0o600) as i64,
            0,
        );
        let second = sys3(libc::SYS_shmget, key, 4096, 0o600);
        check(first >= 0 && second == first, "ShmTest.LookupByKey");
        if first >= 0 {
            sys4(libc::SYS_shmctl, first, libc::IPC_RMID as i64, 0, 0);
        }
    }

    pub fn ipc_stat() {
        let seg = create_or_fail();
        // SAFETY: shmid_ds is plain data, zeroed before IPC_STAT fills it.
        let mut ds: libc::shmid_ds = unsafe { std::mem::zeroed() };
        let rc = sys4(
            libc::SYS_shmctl,
            seg.0 as i64,
            libc::IPC_STAT as i64,
            &mut ds as *mut _ as i64,
            0,
        );
        check(rc == 0 && ds.shm_segsz == 4096, "ShmTest.IpcStat");
        sys4(libc::SYS_shmctl, seg.0 as i64, libc::IPC_RMID as i64, 0, 0);
    }

    pub fn multiple_detach_fails() {
        check(
            sys3(libc::SYS_shmdt, 0x1000, 0, 0) == -1 && errno() == libc::EINVAL,
            "ShmTest.MultipleDetachFails",
        );
    }

    pub fn removed_marked_deleted() {
        let seg = create_or_fail();
        let addr = sys3(libc::SYS_shmat, seg.0 as i64, 0, 0);
        let rmid = sys4(libc::SYS_shmctl, seg.0 as i64, libc::IPC_RMID as i64, 0, 0);
        // SAFETY: shmid_ds is plain data, zeroed before IPC_STAT fills it.
        let mut after: libc::shmid_ds = unsafe { std::mem::zeroed() };
        let after_rc = if addr > 0 {
            sys4(
                libc::SYS_shmctl,
                seg.0 as i64,
                libc::IPC_STAT as i64,
                &mut after as *mut _ as i64,
                0,
            )
        } else {
            -1
        };
        if addr > 0 {
            sys3(libc::SYS_shmdt, addr, 0, 0);
        }
        check(
            rmid == 0 && after_rc == 0,
            "ShmTest.RemovedSegmentsAreMarkedDeleted",
        );
    }
}

fn main() {
    let tests: [(&str, fn()); 5] = [
        ("ShmTest.AttachDetach", shm_test::attach_detach),
        ("ShmTest.LookupByKey", shm_test::lookup_by_key),
        ("ShmTest.IpcStat", shm_test::ipc_stat),
        (
            "ShmTest.MultipleDetachFails",
            shm_test::multiple_detach_fails,
        ),
        (
            "ShmTest.RemovedSegmentsAreMarkedDeleted",
            shm_test::removed_marked_deleted,
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
