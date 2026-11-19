/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The run mode: Detcore's type parameter, composing the two choices a run
//! makes once, when it is built.
//!
//! [`RunMode`] is type composition only, with exactly two axes: the
//! record-or-replay sub-tool ([`RunMode::RecordReplay`], with its existing
//! bound) and the scheduling-choice policy ([`RunMode::Choices`]). It carries
//! no runtime value, flag or backend selection itself. [`Mode`] composes the
//! two. [`ConfiguredMode`] is the production mode: its policy,
//! [`RunChoices`], is seeded or controlled by the run's config, so seeded
//! and controlled runs share one Detcore instantiation. [`SeededMode`] and
//! [`ControlledMode`] fix the policy by type, for tests.

use std::marker::PhantomData;

use crate::choice::ControlledChoices;
use crate::choice::RunChoices;
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
}

/// The run mode of sub-tool `R` and choice policy `C`. A type only; it is
/// never constructed.
pub struct Mode<R, C>(PhantomData<fn() -> (R, C)>);

impl<R: RecordOrReplay, C: SchedulingChoices> RunMode for Mode<R, C> {
    type RecordReplay = R;
    type Choices = C;
}

/// The production run of sub-tool `R`: seeded, or controlled when the run's
/// config names a choice source ([`RunChoices`]).
pub type ConfiguredMode<R = NoopTool> = Mode<R, RunChoices>;

/// A run of sub-tool `R` whose policy is seeded by type: Detcore's seeded
/// draws decide.
pub type SeededMode<R = NoopTool> = Mode<R, SeededChoices>;

/// A run of sub-tool `R` whose policy is controlled by type: a choice
/// source answers its scheduling choices.
pub type ControlledMode<R = NoopTool> = Mode<R, ControlledChoices>;

/// The thread state of the record-or-replay sub-tool of mode `M`.
pub type RecordReplayState<M> = <<M as RunMode>::RecordReplay as reverie::Tool>::ThreadState;
