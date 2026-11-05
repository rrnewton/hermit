//! Burn rate: how fast a meter moved over the last 15 minutes, 1 hour, 3 hours and 24 hours.
//!
//! A meter is a percentage sampled now and then. Over a window, the burn is the sum of its
//! increases between consecutive samples. Two rules keep resets from reading as negative burn:
//!
//! - when a sample is taken after the previous sample's reset time, or the reset time moved
//!   forward, the window reset in between, and the increase is the new value itself (what was
//!   used since the reset; anything used between the old sample and the reset is not seen);
//! - otherwise a decrease counts as zero (providers round, and a window can shed old usage).
//!
//! A segment that starts before the window is prorated linearly, so a sparse history still
//! gives the share of its increase that fell inside the window. The rate divides by the time
//! actually covered, not the nominal window, and that span is reported so a reader can see when a
//! "24 h" figure rests on twenty minutes of history.

use crate::model::Sample;
use serde::Serialize;

/// The windows reported, as `(name, seconds)`.
pub const WINDOWS: [(&str, i64); 4] =
    [("15m", 900), ("1h", 3_600), ("3h", 10_800), ("24h", 86_400)];

/// Two readings at most this far apart in reset time are the same window (providers jitter the
/// fractional seconds of `resets_at`).
const RESET_SLACK: i64 = 120;

/// One observation of a meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// Unix seconds.
    pub ts: i64,
    /// Value: a percentage for plan meters, a cumulative count for token counters.
    pub value: f64,
    /// When the window resets, if known.
    pub resets_at: Option<i64>,
}

/// Burn over one window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Burn {
    /// Window name (`15m`, `1h`, `3h`, `24h`).
    pub window: &'static str,
    /// Nominal window length in seconds.
    pub window_secs: i64,
    /// Increase inside the window (percentage points, or tokens).
    pub used: f64,
    /// Seconds of the window the samples actually cover.
    pub covered_secs: i64,
    /// `used` per hour of covered time.
    pub per_hour: f64,
    /// Samples that fell inside the window (the baseline before it not counted).
    pub samples: usize,
    /// Resets detected inside the window.
    pub resets: u32,
}

fn reset_between(a: &Point, b: &Point) -> bool {
    match (a.resets_at, b.resets_at) {
        (Some(ra), _) if b.ts >= ra + RESET_SLACK / 2 => true,
        (Some(ra), Some(rb)) => rb > ra + RESET_SLACK,
        _ => false,
    }
}

/// Burn of `points` (sorted by time) over the `window_secs` ending at `now`. `None` when fewer
/// than two usable points exist or they cover under a minute.
pub fn burn(points: &[Point], now: i64, window: &'static str, window_secs: i64) -> Option<Burn> {
    let start = now - window_secs;
    let usable: Vec<&Point> = points.iter().filter(|p| p.ts <= now).collect();
    let first_inside = usable.iter().position(|p| p.ts > start)?;
    let from = first_inside.saturating_sub(1);
    let series = &usable[from..];
    if series.len() < 2 {
        return None;
    }
    let mut used = 0.0;
    let mut resets = 0;
    for pair in series.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let mut inc = if reset_between(a, b) {
            resets += 1;
            b.value
        } else {
            (b.value - a.value).max(0.0)
        };
        if a.ts < start && b.ts > a.ts {
            inc *= (b.ts - start) as f64 / (b.ts - a.ts) as f64;
        }
        used += inc;
    }
    let last = series[series.len() - 1].ts;
    let covered = last - series[0].ts.max(start);
    if covered < 60 {
        return None;
    }
    Some(Burn {
        window,
        window_secs,
        used,
        covered_secs: covered,
        per_hour: used * 3_600.0 / covered as f64,
        samples: usable.len() - first_inside,
        resets,
    })
}

/// Points for one plan meter of one provider from the history.
pub fn meter_points(samples: &[Sample], provider: &str, meter_id: &str) -> Vec<Point> {
    samples
        .iter()
        .filter(|s| s.provider == provider)
        .filter_map(|s| {
            s.meter(meter_id).map(|m| Point {
                ts: s.ts,
                value: m.used_pct,
                resets_at: m.resets_at,
            })
        })
        .collect()
}

/// Points for a provider's cumulative token counter (total field) from the history.
pub fn token_points(samples: &[Sample], provider: &str) -> Vec<Point> {
    samples
        .iter()
        .filter(|s| s.provider == provider)
        .filter_map(|s| {
            s.tokens_cumulative.map(|t| Point {
                ts: s.ts,
                value: t.total as f64,
                resets_at: None,
            })
        })
        .collect()
}

/// Burns over every window in [`WINDOWS`].
pub fn all_windows(points: &[Point], now: i64) -> Vec<Burn> {
    WINDOWS
        .iter()
        .filter_map(|(name, secs)| burn(points, now, name, *secs))
        .collect()
}

/// Where a meter is heading at a given burn rate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Projection {
    /// The window whose rate was used.
    pub basis: &'static str,
    /// Percentage points per hour.
    pub per_hour: f64,
    /// Seconds until 100 % at that rate; `None` when the rate is zero.
    pub secs_to_full: Option<i64>,
    /// Whether 100 % would be reached before the window resets (`None` when either is unknown).
    pub full_before_reset: Option<bool>,
}

/// Project a meter at `used_pct` forward, preferring the 1 h rate, then 3 h, then 15 m.
pub fn project(
    burns: &[Burn],
    used_pct: f64,
    resets_at: Option<i64>,
    now: i64,
) -> Option<Projection> {
    let basis = ["1h", "3h", "15m", "24h"]
        .iter()
        .find_map(|w| burns.iter().find(|b| b.window == *w))?;
    let secs_to_full = (basis.per_hour > 1e-9)
        .then(|| (((100.0 - used_pct).max(0.0) / basis.per_hour) * 3_600.0) as i64);
    let full_before_reset = match (secs_to_full, resets_at) {
        (Some(s), Some(r)) => Some(now + s < r),
        (None, Some(_)) => Some(false),
        _ => None,
    };
    Some(Projection {
        basis: basis.window,
        per_hour: basis.per_hour,
        secs_to_full,
        full_before_reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(ts: i64, value: f64, resets_at: Option<i64>) -> Point {
        Point {
            ts,
            value,
            resets_at,
        }
    }

    #[test]
    fn steady_burn_over_each_window() {
        // One sample every 15 minutes for 24 h, +1 point each, no reset.
        let now = 100_000;
        let pts: Vec<_> = (0..=96)
            .map(|i| p(now - (96 - i) * 900, i as f64 * 1.0, Some(now + 50_000)))
            .collect();
        let b15 = burn(&pts, now, "15m", 900).unwrap();
        assert_eq!(b15.used, 1.0);
        assert_eq!(b15.covered_secs, 900);
        assert!((b15.per_hour - 4.0).abs() < 1e-9);
        let b1h = burn(&pts, now, "1h", 3_600).unwrap();
        assert_eq!(b1h.used, 4.0);
        assert_eq!(b1h.samples, 4);
        let b24 = burn(&pts, now, "24h", 86_400).unwrap();
        assert_eq!(b24.used, 96.0);
        assert!((b24.per_hour - 4.0).abs() < 1e-9);
        assert_eq!(all_windows(&pts, now).len(), 4);
    }

    #[test]
    fn reset_counts_new_value_not_negative() {
        // 80% before a reset at t=1000, then 5% after it.
        let pts = [
            p(0, 70.0, Some(1_000)),
            p(900, 80.0, Some(1_000)),
            p(1_800, 5.0, Some(19_000)),
        ];
        let b = burn(&pts, 1_800, "1h", 3_600).unwrap();
        assert_eq!(b.used, 10.0 + 5.0);
        assert_eq!(b.resets, 1);
    }

    #[test]
    fn reset_time_moving_forward_is_a_reset_even_without_passing_it() {
        // Same reported reset with jitter is not a reset; a big jump is.
        let a = p(0, 50.0, Some(10_000));
        assert!(!reset_between(&a, &p(60, 51.0, Some(10_030))));
        assert!(reset_between(&a, &p(60, 2.0, Some(30_000))));
        assert!(reset_between(&a, &p(10_100, 2.0, None)));
        assert!(!reset_between(&p(0, 0.0, None), &p(60, 2.0, Some(30_000))));
    }

    #[test]
    fn decreases_without_reset_are_zero() {
        let pts = [
            p(0, 10.0, Some(99_999)),
            p(600, 9.0, Some(99_999)),
            p(1_200, 12.0, Some(99_999)),
        ];
        assert_eq!(burn(&pts, 1_200, "1h", 3_600).unwrap().used, 3.0);
    }

    #[test]
    fn baseline_before_window_is_prorated() {
        // 0 at t=0, 10 at t=2000: window [1000, 2000] gets half of the 10.
        let pts = [p(0, 0.0, None), p(2_000, 10.0, None)];
        let b = burn(&pts, 2_000, "w", 1_000).unwrap();
        assert!((b.used - 5.0).abs() < 1e-9);
        assert_eq!(b.covered_secs, 1_000);
        assert_eq!(b.samples, 1);
    }

    #[test]
    fn partial_history_reports_true_coverage() {
        let pts = [p(9_000, 1.0, None), p(9_600, 3.0, None)];
        let b = burn(&pts, 10_000, "24h", 86_400).unwrap();
        assert_eq!(b.covered_secs, 600);
        assert!((b.per_hour - 12.0).abs() < 1e-9);
    }

    #[test]
    fn too_little_history_is_none() {
        assert!(burn(&[], 10, "1h", 3_600).is_none());
        assert!(burn(&[p(5, 1.0, None)], 10, "1h", 3_600).is_none());
        assert!(burn(&[p(0, 1.0, None), p(30, 2.0, None)], 30, "1h", 3_600).is_none());
        // All points older than the window: nothing inside it.
        assert!(burn(&[p(0, 1.0, None), p(100, 2.0, None)], 10_000, "15m", 900).is_none());
        // Points in the future of `now` are ignored.
        assert!(burn(&[p(0, 1.0, None), p(500, 2.0, None)], 100, "1h", 3_600).is_none());
    }

    #[test]
    fn projection_against_reset() {
        let b = Burn {
            window: "1h",
            window_secs: 3_600,
            used: 10.0,
            covered_secs: 3_600,
            per_hour: 10.0,
            samples: 4,
            resets: 0,
        };
        let pr = project(&[b], 60.0, Some(10_000 + 5 * 3_600), 10_000).unwrap();
        assert_eq!(pr.secs_to_full, Some(4 * 3_600));
        assert_eq!(pr.full_before_reset, Some(true));
        let pr = project(&[b], 60.0, Some(10_000 + 3 * 3_600), 10_000).unwrap();
        assert_eq!(pr.full_before_reset, Some(false));
        let idle = Burn {
            per_hour: 0.0,
            used: 0.0,
            ..b
        };
        let pr = project(&[idle], 60.0, Some(20_000), 10_000).unwrap();
        assert_eq!(pr.secs_to_full, None);
        assert_eq!(pr.full_before_reset, Some(false));
        assert!(project(&[], 1.0, None, 0).is_none());
    }

    #[test]
    fn points_from_samples() {
        use crate::model::{Meter, Status, Tokens};
        let mut a = Sample::new("claude", 10, Status::Ok);
        a.meters.push(Meter {
            id: "session".into(),
            label: "s".into(),
            used_pct: 4.0,
            resets_at: Some(99),
            window_mins: None,
        });
        let mut b = Sample::new("codex", 20, Status::Unavailable);
        b.tokens_cumulative = Some(Tokens {
            total: 500,
            ..Tokens::default()
        });
        let c = Sample::new("claude", 30, Status::Error);
        let all = [a, b, c];
        assert_eq!(
            meter_points(&all, "claude", "session"),
            vec![p(10, 4.0, Some(99))]
        );
        assert_eq!(token_points(&all, "codex"), vec![p(20, 500.0, None)]);
        assert!(meter_points(&all, "claude", "weekly_all").is_empty());
    }
}
