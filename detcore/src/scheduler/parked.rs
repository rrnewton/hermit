/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A logical parked request can use several single-use RPC responses. Only the
//! scheduler daemon moves its queue entry; RPC handlers post bounded intents.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use reverie::CallbackSignalSite;
use reverie::ParkedObservationLease;
use reverie::PreparedSignalToken;
use reverie::ProcessAlarmSignalOutcome;
use reverie::SignalEvent;
use reverie::SignalTarget;
use serde::Deserialize;
use serde::Serialize;

use super::SchedRequest;
use super::SchedResponse;
use super::Scheduler;
use super::real_timer::ExpiryId;
use super::real_timer::TimerFailure;
use super::runqueue::SuspendedRunQueueEntry;
use super::timed_waiters::SignalTimerId;
use crate::ivar::Ivar;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::tool_global::ResumeStatus;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ParkedWaitPolicy {
    NanosleepNoHandlerRestart { absolute_deadline: LogicalTime },
    PauseNoHandlerRestart,
}
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize
)]
pub struct ContinuationId {
    pub dettid: DetTid,
    pub nonce: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum RpcOrigin {
    DirectRequestResources,
    ResumeParkedRequest {
        continuation: ContinuationId,
        cycle: u64,
    },
    ParentContinue,
    TraceSchedEvent,
    FutexAction,
    ThreadStart,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlCapability {
    None,
    ParkedWait {
        policy: ParkedWaitPolicy,
        site: CallbackSignalSite,
    },
    PublishOnly {
        lease: ParkedObservationLease,
        site: CallbackSignalSite,
    },
    /// The backend authenticated the original scalar write and its captured
    /// output alias. Publication resumes this request; it never observes a
    /// signal or interrupts the write before its normal return boundary.
    CapturedWrite {
        site: CallbackSignalSite,
    },
}
impl ControlCapability {
    fn site(self) -> Option<CallbackSignalSite> {
        match self {
            Self::None => None,
            Self::ParkedWait { site, .. }
            | Self::PublishOnly { site, .. }
            | Self::CapturedWrite { site } => Some(site),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceOrigin {
    pub rpc: RpcOrigin,
    pub mm: MmId,
    pub control: ControlCapability,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum NextTurnOwner {
    #[default]
    Ordinary,
    Observation {
        wait: ContinuationId,
        lease: ParkedObservationLease,
    },
    ReturningCaught {
        completed_wait: ContinuationId,
    },
}
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnProtocol {
    pub epoch: u64,
    pub owner: NextTurnOwner,
    pub origin: Option<ResourceOrigin>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestKey {
    pub dettid: DetTid,
    pub mm: MmId,
    pub epoch: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AlarmControl {
    pub expiry: ExpiryId,
    pub continuation: ContinuationId,
    pub request: RequestKey,
    pub site: CallbackSignalSite,
    pub event: SignalEvent,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeTicket {
    pub continuation: ContinuationId,
    pub cycle: u64,
    pub next_epoch: u64,
    pub site: CallbackSignalSite,
    pub nonce: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResourceReply {
    Grant(ResumeStatus),
    PublishAlarm(Box<AlarmControl>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PublicationActivation {
    AwaitResume(ResumeTicket),
    Observe {
        wait: ContinuationId,
        lease: ParkedObservationLease,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ObservationFinish {
    ResumeSameWait,
    InterruptForCaught { selection: PreparedSignalToken },
    Terminate { selection: PreparedSignalToken },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FinishAck {
    AwaitResume(ResumeTicket),
    Interrupted,
    Terminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProtocolFailure {
    Identity,
    Phase,
    Unsupported,
    Overflow,
    UnexpectedControl,
    Timer(TimerFailure),
    Observation(reverie::SignalObservationFailure),
}
impl From<TimerFailure> for ProtocolFailure {
    fn from(value: TimerFailure) -> Self {
        Self::Timer(value)
    }
}
#[derive(Clone, Debug)]
enum SavedMembership {
    Timed(LogicalTime),
    Queued(SuspendedRunQueueEntry),
}
#[derive(Clone, Copy, Debug)]
enum ContinuationPhase {
    Waiting,
    AwaitingPublication(AlarmControl),
    AwaitingResumeRegistration(ResumeTicket),
    Observing(ParkedObservationLease),
}
#[derive(Clone, Debug)]
struct OwnedRequest {
    id: ContinuationId,
    request: Ivar<SchedRequest>,
    response: Ivar<SchedResponse>,
    resources: Resources,
    origin: ResourceOrigin,
    owner: NextTurnOwner,
    membership: Option<SavedMembership>,
    cycle: u64,
    phase: ContinuationPhase,
}
#[derive(Clone, Debug)]
pub(crate) enum ControlIntent {
    Publication {
        control: AlarmControl,
        outcome: ProcessAlarmSignalOutcome,
        ack: Ivar<Result<PublicationActivation, ProtocolFailure>>,
    },
    Finish {
        wait: ContinuationId,
        lease: ParkedObservationLease,
        site: CallbackSignalSite,
        finish: ObservationFinish,
        ack: Ivar<Result<FinishAck, ProtocolFailure>>,
    },
    Resume {
        ticket: ResumeTicket,
        site: CallbackSignalSite,
        response: Ivar<SchedResponse>,
    },
}
#[derive(Debug, Default)]
pub(super) struct ParkedRequests {
    nonce: u64,
    requests: BTreeMap<ContinuationId, OwnedRequest>,
    intents: VecDeque<ControlIntent>,
    wake: Ivar<()>,
    pub failure: Option<ProtocolFailure>,
}

impl Scheduler {
    pub(crate) fn fail_parked(&mut self, tid: DetTid, failure: ProtocolFailure) {
        if self.parked.failure.is_none() {
            tracing::error!(
                "KVM parked signal protocol failed for dettid {}: {:?}",
                tid,
                failure
            );
            self.parked.failure = Some(failure);
        }
        let pid = self
            .thread_tree
            .thread_to_leader
            .get(&tid)
            .copied()
            .unwrap_or(tid);
        self.real_timers.fail(
            pid,
            match failure {
                ProtocolFailure::Timer(f) => f,
                _ => TimerFailure::Unsupported,
            },
        );
        self.blocked.timed_waiters.remove_kvm_real_deadline(pid);
        if let Some(wake) = self.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(pid.as_raw()),
            tid: reverie::Tid::from_raw(tid.as_raw()),
            phase: "KVM parked signal protocol",
        }) {
            let _ = wake.send(());
        }
    }

    pub(crate) fn install_resource_origin(
        &mut self,
        tid: DetTid,
        origin: ResourceOrigin,
    ) -> Result<(), ProtocolFailure> {
        let turn = self.next_turns.get(&tid).ok_or(ProtocolFailure::Identity)?;
        if !self.rpc_incarnation_matches(tid, origin.mm) {
            return Err(ProtocolFailure::Identity);
        }
        if let Some(site) = origin.control.site() {
            if !self.kvm_shared_dequeue_timers || !self.backend_is_kvm {
                return Err(ProtocolFailure::Unsupported);
            }
            let pid = *self
                .thread_tree
                .thread_to_leader
                .get(&tid)
                .ok_or(ProtocolFailure::Identity)?;
            self.real_timers.validate_site(pid, tid, origin.mm, site)?;
            match (origin.control, turn.protocol.owner) {
                (
                    ControlCapability::ParkedWait { .. } | ControlCapability::CapturedWrite { .. },
                    NextTurnOwner::Ordinary | NextTurnOwner::ReturningCaught { .. },
                ) => {}
                (
                    ControlCapability::PublishOnly { lease, .. },
                    NextTurnOwner::Observation {
                        lease: expected, ..
                    },
                ) if lease == expected => {}
                _ => return Err(ProtocolFailure::Identity),
            }
        }
        let turn = self
            .next_turns
            .get_mut(&tid)
            .ok_or(ProtocolFailure::Identity)?;
        if turn.req.try_read().is_some() {
            // Existing legacy signal merging must retain its installed origin.
            if turn.protocol.origin != Some(origin) {
                return Err(ProtocolFailure::Phase);
            }
        } else {
            turn.protocol.origin = Some(origin);
        }
        Ok(())
    }

    pub(crate) fn post_control(
        &mut self,
        tid: DetTid,
        mm: MmId,
        intent: ControlIntent,
    ) -> Result<(), ProtocolFailure> {
        if !self.rpc_incarnation_matches(tid, mm) || self.backend_failed() {
            return Err(ProtocolFailure::Identity);
        }
        let expected_tid = match &intent {
            ControlIntent::Publication { control, .. } => control.request.dettid,
            ControlIntent::Finish { wait, .. } => wait.dettid,
            ControlIntent::Resume { ticket, .. } => ticket.continuation.dettid,
        };
        if tid != expected_tid || self.parked.intents.len() >= 2 {
            return Err(ProtocolFailure::Identity);
        }
        self.parked.intents.push_back(intent);
        self.parked.wake.try_put(());
        Ok(())
    }

    pub(crate) fn registered_process(&self, tid: DetTid) -> Option<DetPid> {
        self.thread_tree.thread_to_leader.get(&tid).copied()
    }

    pub(crate) fn complete_signal_exec(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        previous_mm: MmId,
        current_mm: MmId,
        identity: reverie::SignalTaskIdentity,
    ) -> Result<(), ProtocolFailure> {
        if self.registered_process(tid) != Some(pid)
            || self.parked.requests.keys().any(|wait| wait.dettid == tid)
        {
            return Err(ProtocolFailure::Identity);
        }
        self.real_timers
            .rebind_after_exec(pid, tid, previous_mm, current_mm, identity)?;
        self.exec_incarnations.insert(tid, current_mm);
        Ok(())
    }

    pub(crate) fn parked_resume_resources(
        &self,
        ticket: ResumeTicket,
        tid: DetTid,
        mm: MmId,
        site: CallbackSignalSite,
    ) -> Result<(DetPid, Resources), ProtocolFailure> {
        let owned = self
            .parked
            .requests
            .get(&ticket.continuation)
            .ok_or(ProtocolFailure::Identity)?;
        if owned.id.dettid != tid
            || owned.origin.mm != mm
            || ticket.site != site
            || !matches!(owned.phase, ContinuationPhase::AwaitingResumeRegistration(expected) if expected == ticket)
        {
            return Err(ProtocolFailure::Identity);
        }
        Ok((
            *self
                .thread_tree
                .thread_to_leader
                .get(&tid)
                .ok_or(ProtocolFailure::Identity)?,
            owned.resources.clone(),
        ))
    }

    pub(crate) fn apply_signal_dequeue(
        &mut self,
        pid: DetPid,
        dequeue: reverie::SignalDequeue,
        now: LogicalTime,
    ) -> Result<super::real_timer::DequeueAck, TimerFailure> {
        if self.backend_failed() {
            self.real_timers.fail(pid, TimerFailure::Unsupported);
        }
        let (ack, next) = self.real_timers.dequeue(pid, dequeue, now)?;
        if let Some(deadline) = next {
            self.blocked
                .timed_waiters
                .insert_kvm_real_deadline(deadline, pid);
        }
        Ok(ack)
    }

    pub(crate) fn replace_real_timer(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        now: LogicalTime,
        duration: LogicalTime,
        interval: LogicalTime,
        signal: nix::sys::signal::Signal,
    ) -> Result<(LogicalTime, LogicalTime), TimerFailure> {
        if !self.backend_is_kvm {
            return Ok(self.register_alarm(pid, tid, now, duration, interval, signal));
        }
        if !self.kvm_shared_dequeue_timers || signal != nix::sys::signal::Signal::SIGALRM {
            return Err(TimerFailure::Unsupported);
        }
        let (old, next) = self
            .real_timers
            .replace(pid, tid, now, duration, interval)?;
        self.blocked.timed_waiters.remove_kvm_real_deadline(pid);
        if let Some(deadline) = next {
            self.blocked
                .timed_waiters
                .insert_kvm_real_deadline(deadline, pid);
        }
        Ok((old.remaining, old.interval))
    }

    pub(crate) fn itimer_snapshot(
        &self,
        pid: DetPid,
        now: LogicalTime,
    ) -> Result<super::real_timer::ItimerSnapshot, TimerFailure> {
        if self.backend_is_kvm {
            return self.real_timers.snapshot(pid, now);
        }
        Ok(match self.blocked.timed_waiters.alarm_state(pid) {
            Some((_, interval)) => super::real_timer::ItimerSnapshot {
                remaining: self.alarm_remaining(pid, now),
                interval,
            },
            None => super::real_timer::ItimerSnapshot::default(),
        })
    }

    pub(super) fn control_waiter(&self) -> Ivar<()> {
        self.parked.wake.clone()
    }

    pub(super) fn control_barrier(&self) -> bool {
        self.parked.requests.values().any(|r| {
            matches!(
                r.phase,
                ContinuationPhase::AwaitingPublication(_)
                    | ContinuationPhase::AwaitingResumeRegistration(_)
            )
        })
    }

    /// Both timed pop sites dispatch before any host-capable signal operation.
    pub(super) fn dispatch_timed_signal(
        &mut self,
        deadline: LogicalTime,
        id: SignalTimerId,
        tid: DetTid,
        signal: nix::sys::signal::Signal,
        normal_due: bool,
    ) {
        if self.backend_is_kvm {
            match id {
                SignalTimerId::ChildExit { parent, .. } => {
                    // Keep existing logical SIGCHLD scheduling. No virtual PID
                    // may be passed to the host's kill or pidfd signal APIs.
                    self.blocked.sigchld_ready.insert(parent);
                    if self.blocked.sigchld_deferred.remove(&parent) {
                        self.run_queue.push_eager_io_repoll(parent);
                    }
                }
                SignalTimerId::Alarm(pid) => {
                    if let Err(failure) = self.begin_alarm_publication(pid, tid, deadline) {
                        self.fail_parked(tid, failure);
                    }
                }
                SignalTimerId::Posix(..) => self.fail_parked(tid, ProtocolFailure::Unsupported),
            }
        } else if normal_due && matches!(id, SignalTimerId::ChildExit { .. }) {
            let parent = id.process();
            self.blocked.sigchld_ready.insert(parent);
            if self.blocked.sigchld_deferred.remove(&parent) {
                self.run_queue.push_eager_io_repoll(parent);
            } else {
                self.fire_alarm(parent, tid, signal);
            }
        } else {
            self.fire_alarm(id.process(), tid, signal);
        }
    }

    fn validate_membership(&self, tid: DetTid) -> Result<(), ProtocolFailure> {
        let timed = self.blocked.timed_waiters.thread_deadline(tid).is_some();
        let queued = self.run_queue.contains_tid(tid);
        match (timed, queued) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(ProtocolFailure::Phase),
            (false, false) => Err(ProtocolFailure::Unsupported),
        }
    }

    /// The daemon validates membership before changing the timer phase. It
    /// holds exclusive scheduler access through this infallible removal.
    fn take_validated_membership(&mut self, tid: DetTid) -> SavedMembership {
        if let Some(deadline) = self.blocked.timed_waiters.thread_deadline(tid) {
            self.blocked.timed_waiters.remove(tid);
            return SavedMembership::Timed(deadline);
        }
        let priority = self.get_priority(tid);
        self.run_queue
            .suspend(tid, priority)
            .map(SavedMembership::Queued)
            .expect("validated scheduler membership")
    }

    fn begin_alarm_publication(
        &mut self,
        pid: DetPid,
        tid: DetTid,
        deadline: LogicalTime,
    ) -> Result<(), ProtocolFailure> {
        if !self.kvm_shared_dequeue_timers || self.next_turns.len() != 1 || pid != tid {
            return Err(ProtocolFailure::Unsupported);
        }
        self.real_timers.validate_sole_leader(pid)?;
        let turn = self
            .next_turns
            .get(&tid)
            .cloned()
            .ok_or(ProtocolFailure::Identity)?;
        let origin = turn.protocol.origin.ok_or(ProtocolFailure::Unsupported)?;
        let site = origin.control.site().ok_or(ProtocolFailure::Unsupported)?;
        if !matches!(
            origin.rpc,
            RpcOrigin::DirectRequestResources | RpcOrigin::ResumeParkedRequest { .. }
        ) {
            return Err(ProtocolFailure::UnexpectedControl);
        }
        self.real_timers.validate_site(pid, tid, origin.mm, site)?;
        let resources = turn
            .req
            .try_read()
            .ok_or(ProtocolFailure::Phase)?
            .map_err(|_| ProtocolFailure::Phase)?;
        if turn.resp.try_read().is_some() {
            return Err(ProtocolFailure::Phase);
        }
        if let ControlCapability::ParkedWait { policy, .. } = origin.control {
            let expected = match policy {
                ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline } => {
                    absolute_deadline
                }
                ParkedWaitPolicy::PauseNoHandlerRestart => LogicalTime::INDEFINITE,
            };
            if resources.resources.len() != 1
                || !resources
                    .resources
                    .contains_key(&ResourceID::SleepUntil(expected))
            {
                return Err(ProtocolFailure::Unsupported);
            }
        }
        if matches!(origin.control, ControlCapability::CapturedWrite { .. })
            && (resources.resources.len() != 1
                || !resources.resources.iter().all(|(resource, permission)| {
                    matches!(
                        resource,
                        ResourceID::Device(
                            crate::resources::Device::ContainerStdout
                                | crate::resources::Device::ContainerStderr
                        )
                    ) && *permission == crate::resources::Permission::W
                })
                || self.blocked.timed_waiters.thread_deadline(tid).is_some())
        {
            return Err(ProtocolFailure::Unsupported);
        }
        if matches!(origin.control, ControlCapability::CapturedWrite { .. })
            && !matches!(
                turn.protocol.owner,
                NextTurnOwner::Ordinary | NextTurnOwner::ReturningCaught { .. }
            )
        {
            return Err(ProtocolFailure::Identity);
        }
        let existing = self
            .parked
            .requests
            .iter()
            .find_map(|(id, r)| (r.request == turn.req).then_some(*id));
        let continuation = match existing {
            Some(id) => {
                let r = &self.parked.requests[&id];
                if !matches!(r.phase, ContinuationPhase::Waiting)
                    || r.response != turn.resp
                    || r.origin != origin
                    || (matches!(origin.control, ControlCapability::CapturedWrite { .. })
                        && r.owner != turn.protocol.owner)
                {
                    return Err(ProtocolFailure::Identity);
                }
                id
            }
            None => ContinuationId {
                dettid: tid,
                nonce: self
                    .parked
                    .nonce
                    .checked_add(1)
                    .ok_or(ProtocolFailure::Overflow)?,
            },
        };
        // Publication consumes the old response. Reserve enough transport
        // space to acknowledge it and register the next response before any
        // timer or queue ownership changes.
        turn.protocol
            .epoch
            .checked_add(2)
            .ok_or(ProtocolFailure::Overflow)?;
        existing
            .map(|id| self.parked.requests[&id].cycle)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ProtocolFailure::Overflow)?;
        self.parked
            .nonce
            .checked_add(if existing.is_some() { 1 } else { 2 })
            .ok_or(ProtocolFailure::Overflow)?;
        let mut siginfo = [0; 128];
        siginfo[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
        siginfo[8..12].copy_from_slice(&libc::SI_KERNEL.to_ne_bytes());
        let event = SignalEvent::new(
            libc::SIGALRM,
            siginfo,
            SignalTarget::Process {
                pid: site.process.tgid,
            },
        )
        .map_err(|_| ProtocolFailure::Identity)?;
        self.validate_membership(tid)?;
        let expiry = self.real_timers.expire(pid, deadline)?;
        if existing.is_none() {
            self.parked.nonce = continuation.nonce;
        }
        let membership = self.take_validated_membership(tid);
        let control = AlarmControl {
            expiry,
            continuation,
            request: RequestKey {
                dettid: tid,
                mm: origin.mm,
                epoch: turn.protocol.epoch,
            },
            site,
            event,
        };
        let cycle = existing
            .map(|id| self.parked.requests[&id].cycle)
            .unwrap_or(0);
        self.parked.requests.insert(
            continuation,
            OwnedRequest {
                id: continuation,
                request: turn.req,
                response: turn.resp.clone(),
                resources,
                origin,
                owner: turn.protocol.owner,
                membership: Some(membership),
                cycle,
                phase: ContinuationPhase::AwaitingPublication(control),
            },
        );
        turn.resp
            .put(SchedResponse::PublishAlarm(Box::new(control)));
        Ok(())
    }

    fn prepare_resume_ticket(
        &self,
        owned: &OwnedRequest,
        site: CallbackSignalSite,
    ) -> Result<ResumeTicket, ProtocolFailure> {
        let cycle = owned
            .cycle
            .checked_add(1)
            .ok_or(ProtocolFailure::Overflow)?;
        let epoch = self
            .next_turns
            .get(&owned.id.dettid)
            .ok_or(ProtocolFailure::Identity)?
            .protocol
            .epoch;
        Ok(ResumeTicket {
            continuation: owned.id,
            cycle,
            next_epoch: epoch.checked_add(1).ok_or(ProtocolFailure::Overflow)?,
            site,
            nonce: self
                .parked
                .nonce
                .checked_add(1)
                .ok_or(ProtocolFailure::Overflow)?,
        })
    }

    fn install_resume_ticket(&mut self, owned: &mut OwnedRequest, ticket: ResumeTicket) {
        self.parked.nonce = ticket.nonce;
        owned.cycle = ticket.cycle;
        owned.phase = ContinuationPhase::AwaitingResumeRegistration(ticket);
    }

    fn resume_ticket(
        &mut self,
        owned: &mut OwnedRequest,
        site: CallbackSignalSite,
    ) -> Result<ResumeTicket, ProtocolFailure> {
        let ticket = self.prepare_resume_ticket(owned, site)?;
        self.install_resume_ticket(owned, ticket);
        Ok(ticket)
    }

    fn accept_publication(
        &mut self,
        control: AlarmControl,
        outcome: ProcessAlarmSignalOutcome,
    ) -> Result<PublicationActivation, ProtocolFailure> {
        let mut owned = self
            .parked
            .requests
            .remove(&control.continuation)
            .ok_or(ProtocolFailure::Identity)?;
        let result = (|| {
            let turn = self
                .next_turns
                .get(&owned.id.dettid)
                .ok_or(ProtocolFailure::Identity)?;
            if !matches!(owned.phase, ContinuationPhase::AwaitingPublication(expected) if expected == control)
                || turn.req != owned.request
                || turn.resp != owned.response
                || turn.protocol.epoch != control.request.epoch
                || turn.protocol.origin != Some(owned.origin)
            {
                return Err(ProtocolFailure::Identity);
            }
            let ProcessAlarmSignalOutcome::Accepted(receipt) = outcome else {
                self.real_timers.publication(control.expiry, outcome)?;
                return Err(ProtocolFailure::Timer(TimerFailure::Publication(outcome)));
            };
            if receipt.blocked
                || matches!(
                    owned.origin.control,
                    ControlCapability::PublishOnly { .. } | ControlCapability::CapturedWrite { .. }
                )
            {
                let ticket = self.prepare_resume_ticket(&owned, control.site)?;
                self.real_timers.publication(control.expiry, outcome)?;
                self.install_resume_ticket(&mut owned, ticket);
                return Ok(PublicationActivation::AwaitResume(ticket));
            }
            let lease = ParkedObservationLease {
                nonce: self
                    .parked
                    .nonce
                    .checked_add(1)
                    .ok_or(ProtocolFailure::Overflow)?,
            };
            let next_epoch = turn
                .protocol
                .epoch
                .checked_add(1)
                .ok_or(ProtocolFailure::Overflow)?;
            self.real_timers.publication(control.expiry, outcome)?;
            self.parked.nonce = lease.nonce;
            let turn = self
                .next_turns
                .get_mut(&owned.id.dettid)
                .expect("validated publication turn");
            turn.protocol.epoch = next_epoch;
            turn.protocol.owner = NextTurnOwner::Observation {
                wait: owned.id,
                lease,
            };
            turn.protocol.origin = None;
            turn.req = Ivar::new();
            turn.resp = Ivar::new();
            self.runqueue_push_back(owned.id.dettid);
            owned.phase = ContinuationPhase::Observing(lease);
            Ok(PublicationActivation::Observe {
                wait: owned.id,
                lease,
            })
        })();
        self.parked.requests.insert(owned.id, owned);
        result
    }

    fn finish_observation(
        &mut self,
        wait: ContinuationId,
        lease: ParkedObservationLease,
        site: CallbackSignalSite,
        finish: ObservationFinish,
    ) -> Result<FinishAck, ProtocolFailure> {
        let mut owned = self
            .parked
            .requests
            .remove(&wait)
            .ok_or(ProtocolFailure::Identity)?;
        let result = (|| {
            if !matches!(owned.phase, ContinuationPhase::Observing(expected) if expected == lease)
                || owned.origin.control.site() != Some(site)
            {
                return Err(ProtocolFailure::Identity);
            }
            let turn = self
                .next_turns
                .get(&wait.dettid)
                .ok_or(ProtocolFailure::Identity)?;
            if turn.protocol.owner != (NextTurnOwner::Observation { wait, lease })
                || turn.req.try_read().is_some()
                || turn.resp.try_read().is_some()
                || !self.run_queue.contains_tid(wait.dettid)
            {
                return Err(ProtocolFailure::Phase);
            }
            match finish {
                ObservationFinish::ResumeSameWait => self
                    .resume_ticket(&mut owned, site)
                    .map(FinishAck::AwaitResume),
                ObservationFinish::InterruptForCaught { selection } => {
                    if selection.site != site {
                        return Err(ProtocolFailure::Identity);
                    }
                    let turn = self
                        .next_turns
                        .get_mut(&wait.dettid)
                        .ok_or(ProtocolFailure::Identity)?;
                    turn.protocol.epoch = turn
                        .protocol
                        .epoch
                        .checked_add(1)
                        .ok_or(ProtocolFailure::Overflow)?;
                    turn.protocol.owner = NextTurnOwner::ReturningCaught {
                        completed_wait: wait,
                    };
                    turn.protocol.origin = None;
                    // This empty, queued gate protects the real remaining-time
                    // copyout, posthook and frame delivery. No synthetic turn.
                    Ok(FinishAck::Interrupted)
                }
                ObservationFinish::Terminate { selection } => {
                    if selection.site != site {
                        return Err(ProtocolFailure::Identity);
                    }
                    // Keep the existing execution gate until the driver's real
                    // signal exit performs ordinary logical retirement.
                    Ok(FinishAck::Terminate)
                }
            }
        })();
        if matches!(result, Ok(FinishAck::AwaitResume(_))) || result.is_err() {
            self.parked.requests.insert(wait, owned);
        }
        result
    }

    fn resume_request(
        &mut self,
        ticket: ResumeTicket,
        site: CallbackSignalSite,
        response: Ivar<SchedResponse>,
    ) -> Result<(), ProtocolFailure> {
        let mut owned = self
            .parked
            .requests
            .remove(&ticket.continuation)
            .ok_or(ProtocolFailure::Identity)?;
        let result = (|| {
            if !matches!(owned.phase, ContinuationPhase::AwaitingResumeRegistration(expected) if expected == ticket)
                || ticket.site != site
                || response.try_read().is_some()
            {
                return Err(ProtocolFailure::Identity);
            }
            self.real_timers.validate_site(
                *self
                    .thread_tree
                    .thread_to_leader
                    .get(&owned.id.dettid)
                    .ok_or(ProtocolFailure::Identity)?,
                owned.id.dettid,
                owned.origin.mm,
                site,
            )?;
            let tid = owned.id.dettid;
            let before = self.next_turns.get(&tid).ok_or(ProtocolFailure::Identity)?;
            if before.protocol.epoch.checked_add(1) != Some(ticket.next_epoch) {
                return Err(ProtocolFailure::Identity);
            }
            let owner = match owned.origin.control {
                ControlCapability::CapturedWrite { .. } => {
                    if before.protocol.owner != owned.owner
                        || !matches!(
                            owned.owner,
                            NextTurnOwner::Ordinary | NextTurnOwner::ReturningCaught { .. }
                        )
                        || before.req != owned.request
                        || before.resp != owned.response
                        || before.protocol.origin != Some(owned.origin)
                    {
                        return Err(ProtocolFailure::Identity);
                    }
                    // This scheduler gate can outlive the caught callback and
                    // frame handoff until the next real grant. The backend's
                    // separate query still requires a fresh original Write in
                    // its Ordinary injection guard, with no prepared selection.
                    owned.owner
                }
                ControlCapability::PublishOnly { lease, .. } => match before.protocol.owner {
                    NextTurnOwner::Observation {
                        wait,
                        lease: expected,
                    } if expected == lease => NextTurnOwner::Observation { wait, lease },
                    _ => return Err(ProtocolFailure::Identity),
                },
                _ => NextTurnOwner::Ordinary,
            };
            let membership = owned.membership.take().ok_or(ProtocolFailure::Phase)?;
            match membership {
                SavedMembership::Timed(deadline) => {
                    self.run_queue.remove_tid(tid);
                    self.blocked.timed_waiters.insert(deadline, tid);
                }
                SavedMembership::Queued(entry) => {
                    let priority = self.get_priority(tid);
                    self.run_queue.suspend(tid, priority);
                    self.run_queue.restore(entry, priority);
                }
            }
            owned.origin.rpc = RpcOrigin::ResumeParkedRequest {
                continuation: owned.id,
                cycle: ticket.cycle,
            };
            let turn = self
                .next_turns
                .get_mut(&tid)
                .ok_or(ProtocolFailure::Identity)?;
            turn.protocol.epoch = ticket.next_epoch;
            turn.protocol.origin = Some(owned.origin);
            turn.protocol.owner = owner;
            turn.req = owned.request.clone();
            turn.resp = response.clone();
            owned.response = response;
            owned.phase = ContinuationPhase::Waiting;
            Ok(())
        })();
        self.parked.requests.insert(owned.id, owned);
        result
    }

    /// Called only by the daemon before tentative selection. An RPC may wake
    /// this boundary but never changes runqueue membership itself.
    pub(super) fn drain_control_intents(&mut self) {
        while let Some(intent) = self.parked.intents.pop_front() {
            let (tid, error) = match intent {
                ControlIntent::Publication {
                    control,
                    outcome,
                    ack,
                } => {
                    let result = self.accept_publication(control, outcome);
                    let error = result.as_ref().err().copied();
                    ack.put(result);
                    (control.continuation.dettid, error)
                }
                ControlIntent::Finish {
                    wait,
                    lease,
                    site,
                    finish,
                    ack,
                } => {
                    let result = self.finish_observation(wait, lease, site, finish);
                    let error = result.as_ref().err().copied();
                    ack.put(result);
                    (wait.dettid, error)
                }
                ControlIntent::Resume {
                    ticket,
                    site,
                    response,
                } => (
                    ticket.continuation.dettid,
                    self.resume_request(ticket, site, response).err(),
                ),
            };
            if let Some(error) = error {
                self.fail_parked(tid, error);
                break;
            }
        }
        if self.parked.wake.try_read().is_some() {
            self.parked.wake = Ivar::new();
        }
    }

    pub(super) fn retire_parked_requests(&mut self, tid: DetTid) {
        if self.parked.requests.keys().any(|id| id.dettid == tid) {
            self.fail_parked(tid, ProtocolFailure::Phase);
            self.parked.requests.retain(|id, _| id.dettid != tid);
        }
        self.parked.wake.try_put(());
    }

    pub(super) fn settle_parked_grant(&mut self, tid: DetTid) {
        let Some(turn) = self.next_turns.get(&tid) else {
            return;
        };
        let req = turn.req.clone();
        self.parked.requests.retain(|_, owned| owned.request != req);
    }

    /// Normal polling replaces a request without completing its logical
    /// operation. Match the exact request, so a hook on the same task cannot
    /// change the suspended outer wait's ownership or resources.
    pub(super) fn rebind_parked_request(
        &mut self,
        previous: &Ivar<SchedRequest>,
        replacement: &Ivar<SchedRequest>,
        resources: &Resources,
    ) {
        for owned in self.parked.requests.values_mut() {
            if owned.request == *previous {
                owned.request = replacement.clone();
                owned.resources = resources.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reverie::PendingDomain;
    use reverie::ProcessAlarmSignalDisposition;
    use reverie::ProcessAlarmSignalReceipt;
    use reverie::SignalConsumer;
    use reverie::SignalDequeue;
    use reverie::SignalProcessId;
    use reverie::SignalTaskIdentity;

    use super::*;
    use crate::config::Config;
    use crate::resources::Permission;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::ThreadNextTurn;

    fn time(n: u64) -> LogicalTime {
        LogicalTime::from_nanos(n)
    }
    fn fixture() -> (Scheduler, DetTid, MmId, CallbackSignalSite) {
        let tid = DetTid::from_raw(100);
        let mm = MmId::initial(tid);
        let identity = SignalTaskIdentity {
            process: SignalProcessId {
                tgid: reverie::Pid::from_raw(100),
                generation: 1,
            },
            tid: reverie::Pid::from_raw(100),
            task_generation: 1,
        };
        let site = CallbackSignalSite {
            process: identity.process,
            tid: identity.tid,
            task_generation: 1,
            callback_nonce: 1,
            boundary_nonce: 1,
        };
        let mut s = Scheduler::new(&Config {
            backend_is_kvm: true,
            kvm_shared_dequeue_timers: true,
            ..Config::default()
        });
        s.thread_tree.add_child(tid, tid, true);
        s.priorities.insert(tid, DEFAULT_PRIORITY);
        s.next_turns.insert(
            tid,
            ThreadNextTurn {
                dettid: tid,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        s.real_timers.bind(tid, tid, mm, identity).unwrap();
        (s, tid, mm, site)
    }
    fn receipt(blocked: bool) -> ProcessAlarmSignalOutcome {
        ProcessAlarmSignalOutcome::Accepted(ProcessAlarmSignalReceipt {
            blocked,
            disposition: ProcessAlarmSignalDisposition::Ignored,
            pending_generation: 1,
            coalesced: false,
        })
    }
    fn dequeue(site: CallbackSignalSite, sequence: u64, domain: PendingDomain) -> SignalDequeue {
        let mut info = [0; 128];
        info[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
        let target = match domain {
            PendingDomain::Process => SignalTarget::Process {
                pid: site.process.tgid,
            },
            PendingDomain::Thread => SignalTarget::Thread {
                pid: site.process.tgid,
                tid: site.tid,
            },
        };
        SignalDequeue {
            process: site.process,
            sequence,
            consumer: SignalConsumer::SignalTimedWait,
            domain,
            event: SignalEvent::new(libc::SIGALRM, info, target).unwrap(),
        }
    }
    fn park(
        s: &mut Scheduler,
        tid: DetTid,
        mm: MmId,
        site: CallbackSignalSite,
        deadline: LogicalTime,
    ) -> Ivar<SchedRequest> {
        let mut resources = Resources::new(tid);
        resources.insert(ResourceID::SleepUntil(deadline), Permission::RW);
        s.install_resource_origin(
            tid,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm,
                control: ControlCapability::ParkedWait {
                    policy: ParkedWaitPolicy::NanosleepNoHandlerRestart {
                        absolute_deadline: deadline,
                    },
                    site,
                },
            },
        )
        .unwrap();
        let request = s.next_turns[&tid].req.clone();
        request.put(Ok(resources));
        s.blocked.timed_waiters.insert(deadline, tid);
        request
    }
    fn pop(s: &mut Scheduler, deadline: u64) -> AlarmControl {
        s.committed_time = time(deadline);
        s.step2b_process_timed();
        assert!(!s.backend_failed(), "{:?}", s.parked.failure);
        let turn = s.next_turns.values().next().unwrap();
        match turn.resp.try_read().unwrap() {
            SchedResponse::PublishAlarm(control) => *control,
            other => panic!("{other:?}"),
        }
    }

    fn queue_captured_write(
        s: &mut Scheduler,
        tid: DetTid,
        mm: MmId,
        site: CallbackSignalSite,
    ) -> Resources {
        let mut resources = Resources::new(tid);
        resources.insert(
            ResourceID::Device(crate::resources::Device::ContainerStdout),
            Permission::W,
        );
        // Exercise preservation of the complete queue value and request,
        // even though a first captured write normally has poll_attempt == 0.
        resources.poll_attempt = 3;
        resources.fyi("captured write preservation control");
        s.install_resource_origin(
            tid,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm,
                control: ControlCapability::CapturedWrite { site },
            },
        )
        .unwrap();
        s.next_turns[&tid].req.put(Ok(resources.clone()));
        s.run_queue
            .push_poller(tid, DEFAULT_PRIORITY, resources.poll_attempt);
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(10),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        resources
    }

    #[test]
    fn captured_write_publication_restores_request_and_queue_for_both_masks() {
        for blocked in [false, true] {
            let (mut s, tid, mm, site) = fixture();
            let resources = queue_captured_write(&mut s, tid, mm, site);
            let before_queue = format!("{:?}", s.run_queue);
            let request = s.next_turns[&tid].req.clone();
            let old_response = s.next_turns[&tid].resp.clone();
            let before_origin = s.next_turns[&tid].protocol.origin.unwrap();
            let control = pop(&mut s, 10);
            assert_eq!(s.turn, 0);
            assert!(!s.run_queue.contains_tid(tid));
            assert_eq!(request.try_read().unwrap().unwrap(), resources);
            let PublicationActivation::AwaitResume(ticket) =
                acknowledge(&mut s, mm, control, blocked)
            else {
                panic!("captured writes must never observe before returning");
            };
            assert_eq!(s.turn, 0);
            assert_eq!(s.committed_time, time(10));
            let response = Ivar::new();
            assert_ne!(response, old_response);
            s.resume_request(ticket, site, response.clone()).unwrap();
            assert_eq!(s.committed_time, time(10));
            assert_eq!(s.next_turns[&tid].req, request);
            assert_eq!(request.try_read().unwrap().unwrap(), resources);
            assert_eq!(format!("{:?}", s.run_queue), before_queue);
            assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
            let restored_origin = s.next_turns[&tid].protocol.origin.unwrap();
            assert_eq!(restored_origin.mm, before_origin.mm);
            assert_eq!(restored_origin.control, before_origin.control);
            assert_eq!(
                restored_origin.rpc,
                RpcOrigin::ResumeParkedRequest {
                    continuation: ticket.continuation,
                    cycle: ticket.cycle,
                }
            );
            assert!(response.try_read().is_none());
            let (selected, selected_request, selected_response) = s.step3_peek().unwrap();
            assert_eq!(selected_request, request);
            assert_eq!(selected_response, response);
            // Restoring a poll request does not bypass its ordinary noncommitting
            // turn. The scheduler replaces only its poll count before granting it.
            let before_skip_time = s.committed_time;
            let before_grant_epoch = s.next_turns[&tid].protocol.epoch;
            assert!(matches!(
                s.step4_resource_block(selected, &resources, &response),
                Err(crate::scheduler::SkipTurn)
            ));
            assert_eq!(s.turn, 1);
            assert_eq!(s.committed_time, before_skip_time);
            assert_eq!(s.next_turns[&tid].protocol.epoch, before_grant_epoch);
            assert!(response.try_read().is_none());
            assert_eq!(request.try_read().unwrap().unwrap(), resources);
            let mut runnable_resources = resources.clone();
            runnable_resources.poll_attempt = 0;
            let (selected, runnable_request, selected_response) = s.step3_peek().unwrap();
            assert_eq!(selected, tid);
            assert_ne!(runnable_request, request);
            assert_eq!(
                runnable_request.try_read().unwrap().unwrap(),
                runnable_resources
            );
            let owned = &s.parked.requests[&ticket.continuation];
            assert_eq!(owned.request, runnable_request);
            assert_eq!(owned.resources, runnable_resources);
            assert_eq!(owned.response, response);
            assert_eq!(owned.cycle, ticket.cycle);
            assert_eq!(owned.origin, restored_origin);
            assert!(matches!(owned.phase, ContinuationPhase::Waiting));
            assert_eq!(selected_response, response);
            s.step4_resource_block(selected, &runnable_resources, &response)
                .unwrap();
            s.step5_guest_unblock(selected, &runnable_resources, &response)
                .unwrap();
            s.step6_reenquue(selected, false);
            assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
            assert!(matches!(
                old_response.try_read(),
                Some(SchedResponse::PublishAlarm(_))
            ));
            assert_eq!(s.turn, 2);
            assert_eq!(s.next_turns[&tid].protocol.epoch, before_grant_epoch + 1);
            assert!(s.parked.requests.is_empty());
            assert_eq!(s.host_signal_attempts, 0);
        }
    }

    #[test]
    fn hook_poll_replacement_preserves_the_suspended_wait() {
        let (mut s, tid, mm, site) = fixture();
        park(&mut s, tid, mm, site, time(100));
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(0),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        let control = pop(&mut s, 10);
        let PublicationActivation::Observe { wait, lease } =
            acknowledge(&mut s, mm, control, false)
        else {
            panic!("unblocked wait must enter observation");
        };
        let before_owned = format!("{:?}", s.parked.requests[&wait]);
        let before_turn = s.turn;
        let before_time = s.committed_time;
        s.install_resource_origin(
            tid,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm,
                control: ControlCapability::PublishOnly { lease, site },
            },
        )
        .unwrap();
        let mut hook_resources = Resources::new(tid);
        hook_resources.insert(ResourceID::InternalIOPolling, Permission::W);
        hook_resources.poll_attempt = 3;
        hook_resources.fyi("hook poll replacement control");
        let old_hook = s.next_turns[&tid].req.clone();
        old_hook.put(Ok(hook_resources.clone()));
        s.upgrade_polled_to_runnable(tid, &hook_resources);
        assert_ne!(s.next_turns[&tid].req, old_hook);
        assert_eq!(old_hook.try_read().unwrap().unwrap(), hook_resources);
        hook_resources.poll_attempt = 0;
        assert_eq!(
            s.next_turns[&tid].req.try_read().unwrap().unwrap(),
            hook_resources
        );
        assert_eq!(format!("{:?}", s.parked.requests[&wait]), before_owned);
        assert_eq!(s.turn, before_turn);
        assert_eq!(s.committed_time, before_time);
        assert!(s.next_turns[&tid].resp.try_read().is_none());
    }

    #[test]
    fn captured_write_stale_expiry_preserves_queue_request_and_nonce() {
        let (mut s, tid, mm, site) = fixture();
        let resources = queue_captured_write(&mut s, tid, mm, site);
        let queue = format!("{:?}", s.run_queue);
        let timer = format!("{:?}", s.real_timers);
        let nonce = s.parked.nonce;
        assert_eq!(
            s.begin_alarm_publication(tid, tid, time(11)),
            Err(ProtocolFailure::Timer(TimerFailure::InvalidPhase))
        );
        assert_eq!(format!("{:?}", s.run_queue), queue);
        assert_eq!(format!("{:?}", s.real_timers), timer);
        assert_eq!(s.parked.nonce, nonce);
        assert_eq!(
            s.next_turns[&tid].req.try_read().unwrap().unwrap(),
            resources
        );
        assert!(s.next_turns[&tid].resp.try_read().is_none());
        assert!(s.parked.requests.is_empty());
    }

    #[test]
    fn captured_write_resume_refuses_changed_transport_before_restoring_queue() {
        for changed_request in [false, true] {
            let (mut s, tid, mm, site) = fixture();
            queue_captured_write(&mut s, tid, mm, site);
            let control = pop(&mut s, 10);
            let PublicationActivation::AwaitResume(ticket) =
                acknowledge(&mut s, mm, control, false)
            else {
                panic!("captured write must await resume");
            };
            let turn = s.next_turns.get_mut(&tid).unwrap();
            if changed_request {
                turn.req = Ivar::new();
            } else {
                turn.protocol.owner = NextTurnOwner::ReturningCaught {
                    completed_wait: ticket.continuation,
                };
            }
            let before = turn.clone();
            let queue = format!("{:?}", s.run_queue);
            let response = Ivar::new();
            assert_eq!(
                s.resume_request(ticket, site, response.clone()),
                Err(ProtocolFailure::Identity)
            );
            assert_eq!(s.next_turns[&tid].req, before.req);
            assert_eq!(s.next_turns[&tid].resp, before.resp);
            assert_eq!(s.next_turns[&tid].protocol.owner, before.protocol.owner);
            assert_eq!(s.next_turns[&tid].protocol.epoch, before.protocol.epoch);
            assert_eq!(format!("{:?}", s.run_queue), queue);
            assert!(response.try_read().is_none());
            assert!(s.parked.requests[&ticket.continuation].membership.is_some());
        }
    }

    #[test]
    fn captured_write_transport_overflow_refuses_before_expiry_or_queue_removal() {
        for exhaust_epoch in [false, true] {
            let (mut s, tid, mm, site) = fixture();
            let resources = queue_captured_write(&mut s, tid, mm, site);
            if exhaust_epoch {
                s.next_turns.get_mut(&tid).unwrap().protocol.epoch = u64::MAX - 1;
            } else {
                s.parked.nonce = u64::MAX - 1;
            }
            let queue = format!("{:?}", s.run_queue);
            let timer = format!("{:?}", s.real_timers);
            let nonce = s.parked.nonce;
            assert_eq!(
                s.begin_alarm_publication(tid, tid, time(10)),
                Err(ProtocolFailure::Overflow)
            );
            assert_eq!(format!("{:?}", s.run_queue), queue);
            assert_eq!(format!("{:?}", s.real_timers), timer);
            assert_eq!(s.parked.nonce, nonce);
            assert_eq!(
                s.next_turns[&tid].req.try_read().unwrap().unwrap(),
                resources
            );
            assert!(s.next_turns[&tid].resp.try_read().is_none());
            assert!(s.parked.requests.is_empty());
        }
    }

    #[test]
    fn captured_write_capability_does_not_admit_other_resources() {
        for resource in [
            ResourceID::InternalIOPolling,
            ResourceID::FutexWait,
            ResourceID::SleepUntil(time(100)),
            ResourceID::MemAddrSpace(DetTid::from_raw(17)),
        ] {
            let (mut s, tid, mm, site) = fixture();
            queue_captured_write(&mut s, tid, mm, site);
            let mut resources = Resources::new(tid);
            resources.insert(resource, Permission::W);
            let request = Ivar::new();
            request.put(Ok(resources.clone()));
            s.next_turns.get_mut(&tid).unwrap().req = request;
            let queue = format!("{:?}", s.run_queue);
            let timer = format!("{:?}", s.real_timers);
            assert_eq!(
                s.begin_alarm_publication(tid, tid, time(10)),
                Err(ProtocolFailure::Unsupported)
            );
            assert_eq!(format!("{:?}", s.run_queue), queue);
            assert_eq!(format!("{:?}", s.real_timers), timer);
            assert_eq!(
                s.next_turns[&tid].req.try_read().unwrap().unwrap(),
                resources
            );
            assert!(s.next_turns[&tid].resp.try_read().is_none());
        }
    }
    fn acknowledge(
        s: &mut Scheduler,
        mm: MmId,
        control: AlarmControl,
        blocked: bool,
    ) -> PublicationActivation {
        let ack = Ivar::new();
        s.post_control(
            control.continuation.dettid,
            mm,
            ControlIntent::Publication {
                control,
                outcome: receipt(blocked),
                ack: ack.clone(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        ack.try_read().unwrap().unwrap()
    }

    #[test]
    fn blocked_through_35_rearms_only_after_shared_dequeue() {
        let (mut s, tid, mm, site) = fixture();
        park(&mut s, tid, mm, site, time(100));
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(10),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        let control = pop(&mut s, 10);
        assert!(matches!(
            acknowledge(&mut s, mm, control, true),
            PublicationActivation::AwaitResume(_)
        ));
        assert!(s.blocked.timed_waiters.pop_if_before(time(35)).is_none());
        assert_eq!(
            s.itimer_snapshot(tid, time(35)).unwrap(),
            super::super::real_timer::ItimerSnapshot {
                remaining: time(0),
                interval: time(10)
            }
        );
        let private = dequeue(site, 1, PendingDomain::Thread);
        s.apply_signal_dequeue(tid, private, time(35)).unwrap();
        assert!(s.blocked.timed_waiters.next_deadline().is_none());
        let shared = dequeue(site, 2, PendingDomain::Process);
        let first = s.apply_signal_dequeue(tid, shared, time(35)).unwrap();
        assert_eq!(s.blocked.timed_waiters.next_deadline(), Some(time(40)));
        assert_eq!(
            s.apply_signal_dequeue(tid, shared, time(39)).unwrap(),
            first
        );
        assert_eq!(s.blocked.timed_waiters.next_deadline(), Some(time(40)));
    }

    #[test]
    fn worker_exit_keeps_process_deadline_and_shared_dequeue_rearm() {
        let (mut s, leader, mm, site) = fixture();
        park(&mut s, leader, mm, site, time(100));
        let worker = DetTid::from_raw(101);
        s.thread_tree.add_child(leader, worker, false);
        s.priorities.insert(worker, DEFAULT_PRIORITY);
        s.next_turns.insert(
            worker,
            ThreadNextTurn {
                dettid: worker,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
                protocol: Default::default(),
            },
        );
        s.real_timers
            .bind(
                leader,
                worker,
                mm,
                SignalTaskIdentity {
                    process: site.process,
                    tid: reverie::Pid::from_raw(worker.as_raw()),
                    task_generation: 2,
                },
            )
            .unwrap();
        // The worker is running while the leader waits. Its ordered exit must
        // keep the process timer without retaining the worker as its target.
        s.replace_real_timer(
            leader,
            worker,
            time(0),
            time(10),
            time(10),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        s.logically_kill_thread(&worker, &leader, mm);
        assert_eq!(s.next_turns.len(), 1);
        assert_eq!(
            s.itimer_snapshot(leader, time(0)).unwrap().remaining,
            time(10)
        );
        let control = pop(&mut s, 10);
        assert_eq!(control.request.dettid, leader);
        assert_eq!(control.site, site);
        let PublicationActivation::AwaitResume(ticket) = acknowledge(&mut s, mm, control, true)
        else {
            panic!("blocked publication retains the leader's wait");
        };
        s.post_control(
            leader,
            mm,
            ControlIntent::Resume {
                ticket,
                site,
                response: Ivar::new(),
            },
        )
        .unwrap();
        s.drain_control_intents();
        s.apply_signal_dequeue(leader, dequeue(site, 1, PendingDomain::Process), time(35))
            .unwrap();
        assert_eq!(s.blocked.timed_waiters.next_deadline(), Some(time(40)));
        let control = pop(&mut s, 40);
        assert_eq!(control.request.dettid, leader);
        assert_eq!(control.site, site);
        assert_eq!(
            s.turn, 0,
            "publication and acknowledgment grant no resource turn"
        );
    }

    #[test]
    fn publication_requires_live_sole_leader_before_removing_wait() {
        for (add_worker, retire_leader) in [(true, false), (false, true), (true, true)] {
            let (mut s, leader, mm, site) = fixture();
            park(&mut s, leader, mm, site, time(100));
            s.replace_real_timer(
                leader,
                leader,
                time(0),
                time(10),
                time(10),
                nix::sys::signal::Signal::SIGALRM,
            )
            .unwrap();
            if add_worker {
                let worker = DetTid::from_raw(101);
                s.real_timers
                    .bind(
                        leader,
                        worker,
                        mm,
                        SignalTaskIdentity {
                            process: site.process,
                            tid: reverie::Pid::from_raw(worker.as_raw()),
                            task_generation: 2,
                        },
                    )
                    .unwrap();
            }
            if retire_leader {
                s.real_timers.retire_task(leader, leader);
            }
            // An available response cell is not authority to omit another
            // admitted member or to revive a retired leader's callback.
            let before = s.itimer_snapshot(leader, time(0)).unwrap();
            assert_eq!(
                s.begin_alarm_publication(leader, leader, time(10)),
                Err(ProtocolFailure::Timer(if add_worker {
                    TimerFailure::Unsupported
                } else {
                    TimerFailure::Identity
                }))
            );
            assert_eq!(s.itimer_snapshot(leader, time(0)).unwrap(), before);
            assert_eq!(
                s.blocked.timed_waiters.thread_deadline(leader),
                Some(time(100))
            );
            assert!(s.next_turns[&leader].resp.try_read().is_none());
            assert!(s.parked.requests.is_empty());
        }
    }

    #[test]
    fn two_observations_restore_one_original_wait_with_fresh_responses() {
        let (mut s, tid, mm, site) = fixture();
        let original = park(&mut s, tid, mm, site, time(100));
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(10),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        let mut previous_response = s.next_turns[&tid].resp.clone();
        for cycle in 1..=2 {
            let control = pop(&mut s, cycle * 10);
            let PublicationActivation::Observe { wait, lease } =
                acknowledge(&mut s, mm, control, false)
            else {
                panic!("unblocked must observe");
            };
            assert_eq!(s.run_queue.len(), 1);
            assert!(s.next_turns[&tid].req.try_read().is_none());
            s.apply_signal_dequeue(
                tid,
                dequeue(site, cycle, PendingDomain::Process),
                time(cycle * 10),
            )
            .unwrap();
            let ack = Ivar::new();
            s.post_control(
                tid,
                mm,
                ControlIntent::Finish {
                    wait,
                    lease,
                    site,
                    finish: ObservationFinish::ResumeSameWait,
                    ack: ack.clone(),
                },
            )
            .unwrap();
            s.drain_control_intents();
            let FinishAck::AwaitResume(ticket) = ack.try_read().unwrap().unwrap() else {
                panic!("resumption required");
            };
            assert!(s.control_barrier());
            assert_eq!(
                s.run_queue.len(),
                1,
                "execution gate retained until actual registration"
            );
            let response = Ivar::new();
            assert_ne!(response, previous_response);
            s.post_control(
                tid,
                mm,
                ControlIntent::Resume {
                    ticket,
                    site,
                    response: response.clone(),
                },
            )
            .unwrap();
            s.drain_control_intents();
            assert!(!s.control_barrier());
            assert_eq!(s.next_turns[&tid].req, original);
            assert_eq!(s.next_turns[&tid].resp, response);
            assert_eq!(
                s.blocked.timed_waiters.thread_deadline(tid),
                Some(time(100))
            );
            assert_eq!(
                s.next_turns[&tid].protocol.origin.unwrap().rpc,
                RpcOrigin::ResumeParkedRequest {
                    continuation: wait,
                    cycle
                }
            );
            assert!(response.try_read().is_none());
            previous_response = response;
        }
        assert_eq!(s.turn, 0, "control traffic is not a resource COMMIT");
    }

    #[test]
    fn caught_handoff_keeps_the_actual_empty_gate_without_settling_sleep_normally() {
        let (mut s, tid, mm, site) = fixture();
        park(&mut s, tid, mm, site, time(100));
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(0),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        let control = pop(&mut s, 10);
        let PublicationActivation::Observe { wait, lease } =
            acknowledge(&mut s, mm, control, false)
        else {
            panic!();
        };
        let req = s.next_turns[&tid].req.clone();
        let resp = s.next_turns[&tid].resp.clone();
        let queue = format!("{:?}", s.run_queue);
        let ack = s
            .finish_observation(
                wait,
                lease,
                site,
                ObservationFinish::InterruptForCaught {
                    selection: PreparedSignalToken {
                        site,
                        selection_nonce: 1,
                    },
                },
            )
            .unwrap();
        assert_eq!(ack, FinishAck::Interrupted);
        assert_eq!(s.next_turns[&tid].req, req);
        assert_eq!(s.next_turns[&tid].resp, resp);
        assert_eq!(format!("{:?}", s.run_queue), queue);
        assert!(req.try_read().is_none());
        assert!(resp.try_read().is_none());
        assert!(s.are_all_quiesced().is_some());
        assert!(s.blocked.timed_waiters.thread_deadline(tid).is_none());
        assert_eq!(s.turn, 0);
    }

    #[test]
    fn captured_write_after_caught_handoff_keeps_the_gate_until_real_grant() {
        for blocked in [false, true] {
            let (mut s, tid, mm, site) = fixture();
            park(&mut s, tid, mm, site, time(100));
            s.replace_real_timer(
                tid,
                tid,
                time(0),
                time(10),
                time(0),
                nix::sys::signal::Signal::SIGALRM,
            )
            .unwrap();
            let control = pop(&mut s, 10);
            let PublicationActivation::Observe { wait, lease } = s
                .accept_publication(
                    control,
                    ProcessAlarmSignalOutcome::Accepted(ProcessAlarmSignalReceipt {
                        disposition: ProcessAlarmSignalDisposition::Caught,
                        blocked: false,
                        pending_generation: 1,
                        coalesced: false,
                    }),
                )
                .unwrap()
            else {
                panic!("unblocked caught alarm must enter observation");
            };
            let mut event = dequeue(site, 1, PendingDomain::Process);
            event.consumer = SignalConsumer::ReturnToUser;
            s.apply_signal_dequeue(tid, event, time(10)).unwrap();
            assert_eq!(
                s.finish_observation(
                    wait,
                    lease,
                    site,
                    ObservationFinish::InterruptForCaught {
                        selection: PreparedSignalToken {
                            site,
                            selection_nonce: 1,
                        },
                    },
                )
                .unwrap(),
                FinishAck::Interrupted
            );
            let owner = NextTurnOwner::ReturningCaught {
                completed_wait: wait,
            };
            assert_eq!(s.next_turns[&tid].protocol.owner, owner);
            // Model the next original syscall callback, after the backend has
            // consumed its prepared selection. Actual frame/callback admission
            // is covered by the unchanged guest and the backend controls.
            let write_site = CallbackSignalSite {
                callback_nonce: site.callback_nonce + 1,
                boundary_nonce: site.boundary_nonce + 1,
                ..site
            };
            let mut resources = Resources::new(tid);
            resources.insert(
                ResourceID::Device(crate::resources::Device::ContainerStdout),
                Permission::W,
            );
            s.install_resource_origin(
                tid,
                ResourceOrigin {
                    rpc: RpcOrigin::DirectRequestResources,
                    mm,
                    control: ControlCapability::CapturedWrite { site: write_site },
                },
            )
            .unwrap();
            let request = s.next_turns[&tid].req.clone();
            request.put(Ok(resources.clone()));
            let queue = format!("{:?}", s.run_queue);
            s.replace_real_timer(
                tid,
                tid,
                time(10),
                time(10),
                time(0),
                nix::sys::signal::Signal::SIGALRM,
            )
            .unwrap();
            let control = pop(&mut s, 20);
            let PublicationActivation::AwaitResume(ticket) =
                acknowledge(&mut s, mm, control, blocked)
            else {
                panic!("captured write must resume without observation");
            };
            assert_eq!(s.next_turns[&tid].protocol.owner, owner);
            let response = Ivar::new();
            s.resume_request(ticket, write_site, response.clone())
                .unwrap();
            assert_eq!(s.next_turns[&tid].protocol.owner, owner);
            assert_eq!(s.next_turns[&tid].req, request);
            assert_eq!(request.try_read().unwrap().unwrap(), resources);
            assert_eq!(format!("{:?}", s.run_queue), queue);
            assert_eq!(s.turn, 0);
            assert_eq!(s.committed_time, time(20));
            assert!(response.try_read().is_none());
            let (selected, _, selected_response) = s.step3_peek().unwrap();
            assert_eq!(selected_response, response);
            s.step4_resource_block(selected, &resources, &response)
                .unwrap();
            s.step5_guest_unblock(selected, &resources, &response)
                .unwrap();
            s.step6_reenquue(selected, false);
            assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
            assert_eq!(s.next_turns[&tid].protocol.owner, NextTurnOwner::Ordinary);
            assert_eq!(s.turn, 1);
            assert!(s.parked.requests.is_empty());
        }
    }

    #[test]
    fn resume_refuses_wrong_observation_owner_before_restoring_membership() {
        let (mut s, tid, mm, site) = fixture();
        park(&mut s, tid, mm, site, time(100));
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(10),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        let control = pop(&mut s, 10);
        let PublicationActivation::AwaitResume(ticket) = acknowledge(&mut s, mm, control, true)
        else {
            panic!("blocked publication must await registration");
        };
        let wait = ticket.continuation;
        s.parked.requests.get_mut(&wait).unwrap().origin.control = ControlCapability::PublishOnly {
            lease: ParkedObservationLease { nonce: 123 },
            site,
        };
        let before = format!("{:?}", s.run_queue);
        let request = s.next_turns[&tid].req.clone();
        let response = s.next_turns[&tid].resp.clone();
        let epoch = s.next_turns[&tid].protocol.epoch;
        assert_eq!(
            s.resume_request(ticket, site, Ivar::new()),
            Err(ProtocolFailure::Identity)
        );
        assert_eq!(format!("{:?}", s.run_queue), before);
        assert!(s.blocked.timed_waiters.thread_deadline(tid).is_none());
        assert!(
            matches!(s.parked.requests[&wait].membership, Some(SavedMembership::Timed(deadline)) if deadline == time(100))
        );
        assert_eq!(s.next_turns[&tid].req, request);
        assert_eq!(s.next_turns[&tid].resp, response);
        assert_eq!(s.next_turns[&tid].protocol.epoch, epoch);
    }

    #[test]
    fn exhausted_transport_refuses_before_commit_log_or_grant() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Fields(String);
                impl tracing::field::Visit for Fields {
                    fn record_debug(
                        &mut self,
                        _: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        use std::fmt::Write;
                        write!(&mut self.0, "{value:?}").unwrap();
                    }
                }
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let capture = Capture(Default::default());
        let events = capture.0.clone();
        tracing::subscriber::with_default(capture, || {
            let (mut s, tid, _, _) = fixture();
            let mut resources = Resources::new(tid);
            resources.insert(ResourceID::MemAddrSpace(tid), Permission::RW);
            let turn = s.next_turns.get_mut(&tid).unwrap();
            turn.req.put(Ok(resources.clone()));
            turn.protocol.epoch = u64::MAX;
            let response = turn.resp.clone();
            assert!(s.step5_guest_unblock(tid, &resources, &response).is_err());
            assert_eq!(s.turn, 0);
            assert!(s.backend_failed());
            assert!(!matches!(
                response.try_read(),
                Some(SchedResponse::Go(_) | SchedResponse::Signaled(_))
            ));
        });
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|line| line.contains("KVM parked signal protocol failed")
                    && line.contains("Overflow"))
        );
        assert!(
            events.iter().all(|line| !line.contains("COMMIT turn")),
            "{events:?}"
        );
    }

    #[test]
    fn last_thread_exit_removes_armed_periodic_deadline() {
        let (mut s, tid, mm, site) = fixture();
        s.replace_real_timer(
            tid,
            tid,
            time(0),
            time(10),
            time(10),
            nix::sys::signal::Signal::SIGALRM,
        )
        .unwrap();
        s.logically_kill_thread(&tid, &tid, mm);
        assert!(s.blocked.timed_waiters.is_empty());
        assert!(s.blocked.timed_waiters.pop().is_none());
        assert!(matches!(
            s.apply_signal_dequeue(tid, dequeue(site, 1, PendingDomain::Process), time(35))
                .unwrap(),
            super::super::real_timer::DequeueAck::Retired { sequence: 1 }
        ));
        assert!(s.blocked.timed_waiters.next_deadline().is_none());
    }

    #[test]
    fn kvm_dispatch_never_takes_host_delivery_even_when_policy_disabled() {
        for normal_due in [false, true] {
            for id in [
                SignalTimerId::Alarm(DetPid::from_raw(100)),
                SignalTimerId::Posix(DetPid::from_raw(100), 1),
                SignalTimerId::ChildExit {
                    child: DetPid::from_raw(101),
                    parent: DetPid::from_raw(100),
                },
            ] {
                let (mut s, tid, _, _) = fixture();
                s.kvm_shared_dequeue_timers = false;
                s.dispatch_timed_signal(
                    time(10),
                    id,
                    tid,
                    nix::sys::signal::Signal::SIGALRM,
                    normal_due,
                );
                assert_eq!(
                    s.backend_failed(),
                    !matches!(id, SignalTimerId::ChildExit { .. })
                );
                assert_eq!(s.host_signal_attempts, 0);
                if matches!(id, SignalTimerId::ChildExit { .. }) {
                    assert!(s.blocked.sigchld_ready.contains(&tid));
                }
            }
        }
    }
}
