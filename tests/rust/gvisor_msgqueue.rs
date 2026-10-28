/*
 * Copyright 2021 The gVisor Authors.
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
 * test/syscalls/linux/msgqueue.cc at gVisor commit
 * 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 *
 * Mapping, in the file's send/receive spirit over the same three
 * numbers: a msgsnd/msgrcv round trip with type and bytes intact,
 * msgctl(IPC_STAT), and msgctl(IPC_RMID). No blocking receive is used.
 * Known state: Detcore refuses msgget with ENOSYS on every backend,
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

#[repr(C)]
struct Message {
    mtype: i64,
    text: [u8; 16],
}

mod msgqueue_test {
    use super::*;

    fn create_or_fail() -> i32 {
        let id = sys3(
            libc::SYS_msgget,
            libc::IPC_PRIVATE as i64,
            (libc::IPC_CREAT | 0o600) as i64,
            0,
        );
        check(id >= 0, "msgget");
        id as i32
    }

    pub fn send_receive() {
        let id = create_or_fail();
        let out = Message {
            mtype: 42,
            text: *b"hello\0\0\0\0\0\0\0\0\0\0\0",
        };
        let sent = sys4(libc::SYS_msgsnd, id as i64, &out as *const _ as i64, 5, 0);
        // SAFETY: Message is repr(C) plain data, zeroed before receive.
        let mut incoming: Message = unsafe { std::mem::zeroed() };
        let got = sys5(
            libc::SYS_msgrcv,
            id as i64,
            &mut incoming as *mut _ as i64,
            16,
            42,
            libc::IPC_NOWAIT as i64,
        );
        check(
            sent == 0 && got == 5 && incoming.mtype == 42 && &incoming.text[..5] == b"hello",
            "Msgqueue.SendReceive",
        );
        sys3(libc::SYS_msgctl, id as i64, libc::IPC_RMID as i64, 0);
    }

    pub fn ipc_stat_and_remove() {
        let id = create_or_fail();
        // SAFETY: msqid_ds is plain data, zeroed before IPC_STAT.
        let mut ds: libc::msqid_ds = unsafe { std::mem::zeroed() };
        let rc = sys3(
            libc::SYS_msgctl,
            id as i64,
            libc::IPC_STAT as i64,
            &mut ds as *mut _ as i64,
        );
        check(rc == 0 && ds.msg_qbytes > 0, "Msgqueue.IpcStat");
        check(
            sys3(libc::SYS_msgctl, id as i64, libc::IPC_RMID as i64, 0) == 0,
            "Msgqueue.Remove",
        );
    }
}

fn main() {
    let tests: [(&str, fn()); 2] = [
        ("Msgqueue.SendReceive", msgqueue_test::send_receive),
        ("Msgqueue.IpcStat", msgqueue_test::ipc_stat_and_remove),
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
