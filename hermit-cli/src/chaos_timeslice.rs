/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The maximum timeslice that `hermit run --chaos --chaos-seeded-timeslice`
//! draws from the scheduler seed.
//!
//! Chaos mode ends each timeslice after a random number of retired
//! conditional branches, exponentially distributed with a mean of the
//! maximum timeslice and capped at it. Which mean finds a race depends on the
//! race: one whose window is a few branches long needs slices of about a
//! hundred branches, while one whose second thread must run far into its own
//! work before the conflicting access needs slices that let it get there. A
//! fixed mean suits one of them. Drawing the mean once per scheduler seed,
//! log-uniformly over the range below, spreads a sweep of seeds across five
//! orders of magnitude, so it covers both without a guessed value.
//!
//! The draw is a pure function of the scheduler seed (and of the one-RCB
//! floor, which only a large clock multiplier or slowdown factor raises above
//! the range). It seeds a generator of its own and takes nothing from the
//! streams Detcore already derives from that seed (each thread's chaos
//! generator, the post-fork generator, the per-thread slowdown factors), so
//! those are seeded exactly as before. Its result is an ordinary
//! `--max-timeslice` value: Detcore receives the configuration of a run given
//! that value explicitly.

use std::num::NonZeroU64;

use rand::RngExt as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;

/// The smallest drawn maximum, in virtual nanoseconds: 100 retired
/// conditional branches at the default clock multiplier.
pub const SEEDED_MAX_TIMESLICE_MIN_NS: u64 = 1_000;

/// The largest drawn maximum, in virtual nanoseconds: the `--max-timeslice`
/// default. No draw exceeds it, so a replay of a recorded chaos run, which
/// keeps the default, is never bounded more tightly than the recording was.
pub const SEEDED_MAX_TIMESLICE_MAX_NS: u64 = 200_000_000;

/// [`SEEDED_MAX_TIMESLICE_MAX_NS`] as a maximum: the default that
/// `--chaos-seeded-timeslice` stands in for where it draws nothing.
pub const DEFAULT_MAX_TIMESLICE: NonZeroU64 = NonZeroU64::new(SEEDED_MAX_TIMESLICE_MAX_NS).unwrap();

/// Keeps this stream apart from the other streams seeded from the scheduler
/// seed (`post_fork_prng`'s "postfork", the slowdown factors' "slowdown").
const SEEDED_TIMESLICE_SALT: u64 = 0x7469_6d65_736c_6963; // "timeslic"

/// The maximum timeslice, in virtual nanoseconds, drawn for `sched_seed`:
/// log-uniform over [`SEEDED_MAX_TIMESLICE_MIN_NS`,
/// `SEEDED_MAX_TIMESLICE_MAX_NS`] and rounded to a whole nanosecond, then
/// raised to `minimum_ns` (one RCB at the run's clock multiplier and slowdown,
/// [`detcore::Config::minimum_max_timeslice_nanos`]) if it falls below it.
pub fn seeded_max_timeslice(sched_seed: u64, minimum_ns: u64) -> NonZeroU64 {
    let mut prng = Pcg64Mcg::seed_from_u64(sched_seed ^ SEEDED_TIMESLICE_SALT);
    let low = (SEEDED_MAX_TIMESLICE_MIN_NS as f64).ln();
    let high = (SEEDED_MAX_TIMESLICE_MAX_NS as f64).ln();
    let u: f64 = prng.random();
    let drawn = (low + u * (high - low)).exp().round() as u64;
    let drawn = drawn
        .clamp(SEEDED_MAX_TIMESLICE_MIN_NS, SEEDED_MAX_TIMESLICE_MAX_NS)
        .max(minimum_ns);
    NonZeroU64::new(drawn).expect("the drawn maximum is at least SEEDED_MAX_TIMESLICE_MIN_NS")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn the_largest_draw_is_the_max_timeslice_default() {
        let default = detcore::Config::parse_from(["hermit"]).max_timeslice;
        assert_eq!(default, Some(DEFAULT_MAX_TIMESLICE));
    }

    #[test]
    fn a_seed_always_draws_the_same_value() {
        for seed in [0, 1, 7, u64::MAX] {
            assert_eq!(
                seeded_max_timeslice(seed, 10),
                seeded_max_timeslice(seed, 10)
            );
        }
        // Pinned, so a change to the draw (which changes every seed's
        // schedule) is a visible decision rather than an accident.
        let pinned: Vec<u64> = (0..4)
            .map(|seed| seeded_max_timeslice(seed, 10).get())
            .collect();
        assert_eq!(pinned, [2_123, 6_806_600, 15_858_952, 835_700]);
    }

    /// 4096 seeds: every draw is in range, and each of the 5.3 decades holds
    /// its share (within 20%) of the draws.
    #[test]
    fn draws_are_log_uniform_over_the_range() {
        const SEEDS: u64 = 4096;
        let low = (SEEDED_MAX_TIMESLICE_MIN_NS as f64).log10();
        let span = (SEEDED_MAX_TIMESLICE_MAX_NS as f64).log10() - low;
        let mut buckets = [0u64; 5];
        for seed in 0..SEEDS {
            let drawn = seeded_max_timeslice(seed, 10).get();
            assert!(
                (SEEDED_MAX_TIMESLICE_MIN_NS..=SEEDED_MAX_TIMESLICE_MAX_NS).contains(&drawn),
                "seed {seed} drew {drawn}"
            );
            let decade = ((drawn as f64).log10() - low).floor() as usize;
            if let Some(bucket) = buckets.get_mut(decade) {
                *bucket += 1;
            }
        }
        let expected = SEEDS as f64 / span;
        for (decade, count) in buckets.iter().enumerate() {
            let ratio = *count as f64 / expected;
            assert!(
                (0.8..1.2).contains(&ratio),
                "decade {decade} holds {count} of {SEEDS} draws, expected about {expected:.0}"
            );
        }
    }

    #[test]
    fn a_draw_below_one_rcb_is_raised_to_it() {
        // A clock multiplier of 1000 makes one RCB 10,000 virtual ns.
        for seed in 0..256 {
            assert!(seeded_max_timeslice(seed, 10_000).get() >= 10_000);
            let unclamped = seeded_max_timeslice(seed, 10).get();
            if unclamped >= 10_000 {
                assert_eq!(seeded_max_timeslice(seed, 10_000).get(), unclamped);
            }
        }
    }
}
