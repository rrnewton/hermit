/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bounded parent-thread-death publication and recipient wait admission.
//!
//! The backend admits one live task per recipient process and retains sticky
//! enrollment through clear/exec. This consumer supports only authenticated
//! pause/nanosleep parking in that domain. Other waits fail before entry.

use reverie::ParentDeathPublicationResult;
use reverie::SignalBoundaryOutcome;
use reverie::SignalBoundaryReceipt;
use reverie::SignalProcessId;

use super::Scheduler;
use super::parked::ControlCapability;
use super::parked::NextTurnOwner;
use super::parked::ParkedWaitPolicy;
use super::parked::ProcessGeneration;
use super::parked::ProtocolFailure;
use super::parked::SelectionFailure;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::DetTid;
use crate::types::LogicalTime;

impl Scheduler {
    pub(crate) fn parent_death_enrolled(&self, tid: DetTid) -> Result<bool, ProtocolFailure> {
        if !self.parked.parent_death_enabled {
            return Ok(false);
        }
        let control = self
            .parked
            .control
            .as_ref()
            .ok_or(ProtocolFailure::Identity)?;
        let pid = self
            .registered_process(tid)
            .ok_or(ProtocolFailure::Identity)?;
        let (mm, identity) = self
            .real_timers
            .task_identity(pid, tid)
            .ok_or(ProtocolFailure::Identity)?;
        if !self.rpc_incarnation_matches(tid, mm) {
            return Err(ProtocolFailure::Identity);
        }
        control
            .process
            .parent_death_enrolled(identity.process)
            .map_err(|error| ProtocolFailure::ParentDeathQuery(error.into_raw()))
    }

    pub(crate) fn validate_parent_death_resource(
        &self,
        tid: DetTid,
        resources: &Resources,
        capability: ControlCapability,
    ) -> Result<(), ProtocolFailure> {
        if !self.parent_death_enrolled(tid)? {
            return Ok(());
        }
        for resource in resources.resources.keys() {
            let supported = match resource {
                ResourceID::SleepUntil(deadline) => {
                    resources.resources.len() == 1
                        && match capability {
                            ControlCapability::ParkedWait {
                                policy: ParkedWaitPolicy::PauseNoHandlerRestart,
                                ..
                            } => *deadline == LogicalTime::INDEFINITE,
                            ControlCapability::ParkedWait {
                                policy:
                                    ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline },
                                ..
                            } => *deadline == absolute_deadline,
                            _ => false,
                        }
                }
                // The initial capability does not extend the generic legacy
                // WaitidSignals interruption protocol. A known ready/no-child
                // result may continue without parking or consuming status here.
                ResourceID::WaitChild { parent, spec } => {
                    self.ready_child_wait(*parent, *spec).is_some()
                        || !self.has_child_wait_target(*parent, *spec)
                }
                ResourceID::WaitPhysicalChild(child) => {
                    self.completed_physical_process_exits.contains(child)
                }
                ResourceID::FutexWait
                | ResourceID::InternalIOPolling
                | ResourceID::BlockingExternalIO(_)
                | ResourceID::BlockingVfork(_)
                | ResourceID::BlockingRtSigsuspend(_)
                | ResourceID::HappensBeforeCheckpoint(_) => false,
                ResourceID::FileContents(_)
                | ResourceID::FileMetadata(_)
                | ResourceID::DirectoryContents(_)
                | ResourceID::MemAddrSpace(_)
                | ResourceID::Path(_)
                | ResourceID::PathsTransitive(_)
                | ResourceID::Device(_)
                | ResourceID::Exit { .. }
                | ResourceID::ParentContinue { .. }
                | ResourceID::TraceReplay
                | ResourceID::VforkFailed(_)
                | ResourceID::BlockedExternalContinue(_)
                | ResourceID::PriorityChangePoint(..)
                | ResourceID::InboundSignal(_)
                | ResourceID::WaitidSignals(_)
                | ResourceID::SchedYield => true,
            };
            if !supported {
                return Err(ProtocolFailure::ParentDeathUnsupportedWait);
            }
        }
        Ok(())
    }

    /// The caller validated the exact permit and complete terminal target set.
    /// Do not release that fence until the retained backend batch is accounted.
    pub(super) fn publish_parent_death_boundary(
        &mut self,
        boundary: SignalBoundaryReceipt,
    ) -> Result<(), reverie::Error> {
        if !self.parked.parent_death_enabled
            || !matches!(
                boundary.outcome,
                SignalBoundaryOutcome::Terminated { .. } | SignalBoundaryOutcome::ImageReplaced
            )
        {
            return Ok(());
        }
        let control = self.parked.control.clone().ok_or(reverie::Errno::EINVAL)?;
        let tid = DetTid::from_raw(boundary.permit.task.tid.as_raw());
        let receipt = match control.process.publish_parent_death(boundary) {
            ParentDeathPublicationResult::Committed(receipt) => receipt,
            ParentDeathPublicationResult::RejectedBeforeCommit(errno) => {
                self.fail_parked(tid, ProtocolFailure::Identity);
                return Err(errno.into());
            }
            ParentDeathPublicationResult::FailedAfterCommit { receipt, errno } => {
                self.parked.parent_death_failures.push(receipt);
                self.fail_parked(tid, ProtocolFailure::Identity);
                return Err(errno.into());
            }
        };
        if receipt.boundary != boundary {
            self.fail_parked(tid, ProtocolFailure::Identity);
            return Err(reverie::Errno::EINVAL.into());
        }
        for effect in receipt.signals {
            if !effect.discarded {
                self.parked.parent_death_pending.insert(
                    (
                        ProcessGeneration::from_backend(effect.process),
                        effect.signal,
                    ),
                    effect.pending_generation,
                );
            }
        }
        self.wake_control_waiter();
        Ok(())
    }

    /// Called only at the existing quiescent selection points, never from the
    /// publisher or a host exit callback. Hints authorize no signal send; the
    /// backend's actual pending queue and masks determine current recipients.
    pub(super) fn select_parked_parent_death(&mut self) -> Result<(), SelectionFailure> {
        if !self.parked.parent_death_enabled {
            return Ok(());
        }
        let Some(control) = self.parked.control.clone() else {
            return Ok(());
        };
        // ThreadTree retains historical ancestry after logical exit. Eligibility
        // must use the timer registry's live-process inventory instead; an old
        // hint has no authority to turn ordinary retirement into a backend fault.
        let live_processes = self.real_timers.live_processes();
        let pending = self
            .parked
            .parent_death_pending
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for (generation, signal) in pending {
            let pid = generation.pid;
            let process = SignalProcessId {
                tgid: reverie::Pid::from_raw(pid.as_raw()),
                generation: generation.generation,
            };
            let fail = |failure| SelectionFailure {
                pid,
                tid: None,
                failure,
            };
            if !live_processes.contains(&pid) {
                self.parked
                    .parent_death_pending
                    .remove(&(generation, signal));
                continue;
            }
            if self
                .real_timers
                .process_identity(pid)
                .map_err(|failure| fail(failure.into()))?
                != process
            {
                self.parked
                    .parent_death_pending
                    .remove(&(generation, signal));
                continue;
            }
            if self
                .parked
                .permits
                .values()
                .any(|permit| permit.task.process == process)
            {
                continue;
            }
            let recipients = control
                .process
                .signal_recipients(process, signal)
                .map_err(|_| fail(ProtocolFailure::Identity))?;
            for recipient in recipients {
                let tid = DetTid::from_raw(recipient.task.tid.as_raw());
                let Some((mm, identity)) = self.real_timers.task_identity(pid, tid) else {
                    continue;
                };
                if identity != recipient.task
                    || identity.process != process
                    || !self.rpc_incarnation_matches(tid, mm)
                    || self.thread_is_logically_killed(tid)
                    || self.parked.permits.contains_key(&tid)
                {
                    continue;
                }
                let Some(turn) = self.next_turns.get(&tid) else {
                    continue;
                };
                if turn.protocol.owner != NextTurnOwner::Ordinary {
                    continue;
                }
                let Some(origin) = turn.protocol.origin else {
                    continue;
                };
                let deadline = match origin.control {
                    ControlCapability::ParkedWait { policy, .. } => match policy {
                        ParkedWaitPolicy::NanosleepNoHandlerRestart { absolute_deadline } => {
                            absolute_deadline
                        }
                        ParkedWaitPolicy::PauseNoHandlerRestart => LogicalTime::INDEFINITE,
                    },
                    _ => continue,
                };
                if deadline <= self.committed_time
                    || turn.req.try_read().is_none()
                    || turn.resp.try_read().is_some()
                {
                    continue;
                }
                self.begin_pending_signal_observation(pid, tid, recipient.task)
                    .map_err(|failure| SelectionFailure {
                        pid,
                        tid: Some(tid),
                        failure,
                    })?;
                break;
            }
        }
        Ok(())
    }
}
