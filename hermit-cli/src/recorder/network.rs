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
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::PollFd;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::Syscall;
use reverie::syscalls::family::SockOptFamily;

use super::Recorder;
use crate::event::PollEvent;
use crate::event::RecvMsgEvent;
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

    /// Records a `recvmsg` call.
    ///
    /// Unlike `recvfrom`, `recvmsg` scatters the received payload across the
    /// caller's `msg_iov` buffers and can additionally fill in a source address
    /// (`msg_name`) and ancillary control data (`msg_control`, e.g.
    /// `SCM_RIGHTS` descriptor passing). We capture every field the kernel
    /// writes so replay can reconstruct the `msghdr` exactly.
    pub(super) async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmsg,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|ret| {
                let msg_addr = syscall.msg().ok_or(Errno::EFAULT)?;
                // The kernel updated the header in place; read it back to find
                // the scatter buffers and the output lengths.
                let header: libc::msghdr = guest.memory().read_value(msg_addr)?;

                let data = read_scattered(&guest.memory(), &header, ret as usize)?;
                let control = read_region(
                    &guest.memory(),
                    header.msg_control as usize,
                    header.msg_controllen,
                )?;
                let name = read_region(
                    &guest.memory(),
                    header.msg_name as usize,
                    header.msg_namelen as usize,
                )?;

                Ok(SyscallEvent::RecvMsg(RecvMsgEvent {
                    data,
                    control,
                    name,
                    msg_flags: header.msg_flags,
                }))
            }),
        );

        result
    }

    // TODO: Add support for ppoll, epoll, and select here.
}

/// Reads `length` bytes of guest memory starting at `address`, returning an
/// empty vector when there is nothing to read (a NULL pointer or zero length).
fn read_region<M: MemoryAccess>(
    memory: &M,
    address: usize,
    length: usize,
) -> Result<Vec<u8>, Errno> {
    if address == 0 || length == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; length];
    let addr = Addr::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
    memory.read_exact(addr, &mut buf)?;
    Ok(buf)
}

/// Gathers up to `total` bytes from the scatter buffers described by a
/// `msghdr`'s `msg_iov`/`msg_iovlen`, in iovec order (mirroring how the kernel
/// filled them).
fn read_scattered<M: MemoryAccess>(
    memory: &M,
    header: &libc::msghdr,
    total: usize,
) -> Result<Vec<u8>, Errno> {
    let mut data = Vec::with_capacity(total);
    if total == 0 {
        return Ok(data);
    }
    for iov in read_iovecs(memory, header)? {
        if data.len() == total {
            break;
        }
        let take = (total - data.len()).min(iov.iov_len);
        if take == 0 {
            continue;
        }
        let base = Addr::<u8>::from_raw(iov.iov_base as usize).ok_or(Errno::EFAULT)?;
        let start = data.len();
        data.resize(start + take, 0);
        memory.read_exact(base, &mut data[start..])?;
    }
    Ok(data)
}

/// Reads the `msg_iovlen` `iovec` entries pointed to by a `msghdr`.
pub(crate) fn read_iovecs<M: MemoryAccess>(
    memory: &M,
    header: &libc::msghdr,
) -> Result<Vec<libc::iovec>, Errno> {
    let count = header.msg_iovlen;
    if count == 0 || header.msg_iov.is_null() {
        return Ok(Vec::new());
    }
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        count
    ];
    let addr = Addr::<libc::iovec>::from_raw(header.msg_iov as usize).ok_or(Errno::EFAULT)?;
    memory.read_values(addr, &mut iovecs)?;
    Ok(iovecs)
}
