/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keeping each thread's [`SignalShadow`] in step with the guest.
//!
//! Every function here runs after the guest operation it records has
//! committed, uses only values Detcore already holds for that operation, and
//! issues no injection and no scheduler request: recording changes nothing the
//! guest or the schedule can observe.
//!
//! Where the backend's kernel signal state is the guest's own
//! ([`kernel_state_is_the_guests`]), the
//! copy is checked against the kernel after each recorded call: the old action
//! or old mask the kernel wrote back, and `SigBlk`, `SigIgn` and `SigCgt`. A
//! mismatch is logged once, by name, and the thread's copy is dropped: it is
//! never repaired from the kernel, which would hide the defect the check is
//! for.

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use tracing::debug;
use tracing::warn;

use super::threads::KernelSigaction;
use super::threads::KernelSigset;
use super::threads::host_signal_state_reads_refused;
use super::threads::read_kernel_signal_state;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::signal_state::Action;
use crate::signal_state::Dispositions;
use crate::signal_state::SignalShadow;
use crate::signal_state::ThreadSignals;
use crate::signal_state::sigbit;

/// The prefix of the one log line a divergence produces, for log searches.
pub(crate) const DIVERGED: &str = "SIGNAL_STATE_COPY_DIVERGED";

/// Signals whose kernel state differs from the guest's by design, left out of
/// every comparison:
/// - Reverie's preemption signal is Hermit's, and Detcore keeps the guest from
///   installing or blocking it (`handle_rt_sigaction`, `rt_sigprocmask_inner`).
/// - SIGTRAP, which Detcore already reports as appropriated
///   (`warn_appropriated_signal`): when the ptrace backend single-steps the
///   guest, the kernel force-delivers the step trap, which unblocks SIGTRAP
///   and resets an ignored, or caught but blocked, SIGTRAP to its default
///   (`force_sig_info_to_task`). Reverie characterizes this
///   in `reverie-ptrace/src/plain_guest_regression_tests.rs`
///   (`sigtrap_profile_follows_each_handler_shape`).
fn reserved() -> KernelSigset {
    sigbit(reverie::PERF_EVENT_SIGNAL as i32) | sigbit(libc::SIGTRAP)
}

impl From<KernelSigaction> for Action {
    fn from(action: KernelSigaction) -> Self {
        Action {
            handler: action.handler,
            flags: action.flags,
            restorer: action.restorer,
            mask: action.mask,
        }
    }
}

/// Whether this backend's kernel signal state for a guest thread is the
/// guest's own: the guest thread is a host task, and no runtime in the
/// guest's process installs handlers or masks of its own. This is the mechanism
/// fact `supports_blocked_wait_signal_interruption` describes (true for
/// ptrace, and for e9patch, which runs under ptrace). Elsewhere the copy is
/// neither seeded from the kernel nor checked against it.
fn kernel_state_is_the_guests<G, T>(guest: &G) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    guest
        .config()
        .backend
        .supports_blocked_wait_signal_interruption
        && !host_signal_state_reads_refused()
}

/// Applies `change` to this thread's copy: the actions its group shares and
/// its own masks. Returns `None`, and drops the thread's copy without a new
/// report, when the shared actions are unknown because another thread
/// sharing them lost its copy (that loss was reported).
fn with_copy<G, T, R>(
    guest: &mut G,
    change: impl FnOnce(&mut Dispositions, &mut ThreadSignals) -> R,
) -> Option<R>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let shadow = guest.thread_state_mut().signal_shadow.as_mut()?;
    let actions = std::sync::Arc::clone(&shadow.dispositions);
    let mut actions = actions.lock().unwrap();
    match actions.as_mut() {
        Some(dispositions) => Some(change(dispositions, &mut shadow.thread)),
        None => {
            drop(actions);
            debug!(
                "[dtid {}] signal-state copy dropped: a thread sharing its actions lost its copy",
                guest.thread_state().dettid
            );
            drop_copy(guest);
            None
        }
    }
}

/// Drops this thread's copy for good: it is never seeded again, and threads
/// this one creates start without one.
fn drop_copy<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    guest.thread_state_mut().signal_shadow = None;
    guest.thread_state_mut().signal_shadow_lost = true;
}

/// At a syscall's entry, the thread has run guest code since its last event:
/// a temporary-mask restore no handler took has happened, and an open
/// handler frame may have returned unseen. Accounts for both before the call
/// can depend on the mask (a clone copies it; an exec keeps it).
pub(crate) fn at_syscall_entry<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    shadow.thread.settle();
    reconcile_returns(guest, "syscall entry");
}

/// Accounts for handler returns the kernel ran without a report (the ptrace
/// backend never traps `rt_sigreturn`), from the kernel's current mask, before
/// a transition that depends on the mask. Does nothing while no handler frame
/// is open, or where the kernel's state is not the guest's.
fn reconcile_returns<G, T>(guest: &mut G, before: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let open_frames = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .is_some_and(|shadow| !shadow.thread.activations.is_empty());
    if !open_frames || !kernel_state_is_the_guests(guest) {
        return;
    }
    let Ok(kernel) = read_kernel_signal_state(guest.pid(), guest.tid()) else {
        return;
    };
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    let copy = shadow.thread.blocked;
    if !shadow.thread.reconcile_returns(kernel.blocked, reserved()) {
        diverged(
            guest,
            before,
            &format!(
                "SigBlk kernel {:#018x} is neither the copy's {:#018x} nor an open handler frame's",
                kernel.blocked & !reserved(),
                copy & !reserved()
            ),
        );
    }
}

/// Seeds the first image's copy, or applies `execve`'s reset to a later one.
/// Called from the new image's first callback. The first image's ignored set
/// and mask are what it inherited from the launcher, read from the kernel
/// before it runs a guest instruction; where that read is unavailable, the
/// copy stays unknown. Seeding there needs the backend to report the first
/// image's inherited state, which the A5 design puts in the `Guest` contract
/// (its step 3).
pub(crate) fn at_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() {
        if !shadow.exec() {
            drop_copy(guest);
            return;
        }
        check_against_kernel(guest, "execve");
        return;
    }
    // Only the first image is seeded: a copy dropped earlier is never
    // rebuilt from the kernel.
    if guest.thread_state().signal_shadow_lost || !kernel_state_is_the_guests(guest) {
        return;
    }
    if let Ok(state) = read_kernel_signal_state(guest.pid(), guest.tid()) {
        guest.thread_state_mut().signal_shadow =
            Some(SignalShadow::seeded(state.ignored, state.blocked));
    }
}

/// Records a committed `rt_sigaction`. `new` is the action the kernel
/// installed (Detcore's sanitized copy of an action the kernel validated),
/// `None` for a query. `result` is the call's result: an `EFAULT` after the
/// new action was read can only come from writing `old_action`, after the new
/// action is installed.
pub(super) fn record_rt_sigaction<G, T>(
    guest: &mut G,
    signum: i32,
    new: Option<KernelSigaction>,
    old_action: Option<AddrMut<'_, libc::sigaction>>,
    result: &Result<i64, Errno>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let committed = result.is_ok() || (new.is_some() && *result == Err(Errno::EFAULT));
    if !committed {
        return;
    }
    let Some(predicted) = with_copy(guest, |dispositions, _| {
        dispositions.sigaction(signum, new.map(Action::from))
    }) else {
        return;
    };
    let old = match predicted {
        Ok(old) => old,
        Err(errno) => {
            return diverged(
                guest,
                "rt_sigaction",
                &format!(
                    "the kernel accepted signal {signum}, which the copy refuses with {errno}"
                ),
            );
        }
    };
    if result.is_ok()
        && sigbit(signum) & reserved() == 0
        && let Some(address) = old_action
        && let Ok(written) = guest
            .memory()
            .read_value::<_, KernelSigaction>(address.cast::<KernelSigaction>())
        && Action::from(written) != old
    {
        return diverged(
            guest,
            "rt_sigaction",
            &format!(
                "old action of signal {signum}: the kernel wrote {:?}, the copy holds {old:?}",
                Action::from(written)
            ),
        );
    }
    check_against_kernel(guest, "rt_sigaction");
}

/// Records a committed `rt_sigprocmask`. `set` is the call's set as the
/// kernel used it (without Hermit's reserved signal), `None` when it passed
/// none, and `Some(None)` when Detcore could not read it. `set_validated` says
/// whether the kernel itself accepted reading the set before the call (the
/// `SIG_BLOCK` and `SIG_SETMASK` paths validate it; Detcore's plain read can
/// succeed where the kernel's fails). An `EFAULT` is a committed change only
/// when it can come from writing `oldset` alone.
pub(super) fn record_rt_sigprocmask<G, T>(
    guest: &mut G,
    how: i32,
    set: Option<Option<KernelSigset>>,
    set_validated: bool,
    oldset: Option<AddrMut<'_, libc::sigset_t>>,
    result: &Result<i64, Errno>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let committed = match result {
        Ok(_) => true,
        Err(Errno::EFAULT) => match set {
            None => false,
            Some(_) if set_validated => true,
            Some(_) if oldset.is_none() => false,
            Some(_) => {
                // The set or the old mask: Linux leaves no way to tell which.
                debug!(
                    "[dtid {}] signal-state copy dropped: rt_sigprocmask's EFAULT may come from \
                     its set or its old mask",
                    guest.thread_state().dettid
                );
                drop_copy(guest);
                return;
            }
        },
        Err(_) => false,
    };
    if !committed {
        return;
    }
    let set = match set {
        None => None,
        Some(Some(set)) => Some(set),
        Some(None) => {
            return diverged(
                guest,
                "rt_sigprocmask",
                "the kernel read a set that Detcore could not read",
            );
        }
    };
    let Some(predicted) = with_copy(guest, |_, thread| thread.sigprocmask(how, set)) else {
        return;
    };
    let old = match predicted {
        Ok(old) => old,
        Err(errno) => {
            return diverged(
                guest,
                "rt_sigprocmask",
                &format!("the kernel accepted how={how}, which the copy refuses with {errno}"),
            );
        }
    };
    if result.is_ok()
        && let Some(address) = oldset
        && let Ok(written) = guest
            .memory()
            .read_value::<_, KernelSigset>(address.cast::<KernelSigset>())
        && written & !reserved() != old & !reserved()
    {
        return diverged(
            guest,
            "rt_sigprocmask",
            &format!(
                "old mask: the kernel wrote {:#x}, the copy holds {:#x}",
                written & !reserved(),
                old & !reserved()
            ),
        );
    }
    check_against_kernel(guest, "rt_sigprocmask");
}

/// After a call that sleeps under a temporary mask (`rt_sigsuspend`, or
/// `ppoll`, `pselect6` or `epoll_pwait` given a mask) returned to Detcore
/// interrupted by a signal: Linux returns with that mask still installed and
/// the thread's own mask to be put back, by the handler's frame if one is
/// entered first, else on the way back to the guest (`TIF_RESTORE_SIGMASK`).
pub(crate) fn after_syscall<G, T>(guest: &mut G, call: &Syscall, result: &Result<i64, Error>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.thread_state().signal_shadow.is_none() {
        return;
    }
    let interrupted = matches!(
        result,
        Err(Error::Errno(
            Errno::EINTR
                | Errno::ERESTARTNOHAND
                | Errno::ERESTARTSYS
                | Errno::ERESTARTNOINTR
                | Errno::ERESTART_RESTARTBLOCK
        ))
    );
    if !interrupted {
        return;
    }
    let memory = guest.memory();
    let read_set = |address: Option<Addr<'_, libc::sigset_t>>, size: usize| {
        let address = address?;
        if size != super::threads::KERNEL_SIGSET_SIZE {
            return None;
        }
        memory
            .read_value::<_, KernelSigset>(address.cast::<KernelSigset>())
            .ok()
    };
    let mask = match call {
        Syscall::RtSigsuspend(call) => read_set(call.mask(), call.sigsetsize()),
        Syscall::Ppoll(call) => read_set(call.sigmask(), call.sigsetsize()),
        Syscall::EpollPwait(call) => read_set(call.sigmask(), call.sigsetsize()),
        Syscall::Pselect6(call) => call.sigmask().and_then(|argument| {
            let argument = memory
                .read_value::<_, super::io::Pselect6SigmaskArg>(
                    argument.cast::<super::io::Pselect6SigmaskArg>(),
                )
                .ok()?;
            read_set(Addr::from_raw(argument.sigmask), argument.sigsetsize)
        }),
        _ => return,
    };
    let Some(mask) = mask else {
        return;
    };
    with_copy(guest, |_, thread| {
        thread.interrupted_under_temporary_mask(mask & !reserved());
    });
}

/// Records that the Tool approved `signal` for delivery to this thread, so the
/// backend delivers it now: a caught signal enters its handler with
/// `sa_mask` added, and `SA_RESETHAND` puts the default back. The handler
/// mask is checked at the thread's next recorded call, when the frame has been
/// written. A delivery whose frame cannot be written becomes a SIGSEGV and
/// leaves the old mask, which the check can take for the frame's return: that
/// case is not told apart.
pub(crate) fn record_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // At a delivery stop the kernel has not installed the handler mask yet.
    reconcile_returns(guest, "signal delivery");
    let Some(delivered) = with_copy(guest, |dispositions, thread| {
        crate::signal_state::deliver(dispositions, thread, signal)
    }) else {
        return;
    };
    if let Err(errno) = delivered {
        diverged(
            guest,
            "signal delivery",
            &format!("the backend delivers signal {signal}, which the copy refuses with {errno}"),
        );
    }
}

/// Records an `rt_sigreturn` at its entry, before it runs: Linux installs the
/// frame's `uc_sigmask` as soon as it can read it, before restoring the
/// registers can fail (`arch/x86/kernel/signal_64.c`). A frame whose mask
/// cannot be read changes no mask; the kernel then sends SIGSEGV.
pub(crate) async fn record_rt_sigreturn<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.thread_state().signal_shadow.is_none() {
        return;
    }
    let sp = guest.regs().await.rsp as usize;
    let restored = Addr::<u64>::from_raw(sp.wrapping_add(super::signal::UC_SIGMASK_OFFSET))
        .and_then(|address| guest.memory().read_value(address).ok());
    if let Some(mask) = restored {
        with_copy(guest, |_, thread| thread.sigreturn(mask & !reserved()));
    }
}

/// Compares the copy with the kernel's `/proc` view of this thread, where that
/// view is the guest's own.
fn check_against_kernel<G, T>(guest: &mut G, after: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !kernel_state_is_the_guests(guest) || guest.thread_state().signal_shadow.is_none() {
        return;
    }
    let Ok(kernel) = read_kernel_signal_state(guest.pid(), guest.tid()) else {
        return;
    };
    let Some(copy) = with_copy(guest, |dispositions, thread| {
        // A handler that returned since the last check left the frame's mask.
        thread.reconcile_returns(kernel.blocked, reserved());
        (
            thread.blocked,
            dispositions.ignored(),
            dispositions.caught(),
        )
    }) else {
        return;
    };
    let (blocked, ignored, caught) = copy;
    let unreserved = !reserved();
    let mismatches: Vec<String> = [
        ("SigBlk", kernel.blocked, blocked),
        ("SigIgn", kernel.ignored, ignored),
        ("SigCgt", kernel.caught, caught),
    ]
    .into_iter()
    .filter(|(_, kernel, copy)| kernel & unreserved != copy & unreserved)
    .map(|(field, kernel, copy)| {
        format!(
            "{field} kernel {:#018x} copy {:#018x}",
            kernel & unreserved,
            copy & unreserved
        )
    })
    .collect();
    if !mismatches.is_empty() {
        diverged(guest, after, &mismatches.join(", "));
    }
}

/// Reports a divergence once, drops this thread's copy and marks the actions
/// it shares unknown, so no sharing thread reports the consequence again.
fn diverged<G, T>(guest: &mut G, after: &str, what: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    warn!(
        "[dtid {}] {DIVERGED} after {after}: {what}; this thread's copy is dropped",
        guest.thread_state().dettid
    );
    if let Some(shadow) = guest.thread_state().signal_shadow.as_ref() {
        shadow.forget_dispositions();
    }
    drop_copy(guest);
}
