/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! How a run resolves its scheduling choices.
//!
//! The policy is a type, chosen once when the run is built
//! ([`crate::RunMode`]), the way the record-or-replay sub-tool is. Every draw
//! site calls it the same way, after making Detcore's own seeded draw:
//! - [`SeededChoices`], the ordinary run's policy, returns that draw as the
//!   decision. It holds nothing and does nothing else: no lock, no
//!   allocation, no source, no message.
//! - [`ControlledChoices`] offers the draw as the default of a choice point
//!   to the run's [`ChoiceSession`], through the handle the thread received
//!   when it started.

use std::sync::Arc;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;

use super::ChoiceKind;
use super::ChoicePoint;
use super::ChoiceSession;
use super::ChoiceSite;
use crate::scheduler::Priority;
use crate::scheduler::entropy_to_priority;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::types::DetTid;

/// The drawing thread's identity and position, for a choice point's
/// request. Plain copies of two thread-state fields.
#[derive(Debug, Clone, Copy)]
pub struct DrawContext {
    /// The thread that draws (for a child's starting priority, its parent).
    pub dettid: DetTid,
    /// That thread's committed retired-branch count.
    pub thread_rcbs: u64,
}

/// A run's scheduling-choice policy. See the module documentation.
pub trait SchedulingChoices: Default + Clone + Send + Sync + std::fmt::Debug + 'static {
    /// Before the first draw of a thread that registers itself (the root,
    /// whose registration draws its own starting priority before its start
    /// grant), take from the run whatever this policy needs. Core calls it
    /// uniformly there; whether anything is needed is the policy's business.
    fn init_thread<M, G>(guest: &mut G) -> impl std::future::Future<Output = ()> + Send
    where
        M: crate::RunMode<Choices = Self>,
        G: reverie::Guest<crate::Detcore<M>>;

    /// Take the run's session handle, which the thread's start grant carries
    /// (`GlobalResponse::StartNewThread`), before the thread's first draw.
    fn attach(&mut self, handle: Option<ChoiceHandle>, dettid: DetTid);

    /// The length, in RCBs, of the thread's next timeslice, given Detcore's
    /// draw `seeded` (already made and logged) around the mean `mean_rcbs`.
    fn slice(&self, ctx: DrawContext, seeded: u64, mean_rcbs: f64) -> u64;

    /// The priority to take at `site`, given Detcore's chaos draw `entropy`
    /// (already made and logged), whose priority is the default.
    fn priority(&self, ctx: DrawContext, site: ChoiceSite, entropy: u64) -> Priority;
}

/// The ordinary run's policy: Detcore's own draw is the decision.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeededChoices;

impl SchedulingChoices for SeededChoices {
    /// Nothing to take: the seeded draws need no session.
    #[inline(always)]
    async fn init_thread<M, G>(_guest: &mut G)
    where
        M: crate::RunMode<Choices = Self>,
        G: reverie::Guest<crate::Detcore<M>>,
    {
    }

    #[inline(always)]
    fn attach(&mut self, _handle: Option<ChoiceHandle>, _dettid: DetTid) {}

    #[inline(always)]
    fn slice(&self, _ctx: DrawContext, seeded: u64, _mean_rcbs: f64) -> u64 {
        seeded
    }

    #[inline(always)]
    fn priority(&self, _ctx: DrawContext, _site: ChoiceSite, entropy: u64) -> Priority {
        entropy_to_priority(entropy)
    }
}

/// The policy of a run with a choice source: each draw becomes a choice
/// point whose default is the draw, answered by the run's session.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ControlledChoices {
    /// The run's session, from the thread's start grant ([`Self::attach`]).
    session: Option<ChoiceHandle>,
}

impl ControlledChoices {
    /// The attached session. A thread that draws before it was attached
    /// would ask no one, so it is refused by name rather than answered with
    /// the seeded draw.
    fn session(&self, dettid: DetTid) -> &ChoiceHandle {
        self.session
            .as_ref()
            .unwrap_or_else(|| super::session::refuse_unattached(dettid))
    }
}

impl SchedulingChoices for ControlledChoices {
    /// A thread not yet attached asks the global state for the run's handle
    /// (`GlobalRequest::ChoiceSession`, which consumes no choice and moves no
    /// scheduler or time state) and attaches it as a start grant's would be.
    async fn init_thread<M, G>(guest: &mut G)
    where
        M: crate::RunMode<Choices = Self>,
        G: reverie::Guest<crate::Detcore<M>>,
    {
        if guest.thread_state().choices.session.is_none() {
            let handle = crate::tool_global::request_choice_session(guest).await;
            let dettid = guest.thread_state().dettid;
            guest.thread_state_mut().choices.attach(handle, dettid);
        }
    }

    /// A handle missing (a backend whose responses cross a serialization
    /// boundary drops the in-process handle, [`ChoiceHandle`]) or naming
    /// another session than the one held is refused by name, never run on
    /// seeded behavior; the held session offered again changes nothing.
    fn attach(&mut self, handle: Option<ChoiceHandle>, dettid: DetTid) {
        match (handle, &self.session) {
            (None, _) => super::session::refuse_unattached(dettid),
            (Some(handle), None) => self.session = Some(handle),
            (Some(handle), Some(held)) if handle == *held => {}
            (Some(_), Some(_)) => super::session::refuse_other_session(dettid),
        }
    }

    // `hi` stays at u32::MAX RCBs, so no answer can push the end of the slice
    // past the logical clock's range. A seeded draw above it is clamped there
    // as the default, and only then does an all-default run differ from the
    // seeded one: never in practice at the default 200 ms maximum (a mean of
    // 2*10^7 RCBs), but in about a third of slices at a mean of u32::MAX RCBs
    // (a --max-timeslice near 43 virtual seconds).
    fn slice(&self, ctx: DrawContext, seeded: u64, mean_rcbs: f64) -> u64 {
        let chosen = self.session(ctx.dettid).choose(ChoicePoint {
            kind: ChoiceKind::SchedSlice,
            lo: 1,
            hi: u64::from(u32::MAX),
            default: seeded,
            scale: mean_rcbs.round().max(1.0) as u64,
            dtid: ctx.dettid,
            thread_rcbs: ctx.thread_rcbs,
            site: ChoiceSite::ChaosSlice,
        });
        crate::detlog!("[dtid {}] CHOICE => next_rcbs = {}", ctx.dettid, chosen);
        chosen
    }

    fn priority(&self, ctx: DrawContext, site: ChoiceSite, entropy: u64) -> Priority {
        let chosen = self.session(ctx.dettid).choose(ChoicePoint {
            kind: ChoiceKind::SchedPriority,
            lo: FIRST_PRIORITY,
            hi: LAST_PRIORITY,
            default: entropy_to_priority(entropy),
            scale: 0,
            dtid: ctx.dettid,
            thread_rcbs: ctx.thread_rcbs,
            site,
        });
        crate::detlog!(
            "[dtid {}] CHOICE({}): priority => {}",
            ctx.dettid,
            site.name(),
            chosen
        );
        chosen
    }
}

/// A run's [`ChoiceSession`], shared by the run's threads.
///
/// An in-process capability: the run's global state holds the session and
/// puts a clone of this handle in each thread's start grant. Serialization
/// skips it (`GlobalResponse::StartNewThread`), so a response that crosses a
/// serialization boundary arrives without it, and [`ControlledChoices`]
/// refuses that thread by name. This attachment does not support backends
/// whose responses are serialized; `hermit run` refuses the choice options on
/// them.
#[derive(Clone)]
pub struct ChoiceHandle(Arc<Mutex<ChoiceSession>>);

impl ChoiceHandle {
    /// The handle of a new run's `session`.
    pub fn new(session: ChoiceSession) -> Self {
        Self(Arc::new(Mutex::new(session)))
    }

    /// Resolve `point` in the run's session ([`ChoiceSession::choose`]).
    pub fn choose(&self, point: ChoicePoint) -> u64 {
        self.0.lock().unwrap().choose(point)
    }
}

impl std::fmt::Debug for ChoiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChoiceHandle")
    }
}

/// Two handles are equal when they share one session.
impl PartialEq for ChoiceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ChoiceHandle {}

/// A vfork child's starting priority, decided by the parent when it prepares
/// the vfork (from a copy of its chaos stream, before the parent's own draw)
/// and consumed once, when the child is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PreparedChildPriority(Priority);

impl PreparedChildPriority {
    /// The prepared `priority`.
    pub fn new(priority: Priority) -> Self {
        Self(priority)
    }

    /// The priority, consuming the preparation.
    pub fn into_priority(self) -> Priority {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choice::TapeSource;

    fn ctx() -> DrawContext {
        DrawContext {
            dettid: DetTid::from_raw(5),
            thread_rcbs: 42,
        }
    }

    /// The seeded policy is the identity on slices and Detcore's own mapping
    /// on priorities, for every site.
    #[test]
    fn seeded_choices_return_the_draw() {
        let seeded = SeededChoices;
        for draw in [1, 7, 20_000_000, u64::MAX] {
            assert_eq!(seeded.slice(ctx(), draw, 1e7), draw);
        }
        for site in [
            ChoiceSite::Priority,
            ChoiceSite::ChildPriority,
            ChoiceSite::VforkChildPriority,
        ] {
            for entropy in [0, 1, 9_999, 123_456_789, u64::MAX] {
                assert_eq!(
                    seeded.priority(ctx(), site, entropy),
                    entropy_to_priority(entropy)
                );
            }
        }
    }

    #[test]
    fn seeded_choices_hold_nothing() {
        assert_eq!(std::mem::size_of::<SeededChoices>(), 0);
    }

    /// A controlled policy asks its session: an empty tape answers every
    /// point with its default, and a tape byte selects another value.
    #[test]
    fn controlled_choices_ask_the_attached_session() {
        let session = ChoiceSession::new(Box::new(TapeSource::new(vec![0, 0, 1])), None);
        let mut controlled = ControlledChoices::default();
        controlled.attach(Some(ChoiceHandle::new(session)), DetTid::from_raw(5));
        assert_eq!(controlled.slice(ctx(), 1234, 1e6), 1234);
        assert_eq!(
            controlled.priority(ctx(), ChoiceSite::Priority, 77),
            entropy_to_priority(77)
        );
        assert_eq!(
            controlled.priority(ctx(), ChoiceSite::ChildPriority, 77),
            FIRST_PRIORITY,
            "tape byte 1 is the bottom of the priority range"
        );
    }

    /// Threads of one run share one session: indices continue across them.
    #[test]
    fn clones_of_a_handle_share_the_session() {
        let handle = ChoiceHandle::new(ChoiceSession::new(
            Box::new(TapeSource::new(vec![0, 255])),
            None,
        ));
        let mut first = ControlledChoices::default();
        let mut second = ControlledChoices::default();
        first.attach(Some(handle.clone()), DetTid::from_raw(1));
        second.attach(Some(handle.clone()), DetTid::from_raw(2));
        assert_eq!(first, second);
        assert_eq!(
            first.priority(ctx(), ChoiceSite::Priority, 0),
            FIRST_PRIORITY
        );
        assert_eq!(
            second.priority(ctx(), ChoiceSite::Priority, 0),
            LAST_PRIORITY,
            "the second request reads the second tape byte"
        );
    }

    /// A thread offered the session it already holds (the root: from its
    /// self-registration, then again in its start grant) keeps it.
    #[test]
    fn the_held_session_offered_again_changes_nothing() {
        let handle = ChoiceHandle::new(ChoiceSession::new(Box::new(TapeSource::new(vec![])), None));
        let mut controlled = ControlledChoices::default();
        controlled.attach(Some(handle.clone()), DetTid::from_raw(3));
        let held = controlled.clone();
        controlled.attach(Some(handle), DetTid::from_raw(3));
        assert_eq!(controlled, held);
    }

    #[test]
    fn a_prepared_child_priority_is_its_priority() {
        let prepared = PreparedChildPriority::new(FIRST_PRIORITY + 3);
        assert_eq!(prepared.into_priority(), FIRST_PRIORITY + 3);
    }
}
