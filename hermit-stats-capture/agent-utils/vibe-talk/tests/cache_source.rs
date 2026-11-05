//! `#208 persistent-cache`, milestone `cache-store`: source rules for the message cache.
//!
//! **No replacing insert anywhere in `src/cache/`.** SQLite's replacing conflict resolution deletes
//! the conflicting row WITHOUT firing its delete trigger (recursive triggers are off), so the old
//! text would stay in the full-text index for good, and the new row id would orphan the row's
//! scope memberships. Every upsert there is `INSERT … ON CONFLICT … DO UPDATE` or a read and an
//! `UPDATE`. This test fails on the keyword in any spelling SQL accepts, including the
//! column-constraint form, so the rule cannot be broken by a statement written in lower case.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources_below(path: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_sources_below(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// The SQL words that make an insert replace: `OR REPLACE`, `REPLACE INTO`, and a constraint's
/// `ON CONFLICT REPLACE`, matched case-insensitively across any whitespace; and the bare keyword
/// in capitals, which is how this crate writes SQL.
fn offences(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let words: Vec<(usize, String)> = source
        .lines()
        .enumerate()
        .flat_map(|(n, line)| {
            line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|w| !w.is_empty())
                .map(move |w| (n + 1, w.to_owned()))
                .collect::<Vec<_>>()
        })
        .collect();
    for (i, (line, word)) in words.iter().enumerate() {
        let lower = word.to_ascii_lowercase();
        if lower != "replace" {
            continue;
        }
        let before = i
            .checked_sub(1)
            .map(|j| words[j].1.to_ascii_lowercase())
            .unwrap_or_default();
        let after = words
            .get(i + 1)
            .map(|(_, w)| w.to_ascii_lowercase())
            .unwrap_or_default();
        if word == "REPLACE" || before == "or" || before == "conflict" || after == "into" {
            found.push((*line, word.clone()));
        }
    }
    found
}

#[test]
fn no_replacing_insert_appears_anywhere_in_the_cache_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("cache");
    let mut sources = Vec::new();
    rust_sources_below(&root, &mut sources);
    assert!(
        sources.len() >= 5,
        "the cache module was not found where this test looks: {}",
        root.display()
    );
    let mut failures = Vec::new();
    for path in &sources {
        let text = fs::read_to_string(path).expect("readable source");
        for (line, word) in offences(&text) {
            failures.push(format!("{}:{line}: {word}", path.display()));
        }
    }
    assert!(
        failures.is_empty(),
        "replacing inserts in src/cache/:\n{}",
        failures.join("\n")
    );
}

#[test]
fn the_rule_catches_every_spelling_and_spares_prose() {
    for offending in [
        "INSERT OR REPLACE INTO t VALUES (1)",
        "insert or replace into t values (1)",
        "REPLACE INTO t VALUES (1)",
        "replace\n   into t values (1)",
        "CREATE TABLE t (x UNIQUE ON CONFLICT REPLACE)",
        "create table t (x unique on conflict replace)",
    ] {
        assert!(
            !offences(offending).is_empty(),
            "{offending:?} slipped through"
        );
    }
    for innocent in [
        "a later listing replaces the content",
        "Verdict::Replace { partial }",
        "text.replace('\"', \"\\\"\\\"\")",
        "watch.send_replace(seq)",
    ] {
        assert!(offences(innocent).is_empty(), "{innocent:?} was flagged");
    }
}
