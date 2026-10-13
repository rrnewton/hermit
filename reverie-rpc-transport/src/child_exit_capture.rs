/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exact-parent native publication transactions on existing borrowed RPCs.
//! Scheduling, normal-fork eligibility and child Exit grant ownership stay in
//! the shared Tool. This adapter neither chooses a turn nor synthesizes signals.

use std::fmt;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

use async_trait::async_trait;
use reverie::BackendChildExitCapture;
use reverie::ChildExitCaptureArm;
use reverie::ChildExitCaptureEffect;
use reverie::ChildExitCaptureOutcome;
use reverie::ChildExitCaptureRequest;
use reverie::Error;
use reverie::ReservedChildExitCapture;
use tokio::io::unix::AsyncFd;

use crate::native_control::NativeControlHub;
use crate::native_control::NativeControlOperation;
use crate::native_control::NativeControlReply;
use crate::native_control::NativePeerIdentity;
use crate::native_control::NativeReplyReservation;
use crate::native_control::PendingNativeCall;
use crate::native_control::PublicationHandle;
use crate::native_control::PublicationRequest;
use crate::native_control::SignalDomainSnapshot;

fn failure(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}

/// The hub is the same authenticated RPC registry used by the real Runtime.
pub struct NativeChildExitCapture {
    hub: NativeControlHub,
}

impl NativeChildExitCapture {
    pub fn new(hub: NativeControlHub) -> Self {
        Self { hub }
    }
}

impl fmt::Debug for NativeChildExitCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeChildExitCapture")
            .finish_non_exhaustive()
    }
}

struct Transaction {
    request: ChildExitCaptureRequest,
    parent: PendingNativeCall,
    parent_peer: NativePeerIdentity,
    child_peer: NativePeerIdentity,
    reservation: Option<NativeReplyReservation>,
    armed: Option<PublicationHandle>,
    arm_attempted: bool,
}

impl fmt::Debug for Transaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChildExitCaptureTransaction")
            .field("request", &self.request)
            .field("parent", &self.parent.identity)
            .field("child_peer", &self.child_peer)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

fn authenticated_peer(call: &PendingNativeCall) -> Result<NativePeerIdentity, Error> {
    let peer = call
        .peer_identity()
        .ok_or_else(|| failure("native peer pidfd absent"))?;
    if peer.connection == 0
        || peer.connection != call.identity.connection
        || peer.pid <= 0
        || peer.pid != call.identity.owner.pid
        || call.identity.owner.tid != peer.pid
        || call.identity.owner.image_epoch == 0
        || call.identity.owner.callback_epoch == 0
    {
        return Err(failure(
            "child capture requires an authenticated process-leader callback",
        ));
    }
    Ok(peer)
}

impl BackendChildExitCapture for NativeChildExitCapture {
    fn reserve(
        &self,
        request: ChildExitCaptureRequest,
    ) -> Result<Box<dyn ReservedChildExitCapture>, Error> {
        if request.reservation == 0 || request.parent == request.child {
            return Err(failure("invalid child Exit reservation"));
        }
        let parent = self
            .hub
            .pending(request.parent)
            .ok_or_else(|| failure("selected child's parent has no pending native RPC"))?;
        let child = self
            .hub
            .pending(request.child)
            .ok_or_else(|| failure("selected child has no pending Exit RPC"))?;
        let parent_peer = authenticated_peer(&parent)?;
        let child_peer = authenticated_peer(&child)?;
        if parent_peer.pid == child_peer.pid
            || parent_peer.connection == child_peer.connection
            || parent_peer.pid != request.parent_physical_pid
            || child_peer.pid != request.child_physical_pid
        {
            return Err(failure("child and parent native generations alias"));
        }
        // No I/O here: reserve under the scheduler mutex against the actor's
        // ordinary-reply decision. The pidfds already belong to accepted peers.
        let reservation = parent.reserve_reply(request.reservation).map_err(failure)?;
        Ok(Box::new(Transaction {
            request,
            parent,
            parent_peer,
            child_peer,
            reservation: Some(reservation),
            armed: None,
            arm_attempted: false,
        }))
    }
}

/// A pidfd event means this precise connected process is terminal. It does not
/// prove SIGCHLD publication, raw capture, or Tool admission.
fn terminal(peer: &NativePeerIdentity) -> Result<bool, Error> {
    let mut descriptor = libc::pollfd {
        fd: peer.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(failure("child capture pidfd reported an error"));
    }
    Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}

async fn wait_terminal(peer: &NativePeerIdentity) -> Result<(), Error> {
    // A duplicate avoids registering the registry's descriptor twice in the
    // same reactor. The retained kernel process generation is unchanged.
    let descriptor = AsyncFd::new(peer.pidfd.try_clone()?)?;
    loop {
        if terminal(peer)? {
            return Ok(());
        }
        let mut readiness = descriptor.readable().await?;
        if terminal(peer)? {
            return Ok(());
        }
        readiness.clear_ready();
    }
}

fn namespace_identity(path: &str) -> Result<(u64, u64), Error> {
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

impl Transaction {
    fn validate_namespace_and_liveness(&self) -> Result<(), Error> {
        if terminal(&self.parent_peer)? || terminal(&self.child_peer)? {
            return Err(failure("native capture peer exited before Arm"));
        }
        // Both owner PIDs already equal connect-time SO_PEERCRED. Namespace
        // equality additionally rules out coincidentally equal nested PID values.
        // The child remains held at its selected Exit response during this read.
        let coordinator = namespace_identity("/proc/self/ns/pid")?;
        let parent = namespace_identity(&format!("/proc/{}/ns/pid", self.parent_peer.pid))?;
        let child = namespace_identity(&format!("/proc/{}/ns/pid", self.child_peer.pid))?;
        if coordinator != parent
            || parent != child
            || terminal(&self.parent_peer)?
            || terminal(&self.child_peer)?
        {
            return Err(failure(
                "native capture namespace or peer lifetime changed before Arm",
            ));
        }
        Ok(())
    }

    async fn command(
        &self,
        operation: NativeControlOperation,
    ) -> Result<NativeControlReply, Error> {
        // Peer death need not finish the original Tool future or retire its
        // registry entry. Cancel only this controller wait; the actor retains
        // its uninterrupted frame I/O. The enclosing transaction then drops
        // its reply reservation, aborting the actor without replaying the call.
        let reply = tokio::select! {
            reply = self.parent.command(operation) => reply.map_err(failure)?,
            death = wait_terminal(&self.parent_peer) => {
                death?;
                return Err(failure("parent native process exited during control"));
            }
            _ = self.parent.closed() => {
                return Err(failure("parent native request closed during control"));
            }
        };
        if reply.owner != self.parent.identity.owner || reply.observation_error != 0 {
            return Err(failure("native capture owner/mask observation refused"));
        }
        Ok(reply)
    }

    async fn await_child(&self) -> Result<(), Error> {
        tokio::select! {
            child = wait_terminal(&self.child_peer) => child,
            death = wait_terminal(&self.parent_peer) => {
                death?;
                Err(failure("parent native process exited before child publication"))
            }
            _ = self.parent.closed() => {
                Err(failure("parent native request closed before child publication"))
            }
        }
    }
}

fn terminal_siginfo(words: &[u64; 16], child: i32) -> Result<(i32, i32), Error> {
    const _: () = assert!(std::mem::size_of::<libc::siginfo_t>() == 128);
    // The native ABI retains a full kernel-sized, aligned value, not a pointer
    // into the guest or a formatted log. Only CLD_* interprets the status union.
    let info = unsafe { std::ptr::read_unaligned(words.as_ptr().cast::<libc::siginfo_t>()) };
    if info.si_signo != libc::SIGCHLD
        || !matches!(
            info.si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        )
        || unsafe { info.si_pid() } != child
    {
        return Err(failure(
            "publication receipt is not this child's authentic terminal SIGCHLD",
        ));
    }
    Ok((info.si_code, unsafe { info.si_status() }))
}

fn validate_domain(domain: &SignalDomainSnapshot) -> Result<(), Error> {
    if domain.domain == 0
        || domain.version == 0
        || domain.reserved != 0
        || domain.owner.reserved != 0
        || domain.queued > 2
        || domain.active > 1
        || domain.returning > 1
        || domain.record_count as usize > domain.records.len()
    {
        return Err(failure("native domain snapshot is incomplete"));
    }
    let records = &domain.records[..domain.record_count as usize];
    let mut counts = [0; 3];
    for (index, record) in records.iter().enumerate() {
        if record.generation == 0
            || record.reserved != 0
            || !(1..=3).contains(&record.ownership)
            || records[..index]
                .iter()
                .any(|previous| previous.generation == record.generation)
        {
            return Err(failure("native domain allocation ownership is invalid"));
        }
        counts[record.ownership as usize - 1] += 1;
    }
    if counts != [domain.queued, domain.active, domain.returning]
        || (domain.active != 0 && domain.admission_token == 0)
    {
        return Err(failure(
            "native domain ownership counts disagree with records",
        ));
    }
    Ok(())
}

fn validate_capture(
    reply: &NativeControlReply,
    handle: PublicationHandle,
) -> Result<ChildExitCaptureEffect, Error> {
    let receipt = &reply.publication;
    let domain = &reply.domain;
    validate_domain(domain)?;
    if reply.status != 0
        || reply.phase != 2
        || domain.phase != 2
        || receipt.handle != handle
        || receipt.raw_entries != 1
        || receipt.allocation == 0
        || receipt.version == 0
        || domain.domain != handle.request.domain
        || domain.version != receipt.version
        || domain.interval != handle.interval
        || domain.owner != handle.request.owner
        || domain.owner_pid != handle.owner_pid
        || domain.owner_tid != handle.owner_tid
        || domain.guest_mask != receipt.guest_mask
        || reply.publication_wait_result != 0
        || reply.saved_physical_mask != receipt.saved_physical_mask
        || reply.current_physical_mask != receipt.saved_physical_mask
        || receipt.saved_physical_mask & (1 << (libc::SIGCHLD - 1)) != 0
        || domain.record_count as usize > domain.records.len()
    {
        return Err(failure(
            "native publication/capture receipt is incomplete or stale",
        ));
    }
    let published = terminal_siginfo(
        &reply.publication_wait_siginfo_words,
        handle.request.child_pid,
    )?;
    let incoming = terminal_siginfo(&receipt.incoming_siginfo_words, handle.request.child_pid)?;
    if incoming != published {
        return Err(failure(
            "captured child status differs from actual-parent waitid",
        ));
    }
    let records = &domain.records[..domain.record_count as usize];
    let target = records
        .iter()
        .find(|record| record.generation == receipt.allocation && record.ownership == 1)
        .ok_or_else(|| failure("publication outcome does not name a queued allocation"))?;
    if receipt.outcome != 2 && target.siginfo_words != receipt.incoming_siginfo_words {
        return Err(failure(
            "new allocation did not retain the authentic incoming frame",
        ));
    }
    match receipt.outcome {
        1 if receipt.victim == 0 => Ok(ChildExitCaptureEffect::Linked),
        2 if receipt.victim == 0 => Ok(ChildExitCaptureEffect::Coalesced),
        3 if receipt.victim != 0
            && receipt.victim != receipt.allocation
            && !records
                .iter()
                .any(|record| record.generation == receipt.victim) =>
        {
            Ok(ChildExitCaptureEffect::Replaced)
        }
        _ => Err(failure(
            "native publication has no recognized queue outcome",
        )),
    }
}

#[async_trait]
impl ReservedChildExitCapture for Transaction {
    async fn arm(&mut self) -> Result<ChildExitCaptureArm, Error> {
        if self.arm_attempted || self.reservation.is_none() {
            return Err(failure("child publication Arm was already attempted"));
        }
        self.arm_attempted = true;
        self.validate_namespace_and_liveness()?;
        let observed = self.command(NativeControlOperation::DomainObserve).await?;
        if observed.status == 1 && observed.domain.owner.callback_kind == 3 {
            // An authenticated ordinary Tool borrow can carry RPCs but cannot
            // arm the SDK publication contract. Keep the selected child held;
            // failure cancellation aborts this reservation. Never turn this
            // unavailable domain into the structural-exclusion normal route.
            return Err(failure(
                "selected child Exit refused: parent native callback is ordinary; publication domain unavailable",
            ));
        }
        if observed.status == 6 {
            // Structural refusal does not discharge a selected child's capture
            // requirement. Retain the reply reservation for terminal abort.
            if !matches!(observed.phase, 0 | 3)
                || observed.current_physical_mask != observed.saved_physical_mask
            {
                return Err(failure("unsupported domain observation changed ownership"));
            }
            return Err(failure(format!(
                "selected child Exit refused: DomainObserve status={} phase={} callback_kind={}; required publication domain unsupported",
                observed.status, observed.phase, observed.domain.owner.callback_kind,
            )));
        }
        let domain = observed.domain;
        validate_domain(&domain)?;
        if observed.status != 0
            || domain.domain == 0
            || domain.owner_pid != self.parent_peer.pid
            || domain.owner_tid != self.parent.identity.owner.tid
            || domain.owner.image_epoch != self.parent.identity.owner.image_epoch
            || domain.owner.callback_epoch != self.parent.identity.owner.callback_epoch
        {
            return Err(failure("parent pending domain is unavailable before Arm"));
        }
        if domain.owner.callback_kind == 1 && !self.request.allow_syscall_callback {
            // This is the shared caller's completed-tail precondition, joined to
            // an authenticated actual callback kind, never an endpoint timing test.
            return Err(failure(
                "selected child Exit refused: parent syscall callback lacks the required completed WaitChild tail",
            ));
        }
        if !matches!(domain.owner.callback_kind, 1 | 2)
            || (domain.owner.callback_kind == 2
                && (domain.active != 1 || domain.owner.admission_token != domain.admission_token))
        {
            return Err(failure("parent callback has no supported native owner"));
        }
        let request = PublicationRequest {
            owner: domain.owner,
            domain: domain.domain,
            reservation: self.request.reservation,
            child_generation: self.child_peer.connection,
            child_pid: self.child_peer.pid,
            reserved: 0,
        };
        let reply = self
            .command(NativeControlOperation::PublicationArm { request })
            .await?;
        if reply.status == 6 {
            // Even a non-mutating structural refusal is terminal for this
            // required selection; it cannot authorize an uncaptured Exit.
            if !matches!(reply.phase, 0 | 3)
                || reply.current_physical_mask != observed.current_physical_mask
            {
                return Err(failure("unsupported publication mutated native ownership"));
            }
            return Err(failure(format!(
                "selected child Exit refused: PublicationArm status={} phase={} callback_kind={}; required publication unsupported",
                reply.status, reply.phase, domain.owner.callback_kind,
            )));
        }
        let handle = reply.publication.handle;
        if reply.status != 0
            || reply.phase != 1
            || reply.domain.phase != 1
            || handle.request != request
            || handle.interval == 0
            || handle.owner_pid != self.parent_peer.pid
            || handle.owner_tid != self.parent.identity.owner.tid
            || reply.current_physical_mask
                != (observed.current_physical_mask | (1 << (libc::SIGCHLD - 1)))
        {
            return Err(failure("parent refused exact child publication Arm"));
        }
        self.armed = Some(handle);
        Ok(ChildExitCaptureArm::Armed)
    }

    async fn finish(mut self: Box<Self>) -> Result<ChildExitCaptureOutcome, Error> {
        let handle = self
            .armed
            .ok_or_else(|| failure("unarmed child publication cannot finish"))?;
        self.await_child().await?;
        let captured = self
            .command(NativeControlOperation::PublicationClose { handle })
            .await?;
        let effect = validate_capture(&captured, handle)?;
        let released = self
            .command(NativeControlOperation::PublicationRelease {
                receipt: captured.publication,
            })
            .await?;
        if released.status != 0
            || released.phase != 3
            || released.domain.phase != 3
            || released.publication.handle != handle
            || released.domain.domain != handle.request.domain
            || released.current_physical_mask != captured.publication.saved_physical_mask
        {
            return Err(failure(
                "native publication Release intent was not acknowledged",
            ));
        }
        self.reservation.take().unwrap().finish().map_err(failure)?;
        Ok(ChildExitCaptureOutcome {
            request: self.request,
            domain: handle.request.domain,
            interval: handle.interval,
            allocation: captured.publication.allocation,
            effect,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_control::PublicationOwner;
    use crate::native_control::PublicationReceipt;
    use crate::native_control::SignalDomainRecord;

    fn terminal_frame(pid: i32, status: i32) -> [u64; 16] {
        // Linux x86-64 kernel siginfo: signo/errno, code/pad, pid/uid, status.
        let mut words = [0; 16];
        words[0] = libc::SIGCHLD as u64;
        words[1] = libc::CLD_EXITED as u64;
        words[2] = pid as u32 as u64;
        words[3] = status as u32 as u64;
        words
    }

    fn linked() -> (NativeControlReply, PublicationHandle) {
        let handle = PublicationHandle {
            request: PublicationRequest {
                owner: PublicationOwner {
                    image_epoch: 1,
                    callback_epoch: 2,
                    callback_kind: 1,
                    ..Default::default()
                },
                domain: 88,
                reservation: 9,
                child_generation: 3,
                child_pid: 456,
                reserved: 0,
            },
            interval: 99,
            owner_pid: 123,
            owner_tid: 123,
        };
        let incoming = terminal_frame(456, 31);
        let mut domain = SignalDomainSnapshot {
            owner: handle.request.owner,
            domain: 88,
            version: 4,
            interval: 99,
            owner_pid: 123,
            owner_tid: 123,
            phase: 2,
            queued: 1,
            record_count: 1,
            ..Default::default()
        };
        domain.records[0] = SignalDomainRecord {
            generation: 7,
            ownership: 1,
            siginfo_words: incoming,
            ..Default::default()
        };
        (
            NativeControlReply {
                phase: 2,
                domain,
                publication: PublicationReceipt {
                    handle,
                    version: 4,
                    raw_entries: 1,
                    outcome: 1,
                    allocation: 7,
                    incoming_siginfo_words: incoming,
                    ..Default::default()
                },
                publication_wait_siginfo_words: incoming,
                ..Default::default()
            },
            handle,
        )
    }

    #[test]
    fn exact_child_frame_and_positive_parent_wait_are_both_required() {
        let (good, handle) = linked();
        assert_eq!(
            validate_capture(&good, handle).unwrap(),
            ChildExitCaptureEffect::Linked
        );
        let mut bad = good;
        bad.publication_wait_siginfo_words = terminal_frame(456, 32);
        assert!(validate_capture(&bad, handle).is_err());
        bad = good;
        bad.publication.incoming_siginfo_words = terminal_frame(457, 31);
        assert!(validate_capture(&bad, handle).is_err());
        bad = good;
        bad.publication_wait_result = -(libc::ECHILD as i64);
        assert!(validate_capture(&bad, handle).is_err());
    }

    #[test]
    fn cumulative_arrivals_and_stale_versions_do_not_certify_an_interval() {
        let (good, handle) = linked();
        for count in [0, 2, u64::MAX] {
            let mut bad = good;
            bad.publication.raw_entries = count;
            assert!(validate_capture(&bad, handle).is_err());
        }
        let mut bad = good;
        bad.domain.version += 1;
        assert!(validate_capture(&bad, handle).is_err());
        bad = good;
        bad.domain.records[0].siginfo_words = terminal_frame(456, 30);
        assert!(validate_capture(&bad, handle).is_err());
    }

    #[test]
    fn coalescence_targets_queued_records_never_active_ownership() {
        let (mut reply, handle) = linked();
        reply.publication.outcome = 2;
        reply.domain.records[0].siginfo_words = terminal_frame(444, 20);
        assert_eq!(
            validate_capture(&reply, handle).unwrap(),
            ChildExitCaptureEffect::Coalesced
        );
        reply.domain.records[0].ownership = 2;
        reply.domain.queued = 0;
        reply.domain.active = 1;
        reply.domain.admission_token = 11;
        assert!(validate_capture(&reply, handle).is_err());
    }

    #[test]
    fn duplicate_or_omitted_domain_ownership_is_rejected() {
        let (good, handle) = linked();
        let mut bad = good;
        bad.domain.queued = 2;
        assert!(validate_capture(&bad, handle).is_err());
        bad = good;
        bad.domain.records[1] = bad.domain.records[0];
        bad.domain.record_count = 2;
        bad.domain.queued = 2;
        assert!(validate_capture(&bad, handle).is_err());
        bad = good;
        bad.publication.outcome = 3;
        bad.publication.victim = bad.publication.allocation;
        assert!(validate_capture(&bad, handle).is_err());
    }
}
