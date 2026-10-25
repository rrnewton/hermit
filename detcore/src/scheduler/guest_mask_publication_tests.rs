/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1e A: the scheduler's side of IR-18's tri-state, fork-child
//! seeding, IR-23(a)'s temporary-mask publication and the committed-send
//! window record (`guest_mask_publication`), with the gate forced on in the
//! Detcore `Config` and the in-guest backend's capabilities. Each test names
//! the mutation it catches.

use super::guest_mask_publication::CommittedSendWindow;
use super::guest_mask_publication::PublishedGuestMask;
use super::guest_mask_publication::TemporaryMaskControl;
use super::*;

const PARENT: DetTid = DetTid::from_raw(100);
const THREAD: DetTid = DetTid::from_raw(101);
const CHILD: DetTid = DetTid::from_raw(102);

fn bit(signal: i32) -> u64 {
    kernel_signal_bit(signal)
}

/// An in-guest scheduler with the delivery gate `gate`: `PARENT` and its
/// thread `THREAD`, the kernel's `SigBlk` of `THREAD` scripted as `kernel`.
fn gated(gate: bool, kernel: u64) -> Scheduler {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let mut scheduler = Scheduler::new(&config);
    scheduler.thread_tree.add_child(PARENT, PARENT, true);
    scheduler.thread_tree.add_child(PARENT, THREAD, false);
    for thread in [PARENT, THREAD] {
        scheduler.register_thread_for_test(thread);
    }
    scheduler.test_kernel_signal_states.insert(
        THREAD,
        KernelSignalState {
            blocked: kernel,
            ..Default::default()
        },
    );
    scheduler
}

/// A request of `thread` that publishes `mask`.
fn publishing(thread: DetTid, mask: Option<u64>) -> Resources {
    let mut request = Resources::new(thread);
    request.guest_signal_mask = mask;
    request
}

/// IR-18: a rejoin readmitted at an ordered point (`keep_mask_for_continuation`)
/// that returned under the guest's own mask (`ContinuationMask::Restored`)
/// but published none, on a backend that keeps no guest mask apart, leaves
/// the mask unknown, not the temporary mask its call published: the status
/// reader gets `Unknown` (and refuses, `HERMIT_GUEST_MASK_UNKNOWN`), and the
/// scheduler's own readers take nothing published, the kernel's `SigBlk`.
/// The other rejoins keep 3c.1b's answers: one that publishes its mask
/// records it, a `Temporary` one records the temporary mask, and an unknown
/// one (`rt_sigsuspend`) keeps the call's mask. With the gate off nothing is
/// recorded. Mutations: "entry kept" (the `Restored` row shows the
/// temporary mask), and "unknown for every rejoin that publishes nothing"
/// (the `rt_sigsuspend` row turns unknown).
#[test]
fn ir18_a_restored_rejoin_that_publishes_nothing_leaves_the_mask_unknown() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    let kernel = bit(libc::SIGUSR1) | bit(libc::SIGCHLD);
    for gate in [true, false] {
        for kind in ["restored, none", "restored, own", "temporary", "unknown"] {
            let case = format!("{kind}, gate {gate}");
            let mut scheduler = gated(gate, kernel);
            let mut call = publishing(THREAD, gate.then_some(temporary));
            call.blocked_signal_mask = Some(temporary);
            assert_eq!(scheduler.record_request_guest_mask(THREAD, &call), gate);
            let op = ExternalOpId::new(THREAD, 1);
            let mut rejoin = Resources::new(THREAD);
            rejoin.insert(ResourceID::BlockedExternalContinue(op), Permission::RW);
            match kind {
                "restored, none" => {}
                "restored, own" => rejoin.guest_signal_mask = Some(own),
                "temporary" => {
                    rejoin.blocked_signal_mask = Some(temporary);
                    rejoin.guest_signal_mask = Some(temporary);
                }
                _ => rejoin.blocked_signal_mask_unknown = true,
            }
            if !gate {
                rejoin.guest_signal_mask = None;
            }
            scheduler.next_turns.get_mut(&THREAD).unwrap().req = Ivar::full(Ok(rejoin));
            scheduler.keep_mask_for_continuation(THREAD);
            let want = match (gate, kind) {
                (false, _) => None,
                (true, "restored, none") => Some(PublishedGuestMask::Unknown),
                (true, "restored, own") => Some(PublishedGuestMask::Known(own)),
                (true, _) => Some(PublishedGuestMask::Known(temporary)),
            };
            assert_eq!(scheduler.published_guest_mask_state(THREAD), want, "{case}");
            assert_eq!(scheduler.status_guest_mask(THREAD).mask, want, "{case}");
            let read = match want {
                Some(PublishedGuestMask::Known(mask)) => mask,
                _ => kernel,
            };
            assert_eq!(
                scheduler.read_thread_blocked_mask_result(THREAD),
                Ok(read),
                "{case}: the scheduler's readers"
            );
            // The next request's publication ends an unknown mask.
            scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, Some(own)));
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                gate.then_some(PublishedGuestMask::Known(own)),
                "{case}: after the next request"
            );
        }
    }
}

/// Fork-child seeding: a new task starts with what its creator published,
/// `Known` or `Unknown`, and nothing when its creator published nothing (so
/// its readers take its own `SigBlk`, which Linux copied from its
/// creator's: `arch_dup_task_struct`, kernel/fork.c:1086-1091 at v6.12). A
/// stale publication of a reused tid goes. With the gate off, nothing.
/// Mutation "no seed": every gated row with a parent publication fails.
#[test]
fn a_new_task_starts_with_the_mask_its_creator_published() {
    let own = bit(libc::SIGUSR2);
    for gate in [true, false] {
        for parent in [
            Some(PublishedGuestMask::Known(own)),
            Some(PublishedGuestMask::Unknown),
            None,
        ] {
            let case = format!("{parent:?}, gate {gate}");
            let mut scheduler = gated(gate, 0);
            scheduler.thread_tree.add_child(PARENT, CHILD, true);
            scheduler.register_thread_for_test(CHILD);
            scheduler.put_guest_mask_state(PARENT, parent);
            // A stale entry of a reused tid.
            scheduler.put_guest_mask_state(CHILD, Some(PublishedGuestMask::Known(!0)));
            scheduler.seed_child_guest_sigmask(PARENT, CHILD);
            let want = if gate {
                parent
            } else {
                Some(PublishedGuestMask::Known(!0))
            };
            assert_eq!(scheduler.published_guest_mask_state(CHILD), want, "{case}");
        }
    }
}

/// IR-23(a): a temporary-mask publication makes the published mask the
/// temporary one with D37's held masks, which the status reader gets, and
/// opens the committed-send window. A request the thread files supersedes it
/// (the held masks and the window go, nothing is put back); a withdrawal puts
/// back what was published before it (`Known`, `Unknown` or nothing) and
/// answers the window, once. Another thread cannot publish or withdraw for
/// it. With the gate off nothing is taken. Mutations: "withdrawal puts
/// nothing back" (the prior rows), "held masks dropped" (the status rows),
/// "a request does not supersede" (the held masks survive the request).
#[test]
fn a_temporary_mask_publication_holds_until_a_request_or_its_withdrawal() {
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    let urg = bit(libc::SIGURG);
    let winch = bit(libc::SIGWINCH);
    for gate in [true, false] {
        for prior in [
            Some(PublishedGuestMask::Known(own)),
            Some(PublishedGuestMask::Unknown),
            None,
        ] {
            let case = format!("{prior:?}, gate {gate}");
            let mut scheduler = gated(gate, 0);
            scheduler.put_guest_mask_state(THREAD, prior);
            let publish = TemporaryMaskControl::Publish {
                thread: THREAD,
                mask: temporary,
                held_private: urg,
                held_shared: winch,
            };
            // Not for another thread.
            assert_eq!(scheduler.temporary_mask_control(PARENT, publish), None);
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                prior,
                "{case}: refused for another sender"
            );
            assert_eq!(scheduler.temporary_mask_control(THREAD, publish), None);
            let status = scheduler.status_guest_mask(THREAD);
            if gate {
                assert_eq!(
                    status.mask,
                    Some(PublishedGuestMask::Known(temporary)),
                    "{case}"
                );
                assert_eq!(
                    (status.held_private, status.held_shared),
                    (urg, winch),
                    "{case}"
                );
            } else {
                assert_eq!(status.mask, prior, "{case}");
                assert_eq!((status.held_private, status.held_shared), (0, 0), "{case}");
            }
            scheduler.note_committed_send_in_window(THREAD, libc::SIGRTMIN() + 2, true);
            scheduler.note_committed_send_in_window(THREAD, libc::SIGUSR1, false);
            let withdraw = TemporaryMaskControl::Withdraw { thread: THREAD };
            let window = scheduler.temporary_mask_control(THREAD, withdraw);
            if gate {
                assert_eq!(
                    window,
                    Some(CommittedSendWindow {
                        sent_private: bit(libc::SIGRTMIN() + 2),
                        sent_shared: bit(libc::SIGUSR1),
                    }),
                    "{case}"
                );
            } else {
                assert_eq!(window, None, "{case}");
            }
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                prior,
                "{case}: put back"
            );
            assert_eq!(
                scheduler.status_guest_mask(THREAD).held_private,
                0,
                "{case}"
            );
            // Once only.
            assert_eq!(
                scheduler.temporary_mask_control(THREAD, withdraw),
                None,
                "{case}"
            );

            // A request supersedes a publication: nothing is put back, and
            // the held masks and the window go.
            scheduler.temporary_mask_control(THREAD, publish);
            scheduler.record_request_guest_mask(THREAD, &publishing(THREAD, Some(own)));
            let status = scheduler.status_guest_mask(THREAD);
            assert_eq!((status.held_private, status.held_shared), (0, 0), "{case}");
            scheduler.note_committed_send_in_window(THREAD, libc::SIGUSR1, false);
            assert_eq!(
                scheduler.temporary_mask_control(THREAD, withdraw),
                None,
                "{case}"
            );
            assert_eq!(
                scheduler.published_guest_mask_state(THREAD),
                if gate {
                    Some(PublishedGuestMask::Known(own))
                } else {
                    prior
                },
                "{case}: the request's own mask"
            );
        }
    }
}

/// The committed-send window record at the scheduler's own commit point
/// (`send_guest_signal`, here through its unit-test send, which stands for a
/// `kill` that succeeded): a thread-directed send joins the private queue's
/// record, a process-directed one the shared queue's, only while the target
/// holds a temporary-mask publication. Mutation "the scheduler's sends not
/// recorded": the window is empty.
#[test]
fn the_window_records_the_schedulers_sends_by_queue_while_a_publication_is_in_force() {
    let mut scheduler = gated(true, 0);
    scheduler.test_sent_signals = Some(Vec::new());
    // Not in force: nothing is recorded.
    scheduler.send_guest_signal(
        THREAD,
        Signal::SIGUSR1,
        None,
        None,
        THREAD,
        discarded_at_send::SendFate::Classify(None),
    );
    assert!(scheduler.publish_temporary_mask(THREAD, 0, 0, 0));
    scheduler.send_guest_signal(
        THREAD,
        Signal::SIGUSR2,
        Some(PARENT),
        None,
        THREAD,
        discarded_at_send::SendFate::Classify(None),
    );
    scheduler.send_guest_signal(
        THREAD,
        Signal::SIGALRM,
        None,
        None,
        THREAD,
        discarded_at_send::SendFate::Classify(None),
    );
    assert_eq!(
        scheduler.withdraw_temporary_mask(THREAD),
        Some(CommittedSendWindow {
            sent_private: bit(libc::SIGUSR2),
            sent_shared: bit(libc::SIGALRM),
        })
    );
}
