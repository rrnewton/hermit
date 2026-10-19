/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The Linux policy for a `select` or `pselect6` that sleeps in the kernel on
//! a backend whose own signals interrupt injected waits
//! (`BackendCapabilities::restarts_backend_interrupted_waits`, DBT;
//! <https://github.com/rrnewton/hermit/issues/4483>).
//!
//! DynamoRIO suspends a thread with a signal of its own, for example on every
//! round of the synch a fork from another thread performs. No guest handler
//! sees it, so natively the call would never have been interrupted. The
//! backend supplies only the mechanism: it runs an injected `pselect6` under
//! the mask the call's sigmask argument gives, checks the signals it holds for
//! later delivery against that mask before the call sleeps, and reports an
//! interruption by its own signal as `ERESTARTNOHAND`. Detcore makes every
//! decision Linux makes:
//!
//! - which signals end the wait: the guest's effective mask (the call's
//!   temporary mask, or the thread's) plus every signal whose delivery would
//!   not end it ([`KernelSignalState::interrupting`]: ignored, and the
//!   default-ignored SIGCHLD, SIGCONT, SIGURG and SIGWINCH);
//! - the timeout, validated and normalized as `kern_select` and `do_pselect`
//!   read it;
//! - `STICKY_TIMEOUTS`, read from the personality per call: the kernel does not
//!   update a sticky timeout, so each restart gets the time left on Detcore's
//!   clock (this external wait is host-timed already);
//! - restarting on `ERESTARTNOHAND`, inside the same scheduler grant;
//! - the time left written back: a truncated timeval for `select`, a timespec
//!   for `pselect6`, nothing under `STICKY_TIMEOUTS`, a failed copy ignored.

use std::time::Duration;
use std::time::Instant;

use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;

use super::io::Pselect6SigmaskArg;
use super::io::ppoll_timeout_duration;
use super::io::select_timeout_duration;
use super::signal_shadow::copied_dispositions;
use super::threads::KernelSignalState;
use super::threads::KernelSigset;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// `STICKY_TIMEOUTS` (`include/uapi/linux/personality.h`).
const STICKY_TIMEOUTS: i64 = 0x0400_0000;

/// Where the guest's timeout lives, and in which form.
#[derive(Clone, Copy, Debug)]
pub(super) enum WaitTimeout {
    None,
    Select(usize),
    Pselect6(usize),
}

/// A `select` or `pselect6` for [`Detcore::backend_restartable_select_family`].
#[derive(Clone, Copy, Debug)]
pub(super) struct WaitRequest {
    pub(super) nfds: i32,
    pub(super) readfds: usize,
    pub(super) writefds: usize,
    pub(super) exceptfds: usize,
    pub(super) timeout: WaitTimeout,
    /// A `pselect6`'s temporary mask, read in Linux's order by the caller.
    pub(super) temporary_mask: Option<KernelSigset>,
}

fn timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

async fn inject_raw<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &mut G,
    number: Sysno,
    args: [usize; 6],
) -> Result<i64, Error> {
    let [a0, a1, a2, a3, a4, a5] = args;
    Ok(guest
        .inject(Syscall::from_raw(
            number,
            SyscallArgs::new(a0, a1, a2, a3, a4, a5),
        ))
        .await?)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Injects a `pselect6` that cannot sleep (a zero timeout, or a probe). A
    /// backend whose own signal interrupted it reports `ERESTARTNOHAND`; no
    /// guest handler ran and the call has no time left to keep, so it runs
    /// again, as Linux would restart it.
    pub(super) async fn inject_unsleeping_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Pselect6,
    ) -> Result<i64, reverie::Errno> {
        loop {
            let result = guest.inject(call).await;
            if self.cfg.backend.restarts_backend_interrupted_waits
                && result == Err(reverie::Errno::ERESTARTNOHAND)
            {
                continue;
            }
            return result;
        }
    }

    /// The guest's dispositions in the kernel's layout, from Detcore's copy, or
    /// asked of the backend where the copy cannot answer.
    async fn wait_dispositions<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<KernelSignalState, Error> {
        let mut state = KernelSignalState::default();
        if let Some(copied) = copied_dispositions(guest) {
            state.ignored = copied.ignored;
            state.caught = copied.caught;
            return Ok(state);
        }
        let mut stack = guest.stack().await;
        let old = stack.reserve::<[u64; 4]>();
        let _guard = stack.commit()?;
        for signal in 1..=64usize {
            if signal == libc::SIGKILL as usize || signal == libc::SIGSTOP as usize {
                continue;
            }
            if inject_raw(
                guest,
                Sysno::rt_sigaction,
                [signal, 0, old.as_raw(), 8, 0, 0],
            )
            .await
            .is_err()
            {
                continue;
            }
            let action: [u64; 4] = guest.memory().read_value(old)?;
            let bit = 1u64 << (signal - 1);
            match action[0] as usize {
                libc::SIG_DFL => {}
                libc::SIG_IGN => state.ignored |= bit,
                _ => state.caught |= bit,
            }
        }
        Ok(state)
    }

    /// The thread's own mask, from Detcore's copy, or asked of the backend.
    async fn wait_thread_mask<G: Guest<Self>>(&self, guest: &mut G) -> Result<KernelSigset, Error> {
        if let Some(shadow) = guest.thread_state().signal_shadow.as_ref() {
            return Ok(shadow.thread.blocked);
        }
        let mut stack = guest.stack().await;
        let current = stack.reserve::<KernelSigset>();
        let _guard = stack.commit()?;
        inject_raw(
            guest,
            Sysno::rt_sigprocmask,
            [libc::SIG_BLOCK as usize, 0, current.as_raw(), 8, 0, 0],
        )
        .await?;
        Ok(guest.memory().read_value(current)?)
    }

    /// Runs `request` with the Linux policy above.
    pub(super) async fn backend_restartable_select_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: WaitRequest,
    ) -> Result<i64, Error> {
        let original = match request.timeout {
            WaitTimeout::None => None,
            WaitTimeout::Select(address) => {
                let address =
                    Addr::<libc::timeval>::from_raw(address).ok_or(reverie::Errno::EFAULT)?;
                let timeval: libc::timeval = guest.memory().read_value(address)?;
                Some(select_timeout_duration(timeval)?)
            }
            WaitTimeout::Pselect6(address) => {
                let address = Addr::<Timespec>::from_raw(address).ok_or(reverie::Errno::EFAULT)?;
                let timespec: Timespec = guest.memory().read_value(address)?;
                Some(ppoll_timeout_duration(timespec)?)
            }
        };
        let guest_mask = match request.temporary_mask {
            Some(mask) => mask,
            None => self.wait_thread_mask(guest).await?,
        };
        let dispositions = self.wait_dispositions(guest).await?;
        // Every signal whose delivery would not end the wait stays blocked
        // while it sleeps.
        let wait_mask: KernelSigset = !dispositions.interrupting(guest_mask);
        let sticky = original.is_some()
            && inject_raw(guest, Sysno::personality, [0xffff_ffff, 0, 0, 0, 0, 0])
                .await
                .is_ok_and(|persona| persona & STICKY_TIMEOUTS != 0);

        let mut stack = guest.stack().await;
        let left = stack.reserve::<Timespec>();
        let mask_cell = stack.reserve::<KernelSigset>();
        let argument = stack.reserve::<Pselect6SigmaskArg>();
        let _guard = stack.commit()?;
        guest.memory().write_value(mask_cell, &wait_mask)?;
        guest.memory().write_value(
            argument,
            &Pselect6SigmaskArg {
                sigmask: mask_cell.as_raw(),
                sigsetsize: std::mem::size_of::<KernelSigset>(),
            },
        )?;
        if let Some(original) = original {
            guest.memory().write_value(left, &timespec(original))?;
        }
        let call = Syscall::from_raw(
            Sysno::pselect6,
            SyscallArgs::new(
                request.nfds as usize,
                request.readfds,
                request.writefds,
                request.exceptfds,
                if original.is_some() { left.as_raw() } else { 0 },
                argument.as_raw(),
            ),
        );
        let start = Instant::now();
        // The kernel keeps a non-sticky timeout's time left in `left` itself;
        // a sticky one is recomputed before each restart.
        let left_address = left.as_raw();
        let mut restart = move |guest: &mut G| -> Result<(), Error> {
            if let (true, Some(original), Some(left)) = (
                sticky,
                original,
                AddrMut::<Timespec>::from_raw(left_address),
            ) {
                let remaining = original.saturating_sub(start.elapsed());
                guest.memory().write_value(left, &timespec(remaining))?;
            }
            Ok(())
        };
        let result = self
            .record_or_replay_blocking_restarting(guest, call, request.temporary_mask, &mut restart)
            .await;
        if !sticky {
            match request.timeout {
                WaitTimeout::None => {}
                WaitTimeout::Select(address) => {
                    if let (Ok(remaining), Some(address)) = (
                        guest.memory().read_value(left),
                        AddrMut::<libc::timeval>::from_raw(address),
                    ) {
                        let remaining: Timespec = remaining;
                        let _ = guest.memory().write_value(
                            address,
                            &libc::timeval {
                                tv_sec: remaining.tv_sec,
                                tv_usec: remaining.tv_nsec / 1000,
                            },
                        );
                    }
                }
                WaitTimeout::Pselect6(address) => {
                    if let (Ok(remaining), Some(address)) = (
                        guest.memory().read_value(left),
                        AddrMut::<Timespec>::from_raw(address),
                    ) {
                        let remaining: Timespec = remaining;
                        let _ = guest.memory().write_value(address, &remaining);
                    }
                }
            }
        }
        result
    }
}
