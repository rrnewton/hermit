/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The run mode: Detcore's type parameter, composing the choices a run makes
//! once, when it is built.
//!
//! [`RunMode`] is type composition only: the record-or-replay sub-tool
//! ([`RunMode::RecordReplay`], with its existing bound), the scheduling-choice
//! policy ([`RunMode::Choices`]) and the choice boundary
//! ([`RunMode::Boundary`]). It carries no runtime value, flag or backend
//! selection. [`Mode`] composes a sub-tool and a policy, with no boundary;
//! [`SeededMode`] is the ordinary run and [`ControlledMode`] a run whose
//! scheduling choices a choice source answers. A mode with a boundary of its
//! own is its own type.

use std::marker::PhantomData;

use crate::choice::ChoiceSite;
use crate::choice::ControlledChoices;
use crate::choice::SchedulingChoices;
use crate::choice::SeededChoices;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;

/// A run's mode: see the module documentation.
pub trait RunMode: Send + Sync {
    /// The record-or-replay sub-tool that Detcore forwards to.
    type RecordReplay: RecordOrReplay;
    /// How the run resolves its scheduling choices.
    type Choices: SchedulingChoices;
    /// What runs just before each of the run's scheduling choice points is
    /// drawn.
    type Boundary: ChoiceBoundary<Self::Choices>;
}

/// A run mode's choice boundary, for runs whose choice policy is `C`: what
/// runs on the drawing thread's own task immediately before one of its
/// scheduling choice points is drawn, with that thread's guest.
///
/// Detcore calls it before every draw and nowhere else, uniformly, after
/// announcing the draw to the policy ([`SchedulingChoices::announce`]); it
/// never asks which boundary a run has. A run mode that needs one names its
/// own type, implemented for the policy whose announcement it reads, so the
/// capability is opt-in by type. The ordinary modes' boundary is
/// [`NoBoundary`], a zero-sized no-op.
///
/// Where nothing acts ([`Self::ACTS`] false: no boundary and a policy that
/// announces nothing, the ordinary run), there is no call: each draw site
/// tests that compile-time constant first, so the ordinary run's draw sites
/// hold no await. (An await is executed code even on a ready future: in
/// generic code it is a suspension point of the enclosing future in every
/// instance.)
///
/// No await separates a boundary from its draw, so nothing else runs between
/// them, and under [`ControlledChoices`] the draw checks that it takes the
/// request announced (`ChoiceSession::announce`): a draw without its
/// boundary, or a boundary without its draw, ends the run by name.
///
/// The sites ([`ChoiceSite`]):
/// - `ChaosSlice`: a thread's slice, at its start and at its slice ends.
/// - `Priority`: a thread's priority redraw, at a slice end or a
///   `sched_yield`.
/// - `ChildPriority`: a new thread's starting priority, drawn by its parent
///   after the kernel clone made the thread (or by the run's first thread,
///   for itself, as it registers). A boundary that copies the drawing thread
///   would copy a parent without that thread, so it refuses a capture there
///   by name.
/// - `VforkChildPriority`: a vfork child's starting priority, drawn by its
///   parent before the clone.
pub trait ChoiceBoundary<C: SchedulingChoices>: Send + Sync + 'static {
    /// Whether anything happens before a draw of this run: the policy's
    /// announcement, or this boundary. Where it is false, Detcore's draw
    /// sites do not call the boundary (a compile-time constant).
    const ACTS: bool = true;

    /// Runs immediately before the drawing thread `guest` draws its next
    /// choice, a point at `site` that the policy announced as `announced`
    /// (under [`ControlledChoices`], the index of the request the draw takes).
    /// A boundary that cannot do what a point needs ends the run by name
    /// rather than let the draw happen without it.
    fn before_choice<M, G>(
        guest: &mut G,
        site: ChoiceSite,
        announced: C::Announced,
    ) -> impl std::future::Future<Output = ()> + Send
    where
        M: RunMode<Choices = C, Boundary = Self>,
        G: reverie::Guest<crate::Detcore<M>>;
}

/// The ordinary run's choice boundary: nothing happens at a choice point.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoBoundary;

impl<C: SchedulingChoices> ChoiceBoundary<C> for NoBoundary {
    /// Only the policy's announcement acts: none for seeded choices.
    const ACTS: bool = C::ANNOUNCES;

    #[inline(always)]
    async fn before_choice<M, G>(_guest: &mut G, _site: ChoiceSite, _announced: C::Announced)
    where
        M: RunMode<Choices = C, Boundary = Self>,
        G: reverie::Guest<crate::Detcore<M>>,
    {
    }
}

/// The run mode of sub-tool `R` and choice policy `C`. A type only; it is
/// never constructed. Its choice boundary is [`NoBoundary`].
pub struct Mode<R, C>(PhantomData<fn() -> (R, C)>);

impl<R: RecordOrReplay, C: SchedulingChoices> RunMode for Mode<R, C> {
    type RecordReplay = R;
    type Choices = C;
    type Boundary = NoBoundary;
}

/// The ordinary run of sub-tool `R`: Detcore's seeded draws decide.
pub type SeededMode<R = NoopTool> = Mode<R, SeededChoices>;

/// A run of sub-tool `R` whose scheduling choices a choice source answers.
pub type ControlledMode<R = NoopTool> = Mode<R, ControlledChoices>;

// The ordinary run acts before no draw, so its draw sites make no boundary
// call (`Detcore::BOUNDARY_ACTS`, which also refuses to build a mode whose
// policy announces its draws to a boundary that does not act).
const _: () = assert!(!<NoBoundary as ChoiceBoundary<SeededChoices>>::ACTS);

/// The thread state of the record-or-replay sub-tool of mode `M`.
pub type RecordReplayState<M> = <<M as RunMode>::RecordReplay as reverie::Tool>::ThreadState;
