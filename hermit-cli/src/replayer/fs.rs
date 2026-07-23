/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Getdents;
use reverie::syscalls::Getdents64;
use reverie::syscalls::Ioctl;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Pread64;
use reverie::syscalls::Read;
use reverie::syscalls::Readlink;
use reverie::syscalls::Statx;
use reverie::syscalls::Syscall;
use reverie::syscalls::family::StatFamily;
use reverie::syscalls::family::WriteFamily;
use reverie::syscalls::ioctl;

use super::Replayer;

/// Scatter the recorded flat output `bytes` of a vectored read back into the
/// guest's `iovec` array, filling each buffer in order until the bytes are
/// exhausted. Returns the number of bytes written (the syscall return value).
fn scatter_iovec_output<M: MemoryAccess>(
    memory: &mut M,
    iov_addr: Option<usize>,
    iovcnt: usize,
    bytes: &[u8],
) -> Result<usize, Errno> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let addr = iov_addr
        .and_then(Addr::<libc::iovec>::from_raw)
        .ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        iovcnt
    ];
    memory.read_values(addr, &mut iovecs)?;

    let mut written = 0;
    for iovec in iovecs {
        if written == bytes.len() {
            break;
        }
        let take = (bytes.len() - written).min(iovec.iov_len);
        if take == 0 {
            continue;
        }
        let dst = AddrMut::<u8>::from_raw(iovec.iov_base as usize).ok_or(Errno::EFAULT)?;
        memory.write_exact(dst, &bytes[written..written + take])?;
        written += take;
    }
    // The recorded byte count must fit within the guest's provided iovecs.
    assert_eq!(written, bytes.len());
    Ok(written)
}

impl Replayer {
    /// Replays the vectored read family (`readv`/`preadv`/`preadv2`) by
    /// scattering the recorded flattened output bytes across the guest's current
    /// `iovec` buffers, without touching any live descriptor.
    pub(super) async fn handle_readv_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        iov_addr: Option<usize>,
        iovcnt: usize,
    ) -> Result<i64, Errno> {
        let bytes = next_event!(guest, Readv)?;
        let written = scatter_iovec_output(&mut guest.memory(), iov_addr, iovcnt, &bytes)?;
        Ok(written as i64)
    }

    // FIXME: Generalize the read-family of syscalls with `ReadFamily`.
    pub(super) async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Read,
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

    pub(super) async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pread64,
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

    pub(super) async fn handle_write_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: WriteFamily,
    ) -> Result<i64, Errno> {
        let count = next_event!(guest, Write)?;

        let fd = syscall.fd();

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#236): console-fd tracking (this write gate plus the
        // handle_close/dup/dup2/dup3/fcntl handlers below and EventReader's
        // console_fds set) replaces the previous `fd == 1 || fd == 2` check.
        // Only let a write through to the real console when its fd actually
        // refers to the inherited console in the recorded fd topology. Tracking
        // fds (see `handle_dup2`/`handle_close`) instead of hard-coding fd 1/2
        // is what keeps a redirected fd — e.g. a shell pipeline whose stdout was
        // dup2'd onto a pipe — from leaking its bytes to the console during
        // replay, and conversely lets through a console fd that was dup'd to a
        // higher number.
        if guest.thread_state().is_console(fd) {
            guest.inject_with_retry(Syscall::from(syscall)).await
        } else {
            Ok(count)
        }
    }

    /// Replays a `close`, dropping the fd from the console set so a later syscall
    /// that reuses the number does not inherit its console status.
    pub(super) async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Close,
    ) -> Result<i64, Errno> {
        let ret = next_event!(guest, Return)?;
        // Reaching here means `close` succeeded during recording.
        guest.thread_state_mut().set_console(syscall.fd(), false);
        Ok(ret)
    }

    /// Replays a `dup`, propagating the source fd's console status to the newly
    /// allocated descriptor (returned as the recorded value).
    pub(super) async fn handle_dup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Dup,
    ) -> Result<i64, Errno> {
        let ret = next_event!(guest, Return)?;
        if ret >= 0 {
            let console = guest.thread_state().is_console(syscall.oldfd());
            guest.thread_state_mut().set_console(ret as i32, console);
        }
        Ok(ret)
    }

    /// Replays a `dup2`, making `newfd` mirror `oldfd`'s console status.
    pub(super) async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Dup2,
    ) -> Result<i64, Errno> {
        let ret = next_event!(guest, Return)?;
        if ret >= 0 {
            let console = guest.thread_state().is_console(syscall.oldfd());
            guest
                .thread_state_mut()
                .set_console(syscall.newfd(), console);
        }
        Ok(ret)
    }

    /// Replays a `dup3`. The `O_CLOEXEC` flag only affects behavior across an
    /// `execve`, so for console tracking it is equivalent to `dup2`.
    pub(super) async fn handle_dup3<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Dup3,
    ) -> Result<i64, Errno> {
        let ret = next_event!(guest, Return)?;
        if ret >= 0 {
            let console = guest.thread_state().is_console(syscall.oldfd());
            guest
                .thread_state_mut()
                .set_console(syscall.newfd(), console);
        }
        Ok(ret)
    }

    /// Replays an `fcntl`. Only the fd-duplicating commands affect console
    /// tracking; every other command behaves like a simple return-value syscall.
    pub(super) async fn handle_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: reverie::syscalls::Fcntl,
    ) -> Result<i64, Errno> {
        let ret = next_event!(guest, Return)?;
        if ret >= 0
            && matches!(
                syscall.cmd(),
                reverie::syscalls::FcntlCmd::F_DUPFD(_)
                    | reverie::syscalls::FcntlCmd::F_DUPFD_CLOEXEC(_)
            )
        {
            let console = guest.thread_state().is_console(syscall.fd());
            guest.thread_state_mut().set_console(ret as i32, console);
        }
        Ok(ret)
    }

    pub(super) async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: StatFamily,
    ) -> Result<i64, Errno> {
        next_event!(guest, Stat).and_then(|event| {
            let addr = syscall.stat().ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr.0, &event.statbuf)?;
            // stat calls always return 0 on success.
            Ok(0)
        })
    }

    pub(super) async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<AddrMut<'_, libc::statfs>>,
    ) -> Result<i64, Errno> {
        let bytes = next_event!(guest, Statfs)?;
        assert_eq!(bytes.len(), std::mem::size_of::<libc::statfs>());
        guest
            .memory()
            .write_exact(buf.ok_or(Errno::EFAULT)?.cast(), &bytes)?;
        Ok(0)
    }

    pub(super) async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Statx,
    ) -> Result<i64, Errno> {
        next_event!(guest, Statx).and_then(|buf| {
            let addr = syscall.statx().ok_or(Errno::EFAULT)?;
            guest.memory().write_value(addr.0, &buf.into())?;
            // statx calls always return 0 on success.
            Ok(0)
        })
    }

    pub(super) async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ioctl,
    ) -> Result<i64, Errno> {
        let request = syscall.request();

        if matches!(
            request,
            ioctl::Request::FIOCLEX | ioctl::Request::FIONCLEX | ioctl::Request::FIONBIO(_)
        ) {
            // Replayed opens do not necessarily create host file descriptors.
            // Detcore updates the logical descriptor metadata after this returns.
            next_event!(guest, Return)
        } else if request.direction() == ioctl::Direction::Read {
            let output = next_event!(guest, Ioctl)?;
            request.write_output(&mut guest.memory(), &output)?;
            Ok(0)
        } else {
            let ret = next_event!(guest, Return)?;
            Ok(ret)
        }
    }

    pub(super) async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Readlink,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        debug_assert!(buf.len() <= syscall.bufsize());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        // Make sure we don't overflow the buffer.
        debug_assert!(buf.len() <= syscall.count() as usize);

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.dirent().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }

    pub(super) async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getdents64,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        // Make sure we don't overflow the buffer.
        debug_assert!(buf.len() <= syscall.count() as usize);

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.dirent().unwrap().cast::<u8>(), &buf)?;
        Ok(buf.len() as i64)
    }
}
