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
use crate::syscalls::SignalQueue;
use crate::syscalls::kernel_sigset_bit;

/// What ended a wait for a signal under the in-guest delivery gate: D40's
/// per-syscall flag, set by the path that ends the wait
/// (`ThreadState::wait_signal_end`) and taken by the restart checkpoint
/// straight after the dispatch `match` (`Detcore::wait_end_checkpoint`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitSignalEnd {
    /// The process dies at the return and the guest never resumes after the
    /// call: the guest result is `-EINTR`, and the original restart code is
    /// what the happens-before checkpoint records (IR-2). Either a
    /// non-core-dumping default-fatal signal is deliverable (the first such
    /// one Linux would dequeue), or Linux's delivery loop reaches a
    /// core-dumping default-fatal one that no mask blocks ([`classify`],
    /// [`DequeueWalk::resolve`]).
    ///
    /// The signal is not the exit status for a non-core-dumping end: Linux
    /// takes that from `group_exit_code`, the first such signal SENT, which
    /// `complete_signal` records when it starts the group exit
    /// (kernel/signal.c:1041-1049 at v6.12), not the first one dequeued.
    /// Step 3c.1e, which takes the death's status, must use the send order.
    Fatal(i32),
    /// The first signal Linux would dequeue is caught, and Linux's delivery
    /// loop reaches no default-fatal signal: either none is deliverable, or
    /// the masks of the handlers it sets up before reaching one block it.
    /// That first signal's handler runs at the return, and the call's result
    /// goes through the x86 restart table with that action's flags
    /// (`signal_restart::caught_signal_return`).
    Caught(i32),
    /// The first signal Linux would dequeue is caught and a core-dumping
    /// default-fatal signal is deliverable behind it: whether the process
    /// dies at the return depends on the masks of the handlers Linux sets up
    /// first, which the restart checkpoint reads in the thread's own turn
    /// and resolves to `Fatal` or `Caught` ([`DequeueWalk::resolve`]).
    BehindCaught(DequeueWalk),
}

/// The parts of a caught signal's action that decide what Linux blocks
/// while its handler is set up, and the restart table's flags: its
/// `sa_mask` and `sa_flags`, as a query `rt_sigaction` reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaughtAction {
    pub(crate) mask: u64,
    pub(crate) flags: u64,
}

/// A caught signal's action that the checkpoint could not read while a
/// default-fatal signal stays deliverable behind it
/// ([`DequeueWalk::resolve`]): `caught` is the signal whose action is
/// missing, `behind` the default-fatal signal Linux could reach next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActionUnknown {
    pub(crate) caught: i32,
    pub(crate) behind: i32,
}

/// The input of Linux's delivery loop at a wait's end, for a
/// [`WaitSignalEnd::BehindCaught`]: the deliverable signals on the thread's
/// private queue and on the shared queue, which of them are default-fatal,
/// and the guest's mask they were found under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DequeueWalk {
    thread: u64,
    shared: u64,
    fatal: u64,
    mask: u64,
}

impl DequeueWalk {
    /// Linux's delivery loop over this walk (v6.12). At the return to user
    /// mode `exit_to_user_mode_loop` calls `arch_do_signal_or_restart` for as
    /// long as a signal is pending and unblocked (kernel/entry/common.c:
    /// 97-130; arch/x86/kernel/signal.c:333-341), before any user
    /// instruction runs. Each `get_signal` dequeues the next signal in order
    /// (private queue first, then synchronous signals, then the lowest
    /// number; kernel/signal.c:634-639, :196-218). For a caught one,
    /// `handle_signal` sets up its frame and `signal_delivered`
    /// (kernel/signal.c:2941-2954) adds the action's `sa_mask`, and the
    /// signal itself unless `SA_NODEFER`, to the blocked mask, and the loop
    /// goes on; a default-fatal one not blocked by then ends the process
    /// (:2878-2906) without any handler running. So the end is `Fatal` with
    /// the first default-fatal signal the loop reaches, or `Caught` with the
    /// first caught signal when the accumulated masks block every
    /// default-fatal one. `action` gives a caught signal's action; it is
    /// asked only while a default-fatal signal is still deliverable, and a
    /// `None` then is [`ActionUnknown`].
    ///
    /// `SA_NODEFER` cannot change the outcome: it only leaves the delivered
    /// signal itself unblocked, which is caught, so at most another copy of it
    /// is delivered with the same mask; it never unblocks a default-fatal
    /// signal. It is applied as Linux applies it all the same.
    pub(crate) fn resolve(
        &self,
        mut action: impl FnMut(i32) -> Option<CaughtAction>,
    ) -> Result<WaitSignalEnd, ActionUnknown> {
        let mut blocked = self.mask;
        let (mut thread, mut shared) = (self.thread, self.shared);
        let mut first = None;
        let queued = |thread: u64, shared: u64, set: u64| KernelSignalState {
            pending: (thread | shared) & set,
            thread_pending: thread & set,
            shared_pending: shared & set,
            ..KernelSignalState::default()
        };
        while let Some((signal, queue)) = queued(thread, shared, !0).next_dequeued(blocked) {
            let bit = kernel_sigset_bit(signal);
            if self.fatal & bit != 0 {
                return Ok(WaitSignalEnd::Fatal(signal));
            }
            let first = *first.get_or_insert(signal);
            match queue {
                SignalQueue::Thread => thread &= !bit,
                SignalQueue::Shared => shared &= !bit,
            }
            if (thread | shared) & self.fatal & !blocked == 0 {
                return Ok(WaitSignalEnd::Caught(first));
            }
            let Some(caught) = action(signal) else {
                let behind = queued(thread, shared, self.fatal)
                    .next_dequeued(blocked)
                    .map_or(0, |(signal, _)| signal);
                return Err(ActionUnknown {
                    caught: signal,
                    behind,
                });
            };
            blocked |= caught.mask;
            if caught.flags & libc::SA_NODEFER as u64 == 0 {
                blocked |= bit;
            }
        }
        Ok(WaitSignalEnd::Caught(
            first.expect("a walk starts with a deliverable caught signal"),
        ))
    }
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
/// `state` under the guest's mask `mask`) ends means for the call's result,
/// or `None` when nothing ends it, in Linux's order (v6.12):
///
/// 1. A deliverable default-fatal signal that does not dump core makes it
///    `Fatal`: `complete_signal` starts the group exit when such a signal is
///    sent to a thread that wants it, queueing SIGKILL to every thread
///    (kernel/signal.c:1034-1054, its `!sig_kernel_coredump(sig)` branch), so
///    the process dies at the return whatever else is pending. The signal
///    named is the first such one Linux would dequeue (see `Fatal` for the
///    exit status).
/// 2. Otherwise the first deliverable signal in `get_signal`'s dequeue order
///    comes first: `dequeue_signal` takes the thread's private queue before
///    the shared one (kernel/signal.c:634-639), and `next_signal` the
///    synchronous signals first, then the lowest number (:196-218); here
///    `KernelSignalState::next_dequeued`. A core-dumping one is `Fatal`:
///    `get_signal` ends the process for it (:2881-2905). A caught one is
///    `Caught` when no default-fatal signal is deliverable; when a
///    core-dumping one is, the end depends on the masks of the handlers Linux
///    sets up before reaching it, so it is `BehindCaught`, which the restart
///    checkpoint resolves ([`DequeueWalk::resolve`]).
pub(crate) fn classify(
    state: &KernelSignalState,
    deliverable: u64,
    mask: u64,
) -> Option<WaitSignalEnd> {
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
    let group_exit = fatal & !core_dumping();
    if group_exit != 0 {
        let (signal, _) = among(group_exit).next_dequeued(0)?;
        return Some(WaitSignalEnd::Fatal(signal));
    }
    let (signal, _) = among(deliverable).next_dequeued(0)?;
    if fatal & kernel_sigset_bit(signal) != 0 {
        Some(WaitSignalEnd::Fatal(signal))
    } else if fatal != 0 {
        Some(WaitSignalEnd::BehindCaught(DequeueWalk {
            thread: state.thread_pending & deliverable,
            shared: state.shared_pending & deliverable,
            fatal,
            mask,
        }))
    } else {
        Some(WaitSignalEnd::Caught(signal))
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

/// [`deliverable`] then [`classify`].
pub(crate) fn wait_end(
    state: &KernelSignalState,
    mask: u64,
    reserved: u64,
) -> Option<WaitSignalEnd> {
    wait_end_with_virtual(state, mask, reserved, 0)
}

/// [`wait_end`] with `virtual_caught`, caught signals the scheduler knows
/// are pending on the process's shared queue although no physical copy is:
/// signal phase 1's virtual SIGALRM, which Linux's `ITIMER_REAL` and `alarm`
/// queue on the shared queue (`it_real_fn`, kernel/time/itimer.c:156-166 at
/// v6.12) and which the scheduler's ledger delivers only when the guest's
/// mask admits it. They join the deliverable set and take their place in
/// Linux's dequeue order ([`classify`]).
pub(crate) fn wait_end_with_virtual(
    state: &KernelSignalState,
    mask: u64,
    reserved: u64,
    virtual_caught: u64,
) -> Option<WaitSignalEnd> {
    let deliverable = deliverable(state, mask, reserved) | virtual_caught;
    let state = KernelSignalState {
        pending: state.pending | virtual_caught,
        shared_pending: state.shared_pending | virtual_caught,
        caught: state.caught | virtual_caught,
        ignored: state.ignored & !virtual_caught,
        ..*state
    };
    classify(&state, deliverable, mask)
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

    /// `complete_signal`'s group exit at send (classify, rule 1): a deliverable
    /// default-fatal signal that does not dump core ends the wait as `Fatal`
    /// whatever Linux would dequeue first. Mutation "the first dequeued
    /// decides every end" (rule 1 removed): each row becomes `Caught` or names
    /// the core-dumping signal.
    #[test]
    fn a_non_core_dumping_fatal_signal_makes_the_end_fatal_whatever_is_dequeued_first() {
        let usr1 = bit(libc::SIGUSR1);
        for (name, thread, shared, caught, end) in [
            (
                "a private caught signal before a shared SIGTERM",
                usr1,
                bit(libc::SIGTERM),
                usr1,
                libc::SIGTERM,
            ),
            (
                "a lower-numbered private caught signal before a private SIGTERM",
                usr1 | bit(libc::SIGTERM),
                0,
                usr1,
                libc::SIGTERM,
            ),
            (
                "a synchronous core-dumping signal before a SIGTERM",
                bit(libc::SIGSEGV) | bit(libc::SIGTERM),
                0,
                0,
                libc::SIGTERM,
            ),
        ] {
            let s = state(thread, shared, 0, caught);
            assert_eq!(
                wait_end(&s, 0, 0),
                Some(WaitSignalEnd::Fatal(end)),
                "{name}"
            );
        }
    }

    /// A caught action for the walk's lookups: `mask`, and `SA_NODEFER` or not.
    fn action(mask: u64, nodefer: bool) -> CaughtAction {
        CaughtAction {
            mask,
            flags: if nodefer { libc::SA_NODEFER as u64 } else { 0 },
        }
    }

    /// The walk a `BehindCaught` end carries, for `state` and `mask`.
    fn walk(state: &KernelSignalState, mask: u64) -> DequeueWalk {
        match classify(state, deliverable(state, mask, 0), mask) {
            Some(WaitSignalEnd::BehindCaught(walk)) => walk,
            other => panic!("not a walk: {other:?}"),
        }
    }

    /// The end of a wait on `state` (guest mask 0), a `BehindCaught` one
    /// resolved with `action`.
    fn end_of(
        state: &KernelSignalState,
        action: impl FnMut(i32) -> Option<CaughtAction>,
    ) -> Result<WaitSignalEnd, ActionUnknown> {
        match classify(state, deliverable(state, 0, 0), 0) {
            Some(WaitSignalEnd::BehindCaught(walk)) => walk.resolve(action),
            Some(end) => Ok(end),
            None => panic!("nothing ends the wait"),
        }
    }

    /// Rule 2 with a caught signal first and a core-dumping one deliverable
    /// behind it (Claude review round 2): Linux sets up the caught handler and
    /// goes on delivering under its `sa_mask` (signal_delivered,
    /// kernel/signal.c:2941-2954), so the core-dumping signal ends the
    /// process unless that mask, or a later handler's, holds it. Rows: (a) the
    /// handler's mask blocks it, `Caught` (a private caught signal before a
    /// shared, lower-numbered SIGQUIT; both private, the caught one
    /// lower-numbered); (b) it does not, `Fatal`; (c) `SA_NODEFER` or not, the
    /// same end; (d) two caught signals whose masks together block it,
    /// `Caught`. Mutations: "the handler's sa_mask ignored" ((a), (d) become
    /// `Fatal`), "only the first handler's mask" ((d) becomes `Fatal`), "any
    /// default-fatal signal makes the end fatal" ((a), (d)), "the lowest
    /// number across both queues" (the first (a) row becomes `Fatal(SIGQUIT)`
    /// at classification).
    #[test]
    fn a_caught_signal_dequeued_before_a_core_dumping_one_is_resolved_by_the_handlers_masks() {
        let (hup, usr1, usr2) = (bit(libc::SIGHUP), bit(libc::SIGUSR1), bit(libc::SIGUSR2));
        let (quit, xcpu) = (bit(libc::SIGQUIT), bit(libc::SIGXCPU));
        type Actions = &'static [(i32, u64, bool)];
        let rows: [(&str, u64, u64, u64, Actions, WaitSignalEnd); 7] = [
            (
                "(a) the private caught signal's mask blocks a shared SIGQUIT",
                usr2,
                quit,
                usr2,
                &[(libc::SIGUSR2, 1 << (libc::SIGQUIT - 1), false)],
                WaitSignalEnd::Caught(libc::SIGUSR2),
            ),
            (
                "(a) both private, the caught one's mask blocks SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 1 << (libc::SIGQUIT - 1), false)],
                WaitSignalEnd::Caught(libc::SIGHUP),
            ),
            (
                "(b) the caught signal's mask does not block SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 1 << (libc::SIGUSR1 - 1), false)],
                WaitSignalEnd::Fatal(libc::SIGQUIT),
            ),
            (
                "(c) SA_NODEFER, mask blocking SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 1 << (libc::SIGQUIT - 1), true)],
                WaitSignalEnd::Caught(libc::SIGHUP),
            ),
            (
                "(c) SA_NODEFER, mask not blocking SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 0, true)],
                WaitSignalEnd::Fatal(libc::SIGQUIT),
            ),
            (
                "(d) two caught signals whose masks together block SIGXCPU",
                hup | usr1 | xcpu,
                0,
                hup | usr1,
                &[
                    (libc::SIGHUP, 0, false),
                    (libc::SIGUSR1, 1 << (libc::SIGXCPU - 1), false),
                ],
                WaitSignalEnd::Caught(libc::SIGHUP),
            ),
            (
                "(d) neither mask blocks SIGXCPU",
                hup | usr1 | xcpu,
                0,
                hup | usr1,
                &[(libc::SIGHUP, 0, false), (libc::SIGUSR1, 0, false)],
                WaitSignalEnd::Fatal(libc::SIGXCPU),
            ),
        ];
        // Every row is checked before the test fails, so that a mutation's
        // failure names the rows it changes.
        let mut wrong = Vec::new();
        for (name, thread, shared, caught, actions, end) in rows {
            let s = state(thread, shared, 0, caught);
            let resolved = end_of(&s, |signal| {
                actions
                    .iter()
                    .find(|(known, _, _)| *known == signal)
                    .map(|(_, mask, nodefer)| action(*mask, *nodefer))
            });
            if resolved != Ok(end) {
                wrong.push(format!("{name}: {resolved:?}"));
            }
        }
        // The SA_NODEFER rows give the same ends as without it.
        let s = state(hup | quit, 0, 0, hup);
        for mask in [0, quit] {
            let with = end_of(&s, |_| Some(action(mask, true)));
            let without = end_of(&s, |_| Some(action(mask, false)));
            if with != without {
                wrong.push(format!("(c) SA_NODEFER changes the end, mask {mask:#x}"));
            }
        }
        assert!(wrong.is_empty(), "rows: {wrong:#?}");
    }

    /// (e) An action the checkpoint cannot read while a default-fatal signal
    /// is still deliverable behind its caught signal is `ActionUnknown`, which
    /// the checkpoint fails closed on; one it no longer needs is not asked
    /// for (the first handler's mask already holds the default-fatal signal).
    /// Mutation "no fail-closed" (a missing action taken as an empty mask):
    /// the unknown row becomes `Fatal(SIGQUIT)`.
    #[test]
    fn an_unreadable_caught_action_with_a_fatal_signal_behind_it_is_unknown() {
        let (hup, usr1, quit) = (bit(libc::SIGHUP), bit(libc::SIGUSR1), bit(libc::SIGQUIT));
        let s = state(hup | quit, 0, 0, hup);
        assert_eq!(
            walk(&s, 0).resolve(|_| None),
            Err(ActionUnknown {
                caught: libc::SIGHUP,
                behind: libc::SIGQUIT,
            })
        );
        let s = state(hup | usr1 | quit, 0, 0, hup | usr1);
        let mut asked = Vec::new();
        let resolved = walk(&s, 0).resolve(|signal| {
            asked.push(signal);
            (signal == libc::SIGHUP).then_some(action(quit, false))
        });
        assert_eq!(resolved, Ok(WaitSignalEnd::Caught(libc::SIGHUP)));
        assert_eq!(asked, [libc::SIGHUP]);
    }

    /// Rule 2 with a core-dumping signal first: `get_signal` ends the process
    /// for it, so the end is `Fatal` even though a caught signal is also
    /// deliverable. Mutations: "the caught signal always wins over a
    /// core-dumping one" (each row becomes `Caught`) and "synchronous signals
    /// not first" (the SIGSEGV row becomes `Caught(SIGHUP)`).
    #[test]
    fn a_core_dumping_signal_dequeued_first_makes_the_end_fatal() {
        for (name, thread, shared, caught, end) in [
            (
                "both private, the core-dumping one lower-numbered",
                bit(libc::SIGQUIT) | bit(libc::SIGUSR1),
                0,
                bit(libc::SIGUSR1),
                libc::SIGQUIT,
            ),
            (
                "a synchronous core-dumping signal before a lower-numbered caught one",
                bit(libc::SIGSEGV) | bit(libc::SIGHUP),
                0,
                bit(libc::SIGHUP),
                libc::SIGSEGV,
            ),
            (
                "a private core-dumping signal before a shared caught one",
                bit(libc::SIGABRT),
                bit(libc::SIGHUP),
                bit(libc::SIGHUP),
                libc::SIGABRT,
            ),
        ] {
            let s = state(thread, shared, 0, caught);
            assert_eq!(
                wait_end(&s, 0, 0),
                Some(WaitSignalEnd::Fatal(end)),
                "{name}"
            );
        }
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
