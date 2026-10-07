/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run a command against a private copy of the host mount table.
//!
//! Hermit does not make a changing filesystem deterministic, and AGENTS.md
//! requires a test that needs full isolation to provide a stable filesystem.
//! Detcore passes every host mount row except the ephemeral xarfuse seed rows
//! through to a guest's `/proc/self/mounts`. A guest that compares two opens
//! of that file, or a `--verify` run that compares two executions, therefore
//! observes any host mount or unmount that lands in between. On a shared
//! development host that is routine: a 20-minute sample on devbig014 on
//! 2026-10-06 saw a per-user runtime tmpfs and other tests' bind mounts appear
//! and disappear every 1 to 40 seconds.
//!
//! [`freeze_mount_table`] moves the command into a new user and mount
//! namespace before it executes. The user namespace maps only the caller's own
//! effective user and group IDs onto themselves, so file ownership and
//! permission checks are unchanged. Marking `/` recursively private stops host
//! mount events from propagating into the copy, so the command and everything
//! it starts see the mount table as it was when the command started.

use std::ffi::CStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;

/// Arrange for `command` to run in a private, unchanging copy of the mount
/// table. Apply this after any helper that rebuilds the command, because a
/// rebuilt `Command` does not keep its pre-exec hooks.
pub fn freeze_mount_table(command: &mut Command) {
    // SAFETY: geteuid and getegid cannot fail and have no preconditions.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    // Format the maps before the fork: the hook runs between fork and exec,
    // where it must not allocate.
    let uid_map = format!("{uid} {uid} 1\n").into_bytes();
    let gid_map = format!("{gid} {gid} 1\n").into_bytes();
    // SAFETY: the hook runs in the forked child before exec. It calls only
    // unshare, open, write, close and mount, which are async-signal-safe, and
    // it reads buffers that were allocated before the fork.
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(io::Error::last_os_error());
            }
            // An unprivileged process may write its own gid_map only after
            // it has given up setgroups(2).
            write_proc_file(c"/proc/self/setgroups", b"deny")?;
            write_proc_file(c"/proc/self/uid_map", &uid_map)?;
            write_proc_file(c"/proc/self/gid_map", &gid_map)?;
            // A mount namespace created from a less privileged user namespace
            // turns shared mounts into slaves, which still receive host
            // events; only private mounts stop them.
            if libc::mount(
                ptr::null(),
                c"/".as_ptr(),
                ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                ptr::null(),
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Write `contents` to a procfs control file in one write(2), as the kernel
/// requires for uid_map and gid_map. Async-signal-safe: it does not allocate.
fn write_proc_file(path: &CStr, contents: &[u8]) -> io::Result<()> {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is open for writing and `contents` is a live buffer of
    // `contents.len()` bytes.
    let written = unsafe { libc::write(fd, contents.as_ptr().cast(), contents.len()) };
    let result = if written < 0 {
        Err(io::Error::last_os_error())
    } else if usize::try_from(written) != Ok(contents.len()) {
        Err(io::Error::from_raw_os_error(libc::EIO))
    } else {
        Ok(())
    };
    // SAFETY: `fd` was opened above and is closed exactly once.
    unsafe { libc::close(fd) };
    result
}
