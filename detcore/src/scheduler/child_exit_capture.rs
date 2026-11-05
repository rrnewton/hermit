/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Selected Exit grants with independently serviced native capture.
//! Scheduler locks protect reservations, never native control awaits.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use reverie::BackendChildExitCapture;
use reverie::ChildExitCaptureArm;
use reverie::ChildExitCaptureRequest;
use reverie::ReservedChildExitCapture;

use super::Scheduler;
use super::parked::NextTurnOwner;
use super::parked::RpcOrigin;
use crate::resources::ResourceID;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::MmId;

#[derive(Debug, Default)]
pub(super) struct CaptureState {
    control: Option<Arc<dyn BackendChildExitCapture>>,
    sequence: u64,
    pub(super) holds: BTreeMap<DetTid, Hold>,
    aborted: Vec<Box<dyn ReservedChildExitCapture>>,
}

#[derive(Debug)]
pub(super) struct Hold {
    request: ChildExitCaptureRequest,
    parent_mm: MmId,
    child_mm: MmId,
    transaction: Option<Box<dyn ReservedChildExitCapture>>,
}

/// A committed reservation survives clear_nextturn and the child's consuming
/// exit cleanup. Those are expected transitions, not stale scheduler epochs.
pub(crate) struct SelectedCapture {
    request: ChildExitCaptureRequest,
    transaction: Box<dyn ReservedChildExitCapture>,
}

impl Scheduler {
    pub(super) fn native_child_capture_installed(&self) -> bool {
        self.native_child_capture.control.is_some()
    }

    pub(crate) fn has_native_child_capture(&self, child: DetTid) -> bool {
        self.native_child_capture.holds.contains_key(&child)
    }

    pub(super) fn abort_native_capture_state(&mut self) {
        for (_, mut hold) in std::mem::take(&mut self.native_child_capture.holds) {
            if let Some(transaction) = hold.transaction.take() {
                self.native_child_capture.aborted.push(transaction);
            }
        }
    }

    pub(crate) fn install_child_exit_capture(
        &mut self,
        control: Arc<dyn BackendChildExitCapture>,
        sequential: bool,
    ) -> Result<bool, reverie::Error> {
        if !sequential {
            return Ok(false);
        }
        if self.native_child_capture.control.is_some()
            || !self.next_turns.is_empty()
            || self.turn != 0
            || self.started_up.try_read().is_some()
            || self.signal_control_installed()
        {
            return Err(reverie::Errno::EINVAL.into());
        }
        self.native_child_capture.control = Some(control);
        Ok(true)
    }

    pub(super) fn reserve_native_child_capture(
        &mut self,
        tid: DetTid,
        child: DetPid,
        child_mm: MmId,
    ) -> Result<(), String> {
        let Some(control) = self.native_child_capture.control.clone() else {
            return Ok(());
        };
        // Only a normal-fork, single-task process with its original live wait
        // parent is in this contract. Existing routing owns other topologies.
        if tid != child
            || self.registered_process(tid) != Some(child)
            || self.thread_tree.my_thread_group(&child).len() != 1
        {
            return Ok(());
        }
        let Some(parent) = self.thread_tree.parent_process(&child) else {
            return Ok(());
        };
        let Some(wait) = self.thread_tree.process_wait.get(&child) else {
            return Ok(());
        };
        if parent == child
            || wait.wait_parent != Some(parent)
            || wait.wait_owner != parent
            || wait.exit_signal != libc::SIGCHLD
            || wait.parent_exec_generation != self.thread_tree.exec_generation(parent)
            || self.thread_tree.exec_generation(child) != 0
            || self.thread_tree.reaps_children_automatically(parent)
            || self.thread_tree.init_orphans.contains(&child)
            || !self
                .thread_tree
                .tree
                .get(&parent)
                .is_some_and(|children| children.contains(&child))
            || !self.thread_tree.contains(&parent)
            || self.thread_tree.my_thread_group(&parent).len() != 1
            || self.thread_tree.pid_ns_depth(parent) != 0
            || self.thread_tree.pid_ns_depth(parent) != self.thread_tree.pid_ns_depth(child)
            || self.thread_tree.granted_exits.contains(&parent)
            || self.blocked.external_io_blockers.contains_key(&parent)
            || self.blocked.rt_sigsuspend_blockers.contains_key(&parent)
            || self.blocked.signaled_background.contains(&parent)
            || self.exec_teardowns.contains_key(&parent)
        {
            return Ok(());
        }
        let Some(parent_mm) = self.thread_mm(parent) else {
            return Err("capture parent has no registered address-space identity".into());
        };
        if parent_mm == child_mm {
            return Ok(());
        }
        if self.thread_mm(child) != Some(child_mm) {
            return Err("capture child incarnation changed at selected Exit".into());
        }
        let turn = self
            .next_turns
            .get(&parent)
            .ok_or("capture parent has no pending scheduler turn")?;
        let origin = turn
            .protocol
            .origin
            .ok_or("capture parent has no authenticated RPC origin")?;
        if origin.mm != parent_mm || turn.protocol.owner != NextTurnOwner::Ordinary {
            return Err("capture parent RPC incarnation/owner changed".into());
        }
        // These explicit origins enter through the actual resource RPC. A
        // daemon-generated request without that ownership cannot stand in for
        // a native parent endpoint, even if its IVar happens to be full.
        if !matches!(
            origin.rpc,
            RpcOrigin::DirectRequestResources | RpcOrigin::TraceSchedEvent
        ) {
            return Ok(());
        }
        let resources = turn
            .req
            .try_read()
            .and_then(Result::ok)
            .ok_or("selected Exit overtook parent's ordinary RPC quiescence")?;
        let allow_syscall_callback = resources.resources.keys().any(|resource| {
            matches!(resource, ResourceID::WaitChild { parent: owner, .. } if *owner == parent)
        });
        let (parent_identity_mm, parent_pid, parent_tid, _) = self
            .physical_thread_pidfds
            .get(&parent)
            .ok_or("capture parent physical identity is absent")?;
        let (child_identity_mm, child_pid, child_tid, _) = self
            .physical_thread_pidfds
            .get(&child)
            .ok_or("capture child physical identity is absent")?;
        if parent_pid != parent_tid || child_pid != child_tid {
            return Ok(());
        }
        if *parent_identity_mm != parent_mm || *child_identity_mm != child_mm {
            return Err("capture process physical identity disagrees with its incarnation".into());
        }
        let sequence = self
            .native_child_capture
            .sequence
            .checked_add(1)
            .ok_or("child capture reservation identity exhausted")?;
        let request = ChildExitCaptureRequest {
            parent: reverie::Pid::from_raw(parent.as_raw()),
            child: reverie::Pid::from_raw(child.as_raw()),
            reservation: sequence,
            parent_physical_pid: *parent_pid,
            child_physical_pid: *child_pid,
            allow_syscall_callback,
        };
        if self.native_child_capture.holds.contains_key(&child) {
            return Err("selected child Exit already owns a capture reservation".into());
        }
        // Atomic against the transport's ordinary-reply decision. Missing or
        // closed is an error, never host-timed selection of the old route.
        let transaction = control
            .reserve(request)
            .map_err(|error| error.to_string())?;
        self.native_child_capture.sequence = sequence;
        self.native_child_capture.holds.insert(
            child,
            Hold {
                request,
                parent_mm,
                child_mm,
                transaction: Some(transaction),
            },
        );
        tracing::debug!(
            "[child-exit-capture] reserved reservation={} parent={} child={}",
            sequence,
            parent,
            child
        );
        Ok(())
    }

    pub(crate) fn take_native_child_capture(&mut self, child: DetTid) -> Option<SelectedCapture> {
        let hold = self.native_child_capture.holds.get_mut(&child)?;
        hold.transaction.take().map(|transaction| SelectedCapture {
            request: hold.request,
            transaction,
        })
    }

    pub(super) fn fail_native_child_capture(&mut self, child: DetTid, error: &str) {
        tracing::error!("authentic child-exit capture failed for child {child}: {error}");
        if let Some(wake) = self.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(child.as_raw()),
            tid: reverie::Tid::from_raw(child.as_raw()),
            phase: "authentic child-exit capture reservation",
        }) {
            self.parked.failure_wakes.push(wake);
            self.wake_control_waiter();
        }
    }
}

/// Move abandoned reply reservations out of scheduler ownership before their
/// Drop wakes the transport actor. No native I/O or endpoint mutex is entered
/// while the scheduler lock is held.
pub(crate) fn drain_failure(scheduler: &Arc<Mutex<Scheduler>>) {
    let (aborted, wakes) = {
        let mut sched = scheduler.lock().unwrap();
        let aborted = std::mem::take(&mut sched.native_child_capture.aborted);
        let wakes = if sched.backend_failed() {
            sched.take_signal_failure_wakes()
        } else {
            Vec::new()
        };
        (aborted, wakes)
    };
    drop(aborted);
    for wake in wakes {
        let _ = wake.send(());
    }
}

/// Every scheduler-loop exit, including startup failure and cancellation,
/// retires still-owned publication holds and drains detached reservations. This
/// outer guard runs after all inner scheduler lock guards have been dropped.
pub(super) struct FailureDrain(pub(super) Arc<Mutex<Scheduler>>);

impl Drop for FailureDrain {
    fn drop(&mut self) {
        let wake = {
            let mut sched = self.0.lock().unwrap();
            let request = sched
                .native_child_capture
                .holds
                .values()
                .next()
                .map(|hold| hold.request);
            request.and_then(|request| {
                sched.report_backend_failure(reverie::BackendFailure {
                    pid: request.child,
                    tid: request.child,
                    phase: "scheduler retired with an unfinished child publication",
                })
            })
        };
        if let Some(wake) = wake {
            let _ = wake.send(());
        }
        drain_failure(&self.0);
    }
}

struct GrantGuard {
    scheduler: Arc<Mutex<Scheduler>>,
    request: ChildExitCaptureRequest,
    finished: bool,
}

impl GrantGuard {
    fn complete(&mut self) -> Result<(), String> {
        let mut sched = self.scheduler.lock().unwrap();
        let child = DetTid::from_raw(self.request.child.as_raw());
        let parent = DetTid::from_raw(self.request.parent.as_raw());
        let hold = sched
            .native_child_capture
            .holds
            .get(&child)
            .ok_or("capture hold disappeared before completion")?;
        if hold.request != self.request
            || sched.thread_mm(parent) != Some(hold.parent_mm)
            || sched
                .thread_mm(child)
                .is_some_and(|current| current != hold.child_mm)
            || sched.backend_failed()
        {
            return Err("capture completion no longer owns its grant/incarnation".into());
        }
        sched.native_child_capture.holds.remove(&child);
        self.finished = true;
        Ok(())
    }
}

impl Drop for GrantGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let wake = {
            let mut sched = self.scheduler.lock().unwrap();
            let child = DetTid::from_raw(self.request.child.as_raw());
            if sched
                .native_child_capture
                .holds
                .get(&child)
                .is_some_and(|hold| hold.request == self.request)
            {
                sched.native_child_capture.holds.remove(&child);
            }
            sched.report_backend_failure(reverie::BackendFailure {
                pid: self.request.child,
                tid: self.request.child,
                phase: "authentic child-exit capture aborted",
            })
        };
        if let Some(wake) = wake {
            let _ = wake.send(());
        }
        drain_failure(&self.scheduler);
    }
}

/// Arm before returning the actual child RPC response. The returned future is
/// owned by GlobalState's JoinSet and may finish while both Tool futures remain
/// borrowed. Consuming RobustListWakes/DeregisterThread are never held here.
pub(crate) async fn arm_selected(
    scheduler: Arc<Mutex<Scheduler>>,
    mut selected: SelectedCapture,
) -> Result<Option<impl std::future::Future<Output = ()> + Send + 'static>, ()> {
    let request = selected.request;
    let mut guard = GrantGuard {
        scheduler: scheduler.clone(),
        request,
        finished: false,
    };
    match super::until_backend_failure(&scheduler, selected.transaction.arm()).await {
        Ok(Ok(ChildExitCaptureArm::Unsupported)) => {
            guard.complete();
            return Ok(None);
        }
        Ok(Ok(ChildExitCaptureArm::Armed)) => {}
        Ok(Err(error)) => {
            tracing::error!("child-exit capture Arm failed: {error}");
            return Err(());
        }
        Err(_) => return Err(()),
    }
    tracing::debug!(
        "[child-exit-capture] armed reservation={} parent={} child={}",
        request.reservation,
        request.parent,
        request.child
    );
    Ok(Some(async move {
        match super::until_backend_failure(&scheduler, selected.transaction.finish()).await {
            Ok(Ok(outcome)) if outcome.request == request => {
                tracing::debug!(
                    "[child-exit-capture] captured reservation={} parent={} child={} domain={} interval={} allocation={} effect={:?}",
                    request.reservation,
                    request.parent,
                    request.child,
                    outcome.domain,
                    outcome.interval,
                    outcome.allocation,
                    outcome.effect
                );
                if let Err(error) = guard.complete() {
                    tracing::error!("child-exit capture completion failed: {error}");
                    return;
                }
                tracing::debug!(
                    "[child-exit-capture] released reservation={} parent={} child={}",
                    request.reservation,
                    request.parent,
                    request.child
                );
            }
            Ok(Ok(_)) => tracing::error!("child-exit capture outcome changed reservation identity"),
            Ok(Err(error)) => tracing::error!("child-exit capture completion failed: {error}"),
            Err(_) => {}
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    #[derive(Debug)]
    struct ReservedProbe {
        scheduler: Weak<Mutex<Scheduler>>,
        dropped: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ReservedChildExitCapture for ReservedProbe {
        async fn arm(&mut self) -> Result<ChildExitCaptureArm, reverie::Error> {
            unreachable!("failure test must not send a native Arm")
        }

        async fn finish(
            self: Box<Self>,
        ) -> Result<reverie::ChildExitCaptureOutcome, reverie::Error> {
            unreachable!("failure test must not fabricate capture")
        }
    }

    impl Drop for ReservedProbe {
        fn drop(&mut self) {
            let scheduler = self.scheduler.upgrade().unwrap();
            assert!(
                scheduler.try_lock().is_ok(),
                "reserved endpoint dropped under scheduler mutex"
            );
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn setup() -> (Arc<Mutex<Scheduler>>, ChildExitCaptureRequest) {
        let scheduler = Arc::new(Mutex::new(Scheduler::new(&crate::Config::default())));
        let parent = DetTid::from_raw(1);
        let child = DetTid::from_raw(2);
        let request = ChildExitCaptureRequest {
            parent: reverie::Pid::from_raw(1),
            child: reverie::Pid::from_raw(2),
            reservation: 7,
            parent_physical_pid: 101,
            child_physical_pid: 102,
            allow_syscall_callback: true,
        };
        let mut sched = scheduler.lock().unwrap();
        sched.record_thread_mm(parent, MmId::initial(parent));
        sched.record_thread_mm(child, MmId::initial(child));
        sched.native_child_capture.holds.insert(
            child,
            Hold {
                request,
                parent_mm: MmId::initial(parent),
                child_mm: MmId::initial(child),
                transaction: None,
            },
        );
        drop(sched);
        (scheduler, request)
    }

    #[tokio::test]
    async fn unsupported_required_capture_aborts_without_granting_the_child() {
        #[derive(Debug)]
        struct UnsupportedProbe {
            _reservation: ReservedProbe,
        }

        #[async_trait::async_trait]
        impl ReservedChildExitCapture for UnsupportedProbe {
            async fn arm(&mut self) -> Result<ChildExitCaptureArm, reverie::Error> {
                Ok(ChildExitCaptureArm::Unsupported)
            }

            async fn finish(
                self: Box<Self>,
            ) -> Result<reverie::ChildExitCaptureOutcome, reverie::Error> {
                panic!("unsupported capture must never finish as publication")
            }
        }

        let (scheduler, request) = setup();
        let dropped = Arc::new(AtomicUsize::new(0));
        let selected = SelectedCapture {
            request,
            transaction: Box::new(UnsupportedProbe {
                _reservation: ReservedProbe {
                    scheduler: Arc::downgrade(&scheduler),
                    dropped: dropped.clone(),
                },
            }),
        };
        let result = arm_selected(scheduler.clone(), selected).await;
        assert!(
            result.is_err(),
            "required capture must not take the normal route"
        );
        drop(result);
        let sched = scheduler.lock().unwrap();
        assert!(sched.backend_failed());
        assert!(sched.native_child_capture.holds.is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(
            !sched
                .child_exit_publications_completed
                .contains(&DetTid::from_raw(2))
        );
    }

    #[test]
    fn failure_before_grant_guard_releases_stored_reply_once_outside_scheduler_lock() {
        let (scheduler, request) = setup();
        let dropped = Arc::new(AtomicUsize::new(0));
        let wake = {
            let mut sched = scheduler.lock().unwrap();
            sched
                .native_child_capture
                .holds
                .get_mut(&DetTid::from_raw(2))
                .unwrap()
                .transaction = Some(Box::new(ReservedProbe {
                scheduler: Arc::downgrade(&scheduler),
                dropped: dropped.clone(),
            }));
            let wake = sched.report_backend_failure(reverie::BackendFailure {
                pid: request.child,
                tid: request.child,
                phase: "selected grant refused before Arm",
            });
            assert!(sched.native_child_capture.holds.is_empty());
            assert_eq!(dropped.load(Ordering::SeqCst), 0);
            wake
        };
        drop(FailureDrain(scheduler.clone()));
        if let Some(wake) = wake {
            let _ = wake.send(());
        }
        drain_failure(&scheduler);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn scheduler_scope_cancellation_aborts_a_stored_unarmed_reservation() {
        let (scheduler, _) = setup();
        let dropped = Arc::new(AtomicUsize::new(0));
        scheduler
            .lock()
            .unwrap()
            .native_child_capture
            .holds
            .get_mut(&DetTid::from_raw(2))
            .unwrap()
            .transaction = Some(Box::new(ReservedProbe {
            scheduler: Arc::downgrade(&scheduler),
            dropped: dropped.clone(),
        }));
        drop(FailureDrain(scheduler.clone()));
        let sched = scheduler.lock().unwrap();
        assert!(sched.backend_failed());
        assert!(sched.native_child_capture.holds.is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn capture_completion_survives_child_consuming_cleanup_but_not_parent_reincarnation() {
        let (scheduler, request) = setup();
        scheduler
            .lock()
            .unwrap()
            .thread_mms
            .remove(&DetTid::from_raw(2));
        let mut guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        guard.complete().unwrap();
        assert!(
            scheduler
                .lock()
                .unwrap()
                .native_child_capture
                .holds
                .is_empty()
        );
        assert!(!scheduler.lock().unwrap().backend_failed());

        let (scheduler, request) = setup();
        scheduler
            .lock()
            .unwrap()
            .thread_mms
            .remove(&DetTid::from_raw(1));
        let mut guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        assert!(guard.complete().is_err());
        drop(guard);
        let sched = scheduler.lock().unwrap();
        assert!(sched.backend_failed());
        assert!(sched.native_child_capture.holds.is_empty());
    }

    #[test]
    fn abandoning_armed_transaction_is_terminal_without_publication_credit() {
        let (scheduler, request) = setup();
        let guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        drop(guard);
        let sched = scheduler.lock().unwrap();
        assert!(sched.backend_failed());
        assert!(sched.native_child_capture.holds.is_empty());
        assert!(
            !sched
                .child_exit_publications_completed
                .contains(&DetTid::from_raw(2))
        );
    }
}
