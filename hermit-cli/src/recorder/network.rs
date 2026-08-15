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
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::SockOptFamily;

use super::Recorder;
use crate::event::EpollWaitEvent;
use crate::event::PollEvent;
use crate::event::PpollEvent;
use crate::event::RecvmsgEvent;
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

fn read_iovecs<M: MemoryAccess>(
    memory: &M,
    message: &libc::msghdr,
) -> Result<Vec<libc::iovec>, Errno> {
    if message.msg_iovlen == 0 {
        return Ok(Vec::new());
    }
    let address = Addr::from_raw(message.msg_iov as usize).ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        message.msg_iovlen
    ];
    memory.read_values(address, &mut iovecs)?;
    Ok(iovecs)
}

fn capture_ppoll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, libc::pollfd>>,
    timeout_address: Option<AddrMut<'_, Timespec>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PpollEvent, Errno> {
    let fds_pointer_present = fds_address.is_some();
    let fds = if matches!(result, Ok(_) | Err(Errno::EINTR)) {
        fds_address
            .map(|address| -> Result<Vec<PollFd>, Errno> {
                let mut fds = vec![PollFd::default(); nfds];
                memory.read_values(address.cast::<PollFd>().into(), &mut fds)?;
                Ok(fds)
            })
            .transpose()?
    } else {
        None
    };
    let timeout = timeout_address
        .map(|address| memory.read_value(address))
        .transpose()?;

    Ok(PpollEvent {
        result,
        fds_pointer_present,
        fds,
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

        let event = result.and_then(|ret| {
            let mut fds = vec![PollFd::default(); len];

            // It is fine for `fds` to be NULL. Poll is effectively a
            // `sleep` call and will always return 0 after a "timeout".
            if let Some(addr) = syscall.fds() {
                guest.memory().read_values(addr.into(), &mut fds)?;
            }

            let updated = ret as usize;
            Ok(SyscallEvent::Poll(PollEvent { fds, updated }))
        });

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
                let iovecs = read_iovecs(&guest.memory(), &output)?;
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

    // TODO: Add support for ppoll, epoll, and select here.
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::PollFlags;

    use super::*;

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
