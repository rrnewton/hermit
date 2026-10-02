//! `core::time`, plus `Instant`, `SystemTime` and `UNIX_EPOCH` as values:
//! their arithmetic and comparisons, but no `now()` or `elapsed()`, which read
//! an operating-system clock and stay absent. A `SystemTime` here cannot be
//! earlier than `UNIX_EPOCH`.

use core::fmt;
use core::ops::Add;
use core::ops::AddAssign;
use core::ops::Sub;
use core::ops::SubAssign;
pub use core::time::*;

/// A point on a monotonic clock, as the offset from an unspecified origin.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Instant(Duration);

impl Instant {
    /// `self - earlier`, zero if `earlier` is later.
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// `self - earlier`, if `earlier` is not later.
    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }

    /// `self - earlier`, zero if `earlier` is later.
    pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// `self + d`, if representable.
    pub fn checked_add(&self, d: Duration) -> Option<Instant> {
        self.0.checked_add(d).map(Instant)
    }

    /// `self - d`, if representable.
    pub fn checked_sub(&self, d: Duration) -> Option<Instant> {
        self.0.checked_sub(d).map(Instant)
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, d: Duration) -> Instant {
        self.checked_add(d)
            .expect("overflow when adding duration to instant")
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

impl Sub<Duration> for Instant {
    type Output = Instant;

    fn sub(self, d: Duration) -> Instant {
        self.checked_sub(d)
            .expect("overflow when subtracting duration from instant")
    }
}

impl SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, d: Duration) {
        *self = *self - d;
    }
}

impl Sub<Instant> for Instant {
    type Output = Duration;

    fn sub(self, other: Instant) -> Duration {
        self.duration_since(other)
    }
}

/// A wall-clock time, as the offset from `UNIX_EPOCH`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SystemTime(Duration);

/// 1970-01-01T00:00:00Z.
pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::ZERO);

impl SystemTime {
    /// 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: SystemTime = UNIX_EPOCH;

    /// `self - earlier`, or the error's duration `earlier - self`.
    pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
        self.0
            .checked_sub(earlier.0)
            .ok_or_else(|| SystemTimeError(earlier.0 - self.0))
    }

    /// `self + d`, if representable.
    pub fn checked_add(&self, d: Duration) -> Option<SystemTime> {
        self.0.checked_add(d).map(SystemTime)
    }

    /// `self - d`, if representable.
    pub fn checked_sub(&self, d: Duration) -> Option<SystemTime> {
        self.0.checked_sub(d).map(SystemTime)
    }
}

impl Add<Duration> for SystemTime {
    type Output = SystemTime;

    fn add(self, d: Duration) -> SystemTime {
        self.checked_add(d)
            .expect("overflow when adding duration to instant")
    }
}

impl AddAssign<Duration> for SystemTime {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

impl Sub<Duration> for SystemTime {
    type Output = SystemTime;

    fn sub(self, d: Duration) -> SystemTime {
        self.checked_sub(d)
            .expect("overflow when subtracting duration from instant")
    }
}

impl SubAssign<Duration> for SystemTime {
    fn sub_assign(&mut self, d: Duration) {
        *self = *self - d;
    }
}

/// `SystemTime::duration_since`'s error: how much later `earlier` was.
#[derive(Clone, Debug)]
pub struct SystemTimeError(Duration);

impl SystemTimeError {
    /// How much later the argument was.
    pub fn duration(&self) -> Duration {
        self.0
    }
}

impl fmt::Display for SystemTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("second time provided was later than self")
    }
}

impl core::error::Error for SystemTimeError {}
