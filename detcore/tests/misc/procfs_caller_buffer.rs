/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A sanitized procfs read changes only the bytes it returns.
//!
//! Detcore captures the host file by reading it into the caller's own buffer,
//! then hands the caller the sanitized bytes. `/proc/modules` sanitizes to an
//! empty file (https://github.com/rrnewton/hermit/issues/3815), so the first
//! read returns 0, and the host module table the capture wrote into the buffer
//! must not still be there. Each test uses a fresh descriptor, so its call is
//! the one that performs the capture. The write-only cases cover a destination
//! the kernel may write but a tracer's protection-respecting read cannot see.

use std::fs::File;
use std::os::fd::AsRawFd;

/// Larger than the host `/proc/modules` on a typical machine, so the capture
/// takes the whole file in one read and would leave all of it behind.
const LEN: usize = 64 * 1024;
const SENTINEL: u8 = 0xa5;

/// The leak can only show when the host file has contents to leave behind.
fn require_host_modules() {
    let host = std::fs::read("/proc/modules").unwrap();
    assert!(
        !host.is_empty(),
        "this host has no loaded modules, so the capture writes nothing and \
         these tests cannot detect a leak; run them on a host with modules"
    );
}

fn assert_untouched(buf: &[u8], call: &str) {
    if let Some(at) = buf.iter().position(|&byte| byte != SENTINEL) {
        let end = buf.len().min(at + 64);
        panic!(
            "{call} of the empty /proc/modules returned 0 but changed the caller's buffer at \
             byte {at}: {:?}",
            String::from_utf8_lossy(&buf[at..end])
        );
    }
}

#[test]
fn procfs_read_leaves_no_host_bytes_in_the_caller_buffer() {
    require_host_modules();
    super::det_test_fn_without_pmu(|| {
        let file = File::open("/proc/modules").unwrap();
        let mut buf = vec![SENTINEL; LEN];
        let n = unsafe { libc::read(file.as_raw_fd(), buf.as_mut_ptr().cast(), LEN) };
        assert_eq!(n, 0);
        assert_untouched(&buf, "read");
    });
}

#[test]
fn procfs_pread_leaves_no_host_bytes_in_the_caller_buffer() {
    require_host_modules();
    super::det_test_fn_without_pmu(|| {
        let file = File::open("/proc/modules").unwrap();
        let mut buf = vec![SENTINEL; LEN];
        let n = unsafe { libc::pread(file.as_raw_fd(), buf.as_mut_ptr().cast(), LEN, 0) };
        assert_eq!(n, 0);
        assert_untouched(&buf, "pread");
    });
}

/// A short read into a page the guest made PROT_WRITE-only. The kernel's read
/// may write it, but `process_vm_readv` cannot read it, so a save of the
/// caller's buffer would come back empty.
const WRITE_ONLY_COUNT: usize = 8;

fn assert_write_only_destination_untouched(
    call: &str,
    read: impl FnOnce(libc::c_int, *mut libc::c_void) -> isize,
) {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    unsafe { std::ptr::write_bytes(mapping.cast::<u8>(), SENTINEL, page) };
    assert_eq!(
        unsafe { libc::mprotect(mapping, page, libc::PROT_WRITE) },
        0
    );
    let file = File::open("/proc/modules").unwrap();
    let n = read(file.as_raw_fd(), mapping);
    assert_eq!(
        unsafe { libc::mprotect(mapping, page, libc::PROT_READ | libc::PROT_WRITE) },
        0
    );
    let buf = unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), page) };
    assert_eq!(n, 0);
    assert_untouched(buf, call);
    assert_eq!(unsafe { libc::munmap(mapping, page) }, 0);
}

#[test]
fn procfs_read_leaves_no_host_bytes_in_a_write_only_buffer() {
    require_host_modules();
    super::det_test_fn_without_pmu(|| {
        assert_write_only_destination_untouched("write-only read", |fd, buf| unsafe {
            libc::read(fd, buf, WRITE_ONLY_COUNT)
        });
    });
}

#[test]
fn procfs_pread_leaves_no_host_bytes_in_a_write_only_buffer() {
    require_host_modules();
    super::det_test_fn_without_pmu(|| {
        assert_write_only_destination_untouched("write-only pread", |fd, buf| unsafe {
            libc::pread(fd, buf, WRITE_ONLY_COUNT, 0)
        });
    });
}
