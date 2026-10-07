/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with the file system.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use rand::RngExt as _;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::AddrSlice;
use reverie::syscalls::AddrSliceMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd::*;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::SockFlag;
use reverie::syscalls::StatPtr;
use reverie::syscalls::StatxMask;
use reverie::syscalls::StatxPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use reverie::syscalls::Whence;
use reverie::syscalls::family::StatFamily;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use super::deterministic_stdio_inode_for_resource;
use crate::IdentityLookupRefused;
use crate::config::SchedHeuristic;
use crate::dirents::*;
use crate::fd::*;
use crate::procfs::MappingSubject;
use crate::procfs::MountInfoSnapshot;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsSnapshotContext;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Device;
use crate::resources::HOST_TIMED_INTERNAL_PIPE_IO_FYI;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::HostTimedSignalScope;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::terminal_signals;
use crate::stat::*;
use crate::syscalls::threads::kernel_sigset_bit;
use crate::tool_global::*;
use crate::tool_local::CapturedDetFdInstallError;
use crate::tool_local::Detcore;
use crate::tool_local::finish_partial_record_or_replay_write;
use crate::types::*;

/// A conversion from SOCK_* flags to O_* flags which makes unsafe (but checked during testing) assumptions.
fn oflag_from_sock_bits(s_bits: i32) -> OFlag {
    // An otherwise unsafe "cast" which leans on the `linux_flags_assumptions` below.
    OFlag::from_bits_truncate(s_bits & (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK))
}

const UNIX_AUTOBIND_NAME_LEN: usize = 6;
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-2150): Review timer-slack procfs parsing,
// per-operation target checks, and scalar/vector I/O emulation.
const TIMER_SLACK_PARSE_BYTES: usize = 66;
#[derive(Clone, Copy)]
struct TimerSlackIovec {
    base: usize,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimerSlackBinding {
    target: i32,
    device: u64,
    inode: u64,
}

fn classify_timer_slack_binding(
    binding: TimerSlackBinding,
    observed_identity: Option<(u64, u64)>,
    current_tid: i32,
) -> Result<(), Errno> {
    if observed_identity != Some((binding.device, binding.inode)) {
        // The bound task exited. A missing path and a new task that recycled
        // the same numeric TID are both ESRCH for the old open inode.
        return Err(Errno::ESRCH);
    }
    if current_tid != binding.target {
        // Cross-task CAP_SYS_NICE access is intentionally not exposed.
        return Err(Errno::EPERM);
    }
    Ok(())
}

fn parse_timer_slack_write(bytes: &[u8]) -> Result<u64, Errno> {
    // `kstrtoull_from_user` copies at most sign + 64 binary digits + newline,
    // then accepts decimal digits with one optional leading '+' and one
    // optional trailing newline. An embedded NUL terminates the C string.
    let bytes = &bytes[..bytes.len().min(TIMER_SLACK_PARSE_BYTES)];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let mut value = &bytes[..end];
    if value.first() == Some(&b'+') {
        value = &value[1..];
    }
    if value.last() == Some(&b'\n') {
        value = &value[..value.len() - 1];
    }
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(Errno::EINVAL);
    }
    value.iter().try_fold(0_u64, |parsed, digit| {
        parsed
            .checked_mul(10)
            .and_then(|parsed| parsed.checked_add(u64::from(digit - b'0')))
            .ok_or(Errno::ERANGE)
    })
}

fn vectored_offset(low: u64, high: u64) -> i64 {
    if std::mem::size_of::<usize>() == 8 {
        low as i64
    } else {
        ((high << 32) | (low & u32::MAX as u64)) as i64
    }
}

/// RNG vectors use either the shared stream or an independent explicit offset.
struct RandomVectoredRead {
    address: usize,
    count: usize,
    offset: Option<u64>,
    flags: i32,
}

/// Apply Linux's checks after complete iovec import, before touching output.
fn validate_random_vector_read(offset: Option<u64>, total: usize, flags: i32) -> Result<(), Errno> {
    if total == 0 {
        return Ok(());
    }
    if let Some(offset) = offset
        && offset
            .checked_add(total as u64)
            .is_none_or(|end| end > i64::MAX as u64)
    {
        return Err(Errno::EINVAL);
    }
    // Linux 7.1's RWF_NOSIGNAL is newer than the pinned libc crate. Native
    // random-device controls accept it, including with RWF_NOWAIT.
    const RWF_NOSIGNAL: i32 = 0x100;
    const KNOWN: i32 = libc::RWF_HIPRI
        | libc::RWF_DSYNC
        | libc::RWF_SYNC
        | libc::RWF_NOWAIT
        | libc::RWF_APPEND
        | libc::RWF_NOAPPEND
        | libc::RWF_ATOMIC
        | libc::RWF_DONTCACHE
        | RWF_NOSIGNAL;
    // Unknown flags win over conflicting recognized flags, while recognized
    // but unsupported ATOMIC/DONTCACHE are rejected after APPEND/NOAPPEND.
    if flags & !KNOWN != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if flags & libc::RWF_APPEND != 0 && flags & libc::RWF_NOAPPEND != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & (libc::RWF_ATOMIC | libc::RWF_DONTCACHE) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(())
}

fn read_iovecs<M: MemoryAccess>(
    memory: &M,
    address: Option<Addr<libc::iovec>>,
    count: usize,
) -> Result<Vec<TimerSlackIovec>, Errno> {
    if count > libc::UIO_MAXIOV as usize {
        return Err(Errno::EINVAL);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let address = address.ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        count
    ];
    memory.read_values(address, &mut iovecs)?;
    let total = iovecs.iter().try_fold(0_usize, |total, iovec| {
        total.checked_add(iovec.iov_len).ok_or(Errno::EINVAL)
    })?;
    if total > isize::MAX as usize {
        return Err(Errno::EINVAL);
    }
    Ok(iovecs
        .into_iter()
        .map(|iovec| TimerSlackIovec {
            base: iovec.iov_base as usize,
            len: iovec.iov_len,
        })
        .collect())
}

fn copy_timer_slack_output<M: MemoryAccess>(
    memory: &mut M,
    destination: Option<AddrMut<'_, u8>>,
    bytes: &[u8],
) -> Result<usize, Errno> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let copied = memory.write(destination.ok_or(Errno::EFAULT)?, bytes)?;
    if copied == 0 {
        Err(Errno::EFAULT)
    } else {
        Ok(copied)
    }
}

/// Copy sanitized procfs bytes to the caller's buffer as the kernel's
/// `copy_to_user` does: through the guest's page protections, stopping at the
/// first byte that cannot be written. Returns the bytes copied, or `EFAULT`
/// when none could be. A debugger write is not equivalent: the ptrace
/// backend writes exactly eight bytes with `PTRACE_POKEDATA`, which ignores
/// `PROT_READ` and `PROT_NONE`.
fn copy_procfs_output<M: MemoryAccess>(
    memory: &mut M,
    destination: Option<AddrMut<'_, u8>>,
    bytes: &[u8],
) -> Result<usize, Error> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let destination = destination.ok_or(Errno::EFAULT)?;
    let mut copied = 0;
    while copied < bytes.len() {
        let Some(address) = destination
            .as_raw()
            .checked_add(copied)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        match memory.write_with_user_access(address, &bytes[copied..]) {
            Ok(0) | Err(Errno::EFAULT) => break,
            Ok(n) => copied += n,
            // Any other failure is the backend's, not a guest fault: report it
            // as a tool error and commit nothing.
            Err(errno) => {
                return Err(Error::Tool(anyhow::anyhow!(
                    "procfs read publication failed after {copied} bytes: {errno}"
                )));
            }
        }
    }
    if copied == 0 {
        Err(Errno::EFAULT.into())
    } else {
        Ok(copied)
    }
}

/// Check the caller's whole requested range as `vfs_read`'s `access_ok` does
/// before the file is read, so an address range beyond the user address limit
/// is `EFAULT` even at EOF, when nothing would be copied. A procfs snapshot read
/// never hands that range to the kernel.
///
/// `limit` is asked for the guest's user address limit only when the answer
/// can matter, so a count that no limit admits is `EFAULT` before any query
/// can fail.
fn validate_procfs_destination(
    limit: impl FnOnce() -> Result<crate::iovecs::UserAddressLimit, Error>,
    destination: Option<AddrMut<'_, u8>>,
    len: usize,
) -> Result<(), Error> {
    // No count past isize::MAX fits below a user address limit, so `read`'s
    // `access_ok` refuses it, whatever the limit.
    if isize::try_from(len).is_err() {
        return Err(Errno::EFAULT.into());
    }
    // Two descriptors keep the range uncapped, as `access_ok` sees it; a
    // single one may be capped at MAX_RW_COUNT first (`import_ubuf`).
    limit()?.validate(&[
        crate::iovecs::ImportedIovec {
            base: destination.map_or(0, |address| address.as_raw()),
            len,
        },
        crate::iovecs::ImportedIovec { base: 0, len: 0 },
    ])
}

/// Save the bytes at `scratch` into `original` if the guest's own protections
/// let the capture read land there: the range is readable, and writing the
/// same bytes back through the guest's protections copies all of them.
fn save_procfs_scratch<M: MemoryAccess>(
    memory: &mut M,
    scratch: AddrMut<'_, u8>,
    original: &mut [u8],
) -> bool {
    read_guest_exact(memory, scratch, original).is_ok()
        && memory
            .write_with_user_access(scratch, original)
            .is_ok_and(|written| written == original.len())
}

/// Read exactly `buf.len()` bytes at `addr`, in one `read_vectored` of that
/// length, or fail `EFAULT` if not all of them can be read. `read` and
/// `read_exact` may widen a short read (ptrace turns up to eight bytes into one
/// eight-byte `PTRACE_PEEKDATA`), which fails when a valid extent ends less
/// than eight bytes before an unmapped page.
pub(crate) fn read_guest_exact<M: MemoryAccess>(
    memory: &M,
    addr: AddrMut<'_, u8>,
    buf: &mut [u8],
) -> Result<(), Errno> {
    let len = buf.len();
    let remote = unsafe { AddrSlice::from_raw_parts(addr.into(), len) };
    let remote = [unsafe { remote.as_ioslice() }];
    let mut local = [std::io::IoSliceMut::new(buf)];
    match memory.read_vectored(&remote, &mut local) {
        Ok(copied) if copied == len => Ok(()),
        Ok(_) => Err(Errno::EFAULT),
        Err(errno) => Err(errno),
    }
}

/// Capacity used for pipes that Detcore makes physically nonblocking.
///
/// Linux normally creates 64-KiB pipes on this platform, but silently falls back to two pages
/// once the creating UID crosses `pipe-user-pages-soft`. That host-global accounting can change
/// between the two executions of `hermit run --verify`, changing whether the same write succeeds
/// immediately or enters the scheduler's `InternalIOPolling` retry path. Two pages is the
/// pressure-mode capacity on supported x86-64 Linux hosts and, unlike 64 KiB, never requires an
/// unprivileged capacity increase while the soft limit is active.
///
/// This is also the ceiling the guest is allowed to raise a pipe to, and the
/// value `/proc/sys/fs/pipe-max-size` reports. Those three must be ONE constant:
/// a pinned capacity, an advertised maximum and an enforced maximum that can
/// drift apart are three copies of one rule, and a duplicated rule drifts in
/// N-1 places while each copy looks right on its own.
pub(crate) const DETERMINISTIC_PIPE_CAPACITY_BYTES: i32 = 8 * 1024;

/// Whether a guest's `F_SETPIPE_SZ` request must be refused as a growth past
/// the deterministic ceiling.
///
/// Separated from the handler so the BOUNDARY is testable without a guest. The
/// boundary is the whole content of this rule: a request for exactly the pinned
/// capacity must be allowed, or hermit's own pin value becomes unreachable to a
/// guest that reads `/proc/sys/fs/pipe-max-size` and asks for precisely what it
/// was told.
pub(crate) fn pipe_capacity_request_exceeds_ceiling(requested: i32) -> bool {
    requested > DETERMINISTIC_PIPE_CAPACITY_BYTES
}

/// SIGIO and SIGURG, the signals Linux sends to a descriptor's owner when
/// data, out-of-band data, a lease break, or a directory change arrives.
fn async_io_signals() -> u64 {
    kernel_sigset_bit(libc::SIGIO) | kernel_sigset_bit(libc::SIGURG)
}

/// The signals an `fcntl` command lets Linux send at a moment set by host
/// timing, as a kernel sigset; 0 for every other command. `F_SETOWN` and
/// `F_SETOWN_EX` name the owner, `F_SETLEASE` and `F_NOTIFY` make the caller
/// the owner, `O_ASYNC` enables SIGIO, and `F_SETSIG` replaces SIGIO with
/// another signal.
fn fcntl_host_timed_signals(cmd: syscalls::FcntlCmd<'_>) -> u64 {
    match cmd {
        F_SETFL(flags) if flags & libc::O_ASYNC != 0 => async_io_signals(),
        F_SETOWN | F_SETOWN_EX(_) | F_SETLEASE(_) | F_NOTIFY(_) => async_io_signals(),
        F_SETSIG(signal) => async_io_signals() | kernel_sigset_bit(signal),
        _ => 0,
    }
}

/// The signals an `ioctl` request lets Linux send at a moment set by host
/// timing, as a kernel sigset; 0 for every other request. `FIOASYNC` enables
/// SIGIO, and `FIOSETOWN` and `SIOCSPGRP` name the owner. `TIOCSCTTY` makes
/// the terminal the caller's controlling terminal, `TIOCGPTPEER` without
/// `O_NOCTTY` opens a pseudoterminal's other end as `open` does and so can
/// make it one (`open_can_acquire_controlling_terminal`), and `TIOCSPGRP`
/// names the terminal's foreground process group, so the terminal can then
/// signal the session and that group (`terminal_signals`). `TIOCNOTTY` and
/// `TIOCSWINSZ` are not among them: the signals they send, they send inside
/// the caller's call, in the caller's turn.
fn ioctl_host_timed_signals(request: syscalls::ioctl::Request<'_>) -> u64 {
    match request {
        syscalls::ioctl::Request::FIOASYNC(_)
        | syscalls::ioctl::Request::FIOSETOWN(_)
        | syscalls::ioctl::Request::SIOCSPGRP(_) => async_io_signals(),
        syscalls::ioctl::Request::TIOCSCTTY(_) | syscalls::ioctl::Request::TIOCSPGRP(_) => {
            terminal_signals()
        }
        syscalls::ioctl::Request::TIOCGPTPEER(flags)
            if open_can_acquire_controlling_terminal(OFlag::from_bits_truncate(flags)) =>
        {
            terminal_signals()
        }
        _ => 0,
    }
}

/// Whether an `open` with these flags can make the terminal it opens the
/// caller's controlling terminal. Linux's `tty_open` does that for a session
/// leader that has none, unless `O_NOCTTY` is given; an `O_PATH` open never
/// reaches `tty_open`.
fn open_can_acquire_controlling_terminal(flags: OFlag) -> bool {
    !flags.intersects(OFlag::O_NOCTTY | OFlag::O_PATH)
}

/// Whether the character device `rdev` is a terminal that an `open` can make
/// its caller's controlling terminal. `tty_drivers` is the text of
/// `/proc/tty/drivers`, one line per range of device numbers a terminal
/// driver serves, ending in the major number, the minor number or range, and
/// the driver's type. A device no line names is not a terminal. Linux's
/// `tty_open` never makes `/dev/console` (5:1), `/dev/vc/0` (4:0) or a
/// pseudoterminal master controlling, and `/dev/ptmx` (5:2) opens a master
/// without reaching `tty_open`. `/dev/tty` (5:0) stays in: it reopens the
/// caller's controlling terminal, and if a hangup clears that terminal during
/// the `open`, the `open` can make it controlling again. A table that could
/// not be read (`None`), or a line that does not parse when no other line
/// names the device, answers true.
fn terminal_can_become_controlling(rdev: libc::dev_t, tty_drivers: Option<&str>) -> bool {
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    if matches!((major, minor), (5, 1) | (5, 2) | (4, 0)) {
        return false;
    }
    let Some(table) = tty_drivers else {
        return true;
    };
    let mut unparsed = false;
    for line in table.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let range = match fields[..] {
            [.., line_major, minors, kind] => {
                line_major.parse::<u32>().ok().and_then(|line_major| {
                    let (first, last) = minors.split_once('-').unwrap_or((minors, minors));
                    Some((
                        line_major,
                        first.parse::<u32>().ok()?,
                        last.parse::<u32>().ok()?,
                        kind,
                    ))
                })
            }
            _ => None,
        };
        match range {
            Some((line_major, first, last, kind))
                if line_major == major && (first..=last).contains(&minor) =>
            {
                return kind != "pty:master";
            }
            Some(_) => {}
            None => unparsed = true,
        }
    }
    unparsed
}

/// Whether an `open` in process `pid`, whose `/proc/<pid>/stat` lists
/// `session`, of the character device `rdev` may have made that device
/// `pid`'s controlling terminal: `pid` leads its session and the device is a
/// terminal that can become controlling (`terminal_can_become_controlling`).
/// Neither fact is the controlling terminal procfs lists, which a hangup of
/// the terminal clears at any moment, so the answer does not depend on
/// whether a hangup came before or after the read. A session leader that
/// reopens the terminal it already controls, or opens a terminal while it
/// controls another, also answers true.
fn may_have_acquired_controlling_terminal(
    pid: i32,
    session: i32,
    rdev: libc::dev_t,
    tty_drivers: Option<&str>,
) -> bool {
    session == pid && terminal_can_become_controlling(rdev, tty_drivers)
}

/// Whether the `open` in process `pid` that returned `fd` may have made the
/// opened terminal `pid`'s controlling terminal
/// (`may_have_acquired_controlling_terminal`). `host_stat` is the
/// descriptor's stat when the caller already read it. Every host read that
/// fails answers true: a needless record only holds the terminal's signals
/// (`terminal_signals`) to the end of the gated waits that follow, while a
/// missed one lets the terminal interrupt one at a turn the host chose.
fn opened_controlling_terminal(pid: i32, fd: RawFd, host_stat: Option<&libc::stat>) -> bool {
    let rdev = match host_stat {
        Some(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFCHR => stat.st_rdev,
        Some(_) => return false,
        None => match std::fs::metadata(format!("/proc/{pid}/fd/{fd}")) {
            Ok(metadata) if metadata.file_type().is_char_device() => metadata.rdev(),
            Ok(_) => return false,
            Err(_) => return true,
        },
    };
    let session = match procfs::process::Process::new(pid).and_then(|process| process.stat()) {
        Ok(stat) => stat.session,
        Err(_) => return true,
    };
    let tty_drivers = std::fs::read_to_string("/proc/tty/drivers").ok();
    may_have_acquired_controlling_terminal(pid, session, rdev, tty_drivers.as_deref())
}

/// Why the pin failed, and which descriptors Linux had already created when it did.
///
/// `pipe2` has already SUCCEEDED by the time the capacity is pinned, so the two descriptors
/// exist in the guest whatever happens next. Carrying them alongside the errno is what makes
/// releasing them possible; classifying separately from acting on it is what makes the
/// classification unit-testable without a guest.
#[derive(Debug, PartialEq, Eq)]
struct PipeCapacityFailure {
    created_fds: [i32; 2],
    error: Errno,
}

impl PipeCapacityFailure {
    fn close_syscalls(&self) -> [syscalls::Close; 2] {
        self.created_fds
            .map(|fd| syscalls::Close::new().with_fd(fd))
    }
}

/// Classify the result of pinning a pipe's capacity.
///
/// The ONLY success shape is Linux returning exactly the requested capacity. `F_SETPIPE_SZ`
/// returns the capacity it actually applied, and it may round; a rounded value is a pipe whose
/// size we did not choose, which is the host-dependent capacity this path exists to remove. It
/// is not a kernel errno, so it is reported as `EIO` rather than dressed up as one.
fn pipe_capacity_failure(
    created_fds: [i32; 2],
    capacity_result: Result<i64, Errno>,
) -> Option<PipeCapacityFailure> {
    let error = match capacity_result {
        Ok(applied) if applied == i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES) => return None,
        Ok(_) => Errno::EIO,
        Err(error) => error,
    };
    Some(PipeCapacityFailure { created_fds, error })
}

fn should_tag_host_timed_internal_pipe_io(
    internal_pipe_turns_are_host_timed: bool,
    fd_type: FdType,
    physically_nonblocking: bool,
    logically_nonblocking: bool,
) -> bool {
    internal_pipe_turns_are_host_timed
        && fd_type == FdType::Pipe
        && physically_nonblocking
        && !logically_nonblocking
}

fn random_device_lseek_result(status_flags: i32, whence: Whence) -> Result<i64, Errno> {
    if status_flags & OFlag::O_PATH.bits() != 0 {
        return Err(Errno::EBADF);
    }
    match whence {
        Whence::SEEK_SET
        | Whence::SEEK_CUR
        | Whence::SEEK_END
        | Whence::SEEK_DATA
        | Whence::SEEK_HOLE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

fn require_random_device_read_access(status_flags: i32) -> Result<(), Errno> {
    if status_flags & libc::O_PATH != 0
        || !matches!(
            status_flags & libc::O_ACCMODE,
            libc::O_RDONLY | libc::O_RDWR
        )
    {
        Err(Errno::EBADF)
    } else {
        Ok(())
    }
}

/// Inherited container output is a stream even when an outer runner stores it
/// in a seekable file.  The backing file also carries Hermit's own diagnostics,
/// so exposing its live offset makes tool logging guest-visible.  Preserve real
/// file semantics after a guest replaces stdout/stderr: `dup2(file, 1)` copies
/// the file's resource rather than this container-output resource.
fn is_inherited_container_output(resource: Option<ResourceID>) -> bool {
    matches!(
        resource,
        Some(ResourceID::Device(
            Device::ContainerStdout | Device::ContainerStderr
        ))
    )
}

/// Whether a descriptor carries an inherited container stdio resource: one of
/// descriptors 0-2 as the guest received them, or a dup of one. Its cached
/// stat is the tracer's `fstat(0)` stand-in, not the descriptor's own.
fn is_container_stdio(resource: Option<ResourceID>) -> bool {
    matches!(
        resource,
        Some(ResourceID::Device(
            Device::ContainerStdin | Device::ContainerStdout | Device::ContainerStderr
        ))
    )
}

/// Where `handle_mmap` takes the identity it records for a file mapping.
enum MappedFileIdentity {
    /// No file identity: an anonymous mapping, an untracked descriptor, or one
    /// with no cached stat.
    None,
    /// The descriptor's cached stat.
    Cached(RawInode),
    /// A stdio descriptor, whose cached stat is a stand-in.
    Stdio,
}

fn unix_autobind_addrlen() -> i32 {
    (std::mem::offset_of!(libc::sockaddr_un, sun_path) + UNIX_AUTOBIND_NAME_LEN) as i32
}

fn unix_autobind_address(port: u16) -> libc::sockaddr_un {
    // Linux autobind names are a leading NUL followed by five lowercase hex digits.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path[1..UNIX_AUTOBIND_NAME_LEN]
        .iter_mut()
        .zip(format!("{port:05x}").bytes())
    {
        *destination = source as libc::c_char;
    }
    address
}

// TODO-HUMAN-REVIEW(PR-904): Review the TCP_INFO compatibility boundary.
/// Retain the logical TCP state and negotiated option header while hiding all
/// host timing, rate, packet, and byte counters.
fn canonicalize_tcp_info(info: &mut [u8]) {
    for (offset, byte) in info.iter_mut().enumerate() {
        if !matches!(offset, 0 | 1 | 5 | 6) {
            *byte = 0;
        }
    }
}

// Hermit exposes exactly one isolated guest network namespace.
const DETERMINISTIC_NETNS_COOKIE: u64 = 1;

// Above Linux's PID range and below the high-bit IDs used by kernel autobind.
const DETERMINISTIC_NETLINK_PORT_ID_BASE: u32 = 0x4000_0000;

/// Does the new guest descriptor `fd` reopen an anonymous pipe that the same
/// process already holds as a scheduler-managed pipe (`managed_pipe_fds`)?
///
/// Both `/proc/<pid>/fd/<fd>` links read `pipe:[<inode>]` for the same pipe.
/// A named FIFO links to its path, and on ptrace a host pipe is never in
/// `managed_pipe_fds` (see `scheduler_managed_pipe_fds` for SaBRe), so neither
/// matches: a host writer is outside the scheduler and must not be polled as if
/// it were a guest
/// (<https://github.com/rrnewton/hermit/pull/3534#issuecomment-5962369261>).
fn reopens_scheduler_managed_pipe(pid: i32, fd: RawFd, managed_pipe_fds: &[RawFd]) -> bool {
    let link = |fd: RawFd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok();
    let Some(opened) = link(fd) else {
        return false;
    };
    if !opened
        .to_str()
        .is_some_and(|name| name.starts_with("pipe:["))
    {
        return false;
    }
    managed_pipe_fds
        .iter()
        .any(|&held| held != fd && link(held).as_ref() == Some(&opened))
}

/// The path the kernel resolved an open descriptor to, read from the guest's
/// own `/proc/<pid>/fd/<fd>` link. This is the evidence authority for "which
/// object was opened": it is produced by the kernel from the descriptor itself,
/// so it is independent of the pathname spelling the guest used.
///
/// Used ONLY as a fallback for a pathname that does not classify on its own
/// (see the call site): a spelling that already classifies must keep its own
/// classification, because `/proc/self/...` and `/proc/thread-self/...` are
/// defined by the spelling and resolve to a different numeric path.
///
/// `None` when the link cannot be read (the descriptor is gone, or procfs is
/// unavailable), in which case the lexical result stands unchanged.
fn resolved_open_path(pid: i32, fd: RawFd) -> Option<PathBuf> {
    let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
    // A deleted or anonymous target is not a stable object name.
    link.is_absolute().then_some(link)
}

/// The host's default huge page size (`Hugepagesize` in /proc/meminfo), the
/// size of a MAP_HUGETLB mapping that names none, or 2 MiB, the x86_64
/// default, when it cannot be read (see [`may_change_untraced_code`]).
fn default_huge_page_size() -> u64 {
    static SIZE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| {
                let line = meminfo
                    .lines()
                    .find(|line| line.starts_with("Hugepagesize:"))?;
                let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                kib.checked_mul(1024)
            })
            .unwrap_or(2 << 20)
    })
}

/// What a guest descriptor is, read through the opening thread's
/// `/proc/<tid>/fd/<fd>`: its link as the kernel spells it, and whether the
/// file is on procfs (see [`may_write_process_memory`]). Read through the
/// thread because its descriptor table may not be its leader's (a thread
/// cloned without CLONE_FILES). `None` when either cannot be read.
fn descriptor_identity(tid: i32, fd: RawFd) -> Option<(PathBuf, bool)> {
    let path = format!("/proc/{tid}/fd/{fd}");
    let link = std::fs::read_link(&path).ok()?;
    let path = std::ffi::CString::new(path).ok()?;
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `buf` is large enough for statfs.
    if unsafe { libc::statfs(path.as_ptr(), buf.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statfs succeeded, so it filled `buf`.
    let on_procfs = unsafe { buf.assume_init() }.f_type == libc::PROC_SUPER_MAGIC;
    Some((link, on_procfs))
}

/// Resolve an `AT_FDCWD`-relative spelling in the guest's filesystem view.
///
/// Replayer chroots the guest, so the tracer-visible cwd includes the replay
/// root. Stripping `/proc/<pid>/root` produces the same guest-absolute path in
/// record and replay without depending on the opened descriptor (which is an
/// eventfd placeholder during replay).
fn resolved_at_fdcwd_path(pid: i32, path: &Path) -> Option<PathBuf> {
    debug_assert!(!path.is_absolute());
    let root = std::fs::read_link(format!("/proc/{pid}/root")).ok()?;
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let guest_cwd = cwd.strip_prefix(root).ok()?;
    Some(Path::new("/").join(guest_cwd).join(path))
}

/// Whether removing a name of the file `stat` describes leaves it with no
/// name: a directory has only one, and a non-directory with one link loses
/// its last. A host may then give its inode to another file
/// (<https://github.com/rrnewton/hermit/issues/3840>).
fn retires_on_removal(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR || stat.st_nlink == 1
}

/// The path to report for one operand of a successful namespace change (see
/// `Detcore::record_host_namespace_change`): the operand made absolute, or
/// `/` when it could not be read or made absolute. A tracer that may not read
/// a non-dumpable guest's memory, for one, cannot read it while the change
/// itself succeeds. `/` is an ancestor of every path, so no host input change
/// is named in that run, rather than one the guest may have made.
fn rebound_path(observed: Option<PathBuf>) -> String {
    match observed {
        Some(path) if path.is_absolute() => path.to_string_lossy().into_owned(),
        _ => String::from("/"),
    }
}

/// Whether the raw call `number(args)` may change the guest memory in
/// `range`, `[start, end)` (see `Config::untraced_code_range`):
/// - a fixed mapping over any part of it: `mmap` with MAP_FIXED, `mremap`
///   with MREMAP_FIXED, `shmat` with SHM_REMAP;
/// - a protection, advice or unmapping over any part of it: `mprotect`,
///   `pkey_mprotect`, `madvise`, `munmap`, the old range of `mremap`.
///
/// Linux takes page-aligned addresses for these calls and rounds a length up
/// to whole pages, and `range` is whole pages, so the unrounded interval
/// overlaps it exactly when the rounded one does. A MAP_HUGETLB `mmap` rounds
/// to its own page size on both ends, so it is widened to the size its
/// MAP_HUGE_* bits name, or else to `default_huge_page` (the host's
/// `Hugepagesize`). A zero length counts as its first byte, and a `shmat` as
/// reaching the end of the address space, since its size is the segment's.
/// A mapping without a fixed address never lands on an existing one, and the
/// guest's `process_vm_writev`, `ptrace` and `remap_file_pages` are refused.
/// Scalar arguments are never read as addresses, so an ordinary call is
/// never counted.
pub(crate) fn may_change_untraced_code(
    number: Sysno,
    args: &syscalls::SyscallArgs,
    (start, end): (u64, u64),
    default_huge_page: u64,
) -> bool {
    let [arg0, arg1, arg2, arg3, arg4] =
        [args.arg0, args.arg1, args.arg2, args.arg3, args.arg4].map(|arg| arg as u64);
    let overlaps = |addr: u64, len: u64| addr < end && addr.saturating_add(len.max(1)) > start;
    let overlaps_huge = |addr: u64, len: u64, page: u64| {
        let page = page.max(1);
        let first = addr - addr % page;
        let last = addr
            .saturating_add(len.max(1))
            .div_ceil(page)
            .saturating_mul(page);
        overlaps(first, last.saturating_sub(first).max(1))
    };
    let huge_page = |flags: u64| match (flags >> libc::MAP_HUGE_SHIFT) & libc::MAP_HUGE_MASK as u64
    {
        0 => default_huge_page,
        shift => 1u64.checked_shl(shift as u32).unwrap_or(u64::MAX),
    };
    match number {
        Sysno::mmap if arg3 & libc::MAP_FIXED as u64 == 0 => false,
        Sysno::mmap if arg3 & libc::MAP_HUGETLB as u64 != 0 => {
            overlaps_huge(arg0, arg1, huge_page(arg3))
        }
        Sysno::mmap => overlaps(arg0, arg1),
        Sysno::mprotect | Sysno::pkey_mprotect | Sysno::madvise | Sysno::munmap => {
            overlaps(arg0, arg1)
        }
        Sysno::mremap => {
            overlaps(arg0, arg1) || (arg3 & libc::MREMAP_FIXED as u64 != 0 && overlaps(arg4, arg2))
        }
        Sysno::shmat => arg1 != 0 && arg2 & libc::SHM_REMAP as u64 != 0 && overlaps(arg1, u64::MAX),
        _ => false,
    }
}

/// Whether an open with `flags` that returned a descriptor may write a
/// process's memory: `/proc/<pid>/mem` and `/proc/<pid>/task/<tid>/mem`
/// write through page protections. `descriptor` is what the descriptor is,
/// read through the opening thread's `/proc/<tid>/fd/<fd>` (see
/// [`descriptor_identity`]): its link, which names the file however the guest
/// reached it (a symlink, a `/proc/self/fd` reopen), and whether it is on
/// procfs. It counts only when it is a procfs file named `mem`, so an
/// ordinary file of that name, or a name that leads to a pipe, does not. A
/// descriptor that cannot be read at all counts, since Detcore cannot
/// establish what it is.
pub(crate) fn may_write_process_memory(flags: OFlag, descriptor: Option<(&Path, bool)>) -> bool {
    let writes = !flags.contains(OFlag::O_PATH) && flags & OFlag::O_ACCMODE != OFlag::O_RDONLY;
    writes
        && descriptor.is_none_or(|(link, on_procfs)| {
            on_procfs && link.file_name() == Some(std::ffi::OsStr::new("mem"))
        })
}

/// Writes back the guest bytes that a pre-call lookup buffer (utimensat's, or
/// the removal lookup's) covered.
fn restore_lookup_buffer<M: MemoryAccess>(memory: &mut M, buffer: StatPtr, saved: &[u8]) {
    if memory.write_exact(buffer.0.cast(), saved).is_err() {
        info!("Could not restore the guest bytes under a pre-call lookup buffer.");
    }
}

/// Whether a lookup `buffer` overlaps `path` through its NUL, which a guest
/// call still has to read. A path that cannot be read counts as overlapping,
/// so that the kernel, not a lookup, reports the fault.
fn path_overlaps_buffer<M: MemoryAccess>(
    memory: &M,
    path: Option<syscalls::PathPtr<'_>>,
    buffer: StatPtr,
) -> bool {
    use reverie::syscalls::FromToRaw;

    let start = buffer.0.as_raw();
    let end = start + std::mem::size_of::<libc::stat>();
    path.is_some_and(|ptr| match ptr.read(memory) {
        Ok(read) => {
            let addr = Some(ptr).into_raw();
            let read: PathBuf = read;
            addr < end && start < addr.saturating_add(read.as_os_str().len() + 1)
        }
        Err(_) => true,
    })
}

/// Whether the utimensat lookup buffer overlaps the guest memory Linux reads
/// for `call`: the path through its NUL and, with `guest_times`, the two
/// timespecs. A path that cannot be read counts as overlapping, so that the
/// kernel, not a lookup, reports the fault.
fn utimensat_input_overlaps<M: MemoryAccess>(
    memory: &M,
    call: &syscalls::Utimensat,
    guest_times: bool,
    buffer: StatPtr,
) -> bool {
    let start = buffer.0.as_raw();
    let end = start + std::mem::size_of::<libc::stat>();
    let overlaps = |addr: usize, len: usize| addr < end && start < addr.saturating_add(len);
    let path = path_overlaps_buffer(memory, call.path(), buffer);
    let times = guest_times
        && call
            .times()
            .is_some_and(|times| overlaps(times.as_raw(), std::mem::size_of::<[Timespec; 2]>()));
    path || times
}

/// The descriptor a `*at` stat call describes by itself: `dirfd`, when the
/// call passes `AT_EMPTY_PATH` with an empty or NULL path (Linux 6.11 accepts
/// NULL). `None` for every call that resolves a path, and for `AT_FDCWD`. A
/// path that cannot be read counts as nonempty, so the call keeps the
/// path-based numbering and the kernel reports the fault. The path is read
/// only after the flag and `dirfd` checks, so an ordinary `stat()` reads no
/// guest memory here.
fn empty_path_fd<M: MemoryAccess>(
    memory: &M,
    dirfd: i32,
    path: Option<PathPtr>,
    flags: AtFlags,
) -> Option<i32> {
    if dirfd < 0 || !flags.contains(AtFlags::AT_EMPTY_PATH) {
        return None;
    }
    let empty = match path {
        None => true,
        Some(path) => path
            .read(memory)
            .is_ok_and(|path| path.as_os_str().is_empty()),
    };
    empty.then_some(dirfd)
}

/// The fixed inode that `fd` reports when it is still the container's
/// inherited stdin, stdout or stderr, as fdinfo and `/proc/self/fd/N` report
/// it. Every stat call that describes the descriptor itself must use it, or
/// the guest's libc decides which number `fstat()` sees.
fn stdio_inode_override<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &G,
    fd: i32,
) -> Option<DetInode> {
    guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            deterministic_stdio_inode_for_resource(fd, detfd.resource())
        })
        .ok()
        .flatten()
}

/// The longest path, NUL excluded, that `Detcore::stat_guest_path` stages in
/// the guest stack scratch; a longer one goes to a transient page.
const GUEST_STAT_PATH_CAPACITY: usize = 512;

/// Where `Detcore::stat_guest_path_at` starts a relative path, and the
/// `fstatat` flags it passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatAt {
    dirfd: RawFd,
    flags: AtFlags,
}

impl StatAt {
    /// `stat(path)`, resolved as the guest's own `stat` resolves it: from its
    /// current directory, following a final symlink.
    fn path() -> Self {
        Self {
            dirfd: libc::AT_FDCWD,
            flags: AtFlags::empty(),
        }
    }

    /// The entry `name` of the directory open as `dirfd`, as `lstat` reports
    /// it: `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT)`. A
    /// `d_ino` names the entry itself, not what a symlink entry points to,
    /// and asking must not mount an automount point.
    fn entry_of(dirfd: RawFd) -> Self {
        Self {
            dirfd,
            flags: AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_NO_AUTOMOUNT,
        }
    }

    /// What a refusal says Detcore could not ask the guest to do.
    fn verb(self) -> &'static str {
        if self.flags.contains(AtFlags::AT_SYMLINK_NOFOLLOW) {
            "lstat the directory entry"
        } else {
            "stat"
        }
    }
}

/// What `Detcore::settle_overlay_entry_lookup` asked about one entry of a
/// listing, which `Detcore::directory_entry_identity` uses rather than ask
/// the guest's filesystem again.
struct ToldEntry {
    name: Vec<u8>,
    /// The entry's `lstat`, `None` where the name reached nothing.
    stat: Option<libc::stat>,
}

/// What the stack-scratch attempt of `Detcore::stat_guest_path` found.
enum StackStat {
    /// The scratch held the stat: the guest's answer, `None` when its stat
    /// failed.
    Answered(Option<libc::stat>),
    /// The scratch is too small, or faulted with `EFAULT`.
    Unusable,
}

/// Length of the transient mapping `Detcore::stat_guest_path` stages a path
/// of `path_len` bytes in: the path and its NUL, then an 8-aligned
/// `struct stat`, rounded up to whole pages.
fn transient_stat_page_len(path_len: usize) -> usize {
    // SAFETY: sysconf has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .expect("page size must be positive");
    ((path_len + 1).next_multiple_of(8) + std::mem::size_of::<libc::stat>()).next_multiple_of(page)
}

fn host_page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .expect("page size must be positive")
}

/// `statx` mask bit for the mount's unique id (Linux 6.8 and later). Older
/// kernels ignore it and report the reusable id under `STATX_MNT_ID`.
const STATX_MNT_ID_UNIQUE: u32 = 0x4000;
/// `statmount` request bit for the superblock's device and magic.
const STATMOUNT_SB_BASIC: u64 = 0x1;
/// Size of the first version of `struct mnt_id_req`: `u32 size`, `u32 spare`,
/// `u64 mnt_id`, `u64 param`.
const MNT_ID_REQ_SIZE_VER0: usize = 24;
/// Bytes offered to `statmount`: the whole fixed `struct statmount`, which
/// the kernel copies only as far as it fills.
const STATMOUNT_BUFFER: usize = 512;
/// Offsets in `struct statmount` of `u64 mask`, `u32 sb_dev_major` and
/// `u32 sb_dev_minor`.
const STATMOUNT_MASK_OFFSET: usize = 8;
const STATMOUNT_SB_DEV_MAJOR_OFFSET: usize = 16;
const STATMOUNT_SB_DEV_MINOR_OFFSET: usize = 20;

/// What the guest's kernel answered for the mount a file is on.
#[derive(Debug, PartialEq, Eq)]
enum MountSuperblock {
    /// `statmount` reported this superblock device.
    Device(u64),
    /// The guest cannot see that mount, or the path changed: no proof.
    Unproven,
    /// The kernel reported only this reusable mount id, which the guest's
    /// `mountinfo` lists.
    Listed(u64),
}

/// Whether `errno` from a path lookup the guest made is its filesystem's
/// answer that the path names nothing that lookup reaches, rather than a
/// failure to ask (see `Detcore::stat_guest_path`).
fn is_path_answer(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP | Errno::ENAMETOOLONG | Errno::EACCES
    )
}

/// The error of an identity lookup that could not be asked: a tool error
/// carrying [`IdentityLookupRefused`], which stops the refusing process on
/// every backend, never an errno the guest's call would report as its own
/// result.
fn identity_lookup_refused(what: &str, path: &[u8], cause: impl std::fmt::Display) -> Error {
    IdentityLookupRefused(format!(
        "could not ask the guest to {what} {:?}: {cause}; refusing rather than \
         keying the file's identity on another source",
        String::from_utf8_lossy(path)
    ))
    .into_error()
}

/// A private anonymous mapping of `len` bytes in the guest, made for one
/// identity lookup and unmapped by `unmap_identity_page` before the guest
/// resumes; its address never reaches the guest. A failure is the lookup's
/// refusal, as for the transient page of `Detcore::stat_guest_path`.
async fn map_identity_page<'a, G, T>(
    guest: &mut G,
    len: usize,
    what: &str,
    path: &[u8],
) -> Result<AddrMut<'a, u8>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    map_transient_page(guest, len).await.map_err(|errno| {
        warn!(
            "[detcore] could not map a transient page for a {what} of {:?} ({errno}); \
             refusing rather than keying on another identity",
            String::from_utf8_lossy(path)
        );
        match errno {
            Errno::ESRCH => Error::Errno(errno),
            errno => identity_lookup_refused(what, path, errno),
        }
    })
}

/// A private anonymous mapping of `len` bytes in the guest, or the errno its
/// injected `mmap` failed with; [`map_identity_page`] without its refusal.
async fn map_transient_page<'a, G, T>(guest: &mut G, len: usize) -> Result<AddrMut<'a, u8>, Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mapped = guest
        .inject_with_retry(Syscall::Mmap(
            syscalls::Mmap::new()
                .with_addr(None)
                .with_len(len)
                .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                .with_fd(-1)
                .with_offset(0),
        ))
        .await?;
    Ok(usize::try_from(mapped)
        .ok()
        .and_then(AddrMut::<u8>::from_raw)
        .unwrap_or_else(|| panic!("transient identity page mmap returned {mapped}")))
}

/// Unmaps a page `map_identity_page` made. A failure is only logged, as for
/// `Detcore::stat_guest_path`'s transient page: the lookup's answer stands.
async fn unmap_identity_page<G, T>(guest: &mut G, page: AddrMut<'_, u8>, len: usize, path: &[u8])
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Err(errno) = unmap_transient_page(guest, page, len).await {
        warn!(
            "[detcore] could not unmap the transient identity page for {:?}: {errno}",
            String::from_utf8_lossy(path)
        );
    }
}

/// Unmaps a page [`map_transient_page`] made, or returns the errno its
/// injected `munmap` failed with, the page then still mapped.
async fn unmap_transient_page<G, T>(
    guest: &mut G,
    page: AddrMut<'_, u8>,
    len: usize,
) -> Result<(), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    guest
        .inject_with_retry(Syscall::Munmap(
            syscalls::Munmap::new()
                .with_addr(Some(page.cast::<libc::c_void>().into()))
                .with_len(len),
        ))
        .await
        .map(drop)
}

/// The guest's `/proc/<pid>/mountinfo`, read on the tool's side as the
/// fdinfo mount-id check reads it.
fn guest_mountinfo<G: Guest<Detcore<T>>, T: RecordOrReplay>(guest: &G) -> Result<Vec<u8>, Error> {
    #[cfg(test)]
    inject_fstat_scratch::MOUNTINFO_READS.with(|reads| reads.set(reads.get() + 1));
    #[cfg(test)]
    if let Some(scripted) =
        inject_fstat_scratch::SCRIPTED_MOUNTINFO.with(|scripted| scripted.borrow().clone())
    {
        return Ok(scripted);
    }
    let path = format!("/proc/{}/mountinfo", guest.pid().as_raw());
    std::fs::read(&path).map_err(|error| {
        IdentityLookupRefused(format!(
            "could not read {path} to prove a maps line's device: {error}; refusing \
             rather than keying the file's identity on another source"
        ))
        .into_error()
    })
}

/// The superblock device of mount `mount_id` in `mountinfo`: a row's first
/// field is its mount id and its third the superblock's `major:minor`, in
/// decimal (`show_mountinfo`). Only those fields are read. `Ok(None)` when no
/// row has that id; an error when the row that has it does not give a
/// device.
fn mountinfo_superblock_device(mountinfo: &[u8], mount_id: u64) -> Result<Option<u64>, String> {
    for row in mountinfo.split(|&byte| byte == b'\n') {
        let mut fields = row.split(|&byte| byte == b' ');
        let id = fields
            .next()
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<u64>().ok());
        if id != Some(mount_id) {
            continue;
        }
        let device = fields
            .nth(1)
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.split_once(':'))
            .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)));
        return match device {
            Some((major, minor)) => Ok(Some(libc::makedev(major, minor))),
            None => Err(format!("{:?}", String::from_utf8_lossy(row))),
        };
    }
    Ok(None)
}

/// The run-global identity pools, reached through this guest's RPCs to the
/// global tool: the same `InodePool`/`DevicePool` that `stat` determinizes
/// through, so a mapping line and a `stat` of the same file agree. A line is
/// resolved to its file's identity by its own record
/// (`mapping_recorded_identity`) or else by the identity `stat` reports for
/// its header (`mapping_stat_identity`).
struct GuestMappingMinter<'a, G, T: RecordOrReplay> {
    detcore: &'a Detcore<T>,
    guest: &'a mut G,
    /// Whose address space the snapshot shows (`ProcfsFile::mapping_subject`):
    /// the reader's mapping records describe its lines only when it is the
    /// reader's own.
    subject: MappingSubject,
}

impl<'a, G, T: RecordOrReplay> GuestMappingMinter<'a, G, T> {
    fn new(detcore: &'a Detcore<T>, guest: &'a mut G, subject: MappingSubject) -> Self {
        Self {
            detcore,
            guest,
            subject,
        }
    }
}

impl<G, T> crate::procfs::MappingIdentityMinter for GuestMappingMinter<'_, G, T>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    type Error = Error;

    fn recorded_identity(
        &mut self,
        key: &crate::procfs::MappingKey,
        start: usize,
    ) -> Option<RawInode> {
        self.detcore
            .mapping_recorded_identity(&*self.guest, key, start, self.subject)
    }

    async fn stat_identity(&mut self, key: &crate::procfs::MappingKey) -> Result<RawInode, Error> {
        self.detcore
            .mapping_stat_identity(self.guest, key, self.subject)
            .await
    }

    fn ambiguous_line(
        &mut self,
        line: &crate::procfs::MappingLine,
        identity: RawInode,
        recorded: RawInode,
    ) -> Error {
        let at = match line.start {
            Some(start) => format!("at {start:#x}"),
            None => "whose address range does not parse".to_owned(),
        };
        IdentityLookupRefused(format!(
            "the maps line {at} naming {:?} is ambiguous: it has no mapping record \
             of its own and resolves to the file {identity} (device:inode), while \
             another line with the same device, inode and pathname columns is \
             recorded as the file {recorded}; refusing rather than give the line \
             either file's identity",
            line.key.pathname
        ))
        .into_error()
    }

    async fn inodes(&mut self, raw_files: Vec<RawInode>) -> Vec<DetInode> {
        determinize_mapping_inodes(self.guest, raw_files).await
    }

    async fn device(&mut self, raw_device: u64) -> u64 {
        determinize_device(self.guest, raw_device).await
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Binds, at open time, whose address space a `maps`/`smaps` descriptor
    /// shows (`ProcfsFile::bind_mapping_subject`).
    ///
    /// The opener's thread-group id and thread id are asked of the guest,
    /// through injected `getpid` and `gettid` that the guest does not see,
    /// only when metadata is virtualized: the only mode in which a mapping's
    /// identity is resolved through its subject (`mapping_stat_identity`).
    /// Both are in the guest's pid namespace, the one its `/proc` spellings
    /// name, as in `canonicalize_namespace_readlink_result`.
    pub(crate) async fn bind_procfs_mapping_subject<G: Guest<Self>>(
        guest: &mut G,
        procfs: &mut ProcfsFile,
    ) -> Result<(), Error> {
        if !procfs.needs_mapping_opener_ids() {
            return Ok(());
        }
        let opener_ids = if guest.config().virtualize_metadata {
            let tgid = guest.inject(syscalls::Getpid::new()).await? as i32;
            let tid = guest.inject(syscalls::Gettid::new()).await? as i32;
            Some((tgid, tid))
        } else {
            None
        };
        procfs.bind_mapping_subject(guest.thread_state().mm_id, opener_ids);
        Ok(())
    }

    async fn observe_timer_slack_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: i32,
    ) -> Result<Option<(u64, u64)>, Error> {
        // Re-resolve the numeric proc path on every operation. Linux gives a
        // recycled TID a different proc inode, while the original open file
        // description remains bound to the exited task's inode.
        let path = format!("/proc/{target}/timerslack_ns");
        let path_bytes = path.as_bytes();
        let mut path_buffer = [0_u8; 64];
        assert!(path_bytes.len() < path_buffer.len());
        path_buffer[..path_bytes.len()].copy_from_slice(path_bytes);

        let mut stack = guest.stack().await;
        let path_address = stack.push(path_buffer).cast::<libc::c_char>();
        let statptr = StatPtr(stack.reserve());
        let stack_guard = stack.commit()?;
        let call = syscalls::Fstatat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ))
            .with_stat(Some(statptr))
            .with_flags(AtFlags::empty());
        let mut identity = match guest.inject_with_retry(call).await {
            Ok(_) => {
                let stat = statptr.read(&guest.memory())?;
                Some((stat.st_dev, stat.st_ino))
            }
            Err(Errno::ENOENT) | Err(Errno::ESRCH) => None,
            Err(error) => return Err(error.into()),
        };
        drop(stack_guard);
        // Replayer runs the guest in a filesystem chroot whose `/proc` path is
        // intentionally absent, while the tracing process remains in the same
        // PID namespace and can resolve the task through its own proc mount.
        // Use that equivalent view only for record/replay; other backends keep
        // the guest-path result above as their sole authority.
        if identity.is_none()
            && guest.config().recordreplay_modes
            && let Ok(metadata) = std::fs::metadata(path)
        {
            identity = Some((metadata.dev(), metadata.ino()));
        }
        Ok(identity)
    }

    async fn require_current_timer_slack_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        binding: TimerSlackBinding,
    ) -> Result<(), Error> {
        let observed_identity = self
            .observe_timer_slack_identity(guest, binding.target)
            .await?;
        let current = guest.inject(syscalls::Gettid::new()).await? as i32;
        classify_timer_slack_binding(binding, observed_identity, current).map_err(Into::into)
    }

    fn timer_slack_binding<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
    ) -> Result<Option<TimerSlackBinding>, Errno> {
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd
                .procfs_timer_slack_binding()
                .map(|(target, device, inode)| TimerSlackBinding {
                    target,
                    device,
                    inode,
                })
        })
    }

    fn require_timer_slack_access<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
        write: bool,
    ) -> Result<(), Errno> {
        guest.thread_state().with_detfd(fd, |detfd| {
            let flags = detfd.status_flags();
            let mode = flags & libc::O_ACCMODE;
            let denied = flags & libc::O_PATH != 0
                || if write {
                    mode == libc::O_RDONLY
                } else {
                    mode == libc::O_WRONLY
                };
            (!denied).then_some(()).ok_or(Errno::EBADF)
        })?
    }

    fn read_timer_slack_input<G: Guest<Self>>(
        &self,
        guest: &G,
        buffer: Option<Addr<u8>>,
        count: usize,
    ) -> Result<u64, Errno> {
        let mut bytes = vec![0_u8; count.min(TIMER_SLACK_PARSE_BYTES)];
        if !bytes.is_empty() {
            guest
                .memory()
                .read_exact(buffer.ok_or(Errno::EFAULT)?, &mut bytes)?;
        }
        parse_timer_slack_write(&bytes)
    }

    async fn read_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<AddrMut<'_, u8>>,
        maximum: usize,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, false)?;
        if maximum == 0 {
            return Ok(0);
        }
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack read lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let value = guest.thread_state().timer_slack_ns;
        let preview = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.preview_procfs_timer_slack(value, maximum))?
            .expect("timer-slack procfs state disappeared");
        let copied = copy_timer_slack_output(&mut guest.memory(), buffer, &preview.bytes)?;
        if copied != 0 {
            guest.thread_state().with_detfd(fd, |detfd| {
                detfd.commit_procfs_timer_slack_read(&preview, copied);
            })?;
        }
        Ok(copied as i64)
    }

    async fn pread_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<AddrMut<'_, u8>>,
        maximum: usize,
        offset: i64,
    ) -> Result<i64, Error> {
        if offset < 0 {
            return Err(Errno::EINVAL.into());
        }
        self.require_timer_slack_access(guest, fd, false)?;
        if maximum == 0 {
            return Ok(0);
        }
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack pread lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let value = guest.thread_state().timer_slack_ns;
        let bytes = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                detfd.take_procfs_timer_slack_at(value, offset as usize, maximum)
            })?
            .expect("timer-slack procfs state disappeared");
        Ok(copy_timer_slack_output(&mut guest.memory(), buffer, &bytes)? as i64)
    }

    async fn readv_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        iovecs: Vec<TimerSlackIovec>,
        offset: Option<i64>,
        flags: i32,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, false)?;
        let maximum = iovecs.iter().map(|iovec| iovec.len).sum::<usize>();
        if maximum == 0 {
            return Ok(0);
        }
        if flags & !libc::RWF_HIPRI != 0 {
            return Err(Errno::EOPNOTSUPP.into());
        }
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack readv lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let value = guest.thread_state().timer_slack_ns;
        let mut positioned_offset = offset.map(|offset| offset as usize);
        let mut total = 0_usize;
        for iovec in iovecs {
            if iovec.len == 0 {
                continue;
            }
            let sequential_preview = if positioned_offset.is_none() {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.preview_procfs_timer_slack(value, iovec.len)
                })?
            } else {
                None
            };
            let bytes = match (&sequential_preview, positioned_offset) {
                (Some(preview), None) => preview.bytes.clone(),
                (None, Some(offset)) => guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        detfd.take_procfs_timer_slack_at(value, offset, iovec.len)
                    })?
                    .expect("timer-slack procfs state disappeared"),
                _ => unreachable!("timer-slack read mode changed while reading"),
            };
            if bytes.is_empty() {
                break;
            }
            let copied = match copy_timer_slack_output(
                &mut guest.memory(),
                AddrMut::from_raw(iovec.base),
                &bytes,
            ) {
                Ok(copied) => copied,
                Err(_) if total > 0 => return Ok(total as i64),
                Err(error) => return Err(error.into()),
            };
            if let Some(preview) = &sequential_preview {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.commit_procfs_timer_slack_read(preview, copied);
                })?;
            }
            total += copied;
            if let Some(offset) = positioned_offset.as_mut() {
                *offset += copied;
            }
            if copied != bytes.len() {
                return Ok(total as i64);
            }
            if bytes.len() != iovec.len {
                break;
            }
        }
        Ok(total as i64)
    }

    async fn write_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<Addr<'_, u8>>,
        count: usize,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, true)?;
        let requested = self.read_timer_slack_input(guest, buffer, count)?;
        let binding = self
            .timer_slack_binding(guest, fd)?
            .expect("timer-slack write lost its procfs classification");
        self.require_current_timer_slack_target(guest, binding)
            .await?;
        let state = guest.thread_state_mut();
        state.timer_slack_ns = if requested == 0 {
            state.default_timer_slack_ns
        } else {
            requested
        };
        i64::try_from(count).map_err(|_| Errno::EINVAL.into())
    }

    async fn writev_timer_slack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        iovecs: Vec<TimerSlackIovec>,
        flags: i32,
    ) -> Result<i64, Error> {
        self.require_timer_slack_access(guest, fd, true)?;
        if iovecs.iter().all(|iovec| iovec.len == 0) {
            return Ok(0);
        }
        if flags & !libc::RWF_HIPRI != 0 {
            return Err(Errno::EOPNOTSUPP.into());
        }
        // Procfs supplies only `.write`, so Linux's writev fallback invokes it
        // once per nonempty iovec. Preserve its partial-success behavior and
        // let the last successful segment determine the current slack.
        let mut total = 0_i64;
        for iovec in iovecs {
            if iovec.len == 0 {
                continue;
            }
            let buffer = Addr::from_raw(iovec.base).ok_or(Errno::EFAULT);
            let requested = match buffer
                .and_then(|buffer| self.read_timer_slack_input(guest, Some(buffer), iovec.len))
            {
                Ok(requested) => requested,
                Err(_error) if total > 0 => return Ok(total),
                Err(error) => return Err(error.into()),
            };
            let binding = self
                .timer_slack_binding(guest, fd)?
                .expect("timer-slack writev lost its procfs classification");
            if let Err(error) = self
                .require_current_timer_slack_target(guest, binding)
                .await
            {
                return if total > 0 { Ok(total) } else { Err(error) };
            }
            let state = guest.thread_state_mut();
            state.timer_slack_ns = if requested == 0 {
                state.default_timer_slack_ns
            } else {
                requested
            };
            total = total
                .checked_add(i64::try_from(iovec.len).map_err(|_| Errno::EINVAL)?)
                .ok_or(Errno::EINVAL)?;
        }
        Ok(total)
    }
    /// Set the kernel's O_NONBLOCK on `fd`'s open file description, keeping its
    /// other status flags. The caller records the change with
    /// `maybe_set_nonblocking_fd`; the guest-visible flags are unchanged.
    async fn inject_physical_nonblocking<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<(), Errno> {
        let flags = guest
            .inject(syscalls::Fcntl::new().with_fd(fd).with_cmd(F_GETFL))
            .await?;
        guest
            .inject(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(F_SETFL(flags as i32 | OFlag::O_NONBLOCK.bits())),
            )
            .await?;
        Ok(())
    }

    /// The raw identity the `ino:` line of `/proc/*/fdinfo/<target_fd>` is
    /// keyed on: the descriptor's cached stat, or an `fstat` of it.
    ///
    /// With `virtualize_metadata` that is the object's device and inode, as
    /// `stat` reports them. Without it -- `hermit record` and `hermit replay`
    /// -- a pipe or a socket is keyed on its inode alone, on device 0, which
    /// is how its `/proc/<pid>/fd/<fd>` link is keyed in that mode
    /// (`canonicalize_other_proc_fd_target` and `own_proc_fd_link_identity`),
    /// so the link's `pipe:[N]` and the fdinfo `ino:` line name one inode, as
    /// they do on Linux. Every other file keeps its device, so the same inode
    /// number on two filesystems still names two files
    /// (<https://github.com/rrnewton/hermit/issues/3307>).
    ///
    /// In that mode the pipe's or socket's inode comes from the `ino:` line
    /// of `contents`, the fdinfo bytes being sanitized, and the descriptor's
    /// `fstat` decides only its file type. At replay those bytes are the
    /// recording's (ReadV2 supplies them), as is the link's `pipe:[N]`, while
    /// the replayer's recreated pipe has an inode of its own; keying on the
    /// live inode made the two views name two inodes whenever those numbers
    /// differed. When `contents` has no single well-formed `ino:` line (a
    /// kernel older than the field), the `fstat` inode is used, as before.
    ///
    /// ⚠️ THE FILE TYPE ALONE DECIDES. A named FIFO, or a socket file reached
    /// through an `O_PATH` descriptor, is keyed on device 0 as well, although
    /// its link names a path. Its inode can then share a deterministic inode
    /// with an anonymous pipe or socket whose inode number happens to be
    /// equal, as every file could before the pool was keyed on devices.
    pub(crate) async fn fdinfo_raw_file_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target_fd: i32,
        cached_stat: Option<crate::stat::DetStat>,
        contents: &[u8],
    ) -> Result<RawInode, Error> {
        let (mode, raw) = match cached_stat {
            Some(stat) => (stat.mode, stat.raw_inode()),
            None => {
                let stat = self.inject_fstat(guest, target_fd).await?;
                (stat.st_mode, RawInode::new(stat.st_dev, stat.st_ino))
            }
        };
        let pipe_or_socket = matches!(mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK);
        Ok(if pipe_or_socket && !guest.config().virtualize_metadata {
            let inode = crate::procfs::parse_fdinfo_inode(contents).unwrap_or(raw.ino);
            RawInode::new(0, inode)
        } else {
            raw
        })
    }

    /// Inject an extra fstat to retrieve file metadata.
    ///
    /// The kernel needs a writable `struct stat` in the guest. It is staged
    /// first in the guest stack scratch, which on the ptrace backend lies just
    /// below the red zone under the guest's stack pointer and costs nothing
    /// when the stack has room. That memory is not guaranteed to be writable:
    /// the guest may run with its stack pointer just above a guard page (a
    /// thread, fiber or alternate signal stack), or within a few hundred bytes
    /// of the lowest page of the main-thread stack, which a tracer's write does
    /// not grow. The fault then belongs to Detcore's bookkeeping, not to the
    /// guest, so the same fstat is repeated with its buffer in a transient
    /// private page that is unmapped before the guest resumes
    /// (<https://github.com/rrnewton/hermit/issues/3328>).
    pub(crate) async fn inject_fstat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        raw_fd: RawFd,
    ) -> Result<libc::stat, Errno> {
        info!(
            "Injecting additional fstat to retrieve file metadata on fd {}.",
            raw_fd
        );
        let fstat = move |statptr: StatPtr<'_>| {
            Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(raw_fd)
                    .with_stat(Some(statptr)),
            )
        };
        let copied = match Self::inject_stat_on_stack(guest, fstat).await {
            Err(Errno::EFAULT) => {
                info!(
                    "Guest stack scratch cannot hold the fstat buffer for fd {}; \
                     using a transient page instead.",
                    raw_fd
                );
                Self::inject_stat_in_transient_page(guest, fstat).await?
            }
            result => result?,
        };
        trace!("extra fstat returned inode {}", copied.st_ino);
        Ok(copied)
    }

    /// `fstatat(dirfd, path, AT_SYMLINK_NOFOLLOW)` for a path the guest named
    /// in a call that has not run yet and must still read `inputs`, its paths
    /// ([`Detcore::inject_statat`]).
    async fn inject_lstatat<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: syscalls::PathPtr<'_>,
        inputs: [Option<syscalls::PathPtr<'_>>; 2],
    ) -> Result<libc::stat, Errno> {
        Self::inject_statat(guest, dirfd, path, inputs, AtFlags::AT_SYMLINK_NOFOLLOW).await
    }

    /// `fstatat(dirfd, path, flags)` for a path the guest named in a call that
    /// has not run yet and must still read `inputs`, its paths.
    ///
    /// The buffer is staged as the utimensat target lookup stages its own: in
    /// the guest stack scratch, below the red zone, where a raw syscall may
    /// keep its own path, so only when it overlaps none of `inputs`, and with
    /// the bytes under it saved and put back after the commit and after the
    /// lookup, so that the call reads its inputs unchanged (claude review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P3-2). Otherwise it is
    /// a transient private page, unmapped before the guest resumes.
    pub(crate) async fn inject_statat<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        path: syscalls::PathPtr<'_>,
        inputs: [Option<syscalls::PathPtr<'_>>; 2],
        flags: AtFlags,
    ) -> Result<libc::stat, Errno> {
        let lstatat = move |statptr: StatPtr<'_>| {
            Syscall::Newfstatat(
                syscalls::Newfstatat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_stat(Some(statptr))
                    .with_flags(flags),
            )
        };
        // The scratch starts 128 bytes below the stack pointer, at addresses
        // computed by subtraction.
        let scratch = 128 + std::mem::size_of::<libc::stat>();
        if usize::try_from(guest.regs().await.rsp).map_or(true, |rsp| rsp <= scratch) {
            return Self::inject_stat_in_transient_page(guest, lstatat).await;
        }
        let mut stack = guest.stack().await;
        let statptr: StatPtr = StatPtr(stack.reserve());
        let mut saved = [0u8; std::mem::size_of::<libc::stat>()];
        let staged_on_stack = !inputs
            .iter()
            .any(|input| path_overlaps_buffer(&guest.memory(), *input, statptr))
            && guest
                .memory()
                .read_exact(statptr.0.cast(), &mut saved)
                .is_ok();
        if !staged_on_stack {
            drop(stack);
            return Self::inject_stat_in_transient_page(guest, lstatat).await;
        }
        let guard = match stack.commit() {
            Ok(guard) => guard,
            Err(_) => {
                restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
                return Self::inject_stat_in_transient_page(guest, lstatat).await;
            }
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        let looked_up = Self::inject_stat_into(guest, lstatat(statptr), statptr).await;
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        drop(guard);
        looked_up
    }

    /// The fast path of [`Self::inject_fstat`]: the buffer lives in the guest
    /// stack scratch. Returns `EFAULT` when that scratch cannot be read,
    /// committed or written.
    ///
    /// On ptrace the buffer is the guest's own memory below its red zone,
    /// which the commit and the kernel's `fstat` overwrite. (The KVM backend
    /// of the pinned Reverie hands out its reserved `ToolScratch` region
    /// instead, outside the guest's view, where the save and restore below
    /// protect nothing of the guest's and cost only the copy.) A guest that
    /// issues raw system calls may keep live data there, and Linux never
    /// writes there for it, so the 144 bytes under the buffer are read before
    /// the scratch is committed and written back afterwards: on success, on a
    /// failed `fstat` and after a fault alike, before the scratch guard drops.
    /// A scratch whose bytes cannot be read is not used. This is the same
    /// save and restore [`Self::stat_guest_path`] performs; an error other
    /// than `EFAULT` from any step wins over an `EFAULT` from another.
    async fn inject_stat_on_stack<G: Guest<Self>>(
        guest: &mut G,
        stat_call: impl Fn(StatPtr<'_>) -> Syscall,
    ) -> Result<libc::stat, Errno> {
        Self::inject_into_stack_buffer(guest, |stat| stat_call(StatPtr(stat))).await
    }

    /// Inject the system call `syscall` makes around a `B` buffer in the
    /// guest stack scratch, and read back what the kernel wrote there, saving
    /// and restoring the scratch's bytes as [`Self::inject_stat_on_stack`]
    /// describes. Returns `EFAULT` when the scratch cannot be read, committed
    /// or written.
    async fn inject_into_stack_buffer<G: Guest<Self>, B: Copy>(
        guest: &mut G,
        syscall: impl FnOnce(AddrMut<'_, B>) -> Syscall,
    ) -> Result<B, Errno> {
        const SAVED_MAX: usize = 256;
        let len = std::mem::size_of::<B>();
        assert!(len <= SAVED_MAX, "a {len}-byte stack buffer");
        let mut stack = guest.stack().await;
        let buffer: AddrMut<B> = stack.reserve();
        // On the stack, not the heap: this runs for every getdents, and the
        // Detcore tests run under an allocator that never frees.
        let mut saved = [0u8; SAVED_MAX];
        let saved = &mut saved[..len];
        // `EFAULT` here sends an fstat to a transient page before anything is
        // written.
        guest.memory().read_exact(buffer.cast(), saved)?;
        // Keep the guard until the buffer is written back. DBT and SaBRe free
        // their Tool-owned arena when the guard drops; LiteInst and e9patch
        // move it to a list of committed arenas that is cleared when the
        // current dispatch ends. Either way, an injected call after the drop
        // could write into memory the scratch no longer owns. A failed commit
        // is written back too: the ptrace commit writes page by page, so it
        // can fail after writing a lower writable page.
        let committed = stack.commit();
        let copied = match &committed {
            // NOTE: Must retry the injection here. This could get interrupted
            // and we don't want to rerun the entire syscall handler twice.
            Ok(_) => match guest.inject_with_retry(syscall(buffer)).await {
                Ok(_) => guest.memory().read_value(buffer),
                Err(errno) => Err(errno),
            },
            Err(errno) => Err(*errno),
        };
        let restored = guest.memory().write_exact(buffer.cast(), saved);
        drop(committed);
        match (copied, restored) {
            (Ok(copied), Ok(())) => Ok(copied),
            (Err(errno), Ok(())) | (Ok(_), Err(errno)) => Err(errno),
            (Err(Errno::EFAULT), Err(errno)) | (Err(errno), Err(_)) => Err(errno),
        }
    }

    /// The filesystem type (`f_type`) of the open file `raw_fd`, from an
    /// injected `fstatfs` whose buffer is in the guest stack scratch (see
    /// [`Self::inject_into_stack_buffer`]). There is no transient-page
    /// fallback: the one caller, [`Self::directory_entry_lookup`], has a
    /// faithful answer without the type.
    async fn inject_fstatfs_type<G: Guest<Self>>(
        guest: &mut G,
        raw_fd: RawFd,
    ) -> Result<libc::__fsword_t, Errno> {
        let statfs: libc::statfs = Self::inject_into_stack_buffer(guest, |buf| {
            Syscall::Fstatfs(syscalls::Fstatfs::new().with_fd(raw_fd).with_buf(Some(buf)))
        })
        .await?;
        Ok(statfs.f_type)
    }

    /// The fallback of [`Self::inject_fstat`] and [`Self::inject_lstatat`]:
    /// the buffer lives in a private
    /// anonymous page mapped for this call only. The guest never learns its
    /// address, and it is unmapped before the guest resumes, so the guest's
    /// address space is the same as before the call.
    async fn inject_stat_in_transient_page<G: Guest<Self>>(
        guest: &mut G,
        stat_call: impl Fn(StatPtr<'_>) -> Syscall,
    ) -> Result<libc::stat, Errno> {
        let len = std::mem::size_of::<libc::stat>();
        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await?;
        let page = usize::try_from(mapped)
            .ok()
            .and_then(AddrMut::<libc::stat>::from_raw)
            .unwrap_or_else(|| panic!("transient fstat page mmap returned {mapped}"));
        let copied = Self::inject_stat_into(guest, stat_call(StatPtr(page)), StatPtr(page)).await;
        if let Err(errno) = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(page.cast::<libc::c_void>().into()))
                    .with_len(len),
            ))
            .await
        {
            // Not expected: the page was mapped by this call and its address
            // never reached the guest. The metadata is still valid, so a
            // leftover page is no reason to fail the guest's syscall.
            warn!(
                "[detcore] could not unmap the transient stat page: {}",
                errno
            );
        }
        copied
    }

    /// Inject `stat_call`, which writes a `struct stat` at `statptr`, and read
    /// back what the kernel wrote.
    async fn inject_stat_into<G: Guest<Self>>(
        guest: &mut G,
        stat_call: Syscall,
        statptr: StatPtr<'_>,
    ) -> Result<libc::stat, Errno> {
        // NOTE: Must retry the injection here. This could get interrupted and
        // we don't want to rerun the entire syscall handler twice.
        guest.inject_with_retry(stat_call).await?;
        statptr.read(&guest.memory())
    }

    /// The raw identity the reader's own record gives the `maps`/`smaps` line
    /// whose header is `key` and whose range starts at `start`, if it has
    /// one; `subject` says whose address space the snapshot shows
    /// (`ProcfsFile::mapping_subject`). The line is then keyed on it rather
    /// than on `mapping_stat_identity`, and a line of the same header without
    /// a record of its own that resolves to another file is refused
    /// (`crate::procfs::mint_mapping_identities`).
    ///
    /// A file the reader mapped through a descriptor Detcore tracks has the
    /// `fstat` identity `handle_mmap` recorded for that address range
    /// (`MemoryMetadata::mapped_file_at`). That record needs no path, so it
    /// still holds after the file is unlinked or replaced and its descriptor
    /// closed, when the line reads ` (deleted)`. It is provenance for THAT
    /// range only: two lines with one header can map two files (see
    /// `crate::procfs::MappingLine`), so one line's record never keys
    /// another line. It is consulted only for a snapshot of the reader's own
    /// address space (`MappingSubject::Reader`): a file opened in that same
    /// address space through `/proc/self`, `/proc/thread-self`, or the
    /// opener's own thread-group id or thread id
    /// (`ProcfsFile::bind_mapping_subject`); and only when metadata is
    /// virtualized, for the reason `mapping_stat_identity` falls back to the
    /// header's pair then.
    ///
    /// A record is accepted only if its inode is the one the header reports,
    /// a guard against staleness: the record can outlive the mapping it
    /// describes, because `handle_mmap`, `handle_munmap` and `handle_mremap`
    /// update it only after a successful call, and only they (and `execve`,
    /// which starts an empty record) change it. Unmodelled sources of a stale
    /// record:
    ///
    /// - a range replaced by a call those handlers never see: `shmat` with
    ///   `SHM_REMAP` (refused with `ENOSYS` today, so latent), or any mapping
    ///   call that escapes interception on an in-guest backend;
    /// - a failed `MAP_FIXED` mmap or `MREMAP_FIXED` mremap, which some
    ///   kernels return after unmapping the target range, leaving a record
    ///   for a hole that one of the calls above can then fill.
    ///
    /// A stale record names another file, which almost always has another
    /// inode, so the check discards it and the line resolves as one without a
    /// record. ⚠️ A STALE RECORD WITH THE NEW FILE'S INODE NUMBER STILL WINS
    /// FOR ITS OWN LINE: the same inode number on another device passes. A
    /// record is provenance, not a path, so there is no mount to prove, and
    /// its device cannot be compared with the header's when the header's
    /// device is not `stat`'s. Since a line without a record that resolves to
    /// another file is refused, such a stale record can also refuse the
    /// snapshot of a header it no longer describes.
    fn mapping_recorded_identity<G: Guest<Self>>(
        &self,
        guest: &G,
        key: &crate::procfs::MappingKey,
        start: usize,
        subject: MappingSubject,
    ) -> Option<RawInode> {
        if !guest.config().virtualize_metadata || subject != MappingSubject::Reader {
            return None;
        }
        guest
            .thread_state()
            .mapped_file_at(start)
            .filter(|file| file.ino == key.inode)
    }

    /// The raw identity `stat` reports for the file a `maps`/`smaps` header
    /// names, which keys the deterministic inode of every line with that
    /// header that has no record of its own (`mapping_recorded_identity`).
    /// `subject` says whose address space the snapshot shows
    /// (`ProcfsFile::mapping_subject`).
    ///
    /// The header's own `(device, inode)` pair is NOT always that identity: on
    /// btrfs maps prints the superblock's device and `stat` the subvolume's,
    /// and on overlayfs maps prints the lower file's device. Keying the inode
    /// pool on the maps pair then gives the maps inode column a different
    /// value from `st_ino` for the same file
    /// (<https://github.com/rrnewton/hermit/issues/3307>). In order:
    ///
    /// 1. The pathname is resolved in the guest. This covers the executable
    ///    and the ELF interpreter, which `execve` maps without a system call
    ///    Detcore sees, and files mapped through a descriptor Detcore does
    ///    not track, such as one received over `SCM_RIGHTS`. The file it
    ///    names keys the line only with proof that it lies on the superblock
    ///    the header names (`Self::proves_header_device`).
    /// 2. Otherwise the guest `stat`s the executable link of the process
    ///    whose address space the snapshot shows: `/proc/self/exe` for the
    ///    reader's own, `/proc/<pid>/exe` for a task the file names by
    ///    number (`MappingSubject::Process`), and none when that task is not
    ///    known. That link names the running
    ///    executable even after it is unlinked or replaced (the line then
    ///    reads ` (deleted)`, or its path names another inode), and `stat`
    ///    follows it to the file, so this covers the executable where step 1
    ///    cannot. It keys the line only when the link reads as the line's
    ///    pathname (`Self::link_names_mapping`) and the executable lies on
    ///    the header's superblock (`Self::proves_header_device`): an
    ///    executable that merely shares the line's inode number is another
    ///    file. Linux binds a `maps` file's address space at open, while
    ///    `/proc/<pid>/exe` is resolved at read time; after that task exits or
    ///    calls `execve` the snapshot is empty, so no line asks.
    ///    `/proc/<pid>/map_files/<range>` would name every mapped
    ///    file the same way, but following it needs `CAP_SYS_ADMIN` or
    ///    `CAP_CHECKPOINT_RESTORE` in the initial user namespace
    ///    (`proc_map_files_get_link`), so it is not used.
    ///
    /// Each answer is accepted only if its inode is the one the header
    /// reports and with the superblock proof as well: an inode number alone
    /// names no file, since every filesystem, and every btrfs subvolume,
    /// numbers its own. The superblock comparison still passes two files with
    /// one inode number on ONE superblock: two btrfs subvolumes of one
    /// filesystem, or two overlayfs layers of one overlay mount.
    ///
    /// ⚠️ ONE ANSWER SERVES EVERY UNRECORDED LINE OF THE HEADER. A path or an
    /// executable link names one file, so two lines without records whose
    /// header names two files (a deleted file's pathname on two btrfs
    /// subvolumes) both get the identity of the one this resolves, or the
    /// header's pair. Only a record tells such lines apart, and a record
    /// contradicting this answer refuses the snapshot instead.
    ///
    /// Falls back to the header's pair -- which IS `stat`'s identity on every
    /// filesystem whose `st_dev` is its superblock device, such as ext4, xfs
    /// and tmpfs -- when metadata is not virtualized (record/replay, where
    /// `stat` is not determinized either and a resolution at replay time would
    /// depend on the replay host), and when no step names the file.
    ///
    /// ⚠️ ON BTRFS AND OVERLAYFS THAT FALLBACK DISAGREES WITH `stat`. The maps
    /// inode column then differs from `st_ino` for a file that is absent from
    /// the record and that no step can prove: the ELF interpreter after it is
    /// unlinked or replaced, a file mapped through an untracked descriptor
    /// and then unlinked, any unlinked file other than the executable in
    /// another process's maps (whose records Detcore does not consult), an
    /// unlinked executable in maps whose process is not known by number
    /// (`MappingSubject::Unknown`: a `/proc/self/maps` descriptor read in
    /// another address space when the opener's ids were not asked for), and
    /// a file on a mount outside the guest's root, which its mountinfo does
    /// not list. Before the inode pool was keyed on the device as well, such a
    /// line agreed with `st_ino` only because two files with one inode number
    /// on different devices shared one deterministic inode; no unprivileged
    /// kernel interface gives `stat`'s device for a mapping without a path,
    /// so the line keeps the header's pair rather than borrow another file's
    /// identity. On overlayfs before Linux 6.8 maps prints the real file's
    /// device rather than the overlay's, so no overlay line is proven there.
    /// A lookup that cannot be asked -- `stat_guest_path` finds no scratch,
    /// faults in its own buffers, or an injected stat, `statx` or `readlink`
    /// fails with anything but the guest filesystem's answer that the path
    /// names nothing -- is an error, never a step to the next source or to
    /// this fallback, so host resources cannot choose the identity (see
    /// there).
    ///
    /// ⚠️ WITHOUT `virtualize_metadata` THE SPLIT REACHES FDINFO TOO. There
    /// the header's pair keys every line, while the `ino:` line of
    /// `/proc/*/fdinfo/<fd>` is keyed on `fstat`'s device and inode
    /// (`fdinfo_raw_file_id`). On btrfs and overlayfs the two devices differ,
    /// so a file that is both mapped and open shows one inode in `maps` and
    /// another in its fdinfo, where Linux shows the same number in both.
    /// `stat` itself is not determinized in that mode. Both values stay
    /// deterministic.
    ///
    /// Directory entries mostly avoid that gap when metadata is virtualized:
    /// `getdents` keys a `d_ino` on the device the entry's own `lstat`
    /// reports when that `lstat` reports the same inode. It asks every entry
    /// on overlayfs and CephFS, but elsewhere only `..` and, on btrfs, an
    /// entry whose `d_ino` is 256, so a mount point whose root has the inode
    /// number of the entry it covers keeps the directory's device
    /// (`directory_entry_identity` names the classes).
    async fn mapping_stat_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        key: &crate::procfs::MappingKey,
        subject: MappingSubject,
    ) -> Result<RawInode, Error> {
        let header = RawInode::new(key.device, key.inode);
        if !guest.config().virtualize_metadata {
            return Ok(header);
        }
        for candidate in crate::procfs::mapping_path_candidates(&key.pathname) {
            let path = candidate.as_bytes();
            if let Some(stat) = self.stat_guest_path(guest, path).await?
                && stat.st_ino == key.inode
                && Self::proves_header_device(guest, path, &stat, key.device).await?
            {
                return Ok(RawInode::new(stat.st_dev, stat.st_ino));
            }
        }
        // Step 2: the executable link of the subject's process, resolved by
        // the guest's `fstatat`, so `/proc/self` is the reader.
        let executable_link = match subject {
            MappingSubject::Reader => Some(b"/proc/self/exe".to_vec()),
            MappingSubject::Process(pid) => Some(format!("/proc/{pid}/exe").into_bytes()),
            MappingSubject::Unknown => None,
        };
        if key.pathname.starts_with('/')
            && let Some(link) = executable_link
            && let Some(stat) = self.stat_guest_path(guest, &link).await?
            && stat.st_ino == key.inode
            && Self::link_names_mapping(guest, &link, &key.pathname).await?
            && Self::proves_header_device(guest, &link, &stat, key.device).await?
        {
            return Ok(RawInode::new(stat.st_dev, stat.st_ino));
        }
        Ok(header)
    }

    /// Whether the file a guest `stat` of `path` returned, whose inode is
    /// already the maps header's, lies on the superblock whose device that
    /// header reports. An inode number alone names no file: every filesystem
    /// numbers its own inodes, and btrfs numbers each subvolume's. So steps 1
    /// and 2 of [`Self::mapping_stat_identity`] accept an answer only with
    /// this proof as well.
    ///
    /// Proven at once when `stat`'s device IS the header's, as on ext4, xfs
    /// and tmpfs. Otherwise -- on btrfs, where `stat` reports the subvolume's
    /// device, and on overlayfs, where it can report a layer's -- the guest
    /// `statx`es the same path for the id of its mount, and that mount's
    /// superblock device is the device `maps` prints. The `statx` must report
    /// the device and inode the `stat` did, or the path has changed in
    /// between and proves nothing.
    ///
    /// On Linux 6.8 and later the `statx` reports the mount's unique id
    /// (`STATX_MNT_ID_UNIQUE`), and a guest `statmount` of that id, made in
    /// the same transient page, reports the superblock's device
    /// (`STATMOUNT_SB_BASIC`). Neither call opens anything, so the proof
    /// takes no slot in the guest's descriptor table. On DBT, SaBRe and
    /// LiteInst the tool runs in the guest process and shares that table: a
    /// descriptor the proof held there could change the number another guest
    /// thread's `open` or `SCM_RIGHTS` receipt is given, and at the guest's
    /// descriptor limit the proof could not be made at all.
    ///
    /// An older kernel reports only the reusable id (`STATX_MNT_ID`), and the
    /// proof falls back to that mount's row of the guest's
    /// `/proc/<pid>/mountinfo`, read on the tool's side as the fdinfo
    /// mount-id check reads it. So does a `statmount` that answers `ENOSYS`,
    /// as a seccomp policy can, after a second `statx` for the reusable id.
    /// That read opens a file, so it is made only where the launcher says a
    /// file Detcore opens takes a descriptor outside the guest's table
    /// (`Config::tool_opens_outside_guest_descriptor_table`): the tracer's on
    /// ptrace, the VMM's on KVM. Anywhere else -- DBT, SaBRe and LiteInst --
    /// the read would hold a guest descriptor, with the race above, and fail
    /// at the guest's descriptor limit, so the proof refuses before it opens
    /// anything. On those backends a guest that reads a btrfs or overlayfs
    /// mapping on a kernel before Linux 6.8, or under a policy that denies
    /// `statmount`, is stopped by that refusal rather than given an identity
    /// another source chose.
    ///
    /// `Ok(false)` -- no proof, so the line falls back to its header -- when
    /// that mount's superblock is another one, when the guest's filesystem
    /// answers that the path no longer names that file, or when the mount is
    /// not one the guest can see: `statmount` answers `ENOENT` (no such
    /// mount in the guest's mount namespace) or `EPERM` (outside the guest's
    /// root, for a guest without `CAP_SYS_ADMIN` over that namespace), or the
    /// guest's `mountinfo` lists no row for it. Mounts and namespace changes
    /// are refused once Detcore starts, so that is the guest's own
    /// configuration. A proof that could not be asked is an error, never a
    /// `false`, as in [`Self::stat_guest_path`]: no transient page, a fault
    /// in Detcore's own buffers, any other errno from the injected `statx` or
    /// `statmount`, a kernel that reports no mount id or no superblock, or a
    /// `mountinfo` that cannot be read or has a malformed row for that
    /// mount. Otherwise host resources would choose the identity.
    ///
    /// ⚠️ THE SAME INODE NUMBER IN TWO BTRFS SUBVOLUMES OF ONE FILESYSTEM STILL
    /// PASSES, and so do two overlayfs layers on one overlay mount: both share
    /// the superblock device `maps` prints, which is all a maps line says
    /// about where its file lives. For step 2, [`Self::link_names_mapping`]
    /// also requires the executable link's text to be the line's pathname.
    async fn proves_header_device<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
        stat: &libc::stat,
        header_device: u64,
    ) -> Result<bool, Error> {
        if stat.st_dev == header_device {
            return Ok(true);
        }
        let mount_id = match Self::guest_superblock_of(guest, path, stat).await? {
            MountSuperblock::Device(device) => return Ok(device == header_device),
            MountSuperblock::Unproven => return Ok(false),
            MountSuperblock::Listed(mount_id) => mount_id,
        };
        if !guest.config().tool_opens_outside_guest_descriptor_table {
            return Err(identity_lookup_refused(
                "find the superblock of",
                path,
                format_args!(
                    "the kernel reported only reusable mount id {mount_id}, and reading the \
                     guest's mountinfo for it would hold a descriptor in the guest's own table"
                ),
            ));
        }
        let mountinfo = guest_mountinfo(guest)?;
        match mountinfo_superblock_device(&mountinfo, mount_id) {
            Ok(Some(device)) => Ok(device == header_device),
            // A mount outside the guest's root is not in its mountinfo: an
            // exe link can name one. That is the guest's own configuration,
            // the same on every host, so it is an answer -- no proof -- and
            // not a failure to ask.
            Ok(None) => {
                trace!(
                    "{:?} is on mount {mount_id}, which the guest's mountinfo does not list",
                    String::from_utf8_lossy(path)
                );
                Ok(false)
            }
            Err(malformed) => Err(identity_lookup_refused(
                "find the superblock of",
                path,
                format_args!("its mountinfo row is malformed: {malformed}"),
            )),
        }
    }

    /// The superblock of the mount the file at `path` is on, asked in one
    /// transient page by a guest `statx` and, when that reports the mount's
    /// unique id, a guest `statmount`; see [`Self::proves_header_device`]
    /// for the answers and the errors.
    async fn guest_superblock_of<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
        stat: &libc::stat,
    ) -> Result<MountSuperblock, Error> {
        let statx_offset = (path.len() + 1).next_multiple_of(8);
        let request_offset = statx_offset + std::mem::size_of::<libc::statx>();
        let statmount_offset = request_offset + MNT_ID_REQ_SIZE_VER0;
        let len = (statmount_offset + STATMOUNT_BUFFER).next_multiple_of(host_page_size());
        let page = map_identity_page(guest, len, "statx", path).await?;
        // SAFETY: every offset is within the `len` bytes just mapped.
        let (statxptr, request, statmount) = unsafe {
            (
                StatxPtr(page.add(statx_offset).cast::<libc::statx>()),
                page.add(request_offset),
                page.add(statmount_offset),
            )
        };
        let asked =
            Self::superblock_in_page(guest, path, stat, page, statxptr, request, statmount).await;
        unmap_identity_page(guest, page, len, path).await;
        asked
    }

    /// The questions of [`Self::guest_superblock_of`], in its page.
    async fn superblock_in_page<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
        stat: &libc::stat,
        page: AddrMut<'_, u8>,
        statxptr: StatxPtr<'_>,
        request: AddrMut<'_, u8>,
        statmount: AddrMut<'_, u8>,
    ) -> Result<MountSuperblock, Error> {
        let unique = StatxMask::STATX_INO
            | StatxMask::STATX_MNT_ID
            | StatxMask::from_bits_retain(STATX_MNT_ID_UNIQUE);
        let Some(statx) = Self::statx_of_stat(guest, path, stat, page, statxptr, unique).await?
        else {
            return Ok(MountSuperblock::Unproven);
        };
        if statx.stx_mask & STATX_MNT_ID_UNIQUE == 0 {
            // Before Linux 6.8 the bit is unknown and ignored, and the kernel
            // reports the reusable id instead.
            return Self::listed_mount(path, &statx);
        }
        match Self::statmount_in_page(guest, statx.stx_mnt_id, request, statmount).await {
            Ok(Some(device)) => Ok(MountSuperblock::Device(device)),
            Ok(None) => Err(identity_lookup_refused(
                "statmount",
                path,
                "the kernel reported no superblock (STATMOUNT_SB_BASIC)",
            )),
            Err(errno @ (Errno::ENOENT | Errno::EPERM)) => {
                trace!(
                    "{:?} is on mount {}, which the guest cannot see: {errno}",
                    String::from_utf8_lossy(path),
                    statx.stx_mnt_id
                );
                Ok(MountSuperblock::Unproven)
            }
            Err(Errno::ENOSYS) => {
                let listed = StatxMask::STATX_INO | StatxMask::STATX_MNT_ID;
                match Self::statx_of_stat(guest, path, stat, page, statxptr, listed).await? {
                    Some(statx) => Self::listed_mount(path, &statx),
                    None => Ok(MountSuperblock::Unproven),
                }
            }
            Err(Errno::ESRCH) => Err(Error::Errno(Errno::ESRCH)),
            Err(errno) => Err(identity_lookup_refused("statmount", path, errno)),
        }
    }

    /// A guest `statx` of `path` asking `mask`, made in `page`, when the file
    /// it reports is still the one `stat` describes. `None` when the guest's
    /// filesystem answers that `path` names nothing or another file.
    async fn statx_of_stat<G: Guest<Self>>(
        guest: &mut G,
        path: &[u8],
        stat: &libc::stat,
        page: AddrMut<'_, u8>,
        statxptr: StatxPtr<'_>,
        mask: StatxMask,
    ) -> Result<Option<libc::statx>, Error> {
        let statx =
            match Self::statx_in_page(guest, StatAt::path(), path, page, statxptr, mask).await {
                Ok(Some(statx)) => statx,
                Ok(None) => return Ok(None),
                Err(Errno::ESRCH) => return Err(Error::Errno(Errno::ESRCH)),
                Err(errno) => return Err(identity_lookup_refused("statx", path, errno)),
            };
        let device = libc::makedev(statx.stx_dev_major, statx.stx_dev_minor);
        if (device, statx.stx_ino) != (stat.st_dev, stat.st_ino) {
            trace!(
                "{:?} names another file than its stat did: {:#x}/{} after {:#x}/{}",
                String::from_utf8_lossy(path),
                device,
                statx.stx_ino,
                stat.st_dev,
                stat.st_ino
            );
            return Ok(None);
        }
        Ok(Some(statx))
    }

    /// The reusable mount id a `statx` reported, for the guest's `mountinfo`.
    fn listed_mount(path: &[u8], statx: &libc::statx) -> Result<MountSuperblock, Error> {
        if statx.stx_mask & StatxMask::STATX_MNT_ID.bits() == 0 {
            return Err(identity_lookup_refused(
                "statx",
                path,
                "the kernel reported no mount id (STATX_MNT_ID needs Linux 5.8)",
            ));
        }
        Ok(MountSuperblock::Listed(statx.stx_mnt_id))
    }

    /// Stage `path` NUL-terminated at `path_address` and inject
    /// `statx(at.dirfd, path, at.flags, mask, statxptr)`, with the answers
    /// and errors of [`Self::fstatat_in_scratch`].
    async fn statx_in_page<G: Guest<Self>>(
        guest: &mut G,
        at: StatAt,
        path: &[u8],
        path_address: AddrMut<'_, u8>,
        statxptr: StatxPtr<'_>,
        mask: StatxMask,
    ) -> Result<Option<libc::statx>, Errno> {
        let mut staged = Vec::with_capacity(path.len() + 1);
        staged.extend_from_slice(path);
        staged.push(0);
        guest.memory().write_exact(path_address, &staged)?;
        let call = syscalls::Statx::new()
            .with_dirfd(at.dirfd)
            .with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ))
            .with_flags(at.flags)
            .with_mask(mask)
            .with_statx(Some(statxptr));
        match guest.inject_with_retry(call).await {
            Ok(_) => statxptr.read(&guest.memory()).map(Some),
            Err(error) if is_path_answer(error) => {
                trace!(
                    "guest statx of {:?} failed: {error}",
                    String::from_utf8_lossy(path)
                );
                Ok(None)
            }
            Err(error) => {
                warn!(
                    "[detcore] guest statx of {:?} could not be asked: {error}",
                    String::from_utf8_lossy(path)
                );
                Err(error)
            }
        }
    }

    /// Stage a `struct mnt_id_req` for mount `mount_id` asking
    /// `STATMOUNT_SB_BASIC` at `request`, and inject
    /// `statmount(request, statmount, STATMOUNT_BUFFER, 0)`. The superblock's
    /// device, or `None` when the kernel reports no superblock fields.
    async fn statmount_in_page<G: Guest<Self>>(
        guest: &mut G,
        mount_id: u64,
        request: AddrMut<'_, u8>,
        statmount: AddrMut<'_, u8>,
    ) -> Result<Option<u64>, Errno> {
        let mut staged = [0u8; MNT_ID_REQ_SIZE_VER0];
        staged[..4].copy_from_slice(&(MNT_ID_REQ_SIZE_VER0 as u32).to_ne_bytes());
        // `spare`, bytes 4 to 8, stays zero: the kernel refuses anything else.
        staged[8..16].copy_from_slice(&mount_id.to_ne_bytes());
        staged[16..].copy_from_slice(&STATMOUNT_SB_BASIC.to_ne_bytes());
        guest.memory().write_exact(request, &staged)?;
        let asked = guest
            .inject_with_retry(Syscall::Other(
                Sysno::statmount,
                SyscallArgs::new(
                    request.as_raw(),
                    statmount.as_raw(),
                    STATMOUNT_BUFFER,
                    0,
                    0,
                    0,
                ),
            ))
            .await;
        if let Err(error) = asked {
            trace!("guest statmount of mount {mount_id} failed: {error}");
            return Err(error);
        }
        let mut answer = [0u8; STATMOUNT_SB_DEV_MINOR_OFFSET + 4];
        guest.memory().read_exact(statmount, &mut answer)?;
        let word = |offset: usize| {
            u32::from_ne_bytes(answer[offset..offset + 4].try_into().expect("4 bytes"))
        };
        let mask = u64::from_ne_bytes(
            answer[STATMOUNT_MASK_OFFSET..STATMOUNT_MASK_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        if mask & STATMOUNT_SB_BASIC == 0 {
            return Ok(None);
        }
        Ok(Some(libc::makedev(
            word(STATMOUNT_SB_DEV_MAJOR_OFFSET),
            word(STATMOUNT_SB_DEV_MINOR_OFFSET),
        )))
    }

    /// Whether the guest's `readlink` of the executable link `link` reads as
    /// `pathname`, the maps line's: `maps` prints a mapped file's path as the
    /// link would, with ` (deleted)` once it is unlinked, except that it
    /// spells a newline `\012`. A file that merely shares the executable's
    /// inode number -- a non-executable mapping, a file literally named
    /// `x (deleted)` -- has another pathname, so step 2 of
    /// [`Self::mapping_stat_identity`] does not borrow the executable's
    /// identity for it. `Ok(false)` when the guest's filesystem answers that
    /// the link names nothing; errors as in [`Self::proves_header_device`].
    async fn link_names_mapping<G: Guest<Self>>(
        guest: &mut G,
        link: &[u8],
        pathname: &str,
    ) -> Result<bool, Error> {
        // Twice `PATH_MAX`: `d_path`, which both texts come from, never
        // writes more than one page, so a full buffer cannot be a whole link.
        const LINK_CAPACITY: usize = 2 * libc::PATH_MAX as usize;
        let buffer_offset = (link.len() + 1).next_multiple_of(8);
        let len = (buffer_offset + LINK_CAPACITY).next_multiple_of(host_page_size());
        let page = map_identity_page(guest, len, "readlink", link).await?;
        // SAFETY: `buffer_offset` is within the `len` bytes just mapped.
        let buffer = unsafe { page.add(buffer_offset) };
        let asked = Self::readlink_in_page(guest, link, page, buffer, LINK_CAPACITY).await;
        unmap_identity_page(guest, page, len, link).await;
        let text = match asked {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(false),
            Err(Errno::ESRCH) => return Err(Error::Errno(Errno::ESRCH)),
            Err(errno) => return Err(identity_lookup_refused("readlink", link, errno)),
        };
        if text.len() >= LINK_CAPACITY {
            return Err(identity_lookup_refused(
                "readlink",
                link,
                format_args!("the link filled all {LINK_CAPACITY} bytes of its buffer"),
            ));
        }
        let mut spelled = Vec::with_capacity(text.len());
        for &byte in &text {
            if byte == b'\n' {
                spelled.extend_from_slice(b"\\012");
            } else {
                spelled.push(byte);
            }
        }
        Ok(spelled == pathname.as_bytes())
    }

    /// Stage `link` NUL-terminated at `path_address` and inject
    /// `readlinkat(AT_FDCWD, link, buffer, capacity)`; the link's text, or
    /// `None` when the guest's filesystem answers that `link` names no link.
    async fn readlink_in_page<G: Guest<Self>>(
        guest: &mut G,
        link: &[u8],
        path_address: AddrMut<'_, u8>,
        buffer: AddrMut<'_, u8>,
        capacity: usize,
    ) -> Result<Option<Vec<u8>>, Errno> {
        let mut staged = Vec::with_capacity(link.len() + 1);
        staged.extend_from_slice(link);
        staged.push(0);
        guest.memory().write_exact(path_address, &staged)?;
        let call = syscalls::Readlinkat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ))
            .with_buf(Some(buffer.cast::<libc::c_char>()))
            .with_buf_len(capacity);
        match guest.inject_with_retry(call).await {
            Ok(read) => {
                let read = usize::try_from(read).expect("readlinkat returned a negative length");
                let mut text = vec![0u8; read];
                guest.memory().read_exact(buffer, &mut text)?;
                Ok(Some(text))
            }
            Err(error) if is_path_answer(error) || error == Errno::EINVAL => {
                trace!(
                    "guest readlink of {:?} failed: {error}",
                    String::from_utf8_lossy(link)
                );
                Ok(None)
            }
            Err(error) => {
                warn!(
                    "[detcore] guest readlink of {:?} could not be asked: {error}",
                    String::from_utf8_lossy(link)
                );
                Err(error)
            }
        }
    }

    /// `stat(path)` performed BY THE GUEST, through an injected
    /// `fstatat(AT_FDCWD, path, _, 0)`. `None` when the guest's filesystem
    /// answers that `path` names nothing this stat can reach -- `ENOENT`,
    /// `ENOTDIR`, `ELOOP`, `ENAMETOOLONG` or `EACCES` under the guest's own
    /// credentials -- or when `path` contains a NUL. Any other failure is an
    /// error, not a missing answer: no scratch to stage the call in, a fault
    /// in Detcore's own buffers, or any other errno from the injected stat
    /// (`ENOMEM`, retries exhausted) says nothing about the path, and a caller
    /// that moved on to another identity source would let host resources
    /// choose the identity.
    ///
    /// Injected rather than performed here for the reason `handle_stat_family`
    /// gives: an access from the tracer can hang on some FUSE filesystems
    /// (squashfs_ll), and on in-guest backends "here" is the guest process
    /// anyway. The guest resolves the path in its own mount namespace and
    /// root, which is also the root `maps` printed the path relative to. The
    /// injection is not a guest-visible system call, charges no virtual time,
    /// and leaves no host inode number or timestamp in guest memory.
    ///
    /// The path and the `struct stat` are staged in one of two scratches, as
    /// `inject_fstat` stages its buffer:
    ///
    /// 1. The guest stack scratch, for a path shorter than
    ///    `GUEST_STAT_PATH_CAPACITY` bytes. On the ptrace backend it lies below
    ///    the red zone under the guest's stack pointer, and an injection there
    ///    has 896 bytes, which must hold the path buffer and the 144-byte
    ///    `struct stat`. That memory is the guest's own: a guest that issues
    ///    raw system calls may keep live data below its red zone, and Linux
    ///    never writes there for it. So the bytes under both buffers are read
    ///    before the scratch is committed and written back afterwards, on
    ///    success, on a failed stat and after a fault alike, before the
    ///    scratch guard drops (DBT and SaBRe free their Tool-owned arena
    ///    then; LiteInst and e9patch keep it only until the current dispatch
    ///    ends). A scratch whose bytes cannot be read is not used.
    ///    That memory need not be writable either, for the reasons
    ///    `inject_fstat` gives. The fault belongs to Detcore's bookkeeping,
    ///    not to the guest (<https://github.com/rrnewton/hermit/issues/3328>),
    ///    so an `EFAULT` from reading the scratch, committing it, staging the
    ///    path, the `fstatat` itself (whose path and buffer are both
    ///    Detcore's) or writing either buffer back sends the stat to the
    ///    transient page below. Both buffers are first written back as far as
    ///    they are writable: every write into the scratch, the commit's and
    ///    the kernel's included, runs forward from a buffer's start and stops
    ///    at its first fault, so whatever it changed is a prefix that writing
    ///    back from the same start reaches. Every other error is an error of
    ///    the whole stat (see below).
    /// 2. A private anonymous mapping made for this call alone, of whole
    ///    pages sized for the NUL-terminated path and an 8-aligned
    ///    `struct stat`. It serves a stack scratch that is too small, a fault
    ///    there, and every path of `GUEST_STAT_PATH_CAPACITY` bytes or more.
    ///    The guest never learns its address, and it is unmapped whole before
    ///    the guest resumes. If that `munmap` fails, which is not expected for
    ///    a page this call mapped and whose address the guest never saw,
    ///    Detcore only warns and keeps the answer, and the page stays mapped.
    ///    An `EFAULT` there is an error.
    ///
    /// ⚠️ WHEN THAT MAPPING CANNOT BE MADE, THE STAT FAILS WITH AN ERROR, and
    /// so does the call it serves (a `read` of `maps` or `smaps`, a
    /// `readlink` of `/proc/<pid>/fd/<n>`). Every failure that is not the
    /// guest filesystem's answer is returned as a tool error rather than an
    /// errno, a refusal ([`IdentityLookupRefused`]) that stops the refusing
    /// process on every backend, except `ESRCH`: that thread is gone, and
    /// nothing it would print an identity to is left. Why the `mmap` fails
    /// is partly guest state and partly host state: the guest's `RLIMIT_AS`
    /// and its mapping count are the guest's, but the limit on that count
    /// (`vm.max_map_count`) is a host setting, and a strict overcommit policy
    /// (`vm.overcommit_memory=2`) refuses the page on the host's commit
    /// charge. A caller that treated the failure as "no such file" and keyed
    /// on its fallback identity would key differently on such a host than
    /// elsewhere, and every deterministic inode minted after it would shift.
    /// A host-resource failure may stop a run; it must not change a minted
    /// identity. Detcore logs why at `warn`. No page is reserved up front
    /// instead, because a page kept mapped in the guest would itself be a
    /// line of the guest's `maps`; the pinned KVM backend's reserved
    /// `ToolScratch` memory is outside the guest's view and has no such
    /// cost.
    pub(crate) async fn stat_guest_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &[u8],
    ) -> Result<Option<libc::stat>, Error> {
        Self::stat_guest_path_at(guest, StatAt::path(), path).await
    }

    /// [`Self::stat_guest_path`], resolving `path` from `at.dirfd` with
    /// `at.flags`: the same scratches, answers and refusals.
    async fn stat_guest_path_at<G: Guest<Self>>(
        guest: &mut G,
        at: StatAt,
        path: &[u8],
    ) -> Result<Option<libc::stat>, Error> {
        if path.contains(&0) {
            return Ok(None);
        }
        let asked = if path.len() < GUEST_STAT_PATH_CAPACITY {
            match Self::stat_guest_path_on_stack(guest, at, path).await {
                Ok(StackStat::Answered(identity)) => Ok(identity),
                Ok(StackStat::Unusable) => {
                    Self::stat_guest_path_in_transient_page(guest, at, path).await
                }
                Err(errno) => Err(errno),
            }
        } else {
            Self::stat_guest_path_in_transient_page(guest, at, path).await
        };
        asked.map_err(|errno| match errno {
            // The thread is gone, so nothing it would print an identity to
            // is left; the errno is returned as before.
            Errno::ESRCH => Error::Errno(errno),
            // Not an `Error::Errno`: the guest's call must not see a host
            // resource's errno as its own result. The refusal stops the
            // refusing process on every backend.
            errno => identity_lookup_refused(at.verb(), path, errno),
        })
    }

    /// The fast path of [`Self::stat_guest_path_at`]: the path and the
    /// `struct stat` live in the guest stack scratch.
    async fn stat_guest_path_on_stack<G: Guest<Self>>(
        guest: &mut G,
        at: StatAt,
        path: &[u8],
    ) -> Result<StackStat, Errno> {
        let mut stack = guest.stack().await;
        let needed = GUEST_STAT_PATH_CAPACITY + std::mem::size_of::<libc::stat>();
        if stack.capacity().saturating_sub(stack.size()) < needed {
            trace!(
                "guest stack scratch has no room for a stat of {:?}",
                String::from_utf8_lossy(path)
            );
            return Ok(StackStat::Unusable);
        }
        let path_address: AddrMut<[u8; GUEST_STAT_PATH_CAPACITY]> = stack.reserve();
        let statptr = StatPtr(stack.reserve());
        // On ptrace both buffers are the guest's own stack memory below its
        // red zone, which the commit, the staged path and the kernel's stat
        // overwrite. Save what is there first, so it can be put back.
        let mut saved_path = [0u8; GUEST_STAT_PATH_CAPACITY];
        let mut saved_stat = [0u8; std::mem::size_of::<libc::stat>()];
        let saved = {
            let memory = guest.memory();
            memory
                .read_exact(path_address.cast::<u8>(), &mut saved_path)
                .and_then(|()| memory.read_exact(statptr.0.cast(), &mut saved_stat))
        };
        match saved {
            Ok(()) => {}
            Err(Errno::EFAULT) => {
                trace!(
                    "guest stack scratch cannot be read for a stat of {:?}",
                    String::from_utf8_lossy(path)
                );
                return Ok(StackStat::Unusable);
            }
            Err(errno) => return Err(errno),
        }
        // Keep the guard until both buffers are written back: DBT and SaBRe
        // free their Tool-owned arena when the guard drops, and LiteInst and
        // e9patch move it to a list of committed arenas that is cleared when
        // the current dispatch ends. A failed commit is written back too: the
        // ptrace commit writes page by page, so it can fail after writing a
        // lower writable page.
        let committed = stack.commit();
        let identity = match &committed {
            Ok(_) => {
                Self::fstatat_in_scratch(guest, at, path, path_address.cast::<u8>(), statptr).await
            }
            Err(errno) => Err(*errno),
        };
        let mut memory = guest.memory();
        let restored_stat = memory.write_exact(statptr.0.cast(), &saved_stat);
        let restored_path = memory.write_exact(path_address.cast::<u8>(), &saved_path);
        drop(committed);
        // An error other than EFAULT from any step wins over an EFAULT from
        // another.
        let failure = [
            identity.as_ref().err(),
            restored_stat.as_ref().err(),
            restored_path.as_ref().err(),
        ]
        .into_iter()
        .flatten()
        .copied()
        .reduce(|kept, next| if kept == Errno::EFAULT { next } else { kept });
        match failure {
            None => identity.map(StackStat::Answered),
            Some(Errno::EFAULT) => {
                trace!(
                    "guest stack scratch faulted during a stat of {:?}",
                    String::from_utf8_lossy(path)
                );
                Ok(StackStat::Unusable)
            }
            Some(errno) => Err(errno),
        }
    }

    /// The fallback of [`Self::stat_guest_path_at`]: the path and the
    /// `struct stat` live in a private anonymous mapping made for this call
    /// only, and unmapped whole before it returns.
    async fn stat_guest_path_in_transient_page<G: Guest<Self>>(
        guest: &mut G,
        at: StatAt,
        path: &[u8],
    ) -> Result<Option<libc::stat>, Errno> {
        let stat_offset = (path.len() + 1).next_multiple_of(8);
        let len = transient_stat_page_len(path.len());
        let mapped = match guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await
        {
            Ok(mapped) => mapped,
            Err(errno) => {
                warn!(
                    "[detcore] could not map a transient page for a stat of {:?} ({}); \
                     refusing rather than keying on another identity",
                    String::from_utf8_lossy(path),
                    errno
                );
                return Err(errno);
            }
        };
        let page = usize::try_from(mapped)
            .ok()
            .and_then(AddrMut::<u8>::from_raw)
            .unwrap_or_else(|| panic!("transient stat page mmap returned {mapped}"));
        // SAFETY: `stat_offset` is within the `len` bytes just mapped.
        let statptr = StatPtr(unsafe { page.add(stat_offset) }.cast::<libc::stat>());
        let identity = Self::fstatat_in_scratch(guest, at, path, page, statptr).await;
        if let Err(errno) = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(page.cast::<libc::c_void>().into()))
                    .with_len(len),
            ))
            .await
        {
            // Not expected: the page was mapped by this call and its address
            // never reached the guest. The answer is still valid, so a
            // leftover page is no reason to discard it.
            warn!(
                "[detcore] could not unmap the transient stat page for {:?}: {}",
                String::from_utf8_lossy(path),
                errno
            );
        }
        if let Err(errno) = identity {
            warn!(
                "[detcore] the stat of {:?} in a transient page could not be asked ({}); \
                 refusing rather than keying on another identity",
                String::from_utf8_lossy(path),
                errno
            );
        }
        identity
    }

    /// The body of [`Self::stat_guest_path`] once a scratch is in place: stage
    /// `path` NUL-terminated at `path_address` and inject
    /// `fstatat(at.dirfd, path, statptr, at.flags)`. `Ok(None)` when the guest's
    /// filesystem answers that the path names nothing the stat reaches
    /// (`ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`, `EACCES`);
    /// `Err(EFAULT)` when the scratch faults, from the kernel as from a
    /// staging write, since the path and the buffer are both Detcore's; and
    /// the errno itself for every other failure, which is not an answer
    /// about the path.
    async fn fstatat_in_scratch<G: Guest<Self>>(
        guest: &mut G,
        at: StatAt,
        path: &[u8],
        path_address: AddrMut<'_, u8>,
        statptr: StatPtr<'_>,
    ) -> Result<Option<libc::stat>, Errno> {
        // The NUL is staged with the path: the stack scratch holds whatever
        // the guest left there until the commit, and some backends' commits
        // write nothing.
        let mut staged = Vec::with_capacity(path.len() + 1);
        staged.extend_from_slice(path);
        staged.push(0);
        guest.memory().write_exact(path_address, &staged)?;
        let call = syscalls::Fstatat::new()
            .with_dirfd(at.dirfd)
            .with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ))
            .with_stat(Some(statptr))
            .with_flags(at.flags);
        match guest.inject_with_retry(call).await {
            Ok(_) => statptr.read(&guest.memory()).map(Some),
            Err(Errno::EFAULT) => Err(Errno::EFAULT),
            Err(
                error @ (Errno::ENOENT
                | Errno::ENOTDIR
                | Errno::ELOOP
                | Errno::ENAMETOOLONG
                | Errno::EACCES),
            ) => {
                trace!(
                    "guest stat of {:?} failed: {error}",
                    String::from_utf8_lossy(path)
                );
                Ok(None)
            }
            Err(error) => {
                warn!(
                    "[detcore] guest stat of {:?} could not be asked: {error}",
                    String::from_utf8_lossy(path)
                );
                Err(error)
            }
        }
    }

    // helper function to track a new file descriptor.
    pub(crate) async fn add_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<(), Errno> {
        self.add_fd_with_stat(guest, fd, flags, ty).await.map(drop)
    }

    /// [`Self::add_fd`], also returning the host `fstat` it took of `fd`, which
    /// it takes only when metadata is virtualized.
    async fn add_fd_with_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<Option<libc::stat>, Errno> {
        let host_stat = if guest.config().virtualize_metadata {
            match self.inject_fstat(guest, fd).await {
                Ok(stat) => Some(stat),
                Err(errno) => {
                    // `fd` is already open in the guest, but Detcore cannot
                    // model it: with metadata virtualization on, a descriptor
                    // without its stat is an invariant violation. The caller
                    // reports this error as the syscall's result, so close
                    // the descriptor rather than leave an untracked one open
                    // behind that error. Not retried on EINTR: Linux releases
                    // the descriptor even then, and a retry could close a
                    // reused number.
                    if let Err(close_errno) = guest.inject(syscalls::Close::new().with_fd(fd)).await
                    {
                        warn!(
                            "[detcore] could not close fd {} after failing to record its \
                             metadata ({}): {}",
                            fd, errno, close_errno
                        );
                    }
                    return Err(errno);
                }
            }
        } else {
            None
        };
        guest
            .thread_state()
            .add_fd(fd, flags, ty, host_stat.map(Into::into))?;
        Ok(host_stat)
    }

    pub(crate) async fn release_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
    ) -> Option<u16> {
        let response = send_and_update_time(guest, GlobalRequest::ReleasePort(open_file_id)).await;
        match response.1 {
            GlobalResponse::ReleasePort(port) => port,
            other => panic!("unexpected release-port response: {other:?}"),
        }
    }

    pub(crate) async fn restore_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
        port: u16,
    ) {
        let response =
            send_and_update_time(guest, GlobalRequest::AddUsedPort(port, open_file_id)).await;
        match response.1 {
            GlobalResponse::AddUsedPort => {}
            other => panic!("unexpected restore-port response: {other:?}"),
        }
    }

    /// `path`, as a guest syscall named it relative to `dirfd`, made
    /// absolute in the guest's view. A relative spelling is not the object:
    /// absolute paths are already bound, a dirfd supplies its own prefix, and
    /// AT_FDCWD-relative spellings are resolved through the guest's root and
    /// cwd, so the result does not depend on Replayer's placeholder
    /// descriptor. A spelling that cannot be resolved is kept as given.
    fn observed_path<G: Guest<Self>>(guest: &G, dirfd: i32, path: &Path) -> Result<PathBuf, Error> {
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else if dirfd == libc::AT_FDCWD {
            resolved_at_fdcwd_path(guest.pid().as_raw(), path).unwrap_or_else(|| path.to_path_buf())
        } else {
            guest
                .thread_state()
                .with_detfd(dirfd, |detfd| detfd.path())?
                .map_or_else(|| path.to_path_buf(), |directory| directory.join(path))
        })
    }

    /// For `hermit run --verify`, report each path `call` may rebind in the
    /// guest's file namespace: both names of a rename, the new name of a link
    /// or symlink, an unlinked name, and a node, directory or file it makes
    /// or removes (see `detcore_model::host_input::HostMutationRecord`). A
    /// host file whose path the guest rebinds may have been replaced by the
    /// guest itself. Called before the call runs, whatever its outcome.
    pub(crate) async fn record_host_namespace_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) {
        let at = libc::AT_FDCWD;
        let changed: Vec<(i32, Option<syscalls::PathPtr>)> = match call {
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rename(c) => vec![(at, c.oldpath()), (at, c.newpath())],
            Syscall::Renameat(c) => vec![(c.olddirfd(), c.oldpath()), (c.newdirfd(), c.newpath())],
            Syscall::Renameat2(c) => {
                vec![(c.olddirfd(), c.oldpath()), (c.newdirfd(), c.newpath())]
            }
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Link(c) => vec![(at, c.newpath())],
            Syscall::Linkat(c) => vec![(c.newdirfd(), c.newpath())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Unlink(c) => vec![(at, c.path())],
            Syscall::Unlinkat(c) => vec![(c.dirfd(), c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Symlink(c) => vec![(at, c.linkpath())],
            Syscall::Symlinkat(c) => vec![(c.newdirfd(), c.linkpath())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Mknod(c) => vec![(at, c.path())],
            Syscall::Mknodat(c) => vec![(c.dirfd(), c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Mkdir(c) => vec![(at, c.path())],
            Syscall::Mkdirat(c) => vec![(c.dirfd(), c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rmdir(c) => vec![(at, c.path())],
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Creat(c) => vec![(at, c.path())],
            _ => return,
        };
        for (dirfd, path) in changed {
            let observed = match path.map(|path| path.read(&guest.memory())) {
                Some(Ok(path)) => {
                    let path: PathBuf = path;
                    Self::observed_path(guest, dirfd, &path).ok()
                }
                _ => None,
            };
            crate::tool_global::record_host_mutation(guest, rebound_path(observed)).await;
        }
    }

    /// The host inodes whose last name `call` would remove: the name an
    /// `unlink`, `unlinkat` or `rmdir` removes, or the target a `rename`,
    /// `renameat` or `renameat2` replaces (not with `RENAME_EXCHANGE`), when
    /// it is a directory or a file with one link (see [`retires_on_removal`]).
    /// Each name is stat'ed with an injected `fstatat(AT_SYMLINK_NOFOLLOW)`
    /// before the call runs; a name that does not resolve removes nothing.
    /// Pass the result to [`Self::retire_removed_inodes`] if the call
    /// succeeds. Only with virtualized metadata, the only mode that numbers
    /// inodes.
    ///
    /// Nothing is retired without sequentialized threads. Only then do the
    /// lookups, the call and the retirement run in one turn; otherwise another
    /// thread can link, unlink or rename the file in between, and a file that
    /// keeps a name could be retired and renumbered (codex review of
    /// <https://github.com/rrnewton/hermit/pull/3849>, P2). Without them inode
    /// numbers behave as before retirement existed.
    pub(crate) async fn last_names_removed_by<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Vec<RawInode> {
        if !guest.config().virtualize_metadata || !guest.config().sequentialize_threads {
            return Vec::new();
        }
        let at = libc::AT_FDCWD;
        // The name removed, and for a rename the source, which replaces
        // nothing when it is the target's own file (`rename(a, a)`, or two
        // links of one file).
        let (removed, source) = match call {
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Unlink(c) => ((at, c.path()), None),
            Syscall::Unlinkat(c) => ((c.dirfd(), c.path()), None),
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rmdir(c) => ((at, c.path()), None),
            #[cfg(not(target_arch = "aarch64"))]
            Syscall::Rename(c) => ((at, c.newpath()), Some((at, c.oldpath()))),
            Syscall::Renameat(c) => (
                (c.newdirfd(), c.newpath()),
                Some((c.olddirfd(), c.oldpath())),
            ),
            Syscall::Renameat2(c) if c.flags() & libc::RENAME_EXCHANGE == 0 => (
                (c.newdirfd(), c.newpath()),
                Some((c.olddirfd(), c.oldpath())),
            ),
            _ => return Vec::new(),
        };
        // Every path the call reads, which the lookups must leave unchanged.
        let inputs = [removed.1, source.and_then(|source| source.1)];
        let Some(path) = removed.1 else {
            return Vec::new();
        };
        let Ok(stat) = Self::inject_lstatat(guest, removed.0, path, inputs).await else {
            return Vec::new();
        };
        let inode = RawInode::new(stat.st_dev, stat.st_ino);
        if let Some((dirfd, Some(path))) = source
            && Self::inject_lstatat(guest, dirfd, path, inputs)
                .await
                .is_ok_and(|source| RawInode::new(source.st_dev, source.st_ino) == inode)
        {
            return Vec::new();
        }
        if retires_on_removal(&stat) {
            vec![inode]
        } else {
            Vec::new()
        }
    }

    /// Retire the mappings of `inodes`, whose last names a successful call
    /// removed (see [`Self::last_names_removed_by`] and `InodePool::retire`).
    pub(crate) async fn retire_removed_inodes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        inodes: Vec<RawInode>,
    ) {
        for inode in inodes {
            retire_inode(guest, inode).await;
        }
    }

    /// For `hermit run --verify` on a backend whose guest holds code that
    /// makes untraced syscalls (`Config::untraced_code_range`): report `/`
    /// when `call` may change the memory holding it (see
    /// [`may_change_untraced_code`]). Intact, that code faults right after
    /// its syscall, and the fault is reported (`handle_signal_event`); a
    /// guest that rewrote or replaced it could use it without faulting, so
    /// Detcore could not establish what the guest changed, and `/`, the
    /// ancestor of every path, keeps a host input change from being named
    /// in that run. Called before the call runs, whatever its outcome.
    pub(crate) async fn record_untraced_code_change<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) {
        let Some(range) = guest.config().untraced_code_range else {
            return;
        };
        let (number, args) = call.into_parts();
        if may_change_untraced_code(number, &args, range, default_huge_page_size()) {
            crate::tool_global::record_host_mutation(guest, String::from("/")).await;
        }
    }

    /// Openat system call.
    pub async fn handle_openat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?;
        let path: PathBuf = path.read(&guest.memory())?;
        // A relative spelling is not the object. `chdir("/sys/module/kvm");
        // open("refcnt")` and an absolute open name the SAME kernel object, so
        // classifying the unresolved lexical pathname lets one spelling bypass
        // normalization and expose the host value. Absolute paths are already
        // bound; a dirfd supplies its own prefix; AT_FDCWD-relative spellings
        // are resolved through the guest's root and cwd before the open so the
        // result does not depend on Replayer's placeholder descriptor.
        let observed_path = Self::observed_path(guest, call.dirfd(), &path)?;
        if guest.config().record_host_inputs && call.flags().contains(OFlag::O_CREAT) {
            // It may create the file it opens. Reported before the open runs,
            // as `record_host_namespace_change` reports the other changes.
            crate::tool_global::record_host_mutation(
                guest,
                rebound_path(Some(observed_path.clone())),
            )
            .await;
        }

        let resource = ResourceID::Path(path.clone());
        // Ask for permission to resolve this path into a file:
        let request = guest.thread_state().mk_request(resource, Permission::R);
        resource_request(guest, request).await;
        // Signal phase 1 (`sigalrm_phase1`): in a process that handles SIGALRM,
        // an open that may wait (a FIFO, a character or block device) is
        // refused. Checked after the grant: no other guest runs before the open.
        if guest.thread_state().sigalrm_handled
            && !call.flags().intersects(OFlag::O_DIRECTORY | OFlag::O_PATH)
            && let Some(path) = call.path()
            && crate::sigalrm_phase1::open_refused(
                Self::inject_statat(
                    guest,
                    call.dirfd(),
                    path,
                    [Some(path), None],
                    AtFlags::empty(),
                )
                .await,
                call.flags().contains(OFlag::O_NONBLOCK),
            )
        {
            return Err(Errno::EOPNOTSUPP.into());
        }
        let res = self.record_or_replay(guest, Syscall::Openat(call)).await;

        match res {
            Ok(fd) => {
                let fd = fd as RawFd;
                // Under a network trace mode only; the run ends before the
                // guest sees the descriptor.
                let pid = guest.pid().as_raw();
                self.network_check_open(guest, &observed_path, || resolved_open_path(pid, fd))
                    .await;
                let fd_type = path.to_str().map_or(FdType::Regular, |fname| {
                    if fname == "/dev/random" || fname == "/dev/urandom" {
                        FdType::Rng
                    } else {
                        FdType::Regular
                    }
                });
                // A guest-created pipe reopened by path (`/dev/stdin`,
                // `/proc/self/fd/N`, bash's `< <(cmd)` as `/dev/fd/63`) is a NEW open
                // file description, so it carries neither the Pipe type nor the
                // physical O_NONBLOCK that `handle_pipe2` gave the original. Left as a
                // physically blocking Regular fd, a read that waits for a writer blocks
                // in the kernel while holding the scheduler turn, and the writer never
                // runs (https://github.com/rrnewton/hermit/issues/1850). Give it the
                // same treatment as `handle_pipe2`: Pipe type plus a Detcore-internal
                // physical O_NONBLOCK, which F_GETFL hides from the guest. Only a pipe
                // this process already holds as scheduler-managed qualifies; a host
                // pipe or a named FIFO keeps `deterministic_read`'s Regular handling.
                // Gated like the forced O_NONBLOCK in F_SETFL: Replayer's descriptor
                // is an eventfd placeholder, so record and replay would not classify
                // alike.
                let fd_type = if fd_type == FdType::Regular
                    && self.cfg.use_nonblocking_sockets()
                    && !self.cfg.recordreplay_modes
                    && !call.flags().contains(OFlag::O_PATH)
                    && reopens_scheduler_managed_pipe(
                        guest.pid().as_raw(),
                        fd,
                        &guest.thread_state().scheduler_managed_pipe_fds(),
                    )
                    && (call.flags().contains(OFlag::O_NONBLOCK)
                        || self.inject_physical_nonblocking(guest, fd).await.is_ok())
                {
                    FdType::Pipe
                } else {
                    fd_type
                };
                let host_stat = self
                    .add_fd_with_stat(guest, fd, call.flags(), fd_type)
                    .await?;
                if let (true, Some(stat)) = (guest.config().record_host_inputs, &host_stat) {
                    crate::tool_global::record_host_input(
                        guest,
                        observed_path.to_string_lossy().into_owned(),
                        detcore_model::host_input::HostFileIdentity::from_stat(stat),
                    )
                    .await;
                }
                // An open that finds its file linked, or creates it
                // (`O_TMPFILE`), reaches a file that has a name or never had
                // one, so a retired mapping of its inode described a file the
                // host has freed (https://github.com/rrnewton/hermit/issues/3840).
                // Reopening an unlinked file through `/proc/self/fd/N` finds no
                // link and keeps it.
                if let Some(stat) = &host_stat
                    && (stat.st_nlink > 0 || call.flags().contains(OFlag::O_TMPFILE))
                {
                    forget_retired_inode(guest, RawInode::new(stat.st_dev, stat.st_ino)).await;
                }
                // A descriptor that writes process memory can rewrite the
                // untraced code (see `record_untraced_code_change`). Reported
                // once the open returns, before the guest has the descriptor;
                // the record has no position in the run, so it covers every
                // write through it.
                if guest.config().record_host_inputs
                    && guest.config().untraced_code_range.is_some()
                    && may_write_process_memory(
                        call.flags(),
                        descriptor_identity(guest.tid().as_raw(), fd)
                            .as_ref()
                            .map(|(link, on_procfs)| (link.as_path(), *on_procfs)),
                    )
                {
                    crate::tool_global::record_host_mutation(guest, String::from("/")).await;
                }
                // A session leader with no controlling terminal that opens a
                // terminal without O_NOCTTY gains it as one, and the terminal
                // can then signal the session and its foreground process group
                // at moments set by host timing (`terminal_signals`). Hold
                // those signals in every process, as `handle_ioctl` does for
                // TIOCSCTTY and TIOCGPTPEER. Only the kernel knows whether this
                // open made the terminal controlling, so the record is made
                // after the call; it is still this thread's turn, and no other
                // thread's gated wait reads the record before the turn ends.
                // Skipped under record and replay: Replayer's descriptor is a
                // placeholder that never becomes a terminal, so the two runs
                // would not record alike.
                if guest
                    .config()
                    .backend_supports_blocked_wait_signal_interruption
                    && !self.cfg.recordreplay_modes
                    && open_can_acquire_controlling_terminal(call.flags())
                    && opened_controlling_terminal(guest.pid().as_raw(), fd, host_stat.as_ref())
                {
                    record_host_timed_signals(
                        guest,
                        HostTimedSignalScope::Container,
                        terminal_signals(),
                    )
                    .await;
                }
                if fd_type == FdType::Pipe {
                    self.maybe_set_nonblocking_fd(guest, fd);
                }
                // Classify the spelling the guest used FIRST. Several kinds are
                // defined by that spelling and MUST keep it: `/proc/self/...`,
                // `/proc/thread-self/...` and the mountinfo aliases all resolve
                // through `/proc/<pid>/fd/<fd>` to a numeric `/proc/<pid>/...`
                // path, which is a DIFFERENT (or absent) classification. So
                // resolution must never overwrite a spelling that already
                // classifies.
                //
                // Only when the spelling yields nothing do we ask the kernel
                // what the descriptor actually names. That is exactly the
                // AT_FDCWD/alias gap -- `chdir("/sys/module/kvm"); open("refcnt")`
                // classifies as nothing lexically -- and scoping it this way
                // makes the fallback MONOTONE: it can only add a classification
                // where there was none, never change one that already existed.
                let mut procfs = ProcfsFile::from_path(&observed_path).or_else(|| {
                    resolved_open_path(guest.pid().as_raw(), fd)
                        .filter(|resolved| resolved != &observed_path)
                        .and_then(|resolved| ProcfsFile::from_path(&resolved))
                });
                if let Some(procfs) = procfs.as_mut() {
                    // A `/proc/self/maps` descriptor shows the opener's address
                    // space even when a forked child reads it, or the opener
                    // after an `execve`.
                    Self::bind_procfs_mapping_subject(guest, procfs).await?;
                }
                if procfs
                    .as_ref()
                    .is_some_and(ProcfsFile::needs_bound_thread_identity)
                {
                    // TODO-HUMAN-REVIEW(PR-964): Bind thread-self at open time,
                    // matching procfs inode resolution even if another thread or
                    // a forked process later reads the shared descriptor.
                    let tgid = guest.inject(syscalls::Getpid::new()).await? as i32;
                    let tid = guest.inject(syscalls::Gettid::new()).await? as i32;
                    let ppid = guest.inject(syscalls::Getppid::new()).await? as i32;
                    procfs
                        .as_mut()
                        .expect("thread identity request lost its procfs file")
                        .bind_thread_identity(tgid, tid, ppid);
                }
                if procfs
                    .as_ref()
                    .and_then(ProcfsFile::timer_slack_target)
                    .is_some()
                {
                    let target = procfs
                        .as_ref()
                        .and_then(ProcfsFile::timer_slack_target)
                        .expect("timer-slack target disappeared");
                    let stat = match guest.thread_state().with_detfd(fd, |detfd| detfd.stat())? {
                        Some(stat) => libc::stat::from(&stat),
                        None => self.inject_fstat(guest, fd).await?,
                    };
                    // Recorder opens a real proc inode, while Replayer reserves
                    // the recorded descriptor number with an anonymous eventfd.
                    // A real proc descriptor is the strongest open-time task
                    // incarnation witness. For a virtual replay descriptor,
                    // bind the live numeric proc path instead; every operation
                    // re-resolves that same path, so exit or TID reuse still
                    // changes the inode and returns ESRCH.
                    let identity = if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
                        (stat.st_dev, stat.st_ino)
                    } else {
                        self.observe_timer_slack_identity(guest, target)
                            .await?
                            .unwrap_or((stat.st_dev, stat.st_ino))
                    };
                    procfs
                        .as_mut()
                        .expect("timer-slack classification disappeared")
                        .bind_timer_slack_identity(identity.0, identity.1);
                }
                // Signal phase 1: what the kernel says this file is, for the
                // table's descriptor rules (`sigalrm_phase1`).
                let provenance = guest
                    .thread_state()
                    .sigalrm_handled
                    .then(|| crate::sigalrm_phase1::kernel_provenance(guest.pid().as_raw(), fd));
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_path(&observed_path);
                    if let Some(procfs) = procfs.clone() {
                        detfd.set_procfs(procfs);
                    }
                    if let Some(provenance) = provenance {
                        detfd.set_sigalrm_phase1_provenance(provenance);
                    }
                })?;
                resource_release_all(guest).await;
                Ok(fd as i64)
            }
            // TODO: audit for error-nondeterminism:
            Err(e) => {
                resource_release_all(guest).await;
                Err(e.into())
            }
        }
    }

    /// SYS_close system call.
    pub async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let res = self.record_or_replay(guest, call).await;
        let fd_was_released = !matches!(res, Err(Errno::EBADF) | Err(Errno::ERESTARTSYS));
        if fd_was_released {
            if let Some(open_file_id) = guest.thread_state_mut().remove_fd(fd) {
                self.release_port_for_open_file(guest, open_file_id).await;
            }
            trace!("Closed {}", fd);
        }
        res.map_err(Error::from)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-838): Review close_range descriptor-table synchronization.
    /// Close a contiguous descriptor range and mirror successful closes in Detcore.
    ///
    /// The pinned Reverie revision exposes close_range as `Syscall::Other`. The
    /// common flags=0 operation cannot block and is deterministic for the
    /// process-local descriptor table. CLOSE_RANGE_UNSHARE and
    /// CLOSE_RANGE_CLOEXEC need separate shared-table modeling, so return ENOSYS
    /// for nonzero flags rather than letting strict execution silently diverge.
    pub async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let Syscall::Other(_, args) = call else {
            unreachable!("close_range unexpectedly gained a typed variant")
        };
        let first = args.arg0 as u32;
        let last = args.arg1 as u32;
        let flags = args.arg2 as u32;
        if flags != 0 {
            return Err(Errno::ENOSYS.into());
        }

        let result = self.record_or_replay(guest, call).await;
        if result.is_ok() {
            let released = guest.thread_state_mut().remove_fd_range(first, last);
            for open_file_id in released {
                self.release_port_for_open_file(guest, open_file_id).await;
            }
        }
        result.map_err(Error::from)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// Advisory whole-file locks, forwarded to the kernel.
    ///
    /// This was previously an unconditional no-op success, justified by the
    /// claim that "an advisory whole-file lock is never contended within the
    /// serialized container". That is false: serializing guest threads stops
    /// them EXECUTING simultaneously, it does not stop their lock HOLD
    /// INTERVALS from overlapping. A holder that is descheduled -- because it
    /// blocked, forked, or simply used up its timeslice -- keeps holding while
    /// another process runs and observes the lock. Measured before this change,
    /// on both ptrace and DBI, two processes held the same `LOCK_EX`
    /// simultaneously while native correctly returned `EWOULDBLOCK`.
    ///
    /// A no-op is the wrong failure direction for a determinism tool. It is
    /// deterministically wrong, so double-run verification cannot see it, and it
    /// silently removes mutual exclusion from every guest that uses a lockfile.
    ///
    /// Forwarding is what `fcntl` already does for POSIX record locks, which is
    /// why those work. The guest's descriptor is a real host descriptor, so the
    /// kernel supplies the whole contract for free and consistently with itself:
    /// shared vs exclusive, `LOCK_NB`, upgrade/downgrade (which Linux performs
    /// non-atomically -- see below, this handler compensates), release on
    /// `LOCK_UN`, release when the last descriptor for the open file
    /// description is closed, and release on process exit.
    ///
    /// Determinism, scoped to what is actually true. When every contender is
    /// inside the container the outcome is a function of which guest holds the
    /// lock, and that is fixed by Detcore's deterministic schedule, so a given
    /// program and seed produce the same acquisition outcome every run.
    ///
    /// The scope is not decoration. Because this forwards to the kernel, a
    /// process OUTSIDE the container holding a lock on a guest-visible file
    /// does change the guest's result -- measured: with a host `flock -x`
    /// holder, a guest `LOCK_EX|LOCK_NB` returns `EWOULDBLOCK`, and acquires
    /// without one. That is a host-state leak, it is faithful to Linux, and it
    /// is the same leak `fcntl` record locks have always had here. Hermit
    /// already declines to make a mutating external filesystem deterministic,
    /// and lock state on a shared file is part of that state. Do not restate
    /// this as "no host state enters the decision": it does, and the previous
    /// bug in this very function came from writing down a determinism argument
    /// that was broader than the truth.
    ///
    /// Note that the no-op this replaced was not host-independent in any useful
    /// sense either -- it was host-independent by being wrong in all cases.
    ///
    /// # Why a blocking request is probed non-blockingly, and what that costs
    ///
    /// A guest thread parked inside a kernel `flock` is not visible to the
    /// deterministic scheduler as blocked, so nothing runs to release the lock
    /// and the whole container wedges -- measured: a four-way contention guest
    /// that completes natively hung indefinitely under a plain forwarding
    /// implementation. So a blocking operation is rewritten to `LOCK_NB` and,
    /// if it turns out to be contended, refused rather than hung.
    ///
    /// That rewrite is not free, and the cost is a *lock the guest already
    /// owns*. Linux converts an `flock` lock in place and the conversion is not
    /// atomic: `flock_lock_inode` deletes this open file description's existing
    /// lock **before** it scans for a conflict, so a contended `LOCK_SH` ->
    /// `LOCK_EX` conversion leaves the caller holding nothing and then reports
    /// `EWOULDBLOCK`. Natively the guest never observes that intermediate
    /// state, because a *blocking* request would sleep and eventually acquire.
    /// Under the rewrite it would: the guest asked to wait, got told "no", and
    /// silently lost the shared lock it was already relying on.
    ///
    /// So this handler restores the prior mode before refusing a *blocking*
    /// conversion, making the refusal side-effect-free. It deliberately does
    /// **not** restore when the guest itself passed `LOCK_NB`: there the drop is
    /// exactly what Linux does, and re-acquiring would be a divergence in the
    /// other direction. `DetFd::flock_mode` is what makes the two cases
    /// distinguishable -- it records the mode Detcore last saw the kernel grant
    /// for this open file description, so a first acquisition (nothing to lose)
    /// is not confused with a conversion (something to lose).
    ///
    /// That cache covers locks Detcore granted while it had sole knowledge of
    /// the open file description. State becomes permanently unknown when the
    /// descriptor is inherited across a process fork, discovered after tracing
    /// begins, or received through `SCM_RIGHTS`, because another process can
    /// change that shared kernel lock without updating this cache. A blocking
    /// conversion in unknown state is refused before the nonblocking probe, so
    /// the refusal cannot destroy a lock Detcore cannot restore.
    pub async fn handle_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
    ) -> Result<i64, Error> {
        const LOCK_NB: i32 = libc::LOCK_NB;
        /// `LOCK_SH`/`LOCK_EX`/`LOCK_UN` with `LOCK_NB` (and any padding) masked off.
        const MODE_MASK: i32 = libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN;

        let (fd, operation) = (call.fd(), call.operation());
        let requested = operation & MODE_MASK;
        let caller_wants_nonblocking = operation & LOCK_NB != 0;
        let releasing = requested == libc::LOCK_UN;
        let valid_operation = operation & !(MODE_MASK | LOCK_NB) == 0
            && matches!(requested, libc::LOCK_SH | libc::LOCK_EX | libc::LOCK_UN);
        let dettid = guest.thread_state().dettid;

        // Preserve kernel validation for malformed operations. In particular,
        // an unknown descriptor with an invalid mode must report EINVAL rather
        // than being mistaken for a valid blocking request and refused with
        // ENOLCK.
        if !valid_operation {
            return self
                .record_or_replay(guest, call)
                .await
                .map_err(Error::from);
        }

        // The mode Detcore last saw the kernel grant this open file description.
        let known_held = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.known_flock_mode())
            .unwrap_or(None);

        // A nonblocking conversion is allowed to have Linux's documented
        // non-atomic side effect. A blocking conversion is not: if this open
        // file description was inherited or received and its prior mode is
        // unknown, probing could silently drop a lock we cannot restore.
        // Validate the descriptor without changing lock state, then refuse the
        // uncertain blocking operation before issuing flock at all.
        if !caller_wants_nonblocking && !releasing && known_held.is_none() {
            guest
                .inject_with_retry(Syscall::Fcntl(
                    syscalls::Fcntl::new().with_fd(fd).with_cmd(F_GETFD),
                ))
                .await?;
            error!(
                "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) refused: \
                 this open file description existed before Detcore observed its lock state, \
                 so a nonblocking probe could destroy a lock that cannot be restored. Use \
                 LOCK_NB, or run without --strict to receive ENOLCK."
            );
            return self
                .refuse_unserviceable_operation(guest, Sysno::flock, Errno::ENOLCK)
                .await;
        }
        let held = known_held.flatten();

        // LOCK_UN cannot block, so it is forwarded exactly as the guest wrote it.
        let probe_operation = if releasing {
            operation
        } else {
            operation | LOCK_NB
        };
        let result = self
            .record_or_replay(guest, call.with_operation(probe_operation))
            .await;

        match result {
            Ok(value) => {
                let granted = if releasing { None } else { Some(requested) };
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_flock_mode(granted));
                trace!(
                    "flock(fd={}, operation={:#x}) served, open file now holds {:?}",
                    fd, operation, granted
                );
                Ok(value)
            }
            Err(Errno::EWOULDBLOCK) if caller_wants_nonblocking => {
                // Exactly what the guest asked for, including Linux's own
                // non-atomic conversion behavior: if this was a conversion, the
                // kernel really did drop the prior lock on the way to failing,
                // so the cache must forget it rather than claim a lock the
                // guest no longer holds.
                if held.is_some_and(|held| held != requested) {
                    let _ = guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_flock_mode(None));
                }
                trace!("flock(fd={}, operation={:#x}) would block", fd, operation);
                Err(Errno::EWOULDBLOCK.into())
            }
            Err(Errno::EWOULDBLOCK) => {
                // The guest asked to wait, and Detcore substituted a probe.
                // Undo the probe's collateral damage before refusing.
                if let Some(previous) = held.filter(|previous| *previous != requested) {
                    let restore = call.with_operation(previous | LOCK_NB);
                    match self.record_or_replay(guest, restore).await {
                        Ok(_) => {
                            warn!(
                                "[dtid {dettid}] contended blocking flock(fd={fd}, \
                                 operation={operation:#x}) refused; restored this open file's \
                                 prior {previous:#x} lock, which Linux's non-atomic conversion \
                                 had dropped. The guest holds exactly what it held before the \
                                 call."
                            );
                        }
                        Err(err) => {
                            let _ = guest
                                .thread_state()
                                .with_detfd(fd, |detfd| detfd.set_flock_mode(None));
                            error!(
                                "[dtid {dettid}] contended blocking flock(fd={fd}, \
                                 operation={operation:#x}) refused, AND this open file's prior \
                                 {previous:#x} lock could not be restored ({err}). Linux's \
                                 non-atomic conversion dropped it and something outside this \
                                 container took it in the interval. The guest has lost a lock \
                                 it held; treat any mutual exclusion it was protecting as \
                                 broken."
                            );
                        }
                    }
                }
                // Waiting faithfully needs a wait queue owned by the
                // deterministic scheduler, the way futexes are handled; until
                // that exists, refuse loudly. Returning success would recreate
                // the mutual-exclusion bug this handler was written to fix, and
                // blocking in the kernel would deadlock the container.
                error!(
                    "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) is \
                     contended, and Detcore cannot yet park a thread on a file lock \
                     deterministically. Refusing rather than granting a lock another guest \
                     holds. Use LOCK_NB, or run without --strict to receive ENOLCK."
                );
                self.refuse_unserviceable_operation(guest, Sysno::flock, Errno::ENOLCK)
                    .await
            }
            Err(err) => {
                trace!(
                    "flock(fd={}, operation={:#x}) refused: {}",
                    fd, operation, err
                );
                Err(err.into())
            }
        }
    }

    async fn snapshot_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<Vec<u8>, Error> {
        // A backend-owned read may have advanced the kernel cursor without
        // passing through Detcore's logical procfs cursor (KVM does this for
        // worker-shared descriptors). Rewind before taking the initial snapshot
        // so a later intercepted pread cannot snapshot from EOF.
        //
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#1903): ESPIPE is not a failure here. Several procfs
        // files are legitimately non-seekable -- `/proc/net/*` single-release
        // seq_files return ESPIPE from `llseek` on the host, verified natively:
        // `lseek(fd, 0, SEEK_SET)` on `/proc/net/sockstat` gives ESPIPE while the
        // subsequent `read(2)` returns data. Propagating that ESPIPE made the
        // GUEST's `read` fail on a file Linux reads fine (`cat
        // /proc/net/sockstat` -> "Illegal seek"), which is a deviation from Linux
        // semantics, not a determinism requirement: the rewind is an internal
        // correction Detcore performs for its own benefit and the guest never
        // asked for it. A non-seekable fd also cannot have been advanced behind
        // our back by a seek, and a freshly opened one is already at offset 0, so
        // skipping the rewind loses nothing the rewind was protecting.
        match guest
            .inject_with_retry(Syscall::Lseek(
                syscalls::Lseek::new()
                    .with_fd(call.fd())
                    .with_offset(0)
                    .with_whence(Whence::SEEK_SET),
            ))
            .await
        {
            Ok(_) => {}
            Err(Errno::ESPIPE) => {}
            Err(err) => return Err(err.into()),
        }

        // Capture into scratch memory, never into the caller's buffer. The
        // caller receives only the sanitized bytes, so every host byte a
        // capture read left in its buffer past that length would stay visible
        // after the read returns: all of a host `/proc/modules` chunk behind
        // its empty sanitized view
        // (https://github.com/rrnewton/hermit/issues/3815). Saving and
        // restoring the caller's buffer cannot cover every destination the
        // kernel may write, such as a PROT_WRITE-only page that the save
        // cannot read. The caller's buffer sees only the sanitized
        // publication, which checks its protection as the kernel's copy does.
        let lists_address_space = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_lists_address_space())?;
        if lists_address_space {
            self.capture_procfs_on_stack(guest, call).await
        } else {
            self.capture_procfs_in_mapping(guest, call).await
        }
    }

    /// Capture through a private anonymous mapping that is unmapped before
    /// returning, so no host byte stays in guest memory.
    async fn capture_procfs_in_mapping<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<Vec<u8>, Error> {
        const CAPTURE_CHUNK_BYTES: usize = 64 * 1024;

        let mapped = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(CAPTURE_CHUNK_BYTES)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await?;
        let mapping = usize::try_from(mapped)
            .ok()
            .and_then(AddrMut::<u8>::from_raw)
            .ok_or(Errno::EFAULT)?;
        let result = self
            .drain_procfs(guest, call, mapping, CAPTURE_CHUNK_BYTES)
            .await;
        guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(Addr::from(mapping).cast()))
                    .with_len(CAPTURE_CHUNK_BYTES),
            ))
            .await?;
        result
    }

    /// Capture a file that lists the address space (`maps`, `smaps`,
    /// `numa_maps`, `smaps_rollup`) through the guest stack scratch. A
    /// capture mapping would be listed in the snapshot, or merged into a
    /// neighbouring row, and then be unmapped before the guest sees it. The
    /// stack scratch changes no mapping; its original bytes are put back, so
    /// no host byte stays below the stack pointer either.
    ///
    /// The scratch below the red zone is not always there: a thread, fiber or
    /// alternate signal stack may put the stack pointer just above a guard page
    /// or the end of its mapping (compare [`Self::inject_fstat`]). The read
    /// needs no stack, so that must not fail it. The capture then goes through
    /// the caller's buffer, which this read writes anyway, with the same save
    /// and restore before the output is copied. If not even its first byte can
    /// be read and written, Linux copies nothing and the read fails `EFAULT`;
    /// it does so here without taking the snapshot.
    async fn capture_procfs_on_stack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<Vec<u8>, Error> {
        const CAPTURE_CHUNK_BYTES: usize = 512;

        let mut original = [0_u8; CAPTURE_CHUNK_BYTES];
        {
            let mut stack = guest.stack().await;
            let scratch = stack.reserve::<[u8; CAPTURE_CHUNK_BYTES]>().cast::<u8>();
            // `commit` writes the reserved bytes, so save them before it.
            if save_procfs_scratch(&mut guest.memory(), scratch, &mut original)
                && let Ok(guard) = stack.commit()
            {
                let result = self
                    .drain_procfs(guest, call, scratch, CAPTURE_CHUNK_BYTES)
                    .await;
                guest.memory().write_exact(scratch, &original)?;
                drop(guard);
                return result;
            }
        }

        let buffer = call.buf().ok_or(Errno::EFAULT)?;
        for chunk in [call.len().min(CAPTURE_CHUNK_BYTES), 1] {
            let original = &mut original[..chunk];
            if save_procfs_scratch(&mut guest.memory(), buffer, original) {
                let result = self.drain_procfs(guest, call, buffer, chunk).await;
                guest.memory().write_exact(buffer, original)?;
                return result;
            }
        }
        Err(Errno::EFAULT.into())
    }

    /// Read the host file to EOF through `scratch`, `chunk` bytes at a time.
    async fn drain_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        scratch: AddrMut<'_, u8>,
        chunk: usize,
    ) -> Result<Vec<u8>, Error> {
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

        let capture = call.with_buf(Some(scratch)).with_len(chunk);
        let mut contents = Vec::new();
        loop {
            let bytes_read = self.record_or_replay(guest, capture).await? as usize;
            if bytes_read == 0 {
                return Ok(contents);
            }
            if contents.len() + bytes_read > MAX_SNAPSHOT_BYTES {
                return Err(Errno::EFBIG.into());
            }
            let start = contents.len();
            contents.resize(start + bytes_read, 0);
            read_guest_exact(&guest.memory(), scratch, &mut contents[start..])?;
        }
    }

    async fn initialize_procfs_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<(), Error> {
        let raw_contents = self.snapshot_procfs(guest, call).await?;
        // The guest mount view is the launch namespace (guest mount/unshare/
        // setns are refused under Detcore). Ephemeral host FUSE seed rows
        // (per-process and named-tool seeds) are other tenants' propagated
        // runtime state, not part of that namespace; exclude the class at
        // capture so snapshot identity assignment and every later read agree
        // on the same membership.
        let contents = if guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_mountinfo_identities())?
        {
            crate::procfs::exclude_ephemeral_host_seed_mounts(&raw_contents)
        } else {
            raw_contents
        };
        let virtual_uptime_seconds = self.calculate_procfs_uptime(guest).await?;
        // Only `/proc/stat` renders btime. Computing it for every snapshot let
        // an unrepresentable boot instant refuse unrelated files such as
        // `/proc/uptime`.
        let needs_boot_time = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_boot_time())?;
        let virtual_boot_time_seconds = needs_boot_time
            .then(|| self.calculate_procfs_boot_time())
            .transpose()?;
        let virtual_realtime_seconds = i64::try_from(thread_observe_time(guest).await.as_secs())
            .map_err(|_| Errno::EOVERFLOW)?;
        // TODO-HUMAN-REVIEW(PR-863): Use configured guest memory for meminfo.
        let virtual_memory_kb = guest.config().memory / 1024;
        // TODO-HUMAN-REVIEW(PR-723): Review injected identity snapshot reads.
        let virtual_pid = guest.inject(syscalls::Getpid::new()).await? as i32;
        let virtual_ppid = guest.inject(syscalls::Getppid::new()).await? as i32;
        let virtual_pty_count = guest.thread_state().count_open_files_at_paths(&[
            std::path::Path::new("/dev/ptmx"),
            std::path::Path::new("/dev/pts/ptmx"),
        ]);
        let target_fd = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_target_fd())?;
        let fdinfo_identity = if let Some(target_fd) = target_fd {
            let (logical_flags, open_file_id, fd_type, inode_override) =
                guest.thread_state().with_detfd(target_fd, |detfd| {
                    (
                        detfd.status_flags(),
                        detfd.open_file_id(),
                        detfd.ty(),
                        deterministic_stdio_inode_for_resource(target_fd, detfd.resource()),
                    )
                })?;
            // The descriptor's own identity rather than its cached stat: an
            // inherited stdio descriptor's cached stat is a stand-in for stdin
            // (`descriptor_identity_stat`), which keyed the `ino:` line of a
            // dup of stdout, at descriptor 3 or above, on stdin's file while
            // its `fstat` reports stdout's.
            let identity_stat = self.descriptor_identity_stat(guest, target_fd).await?;
            let raw_inode = self
                .fdinfo_raw_file_id(guest, target_fd, identity_stat, &contents)
                .await?;
            let virtual_inode = match inode_override {
                Some(inode) => inode,
                None => determinize_inode(guest, raw_inode).await.0,
            };
            let raw_mount_id =
                detcore_model::procfs::parse_fdinfo_mount_id(&contents).ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "kernel returned malformed /proc/*/fdinfo without one numeric mnt_id"
                    ))
                })?;
            // CLI container and recording paths provide the exact namespace's
            // row order. Replay intentionally retains the recording-time raw
            // IDs because ReadV2 supplies recording-time fdinfo bytes.
            let has_configured_mount_ids = guest.config().mountinfo_mount_ids_captured;
            let virtual_mount_id = if raw_mount_id == 0 {
                // Linux uses zero for anonymous objects such as memfd. Key the
                // equivalence on the observed value, not our descriptor type.
                0
            } else if !has_configured_mount_ids {
                // Public non-container callers have no pre-captured provenance.
                // Mount/unshare/setns are refused once Detcore starts, so a
                // tracer-side snapshot of this task's namespace is immutable.
                let mountinfo_path = format!("/proc/{}/mountinfo", guest.pid().as_raw());
                let mountinfo_contents = std::fs::read(&mountinfo_path).map_err(|error| {
                    Error::Tool(anyhow::anyhow!(
                        "failed to read {mountinfo_path} while validating fdinfo mnt_id: {error}"
                    ))
                })?;
                // Same guest mount model as the mountinfo capture path:
                // ephemeral host seed rows are not guest namespace members
                // and must not shift run-global mount-ID assignment.
                let mountinfo_contents =
                    crate::procfs::exclude_ephemeral_host_seed_mounts(&mountinfo_contents);
                let mountinfo_rows =
                    crate::procfs::parse_mountinfo(&mountinfo_contents).ok_or_else(|| {
                        Error::Tool(anyhow::anyhow!(
                            "kernel returned malformed {mountinfo_path} while validating fdinfo mnt_id"
                        ))
                    })?;
                let snapshot = MountInfoSnapshot::new(
                    mountinfo_rows,
                    &[],
                    false,
                    BTreeMap::new(),
                    BTreeMap::new(),
                )
                .ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "{mountinfo_path} failed strict identity validation for fdinfo"
                    ))
                })?;
                determinize_mount_id(guest, raw_mount_id, Some(snapshot.raw_mount_id_order()))
                    .await
                    .ok_or_else(|| {
                        Error::Tool(anyhow::anyhow!(
                            "mountinfo mount-ID order changed after the identity snapshot while resolving fdinfo mnt_id {raw_mount_id} for {fd_type:?} from {mountinfo_path}"
                        ))
                    })?
            } else {
                determinize_mount_id(guest, raw_mount_id, None)
                    .await
                    .ok_or_else(|| {
                        Error::Tool(anyhow::anyhow!(
                            "recorded mount identity provenance was invalid while resolving fdinfo mnt_id {raw_mount_id} for {fd_type:?}"
                        ))
                    })?
            };
            Some((
                // Determinized immediately above (stdio-special or
                // `determinize_inode`); lowered to an integer only here, at the
                // point it is rendered into guest-visible fdinfo text.
                virtual_inode.as_raw(),
                logical_flags,
                open_file_id.deterministic_socket_cookie(),
                virtual_mount_id,
            ))
        } else {
            None
        };
        let needs_random_uuid = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_random_uuid())?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
        let random_uuid =
            needs_random_uuid.then(|| guest.thread_state_mut().thread_prng().random::<[u8; 16]>());
        // ⚠️ DETERMINIZE HERE, IN THE CALLER, THROUGH THE SAME POOLS `stat` USES.
        //
        // The sanitizers in `crate::procfs` are pure functions of content and
        // hold no guest handle, so they cannot reach `InodePool`/`DevicePool`.
        // Minting an identity down there would make the maps column stable and
        // STILL DISAGREE with `stat` -- deterministic, reproducible and wrong.
        // This mirrors how `fdinfo_identity` is built a few lines above:
        // determinize with `determinize_inode`/`determinize_device`, then hand
        // the finished values down purely to be rendered.
        //
        // BOTH COLUMNS, not just the inode. `determinize_stat` sanitizes
        // `st_dev` as well, so rewriting only the inode would leave the device
        // disagreeing -- the same defect with the reported symptom removed.
        let needs_mapping_identities = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_mapping_identities())?;
        let mapping_identities = if needs_mapping_identities {
            // A mapping of the object behind an inherited stdio descriptor
            // reports the fixed `deterministic_stdio_inode` value that
            // descriptor's fdinfo reports, rather than a pooled one, only for
            // the object those descriptors stand in for: the `fstat(0)`
            // stand-in `setup_stdio` caches for all three, which is stdin's
            // file (`fixed_stdio_identity_stats`). Matching is by the raw
            // device and inode of each descriptor's own identity, so a
            // mapping of stdin's file is not matched as stdout's when stdout
            // is another file. That other file is not in the table at all:
            // its mappings report the pooled inode an alias of stdout above
            // descriptor 2 and a `stat` of its path report, not descriptor
            // 1's fixed inode, which neither reports (round-11 review of
            // https://github.com/rrnewton/hermit/pull/3255).
            //
            // The inode alone is not enough: an unrelated file on another
            // filesystem can share the stdio inode number (a fresh tmpfs
            // numbers its files 2, 3, ...; `/dev/null` is inode 3 on
            // devtmpfs), and it would then be rendered with the stdio inode
            // in the runs where the host numbers happened to coincide
            // (https://github.com/rrnewton/hermit/issues/3307).
            //
            // When several stdio descriptors share one raw identity the LOWEST
            // descriptor's inode wins: `2>&1`, or one terminal on all three,
            // gives several descriptors one file, which a mapping can name
            // with only one inode. For stdin's file the lowest is the one
            // `fstat(0)` reports (1000); letting the last insert win reported
            // stderr's (1002). Without `virtualize_metadata` every
            // descriptor's identity is still the stand-in, so every mapping of
            // the stdin file matches all three. `namespace.rs`'s
            // `deterministic_stdio_inode_for_raw` applies the same precedence,
            // to the same table.
            let stdio_stats = self.fixed_stdio_identity_stats(guest).await?;
            let mut stdio_by_raw_file: BTreeMap<RawInode, DetInode> = BTreeMap::new();
            for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
                let inode = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        deterministic_stdio_inode_for_resource(fd, detfd.resource())
                    })
                    .ok()
                    .flatten();
                if let (Some(det), Some(stat)) = (inode, stdio_stats[fd as usize]) {
                    stdio_by_raw_file.entry(stat.raw_inode()).or_insert(det);
                }
            }
            // The reader's mapping records describe this snapshot only when it
            // shows the reader's own address space, and the executable link
            // `GuestMappingMinter` falls back to is the subject's.
            let reader_mm = guest.thread_state().mm_id;
            let subject = guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.procfs_mapping_subject(reader_mm))?;
            // The minting loop lives in `crate::procfs::mint_mapping_identities`
            // so the unit tests drive the same code: it mints in maps-TEXT
            // order, never in host raw-number order, because a file first
            // seen here gets the next deterministic inode. For each line it
            // keys the INODE on that line's own record, or else on what `stat`
            // reports for the file its header names
            // (`GuestMappingMinter::stat_identity`), and refuses a line without
            // a record that another line of its header contradicts; it keys
            // the DEVICE column on what maps printed: on btrfs and overlayfs
            // the two devices differ on native Linux as well, while the inode
            // numbers agree. See `ProcfsSnapshotContext::mapping_identities`.
            crate::procfs::mint_mapping_identities(
                &contents,
                &stdio_by_raw_file,
                &mut GuestMappingMinter::new(self, guest, subject),
            )
            .await?
        } else {
            crate::procfs::MappingIdentities::new()
        };
        let mountinfo = if guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_needs_mountinfo_identities())?
        {
            let mut rows = crate::procfs::parse_mountinfo(&contents).ok_or_else(|| {
                Error::Tool(anyhow::anyhow!(
                    "kernel returned malformed /proc/*/mountinfo"
                ))
            })?;
            let mut device_rewrites = BTreeMap::new();
            for &(mountinfo_device, metadata_device) in &guest.config().mountinfo_device_rewrites {
                if device_rewrites
                    .insert(mountinfo_device, metadata_device)
                    .is_some()
                {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "duplicate proven mountinfo device rewrite for raw device {mountinfo_device}"
                    )));
                }
            }
            for row in &mut rows {
                if let Some(metadata_device) = device_rewrites.get(&row.raw_device) {
                    row.raw_device = *metadata_device;
                }
            }
            let mut raw_devices = Vec::new();
            let mut seen_devices = BTreeSet::new();
            for row in &rows {
                if seen_devices.insert(row.raw_device) {
                    raw_devices.push(row.raw_device);
                }
            }
            let mut devices = BTreeMap::new();
            let virtualize_metadata = guest.config().virtualize_metadata;
            if virtualize_metadata {
                // Intentionally use snapshot row order to pre-populate the
                // same run-global DevicePool used by stat/statx. This makes
                // every later observation of a device agree within the run.
                // It does not promise that unlike host filesystem layouts
                // expose the same device equivalence classes or order.
                for raw in raw_devices {
                    devices.insert(raw, determinize_device(guest, raw).await);
                }
            }
            let mut root_rewrites = BTreeMap::new();
            for rewrite in &guest.config().mountinfo_root_rewrites {
                if root_rewrites
                    .insert(rewrite.raw_mount_id, rewrite.clone())
                    .is_some()
                {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "duplicate proven mountinfo root rewrite for mount ID {}",
                        rewrite.raw_mount_id
                    )));
                }
            }
            let mut snapshot = MountInfoSnapshot::new(
                rows,
                if guest.config().mountinfo_mount_ids_captured {
                    &guest.config().mountinfo_mount_ids
                } else {
                    &[]
                },
                virtualize_metadata,
                devices,
                root_rewrites,
            )
            .ok_or_else(|| {
                Error::Tool(anyhow::anyhow!(
                    "mountinfo snapshot failed strict identity validation"
                ))
            })?;
            // Number this view's mount IDs through the same run-global pool
            // as fdinfo. IDs the run has already shown keep their numbers;
            // the rest are numbered in the order this view presents them
            // (rows, then parent-only IDs). The canonical order, which may
            // follow the captured namespace, only validates the view.
            let observation_order = snapshot.raw_mount_id_observation_order();
            let Some(virtual_ids) = resolve_mountinfo_identities(
                guest,
                snapshot.raw_mount_id_order(),
                observation_order.clone(),
            )
            .await
            else {
                return Err(Error::Tool(anyhow::anyhow!(
                    "mountinfo mount-ID order changed after the run-global identity snapshot"
                )));
            };
            if !snapshot.use_run_mount_ids(&observation_order, &virtual_ids) {
                return Err(Error::Tool(anyhow::anyhow!(
                    "run-global mount identities do not cover this mountinfo snapshot"
                )));
            }
            Some(snapshot)
        } else {
            None
        };
        guest.thread_state().with_detfd(call.fd(), |detfd| {
            detfd.initialize_procfs(
                contents.clone(),
                ProcfsSnapshotContext {
                    mapping_identities: mapping_identities.clone(),
                    mountinfo: mountinfo.clone(),
                    virtual_uptime_seconds,
                    virtual_boot_time_seconds,
                    virtual_realtime_seconds,
                    virtual_memory_kb,
                    virtual_pid,
                    virtual_ppid,
                    virtual_pty_count,
                    fdinfo_identity,
                    random_uuid,
                },
            );
        })?;
        Ok(())
    }

    /// SYS_read system call (MAYHANG).
    pub async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return self
                .read_timer_slack(guest, call.fd(), call.buf(), call.len())
                .await;
        }

        if call.len() == 0 {
            if let Ok(Some(status_flags)) = guest.thread_state().with_detfd(call.fd(), |detfd| {
                (detfd.ty() == FdType::Rng).then(|| detfd.status_flags())
            }) {
                // Replay reserves random-device fd slots with eventfds. A
                // physical zero-length read of that placeholder returns EINVAL
                // even though the logical random-device read must return zero.
                require_random_device_read_access(status_flags)?;
                let limit =
                    crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit())?;
                // vfs_read still checks access_ok for a zero-length buffer:
                // NULL is valid, but an address beyond TASK_SIZE is EFAULT.
                limit.validate(&[crate::iovecs::ImportedIovec {
                    base: call.buf().map_or(0, |address| address.as_raw()),
                    len: 0,
                }])?;
                return Ok(0);
            }
            // A zero-count read transfers nothing on a file or stream, but on a
            // datagram or SEQPACKET socket it consumes a pending message. Its
            // result goes through record/replay: a replayed descriptor may be
            // a placeholder whose own zero-count read fails differently.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601)
            return self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
        }

        let (needs_procfs_snapshot, serves_procfs_snapshot) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.procfs_needs_snapshot(),
                    detfd.procfs_serves_snapshot(),
                )
            })?;
        if serves_procfs_snapshot {
            validate_procfs_destination(
                || crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit()),
                call.buf(),
                call.len(),
            )?;
        }
        if needs_procfs_snapshot {
            self.initialize_procfs_snapshot(guest, call).await?;
        }

        // Like a seq_file read, advance the shared cursor only by the bytes
        // actually copied: a read that faults before copying anything leaves
        // it where it was.
        let procfs_preview = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.preview_procfs(call.len()))?;
        if let Some((offset, bytes)) = procfs_preview {
            let copied = copy_procfs_output(&mut guest.memory(), call.buf(), &bytes)?;
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.commit_procfs_read(offset, copied);
            })?;
            return Ok(copied as i64);
        }

        let (fd_type, physically_nonblocking, logically_nonblocking, resource, random_device) =
            guest.thread_state_mut().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.resource(),
                    detfd.clone(),
                )
            })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::R);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => {
                trace!("Read call RNG fd {}, simulating...", call.fd());
                let status_flags = random_device.status_flags();
                random_device
                    .with_random_device_stream(|offset| {
                        require_random_device_read_access(status_flags)?;
                        let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                        self.fill_random_device_bytes(guest, remote_buf, call.len(), offset)
                    })
                    .map(|n| n as i64)
            }
            FdType::Regular => {
                if guest.config().deterministic_io {
                    self.deterministic_read(guest, call).await
                } else {
                    Ok(self.record_or_replay(guest, call).await?)
                }
            }
            FdType::Signalfd | FdType::Eventfd | FdType::Timerfd | FdType::Inotify => {
                trace!(
                    "Possibly blocking read call on notification fd {}, type {:?}",
                    call.fd(),
                    fd_type
                );
                self.execute_nonblockable_fd_syscall(guest, call).await
            }
            FdType::Memfd | FdType::Pidfd | FdType::Userfaultfd | FdType::Epoll => {
                trace!("Read call on unusual fd {}, type {:?}", call.fd(), fd_type);
                Ok(self.record_or_replay(guest, call).await?)
            }

            FdType::Socket | FdType::Pipe => {
                trace!(
                    "Possibly blocking read call on {:?} fd {}",
                    fd_type,
                    call.fd()
                );
                self.execute_nonblockable_fd_syscall(guest, call).await
            }
        };
        resource_release_all(guest).await;
        res
    }

    /// SYS_pread64 system call.
    pub async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return self
                .pread_timer_slack(guest, call.fd(), call.buf(), call.len(), call.offset())
                .await;
        }

        if call.len() == 0 {
            // As for read, a zero-count pread's result goes through
            // record/replay.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601)
            return self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
        }

        let offset = usize::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
        let (needs_procfs_snapshot, serves_procfs_snapshot) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.procfs_needs_snapshot(),
                    detfd.procfs_serves_snapshot(),
                )
            })?;
        if serves_procfs_snapshot {
            validate_procfs_destination(
                || crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit()),
                call.buf(),
                call.len(),
            )?;
        }
        if needs_procfs_snapshot {
            let read = syscalls::Read::new()
                .with_fd(call.fd())
                .with_buf(call.buf())
                .with_len(call.len());
            self.initialize_procfs_snapshot(guest, read).await?;
        }

        let procfs_bytes = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.take_procfs_at(offset, call.len()))?;
        if let Some(bytes) = procfs_bytes {
            let copied = copy_procfs_output(&mut guest.memory(), call.buf(), &bytes)?;
            return Ok(copied as i64);
        }

        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = match fd_type {
            FdType::Rng => (|| -> Result<i64, Error> {
                trace!("Pread64 call RNG fd {}, simulating...", call.fd());
                let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                let n =
                    self.fill_random_device_bytes(guest, remote_buf, call.len(), offset as u64)?;
                Ok(n as i64)
            })(),
            FdType::Regular if guest.config().deterministic_io => {
                self.deterministic_pread64(guest, call).await
            }
            _ => match self.record_or_replay(guest, call).await {
                Ok(value) => Ok(value),
                Err(error) => Err(error.into()),
            },
        };

        resource_release_all(guest).await;
        res
    }

    /// SYS_lseek system call.
    pub async fn handle_lseek<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Lseek,
    ) -> Result<i64, Error> {
        let timer_slack_binding = self.timer_slack_binding(guest, call.fd())?;
        // A seek must not move the position under a `getdents` that is
        // reading or serving this open file's directory.
        let directory_lock = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.directory_lock())?;
        let _seeking = directory_lock.lock().await;
        let (fd_type, status_flags, procfs_position, resource, directory_stream, host_order) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.status_flags(),
                    detfd.procfs_position(),
                    detfd.resource(),
                    detfd.has_directory_stream(),
                    detfd.directory_in_host_order(),
                )
            })?;
        if fd_type == FdType::Rng {
            return random_device_lseek_result(status_flags, call.whence()).map_err(Into::into);
        }
        if is_inherited_container_output(resource) {
            return Err(Errno::ESPIPE.into());
        }
        if timer_slack_binding.is_some() && status_flags & libc::O_PATH != 0 {
            return Err(Errno::EBADF.into());
        }
        if directory_stream {
            // Positions in a sorted directory stream are entry indices (see
            // `DirectoryStream`), not host cookies. The kernel position only
            // follows the stream, for descriptors Detcore does not track. Like
            // tmpfs's `dcache_dir_lseek`, only SEEK_SET and SEEK_CUR are
            // accepted; SEEK_END, SEEK_DATA and SEEK_HOLE get `EINVAL`, where
            // ext4 and btrfs accept SEEK_END
            // (https://github.com/rrnewton/hermit/issues/3724).
            let current = guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.with_directory_stream(|stream| stream.position())
            })??;
            let requested = i128::from(call.offset());
            let position = match call.whence() {
                Whence::SEEK_SET => requested,
                Whence::SEEK_CUR => i128::from(current) + requested,
                _ => return Err(Errno::EINVAL.into()),
            };
            let result = i64::try_from(position)
                .ok()
                .filter(|position| *position >= 0)
                .ok_or(Errno::EINVAL)?;
            let target = guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.with_directory_stream(|stream| {
                    stream.seek(result as u64);
                    stream.kernel_target()
                })
            })??;
            self.move_directory_kernel_position(guest, call.fd() as RawFd, target)
                .await;
            return Ok(result);
        }
        let Some((current, snapshot_len)) = procfs_position else {
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#1044): Regular-file lseek must
            // flow through record_or_replay, exactly as handle_read does for
            // FdType::Regular. Injecting the seek live here is correct for run
            // and --verify (the inner NoopTool just re-injects), but under
            // record/replay the replay descriptor is a virtual placeholder
            // whose kernel position never advances (reads are served from the
            // log, not the file). A live SEEK_CUR then returned Ok(0) on replay
            // versus the recorded offset (e.g. glibc's __tzfile_read rewinds
            // /etc/localtime with lseek(fd, -N, SEEK_CUR)), diverging the
            // guest's control flow and desynchronizing the event stream. Routing
            // through record_or_replay records the offset once and substitutes
            // the recorded value on replay, keeping the two runs identical.
            let position = self.record_or_replay(guest, call).await?;
            if host_order && position == 0 {
                // Back at the start, the next `getdents` can read the whole
                // directory again and serve it as a sorted stream.
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.use_directory_stream())?;
            }
            return Ok(position);
        };

        if let Some(binding) = timer_slack_binding {
            // Linux exposes this file through seq_lseek, which accepts only
            // SEEK_SET and SEEK_CUR. Keep that position entirely in the
            // virtual open-file description even before the first read.
            let requested = i128::from(call.offset());
            let new_offset = match call.whence() {
                Whence::SEEK_SET => requested,
                Whence::SEEK_CUR => current as i128 + requested,
                _ => return Err(Errno::EINVAL.into()),
            };
            let new_offset = usize::try_from(new_offset).map_err(|_| Errno::EINVAL)?;
            let result = i64::try_from(new_offset).map_err(|_| Errno::EOVERFLOW)?;
            // seq_lseek does not call the show callback for a no-op or a reset
            // to zero. Only a traversal to another positive position observes
            // the target task and therefore performs lifetime/access checks.
            if new_offset != 0 && new_offset != current {
                self.require_current_timer_slack_target(guest, binding)
                    .await?;
            }
            guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.set_procfs_offset(new_offset))?;
            return Ok(result);
        }

        let Some(snapshot_len) = snapshot_len else {
            let offset = guest.inject(Syscall::from(call)).await?;
            let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            guest
                .thread_state()
                .with_detfd(call.fd(), |detfd| detfd.set_procfs_offset(offset))?;
            return Ok(offset as i64);
        };

        let requested = i128::from(call.offset());
        let new_offset = match call.whence() {
            Whence::SEEK_SET => requested,
            Whence::SEEK_CUR => current as i128 + requested,
            Whence::SEEK_END => snapshot_len as i128 + requested,
            Whence::SEEK_DATA => {
                if requested < 0 || requested >= snapshot_len as i128 {
                    return Err(Errno::ENXIO.into());
                }
                requested
            }
            Whence::SEEK_HOLE => {
                if requested < 0 || requested >= snapshot_len as i128 {
                    return Err(Errno::ENXIO.into());
                }
                snapshot_len as i128
            }
            _ => return Err(Errno::EINVAL.into()),
        };
        let new_offset = usize::try_from(new_offset).map_err(|_| Errno::EINVAL)?;
        let result = i64::try_from(new_offset).map_err(|_| Errno::EOVERFLOW)?;
        guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.set_procfs_offset(new_offset))?;
        Ok(result)
    }

    /// Helper for performing a deterministic read that retries until it gets all its
    /// bytes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#689): Confirm partial reads take precedence over later errors.
    async fn deterministic_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Read,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested read buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining read buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    // Buf is guaranteed to exist as we already issued a syscall.
                    let old_ptr = call.buf().unwrap().as_raw();
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize));
                }
                Err(error) if total_read_bytes > 0 => {
                    trace!("[detcore/det_io]: returning {total_read_bytes} bytes before {error}");
                    break Ok(total_read_bytes);
                }
                Err(error) => break Err(error.into()),
            }
        }
    }

    /// Perform a positional read until the requested buffer is full or EOF is reached.
    async fn deterministic_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested pread64 buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining pread64 buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    let old_ptr = call
                        .buf()
                        .expect("successful pread64 requires a valid guest buffer")
                        .as_raw();
                    let offset = call.offset().checked_add(res).ok_or(Errno::EOVERFLOW)?;
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize))
                        .with_offset(offset);
                }
                Err(error) => break Err(error.into()),
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-838): Review regular-file sendfile mediation.
    /// Copy data between tracked regular files or memfds.
    ///
    /// The kernel advances the input offset (or the explicit offset pointer) and
    /// destination offset atomically with the copy. Detcore serializes destination
    /// writes while the strict scheduler orders the stable input read, and routes
    /// the syscall through record/replay so that the result and offset update stay
    /// ordered with other file operations. Socket and pipe destinations can block
    /// and need the nonblocking scheduler path; return ENOSYS for those endpoint
    /// types so libc/application fallbacks use Detcore's existing read/write
    /// handlers instead.
    pub async fn handle_sendfile<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendfile,
    ) -> Result<i64, Error> {
        let in_type = guest
            .thread_state()
            .with_detfd(call.in_fd(), |detfd| detfd.ty())?;
        let (out_type, out_resource) = guest
            .thread_state()
            .with_detfd(call.out_fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-973): Refuse sendfile from a procfs input so it
        // cannot bypass the deterministic ProcfsFile snapshot. A procfs fd is
        // classified `FdType::Regular`, so it would otherwise pass the type
        // guard below and copy live kernel bytes straight to the output fd,
        // reintroducing the nondeterminism the mediated read()/write() path
        // sanitizes. Failing closed with ENOSYS makes callers fall back to
        // that mediated path (glibc's sendfile does exactly this).
        let in_is_procfs = guest
            .thread_state()
            .with_detfd(call.in_fd(), |detfd| detfd.procfs_position().is_some())?;
        if in_is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        if !matches!(in_type, FdType::Regular | FdType::Memfd)
            || !matches!(out_type, FdType::Regular | FdType::Memfd)
        {
            return Err(Errno::ENOSYS.into());
        }

        let out_inode = self.written_identity(guest, call.out_fd()).await?;
        let dettid = guest.thread_state().dettid;
        let mut resources = Resources::new(dettid);
        // `out_inode` is the fd's HOST identity, so it must be determinized
        // before naming a resource. It is deliberately left raw for the
        // `touch_file` call below, which takes a `RawInode`.
        let out_resource = match out_resource {
            Some(resource) => Some(resource),
            None => match out_inode {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };
        if let Some(resource) = out_resource {
            resources.insert(resource, Permission::W);
        }
        resources.fyi("sendfile");
        resource_request(guest, resources).await;

        let result = self
            .record_or_replay(guest, call)
            .await
            .map_err(Error::from);
        if guest.config().virtualize_metadata && matches!(&result, Ok(copied) if *copied > 0) {
            let inode = out_inode.expect("virtualized metadata requires stat data for sendfile");
            touch_file(guest, inode).await;
        }
        resource_release_all(guest).await;
        result
    }

    /// SYS_write system call.
    pub async fn handle_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Write,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return self
                .write_timer_slack(guest, call.fd(), call.buf(), call.len())
                .await;
        }

        let (fd_type, physically_nonblocking, logically_nonblocking, open_file_id, resource) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.open_file_id(),
                    detfd.resource(),
                )
            })?;
        let written = self.written_identity(guest, call.fd()).await?;
        // It doesn't matter much where the linearization point for this mtime bump falls:
        if guest.config().virtualize_metadata {
            let r =
                written.expect("Expect that when virtualize_metadata, DetFd's stat is populated!");
            touch_file(guest, r).await;
        }

        if let Some(resource) = resource {
            let container_output = crate::sigalrm_phase1::is_container_output(&resource);
            let mut request = guest.thread_state().mk_request(resource, Permission::W);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
            // Signal phase 1, design closure 1: a write to inherited stdout or
            // stderr may sleep with no scheduler admission. Once the grant is
            // taken no SIGALRM commits until this call ends, so one already due
            // is the only one it can strand: recorded as a determinism loss.
            if guest.thread_state().sigalrm_handled && container_output {
                sigalrm_refuses(guest, SigalrmControl::UnadmittedStdioIo).await;
            }
        }

        // Only route writes through the nonblockable-fd path when the fd is actually
        // physically nonblocking. Detcore-created pipes are physically nonblocking in every
        // sequential mode, including record/replay, so their logically blocking writes use
        // the completion helper below. A physically blocking fd instead uses the original
        // synchronous path: treating an internal pipe as BlockingExternalIO would assume the
        // writer and reader were independent and could deadlock the scheduler.
        let res = if physically_nonblocking && fd_type == FdType::Pipe && !logically_nonblocking {
            self.execute_blocking_pipe_write(guest, call, open_file_id)
                .await
        } else if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else if guest.config().deterministic_io {
            let mut total_written_bytes = 0;
            let mut remaining_buf = call.len();

            trace!(
                "[detcore/det_io]: Requested write buffer size: {:?}",
                remaining_buf
            );

            loop {
                match self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await
                {
                    Ok(res) => {
                        remaining_buf -= res as usize;
                        total_written_bytes += res;

                        trace!(
                            "[detcore/det_io]: Remaining write buffer size: {:?}",
                            remaining_buf
                        );

                        if res == 0 || remaining_buf == 0 {
                            break Ok(total_written_bytes);
                        }

                        // Buf is guaranteed to exist as we already issued a syscall.
                        let old_ptr = call.buf().unwrap().as_raw();
                        call = call
                            .with_len(remaining_buf)
                            .with_buf(Addr::<u8>::from_raw(old_ptr + res as usize));
                    }
                    Err(error) => {
                        break finish_partial_record_or_replay_write(total_written_bytes, error);
                    }
                }
            }
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#683): Confirm positional-write ordering and replay semantics.
    /// SYS_pwrite64 system call.
    pub async fn handle_pwrite64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Pwrite64,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            return Err(if call.offset() < 0 {
                Errno::EINVAL.into()
            } else {
                Errno::ESPIPE.into()
            });
        }

        let resource = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.resource())?;
        let raw_ino = self.written_identity(guest, call.fd()).await?;
        // The fd's cached `DetStat` carries the HOST inode (`DetStat` is built
        // straight from `fstat`/`statx`), so it must be determinized before it
        // can name a guest-visible resource. Passing it through directly used
        // to type-check only because `DetInode` was an alias for `RawInode`.
        let resource = match resource {
            Some(resource) => Some(resource),
            None => match raw_ino {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        let result = if guest.config().deterministic_io {
            let mut total_written = 0_i64;
            let mut remaining = call.len();

            loop {
                match self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await
                {
                    Ok(written) => {
                        let Ok(written) = usize::try_from(written) else {
                            break Err(Errno::EIO.into());
                        };
                        let Ok(written_i64) = i64::try_from(written) else {
                            break Err(Errno::EIO.into());
                        };
                        if written > remaining {
                            break Err(Errno::EIO.into());
                        }
                        remaining -= written;
                        let Some(next_total) = total_written.checked_add(written_i64) else {
                            break Err(Errno::EIO.into());
                        };
                        total_written = next_total;

                        if written == 0 || remaining == 0 {
                            break Ok(total_written);
                        }

                        let Some(old_buf) = call.buf() else {
                            break Err(Errno::EFAULT.into());
                        };
                        let Some(next_buf) = old_buf.as_raw().checked_add(written) else {
                            break Err(Errno::EFAULT.into());
                        };
                        let Some(next_offset) = call.offset().checked_add(written_i64) else {
                            break Err(Errno::EFBIG.into());
                        };
                        let Some(next_buf) = Addr::<u8>::from_raw(next_buf) else {
                            break Err(Errno::EFAULT.into());
                        };
                        call = call
                            .with_buf(Some(next_buf))
                            .with_len(remaining)
                            .with_offset(next_offset);
                    }
                    Err(error) => {
                        break finish_partial_record_or_replay_write(total_written, error);
                    }
                }
            }
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode = raw_ino.expect("virtualized metadata requires stat data for tracked fds");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#547)
    /// SYS_writev system call.
    ///
    /// Preserve the initial writev as one kernel operation so its iovec order remains intact.
    /// Detcore adds open-file resource ordering and nonblocking scheduler integration; a
    /// blocking pipe short write is completed by the helper because Hermit injected O_NONBLOCK.
    pub async fn handle_writev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Writev,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), true)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.len())?;
            return self.writev_timer_slack(guest, call.fd(), iovecs, 0).await;
        }

        let (fd_type, physically_nonblocking, logically_nonblocking, open_file_id, resource) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.open_file_id(),
                    detfd.resource(),
                )
            })?;
        let written = self.written_identity(guest, call.fd()).await?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::W);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let result = if physically_nonblocking && fd_type == FdType::Pipe && !logically_nonblocking
        {
            self.execute_blocking_pipe_writev(guest, call, open_file_id)
                .await
        } else if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        if guest.config().virtualize_metadata && matches!(&result, Ok(bytes) if *bytes > 0) {
            let inode =
                written.expect("virtualized metadata requires stat data for every tracked fd");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_readv system call: the vectored form of `read`.
    ///
    /// Mirrors [`Self::handle_writev`] for the read direction. Detcore adds
    /// open-file resource ordering and, for physically nonblocking pipe/socket
    /// fds, the nonblocking scheduler integration. Random devices use the shared
    /// canonical cursor; other descriptors retain their recorded kernel operation.
    pub async fn handle_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
    ) -> Result<i64, Error> {
        self.handle_readv_with_output(guest, call, &mut None).await
    }

    /// Import after the resource wait and retain that geometry through copyout.
    /// The shared-cursor closure is synchronous and takes no metadata locks.
    fn read_random_vectors<G: Guest<Self>>(
        &self,
        guest: &mut G,
        detfd: &DetFd,
        request: RandomVectoredRead,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        require_random_device_read_access(detfd.status_flags())?;
        let iovecs = crate::iovecs::import_read_iovecs(
            &guest.memory(),
            request.address,
            request.count,
            || crate::iovecs::UserAddressLimit::from_query(guest.user_address_limit()),
        )?;
        let total = iovecs.iter().map(|iov| iov.len).sum();
        validate_random_vector_read(request.offset, total, request.flags)?;
        let written = if let Some(offset) = request.offset {
            self.fill_random_device_iovecs(guest, &iovecs, offset)?
        } else {
            detfd.with_random_device_stream(|offset| {
                self.fill_random_device_iovecs(guest, &iovecs, offset)
            })?
        };
        if written > 0 && self.cfg.detlog_io_buffers && crate::detlog_observed!() {
            *rng_output = Some(crate::io_buffers::rng_readv_extents(&iovecs, written)?);
        }
        Ok(written as i64)
    }

    pub(crate) async fn handle_readv_with_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.len())?;
            return self
                .readv_timer_slack(guest, call.fd(), iovecs, None, 0)
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let (fd_type, physically_nonblocking, logically_nonblocking, resource, detfd) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.is_nonblocking(),
                    detfd.resource(),
                    detfd.clone(),
                )
            })?;

        if let Some(resource) = resource {
            let mut request = guest.thread_state().mk_request(resource, Permission::R);
            if should_tag_host_timed_internal_pipe_io(
                guest.config().backend.internal_pipe_turns_are_host_timed,
                fd_type,
                physically_nonblocking,
                logically_nonblocking,
            ) {
                request.fyi(HOST_TIMED_INTERNAL_PIPE_IO_FYI);
            }
            resource_request(guest, request).await;
        }

        let res = if fd_type == FdType::Rng {
            self.read_random_vectors(
                guest,
                &detfd,
                RandomVectoredRead {
                    address: call.iov().map_or(0, |addr| addr.as_raw()),
                    count: call.len(),
                    offset: None,
                    flags: 0,
                },
                rng_output,
            )
        } else if physically_nonblocking
            && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
        {
            self.execute_nonblockable_fd_syscall(guest, call).await
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };

        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_preadv system call: the vectored form of `pread64`.
    ///
    /// RNG reads use the canonical stream at the explicit offset without
    /// advancing its shared cursor. Other files retain their kernel operation.
    pub async fn handle_preadv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv,
    ) -> Result<i64, Error> {
        self.handle_preadv_with_output(guest, call, &mut None).await
    }

    pub(crate) async fn handle_preadv_with_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        // Linux rejects negative offsets before descriptor lookup or import.
        let offset = vectored_offset(call.pos_l(), call.pos_h());
        if offset < 0 {
            return Err(Errno::EINVAL.into());
        }
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), call.iov_len())?;
            return self
                .readv_timer_slack(guest, call.fd(), iovecs, Some(offset), 0)
                .await;
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let detfd = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.clone())?;

        if let Some(resource) = detfd.resource() {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = if detfd.ty() == FdType::Rng {
            self.read_random_vectors(
                guest,
                &detfd,
                RandomVectoredRead {
                    address: call.iov().map_or(0, |addr| addr.as_raw()),
                    count: call.iov_len(),
                    offset: Some(offset as u64),
                    flags: 0,
                },
                rng_output,
            )
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };
        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_preadv2 system call: positioned vectors, or the shared stream when
    /// offset is -1. RNG flag validation precedes output copying.
    pub async fn handle_preadv2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv2,
    ) -> Result<i64, Error> {
        self.handle_preadv2_with_output(guest, call, &mut None)
            .await
    }

    pub(crate) async fn handle_preadv2_with_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Preadv2,
        rng_output: &mut Option<Vec<crate::io_buffers::BufferExtent>>,
    ) -> Result<i64, Error> {
        let offset = vectored_offset(call.pos_l(), call.pos_h());
        if offset < -1 {
            return Err(Errno::EINVAL.into());
        }
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            self.require_timer_slack_access(guest, call.fd(), false)?;
            let count = usize::try_from(call.iov_len()).map_err(|_| Errno::EINVAL)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), count)?;
            return if offset == -1 {
                self.readv_timer_slack(guest, call.fd(), iovecs, None, call.flags())
                    .await
            } else {
                self.readv_timer_slack(guest, call.fd(), iovecs, Some(offset), call.flags())
                    .await
            };
        }

        let is_procfs = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.procfs_position().is_some())?;
        if is_procfs {
            return Err(Errno::ENOSYS.into());
        }

        let detfd = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.clone())?;

        if let Some(resource) = detfd.resource() {
            let request = guest.thread_state().mk_request(resource, Permission::R);
            resource_request(guest, request).await;
        }

        let res = if detfd.ty() == FdType::Rng {
            self.read_random_vectors(
                guest,
                &detfd,
                RandomVectoredRead {
                    address: call.iov().map_or(0, |addr| addr.as_raw()),
                    count: call.iov_len() as usize,
                    offset: (offset != -1).then_some(offset as u64),
                    flags: call.flags(),
                },
                rng_output,
            )
        } else {
            self.record_or_replay_preserving_tool_errors(guest, call)
                .await
        };
        resource_release_all(guest).await;
        res
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_pwritev system call: the vectored form of `pwrite64`.
    ///
    /// Positioned writes target seekable files and do not block, so this mirrors
    /// [`Self::handle_pwrite64`]'s ordering, records/replays the single kernel
    /// operation, and bumps the virtual mtime on a successful write.
    pub async fn handle_pwritev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pwritev,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            return if offset < 0 {
                Err(Errno::EINVAL.into())
            } else {
                Err(Errno::ESPIPE.into())
            };
        }

        let resource = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.resource())?;
        let raw_ino = self.written_identity(guest, call.fd()).await?;
        // The fd's cached `DetStat` carries the HOST inode (`DetStat` is built
        // straight from `fstat`/`statx`), so it must be determinized before it
        // can name a guest-visible resource. Passing it through directly used
        // to type-check only because `DetInode` was an alias for `RawInode`.
        let resource = match resource {
            Some(resource) => Some(resource),
            None => match raw_ino {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode = raw_ino.expect("virtualized metadata requires stat data for tracked fds");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#794)
    /// SYS_pwritev2 system call: `pwritev` with a trailing per-call flags
    /// argument, which record/replay forwards unchanged.
    pub async fn handle_pwritev2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pwritev2,
    ) -> Result<i64, Error> {
        if self.timer_slack_binding(guest, call.fd())?.is_some() {
            let offset = vectored_offset(call.pos_l(), call.pos_h());
            if offset < -1 {
                return Err(Errno::EINVAL.into());
            }
            if offset >= 0 {
                return Err(Errno::ESPIPE.into());
            }
            self.require_timer_slack_access(guest, call.fd(), true)?;
            let count = usize::try_from(call.iov_len()).map_err(|_| Errno::EINVAL)?;
            let iovecs = read_iovecs(&guest.memory(), call.iov(), count)?;
            return self
                .writev_timer_slack(guest, call.fd(), iovecs, call.flags())
                .await;
        }

        let resource = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.resource())?;
        let raw_ino = self.written_identity(guest, call.fd()).await?;
        // The fd's cached `DetStat` carries the HOST inode (`DetStat` is built
        // straight from `fstat`/`statx`), so it must be determinized before it
        // can name a guest-visible resource. Passing it through directly used
        // to type-check only because `DetInode` was an alias for `RawInode`.
        let resource = match resource {
            Some(resource) => Some(resource),
            None => match raw_ino {
                Some(raw_ino) => Some(ResourceID::FileContents(
                    determinize_inode(guest, raw_ino).await.0,
                )),
                None => None,
            },
        };

        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, request).await;
        }

        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;

        if guest.config().virtualize_metadata && matches!(&result, Ok(written) if *written > 0) {
            let inode = raw_ino.expect("virtualized metadata requires stat data for tracked fds");
            touch_file(guest, inode).await;
        }

        resource_release_all(guest).await;
        result
    }

    /// SYS_mmap system call.
    pub async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mmap,
    ) -> Result<i64, Error> {
        enum SharedBacking {
            Anonymous,
            File {
                object: SharedMemoryObjectId,
                offset: u64,
            },
        }

        let backing = if call.flags().contains(MapFlags::MAP_SHARED) {
            if call.fd() == -1 {
                Some(SharedBacking::Anonymous)
            } else {
                let offset = u64::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |fd| {
                        let object = fd.stat().map_or_else(
                            || SharedMemoryObjectId::OpenFile {
                                id: fd.open_file_id(),
                            },
                            |stat| SharedMemoryObjectId::File {
                                device: stat.dev,
                                inode: stat.inode,
                            },
                        );
                        SharedBacking::File { object, offset }
                    })
                    .ok()
            }
        } else {
            None
        };
        // The raw identity `fstat` reports for a mapped file. A maps line for
        // this range keys its inode on it; see `mapping_stat_identity`.
        //
        // For an ordinary descriptor it is the descriptor's cached stat
        // (present only under `virtualize_metadata`), so it costs no syscall.
        // An inherited stdio descriptor, or a dup of one, is the exception:
        // its cached stat is a stand-in -- the tracer's `fstat(0)`, given to
        // all three (see `setup_stdio`) whether or not metadata is
        // virtualized -- so its identity comes from a real `fstat` of the
        // descriptor once the mapping has succeeded, and only under
        // `virtualize_metadata`. That `fstat` cannot be the descriptor's own
        // answer -- the mapping just succeeded -- so its failure means Detcore
        // could not ask: the mapping is undone and the call is refused
        // (`identity_lookup_refused`) rather than recorded without an identity.
        let mapped_file = if call.flags().contains(MapFlags::MAP_ANONYMOUS) || call.fd() < 0 {
            MappedFileIdentity::None
        } else {
            guest
                .thread_state()
                .with_detfd(call.fd(), |fd| {
                    if is_container_stdio(fd.resource()) {
                        MappedFileIdentity::Stdio
                    } else {
                        fd.stat().map_or(MappedFileIdentity::None, |stat| {
                            MappedFileIdentity::Cached(stat.raw_inode())
                        })
                    }
                })
                .unwrap_or(MappedFileIdentity::None)
        };
        let fd = call.fd();
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        let start = usize::try_from(result).expect("a successful mmap must return an address");
        let mapped_file = match mapped_file {
            MappedFileIdentity::Cached(file) => Some(file),
            MappedFileIdentity::Stdio if guest.config().virtualize_metadata => {
                match self.inject_fstat(guest, fd).await {
                    Ok(stat) => Some(RawInode::new(stat.st_dev, stat.st_ino)),
                    Err(errno) => {
                        // The guest will not learn this mapping's address:
                        // the refusal stops this process on every backend
                        // before the guest runs again. Undo it anyway, so
                        // the address space does not keep a mapping nobody
                        // recorded while the process is being stopped.
                        guest.thread_state().unmap_memory(start, len);
                        if let Err(unmap_errno) = guest
                            .inject_with_retry(Syscall::Munmap(
                                syscalls::Munmap::new()
                                    .with_addr(Addr::from_raw(start))
                                    .with_len(len),
                            ))
                            .await
                        {
                            warn!(
                                "[detcore] could not undo the mapping of stdio descriptor {fd} \
                                 at {start:#x}: {unmap_errno}"
                            );
                        }
                        return Err(identity_lookup_refused(
                            "fstat the stdio descriptor it mapped",
                            format!("fd {fd}").as_bytes(),
                            errno,
                        ));
                    }
                }
            }
            MappedFileIdentity::Stdio | MappedFileIdentity::None => None,
        };

        guest.thread_state().unmap_memory(start, len);
        if let Some(file) = mapped_file {
            guest.thread_state().map_file(start, len, file);
        }
        match backing {
            Some(SharedBacking::Anonymous) => {
                guest.thread_state().map_shared_anonymous(start, len);
            }
            Some(SharedBacking::File { object, offset }) => {
                guest
                    .thread_state()
                    .map_shared_object(start, len, object, offset);
            }
            None => {}
        }
        Ok(result)
    }

    /// SYS_munmap system call.
    pub async fn handle_munmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Munmap,
    ) -> Result<i64, Error> {
        let start = call.addr().map(Addr::as_raw).unwrap_or(0);
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        guest.thread_state().unmap_memory(start, len);
        Ok(result)
    }

    /// SYS_mremap system call.
    pub async fn handle_mremap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mremap,
    ) -> Result<i64, Error> {
        let old_start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
        let old_len = call.old_len();
        let new_len = call.new_len();
        let result = self.record_or_replay(guest, call).await?;
        let new_start =
            usize::try_from(result).expect("a successful mremap must return an address");
        guest
            .thread_state()
            .remap_memory(old_start, old_len, new_start, new_len);
        Ok(result)
    }

    // Determinize stat by doing:
    //   - using virtual inode instead of real inodes. The virtual inodes
    //     increase monolitically and won't be re-used (like ext4)
    //   - use logical modtime which could be used by program like GNU make
    //     to determine file changes. A file first seen with a canonical host
    //     mtime (`CANONICAL_FILE_MTIME_SECONDS`, e.g. the Nix store's 1) keeps
    //     it; any other first-seen file reports the epoch.
    //   - atime, ctime and btime always report the epoch: the kernel sets ctime
    //     and btime itself, so even for a Nix store file they are real
    //     timestamps, and Hermit keeps no per-file atime.
    async fn determinize_stat<G, S>(
        &self,
        guest: &mut G,
        stat: S,
        inode_override: Option<DetInode>,
    ) -> Result<DetStat, Error>
    where
        G: Guest<Self>,
        S: Into<DetStat>,
    {
        let cfg = guest.config().clone();

        let mut stat: DetStat = stat.into();
        let (d_ino, global_mtime) = match inode_override {
            // The container's stdio streams have fixed inodes and always
            // report the epoch: whatever backs them on the host (a pipe, a
            // terminal, a redirected file) is not part of the guest's view.
            Some(inode) => {
                let nanos = cfg
                    .epoch
                    .timestamp_nanos_opt()
                    .expect("epoch cannot be represented in nanoseconds")
                    as u64;
                (inode, LogicalTime::from_nanos(nanos))
            }
            None => {
                // statx fills stx_mtime only when it reports STATX_MTIME.
                let observed = if stat.mask.contains(StatxMask::STATX_MTIME) {
                    ObservedMtime::from_host_mtime(stat.mtime.tv_sec, stat.mtime.tv_nsec)
                } else {
                    ObservedMtime::Unobserved
                };
                // Key on the raw device as well as the raw inode, read before
                // `stat.dev` is overwritten below: inode numbers repeat across
                // filesystems (https://github.com/rrnewton/hermit/issues/3307).
                //
                // A file with a link cannot be one a retired mapping describes:
                // Linux refuses to link an unlinked file again (only an
                // `O_TMPFILE` file, which is never retired, may get a name).
                // So a nonzero link count is a name sighting however the file
                // was reached, by path, descriptor or working directory, and a
                // zero count (`fstat` after `unlink`, `/proc/self/fd/N`) is not.
                let sighting = if stat.mask.contains(StatxMask::STATX_NLINK) && stat.nlink > 0 {
                    InodeSighting::Name
                } else {
                    InodeSighting::Descriptor
                };
                determinize_inode_observing_mtime(guest, stat.raw_inode(), observed, sighting).await
            }
        };
        stat.inode = d_ino.as_raw(); // Reveal only the deterministic inode.

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping.
        // The raw st_dev leaks the kernel's host-wide anonymous block-device
        // number for procfs/sysfs/tmpfs mounts, which drifts between runs (and
        // between the two runs of `--verify`). Reveal only a deterministic
        // device id.
        stat.dev = determinize_device(guest, stat.dev).await;

        let epoch_tp = Timespec {
            tv_sec: cfg.epoch.timestamp(),
            tv_nsec: cfg.epoch.timestamp_subsec_nanos() as i64,
        };

        let mtime: Timespec = global_mtime.into();
        stat.atime = epoch_tp;
        stat.ctime = epoch_tp;
        stat.btime = epoch_tp;

        stat.mtime = mtime;

        Ok(stat)
    }

    /// Handles all stat syscalls.
    pub async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: StatFamily,
    ) -> Result<i64, Error> {
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without
            // kernel because there're many corner cases. i.e.: even access
            // filepath from tracer may cause tracer to hang under certain fuse
            // filesystem (squashfs_ll).
            guest.inject(Syscall::from(call)).await?;
            let statptr = call.stat().ok_or(Errno::EFAULT)?;
            let described_fd = match call {
                StatFamily::Fstat(call) => Some(call.fd()),
                StatFamily::Fstatat(call) => {
                    empty_path_fd(&guest.memory(), call.dirfd(), call.path(), call.flags())
                }
                #[cfg(not(target_arch = "aarch64"))]
                StatFamily::Stat(_) | StatFamily::Lstat(_) => None,
            };
            let inode_override = described_fd.and_then(|fd| stdio_inode_override(guest, fd));
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat, inode_override).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// statx system call
    pub async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statx,
    ) -> Result<i64, Error> {
        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without kernel
            // because there're many corner cases. i.e.: even access filepath from tracer
            // may cause tracer to hang under certain fuse filesystem (squashfs_ll).
            guest.inject(call).await?;
            let statptr = call.statx().ok_or(Errno::EFAULT)?;
            let inode_override =
                empty_path_fd(&guest.memory(), call.dirfd(), call.path(), call.flags())
                    .and_then(|fd| stdio_inode_override(guest, fd));
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat, inode_override).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// Whether a guest's F_SETFL or FIONBIO on this descriptor changes only its
    /// logical (guest-visible) O_NONBLOCK, leaving it physically nonblocking for
    /// the scheduler's nonblockize-and-retry.
    ///
    /// Outside record/replay that holds for every scheduler-managed type. In
    /// record/replay it holds only for a container-internal channel that is
    /// already physically nonblocking: a pipe, which is how `handle_pipe2`
    /// leaves every guest-created pipe in both phases, or a socketpair endpoint,
    /// which is how `handle_socketpair` leaves both ends. Forwarding the guest's
    /// clear would make the recording's next read on that channel a blocking
    /// read on an fd that `syscall_targets_internal_fd` calls internal, which
    /// deadlocks the sequentialized scheduler while the writer waits for its
    /// turn. Both phases take the same branch, because `fd_type`,
    /// `socketpair_endpoint` and `physically_nonblocking` are Detcore
    /// bookkeeping that record and replay evolve identically: an F_SETFL is
    /// still recorded and replayed, with the kernel given `flags | O_NONBLOCK`
    /// in record and the recorded result returned in replay; a FIONBIO returns
    /// before `record_or_replay`, so neither phase records or applies it. Other
    /// sockets and eventfds stay external in record/replay. A pipe whose
    /// physical O_NONBLOCK came from the guest rather than from Detcore is also
    /// kept nonblocking; in record/replay the only such pipes are `pipe2` pipes,
    /// which Detcore has already made nonblocking.
    fn keeps_physically_nonblocking(
        &self,
        fd_type: FdType,
        socketpair_endpoint: bool,
        physically_nonblocking: bool,
    ) -> bool {
        self.cfg.use_nonblocking_sockets()
            && match fd_type {
                FdType::Pipe => !self.cfg.recordreplay_modes || physically_nonblocking,
                FdType::Socket if socketpair_endpoint => {
                    !self.cfg.recordreplay_modes || physically_nonblocking
                }
                FdType::Socket | FdType::Eventfd => !self.cfg.recordreplay_modes,
                _ => false,
            }
    }

    /// fcntl system call
    pub async fn handle_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        // The owner of an asynchronous-I/O descriptor may be any process or
        // process group, and Linux signals it when data arrives, at a moment
        // set by host timing. Record the signals in this turn, before the call
        // reaches the kernel, for every process of the container, and never
        // forget them (`Scheduler::record_host_timed_signals`).
        let signals = fcntl_host_timed_signals(call.cmd());
        if signals & kernel_sigset_bit(libc::SIGALRM) != 0 {
            refuse_sigalrm(guest, SigalrmControl::ArmProducer).await?;
        }
        // A lease makes another process's open of the file wait for the
        // lease break, interruptibly, with no scheduler admission.
        if matches!(call.cmd(), F_SETLEASE(lease) if lease != libc::F_UNLCK) {
            refuse_sigalrm(guest, SigalrmControl::ArmProducer).await?;
        }
        if signals != 0
            && guest
                .config()
                .backend_supports_blocked_wait_signal_interruption
        {
            record_host_timed_signals(guest, HostTimedSignalScope::Container, signals).await;
        }
        let o_cloexec = match call.cmd() {
            F_DUPFD_CLOEXEC(_) => OFlag::O_CLOEXEC,
            _ => OFlag::empty(),
        };
        match call.cmd() {
            F_GETFL => {
                let physical_flags = self.record_or_replay(guest, call).await?;
                let logical_nonblocking = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.is_nonblocking())?;
                let nonblocking = i64::from(OFlag::O_NONBLOCK.bits());
                if logical_nonblocking {
                    Ok(physical_flags | nonblocking)
                } else {
                    Ok(physical_flags & !nonblocking)
                }
            }
            F_SETFL(flags) => {
                let (fd_type, socketpair_endpoint, physically_nonblocking) =
                    guest.thread_state().with_detfd(fd, |detfd| {
                        (
                            detfd.ty(),
                            detfd.is_socketpair_endpoint(),
                            detfd.physically_nonblocking(),
                        )
                    })?;
                let force_nonblocking = self.keeps_physically_nonblocking(
                    fd_type,
                    socketpair_endpoint,
                    physically_nonblocking,
                );
                let physical_flags = if force_nonblocking {
                    flags | OFlag::O_NONBLOCK.bits()
                } else {
                    flags
                };
                let result = self
                    .record_or_replay(guest, call.with_cmd(F_SETFL(physical_flags)))
                    .await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    // Record the guest's *logical* status flags (derives logical
                    // nonblocking); when we forced O_NONBLOCK physically without the
                    // guest asking, mark the description physically nonblocking too.
                    detfd.set_status_flags(flags);
                    if force_nonblocking {
                        detfd.set_physically_nonblocking();
                    }
                })?;
                Ok(result)
            }
            F_DUPFD(_) | F_DUPFD_CLOEXEC(_) => {
                let newfd = self.record_or_replay(guest, call).await? as RawFd;
                let replaced = guest.thread_state_mut().dup_fd(fd, newfd, o_cloexec)?;
                if let Some(open_file_id) = replaced {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                Ok(newfd as i64)
            }
            F_SETFD(flags) => {
                let result = self.record_or_replay(guest, call).await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_cloexec(flags & libc::FD_CLOEXEC != 0);
                })?;
                Ok(result)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-2568): Review refusing guest pipe growth.
            // Raising a pipe above the pinned capacity is the ONE syscall that
            // defeats the pin, and whether it succeeds is decided by the host's
            // `/proc/sys/fs/pipe-max-size`. On a default host (1 MiB ceiling)
            // the guest gets a 1048576-byte pipe; on a hardened host (64 KiB)
            // the identical guest gets EPERM and keeps 8192. Same binary, same
            // `--strict`, guest-visible return value and pipe capacity decided
            // by a host sysctl -- a determinism leak by this project's own
            // definition, and it survived because the capacity pin was applied
            // at creation and never defended afterwards.
            //
            // Refuse deterministically instead of asking Linux. EPERM is the
            // errno Linux itself returns when that ceiling binds, so the guest
            // sees a shape it must already handle rather than a novel one, and
            // it is the answer the hardened host would have given.
            //
            // SHRINKING IS DELIBERATELY LEFT ALONE. It is always permitted for
            // an unprivileged process, it is process-local with no host-derived
            // input, and `tests/c/pipe_capacity.c` locks
            // it as a guest-visible contract: that fixture shrinks to one page
            // and requires the value to round-trip. Clamping every
            // `F_SETPIPE_SZ` to the pinned capacity would break that contract
            // while fixing nothing that is actually nondeterministic.
            F_SETPIPE_SZ(requested) if pipe_capacity_request_exceeds_ceiling(requested) => {
                trace!(
                    "[detcore] refusing F_SETPIPE_SZ({}) above the deterministic pipe ceiling {}",
                    requested, DETERMINISTIC_PIPE_CAPACITY_BYTES
                );
                Err(Errno::EPERM.into())
            }
            _ => {
                trace!(
                    "[detcore-finishme]: fcntl unhandled cases: {:?}",
                    call.cmd()
                );
                Ok(self.record_or_replay(guest, call).await?)
            }
        }
    }

    /// ioctl system call
    pub async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        // As in `handle_fcntl`: an asynchronous-I/O owner, and a session whose
        // controlling terminal hangs up, are signalled at a moment set by host
        // timing, so hold their signals in every process.
        let signals = ioctl_host_timed_signals(call.request());
        if signals != 0
            && guest
                .config()
                .backend_supports_blocked_wait_signal_interruption
        {
            record_host_timed_signals(guest, HostTimedSignalScope::Container, signals).await;
        }
        let (cloexec, nonblocking) = match call.request() {
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1142): Review deterministic SIOCETHTOOL rejection.
            // Ethernet link state belongs to the host network namespace and can change between
            // runs. Match record/replay's established policy instead of exposing that state.
            syscalls::ioctl::Request::SIOCETHTOOL(_) => return Err(Errno::ENODEV.into()),
            syscalls::ioctl::Request::FIOCLEX => (Some(true), None),
            syscalls::ioctl::Request::FIONCLEX => (Some(false), None),
            syscalls::ioctl::Request::FIONBIO(value) => {
                let enabled = guest.memory().read_value(value.ok_or(Errno::EFAULT)?)? != 0;
                (None, Some(enabled))
            }
            _ => (None, None),
        };

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1013): Review logical FIONBIO handling for forced fds.
        // Detcore already keeps scheduler-managed fds physically nonblocking. Satisfy
        // FIONBIO logically instead of forwarding it: some backends cannot apply the
        // ioctl to their proxied pipe fd, and clearing it would violate the scheduler's
        // nonblockize-and-retry invariant. This mirrors F_SETFL's forced state split.
        if let Some(enabled) = nonblocking {
            let (fd_type, socketpair_endpoint, physically_nonblocking) =
                guest.thread_state().with_detfd(fd, |detfd| {
                    (
                        detfd.ty(),
                        detfd.is_socketpair_endpoint(),
                        detfd.physically_nonblocking(),
                    )
                })?;
            if self.keeps_physically_nonblocking(
                fd_type,
                socketpair_endpoint,
                physically_nonblocking,
            ) && physically_nonblocking
            {
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_logical_nonblocking(enabled);
                })?;
                return Ok(0);
            }
        }

        let result = self.record_or_replay(guest, call).await?;
        if cloexec.is_some() || nonblocking.is_some() {
            guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(enabled) = cloexec {
                    detfd.set_cloexec(enabled);
                }
                if let Some(enabled) = nonblocking {
                    detfd.set_nonblocking(enabled);
                }
            })?;
        }
        Ok(result)
    }

    /// statfs: report deterministic filesystem statistics.
    ///
    /// The kernel's `statfs` reflects live host state: the free-block counts
    /// (`f_bfree`, `f_bavail`), the free-inode count (`f_ffree`) and the device
    /// id (`f_fsid`) all vary between runs as the underlying host filesystem
    /// fills and drains, which makes a bare passthrough diverge under `--verify`
    /// (e.g. `tar` calls statfs on its target filesystem). The static geometry
    /// of the mount (`f_type`, `f_bsize`, `f_blocks`, `f_namelen`, ...) is
    /// reproducible, so we run the real syscall and then canonicalize only the
    /// volatile fields.
    pub async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statfs,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        self.canonicalize_statfs_buf(guest, call.buf())?;
        Ok(ret)
    }

    /// fstatfs: same determinization as [`Self::handle_statfs`], keyed on an fd.
    pub async fn handle_fstatfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fstatfs,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        self.canonicalize_statfs_buf(guest, call.buf())?;
        Ok(ret)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#1851): Determinized ownership mutation. Emulate the
    // IDENTITY half of the call and let Linux answer the ARGUMENT half.
    /// The `chown` family (`chown`, `fchown`, `fchownat`, `lchown`).
    ///
    /// Detcore presents a fixed virtual-root identity, so the *permission*
    /// answer must be the one a real root gets: success, for any uid. But root
    /// privilege affects only the ownership permission check — it does not
    /// waive pathname, descriptor, or flag errors. A real root's
    /// `chown("/does/not/exist", 0, 0)` still fails with `ENOENT`.
    ///
    /// So this does not fabricate a bare `Ok(0)`. It translates the mutation
    /// into a side-effect-free metadata lookup with the same target-selection
    /// arguments, and reports success only if that lookup succeeds:
    ///
    /// * `F_GETFL` validates `fchown`'s descriptor and distinguishes an
    ///   `O_PATH` descriptor (valid for `fstat`, invalid for `fchown`);
    ///   `newfstatat` performs the corresponding path walk for the three
    ///   pathname variants, preserving `ENOENT`, `ENOTDIR`, `ELOOP`,
    ///   `ENAMETOOLONG`, `EFAULT`, and `EBADF`;
    /// * `fchownat` flags are checked explicitly before the lookup, so an
    ///   unsupported flag still returns `EINVAL` rather than being accepted by
    ///   a metadata syscall with a wider flag vocabulary;
    /// * the ownership assignment itself is not performed, but the *other*
    ///   consequences of a successful chown are, because Linux applies them
    ///   even when ownership does not change. Measured on this host with
    ///   `chown(path, -1, -1)`: mode `06755`, `04755` and `02755` all become
    ///   `0755`; `02644` keeps `S_ISGID` because the file is not
    ///   group-executable; a directory at `06755` keeps both bits; and ctime
    ///   moves in every one of those cases, including the plain `0644` file
    ///   with nothing to clear. Skipping that is a privilege-containment
    ///   regression, not a bookkeeping one: a guest that builds a setuid
    ///   binary and chowns it would see the setuid bit survive under hermit
    ///   and be cleared on the kernel.
    ///
    /// Rather than reimplement that rule, the consequence is delegated to the
    /// kernel by reissuing the *same* call from the same family with the
    /// `(-1, -1)` sentinel, which is precisely the operation whose only effects
    /// are `ATTR_CTIME | ATTR_KILL_SUID | ATTR_KILL_SGID`. Delegation gets the
    /// directory exemption, the group-executable condition on `S_ISGID`, and
    /// symlink handling right for free, and cannot drift from the kernel the
    /// way a transcribed rule would.
    ///
    /// Routed through `record_or_replay` rather than `inject`, so a replay does
    /// not need the guest's filesystem to still exist.
    ///
    /// **Semantic boundary, stated explicitly.** Detcore does not model
    /// per-file ownership, so the success is not observable through a later
    /// `stat`. A guest that chowns to a foreign uid and reads the owner back
    /// sees the unchanged owner — a divergence a single-uid container cannot
    /// avoid, and strictly smaller than the status quo in which the guest
    /// believes it is root and cannot chown at all.
    ///
    /// **Residual, also stated.** This emulates ownership permission and target
    /// validation, not every write-time filesystem policy. In particular a
    /// target on a read-only mount can pass the metadata lookup where a real
    /// chown would return `EROFS`. Path resolution also still requires search
    /// permission on the parent directories, so `EACCES` remains a function of
    /// the host identity under `--no-namespace`. That exposure is shared with
    /// every pass-through filesystem syscall (`open`, `stat`, `chmod`) and is
    /// not introduced here; it is recorded so the boundary is not overstated.
    ///
    /// **Residual introduced by the delegation, stated too.** The sentinel call
    /// needs the same `inode_owner_or_capable` permission the mode change does,
    /// so on a target the guest does not own it returns `EPERM` and that errno
    /// is propagated. Reporting a successful chown while silently failing to
    /// apply the consequence the kernel guarantees would be the same defect in
    /// a narrower form, so this fails closed instead. It is not a regression:
    /// that is exactly the case in which the pass-through implementation also
    /// returned `EPERM`. The case this change exists to fix — a guest chowning
    /// a file it created — is the owning case, and it succeeds.
    ///
    /// The behavioural contract is bracketed end to end by
    /// `hermit-cli/tests/chown_virtual_root_identity.rs`; the unit tests in
    /// `syscall_classification` pin membership only and cannot see this
    /// function's result.
    pub async fn handle_ownership_change_noop<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        // Unreachable while `is_ownership_change_noop_syscall` and the matches
        // below name the same four syscalls. Fail closed if they drift: "not
        // attempted" must never be observationally identical to a validated
        // emulated success.
        if !matches!(
            call,
            Syscall::Chown(_) | Syscall::Fchown(_) | Syscall::Fchownat(_) | Syscall::Lchown(_)
        ) {
            warn!(
                "ownership-change no-op reached with an unexpected syscall {:?}; \
                 refusing unvalidated success",
                call.number()
            );
            return Err(Error::Errno(Errno::ENOSYS));
        }

        // Flag errors precede path resolution in the kernel, so check first.
        if let Syscall::Fchownat(c) = &call {
            let allowed = AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_NOFOLLOW;
            if c.flags().bits() & !allowed.bits() != 0 {
                return Err(Error::Errno(Errno::EINVAL));
            }
        }

        // Argument half. `F_GETFL` answers it for `fchown`: it validates the
        // descriptor and distinguishes an `O_PATH` descriptor, which `fstat`
        // accepts and `fchown` rejects. For the three pathname variants a
        // `newfstatat` with the same target-selection arguments performs the
        // corresponding path walk.
        if let Syscall::Fchown(c) = &call {
            let flags = self
                .record_or_replay(
                    guest,
                    syscalls::Fcntl::new().with_fd(c.fd()).with_cmd(F_GETFL),
                )
                .await?;
            if flags & i64::from(OFlag::O_PATH.bits()) != 0 {
                return Err(Error::Errno(Errno::EBADF));
            }
        } else {
            let mut stack = guest.stack().await;
            let statptr: StatPtr = StatPtr(stack.reserve());
            stack.commit()?;

            let validate = match &call {
                Syscall::Chown(c) => Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(c.path())
                        .with_stat(Some(statptr))
                        .with_flags(AtFlags::empty()),
                ),
                Syscall::Lchown(c) => Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(c.path())
                        .with_stat(Some(statptr))
                        .with_flags(AtFlags::AT_SYMLINK_NOFOLLOW),
                ),
                Syscall::Fchownat(c) => Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(c.dirfd())
                        .with_path(c.path())
                        .with_stat(Some(statptr))
                        .with_flags(c.flags()),
                ),
                _ => return Err(Error::Errno(Errno::ENOSYS)),
            };

            // The errno of the side-effect-free validating call is the guest's
            // answer; only an actually executed successful validation becomes the
            // emulated success. Clear the scratch output on both paths.
            let result = self.record_or_replay(guest, validate).await;
            guest
                .memory()
                .write_exact(statptr.0.cast(), &[0; std::mem::size_of::<libc::stat>()])?;
            result?;
        }

        // Metadata half, delegated to the kernel. The identity assignment is
        // deliberately not performed, but everything else a successful chown
        // does is, by reissuing the same call with the `(-1, -1)` sentinel:
        // clear `S_ISUID`, clear `S_ISGID` on a group-executable file, exempt
        // directories, and move ctime unconditionally. Letting Linux apply its
        // own rule keeps it from drifting here.
        const KEEP_ID: libc::uid_t = libc::uid_t::MAX;
        let consequence = match &call {
            Syscall::Fchown(c) => Syscall::Fchown(
                syscalls::Fchown::new()
                    .with_fd(c.fd())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID),
            ),
            Syscall::Chown(c) => Syscall::Chown(
                syscalls::Chown::new()
                    .with_path(c.path())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID),
            ),
            Syscall::Lchown(c) => Syscall::Lchown(
                syscalls::Lchown::new()
                    .with_path(c.path())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID),
            ),
            Syscall::Fchownat(c) => Syscall::Fchownat(
                syscalls::Fchownat::new()
                    .with_dirfd(c.dirfd())
                    .with_path(c.path())
                    .with_owner(KEEP_ID)
                    .with_group(KEEP_ID)
                    .with_flags(c.flags()),
            ),
            _ => return Err(Error::Errno(Errno::ENOSYS)),
        };
        self.record_or_replay(guest, consequence).await?;
        Ok(0)
    }

    /// Overwrite the host-varying fields of a `statfs` result buffer with fixed
    /// values, leaving the static per-mount geometry intact. Shared by statfs
    /// and fstatfs. A null buffer (only possible on an error return, which the
    /// caller has already propagated) is a no-op.
    fn canonicalize_statfs_buf<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<AddrMut<libc::statfs>>,
    ) -> Result<(), Error> {
        // Fixed *caps* for the volatile counters. The exact values are
        // arbitrary; they only need to be constant so repeated runs agree. We
        // clamp each free count to the mount's (static) total so we never report
        // the impossible "free > total": a filesystem may be smaller than the
        // cap, and some (e.g. overlayfs) report no inode accounting at all
        // (`f_files == 0`).
        const FREE_BLOCKS_CAP: libc::fsblkcnt_t = 1_000_000;
        const FREE_INODES_CAP: libc::fsfilcnt_t = 500_000;

        if let Some(buf) = buf {
            let mut sf = guest.memory().read_value(buf)?;
            let free_blocks = FREE_BLOCKS_CAP.min(sf.f_blocks);
            sf.f_bfree = free_blocks;
            sf.f_bavail = free_blocks;
            // `f_files == 0` means the filesystem does not track inodes; keep the
            // free count at 0 rather than inventing free inodes on a mount that
            // reports none.
            sf.f_ffree = if sf.f_files == 0 {
                0
            } else {
                FREE_INODES_CAP.min(sf.f_files)
            };
            // f_fsid is a device-dependent filesystem identifier; zero it. An
            // all-zero bit pattern is a valid `fsid_t` (a POD id pair).
            sf.f_fsid = unsafe { std::mem::zeroed() };
            guest.memory().write_value(buf, &sf)?;
        }
        Ok(())
    }

    /// dup system call.
    pub async fn handle_dup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = self.record_or_replay(guest, call).await? as RawFd;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(new_fd as i64)
    }

    /// dup2 system call.
    pub async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup2,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// dup3 system call.
    pub async fn handle_dup3<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup3,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let flags = call.flags();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest.thread_state_mut().dup_fd(old_fd, new_fd, flags)?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// pipe2 system call.
    pub async fn handle_pipe2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pipe2,
    ) -> Result<i64, Error> {
        // Pipes are unambiguously container-internal: both endpoints are owned by
        // guest processes. Make them physically nonblocking whenever we sequentialize
        // threads -- INCLUDING record/replay modes. This lets a potentially-blocking
        // pipe read follow the deterministic nonblockize-and-retry (InternalIOPolling)
        // path instead of being descheduled as BlockingExternalIO. A pipe reader and
        // its paired writer are NOT independent, so treating an internal pipe as
        // "external blocking IO" (safe to run in the background and rejoin whenever)
        // deadlocks the sequentialized scheduler in R/R (the documented pipe hang). The
        // physical O_NONBLOCK is Detcore-internal and invisible to the guest (F_GETFL is
        // virtualized), and mirrors what `hermit run --strict` already does for pipes.
        let internally_nonblocking = self.cfg.use_nonblocking_sockets();
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | OFlag::O_NONBLOCK)
        } else {
            call
        };
        // NO PRE-CALL READ OF `pipefd`. This is load-bearing on three backends, not a style
        // choice. `record_or_replay` below returns early on failure, so every guest memory
        // access AFTER it runs only when pipe2 SUCCEEDED -- and a successful pipe2 means the
        // kernel itself wrote two ints there, which proves the address valid. A pre-call
        // snapshot would be the only access to a kernel-UNVALIDATED address, and `LocalMemory`
        // (reverie-dbt, reverie-e9patch, reverie-liteinst) implements reads as an unsafe
        // `copy_nonoverlapping` that always returns `Ok`: a bad pointer is a hardware fault
        // that `Result::ok` cannot catch, so the guest would die with SIGSEGV before Linux
        // could report EFAULT. Letting the kernel touch `pipefd` first is also what keeps its
        // argument-validation precedence intact -- flags are checked before the pointer, so a
        // bad pointer with bad flags is EINVAL and with good flags EFAULT.
        // A C guest asserting that precedence directly is being added separately.
        let res = self.record_or_replay(guest, injected).await?;
        let memory = guest.memory();

        if let Some(pipefd) = call.pipefd() {
            let fds: [i32; 2] = memory.read_value(pipefd)?;
            if internally_nonblocking {
                let capacity_result = guest
                    .inject(
                        syscalls::Fcntl::new()
                            .with_fd(fds[0])
                            .with_cmd(F_SETPIPE_SZ(DETERMINISTIC_PIPE_CAPACITY_BYTES)),
                    )
                    .await;
                if let Some(failure) = pipe_capacity_failure(fds, capacity_result) {
                    // Release the descriptors Linux already created, THEN stop the run.
                    //
                    // Returning `Err` here -- what this code did before -- unwinds without
                    // closing them. `pipe2` has already succeeded, so both descriptors are
                    // live in the guest and are not yet registered with `add_fd`, which means
                    // Detcore's own bookkeeping never learns they exist. Closing them is the
                    // only way the guest's descriptor table matches Detcore's model.
                    //
                    // We do NOT fabricate a `pipe2` errno. Linux leaves `pipefd` untouched
                    // when `pipe2` fails, so inventing a failure would oblige us to restore
                    // the caller's buffer, which needs the pre-call snapshot the comment above
                    // explains we must never take. And returning success with an unpinned pipe
                    // silently restores exactly the host-dependent capacity this path exists
                    // to remove. An unpinnable pipe means determinism is unavailable for this
                    // run, so fail closed and loudly rather than quietly.
                    //
                    // Defensive, not expected: pinning on a freshly created EMPTY pipe is a
                    // shrink or a no-op. EBUSY needs buffered data and EPERM needs to exceed
                    // `pipe-max-size`; neither can hold here.
                    for close in failure.close_syscalls() {
                        let _ = guest.inject(close).await;
                    }
                    error!(
                        "[detcore] cannot pin scheduler-managed pipe to {} bytes (fds {:?}): {}. \
                         Determinism is unavailable for this run.",
                        DETERMINISTIC_PIPE_CAPACITY_BYTES, failure.created_fds, failure.error,
                    );
                    // Fail-closed policy: determinism is unavailable for this run.
                    unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT).await;
                }
            }
            self.add_fd(guest, fds[0], call.flags(), FdType::Pipe)
                .await?;
            self.add_fd(guest, fds[1], call.flags(), FdType::Pipe)
                .await?;
            if internally_nonblocking {
                self.maybe_set_nonblocking_fd(guest, fds[0]);
                self.maybe_set_nonblocking_fd(guest, fds[1]);
            }
        }

        Ok(res)
    }

    /// utime syscall: update access/modification time on a file
    pub async fn handle_utime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utime,
    ) -> Result<i64, Errno> {
        let times = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                [now, now]
            }
            Some(times) => {
                let utimbuf = guest.memory().read_value(times)?;
                [
                    Timespec {
                        tv_sec: utimbuf.actime,
                        tv_nsec: 0,
                    },
                    Timespec {
                        tv_sec: utimbuf.modtime,
                        tv_nsec: 0,
                    },
                ]
            }
        };

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.path());

        self.set_file_times(guest, utimensat, Some(times)).await
    }

    /// utimes syscall
    pub async fn handle_utimes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimes,
    ) -> Result<i64, Errno> {
        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.filename());

        match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                self.set_file_times(guest, utimensat, Some([now, now]))
                    .await
            }
            Some(times) => {
                // Convert the timeval array to a timespec array.
                let mut memory = guest.memory();
                let tvs = memory.read_value(times)?;
                let tp: Addr<[Timespec; 2]> = times.cast();

                // Safety: The address could point to read-only memory and the
                // write below could fail.
                let tp = unsafe { tp.into_mut() };

                memory.write_value(tp, &[tvs[0].into(), tvs[1].into()])?;
                self.set_file_times(guest, utimensat.with_times(Some(tp.into())), None)
                    .await
            }
        }
    }

    /// ustimensat syscall
    pub async fn handle_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
    ) -> Result<i64, Errno> {
        self.set_file_times(guest, call, None).await
    }

    /// Performs a utimensat call and copies the mtime it sets into the virtual
    /// mtime. With `staged` the times are first pushed onto the guest's scratch
    /// stack and replace the call's `times` pointer; utime and utimes(NULL)
    /// build their times that way.
    async fn set_file_times<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
        staged: Option<[Timespec; 2]>,
    ) -> Result<i64, Errno> {
        if !guest.config().virtualize_metadata {
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        // Without thread sequentialization another guest thread can swap the
        // target away and back around the call, so that both lookups name a
        // file the kernel did not update. No lookup can tell the two apart,
        // so the virtual mtime is left alone.
        if !guest.config().sequentialize_threads {
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        // The scratch stack starts 128 bytes below the stack pointer and its
        // addresses are computed by subtraction, so a stack pointer too close
        // to zero to hold the staged times and the lookup buffer would stop
        // Hermit rather than reach Linux.
        let scratch = 128
            + staged.map_or(0, |_| std::mem::size_of::<[Timespec; 2]>())
            + std::mem::size_of::<libc::stat>();
        if usize::try_from(guest.regs().await.rsp).map_or(true, |rsp| rsp <= scratch) {
            info!(
                "Guest stack pointer cannot hold the utimensat target lookup; \
                 leaving the virtual mtime unchanged."
            );
            return self.utimensat_without_lookup(guest, call, staged).await;
        }

        // The staged times and the buffer for the target lookups share one
        // scratch stack, and its guard is held until the last injected syscall
        // has run, so that neither the kernel's read of the times nor a lookup
        // finds the other's bytes, or a restored stack, at its address.
        let mut stack = guest.stack().await;
        let staged_call = match staged {
            Some(times) => call.with_times(Some(stack.push(times))),
            None => call,
        };
        let statptr: StatPtr = StatPtr(stack.reserve());
        // The scratch stack lies below the guest's red zone, where a raw
        // syscall may keep its own path or times. Writing the lookup buffer
        // over either would change the call before Linux reads it.
        if utimensat_input_overlaps(&guest.memory(), &call, staged.is_none(), statptr) {
            info!(
                "utimensat inputs overlap the target lookup buffer; \
                 leaving the virtual mtime unchanged."
            );
            drop(stack);
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        // The buffer's address range can still share its pages with guest data
        // through a second shared mapping, which no address comparison sees.
        // Its bytes are saved here and put back after the commit and after
        // each lookup, so the lookups and the call read their inputs unchanged
        // and the guest keeps its memory.
        let mut saved = [0u8; std::mem::size_of::<libc::stat>()];
        if guest
            .memory()
            .read_exact(statptr.0.cast(), &mut saved)
            .is_err()
        {
            info!(
                "Guest stack scratch cannot hold the utimensat target lookup; \
                 leaving the virtual mtime unchanged."
            );
            drop(stack);
            return self.utimensat_without_lookup(guest, call, staged).await;
        }
        let _guard = match stack.commit() {
            Ok(guard) => guard,
            // The lookup buffer only serves the virtual update, so a scratch
            // stack that cannot hold it, for example one next to the guard
            // page, must not keep the guest's own call from reaching Linux.
            // The commit writes page by page, so it can fail after writing a
            // lower writable page; those bytes are put back first.
            Err(_) => {
                info!(
                    "Guest stack scratch cannot hold the utimensat target lookup; \
                     leaving the virtual mtime unchanged."
                );
                restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
                return self.utimensat_without_lookup(guest, call, staged).await;
            }
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        let call = staged_call;

        // The kernel applies the new times to the real file, but the guest
        // observes the virtual mtime, which otherwise only moves on writes. Copy
        // the requested mtime into it so that `tar` extraction, `cp -p` and
        // `touch -r` restore a file's mtime and `make` compares the times the
        // build asked for rather than the order in which files were unpacked.
        //
        // Nothing below may change the syscall's result: an unreadable `times`
        // or a failed lookup only skips the virtual update, and the kernel
        // reports its own error for the call itself.
        let mtime = match (staged, call.times()) {
            (Some([_, mtime]), _) => Some(mtime),
            (None, None) => Some(Timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            }),
            (None, Some(times)) => guest
                .memory()
                .read_value(times)
                .ok()
                .map(|[_, mtime]| mtime),
        }
        .filter(|mtime| mtime.tv_nsec != libc::UTIME_OMIT);
        let before = match mtime {
            Some(_) => self.utimensat_target(guest, &call, statptr).await,
            None => None,
        };
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);

        let res = self.record_or_replay(guest, call).await?;

        // Update only the inode the kernel modified. No other guest thread
        // runs during the call, but a process outside the container can still
        // rename, unlink or replace the target; the target is resolved before
        // and after the call, and the update is skipped unless both name the
        // same file. Equal lookups alone do not show that the kernel updated
        // that file: the name may have been swapped away and back during the
        // call, or an inode number reused. So an explicit mtime is copied only
        // when the file holds it, truncated to the filesystem's granularity,
        // and the virtual mtime takes the value the kernel stored, which is
        // what stat reports on Linux. `UTIME_NOW` has no such witness.
        let (Some(mtime), Some(before)) = (mtime, before) else {
            return Ok(res);
        };
        let after = self.utimensat_target(guest, &call, statptr).await;
        restore_lookup_buffer(&mut guest.memory(), statptr, &saved);
        let Some(after) = after else {
            return Ok(res);
        };
        if (after.st_dev, after.st_ino) != (before.st_dev, before.st_ino) {
            return Ok(res);
        }
        let file = RawInode::new(after.st_dev, after.st_ino);
        if mtime.tv_nsec == libc::UTIME_NOW {
            touch_file(guest, file).await;
            return Ok(res);
        }
        const NANOS_PER_SEC: i128 = 1_000_000_000;
        let requested = i128::from(mtime.tv_sec) * NANOS_PER_SEC + i128::from(mtime.tv_nsec);
        let stored = i128::from(after.st_mtime) * NANOS_PER_SEC + i128::from(after.st_mtime_nsec);
        // Linux truncates the requested time down to the filesystem's
        // granularity, at most a second on the filesystems builds use.
        if (0..NANOS_PER_SEC).contains(&(requested - stored)) {
            let nanos = u64::try_from(stored.max(0)).unwrap_or(u64::MAX);
            set_file_mtime(guest, file, LogicalTime::from_nanos(nanos)).await;
        }
        Ok(res)
    }

    /// Performs a utimensat call without the virtual mtime update. Staged
    /// times are then the only scratch-stack allocation, as they were before
    /// the update existed.
    async fn utimensat_without_lookup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
        staged: Option<[Timespec; 2]>,
    ) -> Result<i64, Errno> {
        let Some(times) = staged else {
            return self.record_or_replay(guest, call).await;
        };
        let mut stack = guest.stack().await;
        let call = call.with_times(Some(stack.push(times)));
        let _guard = stack.commit()?;
        self.record_or_replay(guest, call).await
    }

    /// The metadata of the file a utimensat call targets, with the same target
    /// selection: the descriptor itself for `futimens` (a NULL path), else a
    /// path walk honoring the flags utimensat accepts. `None` if the lookup
    /// fails.
    async fn utimensat_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &syscalls::Utimensat,
        statptr: StatPtr<'_>,
    ) -> Option<libc::stat> {
        let lookup = match call.path() {
            None => Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(call.dirfd())
                    .with_stat(Some(statptr)),
            ),
            Some(path) => {
                let allowed = libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH;
                Syscall::Newfstatat(
                    syscalls::Newfstatat::new()
                        .with_dirfd(call.dirfd())
                        .with_path(Some(path))
                        .with_stat(Some(statptr))
                        .with_flags(AtFlags::from_bits_truncate(call.flags() & allowed)),
                )
            }
        };
        self.record_or_replay(guest, lookup).await.ok()?;
        statptr.read(&guest.memory()).ok()
    }

    /// socket system call.
    pub async fn handle_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        // The socket syscall itself is not blocking, but we must decide whether to make the socket
        // returned physically nonblocking.
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // Allow possibly blocking syscall in record mode
            let fd = self.record_or_replay(guest, call).await? as RawFd;
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(call.r#type()),
                FdType::Socket,
            )
            .await?;
            self.mark_sock_diag_fd(guest, fd, &call);
            Ok(fd as i64)
        } else {
            // Under run mode, force all sockets to be registered to be nonblocking in the OS:
            let call2 = if self.cfg.use_nonblocking_sockets() {
                call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
            } else {
                call
            };
            let fd = self.record_or_replay(guest, call2).await? as RawFd; // Cannot hang.
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.maybe_set_nonblocking_fd(guest, fd);
            self.mark_sock_diag_fd(guest, fd, &call);

            Ok(fd as i64)
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1064)
    /// Flag `AF_NETLINK`/`NETLINK_SOCK_DIAG` sockets so `handle_recvmsg`
    /// determinizes the socket inode numbers carried by their binary dump
    /// replies (see `crate::sock_diag`). Best-effort: if the descriptor lookup
    /// fails the reply is simply left unsanitized.
    fn mark_sock_diag_fd<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd, call: &syscalls::Socket) {
        if call.family() != libc::AF_NETLINK {
            return;
        }
        if call.protocol() == libc::NETLINK_SOCK_DIAG {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_sock_diag());
        }
        // TODO-HUMAN-REVIEW(PR-2478)
        // NETLINK_ROUTE link dumps carry live interface counters. They were
        // invisible until IO-buffer hashing went on by default, because the
        // reply's LENGTH and return value are identical between runs and only
        // the payload bytes move.
        if call.protocol() == libc::NETLINK_ROUTE {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_netlink_route());
        }
    }

    /// socketpair system call.
    pub async fn handle_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socketpair,
    ) -> Result<i64, Error> {
        let call2 = if self.cfg.sequentialize_threads && !self.cfg.debug_externalize_sockets {
            call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
        } else {
            call
        };
        let res = self.record_or_replay(guest, call2).await?;
        if let Some(usockvec) = call.usockvec() {
            let memory = guest.memory();
            let fds: [i32; 2] = memory.read_value(usockvec)?;

            // Logical flags are as requested:
            self.add_fd(
                guest,
                fds[0],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.add_fd(
                guest,
                fds[1],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;

            // Both endpoints are container-internal by construction: socketpair(2)
            // creates the pair itself, so no peer outside the container can ever
            // hold one. Record that here, at the only point where it is known,
            // so potentially-blocking I/O on them takes the same
            // nonblockize-and-retry path as a pipe rather than being treated as
            // external blocking I/O against an fd Detcore just made SOCK_NONBLOCK.
            // See `syscall_targets_internal_fd`. Only when the endpoints really
            // are physically nonblocking, which is the same condition as `call2`
            // above: under `debug_externalize_sockets` they stay external.
            if self.cfg.use_nonblocking_sockets() {
                for fd in fds {
                    let _ = guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_socketpair_endpoint());
                }
            }

            self.maybe_set_nonblocking_fd(guest, fds[0]);
            self.maybe_set_nonblocking_fd(guest, fds[1]);
        }
        Ok(res)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Apply a socket option to an already tracked socket. Record mode captures
    /// the result; replay re-applies a successful option before later socket I/O,
    /// which remains mediated by Detcore's nonblocking scheduler paths.
    pub async fn handle_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsockopt,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Transition an already tracked socket into listening state.
    pub async fn handle_listen<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Listen,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return the local address of a tracked socket.
    pub async fn handle_getsockname<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockname,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return the peer address of a tracked socket.
    pub async fn handle_getpeername<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getpeername,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return an option value from a tracked socket. Hermit only promises normal
    /// run determinism for isolated guest networking; record/replay captures the
    /// result when external socket state is part of the recording boundary.
    pub async fn handle_getsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockopt,
    ) -> Result<i64, Error> {
        // TODO-HUMAN-REVIEW(PR-894): Review deterministic network-namespace identity.
        let requested_length =
            if call.level() == libc::SOL_SOCKET && call.optname() == libc::SO_NETNS_COOKIE {
                let fd_type = guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.ty())?;
                if fd_type == FdType::Socket {
                    call.optlen()
                        .map(|length| guest.memory().read_value(length))
                        .transpose()?
                } else {
                    None
                }
            } else {
                None
            };

        // TODO-HUMAN-REVIEW(PR-886): Review deterministic SO_COOKIE identities.
        let deterministic_cookie =
            if call.level() == libc::SOL_SOCKET && call.optname() == libc::SO_COOKIE {
                let requested_length = call
                    .optlen()
                    .map(|length| guest.memory().read_value(length))
                    .transpose()?;
                let open_file_id = guest
                    .thread_state()
                    .with_detfd(call.fd(), |detfd| detfd.open_file_id())?;
                Some((open_file_id.deterministic_socket_cookie(), requested_length))
            } else {
                None
            };

        let result = self.record_or_replay(guest, call).await?;

        // TODO-HUMAN-REVIEW(PR-898): Hermit exposes one virtual CPU, so do not
        // leak the host CPU that processed a socket's most recent packet.
        if result == 0
            && call.level() == libc::SOL_SOCKET
            && call.optname() == libc::SO_INCOMING_CPU
            && let (Some(optval), Some(optlen)) = (call.optval(), call.optlen())
        {
            let returned_len: libc::socklen_t = guest.memory().read_value(optlen)?;
            let zero_cpu = 0_i32.to_ne_bytes();
            let returned_len = (returned_len as usize).min(zero_cpu.len());
            guest
                .memory()
                .write_exact(optval.cast::<u8>(), &zero_cpu[..returned_len])?;
        }
        if result == 0
            && call.level() == libc::IPPROTO_TCP
            && call.optname() == libc::TCP_INFO
            && let (Some(optval), Some(optlen)) = (call.optval(), call.optlen())
        {
            let returned_len: libc::socklen_t = guest.memory().read_value(optlen)?;
            let mut info = vec![0; returned_len as usize];
            let optval = optval.cast::<u8>();
            guest.memory().read_exact(optval, info.as_mut_slice())?;
            canonicalize_tcp_info(&mut info);
            guest.memory().write_exact(optval, info.as_slice())?;
        }
        if let Some(requested_length) = requested_length
            && let Some(value) = call.optval()
        {
            let bytes = DETERMINISTIC_NETNS_COOKIE.to_ne_bytes();
            let write_length = (requested_length as usize).min(bytes.len());
            guest
                .memory()
                .write_exact(value.cast(), &bytes[..write_length])?;
        }
        if let Some((cookie, Some(requested_length))) = deterministic_cookie
            && let Some(value) = call.optval()
        {
            let bytes = cookie.to_ne_bytes();
            let write_length = (requested_length as usize).min(bytes.len());
            guest
                .memory()
                .write_exact(value.cast(), &bytes[..write_length])?;
        }
        Ok(result)
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#818)
    /// Half-close the read and/or write direction of an already tracked socket.
    /// shutdown never blocks and returns no data; its effect is deterministic
    /// given the container's socket state, so it forwards via record_or_replay
    /// exactly like the rest of the socket family (KVM ratchet round 12).
    pub async fn handle_shutdown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Shutdown,
    ) -> Result<i64, Error> {
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// bind system call.
    pub async fn handle_bind<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Bind,
    ) -> Result<i64, Error> {
        // WIP!
        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing bind");
            let resource = ResourceID::PriorityChangePoint(
                LAST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
                guest.thread_state().committed_clock_value,
                Vec::new(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }
        let addr = call.umyaddr().ok_or(Errno::EFAULT)?;
        let sock_fd = call.fd();
        let open_file_id = guest
            .thread_state()
            .with_detfd(sock_fd, |detfd| detfd.open_file_id())?;

        let sockaddr_family = guest.memory().read_value(addr.cast::<u16>())?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-872): Review deterministic AF_UNIX autobind identity.
        if sockaddr_family == libc::AF_UNIX as u16
            && call.addrlen() == std::mem::offset_of!(libc::sockaddr_un, sun_path) as i32
        {
            let resp = send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
            let port = match resp.1 {
                GlobalResponse::RequestPort(port) => port,
                GlobalResponse::PortFull => {
                    return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                }
                _ => unreachable!(),
            };

            let mut stack = guest.stack().await;
            let autobind_addr: AddrMut<libc::sockaddr_un> = stack.reserve();
            let _stack_guard = stack.commit()?;
            guest
                .memory()
                .write_value(autobind_addr, &unix_autobind_address(port))?;
            let deterministic_bind = call
                .with_umyaddr(Some(autobind_addr.cast()))
                .with_addrlen(unix_autobind_addrlen());
            return Ok(self.record_or_replay(guest, deterministic_bind).await?);
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-880): Review deterministic Netlink autobind identities.
        } else if sockaddr_family == libc::AF_NETLINK as u16
            && call.addrlen() >= std::mem::size_of::<libc::sockaddr_nl>() as i32
        {
            let mut sockaddr_nl: libc::sockaddr_nl = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_nl>())?;
            if sockaddr_nl.nl_pid == 0 {
                let resp =
                    send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
                match resp.1 {
                    GlobalResponse::RequestPort(port) => {
                        sockaddr_nl.nl_pid = DETERMINISTIC_NETLINK_PORT_ID_BASE | u32::from(port);
                        let mut stack = guest.stack().await;
                        let deterministic_addr: AddrMut<libc::sockaddr_nl> = stack.reserve();
                        let _stack_guard = stack.commit()?;
                        guest
                            .memory()
                            .write_value(deterministic_addr, &sockaddr_nl)?;
                        let deterministic_bind = call.with_umyaddr(Some(deterministic_addr.cast()));
                        return Ok(self.record_or_replay(guest, deterministic_bind).await?);
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        } else if sockaddr_family == libc::AF_INET as u16 {
            // For IPv4
            let mut sockaddr_in: libc::sockaddr_in = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in>())?;

            let port = sockaddr_in.sin_port.to_be();
            let ipaddr = Ipv4Addr::from(sockaddr_in.sin_addr.s_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                // Send RPC to make sure already used ports are not used.
                let resp =
                    send_and_update_time(guest, GlobalRequest::AddUsedPort(port, open_file_id))
                        .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                // Request a determinzed port
                let resp =
                    send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockaddr_in.sin_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in>(), &sockaddr_in)?;
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        } else if sockaddr_family == libc::AF_INET6 as u16 {
            // For IPv6
            let mut sockfaddr_in: libc::sockaddr_in6 = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in6>())?;
            let port = sockfaddr_in.sin6_port.to_be();
            let ipaddr = Ipv6Addr::from(sockfaddr_in.sin6_addr.s6_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                let resp =
                    send_and_update_time(guest, GlobalRequest::AddUsedPort(port, open_file_id))
                        .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                let resp =
                    send_and_update_time(guest, GlobalRequest::RequestPort(open_file_id)).await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockfaddr_in.sin6_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in6>(), &sockfaddr_in)?;
                        trace!("Port assigned {}", port_assigned)
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        }
        let res = self.record_or_replay(guest, call).await?;

        Ok(res)
    }

    /// Create and register an event notification counter.
    ///
    /// Determinism: strict execution serializes creation, so the initial counter, guest-visible
    /// flags, and descriptor number depend only on syscall arguments and the reconstructed file
    /// table. Any internally added nonblocking flag remains hidden from the guest.
    pub async fn handle_eventfd2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Eventfd2,
    ) -> Result<i64, Error> {
        let internally_nonblocking =
            self.cfg.use_nonblocking_sockets() && !self.cfg.recordreplay_modes;
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | syscalls::EfdFlags::EFD_NONBLOCK)
        } else {
            call
        };
        let fd = self.record_or_replay(guest, injected).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::EFD_CLOEXEC | libc::EFD_NONBLOCK),
            ),
            FdType::Eventfd,
        )
        .await?;
        if internally_nonblocking {
            self.maybe_set_nonblocking_fd(guest, fd);
        }
        Ok(fd as i64)
    }

    /// signalfd4 system call.
    pub async fn handle_signalfd4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Signalfd4,
    ) -> Result<i64, Error> {
        let signalfd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            signalfd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::SFD_CLOEXEC | libc::SFD_NONBLOCK),
            ),
            FdType::Signalfd,
        )
        .await?;
        Ok(signalfd as i64)
    }

    /// Create and register a timer notification descriptor.
    ///
    /// Determinism: strict execution serializes creation, which exposes only kernel validation,
    /// guest-visible flags, and a descriptor number; this operation does not read the clock.
    pub async fn handle_timerfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::TFD_CLOEXEC | libc::TFD_NONBLOCK),
            ),
            FdType::Timerfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// Serialize a notification descriptor control operation.
    async fn notification_fd_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// timerfd_settime system call.
    pub async fn handle_timerfd_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdSettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// timerfd_gettime system call.
    pub async fn handle_timerfd_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdGettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_init1 system call.
    pub async fn handle_inotify_init1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyInit1,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits() & (libc::IN_CLOEXEC | libc::IN_NONBLOCK)),
            FdType::Inotify,
        )
        .await?;
        Ok(fd as i64)
    }

    /// inotify_add_watch system call.
    pub async fn handle_inotify_add_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyAddWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_rm_watch system call.
    pub async fn handle_inotify_rm_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyRmWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// memfd_create system call.
    pub async fn handle_memfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::MemfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate((call.flags() & libc::MFD_CLOEXEC) as i32),
            FdType::Memfd,
        )
        .await?;
        Ok(fd as i64)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-862): pidfd creation and Detcore FD registration.
    /// Create a pidfd through record/replay and synchronize the descriptor with
    /// Detcore's metadata before fcntl, poll, close, or waitid can observe it.
    pub async fn handle_pidfd_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::PidfdOpen,
    ) -> Result<i64, Error> {
        let allowed_flags = libc::O_NONBLOCK as u32;
        if call.flags() & !allowed_flags != 0 {
            return Err(Errno::EINVAL.into());
        }

        let fd = self.record_or_replay(guest, call).await? as RawFd;
        let flags = OFlag::O_CLOEXEC | OFlag::from_bits_truncate(call.flags() as libc::c_int);
        self.add_fd(guest, fd, flags, FdType::Pidfd).await?;
        let target = DetPid::from_raw(call.pid() as i32);
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_pidfd_target(target))?;
        Ok(fd as i64)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1175): pidfd_send_signal(2) determinization.
    /// Deliver a signal to the process referred to by a pidfd.
    ///
    /// `pidfd_send_signal(pidfd, sig, info, flags)` names its target by an open
    /// kernel descriptor rather than a numeric PID. Unlike `kill(2)`, there is
    /// therefore no host-PID/virtual-PID ambiguity for Detcore to resolve: the
    /// pidfd was bound to one specific process at `pidfd_open` time. Signal
    /// generation runs inside this thread's serialized scheduler turn, exactly
    /// like `tgkill`/`tkill`/`rt_tgsigqueueinfo` (which also just forward through
    /// record/replay), so forwarding the kernel call is deterministic by
    /// construction. This handler adds deterministic argument validation ahead of
    /// the forward: a descriptor that Detcore does not model as a pidfd fails
    /// closed with `EBADF`, and the flags field the current kernel reserves is
    /// required to be zero (`EINVAL` otherwise), so the guest-visible errno is
    /// fixed and host-independent.
    ///
    /// `call` is the raw `Syscall::Other`; `pidfd`/`flags` are pre-extracted from
    /// its arguments by the dispatcher.
    pub async fn handle_pidfd_send_signal<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        pidfd: RawFd,
        signal: i32,
        flags: u32,
    ) -> Result<i64, Error> {
        // The kernel currently reserves `flags`; a nonzero value is EINVAL.
        if flags != 0 {
            return Err(Errno::EINVAL.into());
        }
        // Fail closed unless Detcore models this descriptor as a pidfd. This also
        // yields a deterministic EBADF for an unknown/closed descriptor.
        let is_pidfd = guest
            .thread_state()
            .with_detfd(pidfd, |detfd| matches!(detfd.ty(), FdType::Pidfd))?;
        if !is_pidfd {
            return Err(Errno::EBADF.into());
        }
        if signal == libc::SIGALRM {
            // `pidfd_open` records the process a pidfd names; a pidfd with no
            // recorded target is refused while any process handles SIGALRM.
            let control = match guest
                .thread_state()
                .with_detfd(pidfd, |detfd| detfd.pidfd_target())?
            {
                Some(target) => SigalrmControl::SendTo(DetTid::from_raw(target.as_raw())),
                None => SigalrmControl::SendToUnnamed,
            };
            refuse_sigalrm(guest, control).await?;
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1175): pidfd_getfd(2) determinization and the
    // FdType of the duplicated descriptor.
    /// Duplicate a descriptor from the process referred to by a pidfd.
    ///
    /// `pidfd_getfd(pidfd, targetfd, flags)` returns a fresh descriptor in the
    /// caller that aliases `targetfd` in the target process. The source pidfd
    /// names one specific process fixed at `pidfd_open` time, the returned
    /// descriptor number is chosen through record/replay (so it is stable across
    /// runs), and a successful modeled operation executes inside this thread's
    /// serialized turn, so the result is deterministic. For zero flags, Detcore
    /// fails closed with `EBADF` unless it models the descriptor as a pidfd.
    /// Linux checks the kernel-reserved `flags` first, however, so a nonzero
    /// value takes the raw record/replay path and preserves the kernel's exact
    /// `EINVAL` across valid and invalid descriptor combinations.
    ///
    /// The modeled path is narrower than "same process": the caller must be the
    /// thread-group leader named by the pidfd. `CLONE_THREAD` does not imply
    /// `CLONE_FILES`, so a nonleader can share the target's TGID while using a
    /// different descriptor table. Requiring `target == getpid() == gettid()`
    /// proves that `targetfd` is resolved in the caller's exact table. The
    /// returned descriptor is then modeled as a real alias of the source open
    /// file description. Broader support needs a cross-task OFD channel that
    /// Detcore does not have today, so every other target is refused with
    /// `EOPNOTSUPP`. Failed calls leave descriptor state unchanged.
    pub async fn handle_pidfd_getfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        pidfd: RawFd,
        targetfd: RawFd,
        flags: u32,
    ) -> Result<i64, Error> {
        if flags != 0 {
            return match self.record_or_replay(guest, call).await {
                Err(error) => Err(error.into()),
                Ok(fd) => {
                    let fd = fd as RawFd;
                    let close_result = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                    Err(Error::Tool(anyhow::anyhow!(
                        "pidfd_getfd unexpectedly accepted reserved flags and returned fd {fd}; cleanup close result: {close_result:?}"
                    )))
                }
            };
        }

        if !guest.config().sequentialize_threads {
            return self
                .refuse_unserviceable_operation(guest, Sysno::pidfd_getfd, Errno::EOPNOTSUPP)
                .await;
        }

        let current_tgid = DetPid::from_raw(guest.inject(syscalls::Getpid::new()).await? as i32);
        let current_tid = DetTid::from_raw(guest.inject(syscalls::Gettid::new()).await? as i32);
        let source = guest.thread_state().capture_pidfd_getfd_source(
            pidfd,
            targetfd,
            current_tgid,
            current_tid,
        )?;
        let fd = match self.record_or_replay(guest, call).await {
            Ok(fd) => fd as RawFd,
            Err(error) => {
                if let Some(open_file_id) = guest.thread_state().abandon_captured_fd(source) {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                return Err(error.into());
            }
        };
        // pidfd_getfd always sets FD_CLOEXEC on the returned descriptor.
        let replaced = match guest.thread_state_mut().install_captured_fd(
            source,
            fd,
            OFlag::O_CLOEXEC,
        ) {
            Ok(replaced) => replaced,
            Err(error @ CapturedDetFdInstallError { .. }) => {
                let expected_files_id = error.expected_files_id;
                let actual_files_id = error.actual_files_id;
                let cleanup = error.into_cleanup();
                let close_result = guest
                    .inject(syscalls::Close::new().with_fd(cleanup.close_fd))
                    .await;
                if let Some(open_file_id) = cleanup.release_open_file {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                if let Err(close_error) = close_result {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "pidfd_getfd returned fd {fd}, but its captured source table changed from {expected_files_id:?} to {actual_files_id:?}; cleanup close failed with {close_error}"
                    )));
                }
                warn!(
                    "pidfd_getfd returned fd {fd}, but its captured source table changed from {expected_files_id:?} to {actual_files_id:?}; closed the result and refusing with EOPNOTSUPP"
                );
                return Err(Errno::EOPNOTSUPP.into());
            }
        };
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(fd as i64)
    }

    /// userfaultfd system call.
    pub async fn handle_userfaultfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Userfaultfd,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags()),
            FdType::Userfaultfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// accept4 system call (MAYHANG).
    ///
    /// Category: External OR Internal IO
    /// ---------------------------------
    /// When do we know?  We only know if an accept4 did an extra-container IO AFTER it returns.
    /// I.e. we could accept a connection from another endpoint in the container, or from the outside,
    /// and we don't know which at the point where `accept4` is called.
    pub async fn handle_accept4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
    ) -> Result<i64, Error> {
        // This option applies both to the socket we're doing the accept call on, and the connection
        // that we return. We don't have any smart detection yet to separate internal/external, so
        // applies to everything.
        let call2 = if self.cfg.use_nonblocking_sockets() {
            // Let the socket returned from accept4 be physically nonblocking:
            call.with_flags(call.flags() | SockFlag::SOCK_NONBLOCK)
        } else {
            call
        };
        // This will do blocking/polling as appropriate based on the fd status:
        let fd = self.execute_nonblockable_fd_syscall(guest, call2).await? as RawFd;

        self.add_fd(
            guest,
            fd,
            // This will specify whether the socket returned is logically non-blocking:
            oflag_from_sock_bits(call.flags().bits()),
            FdType::Socket,
        )
        .await?;
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_accepted_connection())?;

        self.maybe_set_nonblocking_fd(guest, fd);

        Ok(fd as i64)
    }

    /// The stat that identifies the object behind descriptor `fd`, for the
    /// identity lookups that key a deterministic inode on it; `Ok(None)` when
    /// Detcore tracks no such descriptor or has no stat for it.
    ///
    /// An ordinary descriptor's cached stat is its own and is returned as it
    /// is. An inherited stdio descriptor, or a dup of one, is the exception:
    /// `setup_stdio` gives descriptors 0, 1 and 2 one stand-in, the `fstat(0)`
    /// of the process that runs Detcore (Hermit's on ptrace and KVM, the
    /// guest's on DBT and LiteInst), so a stdout or stderr on another device
    /// than stdin, and every dup of one, would be keyed on stdin's file. With
    /// `virtualize_metadata` its identity is an injected `fstat` of the
    /// descriptor itself, cached on its open file description
    /// (`DetFd::identity_stat`), which every alias shares: an alias's `fstat`
    /// describes the same object, so it does not matter which alias asks
    /// first.
    ///
    /// Without `virtualize_metadata` -- `hermit record` and `hermit replay`
    /// -- the stand-in is still returned, as before: no `fstat` is injected
    /// in that mode, as in `handle_mmap`, because at replay it would describe
    /// the replayer's descriptor rather than the recorded one.
    ///
    /// `EBADF` from the `fstat` is the kernel's answer that the descriptor is
    /// not open, and is returned as that errno. Any other failure means
    /// Detcore could not ask, and is refused (`identity_lookup_refused`).
    pub(crate) async fn descriptor_identity_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<DetStat>, Error> {
        let Ok((stat, stdio, identity_stat)) = guest.thread_state().with_detfd(fd, |detfd| {
            (
                detfd.stat(),
                is_container_stdio(detfd.resource()),
                detfd.identity_stat(),
            )
        }) else {
            return Ok(None);
        };
        if !stdio || !guest.config().virtualize_metadata {
            return Ok(stat);
        }
        if identity_stat.is_some() {
            return Ok(identity_stat);
        }
        match self.inject_fstat(guest, fd).await {
            Ok(own) => {
                let own = DetStat::from(own);
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_identity_stat(own))?;
                Ok(Some(own))
            }
            Err(Errno::EBADF) => Err(Error::Errno(Errno::EBADF)),
            Err(errno) => Err(identity_lookup_refused(
                "fstat the inherited stdio descriptor",
                format!("fd {fd}").as_bytes(),
                errno,
            )),
        }
    }

    /// The raw identity of the file a write through `fd` changes, for the
    /// virtual mtime bump of the write family: the descriptor's own object,
    /// [`Self::descriptor_identity_stat`]. The cached stat of an inherited
    /// stdio descriptor, or of a dup of one, is the `fstat(0)` stand-in all
    /// three share, so bumping it bumped stdin's file for a write to stdout
    /// while a `stat` of stdout's file looked under that file's own identity.
    /// `Ok(None)` when Detcore has no stat for `fd`. Asked before the kernel
    /// writes, so a refusal leaves the guest's file unchanged.
    async fn written_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<RawInode>, Error> {
        Ok(self
            .descriptor_identity_stat(guest, fd)
            .await?
            .map(|stat| stat.raw_inode()))
    }

    /// [`Self::descriptor_identity_stat`] of each inherited stdio descriptor,
    /// 0 to 2, indexed by descriptor number: `None` where the descriptor is
    /// not open, or no longer carries an inherited stdio resource
    /// (`dup2(file, 1)` gives descriptor 1 the file's).
    pub(crate) async fn inherited_stdio_identity_stats<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<[Option<DetStat>; 3], Error> {
        let mut stats = [None; 3];
        for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
            let inherited = guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    deterministic_stdio_inode_for_resource(fd, detfd.resource()).is_some()
                })
                .unwrap_or(false);
            if !inherited {
                continue;
            }
            stats[fd as usize] = match self.descriptor_identity_stat(guest, fd).await {
                Ok(stat) => stat,
                // The kernel holds no descriptor there, so no object is that
                // descriptor's.
                Err(Error::Errno(Errno::EBADF)) => None,
                Err(error) => return Err(error),
            };
        }
        Ok(stats)
    }

    /// The stdio identities that keep a fixed inode (1000 plus the descriptor
    /// number) in the views that name an object rather than a descriptor: the
    /// memory-map sanitizer's `stdio_by_raw_file` table and another process's
    /// `/proc/<pid>/fd` link (`canonicalize_other_proc_fd_target`).
    ///
    /// This is [`Self::inherited_stdio_identity_stats`] with descriptor
    /// `fd`'s entry kept only when its own identity is the stand-in
    /// `setup_stdio` cached for it (`DetFd::stat`): the `fstat(0)` of the
    /// process that runs Detcore (Hermit's on ptrace and KVM, the guest's on
    /// DBT and LiteInst). Those views gave a fixed inode to that object and
    /// to no other before each descriptor's own identity was asked, so the
    /// fixed inode stays only where it was.
    ///
    /// A stdout or stderr that is another object -- a file or a pipe stdout
    /// was redirected to while stdin is something else -- is left out, so
    /// these views name it by its pooled inode. That is the inode an alias of
    /// it above descriptor 2 reports through `fstat` and `fdinfo`, and the
    /// one a `stat` of its path reports. Keeping its own identity in the
    /// table gave its mappings, and other processes' links to it, descriptor
    /// 1's fixed inode instead, which none of those views report (round-11
    /// review of <https://github.com/rrnewton/hermit/pull/3255>). An `fstat`
    /// of descriptor 1 itself, and its own `/proc/self/fd/1` link, still
    /// report the fixed inode. That difference is older than this table:
    /// removing it needs the inode pool to give such an object a single
    /// identity, which is not done here.
    ///
    /// Asks nothing beyond [`Self::inherited_stdio_identity_stats`]: the
    /// stand-in is already cached. Without `virtualize_metadata` every
    /// identity is the stand-in itself, so nothing is dropped. On SaBRe,
    /// `setup_stdio` caches each descriptor's own `fstat`, so nothing is
    /// dropped there either, and a stdout that is another object keeps
    /// descriptor 1's fixed inode in these views, as it did before.
    pub(crate) async fn fixed_stdio_identity_stats<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<[Option<DetStat>; 3], Error> {
        let mut stats = self.inherited_stdio_identity_stats(guest).await?;
        for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
            let stand_in = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.raw_inode()))
                .ok()
                .flatten();
            let slot = &mut stats[fd as usize];
            if slot.is_some_and(|own| Some(own.raw_inode()) != stand_in) {
                *slot = None;
            }
        }
        Ok(stats)
    }

    /// getdents system call.
    pub async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let buf = call.dirent().ok_or(Errno::EFAULT)?.cast::<u8>();
        // Resolved before the real call: see `directory_device`.
        let device = self.directory_device(guest, call.fd() as RawFd).await?;
        let lookup = self.directory_entry_lookup(guest, call.fd() as RawFd).await;
        self.serve_directory_stream(
            guest,
            GetdentsCall {
                call: Syscall::from(call),
                empty: Syscall::from(call.with_count(0)),
                fd: call.fd() as i32,
                device,
                lookup,
                buf,
                capacity: call.count() as usize,
                format: DirentFormat::Legacy,
            },
        )
        .await
    }

    /// getdents64 system call.
    pub async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents64,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }
        let buf = call.dirent().ok_or(Errno::EFAULT)?.cast::<u8>();
        // Resolved before the real call: see `directory_device`.
        let device = self.directory_device(guest, call.fd() as RawFd).await?;
        let lookup = self.directory_entry_lookup(guest, call.fd() as RawFd).await;
        self.serve_directory_stream(
            guest,
            GetdentsCall {
                call: Syscall::from(call),
                empty: Syscall::from(call.with_count(0)),
                fd: call.fd() as i32,
                device,
                lookup,
                buf,
                capacity: call.count() as usize,
                format: DirentFormat::Dirent64,
            },
        )
        .await
    }

    /// Answer a `getdents` call from the open file's sorted directory stream
    /// (see [`DirectoryStream`]), reading the host directory first if the
    /// stream has no snapshot.
    ///
    /// Sorting each kernel buffer on its own is not enough: a directory larger
    /// than one buffer (about a thousand short names for glibc's 32KiB) would
    /// come back as sorted runs whose boundaries, and the host `d_off` cookies
    /// inside them, depend on the filesystem's on-disk layout.
    async fn serve_directory_stream<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<i64, Error> {
        let negative = i32::try_from(call.capacity).is_err();
        let lock = match guest
            .thread_state()
            .with_detfd(call.fd, |detfd| detfd.directory_lock())
        {
            Ok(lock) => lock,
            // Detcore has no open file description for this descriptor (one
            // received through SCM_RIGHTS, for example), so there is nowhere
            // to keep a stream. Sort the one kernel buffer instead, as before
            // directory streams existed; the kernel reports a descriptor that
            // is not open at all.
            Err(Errno::EBADF) if negative => {
                return self.serve_negative_count(guest, call, None).await;
            }
            Err(Errno::EBADF) => return self.sort_one_buffer(guest, call).await,
            Err(errno) => return Err(errno.into()),
        };
        let _serving = lock.lock().await;
        let (host_order, needs_snapshot, position) =
            guest.thread_state().with_detfd(call.fd, |detfd| {
                (
                    detfd.directory_in_host_order(),
                    detfd.directory_needs_snapshot(),
                    detfd.with_directory_stream(|stream| stream.position()).ok(),
                )
            })?;
        if negative {
            let stream = if host_order {
                None
            } else {
                position.map(|position| (position, needs_snapshot))
            };
            return self.serve_negative_count(guest, call, stream).await;
        }
        if host_order {
            return self.sort_one_buffer(guest, call).await;
        }
        if !needs_snapshot {
            return self.serve_next_batch(guest, call).await;
        }
        let retirements = retirement_count(guest).await;
        match self.snapshot_directory_privately(guest, call).await {
            Ok(Some(entries)) => {
                let mut entries = Some(entries);
                guest.thread_state().with_detfd(call.fd, |detfd| {
                    detfd
                        .install_directory_snapshot(entries.take().unwrap_or_default(), retirements)
                })?;
                self.serve_next_batch(guest, call).await
            }
            Ok(None) => {
                guest
                    .thread_state()
                    .with_detfd(call.fd, |detfd| detfd.use_host_directory_order())?;
                self.sort_one_buffer(guest, call).await
            }
            Err(error) => Err(error),
        }
    }

    /// Answer a `getdents` whose count does not fit in an `int`. Linux keeps
    /// the count in one, where it is negative, so no entry fits: the call
    /// fails with `EINVAL`, or returns 0 at the end of the directory, and
    /// moves nothing. It does not look at the buffer, so a count of 0 gets
    /// the same answer for any pointer, and the same `EBADF`, `ENOTDIR` or
    /// `ENOENT`; that is the call issued, since a backend that does not
    /// truncate the guest's count to an `int` would copy host records into
    /// the guest's buffer.
    ///
    /// `stream` is the directory stream's position and whether it needs a
    /// snapshot, or `None` when the open file has no stream (it has not been
    /// read, is read in host order, or is not tracked). Then the guest's
    /// position is the kernel's, so the kernel's answer is Linux's. So it is
    /// for a stream rewound to 0, whose kernel position is the start. A
    /// stream that has a snapshot decides between `EINVAL` and 0 itself. The
    /// kernel position of a stream without a snapshot is moved to the start
    /// first, and then to where the snapshot, if made, puts it. A descriptor
    /// Detcore does not track that shares the open file, and had moved that
    /// position, reads on from there, where on Linux the call moves nothing. A
    /// stream rewound and then seeked past 0 needs to know how many entries
    /// the directory has, so a snapshot is taken (see
    /// [`Self::snapshot_directory_privately`]). If the directory must be
    /// read in host order instead, the kernel's answer from the start is
    /// returned, which is `EINVAL` even at or past the end; if no mapping can
    /// be made for the snapshot, the call fails with `ENOMEM`
    /// (https://github.com/rrnewton/hermit/issues/3723).
    async fn serve_negative_count<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
        stream: Option<(u64, bool)>,
    ) -> Result<i64, Error> {
        if let Some((_, true)) = stream {
            // Without a snapshot, the kernel position of a stream is the
            // start; a descriptor Detcore does not track may have moved it.
            self.move_directory_kernel_position(guest, call.fd, 0).await;
        }
        let kernel = self.record_or_replay(guest, call.empty).await;
        match kernel {
            Ok(_) | Err(Errno::EINVAL) => {}
            Err(errno) => return Err(errno.into()),
        }
        match stream {
            None | Some((0, true)) => return Ok(kernel.map(|_| 0)?),
            Some((_, true)) => {
                let retirements = retirement_count(guest).await;
                match self.snapshot_directory_privately(guest, call).await {
                    Ok(Some(entries)) => {
                        let mut entries = Some(entries);
                        let target = guest.thread_state().with_detfd(call.fd, |detfd| {
                            detfd.install_directory_snapshot(
                                entries.take().unwrap_or_default(),
                                retirements,
                            );
                            detfd.with_directory_stream(|stream| stream.kernel_target())
                        })??;
                        self.move_directory_kernel_position(guest, call.fd, target)
                            .await;
                    }
                    Ok(None) => return Ok(kernel.map(|_| 0)?),
                    Err(error) => return Err(error),
                }
            }
            Some((_, false)) => {}
        }
        let batch = guest.thread_state().with_detfd(call.fd, |detfd| {
            detfd.with_directory_stream(|stream| stream.next_batch(call.format, 0))
        })??;
        batch?;
        Ok(0)
    }

    /// Return the entries at the stream's position that fit in the guest's
    /// buffer, with determinized inodes and each `d_off` naming the position
    /// after its entry, counting from 1.
    ///
    /// Whatever the outcome, the kernel position then follows the stream (see
    /// [`DirectoryStream::kernel_target`]).
    async fn serve_next_batch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: GetdentsCall<'_>,
    ) -> Result<i64, Error> {
        let (start, batch, retirements) = guest.thread_state().with_detfd(call.fd, |detfd| {
            detfd.with_directory_stream(|stream| {
                (
                    stream.position(),
                    stream.next_batch(call.format, call.capacity),
                    stream.snapshot_retirements(),
                )
            })
        })??;
        let (passed, copied) = match batch {
            Ok(batch) => {
                let mut records = Vec::new();
                let mut names = Vec::with_capacity(batch.len());
                // On overlayfs, settled from the whole sorted snapshot, so
                // that where the stream stands, and which entries the guest
                // read before, do not change which entries are asked.
                let told = if batch.is_empty() {
                    Vec::new()
                } else {
                    self.settle_overlay_entry_lookup(guest, &mut call, None)
                        .await?
                };
                // Every entry's identity is resolved before the first
                // numbering request, so a refused entry lookup consumes no
                // number: how many a call consumes must not depend on which
                // host lookup fails (see `Self::directory_entry_identities`).
                let identities = self
                    .directory_entry_identities(guest, &mut call, &batch, &told)
                    .await?;
                for (index, (entry, identity)) in batch.iter().zip(identities).enumerate() {
                    let (d_ino, _) = determinize_listed_inode(guest, identity, retirements).await;
                    let d_off = i64::try_from(start + index as u64 + 1).unwrap_or(i64::MAX);
                    call.format
                        .encode(entry, d_ino.as_raw(), d_off, &mut records);
                    names.push(entry.name.len());
                }
                copy_records(
                    &mut guest.memory(),
                    call.buf,
                    call.format,
                    &records,
                    &names,
                    start,
                )
            }
            Err(errno) => (0, Err(errno)),
        };
        // With --no-sequentialize-threads another thread can close the
        // descriptor during the copy; like Linux, the call's result stands.
        let target = guest.thread_state().with_detfd(call.fd, |detfd| {
            detfd.with_directory_stream(|stream| {
                stream.advance(passed);
                stream.kernel_target()
            })
        });
        if let Ok(Ok(target)) = target {
            self.move_directory_kernel_position(guest, call.fd, target)
                .await;
        }
        Ok(copied.map(|len| len as i64)?)
    }

    /// The raw identity that keys the deterministic inode of `entry`, read
    /// by `call` from the directory open as `call.fd`, whose device is
    /// `call.device` (see [`Self::directory_device`]).
    ///
    /// An entry's inode number is not always on the directory's device. On
    /// overlayfs with `xino=off` over layers on different filesystems, a
    /// file's `d_ino` is its `st_ino`, but `stat` reports the layer's device.
    /// So an entry that `call.lookup` asks (see [`EntryLookup`]) is `lstat`ed
    /// relative to the descriptor, with `AT_SYMLINK_NOFOLLOW` because a
    /// `d_ino` names a symbolic link itself, and `AT_NO_AUTOMOUNT` so that
    /// asking mounts nothing. When that reports the entry's `d_ino`, the
    /// device it reports keys the entry, so the guest's `d_ino` and `st_ino`
    /// for the file agree, as they do natively. When it reports another
    /// inode, `call.device` keys the entry: a mount point's entry names the
    /// directory beneath the mount, and a btrfs subvolume's carries the
    /// subvolume's id, and both differ from `st_ino` natively too. So does an
    /// entry removed or replaced after the read. `call.device` also keys the
    /// entry when the guest's filesystem answers that the name reaches
    /// nothing (`ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`, or `EACCES`
    /// from a directory that may be read but not searched). That is a guess,
    /// not a proof. No `stat` through this name can contradict it, but the
    /// same object may have another link, in a directory the guest can
    /// search, whose `stat` reports it on another device; then the two views
    /// key different identities. This is a known gap.
    ///
    /// On overlayfs, `call.lookup` was settled before the first entry was
    /// keyed (see [`Self::settle_overlay_entry_lookup`]), and an entry whose
    /// `lstat` was asked for that, which is in `told` by name, is not asked
    /// again: its answer keys it as a fresh one would.
    ///
    /// An entry's key depends only on its own answers, never on which
    /// [`EntryLookup`] asked it. So on an overlay settled as
    /// [`EntryLookup::OverlayLayerDevices`], an entry whose `lstat` reports
    /// its `d_ino` is keyed on the device that `lstat` reports, a mount root
    /// included (a file bind-mounted over the entry reports the mount's
    /// source), as its `stat` and its `maps` line key it and as
    /// [`EntryLookup::Every`] keys it, and nothing more is asked. Keying a
    /// mount root on `call.device` there instead gave the guest a `d_ino`
    /// other than its `st_ino` for the file, a key that depended on the
    /// policy, which an inherited seccomp filter chooses, and an injected
    /// mapping that [`EntryLookup::Every`] does not make, which on the KVM
    /// backend moves where a later guest `mmap` lands (Codex review round 11
    /// of https://github.com/rrnewton/hermit/pull/3255). A listing read
    /// before such an overlay settles keys a mount root on `call.device`
    /// (see [`Self::settle_overlay_entry_lookup`]), so on that kind of
    /// overlay a mount root's key changes when the overlay settles: the first
    /// class of the second known gap below.
    ///
    /// An entry that `call.lookup` does not ask is keyed on `call.device`
    /// without a `stat`. Where its `lstat` would have reported its `d_ino` on
    /// another device, the guest's `d_ino` and `st_ino` for it then differ
    /// where Linux's agree. That is a second known gap, narrower than the
    /// first: outside CephFS it needs an entry whose own `lstat` leaves the
    /// directory's device and lands on the same inode number. The classes
    /// are (1) a mount point, or a file bind-mounted over
    /// another, whose mounted root or source file has the inode number of the
    /// entry it covers. A filesystem's root has a small fixed number (1 on
    /// procfs, sysfs and tmpfs, 2 on ext4, usually 128 on xfs, 256 on a btrfs
    /// subvolume), so a mount of a whole filesystem needs a covered entry with
    /// one of those numbers. On btrfs none is an ordinary entry's: every
    /// number below 256 is reserved, and 256 is a subvolume's root, which is
    /// asked. On ext4, 1 and 2 are reserved but 128 and 256 are ordinary, and
    /// on tmpfs, procfs, sysfs or xfs an ordinary entry can have one. A bind
    /// mount of a file, or of a directory other than its filesystem's root,
    /// such as a container's `/etc/hosts` or a volume, can carry any number. A source on
    /// the covered entry's own device has another number, or is the same file
    /// and reports the same device. A source on another device collides in
    /// two ways. By chance, when the numbers happen to coincide; btrfs makes
    /// that likelier than it sounds, since each subvolume is its own device
    /// and numbers its inodes from 256, and a snapshot keeps every number of
    /// its source. And by identity: on an overlay whose layers share one
    /// filesystem, and on one that folds for a file on the upper layer's
    /// filesystem, an entry's `d_ino` is its backing file's inode number, so
    /// that backing file, or a hard link to it, bound over the entry's own
    /// path reports the same number on the layers' device; and on an overlay
    /// that neither shares one filesystem nor folds, a file's `d_ino` is its
    /// backing file's number too, which reports its layer's device. This
    /// class is open wherever an entry is not asked: on btrfs, on an overlay
    /// before it has settled and once it has settled as
    /// [`EntryLookup::OverlayOwnDevice`], and on every filesystem that asks
    /// only `..`; CephFS, and an overlay settled as
    /// [`EntryLookup::OverlayLayerDevices`], ask every entry but `.` and
    /// close it. (2) A filesystem other than overlayfs, CephFS
    /// and btrfs whose `stat` reports a device per entry rather than its
    /// superblock's, or an overlayfs whose devices do not follow the rules of
    /// mainline's `ovl_map_dev_ino` that [`EntryLookup::Overlay`] relies on.
    /// None is handled; bcachefs subvolumes and union filesystems
    /// outside mainline, such as aufs and shiftfs, are not checked, and the
    /// mainline filesystems checked are not all of them. FUSE, NFS and ecryptfs
    /// report the superblock's device, and their submounts are mounts, class
    /// (1).
    ///
    /// Any other failure is a refusal, and `ESRCH` its errno, as for every
    /// guest stat (see [`Self::stat_guest_path`]). The getdents has already
    /// been issued then. Each entry asked costs one injected `fstatat`.
    async fn directory_entry_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &mut GetdentsCall<'_>,
        entry: &DirEntry,
        told: &[ToldEntry],
    ) -> Result<RawInode, Error> {
        if !call.lookup.asks(entry) {
            return Ok(RawInode::new(call.device, entry.ino));
        }
        let stat = match told.iter().find(|told| told.name == entry.name) {
            Some(told) => told.stat,
            None => Self::stat_guest_path_at(guest, StatAt::entry_of(call.fd), &entry.name).await?,
        };
        Ok(match stat {
            Some(stat) if stat.st_ino == entry.ino => RawInode::new(stat.st_dev, entry.ino),
            _ => RawInode::new(call.device, entry.ino),
        })
    }

    /// [`Self::directory_entry_identity`] of each of `entries`, in order,
    /// all resolved before the caller sends its first numbering request.
    ///
    /// The inode pool consumes one number per request whether or not it
    /// mints (<https://github.com/rrnewton/hermit/issues/2897>), and each
    /// entry sends exactly one, whatever its lookup found. A lookup that
    /// fails is a refusal; resolving every entry first means that a refused
    /// listing has consumed no number, rather than one per entry before the
    /// refused one, a count that would depend on which host lookup failed.
    async fn directory_entry_identities<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &mut GetdentsCall<'_>,
        entries: &[DirEntry],
        told: &[ToldEntry],
    ) -> Result<Vec<RawInode>, Error> {
        let mut identities = Vec::with_capacity(entries.len());
        for entry in entries {
            identities.push(
                self.directory_entry_identity(guest, call, entry, told)
                    .await?,
            );
        }
        Ok(identities)
    }

    /// Whether the entry `name` of the directory open as `dirfd`, whose
    /// `lstat` reported `stat`, is on the mount the directory is on, asked by
    /// a guest `statx(dirfd, name, AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT,
    /// STATX_INO)` in a transient page. The last component of a path either
    /// stays on its directory's mount or crosses onto a mount over it, whose
    /// root it then is, so the entry is on the directory's mount exactly when
    /// `statx` does not report `STATX_ATTR_MOUNT_ROOT` (Linux 5.8; `fs/stat.c`
    /// sets it for the root of every mount, a file bind mount included).
    ///
    /// `Some(false)` when the entry is a mount root, and when the guest's
    /// filesystem answers that the name now reaches another file than `stat`
    /// described (the entry changed between the two calls), since then this
    /// `lstat` says nothing about the directory's mount. `Some(true)` when
    /// the kernel does not report the attribute (`stx_attributes_mask`
    /// without it, before Linux 5.8): it cannot tell, and the answer is taken
    /// as it was before this was asked.
    ///
    /// `None` when the question cannot be asked: the guest's `mmap` of the
    /// page, its `statx` or its `munmap` fails with any errno but `ESRCH`, as
    /// under a seccomp filter the launcher did not see (one that refuses
    /// `statx` only in the container's mount namespace, Codex review round 11
    /// of https://github.com/rrnewton/hermit/pull/3255, F4). A `statx` that
    /// reports no file at a name whose `lstat` just answered is such a
    /// failure too: a filter can return `ENOENT` or `EACCES` as well as
    /// `EPERM`. The caller then keys the listing as [`EntryLookup::Every`]
    /// does, which asks no such question. `ESRCH`, the thread being gone, is
    /// that errno, as for the `lstat` (see [`Self::stat_guest_path_at`]).
    ///
    /// Asked only by [`Self::settle_overlay_entry_lookup`], for a candidate
    /// whose `lstat` would settle an overlay as
    /// [`EntryLookup::OverlayLayerDevices`]: at most once per candidate, and
    /// on an overlay that settles, once per run for most overlays.
    async fn entry_is_on_the_listed_mount<G: Guest<Self>>(
        guest: &mut G,
        dirfd: RawFd,
        name: &[u8],
        stat: &libc::stat,
    ) -> Result<Option<bool>, Error> {
        if name.contains(&0) {
            return Ok(Some(false));
        }
        let cannot_ask = |step: &str, errno: Option<Errno>| {
            info!(
                "could not ask whether directory entry {:?} is a mount root ({step}: {errno:?}); \
                 the listing is keyed as Every keys it",
                String::from_utf8_lossy(name)
            );
            Ok(None)
        };
        let statx_offset = (name.len() + 1).next_multiple_of(8);
        let len =
            (statx_offset + std::mem::size_of::<libc::statx>()).next_multiple_of(host_page_size());
        let page = match map_transient_page(guest, len).await {
            Ok(page) => page,
            Err(Errno::ESRCH) => return Err(Error::Errno(Errno::ESRCH)),
            Err(errno) => return cannot_ask("mmap", Some(errno)),
        };
        // SAFETY: the offset is within the `len` bytes just mapped.
        let statxptr = StatxPtr(unsafe { page.add(statx_offset) }.cast::<libc::statx>());
        let asked = Self::statx_in_page(
            guest,
            StatAt::entry_of(dirfd),
            name,
            page,
            statxptr,
            StatxMask::STATX_INO,
        )
        .await;
        let unmapped = unmap_transient_page(guest, page, len).await;
        let statx = match asked {
            Ok(Some(statx)) => statx,
            Err(Errno::ESRCH) => return Err(Error::Errno(Errno::ESRCH)),
            Ok(None) => return cannot_ask("statx reported no file", None),
            Err(errno) => return cannot_ask("statx", Some(errno)),
        };
        match unmapped {
            Ok(()) => {}
            Err(Errno::ESRCH) => return Err(Error::Errno(Errno::ESRCH)),
            Err(errno) => return cannot_ask("munmap", Some(errno)),
        }
        let device = libc::makedev(statx.stx_dev_major, statx.stx_dev_minor);
        if (device, statx.stx_ino) != (stat.st_dev, stat.st_ino) {
            trace!(
                "directory entry {:?} names another file than its lstat did",
                String::from_utf8_lossy(name)
            );
            return Ok(Some(false));
        }
        let mount_root = libc::STATX_ATTR_MOUNT_ROOT as u64;
        if statx.stx_attributes_mask & mount_root == 0 {
            return Ok(Some(true));
        }
        Ok(Some(statx.stx_attributes & mount_root == 0))
    }

    /// On overlayfs, settle which of two kinds the overlay `call` lists is
    /// (see [`EntryLookup::Overlay`]) before any entry of the listing is
    /// keyed, and return the `lstat` of each entry asked for that, by name,
    /// for [`Self::directory_entry_identity`] to use. Any other
    /// `call.lookup` is left as it is, and nothing is asked.
    ///
    /// The kind is a property of the overlay, so the first answer for an
    /// overlay is kept for the whole run (see
    /// [`GlobalRequest::SettleOverlayEntryLookup`]), and every descriptor,
    /// tracked or not, every later open and every alias keys the overlay's
    /// entries alike. Until it is settled, the entries that may tell (see
    /// [`EntryLookup::may_tell_the_overlay_kind`]) are asked in sorted order
    /// until one tells (see [`EntryLookup::learn`]). A candidate whose `lstat`
    /// would settle [`EntryLookup::OverlayLayerDevices`] is first asked
    /// whether it is the root of a mount (see
    /// [`Self::entry_is_on_the_listed_mount`]): a file bind-mounted over a
    /// merged entry is not a file of the overlay, and can report another
    /// device with the entry's own number on an overlay of either kind, so a
    /// mount root tells nothing. Without that, which directory of the overlay
    /// the run listed first chose the kind, and with it the keys of the whole
    /// overlay (Codex review round 9 of
    /// https://github.com/rrnewton/hermit/pull/3255). The answer
    /// [`EntryLookup::OverlayOwnDevice`] needs no such question: only a file
    /// of the overlay reports the overlay's device. For a stream served from
    /// its snapshot (`buffer` `None`), they are those of the whole sorted
    /// snapshot, not of the guest's batch, so neither the stream's position
    /// nor which entries the guest read before changes the answer, nor which
    /// entries are asked. Before this was settled ahead of keying, the first
    /// such entry the guest's reads reached decided, and an entry was keyed
    /// on one device or another depending on the read
    /// (https://github.com/rrnewton/hermit/pull/3255).
    ///
    /// A read with no snapshot, of a descriptor Detcore does not track or of
    /// a directory read in host order, asks the entries of the one kernel
    /// `buffer` it sorted. Where such a read is the first of an overlay in
    /// the run, the answer can come from another file than a snapshot's
    /// first, chosen by the host's order of the directory; the two answers
    /// differ only where that file is a mount, the first class of the second
    /// known gap at [`Self::directory_entry_identity`], and either one is
    /// then kept for the rest of the run. That is a known gap.
    ///
    /// A listing none of whose entries tells is keyed as
    /// [`EntryLookup::OverlayOwnDevice`], and nothing is settled, so a later
    /// listing asks again. The entries that kind leaves unasked are then
    /// directories, which report the overlay's device on every kind, files
    /// whose `lstat` did not report their `d_ino`, which are keyed on the
    /// directory's device either way, and mount roots, so the key is the one
    /// either kind gives, but for a mount point (a directory, or a file
    /// whose `lstat` reported its `d_ino` on another device as a mount
    /// root): the first class of the second known gap again, keyed on the
    /// directory's device in such a listing and on its `lstat`'s device once
    /// the overlay has settled as [`EntryLookup::OverlayLayerDevices`], as
    /// its `stat` and [`EntryLookup::Every`] key it (see
    /// [`Self::directory_entry_identity`]). Such a listing cannot key the
    /// file otherwise: it is the same, answer for answer, as one of an
    /// overlay whose layers share one filesystem, where the file is bound
    /// over the same way and which settles as
    /// [`EntryLookup::OverlayOwnDevice`], keying it on the directory's device
    /// in every listing. Keying it on the directory's device once the overlay
    /// has settled as [`EntryLookup::OverlayLayerDevices`] too, so that its
    /// key would not change when the overlay settles (Codex review round 10
    /// of https://github.com/rrnewton/hermit/pull/3255), made the settled
    /// listing disagree with the file's `stat` and with
    /// [`EntryLookup::Every`] (round 11 of the same review).
    ///
    /// The overlay is known by the raw device its directories report, which
    /// Linux gives no other mounted filesystem while it is mounted; a device
    /// number reused by another overlay after an unmount within one run
    /// would inherit the first one's answer.
    ///
    /// A mount-root question that cannot be asked (see
    /// [`Self::entry_is_on_the_listed_mount`]) keys the listing as
    /// [`EntryLookup::Every`] keys it, which asks every entry's `lstat` and no
    /// mount-root question: the narrow policies are an optimization of
    /// `Every`, so a refusal of a question only they ask gives the guest
    /// exactly `Every`'s identities and the guest continues (Codex review
    /// round 11 of https://github.com/rrnewton/hermit/pull/3255, F4), where
    /// the refusal used to end the guest. The refusal is no evidence of the
    /// overlay's kind, so nothing is settled and nothing is kept on the
    /// descriptor: the same refusal at the same point of a later listing
    /// gives the same keys. The `lstat`s asked before it are reused, and the
    /// listing asks for the same numbers, in the same order, as under
    /// `Every`, so no inode number is spent that `Every` does not spend.
    ///
    /// A failed `lstat` that [`Self::directory_entry_identity`] would refuse
    /// is the same refusal here, before any numbering request.
    async fn settle_overlay_entry_lookup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &mut GetdentsCall<'_>,
        buffer: Option<&[DirEntry]>,
    ) -> Result<Vec<ToldEntry>, Error> {
        let mut told = Vec::new();
        if call.lookup != EntryLookup::Overlay {
            return Ok(told);
        }
        let mut settled = settle_overlay_entry_lookup(guest, call.device, None).await;
        if settled.is_none() {
            let candidates: Vec<DirEntry> = match buffer {
                Some(buffer) => buffer
                    .iter()
                    .filter(|entry| EntryLookup::may_tell_the_overlay_kind(entry))
                    .cloned()
                    .collect(),
                None => guest.thread_state().with_detfd(call.fd, |detfd| {
                    detfd.with_directory_stream(|stream| {
                        stream
                            .entries()
                            .iter()
                            .filter(|entry| EntryLookup::may_tell_the_overlay_kind(entry))
                            .cloned()
                            .collect()
                    })
                })??,
            };
            for entry in candidates {
                let stat =
                    Self::stat_guest_path_at(guest, StatAt::entry_of(call.fd), &entry.name).await?;
                let mut shown = stat
                    .as_ref()
                    .map(|stat| EntryLookup::Overlay.learn(&entry, stat, call.device))
                    .filter(|shown| *shown != EntryLookup::Overlay);
                // Only a file of the overlay itself tells which kind it is. A
                // mount over the entry (a file bind-mounted over it) can show
                // another device with the entry's number on either kind, but
                // never the overlay's own, so only that answer is checked.
                let mut refused = false;
                if let (Some(EntryLookup::OverlayLayerDevices), Some(lstat)) = (shown, &stat) {
                    match Self::entry_is_on_the_listed_mount(guest, call.fd, &entry.name, lstat)
                        .await?
                    {
                        Some(true) => {}
                        Some(false) => shown = None,
                        None => refused = true,
                    }
                }
                told.push(ToldEntry {
                    name: entry.name,
                    stat,
                });
                if refused {
                    // The question this policy asks beyond `Every`'s was
                    // refused: this listing is keyed as `Every` keys it, and
                    // nothing is settled or kept, so a later listing asks
                    // again.
                    call.lookup = EntryLookup::Every;
                    return Ok(told);
                }
                if let Some(shown) = shown {
                    settled = settle_overlay_entry_lookup(guest, call.device, Some(shown)).await;
                    break;
                }
            }
        }
        match settled {
            Some(lookup) => {
                call.lookup = lookup;
                // A descriptor Detcore does not track has nowhere to keep it.
                let _ = guest
                    .thread_state()
                    .with_detfd(call.fd, |detfd| detfd.set_directory_entry_lookup(lookup));
            }
            None => call.lookup = EntryLookup::OverlayOwnDevice,
        }
        Ok(told)
    }

    /// Which entries of the directory open as `fd` are `lstat`ed for the
    /// device their inode number is on (see [`EntryLookup`]), from the type
    /// of the directory's filesystem. A descriptor Detcore tracks keeps the
    /// answer on its open file description, so it is asked once per open
    /// directory, along with an overlay's settled kind (see
    /// [`Self::settle_overlay_entry_lookup`]); for any other descriptor, each
    /// getdents asks an injected `fstatfs` (see
    /// [`Self::inject_fstatfs_type`]) and the overlay's settled kind afresh.
    ///
    /// Asking every entry is never less faithful than asking fewer: an entry
    /// whose `lstat` does not report its `d_ino` is keyed on the directory's
    /// device either way. So when the `fstatfs` fails or cannot be asked (an
    /// unwritable stack scratch, for example), every entry is asked, as
    /// before the type was consulted, and nothing is cached; the failure
    /// never becomes the getdents' result.
    ///
    /// The `fstatfs` is a syscall the guest never made, run in the guest's
    /// thread under every seccomp filter the guest runs under, and a filter's
    /// verdict on it is not always an errno Detcore can fall back from:
    ///
    /// - `SECCOMP_RET_ERRNO` fails the call, and every entry is asked, as
    ///   above.
    /// - `SECCOMP_RET_TRAP` skips the call and sends the thread a `SIGSYS`;
    ///   the ptrace backend reports `ENOSYS` and requeues the signal to the
    ///   guest, which by default dies of it.
    /// - `SECCOMP_RET_KILL_THREAD` and `SECCOMP_RET_KILL_PROCESS` kill before
    ///   Detcore sees any result.
    ///
    /// The last two would make a listing signal or kill a program that runs
    /// natively. So the `fstatfs`, and on an overlay the `mmap`, `statx` and
    /// `munmap` with which [`Self::entry_is_on_the_listed_mount`] asks whether
    /// an entry is a mount root, are injected only where the launcher found
    /// that no filter may refuse them: Detcore refuses a guest's own filter,
    /// and the launcher sets
    /// [`crate::Config::seccomp_may_refuse_entry_lookup_syscalls`] when a
    /// filter of the process that starts the guest, which every guest it
    /// starts inherits, may refuse one of those calls, or when it cannot tell
    /// (that field says how it asks, and what its answer cannot show). Then
    /// every entry is asked, none of those calls is injected, and nothing is
    /// cached. A filter the launcher could not see that refuses one of them
    /// with an errno anyway (one that answers differently in the container's
    /// mount namespace, for example) costs only the narrowing: a refused
    /// `fstatfs` asks every entry, as above, and a refused mount-root
    /// question keys that listing as `Every` keys it (see
    /// [`Self::settle_overlay_entry_lookup`]). The backend's own filter, such
    /// as the ptrace backend's, lets Detcore's injections through. The
    /// `fstatat` of each asked entry, the
    /// `fstat` of a directory Detcore does not track (see
    /// [`Self::directory_device`]), and the `mmap` and `munmap` of the private
    /// mapping a directory snapshot is read into (see
    /// [`Self::snapshot_directory_privately`]) are injected regardless, and a
    /// filter that traps or kills those still signals or kills the guest;
    /// Detcore injects all four numbers for other guest calls too.
    ///
    /// The getdents handlers call this before the real call, after
    /// [`Self::directory_device`].
    async fn directory_entry_lookup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> EntryLookup {
        if guest.config().seccomp_may_refuse_entry_lookup_syscalls {
            return EntryLookup::Every;
        }
        let cached = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.directory_entry_lookup());
        if let Ok(Some(lookup)) = cached {
            return lookup;
        }
        match Self::inject_fstatfs_type(guest, fd).await {
            Ok(f_type) => {
                let lookup = EntryLookup::of_filesystem(f_type);
                // A descriptor Detcore does not track has nowhere to keep it.
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.set_directory_entry_lookup(lookup));
                lookup
            }
            Err(errno) => {
                info!(
                    "fstatfs of directory fd {} failed ({}); every entry's lstat is asked.",
                    fd, errno
                );
                EntryLookup::Every
            }
        }
    }

    /// The host device of the directory open as `fd`, which keys an entry's
    /// inode number unless the entry's own `lstat` reports that number on
    /// another device (see [`Self::directory_entry_identity`]). It is the
    /// device of the descriptor's identity stat (`descriptor_identity_stat`:
    /// its cached stat, or an inherited stdio descriptor's own `fstat` in
    /// place of the stand-in it was given) when there is one, and an injected
    /// `fstat` otherwise.
    ///
    /// A descriptor Detcore does not track (one received over `SCM_RIGHTS`,
    /// for example) has no cached stat, which is not an error: the kernel owns
    /// the descriptor table, and the injected `fstat` asks it directly. The
    /// getdents handlers call this BEFORE the real system call, so an `fstat`
    /// failure is reported without consuming directory entries, and a
    /// successful getdents is never turned into an error afterwards.
    ///
    /// Only `EBADF` is the descriptor's own answer, one the getdents would
    /// report too, so it is the call's result. Any other failure means
    /// Detcore could not ask -- for example `ENOMEM` from the transient page
    /// `inject_fstat` maps when the guest's stack cannot hold the buffer,
    /// which can fail for a valid descriptor at the guest's address-space or
    /// mapping-count limit -- and it is a tool error
    /// (`identity_lookup_refused`), never an errno the guest would take as
    /// its getdents' own result.
    async fn directory_device<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<u64, Error> {
        let cached = self
            .descriptor_identity_stat(guest, fd)
            .await?
            .map(|stat| stat.dev);
        match cached {
            Some(device) => Ok(device),
            None => match self.inject_fstat(guest, fd).await {
                Ok(stat) => Ok(stat.st_dev),
                Err(Errno::EBADF) => Err(Error::Errno(Errno::EBADF)),
                Err(errno) => Err(identity_lookup_refused(
                    "fstat the directory descriptor",
                    format!("fd {fd}").as_bytes(),
                    errno,
                )),
            },
        }
    }

    /// Move the kernel position of the open file behind `fd` to `target`, a
    /// [`DirectoryStream::kernel_target`], so that a descriptor Detcore does
    /// not track that aliases it reads every entry the stream has not
    /// returned. Without this, the snapshot leaves the kernel at the end of
    /// the directory, and such a descriptor would read nothing.
    ///
    /// The guest's own call has already been decided, so a failed seek is not
    /// reported to it; the next move of the stream seeks again.
    async fn move_directory_kernel_position<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        target: i64,
    ) {
        let seek = syscalls::Lseek::new()
            .with_fd(fd)
            .with_offset(target)
            .with_whence(Whence::SEEK_SET);
        let _ = self.record_or_replay(guest, seek).await;
    }

    /// Answer `getdents` without a directory stream by issuing the guest's own
    /// call, sorting the entries of that one kernel buffer and determinizing
    /// their inodes. The host's `d_off` cookies are kept, so the kernel can
    /// still interpret them. A directory larger than one buffer comes back as
    /// sorted runs whose boundaries depend on the host.
    async fn sort_one_buffer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: GetdentsCall<'_>,
    ) -> Result<i64, Error> {
        let len = self.record_or_replay(guest, call.call).await? as usize;
        if len == 0 {
            return Ok(0);
        }
        let mut entries = read_records(&guest.memory(), call.buf, len, call.format)?;
        sort_dir_entries(&mut entries);
        let mut records = Vec::with_capacity(len);
        let told = self
            .settle_overlay_entry_lookup(guest, &mut call, Some(&entries))
            .await?;
        let identities = self
            .directory_entry_identities(guest, &mut call, &entries, &told)
            .await?;
        for (entry, identity) in entries.iter().zip(identities) {
            let (d_ino, _) = determinize_named_inode(guest, identity).await;
            call.format
                .encode(entry, d_ino.as_raw(), entry.off, &mut records);
        }
        if guest.memory().write_exact(call.buf, &records).is_err() {
            // The padding after the last name may not be writable, as Linux
            // does not write it.
            let written = entries.last().map_or(0, |last| {
                records.len() - call.format.record_len(last.name.len())
                    + call.format.written_len(last.name.len())
            });
            guest.memory().write_exact(call.buf, &records[..written])?;
        }
        Ok(records.len() as i64)
    }

    /// Read the whole host directory behind `call.fd` into a private mapping
    /// (see [`Self::snapshot_directory`]), and return its entries in host
    /// order.
    ///
    /// Linux writes into guest memory only the records a call returns and the
    /// fields it stored of one it could not finish, so the reads do not go to
    /// the guest's buffer, which may be too small
    /// for any entry, unreadable, or partly unmapped, nor to its stack. They
    /// go to an anonymous mapping injected for the purpose and unmapped
    /// before the guest runs again, as the atomic `writev` does for its
    /// iovecs. The mapping is fresh, so the reads leave nothing that the
    /// guest could see in memory. On the kvm backend, whose guest allocator
    /// fills the lowest hole below a cursor first and moves the cursor past
    /// each mapping it makes, the mapping moves that cursor, so a later
    /// mapping that fits no hole below it lands elsewhere than on Linux. That
    /// follows from the guest's own calls, so it is the same on every run.
    ///
    /// If a mapping of [`DIRECTORY_DRAIN_COUNT`] bytes cannot be made, one
    /// page is used, which still holds any entry. If that cannot be made
    /// either (the guest's address space is at its limit), the call fails
    /// with `ENOMEM` before anything is read, and the stream stays without a
    /// snapshot, so a later call tries again. Linux's `getdents` does not
    /// fail so; serving the directory in host order would make the order
    /// depend on the host. This divergence is tracked in
    /// https://github.com/rrnewton/hermit/issues/3723.
    async fn snapshot_directory_privately<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<Option<Vec<DirEntry>>, Error> {
        let mut mapping = None;
        for len in [DIRECTORY_DRAIN_COUNT as usize, DIRECTORY_PAGE] {
            let mapped = guest
                .inject_with_retry(Syscall::Mmap(
                    syscalls::Mmap::new()
                        .with_addr(None)
                        .with_len(len)
                        .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                        .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                        .with_fd(-1)
                        .with_offset(0),
                ))
                .await;
            if let Some(scratch) = mapped
                .ok()
                .and_then(|addr| usize::try_from(addr).ok())
                .and_then(AddrMut::<u8>::from_raw)
            {
                mapping = Some((scratch, len));
                break;
            }
        }
        let Some((scratch, len)) = mapping else {
            return Err(Errno::ENOMEM.into());
        };
        let snapshot = self
            .snapshot_directory(guest, call.into_scratch(scratch, len as u32))
            .await;
        let unmapped = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(Some(Addr::from(scratch).cast()))
                    .with_len(len),
            ))
            .await;
        // A mapping left behind would be visible to the guest.
        unmapped?;
        snapshot
    }

    /// Read the whole host directory by issuing `call.call` until it returns
    /// 0, and return its entries in host order. `call` reads into a private
    /// mapping that holds any entry.
    ///
    /// Every step goes through `record_or_replay`, so a replay reads the same
    /// entries from the log. The reads must start at the beginning of the
    /// directory: a known directory is rewound there, and before the first
    /// snapshot the kernel position must already be 0.
    ///
    /// The first read fails as the guest's call would at any position: the
    /// descriptor is not a directory, or the directory has been removed.
    ///
    /// Returns `None` when this open file must be read in host order instead,
    /// with the kernel position where the guest left it. Either the position
    /// was moved before the first `getdents`, to a host cookie that no entry
    /// index stands for, or a read after the first failed, so the directory
    /// cannot be read to its end. A regular file, pipe or `O_PATH` descriptor
    /// whose position is not 0 also lands here, and reading it in host order
    /// reports the kernel's error without moving its offset.
    async fn snapshot_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: GetdentsCall<'_>,
    ) -> Result<Option<Vec<DirEntry>>, Error> {
        let known_directory = guest
            .thread_state()
            .with_detfd(call.fd, |detfd| detfd.has_directory_stream())?;
        let lseek = |offset, whence| {
            syscalls::Lseek::new()
                .with_fd(call.fd)
                .with_offset(offset)
                .with_whence(whence)
        };
        if known_directory {
            self.record_or_replay(guest, lseek(0, Whence::SEEK_SET))
                .await?;
        } else if self
            .record_or_replay(guest, lseek(0, Whence::SEEK_CUR))
            .await
            != Ok(0)
        {
            return Ok(None);
        }

        let mut entries = Vec::new();
        let mut read_any = false;
        let error: Error = loop {
            let len = match self.record_or_replay(guest, call.call).await {
                Ok(0) => return Ok(Some(entries)),
                Ok(len) => len as usize,
                Err(error) => break error.into(),
            };
            read_any = true;
            match read_records(&guest.memory(), call.buf, len, call.format) {
                Ok(parsed) => entries.extend(parsed),
                Err(error) => break error.into(),
            }
        };
        if !read_any {
            return Err(error);
        }
        // The earlier reads moved the position, which the guest's call must
        // find where the guest left it: at the start.
        self.record_or_replay(guest, lseek(0, Whence::SEEK_SET))
            .await?;
        Ok(None)
    }
}

/// The size of the private mapping that a snapshot reads the host directory
/// into, and so the most bytes one read of it asks for. It holds any entry.
const DIRECTORY_DRAIN_COUNT: u32 = 64 * 1024;

/// The granularity at which the guest's buffer can stop being writable.
const DIRECTORY_PAGE: usize = 4096;

/// The most pages one vectored copy names: Linux's `IOV_MAX`.
const PAGES_PER_COPY: usize = 1024;

/// The pieces of the `len` bytes at guest address `addr`, split where they
/// cross a page: the offset and length of each.
fn guest_pages(addr: usize, len: usize) -> Vec<(usize, usize)> {
    let mut pieces = Vec::with_capacity(len / DIRECTORY_PAGE + 2);
    let mut at = 0;
    while at < len {
        let piece = (DIRECTORY_PAGE - (addr + at) % DIRECTORY_PAGE).min(len - at);
        pieces.push((at, piece));
        at += piece;
    }
    pieces
}

/// Read and decode the `len` bytes of records a `getdents` returned in the
/// guest's buffer. Linux does not write the padding after the last record's
/// name, which can lie in a page the guest cannot read.
fn read_records(
    memory: &impl MemoryAccess,
    buf: AddrMut<u8>,
    len: usize,
    format: DirentFormat,
) -> Result<Vec<DirEntry>, Errno> {
    let mut bytes = vec![0; len];
    let readable = match memory.read_exact(buf, &mut bytes) {
        Ok(()) => len,
        Err(_) => read_guest_prefix(memory, buf, &mut bytes)?,
    };
    format.parse_written(&mut bytes, readable)
}

/// Read the start of the guest's buffer into `bytes`, up to the first page the
/// guest cannot read, and return how many bytes were read. Only a fault
/// (`EFAULT`) ends the copy short; any other error of the copy, such as
/// `ESRCH` for a thread that is gone or `EIO` from the backend, is returned.
///
/// This and [`write_guest_pieces`] use only vectored copies, one page per
/// piece, which respect the guest's page protection. Reverie's `read` and
/// `write` do not always: on the ptrace backend, a copy of at most 8 bytes goes
/// through `PTRACE_PEEKDATA` or `PTRACE_POKEDATA`, which reach a page the guest
/// has made inaccessible.
pub(crate) fn read_guest_prefix(
    memory: &impl MemoryAccess,
    buf: AddrMut<u8>,
    bytes: &mut [u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    for pages in guest_pages(buf.as_raw(), bytes.len()).chunks(PAGES_PER_COPY) {
        let from = pages[0].0;
        let len: usize = pages.iter().map(|&(_, len)| len).sum();
        let remote: Vec<AddrSlice<u8>> = pages
            .iter()
            .map(|&(at, len)| unsafe { AddrSlice::from_raw_parts(buf.add(at).into(), len) })
            .collect();
        let remote: Vec<std::io::IoSlice> = remote
            .iter()
            .map(|piece| unsafe { piece.as_ioslice() })
            .collect();
        let mut local = [std::io::IoSliceMut::new(&mut bytes[from..from + len])];
        let copied = match memory.read_vectored(&remote, &mut local) {
            Err(Errno::EFAULT) => 0,
            copied => copied?,
        };
        done += copied;
        if copied < len {
            break;
        }
    }
    Ok(done)
}

/// Write each of `pieces`, an offset in the guest's buffer and the bytes to
/// store there, within one page, in the order given, up to the first the
/// guest cannot write. Return how many bytes of the pieces were written,
/// counted in that order. As in [`read_guest_prefix`], only a fault ends the
/// copy short, and any other error is returned.
fn write_guest_pieces(
    memory: &mut impl MemoryAccess,
    buf: AddrMut<u8>,
    pieces: &[(usize, &[u8])],
) -> Result<usize, Errno> {
    let mut done = 0;
    for pieces in pieces.chunks(PAGES_PER_COPY) {
        let len: usize = pieces.iter().map(|(_, bytes)| bytes.len()).sum();
        let mut remote: Vec<AddrSliceMut<u8>> = pieces
            .iter()
            .map(|&(at, bytes)| unsafe { AddrSliceMut::from_raw_parts(buf.add(at), bytes.len()) })
            .collect();
        let mut remote: Vec<std::io::IoSliceMut> = remote
            .iter_mut()
            .map(|piece| unsafe { piece.as_ioslice_mut() })
            .collect();
        let local: Vec<std::io::IoSlice> = pieces
            .iter()
            .map(|&(_, bytes)| std::io::IoSlice::new(bytes))
            .collect();
        let copied = match memory.write_vectored(&local, &mut remote) {
            Err(Errno::EFAULT) => 0,
            copied => copied?,
        };
        done += copied;
        if copied < len {
            break;
        }
    }
    Ok(done)
}

/// One store that Linux makes into the guest's buffer to copy directory
/// records: where in the buffer, the bytes stored (a range of the records,
/// or of the first record's placeholder `d_off` after them), and whether the
/// CPU makes it as one store, which writes nothing if any byte of it faults.
struct DirentStore {
    at: usize,
    from: std::ops::Range<usize>,
    single: bool,
}

/// The stores Linux makes, in its order, to copy records whose names are
/// `names` bytes long, and for each record, how many of the stores precede
/// the end of its `filldir`. `placeholder` is where the first record's
/// placeholder `d_off` follows the records.
///
/// For each record, `filldir` (fs/readdir.c) first stores the `d_off` of the
/// record before it, which is its own entry's position. The first record of
/// a call has none before it, so its own `d_off` gets that position, to be
/// overwritten. Then come `d_ino`, `d_reclen`, `d_type`, the NUL after the
/// name, and the name. Once no more records are copied, the call stores the
/// last one's `d_off`. No padding is written.
///
/// Linux copies a name in words from its start, after the NUL that follows
/// it. A name spans at most two pages, and the NUL's page could be written,
/// so if a word of the name faults, the first does, and none of the name is
/// written. So it is here, where the name is written page by page.
fn dirent_stores(
    format: DirentFormat,
    names: &[usize],
    placeholder: usize,
) -> (Vec<DirentStore>, Vec<usize>) {
    let field = |at: usize, len: usize| DirentStore {
        at,
        from: at..at + len,
        single: true,
    };
    let mut stores = Vec::with_capacity(names.len() * 6 + 1);
    let mut filled = Vec::with_capacity(names.len());
    let mut previous = None;
    let mut at = 0;
    for &name_len in names {
        let reclen = format.record_len(name_len);
        let name = at + format.name_offset();
        stores.push(match previous {
            Some(previous) => field(previous + 8, 8),
            None => DirentStore {
                at: at + 8,
                from: placeholder..placeholder + 8,
                single: true,
            },
        });
        stores.push(field(at, 8));
        stores.push(field(at + 16, 2));
        stores.push(field(at + format.type_offset(reclen), 1));
        stores.push(field(name + name_len, 1));
        stores.push(DirentStore {
            at: name,
            from: name..name + name_len,
            single: false,
        });
        filled.push(stores.len());
        previous = Some(at);
        at += reclen;
    }
    if let Some(previous) = previous {
        stores.push(field(previous + 8, 8));
    }
    (stores, filled)
}

/// Copy directory records into the guest's buffer as Linux does: store by
/// store (see [`dirent_stores`]), up to the first store the guest cannot
/// write. `records` holds the records of entries whose names are `names`
/// bytes long, the first at position `start`.
///
/// Return how many entries the call passes, and its result: the bytes of the
/// records whose `filldir` completed. It is `EFAULT` if none did, or if the
/// fault hit a `d_off`, which the call's last store would then write again.
/// A copy that fails with any error other than a fault passes no entry and
/// returns that error, as the call did before directory streams.
///
/// Each store within one page is written whole or not at all, as Linux's
/// are. A single store that crosses into another page, a field of a buffer
/// not aligned to 8 bytes, the CPU writes whole or not at all, so its two
/// parts are written one after the other and the first is put back if the
/// second cannot be written. What is put back is read just before the first
/// part is written, after every store before it, which may have changed those
/// bytes through another mapping of the same memory. The part written first
/// is the one whose bytes Detcore can read to put back: the part in the
/// second page, unless only the part in the first page can be read. Where
/// neither can be read with a vectored copy, the part in the second page is
/// written first, so that a write-only buffer ending at a page the guest
/// cannot touch is left as Linux leaves it, and its bytes are read to put
/// back with Reverie's `read`, which for at most 8 bytes is `PTRACE_PEEKDATA`
/// on the ptrace backend and reads a write-only page. While no other thread
/// writes the buffer, only a store from a page the guest can neither read nor
/// write into a write-only page that this read cannot reach either leaves
/// bytes Linux does not write: up to 7, in the write-only page.
fn copy_records(
    memory: &mut impl MemoryAccess,
    buf: AddrMut<u8>,
    format: DirentFormat,
    records: &[u8],
    names: &[usize],
    start: u64,
) -> (usize, Result<usize, Errno>) {
    if names.is_empty() {
        return (0, Ok(0));
    }
    let mut source = records.to_vec();
    let placeholder = source.len();
    source.extend_from_slice(&i64::try_from(start).unwrap_or(i64::MAX).to_ne_bytes());
    let (stores, filled) = dirent_stores(format, names, placeholder);
    // Each piece, within one page: where in the buffer and its bytes, and,
    // in `owner`, its store.
    let mut pieces: Vec<(usize, &[u8])> = Vec::with_capacity(stores.len());
    let mut owner = Vec::with_capacity(stores.len());
    // For each single store that crosses pages: its part written first, as an
    // index into `pieces`, and whether the guest can read that part's bytes.
    let mut crossing = Vec::new();
    for (index, store) in stores.iter().enumerate() {
        let mut split = guest_pages(buf.as_raw() + store.at, store.from.len());
        if store.single && split.len() > 1 {
            let readable: Result<Vec<bool>, Errno> = split
                .iter()
                .map(|&(at, len)| {
                    let mut bytes = vec![0; len];
                    let addr = unsafe { buf.add(store.at + at) };
                    Ok(read_guest_prefix(memory, addr, &mut bytes)? == len)
                })
                .collect();
            let mut readable = match readable {
                Ok(readable) => readable,
                Err(errno) => return (0, Err(errno)),
            };
            if readable[1] || !readable[0] {
                split.reverse();
                readable.reverse();
            }
            crossing.push((pieces.len(), readable[0]));
        }
        for (at, len) in split {
            let from = store.from.start + at;
            pieces.push((store.at + at, &source[from..from + len]));
            owner.push(index);
        }
    }
    // Write the pieces in order, stopping before the first part of each
    // crossing store to save what the guest has there. The stores before it
    // can have changed those bytes through another mapping of the same memory,
    // so they are read only once those stores are made.
    let mut next = 0;
    let mut saved: Option<(usize, Vec<u8>)> = None;
    let mut failed = None;
    for &(stop, readable) in crossing.iter().chain([(pieces.len(), false)].iter()) {
        let segment = &pieces[next..stop];
        let written = match write_guest_pieces(memory, buf, segment) {
            Ok(written) => written,
            Err(errno) => return (0, Err(errno)),
        };
        let mut end = 0;
        if let Some(at) = segment.iter().position(|(_, bytes)| {
            end += bytes.len();
            end > written
        }) {
            failed = Some(next + at);
            break;
        }
        if stop == pieces.len() {
            break;
        }
        let (at, bytes) = pieces[stop];
        let addr = unsafe { buf.add(at) };
        let mut before = vec![0; bytes.len()];
        let read = if readable {
            match read_guest_prefix(memory, addr, &mut before) {
                Ok(read) => read == before.len(),
                Err(errno) => return (0, Err(errno)),
            }
        } else {
            // The part in the second page, at its start, which no vectored copy
            // can read. Reverie's `read` of at most 8 bytes can on the ptrace
            // backend, through `PTRACE_PEEKDATA`, and a read changes nothing
            // the guest sees.
            memory.read_exact(addr, &mut before).is_ok()
        };
        saved = read.then_some((stop, before));
        next = stop;
    }
    let Some(failed) = failed else {
        return (names.len(), Ok(records.len()));
    };
    let store = owner[failed];
    if failed > 0
        && owner[failed - 1] == store
        && let Some((first, before)) = &saved
        && *first == failed - 1
        && let Err(errno) =
            write_guest_pieces(memory, buf, &[(pieces[*first].0, before.as_slice())])
    {
        return (0, Err(errno));
    }
    // The entry whose `filldir` made the store, or all of them for the call's
    // last store, and the first store of that `filldir`.
    let entry = filled.partition_point(|&end| end <= store);
    let first = entry.checked_sub(1).map_or(0, |before| filled[before]);
    let result = if entry == 0 || store == first {
        Err(Errno::EFAULT)
    } else {
        Ok(names[..entry]
            .iter()
            .map(|&name_len| format.record_len(name_len))
            .sum())
    };
    (entry, result)
}

/// A guest `getdents` or `getdents64` call.
#[derive(Clone, Copy)]
struct GetdentsCall<'a> {
    /// The call as the guest made it.
    call: Syscall,
    /// The same call asking for no bytes.
    empty: Syscall,
    fd: RawFd,
    /// The raw device of the directory's filesystem, which keys an entry's
    /// deterministic inode unless the entry's own `lstat` reports it on
    /// another device (see `Detcore::directory_entry_identity`); resolved
    /// before the real call (see `Detcore::directory_device`).
    device: u64,
    /// Which entries are `lstat`ed for that device at all; the others are
    /// keyed on `device` (see `Detcore::directory_entry_lookup`). On
    /// overlayfs it is settled before the first entry is keyed (see
    /// `Detcore::settle_overlay_entry_lookup`).
    lookup: EntryLookup,
    buf: AddrMut<'a, u8>,
    /// The guest's buffer size.
    capacity: usize,
    format: DirentFormat,
}

impl<'a> GetdentsCall<'a> {
    /// The same call reading at most `count` bytes into `buf` instead.
    fn into_scratch<'b>(self, buf: AddrMut<'b, u8>, count: u32) -> GetdentsCall<'b> {
        let call = match self.call {
            Syscall::Getdents(call) => {
                Syscall::from(call.with_dirent(Some(buf.cast())).with_count(count))
            }
            Syscall::Getdents64(call) => {
                Syscall::from(call.with_dirent(Some(buf.cast())).with_count(count))
            }
            other => other,
        };
        GetdentsCall {
            call,
            empty: self.empty,
            fd: self.fd,
            device: self.device,
            lookup: self.lookup,
            buf,
            capacity: count as usize,
            format: self.format,
        }
    }
}

#[cfg(test)]
mod procfs_wiring_guard {
    //! The procfs snapshot WIRING, guarded where the wiring lives.
    //!
    //! WHY THIS IS A SOURCE-LEVEL GUARD AND NOT A BEHAVIOURAL TEST. The thing
    //! at risk is not `ProcfsFile`'s logic -- that is exercised elsewhere. It is
    //! the CALL from each read handler into the snapshot initialiser. Those
    //! handlers are `async fn`s on the `Tool` trait taking a live `Guest`, so a
    //! unit test cannot invoke one without standing up a traced guest process;
    //! that is exactly why the only thing guarding this today is one heavyweight
    //! integration test that compiles a C probe and runs hermit.
    //!
    //! MEASURED GAP (2026-08-07, hermit 75506005d): deleting the pread64
    //! snapshot-initialisation block leaves ALL 386 detcore lib tests green.
    //! A mechanism whose only proof of life is one fixture is one deletion away
    //! from vanishing unnoticed -- the positioned-read determinism bug this
    //! wiring fixes would silently return.
    //!
    //! These assertions are deliberately narrow: they bind to the CALL, name the
    //! mechanism when they fail, and cost nothing to run. They do not claim to
    //! verify that the snapshot is correct.

    /// The production source only. `include_str!` pulls in THIS module too, so a
    /// naive scan counts the guard's own string literals and reports phantom
    /// duplicates -- it did exactly that on first run. Truncating at the guard's
    /// own header makes every assertion below immune to self-reference.
    fn production_source() -> &'static str {
        const WHOLE: &str = include_str!("files.rs");
        const GUARD: &str = "#[cfg(test)]\nmod procfs_wiring_guard {";
        match WHOLE.find(GUARD) {
            Some(cut) => &WHOLE[..cut],
            None => WHOLE,
        }
    }

    /// The body of `fn <name>` up to the next top-level `    }` at fn indent.
    fn handler_body(name: &str) -> &'static str {
        let start = production_source()
            .find(&format!("fn {}<G: Guest<Self>>", name))
            .unwrap_or_else(|| {
                panic!(
                    "procfs wiring guard: handler `{name}` not found in files.rs.\n\
                     TWO VERY DIFFERENT CAUSES, and the guard cannot tell them apart:\n\
                       (a) the handler was RENAMED or its signature changed -- the \
                     mechanism is fine, update the name in this guard; or\n\
                       (b) the handler was DELETED -- the procfs snapshot wiring is gone.\n\
                     Check which before editing. This guard binds to source text on \
                     purpose: the handlers are async `Tool` methods taking a live Guest, \
                     so nothing cheaper can observe the call. It is deliberately loud \
                     when it cannot see the code, because silently passing is the \
                     failure it exists to prevent."
                )
            });
        let rest = &production_source()[start..];
        let end = rest
            .find(
                "
    }
",
            )
            .map(|e| e + 6)
            .unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn pread64_initializes_the_procfs_snapshot() {
        let body = handler_body("handle_pread64");
        assert!(
            body.contains("procfs_needs_snapshot") && body.contains("initialize_procfs_snapshot"),
            "MISSING MECHANISM: the pread64 handler no longer initialises the procfs \
             snapshot. Positioned reads will fall through to LIVE KERNEL BYTES instead of \
             the sanitized ProcfsFile snapshot, reintroducing the positioned-read \
             nondeterminism that hermit-cli/tests/procfs_positioned_determinism.rs exists \
             to catch. Restore the `procfs_needs_snapshot` -> `initialize_procfs_snapshot` \
             call in handle_pread64."
        );
    }

    #[test]
    fn read_initializes_the_procfs_snapshot() {
        let body = handler_body("handle_read");
        assert!(
            body.contains("procfs_needs_snapshot") && body.contains("initialize_procfs_snapshot"),
            "MISSING MECHANISM: the sequential read handler no longer initialises the \
             procfs snapshot. Reads of /proc will observe live kernel bytes. Restore the \
             `procfs_needs_snapshot` -> `initialize_procfs_snapshot` call in handle_read."
        );
    }

    #[test]
    fn both_read_paths_share_one_snapshot_initializer() {
        // The original defect was exactly this asymmetry: `read` consumed the
        // sanitized snapshot while `pread64` did not. Forking the logic into two
        // initialisers is how that asymmetry comes back.
        let n = production_source()
            .matches("async fn initialize_procfs_snapshot")
            .count();
        assert_eq!(
            n, 1,
            "MISSING MECHANISM: expected exactly ONE `initialize_procfs_snapshot` \
             definition so every read path shares it; found {n}. Two initialisers is how \
             read/pread64 drifted apart in the first place."
        );
        for handler in ["handle_read", "handle_pread64"] {
            assert!(
                handler_body(handler).contains("self.initialize_procfs_snapshot("),
                "MISSING MECHANISM: `{handler}` does not call the shared \
                 initialize_procfs_snapshot."
            );
        }
    }

    /// Both mountinfo captures in the snapshot initializer must drop ephemeral
    /// host seed rows (https://github.com/rrnewton/hermit/pull/3219): the guest
    /// read's capture, for the files that carry mountinfo identities, and the
    /// tracer-side capture that validates fdinfo `mnt_id`. Without either, host
    /// seed churn reaches guest-visible mountinfo or shifts mount-ID assignment,
    /// and no unit test of the filter itself notices.
    #[test]
    fn snapshot_initializer_excludes_host_seed_mounts_at_both_captures() {
        let body = handler_body("initialize_procfs_snapshot");
        let calls = body.matches("exclude_ephemeral_host_seed_mounts(").count();
        assert_eq!(
            calls, 2,
            "MISSING MECHANISM: initialize_procfs_snapshot must call \
             exclude_ephemeral_host_seed_mounts exactly twice (the guest mountinfo \
             capture and the fdinfo mnt_id capture); found {calls}. Without it, \
             unrelated host squashfuse seed mounts make /proc/<pid>/mountinfo and \
             fdinfo mount IDs depend on host timing."
        );
        let guest_capture = body
            .find("exclude_ephemeral_host_seed_mounts(&raw_contents)")
            .expect(
                "MISSING MECHANISM: the guest read's procfs capture (`raw_contents`) is \
                 no longer filtered for host seed mounts",
            );
        let gate = body[..guest_capture]
            .rfind("let contents = if")
            .map(|start| &body[start..guest_capture])
            .unwrap_or_default();
        assert!(
            gate.contains("procfs_needs_mountinfo_identities()"),
            "MISSING MECHANISM: the guest capture's seed filter must apply exactly to \
             the files that carry mountinfo identities (`let contents = if ... \
             procfs_needs_mountinfo_identities()`); found {gate:?}"
        );
        let fdinfo_capture = body
            .find("exclude_ephemeral_host_seed_mounts(&mountinfo_contents)")
            .expect(
                "MISSING MECHANISM: the fdinfo mnt_id capture (`mountinfo_contents`) is \
                 no longer filtered for host seed mounts",
            );
        let parse = body
            .find("parse_mountinfo(&mountinfo_contents)")
            .expect("the fdinfo mnt_id capture no longer parses mountinfo_contents");
        assert!(
            fdinfo_capture < parse,
            "MISSING MECHANISM: the fdinfo mnt_id capture parses mountinfo before \
             dropping host seed rows"
        );
    }

    /// What `maps_identities_are_minted_through_the_tested_loop` requires of
    /// the `initialize_procfs_snapshot` body; an empty result means it passes.
    fn maps_minting_wiring_violations(body: &str) -> Vec<&'static str> {
        const CALL: &str = "crate::procfs::mint_mapping_identities(";
        let mut violations = Vec::new();
        match body.find(CALL) {
            None => violations.push("does not call `crate::procfs::mint_mapping_identities`"),
            Some(start) => {
                // The call's argument list, up to its balancing `)`. The
                // snapshot bytes must be the ones passed, unmodified: a
                // re-sorted copy would feed the tested loop host-ordered input.
                // And the stdio map built above, keyed on the raw device and
                // inode `stat` reports, must be the one passed: an empty map
                // here would drop the fdinfo-consistent stdio inode override
                // and no unit test of the loop itself could notice.
                let args = &body[start + CALL.len()..];
                let mut depth = 0usize;
                let end = args
                    .char_indices()
                    .find_map(|(i, c)| match c {
                        '(' | '[' | '{' => {
                            depth += 1;
                            None
                        }
                        ')' | ']' | '}' if depth == 0 => Some(i),
                        ')' | ']' | '}' => {
                            depth -= 1;
                            None
                        }
                        _ => None,
                    })
                    .unwrap_or(args.len());
                if args[..end].split(',').next().map(str::trim) != Some("&contents") {
                    violations.push("does not pass `&contents` as the snapshot to mint from");
                }
                if !args[..end]
                    .split(',')
                    .any(|arg| arg.trim() == "&stdio_by_raw_file")
                {
                    violations.push("does not pass `&stdio_by_raw_file` to the minting loop");
                }
            }
        }
        // Parsing mapping headers or lines, collecting them, or resolving
        // their recorded or `stat` identities here would mean the order in
        // which this body resolves and mints, and the refusal of an ambiguous
        // line, are no longer what the unit tests drive. `mapping_lines` also
        // matches `mapping_lines_in_text_order`; `mapping_keys` is the
        // header-keyed loop's old name.
        if [
            "mapping_header_identity",
            "mapping_header_key",
            "mapping_keys",
            "mapping_lines",
            "parse_mapping_line",
            "mapping_recorded_identity",
            "mapping_stat_identity",
        ]
        .iter()
        .any(|local_parse| body.contains(local_parse))
        {
            violations.push("parses mapping identities itself instead of delegating");
        }
        violations
    }

    #[test]
    fn maps_identities_are_minted_through_the_tested_loop() {
        // The mint ORDER is tested behaviourally, against the production loop
        // itself, by `tool_global::tests::maps_*_are_minted_in_text_order_not_raw_order`,
        // and `maps_keys_are_minted_in_text_order_under_device_keying`, which
        // drive `crate::procfs::mint_mapping_identities`. What those tests
        // cannot see is whether the snapshot initialiser still calls that loop.
        // This checks only that: the body calls it with `&contents` and
        // `&stdio_by_raw_file`, and does not parse mapping headers or resolve
        // their identities on its own. It does not check the order.
        let body = handler_body("initialize_procfs_snapshot");
        assert!(
            body.len() > 200 && body.contains("needs_mapping_identities"),
            "guard extractor did not find a real body for `initialize_procfs_snapshot` \
             (len {}), so the wiring assertion would be vacuous",
            body.len()
        );
        let violations = maps_minting_wiring_violations(body);
        assert!(
            violations.is_empty(),
            "MISSING MECHANISM: `initialize_procfs_snapshot` {violations:?}. /proc/*/maps \
             identities must be minted by `crate::procfs::mint_mapping_identities`, whose \
             text-order minting is what the tool_global unit tests exercise; a local loop \
             could mint in host raw-number order without any test noticing."
        );
    }

    #[test]
    fn maps_minting_wiring_check_rejects_a_local_loop() {
        // Positive controls: bodies that bypass the tested loop must be rejected,
        // otherwise the guard above would pass vacuously. Each control asserts
        // the EXACT violation list, and all but `local_loop` violate one rule
        // only, so disabling any single check makes its own control fail.
        const NO_CALL: &str = "does not call `crate::procfs::mint_mapping_identities`";
        const NOT_CONTENTS: &str = "does not pass `&contents` as the snapshot to mint from";
        const NO_STDIO: &str = "does not pass `&stdio_by_raw_file` to the minting loop";
        const LOCAL_PARSE: &str = "parses mapping identities itself instead of delegating";

        let no_call = "let mapping_identities = BTreeMap::new();";
        assert_eq!(maps_minting_wiring_violations(no_call), [NO_CALL]);
        let local_loop = "let raw_pairs: BTreeSet<(u64, u64)> = String::from_utf8_lossy(&contents)\n\
                          .lines().filter_map(crate::procfs::mapping_header_identity).collect();";
        assert_eq!(
            maps_minting_wiring_violations(local_loop),
            [NO_CALL, LOCAL_PARSE]
        );
        // The keying commit's first draft: a local loop over the keys in
        // sorted order, resolving each one itself.
        let local_order = "for (key, starts) in crate::procfs::mapping_keys(&contents) {\n\
                           let raw_file = self.mapping_stat_identity(guest, &key, subject).await?;";
        assert_eq!(
            maps_minting_wiring_violations(local_order),
            [NO_CALL, LOCAL_PARSE]
        );
        // Each forbidden name alone, next to an otherwise valid call, so
        // dropping any one of them from the check fails its own control.
        let valid_call = "crate::procfs::mint_mapping_identities(\n\
                          &contents,\n\
                          &stdio_by_raw_file,\n\
                          &mut GuestMappingMinter::new(self, guest, subject),\n\
                          )";
        for local_parse in [
            "let keys = crate::procfs::mapping_keys_in_text_order(&contents);",
            "let lines = crate::procfs::mapping_lines_in_text_order(&contents);",
            "let line = crate::procfs::parse_mapping_line(line);",
            "let key = crate::procfs::mapping_header_key(line);",
            "let pair = crate::procfs::mapping_header_identity(line);",
            "let record = self.mapping_recorded_identity(guest, &key, start, subject);",
            "let raw_file = self.mapping_stat_identity(guest, &key, subject).await?;",
        ] {
            assert_eq!(
                maps_minting_wiring_violations(&format!("{local_parse}\n{valid_call}")),
                [LOCAL_PARSE],
                "{local_parse}"
            );
        }
        let other_buffer = "crate::procfs::mint_mapping_identities(\n\
                            &resorted,\n\
                            &stdio_by_raw_file,\n\
                            &mut GuestMappingMinter::new(self, guest, subject),\n\
                            )";
        assert_eq!(maps_minting_wiring_violations(other_buffer), [NOT_CONTENTS]);
        let no_stdio = "crate::procfs::mint_mapping_identities(\n\
                        &contents,\n\
                        &BTreeMap::new(),\n\
                        &mut GuestMappingMinter::new(self, guest, subject),\n\
                        )";
        assert_eq!(maps_minting_wiring_violations(no_stdio), [NO_STDIO]);
        // A stdio map keyed on the raw inode alone, as before
        // https://github.com/rrnewton/hermit/issues/3307 was fixed.
        let inode_keyed_stdio = "crate::procfs::mint_mapping_identities(\n\
                                 &contents,\n\
                                 &stdio_by_raw_inode,\n\
                                 &mut GuestMappingMinter::new(self, guest, subject),\n\
                                 )";
        assert_eq!(
            maps_minting_wiring_violations(inode_keyed_stdio),
            [NO_STDIO]
        );
        assert!(maps_minting_wiring_violations(valid_call).is_empty());
    }

    #[test]
    fn the_guard_can_actually_see_the_handlers() {
        // Positive control: if the extractor silently returned empty bodies the
        // three assertions above would be vacuous rather than protective.
        for handler in ["handle_read", "handle_pread64"] {
            let body = handler_body(handler);
            assert!(
                body.len() > 200 && body.contains("call.fd()"),
                "guard extractor did not find a real body for `{handler}` \
                 (len {}), so the wiring assertions would be vacuous",
                body.len()
            );
        }
    }
}

#[cfg(test)]
mod test {
    use nix::fcntl::OFlag;
    use reverie::syscalls::FromToRaw;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Whence;

    use super::DETERMINISTIC_PIPE_CAPACITY_BYTES;
    use super::fcntl_host_timed_signals;
    use super::ioctl_host_timed_signals;
    use super::may_have_acquired_controlling_terminal;
    use super::open_can_acquire_controlling_terminal;
    use super::opened_controlling_terminal;
    use super::pipe_capacity_request_exceeds_ceiling;
    use super::terminal_can_become_controlling;
    use crate::test_pages::PAGE;
    use crate::test_pages::Pages;

    /// Every `fcntl` and `ioctl` that can make Linux signal a descriptor's
    /// owner at a moment set by host timing names SIGIO and SIGURG, plus the
    /// signal `F_SETSIG` chooses; every other command names nothing
    /// (https://github.com/rrnewton/hermit/issues/3146).
    #[test]
    fn async_io_arming_calls_name_their_host_timed_signals() {
        use reverie::syscalls::FcntlCmd;
        use reverie::syscalls::ioctl::Request;
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let async_io = bit(libc::SIGIO) | bit(libc::SIGURG);
        for (cmd, expected) in [
            (
                FcntlCmd::F_SETFL(libc::O_ASYNC | libc::O_NONBLOCK),
                async_io,
            ),
            (FcntlCmd::F_SETFL(libc::O_NONBLOCK), 0),
            (FcntlCmd::F_SETOWN, async_io),
            (FcntlCmd::F_SETOWN_EX(None), async_io),
            (FcntlCmd::F_SETLEASE(libc::F_RDLCK), async_io),
            // DN_MODIFY, which the libc crate does not export.
            (FcntlCmd::F_NOTIFY(0x2), async_io),
            (
                FcntlCmd::F_SETSIG(libc::SIGUSR1),
                async_io | bit(libc::SIGUSR1),
            ),
            (FcntlCmd::F_SETSIG(0), async_io),
            (FcntlCmd::F_GETFL, 0),
            (FcntlCmd::F_SETFD(libc::FD_CLOEXEC), 0),
        ] {
            assert_eq!(fcntl_host_timed_signals(cmd), expected, "{cmd:?}");
        }
        for (request, expected) in [
            (Request::FIOASYNC(None), async_io),
            (Request::FIOSETOWN(None), async_io),
            (Request::SIOCSPGRP(None), async_io),
            (Request::FIOCLEX, 0),
        ] {
            assert_eq!(ioctl_host_timed_signals(request), expected, "{request:?}");
        }
    }

    /// The calls that give a session its controlling terminal (TIOCSCTTY, and
    /// TIOCGPTPEER without O_NOCTTY, which opens a pseudoterminal's other end
    /// as `open` does) or choose the terminal's foreground process group
    /// (TIOCSPGRP) name every signal Linux sends because of that terminal, at
    /// a moment set by host timing: SIGHUP and SIGCONT when it hangs up or the
    /// leader exits, SIGINT, SIGQUIT and SIGTSTP for its interrupt, quit and
    /// suspend characters, SIGWINCH when its size changes, and SIGTTIN and
    /// SIGTTOU when a background process group reads or writes it. Reading the
    /// foreground group (TIOCGPGRP), giving up the controlling terminal
    /// (TIOCNOTTY), opening the other end with O_NOCTTY, and setting the size
    /// (TIOCSWINSZ, whose SIGWINCH is sent inside the caller's call) name
    /// nothing (round-9 High 2 and the round-11 finding "Terminal resize
    /// remains an admitted host-timed interruption" of
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[test]
    fn terminal_control_calls_name_the_terminal_signals() {
        use reverie::syscalls::ioctl::Request;
        let bit = |signal: i32| 1_u64 << (signal - 1);
        let terminal = [
            libc::SIGHUP,
            libc::SIGCONT,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
            libc::SIGWINCH,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ]
        .into_iter()
        .fold(0, |set, signal| set | bit(signal));
        for (request, expected) in [
            (Request::TIOCSCTTY(0), terminal),
            (Request::TIOCSCTTY(1), terminal),
            (Request::TIOCSPGRP(None), terminal),
            (Request::TIOCGPTPEER(libc::O_RDWR), terminal),
            (
                Request::TIOCGPTPEER(libc::O_RDWR | libc::O_CLOEXEC),
                terminal,
            ),
            (Request::TIOCGPTPEER(libc::O_RDWR | libc::O_NOCTTY), 0),
            (Request::TIOCGPGRP(None), 0),
            (Request::TIOCNOTTY, 0),
            (Request::TIOCSWINSZ(None), 0),
        ] {
            assert_eq!(ioctl_host_timed_signals(request), expected, "{request:?}");
        }
    }

    /// An `open` can give its caller a controlling terminal only without
    /// O_NOCTTY and O_PATH, only in a session leader, and only of a terminal
    /// Linux can make controlling. None of the inputs is the controlling
    /// terminal procfs lists, which a hangup clears
    /// (https://github.com/rrnewton/hermit/issues/3146,
    /// https://github.com/rrnewton/hermit/pull/3361).
    #[test]
    fn an_open_acquires_a_controlling_terminal_only_as_a_session_leader() {
        assert!(open_can_acquire_controlling_terminal(OFlag::O_RDWR));
        assert!(!open_can_acquire_controlling_terminal(
            OFlag::O_RDWR | OFlag::O_NOCTTY
        ));
        assert!(!open_can_acquire_controlling_terminal(OFlag::O_PATH));
        // A host's /proc/tty/drivers, plus a driver whose name has a space.
        let table = Some(
            "/dev/tty             /dev/tty        5       0 system:/dev/tty\n\
             /dev/console         /dev/console    5       1 system:console\n\
             /dev/ptmx            /dev/ptmx       5       2 system\n\
             /dev/vc/0            /dev/vc/0       4       0 system:vtmaster\n\
             serial_8250          /dev/ttyS       4 64-95 serial\n\
             acm serial           /dev/ttyACM   166 0-255 serial\n\
             pty_slave            /dev/pts      136 0-1048575 pty:slave\n\
             pty_master           /dev/ptm      128 0-1048575 pty:master\n\
             unknown              /dev/tty        4 1-63 console\n",
        );
        // /dev/pts/3 is character device 136:3; minor 300 sets bits above 0xff.
        let pts3 = libc::makedev(136, 3);
        let pts300 = libc::makedev(136, 300);
        assert!(may_have_acquired_controlling_terminal(42, 42, pts3, table));
        assert!(may_have_acquired_controlling_terminal(
            42, 42, pts300, table
        ));
        assert!(!may_have_acquired_controlling_terminal(43, 42, pts3, table));
        // Serial lines, virtual consoles and /dev/tty can become controlling;
        // /dev/console, /dev/ptmx, /dev/vc/0, pseudoterminal masters, minors
        // outside every range and devices that are not terminals cannot.
        for (major, minor) in [(4, 64), (4, 95), (166, 7), (4, 1), (4, 63), (5, 0)] {
            let rdev = libc::makedev(major, minor);
            assert!(
                terminal_can_become_controlling(rdev, table),
                "{major}:{minor}"
            );
        }
        for (major, minor) in [(5, 1), (5, 2), (4, 0), (128, 3), (4, 96), (188, 0), (1, 3)] {
            let rdev = libc::makedev(major, minor);
            assert!(
                !terminal_can_become_controlling(rdev, table),
                "{major}:{minor}"
            );
        }
        // A table that cannot be read answers true, except for the devices
        // Linux never makes controlling. A line that does not parse answers
        // true, unless another line names the device.
        let null = libc::makedev(1, 3);
        assert!(terminal_can_become_controlling(pts3, None));
        assert!(terminal_can_become_controlling(null, None));
        assert!(!terminal_can_become_controlling(libc::makedev(5, 1), None));
        let garbled = Some("pty_slave /dev/pts 136 0-1048575 pty:slave\nnot a driver line\n");
        assert!(terminal_can_become_controlling(null, garbled));
        assert!(terminal_can_become_controlling(pts3, garbled));
        let master = Some("pty_master /dev/ptm 128 0-1048575 pty:master\nnot a driver line\n");
        assert!(!terminal_can_become_controlling(
            libc::makedev(128, 3),
            master
        ));
    }

    /// A session leader that opens a pseudoterminal's slave without O_NOCTTY
    /// gains it as its controlling terminal. Closing the master then hangs the
    /// slave up, which clears the controlling terminal procfs lists but changes
    /// neither the session nor the descriptor's device. A hangup that lands
    /// after the `open`, whether before or after the descriptor's stat, must
    /// not hide that the `open` may have acquired the terminal
    /// (https://github.com/rrnewton/hermit/pull/3361).
    #[test]
    fn a_hangup_after_the_open_does_not_hide_the_acquired_terminal() {
        use std::time::Duration;
        use std::time::Instant;

        struct Child(libc::pid_t);
        impl Drop for Child {
            fn drop(&mut self) {
                // SAFETY: the pid is this test's own child, not yet reaped.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }

        // SAFETY (every block below): libc calls on descriptors, buffers and
        // a child process this test owns.
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        let master = unsafe { libc::posix_openpt(flags) };
        assert!(
            master >= 0,
            "posix_openpt: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(unsafe { libc::grantpt(master) }, 0);
        assert_eq!(unsafe { libc::unlockpt(master) }, 0);
        let mut slave_path = [0 as libc::c_char; 64];
        let path_len = slave_path.len();
        assert_eq!(
            unsafe { libc::ptsname_r(master, slave_path.as_mut_ptr(), path_len) },
            0
        );
        // `ready` carries the slave's descriptor number to this process;
        // nothing writes `release`, so this process's exit ends the child.
        let (mut ready, mut release) = ([0; 2], [0; 2]);
        assert_eq!(
            unsafe { libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        assert_eq!(
            unsafe { libc::pipe2(release.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // The child makes only async-signal-safe calls and allocates
            // nothing, so forking from a test harness thread is safe.
            unsafe {
                libc::alarm(60);
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::close(master);
                libc::close(ready[0]);
                libc::close(release[1]);
                if libc::setsid() < 0 {
                    libc::_exit(2);
                }
                let slave = libc::open(slave_path.as_ptr(), libc::O_RDWR);
                if slave < 0 {
                    libc::_exit(3);
                }
                let bytes = slave.to_ne_bytes();
                if libc::write(ready[1], bytes.as_ptr().cast(), bytes.len()) != 4 {
                    libc::_exit(4);
                }
                let mut byte = [0_u8; 1];
                libc::read(release[0], byte.as_mut_ptr().cast(), 1);
                libc::_exit(0);
            }
        }
        let child = Child(pid);
        unsafe {
            libc::close(ready[1]);
            libc::close(release[0]);
        }
        let mut bytes = [0_u8; 4];
        let got = unsafe { libc::read(ready[0], bytes.as_mut_ptr().cast(), bytes.len()) };
        assert_eq!(got, 4, "the child did not open the slave");
        let slave = i32::from_ne_bytes(bytes);
        // The descriptor's stat, read before the hangup, as `handle_openat`
        // reads it before it classifies the open.
        let link = std::ffi::CString::new(format!("/proc/{pid}/fd/{slave}")).unwrap();
        let mut host_stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::stat(link.as_ptr(), &mut host_stat) }, 0);
        let terminal = (
            libc::major(host_stat.st_rdev) as i32,
            libc::minor(host_stat.st_rdev) as i32,
        );
        let process = procfs::process::Process::new(pid).unwrap();
        let before = process.stat().unwrap();
        assert_eq!(
            (before.session, before.tty_nr()),
            (pid, terminal),
            "the child's open did not acquire the terminal"
        );
        assert!(opened_controlling_terminal(pid, slave, Some(&host_stat)));
        // Hang the slave up: Linux clears the session's controlling terminal.
        assert_eq!(unsafe { libc::close(master) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while process.stat().unwrap().tty_nr() != (0, 0) {
            assert!(
                Instant::now() < deadline,
                "the hangup did not clear the controlling terminal"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(process.stat().unwrap().session, pid);
        assert!(
            opened_controlling_terminal(pid, slave, Some(&host_stat)),
            "a hangup after the descriptor's stat hid the acquisition"
        );
        assert!(
            opened_controlling_terminal(pid, slave, None),
            "a hangup before the descriptor's stat hid the acquisition"
        );
        drop(child);
    }

    /// The ceiling is inclusive. A guest that reads the advertised
    /// `pipe-max-size` and asks for exactly that must be allowed to have it;
    /// refusing at the boundary would advertise a size that cannot be set.
    #[test]
    fn the_pipe_ceiling_admits_exactly_the_pinned_capacity() {
        assert!(!pipe_capacity_request_exceeds_ceiling(
            DETERMINISTIC_PIPE_CAPACITY_BYTES
        ));
        assert!(!pipe_capacity_request_exceeds_ceiling(
            DETERMINISTIC_PIPE_CAPACITY_BYTES - 1
        ));
        assert!(pipe_capacity_request_exceeds_ceiling(
            DETERMINISTIC_PIPE_CAPACITY_BYTES + 1
        ));
    }

    /// Shrinking stays legal. `tests/c/pipe_capacity.c`
    /// shrinks to one page and requires the value to round-trip, so a blanket
    /// clamp to the pinned capacity would break a guest-visible contract this
    /// repository already locked.
    #[test]
    fn shrinking_is_never_refused_by_the_ceiling() {
        for requested in [1, 4096, DETERMINISTIC_PIPE_CAPACITY_BYTES / 2] {
            assert!(
                !pipe_capacity_request_exceeds_ceiling(requested),
                "shrink to {requested} must remain permitted"
            );
        }
    }

    /// The host's own ceiling is the value this change exists to stop
    /// consulting: 1048576 on a default host, 65536 on a hardened one. Both are
    /// refused now, so the guest-visible answer no longer depends on which host
    /// it is.
    #[test]
    fn host_ceilings_are_refused_identically_on_any_host() {
        for host_ceiling in [65536, 1048576] {
            assert!(
                pipe_capacity_request_exceeds_ceiling(host_ceiling),
                "{host_ceiling} must be refused regardless of the host sysctl"
            );
        }
    }

    use super::Errno;
    use super::TimerSlackBinding;
    use super::UNIX_AUTOBIND_NAME_LEN;
    use super::canonicalize_tcp_info;
    use super::classify_timer_slack_binding;
    use super::is_inherited_container_output;
    use super::parse_timer_slack_write;
    use super::pipe_capacity_failure;
    use super::random_device_lseek_result;
    use super::should_tag_host_timed_internal_pipe_io;
    use super::unix_autobind_address;
    use super::unix_autobind_addrlen;
    use super::vectored_offset;
    use crate::fd::FdType;
    use crate::resources::Device;
    use crate::resources::ResourceID;

    /// This is an assumption we're making about flags.  Probably these flags can never be
    /// changed, but let's check just in case.
    #[test]
    fn linux_flags_assumptions() {
        assert_eq!(libc::SOCK_NONBLOCK, OFlag::O_NONBLOCK.bits());
        assert_eq!(libc::SOCK_CLOEXEC, OFlag::O_CLOEXEC.bits());
    }

    #[test]
    fn pipe_capacity_failure_classifies_the_errno() {
        let created = [17, 18];

        // The only success shape: Linux applied EXACTLY the capacity we asked for.
        assert_eq!(
            pipe_capacity_failure(created, Ok(i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES))),
            None
        );

        // Successful-but-wrong capacity is a pipe whose size we did not choose. That is not a
        // kernel errno, so it is reported as EIO rather than dressed up as one.
        let mismatch = pipe_capacity_failure(
            created,
            Ok(i64::from(DETERMINISTIC_PIPE_CAPACITY_BYTES) * 2),
        )
        .expect("a capacity Linux rounded away from the pin must not read as success");
        assert_eq!(mismatch.created_fds, created);
        assert_eq!(mismatch.error, Errno::EIO);

        // A real kernel errno is preserved rather than rewritten.
        let denied = pipe_capacity_failure(created, Err(Errno::EPERM))
            .expect("a kernel refusal must not read as success");
        assert_eq!(denied.created_fds, created);
        assert_eq!(denied.error, Errno::EPERM);

        // The descriptors are carried through every failure shape, because they are what the
        // caller has to close; losing them here is how they would leak.
        assert_eq!(denied.created_fds, created);
    }

    #[test]
    fn pipe_capacity_failure_closes_both_created_descriptors() {
        let mut created = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(created.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );

        let pin_result = unsafe { libc::fcntl(created[0], libc::F_SETPIPE_SZ, -1) };
        assert_eq!(pin_result, -1);
        let pin_error = Errno::last();
        // Pin the errno, not just the failure. Without this the test asserts only that
        // `fcntl` returned -1, so it would still pass if the call failed for a reason we
        // did not engineer -- an invalid `created[0]` fails EBADF, every assertion below
        // still holds, and the capacity-pin path is never exercised at all.
        //
        // EINVAL is structural here, not a property of this host. `fcntl`'s argument is an
        // `unsigned long`, so -1 arrives as ULONG_MAX; `round_pipe_size` returns 0 for any
        // size above 2^31, and `pipe_set_size` maps that 0 to -EINVAL BEFORE it consults
        // `CAP_SYS_RESOURCE`. So the result does not depend on privilege or on
        // `/proc/sys/fs/pipe-max-size`.
        assert_eq!(pin_error, Errno::EINVAL);

        let failure = pipe_capacity_failure(created, Err(pin_error))
            .expect("the forced capacity-pin failure must enter the cleanup path");
        for close in failure.close_syscalls() {
            assert_eq!(unsafe { libc::close(close.fd()) }, 0);
        }

        for fd in created {
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
            assert_eq!(Errno::last(), Errno::EBADF);
        }
    }

    #[test]
    fn sabre_pipe_marker_requires_nonblockize_retry_semantics() {
        assert!(should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Pipe,
            true,
            false
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Pipe,
            true,
            true
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Pipe,
            false,
            false
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            false,
            FdType::Pipe,
            true,
            false
        ));
        assert!(!should_tag_host_timed_internal_pipe_io(
            true,
            FdType::Regular,
            true,
            false
        ));
    }

    #[test]
    fn random_device_lseek_matches_linux_noop_llseek() {
        for whence in [
            Whence::SEEK_SET,
            Whence::SEEK_CUR,
            Whence::SEEK_END,
            Whence::SEEK_DATA,
            Whence::SEEK_HOLE,
        ] {
            for status_flags in [
                OFlag::empty().bits(),
                OFlag::O_WRONLY.bits(),
                OFlag::O_RDWR.bits(),
            ] {
                assert_eq!(random_device_lseek_result(status_flags, whence), Ok(0));
            }
            assert_eq!(
                random_device_lseek_result(OFlag::O_PATH.bits(), whence),
                Err(Errno::EBADF)
            );
        }
        assert_eq!(
            random_device_lseek_result(OFlag::empty().bits(), Whence::from_raw(99)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            random_device_lseek_result(OFlag::O_PATH.bits(), Whence::from_raw(99)),
            Err(Errno::EBADF)
        );
    }

    #[test]
    fn timer_slack_write_parser_matches_decimal_procfs_contract() {
        assert_eq!(parse_timer_slack_write(b"0"), Ok(0));
        assert_eq!(parse_timer_slack_write(b"+123\n"), Ok(123));
        assert_eq!(parse_timer_slack_write(b"456\0ignored"), Ok(456));
        assert_eq!(
            parse_timer_slack_write(u64::MAX.to_string().as_bytes()),
            Ok(u64::MAX)
        );

        for invalid in [b"".as_slice(), b"+", b"-1", b" 1", b"1 ", b"1\n2", b"0x10"] {
            assert_eq!(parse_timer_slack_write(invalid), Err(Errno::EINVAL));
        }
        assert_eq!(
            parse_timer_slack_write(b"18446744073709551616"),
            Err(Errno::ERANGE)
        );
    }

    #[test]
    fn timer_slack_vectored_offset_preserves_minus_one_sentinel() {
        assert_eq!(vectored_offset(u64::MAX, u64::MAX), -1);
        assert_eq!(vectored_offset(0, 0), 0);
        assert_eq!(vectored_offset(7, 0), 7);
    }

    #[test]
    fn timer_slack_binding_rejects_exit_reuse_and_other_tasks() {
        let binding = TimerSlackBinding {
            target: 202,
            device: 11,
            inode: 22,
        };
        assert_eq!(
            classify_timer_slack_binding(binding, Some((11, 22)), 202),
            Ok(())
        );
        assert_eq!(
            classify_timer_slack_binding(binding, Some((11, 22)), 303),
            Err(Errno::EPERM)
        );
        assert_eq!(
            classify_timer_slack_binding(binding, None, 202),
            Err(Errno::ESRCH)
        );
        assert_eq!(
            classify_timer_slack_binding(binding, Some((11, 23)), 202),
            Err(Errno::ESRCH),
            "a recycled numeric TID must not revive an old proc inode"
        );
    }

    #[test]
    fn only_inherited_container_output_is_nonseekable() {
        assert!(is_inherited_container_output(Some(ResourceID::Device(
            Device::ContainerStdout
        ))));
        assert!(is_inherited_container_output(Some(ResourceID::Device(
            Device::ContainerStderr
        ))));
        assert!(!is_inherited_container_output(Some(ResourceID::Device(
            Device::ContainerStdin
        ))));
        assert!(!is_inherited_container_output(None));
    }

    #[test]
    fn unix_autobind_address_matches_linux_shape() {
        let address = unix_autobind_address(0x2af);
        assert_eq!(address.sun_family, libc::AF_UNIX as libc::sa_family_t);
        assert_eq!(address.sun_path[0], 0);
        let name = address.sun_path[1..UNIX_AUTOBIND_NAME_LEN]
            .iter()
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>();
        assert_eq!(name, b"002af");
        assert_eq!(
            unix_autobind_addrlen() as usize,
            std::mem::offset_of!(libc::sockaddr_un, sun_path) + UNIX_AUTOBIND_NAME_LEN
        );
    }

    #[test]
    fn tcp_info_retains_only_logical_connection_header() {
        let mut info = [0xff; 16];
        canonicalize_tcp_info(&mut info);

        for (offset, byte) in info.into_iter().enumerate() {
            let expected = if matches!(offset, 0 | 1 | 5 | 6) {
                0xff
            } else {
                0
            };
            assert_eq!(byte, expected, "unexpected byte at offset {offset}");
        }

        for len in 0..8 {
            canonicalize_tcp_info(&mut [0xff; 8][..len]);
        }
    }

    /// Records with the names `names`, as `serve_next_batch` encodes them
    /// from position 0, and the length of each name.
    fn directory_records(format: super::DirentFormat, names: &[&[u8]]) -> (Vec<u8>, Vec<usize>) {
        let mut records = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let entry = super::DirEntry {
                name: name.to_vec(),
                ino: 0x1111_1111_1111_1111,
                off: 0,
                ty: 4,
            };
            format.encode(&entry, entry.ino, index as i64 + 1, &mut records);
        }
        (records, names.iter().map(|name| name.len()).collect())
    }

    /// `records` as Linux leaves them in a buffer that held `Pages::FILL`:
    /// the padding after each name is not written.
    fn as_linux_writes(format: super::DirentFormat, records: &[u8], names: &[usize]) -> Vec<u8> {
        let mut bytes = records.to_vec();
        let mut at = 0;
        for &name_len in names {
            let reclen = format.record_len(name_len);
            let padding = at + format.name_offset() + name_len + 1;
            let end = match format {
                super::DirentFormat::Dirent64 => at + reclen,
                super::DirentFormat::Legacy => at + reclen - 1,
            };
            bytes[padding..end].fill(Pages::FILL);
            at += reclen;
        }
        bytes
    }

    const FORMATS: [super::DirentFormat; 2] =
        [super::DirentFormat::Dirent64, super::DirentFormat::Legacy];

    const TEN: [&[u8]; 10] = [b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h", b"i", b"j"];

    /// A buffer starting 16 bytes before a writable page, in a page the guest
    /// cannot write: Linux's first store, to the first record's `d_off`,
    /// fails, so the call fails and the writable page is left as it was.
    #[test]
    fn a_first_record_that_cannot_be_written_leaves_the_next_page_untouched() {
        for protection in [libc::PROT_NONE, libc::PROT_READ] {
            for format in FORMATS {
                let pages = Pages::new(&[protection, libc::PROT_READ | libc::PROT_WRITE]);
                let (records, names) = directory_records(format, &TEN);
                let copied = super::copy_records(
                    &mut LocalMemory::new(),
                    pages.address(PAGE - 16),
                    format,
                    &records,
                    &names,
                    0,
                );
                assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?} {protection}");
                assert!(
                    pages.contents().iter().all(|&byte| byte == Pages::FILL),
                    "{format:?} after a page with protection {protection}: bytes changed"
                );
            }
        }
    }

    /// Guest memory whose every vectored copy fails with one error that is
    /// not a fault, as when the guest's thread is gone or the backend fails.
    struct FailingMemory(Errno);

    impl reverie::syscalls::MemoryAccess for FailingMemory {
        fn read_vectored(
            &self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            Err(self.0)
        }

        fn write_vectored(
            &mut self,
            _: &[std::io::IoSlice],
            _: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            Err(self.0)
        }
    }

    /// Only a fault ends a copy of directory records short. Any other error
    /// of the copy is the call's error, as it was before directory streams,
    /// and not `EFAULT`: reading the records back, copying them out (through
    /// a buffer whose first store crosses a page, and through one where none
    /// does), and reading the prefix the `getdents64` observer hashes.
    #[test]
    fn a_copy_error_other_than_a_fault_is_returned() {
        let buf = |offset| reverie::syscalls::AddrMut::<u8>::from_raw(16 * PAGE + offset).unwrap();
        for errno in [Errno::ESRCH, Errno::EIO] {
            let mut bytes = [0; 32];
            assert_eq!(
                super::read_guest_prefix(&FailingMemory(errno), buf(0), &mut bytes),
                Err(errno)
            );
            for format in FORMATS {
                let (records, names) = directory_records(format, &TEN);
                for offset in [0, PAGE - 4] {
                    let copied = super::copy_records(
                        &mut FailingMemory(errno),
                        buf(offset),
                        format,
                        &records,
                        &names,
                        0,
                    );
                    assert_eq!(copied, (0, Err(errno)), "{format:?} at {offset}");
                }
                let read =
                    super::read_records(&FailingMemory(errno), buf(0), records.len(), format);
                assert_eq!(read.err(), Some(errno), "{format:?}");
            }
        }
    }

    /// A vectored copy that fails with `EFAULT`, which `process_vm_readv` and
    /// the KVM backend both return when the first byte is out of the guest's
    /// reach, copied nothing rather than failing: the caller decides what an
    /// empty copy means, as it does for a short one.
    #[test]
    fn a_fault_of_the_whole_copy_copies_nothing() {
        let buf = reverie::syscalls::AddrMut::<u8>::from_raw(16 * PAGE).unwrap();
        let mut bytes = [0; 32];
        assert_eq!(
            super::read_guest_prefix(&FailingMemory(Errno::EFAULT), buf, &mut bytes),
            Ok(0)
        );
        assert_eq!(
            super::write_guest_pieces(&mut FailingMemory(Errno::EFAULT), buf, &[(0, &bytes)]),
            Ok(0)
        );
    }

    /// A buffer starting 8 bytes before a writable page, in a page the guest
    /// cannot write. Linux's first store, the first record's position into
    /// its own `d_off`, lands in the writable page; its next, `d_ino`, fails.
    /// The call fails and leaves the position there.
    #[test]
    fn a_first_record_leaves_its_position_in_its_d_off() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ, libc::PROT_READ | libc::PROT_WRITE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 8),
                format,
                &records,
                &names,
                7,
            );
            assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?}");
            let contents = pages.contents();
            assert_eq!(&contents[PAGE..PAGE + 8], &7i64.to_ne_bytes(), "{format:?}");
            assert!(
                contents[..PAGE]
                    .iter()
                    .chain(&contents[PAGE + 8..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes other than the first d_off changed"
            );
        }
    }

    /// A buffer starting 12 bytes before a writable page, in a page the guest
    /// can read but not write. The first record's `d_off` crosses into the
    /// writable page, and the CPU stores it at once, so Linux writes none of
    /// it: the part written in the writable page is put back.
    #[test]
    fn a_store_crossing_out_of_an_unwritable_page_writes_nothing() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ, libc::PROT_READ | libc::PROT_WRITE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 12),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?}");
            assert!(
                pages.contents().iter().all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes changed"
            );
        }
    }

    /// The same store between two pages one of which the guest cannot write,
    /// for each other pair Detcore can leave as Linux does: one page it can
    /// read, or a write-only page before one it cannot touch. Whichever part
    /// is written first must be one Detcore can put back.
    #[test]
    fn a_store_crossing_pages_is_put_back_from_the_page_that_can_be_read() {
        let (none, read, write) = (libc::PROT_NONE, libc::PROT_READ, libc::PROT_WRITE);
        let both = read | write;
        for protections in [
            [read, write],
            [write, read],
            [write, none],
            [both, none],
            [both, read],
            [none, both],
        ] {
            for format in FORMATS {
                let pages = Pages::new(&protections);
                let (records, names) = directory_records(format, &TEN);
                let copied = super::copy_records(
                    &mut LocalMemory::new(),
                    pages.address(PAGE - 12),
                    format,
                    &records,
                    &names,
                    0,
                );
                assert_eq!(
                    copied,
                    (0, Err(Errno::EFAULT)),
                    "{protections:?} {format:?}"
                );
                assert!(
                    pages.contents().iter().all(|&byte| byte == Pages::FILL),
                    "{protections:?} {format:?}: bytes changed"
                );
            }
        }
    }

    /// Records of 24 bytes starting 100 bytes before a page the guest cannot
    /// write. Four fit. The fifth's `d_ino` crosses into that page, so Linux
    /// writes none of it: four records are returned, the fifth's position
    /// (the fourth's `d_off`) is written, and the four bytes before the page
    /// are left as they were.
    #[test]
    fn a_record_crossing_into_an_inaccessible_page_leaves_nothing_before_it() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ | libc::PROT_WRITE, libc::PROT_NONE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 100),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (4, Ok(96)), "{format:?}");
            let contents = pages.contents();
            assert_eq!(
                &contents[PAGE - 100..PAGE - 4],
                &as_linux_writes(format, &records[..96], &names[..4])[..],
                "{format:?}"
            );
            assert!(
                contents[..PAGE - 100]
                    .iter()
                    .chain(&contents[PAGE - 4..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes outside the records copied changed"
            );
        }
    }

    /// A buffer starting 4 bytes before a page the guest cannot write, in a
    /// page it can write but not read. Linux first writes the first record's
    /// `d_off`, 8 bytes in, which fails; it never writes the 4 bytes before
    /// the page.
    #[test]
    fn a_first_record_is_written_from_its_d_off() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_WRITE, libc::PROT_NONE]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 4),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (0, Err(Errno::EFAULT)), "{format:?}");
            assert!(
                pages.contents().iter().all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes before the inaccessible page changed"
            );
        }
    }

    /// Records across two writable pages and into a third the guest cannot
    /// write, starting 40 bytes before the second page, so the third starts
    /// 4136 bytes in. Record 172 starts at 4128: Linux stores its position
    /// into record 171's `d_off` and its `d_ino`, which ends at the third
    /// page, and then fails at its `d_reclen`. The call returns 172 records
    /// and leaves that `d_ino` written.
    #[test]
    fn a_record_not_copied_is_left_as_linux_leaves_it() {
        let names: Vec<[u8; 1]> = (0..200u8).map(|i| [b'a' + i % 26]).collect();
        let names: Vec<&[u8]> = names.iter().map(|name| &name[..]).collect();
        for format in FORMATS {
            let rw = libc::PROT_READ | libc::PROT_WRITE;
            let pages = Pages::new(&[rw, rw, libc::PROT_NONE]);
            let start = PAGE - 40;
            let (records, lens) = directory_records(format, &names);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(start),
                format,
                &records,
                &lens,
                0,
            );
            assert_eq!(copied, (172, Ok(172 * 24)), "{format:?}");
            let contents = pages.contents();
            let mut expected = as_linux_writes(format, &records[..4128], &lens[..172]);
            expected.extend_from_slice(&records[4128..4136]);
            assert_eq!(&contents[start..start + 4136], &expected[..], "{format:?}");
            assert!(
                contents[..start]
                    .iter()
                    .chain(&contents[start + 4136..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes outside what Linux writes changed"
            );
        }
    }

    /// A second record with a 20-byte name, whose record ends past the end of
    /// what the guest can write. For `getdents64`, the NUL after the name is
    /// stored before the name and fails, so none of the name is written; for
    /// `getdents`, `d_type`, at the record's end, is stored before the name
    /// and fails. Either way the record's own `d_off` is not written: Linux
    /// stores it only with the next record or at the end of the call.
    #[test]
    fn a_name_is_stored_after_the_bytes_that_follow_it() {
        for format in FORMATS {
            let pages = Pages::new(&[libc::PROT_READ | libc::PROT_WRITE, libc::PROT_NONE]);
            let (records, names) = directory_records(format, &[b"a", b"abcdefghijklmnopqrst"]);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 50),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (1, Ok(24)), "{format:?}");
            let contents = &pages.contents()[PAGE - 50..PAGE];
            let mut expected = as_linux_writes(format, &records[..24], &names[..1]);
            expected.extend_from_slice(&records[24..32]);
            expected.extend_from_slice(&[Pages::FILL; 8]);
            expected.extend_from_slice(&records[40..42]);
            match format {
                super::DirentFormat::Dirent64 => expected.push(records[42]),
                super::DirentFormat::Legacy => expected.push(Pages::FILL),
            }
            expected.resize(50, Pages::FILL);
            assert_eq!(contents, &expected[..], "{format:?}");
        }
    }

    /// Records that all fit are all copied, apart from the padding after each
    /// name, which Linux does not write, and the bytes after them are left as
    /// they were.
    #[test]
    fn records_that_fit_are_copied_whole() {
        for format in FORMATS {
            let rw = libc::PROT_READ | libc::PROT_WRITE;
            let pages = Pages::new(&[rw, rw]);
            let (records, names) = directory_records(format, &TEN);
            let copied = super::copy_records(
                &mut LocalMemory::new(),
                pages.address(PAGE - 50),
                format,
                &records,
                &names,
                0,
            );
            assert_eq!(copied, (10, Ok(240)), "{format:?}");
            let contents = pages.contents();
            assert_eq!(
                &contents[PAGE - 50..PAGE - 50 + records.len()],
                &as_linux_writes(format, &records, &names)[..],
                "{format:?}"
            );
            assert!(
                contents[..PAGE - 50]
                    .iter()
                    .chain(&contents[PAGE - 50 + records.len()..])
                    .all(|&byte| byte == Pages::FILL),
                "{format:?}: bytes outside the records changed"
            );
        }
    }
}

/// `inject_fstat`, `add_fd` and `stat_guest_path` against a scripted guest
/// whose "address space" is this test process, so every injected syscall runs
/// for real on host memory and a real descriptor or path. Regression coverage
/// for <https://github.com/rrnewton/hermit/issues/3328>; the traced end-to-end
/// cases are `tests_misc::tight_stack_openat` and
/// `tests_misc::tight_stack_maps`. The scripted guest is shared with the
/// `/proc/<pid>/fd` link tests in `namespace`.
#[cfg(test)]
pub(crate) mod inject_fstat_scratch {
    use std::os::fd::IntoRawFd;
    use std::os::fd::RawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Pid;
    use reverie::Tool;
    use reverie::syscalls::FromToRaw;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::ProtFlags;

    use super::*;
    use crate::Config;
    use crate::GlobalState;
    use crate::ThreadState;
    use crate::types::DetPid;

    thread_local! {
        /// The `mountinfo` `guest_mountinfo` returns instead of reading the
        /// guest's, while a test sets one: `ScriptedGuest`'s pid is 1, which
        /// is not the test process.
        pub(crate) static SCRIPTED_MOUNTINFO: std::cell::RefCell<Option<Vec<u8>>> =
            const { std::cell::RefCell::new(None) };
        /// How many times `guest_mountinfo` was called on this thread,
        /// scripted or not.
        pub(crate) static MOUNTINFO_READS: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
        /// (unique mount id, superblock device) pairs an injected
        /// `statmount` answers from instead of running, while a test sets
        /// them: `ENOENT` for a mount not listed, as the guest's kernel
        /// answers for a mount outside its namespace.
        static SCRIPTED_SUPERBLOCKS: std::cell::RefCell<Option<Vec<(u64, u64)>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Room for one `libc::stat`, 8-byte aligned like a real stack slot.
    const ARENA_WORDS: usize = 32;

    /// A guest stack scratch whose commit fails with `commit_error` when one
    /// is set, like the ptrace scratch below an `rsp` with no writable memory
    /// under it (`EFAULT`) or of a task that has gone (`ESRCH`). Otherwise a
    /// commit writes nothing, as on backends whose scratch is a Tool-owned
    /// arena (DBT), so a scratch that is not writable faults only when it is
    /// written -- unless `commit_writes` is set, when it writes zeros over
    /// every reserved byte in one write from the scratch's start and fails
    /// with `EFAULT` at the first byte it cannot write, as the ptrace commit
    /// writes its whole region. `reserve` hands out `arena` in order. The
    /// arena belongs to the guest and outlives every guard, so an early
    /// guard drop is reported by `guard_live` rather than by a write into
    /// freed memory.
    pub(crate) struct ScriptedStack {
        commit_error: Option<Errno>,
        commit_writes: bool,
        arena: usize,
        arena_len: usize,
        reserved: usize,
        /// Whether `size` and `capacity` answer: `inject_fstat` must not ask,
        /// and `stat_guest_path` asks to bound its path.
        sized: bool,
        guard_live: Arc<AtomicBool>,
        commits: Arc<AtomicUsize>,
    }

    pub(crate) struct ScriptedStackGuard {
        guard_live: Arc<AtomicBool>,
    }

    impl Drop for ScriptedStackGuard {
        fn drop(&mut self) {
            self.guard_live.store(false, Ordering::SeqCst);
        }
    }

    impl reverie::Stack for ScriptedStack {
        type StackGuard = ScriptedStackGuard;

        fn size(&self) -> usize {
            assert!(self.sized, "inject_fstat must not query the scratch size");
            self.reserved
        }
        fn capacity(&self) -> usize {
            assert!(
                self.sized,
                "inject_fstat must not query the scratch capacity"
            );
            self.arena_len
        }
        fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
            panic!("inject_fstat reserves its buffer rather than pushing one")
        }
        fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
            let address = self.arena + self.reserved;
            self.reserved += std::mem::size_of::<T>().next_multiple_of(8);
            assert!(
                self.reserved <= self.arena_len,
                "the scripted scratch is full"
            );
            AddrMut::from_raw(address).unwrap()
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            if let Some(errno) = self.commit_error {
                return Err(errno);
            }
            if self.commit_writes {
                LocalMemory::new().write_exact(
                    AddrMut::from_raw(self.arena).unwrap(),
                    &vec![0; self.reserved],
                )?;
            }
            self.guard_live.store(true, Ordering::SeqCst);
            Ok(ScriptedStackGuard {
                guard_live: self.guard_live,
            })
        }
    }

    /// This process's memory as `LocalMemory` reaches it, except that every
    /// write fails with `write_error` when one is set. Counts each write that
    /// starts in the test-supplied stack scratch while no stack guard is
    /// live.
    pub(crate) struct ScriptedMemory {
        write_error: Option<Errno>,
        scratch: Option<(usize, usize)>,
        guard_live: Arc<AtomicBool>,
        unguarded_scratch_writes: Arc<AtomicUsize>,
    }

    impl ScriptedMemory {
        fn note_write(&self, address: usize) {
            if let Some((start, len)) = self.scratch
                && (start..start + len).contains(&address)
                && !self.guard_live.load(Ordering::SeqCst)
            {
                self.unguarded_scratch_writes.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl MemoryAccess for ScriptedMemory {
        fn read_vectored(
            &self,
            read_from: &[std::io::IoSlice],
            write_to: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            LocalMemory::new().read_vectored(read_from, write_to)
        }
        fn write_vectored(
            &mut self,
            read_from: &[std::io::IoSlice],
            write_to: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            for slice in write_to.iter() {
                self.note_write(slice.as_ptr() as usize);
            }
            match self.write_error {
                Some(errno) => Err(errno),
                None => LocalMemory::new().write_vectored(read_from, write_to),
            }
        }
        fn read<'a, A>(&self, addr: A, buf: &mut [u8]) -> Result<usize, Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            LocalMemory::new().read(addr, buf)
        }
        fn write(&mut self, addr: AddrMut<u8>, buf: &[u8]) -> Result<usize, Errno> {
            self.note_write(addr.as_raw());
            match self.write_error {
                Some(errno) => Err(errno),
                None => LocalMemory::new().write(addr, buf),
            }
        }
        fn write_with_user_access(
            &mut self,
            addr: AddrMut<u8>,
            buf: &[u8],
        ) -> Result<usize, Errno> {
            self.note_write(addr.as_raw());
            match self.write_error {
                Some(errno) => Err(errno),
                None => LocalMemory::new().write_with_user_access(addr, buf),
            }
        }
    }

    pub(crate) struct ScriptedGuest {
        pub(crate) config: Config,
        pub(crate) thread: ThreadState<()>,
        commit_error: Option<Errno>,
        /// Whether a commit writes the scratch, as on ptrace: see
        /// `ScriptedStack`.
        commit_writes: bool,
        /// (address, length) of the stack scratch when the test supplies one;
        /// otherwise the scratch is `arena` and does not report its size.
        scratch: Option<(usize, usize)>,
        write_error: Option<Errno>,
        mmap_fails: bool,
        /// The errno every injected `fstatat` fails with instead of running,
        /// when set: a failure the test process cannot provoke on demand.
        fstatat_error: Option<Errno>,
        /// The errno an injected anonymous `mmap` of this many bytes fails
        /// with instead of running, when set: one transient page refused, as
        /// by a seccomp filter, while the guest's other mappings succeed.
        mmap_error_of_len: Option<(usize, Errno)>,
        /// The errno an injected `munmap` of this many bytes fails with
        /// instead of running, when set; the mapping then stays.
        munmap_error_of_len: Option<(usize, Errno)>,
        /// The errno every injected `statx` of a directory entry (relative to
        /// an open directory) fails with instead of running, when set: the
        /// mount-root question refused, as by a seccomp filter.
        entry_statx_error: Option<Errno>,
        arena: Box<[u64; ARENA_WORDS]>,
        guard_live: Arc<AtomicBool>,
        /// How many times a stack scratch was committed, successfully or not.
        commits: Arc<AtomicUsize>,
        /// Writes into the test-supplied stack scratch made while no stack
        /// guard was live.
        unguarded_scratch_writes: Arc<AtomicUsize>,
        pub(crate) injected: Vec<Sysno>,
        /// Whether a stack guard was live when each fstat was injected.
        fstat_guard_live: Vec<bool>,
        /// Buffer address of each injected fstat.
        fstat_buffers: Vec<usize>,
        /// Whether a stack guard was live when each fstatat was injected.
        fstatat_guard_live: Vec<bool>,
        /// Path each injected fstatat named, as the kernel reads it.
        pub(crate) fstatat_paths: Vec<Vec<u8>>,
        /// (dirfd, flags) of each injected fstatat.
        fstatat_starts: Vec<(RawFd, AtFlags)>,
        /// (path, answer): an injected fstatat of `path` reads the stat, or
        /// fails with the errno, instead of running, for a file the test
        /// process cannot make.
        fstatat_answers: Vec<(Vec<u8>, Result<libc::stat, Errno>)>,
        /// Path each injected statx named, as the kernel reads it.
        pub(crate) statx_paths: Vec<Vec<u8>>,
        /// Whether an injected statx ignores `STATX_MNT_ID_UNIQUE`, as a
        /// kernel before Linux 6.8 does, and so reports the reusable mount
        /// id instead.
        statx_without_unique_mount_id: bool,
        /// Directory entries an injected `statx` relative to a directory
        /// reports as the root of a mount (`STATX_ATTR_MOUNT_ROOT`), as a
        /// file bind-mounted over the entry is.
        statx_mount_roots: Vec<Vec<u8>>,
        /// (start, path) of each injected statx.
        statx_starts: Vec<(RawFd, AtFlags)>,
        /// Whether an injected statx reports no `STATX_ATTR_MOUNT_ROOT` in its
        /// attributes mask, as a kernel before Linux 5.8 does.
        statx_without_mount_root_attribute: bool,
        /// The errno every injected `statmount` fails with instead of
        /// running, when set: `ENOSYS` as under a seccomp policy, or an
        /// answer the test process's own mounts do not give.
        statmount_error: Option<Errno>,
        /// Whether a scripted `statmount` answers without the superblock
        /// fields it was asked for.
        statmount_omits_superblock: bool,
        /// Mount id each injected statmount asked for.
        statmount_mounts: Vec<u64>,
        /// Link each injected readlinkat named, as the kernel reads it.
        pub(crate) readlink_paths: Vec<Vec<u8>>,
        /// (link, text): an injected readlinkat of `link` reads `text`
        /// instead of running, for a link the test process cannot make.
        pub(crate) readlink_answers: Vec<(Vec<u8>, Vec<u8>)>,
        /// Buffer address of each injected fstatat.
        fstatat_buffers: Vec<usize>,
        /// (address, length) of each page the guest mapped.
        mapped: Vec<(usize, usize)>,
        /// (address, length) of each successful munmap.
        unmapped: Vec<(usize, usize)>,
        /// (address, length) of each file mapping the guest made.
        file_mapped: Vec<(usize, usize)>,
        /// Descriptors closed through injection.
        closed: Vec<RawFd>,
        /// Whether `send_rpc` answers `DeterminizeInode`, and the
        /// `RetirementCount` a tracked listing asks before it takes its
        /// snapshot (always zero: the scripted pool retires nothing). Off by
        /// default, so a test that expects no RPC still fails on one.
        pub(crate) answers_determinize_inode: bool,
        /// Raw identity of each `DeterminizeInode` request, and of each entry
        /// of a `DeterminizeMappingInodes` request, in order. As in the real
        /// pool, every request consumes a number: an identity first requested
        /// at position `n` of this log (from 0) is answered with
        /// `FIRST_SCRIPTED_INODE + n`, then and on every later request.
        pub(crate) determinized: std::sync::Mutex<Vec<RawInode>>,
        /// Whether `send_rpc` answers `DeterminizeDevice`. Off by default, so
        /// a test that expects no RPC still fails on one.
        pub(crate) answers_determinize_device: bool,
        /// Raw device of each `DeterminizeDevice` request, in order. Every
        /// request for the `n`th DISTINCT device (from 0) is answered with
        /// the same deterministic device, `makedev(0, n + 1)`.
        pub(crate) determinized_devices: std::sync::Mutex<Vec<u64>>,
        /// What `detlog_memory_regions` reports: `None`, as ptrace does, or
        /// the guest ranges a test wants the memory-map DETLOG to hash.
        detlog_regions: Option<Vec<reverie::DetlogMemoryRegion>>,
        /// (descriptor, stat): an injected fstat of the descriptor reads the
        /// stat instead of running, for a file the test process cannot make.
        fstat_answers: Vec<(RawFd, libc::stat)>,
        /// Whether `send_rpc` answers `TouchFile`. Off by default, so a test
        /// that expects no RPC still fails on one.
        answers_touch_file: bool,
        /// Raw identity of each `TouchFile` request, in order.
        touched: std::sync::Mutex<Vec<RawInode>>,
        /// What every injected `fstatfs` answers instead of running, when
        /// set: the filesystem type it reports, or the errno it fails with.
        /// The test's own directories are on whatever filesystem `TMPDIR`
        /// is.
        fstatfs_answer: Option<Result<libc::__fsword_t, Errno>>,
        /// (descriptor, whether a stack guard was live) of each injected
        /// fstatfs.
        fstatfs_calls: Vec<(RawFd, bool)>,
        /// Whether a seccomp filter traps every injected fstatfs. The ptrace
        /// backend then reports `ENOSYS` and requeues the `SIGSYS` to the
        /// guest; this guest counts the signal in `sigsys_queued`.
        fstatfs_traps: bool,
        /// How many `SIGSYS` a trapped injection queued for the guest.
        sigsys_queued: usize,
        /// (name, inode): an injected getdents64 reports `inode` as the
        /// `d_ino` of the entry `name`, for an inode number the test cannot
        /// give a file, such as a btrfs subvolume's.
        getdents_inode_answers: Vec<(Vec<u8>, u64)>,
        /// (raw device, kind) of each overlay settled through
        /// `SettleOverlayEntryLookup`, the first answer for a device kept, as
        /// `GlobalState` keeps it. `send_rpc` answers that request always:
        /// only a listing on overlayfs sends it.
        settled_overlays: std::sync::Mutex<Vec<(u64, EntryLookup)>>,
        /// (raw device, proposal) of each `SettleOverlayEntryLookup` request,
        /// in order.
        settle_requests: std::sync::Mutex<Vec<(u64, Option<EntryLookup>)>>,
    }

    /// The deterministic inode `send_rpc` gives the first `DeterminizeInode`
    /// request.
    pub(crate) const FIRST_SCRIPTED_INODE: u64 = 7000;

    impl ScriptedGuest {
        pub(crate) fn new(stack_writable: bool, mmap_fails: bool) -> (Detcore, Self) {
            let config = Config {
                virtualize_metadata: true,
                ..Config::default()
            };
            let pid = DetPid::from_raw(1);
            let mut thread = ThreadState::new(pid, &config, ());
            thread.detpid = Some(pid);
            let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
            let guest = Self {
                config,
                thread,
                commit_error: (!stack_writable).then_some(Errno::EFAULT),
                commit_writes: false,
                scratch: None,
                write_error: None,
                mmap_fails,
                fstatat_error: None,
                mmap_error_of_len: None,
                munmap_error_of_len: None,
                entry_statx_error: None,
                arena: Box::new([u64::MAX; ARENA_WORDS]),
                guard_live: Arc::new(AtomicBool::new(false)),
                commits: Arc::new(AtomicUsize::new(0)),
                unguarded_scratch_writes: Arc::new(AtomicUsize::new(0)),
                injected: Vec::new(),
                fstat_guard_live: Vec::new(),
                fstat_buffers: Vec::new(),
                fstatat_guard_live: Vec::new(),
                fstatat_paths: Vec::new(),
                fstatat_starts: Vec::new(),
                fstatat_answers: Vec::new(),
                statx_paths: Vec::new(),
                statx_without_unique_mount_id: false,
                statx_mount_roots: Vec::new(),
                statx_starts: Vec::new(),
                statx_without_mount_root_attribute: false,
                statmount_error: None,
                statmount_omits_superblock: false,
                statmount_mounts: Vec::new(),
                readlink_paths: Vec::new(),
                readlink_answers: Vec::new(),
                fstatat_buffers: Vec::new(),
                mapped: Vec::new(),
                unmapped: Vec::new(),
                file_mapped: Vec::new(),
                closed: Vec::new(),
                answers_determinize_inode: false,
                determinized: std::sync::Mutex::new(Vec::new()),
                answers_determinize_device: false,
                determinized_devices: std::sync::Mutex::new(Vec::new()),
                detlog_regions: None,
                fstat_answers: Vec::new(),
                answers_touch_file: false,
                touched: std::sync::Mutex::new(Vec::new()),
                fstatfs_answer: None,
                fstatfs_calls: Vec::new(),
                fstatfs_traps: false,
                sigsys_queued: 0,
                getdents_inode_answers: Vec::new(),
                settled_overlays: std::sync::Mutex::new(Vec::new()),
                settle_requests: std::sync::Mutex::new(Vec::new()),
            };
            (tool, guest)
        }

        /// A guest whose stack scratch is the `len` bytes at `address`, and
        /// whose commit fails with `commit_error` when one is given.
        pub(crate) fn with_scratch(
            address: usize,
            len: usize,
            commit_error: Option<Errno>,
        ) -> (Detcore, Self) {
            let (tool, mut guest) = Self::new(true, false);
            guest.scratch = Some((address, len));
            guest.commit_error = commit_error;
            (tool, guest)
        }

        /// How many writes went into the test-supplied stack scratch while
        /// no stack guard was live.
        fn unguarded_scratch_writes(&self) -> usize {
            self.unguarded_scratch_writes.load(Ordering::SeqCst)
        }

        /// Record one inode request for `raw` in `determinized`, and answer
        /// it as that log's model of the pool does.
        fn scripted_inode(&self, raw: RawInode) -> DetInode {
            let mut determinized = self.determinized.lock().unwrap();
            determinized.push(raw);
            let first_request = determinized
                .iter()
                .position(|seen| *seen == raw)
                .expect("the request was just recorded");
            DetInode::mint(FIRST_SCRIPTED_INODE + first_request as u64)
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ScriptedGuest {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            match message.2 {
                GlobalRequest::DeterminizeInode(raw, _, _) if self.answers_determinize_inode => (
                    None,
                    GlobalResponse::DeterminizeInode((self.scripted_inode(raw), LogicalTime::ZERO)),
                ),
                GlobalRequest::RetirementCount if self.answers_determinize_inode => {
                    (None, GlobalResponse::RetirementCount(0))
                }
                GlobalRequest::DeterminizeMappingInodes(raws) if self.answers_determinize_inode => {
                    (
                        None,
                        GlobalResponse::DeterminizeMappingInodes(
                            raws.into_iter()
                                .map(|raw| self.scripted_inode(raw))
                                .collect(),
                        ),
                    )
                }
                GlobalRequest::DeterminizeDevice(raw) if self.answers_determinize_device => {
                    let mut devices = self.determinized_devices.lock().unwrap();
                    devices.push(raw);
                    let mut distinct: Vec<u64> = Vec::new();
                    for seen in devices.iter() {
                        if !distinct.contains(seen) {
                            distinct.push(*seen);
                        }
                    }
                    let index = distinct
                        .iter()
                        .position(|seen| *seen == raw)
                        .expect("the request was just recorded");
                    let minor = u32::try_from(index + 1).expect("few scripted devices");
                    (
                        None,
                        GlobalResponse::DeterminizeDevice(libc::makedev(0, minor)),
                    )
                }
                GlobalRequest::TouchFile(raw) if self.answers_touch_file => {
                    self.touched.lock().unwrap().push(raw);
                    (None, GlobalResponse::TouchFile(()))
                }
                GlobalRequest::SettleOverlayEntryLookup(device, proposal) => {
                    self.settle_requests
                        .lock()
                        .unwrap()
                        .push((device, proposal));
                    let mut settled = self.settled_overlays.lock().unwrap();
                    let known = settled
                        .iter()
                        .find(|(seen, _)| *seen == device)
                        .map(|(_, lookup)| *lookup);
                    let answer = match (known, proposal) {
                        (Some(known), _) => Some(known),
                        (None, Some(proposal)) => {
                            settled.push((device, proposal));
                            Some(proposal)
                        }
                        (None, None) => None,
                    };
                    (None, GlobalResponse::SettleOverlayEntryLookup(answer))
                }
                request => panic!("fd registration must not send an RPC: {request:?}"),
            }
        }
        fn config(&self) -> &Config {
            &self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for ScriptedGuest {
        type Memory = ScriptedMemory;
        type Stack = ScriptedStack;

        fn tid(&self) -> Pid {
            Pid::from_raw(1)
        }
        fn pid(&self) -> Pid {
            Pid::from_raw(1)
        }
        fn ppid(&self) -> Option<Pid> {
            None
        }
        fn memory(&self) -> Self::Memory {
            ScriptedMemory {
                write_error: self.write_error,
                scratch: self.scratch,
                guard_live: self.guard_live.clone(),
                unguarded_scratch_writes: self.unguarded_scratch_writes.clone(),
            }
        }
        fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
            &mut self.thread
        }
        fn thread_state(&self) -> &ThreadState<()> {
            &self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("fd registration must not read registers")
        }
        async fn stack(&mut self) -> Self::Stack {
            let (arena, arena_len, sized) = match self.scratch {
                Some((address, len)) => (address, len, true),
                None => (
                    self.arena.as_mut_ptr() as usize,
                    ARENA_WORDS * std::mem::size_of::<u64>(),
                    false,
                ),
            };
            ScriptedStack {
                commit_error: self.commit_error,
                commit_writes: self.commit_writes,
                arena,
                arena_len,
                reserved: 0,
                sized,
                guard_live: self.guard_live.clone(),
                commits: self.commits.clone(),
            }
        }
        async fn daemonize(&mut self) {
            panic!("fd registration must not daemonize")
        }
        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            let (number, args) = syscall.into_parts();
            self.injected.push(number);
            // SAFETY: each arm runs the syscall Detcore asked for against this
            // process, on addresses Detcore obtained from this guest.
            let raw = match Syscall::from_raw(number, args) {
                // A guest's file mapping, forwarded by `handle_mmap`.
                Syscall::Mmap(call) if call.fd() >= 0 => {
                    let address = unsafe {
                        libc::mmap(
                            call.addr()
                                .map_or(std::ptr::null_mut(), |addr| addr.as_raw() as *mut _),
                            call.len(),
                            call.prot().bits(),
                            call.flags().bits(),
                            call.fd(),
                            call.offset(),
                        )
                    };
                    if address == libc::MAP_FAILED {
                        -1
                    } else {
                        self.file_mapped.push((address as usize, call.len()));
                        address as i64
                    }
                }
                Syscall::Mmap(call) => {
                    assert!(call.addr().is_none(), "the kernel must choose the address");
                    assert_eq!(call.prot(), ProtFlags::PROT_READ | ProtFlags::PROT_WRITE);
                    assert_eq!(
                        call.flags(),
                        MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS
                    );
                    if self.mmap_fails {
                        return Err(Errno::ENOMEM);
                    }
                    if let Some((len, errno)) = self.mmap_error_of_len
                        && len == call.len()
                    {
                        return Err(errno);
                    }
                    let address = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            call.len(),
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                            -1,
                            0,
                        )
                    };
                    if address == libc::MAP_FAILED {
                        -1
                    } else {
                        self.mapped.push((address as usize, call.len()));
                        address as i64
                    }
                }
                Syscall::Fstat(call) => {
                    let buffer = call.stat().expect("fstat without a buffer").0.as_raw();
                    self.fstat_buffers.push(buffer);
                    self.fstat_guard_live
                        .push(self.guard_live.load(Ordering::SeqCst));
                    match self.fstat_answers.iter().find(|(fd, _)| *fd == call.fd()) {
                        Some(&(_, answer)) => {
                            unsafe { std::ptr::write_unaligned(buffer as *mut libc::stat, answer) };
                            0
                        }
                        None => {
                            i64::from(unsafe { libc::fstat(call.fd(), buffer as *mut libc::stat) })
                        }
                    }
                }
                Syscall::Newfstatat(call) => {
                    // A path's stat, or a directory entry's lstat relative to
                    // the open directory; nothing else.
                    let start = (call.dirfd(), call.flags());
                    assert!(
                        start == (libc::AT_FDCWD, AtFlags::empty())
                            || (start.0 >= 0
                                && start.1
                                    == AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_NO_AUTOMOUNT),
                        "unexpected fstatat start {start:?}"
                    );
                    let path = call.path().expect("fstatat without a path");
                    let buffer = call.stat().expect("fstatat without a buffer").0.as_raw();
                    let named = path
                        .read(&LocalMemory::new())
                        .expect("fstatat names a path it cannot read")
                        .into_os_string()
                        .into_vec();
                    self.fstatat_starts.push(start);
                    self.fstatat_buffers.push(buffer);
                    self.fstatat_guard_live
                        .push(self.guard_live.load(Ordering::SeqCst));
                    let answer = self
                        .fstatat_answers
                        .iter()
                        .find(|(answered, _)| *answered == named)
                        .map(|(_, answer)| *answer);
                    self.fstatat_paths.push(named);
                    if let Some(errno) = self.fstatat_error {
                        return Err(errno);
                    }
                    match answer {
                        Some(Ok(answer)) => {
                            unsafe { std::ptr::write_unaligned(buffer as *mut libc::stat, answer) };
                            0
                        }
                        Some(Err(errno)) => return Err(errno),
                        None => unsafe {
                            libc::syscall(
                                libc::SYS_newfstatat,
                                start.0,
                                Some(path).into_raw(),
                                buffer,
                                start.1.bits(),
                            )
                        },
                    }
                }
                Syscall::Statx(call) => {
                    // A path's statx, or a directory entry's relative to the
                    // open directory, as its lstat is asked; nothing else.
                    let start = (call.dirfd(), call.flags());
                    let entry = start.0 >= 0
                        && start.1 == AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_NO_AUTOMOUNT;
                    assert!(
                        start == (libc::AT_FDCWD, AtFlags::empty()) || entry,
                        "unexpected statx start {start:?}"
                    );
                    let path = call.path().expect("statx without a path");
                    let buffer = call.statx().expect("statx without a buffer").0.as_raw();
                    let named = path
                        .read(&LocalMemory::new())
                        .expect("statx names a path it cannot read")
                        .into_os_string()
                        .into_vec();
                    self.statx_starts.push(start);
                    self.statx_paths.push(named.clone());
                    if let Some(errno) = self.entry_statx_error
                        && entry
                    {
                        return Err(errno);
                    }
                    // An entry whose lstat is scripted is reported by its
                    // statx alike: the same file, device and number.
                    let scripted = entry
                        .then(|| {
                            self.fstatat_answers
                                .iter()
                                .find(|(answered, _)| *answered == named)
                                .map(|(_, answer)| *answer)
                        })
                        .flatten();
                    // The raw mask: `call.mask()` drops bits the pinned
                    // Reverie does not name, such as STATX_MNT_ID_UNIQUE.
                    let mut mask = args.arg3 as u32;
                    if self.statx_without_unique_mount_id {
                        mask &= !STATX_MNT_ID_UNIQUE;
                    }
                    let raw = match scripted {
                        Some(Err(errno)) => return Err(errno),
                        Some(Ok(answer)) => {
                            let mut statx: libc::statx = unsafe { std::mem::zeroed() };
                            statx.stx_mask = libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_INO;
                            statx.stx_mode = answer.st_mode as u16;
                            statx.stx_ino = answer.st_ino;
                            statx.stx_dev_major = libc::major(answer.st_dev);
                            statx.stx_dev_minor = libc::minor(answer.st_dev);
                            statx.stx_attributes_mask = libc::STATX_ATTR_MOUNT_ROOT as u64;
                            unsafe { std::ptr::write_unaligned(buffer as *mut libc::statx, statx) };
                            0
                        }
                        None => unsafe {
                            libc::syscall(
                                libc::SYS_statx,
                                start.0,
                                Some(path).into_raw(),
                                start.1.bits(),
                                mask,
                                buffer,
                            )
                        },
                    };
                    if raw == 0 && entry {
                        let mount_root = libc::STATX_ATTR_MOUNT_ROOT as u64;
                        let mut statx: libc::statx =
                            unsafe { std::ptr::read_unaligned(buffer as *const libc::statx) };
                        if self.statx_mount_roots.contains(&named) {
                            statx.stx_attributes |= mount_root;
                        }
                        if self.statx_without_mount_root_attribute {
                            statx.stx_attributes &= !mount_root;
                            statx.stx_attributes_mask &= !mount_root;
                        }
                        unsafe { std::ptr::write_unaligned(buffer as *mut libc::statx, statx) };
                    }
                    raw
                }
                Syscall::Other(Sysno::statmount, args) => {
                    // SAFETY: Detcore staged the request in a page it mapped.
                    let request = unsafe {
                        std::slice::from_raw_parts(args.arg0 as *const u8, MNT_ID_REQ_SIZE_VER0)
                    };
                    let word = |offset: usize| {
                        u32::from_ne_bytes(request[offset..offset + 4].try_into().unwrap())
                    };
                    let dword = |offset: usize| {
                        u64::from_ne_bytes(request[offset..offset + 8].try_into().unwrap())
                    };
                    assert_eq!(word(0) as usize, MNT_ID_REQ_SIZE_VER0, "mnt_id_req.size");
                    assert_eq!(word(4), 0, "mnt_id_req.spare");
                    assert_eq!(dword(16), STATMOUNT_SB_BASIC, "mnt_id_req.param");
                    assert_eq!(args.arg2, STATMOUNT_BUFFER, "statmount bufsize");
                    assert_eq!(args.arg3, 0, "statmount flags");
                    let mount_id = dword(8);
                    self.statmount_mounts.push(mount_id);
                    if let Some(errno) = self.statmount_error {
                        return Err(errno);
                    }
                    match SCRIPTED_SUPERBLOCKS.with(|scripted| scripted.borrow().clone()) {
                        Some(superblocks) => {
                            let Some(&(_, device)) =
                                superblocks.iter().find(|(id, _)| *id == mount_id)
                            else {
                                return Err(Errno::ENOENT);
                            };
                            let mut answer = [0u8; STATMOUNT_BUFFER];
                            answer[..4].copy_from_slice(&(STATMOUNT_BUFFER as u32).to_ne_bytes());
                            if !self.statmount_omits_superblock {
                                answer[STATMOUNT_MASK_OFFSET..STATMOUNT_MASK_OFFSET + 8]
                                    .copy_from_slice(&STATMOUNT_SB_BASIC.to_ne_bytes());
                                answer[STATMOUNT_SB_DEV_MAJOR_OFFSET
                                    ..STATMOUNT_SB_DEV_MAJOR_OFFSET + 4]
                                    .copy_from_slice(&libc::major(device).to_ne_bytes());
                                answer[STATMOUNT_SB_DEV_MINOR_OFFSET
                                    ..STATMOUNT_SB_DEV_MINOR_OFFSET + 4]
                                    .copy_from_slice(&libc::minor(device).to_ne_bytes());
                            }
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    answer.as_ptr(),
                                    args.arg1 as *mut u8,
                                    answer.len(),
                                )
                            };
                            0
                        }
                        None => unsafe {
                            libc::syscall(
                                Sysno::statmount as libc::c_long,
                                args.arg0,
                                args.arg1,
                                args.arg2,
                                args.arg3,
                            )
                        },
                    }
                }
                Syscall::Readlinkat(call) => {
                    assert_eq!(call.dirfd(), libc::AT_FDCWD);
                    let path = call.path().expect("readlinkat without a path");
                    let buffer = call.buf().expect("readlinkat without a buffer").as_raw();
                    let link = path
                        .read(&LocalMemory::new())
                        .expect("readlinkat names a path it cannot read")
                        .into_os_string()
                        .into_vec();
                    let scripted = self
                        .readlink_answers
                        .iter()
                        .find(|(scripted, _)| *scripted == link)
                        .map(|(_, text)| text.clone());
                    self.readlink_paths.push(link);
                    match scripted {
                        Some(text) => {
                            let len = text.len().min(call.buf_len());
                            unsafe {
                                std::ptr::copy_nonoverlapping(text.as_ptr(), buffer as *mut u8, len)
                            };
                            i64::try_from(len).unwrap()
                        }
                        None => unsafe {
                            libc::syscall(
                                libc::SYS_readlinkat,
                                libc::AT_FDCWD,
                                Some(path).into_raw(),
                                buffer,
                                call.buf_len(),
                            )
                        },
                    }
                }
                Syscall::Munmap(call) => {
                    let address = call.addr().expect("munmap without an address").as_raw();
                    if let Some((len, errno)) = self.munmap_error_of_len
                        && len == call.len()
                    {
                        return Err(errno);
                    }
                    let raw = i64::from(unsafe { libc::munmap(address as *mut _, call.len()) });
                    if raw == 0 {
                        self.unmapped.push((address, call.len()));
                    }
                    raw
                }
                Syscall::Close(call) => {
                    self.closed.push(call.fd());
                    i64::from(unsafe { libc::close(call.fd()) })
                }
                // A write a handler forwards, run as it is: the test process
                // stands in for the guest and owns its buffers.
                Syscall::Write(_)
                | Syscall::Pwrite64(_)
                | Syscall::Writev(_)
                | Syscall::Pwritev(_)
                | Syscall::Pwritev2(_)
                | Syscall::Sendfile(_) => unsafe {
                    libc::syscall(
                        number as libc::c_long,
                        args.arg0,
                        args.arg1,
                        args.arg2,
                        args.arg3,
                        args.arg4,
                        args.arg5,
                    )
                },
                Syscall::Fstatfs(call) => {
                    let buffer = call.buf().expect("fstatfs without a buffer").as_raw();
                    self.fstatfs_calls
                        .push((call.fd(), self.guard_live.load(Ordering::SeqCst)));
                    if self.fstatfs_traps {
                        self.sigsys_queued += 1;
                        return Err(Errno::ENOSYS);
                    }
                    match self.fstatfs_answer {
                        Some(Ok(f_type)) => {
                            let mut answer: libc::statfs = unsafe { std::mem::zeroed() };
                            answer.f_type = f_type;
                            unsafe {
                                std::ptr::write_unaligned(buffer as *mut libc::statfs, answer)
                            };
                            0
                        }
                        Some(Err(errno)) => return Err(errno),
                        None => i64::from(unsafe {
                            libc::fstatfs(call.fd(), buffer as *mut libc::statfs)
                        }),
                    }
                }
                // A directory read a getdents handler issues, run as it is on
                // the test's own descriptor and buffers, with any scripted
                // `d_ino` written over the kernel's.
                Syscall::Getdents64(call) => {
                    let len = unsafe {
                        libc::syscall(number as libc::c_long, args.arg0, args.arg1, args.arg2)
                    };
                    if len > 0 && !self.getdents_inode_answers.is_empty() {
                        let buffer = call.dirent().expect("getdents64 without a buffer");
                        // SAFETY: the kernel just wrote `len` bytes there.
                        let records = unsafe {
                            std::slice::from_raw_parts_mut(buffer.as_raw() as *mut u8, len as usize)
                        };
                        let mut at = 0;
                        while at < records.len() {
                            let reclen =
                                u16::from_ne_bytes([records[at + 16], records[at + 17]]) as usize;
                            let name = &records[at + 19..at + reclen];
                            let name = &name[..name.iter().position(|&b| b == 0).unwrap()];
                            if let Some((_, inode)) = self
                                .getdents_inode_answers
                                .iter()
                                .find(|(scripted, _)| scripted == name)
                            {
                                records[at..at + 8].copy_from_slice(&inode.to_ne_bytes());
                            }
                            at += reclen;
                        }
                    }
                    len
                }
                // A seek a getdents handler issues, run as it is.
                Syscall::Lseek(_) => unsafe {
                    libc::syscall(number as libc::c_long, args.arg0, args.arg1, args.arg2)
                },
                // The test process stands in for the guest's thread.
                Syscall::Getpid(_) => i64::from(unsafe { libc::getpid() }),
                Syscall::Gettid(_) => unsafe { libc::syscall(libc::SYS_gettid) },
                other => panic!("unexpected injected syscall {other:?}"),
            };
            Errno::result(raw)
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("fd registration must not retire the guest")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("fd registration must not set a timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
            panic!("fd registration must not set a timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("fd registration must not read a clock")
        }
        fn detlog_memory_regions(&self) -> Option<Vec<reverie::DetlogMemoryRegion>> {
            self.detlog_regions.clone()
        }
    }

    /// A real descriptor the test owns by number, and its inode. Ownership is
    /// raw so a descriptor that Detcore closes is never closed a second time.
    fn open_file() -> (RawFd, u64) {
        let file = tempfile::tempfile().unwrap();
        let inode = file.metadata().unwrap().ino();
        (file.into_raw_fd(), inode)
    }

    fn close_unless_detcore_did(guest: &ScriptedGuest, fd: RawFd) {
        if !guest.closed.contains(&fd) {
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
    }

    fn recorded_inode(guest: &ScriptedGuest, fd: RawFd) -> Option<u64> {
        guest
            .thread
            .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.inode))
            .unwrap()
    }

    #[tokio::test]
    async fn writable_stack_scratch_is_used_while_its_guard_is_live() {
        let (fd, inode) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(true, false);

        let result = tool
            .add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(result, Ok(()));
        assert_eq!(guest.injected, [Sysno::fstat]);
        assert_eq!(
            guest.fstat_guard_live,
            [true],
            "the stack guard must outlive the injected fstat: backends whose \
             scratch is an arena free it when the guard drops"
        );
        assert_eq!(
            guest.fstat_buffers,
            [guest.arena.as_ptr() as usize],
            "fstat must write into the stack scratch"
        );
        assert!(
            guest.arena.iter().all(|word| *word == u64::MAX),
            "the guest's bytes under the stat buffer must be put back, and the \
             stat must not be left in the guest's stack scratch"
        );
        assert_eq!(recorded_inode(&guest, fd), Some(inode));
    }

    /// `inject_fstat`'s result, printable without `libc::stat: Debug`.
    fn fstat_outcome(result: &Result<libc::stat, Errno>) -> String {
        match result {
            Ok(stat) => format!("Ok(stat of inode {})", stat.st_ino),
            Err(errno) => format!("Err({errno:?})"),
        }
    }

    #[tokio::test]
    async fn inject_fstat_restores_the_guest_bytes_under_its_buffer() {
        let (fd, inode) = open_file();
        // A commit that writes zeros over the scratch, as on ptrace, and one
        // that writes nothing, as on the arena backends.
        for commit_writes in [true, false] {
            let scratch = Pages::sentinel(1, 1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.commit_writes = commit_writes;

            let result = tool.inject_fstat(&mut guest, fd).await;

            let stat = result.unwrap_or_else(|errno| panic!("inject_fstat failed: {errno}"));
            assert_eq!(stat.st_ino, inode);
            assert_eq!(guest.injected, [Sysno::fstat]);
            assert_eq!(
                guest.fstat_buffers,
                [scratch.address],
                "fstat must write into the stack scratch"
            );
            assert_eq!(
                guest.fstat_guard_live,
                [true],
                "the stack guard must outlive the injected fstat"
            );
            assert_eq!(
                guest.unguarded_scratch_writes(),
                0,
                "the buffer must be written back before the stack guard drops"
            );
            assert!(
                scratch.holds_sentinel(0, scratch.len),
                "the guest's bytes under the fstat buffer must be put back \
                 (commit_writes: {commit_writes})"
            );
        }
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[tokio::test]
    async fn inject_fstat_restores_its_scratch_when_the_fstat_fails() {
        // A descriptor number far above any open one: the guest's fstat
        // fails with EBADF, which is the guest's answer.
        let closed: RawFd = 1 << 20;
        let scratch = Pages::sentinel(1, 1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.commit_writes = true;

        let result = tool.inject_fstat(&mut guest, closed).await;

        assert!(
            matches!(result, Err(Errno::EBADF)),
            "a failed fstat is the guest's answer: {}",
            fstat_outcome(&result)
        );
        assert_eq!(
            guest.injected,
            [Sysno::fstat],
            "a failed fstat is not a reason to retry"
        );
        assert_eq!(guest.fstat_guard_live, [true]);
        assert_eq!(guest.unguarded_scratch_writes(), 0);
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the guest's bytes must be put back after a failed fstat"
        );
    }

    #[tokio::test]
    async fn inject_fstat_restores_what_a_failed_commit_wrote() {
        // As on ptrace, the commit writes the whole scratch in one write from
        // its start: here its first 64 bytes, on a writable page, before it
        // faults on the read-only page after them.
        let (fd, inode) = open_file();
        let written = 64;
        let scratch = Pages::sentinel(2, 1, 2);
        let start = page_size() - written;
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address + start, scratch.len - start, None);
        guest.commit_writes = true;

        let result = tool.inject_fstat(&mut guest, fd).await;

        let stat = result.unwrap_or_else(|errno| {
            panic!("a scratch the guest cannot hold must not fail the caller: {errno}")
        });
        assert_eq!(stat.st_ino, inode);
        assert_eq!(guest.commits.load(Ordering::SeqCst), 1);
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::fstat, Sysno::munmap],
            "no fstat may be injected into a scratch whose commit failed"
        );
        let [(page, _)] = guest.mapped[..] else {
            panic!("expected one transient page, got {:?}", guest.mapped);
        };
        assert_eq!(guest.fstat_buffers, [page]);
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the bytes the failed commit wrote must be put back"
        );
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[tokio::test]
    async fn inject_fstat_does_not_use_a_scratch_it_cannot_save() {
        // The buffer starts 64 bytes before the end of a writable page, and
        // the page after it cannot be accessed, so the guest's bytes under the
        // buffer cannot be read and could not be put back.
        let (fd, inode) = open_file();
        let readable = 64;
        let scratch = Pages::sentinel(2, 1, 1);
        let start = page_size() - readable;
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address + start, scratch.len - start, None);
        guest.commit_writes = true;

        let result = tool.inject_fstat(&mut guest, fd).await;

        let stat = result.unwrap_or_else(|errno| {
            panic!("a scratch that cannot be saved must not fail the caller: {errno}")
        });
        assert_eq!(stat.st_ino, inode);
        assert_eq!(
            guest.commits.load(Ordering::SeqCst),
            0,
            "a scratch whose bytes cannot be saved must not be committed"
        );
        assert_eq!(guest.injected, [Sysno::mmap, Sysno::fstat, Sysno::munmap]);
        assert!(
            scratch.holds_sentinel(start, readable),
            "no byte of a scratch that cannot be saved may be written"
        );
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[tokio::test]
    async fn inject_fstat_retries_in_a_transient_page_when_the_kernel_cannot_write_its_buffer() {
        // The scratch can be read, so its bytes can be saved, and the commit
        // writes nothing, as on the arena backends; but no byte of it can be
        // written, so the kernel's fstat faults.
        let (fd, inode) = open_file();
        let scratch = Pages::sentinel(1, 0, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);

        let result = tool.inject_fstat(&mut guest, fd).await;

        let stat = result.unwrap_or_else(|errno| {
            panic!("a buffer the kernel cannot write must not fail the caller: {errno}")
        });
        assert_eq!(stat.st_ino, inode);
        assert_eq!(
            guest.injected,
            [Sysno::fstat, Sysno::mmap, Sysno::fstat, Sysno::munmap]
        );
        assert_eq!(
            guest.fstat_guard_live,
            [true, false],
            "the stack guard must outlive the first fstat and be gone by the retry"
        );
        assert!(scratch.holds_sentinel(0, scratch.len));
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[tokio::test]
    async fn inject_fstat_propagates_a_commit_error_other_than_efault() {
        let (fd, _) = open_file();
        let scratch = Pages::sentinel(1, 1, 1);
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address, scratch.len, Some(Errno::ESRCH));

        let result = tool.inject_fstat(&mut guest, fd).await;

        assert!(
            matches!(result, Err(Errno::ESRCH)),
            "only EFAULT sends the fstat to a transient page: {}",
            fstat_outcome(&result)
        );
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
        assert!(scratch.holds_sentinel(0, scratch.len));
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[tokio::test]
    async fn faulting_stack_scratch_falls_back_to_a_transient_page() {
        let (fd, inode) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(false, false);

        let result = tool
            .add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(
            result,
            Ok(()),
            "a stack that cannot hold the fstat buffer must not fail the open"
        );
        assert_eq!(guest.injected, [Sysno::mmap, Sysno::fstat, Sysno::munmap]);
        let [(page, len)] = guest.mapped[..] else {
            panic!(
                "expected exactly one transient page, got {:?}",
                guest.mapped
            );
        };
        assert_eq!(
            guest.fstat_buffers,
            [page],
            "fstat must write into the transient page"
        );
        assert_eq!(
            guest.unmapped,
            [(page, len)],
            "the transient page must be unmapped, whole"
        );
        assert_eq!(recorded_inode(&guest, fd), Some(inode));
    }

    #[tokio::test]
    async fn descriptor_is_closed_when_no_scratch_can_be_found() {
        let (fd, _) = open_file();
        let (tool, mut guest) = ScriptedGuest::new(false, true);

        let result = tool
            .add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(result, Err(Errno::ENOMEM));
        assert_eq!(guest.injected, [Sysno::mmap, Sysno::close]);
        assert_eq!(
            guest.closed,
            [fd],
            "the descriptor must not stay open behind the error"
        );
        assert_eq!(
            guest.thread.with_detfd(fd, |_| ()),
            Err(Errno::EBADF),
            "a descriptor that failed registration must not be modeled"
        );
    }

    fn page_size() -> usize {
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap()
    }

    /// Fresh zero-filled pages of this process, of which only the first
    /// `writable` can be accessed; unmapped on drop.
    pub(crate) struct Pages {
        pub(crate) address: usize,
        pub(crate) len: usize,
    }

    impl Pages {
        pub(crate) fn map(count: usize, writable: usize) -> Self {
            let len = count * page_size();
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED, "scratch mmap");
            if writable > 0 {
                assert_eq!(
                    unsafe {
                        libc::mprotect(
                            address,
                            writable * page_size(),
                            libc::PROT_READ | libc::PROT_WRITE,
                        )
                    },
                    0,
                    "mprotect of the writable scratch pages"
                );
            }
            Self {
                address: address as usize,
                len,
            }
        }

        /// `count` fresh pages filled with `SENTINEL`, like guest stack memory
        /// that holds live data: the first `writable` can be read and
        /// written, the pages after them up to `readable` can only be read,
        /// and the rest cannot be accessed.
        pub(crate) fn sentinel(count: usize, writable: usize, readable: usize) -> Self {
            assert!(writable <= readable && readable <= count);
            let pages = Self::map(count, count);
            // SAFETY: the `len` bytes at `address` were just mapped writable.
            unsafe { std::ptr::write_bytes(pages.address as *mut u8, SENTINEL, pages.len) };
            for (first, end, prot) in [
                (writable, readable, libc::PROT_READ),
                (readable, count, libc::PROT_NONE),
            ] {
                if end > first {
                    assert_eq!(
                        unsafe {
                            libc::mprotect(
                                (pages.address + first * page_size()) as *mut libc::c_void,
                                (end - first) * page_size(),
                                prot,
                            )
                        },
                        0,
                        "mprotect of the sentinel pages"
                    );
                }
            }
            pages
        }

        /// Whether the `len` bytes at `offset`, which must be readable, all
        /// still hold `SENTINEL`.
        fn holds_sentinel(&self, offset: usize, len: usize) -> bool {
            assert!(offset + len <= self.len);
            unsafe { std::slice::from_raw_parts((self.address + offset) as *const u8, len) }
                .iter()
                .all(|byte| *byte == SENTINEL)
        }
    }

    /// What `Pages::sentinel` fills its pages with: a stand-in for live data
    /// the guest keeps below its red zone, which no step of a stat may
    /// leave changed.
    const SENTINEL: u8 = 0xa5;

    impl Drop for Pages {
        fn drop(&mut self) {
            assert_eq!(
                unsafe { libc::munmap(self.address as *mut libc::c_void, self.len) },
                0
            );
        }
    }

    /// `stat_guest_path`'s result, printable without `libc::stat: Debug`.
    fn outcome(result: &Result<Option<libc::stat>, Error>) -> String {
        match result {
            Ok(Some(stat)) => format!("Ok(Some(stat of inode {}))", stat.st_ino),
            Ok(None) => "Ok(None)".to_owned(),
            Err(error) => format!("Err({error:?})"),
        }
    }

    /// `(device, inode)` of `path` as this process's `stat` reports it.
    fn identity_of(path: &[u8]) -> (u64, u64) {
        let metadata = std::fs::metadata(std::ffi::OsStr::from_bytes(path)).unwrap();
        (metadata.dev(), metadata.ino())
    }

    /// The one transient page `stat_guest_path` mapped for a path of
    /// `path_len` bytes, after checking that it was the whole pages that hold
    /// the NUL-terminated path and an 8-aligned `struct stat`, and that it was
    /// unmapped whole.
    fn sole_transient_page(guest: &ScriptedGuest, path_len: usize) -> usize {
        let [(page, len)] = guest.mapped[..] else {
            panic!(
                "expected exactly one transient page, got {:?}",
                guest.mapped
            );
        };
        let needed = (path_len + 1).next_multiple_of(8) + std::mem::size_of::<libc::stat>();
        assert_eq!(
            len,
            needed.div_ceil(page_size()) * page_size(),
            "the transient mapping must be the whole pages that hold {needed} bytes"
        );
        assert_eq!(
            guest.unmapped,
            [(page, len)],
            "the transient page must be unmapped, whole"
        );
        page
    }

    /// Offset of the `struct stat` in the transient page, after the
    /// NUL-terminated path.
    fn transient_stat_offset(path_len: usize) -> usize {
        (path_len + 1).next_multiple_of(8)
    }

    #[tokio::test]
    async fn stat_guest_path_stats_the_path_in_its_scratch_and_restores_it() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().as_os_str().as_bytes();
        let metadata = file.as_file().metadata().unwrap();
        // A commit that writes zeros over the scratch, as on ptrace, and one
        // that writes nothing over the guest's bytes, so the path's NUL must
        // be staged with it.
        for commit_writes in [true, false] {
            let scratch = Pages::sentinel(1, 1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.commit_writes = commit_writes;

            let result = tool.stat_guest_path(&mut guest, path).await;

            let stat = result
                .expect("stat_guest_path failed")
                .expect("the guest's stat must be the answer");
            assert_eq!((stat.st_dev, stat.st_ino), (metadata.dev(), metadata.ino()));
            assert_eq!(guest.injected, [Sysno::newfstatat]);
            assert_eq!(
                guest.fstatat_paths,
                [path.to_vec()],
                "fstatat must name the whole path, NUL-terminated (commit_writes: \
                 {commit_writes})"
            );
            assert_eq!(
                guest.fstatat_guard_live,
                [true],
                "the stack guard must outlive the injected fstatat: backends whose \
                 scratch is an arena free it when the guard drops"
            );
            assert_eq!(
                guest.unguarded_scratch_writes(),
                0,
                "both buffers must be written back before the stack guard drops"
            );
            assert!(
                guest.mapped.is_empty(),
                "a stack scratch that works needs no transient page: {:?}",
                guest.mapped
            );
            assert!(
                scratch.holds_sentinel(0, scratch.len),
                "the guest's bytes under the path and the stat must be put back \
                 (commit_writes: {commit_writes})"
            );
        }
    }

    #[tokio::test]
    async fn stat_guest_path_restores_its_scratch_when_the_guests_stat_fails() {
        let directory = tempfile::tempdir().unwrap();
        let absent = directory.path().join("absent");
        let path = absent.as_os_str().as_bytes();
        let scratch = Pages::sentinel(1, 1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.commit_writes = true;

        let result = tool.stat_guest_path(&mut guest, path).await;

        assert!(
            matches!(result, Ok(None)),
            "a path the guest cannot stat has no answer: {}",
            outcome(&result)
        );
        assert_eq!(
            guest.injected,
            [Sysno::newfstatat],
            "a failed stat is the guest's answer, not a reason to retry"
        );
        assert_eq!(guest.fstatat_paths, [path.to_vec()]);
        assert_eq!(guest.fstatat_guard_live, [true]);
        assert_eq!(guest.unguarded_scratch_writes(), 0);
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the guest's bytes must be put back after a failed stat"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_refuses_a_path_containing_nul() {
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);

        let result = tool.stat_guest_path(&mut guest, b"/\0/").await;

        assert!(
            matches!(result, Ok(None)),
            "a path with a NUL in it names no file: {}",
            outcome(&result)
        );
        assert_eq!(guest.commits.load(Ordering::SeqCst), 0);
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    #[tokio::test]
    async fn stat_guest_path_uses_a_transient_page_when_its_scratch_is_too_small() {
        // One byte short of the path buffer and the stat buffer.
        let needed = 512 + std::mem::size_of::<libc::stat>();
        let scratch = Pages::sentinel(1, 1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, needed - 1, None);

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        let stat = result
            .expect("a scratch too small for the stat must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(
            guest.commits.load(Ordering::SeqCst),
            0,
            "a scratch without room must not be committed"
        );
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::newfstatat, Sysno::munmap]
        );
        let page = sole_transient_page(&guest, 1);
        assert_eq!(guest.fstatat_paths, [b"/".to_vec()]);
        assert_eq!(guest.fstatat_buffers, [page + transient_stat_offset(1)]);
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the stack scratch was used"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_uses_a_transient_page_when_its_scratch_cannot_be_committed() {
        let scratch = Pages::sentinel(1, 1, 1);
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address, scratch.len, Some(Errno::EFAULT));

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        let stat = result
            .expect("a scratch the guest cannot hold must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(
            guest.commits.load(Ordering::SeqCst),
            1,
            "the fallback must come from the commit, not from the capacity check"
        );
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::newfstatat, Sysno::munmap]
        );
        let page = sole_transient_page(&guest, 1);
        assert_eq!(guest.fstatat_paths, [b"/".to_vec()]);
        assert_eq!(
            guest.fstatat_buffers,
            [page + transient_stat_offset(1)],
            "the stat must be written into the transient page"
        );
        assert_eq!(guest.fstatat_guard_live, [false]);
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "nothing may be left changed in the stack scratch"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_restores_what_a_failed_commit_wrote() {
        // As on ptrace, the commit writes the whole scratch in one write from
        // its start: here its first 256 bytes, on a writable page, before it
        // faults on the read-only page after them.
        let written = 256;
        let scratch = Pages::sentinel(2, 1, 2);
        let start = page_size() - written;
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address + start, scratch.len - start, None);
        guest.commit_writes = true;

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        let stat = result
            .expect("a scratch the guest cannot hold must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(guest.commits.load(Ordering::SeqCst), 1);
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::newfstatat, Sysno::munmap],
            "no stat may be injected into a scratch whose commit failed"
        );
        let page = sole_transient_page(&guest, 1);
        assert_eq!(guest.fstatat_buffers, [page + transient_stat_offset(1)]);
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the bytes the failed commit wrote must be put back"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_does_not_use_a_scratch_it_cannot_save() {
        // The path buffer fills the last 512 bytes of a writable page and the
        // stat buffer starts the inaccessible page after it, whose bytes
        // cannot be read and so could not be put back.
        let path_capacity = 512;
        let scratch = Pages::sentinel(2, 1, 1);
        let path_buffer = page_size() - path_capacity;
        let (tool, mut guest) = ScriptedGuest::with_scratch(
            scratch.address + path_buffer,
            scratch.len - path_buffer,
            None,
        );
        guest.commit_writes = true;

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        let stat = result
            .expect("a scratch that cannot be saved must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(
            guest.commits.load(Ordering::SeqCst),
            0,
            "a scratch whose bytes cannot be saved must not be committed"
        );
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::newfstatat, Sysno::munmap]
        );
        let page = sole_transient_page(&guest, 1);
        assert_eq!(guest.fstatat_buffers, [page + transient_stat_offset(1)]);
        assert!(
            scratch.holds_sentinel(path_buffer, path_capacity),
            "no byte of a scratch that cannot be saved may be written"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_propagates_a_commit_error_other_than_efault() {
        let scratch = Pages::sentinel(1, 1, 1);
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address, scratch.len, Some(Errno::ESRCH));

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        assert!(
            matches!(result, Err(Error::Errno(Errno::ESRCH))),
            "only EFAULT sends the stat to a transient page: {}",
            outcome(&result)
        );
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
        assert!(scratch.holds_sentinel(0, scratch.len));
    }

    #[tokio::test]
    async fn stat_guest_path_uses_a_transient_page_when_its_path_cannot_be_staged() {
        // This scratch can be read, so its bytes can be saved, but no byte of
        // it can be written.
        let scratch = Pages::sentinel(1, 0, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        let stat = result
            .expect("a path the scratch cannot hold must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(guest.commits.load(Ordering::SeqCst), 1);
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::newfstatat, Sysno::munmap],
            "no stat may be injected into the stack scratch without its path"
        );
        let page = sole_transient_page(&guest, 1);
        assert_eq!(guest.fstatat_buffers, [page + transient_stat_offset(1)]);
        assert!(scratch.holds_sentinel(0, scratch.len));
    }

    #[tokio::test]
    async fn stat_guest_path_restores_a_partly_staged_path_before_using_a_transient_page() {
        // The path buffer starts four bytes before the end of a writable page
        // and the page after it can only be read, so staging a five-byte path
        // writes four bytes and then faults.
        let path = b"/////";
        let staged = 4;
        let scratch = Pages::sentinel(2, 1, 2);
        let path_buffer = page_size() - staged;
        let (tool, mut guest) = ScriptedGuest::with_scratch(
            scratch.address + path_buffer,
            scratch.len - path_buffer,
            None,
        );

        let result = tool.stat_guest_path(&mut guest, path).await;

        let stat = result
            .expect("a path the scratch cannot hold must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::newfstatat, Sysno::munmap]
        );
        let page = sole_transient_page(&guest, path.len());
        assert_eq!(guest.fstatat_paths, [path.to_vec()]);
        assert_eq!(
            guest.fstatat_buffers,
            [page + transient_stat_offset(path.len())]
        );
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the guest's bytes under the staged prefix of the path must be put back"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_retries_in_a_transient_page_when_its_stat_buffer_faults() {
        // The path buffer fills the last 512 bytes of a writable page and the
        // stat buffer starts the page after it, which can only be read.
        let path_capacity = 512;
        let scratch = Pages::sentinel(2, 1, 2);
        let path_buffer = page_size() - path_capacity;
        let (tool, mut guest) = ScriptedGuest::with_scratch(
            scratch.address + path_buffer,
            scratch.len - path_buffer,
            None,
        );

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        let stat = result
            .expect("a stat buffer the scratch cannot hold must not fail the caller")
            .expect("the transient page must give the guest's answer");
        assert_eq!((stat.st_dev, stat.st_ino), identity_of(b"/"));
        assert_eq!(
            guest.injected,
            [
                Sysno::newfstatat,
                Sysno::mmap,
                Sysno::newfstatat,
                Sysno::munmap
            ]
        );
        let page = sole_transient_page(&guest, 1);
        assert_eq!(
            guest.fstatat_buffers,
            [
                scratch.address + page_size(),
                page + transient_stat_offset(1)
            ],
            "this test needs the first stat buffer at the start of the read-only \
             page, and the retry in the transient page"
        );
        assert_eq!(
            guest.fstatat_guard_live,
            [true, false],
            "the stack guard must outlive the first fstatat and be gone by the retry"
        );
        assert!(
            scratch.holds_sentinel(0, scratch.len),
            "the guest's bytes under the staged path must be put back"
        );
    }

    #[tokio::test]
    async fn stat_guest_path_stages_a_long_path_in_a_transient_page() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let name = file.path().as_os_str().as_bytes();
        let expected = identity_of(name);
        // 600 bytes needs one page; 4000 bytes needs two.
        for len in [600, 4000] {
            // Extra leading slashes resolve to the same file.
            let mut path = vec![b'/'; len - name.len()];
            path.extend_from_slice(name);
            assert_eq!(path.len(), len);
            let scratch = Pages::sentinel(1, 1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);

            let result = tool.stat_guest_path(&mut guest, &path).await;

            let stat = result
                .expect("stat_guest_path failed")
                .unwrap_or_else(|| panic!("a {len}-byte path must be resolved"));
            assert_eq!((stat.st_dev, stat.st_ino), expected);
            assert_eq!(
                guest.commits.load(Ordering::SeqCst),
                0,
                "a {len}-byte path must not be staged in the stack scratch"
            );
            assert_eq!(
                guest.injected,
                [Sysno::mmap, Sysno::newfstatat, Sysno::munmap]
            );
            let page = sole_transient_page(&guest, len);
            assert_eq!(
                guest.fstatat_paths,
                [path.clone()],
                "fstatat must name the whole {len}-byte path, NUL-terminated"
            );
            assert_eq!(guest.fstatat_buffers, [page + transient_stat_offset(len)]);
            assert!(
                scratch.holds_sentinel(0, scratch.len),
                "the stack scratch was used"
            );
        }
    }

    #[tokio::test]
    async fn stat_guest_path_refuses_when_its_transient_page_cannot_be_mapped() {
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address, scratch.len, Some(Errno::EFAULT));
        guest.mmap_fails = true;

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        assert!(
            refused_with(&result, Errno::ENOMEM),
            "a page Detcore cannot map says nothing about the path, so it must be a tool \
             error naming the errno, not `Ok(None)`: {}",
            outcome(&result)
        );
        assert_eq!(guest.injected, [Sysno::mmap]);
        assert!(guest.unmapped.is_empty(), "unmapped {:?}", guest.unmapped);
    }

    /// Whether `result` is the tool error `stat_guest_path` refuses with
    /// when it could not ask, naming `errno`.
    fn refused_with(result: &Result<Option<libc::stat>, Error>, errno: Errno) -> bool {
        matches!(result, Err(Error::Tool(error)) if error.to_string().contains(&errno.to_string()))
    }

    // A stat the guest's filesystem answers with "no such file" (or one of
    // the other answers about the path) is `None`; any other errno from the
    // injected stat, such as `ENOMEM`, says nothing about the path and is an
    // error, in the stack scratch and in the transient page alike.
    #[tokio::test]
    async fn stat_guest_path_refuses_when_the_guests_stat_cannot_be_asked() {
        let long = [b"/".as_slice(), &[b'x'; 600]].concat();
        for (path, scratch_works) in [(b"/".to_vec(), true), (long, false)] {
            for (errno, answered) in [
                (Errno::ENOENT, true),
                (Errno::ENOTDIR, true),
                (Errno::ELOOP, true),
                (Errno::ENAMETOOLONG, true),
                (Errno::EACCES, true),
                (Errno::ENOMEM, false),
                (Errno::EIO, false),
                (Errno::EOVERFLOW, false),
            ] {
                let scratch = Pages::sentinel(1, 1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.fstatat_error = Some(errno);

                let result = tool.stat_guest_path(&mut guest, &path).await;

                if answered {
                    assert!(
                        matches!(result, Ok(None)),
                        "{errno} from the guest's stat is its answer about the path: {}",
                        outcome(&result)
                    );
                } else {
                    assert!(
                        refused_with(&result, errno),
                        "{errno} from the guest's stat is not an answer about the path, so \
                         it must be a tool error naming it, never `Ok(None)`: {}",
                        outcome(&result)
                    );
                }
                assert_eq!(
                    guest.fstatat_paths.len(),
                    1,
                    "one stat, in the {} (errno {errno})",
                    if scratch_works {
                        "stack scratch"
                    } else {
                        "transient page"
                    }
                );
                assert!(scratch.holds_sentinel(0, scratch.len));
                if !scratch_works {
                    sole_transient_page(&guest, path.len());
                }
            }
        }
    }

    #[tokio::test]
    async fn stat_guest_path_propagates_a_write_error_other_than_efault() {
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.write_error = Some(Errno::EIO);

        let result = tool.stat_guest_path(&mut guest, b"/").await;

        assert!(
            refused_with(&result, Errno::EIO),
            "only EFAULT sends the stat to a transient page; any other failure is a tool \
             error naming the errno: {}",
            outcome(&result)
        );
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    /// `(device, inode)` that `fstat` reports for `fd`.
    fn fd_identity(fd: RawFd) -> RawInode {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
        RawInode::new(stat.st_dev, stat.st_ino)
    }

    /// `handle_mmap` of a page of `fd`, returning the mapping's start once the
    /// test has unmapped it.
    async fn map_and_unmap(tool: &Detcore, guest: &mut ScriptedGuest, fd: RawFd) -> usize {
        let len = page_size();
        let result = tool
            .handle_mmap(
                guest,
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_READ)
                    .with_flags(MapFlags::MAP_PRIVATE)
                    .with_fd(fd)
                    .with_offset(0),
            )
            .await;
        let start = usize::try_from(result.expect("the file mmap must succeed")).unwrap();
        assert_eq!(guest.file_mapped, [(start, len)]);
        assert_eq!(unsafe { libc::munmap(start as *mut libc::c_void, len) }, 0);
        start
    }

    /// A descriptor that the guest's table records as a dup of inherited
    /// stdout -- `prog > file` -- whose cached stat is the tracer's `fstat(0)`
    /// stand-in rather than the file's own.
    fn stdio_backed_by_a_file(guest: &mut ScriptedGuest) -> (RawFd, RawInode) {
        let (fd, _) = open_file();
        guest
            .thread
            .dup_fd(libc::STDOUT_FILENO, fd, OFlag::empty())
            .unwrap();
        let identity = fd_identity(fd);
        let cached = guest
            .thread
            .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.raw_inode()))
            .unwrap();
        assert_ne!(
            cached,
            Some(identity),
            "precondition: the cached stdio stat must not already be the file's"
        );
        (fd, identity)
    }

    #[tokio::test]
    async fn handle_mmap_records_a_stdio_descriptor_by_a_real_fstat() {
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        let (fd, identity) = stdio_backed_by_a_file(&mut guest);

        let start = map_and_unmap(&tool, &mut guest, fd).await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::fstat],
            "the identity must come from an fstat after the mapping succeeds"
        );
        assert_eq!(
            guest.thread.mapped_file_at(start),
            Some(identity),
            "a stdio descriptor's mapping must be recorded with the file's own identity, \
             not the cached fstat(0) stand-in"
        );
    }

    #[tokio::test]
    async fn handle_mmap_refuses_when_it_cannot_ask_a_stdio_identity() {
        // No writable stack scratch and no transient page: the fstat has
        // nowhere to put its buffer, so Detcore cannot ask the file's identity
        // after the guest's mapping succeeded.
        let (tool, mut guest) = ScriptedGuest::new(false, true);
        let (fd, _) = stdio_backed_by_a_file(&mut guest);
        let len = page_size();

        let result = tool
            .handle_mmap(
                &mut guest,
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_READ)
                    .with_flags(MapFlags::MAP_PRIVATE)
                    .with_fd(fd)
                    .with_offset(0),
            )
            .await;
        close_unless_detcore_did(&guest, fd);

        assert!(
            matches!(result, Err(Error::Tool(_))),
            "an identity Detcore could not ask must be a tool error, not a mapping \
             recorded without it: {result:?}"
        );
        assert_eq!(
            guest.injected,
            [Sysno::mmap, Sysno::mmap, Sysno::munmap],
            "the file mapping, the failed transient page for the fstat, then the undo \
             of the file mapping"
        );
        let &[(start, mapped_len)] = guest.file_mapped.as_slice() else {
            panic!("expected one file mapping, got {:?}", guest.file_mapped);
        };
        assert_eq!(
            guest.unmapped,
            [(start, mapped_len)],
            "the refused mapping must not stay in the guest's address space"
        );
        assert_eq!(
            guest.thread.mapped_file_at(start),
            None,
            "a refused mapping must leave nothing recorded"
        );
    }

    #[tokio::test]
    async fn getdents_device_lookup_refuses_what_it_could_not_ask() {
        // A descriptor that is not open is the kernel's own answer, which the
        // getdents would report too.
        // (-1 rather than a closed number, which a parallel test could reuse.)
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        let result: Result<u64, Error> = tool.directory_device(&mut guest, -1).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EBADF))),
            "a descriptor that is not open must be EBADF to the guest: {result:?}"
        );

        // An open descriptor Detcore does not track, with no writable stack
        // scratch and no transient page: the fstat could not be asked, which
        // is not an answer the guest's getdents may report as its own.
        let (tool, mut guest) = ScriptedGuest::new(false, true);
        let (fd, _) = open_file();
        let result: Result<u64, Error> = tool.directory_device(&mut guest, fd).await;
        close_unless_detcore_did(&guest, fd);
        assert_eq!(
            guest.injected,
            [Sysno::mmap],
            "only the failed transient page for the fstat"
        );
        assert!(
            matches!(result, Err(Error::Tool(_))),
            "a device lookup Detcore could not ask must be a tool error, not an errno \
             for the guest's getdents: {result:?}"
        );
    }

    // `prog > dir`: the dup's cached stat is the `fstat(0)` stand-in
    // `setup_stdio` gives every inherited stdio descriptor, so a getdents
    // through it keyed every entry on stdin's device, and the maps, fdinfo
    // and `/proc/<pid>/fd` views matched stdin's file as stdout
    // (https://github.com/rrnewton/hermit/issues/3307).
    #[tokio::test]
    async fn stdio_descriptor_identity_is_its_own_fstat_cached_on_its_description() {
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        let (fd, identity) = stdio_backed_by_a_file(&mut guest);

        let device: Result<u64, Error> = tool.directory_device(&mut guest, fd).await;
        let again = tool.descriptor_identity_stat(&mut guest, fd).await;
        // In the guest's table descriptor 1 shares the dup's open file
        // description, as every alias of one does, so it finds the identity
        // the dup's lookup cached.
        let alias = tool
            .descriptor_identity_stat(&mut guest, libc::STDOUT_FILENO)
            .await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(
            device.ok(),
            Some(identity.dev),
            "a getdents through a stdio dup must key its entries on the file's own device"
        );
        assert_eq!(
            again.ok().flatten().map(|stat| stat.raw_inode()),
            Some(identity),
            "a stdio descriptor's identity must be the file's own fstat, not the stand-in"
        );
        assert_eq!(
            alias.ok().flatten().map(|stat| stat.raw_inode()),
            Some(identity),
            "every alias of the description must find the one cached identity"
        );
        assert_eq!(
            guest.injected,
            [Sysno::fstat],
            "one fstat for the description; every later lookup, through any alias, \
             must use the cached identity"
        );
    }

    #[tokio::test]
    async fn stdio_descriptor_identity_keeps_the_stand_in_without_virtualized_metadata() {
        // `hermit record` and `hermit replay`: at replay an fstat would
        // describe the replayer's descriptor rather than the recorded one.
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        guest.config.virtualize_metadata = false;
        let (fd, _) = stdio_backed_by_a_file(&mut guest);
        let stand_in = guest
            .thread
            .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.raw_inode()))
            .unwrap();

        let identity = tool.descriptor_identity_stat(&mut guest, fd).await;
        close_unless_detcore_did(&guest, fd);

        assert!(
            stand_in.is_some(),
            "precondition: the dup carries the stand-in"
        );
        assert_eq!(
            identity.ok().flatten().map(|stat| stat.raw_inode()),
            stand_in
        );
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    #[tokio::test]
    async fn stdio_descriptor_identity_refuses_what_it_could_not_ask() {
        // No writable stack scratch and no transient page. Answering with the
        // stand-in here would key the descriptor on stdin's file again.
        let (tool, mut guest) = ScriptedGuest::new(false, true);
        let (fd, _) = stdio_backed_by_a_file(&mut guest);

        let result = tool.descriptor_identity_stat(&mut guest, fd).await;
        let cached = guest
            .thread
            .with_detfd(fd, |detfd| detfd.identity_stat().is_some())
            .unwrap();
        close_unless_detcore_did(&guest, fd);

        assert_typed_refusal("stdio identity", result.map(|_| ()), Errno::ENOMEM);
        assert!(!cached, "a refused lookup must cache nothing");
        assert_eq!(
            guest.injected,
            [Sysno::mmap],
            "only the failed transient page for the fstat"
        );
    }

    /// One byte written through `fd` by the write-family call `name`, through
    /// that call's own handler; `sendfile` copies it from `source`.
    async fn write_one_byte(
        tool: &Detcore,
        guest: &mut ScriptedGuest,
        name: &str,
        fd: RawFd,
        source: RawFd,
    ) -> Result<i64, Error> {
        static BYTE: [u8; 1] = *b"x";
        let buf = Addr::from_ptr(BYTE.as_ptr());
        let iovec = [libc::iovec {
            iov_base: BYTE.as_ptr() as *mut libc::c_void,
            iov_len: BYTE.len(),
        }];
        let iov = Addr::from_ptr(iovec.as_ptr());
        match name {
            "write" => {
                let call = syscalls::Write::new().with_fd(fd).with_buf(buf).with_len(1);
                tool.handle_write(guest, call).await
            }
            "pwrite64" => {
                let call = syscalls::Pwrite64::new()
                    .with_fd(fd)
                    .with_buf(buf)
                    .with_len(1)
                    .with_offset(0);
                tool.handle_pwrite64(guest, call).await
            }
            "writev" => {
                let call = syscalls::Writev::new()
                    .with_fd(fd)
                    .with_iov(iov)
                    .with_len(1);
                tool.handle_writev(guest, call).await
            }
            "pwritev" => {
                let call = syscalls::Pwritev::new()
                    .with_fd(fd)
                    .with_iov(iov)
                    .with_iov_len(1)
                    .with_pos_l(0)
                    .with_pos_h(0);
                tool.handle_pwritev(guest, call).await
            }
            "pwritev2" => {
                let call = syscalls::Pwritev2::new()
                    .with_fd(fd)
                    .with_iov(iov)
                    .with_iov_len(1)
                    .with_pos_l(0)
                    .with_pos_h(0)
                    .with_flags(0);
                tool.handle_pwritev2(guest, call).await
            }
            "sendfile" => {
                let call = syscalls::Sendfile::new()
                    .with_out_fd(fd)
                    .with_in_fd(source)
                    .with_offset(None)
                    .with_count(1);
                tool.handle_sendfile(guest, call).await
            }
            other => panic!("not a write-family call: {other}"),
        }
    }

    // A write bumps the virtual mtime of the file it changes, which a later
    // `stat` of that file looks up under the file's own (device, inode). The
    // cached stat of an inherited stdio descriptor, and of every dup of one,
    // is the `fstat(0)` stand-in all three share, so with stdin and stdout on
    // different devices that share a raw inode number,
    // `stat(B); write(1, ...); stat(B)` touched stdin's identity and the
    // guest saw no change. The scripted fstat gives the stdout dup the
    // stand-in's inode number on another device, a pair the test process
    // cannot make. Each write-family call writes twice: the second must use
    // the identity the first cached on the open file description.
    #[tokio::test]
    async fn a_stdio_write_touches_the_descriptors_own_file_not_the_stand_in() {
        let mut observed = Vec::new();
        let mut expected = Vec::new();
        for name in [
            "write", "pwrite64", "writev", "pwritev", "pwritev2", "sendfile",
        ] {
            let (tool, mut guest) = ScriptedGuest::new(true, false);
            guest.config.sequentialize_threads = false;
            guest.answers_touch_file = true;
            let (source, _) = open_file();
            assert_eq!(
                unsafe { libc::pwrite(source, b"xx".as_ptr().cast(), 2, 0) },
                2
            );
            tool.add_fd(&mut guest, source, OFlag::O_RDONLY, FdType::Regular)
                .await
                .expect("precondition: Detcore tracks the sendfile source");
            let (fd, _) = stdio_backed_by_a_file(&mut guest);
            let stand_in = guest
                .thread
                .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.raw_inode()))
                .unwrap()
                .expect("precondition: the dup carries the stand-in");
            let mut answer: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::fstat(fd, &mut answer) }, 0);
            answer.st_dev = stand_in.dev ^ 1;
            answer.st_ino = stand_in.ino;
            guest.fstat_answers.push((fd, answer));
            let own = RawInode::new(answer.st_dev, answer.st_ino);
            guest.injected.clear();

            let first = write_one_byte(&tool, &mut guest, name, fd, source).await;
            let second = write_one_byte(&tool, &mut guest, name, fd, source).await;
            close_unless_detcore_did(&guest, fd);
            close_unless_detcore_did(&guest, source);

            let fstats = guest
                .injected
                .iter()
                .filter(|&&sysno| sysno == Sysno::fstat)
                .count();
            let touched = guest.touched.lock().unwrap().clone();
            observed.push((name, first.ok(), second.ok(), touched, fstats));
            // Both writes succeed and bump the file the descriptor names; one
            // fstat for the description, whose identity the second write
            // finds cached.
            expected.push((name, Some(1), Some(1), vec![own, own], 1));
        }
        assert_eq!(
            observed, expected,
            "(call, first write, second write, identities touched, fstats injected): a \
             write through a stdio dup must bump the mtime of the file the descriptor \
             names, not of the stand-in"
        );
    }

    // With no writable stack scratch and no transient page, Detcore cannot
    // ask a stdio descriptor's own identity. A write must then refuse before
    // the kernel writes anything: touching the stand-in instead would bump
    // stdin's file again. Without virtualized metadata (`hermit record` and
    // `hermit replay`) no mtime is bumped and nothing is asked.
    #[tokio::test]
    async fn a_stdio_write_refuses_before_writing_when_it_cannot_ask_the_identity() {
        let writes = [
            Sysno::write,
            Sysno::pwrite64,
            Sysno::writev,
            Sysno::pwritev,
            Sysno::pwritev2,
            Sysno::sendfile,
        ];
        for virtualize_metadata in [true, false] {
            for name in [
                "write", "pwrite64", "writev", "pwritev", "pwritev2", "sendfile",
            ] {
                let (tool, mut guest) = ScriptedGuest::new(false, true);
                guest.config.sequentialize_threads = false;
                guest.config.virtualize_metadata = virtualize_metadata;
                guest.answers_touch_file = true;
                let (source, _) = open_file();
                assert_eq!(
                    unsafe { libc::pwrite(source, b"x".as_ptr().cast(), 1, 0) },
                    1
                );
                // Tracked directly: with no stack scratch, the fstat
                // `add_fd` injects could not be asked either.
                let mut source_stat: libc::stat = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::fstat(source, &mut source_stat) }, 0);
                guest
                    .thread
                    .add_fd(
                        source,
                        OFlag::O_RDONLY,
                        FdType::Regular,
                        Some(DetStat::from(source_stat)),
                    )
                    .unwrap();
                let (fd, _) = stdio_backed_by_a_file(&mut guest);

                let result = write_one_byte(&tool, &mut guest, name, fd, source).await;
                let length = unsafe {
                    let mut stat: libc::stat = std::mem::zeroed();
                    assert_eq!(libc::fstat(fd, &mut stat), 0);
                    stat.st_size
                };
                close_unless_detcore_did(&guest, fd);
                close_unless_detcore_did(&guest, source);

                if virtualize_metadata {
                    assert_typed_refusal(name, result.map(|_| ()), Errno::ENOMEM);
                    assert_eq!(
                        guest.injected,
                        [Sysno::mmap],
                        "{name}: only the failed transient page for the fstat; the write \
                         must not reach the kernel"
                    );
                    assert_eq!(length, 0, "{name}: the refused write wrote nothing");
                } else {
                    assert_eq!(result.ok(), Some(1), "{name}: the write succeeds");
                    assert!(
                        guest.injected.iter().all(|sysno| writes.contains(sysno)),
                        "{name}: nothing but the write is injected: {:?}",
                        guest.injected
                    );
                    assert_eq!(length, 1, "{name}: the write reached the file");
                }
                assert!(
                    guest.touched.lock().unwrap().is_empty(),
                    "{name}: no mtime is bumped for a refused write or without \
                     virtualized metadata"
                );
            }
        }
    }

    // Each inherited descriptor's own identity, which
    // `fixed_stdio_identity_stats` narrows to the table the maps and
    // `/proc/<pid>/fd` views match stdio objects against: here the test
    // process's own descriptors 0 to 2, where the stand-in gave all three
    // the identity of its stdin.
    #[tokio::test]
    async fn inherited_stdio_identities_are_each_descriptors_own() {
        let expected: Vec<Option<RawInode>> = (libc::STDIN_FILENO..=libc::STDERR_FILENO)
            .map(|fd| {
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                (unsafe { libc::fstat(fd, &mut stat) } == 0)
                    .then(|| RawInode::new(stat.st_dev, stat.st_ino))
            })
            .collect();
        assert!(
            expected.iter().all(Option::is_some),
            "precondition: the test process's descriptors 0 to 2 are open: {expected:?}"
        );
        let (tool, mut guest) = ScriptedGuest::new(true, false);

        let first = tool.inherited_stdio_identity_stats(&mut guest).await;
        let injected_first = std::mem::take(&mut guest.injected);
        let second = tool.inherited_stdio_identity_stats(&mut guest).await;

        let identities = |stats: Result<[Option<crate::stat::DetStat>; 3], Error>| {
            stats
                .expect("the test process's descriptors can be asked")
                .map(|stat| stat.map(|stat| stat.raw_inode()))
                .to_vec()
        };
        assert_eq!(identities(first), expected);
        assert_eq!(identities(second), expected);
        assert_eq!(
            injected_first,
            [Sysno::fstat, Sysno::fstat, Sysno::fstat],
            "one fstat per inherited descriptor"
        );
        assert!(
            guest.injected.is_empty(),
            "a second lookup must use the cached identities: injected {:?}",
            guest.injected
        );

        // Without virtualize_metadata the stand-ins, as before, and no fstat.
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        guest.config.virtualize_metadata = false;
        let stand_ins: Vec<Option<RawInode>> = (libc::STDIN_FILENO..=libc::STDERR_FILENO)
            .map(|fd| {
                guest
                    .thread
                    .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.raw_inode()))
                    .ok()
                    .flatten()
            })
            .collect();
        assert!(
            stand_ins.iter().all(Option::is_some),
            "precondition: the scripted guest tracks descriptors 0 to 2: {stand_ins:?}"
        );
        let recorded = tool.inherited_stdio_identity_stats(&mut guest).await;
        assert_eq!(identities(recorded), stand_ins);
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    // Only the object the stand-in describes keeps a fixed stdio inode in
    // the maps and `/proc/<pid>/fd` views. Here stdin and stderr are that
    // object, as with one terminal on both, and stdout is another, as with
    // `prog > file`: its entry is dropped, so those views name it by the
    // pooled inode its aliases and its path report rather than by
    // descriptor 1's fixed inode (round-11 review of
    // https://github.com/rrnewton/hermit/pull/3255). The descriptors' own
    // identities are scripted, since the test process's descriptors 0 to 2
    // are whatever the test runner gave it.
    #[tokio::test]
    async fn fixed_stdio_identities_keep_only_the_stand_in_object() {
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        assert!(guest.config.virtualize_metadata);
        let stand_ins: Vec<Option<RawInode>> = (libc::STDIN_FILENO..=libc::STDERR_FILENO)
            .map(|fd| {
                guest
                    .thread
                    .with_detfd(fd, |detfd| detfd.stat().map(|stat| stat.raw_inode()))
                    .ok()
                    .flatten()
            })
            .collect();
        let stand_in =
            stand_ins[0].expect("precondition: the scripted guest's stdin has a stand-in");
        assert_eq!(
            stand_ins,
            [Some(stand_in); 3],
            "precondition: one stand-in for all three descriptors"
        );
        let (file, _) = open_file();
        let mut answer: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(file, &mut answer) }, 0);
        assert_eq!(unsafe { libc::close(file) }, 0);
        let other = RawInode::new(stand_in.dev ^ 1, stand_in.ino);
        for (fd, identity) in [
            (libc::STDIN_FILENO, stand_in),
            (libc::STDOUT_FILENO, other),
            (libc::STDERR_FILENO, stand_in),
        ] {
            answer.st_dev = identity.dev;
            answer.st_ino = identity.ino;
            guest.fstat_answers.push((fd, answer));
        }

        let fixed = tool
            .fixed_stdio_identity_stats(&mut guest)
            .await
            .expect("the scripted descriptors can be asked")
            .map(|stat| stat.map(|stat| stat.raw_inode()));

        assert_eq!(
            fixed,
            [Some(stand_in), None, Some(stand_in)],
            "stdout's own identity is not the stand-in, so it must not keep a fixed inode"
        );
        assert_eq!(
            guest.injected,
            [Sysno::fstat, Sysno::fstat, Sysno::fstat],
            "nothing is asked beyond each inherited descriptor's own identity"
        );

        // Without virtualize_metadata every identity is the stand-in, so all
        // three keep their fixed inodes, and nothing is asked.
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        guest.config.virtualize_metadata = false;
        let recorded = tool
            .fixed_stdio_identity_stats(&mut guest)
            .await
            .expect("without virtualized metadata nothing is asked")
            .map(|stat| stat.map(|stat| stat.raw_inode()));
        assert_eq!(recorded, [Some(stand_in); 3]);
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    #[tokio::test]
    async fn handle_mmap_records_no_stdio_identity_without_virtualized_metadata() {
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        guest.config.virtualize_metadata = false;
        let (fd, _) = stdio_backed_by_a_file(&mut guest);

        let start = map_and_unmap(&tool, &mut guest, fd).await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(
            guest.injected,
            [Sysno::mmap],
            "no fstat without virtualize_metadata"
        );
        assert_eq!(guest.thread.mapped_file_at(start), None);
    }

    #[tokio::test]
    async fn handle_mmap_records_an_ordinary_descriptor_from_its_cached_stat() {
        let (fd, _) = open_file();
        let identity = fd_identity(fd);
        let (tool, mut guest) = ScriptedGuest::new(true, false);
        tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await
            .unwrap();
        guest.injected.clear();

        let start = map_and_unmap(&tool, &mut guest, fd).await;
        close_unless_detcore_did(&guest, fd);

        assert_eq!(
            guest.injected,
            [Sysno::mmap],
            "an ordinary descriptor's mapping must cost no extra syscall"
        );
        assert_eq!(guest.thread.mapped_file_at(start), Some(identity));
    }

    /// A maps header's device for the tests below: not the device `stat`
    /// reports for any file they use, as on btrfs, where maps prints the
    /// superblock's device and `stat` the subvolume's.
    const HEADER_DEVICE: u64 = 0xdead_0001;
    /// The device a seeded mapping record names: neither the header's nor
    /// any file's.
    const RECORD_DEVICE: u64 = 0xdead_0002;
    /// The start of a seeded mapping record. Nothing is mapped there: the
    /// record alone is under test.
    const RECORDED_START: usize = 0x7e00_0000_0000;

    /// While alive, `guest_mountinfo` reads this mount table instead of the
    /// guest's, and an injected `statmount` answers from the superblocks
    /// set with it, if any, instead of running.
    struct ScriptedMountinfo;

    impl ScriptedMountinfo {
        fn set(mountinfo: impl Into<Vec<u8>>) -> Self {
            SCRIPTED_MOUNTINFO.with(|scripted| *scripted.borrow_mut() = Some(mountinfo.into()));
            Self
        }

        /// ... and an injected `statmount` of unique mount id `id` reports
        /// superblock `device` for each `(id, device)`, and `ENOENT` for
        /// any other id.
        fn with_superblocks(self, superblocks: Vec<(u64, u64)>) -> Self {
            SCRIPTED_SUPERBLOCKS.with(|scripted| *scripted.borrow_mut() = Some(superblocks));
            self
        }

        /// The mount of `path`, as the test process's own `statx` reports
        /// it, on a superblock with `device`: the only row of the mount
        /// table, and the only mount `statmount` reports.
        fn proving(path: &Path, device: u64) -> Self {
            Self::set(mount_row(mount_id_of(path), device)).with_superblocks(
                unique_mount_id_of(path)
                    .map(|id| (id, device))
                    .into_iter()
                    .collect(),
            )
        }
    }

    impl Drop for ScriptedMountinfo {
        fn drop(&mut self) {
            SCRIPTED_MOUNTINFO.with(|scripted| *scripted.borrow_mut() = None);
            SCRIPTED_SUPERBLOCKS.with(|scripted| *scripted.borrow_mut() = None);
        }
    }

    /// The unique mount id of `path` from the test process's own `statx`,
    /// or `None` on a kernel before Linux 6.8, which does not report one.
    fn unique_mount_id_of(path: &Path) -> Option<u64> {
        let raw_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: a NUL-terminated path and a buffer of the size statx writes.
        let raw = unsafe {
            libc::syscall(
                libc::SYS_statx,
                libc::AT_FDCWD,
                raw_path.as_ptr(),
                0,
                STATX_MNT_ID_UNIQUE,
                statx.as_mut_ptr(),
            )
        };
        assert_eq!(
            raw,
            0,
            "statx of {path:?} failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: statx succeeded, so it wrote the buffer.
        let statx = unsafe { statx.assume_init() };
        (statx.stx_mask & STATX_MNT_ID_UNIQUE != 0).then_some(statx.stx_mnt_id)
    }

    /// The calls a device proof injects on this kernel: a page, the
    /// `statx`, then `statmount` when the kernel reports unique mount ids,
    /// and the page's unmapping. Before Linux 6.8 the proof reads the
    /// guest's mountinfo instead, which injects nothing.
    fn proof_calls() -> Vec<Sysno> {
        if unique_mount_id_of(Path::new("/")).is_some() {
            vec![Sysno::mmap, Sysno::statx, Sysno::statmount, Sysno::munmap]
        } else {
            vec![Sysno::mmap, Sysno::statx, Sysno::munmap]
        }
    }

    /// The mount id of `path`, from the test process's own `statx`.
    fn mount_id_of(path: &Path) -> u64 {
        let raw_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: a NUL-terminated path and a buffer of the size statx writes.
        let raw = unsafe {
            libc::syscall(
                libc::SYS_statx,
                libc::AT_FDCWD,
                raw_path.as_ptr(),
                0,
                StatxMask::STATX_MNT_ID.bits(),
                statx.as_mut_ptr(),
            )
        };
        assert_eq!(
            raw,
            0,
            "statx of {path:?} failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: statx succeeded, so it wrote the buffer.
        let statx = unsafe { statx.assume_init() };
        assert_ne!(
            statx.stx_mask & StatxMask::STATX_MNT_ID.bits(),
            0,
            "precondition: this kernel reports mount ids"
        );
        statx.stx_mnt_id
    }

    /// A mountinfo row for mount `mount_id` on a superblock with `device`.
    fn mount_row(mount_id: u64, device: u64) -> String {
        format!(
            "{mount_id} 1 {}:{} / /scripted rw,relatime shared:1 - scripted scripted rw\n",
            libc::major(device),
            libc::minor(device)
        )
    }

    /// A live file, its raw identity, and a maps header naming it by path
    /// with its inode and `HEADER_DEVICE`.
    fn mapped_path() -> (tempfile::NamedTempFile, RawInode, crate::procfs::MappingKey) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let metadata = file.as_file().metadata().unwrap();
        let identity = RawInode::new(metadata.dev(), metadata.ino());
        assert!(
            ![HEADER_DEVICE, RECORD_DEVICE].contains(&identity.dev),
            "precondition: the file's device {:#x} must differ from the header's and the record's",
            identity.dev
        );
        let key = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: identity.ino,
            pathname: file.path().to_str().unwrap().to_owned(),
        };
        (file, identity, key)
    }

    // A record whose inode is not the header's describes another file -- it is
    // stale (see `mapping_recorded_identity`) -- so the line has no record and
    // the path decides. Without the inode check the record would key the line
    // on that other file.
    #[tokio::test]
    async fn mapping_recorded_identity_discards_a_record_with_another_inode() {
        let (file, identity, key) = mapped_path();
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        let stale = RawInode::new(RECORD_DEVICE, identity.ino + 1);
        guest.thread.map_file(RECORDED_START, page_size(), stale);

        assert_eq!(
            tool.mapping_recorded_identity(&guest, &key, RECORDED_START, MappingSubject::Reader),
            None,
            "a record with inode {} must not key a line whose header has inode {}",
            stale.ino,
            key.inode
        );
        let result = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Reader)
            .await;

        assert_eq!(
            result.expect("mapping_stat_identity failed"),
            identity,
            "a record with inode {} must not key a header with inode {}; the path's stat must",
            stale.ino,
            key.inode
        );
        assert_eq!(
            guest.injected,
            [&[Sysno::newfstatat][..], &proof_calls()].concat()
        );
        assert_eq!(guest.fstatat_paths, [key.pathname.as_bytes().to_vec()]);
        assert_eq!(guest.statx_paths, [key.pathname.as_bytes().to_vec()]);
    }

    // The split pair of https://github.com/rrnewton/hermit/pull/3255's sixth
    // review: a path whose `stat` has the header's inode on another device,
    // as every file on btrfs does. Its identity is the one `fstat` gives the
    // file only when the mount `statx` names is on the header's superblock;
    // the same inode number on another filesystem keeps the header's pair,
    // and so does a mount the guest cannot see. Each case runs twice: with
    // the unique mount id and `statmount` (Linux 6.8 and later), and as a
    // kernel before 6.8 answers, through the guest's mountinfo.
    #[tokio::test]
    async fn a_path_with_the_headers_inode_needs_the_headers_superblock() {
        let (file, identity, key) = mapped_path();
        let header = RawInode::new(HEADER_DEVICE, key.inode);
        let scratch = Pages::map(1, 1);
        let listed = mount_id_of(file.path());
        let unique = unique_mount_id_of(file.path())
            .expect("precondition: this kernel reports unique mount ids (Linux 6.8)");
        let unseen = (u64::MAX - 1, u64::MAX - 1);
        for ((listed, unique), device, expected, why) in [
            (
                (listed, unique),
                RECORD_DEVICE,
                header,
                "another superblock: the same inode number names another file",
            ),
            (
                unseen,
                HEADER_DEVICE,
                header,
                "a mount the guest cannot see proves nothing",
            ),
            (
                (listed, unique),
                HEADER_DEVICE,
                identity,
                "the header's superblock: the identity fstat gives the file",
            ),
        ] {
            for before_6_8 in [false, true] {
                let _mounts = ScriptedMountinfo::set(mount_row(listed, device))
                    .with_superblocks(vec![(unique, device)]);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                // As on ptrace and KVM, where the mountinfo fallback is made.
                guest.config.tool_opens_outside_guest_descriptor_table = true;
                guest.statx_without_unique_mount_id = before_6_8;
                let resolved = tool
                    .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
                    .await;
                assert_eq!(
                    resolved.expect("mapping_stat_identity failed"),
                    expected,
                    "{why} (before Linux 6.8: {before_6_8})"
                );
                let (proof, asked): (&[Sysno], Vec<u64>) = if before_6_8 {
                    (&[Sysno::mmap, Sysno::statx, Sysno::munmap], vec![])
                } else {
                    (
                        &[Sysno::mmap, Sysno::statx, Sysno::statmount, Sysno::munmap],
                        vec![file_unique(&file)],
                    )
                };
                assert_eq!(
                    guest.injected,
                    [&[Sysno::newfstatat][..], proof].concat(),
                    "{why} (before Linux 6.8: {before_6_8})"
                );
                assert_eq!(
                    guest.statmount_mounts, asked,
                    "{why}: statmount must ask for the unique id statx reported"
                );
            }
        }
    }

    // https://github.com/rrnewton/hermit/pull/3255's ninth review: where a
    // file Detcore opens for itself takes a number in the guest's descriptor
    // table (DBT, SaBRe, LiteInst), the proof must not fall back to reading
    // the guest's mountinfo. That read holds a guest descriptor while it
    // runs, so another guest thread's `open` or `SCM_RIGHTS` receipt could be
    // given another number, and at the guest's descriptor limit the read
    // fails. Both ways into the fallback -- a kernel before Linux 6.8, and a
    // `statmount` that answers `ENOSYS` -- refuse with the typed refusal,
    // read no mountinfo, and unmap the page. The scripted mountinfo proves
    // the header's superblock, so a read would show as a proof.
    #[tokio::test]
    async fn a_maps_proof_refuses_rather_than_read_mountinfo_in_the_guests_table() {
        let (file, _identity, key) = mapped_path();
        let scratch = Pages::map(1, 1);
        let mut outcomes = Vec::new();
        for (before_6_8, statmount_error, proof) in [
            (true, None, &[Sysno::mmap, Sysno::statx, Sysno::munmap][..]),
            (
                false,
                Some(Errno::ENOSYS),
                &[
                    Sysno::mmap,
                    Sysno::statx,
                    Sysno::statmount,
                    Sysno::statx,
                    Sysno::munmap,
                ][..],
            ),
        ] {
            let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            let shares_table = !guest.config.tool_opens_outside_guest_descriptor_table;
            guest.statx_without_unique_mount_id = before_6_8;
            guest.statmount_error = statmount_error;
            let reads = MOUNTINFO_READS.with(|reads| reads.get());
            let resolved = tool
                .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
                .await;
            let reads = MOUNTINFO_READS.with(|reads| reads.get()) - reads;
            outcomes.push((
                before_6_8,
                shares_table,
                resolved,
                reads,
                guest.injected,
                proof,
            ));
        }
        for (before_6_8, shares_table, resolved, reads, injected, proof) in outcomes {
            assert!(
                shares_table,
                "precondition: the default config assumes the guest's descriptor table"
            );
            let refused = resolved
                .as_ref()
                .err()
                .and_then(IdentityLookupRefused::carried_by);
            assert!(
                refused.is_some_and(|refused| refused.to_string().contains("mountinfo")),
                "before Linux 6.8: {before_6_8}: the fallback must refuse with an \
                 IdentityLookupRefused naming mountinfo: {resolved:?}"
            );
            assert_eq!(
                reads, 0,
                "before Linux 6.8: {before_6_8}: no mountinfo may be read"
            );
            assert_eq!(
                injected,
                [&[Sysno::newfstatat][..], proof].concat(),
                "before Linux 6.8: {before_6_8}"
            );
        }
    }

    /// The unique mount id of `file`'s mount.
    fn file_unique(file: &tempfile::NamedTempFile) -> u64 {
        unique_mount_id_of(file.path()).expect("precondition: unique mount ids")
    }

    // `statmount` answers that decide nothing, and the proof's response to
    // each: a mount outside the guest's namespace (`ENOENT`) or root
    // (`EPERM`) is the guest's own configuration, so no proof; `ENOSYS`, as
    // a seccomp policy can answer, falls back to the guest's mountinfo after
    // a second `statx` for the reusable id, where the launcher allows that
    // read (ptrace and KVM; see the test above); anything else could not be asked
    // and refuses, as does an answer without the superblock it was asked
    // for. The page is unmapped on every path.
    #[tokio::test]
    async fn a_statmount_that_decides_nothing_proves_nothing_falls_back_or_refuses() {
        let (file, identity, key) = mapped_path();
        let header = RawInode::new(HEADER_DEVICE, key.inode);
        let scratch = Pages::map(1, 1);
        let unique = file_unique(&file);
        let asked = [
            Sysno::newfstatat,
            Sysno::mmap,
            Sysno::statx,
            Sysno::statmount,
        ];
        for (errno, expected) in [(Errno::ENOENT, header), (Errno::EPERM, header)] {
            let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.statmount_error = Some(errno);
            let resolved = tool
                .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
                .await;
            assert_eq!(
                resolved.expect("mapping_stat_identity failed"),
                expected,
                "{errno}"
            );
            assert_eq!(
                guest.injected,
                [&asked[..], &[Sysno::munmap]].concat(),
                "{errno}"
            );
            assert_eq!(guest.statmount_mounts, [unique], "{errno}");
        }

        // ENOSYS: the reusable id, then the guest's mountinfo, proves it, as
        // on ptrace and KVM, where the fallback is made.
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.config.tool_opens_outside_guest_descriptor_table = true;
        guest.statmount_error = Some(Errno::ENOSYS);
        let resolved = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
            .await;
        assert_eq!(resolved.expect("mapping_stat_identity failed"), identity);
        assert_eq!(
            guest.injected,
            [&asked[..], &[Sysno::statx, Sysno::munmap]].concat()
        );
        assert_eq!(guest.statx_paths.len(), 2);
        drop(_mounts);

        // ENOSYS with a mountinfo row on another superblock: no proof.
        let _mounts = ScriptedMountinfo::set(mount_row(mount_id_of(file.path()), RECORD_DEVICE))
            .with_superblocks(vec![(unique, HEADER_DEVICE)]);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.config.tool_opens_outside_guest_descriptor_table = true;
        guest.statmount_error = Some(Errno::ENOSYS);
        let resolved = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
            .await;
        assert_eq!(
            resolved.expect("mapping_stat_identity failed"),
            header,
            "after ENOSYS the mountinfo row decides, not the statmount table"
        );
        drop(_mounts);

        for (errno, omits, cause) in [
            (Some(Errno::EINVAL), false, Errno::EINVAL.to_string()),
            (Some(Errno::EFAULT), false, Errno::EFAULT.to_string()),
            (None, true, "no superblock".to_owned()),
        ] {
            let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.statmount_error = errno;
            guest.statmount_omits_superblock = omits;
            let refused = tool
                .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
                .await;
            assert!(
                matches!(&refused, Err(Error::Tool(error))
                    if error.to_string().contains("statmount")
                        && error.to_string().contains(&cause)),
                "a statmount that could not be asked must refuse ({cause}): {refused:?}"
            );
            assert_eq!(
                guest.injected,
                [&asked[..], &[Sysno::munmap]].concat(),
                "{cause}"
            );
        }

        // ESRCH: the guest is gone, which every injection reports as itself.
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.statmount_error = Some(Errno::ESRCH);
        let gone = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
            .await;
        assert!(
            matches!(gone, Err(Error::Errno(Errno::ESRCH))),
            "a guest that is gone must report ESRCH: {gone:?}"
        );
    }

    // Against the test host's real kernel, with nothing scripted: the
    // superblock device `statmount` reports for the mount `statx` names is
    // the device the test process's own mountinfo lists for that mount,
    // which is the device a maps line prints. This checks the offsets of
    // `struct mnt_id_req` and `struct statmount` against the kernel.
    #[tokio::test]
    async fn statmount_reports_the_superblock_mountinfo_lists() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mountinfo = std::fs::read("/proc/self/mountinfo").unwrap();
        for path in [file.path(), Path::new("/proc/self/exe"), Path::new("/")] {
            unique_mount_id_of(path)
                .expect("precondition: this kernel reports unique mount ids (Linux 6.8)");
            let metadata = std::fs::metadata(path).unwrap();
            // SAFETY: an all-zero stat is a valid value.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            stat.st_dev = metadata.dev();
            stat.st_ino = metadata.ino();
            let scratch = Pages::map(1, 1);
            let (_tool, mut guest) =
                ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            let superblock =
                Detcore::guest_superblock_of(&mut guest, path.as_os_str().as_bytes(), &stat)
                    .await
                    .expect("guest_superblock_of failed");
            let listed = mountinfo_superblock_device(&mountinfo, mount_id_of(path))
                .expect("malformed mountinfo")
                .expect("the test process's mountinfo lists its own mount");
            assert_eq!(
                superblock,
                MountSuperblock::Device(listed),
                "{path:?}: statmount's superblock must be the one mountinfo lists"
            );
            assert_eq!(
                guest.injected,
                [Sysno::mmap, Sysno::statx, Sysno::statmount, Sysno::munmap],
                "{path:?}"
            );
            assert_eq!(guest.mapped.len(), 1);
            assert_eq!(
                guest.unmapped, guest.mapped,
                "{path:?}: the page is unmapped"
            );
        }
    }

    // A device proof that could not be asked refuses, as a stat that could
    // not be asked does: without it the header's pair would key a file whose
    // identity is fstat's on a host where the page can be mapped.
    #[tokio::test]
    async fn a_device_proof_that_cannot_be_asked_refuses() {
        let (file, _identity, key) = mapped_path();
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.mmap_fails = true;

        let refused = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
            .await;

        assert!(
            matches!(&refused, Err(Error::Tool(error))
                if error.to_string().contains(&Errno::ENOMEM.to_string())),
            "a device proof that could not be asked must refuse: {refused:?}"
        );
        assert_eq!(guest.injected, [Sysno::newfstatat, Sysno::mmap]);
    }

    // On the test host's own filesystem, through the kernel's real maps line,
    // statx and mountinfo: a mapped file's line resolves to the identity
    // fstat gives it. On btrfs (and overlayfs) the line's device is not
    // stat's, so this runs the mount proof; elsewhere they agree at once.
    #[tokio::test]
    async fn a_real_maps_line_resolves_to_its_files_fstat_identity() {
        use std::os::fd::AsRawFd;

        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file()
            .set_len(u64::try_from(page_size()).unwrap())
            .unwrap();
        let metadata = file.as_file().metadata().unwrap();
        let identity = RawInode::new(metadata.dev(), metadata.ino());
        // SAFETY: a fresh shared read-only mapping of the whole file.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size(),
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_file().as_raw_fd(),
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let prefix = format!("{:x}-", address as usize);
        let line = maps
            .lines()
            .find(|line| line.starts_with(&prefix))
            .expect("no maps line for the mapping")
            .to_owned();
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (major, minor) = fields[3].split_once(':').unwrap();
        let header_device = libc::makedev(
            u32::from_str_radix(major, 16).unwrap(),
            u32::from_str_radix(minor, 16).unwrap(),
        );
        let key = crate::procfs::MappingKey {
            device: header_device,
            inode: fields[4].parse().unwrap(),
            pathname: fields[5..].join(" "),
        };
        assert_eq!(key.inode, identity.ino, "{line}");
        // Only the fallback reads this, with the pid-1 guest standing in.
        let _mounts = ScriptedMountinfo::set(std::fs::read("/proc/self/mountinfo").unwrap());
        let scratch = Pages::map(1, 1);
        let mut outcomes = Vec::new();
        for before_6_8 in [false, true] {
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            // As on ptrace and KVM, where the mountinfo fallback is made.
            guest.config.tool_opens_outside_guest_descriptor_table = true;
            guest.statx_without_unique_mount_id = before_6_8;
            let resolved = tool
                .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
                .await;
            outcomes.push((before_6_8, resolved, guest.injected));
        }
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(address, page_size()) };

        for (before_6_8, resolved, injected) in outcomes {
            eprintln!(
                "maps device {header_device:#x}, stat device {:#x}; before 6.8: {before_6_8}; \
                 injected {injected:?}",
                identity.dev
            );
            assert_eq!(
                resolved.expect("mapping_stat_identity failed"),
                identity,
                "{line}: a mapped file's line must take the identity fstat gives it \
                 (before Linux 6.8: {before_6_8})"
            );
            let proof = if header_device == identity.dev {
                vec![]
            } else if before_6_8 {
                vec![Sysno::mmap, Sysno::statx, Sysno::munmap]
            } else {
                proof_calls()
            };
            assert_eq!(injected, [&[Sysno::newfstatat][..], &proof].concat());
        }
    }

    // Control for `mapping_recorded_identity_discards_a_record_with_another_inode`:
    // a record with the header's inode is the line's identity, and no stat is
    // made.
    #[tokio::test]
    async fn mapping_recorded_identity_uses_a_record_with_the_headers_inode() {
        let (_file, identity, key) = mapped_path();
        let scratch = Pages::map(1, 1);
        let (tool, guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        let recorded = RawInode::new(RECORD_DEVICE, identity.ino);
        guest.thread.map_file(RECORDED_START, page_size(), recorded);

        let result =
            tool.mapping_recorded_identity(&guest, &key, RECORDED_START, MappingSubject::Reader);

        assert_eq!(result, Some(recorded));
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    // A lookup that could not be asked must refuse rather than step to the
    // next source or to the header: here neither the stack scratch nor a
    // transient page is available, as when the guest has used up its address
    // space or its mapping count. On a host where the page can be mapped the
    // path's stat keys the line, so a fallback would let host resources
    // choose the identity. A path the guest's filesystem reports missing is an
    // answer, and still falls back to the header.
    #[tokio::test]
    async fn mapping_stat_identity_refuses_when_a_lookup_cannot_be_asked() {
        let (_file, _identity, key) = mapped_path();
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address, scratch.len, Some(Errno::EFAULT));
        guest.mmap_fails = true;

        let refused = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Reader)
            .await;

        assert!(
            matches!(&refused, Err(Error::Tool(error))
                if error.to_string().contains(&Errno::ENOMEM.to_string())),
            "a lookup that could not be asked must refuse, never key the line on the \
             header or another source: {refused:?}"
        );
        assert_eq!(
            guest.injected,
            [Sysno::mmap],
            "nothing may be asked after the refusal"
        );

        // Control: a missing path is the filesystem's answer.
        let directory = tempfile::tempdir().unwrap();
        let missing = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: key.inode,
            pathname: directory.path().join("absent").to_str().unwrap().to_owned(),
        };
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        let fallback = tool
            .mapping_stat_identity(&mut guest, &missing, MappingSubject::Unknown)
            .await;
        assert_eq!(
            fallback.expect("a missing path must not be an error"),
            RawInode::new(HEADER_DEVICE, key.inode)
        );
        assert_eq!(guest.injected, [Sysno::newfstatat]);
    }

    /// Asserts that `result` is a refusal carrying [`IdentityLookupRefused`]
    /// whose message names `errno`.
    fn assert_typed_refusal(path: &str, result: Result<(), Error>, errno: Errno) {
        let refused = result
            .as_ref()
            .err()
            .and_then(IdentityLookupRefused::carried_by);
        assert!(
            refused.is_some_and(|refused| refused.to_string().contains(&errno.to_string())),
            "the {path} path's refusal must reach the dispatcher as an IdentityLookupRefused \
             naming {errno}: {result:?}"
        );
    }

    // A refusal must reach Detcore's dispatcher, and the DBT glue, as the
    // typed `IdentityLookupRefused`, because that type is what stops the
    // refusing process: an untyped tool error is still EIO to the guest on
    // DBT. One refusal from each path that asks the guest before it keys an
    // identity, each with neither a stack scratch nor a transient page to ask
    // in: a `maps` snapshot, through `mint_mapping_identities` as the read
    // handler calls it; the stdio identity of an `mmap`; and the directory
    // device of a `getdents64`, which is resolved before the real call.
    #[tokio::test]
    async fn refusals_from_maps_mmap_and_getdents_reach_the_dispatcher_typed() {
        let (_file, _identity, key) = mapped_path();
        let line = format!(
            "{:x}-{:x} r--p 00000000 {:02x}:{:02x} {} {}",
            RECORDED_START,
            RECORDED_START + page_size(),
            libc::major(key.device),
            libc::minor(key.device),
            key.inode,
            key.pathname
        );
        assert_eq!(
            crate::procfs::mapping_header_key(&line),
            Some(key.clone()),
            "precondition: the scripted maps line names the file"
        );
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) =
            ScriptedGuest::with_scratch(scratch.address, scratch.len, Some(Errno::EFAULT));
        guest.mmap_fails = true;
        let maps = crate::procfs::mint_mapping_identities(
            line.as_bytes(),
            &BTreeMap::new(),
            &mut GuestMappingMinter::new(&tool, &mut guest, MappingSubject::Unknown),
        )
        .await;
        assert_typed_refusal("maps", maps.map(|_| ()), Errno::ENOMEM);
        assert_eq!(guest.injected, [Sysno::mmap], "maps: only the failed page");

        let (tool, mut guest) = ScriptedGuest::new(false, true);
        let (fd, _) = stdio_backed_by_a_file(&mut guest);
        let mmap = tool
            .handle_mmap(
                &mut guest,
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(page_size())
                    .with_prot(ProtFlags::PROT_READ)
                    .with_flags(MapFlags::MAP_PRIVATE)
                    .with_fd(fd)
                    .with_offset(0),
            )
            .await;
        close_unless_detcore_did(&guest, fd);
        assert_typed_refusal("mmap", mmap.map(|_| ()), Errno::ENOMEM);

        let (tool, mut guest) = ScriptedGuest::new(false, true);
        let (fd, _) = open_file();
        let getdents = tool
            .handle_getdents64(
                &mut guest,
                syscalls::Getdents64::new()
                    .with_fd(u32::try_from(fd).unwrap())
                    .with_dirent(AddrMut::from_raw(scratch.address))
                    .with_count(u32::try_from(page_size()).unwrap()),
            )
            .await;
        close_unless_detcore_did(&guest, fd);
        assert_typed_refusal("getdents64", getdents.map(|_| ()), Errno::ENOMEM);
        assert_eq!(
            guest.injected,
            [Sysno::mmap],
            "getdents64: the refusal comes before the real call"
        );
    }

    /// A directory holding a subdirectory `covered` and the files `lower`,
    /// `plain` and `vanished`, open as a descriptor the test owns, with each
    /// entry's inode and the directory's device.
    fn directory_with_entries() -> (tempfile::TempDir, RawFd, BTreeMap<&'static str, u64>, u64) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("covered")).unwrap();
        for name in ["lower", "plain", "vanished"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let inodes = ["covered", "lower", "plain", "vanished"]
            .into_iter()
            .map(|name| {
                let metadata = std::fs::symlink_metadata(dir.path().join(name)).unwrap();
                (name, metadata.ino())
            })
            .collect();
        let device = std::fs::metadata(dir.path()).unwrap().dev();
        let fd = std::fs::File::open(dir.path()).unwrap().into_raw_fd();
        (dir, fd, inodes, device)
    }

    /// A `stat` answer for inode `ino` on device `dev`.
    fn stat_answer(dev: u64, ino: u64, mode: libc::mode_t) -> libc::stat {
        let mut answer: libc::stat = unsafe { std::mem::zeroed() };
        answer.st_dev = dev;
        answer.st_ino = ino;
        answer.st_mode = mode;
        answer
    }

    /// The guest's `getdents64` of `fd` into a fresh one-page buffer.
    async fn getdents64_of(
        tool: &Detcore,
        guest: &mut ScriptedGuest,
        fd: RawFd,
    ) -> Result<i64, Error> {
        let mut buffer = vec![0u8; page_size()];
        tool.handle_getdents64(
            guest,
            syscalls::Getdents64::new()
                .with_fd(u32::try_from(fd).unwrap())
                .with_dirent(AddrMut::from_raw(buffer.as_mut_ptr() as usize))
                .with_count(u32::try_from(buffer.len()).unwrap()),
        )
        .await
    }

    /// [`getdents64_of`], and the name and `d_ino` of each record the call
    /// left in the guest's buffer: what the guest sees.
    async fn getdents64_records_of(
        tool: &Detcore,
        guest: &mut ScriptedGuest,
        fd: RawFd,
    ) -> (Result<i64, Error>, Vec<(Vec<u8>, u64)>) {
        let mut buffer = vec![0u8; page_size()];
        let read = tool
            .handle_getdents64(
                guest,
                syscalls::Getdents64::new()
                    .with_fd(u32::try_from(fd).unwrap())
                    .with_dirent(AddrMut::from_raw(buffer.as_mut_ptr() as usize))
                    .with_count(u32::try_from(buffer.len()).unwrap()),
            )
            .await;
        let records = match &read {
            Ok(len) => DirentFormat::Dirent64
                .parse(&buffer[..usize::try_from(*len).unwrap()])
                .expect("the guest's buffer holds whole records")
                .into_iter()
                .map(|entry| (entry.name, entry.ino))
                .collect(),
            Err(_) => Vec::new(),
        };
        (read, records)
    }

    /// The `d_ino` that `records` (see [`getdents64_records_of`]) list for
    /// `name`.
    fn listed_inode(records: &[(Vec<u8>, u64)], name: &[u8]) -> u64 {
        records
            .iter()
            .find(|(listed, _)| listed == name)
            .map(|(_, ino)| *ino)
            .unwrap_or_else(|| panic!("{:?} is listed", String::from_utf8_lossy(name)))
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, round 9,
    // F5. On overlayfs with `xino=off` over layers on different filesystems,
    // a file's `d_ino` is its `st_ino`, but `stat` reports the layer's
    // device, not the directory's, so keying every entry on the directory's
    // device gave the guest a `d_ino` other than the `st_ino` it reads for
    // the same file. A mount point's entry, a btrfs subvolume's and an
    // overlay directory's carry a `d_ino` other than their `st_ino` natively
    // too. Each entry is keyed on the device its own `lstat` reports when
    // that `lstat` reports the entry's `d_ino`, and on the directory's device
    // otherwise. `lower` stands in for the overlay file, `covered` for a
    // mount point, `vanished` for an entry removed after the read, and
    // `plain` is asked for real. Both getdents paths: a descriptor Detcore
    // does not track sorts one kernel buffer, and a tracked one is served
    // from its directory stream. `.` is the open directory, which reports
    // the overlay's device on every overlay, so it is not asked. `lower`,
    // the first file below 2^32 in sorted order, is asked before any entry
    // is keyed, and on its layer's device shows that this overlay reports a
    // layer's device for every non-directory, so every entry but `.` is
    // asked, `lower` keyed on the answer it already gave rather than asked
    // twice, and a tracked descriptor keeps that.
    #[tokio::test]
    async fn getdents_keys_each_entry_on_the_device_its_own_lstat_reports() {
        for tracked in [false, true] {
            let (_dir, fd, inodes, device) = directory_with_entries();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            // Overlayfs, where every entry but `.` is asked until one tells.
            guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
            if tracked {
                tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                    .await
                    .expect("precondition: Detcore tracks the directory");
            }
            // Below 2^32 on any host, as an upper-layer number is.
            guest.getdents_inode_answers = vec![(b"lower".to_vec(), SCRIPTED_FILE_INODE)];
            guest.fstatat_answers = vec![
                (
                    b"lower".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        SCRIPTED_FILE_INODE,
                        libc::S_IFREG | 0o644,
                    )),
                ),
                (
                    b"covered".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        inodes["covered"] + 1,
                        libc::S_IFDIR | 0o755,
                    )),
                ),
                (b"vanished".to_vec(), Err(Errno::ENOENT)),
            ];
            let asked_before = guest.fstatat_paths.len();
            let keyed_before = guest.determinized.lock().unwrap().len();

            let returned = getdents64_of(&tool, &mut guest, fd).await;
            let cached = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.directory_entry_lookup());
            close_unless_detcore_did(&guest, fd);

            assert!(
                returned.as_ref().is_ok_and(|len| *len > 0),
                "tracked {tracked}: {returned:?}"
            );
            let keyed = guest.determinized.lock().unwrap()[keyed_before..].to_vec();
            assert_eq!(
                keyed.get(2..),
                Some(
                    &[
                        RawInode::new(device, inodes["covered"]),
                        RawInode::new(OTHER_DEVICE, SCRIPTED_FILE_INODE),
                        RawInode::new(device, inodes["plain"]),
                        RawInode::new(device, inodes["vanished"]),
                    ][..]
                ),
                "tracked {tracked}: covered, lower, plain and vanished, after . and .."
            );
            let entry_lstat = (fd, AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_NO_AUTOMOUNT);
            let asked: Vec<(&[u8], (RawFd, AtFlags))> = guest.fstatat_paths[asked_before..]
                .iter()
                .map(Vec::as_slice)
                .zip(guest.fstatat_starts[asked_before..].iter().copied())
                .collect();
            assert_eq!(
                asked,
                [&b"lower"[..], b"..", b"covered", b"plain", b"vanished"]
                    .map(|name| (name, entry_lstat)),
                "tracked {tracked}: one lstat of each entry but ., relative to the descriptor, \
                 lower first to settle the overlay"
            );
            let expected_cache = if tracked {
                Ok(Some(EntryLookup::OverlayLayerDevices))
            } else {
                Err(Errno::EBADF)
            };
            assert_eq!(
                cached, expected_cache,
                "tracked {tracked}: a tracked descriptor keeps what lower showed"
            );
        }
    }

    /// A `d_ino` below 2^32 that a scripted getdents64 gives a file.
    const SCRIPTED_FILE_INODE: u64 = 300;

    // The coordinator's measurement of the gate above on overlayfs, where
    // Podman's pinned root keeps `/tmp`: asking every entry still cost the
    // 3000-entry `readdir_order` test 5.65 GiB under the test allocator,
    // over the validation node's 4 GiB cap. An overlay with every layer on
    // one filesystem, or one that folds its layers' inode numbers, reports
    // its own device for each non-directory below 2^32. The first file whose
    // `lstat` shows that (`lower`; the directory `covered`, which could not,
    // is not asked for it) is asked before any entry is keyed, and leaves
    // only `..` and the entries whose `d_ino` is at least 2^32 asked for
    // their keys. `covered`, a mount point whose `lstat` reports another
    // device with its `d_ino`, is keyed on the directory's device in every
    // read of this overlay; when it was read before `lower` had decided
    // anything, it was keyed on the other device, unlike in the rewound read
    // below (https://github.com/rrnewton/hermit/pull/3255). `plain`, whose
    // `lstat` would report another device, is not asked and keeps the
    // directory's, while `vanished`, carrying a folded lower-layer number, is
    // asked and keyed on the device its `lstat` reports. `covered` and
    // `plain` answer as a mount from another filesystem with a coinciding
    // number would, the first class of the second known gap at
    // `Detcore::directory_entry_identity`: keeping the directory's device
    // shows both that they are not asked and that the gap stays as
    // documented, the same in every read. A tracked descriptor keeps what
    // was settled, and a second read after a rewind uses it, even where
    // `lower` would now tell otherwise. Both getdents paths.
    #[tokio::test]
    async fn getdents_on_an_overlay_that_keeps_its_files_asks_only_wide_entries_after_one() {
        const FOLDED_LOWER_INODE: u64 = (1 << 33) | 302;
        // Below 2^32 whatever the host's numbers, so that the second read
        // does not ask it.
        const COVERED_INODE: u64 = 299;
        for tracked in [false, true] {
            let (_dir, fd, _, device) = directory_with_entries();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
            if tracked {
                tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                    .await
                    .expect("precondition: Detcore tracks the directory");
            }
            guest.getdents_inode_answers = vec![
                (b"..".to_vec(), SCRIPTED_PARENT_INODE),
                (b"covered".to_vec(), COVERED_INODE),
                (b"lower".to_vec(), SCRIPTED_FILE_INODE),
                (b"plain".to_vec(), SCRIPTED_FILE_INODE + 1),
                (b"vanished".to_vec(), FOLDED_LOWER_INODE),
            ];
            guest.fstatat_answers = vec![
                (
                    b"..".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        SCRIPTED_PARENT_INODE,
                        libc::S_IFDIR | 0o755,
                    )),
                ),
                (
                    b"covered".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        COVERED_INODE,
                        libc::S_IFDIR | 0o755,
                    )),
                ),
                (
                    b"lower".to_vec(),
                    Ok(stat_answer(
                        device,
                        SCRIPTED_FILE_INODE,
                        libc::S_IFREG | 0o644,
                    )),
                ),
                (
                    b"plain".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        SCRIPTED_FILE_INODE + 1,
                        libc::S_IFREG | 0o644,
                    )),
                ),
                (
                    b"vanished".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        FOLDED_LOWER_INODE,
                        libc::S_IFREG | 0o644,
                    )),
                ),
            ];
            let asked_before = guest.fstatat_paths.len();
            let keyed_before = guest.determinized.lock().unwrap().len();

            let returned = getdents64_of(&tool, &mut guest, fd).await;
            let cached = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.directory_entry_lookup());
            let asked_after_first = guest.fstatat_paths.len();
            let keyed_after_first = guest.determinized.lock().unwrap().len();
            let mut rewound_read = None;
            if tracked {
                // `lower` would now teach that the overlay reports a layer's
                // device, which would ask `covered`, `lower` and every entry
                // after it, were the stored answer not used.
                let lower = guest
                    .fstatat_answers
                    .iter_mut()
                    .find(|(name, _)| name == b"lower")
                    .unwrap();
                lower.1 = Ok(stat_answer(
                    OTHER_DEVICE,
                    SCRIPTED_FILE_INODE,
                    libc::S_IFREG | 0o644,
                ));
                let rewound = tool
                    .handle_lseek(
                        &mut guest,
                        syscalls::Lseek::new()
                            .with_fd(fd)
                            .with_offset(0)
                            .with_whence(Whence::SEEK_SET),
                    )
                    .await;
                assert_eq!(rewound.ok(), Some(0), "the stream rewinds");
                rewound_read = Some(getdents64_of(&tool, &mut guest, fd).await);
            }
            close_unless_detcore_did(&guest, fd);

            assert!(
                returned.as_ref().is_ok_and(|len| *len > 0),
                "tracked {tracked}: {returned:?}"
            );
            let keyed =
                guest.determinized.lock().unwrap()[keyed_before..keyed_after_first].to_vec();
            let first_keys = [
                RawInode::new(OTHER_DEVICE, SCRIPTED_PARENT_INODE),
                RawInode::new(device, COVERED_INODE),
                RawInode::new(device, SCRIPTED_FILE_INODE),
                RawInode::new(device, SCRIPTED_FILE_INODE + 1),
                RawInode::new(OTHER_DEVICE, FOLDED_LOWER_INODE),
            ];
            assert_eq!(
                keyed.get(1..),
                Some(&first_keys[..]),
                "tracked {tracked}: .., covered, lower, plain and vanished, after ."
            );
            assert_eq!(
                entries_asked(&guest, fd, asked_before)[..asked_after_first - asked_before],
                ["lower", "..", "vanished"],
                "tracked {tracked}: lower settles the overlay, then only .. and the entries \
                 at or above 2^32 are asked"
            );
            let expected_cache = if tracked {
                Ok(Some(EntryLookup::OverlayOwnDevice))
            } else {
                Err(Errno::EBADF)
            };
            assert_eq!(
                cached, expected_cache,
                "tracked {tracked}: a tracked descriptor keeps what lower showed"
            );
            if let Some(again) = rewound_read {
                assert!(again.as_ref().is_ok_and(|len| *len > 0), "{again:?}");
                assert_eq!(
                    entries_asked(&guest, fd, asked_after_first),
                    ["..", "vanished"],
                    "the second read asks what the stored answer asks"
                );
                let keyed = guest.determinized.lock().unwrap()[keyed_after_first..].to_vec();
                assert_eq!(
                    keyed.get(1..),
                    Some(
                        &[
                            RawInode::new(OTHER_DEVICE, SCRIPTED_PARENT_INODE),
                            RawInode::new(device, COVERED_INODE),
                            RawInode::new(device, SCRIPTED_FILE_INODE),
                            RawInode::new(device, SCRIPTED_FILE_INODE + 1),
                            RawInode::new(OTHER_DEVICE, FOLDED_LOWER_INODE),
                        ][..]
                    ),
                    "the second read keys covered and lower on the directory's device, unasked"
                );
                assert_eq!(
                    keyed.get(1..),
                    Some(&first_keys[..]),
                    "the rewound read keys every entry as the first read did"
                );
            }
        }
    }

    /// The guest's `getdents64` of `fd` into a fresh buffer of `count` bytes.
    async fn getdents64_sized(
        tool: &Detcore,
        guest: &mut ScriptedGuest,
        fd: RawFd,
        count: usize,
    ) -> Result<i64, Error> {
        let mut buffer = vec![0u8; count];
        tool.handle_getdents64(
            guest,
            syscalls::Getdents64::new()
                .with_fd(u32::try_from(fd).unwrap())
                .with_dirent(AddrMut::from_raw(buffer.as_mut_ptr() as usize))
                .with_count(u32::try_from(count).unwrap()),
        )
        .await
    }

    /// A scripted guest over [`directory_with_entries`] that reads it as a
    /// single-filesystem overlay with a file bind-mounted over `plain` (see
    /// [`getdents_keys_an_overlay_entry_alike_whatever_was_read_before`]).
    fn overlay_with_a_bound_file(device: u64) -> (Detcore, ScriptedGuest, Pages) {
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
        // Every number below 2^32, whatever the host's, so that only `..`
        // is asked for its key on an overlay that keeps its files.
        guest.getdents_inode_answers = vec![
            (b"..".to_vec(), SCRIPTED_PARENT_INODE),
            (b"covered".to_vec(), SCRIPTED_FILE_INODE - 1),
            (b"lower".to_vec(), SCRIPTED_FILE_INODE),
            (b"plain".to_vec(), BOUND_FILE_INODE),
            (b"vanished".to_vec(), SCRIPTED_FILE_INODE + 2),
        ];
        guest.fstatat_answers = vec![
            (
                b"..".to_vec(),
                Ok(stat_answer(
                    OTHER_DEVICE,
                    SCRIPTED_PARENT_INODE,
                    libc::S_IFDIR | 0o755,
                )),
            ),
            (
                b"covered".to_vec(),
                Ok(stat_answer(
                    device,
                    SCRIPTED_FILE_INODE - 1,
                    libc::S_IFDIR | 0o755,
                )),
            ),
            (
                b"lower".to_vec(),
                Ok(stat_answer(
                    device,
                    SCRIPTED_FILE_INODE,
                    libc::S_IFREG | 0o644,
                )),
            ),
            (
                b"plain".to_vec(),
                Ok(stat_answer(
                    OTHER_DEVICE,
                    BOUND_FILE_INODE,
                    libc::S_IFREG | 0o644,
                )),
            ),
            (
                b"vanished".to_vec(),
                Ok(stat_answer(
                    device,
                    SCRIPTED_FILE_INODE + 2,
                    libc::S_IFREG | 0o644,
                )),
            ),
        ];
        (tool, guest, scratch)
    }

    /// The `d_ino` of `plain` in [`overlay_with_a_bound_file`]: the number of
    /// the file bound over it, on the layers' filesystem.
    const BOUND_FILE_INODE: u64 = SCRIPTED_FILE_INODE + 1;

    /// The keys of `plain` among the inode requests after the first `from`.
    fn bound_file_keys(guest: &ScriptedGuest, from: usize) -> Vec<RawInode> {
        guest.determinized.lock().unwrap()[from..]
            .iter()
            .copied()
            .filter(|raw| raw.ino == BOUND_FILE_INODE)
            .collect()
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, F1. On an
    // overlay whose layers share one filesystem, every file reports the
    // overlay's device, but a file bind-mounted over a merged entry reports
    // the bind's source, here `plain` on `OTHER_DEVICE` with its `d_ino`.
    // Such a file is keyed on the directory's device wherever it is not
    // asked: the first class of the second known gap at
    // `Detcore::directory_entry_identity`, which may stay but must be the
    // same in every read. When the first file to reach keying decided how
    // the overlay numbers its files, a read that reached `plain` before
    // `lower` let `plain` decide, and keyed it on `OTHER_DEVICE`, where a
    // sequential read keyed it on the directory's device: a read that seeks
    // past `lower` after a buffer holding only `.` and `..`, the same
    // through a duplicate of the descriptor, and a descriptor Detcore does
    // not track read in buffers of one entry each. Now the overlay is
    // settled before any entry is keyed, from its first file in sorted
    // order, once per run, so all five reads key `plain` alike. A descriptor
    // Detcore does not track read in such buffers before any other read of
    // the overlay is the remaining gap, documented at
    // `Detcore::settle_overlay_entry_lookup`: it is settled from whichever
    // file the host returns first.
    #[tokio::test]
    async fn getdents_keys_an_overlay_entry_alike_whatever_was_read_before() {
        // Room for `.` and `..` (24 bytes each), not for `covered` too.
        const DOTS_ONLY: usize = 48;
        // Room for one record of any of these names, never for two.
        const ONE_ENTRY: usize = 32;
        // The stream position just after `lower`: `.`, `..`, `covered` and
        // `lower` come before it.
        const AFTER_LOWER: i64 = 4;
        let mut keys: Vec<(&str, u64, Vec<RawInode>)> = Vec::new();

        // 1. One sequential read of a tracked descriptor.
        let (_dir, fd, _, device) = directory_with_entries();
        let (tool, mut guest, _scratch) = overlay_with_a_bound_file(device);
        tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await
            .expect("precondition: Detcore tracks the directory");
        let read = getdents64_of(&tool, &mut guest, fd).await;
        assert!(
            read.as_ref().is_ok_and(|len| *len > 0),
            "sequential: {read:?}"
        );
        keys.push(("sequential", device, bound_file_keys(&guest, 0)));
        close_unless_detcore_did(&guest, fd);

        // 2. `.` and `..`, then a seek past `lower`, then the rest.
        let (_dir, fd, _, device) = directory_with_entries();
        let (tool, mut guest, _scratch) = overlay_with_a_bound_file(device);
        tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await
            .expect("precondition: Detcore tracks the directory");
        let dots = getdents64_sized(&tool, &mut guest, fd, DOTS_ONLY).await;
        assert_eq!(dots.ok(), Some(DOTS_ONLY as i64), "seek: . and .. only");
        let seek = tool
            .handle_lseek(
                &mut guest,
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(AFTER_LOWER)
                    .with_whence(Whence::SEEK_SET),
            )
            .await;
        assert_eq!(
            seek.ok(),
            Some(AFTER_LOWER),
            "seek: the stream moves past lower"
        );
        let read = getdents64_of(&tool, &mut guest, fd).await;
        assert!(read.as_ref().is_ok_and(|len| *len > 0), "seek: {read:?}");
        keys.push(("seek past lower", device, bound_file_keys(&guest, 0)));
        close_unless_detcore_did(&guest, fd);

        // 3. As 2, through a duplicate of the descriptor.
        let (_dir, fd, _, device) = directory_with_entries();
        let (tool, mut guest, _scratch) = overlay_with_a_bound_file(device);
        tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await
            .expect("precondition: Detcore tracks the directory");
        let dup = unsafe { libc::dup(fd) };
        assert!(dup >= 0, "dup: {}", Errno::last());
        guest
            .thread_state_mut()
            .dup_fd(fd, dup, OFlag::empty())
            .expect("precondition: Detcore tracks the duplicate");
        let dots = getdents64_sized(&tool, &mut guest, fd, DOTS_ONLY).await;
        assert_eq!(dots.ok(), Some(DOTS_ONLY as i64), "dup: . and .. only");
        let seek = tool
            .handle_lseek(
                &mut guest,
                syscalls::Lseek::new()
                    .with_fd(dup)
                    .with_offset(AFTER_LOWER)
                    .with_whence(Whence::SEEK_SET),
            )
            .await;
        assert_eq!(seek.ok(), Some(AFTER_LOWER), "dup: the shared stream moves");
        let read = getdents64_of(&tool, &mut guest, dup).await;
        assert!(read.as_ref().is_ok_and(|len| *len > 0), "dup: {read:?}");
        keys.push(("duplicate", device, bound_file_keys(&guest, 0)));
        close_unless_detcore_did(&guest, dup);
        close_unless_detcore_did(&guest, fd);

        // 4. A tracked read, then another descriptor of the same directory
        // that Detcore does not track, read one entry at a time.
        let (dir, fd, _, device) = directory_with_entries();
        // Opened while `fd` is, so that it does not take a number Detcore
        // tracks.
        let untracked = std::fs::File::open(dir.path()).unwrap().into_raw_fd();
        let (tool, mut guest, _scratch) = overlay_with_a_bound_file(device);
        tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await
            .expect("precondition: Detcore tracks the directory");
        let read = getdents64_of(&tool, &mut guest, fd).await;
        assert!(
            read.as_ref().is_ok_and(|len| *len > 0),
            "tracked first: {read:?}"
        );
        let keyed_before = guest.determinized.lock().unwrap().len();
        let mut reads = 0;
        loop {
            let read = getdents64_sized(&tool, &mut guest, untracked, ONE_ENTRY).await;
            match read {
                Ok(0) => break,
                Ok(_) => reads += 1,
                Err(error) => panic!("untracked small: {error:?}"),
            }
            assert!(reads <= 6, "untracked small: more reads than entries");
        }
        assert_eq!(reads, 6, "untracked small: one entry per read");
        keys.push((
            "untracked, one entry per read, after a tracked read",
            device,
            bound_file_keys(&guest, keyed_before),
        ));
        close_unless_detcore_did(&guest, untracked);
        close_unless_detcore_did(&guest, fd);

        // 5. A descriptor Detcore does not track, read whole, in a fresh run.
        let (_dir, fd, _, device) = directory_with_entries();
        let (tool, mut guest, _scratch) = overlay_with_a_bound_file(device);
        let read = getdents64_of(&tool, &mut guest, fd).await;
        assert!(
            read.as_ref().is_ok_and(|len| *len > 0),
            "untracked whole: {read:?}"
        );
        keys.push(("untracked, whole", device, bound_file_keys(&guest, 0)));
        close_unless_detcore_did(&guest, fd);

        for (read, device, keyed) in keys {
            assert_eq!(
                keyed,
                [RawInode::new(device, BOUND_FILE_INODE)],
                "{read}: plain is keyed once, on the directory's device, as in every read"
            );
        }
    }

    /// Scripted answers that make the directory read next stand for one of
    /// two directories of the same overlay, whose layers share one
    /// filesystem, so every file of it reports the overlay's device, which
    /// is the directory's (`device`). In `A`, `lower` (`d_ino` 300) has a
    /// file bind-mounted over it, from `OTHER_DEVICE` with the same number,
    /// and `plain` (301) is an ordinary file of the overlay when `ordinary`,
    /// and gone otherwise; in `B`, `lower` (400) is an ordinary file.
    fn read_as_overlay_directory(guest: &mut ScriptedGuest, a: bool, ordinary: bool, device: u64) {
        let base = if a { 300 } else { 400 };
        let file = |ino: u64| stat_answer(device, ino, libc::S_IFREG | 0o644);
        guest.getdents_inode_answers = vec![
            (b"..".to_vec(), SCRIPTED_PARENT_INODE),
            (b"covered".to_vec(), base - 1),
            (b"lower".to_vec(), base),
            (b"plain".to_vec(), base + 1),
            (b"vanished".to_vec(), base + 2),
        ];
        let lower = if a {
            stat_answer(OTHER_DEVICE, base, libc::S_IFREG | 0o644)
        } else {
            file(base)
        };
        let plain = if a && !ordinary {
            Err(Errno::ENOENT)
        } else {
            Ok(file(base + 1))
        };
        guest.fstatat_answers = vec![
            (
                b"..".to_vec(),
                Ok(stat_answer(
                    OTHER_DEVICE,
                    SCRIPTED_PARENT_INODE,
                    libc::S_IFDIR | 0o755,
                )),
            ),
            (
                b"covered".to_vec(),
                Ok(stat_answer(device, base - 1, libc::S_IFDIR | 0o755)),
            ),
            (b"lower".to_vec(), Ok(lower)),
            (b"plain".to_vec(), plain),
            (b"vanished".to_vec(), Err(Errno::ENOENT)),
        ];
        guest.statx_mount_roots = if a { vec![b"lower".to_vec()] } else { vec![] };
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, round 9,
    // P1. The overlay's kind is settled once per run from the first file in
    // sorted order whose `lstat` tells, but a file bind-mounted over a merged
    // entry is not a file of the overlay: on an overlay whose layers share
    // one filesystem it can report another device with the entry's own
    // number, which reads as an overlay that reports a layer's device for
    // every file. When it was taken as such, the kind of the whole overlay,
    // and so the key of `lower` in `A`, depended on whether `A` or `B` was
    // listed first, though both were read whole through tracked descriptors.
    // A candidate that would settle `OverlayLayerDevices` is now asked
    // whether it is the root of a mount (`STATX_ATTR_MOUNT_ROOT`), and a
    // mount root tells nothing, so the overlay settles as it keeps its files
    // in either order, from `plain` or from `B`'s `lower`, and `A`'s `lower`
    // is keyed on the directory's device in every run: the first class of
    // the second known gap at `Detcore::directory_entry_identity`, the same
    // in every read. When `A` has no other file, its first listing settles
    // nothing and is keyed as an overlay that keeps its files, which gives
    // `lower` the same key. A kernel without the attribute (before Linux
    // 5.8) cannot tell, and the answer is taken as before: the documented
    // fallback.
    #[tokio::test]
    async fn getdents_settles_an_overlay_alike_whichever_directory_is_read_first() {
        for ordinary in [true, false] {
            let mut keys = Vec::new();
            for a_first in [true, false] {
                let (_dir_a, fd_a, _, device) = directory_with_entries();
                let (_dir_b, fd_b, _, device_b) = directory_with_entries();
                assert_eq!(device, device_b, "precondition: one filesystem");
                let scratch = Pages::map(1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.answers_determinize_inode = true;
                guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
                for fd in [fd_a, fd_b] {
                    tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                        .await
                        .expect("precondition: Detcore tracks the directory");
                }
                let order = if a_first {
                    [(true, fd_a), (false, fd_b)]
                } else {
                    [(false, fd_b), (true, fd_a)]
                };
                let mut lower_of_a = Vec::new();
                for (a, fd) in order {
                    read_as_overlay_directory(&mut guest, a, ordinary, device);
                    let before = guest.determinized.lock().unwrap().len();
                    let statx_before = guest.statx_paths.len();
                    let read = getdents64_of(&tool, &mut guest, fd).await;
                    assert!(
                        read.as_ref().is_ok_and(|len| *len > 0),
                        "ordinary {ordinary}, A first {a_first}, A {a}: {read:?}"
                    );
                    if a {
                        lower_of_a = guest.determinized.lock().unwrap()[before..]
                            .iter()
                            .copied()
                            .filter(|raw| raw.ino == 300)
                            .collect();
                        assert_eq!(
                            guest.statx_paths[statx_before..],
                            if a_first {
                                vec![b"lower".to_vec()]
                            } else {
                                vec![]
                            },
                            "ordinary {ordinary}, A first {a_first}: only A's bound lower is \
                             asked whether it is a mount, and only before the overlay settles"
                        );
                        if a_first {
                            assert_eq!(
                                guest.statx_starts.last(),
                                Some(&(
                                    fd,
                                    AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_NO_AUTOMOUNT
                                )),
                                "the statx is asked as the lstat is, relative to the directory"
                            );
                        }
                    }
                }
                let proposals: Vec<Option<EntryLookup>> = guest
                    .settle_requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(_, proposal)| *proposal)
                    .collect();
                assert!(
                    !proposals.contains(&Some(EntryLookup::OverlayLayerDevices)),
                    "ordinary {ordinary}, A first {a_first}: a mount root settled the overlay: \
                     {proposals:?}"
                );
                assert!(
                    proposals.contains(&Some(EntryLookup::OverlayOwnDevice)),
                    "ordinary {ordinary}, A first {a_first}: the overlay settles as it keeps its \
                     files: {proposals:?}"
                );
                keys.push((a_first, device, lower_of_a));
                close_unless_detcore_did(&guest, fd_a);
                close_unless_detcore_did(&guest, fd_b);
            }
            for (a_first, device, keyed) in keys {
                assert_eq!(
                    keyed,
                    [RawInode::new(device, 300)],
                    "ordinary {ordinary}, A first {a_first}: A's lower is keyed once, on the \
                     directory's device, whichever directory is read first"
                );
            }
        }
    }

    // The fallback of the test above: a kernel that does not report
    // `STATX_ATTR_MOUNT_ROOT` (before Linux 5.8) cannot tell a mount root,
    // and `A`'s bound `lower` settles the overlay as it did before the
    // question was asked, as `Detcore::entry_is_on_the_listed_mount`
    // documents.
    #[tokio::test]
    async fn getdents_takes_a_candidates_answer_where_the_kernel_cannot_tell_a_mount() {
        let (_dir, fd, _, device) = directory_with_entries();
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
        guest.statx_without_mount_root_attribute = true;
        tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
            .await
            .expect("precondition: Detcore tracks the directory");
        read_as_overlay_directory(&mut guest, true, true, device);
        let read = getdents64_of(&tool, &mut guest, fd).await;
        close_unless_detcore_did(&guest, fd);
        assert!(read.as_ref().is_ok_and(|len| *len > 0), "{read:?}");
        assert_eq!(guest.statx_paths, [b"lower".to_vec()]);
        assert_eq!(
            guest.settle_requests.lock().unwrap().last(),
            Some(&(device, Some(EntryLookup::OverlayLayerDevices)))
        );
    }

    /// Scripted answers that make the directory read next stand for one of
    /// two directories of an overlay of the third kind, whose layers are on
    /// more than one filesystem and which does not fold their inode numbers,
    /// so each of its files reports its layer's device (`OTHER_DEVICE`) and
    /// each directory the overlay's (`device`). In `A`, `lower` (`d_ino` 300)
    /// is a lower-layer file whose own backing file is bind-mounted over its
    /// path, so its `lstat` reports its `d_ino` on a device other than the
    /// overlay's, as an ordinary file of this kind does (scripted here on the
    /// same device, so only the mount-root question tells them apart), and it
    /// is `A`'s only file; in `B`, `lower` (400) and `plain` (401) are
    /// ordinary lower-layer files.
    fn read_as_layered_overlay_directory(guest: &mut ScriptedGuest, a: bool, device: u64) {
        let base = if a { 300 } else { 400 };
        let layer_file = |ino: u64| Ok(stat_answer(OTHER_DEVICE, ino, libc::S_IFREG | 0o644));
        guest.getdents_inode_answers = vec![
            (b"..".to_vec(), SCRIPTED_PARENT_INODE),
            (b"covered".to_vec(), base - 1),
            (b"lower".to_vec(), base),
            (b"plain".to_vec(), base + 1),
            (b"vanished".to_vec(), base + 2),
        ];
        guest.fstatat_answers = vec![
            (
                b"..".to_vec(),
                Ok(stat_answer(
                    OTHER_DEVICE,
                    SCRIPTED_PARENT_INODE,
                    libc::S_IFDIR | 0o755,
                )),
            ),
            (
                b"covered".to_vec(),
                Ok(stat_answer(device, base - 1, libc::S_IFDIR | 0o755)),
            ),
            (b"lower".to_vec(), layer_file(base)),
            (
                b"plain".to_vec(),
                if a {
                    Err(Errno::ENOENT)
                } else {
                    layer_file(base + 1)
                },
            ),
            (b"vanished".to_vec(), Err(Errno::ENOENT)),
        ];
        guest.statx_mount_roots = if a { vec![b"lower".to_vec()] } else { vec![] };
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, round
    // 10, P1. On an overlay of the third kind, a directory whose only file
    // is a mount root (`A`) settles nothing, so its listing is keyed as an
    // overlay that keeps its files would key it, `lower` on the directory's
    // device. Once another directory (`B`) had settled the overlay as one
    // that reports a layer's device, a second read of `A` asked `lower` and
    // keyed it on the device its `lstat` reports: the same file got two
    // inode numbers in one run, and which one a read gave depended on the
    // order in which the directories were listed. The round-10 fix asked
    // such an entry whether it is a mount root on the settled overlay too,
    // and keyed a mount root on the directory's device there as well.
    //
    // Round 11 of the same review, F1: that was the wrong identity. The
    // file's `stat`, like its `maps` line and a listing under `Every`, keys
    // it on the device its `lstat` reports, so the guest's `d_ino` for it
    // differed from its `st_ino`, and from what a run whose inherited
    // seccomp filter chooses `Every` lists. A listing of an overlay settled
    // as reporting layer devices now keys each asked entry whose `lstat`
    // reports its `d_ino` on that `lstat`'s device, a mount root included,
    // and asks no mount-root question, so every read of `A` once the overlay
    // has settled lists `lower` with the number its `stat` reports. Only the
    // settle pass asks whether a file is a mount root, and a mount root is
    // still no evidence there (round 9). The read of `A` before the overlay
    // settles, when `A` is read first, still keys `lower` on the directory's
    // device: that listing is the same, answer for answer, as `A`'s first
    // listing without `plain` in
    // `getdents_settles_an_overlay_alike_whichever_directory_is_read_first`,
    // on an overlay whose layers share one filesystem, which settles as
    // keeping its files and keys `lower` on the directory's device in every
    // read. One listing cannot be keyed two ways, so that read keeps that
    // key: the first class of the second known gap at
    // `Detcore::directory_entry_identity`, on a listing read before the
    // overlay settles. (The test's name records the round-10 intent.)
    #[tokio::test]
    async fn getdents_keys_a_mount_root_alike_before_and_after_an_overlay_settles_layer_devices() {
        for a_first in [true, false] {
            let (_dir_a, fd_a, _, device) = directory_with_entries();
            let (_dir_b, fd_b, _, device_b) = directory_with_entries();
            assert_eq!(device, device_b, "precondition: one filesystem");
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.answers_determinize_device = true;
            guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
            for fd in [fd_a, fd_b] {
                tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                    .await
                    .expect("precondition: Detcore tracks the directory");
            }
            let order: &[(bool, RawFd)] = if a_first {
                &[(true, fd_a), (false, fd_b), (true, fd_a)]
            } else {
                &[(false, fd_b), (true, fd_a)]
            };
            let mut a_reads = 0;
            let mut lower_of_a = Vec::new();
            let mut lower_of_b = Vec::new();
            let mut asked_whether_mounts = Vec::new();
            for &(a, fd) in order {
                read_as_layered_overlay_directory(&mut guest, a, device);
                let settled = guest
                    .settled_overlays
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(overlay, _)| *overlay == device);
                if a && a_reads > 0 {
                    let rewound = tool
                        .handle_lseek(
                            &mut guest,
                            syscalls::Lseek::new()
                                .with_fd(fd)
                                .with_offset(0)
                                .with_whence(Whence::SEEK_SET),
                        )
                        .await;
                    assert_eq!(rewound.ok(), Some(0), "A's stream rewinds");
                }
                let before = guest.determinized.lock().unwrap().len();
                let statx_before = guest.statx_paths.len();
                let (read, records) = getdents64_records_of(&tool, &mut guest, fd).await;
                assert!(
                    read.as_ref().is_ok_and(|len| *len > 0),
                    "A first {a_first}, A {a}: {read:?}"
                );
                let keyed = guest.determinized.lock().unwrap()[before..].to_vec();
                asked_whether_mounts.push((a, settled, guest.statx_paths[statx_before..].to_vec()));
                if a {
                    a_reads += 1;
                    let keyed: Vec<RawInode> =
                        keyed.into_iter().filter(|raw| raw.ino == 300).collect();
                    lower_of_a.push((settled, listed_inode(&records, b"lower"), keyed));
                } else {
                    lower_of_b.extend(keyed.into_iter().filter(|raw| raw.ino == 400));
                }
            }
            close_unless_detcore_did(&guest, fd_a);
            close_unless_detcore_did(&guest, fd_b);
            // The number the guest's `stat` of `A`'s `lower` reports as its
            // `st_ino`: `stat` reports what its `lstat` does, the file
            // bound over the entry.
            let stat_of_lower = tool
                .determinize_stat(
                    &mut guest,
                    stat_answer(OTHER_DEVICE, 300, libc::S_IFREG | 0o644),
                    None,
                )
                .await
                .expect("the stat of A's lower is determinized")
                .inode;
            let proposals: Vec<Option<EntryLookup>> = guest
                .settle_requests
                .lock()
                .unwrap()
                .iter()
                .map(|(_, proposal)| *proposal)
                .collect();
            assert!(
                proposals.contains(&Some(EntryLookup::OverlayLayerDevices)),
                "A first {a_first}: B settles the overlay as one that reports a layer's device: \
                 {proposals:?}"
            );
            assert_eq!(
                lower_of_b,
                [RawInode::new(OTHER_DEVICE, 400)],
                "A first {a_first}: B's lower is keyed on its layer's device"
            );
            assert_eq!(
                lower_of_a
                    .iter()
                    .map(|(settled, _, _)| *settled)
                    .collect::<Vec<_>>(),
                if a_first {
                    vec![false, true]
                } else {
                    vec![true]
                },
                "A first {a_first}: A is read before the overlay settles only when it is read first"
            );
            for (settled, listed, keyed) in lower_of_a {
                if settled {
                    assert_eq!(
                        listed, stat_of_lower,
                        "A first {a_first}: once the overlay has settled, every read lists A's \
                         bound lower with the d_ino its stat reports as st_ino"
                    );
                    assert_eq!(
                        keyed,
                        [RawInode::new(OTHER_DEVICE, 300)],
                        "A first {a_first}: once the overlay has settled, A's bound lower is \
                         keyed as its stat is, on the device its lstat reports"
                    );
                } else {
                    assert_eq!(
                        keyed,
                        [RawInode::new(device, 300)],
                        "A first {a_first}: before the overlay settles, A's bound lower is keyed \
                         as an overlay that keeps its files keys it, on the directory's device"
                    );
                }
            }
            for (a, settled, asked) in asked_whether_mounts {
                let expected = if settled {
                    vec![]
                } else {
                    vec![b"lower".to_vec()]
                };
                assert_eq!(
                    asked, expected,
                    "A first {a_first}, A {a}, settled {settled}: only the settle pass asks \
                     whether a file is a mount, once per candidate; a listing of the settled \
                     overlay asks no entry"
                );
            }
        }
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, round
    // 11, F1 and F5. Codex's scenario: on an overlay of the third kind,
    // `B` is listed first and settles the overlay as one that reports a
    // layer's device; then `A` is listed, whose `lower` is a lower-layer
    // file with a hard link to it bound over its path, so its `lstat`
    // reports its `d_ino` on a layer's device and it is a mount root. The
    // settled listing asked whether `lower` is a mount root and keyed it on
    // the directory's device, while its `stat`, like its `maps` line, keys
    // it on the device its `lstat` reports, as a listing under `Every` does:
    // the guest's `d_ino` for the file differed from its `st_ino` and
    // depended on the policy, which the launcher's inherited seccomp filter
    // chooses. The question also mapped a transient page that `Every` never
    // maps, and on the KVM backend each mapping moves where a later guest
    // `mmap` lands. A listing of the settled overlay now keys `lower` on the
    // device its `lstat` reports and asks no mount-root question, so the
    // guest reads the same records, and Detcore makes the same transient
    // mappings, under either policy.
    #[tokio::test]
    async fn getdents_keys_a_settled_layer_overlays_mount_root_as_stat_and_every_do() {
        let mut runs = Vec::new();
        for every in [false, true] {
            let (_dir_a, fd_a, _, device) = directory_with_entries();
            let (_dir_b, fd_b, _, device_b) = directory_with_entries();
            assert_eq!(device, device_b, "precondition: one filesystem");
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.answers_determinize_device = true;
            guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
            guest.config.seccomp_may_refuse_entry_lookup_syscalls = every;
            for fd in [fd_a, fd_b] {
                tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                    .await
                    .expect("precondition: Detcore tracks the directory");
            }
            read_as_layered_overlay_directory(&mut guest, false, device);
            let (read, _) = getdents64_records_of(&tool, &mut guest, fd_b).await;
            assert!(
                read.as_ref().is_ok_and(|len| *len > 0),
                "Every {every}, B: {read:?}"
            );
            let settled = guest.settle_requests.lock().unwrap().last().copied();
            assert_eq!(
                settled,
                if every {
                    None
                } else {
                    Some((device, Some(EntryLookup::OverlayLayerDevices)))
                },
                "Every {every}: B settles the overlay as one that reports a layer's device, \
                 where the overlay policy is used"
            );

            read_as_layered_overlay_directory(&mut guest, true, device);
            let keyed_before = guest.determinized.lock().unwrap().len();
            let statx_before = guest.statx_paths.len();
            let mapped_before = guest.mapped.len();
            let injected_before = guest.injected.len();
            let (read, records) = getdents64_records_of(&tool, &mut guest, fd_a).await;
            assert!(
                read.as_ref().is_ok_and(|len| *len > 0),
                "Every {every}, A: {read:?}"
            );
            let keyed = guest.determinized.lock().unwrap()[keyed_before..].to_vec();
            let asked_whether_mounts = guest.statx_paths[statx_before..].to_vec();
            let mappings: Vec<usize> = guest.mapped[mapped_before..]
                .iter()
                .map(|(_, len)| *len)
                .collect();
            let mapping_calls: Vec<Sysno> = guest.injected[injected_before..]
                .iter()
                .copied()
                .filter(|sysno| matches!(sysno, Sysno::mmap | Sysno::munmap | Sysno::statx))
                .collect();
            close_unless_detcore_did(&guest, fd_a);
            close_unless_detcore_did(&guest, fd_b);

            let stat_of_lower = tool
                .determinize_stat(
                    &mut guest,
                    stat_answer(OTHER_DEVICE, 300, libc::S_IFREG | 0o644),
                    None,
                )
                .await
                .expect("the stat of A's lower is determinized")
                .inode;
            assert_eq!(
                listed_inode(&records, b"lower"),
                stat_of_lower,
                "Every {every}: A lists its bound lower with the d_ino its stat reports as st_ino"
            );
            assert!(
                keyed.contains(&RawInode::new(OTHER_DEVICE, 300)),
                "Every {every}: A's bound lower is keyed on the device its lstat reports: \
                 {keyed:?}"
            );
            assert_eq!(
                asked_whether_mounts,
                Vec::<Vec<u8>>::new(),
                "Every {every}: no entry of A is asked whether it is a mount"
            );
            // Each entry is keyed once, in the order listed. `.` is each
            // run's own temporary directory, so its raw key differs between
            // the runs; no other entry's may.
            assert_eq!(keyed.len(), records.len(), "Every {every}: {keyed:?}");
            let keyed: Vec<(Vec<u8>, RawInode)> = records
                .iter()
                .map(|(name, _)| name.clone())
                .zip(keyed)
                .filter(|(name, _)| name != b".")
                .collect();
            runs.push((every, records, keyed, mappings, mapping_calls));
        }
        let (_, records, keyed, mappings, mapping_calls) = &runs[0];
        let (_, every_records, every_keyed, every_mappings, every_mapping_calls) = &runs[1];
        assert_eq!(
            records, every_records,
            "the guest reads the same records of A on the settled overlay as under Every"
        );
        assert_eq!(
            keyed, every_keyed,
            "A's entries have the same keys on the settled overlay as under Every"
        );
        assert_eq!(
            (mappings, mapping_calls),
            (every_mappings, every_mapping_calls),
            "listing A on the settled overlay makes the same transient mappings as under Every, \
             so a later guest mmap lands alike on the KVM backend"
        );
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, round
    // 11, F4. The launcher asks, in its own mount namespace, whether an
    // inherited seccomp filter may refuse the questions the narrow policies
    // ask beyond `Every`'s. A supervisor that answers `statx` there but
    // refuses it with `EPERM` in the container's namespace passes that
    // question and then refuses the settle pass's mount-root question, and
    // the refusal ended the guest; at the base, which asked inside the
    // container, the run used `Every`, which asks no such question. The
    // narrow policies are an optimization of `Every`, so a refused extra
    // question (the `statx`, whatever its errno, or the `mmap` or `munmap`
    // of its page) now keys the listing exactly as `Every` keys it and the
    // guest continues. The refusal is no evidence of the overlay's kind and
    // is not kept, so the next read asks again and gets the same keys, and
    // no inode number is spent that `Every` does not spend.
    #[tokio::test]
    async fn getdents_keys_a_listing_as_every_does_when_its_mount_root_question_is_refused() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Refused {
            /// The reference run: `Every`, which asks no mount-root question.
            NothingUnderEvery,
            Statx(Errno),
            Mmap(Errno),
            Munmap(Errno),
        }
        // The page `Detcore::entry_is_on_the_listed_mount` maps for a short
        // name; the listing's snapshot is a larger mapping.
        let page = host_page_size();
        let mut runs = Vec::new();
        for refused in [
            Refused::NothingUnderEvery,
            Refused::Statx(Errno::EPERM),
            Refused::Statx(Errno::EACCES),
            Refused::Mmap(Errno::EPERM),
            Refused::Munmap(Errno::EPERM),
        ] {
            let every = refused == Refused::NothingUnderEvery;
            let (_dir, fd, _, device) = directory_with_entries();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
            guest.config.seccomp_may_refuse_entry_lookup_syscalls = every;
            match refused {
                Refused::NothingUnderEvery => {}
                Refused::Statx(errno) => guest.entry_statx_error = Some(errno),
                Refused::Mmap(errno) => guest.mmap_error_of_len = Some((page, errno)),
                Refused::Munmap(errno) => guest.munmap_error_of_len = Some((page, errno)),
            }
            tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                .await
                .expect("precondition: Detcore tracks the directory");
            // `B`: `lower`, the first candidate, would settle the overlay as
            // one that reports a layer's device, once it is known not to be
            // a mount root.
            read_as_layered_overlay_directory(&mut guest, false, device);
            let mut reads = Vec::new();
            for read in 0..2 {
                if read > 0 {
                    let rewound = tool
                        .handle_lseek(
                            &mut guest,
                            syscalls::Lseek::new()
                                .with_fd(fd)
                                .with_offset(0)
                                .with_whence(Whence::SEEK_SET),
                        )
                        .await;
                    assert_eq!(rewound.ok(), Some(0), "{refused:?}: the stream rewinds");
                }
                let keyed_before = guest.determinized.lock().unwrap().len();
                let statx_before = guest.statx_paths.len();
                let (len, records) = getdents64_records_of(&tool, &mut guest, fd).await;
                assert!(
                    len.as_ref().is_ok_and(|len| *len > 0),
                    "{refused:?}, read {read}: the guest's getdents64 succeeds rather than \
                     the refused question ending the guest: {len:?}"
                );
                let keyed = guest.determinized.lock().unwrap()[keyed_before..].to_vec();
                let asked_whether_mounts = guest.statx_paths[statx_before..].to_vec();
                assert_eq!(
                    asked_whether_mounts,
                    match refused {
                        Refused::NothingUnderEvery | Refused::Mmap(_) => Vec::<Vec<u8>>::new(),
                        Refused::Statx(_) | Refused::Munmap(_) => vec![b"lower".to_vec()],
                    },
                    "{refused:?}, read {read}: only the first candidate is asked whether it \
                     is a mount, in every read, since a refusal is not kept"
                );
                assert!(
                    keyed.contains(&RawInode::new(OTHER_DEVICE, 400)),
                    "{refused:?}, read {read}: B's lower is keyed on the device its lstat \
                     reports, as Every keys it: {keyed:?}"
                );
                // Each entry is keyed once, in the order listed. `.` is each
                // run's own temporary directory, so its raw key differs
                // between the runs; no other entry's may.
                assert_eq!(keyed.len(), records.len(), "{refused:?}: {keyed:?}");
                let keyed: Vec<(Vec<u8>, RawInode)> = records
                    .iter()
                    .map(|(name, _)| name.clone())
                    .zip(keyed)
                    .filter(|(name, _)| name != b".")
                    .collect();
                reads.push((records, keyed));
            }
            let proposals: Vec<_> = guest
                .settle_requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, proposal)| proposal.is_some())
                .copied()
                .collect();
            assert!(
                proposals.is_empty(),
                "{refused:?}: a refused question is no evidence of the overlay's kind: \
                 {proposals:?}"
            );
            assert!(
                guest.settled_overlays.lock().unwrap().is_empty(),
                "{refused:?}: nothing is settled"
            );
            close_unless_detcore_did(&guest, fd);
            runs.push((refused, reads));
        }
        let (_, every_reads) = &runs[0];
        for (refused, reads) in &runs[1..] {
            assert_eq!(
                reads, every_reads,
                "{refused:?}: each read lists the same records of B, with the same keys, as \
                 under Every"
            );
        }
    }

    // The same F5: an entry whose `lstat` cannot be asked (here a scripted
    // `ENOMEM`, as from a transient page the guest could not map) is a
    // refusal on both paths, never the directory's device, which would let a
    // host resource choose the identity; and `ESRCH`, the thread being gone,
    // is that errno, as for every guest stat. Either way the listing has sent
    // no numbering request.
    #[tokio::test]
    async fn getdents_refuses_when_an_entrys_lstat_cannot_be_asked() {
        for tracked in [false, true] {
            for errno in [Errno::ENOMEM, Errno::ESRCH] {
                let (_dir, fd, _, _) = directory_with_entries();
                let scratch = Pages::map(1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.answers_determinize_inode = true;
                // Overlayfs, where `lower` shows a layer's device for a file,
                // so `plain` is asked.
                guest.fstatfs_answer = Some(Ok(libc::OVERLAYFS_SUPER_MAGIC));
                if tracked {
                    tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                        .await
                        .expect("precondition: Detcore tracks the directory");
                }
                guest.getdents_inode_answers = vec![(b"lower".to_vec(), SCRIPTED_FILE_INODE)];
                guest.fstatat_answers = vec![
                    (
                        b"lower".to_vec(),
                        Ok(stat_answer(
                            OTHER_DEVICE,
                            SCRIPTED_FILE_INODE,
                            libc::S_IFREG | 0o644,
                        )),
                    ),
                    (b"plain".to_vec(), Err(errno)),
                ];
                let keyed_before = guest.determinized.lock().unwrap().len();

                let returned = getdents64_of(&tool, &mut guest, fd).await;
                close_unless_detcore_did(&guest, fd);

                if errno == Errno::ESRCH {
                    assert!(
                        matches!(returned, Err(Error::Errno(Errno::ESRCH))),
                        "tracked {tracked}: {returned:?}"
                    );
                } else {
                    assert_typed_refusal("getdents64", returned.map(drop), errno);
                }
                assert_eq!(
                    guest.fstatat_paths.last().map(Vec::as_slice),
                    Some(&b"plain"[..]),
                    "tracked {tracked}, {errno}: no entry after the failed one is asked"
                );
                // The entries before `plain` were resolved, but the pool
                // counts requests, so a refused listing must have sent none:
                // otherwise how many numbers it consumed would depend on
                // which entry's host lookup failed.
                assert_eq!(
                    guest.determinized.lock().unwrap()[keyed_before..],
                    [],
                    "tracked {tracked}, {errno}: a refused listing requests no inode number"
                );
            }
        }
    }

    /// The `d_ino` a scripted getdents64 gives `..`, which a scripted `lstat`
    /// reports on [`OTHER_DEVICE`] as the parent of a mount or subvolume root
    /// does.
    const SCRIPTED_PARENT_INODE: u64 = 4242;

    /// The names of the entries whose `lstat` was injected after the first
    /// `from` ones, each checked to be relative to `fd`.
    fn entries_asked(guest: &ScriptedGuest, fd: RawFd, from: usize) -> Vec<String> {
        let entry_lstat = (fd, AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_NO_AUTOMOUNT);
        assert!(
            guest.fstatat_starts[from..]
                .iter()
                .all(|start| *start == entry_lstat),
            "every lstat is relative to the descriptor: {:?}",
            &guest.fstatat_starts[from..]
        );
        guest.fstatat_paths[from..]
            .iter()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect()
    }

    // https://github.com/rrnewton/hermit/pull/3255 asked every entry's
    // `lstat`, which cost the 3000-entry `readdir_order` test 5.9 GiB and
    // triple its time. On btrfs only two kinds of entry can carry their
    // `st_ino` on another device: `..` of a subvolume or mount root, and the
    // entry of the first subvolume, whose id and root inode are both 256.
    // Those are asked and keyed on the device their `lstat` reports; an
    // entry whose `lstat` would report another device (`lower`) is not
    // asked and keeps the directory's. Both getdents paths.
    #[tokio::test]
    async fn getdents_on_btrfs_asks_only_the_parent_and_a_first_subvolume() {
        for tracked in [false, true] {
            let (_dir, fd, inodes, device) = directory_with_entries();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.fstatfs_answer = Some(Ok(libc::BTRFS_SUPER_MAGIC));
            if tracked {
                tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                    .await
                    .expect("precondition: Detcore tracks the directory");
            }
            guest.getdents_inode_answers = vec![
                (b"..".to_vec(), SCRIPTED_PARENT_INODE),
                (b"covered".to_vec(), 256),
            ];
            guest.fstatat_answers = vec![
                (
                    b"..".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        SCRIPTED_PARENT_INODE,
                        libc::S_IFDIR | 0o755,
                    )),
                ),
                (
                    b"covered".to_vec(),
                    Ok(stat_answer(OTHER_DEVICE, 256, libc::S_IFDIR | 0o755)),
                ),
                (
                    b"lower".to_vec(),
                    Ok(stat_answer(
                        OTHER_DEVICE,
                        inodes["lower"],
                        libc::S_IFREG | 0o644,
                    )),
                ),
            ];
            let asked_before = guest.fstatat_paths.len();
            let keyed_before = guest.determinized.lock().unwrap().len();

            let returned = getdents64_of(&tool, &mut guest, fd).await;
            close_unless_detcore_did(&guest, fd);

            assert!(
                returned.as_ref().is_ok_and(|len| *len > 0),
                "tracked {tracked}: {returned:?}"
            );
            let keyed = guest.determinized.lock().unwrap()[keyed_before..].to_vec();
            assert_eq!(
                keyed.get(1..),
                Some(
                    &[
                        RawInode::new(OTHER_DEVICE, SCRIPTED_PARENT_INODE),
                        RawInode::new(OTHER_DEVICE, 256),
                        RawInode::new(device, inodes["lower"]),
                        RawInode::new(device, inodes["plain"]),
                        RawInode::new(device, inodes["vanished"]),
                    ][..]
                ),
                "tracked {tracked}: .., covered, lower, plain and vanished, after ."
            );
            assert_eq!(
                entries_asked(&guest, fd, asked_before),
                ["..", "covered"],
                "tracked {tracked}: only .. and the entry whose d_ino is 256 are asked"
            );
            assert_eq!(
                guest.fstatfs_calls,
                [(fd, true)],
                "tracked {tracked}: one fstatfs, in the guarded stack scratch"
            );
        }
    }

    // The same measurement: on any filesystem but overlayfs, CephFS and
    // btrfs, `..` is the only entry asked. An ext4 mount root's `..` has inode 2, as
    // the ext4 root above it does, so it is keyed on the device its `lstat`
    // reports; `lower`, whose `lstat` would report another device, keeps
    // the directory's.
    #[tokio::test]
    async fn getdents_elsewhere_asks_only_the_parent() {
        for f_type in [0xef53, 0x0102_1994] {
            for tracked in [false, true] {
                let (_dir, fd, inodes, device) = directory_with_entries();
                let scratch = Pages::map(1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.answers_determinize_inode = true;
                guest.fstatfs_answer = Some(Ok(f_type));
                if tracked {
                    tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                        .await
                        .expect("precondition: Detcore tracks the directory");
                }
                guest.getdents_inode_answers = vec![(b"..".to_vec(), SCRIPTED_PARENT_INODE)];
                guest.fstatat_answers = vec![
                    (
                        b"..".to_vec(),
                        Ok(stat_answer(
                            OTHER_DEVICE,
                            SCRIPTED_PARENT_INODE,
                            libc::S_IFDIR | 0o755,
                        )),
                    ),
                    (
                        b"lower".to_vec(),
                        Ok(stat_answer(
                            OTHER_DEVICE,
                            inodes["lower"],
                            libc::S_IFREG | 0o644,
                        )),
                    ),
                ];
                let asked_before = guest.fstatat_paths.len();
                let keyed_before = guest.determinized.lock().unwrap().len();

                let returned = getdents64_of(&tool, &mut guest, fd).await;
                close_unless_detcore_did(&guest, fd);

                let case = format!("f_type {f_type:#x}, tracked {tracked}");
                assert!(
                    returned.as_ref().is_ok_and(|len| *len > 0),
                    "{case}: {returned:?}"
                );
                let keyed = guest.determinized.lock().unwrap()[keyed_before..].to_vec();
                assert_eq!(
                    keyed.get(1..),
                    Some(
                        &[
                            RawInode::new(OTHER_DEVICE, SCRIPTED_PARENT_INODE),
                            RawInode::new(device, inodes["covered"]),
                            RawInode::new(device, inodes["lower"]),
                            RawInode::new(device, inodes["plain"]),
                            RawInode::new(device, inodes["vanished"]),
                        ][..]
                    ),
                    "{case}: .., covered, lower, plain and vanished, after ."
                );
                assert_eq!(
                    entries_asked(&guest, fd, asked_before),
                    [".."],
                    "{case}: no entry but .. is asked"
                );
            }
        }
    }

    // Asking every entry is never less faithful than asking fewer, so an
    // `fstatfs` that fails -- with the errno a seccomp filter's
    // `SECCOMP_RET_ERRNO` gives it, or `EFAULT` from an unwritable stack
    // scratch -- asks every entry, never fails the getdents, and is not
    // remembered: the next getdents asks again.
    #[tokio::test]
    async fn getdents_asks_every_entry_when_the_filesystem_type_is_unknown() {
        for unwritable_scratch in [false, true] {
            for tracked in [false, true] {
                let (_dir, fd, _, _) = directory_with_entries();
                let scratch = Pages::map(1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.answers_determinize_inode = true;
                if tracked {
                    tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                        .await
                        .expect("precondition: Detcore tracks the directory");
                }
                // The descriptor's device is resolved first and is not
                // what is measured here.
                guest.fstat_answers = vec![(fd, stat_answer(1, 1, libc::S_IFDIR | 0o755))];
                if unwritable_scratch {
                    guest.commit_error = Some(Errno::EFAULT);
                } else {
                    guest.fstatfs_answer = Some(Err(Errno::ENOSYS));
                }
                let asked_before = guest.fstatat_paths.len();
                let case = format!("unwritable scratch {unwritable_scratch}, tracked {tracked}");

                let first = getdents64_of(&tool, &mut guest, fd).await;
                let second = getdents64_of(&tool, &mut guest, fd).await;
                let cached = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.directory_entry_lookup());
                close_unless_detcore_did(&guest, fd);

                assert!(
                    first.as_ref().is_ok_and(|len| *len > 0),
                    "{case}: {first:?}"
                );
                assert!(second.as_ref().is_ok(), "{case}: {second:?}");
                assert_eq!(
                    entries_asked(&guest, fd, asked_before),
                    [".", "..", "covered", "lower", "plain", "vanished"],
                    "{case}: every entry is asked"
                );
                // An unwritable scratch fails before the `fstatfs` is
                // injected; a refused one is injected on each getdents.
                assert_eq!(
                    guest.fstatfs_calls.len(),
                    if unwritable_scratch { 0 } else { 2 },
                    "{case}: each getdents asks the type again"
                );
                let expected_cache = if tracked { Ok(None) } else { Err(Errno::EBADF) };
                assert_eq!(cached, expected_cache, "{case}: nothing is remembered");
            }
        }
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, finding
    // F2. A seccomp filter the guest inherited may trap the injected
    // `fstatfs` (`SECCOMP_RET_TRAP`): the ptrace backend then reports
    // `ENOSYS` and requeues the `SIGSYS`, which reaches the guest for a call
    // it never made. With `seccomp_may_refuse_entry_lookup_syscalls` set, which the launcher
    // sets when a filter it runs under may refuse the call (or it cannot
    // tell), no `fstatfs` is injected, so the
    // guest gets no `SIGSYS`; every entry is asked and keyed exactly as when
    // the type is unknown, and nothing is remembered. Both getdents paths,
    // two reads each.
    #[tokio::test]
    async fn getdents_where_seccomp_may_refuse_the_lookup_injects_no_fstatfs() {
        for tracked in [false, true] {
            let (dir, unknown_fd, _, _) = directory_with_entries();
            let trapped_fd = std::fs::File::open(dir.path()).unwrap().into_raw_fd();
            let mut runs = Vec::new();
            for (fd, inherited) in [(unknown_fd, false), (trapped_fd, true)] {
                let scratch = Pages::map(1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.answers_determinize_inode = true;
                guest.config.seccomp_may_refuse_entry_lookup_syscalls = inherited;
                if inherited {
                    guest.fstatfs_traps = true;
                } else {
                    guest.fstatfs_answer = Some(Err(Errno::ENOSYS));
                }
                if tracked {
                    tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                        .await
                        .expect("precondition: Detcore tracks the directory");
                }
                let case = format!("tracked {tracked}, inherited filter {inherited}");
                let asked_before = guest.fstatat_paths.len();
                let keyed_before = guest.determinized.lock().unwrap().len();

                let first = getdents64_of(&tool, &mut guest, fd).await;
                let second = getdents64_of(&tool, &mut guest, fd).await;
                let cached = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.directory_entry_lookup());
                close_unless_detcore_did(&guest, fd);

                assert!(
                    first.as_ref().is_ok_and(|len| *len > 0),
                    "{case}: {first:?}"
                );
                assert!(second.as_ref().is_ok(), "{case}: {second:?}");
                assert_eq!(
                    entries_asked(&guest, fd, asked_before),
                    [".", "..", "covered", "lower", "plain", "vanished"],
                    "{case}: every entry is asked"
                );
                let expected_cache = if tracked { Ok(None) } else { Err(Errno::EBADF) };
                assert_eq!(cached, expected_cache, "{case}: nothing is remembered");
                if inherited {
                    assert_eq!(guest.sigsys_queued, 0, "{case}: the guest gets no SIGSYS");
                    assert_eq!(guest.fstatfs_calls, [], "{case}: no fstatfs is injected");
                }
                runs.push(guest.determinized.lock().unwrap()[keyed_before..].to_vec());
            }
            assert_eq!(
                runs[1], runs[0],
                "tracked {tracked}: the same keys as when the type is unknown"
            );
        }
    }

    // A descriptor Detcore tracks asks its filesystem's type once, through a
    // real `fstatfs` in the guarded stack scratch that leaves the scratch as
    // it was, and keeps the answer on its open file description; one it does
    // not track asks on every getdents. The answer is the real filesystem's.
    #[tokio::test]
    async fn getdents_asks_a_tracked_directorys_filesystem_type_once() {
        for tracked in [false, true] {
            let (dir, fd, _, device) = directory_with_entries();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            if tracked {
                tool.add_fd(&mut guest, fd, OFlag::O_RDONLY, FdType::Regular)
                    .await
                    .expect("precondition: Detcore tracks the directory");
            }
            let real = nix::sys::statfs::statfs(dir.path()).unwrap();
            // On overlayfs (Podman's `/tmp`), `lower`, the first file in name
            // order after the directory `covered`, can narrow it.
            let lower = nix::sys::stat::lstat(&dir.path().join("lower")).unwrap();
            let lower_entry = DirEntry {
                name: b"lower".to_vec(),
                ino: lower.st_ino,
                off: 0,
                ty: libc::DT_REG,
            };
            let expected = EntryLookup::of_filesystem(real.filesystem_type().0).learn(
                &lower_entry,
                &lower,
                device,
            );

            let first = getdents64_of(&tool, &mut guest, fd).await;
            let second = getdents64_of(&tool, &mut guest, fd).await;
            let cached = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.directory_entry_lookup());
            close_unless_detcore_did(&guest, fd);

            assert!(
                first.as_ref().is_ok_and(|len| *len > 0),
                "tracked {tracked}: {first:?}"
            );
            assert!(second.as_ref().is_ok(), "tracked {tracked}: {second:?}");
            let expected_calls = if tracked {
                vec![(fd, true)]
            } else {
                vec![(fd, true), (fd, true)]
            };
            assert_eq!(guest.fstatfs_calls, expected_calls, "tracked {tracked}");
            assert_eq!(guest.unguarded_scratch_writes(), 0, "tracked {tracked}");
            if tracked {
                assert_eq!(cached, Ok(Some(expected)), "the real filesystem's lookup");
            } else {
                assert_eq!(cached, Err(Errno::EBADF), "nowhere to keep it");
            }
        }
    }

    /// Enables every event at INFO and above, as `--log info` does, and keeps
    /// the memory-map DETLOG records. INFO is what makes that DETLOG read the
    /// guest at all.
    #[derive(Clone, Default)]
    struct MemoryRecords(Arc<std::sync::Mutex<Vec<String>>>);

    impl MemoryRecords {
        fn taken(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    struct MessageText(Option<String>);

    impl tracing::field::Visit for MessageText {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    impl tracing::Subscriber for MemoryRecords {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::INFO
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut text = MessageText(None);
            event.record(&mut text);
            if let Some(text) = text.0
                && text.contains("[memory]")
            {
                self.0.lock().unwrap().push(text);
            }
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A guest for calls dispatched through the whole `handle_syscall_event`,
    /// and the Detcore a backend builds from that guest's config. With
    /// `heap`, the run has `--detlog-heap` and the backend reports `heap` as
    /// the guest's heap, for the memory-map DETLOG to hash after each call.
    /// The harness reads no clock and no registers, so the config has no PMU
    /// timeslice and the backend owns the syscall-clobbered registers, as DBT
    /// does.
    fn dispatched_guest(
        stack_writable: bool,
        mmap_fails: bool,
        heap: Option<&Pages>,
    ) -> (Detcore, ScriptedGuest) {
        let (_, mut guest) = ScriptedGuest::new(stack_writable, mmap_fails);
        guest.config.max_timeslice = None;
        guest.config.sequentialize_threads = false;
        guest.config.backend.virtualizes_syscall_clobbers = true;
        if let Some(heap) = heap {
            guest.config.detlog_heap = true;
            guest.detlog_regions = Some(vec![reverie::DetlogMemoryRegion {
                kind: reverie::DetlogRegionKind::Heap,
                start: heap.address as u64,
                end: (heap.address + heap.len) as u64,
            }]);
        }
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &guest.config);
        (tool, guest)
    }

    /// A private read-only `mmap` of `fd`, a regular file the guest has as
    /// stdout, whose handler asks the guest for the file's own identity.
    fn stdio_file_mmap(fd: RawFd) -> Syscall {
        Syscall::Mmap(
            syscalls::Mmap::new()
                .with_addr(None)
                .with_len(page_size())
                .with_prot(ProtFlags::PROT_READ)
                .with_flags(MapFlags::MAP_PRIVATE)
                .with_fd(fd)
                .with_offset(0),
        )
    }

    fn unmap_file_mappings(guest: &ScriptedGuest) {
        for &(address, len) in &guest.file_mapped {
            assert_eq!(
                unsafe { libc::munmap(address as *mut libc::c_void, len) },
                0
            );
        }
    }

    // A refusal is the call's result through the whole dispatcher, whatever
    // an observer that runs after the handler does. Here the memory-map
    // DETLOG (`--detlog-heap` under INFO logging) fails on its own, reading a
    // heap page the guest cannot read, while the `mmap` handler refuses (no
    // stack scratch, no transient page for the stdio identity). Were the
    // observer to run, its EFAULT would take the refusal's place: an errno
    // DBT hands the guest, which then runs on.
    #[tokio::test]
    async fn a_refusal_outlives_a_failing_observer_in_the_dispatcher() {
        let records = MemoryRecords::default();
        let _info = tracing::subscriber::set_default(records.clone());
        let unreadable = Pages::map(1, 0);
        let (tool, mut guest) = dispatched_guest(false, true, Some(&unreadable));
        let observed = tool.detlog_memory_maps(&mut guest);
        assert!(
            matches!(observed, Err(Error::Errno(Errno::EFAULT))),
            "precondition: the memory-map DETLOG fails on its own: {observed:?}"
        );
        let (fd, _) = stdio_backed_by_a_file(&mut guest);

        let result = tool
            .handle_syscall_event(&mut guest, stdio_file_mmap(fd))
            .await;
        unmap_file_mappings(&guest);
        close_unless_detcore_did(&guest, fd);

        assert_typed_refusal("dispatched mmap", result.map(|_| ()), Errno::ENOMEM);
        assert_eq!(records.taken(), Vec::<String>::new());
    }

    // The control: a call that is not refused still runs the observers after
    // its handler. Same guest and call, with a stack scratch to ask in and a
    // readable heap page, so the handler maps the file and the memory-map
    // DETLOG records the heap.
    #[tokio::test]
    async fn an_unrefused_call_still_runs_the_observers_in_the_dispatcher() {
        let records = MemoryRecords::default();
        let _info = tracing::subscriber::set_default(records.clone());
        let readable = Pages::map(1, 1);
        let (tool, mut guest) = dispatched_guest(true, false, Some(&readable));
        let (fd, _) = stdio_backed_by_a_file(&mut guest);

        let result = tool
            .handle_syscall_event(&mut guest, stdio_file_mmap(fd))
            .await;
        unmap_file_mappings(&guest);
        close_unless_detcore_did(&guest, fd);

        assert!(result.is_ok(), "the dispatched mmap failed: {result:?}");
        assert_eq!(guest.file_mapped.len(), 1, "the handler mapped the file");
        let records = records.taken();
        assert!(
            records.len() == 1 && records[0].contains("Heap"),
            "the memory-map DETLOG must record the heap once after the call: {records:?}"
        );
    }

    // A refused call takes no turn after its handler: the post-hook would end
    // an expired timeslice and hand the turn to another thread before the
    // backend stops the refusing process. Threads are sequentialized, as in a
    // `hermit run`, so ending a slice asks the scheduler for the next turn,
    // an RPC the harness panics on. The thread's slice ends 1 ns after its
    // current time, so the call's own charge expires it.
    #[tokio::test]
    async fn a_refused_call_leaves_the_dispatcher_before_the_post_hook_takes_a_turn() {
        let (_, mut guest) = dispatched_guest(false, true, None);
        guest.config.sequentialize_threads = true;
        let tool = <Detcore as Tool>::new(Pid::from_raw(1), &guest.config);
        assert!(
            crate::syscall_time::cost_ns(Sysno::mmap) > 0,
            "precondition: the call's charge advances the thread's time"
        );
        let slice_end = guest.thread.thread_logical_time.as_nanos() + LogicalTime::from_nanos(1);
        guest.thread.end_of_timeslice = Some(slice_end);
        let slices = guest.thread.stats.timeslice_count;
        let (fd, _) = stdio_backed_by_a_file(&mut guest);

        let result = tool
            .handle_syscall_event(&mut guest, stdio_file_mmap(fd))
            .await;
        unmap_file_mappings(&guest);
        close_unless_detcore_did(&guest, fd);

        assert_typed_refusal("dispatched mmap", result.map(|_| ()), Errno::ENOMEM);
        assert!(
            guest.thread.timeslice_expired(),
            "precondition: the call's charge expired the slice"
        );
        assert_eq!(
            (
                guest.thread.end_of_timeslice,
                guest.thread.stats.timeslice_count
            ),
            (Some(slice_end), slices),
            "the refused call must leave the expired slice for the backend's stop"
        );
    }

    // Step 2 of `mapping_stat_identity`: the reader's own executable after it
    // was unlinked -- ` (deleted)`, so no path candidate -- and with no record,
    // as `execve` maps it, is keyed on the guest's `stat` of `/proc/self/exe`,
    // not on the header's device. For an address space the reader cannot name
    // (`MappingSubject::Unknown`) `/proc/self` is not that process, and the
    // header decides.
    #[tokio::test]
    async fn mapping_stat_identity_keys_the_readers_deleted_executable_on_proc_self_exe() {
        let exe = std::fs::metadata("/proc/self/exe").unwrap();
        let identity = RawInode::new(exe.dev(), exe.ino());
        assert_ne!(identity.dev, HEADER_DEVICE);
        let key = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: identity.ino,
            pathname: "/usr/bin/replaced-executable (deleted)".to_owned(),
        };
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        // The test binary is not deleted, so its link's text is scripted.
        guest.readlink_answers.push((
            b"/proc/self/exe".to_vec(),
            key.pathname.clone().into_bytes(),
        ));
        let _mounts = ScriptedMountinfo::proving(Path::new("/proc/self/exe"), HEADER_DEVICE);
        let exe_proof = [
            &[
                Sysno::newfstatat,
                Sysno::mmap,
                Sysno::readlinkat,
                Sysno::munmap,
            ][..],
            &proof_calls(),
        ]
        .concat();

        assert_eq!(
            tool.mapping_recorded_identity(&guest, &key, RECORDED_START, MappingSubject::Reader),
            None,
            "precondition: `execve` maps the executable without a record"
        );
        let own = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Reader)
            .await;
        assert_eq!(own.expect("mapping_stat_identity failed"), identity);
        assert_eq!(guest.injected, exe_proof);
        assert_eq!(guest.fstatat_paths, [b"/proc/self/exe".to_vec()]);
        assert_eq!(guest.readlink_paths, [b"/proc/self/exe".to_vec()]);
        assert_eq!(guest.statx_paths, [b"/proc/self/exe".to_vec()]);

        let other = tool
            .mapping_stat_identity(&mut guest, &key, MappingSubject::Unknown)
            .await;
        assert_eq!(
            other.expect("mapping_stat_identity failed"),
            RawInode::new(HEADER_DEVICE, identity.ino),
            "an unknown address space's line must not borrow the reader's executable"
        );
        assert_eq!(guest.injected, exe_proof, "no second stat");
    }

    // The reader's mapping records describe only its own address space. For a
    // snapshot of another one, even a record with the header's inode -- the
    // same inode number on another device -- must not key the line.
    #[tokio::test]
    async fn another_address_spaces_maps_line_ignores_the_readers_record() {
        use crate::procfs::MappingIdentityMinter;

        let (file, identity, key) = mapped_path();
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        let recorded = RawInode::new(RECORD_DEVICE, identity.ino);
        guest.thread.map_file(RECORDED_START, page_size(), recorded);
        let path_proof = [&[Sysno::newfstatat][..], &proof_calls()].concat();

        let own = GuestMappingMinter::new(&tool, &mut guest, MappingSubject::Reader)
            .recorded_identity(&key, RECORDED_START);
        assert_eq!(
            own,
            Some(recorded),
            "control: the reader's own snapshot is keyed on its record"
        );
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);

        let other_record = GuestMappingMinter::new(&tool, &mut guest, MappingSubject::Unknown)
            .recorded_identity(&key, RECORDED_START);
        assert_eq!(
            other_record, None,
            "another address space's line must not take the reader's record"
        );
        let other = GuestMappingMinter::new(&tool, &mut guest, MappingSubject::Unknown)
            .stat_identity(&key)
            .await;
        assert_eq!(
            other.expect("stat_identity failed for another address space"),
            identity,
            "another address space's line must be keyed on the path's stat, not the reader's record"
        );
        assert_eq!(guest.injected, path_proof);
        assert_eq!(guest.fstatat_paths, [key.pathname.as_bytes().to_vec()]);

        let numbered_record =
            GuestMappingMinter::new(&tool, &mut guest, MappingSubject::Process(1))
                .recorded_identity(&key, RECORDED_START);
        assert_eq!(
            numbered_record, None,
            "a numbered task's line must not take the reader's record"
        );
        let numbered = GuestMappingMinter::new(&tool, &mut guest, MappingSubject::Process(1))
            .stat_identity(&key)
            .await;
        assert_eq!(
            numbered.expect("stat_identity failed for a numbered address space"),
            identity,
            "a numbered task's line must be keyed on the path's stat, not the reader's record"
        );
        assert_eq!(guest.injected, path_proof.repeat(2));
    }

    /// An inode number no file the tests below use has, shared by the
    /// header of several scripted maps lines.
    const SHARED_INODE: u64 = 0x5ea1;

    /// A device number no test filesystem has, standing in for a second
    /// filesystem on which a file's inode number coincides with another's, or
    /// for the device of an overlayfs layer.
    const OTHER_DEVICE: u64 = 0x7e57_0f05;

    /// A one-page `maps` line at `start` with `key`'s header.
    fn scripted_maps_line(start: usize, key: &crate::procfs::MappingKey) -> String {
        let line = format!(
            "{:x}-{:x} r--p 00000000 {:02x}:{:02x} {} {}",
            start,
            start + page_size(),
            libc::major(key.device),
            libc::minor(key.device),
            key.inode,
            key.pathname
        );
        assert_eq!(
            crate::procfs::mapping_header_key(&line),
            Some(key.clone()),
            "precondition: the scripted maps line names the file"
        );
        line
    }

    /// The inode column of each line of the `maps` snapshot `raw`, minted and
    /// rendered as the read handler mints and renders it.
    async fn minted_maps_inodes(
        tool: &Detcore,
        guest: &mut ScriptedGuest,
        raw: &str,
        subject: MappingSubject,
    ) -> Result<Vec<String>, Error> {
        let table = crate::procfs::mint_mapping_identities(
            raw.as_bytes(),
            &BTreeMap::new(),
            &mut GuestMappingMinter::new(tool, guest, subject),
        )
        .await?;
        let rendered = String::from_utf8(crate::procfs::sanitize_maps(raw.as_bytes(), &table))
            .expect("the rendered snapshot is text");
        Ok(rendered
            .lines()
            .map(|line| line.split_whitespace().nth(4).unwrap().to_owned())
            .collect())
    }

    // Codex review of https://github.com/rrnewton/hermit/pull/3255, round 10,
    // F3: "resolve identity per range using its own provenance". Two lines can
    // share one header -- device, inode and pathname -- and still map two
    // files: here two deleted files whose inode numbers coincide on two
    // devices, each mapped through a descriptor Detcore tracks. Each line is
    // keyed on its own range's record. Keying the header on the first record
    // found gave the second file the first one's inode.
    #[tokio::test]
    async fn maps_lines_of_one_header_keep_their_own_records() {
        let key = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: SHARED_INODE,
            pathname: "/scripted/shared (deleted)".to_owned(),
        };
        let second_start = RECORDED_START + 2 * page_size();
        let raw = format!(
            "{}\n{}\n",
            scripted_maps_line(RECORDED_START, &key),
            scripted_maps_line(second_start, &key)
        );
        let first = RawInode::new(RECORD_DEVICE, SHARED_INODE);
        let second = RawInode::new(OTHER_DEVICE, SHARED_INODE);
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.answers_determinize_device = true;
        guest.thread.map_file(RECORDED_START, page_size(), first);
        guest.thread.map_file(second_start, page_size(), second);

        let inodes = minted_maps_inodes(&tool, &mut guest, &raw, MappingSubject::Reader)
            .await
            .expect("both lines are recorded, so nothing is ambiguous");

        assert_eq!(
            *guest.determinized.lock().unwrap(),
            [first, second],
            "each line's own record keys its inode, in text order"
        );
        assert_eq!(
            inodes,
            [
                FIRST_SCRIPTED_INODE.to_string(),
                (FIRST_SCRIPTED_INODE + 1).to_string()
            ]
        );
        assert!(
            guest.injected.is_empty(),
            "a recorded line needs no lookup: {:?}",
            guest.injected
        );
        assert_eq!(
            *guest.determinized_devices.lock().unwrap(),
            [HEADER_DEVICE],
            "the device column is keyed on the header's device, once"
        );
    }

    /// Asserts that `result` is the typed refusal of the ambiguous maps line
    /// at `start`.
    fn assert_ambiguous_line_refused(result: &Result<Vec<String>, Error>, start: usize) {
        let refused = result
            .as_ref()
            .err()
            .and_then(IdentityLookupRefused::carried_by);
        assert!(
            refused.is_some_and(|refused| {
                let message = refused.to_string();
                message.contains(&format!("{start:#x}")) && message.contains("ambiguous")
            }),
            "the maps line at {start:#x} must be refused as ambiguous, typed so that it \
             stops the reading process: {result:?}"
        );
    }

    // The same F3, "and refuse ambiguous evidence". A line with no record of
    // its own shares its header with a recorded line, and the file that
    // record names is not the file the line resolves to. The header cannot
    // say which file the unrecorded line maps, so the snapshot is refused,
    // typed so that it stops the reading process, before any identity is
    // minted. Three shapes: no step names the file, so the header's pair
    // stands; the line's path is proven to name another file than the
    // record's; and two lines recorded with two files, one of which is the
    // unrecorded line's.
    #[tokio::test]
    async fn an_unrecorded_maps_line_contradicted_by_its_headers_record_is_refused() {
        let second_start = RECORDED_START + 2 * page_size();
        let third_start = RECORDED_START + 4 * page_size();
        let exe = std::fs::metadata("/proc/self/exe").unwrap();
        assert_ne!(
            exe.ino(),
            SHARED_INODE,
            "precondition: step 2 names no file"
        );
        let deleted = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: SHARED_INODE,
            pathname: "/scripted/shared (deleted)".to_owned(),
        };
        let header = RawInode::new(HEADER_DEVICE, SHARED_INODE);

        // No step names the file: the line's identity is the header's pair.
        let raw = format!(
            "{}\n{}\n",
            scripted_maps_line(RECORDED_START, &deleted),
            scripted_maps_line(second_start, &deleted)
        );
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.answers_determinize_device = true;
        guest.thread.map_file(
            RECORDED_START,
            page_size(),
            RawInode::new(RECORD_DEVICE, SHARED_INODE),
        );
        let result = minted_maps_inodes(&tool, &mut guest, &raw, MappingSubject::Reader).await;
        assert_ambiguous_line_refused(&result, second_start);
        assert!(
            guest.determinized.lock().unwrap().is_empty(),
            "nothing minted"
        );
        assert!(guest.determinized_devices.lock().unwrap().is_empty());
        assert_eq!(guest.fstatat_paths, [b"/proc/self/exe".to_vec()]);

        // The path is proven to name a file other than the record's.
        let (file, identity, key) = mapped_path();
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let raw = format!(
            "{}\n{}\n",
            scripted_maps_line(RECORDED_START, &key),
            scripted_maps_line(second_start, &key)
        );
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.answers_determinize_device = true;
        guest.thread.map_file(
            RECORDED_START,
            page_size(),
            RawInode::new(RECORD_DEVICE, identity.ino),
        );
        let result = minted_maps_inodes(&tool, &mut guest, &raw, MappingSubject::Reader).await;
        assert_ambiguous_line_refused(&result, second_start);
        assert!(
            guest.determinized.lock().unwrap().is_empty(),
            "nothing minted"
        );
        assert_eq!(guest.fstatat_paths, [key.pathname.as_bytes().to_vec()]);

        // Two records name two files, and the unrecorded line's identity, the
        // header's pair, is one of them: the other still contradicts it.
        let raw = format!(
            "{}\n{}\n{}\n",
            scripted_maps_line(RECORDED_START, &deleted),
            scripted_maps_line(second_start, &deleted),
            scripted_maps_line(third_start, &deleted)
        );
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.answers_determinize_device = true;
        guest.thread.map_file(RECORDED_START, page_size(), header);
        guest.thread.map_file(
            second_start,
            page_size(),
            RawInode::new(OTHER_DEVICE, SHARED_INODE),
        );
        let result = minted_maps_inodes(&tool, &mut guest, &raw, MappingSubject::Reader).await;
        assert_ambiguous_line_refused(&result, third_start);
        assert!(
            guest.determinized.lock().unwrap().is_empty(),
            "nothing minted"
        );
    }

    // Controls for the refusal above, so that it cannot pass by refusing every
    // snapshot with a recorded line: a line with no record of its own is keyed
    // as before when the file its header's record names is the file the line
    // resolves to, and no record is consulted when the snapshot shows another
    // address space or metadata is not virtualized.
    #[tokio::test]
    async fn an_unrecorded_maps_line_that_agrees_with_its_headers_record_is_keyed() {
        let second_start = RECORDED_START + 2 * page_size();
        let (file, identity, key) = mapped_path();
        let _mounts = ScriptedMountinfo::proving(file.path(), HEADER_DEVICE);
        let raw = format!(
            "{}\n{}\n",
            scripted_maps_line(RECORDED_START, &key),
            scripted_maps_line(second_start, &key)
        );
        let scratch = Pages::map(1, 1);
        let first_inode = vec![FIRST_SCRIPTED_INODE.to_string(); 2];
        // Each of the two lines sends its own numbering request, as the
        // request-counting pool requires, and both name one file.

        // The record names the file the path is proven to name.
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.answers_determinize_inode = true;
        guest.answers_determinize_device = true;
        guest.thread.map_file(RECORDED_START, page_size(), identity);
        let inodes = minted_maps_inodes(&tool, &mut guest, &raw, MappingSubject::Reader).await;
        assert_eq!(inodes.expect("the record agrees"), first_inode);
        assert_eq!(*guest.determinized.lock().unwrap(), [identity; 2]);

        // Another address space: the reader's record, which names another
        // file, is not consulted, so nothing contradicts the path.
        for subject in [MappingSubject::Unknown, MappingSubject::Process(1)] {
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.answers_determinize_device = true;
            guest.thread.map_file(
                RECORDED_START,
                page_size(),
                RawInode::new(RECORD_DEVICE, identity.ino),
            );
            let inodes = minted_maps_inodes(&tool, &mut guest, &raw, subject).await;
            assert_eq!(inodes.expect("no record is consulted"), first_inode);
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [identity; 2],
                "{subject:?}"
            );
        }

        // Metadata not virtualized: the header's pair keys every line.
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        guest.config.virtualize_metadata = false;
        guest.answers_determinize_inode = true;
        guest.answers_determinize_device = true;
        guest.thread.map_file(
            RECORDED_START,
            page_size(),
            RawInode::new(RECORD_DEVICE, identity.ino),
        );
        let inodes = minted_maps_inodes(&tool, &mut guest, &raw, MappingSubject::Reader).await;
        assert_eq!(inodes.expect("no record is consulted"), first_inode);
        assert_eq!(
            *guest.determinized.lock().unwrap(),
            [RawInode::new(HEADER_DEVICE, identity.ino); 2]
        );
        assert!(guest.injected.is_empty(), "injected {:?}", guest.injected);
    }

    /// A running process other than the test's, whose executable stands in
    /// for another guest process's. Killed and reaped on drop.
    struct OtherProcess(std::process::Child);

    impl OtherProcess {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("600")
                    .spawn()
                    .expect("cannot spawn sleep"),
            )
        }

        fn pid(&self) -> i32 {
            i32::try_from(self.0.id()).unwrap()
        }

        /// The raw identity of its executable, as `stat` of
        /// `/proc/<pid>/exe` reports it.
        fn executable(&self) -> RawInode {
            let exe = std::fs::metadata(format!("/proc/{}/exe", self.pid())).unwrap();
            RawInode::new(exe.dev(), exe.ino())
        }
    }

    impl Drop for OtherProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A running copy of `sleep` whose file was unlinked once it ran, so its
    /// exe link and its maps line both read `<copy> (deleted)`. The copy sits
    /// beside the test binary, where executing is allowed.
    struct DeletedExecutable {
        process: OtherProcess,
        directory: tempfile::TempDir,
    }

    impl DeletedExecutable {
        fn spawn() -> Self {
            let beside = std::env::current_exe().unwrap();
            let directory = tempfile::tempdir_in(beside.parent().unwrap()).unwrap();
            let copy = directory.path().join("sleep");
            {
                let original = OtherProcess::spawn();
                std::fs::copy(format!("/proc/{}/exe", original.pid()), &copy).unwrap();
            }
            // Another test's fork can briefly hold the copy's write
            // descriptor, and executing a file open for writing is ETXTBSY.
            let mut attempts = 0;
            let child = loop {
                match std::process::Command::new(&copy).arg("600").spawn() {
                    Ok(child) => break child,
                    Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 100 => {
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("cannot run {copy:?}: {error}"),
                }
            };
            // `spawn` returns once the child has executed the copy.
            std::fs::remove_file(&copy).unwrap();
            Self {
                process: OtherProcess(child),
                directory,
            }
        }

        fn link(&self) -> String {
            format!("/proc/{}/exe", self.process.pid())
        }

        /// Its maps pathname, which its exe link also reads.
        fn pathname(&self) -> String {
            format!(
                "{}/sleep (deleted)",
                self.directory.path().to_str().unwrap()
            )
        }
    }

    // Step 2 for a task the maps file names by number: its executable after
    // it was unlinked -- ` (deleted)`, so no path candidate -- is keyed on the
    // guest's `stat` of `/proc/<pid>/exe`, not on the header's device. Before
    // https://github.com/rrnewton/hermit/pull/3255's fifth round no number
    // reached this function, and the header decided.
    #[tokio::test]
    async fn mapping_stat_identity_keys_a_numbered_tasks_deleted_executable_on_its_exe_link() {
        let other = DeletedExecutable::spawn();
        let identity = other.process.executable();
        assert_ne!(identity.dev, HEADER_DEVICE);
        let link = other.link();
        let key = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: identity.ino,
            pathname: other.pathname(),
        };
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        let _mounts = ScriptedMountinfo::proving(Path::new(&link), HEADER_DEVICE);

        let numbered = tool
            .mapping_stat_identity(
                &mut guest,
                &key,
                MappingSubject::Process(other.process.pid()),
            )
            .await;
        assert_eq!(
            numbered.expect("mapping_stat_identity failed"),
            identity,
            "another task's deleted executable must be keyed on its /proc/<pid>/exe"
        );
        assert_eq!(
            guest.injected,
            [
                &[
                    Sysno::newfstatat,
                    Sysno::mmap,
                    Sysno::readlinkat,
                    Sysno::munmap,
                ][..],
                &proof_calls(),
            ]
            .concat()
        );
        assert_eq!(guest.fstatat_paths, [link.clone().into_bytes()]);
        assert_eq!(guest.readlink_paths, [link.clone().into_bytes()]);
        assert_eq!(guest.statx_paths, [link.into_bytes()]);
    }

    // Step 2 borrows the executable's identity only with proof that the
    // line's file IS the executable: the link must read as the line's
    // pathname, and the executable must lie on the header's superblock. An
    // equal inode number alone -- on another filesystem, in a non-executable
    // mapping, or for a live file literally named `x (deleted)` -- keeps the
    // header's pair.
    #[tokio::test]
    async fn step_two_needs_the_executables_pathname_and_superblock() {
        let other = DeletedExecutable::spawn();
        let identity = other.process.executable();
        let link = other.link();
        let header = RawInode::new(HEADER_DEVICE, identity.ino);
        let literal = tempfile::tempdir().unwrap();
        let literal_path = literal.path().join("x (deleted)");
        std::fs::write(&literal_path, b"not the executable").unwrap();
        let scratch = Pages::map(1, 1);
        let text_mismatch = [
            Sysno::newfstatat,
            Sysno::mmap,
            Sysno::readlinkat,
            Sysno::munmap,
        ];
        let device_mismatch = [&text_mismatch[..], &proof_calls()].concat();
        for (pathname, superblock, expected_calls, why) in [
            (
                other.pathname(),
                RECORD_DEVICE,
                &device_mismatch[..],
                "the executable on another superblock",
            ),
            (
                literal_path.to_str().unwrap().to_owned(),
                HEADER_DEVICE,
                &text_mismatch[..],
                "a live file literally named `x (deleted)`",
            ),
            (
                format!("{}/libgone.so (deleted)", literal.path().to_str().unwrap()),
                HEADER_DEVICE,
                &text_mismatch[..],
                "a non-executable mapping with the executable's inode number",
            ),
        ] {
            let key = crate::procfs::MappingKey {
                device: HEADER_DEVICE,
                inode: identity.ino,
                pathname,
            };
            let _mounts = ScriptedMountinfo::proving(Path::new(&link), superblock);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            let resolved = tool
                .mapping_stat_identity(
                    &mut guest,
                    &key,
                    MappingSubject::Process(other.process.pid()),
                )
                .await;
            assert_eq!(
                resolved.expect("mapping_stat_identity failed"),
                header,
                "{why} must not borrow the executable's identity"
            );
            assert_eq!(guest.injected, expected_calls, "{why}");
            assert_eq!(guest.fstatat_paths, [link.clone().into_bytes()], "{why}");
        }
    }

    // Through the numeric spelling's classification at open time, with the
    // opener's ids from the guest's injected `getpid` and `gettid`: the
    // reader's own thread-group id or thread id binds its own address space,
    // whose record keys a deleted file; another task's number keys that task's
    // deleted executable on its exe link and never consults the reader's
    // record.
    #[tokio::test]
    async fn numeric_maps_spellings_resolve_through_the_task_they_name() {
        use crate::procfs::MappingIdentityMinter;

        let own_tgid = i32::try_from(std::process::id()).unwrap();
        // SAFETY: gettid has no preconditions.
        let own_tid = i32::try_from(unsafe { libc::syscall(libc::SYS_gettid) }).unwrap();
        let deleted = DeletedExecutable::spawn();
        let other = &deleted.process;
        let other_executable = other.executable();
        let _mounts = ScriptedMountinfo::proving(Path::new(&deleted.link()), HEADER_DEVICE);
        let scratch = Pages::map(1, 1);
        let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
        let reader_mm = guest.thread.mm_id;

        // A file the reader mapped and then unlinked: only its record names it.
        let (_file, identity, mut key) = mapped_path();
        key.pathname.push_str(" (deleted)");
        let recorded = RawInode::new(RECORD_DEVICE, identity.ino);
        guest.thread.map_file(RECORDED_START, page_size(), recorded);

        for own in [own_tgid, own_tid] {
            let path = format!("/proc/{own}/maps");
            let mut file = ProcfsFile::from_path(Path::new(&path)).unwrap();
            guest.injected.clear();
            Detcore::bind_procfs_mapping_subject(&mut guest, &mut file)
                .await
                .expect("binding failed");
            assert_eq!(guest.injected, [Sysno::getpid, Sysno::gettid], "{path}");
            let subject = file.mapping_subject(reader_mm);
            assert_eq!(subject, MappingSubject::Reader, "{path}");
            guest.injected.clear();
            let resolved = GuestMappingMinter::new(&tool, &mut guest, subject)
                .recorded_identity(&key, RECORDED_START);
            assert_eq!(
                resolved,
                Some(recorded),
                "{path}: the reader's own deleted file must be keyed on its record"
            );
            assert!(guest.injected.is_empty(), "{path}: {:?}", guest.injected);
        }

        let path = format!("/proc/{}/maps", other.pid());
        let mut file = ProcfsFile::from_path(Path::new(&path)).unwrap();
        guest.injected.clear();
        Detcore::bind_procfs_mapping_subject(&mut guest, &mut file)
            .await
            .expect("binding failed");
        let subject = file.mapping_subject(reader_mm);
        assert_eq!(subject, MappingSubject::Process(other.pid()), "{path}");
        let other_key = crate::procfs::MappingKey {
            device: HEADER_DEVICE,
            inode: other_executable.ino,
            pathname: deleted.pathname(),
        };
        // The reader's record at the same address has the same inode number
        // and must not be used for another task's line.
        guest.thread.map_file(
            RECORDED_START,
            page_size(),
            RawInode::new(RECORD_DEVICE, other_executable.ino),
        );
        guest.injected.clear();
        guest.fstatat_paths.clear();
        assert_eq!(
            GuestMappingMinter::new(&tool, &mut guest, subject)
                .recorded_identity(&other_key, RECORDED_START),
            None,
            "{path}: another task's line must not take the reader's record"
        );
        let resolved = GuestMappingMinter::new(&tool, &mut guest, subject)
            .stat_identity(&other_key)
            .await;
        assert_eq!(
            resolved.expect("stat_identity failed"),
            other_executable,
            "{path}: another task's deleted executable must be keyed on its exe link"
        );
        assert_eq!(
            guest.fstatat_paths,
            [format!("/proc/{}/exe", other.pid()).into_bytes()]
        );
    }
}

#[cfg(test)]
mod rebound_path_tests {
    use std::path::PathBuf;

    use super::rebound_path;

    /// A namespace change's operand is reported as the path it names, made
    /// absolute. One that could not be read (a tracer may not read a
    /// non-dumpable guest's memory) or made absolute is reported as `/`, the
    /// ancestor of every path, so that no host input change is named in that
    /// run instead of one the guest may have made.
    #[test]
    fn an_operand_that_cannot_be_established_is_reported_as_the_root() {
        assert_eq!(rebound_path(Some(PathBuf::from("/data/F"))), "/data/F");
        assert_eq!(rebound_path(None), "/");
        assert_eq!(rebound_path(Some(PathBuf::from("F"))), "/");
    }
}

#[cfg(test)]
mod untraced_code_tests {
    use std::path::Path;

    use nix::fcntl::OFlag;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::descriptor_identity;
    use super::may_change_untraced_code;
    use super::may_write_process_memory;

    const PAGE: (u64, u64) = (0x7100_0000, 0x7100_1000);

    fn args(arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> SyscallArgs {
        SyscallArgs::new(
            arg0 as usize,
            arg1 as usize,
            arg2 as usize,
            arg3 as usize,
            arg4 as usize,
            0,
        )
    }

    /// A thread cloned without CLONE_FILES has its own descriptor table, so
    /// a descriptor is read through the thread that opened it. Through its
    /// leader the same number names nothing here, which would make an
    /// ordinary write-mode open count as unreadable (a false `/` that
    /// withholds a real host input change) and would miss the thread's own
    /// process-memory file.
    #[test]
    fn a_descriptor_is_read_through_the_thread_that_opened_it() {
        use std::os::fd::AsRawFd;

        let directory = tempfile::tempdir().unwrap();
        let ordinary = directory.path().join("out");
        let named_mem = directory.path().join("mem");
        std::thread::spawn(move || {
            // SAFETY: gives only this thread a private copy of the table.
            assert_eq!(unsafe { libc::unshare(libc::CLONE_FILES) }, 0);
            // SAFETY: plain syscalls on this thread's own descriptors.
            let tid = unsafe { libc::gettid() };
            let leader = std::process::id() as i32;
            let files = [
                (std::fs::File::create(&ordinary).unwrap(), false),
                (std::fs::File::create(&named_mem).unwrap(), false),
                (
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/proc/thread-self/mem")
                        .unwrap(),
                    true,
                ),
            ];
            for (number, (file, is_process_memory)) in (900..).zip(files) {
                assert_eq!(unsafe { libc::dup2(file.as_raw_fd(), number) }, number);
                drop(file);
                let identity = descriptor_identity(tid, number);
                assert!(identity.is_some(), "{number}");
                assert_eq!(descriptor_identity(leader, number), None, "{number}");
                assert_eq!(
                    may_write_process_memory(
                        OFlag::O_WRONLY,
                        identity
                            .as_ref()
                            .map(|(link, procfs)| (link.as_path(), *procfs)),
                    ),
                    is_process_memory,
                    "{identity:?}"
                );
                assert_eq!(unsafe { libc::close(number) }, 0);
            }
        })
        .join()
        .unwrap();
    }

    /// Every call that can replace, unprotect, discard or unmap the range
    /// counts when its interval reaches any byte of it, and not when it stops
    /// short of it or starts after it.
    #[test]
    fn a_call_counts_exactly_when_it_may_change_the_range() {
        let fixed = libc::MAP_FIXED as u64 | libc::MAP_PRIVATE as u64;
        let huge = fixed | libc::MAP_HUGETLB as u64;
        let huge_1gb = (30 << libc::MAP_HUGE_SHIFT) as u64;
        let remap_fixed = (libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED) as u64;
        let remap = libc::SHM_REMAP as u64;
        let cases = [
            (
                "mmap fixed over it",
                Sysno::mmap,
                args(0x7100_0000, 0x1000, 7, fixed, 0),
                true,
            ),
            (
                "mmap fixed reaching it",
                Sysno::mmap,
                args(0x70ff_f000, 0x1001, 7, fixed, 0),
                true,
            ),
            (
                "mmap fixed below it",
                Sysno::mmap,
                args(0x70ff_f000, 0x1000, 7, fixed, 0),
                false,
            ),
            (
                "mmap fixed above it",
                Sysno::mmap,
                args(0x7100_1000, 0x1000, 7, fixed, 0),
                false,
            ),
            (
                "mmap hinted at it",
                Sysno::mmap,
                args(0x7100_0000, 0x1000, 7, 0x22, 0),
                false,
            ),
            // The host default here is 2 MiB; 0x7100_0000 is 2 MiB aligned.
            (
                "mmap fixed default huge page holding it",
                Sysno::mmap,
                args(0x7110_0000, 1, 7, huge, 0),
                true,
            ),
            (
                "mmap fixed small page there",
                Sysno::mmap,
                args(0x7110_0000, 1, 7, fixed, 0),
                false,
            ),
            (
                "mmap fixed default huge page below it",
                Sysno::mmap,
                args(0x70ff_f000, 1, 7, huge, 0),
                false,
            ),
            (
                "mmap fixed 1 GiB page holding it",
                Sysno::mmap,
                args(0x4000_0000, 1, 7, huge | huge_1gb, 0),
                true,
            ),
            (
                "mmap fixed 1 GiB page elsewhere",
                Sysno::mmap,
                args(0x8000_0000, 1, 7, huge | huge_1gb, 0),
                false,
            ),
            (
                "mprotect of it",
                Sysno::mprotect,
                args(0x7100_0000, 0x1000, 7, 0, 0),
                true,
            ),
            (
                "mprotect covering it",
                Sysno::mprotect,
                args(0x7000_0000, 0x200_0000, 7, 0, 0),
                true,
            ),
            (
                "mprotect of a zero length",
                Sysno::mprotect,
                args(0x7100_0000, 0, 7, 0, 0),
                true,
            ),
            (
                "pkey_mprotect of it",
                Sysno::pkey_mprotect,
                args(0x7100_0000, 1, 7, 0, 0),
                true,
            ),
            (
                "madvise of it",
                Sysno::madvise,
                args(0x7100_0000, 0x1000, 4, 0, 0),
                true,
            ),
            (
                "munmap of it",
                Sysno::munmap,
                args(0x7100_0000, 0x1000, 0, 0, 0),
                true,
            ),
            (
                "munmap past it",
                Sysno::munmap,
                args(0x7100_1000, 0x1000, 0, 0, 0),
                false,
            ),
            (
                "mremap from it",
                Sysno::mremap,
                args(0x7100_0000, 0x1000, 0x2000, 1, 0),
                true,
            ),
            (
                "mremap fixed onto it",
                Sysno::mremap,
                args(0x6000_0000, 0x1000, 0x1000, remap_fixed, 0x7100_0000),
                true,
            ),
            (
                "mremap fixed of a small page into its gigabyte",
                Sysno::mremap,
                args(0x6000_0000, 0x1000, 0x1000, remap_fixed, 0x4000_0000),
                false,
            ),
            (
                "mremap moving elsewhere",
                Sysno::mremap,
                args(0x6000_0000, 0x1000, 0x2000, 1, 0x7100_0000),
                false,
            ),
            (
                "shmat remapping at it",
                Sysno::shmat,
                args(3, 0x7100_0000, remap, 0, 0),
                true,
            ),
            (
                "shmat remapping below it",
                Sysno::shmat,
                args(3, 0x6000_0000, remap, 0, 0),
                true,
            ),
            (
                "shmat without remap",
                Sysno::shmat,
                args(3, 0x7100_0000, 0, 0, 0),
                false,
            ),
            (
                "shmat anywhere",
                Sysno::shmat,
                args(3, 0, remap, 0, 0),
                false,
            ),
            ("brk", Sysno::brk, args(0x7100_0000, 0, 0, 0, 0), false),
            // A scalar argument that equals an address in the range is not one.
            (
                "lseek to that offset",
                Sysno::lseek,
                args(3, 0x7100_0000, 0, 0, 0),
                false,
            ),
            (
                "an address near the end",
                Sysno::munmap,
                args(u64::MAX - 0xfff, 0x1000, 0, 0, 0),
                false,
            ),
        ];
        for (label, number, args, expected) in cases {
            assert_eq!(
                may_change_untraced_code(number, &args, PAGE, 2 << 20),
                expected,
                "{label}"
            );
        }
    }

    /// A write-mode open counts when its descriptor is a procfs file named
    /// `mem`, or cannot be read at all; a read-only or `O_PATH` open, an
    /// ordinary file named `mem`, a pipe, and another procfs file do not.
    #[test]
    fn an_open_counts_when_it_may_write_process_memory() {
        let proc_mem = Some((Path::new("/proc/12/mem"), true));
        let cases = [
            ("process mem read-write", OFlag::O_RDWR, proc_mem, true),
            (
                "task mem write-only",
                OFlag::O_WRONLY,
                Some((Path::new("/proc/12/task/13/mem"), true)),
                true,
            ),
            ("an unreadable descriptor", OFlag::O_WRONLY, None, true),
            ("process mem read-only", OFlag::O_RDONLY, proc_mem, false),
            (
                "process mem as a path",
                OFlag::O_PATH | OFlag::O_RDWR,
                proc_mem,
                false,
            ),
            (
                "an ordinary file named mem",
                OFlag::O_WRONLY | OFlag::O_CREAT,
                Some((Path::new("/tmp/mem"), false)),
                false,
            ),
            (
                "a name that leads to a pipe",
                OFlag::O_WRONLY,
                Some((Path::new("pipe:[4026]"), false)),
                false,
            ),
            (
                "another procfs file",
                OFlag::O_WRONLY,
                Some((Path::new("/proc/12/attr/current"), true)),
                false,
            ),
            (
                "an unreadable read-only descriptor",
                OFlag::O_RDONLY,
                None,
                false,
            ),
        ];
        for (label, flags, descriptor, expected) in cases {
            assert_eq!(
                may_write_process_memory(flags, descriptor),
                expected,
                "{label}"
            );
        }
    }
}
