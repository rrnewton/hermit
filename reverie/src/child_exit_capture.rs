/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Optional asynchronous ownership of an authentic child-exit notification.
//! This neither synthesizes a signal nor authorizes a Tool signal callback.

use std::fmt::Debug;

use async_trait::async_trait;

use crate::Error;
use crate::Pid;

/// One committed scheduler choice, independent of later scheduler epochs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildExitCaptureRequest {
    /// Logical single-task parent whose backend callback is parked.
    pub parent: Pid,
    /// Logical normal-fork child selected for its final Exit grant.
    pub child: Pid,
    /// Never-reused run-local grant reservation.
    pub reservation: u64,
    /// Physical process identity already registered by the parent Runtime.
    pub parent_physical_pid: i32,
    /// Physical process identity already registered by the child Runtime.
    pub child_physical_pid: i32,
    /// Shared WaitChild proves a completed syscall tail. Other parked resource
    /// requests need an owned signal callback; an arbitrary original syscall
    /// must not run through the release-intent guard.
    pub allow_syscall_callback: bool,
}

/// Arm's boundary between a proved unsupported state and an accepted interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildExitCaptureArm {
    /// The native parent physically excludes SIGCHLD before child continuation.
    Armed,
    /// A positively established structural refusal, with no native effect.
    /// The required capture is unsatisfied: retain the reply reservation for
    /// terminal abort, rather than releasing the child without capture. This
    /// does not classify a pre-reservation event as needing no capture.
    Unsupported,
}

/// Actual mutation of the existing native pending queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildExitCaptureEffect {
    /// A new pending allocation was linked.
    Linked,
    /// The authentic incoming record coalesced with an existing queued record.
    Coalesced,
    /// Existing native policy replaced a queued allocation.
    Replaced,
}

/// Identity of the actual owned SIGCHLD Tool admission. This is not a capture
/// receipt or a routing predicate; it may credit only the matching real hook.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize
)]
pub struct ChildExitSignalAdmission {
    /// Authenticated backend image, changed by successful exec or fork reset.
    pub image_epoch: u64,
    /// Stable native pending-domain identity within the image.
    pub domain: u64,
    /// The detached allocation, including an older coalescence target.
    pub allocation: u64,
    /// Exact live native admission token for this Tool borrow.
    pub token: u64,
}

/// The current owner of one authentic pending allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildExitAllocationState {
    /// Pending in the native queue.
    Queued,
    /// Detached for the current Tool admission.
    Active,
    /// Retained by the native return continuation.
    Returning,
}

/// One live allocation, identified without exporting an address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildExitCaptureAllocation {
    /// Never-reused native allocation generation.
    pub generation: u64,
    /// Exact owner at the protected snapshot boundary.
    pub state: ChildExitAllocationState,
}

/// A complete protected native-domain observation after Release acknowledgement.
/// It is a later observation, not an assertion that acknowledgement and snapshot
/// are simultaneous. Absence never proves delivery of an unresolved obligation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildExitCaptureSnapshot {
    /// Authenticated backend image containing this domain.
    pub image_epoch: u64,
    /// Monotonic mutation version, not a count of delivered signals.
    pub version: u64,
    /// All queued, active and returning allocations; no truncation is permitted.
    pub allocations: Vec<ChildExitCaptureAllocation>,
}

/// The backend authenticated both publication and this interval's exact input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildExitCaptureOutcome {
    /// The original selected grant, not a host PID or current scheduler turn.
    pub request: ChildExitCaptureRequest,
    /// Stable native pending-domain identity.
    pub domain: u64,
    /// Native publication interval identity within that domain.
    pub interval: u64,
    /// Allocation generation participating in the outcome, never its address.
    pub allocation: u64,
    /// Existing pending-queue outcome, not a promise of one hook per child.
    pub effect: ChildExitCaptureEffect,
    /// Exact removed victim for Replaced only; never inferred from absence.
    pub replaced_allocation: Option<u64>,
    /// Complete post-Release ownership while the ordinary reply remains held.
    pub snapshot: ChildExitCaptureSnapshot,
}

/// Synchronous reservation is called under the scheduler mutex. It must only
/// reserve already-authenticated endpoint state; no I/O, Tool call or wait.
pub trait BackendChildExitCapture: Send + Sync + Debug {
    /// Missing/closed endpoints in a scheduler-proved parked parent are errors.
    fn reserve(
        &self,
        request: ChildExitCaptureRequest,
    ) -> Result<Box<dyn ReservedChildExitCapture>, Error>;
}

/// A single consuming transaction. Every await happens outside the scheduler
/// mutex. Drop aborts its reserved parent response; it never reports capture.
#[async_trait]
pub trait ReservedChildExitCapture: Send + Debug {
    /// Arm before exposing the child's Exit response.
    async fn arm(&mut self) -> Result<ChildExitCaptureArm, Error>;

    /// After the child may exit, authenticate publication and consume native
    /// Release intent while retaining the ordinary parent reply reservation.
    /// Called once; neither waits for Tool admission nor completes that reply.
    async fn capture(&mut self) -> Result<ChildExitCaptureOutcome, Error>;

    /// Release the ordinary parent reply only after the shared Tool accounts
    /// for the authentic outcome. Called outside its scheduler mutex. A failed
    /// capture or abandoned transaction must abort through Drop, not release.
    fn release(self: Box<Self>) -> Result<(), Error>;
}
