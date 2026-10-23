/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1c: which pending signals end a parked wait under the in-guest
//! delivery gate (`Config::in_guest_signal_delivery`), and what the wait's
//! end tells the restart checkpoint (design D40, D50; dev-hermit
//! ai_docs/2026-10-16-a5-signal-delivery-design.md, "3c.1 implementation
//! plan, revision 4", <https://github.com/rrnewton/hermit/issues/4485>).
//!
//! Once the in-guest runtime runs under a mask of its own (step 3c.1e), the
//! kernel's `SigBlk` for a guest thread is the runtime's, and a signal sent
//! to a parked thread stays pending in the kernel, held by that mask, until
//! Detcore ends the wait. So Detcore decides from kernel truth plus the one
//! fact the guest publishes (design 16.11):
//!
//! - the target's pending and action sets from `/proc/<tid>/status`
//!   (`SigPnd`, `ShdPnd`, `SigCgt`, `SigIgn`);
//! - the guest's mask as the thread published it at its latest request
//!   (step 3c.1b), never `SigBlk`.
//!
//! The deliverable set is `(SigPnd ∪ ShdPnd) ∖ mask ∩ (SigCgt ∪ (default-fatal
//! ∖ SigIgn))`: a signal Linux would deliver at the wait's return, a caught one
//! by running its handler, a default-fatal one by ending the process. A
//! blocked signal stays pending; an ignored or default-ignored one, or a
//! default stop, does not end the wait (stop sends are refused under the gate,
//! D48). The in-guest runtime's reserved signals
//! (`Config::in_guest_reserved_signals`) are left out: their `SigCgt` bit is
//! the runtime's handler, not a guest disposition, and a cross-process send of
//! one is refused (step 3c.0a).

use crate::syscalls::KernelSignalState;
use crate::syscalls::kernel_sigset_bit;

/// What ended a wait for a signal under the in-guest delivery gate: D40's
/// per-syscall flag, set by the path that ends the wait
/// (`ThreadState::wait_signal_end`) and taken by the restart checkpoint
/// straight after the dispatch `match` (`Detcore::wait_end_checkpoint`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitSignalEnd {
    /// A default-fatal signal is deliverable, so the process dies at the
    /// return and the guest never resumes after the call: the guest result is
    /// `-EINTR`, and the original restart code is what the happens-before
    /// checkpoint records (IR-2). The signal is the first default-fatal one
    /// Linux would dequeue.
    Fatal(i32),
    /// Only caught signals are deliverable: the first one Linux would dequeue
    /// runs its handler at the return, and the call's result goes through the
    /// x86 restart table with that action's flags
    /// (`signal_restart::caught_signal_return`).
    Caught(i32),
}

/// The signals whose default action ends the process (terminate or dump core)
/// and whose disposition is still the default: not caught, not ignored, not a
/// default-ignored signal (`SIGCHLD`, `SIGCONT`, `SIGURG`, `SIGWINCH`), and not
/// a stop. `SIGKILL` always is.
pub(crate) fn default_fatal(state: &KernelSignalState) -> u64 {
    (1..=64).fold(0, |set, signal| {
        if crate::syscalls::default_action_ends_process(state.ignored, state.caught, signal) {
            set | kernel_sigset_bit(signal)
        } else {
            set
        }
    })
}

/// The design's deliverable set for a thread whose guest mask is `mask`: the
/// signals pending for it (`SigPnd ∪ ShdPnd`) that `mask` leaves unblocked and
/// that are caught, or default-fatal and not ignored, less `reserved`, the
/// in-guest runtime's reserved signals.
pub(crate) fn deliverable(state: &KernelSignalState, mask: u64, reserved: u64) -> u64 {
    state.pending & !mask & (state.caught | default_fatal(state)) & !reserved
}

/// What a wait that `deliverable` (a non-empty subset of the design's set for
/// `state`) ends means for the call's result, or `None` when nothing ends it.
/// Any default-fatal signal makes it `Fatal`, since the process then dies at
/// the return whatever handlers run first; otherwise the first caught signal
/// in Linux's dequeue order (`KernelSignalState::next_dequeued`: the thread's
/// queue before the shared one, synchronous signals first) is the delivery
/// whose action the restart table reads.
pub(crate) fn classify(state: &KernelSignalState, deliverable: u64) -> Option<WaitSignalEnd> {
    if deliverable == 0 {
        return None;
    }
    let fatal = deliverable & default_fatal(state);
    let among = |set: u64| KernelSignalState {
        pending: state.pending & set,
        thread_pending: state.thread_pending & set,
        shared_pending: state.shared_pending & set,
        ..*state
    };
    if fatal != 0 {
        let (signal, _) = among(fatal).next_dequeued(0)?;
        return Some(WaitSignalEnd::Fatal(signal));
    }
    let (signal, _) = among(deliverable).next_dequeued(0)?;
    Some(WaitSignalEnd::Caught(signal))
}

/// [`deliverable`] then [`classify`].
pub(crate) fn wait_end(
    state: &KernelSignalState,
    mask: u64,
    reserved: u64,
) -> Option<WaitSignalEnd> {
    classify(state, deliverable(state, mask, reserved))
}

/// D44 (A5 step 3c.1c): whether a send of `signal` to a thread blocked in a
/// kernel-run call (`ResourceID::is_kernel_run_blocking`), whose guest mask at
/// that call's request was `mask`, is refused: Linux would deliver it by
/// interrupting the call, which the kernel runs under the in-guest runtime's
/// mask, so it would stay pending and the call would never end for it. That
/// is a signal `mask` leaves unblocked that is caught, or default-fatal and not
/// ignored. A vfork parent (`vfork`) sleeps killable, and Linux wakes it only
/// for a fatal signal that is not core-dumping (`wait_for_vfork_done`,
/// kernel/fork.c:1572-1590; `complete_signal`, kernel/signal.c:1034-1054): a
/// caught or core-dumping signal waits for the child's release, which keeping
/// it pending reproduces, so only an unblocked, non-core-dumping default-fatal
/// signal is refused there. `state` gives the dispositions; its pending set
/// is not read. SIGKILL and signal 0 are never passed here.
pub(crate) fn kernel_run_send_refused(
    state: &KernelSignalState,
    mask: u64,
    signal: i32,
    vfork: bool,
    reserved: u64,
) -> bool {
    if !(1..=64).contains(&signal) {
        return false;
    }
    let bit = kernel_sigset_bit(signal);
    if mask & bit != 0 || reserved & bit != 0 {
        return false;
    }
    if vfork {
        default_fatal(state) & bit != 0 && core_dumping() & bit == 0
    } else {
        (state.caught | default_fatal(state)) & bit != 0
    }
}

/// The signals whose default action dumps core (`SIG_KERNEL_COREDUMP_MASK`,
/// include/linux/signal.h:426-432 at v6.12).
pub(crate) fn core_dumping() -> u64 {
    [
        libc::SIGQUIT,
        libc::SIGILL,
        libc::SIGTRAP,
        libc::SIGABRT,
        libc::SIGFPE,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGSYS,
        libc::SIGXCPU,
        libc::SIGXFSZ,
    ]
    .into_iter()
    .fold(0, |set, signal| set | kernel_sigset_bit(signal))
}

/// Whether `errno` is one of the kernel's internal restart codes
/// (include/linux/errno.h v6.12 lines 14-18), which a guest never sees: the
/// kernel either restarts the call or turns the code into `EINTR` at the
/// return.
pub(crate) fn is_restart_code(errno: reverie::Errno) -> bool {
    matches!(
        errno,
        reverie::Errno::ERESTARTSYS
            | reverie::Errno::ERESTARTNOINTR
            | reverie::Errno::ERESTARTNOHAND
            | reverie::Errno::ERESTART_RESTARTBLOCK
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(signal: i32) -> u64 {
        kernel_sigset_bit(signal)
    }

    fn state(thread: u64, shared: u64, ignored: u64, caught: u64) -> KernelSignalState {
        KernelSignalState {
            pending: thread | shared,
            thread_pending: thread,
            shared_pending: shared,
            // The kernel's mask is the in-guest runtime's under the gate;
            // the predicate never reads it.
            blocked: !0,
            ignored,
            caught,
        }
    }

    /// Mutation "SigBlk used": the kernel mask here blocks everything, so a
    /// predicate that read it would find nothing.
    #[test]
    fn a_default_fatal_signal_unblocked_by_the_published_mask_is_deliverable() {
        let s = state(bit(libc::SIGTERM), 0, 0, 0);
        assert_eq!(deliverable(&s, 0, 0), bit(libc::SIGTERM));
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Fatal(libc::SIGTERM))
        );
    }

    #[test]
    fn a_signal_the_published_mask_blocks_is_not_deliverable() {
        let s = state(bit(libc::SIGTERM), 0, 0, 0);
        assert_eq!(deliverable(&s, bit(libc::SIGTERM), 0), 0);
        assert_eq!(wait_end(&s, bit(libc::SIGTERM), 0), None);
    }

    /// Mutation "ignored counted as deliverable".
    #[test]
    fn ignored_and_default_ignored_signals_are_not_deliverable() {
        let s = state(
            bit(libc::SIGTERM) | bit(libc::SIGCHLD) | bit(libc::SIGURG) | bit(libc::SIGWINCH),
            bit(libc::SIGCONT),
            bit(libc::SIGTERM),
            0,
        );
        assert_eq!(deliverable(&s, 0, 0), 0);
    }

    #[test]
    fn default_stops_are_not_deliverable_and_caught_ones_are() {
        let stops = bit(libc::SIGTSTP) | bit(libc::SIGTTIN) | bit(libc::SIGTTOU);
        let s = state(stops, 0, 0, 0);
        assert_eq!(deliverable(&s, 0, 0), 0);
        let caught = state(stops, 0, 0, bit(libc::SIGTSTP));
        assert_eq!(deliverable(&caught, 0, 0), bit(libc::SIGTSTP));
        assert_eq!(
            wait_end(&caught, 0, 0),
            Some(WaitSignalEnd::Caught(libc::SIGTSTP))
        );
    }

    /// The 3c.1.0 core-dumping row (https://github.com/rrnewton/hermit/issues/4571):
    /// a core-dumping signal is default-fatal too.
    #[test]
    fn core_dumping_signals_are_default_fatal() {
        for signal in [libc::SIGQUIT, libc::SIGABRT, libc::SIGBUS, libc::SIGFPE] {
            let s = state(0, bit(signal), 0, 0);
            assert_eq!(wait_end(&s, 0, 0), Some(WaitSignalEnd::Fatal(signal)));
        }
    }

    #[test]
    fn the_shared_queue_counts() {
        let s = state(0, bit(libc::SIGHUP), 0, 0);
        assert_eq!(wait_end(&s, 0, 0), Some(WaitSignalEnd::Fatal(libc::SIGHUP)));
    }

    #[test]
    fn a_fatal_signal_behind_a_caught_one_makes_the_end_fatal() {
        let s = state(
            bit(libc::SIGUSR1) | bit(libc::SIGTERM),
            0,
            0,
            bit(libc::SIGUSR1),
        );
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Fatal(libc::SIGTERM))
        );
    }

    #[test]
    fn the_first_caught_signal_follows_linux_dequeue_order() {
        // The private queue first, then synchronous before lower-numbered.
        let caught = bit(libc::SIGUSR1) | bit(libc::SIGUSR2) | bit(libc::SIGHUP);
        let s = state(bit(libc::SIGUSR2), bit(libc::SIGHUP), 0, caught);
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Caught(libc::SIGUSR2))
        );
    }

    #[test]
    fn reserved_signals_are_left_out() {
        let s = state(bit(libc::SIGSYS), 0, 0, bit(libc::SIGSYS));
        assert_eq!(deliverable(&s, 0, bit(libc::SIGSYS)), 0);
        assert_eq!(deliverable(&s, 0, 0), bit(libc::SIGSYS));
    }

    #[test]
    fn sigkill_is_always_deliverable_when_pending() {
        let s = state(bit(libc::SIGKILL), 0, 0, 0);
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Fatal(libc::SIGKILL))
        );
    }

    /// D44's refusal and its vfork narrowing. Mutations: "vfork not
    /// narrowed" (the caught and core-dumping vfork rows), "the mask
    /// ignored" (the blocked rows).
    #[test]
    fn a_kernel_run_send_is_refused_only_for_a_signal_that_would_interrupt_the_call() {
        let usr1 = bit(libc::SIGUSR1);
        let term = bit(libc::SIGTERM);
        let dispositions = state(0, 0, bit(libc::SIGHUP), usr1);
        for (signal, mask, vfork, refused, name) in [
            (libc::SIGTERM, 0, false, true, "default-fatal"),
            (libc::SIGQUIT, 0, false, true, "core-dumping"),
            (libc::SIGUSR1, 0, false, true, "caught"),
            (libc::SIGTERM, term, false, false, "blocked"),
            (libc::SIGHUP, 0, false, false, "SIG_IGN"),
            (libc::SIGURG, 0, false, false, "default-ignored"),
            (libc::SIGTSTP, 0, false, false, "default stop"),
            (libc::SIGTERM, 0, true, true, "vfork: default-fatal"),
            (libc::SIGQUIT, 0, true, false, "vfork: core-dumping"),
            (libc::SIGUSR1, 0, true, false, "vfork: caught"),
            (libc::SIGTERM, term, true, false, "vfork: blocked"),
        ] {
            assert_eq!(
                kernel_run_send_refused(&dispositions, mask, signal, vfork, 0),
                refused,
                "{name}"
            );
        }
        assert!(!kernel_run_send_refused(
            &dispositions,
            0,
            libc::SIGTERM,
            false,
            term
        ));
    }

    #[test]
    fn restart_codes() {
        for errno in [
            reverie::Errno::ERESTARTSYS,
            reverie::Errno::ERESTARTNOINTR,
            reverie::Errno::ERESTARTNOHAND,
            reverie::Errno::ERESTART_RESTARTBLOCK,
        ] {
            assert!(is_restart_code(errno));
        }
        assert!(!is_restart_code(reverie::Errno::EINTR));
    }
}
