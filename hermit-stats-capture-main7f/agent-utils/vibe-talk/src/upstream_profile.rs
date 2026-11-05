//! How long upstream requests take, and how big the messages that come back are, in numbers only.
//! `#208 persistent-cache`, milestone `upstream-profile`.
//!
//! # Why this exists
//!
//! The message cache planned under #208 is justified by what upstream reads cost, and bounded by
//! how much a message weighs. Neither had been measured inside this server: the only timings were
//! the far end's own log, which knows nothing about which of this server's operations a request
//! belonged to, and the only size estimate was a guess. This module measures both, in memory,
//! from the moment the process starts.
//!
//! # Three series per provider
//!
//! * **Operations.** Every [`ChatClient`] method that can reach the provider, timed by
//!   [`ProfiledClient`], which [`crate::providers::ChatRouter`] wraps around each provider's client.
//!   Every caller goes through it, including the live poller, which holds a provider's client
//!   directly rather than the router. Recorded per (operation, outcome): a latency histogram and
//!   the rows that came back.
//! * **Shim calls.** Each HTTP request [`crate::slack::HttpSlackClient`] sends, per Web API
//!   method and outcome. One operation can be many calls (a Slack flat read is about eleven), and
//!   a call answered with a rate limit is one call, as the far end's log counts it. Budgets and
//!   comparisons with that log are made in calls, not operations.
//! * **Content size.** The UTF-8 bytes of each message's `content`, for every message a read
//!   returned. A message read twice counts twice: this describes what crosses the wire, which is
//!   also what a cache would hold most of.
//!
//! # Histograms
//!
//! Latency is bucketed in powers of two from 1 ms to 128 s (2^17 ms), with one bucket below and
//! one above; sizes in powers of two from 1 byte to 1 MiB. Each power of two is split into
//! [`SUBDIVISIONS`] equal steps, so a percentile read off the histogram is within about 6 % of the
//! true value. A plain power-of-two bucket is 100 % wide, which could not support a comparison
//! with the far end's log to within 20 %. The report still lists counts per power of two
//! (`octaves_ms`), which is the shape a reader wants.
//!
//! # Where it shows
//!
//! [`UpstreamProfile::report`] is the `upstream` section of `GET /api/v1/diagnostics`: totals
//! since the process started. [`log_every_hour`] writes one `info` line per provider per hour
//! with the same figures for that hour alone, so an hour of this log lines up with an hour of
//! the far end's.
//!
//! # What it never holds
//!
//! No channel id, message id, author, or text. Only the provider key and display name, which
//! diagnostics already shows; the operation or method name, which is a constant in this crate; an
//! outcome word; and numbers. Memory only: a restart starts again from zero.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;

use crate::chat::{
    ChatClient, ChatError, ChatIdentity, PostLimit, RecentRead, RegisteredChannel, SourceClaim,
};
use crate::model::{ChannelId, Message, MessageId};

/// Equal steps each power of two is divided into.
pub const SUBDIVISIONS: u64 = 8;
/// The smallest latency bucket boundary, in microseconds: 1 ms.
const LATENCY_FLOOR_MICROS: u64 = 1_000;
/// Powers of two above the latency floor: 1 ms · 2^17 ≈ 131 s, "128 s" in binary terms.
const LATENCY_OCTAVES: u32 = 17;
/// Powers of two above one byte: 2^20 bytes = 1 MiB, well past Slack's 40,000-character ceiling.
const SIZE_OCTAVES: u32 = 20;
/// How often [`log_every_hour`] writes.
pub const LOG_INTERVAL: Duration = Duration::from_secs(3600);

/// How one upstream operation or call ended, in the words the report and the log use.
///
/// A provider label ([`ChatError::Provider`]) is looked through, so it does not change the class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The provider answered and the answer was understood.
    Ok,
    /// The request never completed: a connection failure or a timeout.
    Transport,
    /// A rate limit: one 429 for a shim call, or an operation whose wait did not clear in budget.
    RateLimited,
    /// HTTP 401: the credential was refused.
    Unauthorized,
    /// HTTP 403: the credential may not do this.
    Forbidden,
    /// HTTP 404: the named thing does not exist for this credential.
    NotFound,
    /// Any other 4xx.
    ClientError,
    /// A 5xx.
    ServerError,
    /// A status outside 4xx and 5xx that was still not a success.
    UnexpectedStatus,
    /// An answer this server could not understand.
    Shape,
    /// This server refused before contacting the provider.
    Refused,
    /// A forward cursor the provider can no longer answer for.
    CursorExpired,
    /// The caller stopped waiting before an answer arrived, so the time is how long it waited.
    Abandoned,
}

impl Outcome {
    /// The class of `error`.
    #[must_use]
    pub fn of(error: &ChatError) -> Self {
        match error {
            ChatError::Transport(_) => Self::Transport,
            ChatError::RateLimited(_) => Self::RateLimited,
            ChatError::Status { status, .. } => match status {
                401 => Self::Unauthorized,
                403 => Self::Forbidden,
                404 => Self::NotFound,
                429 => Self::RateLimited,
                400..=499 => Self::ClientError,
                500..=599 => Self::ServerError,
                _ => Self::UnexpectedStatus,
            },
            ChatError::Shape(_) => Self::Shape,
            ChatError::Refused(_) => Self::Refused,
            ChatError::CursorExpired(_) => Self::CursorExpired,
            ChatError::Provider { source, .. } => Self::of(source),
        }
    }

    /// The word, as it appears in the report and the log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Transport => "transport",
            Self::RateLimited => "rate_limited",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::UnexpectedStatus => "unexpected_status",
            Self::Shape => "shape",
            Self::Refused => "refused",
            Self::CursorExpired => "cursor_expired",
            Self::Abandoned => "abandoned",
        }
    }
}

/// Which series a measurement belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Series {
    /// A [`ChatClient`] operation, timed by [`ProfiledClient`].
    Operation,
    /// One HTTP request to a provider's Web API, timed by the provider's client.
    ShimCall,
}

/// Counts of values in power-of-two buckets, each split into [`SUBDIVISIONS`] steps.
///
/// Bucket 0 holds values below `floor`. Bucket `1 + k·S + s` holds
/// `[floor·2^k·(1 + s/S), floor·2^k·(1 + (s+1)/S))`. The last bucket holds `floor·2^octaves` and up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram {
    floor: u64,
    octaves: u32,
    counts: Vec<u64>,
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
}

impl Histogram {
    /// An empty histogram from `floor` over `octaves` powers of two.
    #[must_use]
    pub fn new(floor: u64, octaves: u32) -> Self {
        let steps = usize::try_from(u64::from(octaves) * SUBDIVISIONS).unwrap_or(usize::MAX - 2);
        Self {
            floor: floor.max(1),
            octaves,
            counts: vec![0; steps + 2],
            count: 0,
            sum: 0,
            min: 0,
            max: 0,
        }
    }

    /// An empty latency histogram, in microseconds, from 1 ms to 128 s.
    #[must_use]
    pub fn latency() -> Self {
        Self::new(LATENCY_FLOOR_MICROS, LATENCY_OCTAVES)
    }

    /// An empty size histogram, in bytes, from 1 byte to 1 MiB.
    #[must_use]
    pub fn size() -> Self {
        Self::new(1, SIZE_OCTAVES)
    }

    /// The bucket `value` falls in.
    #[must_use]
    pub fn index(&self, value: u64) -> usize {
        if value < self.floor {
            return 0;
        }
        let octave = (value / self.floor).ilog2();
        if octave >= self.octaves {
            return self.counts.len() - 1;
        }
        let base = self.floor << octave;
        let step = u128::from(value - base) * u128::from(SUBDIVISIONS) / u128::from(base);
        1 + usize::try_from(u64::from(octave) * SUBDIVISIONS).unwrap_or(0)
            + usize::try_from(step).unwrap_or(0)
    }

    /// Count one value.
    pub fn record(&mut self, value: u64) {
        let index = self.index(value);
        self.counts[index] += 1;
        self.min = if self.count == 0 {
            value
        } else {
            self.min.min(value)
        };
        self.max = self.max.max(value);
        self.count += 1;
        self.sum = self.sum.saturating_add(value);
    }

    /// How many values were counted.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The sum of every value counted, saturating.
    #[must_use]
    pub const fn sum(&self) -> u64 {
        self.sum
    }

    /// The largest value counted; 0 when empty.
    #[must_use]
    pub const fn max(&self) -> u64 {
        self.max
    }

    /// The mean of every value counted; 0 when empty.
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum as f64 / self.count as f64
        }
    }

    /// The middle of bucket `index`, as the value standing for everything in it.
    fn representative(&self, index: usize) -> f64 {
        let last = self.counts.len() - 1;
        if index == 0 {
            return self.floor as f64 / 2.0;
        }
        if index >= last {
            return (self.floor << self.octaves) as f64;
        }
        let steps = usize::try_from(SUBDIVISIONS).unwrap_or(1);
        let octave = (index - 1) / steps;
        let step = (index - 1) % steps;
        let base = (self.floor << octave) as f64;
        let width = base / SUBDIVISIONS as f64;
        base + width * (step as f64 + 0.5)
    }

    /// The value below which a fraction `q` of the counted values fall, read off the buckets and
    /// kept within the smallest and largest value actually seen. 0 when empty.
    #[must_use]
    pub fn quantile(&self, q: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let rank = ((q.clamp(0.0, 1.0) * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (index, n) in self.counts.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return self
                    .representative(index)
                    .clamp(self.min as f64, self.max as f64);
            }
        }
        self.max as f64
    }

    /// `[lower bound, count]` for every power of two that holds anything, smallest first. The
    /// bucket below the floor reads as lower bound 0.
    #[must_use]
    pub fn octaves(&self) -> Vec<[u64; 2]> {
        let steps = usize::try_from(SUBDIVISIONS).unwrap_or(1);
        let mut out = Vec::new();
        if self.counts[0] > 0 {
            out.push([0, self.counts[0]]);
        }
        for octave in 0..self.octaves {
            let start = 1 + usize::try_from(octave).unwrap_or(0) * steps;
            let n: u64 = self.counts[start..start + steps].iter().sum();
            if n > 0 {
                out.push([self.floor << octave, n]);
            }
        }
        let last = self.counts[self.counts.len() - 1];
        if last > 0 {
            out.push([self.floor << self.octaves, last]);
        }
        out
    }
}

/// One (name, outcome) row: everything since start, and the current hour.
#[derive(Clone, Debug)]
struct Tally {
    total: Histogram,
    hour: Histogram,
    rows_total: u64,
    rows_hour: u64,
}

impl Tally {
    fn new() -> Self {
        Self {
            total: Histogram::latency(),
            hour: Histogram::latency(),
            rows_total: 0,
            rows_hour: 0,
        }
    }
}

/// What one provider's measurements are kept in.
#[derive(Debug)]
struct Tallies {
    operations: BTreeMap<(&'static str, Outcome), Tally>,
    shim_calls: BTreeMap<(&'static str, Outcome), Tally>,
    content_total: Histogram,
    content_hour: Histogram,
    hour_started: Instant,
}

/// One provider's measurements, shared by its [`ProfiledClient`] and, for Slack, its HTTP client.
pub struct ProviderProfile {
    key: String,
    name: String,
    tallies: Mutex<Tallies>,
}

impl std::fmt::Debug for ProviderProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderProfile")
            .field("key", &self.key)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ProviderProfile {
    fn new(key: &str, name: &str) -> Self {
        Self {
            key: key.to_owned(),
            name: name.to_owned(),
            tallies: Mutex::new(Tallies {
                operations: BTreeMap::new(),
                shim_calls: BTreeMap::new(),
                content_total: Histogram::size(),
                content_hour: Histogram::size(),
                hour_started: Instant::now(),
            }),
        }
    }

    /// The provider key this profile is filed under.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tallies> {
        self.tallies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start timing one operation or call. Finish it with [`Timer::finish`]; a timer dropped
    /// unfinished records [`Outcome::Abandoned`] with the time waited.
    pub fn start(&self, series: Series, name: &'static str) -> Timer<'_> {
        Timer {
            profile: self,
            series,
            name,
            started: Instant::now(),
            finished: false,
        }
    }

    /// Record one finished operation or call.
    pub fn record(
        &self,
        series: Series,
        name: &'static str,
        outcome: Outcome,
        elapsed: Duration,
        rows: usize,
    ) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let rows = u64::try_from(rows).unwrap_or(u64::MAX);
        let mut tallies = self.lock();
        let map = match series {
            Series::Operation => &mut tallies.operations,
            Series::ShimCall => &mut tallies.shim_calls,
        };
        let tally = map.entry((name, outcome)).or_insert_with(Tally::new);
        tally.total.record(micros);
        tally.hour.record(micros);
        tally.rows_total = tally.rows_total.saturating_add(rows);
        tally.rows_hour = tally.rows_hour.saturating_add(rows);
    }

    /// Count the content bytes of each of `messages`.
    pub fn record_content(&self, messages: &[Message]) {
        if messages.is_empty() {
            return;
        }
        let mut tallies = self.lock();
        for message in messages {
            let bytes = u64::try_from(message.content.len()).unwrap_or(u64::MAX);
            tallies.content_total.record(bytes);
            tallies.content_hour.record(bytes);
        }
    }

    /// Everything since the process started.
    #[must_use]
    pub fn report(&self) -> ProviderReport {
        let tallies = self.lock();
        ProviderReport {
            provider: self.key.clone(),
            name: self.name.clone(),
            operations: tallies
                .operations
                .iter()
                .map(|(&(name, outcome), tally)| {
                    TimingReport::of(name, outcome, &tally.total, Some(tally.rows_total))
                })
                .collect(),
            shim_calls: tallies
                .shim_calls
                .iter()
                .map(|(&(name, outcome), tally)| {
                    TimingReport::of(name, outcome, &tally.total, None)
                })
                .collect(),
            content_bytes: SizeReport::of(&tallies.content_total),
        }
    }

    /// The figures for the hour now ending, and start the next one.
    #[must_use]
    pub fn take_hour(&self) -> HourReport {
        let mut tallies = self.lock();
        let seconds = tallies.hour_started.elapsed().as_secs();
        tallies.hour_started = Instant::now();
        let take = |map: &mut BTreeMap<(&'static str, Outcome), Tally>, rows: bool| {
            let mut out = Vec::new();
            for (&(name, outcome), tally) in map.iter_mut() {
                if tally.hour.count() > 0 {
                    out.push(TimingReport::of(
                        name,
                        outcome,
                        &tally.hour,
                        rows.then_some(tally.rows_hour),
                    ));
                }
                tally.hour = Histogram::latency();
                tally.rows_hour = 0;
            }
            out
        };
        let operations = take(&mut tallies.operations, true);
        let shim_calls = take(&mut tallies.shim_calls, false);
        let content_bytes = SizeReport::of(&tallies.content_hour);
        tallies.content_hour = Histogram::size();
        HourReport {
            provider: self.key.clone(),
            seconds,
            operations,
            shim_calls,
            content_bytes,
        }
    }
}

/// One operation or call being timed. See [`ProviderProfile::start`].
#[must_use = "a timer records when it is finished or dropped"]
pub struct Timer<'p> {
    profile: &'p ProviderProfile,
    series: Series,
    name: &'static str,
    started: Instant,
    finished: bool,
}

impl Timer<'_> {
    /// Record how this one ended, and return how long it took.
    pub fn finish(mut self, outcome: Outcome, rows: usize) -> Duration {
        self.finished = true;
        let elapsed = self.started.elapsed();
        self.profile
            .record(self.series, self.name, outcome, elapsed, rows);
        elapsed
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.profile.record(
                self.series,
                self.name,
                Outcome::Abandoned,
                self.started.elapsed(),
                0,
            );
        }
    }
}

/// Every provider's measurements in one process.
pub struct UpstreamProfile {
    started: Instant,
    started_at: jiff::Timestamp,
    providers: Mutex<Vec<Arc<ProviderProfile>>>,
}

impl std::fmt::Debug for UpstreamProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamProfile")
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

impl Default for UpstreamProfile {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamProfile {
    /// An empty profile, counting from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            started_at: jiff::Timestamp::now(),
            providers: Mutex::new(Vec::new()),
        }
    }

    /// The profile filed under provider `key`, created with display `name` the first time.
    #[must_use]
    pub fn provider(&self, key: &str, name: &str) -> Arc<ProviderProfile> {
        let mut providers = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = providers.iter().find(|profile| profile.key == key) {
            return Arc::clone(found);
        }
        let created = Arc::new(ProviderProfile::new(key, name));
        providers.push(Arc::clone(&created));
        created
    }

    fn all(&self) -> Vec<Arc<ProviderProfile>> {
        self.providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every provider's figures since the process started, in configuration order.
    #[must_use]
    pub fn report(&self) -> UpstreamReport {
        UpstreamReport {
            since: self.started_at.to_string(),
            seconds: self.started.elapsed().as_secs(),
            providers: self.all().iter().map(|profile| profile.report()).collect(),
        }
    }

    /// Each provider's figures for the hour now ending, starting the next hour.
    #[must_use]
    pub fn take_hour(&self) -> Vec<HourReport> {
        self.all()
            .iter()
            .map(|profile| profile.take_hour())
            .collect()
    }

    /// Write one `info` line per provider for the hour now ending. Names and numbers only.
    pub fn log_hour(&self) {
        for hour in self.take_hour() {
            hour.log();
        }
    }
}

/// Write [`UpstreamProfile::log_hour`] every [`LOG_INTERVAL`], first one interval from now.
pub async fn log_every_hour(profile: Arc<UpstreamProfile>) {
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + LOG_INTERVAL, LOG_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        profile.log_hour();
    }
}

/// The `upstream` section of `GET /api/v1/diagnostics`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpstreamReport {
    /// When counting started: this process's start, RFC 3339.
    pub since: String,
    /// Seconds counted so far.
    pub seconds: u64,
    /// One entry per configured provider.
    pub providers: Vec<ProviderReport>,
}

/// One provider's figures.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderReport {
    /// The provider key, as in the configuration.
    pub provider: String,
    /// The provider's display name.
    pub name: String,
    /// One row per (operation, outcome) seen.
    pub operations: Vec<TimingReport>,
    /// One row per (Web API method, outcome) seen. Empty for a provider whose client does not
    /// time its own calls.
    pub shim_calls: Vec<TimingReport>,
    /// Bytes of `content` per message read.
    pub content_bytes: SizeReport,
}

/// Latency figures for one (name, outcome).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimingReport {
    /// The operation (a [`ChatClient`] method) or the Web API method.
    pub name: &'static str,
    /// How they ended.
    pub outcome: Outcome,
    /// How many.
    pub count: u64,
    /// Rows the operations returned, in total. Absent for shim calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// Mean latency, milliseconds.
    pub mean_ms: f64,
    /// Median latency, milliseconds, read off the histogram.
    pub p50_ms: f64,
    /// 90th percentile latency, milliseconds, read off the histogram.
    pub p90_ms: f64,
    /// 99th percentile latency, milliseconds, read off the histogram.
    pub p99_ms: f64,
    /// The slowest, milliseconds.
    pub max_ms: f64,
    /// `[from_ms, count]` per power of two that holds anything: `count` took at least `from_ms`
    /// and less than twice that. `from_ms = 0` is under 1 ms.
    pub octaves_ms: Vec<[u64; 2]>,
}

/// Microseconds as milliseconds, to a tenth.
fn millis(micros: f64) -> f64 {
    (micros / 100.0).round() / 10.0
}

impl TimingReport {
    fn of(name: &'static str, outcome: Outcome, histogram: &Histogram, rows: Option<u64>) -> Self {
        Self {
            name,
            outcome,
            count: histogram.count(),
            rows,
            mean_ms: millis(histogram.mean()),
            p50_ms: millis(histogram.quantile(0.5)),
            p90_ms: millis(histogram.quantile(0.9)),
            p99_ms: millis(histogram.quantile(0.99)),
            max_ms: millis(histogram.max() as f64),
            octaves_ms: histogram
                .octaves()
                .into_iter()
                .map(|[from, count]| [from / LATENCY_FLOOR_MICROS, count])
                .collect(),
        }
    }
}

/// Size figures, in bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SizeReport {
    /// Messages counted.
    pub messages: u64,
    /// Their content bytes, in total.
    pub total: u64,
    /// Mean bytes per message, rounded.
    pub mean: u64,
    /// Median bytes, read off the histogram and rounded.
    pub p50: u64,
    /// 90th percentile bytes, read off the histogram and rounded.
    pub p90: u64,
    /// 99th percentile bytes, read off the histogram and rounded.
    pub p99: u64,
    /// The largest.
    pub max: u64,
    /// `[from, count]` per power of two that holds anything: `count` messages had at least
    /// `from` bytes and fewer than twice that. `from = 0` is an empty message.
    pub octaves: Vec<[u64; 2]>,
}

impl SizeReport {
    fn of(histogram: &Histogram) -> Self {
        let round = |value: f64| value.round() as u64;
        Self {
            messages: histogram.count(),
            total: histogram.sum(),
            mean: round(histogram.mean()),
            p50: round(histogram.quantile(0.5)),
            p90: round(histogram.quantile(0.9)),
            p99: round(histogram.quantile(0.99)),
            max: histogram.max(),
            octaves: histogram.octaves(),
        }
    }
}

/// One provider's figures for one hour, as the hourly line writes them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HourReport {
    /// The provider key.
    pub provider: String,
    /// How long the hour actually was, in seconds.
    pub seconds: u64,
    /// One row per (operation, outcome) seen this hour.
    pub operations: Vec<TimingReport>,
    /// One row per (Web API method, outcome) seen this hour.
    pub shim_calls: Vec<TimingReport>,
    /// Bytes of `content` per message read this hour.
    pub content_bytes: SizeReport,
}

/// `fetch_recent ok n=3 p50_ms=12 p90_ms=40; …`: names, outcome words and numbers.
fn timings_text(rows: &[TimingReport]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{} {} n={} p50_ms={} p90_ms={} max_ms={}",
                row.name,
                row.outcome.as_str(),
                row.count,
                row.p50_ms,
                row.p90_ms,
                row.max_ms
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl HourReport {
    /// Operations this hour, every outcome.
    #[must_use]
    pub fn operation_count(&self) -> u64 {
        self.operations.iter().map(|row| row.count).sum()
    }

    /// Shim calls this hour, every outcome.
    #[must_use]
    pub fn shim_call_count(&self) -> u64 {
        self.shim_calls.iter().map(|row| row.count).sum()
    }

    fn failed(rows: &[TimingReport]) -> u64 {
        rows.iter()
            .filter(|row| row.outcome != Outcome::Ok)
            .map(|row| row.count)
            .sum()
    }

    /// Write this hour as one `info` line.
    pub fn log(&self) {
        let content = &self.content_bytes;
        tracing::info!(
            provider = self.provider.as_str(),
            seconds = self.seconds,
            operations = self.operation_count(),
            operations_failed = Self::failed(&self.operations),
            operation_timings = timings_text(&self.operations).as_str(),
            shim_calls = self.shim_call_count(),
            shim_calls_failed = Self::failed(&self.shim_calls),
            shim_call_timings = timings_text(&self.shim_calls).as_str(),
            content_messages = content.messages,
            content_bytes_p50 = content.p50,
            content_bytes_p90 = content.p90,
            content_bytes_max = content.max,
            "upstream timings for the last hour"
        );
    }
}

/// What a result contributes to the figures: rows returned, and messages whose size is counted.
trait Rows {
    fn rows(&self) -> usize {
        0
    }
    fn messages(&self) -> &[Message] {
        &[]
    }
}

impl Rows for Vec<Message> {
    fn rows(&self) -> usize {
        self.len()
    }
    fn messages(&self) -> &[Message] {
        self
    }
}

impl Rows for Message {
    fn rows(&self) -> usize {
        1
    }
    fn messages(&self) -> &[Message] {
        std::slice::from_ref(self)
    }
}

impl Rows for crate::threads::TimelinePage {
    fn rows(&self) -> usize {
        self.messages.len() + self.threads.len()
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
}

impl Rows for crate::timeline_forward::GenericDelta {
    fn rows(&self) -> usize {
        self.page.rows()
    }
    fn messages(&self) -> &[Message] {
        self.page.messages()
    }
}

impl Rows for RecentRead {
    fn rows(&self) -> usize {
        self.messages.len()
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
}

impl Rows for crate::directory::DirectoryPage {
    fn rows(&self) -> usize {
        self.entries.len()
    }
}

impl Rows for () {}
impl Rows for ChatIdentity {}
impl Rows for RegisteredChannel {}
impl Rows for crate::chat::UpstreamClearOutcome {}

/// Whether an operation's messages are counted for size: reads only, not our own posts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Read,
    Other,
}

/// A provider's client, timed. Every [`ChatClient`] method is forwarded to the wrapped client's
/// own, so an override there is always the one that runs.
///
/// **Every method is written out**, including those with default bodies. A default left to the
/// trait would run the trait's body on this wrapper, calling the wrapper's other methods, and
/// silently bypass whatever the provider overrides. `tests/upstream_profile.rs` parses the trait
/// in `src/chat.rs` and fails when a method is missing here.
///
/// Only methods that may reach the provider are timed; capability questions answer from memory.
pub struct ProfiledClient {
    inner: Arc<dyn ChatClient>,
    profile: Arc<ProviderProfile>,
}

impl ProfiledClient {
    /// Time `inner` into `profile`.
    #[must_use]
    pub fn new(inner: Arc<dyn ChatClient>, profile: Arc<ProviderProfile>) -> Self {
        Self { inner, profile }
    }

    /// The profile this client records into.
    #[must_use]
    pub fn profile(&self) -> &Arc<ProviderProfile> {
        &self.profile
    }

    async fn timed<T, F>(
        &self,
        operation: &'static str,
        direction: Direction,
        work: F,
    ) -> Result<T, ChatError>
    where
        T: Rows + Send,
        F: std::future::Future<Output = Result<T, ChatError>> + Send,
    {
        let timer = self.profile.start(Series::Operation, operation);
        let result = work.await;
        match &result {
            Ok(value) => {
                timer.finish(Outcome::Ok, value.rows());
                if direction == Direction::Read {
                    self.profile.record_content(value.messages());
                }
            }
            Err(error) => {
                timer.finish(Outcome::of(error), 0);
            }
        }
        result
    }
}

#[async_trait]
impl ChatClient for ProfiledClient {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn claims_source(&self, source: &str) -> SourceClaim {
        self.inner.claims_source(source)
    }

    fn self_author_id(&self) -> Option<String> {
        self.inner.self_author_id()
    }

    fn owner_author_id(&self) -> Option<String> {
        self.inner.owner_author_id()
    }

    fn supports_threading(&self) -> bool {
        self.inner.supports_threading()
    }

    fn replies_in_thread(&self) -> bool {
        self.inner.replies_in_thread()
    }

    async fn fetch_timeline(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
    ) -> Result<crate::threads::TimelinePage, ChatError> {
        self.timed(
            "fetch_timeline",
            Direction::Read,
            self.inner.fetch_timeline(channel, request),
        )
        .await
    }

    async fn fetch_timeline_since(
        &self,
        channel: &ChannelId,
        request: &crate::threads::TimelineRequest,
        since: &crate::timeline_forward::GenericPosition,
    ) -> Result<crate::timeline_forward::GenericDelta, ChatError> {
        self.timed(
            "fetch_timeline_since",
            Direction::Read,
            self.inner.fetch_timeline_since(channel, request, since),
        )
        .await
    }

    async fn post_in_thread(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.timed(
            "post_in_thread",
            Direction::Other,
            self.inner
                .post_in_thread(channel, thread_id, content, reply_to),
        )
        .await
    }

    async fn fetch_thread_message(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        message_id: &MessageId,
    ) -> Result<Message, ChatError> {
        self.timed(
            "fetch_thread_message",
            Direction::Read,
            self.inner
                .fetch_thread_message(channel, thread_id, message_id),
        )
        .await
    }

    fn supports_channel_registration(&self) -> bool {
        self.inner.supports_channel_registration()
    }

    async fn register_channel(
        &self,
        source: &str,
        label: &str,
    ) -> Result<RegisteredChannel, ChatError> {
        self.timed(
            "register_channel",
            Direction::Other,
            self.inner.register_channel(source, label),
        )
        .await
    }

    async fn unregister_channel(&self, channel: &ChannelId) -> Result<(), ChatError> {
        self.timed(
            "unregister_channel",
            Direction::Other,
            self.inner.unregister_channel(channel),
        )
        .await
    }

    fn supports_channel_discovery(&self) -> bool {
        self.inner.supports_channel_discovery()
    }

    async fn discover_channels(
        &self,
        request: &crate::directory::DirectoryRequest,
    ) -> Result<crate::directory::DirectoryPage, ChatError> {
        self.timed(
            "discover_channels",
            Direction::Other,
            self.inner.discover_channels(request),
        )
        .await
    }

    fn supports_upstream_read_mark(&self) -> bool {
        self.inner.supports_upstream_read_mark()
    }

    async fn identity(&self) -> Result<ChatIdentity, ChatError> {
        self.timed("identity", Direction::Other, self.inner.identity())
            .await
    }

    async fn fetch_page(
        &self,
        channel: &ChannelId,
        limit: u16,
        before: Option<&MessageId>,
        after: Option<&MessageId>,
    ) -> Result<Vec<Message>, ChatError> {
        self.timed(
            "fetch_page",
            Direction::Read,
            self.inner.fetch_page(channel, limit, before, after),
        )
        .await
    }

    async fn probe_channel(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.timed(
            "probe_channel",
            Direction::Read,
            self.inner.probe_channel(channel, limit),
        )
        .await
    }

    async fn fetch_recent(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<Vec<Message>, ChatError> {
        self.timed(
            "fetch_recent",
            Direction::Read,
            self.inner.fetch_recent(channel, limit),
        )
        .await
    }

    async fn fetch_recent_dated(
        &self,
        channel: &ChannelId,
        limit: u16,
    ) -> Result<RecentRead, ChatError> {
        self.timed(
            "fetch_recent_dated",
            Direction::Read,
            self.inner.fetch_recent_dated(channel, limit),
        )
        .await
    }

    async fn post_message(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
    ) -> Result<Message, ChatError> {
        self.timed(
            "post_message",
            Direction::Other,
            self.inner.post_message(channel, content, reply_to),
        )
        .await
    }

    fn supports_idempotent_posts(&self) -> bool {
        self.inner.supports_idempotent_posts()
    }

    async fn post_message_keyed(
        &self,
        channel: &ChannelId,
        content: &str,
        reply_to: Option<&MessageId>,
        key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.timed(
            "post_message_keyed",
            Direction::Other,
            self.inner
                .post_message_keyed(channel, content, reply_to, key),
        )
        .await
    }

    async fn post_in_thread_keyed(
        &self,
        channel: &ChannelId,
        thread_id: &str,
        content: &str,
        reply_to: Option<&MessageId>,
        key: Option<&str>,
    ) -> Result<Message, ChatError> {
        self.timed(
            "post_in_thread_keyed",
            Direction::Other,
            self.inner
                .post_in_thread_keyed(channel, thread_id, content, reply_to, key),
        )
        .await
    }

    fn post_limit(&self, channel: &ChannelId) -> PostLimit {
        self.inner.post_limit(channel)
    }

    fn continuation_thread(&self, channel: &ChannelId, first: &Message) -> Option<String> {
        self.inner.continuation_thread(channel, first)
    }

    async fn mark_read_upstream(
        &self,
        channel: &ChannelId,
        through: &MessageId,
    ) -> Result<(), ChatError> {
        self.timed(
            "mark_read_upstream",
            Direction::Other,
            self.inner.mark_read_upstream(channel, through),
        )
        .await
    }

    fn supports_upstream_clear(&self) -> bool {
        self.inner.supports_upstream_clear()
    }

    async fn clear_upstream_through(
        &self,
        channel: &ChannelId,
        through: &MessageId,
        dry_run: bool,
    ) -> Result<crate::chat::UpstreamClearOutcome, ChatError> {
        self.timed(
            "clear_upstream_through",
            Direction::Other,
            self.inner.clear_upstream_through(channel, through, dry_run),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::RateLimitExhausted;

    #[test]
    fn latency_buckets_are_powers_of_two_from_one_millisecond_split_in_eighths() {
        let h = Histogram::latency();
        assert_eq!(h.index(0), 0);
        assert_eq!(h.index(999), 0, "under 1 ms is the first bucket");
        assert_eq!(h.index(1_000), 1, "1 ms opens the first power of two");
        assert_eq!(h.index(1_124), 1);
        assert_eq!(
            h.index(1_125),
            2,
            "an eighth of the way to 2 ms is the next step"
        );
        assert_eq!(h.index(1_999), 8);
        assert_eq!(h.index(2_000), 9, "2 ms opens the second power of two");
        assert_eq!(
            h.index(131_071_999),
            17 * 8,
            "just under 2^17 ms is the last step"
        );
        let overflow = 17 * 8 + 1;
        assert_eq!(
            h.index(131_072_000),
            overflow,
            "2^17 ms and up is one bucket"
        );
        assert_eq!(h.index(u64::MAX), overflow);

        let mut h = Histogram::latency();
        for micros in [500, 1_500, 3_000, 3_500, 200_000_000] {
            h.record(micros);
        }
        assert_eq!(
            h.octaves(),
            [[0, 1], [1_000, 1], [2_000, 2], [131_072_000, 1]],
            "counts per power of two, with the floor below and the ceiling above"
        );
        assert_eq!(h.count(), 5);
        assert_eq!(h.max(), 200_000_000);
    }

    #[test]
    fn size_buckets_start_at_one_byte_and_an_empty_message_is_its_own() {
        let mut h = Histogram::size();
        for bytes in [0, 1, 2, 3, 40_000, 2_000_000] {
            h.record(bytes);
        }
        assert_eq!(
            h.octaves(),
            [[0, 1], [1, 1], [2, 2], [32_768, 1], [1_048_576, 1]]
        );
        assert_eq!(h.sum(), 2_040_006);
    }

    #[test]
    fn a_percentile_read_off_the_buckets_is_within_a_tenth_of_the_exact_one() {
        // A spread like a shim's: most calls a few hundred milliseconds, a tail of seconds.
        let mut values: Vec<u64> = (0..1_000_u64)
            .map(|i| 150_000 + (i * i * 37) % 2_500_000)
            .collect();
        let mut h = Histogram::latency();
        for value in &values {
            h.record(*value);
        }
        values.sort_unstable();
        for q in [0.5, 0.9, 0.99] {
            let exact = values[(q * 1_000.0_f64).ceil() as usize - 1] as f64;
            let estimate = h.quantile(q);
            let error = (estimate - exact).abs() / exact;
            assert!(
                error < 0.10,
                "p{q}: estimate {estimate} vs exact {exact} ({error:.3} off)"
            );
        }
        assert!(h.quantile(1.0) <= h.max() as f64);
        assert_eq!(Histogram::latency().quantile(0.5), 0.0, "empty is zero");
        let mut one = Histogram::latency();
        one.record(4_321);
        assert_eq!(one.quantile(0.5), 4_321.0, "kept within what was seen");
    }

    #[test]
    fn outcomes_are_read_off_the_cause_not_the_provider_label() {
        let status = |status: u16| ChatError::Status {
            status,
            body: "conversations.history answered channel_not_found".to_owned(),
        };
        let cases = [
            (ChatError::Transport("reset".to_owned()), Outcome::Transport),
            (status(401), Outcome::Unauthorized),
            (status(403), Outcome::Forbidden),
            (status(404), Outcome::NotFound),
            (status(400), Outcome::ClientError),
            (status(409), Outcome::ClientError),
            (status(429), Outcome::RateLimited),
            (status(502), Outcome::ServerError),
            (status(302), Outcome::UnexpectedStatus),
            (ChatError::Shape("odd".to_owned()), Outcome::Shape),
            (ChatError::Refused("no".to_owned()), Outcome::Refused),
            (
                ChatError::CursorExpired("old".to_owned()),
                Outcome::CursorExpired,
            ),
            (
                ChatError::RateLimited(RateLimitExhausted {
                    provider: "slack",
                    route: "conversations.history".to_owned(),
                    attempts: 5,
                    waited: Duration::from_secs(4),
                    budget: Duration::from_secs(30),
                    retry_after: Duration::from_secs(30),
                    global: false,
                }),
                Outcome::RateLimited,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(Outcome::of(&error), expected, "{error}");
            let labelled = error.with_provider("Slack").with_provider("Outer");
            assert_eq!(Outcome::of(&labelled), expected, "{labelled}");
        }
        assert_eq!(Outcome::Abandoned.as_str(), "abandoned");
        assert_eq!(
            serde_json::to_value(Outcome::RateLimited).expect("serializes"),
            serde_json::json!("rate_limited")
        );
    }

    #[test]
    fn a_timer_dropped_before_it_finished_is_counted_as_abandoned() {
        let profile = UpstreamProfile::new();
        let slack = profile.provider("slack", "Slack");
        drop(slack.start(Series::ShimCall, "conversations.history"));
        let _took = slack
            .start(Series::Operation, "fetch_timeline")
            .finish(Outcome::Ok, 7);
        let report = slack.report();
        assert_eq!(report.shim_calls.len(), 1);
        assert_eq!(report.shim_calls[0].outcome, Outcome::Abandoned);
        assert_eq!(report.shim_calls[0].rows, None, "calls carry no rows");
        assert_eq!(report.operations[0].rows, Some(7));
        assert!(
            Arc::ptr_eq(&slack, &profile.provider("slack", "ignored")),
            "one provider key is one profile"
        );
    }

    #[test]
    fn an_hour_is_reported_once_and_the_totals_keep_counting() {
        let profile = UpstreamProfile::new();
        let slack = profile.provider("slack", "Slack");
        slack.record(
            Series::ShimCall,
            "conversations.history",
            Outcome::Ok,
            Duration::from_millis(700),
            0,
        );
        let hour = profile.take_hour();
        assert_eq!(hour.len(), 1);
        assert_eq!(hour[0].shim_call_count(), 1);
        assert_eq!(hour[0].shim_calls[0].p50_ms, 700.0);
        let next = profile.take_hour();
        assert_eq!(next[0].shim_call_count(), 0, "the next hour starts empty");
        assert!(next[0].shim_calls.is_empty());
        assert_eq!(
            profile.report().providers[0].shim_calls[0].count,
            1,
            "diagnostics keep the total since start"
        );
    }
}
