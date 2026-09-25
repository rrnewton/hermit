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
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use crate::digest::Digest;
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

/// The segments a guest `iovec` array declares, before any return-value bound.
///
/// Mirrors the existing traversal in `crate::syscalls::io`: skip null/empty
/// segments. Each extent's `len` here is the segment's CAPACITY; see
/// [`bound_segments`] for the bytes actually moved.
fn declared_segments(iovecs: &[libc::iovec]) -> Vec<BufferExtent> {
    iovecs
        .iter()
        .filter(|iov| !iov.iov_base.is_null() && iov.iov_len != 0)
        .map(|iov| BufferExtent {
            addr: iov.iov_base as u64,
            len: iov.iov_len as u64,
        })
        .collect()
}

/// Return the declared segments the syscall actually filled, bounded by
/// `moved`.
///
/// Stop once `moved` bytes are accounted for. Under `MSG_TRUNC` the returned
/// count can exceed the buffers' capacity, which is why the running remainder
/// rather than `moved` alone bounds each segment.
fn bound_segments(segments: &[BufferExtent], moved: i64) -> Vec<BufferExtent> {
    let mut remaining = u64::try_from(moved).unwrap_or(0);

    let mut out = Vec::new();
    for segment in segments {
        if remaining == 0 {
            break;
        }
        let take = segment.len.min(remaining);
        out.push(BufferExtent {
            addr: segment.addr,
            len: take,
        });
        remaining -= take;
    }
    out
}

/// Apply the return-value bound to a guest `iovec` array.
///
/// This is separate from the memory access so the short-transfer rule can be
/// tested directly for every syscall family that shares it.
#[cfg(test)]
fn iovec_extents_from_slice(iovecs: &[libc::iovec], moved: i64) -> Vec<BufferExtent> {
    bound_segments(&declared_segments(iovecs), moved)
}

/// Return the guest `iovec` array described by a vectored I/O syscall.
///
/// Keep the family in one match so adding a syscall variant cannot update the
/// log direction without also making its complete, return-value-bounded iovec
/// prefix available to [`extents`].
fn iovec_extent_arguments(call: &Syscall) -> Option<(usize, usize)> {
    match call {
        Syscall::Readv(call) => Some((call.iov().map_or(0, |p| p.as_raw()), call.len())),
        Syscall::Preadv(call) => Some((call.iov().map_or(0, |p| p.as_raw()), call.iov_len())),
        Syscall::Preadv2(call) => Some((
            call.iov().map_or(0, |p| p.as_raw()),
            usize::try_from(call.iov_len()).unwrap_or(usize::MAX),
        )),
        Syscall::Writev(call) => Some((call.iov().map_or(0, |p| p.as_raw()), call.len())),
        Syscall::Pwritev(call) => Some((call.iov().map_or(0, |p| p.as_raw()), call.iov_len())),
        Syscall::Pwritev2(call) => Some((
            call.iov().map_or(0, |p| p.as_raw()),
            usize::try_from(call.iov_len()).unwrap_or(usize::MAX),
        )),
        _ => None,
    }
}

/// The `iovec` arrays a vectored syscall named, as they stood when it was
/// issued: one entry per message, in message order, each holding that
/// message's declared segments.
///
/// ⚠️ READ AT ENTRY, NOT AFTER THE CALL. Linux copies an `iovec` array in once,
/// when the syscall starts (`import_iovec`, reached through
/// `copy_msghdr_from_user` for the header that points at it), and never looks
/// at the guest's copy again. The syscall's own output is allowed to land on
/// top of that copy: `recvmsg` may place its control buffer over the `msghdr`
/// itself, and `readv` may read into the memory holding its own `iovec` array.
/// Re-reading after the call then walks timestamp or payload bytes as if they
/// were pointers. Measured on `tests/c/socket_timestamp_edge_cases.c` under
/// the ptrace backend: the aliased `recvmsg`'s `msg_iov` had become
/// `SCM_TIMESTAMPNS`'s `tv_sec`, the hashing read failed with EFAULT, and that
/// EFAULT was returned to the guest in place of the syscall's successful
/// result -- for a datagram the kernel had already delivered.
///
/// ⚠️ THE BATCH CALLS HAVE A GAP THAT NEITHER READ CLOSES. The kernel reads
/// each `mmsghdr` only when it reaches that message (`do_recvmmsg` and
/// `__sys_sendmmsg` call the single-message path once per entry), so header
/// `i` as the kernel used it is the state AFTER messages `0..i` wrote and
/// BEFORE message `i` did. The entry snapshot is right unless an earlier
/// message wrote over header `i`; a post-call read is right unless message `i`
/// wrote over its own header. Each covers the shape the other misses, and a
/// batch doing both is covered by neither. The entry snapshot is kept because
/// its shape is the one real programs produce: a control buffer placed over
/// the message's own header, as above.
///
/// ⚠️ THE SNAPSHOT CAN ALSO GO STALE BEFORE THE KERNEL READS IT. The capture
/// runs before Detcore dispatches the call, and dispatch may deschedule the
/// thread first: a socket call can go through `BlockingExternalIO`, and an
/// internal descriptor goes through the `InternalIOPolling` retry loop (see
/// `execute_nonblockable_fd_syscall`). Another guest thread may rewrite the
/// header or array in that window. The schedule is deterministic, so this
/// never makes the hashed extents differ between runs, but the extents need
/// not be the ones the kernel used.
///
/// A header or array that could not be read is kept as its errno rather than
/// dropped. It is consulted only if the kernel then reports that message
/// complete. For a later batch message the kernel may have read a header that
/// an earlier message's output made readable, so [`message_segments`] falls
/// back to that header as it stands after the call. More generally, whenever
/// the extents from the entry snapshot cannot be read after the call, whether
/// the capture failed or it named memory that is no longer readable,
/// [`moved_extent_digests`] retries the whole call from the arrays as the
/// call left them. A successful call becomes a guest errno only when both
/// reads fail, which is the pre-existing behaviour for a genuinely unreadable
/// buffer. A batch that needs the entry snapshot for one message and the
/// post-call read for another can still fail both reads, and a retry that
/// succeeds carries the post-call read's own gap described above.
///
/// The default value is "not captured", and is distinct from a capture that
/// found no `iovec` arrays: see [`EntrySegments::Missing`].
#[derive(Default)]
pub(crate) struct EntryIovecs(Option<Vec<Result<Vec<BufferExtent>, Errno>>>);

/// What the entry capture recorded for one message.
#[derive(Debug, PartialEq, Eq)]
enum EntrySegments<'a> {
    /// The message's declared segments, as issued.
    Declared(&'a [BufferExtent]),
    /// The header or `iovec` array could not be read at entry. A message past
    /// the end of a capture that stopped at an unreadable header repeats that
    /// header's errno.
    Unreadable(Errno),
    /// Nothing was captured for this message. That is a Detcore bookkeeping
    /// fault -- the capture guard and the hashing guard disagreed, or an arm
    /// of [`extents`] consults an `iovec` array that [`entry_iovecs`] does not
    /// capture -- and never something the guest did, so it must not become a
    /// guest errno.
    Missing,
}

impl EntryIovecs {
    /// What the capture recorded for message `index`.
    fn message(&self, index: usize) -> EntrySegments<'_> {
        let Some(messages) = &self.0 else {
            return EntrySegments::Missing;
        };
        match messages.get(index).or_else(|| messages.last()) {
            Some(Ok(segments)) if index < messages.len() => EntrySegments::Declared(segments),
            Some(Err(errno)) => EntrySegments::Unreadable(*errno),
            _ => EntrySegments::Missing,
        }
    }
}

/// The declared segments to bound for message `index` of `call`.
///
/// `post_call_header` is message `index`'s header as it stands after the call,
/// for the batch calls, which read it anyway for `msg_len`. It is used only
/// where the entry snapshot cannot answer; see [`EntryIovecs`] for when each
/// read is right.
fn message_segments<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    entry: &EntryIovecs,
    index: usize,
    post_call_header: Option<&libc::msghdr>,
) -> Result<Vec<BufferExtent>, Errno> {
    let post_call =
        |header: &libc::msghdr| read_segments(memory, header.msg_iov as usize, header.msg_iovlen);
    match entry.message(index) {
        EntrySegments::Declared(segments) => Ok(segments.to_vec()),
        EntrySegments::Unreadable(errno) => match post_call_header {
            Some(header) if index > 0 => post_call(header),
            _ => Err(errno),
        },
        EntrySegments::Missing => {
            // Fall back to reading after the call, which is what this code did
            // before entry capture existed: correct for every call whose output
            // does not overwrite its own header, and never a guest errno
            // invented by Detcore's bookkeeping.
            if let Some(header) = post_call_header {
                return post_call(header);
            }
            match entry_iovecs(memory, call).message(index) {
                EntrySegments::Declared(segments) => Ok(segments.to_vec()),
                EntrySegments::Unreadable(errno) => Err(errno),
                EntrySegments::Missing => {
                    debug_assert!(
                        false,
                        "io-buffer hashing consults an iovec array for {} that entry_iovecs \
                         does not capture",
                        call.name()
                    );
                    Ok(Vec::new())
                }
            }
        }
    }
}

/// Read a guest `iovec` array's declared segments.
fn read_segments<M: MemoryAccess>(
    memory: &M,
    iov_addr: usize,
    iov_count: usize,
) -> Result<Vec<BufferExtent>, Errno> {
    if iov_addr == 0 || iov_count == 0 {
        return Ok(Vec::new());
    }
    let iov_count = iov_count.min(libc::UIO_MAXIOV as usize);
    let iov_address: AddrMut<'_, libc::iovec> = AddrMut::from_raw(iov_addr).ok_or(Errno::EFAULT)?;
    // SAFETY: `iovec` is a plain C record; an all-zero value is a valid staging
    // value that `read_values` immediately overwrites.
    let mut iovecs: Vec<libc::iovec> = (0..iov_count)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();
    memory.read_values(iov_address.into(), &mut iovecs)?;
    Ok(declared_segments(&iovecs))
}

/// Read a `msghdr` out of the guest and the segments of the iovecs it points at.
fn msghdr_segments<M: MemoryAccess>(
    memory: &M,
    msg_addr: usize,
) -> Result<Vec<BufferExtent>, Errno> {
    if msg_addr == 0 {
        return Ok(Vec::new());
    }
    let address: AddrMut<'_, libc::msghdr> = AddrMut::from_raw(msg_addr).ok_or(Errno::EFAULT)?;
    let message: libc::msghdr = memory.read_value(address)?;
    read_segments(memory, message.msg_iov as usize, message.msg_iovlen)
}

/// Read each `mmsghdr` in a batch and the segments of the iovecs it points at,
/// stopping at the first header that cannot be read.
///
/// This runs before every batch call whether or not it then delivers
/// anything, so the header array is read in one access; only an array that
/// runs into unreadable memory is walked header by header, to find where the
/// readable prefix ends.
fn mmsghdr_segments<M: MemoryAccess>(memory: &M, mmsg_addr: usize, vlen: u32) -> EntryIovecs {
    let count = (vlen as usize).min(libc::UIO_MAXIOV as usize);
    let mut out = Vec::with_capacity(count);
    if mmsg_addr == 0 || count == 0 {
        return EntryIovecs(Some(out));
    }
    // SAFETY: `mmsghdr` is a plain C record; an all-zero value is a valid
    // staging value that `read_values` immediately overwrites.
    let mut headers: Vec<libc::mmsghdr> =
        (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    let whole = AddrMut::<'_, libc::mmsghdr>::from_raw(mmsg_addr)
        .ok_or(Errno::EFAULT)
        .and_then(|address| memory.read_values(address.into(), &mut headers));
    if whole.is_ok() {
        for header in &headers {
            out.push(read_segments(
                memory,
                header.msg_hdr.msg_iov as usize,
                header.msg_hdr.msg_iovlen,
            ));
        }
        return EntryIovecs(Some(out));
    }
    for index in 0..count {
        let header = mmsg_addr
            .checked_add(index * std::mem::size_of::<libc::mmsghdr>())
            .and_then(AddrMut::<'_, libc::mmsghdr>::from_raw)
            .ok_or(Errno::EFAULT)
            .and_then(|address| memory.read_value(address));
        match header {
            Ok(header) => out.push(read_segments(
                memory,
                header.msg_hdr.msg_iov as usize,
                header.msg_hdr.msg_iovlen,
            )),
            Err(errno) => {
                out.push(Err(errno));
                break;
            }
        }
    }
    EntryIovecs(Some(out))
}

/// Capture, before the syscall runs, every `iovec` array [`extents`] will
/// need afterwards. A capture with no messages for a syscall that names none.
fn entry_iovecs<M: MemoryAccess>(memory: &M, call: &Syscall) -> EntryIovecs {
    if let Some((iov_addr, iov_count)) = iovec_extent_arguments(call) {
        return EntryIovecs(Some(vec![read_segments(memory, iov_addr, iov_count)]));
    }
    match call {
        Syscall::Recvmsg(c) => EntryIovecs(Some(vec![msghdr_segments(
            memory,
            c.msg().map_or(0, |p| p.as_raw()),
        )])),
        Syscall::Sendmsg(c) => EntryIovecs(Some(vec![msghdr_segments(
            memory,
            c.msg().map_or(0, |p| p.as_raw()),
        )])),
        Syscall::Recvmmsg(c) => {
            mmsghdr_segments(memory, c.mmsg().map_or(0, |p| p.as_raw()), c.vlen())
        }
        Syscall::Sendmmsg(c) => {
            mmsghdr_segments(memory, c.msgvec().map_or(0, |p| p.as_raw()), c.vlen())
        }
        _ => EntryIovecs(Some(Vec::new())),
    }
}

/// Number of completed messages whose per-message lengths are meaningful.
fn completed_mmsghdr_count(vlen: u32, completed: i64) -> usize {
    usize::try_from(completed)
        .unwrap_or(0)
        .min(vlen as usize)
        .min(libc::UIO_MAXIOV as usize)
}

/// Walk the `mmsghdr` array a batch send or receive completed.
///
/// `moved` here is a COUNT OF MESSAGES, not a byte count -- which is why
/// these calls cannot share the `clamp`/single-message path that every other
/// send or receive uses. Each completed message carries its own byte count in
/// `msg_len`, so each is walked separately and bounded by that; treating the
/// batch as one buffer would let one message's length run into the next
/// message's memory.
///
/// `msg_len` is the kernel's OUTPUT and so is read after the call; the segments
/// it bounds come from `entry`, read before it (see [`EntryIovecs`]).
fn mmsghdr_extents<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    mmsg_addr: usize,
    vlen: u32,
    delivered: i64,
    entry: &EntryIovecs,
) -> Result<Vec<BufferExtent>, Error> {
    let count = completed_mmsghdr_count(vlen, delivered);
    if mmsg_addr == 0 || count == 0 {
        return Ok(Vec::new());
    }
    let address: AddrMut<'_, libc::mmsghdr> = AddrMut::from_raw(mmsg_addr).ok_or(Errno::EFAULT)?;
    // SAFETY: `mmsghdr` is a plain C record; an all-zero value is a valid
    // staging value that `read_values` immediately overwrites.
    let mut headers: Vec<libc::mmsghdr> =
        (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    memory.read_values(address.into(), &mut headers)?;

    let mut out = Vec::new();
    for (index, header) in headers.iter().enumerate() {
        out.extend(bound_segments(
            &message_segments(memory, call, entry, index, Some(&header.msg_hdr))?,
            i64::from(header.msg_len),
        ));
    }
    Ok(out)
}

/// Whether this syscall's return value bounds what it wrote.
///
/// True for the ordinary case, where the return IS the byte count, so a return
/// of 0 or less means nothing was written. FALSE for `poll`/`ppoll`, whose
/// return is a COUNT OF READY DESCRIPTORS: the kernel rewrites `revents` on
/// every entry even when it returns 0. Measured -- `poll(timeout=0)` on two
/// never-ready pipe fds returns 0 and still moves `revents` from a poisoned
/// 0x7FFF to 0 on both entries.
///
/// This exists as a predicate consulted BY the single `ret <= 0` guard, rather
/// than as poll arms placed above it, so the guard cannot be reordered back
/// into swallowing the zero-ready case. The original gap was exactly that: the
/// guard sat above the poll arms, and the unit test called `whole` directly, so
/// it could not see the short-circuit above the code it exercised.
/// Every syscall `extents` below returns a non-empty result for.
///
/// KEEP THIS IN STEP WITH THE `extents` MATCH -- it is the same set written
/// twice, once as `Syscall::` arms (which need constructed calls and a `Guest`
/// to exercise) and once as bare `Sysno`s (which a subscription can be asked
/// about). The second spelling exists so the relationship between this check
/// and what Detcore actually intercepts can be ASSERTED rather than assumed;
/// see `passthru_opt_leaves_io_buffer_hashing_blind_only_for_getcwd` in
/// `lib.rs`. Adding an arm to `extents` without adding it here does not break
/// that test, so add both.
#[cfg(test)]
pub(crate) const HASHED_SYSCALLS: &[reverie::syscalls::Sysno] = &[
    // Bytes the kernel produced.
    reverie::syscalls::Sysno::read,
    reverie::syscalls::Sysno::pread64,
    reverie::syscalls::Sysno::recvfrom,
    reverie::syscalls::Sysno::getrandom,
    reverie::syscalls::Sysno::getcwd,
    reverie::syscalls::Sysno::getdents64,
    reverie::syscalls::Sysno::readlink,
    reverie::syscalls::Sysno::readlinkat,
    reverie::syscalls::Sysno::recvmsg,
    reverie::syscalls::Sysno::recvmmsg,
    reverie::syscalls::Sysno::readv,
    reverie::syscalls::Sysno::preadv,
    reverie::syscalls::Sysno::preadv2,
    // Bytes the guest produced.
    reverie::syscalls::Sysno::write,
    reverie::syscalls::Sysno::pwrite64,
    reverie::syscalls::Sysno::sendto,
    reverie::syscalls::Sysno::sendmsg,
    reverie::syscalls::Sysno::sendmmsg,
    reverie::syscalls::Sysno::writev,
    reverie::syscalls::Sysno::pwritev,
    reverie::syscalls::Sysno::pwritev2,
    // Rewritten in place across the whole array.
    reverie::syscalls::Sysno::poll,
    reverie::syscalls::Sysno::ppoll,
];

fn ret_gates_output(call: &Syscall) -> bool {
    !matches!(call, Syscall::Poll(_) | Syscall::Ppoll(_))
}

/// The extents a completed syscall moved, or `None` when this syscall has no
/// output buffer worth hashing.
///
/// Only syscalls whose buffer CONTENT the INFO record does not already show are
/// listed. `clock_gettime` and `newfstatat`, for instance, are absent because
/// Reverie's typed display already dereferences and prints their output.
///
/// `entry` must be what [`entry_iovecs`] captured for this same call before it
/// ran; every `iovec` segment comes from there and not from a re-read.
fn extents<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    ret: i64,
    entry: &EntryIovecs,
) -> Result<Vec<BufferExtent>, Error> {
    // Nothing was written on a failed or empty call -- for every syscall whose
    // return value is a byte count. `ret_gates_output` is what keeps the poll
    // family out of this, and it is a predicate rather than an arm placed above
    // so the exclusion cannot be undone by moving code.
    if ret <= 0 && ret_gates_output(call) {
        return Ok(Vec::new());
    }
    if iovec_extent_arguments(call).is_some() {
        return Ok(bound_segments(
            &message_segments(memory, call, entry, 0, None)?,
            ret,
        ));
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
        Syscall::Recvmsg(_) => {
            bound_segments(&message_segments(memory, call, entry, 0, None)?, ret)
        }
        // `ret` is a MESSAGE count here, not a byte count; see
        // `mmsghdr_extents`. recvmmsg is one of the four receive syscalls
        // that could reach a NETLINK_SOCK_DIAG dump without passing the
        // sock_diag sanitizer, so leaving it unhashed left this check blind
        // to exactly the bypass it would otherwise have reported.
        Syscall::Recvmmsg(c) => mmsghdr_extents(
            memory,
            call,
            c.mmsg().map_or(0, |p| p.as_raw()),
            c.vlen(),
            ret,
            entry,
        )?,
        // Bytes the guest produced. These never reach stdout/stderr for a QEMU
        // boot -- measured, all 234,872 writes went to fds 7/12/14/11/13/4/8/19/23
        // and none to fd 1 or 2 -- so `--verify`'s stdout/stderr comparison does
        // not cover them either.
        Syscall::Write(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Pwrite64(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Sendto(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.size(), ret),
        Syscall::Sendmsg(_) => {
            bound_segments(&message_segments(memory, call, entry, 0, None)?, ret)
        }
        // Like recvmmsg, `ret` counts completed messages and each completed
        // header's `msg_len` bounds the bytes consumed from that message.
        Syscall::Sendmmsg(c) => mmsghdr_extents(
            memory,
            call,
            c.msgvec().map_or(0, |p| p.as_raw()),
            c.vlen(),
            ret,
            entry,
        )?,
        // Rewritten in place across the WHOLE array: `poll` sets `revents` on
        // every entry, not just on the `ret` that were ready, so the extent is
        // the array and not a prefix of it -- and it is reached even when
        // `ret == 0`, via `ret_gates_output`.
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
        | Syscall::Sendmmsg(_)
        | Syscall::Writev(_)
        | Syscall::Pwritev(_)
        | Syscall::Pwritev2(_) => Direction::Out,
        _ => Direction::In,
    }
}

/// Emit one deterministic record per buffer a completed syscall moved.
///
/// Digests that LOCATE a divergence instead of only detecting one.
///
/// The whole-extent digest says two buffers differ and nothing else -- not
/// which bytes, not which field. Comparing two logs of per-chunk digests names
/// the FIRST DIFFERING CHUNK, which is the offset and a bounded window around
/// it. That is what a content divergence needs to be classifiable, and it costs
/// no retained bytes: the guest's data never leaves the guest.
///
/// ⚠️ ONE GUEST READ, NOT ONE PER CHUNK. Reading guest memory is the expensive
/// half of this check -- `compute_hash_range` does a `read_values` per call --
/// so the extent is read ONCE and every digest is taken from that local copy.
/// The guest-memory cost is therefore identical to the single-digest version
/// this replaces; only local hashing and log width grow.
///
/// The chunk count is CAPPED so a large buffer cannot produce an unbounded log
/// line: at most [`CHUNK_CAP`] digests always span the whole extent, so the
/// window is `max(CHUNK_MIN, ceil(len / CHUNK_CAP))`. A buffer that fits in one
/// chunk emits no chunk list, because a single chunk repeats what the
/// whole-extent digest already said.
const CHUNK_CAP: usize = 8;
const CHUNK_MIN: usize = 256;

/// One moved extent and the digests of the bytes it held after the call.
struct ExtentDigests {
    extent: BufferExtent,
    whole: Digest,
    chunk: usize,
    chunks: Vec<String>,
}

/// Read and digest each extent, one extent in memory at a time.
fn digest_extents<M: MemoryAccess>(
    memory: &M,
    extents: Vec<BufferExtent>,
) -> Result<Vec<ExtentDigests>, Error> {
    extents
        .into_iter()
        .map(|extent| {
            let mut buf = vec![0u8; extent.len as usize];
            if !buf.is_empty() {
                let start = Addr::<u8>::from_raw(extent.addr as usize).ok_or(Errno::EFAULT)?;
                memory.read_values(start, buf.as_mut_slice())?;
            }
            let whole = Digest::new(buf.as_slice());
            let (chunk, chunks) = chunk_digests(buf.as_slice());
            Ok(ExtentDigests {
                extent,
                whole,
                chunk,
                chunks,
            })
        })
        .collect()
}

/// Whether [`extents`] takes any of this syscall's segments from an `iovec`
/// array, and so from an [`EntryIovecs`] capture.
fn names_iovec_arrays(call: &Syscall) -> bool {
    iovec_extent_arguments(call).is_some()
        || matches!(
            call,
            Syscall::Recvmsg(_) | Syscall::Sendmsg(_) | Syscall::Recvmmsg(_) | Syscall::Sendmmsg(_)
        )
}

/// The digests of every extent a completed syscall moved.
///
/// The segments come from the entry snapshot first. If those cannot be read
/// after the call, they are retried once from the `iovec` arrays as the call
/// left them, the read this module made before entry capture existed. The
/// entry snapshot can go stale before the kernel reads the arrays in two ways;
/// see [`EntryIovecs`]. Only if the retry fails as well is the entry
/// snapshot's error returned, which becomes the guest's result.
fn moved_extent_digests<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    ret: i64,
    entry: &EntryIovecs,
) -> Result<Vec<ExtentDigests>, Error> {
    let from_entry = extents(memory, call, ret, entry).and_then(|e| digest_extents(memory, e));
    match from_entry {
        Err(entry_error) if names_iovec_arrays(call) => {
            let after_call = entry_iovecs(memory, call);
            extents(memory, call, ret, &after_call)
                .and_then(|e| digest_extents(memory, e))
                .map_err(|_| entry_error)
        }
        moved => moved,
    }
}

/// The locating half, split out from the guest read so it can be bracketed.
///
/// Returns the chunk width and one short digest per chunk, or an empty vector
/// when the extent fits in a single chunk and the list would only repeat the
/// whole-extent digest.
fn chunk_digests(buf: &[u8]) -> (usize, Vec<String>) {
    let size = buf.len();
    let chunk = std::cmp::max(CHUNK_MIN, size.div_ceil(CHUNK_CAP));
    if size <= chunk {
        return (chunk, Vec::new());
    }
    let chunks = buf
        .chunks(chunk)
        // Short prefix per chunk: this locates a window, it does not
        // authenticate one, and eight full SHA-256 digests per buffer would
        // dominate the line without making the location any sharper. Truncated
        // explicitly rather than with a `{:.8}` precision, which `Digest`'s
        // Display does not honour -- it delegates to LowerHex and ignores the
        // formatter's precision, so the specifier silently printed the full
        // digest.
        .map(|c| {
            Digest::new(c)
                .to_string()
                .chars()
                .take(8)
                .collect::<String>()
        })
        .collect();
    (chunk, chunks)
}

/// Capture the `iovec` arrays a syscall names BEFORE Detcore runs it, for
/// [`detlog_io_buffers`] to use afterwards; see [`EntryIovecs`] for why the
/// arrays cannot be re-read once the call has completed.
///
/// Guarded exactly as [`detlog_io_buffers`] is, so the disabled path reads no
/// guest memory here either.
pub(crate) fn capture_entry_iovecs<G, T>(guest: &G, call: &Syscall) -> EntryIovecs
where
    G: Guest<T>,
    T: Tool,
{
    if !crate::detlog_observed!() {
        return EntryIovecs::default();
    }
    entry_iovecs(&guest.memory(), call)
}

/// ⚠️ THE GUARD IS FIRST AND THAT IS THE POINT. Everything below it touches
/// guest memory: for `recvmsg` the extents cannot even be computed without
/// reading a `msghdr` and an `iovec` array out of the guest -- at entry, in
/// [`capture_entry_iovecs`], which carries the same guard. That is
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
    entry: &EntryIovecs,
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
    // NAME THE DESCRIPTOR THE BYTES CAME THROUGH.
    //
    // A content divergence is classified by WHAT the differing bytes are, and
    // the descriptor is the cheapest available answer to that. Without it,
    // identifying one real case cost reading backward five syscalls through a
    // 5.8 MB log to find `socket(16, 524291, 0) = Ok(11)` and recognising
    // AF_NETLINK by hand; with it, "this is a netlink dump" is on the line that
    // diverged.
    //
    // Safe to add to a byte-for-byte compared surface because the value is
    // ALREADY compared and already stable: the `[syscall]` lines carry these
    // same fds, and across two verify pairs measured 2026-08-25 every
    // fd-bearing syscall line matched with zero differing lines. `-` covers the
    // calls with no single fd argument (getrandom, getcwd, readlink), which is
    // a fact about the call rather than a missing lookup.
    let fd = match crate::syscalls::helpers::get_fd(*call) {
        Some(fd) => fd.to_string(),
        None => "-".to_string(),
    };
    let moved = {
        let memory = guest.memory();
        moved_extent_digests(&memory, call, ret, entry)?
    };
    for ExtentDigests {
        extent,
        whole,
        chunk,
        chunks,
    } in moved
    {
        let located = if chunks.is_empty() {
            String::new()
        } else {
            format!(" chunks={}:{}", chunk, chunks.join(","))
        };
        crate::detlog!(
            "[iobuf][dtid {}] {} {} fd={} {:#x}+{}->{}{}",
            dettid,
            name,
            dir,
            fd,
            extent.addr,
            extent.len,
            whole,
            located
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use reverie::syscalls;
    use reverie::syscalls::LocalMemory;

    /// A divergence must be LOCATED, not merely detected -- that is the whole
    /// reason the chunk list exists, so it is asserted rather than assumed.
    #[test]
    fn chunk_digests_name_the_first_differing_window() {
        let len = 1468; // the real netlink dump size
        let left = vec![0xABu8; len];
        let mut right = left.clone();
        let offset = 900;
        right[offset] ^= 0xFF; // one flipped byte, nothing else
        let (chunk, l) = chunk_digests(&left);
        let (_, r) = chunk_digests(&right);
        assert_eq!(chunk, 256);
        assert_eq!(l.len(), r.len());
        let first_diff = l.iter().zip(&r).position(|(a, b)| a != b);
        assert_eq!(
            first_diff,
            Some(offset / chunk),
            "must name the window holding the flip"
        );
        // and ONLY that window: a single-byte change must not smear.
        assert_eq!(l.iter().zip(&r).filter(|(a, b)| a != b).count(), 1);
    }

    #[test]
    fn identical_buffers_produce_identical_chunk_lists() {
        let buf = vec![7u8; 1468];
        assert_eq!(chunk_digests(&buf).1, chunk_digests(&buf.clone()).1);
    }

    /// A buffer that fits in one chunk gets no list: it would only repeat the
    /// whole-extent digest, and this check is always on.
    #[test]
    fn a_single_chunk_extent_emits_no_list() {
        assert!(chunk_digests(&vec![0u8; CHUNK_MIN]).1.is_empty());
        assert!(chunk_digests(&[]).1.is_empty());
    }

    /// The log line must stay bounded however large the buffer is.
    #[test]
    fn the_chunk_count_is_capped_and_still_spans_the_extent() {
        for len in [2048usize, 65536, 1 << 20] {
            let (chunk, chunks) = chunk_digests(&vec![0u8; len]);
            assert!(
                chunks.len() <= CHUNK_CAP,
                "len {len} produced {} chunks",
                chunks.len()
            );
            assert!(
                chunk * chunks.len() >= len,
                "chunks must span the whole extent"
            );
        }
    }

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

    /// The gap this file had: `poll(...) = 0` produced no record at all,
    /// because the `ret <= 0` guard sat above the poll arms. The pre-existing
    /// test called `whole()` directly, so it passed throughout -- it could not
    /// see a short-circuit above the code it called.
    ///
    /// This drives `ret_gates_output`, which is the predicate the single guard
    /// consults, so the property is tested where it is decided rather than one
    /// level away from it.
    #[test]
    fn the_poll_family_is_exempt_from_the_return_value_guard() {
        let nfds = 3;
        for call in [
            Syscall::Poll(
                syscalls::Poll::new()
                    .with_fds(AddrMut::from_raw(0x2000))
                    .with_nfds(nfds),
            ),
            Syscall::Ppoll(
                syscalls::Ppoll::new()
                    .with_fds(AddrMut::from_raw(0x2000))
                    .with_nfds(nfds),
            ),
        ] {
            assert!(
                !ret_gates_output(&call),
                "{} returns a count of READY DESCRIPTORS, not a byte count, so a \
                 zero return must not suppress its extent",
                call.name()
            );
        }

        // Everything else must stay gated, or a failed or empty call would hash
        // bytes the kernel never wrote.
        for call in [
            Syscall::Read(syscalls::Read::new()),
            Syscall::Recvmsg(syscalls::Recvmsg::new()),
            Syscall::Recvmmsg(syscalls::Recvmmsg::new()),
            Syscall::Sendmmsg(syscalls::Sendmmsg::new()),
        ] {
            assert!(
                ret_gates_output(&call),
                "{} returns a byte or message count, so a zero return means \
                 nothing was written",
                call.name()
            );
        }
    }

    #[test]
    fn direction_separates_kernel_produced_from_guest_produced() {
        assert_eq!(Direction::In.as_str(), "in");
        assert_eq!(Direction::Out.as_str(), "out");

        assert_eq!(
            direction(&Syscall::Preadv2(syscalls::Preadv2::new())),
            Direction::In
        );
        assert_eq!(
            direction(&Syscall::Pwritev2(syscalls::Pwritev2::new())),
            Direction::Out
        );
        assert_eq!(
            direction(&Syscall::Sendmmsg(syscalls::Sendmmsg::new())),
            Direction::Out
        );
    }

    #[test]
    fn vectored_v2_calls_are_hashed_over_their_complete_iovec_prefix() {
        for sysno in [syscalls::Sysno::preadv2, syscalls::Sysno::pwritev2] {
            assert!(
                HASHED_SYSCALLS.contains(&sysno),
                "{sysno:?} must remain in strict io-buffer observation"
            );
        }

        let first = [0_u8; 4];
        let second = [0_u8; 8];
        let third = [0_u8; 16];
        let iovecs = [
            libc::iovec {
                iov_base: first.as_ptr() as *mut libc::c_void,
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_ptr() as *mut libc::c_void,
                iov_len: second.len(),
            },
            libc::iovec {
                iov_base: third.as_ptr() as *mut libc::c_void,
                iov_len: third.len(),
            },
        ];
        let iov = Addr::from_ptr(iovecs.as_ptr());
        let calls = [
            Syscall::Preadv2(syscalls::Preadv2::new().with_iov(iov).with_iov_len(3)),
            Syscall::Pwritev2(syscalls::Pwritev2::new().with_iov(iov).with_iov_len(3)),
        ];
        let expected = vec![
            BufferExtent {
                addr: first.as_ptr() as u64,
                len: 4,
            },
            BufferExtent {
                addr: second.as_ptr() as u64,
                len: 2,
            },
        ];
        let memory = LocalMemory::new();

        for call in calls {
            assert_eq!(
                iovec_extent_arguments(&call),
                Some((iovecs.as_ptr() as usize, 3))
            );
            let entry = entry_iovecs(&memory, &call);
            assert_eq!(extents(&memory, &call, 6, &entry).unwrap(), expected);
            assert!(extents(&memory, &call, 0, &entry).unwrap().is_empty());
            assert!(extents(&memory, &call, -1, &entry).unwrap().is_empty());
        }
    }

    #[test]
    fn vectored_io_extents_stop_at_the_completed_byte_count() {
        let iovecs = [
            libc::iovec {
                iov_base: 0x1000usize as *mut libc::c_void,
                iov_len: 4,
            },
            libc::iovec {
                iov_base: 0x2000usize as *mut libc::c_void,
                iov_len: 8,
            },
            libc::iovec {
                iov_base: 0x3000usize as *mut libc::c_void,
                iov_len: 16,
            },
        ];

        assert_eq!(
            iovec_extents_from_slice(&iovecs, 6),
            vec![
                BufferExtent {
                    addr: 0x1000,
                    len: 4,
                },
                BufferExtent {
                    addr: 0x2000,
                    len: 2,
                },
            ]
        );
        assert!(iovec_extents_from_slice(&iovecs, 0).is_empty());
        assert!(iovec_extents_from_slice(&iovecs, -1).is_empty());
    }

    #[test]
    fn sendmmsg_hashes_only_completed_messages_and_each_reported_byte_prefix() {
        assert!(HASHED_SYSCALLS.contains(&syscalls::Sysno::sendmmsg));

        let first = [0_u8; 4];
        let second = [0_u8; 8];
        let third = [0_u8; 16];
        let first_iovecs = [
            libc::iovec {
                iov_base: first.as_ptr() as *mut libc::c_void,
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_ptr() as *mut libc::c_void,
                iov_len: second.len(),
            },
            libc::iovec {
                iov_base: third.as_ptr() as *mut libc::c_void,
                iov_len: third.len(),
            },
        ];
        let fourth = [0_u8; 5];
        let fifth = [0_u8; 7];
        let second_iovecs = [
            libc::iovec {
                iov_base: fourth.as_ptr() as *mut libc::c_void,
                iov_len: fourth.len(),
            },
            libc::iovec {
                iov_base: fifth.as_ptr() as *mut libc::c_void,
                iov_len: fifth.len(),
            },
        ];

        // SAFETY: `mmsghdr` is a plain C record and every field consumed by
        // `extents` is initialized below before LocalMemory reads it.
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        headers[0].msg_hdr.msg_iov = first_iovecs.as_ptr() as *mut libc::iovec;
        headers[0].msg_hdr.msg_iovlen = first_iovecs.len();
        headers[0].msg_len = 6;
        headers[1].msg_hdr.msg_iov = second_iovecs.as_ptr() as *mut libc::iovec;
        headers[1].msg_hdr.msg_iovlen = second_iovecs.len();
        headers[1].msg_len = 9;

        let call = Syscall::Sendmmsg(
            syscalls::Sendmmsg::new()
                .with_msgvec(Addr::from_ptr(headers.as_ptr().cast::<libc::msghdr>()))
                .with_vlen(2),
        );
        let memory = LocalMemory::new();
        let first_message = vec![
            BufferExtent {
                addr: first.as_ptr() as u64,
                len: 4,
            },
            BufferExtent {
                addr: second.as_ptr() as u64,
                len: 2,
            },
        ];
        let entry = entry_iovecs(&memory, &call);
        assert_eq!(extents(&memory, &call, 1, &entry).unwrap(), first_message);

        let both_messages = vec![
            BufferExtent {
                addr: first.as_ptr() as u64,
                len: 4,
            },
            BufferExtent {
                addr: second.as_ptr() as u64,
                len: 2,
            },
            BufferExtent {
                addr: fourth.as_ptr() as u64,
                len: 5,
            },
            BufferExtent {
                addr: fifth.as_ptr() as u64,
                len: 4,
            },
        ];
        assert_eq!(extents(&memory, &call, 2, &entry).unwrap(), both_messages);
        assert!(extents(&memory, &call, 0, &entry).unwrap().is_empty());
        assert!(extents(&memory, &call, -1, &entry).unwrap().is_empty());
        assert_eq!(completed_mmsghdr_count(1, 2), 1);
    }

    /// A connected `AF_UNIX` datagram pair whose receiving end, `[1]`, stamps
    /// every message with `SCM_TIMESTAMPNS`, holding `payloads` already queued.
    fn timestamped_datagrams(payloads: &[u8]) -> [libc::c_int; 2] {
        let mut sockets = [0; 2];
        // SAFETY: `sockets` has room for the two descriptors socketpair writes.
        let paired =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sockets.as_mut_ptr()) };
        assert_eq!(paired, 0, "socketpair: {}", std::io::Error::last_os_error());
        let enabled: libc::c_int = 1;
        // SAFETY: the option value is a live `c_int` of the length passed.
        let stamped = unsafe {
            libc::setsockopt(
                sockets[1],
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMPNS,
                (&raw const enabled).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(
            stamped,
            0,
            "SO_TIMESTAMPNS: {}",
            std::io::Error::last_os_error()
        );
        for payload in payloads {
            // SAFETY: one readable byte at `payload`.
            let sent = unsafe { libc::send(sockets[0], (payload as *const u8).cast(), 1, 0) };
            assert_eq!(sent, 1, "send: {}", std::io::Error::last_os_error());
        }
        sockets
    }

    fn close_all(fds: [libc::c_int; 2]) {
        for fd in fds {
            // SAFETY: each descriptor was opened by this test and is closed once.
            unsafe { libc::close(fd) };
        }
    }

    /// The shape `tests/c/socket_timestamp_edge_cases.c` exercises, driven
    /// through the REAL kernel rather than a hand-built imitation of what it
    /// writes: a `recvmsg` whose control buffer is its own `msghdr`.
    ///
    /// Linux puts the `SCM_TIMESTAMPNS` record over the header's first 32
    /// bytes, so afterwards `msg_iov` holds the timestamp's `tv_sec` and
    /// `msg_iovlen` its `tv_nsec`. Reading the header after the call walked
    /// those as a pointer and a count; under the ptrace backend the read
    /// failed with EFAULT, which Detcore returned to the guest in place of the
    /// successful receive. The extent must be the one-byte buffer the kernel
    /// actually filled.
    #[test]
    fn recvmsg_extents_come_from_the_header_as_issued_not_as_overwritten() {
        let sockets = timestamped_datagrams(b"a");
        let mut byte = 0_u8;
        let mut iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        // SAFETY: `msghdr` is a plain C record; every field the kernel reads
        // is set below.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = (&raw mut message).cast();
        message.msg_controllen = std::mem::size_of::<libc::msghdr>();
        let call = Syscall::Recvmsg(
            syscalls::Recvmsg::new()
                .with_sockfd(sockets[1])
                .with_msg(AddrMut::from_raw(&raw mut message as usize)),
        );
        let memory = LocalMemory::new();

        let entry = entry_iovecs(&memory, &call);
        // SAFETY: `message` describes live buffers, including itself as the
        // control buffer, which Linux permits.
        let received = unsafe { libc::recvmsg(sockets[1], &raw mut message, 0) };
        close_all(sockets);
        assert_eq!(received, 1, "recvmsg: {}", std::io::Error::last_os_error());
        assert_eq!(byte, b'a');
        // The premise: if the kernel ever stops overwriting the header, this
        // test no longer exercises the alias and must say so.
        assert_ne!(
            message.msg_iov, &raw mut iov,
            "the control record no longer lands on msg_iov"
        );

        assert_eq!(
            extents(&memory, &call, received as i64, &entry).unwrap(),
            vec![BufferExtent {
                addr: &raw const byte as u64,
                len: 1,
            }]
        );
    }

    /// The same defect through the vectored-read family, with no socket
    /// involved: a `readv` whose destination is its own `iovec` array. Linux
    /// copied the array in before reading, so it delivers all 16 bytes to the
    /// array's storage; a post-call re-read instead finds a segment at
    /// 0xa5a5a5a5a5a5a5a5.
    #[test]
    fn readv_extents_come_from_the_iovec_array_as_issued_not_as_overwritten() {
        let mut pipe = [0; 2];
        // SAFETY: `pipe` has room for the two descriptors pipe writes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let payload = [0xa5_u8; std::mem::size_of::<libc::iovec>()];
        // SAFETY: `payload` is readable for its full length.
        let written = unsafe { libc::write(pipe[1], payload.as_ptr().cast(), payload.len()) };
        assert_eq!(written, payload.len() as isize);

        // SAFETY: `iovec` is a plain C record; both fields are set below.
        let mut slot: libc::iovec = unsafe { std::mem::zeroed() };
        slot.iov_base = (&raw mut slot).cast();
        slot.iov_len = std::mem::size_of::<libc::iovec>();
        let call = Syscall::Readv(
            syscalls::Readv::new()
                .with_fd(pipe[0])
                .with_iov(Addr::from_ptr(&raw const slot))
                .with_len(1),
        );
        let memory = LocalMemory::new();

        let entry = entry_iovecs(&memory, &call);
        // SAFETY: the single segment covers `slot`'s own live storage.
        let read = unsafe { libc::readv(pipe[0], &raw const slot, 1) };
        close_all(pipe);
        assert_eq!(read, payload.len() as isize);
        assert_eq!(
            slot.iov_base as usize, 0xa5a5_a5a5_a5a5_a5a5,
            "the premise: the read overwrote the array it was described by"
        );

        assert_eq!(
            extents(&memory, &call, read as i64, &entry).unwrap(),
            vec![BufferExtent {
                addr: &raw const slot as u64,
                len: std::mem::size_of::<libc::iovec>() as u64,
            }]
        );
    }

    /// The batch receive, where each message's control buffer is that
    /// message's own header. `msg_len` must still come from after the call --
    /// it is the kernel's output -- while each message's segments come from
    /// before it.
    #[test]
    fn recvmmsg_extents_pair_entry_segments_with_completed_lengths() {
        let sockets = timestamped_datagrams(b"01");
        let mut bytes = [0_u8; 2];
        let mut iovecs = [
            libc::iovec {
                iov_base: (&raw mut bytes[0]).cast(),
                iov_len: 1,
            },
            libc::iovec {
                iov_base: (&raw mut bytes[1]).cast(),
                iov_len: 1,
            },
        ];
        // SAFETY: `mmsghdr` is a plain C record; every field the kernel reads
        // is set below.
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        for (header, iov) in headers.iter_mut().zip(iovecs.iter_mut()) {
            header.msg_hdr.msg_iov = iov;
            header.msg_hdr.msg_iovlen = 1;
            header.msg_hdr.msg_control = (&raw mut header.msg_hdr).cast();
            header.msg_hdr.msg_controllen = std::mem::size_of::<libc::msghdr>();
        }
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(sockets[1])
                .with_mmsg(AddrMut::from_raw(headers.as_mut_ptr() as usize))
                .with_vlen(2),
        );
        let memory = LocalMemory::new();

        let entry = entry_iovecs(&memory, &call);
        // SAFETY: both headers describe live buffers, each using itself as
        // its control buffer.
        let received = unsafe {
            libc::recvmmsg(
                sockets[1],
                headers.as_mut_ptr(),
                2,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        close_all(sockets);
        assert_eq!(received, 2, "recvmmsg: {}", std::io::Error::last_os_error());
        assert_eq!(&bytes, b"01");
        for (header, iov) in headers.iter().zip(iovecs.iter()) {
            assert_eq!(header.msg_len, 1);
            assert_ne!(
                header.msg_hdr.msg_iov.cast_const(),
                iov as *const libc::iovec,
                "the control record no longer lands on msg_iov"
            );
        }

        assert_eq!(
            extents(&memory, &call, received as i64, &entry).unwrap(),
            vec![
                BufferExtent {
                    addr: &raw const bytes[0] as u64,
                    len: 1,
                },
                BufferExtent {
                    addr: &raw const bytes[1] as u64,
                    len: 1,
                },
            ]
        );
    }

    /// A header the capture could not read is reported only when the kernel
    /// says that message completed, and it keeps its errno; a message past
    /// the capture repeats the errno that stopped it.
    #[test]
    fn an_unreadable_entry_is_reported_only_for_a_completed_message() {
        let memory = LocalMemory::new();
        // Page zero is never mapped for a user process.
        let call = Syscall::Recvmsg(syscalls::Recvmsg::new().with_msg(AddrMut::from_raw(0x10)));
        let entry = entry_iovecs(&memory, &call);
        assert!(extents(&memory, &call, 0, &entry).unwrap().is_empty());
        assert!(extents(&memory, &call, -1, &entry).unwrap().is_empty());
        assert!(matches!(
            extents(&memory, &call, 1, &entry),
            Err(Error::Errno(Errno::EFAULT))
        ));

        let stopped = EntryIovecs(Some(vec![Ok(Vec::new()), Err(Errno::EFAULT)]));
        assert_eq!(stopped.message(0), EntrySegments::Declared(&[]));
        assert_eq!(stopped.message(1), EntrySegments::Unreadable(Errno::EFAULT));
        assert_eq!(stopped.message(2), EntrySegments::Unreadable(Errno::EFAULT));
        // Past the end of a capture that did NOT stop at an error, and no
        // capture at all, are Detcore's bookkeeping, not the guest's memory.
        let complete = EntryIovecs(Some(vec![Ok(Vec::new())]));
        assert_eq!(complete.message(1), EntrySegments::Missing);
        assert_eq!(
            EntryIovecs(Some(Vec::new())).message(0),
            EntrySegments::Missing
        );
        assert_eq!(EntryIovecs::default().message(0), EntrySegments::Missing);
    }

    /// A capture that was never taken must not become a guest errno. Before
    /// entry capture existed the hashing read after the call; that is the
    /// fallback, and here, with nothing overwritten, it is also exact.
    #[test]
    fn a_missing_capture_falls_back_to_the_post_call_read_not_to_efault() {
        let mut bytes = [0_u8; 3];
        let iovecs = [libc::iovec {
            iov_base: (&raw mut bytes[0]).cast(),
            iov_len: 3,
        }];
        let call = Syscall::Readv(
            syscalls::Readv::new()
                .with_iov(Addr::from_raw(iovecs.as_ptr() as usize))
                .with_len(1),
        );
        let memory = LocalMemory::new();
        assert_eq!(
            extents(&memory, &call, 2, &EntryIovecs::default()).unwrap(),
            vec![BufferExtent {
                addr: &raw const bytes[0] as u64,
                len: 2,
            }]
        );
    }

    /// Every hashed syscall that names an `iovec` array must have message 0
    /// captured by `entry_iovecs`. An uncaptured message is `Missing`, and the
    /// batch calls answer `Missing` from the post-call header without reaching
    /// any assertion, so the capture itself is what is checked here. The set
    /// of such syscalls is written out rather than taken from
    /// `names_iovec_arrays`, so that predicate is checked too.
    #[test]
    fn every_hashed_syscall_finds_its_entry_capture() {
        use reverie::syscalls::Sysno;
        const NAMES_IOVEC_ARRAYS: &[Sysno] = &[
            Sysno::recvmsg,
            Sysno::recvmmsg,
            Sysno::readv,
            Sysno::preadv,
            Sysno::preadv2,
            Sysno::sendmsg,
            Sysno::sendmmsg,
            Sysno::writev,
            Sysno::pwritev,
            Sysno::pwritev2,
        ];
        // Zeroed memory: every header and iovec read from it is valid and
        // declares no segments, so every arm reaches its capture lookup.
        let zeroed = [0_u64; 512];
        let pointer = zeroed.as_ptr() as usize;
        let memory = LocalMemory::new();
        for &sysno in HASHED_SYSCALLS {
            // (fd, buffer/iov/msg, count/vlen, ...) fits every hashed syscall.
            let call = Syscall::from_raw(
                sysno,
                reverie::syscalls::SyscallArgs::new(3, pointer, 1, 0, 0, 0),
            );
            let entry = entry_iovecs(&memory, &call);
            assert!(entry.0.is_some(), "{sysno}: no capture");
            let names_arrays = NAMES_IOVEC_ARRAYS.contains(&sysno);
            assert_eq!(names_iovec_arrays(&call), names_arrays, "{sysno}");
            if names_arrays {
                assert_eq!(
                    entry.message(0),
                    EntrySegments::Declared(&[]),
                    "{sysno}: message 0 was not captured"
                );
            }
            if let Err(error) = extents(&memory, &call, 1, &entry) {
                panic!("{sysno}: {error:?}");
            }
        }
    }

    /// The cross-message shape the entry snapshot cannot see: the kernel reads
    /// header 1 only after message 0's payload has rewritten it. At entry
    /// header 1 names unmapped memory, and a delivered message must not become
    /// EFAULT; the header as the call left it is the one the kernel used.
    #[test]
    fn a_batch_header_made_readable_by_an_earlier_message_is_read_after_the_call() {
        let mut sockets = [0; 2];
        // SAFETY: `sockets` has room for the two descriptors socketpair writes.
        let paired =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sockets.as_mut_ptr()) };
        assert_eq!(paired, 0, "socketpair: {}", std::io::Error::last_os_error());

        let mut byte = 0_u8;
        let second_iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        // SAFETY: `mmsghdr` is a plain C record; every field the kernel reads
        // is set below.
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        let second_msg_iov = &raw mut headers[1].msg_hdr.msg_iov;
        let first_iov = libc::iovec {
            iov_base: second_msg_iov.cast(),
            iov_len: std::mem::size_of::<*mut libc::iovec>(),
        };
        headers[0].msg_hdr.msg_iov = (&raw const first_iov).cast_mut();
        headers[0].msg_hdr.msg_iovlen = 1;
        // Page zero is never mapped for a user process.
        headers[1].msg_hdr.msg_iov = 0x10 as *mut libc::iovec;
        headers[1].msg_hdr.msg_iovlen = 1;

        let pointer = ((&raw const second_iov) as usize).to_ne_bytes();
        for payload in [&pointer[..], b"Z"] {
            // SAFETY: `payload` is readable for its length.
            let sent = unsafe { libc::send(sockets[0], payload.as_ptr().cast(), payload.len(), 0) };
            assert_eq!(sent, payload.len() as isize);
        }
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(sockets[1])
                .with_mmsg(AddrMut::from_raw(headers.as_mut_ptr() as usize))
                .with_vlen(2),
        );
        let memory = LocalMemory::new();

        let entry = entry_iovecs(&memory, &call);
        assert!(matches!(entry.message(1), EntrySegments::Unreadable(_)));
        // SAFETY: header 0 describes a live buffer; header 1 becomes live when
        // message 0 is delivered into its `msg_iov` field.
        let received = unsafe {
            libc::recvmmsg(
                sockets[1],
                headers.as_mut_ptr(),
                2,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        close_all(sockets);
        assert_eq!(received, 2, "recvmmsg: {}", std::io::Error::last_os_error());
        assert_eq!(byte, b'Z', "the kernel read header 1 after message 0");

        assert_eq!(
            extents(&memory, &call, received as i64, &entry).unwrap(),
            vec![
                BufferExtent {
                    addr: second_msg_iov as u64,
                    len: std::mem::size_of::<*mut libc::iovec>() as u64,
                },
                BufferExtent {
                    addr: &raw const byte as u64,
                    len: 1,
                },
            ]
        );
    }

    /// The header array is read in one access, and only an array running into
    /// unmapped memory is walked header by header. The walk must keep the
    /// readable prefix and stop at the first unreadable header with its errno.
    #[test]
    fn a_batch_running_into_unmapped_memory_keeps_its_readable_prefix() {
        let page = 4096;
        // SAFETY: an anonymous two-page mapping; the second page is made
        // inaccessible below and both are released at the end. Protecting it
        // rather than unmapping it keeps a concurrent test's `mmap` from
        // landing in the hole and making header 1 readable.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        // SAFETY: the second page belongs to the mapping above.
        assert_eq!(
            unsafe { libc::mprotect(base.cast::<u8>().add(page).cast(), page, libc::PROT_NONE) },
            0
        );
        let mut byte = 0_u8;
        let iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        let size = std::mem::size_of::<libc::mmsghdr>();
        let first = base as usize + page - size;
        // SAFETY: `first` is the last whole `mmsghdr` slot in the mapped page;
        // the all-zero record is valid and only `msg_iov` is set.
        unsafe {
            let header = first as *mut libc::mmsghdr;
            header.write(std::mem::zeroed());
            (*header).msg_hdr.msg_iov = (&raw const iov).cast_mut();
            (*header).msg_hdr.msg_iovlen = 1;
        }

        let entry = mmsghdr_segments(&LocalMemory::new(), first, 3);
        assert_eq!(
            entry.message(0),
            EntrySegments::Declared(&[BufferExtent {
                addr: &raw const byte as u64,
                len: 1,
            }])
        );
        assert_eq!(entry.message(1), EntrySegments::Unreadable(Errno::EFAULT));
        assert_eq!(entry.message(2), EntrySegments::Unreadable(Errno::EFAULT));
        assert_eq!(entry.0.as_ref().map(Vec::len), Some(2));
        // SAFETY: both pages belong to the mapping above and are unused from here.
        assert_eq!(unsafe { libc::munmap(base, 2 * page) }, 0);
    }

    /// Each extent paired with its whole-extent digest, so one assertion
    /// checks both which bytes were hashed and what they contained.
    fn extent_list(moved: &[ExtentDigests]) -> Vec<(BufferExtent, Digest)> {
        moved.iter().map(|m| (m.extent, m.whole)).collect()
    }

    /// The cross-message shape where header 1 WAS readable at entry but named
    /// a buffer only message 0's payload made valid: header 1's `iovec` array
    /// is readable and its `iov_base` is 0x10 until message 0 overwrites it.
    /// The entry snapshot names 0x10; that must be retried from the arrays as
    /// the call left them rather than becoming EFAULT for a delivered batch.
    #[test]
    fn a_batch_buffer_made_valid_by_an_earlier_message_is_read_after_the_call() {
        let mut sockets = [0; 2];
        // SAFETY: `sockets` has room for the two descriptors socketpair writes.
        let paired =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sockets.as_mut_ptr()) };
        assert_eq!(paired, 0, "socketpair: {}", std::io::Error::last_os_error());

        let mut byte = 0_u8;
        // Page zero is never mapped for a user process.
        let mut second_iov = libc::iovec {
            iov_base: 0x10 as *mut libc::c_void,
            iov_len: 1,
        };
        let second_iov_base = &raw mut second_iov.iov_base;
        let first_iov = libc::iovec {
            iov_base: second_iov_base.cast(),
            iov_len: std::mem::size_of::<*mut libc::c_void>(),
        };
        // SAFETY: `mmsghdr` is a plain C record; every field the kernel reads
        // is set below.
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        headers[0].msg_hdr.msg_iov = (&raw const first_iov).cast_mut();
        headers[0].msg_hdr.msg_iovlen = 1;
        headers[1].msg_hdr.msg_iov = &raw mut second_iov;
        headers[1].msg_hdr.msg_iovlen = 1;

        let pointer = ((&raw mut byte) as usize).to_ne_bytes();
        for payload in [&pointer[..], b"Z"] {
            // SAFETY: `payload` is readable for its length.
            let sent = unsafe { libc::send(sockets[0], payload.as_ptr().cast(), payload.len(), 0) };
            assert_eq!(sent, payload.len() as isize);
        }
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(sockets[1])
                .with_mmsg(AddrMut::from_raw(headers.as_mut_ptr() as usize))
                .with_vlen(2),
        );
        let memory = LocalMemory::new();

        let entry = entry_iovecs(&memory, &call);
        assert_eq!(
            entry.message(1),
            EntrySegments::Declared(&[BufferExtent { addr: 0x10, len: 1 }]),
            "the premise: header 1 is readable at entry and names page zero"
        );
        // SAFETY: header 0 describes a live buffer; header 1's buffer becomes
        // live when message 0 is delivered into its `iov_base`.
        let received = unsafe {
            libc::recvmmsg(
                sockets[1],
                headers.as_mut_ptr(),
                2,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        close_all(sockets);
        assert_eq!(received, 2, "recvmmsg: {}", std::io::Error::last_os_error());
        assert_eq!(
            byte, b'Z',
            "the kernel read header 1's array after message 0"
        );

        let moved = moved_extent_digests(&memory, &call, received as i64, &entry).unwrap();
        assert_eq!(
            extent_list(&moved),
            vec![
                (
                    BufferExtent {
                        addr: second_iov_base as u64,
                        len: pointer.len() as u64,
                    },
                    Digest::new(&pointer),
                ),
                (
                    BufferExtent {
                        addr: &raw const byte as u64,
                        len: 1,
                    },
                    Digest::new(b"Z"),
                ),
            ]
        );
    }

    /// The deschedule window: another guest thread may rewrite a single
    /// message's array between the entry capture and the kernel's read. The
    /// captured segment is then no longer readable, and the retry must hash
    /// the segment the kernel actually filled.
    #[test]
    fn an_array_rewritten_before_the_kernel_read_it_is_read_after_the_call() {
        let page = 4096;
        // SAFETY: a fresh anonymous one-page mapping, made inaccessible below
        // and released at the end.
        let stale = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(stale, libc::MAP_FAILED);
        let mut pipe = [0; 2];
        // SAFETY: `pipe` has room for the two descriptors pipe writes.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // SAFETY: the payload is readable for its length.
        assert_eq!(unsafe { libc::write(pipe[1], b"hi".as_ptr().cast(), 2) }, 2);

        let mut fresh = [0_u8; 2];
        let mut iov = libc::iovec {
            iov_base: stale,
            iov_len: 2,
        };
        let call = Syscall::Readv(
            syscalls::Readv::new()
                .with_fd(pipe[0])
                .with_iov(Addr::from_ptr(&raw const iov))
                .with_len(1),
        );
        let memory = LocalMemory::new();
        let entry = entry_iovecs(&memory, &call);

        // What another thread does while this one is descheduled.
        iov.iov_base = fresh.as_mut_ptr().cast();
        // SAFETY: `stale` is the mapping above.
        assert_eq!(unsafe { libc::mprotect(stale, page, libc::PROT_NONE) }, 0);
        // SAFETY: the array now names `fresh`, which is live.
        let read = unsafe { libc::readv(pipe[0], &raw const iov, 1) };
        close_all(pipe);
        assert_eq!(read, 2);
        assert_eq!(&fresh, b"hi");
        assert_eq!(
            extents(&memory, &call, read as i64, &entry).unwrap(),
            vec![BufferExtent {
                addr: stale as u64,
                len: 2,
            }],
            "the premise: the entry snapshot names the stale buffer"
        );

        let moved = moved_extent_digests(&memory, &call, read as i64, &entry).unwrap();
        assert_eq!(
            extent_list(&moved),
            vec![(
                BufferExtent {
                    addr: fresh.as_ptr() as u64,
                    len: 2,
                },
                Digest::new(b"hi"),
            )]
        );
        // SAFETY: `stale` is the mapping above and unused from here.
        assert_eq!(unsafe { libc::munmap(stale, page) }, 0);
    }

    /// When the retry cannot read the arrays either, the entry snapshot's
    /// error is the one reported.
    #[test]
    fn an_extent_unreadable_both_ways_reports_the_entry_error() {
        let memory = LocalMemory::new();
        // Page zero is never mapped for a user process.
        let call = Syscall::Recvmsg(syscalls::Recvmsg::new().with_msg(AddrMut::from_raw(0x10)));
        let entry = entry_iovecs(&memory, &call);
        assert!(matches!(
            moved_extent_digests(&memory, &call, 1, &entry),
            Err(Error::Errno(Errno::EFAULT))
        ));
        assert!(
            moved_extent_digests(&memory, &call, 0, &entry)
                .unwrap()
                .is_empty()
        );
    }
}
