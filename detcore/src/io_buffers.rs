/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hash the bytes a syscall moved through a guest buffer, at the syscall
//! boundary.
//!
//! WHAT GAP THIS FILLS. `--detlog-stack` and `--detlog-heap` hash a whole named
//! mapping, so what they can see is decided by where the guest happened to
//! ALLOCATE a buffer rather than by what the syscall did. Measured 2026-08-20,
//! three runs per cell, running the same netlink `RTM_GETLINK` exchange and
//! changing only the receive buffer's home:
//!
//! | buffer home     | `--detlog-stack` | `--detlog-heap` | both   |
//! |-----------------|------------------|-----------------|--------|
//! | `[stack]`       | CAUGHT           | MISSED          | CAUGHT |
//! | `[heap]` (brk)  | MISSED           | CAUGHT          | CAUGHT |
//! | BSS / static    | MISSED           | MISSED          | MISSED |
//! | anonymous mmap  | MISSED           | MISSED          | MISSED |
//!
//! Two of the four are invisible even with both flags on, and anonymous mmap is
//! not a corner case: it is where glibc puts any `malloc` above the 128 KiB
//! `M_MMAP_THRESHOLD`. Taking the address and length from the SYSCALL ARGUMENTS
//! instead makes all four rows irrelevant by construction.
//!
//! WHAT IT CATCHES THAT `--verify` CANNOT. `--verify` compares the INFO record,
//! and a syscall whose output buffer is typed as a bare pointer in Reverie
//! prints the address, not the contents (`reverie-syscalls/src/syscalls.rs`
//! carries standing TODOs saying exactly this for `Read` and `Write`). So a
//! `recvmsg` that returns a stable `Ok(1468)` while four bytes of its payload
//! differ produces a character-identical record and `--verify` reports
//! `bitwise_parity: true`. Measured on one QEMU/Linux boot, 278,824 of 632,228
//! syscalls (44.1%) move bytes through a buffer whose content the log never
//! shows.
//!
//! COST SHAPE, and it differs from the whole-mapping flags in kind rather than
//! degree. `--detlog-heap` is `O(syscalls x region_size)` -- it re-reads the
//! entire heap after every syscall, hashing 10.9 TB per boot to watch a 16.49
//! MiB heap. This is `O(bytes the syscalls actually returned)`: 139.1 MB per
//! boot, measured by summing real return values.

use reverie::Error;
use reverie::Guest;
use reverie::Tool;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use crate::procmaps::compute_hash_range;
use crate::types::DetTid;

/// One contiguous run of guest bytes a completed syscall moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferExtent {
    /// Guest virtual address of the first byte.
    pub addr: u64,
    /// Number of bytes actually moved, which is bounded by the syscall's return
    /// value and not by the buffer's declared capacity.
    pub len: u64,
}

/// Direction of travel, recorded so a reader can tell a value the kernel
/// produced from one the guest produced without knowing every syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The kernel filled the buffer: an unvirtualized field here is a leak INTO
    /// the guest, which is the netlink case.
    In,
    /// The guest filled the buffer: a difference here means the guest computed
    /// different bytes, which is how divergence reaches a file or a device.
    Out,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// Clamp a declared buffer to what the syscall says it actually moved.
///
/// `capacity` is the argument the guest passed; `moved` is the return value.
/// Hashing `capacity` would fold in bytes the syscall never wrote, which are
/// whatever was in the buffer beforehand -- for a stack buffer that is previous
/// frames, so the hash would report divergence for an unrelated reason.
fn clamp(addr: Option<u64>, capacity: usize, moved: i64) -> Vec<BufferExtent> {
    let Some(addr) = addr else { return Vec::new() };
    let moved = u64::try_from(moved).unwrap_or(0);
    let len = moved.min(capacity as u64);
    if len == 0 {
        return Vec::new();
    }
    vec![BufferExtent { addr, len }]
}

/// A buffer written wholly in place, whose size does not depend on the return
/// value (`poll`'s `revents` are rewritten across the whole array regardless of
/// how many descriptors were ready).
fn whole(addr: Option<u64>, len: u64) -> Vec<BufferExtent> {
    match addr {
        Some(addr) if len > 0 => vec![BufferExtent { addr, len }],
        _ => Vec::new(),
    }
}

/// Walk an `iovec` array and return the segments the syscall actually filled,
/// bounded by `moved`.
///
/// Mirrors the existing traversal in `crate::syscalls::io`: clamp the count to
/// `UIO_MAXIOV`, skip null/empty segments, and stop once `moved` bytes are
/// accounted for. Under `MSG_TRUNC` the returned count can exceed the buffers'
/// capacity, which is why the running remainder rather than `moved` alone
/// bounds each segment.
fn iovec_extents<G, T>(
    guest: &mut G,
    iov_addr: usize,
    iov_count: usize,
    moved: i64,
) -> Result<Vec<BufferExtent>, Error>
where
    G: Guest<T>,
    T: Tool,
{
    let mut remaining = u64::try_from(moved).unwrap_or(0);
    if iov_addr == 0 || iov_count == 0 || remaining == 0 {
        return Ok(Vec::new());
    }
    let iov_count = iov_count.min(libc::UIO_MAXIOV as usize);
    let iov_address: AddrMut<'_, libc::iovec> = AddrMut::from_raw(iov_addr).ok_or(Errno::EFAULT)?;
    // SAFETY: `iovec` is a plain C record; an all-zero value is a valid staging
    // value that `read_values` immediately overwrites.
    let mut iovecs: Vec<libc::iovec> = (0..iov_count)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();
    guest
        .memory()
        .read_values(iov_address.into(), &mut iovecs)?;

    let mut out = Vec::new();
    for iov in &iovecs {
        if remaining == 0 {
            break;
        }
        if iov.iov_base.is_null() || iov.iov_len == 0 {
            continue;
        }
        let take = (iov.iov_len as u64).min(remaining);
        out.push(BufferExtent {
            addr: iov.iov_base as u64,
            len: take,
        });
        remaining -= take;
    }
    Ok(out)
}

/// Read a `msghdr` out of the guest and walk the iovecs it points at.
fn msghdr_extents<G, T>(
    guest: &mut G,
    msg_addr: usize,
    moved: i64,
) -> Result<Vec<BufferExtent>, Error>
where
    G: Guest<T>,
    T: Tool,
{
    if msg_addr == 0 {
        return Ok(Vec::new());
    }
    let address: AddrMut<'_, libc::msghdr> = AddrMut::from_raw(msg_addr).ok_or(Errno::EFAULT)?;
    let message: libc::msghdr = guest.memory().read_value(address)?;
    iovec_extents(guest, message.msg_iov as usize, message.msg_iovlen, moved)
}

/// The extents a completed syscall moved, or `None` when this syscall has no
/// output buffer worth hashing.
///
/// Only syscalls whose buffer CONTENT the INFO record does not already show are
/// listed. `clock_gettime` and `newfstatat`, for instance, are absent because
/// Reverie's typed display already dereferences and prints their output.
fn extents<G, T>(guest: &mut G, call: &Syscall, ret: i64) -> Result<Vec<BufferExtent>, Error>
where
    G: Guest<T>,
    T: Tool,
{
    // Nothing was written on a failed or empty call.
    if ret <= 0 {
        return Ok(Vec::new());
    }
    let raw = |a: Option<AddrMut<'_, u8>>| a.map(|p| p.as_raw() as u64);
    Ok(match call {
        // Bytes the kernel produced.
        Syscall::Read(c) => clamp(raw(c.buf()), c.len(), ret),
        Syscall::Pread64(c) => clamp(raw(c.buf()), c.len(), ret),
        Syscall::Recvfrom(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Getrandom(c) => clamp(raw(c.buf()), c.buflen(), ret),
        Syscall::Getcwd(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.size(), ret),
        Syscall::Getdents64(c) => clamp(
            c.dirent().map(|p| p.as_raw() as u64),
            c.count() as usize,
            ret,
        ),
        Syscall::Readlink(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.bufsize(), ret),
        Syscall::Readlinkat(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.buf_len(), ret),
        Syscall::Recvmsg(c) => msghdr_extents(guest, c.msg().map_or(0, |p| p.as_raw()), ret)?,
        Syscall::Readv(c) => iovec_extents(guest, c.iov().map_or(0, |p| p.as_raw()), c.len(), ret)?,
        Syscall::Preadv(c) => {
            iovec_extents(guest, c.iov().map_or(0, |p| p.as_raw()), c.iov_len(), ret)?
        }

        // Bytes the guest produced. These never reach stdout/stderr for a QEMU
        // boot -- measured, all 234,872 writes went to fds 7/12/14/11/13/4/8/19/23
        // and none to fd 1 or 2 -- so `--verify`'s stdout/stderr comparison does
        // not cover them either.
        Syscall::Write(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Pwrite64(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Sendto(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.size(), ret),
        Syscall::Sendmsg(c) => msghdr_extents(guest, c.msg().map_or(0, |p| p.as_raw()), ret)?,
        Syscall::Writev(c) => {
            iovec_extents(guest, c.iov().map_or(0, |p| p.as_raw()), c.len(), ret)?
        }
        Syscall::Pwritev(c) => {
            iovec_extents(guest, c.iov().map_or(0, |p| p.as_raw()), c.iov_len(), ret)?
        }

        // Rewritten in place across the WHOLE array: `poll` sets `revents` on
        // every entry, not just on the `ret` that were ready, so the extent is
        // the array and not a prefix of it.
        Syscall::Poll(c) => whole(
            c.fds().map(|p| p.as_raw() as u64),
            c.nfds() * std::mem::size_of::<libc::pollfd>() as u64,
        ),
        Syscall::Ppoll(c) => whole(
            c.fds().map(|p| p.as_raw() as u64),
            c.nfds() * std::mem::size_of::<libc::pollfd>() as u64,
        ),

        _ => Vec::new(),
    })
}

/// Which way the bytes travelled, for the record's label.
fn direction(call: &Syscall) -> Direction {
    match call {
        Syscall::Write(_)
        | Syscall::Pwrite64(_)
        | Syscall::Sendto(_)
        | Syscall::Sendmsg(_)
        | Syscall::Writev(_)
        | Syscall::Pwritev(_) => Direction::Out,
        _ => Direction::In,
    }
}

/// Emit one deterministic record per buffer a completed syscall moved.
///
/// ⚠️ THE GUARD IS FIRST AND THAT IS THE POINT. Everything below it touches
/// guest memory: for `recvmsg` the extents cannot even be computed without
/// reading a `msghdr` and an `iovec` array out of the guest. That is
/// preparatory work done BEFORE the `detlog!`, which is exactly the shape that
/// made `--detlog-stack` and `--detlog-heap` cost 4.36x and 4.76x on a boot
/// with logging off, producing 123 bytes of log. `detlog_observed!()` is
/// checked before any of it so the disabled path is genuinely inert, which is
/// the property a default-on check has to have.
pub(crate) fn detlog_io_buffers<G, T>(
    guest: &mut G,
    call: &Syscall,
    ret: i64,
    dettid: DetTid,
) -> Result<(), Error>
where
    G: Guest<T>,
    T: Tool,
{
    if !crate::detlog_observed!() {
        return Ok(());
    }
    let dir = direction(call).as_str();
    let name = call.name();
    for extent in extents(guest, call, ret)? {
        crate::detlog!(
            "[iobuf][dtid {}] {} {} {:#x}+{}->{}",
            dettid,
            name,
            dir,
            extent.addr,
            extent.len,
            compute_hash_range(guest, extent.addr, extent.addr + extent.len)?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_bounds_the_extent_by_what_was_moved_not_by_capacity() {
        // A 4096-byte buffer that received 10 bytes must hash 10, not 4096:
        // the other 4086 are whatever was there before, which for a stack
        // buffer is previous frames and would report unrelated divergence.
        assert_eq!(
            clamp(Some(0x1000), 4096, 10),
            vec![BufferExtent {
                addr: 0x1000,
                len: 10
            }]
        );
    }

    #[test]
    fn clamp_never_exceeds_capacity() {
        // MSG_TRUNC lets a receive report more bytes than the buffer held.
        assert_eq!(
            clamp(Some(0x1000), 64, 4096),
            vec![BufferExtent {
                addr: 0x1000,
                len: 64
            }]
        );
    }

    #[test]
    fn nothing_is_hashed_for_a_null_buffer_or_an_empty_move() {
        assert!(clamp(None, 4096, 10).is_empty());
        assert!(clamp(Some(0x1000), 4096, 0).is_empty());
        assert!(clamp(Some(0x1000), 4096, -1).is_empty());
    }

    #[test]
    fn poll_hashes_the_whole_array_not_a_prefix() {
        // `poll` writes `revents` on every entry regardless of the return
        // value, so three descriptors are 3 * sizeof(pollfd) = 24 bytes even
        // when the call returns 0 ready.
        assert_eq!(
            whole(Some(0x2000), 3 * std::mem::size_of::<libc::pollfd>() as u64),
            vec![BufferExtent {
                addr: 0x2000,
                len: 24
            }]
        );
        assert!(whole(None, 24).is_empty());
        assert!(whole(Some(0x2000), 0).is_empty());
    }

    #[test]
    fn direction_separates_kernel_produced_from_guest_produced() {
        assert_eq!(Direction::In.as_str(), "in");
        assert_eq!(Direction::Out.as_str(), "out");
    }
}
