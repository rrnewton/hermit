//! Behaviour tests for the cache as a whole: the file, the writer, the merge rules applied, the
//! coverage stamps, the erase protocol and the bounds. `#208 persistent-cache`, milestone
//! `cache-store`.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicI64;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::coverage::{Interval, Source, SETTLE_AFTER_LOCAL_MS, SETTLE_MS};
use super::fixtures::{self, message, newest_proof, reply, Fixture, ManualClock, T0};
use super::*;
use crate::model::{Message, Reaction};
use crate::threads::MessageThread;

pub(super) const STEP: i64 = 600_000;

/// Message `i`: created `i` steps of ten minutes after the origin.
pub(super) fn m(i: i64) -> Message {
    message(
        &format!("m{i}"),
        "ada",
        &format!("body number {i}"),
        T0 + i * STEP,
    )
}

pub(super) fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

pub(super) fn mid(id: &str) -> MessageId {
    MessageId(id.to_owned())
}

pub(super) fn push(fx: &Fixture, message: Message, update: bool, observed_ms: i64) {
    fx.offer(
        &fx.chan,
        Observation::Push {
            push: Push::Upsert {
                message: Box::new(Incoming::new(message, Scope::Main)),
                update,
                partial: false,
            },
            observed_ms,
        },
    );
}

pub(super) fn push_delete(fx: &Fixture, id: &str, observed_ms: i64) {
    fx.offer(
        &fx.chan,
        Observation::Push {
            push: Push::Delete {
                id: mid(id),
                created_ms: None,
            },
            observed_ms,
        },
    );
}

pub(super) fn held(fx: &Fixture, id: &str) -> Option<CachedMessage> {
    fx.cache
        .message(&fx.chan, &mid(id))
        .served()
        .expect("served")
}

pub(super) fn content(fx: &Fixture, id: &str) -> Option<String> {
    held(fx, id).map(|m| m.message.content)
}

pub(super) fn intervals(fx: &Fixture, scope: &Scope) -> Vec<Interval> {
    fx.cache.coverage(&fx.chan, scope).served().expect("served")
}

pub(super) fn sql_count(cache: &MessageCache, sql: &str) -> i64 {
    cache
        .read(None, |c| c.query_row(sql, [], |r| r.get(0)))
        .served()
        .expect("served")
}

/// The FTS integrity check against the content table, on a private connection: the readers are
/// query-only.
pub(super) fn integrity(fx: &Fixture) {
    let connection = rusqlite::Connection::open(fx.path()).expect("open");
    connection
        .execute(
            "INSERT INTO message_fts (message_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .expect("the full-text index agrees with the message table");
}

// ---- the file ----------------------------------------------------------------------------------

#[test]
fn every_connection_has_its_pragmas_after_a_close_and_a_reopen() {
    let dir = crate::testing::TempDir::new("cache-pragmas");
    let clock = Arc::new(ManualClock(AtomicI64::new(T0)));
    let expected: Vec<(String, String)> = [
        ("secure_delete", "1"),
        ("synchronous", "1"),
        ("journal_size_limit", "4194304"),
        ("busy_timeout", "2000"),
        ("temp_store", "2"),
        ("foreign_keys", "1"),
        ("journal_mode", "wal"),
        ("application_id", "1987333443"),
        ("auto_vacuum", "2"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
    .collect();
    for _ in 0..2 {
        let cache = fixtures::open(&dir, &clock, Limits::default());
        let probed = cache.probe_pragmas();
        assert_eq!(
            probed.len(),
            4,
            "the writer, the checkpointer and both readers"
        );
        for (connection, pragmas) in probed.iter().enumerate() {
            assert_eq!(pragmas, &expected, "connection {connection}");
        }
        drop(cache);
    }
}

#[cfg(unix)]
#[test]
fn the_file_and_its_wal_and_shm_are_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new("cache-modes");
    fx.list(vec![m(1)], T0, None);
    for member in file::family(&fx.path()).iter().take(3) {
        let mode = std::fs::metadata(member)
            .unwrap_or_else(|e| panic!("{} exists: {e}", member.display()))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{}", member.display());
    }
    let dir_mode = std::fs::metadata(fx.path().parent().expect("dir"))
        .expect("dir")
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
}

#[test]
fn a_foreign_file_is_refused_and_left_exactly_as_it_was() {
    let dir = crate::testing::TempDir::new("cache-foreign");
    let clock = Arc::new(ManualClock(AtomicI64::new(T0)));
    let cache_dir = dir.path().join("cache");
    file::ensure_private_dir(&cache_dir).expect("dir");
    let path = cache_dir.join("c.sqlite3");
    let open = || {
        MessageCache::open(CacheOptions {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            ..CacheOptions::new(&path)
        })
    };
    std::fs::write(&path, b"somebody else's notes").expect("write");
    assert!(matches!(open(), Err(CacheError::Foreign(_))));
    assert_eq!(
        std::fs::read(&path).expect("read"),
        b"somebody else's notes"
    );

    std::fs::remove_file(&path).expect("rm");
    let other = rusqlite::Connection::open(&path).expect("other db");
    other
        .execute_batch("CREATE TABLE theirs (x); INSERT INTO theirs VALUES (1);")
        .expect("theirs");
    drop(other);
    let before = std::fs::read(&path).expect("read");
    assert!(matches!(open(), Err(CacheError::Foreign(_))));
    assert_eq!(std::fs::read(&path).expect("read"), before);
}

#[test]
fn a_half_created_file_is_taken_as_ours_and_initialised() {
    let dir = crate::testing::TempDir::new("cache-half");
    let clock = Arc::new(ManualClock(AtomicI64::new(T0)));
    let cache_dir = dir.path().join("cache");
    file::ensure_private_dir(&cache_dir).expect("dir");
    let path = cache_dir.join("c.sqlite3");
    // An empty file: created, and nothing more, before a crash.
    file::create_if_absent(&path).expect("create");
    let cache = fixtures::open(&dir, &clock, Limits::default());
    assert_eq!(cache.counts().served().expect("counts"), Counts::default());
    drop(cache);
    file::unlink_family(&path);
    // A SQLite file with no application id and no schema.
    let bare = rusqlite::Connection::open(&path).expect("bare");
    bare.execute_batch("CREATE TABLE t (x); DROP TABLE t;")
        .expect("touch");
    drop(bare);
    assert_eq!(
        schema::read_header(&path).expect("header"),
        schema::Header::Unmarked
    );
    let cache = fixtures::open(&dir, &clock, Limits::default());
    let writer = cache.probe_pragmas().remove(0);
    assert!(
        writer.contains(&("auto_vacuum".to_owned(), "2".to_owned())),
        "{writer:?}"
    );
    drop(cache);
    assert_eq!(
        schema::read_header(&path).expect("header"),
        schema::Header::Ours
    );
}

#[test]
fn a_second_open_of_one_path_refuses_and_the_store_file_is_refused() {
    let fx = Fixture::new("cache-twice");
    let second = MessageCache::open(CacheOptions::new(fx.path()));
    assert!(matches!(second, Err(CacheError::Locked(_))), "{second:?}");
    let store = fx.dir.path().join("cache").join("store.sqlite3");
    let refused = MessageCache::open(CacheOptions {
        store_path: Some(store.clone()),
        ..CacheOptions::new(&store)
    });
    assert!(matches!(refused, Err(CacheError::Filesystem(_))));
}

#[test]
fn an_unreadable_file_of_ours_is_rebuilt() {
    let dir = crate::testing::TempDir::new("cache-corrupt");
    let clock = Arc::new(ManualClock(AtomicI64::new(T0)));
    let cache_dir = dir.path().join("cache");
    file::ensure_private_dir(&cache_dir).expect("dir");
    let path = cache_dir.join("c.sqlite3");
    let mut bytes = vec![0x5a_u8; 8192];
    bytes[..16].copy_from_slice(b"SQLite format 3\0");
    bytes[68..72].copy_from_slice(&schema::APPLICATION_ID_BYTES);
    std::fs::write(&path, &bytes).expect("write");
    let cache = fixtures::open(&dir, &clock, Limits::default());
    assert_eq!(cache.counts().served().expect("usable"), Counts::default());
}

#[test]
fn a_newer_additive_schema_is_used_and_an_incompatible_one_is_rebuilt() {
    let fx = Fixture::new("cache-compat");
    fx.list(vec![m(1)], T0, None);
    let path = fx.path();
    let Fixture {
        cache, clock, dir, ..
    } = fx;
    drop(cache);
    let poke = |version: i64, compat: &str| {
        let c = rusqlite::Connection::open(&path).expect("open");
        c.pragma_update(None, "user_version", version)
            .expect("version");
        c.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_compat'",
            [compat],
        )
        .expect("compat");
    };
    poke(2, "1");
    let cache = fixtures::open(&dir, &clock, Limits::default());
    assert_eq!(cache.counts().served().expect("counts").messages, 1, "kept");
    drop(cache);
    poke(2, "2");
    let cache = fixtures::open(&dir, &clock, Limits::default());
    assert_eq!(
        cache.counts().served().expect("counts").messages,
        0,
        "rebuilt"
    );
}

#[test]
fn migrations_are_additive_and_keep_what_the_file_holds() {
    let fx = Fixture::new("cache-migrate");
    fx.list(vec![m(1), m(2)], T0, None);
    let path = fx.path();
    drop(fx.cache);
    let connection = rusqlite::Connection::open(&path).expect("open");
    let ladder = [
        schema::STEPS[0],
        schema::Step {
            sql: "ALTER TABLE message ADD COLUMN later_column INTEGER;
                  CREATE INDEX message_by_later ON message (later_column);",
            compat: 1,
        },
    ];
    schema::migrate(&connection, &ladder).expect("additive step");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("version");
    assert_eq!(version, 2);
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 2);
    // Applying the ladder again does nothing.
    schema::migrate(&connection, &ladder).expect("idempotent");
}

// ---- the full-text index and privacy -----------------------------------------------------------

/// The oracle test: after mixed inserts, edits, deletions by push and by absence, and a cascade
/// erase, `MATCH` finds exactly what `instr` finds, and the index passes its integrity check.
#[test]
fn match_agrees_with_instr_through_mixed_writes_and_erases() {
    let fx = Fixture::new("cache-oracle");
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let pick = |n: u64| usize::try_from(n).unwrap_or(0);
    let other = ChanKey::new("p", "C2");
    let mut observed = T0;
    for round in 0..40_i64 {
        observed += 1_000;
        let mut rows = Vec::new();
        for _ in 0..(1 + next(6)) {
            let i = i64::try_from(next(30)).unwrap_or(0);
            let text = format!(
                "{} {} round{round}",
                words[pick(next(8))],
                words[pick(next(8))]
            );
            rows.push(message(&format!("m{i}"), "ada", &text, T0 + i * STEP));
        }
        let chan = if next(4) == 0 { &other } else { &fx.chan };
        let listing = |proof: Option<Proof>| {
            Observation::Listing(Listing {
                scope: Scope::Main,
                rows: rows
                    .iter()
                    .cloned()
                    .map(|r| Incoming::new(r, Scope::Main))
                    .collect(),
                observed_ms: observed,
                proof,
            })
        };
        match next(5) {
            0 => fx.offer(
                chan,
                Observation::Push {
                    push: Push::Delete {
                        id: mid(&format!("m{}", next(30))),
                        created_ms: None,
                    },
                    observed_ms: observed,
                },
            ),
            1 => fx.offer(chan, listing(Some(newest_proof(ListStart::ScopeStart)))),
            _ => fx.offer(chan, listing(None)),
        }
        if round == 30 {
            fx.cache.erase_channels(std::slice::from_ref(&other));
            fx.settle();
        }
        let terms = words
            .iter()
            .map(|w| (*w).to_owned())
            .chain([format!("round{round}"), "number".to_owned()]);
        for term in terms {
            let (by_index, by_scan) = fx
                .cache
                .read(None, |c| {
                    let quoted = search::fts_query(std::slice::from_ref(&term));
                    let mut a =
                        c.prepare("SELECT rowid FROM message_fts WHERE message_fts MATCH ?1")?;
                    let found: BTreeSet<i64> = a
                        .query_map([quoted], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    let mut b =
                        c.prepare("SELECT row_id FROM message WHERE instr(search_text, ?1) > 0")?;
                    let scanned: BTreeSet<i64> = b
                        .query_map([&term], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    Ok((found, scanned))
                })
                .served()
                .expect("served");
            assert_eq!(by_index, by_scan, "round {round}, term {term:?}");
        }
    }
    integrity(&fx);
    assert_eq!(fx.cache.stats().ops_failed, 0);
}

#[test]
fn a_tombstoned_unique_string_leaves_the_index_the_file_and_the_wal() {
    let fx = Fixture::new("cache-privacy");
    let secret = "zebracorn-quasar-71f3";
    fx.list(
        vec![
            message("keep", "ada", "ordinary words", T0),
            message(
                "gone",
                "ada",
                &format!("a message saying {secret} once"),
                T0 + 1,
            ),
        ],
        T0,
        None,
    );
    push_delete(&fx, "gone", T0 + 10);
    // The writer checkpointed after the tombstone batch; no `optimize` anywhere.
    let found: i64 = fx
        .cache
        .read(None, |c| {
            c.query_row(
                "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH ?1",
                [search::fts_query(&[secret.to_owned()])],
                |r| r.get(0),
            )
        })
        .served()
        .expect("served");
    assert_eq!(found, 0);
    assert!(fx.cache.stats().checkpoints_truncated >= 1);
    let needle = secret.as_bytes();
    for member in file::family(&fx.path()).iter().take(2) {
        let bytes = std::fs::read(member).unwrap_or_default();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{} still holds the deleted text",
            member.display()
        );
    }
    assert!(fx
        .cache
        .tombstoned(&fx.chan, &mid("gone"))
        .served()
        .expect("served"));
    integrity(&fx);
}

#[test]
fn listing_an_unchanged_row_again_writes_nothing() {
    let fx = Fixture::new("cache-unchanged");
    let rows = vec![m(1), m(2), m(3)];
    fx.list(rows.clone(), T0, None);
    let before = fx.cache.stats();
    assert_eq!(before.rows_written, 3);
    fx.list(rows, T0 + 5_000, None);
    let after = fx.cache.stats();
    assert_eq!(after.rows_written, 3, "nothing rewritten");
    assert_eq!(after.rows_unchanged, 3);
}

// ---- the merge rules ---------------------------------------------------------------------------

#[test]
fn a_read_that_started_before_a_pushed_edit_never_reverts_it() {
    let fx = Fixture::new("cache-revert");
    fx.list(vec![m(1)], T0 + 100, None);
    let edited = Message {
        content: "edited body".to_owned(),
        ..m(1)
    };
    push(&fx, edited, true, T0 + 200);
    assert_eq!(content(&fx, "m1").as_deref(), Some("edited body"));
    // A read that started at 150 (or a snapshot as of 150) brings the old text: kept out.
    fx.list(vec![m(1)], T0 + 150, None);
    assert_eq!(content(&fx, "m1").as_deref(), Some("edited body"));
    // A later read with newer text replaces it.
    let again = Message {
        content: "edited twice".to_owned(),
        ..m(1)
    };
    fx.list(vec![again], T0 + 300, None);
    assert_eq!(content(&fx, "m1").as_deref(), Some("edited twice"));
}

#[test]
fn a_deletion_leaves_only_ids_and_times_and_only_a_later_read_brings_it_back() {
    let fx = Fixture::new("cache-tombstone");
    fx.list(vec![m(1)], T0 + 100, None);
    push_delete(&fx, "m1", T0 + 200);
    assert_eq!(content(&fx, "m1"), None);
    let columns: Vec<String> = fx
        .cache
        .read(None, |c| {
            let mut s = c.prepare("SELECT name FROM pragma_table_info('tombstone')")?;
            let names = s.query_map([], |r| r.get(0))?.collect();
            names
        })
        .served()
        .expect("served");
    assert_eq!(
        columns,
        ids(&["chan", "message_id", "created_ms", "deleted_ms"])
    );
    fx.list(vec![m(1)], T0 + 150, None);
    assert_eq!(content(&fx, "m1"), None, "a read from before the deletion");
    assert_eq!(fx.cache.stats().rows_held_by_tombstone, 1);
    fx.list(vec![m(1)], T0 + 250, None);
    assert!(content(&fx, "m1").is_some(), "a read from after it");
    assert!(!fx
        .cache
        .tombstoned(&fx.chan, &mid("m1"))
        .served()
        .expect("served"));
    // A deletion of an id the cache never held still leaves a tombstone.
    push_delete(&fx, "never-seen", T0 + 300);
    assert!(fx
        .cache
        .tombstoned(&fx.chan, &mid("never-seen"))
        .served()
        .expect("served"));
}

#[test]
fn absence_from_an_exhaustive_home_listing_spares_the_band_and_rows_pushed_meanwhile() {
    let fx = Fixture::new("cache-absence");
    let proof = newest_proof(ListStart::ScopeStart);
    fx.list(vec![m(1), m(2), m(3), m(10)], T0 + 100, Some(proof));
    // Pushed while the next read runs: observed at 150, after that read started at 120.
    push(&fx, m(4), false, T0 + 150);
    // In m10's band: a row created a minute before m10, seen before the read.
    let late = message("late", "ada", "late", T0 + 10 * STEP - 60_000);
    push(&fx, late, false, T0 + 110);
    // The read that started at 120 lacks m2, m4 and the late one.
    fx.list(vec![m(1), m(3), m(10)], T0 + 120, Some(proof));
    assert_eq!(content(&fx, "m2"), None, "absent and seen before: deleted");
    assert!(
        content(&fx, "m4").is_some(),
        "pushed after the read started"
    );
    assert!(content(&fx, "late").is_some(), "inside the unsettled band");
    assert!(fx
        .cache
        .tombstoned(&fx.chan, &mid("m2"))
        .served()
        .expect("served"));
    // A listing without a proof deletes nothing.
    fx.list(vec![m(10)], T0 + 130, None);
    assert!(content(&fx, "m1").is_some());
}

#[test]
fn a_push_of_the_text_already_held_protects_the_row_from_an_older_listings_absence() {
    let fx = Fixture::new("cache-push-refresh");
    let proof = newest_proof(ListStart::At(T0));
    fx.list(vec![m(1), m(2), m(10)], T0 + 100, Some(proof));
    let before = fx.cache.stats().rows_written;
    // The live feed says m2 is there, as it is, at 200.
    push(&fx, m(2), true, T0 + 200);
    // A read that started at 150 lacks it: it began before the push saw it, so proves nothing.
    fx.list(vec![m(1), m(10)], T0 + 150, Some(proof));
    assert!(content(&fx, "m2").is_some());
    assert_eq!(
        held(&fx, "m2").expect("held").observed_ms,
        T0 + 200,
        "the push moved the time its text was last seen"
    );
    // A re-listing of unchanged rows still writes nothing.
    fx.list(vec![m(1), m(2), m(10)], T0 + 300, None);
    assert_eq!(
        fx.cache.stats().rows_written,
        before + 1,
        "only the push wrote"
    );
}

#[test]
fn listing_a_threads_replies_again_rewrites_neither_its_rows_nor_its_record() {
    let fx = Fixture::new("cache-thread-quiet");
    let replies = || {
        vec![
            reply("r1", "m1", "one", T0 + STEP + 1),
            reply("r2", "m1", "two", T0 + STEP + 2),
        ]
    };
    let thread = Scope::Thread("m1".to_owned());
    fx.list_scope(thread.clone(), replies(), T0 + 10, None);
    let verified = |fx: &Fixture| {
        sql_count(
            &fx.cache,
            "SELECT verified_ms FROM thread WHERE thread_id = 'm1'",
        )
    };
    assert_eq!(verified(&fx), T0 + 10);
    let written = fx.cache.stats().rows_written;
    fx.list_scope(thread, replies(), T0 + 50, None);
    assert_eq!(fx.cache.stats().rows_written, written);
    assert_eq!(verified(&fx), T0 + 10, "a reply carries no count to verify");
    // A thread list that counts the replies does verify the record.
    fx.offer(
        &fx.chan,
        Observation::Threads {
            threads: vec![ThreadRecord {
                thread_id: "m1".to_owned(),
                root_id: Some(mid("m1")),
                reply_count: Some(2),
                reply_count_exact: true,
                last_activity_ms: T0 + STEP + 2,
            }],
            observed_ms: T0 + 60,
        },
    );
    assert_eq!(verified(&fx), T0 + 60);
}

#[test]
fn absence_from_a_listing_of_another_scope_only_drops_that_membership() {
    let fx = Fixture::new("cache-membership");
    // A reply also sent to the channel: home is its thread, and it is listed in main too.
    let broadcast = reply("r1", "m1", "also in the channel", T0 + 2 * STEP);
    fx.offer(
        &fx.chan,
        Observation::Listing(Listing {
            scope: Scope::Main,
            rows: vec![
                Incoming::new(m(1), Scope::Main),
                Incoming::new(broadcast, Scope::Thread("m1".to_owned())),
                Incoming::new(m(5), Scope::Main),
            ],
            observed_ms: T0 + 100,
            proof: None,
        }),
    );
    assert_eq!(
        fx.cache
            .memberships(&fx.chan, &mid("r1"))
            .served()
            .expect("served"),
        vec![Scope::Main, Scope::Thread("m1".to_owned())]
    );
    fx.list(
        vec![m(1), m(5)],
        T0 + 200,
        Some(newest_proof(ListStart::ScopeStart)),
    );
    assert!(content(&fx, "r1").is_some(), "not deleted");
    assert_eq!(
        fx.cache
            .memberships(&fx.chan, &mid("r1"))
            .served()
            .expect("served"),
        vec![Scope::Thread("m1".to_owned())]
    );
}

#[test]
fn a_deleted_root_with_replies_leaves_its_replies_in_their_thread() {
    let fx = Fixture::new("cache-root");
    let mut root = m(1);
    root.thread = Some(MessageThread {
        id: "m1".to_owned(),
        root_message_id: Some(mid("m1")),
        is_root: true,
        reply_count: Some(2),
        reply_count_exact: true,
    });
    fx.list(vec![root, m(5)], T0 + 100, None);
    fx.list_scope(
        Scope::Thread("m1".to_owned()),
        vec![
            reply("r1", "m1", "one", T0 + STEP + 1),
            reply("r2", "m1", "two", T0 + STEP + 2),
        ],
        T0 + 100,
        None,
    );
    // The provider now answers the root as a tombstone, which the client filters out.
    fx.list(
        vec![m(5)],
        T0 + 200,
        Some(newest_proof(ListStart::ScopeStart)),
    );
    assert_eq!(content(&fx, "m1"), None);
    assert_eq!(fx.ids(&Scope::Thread("m1".to_owned())), ids(&["r1", "r2"]));
    assert!(fx
        .cache
        .thread(&fx.chan, "m1")
        .served()
        .expect("served")
        .is_some());
}

#[test]
fn a_complete_deltas_deletions_remove_ghosts_but_not_what_was_seen_after_it_started() {
    // A bridge supersedes a message's id; the ghost goes by the delta's deleted list.
    let fx = Fixture::new("cache-delta");
    fx.list_scope(Scope::Flat, vec![m(1), m(2)], T0 + 100, None);
    push(&fx, m(3), false, T0 + 300);
    fx.offer(
        &fx.chan,
        Observation::Deleted {
            ids: vec![mid("m1"), mid("m3"), mid("unknown")],
            observed_ms: T0 + 200,
        },
    );
    assert_eq!(content(&fx, "m1"), None, "the ghost is gone");
    assert!(content(&fx, "m2").is_some());
    assert!(
        content(&fx, "m3").is_some(),
        "set by a push after the delta's read started"
    );
    assert!(fx
        .cache
        .tombstoned(&fx.chan, &mid("unknown"))
        .served()
        .expect("served"));
    // ...and an exhaustive Flat listing that lacks a row removes it the same way.
    fx.list_scope(
        Scope::Flat,
        vec![m(3)],
        T0 + 400,
        Some(newest_proof(ListStart::ScopeStart)),
    );
    assert_eq!(content(&fx, "m2"), None);
    // A thread list's record is kept; its count only moves with a newer read.
    fx.offer(
        &fx.chan,
        Observation::Threads {
            threads: vec![ThreadRecord {
                thread_id: "t9".to_owned(),
                root_id: Some(mid("m3")),
                reply_count: Some(4),
                reply_count_exact: true,
                last_activity_ms: T0 + 9,
            }],
            observed_ms: T0 + 500,
        },
    );
    fx.offer(
        &fx.chan,
        Observation::Threads {
            threads: vec![ThreadRecord {
                thread_id: "t9".to_owned(),
                root_id: None,
                reply_count: Some(1),
                reply_count_exact: true,
                last_activity_ms: T0 + 5,
            }],
            observed_ms: T0 + 450,
        },
    );
    let record = fx
        .cache
        .thread(&fx.chan, "t9")
        .served()
        .expect("served")
        .expect("kept");
    assert_eq!(
        record.reply_count,
        Some(4),
        "an older read does not lower it"
    );
    assert_eq!(record.root_id, Some(mid("m3")));
    assert_eq!(record.last_activity_ms, T0 + 9);
}

#[test]
fn a_pushed_snippet_never_overwrites_listing_text_and_a_listing_replaces_a_partial() {
    let fx = Fixture::new("cache-snippet");
    let full = Message {
        content: "The whole body of a long message about the release".to_owned(),
        ..m(1)
    };
    fx.list(vec![full.clone()], T0 + 100, None);
    let snippet = Message {
        content: "The whole body of a long…".to_owned(),
        ..m(1)
    };
    push(&fx, snippet, true, T0 + 200);
    assert_eq!(content(&fx, "m1"), Some(full.content.clone()));
    // A pushed create of an unknown id is held as partial...
    let unknown = Message {
        content: "The start of".to_owned(),
        ..m(2)
    };
    push(&fx, unknown, false, T0 + 300);
    assert!(held(&fx, "m2").expect("held").content_partial);
    // ...and any listing replaces it, even one that started earlier.
    fx.list(vec![m(2)], T0 + 250, None);
    let replaced = held(&fx, "m2").expect("held");
    assert!(!replaced.content_partial);
    assert_eq!(replaced.message.content, m(2).content);
}

#[test]
fn a_copy_that_cannot_see_reactions_never_erases_held_ones() {
    let fx = Fixture::new("cache-reactions");
    let with = Message {
        reactions: Some(vec![Reaction {
            emoji: "👀".to_owned(),
            custom: false,
            custom_id: None,
            count: 2,
        }]),
        ..m(1)
    };
    fx.list(vec![with.clone()], T0 + 100, None);
    push(&fx, m(1), true, T0 + 200);
    assert_eq!(
        held(&fx, "m1").expect("held").message.reactions,
        with.reactions
    );
    // Empty is an answer, and a newer read gives it.
    let none = Message {
        reactions: Some(Vec::new()),
        ..m(1)
    };
    fx.list(vec![none], T0 + 300, None);
    assert_eq!(
        held(&fx, "m1").expect("held").message.reactions,
        Some(Vec::new())
    );
}

#[test]
fn a_message_reads_back_exactly_as_the_client_produced_it_minus_derived_fields() {
    let fx = Fixture::new("cache-roundtrip");
    let mut original = reply("r9", "m1", "hello **there**", T0 + 7);
    original.reply_to = Some(mid("m1"));
    original.author_is_bot = true;
    original.reactions = Some(vec![Reaction {
        emoji: "party".to_owned(),
        custom: true,
        custom_id: Some("E1".to_owned()),
        count: 1,
    }]);
    let mut decorated = original.clone();
    decorated.spoken_time = "09:51".to_owned();
    decorated.spoken_content = "hello there".to_owned();
    decorated.content_html = "<p>hello</p>".to_owned();
    decorated.noise = true;
    decorated.heading = Some(crate::model::Heading::default());
    fx.list_scope(Scope::Thread("m1".to_owned()), vec![decorated], T0, None);
    assert_eq!(held(&fx, "r9").expect("held").message, original);
}

#[test]
fn thread_records_need_replies_and_their_activity_only_moves_forward() {
    let fx = Fixture::new("cache-threads");
    let mut unanswered = m(1);
    unanswered.thread = Some(MessageThread {
        id: "m1".to_owned(),
        root_message_id: Some(mid("m1")),
        is_root: true,
        reply_count: Some(0),
        reply_count_exact: true,
    });
    fx.list(vec![unanswered], T0, None);
    assert_eq!(
        fx.cache.thread(&fx.chan, "m1").served().expect("served"),
        None
    );
    assert_eq!(
        sql_count(
            &fx.cache,
            "SELECT COUNT(*) FROM message WHERE thread_id IS NOT NULL"
        ),
        0,
        "an unanswered top-level message is not a thread"
    );
    fx.list_scope(
        Scope::Thread("m1".to_owned()),
        vec![reply("r2", "m1", "late reply", T0 + 5 * STEP)],
        T0 + 10,
        None,
    );
    fx.list_scope(
        Scope::Thread("m1".to_owned()),
        vec![reply("r1", "m1", "early reply", T0 + 2 * STEP)],
        T0 + 20,
        None,
    );
    let record = fx
        .cache
        .thread(&fx.chan, "m1")
        .served()
        .expect("served")
        .expect("a thread now");
    assert_eq!(record.last_activity_ms, T0 + 5 * STEP);
    fx.offer(
        &fx.chan,
        Observation::Threads {
            threads: vec![ThreadRecord {
                thread_id: "empty".to_owned(),
                root_id: None,
                reply_count: Some(0),
                reply_count_exact: true,
                last_activity_ms: T0,
            }],
            observed_ms: T0 + 30,
        },
    );
    assert_eq!(
        fx.cache.thread(&fx.chan, "empty").served().expect("served"),
        None
    );
}

#[test]
fn unrouted_channels_old_rows_and_coverage_below_the_age_limit_are_never_written() {
    let fx = Fixture::with_limits(
        "cache-never",
        Limits {
            max_days: 1,
            ..Limits::default()
        },
    );
    let stranger = ChanKey::new("p", "C9");
    fx.offer(
        &stranger,
        Observation::Listing(Listing {
            scope: Scope::Main,
            rows: vec![Incoming::new(m(1), Scope::Main)],
            observed_ms: T0,
            proof: None,
        }),
    );
    assert_eq!(fx.cache.stats().ops_unrouted, 1);
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
    // Local time is a week after the messages were written: older than the one-day limit.
    fx.clock.set(T0 + 7 * DAY_MS);
    fx.list(
        vec![m(1), m(2)],
        T0 + 7 * DAY_MS,
        Some(newest_proof(ListStart::ScopeStart)),
    );
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
    assert_eq!(fx.cache.stats().rows_too_old, 2);
    assert!(
        intervals(&fx, &Scope::Main).is_empty(),
        "coverage below the cutoff"
    );
}

// ---- coverage ----------------------------------------------------------------------------------

#[test]
fn only_a_proof_stamps_and_an_empty_listing_stamps_nothing() {
    let fx = Fixture::new("cache-proof");
    fx.list(vec![m(1), m(2), m(3)], T0 + 100, None);
    assert!(
        intervals(&fx, &Scope::Main).is_empty(),
        "a short read is not a proof"
    );
    // A proof from the oldest row, not from the scope's start: the start is never inferred.
    fx.list(
        vec![m(1), m(2), m(3)],
        T0 + 300,
        Some(newest_proof(ListStart::At(T0 + STEP))),
    );
    let stamped = intervals(&fx, &Scope::Main);
    assert_eq!(stamped.len(), 1);
    assert_eq!(stamped[0].t0_ms, T0 + STEP);
    assert_eq!(
        stamped[0].t1_ms,
        T0 + 3 * STEP - SETTLE_MS,
        "below the band"
    );
    assert_eq!(stamped[0].listed_ms, T0 + 300);
    assert_eq!(stamped[0].full_ms, Some(T0 + 300));
    assert!(stamped[0].complete);
    // An empty page proves nothing, even with a proof, and even where nothing else would keep its
    // claim back: the head is far above it, so its span is settled.
    fx.list(
        vec![m(1), m(60)],
        T0 + 400,
        Some(newest_proof(ListStart::At(T0 + 60 * STEP))),
    );
    let before = intervals(&fx, &Scope::Main);
    let range = Proof {
        from: ListStart::At(T0 + 10 * STEP),
        until: ListEnd::Before(T0 + 20 * STEP),
        complete: true,
        full: true,
        source: Source::Range,
    };
    fx.list(Vec::new(), T0 + 500, Some(range));
    assert_eq!(
        intervals(&fx, &Scope::Main),
        before,
        "an empty page proves nothing"
    );
    // The same span with one row in it is a proof.
    fx.list(vec![m(15)], T0 + 600, Some(range));
    assert!(intervals(&fx, &Scope::Main)
        .iter()
        .any(|i| i.t0_ms == T0 + 10 * STEP && i.t1_ms == T0 + 20 * STEP));
    // A provider proof that the scope starts here stamps from its start (clamped to the age limit).
    fx.list(
        vec![m(1), m(2), m(3)],
        T0 + 700,
        Some(newest_proof(ListStart::ScopeStart)),
    );
    let cutoff = Limits::default().cutoff_ms("C1", T0);
    assert_eq!(intervals(&fx, &Scope::Main)[0].t0_ms, cutoff);
}

#[test]
fn coverage_stays_out_of_the_unsettled_band_until_a_later_read_settles_it() {
    let fx = Fixture::new("cache-band");
    let proof = newest_proof(ListStart::At(T0 + STEP));
    fx.list(vec![m(1), m(2), m(3)], T0 + 1_000, Some(proof));
    let head = fx
        .cache
        .head(&fx.chan, &Scope::Main)
        .served()
        .expect("served")
        .expect("head");
    assert_eq!(
        head.band(),
        Some((T0 + 3 * STEP - SETTLE_MS, T0 + 3 * STEP))
    );
    // A row inside the band, listed late, is normal: inserted, no contradiction.
    let late = || message("late", "ada", "late", T0 + 3 * STEP - 30_000);
    fx.list(vec![m(1), m(2), late(), m(3)], T0 + 2_000, Some(proof));
    assert!(content(&fx, "late").is_some());
    assert_eq!(
        intervals(&fx, &Scope::Main).last().map(|i| i.t1_ms),
        Some(T0 + 3 * STEP - SETTLE_MS)
    );
    // Ten minutes (local) after the head was listed, a read of the newest end settles the band.
    fx.list(
        vec![m(1), m(2), late(), m(3)],
        T0 + 1_000 + SETTLE_AFTER_LOCAL_MS,
        Some(proof),
    );
    let head = fx
        .cache
        .head(&fx.chan, &Scope::Main)
        .served()
        .expect("served")
        .expect("head");
    assert_eq!(head.band(), None);
    assert_eq!(
        intervals(&fx, &Scope::Main).last().map(|i| i.t1_ms),
        Some(T0 + 3 * STEP + 1)
    );
}

#[test]
fn a_bounded_flat_page_never_deletes_a_reply_or_stamps_a_thread() {
    // Slack's "All" view: Main plus the replies of at most ten threads, the newest fifty of each.
    let fx = Fixture::new("cache-flat");
    let roots: Vec<Message> = (0..11)
        .map(|i| {
            let mut root = m(i * 10);
            root.thread = Some(MessageThread {
                id: root.id.0.clone(),
                root_message_id: Some(root.id.clone()),
                is_root: true,
                reply_count: Some(if i == 0 { 60 } else { 1 }),
                reply_count_exact: true,
            });
            root
        })
        .collect();
    let replies = |count: i64| -> Vec<Incoming> {
        (0..count)
            .map(|n| {
                Incoming::new(
                    reply(&format!("r{n}"), "m0", "reply", T0 + 1_000 + n),
                    Scope::Thread("m0".to_owned()),
                )
            })
            .collect()
    };
    let flat_page = |observed_ms: i64, roots: &[Message], replies: Vec<Incoming>| {
        // Roots are a Main listing with the main part's proof; replies are upserted only.
        fx.offer(
            &fx.chan,
            Observation::Listing(Listing {
                scope: Scope::Main,
                rows: roots
                    .iter()
                    .cloned()
                    .map(|r| Incoming::new(r, Scope::Main))
                    .collect(),
                observed_ms,
                proof: Some(newest_proof(ListStart::At(T0))),
            }),
        );
        fx.offer(
            &fx.chan,
            Observation::Rows {
                rows: replies,
                scope: None,
                observed_ms,
            },
        );
    };
    flat_page(T0 + 100, &roots, replies(60));
    assert_eq!(fx.ids(&Scope::Thread("m0".to_owned())).len(), 60);
    // The next page is bounded: ten threads, and fifty of the sixty replies.
    flat_page(T0 + 200, &roots[..10], replies(50));
    assert_eq!(
        fx.ids(&Scope::Thread("m0".to_owned())).len(),
        60,
        "no reply was deleted by a bounded page"
    );
    assert_eq!(
        sql_count(
            &fx.cache,
            "SELECT COUNT(*) FROM coverage WHERE scope_key <> 'main'"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &fx.cache,
            "SELECT COUNT(*) FROM scope_head WHERE scope_key <> 'main'"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &fx.cache,
            "SELECT COUNT(*) FROM tombstone WHERE message_id LIKE 'r%'"
        ),
        0
    );
}

// ---- the writer --------------------------------------------------------------------------------

#[test]
fn an_offer_too_big_for_the_queue_is_dropped_and_marks_the_channel_suspect() {
    let fx = Fixture::new("cache-drop");
    let huge = Message {
        content: "x".repeat(QUEUE_BYTES),
        ..m(1)
    };
    let stamp = fx.cache.stamp(&fx.chan);
    let offered = fx.cache.offer(
        fx.chan.clone(),
        stamp,
        Observation::Listing(Listing {
            scope: Scope::Main,
            rows: vec![Incoming::new(huge, Scope::Main)],
            observed_ms: T0,
            proof: None,
        }),
    );
    assert_eq!(offered, Offered::Dropped);
    assert!(fx.cache.is_suspect(&fx.chan));
    assert_eq!(fx.cache.stats().ops_dropped, 1);
    // A read of the newest end that started after the drop makes the head trusted again.
    fx.list(vec![m(1)], T0 + 20, Some(newest_proof(ListStart::At(T0))));
    assert!(!fx.cache.is_suspect(&fx.chan));
}

#[test]
fn a_barrier_with_nothing_to_wait_for_leaves_the_flush_flag_down() {
    let fx = Fixture::new("cache-flush-flag");
    fx.list(vec![m(1)], T0, None);
    assert!(fx.cache.barrier_blocking(Duration::from_secs(1)));
    assert!(
        !fx.cache
            .shared
            .seq
            .flush
            .load(std::sync::atomic::Ordering::SeqCst),
        "a raised flag would cut the next batch short for no one"
    );
}

#[tokio::test]
async fn the_async_barrier_waits_for_what_was_offered_before_it() {
    let fx = Fixture::new("cache-barrier");
    let stamp = fx.cache.stamp(&fx.chan);
    let mut last = 0;
    for i in 0..50 {
        if let Offered::Queued(seq) = fx.cache.offer(
            fx.chan.clone(),
            stamp,
            Observation::Rows {
                rows: vec![Incoming::new(m(i), Scope::Main)],
                scope: None,
                observed_ms: T0,
            },
        ) {
            last = seq;
        }
    }
    assert!(fx.cache.barrier(Duration::from_secs(10)).await);
    assert!(fx.cache.applied_seq() >= last);
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 50);
}

#[test]
fn search_text_is_rebuilt_when_the_rendering_rules_change() {
    struct Labelled;
    impl SearchTextSource for Labelled {
        fn version(&self) -> String {
            "labelled-2".to_owned()
        }
        fn rendered(&self, _provider_key: &str, message: &Message) -> Rendered {
            Rendered {
                text: format!("{} widgets#123", message.content),
                hrefs: vec!["https://example.test/issue/123".to_owned()],
            }
        }
    }
    let fx = Fixture::new("cache-rebuild");
    fx.list((0..1_200).map(m).collect(), T0, None);
    let path = fx.path();
    let Fixture {
        cache, clock, dir, ..
    } = fx;
    drop(cache);
    let cache = MessageCache::open(CacheOptions {
        search_text: Arc::new(Labelled),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        ..CacheOptions::new(&path)
    })
    .expect("reopen");
    cache.maintain(Maintenance::RebuildSearchText);
    assert!(cache.quiesce_blocking(Duration::from_secs(30)));
    assert!(cache.search_text_current());
    let labelled = sql_count(
        &cache,
        "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH '\"widgets#123\"'",
    );
    assert_eq!(labelled, 1_200);
    drop(dir);
}

#[test]
fn a_rebuild_that_fails_is_retried_rather_than_taken_as_done() {
    struct Relabelled;
    impl SearchTextSource for Relabelled {
        fn version(&self) -> String {
            "relabelled-3".to_owned()
        }
        fn rendered(&self, _provider_key: &str, message: &Message) -> Rendered {
            Rendered {
                text: format!("{} relabelled", message.content),
                hrefs: Vec::new(),
            }
        }
    }
    let fx = Fixture::new("cache-rebuild-retry");
    fx.list((0..50).map(m).collect(), T0, None);
    let path = fx.path();
    let Fixture {
        cache, clock, dir, ..
    } = fx;
    drop(cache);
    let raw = rusqlite::Connection::open(&path).expect("raw");
    raw.execute_batch(
        "CREATE TRIGGER refuse_rebuild BEFORE UPDATE OF search_text ON message
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .expect("sabotage");
    let cache = MessageCache::open(CacheOptions {
        search_text: Arc::new(Relabelled),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        ..CacheOptions::new(&path)
    })
    .expect("reopen");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !cache.search_text_current(),
        "a rebuild that failed was reported done"
    );
    raw.execute_batch("DROP TRIGGER refuse_rebuild")
        .expect("repair");
    drop(raw);
    assert!(cache.quiesce_blocking(Duration::from_secs(30)));
    assert!(cache.search_text_current());
    assert_eq!(
        sql_count(
            &cache,
            "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH '\"relabelled\"'",
        ),
        50
    );
    drop(dir);
}

// ---- erase -------------------------------------------------------------------------------------

#[test]
fn an_observation_queued_before_an_erase_is_dropped_when_it_arrives() {
    let fx = Fixture::new("cache-erase-queued");
    fx.list(vec![m(1)], T0, None);
    let stamp = fx.cache.stamp(&fx.chan);
    // Queue an observation, then erase, and let both reach the writer in that order.
    fx.cache.offer(
        fx.chan.clone(),
        stamp,
        Observation::Rows {
            rows: vec![Incoming::new(m(2), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    fx.settle();
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
    integrity(&fx);
}

#[test]
fn removing_a_channel_while_its_read_is_in_flight_leaves_nothing_afterwards() {
    let fx = Fixture::new("cache-erase-inflight");
    let other = ChanKey::new("p", "C2");
    // The read starts: its stamp is taken now.
    let stamp = fx.cache.stamp(&fx.chan);
    let other_stamp = fx.cache.stamp(&other);
    // The channel is removed while the read is running.
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    // The read lands.
    for (chan, stamp) in [(&fx.chan, stamp), (&other, other_stamp)] {
        fx.cache.offer(
            chan.clone(),
            stamp,
            Observation::Listing(Listing {
                scope: Scope::Main,
                rows: vec![Incoming::new(m(1), Scope::Main)],
                observed_ms: T0,
                proof: Some(newest_proof(ListStart::ScopeStart)),
            }),
        );
    }
    fx.settle();
    assert!(fx.ids(&Scope::Main).is_empty());
    assert!(intervals(&fx, &Scope::Main).is_empty());
    assert_eq!(fx.cache.stats().ops_stale, 1);
    assert_eq!(
        fx.cache
            .newest(&other, &Scope::Main, 10)
            .served()
            .expect("served")
            .len(),
        1,
        "another channel is untouched"
    );
}

#[test]
fn a_read_that_began_before_a_file_erase_never_returns_a_row_after_it() {
    let fx = Fixture::new("cache-erase-reader");
    fx.list(vec![m(1), m(2)], T0, None);
    let Fixture {
        cache, chan, dir, ..
    } = fx;
    let cache = Arc::new(cache);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let reader = {
        let cache = Arc::clone(&cache);
        let chan = chan.clone();
        std::thread::spawn(move || {
            cache.read(Some(&chan), |c| {
                let _ = started_tx.send(());
                let _ = go_rx.recv_timeout(Duration::from_secs(10));
                c.query_row("SELECT COUNT(*) FROM message", [], |r| r.get::<_, i64>(0))
            })
        })
    };
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the read began");
    let eraser = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || cache.erase_file(true))
    };
    // Let the erase uninstall the file before the read runs its SQL.
    while cache.is_open() {
        std::thread::sleep(Duration::from_millis(1));
    }
    go_tx.send(()).expect("go");
    let answer = reader.join().expect("reader");
    assert_eq!(
        answer,
        Read::Moved,
        "rows read across an erase are never served"
    );
    let report = eraser.join().expect("eraser").expect("erased");
    assert!(report.reopened);
    assert_eq!(cache.counts().served().expect("counts").messages, 0);
    assert!(cache
        .newest(&chan, &Scope::Main, 10)
        .served()
        .expect("served")
        .is_empty());
    drop(dir);
}

#[test]
fn a_channel_being_erased_is_never_read_and_one_off_the_list_is_blocked() {
    let fx = Fixture::new("cache-erase-blocked");
    fx.list((0..200).map(m).collect(), T0, None);
    let other = ChanKey::new("p", "C2");
    fx.offer(
        &other,
        Observation::Rows {
            rows: vec![Incoming::new(m(1), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    // However the erase and this read interleave, the read never returns the erased rows.
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    match fx.cache.newest(&fx.chan, &Scope::Main, 1_000) {
        Read::Served(rows) => assert!(rows.is_empty(), "{} rows served", rows.len()),
        Read::Blocked | Read::Moved => {}
        other => panic!("{other:?}"),
    }
    fx.settle();
    assert!(fx.ids(&Scope::Main).is_empty());
    // A channel the route snapshot does not list is blocked, whatever the file holds.
    fx.cache
        .retain_only(RouteSnapshot::new([fx.chan.clone()], ["p".to_owned()]));
    assert_eq!(fx.cache.newest(&other, &Scope::Main, 10), Read::Blocked);
}

#[test]
fn a_file_erase_deletes_everything_and_stamps_from_before_it_are_stale() {
    let fx = Fixture::new("cache-erase-file");
    fx.list(vec![m(1)], T0, None);
    let old = fx.cache.stamp(&fx.chan);
    let report = fx.cache.erase_file(true).expect("erase");
    assert!(report.bytes > 0);
    // The route snapshot survives a file erase; the stamp from before it does not.
    fx.cache.offer(
        fx.chan.clone(),
        old,
        Observation::Rows {
            rows: vec![Incoming::new(m(2), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    fx.settle();
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
    assert_eq!(fx.cache.stats().ops_stale, 1);
    // Turning it off leaves no file at all, and nothing to read.
    let off = fx.cache.erase_file(false).expect("off");
    assert!(!off.reopened);
    assert!(!fx.path().exists());
    assert_eq!(fx.cache.counts(), Read::Off);
    let after = fx.cache.offer(
        fx.chan.clone(),
        fx.cache.stamp(&fx.chan),
        Observation::Deleted {
            ids: Vec::new(),
            observed_ms: T0,
        },
    );
    assert_eq!(after, Offered::Off);
    assert!(!fx.cache.barrier_blocking(Duration::from_millis(10)));
}

#[test]
fn a_crash_between_the_marker_and_the_unlink_erases_at_the_next_start() {
    let fx = Fixture::new("cache-crash");
    fx.list(vec![m(1)], T0, None);
    let marker_file = fx.dir.path().join("store.sqlite3.message-cache.json");
    let path = fx.path();
    // The owner's "off" writes the marker first...
    let mut sidecar = marker::Sidecar {
        off: true,
        off_at_ms: Some(T0),
        ..marker::Sidecar::default()
    };
    sidecar.register(&path);
    marker::store(&marker_file, &sidecar).expect("marker");
    // ...and the process dies before the unlink.
    let Fixture { cache, dir, .. } = fx;
    drop(cache);
    assert!(path.exists());
    let config = crate::config::MessageCacheConfig {
        enabled: true,
        ..crate::config::MessageCacheConfig::default()
    };
    let next_start = marker::reconcile_at_start(Some(&config), &marker_file, path.parent(), T0 + 1)
        .expect("reconcile");
    assert_eq!(next_start.action, marker::StartupAction::EraseAll);
    assert!(next_start.erased_bytes > 0);
    for member in file::family(&path) {
        assert!(!member.exists(), "{} survived", member.display());
    }
    drop(dir);
}

#[test]
fn switching_off_writes_the_marker_before_it_erases() {
    let fx = Fixture::new("cache-switch");
    fx.list(vec![m(1)], T0, None);
    let marker_file = fx.dir.path().join("store.sqlite3.message-cache.json");
    let report = marker::switch_off(&fx.cache, &marker_file, T0 + 5).expect("off");
    assert!(report.bytes > 0);
    let sidecar = marker::load(&marker_file).expect("load");
    assert!(sidecar.off);
    assert_eq!(sidecar.paths, vec![fx.path()]);
    assert!(!fx.path().exists());
}

#[test]
fn a_channel_erase_cascades_everywhere_and_the_index_stays_intact() {
    let fx = Fixture::new("cache-cascade");
    let other = ChanKey::new("p", "C2");
    let mut root = m(1);
    root.thread = Some(MessageThread {
        id: "m1".to_owned(),
        root_message_id: Some(mid("m1")),
        is_root: true,
        reply_count: Some(1),
        reply_count_exact: true,
    });
    fx.list(
        vec![root, m(2), m(3)],
        T0,
        Some(newest_proof(ListStart::ScopeStart)),
    );
    push_delete(&fx, "m2", T0 + 10);
    fx.offer(
        &other,
        Observation::Rows {
            rows: vec![Incoming::new(m(7), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    fx.settle();
    for table in [
        "message",
        "tombstone",
        "coverage",
        "scope_head",
        "thread",
        "message_scope",
    ] {
        let left = sql_count(
            &fx.cache,
            &format!(
                "SELECT COUNT(*) FROM {table} WHERE chan NOT IN \
                 (SELECT chan FROM channel WHERE channel_id = 'C2')"
            ),
        );
        assert_eq!(left, 0, "{table} kept rows of the erased channel");
    }
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 1);
    integrity(&fx);
    // Erasing a channel that holds nothing costs nothing.
    let erased = fx.cache.stats().erased_channels;
    fx.cache
        .erase_channels(&[ChanKey::new("p", "nothing-here")]);
    fx.settle();
    assert_eq!(fx.cache.stats().erased_channels, erased);
}

#[test]
fn retain_only_sweeps_every_channel_and_provider_off_the_list() {
    let fx = Fixture::new("cache-retain");
    let other = ChanKey::new("p", "C2");
    fx.list(vec![m(1)], T0, None);
    fx.offer(
        &other,
        Observation::Rows {
            rows: vec![Incoming::new(m(2), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    fx.cache
        .retain_only(RouteSnapshot::new([other.clone()], ["p".to_owned()]));
    fx.settle();
    let held_for = |channel_id: &str| {
        sql_count(
            &fx.cache,
            &format!(
                "SELECT COUNT(*) FROM message m JOIN channel c ON c.chan = m.chan \
                 WHERE c.channel_id = '{channel_id}'"
            ),
        )
    };
    assert_eq!(held_for("C1"), 0, "swept");
    assert_eq!(fx.cache.newest(&fx.chan, &Scope::Main, 10), Read::Blocked);
    assert_eq!(
        fx.cache
            .newest(&other, &Scope::Main, 10)
            .served()
            .expect("served")
            .len(),
        1
    );
    // ...and a later offer for the swept channel is refused at the gate.
    fx.list(vec![m(3)], T0 + 1, None);
    assert_eq!(held_for("C1"), 0);
    assert_eq!(fx.cache.stats().ops_unrouted, 1);
    fx.cache.erase_provider("p");
    fx.settle();
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
}

// ---- bounds and maintenance --------------------------------------------------------------------

#[test]
fn eviction_converges_below_the_size_limit_in_a_bounded_number_of_chunks() {
    let fx = Fixture::with_limits(
        "cache-evict",
        Limits {
            max_bytes: 4 * 1024 * 1024 * 1024,
            ..Limits::default()
        },
    );
    let body = "lorem ipsum dolor sit amet ".repeat(40);
    let rows = 12_000_i64;
    for chunk in 0..(rows / 500) {
        fx.list(
            (chunk * 500..(chunk + 1) * 500)
                .map(|i| {
                    message(
                        &format!("m{i}"),
                        "ada",
                        &format!("{body} {i}"),
                        T0 + i * 1_000,
                    )
                })
                .collect(),
            T0,
            Some(Proof {
                from: ListStart::At(T0 + chunk * 500 * 1_000),
                until: ListEnd::Before(T0 + (chunk + 1) * 500 * 1_000),
                complete: true,
                full: true,
                source: Source::Walk,
            }),
        );
    }
    let path = fx.path();
    let Fixture {
        cache,
        clock,
        dir,
        chan,
    } = fx;
    drop(cache);
    let full = file::bytes_on_disk(&path);
    let limit: u64 = 16 * 1024 * 1024;
    assert!(full > limit * 2, "the fill must be over the limit: {full}");
    let cache = fixtures::open(
        &dir,
        &clock,
        Limits {
            max_bytes: limit,
            ..Limits::default()
        },
    );
    cache.maintain(Maintenance::Bounds);
    assert!(cache.quiesce_blocking(Duration::from_secs(60)));
    let stats = cache.stats();
    assert!(
        cache.bytes_on_disk() <= limit,
        "{} > {limit}",
        cache.bytes_on_disk()
    );
    // Bounded: every chunk deletes at least one row and at most a thousand, and the rounds that
    // measure, delete and compact stop at a handful, so the chunks are bounded by the rows.
    let rows = u64::try_from(rows).unwrap_or(0);
    assert!(stats.evicted_rows > 0);
    assert!(stats.evicted_rows < rows, "evicted everything: {stats:?}");
    assert!(
        stats.eviction_chunks <= stats.evicted_rows,
        "{} chunks for {} rows",
        stats.eviction_chunks,
        stats.evicted_rows
    );
    assert!(stats.eviction_chunks >= stats.evicted_rows / u64::try_from(DELETE_CHUNK).unwrap_or(1));
    assert!(stats.compactions >= 1, "eviction compacts the index");
    // Coverage never claims what eviction removed.
    let oldest: i64 = cache
        .read(None, |c| {
            c.query_row("SELECT MIN(created_ms) FROM message", [], |r| r.get(0))
        })
        .served()
        .expect("served");
    cache.retain_only(RouteSnapshot::new([chan.clone()], ["p".to_owned()]));
    assert!(cache.barrier_blocking(Duration::from_secs(10)));
    let stamped = cache
        .coverage(&chan, &Scope::Main)
        .served()
        .expect("served");
    assert!(!stamped.is_empty());
    assert!(
        stamped.iter().all(|i| i.t0_ms > oldest - 1_000),
        "{stamped:?} vs {oldest}"
    );
}

#[test]
fn the_hourly_pass_expires_old_rows_unrevalidated_rows_and_old_tombstones() {
    let fx = Fixture::with_limits(
        "cache-hourly",
        Limits {
            max_days: 30,
            channel_days: [("C2".to_owned(), 1)].into_iter().collect(),
            ..Limits::default()
        },
    );
    let other = ChanKey::new("p", "C2");
    // Revalidated: an exhaustive, edit-aware listing covers m1..m3.
    fx.list(
        vec![m(1), m(2), m(3)],
        T0,
        Some(newest_proof(ListStart::At(T0))),
    );
    // Never revalidated: a row only seen once, without a proof.
    fx.list_scope(Scope::Flat, vec![m(20)], T0, None);
    fx.offer(
        &other,
        Observation::Rows {
            rows: vec![Incoming::new(m(30), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    push_delete(&fx, "m3", T0 + 1);
    // Two days on: the one-day channel's row has aged out; nothing else has.
    fx.clock.set(T0 + 2 * DAY_MS);
    fx.cache.maintain(Maintenance::Hourly);
    fx.settle();
    assert!(fx
        .cache
        .newest(&other, &Scope::Main, 10)
        .served()
        .expect("served")
        .is_empty());
    assert!(content(&fx, "m20").is_some());
    assert_eq!(fx.cache.counts().served().expect("counts").tombstones, 1);
    // Eight days on: what nothing revalidated within a week is gone, and so is the tombstone.
    fx.clock.set(T0 + 8 * DAY_MS);
    fx.cache.maintain(Maintenance::Hourly);
    fx.settle();
    assert_eq!(content(&fx, "m20"), None, "never revalidated");
    assert_eq!(
        content(&fx, "m1"),
        None,
        "its covering read is more than a week old"
    );
    assert_eq!(fx.cache.counts().served().expect("counts").tombstones, 0);
    assert_eq!(fx.cache.stats().integrity_failures, 0);
}

#[test]
fn revalidation_within_the_week_keeps_rows() {
    let fx = Fixture::new("cache-revalidate");
    fx.list(vec![m(1), m(2)], T0, Some(newest_proof(ListStart::At(T0))));
    fx.clock.set(T0 + 6 * DAY_MS);
    fx.list(
        vec![m(1), m(2)],
        T0 + 6 * DAY_MS,
        Some(newest_proof(ListStart::At(T0))),
    );
    fx.clock.set(T0 + 8 * DAY_MS);
    fx.cache.maintain(Maintenance::Hourly);
    fx.settle();
    assert_eq!(fx.ids(&Scope::Main), ids(&["m1", "m2"]));
}
