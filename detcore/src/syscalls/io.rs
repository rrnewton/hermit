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
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;
use tracing::debug;
use tracing::trace;

use crate::config::SchedHeuristic;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::ParsedTimeout;
use crate::syscalls::helpers::execute_internal_io_polling;
use crate::syscalls::helpers::millis_timeout;
use crate::syscalls::helpers::relative_timespec_timeout;
use crate::syscalls::helpers::relative_timeval_timeout;
use crate::tool_global::*;
use crate::tool_local::Detcore;

#[derive(Clone, Copy)]
#[repr(C)]
struct Pselect6SigmaskArg {
    sigmask: usize,
    sigsetsize: usize,
}

fn fd_set_exceeds_scratch_capacity(nfds: i32) -> bool {
    // The raw Linux ABI accepts dynamically sized fd sets, but Reverie's syscall model and our
    // retry scratch storage use libc's fixed-size fd_set.
    nfds > libc::FD_SETSIZE as i32
}

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
        let timeout = millis_timeout(guest, call.timeout()).await;
        execute_internal_io_polling(guest, call, timeout).await
    }

    /// ppoll syscall (MAYHANG)
    pub async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<i64, Error> {
        if !self.cfg.sequentialize_threads
            || self.cfg.recordreplay_modes
            || call.sigmask().is_some()
        {
            return self
                .record_or_replay_blocking(guest, Syscall::Ppoll(call))
                .await;
        }

        let timeout_addr = if let Some(addr) = call.timeout() {
            // Reverie currently types this ABI-compatible pointer as timeval, while Linux
            // ppoll interprets it as timespec.
            Some(AddrMut::<Timespec>::from_raw(addr.as_raw()).ok_or(Errno::EFAULT)?)
        } else {
            None
        };
        let timeout = if let Some(addr) = timeout_addr {
            relative_timespec_timeout(guest, Some(guest.memory().read_value(addr)?)).await?
        } else {
            ParsedTimeout::Infinite
        };
        let result = execute_internal_io_polling(guest, call, timeout).await;

        if let (Some(addr), ParsedTimeout::Deadline(deadline)) = (timeout_addr, timeout) {
            let now = thread_observe_time(guest).await;
            let remaining_nanos = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = Timespec {
                tv_sec: (remaining_nanos / 1_000_000_000) as i64,
                tv_nsec: (remaining_nanos % 1_000_000_000) as i64,
            };
            guest.memory().write_value(addr, &remaining)?;
        }

        result
    }

    /// select syscall (MAYHANG)
    pub async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
    ) -> Result<i64, Error> {
        if !self.cfg.sequentialize_threads
            || self.cfg.recordreplay_modes
            || fd_set_exceeds_scratch_capacity(call.nfds())
        {
            return self
                .record_or_replay_blocking(guest, Syscall::Select(call))
                .await;
        }

        let timeout_addr = call.timeout();
        let timeout = if let Some(addr) = timeout_addr {
            relative_timeval_timeout(guest, Some(guest.memory().read_value(addr)?)).await?
        } else {
            ParsedTimeout::Infinite
        };
        let result = execute_internal_io_polling(guest, call, timeout).await;

        if let (Some(addr), ParsedTimeout::Deadline(deadline)) = (timeout_addr, timeout) {
            let now = thread_observe_time(guest).await;
            let remaining_nanos = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = libc::timeval {
                tv_sec: (remaining_nanos / 1_000_000_000) as libc::time_t,
                tv_usec: ((remaining_nanos % 1_000_000_000) / 1_000) as libc::suseconds_t,
            };
            guest.memory().write_value(addr, &remaining)?;
        }

        result
    }

    /// pselect6 syscall (MAYHANG)
    pub async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        if !self.cfg.sequentialize_threads
            || self.cfg.recordreplay_modes
            || fd_set_exceeds_scratch_capacity(call.nfds())
        {
            return self
                .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                .await;
        }
        if let Some(addr) = call.sigmask() {
            let arg: Pselect6SigmaskArg = guest.memory().read_value(addr.cast())?;
            if arg.sigmask != 0 {
                return self
                    .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                    .await;
            }
        }

        let timeout_addr = if let Some(addr) = call.timeout() {
            // Reverie types this ABI-compatible pointer as timeval, while Linux
            // pselect6 interprets it as timespec.
            Some(AddrMut::<Timespec>::from_raw(addr.as_raw()).ok_or(Errno::EFAULT)?)
        } else {
            None
        };
        let timeout = if let Some(addr) = timeout_addr {
            relative_timespec_timeout(guest, Some(guest.memory().read_value(addr)?)).await?
        } else {
            ParsedTimeout::Infinite
        };
        let result = execute_internal_io_polling(guest, call, timeout).await;

        if let (Some(addr), ParsedTimeout::Deadline(deadline)) = (timeout_addr, timeout) {
            let now = thread_observe_time(guest).await;
            let remaining_nanos = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = Timespec {
                tv_sec: (remaining_nanos / 1_000_000_000) as i64,
                tv_nsec: (remaining_nanos % 1_000_000_000) as i64,
            };
            guest.memory().write_value(addr, &remaining)?;
        }

        result
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
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        // Register the epoll fd in the DetFd table like every other
        // fd-creating syscall (openat, eventfd2, pipe2, socket, ...). Without
        // this, later operations that consult the table via `with_detfd` /
        // `dup_fd` (F_GETFL, F_SETFD, F_DUPFD[_CLOEXEC], dup, ...) would fail
        // with EBADF even though the underlying kernel fd is valid. This broke,
        // for example, running rustup proxies (cargo/rustc) under hermit, whose
        // tokio runtime dups its epoll fd at startup.
        //
        // EPOLL_CLOEXEC shares the same bit value as O_CLOEXEC, so we can carry
        // the cloexec flag straight across.
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits()),
            FdType::Epoll,
        )
        .await?;
        Ok(fd as i64)
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
        let timeout = millis_timeout(guest, call.timeout()).await;
        execute_internal_io_polling(guest, call, timeout).await
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

    /// Handles all of: recvfrom, recvmsg, sendto, sendmsg, sendmmsg syscalls (MAYHANG)
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
}
