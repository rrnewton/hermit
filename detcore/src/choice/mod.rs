/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Experimental: choice points answered by an external controller.
//!
//! Under `--chaos`, Detcore resolves scheduling nondeterminism itself, from the
//! per-thread `chaos_prng`. Each such resolution is a *choice point*. With one
//! of `--choice-tape` or `--choice-controller`, Detcore still draws the value
//! it would have used (the *default*, so the seeded stream is consumed exactly
//! as before) and then asks a [`ChoiceSource`] for the value to use instead.
//!
//! Contract of a [`ChoiceSource`]:
//! - Promises: an answer lies in the request's `[lo, hi]` and depends only on
//!   the requests this run has made so far and the source's own configuration.
//! - May assume: requests arrive one at a time, in run order, with consecutive
//!   indices from 0. Hermit serializes the guest, so the order is a function
//!   of the previous answers and the guest's inputs.
//! - May refuse: a request it cannot answer (a controller that replies
//!   `"refuse"`, or whose socket fails). The refusal names the index; the run
//!   stops there, which localizes a first divergence to one choice.
//!
//! A source that always answers the default reproduces the seeded run, so an
//! empty tape is the ordinary `--chaos --sched-seed` run.
//!
//! Structure:
//! - [`policy`]: how a run resolves its scheduling choices, chosen once, by
//!   type, when the run is built ([`crate::RunMode`]). [`SeededChoices`]
//!   returns Detcore's own draw and does nothing else; [`ControlledChoices`]
//!   asks the run's session.
//! - [`session`]: a run's [`ChoiceSession`] (its source, the next index and
//!   the record), owned by the run's global state and handed to each thread
//!   when it starts.
//! - [`source`]: the tape and the controller socket, and the wire format.
//!
//! Scope: the ptrace backend, where the session's handle reaches every thread
//! in process; `hermit run` refuses the options on the other backends. The
//! wired choice points ([`ChoiceSite`]): `chaos_slice` (a thread's timeslice
//! length), `priority` (a thread's priority redraw), `child_priority` (a new
//! thread's starting priority) and `vfork_child_priority` (a vfork child's).
//! Detcore's other seeded chaos draws are not choice points: the wake
//! thresholds of `--chaos-timed-wait-spread`, the per-thread mean of
//! `--chaos-timeslice-range`, and `--chaos-instruction-offset`.
//! `--chaos-target-races`, which sets a new thread's priority itself,
//! conflicts with the options. `--choice-record` writes every request and its
//! answer as JSON lines.
//!
//! Per run: the run's global state builds its session when it is built, so
//! each run (each of `--verify`'s two runs, for example) reads its tape from
//! the start and writes its record from index 0. A controller's answer has no
//! deadline: Hermit waits for it, so a run is bounded from outside (for
//! example by `bin/safehermit`'s deadline).
//!
//! Wire format, version [`SCHEMA`]: a request is one JSON object,
//! `{"schema":1,"i":N,"kind":K,"lo":L,"hi":H,"default":D,"scale":S,
//! "ctx":{"dtid":T,"thread_rcbs":R,"site":W}}`; a record line is the request
//! with `"v":V`, the answer used, appended, or `"refused":REASON` for the
//! request that stopped the run. A controller replies with one of
//! `{"i":N,"v":V}`, `{"i":N,"default":true}` or `{"i":N,"refuse":"reason"}`;
//! a reply with any other key (a `"capture"` command, for one) is refused by
//! that key's name.

pub mod policy;
pub mod session;
pub mod source;

pub use policy::ChoiceHandle;
pub use policy::ControlledChoices;
pub use policy::DrawContext;
pub use policy::PreparedChildPriority;
pub use policy::RunChoices;
pub use policy::SchedulingChoices;
pub use policy::SeededChoices;
pub use session::ChoiceSession;
pub use source::SocketSource;
pub use source::TapeSource;
pub use source::request_json;

use crate::types::DetTid;

/// The version of the request and record JSON lines (see the module docs).
pub const SCHEMA: u32 = 1;

/// What a choice point decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    /// The length, in RCBs, of the timeslice a thread is about to run.
    SchedSlice,
    /// The priority a thread takes at a chaos priority redraw (its own, or a
    /// child's at creation). Lower runs first.
    SchedPriority,
}

impl ChoiceKind {
    fn name(self) -> &'static str {
        match self {
            ChoiceKind::SchedSlice => "sched.slice",
            ChoiceKind::SchedPriority => "sched.priority",
        }
    }
}

/// The draw site that asks: where in Detcore a choice point is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceSite {
    /// A thread's next timeslice length.
    ChaosSlice,
    /// A thread's own chaos priority redraw.
    Priority,
    /// A new (cloned or forked) thread's starting priority, drawn by its parent.
    ChildPriority,
    /// A vfork child's starting priority, drawn by its parent.
    VforkChildPriority,
}

impl ChoiceSite {
    /// The site's name in requests and records, which is also the label of
    /// the chaos draw it follows (`CHAOSRAND(<name>)`), where there is one.
    pub fn name(self) -> &'static str {
        match self {
            ChoiceSite::ChaosSlice => "chaos_slice",
            ChoiceSite::Priority => "priority",
            ChoiceSite::ChildPriority => "child_priority",
            ChoiceSite::VforkChildPriority => "vfork_child_priority",
        }
    }
}

/// One choice point, as the controller sees it.
#[derive(Debug, Clone)]
pub struct ChoiceRequest {
    /// Position in this run's choice sequence, from 0.
    pub index: u64,
    /// What is being decided.
    pub kind: ChoiceKind,
    /// Smallest admissible answer.
    pub lo: u64,
    /// Largest admissible answer.
    pub hi: u64,
    /// The value Detcore would use without a controller.
    pub default: u64,
    /// A natural magnitude for the decision (the mean slice length for
    /// [`ChoiceKind::SchedSlice`]), so byte encodings can be scale relative.
    pub scale: u64,
    /// The thread that drew: for a child's starting priority (`child_priority`,
    /// `vfork_child_priority`), its parent.
    pub dtid: DetTid,
    /// That thread's committed retired-branch count when it asked: its own
    /// position in the run. (The global scheduler turn is not visible from the
    /// thread side; a later version adds it.)
    pub thread_rcbs: u64,
    /// Which draw site asked.
    pub site: ChoiceSite,
}

/// A source's refusal to answer, naming the request it refused.
#[derive(Debug, Clone)]
pub struct ChoiceRefusal {
    /// The index of the refused request.
    pub index: u64,
    /// Why the source refused, for the run's error message.
    pub reason: String,
}

/// Answers choice requests. See the module documentation for the contract.
pub trait ChoiceSource: Send {
    /// The value to use for `req`, within `[req.lo, req.hi]`, or a refusal.
    fn answer(&mut self, req: &ChoiceRequest) -> Result<u64, ChoiceRefusal>;
}

/// A choice point as a draw site describes it: a [`ChoiceRequest`] without
/// its index, which the session assigns ([`ChoiceSession::choose`]).
#[derive(Debug, Clone, Copy)]
pub struct ChoicePoint {
    /// What is being decided.
    pub kind: ChoiceKind,
    /// Smallest admissible answer.
    pub lo: u64,
    /// Largest admissible answer.
    pub hi: u64,
    /// The value Detcore would use without a controller.
    pub default: u64,
    /// A natural magnitude for the decision ([`ChoiceRequest::scale`]).
    pub scale: u64,
    /// The thread that drew: for a child's starting priority (`child_priority`,
    /// `vfork_child_priority`), its parent.
    pub dtid: DetTid,
    /// That thread's committed retired-branch count.
    pub thread_rcbs: u64,
    /// Which draw site asked.
    pub site: ChoiceSite,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A request for the submodules' tests.
    pub(crate) fn req(kind: ChoiceKind, default: u64) -> ChoiceRequest {
        ChoiceRequest {
            index: 0,
            kind,
            lo: 1,
            hi: 10_000,
            default,
            scale: 1000,
            dtid: DetTid::from_raw(3),
            thread_rcbs: 0,
            site: ChoiceSite::Priority,
        }
    }

    #[test]
    fn site_names_are_the_chaos_draw_labels() {
        assert_eq!(ChoiceSite::ChaosSlice.name(), "chaos_slice");
        assert_eq!(ChoiceSite::Priority.name(), "priority");
        assert_eq!(ChoiceSite::ChildPriority.name(), "child_priority");
        assert_eq!(
            ChoiceSite::VforkChildPriority.name(),
            "vfork_child_priority"
        );
    }
}
