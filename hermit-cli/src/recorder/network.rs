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
use reverie::Stack;
use reverie::syscalls::Accept4;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::EpollWait;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Ppoll;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmmsg;
use reverie::syscalls::Recvmsg;
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
use crate::event::SockOptEvent;
use crate::event::SocketShape;
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

    let name_length = name_capacity.min(output.msg_namelen as usize);
    let control_length = control_capacity.min(output.msg_controllen);

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

/// Write `bytes` to guest `address` with the page permissions the guest's own
/// stores, and so Linux's copy-out, would have. A plain debugger write can
/// store through a read-only page.
fn write_as_guest<M: MemoryAccess>(
    memory: &mut M,
    address: usize,
    bytes: &[u8],
) -> Result<(), Errno> {
    let address = AddrMut::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
    match memory.write_with_user_access(address, bytes) {
        Ok(written) if written == bytes.len() => Ok(()),
        Ok(_) => Err(Errno::EFAULT),
        // Recording always runs on ptrace, which supports this.
        Err(Errno::ENOSYS) => panic!("recording needs permission-checked guest writes"),
        Err(errno) => Err(errno),
    }
}

/// Whether Linux could write back to `mmsghdr` entry `index` at `base`.
/// Writing back the bytes just read leaves guest memory as it was.
fn mmsghdr_is_writable<M: MemoryAccess>(memory: &mut M, base: usize, index: usize) -> bool {
    let mut bytes = [0u8; std::mem::size_of::<libc::mmsghdr>()];
    mmsghdr_entry(base, index)
        .and_then(|entry| {
            let entry = Addr::<u8>::from_raw(entry).ok_or(Errno::EFAULT)?;
            memory.read_exact(entry, &mut bytes)?;
            write_as_guest(memory, entry.as_raw(), &bytes)
        })
        .is_ok()
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
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3550)
    /// Record every message a `recvmmsg` received through the same capture as
    /// `recvmsg`, plus the remaining timeout Linux writes back.
    pub(super) async fn handle_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmmsg,
    ) -> Result<i64, Errno> {
        // Linux silently clamps vlen to UIO_MAXIOV.
        let vlen = (syscall.vlen() as usize).min(libc::UIO_MAXIOV as usize);
        let base = syscall.mmsg().map(|address| address.as_raw());
        // Reduced to plain capacities inside this block so no raw pointer is
        // held across an await, which would make this future non-`Send`.
        let (readable, writable, input) = {
            // Linux stops at the first header it cannot read, without side
            // effects.
            let headers = base
                .map(|base| read_readable_mmsghdrs(&guest.memory(), base, vlen))
                .unwrap_or_default();
            // But it receives into a header before writing results back to it,
            // so a header it can read but not write loses a message. Offer
            // Linux only the writable prefix, and refuse below if that could
            // have mattered.
            let writable = match base {
                Some(base) => headers
                    .iter()
                    .enumerate()
                    .take_while(|(index, _)| mmsghdr_is_writable(&mut guest.memory(), base, *index))
                    .count(),
                None => 0,
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
            (headers.len(), writable, input)
        };
        let clamped = writable < readable;
        let refusal = |index: usize| -> ! {
            panic!(
                "recvmmsg entry {index} can be read but not written: Linux would receive \
                 into it and then fail, which cannot be recorded \
                 (https://github.com/rrnewton/hermit/issues/3583)"
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
        let timeout = syscall
            .timeout()
            .map(|address| guest.memory().read_value(address));
        let (mut result, remaining) = match timeout {
            Some(Ok(timeout)) => {
                let mut stack = guest.stack().await;
                let scratch = stack.push(timeout);
                let _guard = stack.commit()?;
                let result = guest.inject(call.with_timeout(Some(scratch))).await;
                let remaining: Timespec = guest.memory().read_value(scratch)?;
                let mut bytes = [0u8; std::mem::size_of::<Timespec>()];
                guest
                    .memory()
                    .read_exact(scratch.cast::<u8>(), &mut bytes)?;
                (result, Some((remaining, bytes)))
            }
            // An unreadable timeout fails before any receive.
            Some(Err(_)) | None => (guest.inject(call).await, None),
        };
        if clamped && result == Ok(writable as i64) {
            refusal(writable);
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
        let mut timeout_fault = false;
        if let Some((_, bytes)) = &remaining {
            let address = syscall
                .timeout()
                .expect("a remaining timeout needs a timeout");
            timeout_fault = write_as_guest(&mut guest.memory(), address.as_raw(), bytes).is_err();
        }

        self.record_event(
            guest,
            messages.map(|messages| {
                SyscallEvent::Recvmmsg(RecvmmsgEvent {
                    messages,
                    timeout: remaining
                        .map(|(remaining, _)| remaining)
                        .filter(|_| !timeout_fault),
                    timeout_fault,
                })
            }),
        );

        if timeout_fault {
            result = Err(Errno::EFAULT);
        }
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3550)
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
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3550)
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
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3550)
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
}
