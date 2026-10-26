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
//! advance (`fsnotify_get_cookie`, fs/notify/fsnotify.c), so the values a
//! guest reads depend on everything else renaming files on the host
//! (https://github.com/rrnewton/hermit/issues/4765). After a successful
//! `read` or `readv` of an inotify descriptor, each nonzero cookie in the
//! events read is replaced by a virtual one, numbered from 1 in the order the
//! run first observes it ([`CookieTable`], kept in global state through
//! `GlobalRequest::VirtualInotifyCookies`).
//!
//! Determinism: the events come from the guest's own filesystem operations,
//! and the reads that observe them run in the scheduler's order, so the order
//! of first observations, and with it every virtual cookie, is a function of
//! the schedule. The table is run-wide, so every inotify instance and every
//! process agrees on a rename's cookie, as on Linux, and a pair keeps one
//! value. A read that carries no rename (every cookie zero) is left alone and
//! asks global state nothing.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// `sizeof(struct inotify_event)`: `wd`, `mask`, `cookie`, `len`.
const EVENT_HEADER: usize = 16;
/// Offset of `cookie` within `struct inotify_event`.
const EVENT_COOKIE: usize = 8;
/// Offset of `len` within `struct inotify_event`.
const EVENT_LENGTH: usize = 12;
/// Host cookies remembered for pairing. A pair is normally read together; the
/// bound only limits memory for a program that renames without ever reading.
const COOKIE_CAPACITY: usize = 1 << 16;

/// Run-wide map from host rename cookies to virtual ones.
#[derive(Debug, Default)]
pub(crate) struct CookieTable {
    last: u32,
    virtual_by_host: BTreeMap<u32, u32>,
    host_by_age: VecDeque<u32>,
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
        if self.host_by_age.len() == COOKIE_CAPACITY
            && let Some(oldest) = self.host_by_age.pop_front()
        {
            self.virtual_by_host.remove(&oldest);
        }
        self.virtual_by_host.insert(host, self.last);
        self.host_by_age.push_back(host);
        self.last
    }
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

/// Replace the cookies of the events in `records` with their virtual ones,
/// asking global state only when some cookie is nonzero. Returns whether
/// `records` changed.
async fn virtualize_records<G, T>(guest: &mut G, records: &mut [u8]) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let offsets = cookie_offsets(records);
    let host: Vec<u32> = offsets
        .iter()
        .map(|&offset| {
            u32::from_ne_bytes(
                records[offset..offset + 4]
                    .try_into()
                    .expect("a cookie is 4 bytes"),
            )
        })
        .collect();
    if host.iter().all(|cookie| *cookie == 0) {
        return false;
    }
    let virtual_cookies = crate::tool_global::virtual_inotify_cookies(guest, host.clone()).await;
    if virtual_cookies.len() != host.len() || virtual_cookies == host {
        return false;
    }
    for (offset, cookie) in offsets.into_iter().zip(virtual_cookies) {
        records[offset..offset + 4].copy_from_slice(&cookie.to_ne_bytes());
    }
    true
}

/// After a `read` of an inotify descriptor into the buffer at address `buf`
/// returned `result`: put virtual cookies in the events it read.
pub(crate) async fn finish_read<G, T>(
    guest: &mut G,
    buf: usize,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(length) = read_length(&result) else {
        return result;
    };
    let Some(base) = AddrMut::<u8>::from_raw(buf) else {
        return result;
    };
    let pieces = vec![(base.as_raw(), length)];
    finish(guest, pieces, result).await
}

/// After a `readv` of an inotify descriptor with the `iovcnt` iovecs at
/// address `iov` returned `result`. The kernel scatters the events through
/// the iovecs in order, so the cookies are gathered and scattered back the
/// same way.
pub(crate) async fn finish_readv<G, T>(
    guest: &mut G,
    iov: usize,
    iovcnt: usize,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(length) = read_length(&result) else {
        return result;
    };
    let pieces = filled_iovecs(guest, iov, iovcnt, length)?;
    finish(guest, pieces, result).await
}

/// The byte count of a successful read long enough to hold an event.
fn read_length(result: &Result<i64, Error>) -> Option<usize> {
    let length = usize::try_from(*result.as_ref().ok()?).ok()?;
    (length >= EVENT_HEADER).then_some(length)
}

/// The (address, length) of the filled part of each of the `iovcnt` iovecs at
/// `iov`, in order, for a read that filled `length` bytes.
fn filled_iovecs<G, T>(
    guest: &mut G,
    iov: usize,
    iovcnt: usize,
    length: usize,
) -> Result<Vec<(usize, usize)>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut pieces = Vec::new();
    let mut filled = 0usize;
    for index in 0..iovcnt {
        if filled == length {
            break;
        }
        let Some(slot) = syscalls::Addr::<libc::iovec>::from_raw(
            iov + index * std::mem::size_of::<libc::iovec>(),
        ) else {
            break;
        };
        let entry: libc::iovec = guest.memory().read_value(slot)?;
        let take = entry.iov_len.min(length - filled);
        if take == 0 {
            continue;
        }
        pieces.push((entry.iov_base as usize, take));
        filled += take;
    }
    Ok(pieces)
}

/// Gather the events in `pieces`, put virtual cookies in them, and scatter
/// them back when any cookie changed.
async fn finish<G, T>(
    guest: &mut G,
    pieces: Vec<(usize, usize)>,
    result: Result<i64, Error>,
) -> Result<i64, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut records = Vec::new();
    for &(address, take) in &pieces {
        let Some(base) = AddrMut::<u8>::from_raw(address) else {
            return result;
        };
        let start = records.len();
        records.resize(start + take, 0);
        guest
            .memory()
            .read_exact(base.cast(), &mut records[start..])?;
    }
    if virtualize_records(guest, &mut records).await {
        let mut written = 0;
        for (address, take) in pieces {
            let Some(base) = AddrMut::<u8>::from_raw(address) else {
                return result;
            };
            guest
                .memory()
                .write_exact(base, &records[written..written + take])?;
            written += take;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_table_forgets_its_oldest_cookie_at_capacity() {
        let mut table = CookieTable::default();
        for host in 1..=COOKIE_CAPACITY as u32 {
            assert_eq!(table.virtualize(host), host);
        }
        assert_eq!(table.virtualize(1), 1, "still remembered");
        assert_eq!(table.virtualize(1_000_000), COOKIE_CAPACITY as u32 + 1);
        assert_eq!(
            table.virtualize(1),
            COOKIE_CAPACITY as u32 + 2,
            "the oldest host cookie was dropped"
        );
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
}
