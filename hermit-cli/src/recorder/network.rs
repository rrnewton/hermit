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
use reverie::syscalls::Accept4;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Syscall;
use reverie::syscalls::family::SockOptFamily;

use super::Recorder;
use crate::event::AcceptEvent;
use crate::event::PollEvent;
use crate::event::SockOptEvent;
use crate::event::SyscallEvent;

impl Recorder {
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

            // FIXME: There are cases where optval can be NULL.
            let mut value = vec![0u8; buflen as usize];
            guest.memory().read_exact(
                syscall.value().ok_or(Errno::EFAULT).unwrap().cast::<u8>(),
                &mut value,
            )?;

            // Need to read the (new) length. This might not have been updated,
            // but we don't know until we check it.
            let length: libc::socklen_t = guest.memory().read_value(buflen_addr)?;

            Ok(SyscallEvent::SockOpt(SockOptEvent { value, length }))
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

    /// Records `accept`/`accept4`.
    ///
    /// `accept` returns a new connection fd and, when the caller provides
    /// non-NULL `addr`/`addrlen`, writes the peer socket address plus its length
    /// into caller memory. We record the returned fd, the (possibly truncated)
    /// address bytes, and the length value so the connection can be replayed
    /// without a live peer. `accept` is dispatched here via `Accept4` (a plain
    /// `accept` is an `accept4` with zero flags).
    pub(super) async fn handle_accept<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Accept4,
    ) -> Result<i64, Errno> {
        // The caller's `addrlen` is an in/out parameter: on input it is the
        // buffer capacity, on output the (untruncated) peer address length. Read
        // the capacity before the syscall overwrites it.
        let capacity: Option<usize> = match syscall.addrlen() {
            Some(addr) => Some(guest.memory().read_value(addr)?),
            None => None,
        };

        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|fd| {
                let (sockaddr, addrlen) = match (syscall.sockaddr(), syscall.addrlen(), capacity) {
                    (Some(addr), Some(len_addr), Some(capacity)) => {
                        // The kernel wrote the untruncated length here and copied
                        // at most `capacity` bytes into the address buffer.
                        let out_len: usize = guest.memory().read_value(len_addr)?;
                        let copied = out_len.min(capacity);
                        let mut buf = vec![0u8; copied];
                        if copied > 0 {
                            guest.memory().read_exact(addr.cast::<u8>(), &mut buf)?;
                        }
                        (buf, out_len)
                    }
                    // Caller did not ask for the peer address.
                    _ => (Vec::new(), 0),
                };

                Ok(SyscallEvent::Accept(AcceptEvent {
                    fd,
                    sockaddr,
                    addrlen,
                }))
            }),
        );

        result
    }

    // TODO: Add support for ppoll, epoll, and select here.
}
