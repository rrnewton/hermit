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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/inotify.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26).
 * This file was translated from C++ to Rust and modified: the
 * googletest harness is replaced by a small built-in runner using
 * googletest matchers (assert_that!), operating-system calls go
 * through the libc crate, and the cases named as excluded below
 * are omitted. libc + googletest matchers only.
 * The descriptor is created through SYS_inotify_init (the plain number
 * glibc bypasses with inotify_init1).
 *
 * Mapping: Inotify.AddWatchOnInvalidFdFails (EBADF),
 * Inotify.CreatingFileGeneratesEvents (IN_CREATE naming the created
 * file, read after creation so no wait is a timing oracle).
 * Inotify.IllegalSeek is excluded: gVisor expects ESPIPE there but
 * Linux returns 0 from lseek on an inotify descriptor. The
 * deletion/rename/move event-ordering cases are excluded (covered for
 * the exercised number by inotify_watch / inotify_rename_cookies) and
 * ConcurrentFileDeletionAndWatchRemoval is a race by construction.
 * Known failure: the KVM executor answers inotify_init with ENOSYS:
 * https://github.com/rrnewton/hermit/issues/4852
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

mod inotify_test {
    use super::*;

    fn create() -> i32 {
        // SAFETY: inotify_init takes no arguments.
        let fd = unsafe { libc::syscall(libc::SYS_inotify_init) };
        check(fd >= 0, "inotify_init");
        fd as i32
    }

    pub fn add_watch_on_invalid_fd_fails() {
        // SAFETY: inotify_add_watch on an invalid fd with a valid path.
        let rc = unsafe { libc::inotify_add_watch(-1, cstr(".").as_ptr(), libc::IN_CREATE) };
        check(
            rc == -1 && errno() == libc::EBADF,
            "Inotify.AddWatchOnInvalidFdFails",
        );
    }

    pub fn creating_file_generates_events() {
        let fd = create();
        // SAFETY: mkdir/open/add_watch with NUL-terminated paths.
        unsafe {
            libc::mkdir(cstr("watched").as_ptr(), 0o700);
        }
        let wd = unsafe { libc::inotify_add_watch(fd, cstr("watched").as_ptr(), libc::IN_CREATE) };
        let created = unsafe {
            libc::open(
                cstr("watched/newfile").as_ptr(),
                libc::O_CREAT | libc::O_WRONLY,
                0o600,
            )
        };
        if created >= 0 {
            unsafe { libc::close(created) };
        }
        let mut buf = [0u8; 272];
        // SAFETY: read of one inotify_event plus a name into the buffer.
        let got = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        let mask = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
        let name = std::ffi::CStr::from_bytes_until_nul(&buf[16..])
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        check(
            wd >= 0 && got >= 16 && (mask & libc::IN_CREATE) != 0 && name == "newfile",
            "Inotify.CreatingFileGeneratesEvents",
        );
        unsafe {
            libc::unlink(cstr("watched/newfile").as_ptr());
            libc::rmdir(cstr("watched").as_ptr());
            libc::close(fd);
        }
    }
}

fn main() {
    let tests: [(&str, fn()); 2] = [
        (
            "Inotify.AddWatchOnInvalidFdFails",
            inotify_test::add_watch_on_invalid_fd_fails,
        ),
        (
            "Inotify.CreatingFileGeneratesEvents",
            inotify_test::creating_file_generates_events,
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
