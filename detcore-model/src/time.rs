/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::fmt;
use std::ops::Add;
use std::ops::Sub;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use reverie_syscalls::Timespec;
use reverie_syscalls::Timeval;
use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use crate::config::Config;
use crate::pid::DetTid;

// Time conversion constants from https://doc.rust-lang.org/stable/src/core/time.rs.html#26-30
const NANOS_PER_SEC: u64 = 1_000_000_000;
const NANOS_PER_MILLI: u64 = 1_000_000;
const NANOS_PER_MICRO: u64 = 1_000;
const MILLIS_PER_SEC: u64 = 1_000;
const MICROS_PER_SEC: u64 = 1_000_000;

// TODO: make all of these integral types to rule out fractional values.

/// Default virtual nanoseconds elapsed for callers that do not provide a syscall-specific cost.
pub const NANOS_PER_SYSCALL: f64 = 10000.0;

/// Virtual nanoseconds elapsed per Retired Conditional Branch.
pub const NANOS_PER_RCB: f64 = 10.0;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1151)
/// Fixed-point scale used for deterministic per-thread RCB time multipliers.
/// Q32 keeps accumulation independent of how a backend batches RCB updates.
const RCB_TIME_MULTIPLIER_SCALE: u64 = 1_u64 << 32;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1151)
/// A positive Q32 multiplier for converting RCB progress into virtual time.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
    Hash
)]
pub struct RcbTimeMultiplier(u64);

impl RcbTimeMultiplier {
    /// The identity multiplier.
    pub const ONE: Self = Self(RCB_TIME_MULTIPLIER_SCALE);

    /// Largest representable multiplier.
    pub const MAX: f64 = u64::MAX as f64 / RCB_TIME_MULTIPLIER_SCALE as f64;

    /// Quantize a finite positive multiplier to deterministic Q32 units.
    pub fn from_f64(value: f64) -> Self {
        assert!(value.is_finite() && value > 0.0 && value <= Self::MAX);
        let scaled = (value * RCB_TIME_MULTIPLIER_SCALE as f64).round() as u64;
        Self(scaled.max(1))
    }

    /// Convert the fixed-point value back to a floating-point multiplier.
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / RCB_TIME_MULTIPLIER_SCALE as f64
    }

    fn units(self) -> u64 {
        self.0
    }
}

impl Default for RcbTimeMultiplier {
    fn default() -> Self {
        Self::ONE
    }
}

/// Virtual nanoseconds elapsed per nondeterministic instruction other than system calls.
pub const NANOS_PER_NONDET_INSTR: f64 = 25.0;

/// Virtual nanoseconds elapsed per step of the scheduler.
pub const NANOS_PER_SCHED: f64 = 500_000.0;

// TODO: should map addresses to physical addresses.

// Deterministic Time:
//--------------------------------------------------------------------------------

/// Represents an absolute point in time in nanoseconds.
/// Parts of this API are largely inspired by `std::time::Duration`.
/// This could go to 128 bits if we need more than ~585 years of nanosecond precision.
#[derive(
    Default,
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
    Hash
)]
pub struct LogicalTime(u64);

// TODO: replace this with a wrapper around Duration, and change places that currently use Duration
// but should use LogicalDuration.
pub type LogicalDuration = LogicalTime;

impl LogicalTime {
    /// 0 integer nanoseconds.
    pub const ZERO: LogicalTime = LogicalTime(0);
    /// The maximum representable integer nanoseconds.
    pub const MAX: LogicalTime = LogicalTime(u64::MAX);

    /// The sentinel deadline for a wait that has *no* deadline.
    ///
    /// A `pause(2)` blocks until a signal arrives and can never time out, so it
    /// registers this value rather than a real deadline. The saturating `Add`
    /// impls below also land here when a guest arms an absurdly far-future timer
    /// (issue #219), which is the same situation: a deadline that can never be
    /// reached while virtual time remains representable.
    ///
    /// Callers that fast-forward virtual time to a pending deadline must check
    /// [`LogicalTime::is_indefinite`] first. Jumping the global clock onto this
    /// value would both destroy the continuity of virtual time (a ~584-year
    /// step) and wake a waiter that Linux would have left blocked.
    pub const INDEFINITE: LogicalTime = LogicalTime::MAX;

    /// Is this the [`LogicalTime::INDEFINITE`] sentinel, i.e. "no deadline"?
    pub fn is_indefinite(&self) -> bool {
        *self == LogicalTime::INDEFINITE
    }

    /// Returns the total number of whole microseconds contained by this `LogicalTime`.
    pub fn as_micros(&self) -> u64 {
        self.0 / NANOS_PER_MICRO
    }

    /// Returns the total number of whole milliseconds contained by this `LogicalTime`.
    pub fn as_millis(&self) -> u64 {
        self.0 / NANOS_PER_MILLI
    }

    /// Returns the total number of nanoseconds contained by this `LogicalTime`.
    pub fn as_nanos(&self) -> u64 {
        self.0
    }

    /// Returns the total number of *whole* seconds contained by this `LogicalTime`.
    /// The returned value does not include fractional (nanosecond) part of the duration,
    /// which can be obtained using `subsec_nanos`.
    pub fn as_secs(&self) -> u64 {
        self.0 / NANOS_PER_SEC
    }

    /// Creates a new `LogicalTime` from the specified number of microseconds.
    pub fn from_micros(micros: u64) -> Self {
        LogicalTime(micros * NANOS_PER_MICRO)
    }

    /// Creates a new `LogicalTime` from the specified number of milliseconds.
    pub fn from_millis(millis: u64) -> Self {
        LogicalTime(millis * NANOS_PER_MILLI)
    }

    /// Creates a new `LogicalTime` from the specified number of nanoseconds.
    pub fn from_nanos(nanos: u64) -> Self {
        LogicalTime(nanos)
    }

    /// Creates a new `LogicalTime` from the specified number of nanoseconds.
    pub fn from_big_nanos(nanos: u128) -> Self {
        // No good solution for this until we change the internal rep to 128 bit:
        LogicalTime(nanos as u64)
    }

    /// Creates a new `LogicalTime` from the specified number of seconds.
    pub fn from_secs(secs: u64) -> Self {
        LogicalTime(secs * NANOS_PER_SEC)
    }

    /// Returns the fractional part of this `LogicalTime`, in microseconds.
    /// This method does not return the length of the duration when represented by microseconds. The returned number always represents
    /// a fractional portion of a second (i.e., it is less than one million).
    pub fn subsec_micros(&self) -> u32 {
        ((self.0 / NANOS_PER_MICRO) % MICROS_PER_SEC) as u32
    }

    /// Returns the fractional part of this `LogicalTime`, in milliseconds.
    /// This method does not return the length of the duration when represented by milliseconds. The returned number always represents
    /// a fractional portion of a second (i.e., it is less than one thousand).
    pub fn subsec_millis(&self) -> u32 {
        ((self.0 / NANOS_PER_MILLI) % MILLIS_PER_SEC) as u32
    }

    /// Returns the fractional part of this `LogicalTime`, in nanoseconds.
    /// This method does not return the length of the duration when represented by nanoseconds. The returned number always represents
    /// a fractional portion of a second (i.e., it is less than one billion).
    pub fn subsec_nanos(&self) -> u32 {
        (self.0 % NANOS_PER_SEC) as u32
    }

    /// Convert a number of Retired Conditional Branches (RCBs) to Nanoseconds
    pub fn from_rcbs(n: u64) -> Self {
        LogicalTime((n as f64 * NANOS_PER_RCB) as u64)
    }

    /// Inverse of from_rcbs.  Non-injective, as it loses information, truncating to a
    /// coarser grained unit of time.
    pub fn into_rcbs(self) -> u64 {
        (self.0 as f64 / NANOS_PER_RCB) as u64
    }

    /// Convert a virtual duration to RCBs after applying a logical clock multiplier.
    pub fn into_rcbs_with_multiplier(self, multiplier: f64) -> u64 {
        debug_assert!(multiplier > 0.0);
        (self.0 as f64 / (NANOS_PER_RCB * multiplier)).floor() as u64
    }

    /// Test if the quantity is zero nanoseconds.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Measure the duration of the time interval since a previous time.
    pub fn duration_since(&self, from: LogicalTime) -> Duration {
        if from.0 > self.0 {
            panic!(
                "LogicalTime::duration_since cannot take duration since a time in the *future* ({}), relative to {}",
                from, self
            );
        }
        Duration::from_nanos(self.0 - from.0)
    }
}

impl std::fmt::Display for LogicalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Start with the raw characters for printed u64:
        let chars = format!("{}", self.0);
        let mut remain = chars.len();
        let mut first_char = true;
        for ch in chars.chars() {
            if !first_char && remain % 3 == 0 {
                if remain == 9 {
                    write!(f, ".")?;
                } else {
                    // Could also consider \u{2009} thin space here, but it prints fixed
                    // width in most terminals anyway:
                    write!(f, "_")?;
                }
            }
            first_char = false;
            remain -= 1;
            write!(f, "{}", ch)?;
        }
        if chars.len() <= 9 {
            write!(f, "ns")
        } else {
            write!(f, "s")
        }
    }
}

impl Add for LogicalTime {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        // Saturate at LogicalTime::MAX rather than panicking on overflow: a
        // far-future logical time is clamped to "the end of time".
        LogicalTime(self.0.saturating_add(rhs.0))
    }
}

impl Add<Duration> for LogicalTime {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self {
        // A `Duration` can hold more nanoseconds than fit in u64, and the sum
        // can exceed u64::MAX (e.g. Java arms a far-future timer, issue #219).
        // Saturate the u128->u64 conversion and the addition instead of
        // overflowing/truncating.
        let nanos = u64::try_from(rhs.as_nanos()).unwrap_or(u64::MAX);
        LogicalTime(self.0.saturating_add(nanos))
    }
}

impl Add<u128> for LogicalTime {
    type Output = Self;
    fn add(self, rhs: u128) -> Self {
        // Saturate both the u128->u64 clamp and the addition.
        LogicalTime(self.0.saturating_add(rhs.min(u64::MAX as u128) as u64))
    }
}

impl Sub for LogicalTime {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        LogicalTime(self.0 - rhs.0)
    }
}

impl From<LogicalTime> for Timespec {
    fn from(logical_time: LogicalTime) -> Timespec {
        Timespec {
            tv_sec: logical_time.as_secs() as i64,
            tv_nsec: logical_time.subsec_nanos() as i64,
        }
    }
}

impl From<LogicalTime> for Timeval {
    fn from(logical_time: LogicalTime) -> Timeval {
        Timeval {
            tv_sec: logical_time.as_secs() as i64,
            tv_usec: logical_time.subsec_micros() as i64,
        }
    }
}

#[test]
fn print_nanoseconds() {
    let ns1 = LogicalTime(946_684_799_000_000_000);
    let ns2 = LogicalTime(729_860_000);
    assert_eq!(format!("{}", ns1), "946_684_799.000_000_000s");
    assert_eq!(format!("{}", ns1 + ns2), "946_684_799.729_860_000s");
    assert_eq!(format!("{}", ns2), "729_860_000ns");
}

#[test]
fn subsecond_units_are_converted_from_nanoseconds() {
    let time = LogicalTime::from_secs(2) + LogicalTime::from_nanos(345_678_901);

    assert_eq!(time.subsec_millis(), 345);
    assert_eq!(time.subsec_micros(), 345_678);
    assert_eq!(time.subsec_nanos(), 345_678_901);

    let timeval: Timeval = time.into();
    assert_eq!(timeval.tv_sec, 2);
    assert_eq!(timeval.tv_usec, 345_678);
}

#[test]
fn indefinite_is_recognized_and_nothing_else_is() {
    assert!(LogicalTime::INDEFINITE.is_indefinite());
    assert!(!LogicalTime::ZERO.is_indefinite());
    assert!(!LogicalTime::from_secs(1_767_225_600).is_indefinite());
    assert!(!LogicalTime(u64::MAX - 1).is_indefinite());

    // A saturated far-future deadline (issue #219) lands on the same sentinel,
    // and is equally unreachable, so the scheduler must treat it the same way.
    assert!((LogicalTime(u64::MAX - 10) + LogicalTime::from_secs(1)).is_indefinite());
}

#[test]
fn add_saturates_instead_of_overflowing() {
    // Regression for issue #219: Java arms a far-future timer whose deadline
    // overflows u64. All three `Add` impls must saturate at LogicalTime::MAX
    // rather than panic.
    let near_max = LogicalTime(u64::MAX - 10);

    // Add<LogicalTime>
    assert_eq!(near_max + LogicalTime::from_secs(1), LogicalTime::MAX);

    // Add<Duration>, including a Duration whose nanos exceed u64::MAX.
    assert_eq!(near_max + Duration::from_secs(1), LogicalTime::MAX);
    assert_eq!(
        LogicalTime::ZERO + Duration::from_secs(u64::MAX / 1_000_000_000 + 1),
        LogicalTime::MAX
    );

    // Add<u128>, including an rhs larger than u64::MAX.
    assert_eq!(near_max + 100u128, LogicalTime::MAX);
    assert_eq!(
        LogicalTime::ZERO + (u128::from(u64::MAX) + 5),
        LogicalTime::MAX
    );

    // A non-overflowing add still produces the exact sum.
    assert_eq!(
        LogicalTime::from_secs(2) + Duration::from_secs(3),
        LogicalTime::from_secs(5)
    );
}

/// The same basic type alias as nanoseconds. Just for clarity/readability.
pub type Microseconds = u64;

/// A determinstic notion of time, measuring progress of one thread.
///
/// It is based on counting syscalls and conditionals.  Ideally it would count
/// instructions, but that is not possible deterministically.
///
/// `DetTime` is a measure of the LOCAL progress of a thread. The notion of
/// global time is defined by aggretion of multiple local times (vector
/// clocks, as in the Kendo algorithm).
///
/// Here are a few relevant definitions for deterministic time:
///
/// **Granularity**
///
/// How rapidly and consistently does the clock tick with thread progress?
/// A finer notion of deterministic time counts *more* events (ideally instructions).
/// A coarser notion of deterministic time counts fewer events (like syscalls).
///
/// **Productivity**
///
/// In corecursion, or coinductive datatypes like streams, productivity means you can get
/// the next result with finite work. Deterministic time can be viewed a stream of "tick
/// events". We don't want a guest thread to be able to do an unbounded amount of work,
/// without a tick occurring. For example, retired branches are a safe bet (any
/// non-trivial amount of work will execute a branch), but system calls are not: a
/// spinning thread can burn cycles forever without executing a syscall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetTime {
    /// The syscalls issued by this thread.
    syscalls: u64,

    /// Accumulated syscall cost before applying `multiplier`.
    ///
    /// `None` preserves the uniform-cost interpretation of serialized `DetTime` values created
    /// before syscall-specific costs were introduced.
    #[serde(default)]
    syscall_nanos: Option<u64>,

    /// Retired conditional branches, as given "opaquely" by the reverie clock.
    /// Technically, that these are RCBs is an implementation detail of reverie.
    rcbs: u64,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// RCB progress weighted by per-thread virtual-time multipliers, in Q32 RCB units.
    /// `None` preserves the uniform interpretation of older serialized values.
    // This field participates in Reverie's non-self-describing bincode RPC.
    // Never skip it during serialization: doing so would shift the following
    // `GlobalRequest` bytes and corrupt the tuple on decode.
    #[serde(default)]
    weighted_rcbs: Option<u128>,

    /// Number of nondeterministic instructions (rdtsc, cpuid)
    nondet_instrs: u64,

    /// Explicit virtual-time advances, such as a PMU maximum when RCB accounting is disabled.
    #[serde(default)]
    extra_nanos: u64,

    /// Baseline amount of time to add.
    starting_micros: Microseconds,

    /// Multiplier for all time advances.
    multiplier: f64,

    /// Elapsed local time inherited when this thread was created. It remains
    /// part of the absolute clock, but is work already charged to its ancestors.
    /// Like the other clock fields, this must always be present in bincode RPCs.
    #[serde(default)]
    inherited_nanos: LogicalDuration,

    /// Submicrosecond part of the configured origin, never elapsed guest work.
    /// Missing legacy JSON fields mean the old microsecond origin. This field
    /// is always serialized, including zero, in positional bincode RPCs;
    /// mixed RPC layouts require matching config wire fingerprints, not serde
    /// defaults. Keep the representation canonical in the range 0..1000.
    #[serde(default, deserialize_with = "deserialize_origin_remainder")]
    starting_submicro_nanos: u16,
}

fn deserialize_origin_remainder<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let remainder = u16::deserialize(deserializer)?;
    if remainder >= 1_000 {
        return Err(serde::de::Error::custom(
            "clock origin remainder must be less than 1000 nanoseconds",
        ));
    }
    Ok(remainder)
}

// Don't derive Default because it would give us a 0.0 multiplier:
impl Default for DetTime {
    fn default() -> Self {
        DetTime {
            syscalls: 0,
            syscall_nanos: Some(0),
            rcbs: 0,
            weighted_rcbs: None,
            nondet_instrs: 0,
            extra_nanos: 0,
            starting_micros: 0,
            multiplier: 1.0,
            inherited_nanos: LogicalTime::ZERO,
            starting_submicro_nanos: 0,
        }
    }
}

impl Eq for DetTime {}

/// `DetTime` behaves as a totally ordered scalar.
impl Ord for DetTime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let nanos = other.as_nanos();
        self.as_nanos().cmp(&nanos)
    }
}

#[allow(clippy::non_canonical_partial_ord_impl)]
impl PartialOrd for DetTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let nanos = other.as_nanos();
        self.as_nanos().partial_cmp(&nanos)
    }
}

impl PartialEq for DetTime {
    fn eq(&self, other: &Self) -> bool {
        self.as_nanos() == other.as_nanos()
    }
}

impl From<&DateTime<Utc>> for DetTime {
    fn from(dt: &DateTime<Utc>) -> Self {
        DetTime {
            syscalls: 0,
            syscall_nanos: Some(0),
            rcbs: 0,
            weighted_rcbs: None,
            nondet_instrs: 0,
            extra_nanos: 0,
            starting_micros: micros_from_utc(dt),
            multiplier: 1.0,
            inherited_nanos: LogicalTime::ZERO,
            starting_submicro_nanos: (dt.timestamp_subsec_nanos() % 1_000) as u16,
        }
    }
}

fn micros_from_utc(dt: &DateTime<Utc>) -> Microseconds {
    dt.timestamp() as Microseconds * 1_000_000 + dt.timestamp_subsec_micros() as Microseconds
}

// implementing From<DetTime> for Timespec is not possible due to dependency graph
#[allow(clippy::from_over_into)]
impl Into<Timespec> for DetTime {
    fn into(self) -> Timespec {
        self.as_nanos().into()
    }
}

impl DetTime {
    /// Create an initial `DetTime` respecting a `Config`
    pub fn new(cfg: &Config) -> Self {
        // We inflate the amount of time everything consumes to compensate for the fact that we
        // are ONLY counting certain events sparsely. In theory this should be based on some kind
        // of expected value for compute-between-syscalls on average applications. (But is that
        // even a normal distribution?)
        let additional_multiplier = if cfg.sequentialize_threads && !cfg.use_rcb_time() {
            500.0
        } else {
            // Otherwise, virtual time isn't really used for scheduling, just for
            // metadata, so it doesn't really matter what the rate of ticking is.
            1.0
        };
        match cfg.clock_multiplier {
            Some(m) => DetTime::from(&cfg.epoch).with_multiplier(m * additional_multiplier),
            None => DetTime::from(&cfg.epoch).with_multiplier(additional_multiplier),
        }
    }

    /// Create a new `Dettime` which is the earliest possible.
    pub fn zero() -> Self {
        DetTime {
            syscalls: 0,
            syscall_nanos: Some(0),
            rcbs: 0,
            weighted_rcbs: None,
            nondet_instrs: 0,
            extra_nanos: 0,
            starting_micros: 0,
            multiplier: 1.0,
            inherited_nanos: LogicalTime::ZERO,
            starting_submicro_nanos: 0,
        }
    }

    /// Inherit an absolute clock for a newly created thread without attributing
    /// its ancestors' work to that thread. Ordinary `Clone` still copies an
    /// existing clock, including its original inheritance, for RPC snapshots.
    pub fn clone_for_child(&self) -> Self {
        let mut child = self.clone();
        child.inherited_nanos = self.without_starting();
        child
    }

    /// Local elapsed time already accounted for by this thread's ancestors.
    pub fn inherited_nanos(&self) -> LogicalDuration {
        self.inherited_nanos
    }

    /// Register that another syscall has executed.
    pub fn add_syscall(&mut self) {
        self.add_syscall_with_cost(NANOS_PER_SYSCALL as u64);
    }

    /// Register a syscall with its unscaled virtual-time cost in nanoseconds.
    pub fn add_syscall_with_cost(&mut self, nanos: u64) {
        let previous_uniform_nanos = (self.syscalls as f64 * NANOS_PER_SYSCALL) as u64;
        self.syscalls += 1;
        match &mut self.syscall_nanos {
            Some(syscall_nanos) => *syscall_nanos += nanos,
            None => self.syscall_nanos = Some(previous_uniform_nanos + nanos),
        }
        trace!(
            "[detcore] added syscall cost of {}ns to logical time, yielding: {:?}",
            nanos, self
        );
    }

    /// Register that an `rdtsc` intsruction has executed.
    pub fn add_rdtsc(&mut self) {
        self.nondet_instrs += 1;
    }

    /// Register that an `cpuid` intsruction has executed.
    pub fn add_cpuid(&mut self) {
        self.nondet_instrs += 1;
    }

    /// Update internal counts using the reverie clock value.
    pub fn add_rcbs(&mut self, count: u64) {
        if let Some(weighted_rcbs) = &mut self.weighted_rcbs {
            *weighted_rcbs += u128::from(count) * u128::from(RCB_TIME_MULTIPLIER_SCALE);
        }
        self.rcbs += count;
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    /// Add RCB progress using a deterministic per-thread virtual-time multiplier.
    ///
    /// The Q32 accumulator makes the result independent of whether a backend reports
    /// the same RCB total in one update or several smaller updates.
    pub fn add_rcbs_with_multiplier(&mut self, count: u64, factor: RcbTimeMultiplier) {
        let weighted_rcbs = self
            .weighted_rcbs
            .get_or_insert_with(|| u128::from(self.rcbs) * u128::from(RCB_TIME_MULTIPLIER_SCALE));
        *weighted_rcbs += u128::from(count) * u128::from(factor.units());
        self.rcbs += count;
    }

    /// Advance this local clock to an explicit virtual deadline.
    pub fn advance_to(&mut self, deadline: LogicalTime) {
        let current = self.as_nanos();
        assert!(deadline >= current);
        self.extra_nanos += (deadline - current).as_nanos();
    }

    /// Return current rcbs
    pub fn rcbs(&self) -> u64 {
        self.rcbs
    }

    /// Project deterministic logical time into a rough number of nanoseconds.
    pub fn as_nanos(&self) -> LogicalTime {
        // Note: these counts could be pre-collapsed into scalar within the DetTime
        // representation.  But currently we leave them separate for debuggability.
        let syscall_nanos = self
            .syscall_nanos
            .unwrap_or((self.syscalls as f64 * NANOS_PER_SYSCALL) as u64);
        let rcb_nanos = self.weighted_rcbs.map_or_else(
            || self.rcbs as f64 * NANOS_PER_RCB,
            |weighted| weighted as f64 * NANOS_PER_RCB / RCB_TIME_MULTIPLIER_SCALE as f64,
        );
        LogicalTime(
            self.starting_nanos()
                + self.extra_nanos
                + ((syscall_nanos as f64 * self.multiplier) as u64)
                + ((rcb_nanos * self.multiplier) as u64)
                + ((self.nondet_instrs as f64 * NANOS_PER_NONDET_INSTR * self.multiplier) as u64),
        )
    }

    /// Same as as_nanos but without the starting time.
    pub fn without_starting(&self) -> LogicalDuration {
        let LogicalTime(t1) = self.as_nanos();
        LogicalTime(t1 - self.starting_nanos())
    }

    fn starting_nanos(&self) -> u64 {
        self.starting_micros * 1_000 + u64::from(self.starting_submicro_nanos)
    }

    // TODO-HUMAN-REVIEW(#797): Review logical user/system CPU-time projections.
    /// Guest-execution time that corresponds to user-space instructions.
    pub fn user_cpu_time(&self) -> LogicalDuration {
        let rcb_nanos = self.weighted_rcbs.map_or_else(
            || self.rcbs as f64 * NANOS_PER_RCB,
            |weighted| weighted as f64 * NANOS_PER_RCB / RCB_TIME_MULTIPLIER_SCALE as f64,
        );
        LogicalTime(
            ((rcb_nanos + (self.nondet_instrs as f64 * NANOS_PER_NONDET_INSTR)) * self.multiplier)
                as u64,
        )
    }

    // TODO-HUMAN-REVIEW(#797): Review logical user/system CPU-time projections.
    /// Synthetic time charged for intercepted syscall execution.
    pub fn system_cpu_time(&self) -> LogicalDuration {
        let syscall_nanos = self
            .syscall_nanos
            .unwrap_or((self.syscalls as f64 * NANOS_PER_SYSCALL) as u64);
        LogicalTime((syscall_nanos as f64 * self.multiplier) as u64)
    }

    /// Project deterministic logical time into a rough number of microseconds.
    pub fn as_micros(&self) -> Microseconds {
        self.as_nanos().0 / 1000
    }

    /// Set the clock multiplier
    pub fn with_multiplier(mut self, m: f64) -> Self {
        self.multiplier = m;
        self
    }

    /// Project deterministic time duration from imaginary starting point of deterministic time creation
    pub fn as_duration(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.without_starting().0)
    }
}

#[cfg(test)]
mod rcb_multiplier_tests {
    use super::*;

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn weighted_rcb_time_is_batching_independent() {
        let factor = RcbTimeMultiplier::from_f64(2.5);
        let mut one_batch = DetTime::zero();
        one_batch.add_rcbs_with_multiplier(10, factor);

        let mut split_batches = DetTime::zero();
        split_batches.add_rcbs_with_multiplier(4, factor);
        split_batches.add_rcbs_with_multiplier(6, factor);

        assert_eq!(one_batch.rcbs(), 10);
        assert_eq!(split_batches.rcbs(), 10);
        assert_eq!(one_batch.as_nanos(), LogicalTime::from_nanos(250));
        assert_eq!(one_batch.as_nanos(), split_batches.as_nanos());
        assert_eq!(one_batch.user_cpu_time(), split_batches.user_cpu_time());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    #[test]
    fn uniform_rcbs_after_weighted_rcbs_keep_continuity() {
        let mut time = DetTime::zero();
        time.add_rcbs_with_multiplier(10, RcbTimeMultiplier::from_f64(0.5));
        assert_eq!(time.as_nanos(), LogicalTime::from_nanos(50));

        time.add_rcbs(5);
        assert_eq!(time.rcbs(), 15);
        assert_eq!(time.as_nanos(), LogicalTime::from_nanos(100));
    }
}

/// Deterministic global time, combining local times.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct GlobalTime {
    /// The time when the container began execution.
    starting_nanos: LogicalTime,

    /// Latest absolute local duration for each thread, excluding the epoch.
    /// This includes inherited history for scheduler and replay consumers.
    time_vector: HashMap<DetTid, LogicalTime>,

    /// The inherited part of each local duration. It contributes no new work
    /// to aggregate time. Missing entries represent zero for older snapshots.
    #[serde(default)]
    inherited_time: HashMap<DetTid, LogicalDuration>,

    /// A source of central, logically external, time passage generated by the scheduler.
    extra_time: LogicalTime,

    /// This won't always be possible, but for now, since local clocks don't tick
    /// asynchronously, it is quite straightforward to keep a count of cumulative progress.
    total: LogicalTime,

    /// Immutable. Simply copied from the Config.
    multiplier: f64,
}

impl GlobalTime {
    /// Create a fresh global time, respecting the `Config`.
    pub fn new(cfg: &Config) -> Self {
        let base = DetTime::new(cfg);
        GlobalTime {
            starting_nanos: LogicalTime::from_nanos(base.starting_nanos()),
            time_vector: HashMap::new(),
            inherited_time: HashMap::new(),
            extra_time: LogicalTime::from_nanos(0),
            total: base.as_nanos(),
            multiplier: cfg.clock_multiplier.unwrap_or(1.0),
        }
    }

    /// Tick the time of a particular thread.
    pub fn update_global_time(
        &mut self,
        tid: DetTid,
        newtime: LogicalTime,
        inherited: LogicalDuration,
    ) {
        if newtime < self.starting_nanos {
            panic!(
                "update_global_time: Cannot set thread {} time to {}, which is before start of container execution {}",
                tid, newtime, self.starting_nanos
            );
        }

        // TODO(T136359599): change to duration_since, and store durations in time_vector:
        let newtime = newtime - self.starting_nanos;
        trace!(
            "[tid {}] ticked its global time component to {}",
            tid, newtime,
        );
        if let Some(old) = self.time_vector.get_mut(&tid) {
            if *old > newtime {
                panic!(
                    "Attempted to update tid {} time to {}, but was already {}",
                    tid, newtime, old
                );
            }
            // Update the cached total for efficiency:
            let LogicalTime(diff) = newtime - *old;
            *old = newtime;
            // Exec may reload fresh local state. Its first response restores
            // the absolute clock, while this existing component retains the
            // original inherited baseline rather than charging that work again.
            self.bump_total(Duration::from_nanos(diff));
        } else {
            assert!(
                inherited <= newtime,
                "thread {tid} inherited time {inherited} beyond its local duration {newtime}"
            );
            self.time_vector.insert(tid, newtime);
            self.inherited_time.insert(tid, inherited);
            // A child's first startup RPC reports its inherited clock before
            // it has executed guest work. Its arrival must contribute zero,
            // whether it precedes or follows the scheduler's time snapshot.
            self.bump_total(Duration::from_nanos((newtime - inherited).0));
        }
    }

    fn sanity(&self) {
        debug_assert_eq!(self.sum_up(), self.total);
    }

    fn bump_total(&mut self, delta: Duration) {
        self.total = self.total + delta;
        self.sanity();
    }

    // The expensive way to get the total (internal)
    fn sum_up(&self) -> LogicalTime {
        let mut sum = self.starting_nanos;
        for (tid, tm) in &self.time_vector {
            sum = sum + (*tm - self.inherited_duration(*tid));
        }
        sum + self.extra_time
    }

    /// Add time that passage is not driven by the internal events within guest threads.
    /// This is effectively used to account for "time" consumed by the scheduler, and to
    /// ensure monotonic increase of global time while scheduling.
    pub fn add_scheduler_time(&mut self) -> LogicalTime {
        let delta = Duration::from_nanos((NANOS_PER_SCHED * self.multiplier) as u64);
        self.add_extra_time(delta)
    }

    /// Update the global clock to account for time not driven by internal
    /// within guest threads.  This is a central or external expenditure of
    /// time, rather than a thread-internal one.
    ///
    /// The argument is in nanosecods and should have had any clock multiplier
    /// applied alreday.
    pub fn add_extra_time(&mut self, delta: Duration) -> LogicalTime {
        self.extra_time = self.extra_time + delta;
        // Update the cached total for efficiency:
        self.bump_total(delta);
        self.as_nanos()
    }

    /// Project a thread's absolute local clock, including its inherited history
    /// and the epoch. Exec recovery and scheduler replay use this projection.
    pub fn threads_time(&self, dtid: DetTid) -> LogicalTime {
        self.starting_nanos + self.threads_duration(dtid)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review thread-presence detection for backend reconnects.
    /// Returns whether this clock has observed work from a thread.
    pub fn contains_thread(&self, dtid: DetTid) -> bool {
        self.time_vector.contains_key(&dtid)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1173): Review SaBRe exec clock reassignment.
    /// Move a surviving thread's clock component to the process-leader TID
    /// installed by Linux after a non-leader thread successfully execs.
    ///
    /// Work previously performed by the old leader remains part of aggregate
    /// time, but no longer belongs to the new image's local thread clock.
    pub fn reassign_thread(&mut self, from: DetTid, to: DetTid) {
        if from == to {
            return;
        }

        let survivor_time = self
            .time_vector
            .remove(&from)
            .unwrap_or_else(|| panic!("cannot reassign missing thread clock {from}"));
        let survivor_inherited = self.inherited_time.remove(&from).unwrap_or_default();
        let retired_inherited = self.inherited_time.remove(&to).unwrap_or_default();
        if let Some(retired_leader_time) = self.time_vector.remove(&to) {
            self.extra_time = self.extra_time + (retired_leader_time - retired_inherited);
        }
        self.time_vector.insert(to, survivor_time);
        self.inherited_time.insert(to, survivor_inherited);
        self.sanity();
    }

    fn inherited_duration(&self, dtid: DetTid) -> LogicalDuration {
        self.inherited_time.get(&dtid).copied().unwrap_or_default()
    }

    /// Project a thread's local duration, including inherited history but
    /// excluding the epoch. This is the duration used by scheduler replay;
    /// aggregate time separately excludes the inherited part.
    pub fn threads_duration(&self, dtid: DetTid) -> LogicalDuration {
        *self.time_vector.get(&dtid).unwrap_or_else(|| {
            panic!(
                "Trying to extract time for thread {}, but no entry found!",
                dtid
            )
        })
    }

    /// Deterministic lower bound on the amount of work that has happened across all
    /// threads, starting with the same epoch time as individual thread clocks.
    ///
    /// This roughly models something like real time if all threads were running on one core.
    pub fn as_nanos(&self) -> LogicalTime {
        self.total
    }
}

#[cfg(test)]
mod global_time_tests {
    use super::*;

    #[test]
    fn fractional_network_trace_v1_keeps_its_original_bytes_and_time_domain() {
        use crate::fd::OpenFileId;
        use crate::network_trace::*;

        let epoch = DateTime::from_timestamp(1_767_225_600, 123_456_789).unwrap();
        let channel = OpenFileId::new_socket(DetTid::from_raw(1), 0);
        // V1 predates the nanosecond clock origin. Keep its historical
        // microsecond origin and absolute release timestamps unchanged.
        let origin = LogicalTime::from_nanos(1_767_225_600_123_456_000);
        let mut trace = NetworkTraceV1 {
            epoch,
            channels: vec![NetworkChannelV1 {
                id: channel,
                transport: NetworkTransportV1::Tcp,
                role: NetworkEndpointRoleV1::OutboundClient,
                local_address: NetworkAddressV1::Inet4 {
                    address: [10, 0, 0, 2],
                    port: 40_000,
                },
                peer_address: NetworkAddressV1::Inet4 {
                    address: [192, 0, 2, 10],
                    port: 443,
                },
                created_before_competing_threads: true,
            }],
            inputs: vec![NetworkInputEventV1 {
                ordinal: 0,
                channel,
                release: NetworkReleaseV1 {
                    not_before_global_time: origin + LogicalTime::from_nanos(10),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV1::InboundBytes {
                    stream_offset: 0,
                    bytes: b"x".to_vec(),
                },
            }],
            outputs: vec![],
        };
        let mut frame = Vec::new();
        trace.write_framed(&mut frame).unwrap();
        // Captured with the pre-fix producer at 173929ab, before adding the
        // origin remainder. This is a format fixture, not a fresh round-trip
        // expectation derived from the implementation under test.
        const ORIGINAL_FRAME: &[u8] = &[
            72, 69, 82, 77, 73, 84, 45, 78, 69, 84, 45, 84, 82, 65, 67, 69, 1, 0, 0, 0, 88, 0, 0,
            0, 0, 0, 0, 0, 30, 50, 48, 50, 54, 45, 48, 49, 45, 48, 49, 84, 48, 48, 58, 48, 48, 58,
            48, 48, 46, 49, 50, 51, 52, 53, 54, 55, 56, 57, 90, 1, 2, 253, 0, 0, 0, 0, 0, 0, 0,
            128, 0, 0, 0, 10, 0, 0, 2, 251, 64, 156, 0, 192, 0, 2, 10, 251, 187, 1, 1, 1, 0, 2,
            253, 0, 0, 0, 0, 0, 0, 0, 128, 253, 10, 202, 85, 245, 81, 114, 134, 24, 0, 0, 0, 1,
            120, 0,
        ];
        assert_eq!(frame, ORIGINAL_FRAME);
        let decoded = NetworkTraceV1::read_framed(frame.as_slice()).unwrap();
        assert_eq!(decoded, trace);
        assert_eq!(decoded.epoch, epoch);
        assert_eq!(decoded.epoch_global_time(), Ok(origin));
        let current = GlobalTime::new(&Config {
            epoch,
            ..Config::default()
        });
        assert_eq!(current.as_nanos(), origin + LogicalTime::from_nanos(789));
        let release = decoded.inputs[0].release;
        assert!(!release.is_eligible(origin + LogicalTime::from_nanos(9), 0));
        assert!(release.is_eligible(origin + LogicalTime::from_nanos(10), 0));
        trace.inputs[0].release.not_before_global_time = origin - LogicalTime::from_nanos(1);
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ReleaseBeforeEpoch)
        );
    }

    #[test]
    fn fractional_epoch_is_an_exact_origin_not_elapsed_work() {
        for nanos in [0, 1, 13, 999, 1_000, 123_456_789, 999_999_999] {
            let epoch = DateTime::from_timestamp(1_767_225_600, nanos).unwrap();
            let expected = LogicalTime::from_nanos(epoch.timestamp_nanos_opt().unwrap() as u64);
            let config = Config {
                epoch,
                clock_multiplier: Some(2.0),
                ..Config::default()
            };
            let mut clock = DetTime::new(&config);
            let mut global = GlobalTime::new(&config);
            assert_eq!(clock.as_nanos(), expected, "epoch fraction {nanos}");
            assert_eq!(global.as_nanos(), expected);
            assert_eq!(clock.without_starting(), LogicalTime::ZERO);
            assert_eq!(clock.as_duration(), Duration::ZERO);
            assert_eq!(clock.user_cpu_time(), LogicalTime::ZERO);
            assert_eq!(clock.system_cpu_time(), LogicalTime::ZERO);

            // In particular, 999ns + 1ns crosses a microsecond without rounding
            // the configured origin or charging it as guest work.
            clock.advance_to(expected + LogicalTime::from_nanos(1));
            publish(&mut global, DetTid::from_raw(3), &clock);
            assert_eq!(clock.as_duration(), Duration::from_nanos(1));
            assert_eq!(global.as_nanos(), expected + LogicalTime::from_nanos(1));
            let mut zero_origin = DetTime::zero().with_multiplier(clock.multiplier);
            zero_origin.advance_to(LogicalTime::from_nanos(1));
            for time in [&mut clock, &mut zero_origin] {
                time.add_syscall_with_cost(7);
                time.add_rcbs(3);
                time.add_rdtsc();
            }
            assert_eq!(clock.without_starting(), zero_origin.as_nanos());
            assert_eq!(clock.user_cpu_time(), zero_origin.user_cpu_time());
            assert_eq!(clock.system_cpu_time(), zero_origin.system_cpu_time());
            publish(&mut global, DetTid::from_raw(3), &clock);
            assert_eq!(global.as_nanos(), expected + zero_origin.as_nanos());
        }
    }

    fn publish(time: &mut GlobalTime, tid: DetTid, clock: &DetTime) {
        time.update_global_time(tid, clock.as_nanos(), clock.inherited_nanos());
    }

    #[test]
    fn descendants_preserve_absolute_clocks_and_charge_only_their_own_work() {
        let config = Config {
            epoch: DateTime::from_timestamp(1_767_225_600, 999).unwrap(),
            clock_multiplier: Some(2.0),
            ..Config::default()
        };
        let root = DetTid::from_raw(3);
        let child = DetTid::from_raw(4);
        let grandchild = DetTid::from_raw(5);
        let mut time = GlobalTime::new(&config);
        let start = time.as_nanos();
        let mut root_clock = DetTime::new(&config);
        root_clock.add_syscall_with_cost(11);
        root_clock.add_rcbs(3);
        root_clock.add_rdtsc();
        root_clock.advance_to(root_clock.as_nanos() + LogicalTime::from_nanos(1));
        assert_eq!(root_clock.without_starting(), LogicalTime::from_nanos(133));
        publish(&mut time, root, &root_clock);

        let mut child_clock = root_clock.clone_for_child();
        assert_eq!(child_clock.as_nanos(), root_clock.as_nanos());
        publish(&mut time, child, &child_clock);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(133));
        assert_eq!(time.threads_time(child), child_clock.as_nanos());
        child_clock.add_syscall_with_cost(7);
        let mut split_rcbs = child_clock.clone();
        split_rcbs.add_rcbs_with_multiplier(1, RcbTimeMultiplier::from_f64(0.5));
        split_rcbs.add_rcbs_with_multiplier(3, RcbTimeMultiplier::from_f64(0.5));
        child_clock.add_rcbs_with_multiplier(4, RcbTimeMultiplier::from_f64(0.5));
        assert_eq!(split_rcbs.as_nanos(), child_clock.as_nanos());
        assert_eq!(split_rcbs.inherited_nanos(), child_clock.inherited_nanos());
        child_clock.add_cpuid();
        child_clock.advance_to(child_clock.as_nanos() + LogicalTime::from_nanos(1));
        assert_eq!(child_clock.without_starting(), LogicalTime::from_nanos(238));
        let snapshot = child_clock.clone();
        assert_eq!(snapshot.inherited_nanos(), LogicalTime::from_nanos(133));
        publish(&mut time, child, &snapshot);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(238));
        publish(&mut time, child, &snapshot);
        publish(&mut time, child, &snapshot);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(238));

        let mut grandchild_clock = child_clock.clone_for_child();
        assert_eq!(
            grandchild_clock.inherited_nanos(),
            LogicalTime::from_nanos(238)
        );
        publish(&mut time, grandchild, &grandchild_clock);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(238));
        grandchild_clock.add_syscall_with_cost(3);
        grandchild_clock.advance_to(grandchild_clock.as_nanos() + LogicalTime::from_nanos(1));
        publish(&mut time, grandchild, &grandchild_clock);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(245));
        assert_eq!(time.threads_time(child), child_clock.as_nanos());
        assert_eq!(time.threads_time(grandchild), grandchild_clock.as_nanos());
        assert_eq!(
            time.threads_duration(grandchild),
            LogicalTime::from_nanos(245)
        );

        time.add_scheduler_time();
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_000_245));
    }

    #[test]
    fn exec_preserves_inherited_baselines_and_each_retired_threads_work() {
        let config = Config {
            epoch: DateTime::from_timestamp(1_767_225_600, 13).unwrap(),
            ..Config::default()
        };
        let ancestor = DetTid::from_raw(3);
        let leader = DetTid::from_raw(4);
        let worker = DetTid::from_raw(5);
        let child_after_exec = DetTid::from_raw(6);
        let mut time = GlobalTime::new(&config);
        let start = time.as_nanos();
        let mut ancestor_clock = DetTime::new(&config);
        ancestor_clock.advance_to(start + LogicalTime::from_nanos(1_000));
        publish(&mut time, ancestor, &ancestor_clock);
        let mut leader_clock = ancestor_clock.clone_for_child();
        leader_clock.advance_to(start + LogicalTime::from_nanos(1_100));
        publish(&mut time, leader, &leader_clock);
        let mut worker_clock = leader_clock.clone_for_child();
        worker_clock.advance_to(start + LogicalTime::from_nanos(1_350));
        publish(&mut time, worker, &worker_clock);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_350));

        time.reassign_thread(worker, leader);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_350));
        assert_eq!(time.threads_time(leader), worker_clock.as_nanos());
        assert!(!time.contains_thread(worker));

        // An in-process backend reloads DetTime after exec and restores its
        // absolute clock from the RPC response. The global component must keep
        // the pre-exec baseline even though this fresh local field is zero.
        let mut reloaded = DetTime::new(&config);
        reloaded.advance_to(time.threads_time(leader));
        assert_eq!(reloaded.inherited_nanos(), LogicalTime::ZERO);
        publish(&mut time, leader, &reloaded);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_350));
        reloaded.advance_to(reloaded.as_nanos() + LogicalTime::from_nanos(1));
        publish(&mut time, leader, &reloaded);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_351));
        // A subsequent leader exec retains the same component and baseline.
        time.reassign_thread(leader, leader);
        let mut leader_reload = DetTime::new(&config);
        leader_reload.advance_to(time.threads_time(leader));
        publish(&mut time, leader, &leader_reload);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_351));
        let mut after_exec = reloaded.clone_for_child();
        publish(&mut time, child_after_exec, &after_exec);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_351));
        after_exec.advance_to(after_exec.as_nanos() + LogicalTime::from_nanos(1));
        publish(&mut time, child_after_exec, &after_exec);
        assert_eq!(time.as_nanos(), start + LogicalTime::from_nanos(1_352));
    }

    #[test]
    fn inherited_clock_survives_rpc_serialization_and_legacy_json_defaults() {
        let mut wire_lengths = Vec::new();
        for nanos in [0, 1, 13, 999, 1_000] {
            let epoch = DateTime::from_timestamp(1_767_225_600, nanos).unwrap();
            let origin = LogicalTime::from_nanos(epoch.timestamp_nanos_opt().unwrap() as u64);
            let mut parent = DetTime::from(&epoch);
            parent.add_syscall_with_cost(13);
            let child = parent.clone_for_child();
            // The following tuple field and full-consumption check detect a
            // skipped clock field consuming bytes from the next RPC argument.
            let wire = bincode::serde::encode_to_vec(
                (child.clone(), 0x1234_5678_u64),
                bincode::config::legacy(),
            )
            .unwrap();
            wire_lengths.push(wire.len());
            let ((restored, following), consumed): ((DetTime, u64), usize) =
                bincode::serde::decode_from_slice(&wire, bincode::config::legacy()).unwrap();
            assert_eq!(consumed, wire.len());
            assert_eq!(following, 0x1234_5678);
            assert_eq!(restored.as_nanos(), origin + LogicalTime::from_nanos(13));
            assert_eq!(restored.inherited_nanos(), LogicalTime::from_nanos(13));

            let mut legacy = serde_json::to_value(parent.clone()).unwrap();
            legacy.as_object_mut().unwrap().remove("inherited_nanos");
            legacy
                .as_object_mut()
                .unwrap()
                .remove("starting_submicro_nanos");
            let restored: DetTime = serde_json::from_value(legacy).unwrap();
            assert_eq!(
                restored.as_nanos(),
                parent.as_nanos() - LogicalTime::from_nanos(u64::from(nanos % 1_000))
            );
            assert_eq!(restored.inherited_nanos(), LogicalTime::ZERO);
            assert_eq!(restored.as_duration(), Duration::from_nanos(13));
        }
        assert!(wire_lengths.iter().all(|length| *length == wire_lengths[0]));

        // Actual old positional bytes (including a following u64), captured
        // before the layout change. JSON defaults are not binary compatibility:
        // old/new coordinator and plugin pairs must refuse by wire fingerprint.
        const OLD_RPC: &[u8] = &[
            1, 0, 0, 0, 0, 0, 0, 0, 1, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 240,
            63, 13, 0, 0, 0, 0, 0, 0, 0, 120, 86, 52, 18, 0, 0, 0, 0,
        ];
        assert!(
            bincode::serde::decode_from_slice::<(DetTime, u64), _>(
                OLD_RPC,
                bincode::config::legacy(),
            )
            .is_err()
        );
    }

    #[test]
    fn serialized_origin_remainder_must_be_canonical() {
        for invalid in [1_000, u16::MAX] {
            let mut json = serde_json::to_value(DetTime::zero()).unwrap();
            json["starting_submicro_nanos"] = invalid.into();
            assert!(serde_json::from_value::<DetTime>(json).is_err());
            let invalid_clock = DetTime {
                starting_submicro_nanos: invalid,
                ..DetTime::zero()
            };
            let wire =
                bincode::serde::encode_to_vec(invalid_clock, bincode::config::legacy()).unwrap();
            assert!(
                bincode::serde::decode_from_slice::<DetTime, _>(&wire, bincode::config::legacy())
                    .is_err()
            );
        }
    }

    #[test]
    #[should_panic(expected = "beyond its local duration")]
    fn inherited_baseline_cannot_exceed_the_first_local_duration() {
        let config = Config::default();
        let mut time = GlobalTime::new(&config);
        time.update_global_time(
            DetTid::from_raw(3),
            time.as_nanos(),
            LogicalTime::from_nanos(1),
        );
    }

    #[test]
    fn exec_reassigns_survivor_clock_without_losing_aggregate_time() {
        let config = Config::default();
        let mut time = GlobalTime::new(&config);
        let leader = DetTid::from_raw(17);
        let worker = DetTid::from_raw(18);
        let start = DetTime::new(&config).as_nanos();
        let leader_time = start + LogicalTime::from_nanos(100);
        let worker_time = start + LogicalTime::from_nanos(250);
        time.update_global_time(leader, leader_time, LogicalTime::ZERO);
        time.update_global_time(worker, worker_time, LogicalTime::ZERO);
        let total_before = time.as_nanos();

        time.reassign_thread(worker, leader);

        assert_eq!(time.as_nanos(), total_before);
        assert_eq!(time.threads_time(leader), worker_time);
        assert!(time.contains_thread(leader));
        assert!(!time.contains_thread(worker));
    }
}
