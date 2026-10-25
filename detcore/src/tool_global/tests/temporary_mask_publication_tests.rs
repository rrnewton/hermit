/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1e A, through the coordinator's real RPC handlers
//! (`GlobalState::receive_rpc`, `GlobalState::recv_create_child_thread`):
//! fork-child seeding, the committed-send window record at a guest send's
//! commit point (`NotifySignalPending`), the temporary-mask publication and
//! its withdrawal (`PublishTemporaryMask`), and the `/proc` status query's
//! appended answer under the in-guest delivery gate. Each test names the
//! mutation it catches.

use reverie::Tid;
use reverie::syscalls::CloneFlags;

use super::*;
use crate::scheduler::guest_mask_publication::CommittedSendWindow;
use crate::scheduler::guest_mask_publication::PublishedGuestMask;
use crate::scheduler::guest_mask_publication::StatusGuestMask;
use crate::scheduler::guest_mask_publication::TemporaryMaskControl;
use crate::tool_global::ChildRegistration;
use crate::tool_global::SignalScope;

const SENDER: DetTid = DetTid::from_raw(61);
const TARGET: DetTid = DetTid::from_raw(62);

fn bit(signal: i32) -> u64 {
    crate::scheduler::kernel_signal_bit(signal)
}

/// An in-guest configuration with the delivery gate `gate`.
fn config(gate: bool) -> Config {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    config
}

/// A coordinator with `SENDER` and `TARGET` registered, each its own process.
fn coordinator(config: &Config) -> GlobalState {
    let state = GlobalState::initialize(config, false);
    for thread in [SENDER, TARGET] {
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(thread, thread, true);
        install_test_registration(&state, thread, Ivar::new());
    }
    state
}

/// `request` sent by `from`, answered by the real handler.
async fn rpc(
    state: &GlobalState,
    config: &Config,
    from: DetTid,
    request: GlobalRequest,
) -> GlobalResponse {
    state
        .receive_rpc(
            Tid::from_raw(from.as_raw()),
            (
                DetTime::new(config),
                MmId::initial(DetPid::from_raw(from.as_raw())),
                request,
            ),
        )
        .await
        .1
}

/// The committed-send window record at a guest send's commit point: the
/// `NotifySignalPending` handler, which the sender files only after its
/// injected send succeeded, records a thread-directed send in the target's
/// private queue and a process-directed one in its shared queue, while the
/// target holds a temporary-mask publication; the target's withdrawal
/// answers them. A send before the publication is not recorded. With the
/// gate off the publication is not taken and nothing is recorded. Mutation
/// "the guest send not recorded at its commit point": the window is empty.
#[tokio::test]
async fn a_guest_send_committed_during_a_publication_joins_the_targets_window() {
    let rt = libc::SIGRTMIN() + 2;
    for gate in [true, false] {
        let config = config(gate);
        let state = coordinator(&config);
        let notify = |signal, scope| {
            GlobalRequest::NotifySignalPending(
                TARGET,
                SigWrapper(signal),
                Some(DetPid::from_raw(TARGET.as_raw())),
                scope,
            )
        };
        // Before the publication: not recorded.
        assert_eq!(
            rpc(
                &state,
                &config,
                SENDER,
                notify(libc::SIGHUP, SignalScope::Thread)
            )
            .await,
            GlobalResponse::NotifySignalPending(())
        );
        let publish = TemporaryMaskControl::Publish {
            thread: TARGET,
            mask: 0,
            held_private: 0,
            held_shared: 0,
        };
        assert_eq!(
            rpc(
                &state,
                &config,
                TARGET,
                GlobalRequest::PublishTemporaryMask(publish)
            )
            .await,
            GlobalResponse::PublishTemporaryMask(None)
        );
        rpc(&state, &config, SENDER, notify(rt, SignalScope::Thread)).await;
        rpc(
            &state,
            &config,
            SENDER,
            notify(libc::SIGUSR1, SignalScope::Process),
        )
        .await;
        let withdrawn = rpc(
            &state,
            &config,
            TARGET,
            GlobalRequest::PublishTemporaryMask(TemporaryMaskControl::Withdraw {
                thread: TARGET,
                keep_published: false,
            }),
        )
        .await;
        let want = gate.then_some(CommittedSendWindow {
            sent_private: bit(rt),
            sent_shared: bit(libc::SIGUSR1),
        });
        assert_eq!(
            withdrawn,
            GlobalResponse::PublishTemporaryMask(want),
            "gate {gate}"
        );
    }
}

/// The `/proc` status query (`PublishedGuestSigmask`) under the gate is
/// answered with the appended `GuestSigmaskForStatus`: the tri-state and the
/// held masks of a publication in force; with the gate off, with the old
/// answer, byte for byte as before. Mutation "the old answer under the
/// gate": the held masks never reach the status reader.
#[tokio::test]
async fn the_status_query_answers_the_tri_state_and_held_masks_under_the_gate_only() {
    let temporary = bit(libc::SIGTERM);
    for gate in [true, false] {
        let config = config(gate);
        let state = coordinator(&config);
        let publish = TemporaryMaskControl::Publish {
            thread: TARGET,
            mask: temporary,
            held_private: bit(libc::SIGURG),
            held_shared: bit(libc::SIGWINCH),
        };
        rpc(
            &state,
            &config,
            TARGET,
            GlobalRequest::PublishTemporaryMask(publish),
        )
        .await;
        let answer = rpc(
            &state,
            &config,
            SENDER,
            GlobalRequest::PublishedGuestSigmask(TARGET),
        )
        .await;
        if gate {
            assert_eq!(
                answer,
                GlobalResponse::GuestSigmaskForStatus(StatusGuestMask {
                    mask: Some(PublishedGuestMask::Known(temporary)),
                    held_private: bit(libc::SIGURG),
                    held_shared: bit(libc::SIGWINCH),
                })
            );
        } else {
            assert_eq!(answer, GlobalResponse::PublishedGuestSigmask(None));
        }
    }
}

/// Fork-child seeding through `recv_create_child_thread`: a child registered
/// in its creator's turn starts with the mask its creator published (here
/// `Known`, and `Unknown`), as Linux's child starts with its creator's
/// `blocked` (kernel/fork.c:1086-1091 at v6.12). With the gate off, nothing.
/// Mutation "no seed at registration": the child holds nothing.
#[tokio::test]
async fn the_coordinator_seeds_a_new_child_with_its_creators_published_mask() {
    let parent = DetTid::from_raw(3);
    let child = DetTid::from_raw(4);
    let own = bit(libc::SIGUSR2);
    for gate in [true, false] {
        for published in [
            Some(PublishedGuestMask::Known(own)),
            Some(PublishedGuestMask::Unknown),
        ] {
            let case = format!("{published:?}, gate {gate}");
            // Registration as the coordinator's unit tests drive it, with no
            // serial grant to wait for.
            let config = Config {
                sequentialize_threads: false,
                ..config(gate)
            };
            let state = GlobalState::initialize(&config, false);
            let registration = |child_dettid, flags| ChildRegistration {
                parent_dettid: parent,
                parent_detpid: parent,
                child_dettid,
                child_tid_addr: 0,
                flags,
                exit_signal: libc::SIGCHLD,
                physical_ids: None,
                maybe_priority: Some(DEFAULT_PRIORITY),
                parent_is_kernel_blocked: false,
            };
            let parent_mm = MmId::initial(parent);
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        parent_mm,
                        registration(parent, None),
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
            state
                .sched
                .lock()
                .unwrap()
                .put_guest_mask_state_for_test(parent, published);
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        parent_mm,
                        registration(child, Some(CloneFlags::empty())),
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
            let held = state
                .sched
                .lock()
                .unwrap()
                .published_guest_mask_state(child);
            assert_eq!(held, gate.then_some(published).flatten(), "{case}");
        }
    }
}
