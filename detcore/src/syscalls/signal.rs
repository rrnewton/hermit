/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with signals.

use nix::sys::signal::Signal;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Timespec;
use tracing::info;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::helpers::execute_internal_io_polling;
use crate::syscalls::helpers::relative_timespec_timeout;
use crate::tool_global::ResumeStatus;
use crate::tool_global::register_alarm;
use crate::tool_global::resource_request;
use crate::tool_global::send_signal;
use crate::types::DetPid;
use crate::types::LogicalTime;

// NB: note kernel has different notation of sigaction, we cannot
// use libc's sigaction here unfortunately. See:
// https://elixir.bootlin.com/linux/latest/source/include/uapi/asm-generic/signal.h#L75
const SA_MASK_OFFET: usize = 3 * std::mem::size_of::<u64>();

impl<T: RecordOrReplay> Detcore<T> {
    /// We send the alarms to the global scheduler to handle.
    pub async fn handle_alarm<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Alarm,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            let remaining = register_alarm(guest, call.seconds(), Signal::SIGALRM).await;
            Ok(remaining as i64)
        } else {
            info!(
                "[dtid {}] Running without scheduler, so letting alarm call through...",
                guest.thread_state().dettid
            );
            Ok(guest.inject(call).await?)
        }
    }

    /// A pause is really just an unbounded sleep.
    pub async fn handle_pause<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pause,
    ) -> Result<i64, Error> {
        if guest.config().sequentialize_threads {
            let req = Self::sleep_request_abs(guest, LogicalTime::from_nanos(u64::MAX)).await;
            match resource_request(guest, req).await {
                ResumeStatus::Normal => {
                    panic!(
                        "Internal violation: pause should never return from the scheduler except by interruption!"
                    )
                }
                ResumeStatus::Signaled => Err(reverie::Error::Errno(Errno::EINTR)),
            }
        } else {
            info!(
                "[dtid {}] Running without scheduler, so letting pause call through...",
                guest.thread_state().dettid
            );
            Ok(guest.inject(call).await?)
        }
    }

    /// Run rt_sigsuspend without holding the deterministic scheduler turn.
    ///
    /// The kernel must perform the temporary mask swap atomically and restore the
    /// original mask after signal delivery, so execute the real blocking syscall
    /// while marking this thread as blocked outside the runnable set.
    pub async fn handle_rt_sigsuspend<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigsuspend,
    ) -> Result<i64, Error> {
        self.record_or_replay_blocking(guest, call.into()).await
    }

    /// rt_sigaction
    pub async fn handle_rt_sigaction<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigaction,
    ) -> Result<i64, Error> {
        // PERF_EVENT_SIGNAL is reserved.
        if call.signum() == reverie::PERF_EVENT_SIGNAL as i32 {
            // The go runtime attempts to register this (unused) signal handler.  We will never
            // deliver signals of this kind to the guest, so we just turn this action into a noop
            // rather than returning `Err(Errno::EINVAL.into())`.
            return Ok(0);
        }
        Ok(if let Some(action) = call.action() {
            let mut memory = guest.memory();
            let sa_mask: AddrMut<libc::sigset_t> =
                AddrMut::from_raw(SA_MASK_OFFET + action.as_raw()).unwrap();
            let mut mask = memory.read_value(sa_mask)?;
            unsafe { libc::sigdelset(&mut mask as *mut _, reverie::PERF_EVENT_SIGNAL as i32) };
            memory.write_value(sa_mask, &mask)?;
            guest.inject(call).await?
        } else {
            guest.inject(call).await?
        })
    }

    /// rt_sigprocmask
    pub async fn handle_rt_sigprocmask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigprocmask,
    ) -> Result<i64, Error> {
        if call.how() != libc::SIG_BLOCK && call.how() != libc::SIG_SETMASK {
            Ok(guest.inject(call).await?)
        } else if let Some(set) = call.set() {
            let memory = guest.memory();
            let mut stack = guest.stack().await;
            let mut set_mask = memory.read_value(set)?;
            unsafe { libc::sigdelset(&mut set_mask as *mut _, reverie::PERF_EVENT_SIGNAL as i32) };
            let new_set = stack.push(set_mask);
            stack.commit()?;
            let modified_call = syscalls::RtSigprocmask::new()
                .with_how(call.how())
                .with_set(Some(new_set))
                .with_oldset(call.oldset())
                .with_sigsetsize(call.sigsetsize());
            // Using inject (intead of tail_inject) here so that
            // post_handler_hook can be called.
            Ok(guest.inject(modified_call).await?)
        } else {
            Ok(guest.inject(call).await?)
        }
    }

    /// getpid: return the deterministic virtual process id (thread-group leader).
    pub async fn handle_getpid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _call: syscalls::Getpid,
    ) -> Result<i64, Error> {
        let detpid = guest
            .thread_state()
            .detpid
            .unwrap_or_else(|| guest.thread_state().dettid);
        Ok(detpid.as_raw() as i64)
    }

    /// gettid: return the deterministic virtual thread id.
    pub async fn handle_gettid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _call: syscalls::Gettid,
    ) -> Result<i64, Error> {
        Ok(guest.thread_state().dettid.as_raw() as i64)
    }

    /// kill: deliver a process-directed signal.
    pub async fn handle_kill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Kill,
    ) -> Result<i64, Error> {
        let raw_pid = call.pid();
        // Determine the target process. pid > 0 targets that process; pid == 0
        // targets the caller's own process group, which within a single Hermit
        // container we approximate as the caller's own process. Broadcast and
        // arbitrary process-group targets (pid < 0) are not modeled
        // deterministically yet.
        let own_pid = guest
            .thread_state()
            .detpid
            .unwrap_or_else(|| guest.thread_state().dettid);
        let target_pid = if raw_pid > 0 {
            DetPid::from_raw(raw_pid)
        } else if raw_pid == 0 {
            own_pid
        } else {
            info!(
                "[dtid {}] kill({}, {}): process-group/broadcast signals are not yet modeled deterministically",
                guest.thread_state().dettid,
                raw_pid,
                call.sig()
            );
            return Err(Error::Errno(Errno::ENOSYS));
        };
        self.deliver_kill_family(guest, target_pid, None, call.sig())
            .await
    }

    /// tkill: deliver a thread-directed signal to a single thread.
    pub async fn handle_tkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tkill,
    ) -> Result<i64, Error> {
        let target_tid = DetPid::from_raw(call.tid());
        // For a thread-directed signal the "process" argument is only used as a
        // fallback; pass the target thread's own id.
        self.deliver_kill_family(guest, target_tid, Some(target_tid), call.sig())
            .await
    }

    /// tgkill: deliver a thread-directed signal to a thread in a thread group.
    pub async fn handle_tgkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tgkill,
    ) -> Result<i64, Error> {
        let target_pid = DetPid::from_raw(call.tgid());
        let target_tid = DetPid::from_raw(call.tid());
        self.deliver_kill_family(guest, target_pid, Some(target_tid), call.sig())
            .await
    }

    /// Shared implementation for kill/tkill/tgkill: validate the signal number
    /// and route delivery through the deterministic scheduler.
    async fn deliver_kill_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target_pid: DetPid,
        m_dettid: Option<DetPid>,
        raw_sig: i32,
    ) -> Result<i64, Error> {
        if !guest.config().sequentialize_threads {
            // Without the scheduler engaged there is nothing to route through;
            // preserve the prior best-effort behavior.
            info!(
                "[dtid {}] kill-family signal {} to {} passed through (scheduler disabled)",
                guest.thread_state().dettid,
                raw_sig,
                target_pid
            );
            return Err(Error::Errno(Errno::ENOSYS));
        }
        // A signal number of 0 performs no delivery; it only checks that the
        // target exists. We optimistically report success.
        if raw_sig == 0 {
            return Ok(0);
        }
        let signal = Signal::try_from(raw_sig).map_err(|_| Error::Errno(Errno::EINVAL))?;
        let delivered = send_signal(guest, target_pid, m_dettid, signal).await;
        if delivered {
            Ok(0)
        } else {
            Err(Error::Errno(Errno::ESRCH))
        }
    }

    /// rt_sigtimedwait system call
    ///
    /// This is handled by the scheduler and not passed to the record/replay layer,
    /// because currently signals are not recorded.
    pub async fn handle_rt_sigtimedwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigtimedwait,
    ) -> Result<i64, Error> {
        let timeout = if let Some(timeout) = call.timeout() {
            let timespec: Timespec = guest.memory().read_value(timeout)?;
            relative_timespec_timeout(guest, Some(timespec)).await?
        } else {
            relative_timespec_timeout(guest, None).await?
        };
        execute_internal_io_polling(guest, call, timeout).await
    }
}
