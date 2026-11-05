//! `#208 persistent-cache`, milestone `cache-store`: benchmarks of the message cache, IGNORED by
//! default. They take a few minutes and their numbers mean something only in a release build:
//!
//! ```text
//! cargo test --release --test cache_bench -- --ignored --nocapture --test-threads 1
//! ```
//!
//! What they measure, against the plan's targets:
//!
//! - the search milestone's exact SQL (rare terms through the index, common and short ones by the
//!   newest-first scan, and thread search), on 110k rows in one channel and on 12 channels of 10k,
//!   with p50 and p99 over repeated runs, and with the candidates re-matched in Rust;
//! - the writer's sustained rate, filling those files through `offer` in 25 ms batches, and how
//!   long its background merging takes to catch up afterwards;
//! - evicting a tenth of the 110k-row file, erasing one 10k-row channel, rebuilding `search_text`
//!   for 110k rows, and the hourly pass: each one's total time, its longest step, and — what the
//!   stall targets are for — how long a barrier taken while it runs waits, measured by a probe
//!   that offers one message every 50 ms and waits for it.
//!
//! The corpus is this crate's own README, cut into messages of about 270 bytes: prose, not random
//! strings, so trigram frequencies are those of real text. Six messages carry a rare word. Every
//! report line starts with the load average, because the development host is shared.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{named_params, Connection, OpenFlags};
use vibe_talk::cache::search::{self, Path as SearchPath};
use vibe_talk::cache::{
    CacheOptions, ChanKey, Incoming, Limits, Listing, Maintenance, MessageCache, Observation,
    Rendered, RouteSnapshot, Scope, SearchTextSource,
};
use vibe_talk::model::{ChannelId, Message, MessageId, UserId};
use vibe_talk::threads::MessageThread;

const RARE: &str = "quokkaferret";
const ABSENT: &str = "zzqxjvbw";
const COMMON: &str = "the";
const SHORT: &str = "go";
/// A two-letter term no message holds: the scan reads the whole channel to find nothing.
const SHORT_RARE: &str = "qz";
const RUNS: usize = 200;
/// How often the probe offers a message while bulk work runs.
const PROBE_EVERY: Duration = Duration::from_millis(50);
/// The barrier a current read takes: past this it goes upstream instead.
const BARRIER: Duration = Duration::from_millis(100);

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

fn corpus() -> Vec<String> {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("the README");
    text.split_whitespace().map(str::to_owned).collect()
}

fn message(
    id: u64,
    channel: &str,
    words: &[String],
    created_ms: i64,
    thread: Option<u64>,
) -> Message {
    let start = usize::try_from(id.wrapping_mul(7919)).unwrap_or(0) % (words.len() - 60);
    let mut content = String::new();
    for word in &words[start..] {
        if content.len() > 260 {
            break;
        }
        if !content.is_empty() {
            content.push(' ');
        }
        content.push_str(word);
    }
    if id % 18_333 == 7 {
        content.push_str(" a ");
        content.push_str(RARE);
    }
    Message {
        thread: thread.map(|t| MessageThread {
            id: format!("t{t}"),
            root_message_id: None,
            is_root: false,
            reply_count: None,
            reply_count_exact: false,
        }),
        id: MessageId(format!("{channel}-{id}")),
        channel_id: ChannelId(channel.to_owned()),
        author: format!("author{}", id % 37),
        author_id: UserId(format!("U{}", id % 37)),
        author_is_bot: false,
        timestamp: vibe_talk::clock::iso_from_ms(created_ms),
        spoken_time: String::new(),
        reply_to: None,
        content,
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        reactions: None,
        heading: None,
    }
}

struct Bench {
    cache: MessageCache,
    dir: vibe_talk::testing::TempDir,
    channels: Vec<ChanKey>,
}

impl Bench {
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("cache").join("bench.sqlite3")
    }
}

fn probe_key() -> ChanKey {
    ChanKey::new("p", "probe")
}

fn routes(channels: &[ChanKey]) -> RouteSnapshot {
    RouteSnapshot::new(
        channels.iter().cloned().chain([probe_key()]),
        ["p".to_owned()],
    )
}

/// Fill a cache through the writer, `per_channel` rows in each of `channels`, one in five a thread
/// reply. Reports the sustained rate.
fn fill(tag: &str, channels: usize, per_channel: u64) -> Bench {
    let dir = vibe_talk::testing::TempDir::new(tag);
    let cache = MessageCache::open(CacheOptions {
        limits: Limits {
            max_bytes: 8 * 1024 * 1024 * 1024,
            ..Limits::default()
        },
        ..CacheOptions::new(dir.path().join("cache").join("bench.sqlite3"))
    })
    .expect("open");
    let keys: Vec<ChanKey> = (0..channels)
        .map(|c| ChanKey::new("p", format!("C{c}")))
        .collect();
    cache.retain_only(routes(&keys));
    let words = corpus();
    let now = vibe_talk::cache::wall_ms();
    let load_before = load();
    let started = Instant::now();
    let mut rows = 0_u64;
    for (c, chan) in keys.iter().enumerate() {
        let mut i = 0;
        while i < per_channel {
            let batch: Vec<Incoming> = (i..(i + 100).min(per_channel))
                .map(|n| {
                    let created = now - i64::try_from(per_channel - n).unwrap_or(0) * 20_000;
                    let id = n + u64::try_from(c).unwrap_or(0) * 1_000_000;
                    if n % 5 == 0 {
                        let thread = n / 50;
                        Incoming::new(
                            message(id, &chan.channel_id, &words, created, Some(thread)),
                            Scope::Thread(format!("t{thread}")),
                        )
                    } else {
                        Incoming::new(
                            message(id, &chan.channel_id, &words, created, None),
                            Scope::Main,
                        )
                    }
                })
                .collect();
            rows += u64::try_from(batch.len()).unwrap_or(0);
            let stamp = cache.stamp(chan);
            cache.offer(
                chan.clone(),
                stamp,
                Observation::Listing(Listing {
                    scope: Scope::Main,
                    rows: batch,
                    observed_ms: now,
                    proof: None,
                }),
            );
            i += 100;
            // Keep the queue short of its byte bound: this measures the writer, not the drop path.
            if i % 5_000 == 0 {
                assert!(cache.barrier_blocking(Duration::from_secs(120)));
            }
        }
    }
    assert!(cache.barrier_blocking(Duration::from_secs(600)));
    let applied = started.elapsed();
    assert!(cache.quiesce_blocking(Duration::from_secs(600)));
    let quiet = started.elapsed();
    let stats = cache.stats();
    #[allow(clippy::cast_precision_loss)]
    let rate = rows as f64 / applied.as_secs_f64();
    eprintln!(
        "[load {load_before}] [{tag}] writer: {rows} rows applied in {:.2} s = {rate:.0} rows/s; \
         background merging done {:.2} s later; {} batches, longest {:.1} ms; {} steps, longest \
         {:.1} ms ({} merge steps); longest checkpoint {:.1} ms on the writer, {:.1} ms on its \
         own thread; file {:.1} MB; dropped {}",
        applied.as_secs_f64(),
        (quiet - applied).as_secs_f64(),
        stats.batches,
        ms(stats.batch_max_us),
        stats.steps,
        ms(stats.step_max_us),
        stats.merge_steps,
        ms(stats.writer_checkpoint_max_us),
        ms(stats.checkpoint_max_us),
        mb(cache.bytes_on_disk()),
        stats.ops_dropped,
    );
    assert_eq!(stats.ops_dropped, 0);
    Bench {
        cache,
        dir,
        channels: keys,
    }
}

#[allow(clippy::cast_precision_loss)]
fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn percentiles(mut samples: Vec<Duration>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    samples.sort();
    let at = |q: f64| {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let index = ((samples.len() as f64 - 1.0) * q).round() as usize;
        samples[index].as_secs_f64() * 1000.0
    };
    (at(0.5), at(0.99))
}

/// Offers one message every [`PROBE_EVERY`] on the probe channel, and times how long each takes
/// to be applied: what a current read's barrier would wait while the work being measured runs.
struct Probe {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Vec<Duration>>,
}

impl Probe {
    fn start(cache: &Arc<MessageCache>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let cache = Arc::clone(cache);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let words = corpus();
                let key = probe_key();
                let mut waits = Vec::new();
                let mut n = 0_u64;
                while !stop.load(Ordering::SeqCst) {
                    n += 1;
                    let stamp = cache.stamp(&key);
                    let message = message(
                        9_000_000 + n,
                        "probe",
                        &words,
                        vibe_talk::cache::wall_ms(),
                        None,
                    );
                    let started = Instant::now();
                    cache.offer(
                        key.clone(),
                        stamp,
                        Observation::Rows {
                            rows: vec![Incoming::new(message, Scope::Main)],
                            scope: None,
                            observed_ms: vibe_talk::cache::wall_ms(),
                        },
                    );
                    if cache.barrier_blocking(Duration::from_secs(60)) {
                        waits.push(started.elapsed());
                    }
                    std::thread::sleep(PROBE_EVERY.saturating_sub(started.elapsed()));
                }
                waits
            })
        };
        Self { stop, handle }
    }

    /// Stop, and say how the barriers went.
    fn finish(self) -> String {
        self.stop.store(true, Ordering::SeqCst);
        let waits = self.handle.join().expect("probe");
        let late = waits.iter().filter(|w| **w > BARRIER).count();
        let max = waits.iter().max().copied().unwrap_or_default();
        let count = waits.len();
        let (p50, p99) = percentiles(waits);
        format!(
            "{count} probe barriers: p50 {p50:.1} ms, p99 {p99:.1} ms, max {:.1} ms, {late} over \
             {} ms",
            max.as_secs_f64() * 1000.0,
            BARRIER.as_millis()
        )
    }
}

fn chan_id(connection: &Connection, chan: &ChanKey) -> i64 {
    connection
        .query_row(
            "SELECT chan FROM channel WHERE provider_key = ?1 AND channel_id = ?2",
            [&chan.provider_key, &chan.channel_id],
            |row| row.get(0),
        )
        .expect("interned")
}

/// Run one term through the shape `choose` picks, `RUNS` times, and with Rust re-matching.
fn search_term(connection: &Connection, chan: i64, term: &str, tag: &str) {
    let terms = vec![term.to_owned()];
    let path = search::choose(connection, chan, &terms).expect("choose");
    let query = search::fts_query(&terms);
    let scan_sql = search::scan_sql(1);
    let run = |shape: SearchPath| -> Vec<i64> {
        match shape {
            SearchPath::Index => {
                let mut statement = connection.prepare_cached_free(search::RARE_SQL);
                statement
                    .query_map(
                        named_params! {":q": query, ":chan": chan, ":from_ms": 0_i64,
                        ":before_ms": i64::MAX, ":limit": 50_i64},
                        |row| row.get(0),
                    )
                    .expect("query")
                    .collect::<rusqlite::Result<Vec<i64>>>()
                    .expect("rows")
            }
            SearchPath::Scan => {
                let mut statement = connection.prepare_cached_free(&scan_sql);
                statement
                    .query_map(
                        named_params! {":chan": chan, ":from_ms": 0_i64, ":before_ms": i64::MAX,
                        ":limit": 50_i64, ":t1": term},
                        |row| row.get(0),
                    )
                    .expect("query")
                    .collect::<rusqlite::Result<Vec<i64>>>()
                    .expect("rows")
            }
        }
    };
    let load_now = load();
    for shape in [SearchPath::Index, SearchPath::Scan] {
        if shape == SearchPath::Index && term.chars().count() < 3 {
            continue;
        }
        let mut samples = Vec::with_capacity(RUNS);
        let mut hits = 0;
        for _ in 0..RUNS {
            let started = Instant::now();
            hits = run(shape).len();
            samples.push(started.elapsed());
        }
        let (p50, p99) = percentiles(samples);
        let chosen = if shape == path { " (chosen)" } else { "" };
        eprintln!(
            "[load {load_now}] [{tag}] {term:>12} {shape:?}{chosen}: {hits} hits, p50 {p50:.3} ms, \
             p99 {p99:.3} ms"
        );
    }
    // The chosen shape end to end: the choice, the SQL, and every candidate matched again in Rust.
    let mut samples = Vec::with_capacity(RUNS);
    let mut fetch = connection
        .prepare("SELECT author, content FROM message WHERE row_id = ?1")
        .expect("fetch");
    for _ in 0..RUNS {
        let started = Instant::now();
        let shape = search::choose(connection, chan, &terms).expect("choose");
        let candidates = run(shape);
        let mut kept = 0;
        for row_id in candidates {
            let (author, content): (String, String) = fetch
                .query_row([row_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("row");
            if search::rematch(&format!("{author}\n{content}"), &terms) {
                kept += 1;
            }
        }
        assert!(kept <= 50);
        samples.push(started.elapsed());
    }
    let (p50, p99) = percentiles(samples);
    eprintln!(
        "[load {load_now}] [{tag}] {term:>12} with re-matching: p50 {p50:.3} ms, p99 {p99:.3} ms"
    );
}

fn search_threads(connection: &Connection, chan: i64, term: &str, tag: &str) {
    let query = search::fts_query(&[term.to_owned()]);
    let mut samples = Vec::with_capacity(RUNS);
    let mut threads = 0;
    for _ in 0..RUNS {
        let started = Instant::now();
        let mut statement = connection.prepare_cached_free(search::THREADS_SQL);
        threads = statement
            .query_map(named_params! {":q": query, ":chan": chan}, |row| {
                row.get::<_, String>(0)
            })
            .expect("query")
            .count();
        samples.push(started.elapsed());
    }
    let (p50, p99) = percentiles(samples);
    eprintln!(
        "[load {}] [{tag}] threads {term:>12}: {threads} threads, p50 {p50:.3} ms, p99 {p99:.3} ms",
        load()
    );
}

/// `Connection::prepare` under a name that says why it is not cached: this crate builds rusqlite
/// without its statement cache, and the benchmark should pay what a route would.
trait PrepareFree {
    fn prepare_cached_free(&self, sql: &str) -> rusqlite::Statement<'_>;
}

impl PrepareFree for Connection {
    fn prepare_cached_free(&self, sql: &str) -> rusqlite::Statement<'_> {
        self.prepare(sql).expect("prepare")
    }
}

fn reader(path: &std::path::Path) -> Connection {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("reader");
    connection
        .execute_batch("PRAGMA temp_store = MEMORY; PRAGMA query_only = ON;")
        .expect("pragmas");
    connection
}

fn searches(bench: &Bench, channel: usize, tag: &str) {
    let connection = reader(&bench.path());
    let chan = chan_id(&connection, &bench.channels[channel]);
    for term in [RARE, ABSENT, COMMON, SHORT, SHORT_RARE] {
        search_term(&connection, chan, term, tag);
    }
    for term in [RARE, COMMON] {
        search_threads(&connection, chan, term, tag);
    }
}

/// Run `work` with the probe going, then wait for the background work it left, and report. The
/// probe runs until that work is done, or for `probe_for` when given: the hourly integrity check
/// waits for a quiet moment, which a probe never leaves.
fn measured(
    cache: &Arc<MessageCache>,
    tag: &str,
    what: &str,
    probe_for: Option<Duration>,
    work: impl FnOnce(&MessageCache),
) {
    let load_before = load();
    let before = cache.stats();
    let probe = Probe::start(cache);
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    work(cache);
    assert!(cache.barrier_blocking(Duration::from_secs(60)));
    let applied = started.elapsed();
    let barriers = match probe_for {
        Some(window) => {
            std::thread::sleep(window.saturating_sub(started.elapsed()));
            probe.finish()
        }
        None => {
            assert!(cache.quiesce_blocking(Duration::from_secs(1_200)));
            probe.finish()
        }
    };
    assert!(cache.quiesce_blocking(Duration::from_secs(1_200)));
    let done = started.elapsed();
    let after = cache.stats();
    eprintln!(
        "[load {load_before}] [{tag}] {what}: applied in {:.1} ms, all done in {:.2} s; longest \
         step {:.1} ms (eviction step {:.1} ms, compaction step {:.1} ms); {} steps, {} of them \
         compaction; longest checkpoint {:.1} ms on the writer, {:.1} ms on its own thread; \
         erased {} rows, evicted {} rows, expired {} rows; {barriers}",
        applied.as_secs_f64() * 1000.0,
        done.as_secs_f64(),
        ms(after.step_max_us),
        ms(after.eviction_chunk_max_us),
        ms(after.compaction_slice_max_us),
        after.steps - before.steps,
        after.compaction_steps - before.compaction_steps,
        ms(after.writer_checkpoint_max_us),
        ms(after.checkpoint_max_us),
        after.erased_rows - before.erased_rows,
        after.evicted_rows - before.evicted_rows,
        after.expired_rows - before.expired_rows,
    );
}

fn reopen(
    path: &std::path::Path,
    limits: Limits,
    source: Option<Arc<dyn SearchTextSource>>,
) -> Arc<MessageCache> {
    let mut options = CacheOptions {
        limits,
        ..CacheOptions::new(path)
    };
    if let Some(source) = source {
        options.search_text = source;
    }
    Arc::new(MessageCache::open(options).expect("reopen"))
}

fn used_bytes(path: &std::path::Path) -> u64 {
    let c = reader(path);
    let pages: i64 = c
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("pages");
    let free: i64 = c
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .expect("free");
    let size: i64 = c
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .expect("size");
    u64::try_from((pages - free) * size).expect("size")
}

#[test]
#[ignore = "a benchmark: run by hand in release mode"]
fn bench_one_channel_of_110k_rows_search_eviction_and_the_hourly_pass() {
    let bench = fill("bench-110k", 1, 110_000);
    searches(&bench, 0, "110k");
    let path = bench.path();
    let Bench {
        cache,
        dir,
        channels,
    } = bench;
    drop(cache);

    // The hourly pass with nothing due: the scans, the vacuum and the checkpoint. The integrity
    // check waits for a quiet moment, which the probe never leaves; it is timed alone after.
    let cache = reopen(&path, Limits::default(), None);
    cache.retain_only(routes(&channels));
    assert!(cache.quiesce_blocking(Duration::from_secs(600)));
    measured(
        &cache,
        "110k",
        "hourly pass, nothing due",
        Some(Duration::from_secs(20)),
        |c| {
            c.maintain(Maintenance::Hourly);
        },
    );
    let started = Instant::now();
    assert!(cache.quiesce_blocking(Duration::from_secs(600)));
    eprintln!(
        "[load {}] [110k] the integrity check, run once the queue was quiet: done {:.2} s after \
         the pass",
        load(),
        started.elapsed().as_secs_f64()
    );
    drop(cache);

    // Evict a tenth: reopen with the limit at the pages now in use, so the target (90 % of it) is
    // a tenth below.
    let used = used_bytes(&path);
    let cache = reopen(
        &path,
        Limits {
            max_bytes: used,
            ..Limits::default()
        },
        None,
    );
    cache.retain_only(routes(&channels));
    let before = cache.counts().served().expect("counts").messages;
    measured(
        &cache,
        "110k",
        &format!("eviction to 90 % of {:.1} MB from {before} rows", mb(used)),
        None,
        |c| c.maintain(Maintenance::Bounds),
    );
    eprintln!(
        "[load {}] [110k] after eviction: {} rows, file {:.1} MB",
        load(),
        cache.counts().served().expect("counts").messages,
        mb(cache.bytes_on_disk())
    );
    drop(cache);
    drop(dir);
}

struct Relabelled;

impl SearchTextSource for Relabelled {
    fn version(&self) -> String {
        "bench-relabelled".to_owned()
    }
    fn rendered(&self, _provider_key: &str, message: &Message) -> Rendered {
        Rendered {
            text: format!("{} label#1", message.content),
            hrefs: vec!["https://example.test/1".to_owned()],
        }
    }
}

#[test]
#[ignore = "a benchmark: run by hand in release mode"]
fn bench_twelve_channels_of_10k_rows_search_erase_and_rebuild() {
    let bench = fill("bench-12x10k", 12, 10_000);
    searches(&bench, 5, "12x10k");
    let Bench {
        cache,
        dir,
        channels,
    } = bench;
    // Each measurement on a fresh handle, so its longest step is its own.
    let path = dir.path().join("cache").join("bench.sqlite3");
    drop(cache);
    let cache = reopen(&path, Limits::default(), None);
    cache.retain_only(routes(&channels));
    assert!(cache.quiesce_blocking(Duration::from_secs(600)));
    let doomed = channels[5].clone();
    measured(
        &cache,
        "12x10k",
        "erase of one 10k-row channel",
        None,
        |c| {
            c.erase_channels(std::slice::from_ref(&doomed));
        },
    );
    drop(cache);

    // A new rendering version: every row's search_text is rebuilt, starting as the file opens.
    let reopened = Instant::now();
    let cache = reopen(
        &path,
        Limits {
            max_bytes: 8 * 1024 * 1024 * 1024,
            ..Limits::default()
        },
        Some(Arc::new(Relabelled)),
    );
    cache.retain_only(routes(&channels));
    measured(
        &cache,
        "12x10k",
        "search_text rebuild of 110k rows",
        None,
        |_| {},
    );
    assert!(cache.search_text_current());
    eprintln!(
        "[load {}] [12x10k] the rebuild, from the reopen: {:.2} s",
        load(),
        reopened.elapsed().as_secs_f64()
    );
    drop(cache);
    drop(dir);
}
