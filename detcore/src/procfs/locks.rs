/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The rows of the kernel's `/proc/locks`, split into the fields that decide
//! whether the guest owns them, and the snapshot with only the kept rows.
//!
//! Detcore renders `/proc/locks` from the kernel's file, read through the
//! guest's own descriptor. In the guest's PID namespace Linux hides every
//! POSIX, flock and lease row whose owner the namespace cannot see
//! (`locks_show` drops a row whose `locks_translate_pid` is 0), but it cannot
//! hide an open file description (OFD) lock: such a lock has no owning task,
//! and Linux prints it as owner `-1` from every namespace, so any process on
//! the host that held an OFD lock added rows to the guest's snapshot at
//! moments the host decided. Which rows stay is decided from the kernel's own
//! state at the snapshot, in `procfs::locks_scan`: a row a guest process owns
//! by its positive pid, at most as many of an OFD lock's rows as distinct
//! guest open file descriptions hold it now (each description's
//! `/proc/<pid>/fdinfo/<fd>` lists exactly the locks it owns, spelled as
//! `/proc/locks` spells them, [`held_locks_in_fdinfo`]), and, as a TEMPORARY
//! EXEMPTION for KVM, a row on device `00:00` ([`HeldLock::on_null_device`]).
//! Rows that go are dropped before the sanitizer numbers identities
//! ([`keep_lock_rows`]), so they shift no remaining identity.

use serde::Deserialize;
use serde::Serialize;

/// One row of `/proc/locks`, split into fields:
///     `SEQ: [->] CLASS MODE RW PID MAJOR:MINOR:INODE START END`
#[derive(Clone)]
pub(super) struct LockLine {
    pub(super) sequence: String,
    pub(super) waiter: bool,
    pub(super) fields: Vec<String>,
    pub(super) owner_index: usize,
    pub(super) object_index: usize,
}

/// `line` as a `/proc/locks` row, or `None` when it is not one. The owner is
/// a signed integer and the object has three `:`-separated parts.
pub(super) fn parse_line(line: &str) -> Option<LockLine> {
    let fields = line
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let waiter = fields.get(1).is_some_and(|field| field == "->");
    let details = usize::from(waiter) + 1;
    let owner_index = details + 3;
    let object_index = details + 4;
    let sequence = fields
        .first()
        .and_then(|field| field.strip_suffix(':'))
        .filter(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))?
        .to_owned();
    if fields.len() != details + 7
        || fields[owner_index].parse::<i64>().is_err()
        || fields[object_index].split(':').count() != 3
    {
        return None;
    }
    Some(LockLine {
        sequence,
        waiter,
        fields,
        owner_index,
        object_index,
    })
}

/// One granted lock as `/proc/locks` and fdinfo spell it, its owner aside:
/// class (`OFDLCK`, `FLOCK`, `POSIX`, ...), mode (`ADVISORY`), access
/// (`READ`, `WRITE`), object (`MAJOR:MINOR:INODE`, hexadecimal device numbers
/// and a decimal inode, `%02x:%02x:%lu`) and range.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HeldLock {
    /// The lock's class.
    pub class: String,
    /// The lock's mode.
    pub mode: String,
    /// The lock's access.
    pub access: String,
    /// The locked object, as the kernel prints it.
    pub object: String,
    /// The range's first byte.
    pub start: String,
    /// The range's last byte, or `EOF`.
    pub end: String,
}

impl HeldLock {
    fn from_line(row: &LockLine) -> Self {
        let first = usize::from(row.waiter) + 1;
        let field = |index: usize| row.fields[index].clone();
        Self {
            class: field(first),
            mode: field(first + 1),
            access: field(first + 2),
            object: field(row.object_index),
            start: field(row.object_index + 1),
            end: field(row.object_index + 2),
        }
    }

    /// Whether the object is on device `00:00`: the temporary KVM exemption
    /// (see the module comment), removed when package K slice K8 replaces
    /// KVM's local `/proc/locks`. Linux never prints that device for a
    /// lockable object.
    pub(super) fn on_null_device(&self) -> bool {
        let mut parts = self.object.split(':');
        let number = |part: Option<&str>| part.and_then(|part| u32::from_str_radix(part, 16).ok());
        number(parts.next()) == Some(0) && number(parts.next()) == Some(0)
    }

    /// Whether Linux ties this lock to an open file description rather than
    /// to a task: an OFD lock or a flock.
    fn belongs_to_a_description(&self) -> bool {
        self.class == "OFDLCK" || self.class == "FLOCK"
    }
}

/// The fields of one `/proc/locks` row that decide whether the guest owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRowFacts {
    /// The group's sequence number, which a granted lock and its waiters share.
    pub sequence: String,
    /// Whether this is a waiter (`->`) row.
    pub waiter: bool,
    /// The owner field: a pid, `-1` for an OFD lock, `0` for a task the
    /// reader's PID namespace cannot see, or a remote lock's own value.
    pub owner: i64,
    /// The lock, its owner aside.
    pub lock: HeldLock,
}

/// The facts of every row of `contents`, in row order, or `None` when any row
/// is not a `/proc/locks` row (the sanitizer then refuses the whole snapshot).
pub(crate) fn lock_rows(contents: &[u8]) -> Option<Vec<LockRowFacts>> {
    let text = std::str::from_utf8(contents).ok()?;
    text.lines()
        .map(|line| {
            let row = parse_line(line)?;
            Some(LockRowFacts {
                owner: row.fields[row.owner_index].parse().ok()?,
                lock: HeldLock::from_line(&row),
                sequence: row.sequence,
                waiter: row.waiter,
            })
        })
        .collect()
}

/// The OFD and flock locks a description holds, from its
/// `/proc/<pid>/fdinfo/<fd>` text: its `lock:` lines, which the kernel spells
/// as `/proc/locks` rows. Lines of other classes (a POSIX lock this process
/// took on the same file) and lines that do not parse are left out.
pub(crate) fn held_locks_in_fdinfo(contents: &[u8]) -> Vec<HeldLock> {
    let text = String::from_utf8_lossy(contents);
    text.lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .filter_map(|row| parse_line(row.trim()))
        .filter(|row| !row.waiter)
        .map(|row| HeldLock::from_line(&row))
        .filter(HeldLock::belongs_to_a_description)
        .collect()
}

/// The rows of `contents` that `kept` keeps, in order. With no decision (a
/// snapshot whose rows do not parse, which the sanitizer refuses anyway, or a
/// caller that made none) every row is kept. A decision for a different
/// number of rows keeps none.
pub(crate) fn keep_lock_rows(contents: &[u8], kept: Option<&[bool]>) -> Vec<u8> {
    let Some(kept) = kept else {
        return contents.to_vec();
    };
    let Ok(text) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != kept.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (line, keep) in lines.into_iter().zip(kept) {
        if *keep {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    if !text.ends_with('\n') && !out.is_empty() {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rows as Linux prints them, from a host where a guest in a PID namespace
    // holds a POSIX lock (namespace pid 3), an OFD lock and a flock, and the
    // host holds others. `00:2a` is a btrfs superblock device.
    const SNAPSHOT: &[u8] = b"\
1: POSIX  ADVISORY  WRITE 3 00:2a:1001 0 EOF
2: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0
3: OFDLCK ADVISORY  WRITE -1 00:18:588084 1073741824 1073741824
4: OFDLCK ADVISORY  WRITE -1 00:2a:1002 10 10
5: FLOCK  ADVISORY  WRITE 4242 00:2a:1001 0 EOF
6: POSIX  ADVISORY  WRITE 4243 00:2a:1004 0 EOF
6: -> POSIX  ADVISORY  WRITE 3 00:2a:1004 0 EOF
7: OFDLCK ADVISORY  WRITE -1 00:2a:1003 0 EOF
7: -> OFDLCK ADVISORY  WRITE -1 00:2a:1003 0 EOF
7: -> POSIX  ADVISORY  WRITE 3 00:2a:1003 0 EOF
7: -> POSIX  ADVISORY  WRITE 0 00:2a:1003 0 EOF
";

    // What fdinfo shows for the guest's descriptions: one OFD lock on inode
    // 1002 at byte 0, one flock (taken by pid 4242, since reaped) and one OFD
    // lock on inode 1003. The POSIX line is this process's own POSIX lock on
    // the same file, which fdinfo lists too.
    const FDINFO: &[u8] = b"pos:\t0\nflags:\t02100002\nmnt_id:\t31\nino:\t1002\n\
lock:\t1: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0\n\
lock:\t2: POSIX  ADVISORY  WRITE 3 00:2a:1002 5 5\n\
lock:\t3: FLOCK  ADVISORY  WRITE 4242 00:2a:1001 0 EOF\n\
lock:\t4: OFDLCK ADVISORY  WRITE -1 00:2a:1003 0 EOF\n";

    fn guest(owner: i64) -> bool {
        owner == 3
    }

    #[test]
    fn parses_every_row_and_refuses_a_malformed_one() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        assert_eq!(rows.len(), 11);
        assert_eq!(rows[2].owner, -1);
        assert_eq!(rows[2].lock.object, "00:18:588084");
        assert_eq!(rows[2].lock.start, "1073741824");
        assert!(rows[6].waiter && rows[6].sequence == "6" && rows[6].lock.class == "POSIX");
        assert_eq!(rows[10].owner, 0);
        assert!(lock_rows(b"1: POSIX ADVISORY WRITE 3 00:2a:1001 0 EOF\nnot a row\n").is_none());
        assert!(lock_rows(b"").unwrap().is_empty());
    }

    #[test]
    fn fdinfo_names_the_descriptions_ofd_and_flock_locks() {
        let held = held_locks_in_fdinfo(FDINFO);
        assert_eq!(
            held.iter()
                .map(|lock| (
                    lock.class.as_str(),
                    lock.object.as_str(),
                    lock.start.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("OFDLCK", "00:2a:1002", "0"),
                ("FLOCK", "00:2a:1001", "0"),
                ("OFDLCK", "00:2a:1003", "0"),
            ]
        );
        assert!(held_locks_in_fdinfo(b"pos:\t0\nflags:\t02\n").is_empty());
        assert!(held_locks_in_fdinfo(b"lock:\tnot a lock\n").is_empty());
    }

    #[test]
    fn rows_a_backend_rendered_on_the_null_device_are_kept() {
        // KVM's backend-local copy of the file (the temporary exemption,
        // removed with package K slice K8).
        let contents = b"\
1: OFDLCK ADVISORY WRITE -1 00:00:1 0 EOF
2: FLOCK ADVISORY WRITE 98765 00:00:2 0 EOF
";
        let rows = lock_rows(contents).unwrap();
        assert_eq!(
            super::super::locks_scan::keep_rows(&rows, |_| false, &Default::default()),
            [true, true]
        );
    }

    #[test]
    fn dropped_rows_shift_no_remaining_identity() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        let kept = super::super::locks_scan::keep_rows(&rows, guest, &Default::default());
        let mut file =
            super::super::ProcfsFile::from_path(std::path::Path::new("/proc/locks")).unwrap();
        file.initialize(
            SNAPSHOT.to_vec(),
            super::super::ProcfsSnapshotContext {
                lock_rows_kept: Some(kept),
                ..Default::default()
            },
        );
        let rendered = file.take(usize::MAX).unwrap();
        // Exactly what the guest's one row alone renders: the host's rows,
        // gone, numbered nothing.
        assert_eq!(
            rendered,
            super::super::sanitize_locks(b"1: POSIX  ADVISORY  WRITE 3 00:2a:1001 0 EOF\n")
        );
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            "1: POSIX ADVISORY WRITE 1 00:00:1 0 EOF\n"
        );
    }

    #[test]
    fn kept_rows_preserve_order_and_the_final_newline() {
        let contents = b"1: A\n2: B\n3: C\n";
        assert_eq!(
            keep_lock_rows(contents, Some(&[true, false, true][..])),
            b"1: A\n3: C\n"
        );
        assert_eq!(
            keep_lock_rows(b"1: A\n2: B", Some(&[false, true][..])),
            b"2: B"
        );
        assert_eq!(keep_lock_rows(contents, Some(&[false; 3][..])), b"");
        assert_eq!(keep_lock_rows(contents, None), contents);
        assert_eq!(keep_lock_rows(contents, Some(&[true][..])), b"");
    }
}
