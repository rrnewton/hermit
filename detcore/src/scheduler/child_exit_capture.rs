/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Selected Exit grants with independently serviced native capture.
//! Scheduler locks protect reservations, never native control awaits.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;

use reverie::BackendChildExitCapture;
use reverie::ChildExitAllocationState;
use reverie::ChildExitCaptureArm;
use reverie::ChildExitCaptureEffect;
use reverie::ChildExitCaptureOutcome;
use reverie::ChildExitCaptureRequest;
use reverie::ChildExitSignalAdmission;
use reverie::ReservedChildExitCapture;

use super::SchedResponse;
use super::Scheduler;
use super::parked::NextTurnOwner;
use super::parked::ResourceOrigin;
use super::parked::RpcOrigin;
use crate::ivar::Ivar;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::ChildWaitSpec;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::MmId;
use crate::types::SigWrapper;

#[derive(Debug, Default)]
pub(super) struct CaptureState {
    control: Option<Arc<dyn BackendChildExitCapture>>,
    sequence: u64,
    pub(super) holds: BTreeMap<DetTid, Hold>,
    aborted: Vec<Box<dyn ReservedChildExitCapture>>,
    coverage: BTreeMap<DetTid, Coverage>,
    /// Required allocations cancelled by terminal parent lifetime, not delivered.
    terminal_cancellations: u64,
}

#[derive(Debug)]
pub(super) struct Hold {
    request: ChildExitCaptureRequest,
    parent_mm: MmId,
    child_mm: MmId,
    parent_turn: ParentTurn,
    transaction: Option<Box<dyn ReservedChildExitCapture>>,
}

/// The ordinary reply remains reserved until receipt accounting is committed.
/// A signal drain may replace its request, but must not replace its response,
/// epoch or RPC incarnation. No extra parent turn is introduced.
#[derive(Clone, Debug)]
struct ParentTurn {
    response: Ivar<SchedResponse>,
    epoch: u64,
    origin: ResourceOrigin,
    wait: Option<ChildWaitSpec>,
}

#[derive(Clone, Debug)]
struct AllocationCoverage {
    state: ChildExitAllocationState,
    notified: bool,
    /// A real RW InboundSignal request, not a wake or Release intent.
    hook_token: Option<u64>,
    required: bool,
    /// Obligations transferred by actual native replacement receipts. They
    /// remain owed by this allocation; replacement is not an observed hook.
    superseded: u64,
}

/// At most the complete native live allocation set. Missing allocations may
/// shed dedup memory only after their required hook was actually observed.
/// Snapshot absence never means delivery, nor does it authorize suppression.
#[derive(Clone, Debug)]
struct Coverage {
    mm: MmId,
    image_epoch: u64,
    domain: u64,
    version: u64,
    interval: u64,
    reservation: u64,
    highest_generation: u64,
    allocations: BTreeMap<u64, AllocationCoverage>,
}

impl Coverage {
    fn from_receipt(mm: MmId, outcome: &ChildExitCaptureOutcome) -> Result<Self, String> {
        let mut coverage = Self {
            mm,
            image_epoch: outcome.snapshot.image_epoch,
            domain: outcome.domain,
            version: 0,
            interval: 0,
            reservation: 0,
            highest_generation: 0,
            allocations: BTreeMap::new(),
        };
        coverage.observe(outcome)?;
        Ok(coverage)
    }

    fn observe(&mut self, outcome: &ChildExitCaptureOutcome) -> Result<(), String> {
        let snapshot = &outcome.snapshot;
        if snapshot.image_epoch == 0
            || snapshot.image_epoch != self.image_epoch
            || outcome.domain == 0
            || outcome.domain != self.domain
            || outcome.interval <= self.interval
            || outcome.request.reservation <= self.reservation
            || snapshot.version <= self.version
            || snapshot.allocations.len() > 4
        {
            return Err("capture domain snapshot changed identity or regressed".into());
        }
        let live: BTreeSet<_> = snapshot.allocations.iter().map(|r| r.generation).collect();
        let queued = snapshot
            .allocations
            .iter()
            .filter(|a| a.state == ChildExitAllocationState::Queued)
            .count();
        let active = snapshot
            .allocations
            .iter()
            .filter(|a| a.state == ChildExitAllocationState::Active)
            .count();
        let returning = snapshot
            .allocations
            .iter()
            .filter(|a| a.state == ChildExitAllocationState::Returning)
            .count();
        if live.len() != snapshot.allocations.len()
            || live.contains(&0)
            || queued > 2
            || active > 1
            || returning > 1
            || !snapshot.allocations.iter().any(|r| {
                r.generation == outcome.allocation && r.state == ChildExitAllocationState::Queued
            })
        {
            return Err("capture snapshot is not a complete queued-allocation witness".into());
        }
        match (outcome.effect, outcome.replaced_allocation) {
            (ChildExitCaptureEffect::Replaced, Some(victim))
                if victim != 0 && victim != outcome.allocation && !live.contains(&victim) => {}
            (ChildExitCaptureEffect::Linked | ChildExitCaptureEffect::Coalesced, None) => {}
            _ => return Err("capture replacement lacks its exact native victim".into()),
        }
        let mut superseded = 0_u64;
        let mut transferred_notification = false;
        for (generation, old) in &self.allocations {
            if !live.contains(generation) && old.required && old.hook_token.is_none() {
                if outcome.replaced_allocation != Some(*generation) {
                    return Err(
                        "required captured allocation vanished without its real Tool hook".into(),
                    );
                }
                superseded = old
                    .superseded
                    .checked_add(1)
                    .ok_or("captured replacement obligation count exhausted")?;
                // Replacing an undelivered pending allocation transfers its
                // notification ownership too. It must not interrupt the same
                // borrowed wait a second time merely by changing allocation.
                transferred_notification = old.notified;
            }
        }
        let mut next = BTreeMap::new();
        for allocation in &snapshot.allocations {
            let mut entry = if let Some(old) = self.allocations.get(&allocation.generation) {
                if matches!(
                    (old.state, allocation.state),
                    (
                        ChildExitAllocationState::Active,
                        ChildExitAllocationState::Queued
                    ) | (
                        ChildExitAllocationState::Returning,
                        ChildExitAllocationState::Queued | ChildExitAllocationState::Active
                    )
                ) {
                    return Err("captured allocation ownership moved backwards".into());
                }
                old.clone()
            } else {
                // Every prior observation is complete. Monotonic, never-reused
                // native generations therefore cannot reappear below this mark.
                if allocation.generation <= self.highest_generation {
                    return Err("retired native allocation generation reappeared".into());
                }
                AllocationCoverage {
                    state: allocation.state,
                    notified: false,
                    hook_token: None,
                    required: false,
                    superseded: 0,
                }
            };
            entry.state = allocation.state;
            if allocation.generation == outcome.allocation {
                entry.required = true;
                entry.notified |= transferred_notification;
                entry.superseded = entry
                    .superseded
                    .checked_add(superseded)
                    .ok_or("captured replacement obligation count exhausted")?;
            }
            next.insert(allocation.generation, entry);
        }
        self.highest_generation = live
            .last()
            .copied()
            .unwrap_or(0)
            .max(self.highest_generation);
        self.allocations = next;
        self.version = snapshot.version;
        self.interval = outcome.interval;
        self.reservation = outcome.request.reservation;
        Ok(())
    }
}

fn managed_child_wait(parent: DetTid, resources: &Resources) -> Option<ChildWaitSpec> {
    if resources.tid != parent || resources.resources.len() != 1 {
        return None;
    }
    resources
        .resources
        .iter()
        .find_map(|(resource, permission)| match resource {
            ResourceID::WaitChild {
                parent: owner,
                spec,
            } if *owner == parent && *permission == Permission::R => Some(*spec),
            _ => None,
        })
}

impl ParentTurn {
    fn validate(&self, sched: &Scheduler, parent: DetTid) -> Result<bool, String> {
        let turn = sched
            .next_turns
            .get(&parent)
            .ok_or("captured parent turn disappeared")?;
        let origin = turn
            .protocol
            .origin
            .ok_or("captured parent RPC origin disappeared")?;
        if turn.resp != self.response
            || turn.protocol.epoch != self.epoch
            || turn.protocol.owner != NextTurnOwner::Ordinary
            || origin.rpc != self.origin.rpc
            || origin.mm != self.origin.mm
            || turn.resp.try_read().is_some()
        {
            return Err("capture receipt no longer owns the reserved parent reply".into());
        }
        let Some(spec) = self.wait else {
            return Ok(false);
        };
        let resources = turn
            .req
            .try_read()
            .and_then(Result::ok)
            .ok_or("captured wait has no pending resource request")?;
        let same_wait = managed_child_wait(parent, &resources) == Some(spec);
        // Another authentic signal may already have been drained for this
        // exact response/epoch. Its rewrite is still this same waiting borrow.
        let signaled_wait = resources.resources.len() == 1
            && resources.signal_interrupt_errno().is_none()
            && resources.resources.iter().any(|(resource, permission)| {
                matches!(resource, ResourceID::WaitidSignals(_)) && *permission == Permission::W
            });
        if !same_wait && !signaled_wait {
            return Err("capture receipt no longer owns its managed child wait".into());
        }
        Ok(true)
    }
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

    /// The caller proves this process incarnation will execute no further
    /// visible instructions. An exec teardown is not that proof: pending signals
    /// may survive exec, so that path must retain or explicitly transfer coverage.
    pub(super) fn retire_native_capture_parent(&mut self, parent: DetTid, mm: MmId) {
        if self
            .native_child_capture
            .coverage
            .get(&parent)
            .is_some_and(|c| c.mm == mm)
        {
            let retired = self.native_child_capture.coverage.remove(&parent).unwrap();
            let cancelled = retired
                .allocations
                .values()
                .filter(|a| a.required && a.hook_token.is_none())
                .count() as u64;
            self.native_child_capture.terminal_cancellations = self
                .native_child_capture
                .terminal_cancellations
                .checked_add(cancelled)
                .expect("capture terminal accounting overflow");
            tracing::debug!(
                "[child-exit-capture] parent-terminal parent={} cancelled-required={}",
                parent,
                cancelled
            );
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
        let wait = managed_child_wait(parent, &resources);
        let allow_syscall_callback = wait.is_some();
        if turn.resp.try_read().is_some() {
            return Err("capture parent ordinary response was already granted".into());
        }
        let parent_turn = ParentTurn {
            response: turn.resp.clone(),
            epoch: turn.protocol.epoch,
            origin,
            wait,
        };
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
                parent_turn,
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

    /// Account only an actual signal-hook resource. Identity is not a resource
    /// key, does not affect conflict ordering, and never requests another wake.
    pub(crate) fn observe_child_signal_hook(
        &mut self,
        parent: DetTid,
        mm: MmId,
        resources: &Resources,
        admission: Option<ChildExitSignalAdmission>,
    ) -> Result<(), String> {
        let real_hook = resources.tid == parent
            && resources.resources.len() == 1
            && resources
                .resources
                .get(&ResourceID::InboundSignal(SigWrapper::from(
                    nix::sys::signal::Signal::SIGCHLD,
                )))
                == Some(&Permission::RW);
        if !real_hook {
            return if admission.is_some() {
                Err("native child admission attached to a non-delivery resource".into())
            } else {
                Ok(())
            };
        }
        let Some(admission) = admission else {
            return if self.native_child_capture.coverage.contains_key(&parent) {
                Err("native child signal hook omitted its allocation identity".into())
            } else {
                Ok(())
            };
        };
        if admission.image_epoch == 0
            || admission.domain == 0
            || admission.allocation == 0
            || admission.token == 0
            || self.thread_mm(parent) != Some(mm)
        {
            return Err("native child signal hook has no current identity".into());
        }
        let Some(coverage) = self.native_child_capture.coverage.get_mut(&parent) else {
            // Real unqualified signals still traverse the unchanged hook. They
            // acquire no publication credit from merely carrying an identity.
            return Ok(());
        };
        if coverage.mm != mm
            || coverage.image_epoch != admission.image_epoch
            || coverage.domain != admission.domain
        {
            return Err("native child signal hook changed captured incarnation/domain".into());
        }
        let Some(allocation) = coverage.allocations.get_mut(&admission.allocation) else {
            return Ok(());
        };
        if allocation.hook_token.is_some() {
            return Err("native allocation reported more than one actual Tool hook".into());
        }
        allocation.hook_token = Some(admission.token);
        allocation.state = ChildExitAllocationState::Active;
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
    fn account(&self, outcome: &ChildExitCaptureOutcome) -> Result<(), String> {
        let mut sched = self.scheduler.lock().unwrap();
        let child = DetTid::from_raw(self.request.child.as_raw());
        let parent = DetTid::from_raw(self.request.parent.as_raw());
        let hold = sched
            .native_child_capture
            .holds
            .get(&child)
            .ok_or("capture hold disappeared before receipt accounting")?;
        if outcome.request != self.request
            || hold.request != self.request
            || sched.thread_mm(parent) != Some(hold.parent_mm)
            || sched.thread_mm(child).is_some_and(|mm| mm != hold.child_mm)
            || sched.backend_failed()
        {
            return Err("capture receipt changed its grant or incarnation".into());
        }
        let notify_wait = hold.parent_turn.validate(&sched, parent)?;
        let parent_mm = hold.parent_mm;
        // Validate and reconcile a private copy before publishing any ledger
        // or notification state. All native awaits and reply release are outside
        // this lock; the run-wide hold stays live until release succeeds.
        let mut coverage = if let Some(old) = sched.native_child_capture.coverage.get(&parent) {
            if old.mm != parent_mm {
                return Err("capture ledger belongs to a retired parent incarnation".into());
            }
            let mut next = old.clone();
            next.observe(outcome)?;
            next
        } else {
            Coverage::from_receipt(parent_mm, outcome)?
        };
        let allocation = coverage
            .allocations
            .get_mut(&outcome.allocation)
            .expect("validated queued capture allocation");
        let notify_wait = notify_wait && !allocation.notified;
        if notify_wait {
            allocation.notified = true;
        }
        sched.native_child_capture.coverage.insert(parent, coverage);
        if notify_wait {
            // This only posts the existing deferred metadata wake. The regular
            // wait code decides readiness, mask, disposition and restart; the
            // actual signal resource and handler have not happened here.
            sched
                .notify_signal_pending(parent, SigWrapper::from(nix::sys::signal::Signal::SIGCHLD));
            sched.wake_control_waiter();
        }
        Ok(())
    }

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
            tracing::error!(
                "[child-exit-capture] required capture unsupported reservation={} parent={} child={}",
                request.reservation,
                request.parent,
                request.child
            );
            // Selection already committed to capture. GrantGuard must latch
            // backend failure and abort the hold, not grant an uncaptured Exit.
            return Err(());
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
        match super::until_backend_failure(&scheduler, selected.transaction.capture()).await {
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
                if let Some(victim) = outcome.replaced_allocation {
                    tracing::debug!(
                        "[child-exit-capture] replacement reservation={} parent={} domain={} victim={} allocation={}",
                        request.reservation,
                        request.parent,
                        outcome.domain,
                        victim,
                        outcome.allocation
                    );
                }
                if let Err(error) = guard.account(&outcome) {
                    tracing::error!("child-exit capture accounting failed: {error}");
                    return;
                }
                // Consumes the transport reservation only after the ledger and
                // notification are visible. A failed release remains terminal;
                // its Drop and every endpoint lock occur outside scheduler.lock.
                if let Err(error) = selected.transaction.release() {
                    tracing::error!("child-exit capture reply release failed: {error}");
                    return;
                }
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

    use reverie::ChildExitCaptureAllocation;
    use reverie::ChildExitCaptureEffect;
    use reverie::ChildExitCaptureSnapshot;

    use super::*;
    use crate::types::ChildWaitExitClass;
    use crate::types::ChildWaitSelector;

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

        async fn capture(&mut self) -> Result<reverie::ChildExitCaptureOutcome, reverie::Error> {
            unreachable!("failure test must not fabricate capture")
        }

        fn release(self: Box<Self>) -> Result<(), reverie::Error> {
            unreachable!("failure test must not release the parent reply")
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
                parent_turn: ParentTurn {
                    response: Ivar::new(),
                    epoch: 1,
                    origin: ResourceOrigin {
                        rpc: RpcOrigin::DirectRequestResources,
                        mm: MmId::initial(parent),
                        control: super::super::parked::ControlCapability::None,
                    },
                    wait: None,
                },
                transaction: None,
            },
        );
        drop(sched);
        (scheduler, request)
    }

    fn setup_wait_for_b() -> (Arc<Mutex<Scheduler>>, ChildExitCaptureRequest) {
        let (scheduler, request) = setup();
        let parent = DetTid::from_raw(1);
        let spec = ChildWaitSpec {
            selector: ChildWaitSelector::Exact(DetPid::from_raw(3)),
            owner: None,
            exit_class: ChildWaitExitClass::Sigchld,
        };
        let mut sched = scheduler.lock().unwrap();
        super::super::test::register_known_thread_for(&mut sched, parent);
        let mut waiting = Resources::new(parent);
        waiting.insert(ResourceID::WaitChild { parent, spec }, Permission::R);
        let origin = ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources,
            mm: MmId::initial(parent),
            control: super::super::parked::ControlCapability::None,
        };
        let turn = sched.next_turns.get_mut(&parent).unwrap();
        turn.req = Ivar::full(Ok(waiting));
        turn.protocol.origin = Some(origin);
        turn.protocol.epoch = 11;
        let parent_turn = ParentTurn {
            response: turn.resp.clone(),
            epoch: 11,
            origin,
            wait: Some(spec),
        };
        sched
            .native_child_capture
            .holds
            .get_mut(&DetTid::from_raw(2))
            .unwrap()
            .parent_turn = parent_turn;
        sched.blocked.child_waiters.insert(parent, (parent, spec));
        drop(sched);
        (scheduler, request)
    }

    fn receipt(request: ChildExitCaptureRequest, generation: u64) -> ChildExitCaptureOutcome {
        ChildExitCaptureOutcome {
            request,
            domain: 50,
            interval: 60,
            allocation: generation,
            effect: ChildExitCaptureEffect::Linked,
            replaced_allocation: None,
            snapshot: ChildExitCaptureSnapshot {
                image_epoch: 1,
                version: 2,
                allocations: vec![ChildExitCaptureAllocation {
                    generation,
                    state: ChildExitAllocationState::Queued,
                }],
            },
        }
    }

    #[test]
    fn captured_a_notifies_exact_b_wait_once_across_coalesced_intervals() {
        let (scheduler, request) = setup_wait_for_b();
        let mut guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        let mut captured = receipt(request, 61);
        guard.account(&captured).unwrap();
        {
            let mut sched = scheduler.lock().unwrap();
            assert_eq!(
                sched.pending_cross_task_signals[&DetTid::from_raw(1)].len(),
                1
            );
            sched.drain_pending_cross_task_signals();
            assert!(sched.blocked.child_waiters.is_empty());
            assert_eq!(
                sched.inbound_signals(DetTid::from_raw(1)),
                vec![SigWrapper::from(nix::sys::signal::Signal::SIGCHLD)]
            );
            assert_eq!(
                sched
                    .run_queue
                    .tids()
                    .filter(|tid| **tid == DetTid::from_raw(1))
                    .count(),
                1
            );
            // A new genuine publication can coalesce onto the still-queued A.
            // It has a new interval and reservation, but not a second signal.
            captured.request.reservation += 1;
            captured.interval += 2;
            captured.snapshot.version += 2;
            captured.effect = ChildExitCaptureEffect::Coalesced;
            sched
                .native_child_capture
                .holds
                .get_mut(&DetTid::from_raw(2))
                .unwrap()
                .request = captured.request;
        }
        guard.request = captured.request;
        guard.account(&captured).unwrap();
        {
            let sched = scheduler.lock().unwrap();
            assert!(
                sched.pending_cross_task_signals.is_empty(),
                "draining does not erase allocation dedup ownership"
            );
            assert!(
                sched.native_child_capture.coverage[&DetTid::from_raw(1)].allocations[&61]
                    .hook_token
                    .is_none(),
                "a wake is not the real Tool hook"
            );
        }
        guard.complete().unwrap();
    }

    #[test]
    fn snapshot_absence_cannot_discharge_required_hook_and_cannot_reuse_generation() {
        let (_, request) = setup();
        let old = receipt(request, 61);
        let mut coverage =
            Coverage::from_receipt(MmId::initial(DetTid::from_raw(1)), &old).unwrap();
        let mut next = receipt(request, 71);
        next.request.reservation += 1;
        next.interval = 70;
        next.snapshot.version = 7;
        assert!(coverage.observe(&next).is_err(), "absence is not delivery");
        assert_eq!(
            coverage.interval, old.interval,
            "failed reconciliation is inert"
        );
        coverage.allocations.get_mut(&61).unwrap().hook_token = Some(81);
        coverage.observe(&next).unwrap();
        assert!(!coverage.allocations.contains_key(&61));
        let mut replay = receipt(next.request, 61);
        replay.request.reservation += 1;
        replay.interval = 90;
        replay.snapshot.version = 9;
        coverage.allocations.get_mut(&71).unwrap().hook_token = Some(91);
        assert!(
            coverage.observe(&replay).is_err(),
            "eviction must not permit old generation replay"
        );
    }

    #[test]
    fn exact_replacement_transfers_obligation_and_notification_without_delivery() {
        let (_, request) = setup();
        let mut coverage =
            Coverage::from_receipt(MmId::initial(DetTid::from_raw(1)), &receipt(request, 61))
                .unwrap();
        coverage.allocations.get_mut(&61).unwrap().notified = true;
        let mut replacement = receipt(request, 71);
        replacement.request.reservation += 1;
        replacement.interval += 2;
        replacement.snapshot.version += 2;
        replacement.effect = ChildExitCaptureEffect::Replaced;
        replacement.replaced_allocation = Some(62);
        assert!(
            coverage.observe(&replacement).is_err(),
            "wrong victim is no retirement proof"
        );
        replacement.replaced_allocation = Some(61);
        coverage.observe(&replacement).unwrap();
        assert!(!coverage.allocations.contains_key(&61));
        let allocation = &coverage.allocations[&71];
        assert!(allocation.required && allocation.notified);
        assert_eq!(allocation.superseded, 1);
        assert!(
            allocation.hook_token.is_none(),
            "replacement is not delivery"
        );
        let mut disappearance = receipt(replacement.request, 81);
        disappearance.request.reservation += 1;
        disappearance.interval = 90;
        disappearance.snapshot.version = 9;
        assert!(
            coverage.observe(&disappearance).is_err(),
            "transferred obligation stays owed"
        );
    }

    #[test]
    fn terminal_parent_cancellation_requires_the_exact_incarnation() {
        let (scheduler, request) = setup_wait_for_b();
        let parent = DetTid::from_raw(1);
        let mm = MmId::initial(parent);
        let mut guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        guard.account(&receipt(request, 61)).unwrap();
        guard.complete().unwrap();
        let mut sched = scheduler.lock().unwrap();
        sched.retire_native_capture_parent(parent, mm.for_exec(parent));
        assert!(sched.native_child_capture.coverage.contains_key(&parent));
        assert_eq!(sched.native_child_capture.terminal_cancellations, 0);
        sched.retire_native_capture_parent(parent, mm);
        assert!(!sched.native_child_capture.coverage.contains_key(&parent));
        assert_eq!(sched.native_child_capture.terminal_cancellations, 1);
    }

    #[test]
    fn leader_thread_exit_cannot_cancel_a_live_process_obligation() {
        let (_, request) = setup();
        let leader = DetTid::from_raw(1);
        let worker = DetTid::from_raw(4);
        let mm = MmId::initial(leader);
        let mut sched = Scheduler::new(&crate::Config::default());
        sched.thread_tree.add_child(leader, leader, true);
        sched.thread_tree.add_child(leader, worker, false);
        for tid in [leader, worker] {
            super::super::test::register_known_thread_for(&mut sched, tid);
            sched.record_thread_mm(tid, mm);
        }
        sched.native_child_capture.coverage.insert(
            leader,
            Coverage::from_receipt(mm, &receipt(request, 61)).unwrap(),
        );
        sched.logically_kill_thread(&leader, &leader, mm);
        assert!(sched.native_child_capture.coverage.contains_key(&leader));
        assert_eq!(sched.native_child_capture.terminal_cancellations, 0);
        sched.logically_kill_thread(&worker, &leader, mm);
        assert!(!sched.native_child_capture.coverage.contains_key(&leader));
        assert_eq!(sched.native_child_capture.terminal_cancellations, 1);
    }

    #[test]
    fn only_exact_real_hook_identity_accounts_capture_without_another_wake() {
        let (scheduler, request) = setup_wait_for_b();
        let mut guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        guard.account(&receipt(request, 61)).unwrap();
        let parent = DetTid::from_raw(1);
        let mm = MmId::initial(parent);
        let mut hook = Resources::new(parent);
        hook.insert(
            ResourceID::InboundSignal(SigWrapper::from(nix::sys::signal::Signal::SIGCHLD)),
            Permission::RW,
        );
        let admission = ChildExitSignalAdmission {
            image_epoch: 1,
            domain: 50,
            allocation: 61,
            token: 81,
        };
        {
            let mut sched = scheduler.lock().unwrap();
            sched.pending_cross_task_signals.clear();
            let mut wrong = admission;
            wrong.domain += 1;
            assert!(
                sched
                    .observe_child_signal_hook(parent, mm, &hook, Some(wrong))
                    .is_err()
            );
            assert!(
                sched
                    .observe_child_signal_hook(parent, mm.for_exec(parent), &hook, Some(admission))
                    .is_err()
            );
            sched
                .observe_child_signal_hook(parent, mm, &hook, Some(admission))
                .unwrap();
            assert!(sched.pending_cross_task_signals.is_empty());
            assert_eq!(
                sched.native_child_capture.coverage[&parent].allocations[&61].hook_token,
                Some(81)
            );
            assert!(
                sched
                    .observe_child_signal_hook(parent, mm, &hook, Some(admission))
                    .is_err(),
                "duplicate actual hook is not another publication"
            );
        }
        guard.complete().unwrap();
    }

    #[test]
    fn new_parent_response_cannot_receive_old_capture_notification() {
        let (scheduler, request) = setup_wait_for_b();
        let guard = GrantGuard {
            scheduler: scheduler.clone(),
            request,
            finished: false,
        };
        scheduler
            .lock()
            .unwrap()
            .next_turns
            .get_mut(&DetTid::from_raw(1))
            .unwrap()
            .resp = Ivar::new();
        assert!(guard.account(&receipt(request, 61)).is_err());
        assert!(
            scheduler
                .lock()
                .unwrap()
                .pending_cross_task_signals
                .is_empty()
        );
        drop(guard);
        assert!(scheduler.lock().unwrap().backend_failed());
    }

    #[tokio::test]
    async fn receipt_accounting_precedes_outside_lock_reply_release() {
        #[derive(Debug)]
        struct CapturedProbe {
            reservation: ReservedProbe,
            outcome: ChildExitCaptureOutcome,
        }
        #[async_trait::async_trait]
        impl ReservedChildExitCapture for CapturedProbe {
            async fn arm(&mut self) -> Result<ChildExitCaptureArm, reverie::Error> {
                Ok(ChildExitCaptureArm::Armed)
            }
            async fn capture(&mut self) -> Result<ChildExitCaptureOutcome, reverie::Error> {
                Ok(self.outcome.clone())
            }
            fn release(self: Box<Self>) -> Result<(), reverie::Error> {
                let scheduler = self.reservation.scheduler.upgrade().unwrap();
                let sched = scheduler
                    .try_lock()
                    .expect("reply released under scheduler lock");
                let parent = DetTid::from_raw(1);
                assert!(
                    sched
                        .native_child_capture
                        .holds
                        .contains_key(&DetTid::from_raw(2))
                );
                assert!(sched.native_child_capture.coverage[&parent].allocations[&61].notified);
                assert_eq!(sched.pending_cross_task_signals[&parent].len(), 1);
                drop(sched);
                Ok(())
            }
        }
        let (scheduler, request) = setup_wait_for_b();
        let dropped = Arc::new(AtomicUsize::new(0));
        let selected = SelectedCapture {
            request,
            transaction: Box::new(CapturedProbe {
                reservation: ReservedProbe {
                    scheduler: Arc::downgrade(&scheduler),
                    dropped: dropped.clone(),
                },
                outcome: receipt(request, 61),
            }),
        };
        arm_selected(scheduler.clone(), selected)
            .await
            .unwrap()
            .unwrap()
            .await;
        let sched = scheduler.lock().unwrap();
        assert!(!sched.backend_failed());
        assert!(sched.native_child_capture.holds.is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
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

            async fn capture(
                &mut self,
            ) -> Result<reverie::ChildExitCaptureOutcome, reverie::Error> {
                panic!("unsupported capture must never finish as publication")
            }

            fn release(self: Box<Self>) -> Result<(), reverie::Error> {
                panic!("unsupported capture must never release its parent reply")
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
