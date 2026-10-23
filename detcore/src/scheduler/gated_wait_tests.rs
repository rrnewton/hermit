/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1c: the scheduler's side of the parked-wait checks under the
//! in-guest delivery gate (`Config::in_guest_signal_delivery`; design 16.11,
//! D50 readers 1, 8 and 9). The gate is forced on in the Detcore `Config`
//! with the in-guest backend's capabilities, whose interruption capability
//! fields stay false, so every wake below is the gate's.
//!
//! Each parked wait kind is sent a signal at a committed point, both as a
//! guest's cross-task send (`notify_signal_pending`, then the step-2 drain)
//! and as the scheduler's own send (`wake_signaled_guest_as`). The target's
//! `/proc` state is scripted, and its guest mask is the one it published
//! (`saved_guest_sigmasks`, step 3c.1b). Mutations each assertion catches:
//! - "SigBlk used": the scripted kernel mask is the in-guest runtime's, all
//!   blocked, for the rows that must wake, and empty for the row the
//!   published mask blocks, so a predicate that read `SigBlk` gets both
//!   wrong;
//! - "ignored counted as deliverable": the `SIG_IGN` and default-ignored
//!   rows stay parked;
//! - "the reader left on the field alone": the capability field is false, so
//!   a restartable poll, and the parked pipe write among them, would never
//!   wake.

use super::*;
use crate::resources::NANOSLEEP_FYI;
use crate::resources::PAUSE_FYI;

const TARGET: DetTid = DetTid::from_raw(100);
const OTHER: DetTid = DetTid::from_raw(200);

fn bit(signal: i32) -> u64 {
    kernel_signal_bit(signal)
}

/// An in-guest scheduler with the delivery gate `gate`, and threads 100 and
/// 200, each the leader of its own process.
fn gated(gate: bool) -> Scheduler {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    assert!(!config.backend.supports_parked_write_signal_interruption);
    assert!(!config.backend.supports_blocked_wait_signal_interruption);
    let mut scheduler = Scheduler::new(&config);
    assert!(!scheduler.models_signal_targets);
    for thread in [TARGET, OTHER] {
        scheduler.thread_tree.add_child(thread, thread, true);
        scheduler.register_thread_for_test(thread);
    }
    scheduler
}

/// The parked wait kinds D50 gives step 3c.1c.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Reader 1/2: a restartable internal IO poll (a read of an empty pipe),
    /// queued.
    Poll,
    /// Reader 1: a parked pipe write, which decides the disposition itself.
    PipeWrite,
    /// Reader 8: `pause`, parked in the timed waiters.
    Pause,
    /// Reader 8: `nanosleep` with a deadline ahead, parked.
    Nanosleep,
    /// Reader 9: a precise futex wait.
    Futex,
}

const KINDS: [Kind; 5] = [
    Kind::Poll,
    Kind::PipeWrite,
    Kind::Pause,
    Kind::Nanosleep,
    Kind::Futex,
];

/// Park `thread` in a wait of `kind`, having published `mask`.
fn park(scheduler: &mut Scheduler, thread: DetTid, kind: Kind, mask: u64) {
    assert!(scheduler.set_saved_guest_sigmask(thread, Some(mask)));
    match kind {
        Kind::Poll | Kind::PipeWrite => {
            let mut polling = Resources::new(thread);
            polling.insert(ResourceID::InternalIOPolling, Permission::W);
            polling.set_signal_interrupt_errno(Errno::ERESTARTSYS);
            polling.signal_state_task = Some((thread.as_raw(), thread.as_raw()));
            polling.decides_signal_interruption_itself = kind == Kind::PipeWrite;
            scheduler.next_turns.get_mut(&thread).unwrap().req = Ivar::full(Ok(polling));
            scheduler.runqueue_push_back(thread);
        }
        Kind::Pause | Kind::Nanosleep => {
            let (deadline, fyi) = if kind == Kind::Pause {
                (LogicalTime::INDEFINITE, PAUSE_FYI)
            } else {
                (LogicalTime::from_nanos(5_000_000_000), NANOSLEEP_FYI)
            };
            let mut request = Resources::new(thread);
            request.insert(ResourceID::SleepUntil(deadline), Permission::W);
            request.fyi(fyi);
            scheduler.next_turns.get_mut(&thread).unwrap().req = Ivar::full(Ok(request));
            scheduler.blocked.timed_waiters.insert(deadline, thread);
        }
        Kind::Futex => {
            let futex = FutexID::private(MmId::initial(DetPid::from_raw(thread.as_raw())), 0x4040);
            // On the in-guest backends a futex wait reports a watch only
            // under the gate (`handle_futex_blocking`), as here.
            let watch = scheduler
                .in_guest_signal_delivery
                .then_some(FutexSignalWatch {
                    unblocked: !mask,
                    defers_default_stops: false,
                    pid: thread.as_raw(),
                    tid: thread.as_raw(),
                });
            scheduler.sleep_futex_waiter(&thread, futex, None, u32::MAX, watch);
        }
    }
}

/// A `/proc` state with `pending` in the thread's queue, `shared` in its
/// process's, the kernel mask `kernel` (the in-guest runtime's under the
/// gate), and `caught` and `ignored` as the dispositions.
fn state(pending: u64, shared: u64, kernel: u64, caught: u64, ignored: u64) -> KernelSignalState {
    KernelSignalState {
        pending: pending | shared,
        thread_pending: pending,
        shared_pending: shared,
        blocked: kernel,
        caught,
        ignored,
    }
}

/// Whether `thread`'s wait was ended for `signal`: its request now carries
/// the signal (the scheduler's `Signaled` answer at its next turn).
fn woken(scheduler: &Scheduler, thread: DetTid, signal: i32) -> bool {
    scheduler
        .inbound_signals(thread)
        .contains(&SigWrapper(signal))
}

/// One row of the table: the signal sent, the published mask, the scripted
/// state, and whether the wait must end.
struct Row {
    name: &'static str,
    signal: i32,
    published: u64,
    state: KernelSignalState,
    ends: bool,
}

fn rows() -> Vec<Row> {
    let term = bit(libc::SIGTERM);
    vec![
        Row {
            name: "default-fatal, unblocked by the published mask, the kernel's all blocked",
            signal: libc::SIGTERM,
            published: 0,
            state: state(term, 0, !0, 0, 0),
            ends: true,
        },
        Row {
            name: "core-dumping, in the shared queue (https://github.com/rrnewton/hermit/issues/4571)",
            signal: libc::SIGQUIT,
            published: 0,
            state: state(0, bit(libc::SIGQUIT), !0, 0, 0),
            ends: true,
        },
        Row {
            name: "caught, unblocked by the published mask",
            signal: libc::SIGUSR1,
            published: 0,
            state: state(bit(libc::SIGUSR1), 0, !0, bit(libc::SIGUSR1), 0),
            ends: true,
        },
        Row {
            name: "blocked by the published mask, the kernel's unblocked",
            signal: libc::SIGTERM,
            published: term,
            state: state(term, 0, 0, 0, 0),
            ends: false,
        },
        Row {
            name: "SIG_IGN, held pending by the runtime mask",
            signal: libc::SIGTERM,
            published: 0,
            state: state(term, 0, !0, 0, term),
            ends: false,
        },
        Row {
            name: "default-ignored",
            signal: libc::SIGURG,
            published: 0,
            state: state(bit(libc::SIGURG), 0, !0, 0, 0),
            ends: false,
        },
    ]
}

/// D50 readers 1, 8 and 9, at a guest's cross-task send: a parked wait of
/// each kind ends for a deliverable signal (default-fatal, core-dumping,
/// caught) and stays parked for one its published mask blocks or that is
/// ignored.
#[test]
fn under_the_gate_a_guest_send_ends_each_parked_wait_only_for_a_deliverable_signal() {
    for kind in KINDS {
        for row in rows() {
            let mut scheduler = gated(true);
            park(&mut scheduler, TARGET, kind, row.published);
            scheduler
                .test_kernel_signal_states
                .insert(TARGET, row.state);
            scheduler.notify_signal_pending(TARGET, SigWrapper(row.signal));
            scheduler.drain_pending_cross_task_signals();
            assert_eq!(
                woken(&scheduler, TARGET, row.signal),
                row.ends,
                "{kind:?}: {}",
                row.name
            );
        }
    }
}

/// The same table at the scheduler's own send (a timer, a child-exit
/// signal), `wake_signaled_guest_as`, at its commit.
#[test]
fn under_the_gate_a_scheduler_send_ends_each_parked_wait_only_for_a_deliverable_signal() {
    for kind in KINDS {
        for row in rows() {
            let mut scheduler = gated(true);
            park(&mut scheduler, TARGET, kind, row.published);
            scheduler
                .test_kernel_signal_states
                .insert(TARGET, row.state);
            scheduler.wake_signaled_guest_as(TARGET, SigWrapper(row.signal), false);
            assert_eq!(
                woken(&scheduler, TARGET, row.signal),
                row.ends,
                "{kind:?}: {}",
                row.name
            );
        }
    }
}

/// With the gate off the same in-guest scheduler wakes none of these waits
/// for a guest's send, as before step 3c.1c: the capability fields are
/// false, and nothing else turns the readers on.
#[test]
fn with_the_gate_off_a_guest_send_wakes_no_parked_wait_on_in_guest() {
    for kind in KINDS {
        let mut scheduler = gated(false);
        park(&mut scheduler, TARGET, kind, 0);
        scheduler
            .test_kernel_signal_states
            .insert(TARGET, state(bit(libc::SIGTERM), 0, 0, 0, 0));
        scheduler.notify_signal_pending(TARGET, SigWrapper(libc::SIGTERM));
        scheduler.drain_pending_cross_task_signals();
        assert!(!woken(&scheduler, TARGET, libc::SIGTERM), "{kind:?}");
    }
}

/// The check covers every parked thread a send reaches, and leaves alone one
/// it does not: with the signal pending only in the target's process, a loop
/// over both parked threads (the form a group or broadcast send would take,
/// which no live path has under the gate, since every non-SIGKILL group send
/// is refused, F15) ends the target's wait and leaves the other's, whose
/// state holds nothing, parked.
#[test]
fn under_the_gate_every_reached_parked_thread_is_checked_and_an_unreached_one_stays_parked() {
    for kind in KINDS {
        let mut scheduler = gated(true);
        park(&mut scheduler, TARGET, kind, 0);
        park(&mut scheduler, OTHER, kind, 0);
        scheduler
            .test_kernel_signal_states
            .insert(TARGET, state(0, bit(libc::SIGHUP), !0, 0, 0));
        scheduler
            .test_kernel_signal_states
            .insert(OTHER, state(0, 0, !0, 0, 0));
        for thread in [TARGET, OTHER] {
            scheduler.notify_signal_pending(thread, SigWrapper(libc::SIGHUP));
        }
        scheduler.drain_pending_cross_task_signals();
        assert!(woken(&scheduler, TARGET, libc::SIGHUP), "{kind:?}");
        assert!(!woken(&scheduler, OTHER, libc::SIGHUP), "{kind:?}");
    }
}

/// A restartable poll is admitted in the sender's turn under its published
/// mask, not `SigBlk` (the 3c.1b carry: `poll_mask_admits` on the gate path).
#[test]
fn under_the_gate_a_poll_is_admitted_by_its_published_mask() {
    let term = SigWrapper(libc::SIGTERM);
    let mut scheduler = gated(true);
    park(&mut scheduler, TARGET, Kind::Poll, bit(libc::SIGTERM));
    scheduler
        .test_kernel_signal_states
        .insert(TARGET, state(bit(libc::SIGTERM), 0, 0, 0, 0));
    assert!(!scheduler.poll_mask_admits(TARGET, term));
    assert!(scheduler.set_saved_guest_sigmask(TARGET, Some(0)));
    scheduler
        .test_kernel_signal_states
        .insert(TARGET, state(bit(libc::SIGTERM), 0, !0, 0, 0));
    assert!(scheduler.poll_mask_admits(TARGET, term));
    // Gate off: every signal is admitted, as before.
    let mut scheduler = gated(false);
    park(&mut scheduler, TARGET, Kind::Poll, bit(libc::SIGTERM));
    assert!(scheduler.poll_mask_admits(TARGET, term));
}
