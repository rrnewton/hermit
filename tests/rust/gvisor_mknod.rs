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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/mknod.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 * Calls go through SYS_mknod.
 *
 * Mapping: MknodTest.RegularFile, MknodTest.RegularFilePermissions
 * (with the umask cleared, as the original's fixture arranges),
 * MknodTest.FifoDoesNotRequireCapMknod,
 * MknodTest.MknodOnExistingPathFails.
 * Excluded: CharacterDeviceRequiresCapMknod and
 * BlockDeviceRequiresCapMknod (CAP_MKNOD behaviour guests are not
 * given), MknodAtFIFO and MknodAtEmptyPath (mknodat, already
 * exercised), Socket (a socket node is the EINVAL family covered by
 * the existing-path case here).
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

fn cstr(path: &str) -> std::ffi::CString {
    std::ffi::CString::new(path).unwrap()
}

fn mknod(path: &str, mode: u32) -> i64 {
    // SAFETY: mknod with a NUL-terminated path; dev_t 0 for non-device.
    unsafe { libc::syscall(libc::SYS_mknod, cstr(path).as_ptr(), mode, 0) }
}

fn mode_of(path: &str) -> Option<u32> {
    // SAFETY: stat into a zeroed buffer with a NUL-terminated path.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(cstr(path).as_ptr(), &mut st) != 0 {
            return None;
        }
        Some(st.st_mode)
    }
}

mod mknod_test {
    use super::*;

    pub fn regular_file() {
        check(
            mknod("regular", libc::S_IFREG | 0o600) == 0
                && mode_of("regular").is_some_and(|m| (m & libc::S_IFMT) == libc::S_IFREG),
            "MknodTest.RegularFile",
        );
    }

    pub fn regular_file_permissions() {
        // SAFETY: umask takes the new mask; clearing it makes the mode
        // land exactly, as the original's umask arrangement requires.
        unsafe { libc::umask(0) };
        check(
            mknod("perms", libc::S_IFREG | 0o640) == 0
                && mode_of("perms").is_some_and(|m| (m & 0o777) == 0o640),
            "MknodTest.RegularFilePermissions",
        );
    }

    pub fn fifo_does_not_require_cap_mknod() {
        check(
            mknod("fifo", libc::S_IFIFO | 0o600) == 0
                && mode_of("fifo").is_some_and(|m| (m & libc::S_IFMT) == libc::S_IFIFO),
            "MknodTest.FifoDoesNotRequireCapMknod",
        );
    }

    pub fn mknod_on_existing_path_fails() {
        let rc = mknod("fifo", libc::S_IFIFO | 0o600);
        check(
            rc == -1 && errno() == libc::EEXIST,
            "MknodTest.MknodOnExistingPathFails",
        );
    }
}

fn main() {
    let tests: [(&str, fn()); 4] = [
        ("MknodTest.RegularFile", mknod_test::regular_file),
        (
            "MknodTest.RegularFilePermissions",
            mknod_test::regular_file_permissions,
        ),
        (
            "MknodTest.FifoDoesNotRequireCapMknod",
            mknod_test::fifo_does_not_require_cap_mknod,
        ),
        (
            "MknodTest.MknodOnExistingPathFails",
            mknod_test::mknod_on_existing_path_fails,
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
    // SAFETY: cleanup of guest-created paths.
    unsafe {
        libc::unlink(cstr("regular").as_ptr());
        libc::unlink(cstr("perms").as_ptr());
        libc::unlink(cstr("fifo").as_ptr());
    }
    println!("failures={failures}");
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
