/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual scheduler fence glue with explicitly scripted native receipts.
//! These controls do not claim that a fake receipt proves ptrace completion.

use reverie::reserved_input::InputGrantKind;
use reverie::reserved_input::ReservedInputControl;

use super::Scheduler;
use crate::config::Config;
use crate::types::DetTid;

#[test]
fn reserved_input_fence_keeps_late_group_members_until_all_exact_fences_complete() {
    let control = ReservedInputControl::new_backend().unwrap();
    let parent = control.backend_register_root(10, 10).unwrap();
    let first = control
        .backend_register_child(parent.task, 11, 10, Some(true), Some(true))
        .unwrap();
    let cohort = control.freeze_group(parent.task, false).unwrap();
    let late = control
        .backend_register_child(parent.task, 12, 10, Some(true), Some(true))
        .unwrap();
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_control = Some(control.clone());
    scheduler
        .reserved_input_tasks
        .insert(DetTid::from_raw(11), first.task);
    scheduler
        .reserved_input_tasks
        .insert(DetTid::from_raw(12), late.task);
    scheduler.freeze_reserved_input_thread(DetTid::from_raw(11));
    scheduler.freeze_reserved_input_thread(DetTid::from_raw(12));
    control.backend_terminal(first.task).unwrap();
    scheduler.settle_reserved_input_fence(&[DetTid::from_raw(11)]);
    assert_eq!(control.cohort_members(&cohort).unwrap().len(), 2);
    assert_eq!(
        control
            .grant(late.task, InputGrantKind::Ordinary)
            .unwrap_err()
            .reason(),
        "input-task-frozen"
    );
    control.backend_terminal(late.task).unwrap();
    scheduler.settle_reserved_input_fence(&[DetTid::from_raw(12)]);
    assert_eq!(
        control.cohort_members(&cohort).unwrap_err().reason(),
        "input-freeze-missing"
    );
    assert!(control.grant(parent.task, InputGrantKind::Ordinary).is_ok());
}

#[test]
fn reserved_input_fence_uses_the_retained_old_generation_not_a_reused_number() {
    let control = ReservedInputControl::new_backend().unwrap();
    let old = control.backend_register_root(11, 11).unwrap();
    let mut scheduler = Scheduler::new(&Config::default());
    let tid = DetTid::from_raw(11);
    scheduler.reserved_input_control = Some(control.clone());
    scheduler.reserved_input_tasks.insert(tid, old.task);
    scheduler.freeze_reserved_input_thread(tid);
    control.backend_terminal(old.task).unwrap();
    let replacement = control.backend_register_root(11, 11).unwrap();
    assert_ne!(old.task, replacement.task);
    scheduler.reserved_input_tasks.insert(tid, replacement.task);
    scheduler.settle_reserved_input_fence(&[tid]);
    assert_eq!(control.native_terminal(replacement.task), Some(false));
    assert!(
        control
            .grant(replacement.task, InputGrantKind::Ordinary)
            .is_ok()
    );
    assert!(
        !scheduler
            .reserved_input_settled_tasks
            .contains(&replacement.task)
    );
}

#[test]
fn reserved_input_fence_never_silently_omits_a_missing_native_binding() {
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_control = Some(ReservedInputControl::new_backend().unwrap());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler.settle_reserved_input_fence(&[DetTid::from_raw(44)]);
    }));
    assert!(result.is_err());
}

#[test]
fn reserved_input_fence_cannot_use_another_generations_terminal_receipt() {
    let control = ReservedInputControl::new_backend().unwrap();
    let old = control.backend_register_root(11, 11).unwrap();
    control.backend_terminal(old.task).unwrap();
    let replacement = control.backend_register_root(11, 11).unwrap();
    assert_ne!(old.task, replacement.task);
    let mut scheduler = Scheduler::new(&Config::default());
    let tid = DetTid::from_raw(11);
    scheduler.reserved_input_control = Some(control.clone());
    scheduler.reserved_input_tasks.insert(tid, replacement.task);
    scheduler.freeze_reserved_input_thread(tid);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler.settle_reserved_input_fence(&[tid]);
    }));
    assert!(result.is_err());
    assert!(
        !scheduler
            .reserved_input_settled_tasks
            .contains(&replacement.task)
    );
    assert_eq!(
        control
            .grant(replacement.task, InputGrantKind::Ordinary)
            .unwrap_err()
            .reason(),
        "input-task-frozen"
    );
}

#[test]
fn reserved_input_nonleader_exec_settles_old_images_before_binding_the_survivor() {
    let control = ReservedInputControl::new_backend().unwrap();
    let leader_native = control.backend_register_root(123, 123).unwrap();
    let caller_native = control
        .backend_register_child(leader_native.task, 124, 123, Some(true), Some(true))
        .unwrap();
    let leader = DetTid::from_raw(17);
    let caller = DetTid::from_raw(18);
    let mm = crate::types::MmId::initial(leader);
    let mut scheduler = Scheduler::new(&Config::default());
    scheduler.reserved_input_control = Some(control.clone());
    for (tid, native) in [(leader, leader_native), (caller, caller_native)] {
        control
            .backend_bind_tool_identity(native.task, tid.as_raw(), leader.as_raw())
            .unwrap();
        scheduler.reserved_input_tool_tasks.insert(tid, native.task);
        scheduler.record_thread_mm(tid, mm);
        scheduler.register_thread_for_test(tid);
        scheduler.runqueue_push_back(tid);
    }
    scheduler.thread_tree.add_child(leader, leader, true);
    scheduler.thread_tree.add_child(leader, caller, false);
    // Explicitly scripted authenticated native Exec. The actual kernel seam
    // is a separate live/backend control, not proved by this state setup.
    let replacement = control
        .backend_exec_replacement(leader_native.task, caller_native.task)
        .unwrap();
    control
        .backend_bind_tool_identity(replacement.task, leader.as_raw(), leader.as_raw())
        .unwrap();
    scheduler
        .reserved_input_tool_tasks
        .insert(leader, replacement.task);
    assert_eq!(scheduler.reserved_input_tasks[&leader], leader_native.task);
    let retired = scheduler.reconnect_transferred_exec(super::ExecReconnect {
        caller,
        new_leader: leader,
        detpid: leader,
        pre_exec_mm: mm,
        post_exec_mm: mm.for_exec(leader),
        child_tid_addr: 0,
        reconnect_priority: Some(super::DEFAULT_PRIORITY),
    });
    assert_eq!(retired, vec![leader, caller]);
    assert_eq!(scheduler.reserved_input_tasks[&leader], replacement.task);
    assert!(
        scheduler
            .reserved_input_settled_tasks
            .contains(&leader_native.task)
    );
    assert!(
        scheduler
            .reserved_input_settled_tasks
            .contains(&caller_native.task)
    );
    assert!(
        !scheduler
            .reserved_input_settled_tasks
            .contains(&replacement.task)
    );
    assert_eq!(control.native_terminal(replacement.task), Some(false));
    assert!(
        control
            .grant(replacement.task, InputGrantKind::Ordinary)
            .is_ok()
    );
    assert_eq!(
        scheduler.turn, 0,
        "the fence/rebinding must not create a turn"
    );
}
