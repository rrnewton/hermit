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

#[cfg(test)]
#[path = "ordinary_fd/shared_attempt_tests.rs"]
mod shared_attempt_tests;

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
    sole_initial_root: Option<&'a crate::network_runtime::ForegroundRoot>,
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
    /// Only the full retained census/history check below fills this borrow.
    /// A generic Normal grant cannot be promoted by the engine.
    pub(crate) fn admits_sole_initial_root(
        &self,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> bool {
        self.sole_initial_root.is_some_and(|retained| {
            std::ptr::eq(retained, root) && root.is_sole_initial_root(self.owner())
        }) && self.resume() == OrdinaryFdResume::Normal
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
        Ok(OrdinaryFdObservation {
            grant,
            sole_initial_root: None,
        })
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
    /// V4's existing sole-initial-root policy, distinct from generic task/epoll
    /// identity. The borrow keeps the scheduler census fixed through admission.
    pub(crate) fn foreground_native_observation<'a>(
        &'a self,
        owner: NetworkStreamOwner,
        root: &'a crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<OrdinaryFdObservation<'a>> {
        let bad = || std::io::Error::other("foreground ctl lacks unchanged sole native root grant");
        let mut grant = self.ordinary_fd_observation(owner).map_err(|_| bad())?;
        if grant.resume() != OrdinaryFdResume::Normal {
            return Err(bad());
        }
        self.validate_native_initial_root(owner, root)?;
        grant.sole_initial_root = Some(root);
        Ok(grant)
    }

    pub(super) fn validate_native_initial_root(
        &self,
        owner: NetworkStreamOwner,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<()> {
        use crate::network_runtime::native_birth_outcome::NativeTaskProjection;
        let bad = || std::io::Error::other("native observation lacks unchanged sole initial root");
        self.validate_native_foreground_task(owner, root)?;
        if !root.is_sole_initial_root(owner)
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

    /// Common unchanged census/registration predicate. This grants no turn or
    /// external request; each caller must separately prove its actual phase.
    pub(super) fn validate_native_foreground_task(
        &self,
        owner: NetworkStreamOwner,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<()> {
        let bad = || std::io::Error::other("native observation lacks an unchanged task projection");
        if !root.is_current(owner)
            || root.owner() != owner
            || !self.thread_tree.tree.contains_key(&owner.thread)
        {
            return Err(bad());
        }
        let process = self.registered_process(owner.thread).ok_or_else(bad)?;
        let (mm, raw_process, raw_thread, _pin, _) = self
            .physical_thread_pidfds
            .get(&owner.thread)
            .ok_or_else(bad)?;
        if *mm != owner.mm || *raw_process != root.process() || *raw_thread != root.thread() {
            return Err(bad());
        }
        let entry = self
            .thread_tree
            .process_wait
            .get(&process)
            .ok_or_else(bad)?;
        let identity = root.native_identity();
        let Some(projection) = entry
            .native_projections
            .iter()
            .find(|projection| projection.thread() == owner.thread)
        else {
            return Err(bad());
        };
        if entry.reaped
            || !projection.matches_foreground_identity(owner, identity)
            || entry
                .native_projections
                .iter()
                .any(|other| !other.same_process(projection))
        {
            return Err(bad());
        }
        Ok(())
    }
}

/// Current Normal grant joined to a complete physical shared-MM/files census.
/// Neither this borrow nor a root proves that backend peers are stopped.
#[derive(Debug)]
pub(crate) struct SharedMmForegroundObservation<'a> {
    grant: OrdinaryFdObservation<'a>,
    lineage: &'a crate::network_runtime::SharedForegroundLineage<'a>,
}
impl SharedMmForegroundObservation<'_> {
    pub(crate) fn owner(&self) -> NetworkStreamOwner { self.grant.owner() }
    pub(crate) fn epoch(&self) -> u64 { self.grant.epoch() }
    pub(crate) fn root(&self) -> &std::sync::Arc<crate::network_runtime::ForegroundRoot> { self.lineage.root() }
    pub(crate) fn contains_root(
        &self,
        root: &std::sync::Arc<crate::network_runtime::ForegroundRoot>,
    ) -> bool {
        self.lineage
            .members()
            .any(|actual| std::sync::Arc::ptr_eq(actual, root))
    }
}
impl Scheduler {
    /// Retained historical identity, not a final-wait issuer. Cleanup may have
    /// removed the live registration; the physical callback joins its owner.
    pub(crate) fn shared_terminal_projection(
        &self,
        owner: NetworkStreamOwner,
        process: crate::types::DetPid,
    ) -> std::io::Result<
        Option<std::sync::Arc<crate::network_runtime::native_birth_outcome::NativeTaskProjection>>,
    > {
        let bad = || std::io::Error::other("shared final wait changed historical process/task");
        let entry = self
            .thread_tree
            .process_wait
            .get(&process)
            .ok_or_else(bad)?;
        let mut matches = entry
            .native_projections
            .iter()
            .filter(|p| p.thread() == owner.thread);
        let projection = matches.next().ok_or_else(bad)?;
        if entry.reaped
            || matches.next().is_some()
            || projection.process() != process
            || entry
                .native_projections
                .iter()
                .any(|p| !p.same_process(projection))
        {
            return Err(bad());
        }
        Ok((!projection.is_initial()).then(|| projection.clone()))
    }

    pub(crate) fn shared_mm_foreground_observation<'a>(
        &'a self,
        owner: NetworkStreamOwner,
        lineage: &'a crate::network_runtime::SharedForegroundLineage<'a>,
    ) -> std::io::Result<SharedMmForegroundObservation<'a>> {
        let bad = || std::io::Error::other("shared attempt lacks current Normal grant and complete native census");
        let grant = self.ordinary_fd_observation(owner).map_err(|_| bad())?;
        if grant.resume() != OrdinaryFdResume::Normal || lineage.root().owner() != owner {
            return Err(bad());
        }
        let mut members = std::collections::BTreeSet::new();
        for root in lineage.members() {
            self.validate_native_foreground_task(root.owner(), root)?;
            if !root.has_shared_mm_history() || !members.insert(root.owner().thread) {
                return Err(bad());
            }
        }
        let registered: std::collections::BTreeSet<_> = self.physical_thread_pidfds.keys().copied().collect();
        if members != registered || self.thread_tree.process_wait.len() != 1
            || !self.pending_physical_process_exits.is_empty()
        { return Err(bad()); }
        let process = self.registered_process(owner.thread).ok_or_else(bad)?;
        let entry = self.thread_tree.process_wait.get(&process).ok_or_else(bad)?;
        let initial = lineage.root().initial_ancestor();
        if !members.contains(&initial.owner().thread)
            || !initial.is_current(initial.owner())
            || !lineage
                .members()
                .any(|root| std::ptr::eq(root.as_ref(), initial))
        {
            return Err(bad());
        }
        let mut history = std::collections::BTreeSet::new();
        if entry.reaped || !entry.births.is_empty()
            || entry.historical_births.iter().any(|birth| !birth.complete())
        { return Err(bad()); }
        for projection in &entry.native_projections {
            let tid = projection.thread();
            if !history.insert(tid) {
                return Err(bad());
            }
            if members.contains(&tid) {
                continue;
            }
            if projection.completed_final_wait(initial).is_none()
                || self.next_turns.contains_key(&tid)
                || !matches!(self.thread_status(tid), super::ThreadStatus::Gone)
                || self.pending_run_queue_removals.contains_key(&tid)
                || self.pending_cross_task_signals.contains_key(&tid)
                || self.network_capture_blockers.contains_key(&tid)
                || self.replay_connect.contains_key(&tid)
                || self.blocked.timed_out_futex_waiters.contains(&tid)
                || self.blocked.physical_child_ready.contains(&tid)
                || self.blocked.sigchld_deferred.contains(&tid)
                || self.blocked.sigchld_ready.contains(&tid)
                || self.blocked.zero_stream_waiters.contains_key(&tid)
            {
                return Err(bad());
            }
        }
        // ThreadTree and the native rows are history, not just live members.
        // Missing rows are not silently reclassified by this partition.
        if history != self.thread_tree.tree.keys().copied().collect()
            || !members.is_subset(&history)
        {
            return Err(bad());
        }
        for retired in lineage.retired() {
            if !entry.native_projections.iter().any(|p| {
                p.thread() == retired.owner().thread
                    && p.completed_final_wait(initial)
                        .is_some_and(|root| std::sync::Arc::ptr_eq(root, retired))
            }) {
                return Err(bad());
            }
        }
        Ok(SharedMmForegroundObservation { grant, lineage })
    }
}

impl Scheduler {
    pub(crate) fn foreground_epoll_observation(
        &self,
        owner: NetworkStreamOwner,
        root: &crate::network_runtime::ForegroundRoot,
    ) -> std::io::Result<OrdinaryFdObservation<'_>> {
        let grant = self
            .ordinary_fd_observation(owner)
            .map_err(|_| std::io::Error::other("foreground epoll lacks its actual grant"))?;
        if grant.resume() != OrdinaryFdResume::Normal {
            return Err(std::io::Error::other(
                "foreground epoll lacks a normal grant",
            ));
        }
        self.validate_native_foreground_task(owner, root)?;
        Ok(grant)
    }
}

#[cfg(test)]
impl Scheduler {
    /// Component composition: select a real BlockingNetworkCapture request.
    /// The fixture separately obtains/binds the engine's actual FD reader;
    /// this does not claim a backend syscall or fabricate a GoFdRead receipt.
    pub(crate) fn controlled_selected_network_capture(
        &mut self,
        owner: NetworkStreamOwner,
        operation: crate::resources::ExternalOpId,
    ) {
        use super::parked::ControlCapability;
        use super::parked::ResourceOrigin;
        use super::parked::RpcOrigin;
        use crate::resources::Permission;
        use crate::resources::ResourceID;
        use crate::resources::Resources;
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
            ResourceID::BlockingNetworkCapture(operation),
            Permission::RW,
        );
        let req = self.next_turns[&owner.thread].req.clone();
        self.request_put(
            &req,
            request,
            &std::sync::Arc::new(std::sync::Mutex::new(crate::types::GlobalTime::new(
                &crate::config::Config::default(),
            ))),
        );
        let (tid, request, response) = self.step3_peek().unwrap();
        assert_eq!(tid, owner.thread);
        let request = request.try_read().unwrap().unwrap();
        assert!(matches!(
            self.step4_resource_block(tid, &request, &response),
            Err(super::SkipTurn)
        ));
        assert!(matches!(response.try_read(), Some(SchedResponse::Go(_))));
        assert!(self.original_external_grant_matches(owner, operation));
    }

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

#[cfg(test)]
impl Scheduler {
    pub(crate) fn controlled_drain_shared_terminal_removals(&mut self) {
        self.drain_pending_run_queue_removals();
    }

    /// Actual retained birth/projection consumer with a controlled physical
    /// descriptor stand-in. This proves the H join, not a kernel child stop.
    pub(crate) fn controlled_shared_birth_census(
        &mut self,
        parent_root: &crate::network_runtime::ForegroundRoot,
        child_root: &crate::network_runtime::ForegroundRoot,
        admission: &crate::network_runtime::native_birth::NativeBirthAdmission,
    ) {
        let admission = admission.clone();
        let parent = parent_root.owner();
        let child = child_root.owner();
        let prepared = self.thread_tree.prepare_no_seq_birth(
            parent, parent_root.logical_process(),
            crate::resources::ExternalOpId::new(parent.thread, 900),
            (admission.flags(), 0, 0), None, Some(admission.permit()),
        ).unwrap();
        let submitted = self.thread_tree.submit_no_seq_birth(&prepared).unwrap();
        let mut inherited = submitted.clone();
        self.thread_tree.rebind_native_birth(&mut inherited).unwrap();
        let outcome = inherited.native_owner.as_ref().unwrap().attach(admission).unwrap();
        assert_eq!(outcome.child(), child);
        inherited.child = Some(child.thread);
        assert!(self.thread_tree.consume_no_seq_birth(&inherited, child.thread));
        assert_eq!(self.thread_tree.join_no_seq_birth(&submitted, child.thread), Some(true));
        self.install_test_exec_incarnation(child.thread, child.mm);
        // The fixture's provider uses controlled task identities. Retain an
        // owned descriptor, rather than claiming that child.thread is an OS TID.
        let pin = self.physical_thread_pidfds[&parent.thread].3.try_clone().unwrap();
        assert!(self.physical_thread_pidfds.insert(child.thread,
            (child.mm, child_root.process(), child_root.thread(), pin, true)).is_none());
    }

    /// Issue the next real Normal foreground request for an already registered
    /// shared parent. The peer remains in the retained task census.
    pub(crate) fn controlled_shared_foreground_grant(
        &mut self, root: &crate::network_runtime::ForegroundRoot,
    ) {
        use super::parked::{ControlCapability, ResourceOrigin, RpcOrigin};
        let owner = root.owner();
        self.install_resource_origin(owner.thread, ResourceOrigin {
            rpc: RpcOrigin::DirectRequestResources, mm: owner.mm, control: ControlCapability::None,
        }).unwrap();
        let req = self.next_turns[&owner.thread].req.clone();
        self.request_put(&req, crate::resources::Resources::new(owner.thread),
            &std::sync::Arc::new(std::sync::Mutex::new(crate::types::GlobalTime::new(&crate::config::Config::default()))));
        let (tid, request, response) = self.step3_peek().unwrap();
        assert_eq!(tid, owner.thread);
        let request = request.try_read().unwrap().unwrap();
        self.step4_resource_block(tid, &request, &response).unwrap();
        self.step5_guest_unblock(tid, &request, &response).unwrap();
        self.step6_reenquue(tid, false);
        assert_eq!(self.ordinary_fd_observation(owner).unwrap().resume(), OrdinaryFdResume::Normal);
    }
}

#[cfg(test)]
impl Scheduler {
    /// Queue a controlled, already authenticated child registration, then use
    /// the real Normal request/response path. This does not issue a birth or
    /// replace the retained native projection installed by the fixture.
    pub(crate) fn controlled_shared_child_grant(
        &mut self,
        root: &crate::network_runtime::ForegroundRoot,
    ) {
        let owner = root.owner();
        assert!(self.physical_thread_pidfds.contains_key(&owner.thread));
        assert!(self.thread_tree.tree.contains_key(&owner.thread));
        assert!(!self.next_turns.contains_key(&owner.thread));
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
        self.runqueue_push_front(owner.thread);
        self.controlled_shared_foreground_grant(root);
    }

    pub(crate) fn controlled_park_shared_wait(
        &mut self,
        owner: NetworkStreamOwner,
        interests: Vec<(
            crate::network_replay::NetworkStreamCallId,
            crate::resources::NetworkWaitKind,
        )>,
        deadline: Option<crate::types::LogicalTime>,
        engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
    ) {
        use super::parked::ControlCapability;
        use super::parked::ResourceOrigin;
        use super::parked::RpcOrigin;
        self.set_network_engine(Some(engine));
        self.install_resource_origin(
            owner.thread,
            ResourceOrigin {
                rpc: RpcOrigin::DirectRequestResources,
                mm: owner.mm,
                control: ControlCapability::None,
            },
        )
        .unwrap();
        let mut request = crate::resources::Resources::new(owner.thread);
        request.insert(
            crate::resources::ResourceID::NetworkCallWaitSet {
                interests: interests.clone(),
                deadline,
                zero_wait: None,
            },
            crate::resources::Permission::RW,
        );
        let req = self.next_turns[&owner.thread].req.clone();
        self.request_put(
            &req,
            request,
            &std::sync::Arc::new(std::sync::Mutex::new(crate::types::GlobalTime::new(
                &crate::config::Config::default(),
            ))),
        );
        // A controlled startup ordering chooses this already granted member.
        self.run_queue.remove_tid(owner.thread);
        self.runqueue_push_front(owner.thread);
        let (tid, request, response) = self.step3_peek().unwrap();
        assert_eq!(tid, owner.thread);
        let request = request.try_read().unwrap().unwrap();
        assert!(matches!(
            self.step4_resource_block(tid, &request, &response),
            Err(super::SkipTurn)
        ));
        assert_eq!(
            self.blocked.network_call_waiters.get(&tid),
            Some(&(owner, interests))
        );
        assert_eq!(self.blocked.timed_waiters.thread_deadline(tid), deadline);
        let turn = self.turn;
        let time = self.committed_time;
        self.step2_network_replay_ready().unwrap();
        assert!(self.blocked.network_call_waiters.contains_key(&tid));
        assert_eq!(self.turn, turn);
        assert_eq!(self.committed_time, time);
        assert!(self.terminal_deadlock.is_none());
    }
}

#[cfg(test)]
impl Scheduler {
    /// Exercise the real registration and maintenance lookups. The engine's
    /// generic readiness is a deliberately conflicting controlled premise;
    /// only the retained Record Pending publication may bind this wait.
    pub(crate) fn controlled_pending_record_poll(
        &mut self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        deadline: crate::types::LogicalTime,
        engine: std::sync::Arc<std::sync::Mutex<crate::network_replay::NetworkReplayEngine>>,
    ) {
        let kind = crate::resources::NetworkWaitKind::PollReadable;
        {
            let engine = engine.lock().unwrap();
            assert_eq!(
                engine.mode(),
                crate::network_replay::NetworkEngineMode::Record
            );
            let binding = engine
                .call_wait_binding(owner, call, kind, Some(deadline))
                .unwrap();
            assert!(Self::network_wait_is_ready(&engine, binding.open_file, kind).unwrap());
            assert_eq!(binding.observed_ready, Some(false));
        }
        let turn = self.turn;
        let time = self.committed_time;
        self.controlled_park_shared_wait(owner, vec![(call, kind)], Some(deadline), engine);
        // Existing NONCOMMIT parking advances the turn index once; it grants
        // no guest execution and adds no committed logical time.
        let parked_turn = turn.checked_add(1).unwrap();
        assert_eq!(self.turn, parked_turn);
        assert_eq!(self.committed_time, time);
        assert!(!self.run_queue.contains_tid(owner.thread));
        assert!(self.next_turns[&owner.thread].resp.try_read().is_none());
        assert_eq!(
            self.blocked.timed_waiters.thread_deadline(owner.thread),
            Some(deadline)
        );
        self.step2_network_replay_ready().unwrap();
        assert_eq!(
            self.blocked.network_call_waiters.get(&owner.thread),
            Some(&(owner, vec![(call, kind)]))
        );
        assert!(!self.run_queue.contains_tid(owner.thread));
        assert!(self.next_turns[&owner.thread].resp.try_read().is_none());
        assert_eq!(
            self.blocked.timed_waiters.thread_deadline(owner.thread),
            Some(deadline)
        );
        assert_eq!(self.turn, parked_turn);
        assert_eq!(self.committed_time, time);
    }
}
