//! Replay source for the existing external Connect scheduling pair.
//! The trace claim lives on an ordinary engine Call, never in this transport join.
//! https://github.com/rrnewton/hermit/issues/3630
//! AUTONOMOUS-BOT-IMPLEMENTED
//! TODO-HUMAN-REVIEW(PR-3464): https://github.com/rrnewton/hermit/pull/3464
use super::*;
use crate::network_replay::NetworkReplayError;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamOwner;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Queued,
    Granted,
}

enum IdleBoundary {
    Hold,
    Advance(Option<LogicalTime>),
}

pub(super) struct Pending {
    owner: NetworkStreamOwner,
    operation: ExternalOpId,
    call: NetworkStreamCallId,
    request: Ivar<SchedRequest>,
    response: Ivar<SchedResponse>,
    epoch: u64,
    phase: Phase,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayConnectPending")
            .field("owner", &self.owner)
            .field("operation", &self.operation)
            .field("call", &self.call)
            .field("epoch", &self.epoch)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

fn invalid(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(format!("Replay Connect continuation: {message}"))
}

impl Scheduler {
    pub(crate) fn check_replay_connect_start(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
    ) -> Result<(), NetworkReplayError> {
        let turn = self
            .next_turns
            .get(&owner.thread)
            .ok_or_else(|| invalid("missing task"))?;
        if self.backend_failed()
            || self.thread_is_logically_killed(owner.thread)
            || operation.tid != owner.thread
            || !self.original_external_grant_matches(owner, operation)
            || self.network_capture_uses_host_time
            || self.replay_connect.contains_key(&owner.thread)
            || turn.req.try_read().is_some()
            || turn.resp.try_read().is_some()
        {
            return Err(invalid("start is not its live selected external grant"));
        }
        Ok(())
    }

    pub(crate) fn enroll_replay_connect(
        &mut self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        self.check_replay_connect_start(owner, operation)?;
        let turn = &self.next_turns[&owner.thread];
        self.replay_connect.insert(
            owner.thread,
            Pending {
                owner,
                operation,
                call,
                request: turn.req.clone(),
                response: turn.resp.clone(),
                epoch: turn.protocol.epoch,
                phase: Phase::Waiting,
            },
        );
        Ok(())
    }

    fn replay_connect_request(
        &self,
        pending: &Pending,
    ) -> Result<Option<Resources>, NetworkReplayError> {
        let turn = self
            .next_turns
            .get(&pending.owner.thread)
            .ok_or_else(|| invalid("missing continuation owner"))?;
        if self.backend_failed()
            || self.thread_is_logically_killed(pending.owner.thread)
            || !self.rpc_incarnation_matches(pending.owner.thread, pending.owner.mm)
            || turn.req != pending.request
            || turn.resp != pending.response
            || turn.protocol.epoch != pending.epoch
        {
            return Err(invalid("continuation transport/task/MM changed"));
        }
        let Some(request) = turn.req.try_read() else {
            return Ok(None);
        };
        let resources = request.map_err(|_| invalid("continuation owner exited"))?;
        if turn
            .protocol
            .origin
            .as_ref()
            .is_none_or(|origin| origin.mm != pending.owner.mm)
            || resources.tid != pending.owner.thread
            || resources.resources.len() != 1
            || resources
                .resources
                .get(&ResourceID::BlockedExternalContinue(pending.operation))
                != Some(&Permission::RW)
            || resources.poll_attempt != 0
            || resources.fd_read.is_some()
            || resources.signal_interrupt_errno().is_some()
            || !self.inbound_signals(pending.owner.thread).is_empty()
        {
            return Err(invalid(
                "expected exact Normal Connect continuation, not a signal or another operation",
            ));
        }
        Ok(Some(resources))
    }

    pub(super) fn replay_connect_ready(
        &self,
        tid: DetTid,
        operation: ExternalOpId,
    ) -> Result<bool, NetworkReplayError> {
        let pending = &self.replay_connect[&tid];
        if pending.operation != operation || pending.phase != Phase::Waiting {
            return Err(invalid("external blocker changed operation/phase"));
        }
        if self.replay_connect_request(pending)?.is_none() {
            return Ok(false);
        }
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| invalid("missing engine"))?
            .lock()
            .unwrap();
        engine.release_eligible(self.committed_time)?;
        Ok(engine
            .replay_connect_status(pending.owner, operation, pending.call, self.committed_time)?
            .ready)
    }

    pub(super) fn requeue_replay_connect(&mut self, tid: DetTid) {
        if let Some(pending) = self.replay_connect.get_mut(&tid) {
            assert_eq!(pending.phase, Phase::Waiting);
            pending.phase = Phase::Queued;
        }
    }

    /// Called after ordinary external completion harvesting, including its
    /// spinning path. A posted continuation and exact trace gate are both
    /// necessary; a late RPC is neither a completed operation nor deadlock.
    pub(super) fn advance_replay_connect(
        &mut self,
        global_time: &Arc<Mutex<GlobalTime>>,
    ) -> Result<(), SkipTurn> {
        if self.backend_failed() || self.terminal_deadlock.is_some() {
            return Err(SkipTurn);
        }
        if self.replay_connect.is_empty()
            || !self.run_queue.is_empty()
            || !self.pending_physical_process_exits.is_empty()
        {
            return Ok(());
        }
        // Do not let a logical completion authorize progress for a different
        // operation still executing in the host kernel.
        if self
            .blocked
            .external_io_blockers
            .keys()
            .any(|tid| !self.replay_connect.contains_key(tid))
        {
            self.terminal_deadlock.get_or_insert_with(|| {
                invalid("sole-root trace continuation mixed with unresolved native IO").to_string()
            });
            return Err(SkipTurn);
        }
        let next = (|| -> Result<IdleBoundary, NetworkReplayError> {
            let mut next = None;
            let mut engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| invalid("missing engine"))?
                .lock()
                .unwrap();
            engine.release_eligible(self.committed_time)?;
            for pending in self.replay_connect.values() {
                if pending.phase != Phase::Waiting
                    || self.replay_connect_request(pending)?.is_none()
                {
                    return Ok(IdleBoundary::Hold);
                }
                if self.blocked.external_io_blockers.get(&pending.owner.thread)
                    != Some(&pending.operation)
                    || self.network_capture_blockers.get(&pending.owner.thread)
                        != Some(&pending.operation)
                {
                    return Err(invalid(
                        "pending operation lost external capture membership",
                    ));
                }
                let status = engine.replay_connect_status(
                    pending.owner,
                    pending.operation,
                    pending.call,
                    self.committed_time,
                )?;
                if status.ready {
                    return Ok(IdleBoundary::Hold);
                }
                if let Some(time) = status.next_release {
                    next = Some(next.map_or(time, |old: LogicalTime| old.min(time)));
                }
            }
            Ok(IdleBoundary::Advance(next))
        })();
        let next = match next {
            Ok(IdleBoundary::Hold) => return Ok(()),
            Ok(IdleBoundary::Advance(next)) => next,
            Err(error) => {
                self.terminal_deadlock
                    .get_or_insert_with(|| error.to_string());
                return Err(SkipTurn);
            }
        };
        let next_timer = self
            .blocked
            .timed_waiters
            .next_deadline()
            .filter(|time| !time.is_indefinite());
        let next = match (next, next_timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(next) = next {
            let mut time = global_time.lock().unwrap();
            let now = time.as_nanos();
            if next > now {
                time.add_extra_time(next.duration_since(now));
                // Publish through the normal next step1; due timers still run
                // in step2b before any external continuation is admitted.
                return Err(SkipTurn);
            }
        }
        Ok(())
    }

    pub(super) fn check_replay_connect_grant(&self, tid: DetTid) -> Result<(), NetworkReplayError> {
        let Some(pending) = self.replay_connect.get(&tid) else {
            return Ok(());
        };
        if pending.phase != Phase::Queued || self.replay_connect_request(pending)?.is_none() {
            return Err(invalid("grant lacks its exact queued continuation"));
        }
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| invalid("missing engine"))?
            .lock()
            .unwrap();
        if !engine
            .replay_connect_status(
                pending.owner,
                pending.operation,
                pending.call,
                self.committed_time,
            )?
            .ready
        {
            return Err(invalid("claimed Connect is no longer eligible"));
        }
        Ok(())
    }

    pub(super) fn record_replay_connect_grant(&mut self, tid: DetTid) {
        if let Some(pending) = self.replay_connect.get_mut(&tid) {
            assert_eq!(pending.phase, Phase::Queued);
            let turn = &self.next_turns[&tid];
            pending.phase = Phase::Granted;
            pending.epoch = turn.protocol.epoch;
            pending.request = turn.req.clone();
            pending.response = turn.resp.clone();
        }
    }

    pub(crate) fn check_replay_connect_completion(
        &self,
        owner: NetworkStreamOwner,
        operation: ExternalOpId,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        let pending = self
            .replay_connect
            .get(&owner.thread)
            .ok_or_else(|| invalid("missing granted claim"))?;
        let turn = self
            .next_turns
            .get(&owner.thread)
            .ok_or_else(|| invalid("missing granted owner"))?;
        let observation = self
            .ordinary_fd_observation(owner)
            .map_err(|_| invalid("lost foreground continuation"))?;
        if pending.owner != owner
            || pending.operation != operation
            || pending.call != call
            || pending.phase != Phase::Granted
            || pending.epoch != observation.epoch()
            || observation.resume() != ordinary_fd::OrdinaryFdResume::Normal
            || turn.req != pending.request
            || turn.resp != pending.response
            || self
                .blocked
                .external_io_blockers
                .contains_key(&owner.thread)
            || self.network_capture_blockers.contains_key(&owner.thread)
        {
            return Err(invalid("result differs from selected continuation"));
        }
        Ok(())
    }

    pub(crate) fn finish_replay_connect(&mut self, owner: NetworkStreamOwner) {
        let pending = self
            .replay_connect
            .remove(&owner.thread)
            .expect("validated Connect continuation");
        assert_eq!(pending.phase, Phase::Granted);
    }
}

#[cfg(test)]
mod tests;
