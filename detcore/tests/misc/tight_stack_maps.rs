/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Traced coverage for the `/proc/<pid>/maps` side of
//! <https://github.com/rrnewton/hermit/issues/3328>.
//!
//! With metadata virtualized, Detcore renders the maps inode column from the
//! identity `stat` reports for each mapped file. A file with no `mmap` record
//! is resolved by pathname through an injected `fstatat`, whose path and
//! `struct stat` buffers are staged in the guest's stack below `rsp`. The
//! ptrace backend writes that scratch when it commits it, so a guest whose
//! stack pointer sat close to the end of its writable stack saw its `read` of
//! the maps file fail with `EFAULT`. A scratch fault now sends the `fstatat`
//! to a transient page that Detcore maps and unmaps around it, so the line
//! still gets `stat`'s identity.
//!
//! The guest here runs as a forked child, so every file it has mapped was
//! mapped before tracing began, has no `mmap` record, and is resolved by
//! pathname. It issues the first `read` of `/proc/self/maps`, which takes the
//! snapshot, with its stack pointer a chosen number of bytes above memory it
//! cannot write, and requires what Linux gives natively: the file's contents,
//! with the executable's maps inode equal to the `st_ino` it reports. On a
//! filesystem whose maps header pair is not `stat`'s (btrfs, overlayfs) that
//! equality holds only if the pathname was resolved despite the fault.

use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;

use super::tight_stack_openat::BelowStack;
use super::tight_stack_openat::tight_stack;

const MAPS: &CStr = c"/proc/self/maps";

/// Issue `read(fd, buffer, buffer.len())` with `rsp` switched to `stack` for
/// the duration of the `syscall` instruction.
///
/// # Safety
///
/// `stack` must be a writable address; nothing is pushed to it, but a signal
/// delivered during the syscall would run its handler on it.
unsafe fn raw_read_on_stack(stack: *mut u8, fd: i32, buffer: &mut [u8]) -> i64 {
    let result: i64;
    // SAFETY: rsp is swapped with r12 and swapped back before the block ends;
    // `syscall` itself does not touch the user stack. rcx and r11 are the
    // registers `syscall` clobbers.
    unsafe {
        std::arch::asm!(
            "xchg rsp, r12",
            "syscall",
            "xchg rsp, r12",
            inout("r12") stack => _,
            inlateout("rax") libc::SYS_read => result,
            in("rdi") i64::from(fd),
            in("rsi") buffer.as_mut_ptr(),
            in("rdx") buffer.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// The inode column of the first maps line naming `path`.
fn maps_inode_of(contents: &str, path: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        (fields.len() > 5 && fields[5..].join(" ") == path)
            .then(|| fields[4].parse().expect("maps inode column"))
    })
}

#[test]
fn maps_read_succeeds_without_writable_stack_below_rsp() {
    super::det_test_fn_sequential_without_pmu(|| {
        let executable = std::env::current_exe().unwrap();
        let executable_inode = std::fs::metadata(&executable).unwrap().ino();
        let executable = executable.to_str().unwrap().to_owned();
        let mut buffer = vec![0_u8; 64 * 1024];

        // 1024 bytes holds the 128-byte red zone and the whole scratch area
        // and is the control; 192 is the probe value from the issue; 0 puts
        // rsp at the boundary.
        for (below, writable_bytes) in [
            (BelowStack::GuardPage, 1024),
            (BelowStack::GuardPage, 192),
            (BelowStack::GuardPage, 0),
            (BelowStack::Unmapped, 192),
        ] {
            let fd = unsafe { libc::open(MAPS.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            assert!(fd >= 0, "open of {MAPS:?} failed");
            let (stack, mapping, mapping_len) = tight_stack(below, writable_bytes);
            let first = unsafe { raw_read_on_stack(stack, fd, &mut buffer) };
            assert_eq!(unsafe { libc::munmap(mapping, mapping_len) }, 0);
            assert!(
                first > 0,
                "raw read of {MAPS:?} with {writable_bytes} writable bytes above a {below:?} \
                 region returned {first}; natively it returns the file's first bytes"
            );
            let mut contents = buffer[..usize::try_from(first).unwrap()].to_vec();
            loop {
                let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                assert!(read >= 0, "read of {MAPS:?} failed");
                if read == 0 {
                    break;
                }
                contents.extend_from_slice(&buffer[..usize::try_from(read).unwrap()]);
            }
            assert_eq!(unsafe { libc::close(fd) }, 0);

            let contents = String::from_utf8(contents).unwrap();
            let inode = maps_inode_of(&contents, &executable).unwrap_or_else(|| {
                panic!("{MAPS:?} read on a tight stack does not map {executable}:\n{contents}")
            });
            // Every case, the control included, must resolve the executable's
            // path: a faulting stack scratch falls back to a transient page,
            // not to the header's identity.
            assert_eq!(
                inode, executable_inode,
                "with {writable_bytes} writable bytes above a {below:?} region, the maps \
                 inode of {executable} must be the inode stat reports"
            );
        }
    });
}
