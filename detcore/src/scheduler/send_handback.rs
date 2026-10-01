//! Scheduler-issued temporal evidence, NOT an original-syscall return proof.
//! No constructor accepts serialized time; only existing scheduler transitions
//! issue stamps. Nonparticipants retain the exact existing scheduling policy.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::Scheduler;
use super::ordinary_fd::OrdinaryFdResume;
use crate::network_replay::send_timing::PendingSendTiming;
use crate::resources::ExternalOpId;
use crate::resources::ResourceID;
use crate::types::DetTid;
use crate::types::LogicalTime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendTimingError {
    Missing,
    Identity,
    Phase,
    UnsupportedSignalOrRestart,
    DueTimer,
    UnpublishedIdleTail,
    Cancelled,
}

/// A diagnostic projection of scheduler evidence; never accepted as authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SendTimingStamp {
    pub(crate) committed_time: LogicalTime,
    pub(crate) turn: u64,
    pub(crate) epoch: u64,
    pub(crate) publication: u64,
}

#[derive(Debug)]
pub(crate) struct SendTimingHandle {
    identity: Arc<()>,
    owner: crate::network_replay::NetworkStreamOwner,
}

#[derive(Debug)]
pub(crate) struct SendHandbackReceipt {
    pending: PendingSendTiming,
    operation: ExternalOpId,
    normal: SendTimingStamp,
    entry: SendTimingStamp,
    completion: SendTimingStamp,
    handback: SendTimingStamp,
}

impl SendHandbackReceipt {
    pub(crate) fn matches(&self, pending: &PendingSendTiming) -> bool {
        self.pending.same_pending(pending)
    }
    pub(crate) fn operation(&self) -> ExternalOpId {
        self.operation
    }
    pub(crate) fn stamps(&self) -> [SendTimingStamp; 4] {
        [self.normal, self.entry, self.completion, self.handback]
    }
}

#[derive(Debug)]
struct Attempt {
    identity: Arc<()>,
    pending: PendingSendTiming,
    operation: ExternalOpId,
    normal: SendTimingStamp,
    entry: Option<SendTimingStamp>,
    completion: Option<SendTimingStamp>,
    handback: Option<SendTimingStamp>,
    refused: Option<SendTimingError>,
}

#[derive(Default, Debug)]
pub(super) struct SendTimingBook {
    attempts: BTreeMap<DetTid, Attempt>,
    // Written only after bump_global_time has published committed_time.
    published: Option<(LogicalTime, u64)>,
    publication_exhausted: bool,
}

impl Scheduler {
    /// Production lock-order seam for the original-send owner to use before
    /// requesting its existing external pair. It performs no physical effect.
    pub(crate) fn claim_send_timing(
        &mut self,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        root: &crate::network_runtime::ForegroundRoot,
        lease: crate::network_replay::NetworkStreamLeaseId,
        operation: ExternalOpId,
    ) -> Result<SendTimingHandle, crate::network_replay::NetworkReplayError> {
        let invalid =
            || crate::network_replay::NetworkReplayError::UnresolvedStreamOperation(lease);
        self.send_clock_publication().map_err(|_| invalid())?;
        let grant = self
            .foreground_native_observation(root.owner(), root)
            .map_err(|_| invalid())?;
        let pending = engine.take_pending_send_timing(lease, &grant, self.committed_time)?;
        // If enrollment fails, the engine's claimed bit remains set: no
        // fallback through the old worker/V4 confirmation can hide that loss.
        self.enroll_send_timing(pending, operation)
            .map_err(|_| invalid())
    }

    pub(super) fn publish_send_clock(&mut self) {
        if self.send_timing.publication_exhausted {
            return;
        }
        let generation = self
            .send_timing
            .published
            .map_or(Some(1), |(_, n)| n.checked_add(1));
        self.send_timing.published = generation.map(|n| (self.committed_time, n));
        if generation.is_none() {
            self.send_timing.publication_exhausted = true;
            for attempt in self.send_timing.attempts.values_mut() {
                attempt.refused.get_or_insert(SendTimingError::Phase);
            }
        }
    }

    fn send_clock_publication(&self) -> Result<u64, SendTimingError> {
        self.send_timing
            .published
            .filter(|(time, _)| *time == self.committed_time)
            .map(|(_, generation)| generation)
            .ok_or(SendTimingError::Phase)
    }

    /// Call under scheduler -> engine lock order, immediately after claiming
    /// the engine pending capability. No request/turn/time is added here.
    pub(crate) fn enroll_send_timing(
        &mut self,
        pending: PendingSendTiming,
        operation: ExternalOpId,
    ) -> Result<SendTimingHandle, SendTimingError> {
        let owner = pending.owner();
        if owner.thread != operation.tid {
            return Err(SendTimingError::Identity);
        }
        let normal = self
            .ordinary_fd_observation(owner)
            .map_err(|_| SendTimingError::Phase)?;
        if normal.resume() != OrdinaryFdResume::Normal {
            return Err(SendTimingError::UnsupportedSignalOrRestart);
        }
        if normal.epoch() != pending.normal_epoch() {
            return Err(SendTimingError::Identity);
        }
        let stamp = SendTimingStamp {
            committed_time: self.committed_time,
            turn: self.turn,
            epoch: normal.epoch(),
            publication: self.send_clock_publication()?,
        };
        if self.send_timing.attempts.contains_key(&owner.thread) {
            return Err(SendTimingError::Phase);
        }
        let identity = Arc::new(());
        self.send_timing.attempts.insert(
            owner.thread,
            Attempt {
                identity: identity.clone(),
                pending,
                operation,
                normal: stamp,
                entry: None,
                completion: None,
                handback: None,
                refused: None,
            },
        );
        Ok(SendTimingHandle { identity, owner })
    }

    /// Read only after the real Normal response has installed its empty gate.
    /// Move out once; a copied operation number cannot recreate this handle.
    pub(crate) fn take_send_handback(
        &mut self,
        handle: &SendTimingHandle,
    ) -> Result<SendHandbackReceipt, SendTimingError> {
        let attempt = self
            .send_timing
            .attempts
            .get(&handle.owner.thread)
            .ok_or(SendTimingError::Missing)?;
        if !Arc::ptr_eq(&attempt.identity, &handle.identity)
            || attempt.pending.owner() != handle.owner
        {
            return Err(SendTimingError::Identity);
        }
        if let Some(error) = attempt.refused {
            return Err(error);
        }
        let handback = attempt.handback.ok_or(SendTimingError::Phase)?;
        let grant = self
            .ordinary_fd_observation(handle.owner)
            .map_err(|_| SendTimingError::Phase)?;
        if grant.resume() != OrdinaryFdResume::Normal || grant.epoch() != handback.epoch {
            return Err(SendTimingError::Phase);
        }
        let entry = attempt.entry.ok_or(SendTimingError::Phase)?;
        let completion = attempt.completion.ok_or(SendTimingError::Phase)?;
        let attempt = self
            .send_timing
            .attempts
            .remove(&handle.owner.thread)
            .unwrap();
        Ok(SendHandbackReceipt {
            pending: attempt.pending,
            operation: attempt.operation,
            normal: attempt.normal,
            entry,
            completion,
            handback,
        })
    }

    /// Called before clear_nextturn/response publication, never from RPC data.
    /// Refusal invalidates this evidence ONLY; existing Linux signal/cleanup
    /// behavior must continue unchanged until its outcome is explicitly modeled.
    pub(super) fn observe_send_grant(&mut self, tid: DetTid, normal: bool) {
        if !self.send_timing.attempts.contains_key(&tid) {
            return;
        }
        let publication = self.send_clock_publication();
        let turn = &self.next_turns[&tid];
        let epoch = turn.protocol.epoch.checked_add(1);
        let operation = turn
            .req
            .try_read()
            .and_then(|request| request.ok())
            .and_then(|r| {
                if r.resources.len() != 1 {
                    return None;
                }
                match r.resources.keys().next()? {
                    ResourceID::BlockingNetworkCapture(op) => Some((true, *op)),
                    ResourceID::BlockedExternalContinue(op) => Some((false, *op)),
                    _ => None,
                }
            });
        let owner_matches =
            turn.protocol.origin.as_ref().is_some_and(|origin| {
                origin.mm == self.send_timing.attempts[&tid].pending.owner().mm
            });
        let due = self
            .blocked
            .timed_waiters
            .next_deadline()
            .is_some_and(|t| t <= self.committed_time);
        let idle_tail = self.network_capture_idle_since.is_some();
        let attempt = self.send_timing.attempts.get_mut(&tid).unwrap();
        if attempt.refused.is_some() {
            return;
        }
        let result = (|| {
            if !normal {
                return Err(SendTimingError::UnsupportedSignalOrRestart);
            }
            if !owner_matches {
                return Err(SendTimingError::Identity);
            }
            let (entry, operation) = operation.ok_or(SendTimingError::Phase)?;
            if operation != attempt.operation {
                return Err(SendTimingError::Identity);
            }
            let stamp = SendTimingStamp {
                committed_time: self.committed_time,
                turn: self.turn.checked_add(1).ok_or(SendTimingError::Phase)?,
                epoch: epoch.ok_or(SendTimingError::Phase)?,
                publication: publication?,
            };
            if entry {
                if attempt.entry.is_some() || turn.protocol.epoch != attempt.normal.epoch {
                    return Err(SendTimingError::Phase);
                }
                attempt.entry = Some(stamp);
            } else {
                let entry = attempt.entry.ok_or(SendTimingError::Phase)?;
                let completion = attempt.completion.ok_or(SendTimingError::Phase)?;
                if attempt.handback.is_some()
                    || completion.committed_time > stamp.committed_time
                    || completion.epoch != entry.epoch
                    || turn.protocol.epoch != completion.epoch
                {
                    return Err(SendTimingError::Phase);
                }
                if idle_tail {
                    return Err(SendTimingError::UnpublishedIdleTail);
                }
                if due {
                    return Err(SendTimingError::DueTimer);
                }
                attempt.handback = Some(stamp);
            }
            Ok(())
        })();
        if let Err(error) = result {
            attempt.refused = Some(error);
        }
    }

    /// The existing harvest checks the original external operation. This stamp
    /// is its observation at published time, NOT backend EXIT or final idle time.
    pub(super) fn observe_send_completion(&mut self, tid: DetTid, operation: ExternalOpId) {
        let publication = self.send_clock_publication();
        let Some(attempt) = self.send_timing.attempts.get_mut(&tid) else {
            return;
        };
        if attempt.refused.is_some() {
            return;
        }
        let publication = match publication {
            Ok(publication) => publication,
            Err(error) => {
                attempt.refused = Some(error);
                return;
            }
        };
        let turn = &self.next_turns[&tid];
        let exact = turn.req.try_read().is_some_and(|r| {
            r.is_ok_and(|r| {
                r.resources.len() == 1
                    && r.resources
                        .contains_key(&ResourceID::BlockedExternalContinue(operation))
            })
        });
        if attempt.operation != operation || !exact {
            attempt.refused = Some(SendTimingError::UnsupportedSignalOrRestart);
        } else if attempt.entry.is_none() || attempt.completion.is_some() {
            attempt.refused = Some(SendTimingError::Phase);
        } else {
            if turn.protocol.epoch != attempt.entry.unwrap().epoch {
                attempt.refused = Some(SendTimingError::Phase);
                return;
            }
            attempt.completion = Some(SendTimingStamp {
                committed_time: self.committed_time,
                turn: self.turn,
                epoch: turn.protocol.epoch,
                publication,
            });
        }
    }

    pub(super) fn cancel_send_timing(&mut self, tid: DetTid) {
        if let Some(attempt) = self.send_timing.attempts.get_mut(&tid) {
            attempt.refused.get_or_insert(SendTimingError::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl Scheduler {
    /// Engine components publish through the existing clock operation. This
    /// does not construct a publication number, grant, handle or receipt.
    pub(crate) fn controlled_publish_send_clock(
        &mut self,
        global: &std::sync::Mutex<crate::types::GlobalTime>,
    ) {
        self.bump_global_time(global, &Err(super::SkipTurn));
    }

    /// Drive the existing external pair and actual queue/harvest/Normal
    /// transitions. The component supplies no physical send or backend EXIT.
    /// Only take_send_handback may subsequently produce the one-use receipt.
    pub(crate) fn controlled_complete_engine_send(
        &mut self,
        owner: crate::network_replay::NetworkStreamOwner,
        operation: ExternalOpId,
        global: &std::sync::Arc<std::sync::Mutex<crate::types::GlobalTime>>,
    ) {
        use super::parked::ControlCapability;
        use super::parked::ResourceOrigin;
        use super::parked::RpcOrigin;
        use crate::resources::Permission;
        use crate::resources::Resources;
        self.controlled_selected_network_capture(owner, operation);
        self.install_resource_origin(
            owner.thread,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm: owner.mm,
                control: ControlCapability::None,
            },
        )
        .unwrap();
        let mut request = Resources::new(owner.thread);
        request.insert(
            ResourceID::BlockedExternalContinue(operation),
            Permission::RW,
        );
        let req = self.next_turns[&owner.thread].req.clone();
        self.request_put(&req, request, global);
        self.step2c_process_io_blockers().unwrap();
        self.bump_global_time(global, &Err(super::SkipTurn));
        let (tid, request, response) = self.step3_peek().unwrap();
        assert_eq!(tid, owner.thread);
        let request = request.try_read().unwrap().unwrap();
        self.step4_resource_block(tid, &request, &response).unwrap();
        self.step5_guest_unblock(tid, &request, &response).unwrap();
        self.step6_reenquue(tid, false);
        assert!(matches!(
            response.try_read(),
            Some(super::SchedResponse::Go(_))
        ));
        assert!(self.ordinary_fd_observation(owner).is_ok());
    }
}
