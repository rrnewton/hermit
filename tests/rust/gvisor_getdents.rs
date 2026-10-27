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
 * Port provenance: Rust port of gVisor's test/syscalls/linux/getdents.cc
 * at gVisor commit 012cf0b0b14d7b8aa2a4424867ecec8e6121e69d (2026-07-26),
 * from dev-hermit/experiments/gvisor. libc + googletest matchers only.
 * Calls go through SYS_getdents (the plain number glibc bypasses with
 * getdents64).
 *
 * Mapping, in the spirit of the original's directory cases:
 * ReaddirTest.OpenDir (a directory holding two named files lists
 * both), ReaddirTest.GoneAfterRemoveCache (after a removal a fresh
 * listing no longer names it), plus a too-small buffer (EINVAL) and
 * getdents on a regular file (ENOTDIR).
 * Excluded: the /proc, /dev and root-listing cases (host filesystem
 * contents, not a syscall contract) and the d_off cases (covered for
 * getdents64 by proc_fd_directory).
 * Known failure: the KVM executor answers getdents with ENOSYS:
 * https://github.com/rrnewton/hermit/issues/4849
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

fn listing_names(fd: i32) -> Vec<String> {
    let mut names = Vec::new();
    // SAFETY: lseek back to the start of the directory stream.
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
        return names;
    }
    let mut buf = [0u8; 1024];
    // SAFETY: getdents into a valid buffer.
    let n = unsafe { libc::syscall(libc::SYS_getdents, fd, buf.as_mut_ptr(), buf.len()) };
    if n <= 0 {
        return names;
    }
    let mut at = 0usize;
    while at < n as usize {
        // SAFETY: records are walked by their kernel-written reclen.
        let reclen = u16::from_ne_bytes(buf[at + 16..at + 18].try_into().unwrap()) as usize;
        if reclen == 0 {
            break;
        }
        let name_bytes = &buf[at + 18..at + reclen];
        let end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        names.push(String::from_utf8_lossy(&name_bytes[..end]).into_owned());
        at += reclen;
    }
    names
}

mod getdents_test {
    use super::*;

    fn setup() -> i32 {
        // SAFETY: mkdir/open with NUL-terminated paths.
        unsafe {
            libc::mkdir(cstr("dir").as_ptr(), 0o700);
            for name in ["dir/alpha", "dir/beta"] {
                let fd = libc::open(cstr(name).as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o600);
                if fd >= 0 {
                    libc::close(fd);
                }
            }
            libc::open(cstr("dir").as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
        }
    }

    pub fn open_dir() {
        let fd = setup();
        let names = listing_names(fd);
        check(
            fd >= 0 && names.iter().any(|n| n == "alpha") && names.iter().any(|n| n == "beta"),
            "ReaddirTest.OpenDir",
        );
        unsafe { libc::close(fd) };
    }

    pub fn gone_after_remove() {
        let fd = setup();
        // SAFETY: unlink of a guest-created path.
        unsafe { libc::unlink(cstr("dir/alpha").as_ptr()) };
        let names = listing_names(fd);
        check(
            fd >= 0 && !names.iter().any(|n| n == "alpha") && names.iter().any(|n| n == "beta"),
            "ReaddirTest.GoneAfterRemoveCache",
        );
        unsafe { libc::close(fd) };
    }

    pub fn small_buffer() {
        let fd = setup();
        // SAFETY: lseek + getdents into an 8-byte buffer.
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        let mut tiny = [0u8; 8];
        let rc = unsafe { libc::syscall(libc::SYS_getdents, fd, tiny.as_mut_ptr(), tiny.len()) };
        check(rc == -1 && errno() == libc::EINVAL, "getdents small buffer");
        unsafe { libc::close(fd) };
    }

    pub fn not_dir() {
        setup();
        // SAFETY: open of a guest-created regular file.
        let file = unsafe { libc::open(cstr("dir/beta").as_ptr(), libc::O_RDONLY) };
        let mut buf = [0u8; 256];
        let rc = if file >= 0 {
            unsafe { libc::syscall(libc::SYS_getdents, file, buf.as_mut_ptr(), buf.len()) }
        } else {
            -2
        };
        check(rc == -1 && errno() == libc::ENOTDIR, "getdents ENOTDIR");
        if file >= 0 {
            unsafe { libc::close(file) };
        }
    }
}

fn main() {
    let tests: [(&str, fn()); 4] = [
        ("ReaddirTest.OpenDir", getdents_test::open_dir),
        (
            "ReaddirTest.GoneAfterRemoveCache",
            getdents_test::gone_after_remove,
        ),
        ("getdents-small-buffer", getdents_test::small_buffer),
        ("getdents-notdir", getdents_test::not_dir),
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
        libc::unlink(cstr("dir/alpha").as_ptr());
        libc::unlink(cstr("dir/beta").as_ptr());
        libc::rmdir(cstr("dir").as_ptr());
    }
    println!("failures={failures}");
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
