/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls dealing with IO and networking.
//!
//! Of course this overlaps somewhat with "files.rs".

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::os::unix::io::RawFd;
use std::time::Duration;

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Displayable;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use tracing::debug;
use tracing::trace;

use crate::config::SchedHeuristic;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::resources::SABRE_LOOPBACK_POLL_YIELD_FYI;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::TimeoutableSyscall;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::syscalls::signal::read_kernel_sigset;
use crate::tool_global::*;
use crate::tool_local::Detcore;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::OpenFileId;

// Printing helper
// TODO: this should be subsumed by better syscall printing.
fn print_poll(call: &syscalls::Poll) {
    let len = call.nfds();
    debug!("POLL: on {} fds, timeout {}", len, call.timeout());
    // TODO: nicer API for reading arrays from the guest:
    unsafe {
        for i in 0..len {
            debug!(
                "POLL: fd {} = {}",
                i,
                call.fds().unwrap().offset(i as isize)
            );
        }
    }
}

/// Build the scheduler request for a zero-timeout poll.
///
/// An empty request only rotates the caller within its current priority band. That is not a
/// yield when a busy poller has a higher priority than the producer whose readiness it is
/// probing. `SchedYield` excludes the caller from the next selection, allowing exactly one
/// other runnable guest to make progress before the nonblocking poll executes. Limit that strong
/// yield to SaBRe tasks with a loopback peer: libcurl alternates its loopback socket with an
/// internal wakeup fd, and either zero-timeout probe can otherwise starve the peer. General
/// build-tool polling retains the existing empty-turn behavior.
fn zero_timeout_poll_request(dettid: DetTid, yield_to_peer: bool) -> Resources {
    let mut request = Resources::new(dettid);
    if yield_to_peer {
        request.insert(ResourceID::SchedYield, Permission::W);
        request.fyi(SABRE_LOOPBACK_POLL_YIELD_FYI);
    }
    request
}

/// One virtual timerfd event an epoll wait may report, with what delivering
/// it commits: the interest key and the arming generation it observed.
struct EpollTimerEvent {
    key: (i32, OpenFileId),
    generation: u64,
    event: libc::epoll_event,
}

/// One wait round on an epoll that may shadow virtual timerfds, decided
/// before the host probe so that the timers can claim maxevents slots
/// ahead of it.
struct EpollTimerRound {
    /// Whether the epoll still shadows any live virtual timerfd interest.
    watched: bool,
    /// The virtual timerfd events ready now, in the epoll's ready order.
    ready: Vec<EpollTimerEvent>,
    /// The maxevents the host probe may fill when the ready timers have the
    /// first claim, or None to probe with the guest's own maxevents. Zero
    /// means the timers fill every slot and the host is not probed.
    host_limit: Option<i32>,
}

/// The per-round state of a wait that may involve virtual timerfds.
enum TimerWaitRound {
    /// poll and ppoll report every ready entry, so their timer scan simply
    /// follows the host probe.
    Poll,
    /// epoll decides its timer events before the probe.
    Epoll(EpollTimerRound),
}

impl TimerWaitRound {
    fn host_limit(&self) -> Option<i32> {
        match self {
            TimerWaitRound::Poll => None,
            TimerWaitRound::Epoll(round) => round.host_limit,
        }
    }
}

/// A host probe of a wait whose output capacity a round can lower: epoll's
/// maxevents. poll and ppoll never get a host limit (see
/// [`TimerWaitRound::Poll`]), so theirs is the identity.
trait TimerWaitProbe: Copy {
    fn with_host_limit(self, limit: i32) -> Self;
}

impl TimerWaitProbe for syscalls::EpollWait {
    fn with_host_limit(self, limit: i32) -> Self {
        self.with_maxevents(limit)
    }
}

impl TimerWaitProbe for syscalls::EpollPwait {
    fn with_host_limit(self, limit: i32) -> Self {
        self.with_maxevents(limit)
    }
}

impl TimerWaitProbe for syscalls::Poll {
    fn with_host_limit(self, _limit: i32) -> Self {
        self
    }
}

impl TimerWaitProbe for syscalls::Ppoll {
    fn with_host_limit(self, _limit: i32) -> Self {
        self
    }
}

/// Linux's largest maxevents: `EP_MAX_EVENTS`, `INT_MAX / sizeof(struct
/// epoll_event)`. epoll_wait fails with EINVAL above it or at zero or below.
const EP_MAX_EVENTS: i32 = (i32::MAX as usize / std::mem::size_of::<libc::epoll_event>()) as i32;

/// The lowest top of the user address range on any x86_64 host (4-level
/// paging). Linux checks the whole output array against its own limit before
/// waiting; a round lowers maxevents only when that check cannot fail on any
/// host, so the probe keeps reporting Linux's EFAULT for a bad array.
const EPOLL_EVENTS_ADDRESS_LIMIT: u64 = 0x7fff_ffff_f000;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3229): virtual timerfd and host fairness under maxevents.
/// The maxevents a host probe may fill when `ready` virtual timer events
/// have the first claim on `maxevents`, or None when the probe must keep the
/// guest's own maxevents because Linux would reject the call: maxevents out
/// of range (EINVAL) or an output array that may cross the top of the user
/// address range (EFAULT).
fn epoll_host_limit(events_raw: Option<usize>, maxevents: i32, ready: usize) -> Option<i32> {
    if ready == 0 || !(1..=EP_MAX_EVENTS).contains(&maxevents) {
        return None;
    }
    let events = events_raw? as u64;
    let bytes = maxevents as u64 * std::mem::size_of::<libc::epoll_event>() as u64;
    if events.checked_add(bytes)? > EPOLL_EVENTS_ADDRESS_LIMIT {
        return None;
    }
    let claimed = ready.min(maxevents as usize) as i32;
    Some(maxevents - claimed)
}

/// Which side claims maxevents slots first on the epoll's next wait, given
/// this round's outcome; None keeps the current order.
///
/// When the timers had the first claim (`host_limit` is Some) and the host
/// filled all of its share, it may have had more ready, so the host goes
/// first next time. When the host had the first claim and pushed out at
/// least one ready timer event, the timers go first next time. Otherwise
/// nothing was cut, and the order stays. The host keeps its own ready order
/// within its share, so neither side can starve the other.
fn next_epoll_fill_order(
    host_limit: Option<i32>,
    host_events: i64,
    omitted_timers: usize,
) -> Option<bool> {
    match host_limit {
        Some(limit) => (host_events >= i64::from(limit)).then_some(false),
        None => (host_events > 0 && omitted_timers > 0).then_some(true),
    }
}

/// What a blocking wait that may involve virtual timerfds rescans after each
/// host probe. A virtual timerfd never arms its host vessel, so the host probe
/// alone can never report it.
#[derive(Clone, Copy)]
enum TimerWaitSet {
    /// A poll or ppoll pollfd array.
    Poll { fds_raw: Option<usize>, nfds: u64 },
    /// An epoll instance and the caller's output array.
    Epoll {
        epfd: i32,
        events_raw: Option<usize>,
        maxevents: i32,
    },
}

impl From<syscalls::EpollWait> for TimerWaitSet {
    fn from(call: syscalls::EpollWait) -> Self {
        TimerWaitSet::Epoll {
            epfd: call.epfd(),
            events_raw: call.events().map(|addr| addr.as_raw()),
            maxevents: call.maxevents(),
        }
    }
}

impl From<syscalls::EpollPwait> for TimerWaitSet {
    fn from(call: syscalls::EpollPwait) -> Self {
        TimerWaitSet::Epoll {
            epfd: call.epfd(),
            events_raw: call.events().map(|addr| addr.as_raw()),
            maxevents: call.maxevents(),
        }
    }
}

/// Largest pollfd array scanned for virtual timerfds. Linux rejects arrays
/// longer than RLIMIT_NOFILE with EINVAL, and its hard ceiling (fs.nr_open)
/// defaults to 2^20.
const POLL_TIMERFD_SCAN_MAX_NFDS: u64 = 1 << 20;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
/// How long handing an expired timerfd to the kernel waits for its 1 ns
/// arming to fire.
const TIMERFD_HANDOFF_POLL_MS: libc::c_int = 1000;

/// select and pselect6 descriptors above this are not checked for epolls
/// whose timerfds must be handed to the kernel.
const SELECT_EPOLL_SCAN_MAX_NFDS: i32 = 1 << 20;

/// The guest's pollfd array, or nothing when it cannot be read (the real
/// call then reports the fault) or is larger than any scan here covers.
fn read_guest_pollfds<T, G>(guest: &mut G, fds_raw: Option<usize>, nfds: u64) -> Vec<libc::pollfd>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(fds_raw) = fds_raw else {
        return Vec::new();
    };
    if nfds == 0 || nfds > POLL_TIMERFD_SCAN_MAX_NFDS {
        return Vec::new();
    }
    let mut entries = vec![
        libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        };
        nfds as usize
    ];
    let Some(addr) = AddrMut::<u8>::from_raw(fds_raw) else {
        return Vec::new();
    };
    // SAFETY: pollfd is plain old data; the byte view covers exactly `entries`.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            entries.as_mut_ptr().cast::<u8>(),
            entries.len() * std::mem::size_of::<libc::pollfd>(),
        )
    };
    if guest.memory().read_exact(addr, bytes).is_err() {
        return Vec::new();
    }
    entries
}

fn connect_result_allows_peer_classification(result: &Result<i64, Error>) -> bool {
    match result {
        Ok(_) => true,
        Err(Error::Errno(errno)) => *errno == Errno::EINPROGRESS,
        Err(_) => false,
    }
}

const KERNEL_SIGSET_SIZE: usize = std::mem::size_of::<u64>();
const PSELECT6_INTERNAL_MAX_NFDS: i32 = (std::mem::size_of::<libc::c_ulong>() * 8) as i32;

#[derive(Clone, Copy)]
#[repr(C)]
struct Pselect6SigmaskArg {
    sigmask: usize,
    sigsetsize: usize,
}

fn pselect6_fd_set_len(nfds: i32) -> Result<usize, Errno> {
    let nfds = usize::try_from(nfds).map_err(|_| Errno::EINVAL)?;
    let bits_per_word = std::mem::size_of::<libc::c_ulong>() * 8;
    Ok(nfds.div_ceil(bits_per_word) * std::mem::size_of::<libc::c_ulong>())
}

fn pselect6_probe_result(result: Result<i64, Errno>) -> Result<i64, Errno> {
    match result {
        // The injected syscall runs outside the guest's original restart frame.
        // Do not expose this kernel-internal restart instruction at the
        // rewritten pselect6 call site.
        Err(Errno::ERESTARTSYS) => Err(Errno::EINTR),
        result => result,
    }
}

fn read_pselect6_fd_set<T, G>(
    guest: &mut G,
    address: Option<AddrMut<'_, libc::fd_set>>,
    len: usize,
) -> Result<Option<Vec<u8>>, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(address) = address else {
        return Ok(None);
    };
    let mut bytes = vec![0; len];
    if len != 0 {
        guest
            .memory()
            .read_exact(address.cast(), &mut bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(Some(bytes))
}

fn write_pselect6_fd_set<T, G>(
    guest: &mut G,
    address: Option<AddrMut<'_, libc::fd_set>>,
    bytes: &Option<Vec<u8>>,
) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if let (Some(address), Some(bytes)) = (address, bytes)
        && !bytes.is_empty()
    {
        guest
            .memory()
            .write_exact(address.cast(), bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

/// Read the byte of a guest select bitmap that holds `fd`'s bit.
fn read_select_byte<T, G>(
    guest: &mut G,
    fds: usize,
    fd: i32,
) -> Result<(AddrMut<'static, u8>, u8), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let address = fds
        .checked_add((fd / 8) as usize)
        .and_then(AddrMut::<u8>::from_raw)
        .ok_or(Errno::EFAULT)?;
    let mut byte = [0u8];
    guest
        .memory()
        .read_exact(Addr::from(address), &mut byte)
        .map_err(|_| Errno::EFAULT)?;
    Ok((address, byte[0]))
}

/// Set the given descriptors' bits in a guest select read set. Only the
/// bytes holding those bits are touched; each lies below the fd-table size
/// that bounds Linux's own copy, because the descriptor is open.
fn set_select_read_bits<T, G>(
    guest: &mut G,
    readfds: Option<usize>,
    fds: &[i32],
) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(readfds) = readfds else {
        return Ok(());
    };
    for &fd in fds {
        let (address, byte) = read_select_byte(guest, readfds, fd)?;
        guest
            .memory()
            .write_exact(address, &[byte | (1u8 << (fd % 8))])
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

fn copy_pselect6_fd_set<T, G>(
    guest: &mut G,
    source: Option<AddrMut<'_, libc::fd_set>>,
    destination: Option<AddrMut<'_, libc::fd_set>>,
    len: usize,
) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if let (Some(source), Some(destination)) = (source, destination)
        && len != 0
    {
        let mut bytes = vec![0; len];
        guest
            .memory()
            .read_exact(source.cast(), &mut bytes)
            .map_err(|_| Errno::EFAULT)?;
        guest
            .memory()
            .write_exact(destination.cast(), &bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

/// One select or pselect6 probe whose fd sets are wider than the `fd_set`
/// slots the retry loop reserves on the guest stack (nfds above FD_SETSIZE).
/// The guest-stack scratch cannot grow to nfds, so the probe's sets live in
/// an anonymous mapping injected around this single probe: the original sets
/// are written in, the probe runs, the kernel's result sets are read back,
/// and the mapping is removed, all inside the caller's scheduler turn, so no
/// other guest thread runs while it exists. The guest's own sets are left
/// for the caller to write at completion, as with the stack scratch. Returns
/// the probe's result and, when it succeeded, the result sets in
/// `[read, write, except]` order.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3229): select and pselect6 above FD_SETSIZE probe through a per-probe mapping.
async fn inject_wide_select_probe<T, G, C>(
    guest: &mut G,
    originals: [&Option<Vec<u8>>; 3],
    len: usize,
    probe: impl FnOnce([Option<AddrMut<'static, libc::fd_set>>; 3]) -> C,
) -> Result<(Result<i64, Errno>, [Option<Vec<u8>>; 3]), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
    C: SyscallInfo,
{
    let present = originals.iter().filter(|set| set.is_some()).count();
    let mapping_len = present.checked_mul(len).ok_or(Errno::ENOMEM)?;
    if mapping_len == 0 {
        return Ok((guest.inject(probe([None; 3])).await, [None, None, None]));
    }
    let mapped = guest
        .inject_with_retry(Syscall::Mmap(
            syscalls::Mmap::new()
                .with_addr(None)
                .with_len(mapping_len)
                .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                .with_fd(-1)
                .with_offset(0),
        ))
        .await?;
    let base = usize::try_from(mapped).map_err(|_| Errno::EFAULT)?;
    let mut slots = [None; 3];
    let mut next = base;
    for (slot, original) in slots.iter_mut().zip(originals) {
        if original.is_some() {
            *slot = AddrMut::<libc::fd_set>::from_raw(next);
            next += len;
        }
    }
    // The slots are held by value across the probe: `AddrMut` is Send but
    // not Sync, so a borrow of them would make this future non-Send.
    let [readfds, writefds, exceptfds] = slots;
    let written = write_pselect6_fd_set(guest, readfds, originals[0])
        .and_then(|()| write_pselect6_fd_set(guest, writefds, originals[1]))
        .and_then(|()| write_pselect6_fd_set(guest, exceptfds, originals[2]));
    let outcome = match written {
        Ok(()) => {
            let result = guest.inject(probe(slots)).await;
            if result.is_ok() {
                read_pselect6_fd_set(guest, readfds, len).and_then(|read| {
                    let write = read_pselect6_fd_set(guest, writefds, len)?;
                    let except = read_pselect6_fd_set(guest, exceptfds, len)?;
                    Ok((result, [read, write, except]))
                })
            } else {
                Ok((result, [None, None, None]))
            }
        }
        Err(error) => Err(error),
    };
    guest
        .inject_with_retry(Syscall::Munmap(
            syscalls::Munmap::new()
                .with_addr(Addr::from_raw(base))
                .with_len(mapping_len),
        ))
        .await?;
    outcome
}

fn ppoll_timeout_duration(timeout: Timespec) -> Result<Duration, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let nanoseconds = u32::try_from(timeout.tv_nsec).map_err(|_| Errno::EINVAL)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(seconds, nanoseconds))
}

fn select_timeout_duration(timeout: libc::timeval) -> Result<Duration, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let microseconds = u64::try_from(timeout.tv_usec).map_err(|_| Errno::EINVAL)?;
    // Linux rejects select timeouts whose microsecond field is out of range.
    if microseconds >= 1_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(seconds, (microseconds * 1_000) as u32))
}

fn timespec_from_duration(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

const SCM_TIMESTAMP_OLD: libc::c_int = 29;
const SCM_TIMESTAMPNS_OLD: libc::c_int = 35;
const SCM_TIMESTAMPING_OLD: libc::c_int = 37;
const SCM_TIMESTAMP_NEW: libc::c_int = 63;
const SCM_TIMESTAMPNS_NEW: libc::c_int = 64;
const SCM_TIMESTAMPING_NEW: libc::c_int = 65;
const MAX_CONTROL_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum SocketTimestampKind {
    Timeval,
    Timespec,
    Timestamping,
}

#[derive(Clone, Copy)]
struct SocketTimestampMessage {
    data_offset: usize,
    available_data_len: usize,
    kind: SocketTimestampKind,
}

fn cmsg_align(length: usize) -> usize {
    let alignment = std::mem::size_of::<usize>();
    (length + alignment - 1) & !(alignment - 1)
}

fn read_control_value<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: the length check guarantees a complete T, and read_unaligned
    // permits the control buffer's byte alignment.
    Some(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

fn write_control_value<T: Copy>(bytes: &mut [u8], value: T) -> bool {
    if bytes.len() < std::mem::size_of::<T>() {
        return false;
    }
    // SAFETY: the length check guarantees room for T, and write_unaligned
    // permits the control buffer's byte alignment.
    unsafe { bytes.as_mut_ptr().cast::<T>().write_unaligned(value) };
    true
}

fn write_control_prefix<T: Copy>(bytes: &mut [u8], value: T) -> usize {
    let value_len = std::mem::size_of::<T>();
    let write_len = bytes.len().min(value_len);
    // SAFETY: `value` is alive for this copy and the resulting byte view has
    // exactly its initialized object representation.
    let value_bytes =
        unsafe { std::slice::from_raw_parts((&value as *const T).cast::<u8>(), value_len) };
    bytes[..write_len].copy_from_slice(&value_bytes[..write_len]);
    write_len
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3229): timerfds sent over SCM_RIGHTS go to the kernel.
/// The descriptors named by the SCM_RIGHTS messages of a control buffer that
/// is about to be sent. The walk follows net/core/scm.c (__scm_send): each
/// header must hold at least itself and fit in the buffer, the payload is the
/// rest of the declared length, and the next header starts at the aligned end.
/// A malformed header ends the walk; the kernel then fails the send with
/// EINVAL.
fn scm_rights_fds(control: &[u8]) -> Vec<i32> {
    let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
    let mut fds = Vec::new();
    let mut offset = 0usize;
    while let Some(header_bytes) = control.get(offset..) {
        let Some(header) = read_control_value::<libc::cmsghdr>(header_bytes) else {
            break;
        };
        if header.cmsg_len < header_len {
            break;
        }
        let Some(end) = offset.checked_add(header.cmsg_len) else {
            break;
        };
        if end > control.len() {
            break;
        }
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            fds.extend(
                control[offset + header_len..end]
                    .chunks_exact(std::mem::size_of::<i32>())
                    .filter_map(|chunk| chunk.try_into().ok().map(i32::from_ne_bytes)),
            );
        }
        let Some(next) = offset.checked_add(cmsg_align(header.cmsg_len)) else {
            break;
        };
        if next <= offset {
            break;
        }
        offset = next;
    }
    fds
}

// AUTONOMOUS-BOT-IMPLEMENTED
/// The (address, length) of a message header's control buffer, if it has one.
fn message_control(message: &libc::msghdr) -> Option<(usize, usize)> {
    (!message.msg_control.is_null() && message.msg_controllen != 0)
        .then(|| (message.msg_control as usize, message.msg_controllen))
}

fn socket_timestamp_messages(control: &[u8]) -> Vec<SocketTimestampMessage> {
    let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
    let mut messages = Vec::new();
    let mut offset = 0usize;

    while let Some(header_bytes) = control.get(offset..) {
        let Some(header) = read_control_value::<libc::cmsghdr>(header_bytes) else {
            break;
        };
        if header.cmsg_len < header_len {
            break;
        }
        let Some(end) = offset.checked_add(header.cmsg_len) else {
            break;
        };

        if header.cmsg_level == libc::SOL_SOCKET {
            let data_offset = offset + header_len;
            let declared_data_len = header.cmsg_len - header_len;
            let available_data_len = control
                .len()
                .saturating_sub(data_offset)
                .min(declared_data_len);
            let kind = match header.cmsg_type {
                SCM_TIMESTAMP_OLD | SCM_TIMESTAMP_NEW => Some(SocketTimestampKind::Timeval),
                SCM_TIMESTAMPNS_OLD | SCM_TIMESTAMPNS_NEW => Some(SocketTimestampKind::Timespec),
                SCM_TIMESTAMPING_OLD | SCM_TIMESTAMPING_NEW => {
                    Some(SocketTimestampKind::Timestamping)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                messages.push(SocketTimestampMessage {
                    data_offset,
                    available_data_len,
                    kind,
                });
            }
        }

        if end > control.len() {
            break;
        }

        let step = cmsg_align(header.cmsg_len);
        let Some(next) = offset.checked_add(step) else {
            break;
        };
        if next <= offset {
            break;
        }
        offset = next;
    }
    messages
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-901)
fn canonicalize_socket_timestamps(control: &mut [u8], now: LogicalTime) -> usize {
    let messages = socket_timestamp_messages(control);
    let timespec = libc::timespec {
        tv_sec: now.as_secs() as libc::time_t,
        tv_nsec: now.subsec_nanos() as libc::c_long,
    };
    let timeval = libc::timeval {
        tv_sec: timespec.tv_sec,
        tv_usec: (timespec.tv_nsec / 1_000) as libc::suseconds_t,
    };

    for message in &messages {
        let available_end = message
            .data_offset
            .saturating_add(message.available_data_len)
            .min(control.len());
        let data = &mut control[message.data_offset..available_end];
        match message.kind {
            SocketTimestampKind::Timeval => {
                write_control_prefix(data, timeval);
            }
            SocketTimestampKind::Timespec => {
                write_control_prefix(data, timespec);
            }
            SocketTimestampKind::Timestamping => {
                let size = std::mem::size_of::<libc::timespec>();
                let zero = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                for slot in 0..3 {
                    let start = slot * size;
                    if start >= data.len() {
                        break;
                    }
                    let end = (start + size).min(data.len());
                    let slot_bytes = &mut data[start..end];
                    if slot_bytes.len() != size {
                        slot_bytes.fill(0);
                        continue;
                    }
                    let original = read_control_value::<libc::timespec>(slot_bytes)
                        .expect("complete timestamping slot");
                    let replacement = if original.tv_sec == 0 && original.tv_nsec == 0 {
                        zero
                    } else {
                        timespec
                    };
                    let _ = write_control_value(slot_bytes, replacement);
                }
            }
        }
    }
    messages.len()
}

fn sanitize_ppoll_signal_mask(mask: u64) -> u64 {
    let signal_bit = (reverie::PERF_EVENT_SIGNAL as usize) - 1;
    mask & !(1_u64 << signal_bit)
}

fn ppoll_uses_kernel_wait(
    sequentialize_threads: bool,
    recordreplay_modes: bool,
    has_signal_mask: bool,
) -> bool {
    !sequentialize_threads || (recordreplay_modes && !has_signal_mask)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// poll syscall (MAYHANG)
    // TODO-HUMAN-REVIEW(PR-1023): Review zero-timeout poll scheduling across backends.
    pub async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,

        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        self.hand_polled_epoll_timerfds_to_kernel(
            guest,
            call.fds().map(|a| a.as_raw()),
            call.nfds(),
        )
        .await?;
        if self.cfg.sequentialize_threads && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            let yield_to_peer =
                self.cfg.discover_live_file_metadata && guest.thread_state().has_loopback_peer();
            resource_request(
                guest,
                zero_timeout_poll_request(guest.thread_state().dettid, yield_to_peer),
            )
            .await;
            if self.cfg.recordreplay_modes {
                Ok(self.record_or_replay(guest, call).await?)
            } else {
                let host = guest.inject(call).await?;
                self.merge_poll_timerfds(guest, call.fds().map(|a| a.as_raw()), call.nfds(), host)
                    .await
            }
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // In replay mode, we cannot assume the existence of FILES during replay.
            // Thus we must record the poll and replay it from the trace.
            Ok(self.handle_external_poll(guest, call).await?)
        } else {
            // TODO:
            // if is-external-poll { self.handle_external_poll(guest, call) }
            self.handle_internal_poll(guest, call).await
        }
    }

    /// Record or replay a raw `select` or `pselect6` the way `poll` is: a zero
    /// timeout cannot block and takes an ordinary scheduler turn; any other call
    /// may wait in the kernel, so it runs as a blocking external operation. The
    /// recorder captures the descriptor sets and remaining time, so replay never
    /// asks the kernel about descriptors it did not recreate.
    async fn record_or_replay_select_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        zero_timeout: bool,
    ) -> Result<i64, Error> {
        if zero_timeout {
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else {
            self.record_or_replay_blocking(guest, call).await
        }
    }

    /// pselect6 syscall (MAYHANG).
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
    pub async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes {
            let zero_timeout = call.timeout().is_some_and(|timeout| {
                guest
                    .memory()
                    .read_value(timeout)
                    .is_ok_and(|timeout: Timespec| timeout.tv_sec == 0 && timeout.tv_nsec == 0)
            });
            return self
                .record_or_replay_select_family(guest, Syscall::Pselect6(call), zero_timeout)
                .await;
        }
        if !self.cfg.sequentialize_threads {
            return Ok(guest.inject(call).await?);
        }

        if call.nfds() < 0 {
            return Ok(guest.inject(call).await?);
        }
        self.hand_selected_epoll_timerfds_to_kernel(
            guest,
            call.nfds(),
            [
                call.readfds().map(|set| set.as_raw()),
                call.writefds().map(|set| set.as_raw()),
                call.exceptfds().map(|set| set.as_raw()),
            ],
        )
        .await?;

        // Linux copies pselect6's outer { sigmask, sigsetsize } wrapper before
        // validating the timeout. Copy only the wrapper here; validation of the
        // pointed-to signal mask remains below, after timeout validation.
        let sigmask_argument = match call.sigmask() {
            Some(argument) => {
                // A split read can fall back to PTRACE_PEEKDATA for the final
                // word, bypassing PROT_NONE or reporting EIO for an unmapped
                // page. Have Linux validate both wrapper words first. It copies
                // this wrapper before rejecting a malformed timeout, without
                // reading the inner mask, changing it, waiting, or writing output.
                let mut stack = guest.stack().await;
                let validation_timeout = stack.reserve::<Timespec>();
                let _guard = stack.commit()?;
                guest.memory().write_value(
                    validation_timeout,
                    &Timespec {
                        tv_sec: 0,
                        tv_nsec: 1_000_000_000,
                    },
                )?;
                let validation = syscalls::Pselect6::new()
                    .with_nfds(0)
                    .with_readfds(None)
                    .with_writefds(None)
                    .with_exceptfds(None)
                    .with_timeout(Some(validation_timeout))
                    .with_sigmask(Some(argument));
                match guest.inject(validation).await {
                    Err(Errno::EINVAL) => {}
                    Err(errno) => return Err(errno.into()),
                    // Success would mean the backend did not validate the probe.
                    Ok(_) => return Err(Errno::EIO.into()),
                }
                let argument: Pselect6SigmaskArg = guest.memory().read_value(argument.cast())?;
                Some(argument)
            }
            None => None,
        };
        let raw_timeout = match call.timeout() {
            Some(timeout) => {
                let timeout: Timespec = guest.memory().read_value(timeout)?;
                Some(timeout)
            }
            None => None,
        };
        let timeout = raw_timeout.map(ppoll_timeout_duration).transpose()?;
        if timeout == Some(Duration::ZERO) {
            let readfds = call.readfds().map(|addr| addr.as_raw());
            return self
                .select_poll_with_timerfds(guest, call.nfds(), readfds, call)
                .await;
        }

        // Linux clamps raw fd-set copies to the process fd table's current max_fds.
        // Its initial table holds one machine word; larger nfds values can therefore
        // require fewer bytes than a userspace calculation predicts. Keep those calls
        // under kernel ownership rather than over-reading the guest bitmap, unless
        // the read set names a virtual timerfd the kernel could never report.
        if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS
            && !self.wide_select_needs_detcore(
                guest,
                call.nfds(),
                call.readfds().map(|addr| addr.as_raw()),
            )
        {
            return self
                .record_or_replay_blocking(guest, Syscall::Pselect6(call))
                .await;
        }

        // Linux wraps pselect6's temporary mask in { pointer, size }. Glibc supplies
        // the wrapper even when the inner pointer is null. A real mask must stay in
        // effect for the whole wait so an unblocked signal (make's jobserver unblocks
        // SIGCHLD) can interrupt it. Previously that forced the external-blocking path,
        // whose completion timing is host-decided and is a source of `make -jN`
        // execution-log divergence. With SIGCHLD admission now deterministic (scheduler
        // `sigchld_deferred`/`sigchld_ready`), honor the mask on each deterministic poll
        // probe instead: a pending unblocked signal is observed at a scheduler-decided
        // probe point rather than at host signal-arrival time.
        let sigmask = if let Some(argument) = sigmask_argument {
            if argument.sigmask != 0 {
                if argument.sigsetsize != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let mask_addr =
                    Addr::<libc::sigset_t>::from_raw(argument.sigmask).ok_or(Errno::EFAULT)?;
                let mask = read_kernel_sigset(guest, mask_addr).await?;
                Some(sanitize_ppoll_signal_mask(mask))
            } else {
                None
            }
        } else {
            None
        };
        // The inner mask was snapshotted above. Do not let later guest mutations of the
        // outer wrapper change the meaning of a retry probe.
        let call = call.with_sigmask(None);

        self.handle_internal_pselect6(guest, call, timeout, sigmask)
            .await
    }

    async fn handle_internal_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        timeout: Option<Duration>,
        sigmask: Option<u64>,
    ) -> Result<i64, Error> {
        let len = pselect6_fd_set_len(call.nfds())?;
        let deadline = match timeout {
            Some(timeout) => Some(thread_observe_time(guest).await + timeout),
            None => None,
        };
        let original_readfds = match read_pselect6_fd_set(guest, call.readfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_writefds = match read_pselect6_fd_set(guest, call.writefds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_exceptfds = match read_pselect6_fd_set(guest, call.exceptfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };

        // Sets wider than an fd_set do not fit the stack scratch; each probe
        // then maps its own (`inject_wide_select_probe`).
        let wide = len > std::mem::size_of::<libc::fd_set>();
        let mut stack = guest.stack().await;
        let readfds = call
            .readfds()
            .filter(|_| !wide)
            .map(|_| stack.reserve::<libc::fd_set>());
        let writefds = call
            .writefds()
            .filter(|_| !wide)
            .map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = call
            .exceptfds()
            .filter(|_| !wide)
            .map(|_| stack.reserve::<libc::fd_set>());
        // pselect6's timeout is a writable in-out kernel timespec, so the probe
        // needs a mutable scratch cell (re-zeroed each iteration below to keep
        // every probe a non-blocking poll).
        let probe_timeout = stack.reserve::<Timespec>();
        // Carry the temporary signal mask on every zero-timeout probe so the kernel
        // applies it atomically: a pending, mask-unblocked signal makes the probe return
        // EINTR at a deterministic scheduler point. The probe's wrapper points at scratch
        // memory the guard keeps alive across each injection.
        let probe_sigmask = sigmask.map(|mask| {
            let sigset = stack.push(mask);
            stack
                .push(Pselect6SigmaskArg {
                    sigmask: sigset.as_raw(),
                    sigsetsize: KERNEL_SIGSET_SIZE,
                })
                .cast()
        });
        let _guard = stack.commit()?;
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout))
            .with_sigmask(probe_sigmask);

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi("pselect6");
        // Keep the request metadata accurate, but do not make it eligible for
        // the scheduler's ERESTARTSYS wakeup. A cross-task signal must first be
        // checked against pselect6's snapshotted temporary mask and disposition.
        resources.set_signal_interrupt_errno(Errno::EINTR);

        loop {
            if matches!(
                resource_request(guest, resources.clone()).await,
                ResumeStatus::Signaled(_)
            ) {
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(Errno::EINTR.into());
            }
            guest.memory().write_value(
                probe_timeout,
                &Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
            )?;
            let (result, wide_sets) = if wide {
                match inject_wide_select_probe(
                    guest,
                    [&original_readfds, &original_writefds, &original_exceptfds],
                    len,
                    move |[readfds, writefds, exceptfds]| {
                        probe
                            .with_readfds(readfds)
                            .with_writefds(writefds)
                            .with_exceptfds(exceptfds)
                    },
                )
                .await
                {
                    Ok((result, sets)) => (pselect6_probe_result(result), Some(sets)),
                    Err(error) => {
                        self.write_pselect6_remaining(guest, call, deadline).await?;
                        return Err(error);
                    }
                }
            } else {
                write_pselect6_fd_set(guest, probe.readfds(), &original_readfds)?;
                write_pselect6_fd_set(guest, probe.writefds(), &original_writefds)?;
                write_pselect6_fd_set(guest, probe.exceptfds(), &original_exceptfds)?;
                (pselect6_probe_result(guest.inject(probe).await), None)
            };
            // Virtual timerfds are never readable on the host; add the ready
            // ones to a successful probe's read set and count.
            let timer_ready = match result {
                Ok(_) => {
                    self.select_timerfd_scan(guest, call.nfds(), &original_readfds)
                        .await?
                }
                Err(_) => Vec::new(),
            };
            if result != Ok(0) || !timer_ready.is_empty() {
                let copy_result = match result {
                    Ok(host) => self
                        .copy_pselect6_results(guest, probe, call, len, &wide_sets)
                        .and_then(|()| {
                            set_select_read_bits(
                                guest,
                                call.readfds().map(|addr| addr.as_raw()),
                                &timer_ready,
                            )
                        })
                        .map(|()| host + timer_ready.len() as i64),
                    Err(_) => Ok(0),
                };
                self.write_pselect6_remaining(guest, call, deadline).await?;
                let count = copy_result?;
                return result.map(|_| count).map_err(Into::into);
            }
            resources.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                let copy_result = self.copy_pselect6_results(guest, probe, call, len, &wide_sets);
                self.write_pselect6_remaining(guest, call, Some(deadline))
                    .await?;
                copy_result?;
                return Ok(0);
            }
            trace!(
                "Retry #{} for syscall due to result Ok(0): {}",
                resources.poll_attempt,
                probe.display(&guest.memory())
            );
            record_retry_event(guest, probe).await;
        }
    }

    /// Copy a probe's result sets to the guest's: from the stack scratch, or
    /// from the sets a wide probe read back before unmapping its own.
    fn copy_pselect6_results<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::Pselect6,
        call: syscalls::Pselect6,
        len: usize,
        wide_sets: &Option<[Option<Vec<u8>>; 3]>,
    ) -> Result<(), Error> {
        if let Some([readfds, writefds, exceptfds]) = wide_sets {
            write_pselect6_fd_set(guest, call.readfds(), readfds)?;
            write_pselect6_fd_set(guest, call.writefds(), writefds)?;
            return write_pselect6_fd_set(guest, call.exceptfds(), exceptfds);
        }
        copy_pselect6_fd_set(guest, probe.readfds(), call.readfds(), len)?;
        copy_pselect6_fd_set(guest, probe.writefds(), call.writefds(), len)?;
        copy_pselect6_fd_set(guest, probe.exceptfds(), call.exceptfds(), len)
    }

    async fn write_pselect6_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        deadline: Option<LogicalTime>,
    ) -> Result<(), Error> {
        if let (Some(timeout), Some(deadline)) = (call.timeout(), deadline) {
            let now = thread_observe_time(guest).await;
            let remaining = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = Timespec {
                tv_sec: (remaining / 1_000_000_000) as libc::time_t,
                tv_nsec: (remaining % 1_000_000_000) as libc::c_long,
            };
            // pselect6's timeout is a writable in-out kernel timespec; reverie-syscalls
            // now types it as `AddrMut<Timespec>`, so the remaining time can be written
            // back directly without an unsafe pointer cast.
            if let Err(error) = guest.memory().write_value(timeout, &remaining) {
                // Linux preserves the pselect6 result when remaining-time copyout faults.
                trace!(?error, "ignoring pselect6 timeout writeback failure");
            }
        }
        Ok(())
    }

    /// select syscall (MAYHANG).
    ///
    /// `select` is the classic `timeval` sibling of `pselect6` (which is already
    /// Determinized). It reuses the pselect6 fd-set scratch machinery, but takes
    /// a `struct timeval` timeout (which Linux updates in place with the time not
    /// slept) and carries no signal mask.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#800): Review select determinization mirroring pselect6.
    pub async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
    ) -> Result<i64, Error> {
        if self.cfg.recordreplay_modes {
            let zero_timeout = call.timeout().is_some_and(|timeout| {
                guest
                    .memory()
                    .read_value(timeout)
                    .is_ok_and(|timeout: libc::timeval| timeout.tv_sec == 0 && timeout.tv_usec == 0)
            });
            return self
                .record_or_replay_select_family(guest, Syscall::Select(call), zero_timeout)
                .await;
        }
        if !self.cfg.sequentialize_threads {
            return Ok(guest.inject(call).await?);
        }

        if call.nfds() < 0 {
            return Ok(guest.inject(call).await?);
        }
        self.hand_selected_epoll_timerfds_to_kernel(
            guest,
            call.nfds(),
            [
                call.readfds().map(|set| set.as_raw()),
                call.writefds().map(|set| set.as_raw()),
                call.exceptfds().map(|set| set.as_raw()),
            ],
        )
        .await?;

        let raw_timeout = match call.timeout() {
            Some(timeout) => {
                let timeout: libc::timeval = guest.memory().read_value(timeout)?;
                Some(timeout)
            }
            None => None,
        };
        if matches!(raw_timeout, Some(timeout) if timeout.tv_sec == 0 && timeout.tv_usec == 0) {
            // A zero timeout is a pure non-blocking poll; the kernel can service it directly.
            let readfds = call.readfds().map(|addr| addr.as_raw());
            return self
                .select_poll_with_timerfds(guest, call.nfds(), readfds, call)
                .await;
        }

        // Mirror pselect6: keep large fd tables under kernel ownership rather than
        // over-reading the guest bitmap (Linux clamps raw fd-set copies to max_fds).
        if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS
            && !self.wide_select_needs_detcore(
                guest,
                call.nfds(),
                call.readfds().map(|addr| addr.as_raw()),
            )
        {
            return self
                .record_or_replay_blocking(guest, Syscall::Select(call))
                .await;
        }

        let timeout = raw_timeout.map(select_timeout_duration).transpose()?;
        self.handle_internal_select(guest, call, timeout).await
    }

    async fn handle_internal_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        let len = pselect6_fd_set_len(call.nfds())?;
        let deadline = match timeout {
            Some(timeout) => Some(thread_observe_time(guest).await + timeout),
            None => None,
        };
        let original_readfds = match read_pselect6_fd_set(guest, call.readfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_writefds = match read_pselect6_fd_set(guest, call.writefds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };
        let original_exceptfds = match read_pselect6_fd_set(guest, call.exceptfds(), len) {
            Ok(value) => value,
            Err(error) => {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(error);
            }
        };

        // Sets wider than an fd_set do not fit the stack scratch; each probe
        // then maps its own (`inject_wide_select_probe`).
        let wide = len > std::mem::size_of::<libc::fd_set>();
        let mut stack = guest.stack().await;
        let readfds = call
            .readfds()
            .filter(|_| !wide)
            .map(|_| stack.reserve::<libc::fd_set>());
        let writefds = call
            .writefds()
            .filter(|_| !wide)
            .map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = call
            .exceptfds()
            .filter(|_| !wide)
            .map(|_| stack.reserve::<libc::fd_set>());
        // select modifies its timeout in place, so the probe timeout must be a
        // writable scratch cell. It is re-zeroed each iteration to keep every
        // probe a non-blocking poll (a NULL timeout would block indefinitely).
        let probe_timeout = stack.reserve::<libc::timeval>();
        let _guard = stack.commit()?;
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout));

        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::InternalIOPolling, Permission::W);
        resources.fyi("select");
        // EINTR records select's interruption result. The scheduler only wakes
        // ERESTARTSYS requests: blocked and ignored signals still need a target-
        // side disposition check before this Signaled path can be used.
        resources.set_signal_interrupt_errno(Errno::EINTR);

        loop {
            if matches!(
                resource_request(guest, resources.clone()).await,
                ResumeStatus::Signaled(_)
            ) {
                self.write_select_remaining(guest, call, deadline).await?;
                return Err(Errno::EINTR.into());
            }
            guest.memory().write_value(
                probe_timeout,
                &libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
            )?;
            let (result, wide_sets) = if wide {
                match inject_wide_select_probe(
                    guest,
                    [&original_readfds, &original_writefds, &original_exceptfds],
                    len,
                    move |[readfds, writefds, exceptfds]| {
                        probe
                            .with_readfds(readfds)
                            .with_writefds(writefds)
                            .with_exceptfds(exceptfds)
                    },
                )
                .await
                {
                    Ok((result, sets)) => (result, Some(sets)),
                    Err(error) => {
                        self.write_select_remaining(guest, call, deadline).await?;
                        return Err(error);
                    }
                }
            } else {
                write_pselect6_fd_set(guest, probe.readfds(), &original_readfds)?;
                write_pselect6_fd_set(guest, probe.writefds(), &original_writefds)?;
                write_pselect6_fd_set(guest, probe.exceptfds(), &original_exceptfds)?;
                (guest.inject(probe).await, None)
            };
            // Virtual timerfds are never readable on the host; add the ready
            // ones to a successful probe's read set and count.
            let timer_ready = match result {
                Ok(_) => {
                    self.select_timerfd_scan(guest, call.nfds(), &original_readfds)
                        .await?
                }
                Err(_) => Vec::new(),
            };
            if result != Ok(0) || !timer_ready.is_empty() {
                let copy_result = match result {
                    Ok(host) => self
                        .copy_select_results(guest, probe, call, len, &wide_sets)
                        .and_then(|()| {
                            set_select_read_bits(
                                guest,
                                call.readfds().map(|addr| addr.as_raw()),
                                &timer_ready,
                            )
                        })
                        .map(|()| host + timer_ready.len() as i64),
                    Err(_) => Ok(0),
                };
                self.write_select_remaining(guest, call, deadline).await?;
                let count = copy_result?;
                return result.map(|_| count).map_err(Into::into);
            }
            resources.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                let copy_result = self.copy_select_results(guest, probe, call, len, &wide_sets);
                self.write_select_remaining(guest, call, Some(deadline))
                    .await?;
                copy_result?;
                return Ok(0);
            }
            trace!(
                "Retry #{} for syscall due to result Ok(0): {}",
                resources.poll_attempt,
                probe.display(&guest.memory())
            );
            record_retry_event(guest, probe).await;
        }
    }

    /// Copy a probe's result sets to the guest's: from the stack scratch, or
    /// from the sets a wide probe read back before unmapping its own.
    fn copy_select_results<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::Select,
        call: syscalls::Select,
        len: usize,
        wide_sets: &Option<[Option<Vec<u8>>; 3]>,
    ) -> Result<(), Error> {
        if let Some([readfds, writefds, exceptfds]) = wide_sets {
            write_pselect6_fd_set(guest, call.readfds(), readfds)?;
            write_pselect6_fd_set(guest, call.writefds(), writefds)?;
            return write_pselect6_fd_set(guest, call.exceptfds(), exceptfds);
        }
        copy_pselect6_fd_set(guest, probe.readfds(), call.readfds(), len)?;
        copy_pselect6_fd_set(guest, probe.writefds(), call.writefds(), len)?;
        copy_pselect6_fd_set(guest, probe.exceptfds(), call.exceptfds(), len)
    }

    async fn write_select_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
        deadline: Option<LogicalTime>,
    ) -> Result<(), Error> {
        if let (Some(timeout), Some(deadline)) = (call.timeout(), deadline) {
            let now = thread_observe_time(guest).await;
            let remaining = deadline.as_nanos().saturating_sub(now.as_nanos());
            let remaining = libc::timeval {
                tv_sec: (remaining / 1_000_000_000) as libc::time_t,
                tv_usec: ((remaining % 1_000_000_000) / 1_000) as libc::suseconds_t,
            };
            // select's timeout is a writable in-out kernel timeval reporting the
            // time not slept; derive it from deterministic virtual time.
            if let Err(error) = guest.memory().write_value(timeout, &remaining) {
                // Linux preserves the select result when remaining-time copyout faults.
                trace!(?error, "ignoring select timeout writeback failure");
            }
        }
        Ok(())
    }

    /// ppoll syscall (MAYHANG)
    // TODO-HUMAN-REVIEW(PR-273)
    pub async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<i64, Error> {
        self.hand_polled_epoll_timerfds_to_kernel(
            guest,
            call.fds().map(|a| a.as_raw()),
            call.nfds(),
        )
        .await?;
        let timeout_address = call.timeout();
        let timeout = match timeout_address {
            Some(timeout) => Some(ppoll_timeout_duration(guest.memory().read_value(timeout)?)?),
            None => None,
        };

        let result: Result<i64, Error> = if timeout == Some(Duration::ZERO) {
            if self.cfg.recordreplay_modes {
                resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            }
            let (probe, _probe_guard) = self.prepare_ppoll_probe(guest, call).await?;
            let result = if self.cfg.recordreplay_modes {
                Ok(self.record_or_replay(guest, probe).await?)
            } else {
                let host = guest.inject_with_retry(probe).await?;
                self.merge_poll_timerfds(guest, call.fds().map(|a| a.as_raw()), call.nfds(), host)
                    .await
            };
            // Linux does not write back an initially zero timeout. Besides matching the
            // kernel, omitting this write matters when the timeout aliases the pollfd array:
            // the injected probe may have just stored revents in those same bytes.
            result
        } else if ppoll_uses_kernel_wait(
            self.cfg.sequentialize_threads,
            self.cfg.recordreplay_modes,
            call.sigmask().is_some(),
        ) {
            // The kernel owns the blocking wait when threads are not sequentialized and for
            // unmasked record/replay calls. A masked sequentialized wait must use the probe
            // below so a call that would block keeps the strict fail-closed behavior.
            // Use scratch memory only for the signal mask so raw ppoll can still update the
            // guest timeout.
            let mut signal_mask_guard = None;
            let call = if let Some(signal_mask) = call.sigmask() {
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let signal_mask = read_kernel_sigset(guest, signal_mask).await?;
                let mut stack = guest.stack().await;
                let signal_mask = stack.push(sanitize_ppoll_signal_mask(signal_mask)).cast();
                signal_mask_guard = Some(stack.commit()?);
                call.with_sigmask(Some(signal_mask))
            } else {
                call
            };
            let result = Ok(self
                .record_or_replay_blocking(guest, Syscall::Ppoll(call))
                .await?);
            drop(signal_mask_guard);
            result
        } else {
            self.handle_internal_ppoll(guest, call, timeout).await
        };

        result
    }

    async fn prepare_ppoll_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<(syscalls::Ppoll, <G::Stack as Stack>::StackGuard), Error> {
        let signal_mask = match call.sigmask() {
            Some(signal_mask) => {
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                let signal_mask = read_kernel_sigset(guest, signal_mask).await?;
                Some(sanitize_ppoll_signal_mask(signal_mask))
            }
            None => None,
        };

        let mut stack = guest.stack().await;
        let timeout = stack.push(timespec_from_duration(Duration::ZERO));
        // The scratch stack guard outlives the injected syscall, so the pointee is writable.
        let timeout = unsafe { timeout.into_mut() };
        let mut probe = call.with_timeout(Some(timeout));
        if let Some(signal_mask) = signal_mask {
            probe = probe.with_sigmask(Some(stack.push(signal_mask).cast()));
        }
        let guard = stack.commit()?;
        Ok((probe, guard))
    }

    /// Handle a guest-internal `ppoll` using zero-time kernel probes.
    async fn handle_internal_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        debug_assert_ne!(timeout, Some(Duration::ZERO));
        let timeout_address = call.timeout();
        let started_at = if timeout.is_some() {
            Some(thread_observe_time(guest).await)
        } else {
            None
        };
        let deadline = match (timeout, started_at) {
            (Some(duration), Some(started_at)) => Some(started_at + duration),
            (None, None) => None,
            _ => unreachable!(),
        };
        let fds_raw = call.fds().map(|a| a.as_raw());
        let nfds = call.nfds();

        // A zero probe can honor a temporary signal mask atomically. Keeping that mask
        // active while parked would require scheduler-level pending-signal state, so fail
        // closed rather than letting a masked signal interrupt a simulated wait. A ready
        // virtual timerfd means the call does not wait, so the probe alone answers it.
        let timer_ready = !self
            .poll_timerfd_scan(guest, fds_raw, nfds)
            .await?
            .is_empty();
        let result = if call.sigmask().is_some() || timer_ready {
            let (probe, _probe_guard) = self.prepare_ppoll_probe(guest, call).await?;
            let result = if self.cfg.recordreplay_modes {
                self.record_or_replay(guest, probe).await
            } else {
                guest.inject_with_retry(probe).await
            };
            let result = match result {
                Ok(host) => {
                    self.merge_timer_wait_set(
                        guest,
                        TimerWaitSet::Poll { fds_raw, nfds },
                        TimerWaitRound::Poll,
                        host,
                    )
                    .await
                }
                Err(errno) => Err(errno.into()),
            };
            if call.sigmask().is_some() && matches!(result, Ok(0)) {
                return Err(Errno::ENOSYS.into());
            }
            result
        } else if self.poll_has_timerfds(guest, fds_raw, nfds) {
            self.wait_with_timerfds(
                guest,
                call,
                TimerWaitSet::Poll { fds_raw, nfds },
                deadline,
                "ppoll",
            )
            .await
        } else {
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("ppoll");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await
        };
        if let (Some(timeout_address), Some(timeout), Some(started_at)) =
            (timeout_address, timeout, started_at)
        {
            self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                .await?;
        }
        result
    }

    async fn write_ppoll_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        timeout_address: AddrMut<'_, Timespec>,
        timeout: Duration,
        started_at: LogicalTime,
    ) -> Result<(), Error> {
        let now = thread_observe_time(guest).await;
        let elapsed = Duration::from_nanos(now.as_nanos().saturating_sub(started_at.as_nanos()));
        let remaining = timeout.saturating_sub(elapsed);
        if let Err(error) = guest
            .memory()
            .write_value(timeout_address, &timespec_from_duration(remaining))
        {
            // Linux preserves the ppoll result when remaining-time copyout faults.
            trace!(?error, "ignoring ppoll timeout writeback failure");
        }
        Ok(())
    }

    /// Handle a guest-internal poll call that can be fully determinized.
    // TODO-HUMAN-REVIEW(PR-1052): Review scheduler fairness for zero-timeout poll.
    pub async fn handle_internal_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            // A nonblocking poll can still be the synchronization point in a
            // userspace polling loop. Yield once before probing so backends
            // without PMU preemption cannot let that loop starve its producer.
            let yield_to_peer =
                self.cfg.discover_live_file_metadata && guest.thread_state().has_loopback_peer();
            resource_request(
                guest,
                zero_timeout_poll_request(guest.thread_state().dettid, yield_to_peer),
            )
            .await;
            let host = guest.inject(call).await?; // Already non-blocking.
            let set = TimerWaitSet::Poll {
                fds_raw: call.fds().map(|a| a.as_raw()),
                nfds: call.nfds(),
            };
            self.merge_timer_wait_set(guest, set, TimerWaitRound::Poll, host)
                .await
        } else {
            let fds_raw = call.fds().map(|a| a.as_raw());
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            if self.poll_has_timerfds(guest, fds_raw, call.nfds()) {
                let set = TimerWaitSet::Poll {
                    fds_raw,
                    nfds: call.nfds(),
                };
                return self
                    .wait_with_timerfds(guest, call, set, maybe_timeout_ns, "poll")
                    .await;
            }
            let mut rsrc = Resources::new(guest.thread_state().dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("poll");
            retry_nonblocking_syscall_with_timeout(guest, call, rsrc, maybe_timeout_ns).await
        }
    }

    /// The virtual timerfds named in a guest pollfd array, with each entry's
    /// requested events. An unreadable or oversized array yields nothing and
    /// is left to the kernel, which reports its own error.
    fn poll_timerfds<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds_raw: Option<usize>,
        nfds: u64,
    ) -> Vec<(i32, libc::c_short, crate::fd::TimerFdState)> {
        let mut timers = Vec::new();
        if !self.virtual_timerfds() {
            // No descriptor can carry virtual timer state; skip reading the array.
            return timers;
        }
        for entry in read_guest_pollfds(guest, fds_raw, nfds) {
            if entry.fd < 0 {
                continue;
            }
            let state = guest
                .thread_state()
                .with_detfd(entry.fd, |detfd| detfd.timerfd_state())
                .ok()
                .flatten();
            if let Some(state) = state {
                timers.push((entry.fd, entry.events, state));
            }
        }
        timers
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
    /// Whether `fd` is a timerfd whose timer was handed to the kernel. Its
    /// reads, settime and gettime then go to the kernel, as they do without
    /// virtual timerfds.
    pub(crate) fn timerfd_kernel_backed<G: Guest<Self>>(&self, guest: &mut G, fd: i32) -> bool {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.timerfd_kernel_backed())
            .unwrap_or(false)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): timerfds received over SCM_RIGHTS go to the
    // kernel.
    /// Whether timerfd_settime and timerfd_gettime on `fd` go to the kernel:
    /// the timer was handed to the kernel, or Detcore never saw the
    /// descriptor created, as for one received over SCM_RIGHTS. Base main
    /// sends every timerfd call to the kernel, so such a descriptor behaves
    /// as it does there, including the kernel's EBADF for a closed number and
    /// EINVAL for one that is not a timerfd. A sender hands its virtual
    /// timerfds to the kernel before the send (`hand_sent_timerfds_to_kernel`),
    /// so both ends then use the kernel's timer.
    pub(crate) fn timerfd_control_on_kernel<G: Guest<Self>>(&self, guest: &mut G, fd: i32) -> bool {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.timerfd_kernel_backed())
            .unwrap_or(true)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
    /// Hand a virtual timerfd to the kernel, where base main keeps every
    /// timerfd. The host vessel is armed with the virtual timer's remaining
    /// time and interval, and the timer is marked kernel-backed, so every
    /// later operation on it, through any alias, goes to the kernel. A timer
    /// that has already expired is armed to fire after 1 ns, and the vessel
    /// is polled until it fires, so it is readable when the guest next
    /// looks; the kernel then counts one expiration, however many were
    /// pending. If the vessel cannot be armed, the timer stays virtual.
    pub(crate) async fn hand_timerfd_to_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<(), Error> {
        let Some(state) = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.timerfd_state())
            .ok()
            .flatten()
        else {
            return Ok(());
        };
        let now = thread_observe_time(guest).await;
        let pending = state.pending(now);
        let value_ns = if pending > 0 {
            1
        } else {
            state
                .next_expiry(now)
                .map_or(0, |next| next.as_nanos().saturating_sub(now.as_nanos()))
        };
        if value_ns != 0 {
            let timespec = |ns: u64| libc::timespec {
                tv_sec: (ns / 1_000_000_000) as libc::time_t,
                tv_nsec: (ns % 1_000_000_000) as libc::c_long,
            };
            let spec = libc::itimerspec {
                it_interval: timespec(state.interval.as_nanos()),
                it_value: timespec(value_ns),
            };
            let mut stack = guest.stack().await;
            let spec = stack.push(spec);
            let pollfd = stack.reserve::<libc::pollfd>();
            let _guard = stack.commit()?;
            let settime = syscalls::TimerfdSettime::new()
                .with_fd(fd)
                .with_flags(0)
                .with_new_value(Some(spec))
                .with_old_value(None);
            if let Err(errno) = guest.inject(settime).await {
                debug!(
                    fd,
                    ?errno,
                    "could not arm a timerfd vessel; the timer stays virtual"
                );
                return Ok(());
            }
            if pending > 0 {
                guest.memory().write_value(
                    pollfd,
                    &libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                )?;
                let wait = syscalls::Poll::new()
                    .with_fds(Some(pollfd.cast()))
                    .with_nfds(1)
                    .with_timeout(TIMERFD_HANDOFF_POLL_MS);
                if let Err(errno) = guest.inject(wait).await {
                    debug!(
                        fd,
                        ?errno,
                        "waiting for a handed-off timerfd to fire failed"
                    );
                }
            }
        }
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.hand_timerfd_to_kernel())?;
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
    /// Hand every virtual timerfd that the epoll `epfd` watches to the
    /// kernel. The shadow keys an interest by the fd number it was added
    /// with; a timerfd no longer open under that number stays virtual.
    async fn hand_epoll_timerfds_to_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        epfd: i32,
    ) -> Result<(), Error> {
        let interests = guest
            .thread_state()
            .with_detfd(epfd, |detfd| detfd.epoll_timer_interests())
            .unwrap_or_default();
        for ((fd, open_file), _) in interests {
            let same_file = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.open_file_id() == open_file)
                .unwrap_or(false);
            if same_file {
                self.hand_timerfd_to_kernel(guest, fd).await?;
            }
        }
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
    /// A poll of an epoll asks the kernel whether the epoll is ready, and
    /// the kernel cannot see virtual timerfds. Hand the timerfds of every
    /// epoll the pollfd array names to the kernel first.
    async fn hand_polled_epoll_timerfds_to_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds_raw: Option<usize>,
        nfds: u64,
    ) -> Result<(), Error> {
        if !self.virtual_timerfds() || !self.cfg.sequentialize_threads {
            return Ok(());
        }
        for entry in read_guest_pollfds(guest, fds_raw, nfds) {
            if entry.fd >= 0 && self.epoll_has_timerfds(guest, entry.fd) {
                self.hand_epoll_timerfds_to_kernel(guest, entry.fd).await?;
            }
        }
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
    /// select and pselect6 counterpart of
    /// [`Self::hand_polled_epoll_timerfds_to_kernel`]: hand the timerfds of
    /// every epoll named in any of the three sets to the kernel first.
    async fn hand_selected_epoll_timerfds_to_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        nfds: i32,
        sets: [Option<usize>; 3],
    ) -> Result<(), Error> {
        if !self.virtual_timerfds() || nfds <= 0 {
            return Ok(());
        }
        let nfds = nfds.min(SELECT_EPOLL_SCAN_MAX_NFDS) as usize;
        let mut named = vec![0u8; nfds.div_ceil(64) * 8];
        for set in sets.into_iter().flatten() {
            let mut bytes = vec![0u8; named.len()];
            let Some(addr) = AddrMut::<u8>::from_raw(set) else {
                continue;
            };
            // A set the kernel cannot read either fails the call itself.
            if guest.memory().read_exact(addr, &mut bytes).is_ok() {
                for (named, byte) in named.iter_mut().zip(bytes) {
                    *named |= byte;
                }
            }
        }
        for (index, byte) in named.into_iter().enumerate() {
            for bit in 0..8 {
                let fd = index * 8 + bit;
                if byte & (1 << bit) != 0 && fd < nfds && self.epoll_has_timerfds(guest, fd as i32)
                {
                    self.hand_epoll_timerfds_to_kernel(guest, fd as i32).await?;
                }
            }
        }
        Ok(())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): timerfds sent over SCM_RIGHTS go to the
    // kernel.
    /// Hand every virtual timerfd that an SCM_RIGHTS message in these control
    /// buffers names to the kernel, before the send. The receiver gets a
    /// descriptor Detcore never saw created, whose timerfd_settime and
    /// timerfd_gettime go to the kernel (`timerfd_control_on_kernel`), so
    /// the timer must already be there for the two ends to share it. Each
    /// control buffer is an (address, length) pair. One that cannot be read
    /// is skipped: the kernel cannot read it either and fails the send.
    async fn hand_sent_timerfds_to_kernel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        controls: Vec<(usize, usize)>,
    ) -> Result<(), Error> {
        if !self.virtual_timerfds() {
            return Ok(());
        }
        let mut sent = Vec::new();
        for (address, length) in controls {
            let Some(addr) = AddrMut::<u8>::from_raw(address) else {
                continue;
            };
            let mut bytes = vec![0u8; length.min(MAX_CONTROL_BYTES)];
            if guest.memory().read_exact(addr, &mut bytes).is_ok() {
                sent.extend(scm_rights_fds(&bytes));
            }
        }
        for fd in sent {
            // A descriptor that is not a virtual timerfd is left alone.
            self.hand_timerfd_to_kernel(guest, fd).await?;
        }
        Ok(())
    }

    /// Whether a pollfd array names any virtual timerfd.
    fn poll_has_timerfds<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds_raw: Option<usize>,
        nfds: u64,
    ) -> bool {
        !self.poll_timerfds(guest, fds_raw, nfds).is_empty()
    }

    /// The virtual timerfds in a guest pollfd array that are ready at virtual
    /// now. Only POLLIN interest can observe a timerfd, as on Linux. Time is
    /// observed only when the array names a virtual timerfd.
    // TODO-HUMAN-REVIEW(PR-3229): virtual timerfd poll readiness.
    pub(crate) async fn poll_timerfd_scan<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds_raw: Option<usize>,
        nfds: u64,
    ) -> Result<Vec<i32>, Error> {
        let timers = self.poll_timerfds(guest, fds_raw, nfds);
        if timers.is_empty() {
            return Ok(Vec::new());
        }
        let now = thread_observe_time(guest).await;
        Ok(timers
            .into_iter()
            .filter(|(_, events, state)| events & libc::POLLIN != 0 && state.pending(now) > 0)
            .map(|(fd, _, _)| fd)
            .collect())
    }

    /// Add ready virtual timerfds to a successful poll/ppoll result: the host
    /// never reports an unarmed timerfd, so set POLLIN on each such entry and
    /// count it alongside the host's ready entries.
    async fn merge_poll_timerfds<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds_raw: Option<usize>,
        nfds: u64,
        host_result: i64,
    ) -> Result<i64, Error> {
        if host_result < 0 {
            return Ok(host_result);
        }
        let ready = self.poll_timerfd_scan(guest, fds_raw, nfds).await?;
        if ready.is_empty() {
            return Ok(host_result);
        }
        let extra = self.poll_timerfd_write(guest, fds_raw, nfds, &ready)?;
        Ok(host_result + extra)
    }

    /// Decide the virtual timer side of one wait round before its host
    /// probe. Only an epoll needs to (see [`TimerWaitRound`]).
    async fn timer_wait_round<G: Guest<Self>>(
        &self,
        guest: &mut G,
        set: TimerWaitSet,
    ) -> Result<TimerWaitRound, Error> {
        match set {
            TimerWaitSet::Poll { .. } => Ok(TimerWaitRound::Poll),
            TimerWaitSet::Epoll {
                epfd,
                events_raw,
                maxevents,
            } => Ok(TimerWaitRound::Epoll(
                self.epoll_timer_round(guest, epfd, events_raw, maxevents)
                    .await?,
            )),
        }
    }

    /// Merge virtual timerfd readiness into a successful host probe result.
    async fn merge_timer_wait_set<G: Guest<Self>>(
        &self,
        guest: &mut G,
        set: TimerWaitSet,
        round: TimerWaitRound,
        host_result: i64,
    ) -> Result<i64, Error> {
        match (set, round) {
            (TimerWaitSet::Poll { fds_raw, nfds }, _) => {
                self.merge_poll_timerfds(guest, fds_raw, nfds, host_result)
                    .await
            }
            (
                TimerWaitSet::Epoll {
                    epfd,
                    events_raw,
                    maxevents,
                },
                TimerWaitRound::Epoll(round),
            ) => {
                self.merge_epoll_timer_events(
                    guest,
                    epfd,
                    events_raw,
                    maxevents,
                    round,
                    host_result,
                )
                .await
            }
            (TimerWaitSet::Epoll { .. }, TimerWaitRound::Poll) => {
                unreachable!("an epoll wait set always gets an epoll round")
            }
        }
    }

    /// A blocking poll, ppoll, epoll_wait or epoll_pwait that may involve
    /// virtual timerfds.
    ///
    /// Like `retry_nonblocking_syscall_with_timeout`, each retry takes a
    /// scheduler turn and probes the host with a zero timeout; in addition it
    /// rescans the virtual timers on every retry (an epoll just before its
    /// probe, a poll just after). Readiness is therefore judged against the
    /// timer state at the logical time of that retry, so a timer that another
    /// thread arms, re-arms, reads or (for epoll) adds while this thread waits
    /// is seen at the next retry. The loop keeps no timer deadline of its own:
    /// it ends only on readiness, the guest deadline, a host error, or a
    /// signal.
    // TODO-HUMAN-REVIEW(PR-3229): virtual timerfd blocking wait loop.
    async fn wait_with_timerfds<G: Guest<Self>, C>(
        &self,
        guest: &mut G,
        call: C,
        set: TimerWaitSet,
        guest_deadline: Option<LogicalTime>,
        fyi: &'static str,
    ) -> Result<i64, Error>
    where
        C: NonblockableSyscall + TimeoutableSyscall + TimerWaitProbe + Into<Syscall> + Copy,
    {
        // The probe's scratch memory must outlive every injection below.
        let (probe, _probe_guard) = call.into_nonblocking(guest).await;
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi(fyi);
        loop {
            if let ResumeStatus::Signaled(_) = resource_request(guest, rsrc.clone()).await {
                return Err(probe.signal_interrupt_errno().into());
            }
            let round = self.timer_wait_round(guest, set).await?;
            let result = match round.host_limit() {
                // The ready timers fill every slot: there is nothing to probe.
                Some(0) => Ok(0),
                limit => {
                    let probe = limit.map_or(probe, |limit| probe.with_host_limit(limit));
                    match guest.inject_with_retry(probe).await.map_err(Error::from) {
                        Ok(value) => Ok(value),
                        Err(Error::Errno(errno)) => Err(errno),
                        Err(error) => return Err(error),
                    }
                }
            };
            let blocked = probe.syscall_would_have_blocked(result);
            let host = if blocked {
                0
            } else {
                probe.normalize_nonblocking_result(result, rsrc.poll_attempt > 0)?
            };
            let total = self.merge_timer_wait_set(guest, set, round, host).await?;
            if total != 0 || !blocked {
                return Ok(total);
            }
            rsrc.poll_attempt += 1;
            if let Some(deadline) = guest_deadline
                && thread_observe_time(guest).await >= deadline
            {
                return probe.timeout_return_val().map_err(Error::from);
            }
            record_retry_event(guest, probe).await;
        }
    }

    /// Write POLLIN revents for ready virtual timerfds; returns their count.
    fn poll_timerfd_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds_raw: Option<usize>,
        nfds: u64,
        ready: &[i32],
    ) -> Result<i64, Error> {
        let Some(fds_raw) = fds_raw else { return Ok(0) };
        let mut count = 0;
        for i in 0..nfds {
            let raw = fds_raw + (i as usize) * std::mem::size_of::<libc::pollfd>();
            let addr = AddrMut::<libc::pollfd>::from_raw(raw).ok_or(Errno::EFAULT)?;
            let mut entry: libc::pollfd = guest.memory().read_value(addr)?;
            if ready.contains(&entry.fd) && entry.events & libc::POLLIN != 0 && entry.revents == 0 {
                entry.revents = libc::POLLIN;
                guest.memory().write_value(addr, &entry)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Handle a poll syscall that deponds on external, nondeterminstic IO.
    pub async fn handle_external_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let len = call.nfds();
        let time_delta = Duration::from_millis(call.timeout() as u64);

        if len == 0 && time_delta.is_zero() {
            let request = Self::sleep_request(guest, time_delta).await;
            resource_request(guest, request).await;
            Ok(0)
        } else {
            print_poll(&call);
            Ok(self
                .record_or_replay_blocking(guest, Syscall::Poll(call))
                .await?)
        }
    }

    /// epoll_create1 syscall
    pub async fn handle_epoll_create1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCreate1,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        // Register the epoll fd in the DetFd table like every other
        // fd-creating syscall (openat, eventfd2, pipe2, socket, ...). Without
        // this, later operations that consult the table via `with_detfd` /
        // `dup_fd` (F_GETFL, F_SETFD, F_DUPFD[_CLOEXEC], dup, ...) would fail
        // with EBADF even though the underlying kernel fd is valid. This broke,
        // for example, running rustup proxies (cargo/rustc) under hermit, whose
        // tokio runtime dups its epoll fd at startup.
        //
        // EPOLL_CLOEXEC shares the same bit value as O_CLOEXEC, so we can carry
        // the cloexec flag straight across.
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits()),
            FdType::Epoll,
        )
        .await?;
        Ok(fd as i64)
    }

    /// Apply an ADD, MOD, or DEL mutation to an epoll interest list.
    ///
    /// Determinism: strict execution serializes this mutation, so its result depends only on the
    /// operation, event payload, and deterministically reconstructed epoll/file-descriptor state.
    /// Record/replay rebuilds that state by reinjecting the same control operations.
    pub async fn handle_epoll_ctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCtl,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        let result = self.record_or_replay(guest, call).await?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3229): nested epoll hands timerfds to the kernel.
        // The shadow below cannot be seen through a nested epoll: an epoll
        // that watches this one asks the kernel, and the host vessel never
        // fires. Until the shadow models nesting, every timerfd a nested
        // epoll watches goes back to the kernel, where base main keeps every
        // timerfd: the ones it watches when another epoll first watches it,
        // and the ones added to it later.
        if result == 0
            && self.virtual_timerfds()
            && matches!(call.op(), libc::EPOLL_CTL_ADD | libc::EPOLL_CTL_MOD)
        {
            let (target_is_epoll, target_is_timerfd) = guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| {
                    (detfd.ty() == FdType::Epoll, detfd.is_timerfd())
                })
                .unwrap_or((false, false));
            if target_is_epoll {
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.set_epoll_nested())?;
                self.hand_epoll_timerfds_to_kernel(guest, call.fd()).await?;
            } else if target_is_timerfd
                && guest
                    .thread_state()
                    .with_detfd(call.epfd(), |detfd| detfd.is_epoll_nested())
                    .unwrap_or(false)
            {
                self.hand_timerfd_to_kernel(guest, call.fd()).await?;
            }
        }
        // Mirror interests in virtual timerfds into the epoll fd's shadow:
        // host epoll can never report their (virtual) readiness. Linux keys an
        // interest by (fd, file) and drops it when the file's last reference
        // closes, so the shadow is keyed by the open file and holds only a weak
        // link to it.
        let target = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| {
                detfd
                    .timerfd_link()
                    .map(|link| (detfd.open_file_id(), link))
            })
            .ok()
            .flatten();
        if let (Some((open_file, link)), 0) = (target, result) {
            let epfd = call.epfd();
            match call.op() {
                libc::EPOLL_CTL_ADD | libc::EPOLL_CTL_MOD => {
                    if let Some(event_ptr) = call.event() {
                        let event: libc::epoll_event = guest.memory().read_value(event_ptr)?;
                        // ADD and MOD both start a fresh interest: MOD re-enables
                        // an EPOLLONESHOT interest and re-arms edge reporting.
                        guest.thread_state().with_detfd(epfd, |detfd| {
                            detfd.epoll_timer_add(
                                call.fd(),
                                open_file,
                                crate::fd::EpollTimerInterest {
                                    events: event.events,
                                    data: event.u64,
                                    edge_reported: None,
                                    oneshot_disarmed: false,
                                    // epoll_timer_add assigns the ready rank.
                                    ready_rank: 0,
                                    target: link.clone(),
                                },
                            )
                        })?;
                    }
                }
                libc::EPOLL_CTL_DEL => {
                    guest
                        .thread_state()
                        .with_detfd(epfd, |detfd| detfd.epoll_timer_remove(call.fd(), open_file))?;
                }
                _ => {}
            }
        }
        Ok(result)
    }

    /// epoll_pwait syscall (MAYHANG)
    pub async fn handle_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        if call.sigmask().is_some() {
            return self.handle_masked_epoll_pwait(guest, call).await;
        }
        if self.epoll_has_timerfds(guest, call.epfd()) {
            return self.handle_internal_epoll_pwait(guest, call).await;
        }
        // This used to unconditionally inject the raw
        // call and wait for it to return. With an infinite timeout under
        // `--sequentialize-threads` that DEADLOCKS the whole guest: the calling
        // task holds the scheduler turn while blocked in the kernel, and the
        // only task that could ever satisfy the wait is sitting in the run queue
        // waiting for a turn that never comes. Observed as `cmake` configure
        // hanging forever at zero CPU with a grandchild frozen mid-`openat`;
        // hermit's own scheduler log ends at `COMMIT turn N, dettid <parent>`
        // injecting `epoll_pwait(..., -1, NULL, 8)` with `queue len 2`.
        //
        // WHO ACTUALLY REACHES THIS, measured rather than assumed. It is NOT
        // glibc's `epoll_wait(2)` on this architecture: glibc calls
        // `SYS_epoll_pwait` only where `__NR_epoll_wait` does not exist (arm64
        // and friends). x86_64 has it, and `strace` on glibc 2.34/x86_64 shows
        // a plain `epoll_wait` syscall, which `handle_epoll_wait` has always
        // handled correctly. The callers that land here are programs issuing
        // `epoll_pwait` DIRECTLY -- libuv does, which is how the original
        // `cmake` hang was found. With a NULL sigmask the two calls are
        // semantically identical, so route them together. A non-NULL sigmask
        // is handled by `handle_masked_epoll_pwait`.
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // Cannot block, but still yield a scheduler turn so a polling thread
            // cannot monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::EpollPwait(call))
                .await?)
        } else {
            self.handle_internal_epoll_pwait(guest, call).await
        }
    }

    /// epoll_pwait with a non-NULL signal mask.
    ///
    /// The scheduler turn comes first. Whether the epoll watches a virtual
    /// timerfd, and which of its timers are ready, are both decided after
    /// it: another thread may arm, read, close, add or remove a timerfd
    /// while this one waits for its turn, and the decision must use the
    /// state at the turn, the logical time of the call. Deciding the entry
    /// before the turn and scanning after it could return 0 from an
    /// infinite wait whose timer a peer consumed in between, or refuse a
    /// wait whose timer a peer armed in between.
    ///
    /// Every outcome is decided on that one post-turn scan merged with the
    /// host probe, never on anything seen before the turn. Two cases follow:
    ///
    /// - A timer that was ready when the call was made, but that a sibling
    ///   thread read during the yield, is not counted. A nonzero or
    ///   infinite timeout therefore never returns 0 early (Linux's
    ///   `ep_poll` never does); with nothing else ready the wait blocks as
    ///   described below.
    /// - A timer that was not ready when the call was made, but whose
    ///   expiry the virtual clock passed during the yield (or that a peer
    ///   armed or added then), is counted and returned at once.
    ///
    /// With a virtual timerfd interest, a wait that need not block is one
    /// timeout-0 probe under the caller's mask, which is atomic exactly as
    /// on Linux. The host is probed too, so a ready host fd returns at once,
    /// as it does on Linux. A wait that must block hands the epoll's
    /// timerfds to the kernel and then waits there, as below.
    ///
    /// Without one, this keeps the previous behavior. A non-NULL sigmask's
    /// whole purpose is to swap the signal mask atomically for the duration
    /// of the wait, and a timeout-0 polling loop cannot reproduce that
    /// atomicity. Such calls remain able to block the scheduler; that is a
    /// known remaining gap rather than something this change silently
    /// pretends to fix.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): masked epoll_pwait decides on the timer
    // state of its own turn.
    async fn handle_masked_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        if !self.epoll_has_timerfds(guest, call.epfd()) {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let set = TimerWaitSet::from(call);
        let round = self.timer_wait_round(guest, set).await?;
        if let TimerWaitRound::Epoll(EpollTimerRound { watched: false, .. }) = round {
            // No timerfd interest is live at the turn: this is the plain
            // masked wait above, whose turn is already taken.
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let host = match round.host_limit() {
            Some(0) => {
                // The ready timers fill every slot, so there is no host
                // probe. Linux installs the mask before it looks at the
                // epoll, and its timeout-0 wait never checks for signals
                // and restores the old mask before returning, so only
                // the mask's own checks remain to be made here.
                if call.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                if let Some(mask) = call.sigmask() {
                    read_kernel_sigset(guest, mask).await?;
                }
                0
            }
            limit => {
                let probe = call.with_timeout(0);
                guest
                    .inject(limit.map_or(probe, |limit| probe.with_host_limit(limit)))
                    .await?
            }
        };
        let total = self.merge_timer_wait_set(guest, set, round, host).await?;
        if total > 0 || call.timeout() == 0 {
            return Ok(total);
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3229): a masked wait that must block hands
        // its timerfds to the kernel instead of refusing.
        // Blocking with a temporary mask cannot be reproduced by a polling
        // loop (see above), and the host wait would never observe a virtual
        // timer. Hand the epoll's timerfds to the kernel, where base main
        // keeps every timerfd, and make the masked wait there, as base main
        // does: the kernel installs the mask atomically, and the timers then
        // run on the host clock.
        self.hand_epoll_timerfds_to_kernel(guest, call.epfd())
            .await?;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// Handle a guest-internal `epoll_pwait` (NULL sigmask) that can be fully
    /// determinized. Mirrors `handle_internal_epoll_wait`.
    pub async fn handle_internal_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            // Cannot block, but must still yield a scheduler turn: a
            // zero-timeout polling loop that never requests a resource can
            // monopolize the guest between preemptions and starve the producer
            // it is polling for. `handle_poll` takes a turn for every
            // sequential mode, and the record/replay arm of `handle_epoll_pwait`
            // does the same; before this PR routed NULL-sigmask `epoll_pwait`
            // here, the old handler always made an empty request. Omitting it
            // only on the plain-strict path would be a scheduling regression,
            // not a refactor.
            if self.cfg.sequentialize_threads {
                let yield_to_peer = self.cfg.discover_live_file_metadata
                    && guest.thread_state().has_loopback_peer();
                resource_request(
                    guest,
                    zero_timeout_poll_request(guest.thread_state().dettid, yield_to_peer),
                )
                .await;
            }
            self.epoll_wait_once(guest, call).await
        } else {
            // Every blocking wait rescans the epoll's timer interests on each
            // retry, so a timerfd added by epoll_ctl while this thread waits
            // is seen; with no interests the rescan observes no time and this
            // is the plain polling loop.
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            self.wait_with_timerfds(guest, call, call.into(), maybe_timeout_ns, "epoll_pwait")
                .await
        }
    }

    /// epoll_pwait2 syscall (MAYHANG).
    ///
    /// epoll_pwait2 is epoll_pwait with a `struct timespec *` timeout instead of
    /// an int-milliseconds timeout; recent glibc implements epoll_wait/
    /// epoll_pwait via epoll_pwait2 when the kernel supports it. The pinned
    /// Reverie revision has no typed variant, so it arrives as a raw
    /// `Syscall::Other` and is dispatched here by Sysno. Detcore treats it
    /// exactly like epoll_pwait: a scheduler yield point followed by
    /// record/replay-aware forwarding of the raw call. An epoll that watches
    /// a virtual timerfd at the turn takes `handle_timer_epoll_pwait2`
    /// instead; as for a masked epoll_pwait, that is decided after the turn,
    /// since a peer may add or remove one while this thread waits for it.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#773)
    pub async fn handle_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        let (_, args) = call.into_parts();
        if self.epoll_has_timerfds(guest, args.arg0 as i32) {
            return self.handle_timer_epoll_pwait2(guest, call, args).await;
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// epoll_pwait2 on an epoll watching a virtual timerfd, after its turn.
    ///
    /// The host never reports a virtual timerfd, so the call runs through
    /// the epoll_pwait timer machinery. Its host probe is an epoll_pwait with
    /// timeout 0 and the guest's epoll, output array, maxevents, signal mask
    /// and mask size. Once Linux has read the timeout, it judges both calls
    /// by the same `do_epoll_pwait`, so the probe reports Linux's errors in
    /// Linux's order; the timeout's own errors come first (see
    /// `read_epoll_pwait2_timeout`).
    ///
    /// A zero timeout is one non-blocking round, which does not check for
    /// signals. A NULL timeout waits forever, and any other is a deadline
    /// that far past the virtual time of the call, in nanoseconds. Without a
    /// mask a wait that blocks is the epoll_pwait polling loop
    /// (`wait_with_timerfds`). With one, a wait that need not block is one
    /// timeout-0 probe under the mask, and a wait that must block hands the
    /// epoll's timerfds to the kernel and is forwarded, both exactly as for
    /// a masked epoll_pwait.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): epoll_pwait2 on a virtual timerfd reuses
    // the epoll_pwait timer machinery with a nanosecond deadline.
    async fn handle_timer_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        args: SyscallArgs,
    ) -> Result<i64, Error> {
        let timeout = self.read_epoll_pwait2_timeout(guest, args).await?;
        let Syscall::EpollPwait(probe) = Syscall::from_raw(
            Sysno::epoll_pwait,
            SyscallArgs::new(args.arg0, args.arg1, args.arg2, 0, args.arg4, args.arg5),
        ) else {
            unreachable!("epoll_pwait is a typed syscall")
        };
        let may_block = timeout != Some(Duration::ZERO);
        if probe.sigmask().is_some() {
            let probed = self
                .masked_epoll_timer_probe(guest, probe, may_block)
                .await?;
            return match probed {
                Some(total) => Ok(total),
                // No timerfd interest is live, or the timerfds were handed
                // to the kernel: forward the plain masked wait.
                None => Ok(self.record_or_replay(guest, call).await?),
            };
        }
        if !may_block {
            return self.epoll_wait_once(guest, probe).await;
        }
        let deadline = match timeout {
            Some(timeout) => Some(thread_observe_time(guest).await + timeout),
            None => None,
        };
        self.wait_with_timerfds(guest, probe, probe.into(), deadline, "epoll_pwait2")
            .await
    }

    /// The timeout of an epoll_pwait2, judged as Linux judges it before
    /// anything else: EFAULT when the kernel cannot copy it, then EINVAL when
    /// it is not a valid timespec (negative seconds, or nanoseconds outside
    /// [0, 1e9)). None is a NULL timeout.
    ///
    /// `read_exact_with_user_access` refuses a page the guest cannot read, as
    /// the kernel's copy does, but on ptrace (`process_vm_readv`) it also
    /// refuses some pages the guest can read, such as a write-only mapping.
    /// When it refuses, the kernel decides: with no mask and maxevents 0,
    /// epoll_pwait2 copies and checks the timeout, then fails with EINVAL
    /// before it looks at the epoll, so only a failed copy reports EFAULT.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): epoll_pwait2 timeout validation order.
    async fn read_epoll_pwait2_timeout<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
    ) -> Result<Option<Duration>, Error> {
        let Some(address) = Addr::<Timespec>::from_raw(args.arg3) else {
            return Ok(None);
        };
        let mut bytes = [0_u8; std::mem::size_of::<Timespec>()];
        if guest
            .memory()
            .read_exact_with_user_access(address.cast::<u8>(), &mut bytes)
            .is_err()
        {
            let check = Syscall::from_raw(
                Sysno::epoll_pwait2,
                SyscallArgs::new(args.arg0, 0, 0, args.arg3, 0, 0),
            );
            match guest.inject(check).await {
                Err(Errno::EINVAL) => {}
                Err(errno) => return Err(errno.into()),
                // Linux always rejects maxevents 0.
                Ok(_) => return Err(Errno::EIO.into()),
            }
            // The kernel copied it, so read it a word at a time, which a
            // ptrace peek can do.
            for (index, word) in bytes.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                let raw = args.arg3.checked_add(index * 8).ok_or(Errno::EFAULT)?;
                let address = Addr::<u64>::from_raw(raw).ok_or(Errno::EFAULT)?;
                *word = guest.memory().read_value(address)?.to_ne_bytes();
            }
        }
        let (tv_sec, tv_nsec) = bytes.split_at(8);
        let timeout = Timespec {
            tv_sec: libc::time_t::from_ne_bytes(tv_sec.try_into().unwrap()),
            tv_nsec: libc::c_long::from_ne_bytes(tv_nsec.try_into().unwrap()),
        };
        Ok(Some(ppoll_timeout_duration(timeout)?))
    }

    /// One masked timeout-0 epoll_pwait probe on an epoll watching a virtual
    /// timerfd, after the caller's turn: the timer half of
    /// `handle_masked_epoll_pwait`, for epoll_pwait2, whose blocking is
    /// decided by its timespec rather than the probe's timeout. None when no
    /// timerfd interest is live at the turn, or when the wait must block and
    /// the epoll's timerfds were handed to the kernel, so the caller
    /// forwards its own call.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): masked epoll_pwait2 decides on the timer
    // state of its own turn.
    async fn masked_epoll_timer_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::EpollPwait,
        may_block: bool,
    ) -> Result<Option<i64>, Error> {
        let set = TimerWaitSet::from(probe);
        let round = self.timer_wait_round(guest, set).await?;
        if let TimerWaitRound::Epoll(EpollTimerRound { watched: false, .. }) = round {
            return Ok(None);
        }
        let host = match round.host_limit() {
            Some(0) => {
                // As in `handle_masked_epoll_pwait`: no host probe, so only
                // the mask's own checks remain.
                if probe.sigsetsize() != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                if let Some(mask) = probe.sigmask() {
                    read_kernel_sigset(guest, mask).await?;
                }
                0
            }
            limit => {
                guest
                    .inject(limit.map_or(probe, |limit| probe.with_host_limit(limit)))
                    .await?
            }
        };
        let total = self.merge_timer_wait_set(guest, set, round, host).await?;
        if total > 0 || !may_block {
            return Ok(Some(total));
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3229): a masked wait that must block hands
        // its timerfds to the kernel instead of refusing.
        // As in `handle_masked_epoll_pwait`.
        self.hand_epoll_timerfds_to_kernel(guest, probe.epfd())
            .await?;
        Ok(None)
    }

    /// epoll_wait syscall (MAYHANG)
    pub async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        // An epoll holding virtual timerfds must take the deterministic merge
        // path in every mode: host waits can never observe virtual readiness.
        if self.epoll_has_timerfds(guest, call.epfd()) {
            return self.handle_internal_epoll_wait(guest, call).await;
        }
        if self.cfg.recordreplay_modes && call.timeout() == 0 {
            // This cannot block, but still yield a scheduler turn so a polling thread cannot
            // monopolize the guest between preemptions.
            resource_request(guest, Resources::new(guest.thread_state().dettid)).await;
            Ok(self.record_or_replay(guest, call).await?)
        } else if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            Ok(self
                .record_or_replay_blocking(guest, Syscall::EpollWait(call))
                .await?)
        } else {
            self.handle_internal_epoll_wait(guest, call).await
        }
    }

    /// Whether this epoll instance shadows any live virtual timerfd interest.
    pub(crate) fn epoll_has_timerfds<G: Guest<Self>>(&self, guest: &mut G, epfd: i32) -> bool {
        guest
            .thread_state()
            .with_detfd(epfd, |detfd| !detfd.epoll_timer_interests().is_empty())
            .unwrap_or(false)
    }

    /// One timeout-0 epoll wait (epoll_wait, or epoll_pwait with a NULL
    /// mask) that may merge virtual timerfd events: decide the round, probe
    /// the host with what is left of maxevents, and merge.
    async fn epoll_wait_once<G: Guest<Self>, C>(&self, guest: &mut G, call: C) -> Result<i64, Error>
    where
        C: TimerWaitProbe + SyscallInfo + Into<TimerWaitSet>,
    {
        let set: TimerWaitSet = call.into();
        let round = self.timer_wait_round(guest, set).await?;
        let host = match round.host_limit() {
            // The ready timers fill every slot: there is nothing to probe.
            // The epoll and maxevents are valid (see epoll_host_limit), and
            // there is no signal mask to install, so the call cannot fail.
            Some(0) => 0,
            // Already non-blocking.
            limit => {
                guest
                    .inject(limit.map_or(call, |limit| call.with_host_limit(limit)))
                    .await?
            }
        };
        self.merge_timer_wait_set(guest, set, round, host).await
    }

    /// The virtual timer side of one epoll wait round: the timerfd events
    /// this epoll would report at virtual now, in its ready order, and the
    /// maxevents its host probe may fill.
    ///
    /// Only EPOLLIN is ever reported (a timerfd is never writable). An
    /// EPOLLONESHOT interest that already fired stays silent until MOD. An
    /// EPOLLET interest reports once per arming generation: Linux raises one
    /// wakeup per settime or consuming read, even for a periodic timer, because
    /// its hrtimer is forwarded only by a read. The ready order rotates as
    /// events are delivered (see `DetFd::epoll_timer_interests`), and when the
    /// epoll gives the timers the first claim on maxevents the host probe is
    /// limited to the slots they leave (see `epoll_host_limit`). Nothing is
    /// committed here; see `merge_epoll_timer_events`.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): virtual timerfd epoll readiness, ready
    // order, ET and ONESHOT semantics.
    async fn epoll_timer_round<G: Guest<Self>>(
        &self,
        guest: &mut G,
        epfd: i32,
        events_raw: Option<usize>,
        maxevents: i32,
    ) -> Result<EpollTimerRound, Error> {
        let (interests, timers_first) = guest
            .thread_state()
            .with_detfd(epfd, |detfd| {
                (detfd.epoll_timer_interests(), detfd.epoll_timers_first())
            })
            .unwrap_or_default();
        let mut round = EpollTimerRound {
            watched: !interests.is_empty(),
            ready: Vec::new(),
            host_limit: None,
        };
        if interests.is_empty() {
            return Ok(round);
        }
        let now = thread_observe_time(guest).await;
        for (key, interest) in interests {
            if interest.oneshot_disarmed || interest.events & (libc::EPOLLIN as u32) == 0 {
                continue;
            }
            let Some(state) = interest.target.state() else {
                continue;
            };
            if state.pending(now) == 0 {
                continue;
            }
            if interest.events & (libc::EPOLLET as u32) != 0
                && interest.edge_reported == Some(state.generation)
            {
                continue;
            }
            round.ready.push(EpollTimerEvent {
                key,
                generation: state.generation,
                event: libc::epoll_event {
                    events: libc::EPOLLIN as u32,
                    u64: interest.data,
                },
            });
        }
        if timers_first {
            round.host_limit = epoll_host_limit(events_raw, maxevents, round.ready.len());
        }
        Ok(round)
    }

    /// Ready virtual timerfds present in a select read bitmap.
    ///
    /// Time is observed only when the set holds a virtual timerfd, so a
    /// select without one sees no extra clock read.
    // TODO-HUMAN-REVIEW(PR-3229): virtual timerfd select readiness.
    pub(crate) async fn select_timerfd_scan<G: Guest<Self>>(
        &self,
        guest: &mut G,
        nfds: i32,
        readfds: &Option<Vec<u8>>,
    ) -> Result<Vec<i32>, Error> {
        let mut timers = Vec::new();
        if let Some(bytes) = readfds {
            for fd in 0..nfds.max(0) {
                let byte = bytes.get((fd / 8) as usize).copied().unwrap_or(0);
                if byte & (1u8 << (fd % 8)) == 0 {
                    continue;
                }
                let state = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.timerfd_state())
                    .ok()
                    .flatten();
                if let Some(state) = state {
                    timers.push((fd, state));
                }
            }
        }
        if timers.is_empty() {
            return Ok(Vec::new());
        }
        let now = thread_observe_time(guest).await;
        Ok(timers
            .into_iter()
            .filter(|(_, state)| state.pending(now) > 0)
            .map(|(fd, _)| fd)
            .collect())
    }

    /// Virtual timerfds whose bits are set in a guest select read set,
    /// read before the kernel overwrites it. Only the bytes holding an open
    /// timerfd's bit are read, and Linux reads those too; an unreadable byte
    /// is left to the kernel, which reports its own error.
    fn select_read_timerfds<G: Guest<Self>>(
        &self,
        guest: &mut G,
        nfds: i32,
        readfds: Option<usize>,
    ) -> Vec<i32> {
        let Some(readfds) = readfds.filter(|_| self.virtual_timerfds()) else {
            return Vec::new();
        };
        guest
            .thread_state()
            .timerfds_below(nfds)
            .into_iter()
            .filter(|&fd| {
                read_select_byte(guest, readfds, fd)
                    .is_ok_and(|(_, byte)| byte & (1u8 << (fd % 8)) != 0)
            })
            .collect()
    }

    /// Whether a blocking select or pselect6 wider than one word must stay
    /// with Detcore: its read set names a virtual timerfd, whose never-armed
    /// host vessel the kernel would never report ready. The retry loop's
    /// probe sets hold any nfds: stack scratch up to FD_SETSIZE, and beyond
    /// it a mapping that lives for one probe (`inject_wide_select_probe`).
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): select and pselect6 above FD_SETSIZE no longer refuse with ENOSYS.
    fn wide_select_needs_detcore<G: Guest<Self>>(
        &self,
        guest: &mut G,
        nfds: i32,
        readfds: Option<usize>,
    ) -> bool {
        !self.select_read_timerfds(guest, nfds, readfds).is_empty()
    }

    /// A zero-timeout select or pselect6: one host poll, plus ready virtual
    /// timerfds from the read set.
    async fn select_poll_with_timerfds<G: Guest<Self>, C: SyscallInfo + Into<Syscall> + Copy>(
        &self,
        guest: &mut G,
        nfds: i32,
        readfds: Option<usize>,
        call: C,
    ) -> Result<i64, Error> {
        let named = self.select_read_timerfds(guest, nfds, readfds);
        let host = guest.inject(call).await?;
        if named.is_empty() {
            return Ok(host);
        }
        let now = thread_observe_time(guest).await;
        let ready: Vec<i32> = named
            .into_iter()
            .filter(|&fd| {
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.timerfd_state())
                    .ok()
                    .flatten()
                    .is_some_and(|state| state.pending(now) > 0)
            })
            .collect();
        set_select_read_bits(guest, readfds, &ready)?;
        Ok(host + ready.len() as i64)
    }

    /// Handle a guest-internal `epoll_wait` call that can be fully determinized.
    pub async fn handle_internal_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollWait,
    ) -> Result<i64, Error> {
        let timeout_millis = call.timeout();
        if timeout_millis == 0 {
            self.epoll_wait_once(guest, call).await
        } else {
            // See handle_internal_epoll_pwait: the loop rescans timer interests.
            let maybe_timeout_ns = millis_duration_to_absolute_timeout(guest, timeout_millis).await;
            self.wait_with_timerfds(guest, call, call.into(), maybe_timeout_ns, "epoll_wait")
                .await
        }
    }

    /// Append a round's ready virtual timerfd events after the host probe's
    /// events, up to maxevents. Host events keep their order and come first
    /// in the array; the timerfd events follow in the epoll's ready order.
    /// When the round gave the timers the first claim, the probe was limited
    /// to the slots they leave, so every claimed event fits.
    ///
    /// Only events actually written are delivered: an EPOLLET edge or
    /// EPOLLONESHOT interest left out by maxevents stays pending for the next
    /// wait, as on Linux, and each delivered interest moves to the back of
    /// the ready order, as Linux moves a delivered level-triggered item to
    /// the tail of its ready list. Then the epoll records which side claims
    /// maxevents first next time (see `next_epoll_fill_order`).
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3229): virtual timerfd epoll delivery and fairness.
    async fn merge_epoll_timer_events<G: Guest<Self>>(
        &self,
        guest: &mut G,
        epfd: i32,
        events_raw: Option<usize>,
        maxevents: i32,
        round: EpollTimerRound,
        host_result: i64,
    ) -> Result<i64, Error> {
        if host_result < 0 || round.ready.is_empty() {
            return Ok(host_result);
        }
        let events_raw = events_raw.ok_or(Errno::EFAULT)?;
        let maxevents = maxevents.max(0) as i64;
        let mut written = host_result;
        let mut delivered = 0;
        for ready in &round.ready {
            if written >= maxevents {
                break;
            }
            let raw = events_raw + (written as usize) * std::mem::size_of::<libc::epoll_event>();
            let addr = AddrMut::<libc::epoll_event>::from_raw(raw).ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr, &ready.event)?;
            guest.thread_state().with_detfd(epfd, |detfd| {
                detfd.epoll_timer_delivered(ready.key, ready.generation)
            })?;
            written += 1;
            delivered += 1;
        }
        let omitted = round.ready.len() - delivered;
        if let Some(timers_first) = next_epoll_fill_order(round.host_limit, host_result, omitted) {
            guest
                .thread_state()
                .with_detfd(epfd, |detfd| detfd.set_epoll_timers_first(timers_first))?;
        }
        Ok(written)
    }

    /// Connect system call (MAYHANG)
    /// Note that connect waits until a TCP handshake but does not wait for accept() on the other end.
    /// Nevertheless, it can block for a long time while waiting for connection, unless the socket
    /// is already nonblocking.
    pub async fn handle_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let uservaddr = call.uservaddr();
        let addrlen = call.addrlen();

        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing connect");
            let resource = ResourceID::PriorityChangePoint(
                FIRST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
                guest.thread_state().committed_clock_value,
                Vec::new(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }

        let result = self.execute_nonblockable_fd_syscall(guest, call).await;
        if self.cfg.discover_live_file_metadata
            && connect_result_allows_peer_classification(&result)
        {
            // This metadata is a SaBRe-only scheduling hint, not part of connect's semantics.
            // Let the kernel establish the authoritative result first, then classify the peer
            // best-effort so an invalid guest pointer or an untracked fd can never replace the
            // kernel's errno.
            let loopback_peer = (|| -> Result<Option<bool>, Error> {
                let Some(address) = uservaddr else {
                    return Ok(None);
                };
                let addrlen = usize::try_from(addrlen).unwrap_or(0);
                if addrlen < std::mem::size_of::<u16>() {
                    return Ok(None);
                }
                let family: u16 = guest.memory().read_value(address.cast())?;
                if family == libc::AF_INET as u16 {
                    if addrlen < std::mem::size_of::<libc::sockaddr_in>() {
                        return Ok(None);
                    }
                    let address: libc::sockaddr_in = guest.memory().read_value(address.cast())?;
                    Ok(Some(
                        Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()).is_loopback(),
                    ))
                } else if family == libc::AF_INET6 as u16 {
                    if addrlen < std::mem::size_of::<libc::sockaddr_in6>() {
                        return Ok(None);
                    }
                    let address: libc::sockaddr_in6 = guest.memory().read_value(address.cast())?;
                    Ok(Some(
                        Ipv6Addr::from(address.sin6_addr.s6_addr).is_loopback(),
                    ))
                } else {
                    // This includes a successful AF_UNSPEC disconnect and successful connects
                    // to non-IP families, neither of which has a loopback IP peer.
                    Ok(Some(false))
                }
            })()
            .ok()
            .flatten();
            if let Some(loopback_peer) = loopback_peer {
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_loopback_peer(loopback_peer));
            }
        }

        result
    }

    /// Handles sendto, sendmsg, and sendmmsg syscalls (MAYHANG).
    pub async fn handle_sendrecv<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
    ) -> Result<i64, Error> {
        self.execute_nonblockable_fd_syscall(guest, call).await
    }

    /// Sends one message and invalidates process-wide flock knowledge after success.
    ///
    /// The guest can mutate shared message and control memory while this helper
    /// deschedules. Parsing before the syscall would not prove which descriptors the
    /// kernel later transferred, so a successful unbound send conservatively makes
    /// every cached open-file-description lock mode unknown.
    pub async fn handle_sendmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendmsg,
    ) -> Result<i64, Error> {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3229): timerfds sent over SCM_RIGHTS go to the
        // kernel. A header the kernel cannot read either fails the send.
        if self.virtual_timerfds() {
            let controls: Vec<(usize, usize)> = call
                .msg()
                .and_then(|address| guest.memory().read_value(address).ok())
                .and_then(|message: libc::msghdr| message_control(&message))
                .into_iter()
                .collect();
            self.hand_sent_timerfds_to_kernel(guest, controls).await?;
        }
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        guest.thread_state().forget_flock_modes();
        Ok(result)
    }

    /// Sends a message batch and invalidates process-wide flock knowledge when the
    /// kernel reports at least one message sent. This intentionally includes
    /// descriptors named only by an unsent tail message: the mutable guest array is
    /// not stable across a possible deschedule, so narrower attribution is unsafe.
    pub async fn handle_sendmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendmmsg,
    ) -> Result<i64, Error> {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3229): timerfds sent over SCM_RIGHTS go to the
        // kernel. net/socket.c (__sys_sendmmsg) caps the batch at UIO_MAXIOV
        // and stops at the first header it cannot read, so the headers read
        // here are the ones the kernel can send.
        if self.virtual_timerfds() {
            let controls: Vec<(usize, usize)> = match call.msgvec() {
                Some(address) => {
                    let first = address.as_raw();
                    let count = call.vlen().min(libc::UIO_MAXIOV as u32) as usize;
                    let stride = std::mem::size_of::<libc::mmsghdr>();
                    (0..count)
                        .map_while(|index| {
                            let address = Addr::<libc::mmsghdr>::from_raw(
                                first.checked_add(index * stride)?,
                            )?;
                            guest.memory().read_value(address).ok()
                        })
                        .filter_map(|message: libc::mmsghdr| message_control(&message.msg_hdr))
                        .collect()
                }
                None => Vec::new(),
            };
            self.hand_sent_timerfds_to_kernel(guest, controls).await?;
        }
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        if result > 0 {
            guest.thread_state().forget_flock_modes();
        }
        Ok(result)
    }

    // TODO-HUMAN-REVIEW(PR-912): Review receive-time capture across socket aliases.
    async fn observe_socket_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<LogicalTime, Error> {
        let timestamp = thread_observe_time(guest).await;
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_socket_receive_timestamp(timestamp);
        })?;
        Ok(timestamp)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive one message and replace host socket timestamps with logical time.
    pub async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmsg,
    ) -> Result<i64, Error> {
        // NETLINK_SOCK_DIAG replies carry host-assigned socket identities in
        // their msg_iov payload (not msg_control). Canonicalize the supported
        // fields before the binary reply reaches the guest.
        // The predicate is shared with the read/readv/recvfrom/recvmmsg paths so
        // the five receive syscalls cannot drift apart again.
        if self.sock_diag_reply_fd(guest, call.sockfd()) {
            return self.handle_sock_diag_recvmsg(guest, call).await;
        }

        if !self.cfg.virtualize_time {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        let Some(message_address) = call.msg() else {
            return self
                .handle_socket_receive(guest, call, call.sockfd(), true)
                .await;
        };
        // Snapshot every input field before the receive. Linux permits the
        // control buffer to overlap this header, so rereading it afterward can
        // turn a successful consuming receive into an artificial EFAULT.
        let message: libc::msghdr = guest.memory().read_value(message_address)?;
        if message.msg_control.is_null() || message.msg_controllen == 0 {
            return self
                .handle_socket_receive(guest, call, call.sockfd(), true)
                .await;
        }
        let control_len = message.msg_controllen.min(MAX_CONTROL_BYTES);
        let control_address: AddrMut<'_, u8> =
            AddrMut::from_raw(message.msg_control as usize).ok_or(Errno::EFAULT)?;
        let mut control = vec![0; control_len];
        // Validate the output region before consuming a datagram.
        guest.memory().read_exact(control_address, &mut control)?;

        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let now = self.observe_socket_receive(guest, call.sockfd()).await?;
        let mut control = vec![0; control_len];
        guest.memory().read_exact(control_address, &mut control)?;
        if socket_timestamp_messages(&control).is_empty() {
            return Ok(result);
        }

        canonicalize_socket_timestamps(&mut control, now);
        guest.memory().write_exact(control_address, &control)?;
        Ok(result)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Whether replies received on `fd` must have their supported socket
    /// identities canonicalized.
    ///
    /// This is the single predicate every receive path consults. It exists as
    /// one function because the flag used to be tested inline in `recvmsg`
    /// alone, and four other receive syscalls reached the same dump without it
    /// (see [`Self::sanitize_sock_diag_segments`]).
    pub(crate) fn sock_diag_reply_fd<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> bool {
        self.cfg.virtualize_metadata
            && guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.is_sock_diag() || detfd.is_netlink_route())
                .unwrap_or(false)
    }

    /// Whether this descriptor is specifically a `NETLINK_ROUTE` socket, which
    /// needs the link-counter sanitizer rather than the sock-diag one.
    fn netlink_route_reply_fd<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> bool {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.is_netlink_route())
            .unwrap_or(false)
    }

    /// Read an `iovec` array out of guest memory as plain `(address, capacity)`
    /// scalars, so no non-`Send` raw pointer is held across an await.
    fn read_iov_segments<G: Guest<Self>>(
        guest: &mut G,
        iov: usize,
        iovlen: usize,
    ) -> Result<Vec<(usize, usize)>, Error> {
        if iov == 0 || iovlen == 0 {
            return Ok(Vec::new());
        }
        let count = iovlen.min(libc::UIO_MAXIOV as usize);
        let address: AddrMut<'_, libc::iovec> = AddrMut::from_raw(iov).ok_or(Errno::EFAULT)?;
        // SAFETY: `iovec` is a plain C record; an all-zero value is a valid
        // staging value immediately overwritten by `read_values`.
        let mut iovecs: Vec<libc::iovec> =
            (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        guest.memory().read_values(address.into(), &mut iovecs)?;
        Ok(iovecs
            .iter()
            .map(|iov| (iov.iov_base as usize, iov.iov_len))
            .collect())
    }

    /// Canonicalize host-assigned identities in a `NETLINK_SOCK_DIAG` reply that
    /// the kernel has already written into guest memory.
    ///
    /// `segments` describes the destination buffers as `(address, capacity)` in
    /// the order the kernel filled them; `received` is the syscall's return
    /// value. The reply is gathered contiguously (it may be scattered across
    /// several buffers), sanitized via `crate::sock_diag` (fail-open,
    /// zero-only, never resizes), and written back preserving the original
    /// boundaries. Bounded by both each buffer's capacity and `received`, which
    /// under `MSG_TRUNC` can exceed the total capacity.
    ///
    /// Synchronous on purpose: every caller has already completed its receive,
    /// so nothing here awaits and no guest address outlives the borrow.
    fn sanitize_sock_diag_segments<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        segments: &[(usize, usize)],
        received: usize,
    ) -> Result<(), Error> {
        if received == 0 || segments.is_empty() {
            return Ok(());
        }
        let mut filled: Vec<(AddrMut<'_, u8>, usize)> = Vec::new();
        let mut buffer: Vec<u8> = Vec::with_capacity(received);
        let mut remaining = received;
        for &(base, capacity) in segments {
            if remaining == 0 {
                break;
            }
            if base == 0 || capacity == 0 {
                continue;
            }
            let take = capacity.min(remaining);
            let address: AddrMut<'_, u8> = AddrMut::from_raw(base).ok_or(Errno::EFAULT)?;
            let mut segment = vec![0u8; take];
            guest.memory().read_exact(address, &mut segment)?;
            buffer.extend_from_slice(&segment);
            filled.push((address, take));
            remaining -= take;
        }

        // NETLINK_ROUTE and NETLINK_SOCK_DIAG replies need different
        // sanitizers: one zeroes live interface counters, the other determinizes
        // supported socket identities. The descriptor decides which, so a guest
        // holding both kinds of socket gets each handled correctly.
        let modified = if self.netlink_route_reply_fd(guest, fd) {
            crate::netlink_route::sanitize_route_link_stats(&mut buffer)
        } else {
            crate::sock_diag::sanitize_sock_diag_identities(&mut buffer)
        };
        if !modified {
            return Ok(());
        }

        let mut offset = 0;
        for (address, len) in filled {
            guest
                .memory()
                .write_exact(address, &buffer[offset..offset + len])?;
            offset += len;
        }
        Ok(())
    }

    /// `recvfrom` on a socket-diag descriptor: one destination buffer.
    ///
    /// `recv(2)` has no syscall of its own on x86_64 — glibc lowers it to
    /// `recvfrom` with a null address — so this covers `recv` as well, which is
    /// what Python's `socket.recv()` reaches.
    pub async fn handle_sock_diag_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvfrom,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let base = call.buf().map(|address| address.as_raw()).unwrap_or(0);
        let len = call.len();
        let result = self.handle_socket_receive(guest, call, fd, true).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &[(base, len)], received)?;
        Ok(result)
    }

    /// `read` on a socket-diag descriptor: one destination buffer.
    pub async fn handle_sock_diag_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let base = call.buf().map(|address| address.as_raw()).unwrap_or(0);
        let len = call.len();
        let result = self.handle_read(guest, call).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &[(base, len)], received)?;
        Ok(result)
    }

    /// Receive a `NETLINK_SOCK_DIAG` dump and zero the host-assigned socket
    /// inode numbers in the reply so `ss`-style enumeration is deterministic.
    ///
    /// The dump lands in `msg_iov` (netlink diag sockets carry no ancillary
    /// data), possibly scattered across several iovecs.
    async fn handle_sock_diag_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmsg,
    ) -> Result<i64, Error> {
        let fd = call.sockfd();
        let Some(message_address) = call.msg() else {
            return self.handle_socket_receive(guest, call, fd, true).await;
        };
        // Snapshot the header's iovec pointer/count before the receive (they are
        // stable across it; the kernel fills the pointed-to buffers, not the
        // array). Scoped so the `msghdr`'s raw pointers are dropped before the
        // await below: holding one would make this future non-`Send`.
        let segments = {
            let message: libc::msghdr = guest.memory().read_value(message_address)?;
            Self::read_iov_segments(guest, message.msg_iov as usize, message.msg_iovlen)?
        };

        let result = self.handle_socket_receive(guest, call, fd, true).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &segments, received)?;
        Ok(result)
    }

    /// `readv` on a socket-diag descriptor: an `iovec` array, no message header.
    pub async fn handle_sock_diag_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let iov = call.iov().map(|address| address.as_raw()).unwrap_or(0);
        let segments = Self::read_iov_segments(guest, iov, call.len())?;
        let result = self.handle_readv(guest, call).await?;
        let received = usize::try_from(result).unwrap_or(0);
        self.sanitize_sock_diag_segments(guest, fd, &segments, received)?;
        Ok(result)
    }

    /// `recvmmsg` on a socket-diag descriptor.
    ///
    /// Each delivered `mmsghdr` is a separate datagram with its own byte count
    /// in `msg_len`, so each is gathered and sanitized independently; treating
    /// the batch as one buffer would let one message's length run into the
    /// next message's memory.
    pub async fn handle_sock_diag_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmmsg,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let Some(messages_address) = call.mmsg() else {
            return self.handle_recvmmsg(guest, call).await;
        };
        let vlen = call.vlen();
        if vlen == 0 || vlen > libc::UIO_MAXIOV as u32 {
            return self.handle_recvmmsg(guest, call).await;
        }
        let base = messages_address.as_raw();

        // Snapshot each message's iovec geometry BEFORE the receive: the kernel
        // fills the pointed-to buffers and writes msg_len, but does not move the
        // iovec arrays themselves. Scoped, and reduced to plain scalars, so the
        // `mmsghdr` raw pointers are dropped before the await below: holding one
        // would make this future non-`Send`.
        let geometry: Vec<Vec<(usize, usize)>> = {
            // SAFETY: `mmsghdr` is a plain C record; an all-zero value is a
            // valid staging value immediately overwritten by `read_values`.
            let mut headers: Vec<libc::mmsghdr> = (0..vlen as usize)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            guest
                .memory()
                .read_values(messages_address.into(), &mut headers)?;
            let mut geometry = Vec::with_capacity(headers.len());
            for header in &headers {
                geometry.push(Self::read_iov_segments(
                    guest,
                    header.msg_hdr.msg_iov as usize,
                    header.msg_hdr.msg_iovlen,
                )?);
            }
            geometry
        };

        let result = self.handle_recvmmsg(guest, call).await?;
        let delivered = usize::try_from(result).unwrap_or(0).min(geometry.len());
        if delivered == 0 {
            return Ok(result);
        }

        // Re-read the array for the per-message byte counts the kernel just
        // wrote. Also scoped: nothing awaits past this point, but keeping the
        // raw pointers contained keeps the rule visible.
        let counts: Vec<usize> = {
            let address: AddrMut<'_, libc::mmsghdr> =
                AddrMut::from_raw(base).ok_or(Errno::EFAULT)?;
            // SAFETY: as above.
            let mut headers: Vec<libc::mmsghdr> = (0..vlen as usize)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            guest.memory().read_values(address.into(), &mut headers)?;
            headers.iter().map(|h| h.msg_len as usize).collect()
        };
        for (index, segments) in geometry.iter().enumerate().take(delivered) {
            self.sanitize_sock_diag_segments(guest, fd, segments, counts[index])?;
        }
        Ok(result)
    }

    // TODO-HUMAN-REVIEW(PR-912): Review receive-time capture across socket aliases.
    /// Handle a socket receive and retain one timestamp for every alias of its open file.
    pub async fn handle_socket_receive<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
        fd: i32,
        zero_delivers_packet: bool,
    ) -> Result<i64, Error> {
        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        if self.cfg.virtualize_time && (result > 0 || (result == 0 && zero_delivers_packet)) {
            self.observe_socket_receive(guest, fd).await?;
        }
        Ok(result)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-901)
    /// Receive a message batch and replace every host socket timestamp with logical time.
    pub async fn handle_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvmmsg,
    ) -> Result<i64, Error> {
        if !self.cfg.virtualize_time || call.vlen() > libc::UIO_MAXIOV as u32 {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        }

        let Some(messages_address) = call.mmsg() else {
            return self.execute_nonblockable_fd_syscall(guest, call).await;
        };
        let controls = {
            // SAFETY: `mmsghdr` is a plain C record and an all-zero value is a valid
            // initialized staging value that is immediately overwritten by `read_values`.
            let mut messages: Vec<libc::mmsghdr> = (0..call.vlen())
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            guest
                .memory()
                .read_values(messages_address.into(), &mut messages)?;

            let mut controls = Vec::with_capacity(messages.len());
            for message in &messages {
                let header = &message.msg_hdr;
                if header.msg_control.is_null() || header.msg_controllen == 0 {
                    controls.push(None);
                    continue;
                }
                controls.push(Some((
                    header.msg_control as usize,
                    header.msg_controllen.min(MAX_CONTROL_BYTES),
                )));
            }
            controls
        };

        let result = self.execute_nonblockable_fd_syscall(guest, call).await?;
        let delivered = usize::try_from(result).unwrap_or(0).min(controls.len());
        if delivered == 0 {
            return Ok(result);
        }
        let now = self.observe_socket_receive(guest, call.fd()).await?;
        let mut timestamped = Vec::new();
        for (address, length) in controls.into_iter().take(delivered).flatten() {
            let address: AddrMut<'_, u8> = AddrMut::from_raw(address).ok_or(Errno::EFAULT)?;
            let mut bytes = vec![0; length];
            guest.memory().read_exact(address, &mut bytes)?;
            if !socket_timestamp_messages(&bytes).is_empty() {
                timestamped.push((address, bytes));
            }
        }
        if timestamped.is_empty() {
            return Ok(result);
        }

        for (address, mut bytes) in timestamped {
            canonicalize_socket_timestamps(&mut bytes, now);
            guest.memory().write_exact(address, &bytes)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ep_max_events_matches_linux() {
        // x86_64 packs struct epoll_event into 12 bytes; Linux's
        // EP_MAX_EVENTS is INT_MAX / 12.
        assert_eq!(std::mem::size_of::<libc::epoll_event>(), 12);
        assert_eq!(EP_MAX_EVENTS, 178_956_970);
    }

    #[test]
    fn epoll_host_limit_leaves_the_slots_the_ready_timers_do_not_claim() {
        let events = Some(0x1000_0000_usize);
        assert_eq!(epoll_host_limit(events, 1, 1), Some(0));
        assert_eq!(epoll_host_limit(events, 4, 1), Some(3));
        assert_eq!(epoll_host_limit(events, 4, 4), Some(0));
        assert_eq!(epoll_host_limit(events, 4, 9), Some(0));
        assert_eq!(
            epoll_host_limit(events, EP_MAX_EVENTS, 2),
            Some(EP_MAX_EVENTS - 2)
        );
        // No ready timer claims anything: probe with the guest's maxevents.
        assert_eq!(epoll_host_limit(events, 4, 0), None);
        // Linux rejects these before it waits, so the probe must keep the
        // guest's maxevents to report the same error.
        assert_eq!(epoll_host_limit(events, 0, 1), None);
        assert_eq!(epoll_host_limit(events, -1, 1), None);
        assert_eq!(epoll_host_limit(events, EP_MAX_EVENTS + 1, 1), None);
        assert_eq!(epoll_host_limit(None, 4, 1), None);
        // The whole array must lie below the lowest user address limit.
        let top = EPOLL_EVENTS_ADDRESS_LIMIT as usize;
        assert_eq!(epoll_host_limit(Some(top - 12), 1, 1), Some(0));
        assert_eq!(epoll_host_limit(Some(top - 12), 2, 1), None);
        assert_eq!(epoll_host_limit(Some(usize::MAX - 4), 1, 1), None);
    }

    #[test]
    fn epoll_fill_order_changes_only_when_a_side_was_cut() {
        // The timers went first and the host filled all of its share (or
        // had none): the host goes first next time.
        assert_eq!(next_epoll_fill_order(Some(0), 0, 0), Some(false));
        assert_eq!(next_epoll_fill_order(Some(3), 3, 0), Some(false));
        // The timers went first and the host had less than its share.
        assert_eq!(next_epoll_fill_order(Some(3), 2, 0), None);
        // The host went first and pushed out a ready timer.
        assert_eq!(next_epoll_fill_order(None, 1, 1), Some(true));
        // The host went first and cut nothing, or only timers competed.
        assert_eq!(next_epoll_fill_order(None, 1, 0), None);
        assert_eq!(next_epoll_fill_order(None, 0, 2), None);
    }

    #[test]
    fn zero_timeout_socket_poll_requests_a_strong_one_turn_yield() {
        let request = zero_timeout_poll_request(DetTid::from_raw(17), true);

        assert_eq!(request.resources.len(), 1);
        assert_eq!(
            request.resources.get(&ResourceID::SchedYield),
            Some(&Permission::W)
        );
        assert_eq!(request.fyi, SABRE_LOOPBACK_POLL_YIELD_FYI);
    }

    #[test]
    fn zero_timeout_non_socket_poll_keeps_the_existing_empty_turn() {
        let request = zero_timeout_poll_request(DetTid::from_raw(17), false);

        assert!(request.resources.is_empty());
        assert!(request.fyi.is_empty());
    }

    #[test]
    fn connect_peer_classification_never_overrides_kernel_errors() {
        assert!(connect_result_allows_peer_classification(&Ok(0)));
        assert!(connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EINPROGRESS)
        )));
        assert!(!connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EALREADY)
        )));
        assert!(!connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EBADF)
        )));
        assert!(!connect_result_allows_peer_classification(&Err(
            Error::Errno(Errno::EFAULT)
        )));
    }

    #[test]
    fn ppoll_timeout_uses_timespec_units() {
        assert_eq!(
            ppoll_timeout_duration(Timespec {
                tv_sec: 2,
                tv_nsec: 345_678_901,
            }),
            Ok(Duration::new(2, 345_678_901))
        );
        assert_eq!(
            timespec_from_duration(Duration::new(2, 345_678_901)),
            Timespec {
                tv_sec: 2,
                tv_nsec: 345_678_901,
            }
        );
    }

    #[test]
    fn pselect6_fd_set_lengths_follow_the_raw_linux_abi() {
        assert_eq!(PSELECT6_INTERNAL_MAX_NFDS, 64);
        assert_eq!(pselect6_fd_set_len(-1), Err(Errno::EINVAL));
        assert_eq!(pselect6_fd_set_len(0), Ok(0));
        assert_eq!(pselect6_fd_set_len(1), Ok(8));
        assert_eq!(pselect6_fd_set_len(65), Ok(16));
        assert_eq!(pselect6_fd_set_len(libc::FD_SETSIZE as i32), Ok(128));
    }

    #[test]
    fn pselect6_probe_result_maps_only_erestartsys_to_eintr() {
        assert_eq!(
            pselect6_probe_result(Err(Errno::ERESTARTSYS)),
            Err(Errno::EINTR)
        );

        assert_eq!(pselect6_probe_result(Ok(0)), Ok(0));
        assert_eq!(pselect6_probe_result(Ok(1)), Ok(1));
        assert_eq!(pselect6_probe_result(Err(Errno::EBADF)), Err(Errno::EBADF));
        assert_eq!(
            pselect6_probe_result(Err(Errno::EINVAL)),
            Err(Errno::EINVAL)
        );
        assert_eq!(pselect6_probe_result(Err(Errno::EINTR)), Err(Errno::EINTR));
    }

    #[test]
    fn ppoll_signal_mask_keeps_reverie_preemption_unblocked() {
        let preemption_bit = 1_u64 << ((reverie::PERF_EVENT_SIGNAL as usize) - 1);
        assert_eq!(sanitize_ppoll_signal_mask(u64::MAX), !preemption_bit);
    }

    #[test]
    fn ppoll_record_replay_masked_waits_keep_fail_closed_probe() {
        assert!(!ppoll_uses_kernel_wait(true, true, true));
        assert!(ppoll_uses_kernel_wait(true, true, false));
        assert!(!ppoll_uses_kernel_wait(true, false, true));
        assert!(ppoll_uses_kernel_wait(false, true, true));
    }

    #[test]
    fn ppoll_timeout_rejects_invalid_timespecs() {
        assert_eq!(
            ppoll_timeout_duration(Timespec {
                tv_sec: -1,
                tv_nsec: 0,
            }),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            ppoll_timeout_duration(Timespec {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            }),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn socket_timestamp_control_messages_use_logical_time() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let timeval_len = std::mem::size_of::<libc::timeval>();
        let timespec_len = std::mem::size_of::<libc::timespec>();
        let first_len = header_len + timeval_len;
        let second_offset = cmsg_align(first_len);
        let second_len = header_len + timespec_len;
        let third_offset = second_offset + cmsg_align(second_len);
        let third_len = header_len + std::mem::size_of::<i32>();
        let mut control = vec![0; third_offset + cmsg_align(third_len)];

        assert!(write_control_value(
            &mut control,
            libc::cmsghdr {
                cmsg_len: first_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMP_OLD,
            }
        ));
        assert!(write_control_value(
            &mut control[header_len..],
            libc::timeval {
                tv_sec: 99,
                tv_usec: 88,
            }
        ));
        assert!(write_control_value(
            &mut control[second_offset..],
            libc::cmsghdr {
                cmsg_len: second_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMPNS_OLD,
            }
        ));
        assert!(write_control_value(
            &mut control[second_offset + header_len..],
            libc::timespec {
                tv_sec: 77,
                tv_nsec: 66,
            }
        ));
        assert!(write_control_value(
            &mut control[third_offset..],
            libc::cmsghdr {
                cmsg_len: third_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: libc::SCM_RIGHTS,
            }
        ));
        assert!(write_control_value(
            &mut control[third_offset + header_len..],
            42_i32
        ));
        let unrelated_message = control[third_offset..].to_vec();

        assert_eq!(
            canonicalize_socket_timestamps(&mut control, LogicalTime::from_nanos(2_345_678_901)),
            2
        );
        let timeval = read_control_value::<libc::timeval>(&control[header_len..]).unwrap();
        assert_eq!(timeval.tv_sec, 2);
        assert_eq!(timeval.tv_usec, 345_678);
        let timespec =
            read_control_value::<libc::timespec>(&control[second_offset + header_len..]).unwrap();
        assert_eq!(timespec.tv_sec, 2);
        assert_eq!(timespec.tv_nsec, 345_678_901);
        assert_eq!(control[third_offset..], unrelated_message);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    #[test]
    fn scm_rights_fds_names_the_descriptors_each_rights_message_sends() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let int_len = std::mem::size_of::<i32>();
        // Two descriptors, then a timestamp message, then one descriptor.
        let first_len = header_len + 2 * int_len;
        let second_offset = cmsg_align(first_len);
        let second_len = header_len + std::mem::size_of::<libc::timeval>();
        let third_offset = second_offset + cmsg_align(second_len);
        let third_len = header_len + int_len;
        let mut control = vec![0; third_offset + cmsg_align(third_len)];
        let header = |cmsg_len, cmsg_level, cmsg_type| libc::cmsghdr {
            cmsg_len,
            cmsg_level,
            cmsg_type,
        };
        assert!(write_control_value(
            &mut control,
            header(first_len, libc::SOL_SOCKET, libc::SCM_RIGHTS)
        ));
        assert!(write_control_value(&mut control[header_len..], 7_i32));
        assert!(write_control_value(
            &mut control[header_len + int_len..],
            9_i32
        ));
        assert!(write_control_value(
            &mut control[second_offset..],
            header(second_len, libc::SOL_SOCKET, SCM_TIMESTAMP_OLD)
        ));
        assert!(write_control_value(
            &mut control[third_offset..],
            header(third_len, libc::SOL_SOCKET, libc::SCM_RIGHTS)
        ));
        assert!(write_control_value(
            &mut control[third_offset + header_len..],
            11_i32
        ));
        assert_eq!(scm_rights_fds(&control), vec![7, 9, 11]);

        // The same type number at another level sends no descriptors.
        let mut other_level = control.clone();
        assert!(write_control_value(
            &mut other_level,
            header(first_len, libc::IPPROTO_IP, libc::SCM_RIGHTS)
        ));
        assert_eq!(scm_rights_fds(&other_level), vec![11]);

        // A header shorter than itself, or one that runs past the buffer, is
        // the kernel's EINVAL: the walk stops there.
        let mut short = control.clone();
        assert!(write_control_value(
            &mut short[second_offset..],
            header(header_len - 1, libc::SOL_SOCKET, SCM_TIMESTAMP_OLD)
        ));
        assert_eq!(scm_rights_fds(&short), vec![7, 9]);
        let mut long = control.clone();
        assert!(write_control_value(
            &mut long[third_offset..],
            header(third_len + 64, libc::SOL_SOCKET, libc::SCM_RIGHTS)
        ));
        assert_eq!(scm_rights_fds(&long), vec![7, 9]);

        // A buffer too short for one header sends nothing.
        assert!(scm_rights_fds(&control[..header_len - 1]).is_empty());
    }

    #[test]
    fn truncated_timestamp_payload_prefix_is_rewritten() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let full_len = header_len + std::mem::size_of::<libc::timeval>();
        let mut control = vec![0xaa; header_len + std::mem::size_of::<i32>()];
        assert!(write_control_value(
            &mut control,
            libc::cmsghdr {
                cmsg_len: full_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMP_OLD,
            }
        ));

        assert_eq!(
            canonicalize_socket_timestamps(&mut control, LogicalTime::from_nanos(2_345_678_901)),
            1
        );
        assert_eq!(
            read_control_value::<i32>(&control[header_len..]),
            Some(2),
            "the visible timeval prefix must not retain host seconds"
        );
    }

    #[test]
    fn timestamping_preserves_populated_source_slots() {
        let header_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
        let timespec_len = std::mem::size_of::<libc::timespec>();
        let message_len = header_len + 3 * timespec_len;
        let mut control = vec![0; cmsg_align(message_len)];
        assert!(write_control_value(
            &mut control,
            libc::cmsghdr {
                cmsg_len: message_len,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: SCM_TIMESTAMPING_OLD,
            }
        ));
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let populated = libc::timespec {
            tv_sec: 99,
            tv_nsec: 88,
        };
        assert!(write_control_value(&mut control[header_len..], zero));
        assert!(write_control_value(
            &mut control[header_len + timespec_len..],
            populated
        ));
        assert!(write_control_value(
            &mut control[header_len + 2 * timespec_len..],
            populated
        ));

        assert_eq!(
            canonicalize_socket_timestamps(&mut control, LogicalTime::from_nanos(2_345_678_901)),
            1
        );
        let first = read_control_value::<libc::timespec>(&control[header_len..]).unwrap();
        let second =
            read_control_value::<libc::timespec>(&control[header_len + timespec_len..]).unwrap();
        let third = read_control_value::<libc::timespec>(&control[header_len + 2 * timespec_len..])
            .unwrap();
        assert_eq!((first.tv_sec, first.tv_nsec), (0, 0));
        assert_eq!((second.tv_sec, second.tv_nsec), (2, 345_678_901));
        assert_eq!((third.tv_sec, third.tv_nsec), (2, 345_678_901));
    }
}
