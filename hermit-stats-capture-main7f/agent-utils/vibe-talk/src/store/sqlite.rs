//! The shipped [`StateStore`]: one SQLite file, hand-written SQL, an explicit migration ladder.
//!
//! # Why SQLite and not a file format
//!
//! The thing a store has to survive is a power cut in the middle of a write. Append-only text
//! gets that half right — a torn append costs the last line — but it gets *deletion* wrong, and
//! this store deletes constantly: retention prunes on every append, and the operator's purge has
//! to be atomic or it is not a purge. SQLite gives a transaction, so a prune either happened or
//! did not, and gives it for one dependency.
//!
//! # Why not an ORM
//!
//! Four tables and about a dozen statements. An ORM would be bought with a large dependency tree
//! and paid for in indirection at every call site, and the portability it sells is already sold
//! by [`StateStore`] itself — the seam that lets a hosted backend replace this file lives one
//! level up, where it belongs.
//!
//! # Migrations
//!
//! [`MIGRATIONS`] is an append-only list, and SQLite's `user_version` pragma records how far a
//! given file has been taken. Opening an older file applies the missing steps in order; opening a
//! file from a *newer* server is refused rather than guessed at, because a schema this code does
//! not know is not a schema it can safely write to.
//!
//! # Blocking
//!
//! `rusqlite` is a blocking API. Every statement here runs inside [`tokio::task::spawn_blocking`]
//! so a slow disk cannot stall the executor that is also serving HTTP.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension as _};

use super::{
    now_ms, Bookmark, BookmarkAnchor, BookmarkChange, BookmarkPlace, CachedSummary, ChannelAlias,
    ConversationId, ConversationSummary, Pin, PinChange, PinSnapshot, Pins, ReadMark, RecordedTurn,
    Retention, Speaker, StateStore, StoreError, SummaryKey, TranscriptCursor, TranscriptPage, Turn,
    UpstreamClearAttempt, UpstreamClearMode, UpstreamClearResult, UpstreamClearSetting,
    MAX_PINS_PER_CHANNEL, MAX_TRANSCRIPT_PAGE, MAX_TURN_CHARS,
};
use crate::model::{ChannelId, MessageId, UserId};
use crate::voice_agent::{EditedPrompt, OwnerPrompt};

/// The schema, one step per released shape. **Append only** — editing a landed entry would leave
/// every existing file believing it has a schema it does not have.
pub const MIGRATIONS: &[&str] = &[
    // v1 — transcripts, and this server's own read marks.
    "
    CREATE TABLE conversations (
        id            TEXT    PRIMARY KEY NOT NULL,
        started_at_ms INTEGER NOT NULL,
        last_at_ms    INTEGER NOT NULL,
        preview       TEXT    NOT NULL
    ) STRICT;

    CREATE TABLE turns (
        conversation_id TEXT    NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        seq             INTEGER NOT NULL,
        speaker         TEXT    NOT NULL,
        text            TEXT    NOT NULL,
        at_ms           INTEGER NOT NULL,
        PRIMARY KEY (conversation_id, seq)
    ) STRICT;

    CREATE TABLE read_marks (
        channel_id           TEXT    PRIMARY KEY NOT NULL,
        last_read_message_id TEXT    NOT NULL,
        last_read_numeric    INTEGER NOT NULL,
        marked_at_ms         INTEGER NOT NULL
    ) STRICT;
    ",
    // v2 — cached summaries. `#49 cached-summaries`. Added as a SECOND step rather than folded
    // into v1 on purpose: the migration ladder is only real if it has been walked, and this is
    // the first file in the wild that will be upgraded rather than created.
    //
    // `version` is first in the primary key so the startup sweep — DELETE everything not under
    // the current policy — is one indexed range rather than a table scan.
    "
    CREATE TABLE summaries (
        version      TEXT    NOT NULL,
        channel_id   TEXT    NOT NULL,
        message_id   TEXT    NOT NULL,
        content_hash TEXT    NOT NULL,
        summary      TEXT    NOT NULL,
        made_at_ms   INTEGER NOT NULL,
        PRIMARY KEY (version, channel_id, message_id, content_hash)
    ) STRICT;
    ",
    // v3 — the per-message inbox overlay. `#50 todo-view`.
    //
    // This is the table `#61 unread-status` decided is OURS: Discord shares no read state with a
    // bot, so "I have dealt with this one" exists nowhere else and is authored here. It holds NO
    // message text — two snowflakes and an instant — which is why it is the one table in this
    // file that the age bound does not reach: an age limit would put a message the owner cleared
    // last month back in front of him for no reason he could see.
    //
    // `numeric` is the snowflake as an integer, so "everything through this message" is one
    // indexed range rather than a comparison this code would have to do in Rust over every row.
    // A snowflake that will not parse is refused at the door and never reaches this table.
    "
    CREATE TABLE dismissals (
        channel_id   TEXT    NOT NULL,
        message_id   TEXT    NOT NULL,
        numeric      INTEGER NOT NULL,
        at_ms        INTEGER NOT NULL,
        PRIMARY KEY (channel_id, message_id)
    ) STRICT;

    CREATE INDEX dismissals_by_position ON dismissals (channel_id, numeric);
    ",
    // v4 — the operator's own local names for channels. `#39 channel-alias`.
    //
    // One row per configured channel at most, which is why nothing in `Retention` reaches it: it
    // is bounded by the allowlist rather than by use, exactly as `read_marks` is. An age bound
    // would be worse than absent — it would rename a channel back without saying so.
    //
    // The alias is validated before it gets here (`super::validate_alias`), so this column holds
    // a trimmed, non-empty, control-character-free string.
    "
    CREATE TABLE channel_aliases (
        channel_id TEXT    PRIMARY KEY NOT NULL,
        alias      TEXT    NOT NULL,
        set_at_ms  INTEGER NOT NULL
    ) STRICT;
    ",
    // Channels the OWNER added from inside the app, as opposed to the ones in the configuration
    // file. Both end up in the same allowlist; only these can be removed again from the app,
    // because the others are a fact about a file this server does not write.
    //
    // `writable` is stored rather than assumed. Adding a channel the bridge may POST to is a
    // bigger act than adding one it may read, and which of the two was chosen has to survive a
    // restart — inferring it later would be inventing an answer to a question the owner already
    // answered.
    "
    CREATE TABLE added_channels (
        channel_id TEXT    PRIMARY KEY NOT NULL,
        label      TEXT    NOT NULL,
        writable   INTEGER NOT NULL,
        added_at_ms INTEGER NOT NULL
    ) STRICT;
    ",
    // v6 — which provider namespace owns an added channel's registration lifecycle.
    // Existing rows predate managed registration and therefore remain direct (`NULL`).
    "
    ALTER TABLE added_channels ADD COLUMN registration_provider TEXT DEFAULT NULL;
    ",
    // v7 — the order a reader scrolls the transcript in.
    //
    // `turns` is keyed `(conversation_id, seq)`, which answers "this conversation, in order" and
    // nothing else. Browsing the record asks the other question — "the newest forty turns, from
    // anywhere" — and without this index that is a scan of every turn ever stored followed by a
    // sort, once per step of the walk back.
    //
    // The columns are exactly `super::TranscriptCursor`'s three, in its order, so the cursored
    // read is one indexed range and not a filter over a scan. See `StateStore::transcript` for
    // why the triple rather than `at_ms` alone.
    "
    CREATE INDEX turns_by_time ON turns (at_ms, conversation_id, seq);
    ",
    // v8 — messages read automatically. `#196 auto-read-noise`.
    //
    // `noise_rules` is ONE row or none, and the difference is the point: no row means the owner
    // has never saved a list, so `noise::DEFAULT_RULES` stand in; a row holding `[]` means he
    // removed every rule on purpose. Nothing is inserted here, because seeding the default into
    // the file would make the two states indistinguishable from the first migration on. The list
    // is a JSON array of the rules as he typed them, already validated by
    // `noise::validate_rules`; it is read and written whole, never one rule at a time.
    //
    // `noise_exemptions` is the owner's "not noise" on a false positive: two ids and an instant,
    // no message text, bounded by the same count as `dismissals` for the same reasons. Unlike a
    // dismissal it does not need to be ordered, so there is no numeric column and an id that is
    // not a snowflake is fine.
    "
    CREATE TABLE noise_rules (
        id        INTEGER PRIMARY KEY CHECK (id = 1),
        rules     TEXT    NOT NULL,
        set_at_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE noise_exemptions (
        channel_id TEXT    NOT NULL,
        message_id TEXT    NOT NULL,
        at_ms      INTEGER NOT NULL,
        PRIMARY KEY (channel_id, message_id)
    ) STRICT;
    ",
    // v9 — configured channels the owner took off his list. `#199 removable-config-channels`.
    //
    // The configuration file stays the statement of which channels exist, and this server still
    // never writes it; a row here records only that the owner does not want one of them. That is
    // what makes a removal from the app outlast a restart without the file and the app disagreeing
    // about who owns the channel.
    //
    // Bounded by the configured channels, one row each at most, so `Retention` does not reach it.
    // The purge leaves it alone for the reason it leaves `added_channels` alone: these two tables
    // are the shape of the channel list, mirrored in memory, not a record of anything read or said.
    "
    CREATE TABLE hidden_channels (
        channel_id   TEXT    PRIMARY KEY NOT NULL,
        hidden_at_ms INTEGER NOT NULL
    ) STRICT;
    ",
    // v10 — the owner's own pinned messages. `#206 pin-message`.
    //
    // Two NEW tables and nothing else: no existing table is altered, no row is rewritten, and the
    // step runs in one transaction with its version stamp (see `migrate`), so a file in the wild
    // either is at v9 untouched or is at v10 with both tables empty.
    //
    // `pins` holds a bounded snapshot of each pinned message (`super::PinSnapshot`), because a pin
    // has to be drawable when its message is older than anything the page has loaded. That makes
    // it the second table here, after `summaries`, to hold other people's words — bounded by
    // `super::MAX_PINS_PER_CHANNEL` rows a channel and `super::MAX_PIN_TEXT_CHARS` a row, and
    // erased by the purge. `sent_at_ms` is `sent_at` parsed, for ordering; NULL when a provider's
    // timestamp does not parse, and such a row sorts by when it was pinned instead.
    //
    // `pin_revisions` is one counter per channel that ever had a pin, moved on every change to
    // that channel's pins, so a timeline read can say whether the list changed with one indexed
    // lookup instead of the page reading every snapshot on every refresh.
    "
    CREATE TABLE pins (
        channel_id    TEXT    NOT NULL,
        message_id    TEXT    NOT NULL,
        author        TEXT    NOT NULL,
        author_id     TEXT    NOT NULL,
        author_is_bot INTEGER NOT NULL,
        content       TEXT    NOT NULL,
        truncated     INTEGER NOT NULL,
        sent_at       TEXT    NOT NULL,
        sent_at_ms    INTEGER,
        thread_id     TEXT,
        thread_root   INTEGER NOT NULL,
        pinned_at_ms  INTEGER NOT NULL,
        PRIMARY KEY (channel_id, message_id)
    ) STRICT;

    CREATE TABLE pin_revisions (
        channel_id TEXT    PRIMARY KEY NOT NULL,
        revision   INTEGER NOT NULL
    ) STRICT;
    ",
    // v11 — clearing a channel's unread state at its chat provider. `#232 upstream-mark-read`.
    //
    // ONE new table and nothing else, for the reason v10 gives. One row per channel at most, and
    // only for a channel the owner has set, so `Retention` does not reach it; no row is `off`.
    //
    // `pushed_*` is the newest message the provider's cursor is known to cover because of this
    // server, and it outlives the mode being turned off: it is what stops a later push from moving
    // the provider's cursor backwards. `previous_cursor` is the provider's cursor from before the
    // first push, kept for a manual undo. `last_*` is the most recent attempt of any kind, for the
    // status line in Settings. No message text is kept, only ids and instants.
    "
    CREATE TABLE upstream_clear (
        channel_id         TEXT    PRIMARY KEY NOT NULL,
        mode               TEXT    NOT NULL CHECK (mode IN ('off', 'dry_run', 'on')),
        set_at_ms          INTEGER NOT NULL,
        enabled_at_ms      INTEGER,
        pushed_message_id  TEXT,
        pushed_message_ms  INTEGER,
        pushed_result      TEXT,
        pushed_target      TEXT,
        pushed_previous    TEXT,
        pushed_at_ms       INTEGER,
        previous_cursor    TEXT,
        last_message_id    TEXT,
        last_message_ms    INTEGER,
        last_result        TEXT,
        last_target        TEXT,
        last_previous      TEXT,
        last_error         TEXT,
        last_at_ms         INTEGER,
        failures           INTEGER NOT NULL DEFAULT 0
    ) STRICT;
    ",
    // v12 — the owner's reading bookmark, one per channel. `#240 reading-bookmark`.
    //
    // ONE new table and nothing else, for the reason v10 gives. One row per channel at most, and
    // only for a channel the owner has set one in, so `Retention` does not reach it: the allowlist
    // bounds it, as it bounds `read_marks`. A cleared bookmark KEEPS its row, with the anchor
    // columns NULL, so its revision goes on growing and a page holding an older one is refused;
    // only the purge deletes rows, and the next revision then starts from the clock
    // (`super::next_pin_revision`). Ids and an instant, and no message text.
    "
    CREATE TABLE bookmarks (
        channel_id TEXT    PRIMARY KEY NOT NULL,
        message_id TEXT,
        place      TEXT    CHECK (place IN ('above', 'below')),
        sent_at    TEXT,
        thread_id  TEXT,
        set_at_ms  INTEGER NOT NULL,
        revision   INTEGER NOT NULL,
        CHECK ((message_id IS NULL) = (place IS NULL)),
        CHECK ((message_id IS NULL) = (sent_at IS NULL)),
        CHECK (message_id IS NOT NULL OR thread_id IS NULL)
    ) STRICT;
    ",
    // v13 — whether the voice agent must ask before it sends. `#264 voice-send-approval`.
    //
    // ONE row or none, for the reason `noise_rules` gives: no row means the owner has never chosen,
    // so the configuration file's `[conversation] ask_before_send` stands in, and a row holding 1
    // or 0 is his own choice. Nothing is inserted here, because seeding the default would make
    // "never chosen" and "chose the default" indistinguishable. One boolean and an instant; no
    // text of any kind.
    "
    CREATE TABLE voice_settings (
        id              INTEGER PRIMARY KEY CHECK (id = 1),
        ask_before_send INTEGER NOT NULL CHECK (ask_before_send IN (0, 1)),
        set_at_ms       INTEGER NOT NULL
    ) STRICT;
    ",
    // v14 — the length a cached summary was cut at. `#208 persistent-cache`, milestone
    // `summary-keep`.
    //
    // ONE nullable column and nothing else: every existing row reads NULL, "whole", until the
    // first start re-keys the rows of the previous policy version and marks the ones it can see
    // were cut (`StateStore::rekey_summaries`). A summary is now filed uncut up to a generous
    // ceiling and cut when it is served, so a cap change re-derives nothing unless a stored answer
    // was cut shorter than the new cap; this column is how a stored answer says so. No text.
    //
    // This step's place in the ladder is all that numbers it: a step landed meanwhile goes before
    // it, and the tests find it by what it does rather than by its index.
    "
    ALTER TABLE summaries ADD COLUMN cut_at INTEGER;
    ",
    // v15 — the owner's own words for the voice agent. `#272 voice-agent-instructions`.
    //
    // ONE row or none, for the reason `voice_settings` gives: no row means he has never saved
    // anything, so the agent gets the shipped prompt. `instructions` is '' for none.
    // `system_prompt` is his edited copy of the base prompt, NULL for the shipped one, and
    // `edited_from` the prompt version that copy was made from, present exactly when the copy is.
    // His own text, which he wrote and nobody else's; no channel text is ever kept here.
    "
    CREATE TABLE voice_prompt (
        id            INTEGER PRIMARY KEY CHECK (id = 1),
        instructions  TEXT    NOT NULL,
        system_prompt TEXT,
        edited_from   INTEGER,
        set_at_ms     INTEGER NOT NULL,
        CHECK ((system_prompt IS NULL) = (edited_from IS NULL))
    ) STRICT;
    ",
];

/// A [`StateStore`] backed by one SQLite file.
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    retention: Retention,
    path: PathBuf,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Open — creating if absent — the database at `path`, and bring its schema up to date.
    ///
    /// The parent directory is created `0700` and the file is set to `0600`, because what lands
    /// in it is the owner's speech and other people's channel text. On a non-Unix host those
    /// calls do not exist and the modes are simply not applied; the deployment this project has
    /// is a Linux container.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] when the directory cannot be created, the file cannot be opened,
    /// or the schema cannot be applied — including the case where the file was written by a
    /// newer version of this server.
    pub fn open(path: &Path, retention: Retention) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }
        let connection = Connection::open(path).map_err(|error| {
            StoreError::Backend(format!("cannot open {}: {error}", path.display()))
        })?;
        restrict_file(path)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(backend)?;
        // What is deleted is overwritten, not left in free pages for anyone who reads the file:
        // the purge, a pruned summary, a hidden channel's summaries. Per connection, so on every
        // open; the bundled SQLite is not built with it on. `#208 persistent-cache`.
        connection
            .pragma_update(None, "secure_delete", "ON")
            .map_err(backend)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            retention,
            path: path.to_owned(),
        })
    }

    /// The file this store lives in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run one closure against the connection on the blocking pool.
    async fn with_connection<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let mut guard = connection.lock().unwrap_or_else(PoisonError::into_inner);
            work(&mut guard)
        })
        .await
        .map_err(|error| StoreError::Backend(format!("the storage task did not finish: {error}")))?
    }
}

/// A content hash as text.
///
/// TEXT rather than INTEGER because SQLite's integers are signed 64-bit and the hash is unsigned:
/// storing it as a number means a wrapping cast at both ends, which is one more place for the two
/// directions to disagree. Hexadecimal is also what a person greps for.
fn hex(content_hash: u64) -> String {
    format!("{content_hash:016x}")
}

fn backend(error: rusqlite::Error) -> StoreError {
    StoreError::Backend(error.to_string())
}

/// Apply every migration this file has not seen yet.
fn migrate(connection: &Connection) -> Result<(), StoreError> {
    migrate_through(connection, MIGRATIONS)
}

/// Apply every step of `steps` this file has not seen yet. [`migrate`] with the ladder as an
/// argument, so a test can walk one that fails partway.
fn migrate_through(connection: &Connection, steps: &[&str]) -> Result<(), StoreError> {
    let applied: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(backend)?;
    let applied = usize::try_from(applied)
        .map_err(|_| StoreError::Backend("the schema version is negative".to_owned()))?;
    if applied > steps.len() {
        // Refuse rather than guess. A file from a newer server has tables and columns this code
        // does not know about, and writing to it with the old statements is how a downgrade
        // silently corrupts data.
        return Err(StoreError::Backend(format!(
            "this database is at schema version {applied}, but this server only knows {}; it was \
             written by a newer vibe-talk and will not be downgraded",
            steps.len()
        )));
    }
    for (index, statement) in steps.iter().enumerate().skip(applied) {
        // ONE TRANSACTION PER STEP, its version stamp included. SQLite's DDL and `user_version`
        // are both transactional, so a step that fails — or a process that dies in the middle of
        // one — leaves the file exactly at the previous version rather than holding half a step's
        // tables under the old number, which the next start would then fail to re-create. That
        // matters most for the files already deployed, which are upgraded rather than created.
        let failed = |error: rusqlite::Error| {
            StoreError::Backend(format!("migration {} failed: {error}", index + 1))
        };
        let transaction = connection.unchecked_transaction().map_err(failed)?;
        transaction.execute_batch(statement).map_err(failed)?;
        transaction
            .pragma_update(None, "user_version", i64::try_from(index + 1).unwrap_or(0))
            .map_err(failed)?;
        transaction.commit().map_err(failed)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| StoreError::Backend(format!("cannot create {}: {error}", dir.display())))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir)
        .map_err(|error| StoreError::Backend(format!("cannot create {}: {error}", dir.display())))
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        StoreError::Backend(format!("cannot restrict {}: {error}", path.display()))
    })
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

/// Delete whatever retention no longer allows of the SUMMARY cache, inside a transaction.
///
/// Two bounds, and they collect different failures. The age limit is the only thing that ever
/// removes an ORPHAN — an entry whose message was edited or deleted upstream, which is
/// unreachable by key and which nothing will ever announce. The count limit is the ceiling: it
/// holds even against a caller that asks for a summary of a different message every second.
fn prune_summaries(
    transaction: &rusqlite::Transaction<'_>,
    retention: Retention,
) -> Result<(), StoreError> {
    // Counted from `made_at_ms`, which a write-scope hit moves forward (`touch_summary`): from the
    // last use, not from when the entry was made. `#208 persistent-cache`.
    if retention.summary_retain_days > 0 {
        let cutoff = now_ms() - i64::from(retention.summary_retain_days) * 86_400_000;
        transaction
            .execute("DELETE FROM summaries WHERE made_at_ms < ?1", [cutoff])
            .map_err(backend)?;
    }
    // The count bound, on each kind separately: ordinary summaries, and the entries kept across
    // policies (`super::KEPT_VERSION`, thread names). A flood of one never evicts the other.
    for statement in [
        "DELETE FROM summaries WHERE version <> ?2 AND rowid NOT IN (
             SELECT rowid FROM summaries WHERE version <> ?2
             ORDER BY made_at_ms DESC, rowid DESC LIMIT ?1
         )",
        "DELETE FROM summaries WHERE version = ?2 AND rowid NOT IN (
             SELECT rowid FROM summaries WHERE version = ?2
             ORDER BY made_at_ms DESC, rowid DESC LIMIT ?1
         )",
    ] {
        transaction
            .execute(
                statement,
                rusqlite::params![i64::from(retention.max_summaries), super::KEPT_VERSION],
            )
            .map_err(backend)?;
    }
    Ok(())
}

/// Keep the dismissal table under its ceiling, inside the caller's transaction.
///
/// ONE bound, not two, and the missing one is deliberate: see [`Retention::max_dismissals`]. The
/// oldest marks go first, which is the right end — the reader is never shown a message old enough
/// for its mark to have been evicted, because the window this filters is the recent one.
fn prune_dismissals(
    transaction: &rusqlite::Transaction<'_>,
    retention: Retention,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "DELETE FROM dismissals WHERE rowid NOT IN (
                 SELECT rowid FROM dismissals ORDER BY at_ms DESC, rowid DESC LIMIT ?1
             )",
            [i64::from(retention.max_dismissals)],
        )
        .map_err(backend)?;
    Ok(())
}

/// Keep the "not noise" table under its ceiling, inside the caller's transaction.
///
/// The dismissal bound, applied to a separate table: see [`Retention::max_dismissals`].
fn prune_noise_exemptions(
    transaction: &rusqlite::Transaction<'_>,
    retention: Retention,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "DELETE FROM noise_exemptions WHERE rowid NOT IN (
                 SELECT rowid FROM noise_exemptions ORDER BY at_ms DESC, rowid DESC LIMIT ?1
             )",
            [i64::from(retention.max_dismissals)],
        )
        .map_err(backend)?;
    Ok(())
}

/// Pair every id with its numeric snowflake, refusing the batch if any of them will not order.
///
/// All-or-nothing on purpose. A bulk dismissal that silently dropped the one id it could not read
/// would report a count that does not match what the reader saw, and the undo built from that
/// count would put back the wrong set.
fn orderable(messages: &[MessageId]) -> Result<Vec<(MessageId, i64)>, StoreError> {
    messages
        .iter()
        .map(|message| {
            let numeric = message
                .numeric()
                .and_then(|raw| i64::try_from(raw).ok())
                .ok_or_else(|| {
                    StoreError::BadId(format!(
                        "{message:?} is not a message snowflake, so this server cannot order it"
                    ))
                })?;
            Ok((message.clone(), numeric))
        })
        .collect()
}

/// Delete whatever retention no longer allows, inside the caller's transaction.
fn prune(transaction: &rusqlite::Transaction<'_>, retention: Retention) -> Result<(), StoreError> {
    if retention.retain_days > 0 {
        let cutoff = now_ms() - i64::from(retention.retain_days) * 86_400_000;
        transaction
            .execute("DELETE FROM conversations WHERE last_at_ms < ?1", [cutoff])
            .map_err(backend)?;
    }
    transaction
        .execute(
            "DELETE FROM conversations WHERE id NOT IN (
                 SELECT id FROM conversations ORDER BY last_at_ms DESC, id DESC LIMIT ?1
             )",
            [i64::from(retention.max_conversations)],
        )
        .map_err(backend)?;
    Ok(())
}

/// The revision this channel's pins are at, inside the caller's connection or transaction.
fn pin_revision(connection: &Connection, channel: &str) -> Result<i64, StoreError> {
    Ok(connection
        .query_row(
            "SELECT revision FROM pin_revisions WHERE channel_id = ?1",
            [channel],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(backend)?
        .unwrap_or(0))
}

/// Move this channel's pin revision on, inside the caller's transaction, and return it.
fn bump_pin_revision(
    transaction: &rusqlite::Transaction<'_>,
    channel: &str,
) -> Result<i64, StoreError> {
    let next = super::next_pin_revision(pin_revision(transaction, channel)?, now_ms());
    transaction
        .execute(
            "INSERT INTO pin_revisions (channel_id, revision) VALUES (?1, ?2)
             ON CONFLICT(channel_id) DO UPDATE SET revision = excluded.revision",
            rusqlite::params![channel, next],
        )
        .map_err(backend)?;
    Ok(next)
}

/// This channel's bookmark, inside the caller's connection or transaction. `#240 reading-bookmark`.
fn bookmark_row(connection: &Connection, channel: &str) -> Result<Bookmark, StoreError> {
    let row = connection
        .query_row(
            "SELECT message_id, place, sent_at, thread_id, revision FROM bookmarks
             WHERE channel_id = ?1",
            [channel],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(backend)?;
    let Some((message, place, sent_at, thread_id, revision)) = row else {
        return Ok(Bookmark::unset());
    };
    let anchor = match (message, place, sent_at) {
        (Some(message), Some(place), Some(sent_at)) => Some(BookmarkAnchor {
            message_id: MessageId(message),
            place: BookmarkPlace::parse(&place).ok_or_else(|| {
                StoreError::Backend(format!(
                    "a bookmark's place is {place:?}, which no version wrote"
                ))
            })?,
            timestamp: sent_at,
            thread_id,
        }),
        _ => None,
    };
    Ok(Bookmark { revision, anchor })
}

/// The columns of `upstream_clear`, in the order [`upstream_clear_row`] reads them.
const UPSTREAM_CLEAR_COLUMNS: &str = "channel_id, mode, set_at_ms, enabled_at_ms, \
     pushed_message_id, pushed_message_ms, pushed_result, pushed_target, pushed_previous, \
     pushed_at_ms, previous_cursor, last_message_id, last_message_ms, last_result, last_target, \
     last_previous, last_error, last_at_ms, failures";

/// One attempt out of five columns, or `None` when the row has none.
fn upstream_clear_attempt(
    row: &rusqlite::Row<'_>,
    first: usize,
    error: Option<String>,
) -> rusqlite::Result<Option<UpstreamClearAttempt>> {
    let message: Option<String> = row.get(first)?;
    let message_ms: Option<i64> = row.get(first + 1)?;
    let result: Option<String> = row.get(first + 2)?;
    let (Some(message), Some(message_ms), Some(result)) = (message, message_ms, result) else {
        return Ok(None);
    };
    let Some(result) = UpstreamClearResult::parse(&result) else {
        return Ok(None);
    };
    Ok(Some(UpstreamClearAttempt {
        at_ms: 0,
        message: MessageId(message),
        message_ms,
        result,
        target: row.get(first + 3)?,
        previous: row.get(first + 4)?,
        error,
    }))
}

/// One `upstream_clear` row, read in [`UPSTREAM_CLEAR_COLUMNS`] order.
fn upstream_clear_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UpstreamClearSetting> {
    let mode: String = row.get(1)?;
    let pushed = upstream_clear_attempt(row, 4, None)?.map(|attempt| UpstreamClearAttempt {
        at_ms: row.get::<_, Option<i64>>(9).ok().flatten().unwrap_or(0),
        ..attempt
    });
    let last_error: Option<String> = row.get(16)?;
    let last = upstream_clear_attempt(row, 11, last_error)?.map(|attempt| UpstreamClearAttempt {
        at_ms: row.get::<_, Option<i64>>(17).ok().flatten().unwrap_or(0),
        ..attempt
    });
    Ok(UpstreamClearSetting {
        channel: ChannelId(row.get(0)?),
        // The CHECK constraint admits nothing else; an unknown spelling reads as off, the safe
        // answer, rather than failing every read of every channel's setting.
        mode: UpstreamClearMode::parse(&mode).unwrap_or_default(),
        set_at_ms: row.get(2)?,
        enabled_at_ms: row.get(3)?,
        pushed,
        previous_cursor: row.get(10)?,
        last,
        failures: u32::try_from(row.get::<_, i64>(18)?).unwrap_or(u32::MAX),
    })
}

/// One channel's setting inside the caller's connection or transaction, or `None` without a row.
fn upstream_clear_setting(
    connection: &Connection,
    channel: &str,
) -> Result<Option<UpstreamClearSetting>, StoreError> {
    connection
        .query_row(
            &format!("SELECT {UPSTREAM_CLEAR_COLUMNS} FROM upstream_clear WHERE channel_id = ?1"),
            [channel],
            upstream_clear_row,
        )
        .optional()
        .map_err(backend)
}

/// Write one channel's whole setting, inside the caller's transaction.
fn write_upstream_clear(
    transaction: &rusqlite::Transaction<'_>,
    setting: &UpstreamClearSetting,
) -> Result<(), StoreError> {
    let pushed = setting.pushed.as_ref();
    let last = setting.last.as_ref();
    transaction
        .execute(
            &format!(
                "INSERT OR REPLACE INTO upstream_clear ({UPSTREAM_CLEAR_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19)"
            ),
            rusqlite::params![
                setting.channel.as_str(),
                setting.mode.as_str(),
                setting.set_at_ms,
                setting.enabled_at_ms,
                pushed.map(|attempt| attempt.message.as_str()),
                pushed.map(|attempt| attempt.message_ms),
                pushed.map(|attempt| attempt.result.as_str()),
                pushed.and_then(|attempt| attempt.target.as_deref()),
                pushed.and_then(|attempt| attempt.previous.as_deref()),
                pushed.map(|attempt| attempt.at_ms),
                setting.previous_cursor.as_deref(),
                last.map(|attempt| attempt.message.as_str()),
                last.map(|attempt| attempt.message_ms),
                last.map(|attempt| attempt.result.as_str()),
                last.and_then(|attempt| attempt.target.as_deref()),
                last.and_then(|attempt| attempt.previous.as_deref()),
                last.and_then(|attempt| attempt.error.as_deref()),
                last.map(|attempt| attempt.at_ms),
                i64::from(setting.failures),
            ],
        )
        .map_err(backend)?;
    Ok(())
}

#[async_trait]
impl StateStore for SqliteStore {
    fn describe(&self) -> String {
        format!("SQLite at {}", self.path.display())
    }

    async fn check_writable(&self) -> Result<String, StoreError> {
        let path = self.path.clone();
        self.with_connection(move |connection| {
            // `BEGIN IMMEDIATE`, not `BEGIN`. A deferred transaction takes no lock until the
            // first statement, so it would succeed against a read-only file and prove precisely
            // nothing; an immediate one takes the RESERVED lock up front and fails with
            // SQLITE_READONLY when the file, the directory, or the filesystem will not have it.
            //
            // Dropping it rolls back. That is the whole design: this is reached from a read-scope
            // route, and it must leave the database byte-for-byte as it found it.
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(backend)?;
            transaction.rollback().map_err(backend)?;
            Ok(format!(
                "the SQLite file at {} took a write lock and released it; nothing was written",
                path.display()
            ))
        })
        .await
    }

    async fn append_turn(
        &self,
        conversation: &ConversationId,
        turn: &Turn,
    ) -> Result<(), StoreError> {
        if turn.text.chars().count() > MAX_TURN_CHARS {
            return Err(StoreError::TooLarge(format!(
                "one turn may be at most {MAX_TURN_CHARS} characters"
            )));
        }
        let id = conversation.as_str().to_owned();
        let turn = turn.clone();
        let retention = self.retention;
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let held: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM turns WHERE conversation_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(backend)?;
            if held >= i64::from(retention.max_turns_per_conversation) {
                return Err(StoreError::TooLarge(format!(
                    "this conversation already holds its limit of {} turns",
                    retention.max_turns_per_conversation
                )));
            }
            transaction
                .execute(
                    "INSERT INTO conversations (id, started_at_ms, last_at_ms, preview)
                     VALUES (?1, ?2, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET last_at_ms = ?2",
                    rusqlite::params![&id, turn.at_ms, super::preview_of(&turn.text)],
                )
                .map_err(backend)?;
            transaction
                .execute(
                    "INSERT INTO turns (conversation_id, seq, speaker, text, at_ms)
                     VALUES (
                        ?1,
                        (SELECT IFNULL(MAX(seq), 0) + 1 FROM turns WHERE conversation_id = ?1),
                        ?2, ?3, ?4
                     )",
                    rusqlite::params![&id, turn.speaker.as_str(), &turn.text, turn.at_ms],
                )
                .map_err(backend)?;
            prune(&transaction, retention)?;
            transaction.commit().map_err(backend)
        })
        .await
    }

    async fn conversations(&self) -> Result<Vec<ConversationSummary>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT c.id, c.started_at_ms, c.last_at_ms, c.preview,
                            (SELECT COUNT(*) FROM turns t WHERE t.conversation_id = c.id)
                     FROM conversations c
                     ORDER BY c.last_at_ms DESC, c.id DESC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(backend)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, started_at_ms, last_at_ms, preview, turns) = row.map_err(backend)?;
                out.push(ConversationSummary {
                    id: ConversationId::parse(&id)?,
                    started_at_ms,
                    last_at_ms,
                    preview,
                    turns: u32::try_from(turns).unwrap_or(u32::MAX),
                });
            }
            Ok(out)
        })
        .await
    }

    async fn turns(&self, conversation: &ConversationId) -> Result<Vec<Turn>, StoreError> {
        let id = conversation.as_str().to_owned();
        self.with_connection(move |connection| {
            let exists: Option<i64> = connection
                .query_row("SELECT 1 FROM conversations WHERE id = ?1", [&id], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(backend)?;
            if exists.is_none() {
                return Err(StoreError::NotFound);
            }
            let mut statement = connection
                .prepare(
                    "SELECT speaker, text, at_ms FROM turns
                     WHERE conversation_id = ?1 ORDER BY seq ASC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([&id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(backend)?;
            let mut out = Vec::new();
            for row in rows {
                let (speaker, text, at_ms) = row.map_err(backend)?;
                out.push(Turn {
                    speaker: Speaker::parse(&speaker)?,
                    text,
                    at_ms,
                });
            }
            Ok(out)
        })
        .await
    }

    async fn transcript(
        &self,
        limit: u16,
        before: Option<&TranscriptCursor>,
    ) -> Result<TranscriptPage, StoreError> {
        let limit = limit.clamp(1, MAX_TRANSCRIPT_PAGE);
        let before = before.cloned();
        self.with_connection(move |connection| {
            // ONE MORE THAN ASKED FOR. That extra row is how "is anything older still held" is
            // answered without a second statement: a COUNT would be a scan of the whole table on
            // every step of the walk, and a second cursored read would be a second round trip
            // that could disagree with the first if a turn landed between them.
            let wanted = i64::from(limit) + 1;
            let mut rows = Vec::new();
            {
                // Row-value comparison rather than the unrolled
                // `at_ms < ?  OR (at_ms = ? AND ...)` form. It is the same predicate, and SQLite
                // drives `turns_by_time` with it directly — the unrolled one is three disjuncts
                // that have to be proved equivalent to the index order by hand every time this is
                // read.
                let sql = if before.is_some() {
                    "SELECT conversation_id, seq, speaker, text, at_ms FROM turns
                     WHERE (at_ms, conversation_id, seq) < (?2, ?3, ?4)
                     ORDER BY at_ms DESC, conversation_id DESC, seq DESC
                     LIMIT ?1"
                } else {
                    "SELECT conversation_id, seq, speaker, text, at_ms FROM turns
                     ORDER BY at_ms DESC, conversation_id DESC, seq DESC
                     LIMIT ?1"
                };
                let mut statement = connection.prepare(sql).map_err(backend)?;
                let read = |row: &rusqlite::Row<'_>| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                };
                let mapped = match &before {
                    Some(cursor) => statement.query_map(
                        rusqlite::params![
                            wanted,
                            cursor.at_ms,
                            cursor.conversation.as_str(),
                            cursor.seq
                        ],
                        read,
                    ),
                    None => statement.query_map(rusqlite::params![wanted], read),
                }
                .map_err(backend)?;
                for row in mapped {
                    let (conversation, seq, speaker, text, at_ms) = row.map_err(backend)?;
                    rows.push(RecordedTurn {
                        conversation: ConversationId::parse(&conversation)?,
                        seq,
                        turn: Turn {
                            speaker: Speaker::parse(&speaker)?,
                            text,
                            at_ms,
                        },
                    });
                }
            }
            let has_more = rows.len() > usize::from(limit);
            rows.truncate(usize::from(limit));
            let next = rows.last().map(|last| TranscriptCursor {
                at_ms: last.turn.at_ms,
                conversation: last.conversation.clone(),
                seq: last.seq,
            });
            Ok(TranscriptPage {
                turns: rows,
                has_more,
                next,
            })
        })
        .await
    }

    async fn forget_conversation(&self, conversation: &ConversationId) -> Result<(), StoreError> {
        let id = conversation.as_str().to_owned();
        self.with_connection(move |connection| {
            let removed = connection
                .execute("DELETE FROM conversations WHERE id = ?1", [&id])
                .map_err(backend)?;
            if removed == 0 {
                return Err(StoreError::NotFound);
            }
            Ok(())
        })
        .await
    }

    async fn forget_all_conversations(&self) -> Result<u64, StoreError> {
        self.with_connection(|connection| {
            let removed = connection
                .execute("DELETE FROM conversations", [])
                .map_err(backend)?;
            Ok(removed as u64)
        })
        .await
    }

    async fn read_mark(&self, channel: &ChannelId) -> Result<Option<ReadMark>, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT last_read_message_id, marked_at_ms FROM read_marks
                     WHERE channel_id = ?1",
                    [channel.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(backend)
                .map(|found| {
                    found.map(|(last_read, marked_at_ms)| ReadMark {
                        channel,
                        last_read: MessageId(last_read),
                        marked_at_ms,
                    })
                })
        })
        .await
    }

    async fn read_marks(&self) -> Result<Vec<ReadMark>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT channel_id, last_read_message_id, marked_at_ms FROM read_marks
                     ORDER BY channel_id ASC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ReadMark {
                        channel: ChannelId(row.get::<_, String>(0)?),
                        last_read: MessageId(row.get::<_, String>(1)?),
                        marked_at_ms: row.get::<_, i64>(2)?,
                    })
                })
                .map_err(backend)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(backend)
        })
        .await
    }

    async fn mark_read(
        &self,
        channel: &ChannelId,
        upto: &MessageId,
    ) -> Result<ReadMark, StoreError> {
        let numeric = upto.numeric().ok_or_else(|| {
            StoreError::BadId(format!(
                "{upto:?} is not a message snowflake, so this server cannot order it"
            ))
        })?;
        let numeric = i64::try_from(numeric)
            .map_err(|_| StoreError::BadId("that snowflake is out of range".to_owned()))?;
        let channel = channel.clone();
        let upto = upto.clone();
        self.with_connection(move |connection| {
            let at = now_ms();
            // Monotonic: `WHERE excluded.last_read_numeric > last_read_numeric` keeps a stale
            // client from dragging the mark backwards and re-unreading messages the owner has
            // already been shown.
            connection
                .execute(
                    "INSERT INTO read_marks
                        (channel_id, last_read_message_id, last_read_numeric, marked_at_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(channel_id) DO UPDATE SET
                        last_read_message_id = excluded.last_read_message_id,
                        last_read_numeric    = excluded.last_read_numeric,
                        marked_at_ms         = excluded.marked_at_ms
                     WHERE excluded.last_read_numeric > read_marks.last_read_numeric",
                    rusqlite::params![channel.as_str(), upto.as_str(), numeric, at],
                )
                .map_err(backend)?;
            connection
                .query_row(
                    "SELECT last_read_message_id, marked_at_ms FROM read_marks
                     WHERE channel_id = ?1",
                    [channel.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .map_err(backend)
                .map(|(last_read, marked_at_ms)| ReadMark {
                    channel,
                    last_read: MessageId(last_read),
                    marked_at_ms,
                })
        })
        .await
    }

    async fn forget_read_mark(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let removed = connection
                .execute(
                    "DELETE FROM read_marks WHERE channel_id = ?1",
                    [channel.as_str()],
                )
                .map_err(backend)?;
            if removed == 0 {
                return Err(StoreError::NotFound);
            }
            Ok(())
        })
        .await
    }

    async fn channel_aliases(&self) -> Result<Vec<ChannelAlias>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT channel_id, alias, set_at_ms FROM channel_aliases
                     ORDER BY channel_id ASC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ChannelAlias {
                        channel: ChannelId(row.get::<_, String>(0)?),
                        alias: row.get::<_, String>(1)?,
                        set_at_ms: row.get::<_, i64>(2)?,
                    })
                })
                .map_err(backend)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(backend)
        })
        .await
    }

    async fn added_channels(&self) -> Result<Vec<crate::store::AddedChannel>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT channel_id, label, writable, registration_provider, added_at_ms
                     FROM added_channels ORDER BY added_at_ms ASC, channel_id ASC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(crate::store::AddedChannel {
                        channel: ChannelId(row.get::<_, String>(0)?),
                        label: row.get::<_, String>(1)?,
                        writable: row.get::<_, i64>(2)? != 0,
                        registration_provider: row.get::<_, Option<String>>(3)?,
                        added_at_ms: row.get::<_, i64>(4)?,
                    })
                })
                .map_err(backend)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(backend)
        })
        .await
    }

    async fn add_channel(
        &self,
        channel: &ChannelId,
        label: &str,
        writable: bool,
        registration_provider: Option<&str>,
        at_ms: i64,
    ) -> Result<(), StoreError> {
        let channel = channel.0.clone();
        let label = label.to_owned();
        let registration_provider = registration_provider.map(str::to_owned);
        self.with_connection(move |connection| {
            connection
                .execute(
                    "INSERT INTO added_channels
                        (channel_id, label, writable, registration_provider, added_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(channel_id) DO UPDATE SET
                        label = ?2, writable = ?3, registration_provider = ?4",
                    rusqlite::params![
                        channel,
                        label,
                        i64::from(writable),
                        registration_provider,
                        at_ms
                    ],
                )
                .map_err(backend)?;
            Ok(())
        })
        .await
    }

    async fn remove_added_channel(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let channel = channel.0.clone();
        self.with_connection(move |connection| {
            connection
                .execute(
                    "DELETE FROM added_channels WHERE channel_id = ?1",
                    rusqlite::params![channel],
                )
                .map_err(backend)?;
            Ok(())
        })
        .await
    }

    async fn hidden_channels(&self) -> Result<Vec<ChannelId>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT channel_id FROM hidden_channels ORDER BY channel_id ASC")
                .map_err(backend)?;
            let rows = statement
                .query_map([], |row| Ok(ChannelId(row.get::<_, String>(0)?)))
                .map_err(backend)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(backend)
        })
        .await
    }

    async fn hide_channel(&self, channel: &ChannelId, at_ms: i64) -> Result<(), StoreError> {
        let channel = channel.0.clone();
        self.with_connection(move |connection| {
            connection
                .execute(
                    "INSERT INTO hidden_channels (channel_id, hidden_at_ms) VALUES (?1, ?2)
                     ON CONFLICT(channel_id) DO NOTHING",
                    rusqlite::params![channel, at_ms],
                )
                .map_err(backend)?;
            Ok(())
        })
        .await
    }

    async fn unhide_channel(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let channel = channel.0.clone();
        self.with_connection(move |connection| {
            connection
                .execute(
                    "DELETE FROM hidden_channels WHERE channel_id = ?1",
                    rusqlite::params![channel],
                )
                .map_err(backend)?;
            Ok(())
        })
        .await
    }

    async fn set_channel_alias(
        &self,
        channel: &ChannelId,
        alias: &str,
    ) -> Result<ChannelAlias, StoreError> {
        let alias = super::validate_alias(alias)?;
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let at = now_ms();
            connection
                .execute(
                    "INSERT INTO channel_aliases (channel_id, alias, set_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(channel_id) DO UPDATE SET
                        alias     = excluded.alias,
                        set_at_ms = excluded.set_at_ms",
                    rusqlite::params![channel.as_str(), &alias, at],
                )
                .map_err(backend)?;
            Ok(ChannelAlias {
                channel,
                alias,
                set_at_ms: at,
            })
        })
        .await
    }

    async fn clear_channel_alias(&self, channel: &ChannelId) -> Result<(), StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let removed = connection
                .execute(
                    "DELETE FROM channel_aliases WHERE channel_id = ?1",
                    [channel.as_str()],
                )
                .map_err(backend)?;
            if removed == 0 {
                return Err(StoreError::NotFound);
            }
            Ok(())
        })
        .await
    }

    async fn dismissals(&self, channel: &ChannelId) -> Result<Vec<MessageId>, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT message_id FROM dismissals WHERE channel_id = ?1
                     ORDER BY numeric DESC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([channel.as_str()], |row| row.get::<_, String>(0))
                .map_err(backend)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(MessageId(row.map_err(backend)?));
            }
            Ok(out)
        })
        .await
    }

    async fn dismiss(
        &self,
        channel: &ChannelId,
        messages: &[MessageId],
    ) -> Result<u64, StoreError> {
        let channel = channel.clone();
        let ordered = orderable(messages)?;
        let retention = self.retention;
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let at = now_ms();
            let mut added = 0_u64;
            for (message, numeric) in &ordered {
                // DO NOTHING, not DO UPDATE. Dismissing something twice must not move it, or the
                // count below stops meaning "how many the reader actually cleared" and the
                // retention queue would reorder itself under a repeated tap.
                added += transaction
                    .execute(
                        "INSERT INTO dismissals (channel_id, message_id, numeric, at_ms)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(channel_id, message_id) DO NOTHING",
                        rusqlite::params![channel.as_str(), message.as_str(), numeric, at],
                    )
                    .map_err(backend)? as u64;
            }
            prune_dismissals(&transaction, retention)?;
            transaction.commit().map_err(backend)?;
            Ok(added)
        })
        .await
    }

    async fn restore(
        &self,
        channel: &ChannelId,
        messages: &[MessageId],
    ) -> Result<u64, StoreError> {
        let channel = channel.clone();
        let messages: Vec<MessageId> = messages.to_vec();
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let mut removed = 0_u64;
            for message in &messages {
                removed += transaction
                    .execute(
                        "DELETE FROM dismissals WHERE channel_id = ?1 AND message_id = ?2",
                        rusqlite::params![channel.as_str(), message.as_str()],
                    )
                    .map_err(backend)? as u64;
            }
            transaction.commit().map_err(backend)?;
            Ok(removed)
        })
        .await
    }

    async fn noise_rules(&self) -> Result<Option<Vec<String>>, StoreError> {
        self.with_connection(|connection| {
            let stored = connection
                .query_row("SELECT rules FROM noise_rules WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
                .map_err(backend)?;
            stored
                .map(|text| {
                    // A row this server cannot read is a refusal, not "no rules": treating it as
                    // empty would silently turn the feature off, and treating it as the defaults
                    // would silently undo whatever the owner had saved.
                    serde_json::from_str::<Vec<String>>(&text).map_err(|error| {
                        StoreError::Backend(format!(
                            "the stored noise rules are unreadable: {error}"
                        ))
                    })
                })
                .transpose()
        })
        .await
    }

    async fn set_noise_rules(&self, rules: &[String]) -> Result<Vec<String>, StoreError> {
        let rules = crate::noise::validate_rules(rules)?;
        let encoded = serde_json::to_string(&rules).map_err(|error| {
            StoreError::Backend(format!("cannot encode the noise rules: {error}"))
        })?;
        self.with_connection(move |connection| {
            connection
                .execute(
                    "INSERT INTO noise_rules (id, rules, set_at_ms) VALUES (1, ?1, ?2)
                     ON CONFLICT(id) DO UPDATE SET
                        rules     = excluded.rules,
                        set_at_ms = excluded.set_at_ms",
                    rusqlite::params![encoded, now_ms()],
                )
                .map_err(backend)?;
            Ok(rules)
        })
        .await
    }

    async fn ask_before_send(&self) -> Result<Option<bool>, StoreError> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT ask_before_send FROM voice_settings WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(backend)
                .map(|stored| stored.map(|value| value != 0))
        })
        .await
    }

    async fn set_ask_before_send(&self, ask: bool) -> Result<bool, StoreError> {
        self.with_connection(move |connection| {
            connection
                .execute(
                    "INSERT INTO voice_settings (id, ask_before_send, set_at_ms) VALUES (1, ?1, ?2)
                     ON CONFLICT(id) DO UPDATE SET
                        ask_before_send = excluded.ask_before_send,
                        set_at_ms       = excluded.set_at_ms",
                    rusqlite::params![i64::from(ask), now_ms()],
                )
                .map_err(backend)?;
            Ok(ask)
        })
        .await
    }

    async fn owner_prompt(&self) -> Result<Option<OwnerPrompt>, StoreError> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT instructions, system_prompt, edited_from FROM voice_prompt WHERE id = 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(backend)?
                .map(|(instructions, text, from)| {
                    let system_prompt = match (text, from) {
                        (Some(text), Some(from)) => Some(EditedPrompt {
                            text,
                            from_version: u32::try_from(from).map_err(|_| {
                                StoreError::Backend(format!(
                                    "the edited prompt's version {from} is not a version"
                                ))
                            })?,
                        }),
                        _ => None,
                    };
                    Ok(OwnerPrompt {
                        instructions,
                        system_prompt,
                    })
                })
                .transpose()
        })
        .await
    }

    async fn set_owner_prompt(&self, prompt: &OwnerPrompt) -> Result<OwnerPrompt, StoreError> {
        let prompt = prompt.clone();
        self.with_connection(move |connection| {
            let (text, from) = match &prompt.system_prompt {
                Some(edited) => (Some(edited.text.clone()), Some(i64::from(edited.from_version))),
                None => (None, None),
            };
            connection
                .execute(
                    "INSERT INTO voice_prompt (id, instructions, system_prompt, edited_from, set_at_ms)
                     VALUES (1, ?1, ?2, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET
                        instructions  = excluded.instructions,
                        system_prompt = excluded.system_prompt,
                        edited_from   = excluded.edited_from,
                        set_at_ms     = excluded.set_at_ms",
                    rusqlite::params![prompt.instructions, text, from, now_ms()],
                )
                .map_err(backend)?;
            Ok(prompt)
        })
        .await
    }

    async fn noise_exemptions(&self, channel: &ChannelId) -> Result<Vec<MessageId>, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT message_id FROM noise_exemptions WHERE channel_id = ?1
                     ORDER BY message_id ASC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([channel.as_str()], |row| row.get::<_, String>(0))
                .map_err(backend)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(MessageId(row.map_err(backend)?));
            }
            Ok(out)
        })
        .await
    }

    async fn exempt_from_noise(
        &self,
        channel: &ChannelId,
        messages: &[MessageId],
    ) -> Result<u64, StoreError> {
        crate::noise::validate_exempt_ids(messages)?;
        let channel = channel.clone();
        let messages: Vec<MessageId> = messages.to_vec();
        let retention = self.retention;
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let at = now_ms();
            let mut added = 0_u64;
            for message in &messages {
                added += transaction
                    .execute(
                        "INSERT INTO noise_exemptions (channel_id, message_id, at_ms)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(channel_id, message_id) DO NOTHING",
                        rusqlite::params![channel.as_str(), message.as_str(), at],
                    )
                    .map_err(backend)? as u64;
            }
            prune_noise_exemptions(&transaction, retention)?;
            transaction.commit().map_err(backend)?;
            Ok(added)
        })
        .await
    }

    async fn pins(&self, channel: &ChannelId) -> Result<Pins, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            // One statement for the rows and one for the revision, under the one connection lock,
            // so the revision handed back is the revision of exactly these rows.
            let mut statement = connection
                .prepare(
                    "SELECT message_id, author, author_id, author_is_bot, content, truncated,
                            sent_at, thread_id, thread_root, pinned_at_ms
                     FROM pins WHERE channel_id = ?1
                     ORDER BY COALESCE(sent_at_ms, pinned_at_ms) ASC, message_id ASC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([channel.as_str()], |row| {
                    Ok(Pin {
                        snapshot: PinSnapshot {
                            message_id: MessageId(row.get::<_, String>(0)?),
                            author: row.get::<_, String>(1)?,
                            author_id: UserId(row.get::<_, String>(2)?),
                            author_is_bot: row.get::<_, i64>(3)? != 0,
                            content: row.get::<_, String>(4)?,
                            truncated: row.get::<_, i64>(5)? != 0,
                            timestamp: row.get::<_, String>(6)?,
                            thread_id: row.get::<_, Option<String>>(7)?,
                            thread_root: row.get::<_, i64>(8)? != 0,
                        },
                        pinned_at_ms: row.get::<_, i64>(9)?,
                        content_html: String::new(),
                        heading: None,
                    })
                })
                .map_err(backend)?;
            let pins = rows.collect::<Result<Vec<_>, _>>().map_err(backend)?;
            Ok(Pins {
                pins,
                revision: pin_revision(connection, channel.as_str())?,
            })
        })
        .await
    }

    async fn pins_revision(&self, channel: &ChannelId) -> Result<i64, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| pin_revision(connection, channel.as_str()))
            .await
    }

    async fn pin(
        &self,
        channel: &ChannelId,
        snapshot: &PinSnapshot,
    ) -> Result<PinChange, StoreError> {
        let snapshot = super::validate_pin(snapshot)?;
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let id = snapshot.message_id.as_str();
            // The snapshot is refreshed, `pinned_at_ms` is NOT: a second pin of the same message
            // keeps its place in the bound's queue, so a repeated tap cannot make an old pin the
            // newest and spare it at another's expense. The WHERE makes an identical repeat a
            // no-op SQLite reports as no change, which is what keeps the revision still for it.
            let written = transaction
                .execute(
                    "INSERT INTO pins (channel_id, message_id, author, author_id, author_is_bot,
                                       content, truncated, sent_at, sent_at_ms, thread_id,
                                       thread_root, pinned_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(channel_id, message_id) DO UPDATE SET
                        author = excluded.author, author_id = excluded.author_id,
                        author_is_bot = excluded.author_is_bot, content = excluded.content,
                        truncated = excluded.truncated, sent_at = excluded.sent_at,
                        sent_at_ms = excluded.sent_at_ms, thread_id = excluded.thread_id,
                        thread_root = excluded.thread_root
                     WHERE pins.author IS NOT excluded.author
                        OR pins.author_id IS NOT excluded.author_id
                        OR pins.author_is_bot IS NOT excluded.author_is_bot
                        OR pins.content IS NOT excluded.content
                        OR pins.truncated IS NOT excluded.truncated
                        OR pins.sent_at IS NOT excluded.sent_at
                        OR pins.thread_id IS NOT excluded.thread_id
                        OR pins.thread_root IS NOT excluded.thread_root",
                    rusqlite::params![
                        channel.as_str(),
                        id,
                        &snapshot.author,
                        snapshot.author_id.as_str(),
                        i64::from(snapshot.author_is_bot),
                        &snapshot.content,
                        i64::from(snapshot.truncated),
                        &snapshot.timestamp,
                        crate::clock::instant_ms(&snapshot.timestamp),
                        snapshot.thread_id.as_deref(),
                        i64::from(snapshot.thread_root),
                        now_ms()
                    ],
                )
                .map_err(backend)?;
            // The bound, in the same transaction: every OTHER pin in the channel past the newest
            // `MAX_PINS_PER_CHANNEL - 1` goes, oldest pinned first. Excluding this message by id
            // rather than trusting it to sort newest is what guarantees the pin just asked for is
            // never the one dropped, whatever the clock did.
            let keep = i64::try_from(MAX_PINS_PER_CHANNEL.saturating_sub(1)).unwrap_or(i64::MAX);
            let mut unpinned = Vec::new();
            {
                let mut statement = transaction
                    .prepare(
                        "SELECT message_id FROM pins WHERE channel_id = ?1 AND message_id <> ?2
                         ORDER BY pinned_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?3",
                    )
                    .map_err(backend)?;
                let rows = statement
                    .query_map(rusqlite::params![channel.as_str(), id, keep], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(backend)?;
                for row in rows {
                    unpinned.push(MessageId(row.map_err(backend)?));
                }
            }
            // Named oldest first, which is the order the owner would have pinned them in.
            unpinned.reverse();
            for gone in &unpinned {
                transaction
                    .execute(
                        "DELETE FROM pins WHERE channel_id = ?1 AND message_id = ?2",
                        rusqlite::params![channel.as_str(), gone.as_str()],
                    )
                    .map_err(backend)?;
            }
            let changed = written > 0;
            let revision = if changed || !unpinned.is_empty() {
                bump_pin_revision(&transaction, channel.as_str())?
            } else {
                pin_revision(&transaction, channel.as_str())?
            };
            let pinned_at_ms: i64 = transaction
                .query_row(
                    "SELECT pinned_at_ms FROM pins WHERE channel_id = ?1 AND message_id = ?2",
                    rusqlite::params![channel.as_str(), id],
                    |row| row.get(0),
                )
                .map_err(backend)?;
            transaction.commit().map_err(backend)?;
            Ok(PinChange {
                changed,
                pin: Some(Pin {
                    snapshot,
                    pinned_at_ms,
                    content_html: String::new(),
                    heading: None,
                }),
                unpinned,
                revision,
            })
        })
        .await
    }

    async fn unpin(
        &self,
        channel: &ChannelId,
        message: &MessageId,
    ) -> Result<PinChange, StoreError> {
        super::validate_pin_id(message)?;
        let channel = channel.clone();
        let message = message.clone();
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let removed = transaction
                .execute(
                    "DELETE FROM pins WHERE channel_id = ?1 AND message_id = ?2",
                    rusqlite::params![channel.as_str(), message.as_str()],
                )
                .map_err(backend)?;
            let revision = if removed > 0 {
                bump_pin_revision(&transaction, channel.as_str())?
            } else {
                pin_revision(&transaction, channel.as_str())?
            };
            transaction.commit().map_err(backend)?;
            Ok(PinChange {
                changed: removed > 0,
                pin: None,
                unpinned: Vec::new(),
                revision,
            })
        })
        .await
    }

    async fn bookmark(&self, channel: &ChannelId) -> Result<Bookmark, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| bookmark_row(connection, channel.as_str()))
            .await
    }

    async fn set_bookmark(
        &self,
        channel: &ChannelId,
        anchor: Option<&BookmarkAnchor>,
        if_revision: Option<i64>,
    ) -> Result<BookmarkChange, StoreError> {
        if let Some(anchor) = anchor {
            super::validate_bookmark(anchor)?;
        }
        let channel = channel.clone();
        let anchor = anchor.cloned();
        self.with_connection(move |connection| {
            // The read, the decision and the write under one transaction and one connection lock,
            // so a conditional write cannot pass its check against a row another write replaces.
            let transaction = connection.transaction().map_err(backend)?;
            let held = bookmark_row(&transaction, channel.as_str())?;
            let now = now_ms();
            let change = super::decide_bookmark(&held, anchor.as_ref(), if_revision, now);
            if change.changed {
                let stored = change.bookmark.anchor.as_ref();
                transaction
                    .execute(
                        "INSERT INTO bookmarks (channel_id, message_id, place, sent_at, thread_id,
                                                set_at_ms, revision)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(channel_id) DO UPDATE SET
                            message_id = excluded.message_id, place = excluded.place,
                            sent_at = excluded.sent_at, thread_id = excluded.thread_id,
                            set_at_ms = excluded.set_at_ms, revision = excluded.revision",
                        rusqlite::params![
                            channel.as_str(),
                            stored.map(|held| held.message_id.as_str()),
                            stored.map(|held| held.place.as_str()),
                            stored.map(|held| held.timestamp.as_str()),
                            stored.and_then(|held| held.thread_id.as_deref()),
                            now,
                            change.bookmark.revision,
                        ],
                    )
                    .map_err(backend)?;
            }
            transaction.commit().map_err(backend)?;
            Ok(change)
        })
        .await
    }

    async fn cached_summary(&self, key: &SummaryKey) -> Result<Option<CachedSummary>, StoreError> {
        let key = key.clone();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT summary, cut_at, made_at_ms FROM summaries
                     WHERE version = ?1 AND channel_id = ?2 AND message_id = ?3
                       AND content_hash = ?4",
                    rusqlite::params![
                        &key.version,
                        key.channel.as_str(),
                        key.message.as_str(),
                        hex(key.content_hash)
                    ],
                    |row| {
                        Ok(CachedSummary {
                            text: row.get(0)?,
                            // A length past `u32` cannot have been written by this server; read it
                            // as "cut very long", which is never below any cap.
                            cut_at: row
                                .get::<_, Option<i64>>(1)?
                                .map(|at| u32::try_from(at).unwrap_or(u32::MAX)),
                            made_at_ms: row.get(2)?,
                        })
                    },
                )
                .optional()
                .map_err(backend)
        })
        .await
    }

    async fn cache_summary(
        &self,
        key: &SummaryKey,
        summary: &str,
        cut_at: Option<u32>,
    ) -> Result<(), StoreError> {
        let key = key.clone();
        let summary = summary.to_owned();
        let retention = self.retention;
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            transaction
                .execute(
                    "INSERT INTO summaries
                        (version, channel_id, message_id, content_hash, summary, made_at_ms,
                         cut_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(version, channel_id, message_id, content_hash) DO UPDATE SET
                        summary = excluded.summary, made_at_ms = excluded.made_at_ms,
                        cut_at = excluded.cut_at",
                    rusqlite::params![
                        &key.version,
                        key.channel.as_str(),
                        key.message.as_str(),
                        hex(key.content_hash),
                        &summary,
                        now_ms(),
                        cut_at.map(i64::from),
                    ],
                )
                .map_err(backend)?;
            // In the same transaction as the insert, for the same reason the transcript prunes
            // there: a bound applied "soon afterwards" is a bound a crash can skip.
            prune_summaries(&transaction, retention)?;
            transaction.commit().map_err(backend)
        })
        .await
    }

    async fn touch_summary(&self, key: &SummaryKey) -> Result<bool, StoreError> {
        let key = key.clone();
        self.with_connection(move |connection| {
            let now = now_ms();
            let touched = connection
                .execute(
                    "UPDATE summaries SET made_at_ms = ?5
                     WHERE version = ?1 AND channel_id = ?2 AND message_id = ?3
                       AND content_hash = ?4 AND made_at_ms < ?6",
                    rusqlite::params![
                        &key.version,
                        key.channel.as_str(),
                        key.message.as_str(),
                        hex(key.content_hash),
                        now,
                        now - super::SUMMARY_TOUCH_AFTER_MS,
                    ],
                )
                .map_err(backend)?;
            Ok(touched > 0)
        })
        .await
    }

    async fn rekey_summaries(
        &self,
        from: &str,
        to: &str,
        cap_then: u32,
    ) -> Result<u64, StoreError> {
        let from = from.to_owned();
        let to = to.to_owned();
        self.with_connection(move |connection| {
            if from == to {
                return Ok(0);
            }
            let transaction = connection.transaction().map_err(backend)?;
            // A thread sentence is a JSON record whose `summary` is the text that was cut; any
            // other row's text is the summary itself. `json_valid` first, so one malformed row is
            // read as plain text rather than failing the whole move.
            let moved = transaction
                .execute(
                    "INSERT OR IGNORE INTO summaries
                        (version, channel_id, message_id, content_hash, summary, made_at_ms,
                         cut_at)
                     SELECT ?2, channel_id, message_id, content_hash, summary, made_at_ms,
                            CASE WHEN substr(
                                     CASE WHEN substr(message_id, 1, length(?4)) = ?4
                                               AND json_valid(summary)
                                          THEN json_extract(summary, '$.summary')
                                          ELSE summary END,
                                     -1) = '…'
                                 THEN ?3 ELSE NULL END
                     FROM summaries WHERE version = ?1",
                    rusqlite::params![
                        &from,
                        &to,
                        i64::from(cap_then),
                        crate::summarize::threads::SENTENCE_PREFIX
                    ],
                )
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM summaries WHERE version = ?1", [&from])
                .map_err(backend)?;
            transaction.commit().map_err(backend)?;
            Ok(moved as u64)
        })
        .await
    }

    async fn forget_channel_summaries(&self, channel: &ChannelId) -> Result<u64, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let removed = connection
                .execute(
                    "DELETE FROM summaries WHERE channel_id = ?1 AND version <> ?2",
                    [channel.as_str(), super::KEPT_VERSION],
                )
                .map_err(backend)?;
            Ok(removed as u64)
        })
        .await
    }

    async fn forget_summaries_except(&self, version: &str) -> Result<u64, StoreError> {
        let version = version.to_owned();
        self.with_connection(move |connection| {
            let removed = connection
                .execute(
                    "DELETE FROM summaries WHERE version <> ?1 AND version <> ?2",
                    [version.as_str(), super::KEPT_VERSION],
                )
                .map_err(backend)?;
            Ok(removed as u64)
        })
        .await
    }

    async fn upstream_clear_settings(&self) -> Result<Vec<UpstreamClearSetting>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT {UPSTREAM_CLEAR_COLUMNS} FROM upstream_clear ORDER BY channel_id ASC"
                ))
                .map_err(backend)?;
            let rows = statement
                .query_map([], upstream_clear_row)
                .map_err(backend)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(backend)
        })
        .await
    }

    async fn set_upstream_clear_mode(
        &self,
        channel: &ChannelId,
        mode: UpstreamClearMode,
        at_ms: i64,
    ) -> Result<UpstreamClearSetting, StoreError> {
        let channel = channel.clone();
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let mut setting = upstream_clear_setting(&transaction, channel.as_str())?
                .unwrap_or_else(|| UpstreamClearSetting::unset(channel.clone()));
            setting.change_mode(mode, at_ms);
            write_upstream_clear(&transaction, &setting)?;
            transaction.commit().map_err(backend)?;
            Ok(setting)
        })
        .await
    }

    async fn record_upstream_clear(
        &self,
        channel: &ChannelId,
        attempt: &UpstreamClearAttempt,
    ) -> Result<(), StoreError> {
        let channel = channel.clone();
        let attempt = attempt.clone();
        self.with_connection(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            let Some(mut setting) = upstream_clear_setting(&transaction, channel.as_str())? else {
                return Ok(());
            };
            setting.record(&attempt);
            write_upstream_clear(&transaction, &setting)?;
            transaction.commit().map_err(backend)
        })
        .await
    }

    async fn purge_everything(&self) -> Result<(), StoreError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction().map_err(backend)?;
            // Every channel back to off, which is the shipped default and what no row means.
            transaction
                .execute("DELETE FROM upstream_clear", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM conversations", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM read_marks", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM summaries", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM dismissals", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM channel_aliases", [])
                .map_err(backend)?;
            // Back to the shipped default, which is what "erase everything this store holds"
            // means for a list that has one: no row is the never-saved state.
            transaction
                .execute("DELETE FROM noise_rules", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM noise_exemptions", [])
                .map_err(backend)?;
            // The pins, snapshots and all, and their counters with them: a revision of 0 after
            // this is what tells every page holding an older list to read it again, and the next
            // pin's revision starts from the clock, past every one handed out before.
            transaction
                .execute("DELETE FROM pins", [])
                .map_err(backend)?;
            transaction
                .execute("DELETE FROM pin_revisions", [])
                .map_err(backend)?;
            // The bookmarks, revisions and all, for the pins' reason: the next one starts from the
            // clock, past every revision a page may still hold. `#240 reading-bookmark`.
            transaction
                .execute("DELETE FROM bookmarks", [])
                .map_err(backend)?;
            // Back to the configured default. That asks before the voice agent sends unless the
            // operator configured `ask_before_send = false`, in which case a purge turns direct
            // sends back on for an owner who had chosen to be asked: the purge restores what the
            // operator chose, not necessarily the safer setting. `#264 voice-send-approval`.
            transaction
                .execute("DELETE FROM voice_settings", [])
                .map_err(backend)?;
            // Back to the shipped prompt, with no instructions of his. `#272
            // voice-agent-instructions`.
            transaction
                .execute("DELETE FROM voice_prompt", [])
                .map_err(backend)?;
            transaction.commit().map_err(backend)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn store(dir: &TempDir, retention: Retention) -> SqliteStore {
        SqliteStore::open(
            &dir.path().join("state").join("vibe-talk.sqlite3"),
            retention,
        )
        .expect("the store opens")
    }

    fn id(text: &str) -> ConversationId {
        ConversationId::parse(text).expect("valid id")
    }

    #[tokio::test]
    async fn a_transcript_survives_the_store_being_closed_and_reopened() {
        // This is the property that distinguishes storage from a cache, and it is the one the
        // whole issue is about: it cannot be shown by the in-memory fake.
        let dir = TempDir::new("sqlite-reopen");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            store
                .append_turn(&id("conv1"), &Turn::now(Speaker::You, "what happened?"))
                .await
                .expect("append");
            store
                .append_turn(
                    &id("conv1"),
                    &Turn::now(Speaker::Agent, "the runner stalled"),
                )
                .await
                .expect("append");
        }
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        let turns = reopened.turns(&id("conv1")).await.expect("turns");
        assert_eq!(turns.len(), 2, "the transcript did not survive a restart");
        assert_eq!(turns[0].speaker, Speaker::You);
        assert_eq!(turns[0].text, "what happened?");
        assert_eq!(
            turns[1].text, "the runner stalled",
            "turns must come back in the order they were said"
        );
    }

    /// A turn at an instant the test chose, rather than at `now`.
    ///
    /// The whole subject of the tests below is what happens when two instants are EQUAL, and
    /// `Turn::now` cannot be asked for that.
    fn said(speaker: Speaker, text: &str, at_ms: i64) -> Turn {
        Turn {
            speaker,
            text: text.to_owned(),
            at_ms,
        }
    }

    #[tokio::test]
    async fn the_walk_back_crosses_a_millisecond_collision_without_losing_a_turn() {
        // The case a cursor of `at_ms` alone gets wrong, and the reason the cursor is a triple.
        // Two conversations hold turns stamped with the SAME millisecond — which is what happens
        // while a call is open and a channel is being read aloud into a second one — and a page
        // boundary is made to land in the middle of them.
        let dir = TempDir::new("sqlite-transcript-collision");
        let store = store(&dir, Retention::default());
        // Near now, not a memorable round number: retention prunes anything older than
        // `retain_days`, so a fixed 2023 timestamp would be swept and this test would assert
        // against an empty store.
        let tied = now_ms();
        for (conversation, speaker, text, at_ms) in [
            ("conv_a", Speaker::You, "oldest", tied - 10),
            ("conv_a", Speaker::Agent, "tied a1", tied),
            ("conv_b", Speaker::You, "tied b1", tied),
            ("conv_b", Speaker::Agent, "tied b2", tied),
            ("conv_a", Speaker::You, "newest", tied + 10),
        ] {
            store
                .append_turn(&id(conversation), &said(speaker, text, at_ms))
                .await
                .expect("append");
        }

        // One turn at a time, so EVERY boundary falls inside the tie.
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.transcript(1, cursor.as_ref()).await.expect("page");
            for entry in &page.turns {
                seen.push(entry.turn.text.clone());
            }
            if !page.has_more {
                break;
            }
            cursor = page.next;
            assert!(seen.len() < 10, "the walk did not terminate: {seen:?}");
        }
        assert_eq!(seen.len(), 5, "a turn fell down a page boundary: {seen:?}");
        assert_eq!(seen.first().map(String::as_str), Some("newest"));
        assert_eq!(seen.last().map(String::as_str), Some("oldest"));
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 5, "a turn came back twice: {seen:?}");
        // The three tied turns are in the middle in SOME order — which one is the tie-break's
        // business — but they are all there, which is the property being asserted.
        assert_eq!(
            seen[1..4].iter().filter(|t| t.starts_with("tied")).count(),
            3,
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn one_page_of_the_record_is_the_newest_turns_wherever_they_were_said() {
        // The read a reader's first glance makes: the newest N turns, from anywhere, rather than
        // the whole of the newest conversation.
        let dir = TempDir::new("sqlite-transcript-newest");
        let store = store(&dir, Retention::default());
        // Near now, for the reason given in the test above: retention would sweep a fixed old one.
        let base = now_ms();
        // A long earlier call, and a short recent one. Asking for three turns must not return the
        // beginning of the long one.
        for index in 0..20 {
            store
                .append_turn(
                    &id("conv_long"),
                    &said(Speaker::You, &format!("old {index}"), base + index),
                )
                .await
                .expect("append");
        }
        for index in 0..2 {
            store
                .append_turn(
                    &id("conv_recent"),
                    &said(
                        Speaker::Agent,
                        &format!("new {index}"),
                        base + 1_000 + index,
                    ),
                )
                .await
                .expect("append");
        }

        let page = store.transcript(3, None).await.expect("page");
        assert_eq!(
            page.turns
                .iter()
                .map(|entry| entry.turn.text.as_str())
                .collect::<Vec<_>>(),
            vec!["new 1", "new 0", "old 19"],
            "the page is not the newest three turns of the whole record"
        );
        assert!(
            page.has_more,
            "nineteen turns are still older than this page"
        );
        // Each one says where it came from, which is what lets a reader be shown that the call
        // changed rather than being handed two calls as one conversation.
        assert_eq!(page.turns[0].conversation.as_str(), "conv_recent");
        assert_eq!(page.turns[2].conversation.as_str(), "conv_long");
        assert_eq!(page.turns[2].seq, 20, "seq is the position within its call");
    }

    #[tokio::test]
    async fn the_database_file_and_its_directory_are_private() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = TempDir::new("sqlite-modes");
            let store = store(&dir, Retention::default());
            let file_mode = std::fs::metadata(store.path())
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                file_mode, 0o600,
                "the transcript database is readable by others"
            );
            let dir_mode = std::fs::metadata(store.path().parent().expect("parent"))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                dir_mode, 0o700,
                "the storage directory is traversable by others"
            );
        }
    }

    #[tokio::test]
    async fn a_listing_is_newest_first_and_carries_a_preview_without_the_transcript() {
        let dir = TempDir::new("sqlite-list");
        // retain_days = 0 so the synthetic epoch-millis timestamps below are not swept away by
        // the age limit before the ordering can be asserted.
        let store = store(
            &dir,
            Retention {
                retain_days: 0,
                ..Retention::default()
            },
        );
        store
            .append_turn(
                &id("older"),
                &Turn {
                    speaker: Speaker::You,
                    text: "the first thing said".to_owned(),
                    at_ms: 1_000,
                },
            )
            .await
            .expect("append");
        store
            .append_turn(
                &id("newer"),
                &Turn {
                    speaker: Speaker::You,
                    text: "said later".to_owned(),
                    at_ms: 2_000,
                },
            )
            .await
            .expect("append");
        let listed = store.conversations().await.expect("list");
        assert_eq!(
            listed.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["newer", "older"],
            "a listing must be most-recently-active first"
        );
        assert_eq!(listed[1].preview, "the first thing said");
        assert_eq!(listed[1].turns, 1);
        assert_eq!(listed[1].started_at_ms, 1_000);
    }

    #[tokio::test]
    async fn retention_drops_the_least_recently_active_conversation() {
        let dir = TempDir::new("sqlite-retention");
        let store = store(
            &dir,
            Retention {
                max_conversations: 2,
                retain_days: 0,
                ..Retention::default()
            },
        );
        for (n, at) in [("a", 1_000), ("b", 2_000), ("c", 3_000)] {
            store
                .append_turn(
                    &id(n),
                    &Turn {
                        speaker: Speaker::You,
                        text: n.to_owned(),
                        at_ms: at,
                    },
                )
                .await
                .expect("append");
        }
        let kept: Vec<String> = store
            .conversations()
            .await
            .expect("list")
            .into_iter()
            .map(|c| c.id.as_str().to_owned())
            .collect();
        assert_eq!(kept, vec!["c".to_owned(), "b".to_owned()]);
        assert!(
            matches!(store.turns(&id("a")).await, Err(StoreError::NotFound)),
            "the pruned conversation's turns must go with it, not linger as orphans"
        );
    }

    #[tokio::test]
    async fn retention_drops_a_conversation_that_is_older_than_the_age_limit() {
        let dir = TempDir::new("sqlite-age");
        let store = store(
            &dir,
            Retention {
                retain_days: 1,
                ..Retention::default()
            },
        );
        let two_days_ago = now_ms() - 2 * 86_400_000;
        store
            .append_turn(
                &id("stale"),
                &Turn {
                    speaker: Speaker::You,
                    text: "old".to_owned(),
                    at_ms: two_days_ago,
                },
            )
            .await
            .expect("append");
        assert_eq!(
            store.conversations().await.expect("list").len(),
            0,
            "a conversation past retain_days must not survive the append that noticed it"
        );
    }

    #[tokio::test]
    async fn a_conversation_at_its_turn_ceiling_refuses_further_turns() {
        let dir = TempDir::new("sqlite-ceiling");
        let store = store(
            &dir,
            Retention {
                max_turns_per_conversation: 2,
                ..Retention::default()
            },
        );
        for _ in 0..2 {
            store
                .append_turn(&id("conv"), &Turn::now(Speaker::You, "hello"))
                .await
                .expect("append");
        }
        let error = store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "hello"))
            .await
            .expect_err("a third turn must be refused");
        assert_eq!(error.code(), "too_large", "{error}");
        assert_eq!(
            store.turns(&id("conv")).await.expect("turns").len(),
            2,
            "the refused turn must not have been written"
        );
    }

    #[tokio::test]
    async fn an_oversized_turn_is_refused_before_it_reaches_the_database() {
        let dir = TempDir::new("sqlite-oversize");
        let store = store(&dir, Retention::default());
        let error = store
            .append_turn(
                &id("conv"),
                &Turn::now(Speaker::You, "x".repeat(MAX_TURN_CHARS + 1)),
            )
            .await
            .expect_err("must refuse");
        assert_eq!(error.code(), "too_large", "{error}");
        assert!(
            matches!(store.turns(&id("conv")).await, Err(StoreError::NotFound)),
            "a refused first turn must not create the conversation"
        );
    }

    #[tokio::test]
    async fn forgetting_says_whether_there_was_anything_to_forget() {
        let dir = TempDir::new("sqlite-forget");
        let store = store(&dir, Retention::default());
        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "hello"))
            .await
            .expect("append");
        store
            .forget_conversation(&id("conv"))
            .await
            .expect("forget");
        assert!(
            matches!(
                store.forget_conversation(&id("conv")).await,
                Err(StoreError::NotFound)
            ),
            "forgetting something twice must not report success the second time"
        );
        assert_eq!(
            store.forget_all_conversations().await.expect("forget all"),
            0
        );
    }

    #[tokio::test]
    async fn a_read_mark_moves_forward_and_never_backwards() {
        let dir = TempDir::new("sqlite-marks");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        assert!(store.read_mark(&channel).await.expect("read").is_none());

        let mark = store
            .mark_read(&channel, &MessageId("1000000000000000200".to_owned()))
            .await
            .expect("mark");
        assert_eq!(mark.last_read.as_str(), "1000000000000000200");

        let back = store
            .mark_read(&channel, &MessageId("1000000000000000100".to_owned()))
            .await
            .expect("mark");
        assert_eq!(
            back.last_read.as_str(),
            "1000000000000000200",
            "a stale client must not drag the read mark backwards"
        );

        let forward = store
            .mark_read(&channel, &MessageId("1000000000000000300".to_owned()))
            .await
            .expect("mark");
        assert_eq!(forward.last_read.as_str(), "1000000000000000300");
        assert_eq!(store.read_marks().await.expect("marks").len(), 1);
    }

    #[tokio::test]
    async fn the_inbox_overlay_survives_the_store_being_closed_and_reopened() {
        // THE property that distinguishes an overlay from a cache, and the reason `#50 todo-view`
        // depends on `#48 transcript-storage`: a database inside a container image is storage
        // until the first rebuild. Nothing in `store::fake` can make this claim.
        let dir = TempDir::new("sqlite-dismissals");
        let channel = ChannelId("1111111111".to_owned());
        let dealt = MessageId("1000000000000000200".to_owned());
        let left = MessageId("1000000000000000300".to_owned());
        {
            let store = store(&dir, Retention::default());
            assert_eq!(
                store
                    .dismiss(&channel, std::slice::from_ref(&dealt))
                    .await
                    .expect("dismiss"),
                1
            );
            // The second one is the control: without it, "the file remembers" is satisfied by a
            // reopen that answers everything.
            assert_eq!(
                store
                    .dismiss(&channel, std::slice::from_ref(&dealt))
                    .await
                    .expect("dismiss again"),
                0,
                "dismissing something twice reported a second change"
            );
        }

        let reopened = store(&dir, Retention::default());
        assert_eq!(
            reopened.dismissals(&channel).await.expect("read"),
            vec![dealt.clone()],
            "the inbox overlay did not survive a restart, which makes it a cache"
        );
        assert!(
            !reopened
                .dismissals(&channel)
                .await
                .expect("read")
                .contains(&left),
            "a message nobody dismissed came back marked as dealt with"
        );

        // ...and the undo is durable too, which is the half a write-only overlay would fail.
        assert_eq!(
            reopened
                .restore(&channel, std::slice::from_ref(&dealt))
                .await
                .expect("restore"),
            1
        );
        drop(reopened);
        assert!(
            store(&dir, Retention::default())
                .dismissals(&channel)
                .await
                .expect("read")
                .is_empty(),
            "an undo did not survive the restart the dismissal did"
        );
    }

    #[tokio::test]
    async fn the_dismissal_table_is_bounded_by_count_and_the_oldest_mark_goes_first() {
        let dir = TempDir::new("sqlite-dismissal-bound");
        let store = store(
            &dir,
            Retention {
                max_dismissals: 2,
                ..Retention::default()
            },
        );
        let channel = ChannelId("1111111111".to_owned());
        for n in 0..2_u64 {
            store
                .dismiss(
                    &channel,
                    &[MessageId(format!("100000000000000{:04}", 100 + n))],
                )
                .await
                .expect("dismiss");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        store
            .dismiss(&channel, &[MessageId("1000000000000000999".to_owned())])
            .await
            .expect("dismiss");
        let held = store.dismissals(&channel).await.expect("read");
        assert_eq!(held.len(), 2, "the ceiling did not hold");
        assert!(
            held.iter().any(|id| id.as_str() == "1000000000000000999"),
            "the ceiling evicted the mark that was just made: {held:?}"
        );
        assert!(
            !held.iter().any(|id| id.as_str() == "1000000000000000100"),
            "the ceiling evicted the newest mark instead of the oldest: {held:?}"
        );
    }

    #[tokio::test]
    async fn an_old_dismissal_is_never_collected_by_age_the_way_an_old_summary_is() {
        // The deliberate asymmetry in `Retention`, and the ONE test that can see it. A dismissal
        // written now is inside any age limit expressible in whole days, so this backdates the row
        // by a hundred days in the file itself and then triggers a prune — which is what a real
        // deployment does after three months of use. An age bound here would put a message the
        // owner cleared in the spring back in front of him in the summer, and nothing on the row
        // would say why.
        let dir = TempDir::new("sqlite-dismissal-age");
        let store = store(
            &dir,
            Retention {
                retain_days: 30,
                ..Retention::default()
            },
        );
        let channel = ChannelId("1111111111".to_owned());
        let ancient = MessageId("1000000000000000100".to_owned());
        store
            .dismiss(&channel, std::slice::from_ref(&ancient))
            .await
            .expect("dismiss");
        {
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .execute(
                    "UPDATE dismissals SET at_ms = ?1",
                    [now_ms() - 100 * 86_400_000],
                )
                .expect("backdate");
        }

        // THE CONTROL, in the same file under the same retention: a summary of the same age IS
        // collected. Without it this test would pass on a store that prunes nothing at all.
        let key = SummaryKey {
            channel: channel.clone(),
            message: ancient.clone(),
            content_hash: 7,
            version: "current".to_owned(),
        };
        store
            .cache_summary(&key, "old text", None)
            .await
            .expect("cache");
        {
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .execute(
                    "UPDATE summaries SET made_at_ms = ?1",
                    [now_ms() - 100 * 86_400_000],
                )
                .expect("backdate");
        }

        // One more write of each, which is when every bound in this store is applied.
        store
            .dismiss(&channel, &[MessageId("1000000000000000200".to_owned())])
            .await
            .expect("dismiss");
        let fresh = SummaryKey {
            message: MessageId("1000000000000000200".to_owned()),
            ..key.clone()
        };
        store
            .cache_summary(&fresh, "new text", None)
            .await
            .expect("cache");

        assert!(
            store
                .dismissals(&channel)
                .await
                .expect("read")
                .contains(&ancient),
            "an age bound reached the inbox overlay and resurrected a message the owner cleared"
        );
        assert_eq!(
            store.cached_summary(&key).await.expect("read"),
            None,
            "the control failed: the age bound is not collecting anything at all, so the \
             assertion above says nothing"
        );
    }

    #[tokio::test]
    async fn a_read_mark_refuses_an_id_it_cannot_order() {
        let dir = TempDir::new("sqlite-badmark");
        let store = store(&dir, Retention::default());
        let error = store
            .mark_read(
                &ChannelId("1111111111".to_owned()),
                &MessageId("newest".to_owned()),
            )
            .await
            .expect_err("must refuse");
        assert_eq!(error.code(), "bad_id", "{error}");
    }

    #[tokio::test]
    async fn purging_erases_both_kinds_of_state_and_leaves_the_store_usable() {
        let dir = TempDir::new("sqlite-purge");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "hello"))
            .await
            .expect("append");
        store
            .mark_read(&channel, &MessageId("1000000000000000200".to_owned()))
            .await
            .expect("mark");
        store
            .dismiss(&channel, &[MessageId("1000000000000000300".to_owned())])
            .await
            .expect("dismiss");

        store.purge_everything().await.expect("purge");
        assert!(store.conversations().await.expect("list").is_empty());
        assert!(store.read_marks().await.expect("marks").is_empty());
        assert!(
            store.dismissals(&channel).await.expect("read").is_empty(),
            "the operator's erase left the record of what he had dealt with behind"
        );

        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "still working"))
            .await
            .expect("a purge must not break the store");
    }

    #[test]
    fn a_database_from_a_newer_server_is_refused_rather_than_written_to() {
        let dir = TempDir::new("sqlite-future");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .pragma_update(None, "user_version", 99_i64)
                .expect("pretend a newer server wrote this");
        }
        let error = SqliteStore::open(&file, Retention::default()).expect_err("must refuse");
        assert_eq!(error.code(), "storage_error", "{error}");
        assert!(
            error.to_string().contains("newer vibe-talk"),
            "the operator has to be told why: {error}"
        );
    }

    #[tokio::test]
    async fn a_cached_summary_is_returned_only_for_the_exact_key_it_was_filed_under() {
        let dir = TempDir::new("sqlite-summaries");
        let store = store(&dir, Retention::default());
        let key = SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId("1000000000000000200".to_owned()),
            content_hash: 0xdead_beef_dead_beef,
            version: "v1-elevenlabs-agent-w3-c160-aaaa".to_owned(),
        };
        assert_eq!(store.cached_summary(&key).await.expect("miss"), None);

        store
            .cache_summary(&key, "the runner stalled overnight", None)
            .await
            .expect("cache");
        assert_eq!(
            store
                .cached_summary(&key)
                .await
                .expect("hit")
                .map(|hit| hit.text)
                .as_deref(),
            Some("the runner stalled overnight")
        );

        // Each part of the key is asserted separately, because a lookup that ignored one of them
        // would serve a stale summary for exactly one reason and pass every other check here.
        let edited = SummaryKey {
            content_hash: 0x0bad_c0de_0bad_c0de,
            ..key.clone()
        };
        assert_eq!(
            store.cached_summary(&edited).await.expect("miss"),
            None,
            "an edited message must not be served the old summary"
        );
        let repolicied = SummaryKey {
            version: "v1-elevenlabs-agent-w3-c200-bbbb".to_owned(),
            ..key.clone()
        };
        assert_eq!(
            store.cached_summary(&repolicied).await.expect("miss"),
            None,
            "a changed policy must not be served a summary produced under the old one"
        );
        let elsewhere = SummaryKey {
            message: MessageId("1000000000000000201".to_owned()),
            ..key.clone()
        };
        assert_eq!(store.cached_summary(&elsewhere).await.expect("miss"), None);
    }

    #[tokio::test]
    async fn the_sweep_collects_every_summary_from_an_older_policy_and_nothing_else() {
        let dir = TempDir::new("sqlite-sweep");
        let store = store(&dir, Retention::default());
        let base = SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId("1000000000000000200".to_owned()),
            content_hash: 1,
            version: String::new(),
        };
        for version in ["old-a", "old-b", "current"] {
            store
                .cache_summary(
                    &SummaryKey {
                        version: version.to_owned(),
                        ..base.clone()
                    },
                    version,
                    None,
                )
                .await
                .expect("cache");
        }

        assert_eq!(
            store
                .forget_summaries_except("current")
                .await
                .expect("sweep"),
            2,
            "the sweep must collect exactly the entries produced under an older policy"
        );
        assert_eq!(
            store
                .cached_summary(&SummaryKey {
                    version: "current".to_owned(),
                    ..base.clone()
                })
                .await
                .expect("hit")
                .map(|hit| hit.text)
                .as_deref(),
            Some("current"),
            "the sweep took the current policy's entries with it"
        );
        assert_eq!(
            store
                .forget_summaries_except("current")
                .await
                .expect("sweep"),
            0,
            "a second sweep must find nothing left to do"
        );
    }

    /// Count the rows the summary cache is actually holding.
    ///
    /// Straight SQL rather than a trait method, because what is under test is the TABLE — an
    /// entry the key can no longer reach still occupies a row, and a count that went through
    /// `cached_summary` could not see one.
    fn summary_rows(store: &SqliteStore) -> i64 {
        let guard = store
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard
            .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
            .expect("count")
    }

    fn summary_key(message: &str, content_hash: u64) -> SummaryKey {
        SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId(message.to_owned()),
            content_hash,
            version: "current".to_owned(),
        }
    }

    #[tokio::test]
    async fn the_summary_cache_is_bounded_by_count_and_evicts_the_oldest_first() {
        // Without this the table is the one thing in the store with no bound at all: every edit
        // of a message adds a row and orphans the last one, and nothing ever collects it.
        let dir = TempDir::new("sqlite-summary-count");
        let store = store(
            &dir,
            Retention {
                max_summaries: 3,
                retain_days: 0,
                summary_retain_days: 0,
                ..Retention::default()
            },
        );
        for n in 0..5_u64 {
            store
                .cache_summary(
                    &summary_key(&format!("100000000000000000{n}"), n),
                    &format!("summary {n}"),
                    None,
                )
                .await
                .expect("cache");
            // The bound is on made_at_ms, which comes from the clock; without a gap the five
            // writes can share a millisecond and the eviction order would be decided by the
            // rowid tie-break alone.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            summary_rows(&store),
            3,
            "the summary table grew past its ceiling"
        );
        for n in 0..2_u64 {
            assert_eq!(
                store
                    .cached_summary(&summary_key(&format!("100000000000000000{n}"), n))
                    .await
                    .expect("read"),
                None,
                "entry {n} is the oldest and must have been evicted first"
            );
        }
        for n in 2..5_u64 {
            assert_eq!(
                store
                    .cached_summary(&summary_key(&format!("100000000000000000{n}"), n))
                    .await
                    .expect("read")
                    .map(|hit| hit.text)
                    .as_deref(),
                Some(format!("summary {n}").as_str()),
                "the newest entries must be the ones kept"
            );
        }
    }

    #[tokio::test]
    async fn a_kept_entry_outlives_the_policy_sweep_and_a_flood_of_summaries_and_is_bounded_alone()
    {
        // `#233 llm-summarizer`. A thread's display name is chosen once. Filed in this table, it
        // used to be evicted by ordinary summary traffic and swept by any policy change, and the
        // thread was then given a different name.
        let dir = TempDir::new("sqlite-summary-kept");
        let store = store(
            &dir,
            Retention {
                max_summaries: 3,
                retain_days: 0,
                summary_retain_days: 0,
                ..Retention::default()
            },
        );
        let kept = SummaryKey {
            version: super::super::KEPT_VERSION.to_owned(),
            ..summary_key("thread-name:spaces/A/threads/runner", 0)
        };
        store
            .cache_summary(&kept, "runner-wedged", None)
            .await
            .expect("cache");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        for n in 0..10_u64 {
            store
                .cache_summary(
                    &summary_key(&format!("100000000000000000{n}"), n),
                    &format!("summary {n}"),
                    None,
                )
                .await
                .expect("cache");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            store
                .cached_summary(&kept)
                .await
                .expect("read")
                .map(|hit| hit.text)
                .as_deref(),
            Some("runner-wedged"),
            "ten newer summaries under a ceiling of three evicted the kept entry"
        );
        assert_eq!(
            summary_rows(&store),
            4,
            "three summaries plus the kept entry"
        );
        assert_eq!(
            store
                .forget_summaries_except("a-new-policy")
                .await
                .expect("sweep"),
            3,
            "the sweep collects the summaries of the old policy..."
        );
        assert_eq!(
            store
                .cached_summary(&kept)
                .await
                .expect("read")
                .map(|hit| hit.text)
                .as_deref(),
            Some("runner-wedged"),
            "...and never the entry that belongs to no policy"
        );
        // Kept entries are bounded too, on their own.
        for n in 0..5_u64 {
            store
                .cache_summary(
                    &SummaryKey {
                        version: super::super::KEPT_VERSION.to_owned(),
                        ..summary_key(&format!("thread-name:{n}"), 0)
                    },
                    &format!("name-{n}"),
                    None,
                )
                .await
                .expect("cache");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            summary_rows(&store),
            3,
            "kept entries grew past their own ceiling"
        );
        assert_eq!(store.cached_summary(&kept).await.expect("read"), None);
    }

    #[tokio::test]
    async fn an_orphaned_summary_is_collected_by_the_age_limit_the_sweep_cannot_reach() {
        // The exact shape of the leak: a message is EDITED upstream, so the entry under the old
        // content hash is unreachable by key and stays under the CURRENT policy version — which
        // is the one thing `forget_summaries_except` will never delete. The age bound is what
        // collects it, and this test is the only thing that says so.
        let dir = TempDir::new("sqlite-summary-orphan");
        let store = store(
            &dir,
            Retention {
                retain_days: 1,
                summary_retain_days: 1,
                ..Retention::default()
            },
        );
        let key = summary_key("1000000000000000200", 0xdead_beef);
        store
            .cache_summary(&key, "the runner stalled overnight", None)
            .await
            .expect("cache");
        // Age it two days, which is what the passage of time would do.
        {
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .execute(
                    "UPDATE summaries SET made_at_ms = ?1",
                    [now_ms() - 2 * 86_400_000],
                )
                .expect("age the row");
        }
        assert_eq!(summary_rows(&store), 1, "the row to be collected is there");
        assert_eq!(
            store
                .forget_summaries_except("current")
                .await
                .expect("sweep"),
            0,
            "the startup sweep must NOT be what collects this; it deletes by policy version, and \
             an orphan is under the current one"
        );
        assert_eq!(summary_rows(&store), 1);

        // The edited message's summary is written under a new content hash — and that write is
        // what collects the orphan.
        store
            .cache_summary(
                &summary_key("1000000000000000200", 0x0bad_c0de),
                "reverted",
                None,
            )
            .await
            .expect("cache");
        assert_eq!(
            summary_rows(&store),
            1,
            "the entry for the message's old text outlived the age limit"
        );
        assert_eq!(
            store
                .cached_summary(&summary_key("1000000000000000200", 0x0bad_c0de))
                .await
                .expect("read")
                .map(|hit| hit.text)
                .as_deref(),
            Some("reverted"),
            "the age limit took the live entry too"
        );
    }

    #[tokio::test]
    async fn a_database_at_an_older_schema_version_is_brought_forward() {
        // The migration ladder is only real once it has been walked. This builds a file with ONLY
        // the first migration applied -- the shape every existing deployment is at -- and requires
        // that opening it with today's code makes the newer tables usable.
        let dir = TempDir::new("sqlite-upgrade");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open");
            connection.execute_batch(MIGRATIONS[0]).expect("v1");
            connection
                .pragma_update(None, "user_version", 1_i64)
                .expect("stamp");
        }
        let store = SqliteStore::open(&file, Retention::default()).expect("upgrade");
        let key = SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId("1000000000000000200".to_owned()),
            content_hash: 7,
            version: "current".to_owned(),
        };
        store
            .cache_summary(&key, "it works", None)
            .await
            .expect("the newer table has to exist after the upgrade");
        assert_eq!(
            store
                .cached_summary(&key)
                .await
                .expect("hit")
                .map(|hit| hit.text)
                .as_deref(),
            Some("it works")
        );
        // ...and so does the one after it, which is the whole point of a LADDER: a file at v1
        // has to arrive at the newest shape, not at the shape of the migration that came next.
        store
            .dismiss(
                &ChannelId("1111111111".to_owned()),
                &[MessageId("1000000000000000200".to_owned())],
            )
            .await
            .expect("the v3 table has to exist after the upgrade too");
        // ...and the v4 one. `#39 channel-alias` added the newest rung, and a ladder is only
        // proven at its top.
        store
            .set_channel_alias(&ChannelId("1111111111".to_owned()), "the build channel")
            .await
            .expect("the v4 table has to exist after the upgrade too");
        // And the older table's data is still reachable, which is the half a DROP-and-recreate
        // migration would silently lose.
        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "still here"))
            .await
            .expect("append");
    }

    #[tokio::test]
    async fn a_channel_alias_survives_a_restart_and_replaces_rather_than_accumulates() {
        // `#39 channel-alias`. The point of putting the name in the store instead of in memory:
        // the operator names a channel once and it is still named after a deploy.
        let dir = TempDir::new("sqlite-alias");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        let build = ChannelId("1111111111".to_owned());
        let lead = ChannelId("2222222222".to_owned());
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            store
                .set_channel_alias(&build, "the build channel")
                .await
                .expect("name it");
            store
                .set_channel_alias(&lead, "the team")
                .await
                .expect("name the other one");
            // Renaming REPLACES. Two rows for one channel would make "what is it called" a
            // question with two answers and no tie-break.
            store
                .set_channel_alias(&build, "the noisy one")
                .await
                .expect("rename it");
        }
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        let found = reopened.channel_aliases().await.expect("read back");
        assert_eq!(found.len(), 2, "a rename must replace, not accumulate");
        assert_eq!(found[0].channel, build);
        assert_eq!(found[0].alias, "the noisy one");
        assert_eq!(found[1].channel, lead);
        assert_eq!(found[1].alias, "the team");

        reopened
            .clear_channel_alias(&build)
            .await
            .expect("clear one");
        let error = reopened
            .clear_channel_alias(&build)
            .await
            .expect_err("clearing a name that is gone is not a success");
        assert_eq!(error.code(), "not_found", "{error}");
        let left = reopened.channel_aliases().await.expect("read back");
        assert_eq!(left.len(), 1, "clearing one must not clear the other");
        assert_eq!(left[0].channel, lead);
    }

    #[tokio::test]
    async fn retention_never_renames_a_channel_back_behind_the_operators_back() {
        // Deliberately exempt from `Retention`, and this pins the exemption: an alias collected
        // on age would put the configured label back months later, with nothing on screen saying
        // why. The row is aged a year and every sweep this store runs is provoked.
        let dir = TempDir::new("sqlite-alias-retention");
        let store = store(
            &dir,
            Retention {
                retain_days: 1,
                summary_retain_days: 1,
                max_conversations: 1,
                max_summaries: 1,
                max_dismissals: 1,
                ..Retention::default()
            },
        );
        let channel = ChannelId("1111111111".to_owned());
        store
            .set_channel_alias(&channel, "the build channel")
            .await
            .expect("name it");
        {
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .execute(
                    "UPDATE channel_aliases SET set_at_ms = ?1",
                    [now_ms() - 365 * 86_400_000],
                )
                .expect("age the row");
        }
        // Every kind of write, because each one runs its own sweep and any of them could be the
        // one that reaches this table.
        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "anything"))
            .await
            .expect("append");
        store
            .cache_summary(&summary_key("1000000000000000200", 1), "a summary", None)
            .await
            .expect("cache");
        store
            .dismiss(&channel, &[MessageId("1000000000000000200".to_owned())])
            .await
            .expect("dismiss");
        store
            .mark_read(&channel, &MessageId("1000000000000000200".to_owned()))
            .await
            .expect("mark");

        let found = store.channel_aliases().await.expect("read back");
        assert_eq!(
            found.len(),
            1,
            "no sweep may reach the operator's own names, however old they are"
        );
        assert_eq!(found[0].alias, "the build channel");
    }

    #[tokio::test]
    async fn forgetting_everything_forgets_the_names_too() {
        // The other side of the exemption above: "erase everything" has to mean everything, or
        // the one thing left behind is the one nothing on the screen mentions.
        let dir = TempDir::new("sqlite-alias-purge");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        store
            .set_channel_alias(&channel, "the build channel")
            .await
            .expect("name it");
        store.purge_everything().await.expect("purge");
        assert!(
            store.channel_aliases().await.expect("read back").is_empty(),
            "a purge that leaves the aliases behind has not erased everything"
        );
    }

    #[tokio::test]
    async fn added_channel_provenance_migrates_default_off_and_survives_restart() {
        let dir = TempDir::new("sqlite-added-channel-provenance");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..5] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute(
                    "INSERT INTO added_channels (channel_id, label, writable, added_at_ms)
                     VALUES ('direct-old', 'old direct row', 1, 7)",
                    [],
                )
                .expect("insert old row");
            connection
                .pragma_update(None, "user_version", 5_i64)
                .expect("stamp old schema");
        }

        let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
        let rows = store.added_channels().await.expect("load migrated row");
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].registration_provider.is_none(),
            "pre-managed rows must remain direct"
        );
        store
            .add_channel(
                &ChannelId("managed-new".to_owned()),
                "managed row",
                false,
                Some("https://bridge.example/v1"),
                8,
            )
            .await
            .expect("store managed row");
        drop(store);

        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        let rows = reopened.added_channels().await.expect("reload rows");
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.channel.as_str() == "direct-old"
                    && row.registration_provider.is_none())
        );
        assert!(rows.iter().any(|row| row.channel.as_str() == "managed-new"
            && row.registration_provider.as_deref() == Some("https://bridge.example/v1")));
    }

    #[tokio::test]
    async fn hidden_channels_arrive_on_an_existing_file_and_outlast_a_restart_and_a_purge() {
        // `#199 removable-config-channels`. The file in the wild is a v8 file with an owner's
        // list already in it, so that is what is upgraded here rather than a fresh one.
        let dir = TempDir::new("sqlite-hidden-channels");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..8] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute(
                    "INSERT INTO added_channels
                        (channel_id, label, writable, registration_provider, added_at_ms)
                     VALUES ('added-old', 'added before hiding existed', 0, NULL, 7)",
                    [],
                )
                .expect("insert old row");
            connection
                .pragma_update(None, "user_version", 8_i64)
                .expect("stamp old schema");
        }

        let configured = ChannelId("1111111111".to_owned());
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
            assert!(
                store.hidden_channels().await.expect("read").is_empty(),
                "an upgraded file must hide nothing the owner did not hide"
            );
            assert_eq!(
                store.added_channels().await.expect("read").len(),
                1,
                "the upgrade lost an added channel"
            );
            store.hide_channel(&configured, 10).await.expect("hide");
            store
                .hide_channel(&configured, 20)
                .await
                .expect("hiding twice is not an error");
        }

        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        assert_eq!(
            reopened.hidden_channels().await.expect("reload"),
            vec![configured.clone()],
            "a hidden channel came back after a restart"
        );
        reopened.purge_everything().await.expect("purge");
        assert_eq!(
            reopened.hidden_channels().await.expect("after purge"),
            vec![configured.clone()],
            "the purge reshaped the channel list, which it leaves alone like added channels"
        );
        reopened.unhide_channel(&configured).await.expect("show");
        reopened
            .unhide_channel(&configured)
            .await
            .expect("showing one that is not hidden is not an error");
        assert!(reopened.hidden_channels().await.expect("read").is_empty());
    }

    #[test]
    fn every_migration_applies_to_a_fresh_file_and_leaves_the_version_at_the_end() {
        let dir = TempDir::new("sqlite-migrate");
        let file = dir.path().join("vibe-talk.sqlite3");
        let store = SqliteStore::open(&file, Retention::default()).expect("open");
        let guard = store
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let version: i64 = guard
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(
            usize::try_from(version).expect("non-negative"),
            MIGRATIONS.len(),
            "a fresh file must end at the newest schema version"
        );
    }

    #[tokio::test]
    async fn noise_rules_start_unsaved_survive_a_restart_and_an_empty_list_stays_empty() {
        // `#196 auto-read-noise`. Three states, and the file has to keep them apart: never saved
        // (the default stands in), a saved list, and a saved EMPTY list — the owner turning the
        // feature off, which must not bring the default back on the next start.
        let dir = TempDir::new("sqlite-noise");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            assert_eq!(
                store.noise_rules().await.expect("read"),
                None,
                "a fresh file must say 'never saved', not seed a list a read could not have written"
            );
            let stored = store
                .set_noise_rules(&[
                    "  Working…  ".to_owned(),
                    "working...".to_owned(),
                    "Thinking…".to_owned(),
                ])
                .await
                .expect("save");
            assert_eq!(stored, vec!["Working…".to_owned(), "Thinking…".to_owned()]);
        }
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        assert_eq!(
            reopened.noise_rules().await.expect("read back"),
            Some(vec!["Working…".to_owned(), "Thinking…".to_owned()])
        );
        reopened.set_noise_rules(&[]).await.expect("turn it off");
        drop(reopened);
        let again = SqliteStore::open(&file, Retention::default()).expect("reopen again");
        assert_eq!(again.noise_rules().await.expect("read"), Some(Vec::new()));

        // A list the validator refuses changes nothing at all.
        let error = again
            .set_noise_rules(&["Working…".to_owned(), "*".to_owned()])
            .await
            .expect_err("a bare star would match everything");
        assert_eq!(error.code(), "bad_id", "{error}");
        assert_eq!(again.noise_rules().await.expect("read"), Some(Vec::new()));

        // The purge is the operator's "erase everything", and for this list that is the default.
        again
            .set_noise_rules(&["Working…".to_owned()])
            .await
            .expect("save");
        again.purge_everything().await.expect("purge");
        assert_eq!(again.noise_rules().await.expect("read"), None);
    }

    #[tokio::test]
    async fn a_not_noise_exemption_survives_a_restart_and_is_bounded_like_a_dismissal() {
        let dir = TempDir::new("sqlite-noise-exempt");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        let channel = ChannelId("1111111111".to_owned());
        let other = ChannelId("2222222222".to_owned());
        let retention = Retention {
            max_dismissals: 2,
            ..Retention::default()
        };
        {
            let store = SqliteStore::open(&file, retention).expect("open");
            // Not a snowflake, on purpose: nothing orders these, so nothing requires one.
            let added = store
                .exempt_from_noise(&channel, &[MessageId("spaces/A/messages/B".to_owned())])
                .await
                .expect("exempt");
            assert_eq!(added, 1);
            let again = store
                .exempt_from_noise(&channel, &[MessageId("spaces/A/messages/B".to_owned())])
                .await
                .expect("exempt again");
            assert_eq!(again, 0, "a repeated tap is not a second exemption");
        }
        let store = SqliteStore::open(&file, retention).expect("reopen");
        assert_eq!(
            store.noise_exemptions(&channel).await.expect("read"),
            vec![MessageId("spaces/A/messages/B".to_owned())]
        );
        assert!(
            store
                .noise_exemptions(&other)
                .await
                .expect("read")
                .is_empty(),
            "one channel's exemption leaked into another"
        );
        for id in ["1000000000000000001", "1000000000000000002"] {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            store
                .exempt_from_noise(&channel, &[MessageId(id.to_owned())])
                .await
                .expect("exempt");
        }
        let held = store.noise_exemptions(&channel).await.expect("read");
        assert_eq!(held.len(), 2, "the exemption table is unbounded");
        assert!(
            !held.contains(&MessageId("spaces/A/messages/B".to_owned())),
            "the ceiling evicted the wrong end: the oldest exemption goes first"
        );
        let error = store
            .exempt_from_noise(&channel, &[MessageId(String::new())])
            .await
            .expect_err("an empty id is not a message");
        assert_eq!(error.code(), "bad_id");
        store.purge_everything().await.expect("purge");
        assert!(store
            .noise_exemptions(&channel)
            .await
            .expect("read")
            .is_empty());
    }

    // --- pins, `#206 pin-message` ------------------------------------------------------------

    /// A pin of message `id`, sent at `sent` (ISO-8601).
    fn pinned(id: &str, sent: &str) -> PinSnapshot {
        PinSnapshot {
            message_id: MessageId(id.to_owned()),
            author: "build-bot".to_owned(),
            author_id: UserId("1000000000000000009".to_owned()),
            author_is_bot: true,
            content: format!("message {id}"),
            truncated: false,
            timestamp: sent.to_owned(),
            thread_id: None,
            thread_root: false,
        }
    }

    fn pin_ids(pins: &Pins) -> Vec<&str> {
        pins.pins
            .iter()
            .map(|pin| pin.snapshot.message_id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn a_pin_is_idempotent_both_ways_and_only_a_real_change_moves_the_revision() {
        let dir = TempDir::new("sqlite-pins-idempotent");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        let other = ChannelId("2222222222".to_owned());
        assert_eq!(store.pins_revision(&channel).await.expect("read"), 0);

        let first = store
            .pin(
                &channel,
                &pinned("1000000000000000200", "2026-10-04T12:00:00Z"),
            )
            .await
            .expect("pin");
        assert!(first.changed && first.unpinned.is_empty());
        assert!(first.revision > 0, "a pin left the revision at zero");

        // THE REPEAT: same message, same snapshot. Not an error, and not a change — the revision
        // stays, so no other device is sent to re-read a list that did not move.
        let again = store
            .pin(
                &channel,
                &pinned("1000000000000000200", "2026-10-04T12:00:00Z"),
            )
            .await
            .expect("pin again");
        assert!(!again.changed, "pinning a pinned message reported a change");
        assert_eq!(again.revision, first.revision);
        let at = |change: &PinChange| change.pin.as_ref().map(|pin| pin.pinned_at_ms);
        assert!(at(&first).is_some());
        assert_eq!(
            at(&again),
            at(&first),
            "a repeat moved the pin in the bound's queue"
        );
        assert_eq!(store.pins(&channel).await.expect("read").pins.len(), 1);

        // A second pin after an EDIT refreshes the snapshot, and that is a change.
        let edited = store
            .pin(
                &channel,
                &PinSnapshot {
                    content: "edited since".to_owned(),
                    ..pinned("1000000000000000200", "2026-10-04T12:00:00Z")
                },
            )
            .await
            .expect("pin the edit");
        assert!(edited.changed && edited.revision > first.revision);
        assert_eq!(
            store.pins(&channel).await.expect("read").pins[0]
                .snapshot
                .content,
            "edited since"
        );

        // One channel's pin is not another's.
        assert!(store.pins(&other).await.expect("read").pins.is_empty());
        assert_eq!(store.pins_revision(&other).await.expect("read"), 0);

        let message = MessageId("1000000000000000200".to_owned());
        let off = store.unpin(&channel, &message).await.expect("unpin");
        assert!(off.changed && off.revision > edited.revision && off.pin.is_none());
        let off_again = store.unpin(&channel, &message).await.expect("unpin again");
        assert!(
            !off_again.changed,
            "unpinning an unpinned message reported a change"
        );
        assert_eq!(off_again.revision, off.revision);
        let list = store.pins(&channel).await.expect("read");
        assert!(list.pins.is_empty());
        assert_eq!(
            list.revision, off.revision,
            "the list and its revision disagree"
        );
    }

    #[tokio::test]
    async fn pins_survive_the_store_being_closed_and_reopened_snapshot_and_all() {
        // The property that makes a pin the server's and not a tab's: it is still there after a
        // restart, on whichever device asks.
        let dir = TempDir::new("sqlite-pins-reopen");
        let channel = ChannelId("1111111111".to_owned());
        let threaded = PinSnapshot {
            thread_id: Some("spaces/A/threads/B".to_owned()),
            thread_root: true,
            content: "é".repeat(2_100),
            ..pinned("1000000000000000300", "2026-10-04T13:00:00Z")
        };
        let revision = {
            let store = store(&dir, Retention::default());
            store
                .pin(
                    &channel,
                    &pinned("1000000000000000200", "2026-10-04T12:00:00Z"),
                )
                .await
                .expect("pin");
            store.pin(&channel, &threaded).await.expect("pin").revision
        };
        let reopened = store(&dir, Retention::default());
        let list = reopened.pins(&channel).await.expect("read");
        assert_eq!(
            pin_ids(&list),
            vec!["1000000000000000200", "1000000000000000300"],
            "the pins did not survive a restart"
        );
        assert_eq!(
            list.revision, revision,
            "the revision did not survive a restart"
        );
        let kept = &list.pins[1].snapshot;
        assert_eq!(kept.thread_id.as_deref(), Some("spaces/A/threads/B"));
        assert!(kept.thread_root && kept.author_is_bot);
        assert_eq!(kept.author_id.as_str(), "1000000000000000009");
        assert_eq!(
            kept.content.chars().count(),
            super::super::MAX_PIN_TEXT_CHARS,
            "the store kept more of the text than its bound"
        );
        assert!(kept.truncated);
        assert!(list.pins[1].pinned_at_ms > 0);
    }

    #[tokio::test]
    async fn pins_come_back_in_the_order_their_messages_were_sent_not_the_order_they_were_pinned() {
        let dir = TempDir::new("sqlite-pins-order");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        // Pinned newest-message first; two zones, so a string sort would get it wrong.
        for (id, sent) in [
            ("1000000000000000300", "2026-10-04T15:00:00+02:00"),
            ("1000000000000000100", "2026-10-04T11:00:00Z"),
            ("1000000000000000200", "2026-10-04T12:30:00Z"),
        ] {
            store.pin(&channel, &pinned(id, sent)).await.expect("pin");
        }
        assert_eq!(
            pin_ids(&store.pins(&channel).await.expect("read")),
            vec![
                "1000000000000000100",
                "1000000000000000200",
                "1000000000000000300"
            ]
        );
    }

    #[tokio::test]
    async fn the_pin_bound_drops_the_oldest_pin_in_that_channel_and_says_which() {
        let dir = TempDir::new("sqlite-pins-bound");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        let quiet = ChannelId("2222222222".to_owned());
        store
            .pin(
                &quiet,
                &pinned("1000000000000009999", "2026-10-04T09:00:00Z"),
            )
            .await
            .expect("the quiet channel's one pin");
        // Pinned in order 0..100, so pin 0 is the oldest PIN; its message is the NEWEST message,
        // so a bound that dropped by message time would pick the wrong one.
        let id = |n: usize| format!("{}", 1_000_000_000_000_001_000_u64 + n as u64);
        let sent = |n: usize| format!("2026-10-04T12:{:02}:{:02}Z", 59 - n / 60, 59 - n % 60);
        for n in 0..MAX_PINS_PER_CHANNEL {
            let change = store
                .pin(&channel, &pinned(&id(n), &sent(n)))
                .await
                .expect("pin");
            assert!(
                change.unpinned.is_empty(),
                "pin {n} unpinned something under the bound"
            );
            // The bound orders by `pinned_at_ms`; a gap keeps two pins out of one millisecond.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // A repeat of the oldest pin at the bound: not a new pin, so nothing is dropped, and it
        // does not move to the back of the queue.
        let repeat = store
            .pin(&channel, &pinned(&id(0), &sent(0)))
            .await
            .expect("repeat");
        assert!(
            repeat.unpinned.is_empty(),
            "a repeat at the bound dropped a pin"
        );

        let over = store
            .pin(
                &channel,
                &pinned("1000000000000000001", "2026-10-01T00:00:00Z"),
            )
            .await
            .expect("one past the bound");
        assert_eq!(
            over.unpinned,
            vec![MessageId(id(0))],
            "the bound did not drop exactly the oldest pin"
        );
        let held = store.pins(&channel).await.expect("read");
        assert_eq!(
            held.pins.len(),
            MAX_PINS_PER_CHANNEL,
            "the bound did not hold"
        );
        assert!(
            pin_ids(&held).contains(&"1000000000000000001"),
            "the new pin was dropped"
        );
        assert!(!pin_ids(&held).contains(&id(0).as_str()));
        assert_eq!(
            pin_ids(&store.pins(&quiet).await.expect("read")),
            vec!["1000000000000009999"],
            "a busy channel's bound reached a quiet channel's pin"
        );
    }

    #[tokio::test]
    async fn no_age_limit_unpins_a_message_and_the_purge_erases_every_pin() {
        let dir = TempDir::new("sqlite-pins-age-purge");
        let store = store(
            &dir,
            Retention {
                retain_days: 1,
                summary_retain_days: 1,
                max_summaries: 1,
                ..Retention::default()
            },
        );
        let channel = ChannelId("1111111111".to_owned());
        store
            .pin(
                &channel,
                &pinned("1000000000000000200", "2025-01-01T00:00:00Z"),
            )
            .await
            .expect("pin");
        {
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .execute(
                    "UPDATE pins SET pinned_at_ms = ?1",
                    [now_ms() - 400 * 86_400_000],
                )
                .expect("age the pin");
        }
        // Every write that runs a sweep.
        store
            .append_turn(&id("conv"), &Turn::now(Speaker::You, "anything"))
            .await
            .expect("append");
        store
            .cache_summary(&summary_key("1000000000000000300", 1), "a summary", None)
            .await
            .expect("cache");
        let before = store.pins(&channel).await.expect("read");
        assert_eq!(before.pins.len(), 1, "an age limit unpinned a message");

        store.purge_everything().await.expect("purge");
        let after = store.pins(&channel).await.expect("read");
        assert!(
            after.pins.is_empty(),
            "the purge left a copy of somebody's words behind"
        );
        assert_eq!(after.revision, 0, "the purge left the revision behind");
        let next = store
            .pin(
                &channel,
                &pinned("1000000000000000400", "2026-10-04T12:00:00Z"),
            )
            .await
            .expect("the store still works");
        assert!(
            next.revision > before.revision,
            "a revision handed out before the purge was handed out again for another list"
        );
    }

    #[tokio::test]
    async fn the_pin_tables_arrive_on_a_file_at_the_previous_schema_with_everything_in_it_kept() {
        // The file in the wild is a v9 file holding a real owner's state. Built here with the v9
        // ladder and a row in every table, then opened with today's code.
        let dir = TempDir::new("sqlite-pins-upgrade");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..9] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute_batch(
                    "INSERT INTO conversations VALUES ('conv', 1, 2, 'hello');
                     INSERT INTO turns VALUES ('conv', 1, 'you', 'hello', 1);
                     INSERT INTO read_marks VALUES ('1111111111', '1000000000000000100',
                                                    1000000000000000100, 5);
                     INSERT INTO dismissals VALUES ('1111111111', '1000000000000000150',
                                                    1000000000000000150, 6);
                     INSERT INTO channel_aliases VALUES ('1111111111', 'the build channel', 7);
                     INSERT INTO added_channels VALUES ('added-old', 'added', 0, 8, NULL);
                     INSERT INTO noise_exemptions VALUES ('1111111111', 'spaces/A/messages/B', 9);
                     INSERT INTO hidden_channels VALUES ('3333333333', 10);",
                )
                .expect("an owner's state");
            connection
                .pragma_update(None, "user_version", 9_i64)
                .expect("stamp old schema");
        }

        let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
        {
            let guard = store
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let version: i64 = guard
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("version");
            assert_eq!(version, i64::try_from(MIGRATIONS.len()).expect("small"));
        }
        let channel = ChannelId("1111111111".to_owned());
        assert!(
            store.pins(&channel).await.expect("read").pins.is_empty(),
            "an upgraded file must pin nothing the owner did not pin"
        );
        store
            .pin(
                &channel,
                &pinned("1000000000000000200", "2026-10-04T12:00:00Z"),
            )
            .await
            .expect("the new tables are usable");
        // Everything that was there is still there.
        assert_eq!(store.conversations().await.expect("read").len(), 1);
        assert_eq!(store.read_marks().await.expect("read").len(), 1);
        assert_eq!(
            store.dismissals(&channel).await.expect("read"),
            vec![MessageId("1000000000000000150".to_owned())]
        );
        assert_eq!(store.channel_aliases().await.expect("read").len(), 1);
        assert_eq!(store.added_channels().await.expect("read").len(), 1);
        assert_eq!(
            store.noise_exemptions(&channel).await.expect("read").len(),
            1
        );
        assert_eq!(store.hidden_channels().await.expect("read").len(), 1);
        drop(store);
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        assert_eq!(reopened.pins(&channel).await.expect("read").pins.len(), 1);
    }

    fn clear_attempt(
        id: &str,
        message_ms: i64,
        result: UpstreamClearResult,
        previous: Option<&str>,
    ) -> UpstreamClearAttempt {
        UpstreamClearAttempt {
            at_ms: message_ms + 5_000,
            message: MessageId(id.to_owned()),
            message_ms,
            result,
            target: Some(format!("target-{message_ms}")),
            previous: previous.map(str::to_owned),
            error: (result == UpstreamClearResult::Failed).then(|| "bridge said no".to_owned()),
        }
    }

    #[tokio::test]
    async fn upstream_clear_arrives_on_an_existing_file_outlasts_a_restart_and_a_purge_turns_it_off(
    ) {
        // `#232 upstream-mark-read`. The file in the wild is a v10 file holding an owner's state.
        let dir = TempDir::new("sqlite-upstream-clear");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..10] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute_batch(
                    "INSERT INTO read_marks VALUES ('1111111111', '1000000000000000100',
                                                    1000000000000000100, 5);
                     INSERT INTO hidden_channels VALUES ('3333333333', 10);",
                )
                .expect("an owner's state");
            connection
                .pragma_update(None, "user_version", 10_i64)
                .expect("stamp old schema");
        }
        let channel = ChannelId("1111111111".to_owned());
        let other = ChannelId("2222222222".to_owned());
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
            assert!(
                store
                    .upstream_clear_settings()
                    .await
                    .expect("read")
                    .is_empty(),
                "an upgraded file must clear nothing upstream that the owner did not ask for"
            );
            assert_eq!(store.read_marks().await.expect("read").len(), 1);
            // An attempt never creates a setting.
            store
                .record_upstream_clear(
                    &other,
                    &clear_attempt(
                        "1000000000000000300",
                        3_000,
                        UpstreamClearResult::Advanced,
                        None,
                    ),
                )
                .await
                .expect("record without a setting");
            let set = store
                .set_upstream_clear_mode(&channel, UpstreamClearMode::DryRun, 100)
                .await
                .expect("dry run");
            assert_eq!(set.mode, UpstreamClearMode::DryRun);
            assert_eq!(set.enabled_at_ms, Some(100));
            store
                .record_upstream_clear(
                    &channel,
                    &clear_attempt(
                        "1000000000000000200",
                        2_000,
                        UpstreamClearResult::DryRun,
                        Some("p0"),
                    ),
                )
                .await
                .expect("dry run attempt");
            let on = store
                .set_upstream_clear_mode(&channel, UpstreamClearMode::On, 200)
                .await
                .expect("on");
            assert_eq!(
                on.enabled_at_ms,
                Some(100),
                "dry run to on is not a fresh enable"
            );
            assert_eq!(on.pushed, None, "a dry run pushed nothing");
            for attempt in [
                clear_attempt(
                    "1000000000000000210",
                    2_100,
                    UpstreamClearResult::Failed,
                    None,
                ),
                clear_attempt(
                    "1000000000000000210",
                    2_100,
                    UpstreamClearResult::Failed,
                    None,
                ),
            ] {
                store
                    .record_upstream_clear(&channel, &attempt)
                    .await
                    .expect("failure");
            }
            store
                .record_upstream_clear(
                    &channel,
                    &clear_attempt(
                        "1000000000000000220",
                        2_200,
                        UpstreamClearResult::Advanced,
                        Some("before-any"),
                    ),
                )
                .await
                .expect("advanced");
            // An older attempt arriving late never moves `pushed` back.
            store
                .record_upstream_clear(
                    &channel,
                    &clear_attempt(
                        "1000000000000000215",
                        2_150,
                        UpstreamClearResult::AlreadyRead,
                        Some("target-2200"),
                    ),
                )
                .await
                .expect("late");
        }
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        let settings = reopened.upstream_clear_settings().await.expect("reload");
        assert_eq!(settings.len(), 1, "{settings:?}");
        let setting = &settings[0];
        assert_eq!(setting.channel, channel);
        assert_eq!(setting.mode, UpstreamClearMode::On);
        assert_eq!(setting.failures, 0, "a success resets the failure count");
        let pushed = setting.pushed.as_ref().expect("pushed");
        assert_eq!(pushed.message, MessageId("1000000000000000220".to_owned()));
        assert_eq!(pushed.message_ms, 2_200);
        assert_eq!(pushed.target.as_deref(), Some("target-2200"));
        assert_eq!(pushed.at_ms, 7_200);
        assert_eq!(setting.previous_cursor.as_deref(), Some("before-any"));
        let last = setting.last.as_ref().expect("last");
        assert_eq!(last.result, UpstreamClearResult::AlreadyRead);
        assert_eq!(last.message, MessageId("1000000000000000215".to_owned()));

        // Off keeps what was pushed, so turning it on again cannot move the cursor back.
        let off = reopened
            .set_upstream_clear_mode(&channel, UpstreamClearMode::Off, 300)
            .await
            .expect("off");
        assert_eq!(off.enabled_at_ms, None);
        assert_eq!(off.pushed.as_ref().map(|p| p.message_ms), Some(2_200));

        reopened.purge_everything().await.expect("purge");
        assert!(
            reopened
                .upstream_clear_settings()
                .await
                .expect("after purge")
                .is_empty(),
            "the purge erases the setting, which leaves every channel off"
        );
        assert_eq!(reopened.hidden_channels().await.expect("read").len(), 1);
    }

    #[tokio::test]
    async fn upstream_clear_failures_count_up_and_a_mode_change_resets_them() {
        let dir = TempDir::new("sqlite-upstream-clear-failures");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        store
            .set_upstream_clear_mode(&channel, UpstreamClearMode::On, 1)
            .await
            .expect("on");
        for _ in 0..3 {
            store
                .record_upstream_clear(
                    &channel,
                    &clear_attempt(
                        "1000000000000000200",
                        2_000,
                        UpstreamClearResult::Failed,
                        None,
                    ),
                )
                .await
                .expect("failure");
        }
        let setting = &store.upstream_clear_settings().await.expect("read")[0];
        assert_eq!(setting.failures, 3);
        assert_eq!(
            setting.last.as_ref().and_then(|last| last.error.as_deref()),
            Some("bridge said no")
        );
        assert_eq!(setting.pushed, None);
        // A skipped message neither counts as a failure nor clears the count.
        store
            .record_upstream_clear(
                &channel,
                &clear_attempt(
                    "1000000000000000201",
                    2_001,
                    UpstreamClearResult::Skipped,
                    None,
                ),
            )
            .await
            .expect("skipped");
        assert_eq!(
            store.upstream_clear_settings().await.expect("read")[0].failures,
            3
        );
        let dry = store
            .set_upstream_clear_mode(&channel, UpstreamClearMode::DryRun, 2)
            .await
            .expect("dry run");
        assert_eq!(dry.failures, 0);
    }

    // --- `#240 reading-bookmark` ---------------------------------------------------------------

    fn mark_at(id: &str, place: BookmarkPlace) -> BookmarkAnchor {
        BookmarkAnchor {
            message_id: MessageId(id.to_owned()),
            place,
            timestamp: "2026-10-12T12:00:00Z".to_owned(),
            thread_id: (place == BookmarkPlace::Above).then(|| "spaces/A/threads/B".to_owned()),
        }
    }

    #[tokio::test]
    async fn a_bookmark_survives_a_restart_and_a_cleared_one_keeps_its_revision_growing() {
        let dir = TempDir::new("sqlite-bookmark-restart");
        let channel = ChannelId("1111111111".to_owned());
        let other = ChannelId("2222222222".to_owned());
        let first;
        {
            let store = store(&dir, Retention::default());
            assert_eq!(
                store.bookmark(&channel).await.expect("read"),
                Bookmark::unset()
            );
            let set = store
                .set_bookmark(
                    &channel,
                    Some(&mark_at("1000000000000000100", BookmarkPlace::Above)),
                    None,
                )
                .await
                .expect("set");
            assert!(set.applied && set.changed);
            first = set.bookmark.revision;
            assert!(first > 0);
        }
        let store = store(&dir, Retention::default());
        let held = store
            .bookmark(&channel)
            .await
            .expect("read after a restart");
        assert_eq!(held.revision, first);
        assert_eq!(
            held.anchor,
            Some(mark_at("1000000000000000100", BookmarkPlace::Above)),
            "the anchor came back changed: its thread or its place was lost"
        );
        assert_eq!(
            store.bookmark(&other).await.expect("read"),
            Bookmark::unset(),
            "one channel's bookmark showed up in another"
        );
        let cleared = store
            .set_bookmark(&channel, None, Some(first))
            .await
            .expect("clear");
        assert!(cleared.applied && cleared.changed);
        assert_eq!(cleared.bookmark.anchor, None);
        assert!(cleared.bookmark.revision > first);
        drop(store);
        let store = super::SqliteStore::open(
            &dir.path().join("state").join("vibe-talk.sqlite3"),
            Retention::default(),
        )
        .expect("reopen");
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            cleared.bookmark,
            "a cleared bookmark lost its revision across a restart, so an old page's write would pass"
        );
        let stale = store
            .set_bookmark(
                &channel,
                Some(&mark_at("1000000000000000200", BookmarkPlace::Below)),
                Some(first),
            )
            .await
            .expect("a stale write is an answer, not an error");
        assert!(!stale.applied && !stale.changed);
        assert_eq!(stale.bookmark, cleared.bookmark);
    }

    #[tokio::test]
    async fn the_purge_erases_every_bookmark_and_the_next_revision_starts_past_the_old_ones() {
        let dir = TempDir::new("sqlite-bookmark-purge");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        let set = store
            .set_bookmark(
                &channel,
                Some(&mark_at("1000000000000000100", BookmarkPlace::Above)),
                None,
            )
            .await
            .expect("set");
        store.purge_everything().await.expect("purge");
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            Bookmark::unset()
        );
        let again = store
            .set_bookmark(
                &channel,
                Some(&mark_at("1000000000000000100", BookmarkPlace::Above)),
                None,
            )
            .await
            .expect("set again");
        assert!(
            again.bookmark.revision > set.bookmark.revision,
            "a revision was issued twice across a purge"
        );
    }

    #[tokio::test]
    async fn the_bookmark_table_arrives_on_a_file_at_the_previous_schema_with_everything_kept() {
        let dir = TempDir::new("sqlite-bookmark-migrate");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..11] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute_batch(
                    "INSERT INTO read_marks VALUES ('1111111111', '1000000000000000100',
                                                    1000000000000000100, 5);
                     INSERT INTO dismissals VALUES ('1111111111', '1000000000000000100',
                                                    1000000000000000100, 6);",
                )
                .expect("an owner's state");
            connection
                .pragma_update(None, "user_version", 11_i64)
                .expect("stamp old schema");
        }
        let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
        let channel = ChannelId("1111111111".to_owned());
        assert_eq!(store.read_marks().await.expect("read").len(), 1);
        assert_eq!(store.dismissals(&channel).await.expect("read").len(), 1);
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            Bookmark::unset(),
            "an upgraded file arrived with a bookmark nobody set"
        );
        store
            .set_bookmark(
                &channel,
                Some(&mark_at("1000000000000000100", BookmarkPlace::Above)),
                None,
            )
            .await
            .expect("the new table takes a row");
    }

    #[tokio::test]
    async fn an_anchor_no_provider_could_have_served_is_refused_before_anything_is_written() {
        let dir = TempDir::new("sqlite-bookmark-refuse");
        let store = store(&dir, Retention::default());
        let channel = ChannelId("1111111111".to_owned());
        for (what, broken) in [
            (
                "an empty id",
                BookmarkAnchor {
                    message_id: MessageId(String::new()),
                    ..mark_at("1", BookmarkPlace::Above)
                },
            ),
            (
                "a control character",
                BookmarkAnchor {
                    message_id: MessageId("12\n34".to_owned()),
                    ..mark_at("1", BookmarkPlace::Above)
                },
            ),
            (
                "an oversized timestamp",
                BookmarkAnchor {
                    timestamp: "2".repeat(super::super::MAX_PIN_TIMESTAMP_BYTES + 1),
                    ..mark_at("1", BookmarkPlace::Above)
                },
            ),
            (
                "an empty thread id",
                BookmarkAnchor {
                    thread_id: Some(String::new()),
                    ..mark_at("1", BookmarkPlace::Above)
                },
            ),
        ] {
            let error = store
                .set_bookmark(&channel, Some(&broken), None)
                .await
                .expect_err(what);
            assert_eq!(error.code(), "bad_id", "{what}: {error}");
        }
        assert_eq!(
            store.bookmark(&channel).await.expect("read"),
            Bookmark::unset()
        );
    }

    /// A small deterministic generator, so a property test is the same test on every run and a
    /// failure names the seed and the step that reproduce it.
    struct Steps(u64);

    impl Steps {
        fn next(&mut self) -> u64 {
            // xorshift64*: plenty for choosing operations, and no dependency for it.
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[tokio::test]
    async fn the_bookmark_agrees_with_a_one_slot_model_under_random_writes_and_restarts() {
        // `#240 reading-bookmark`. Property-style: random set, conditional set (with the revision
        // held, and with a stale one) and clear, across two channels, with the store closed and
        // reopened now and then, against the simplest model there is: one optional anchor and one
        // number per channel. The model's rule is written out here rather than borrowed from
        // `decide_bookmark`, so the two have to agree rather than being the same code.
        for seed in [1_u64, 7, 42, 2_026, 99_991] {
            let dir = TempDir::new("sqlite-bookmark-property");
            let path = dir.path().join("state").join("vibe-talk.sqlite3");
            let mut store = SqliteStore::open(&path, Retention::default()).expect("open");
            let channels = [
                ChannelId("1111111111".to_owned()),
                ChannelId("2222222222".to_owned()),
            ];
            let mut model: [(i64, Option<BookmarkAnchor>); 2] = [(0, None), (0, None)];
            let mut steps = Steps(seed);
            for step in 0..300 {
                let which = usize::try_from(steps.below(2)).expect("small");
                let channel = &channels[which];
                let (held_revision, held_anchor) = model[which].clone();
                let anchor = (steps.below(4) != 0).then(|| {
                    mark_at(
                        &format!("10000000000000{:05}", steps.below(6)),
                        if steps.below(3) == 0 {
                            BookmarkPlace::Below
                        } else {
                            BookmarkPlace::Above
                        },
                    )
                });
                let condition = match steps.below(3) {
                    0 => None,
                    1 => Some(held_revision),
                    _ => Some(held_revision - 1 - i64::try_from(steps.below(3)).expect("small")),
                };
                let change = store
                    .set_bookmark(channel, anchor.as_ref(), condition)
                    .await
                    .expect("write");
                let context = format!("seed {seed}, step {step}");
                let refused = condition.is_some_and(|c| c != held_revision);
                assert_eq!(change.applied, !refused, "{context}");
                if refused || anchor == held_anchor {
                    assert!(!change.changed, "{context}: nothing should have changed");
                    assert_eq!(change.bookmark.revision, held_revision, "{context}");
                } else {
                    assert!(change.changed, "{context}");
                    assert!(change.bookmark.revision > held_revision, "{context}");
                    model[which] = (change.bookmark.revision, anchor.clone());
                }
                assert_eq!(change.bookmark.anchor, model[which].1, "{context}");
                if steps.below(25) == 0 {
                    drop(store);
                    store = SqliteStore::open(&path, Retention::default()).expect("reopen");
                }
                for (at, held) in channels.iter().enumerate() {
                    let read = store.bookmark(held).await.expect("read");
                    assert_eq!(
                        (read.revision, read.anchor),
                        model[at].clone(),
                        "{context}: channel {at} disagrees with the model"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn done_marks_agree_with_a_set_model_under_random_batches_and_restarts() {
        // `#240 reading-bookmark` raised one batch to a thousand ids. Property-style: random
        // dismissals and restorations of up to a thousand ids at once, overlapping, across
        // restarts, against a set; each answer's count is how many really changed.
        for seed in [3_u64, 11, 4_096] {
            let dir = TempDir::new("sqlite-dismiss-property");
            let path = dir.path().join("state").join("vibe-talk.sqlite3");
            let mut store = SqliteStore::open(&path, Retention::default()).expect("open");
            let channel = ChannelId("1111111111".to_owned());
            let mut model = std::collections::BTreeSet::new();
            let mut steps = Steps(seed);
            for step in 0..40 {
                let size = if steps.below(5) == 0 {
                    1_000
                } else {
                    1 + steps.below(60)
                };
                let start = steps.below(1_500);
                let ids: Vec<MessageId> = (start..start + size)
                    .map(|n| MessageId(format!("{}", 1_000_000_000_000_000_000_u64 + n)))
                    .collect();
                let context = format!("seed {seed}, step {step}");
                if steps.below(3) == 0 {
                    let expected = ids.iter().filter(|id| model.remove(id.as_str())).count();
                    let removed = store.restore(&channel, &ids).await.expect("restore");
                    assert_eq!(removed, expected as u64, "{context}");
                } else {
                    let expected = ids
                        .iter()
                        .filter(|id| model.insert(id.as_str().to_owned()))
                        .count();
                    let added = store.dismiss(&channel, &ids).await.expect("dismiss");
                    assert_eq!(added, expected as u64, "{context}");
                }
                if steps.below(8) == 0 {
                    drop(store);
                    store = SqliteStore::open(&path, Retention::default()).expect("reopen");
                }
                let held: std::collections::BTreeSet<String> = store
                    .dismissals(&channel)
                    .await
                    .expect("read")
                    .into_iter()
                    .map(|id| id.0)
                    .collect();
                assert_eq!(held, model, "{context}");
            }
        }
    }

    #[test]
    fn a_migration_step_that_fails_leaves_the_file_at_the_version_before_it() {
        // Why `migrate` wraps each step in a transaction with its stamp. Without that, the failure
        // below would leave `half` created under version 1, and every later start would fail on
        // "table half already exists" for a step that never finished.
        let dir = TempDir::new("sqlite-migrate-atomic");
        let connection = Connection::open(dir.path().join("vibe-talk.sqlite3")).expect("open");
        let steps = [
            "CREATE TABLE first (x INTEGER) STRICT;",
            "CREATE TABLE half (x INTEGER) STRICT; CREATE TABLE broken (",
        ];
        let error = migrate_through(&connection, &steps).expect_err("the second step is broken");
        assert!(error.to_string().contains("migration 2 failed"), "{error}");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, 1, "the failed step moved the version");
        let tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            tables,
            vec!["first".to_owned()],
            "half of the failed step was kept"
        );
        // ...and once the step is fixed, the same file takes it.
        migrate_through(
            &connection,
            &[steps[0], "CREATE TABLE half (x INTEGER) STRICT;"],
        )
        .expect("the fixed ladder applies");
    }

    #[tokio::test]
    async fn the_voice_send_choice_starts_unmade_survives_a_restart_and_the_purge_unmakes_it() {
        // `#264 voice-send-approval`. Two states the file must keep apart: never chosen (the
        // configured default applies) and chosen — including chosen to ask, which is the default's
        // value but not "never chosen".
        let dir = TempDir::new("sqlite-voice-settings");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            assert_eq!(store.ask_before_send().await.expect("read"), None);
            assert!(!store.set_ask_before_send(false).await.expect("save"));
        }
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        assert_eq!(
            reopened.ask_before_send().await.expect("read back"),
            Some(false),
            "the owner's choice did not survive a restart"
        );
        assert!(reopened.set_ask_before_send(true).await.expect("save"));
        assert_eq!(reopened.ask_before_send().await.expect("read"), Some(true));
        reopened.purge_everything().await.expect("purge");
        assert_eq!(
            reopened.ask_before_send().await.expect("read"),
            None,
            "the purge left a choice behind instead of restoring the configured default"
        );
    }

    #[tokio::test]
    async fn the_voice_settings_table_arrives_on_a_file_at_the_previous_schema_unchosen() {
        let dir = TempDir::new("sqlite-voice-settings-migrate");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..12] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute_batch(
                    "INSERT INTO read_marks VALUES ('1111111111', '1000000000000000100',
                                                    1000000000000000100, 5);",
                )
                .expect("an owner's state");
            connection
                .pragma_update(None, "user_version", 12_i64)
                .expect("stamp old schema");
        }
        let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
        assert_eq!(store.read_marks().await.expect("read").len(), 1);
        assert_eq!(
            store.ask_before_send().await.expect("read"),
            None,
            "an upgraded file arrived with a choice nobody made"
        );
        assert!(!store.set_ask_before_send(false).await.expect("save"));
    }

    fn edited(text: &str, from_version: u32) -> OwnerPrompt {
        OwnerPrompt {
            instructions: "Call me Sam.\nBe brief.".to_owned(),
            system_prompt: Some(EditedPrompt {
                text: text.to_owned(),
                from_version,
            }),
        }
    }

    #[tokio::test]
    async fn the_owners_prompt_starts_unsaved_survives_a_restart_and_the_purge_forgets_it() {
        // `#272 voice-agent-instructions`. Never saved (the shipped prompt) and saved are two
        // states; so are an edited base and none, and the version an edit was made from is kept
        // with it.
        let dir = TempDir::new("sqlite-voice-prompt");
        let file = dir.path().join("state").join("vibe-talk.sqlite3");
        {
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            assert_eq!(store.owner_prompt().await.expect("read"), None);
            let saved = store
                .set_owner_prompt(&edited("You read me the chat.", 8))
                .await
                .expect("save");
            assert_eq!(saved, edited("You read me the chat.", 8));
        }
        let reopened = SqliteStore::open(&file, Retention::default()).expect("reopen");
        assert_eq!(
            reopened.owner_prompt().await.expect("read back"),
            Some(edited("You read me the chat.", 8)),
            "the owner's words did not survive a restart"
        );
        // Back to the shipped base, keeping his instructions: the edit and its version both go.
        let shipped_base = OwnerPrompt {
            instructions: "Be brief.".to_owned(),
            system_prompt: None,
        };
        reopened
            .set_owner_prompt(&shipped_base)
            .await
            .expect("save");
        assert_eq!(
            reopened.owner_prompt().await.expect("read"),
            Some(shipped_base)
        );
        reopened.purge_everything().await.expect("purge");
        assert_eq!(
            reopened.owner_prompt().await.expect("read"),
            None,
            "the purge left the owner's words behind instead of the shipped prompt"
        );
    }

    #[tokio::test]
    async fn the_voice_prompt_table_arrives_on_a_file_at_the_previous_schema_unsaved() {
        // Found by what it does rather than by its index, so a step landed meanwhile can go before
        // it without this test numbering the ladder.
        let at = MIGRATIONS
            .iter()
            .position(|step| step.contains("CREATE TABLE voice_prompt"))
            .expect("the voice_prompt step");
        let dir = TempDir::new("sqlite-voice-prompt-migrate");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..at] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute_batch("INSERT INTO voice_settings VALUES (1, 0, 5);")
                .expect("an owner's choice");
            connection
                .pragma_update(None, "user_version", i64::try_from(at).expect("small"))
                .expect("stamp old schema");
        }
        let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
        assert_eq!(
            store.ask_before_send().await.expect("read"),
            Some(false),
            "the upgrade lost the send choice"
        );
        assert_eq!(
            store.owner_prompt().await.expect("read"),
            None,
            "an upgraded file arrived with instructions nobody wrote"
        );
        store
            .set_owner_prompt(&edited("Be brief.", PROMPT_VERSION_FOR_TEST))
            .await
            .expect("save");
    }

    /// Any version will do for a stored edit; this one reads as the build's own.
    const PROMPT_VERSION_FOR_TEST: u32 = crate::voice_agent::PROMPT_VERSION;

    #[test]
    fn the_voice_prompt_table_refuses_an_edit_without_its_version() {
        let connection = Connection::open_in_memory().expect("open");
        migrate(&connection).expect("schema");
        for (text, from) in [("'mine'", "NULL"), ("NULL", "8")] {
            assert!(
                connection
                    .execute_batch(&format!(
                        "INSERT INTO voice_prompt VALUES (1, '', {text}, {from}, 5);"
                    ))
                    .is_err(),
                "an edit and its version must be present together ({text}, {from})"
            );
        }
    }

    // --- #208 persistent-cache, milestone summary-keep ---------------------------------------

    /// Where the `cut_at` step sits in the ladder, found by what it does rather than by its index,
    /// so a step landed before it moves nothing here.
    fn cut_at_step() -> usize {
        MIGRATIONS
            .iter()
            .position(|step| step.contains("ADD COLUMN cut_at"))
            .expect("the cut_at step is in the ladder")
    }

    /// Every summary row as `(version, channel, message, summary, cut_at)`, in a stable order.
    fn summary_table(store: &SqliteStore) -> Vec<(String, String, String, String, Option<i64>)> {
        let guard = store
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut statement = guard
            .prepare(
                "SELECT version, channel_id, message_id, summary, cut_at FROM summaries
                 ORDER BY version, channel_id, message_id",
            )
            .expect("prepare");
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("query");
        rows.collect::<Result<_, _>>().expect("rows")
    }

    #[tokio::test]
    async fn the_first_start_re_keys_the_previous_versions_rows_and_marks_the_cut_ones() {
        // A file at the schema before `cut_at`, holding what a deployment really holds: whole
        // summaries, summaries cut with "…", thread sentences (JSON) cut and not, thread names, and
        // rows of an older version that no longer applies.
        let dir = TempDir::new("sqlite-summary-rekey");
        let file = dir.path().join("vibe-talk.sqlite3");
        let previous = "v1-elevenlabs-agent-w3-c160-75ba8b1506ab0af4";
        let older = "v1-elevenlabs-agent-w3-c160-0000000000000000";
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..cut_at_step()] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            let row = |version: &str, message: &str, summary: &str| {
                connection
                    .execute(
                        "INSERT INTO summaries
                         VALUES (?1, '1111111111', ?2, '0000000000000007', ?3, 5)",
                        [version, message, summary],
                    )
                    .expect("a row");
            };
            row(previous, "1000000000000000100", "A whole answer.");
            row(previous, "1000000000000000101", "An answer cut at the cap…");
            row(
                previous,
                "thread-sentence:spaces/A/threads/one",
                r#"{"summary":"A sentence cut at the cap…","at":4}"#,
            );
            row(
                previous,
                "thread-sentence:spaces/A/threads/two",
                r#"{"summary":"A whole sentence.","at":2}"#,
            );
            row(
                KEPT_VERSION_FOR_TESTS,
                "thread-name:spaces/A/threads/one",
                r#"{"name":"runner-wedged","touched":1}"#,
            );
            row(
                older,
                "1000000000000000102",
                "Under a wording that no longer applies.",
            );
            connection
                .pragma_update(
                    None,
                    "user_version",
                    i64::try_from(cut_at_step()).expect("small"),
                )
                .expect("stamp old schema");
        }
        let store = SqliteStore::open(&file, Retention::default()).expect("migrate");
        assert!(
            summary_table(&store).iter().all(|row| row.4.is_none()),
            "the new column reads whole until the re-key says otherwise"
        );

        let current = "v2-elevenlabs-agent-w3-c160-1111111111111111";
        assert_eq!(
            store
                .rekey_summaries(previous, current, 640)
                .await
                .expect("re-key"),
            4,
            "every row of the previous version moves"
        );
        assert_eq!(
            store.forget_summaries_except(current).await.expect("sweep"),
            1,
            "only the row of the older wording is swept"
        );
        let cut_of = |message: &str| {
            summary_table(&store)
                .into_iter()
                .find(|row| row.2 == message)
                .unwrap_or_else(|| panic!("{message} is gone"))
        };
        assert_eq!(cut_of("1000000000000000100").0, current);
        assert_eq!(
            cut_of("1000000000000000100").4,
            None,
            "a whole answer is whole"
        );
        assert_eq!(
            cut_of("1000000000000000101").4,
            Some(640),
            "a cut answer says where"
        );
        assert_eq!(cut_of("thread-sentence:spaces/A/threads/one").4, Some(640));
        assert_eq!(cut_of("thread-sentence:spaces/A/threads/two").4, None);
        let name = cut_of("thread-name:spaces/A/threads/one");
        assert_eq!(
            name.0, KEPT_VERSION_FOR_TESTS,
            "a thread name is not re-keyed"
        );
        assert_eq!(name.4, None);
        assert_eq!(summary_rows(&store), 5);
        // The text itself is untouched, and reachable under the new version.
        let hit = store
            .cached_summary(&SummaryKey {
                channel: ChannelId("1111111111".to_owned()),
                message: MessageId("1000000000000000101".to_owned()),
                content_hash: 7,
                version: current.to_owned(),
            })
            .await
            .expect("read")
            .expect("re-keyed");
        assert_eq!(hit.text, "An answer cut at the cap…");
        assert_eq!(hit.cut_at, Some(640));
        assert_eq!(hit.made_at_ms, 5, "the age is kept, not restarted");

        // Every later start finds nothing to move and nothing to sweep.
        assert_eq!(
            store
                .rekey_summaries(previous, current, 640)
                .await
                .expect("re-key"),
            0
        );
        assert_eq!(
            store.forget_summaries_except(current).await.expect("sweep"),
            0
        );
        assert_eq!(summary_rows(&store), 5);
    }

    #[tokio::test]
    async fn a_file_past_the_voice_settings_step_takes_the_cut_at_step_with_the_choice_kept() {
        // `#264 voice-send-approval` and `#208 persistent-cache` each wrote a step after the
        // bookmarks; the voice-settings one landed first, so the `cut_at` one follows it. A file a
        // deployment already moved past the voice-settings step must take the `cut_at` step and
        // keep the owner's choice, and a fresh file must arrive with both.
        let voice_step = MIGRATIONS
            .iter()
            .position(|step| step.contains("CREATE TABLE voice_settings"))
            .expect("the voice settings step is in the ladder");
        assert!(
            voice_step < cut_at_step(),
            "the cut_at step must follow the voice settings step a deployment may already hold"
        );

        let dir = TempDir::new("sqlite-voice-then-cut-at");
        let file = dir.path().join("vibe-talk.sqlite3");
        {
            let connection = Connection::open(&file).expect("open old database");
            for migration in &MIGRATIONS[..cut_at_step()] {
                connection
                    .execute_batch(migration)
                    .expect("apply old schema");
            }
            connection
                .execute_batch(
                    "INSERT INTO voice_settings VALUES (1, 0, 5);
                     INSERT INTO summaries
                     VALUES ('v1-old', '1111111111', '1000000000000000100', '0000000000000007',
                             'A whole answer.', 5);",
                )
                .expect("an owner's choice and a summary");
            connection
                .pragma_update(
                    None,
                    "user_version",
                    i64::try_from(cut_at_step()).expect("small"),
                )
                .expect("stamp old schema");
        }
        let upgraded = SqliteStore::open(&file, Retention::default()).expect("migrate");
        assert_eq!(
            upgraded.ask_before_send().await.expect("read"),
            Some(false),
            "the cut_at step lost the owner's choice"
        );
        assert_eq!(
            summary_table(&upgraded),
            vec![(
                "v1-old".to_owned(),
                "1111111111".to_owned(),
                "1000000000000000100".to_owned(),
                "A whole answer.".to_owned(),
                None,
            )],
            "a summary filed before the step reads whole"
        );
        {
            let guard = upgraded
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let version: i64 = guard
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("version");
            assert_eq!(version, i64::try_from(MIGRATIONS.len()).expect("small"));
        }

        let fresh_dir = TempDir::new("sqlite-voice-and-cut-at-fresh");
        let fresh = store(&fresh_dir, Retention::default());
        assert_eq!(fresh.ask_before_send().await.expect("read"), None);
        assert!(!fresh.set_ask_before_send(false).await.expect("save"));
        let key = summary_key("1000000000000000100", 7);
        fresh
            .cache_summary(&key, "An answer cut…", Some(640))
            .await
            .expect("file");
        let hit = fresh
            .cached_summary(&key)
            .await
            .expect("read")
            .expect("filed");
        assert_eq!(hit.cut_at, Some(640));
        assert_eq!(fresh.ask_before_send().await.expect("read"), Some(false));
    }

    const KEPT_VERSION_FOR_TESTS: &str = super::super::KEPT_VERSION;

    #[tokio::test]
    async fn a_row_already_filed_under_the_new_version_wins_over_the_old_one() {
        let dir = TempDir::new("sqlite-summary-rekey-collide");
        let store = store(&dir, Retention::default());
        let old = SummaryKey {
            version: "v1-old".to_owned(),
            ..summary_key("1000000000000000100", 7)
        };
        let new = SummaryKey {
            version: "v2-new".to_owned(),
            ..old.clone()
        };
        store.cache_summary(&old, "old…", None).await.expect("file");
        store.cache_summary(&new, "new", None).await.expect("file");
        assert_eq!(
            store
                .rekey_summaries("v1-old", "v2-new", 640)
                .await
                .expect("re-key"),
            0
        );
        assert_eq!(summary_rows(&store), 1);
        let hit = store
            .cached_summary(&new)
            .await
            .expect("read")
            .expect("kept");
        assert_eq!((hit.text.as_str(), hit.cut_at), ("new", None));
    }

    /// Age one row by hand.
    fn age(store: &SqliteStore, message: &str, made_at_ms: i64) {
        let guard = store
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard
            .execute(
                "UPDATE summaries SET made_at_ms = ?1 WHERE message_id = ?2",
                rusqlite::params![made_at_ms, message],
            )
            .expect("age it");
    }

    #[tokio::test]
    async fn keeping_a_summary_alive_rewrites_only_one_more_than_a_day_old() {
        let dir = TempDir::new("sqlite-summary-touch");
        let store = store(&dir, Retention::default());
        let fresh = summary_key("1000000000000000100", 1);
        let stale = summary_key("1000000000000000101", 2);
        store
            .cache_summary(&fresh, "fresh", None)
            .await
            .expect("file");
        store
            .cache_summary(&stale, "stale", None)
            .await
            .expect("file");
        let hour_ago = now_ms() - 3_600_000;
        let two_days_ago = now_ms() - 2 * 86_400_000;
        age(&store, "1000000000000000100", hour_ago);
        age(&store, "1000000000000000101", two_days_ago);

        assert!(!store.touch_summary(&fresh).await.expect("touch"));
        assert!(store.touch_summary(&stale).await.expect("touch"));
        let made = |key: &SummaryKey| {
            let store = &store;
            let key = key.clone();
            async move {
                store
                    .cached_summary(&key)
                    .await
                    .expect("read")
                    .expect("there")
                    .made_at_ms
            }
        };
        assert_eq!(
            made(&fresh).await,
            hour_ago,
            "a row used within the day is not rewritten"
        );
        assert!(
            made(&stale).await > now_ms() - 60_000,
            "an old row in use is kept alive"
        );
        assert!(
            !store
                .touch_summary(&summary_key("1000000000000000199", 3))
                .await
                .expect("touch"),
            "nothing is written for a key with no entry"
        );
        assert_eq!(summary_rows(&store), 2);
    }

    #[tokio::test]
    async fn the_summary_age_limit_is_its_own_setting_and_counts_from_the_last_use() {
        let dir = TempDir::new("sqlite-summary-retain");
        let store = store(
            &dir,
            Retention {
                retain_days: 365,
                summary_retain_days: 2,
                ..Retention::default()
            },
        );
        let used = summary_key("1000000000000000100", 1);
        let unused = summary_key("1000000000000000101", 2);
        store
            .cache_summary(&used, "used", None)
            .await
            .expect("file");
        store
            .cache_summary(&unused, "unused", None)
            .await
            .expect("file");
        let three_days_ago = now_ms() - 3 * 86_400_000;
        age(&store, "1000000000000000100", three_days_ago);
        age(&store, "1000000000000000101", three_days_ago);
        // A write-scope hit keeps the used one alive; the next write prunes the other.
        assert!(store.touch_summary(&used).await.expect("touch"));
        store
            .cache_summary(&summary_key("1000000000000000102", 3), "new", None)
            .await
            .expect("file");
        assert!(store.cached_summary(&used).await.expect("read").is_some());
        assert_eq!(
            store.cached_summary(&unused).await.expect("read"),
            None,
            "past summary_retain_days, though well within retain_days"
        );
    }

    #[tokio::test]
    async fn taking_a_channel_off_the_list_forgets_its_summaries_and_keeps_its_thread_names() {
        let dir = TempDir::new("sqlite-summary-forget-channel");
        let store = store(&dir, Retention::default());
        let elsewhere = SummaryKey {
            channel: ChannelId("2222222222".to_owned()),
            ..summary_key("1000000000000000100", 1)
        };
        let name = SummaryKey {
            version: KEPT_VERSION_FOR_TESTS.to_owned(),
            ..summary_key("thread-name:t", 0)
        };
        let sentence = summary_key("thread-sentence:t", 0);
        let older = SummaryKey {
            version: "an-older-version".to_owned(),
            ..summary_key("1000000000000000101", 2)
        };
        for key in [
            &summary_key("1000000000000000100", 1),
            &sentence,
            &older,
            &name,
            &elsewhere,
        ] {
            store.cache_summary(key, "text", None).await.expect("file");
        }
        assert_eq!(
            store
                .forget_channel_summaries(&ChannelId("1111111111".to_owned()))
                .await
                .expect("forget"),
            3,
            "the summary, the sentence and the row of an older version"
        );
        assert!(
            store.cached_summary(&name).await.expect("read").is_some(),
            "the name is kept"
        );
        assert!(
            store
                .cached_summary(&elsewhere)
                .await
                .expect("read")
                .is_some(),
            "another channel is untouched"
        );
        assert_eq!(summary_rows(&store), 2);
    }

    #[tokio::test]
    async fn the_purge_leaves_no_copy_of_a_summary_in_the_file() {
        // `secure_delete`: what is deleted is overwritten, not left in a free page for anyone who
        // reads the file. THE CONTROL first: the same steps on a connection with it turned off
        // leave the text in the file, so the assertion after it can fail.
        let unique = "a-summary-nobody-else-would-write-7f3a9c";
        for secure in [false, true] {
            let dir = TempDir::new("sqlite-secure-delete");
            let file = dir.path().join("vibe-talk.sqlite3");
            let store = SqliteStore::open(&file, Retention::default()).expect("open");
            if !secure {
                store
                    .connection
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pragma_update(None, "secure_delete", "OFF")
                    .expect("turn it off");
            }
            store
                .cache_summary(&summary_key("1000000000000000100", 1), unique, None)
                .await
                .expect("file");
            store.purge_everything().await.expect("purge");
            drop(store);
            let bytes = std::fs::read(&file).expect("read the file");
            let present = bytes
                .windows(unique.len())
                .any(|window| window == unique.as_bytes());
            assert_eq!(
                present,
                !secure,
                "secure_delete {secure}: the purged summary is {} the file",
                if present { "still in" } else { "absent from" }
            );
        }
    }

    #[tokio::test]
    async fn the_fake_store_agrees_with_the_real_one_on_the_summary_methods() {
        // One script, both stores, the same answers: a test that passes against the fake must mean
        // the same thing against the file a deployment runs.
        async fn script(store: &dyn StateStore) -> Vec<String> {
            let mut said = Vec::new();
            let key = |message: &str, version: &str| SummaryKey {
                version: version.to_owned(),
                ..summary_key(message, 7)
            };
            let sentence = r#"{"summary":"cut…","at":1}"#;
            for (message, version, text) in [
                ("1000000000000000100", "v1", "whole"),
                ("1000000000000000101", "v1", "cut…"),
                ("thread-sentence:t", "v1", sentence),
                ("thread-name:t", KEPT_VERSION_FOR_TESTS, "{}"),
                ("1000000000000000102", "v0", "older"),
            ] {
                store
                    .cache_summary(&key(message, version), text, None)
                    .await
                    .expect("file");
            }
            said.push(format!(
                "{:?}",
                store.rekey_summaries("v1", "v2", 640).await
            ));
            said.push(format!(
                "{:?}",
                store.rekey_summaries("v1", "v2", 640).await
            ));
            said.push(format!("{:?}", store.forget_summaries_except("v2").await));
            for (message, version) in [
                ("1000000000000000100", "v2"),
                ("1000000000000000101", "v2"),
                ("thread-sentence:t", "v2"),
                ("thread-name:t", KEPT_VERSION_FOR_TESTS),
                ("1000000000000000100", "v1"),
            ] {
                let hit = store
                    .cached_summary(&key(message, version))
                    .await
                    .expect("read");
                said.push(format!(
                    "{message} {version} {:?}",
                    hit.map(|h| (h.text, h.cut_at))
                ));
            }
            said.push(format!(
                "{:?}",
                store.touch_summary(&key("1000000000000000100", "v2")).await
            ));
            said.push(format!(
                "{:?}",
                store
                    .forget_channel_summaries(&ChannelId("1111111111".to_owned()))
                    .await
            ));
            said.push(format!(
                "{:?}",
                store
                    .cached_summary(&key("thread-name:t", KEPT_VERSION_FOR_TESTS))
                    .await
                    .map(|hit| hit.map(|h| h.text))
            ));
            said
        }
        let dir = TempDir::new("sqlite-summary-parity");
        let real = store(&dir, Retention::default());
        let fake = crate::store::fake::FakeStore::new();
        assert_eq!(script(&real).await, script(&fake).await);
        // And the store with nothing configured refuses every one of them by name.
        let disabled = crate::store::disabled::DisabledStore;
        let key = summary_key("1000000000000000100", 7);
        assert!(matches!(
            disabled.touch_summary(&key).await,
            Err(StoreError::Unavailable(_))
        ));
        assert!(matches!(
            disabled.rekey_summaries("v1", "v2", 640).await,
            Err(StoreError::Unavailable(_))
        ));
        assert!(matches!(
            disabled
                .forget_channel_summaries(&ChannelId("1111111111".to_owned()))
                .await,
            Err(StoreError::Unavailable(_))
        ));
    }
}
