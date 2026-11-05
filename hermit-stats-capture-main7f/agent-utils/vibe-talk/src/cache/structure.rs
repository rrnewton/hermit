//! The full-text index's structure record, read so that a compaction can tell when it is done.
//!
//! FTS5 keeps its index as segments arranged in levels, and describes them in one record, row 10
//! of `message_fts_data`. Its layout is part of FTS5's file format (`fts5StructureWrite` in
//! `fts5_index.c`): a four-byte cookie; in the second version, four marker bytes; the number of
//! levels, of segments and the write counter; then, per level, the number of its segments that
//! are inputs to a merge in progress and the number it holds, and per segment its id, first and
//! last page (and, in the second version, five more numbers). Every number is a SQLite variable
//! length integer.
//!
//! # Why a compaction needs it
//!
//! A deletion by marker leaves the deleted entries in the old segments, cancelled by a marker in
//! a newer one, until a merge takes both in and writes neither. A compaction starts one merge of
//! every segment (`merge` with a negative page count), and its output, the only segment of a new
//! last level, is the "oldest" kind that drops entries and markers alike. Its inputs leave the
//! structure only when that merge completes, all at once. Batches go on writing new segments
//! while it runs, so "a step found nothing to merge" is no sign that it is over: [`merge_done`]
//! looks at that merge itself.

use rusqlite::{Connection, OptionalExtension as _};

/// The marker bytes after the cookie in the second version of the record.
const VERSION_2: &[u8; 4] = b"\xff\x00\x00\x01";
/// FTS5's own ceiling on levels and segments; anything above is a damaged record.
const MAX: u64 = 2_000;

/// What the structure record says, as far as a compaction needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Structure {
    /// Level 0 first.
    pub(crate) levels: Vec<Level>,
}

/// One level of segments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Level {
    /// How many of its segments, from the first, are inputs to a merge in progress.
    pub(crate) merge: usize,
    /// Its segments' ids, oldest first.
    pub(crate) segments: Vec<u64>,
}

/// A merge in progress, as a compaction tracks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Merge {
    /// The segment the merge is writing.
    pub(crate) output: u64,
    /// Whether that output is the only segment of the last level: the kind of merge that drops
    /// deleted entries and their markers alike, rather than carrying the markers forward.
    pub(crate) oldest: bool,
}

/// SQLite's variable-length integer: big-endian groups of seven bits, the high bit set on every
/// byte but the last; a ninth byte, if reached, carries eight.
fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let byte = *bytes.get(*at)?;
        *at += 1;
        if i == 8 {
            return Some((value << 8) | u64::from(byte));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Read a structure record. `None` when it does not parse: the caller then cannot tell, and
/// must not guess.
#[must_use]
pub(crate) fn parse(blob: &[u8]) -> Option<Structure> {
    let mut at = 4;
    let second = blob.get(4..8) == Some(VERSION_2.as_slice());
    if second {
        at += 4;
    }
    let levels = varint(blob, &mut at)?;
    let segments = varint(blob, &mut at)?;
    let _write_counter = varint(blob, &mut at)?;
    if levels > MAX || segments > MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut seen = 0;
    for _ in 0..levels {
        let merge = varint(blob, &mut at)?;
        let count = varint(blob, &mut at)?;
        if merge > count || count > MAX {
            return None;
        }
        let mut ids = Vec::new();
        for _ in 0..count {
            ids.push(varint(blob, &mut at)?);
            let _first = varint(blob, &mut at)?;
            let _last = varint(blob, &mut at)?;
            if second {
                for _ in 0..5 {
                    varint(blob, &mut at)?;
                }
            }
        }
        seen += count;
        out.push(Level {
            merge: usize::try_from(merge).ok()?,
            segments: ids,
        });
    }
    (seen == segments).then_some(Structure { levels: out })
}

/// Read the index's structure record now.
///
/// # Errors
///
/// When SQLite fails. A record that does not parse is `Ok(None)`.
pub(crate) fn read(connection: &Connection) -> rusqlite::Result<Option<Structure>> {
    let blob: Option<Vec<u8>> = connection
        .query_row(
            "SELECT block FROM message_fts_data WHERE id = 10",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(blob.as_deref().and_then(parse))
}

impl Structure {
    /// The merge in progress, when there is exactly one: the one a compaction's first step has
    /// just begun. Its output is the last segment of the level above its inputs.
    #[must_use]
    pub(crate) fn sole_merge(&self) -> Option<Merge> {
        let mut merging = self
            .levels
            .iter()
            .enumerate()
            .filter(|(_, level)| level.merge > 0);
        let (index, _) = merging.next()?;
        if merging.next().is_some() {
            return None;
        }
        let out = self.levels.get(index + 1)?;
        Some(Merge {
            output: *out.segments.last()?,
            oldest: index + 2 == self.levels.len() && out.segments.len() == 1,
        })
    }

    /// Whether no merge is in progress anywhere.
    #[must_use]
    pub(crate) fn quiet(&self) -> bool {
        self.levels.iter().all(|level| level.merge == 0)
    }

    /// How many segments it holds.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn segments(&self) -> usize {
        self.levels.iter().map(|level| level.segments.len()).sum()
    }
}

/// Whether the merge writing `output` is over. While it runs, its output is the last segment of
/// its level and the level below has a merge in progress; nothing else adds to the level it
/// writes (FTS5 moves no segment past a level that is merging). When it completes, its inputs
/// all leave together and the level below stops merging, or starts a new merge whose output is
/// added after this one; or, when every entry it read was deleted, the output itself is dropped.
/// So it is over exactly when the output is gone, is no longer last, or the level below is not
/// merging.
#[must_use]
pub(crate) fn merge_done(structure: &Structure, output: u64) -> bool {
    let Some((index, position)) = structure
        .levels
        .iter()
        .enumerate()
        .find_map(|(index, level)| {
            level
                .segments
                .iter()
                .position(|&id| id == output)
                .map(|position| (index, position))
        })
    else {
        return true;
    };
    let last = position + 1 == structure.levels[index].segments.len();
    index == 0 || !last || structure.levels[index - 1].merge == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(out: &mut Vec<u8>, value: u64) {
        // The inverse of `varint`, for values below 2^56.
        let mut groups = vec![u8::try_from(value & 0x7f).unwrap_or(0)];
        let mut rest = value >> 7;
        while rest > 0 {
            groups.push(u8::try_from(rest & 0x7f).unwrap_or(0) | 0x80);
            rest >>= 7;
        }
        groups.reverse();
        out.extend(groups);
    }

    fn record(levels: &[(u64, &[u64])], second: bool) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 7];
        if second {
            out.extend_from_slice(VERSION_2);
        }
        put(&mut out, u64::try_from(levels.len()).unwrap_or(0));
        let segments: usize = levels.iter().map(|(_, s)| s.len()).sum();
        put(&mut out, u64::try_from(segments).unwrap_or(0));
        put(&mut out, 300);
        for (merge, ids) in levels {
            put(&mut out, *merge);
            put(&mut out, u64::try_from(ids.len()).unwrap_or(0));
            for id in *ids {
                put(&mut out, *id);
                put(&mut out, 1);
                put(&mut out, 200);
                if second {
                    for n in 0..5 {
                        put(&mut out, n * 1_000);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn both_versions_of_the_record_parse() {
        for second in [false, true] {
            let parsed =
                parse(&record(&[(0, &[3]), (2, &[1, 2, 9]), (0, &[4])], second)).expect("parses");
            assert_eq!(parsed.levels.len(), 3);
            assert_eq!(parsed.levels[1].merge, 2);
            assert_eq!(parsed.levels[1].segments, vec![1, 2, 9]);
            assert_eq!(parsed.segments(), 5);
        }
        assert_eq!(parse(&[0, 0, 0]), None, "too short");
        let mut bad = record(&[(3, &[1, 2])], false);
        assert_eq!(parse(&bad), None, "more merging than held");
        bad.truncate(bad.len() - 1);
        assert_eq!(parse(&bad), None, "cut short");
        assert_eq!(varint(&[0x81, 0x00], &mut 0), Some(128));
        assert_eq!(varint(&[0xff; 9], &mut 0), Some(u64::MAX));
    }

    #[test]
    fn a_compactions_merge_is_found_and_its_end_recognised() {
        // Just begun: every old segment is an input at level 2; the output is level 3's only one.
        let begun = parse(&record(
            &[(0, &[]), (0, &[]), (3, &[5, 6, 7]), (0, &[8])],
            false,
        ))
        .expect("parses");
        assert_eq!(
            begun.sole_merge(),
            Some(Merge {
                output: 8,
                oldest: true
            })
        );
        assert!(!merge_done(&begun, 8));
        // Batches wrote new segments meanwhile; one level-0 merge finished into level 1.
        let going = parse(&record(
            &[(0, &[9]), (0, &[10]), (3, &[5, 6, 7, 11]), (0, &[8])],
            false,
        ))
        .expect("parses");
        assert!(!merge_done(&going, 8));
        // Done: its inputs are gone and level 2 is not merging...
        let done = parse(&record(
            &[(0, &[9]), (0, &[10]), (0, &[11]), (0, &[8])],
            false,
        ))
        .expect("parses");
        assert!(merge_done(&done, 8));
        // ...or level 2 has started a new merge, whose output comes after this one...
        let next = parse(&record(
            &[(0, &[9]), (0, &[]), (2, &[11, 12]), (0, &[8, 5])],
            false,
        ))
        .expect("parses");
        assert!(merge_done(&next, 8));
        // ...or everything it read was deleted, and its output was dropped.
        let empty =
            parse(&record(&[(0, &[9]), (0, &[]), (0, &[]), (0, &[])], false)).expect("parses");
        assert!(merge_done(&empty, 8));
        // A merge whose output shares the last level is not the oldest kind.
        let shared = parse(&record(&[(2, &[1, 2]), (0, &[3, 4])], false)).expect("parses");
        assert_eq!(
            shared.sole_merge(),
            Some(Merge {
                output: 4,
                oldest: false
            })
        );
        let two = parse(&record(&[(2, &[1, 2]), (2, &[3, 4]), (0, &[5])], false)).expect("parses");
        assert_eq!(two.sole_merge(), None, "two merges at once");
        assert!(done.quiet());
        assert!(!two.quiet());
    }

    /// Against the real thing: a trigram index built and merged by this SQLite, read back.
    #[test]
    fn the_linked_sqlites_record_parses_and_a_full_merge_is_tracked_to_its_end() {
        let c = Connection::open_in_memory().expect("memory");
        c.execute_batch(
            "CREATE TABLE message (row_id INTEGER PRIMARY KEY, search_text TEXT);
             CREATE VIRTUAL TABLE message_fts USING fts5(search_text, content = 'message',
                 content_rowid = 'row_id', tokenize = 'trigram', detail = full);
             INSERT INTO message_fts (message_fts, rank) VALUES ('automerge', 0);",
        )
        .expect("schema");
        for batch in 0..12 {
            c.execute_batch("BEGIN").expect("begin");
            for i in 0..200 {
                let id = batch * 200 + i;
                c.execute(
                    "INSERT INTO message (row_id, search_text) VALUES (?1, ?2)",
                    rusqlite::params![
                        id,
                        format!("message {id} about things {}", id * 7919 % 1009)
                    ],
                )
                .expect("row");
                c.execute(
                    "INSERT INTO message_fts (rowid, search_text) VALUES (?1, ?2)",
                    rusqlite::params![
                        id,
                        format!("message {id} about things {}", id * 7919 % 1009)
                    ],
                )
                .expect("index");
            }
            c.execute_batch("COMMIT").expect("commit");
        }
        let before = read(&c).expect("read").expect("parses");
        assert_eq!(before.segments(), 12, "{before:?}");
        c.execute(
            "INSERT INTO message_fts (message_fts, rank) VALUES ('merge', -2)",
            [],
        )
        .expect("begin the merge");
        let merge = read(&c)
            .expect("read")
            .expect("parses")
            .sole_merge()
            .expect("one merge in progress");
        assert!(merge.oldest);
        let mut steps = 0;
        loop {
            let now = read(&c).expect("read").expect("parses");
            if merge_done(&now, merge.output) {
                // The output leads the last level, and nothing merges into it any more: every
                // segment from before went into it. (Their ids may be reused by newer segments.)
                assert_eq!(
                    now.levels.last().and_then(|l| l.segments.first()),
                    Some(&merge.output),
                    "{now:?}"
                );
                break;
            }
            // New segments arrive meanwhile, as batches would write them.
            c.execute(
                "INSERT INTO message_fts (rowid, search_text) VALUES (?1, 'a later message')",
                [10_000 + steps],
            )
            .expect("a newer row");
            c.execute(
                "INSERT INTO message_fts (message_fts, rank) VALUES ('merge', 2)",
                [],
            )
            .expect("a step");
            steps += 1;
            assert!(steps < 10_000, "the merge never ended");
        }
        assert!(steps > 1, "it took more than one step: {steps}");
    }
}
