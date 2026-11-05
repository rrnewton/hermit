//! What deleted and edited text leaves behind in the full-text index and the files. `#208
//! persistent-cache`, milestone `cache-store`.
//!
//! The byte scans here see the message table's pages, which `secure_delete` zeroes; they cannot
//! see the index's, which hold prefix-compressed trigrams rather than whole strings. What the
//! index still holds is checked through the index instead: `MATCH`, FTS5's own `secure-delete`
//! setting, and the number of index segments a deletion leaves, which a deletion by marker
//! raises (a segment of markers) and a deletion in place does not.

use std::sync::Arc;
use std::time::Duration;

use super::fixtures::{self, message, Fixture, T0};
use super::tests::{integrity, m, push, push_delete};
use super::*;
use crate::model::Message;

const SECONDS: Duration = Duration::from_secs(60);

/// FTS5's stored `secure-delete` setting: 1 when deleted rows leave the index in place.
fn fts_secure_delete(cache: &MessageCache) -> i64 {
    cache
        .read(None, |c| {
            c.query_row(
                "SELECT v FROM message_fts_config WHERE k = 'secure-delete'",
                [],
                |r| r.get(0),
            )
        })
        .served()
        .expect("served")
}

fn matches(cache: &MessageCache, term: &str) -> i64 {
    cache
        .read(None, |c| {
            c.query_row(
                "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH ?1",
                [search::fts_query(&[term.to_owned()])],
                |r| r.get(0),
            )
        })
        .served()
        .expect("served")
}

fn segments(cache: &MessageCache) -> i64 {
    cache
        .read(None, |c| {
            c.query_row(
                "SELECT COUNT(DISTINCT segid) FROM message_fts_idx",
                [],
                |r| r.get(0),
            )
        })
        .served()
        .expect("served")
}

/// The bytes of the index's own pages.
fn index_bytes(cache: &MessageCache) -> i64 {
    cache
        .read(None, |c| {
            c.query_row(
                "SELECT COALESCE(SUM(LENGTH(block)), 0) FROM message_fts_data",
                [],
                |r| r.get(0),
            )
        })
        .served()
        .expect("served")
}

/// Whether any of the cache's database and write-ahead log still holds `needle`'s bytes.
fn on_disk(fx_path: &std::path::Path, needle: &str) -> Vec<std::path::PathBuf> {
    file::family(fx_path)
        .iter()
        .take(2)
        .filter(|member| {
            let bytes = std::fs::read(member).unwrap_or_default();
            bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
        })
        .cloned()
        .collect()
}

/// Prose-like text with a large trigram vocabulary: words drawn from letters by a fixed
/// generator, so the index is as big as real text makes it and a compaction takes many steps.
fn varied(seed: i64) -> String {
    let mut state = u64::try_from(seed)
        .unwrap_or(0)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        | 1;
    let mut text = String::new();
    while text.len() < 260 {
        let mut word = String::new();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for k in 0..(3 + state % 7) {
            let letter = (state >> (k * 5)) % 26;
            word.push(char::from(b'a' + u8::try_from(letter).unwrap_or(0)));
        }
        text.push_str(&word);
        text.push(' ');
    }
    text
}

fn fill_varied(fx: &Fixture, chan: &ChanKey, from: i64, count: i64) {
    for chunk in (from..from + count).step_by(250) {
        let end = (chunk + 250).min(from + count);
        fx.offer(
            chan,
            Observation::Rows {
                rows: (chunk..end)
                    .map(|i| {
                        Incoming::new(
                            message(&format!("v{i}"), "ada", &varied(i), T0 + i),
                            Scope::Main,
                        )
                    })
                    .collect(),
                scope: None,
                observed_ms: T0,
            },
        );
    }
}

fn fill(fx: &Fixture, chan: &ChanKey, from: i64, count: i64) {
    let body = "lorem ipsum dolor sit amet ".repeat(4);
    for chunk in (from..from + count).step_by(250) {
        let end = (chunk + 250).min(from + count);
        fx.offer(
            chan,
            Observation::Rows {
                rows: (chunk..end)
                    .map(|i| {
                        Incoming::new(
                            message(&format!("f{i}"), "ada", &format!("{body} {i}"), T0 + i),
                            Scope::Main,
                        )
                    })
                    .collect(),
                scope: None,
                observed_ms: T0,
            },
        );
    }
}

/// An edit is a deletion of the words it took away: they leave the index, and the new ones are
/// in it, whether the edit came pushed or by a later read.
#[test]
fn an_edit_takes_the_old_words_out_of_the_index_and_puts_the_new_ones_in() {
    let fx = Fixture::new("cache-privacy-edit");
    fx.list(
        vec![Message {
            content: "the first wording mentions zanzibarquill".to_owned(),
            ..m(1)
        }],
        T0 + 100,
        None,
    );
    assert_eq!(matches(&fx.cache, "zanzibarquill"), 1);
    push(
        &fx,
        Message {
            content: "the corrected wording mentions quixoticfern".to_owned(),
            ..m(1)
        },
        true,
        T0 + 200,
    );
    assert_eq!(
        matches(&fx.cache, "zanzibarquill"),
        0,
        "the old words stayed"
    );
    assert_eq!(
        matches(&fx.cache, "quixoticfern"),
        1,
        "the new words are missing"
    );
    fx.list(
        vec![Message {
            content: "a later read says marmaladetrout".to_owned(),
            ..m(1)
        }],
        T0 + 300,
        None,
    );
    assert_eq!(matches(&fx.cache, "quixoticfern"), 0);
    assert_eq!(matches(&fx.cache, "marmaladetrout"), 1);
    integrity(&fx);
}

/// FTS5 `secure-delete` is a setting stored in the file. It is on after an open and a reopen,
/// after every kind of bulk deletion, and after an open of a file that some earlier build or a
/// crash in one left with it off.
#[test]
fn fts_secure_delete_is_on_after_every_open_and_every_bulk_deletion() {
    struct Relabelled;
    impl SearchTextSource for Relabelled {
        fn version(&self) -> String {
            "relabelled-2".to_owned()
        }
        fn rendered(&self, _provider_key: &str, message: &Message) -> Rendered {
            Rendered {
                text: format!("{} relabelled", message.content),
                hrefs: Vec::new(),
            }
        }
    }
    let fx = Fixture::with_limits(
        "cache-privacy-setting",
        Limits {
            channel_days: [("C2".to_owned(), 1)].into_iter().collect(),
            ..Limits::default()
        },
    );
    assert_eq!(fts_secure_delete(&fx.cache), 1, "a fresh file");
    let other = ChanKey::new("p", "C2");
    fill(&fx, &fx.chan, 0, 600);
    fill(&fx, &other, 1_000, 300);
    // An erase.
    fx.cache.erase_channels(std::slice::from_ref(&other));
    fx.settle();
    assert_eq!(fts_secure_delete(&fx.cache), 1, "after an erase");
    // Expiry by age.
    fill(&fx, &other, 2_000, 50);
    fx.clock.set(T0 + 2 * DAY_MS);
    fx.cache.maintain(Maintenance::Hourly);
    fx.settle();
    assert_eq!(fx.cache.stats().expired_rows, 50);
    assert_eq!(fts_secure_delete(&fx.cache), 1, "after the age pass");
    let path = fx.path();
    let Fixture {
        cache, clock, dir, ..
    } = fx;
    drop(cache);
    // A rebuild of search_text, and a reopen.
    let cache = MessageCache::open(CacheOptions {
        search_text: Arc::new(Relabelled),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        ..CacheOptions::new(&path)
    })
    .expect("reopen");
    assert_eq!(fts_secure_delete(&cache), 1, "after a reopen");
    assert!(cache.quiesce_blocking(SECONDS));
    assert!(cache.search_text_current());
    assert_eq!(fts_secure_delete(&cache), 1, "after a rebuild");
    drop(cache);
    // Eviction.
    let cache = fixtures::open(
        &dir,
        &clock,
        Limits {
            max_bytes: 256 * 1024,
            ..Limits::default()
        },
    );
    cache.maintain(Maintenance::Bounds);
    assert!(cache.quiesce_blocking(SECONDS));
    assert!(cache.stats().evicted_rows > 0);
    assert_eq!(fts_secure_delete(&cache), 1, "after eviction");
    drop(cache);
    // A file left with the setting off, and a compaction owed, by an earlier build.
    let raw = rusqlite::Connection::open(&path).expect("raw");
    raw.execute_batch(
        "INSERT INTO message_fts (message_fts, rank) VALUES ('secure-delete', 0);
         INSERT INTO meta (key, value) VALUES ('compaction_due', '1');",
    )
    .expect("an older build's state");
    drop(raw);
    let cache = fixtures::open(&dir, &clock, Limits::default());
    assert_eq!(fts_secure_delete(&cache), 1, "turned back on at open");
    assert!(cache.quiesce_blocking(SECONDS));
    assert!(cache.stats().compactions >= 1, "the owed compaction ran");
    let owed = cache
        .read(None, |c| {
            c.query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'compaction_due'",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .served()
        .expect("served");
    assert_eq!(owed, 0);
}

/// After eviction has deleted by marker, a single deletion in the same session is still made in
/// place: it adds no segment of markers to the index, `MATCH` finds nothing, and the text is in
/// neither file.
#[test]
fn a_tombstone_after_an_eviction_is_taken_out_of_the_index_in_place() {
    let fx = Fixture::with_limits(
        "cache-privacy-after-bulk",
        Limits {
            max_bytes: 640 * 1024,
            ..Limits::default()
        },
    );
    fill(&fx, &fx.chan, 0, 2_000);
    assert!(fx.cache.stats().evicted_rows > 0, "{:?}", fx.cache.stats());
    let secret = "pangolinsextant-93ac";
    let mut rows: Vec<Message> = (0..20)
        .map(|i| {
            message(
                &format!("n{i}"),
                "ada",
                &format!("neighbour {i}"),
                T0 + 5_000 + i,
            )
        })
        .collect();
    rows.push(message(
        "gone",
        "ada",
        &format!("this message says {secret} once"),
        T0 + 5_100,
    ));
    fx.list(rows, T0 + 10, None);
    let evicted = fx.cache.stats().evicted_rows;
    let before = segments(&fx.cache);
    push_delete(&fx, "gone", T0 + 20);
    assert_eq!(
        fx.cache.stats().evicted_rows,
        evicted,
        "no eviction in between"
    );
    assert_eq!(matches(&fx.cache, secret), 0);
    assert_eq!(
        segments(&fx.cache),
        before,
        "the deletion left a segment of markers: it was not made in place"
    );
    assert_eq!(fts_secure_delete(&fx.cache), 1);
    let path = fx.path();
    assert!(
        on_disk(&path, secret).is_empty(),
        "{:?}",
        on_disk(&path, secret)
    );
    integrity(&fx);
}

/// An erased channel's text leaves the index, the database and the write-ahead log, though its
/// rows go in steps after the erase is applied.
#[test]
fn an_erased_channels_text_leaves_the_index_the_file_and_the_wal() {
    let fx = Fixture::new("cache-privacy-erase");
    let secret = "axolotlmeridian-5d0e";
    let other = ChanKey::new("p", "C2");
    fill(&fx, &fx.chan, 0, 400);
    fx.list(
        vec![message(
            "secret",
            "ada",
            &format!("only here: {secret}"),
            T0 + 9_000,
        )],
        T0,
        None,
    );
    fill(&fx, &other, 1_000, 100);
    assert_eq!(matches(&fx.cache, secret), 1);
    let index_before = index_bytes(&fx.cache);
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    fx.settle();
    assert_eq!(matches(&fx.cache, secret), 0);
    // Four rows in five went: their entries were taken out of the index's pages, so it shrank.
    // A deletion by marker would have made it larger.
    let index_after = index_bytes(&fx.cache);
    assert!(
        index_after * 2 < index_before,
        "the index kept the erased entries: {index_before} bytes before, {index_after} after"
    );
    assert!(fx.cache.stats().checkpoints_truncated >= 1);
    let path = fx.path();
    assert!(
        on_disk(&path, secret).is_empty(),
        "{:?}",
        on_disk(&path, secret)
    );
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 100);
    integrity(&fx);
}

/// An erase too small to pay for a compaction takes each row out of the index in place: no
/// compaction, no segment of markers, nothing left to find.
#[test]
fn a_small_erase_takes_its_rows_out_of_the_index_in_place() {
    let fx = Fixture::new("cache-privacy-small-erase");
    let other = ChanKey::new("p", "C2");
    fill(&fx, &other, 0, 2_000);
    let secret = "quagmirelantern-77b1";
    fx.list(
        (0..10)
            .map(|i| {
                message(
                    &format!("s{i}"),
                    "ada",
                    &format!("{secret} {i}"),
                    T0 + 9_000 + i,
                )
            })
            .collect(),
        T0,
        None,
    );
    let before = segments(&fx.cache);
    let compactions = fx.cache.stats().compactions;
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    fx.settle();
    assert_eq!(matches(&fx.cache, secret), 0);
    assert_eq!(fx.cache.stats().compactions, compactions, "no compaction");
    assert_eq!(segments(&fx.cache), before, "no segment of markers");
    assert!(on_disk(&fx.path(), secret).is_empty());
    integrity(&fx);
}

/// Markers written while a compaction runs are not in its merge, so another compaction follows
/// it: at the end the index is one segment again, holding nothing erased.
#[test]
fn an_erase_landing_during_a_compaction_is_compacted_too() {
    let fx = Fixture::new("cache-privacy-compact-again");
    let second = ChanKey::new("p", "C2");
    fill_varied(&fx, &fx.chan, 0, 3_000);
    fill_varied(&fx, &second, 10_000, 3_000);
    let secret = "marsupialtrellis-0c2d";
    fx.offer(
        &second,
        Observation::Rows {
            rows: vec![Incoming::new(
                message("hidden", "ada", &format!("only {secret}"), T0 + 50_000),
                Scope::Main,
            )],
            scope: None,
            observed_ms: T0,
        },
    );
    let started = fx.cache.stats();
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    // Wait for the first compaction to be under way, then erase the second channel.
    let deadline = std::time::Instant::now() + SECONDS;
    while fx.cache.stats().compaction_slice_max_us == started.compaction_slice_max_us
        && fx.cache.stats().compactions == started.compactions
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_micros(200));
    }
    fx.cache.erase_channels(std::slice::from_ref(&second));
    fx.settle();
    assert_eq!(matches(&fx.cache, secret), 0);
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
    let structure = fx
        .cache
        .read(None, super::structure::read)
        .served()
        .expect("served")
        .expect("parses");
    assert!(
        structure.segments() <= 1,
        "segments the compactions did not take in: {structure:?}"
    );
    let stats = fx.cache.stats();
    assert!(
        stats.compaction_steps >= stats.compactions * 3,
        "each compaction went on until its merge was over: {stats:?}"
    );
    assert!(on_disk(&fx.path(), secret).is_empty());
    integrity(&fx);
}
