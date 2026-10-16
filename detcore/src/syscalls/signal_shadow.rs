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
//! Where the backend's kernel signal state is the guest's own (the host's
//! `/proc` names guest threads; see [`host_signal_state_reads_refused`]), the
//! copy is checked against the kernel after each recorded call: the old action
//! or old mask the kernel wrote back, and `SigBlk`, `SigIgn` and `SigCgt`. A
//! mismatch is logged once, by name, and the thread's copy is dropped: it is
//! never repaired from the kernel, which would hide the defect the check is
//! for.

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use tracing::warn;

use super::threads::KernelSigaction;
use super::threads::KernelSigset;
use super::threads::host_signal_state_reads_refused;
use super::threads::read_kernel_signal_state;
use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::signal_state::Action;
use crate::signal_state::SignalShadow;
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
///   and resets an ignored SIGTRAP to its default. Reverie characterizes this
///   in `reverie-ptrace/src/plain_guest_regression_tests.rs`
///   (`sigtrap_profile_follows_each_handler_shape`).
fn reserved() -> KernelSigset {
    sigbit(reverie::PERF_EVENT_SIGNAL as i32) | sigbit(libc::SIGTRAP)
}

/// Accounts for handler returns the kernel ran without a report (the ptrace
/// backend never traps `rt_sigreturn`), from the kernel's current mask, before
/// a transition that depends on the mask. Does nothing while no handler frame
/// is open, or where the kernel's state is not the guest's.
pub(super) fn reconcile_returns<G, T>(guest: &mut G, before: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let open_frames = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .is_some_and(|shadow| !shadow.thread.activations.is_empty());
    if !open_frames || host_signal_state_reads_refused() {
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

/// Seeds the first image's copy, or applies `execve`'s reset to a later one.
/// Called from the new image's first callback. The first image's ignored set
/// and mask are what it inherited from the launcher, read from the kernel
/// before it runs a guest instruction; where that read is unavailable, the
/// copy stays unknown.
pub(crate) fn at_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() {
        shadow.exec();
        check_against_kernel(guest, "execve");
        return;
    }
    if host_signal_state_reads_refused() {
        return;
    }
    if let Ok(state) = read_kernel_signal_state(guest.pid(), guest.tid()) {
        guest.thread_state_mut().signal_shadow = Some(SignalShadow::seeded(
            state.ignored & !reserved(),
            state.blocked & !reserved(),
        ));
    }
}

/// Records a committed `rt_sigaction`. `new` is the action the kernel
/// installed (Detcore's sanitized copy), `None` for a query. `result` is the
/// call's result: an `EFAULT` after a successful read of the new action can
/// only come from writing `old_action`, after the new action is installed.
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
    let Some(shadow) = guest.thread_state().signal_shadow.as_ref() else {
        return;
    };
    let predicted = shadow
        .dispositions
        .lock()
        .unwrap()
        .sigaction(signum, new.map(Action::from));
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
        && let Some(address) = old_action
    {
        match guest
            .memory()
            .read_value::<_, KernelSigaction>(address.cast::<KernelSigaction>())
        {
            Ok(written) if Action::from(written) == old => {}
            Ok(written) => {
                return diverged(
                    guest,
                    "rt_sigaction",
                    &format!(
                        "old action of signal {signum}: the kernel wrote {:?}, the copy holds {old:?}",
                        Action::from(written)
                    ),
                );
            }
            Err(_) => {}
        }
    }
    check_against_kernel(guest, "rt_sigaction");
}

/// Records a committed `rt_sigprocmask`. `has_set` says whether the call
/// passed a set, and `set` is its value as the kernel used it (without
/// Hermit's reserved signal), `None` if Detcore could not read it. `result`'s
/// `EFAULT` after the set was read can only come from writing `oldset`, after
/// the change.
pub(super) fn record_rt_sigprocmask<G, T>(
    guest: &mut G,
    how: i32,
    has_set: bool,
    set: Option<KernelSigset>,
    oldset: Option<AddrMut<'_, libc::sigset_t>>,
    result: &Result<i64, Errno>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let committed = result.is_ok() || (set.is_some() && *result == Err(Errno::EFAULT));
    if !committed {
        return;
    }
    if has_set && set.is_none() {
        return diverged(
            guest,
            "rt_sigprocmask",
            "the kernel read a set that Detcore could not read",
        );
    }
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    let old = match shadow.thread.sigprocmask(how, set) {
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
    {
        match guest
            .memory()
            .read_value::<_, KernelSigset>(address.cast::<KernelSigset>())
        {
            Ok(written) if written & !reserved() == old => {}
            Ok(written) => {
                return diverged(
                    guest,
                    "rt_sigprocmask",
                    &format!("old mask: the kernel wrote {written:#x}, the copy holds {old:#x}"),
                );
            }
            Err(_) => {}
        }
    }
    check_against_kernel(guest, "rt_sigprocmask");
}

/// Records that the Tool approved `signal` for delivery to this thread, so the
/// backend delivers it now: a caught signal enters its handler with
/// `sa_mask` added, and `SA_RESETHAND` puts the default back. The handler
/// mask is checked at the thread's next recorded call, when the frame has been
/// written; a delivery whose frame cannot be written becomes a SIGSEGV, which
/// that check reports.
pub(crate) fn record_delivery<G, T>(guest: &mut G, signal: i32)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // At a delivery stop the kernel has not installed the handler mask yet.
    reconcile_returns(guest, "signal delivery");
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    let delivered = {
        let mut dispositions = shadow.dispositions.lock().unwrap();
        crate::signal_state::deliver(&mut dispositions, &mut shadow.thread, signal)
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
    let restored =
        reverie::syscalls::Addr::<u64>::from_raw(sp.wrapping_add(super::signal::UC_SIGMASK_OFFSET))
            .and_then(|address| guest.memory().read_value(address).ok());
    if let Some(mask) = restored
        && let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut()
    {
        shadow.thread.sigreturn(mask & !reserved());
    }
}

/// Compares the copy with the kernel's `/proc` view of this thread, where that
/// view is the guest's own.
fn check_against_kernel<G, T>(guest: &mut G, after: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if host_signal_state_reads_refused() {
        return;
    }
    if guest.thread_state().signal_shadow.is_none() {
        return;
    }
    let Ok(kernel) = read_kernel_signal_state(guest.pid(), guest.tid()) else {
        return;
    };
    let Some(shadow) = guest.thread_state_mut().signal_shadow.as_mut() else {
        return;
    };
    // A handler that returned since the last check left the frame's mask.
    shadow.thread.reconcile_returns(kernel.blocked, reserved());
    let shadow = guest
        .thread_state()
        .signal_shadow
        .as_ref()
        .expect("checked above");
    let (blocked, ignored, caught) = {
        let dispositions = shadow.dispositions.lock().unwrap();
        (
            shadow.thread.blocked,
            dispositions.ignored(),
            dispositions.caught(),
        )
    };
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

/// Reports a divergence once and drops this thread's copy.
fn diverged<G, T>(guest: &mut G, after: &str, what: &str)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    warn!(
        "[dtid {}] {DIVERGED} after {after}: {what}; this thread's copy is dropped",
        guest.thread_state().dettid
    );
    guest.thread_state_mut().signal_shadow = None;
}
