/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): how a copy gets its resources, and
//! where it runs.
//!
//! Both are policies, never fixed behavior. A driver chooses them per run
//! ([`super::RunSpec::copy`] and [`super::RunSpec::placement`]) and may
//! override them for the copies one call makes ([`Batch`],
//! [`super::Timelines::fork_batch`]). An implementation that cannot honor a
//! choice refuses it by name ([`super::Timelines::honors`], P7) rather than
//! serving it some other way. They apply to copies forked from a captured
//! run: a copy of a START checkpoint is a fresh run, which has nothing to
//! copy and places itself as any fresh run does.

/// When, and how, a copy gets its own instance of one kind of resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMode {
    /// Copied in full when the copy is made.
    Eager,
    /// Shared with the checkpoint, which never changes it, until the copy
    /// first writes it; split then (copy-on-write).
    Lazy,
    /// Not made when the copy is made, but when the copy is first resumed.
    /// A copy that is never resumed costs nothing but its handle.
    Dormant,
}

impl CopyMode {
    pub fn name(self) -> &'static str {
        match self {
            CopyMode::Eager => "eager",
            CopyMode::Lazy => "lazy",
            CopyMode::Dormant => "dormant",
        }
    }
}

/// One [`CopyMode`] for each kind of resource a run holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyPolicy {
    /// The guest's memory.
    pub memory: CopyMode,
    /// The guest's file descriptors and what they reach: pipes, socket
    /// pairs, eventfds, epoll instances, regular files' offsets.
    pub descriptors: CopyMode,
    /// The rest of the run's kernel state: the VM and its vCPUs, the guest's
    /// threads, timers.
    pub kernel_objects: CopyMode,
}

impl CopyPolicy {
    /// Fork-and-run, the default: copies run as soon as they are made, so
    /// descriptors and kernel objects are made at fork, and memory is
    /// copy-on-write.
    pub const FORK_AND_RUN: Self = Self {
        memory: CopyMode::Lazy,
        descriptors: CopyMode::Eager,
        kernel_objects: CopyMode::Eager,
    };

    /// Backups, rr style: copies kept in case the driver goes back to them,
    /// most never resumed, so nothing is made until one is.
    pub const BACKUP: Self = Self {
        memory: CopyMode::Dormant,
        descriptors: CopyMode::Dormant,
        kernel_objects: CopyMode::Dormant,
    };

    /// Everything copied when the copy is made: the comparison, and the
    /// escape hatch.
    pub const EAGER: Self = Self {
        memory: CopyMode::Eager,
        descriptors: CopyMode::Eager,
        kernel_objects: CopyMode::Eager,
    };

    /// `memory=M,descriptors=D,kernel_objects=K`, the form the control
    /// protocol and the bench take.
    pub fn describe(&self) -> String {
        format!(
            "memory={},descriptors={},kernel_objects={}",
            self.memory.name(),
            self.descriptors.name(),
            self.kernel_objects.name()
        )
    }
}

impl Default for CopyPolicy {
    fn default() -> Self {
        Self::FORK_AND_RUN
    }
}

/// A hint for a `sched_ext` scheduler about where a copy should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementHint {
    /// Prefer a CPU other than the parent's for a new copy, rather than
    /// sharing the parent's.
    AwayFromParent,
}

/// Where a copy's process runs. No pinning by default: the owner's rule
/// (Slack 6784) is to let the operating system schedule copies unless a
/// measurement proves that pinning pays off, and design section 13.4
/// measured no throughput gain from either pinned placement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// Pinned, to CPUs the driver owns (for example a fuzzer's container
    /// deciding which copies run where): copy j of a batch runs on CPU
    /// `cpus[j % cpus.len()]`. Opt-in, for experiments.
    Pool(Vec<usize>),
    /// Pinned, to an idle CPU each copy chooses and reserves the way a fresh
    /// pinned run does (Hermit's pinned-CPU placement). Opt-in, for
    /// experiments.
    SelfPlaced,
    /// Not pinned, the default: a copy may run on any CPU the run may use,
    /// and the Linux scheduler places it.
    #[default]
    Unpinned,
    /// Not pinned, with a hint for a `sched_ext` scheduler.
    Hinted(PlacementHint),
}

impl Placement {
    /// `pool:A:B`, `self`, `unpinned` or `hint:away-from-parent`, the form
    /// the control protocol and the bench take.
    pub fn describe(&self) -> String {
        match self {
            Placement::Pool(cpus) => {
                let cpus: Vec<String> = cpus.iter().map(|cpu| cpu.to_string()).collect();
                format!("pool:{}", cpus.join(":"))
            }
            Placement::SelfPlaced => "self".into(),
            Placement::Unpinned => "unpinned".into(),
            Placement::Hinted(PlacementHint::AwayFromParent) => "hint:away-from-parent".into(),
        }
    }

    /// Parses [`Placement::describe`]'s form.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "self" => Ok(Placement::SelfPlaced),
            "unpinned" => Ok(Placement::Unpinned),
            "hint:away-from-parent" => Ok(Placement::Hinted(PlacementHint::AwayFromParent)),
            _ => {
                let cpus = text
                    .strip_prefix("pool:")
                    .ok_or_else(|| format!("unknown placement {text:?}"))?;
                let cpus = cpus
                    .split(':')
                    .map(|cpu| cpu.parse::<usize>().map_err(|e| format!("{text:?}: {e}")))
                    .collect::<Result<Vec<_>, _>>()?;
                if cpus.is_empty() {
                    return Err(format!("{text:?}: a pool needs a CPU"));
                }
                Ok(Placement::Pool(cpus))
            }
        }
    }
}

/// Overrides of the run's policies for the copies one call makes. `None`
/// keeps the run's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub copy: Option<CopyPolicy>,
    pub placement: Option<Placement>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placements_round_trip_through_their_text_form() {
        for placement in [
            Placement::Pool(vec![3, 5]),
            Placement::SelfPlaced,
            Placement::Unpinned,
            Placement::Hinted(PlacementHint::AwayFromParent),
        ] {
            assert_eq!(Placement::parse(&placement.describe()), Ok(placement));
        }
        assert!(Placement::parse("pool:").is_err());
        assert!(Placement::parse("anywhere").is_err());
    }

    #[test]
    fn copies_are_not_pinned_by_default() {
        assert_eq!(Placement::default(), Placement::Unpinned);
        assert_eq!(Batch::default().placement, None);
    }

    #[test]
    fn the_presets_name_every_kind() {
        assert_eq!(
            CopyPolicy::default().describe(),
            "memory=lazy,descriptors=eager,kernel_objects=eager"
        );
        assert_eq!(
            CopyPolicy::BACKUP.describe(),
            "memory=dormant,descriptors=dormant,kernel_objects=dormant"
        );
    }
}
