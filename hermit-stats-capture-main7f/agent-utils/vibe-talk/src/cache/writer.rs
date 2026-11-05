//! The single writer: one thread that owns the cache's read-write connection, applies every
//! queued operation in sequence order, and does the cache's slow work in short steps between
//! batches.
//!
//! - **Batches are time-bounded.** Observations are applied in one transaction until the queue is
//!   empty, [`BATCH`] has passed, or a barrier raised the flush flag; then the batch commits and
//!   is published to barrier waiters at once, whatever comes after it. There are no savepoints:
//!   FTS5 writes its pending terms out as a new index segment at every savepoint, so one per
//!   observation would leave a segment per message. When an observation fails, the batch is
//!   rolled back and its observations are applied again one transaction each, so only the
//!   failing one is lost.
//! - **Control operations** (an erase, a sweep, maintenance) run alone, after the batch before
//!   them has committed and been published, and are published themselves as soon as their own
//!   short transaction commits. An erase detaches the channel's rows from its name (no read can
//!   reach them from then on) and leaves deleting them to background steps.
//! - **Background work runs in steps** of about [`STEP`], one transaction each, alternating with
//!   the queue so that neither starves the other: the rows of erased channels, expired
//!   tombstones, the age and revalidation pass, eviction, rebuilding `search_text`, compacting the
//!   index after deletions by marker, giving free pages back, checkpoints, merging the index's
//!   segments (fitted in among long-running work, too), and the hourly integrity check at a quiet
//!   moment. Steps that write their index entries out at the end learn how long to work first.
//! - **The full-text index never merges inside a batch.** FTS5 `automerge` is off and merging is
//!   background work, so a batch's commit writes its own rows' entries and nothing more. A
//!   deletion's cost in the index is paid where it is measured: whatever deleted index entries
//!   flushes them before the batch or step decides whether to go on.
//! - **Deleted text leaves the index at once, or a compaction is owed.** FTS5 `secure-delete` is
//!   on at every commit, so a tombstone or an edit takes the old text out of the index's pages
//!   in place. A bulk deletion (an erase or age pass large enough to pay for it, eviction, a
//!   rebuild of `search_text`) turns it off INSIDE its own transaction, deletes by marker, turns
//!   it back on before committing, and records the compaction it owes in the same transaction;
//!   the compaction is tracked to its end through the index's structure record
//!   ([`super::structure`]).
//! - **The log is checkpointed off this thread.** SQLite's automatic checkpoint is off; the
//!   checkpointer copies the log back concurrently, and this thread only checkpoints it itself
//!   when it has grown large and the checkpointer has nearly caught up, or to truncate it after
//!   a deletion.
//! - **Hot statements are prepared once per connection**, in this thread's frame.
//! - **Every merge rule is in [`crate::cache::merge`]**; this file applies them. It never uses a
//!   replacing insert: a conflicting row would be deleted without its delete trigger, leaving its
//!   old text in the full-text index for good. Every upsert is a read, a decision, and an
//!   `INSERT` or an `UPDATE`, and `search_text` is set only when it changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension as _, Statement};

use super::checkpoint::Checkpointer;
use super::coverage::{self, Head, Interval, Params};
use super::merge::{self, Arrival, Held, Offered, Verdict};
use super::rows::{self, Extra};
use super::schema::sqlite_code;
use super::structure;
use super::{
    file, lock, CacheError, ChanKey, Envelope, Erasing, Incoming, ListEnd, ListStart, Listing,
    Maintenance, Observation, Op, Push, RouteSnapshot, Scope, Shared, Stamp, Stats, ThreadRecord,
    BATCH, DELETE_CHUNK, REVALIDATE_WITHIN_MS, STEP, TOMBSTONE_KEEP_MS,
};
use crate::model::MessageId;

/// How often the hourly pass runs.
const HOURLY: Duration = Duration::from_secs(60 * 60);
/// How often the weekly merge runs, in local milliseconds.
const WEEKLY_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// How long the queue must have been quiet before the hourly integrity check runs: it reads the
/// whole index in one statement.
const IDLE_FOR: Duration = Duration::from_secs(1);
/// How long a `TRUNCATE` checkpoint that found a reader waits before it tries again.
const CHECKPOINT_RETRY: Duration = Duration::from_millis(250);
/// How long a truncation waits, past its retry time, for the checkpointer to catch up before
/// it goes ahead anyway.
const TRUNCATE_PATIENCE: Duration = Duration::from_secs(2);
/// How long background work rests after a step failed; doubled after each failure in a row, up
/// to a minute.
const BACKOFF: Duration = Duration::from_secs(1);
/// The most doublings of [`BACKOFF`].
const BACKOFF_DOUBLINGS: u32 = 6;
/// Rows one deletion step picks; the step stops earlier when its time is up.
const PICK: i64 = 256;
/// A bulk deletion of at least one row in this many (and at least [`MARKER_MIN`] rows) deletes by
/// marker and compacts afterwards. Below it, rows leave the index in place: measured on 60k rows,
/// in place costs about 5 ms and some hundreds of kilobytes of log per row, by marker about a
/// tenth of a millisecond, and the compaction rewrites the index once, about a kilobyte a row.
const MARKER_RATIO: i64 = 400;
/// The fewest rows a bulk deletion deletes by marker.
const MARKER_MIN: i64 = 32;
/// Once the write-ahead log is past this, the writer checkpoints it itself between steps, as soon
/// as the checkpointer has nearly caught up: the checkpointer cannot finish while the writer
/// never pauses, and a log is restarted only by a write that finds every frame copied back. The
/// writer then copies only the last few frames, and its sync has little left to write.
const WAL_HIGH_BYTES: u64 = 32 * 1024 * 1024;
/// Past this the writer checkpoints the log whether the checkpointer has caught up or not.
const WAL_CEILING_BYTES: u64 = 128 * 1024 * 1024;
/// "Nearly caught up": at most this many frames (four kilobytes each) left to copy.
const SMALL_BACKLOG: i64 = 512;
/// While one kind of background work runs on, a merge step is fitted in after this many of its
/// steps, so that the index's segments never pile up into a merge FTS5 would force on a commit.
const MERGE_EVERY: u32 = 2;
/// Row ids one step of the revalidation pass examines.
const WINDOW: i64 = 1_024;
/// Rows one `search_text` rebuild step picks.
const REBUILD_PICK: i64 = 128;
/// Pages of the full-text index one merge step writes.
const MERGE_PAGES: i64 = 64;
/// Pages of the full-text index one compaction step writes.
const COMPACT_PAGES: i64 = 100;
/// Free pages one vacuum step gives back.
const VACUUM_PAGES: i64 = 256;
/// Free pages the hourly pass gives back at most.
const HOURLY_VACUUM_PAGES: i64 = 256;
/// Pages the weekly merge writes.
const WEEKLY_MERGE_PAGES: i64 = 500;
/// [`DELETE_CHUNK`] as SQLite binds it.
#[allow(clippy::cast_possible_wrap)]
const CHUNK_LIMIT: i64 = DELETE_CHUNK as i64;
/// The writer's page cache, as SQLite spells a size in KiB: negative.
const WRITER_CACHE_KIB: i64 = -32_768;
/// The most eviction rounds (delete, compact, measure) between two hourly passes.
const EVICTION_ROUNDS: usize = 6;
/// FTS5 `usermerge`: a background merge combines a level's segments once it has this many.
const USERMERGE: i64 = 4;
/// `usermerge` while a compaction runs: the compaction's own merge, and any level of new
/// segments, may then always go on.
const COMPACT_USERMERGE: i64 = 2;
/// The provider key an erased channel's row is renamed to until its rows are gone. Configured
/// provider keys are lowercase letters, digits and hyphens, so it can never be one.
pub(crate) const ERASED_PROVIDER: &str = "(erased)";

/// Open the read-write connection. The file already exists: it was created `0600` first.
pub(crate) fn open_connection(path: &Path) -> Result<Connection, CacheError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| CacheError::Sqlite(sqlite_code(&e)))
}

/// The `search_text` rules a file was built with.
pub(crate) fn search_text_version(connection: &Connection) -> Option<String> {
    connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'search_text_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
}

/// The writer thread's body. `generation` is the file generation it serves; `started` is told
/// once its first steps are done.
pub(crate) fn run(
    connection: Connection,
    rx: &Receiver<Envelope>,
    shared: &Arc<Shared>,
    path: &Path,
    generation: u64,
    started: &mpsc::Sender<()>,
) {
    let ack = match Hot::prepare(&connection) {
        Ok(hot) => {
            let checkpointer = match Checkpointer::start(path, Arc::clone(shared)) {
                Ok(checkpointer) => Some(checkpointer),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "the message cache checkpointer did not start; the writer checkpoints itself"
                    );
                    None
                }
            };
            let mut writer = Writer {
                conn: &connection,
                shared,
                hot,
                path,
                checkpointer,
                generation,
                work: Work::default(),
                last_hourly: Instant::now(),
                last_batch: Instant::now(),
                backoff_until: None,
                failures: 0,
                unflushed: 0,
                backfill_armed: false,
                pace_markers: Pace::new(),
                pace_rebuild: Pace::new(),
                since_merge: 0,
            };
            writer.start();
            let _ = started.send(());
            writer.run(rx)
        }
        Err(error) => {
            tracing::warn!(
                code = %sqlite_code(&error),
                "the message cache writer could not prepare its statements; nothing will be cached"
            );
            let _ = started.send(());
            None
        }
    };
    if shared.epochs.generation() == generation {
        shared.seq.set_busy(false);
    }
    let _ = connection.close();
    // Whatever is still queued will never be applied: give its bytes back.
    while let Ok(envelope) = rx.try_recv() {
        shared.budget.release(envelope.bytes);
        if let Op::Close { ack } = envelope.op {
            let _ = ack.send(());
        }
    }
    if let Some(ack) = ack {
        let _ = ack.send(());
    }
}

/// The statements every observation uses, prepared once per connection.
struct Hot<'c> {
    chan_select: Statement<'c>,
    chan_insert: Statement<'c>,
    row_select: Statement<'c>,
    row_insert: Statement<'c>,
    row_update: Statement<'c>,
    row_update_search: Statement<'c>,
    row_delete: Statement<'c>,
    scope_insert: Statement<'c>,
    tomb_select: Statement<'c>,
    tomb_upsert: Statement<'c>,
    tomb_delete: Statement<'c>,
    thread_upsert: Statement<'c>,
    fts_flush: Statement<'c>,
    fts_config: Statement<'c>,
}

impl<'c> Hot<'c> {
    fn prepare(conn: &'c Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            chan_select: conn
                .prepare("SELECT chan FROM channel WHERE provider_key = ?1 AND channel_id = ?2")?,
            chan_insert: conn.prepare(
                "INSERT INTO channel (provider_key, channel_id) VALUES (?1, ?2)
                 ON CONFLICT (provider_key, channel_id) DO NOTHING",
            )?,
            row_select: conn.prepare(
                "SELECT row_id, numeric_id, created_ms, edited_ms, observed_ms, home_scope,
                        thread_id, is_root, reply_to, author_id, author, author_is_bot, content,
                        content_hash, content_partial, search_text, extra
                 FROM message WHERE chan = ?1 AND message_id = ?2",
            )?,
            row_insert: conn.prepare(
                "INSERT INTO message (chan, message_id, numeric_id, created_ms, edited_ms,
                        observed_ms, home_scope, thread_id, is_root, reply_to, author_id, author,
                        author_is_bot, content, content_hash, content_partial, search_text, extra)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18)",
            )?,
            row_update: conn.prepare(
                "UPDATE message SET numeric_id = ?2, edited_ms = ?3, observed_ms = ?4,
                        thread_id = ?5, is_root = ?6, reply_to = ?7, author_id = ?8, author = ?9,
                        author_is_bot = ?10, content = ?11, content_hash = ?12,
                        content_partial = ?13, extra = ?14
                 WHERE row_id = ?1",
            )?,
            row_update_search: conn
                .prepare("UPDATE message SET search_text = ?2 WHERE row_id = ?1")?,
            row_delete: conn.prepare("DELETE FROM message WHERE row_id = ?1")?,
            scope_insert: conn.prepare(
                "INSERT INTO message_scope (chan, scope_key, created_ms, row_id)
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            )?,
            tomb_select: conn
                .prepare("SELECT deleted_ms FROM tombstone WHERE chan = ?1 AND message_id = ?2")?,
            tomb_upsert: conn.prepare(
                "INSERT INTO tombstone (chan, message_id, created_ms, deleted_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (chan, message_id)
                 DO UPDATE SET deleted_ms = MAX(tombstone.deleted_ms, excluded.deleted_ms)",
            )?,
            tomb_delete: conn
                .prepare("DELETE FROM tombstone WHERE chan = ?1 AND message_id = ?2")?,
            // A record derived from a reply carries no count, and changes the stored one only
            // when it moves the activity forward or names the root: re-listing a thread's
            // replies writes nothing.
            thread_upsert: conn.prepare(
                "INSERT INTO thread (chan, thread_id, root_id, reply_count, reply_count_exact,
                        last_activity_ms, verified_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (chan, thread_id) DO UPDATE SET
                    root_id = COALESCE(excluded.root_id, thread.root_id),
                    reply_count = CASE
                        WHEN excluded.reply_count IS NOT NULL
                         AND excluded.verified_ms >= thread.verified_ms
                        THEN excluded.reply_count ELSE thread.reply_count END,
                    reply_count_exact = CASE
                        WHEN excluded.reply_count IS NOT NULL
                         AND excluded.verified_ms >= thread.verified_ms
                        THEN excluded.reply_count_exact ELSE thread.reply_count_exact END,
                    last_activity_ms = MAX(thread.last_activity_ms, excluded.last_activity_ms),
                    verified_ms = CASE
                        WHEN excluded.reply_count IS NOT NULL
                        THEN MAX(thread.verified_ms, excluded.verified_ms)
                        ELSE thread.verified_ms END
                 WHERE (excluded.root_id IS NOT NULL AND thread.root_id IS NOT excluded.root_id)
                    OR excluded.last_activity_ms > thread.last_activity_ms
                    OR (excluded.reply_count IS NOT NULL
                        AND (excluded.verified_ms > thread.verified_ms
                             OR (excluded.verified_ms = thread.verified_ms
                                 AND (thread.reply_count IS NOT excluded.reply_count
                                      OR thread.reply_count_exact <> excluded.reply_count_exact))))",
            )?,
            fts_flush: conn.prepare("INSERT INTO message_fts (message_fts) VALUES ('flush')")?,
            fts_config: conn
                .prepare("INSERT INTO message_fts (message_fts, rank) VALUES (?1, ?2)")?,
        })
    }
}

/// A stored message row, as the writer reads it to decide.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    row_id: i64,
    numeric_id: Option<i64>,
    created_ms: i64,
    edited_ms: Option<i64>,
    observed_ms: i64,
    home_scope: String,
    thread_id: Option<String>,
    is_root: bool,
    reply_to: Option<String>,
    author_id: String,
    author: String,
    author_is_bot: bool,
    content: String,
    content_hash: String,
    content_partial: bool,
    search_text: String,
    extra: String,
}

impl Row {
    fn from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            row_id: row.get(0)?,
            numeric_id: row.get(1)?,
            created_ms: row.get(2)?,
            edited_ms: row.get(3)?,
            observed_ms: row.get(4)?,
            home_scope: row.get(5)?,
            thread_id: row.get(6)?,
            is_root: row.get(7)?,
            reply_to: row.get(8)?,
            author_id: row.get(9)?,
            author: row.get(10)?,
            author_is_bot: row.get(11)?,
            content: row.get(12)?,
            content_hash: row.get(13)?,
            content_partial: row.get(14)?,
            search_text: row.get(15)?,
            extra: row.get(16)?,
        })
    }
}

/// The scope key a row's home scope stands for.
fn home_key(home_scope: &str, thread_id: Option<&str>) -> String {
    match home_scope {
        "thread" => format!("thread:{}", thread_id.unwrap_or_default()),
        other => other.to_owned(),
    }
}

fn later(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn count_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// What became of one observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Applied,
    /// An erase came after its read started.
    Stale,
    /// Its channel is not in the route snapshot.
    Unrouted,
    /// SQLite refused it, alone in its own transaction.
    Failed,
}

/// The age and revalidation pass, in progress.
#[derive(Debug)]
struct Expire {
    /// Each channel's age cutoff, by interned id.
    cutoffs: HashMap<i64, i64>,
    /// The latest of them: no row at or after it is too old for any channel.
    max_cutoff: i64,
    /// Rows observed before this, and covered by no edit-aware read since, go.
    week_ago: i64,
    /// The newest row id when the pass began: the revalidation phase stops there.
    until: i64,
    /// Rows the file held when the pass began, and rows the pass has deleted so far: once it
    /// has deleted enough to pay for a compaction, it goes on by marker.
    total: i64,
    deleted: i64,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Rows older than their channel's cutoff, oldest first, after this (created, row id).
    Age { after: (i64, i64) },
    /// Coverage raised to each channel's cutoff.
    Floors,
    /// Rows nothing revalidated within a week, by row id after this.
    Revalidate { after: i64 },
}

/// A compaction in progress.
#[derive(Debug)]
struct Compact {
    started: Instant,
    /// The segment its merge writes, once its first step has begun that merge.
    output: Option<u64>,
    steps: u32,
    /// Past this many steps (four times what the index's size suggests) it finishes in one go.
    cap: u32,
}

/// What a compaction step found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Progress {
    /// Its merge, writing this segment, goes on.
    Going(u64),
    /// Its merge is over.
    Done,
    /// It cannot tell, for this reason.
    Unsure(&'static str),
}

/// The background work waiting.
#[derive(Debug, Default)]
struct Work {
    /// Detached (erased) channels still hold rows.
    erase: bool,
    /// Tombstones deleted before this local time are to go.
    tombstones: Option<i64>,
    expire: Option<Expire>,
    /// Rows the current eviction round has still to delete.
    evict: Option<usize>,
    /// Eviction rounds since the last hourly pass, or since the file was last under its target.
    rounds: usize,
    /// A compaction should run: marker deletions have left the index larger than it need be.
    compact_due: bool,
    compact: Option<Compact>,
    /// Free pages to give back: `Some(None)` until none are left, `Some(Some(n))` at most `n`.
    vacuum: Option<Option<i64>>,
    /// A merge step may find segments to merge.
    merge: bool,
    /// Pages the weekly merge has still to write.
    weekly_pages: i64,
    /// The hourly integrity check is waiting for a quiet moment, since this instant.
    integrity_since: Option<Instant>,
    /// Deleted text may still be in the write-ahead log.
    checkpoint: bool,
    /// Not before this: the last `TRUNCATE` found a reader.
    checkpoint_after: Option<Instant>,
    /// Since when deleted text has been waiting to leave the log.
    truncate_since: Option<Instant>,
    /// Whether the erase in progress deletes by marker; decided at its first step.
    erase_markers: Option<bool>,
    /// Markers were written while a compaction ran: its merge did not take them in, so another
    /// compaction must follow it.
    compact_again: bool,
}

/// One kind of background step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Task {
    Erase,
    Tombstones,
    Expire,
    Evict,
    Rebuild,
    Compact,
    Vacuum,
    Checkpoint,
    Merge,
    Integrity,
}

impl Task {
    fn name(self) -> &'static str {
        match self {
            Self::Erase => "erase",
            Self::Tombstones => "tombstones",
            Self::Expire => "age limit",
            Self::Evict => "eviction",
            Self::Rebuild => "search text rebuild",
            Self::Compact => "compaction",
            Self::Vacuum => "incremental vacuum",
            Self::Checkpoint => "checkpoint",
            Self::Merge => "merge",
            Self::Integrity => "integrity check",
        }
    }
}

struct Writer<'c> {
    conn: &'c Connection,
    shared: &'c Shared,
    hot: Hot<'c>,
    path: &'c Path,
    /// Copies the log back into the database off this thread. `None` if it could not start:
    /// then SQLite's automatic checkpoint stays on and this thread truncates the log itself.
    checkpointer: Option<Checkpointer>,
    generation: u64,
    work: Work,
    last_hourly: Instant,
    last_batch: Instant,
    backoff_until: Option<Instant>,
    /// Steps that have failed in a row.
    failures: u32,
    /// Index entries deleted since the index last flushed: their cost is paid at the flush.
    unflushed: usize,
    /// Something has written since this thread last checkpointed the log itself. A checkpoint
    /// does not shrink the log file (the next write transaction restarts it), so without this
    /// it would see the same large file and checkpoint again for ever.
    backfill_armed: bool,
    /// How long marker deletions, and rebuilds of `search_text`, work before writing their
    /// index entries out, learnt from how long their steps took.
    pace_markers: Pace,
    pace_rebuild: Pace,
    /// Background steps since the last merge step.
    since_merge: u32,
}

/// How long one kind of step works before it writes its index entries out, which takes a time
/// that grows with the work and that the step cannot see coming: halved when a step overran
/// [`STEP`], grown by a quarter when one took less than half of it.
#[derive(Clone, Copy, Debug)]
struct Pace {
    budget: Duration,
}

impl Pace {
    fn new() -> Self {
        Self { budget: STEP / 2 }
    }

    fn deadline(self, started: Instant) -> Instant {
        started + self.budget
    }

    fn learn(&mut self, took: Duration) {
        if took > STEP {
            self.budget = (self.budget / 2).max(Duration::from_millis(1));
        } else if took < STEP / 2 {
            self.budget = (self.budget * 5 / 4).min(STEP);
        }
    }
}

impl Writer<'_> {
    fn now(&self) -> i64 {
        self.shared.clock.now_ms()
    }

    /// Whether the file this writer serves is being erased: then nothing it does matters, and
    /// everything still queued is discarded.
    fn stale(&self) -> bool {
        self.shared.epochs.generation() != self.generation
    }

    fn rebuilding(&self) -> bool {
        !self.shared.search_text_current.load(Ordering::SeqCst)
    }

    fn pending(&self) -> bool {
        let w = &self.work;
        w.erase
            || w.tombstones.is_some()
            || w.expire.is_some()
            || w.evict.is_some()
            || w.compact_due
            || w.compact.is_some()
            || w.vacuum.is_some()
            || w.merge
            || w.weekly_pages > 0
            || w.integrity_since.is_some()
            || w.checkpoint
            || self.rebuilding()
    }

    /// Deleted text may be in the write-ahead log: have it emptied.
    fn request_truncate(&mut self) {
        self.work.checkpoint = true;
        self.work.truncate_since.get_or_insert_with(Instant::now);
    }

    /// Markers have been written: a compaction is owed, and if one is running it does not cover
    /// them, so another must follow.
    fn owe_compaction(&mut self) {
        if self.work.compact.is_some() {
            self.work.compact_again = true;
        } else {
            self.work.compact_due = true;
        }
    }

    /// Whether a bulk deletion of `rows` of a file of `total` pays for a compaction.
    fn by_marker(rows: i64, total: i64) -> bool {
        rows >= MARKER_MIN && rows.saturating_mul(MARKER_RATIO) >= total
    }

    /// Tell waiters whether background work is left. Called before every applied mark moves.
    fn publish(&self) {
        self.shared.seq.set_busy(!self.stale() && self.pending());
    }

    fn run(&mut self, rx: &Receiver<Envelope>) -> Option<mpsc::Sender<()>> {
        loop {
            // Checked on every turn, not only when the queue goes quiet: a writer that is never
            // idle for an hour must still expire what has aged out.
            if self.last_hourly.elapsed() >= HOURLY {
                self.schedule_hourly();
            }
            // The queue first, then one step of background work, so that neither starves the
            // other: a barrier waits behind at most one step.
            match rx.try_recv() {
                Ok(envelope) => {
                    if let Some(ack) = self.apply(envelope, rx) {
                        return Some(ack);
                    }
                    self.step();
                    continue;
                }
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            if self.step() {
                continue;
            }
            self.publish();
            match rx.recv_timeout(self.wait()) {
                Ok(envelope) => {
                    if let Some(ack) = self.apply(envelope, rx) {
                        return Some(ack);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// How long to wait for the queue when no step can run now.
    fn wait(&self) -> Duration {
        let now = Instant::now();
        let mut wait = HOURLY.saturating_sub(self.last_hourly.elapsed());
        if let Some(until) = self.backoff_until {
            wait = wait.min(until.saturating_duration_since(now));
        }
        if self.work.checkpoint {
            // Until it may be tried again, and at most a little longer than the checkpointer
            // takes to look at the log again.
            let after = self.work.checkpoint_after.unwrap_or(now);
            wait = wait
                .min(after.saturating_duration_since(now))
                .min(Duration::from_millis(100));
        }
        if self.work.integrity_since.is_some() {
            wait = wait.min(IDLE_FOR.saturating_sub(self.last_batch.elapsed()));
        }
        wait.max(Duration::from_millis(1))
    }

    fn apply(&mut self, first: Envelope, rx: &Receiver<Envelope>) -> Option<mpsc::Sender<()>> {
        if matches!(first.op, Op::Observe { .. }) {
            self.batch(first, rx)
        } else {
            self.control(first)
        }
    }

    /// Apply one batch of observations, starting with `first`, in one transaction. A control
    /// operation ends the batch and runs on its own once the batch is committed and published.
    /// Returns the close acknowledgement when that operation was a close.
    fn batch(&mut self, first: Envelope, rx: &Receiver<Envelope>) -> Option<mpsc::Sender<()>> {
        let started = Instant::now();
        let mut done: Vec<(ChanKey, Stamp, Box<Observation>, Outcome)> = Vec::new();
        let mut last_seq = first.seq;
        let mut control = None;
        let began = self.conn.execute_batch("BEGIN IMMEDIATE");
        let mut failed = began.is_err();
        let mut next = Some(first);
        self.unflushed = 0;
        while let Some(envelope) = next.take() {
            self.shared.budget.release(envelope.bytes);
            match envelope.op {
                Op::Observe {
                    chan,
                    stamp,
                    observation,
                } => {
                    last_seq = envelope.seq;
                    let mut outcome = Outcome::Applied;
                    if !failed {
                        match self
                            .observe_op(&chan, stamp, &observation)
                            .and_then(|o| self.flush_deletes().map(|()| o))
                        {
                            Ok(o) => outcome = o,
                            Err(error) => {
                                tracing::warn!(
                                    code = %sqlite_code(&error),
                                    "a message cache write failed; its batch is applied again one by one"
                                );
                                failed = true;
                            }
                        }
                    }
                    done.push((chan, stamp, observation, outcome));
                }
                other => {
                    control = Some(Envelope {
                        seq: envelope.seq,
                        bytes: 0,
                        op: other,
                    });
                    break;
                }
            }
            let flush = self.shared.seq.flush.swap(false, Ordering::SeqCst);
            if failed || flush || started.elapsed() >= BATCH {
                break;
            }
            next = rx.try_recv().ok();
        }
        if !failed {
            if let Err(error) = self.conn.execute_batch("COMMIT") {
                tracing::warn!(code = %sqlite_code(&error), "a message cache batch did not commit");
                failed = true;
            }
        }
        if failed {
            if began.is_ok() {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
            self.unflushed = 0;
            for entry in &mut done {
                let (chan, stamp, observation) = (&entry.0, entry.1, &entry.2);
                let result = self.transaction(|w| w.observe_op(chan, stamp, observation));
                match result {
                    Ok(outcome) => entry.3 = outcome,
                    Err(error) => {
                        Stats::add(&self.shared.stats.ops_failed, 1);
                        tracing::warn!(code = %sqlite_code(&error), "a message cache write was rolled back");
                        entry.3 = Outcome::Failed;
                    }
                }
            }
        }
        for (_, _, _, outcome) in &done {
            let counter = match outcome {
                Outcome::Applied => &self.shared.stats.ops_applied,
                Outcome::Stale => &self.shared.stats.ops_stale,
                Outcome::Unrouted => &self.shared.stats.ops_unrouted,
                Outcome::Failed => continue,
            };
            Stats::add(counter, 1);
        }
        Stats::add(&self.shared.stats.batches, 1);
        Stats::max(&self.shared.stats.batch_max_us, micros(started.elapsed()));
        self.work.merge = true;
        self.backfill_armed = true;
        self.last_batch = Instant::now();
        // Published now, before any control operation behind it runs: what this batch wrote is
        // visible, and a barrier waiting for it must not wait for an erase or an eviction.
        self.publish();
        self.shared.seq.mark_applied(last_seq);
        self.check_bytes();
        match control {
            Some(envelope) => self.control(envelope),
            None => None,
        }
    }

    /// Apply one control operation and publish it. A close is acknowledged by the caller.
    fn control(&mut self, envelope: Envelope) -> Option<mpsc::Sender<()>> {
        let Envelope { seq, op, .. } = envelope;
        if let Op::Close { ack } = op {
            return Some(ack);
        }
        // Queued before a file erase: discarded, as the erase protocol says.
        if !self.stale() {
            self.standalone(op);
        }
        self.publish();
        self.shared.seq.mark_applied(seq);
        self.check_bytes();
        None
    }

    fn standalone(&mut self, op: Op) {
        match op {
            Op::Erase { chans } => {
                let result = self.transaction(|w| w.detach(&chans));
                if result.is_err() {
                    let mut failed = lock(&self.shared.erase_failed);
                    failed.extend(chans.into_iter().map(Erasing::Channel));
                }
                self.report("erase", result);
            }
            Op::EraseProvider { provider_key } => {
                let result = self.transaction(|w| w.erase_provider(&provider_key));
                if result.is_err() {
                    lock(&self.shared.erase_failed).insert(Erasing::Provider(provider_key));
                }
                self.report("provider erase", result);
            }
            Op::RetainOnly { routes } => {
                let result = self.transaction(|w| w.retain_only(&routes));
                if result.is_err() {
                    lock(&self.shared.erase_failed).insert(Erasing::Sweep);
                }
                self.report("sweep", result);
            }
            Op::Maintain { kind } => self.maintain(kind),
            #[cfg(test)]
            Op::Probe { reply } => {
                let mut all = vec![super::probe(self.conn)];
                all.extend(self.checkpointer.as_ref().map(Checkpointer::probe));
                let _ = reply.send(all);
            }
            #[cfg(test)]
            Op::Pause { paused, resume } => {
                let _ = paused.send(());
                let _ = resume.recv();
            }
            Op::Observe { .. } | Op::Close { .. } => {}
        }
        Stats::add(&self.shared.stats.ops_applied, 1);
    }

    fn routed(&self, chan: &ChanKey) -> bool {
        self.shared
            .routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|routes| routes.allows(chan))
    }

    /// Run `work` in its own transaction, outside any batch.
    fn transaction<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match work(self) {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Write out the index entries deleted since the last flush, so their cost is paid before
    /// the batch or step looks at its clock.
    fn flush_deletes(&mut self) -> rusqlite::Result<()> {
        if self.unflushed > 0 {
            self.hot.fts_flush.execute([])?;
            self.unflushed = 0;
        }
        Ok(())
    }

    /// Set an FTS5 option. Each option flushes the index's pending entries first, with the
    /// option's old value.
    fn fts_option(&mut self, name: &str, value: i64) -> rusqlite::Result<()> {
        self.hot.fts_config.execute(params![name, value])?;
        Ok(())
    }

    fn find_chan(&mut self, chan: &ChanKey) -> rusqlite::Result<Option<i64>> {
        self.hot
            .chan_select
            .query_row(params![chan.provider_key, chan.channel_id], |row| {
                row.get(0)
            })
            .optional()
    }

    fn intern(&mut self, chan: &ChanKey) -> rusqlite::Result<i64> {
        if let Some(id) = self.find_chan(chan)? {
            return Ok(id);
        }
        self.hot
            .chan_insert
            .execute(params![chan.provider_key, chan.channel_id])?;
        self.find_chan(chan)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    // ---- observations ---------------------------------------------------------------------

    fn observe_op(
        &mut self,
        chan: &ChanKey,
        stamp: Stamp,
        observation: &Observation,
    ) -> rusqlite::Result<Outcome> {
        if !self.shared.epochs.current(chan, stamp) {
            return Ok(Outcome::Stale);
        }
        if !self.routed(chan) {
            return Ok(Outcome::Unrouted);
        }
        self.observe(chan, observation)?;
        Ok(Outcome::Applied)
    }

    fn observe(&mut self, chan: &ChanKey, observation: &Observation) -> rusqlite::Result<()> {
        let chan_id = self.intern(chan)?;
        let cutoff = self.shared.limits.cutoff_ms(&chan.channel_id, self.now());
        match observation {
            Observation::Listing(listing) => self.listing(chan, chan_id, cutoff, listing),
            Observation::Rows {
                rows,
                scope,
                observed_ms,
            } => {
                for row in rows {
                    self.upsert(
                        chan,
                        chan_id,
                        cutoff,
                        row,
                        *observed_ms,
                        Arrival::Rows,
                        false,
                        scope.as_ref(),
                    )?;
                }
                Ok(())
            }
            Observation::Deleted { ids, observed_ms } => {
                for id in ids {
                    self.delete_id(chan_id, id, None, *observed_ms)?;
                }
                Ok(())
            }
            Observation::Push { push, observed_ms } => match push {
                Push::Upsert {
                    message,
                    update,
                    partial,
                } => {
                    let arrival = if *update {
                        Arrival::PushUpdate
                    } else {
                        Arrival::PushCreate
                    };
                    self.upsert(
                        chan,
                        chan_id,
                        cutoff,
                        message,
                        *observed_ms,
                        arrival,
                        *partial,
                        None,
                    )
                }
                Push::Delete { id, created_ms } => {
                    self.delete_id(chan_id, id, *created_ms, *observed_ms)
                }
            },
            Observation::Threads {
                threads,
                observed_ms,
            } => {
                for thread in threads.iter().filter(|t| t.reply_count != Some(0)) {
                    self.upsert_thread(chan_id, thread, *observed_ms)?;
                }
                Ok(())
            }
        }
    }

    fn listing(
        &mut self,
        chan: &ChanKey,
        chan_id: i64,
        cutoff: i64,
        listing: &Listing,
    ) -> rusqlite::Result<()> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(listing.rows.len());
        let mut newest: Option<i64> = None;
        for row in &listing.rows {
            let Some(created) = rows::created_ms(&row.message) else {
                continue;
            };
            seen.insert(row.message.id.as_str());
            newest = Some(newest.map_or(created, |n| n.max(created)));
            self.upsert(
                chan,
                chan_id,
                cutoff,
                row,
                listing.observed_ms,
                Arrival::Listing,
                false,
                Some(&listing.scope),
            )?;
        }
        // No proof, no claim; and an empty listing proves nothing even with one, because a
        // provider can answer a proven stretch with an empty page that is not its last.
        let (Some(proof), Some(newest)) = (listing.proof, newest) else {
            return Ok(());
        };
        let scope_key = listing.scope.key();
        let stored = self.load_head(chan_id, &scope_key)?;
        let head = match proof.until {
            ListEnd::Newest => {
                let head = coverage::advance_head(stored, newest, listing.observed_ms);
                if Some(head) != stored {
                    self.store_head(chan_id, &scope_key, &head)?;
                }
                self.clear_suspect(chan, listing.observed_ms);
                Some(head)
            }
            ListEnd::Before(_) => stored,
        };
        let from = match proof.from {
            ListStart::ScopeStart => 0,
            ListStart::At(ms) => ms,
        };
        let until = match proof.until {
            ListEnd::Newest => head
                .map_or(newest, |h| h.head_ms.max(newest))
                .saturating_add(1),
            ListEnd::Before(ms) => ms,
        };
        let settled = coverage::absence_bound(head.as_ref(), newest, listing.observed_ms);
        let end = until.min(settled);
        if from < end {
            self.absent(chan_id, &scope_key, from, end, listing.observed_ms, &seen)?;
        }
        let t0 = from.max(cutoff);
        if t0 < end {
            let claim = Interval {
                t0_ms: t0,
                t1_ms: end,
                listed_ms: listing.observed_ms,
                full_ms: proof.full.then_some(listing.observed_ms),
                complete: proof.complete,
                source: proof.source,
            };
            let existing = self.load_coverage(chan_id, &scope_key)?;
            let stamped = coverage::stamp(
                &existing,
                claim,
                head.map(|h| h.head_ms),
                &Params::default(),
            );
            if stamped != existing {
                self.store_coverage(chan_id, &scope_key, &stamped)?;
            }
        }
        Ok(())
    }

    /// A read of the newest end that STARTED after the drop that made the channel suspect has
    /// landed: its head is trusted again. One that started earlier proves nothing about the
    /// dropped offer.
    fn clear_suspect(&self, chan: &ChanKey, observed_ms: i64) {
        let mut suspects = lock(&self.shared.suspects);
        if suspects
            .get(chan)
            .is_some_and(|&dropped_at| observed_ms > dropped_at)
        {
            suspects.remove(chan);
        }
    }

    /// Rule 4: rows of this scope in `[from, to)` that the listing did not carry.
    fn absent(
        &mut self,
        chan_id: i64,
        scope_key: &str,
        from: i64,
        to: i64,
        observed_ms: i64,
        seen: &HashSet<&str>,
    ) -> rusqlite::Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT m.row_id, m.message_id, m.created_ms, m.observed_ms, m.home_scope, m.thread_id
             FROM message_scope s JOIN message m ON m.row_id = s.row_id
             WHERE s.chan = ?1 AND s.scope_key = ?2 AND s.created_ms >= ?3 AND s.created_ms < ?4",
        )?;
        let candidates = statement
            .query_map(params![chan_id, scope_key, from, to], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (row_id, message_id, created_ms, row_observed, home_scope, thread_id) in candidates {
            if seen.contains(message_id.as_str())
                || !merge::absent_is_deleted(row_observed, observed_ms, created_ms, to)
            {
                continue;
            }
            if home_key(&home_scope, thread_id.as_deref()) == scope_key {
                self.tombstone(chan_id, row_id, &message_id, created_ms, observed_ms)?;
            } else {
                self.conn.execute(
                    "DELETE FROM message_scope WHERE chan = ?1 AND scope_key = ?2 AND row_id = ?3",
                    params![chan_id, scope_key, row_id],
                )?;
            }
        }
        Ok(())
    }

    /// Delete a row and leave a tombstone of ids and times only.
    fn tombstone(
        &mut self,
        chan_id: i64,
        row_id: i64,
        message_id: &str,
        created_ms: i64,
        deleted_ms: i64,
    ) -> rusqlite::Result<()> {
        self.hot.row_delete.execute([row_id])?;
        self.unflushed += 1;
        self.hot
            .tomb_upsert
            .execute(params![chan_id, message_id, created_ms, deleted_ms])?;
        Stats::add(&self.shared.stats.tombstoned, 1);
        self.request_truncate();
        Ok(())
    }

    /// A deletion by id, from a push or a complete delta.
    fn delete_id(
        &mut self,
        chan_id: i64,
        id: &MessageId,
        created_hint: Option<i64>,
        observed_ms: i64,
    ) -> rusqlite::Result<()> {
        let held = self
            .hot
            .row_select
            .query_row(params![chan_id, id.as_str()], Row::from_sql)
            .optional()?;
        match held {
            // Content set by an observation that started after this deletion was seen stands.
            Some(row) if row.observed_ms > observed_ms => Ok(()),
            Some(row) => self.tombstone(
                chan_id,
                row.row_id,
                id.as_str(),
                row.created_ms,
                observed_ms,
            ),
            None => {
                let created = created_hint
                    .or_else(|| id.created_at_ms())
                    .unwrap_or(observed_ms);
                self.hot.tomb_upsert.execute(params![
                    chan_id,
                    id.as_str(),
                    created,
                    observed_ms
                ])?;
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert(
        &mut self,
        chan: &ChanKey,
        chan_id: i64,
        cutoff: i64,
        incoming: &Incoming,
        observed_ms: i64,
        arrival: Arrival,
        offered_partial: bool,
        listed_in: Option<&Scope>,
    ) -> rusqlite::Result<()> {
        let message = &incoming.message;
        let Some(created) = rows::created_ms(message) else {
            return Ok(());
        };
        if created < cutoff {
            Stats::add(&self.shared.stats.rows_too_old, 1);
            return Ok(());
        }
        let id = message.id.as_str();
        if let Some(deleted_ms) = self
            .hot
            .tomb_select
            .query_row(params![chan_id, id], |row| row.get::<_, i64>(0))
            .optional()?
        {
            if !merge::revives(deleted_ms, observed_ms) {
                Stats::add(&self.shared.stats.rows_held_by_tombstone, 1);
                return Ok(());
            }
            self.hot.tomb_delete.execute(params![chan_id, id])?;
        }
        let hash = rows::content_hash(&message.content);
        let offered_thread = match &incoming.home {
            Scope::Thread(thread) => Some(thread.clone()),
            _ => rows::thread_id(message),
        };
        let held = self
            .hot
            .row_select
            .query_row(params![chan_id, id], Row::from_sql)
            .optional()?;
        let (row_id, created) = match held {
            None => {
                let partial = merge::new_row_partial(arrival, offered_partial);
                let extra = serde_json::to_string(&Extra {
                    timestamp: message.timestamp.clone(),
                    thread: message.thread.clone(),
                    reactions: message.reactions.clone(),
                })
                .unwrap_or_else(|_| "{}".to_owned());
                let search_text = self.search_text(&chan.provider_key, message);
                self.hot.row_insert.execute(params![
                    chan_id,
                    id,
                    rows::numeric_id(message),
                    created,
                    incoming.edited_ms,
                    observed_ms,
                    incoming.home.kind(),
                    offered_thread,
                    message.thread.as_ref().is_some_and(|t| t.is_root),
                    message.reply_to.as_ref().map(|r| r.0.clone()),
                    message.author_id.as_str(),
                    message.author,
                    message.author_is_bot,
                    message.content,
                    hash,
                    partial,
                    search_text,
                    extra,
                ])?;
                Stats::add(&self.shared.stats.rows_written, 1);
                (self.conn.last_insert_rowid(), created)
            }
            Some(row) => {
                let (row_id, created) = (row.row_id, row.created_ms);
                self.update(
                    chan,
                    row,
                    incoming,
                    &hash,
                    offered_thread,
                    observed_ms,
                    arrival,
                    offered_partial,
                )?;
                (row_id, created)
            }
        };
        let mut memberships: Vec<String> = vec![incoming.home.key()];
        if let Some(scope) = listed_in {
            memberships.push(scope.key());
        }
        memberships.sort();
        memberships.dedup();
        for scope_key in memberships {
            self.hot
                .scope_insert
                .execute(params![chan_id, scope_key, created, row_id])?;
        }
        self.thread_from_row(chan_id, incoming, created, observed_ms)
    }

    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        chan: &ChanKey,
        held: Row,
        incoming: &Incoming,
        hash: &str,
        offered_thread: Option<String>,
        observed_ms: i64,
        arrival: Arrival,
        offered_partial: bool,
    ) -> rusqlite::Result<()> {
        let message = &incoming.message;
        let verdict = merge::content_verdict(
            &Held {
                content: &held.content,
                content_hash: &held.content_hash,
                observed_ms: held.observed_ms,
                edited_ms: held.edited_ms,
                partial: held.content_partial,
            },
            &Offered {
                content: &message.content,
                content_hash: hash,
                observed_ms,
                edited_ms: incoming.edited_ms,
                arrival,
                partial: offered_partial,
            },
        );
        let mut next = held.clone();
        match verdict {
            Verdict::Keep => {
                // A later push of the same text says the message was there, as it is, then: a
                // listing that started before it must not take its absence as a deletion, nor
                // bring back text from before it. A re-listing changes nothing, so that listing
                // an unchanged row again still writes nothing.
                if arrival.is_push() && hash == held.content_hash && observed_ms > held.observed_ms
                {
                    next.observed_ms = observed_ms;
                }
            }
            Verdict::Confirm => next.content_partial = false,
            Verdict::Replace { partial } => {
                next.content.clone_from(&message.content);
                hash.clone_into(&mut next.content_hash);
                next.content_partial = partial;
                next.observed_ms = observed_ms;
                next.edited_ms = later(held.edited_ms, incoming.edited_ms);
            }
        }
        // The rest of the message follows the newer observation; an older one changes nothing
        // but what only ever accumulates.
        if observed_ms >= held.observed_ms || matches!(verdict, Verdict::Replace { .. }) {
            let held_extra: Extra = serde_json::from_str(&held.extra).unwrap_or_default();
            next.author.clone_from(&message.author);
            message.author_id.0.clone_into(&mut next.author_id);
            next.author_is_bot = message.author_is_bot;
            if let Some(reply_to) = &message.reply_to {
                next.reply_to = Some(reply_to.0.clone());
            }
            next.is_root = message.thread.as_ref().map_or(held.is_root, |t| t.is_root);
            next.numeric_id = rows::numeric_id(message).or(held.numeric_id);
            next.extra = serde_json::to_string(&Extra {
                timestamp: message.timestamp.clone(),
                thread: message.thread.clone().or(held_extra.thread),
                // A copy that could not see reactions never erases ones a read brought.
                reactions: message.reactions.clone().or(held_extra.reactions),
            })
            .unwrap_or(held.extra.clone());
        }
        // Once a thread is known to have replies, it stays known.
        next.thread_id = held.thread_id.clone().or(offered_thread);
        if next.content != held.content || next.author != held.author {
            let rebuilt = rows::reconstruct(
                &chan.channel_id,
                message.id.as_str(),
                &next.author_id,
                &next.author,
                next.author_is_bot,
                next.reply_to.clone(),
                next.content.clone(),
                &next.extra,
            );
            next.search_text = self.search_text(&chan.provider_key, &rebuilt);
        }
        if next == held {
            Stats::add(&self.shared.stats.rows_unchanged, 1);
            return Ok(());
        }
        self.hot.row_update.execute(params![
            next.row_id,
            next.numeric_id,
            next.edited_ms,
            next.observed_ms,
            next.thread_id,
            next.is_root,
            next.reply_to,
            next.author_id,
            next.author,
            next.author_is_bot,
            next.content,
            next.content_hash,
            next.content_partial,
            next.extra,
        ])?;
        if next.search_text != held.search_text {
            // The old text leaves the index here, in place: an edit is a deletion of the words
            // it took away.
            self.hot
                .row_update_search
                .execute(params![next.row_id, next.search_text])?;
            self.unflushed += 1;
        }
        Stats::add(&self.shared.stats.rows_written, 1);
        Ok(())
    }

    fn search_text(&self, provider_key: &str, message: &crate::model::Message) -> String {
        let rendered = self.shared.search_text.rendered(provider_key, message);
        super::search_text::build(&message.author, &message.content, &rendered)
    }

    /// Thread records from a message: a root that has replies, or a reply.
    fn thread_from_row(
        &mut self,
        chan_id: i64,
        incoming: &Incoming,
        created: i64,
        observed_ms: i64,
    ) -> rusqlite::Result<()> {
        let message = &incoming.message;
        let record = match (&message.thread, &incoming.home) {
            (Some(thread), _) if thread.is_root => {
                if thread.reply_count.unwrap_or(0) == 0 {
                    return Ok(());
                }
                ThreadRecord {
                    thread_id: thread.id.clone(),
                    root_id: Some(message.id.clone()),
                    reply_count: thread.reply_count,
                    reply_count_exact: thread.reply_count_exact,
                    last_activity_ms: created,
                }
            }
            (Some(thread), _) => ThreadRecord {
                thread_id: thread.id.clone(),
                root_id: thread.root_message_id.clone(),
                reply_count: None,
                reply_count_exact: false,
                last_activity_ms: created,
            },
            (None, Scope::Thread(thread_id)) => ThreadRecord {
                thread_id: thread_id.clone(),
                root_id: None,
                reply_count: None,
                reply_count_exact: false,
                last_activity_ms: created,
            },
            (None, _) => return Ok(()),
        };
        self.upsert_thread(chan_id, &record, observed_ms)
    }

    fn upsert_thread(
        &mut self,
        chan_id: i64,
        record: &ThreadRecord,
        observed_ms: i64,
    ) -> rusqlite::Result<()> {
        self.hot.thread_upsert.execute(params![
            chan_id,
            record.thread_id,
            record.root_id.as_ref().map(|r| r.0.clone()),
            record
                .reply_count
                .map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            record.reply_count_exact,
            record.last_activity_ms,
            observed_ms,
        ])?;
        Ok(())
    }

    fn load_head(&self, chan_id: i64, scope_key: &str) -> rusqlite::Result<Option<Head>> {
        self.conn
            .query_row(
                "SELECT head_ms, unsettled_from_ms, head_listed_ms FROM scope_head
                 WHERE chan = ?1 AND scope_key = ?2",
                params![chan_id, scope_key],
                |row| {
                    Ok(Head {
                        head_ms: row.get(0)?,
                        unsettled_from_ms: row.get(1)?,
                        head_listed_ms: row.get(2)?,
                    })
                },
            )
            .optional()
    }

    fn store_head(&self, chan_id: i64, scope_key: &str, head: &Head) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO scope_head (chan, scope_key, head_ms, unsettled_from_ms, head_listed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (chan, scope_key) DO UPDATE SET head_ms = excluded.head_ms,
                unsettled_from_ms = excluded.unsettled_from_ms,
                head_listed_ms = excluded.head_listed_ms",
            params![
                chan_id,
                scope_key,
                head.head_ms,
                head.unsettled_from_ms,
                head.head_listed_ms
            ],
        )?;
        Ok(())
    }

    fn load_coverage(&self, chan_id: i64, scope_key: &str) -> rusqlite::Result<Vec<Interval>> {
        super::reader::coverage_by_id(self.conn, chan_id, scope_key)
    }

    fn store_coverage(
        &self,
        chan_id: i64,
        scope_key: &str,
        intervals: &[Interval],
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM coverage WHERE chan = ?1 AND scope_key = ?2",
            params![chan_id, scope_key],
        )?;
        let mut insert = self.conn.prepare(
            "INSERT INTO coverage (chan, scope_key, t0_ms, t1_ms, listed_ms, full_ms, complete, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for interval in intervals {
            insert.execute(params![
                chan_id,
                scope_key,
                interval.t0_ms,
                interval.t1_ms,
                interval.listed_ms,
                interval.full_ms,
                interval.complete,
                interval.source.as_str()
            ])?;
        }
        Ok(())
    }

    // ---- erase ----------------------------------------------------------------------------

    /// Erase channels, logically and at once: a channel that holds messages has its row renamed
    /// out of every reader's reach ([`ERASED_PROVIDER`]), and its rows are deleted by later
    /// steps; one that holds none is deleted outright, its tombstones, coverage and heads with
    /// it. A channel the file never held costs nothing.
    fn detach(&mut self, chans: &[ChanKey]) -> rusqlite::Result<()> {
        for chan in chans {
            let Some(id) = self.find_chan(chan)? else {
                continue;
            };
            let holds: bool = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM message WHERE chan = ?1)",
                [id],
                |row| row.get(0),
            )?;
            if holds {
                self.conn.execute(
                    "UPDATE channel SET provider_key = ?2, channel_id = 'erased:' || chan
                     WHERE chan = ?1",
                    params![id, ERASED_PROVIDER],
                )?;
                self.work.erase = true;
                self.request_truncate();
            } else {
                self.conn
                    .execute("DELETE FROM channel WHERE chan = ?1", [id])?;
            }
            Stats::add(&self.shared.stats.erased_channels, 1);
        }
        Ok(())
    }

    fn erase_provider(&mut self, provider_key: &str) -> rusqlite::Result<()> {
        let doomed: Vec<ChanKey> = self
            .channels()?
            .into_iter()
            .map(|(_, chan)| chan)
            .filter(|chan| chan.provider_key == provider_key)
            .collect();
        self.detach(&doomed)?;
        self.conn.execute(
            "DELETE FROM account WHERE provider_key = ?1",
            [provider_key],
        )?;
        Ok(())
    }

    fn retain_only(&mut self, routes: &RouteSnapshot) -> rusqlite::Result<()> {
        let doomed: Vec<ChanKey> = self
            .channels()?
            .into_iter()
            .map(|(_, chan)| chan)
            .filter(|chan| !routes.allows(chan))
            .collect();
        // Including channels this process never stamped: a fill that started before the sweep,
        // for a channel added back afterwards, is from before the erase.
        for chan in &doomed {
            self.shared.epochs.bump_channel(chan);
        }
        self.detach(&doomed)?;
        let accounts: Vec<String> = {
            let mut statement = self.conn.prepare("SELECT provider_key FROM account")?;
            let keys = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            keys
        };
        for key in accounts.iter().filter(|key| !routes.has_provider(key)) {
            self.conn
                .execute("DELETE FROM account WHERE provider_key = ?1", [key])?;
        }
        Ok(())
    }

    /// Every channel the file names, erased ones aside.
    fn channels(&self) -> rusqlite::Result<Vec<(i64, ChanKey)>> {
        let mut statement = self.conn.prepare(
            "SELECT chan, provider_key, channel_id FROM channel WHERE provider_key <> ?1",
        )?;
        let rows = statement
            .query_map([ERASED_PROVIDER], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    ChanKey::new(row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                ))
            })?
            .collect();
        rows
    }

    // ---- background steps -----------------------------------------------------------------

    /// The next background step that may run now, most urgent first: the owner's erases, then
    /// what the age limit and the size limit demand, then housekeeping.
    fn next_task(&self) -> Option<Task> {
        let w = &self.work;
        let now = Instant::now();
        let log = file::family(self.path)[1].clone();
        let merging_due = (w.merge || w.weekly_pages > 0)
            && w.compact.is_none()
            && self.since_merge >= MERGE_EVERY;
        let log_bytes = std::fs::metadata(&log).map_or(0, |m| m.len());
        let caught_up = self.checkpointer.is_none()
            || log_bytes <= super::checkpoint::PASSIVE_BYTES
            || self.shared.log_backlog.load(Ordering::SeqCst) <= SMALL_BACKLOG;
        let log_due = self.backfill_armed
            && ((log_bytes > WAL_HIGH_BYTES && caught_up) || log_bytes > WAL_CEILING_BYTES);
        // Deleted text in the log: truncated soon, but preferably once the checkpointer has
        // copied nearly everything, so that truncating has next to nothing to copy and sync.
        let truncate_due = w.checkpoint
            && w.checkpoint_after.is_none_or(|after| now >= after)
            && (caught_up
                || w.truncate_since
                    .is_some_and(|since| now >= since + TRUNCATE_PATIENCE));
        if log_due || truncate_due {
            Some(Task::Checkpoint)
        } else if merging_due {
            Some(Task::Merge)
        } else if w.erase {
            Some(Task::Erase)
        } else if w.tombstones.is_some() {
            Some(Task::Tombstones)
        } else if w.expire.is_some() {
            Some(Task::Expire)
        } else if w.evict.is_some() {
            Some(Task::Evict)
        } else if self.rebuilding() {
            Some(Task::Rebuild)
        } else if w.compact_due || w.compact.is_some() {
            Some(Task::Compact)
        } else if w.vacuum.is_some() {
            Some(Task::Vacuum)
        } else if w.merge || w.weekly_pages > 0 {
            Some(Task::Merge)
        } else if w
            .integrity_since
            .is_some_and(|since| self.last_batch.elapsed() >= IDLE_FOR || since.elapsed() >= HOURLY)
        {
            Some(Task::Integrity)
        } else {
            None
        }
    }

    /// Run one background step, if one may run now. Returns whether one ran.
    fn step(&mut self) -> bool {
        if self.stale() {
            self.work = Work::default();
            return false;
        }
        if self
            .backoff_until
            .is_some_and(|until| Instant::now() < until)
        {
            return false;
        }
        self.backoff_until = None;
        let Some(task) = self.next_task() else {
            return false;
        };
        let started = Instant::now();
        let deadline = started + STEP;
        self.unflushed = 0;
        let result = match task {
            Task::Erase => self.erase_step(deadline),
            Task::Tombstones => self.tombstone_step(),
            Task::Expire => self.expire_step(deadline),
            Task::Evict => self.evict_step(),
            Task::Rebuild => self.rebuild_step(),
            Task::Compact => self.compact_step(),
            Task::Vacuum => self.vacuum_step(),
            Task::Checkpoint => {
                self.checkpoint();
                Ok(())
            }
            Task::Merge => self.merge_step(),
            Task::Integrity => {
                self.integrity();
                Ok(())
            }
        };
        if result.is_ok() {
            self.failures = 0;
        }
        if let Err(error) = result {
            self.report(task.name(), Err(error));
            let rest = BACKOFF * (1_u32 << self.failures.min(BACKOFF_DOUBLINGS));
            self.backoff_until = Some(Instant::now() + rest);
            self.failures = self.failures.saturating_add(1);
            self.abandon(task);
        }
        let took = started.elapsed();
        Stats::add(&self.shared.stats.steps, 1);
        Stats::max(&self.shared.stats.step_max_us, micros(took));
        match task {
            Task::Erase | Task::Expire | Task::Evict => self.pace_markers.learn(took),
            Task::Rebuild => self.pace_rebuild.learn(took),
            _ => {}
        }
        if task == Task::Merge {
            self.since_merge = 0;
        } else {
            self.since_merge = self.since_merge.saturating_add(1);
        }
        if !matches!(task, Task::Checkpoint | Task::Integrity) {
            self.backfill_armed = true;
        }
        self.check_bytes();
        self.publish();
        true
    }

    /// Give up a step that failed. What the owner asked to forget is never given up: an erase, a
    /// compaction owed by markers, and a rebuild are retried after the backoff, the compaction
    /// from its start.
    fn abandon(&mut self, task: Task) {
        match task {
            Task::Erase | Task::Rebuild => {}
            Task::Tombstones => self.work.tombstones = None,
            Task::Expire => self.work.expire = None,
            Task::Evict => self.work.evict = None,
            Task::Compact => {
                self.work.compact = None;
                self.work.compact_due = true;
                let restored = self.transaction(|w| w.fts_option("usermerge", USERMERGE));
                self.report("compaction", restored);
            }
            Task::Vacuum => self.work.vacuum = None,
            Task::Checkpoint => self.work.checkpoint = false,
            Task::Merge => {
                self.work.merge = false;
                self.work.weekly_pages = 0;
            }
            Task::Integrity => self.work.integrity_since = None,
        }
    }

    /// Delete rows of erased channels until the step's time is up; once none are left, the
    /// channel rows themselves, whose cascades take tombstones, coverage, heads and thread
    /// records. A large erase deletes by marker and leaves a compaction owed; a small one takes
    /// each row out of the index in place.
    fn erase_step(&mut self, deadline: Instant) -> rusqlite::Result<()> {
        let markers = match self.work.erase_markers {
            Some(markers) => markers,
            None => {
                let rows: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM message
                     WHERE chan IN (SELECT chan FROM channel WHERE provider_key = ?1)",
                    [ERASED_PROVIDER],
                    |row| row.get(0),
                )?;
                let total: i64 =
                    self.conn
                        .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))?;
                let markers = Self::by_marker(rows, total);
                self.work.erase_markers = Some(markers);
                markers
            }
        };
        let pick = if markers { CHUNK_LIMIT } else { PICK };
        let finished = self.transaction(|w| {
            let picked: Vec<i64> = {
                let mut select = w.conn.prepare(
                    "SELECT row_id FROM message
                     WHERE chan IN (SELECT chan FROM channel WHERE provider_key = ?1) LIMIT ?2",
                )?;
                let picked = select
                    .query_map(params![ERASED_PROVIDER, pick], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                picked
            };
            if picked.is_empty() {
                w.conn.execute(
                    "DELETE FROM channel WHERE provider_key = ?1",
                    [ERASED_PROVIDER],
                )?;
                return Ok(true);
            }
            let deleted = w.delete_rows(&picked, markers, deadline)?;
            Stats::add(&w.shared.stats.erased_rows, deleted);
            Ok(false)
        })?;
        if finished {
            self.work.erase = false;
            self.work.erase_markers = None;
        }
        self.request_truncate();
        Ok(())
    }

    /// Delete `rows` inside the current transaction until `deadline`, at least one. By marker:
    /// FTS5 `secure-delete` off, the deletions, back on (which writes the markers out, cheaply),
    /// and the compaction owed recorded in the same transaction, so that a crash before it runs
    /// still leaves it owed. In place: each row's entries leave the index pages before the next
    /// row, so the clock sees what they cost. Returns how many went.
    fn delete_rows(
        &mut self,
        rows: &[i64],
        markers: bool,
        deadline: Instant,
    ) -> rusqlite::Result<u64> {
        if markers {
            self.fts_option("secure-delete", 0)?;
        }
        // By marker, the clock stops early: writing the markers out at the end takes the rest of
        // the step, by an amount the pace has learnt.
        let deadline = if markers {
            let started = deadline.checked_sub(STEP).unwrap_or_else(Instant::now);
            self.pace_markers.deadline(started)
        } else {
            deadline
        };
        let mut deleted = 0;
        for row_id in rows {
            self.hot.row_delete.execute([row_id])?;
            if !markers {
                self.hot.fts_flush.execute([])?;
            }
            deleted += 1;
            if Instant::now() >= deadline {
                break;
            }
        }
        if markers {
            self.fts_option("secure-delete", 1)?;
            self.conn.execute(
                "INSERT INTO meta (key, value) VALUES ('compaction_due', '1')
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [],
            )?;
            self.owe_compaction();
        }
        Ok(deleted)
    }

    fn tombstone_step(&mut self) -> rusqlite::Result<()> {
        let Some(before) = self.work.tombstones else {
            return Ok(());
        };
        let deleted = self.transaction(|w| {
            w.conn.execute(
                "DELETE FROM tombstone WHERE (chan, message_id) IN
                    (SELECT chan, message_id FROM tombstone WHERE deleted_ms < ?1 LIMIT ?2)",
                params![before, CHUNK_LIMIT],
            )
        })?;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < CHUNK_LIMIT {
            self.work.tombstones = None;
        }
        Ok(())
    }

    /// Plan the age and revalidation pass at `now`.
    fn expire_plan(&self, now: i64) -> rusqlite::Result<Expire> {
        let cutoffs: HashMap<i64, i64> = self
            .channels()?
            .into_iter()
            .map(|(id, chan)| (id, self.shared.limits.cutoff_ms(&chan.channel_id, now)))
            .collect();
        let max_cutoff = cutoffs.values().copied().max().unwrap_or(i64::MIN);
        // The revalidation phase stops at the newest row present when the pass began.
        let until: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(row_id), 0) FROM message", [], |row| {
                    row.get(0)
                })?;
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))?;
        Ok(Expire {
            cutoffs,
            max_cutoff,
            week_ago: now.saturating_sub(REVALIDATE_WITHIN_MS),
            until,
            total,
            deleted: 0,
            phase: Phase::Age {
                after: (i64::MIN, i64::MIN),
            },
        })
    }

    fn expire_step(&mut self, deadline: Instant) -> rusqlite::Result<()> {
        let Some(mut plan) = self.work.expire.take() else {
            return Ok(());
        };
        let done = self.transaction(|w| w.expire_in(&mut plan, deadline))?;
        if !done {
            self.work.expire = Some(plan);
        }
        self.request_truncate();
        Ok(())
    }

    /// One step of the pass. Returns whether the pass is over.
    fn expire_in(&mut self, plan: &mut Expire, deadline: Instant) -> rusqlite::Result<bool> {
        match plan.phase {
            Phase::Age { after } => {
                let picked: Vec<(i64, i64, i64)> = {
                    let mut select = self.conn.prepare(
                        "SELECT row_id, chan, created_ms FROM message
                         WHERE created_ms < ?1
                           AND (created_ms > ?2 OR (created_ms = ?2 AND row_id > ?3))
                         ORDER BY created_ms, row_id LIMIT ?4",
                    )?;
                    let picked = select
                        .query_map(params![plan.max_cutoff, after.0, after.1, PICK], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    picked
                };
                if picked.is_empty() {
                    plan.phase = Phase::Floors;
                    return Ok(false);
                }
                let last = picked
                    .last()
                    .map_or(after, |&(row_id, _, created)| (created, row_id));
                // An erased channel has no cutoff here: its rows go by the erase.
                let due: Vec<(i64, (i64, i64))> = picked
                    .into_iter()
                    .filter(|&(_, chan, created)| {
                        created < plan.cutoffs.get(&chan).copied().unwrap_or(i64::MIN)
                    })
                    .map(|(row_id, _, created)| (row_id, (created, row_id)))
                    .collect();
                let cursor = if due.is_empty() {
                    last
                } else {
                    let ids: Vec<i64> = due.iter().map(|&(row_id, _)| row_id).collect();
                    let markers = Self::by_marker(plan.deleted + count_i64(ids.len()), plan.total);
                    let deleted = self.delete_rows(&ids, markers, deadline)?;
                    plan.deleted += i64::try_from(deleted).unwrap_or(i64::MAX);
                    Stats::add(&self.shared.stats.expired_rows, deleted);
                    match usize::try_from(deleted).ok().filter(|&n| n < due.len()) {
                        Some(n) => due[n.saturating_sub(1)].1,
                        None => last,
                    }
                };
                plan.phase = Phase::Age { after: cursor };
                Ok(false)
            }
            Phase::Floors => {
                for (chan, cutoff) in &plan.cutoffs {
                    self.raise_floor(*chan, *cutoff)?;
                }
                plan.phase = Phase::Revalidate { after: 0 };
                Ok(false)
            }
            Phase::Revalidate { after } => {
                let until = plan.until;
                if after >= until {
                    return Ok(true);
                }
                let high = after.saturating_add(WINDOW).min(until);
                let picked: Vec<i64> = {
                    let mut select = self.conn.prepare(
                        "SELECT row_id FROM message
                         WHERE row_id > ?1 AND row_id <= ?2 AND observed_ms < ?3
                           AND NOT EXISTS (
                             SELECT 1 FROM message_scope s
                             JOIN coverage c ON c.chan = s.chan AND c.scope_key = s.scope_key
                             WHERE s.row_id = message.row_id
                               AND c.t0_ms <= message.created_ms AND message.created_ms < c.t1_ms
                               AND c.full_ms >= ?3)
                         ORDER BY row_id",
                    )?;
                    let picked = select
                        .query_map(params![after, high, plan.week_ago], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<i64>>>()?;
                    picked
                };
                let cursor = if picked.is_empty() {
                    high
                } else {
                    let markers =
                        Self::by_marker(plan.deleted + count_i64(picked.len()), plan.total);
                    let deleted = self.delete_rows(&picked, markers, deadline)?;
                    plan.deleted += i64::try_from(deleted).unwrap_or(i64::MAX);
                    Stats::add(&self.shared.stats.expired_rows, deleted);
                    match usize::try_from(deleted).ok().filter(|&n| n < picked.len()) {
                        Some(n) => picked[n.saturating_sub(1)],
                        None => high,
                    }
                };
                plan.phase = Phase::Revalidate { after: cursor };
                Ok(cursor >= until)
            }
        }
    }

    /// Coverage of a channel never reaches below `floor`: the rows there are gone.
    fn raise_floor(&self, chan_id: i64, floor: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM coverage WHERE chan = ?1 AND t1_ms <= ?2",
            params![chan_id, floor],
        )?;
        self.conn.execute(
            "UPDATE coverage SET t0_ms = ?2 WHERE chan = ?1 AND t0_ms < ?2 AND t1_ms > ?2",
            params![chan_id, floor],
        )?;
        Ok(())
    }

    /// The size limit. Above it, a round deletes the oldest rows across channels, as many as the
    /// excess over 90 % of the limit suggests; a compaction then gives their space back, and the
    /// next check measures again. A few rounds between hourly passes at most.
    fn check_bytes(&mut self) {
        let w = &self.work;
        if self.stale()
            || w.evict.is_some()
            || w.compact_due
            || w.compact.is_some()
            || w.vacuum.is_some()
        {
            return;
        }
        let max = self.shared.limits.max_bytes;
        if file::bytes_on_disk(self.path) <= max {
            self.work.rounds = 0;
            return;
        }
        // Rounds of anything, eviction or tidying, are counted: a file that cannot get under
        // its limit (a reader holding the log, a limit below the schema) must not keep the
        // writer measuring and checkpointing for ever. They start again with the hourly pass.
        if self.work.rounds >= EVICTION_ROUNDS {
            return;
        }
        self.work.rounds += 1;
        let target = max / 10 * 9;
        let measured = self.page_counted_bytes().and_then(|(used, free)| {
            let rows: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))?;
            Ok((used, free, u64::try_from(rows).unwrap_or(0)))
        });
        match measured {
            Ok((used, _, rows)) if used >= target && rows > 0 => {
                // The share of rows to go is the share of bytes over the target, plus one
                // percent.
                let share = (used - target).saturating_mul(rows) / used.max(1) + rows / 100 + 1;
                self.work.evict = Some(usize::try_from(share).unwrap_or(usize::MAX));
            }
            Ok((_, free, _)) => {
                // The pages in use fit: what is over is free pages, or the write-ahead log.
                if free > 0 {
                    self.work.vacuum = Some(None);
                }
                self.request_truncate();
            }
            Err(error) => self.report("size check", Err(error)),
        }
    }

    /// The size of the pages in use, and how many pages are free.
    fn page_counted_bytes(&self) -> rusqlite::Result<(u64, i64)> {
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((
            u64::try_from((pages - free).max(0) * size).unwrap_or(0),
            free,
        ))
    }

    /// One eviction step: the oldest rows, by marker, with coverage raised above them in the
    /// same transaction. FTS5 `secure-delete` is off only inside this transaction, and back on
    /// before it commits.
    fn evict_step(&mut self) -> rusqlite::Result<()> {
        let Some(left) = self.work.evict else {
            return Ok(());
        };
        let started = Instant::now();
        let take = left.min(DELETE_CHUNK);
        let deleted = self.transaction(|w| {
            let picked: Vec<(i64, i64, i64)> = {
                let mut select = w.conn.prepare(
                    "SELECT row_id, chan, created_ms FROM message
                     ORDER BY created_ms, row_id LIMIT ?1",
                )?;
                let picked = select
                    .query_map([i64::try_from(take).unwrap_or(i64::MAX)], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                picked
            };
            if picked.is_empty() {
                return Ok(0);
            }
            let ids: Vec<i64> = picked.iter().map(|&(row_id, _, _)| row_id).collect();
            let deleted = w.delete_rows(&ids, true, started + STEP)?;
            let deleted = usize::try_from(deleted)
                .unwrap_or(usize::MAX)
                .min(picked.len());
            let mut floors: HashMap<i64, i64> = HashMap::new();
            for &(_, chan, created) in &picked[..deleted] {
                let floor = floors.entry(chan).or_insert(created);
                *floor = (*floor).max(created);
            }
            for (chan, newest) in floors {
                w.raise_floor(chan, newest.saturating_add(1))?;
            }
            Ok(deleted)
        })?;
        Stats::add(&self.shared.stats.eviction_chunks, 1);
        Stats::max(
            &self.shared.stats.eviction_chunk_max_us,
            micros(started.elapsed()),
        );
        Stats::add(&self.shared.stats.evicted_rows, count(deleted));
        if deleted == 0 || deleted >= left {
            // The compaction its markers owe gives the space back; the next size check measures.
            self.work.evict = None;
        } else {
            self.work.evict = Some(left - deleted);
        }
        self.request_truncate();
        Ok(())
    }

    /// One step of rebuilding `search_text` with the current rendering rules, by marker inside
    /// the step's own transaction, as eviction does: the old text of a row being re-rendered is
    /// nothing anyone asked to forget, and a compaction follows to give the space back.
    fn rebuild_step(&mut self) -> rusqlite::Result<()> {
        let version = self.shared.search_text.version();
        let source = Arc::clone(&self.shared.search_text);
        let deadline = self.pace_rebuild.deadline(Instant::now());
        let (done, changed) = self.transaction(|w| {
            let after: i64 = w
                .conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'search_text_rebuild_after'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let rows: Vec<(i64, String, crate::model::Message, String)> = {
                let mut statement = w.conn.prepare(
                    "SELECT m.row_id, c.provider_key, c.channel_id, m.message_id, m.author_id,
                            m.author, m.author_is_bot, m.reply_to, m.content, m.extra,
                            m.search_text
                     FROM message m JOIN channel c ON c.chan = m.chan
                     WHERE m.row_id > ?1 ORDER BY m.row_id LIMIT ?2",
                )?;
                let rows = statement
                    .query_map(params![after, REBUILD_PICK], |row| {
                        let channel_id: String = row.get(2)?;
                        let message = rows::reconstruct(
                            &channel_id,
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                            &row.get::<_, String>(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            &row.get::<_, String>(9)?,
                        );
                        Ok((row.get(0)?, row.get(1)?, message, row.get(10)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            w.fts_option("secure-delete", 0)?;
            let mut last = after;
            let mut examined = 0;
            let mut changed = 0;
            for (row_id, provider_key, message, stored) in &rows {
                let rendered = source.rendered(provider_key, message);
                let text = super::search_text::build(&message.author, &message.content, &rendered);
                if &text != stored {
                    w.hot.row_update_search.execute(params![row_id, text])?;
                    changed += 1;
                }
                last = *row_id;
                examined += 1;
                if Instant::now() >= deadline {
                    break;
                }
            }
            w.fts_option("secure-delete", 1)?;
            if changed > 0 {
                w.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('compaction_due', '1')
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    [],
                )?;
                w.owe_compaction();
            }
            let reached_end =
                examined == rows.len() && i64::try_from(rows.len()).unwrap_or(0) < REBUILD_PICK;
            if reached_end {
                w.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('search_text_version', ?1)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    [&version],
                )?;
                w.conn.execute(
                    "DELETE FROM meta WHERE key = 'search_text_rebuild_after'",
                    [],
                )?;
            } else {
                w.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('search_text_rebuild_after', ?1)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    [last.to_string()],
                )?;
            }
            Ok((reached_end, changed))
        })?;
        let _ = changed;
        if done {
            self.shared
                .search_text_current
                .store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// One compaction step: rewrite the index without what markers cancel, in steps between
    /// batches, and know when it is really done.
    ///
    /// The first step gathers every segment into one merge (FTS5's `merge` with a negative page
    /// count), whose output is the only segment of a new last level: the kind of merge that
    /// drops deleted entries and their markers alike. The rest go on with it (a positive count),
    /// with `usermerge` lowered so that neither it nor a level of segments new since it began
    /// waits on the other. Batches keep writing segments meanwhile, so "a step found nothing to
    /// merge" says nothing; [`structure::merge_done`] reads the index's structure record after
    /// every step, in the step's own transaction, and the compaction is over when that merge is.
    /// If the structure cannot be read, or the merge is not of the kind that drops markers, or
    /// it has run four times longer than the index's size suggests, the rest is done in one go
    /// with `optimize`: slower, but never left undone.
    fn compact_step(&mut self) -> rusqlite::Result<()> {
        let mut compact = match self.work.compact.take() {
            Some(compact) => compact,
            None => {
                let pages: i64 =
                    self.conn
                        .query_row("SELECT COUNT(*) FROM message_fts_data", [], |row| {
                            row.get(0)
                        })?;
                self.transaction(|w| w.fts_option("usermerge", COMPACT_USERMERGE))?;
                self.work.compact_due = false;
                self.work.compact_again = false;
                let expected = pages / COMPACT_PAGES + 1;
                Compact {
                    started: Instant::now(),
                    output: None,
                    steps: 0,
                    cap: u32::try_from(expected.saturating_mul(4) + 100).unwrap_or(u32::MAX),
                }
            }
        };
        let slice_started = Instant::now();
        let output = compact.output;
        let progress = self.transaction(|w| {
            let pages = if output.is_some() {
                COMPACT_PAGES
            } else {
                -COMPACT_PAGES
            };
            w.conn.execute(
                "INSERT INTO message_fts (message_fts, rank) VALUES ('merge', ?1)",
                [pages],
            )?;
            let Some(now) = structure::read(w.conn)? else {
                return Ok(Progress::Unsure(
                    "the index's structure record did not parse",
                ));
            };
            Ok(match output {
                Some(output) if structure::merge_done(&now, output) => Progress::Done,
                Some(output) => Progress::Going(output),
                None => match now.sole_merge() {
                    Some(merge) if merge.oldest => Progress::Going(merge.output),
                    None if now.quiet() => Progress::Done,
                    _ => Progress::Unsure("the compaction's merge is not one that drops markers"),
                },
            })
        });
        let progress = match progress {
            Ok(progress) => progress,
            Err(error) => {
                // Abandoning it puts `usermerge` back; the debt stays recorded in the file.
                self.work.compact = Some(compact);
                return Err(error);
            }
        };
        compact.steps += 1;
        Stats::add(&self.shared.stats.compaction_steps, 1);
        Stats::max(
            &self.shared.stats.compaction_slice_max_us,
            micros(slice_started.elapsed()),
        );
        match progress {
            Progress::Going(output) if compact.steps < compact.cap => {
                compact.output = Some(output);
                self.work.compact = Some(compact);
                return Ok(());
            }
            Progress::Going(_) => self.optimize("the compaction ran past its expected length")?,
            Progress::Unsure(why) => self.optimize(why)?,
            Progress::Done => {}
        }
        self.transaction(|w| {
            w.fts_option("usermerge", USERMERGE)?;
            if !w.work.compact_again {
                w.conn
                    .execute("DELETE FROM meta WHERE key = 'compaction_due'", [])?;
            }
            Ok(())
        })?;
        if std::mem::take(&mut self.work.compact_again) {
            self.work.compact_due = true;
        }
        Stats::add(&self.shared.stats.compactions, 1);
        Stats::max(
            &self.shared.stats.compaction_max_us,
            micros(compact.started.elapsed()),
        );
        self.work.vacuum = Some(None);
        self.request_truncate();
        self.work.merge = true;
        Ok(())
    }

    /// Finish a compaction in one statement: FTS5's `optimize` merges every segment into one.
    fn optimize(&mut self, why: &str) -> rusqlite::Result<()> {
        tracing::warn!(
            reason = why,
            "the message cache compacts its index in one go; the writer pauses for it"
        );
        self.transaction(|w| {
            w.conn.execute(
                "INSERT INTO message_fts (message_fts) VALUES ('optimize')",
                [],
            )?;
            Ok(())
        })
    }

    fn vacuum_step(&mut self) -> rusqlite::Result<()> {
        let Some(budget) = self.work.vacuum else {
            return Ok(());
        };
        let pages = budget.map_or(VACUUM_PAGES, |left| left.min(VACUUM_PAGES));
        self.drain(&format!("PRAGMA incremental_vacuum({pages})"))?;
        let free: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let left = budget.map(|left| left - pages);
        if free == 0 || left.is_some_and(|left| left <= 0) {
            self.work.vacuum = None;
            self.request_truncate();
        } else {
            self.work.vacuum = Some(left);
        }
        Ok(())
    }

    /// One merge step of the index's segments, which FTS5 would otherwise do inside a batch's
    /// commit. Ends the merging when a step finds nothing to merge.
    fn merge_step(&mut self) -> rusqlite::Result<()> {
        let before = self.conn.total_changes();
        self.transaction(|w| {
            w.conn.execute(
                "INSERT INTO message_fts (message_fts, rank) VALUES ('merge', ?1)",
                [MERGE_PAGES],
            )?;
            Ok(())
        })?;
        Stats::add(&self.shared.stats.merge_steps, 1);
        let idle = self.conn.total_changes().saturating_sub(before) <= 1;
        if self.work.weekly_pages > 0 {
            self.work.weekly_pages -= MERGE_PAGES;
            if idle || self.work.weekly_pages <= 0 {
                self.work.weekly_pages = 0;
                let now = self.now();
                self.transaction(|w| {
                    w.conn.execute(
                        "INSERT INTO meta (key, value) VALUES ('last_merge_ms', ?1)
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                        [now.to_string()],
                    )?;
                    Ok(())
                })?;
            }
        }
        if idle {
            self.work.merge = false;
        }
        Ok(())
    }

    // ---- maintenance ----------------------------------------------------------------------

    /// What every new writer does first: make sure `secure-delete` is on and automatic merging
    /// off whatever an earlier build or a crash left, and find the work a crash interrupted.
    fn start(&mut self) {
        // The writer alone gets a larger page cache: it walks the index's segments on every
        // insert, merge and delete, and re-reading them from the file was a tenth of its time.
        let cache = self
            .conn
            .pragma_update(None, "cache_size", WRITER_CACHE_KIB.to_string());
        self.report("page cache", cache);
        if self.checkpointer.is_some() {
            // The checkpointer copies the log back, off this thread.
            let automatic = self.conn.pragma_update(None, "wal_autocheckpoint", 0);
            self.report("automatic checkpoint", automatic);
        }
        let options = self.transaction(|w| {
            w.fts_option("secure-delete", 1)?;
            w.fts_option("automerge", 0)?;
            w.fts_option("usermerge", USERMERGE)?;
            // Markers a crash (or an earlier build) left uncompacted: the debt is recorded with
            // them, and cleared only when a compaction has dropped them.
            let owed: bool = w.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM meta WHERE key = 'compaction_due')",
                [],
                |row| row.get(0),
            )?;
            w.work.compact_due = owed;
            Ok(())
        });
        self.report("index options", options);
        let erased = self
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM channel WHERE provider_key = ?1)",
                [ERASED_PROVIDER],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        self.work.erase = erased;
        self.work.merge = true;
        self.publish();
    }

    fn maintain(&mut self, kind: Maintenance) {
        match kind {
            Maintenance::Hourly => self.schedule_hourly(),
            Maintenance::Weekly => self.work.weekly_pages = WEEKLY_MERGE_PAGES,
            Maintenance::RebuildSearchText => self
                .shared
                .search_text_current
                .store(false, Ordering::SeqCst),
            Maintenance::Bounds => {
                self.work.rounds = 0;
                self.check_bytes();
            }
        }
    }

    fn report(&self, what: &str, result: rusqlite::Result<()>) {
        if let Err(error) = result {
            tracing::warn!(code = %sqlite_code(&error), step = what, "message cache maintenance failed");
        }
    }

    /// The hourly pass, as background work: expired tombstones, the age limit, rows not
    /// revalidated within a week, the integrity check (at a quiet moment), a bounded vacuum, a
    /// checkpoint, and the weekly merge when it is due.
    fn schedule_hourly(&mut self) {
        self.last_hourly = Instant::now();
        let now = self.now();
        self.work.tombstones = Some(now.saturating_sub(TOMBSTONE_KEEP_MS));
        match self.expire_plan(now) {
            Ok(plan) => self.work.expire = Some(plan),
            Err(error) => self.report("age limit", Err(error)),
        }
        self.work.integrity_since.get_or_insert_with(Instant::now);
        self.work.vacuum.get_or_insert(Some(HOURLY_VACUUM_PAGES));
        self.request_truncate();
        self.work.rounds = 0;
        let last_merge: i64 = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_merge_ms'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now.saturating_sub(last_merge) >= WEEKLY_MS {
            self.work.weekly_pages = WEEKLY_MERGE_PAGES;
        }
    }

    /// The FTS integrity check against the content table; on failure, a rebuild from it, which
    /// needs nothing from upstream.
    fn integrity(&mut self) {
        self.work.integrity_since = None;
        let checked = self.conn.execute(
            "INSERT INTO message_fts (message_fts, rank) VALUES ('integrity-check', 1)",
            [],
        );
        if let Err(error) = checked {
            Stats::add(&self.shared.stats.integrity_failures, 1);
            tracing::warn!(
                code = %sqlite_code(&error),
                "the message cache's full-text index failed its integrity check; rebuilding it"
            );
            let rebuilt = self.transaction(|w| {
                w.conn.execute(
                    "INSERT INTO message_fts (message_fts) VALUES ('rebuild')",
                    [],
                )?;
                Ok(())
            });
            self.report("index rebuild", rebuilt);
        }
    }

    /// Run a statement that may answer rows, and discard them.
    fn drain(&self, sql: &str) -> rusqlite::Result<()> {
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    }

    /// A checkpoint by the writer itself, between its steps: a `TRUNCATE` one when deleted text
    /// may be in the log and no read is running, a `PASSIVE` one otherwise. Neither waits: the
    /// busy timeout is zero for it, and a busy answer (a read in progress, or the checkpointer
    /// in the middle of its own) leaves it to be tried again shortly. With nothing else writing,
    /// it copies every frame the checkpointer has not, so the next write restarts the log.
    fn checkpoint(&mut self) {
        let started = Instant::now();
        self.backfill_armed = false;
        let truncate =
            self.work.checkpoint && self.shared.readers_in_use.load(Ordering::SeqCst) == 0;
        let mode = if truncate { "TRUNCATE" } else { "PASSIVE" };
        let _ = self.conn.pragma_update(None, "busy_timeout", 0);
        let answer = self
            .conn
            .query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
                row.get::<_, i64>(0)
            });
        let _ = self.conn.pragma_update(None, "busy_timeout", 2000);
        match answer {
            Ok(0) if truncate => {
                Stats::add(&self.shared.stats.checkpoints_truncated, 1);
                self.work.checkpoint = false;
                self.work.checkpoint_after = None;
                self.work.truncate_since = None;
            }
            _ => {
                self.work.checkpoint_after = Some(Instant::now() + CHECKPOINT_RETRY);
            }
        }
        Stats::max(
            &self.shared.stats.writer_checkpoint_max_us,
            micros(started.elapsed()),
        );
    }
}
