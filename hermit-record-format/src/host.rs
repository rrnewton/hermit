/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The host queries and descriptor operations the record/replay tools
//! perform outside the guest: what a guest descriptor is, and holding a
//! duplicate of it in the tool process. Switching backend is switching
//! the [`RecordHost`] implementation; syscall handlers call only this
//! trait for these operations and contain no backend conditionals.
//!
//! Slice scope: descriptor classification queries and descriptor
//! duplication/identity. Guest-path string logic, `/proc/<pid>/maps`
//! and limits reads, captured-output identity, and replay
//! materialization still live in the tools themselves and move behind
//! this trait with the KVM implementation.

use std::io;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::path::PathBuf;

use reverie::Pid;
use serde::Deserialize;
use serde::Serialize;

/// Host-side descriptor queries and operations for one backend.
///
/// Implementations are stateless namespaces: every method is associated,
/// and a tool names its host as a type parameter (for example
/// `Recorder<PtraceHost>`), so no host value is stored or migrated.
pub trait RecordHost: Send + Sync + 'static {
    /// Duplicates guest descriptor `fd` of process `pid` into this
    /// process, preserving its open file description.
    fn duplicate_guest_fd(pid: Pid, fd: RawFd) -> io::Result<OwnedFd>;

    /// Duplicates descriptor `fd` from the table of guest thread `tid`
    /// in process `pid` (see [`crate::fd::duplicate_guest_thread_fd`]).
    fn duplicate_guest_thread_fd(pid: Pid, tid: Pid, fd: RawFd) -> io::Result<OwnedFd>;

    /// Whether two descriptors held by this process share one open
    /// file description.
    fn same_open_file_description(left: RawFd, right: RawFd) -> io::Result<bool>;

    /// The link target naming what guest descriptor `fd` of process
    /// `pid` refers to (Linux: `/proc/<pid>/fd/<fd>`), if it can be read.
    fn fd_link(pid: Pid, fd: RawFd) -> Option<PathBuf>;

    /// Whether guest descriptor `fd` of process `pid` refers to a
    /// regular file.
    fn fd_is_regular_file(pid: Pid, fd: RawFd) -> bool;

    /// The descriptor's `fdinfo` text, if it can be read.
    fn fdinfo_text(pid: Pid, fd: RawFd) -> Option<String>;
}

/// The ptrace backend's host: the guest is host tasks, so queries read
/// the guest's own `/proc` entries and duplication goes through pidfd
/// (see [`crate::fd`]). This is the behaviour the tools had before the
/// trait existed, moved unchanged.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct PtraceHost;

impl RecordHost for PtraceHost {
    fn duplicate_guest_fd(pid: Pid, fd: RawFd) -> io::Result<OwnedFd> {
        crate::fd::duplicate_guest_fd(pid, fd)
    }

    fn duplicate_guest_thread_fd(pid: Pid, tid: Pid, fd: RawFd) -> io::Result<OwnedFd> {
        crate::fd::duplicate_guest_thread_fd(pid, tid, fd)
    }

    fn same_open_file_description(left: RawFd, right: RawFd) -> io::Result<bool> {
        crate::fd::same_open_file_description(left, right)
    }

    fn fd_link(pid: Pid, fd: RawFd) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()
    }

    fn fd_is_regular_file(pid: Pid, fd: RawFd) -> bool {
        std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw()))
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
    }

    fn fdinfo_text(pid: Pid, fd: RawFd) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", pid.as_raw())).ok()
    }
}
