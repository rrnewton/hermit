/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with signals.

use std::time::Duration;

use nix::sys::signal::Signal;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Timespec;
use tracing::info;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::tool_global::ResumeStatus;
use crate::tool_global::register_alarm;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::types::LogicalTime;

// NB: note kernel has different notation of sigaction, we cannot
// use libc's sigaction here unfortunately. See:
// https://elixir.bootlin.com/linux/latest/source/include/uapi/asm-generic/signal.h#L75
const KERNEL_SIGACTION_WORDS: usize = 4;
const SA_MASK_INDEX: usize = KERNEL_SIGACTION_WORDS - 1;
const KERNEL_SIGSET_SIZE: usize = std::mem::size_of::<u64>();

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-pending)
fn clear_reserved_signal(mask: u64) -> u64 {
    mask & !(1_u64 << (reverie::PERF_EVENT_SIGNAL as u32 - 1))
}

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
            if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                return Ok(guest.inject(call).await?);
            }
            let memory = guest.memory();
            let mut stack = guest.stack().await;
            let mut kernel_action: [u64; KERNEL_SIGACTION_WORDS] =
                memory.read_value(action.cast())?;
            kernel_action[SA_MASK_INDEX] = clear_reserved_signal(kernel_action[SA_MASK_INDEX]);
            let copied_action = stack.push(kernel_action);
            let _stack_guard = stack.commit()?;
            let modified_call = syscalls::RtSigaction::new()
                .with_signum(call.signum())
                .with_action(Some(copied_action.cast()))
                .with_old_action(call.old_action())
                .with_sigsetsize(call.sigsetsize());
            guest.inject(modified_call).await?
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
            if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                return Ok(guest.inject(call).await?);
            }
            let memory = guest.memory();
            let mut stack = guest.stack().await;
            let set_mask: u64 = memory.read_value(set.cast())?;
            let new_set = stack.push(clear_reserved_signal(set_mask));
            let _stack_guard = stack.commit()?;
            let modified_call = syscalls::RtSigprocmask::new()
                .with_how(call.how())
                .with_set(Some(new_set.cast()))
                .with_oldset(call.oldset())
                .with_sigsetsize(call.sigsetsize());
            // Using inject (intead of tail_inject) here so that
            // post_handler_hook can be called.
            Ok(guest.inject(modified_call).await?)
        } else {
            Ok(guest.inject(call).await?)
        }
    }

    /// Sends a process-directed signal through the kernel. The backend's signal
    /// event callback routes the selected delivery back through Detcore.
    pub async fn handle_kill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Kill,
    ) -> Result<i64, Error> {
        Ok(guest.inject(call).await?)
    }

    /// Sends a thread-directed signal through the kernel before Detcore
    /// schedules its delivery callback.
    pub async fn handle_tgkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tgkill,
    ) -> Result<i64, Error> {
        Ok(guest.inject(call).await?)
    }

    /// Handles the legacy thread-directed signal syscall.
    pub async fn handle_tkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tkill,
    ) -> Result<i64, Error> {
        Ok(guest.inject(call).await?)
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
        let dettid = guest.thread_state().dettid;

        let maybe_timeout = if let Some(timeout) = call.timeout() {
            let ts: Timespec = guest.memory().read_value(timeout)?;
            let ns_delta =
                Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64);
            let base_time = thread_observe_time(guest).await;
            let target_time = base_time + ns_delta;
            Some(target_time)
        } else {
            None
        };
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("rt_sigtimedwait");
        retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_sigset_clears_only_the_reserved_signal_bit() {
        let full = u64::MAX;
        let cleared = clear_reserved_signal(full);
        let reserved = 1_u64 << (reverie::PERF_EVENT_SIGNAL as u32 - 1);
        assert_eq!(cleared, full & !reserved);
        assert_eq!(KERNEL_SIGSET_SIZE, 8);
    }
}
