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

use std::time::Duration;

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
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
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
        if call.nfds() < 0 {
            return Ok(self.record_or_replay(guest, call).await?);
        }

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
        if call.nfds() < 0 {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        if !self.cfg.sequentialize_threads
            || self.cfg.recordreplay_modes
            || fd_set_exceeds_scratch_capacity(call.nfds())
        {
            return self
                .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                .await;
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
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
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
        // Capture the output buffer before `call` is consumed by the poll loop.
        let events = call.events();
        let timeout = millis_timeout(guest, call.timeout()).await;
        let ready = execute_internal_io_polling(guest, call, timeout).await?;
        if ready > 0 && let Some(events) = events {
            Self::determinize_epoll_events(guest, events, ready as usize)?;
        }
        Ok(ready)
    }

    /// Impose a deterministic order on the `epoll_event`s the kernel wrote into
    /// the guest's output buffer.
    ///
    /// The kernel returns ready events in an unspecified order that depends on
    /// host timing and the internal ready-list state of the epoll instance;
    /// `epoll_wait(2)` guarantees nothing about ordering, so reordering the
    /// results is always valid for a conforming program. Hermit must
    /// nonetheless emit identical bytes on every run, so we sort the returned
    /// slice by the caller-registered `data` value — the key an application
    /// attaches with `epoll_ctl`, conventionally the fd — using the event mask
    /// as a tiebreaker. Sorting on `data` rather than on any host-dependent
    /// quantity keeps the order stable across runs and hosts.
    fn determinize_epoll_events<G: Guest<Self>>(
        guest: &mut G,
        events: AddrMut<libc::epoll_event>,
        count: usize,
    ) -> Result<(), Error> {
        if count <= 1 {
            return Ok(());
        }
        let mut buf = vec![libc::epoll_event { events: 0, u64: 0 }; count];
        let mut memory = guest.memory();
        memory.read_values(events.into(), &mut buf)?;
        buf.sort_by_key(|event| {
            // `libc::epoll_event` is `#[repr(packed)]`, so copy the fields out
            // by value rather than taking references to unaligned storage.
            let data = event.u64;
            let mask = event.events;
            (data, mask)
        });
        memory.write_values(events, &buf)?;
        Ok(())
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
