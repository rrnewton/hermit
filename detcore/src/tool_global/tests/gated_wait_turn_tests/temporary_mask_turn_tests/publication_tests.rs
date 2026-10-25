/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1e A (plan-3c1e-r4 section 1 A, with revision 3 and 2 section
//! 1 A): IR-23 and D37's R2 with private and shared held masks at the masked
//! waits' entry and end (`temporary_mask_forms::TemporaryMaskCall`), and the
//! `/proc` status reader's IR-18 refusal, on the waiting thread's own side,
//! with the parent modules' harness. These rows need step 3c.1e B's runtime
//! mask end to end (an ignored signal is queued only under it), so they are
//! unit-level here and re-run end to end in step 3c.1e C. Each test names the
//! mutation it catches.

use super::*;
use crate::scheduler::guest_mask_publication::CommittedSendWindow;
use crate::scheduler::guest_mask_publication::PublishedGuestMask;
use crate::scheduler::guest_mask_publication::StatusGuestMask;
use crate::scheduler::guest_mask_publication::TemporaryMaskControl;

/// The five masked sites that publish expired virtual timerfds before a
/// first probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Site {
    /// The internal `pselect6` path (`handle_internal_pselect6`), a 1 s
    /// timeout.
    InternalPselect6,
    /// The staged `pselect6` (`gated_staged_pselect6`), a zero timeout.
    StagedPselect6,
    /// `gated_masked_ppoll`, a 1 s timeout.
    Ppoll,
    /// `gated_zero_timeout_epoll`, a zero timeout.
    ZeroEpoll,
    /// `gated_masked_epoll`, a 1 s timeout.
    Epoll,
}

const SITES: [Site; 5] = [
    Site::InternalPselect6,
    Site::StagedPselect6,
    Site::Ppoll,
    Site::ZeroEpoll,
    Site::Epoll,
];

impl Site {
    fn zero(self) -> bool {
        matches!(self, Site::StagedPselect6 | Site::ZeroEpoll)
    }

    /// The guest memory the site's call names: its timeout and its temporary
    /// `mask`.
    fn memory(self, mask: u64) -> FormMemory {
        FormMemory::new(Some(if self.zero() { 0 } else { 1_000_000_000 }), mask)
    }

    /// Runs the site's call through its handler.
    async fn run(self, guest: &mut TurnGuest, memory: &FormMemory) -> Result<i64, Error> {
        let tool = tool(guest);
        match self {
            Site::InternalPselect6 | Site::StagedPselect6 => {
                tool.handle_pselect6(guest, pselect6(memory, 1, FormMemory::SET, true, true))
                    .await
            }
            Site::Ppoll => {
                tool.handle_ppoll(guest, ppoll(memory, FormMemory::SET))
                    .await
            }
            Site::ZeroEpoll => tool.handle_epoll_pwait(guest, epoll_pwait(memory, 0)).await,
            Site::Epoll => {
                tool.handle_epoll_pwait(guest, epoll_pwait(memory, 1000))
                    .await
            }
        }
    }
}

/// A scripted `/proc` state with `private` and `shared` pending, the kernel
/// mask the in-guest runtime's (all blocked), and `ignored` set to `SIG_IGN`.
fn queues(private: u64, shared: u64, ignored: u64) -> KernelSignalState {
    KernelSignalState {
        pending: private | shared,
        thread_pending: private,
        shared_pending: shared,
        blocked: !0,
        caught: 0,
        ignored,
    }
}

/// The index of the first RPC `which` picks, if any.
fn first(guest: &TurnGuest, which: impl Fn(&GlobalRequest) -> bool) -> Option<usize> {
    guest.rpcs().iter().position(which)
}

fn is_publish(rpc: &GlobalRequest) -> bool {
    matches!(
        rpc,
        GlobalRequest::PublishTemporaryMask(TemporaryMaskControl::Publish { .. })
    )
}

fn is_withdraw(rpc: &GlobalRequest) -> bool {
    matches!(
        rpc,
        GlobalRequest::PublishTemporaryMask(TemporaryMaskControl::Withdraw { .. })
    )
}

/// The publications the thread filed, with their payloads.
fn publications(guest: &TurnGuest) -> Vec<(u64, u64, u64)> {
    guest
        .rpcs()
        .into_iter()
        .filter_map(|rpc| match rpc {
            GlobalRequest::PublishTemporaryMask(TemporaryMaskControl::Publish {
                mask,
                held_private,
                held_shared,
                ..
            }) => Some((mask, held_private, held_shared)),
            _ => None,
        })
        .collect()
}

/// IR-23(a) and the site census: with an armed virtual timerfd, each of the
/// five masked sites publishes its temporary mask, as Linux installs it,
/// exactly once per call, before the first clock observation (the
/// expired-timerfd publication's, or IR-23(c)'s deadline) and before any
/// request of the call; the zero-timeout `epoll_pwait` after the empty
/// request that precedes its entry. A call that files no later request
/// withdraws it at its end; a parked one, whose requests publish the
/// temporary mask, does not. Without an armed timerfd nothing is published
/// or withdrawn: no RPC is added. Mutations: "filed per iteration" (the
/// parked rows count more than one), "published after the RPC" (the order
/// rows), "a site skipped" (that site's row has none), "always published"
/// (the unarmed rows).
#[tokio::test]
async fn ir23_each_masked_site_publishes_its_temporary_mask_once_before_the_timerfd_rpc() {
    let own = bit(libc::SIGUSR1);
    let temporary = bit(libc::SIGTERM) | bit(libc::SIGKILL);
    let installed = bit(libc::SIGTERM);
    for armed in [true, false] {
        for site in SITES {
            let case = format!("{site:?}, armed {armed}");
            let memory = site.memory(temporary);
            let mut guest = turn_guest(true, Some(own));
            if armed {
                arm_virtual_timerfd(&mut guest);
            }
            let _seam = script(proc_state(0, 0, 0));
            let result = site.run(&mut guest, &memory).await;
            assert_eq!(errno_result(&result), Ok(0), "{case}");
            let published = publications(&guest);
            if !armed {
                assert!(published.is_empty(), "{case}: nothing published");
                assert_eq!(first(&guest, is_withdraw), None, "{case}");
                continue;
            }
            assert_eq!(
                published,
                vec![(installed, 0, 0)],
                "{case}: once, as installed"
            );
            let publish = first(&guest, is_publish).unwrap();
            let clock = first(&guest, |rpc| {
                matches!(rpc, GlobalRequest::GlobalTimeLowerBound)
            })
            .expect("the timerfd publication observes the clock");
            assert!(publish < clock, "{case}: published before the RPC");
            let requests: Vec<usize> = guest
                .rpcs()
                .iter()
                .enumerate()
                .filter(|(_, rpc)| matches!(rpc, GlobalRequest::RequestResources(..)))
                .map(|(index, _)| index)
                .collect();
            if site == Site::ZeroEpoll {
                assert_eq!(requests.len(), 1, "{case}: the empty request only");
                assert!(
                    requests[0] < publish,
                    "{case}: the request comes before the entry"
                );
            } else {
                assert!(
                    requests.iter().all(|request| *request > publish),
                    "{case}: no request before the publication"
                );
            }
            if site.zero() {
                let withdraw = first(&guest, is_withdraw).expect("withdrawn at the end");
                assert!(withdraw > clock, "{case}");
            } else {
                assert!(!requests.is_empty(), "{case}: parked");
                assert!(
                    guest.probe_count.load(std::sync::atomic::Ordering::SeqCst) > 1,
                    "{case}: probed across turns, with one publication"
                );
                assert_eq!(first(&guest, is_withdraw), None, "{case}: superseded");
            }
        }
    }
}

/// IR-23(c): with an armed virtual timerfd, a timed masked wait observes its
/// deadline at the entry, after its temporary mask's publication and before
/// the expired-timerfd publication, so a concurrently running sibling's time
/// during that RPC (here each clock observation advances the harness clock
/// by 300 ms) does not move it: the park's deadline is the entry's 1 s. With
/// no timerfd the deadline is observed after the first probe and check, at
/// the first observation, as in step 3c.1d. Mutation "observed after the
/// publication": the armed rows' deadline is 1.3 s.
#[tokio::test]
async fn ir23c_with_an_armed_timerfd_the_deadline_is_the_entrys() {
    for armed in [true, false] {
        for site in [Site::InternalPselect6, Site::Ppoll, Site::Epoll] {
            let case = format!("{site:?}, armed {armed}");
            let memory = site.memory(0);
            let mut guest = turn_guest(true, Some(0));
            if armed {
                arm_virtual_timerfd(&mut guest);
            }
            let _seam = script(proc_state(0, 0, 0));
            site.run(&mut guest, &memory).await.unwrap();
            let park = guest
                .requests()
                .into_iter()
                .find(|request| {
                    request
                        .resources
                        .contains_key(&ResourceID::InternalIOPolling)
                })
                .expect("parked");
            assert_eq!(
                park.poll_deadline,
                Some(crate::types::LogicalTime::from_nanos(1_000_000_000)),
                "{case}"
            );
        }
    }
}

/// IR-23(b): an ignored signal pending at the call's entry that the guest's
/// own mask leaves unblocked was discarded by Linux when it was sent, so it
/// is removed in the call's own turn before any RPC, ahead of the
/// publication; one sent again during the publication RPC under a temporary
/// mask that blocks it is queued by Linux and kept. The armed `ppoll` with
/// SIGURG (default-ignored) pending at entry, own mask empty, temporary mask
/// SIGURG, and SIGURG committed again during the deadline observation (a
/// hand-off). Mutation "no entry removal": the one removal comes at the
/// first check, after the RPCs, and takes the instance Linux keeps.
#[tokio::test]
async fn ir23b_an_ignored_signal_the_own_mask_lets_in_is_removed_before_the_rpc() {
    let urg = bit(libc::SIGURG);
    let memory = Site::Ppoll.memory(urg);
    let mut guest = turn_guest(true, Some(0));
    arm_virtual_timerfd(&mut guest);
    guest
        .handoffs_left
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let _seam = script_by_handoff(&guest, proc_state(urg, 0, 0), proc_state(urg, 0, 0));
    let result = Site::Ppoll.run(&mut guest, &memory).await;
    assert_eq!(errno_result(&result), Ok(0));
    assert_eq!(guest.sigtimedwait_sets, vec![urg], "removed once");
    assert_eq!(
        guest.sigtimedwait_after_rpcs,
        vec![0],
        "removed before any RPC, ahead of the publication"
    );
    assert_eq!(publications(&guest), vec![(urg, 0, 0)]);
}

/// D37's held masks and R2 per queue, both orderings, a standard signal
/// (SIGURG, ignored by default) and a real-time one (SIGRTMIN+2 with
/// `SIG_IGN`), each blocked by the own mask and let in by the temporary
/// mask (empty), at the armed zero-timeout `epoll_pwait`:
///
/// - (A) an entry instance in the private queue, and an arrival in the
///   shared queue during the publication RPC (a hand-off);
/// - (B) an entry instance in the shared queue, and an arrival in the
///   private queue.
///
/// The publication carries each queue's held mask; another process's
/// `/proc` status during the RPC shows the entry instance in its own queue's
/// field and hides the arrival, which Linux discarded at send; and the end
/// (R2) removes exactly the arrival: (A) dequeues the private instance with
/// its siginfo, then the shared ones, and queues the private one again; (B)
/// dequeues the private one only. Mutations: "one held mask for both queues"
/// (the status shows the arrival), "arrivals by both queues' bits" (nothing
/// is removed), "removed from both queues" (the entry instance goes too).
#[tokio::test]
async fn held_masks_and_r2_are_per_queue_in_both_orders() {
    let rt = libc::SIGRTMIN() + 2;
    for (name, signal, ignored) in [("standard", libc::SIGURG, 0), ("real-time", rt, bit(rt))] {
        let s = bit(signal);
        for order in ["(A) private entry", "(B) shared entry"] {
            let case = format!("{name} {order}");
            let private_entry = order.starts_with("(A)");
            let memory = Site::ZeroEpoll.memory(0);
            let mut guest = turn_guest(true, Some(s));
            arm_virtual_timerfd(&mut guest);
            guest
                .handoffs_left
                .store(1, std::sync::atomic::Ordering::SeqCst);
            let entry = if private_entry {
                queues(s, 0, ignored)
            } else {
                queues(0, s, ignored)
            };
            let handoffs = guest.handoffs.clone();
            let dequeues = guest.dequeues.clone();
            let _seam = signal_state_read_seam::install(move |pid, _| {
                if pid.as_raw() != THREAD.as_raw() {
                    return None;
                }
                if handoffs.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    return Some(Ok(entry));
                }
                let taken = dequeues.load(std::sync::atomic::Ordering::SeqCst);
                Some(Ok(match (private_entry, taken) {
                    (_, 0) => queues(s, s, ignored),
                    (true, 1) => queues(0, s, ignored),
                    (true, _) => queues(0, 0, ignored),
                    (false, _) => queues(0, s, ignored),
                }))
            });
            if private_entry {
                guest.sigtimedwait_results.extend([Ok(signal as i64); 2]);
            } else {
                guest.sigtimedwait_results.push_back(Ok(signal as i64));
            }
            let result = Site::ZeroEpoll.run(&mut guest, &memory).await;
            assert_eq!(errno_result(&result), Ok(0), "{case}");
            let held = if private_entry { (s, 0) } else { (0, s) };
            assert_eq!(publications(&guest), vec![(0, held.0, held.1)], "{case}");

            // Another process's status read during the RPC.
            let status = format!(
                "Name:\tguest\nPid:\t{}\nSigPnd:\t{s:016x}\nShdPnd:\t{s:016x}\n\
                 SigBlk:\tffffffffffffffff\nSigIgn:\t{ignored:016x}\nSigCgt:\t0000000000000000\n",
                THREAD.as_raw()
            );
            let mut file = crate::procfs::ProcfsFile::from_path(std::path::Path::new(
                "/proc/thread-self/status",
            ))
            .expect("a status file");
            file.bind_thread_identity(THREAD.as_raw(), THREAD.as_raw(), 1);
            file.initialize(
                status.into_bytes(),
                crate::procfs::ProcfsSnapshotContext {
                    published_sigblk: Some(0),
                    status_hides_ignored: true,
                    status_held_ignored: held,
                    ..Default::default()
                },
            );
            let shown = String::from_utf8(file.take_at(0, usize::MAX).unwrap()).unwrap();
            let field = |label: &str| {
                shown
                    .lines()
                    .find_map(|line| line.strip_prefix(label))
                    .map(|value| u64::from_str_radix(value.trim(), 16).unwrap())
                    .unwrap()
            };
            assert_eq!(
                (field("SigPnd:"), field("ShdPnd:")),
                held,
                "{case}: each queue shows only its entry instance"
            );

            // R2 at the end.
            assert!(
                guest.sigtimedwait_sets.iter().all(|set| *set == s),
                "{case}: {:?}",
                guest.sigtimedwait_sets
            );
            if private_entry {
                assert_eq!(
                    guest.sigtimedwait_sets.len(),
                    3,
                    "{case}: private, shared, EAGAIN"
                );
                assert_eq!(
                    guest
                        .requeued
                        .iter()
                        .map(|(sig, _)| *sig)
                        .collect::<Vec<_>>(),
                    vec![signal],
                    "{case}: the private entry instance queued again"
                );
            } else {
                assert_eq!(
                    guest.sigtimedwait_sets.len(),
                    1,
                    "{case}: the private arrival"
                );
                assert!(guest.requeued.is_empty(), "{case}");
            }
            let withdraw = first(&guest, is_withdraw).expect("withdrawn");
            assert!(
                guest
                    .sigtimedwait_after_rpcs
                    .iter()
                    .all(|rpcs| *rpcs > withdraw),
                "{case}: removed after the withdrawal"
            );
        }
    }
}

/// D37's R2 and IR-7 row (A) (D37 rows 5 and 6), as the result is decided:
/// a masked wait whose first probe finds a descriptor ready keeps the
/// ignored instance pending from entry that only its temporary mask lets in
/// (SIGURG: Linux restores the own mask with no `get_signal` under the
/// temporary one), and removes the ignored one sent during the publication
/// RPC, blocked by the own mask but outside the temporary mask (SIGWINCH:
/// discarded by Linux at send, as the temporary mask was installed), before
/// the one yielding empty request that follows a first-probe result of a
/// nonzero timeout.
/// Mutations: "no R2" (nothing removed), "end removal over all of `I ∖
/// M_temp`" (SIGURG removed too), "R2 after the yield" (the removal follows
/// the request).
#[tokio::test]
async fn r2_keeps_the_entry_instance_and_removes_the_arrival_before_the_yield() {
    let urg = bit(libc::SIGURG);
    let winch = bit(libc::SIGWINCH);
    for site in [Site::Ppoll, Site::ZeroEpoll, Site::InternalPselect6] {
        let case = format!("{site:?}");
        let memory = site.memory(0);
        let mut guest = turn_guest(true, Some(urg | winch));
        arm_virtual_timerfd(&mut guest);
        // The entry reads come before any probe; SIGWINCH arrives during the
        // publication RPC, so every read after the probe sees it.
        let probes = guest.probe_count.clone();
        let _seam = signal_state_read_seam::install(move |pid, _| {
            if pid.as_raw() != THREAD.as_raw() {
                return None;
            }
            let probed = probes.load(std::sync::atomic::Ordering::SeqCst) != 0;
            Some(Ok(if probed {
                proc_state(urg | winch, 0, 0)
            } else {
                proc_state(urg, 0, 0)
            }))
        });
        guest.probe_results.push_back(Ok(1));
        let result = site.run(&mut guest, &memory).await;
        assert_eq!(errno_result(&result), Ok(1), "{case}");
        assert_eq!(
            guest.sigtimedwait_sets,
            vec![winch],
            "{case}: the arrival only"
        );
        if !site.zero() {
            assert_eq!(
                guest.sigtimedwait_after_requests,
                vec![0],
                "{case}: before the yield"
            );
            assert_eq!(guest.requests().len(), 1, "{case}: one yield");
        }
    }
}

/// `HERMIT_IGNORED_REALTIME_INSTANCES`, decided from the committed-send
/// window record, never from bits: an ignored real-time signal (SIGRTMIN+2,
/// `SIG_IGN`) outside the temporary mask, pending in the private queue from
/// the call's entry, and sent to the same queue during the publication (the
/// withdrawal's window says so; the queue's bit shows nothing new): the run
/// is refused by name and nothing is removed. With an empty window the
/// entry instance is kept, with no refusal. A send in the window to the
/// other queue is an arrival there, removed from that queue alone. A
/// standard signal never refuses (Linux queues one instance per queue).
/// Mutation "detect by bit difference": the first row never refuses.
#[tokio::test]
async fn ignored_realtime_instances_are_refused_from_the_committed_record() {
    let rt = libc::SIGRTMIN() + 2;
    let r = bit(rt);
    let urg = bit(libc::SIGURG);
    for (name, signal, ignored, window, end_shared, refused) in [
        ("rt sent to the same queue", rt, r, (r, 0), 0, true),
        ("rt, nothing sent", rt, r, (0, 0), 0, false),
        ("rt sent to the other queue", rt, r, (0, r), r, false),
        (
            "standard sent to the same queue",
            libc::SIGURG,
            0,
            (urg, 0),
            0,
            false,
        ),
    ] {
        let s = bit(signal);
        let memory = Site::ZeroEpoll.memory(0);
        let mut guest = turn_guest(true, Some(s));
        arm_virtual_timerfd(&mut guest);
        guest.send_window = CommittedSendWindow {
            sent_private: window.0,
            sent_shared: window.1,
        };
        let shared = end_shared & s;
        // The timerfd publication's clock observation is a hand-off, during
        // which the window's send lands.
        guest
            .handoffs_left
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let handoffs = guest.handoffs.clone();
        let dequeues = guest.dequeues.clone();
        let _seam = signal_state_read_seam::install(move |pid, _| {
            if pid.as_raw() != THREAD.as_raw() {
                return None;
            }
            if handoffs.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return Some(Ok(queues(s, 0, ignored)));
            }
            let taken = dequeues.load(std::sync::atomic::Ordering::SeqCst);
            Some(Ok(match taken {
                // Before the private instance is dequeued.
                0 => queues(s, shared, ignored),
                1 => queues(0, shared, ignored),
                _ => queues(0, 0, ignored),
            }))
        });
        guest.sigtimedwait_results.extend([Ok(signal as i64); 2]);
        let result = Site::ZeroEpoll.run(&mut guest, &memory).await;
        let refusal = guest.rpcs().into_iter().any(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_IGNORED_REALTIME_INSTANCES"))
        });
        assert_eq!(refusal, refused, "{name}");
        if refused {
            assert_eq!(errno_result(&result), Err(Errno::ENOSYS), "{name}");
            assert!(
                guest.sigtimedwait_sets.is_empty(),
                "{name}: nothing removed"
            );
        } else if end_shared != 0 {
            assert_eq!(errno_result(&result), Ok(0), "{name}");
            assert_eq!(
                guest.sigtimedwait_sets.len(),
                3,
                "{name}: the shared arrival only"
            );
            assert_eq!(guest.requeued.len(), 1, "{name}: the entry instance kept");
        } else {
            assert_eq!(errno_result(&result), Ok(0), "{name}");
            assert!(guest.sigtimedwait_sets.is_empty(), "{name}: kept");
        }
    }
}

/// IR-18's reader: a `/proc` status read under the gate of a thread whose
/// mask the scheduler holds as unknown files the run-wide stopping refusal
/// HERMIT_GUEST_MASK_UNKNOWN and returns Hermit's own failure, never the
/// kernel's `SigBlk` (the in-guest runtime's mask once it has one), and no
/// errno to the reader; a known mask is reported with the held masks, and
/// nothing published keeps the kernel's. Mutation "`SigBlk` on unknown": the
/// unknown row reports nothing published.
#[tokio::test]
async fn ir18_a_status_read_of_an_unknown_mask_is_a_named_refusal() {
    let own = bit(libc::SIGUSR2);
    for mask in [
        Some(PublishedGuestMask::Unknown),
        Some(PublishedGuestMask::Known(own)),
        None,
    ] {
        let case = format!("{mask:?}");
        let mut guest = turn_guest(true, Some(own));
        guest.status_mask = StatusGuestMask {
            mask,
            held_private: bit(libc::SIGURG),
            held_shared: 0,
        };
        let facts = crate::syscalls::status_published_sigblk(&mut guest, Some(PEER.as_raw())).await;
        let refused = guest.rpcs().into_iter().any(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_GUEST_MASK_UNKNOWN"))
        });
        match mask {
            Some(PublishedGuestMask::Unknown) => {
                assert!(refused, "{case}");
                assert!(matches!(facts, Err(Error::Tool(_))), "{case}: {facts:?}");
            }
            Some(PublishedGuestMask::Known(known)) => {
                assert!(!refused, "{case}");
                let facts = facts.unwrap();
                assert_eq!(facts.sigblk, Some(known), "{case}");
                assert_eq!(facts.held_private, bit(libc::SIGURG), "{case}");
            }
            None => {
                assert!(!refused, "{case}");
                assert_eq!(facts.unwrap().sigblk, None, "{case}");
            }
        }
    }
}
