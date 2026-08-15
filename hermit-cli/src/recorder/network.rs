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

/// Capture `poll`'s guest-visible outputs, INCLUDING on the error path.
///
/// This mirrors [`capture_ppoll_event`]; see that function for the full
/// reasoning. The short version: `poll` previously built its event with
/// `result.and_then(..)`, and `and_then` NEVER RUNS ITS CLOSURE ON AN `Err`, so
/// an EFAULT return after Linux had already written some `revents` entries
/// discarded every one of them. Measured divergence: record `revents0=1`
/// against replay `revents0=4660`, where 4660 is 0x1234, the sentinel the guest
/// itself wrote before the call -- the field was never written back.
fn capture_poll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PollEvent, Errno> {
    let fds_pointer_present = fds_address.is_some();
    let read_fds = |address: AddrMut<'_, PollFd>| -> Result<Vec<PollFd>, Errno> {
        let mut fds = vec![PollFd::default(); nfds];
        memory.read_values(address.into(), &mut fds)?;
        Ok(fds)
    };
    let fds = match result {
        // It is fine for `fds` to be NULL: poll is then effectively a sleep.
        Ok(_) | Err(Errno::EINTR) => fds_address.map(read_fds).transpose()?,
        // BEST EFFORT, AND THE PREDICATE MUST STAY THIS NARROW. The same EFAULT
        // is also what a wholly bad `fds` pointer produces, and then this read
        // fails too; `?` here would discard the whole event and record a bare
        // `Err` -- exactly the bug being fixed. `fds_pointer_present` already
        // distinguishes "no pointer" from "pointer we could not read".
        //
        // Errors other than EFAULT and EINTR must not read AT ALL. An earlier
        // attempt on the ppoll side read on EVERY error and regressed
        // `review-cases invalid-nfds`, which passes fds=(void*)1 with nfds over
        // RLIMIT_NOFILE and gets EINVAL -- the read's own EFAULT(14) replaced
        // the kernel's EINVAL(22) and flipped that case from matched to
        // diverged. WITHOUT THAT GUARD THIS CHANGE LOOKS CORRECT AND SILENTLY
        // CORRUPTS A RECORDED ERRNO.
        //
        // NOT UNIT-TESTED, DELIBERATELY: the unit tests use `LocalMemory`, which
        // reads this process's own memory and SEGFAULTS THE TEST BINARY on a bad
        // pointer instead of returning `Err`, so `.ok()` cannot catch it. The
        // policy is covered end to end by `review-cases invalid-nfds` staying
        // matched.
        Err(Errno::EFAULT) => fds_address.and_then(|address| read_fds(address).ok()),
        _ => None,
    };

    Ok(PollEvent {
        result,
        fds_pointer_present,
        fds,
    })
}

fn capture_ppoll_event<M: MemoryAccess>(
    memory: &M,
    fds_address: Option<AddrMut<'_, libc::pollfd>>,
    timeout_address: Option<AddrMut<'_, Timespec>>,
    nfds: usize,
    result: Result<i64, Errno>,
) -> Result<PpollEvent, Errno> {
    let fds_pointer_present = fds_address.is_some();
    let read_fds = |address: AddrMut<'_, libc::pollfd>| -> Result<Vec<PollFd>, Errno> {
        let mut fds = vec![PollFd::default(); nfds];
        memory.read_values(address.cast::<PollFd>().into(), &mut fds)?;
        Ok(fds)
    };
    let fds = match result {
        Ok(_) | Err(Errno::EINTR) => fds_address.map(read_fds).transpose()?,
        // Linux writes `revents` entry by entry and can fault partway through
        // the copy-out, so an EFAULT return may still leave earlier entries
        // written. Those are guest-visible, so capture them or replay restores
        // nothing and the guest keeps its pre-syscall values.
        //
        // BEST EFFORT, AND THAT IS LOAD-BEARING. The same EFAULT is also what a
        // wholly bad `fds` pointer produces, and then this read fails too. `?`
        // here would discard the WHOLE event -- including the captured timeout
        // this function exists to preserve -- and record a bare `Err` instead.
        // `fds_pointer_present` already distinguishes "no pointer" from "pointer
        // we could not read", so `None` here is not ambiguous.
        //
        // NOT UNIT-TESTED, DELIBERATELY, AND THIS IS NOT AN OVERSIGHT: the unit
        // tests use `LocalMemory`, which reads this process's own memory
        // directly, so an unreadable address SEGFAULTS THE TEST BINARY instead
        // of returning `Err` -- `.ok()` cannot catch that, and a test written
        // that way crashes rather than failing. Guest memory in production goes
        // through `process_vm_readv`, which does return `Err`. The policy is
        // therefore covered end to end by `review-cases invalid-nfds` staying
        // matched, not by a unit test.
        //
        // Note the predicate, not just the policy: errors other than EFAULT and
        // EINTR must not read at all. An earlier attempt read on EVERY error and
        // regressed `review-cases invalid-nfds`, which passes fds=(void*)1 with
        // nfds over RLIMIT_NOFILE and gets EINVAL -- the read's own EFAULT(14)
        // replaced the kernel's EINVAL(22) and flipped that case from matched to
        // diverged.
        Err(Errno::EFAULT) => fds_address.and_then(|address| read_fds(address).ok()),
        _ => None,
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
