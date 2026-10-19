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
//!   read it, with its deadline set as the call enters;
//! - `STICKY_TIMEOUTS`, read from the personality per call;
//! - restarting on `ERESTARTNOHAND`, inside the same scheduler grant;
//! - the time left written back: a truncated timeval for `select`, a timespec
//!   for `pselect6`, nothing under `STICKY_TIMEOUTS`, a failed copy ignored.
//!
//! Another thread may switch a signal between ignored and caught while this
//! one waits, and Linux applies the new action at once. So the wait runs as
//! kernel waits of at most [`WAIT_SLICE`], each under the mask computed from
//! the dispositions as they are when it starts: a handler installed during the
//! wait, then a signal, ends it with EINTR within one slice. What this cannot
//! match is a signal that arrives while it is ignored and whose action changes
//! before the wait ends. Linux discards it on arrival; here the kernel holds it
//! blocked, and DynamoRIO delivers it under the new action when it is
//! unblocked. Discarding on arrival needs the backend to drop a signal its
//! guest ignores as it receives it, which DynamoRIO's queue does not do while
//! a client hooks signals.
//!
//! The scheduler is told the guest's temporary mask
//! (`Resources::blocked_signal_mask`), while the kernel sleeps under the wider
//! wait mask. That is sound only where Detcore does not model which thread
//! the kernel gives a signal (`bundled_signal_facts::models_signal_targets`,
//! false on DBT): where it does, the scheduler would wait for a sleeping mask
//! the kernel never installs. The routine refuses that combination.

use std::time::Duration;
use std::time::Instant;

use detcore_model::bundled_signal_facts;
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

use super::helpers::RestartBlockingCall;
use super::io::Pselect6SigmaskArg;
use super::io::ppoll_timeout_duration;
use super::io::pselect6_fd_set_len;
use super::io::read_pselect6_fd_set;
use super::io::select_timeout_duration;
use super::io::write_pselect6_fd_set;
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

    /// The mask the kernel sleeps under: the guest's mask plus every signal
    /// whose delivery would not end the wait, under the dispositions as they
    /// are now.
    async fn wait_mask<G: Guest<Self>>(
        &self,
        guest: &mut G,
        guest_mask: KernelSigset,
    ) -> Result<KernelSigset, Error> {
        Ok(!self
            .wait_dispositions(guest)
            .await?
            .interrupting(guest_mask))
    }

    /// Runs `request` with the Linux policy above.
    pub(super) async fn backend_restartable_select_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: WaitRequest,
    ) -> Result<i64, Error> {
        assert!(
            !bundled_signal_facts::models_signal_targets(&self.cfg),
            "restarts_backend_interrupted_waits cannot be combined with modelled signal \
             targets: the scheduler would wait for a sleeping mask the kernel never installs"
        );
        // Linux starts the timeout as the call enters (`poll_select_set_timeout`).
        let start = Instant::now();
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
        let sticky = original.is_some()
            && inject_raw(guest, Sysno::personality, [0xffff_ffff, 0, 0, 0, 0, 0])
                .await
                .is_ok_and(|persona| persona & STICKY_TIMEOUTS != 0);

        let mut stack = guest.stack().await;
        let slice = stack.reserve::<Timespec>();
        let mask_cell = stack.reserve::<KernelSigset>();
        let argument = stack.reserve::<Pselect6SigmaskArg>();
        let _guard = stack.commit()?;
        guest.memory().write_value(
            argument,
            &Pselect6SigmaskArg {
                sigmask: mask_cell.as_raw(),
                sigsetsize: std::mem::size_of::<KernelSigset>(),
            },
        )?;
        let call = Syscall::from_raw(
            Sysno::pselect6,
            SyscallArgs::new(
                request.nfds as usize,
                request.readfds,
                request.writefds,
                request.exceptfds,
                slice.as_raw(),
                argument.as_raw(),
            ),
        );
        // A kernel wait that times out clears the descriptor sets, so a slice
        // that ends short of the guest's deadline puts them back before the
        // next one. Where they cannot be read here (Linux reads only up to the
        // fd table's size, which can be smaller than nfds says), the wait is
        // not sliced.
        let len = pselect6_fd_set_len(request.nfds)?;
        let mut saved_sets = Some(Vec::with_capacity(3));
        for address in [request.readfds, request.writefds, request.exceptfds] {
            match read_pselect6_fd_set(guest, AddrMut::from_raw(address), len) {
                Ok(bytes) => {
                    if let Some(saved) = saved_sets.as_mut() {
                        saved.push((address, bytes));
                    }
                }
                Err(_) => saved_sets = None,
            }
        }
        let mut slicing = WaitSlices {
            detcore: self,
            guest_mask,
            deadline: original.map(|original| start + original),
            slice_len: saved_sets.is_some().then_some(WAIT_SLICE),
            saved_sets: saved_sets.unwrap_or_default(),
            slice: slice.as_raw(),
            mask_cell: mask_cell.as_raw(),
            slice_end: None,
            sliced: false,
            restarts: 0,
        };
        slicing.prepare(guest).await?;
        let result = self
            .record_or_replay_blocking_restarting(guest, call, request.temporary_mask, &mut slicing)
            .await;
        if !sticky {
            // The time left, as `poll_select_finish` computes it from the
            // deadline the call set as it entered.
            let remaining = original.map(|original| original.saturating_sub(start.elapsed()));
            match (request.timeout, remaining) {
                (WaitTimeout::Select(address), Some(remaining)) => {
                    if let Some(address) = AddrMut::<libc::timeval>::from_raw(address) {
                        let _ = guest.memory().write_value(
                            address,
                            &libc::timeval {
                                tv_sec: remaining.as_secs() as libc::time_t,
                                tv_usec: remaining.subsec_micros() as libc::suseconds_t,
                            },
                        );
                    }
                }
                (WaitTimeout::Pselect6(address), Some(remaining)) => {
                    if let Some(address) = AddrMut::<Timespec>::from_raw(address) {
                        let _ = guest.memory().write_value(address, &timespec(remaining));
                    }
                }
                _ => {}
            }
        }
        result
    }
}

/// The longest a single kernel wait lasts before Detcore reads the
/// dispositions again. Another thread may switch a signal between ignored and
/// caught while this one waits; Linux applies the new action at once
/// (`kernel/signal.c`, `sig_ignored` and `get_signal`), so the wait mask
/// follows it within one slice.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// Runs one restartable wait as a series of kernel waits of at most
/// [`WAIT_SLICE`], each under the wait mask as it is when it starts.
struct WaitSlices<'a, T: RecordOrReplay> {
    detcore: &'a Detcore<T>,
    guest_mask: KernelSigset,
    /// When the guest's timeout expires; `None` for no timeout.
    deadline: Option<Instant>,
    /// [`WAIT_SLICE`], or `None` where the wait is not sliced.
    slice_len: Option<Duration>,
    /// The guest's descriptor sets as the call passed them.
    saved_sets: Vec<(usize, Option<Vec<u8>>)>,
    /// When the current slice ends, and the mask is read again; `None` before
    /// the first.
    slice_end: Option<Instant>,
    /// The kernel wait's timespec and mask, in the guest's scratch stack.
    slice: usize,
    mask_cell: usize,
    /// Whether the kernel wait ends at the slice's end, short of the guest's
    /// deadline.
    sliced: bool,
    /// How many times the backend's own signal interrupted the wait.
    restarts: u64,
}

impl<T: RecordOrReplay> WaitSlices<'_, T> {
    /// Writes the next kernel wait's timeout: the time left until the guest's
    /// deadline or the slice's end, whichever is first. Once the slice has
    /// ended, a new one starts, under the mask the dispositions give now.
    ///
    /// A restart after the backend's own signal does no more than rewrite the
    /// timeout: DynamoRIO's fork synch sends that signal every 5 ms, and if it
    /// gives up on this thread, the child inherits whatever the thread holds,
    /// so the thread stays out of locks between those signals.
    async fn prepare<G: Guest<Detcore<T>>>(&mut self, guest: &mut G) -> Result<(), Error> {
        let now = Instant::now();
        if self.slice_end.is_none_or(|end| now >= end) {
            self.slice_end = self.slice_len.map(|len| now + len);
            let mask = self.detcore.wait_mask(guest, self.guest_mask).await?;
            let mask_cell =
                AddrMut::<KernelSigset>::from_raw(self.mask_cell).ok_or(reverie::Errno::EFAULT)?;
            guest.memory().write_value(mask_cell, &mask)?;
        }
        let (end, sliced) = match (self.deadline, self.slice_end) {
            (Some(deadline), Some(slice_end)) if slice_end < deadline => (Some(slice_end), true),
            (Some(deadline), _) => (Some(deadline), false),
            (None, slice_end) => (slice_end, slice_end.is_some()),
        };
        self.sliced = sliced;
        let slice = AddrMut::<Timespec>::from_raw(self.slice).ok_or(reverie::Errno::EFAULT)?;
        match end {
            Some(end) => guest
                .memory()
                .write_value(slice, &timespec(end.saturating_duration_since(now)))?,
            // No deadline and no slicing: wait without a timeout. The kernel
            // reads the scratch timespec only through the call's pointer, so
            // this writes the largest time it accepts instead.
            None => guest.memory().write_value(
                slice,
                &Timespec {
                    tv_sec: libc::time_t::MAX,
                    tv_nsec: 0,
                },
            )?,
        }
        Ok(())
    }

    /// Puts the guest's descriptor sets back after a slice's kernel wait
    /// timed out and cleared them.
    fn restore_sets<G: Guest<Detcore<T>>>(&self, guest: &mut G) -> Result<(), Error> {
        for (address, bytes) in &self.saved_sets {
            write_pselect6_fd_set(guest, AddrMut::from_raw(*address), bytes)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<G: Guest<Detcore<T>>, T: RecordOrReplay> RestartBlockingCall<G> for WaitSlices<'_, T> {
    async fn again(&mut self, guest: &mut G, result: &Result<i64, Error>) -> Result<bool, Error> {
        match result {
            // The backend's own signal interrupted the wait and no guest
            // handler ran, so Linux would not have ended it.
            Err(Error::Errno(reverie::Errno::ERESTARTNOHAND)) => self.restarts += 1,
            // The slice ended before the guest's deadline.
            Ok(0) if self.sliced => {
                self.restore_sets(guest)?;
                self.slice_end = Some(Instant::now());
            }
            _ => {
                if self.restarts > 0 {
                    tracing::debug!(
                        "pselect6 restarted {} times after the backend's own signal",
                        self.restarts
                    );
                }
                return Ok(false);
            }
        }
        self.prepare(guest).await?;
        Ok(true)
    }
}
