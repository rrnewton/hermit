/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Recvmsg;
use reverie::syscalls::family::SockOptFamily;

use super::Replayer;
use crate::recorder::read_iovecs;

impl Replayer {
    pub(super) async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Poll,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Poll)?;

        let nfds = syscall.nfds() as usize;

        assert_eq!(event.fds.len(), nfds);

        // Write out the recorded fds (if any).
        if let Some(addr) = syscall.fds() {
            guest.memory().write_values(addr, &event.fds)?;
        }

        Ok(event.updated as i64)
    }

    pub(super) async fn handle_sockopt_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: SockOptFamily,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, SockOpt)?;

        // Write out the value.
        guest.memory().write_exact(
            syscall.value().ok_or(Errno::EFAULT)?.cast::<u8>(),
            &event.value,
        )?;

        // Write out the length parameter.
        guest
            .memory()
            .write_value(syscall.value_len().ok_or(Errno::EFAULT)?, &event.length)?;

        Ok(0)
    }

    pub(super) async fn handle_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvfrom,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        assert!(buf.len() <= syscall.len());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap(), &buf)
            .unwrap();
        Ok(buf.len() as i64)
    }

    /// Replays a `recvmsg` call by reconstructing the `msghdr` the kernel would
    /// have produced: the payload is scattered back across the caller's
    /// `msg_iov` buffers, and the source address, ancillary control data (e.g.
    /// `SCM_RIGHTS`), lengths, and `msg_flags` are restored in place.
    pub(super) async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmsg,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, RecvMsg)?;

        let msg_addr = syscall.msg().ok_or(Errno::EFAULT)?;
        let mut header: libc::msghdr = guest.memory().read_value(msg_addr)?;

        // Scatter the recorded payload across the guest's iovecs, in order.
        let mut offset = 0;
        for iov in read_iovecs(&guest.memory(), &header)? {
            if offset == event.data.len() {
                break;
            }
            let take = (event.data.len() - offset).min(iov.iov_len);
            if take == 0 {
                continue;
            }
            let base = AddrMut::<u8>::from_raw(iov.iov_base as usize).ok_or(Errno::EFAULT)?;
            guest
                .memory()
                .write_exact(base, &event.data[offset..offset + take])?;
            offset += take;
        }

        // Restore the ancillary control data (SCM_RIGHTS lives here). The guest
        // supplied a buffer at least as large during recording, so the recorded
        // bytes fit.
        if !event.control.is_empty() {
            let addr = AddrMut::<u8>::from_raw(header.msg_control as usize).ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(addr, &event.control)?;
        }
        header.msg_controllen = event.control.len();

        // Restore the source address.
        if !event.name.is_empty() {
            let addr = AddrMut::<u8>::from_raw(header.msg_name as usize).ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(addr, &event.name)?;
        }
        header.msg_namelen = event.name.len() as libc::socklen_t;
        header.msg_flags = event.msg_flags;

        // Write the updated header (lengths and flags) back to the guest.
        guest.memory().write_value(msg_addr, &header)?;

        Ok(event.data.len() as i64)
    }
}
