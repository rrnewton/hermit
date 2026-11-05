//! Token bucket plus a sliding one-hour window, per request class.
//!
//! A call of cost `c` is admitted when the bucket holds at least `min(c, burst)` tokens AND the
//! last 3600 s of admitted cost plus `c` stays within the hourly cap. Admission subtracts the full
//! cost, so the level may go negative: the calls after an expensive call wait until the debt is
//! repaid. The wrapper refuses any call costing more than the burst except a watch loop, whose
//! requests are spread over its poll interval, so only a watch goes into debt. The bucket
//! refills at `per_minute / 60` tokens per second up to `burst`.

use crate::config::ClassLimits;
use serde::{Deserialize, Serialize};

/// Length of the sliding window, seconds.
pub const HOUR: f64 = 3600.0;

/// Slack for floating-point error. Unix times near 1.8e9 carry only about 2.4e-7 s of
/// precision, so a sleep of exactly the computed wait can land a hair short of the boundary;
/// without this slack the admission loop would compute a tiny positive wait forever.
pub const EPS: f64 = 1e-6;

/// Round a positive wait up to whole milliseconds, and to at least one millisecond, so that every
/// sleep moves the clock past the floating-point noise of the boundary it waits for.
pub fn round_up_ms(secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    if !secs.is_finite() {
        return secs;
    }
    ((secs * 1000.0).ceil() / 1000.0).max(0.001)
}

/// One class's bucket and hourly history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    /// Tokens available (negative = debt).
    pub level: f64,
    /// When `level` was last brought up to date (Unix seconds).
    pub updated: f64,
    /// Admitted calls in the last hour: `(time, cost)`, oldest first.
    #[serde(default)]
    pub window: Vec<(f64, u32)>,
    /// Nothing in this class is admitted before this time (Unix seconds), whatever the limits
    /// are by then. Set by state recovery: the lost history may have held a full hour of calls.
    /// A window entry sized by the hourly cap in force at recovery would not do, because raising
    /// the cap later (removing a tightening override) would reopen the class early.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_until: Option<f64>,
}

/// Limits after the global-feedback adjustment: halve rate, burst and hourly cap.
pub fn halved(l: ClassLimits) -> ClassLimits {
    ClassLimits {
        per_minute: l.per_minute / 2.0,
        burst: (l.burst / 2.0).floor().max(1.0),
        per_hour: (l.per_hour / 2).max(1),
    }
}

impl Bucket {
    /// A full bucket (first use on this host).
    pub fn full(limits: ClassLimits, now: f64) -> Self {
        Self {
            level: limits.burst,
            updated: now,
            window: Vec::new(),
            blocked_until: None,
        }
    }

    /// An empty bucket with no hourly history.
    pub fn empty(now: f64) -> Self {
        Self {
            level: 0.0,
            updated: now,
            window: Vec::new(),
            blocked_until: None,
        }
    }

    /// An empty bucket that admits nothing in this class for the next hour, independently of
    /// the limits in force later (see [`Bucket::blocked_until`]). Used when the state file was
    /// unusable, because the lost history may have held a full hour of calls.
    pub fn saturated(now: f64) -> Self {
        Self {
            level: 0.0,
            updated: now,
            window: Vec::new(),
            blocked_until: Some(now + HOUR),
        }
    }

    /// Seconds until a recovery block ends (0 when there is none or it has passed).
    pub fn blocked_wait(&self, now: f64) -> f64 {
        self.blocked_until
            .map(|b| round_up_ms(b - now))
            .unwrap_or(0.0)
    }

    /// Refill for the time elapsed since the last update and drop window entries older than an
    /// hour. A clock that moved backwards refills nothing.
    pub fn refresh(&mut self, limits: ClassLimits, now: f64) {
        let elapsed = (now - self.updated).max(0.0);
        let rate = limits.per_minute / 60.0;
        self.level = (self.level + elapsed * rate).min(limits.burst);
        if now > self.updated {
            self.updated = now;
        }
        self.window.retain(|(t, _)| *t + HOUR > now + EPS);
    }

    /// Cost admitted in the last hour (call [`Bucket::refresh`] first).
    pub fn hour_used(&self) -> u64 {
        self.window.iter().map(|(_, c)| u64::from(*c)).sum()
    }

    /// Seconds until the bucket holds `min(cost, burst)` tokens (call refresh first).
    pub fn bucket_wait(&self, limits: ClassLimits, cost: u32) -> f64 {
        let need = f64::from(cost).min(limits.burst);
        if self.level + EPS >= need {
            return 0.0;
        }
        let rate = limits.per_minute / 60.0;
        if rate <= 0.0 {
            return f64::INFINITY;
        }
        round_up_ms((need - self.level) / rate)
    }

    /// Seconds until the hourly window has room for `cost` and any recovery block has ended
    /// (call refresh first). A cost larger than the whole cap can never fit, so the wait is
    /// infinite and the caller refuses it.
    pub fn hour_wait(&self, limits: ClassLimits, cost: u32, now: f64) -> f64 {
        let cap = u64::from(limits.per_hour);
        let cost = u64::from(cost);
        if cost > cap {
            return f64::INFINITY;
        }
        self.window_wait(cap, cost, now).max(self.blocked_wait(now))
    }

    fn window_wait(&self, cap: u64, cost: u64, now: f64) -> f64 {
        let used = self.hour_used();
        if used + cost <= cap {
            return 0.0;
        }
        let mut freed = 0u64;
        for (t, c) in &self.window {
            freed += u64::from(*c);
            if used - freed + cost <= cap {
                return round_up_ms(t + HOUR - now);
            }
        }
        self.window
            .last()
            .map(|(t, _)| round_up_ms(t + HOUR - now))
            .unwrap_or(0.0)
    }

    /// Record an admitted call.
    pub fn charge(&mut self, cost: u32, now: f64) {
        self.level -= f64::from(cost);
        self.window.push((now, cost));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_limits() -> ClassLimits {
        ClassLimits {
            per_minute: 2.0,
            burst: 1.0,
            per_hour: 30,
        }
    }

    #[test]
    fn writes_are_spaced_thirty_seconds() {
        let l = write_limits();
        let mut b = Bucket::full(l, 0.0);
        b.refresh(l, 0.0);
        assert_eq!(b.bucket_wait(l, 1), 0.0);
        b.charge(1, 0.0);
        b.refresh(l, 0.0);
        assert!((b.bucket_wait(l, 1) - 30.0).abs() < 1e-9);
        b.refresh(l, 29.0);
        assert!(b.bucket_wait(l, 1) > 0.0);
        b.refresh(l, 30.0);
        assert_eq!(b.bucket_wait(l, 1), 0.0);
    }

    #[test]
    fn hourly_cap_waits_for_the_oldest_entry() {
        let l = write_limits();
        let mut b = Bucket::full(l, 0.0);
        for i in 0..30 {
            b.charge(1, f64::from(i) * 30.0);
        }
        let now = 900.0;
        b.refresh(l, now);
        assert_eq!(b.hour_used(), 30);
        assert!((b.hour_wait(l, 1, now) - 2700.0).abs() < 1e-9);
        b.refresh(l, 3600.5);
        assert_eq!(b.hour_used(), 29);
        assert_eq!(b.hour_wait(l, 1, 3600.5), 0.0);
    }

    #[test]
    fn debt_model_for_expensive_calls() {
        let l = ClassLimits {
            per_minute: 20.0,
            burst: 10.0,
            per_hour: 500,
        };
        let mut b = Bucket::full(l, 0.0);
        b.refresh(l, 0.0);
        // A paginated read costs 10: admitted on a full bucket, leaves it empty.
        assert_eq!(b.bucket_wait(l, 10), 0.0);
        b.charge(10, 0.0);
        // A watch costing 20 needs only a full bucket (burst 10), then goes 10 into debt.
        b.refresh(l, 30.0);
        assert_eq!(b.bucket_wait(l, 20), 0.0);
        b.charge(20, 30.0);
        assert!((b.level + 10.0).abs() < 1e-9);
        // The next single read waits until the debt is repaid: 11 tokens at 1/3 per second.
        b.refresh(l, 30.0);
        assert!((b.bucket_wait(l, 1) - 33.0).abs() < 1e-9);
    }

    #[test]
    fn halving() {
        let h = halved(ClassLimits {
            per_minute: 20.0,
            burst: 10.0,
            per_hour: 500,
        });
        assert_eq!((h.per_minute, h.burst, h.per_hour), (10.0, 5.0, 250));
        let h = halved(write_limits());
        assert_eq!((h.per_minute, h.burst, h.per_hour), (1.0, 1.0, 15));
    }

    /// A cost above the whole hourly cap can never fit, even in an empty window, so it is never
    /// admitted (the wrapper refuses it at once instead of waiting).
    #[test]
    fn cost_above_the_hourly_cap_is_never_admitted() {
        let l = ClassLimits {
            per_minute: 60.0,
            burst: 10.0,
            per_hour: 5,
        };
        let mut b = Bucket::full(l, 0.0);
        assert!(b.hour_wait(l, 50, 0.0).is_infinite(), "empty window");
        assert!(b.hour_wait(l, 6, 0.0).is_infinite(), "one over the cap");
        assert_eq!(b.hour_wait(l, 5, 0.0), 0.0, "exactly the cap fits");
        b.charge(1, 0.0);
        b.charge(1, 10.0);
        b.refresh(l, 20.0);
        assert!(b.hour_wait(l, 50, 20.0).is_infinite());
        assert!((b.hour_wait(l, 5, 20.0) - 3590.0).abs() < 1e-9);
    }

    /// At real Unix times a sleep of exactly the computed wait must be enough: the next check
    /// admits instead of asking for another sub-microsecond sleep (which would spin forever on a
    /// clock that cannot represent the step).
    #[test]
    fn sleeping_the_computed_wait_admits_at_real_epoch_times() {
        let read = ClassLimits {
            per_minute: 20.0,
            burst: 10.0,
            per_hour: 500,
        };
        for limits in [read, write_limits()] {
            let mut now = 1_791_126_252.356_f64;
            let mut b = Bucket::full(limits, now);
            for _ in 0..200 {
                b.refresh(limits, now);
                let mut w = b.bucket_wait(limits, 1).max(b.hour_wait(limits, 1, now));
                if w > 0.0 {
                    assert!(w >= 0.001, "wait {w} below a millisecond");
                    now += w;
                    b.refresh(limits, now);
                    w = b.bucket_wait(limits, 1).max(b.hour_wait(limits, 1, now));
                    assert_eq!(
                        w, 0.0,
                        "still waiting {w} s after sleeping the computed wait"
                    );
                }
                b.charge(1, now);
                now += 1.234;
            }
        }
    }

    /// A saturated bucket (state recovery) admits nothing for a full hour, however long the
    /// refill takes, and admits again once that hour has passed. The block does not depend on
    /// the limits: it was a window entry of `per_hour` cost before, which shrank when the cap was
    /// raised after recovery (a recovery under a tightened cap of 1 reopened after 900 s once the
    /// tightening was removed). The block now holds for the full hour under every cap.
    #[test]
    fn saturated_bucket_blocks_for_an_hour() {
        let l = write_limits();
        let now = 1000.0;
        let mut b = Bucket::saturated(now);
        b.refresh(l, now);
        assert!((b.hour_wait(l, 1, now) - HOUR).abs() < 1e-6);
        b.refresh(l, now + 600.0);
        assert_eq!(b.bucket_wait(l, 1), 0.0, "the token bucket refilled");
        assert!(
            b.hour_wait(l, 1, now + 600.0) > 2999.0,
            "the hour cap did not"
        );
        for per_hour in [1, 30, 500, 5000] {
            let raised = ClassLimits { per_hour, ..l };
            assert!(
                b.hour_wait(raised, 1, now + 900.0) > 2699.0,
                "cap {per_hour} reopened the class early"
            );
        }
        // The block survives a save and load.
        let text = serde_json::to_string(&b).expect("encode");
        let back: Bucket = serde_json::from_str(&text).expect("decode");
        assert_eq!(back.blocked_until, Some(now + HOUR));
        b.refresh(l, now + HOUR + 0.01);
        assert_eq!(b.hour_wait(l, 1, now + HOUR + 0.01), 0.0);
        assert_eq!(b.blocked_wait(now + HOUR + 0.01), 0.0);
    }

    #[test]
    fn round_up_ms_rounds_up_and_never_to_zero() {
        assert_eq!(round_up_ms(0.0), 0.0);
        assert_eq!(round_up_ms(-1.0), 0.0);
        assert_eq!(round_up_ms(1e-9), 0.001);
        assert_eq!(round_up_ms(2.0001), 2.001);
        assert_eq!(round_up_ms(30.0), 30.0);
        assert!(round_up_ms(f64::INFINITY).is_infinite());
    }
}
