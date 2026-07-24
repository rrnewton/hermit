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
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
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
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::nanos_duration_to_absolute_timeout;
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

    /// select syscall (MAYHANG).
    ///
    /// Determinized exactly like `poll`: instead of a real-time kernel block we
    /// convert to a zero-timeout probe and retry under the deterministic
    /// scheduler (`retry_nonblocking_syscall_with_timeout`), or hand truly
    /// external / record-replay cases to `record_or_replay_blocking`
    /// (BlockingExternalIO). `timeout` is a `*timeval`: NULL means block forever
    /// (no logical deadline), `{0,0}` (or nonpositive) means return immediately,
    /// and a positive value becomes an absolute logical deadline.
    pub async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
    ) -> Result<i64, Error> {
        let maybe_tv: Option<libc::timeval> = match call.timeout() {
            None => None,
            Some(p) => Some(guest.memory().read_value(p)?),
        };
        let is_zero = matches!(maybe_tv, Some(tv) if tv.tv_sec == 0 && tv.tv_usec == 0);

        if self.cfg.recordreplay_modes && is_zero {
            // Cannot block, but still yield a scheduler turn so a polling thread
            // cannot monopolize the guest between preemptions (parity w/ poll).
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::Select(call))
                .await?)
        } else {
            self.handle_internal_select(guest, call, maybe_tv).await
        }
    }

    /// Handle a guest-internal `select` call that can be fully determinized.
    pub async fn handle_internal_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
        maybe_tv: Option<libc::timeval>,
    ) -> Result<i64, Error> {
        // Immediate / already-nonblocking or invalid timeout: a single probe is
        // sufficient (the kernel returns 0 for {0,0} or EINVAL for a malformed
        // timeval), so there is nothing to poll-retry.
        let probe_once = match maybe_tv {
            Some(tv) => {
                (tv.tv_sec == 0 && tv.tv_usec == 0)
                    || tv.tv_sec < 0
                    || tv.tv_usec < 0
                    || tv.tv_usec >= 1_000_000
            }
            None => false,
        };
        if probe_once {
            return Ok(guest.inject(call).await?);
        }
        let maybe_timeout_ns = match maybe_tv {
            None => None, // NULL timeout: block (poll-retry) until a descriptor is ready.
            Some(tv) => {
                let nanos = (tv.tv_sec as u128) * 1_000_000_000 + (tv.tv_usec as u128) * 1_000;
                nanos_duration_to_absolute_timeout(guest, nanos).await
            }
        };
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("select");
        retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
    }

    /// pselect6 syscall (MAYHANG).
    ///
    /// Same determinization as `select`. NOTE: pselect6 atomically installs
    /// `sigmask` for the duration of the wait; the nonblockize-and-retry model
    /// applies the mask per probe rather than atomically across the whole logical
    /// wait, so a signal that an atomic pselect6 would have blocked could in
    /// principle be observed between probes. Detcore already serializes and
    /// determinizes signal delivery, and the retry loop honors `ResumeStatus::
    /// Signaled`, so this is a documented approximation rather than a
    /// nondeterminism source. reverie models the timeout as a `*timeval`.
    pub async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        let maybe_tv: Option<libc::timeval> = match call.timeout() {
            None => None,
            Some(p) => Some(guest.memory().read_value(p)?),
        };
        let is_zero = matches!(maybe_tv, Some(tv) if tv.tv_sec == 0 && tv.tv_usec == 0);

        if self.cfg.recordreplay_modes && is_zero {
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                .await?)
        } else {
            self.handle_internal_pselect6(guest, call, maybe_tv).await
        }
    }

    /// Handle a guest-internal `pselect6` call that can be fully determinized.
    pub async fn handle_internal_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        maybe_tv: Option<libc::timeval>,
    ) -> Result<i64, Error> {
        let probe_once = match maybe_tv {
            Some(tv) => {
                (tv.tv_sec == 0 && tv.tv_usec == 0)
                    || tv.tv_sec < 0
                    || tv.tv_usec < 0
                    || tv.tv_usec >= 1_000_000
            }
            None => false,
        };
        if probe_once {
            return Ok(guest.inject(call).await?);
        }
        let maybe_timeout_ns = match maybe_tv {
            None => None,
            Some(tv) => {
                let nanos = (tv.tv_sec as u128) * 1_000_000_000 + (tv.tv_usec as u128) * 1_000;
                nanos_duration_to_absolute_timeout(guest, nanos).await
            }
        };
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("pselect6");
        retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
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
