/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Borrowed authority for an FD observation inside an already granted turn.
//! No resource, response, turn, queue entry or physical completion is created.

use super::SchedRequest;
use super::SchedResponse;
use super::Scheduler;
use super::ThreadNextTurn;
use super::parked::NextTurnOwner;
use super::parked::ProtocolFailure;
use crate::ivar::Ivar;
use crate::network_replay::NetworkStreamOwner;
use crate::types::DetTid;
use crate::types::MmId;

/// Why the existing foreground execution gate is open. None of these outcomes
/// is a result or selection receipt for a native syscall.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OrdinaryFdResume {
    /// The scheduler emitted its original foreground Go or GoFdRead response.
    Normal,
    /// The scheduler emitted Signaled, allowing actual handler execution.
    SignalResume,
    /// The actual parked observation handed back its caught-signal gate.
    ReturningCaught,
}

/// Private identity of the empty transport installed by the real grant. An
/// old record is inert after any request fill, replacement, or epoch change.
#[derive(Clone, Debug)]
pub(super) struct ForegroundFdGrant {
    owner: NetworkStreamOwner,
    epoch: u64,
    request: Ivar<SchedRequest>,
    response: Ivar<SchedResponse>,
    resume: OrdinaryFdResume,
}

impl ForegroundFdGrant {
    fn matches(&self, turn: &ThreadNextTurn, owner: NetworkStreamOwner) -> bool {
        self.owner == owner
            && self.epoch == turn.protocol.epoch
            && self.request == turn.req
            && self.response == turn.resp
    }
}

/// A nonserializable, non-cloneable proof borrowed from the scheduler guard.
/// Keep the guard held through actual metadata -> engine admission. This
/// authorizes only an observation; it cannot certify a syscall return/selection.
#[derive(Debug)]
pub(crate) struct OrdinaryFdObservation<'a> {
    grant: &'a ForegroundFdGrant,
}

impl OrdinaryFdObservation<'_> {
    /// Exact logical task and MM associated with this real foreground grant.
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.grant.owner
    }
    /// Existing turn-protocol epoch; querying it does not advance a turn.
    pub(crate) fn epoch(&self) -> u64 {
        self.grant.epoch
    }
    /// Original handback kind, never a native syscall completion.
    pub(crate) fn resume(&self) -> OrdinaryFdResume {
        self.grant.resume
    }
}

impl Scheduler {
    // This is the exact empty gate which are_all_quiesced waits on. Merely
    // being live, last granted, or present in the run queue is insufficient.
    fn ordinary_fd_gate(&self, tid: DetTid) -> bool {
        !self.backend_failed()
            && !self.thread_is_logically_killed(tid)
            && self.run_queue.contains_tid(tid)
            && !self.pending_run_queue_removals.contains_key(&tid)
            && !self.pending_run_queue_admissions.contains_key(&tid)
            && !self.blocked.external_io_blockers.contains_key(&tid)
            && !self.blocked.rt_sigsuspend_blockers.contains_key(&tid)
            && !self.network_capture_blockers.contains_key(&tid)
            && self.next_turns.get(&tid).is_some_and(|turn| {
                turn.dettid == tid
                    && !matches!(turn.protocol.owner, NextTurnOwner::Observation { .. })
                    && turn.protocol.origin.is_none()
                    && turn.req.try_read().is_none()
                    && turn.resp.try_read().is_none()
            })
    }

    /// Validate a strict-mode no-resource observation in the current real
    /// foreground gate. NoSeq consumers must use their separate engine path.
    /// The caller also validates actual registered metadata/MM under the same
    /// scheduler -> metadata -> engine lock order before consuming a reader.
    pub(crate) fn ordinary_fd_observation(
        &self,
        owner: NetworkStreamOwner,
    ) -> Result<OrdinaryFdObservation<'_>, ProtocolFailure> {
        if !self.rpc_incarnation_matches(owner.thread, owner.mm) {
            return Err(ProtocolFailure::Identity);
        }
        let turn = self
            .next_turns
            .get(&owner.thread)
            .ok_or(ProtocolFailure::Identity)?;
        let grant = turn
            .protocol
            .foreground_fd
            .as_ref()
            .ok_or(ProtocolFailure::Phase)?;
        if grant.owner != owner {
            return Err(ProtocolFailure::Identity);
        }
        if !grant.matches(turn, owner) || !self.ordinary_fd_gate(owner.thread) {
            return Err(ProtocolFailure::Phase);
        }
        Ok(OrdinaryFdObservation { grant })
    }

    // Read before clear_nextturn discards the request origin. A signal-only
    // replacement can legitimately have no new origin: the retained previous
    // real grant supplies its MM, never an arbitrary incoming RPC's claim.
    pub(super) fn ordinary_fd_grant_mm(&self, tid: DetTid) -> Option<MmId> {
        let turn = self.next_turns.get(&tid)?;
        turn.protocol
            .origin
            .map(|origin| origin.mm)
            .or_else(|| {
                turn.protocol
                    .foreground_fd
                    .as_ref()
                    .map(|grant| grant.owner.mm)
            })
            .filter(|mm| self.rpc_incarnation_matches(tid, *mm))
    }

    pub(super) fn record_ordinary_fd_grant(
        &mut self,
        owner: NetworkStreamOwner,
        resume: OrdinaryFdResume,
    ) {
        // External Go removes this task from the run queue before unblocking.
        // ObserveSignal likewise never uses this issuer. No false authority is
        // made from the fact that either response wakes an RPC future.
        if !self.ordinary_fd_gate(owner.thread) {
            return;
        }
        let turn = self
            .next_turns
            .get_mut(&owner.thread)
            .expect("validated foreground gate");
        turn.protocol.foreground_fd = Some(ForegroundFdGrant {
            owner,
            epoch: turn.protocol.epoch,
            request: turn.req.clone(),
            response: turn.resp.clone(),
            resume,
        });
    }

    /// Transfer an already granted same-thread execution gate at the existing
    /// authenticated successful-exec handback. A new/nonleader registration
    /// cannot inherit it. Failure/rollback never calls this handback.
    pub(crate) fn rebind_ordinary_fd_exec(
        &mut self,
        before: NetworkStreamOwner,
        after: NetworkStreamOwner,
    ) {
        if before.thread != after.thread
            || self
                .registered_process(after.thread)
                .is_none_or(|pid| before.mm.for_exec(pid) != after.mm)
            || !self.rpc_incarnation_matches(after.thread, after.mm)
            || !self.ordinary_fd_gate(after.thread)
        {
            return;
        }
        let resume = self.next_turns.get(&before.thread).and_then(|turn| {
            turn.protocol
                .foreground_fd
                .as_ref()
                .filter(|grant| grant.matches(turn, before))
                .map(|grant| grant.resume)
        });
        if let Some(resume) = resume {
            self.record_ordinary_fd_grant(after, resume);
        }
    }
}

impl Scheduler {
    /// Narrow positive grant for an unchanged native initial root. The actual
    /// census projection, backend registration and normal foreground transport
    /// must all agree; no queue-length or host-ready heuristic issues authority.
    pub(crate) fn foreground_native_observation(
        &self,
        owner: NetworkStreamOwner,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<OrdinaryFdObservation<'_>> {
        let bad = || std::io::Error::other("foreground ctl lacks unchanged sole native root grant");
        let grant = self.ordinary_fd_observation(owner).map_err(|_| bad())?;
        if grant.resume() != OrdinaryFdResume::Normal {
            return Err(bad());
        }
        self.validate_native_initial_root(owner, root)?;
        Ok(grant)
    }

    /// Common unchanged census/registration predicate. This grants no turn or
    /// external request; each caller must separately prove its actual phase.
    pub(super) fn validate_native_initial_root(
        &self,
        owner: NetworkStreamOwner,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<()> {
        use crate::network_runtime::native_birth_outcome::NativeTaskProjection;
        let bad = || std::io::Error::other("native observation lacks unchanged sole initial root");
        if !root.is_current(owner)
            || root.owner() != owner
            || self.thread_tree.root != Some(owner.thread)
            || self.thread_tree.tree.len() != 1
            || self.physical_thread_pidfds.len() != 1
            || self
                .thread_tree
                .tree
                .get(&owner.thread)
                .is_none_or(|children| !children.is_empty())
            || self.thread_tree.process_wait.len() != 1
        {
            return Err(bad());
        }
        let process = self.registered_process(owner.thread).ok_or_else(bad)?;
        let (mm, raw_process, raw_thread, pin, _) = self
            .physical_thread_pidfds
            .get(&owner.thread)
            .ok_or_else(bad)?;
        if *mm != owner.mm
            || *raw_process != root.association().process()
            || raw_process != raw_thread
        {
            return Err(bad());
        }
        let registration = super::InitialRootRegistration {
            owner,
            process,
            raw_process: *raw_process,
            _pin: pin,
        };
        let projection =
            NativeTaskProjection::from_initial_root(root.association(), &registration)?;
        let entry = self
            .thread_tree
            .process_wait
            .get(&process)
            .ok_or_else(bad)?;
        if entry.reaped
            || entry.wait_parent.is_some()
            || entry.wait_owner != owner.thread
            || entry.native_birth_parent.is_some()
            || !entry.births.is_empty()
            || !entry.historical_births.is_empty()
            || !entry.birth_sequences.is_empty()
            || entry.native_projections.len() != 1
            || *entry.native_projections[0] != *projection
        {
            return Err(bad());
        }
        Ok(())
    }
}

impl Scheduler {
    pub(crate) fn foreground_epoll_observation(
        &self,
        owner: NetworkStreamOwner,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<OrdinaryFdObservation<'_>> {
        self.foreground_native_observation(owner, root)
    }
}

#[cfg(test)]
impl Scheduler {
    /// Component fixture using the actual initial projection and turn issuer.
    /// Controlled census metadata is not a native provider qualification.
    pub(crate) fn controlled_foreground_store_grant(
        &mut self,
        root: &crate::network_runtime::ForegroundRoot,
    ) {
        use super::parked::ControlCapability;
        use super::parked::ResourceOrigin;
        use super::parked::RpcOrigin;
        use crate::resources::Resources;
        let owner = root.owner();
        if !self.next_turns.contains_key(&owner.thread) {
            self.thread_tree.add_child(owner.thread, owner.thread, true);
            self.next_turns.insert(
                owner.thread,
                ThreadNextTurn {
                    dettid: owner.thread,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            self.priorities
                .insert(owner.thread, super::DEFAULT_PRIORITY);
            self.runqueue_push_back(owner.thread);
            self.install_test_exec_incarnation(owner.thread, owner.mm);
            let raw = root.association().process();
            self.register_physical_thread(owner.thread, owner.mm, raw, raw)
                .unwrap();
            let pin = self.physical_thread_pidfds[&owner.thread]
                .3
                .try_clone()
                .unwrap();
            self.admit_initial_native_root(root.association(), &pin, None, |_| Ok(()))
                .unwrap();
        }
        self.install_resource_origin(
            owner.thread,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm: owner.mm,
                control: ControlCapability::None,
            },
        )
        .unwrap();
        let req = self.next_turns[&owner.thread].req.clone();
        self.request_put(
            &req,
            Resources::new(owner.thread),
            &std::sync::Arc::new(std::sync::Mutex::new(crate::types::GlobalTime::new(
                &crate::config::Config::default(),
            ))),
        );
        let (tid, request, response) = self.step3_peek().unwrap();
        assert_eq!(tid, owner.thread);
        let request = request.try_read().unwrap().unwrap();
        self.step4_resource_block(tid, &request, &response).unwrap();
        self.step5_guest_unblock(tid, &request, &response).unwrap();
        self.step6_reenquue(tid, false);
        assert!(self.foreground_native_observation(owner, root).is_ok());
    }
}
