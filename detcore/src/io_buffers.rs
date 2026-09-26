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

/// Bind successful RNG output to the very array the synchronous handler used.
/// The return count, not the declared capacity, bounds each observed extent.
pub(crate) fn rng_readv_extents(
    iovecs: &[crate::iovecs::ImportedIovec],
    written: usize,
) -> Result<Vec<BufferExtent>, Error> {
    let mut output = rng_observation_vec(iovecs.len(), "imported geometry")?;
    let mut remaining = written;
    for iov in iovecs {
        let take = remaining.min(iov.len);
        remaining -= take;
        if take > 0 {
            output.push(BufferExtent {
                addr: iov.base as u64,
                len: take as u64,
            });
        }
    }
    Ok(output)
}

/// RNG observation happens after the output and cursor have committed. A
/// recoverable reservation failure must stop the tool, not become guest errno.
/// This does not promise to catch physical OOM or other logging allocations.
fn rng_observation_vec<T>(capacity: usize, purpose: &str) -> Result<Vec<T>, Error> {
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        Error::Tool(anyhow::anyhow!(
            "RNG readv observation after cursor commit: cannot reserve {purpose}: {error}"
        ))
    })?;
    Ok(output)
}

fn observed_extents(
    memory: &impl MemoryAccess,
    call: &Syscall,
    ret: i64,
    rng_output: Option<&[BufferExtent]>,
) -> Result<Vec<BufferExtent>, Error> {
    if let Some(output) = rng_output {
        if ret <= 0 {
            return Ok(Vec::new());
        }
        let mut observed = rng_observation_vec(output.len(), "observed geometry")?;
        let mut remaining = ret as u64;
        for extent in output {
            let take = remaining.min(extent.len);
            remaining -= take;
            if take > 0 {
                observed.push(BufferExtent {
                    addr: extent.addr,
                    len: take,
                });
            }
        }
        return Ok(observed);
    }
    extents(memory, call, ret)
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
fn iovec_extents<M: MemoryAccess>(
    memory: &M,
    iov_addr: usize,
    iov_count: usize,
    moved: i64,
) -> Result<Vec<BufferExtent>, Error> {
    let remaining = u64::try_from(moved).unwrap_or(0);
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
    memory.read_values(iov_address.into(), &mut iovecs)?;
    Ok(iovec_extents_from_slice(&iovecs, moved))
}

/// Apply the return-value bound after the guest `iovec` array has been read.
///
/// This is separate from the memory access so the short-transfer rule can be
/// tested directly for every syscall family that shares it.
fn iovec_extents_from_slice(iovecs: &[libc::iovec], moved: i64) -> Vec<BufferExtent> {
    let mut remaining = u64::try_from(moved).unwrap_or(0);

    let mut out = Vec::new();
    for iov in iovecs {
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
    out
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

/// Read a `msghdr` out of the guest and walk the iovecs it points at.
fn msghdr_extents<M: MemoryAccess>(
    memory: &M,
    msg_addr: usize,
    moved: i64,
) -> Result<Vec<BufferExtent>, Error> {
    if msg_addr == 0 {
        return Ok(Vec::new());
    }
    let address: AddrMut<'_, libc::msghdr> = AddrMut::from_raw(msg_addr).ok_or(Errno::EFAULT)?;
    let message: libc::msghdr = memory.read_value(address)?;
    iovec_extents(memory, message.msg_iov as usize, message.msg_iovlen, moved)
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
/// these calls cannot share the `clamp`/`msghdr_extents` path that every other
/// send or receive uses. Each completed message carries its own byte count in
/// `msg_len`, so each is walked separately and bounded by that; treating the
/// batch as one buffer would let one message's length run into the next
/// message's memory.
fn mmsghdr_extents<M: MemoryAccess>(
    memory: &M,
    mmsg_addr: usize,
    vlen: u32,
    delivered: i64,
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
    for header in &headers {
        out.extend(iovec_extents(
            memory,
            header.msg_hdr.msg_iov as usize,
            header.msg_hdr.msg_iovlen,
            i64::from(header.msg_len),
        )?);
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
fn extents<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    ret: i64,
) -> Result<Vec<BufferExtent>, Error> {
    // Nothing was written on a failed or empty call -- for every syscall whose
    // return value is a byte count. `ret_gates_output` is what keeps the poll
    // family out of this, and it is a predicate rather than an arm placed above
    // so the exclusion cannot be undone by moving code.
    if ret <= 0 && ret_gates_output(call) {
        return Ok(Vec::new());
    }
    if let Some((iov_addr, iov_count)) = iovec_extent_arguments(call) {
        return iovec_extents(memory, iov_addr, iov_count, ret);
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
        Syscall::Recvmsg(c) => msghdr_extents(memory, c.msg().map_or(0, |p| p.as_raw()), ret)?,
        // `ret` is a MESSAGE count here, not a byte count; see
        // `mmsghdr_extents`. recvmmsg is one of the four receive syscalls
        // that could reach a NETLINK_SOCK_DIAG dump without passing the
        // sock_diag sanitizer, so leaving it unhashed left this check blind
        // to exactly the bypass it would otherwise have reported.
        Syscall::Recvmmsg(c) => {
            mmsghdr_extents(memory, c.mmsg().map_or(0, |p| p.as_raw()), c.vlen(), ret)?
        }
        // Bytes the guest produced. These never reach stdout/stderr for a QEMU
        // boot -- measured, all 234,872 writes went to fds 7/12/14/11/13/4/8/19/23
        // and none to fd 1 or 2 -- so `--verify`'s stdout/stderr comparison does
        // not cover them either.
        Syscall::Write(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Pwrite64(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.len(), ret),
        Syscall::Sendto(c) => clamp(c.buf().map(|p| p.as_raw() as u64), c.size(), ret),
        Syscall::Sendmsg(c) => msghdr_extents(memory, c.msg().map_or(0, |p| p.as_raw()), ret)?,
        // Like recvmmsg, `ret` counts completed messages and each completed
        // header's `msg_len` bounds the bytes consumed from that message.
        Syscall::Sendmmsg(c) => {
            mmsghdr_extents(memory, c.msgvec().map_or(0, |p| p.as_raw()), c.vlen(), ret)?
        }
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

fn extent_digests<G, T>(
    guest: &mut G,
    addr: u64,
    len: u64,
    rng_output: bool,
) -> Result<(Digest, usize, Vec<String>), Error>
where
    G: Guest<T>,
    T: Tool,
{
    let size = len as usize;
    let mut buf = if rng_output {
        let mut buf = rng_observation_vec(size, "digest payload")?;
        buf.resize(size, 0u8);
        buf
    } else {
        vec![0u8; size]
    };
    if size > 0 {
        let start = Addr::<u8>::from_raw(addr as usize).ok_or(Errno::EFAULT)?;
        guest.memory().read_values(start, buf.as_mut_slice())?;
    }
    let whole = Digest::new(buf.as_slice());
    let (chunk, chunks) = chunk_digests(buf.as_slice());
    Ok((whole, chunk, chunks))
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
    rng_output: Option<&[BufferExtent]>,
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
    let moved_extents = {
        let memory = guest.memory();
        observed_extents(&memory, call, ret, rng_output)?
    };
    for extent in moved_extents {
        let (whole, chunk, chunks) =
            extent_digests(guest, extent.addr, extent.len, rng_output.is_some()).map_err(
                |error| {
                    if rng_output.is_some() {
                        Error::Tool(anyhow::Error::new(error).context(format!(
                            "RNG readv observation after cursor commit: digest read at {:#x}+{}",
                            extent.addr, extent.len
                        )))
                    } else {
                        error
                    }
                },
            )?;
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
mod event_tests {
    include!("io_buffers/user_access_event_tests.rs");
    use std::io::IoSlice;
    use std::io::IoSliceMut;
    use std::sync::Arc;
    use std::sync::Mutex;

    use nix::fcntl::OFlag;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Pid;
    use tokio::sync::oneshot;
    use tracing::Event;
    use tracing::Id;
    use tracing::Level;
    use tracing::Metadata;
    use tracing::Subscriber;
    use tracing::field::Field;
    use tracing::field::Visit;
    use tracing::span::Attributes;
    use tracing::span::Record;

    use super::*;
    use crate::Config;
    use crate::Detcore;
    use crate::GlobalState;
    use crate::ThreadState;
    use crate::fd::DetFd;
    use crate::fd::FdType;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
    use crate::tool_global::GlobalRequest;
    use crate::tool_global::GlobalResponse;
    use crate::tool_global::ResumeStatus;
    use crate::types::DetPid;
    use crate::types::OpenFileId;

    const FD: i32 = 3;
    const IOV: usize = 0x1000;
    const FIRST_DEST: usize = 0x2000;
    const RETRY_DEST: usize = 0x3000;
    const PIPE_BYTES: &[u8; 4] = b"pipe";
    const CANARY: u8 = 0xa5;

    // Numeric guest addresses never become host slices. The fixed arena also
    // makes corrupted descriptors fail promptly rather than allocate by length.
    #[derive(Clone)]
    struct EventMemory(Arc<Mutex<Vec<u8>>>, Arc<Mutex<MemoryReads>>);

    #[derive(Default)]
    struct MemoryReads {
        imported_entries: usize,
        observer_reads: Vec<(usize, usize)>,
        digest_error: Option<Errno>,
        user_copy_audit: bool,
        copy_done: bool,
        copy_actions: std::collections::VecDeque<(usize, Result<usize, Errno>)>,
        copy_lengths: Vec<usize>,
        after_copy: Vec<&'static str>,
    }

    impl EventMemory {
        fn new() -> Self {
            Self(
                Arc::new(Mutex::new(vec![CANARY; 0x4000])),
                Arc::new(Mutex::new(MemoryReads::default())),
            )
        }

        fn put_iovec(&self, index: usize, base: usize, len: usize) {
            let start = IOV + index * std::mem::size_of::<libc::iovec>();
            let mut bytes = self.0.lock().unwrap();
            bytes[start..start + 8].copy_from_slice(&base.to_ne_bytes());
            bytes[start + 8..start + 16].copy_from_slice(&len.to_ne_bytes());
        }

        fn iovec(&self, index: usize) -> (usize, usize) {
            let address = IOV + index * std::mem::size_of::<libc::iovec>();
            let iov: libc::iovec = self
                .read_value(Addr::<libc::iovec>::from_raw(address).unwrap())
                .unwrap();
            (iov.iov_base as usize, iov.iov_len)
        }

        fn bytes(&self, address: usize, len: usize) -> Vec<u8> {
            self.0.lock().unwrap()[address..address + len].to_vec()
        }

        fn copy_read(&self, start: usize, buf: &mut [u8]) -> Result<(), Errno> {
            let end = start.checked_add(buf.len()).ok_or(Errno::EFAULT)?;
            let bytes = self.0.lock().unwrap();
            buf.copy_from_slice(bytes.get(start..end).ok_or(Errno::EFAULT)?);
            Ok(())
        }
    }

    impl MemoryAccess for EventMemory {
        fn write_with_user_access(
            &mut self,
            addr: AddrMut<u8>,
            buf: &[u8],
        ) -> Result<usize, Errno> {
            let (written, outcome) = {
                let mut audit = self.1.lock().unwrap();
                audit.copy_lengths.push(buf.len());
                audit
                    .copy_actions
                    .pop_front()
                    .unwrap_or((buf.len(), Ok(buf.len())))
            };
            assert!(written <= buf.len());
            let start = addr.as_raw();
            let end = start.checked_add(written).ok_or(Errno::EFAULT)?;
            self.0
                .lock()
                .unwrap()
                .get_mut(start..end)
                .ok_or(Errno::EFAULT)?
                .copy_from_slice(&buf[..written]);
            self.1.lock().unwrap().copy_done = true;
            outcome
        }

        fn read_vectored(
            &self,
            _remote: &[IoSlice],
            _local: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("event fixture must use its scalar memory override")
        }

        fn write_vectored(
            &mut self,
            _local: &[IoSlice],
            _remote: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("event fixture must use its scalar memory override")
        }

        fn read<'a, A>(&self, addr: A, buf: &mut [u8]) -> Result<usize, Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            let start = addr.into().as_raw();
            let mut reads = self.1.lock().unwrap();
            reads.observer_reads.push((start, buf.len()));
            if reads.user_copy_audit && reads.copy_done {
                reads.after_copy.push("memory-read");
            }
            if start == FIRST_DEST
                && let Some(error) = reads.digest_error
            {
                return Err(error);
            }
            drop(reads);
            self.copy_read(start, buf)?;
            Ok(buf.len())
        }

        fn read_exact_with_user_access<'a, A>(&self, addr: A, buf: &mut [u8]) -> Result<(), Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            {
                let mut reads = self.1.lock().unwrap();
                reads.imported_entries += 1;
                if reads.user_copy_audit && reads.copy_done {
                    reads.after_copy.push("user-read");
                }
            }
            self.copy_read(addr.into().as_raw(), buf)
        }

        fn write(&mut self, addr: AddrMut<u8>, buf: &[u8]) -> Result<usize, Errno> {
            assert!(
                !self.1.lock().unwrap().user_copy_audit,
                "random output used debugger write"
            );
            let start = addr.as_raw();
            let end = start.checked_add(buf.len()).ok_or(Errno::EFAULT)?;
            let mut bytes = self.0.lock().unwrap();
            bytes
                .get_mut(start..end)
                .ok_or(Errno::EFAULT)?
                .copy_from_slice(buf);
            Ok(buf.len())
        }
    }

    type RetryGate = (oneshot::Sender<()>, oneshot::Receiver<()>);

    struct EventGuest {
        config: Config,
        thread: ThreadState<()>,
        memory: EventMemory,
        injected_iovecs: Vec<(usize, usize)>,
        injected_zero_reads: usize,
        polls: Mutex<Vec<u32>>,
        releases: Mutex<usize>,
        retry_gate: Mutex<Option<RetryGate>>,
    }

    impl EventGuest {
        fn audit_call(&self, operation: &'static str) -> bool {
            let mut audit = self.memory.1.lock().unwrap();
            if audit.user_copy_audit && audit.copy_done {
                audit.after_copy.push(operation);
            }
            audit.user_copy_audit
        }
    }

    struct UnusedStack;
    struct UnusedStackGuard;

    impl Drop for UnusedStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for UnusedStack {
        type StackGuard = UnusedStackGuard;

        fn size(&self) -> usize {
            panic!("readv event must not use a guest stack")
        }

        fn capacity(&self) -> usize {
            panic!("readv event must not use a guest stack")
        }

        fn push<'stack, T>(&mut self, _value: T) -> Addr<'stack, T> {
            panic!("readv event must not use a guest stack")
        }

        fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
            panic!("readv event must not use a guest stack")
        }

        fn commit(self) -> Result<Self::StackGuard, Errno> {
            panic!("readv event must not use a guest stack")
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for EventGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let response = match message.2 {
                GlobalRequest::RequestResources(request, _) => {
                    assert_eq!(request.resources.len(), 1);
                    assert_eq!(
                        request.resources.get(&ResourceID::InternalIOPolling),
                        Some(&Permission::W)
                    );
                    self.polls.lock().unwrap().push(request.poll_attempt);
                    match request.poll_attempt {
                        0 => {}
                        1 => {
                            assert_eq!(self.injected_iovecs, [(FIRST_DEST, 8)]);
                            let (parked, resume) = self.retry_gate.lock().unwrap().take().unwrap();
                            parked.send(()).unwrap();
                            resume.await.unwrap();
                        }
                        attempt => panic!("unexpected extra readv poll {attempt}"),
                    }
                    GlobalResponse::RequestResources(ResumeStatus::Normal)
                }
                GlobalRequest::ReleaseAllResources => {
                    self.audit_call("release");
                    *self.releases.lock().unwrap() += 1;
                    GlobalResponse::ReleaseAllResources(())
                }
                request => panic!("unexpected readv event RPC: {request:?}"),
            };
            (None, response)
        }

        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for EventGuest {
        type Memory = EventMemory;
        type Stack = UnusedStack;

        fn tid(&self) -> Pid {
            Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> Pid {
            Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<Pid> {
            None
        }

        fn memory(&self) -> Self::Memory {
            self.memory.clone()
        }

        fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            if self.audit_call("regs") {
                return libc::user_regs_struct {
                    rip: 0x4000,
                    eflags: 2,
                    ..unsafe { std::mem::zeroed() }
                };
            }
            panic!("buffer observation must not request register evidence")
        }

        async fn set_regs(&mut self, _regs: libc::user_regs_struct) -> Result<(), Error> {
            assert!(self.audit_call("set-regs"));
            Ok(())
        }

        fn detlog_memory_regions(&self) -> Option<Vec<reverie::DetlogMemoryRegion>> {
            if !self.audit_call("memory-regions") {
                return None;
            }
            Some(vec![
                reverie::DetlogMemoryRegion {
                    kind: reverie::DetlogRegionKind::Stack,
                    start: FIRST_DEST as u64,
                    end: (FIRST_DEST + 8) as u64,
                },
                reverie::DetlogMemoryRegion {
                    kind: reverie::DetlogRegionKind::Heap,
                    start: RETRY_DEST as u64,
                    end: (RETRY_DEST + 8) as u64,
                },
            ])
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("readv event must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("readv event must not daemonize")
        }

        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            let (number, args) = syscall.into_parts();
            if let Syscall::Read(call) = Syscall::from_raw(number, args) {
                assert_eq!(call.len(), 0);
                self.injected_zero_reads += 1;
                if call.fd() == -1 {
                    return Err(Errno::EBADF);
                }
                assert_eq!(call.fd(), FD);
                assert_ne!(
                    self.thread.with_detfd(FD, |fd| fd.ty()).unwrap(),
                    FdType::Rng
                );
                return Err(Errno::EINVAL);
            }
            let Syscall::Readv(call) = Syscall::from_raw(number, args) else {
                panic!("unexpected injected syscall {number}");
            };
            assert_eq!(call.fd(), FD);
            assert_eq!(call.iov().unwrap().as_raw(), IOV);
            assert_eq!(call.len(), 1);
            assert_eq!(
                self.thread.with_detfd(FD, |fd| fd.ty()).unwrap(),
                FdType::Pipe,
                "an RNG readv must be emulated without injection"
            );
            // Each actual nonblocking kernel attempt imports its own array.
            // The first attempt moves nothing; only the retry writes bytes.
            let (base, len) = self.memory.iovec(0);
            self.injected_iovecs.push((base, len));
            match self.injected_iovecs.len() {
                1 => Err(Errno::EAGAIN),
                2 => {
                    assert!(len >= PIPE_BYTES.len());
                    self.memory
                        .write_exact(AddrMut::from_raw(base).unwrap(), PIPE_BYTES)?;
                    Ok(PIPE_BYTES.len() as i64)
                }
                attempt => panic!("unexpected extra readv injection {attempt}"),
            }
        }

        async fn tail_inject<S: SyscallInfo>(&mut self, _syscall: S) -> reverie::Never {
            panic!("readv must return through the event observer")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), Error> {
            if self.audit_call("timer") {
                return Ok(());
            }
            panic!("event fixture has no PMU timeslice")
        }

        fn set_timer_precise(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), Error> {
            if self.audit_call("timer") {
                return Ok(());
            }
            panic!("event fixture has no PMU timeslice")
        }

        fn read_clock(&mut self) -> Result<u64, Error> {
            if self.audit_call("clock") {
                return Ok(0);
            }
            panic!("event fixture must not read a host clock")
        }
    }

    fn event_guest(ty: FdType, retry_gate: Option<RetryGate>) -> (Detcore, EventGuest) {
        let config = Config {
            seed: 0,
            rng_seed: Some(0),
            sequentialize_threads: true,
            recordreplay_modes: false,
            record_preemptions: false,
            max_timeslice: None,
            detlog_io_buffers: true,
            detlog_heap: false,
            detlog_stack: false,
            detlog_regs: false,
            backend_is_kvm: true,
            syscall_clobbers_virtualized_by_backend: true,
            ..Config::default()
        };
        let pid = DetPid::from_raw(1);
        let mut thread = ThreadState::new(pid, &config, ());
        thread.detpid = Some(pid);
        let fd = DetFd::new(FD, OFlag::O_RDONLY, ty, OpenFileId::new(pid, 99));
        if ty == FdType::Pipe {
            fd.set_physically_nonblocking();
        }
        thread
            .file_metadata
            .lock()
            .unwrap()
            .file_handles
            .insert(FD, fd);
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
        let guest = EventGuest {
            config,
            thread,
            memory: EventMemory::new(),
            injected_iovecs: Vec::new(),
            injected_zero_reads: 0,
            polls: Mutex::new(Vec::new()),
            releases: Mutex::new(0),
            retry_gate: Mutex::new(retry_gate),
        };
        (tool, guest)
    }

    #[derive(Clone, Default)]
    struct BufferLog(Arc<Mutex<Vec<String>>>);

    struct MessageVisitor(Option<String>);

    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    impl Subscriber for BufferLog {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::INFO
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = MessageVisitor(None);
            event.record(&mut visitor);
            if let Some(message) = visitor.0
                && message.contains("[iobuf]")
            {
                self.0.lock().unwrap().push(message);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    fn readv(count: usize) -> Syscall {
        reverie::syscalls::Readv::new()
            .with_fd(FD)
            .with_iov(Addr::from_raw(IOV))
            .with_len(count)
            .into()
    }

    fn assert_extent(message: &str, address: usize, bytes: &[u8]) {
        let expected = format!(
            "[iobuf][dtid 1] readv in fd={FD} {address:#x}+{}->{}",
            bytes.len(),
            Digest::new(bytes)
        );
        assert!(
            message.contains(&expected),
            "expected {expected}, got {message}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pipe_readv_event_observes_destination_imported_after_retry_wait() {
        let logs = BufferLog::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let (parked_tx, parked_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (tool, mut guest) = event_guest(FdType::Pipe, Some((parked_tx, resume_rx)));
        let memory = guest.memory.clone();
        memory.put_iovec(0, FIRST_DEST, 8);

        // This is the real Detcore syscall-event dispatch and observation path,
        // including its nonblocking EAGAIN retry. No helper supplies geometry.
        let event = tool.handle_syscall_event(&mut guest, readv(1));
        let mutate_during_retry = async {
            parked_rx.await.unwrap();
            assert_eq!(memory.iovec(0), (FIRST_DEST, 8));
            assert_eq!(memory.bytes(FIRST_DEST, 8), [CANARY; 8]);
            memory.put_iovec(0, RETRY_DEST, 8);
            resume_tx.send(()).unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(event, mutate_during_retry)
        })
        .await
        .expect("readv did not cross its retry resource wait");
        assert_eq!(result.unwrap(), 4);
        assert_eq!(guest.injected_iovecs, [(FIRST_DEST, 8), (RETRY_DEST, 8)]);
        assert_eq!(*guest.polls.lock().unwrap(), [0, 1]);
        assert_eq!(*guest.releases.lock().unwrap(), 1);
        assert_eq!(guest.thread.stats.syscall_count, 1);
        assert_eq!(memory.bytes(FIRST_DEST, 8), [CANARY; 8]);
        assert_eq!(
            memory.bytes(RETRY_DEST, 4).as_slice(),
            PIPE_BYTES.as_slice()
        );
        assert_eq!(memory.bytes(RETRY_DEST + 4, 4), [CANARY; 4]);
        let messages = logs.0.lock().unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_extent(&messages[0], RETRY_DEST, PIPE_BYTES);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rng_readv_event_observes_imported_array_after_output_overwrites_it() {
        let logs = BufferLog::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let (tool, mut guest) = event_guest(FdType::Rng, None);
        let memory = guest.memory.clone();
        let second_iovec = IOV + std::mem::size_of::<libc::iovec>();
        memory.put_iovec(0, second_iovec, 16);
        memory.put_iovec(1, FIRST_DEST, 4);
        let original_second_iovec = memory.bytes(second_iovec, 16);

        let result = tool.handle_syscall_event(&mut guest, readv(2)).await;
        assert_eq!(result.unwrap(), 20);
        let expected = [
            41, 114, 187, 4, 77, 150, 223, 40, 113, 186, 3, 76, 149, 222, 39, 112, 185, 2, 75, 148,
        ];
        assert_eq!(memory.bytes(second_iovec, 16), expected[..16]);
        assert_ne!(memory.bytes(second_iovec, 16), original_second_iovec);
        assert_eq!(memory.bytes(FIRST_DEST, 4), expected[16..]);
        assert_eq!(memory.bytes(FIRST_DEST + 4, 4), [CANARY; 4]);
        assert!(guest.injected_iovecs.is_empty());
        assert!(guest.polls.lock().unwrap().is_empty());
        assert_eq!(*guest.releases.lock().unwrap(), 1);
        assert_eq!(guest.thread.stats.syscall_count, 1);
        assert_eq!(
            guest
                .thread
                .with_detfd(FD, |fd| fd.random_device_offset())
                .unwrap(),
            20
        );
        let messages = logs.0.lock().unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_extent(&messages[0], second_iovec, &expected[..16]);
        assert_extent(&messages[1], FIRST_DEST, &expected[16..]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rng_readv_event_digest_failure_is_terminal_after_bytes_and_cursor_commit() {
        for fault in [Errno::EFAULT, Errno::EIO] {
            let logs = BufferLog::default();
            let _subscriber = tracing::subscriber::set_default(logs.clone());
            let (tool, mut guest) = event_guest(FdType::Rng, None);
            let memory = guest.memory.clone();
            memory.put_iovec(0, RETRY_DEST, 3);
            memory.put_iovec(1, FIRST_DEST, 5);
            memory.1.lock().unwrap().digest_error = Some(fault);

            let result = tool.handle_syscall_event(&mut guest, readv(2)).await;
            let Err(Error::Tool(error)) = result else {
                panic!("completed RNG readv digest failure became guest result: {result:?}");
            };
            assert!(
                error
                    .to_string()
                    .contains("RNG readv observation after cursor commit")
            );
            assert!(error.chain().any(|cause| {
                matches!(cause.downcast_ref::<Error>(), Some(Error::Errno(error)) if *error == fault)
            }));
            assert_eq!(memory.bytes(RETRY_DEST, 4), [41, 114, 187, CANARY]);
            assert_eq!(memory.bytes(FIRST_DEST, 6), [4, 77, 150, 223, 40, CANARY]);
            assert_eq!(
                guest
                    .thread
                    .with_detfd(FD, |fd| fd.random_device_offset())
                    .unwrap(),
                8
            );
            assert_eq!(*guest.releases.lock().unwrap(), 1);
            assert!(guest.injected_iovecs.is_empty());
            let reads = memory.1.lock().unwrap();
            assert_eq!(reads.imported_entries, 2);
            assert_eq!(reads.observer_reads, [(RETRY_DEST, 3), (FIRST_DEST, 5)]);
            let messages = logs.0.lock().unwrap();
            assert_eq!(messages.len(), 1);
            assert_extent(&messages[0], RETRY_DEST, &[41, 114, 187]);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rng_readv_event_observer_configuration_and_subscription_are_inert() {
        for (configured, subscribed) in [(true, true), (false, true), (true, false)] {
            let logs = BufferLog::default();
            let dispatch = if subscribed {
                tracing::Dispatch::new(logs.clone())
            } else {
                tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())
            };
            let _subscriber = tracing::dispatcher::set_default(&dispatch);
            let (mut tool, mut guest) = event_guest(FdType::Rng, None);
            tool.cfg.detlog_io_buffers = configured;
            guest.config.detlog_io_buffers = configured;
            let memory = guest.memory.clone();
            memory.put_iovec(0, FIRST_DEST, 3);
            memory.put_iovec(1, RETRY_DEST, 5);

            // Import and evidence must narrow the same raw count. In particular,
            // the observer must not fetch 1024 descriptors after a two-entry read.
            let result = tool
                .handle_syscall_event(&mut guest, readv((1_usize << 32) | 2))
                .await;
            assert_eq!(result.unwrap(), 8);
            assert_eq!(memory.bytes(FIRST_DEST, 4), [41, 114, 187, CANARY]);
            assert_eq!(memory.bytes(RETRY_DEST, 6), [4, 77, 150, 223, 40, CANARY]);
            assert_eq!(
                guest
                    .thread
                    .with_detfd(FD, |fd| fd.random_device_offset())
                    .unwrap(),
                8
            );
            assert_eq!(*guest.releases.lock().unwrap(), 1);
            assert!(guest.injected_iovecs.is_empty());
            let reads = memory.1.lock().unwrap();
            assert_eq!(reads.imported_entries, 2);
            let messages = logs.0.lock().unwrap();
            if configured && subscribed {
                assert_eq!(reads.observer_reads, [(FIRST_DEST, 3), (RETRY_DEST, 5)]);
                assert_eq!(messages.len(), 2);
                assert_extent(&messages[0], FIRST_DEST, &[41, 114, 187]);
                assert_extent(&messages[1], RETRY_DEST, &[4, 77, 150, 223, 40]);
            } else {
                assert!(
                    reads.observer_reads.is_empty(),
                    "disabled observer read guest bytes"
                );
                assert!(messages.is_empty(), "disabled observer emitted hashes");
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rng_readv_event_zero_and_malformed_requests_have_no_observer_effects() {
        for (count, length, expected, imports) in [
            (0, 3, Ok(0), 0),
            (1025, 3, Err(Errno::EINVAL), 0),
            (1, usize::MAX, Err(Errno::EINVAL), 1),
        ] {
            let logs = BufferLog::default();
            let _subscriber = tracing::subscriber::set_default(logs.clone());
            let (tool, mut guest) = event_guest(FdType::Rng, None);
            let memory = guest.memory.clone();
            memory.put_iovec(0, FIRST_DEST, length);
            let result = tool.handle_syscall_event(&mut guest, readv(count)).await;
            assert_eq!(
                result.map_err(|error| error.into_errno().unwrap()),
                expected
            );
            assert_eq!(memory.bytes(FIRST_DEST, 8), [CANARY; 8]);
            assert_eq!(
                guest
                    .thread
                    .with_detfd(FD, |fd| fd.random_device_offset())
                    .unwrap(),
                0
            );
            assert_eq!(*guest.releases.lock().unwrap(), 1);
            let reads = memory.1.lock().unwrap();
            assert_eq!(reads.imported_entries, imports);
            assert!(reads.observer_reads.is_empty());
            assert!(logs.0.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rng_scalar_zero_read_checks_access_without_touching_replay_placeholders() {
        for mode in [
            OFlag::O_RDONLY,
            OFlag::O_RDWR,
            OFlag::O_WRONLY,
            OFlag::O_ACCMODE,
            OFlag::O_PATH,
        ] {
            for address in [0, usize::MAX] {
                let logs = BufferLog::default();
                let _subscriber = tracing::subscriber::set_default(logs.clone());
                let (tool, mut guest) = event_guest(FdType::Rng, None);
                let fd = DetFd::new(
                    FD,
                    mode,
                    FdType::Rng,
                    OpenFileId::new(DetTid::from_raw(1), 99),
                );
                fd.advance_random_device_offset(7);
                guest
                    .thread
                    .file_metadata
                    .lock()
                    .unwrap()
                    .file_handles
                    .insert(FD, fd);
                let call = reverie::syscalls::Read::new()
                    .with_fd(FD)
                    .with_buf(AddrMut::from_raw(address))
                    .with_len(0);
                let result = tool.handle_syscall_event(&mut guest, call.into()).await;
                let expected = if mode == OFlag::O_RDONLY || mode == OFlag::O_RDWR {
                    if address == 0 {
                        Ok(0)
                    } else {
                        Err(Errno::EFAULT)
                    }
                } else {
                    Err(Errno::EBADF)
                };
                assert_eq!(
                    result.map_err(|error| error.into_errno().unwrap()),
                    expected
                );
                assert_eq!(
                    guest
                        .thread
                        .with_detfd(FD, |fd| fd.random_device_offset())
                        .unwrap(),
                    7
                );
                assert!(guest.injected_iovecs.is_empty());
                assert_eq!(guest.injected_zero_reads, 0);
                assert!(guest.polls.lock().unwrap().is_empty());
                let reads = guest.memory.1.lock().unwrap();
                assert_eq!(reads.imported_entries, 0);
                assert!(reads.observer_reads.is_empty());
                assert!(logs.0.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_rng_zero_reads_keep_the_backend_result() {
        for (fd, expected) in [(FD, Errno::EINVAL), (-1, Errno::EBADF)] {
            let (tool, mut guest) = event_guest(FdType::Regular, None);
            let call = reverie::syscalls::Read::new().with_fd(fd).with_len(0);
            let result = tool.handle_syscall_event(&mut guest, call.into()).await;
            assert!(matches!(result, Err(Error::Errno(error)) if error == expected));
            // The incumbent fd precheck rejects -1 before the handler's
            // injected zero-read path. A valid ordinary fd still reaches it.
            assert_eq!(guest.injected_zero_reads, usize::from(fd == FD));
            assert!(guest.memory.1.lock().unwrap().observer_reads.is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls;
    use reverie::syscalls::LocalMemory;

    #[test]
    fn rng_observation_capacity_failure_is_a_tool_error() {
        let error = rng_observation_vec::<u8>(usize::MAX, "digest payload").unwrap_err();
        let Error::Tool(error) = error else {
            panic!("post-commit allocation failure must not become guest errno");
        };
        assert!(error.to_string().contains("after cursor commit"));
        assert!(error.to_string().contains("digest payload"));
    }

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
            assert_eq!(extents(&memory, &call, 6).unwrap(), expected);
            assert!(extents(&memory, &call, 0).unwrap().is_empty());
            assert!(extents(&memory, &call, -1).unwrap().is_empty());
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
        assert_eq!(extents(&memory, &call, 1).unwrap(), first_message);

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
        assert_eq!(extents(&memory, &call, 2).unwrap(), both_messages);
        assert!(extents(&memory, &call, 0).unwrap().is_empty());
        assert!(extents(&memory, &call, -1).unwrap().is_empty());
        assert_eq!(completed_mmsghdr_count(1, 2), 1);
    }
}
