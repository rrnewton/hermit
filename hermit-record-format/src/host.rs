/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The descriptor seam between the record/replay tools and the backend
//! that runs their guest: typed descriptor facts, descriptor
//! duplication into the tool process, and open-file-description
//! identity. Switching backend is switching the [`RecordHost`]
//! implementation. Descriptor classification, `fdinfo` and link
//! reads, descriptor `stat` guards, and every descriptor duplication
//! or identity comparison in the two tools go through this trait;
//! the reads that remain inline in the tools are named below.
//!
//! Every operation takes a borrowed [`DescriptorAccess`]: the
//! run-bound capability naming the guest process and descriptor
//! table in scope, plus the caller's [`Guest`] handle where one
//! exists. Fact queries are guest-bound and asynchronous, so an
//! implementation whose facts live behind the guest interface
//! (executor descriptor tables, injected probes) can answer without
//! ambient state. Synchronous operations (duplication, identity,
//! registry queries) take the same access: they also run where no
//! guest handle exists yet (tool construction, synchronous replay
//! registries), where the access is task-bound only, and a backend
//! with no host descriptor to duplicate refuses them by error.
//!
//! Still inline in the tools, exhaustively: guest-path string logic
//! (pure text, no host read), `/proc/<pid>/maps` and `mem` reads
//! (sendfile offset permissions, mmap capture, network copy paths),
//! `/proc/<pid>/limits` and `status` reads, the `/proc/<tid>/net/unix`
//! table read, process root/directory opens
//! (`record_replay_path`), the replayer `FICLONE` destination reopen
//! by its `/proc/<pid>/fd` path, the exec-restore descriptor scan,
//! the tool's own `/proc/self` descriptor reads, captured-output
//! identity matching over `stat` results this trait returns, and
//! replay materialization. A second [`RecordHost`] implementation
//! must route or refuse each of those for its backend before it
//! lands; none of them may silently fall back to procfs. The 6805
//! no-touch replay default (virtual files, memfd materialization)
//! must extend this seam with effect operations or gate at the
//! handlers; this trait covers queries and duplication only.
//!
//! Duplication contract: a host must duplicate the host-backed
//! descriptions whose offsets and effects callers then drive through
//! the duplicate (`lseek`, positioned writes, `set_len`, `sendfile`,
//! `SCM_RIGHTS` inspection) — regular files at minimum. Descriptions
//! a backend models itself with no host descriptor behind them
//! (captured pipes, eventfd, epoll, synthetic files) refuse
//! duplication by error, and each call site names that refusal
//! rather than serving a substitute descriptor.
//!
//! [`RecordHost::fd_link`]'s target strings are Linux's procfs
//! renderings (`anon_inode:[eventfd]`, `pipe:[…]`, an absolute path):
//! an implementation without procfs must synthesize identical
//! strings for the anonymous kinds, as typed classification covers
//! the rest.

use std::io;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::path::Path;
use std::path::PathBuf;

use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use serde::Deserialize;
use serde::Serialize;

/// What one guest descriptor is, as the tools need to tell apart.
/// Every other kind (pipes, sockets, directories, unreadable
/// descriptors) is [`DescriptorKind::Other`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorKind {
    /// An eventfd (`anon_inode:[eventfd]`).
    Eventfd,
    /// A signalfd (`anon_inode:[signalfd]`).
    Signalfd,
    /// A regular file.
    RegularFile,
    /// Anything else, or nothing could be read.
    Other,
}

/// A borrowed, run-bound capability for descriptor operations: the
/// guest process (`pid`) and descriptor-table owner (`table`) in
/// scope, plus the caller's guest handle where one exists.
///
/// Construction is explicit at each boundary:
/// [`DescriptorAccess::guest_bound`] in syscall handlers (the guest
/// handle is what an executor-backed implementation answers
/// through), [`DescriptorAccess::task`] at tool construction and in
/// synchronous registry paths that name a guest task but hold no
/// handle. There is no ambient or global access.
pub struct DescriptorAccess<'a, G> {
    pid: Pid,
    table: Pid,
    guest: Option<&'a mut G>,
}

impl<'a, G> DescriptorAccess<'a, G> {
    /// Access bound to guest process `pid` and descriptor table
    /// `table` only, with no guest handle: tool construction and
    /// synchronous registry paths.
    pub fn task(pid: Pid, table: Pid) -> Self {
        Self {
            pid,
            table,
            guest: None,
        }
    }

    /// The guest process this access is bound to.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// The guest task owning the descriptor table in scope.
    pub fn table(&self) -> Pid {
        self.table
    }

    /// The guest handle, when this access is guest-bound. Fact
    /// queries on a backend whose facts live behind the guest
    /// interface require it and refuse by their empty answer
    /// (`Other`/`None`/`false`) without one.
    pub fn guest_mut(&mut self) -> Option<&mut G> {
        self.guest.as_deref_mut()
    }

    /// Access bound to `guest`, for descriptor table `table` (the
    /// calling thread's table, which is not the leader's for a
    /// thread cloned without `CLONE_FILES`).
    pub fn guest_bound<T: Tool>(guest: &'a mut G, table: Pid) -> Self
    where
        G: Guest<T>,
    {
        let pid = guest.pid();
        Self {
            pid,
            table,
            guest: Some(guest),
        }
    }
}

/// Descriptor facts and host descriptor operations for one backend.
///
/// Implementations are zero-sized, statically dispatched namespaces:
/// a tool names its host as a type parameter (for example
/// `RecorderWithHost<PtraceHost>`), so no host value is stored or
/// migrated. Run context arrives per call as [`DescriptorAccess`].
pub trait RecordHost: Send + Sync + 'static {
    /// Classifies descriptor `fd` in the access's descriptor table.
    /// Asynchronous because a backend whose facts live in its
    /// executor may need to inject a probe (`Fstat`) to answer; the
    /// ptrace implementation reads `/proc` and never awaits.
    fn classify_fd<G: Guest<T>, T: Tool>(
        access: &mut DescriptorAccess<'_, G>,
        fd: RawFd,
    ) -> impl std::future::Future<Output = DescriptorKind> + Send;

    /// Whether descriptor `fd` in the access's table is an eventfd:
    /// the link-only predicate, for callers that need only this bit
    /// (a full classification would also stat on the ptrace path).
    fn fd_is_eventfd<G: Guest<T>, T: Tool>(
        access: &mut DescriptorAccess<'_, G>,
        fd: RawFd,
    ) -> impl std::future::Future<Output = bool> + Send;

    /// The descriptor's `fdinfo` text, if it can be read.
    /// Asynchronous for the same reason as [`Self::classify_fd`].
    fn fdinfo_text<G: Guest<T>, T: Tool>(
        access: &mut DescriptorAccess<'_, G>,
        fd: RawFd,
    ) -> impl std::future::Future<Output = Option<String>> + Send;

    /// The link target naming what descriptor `fd` in the access's
    /// table refers to, if it can be read. Synchronous for registry
    /// paths that hold no guest handle.
    fn fd_link<G>(access: &DescriptorAccess<'_, G>, fd: RawFd) -> Option<PathBuf>;

    /// The descriptor's `fdinfo` text by task; the synchronous
    /// counterpart of [`Self::fdinfo_text`] for paths that hold no
    /// guest handle.
    fn fdinfo_text_at<G>(access: &DescriptorAccess<'_, G>, fd: RawFd) -> Option<String>;

    /// `stat` of descriptor `fd` in the access's table, following
    /// its link as `stat` on the descriptor does, if it can be read.
    /// Synchronous: the guards that decide whether an effect through
    /// [`Self::duplicate_guest_fd`] applies read it in the same
    /// function as the effect.
    fn fd_metadata<G>(access: &DescriptorAccess<'_, G>, fd: RawFd) -> Option<std::fs::Metadata>;

    /// Duplicates guest descriptor `fd` of process `pid` into this
    /// process, preserving its open file description. Synchronous
    /// and task-keyed because it also runs at tool construction,
    /// before any guest handle exists; a backend with no host
    /// descriptor behind the guest's refuses by error (named
    /// refusal, never a silently wrong duplicate). The access names
    /// the run the duplicate belongs to.
    fn duplicate_guest_fd<G>(
        access: &DescriptorAccess<'_, G>,
        pid: Pid,
        fd: RawFd,
    ) -> io::Result<OwnedFd>;

    /// Duplicates descriptor `fd` from the table of guest thread
    /// `tid` in process `pid` (see
    /// [`crate::fd::duplicate_guest_thread_fd`]).
    fn duplicate_guest_thread_fd<G>(
        access: &DescriptorAccess<'_, G>,
        pid: Pid,
        tid: Pid,
        fd: RawFd,
    ) -> io::Result<OwnedFd>;

    /// Whether two descriptors held by this process share one open
    /// file description. Host-local (both descriptors are the
    /// tool's own), so it takes no access.
    fn same_open_file_description(left: RawFd, right: RawFd) -> io::Result<bool>;

    /// Whether descriptors `left` and `right` in the access's table
    /// share one open file description (Linux: `kcmp` `KCMP_FILE`
    /// between the task's descriptors). An error means the
    /// comparison could not be made, never that they differ.
    fn guest_fds_share_description<G>(
        access: &DescriptorAccess<'_, G>,
        left: RawFd,
        right: RawFd,
    ) -> io::Result<bool>;

    /// Visits the descriptor numbers open in the access process's
    /// table, in no promised order, stopping when `visit` returns
    /// `false` — lazily, so a caller searching for the first match
    /// pays only for the descriptors it inspects. `None` when the
    /// table cannot be listed.
    fn visit_descriptor_numbers<G>(
        access: &DescriptorAccess<'_, G>,
        visit: &mut dyn FnMut(RawFd) -> bool,
    ) -> Option<()>;

    /// Whether guest task `pid` still exists.
    fn guest_task_exists<G>(access: &DescriptorAccess<'_, G>, pid: Pid) -> bool;
}

/// The ptrace backend's host: the guest is host tasks, so queries read
/// the guest's own `/proc` entries and duplication goes through pidfd
/// (see [`crate::fd`]). This is the behaviour the tools had before the
/// trait existed, moved unchanged.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct PtraceHost;

/// The ptrace classification core: one `/proc/<pid>/fd/<fd>` path,
/// formatted once, serving the link read (eventfd, then signalfd,
/// early returns) and then the metadata read — the parent code's
/// single allocation, order, and early return.
fn classify_ptrace(pid: Pid, fd: RawFd) -> DescriptorKind {
    let path = format!("/proc/{}/fd/{fd}", pid.as_raw());
    if let Ok(target) = std::fs::read_link(&path) {
        if target == Path::new("anon_inode:[eventfd]") {
            return DescriptorKind::Eventfd;
        }
        if target == Path::new("anon_inode:[signalfd]") {
            return DescriptorKind::Signalfd;
        }
    }
    if std::fs::metadata(&path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_file())
    {
        DescriptorKind::RegularFile
    } else {
        DescriptorKind::Other
    }
}

/// Whether a descriptor link target is the eventfd rendering.
fn link_is_eventfd(target: &Path) -> bool {
    target == Path::new("anon_inode:[eventfd]")
}

impl RecordHost for PtraceHost {
    async fn classify_fd<G: Guest<T>, T: Tool>(
        access: &mut DescriptorAccess<'_, G>,
        fd: RawFd,
    ) -> DescriptorKind {
        classify_ptrace(access.table(), fd)
    }

    async fn fd_is_eventfd<G: Guest<T>, T: Tool>(
        access: &mut DescriptorAccess<'_, G>,
        fd: RawFd,
    ) -> bool {
        Self::fd_link(access, fd).is_some_and(|target| link_is_eventfd(&target))
    }

    async fn fdinfo_text<G: Guest<T>, T: Tool>(
        access: &mut DescriptorAccess<'_, G>,
        fd: RawFd,
    ) -> Option<String> {
        Self::fdinfo_text_at(access, fd)
    }

    fn fd_link<G>(access: &DescriptorAccess<'_, G>, fd: RawFd) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/fd/{fd}", access.table().as_raw())).ok()
    }

    fn fdinfo_text_at<G>(access: &DescriptorAccess<'_, G>, fd: RawFd) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", access.table().as_raw())).ok()
    }

    fn fd_metadata<G>(access: &DescriptorAccess<'_, G>, fd: RawFd) -> Option<std::fs::Metadata> {
        std::fs::metadata(format!("/proc/{}/fd/{fd}", access.table().as_raw())).ok()
    }

    fn duplicate_guest_fd<G>(
        _access: &DescriptorAccess<'_, G>,
        pid: Pid,
        fd: RawFd,
    ) -> io::Result<OwnedFd> {
        crate::fd::duplicate_guest_fd(pid, fd)
    }

    fn duplicate_guest_thread_fd<G>(
        _access: &DescriptorAccess<'_, G>,
        pid: Pid,
        tid: Pid,
        fd: RawFd,
    ) -> io::Result<OwnedFd> {
        crate::fd::duplicate_guest_thread_fd(pid, tid, fd)
    }

    fn same_open_file_description(left: RawFd, right: RawFd) -> io::Result<bool> {
        crate::fd::same_open_file_description(left, right)
    }

    fn guest_fds_share_description<G>(
        access: &DescriptorAccess<'_, G>,
        left: RawFd,
        right: RawFd,
    ) -> io::Result<bool> {
        const KCMP_FILE: libc::c_int = 0;
        // SAFETY: kcmp only compares two descriptor-table entries of
        // the named task.
        let comparison = unsafe {
            libc::syscall(
                libc::SYS_kcmp,
                access.table().as_raw(),
                access.table().as_raw(),
                KCMP_FILE,
                libc::c_long::from(left),
                libc::c_long::from(right),
            )
        };
        if comparison == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(comparison == 0)
        }
    }

    fn visit_descriptor_numbers<G>(
        access: &DescriptorAccess<'_, G>,
        visit: &mut dyn FnMut(RawFd) -> bool,
    ) -> Option<()> {
        let entries = std::fs::read_dir(format!("/proc/{}/fd", access.pid().as_raw())).ok()?;
        for entry in entries.flatten() {
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<RawFd>().ok())
            else {
                continue;
            };
            if !visit(fd) {
                break;
            }
        }
        Some(())
    }

    fn guest_task_exists<G>(_access: &DescriptorAccess<'_, G>, pid: Pid) -> bool {
        Path::new(&format!("/proc/{}", pid.as_raw())).exists()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;

    use super::*;

    fn own_pid() -> Pid {
        Pid::from_raw(std::process::id() as libc::pid_t)
    }

    #[test]
    fn ptrace_classify_reads_own_descriptors() {
        let pid = own_pid();

        let mut file = tempfile();
        file.write_all(b"x").unwrap();
        assert_eq!(
            classify_ptrace(pid, file.as_raw_fd()),
            DescriptorKind::RegularFile
        );

        // SAFETY: eventfd creates a new descriptor owned by this process.
        let eventfd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(eventfd >= 0, "eventfd: {}", io::Error::last_os_error());
        assert_eq!(classify_ptrace(pid, eventfd), DescriptorKind::Eventfd);
        // The typed eventfd predicate's link core agrees, link-only.
        let access = DescriptorAccess::<()>::task(pid, pid);
        assert!(link_is_eventfd(
            &PtraceHost::fd_link(&access, eventfd).unwrap()
        ));
        assert!(!link_is_eventfd(
            &PtraceHost::fd_link(&access, file.as_raw_fd()).unwrap()
        ));
        // SAFETY: eventfd is the descriptor just created.
        unsafe { libc::close(eventfd) };

        // SAFETY: an empty signal set is valid; signalfd creates a new
        // descriptor owned by this process.
        let signalfd = unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::signalfd(-1, &mask, libc::SFD_CLOEXEC)
        };
        assert!(signalfd >= 0, "signalfd: {}", io::Error::last_os_error());
        assert_eq!(classify_ptrace(pid, signalfd), DescriptorKind::Signalfd);
        // SAFETY: signalfd is the descriptor just created.
        unsafe { libc::close(signalfd) };

        let (reader, _writer) = std::io::pipe().unwrap();
        assert_eq!(
            classify_ptrace(pid, reader.as_raw_fd()),
            DescriptorKind::Other
        );
        assert_eq!(classify_ptrace(pid, -1), DescriptorKind::Other);
    }

    #[test]
    fn visit_descriptor_numbers_stops_at_first_reject() {
        let pid = own_pid();
        let access = DescriptorAccess::<()>::task(pid, pid);
        let mut visited = 0;
        let listed = PtraceHost::visit_descriptor_numbers(&access, &mut |_fd| {
            visited += 1;
            false
        });
        assert_eq!(listed, Some(()));
        assert_eq!(visited, 1);
    }

    fn tempfile() -> std::fs::File {
        let path = std::env::temp_dir().join(format!(
            "hermit-record-host-classify-{}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let _ = std::fs::remove_file(&path);
        file
    }
}
