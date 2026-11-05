//! The cache file's schema, its pragmas, the capability check, and how a file is recognised as
//! ours.
//!
//! # Settings made once, and settings made on every open
//!
//! `application_id`, `auto_vacuum` and `journal_mode = WAL` live in the file and are set once, at
//! creation, before the first table. Everything else in [`PER_OPEN`] belongs to a CONNECTION and
//! is forgotten when it closes, so it is set on every open of every connection, writer and readers
//! alike. `secure_delete` matters most: the bundled SQLite is not built with it on, so without
//! this, deletes stop zeroing pages after the first restart.
//!
//! # Migrations are additive
//!
//! `user_version` counts the steps applied. A step adds tables, columns or indexes; an index
//! change is a rebuild from the content table, which needs no refetch. The file is rebuilt only
//! when `meta.schema_compat` says this code cannot use it: a refill takes days on a provider that
//! returns twenty rows a call.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension as _};

/// `application_id`: the bytes `vtMC` at offset 68 of the file header.
pub const APPLICATION_ID: i32 = 0x7674_4d43;
/// The header bytes [`APPLICATION_ID`] is stored as.
pub const APPLICATION_ID_BYTES: [u8; 4] = *b"vtMC";
/// The oldest linked SQLite the cache will run on: FTS5 `secure-delete` arrived in 3.42.
pub const MIN_SQLITE_VERSION: i32 = 3_042_000;

/// Connection settings made on every open, in this order.
pub const PER_OPEN: &[(&str, &str)] = &[
    ("secure_delete", "ON"),
    ("synchronous", "NORMAL"),
    ("journal_size_limit", "4194304"),
    ("busy_timeout", "2000"),
    ("temp_store", "MEMORY"),
    ("foreign_keys", "ON"),
];

/// One migration step: its SQL, and the oldest code schema that can still use a file once it is
/// applied (equal to the step's own number for an additive step that older code may ignore... and
/// raised by an incompatible one).
#[derive(Clone, Copy, Debug)]
pub struct Step {
    /// The statements, run in one transaction with the version stamp.
    pub sql: &'static str,
    /// The `meta.schema_compat` the file carries once this step is applied.
    pub compat: i64,
}

/// Cache schema 1.
const V1: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) STRICT;

CREATE TABLE account (
    provider_key TEXT PRIMARY KEY NOT NULL,
    account_id   TEXT NOT NULL,
    workspace_id TEXT,
    verified_ms  INTEGER NOT NULL
) STRICT;

CREATE TABLE channel (
    chan         INTEGER PRIMARY KEY,
    provider_key TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    UNIQUE (provider_key, channel_id)
) STRICT;

CREATE TABLE message (
    row_id          INTEGER PRIMARY KEY,
    chan            INTEGER NOT NULL REFERENCES channel (chan) ON DELETE CASCADE,
    message_id      TEXT    NOT NULL,
    numeric_id      INTEGER,
    created_ms      INTEGER NOT NULL,
    edited_ms       INTEGER,
    observed_ms     INTEGER NOT NULL,
    home_scope      TEXT    NOT NULL CHECK (home_scope IN ('main', 'flat', 'thread')),
    thread_id       TEXT,
    is_root         INTEGER NOT NULL DEFAULT 0 CHECK (is_root IN (0, 1)),
    reply_to        TEXT,
    author_id       TEXT    NOT NULL,
    author          TEXT    NOT NULL,
    author_is_bot   INTEGER NOT NULL CHECK (author_is_bot IN (0, 1)),
    content         TEXT    NOT NULL,
    content_hash    TEXT    NOT NULL,
    content_partial INTEGER NOT NULL DEFAULT 0 CHECK (content_partial IN (0, 1)),
    search_text     TEXT    NOT NULL,
    extra           TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (chan, message_id)
) STRICT;
CREATE INDEX message_by_time   ON message (chan, created_ms, numeric_id);
CREATE INDEX message_by_thread ON message (chan, thread_id, created_ms) WHERE thread_id IS NOT NULL;
CREATE INDEX message_by_age    ON message (created_ms);

CREATE TABLE tombstone (
    chan       INTEGER NOT NULL REFERENCES channel (chan) ON DELETE CASCADE,
    message_id TEXT    NOT NULL,
    created_ms INTEGER NOT NULL,
    deleted_ms INTEGER NOT NULL,
    PRIMARY KEY (chan, message_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX tombstone_by_age ON tombstone (deleted_ms);

CREATE TABLE message_scope (
    chan       INTEGER NOT NULL,
    scope_key  TEXT    NOT NULL,
    created_ms INTEGER NOT NULL,
    row_id     INTEGER NOT NULL REFERENCES message (row_id) ON DELETE CASCADE,
    PRIMARY KEY (chan, scope_key, created_ms, row_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX message_scope_by_row ON message_scope (row_id);

CREATE TABLE coverage (
    chan      INTEGER NOT NULL REFERENCES channel (chan) ON DELETE CASCADE,
    scope_key TEXT    NOT NULL,
    t0_ms     INTEGER NOT NULL,
    t1_ms     INTEGER NOT NULL,
    listed_ms INTEGER NOT NULL,
    full_ms   INTEGER,
    complete  INTEGER NOT NULL CHECK (complete IN (0, 1)),
    source    TEXT    NOT NULL CHECK (source IN ('read', 'poll', 'walk', 'range', 'refresh')),
    PRIMARY KEY (chan, scope_key, t0_ms),
    CHECK (t0_ms < t1_ms),
    CHECK (full_ms IS NULL OR full_ms <= listed_ms)
) STRICT;

CREATE TABLE scope_head (
    chan              INTEGER NOT NULL REFERENCES channel (chan) ON DELETE CASCADE,
    scope_key         TEXT    NOT NULL,
    head_ms           INTEGER NOT NULL,
    unsettled_from_ms INTEGER NOT NULL,
    head_listed_ms    INTEGER NOT NULL,
    PRIMARY KEY (chan, scope_key)
) STRICT;

CREATE TABLE thread (
    chan              INTEGER NOT NULL REFERENCES channel (chan) ON DELETE CASCADE,
    thread_id         TEXT    NOT NULL,
    root_id           TEXT,
    reply_count       INTEGER,
    reply_count_exact INTEGER NOT NULL DEFAULT 0 CHECK (reply_count_exact IN (0, 1)),
    last_activity_ms  INTEGER NOT NULL,
    verified_ms       INTEGER NOT NULL,
    PRIMARY KEY (chan, thread_id)
) STRICT;
CREATE INDEX thread_by_activity ON thread (chan, last_activity_ms);

CREATE VIRTUAL TABLE message_fts USING fts5(
    search_text, content = 'message', content_rowid = 'row_id', tokenize = 'trigram', detail = full
);
INSERT INTO message_fts (message_fts, rank) VALUES ('secure-delete', 1);
CREATE VIRTUAL TABLE message_fts_vocab USING fts5vocab(message_fts, row);
CREATE TRIGGER message_fts_ai AFTER INSERT ON message BEGIN
    INSERT INTO message_fts (rowid, search_text) VALUES (new.row_id, new.search_text);
END;
CREATE TRIGGER message_fts_ad AFTER DELETE ON message BEGIN
    INSERT INTO message_fts (message_fts, rowid, search_text) VALUES ('delete', old.row_id, old.search_text);
END;
CREATE TRIGGER message_fts_au AFTER UPDATE OF search_text ON message BEGIN
    INSERT INTO message_fts (message_fts, rowid, search_text) VALUES ('delete', old.row_id, old.search_text);
    INSERT INTO message_fts (rowid, search_text) VALUES (new.row_id, new.search_text);
END;
";

/// Every step this code knows, in order. Step `n` (from 1) brings a file to `user_version = n`.
pub const STEPS: &[Step] = &[Step { sql: V1, compat: 1 }];

/// Why a file cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The file belongs to something else. It is left exactly as it is.
    Foreign(String),
    /// The file is ours but this code cannot use it: rebuild it.
    Rebuild(String),
    /// SQLite failed in a way that says nothing about the file.
    Sqlite(String),
}

/// What the first bytes of a file say about whose it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    /// Empty: a file this code pre-created and never got further with.
    Empty,
    /// A SQLite file whose header carries [`APPLICATION_ID_BYTES`].
    Ours,
    /// A SQLite file with no application id: ours only if it holds no schema either.
    Unmarked,
    /// Anything else.
    Foreign,
}

/// Read the header of the file at `path`. Works on a corrupt database, because it does not ask
/// SQLite.
///
/// # Errors
///
/// When the file cannot be read.
pub fn read_header(path: &Path) -> std::io::Result<Header> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut bytes = [0_u8; 100];
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(classify_header(&bytes[..filled]))
}

/// [`read_header`] on bytes already read.
#[must_use]
pub fn classify_header(bytes: &[u8]) -> Header {
    if bytes.is_empty() {
        return Header::Empty;
    }
    if bytes.len() < 100 || &bytes[..16] != b"SQLite format 3\0" {
        return Header::Foreign;
    }
    match &bytes[68..72] {
        id if id == APPLICATION_ID_BYTES => Header::Ours,
        [0, 0, 0, 0] => Header::Unmarked,
        _ => Header::Foreign,
    }
}

/// Apply [`PER_OPEN`] to a connection.
///
/// # Errors
///
/// When SQLite refuses one.
pub fn apply_per_open(connection: &Connection) -> rusqlite::Result<()> {
    for (name, value) in PER_OPEN {
        // `busy_timeout` and `journal_size_limit` answer with a row; the rest answer nothing.
        // Reading through `pragma_update_and_check` would refuse the silent ones, so issue each
        // as a query and drain whatever comes back.
        let mut statement = connection.prepare(&format!("PRAGMA {name} = {value}"))?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
    }
    Ok(())
}

/// The value of one pragma, as text, for the per-open assertion.
///
/// # Errors
///
/// When SQLite refuses the query.
pub fn pragma_text(connection: &Connection, name: &str) -> rusqlite::Result<String> {
    connection.query_row(&format!("PRAGMA {name}"), [], |row| {
        let value: rusqlite::types::Value = row.get(0)?;
        Ok(match value {
            rusqlite::types::Value::Integer(i) => i.to_string(),
            rusqlite::types::Value::Text(t) => t,
            other => format!("{other:?}"),
        })
    })
}

/// The linked SQLite can do everything the cache needs: a recent enough version, FTS5, the
/// trigram tokenizer, and FTS5 `secure-delete`. Checked at every open, in a private in-memory
/// database, because a build that stopped bundling SQLite would lose them silently.
///
/// # Errors
///
/// A sentence saying what is missing.
pub fn capability_check() -> Result<(), String> {
    let connection = Connection::open_in_memory().map_err(|e| format!("no SQLite: {e}"))?;
    let version: i32 = connection
        .query_row("SELECT sqlite_version()", [], |row| row.get::<_, String>(0))
        .map(|text| version_number(&text))
        .map_err(|e| format!("cannot read the SQLite version: {e}"))?;
    if version < MIN_SQLITE_VERSION {
        return Err(format!(
            "the linked SQLite is {version}; the message cache needs 3.42 or newer"
        ));
    }
    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE temp.capability USING fts5(x, tokenize = 'trigram', detail = full);
             INSERT INTO temp.capability (capability, rank) VALUES ('secure-delete', 1);
             INSERT INTO temp.capability (x) VALUES ('capability check');
             DELETE FROM temp.capability;
             DROP TABLE temp.capability;",
        )
        .map_err(|e| {
            format!(
                "the linked SQLite lacks FTS5, its trigram tokenizer or secure-delete ({})",
                e.sqlite_error_code()
                    .map_or_else(|| "unknown".to_owned(), |c| format!("{c:?}"))
            )
        })
}

/// `3.53.2` as `3053002`, the form `SQLITE_VERSION_NUMBER` uses.
fn version_number(text: &str) -> i32 {
    let mut parts = text.split('.').map(|p| p.parse::<i32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    major * 1_000_000 + minor * 1_000 + patch
}

/// Whether the database has any schema at all.
fn has_schema(connection: &Connection) -> rusqlite::Result<bool> {
    connection
        .query_row("SELECT 1 FROM sqlite_schema LIMIT 1", [], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
}

/// Prepare a freshly opened writer connection: recognise the file, make the creation-time
/// settings on a new one, apply [`PER_OPEN`], and bring the schema up to date with `steps`.
///
/// # Errors
///
/// [`Refusal::Foreign`] for a file that is not ours, [`Refusal::Rebuild`] for one of ours this code
/// cannot use, [`Refusal::Sqlite`] otherwise.
pub fn prepare_writer(
    connection: &Connection,
    header: Header,
    steps: &[Step],
) -> Result<(), Refusal> {
    let sqlite = |e: rusqlite::Error| Refusal::Sqlite(sqlite_code(&e));
    let unreadable =
        |e: rusqlite::Error| Refusal::Rebuild(format!("unreadable: {}", sqlite_code(&e)));
    match header {
        Header::Foreign => {
            return Err(Refusal::Foreign(
                "the file is not a message cache this server made".to_owned(),
            ))
        }
        Header::Unmarked => {
            if has_schema(connection).map_err(sqlite)? {
                return Err(Refusal::Foreign(
                    "the file is a SQLite database that holds something else".to_owned(),
                ));
            }
        }
        Header::Empty | Header::Ours => {}
    }
    let fresh = header != Header::Ours || !has_schema(connection).map_err(unreadable)?;
    if fresh {
        // Before the first table, and `auto_vacuum` before anything writes the header: once the
        // first page exists its vacuum mode is fixed, so a half-created file that already has
        // one is vacuumed (empty, so instantly) to take the new mode.
        let started = header != Header::Empty;
        if started {
            let _: String = connection
                .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
                .map_err(sqlite)?;
        }
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(sqlite)?;
        if started {
            connection.execute_batch("VACUUM").map_err(sqlite)?;
        }
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(sqlite)?;
    }
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(sqlite)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Refusal::Sqlite(format!("journal mode stayed {mode}")));
    }
    apply_per_open(connection).map_err(sqlite)?;
    migrate(connection, steps)
}

/// Bring the schema up to `steps.len()`, one transaction per step, or refuse the file.
///
/// # Errors
///
/// [`Refusal::Rebuild`] when the file is from code whose changes this code cannot use, or a step
/// fails on it.
pub fn migrate(connection: &Connection, steps: &[Step]) -> Result<(), Refusal> {
    let unreadable =
        |e: rusqlite::Error| Refusal::Rebuild(format!("unreadable: {}", sqlite_code(&e)));
    let applied: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(unreadable)?;
    let known = i64::try_from(steps.len()).unwrap_or(i64::MAX);
    if applied > known {
        // A newer build's file. Its additive steps are harmless to ignore; its compat says so.
        let compat = schema_compat(connection).map_err(unreadable)?;
        return if compat.is_some_and(|c| c <= known) {
            Ok(())
        } else {
            Err(Refusal::Rebuild(format!(
                "schema {applied} needs code schema {} or newer; this is {known}",
                compat.map_or_else(|| "unknown".to_owned(), |c| c.to_string())
            )))
        };
    }
    let start = usize::try_from(applied).unwrap_or(0);
    for (index, step) in steps.iter().enumerate().skip(start) {
        let failed = |e: rusqlite::Error| {
            Refusal::Rebuild(format!(
                "migration {} failed: {}",
                index + 1,
                sqlite_code(&e)
            ))
        };
        let version = i64::try_from(index + 1).unwrap_or(i64::MAX);
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(failed)?;
        let result = (|| {
            connection.execute_batch(step.sql)?;
            connection.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_compat', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [step.compat.to_string()],
            )?;
            if index == 0 {
                connection.execute(
                    "INSERT INTO meta (key, value) VALUES ('created_ms', ?1)
                     ON CONFLICT (key) DO NOTHING",
                    [crate::cache::wall_ms().to_string()],
                )?;
            }
            connection.pragma_update(None, "user_version", version)
        })();
        match result {
            Ok(()) => connection.execute_batch("COMMIT").map_err(failed)?,
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(failed(error));
            }
        }
    }
    Ok(())
}

fn schema_compat(connection: &Connection) -> rusqlite::Result<Option<i64>> {
    connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_compat'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map(|value| value.and_then(|v| v.parse().ok()))
}

/// A SQLite error as its result code only. FTS errors quote the query and constraint errors can
/// quote values, so the message itself is never logged or returned from this module.
#[must_use]
pub fn sqlite_code(error: &rusqlite::Error) -> String {
    match error.sqlite_error_code() {
        Some(code) => format!("sqlite {code:?}"),
        None => match error {
            rusqlite::Error::QueryReturnedNoRows => "no rows".to_owned(),
            rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::FromSqlConversionFailure(..) => "unexpected column type".to_owned(),
            _ => "sqlite error".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linked_sqlite_has_fts5_trigram_and_secure_delete() {
        // `build.rs` honours a `LIBSQLITE3_FLAGS` override, and a move away from the bundled
        // build would silently lose these. This is the unit test that says so.
        capability_check().expect("the bundled SQLite must carry everything the cache needs");
        assert!(version_number("3.53.2") > MIN_SQLITE_VERSION);
        assert_eq!(version_number("3.42.0"), MIN_SQLITE_VERSION);
    }

    #[test]
    fn headers_are_told_apart_without_asking_sqlite() {
        assert_eq!(classify_header(&[]), Header::Empty);
        assert_eq!(classify_header(b"hello"), Header::Foreign);
        let mut page = [0_u8; 100];
        page[..16].copy_from_slice(b"SQLite format 3\0");
        assert_eq!(classify_header(&page), Header::Unmarked);
        page[68..72].copy_from_slice(&APPLICATION_ID_BYTES);
        assert_eq!(classify_header(&page), Header::Ours);
        page[68..72].copy_from_slice(b"else");
        assert_eq!(classify_header(&page), Header::Foreign);
        assert_eq!(APPLICATION_ID.to_be_bytes(), APPLICATION_ID_BYTES);
    }

    #[test]
    fn every_table_is_strict() {
        let connection = Connection::open_in_memory().expect("memory");
        connection.execute_batch(V1).expect("schema");
        let mut statement = connection
            .prepare("SELECT name, strict FROM pragma_table_list WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'message_fts%'")
            .expect("list");
        let tables: Vec<(String, i64)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(tables.len(), 9, "{tables:?}");
        for (name, strict) in tables {
            assert_eq!(strict, 1, "{name} is not STRICT");
        }
    }
}
