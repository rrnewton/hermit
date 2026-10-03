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
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::EpollWait;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Ppoll;
use reverie::syscalls::Pselect6;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Select;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::family::SockOptFamily;

use super::Recorder;
use crate::event::EpollWaitEvent;
use crate::event::PollEvent;
use crate::event::PpollEvent;
use crate::event::RecvmsgEvent;
use crate::event::SelectEvent;
use crate::event::SockOptEvent;
use crate::event::SyscallEvent;

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

/// The smallest fd table Linux gives a process (`NR_OPEN_DEFAULT`, one word of
/// descriptors). An `nfds` at or below this needs no fd-table lookup.
const MIN_FD_TABLE_SIZE: usize = libc::c_ulong::BITS as usize;

/// Linux `FDS_BYTES`: the whole `unsigned long` words covering `nfds` bits.
pub(crate) fn select_fd_set_bytes(nfds: usize) -> usize {
    nfds.div_ceil(libc::c_ulong::BITS as usize) * std::mem::size_of::<libc::c_ulong>()
}

/// The `FDSize` of `tid`'s fd table, which Linux uses to clamp select's `nfds`.
fn fd_table_size(tid: reverie::Pid) -> Result<usize, Errno> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", tid.as_raw()))
        .map_err(|error| Errno::new(error.raw_os_error().unwrap_or(libc::EIO)))?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("FDSize:"))
        .and_then(|size| size.trim().parse().ok())
        .ok_or(Errno::EIO)
}

/// The number of bytes Linux copies out to each select fd set for `nfds`.
///
/// Linux clamps `nfds` to the caller's fd-table size, which can be smaller than
/// a userspace `nfds` such as `FD_SETSIZE`. Reading the table size after the
/// call gives an upper bound when another thread grew the table concurrently;
/// any extra bytes are then the unchanged guest bytes, so replay restoring them
/// is a no-op. A negative `nfds` fails with EINVAL before any copy-out.
fn select_copyout_len(
    nfds: i32,
    fd_table_size: impl FnOnce() -> Result<usize, Errno>,
) -> Result<usize, Errno> {
    let Ok(nfds) = usize::try_from(nfds) else {
        return Ok(0);
    };
    let nfds = if nfds <= MIN_FD_TABLE_SIZE {
        nfds
    } else {
        nfds.min(fd_table_size()?)
    };
    Ok(select_fd_set_bytes(nfds))
}

/// Read the readable prefix of `length` bytes in bounded chunks, so an
/// unmapped tail neither fails the capture nor forces a large allocation.
fn read_byte_prefix<M: MemoryAccess>(
    memory: &M,
    address: AddrMut<'_, u8>,
    length: usize,
) -> Vec<u8> {
    const BYTES_PER_READ: usize = 4096;

    let mut bytes = Vec::new();
    while bytes.len() < length {
        let Some(chunk_address) = address
            .as_raw()
            .checked_add(bytes.len())
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        let mut chunk = vec![0; (length - bytes.len()).min(BYTES_PER_READ)];
        let bytes_read = match memory.read(chunk_address, &mut chunk) {
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        bytes.extend_from_slice(&chunk[..bytes_read]);
        if bytes_read < chunk.len() {
            break;
        }
    }
    bytes
}

/// Capture the post-kernel outputs of `select` or `pselect6`.
///
/// `fd_set_len` is the per-set copy-out length from [`select_copyout_len`]; it
/// is consulted only for results after which Linux may have written the sets.
fn capture_select_event<M: MemoryAccess, T>(
    memory: &M,
    fd_sets: [Option<AddrMut<'_, libc::fd_set>>; 3],
    timeout_address: Option<AddrMut<'_, T>>,
    fd_set_len: usize,
    result: Result<i64, Errno>,
) -> SelectEvent<T> {
    // Linux writes the sets on success, including a zero count, and may have
    // written a prefix before a copy-out fault turned the result into EFAULT.
    // An EFAULT from unreadable input leaves the guest bytes unchanged, which
    // replay then restores as a no-op. Every other error, notably EINVAL for an
    // invalid nfds or timeout, precedes the copy-out, so nothing is read.
    let fd_sets_written = matches!(result, Ok(_) | Err(Errno::EFAULT));
    SelectEvent {
        result,
        fd_set_pointers_present: fd_sets.map(|address| address.is_some()),
        fd_sets: fd_sets.map(|address| {
            address
                .filter(|_| fd_sets_written)
                .map(|address| read_byte_prefix(memory, address.cast(), fd_set_len))
        }),
        timeout_pointer_present: timeout_address.is_some(),
        // Linux writes the remaining time back after any result once it has
        // accepted the timeout input, and ignores a fault doing so. Read it
        // best effort for every result: where Linux did not write it, the value
        // is the unchanged input and restoring it on replay is a no-op.
        timeout: timeout_address.and_then(|address| memory.read_value(address).ok()),
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
                let iovecs = crate::read_iovecs(&guest.memory(), &output)?;
                let mut remaining = usize::try_from(result).map_err(|_| Errno::EINVAL)?;
                let mut buffers = Vec::with_capacity(iovecs.len());
                for iovec in iovecs {
                    let length = remaining.min(iovec.iov_len);
                    buffers.push(read_bytes(&guest.memory(), iovec.iov_base, length)?);
                    remaining -= length;
                }

                let name_length = name_capacity.min(output.msg_namelen as usize);
                let control_length = control_capacity.min(output.msg_controllen);

                Ok(SyscallEvent::Recvmsg(RecvmsgEvent {
                    result,
                    iovs: buffers,
                    name: read_bytes(&guest.memory(), output.msg_name, name_length)?,
                    name_len: output.msg_namelen,
                    control: read_bytes(&guest.memory(), output.msg_control, control_length)?,
                    control_len: output.msg_controllen,
                    flags: output.msg_flags,
                }))
            }),
        );

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

    pub(super) async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Select,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let event = select_copyout_len(syscall.nfds(), || fd_table_size(guest.tid())).map(|len| {
            SyscallEvent::Select(capture_select_event(
                &guest.memory(),
                [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
                syscall.timeout().map(|address| address.cast::<Timeval>()),
                len,
                result,
            ))
        });
        self.record_event(guest, event);
        result
    }

    pub(super) async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pselect6,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let event = select_copyout_len(syscall.nfds(), || fd_table_size(guest.tid())).map(|len| {
            SyscallEvent::Pselect6(capture_select_event(
                &guest.memory(),
                [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
                syscall.timeout(),
                len,
                result,
            ))
        });
        self.record_event(guest, event);
        result
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::PollFlags;

    use super::*;

    fn fd_set_with(fds: &[i32]) -> libc::fd_set {
        // SAFETY: fd_set is plain old data and FD_ZERO/FD_SET only touch it.
        unsafe {
            let mut set = std::mem::zeroed::<libc::fd_set>();
            libc::FD_ZERO(&mut set);
            for &fd in fds {
                libc::FD_SET(fd, &mut set);
            }
            set
        }
    }

    fn fd_set_address(set: &libc::fd_set) -> Option<AddrMut<'_, libc::fd_set>> {
        AddrMut::from_raw((set as *const libc::fd_set) as usize)
    }

    #[test]
    fn select_copyout_len_clamps_to_the_fd_table() {
        assert_eq!(select_copyout_len(-1, || panic!("no copy-out")), Ok(0));
        assert_eq!(select_copyout_len(0, || panic!("no table read")), Ok(0));
        assert_eq!(select_copyout_len(3, || panic!("no table read")), Ok(8));
        assert_eq!(select_copyout_len(64, || panic!("no table read")), Ok(8));
        assert_eq!(select_copyout_len(1024, || Ok(64)), Ok(8));
        assert_eq!(select_copyout_len(1024, || Ok(256)), Ok(32));
        assert_eq!(select_copyout_len(100, || Ok(4096)), Ok(16));
        assert_eq!(
            select_copyout_len(1024, || Err(Errno::ENOENT)),
            Err(Errno::ENOENT)
        );
    }

    #[test]
    fn fd_table_size_reads_this_process() {
        let size = fd_table_size(reverie::Pid::this()).unwrap();
        assert!(size >= MIN_FD_TABLE_SIZE, "FDSize {size}");
    }

    #[test]
    fn capture_select_keeps_sets_and_timeout_on_success() {
        let readfds = fd_set_with(&[3]);
        let exceptfds = fd_set_with(&[]);
        let timeout = Timespec {
            tv_sec: 4,
            tv_nsec: 999_000_123,
        };
        let event = capture_select_event(
            &LocalMemory::new(),
            [fd_set_address(&readfds), None, fd_set_address(&exceptfds)],
            AddrMut::from_raw((&timeout as *const Timespec) as usize),
            8,
            Ok(1),
        );

        assert_eq!(event.result, Ok(1));
        assert_eq!(event.fd_set_pointers_present, [true, false, true]);
        assert_eq!(
            event.fd_sets[0].as_deref(),
            Some(&[0b1000, 0, 0, 0, 0, 0, 0, 0][..])
        );
        assert!(event.fd_sets[1].is_none());
        assert_eq!(event.fd_sets[2].as_deref(), Some(&[0; 8][..]));
        assert!(event.timeout_pointer_present);
        assert_eq!(event.timeout, Some(timeout));
    }

    #[test]
    fn capture_select_keeps_sets_on_zero_and_efault() {
        let readfds = fd_set_with(&[9]);
        for result in [Ok(0), Err(Errno::EFAULT)] {
            let event = capture_select_event::<_, Timespec>(
                &LocalMemory::new(),
                [fd_set_address(&readfds), None, None],
                None,
                16,
                result,
            );
            assert_eq!(
                event.fd_sets[0].as_ref().map(Vec::len),
                Some(16),
                "{result:?}"
            );
            assert!(!event.timeout_pointer_present);
            assert!(event.timeout.is_none());
        }
    }

    #[test]
    fn capture_select_reads_no_sets_on_einval_or_eintr_but_keeps_timeout() {
        let timeout = Timespec {
            tv_sec: 1,
            tv_nsec: 2,
        };
        for result in [Err(Errno::EINVAL), Err(Errno::EINTR), Err(Errno::ENOMEM)] {
            // An unreadable set address proves nothing is read.
            let event = capture_select_event(
                &LocalMemory::new(),
                [AddrMut::from_raw(1), AddrMut::from_raw(1), None],
                AddrMut::from_raw((&timeout as *const Timespec) as usize),
                usize::MAX,
                result,
            );
            assert_eq!(event.fd_set_pointers_present, [true, true, false]);
            assert!(event.fd_sets.iter().all(Option::is_none), "{result:?}");
            assert_eq!(event.timeout, Some(timeout));
        }
    }

    #[test]
    fn capture_select_stops_at_an_unreadable_prefix() {
        let event = capture_select_event::<_, Timespec>(
            &LocalMemory::new(),
            [AddrMut::from_raw(1), None, None],
            AddrMut::from_raw(1),
            8,
            Err(Errno::EFAULT),
        );
        assert_eq!(event.fd_sets[0].as_deref(), Some(&[][..]));
        assert!(event.timeout_pointer_present);
        assert!(event.timeout.is_none());
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
}
