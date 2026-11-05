//! Coverage: which stretches of a scope's history the cache holds EVERY message of.
//!
//! Pure interval algebra, with no SQL, so that it can be tested against a brute-force model (the
//! property test at the bottom of this file) rather than only by example.
//!
//! # What an interval claims
//!
//! An [`Interval`] `[t0, t1)` with `listed_ms = L` says: every message whose provider creation time
//! falls in `[t0, t1)` was listed by a read that started at local time `L`. `full_ms`, when set,
//! says edits and deletions in that stretch were reflected as of that time too, and `complete`
//! says the source could not have left a row out. A claim may be WEAKER than the truth — an older
//! `listed_ms`, `complete` false — but never stronger: a false "complete" is how a page drops a
//! row it should have kept, or a count lies.
//!
//! Two clocks meet here and are never compared with each other: `t0`/`t1` and every head are
//! PROVIDER time (when a message says it was written); `listed_ms`, `full_ms` and
//! `head_listed_ms` are LOCAL time (when this server read it).
//!
//! # The unsettled band
//!
//! A provider can list a message up to [`SETTLE_MS`] after its own timestamp. So a read of the
//! newest end proves nothing about the last two minutes before its newest row: a message written
//! then may simply not be visible yet. [`Head`] records that band, coverage never enters it, and
//! nothing inside it is taken as deleted for being absent.
//!
//! Two things settle it, and neither compares the local clock with a provider's:
//!
//! - **A newer row.** When the provider lists a row written at `N`, its own clock has passed `N`,
//!   so every message written before `N - SETTLE_MS` is visible to that read: the band never
//!   reaches further back than [`SETTLE_MS`] below the newest row of the latest read. In a busy
//!   channel, read once a minute with a message a minute, the band therefore stays two minutes
//!   wide rather than growing for as long as the channel stays busy.
//! - **Time.** A later read of the newest end that started [`SETTLE_AFTER_LOCAL_MS`] (local)
//!   after the read that opened the band settles everything up to the old head: that is how a
//!   quiet channel's band closes.

/// How long after its own timestamp a provider may first list a message, in provider
/// milliseconds. The same bound the forward walk overlaps by (`timeline_forward`).
pub const SETTLE_MS: i64 = 120_000;
/// How long, in local milliseconds, after the read that set a head a later read of the newest end
/// must have started to settle that head's band.
pub const SETTLE_AFTER_LOCAL_MS: i64 = 600_000;
/// Neighbouring intervals whose times agree within this many milliseconds are merged, keeping
/// the smaller times.
pub const MERGE_WITHIN_MS: i64 = 120_000;
/// The most intervals one scope keeps. Past it, the oldest neighbours are merged, and failing
/// that the oldest interval is dropped: a claim may be given up, never invented.
pub const MAX_INTERVALS: usize = 512;
/// Only intervals ending this long before the scope's head are merged to honour
/// [`MAX_INTERVALS`].
pub const OLD_AFTER_MS: i64 = 48 * 60 * 60 * 1000;

/// What kind of read stamped an interval. Diagnostic only; nothing decides on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A read a caller asked for: a timeline page, a message window.
    Read,
    /// The poller's forward walk.
    Poll,
    /// A walk this server made itself, following its own continuation.
    Walk,
    /// A read of an explicit time range.
    Range,
    /// The refresher re-reading what it holds.
    Refresh,
}

impl Source {
    /// The value stored in `coverage.source`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Poll => "poll",
            Self::Walk => "walk",
            Self::Range => "range",
            Self::Refresh => "refresh",
        }
    }

    /// Read a stored `coverage.source`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "read" => Self::Read,
            "poll" => Self::Poll,
            "walk" => Self::Walk,
            "range" => Self::Range,
            "refresh" => Self::Refresh,
            _ => return None,
        })
    }
}

/// One claim: every message created in `[t0_ms, t1_ms)` was listed as of `listed_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    /// Provider time, inclusive.
    pub t0_ms: i64,
    /// Provider time, exclusive.
    pub t1_ms: i64,
    /// Local time: creations complete as of this time.
    pub listed_ms: i64,
    /// Local time: edits and deletions reflected as of this time. Never later than `listed_ms`.
    pub full_ms: Option<i64>,
    /// Whether the source could not have omitted a row.
    pub complete: bool,
    /// What kind of read made the claim.
    pub source: Source,
}

/// The tunables of [`stamp`]. [`Params::default`] is what the cache uses; the property test uses
/// small values so that a small time domain exercises every branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// See [`MERGE_WITHIN_MS`].
    pub merge_within_ms: i64,
    /// See [`MAX_INTERVALS`].
    pub max_intervals: usize,
    /// See [`OLD_AFTER_MS`].
    pub old_after_ms: i64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            merge_within_ms: MERGE_WITHIN_MS,
            max_intervals: MAX_INTERVALS,
            old_after_ms: OLD_AFTER_MS,
        }
    }
}

/// Lay a new claim over a scope's stored intervals, and return the scope's new intervals.
///
/// Split the stored intervals at the new claim's ends; inside it, a stored piece takes the new
/// claim's `listed_ms` and `complete` when the new read is at least as recent, and keeps its own
/// otherwise; `full_ms` takes the later of the two. Gaps inside the new claim become new pieces.
/// Then neighbours that agree (same `complete`, times within [`Params::merge_within_ms`]) are
/// merged, keeping the smaller times; and past [`Params::max_intervals`] the oldest touching
/// neighbours ending [`Params::old_after_ms`] before `head_ms` are merged, then the oldest
/// intervals dropped.
///
/// `existing` must be sorted and disjoint, as every result of this function is.
#[must_use]
pub fn stamp(
    existing: &[Interval],
    new: Interval,
    head_ms: Option<i64>,
    params: &Params,
) -> Vec<Interval> {
    if new.t0_ms >= new.t1_ms {
        return existing.to_vec();
    }
    let mut pieces: Vec<Interval> = Vec::with_capacity(existing.len() + 3);
    let mut cursor = new.t0_ms;
    for stored in existing {
        if stored.t1_ms <= new.t0_ms || stored.t0_ms >= new.t1_ms {
            pieces.push(*stored);
            continue;
        }
        if stored.t0_ms < new.t0_ms {
            pieces.push(Interval {
                t1_ms: new.t0_ms,
                ..*stored
            });
        }
        let from = stored.t0_ms.max(cursor);
        if from > cursor {
            pieces.push(Interval {
                t0_ms: cursor,
                t1_ms: from,
                ..new
            });
        }
        let to = stored.t1_ms.min(new.t1_ms);
        pieces.push(combine(stored, &new, from, to));
        cursor = to;
        if stored.t1_ms > new.t1_ms {
            pieces.push(Interval {
                t0_ms: new.t1_ms,
                ..*stored
            });
        }
    }
    if cursor < new.t1_ms {
        pieces.push(Interval {
            t0_ms: cursor,
            ..new
        });
    }
    pieces.sort_by_key(|piece| piece.t0_ms);
    let mut merged = merge_agreeing(pieces, params.merge_within_ms);
    enforce_cap(&mut merged, head_ms, params);
    merged
}

/// The overlap of a stored piece and a new claim, over `[from, to)`.
fn combine(stored: &Interval, new: &Interval, from: i64, to: i64) -> Interval {
    let newer = new.listed_ms >= stored.listed_ms;
    let (listed_ms, complete, source) = if newer {
        (new.listed_ms, new.complete, new.source)
    } else {
        (stored.listed_ms, stored.complete, stored.source)
    };
    Interval {
        t0_ms: from,
        t1_ms: to,
        listed_ms,
        full_ms: later(stored.full_ms, new.full_ms),
        complete,
        source,
    }
}

fn later(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Whether two touching pieces may become one under the within-tolerance rule.
fn agree(a: &Interval, b: &Interval, within: i64) -> bool {
    a.t1_ms == b.t0_ms
        && a.complete == b.complete
        && (a.listed_ms - b.listed_ms).abs() <= within
        && match (a.full_ms, b.full_ms) {
            (None, None) => true,
            (Some(x), Some(y)) => (x - y).abs() <= within,
            _ => false,
        }
}

/// One piece standing for two touching ones, with the WEAKER claim of the pair: the smaller
/// times, and `complete` only if both were.
fn join(a: &Interval, b: &Interval) -> Interval {
    Interval {
        t0_ms: a.t0_ms,
        t1_ms: b.t1_ms,
        listed_ms: a.listed_ms.min(b.listed_ms),
        full_ms: match (a.full_ms, b.full_ms) {
            (Some(x), Some(y)) => Some(x.min(y)),
            _ => None,
        },
        complete: a.complete && b.complete,
        source: if a.listed_ms <= b.listed_ms {
            a.source
        } else {
            b.source
        },
    }
}

fn merge_agreeing(pieces: Vec<Interval>, within: i64) -> Vec<Interval> {
    let mut out: Vec<Interval> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match out.last_mut() {
            Some(last) if agree(last, &piece, within) => *last = join(last, &piece),
            _ => out.push(piece),
        }
    }
    out
}

fn enforce_cap(intervals: &mut Vec<Interval>, head_ms: Option<i64>, params: &Params) {
    if intervals.len() <= params.max_intervals {
        return;
    }
    let head = head_ms
        .or_else(|| intervals.last().map(|last| last.t1_ms))
        .unwrap_or(i64::MAX);
    let old_before = head.saturating_sub(params.old_after_ms);
    while intervals.len() > params.max_intervals {
        let pair = (0..intervals.len().saturating_sub(1)).find(|&i| {
            intervals[i].t1_ms == intervals[i + 1].t0_ms && intervals[i + 1].t1_ms <= old_before
        });
        match pair {
            Some(i) => {
                let joined = join(&intervals[i], &intervals[i + 1]);
                intervals[i] = joined;
                intervals.remove(i + 1);
            }
            // Nothing old enough touches anything: give up the oldest claim instead. Coverage
            // is a promise about what the cache holds, and dropping one only makes it smaller.
            None => {
                intervals.remove(0);
            }
        }
    }
}

/// The newest end of one scope: nothing newer than `head_ms` was listed at `head_listed_ms`, and
/// `[unsettled_from_ms, head_ms]` is the late-visibility band (empty once settled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// Provider time of the newest row a read of the newest end has listed.
    pub head_ms: i64,
    /// Provider time from which rows may still be appearing late. Greater than `head_ms` once the
    /// band has settled.
    pub unsettled_from_ms: i64,
    /// Local time the read that listed the head started.
    pub head_listed_ms: i64,
}

impl Head {
    /// The band `[from, to]`, inclusive, when it is not empty.
    #[must_use]
    pub fn band(&self) -> Option<(i64, i64)> {
        (self.unsettled_from_ms <= self.head_ms).then_some((self.unsettled_from_ms, self.head_ms))
    }

    /// Whether a provider time lies in the band.
    #[must_use]
    pub fn in_band(&self, ms: i64) -> bool {
        ms >= self.unsettled_from_ms
    }
}

/// A read of the newest end has landed: its newest row was created at `newest_row_ms`, and the
/// read started at local time `listed_ms`. Returns the scope's new head.
///
/// Rows at or below the old head are settled for good once either settling condition holds, and
/// rows more than [`SETTLE_MS`] older than this read's newest row are settled whatever else holds;
/// the band never reopens, and a read that is no newer than the stored head, in either clock,
/// changes nothing. `head_listed_ms` is the time of the latest read of the newest end, except
/// while a band is open and no newer row has arrived: then it stays the time of the read that
/// opened the band, which is what the ten minutes are counted from.
#[must_use]
pub fn advance_head(old: Option<Head>, newest_row_ms: i64, listed_ms: i64) -> Head {
    let Some(old) = old else {
        return Head {
            head_ms: newest_row_ms,
            unsettled_from_ms: newest_row_ms.saturating_sub(SETTLE_MS),
            head_listed_ms: listed_ms,
        };
    };
    if listed_ms <= old.head_listed_ms && newest_row_ms <= old.head_ms {
        return old;
    }
    let head_ms = old.head_ms.max(newest_row_ms);
    let settled = listed_ms >= old.head_listed_ms.saturating_add(SETTLE_AFTER_LOCAL_MS)
        || newest_row_ms > old.head_ms.saturating_add(SETTLE_MS);
    // Whatever was written more than the late bound before this read's newest row was visible to
    // it, settled or not by the rules above.
    let behind_newest = newest_row_ms.saturating_sub(SETTLE_MS);
    let unsettled_from_ms = if settled {
        old.head_ms
            .saturating_add(1)
            .max(head_ms.saturating_sub(SETTLE_MS))
            .max(old.unsettled_from_ms)
    } else {
        old.unsettled_from_ms.max(behind_newest)
    };
    // While a band is open and the head has not moved, the read that opened it keeps its time:
    // otherwise a quiet channel read every minute would restart the ten minutes on every read,
    // and its band would never settle.
    let keeps_band_clock = !settled && newest_row_ms <= old.head_ms && old.band().is_some();
    Head {
        head_ms,
        unsettled_from_ms,
        head_listed_ms: if keeps_band_clock {
            old.head_listed_ms
        } else {
            old.head_listed_ms.max(listed_ms)
        },
    }
}

/// Where an exhaustive listing's claim (and its deletions by absence) must stop: below what is
/// settled AS OF THAT LISTING'S OWN START.
///
/// - For a listing at least as recent as the read that last set the head, that is the head's
///   `unsettled_from_ms`: everything below it was visible to a read that started no later.
/// - For an older listing that lands late, the head may have been settled by a read it predates,
///   so the bound is also kept to [`SETTLE_MS`] below the listing's own newest row, which it saw.
/// - With no head (a range read of a scope whose newest end was never read), the listing's own
///   newest row stands in for one.
///
/// `newest_row_ms` is the listing's newest row; `listed_ms` the local time it started.
#[must_use]
pub fn absence_bound(head: Option<&Head>, newest_row_ms: i64, listed_ms: i64) -> i64 {
    let own = newest_row_ms.saturating_sub(SETTLE_MS);
    match head {
        Some(head) if listed_ms < head.head_listed_ms => head.unsettled_from_ms.min(own),
        Some(head) => head.unsettled_from_ms,
        None => own,
    }
}

/// What the stored intervals say about `[from_ms, to_ms)`, when they cover all of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cover {
    /// The oldest `listed_ms` among the intervals used: the honest "as of" of the whole span.
    pub listed_ms: i64,
    /// The oldest `full_ms` among them, `None` when any lacks one.
    pub full_ms: Option<i64>,
    /// Whether every interval used is complete.
    pub complete: bool,
}

/// Whether the sorted, disjoint `intervals` cover every instant of `[from_ms, to_ms)` without a
/// gap, and if so with what claim.
#[must_use]
pub fn covers(intervals: &[Interval], from_ms: i64, to_ms: i64) -> Option<Cover> {
    if from_ms >= to_ms {
        return None;
    }
    let mut cursor = from_ms;
    let mut cover: Option<Cover> = None;
    for interval in intervals {
        if interval.t1_ms <= cursor {
            continue;
        }
        if interval.t0_ms > cursor {
            return None;
        }
        cover = Some(match cover {
            None => Cover {
                listed_ms: interval.listed_ms,
                full_ms: interval.full_ms,
                complete: interval.complete,
            },
            Some(sofar) => Cover {
                listed_ms: sofar.listed_ms.min(interval.listed_ms),
                full_ms: match (sofar.full_ms, interval.full_ms) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    _ => None,
                },
                complete: sofar.complete && interval.complete,
            },
        });
        cursor = interval.t1_ms;
        if cursor >= to_ms {
            return cover;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64: a deterministic generator, so a failure names a seed that reproduces it, and no
    /// property-testing dependency is needed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: i64) -> i64 {
            i64::try_from(self.next() % u64::try_from(n).unwrap_or(1)).unwrap_or(0)
        }
    }

    fn claim(t0: i64, t1: i64, listed: i64) -> Interval {
        Interval {
            t0_ms: t0,
            t1_ms: t1,
            listed_ms: listed,
            full_ms: None,
            complete: true,
            source: Source::Read,
        }
    }

    const EXACT: Params = Params {
        merge_within_ms: 0,
        max_intervals: usize::MAX,
        old_after_ms: 0,
    };

    #[test]
    fn a_claim_inside_a_newer_one_changes_nothing_and_an_older_listing_never_lowers_a_time() {
        let stored = stamp(&[], claim(0, 100, 50), None, &EXACT);
        assert_eq!(stored, vec![claim(0, 100, 50)]);
        // Older listing over the middle: times only move forward.
        assert_eq!(stamp(&stored, claim(20, 40, 10), None, &EXACT), stored);
        // Newer listing over the middle splits it in three.
        assert_eq!(
            stamp(&stored, claim(20, 40, 70), None, &EXACT),
            vec![claim(0, 20, 50), claim(20, 40, 70), claim(40, 100, 50)]
        );
        // A gap is filled by the new claim; the stored pieces either side keep theirs.
        // Identical neighbouring claims are one claim.
        let two = vec![claim(0, 10, 5), claim(20, 30, 5)];
        assert_eq!(
            stamp(&two, claim(5, 25, 9), None, &EXACT),
            vec![claim(0, 5, 5), claim(5, 25, 9), claim(25, 30, 5)]
        );
        // An empty claim is no claim.
        assert_eq!(stamp(&two, claim(7, 7, 99), None, &EXACT), two);
    }

    #[test]
    fn neighbours_that_agree_merge_with_the_weaker_claim_and_completeness_is_never_mixed() {
        let params = Params {
            merge_within_ms: 10,
            ..Params::default()
        };
        let stored = stamp(&[], claim(0, 10, 100), None, &params);
        let merged = stamp(&stored, claim(10, 20, 105), None, &params);
        assert_eq!(merged, vec![claim(0, 20, 100)]);
        let far = stamp(&merged, claim(20, 30, 200), None, &params);
        assert_eq!(far.len(), 2, "times 100 apart do not merge");
        let incomplete = Interval {
            complete: false,
            ..claim(30, 40, 200)
        };
        let mixed = stamp(&far, incomplete, None, &params);
        assert_eq!(
            mixed.len(),
            3,
            "a complete and an incomplete piece stay apart"
        );
    }

    #[test]
    fn past_the_cap_old_neighbours_merge_and_then_the_oldest_claims_are_given_up() {
        let params = Params {
            merge_within_ms: 0,
            max_intervals: 3,
            old_after_ms: 100,
        };
        let mut stored = Vec::new();
        for i in 0..6 {
            stored = stamp(&stored, claim(i * 10, i * 10 + 10, i), Some(1000), &params);
        }
        assert_eq!(stored.len(), 3);
        assert_eq!(stored.last().map(|i| i.t1_ms), Some(60));
        assert_eq!(
            stored[0].t0_ms, 0,
            "touching old pieces merged, nothing dropped"
        );
        assert!(stored.iter().all(|i| i.listed_ms <= 5));
        // Disjoint pieces with gaps cannot merge: the oldest is dropped instead.
        let mut gappy = Vec::new();
        for i in 0..6 {
            gappy = stamp(&gappy, claim(i * 20, i * 20 + 10, 1), Some(1000), &params);
        }
        assert_eq!(gappy.len(), 3);
        assert_eq!(gappy[0].t0_ms, 60);
    }

    #[test]
    fn the_head_band_never_reopens_and_settles_by_local_time_or_a_newer_row() {
        let head = advance_head(None, 1_000_000, 50);
        assert_eq!(head.band(), Some((1_000_000 - SETTLE_MS, 1_000_000)));
        // A minute later, one row a little newer: still unsettled up to the old head, and the
        // band follows the newest row up, never more than the late bound wide below it.
        let soon = advance_head(Some(head), 1_030_000, 60_050);
        assert_eq!(soon.unsettled_from_ms, 1_030_000 - SETTLE_MS);
        assert_eq!(soon.head_ms, 1_030_000);
        // Re-read with nothing new: the band keeps the clock of the read that last moved the head.
        let quiet = advance_head(Some(soon), 1_030_000, 300_000);
        assert_eq!(quiet.head_listed_ms, 60_050);
        assert_eq!(quiet.band(), soon.band());
        // Ten minutes after that read, nothing new: the band is empty.
        let later = advance_head(Some(quiet), 1_030_000, 60_050 + SETTLE_AFTER_LOCAL_MS);
        assert_eq!(later.band(), None);
        assert!(!later.in_band(1_030_000));
        assert_eq!(later.head_listed_ms, 60_050 + SETTLE_AFTER_LOCAL_MS);
        // Once settled, every read of the newest end moves the time forward.
        let fresh = advance_head(Some(later), 1_030_000, 60_050 + SETTLE_AFTER_LOCAL_MS + 5);
        assert_eq!(fresh.head_listed_ms, 60_050 + SETTLE_AFTER_LOCAL_MS + 5);
        // A stale read changes nothing, and in particular does not reopen it.
        assert_eq!(advance_head(Some(later), 1_000_000, 10), later);
        assert_eq!(advance_head(Some(later), 1_030_000, 60_100), later);
        assert_eq!(advance_head(Some(later), 1_000_500, 60_100), later);
        // A row more than the late bound past the head settles the old band at once.
        let jump = advance_head(Some(head), 1_000_000 + SETTLE_MS + 1, 100);
        assert_eq!(jump.unsettled_from_ms, 1_000_001);
        assert_eq!(absence_bound(Some(&jump), jump.head_ms, 100), 1_000_001);
        assert_eq!(absence_bound(None, 500_000, 7), 500_000 - SETTLE_MS);
        // A listing older than the read that last set the head keeps to its own newest row.
        assert_eq!(
            absence_bound(Some(&jump), 990_000, 99),
            990_000 - SETTLE_MS,
            "it may predate what settled the head"
        );
    }

    /// The case that kept a busy channel unsettled for as long as it stayed busy: a message a
    /// minute, and a read of the newest end a minute, for ten hours. Each read moves the head by
    /// less than the late bound and starts less than ten minutes after the last, so neither of
    /// the band's own rules ever fires; the newest row still settles what is two minutes below
    /// it.
    #[test]
    fn a_busy_channels_band_stays_two_minutes_wide() {
        let mut head = None;
        for minute in 0..600 {
            let written = 1_000_000 + minute * 60_000;
            head = Some(advance_head(head, written, written + 5_000));
            let band = head.and_then(|h| h.band()).expect("a band");
            assert!(
                band.1 - band.0 <= SETTLE_MS,
                "minute {minute}: the band is {} ms wide",
                band.1 - band.0
            );
        }
    }

    /// The band against a simulated provider: messages written at random provider times, each
    /// first listed up to [`SETTLE_MS`] later; reads of the newest end at random local times,
    /// some landing out of order; and the provider's clock off the local one by up to five
    /// minutes. After every read, everything below the bound that read uses for deletions by
    /// absence was visible to it, so its absence really means a deletion.
    #[test]
    fn the_band_never_settles_a_message_the_read_could_not_yet_see() {
        for seed in 0..2_000_u64 {
            let mut rng = Rng(seed);
            let skew = rng.below(600_001) - 300_000;
            // Messages: (written, first listed), both provider time.
            let mut messages: Vec<(i64, i64)> = Vec::new();
            // Busy channels and quiet ones, read often and seldom.
            let (gap, read_gap) = match seed % 3 {
                0 => (300_000, 900_000),
                1 => (90_000, 200_000),
                _ => (20_000, 60_000),
            };
            let mut t = 10_000_000;
            for _ in 0..(5 + rng.below(60)) {
                t += rng.below(gap);
                messages.push((t, t + rng.below(SETTLE_MS + 1)));
            }
            // Reads: local start times, each answering what was listed by then.
            let mut starts: Vec<i64> = Vec::new();
            let mut local = 10_000_000 - skew - 200_000;
            for _ in 0..(3 + rng.below(40)) {
                local += rng.below(read_gap);
                starts.push(local);
            }
            // Some land out of order.
            for i in 1..starts.len() {
                if rng.below(4) == 0 {
                    starts.swap(i - 1, i);
                }
            }
            let mut head: Option<Head> = None;
            for listed_ms in starts {
                let provider_now = listed_ms + skew;
                let Some(newest) = messages
                    .iter()
                    .filter(|(_, visible)| *visible <= provider_now)
                    .map(|(written, _)| *written)
                    .max()
                else {
                    continue; // an empty listing proves nothing, and moves nothing
                };
                head = Some(advance_head(head, newest, listed_ms));
                let bound = absence_bound(head.as_ref(), newest, listed_ms);
                for (written, visible) in &messages {
                    assert!(
                        *written >= bound || *visible <= provider_now,
                        "seed {seed}: a message written at {written}, listed from {visible}, \
                         is below the bound {bound} of a read at provider time {provider_now}"
                    );
                }
            }
        }
    }

    #[test]
    fn covers_needs_every_instant_and_reports_the_weakest_claim_used() {
        let stored = vec![
            claim(0, 10, 5),
            Interval {
                full_ms: Some(3),
                complete: false,
                ..claim(10, 20, 9)
            },
            claim(25, 30, 1),
        ];
        assert_eq!(
            covers(&stored, 2, 15),
            Some(Cover {
                listed_ms: 5,
                full_ms: None,
                complete: false
            })
        );
        assert_eq!(covers(&stored, 15, 26), None, "the gap at 20..25");
        assert_eq!(covers(&stored, 25, 30).map(|c| c.listed_ms), Some(1));
        assert_eq!(covers(&stored, 5, 5), None);
        assert_eq!(covers(&stored, 29, 31), None);
    }

    /// What one instant was told by the stamps that covered it, in the order they were applied.
    #[derive(Clone, Default)]
    struct Told(Vec<Interval>);

    impl Told {
        /// The claim [`stamp`] must reproduce exactly when nothing is merged: the most recent
        /// listing (the later one on a tie) decides `listed` and `complete`; `full` is the latest.
        fn exact(&self) -> Option<(i64, bool, Option<i64>)> {
            let mut best: Option<&Interval> = None;
            for s in &self.0 {
                if best.is_none_or(|b| s.listed_ms >= b.listed_ms) {
                    best = Some(s);
                }
            }
            let full = self.0.iter().filter_map(|s| s.full_ms).max();
            best.map(|b| (b.listed_ms, b.complete, full))
        }

        /// Whether a stored claim is SOUND: some stamp that covered this instant listed it at
        /// least that recently (completely, if the claim says complete), and some stamp's
        /// `full_ms` is at least the claimed one.
        fn supports(&self, stored: &Interval) -> bool {
            let listed = self
                .0
                .iter()
                .any(|s| s.listed_ms >= stored.listed_ms && (!stored.complete || s.complete));
            let full = stored
                .full_ms
                .is_none_or(|f| self.0.iter().any(|s| s.full_ms.is_some_and(|g| g >= f)));
            listed && full
        }
    }

    fn check_shape(out: &[Interval], seed: u64) {
        for pair in out.windows(2) {
            assert!(
                pair[0].t1_ms <= pair[1].t0_ms,
                "seed {seed}: overlap {pair:?}"
            );
        }
        for interval in out {
            assert!(
                interval.t0_ms < interval.t1_ms,
                "seed {seed}: empty {interval:?}"
            );
            assert!(
                interval.full_ms.is_none_or(|f| f <= interval.listed_ms),
                "seed {seed}: full after listed {interval:?}"
            );
        }
    }

    fn at(out: &[Interval], t: i64) -> Option<&Interval> {
        out.iter().find(|i| i.t0_ms <= t && t < i.t1_ms)
    }

    /// The property test the plan asks for: [`stamp`] against a brute-force bitmap of what every
    /// instant was told, over thousands of random stamp sequences in a small time domain.
    #[test]
    fn stamping_agrees_with_a_brute_force_bitmap() {
        const DOMAIN: i64 = 120;
        for seed in 0..3_000_u64 {
            let mut rng = Rng(seed);
            let params = match seed % 3 {
                0 => EXACT,
                1 => Params {
                    merge_within_ms: 15,
                    max_intervals: usize::MAX,
                    old_after_ms: 30,
                },
                _ => Params {
                    merge_within_ms: rng.below(20),
                    max_intervals: 4,
                    old_after_ms: 30,
                },
            };
            let capped = params.max_intervals != usize::MAX;
            let mut bitmap: Vec<Told> = vec![Told::default(); usize::try_from(DOMAIN).unwrap_or(0)];
            let mut out: Vec<Interval> = Vec::new();
            for _ in 0..(1 + rng.below(12)) {
                let t0 = rng.below(DOMAIN);
                let t1 = t0 + 1 + rng.below(DOMAIN - t0);
                let listed = rng.below(1_000);
                let new = Interval {
                    t0_ms: t0,
                    t1_ms: t1,
                    listed_ms: listed,
                    full_ms: (rng.below(2) == 0).then(|| listed - rng.below(listed + 1)),
                    complete: rng.below(3) != 0,
                    source: Source::Read,
                };
                out = stamp(&out, new, Some(DOMAIN), &params);
                for t in t0..t1 {
                    bitmap[usize::try_from(t).unwrap_or(0)].0.push(new);
                }
                check_shape(&out, seed);
                // `covers`, which serving will rest on, against the same intervals instant by
                // instant over a random span: whole or nothing, and the weakest claim used.
                let from = rng.below(DOMAIN);
                let to = from + 1 + rng.below(DOMAIN - from);
                let expected = (from..to)
                    .try_fold(None::<Cover>, |sofar, t| {
                        let stored = at(&out, t)?;
                        Some(Some(match sofar {
                            None => Cover {
                                listed_ms: stored.listed_ms,
                                full_ms: stored.full_ms,
                                complete: stored.complete,
                            },
                            Some(c) => Cover {
                                listed_ms: c.listed_ms.min(stored.listed_ms),
                                full_ms: match (c.full_ms, stored.full_ms) {
                                    (Some(a), Some(b)) => Some(a.min(b)),
                                    _ => None,
                                },
                                complete: c.complete && stored.complete,
                            },
                        }))
                    })
                    .flatten();
                assert_eq!(
                    covers(&out, from, to),
                    expected,
                    "seed {seed}: covers({from}, {to}) over {out:?}"
                );
                for t in 0..DOMAIN {
                    let told = &bitmap[usize::try_from(t).unwrap_or(0)];
                    match at(&out, t) {
                        Some(stored) => {
                            assert!(!told.0.is_empty(), "seed {seed}: {t} claimed, never listed");
                            assert!(told.supports(stored), "seed {seed}: {t} over-claims");
                            if params.merge_within_ms == 0 && !capped {
                                assert_eq!(
                                    Some((stored.listed_ms, stored.complete, stored.full_ms)),
                                    told.exact(),
                                    "seed {seed}: {t} is not exactly what it was told"
                                );
                            }
                        }
                        None => assert!(
                            capped || told.0.is_empty(),
                            "seed {seed}: {t} was listed but lost its coverage"
                        ),
                    }
                }
            }
            assert!(out.len() <= params.max_intervals, "seed {seed}");
        }
    }

    #[test]
    fn source_names_round_trip() {
        for source in [
            Source::Read,
            Source::Poll,
            Source::Walk,
            Source::Range,
            Source::Refresh,
        ] {
            assert_eq!(Source::parse(source.as_str()), Some(source));
        }
        assert_eq!(Source::parse("guess"), None);
    }
}
