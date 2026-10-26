/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rename cookies in inotify events (inotify(7)).
//!
//! Linux draws the cookie that pairs an `IN_MOVED_FROM` event with its
//! `IN_MOVED_TO` from one host-wide counter that every process's renames
//! advance (`fsnotify_get_cookie`, fs/notify/notification.c), so the values a
//! guest reads depend on everything else renaming files on the host
//! (<https://github.com/rrnewton/hermit/issues/4765>). After an inotify
//! descriptor's `read`, `readv`, or `preadv2` at the current offset (-1,
//! which Linux serves as `readv`) succeeds, each nonzero cookie in the events
//! read is replaced by a virtual one, numbered from 1 in the order the run
//! first observes it ([`CookieTable`], kept in global state through
//! `GlobalRequest::VirtualInotifyCookies`). Only the 4-byte cookie fields that
//! change are written back.
//!
//! Determinism: the table is run-wide and forgets nothing, so every inotify
//! instance and every process agrees on a rename's cookie, as on Linux, and a
//! pair keeps one value however far apart its halves are read. For
//! nonblocking reads (`IN_NONBLOCK`), the filesystem operations that queue
//! the events and the reads that observe them all run in the scheduler's
//! order, so the order of first observations, and with it every virtual
//! cookie, is a function of the schedule. A blocking read is not covered: it
//! waits in the host, so which events it returns, and so the numbering, can
//! depend on host timing (<https://github.com/rrnewton/hermit/issues/4768>);
//! KVM refuses blocking inotify reads. A read that carries no rename (every
//! cookie zero) is left alone and asks global state nothing.
//!
//! Where host cookies stay in guest memory:
//! - A read that fails with EFAULT (warned). `inotify_read` copies events one
//!   at a time and returns -EFAULT when a later copy faults, after the
//!   earlier events were copied and dequeued (fs/notify/inotify/inotify_user.c),
//!   and Detcore does not learn how much was written. A `readv` or `preadv2`
//!   whose later iovec faults returns the earlier iovecs' bytes instead (not
//!   warned: nothing tells it apart from a short read); the events copied into
//!   the faulting iovec keep theirs
//!   (<https://github.com/rrnewton/hermit/issues/4793>).
//! - A destination Hermit cannot read (warned): a write-only page on the
//!   in-process backends (<https://github.com/rrnewton/hermit/issues/4530>).
//! - A `readv` or `preadv2` whose iovecs overlap (warned). Linux reads one
//!   iovec at a time, so a later event overwrites part of an earlier one
//!   where they overlap, and the bytes left no longer parse as the events
//!   Linux returned: a field that looks like a cookie there may be a watch
//!   descriptor or part of a name. Those bytes are left exactly as Linux
//!   wrote them. The iovec after the ones filled counts too, when the count
//!   ends at its start: Linux may have written it before a fault
//!   ([`Destination::plan`]).
//! - A descriptor received over `SCM_RIGHTS` where Detcore records no
//!   received descriptor: under record and replay, without sequentialized
//!   threads, or from a receive on the background path
//!   (`crate::syscalls::scm_rights`). Its reads do not come here. Elsewhere
//!   a received inotify descriptor is recorded as one: an anonymous inode
//!   has no file type, so its link ([`INOTIFY_LINK`]) says what it is.

use std::collections::BTreeMap;
use std::ops::Range;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use tracing::warn;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// `sizeof(struct inotify_event)`: `wd`, `mask`, `cookie`, `len`.
const EVENT_HEADER: usize = 16;
/// Offset of `cookie` within `struct inotify_event`.
const EVENT_COOKIE: usize = 8;
/// Offset of `len` within `struct inotify_event`.
const EVENT_LENGTH: usize = 12;
/// `sizeof(struct iovec)`.
const IOVEC: usize = std::mem::size_of::<libc::iovec>();
/// What an inotify instance's descriptor links to: `inotify_init1` makes it
/// with `anon_inode_getfd("inotify", ...)` (fs/notify/inotify/inotify_user.c),
/// and an anonymous inode has no file type to tell it by.
pub(crate) const INOTIFY_LINK: &[u8] = b"anon_inode:inotify";

/// Run-wide map from host rename cookies to virtual ones.
///
/// Nothing is forgotten. An event can wait in an instance that nobody reads
/// for the rest of the run, and when it is read, as the other half of its
/// pair or as another instance's copy, it must get the value its rename's
/// first observation got; a table that dropped its oldest entries would give
/// it a new one. Every entry is a host cookie that some read returned in an
/// event of at least 16 bytes, so the table stays within a small multiple of
/// the bytes the guest has read from inotify descriptors (an entry costs
/// 12-20 bytes of coordinator memory: about 1-1.5 bytes per byte read), held
/// for the rest of the run.
#[derive(Debug, Default)]
pub(crate) struct CookieTable {
    last: u32,
    virtual_by_host: BTreeMap<u32, u32>,
}

impl CookieTable {
    /// The virtual cookie for `host`: 0 for 0 (an event that is not part of a
    /// rename), the value already given to `host`, or the next one.
    pub(crate) fn virtualize(&mut self, host: u32) -> u32 {
        if host == 0 {
            return 0;
        }
        if let Some(&virtual_cookie) = self.virtual_by_host.get(&host) {
            return virtual_cookie;
        }
        self.last = self.last.wrapping_add(1);
        if self.last == 0 {
            self.last = 1;
        }
        self.virtual_by_host.insert(host, self.last);
        self.last
    }
}

/// Where a read of an inotify descriptor puts what it reads: the address and
/// length of each of its buffers, in order. Captured before the call: Linux
/// imports a vectored read's iovecs before it reads, and the events it writes
/// may overwrite them.
#[derive(Debug)]
pub(crate) struct Destination(Vec<(usize, usize)>);

impl Destination {
    /// The buffer of `read(fd, buf, count)`.
    pub(crate) fn buffer(buf: usize, count: usize) -> Self {
        Self(vec![(buf, count)])
    }

    /// The `count` iovecs at `iov` of a `readv` or `preadv2`, read now.
    /// `None` when Linux refuses the count (more than `UIO_MAXIOV`: EINVAL),
    /// or Hermit cannot read the array, which the kernel then fails with
    /// EFAULT unless only the kernel can read it.
    pub(crate) fn iovecs<M: MemoryAccess>(memory: &M, iov: usize, count: usize) -> Option<Self> {
        let count = crate::io_buffers::linux_iovec_count(count as u64);
        if count > libc::UIO_MAXIOV as usize {
            return None;
        }
        let mut raw = vec![0u8; count * IOVEC];
        if count > 0 {
            let start = AddrMut::<u8>::from_raw(iov)?;
            crate::io_buffers::read_written(memory, start, &mut raw).ok()?;
        }
        let word = |bytes: &[u8]| usize::from_ne_bytes(bytes.try_into().expect("eight bytes"));
        Some(Self(
            raw.chunks(IOVEC)
                .map(|iovec| (word(&iovec[..8]), word(&iovec[8..])))
                .collect(),
        ))
    }

    /// The parts of the buffers that a read returning `length` bytes filled:
    /// the first `length` bytes of their concatenation. Linux fills each
    /// buffer before it moves to the next (inotify has no `read_iter`, so
    /// `do_loop_readv_writev` reads one iovec at a time and stops at the first
    /// it does not fill).
    fn filled(&self, length: usize) -> Vec<(usize, usize)> {
        let mut pieces = Vec::new();
        let mut remaining = length;
        for &(address, size) in &self.0 {
            if remaining == 0 {
                break;
            }
            let take = size.min(remaining);
            if take > 0 {
                pieces.push((address, take));
            }
            remaining -= take;
        }
        pieces
    }

    /// The buffer that a read which returned `length` bytes may have
    /// written past them before a fault: the first nonempty buffer after the
    /// ones it filled, when they end exactly at its start. Linux's readv goes
    /// on to the next iovec only after filling one, and returns the bytes
    /// before an iovec whose read fails (do_loop_readv_writev). A buffer
    /// shorter than an event header is never written: inotify_read refuses a
    /// count smaller than the next event before it copies anything.
    fn possibly_written(&self, length: usize) -> Option<(usize, usize)> {
        let mut filled = 0usize;
        for &(address, size) in &self.0 {
            if size == 0 {
                continue;
            }
            if filled == length {
                return (size >= EVENT_HEADER).then_some((address, size));
            }
            filled = filled.checked_add(size)?;
            if filled > length {
                return None;
            }
        }
        None
    }

    /// What [`finish`] does after a read returned `length` bytes: renumber
    /// the events in the buffers it filled, unless those, or the buffer it may
    /// have written before a fault, overlap.
    fn plan(&self, length: usize) -> Plan {
        let filled = self.filled(length);
        let mut written = filled.clone();
        written.extend(self.possibly_written(length));
        if overlapping(&written) {
            Plan::LeaveOverlapping(written)
        } else {
            Plan::Renumber(filled)
        }
    }
}

/// What [`finish`] does with the buffers of a read ([`Destination::plan`]).
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Renumber the cookies of the events the read returned in these
    /// buffers.
    Renumber(Vec<(usize, usize)>),
    /// Leave every byte as Linux wrote it, and ask global state nothing: the
    /// buffers the read wrote, or may have written before a fault, overlap,
    /// so the bytes left are not the events it returned.
    LeaveOverlapping(Vec<(usize, usize)>),
}

/// Whether two of `pieces` share a byte.
fn overlapping(pieces: &[(usize, usize)]) -> bool {
    let mut sorted: Vec<(usize, usize)> = pieces
        .iter()
        .copied()
        .filter(|&(_, size)| size > 0)
        .collect();
    sorted.sort_unstable();
    sorted
        .windows(2)
        .any(|pair| pair[0].0.saturating_add(pair[0].1) > pair[1].0)
}

/// The byte offset of each complete event's cookie in `records`, which start
/// at an event boundary, in order.
fn cookie_offsets(records: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut offset = 0usize;
    while offset + EVENT_HEADER <= records.len() {
        offsets.push(offset + EVENT_COOKIE);
        let length = u32::from_ne_bytes(
            records[offset + EVENT_LENGTH..offset + EVENT_HEADER]
                .try_into()
                .expect("the header holds a 4-byte length"),
        ) as usize;
        match offset
            .checked_add(EVENT_HEADER)
            .and_then(|end| end.checked_add(length))
        {
            Some(next) => offset = next,
            None => break,
        }
    }
    offsets
}

/// The cookie at `offset` in `records`.
fn cookie_at(records: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        records[offset..offset + 4]
            .try_into()
            .expect("a cookie is 4 bytes"),
    )
}

/// The bytes of `pieces`, concatenated. They are read as the io-buffer
/// observation reads what a syscall wrote (`read_written`), so a write-only
/// destination is read wherever the backend can read it.
fn gather<M: MemoryAccess>(memory: &M, pieces: &[(usize, usize)]) -> Result<Vec<u8>, Errno> {
    let mut records = vec![0u8; pieces.iter().map(|&(_, size)| size).sum()];
    let mut at = 0;
    for &(address, size) in pieces {
        let start = AddrMut::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
        crate::io_buffers::read_written(memory, start, &mut records[at..at + size])?;
        at += size;
    }
    Ok(records)
}

/// Where bytes `offset..offset + length` of the concatenation of `pieces`
/// are: each guest address that starts a run of them, with the range of the
/// bytes stored from it.
fn spans(pieces: &[(usize, usize)], offset: usize, length: usize) -> Vec<(usize, Range<usize>)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for &(address, size) in pieces {
        let end = start + size;
        let from = offset.max(start);
        let to = (offset + length).min(end);
        if from < to {
            spans.push((address + (from - start), from - offset..to - offset));
        }
        start = end;
    }
    spans
}

/// Write each `(offset, cookie)` of `changes`, whose offset is into the
/// concatenation of `pieces`, to the guest memory that holds those bytes.
/// Nothing else is written.
fn write_cookies<M: MemoryAccess>(
    memory: &mut M,
    pieces: &[(usize, usize)],
    changes: &[(usize, u32)],
) -> Result<(), Errno> {
    for &(offset, cookie) in changes {
        let bytes = cookie.to_ne_bytes();
        for (address, range) in spans(pieces, offset, bytes.len()) {
            let at = AddrMut::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
            memory.write_exact(at, &bytes[range])?;
        }
    }
    Ok(())
}

/// `pieces` as `address+length` terms, for a message.
fn describe(pieces: &[(usize, usize)]) -> String {
    pieces
        .iter()
        .map(|(address, length)| format!("{address:#x}+{length}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `(offset, virtual cookie)` of each cookie field whose virtual value
/// differs from its host value.
fn changed_cookies(offsets: &[usize], host: &[u32], virtual_cookies: &[u32]) -> Vec<(usize, u32)> {
    offsets
        .iter()
        .zip(host.iter().zip(virtual_cookies))
        .filter(|(_, (host, virtual_cookie))| host != virtual_cookie)
        .map(|(&offset, (_, &virtual_cookie))| (offset, virtual_cookie))
        .collect()
}

/// After `call`, a read of an inotify descriptor into `destination` (`None`:
/// iovecs Hermit could not read), returned `result`: put virtual cookies in
/// the events it read. The result is returned as it is, whatever happens
/// here.
pub(crate) async fn finish<G, T>(
    guest: &mut G,
    call: Sysno,
    destination: Option<Destination>,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let length = match &result {
        Ok(length) => match usize::try_from(*length) {
            Ok(length) if length >= EVENT_HEADER => length,
            _ => return result,
        },
        Err(Error::Errno(Errno::EFAULT)) => {
            warn!(
                "[detcore, dtid {dettid}] an inotify {call} failed with EFAULT: any events Linux \
                 copied before the fault keep their host rename cookies \
                 (https://github.com/rrnewton/hermit/issues/4793)"
            );
            return result;
        }
        Err(_) => return result,
    };
    let Some(destination) = destination else {
        warn!(
            "[detcore, dtid {dettid}] an inotify {call}'s events keep their host rename cookies: \
             Hermit cannot read its iovecs (https://github.com/rrnewton/hermit/issues/4530)"
        );
        return result;
    };
    let pieces = match destination.plan(length) {
        Plan::Renumber(pieces) => pieces,
        Plan::LeaveOverlapping(written) => {
            warn!(
                "[detcore, dtid {dettid}] an inotify {call}'s events at {} keep their host \
                 rename cookies: its iovecs overlap, so the bytes left are not the events Linux \
                 returned in order, and they are left as Linux wrote them",
                describe(&written)
            );
            return result;
        }
    };
    let records = match gather(&guest.memory(), &pieces) {
        Ok(records) => records,
        Err(error) => {
            warn!(
                "[detcore, dtid {dettid}] the events an inotify {call} wrote at {} keep their \
                 host rename cookies: Hermit cannot read them ({error}; \
                 https://github.com/rrnewton/hermit/issues/4530)",
                describe(&pieces)
            );
            return result;
        }
    };
    let offsets = cookie_offsets(&records);
    let host: Vec<u32> = offsets
        .iter()
        .map(|&offset| cookie_at(&records, offset))
        .collect();
    if host.iter().all(|cookie| *cookie == 0) {
        return result;
    }
    let virtual_cookies = crate::tool_global::virtual_inotify_cookies(guest, host.clone()).await;
    let changes = changed_cookies(&offsets, &host, &virtual_cookies);
    if let Err(error) = write_cookies(&mut guest.memory(), &pieces, &changes) {
        warn!(
            "[detcore, dtid {dettid}] the events an inotify {call} wrote at {} keep some host \
             rename cookies: Hermit cannot write them ({error})",
            describe(&pieces)
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io;

    use reverie::syscalls::Addr;

    use super::*;

    /// A host event: wd 1, `IN_MOVED_FROM`, `cookie`, and `name` (already
    /// padded).
    fn event(cookie: u32, name: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_i32.to_ne_bytes());
        bytes.extend_from_slice(&0x40_u32.to_ne_bytes());
        bytes.extend_from_slice(&cookie.to_ne_bytes());
        bytes.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        bytes.extend_from_slice(name);
        bytes
    }

    #[test]
    fn cookies_are_numbered_by_first_observation_and_pairs_share_one() {
        let mut table = CookieTable::default();
        assert_eq!(table.virtualize(0), 0);
        assert_eq!(table.virtualize(188_301_299), 1);
        assert_eq!(table.virtualize(188_301_299), 1, "the pair's second event");
        assert_eq!(table.virtualize(5), 2);
        assert_eq!(table.virtualize(188_301_299), 1);
        assert_eq!(table.virtualize(0), 0);
    }

    /// A rename whose `IN_MOVED_TO` half, and another instance's copy of the
    /// pair, are read only after 70,000 other renames were observed (more
    /// than the 65,536 entries of a table that evicted its oldest) keeps the
    /// value its first half got.
    #[test]
    fn a_delayed_half_and_a_delayed_instance_keep_the_first_value() {
        let mut table = CookieTable::default();
        let delayed = 4_000_000_000;
        assert_eq!(table.virtualize(delayed), 1, "IN_MOVED_FROM, read first");
        for host in 1..=70_000 {
            assert_eq!(table.virtualize(host), host + 1);
        }
        assert_eq!(table.virtualize(delayed), 1, "its IN_MOVED_TO, read late");
        assert_eq!(table.virtualize(delayed), 1, "another instance's copy");
        assert_eq!(table.virtualize(70_001), 70_002, "numbering goes on");
    }

    #[test]
    fn cookie_offsets_follow_each_event_s_name_length() {
        let mut records = event(7, b"a\0\0\0");
        records.extend(event(0, b""));
        records.extend(event(9, b"longer-name\0\0\0\0\0"));
        assert_eq!(cookie_offsets(&records), [8, 28, 44]);
        // A trailing partial header is not an event.
        records.extend_from_slice(&[0; 10]);
        assert_eq!(cookie_offsets(&records), [8, 28, 44]);
        assert_eq!(cookie_offsets(&records[..15]), Vec::<usize>::new());
    }

    /// A readv's bytes fill each iovec in turn, skipping empty ones, and stop
    /// at the length read; a read's single buffer takes them all.
    #[test]
    fn a_read_fills_its_buffers_in_order() {
        let iovecs = Destination(vec![(0x1000, 32), (0x9000, 0), (0x2000, 64), (0x3000, 100)]);
        assert_eq!(iovecs.filled(32), [(0x1000, 32)]);
        assert_eq!(iovecs.filled(64), [(0x1000, 32), (0x2000, 32)]);
        assert_eq!(
            iovecs.filled(128),
            [(0x1000, 32), (0x2000, 64), (0x3000, 32)]
        );
        assert_eq!(iovecs.filled(0), []);
        assert_eq!(Destination::buffer(0x5000, 4096).filled(48), [(0x5000, 48)]);
    }

    /// Iovecs that share a byte overlap; adjacent ones, and empty ones, do not.
    #[test]
    fn overlapping_iovecs_are_found() {
        assert!(overlapping(&[(0x1000, 16), (0x1008, 16)]));
        assert!(overlapping(&[(0x1000, 32), (0x1000, 4096)]), "one address");
        assert!(
            overlapping(&[(0x1008, 16), (0x1000, 16)]),
            "in either order"
        );
        assert!(overlapping(&[(0x3000, 8), (0x1000, 0x1001), (0x2000, 8)]));
        assert!(!overlapping(&[(0x1000, 16), (0x1010, 16)]), "adjacent");
        assert!(!overlapping(&[(0x1010, 16), (0x1000, 16)]));
        assert!(!overlapping(&[(0x1000, 0), (0x1000, 16)]), "an empty iovec");
        assert!(!overlapping(&[(0x1000, 16)]));
        assert!(overlapping(&[(usize::MAX - 4, 16), (usize::MAX - 2, 1)]));
    }

    /// `finish` leaves a read alone, without asking global state, exactly when
    /// the buffers it wrote overlap: those it filled, and the next one when
    /// the count ends at its start, which it may have written before a fault.
    #[test]
    fn a_read_into_overlapping_buffers_is_left_alone() {
        // The case Linux was measured on: two 16-byte IN_IGNORED events read
        // into {buf, 16} and {buf + 8, 16}.
        let overlapping = Destination(vec![(0x1000, 16), (0x1008, 16)]);
        assert_eq!(
            overlapping.plan(32),
            Plan::LeaveOverlapping(vec![(0x1000, 16), (0x1008, 16)])
        );
        // One event returned: the second buffer may hold the start of the
        // next event, copied before a fault over the first event's last bytes.
        assert_eq!(
            overlapping.plan(16),
            Plan::LeaveOverlapping(vec![(0x1000, 16), (0x1008, 16)])
        );
        // A next buffer shorter than an event header is never written, so a
        // completed pair before it is renumbered.
        let short_next = Destination(vec![(0x1000, 64), (0x1008, 1)]);
        assert_eq!(short_next.plan(64), Plan::Renumber(vec![(0x1000, 64)]));
        let apart = Destination(vec![(0x1000, 16), (0x2000, 64), (0x2000, 64)]);
        assert_eq!(
            apart.plan(48),
            Plan::Renumber(vec![(0x1000, 16), (0x2000, 32)]),
            "the third iovec, which overlaps the second, was never reached"
        );
        assert_eq!(
            apart.plan(16),
            Plan::Renumber(vec![(0x1000, 16)]),
            "the second iovec may hold bytes copied before a fault, but overlaps nothing written"
        );
        assert_eq!(
            apart.plan(80),
            Plan::LeaveOverlapping(vec![(0x1000, 16), (0x2000, 64), (0x2000, 64)]),
            "the second iovec filled, the third may hold bytes copied before a fault"
        );
    }

    /// The iovec geometry is what the array held when it was read, before
    /// the call; an array Hermit cannot read, or a count Linux refuses, gives
    /// none.
    #[test]
    fn iovecs_are_read_from_the_array_before_the_call() {
        let mut array = Vec::new();
        for (base, length) in [(0x1000_usize, 32_usize), (0x2000, 64)] {
            array.extend_from_slice(&base.to_ne_bytes());
            array.extend_from_slice(&length.to_ne_bytes());
        }
        let memory = Model::new(BASE, &array, true);
        let destination = Destination::iovecs(&memory, BASE, 2).unwrap();
        assert_eq!(destination.0, [(0x1000, 32), (0x2000, 64)]);
        // Linux takes the count as an unsigned int.
        assert_eq!(
            Destination::iovecs(&memory, BASE, (1 << 32) | 1).unwrap().0,
            [(0x1000, 32)]
        );
        assert!(Destination::iovecs(&memory, BASE, 1025).is_none());
        assert!(Destination::iovecs(&memory, BASE + 0x200 - 16, 2).is_none());
        assert!(Destination::iovecs(&memory, 0, 0).unwrap().0.is_empty());
    }

    /// Bytes of the concatenation are found in the piece that holds them,
    /// including a field split across two pieces.
    #[test]
    fn spans_map_concatenated_bytes_to_guest_addresses() {
        let pieces = [(0x1000, 32), (0x2000, 6), (0x3000, 10)];
        assert_eq!(spans(&pieces, 8, 4), [(0x1008, 0..4)]);
        assert_eq!(spans(&pieces, 30, 4), [(0x101e, 0..2), (0x2000, 2..4)]);
        assert_eq!(
            spans(&pieces, 36, 4),
            [(0x2004, 0..2), (0x3000, 2..4)],
            "a field over the end of the middle piece"
        );
        assert_eq!(spans(&pieces, 44, 4), [(0x3006, 0..4)]);
    }

    /// Only the cookie fields whose value changes are written; every other
    /// byte, a zero cookie and a cookie already equal to its virtual value
    /// included, is left as the read wrote it.
    #[test]
    fn only_changed_cookie_fields_are_written() {
        let mut records = event(188_656_515, b"first\0\0\0\0\0\0\0\0\0\0\0");
        records.extend(event(0, b""));
        records.extend(event(2, b"x\0\0\0"));
        let offsets = cookie_offsets(&records);
        let host: Vec<u32> = offsets.iter().map(|&at| cookie_at(&records, at)).collect();
        assert_eq!(host, [188_656_515, 0, 2]);
        let changes = changed_cookies(&offsets, &host, &[1, 0, 2]);
        assert_eq!(changes, [(8, 1)]);

        // The records split over two buffers, the first cut 2 bytes into the
        // first event's cookie, as no kernel splits them, to check the scatter.
        let (first, second) = records.split_at(10);
        let mut memory = Model::new(BASE, &records, true);
        let pieces = [(BASE, first.len()), (BASE + 0x100, second.len())];
        memory.bytes[0x100..0x100 + second.len()].copy_from_slice(second);
        memory.bytes.truncate(0x100 + second.len());
        let before = memory.bytes.clone();
        write_cookies(&mut memory, &pieces, &changes).unwrap();
        let mut expected = before;
        expected[8..10].copy_from_slice(&1_u32.to_ne_bytes()[..2]);
        expected[0x100..0x102].copy_from_slice(&1_u32.to_ne_bytes()[2..]);
        assert_eq!(memory.bytes, expected);
        assert_eq!(memory.writes, [(BASE + 8, 2), (BASE + 0x100, 2)]);
    }

    /// Events in a write-only destination are gathered with the word reads
    /// that ptrace's `PTRACE_PEEKDATA` serves; where no read can reach them,
    /// as on the in-process backends, gathering fails, and `finish` then
    /// keeps the read's result and leaves the events as they are.
    #[test]
    fn a_write_only_destination_is_gathered_where_the_backend_can_read_it() {
        let mut records = event(188_656_515, b"first\0\0\0\0\0\0\0\0\0\0\0");
        records.extend(event(188_656_515, b"second\0\0\0\0\0\0\0\0\0\0"));
        let pieces = [(BASE, 32), (BASE + 32, 32)];
        let readable = Model::new(BASE, &records, true);
        assert_eq!(gather(&readable, &pieces).unwrap(), records);
        let peeks = Model::new(BASE, &records, false);
        assert_eq!(gather(&peeks, &pieces).unwrap(), records);
        let unreadable = Model {
            peeks: false,
            ..Model::new(BASE, &records, false)
        };
        assert_eq!(gather(&unreadable, &pieces), Err(Errno::EFAULT));
    }

    const BASE: usize = 0x10000;

    /// Guest memory at `base`: `process_vm_readv`/`process_vm_writev` when
    /// `readable` (a write-only page refuses the read), and eight-byte word
    /// reads when `peeks`, as ptrace's `PTRACE_PEEKDATA` reads any mapped
    /// byte.
    struct Model {
        base: usize,
        bytes: Vec<u8>,
        readable: bool,
        peeks: bool,
        writes: Vec<(usize, usize)>,
    }

    impl Model {
        fn new(base: usize, bytes: &[u8], readable: bool) -> Self {
            let mut bytes = bytes.to_vec();
            bytes.resize(bytes.len().max(0x200), 0);
            Self {
                base,
                bytes,
                readable,
                peeks: true,
                writes: Vec::new(),
            }
        }

        fn range(&self, start: usize, length: usize) -> Result<Range<usize>, Errno> {
            let offset = start.checked_sub(self.base).ok_or(Errno::EFAULT)?;
            let end = offset.checked_add(length).ok_or(Errno::EFAULT)?;
            (end <= self.bytes.len())
                .then_some(offset..end)
                .ok_or(Errno::EFAULT)
        }

        /// `process_vm_readv` of `buf.len()` bytes at `start`.
        fn readv(&self, start: usize, buf: &mut [u8]) -> Result<usize, Errno> {
            if !self.readable {
                return Err(Errno::EFAULT);
            }
            buf.copy_from_slice(&self.bytes[self.range(start, buf.len())?]);
            Ok(buf.len())
        }
    }

    impl MemoryAccess for Model {
        fn read_vectored(
            &self,
            remote: &[io::IoSlice],
            local: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            let ([remote], [local]) = (remote, local) else {
                panic!("one extent per vectored read")
            };
            self.readv(remote.as_ptr() as usize, local)
        }

        fn write_vectored(
            &mut self,
            local: &[io::IoSlice],
            remote: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            let ([local], [remote]) = (local, remote) else {
                panic!("one extent per vectored write")
            };
            let start = remote.as_ptr() as usize;
            let range = self.range(start, local.len())?;
            self.bytes[range].copy_from_slice(local);
            self.writes.push((start, local.len()));
            Ok(local.len())
        }

        fn read<'a, A>(&self, addr: A, buf: &mut [u8]) -> Result<usize, Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            let start = addr.into().as_raw();
            if buf.len() > 8 || !self.peeks {
                return self.readv(start, buf);
            }
            let range = self.range(start, 8).map_err(|_| Errno::EIO)?;
            buf.copy_from_slice(&self.bytes[range][..buf.len()]);
            Ok(buf.len())
        }
    }
}
