// Copyright (c) Meta Platforms, Inc. and affiliates.
// Licensed under the BSD-style license in the LICENSE file.

//! Scheduler ownership for target-side child-exit control exchanges.

use super::*;
use crate::child_exit::Attempt;
use crate::child_exit::AttemptPhase;
use crate::child_exit::Command;
use crate::child_exit::ControlResult;
use crate::child_exit::Delivery;
use crate::child_exit::Disposition;
use crate::child_exit::Failure;
use crate::child_exit::FailureStage;
use crate::child_exit::FatalRecord;
use crate::child_exit::NormalExit;
use crate::child_exit::Operation;
use crate::child_exit::OperationId;
use crate::child_exit::Outcome;
use crate::child_exit::Phase;

pub(super) enum ControlWait {
    Acknowledgement(Ivar<ControlResult>),
    Request(Ivar<SchedRequest>),
}

#[derive(Debug)]
pub(crate) struct ResumedOperation {
    pub operation: Arc<Operation>,
    pub response: Ivar<SchedResponse>,
}

impl Scheduler {
    pub(crate) fn begin_child_exit_operation(
        &mut self,
        tid: DetTid,
        mm: MmId,
        parent: DetPid,
        original: Resources,
        guest_time: LogicalTime,
    ) -> Result<Arc<Operation>, Arc<FatalRecord>> {
        let sequence = self.child_exits.sequences.entry(tid).or_default();
        *sequence = sequence
            .checked_add(1)
            .expect("resource-operation sequence overflow");
        let id = OperationId {
            tid,
            mm,
            sequence: *sequence,
        };
        if original.tid != tid || self.child_exits.current.contains_key(&tid) {
            return Err(self.fail_child_exit(Failure::protocol(id, 0, parent)));
        }
        let operation = Arc::new(Operation {
            id,
            parent,
            original,
            guest_time,
            phase: Mutex::new(Phase::Waiting),
        });
        self.child_exits.current.insert(tid, id);
        assert!(
            self.child_exits
                .operations
                .insert(id, operation.clone())
                .is_none()
        );
        Ok(operation)
    }

    pub(crate) fn fail_child_exit(&mut self, failure: Failure) -> Arc<FatalRecord> {
        self.child_exits
            .fatal
            .get_or_insert_with(|| FatalRecord::new(failure))
            .clone()
    }

    pub(crate) fn retire_child_exit_operation(&mut self, tid: DetTid, mm: MmId) {
        let Some(id) = self.child_exits.current.get(&tid).copied() else {
            return;
        };
        if id.mm != mm {
            return;
        }
        self.child_exits.current.remove(&tid);
        if let Some(operation) = self.child_exits.operations.get(&id) {
            operation.retire();
        }
    }

    pub(crate) fn outstanding_child_exit_completion(&self) -> Option<Ivar<ControlResult>> {
        self.child_exits
            .operations
            .values()
            .filter_map(|op| {
                op.awaiting_completion()
                    .map(|iv| ((op.id.tid, op.id.sequence), iv))
            })
            .min_by_key(|(id, _)| *id)
            .map(|(_, iv)| iv)
    }

    pub(crate) fn finish_child_exit_operation(
        &mut self,
        id: OperationId,
    ) -> Result<(), Arc<FatalRecord>> {
        let Some(operation) = self.child_exits.operations.get(&id).cloned() else {
            return Err(self.fail_child_exit(Failure::protocol(id, 0, id.tid)));
        };
        let granted = matches!(*operation.phase.lock().unwrap(), Phase::Granted);
        if !granted {
            return Err(self.fail_child_exit(Failure::protocol(id, 0, operation.parent)));
        }
        self.child_exits.current.remove(&id.tid);
        self.child_exits.operations.remove(&id);
        Ok(())
    }

    pub(super) fn grant_child_exit_operation(&mut self, tid: DetTid) {
        let Some(id) = self.child_exits.current.get(&tid) else {
            return;
        };
        let operation = &self.child_exits.operations[id];
        let mut phase = operation.phase.lock().unwrap();
        assert!(
            matches!(*phase, Phase::Waiting),
            "child delivery must settle before resource grant"
        );
        *phase = Phase::Granted;
    }

    pub(super) fn prepare_virtual_child_exit(
        &mut self,
        tid: DetTid,
        group: bool,
        process: DetPid,
        mm: MmId,
    ) {
        let Some(parent) = self.thread_tree.parent_process(&process) else {
            return;
        };
        // clone with an exit signal of zero requests no child notification.
        // Its wait status remains independent; do not invent SIGCHLD or refuse it.
        if self
            .thread_tree
            .process_wait
            .get(&process)
            .is_some_and(|record| record.exit_signal == 0)
        {
            return;
        }
        let threads = self.process_signal_targets(process);
        // The first producer is a normal single-thread child exit. A group exit
        // with live siblings needs final accounting and completion ordering of its own.
        if threads != [tid] {
            self.refuse_virtual_signal_route(tid);
            return;
        }
        let Some(id) = self.child_exits.current.get(&tid).copied() else {
            self.refuse_virtual_signal_route(tid);
            return;
        };
        let operation = &self.child_exits.operations[&id];
        let Some(exit) = operation.original.normal_exit else {
            self.fail_child_exit(Failure::protocol(id, 0, process));
            return;
        };
        if operation.original.exit_identity() != Some((group, process, mm)) {
            self.fail_child_exit(Failure::protocol(id, 0, process));
            return;
        }
        if self
            .thread_tree
            .process_wait
            .get(&process)
            .map(|record| record.exit_signal)
            != Some(libc::SIGCHLD)
        {
            self.refuse_virtual_signal_route(tid);
            return;
        }
        if self
            .child_exits
            .normal_exits
            .insert((process, mm), exit)
            .is_some()
        {
            self.fail_child_exit(Failure::protocol(id, 0, process));
            return;
        }
        self.child_exits.next_delivery = self
            .child_exits
            .next_delivery
            .checked_add(1)
            .expect("child-delivery sequence overflow");
        let delivery = Delivery {
            id: self.child_exits.next_delivery,
            child: process,
            child_mm: mm,
            parent,
            exit,
            deadline: self.committed_time + LogicalTime::from_nanos(1),
        };
        self.blocked
            .timed_waiters
            .insert_child_exit(delivery.deadline, process, parent, parent);
        self.child_exits.pending.insert(delivery.id, delivery);
    }

    pub(super) fn child_exit_became_due(&mut self, child: DetPid, parent: DetPid) {
        let ids: Vec<_> = self
            .child_exits
            .pending
            .iter()
            .filter_map(|(id, d)| (d.child == child && d.parent == parent).then_some(*id))
            .collect();
        if ids.len() != 1 {
            self.refuse_virtual_signal_route(parent);
            return;
        }
        let delivery = self.child_exits.pending.remove(&ids[0]).unwrap();
        self.child_exits.due.insert(delivery.id, delivery);
    }

    pub(super) fn refuse_virtual_signal_route(&mut self, tid: DetTid) {
        let operation = self
            .child_exits
            .current
            .get(&tid)
            .copied()
            .unwrap_or(OperationId {
                tid,
                mm: MmId::initial(tid),
                sequence: 0,
            });
        let mut failure = Failure::protocol(operation, 0, tid);
        failure.errno = libc::ENOSYS;
        failure.stage = FailureStage::UnsupportedRoute;
        failure.unsupported = true;
        self.fail_child_exit(failure);
    }

    pub(super) fn dispatch_child_exit_control(
        &mut self,
    ) -> Result<Option<ControlWait>, Arc<FatalRecord>> {
        if let Some(fatal) = &self.child_exits.fatal {
            return Err(fatal.clone());
        }
        let (delivery, targets) = loop {
            let Some(delivery) = self.child_exits.due.values().next().cloned() else {
                return Ok(None);
            };
            let targets = self.process_signal_targets(delivery.parent);
            if !targets.is_empty() {
                break (delivery, targets);
            }
            // The whole destination process has retired. No replacement task owns
            // its event. Iterate in the same delivery order without growing the stack.
            self.child_exits.due.remove(&delivery.id);
        };
        if targets.len() != 1 {
            self.refuse_virtual_signal_route(delivery.parent);
            return Err(self.child_exits.fatal.clone().unwrap());
        }
        let target = targets[0];
        let nextturn = self.next_turns[&target].clone();
        let operation = self
            .child_exits
            .current
            .get(&target)
            .and_then(|id| self.child_exits.operations.get(id))
            .cloned();
        let Some(operation) = operation else {
            if nextturn.req.try_read().is_none() {
                return Ok(Some(ControlWait::Request(nextturn.req)));
            }
            // FutexAction and lifecycle callbacks have different response consumers.
            // Do not send a resource control to an unrelated one-shot channel.
            self.refuse_virtual_signal_route(target);
            return Err(self.child_exits.fatal.clone().unwrap());
        };
        let mut phase = operation.phase.lock().unwrap();
        match &*phase {
            Phase::Waiting => {}
            Phase::Granted if nextturn.req.try_read().is_none() => {
                return Ok(Some(ControlWait::Request(nextturn.req)));
            }
            Phase::Delivering(attempt) => {
                return Ok(Some(ControlWait::Acknowledgement(
                    attempt.acknowledgement.clone(),
                )));
            }
            Phase::Failed(fatal) => return Err(fatal.clone()),
            _ => {
                drop(phase);
                return Err(self.fail_child_exit(Failure::protocol(
                    operation.id,
                    delivery.id,
                    delivery.child,
                )));
            }
        }
        if operation.parent != delivery.parent
            || nextturn.resp.try_read().is_some()
            || !self.rpc_incarnation_matches(target, operation.id.mm)
        {
            drop(phase);
            return Err(self.fail_child_exit(Failure::protocol(
                operation.id,
                delivery.id,
                delivery.child,
            )));
        }
        let command = Command::new(operation.id, delivery);
        let acknowledgement = Ivar::new();
        *phase = Phase::Delivering(Box::new(Attempt {
            command: command.clone(),
            phase: AttemptPhase::NotReturned,
            acknowledgement: acknowledgement.clone(),
            completion: Ivar::new(),
        }));
        nextturn
            .resp
            .put(SchedResponse::DeliverChildExit(Box::new(command)));
        Ok(Some(ControlWait::Acknowledgement(acknowledgement)))
    }

    pub(crate) fn acknowledge_child_exit(
        &mut self,
        id: OperationId,
        delivery_id: u64,
        outcome: Outcome,
    ) -> Result<Option<ResumedOperation>, Arc<FatalRecord>> {
        let Some(operation) = self.child_exits.operations.get(&id).cloned() else {
            return Err(self.fail_child_exit(Failure::protocol(id, delivery_id, id.tid)));
        };
        let mut phase = operation.phase.lock().unwrap();
        let Phase::Delivering(attempt) = &mut *phase else {
            drop(phase);
            return Err(self.fail_child_exit(Failure::protocol(id, delivery_id, id.tid)));
        };
        if attempt.command.delivery.id != delivery_id
            || !matches!(
                attempt.phase,
                AttemptPhase::InCallback | AttemptPhase::TerminalAwaitingCallback
            )
        {
            drop(phase);
            return Err(self.fail_child_exit(Failure::protocol(id, delivery_id, id.tid)));
        }
        if let Some(failure) = Failure::from_outcome(&attempt.command, outcome) {
            let fatal = self.fail_child_exit(failure);
            attempt
                .acknowledgement
                .try_put(ControlResult::Failed(fatal.clone()));
            attempt
                .completion
                .try_put(ControlResult::Failed(fatal.clone()));
            *phase = Phase::Failed(fatal.clone());
            self.child_exits.current.remove(&id.tid);
            return Err(fatal);
        }
        let retired = attempt.phase == AttemptPhase::TerminalAwaitingCallback;
        self.child_exits.due.remove(&delivery_id);
        attempt.completion.put(if retired {
            ControlResult::TargetRetired
        } else {
            ControlResult::Accepted
        });
        attempt.phase = AttemptPhase::Settled;
        if retired {
            *phase = Phase::Retired;
            return Ok(None);
        }
        let resp = Ivar::new();
        let Some(nextturn) = self.next_turns.get_mut(&id.tid) else {
            drop(phase);
            return Err(self.fail_child_exit(Failure::protocol(id, delivery_id, id.tid)));
        };
        nextturn.resp = resp.clone();
        let acknowledgement = attempt.acknowledgement.clone();
        *phase = Phase::Waiting;
        drop(phase);
        if matches!(
            outcome,
            Outcome::Accepted {
                disposition: Disposition::PendingEligible,
                ..
            }
        ) {
            self.blocked.sigchld_ready.insert(id.tid);
            self.wake_signaled_guest(id.tid, Signal::SIGCHLD);
        }
        acknowledgement.put(ControlResult::Accepted);
        Ok(Some(ResumedOperation {
            operation,
            response: resp,
        }))
    }

    pub(crate) fn verify_normal_child_exit(
        &mut self,
        process: DetPid,
        mm: MmId,
        actual: Option<NormalExit>,
    ) {
        if let Some(expected) = self.child_exits.normal_exits.remove(&(process, mm))
            && actual != Some(expected)
        {
            let mut failure = Failure::protocol(
                OperationId {
                    tid: process,
                    mm,
                    sequence: 0,
                },
                0,
                process,
            );
            failure.stage = FailureStage::ExitMismatch;
            self.fail_child_exit(failure);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_child_exit_destinations_preserve_the_first_live_delivery() {
        let config = Config {
            backend_uses_virtual_signal_targets: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(4);
        scheduler.thread_tree.add_child(parent, parent, true);
        let original = Resources::new(parent);
        let request = Ivar::full(Ok(original.clone()));
        let response = Ivar::new();
        scheduler.next_turns.insert(
            parent,
            ThreadNextTurn {
                dettid: parent,
                child_tid_addr: 0,
                req: request.clone(),
                resp: response.clone(),
            },
        );
        let operation = scheduler
            .begin_child_exit_operation(
                parent,
                MmId::initial(parent),
                parent,
                original.clone(),
                LogicalTime::ZERO,
            )
            .unwrap();
        let first_live = Delivery {
            id: 513,
            child,
            child_mm: MmId::initial(child),
            parent,
            exit: NormalExit {
                status: 37,
                uid: 0,
                user_ticks: 23,
                system_ticks: 11,
            },
            deadline: LogicalTime::from_nanos(1),
        };
        // Insert in the opposite order. Selection must retain the delivery IDs'
        // existing order while discarding every already-retired destination.
        for id in (1..=514).rev() {
            let mut delivery = first_live.clone();
            delivery.id = id;
            if id < first_live.id {
                delivery.parent = DetPid::from_raw(999);
            }
            scheduler.child_exits.due.insert(id, delivery);
        }
        assert!(matches!(
            scheduler.dispatch_child_exit_control().unwrap(),
            Some(ControlWait::Acknowledgement(_))
        ));
        let Some(SchedResponse::DeliverChildExit(command)) = response.try_read() else {
            panic!("the first live delivery was lost")
        };
        assert_eq!(command.operation, operation.id);
        assert_eq!(command.delivery, first_live);
        assert_eq!(
            scheduler
                .child_exits
                .due
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![513, 514]
        );
        assert_eq!(scheduler.next_turns[&parent].req, request);
        assert_eq!(operation.original, original);
        assert_eq!(scheduler.turn, 0);
    }

    #[test]
    fn zero_clone_exit_signal_keeps_wait_evidence_without_child_notification() {
        let config = Config {
            backend_uses_virtual_signal_targets: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let parent = DetPid::from_raw(3);
        let child = DetPid::from_raw(4);
        scheduler.thread_tree.add_child(parent, parent, true);
        scheduler
            .thread_tree
            .add_child_with_wait_metadata(parent, child, true, false, 0);
        scheduler.prepare_virtual_child_exit(child, true, child, MmId::initial(child));
        assert!(scheduler.child_exits.pending.is_empty());
        assert!(scheduler.child_exits.due.is_empty());
        assert!(scheduler.child_exits.fatal.is_none());
        assert_eq!(scheduler.thread_tree.parent_process(&child), Some(parent));
        assert_eq!(scheduler.thread_tree.process_wait[&child].exit_signal, 0);
    }
}
