/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A deregistration that reports a death by a signal that ends the whole
//! thread group, SIGKILL here, makes an empty run queue wait for the rest of
//! the group's deaths (`Scheduler::note_signal_killed_group`).

use super::*;

const PARENT: DetTid = DetTid::from_raw(1);
const LEADER: DetTid = DetTid::from_raw(3);
const WORKER: DetTid = DetTid::from_raw(5);

fn state() -> (GlobalState, DetTime) {
    let config = Config {
        sequentialize_threads: true,
        ..Config::default()
    }
    .with_backend(|backend| {
        backend.needs_killed_thread_rpc_cancellation = false;
    });
    let state = GlobalState::initialize(&config, false);
    {
        let mut sched = state.sched.lock().unwrap();
        sched.thread_tree.add_child(PARENT, PARENT, true);
        sched.thread_tree.add_child(PARENT, LEADER, true);
        sched.thread_tree.add_child(LEADER, WORKER, false);
    }
    let time = DetTime::new(&config);
    for tid in [LEADER, WORKER] {
        install_test_registration(&state, tid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(tid, MmId::initial(LEADER));
        state.global_time.lock().unwrap().update_global_time(
            tid,
            time.as_nanos(),
            time.inherited_nanos(),
        );
    }
    (state, time)
}

fn deregistration(tid: DetTid, sigkilled: bool) -> ThreadDeregistration {
    ThreadDeregistration {
        dettid: tid,
        detpid: LEADER,
        mm: MmId::initial(LEADER),
        thread_start_entered: true,
        timeslice_stats: TimesliceStats::default(),
        syscall_count: 1,
        chaos_epochs: Vec::new(),
        killed_by_group_signal: sigkilled,
    }
}

/// A worker's death by SIGKILL makes the scheduler wait for the rest of its
/// group to die; any other death does not. Either way the leader is retired
/// only at its own deregistration.
#[tokio::test]
async fn a_sigkill_deregistration_waits_for_the_rest_of_its_group() {
    for sigkilled in [false, true] {
        let (state, time) = state();
        let response = state
            .receive_rpc(
                Tid::from_raw(WORKER.as_raw()),
                (
                    time,
                    MmId::initial(LEADER),
                    GlobalRequest::DeregisterThread(deregistration(WORKER, sigkilled)),
                ),
            )
            .await;
        assert_eq!(response, (None, GlobalResponse::DeregisterThread(())));
        let sched = state.sched.lock().unwrap();
        assert!(!sched.next_turns.contains_key(&WORKER));
        assert!(sched.next_turns.contains_key(&LEADER));
        assert_eq!(
            sched.signal_death_victim(DetPid::from_raw(LEADER.as_raw())),
            sigkilled,
            "sigkilled: {sigkilled}"
        );
    }
}
