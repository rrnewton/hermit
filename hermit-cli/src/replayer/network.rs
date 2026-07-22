/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::Accept4;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Poll;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::family::SockOptFamily;

use super::Replayer;

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

    /// Replays `accept`/`accept4` from the recorded event.
    ///
    /// Writes the recorded peer address and length back into the caller's
    /// buffers (when it asked for them) and returns the recorded connection fd,
    /// without executing a real `accept`. Mirrors [`Recorder::handle_accept`].
    ///
    /// [`Recorder::handle_accept`]: crate::recorder::Recorder::handle_accept
    pub(super) async fn handle_accept<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Accept4,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Accept)?;

        // Restore the peer address bytes, if the caller provided a buffer and one
        // was recorded.
        if let Some(addr) = syscall.sockaddr() {
            if !event.sockaddr.is_empty() {
                guest
                    .memory()
                    .write_exact(addr.cast::<u8>(), &event.sockaddr)?;
            }
        }

        // Restore the (untruncated) address length.
        if let Some(len_addr) = syscall.addrlen() {
            guest.memory().write_value(len_addr, &event.addrlen)?;
        }

        Ok(event.fd)
    }
}
