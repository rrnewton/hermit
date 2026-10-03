/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Handles poll, ppoll, epoll, and select system calls.

use reverie::Errno;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::syscalls::Accept4;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::EpollWait;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Ppoll;
use reverie::syscalls::Pselect6;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmmsg;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Select;
use reverie::syscalls::Sendmmsg;
use reverie::syscalls::Socketpair;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::SockOptFamily;

use super::Recorder;
use crate::event::AcceptEvent;
use crate::event::EpollWaitEvent;
use crate::event::PollEvent;
use crate::event::PpollEvent;
use crate::event::RecvmmsgEvent;
use crate::event::RecvmsgEvent;
use crate::event::SelectEvent;
use crate::event::SockOptEvent;
use crate::event::SocketShape;
use crate::event::SyscallEvent;
use crate::event::fd_set_bytes;

fn read_bytes<M: MemoryAccess>(
    memory: &M,
    pointer: *mut libc::c_void,
    length: usize,
) -> Result<Vec<u8>, Errno> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let address = Addr::<u8>::from_raw(pointer as usize).ok_or(Errno::EFAULT)?;
    let mut bytes = vec![0; length];
    memory.read_exact(address.cast(), &mut bytes)?;
    Ok(bytes)
}

/// Capture what a successful receive of `result` bytes wrote through `output`,
/// a `msghdr` whose name and control buffers held `name_capacity` and
/// `control_capacity` bytes before the call. Shared by `recvmsg` and every
/// message of `recvmmsg`.
fn capture_recvmsg<M: MemoryAccess>(
    memory: &M,
    name_capacity: usize,
    control_capacity: usize,
    output: &libc::msghdr,
    result: i64,
) -> Result<RecvmsgEvent, Errno> {
    let iovecs = crate::read_iovecs(memory, output)?;
    let mut remaining = usize::try_from(result).map_err(|_| Errno::EINVAL)?;
    let mut buffers = Vec::with_capacity(iovecs.len());
    for iovec in iovecs {
        let length = remaining.min(iovec.iov_len);
        buffers.push(read_bytes(memory, iovec.iov_base, length)?);
        remaining -= length;
    }

    // Without a buffer Linux stores nothing, whatever length the header holds.
    let stored = |buffer: *mut libc::c_void, length: usize| {
        if buffer.is_null() { 0 } else { length }
    };
    let name_length = stored(
        output.msg_name,
        name_capacity.min(output.msg_namelen as usize),
    );
    let control_length = stored(
        output.msg_control,
        control_capacity.min(output.msg_controllen),
    );

    Ok(RecvmsgEvent {
        result,
        iovs: buffers,
        name: read_bytes(memory, output.msg_name, name_length)?,
        name_len: output.msg_namelen,
        control: read_bytes(memory, output.msg_control, control_length)?,
        control_len: output.msg_controllen,
        flags: output.msg_flags,
    })
}

/// The address of `mmsghdr` entry `index` of the array at `base`.
fn mmsghdr_entry(base: usize, index: usize) -> Result<usize, Errno> {
    index
        .checked_mul(std::mem::size_of::<libc::mmsghdr>())
        .and_then(|offset| base.checked_add(offset))
        .ok_or(Errno::EFAULT)
}

/// Read `count` consecutive `mmsghdr` entries starting at `address`.
fn read_mmsghdrs<M: MemoryAccess>(
    memory: &M,
    address: usize,
    count: usize,
) -> Result<Vec<libc::mmsghdr>, Errno> {
    // SAFETY: `mmsghdr` is a plain C record and an all-zero value is a valid
    // staging value that is immediately overwritten by `read_values`.
    let mut headers: Vec<libc::mmsghdr> =
        (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    if count != 0 {
        let address = Addr::<libc::mmsghdr>::from_raw(address).ok_or(Errno::EFAULT)?;
        memory.read_values(address, &mut headers)?;
    }
    Ok(headers)
}

/// Read the `mmsghdr` entries at `address` up to the first unreadable one, at
/// most `count`. Linux copies each header in just before receiving into it, so
/// it can return a positive count without touching a later, unmapped entry.
fn read_readable_mmsghdrs<M: MemoryAccess>(
    memory: &M,
    address: usize,
    count: usize,
) -> Vec<libc::mmsghdr> {
    let mut headers = Vec::new();
    for index in 0..count {
        let Ok(header) = mmsghdr_entry(address, index)
            .and_then(|entry| Addr::<libc::mmsghdr>::from_raw(entry).ok_or(Errno::EFAULT))
            .and_then(|entry| memory.read_value(entry))
        else {
            break;
        };
        headers.push(header);
    }
    headers
}

/// Copy `bytes` to guest `address` as Linux's copy-out would: with the
/// guest's page permissions, stopping at the first byte it cannot write. A
/// plain debugger write can store through a read-only page. Returns the
/// number of bytes copied.
fn copy_out_as_guest<M: MemoryAccess>(memory: &mut M, address: usize, bytes: &[u8]) -> usize {
    let Some(address) = AddrMut::<u8>::from_raw(address) else {
        return 0;
    };
    match memory.write_with_user_access(address, bytes) {
        Ok(copied) => copied,
        Err(Errno::EFAULT) => 0,
        // Recording always runs on ptrace, which supports this.
        Err(errno) => panic!("permission-checked guest write failed: {errno}"),
    }
}

/// How many `recvmmsg` headers the recorder examines before a call: 4 MiB
/// of headers, far more datagrams than a default socket receive queue holds.
const RECVMMSG_SCAN_LIMIT: usize = 65536;

/// The guest's address ranges, read from `/proc/<pid>/maps` without touching
/// guest memory.
struct GuestMappings(Vec<GuestMapping>);

struct GuestMapping {
    start: usize,
    end: usize,
    writable: bool,
    /// Whether the guest can read it. On x86_64 every mapping that is not
    /// `PROT_NONE` is user-readable, including a `PROT_WRITE`-only one that
    /// `process_vm_readv` refuses to read.
    accessible: bool,
}

impl GuestMappings {
    fn read(pid: reverie::Pid) -> Self {
        let maps = std::fs::read(format!("/proc/{}/maps", pid.as_raw()))
            .unwrap_or_else(|error| panic!("cannot read the memory map of {pid}: {error}"));
        // A mapped path need not be UTF-8; only the leading fields are parsed.
        let maps = String::from_utf8_lossy(&maps);
        let mappings = maps
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let (start, end) = fields.next()?.split_once('-')?;
                let permissions = fields.next()?.as_bytes();
                let granted = |index: usize, flag: u8| permissions.get(index) == Some(&flag);
                Some(GuestMapping {
                    start: usize::from_str_radix(start, 16).ok()?,
                    end: usize::from_str_radix(end, 16).ok()?,
                    writable: granted(1, b'w'),
                    accessible: granted(0, b'r') || granted(1, b'w') || granted(2, b'x'),
                })
            })
            .collect();
        Self(mappings)
    }

    fn covers_byte(&self, address: usize, permitted: impl Fn(&GuestMapping) -> bool) -> bool {
        self.0
            .iter()
            .any(|mapping| mapping.start <= address && address < mapping.end && permitted(mapping))
    }

    /// Whether every byte of `[address, address + length)` lies in a mapping
    /// for which `permitted` holds. A field no larger than a page spans at
    /// most two pages, so checking its first and last bytes suffices.
    fn covers(
        &self,
        address: usize,
        length: usize,
        permitted: impl Fn(&GuestMapping) -> bool + Copy,
    ) -> bool {
        address.checked_add(length - 1).is_some_and(|last| {
            self.covers_byte(address, permitted) && self.covers_byte(last, permitted)
        })
    }

    fn writable(&self, address: usize, length: usize) -> bool {
        self.covers(address, length, |mapping| mapping.writable)
    }

    fn accessible(&self, address: usize, length: usize) -> bool {
        self.covers(address, length, |mapping| mapping.accessible)
    }
}

/// Whether Linux could write its results back to `header`, entry `index` at
/// `base`: `msg_namelen` if it has a name buffer, and `msg_controllen`,
/// `msg_flags` and `msg_len`.
fn mmsghdr_is_writable(
    mappings: &GuestMappings,
    base: usize,
    index: usize,
    header: &libc::mmsghdr,
) -> bool {
    use std::mem::offset_of;
    use std::mem::size_of;
    let message = offset_of!(libc::mmsghdr, msg_hdr);
    let name_length = (
        message + offset_of!(libc::msghdr, msg_namelen),
        size_of::<libc::socklen_t>(),
    );
    let fields = [
        (
            message + offset_of!(libc::msghdr, msg_controllen),
            size_of::<usize>(),
        ),
        (
            message + offset_of!(libc::msghdr, msg_flags),
            size_of::<libc::c_int>(),
        ),
        (offset_of!(libc::mmsghdr, msg_len), size_of::<u32>()),
    ];
    let named = !header.msg_hdr.msg_name.is_null();
    mmsghdr_entry(base, index).is_ok_and(|entry| {
        named
            .then_some(name_length)
            .into_iter()
            .chain(fields)
            .all(|(offset, length)| {
                entry
                    .checked_add(offset)
                    .is_some_and(|field| mappings.writable(field, length))
            })
    })
}

/// Whether the guest could read the `mmsghdr` entry `index` at `base`.
fn mmsghdr_is_accessible(mappings: &GuestMappings, base: usize, index: usize) -> bool {
    mmsghdr_entry(base, index)
        .is_ok_and(|entry| mappings.accessible(entry, std::mem::size_of::<libc::mmsghdr>()))
}

/// Query the domain, type and protocol of the guest's socket `fd`.
fn guest_socket_shape(pid: reverie::Pid, fd: libc::c_int) -> Option<SocketShape> {
    use std::os::fd::AsRawFd;

    let socket = crate::fd::duplicate_guest_fd(pid, fd).ok()?;
    let option = |name: libc::c_int| {
        let mut value: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `value` and `length` are valid for writes of the sizes passed.
        let status = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                name,
                (&mut value as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        (status == 0).then_some(value)
    };
    Some(SocketShape {
        domain: option(libc::SO_DOMAIN)?,
        r#type: option(libc::SO_TYPE)?,
        protocol: option(libc::SO_PROTOCOL)?,
    })
}

fn pollfd_address<'a>(address: AddrMut<'a, PollFd>, index: usize) -> Option<AddrMut<'a, PollFd>> {
    let offset = index.checked_mul(std::mem::size_of::<PollFd>())?;
    AddrMut::from_raw(address.as_raw().checked_add(offset)?)
}

fn read_pollfds<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, PollFd>,
    nfds: usize,
) -> Result<Vec<PollFd>, Errno> {
    let mut fds = vec![PollFd::default(); nfds];
    memory.read_values(address.into(), &mut fds)?;
    Ok(fds)
}

/// Read as much of an EFAULT result as remains readable without allocating from
/// an untrusted `nfds` up front. Linux may have written earlier `revents`
/// entries before faulting on a later copy-out.
fn read_pollfd_prefix<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, PollFd>,
    nfds: usize,
) -> Vec<PollFd> {
    const ENTRIES_PER_READ: usize = 256;

    let mut fds = Vec::new();
    let mut index = 0;
    while index < nfds {
        let Some(chunk_address) = pollfd_address(address, index) else {
            break;
        };
        let chunk_len = (nfds - index).min(ENTRIES_PER_READ);
        let mut chunk = vec![PollFd::default(); chunk_len];
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                chunk.as_mut_ptr().cast::<u8>(),
                std::mem::size_of_val(chunk.as_slice()),
            )
        };
        let bytes_read = match memory.read(chunk_address.cast::<u8>(), bytes) {
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        let complete_entries = bytes_read / std::mem::size_of::<PollFd>();
        if complete_entries == 0 {
            break;
        }
        fds.extend(chunk.into_iter().take(complete_entries));
        index += complete_entries;
        if bytes_read % std::mem::size_of::<PollFd>() != 0 {
            break;
        }
    }
    fds
}

fn capture_pollfds<M: MemoryAccess>(
    memory: &M,
    address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<Option<Vec<PollFd>>, Errno> {
    match result {
        Ok(_) | Err(Errno::EINTR) => address
            .map(|address| read_pollfds(memory, address, nfds))
            .transpose(),
        // Best effort is load-bearing: a wholly bad pointer also returns
        // EFAULT. A failed diagnostic read must not replace the recorded errno.
        Err(Errno::EFAULT) => Ok(address.map(|address| read_pollfd_prefix(memory, address, nfds))),
        // In particular, EINVAL for nfds > RLIMIT_NOFILE must not trigger a
        // read or an allocation based on the invalid count.
        _ => Ok(None),
    }
}

fn capture_poll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PollEvent, Errno> {
    Ok(PollEvent {
        result,
        fds_pointer_present: fds_address.is_some(),
        fds: capture_pollfds(memory, fds_address, nfds, result)?,
    })
}

fn capture_ppoll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, libc::pollfd>>,
    timeout_address: Option<AddrMut<'_, Timespec>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PpollEvent, Errno> {
    let timeout = match result {
        // The timeout can also be updated when pollfd copy-out later faults.
        // If EFAULT instead came from an unreadable timeout pointer, retain the
        // syscall result and simply record no readable timeout value.
        Err(Errno::EFAULT) => timeout_address.and_then(|address| memory.read_value(address).ok()),
        // Raw ppoll can update its remaining-time argument on success and on
        // error returns such as EINTR, EBADF, or EINVAL. Preserve future errno
        // behavior too: if Linux accepted a readable timeout input before
        // producing the result, its post-call bytes are guest-visible.
        _ => timeout_address
            .map(|address| memory.read_value(address))
            .transpose()?,
    };
    let fds =
        if matches!(result, Err(Errno::EFAULT)) && timeout_address.is_some() && timeout.is_none() {
            // Linux validates the timeout input before polling. If that pointer is
            // unreadable, this EFAULT occurred before any pollfd output copy-out;
            // do not scan a potentially huge valid array looking for outputs that
            // the kernel never attempted to write.
            None
        } else {
            capture_pollfds(
                memory,
                fds_address.map(|address| address.cast::<PollFd>()),
                nfds,
                result,
            )?
        };

    Ok(PpollEvent {
        result,
        fds_pointer_present: fds_address.is_some(),
        fds,
        timeout_pointer_present: timeout_address.is_some(),
        timeout,
    })
}

/// Read up to `length` bytes, stopping at the first unreadable byte. Linux
/// clamps `nfds` to the descriptor table size, so a successful call writes a
/// prefix of the guest's range; any readable bytes past it are unchanged guest
/// memory, which replay restores to the same value.
fn read_byte_prefix<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, u8>,
    length: usize,
) -> Vec<u8> {
    let mut bytes = vec![0; length];
    let mut filled = 0;
    while filled < length {
        let Some(chunk) = address
            .as_raw()
            .checked_add(filled)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        match memory.read(chunk, &mut bytes[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => filled += count,
        }
    }
    bytes.truncate(filled);
    bytes
}

/// The kernel's descriptor-table size for `tid`, from the `FDSize:` line of
/// `/proc/<tid>/status`. `core_sys_select` clamps `nfds` to this before it
/// reads or writes a set, so no byte past it belongs to the call.
fn guest_max_fds(tid: Pid) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", tid.as_raw())).ok()?;
    parse_fd_size(&status)
}

/// A task's table holds at least `NR_OPEN_DEFAULT` (`BITS_PER_LONG`) entries.
/// A smaller value means a task without a table, or the wrong task, and would
/// silently record empty sets, so it is treated as unknown.
fn parse_fd_size(status: &str) -> Option<i32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("FDSize:"))
        .and_then(|value| value.trim().parse().ok())
        .filter(|&size: &i32| size >= 64)
}

/// The `nfds` the kernel acted on. Without a table size, fall back to the
/// default `fs.nr_open` limit of 1048576 descriptors (128 KiB per set), so a
/// guest passing `INT_MAX` cannot make the recorder allocate gigabytes.
fn select_capture_nfds(nfds: i32, max_fds: Option<i32>) -> i32 {
    nfds.min(max_fds.unwrap_or(1 << 20))
}

fn capture_select_event<M: MemoryAccess>(
    memory: &M,
    nfds: i32,
    fd_sets: [Option<AddrMut<'_, libc::fd_set>>; 3],
    timeout: Option<(AddrMut<'_, u8>, usize)>,
    result: Result<i64, Errno>,
) -> SelectEvent {
    // fs/select.c copies the sets out after a successful wait and can fault
    // partway through; every other result leaves them as the guest wrote them.
    // `nfds` must already be clamped to the descriptor table.
    let copied_out = matches!(result, Ok(_) | Err(Errno::EFAULT));
    let length = fd_set_bytes(nfds);
    let fd_sets = fd_sets.map(|address| {
        address
            .filter(|_| copied_out)
            .map(|address| read_byte_prefix(memory, address.cast(), length))
    });
    // Linux may write the remaining time back on any result that reached the
    // wait, and leaves it alone otherwise; capturing the post-call bytes is
    // right either way, because replaying unchanged bytes changes nothing. An
    // unreadable pointer could not have been written.
    let timeout = timeout
        .map(|(address, length)| read_byte_prefix(memory, address, length))
        .filter(|bytes| !bytes.is_empty());
    SelectEvent {
        result,
        fd_sets,
        timeout,
    }
}

impl Recorder {
    pub(super) async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: EpollWait,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        let event = result.and_then(|ret| {
            let updated = ret as usize;
            let mut events = vec![0; updated * std::mem::size_of::<libc::epoll_event>()];
            if !events.is_empty() {
                guest
                    .memory()
                    .read_exact(syscall.events().ok_or(Errno::EFAULT)?.cast(), &mut events)?;
            }
            Ok(SyscallEvent::EpollWait(EpollWaitEvent {
                events,
                updated,
                replay_kernel_side_effect: self
                    .epoll_requires_replay_kernel_side_effect(guest.pid(), syscall.epfd()),
            }))
        });

        self.record_event(guest, event);
        result
    }

    pub(super) async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Poll,
    ) -> Result<i64, Errno> {
        let len = syscall.nfds() as usize;
        let result = guest.inject(syscall).await;

        let event =
            capture_poll_event(&guest.memory(), syscall.fds(), len, result).map(SyscallEvent::Poll);

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ppoll,
    ) -> Result<i64, Errno> {
        let len = syscall.nfds() as usize;
        let timeout_is_zero = syscall
            .timeout()
            .and_then(|address| {
                let timeout: Timespec = guest.memory().read_value(address).ok()?;
                Some(timeout)
            })
            .is_some_and(|timeout| timeout.tv_sec == 0 && timeout.tv_nsec == 0);
        tracing::trace!(
            has_signal_mask = syscall.sigmask().is_some(),
            timeout_is_zero,
            "Recorder observed ppoll input"
        );
        let result = guest.inject(syscall).await;

        let event = capture_ppoll_event(
            &guest.memory(),
            syscall.fds(),
            syscall.timeout(),
            len,
            result,
        )
        .map(SyscallEvent::Ppoll);

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Select,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let event = capture_select_event(
            &guest.memory(),
            select_capture_nfds(syscall.nfds(), guest_max_fds(guest.tid())),
            [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
            syscall
                .timeout()
                .map(|address| (address.cast(), std::mem::size_of::<libc::timeval>())),
            result,
        );
        self.record_event(guest, Ok(SyscallEvent::Select(event)));
        result
    }

    pub(super) async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pselect6,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let event = capture_select_event(
            &guest.memory(),
            select_capture_nfds(syscall.nfds(), guest_max_fds(guest.tid())),
            [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
            syscall
                .timeout()
                .map(|address| (address.cast(), std::mem::size_of::<Timespec>())),
            result,
        );
        self.record_event(guest, Ok(SyscallEvent::Select(event)));
        result
    }

    pub(super) async fn handle_sockopt_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: SockOptFamily,
    ) -> Result<i64, Errno> {
        // The buffer length is both an input and output. If optlen is smaller
        // than the real value, then the value will be truncated.

        let buflen_addr = syscall.value_len().ok_or(Errno::EFAULT)?;

        // `optlen` will be updated after the syscall has been injected.
        let buflen: libc::socklen_t = guest.memory().read_value(buflen_addr)?;

        let result = guest.inject(Syscall::from(syscall)).await;

        let event = result.and_then(|ret| {
            debug_assert_eq!(ret, 0);

            // Linux permits a NULL value buffer when its input length is zero.
            let value = if let Some(address) = syscall.value() {
                let mut value = vec![0u8; buflen as usize];
                guest
                    .memory()
                    .read_exact(address.cast::<u8>(), &mut value)?;
                value
            } else {
                Vec::new()
            };

            // Need to read the (new) length. This might not have been updated,
            // but we don't know until we check it.
            let length: libc::socklen_t = guest.memory().read_value(buflen_addr)?;

            Ok(SyscallEvent::SockOpt(SockOptEvent { value, length }))
        });

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmsg,
    ) -> Result<i64, Errno> {
        let input = syscall
            .msg()
            .ok_or(Errno::EFAULT)
            .and_then(|address| guest.memory().read_value(address))
            .map(|message: libc::msghdr| (message.msg_namelen as usize, message.msg_controllen));
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|result| {
                let (name_capacity, control_capacity) = input?;
                let message_address = syscall.msg().ok_or(Errno::EFAULT)?;
                let output: libc::msghdr = guest.memory().read_value(message_address)?;
                capture_recvmsg(
                    &guest.memory(),
                    name_capacity,
                    control_capacity,
                    &output,
                    result,
                )
                .map(SyscallEvent::Recvmsg)
            }),
        );

        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Record every message a `recvmmsg` received through the same capture as
    /// `recvmsg`, plus the remaining timeout Linux writes back.
    pub(super) async fn handle_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmmsg,
    ) -> Result<i64, Errno> {
        // Unlike sendmmsg, Linux does not clamp recvmmsg's vlen. It reads each
        // header lazily, but the recorder must decide before the call how many
        // Linux may use, so it looks at no more than `RECVMMSG_SCAN_LIMIT`.
        let vlen = syscall.vlen() as usize;
        let scanned = vlen.min(RECVMMSG_SCAN_LIMIT);
        let base = syscall.mmsg().map(|address| address.as_raw());
        let timeout_address = syscall.timeout();
        let timeout = timeout_address.map(|address| guest.memory().read_value(address));
        // Reduced to plain capacities inside this block so no raw pointer is
        // held across an await, which would make this future non-`Send`.
        let (readable, writable, input) = {
            // Linux stops at the first header it cannot read, without side
            // effects.
            let headers = base
                .map(|base| read_readable_mmsghdrs(&guest.memory(), base, scanned))
                .unwrap_or_default();
            let mappings = (base.is_some() && scanned != 0 || matches!(timeout, Some(Err(_))))
                .then(|| GuestMappings::read(guest.pid()));
            // The recorder reads with process_vm_readv, which refuses a
            // PROT_WRITE-only page the guest itself can read. Linux would use
            // such a header or timeout where the recorder could not see it.
            if let (Some(base), Some(mappings)) = (base, &mappings) {
                assert!(
                    headers.len() == scanned
                        || !mmsghdr_is_accessible(mappings, base, headers.len()),
                    "recvmmsg entry {} is readable by the guest but not by the recorder \
                     (https://github.com/rrnewton/hermit/issues/3583)",
                    headers.len()
                );
            }
            if let (Some(Err(_)), Some(address), Some(mappings)) =
                (&timeout, timeout_address, &mappings)
            {
                assert!(
                    !mappings.accessible(address.as_raw(), std::mem::size_of::<Timespec>()),
                    "recvmmsg's timeout is readable by the guest but not by the recorder \
                     (https://github.com/rrnewton/hermit/issues/3583)"
                );
            }
            // Linux receives into a header before writing results back to it,
            // so a header it can read but not write loses a message. Offer
            // Linux only the writable prefix, and refuse below if that could
            // have mattered.
            let writable = match (base, &mappings) {
                (Some(base), Some(mappings)) => headers
                    .iter()
                    .enumerate()
                    .take_while(|(index, header)| {
                        mmsghdr_is_writable(mappings, base, *index, header)
                    })
                    .count(),
                _ => 0,
            };
            let input: Vec<(usize, usize)> = headers[..writable]
                .iter()
                .map(|header| {
                    (
                        header.msg_hdr.msg_namelen as usize,
                        header.msg_hdr.msg_controllen,
                    )
                })
                .collect();
            // Linux would have gone on past the scanned headers.
            let readable = if headers.len() == scanned {
                vlen
            } else {
                headers.len()
            };
            (readable, writable, input)
        };
        let clamped = writable < readable;
        let refusal = |index: usize| -> ! {
            panic!(
                "recvmmsg entry {index} can be read but not written, or lies beyond the \
                 {RECVMMSG_SCAN_LIMIT} entries the recorder checks: Linux would receive into \
                 it, which cannot be recorded (https://github.com/rrnewton/hermit/issues/3583)"
            )
        };
        if clamped && writable == 0 {
            refusal(0);
        }
        let call = if clamped {
            syscall.with_vlen(writable as u32)
        } else {
            syscall
        };

        // Linux writes the remaining timeout back only after receiving, and a
        // failed write turns the count into EFAULT. Give it a scratch copy and
        // write the remainder back here, so the received messages are known.
        let (mut result, remaining) = match timeout {
            Some(Ok(timeout)) => {
                let mut stack = guest.stack().await;
                let scratch = stack.push(timeout);
                // Nothing has been received yet, but the call cannot be
                // recorded without its scratch timeout.
                let _guard = stack
                    .commit()
                    .expect("cannot place recvmmsg's timeout on the guest stack");
                let result = guest.inject(call.with_timeout(Some(scratch))).await;
                let remaining: Timespec = guest
                    .memory()
                    .read_value(scratch)
                    .expect("cannot read back recvmmsg's scratch timeout");
                let mut bytes = [0u8; std::mem::size_of::<Timespec>()];
                guest
                    .memory()
                    .read_exact(scratch.cast::<u8>(), &mut bytes)
                    .expect("cannot read back recvmmsg's scratch timeout");
                (result, Some((remaining, bytes)))
            }
            // An unreadable timeout fails before any receive.
            Some(Err(_)) | None => (guest.inject(call).await, None),
        };
        if clamped && result == Ok(writable as i64) {
            refusal(writable);
        }
        // The writable prefix was decided from a snapshot of the memory map.
        // A blocking receive lets other guest threads run, and one that
        // changed the protection of a header Linux used meanwhile would leave
        // a result the recording does not describe; refuse rather than record
        // it. Rechecking afterwards catches every change that is still in
        // place when the call returns. The header after the last received one
        // is included: Linux may have received into it and then failed to
        // write back, which is exactly the lost message.
        if let (Ok(received), Some(base)) = (result, base) {
            let used = usize::try_from(received)
                .unwrap_or(0)
                .saturating_add(1)
                .min(writable);
            if used != 0 {
                let mappings = GuestMappings::read(guest.pid());
                let headers = read_readable_mmsghdrs(&guest.memory(), base, used);
                let unchanged = headers.len() == used
                    && headers
                        .iter()
                        .enumerate()
                        .all(|(index, header)| mmsghdr_is_writable(&mappings, base, index, header));
                assert!(
                    unchanged,
                    "a recvmmsg header changed protection while the call ran \
                     (https://github.com/rrnewton/hermit/issues/3583)"
                );
            }
        }
        // Capture the messages before writing the timeout back: Linux writes it
        // last, so a timeout aliasing a header overwrites that header's result.
        let messages = result.and_then(|received| {
            let received = usize::try_from(received).map_err(|_| Errno::EINVAL)?;
            assert!(received <= input.len());
            let address = base.ok_or(Errno::EFAULT)?;
            let output = read_mmsghdrs(&guest.memory(), address, received)?;
            input
                .iter()
                .zip(&output)
                .map(|((name_capacity, control_capacity), output)| {
                    capture_recvmsg(
                        &guest.memory(),
                        *name_capacity,
                        *control_capacity,
                        &output.msg_hdr,
                        i64::from(output.msg_len),
                    )
                })
                .collect::<Result<Vec<_>, _>>()
        });
        // Linux writes the remaining time back only after receiving something.
        let remaining = remaining.filter(|_| matches!(result, Ok(received) if received > 0));
        // Linux's copy-out can store a prefix of the timeout before faulting.
        let mut timeout_fault = None;
        if let Some((_, bytes)) = &remaining {
            let address = timeout_address.expect("a remaining timeout needs a timeout");
            let copied = copy_out_as_guest(&mut guest.memory(), address.as_raw(), bytes);
            if copied < bytes.len() {
                timeout_fault = Some(bytes[..copied].to_vec());
            }
        }

        self.record_event(
            guest,
            messages.map(|messages| {
                SyscallEvent::Recvmmsg(RecvmmsgEvent {
                    messages,
                    timeout: remaining
                        .map(|(remaining, _)| remaining)
                        .filter(|_| timeout_fault.is_none()),
                    timeout_fault: timeout_fault.clone(),
                })
            }),
        );

        if timeout_fault.is_some() {
            result = Err(Errno::EFAULT);
        }
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Record the `msg_len` of every message a `sendmmsg` sent.
    pub(super) async fn handle_sendmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Sendmmsg,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|sent| {
                let sent = usize::try_from(sent).map_err(|_| Errno::EINVAL)?;
                let address = syscall.msgvec().ok_or(Errno::EFAULT)?.as_raw();
                let headers = read_mmsghdrs(&guest.memory(), address, sent)?;
                Ok(SyscallEvent::Sendmmsg(
                    headers.iter().map(|header| header.msg_len).collect(),
                ))
            }),
        );

        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Record the fd, peer address and socket shape of an `accept`/`accept4`,
    /// so replay can reproduce the connection without a live peer.
    pub(super) async fn handle_accept<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
        call: Accept4,
    ) -> Result<i64, Errno> {
        // Reverie types addrlen as a usize; Linux reads and writes a socklen_t.
        let addr_len_address = call
            .addrlen()
            .map(|address| address.cast::<libc::socklen_t>());
        // Linux reads the address capacity only when an address buffer is given.
        let capacity = match (call.sockaddr(), addr_len_address) {
            (Some(_), Some(address)) => Some(guest.memory().read_value(address)),
            _ => None,
        };
        let result = guest.inject(syscall).await;

        let event = result.and_then(|fd| {
            let fd = i32::try_from(fd).map_err(|_| Errno::EINVAL)?;
            let (addr, addr_len) = match (call.sockaddr(), addr_len_address, capacity) {
                (Some(addr), Some(addr_len_address), Some(capacity)) => {
                    let capacity: libc::socklen_t = capacity?;
                    let addr_len: libc::socklen_t = guest.memory().read_value(addr_len_address)?;
                    let bytes = read_bytes(
                        &guest.memory(),
                        addr.as_raw() as *mut libc::c_void,
                        capacity.min(addr_len) as usize,
                    )?;
                    (bytes, Some(addr_len))
                }
                _ => (Vec::new(), None),
            };
            Ok(SyscallEvent::Accept(AcceptEvent {
                fd,
                addr,
                addr_len,
                shape: guest_socket_shape(guest.pid(), fd),
            }))
        });

        self.record_event(guest, event);
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Record the two fds a `socketpair` wrote to the guest.
    pub(super) async fn handle_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Socketpair,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        let event = result.and_then(|_| {
            let fds = guest
                .memory()
                .read_value(syscall.usockvec().ok_or(Errno::EFAULT)?)?;
            Ok(SyscallEvent::Socketpair(fds))
        });

        self.record_event(guest, event);
        result
    }

    pub(super) async fn handle_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvfrom,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        // TODO: Handle `addr` and `addr_len` parameters. These are NULL most of
        // the time. Maybe these can be recorded as a separate event SockOpt
        // event if non-NULL.

        // Treat this exactly the same way as a `read` syscall.
        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                let addr = syscall.buf().ok_or(Errno::EFAULT)?;
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }

    // TODO: Add support for select here.
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::PollFlags;

    use super::*;

    /// recvmsg and every recvmmsg message share this capture: it must keep only
    /// the received prefix of each iovec and only the part of the name that fit
    /// the guest's buffer, while preserving the full kernel-written lengths.
    #[test]
    fn capture_recvmsg_keeps_received_prefix_and_truncated_name() {
        let mut first = *b"abcd";
        let mut second = *b"efgh";
        let mut iovecs = [
            libc::iovec {
                iov_base: first.as_mut_ptr().cast(),
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_mut_ptr().cast(),
                iov_len: second.len(),
            },
        ];
        let mut name = *b"NAME";
        // SAFETY: an all-zero msghdr is valid; the fields used are set below.
        let mut output: libc::msghdr = unsafe { std::mem::zeroed() };
        output.msg_iov = iovecs.as_mut_ptr();
        output.msg_iovlen = iovecs.len();
        output.msg_name = name.as_mut_ptr().cast();
        // The kernel reports the full 16-byte address it could not fit.
        output.msg_namelen = 16;
        output.msg_flags = libc::MSG_TRUNC;

        let event = capture_recvmsg(&LocalMemory::new(), 2, 0, &output, 6).unwrap();

        assert_eq!(event.result, 6);
        assert_eq!(event.iovs, vec![b"abcd".to_vec(), b"ef".to_vec()]);
        assert_eq!(event.name, b"NA".to_vec());
        assert_eq!(event.name_len, 16);
        assert!(event.control.is_empty());
        assert_eq!(event.flags, libc::MSG_TRUNC);
    }

    #[test]
    fn capture_poll_keeps_outputs_on_efault() {
        let fds = [
            PollFd {
                fd: 3,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            },
            PollFd {
                fd: 5,
                events: PollFlags::POLLIN,
                revents: PollFlags::empty(),
            },
        ];
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            fds.len(),
            Err(Errno::EFAULT),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert!(event.fds_pointer_present);
        assert_eq!(event.fds.as_deref(), Some(&fds[..]));
    }

    #[test]
    fn capture_ppoll_keeps_timeout_and_outputs_on_efault() {
        let fds = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::POLLIN,
        }];
        let timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 345_678_901,
        };
        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::<PollFd>::from_raw(fds.as_ptr() as usize)
                .map(|address| address.cast::<libc::pollfd>()),
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            fds.len(),
            Err(Errno::EFAULT),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert_eq!(event.fds.as_deref(), Some(&fds[..]));
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_poll_keeps_outputs_on_eintr() {
        let fds = [PollFd {
            fd: 9,
            events: PollFlags::POLLIN,
            revents: PollFlags::empty(),
        }];
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            fds.len(),
            Err(Errno::EINTR),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINTR));
        assert_eq!(event.fds.as_deref(), Some(&fds[..]));
    }

    #[test]
    fn early_einval_does_not_read_or_allocate_pollfds() {
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds_pointer_present);
        assert!(event.fds.is_none());
    }

    #[test]
    fn ppoll_einval_preserves_timeout_without_reading_pollfds() {
        let timeout = Timespec {
            tv_sec: 1,
            tv_nsec: 234_567_890,
        };
        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds.is_none());
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_ppoll_preserves_exact_timeout_on_eintr() {
        let timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 345_678_901,
        };

        let event = capture_ppoll_event(
            &LocalMemory::new(),
            None,
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            0,
            Err(Errno::EINTR),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINTR));
        assert!(!event.fds_pointer_present);
        assert!(event.fds.is_none());
        assert_eq!(event.timeout, Some(timeout));
    }

    /// Linux can fault partway through the `revents` copy-out, leaving earlier
    /// entries written and still returning EFAULT. Those writes are
    /// guest-visible, so they must be captured or replay restores nothing and
    /// the guest keeps its pre-syscall sentinels -- measured as a real
    /// divergence: record `revents0=1` against replay `revents0=4660`, where
    /// 4660 is 0x1234, the value the guest itself wrote before the call.
    #[test]
    fn capture_ppoll_keeps_the_partial_copyout_on_efault() {
        let fds = [
            PollFd {
                fd: 3,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            },
            PollFd {
                fd: 5,
                events: PollFlags::POLLIN,
                revents: PollFlags::empty(),
            },
        ];

        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            None,
            fds.len(),
            Err(Errno::EFAULT),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert!(event.fds_pointer_present);
        assert_eq!(
            event.fds.as_deref(),
            Some(&fds[..]),
            "a partially completed copy-out must be captured, not discarded"
        );
    }

    #[test]
    fn capture_poll_preserves_ready_fds() {
        let fds = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::POLLIN,
        }];

        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            fds.len(),
            Ok(1),
        )
        .unwrap();

        assert_eq!(event.result, Ok(1));
        assert!(event.fds_pointer_present);
        assert_eq!(event.fds.unwrap()[0].revents, PollFlags::POLLIN);
    }

    #[test]
    fn capture_poll_keeps_the_result_on_an_error_return() {
        // The defect this fixes: the old `result.and_then(..)` recorded NOTHING
        // on an error, so the event -- and with it every already-written
        // `revents` entry -- was discarded. The result must now live inside the
        // event so replay has something to restore before returning it.
        let event = capture_poll_event(&LocalMemory::new(), None, 0, Err(Errno::EFAULT)).unwrap();

        assert_eq!(event.result, Err(Errno::EFAULT));
        assert!(!event.fds_pointer_present);
        assert!(event.fds.is_none());
    }

    #[test]
    fn capture_poll_early_error_does_not_read_or_allocate_pollfds() {
        // EINVAL must not trigger the best-effort read. An earlier attempt on
        // the ppoll side read on EVERY error and the read's own EFAULT replaced
        // the kernel's EINVAL, flipping `review-cases invalid-nfds` red. `nfds`
        // is deliberately absurd: a read would try to allocate it and abort.
        let event = capture_poll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds_pointer_present);
        assert!(event.fds.is_none());
    }

    #[test]
    fn capture_ppoll_preserves_ready_fds_and_unchanged_timeout() {
        let fds = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::POLLIN,
        }];
        let timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_789_123,
        };

        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(fds.as_ptr() as usize),
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            fds.len(),
            Ok(1),
        )
        .unwrap();

        assert_eq!(event.result, Ok(1));
        assert!(event.fds_pointer_present);
        assert_eq!(event.fds.unwrap()[0].revents, PollFlags::POLLIN);
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_ppoll_early_error_does_not_read_or_allocate_pollfds() {
        let event = capture_ppoll_event(
            &LocalMemory::new(),
            AddrMut::from_raw(1),
            None,
            usize::MAX,
            Err(Errno::EINVAL),
        )
        .unwrap();

        assert_eq!(event.result, Err(Errno::EINVAL));
        assert!(event.fds_pointer_present);
        assert!(event.fds.is_none());
        assert!(event.timeout.is_none());
    }

    fn fd_set_with(fds: &[i32]) -> libc::fd_set {
        let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
        for &fd in fds {
            unsafe { libc::FD_SET(fd, &mut set) };
        }
        set
    }

    #[test]
    fn fd_set_bytes_rounds_up_to_whole_longs() {
        assert_eq!(fd_set_bytes(-1), 0);
        assert_eq!(fd_set_bytes(0), 0);
        assert_eq!(fd_set_bytes(1), 8);
        assert_eq!(fd_set_bytes(64), 8);
        assert_eq!(fd_set_bytes(65), 16);
    }

    #[test]
    fn capture_select_keeps_sets_and_timeout_on_success() {
        let mut readfds = fd_set_with(&[3]);
        let mut timeout = libc::timeval {
            tv_sec: 1,
            tv_usec: 234_567,
        };
        let event = capture_select_event(
            &LocalMemory::new(),
            4,
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            AddrMut::<u8>::from_raw((&mut timeout as *mut libc::timeval) as usize)
                .map(|address| (address, std::mem::size_of::<libc::timeval>())),
            Ok(1),
        );

        assert_eq!(event.result, Ok(1));
        let [read, write, except] = event.fd_sets;
        assert_eq!(read, Some(vec![0b1000, 0, 0, 0, 0, 0, 0, 0]));
        assert!(write.is_none());
        assert!(except.is_none());
        let mut expected = vec![0u8; std::mem::size_of::<libc::timeval>()];
        expected[..8].copy_from_slice(&1i64.to_ne_bytes());
        expected[8..].copy_from_slice(&234_567i64.to_ne_bytes());
        assert_eq!(event.timeout, Some(expected));
    }

    #[test]
    fn select_capture_is_clamped_to_the_descriptor_table() {
        let status = "Name:\tguest\nFDSize:\t64\nGroups:\t\n";
        assert_eq!(parse_fd_size(status), Some(64));
        assert_eq!(parse_fd_size("Name:\tguest\n"), None);
        assert_eq!(parse_fd_size("FDSize:\t0\n"), None);
        let own = Pid::from_raw(unsafe { libc::gettid() });
        assert!(guest_max_fds(own).is_some_and(|size| size >= 64));

        assert_eq!(select_capture_nfds(4, Some(64)), 4);
        assert_eq!(select_capture_nfds(i32::MAX, Some(64)), 64);
        assert_eq!(fd_set_bytes(select_capture_nfds(i32::MAX, Some(64))), 8);
        assert_eq!(select_capture_nfds(i32::MAX, None), 1 << 20);
        assert_eq!(select_capture_nfds(-1, Some(64)), -1);

        // A guest-sized nfds far past its table reads only the table's bytes.
        let mut readfds = fd_set_with(&[3]);
        let event = capture_select_event(
            &LocalMemory::new(),
            select_capture_nfds(i32::MAX, Some(64)),
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            None,
            Ok(1),
        );
        assert_eq!(event.fd_sets[0], Some(vec![0b1000, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn capture_select_keeps_the_readable_prefix_and_drops_an_unreadable_timeout() {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
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
        assert_eq!(
            unsafe { libc::mprotect(base.cast::<u8>().add(page).cast(), page, libc::PROT_NONE) },
            0
        );
        // The set starts 16 bytes before the inaccessible page, as a partial
        // copy-out before EFAULT would leave it.
        let set = base as usize + page - 16;
        unsafe { std::ptr::write_bytes(set as *mut u8, 0xa5, 16) };
        let event = capture_select_event(
            &LocalMemory::new(),
            1024,
            [AddrMut::from_raw(set), None, None],
            AddrMut::<u8>::from_raw(base as usize + page)
                .map(|address| (address, std::mem::size_of::<Timespec>())),
            Err(Errno::EFAULT),
        );
        unsafe { libc::munmap(base, 2 * page) };

        assert_eq!(event.fd_sets[0], Some(vec![0xa5; 16]));
        assert!(event.timeout.is_none());
    }

    #[test]
    fn capture_select_omits_sets_the_kernel_did_not_copy_out() {
        let mut readfds = fd_set_with(&[3]);
        let event = capture_select_event(
            &LocalMemory::new(),
            4,
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            None,
            Err(Errno::EBADF),
        );

        assert_eq!(event.result, Err(Errno::EBADF));
        assert!(event.fd_sets.iter().all(Option::is_none));
        assert!(event.timeout.is_none());
    }
}
