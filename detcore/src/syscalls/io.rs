/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with IO and networking.
//!
//! Of course this overlaps somewhat with "files.rs".

use std::os::unix::io::RawFd;
use std::time::Duration;

use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::config::SchedHeuristic;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::tool_global::*;
use crate::tool_local::Detcore;

// Printing helper
// TODO: this should be subsumed by better syscall printing.
fn print_poll(call: &syscalls::Poll) {
    let len = call.nfds();
    debug!("POLL: on {} fds, timeout {}", len, call.timeout());
    // TODO: nicer API for reading arrays from the guest:
    unsafe {
        for i in 0..len {
            debug!(
                "POLL: fd {} = {}",
                i,
                call.fds().unwrap().offset(i as isize)
            );
        }
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// poll syscall (MAYHANG)
    pub async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,

        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // In replay mode, we cannot assume the existence of FILES during replay.
            // Thus we must record the poll and replay it from the trace.
            Ok(self.handle_external_poll(guest, call).await?)
        } else {
            // TODO:
            // if is-external-poll { self.handle_external_poll(guest, call) }
            self.handle_internal_poll(guest, call).await
        }
    }

    /// Handle a guest-internal poll call that can be fully determinized.
    pub async fn handle_internal_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            Ok(guest.inject(call).await?) // Already non-blocking.
        } else {
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("poll");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
        }
    }

    /// Handle a poll syscall that deponds on external, nondeterminstic IO.
    pub async fn handle_external_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let len = call.nfds();
        let time_delta = Duration::from_millis(call.timeout() as u64);

        if len == 0 && time_delta.is_zero() {
            let request = Self::sleep_request(guest, time_delta).await;
            resource_request(guest, request).await;
            Ok(0)
        } else {
            print_poll(&call);
            Ok(self
                .record_or_replay_blocking(guest, Syscall::Poll(call))
                .await?)
        }
    }

    /// epoll_create1 syscall
    pub async fn handle_epoll_create1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCreate1,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// epoll_ctl syscall
    pub async fn handle_epoll_ctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCtl,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// epoll_pwait syscall (MAYHANG)
    pub async fn handle_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// epoll_wait syscall (MAYHANG)
    pub async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::EpollWait(call))
                .await?)
        } else {
            self.handle_internal_epoll_wait(guest, call).await
        }
    }

    /// Handle a guest-internal `epoll_wait` call that can be fully determinized.
    pub async fn handle_internal_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            Ok(guest.inject(call).await?) // Already non-blocking.
        } else {
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("epoll_wait");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
        }
    }

    /// Connect system call (MAYHANG)
    /// Note that connect waits until a TCP handshake but does not wait for accept() on the other end.
    /// Nevertheless, it can block for a long time while waiting for connection, unless the socket
    /// is already nonblocking.
    pub async fn handle_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing connect");
            let resource = ResourceID::PriorityChangePoint(
                FIRST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }

        self.execute_nonblockable_fd_syscall(guest, call).await
    }

    /// Handles all of: recvfrom, sendto, sendmsg, sendmmsg syscalls (MAYHANG)
    ///
    /// `recvmsg` has its own wrapper ([`Self::handle_recvmsg`]) because it can
    /// carry `SCM_RIGHTS` ancillary file descriptors that Detcore must register.
    pub async fn handle_sendrecv<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
    ) -> Result<i64, Error> {
        self.execute_nonblockable_fd_syscall(guest, call).await
    }

    /// Handles `recvmsg` (MAYHANG).
    ///
    /// A `recvmsg` on a Unix domain socket can carry `SCM_RIGHTS` ancillary
    /// messages, which hand the receiving process brand-new file descriptors
    /// (this is how, for example, the QEMU vhost-user protocol passes shared
    /// memory `memfd`s and socket endpoints). Those descriptors are created by
    /// the kernel as a side effect of the syscall, so — like the fds returned by
    /// `accept`, `socket`, or `pipe` — Detcore must add them to its per-thread
    /// [`crate::fd::DetFd`] table. Without this, the very next operation on a
    /// received fd (e.g. `read`) fails with `EBADF` because
    /// [`crate::tool_local`]'s `with_detfd` cannot find it.
    pub async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Recvmsg,
    ) -> Result<i64, Error> {
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        if result >= 0 {
            self.register_received_fds(guest, call).await?;
        }
        Ok(result)
    }

    /// Parse the `msg_control` buffer written by a completed `recvmsg` and
    /// register every descriptor delivered via an `SCM_RIGHTS` control message
    /// in Detcore's fd table.
    async fn register_received_fds<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Recvmsg,
    ) -> Result<(), Error> {
        let msg_addr = match call.msg() {
            Some(addr) => addr,
            None => return Ok(()),
        };
        // `AddrMut<msghdr>` -> read the (kernel-updated) header back out.
        let header: libc::msghdr = guest.memory().read_value(msg_addr)?;
        if header.msg_control.is_null() || header.msg_controllen == 0 {
            return Ok(());
        }

        let control_len = header.msg_controllen;
        let control_addr =
            Addr::<u8>::from_raw(header.msg_control as usize).ok_or(Errno::EFAULT)?;
        let mut control = vec![0u8; control_len];
        guest.memory().read_exact(control_addr, &mut control)?;

        for fd in scm_rights_fds(&control) {
            self.register_received_fd(guest, fd).await?;
        }
        Ok(())
    }

    /// Register a single `SCM_RIGHTS`-received descriptor.
    async fn register_received_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Error> {
        if guest.config().recordreplay_modes {
            // In record/replay mode the descriptor's byte-level behavior is
            // reconstructed from the recorded event stream, so record and replay
            // must take the *same* Detcore code path for every later syscall on
            // this fd. We therefore register it generically rather than probing
            // the live kernel object: during replay the recorded fd number is
            // not a real kernel descriptor, so an `fstat` would fail and, worse,
            // classifying by type would diverge from the recording. `Regular`
            // routes reads/writes through `record_or_replay`, matching how the
            // recorder captured them.
            guest
                .thread_state()
                .add_fd(fd, OFlag::empty(), FdType::Regular, None)?;
            return Ok(());
        }

        // Plain `hermit run`: the received descriptor is a live kernel object.
        // Classify it so later syscalls are routed correctly, and reject types
        // we do not know how to model deterministically.
        let stat = self.inject_fstat(guest, fd).await?;
        let ty = received_fd_type(stat.st_mode).ok_or_else(|| {
            warn!(
                "recvmsg received an SCM_RIGHTS fd {} of unsupported type (st_mode {:#o}); \
                 only regular files, memfds, sockets, pipes, and character devices are supported",
                fd, stat.st_mode
            );
            Errno::EOPNOTSUPP
        })?;
        let det_stat = guest.config().virtualize_metadata.then(|| stat.into());
        guest
            .thread_state()
            .add_fd(fd, OFlag::empty(), ty, det_stat)?;
        Ok(())
    }
}

/// Extract the file descriptors carried by every `SOL_SOCKET`/`SCM_RIGHTS`
/// control message in a `msg_control` buffer.
///
/// This walks the `cmsghdr` chain manually rather than using the libc `CMSG_*`
/// macros on a borrowed buffer so it can operate on bytes copied out of guest
/// memory. Partially-delivered fd arrays (possible when `MSG_CTRUNC` is set) are
/// handled by only reading whole `RawFd`s that fit within `cmsg_len`.
fn scm_rights_fds(control: &[u8]) -> Vec<RawFd> {
    let hdr_size = std::mem::size_of::<libc::cmsghdr>();
    let align = std::mem::align_of::<libc::cmsghdr>();
    let mut fds = Vec::new();
    let mut offset = 0usize;

    while offset + hdr_size <= control.len() {
        // SAFETY: we only read a `cmsghdr` worth of bytes that lie within the
        // buffer, and `cmsghdr` is a plain-old-data C struct.
        let mut cmsg: libc::cmsghdr = unsafe { std::mem::zeroed() };
        unsafe {
            std::ptr::copy_nonoverlapping(
                control[offset..].as_ptr(),
                &mut cmsg as *mut libc::cmsghdr as *mut u8,
                hdr_size,
            );
        }

        let cmsg_len = cmsg.cmsg_len as usize;
        // A well-formed control message covers at least its own header and does
        // not run past the end of the buffer.
        if cmsg_len < hdr_size || offset + cmsg_len > control.len() {
            break;
        }

        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            let data_start = offset + cmsg_align(hdr_size, align);
            let data_end = offset + cmsg_len;
            let mut cursor = data_start;
            while cursor + std::mem::size_of::<RawFd>() <= data_end {
                let mut raw = [0u8; std::mem::size_of::<RawFd>()];
                raw.copy_from_slice(&control[cursor..cursor + std::mem::size_of::<RawFd>()]);
                fds.push(RawFd::from_ne_bytes(raw));
                cursor += std::mem::size_of::<RawFd>();
            }
        }

        // Advance to the next header, honoring the kernel's cmsg alignment.
        let advance = cmsg_align(cmsg_len, align).max(cmsg_align(hdr_size, align));
        offset += advance;
    }

    fds
}

/// Round `len` up to the control-message alignment, matching `CMSG_ALIGN`.
fn cmsg_align(len: usize, align: usize) -> usize {
    len.div_ceil(align) * align
}

/// Map a received descriptor's `st_mode` to the Detcore [`FdType`] used to route
/// its syscalls, or `None` for a type we do not support passing via
/// `SCM_RIGHTS`.
fn received_fd_type(st_mode: libc::mode_t) -> Option<FdType> {
    match st_mode & libc::S_IFMT {
        // Regular files and memfds are both `S_IFREG`; a memfd is just an
        // anonymous regular file, and both route through `record_or_replay`.
        libc::S_IFREG => Some(FdType::Regular),
        libc::S_IFSOCK => Some(FdType::Socket),
        libc::S_IFIFO => Some(FdType::Pipe),
        libc::S_IFCHR => Some(FdType::Regular),
        _ => None,
    }
}
