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

use super::Replayer;
use crate::event::PollEvent;
use crate::event::PpollEvent;
use crate::event::RecvmsgEvent;
use crate::event::SelectEvent;
use crate::event::fd_set_bytes;

fn replay_pollfds<M: MemoryAccess>(
    memory: &mut M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    result: Result<i64, Errno>,
    fds_pointer_present: bool,
    fds: Option<Vec<PollFd>>,
) -> Result<(), Errno> {
    assert_eq!(
        fds_address.is_some(),
        fds_pointer_present,
        "recorded pollfd pointer shape diverged during replay"
    );

    if matches!(result, Ok(_) | Err(Errno::EINTR)) {
        assert_eq!(fds.is_some(), fds_pointer_present);
        if let Ok(updated) = result {
            assert!(updated >= 0);
            assert!((updated as usize) <= nfds);
        }
    } else if !matches!(result, Err(Errno::EFAULT)) {
        assert!(fds.is_none());
    }

    if let Some(fds) = fds {
        assert!(fds.len() <= nfds);
        if matches!(result, Ok(_) | Err(Errno::EINTR)) {
            assert_eq!(fds.len(), nfds);
        }
        let address = fds_address.expect("recorded pollfd output requires a pointer");
        let write_result = memory.write_values(address, &fds);
        if matches!(result, Err(Errno::EFAULT)) {
            // The same page boundary that made Linux return EFAULT can make
            // this replay write fault after restoring an earlier prefix. Keep
            // going so ppoll can also restore its captured timeout, then return
            // the recorded errno unchanged.
            if let Err(error) = write_result {
                tracing::trace!(?error, "partial pollfd replay write returned an error");
            }
        } else {
            write_result?;
        }
    }
    Ok(())
}

fn replay_poll_event<M: MemoryAccess>(
    memory: &mut M,
    fds_address: Option<AddrMut<'_, PollFd>>,
    nfds: usize,
    event: PollEvent,
) -> Result<i64, Errno> {
    let PollEvent {
        result,
        fds_pointer_present,
        fds,
    } = event;
    replay_pollfds(memory, fds_address, nfds, result, fds_pointer_present, fds)?;
    result
}

fn replay_ppoll_event<M: MemoryAccess>(
    memory: &mut M,
    fds_address: Option<AddrMut<'_, libc::pollfd>>,
    timeout_address: Option<AddrMut<'_, Timespec>>,
    nfds: usize,
    event: PpollEvent,
) -> Result<i64, Errno> {
    let PpollEvent {
        result,
        fds_pointer_present,
        fds,
        timeout_pointer_present,
        timeout,
    } = event;

    replay_pollfds(
        memory,
        fds_address.map(|address| address.cast::<PollFd>()),
        nfds,
        result,
        fds_pointer_present,
        fds,
    )?;

    assert_eq!(
        timeout_address.is_some(),
        timeout_pointer_present,
        "recorded ppoll timeout pointer shape diverged during replay"
    );
    if !matches!(result, Err(Errno::EFAULT)) {
        assert_eq!(timeout.is_some(), timeout_pointer_present);
    }
    if let Some(timeout) = timeout {
        let address = timeout_address.expect("recorded ppoll timeout requires a pointer");
        // Linux preserves the ppoll result when remaining-time copyout faults.
        // Restore the exact captured value when possible, but never replace the
        // recorded result with a replay-only memory error.
        if let Err(error) = memory.write_value(address, &timeout) {
            tracing::trace!(?error, "ppoll timeout replay write returned an error");
        }
    }

    result
}

fn write_bytes<M: MemoryAccess>(
    memory: &mut M,
    pointer: *mut libc::c_void,
    bytes: &[u8],
) -> Result<(), Errno> {
    if bytes.is_empty() {
        return Ok(());
    }
    let address = AddrMut::<u8>::from_raw(pointer as usize).ok_or(Errno::EFAULT)?;
    memory.write_exact(address.cast(), bytes)
}

/// Write a recorded receive back through the guest `msghdr` at
/// `message_address`. Shared by `recvmsg` and every message of `recvmmsg`.
fn restore_recvmsg<M: MemoryAccess>(
    memory: &mut M,
    message_address: AddrMut<'_, libc::msghdr>,
    event: &RecvmsgEvent,
) -> Result<(), Errno> {
    let message: libc::msghdr = memory.read_value(message_address)?;
    let iovecs = crate::read_iovecs(memory, &message)?;
    assert_eq!(iovecs.len(), event.iovs.len());

    for (iovec, bytes) in iovecs.into_iter().zip(&event.iovs) {
        assert!(bytes.len() <= iovec.iov_len);
        write_bytes(memory, iovec.iov_base, bytes)?;
    }

    assert!(event.name.len() <= message.msg_namelen as usize);
    assert!(event.control.len() <= message.msg_controllen);
    write_bytes(memory, message.msg_name, &event.name)?;
    write_bytes(memory, message.msg_control, &event.control)?;

    // Linux writes back only these fields, and msg_namelen only with a name
    // buffer; the rest of the header may be on a page the guest cannot write.
    let header = message_address.as_raw();
    if !message.msg_name.is_null() {
        write_field(
            memory,
            header + std::mem::offset_of!(libc::msghdr, msg_namelen),
            &event.name_len,
        )?;
    }
    write_field(
        memory,
        header + std::mem::offset_of!(libc::msghdr, msg_controllen),
        &event.control_len,
    )?;
    write_field(
        memory,
        header + std::mem::offset_of!(libc::msghdr, msg_flags),
        &event.flags,
    )
}

fn write_field<M: MemoryAccess, T: Copy>(
    memory: &mut M,
    address: usize,
    value: &T,
) -> Result<(), Errno> {
    let address = AddrMut::<T>::from_raw(address).ok_or(Errno::EFAULT)?;
    memory.write_value(address, value)
}

/// The address of `mmsghdr` entry `index` of the array at `base`.
fn mmsghdr_address<'a>(base: usize, index: usize) -> Result<AddrMut<'a, libc::mmsghdr>, Errno> {
    index
        .checked_mul(std::mem::size_of::<libc::mmsghdr>())
        .and_then(|offset| base.checked_add(offset))
        .and_then(AddrMut::from_raw)
        .ok_or(Errno::EFAULT)
}

/// Write `length` to the `msg_len` field of `mmsghdr` entry `index`.
fn write_mmsg_len<M: MemoryAccess>(
    memory: &mut M,
    base: usize,
    index: usize,
    length: u32,
) -> Result<(), Errno> {
    let entry = mmsghdr_address(base, index)?.as_raw();
    let field = AddrMut::<u32>::from_raw(entry + std::mem::offset_of!(libc::mmsghdr, msg_len))
        .ok_or(Errno::EFAULT)?;
    memory.write_value(field, &length)
}

fn replay_select_event<M: MemoryAccess>(
    memory: &mut M,
    nfds: i32,
    fd_set_addresses: [Option<AddrMut<'_, libc::fd_set>>; 3],
    timeout_address: Option<AddrMut<'_, u8>>,
    event: SelectEvent,
) -> Result<i64, Errno> {
    let SelectEvent {
        result,
        fd_sets,
        timeout,
    } = event;
    let length = fd_set_bytes(nfds);
    for (address, bytes) in fd_set_addresses.into_iter().zip(fd_sets) {
        let Some(bytes) = bytes else {
            assert!(
                address.is_none() || result.is_err(),
                "recorded select omitted a set the kernel copied out"
            );
            continue;
        };
        assert!(bytes.len() <= length);
        let address = address.expect("recorded select set output requires a pointer");
        let write_result = write_bytes(memory, address.as_raw() as *mut libc::c_void, &bytes);
        if matches!(result, Err(Errno::EFAULT)) {
            // As for ppoll: the fault that ended the recorded copy-out may end
            // this one too. The recorded errno is still the result.
            if let Err(error) = write_result {
                tracing::trace!(?error, "partial select set replay write returned an error");
            }
        } else {
            write_result?;
        }
    }
    if let Some(timeout) = timeout {
        let address = timeout_address.expect("recorded select timeout requires a pointer");
        // Linux keeps the select result when the remaining-time write faults.
        if let Err(error) = write_bytes(memory, address.as_raw() as *mut libc::c_void, &timeout) {
            tracing::trace!(?error, "select timeout replay write returned an error");
        }
    }
    result
}

fn cmsg_align(length: usize) -> Option<usize> {
    let alignment = std::mem::size_of::<usize>();
    length
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn scm_rights_fds(control: &[u8]) -> Vec<i32> {
    let header_size = std::mem::size_of::<libc::cmsghdr>();
    let data_offset = cmsg_align(header_size).unwrap();
    let mut offset: usize = 0;
    let mut fds = Vec::new();

    while offset
        .checked_add(header_size)
        .is_some_and(|end| end <= control.len())
    {
        // The recorded control buffer has native cmsghdr layout but may not be
        // aligned as a Vec<u8>, so read the header without assuming alignment.
        let header = unsafe {
            std::ptr::read_unaligned(control.as_ptr().add(offset).cast::<libc::cmsghdr>())
        };
        let length = header.cmsg_len;
        let Some(end) = offset.checked_add(length) else {
            break;
        };
        if length < data_offset || end > control.len() {
            break;
        }

        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            let (fd_bytes, _) =
                control[offset + data_offset..end].as_chunks::<{ std::mem::size_of::<i32>() }>();
            for bytes in fd_bytes {
                let fd = i32::from_ne_bytes(*bytes);
                if fd >= 0 {
                    fds.push(fd);
                }
            }
        }

        let Some(aligned_length) = cmsg_align(length) else {
            break;
        };
        let Some(next) = offset.checked_add(aligned_length) else {
            break;
        };
        if next <= offset {
            break;
        }
        offset = next;
    }
    fds
}

impl Replayer {
    pub(super) async fn handle_epoll_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: EpollWait,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, EpollWait)?;
        if event.replay_kernel_side_effect {
            let actual = guest.inject(syscall).await;
            assert_eq!(
                actual,
                Ok(event.updated as i64),
                "replayed epoll_wait kernel side effect diverged"
            );
        }
        assert_eq!(
            event.events.len(),
            event.updated * std::mem::size_of::<libc::epoll_event>()
        );
        assert!(event.updated <= syscall.maxevents() as usize);

        if !event.events.is_empty() {
            guest
                .memory()
                .write_exact(syscall.events().ok_or(Errno::EFAULT)?.cast(), &event.events)?;
        }
        Ok(event.updated as i64)
    }

    pub(super) async fn handle_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Poll,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Poll)?;
        replay_poll_event(
            &mut guest.memory(),
            syscall.fds(),
            syscall.nfds() as usize,
            event,
        )
    }

    pub(super) async fn handle_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Ppoll,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Ppoll)?;
        replay_ppoll_event(
            &mut guest.memory(),
            syscall.fds(),
            syscall.timeout(),
            syscall.nfds() as usize,
            event,
        )
    }

    pub(super) async fn handle_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Select,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Select)?;
        replay_select_event(
            &mut guest.memory(),
            syscall.nfds(),
            [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
            syscall.timeout().map(AddrMut::cast),
            event,
        )
    }

    pub(super) async fn handle_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Pselect6,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Select)?;
        replay_select_event(
            &mut guest.memory(),
            syscall.nfds(),
            [syscall.readfds(), syscall.writefds(), syscall.exceptfds()],
            syscall.timeout().map(AddrMut::cast),
            event,
        )
    }

    pub(super) async fn handle_sockopt_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: SockOptFamily,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, SockOpt)?;

        // A NULL value buffer is valid when the recorded value is empty.
        if let Some(address) = syscall.value() {
            guest
                .memory()
                .write_exact(address.cast::<u8>(), &event.value)?;
        } else if !event.value.is_empty() {
            return Err(Errno::EFAULT);
        }

        // Write out the length parameter.
        guest
            .memory()
            .write_value(syscall.value_len().ok_or(Errno::EFAULT)?, &event.length)?;

        Ok(0)
    }

    pub(super) async fn handle_recvmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmsg,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Recvmsg)?;
        let cloexec = syscall.flags() & libc::MSG_CMSG_CLOEXEC != 0;
        for fd in scm_rights_fds(&event.control) {
            self.reserve_replay_fd(guest, fd, cloexec).await;
        }

        let message_address = syscall.msg().ok_or(Errno::EFAULT)?;
        restore_recvmsg(&mut guest.memory(), message_address, &event)?;

        Ok(event.result)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Restore every recorded message of a `recvmmsg` without receiving live.
    pub(super) async fn handle_recvmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Recvmmsg,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Recvmmsg)?;
        assert!(event.messages.len() <= syscall.vlen() as usize);
        let cloexec = syscall.flags() as i32 & libc::MSG_CMSG_CLOEXEC != 0;
        let base = syscall.mmsg().ok_or(Errno::EFAULT)?.as_raw();

        for (index, message) in event.messages.iter().enumerate() {
            // Linux installs each message's SCM_RIGHTS fds in message order.
            for fd in scm_rights_fds(&message.control) {
                self.reserve_replay_fd(guest, fd, cloexec).await;
            }
            // msg_hdr is the first field of mmsghdr.
            let header = mmsghdr_address(base, index)?.cast::<libc::msghdr>();
            restore_recvmsg(&mut guest.memory(), header, message)?;
            let length = u32::try_from(message.result).expect("recorded msg_len fits in u32");
            write_mmsg_len(&mut guest.memory(), base, index, length)?;
        }

        if let Some(prefix) = &event.timeout_fault {
            // Linux received these messages, then copied only `prefix` of the
            // remaining timeout back before faulting.
            assert!(event.timeout.is_none() && !event.messages.is_empty());
            let address = syscall
                .timeout()
                .expect("a timeout fault needs a timeout")
                .as_raw();
            let address = AddrMut::<u8>::from_raw(address).ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(address, prefix)?;
            return Err(Errno::EFAULT);
        }
        // Linux writes the remaining timeout back exactly when it was given
        // one and received something.
        assert_eq!(
            event.timeout.is_some(),
            syscall.timeout().is_some() && !event.messages.is_empty()
        );
        if let Some(timeout) = event.timeout {
            let address = AddrMut::<Timespec>::from_raw(syscall.timeout().unwrap().as_raw())
                .ok_or(Errno::EFAULT)?;
            guest.memory().write_value(address, &timeout)?;
        }

        Ok(event.messages.len() as i64)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Restore the recorded `msg_len` of every message a `sendmmsg` sent.
    pub(super) async fn handle_sendmmsg<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Sendmmsg,
    ) -> Result<i64, Errno> {
        let lengths = next_event!(guest, Sendmmsg)?;
        assert!(lengths.len() <= syscall.vlen() as usize);
        let base = syscall.msgvec().ok_or(Errno::EFAULT)?.as_raw();
        for (index, length) in lengths.iter().enumerate() {
            write_mmsg_len(&mut guest.memory(), base, index, *length)?;
        }
        Ok(lengths.len() as i64)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Restore a recorded `accept`/`accept4` without accepting live: the
    /// recorded peer never connects during replay, so a live accept would hang.
    pub(super) async fn handle_accept<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _syscall: Syscall,
        call: Accept4,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Accept)?;

        if let Some(addr_len) = event.addr_len {
            let addr = call
                .sockaddr()
                .expect("recorded peer address requires a buffer");
            let addr_len_address = call
                .addrlen()
                .expect("recorded peer address requires a length")
                .cast::<libc::socklen_t>();
            let capacity: libc::socklen_t = guest.memory().read_value(addr_len_address)?;
            assert!(event.addr.len() <= capacity as usize);
            write_bytes(
                &mut guest.memory(),
                addr.as_raw() as *mut libc::c_void,
                &event.addr,
            )?;
            guest.memory().write_value(addr_len_address, &addr_len)?;
        } else {
            assert!(
                call.sockaddr().is_none(),
                "accept peer address shape diverged during replay"
            );
        }

        let shape = event.shape.unwrap_or_else(|| {
            panic!(
                "recorded accept of fd {} lacks the socket shape replay needs to reserve it",
                event.fd
            )
        });
        self.reserve_replay_socket(guest, event.fd, shape, call.flags())
            .await;
        Ok(i64::from(event.fd))
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3579)
    /// Recreate a recorded `socketpair` live. It touches no network, and the
    /// resulting connected pair keeps later live fd operations valid; assert
    /// the kernel chose the recorded fds.
    pub(super) async fn handle_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Socketpair,
    ) -> Result<i64, Errno> {
        let recorded = next_event!(guest, Socketpair)?;
        let actual = guest.inject_with_retry(syscall).await;
        assert_eq!(actual, Ok(0), "socketpair side effects diverged");
        let fds: [i32; 2] = guest
            .memory()
            .read_value(syscall.usockvec().ok_or(Errno::EFAULT)?)?;
        assert_eq!(fds, recorded, "socketpair fd allocation diverged");
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
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::PollFlags;

    use super::*;

    #[test]
    fn restore_recvmsg_writes_payload_name_and_header() {
        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
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
        let mut name = [0u8; 2];
        // SAFETY: an all-zero msghdr is valid; the fields used are set below.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = iovecs.as_mut_ptr();
        message.msg_iovlen = iovecs.len();
        message.msg_name = name.as_mut_ptr().cast();
        message.msg_namelen = name.len() as libc::socklen_t;
        let event = RecvmsgEvent {
            result: 6,
            iovs: vec![b"abcd".to_vec(), b"ef".to_vec()],
            name: b"NA".to_vec(),
            name_len: 16,
            control: Vec::new(),
            control_len: 0,
            flags: libc::MSG_TRUNC,
        };

        restore_recvmsg(
            &mut LocalMemory::new(),
            AddrMut::from_raw((&mut message as *mut libc::msghdr) as usize).unwrap(),
            &event,
        )
        .unwrap();

        assert_eq!(&first, b"abcd");
        assert_eq!(&second, b"ef\0\0");
        assert_eq!(&name, b"NA");
        assert_eq!(message.msg_namelen, 16);
        assert_eq!(message.msg_controllen, 0);
        assert_eq!(message.msg_flags, libc::MSG_TRUNC);
    }

    /// sendmmsg/recvmmsg replay writes each message's msg_len into its own
    /// mmsghdr entry and leaves the header beside it untouched.
    #[test]
    fn write_mmsg_len_targets_each_entry() {
        // SAFETY: an all-zero mmsghdr is valid.
        let mut entries: [libc::mmsghdr; 3] = unsafe { std::mem::zeroed() };
        entries[1].msg_hdr.msg_flags = 0x55;
        let base = entries.as_mut_ptr() as usize;

        write_mmsg_len(&mut LocalMemory::new(), base, 0, 7).unwrap();
        write_mmsg_len(&mut LocalMemory::new(), base, 1, 9).unwrap();

        assert_eq!(entries[0].msg_len, 7);
        assert_eq!(entries[1].msg_len, 9);
        assert_eq!(entries[2].msg_len, 0);
        assert_eq!(entries[1].msg_hdr.msg_flags, 0x55);
    }

    #[test]
    fn replay_poll_restores_outputs_before_returning_efault() {
        let mut output = libc::pollfd {
            fd: 3,
            events: libc::POLLIN,
            revents: 0x1234,
        };
        let event = PollEvent {
            result: Err(Errno::EFAULT),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 3,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
        };
        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::<libc::pollfd>::from_raw((&mut output as *mut libc::pollfd) as usize)
                .map(|address| address.cast::<PollFd>()),
            1,
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert_eq!(output.revents, libc::POLLIN);
    }

    #[test]
    fn replay_ppoll_restores_outputs_and_exact_timeout_before_efault() {
        let mut output = libc::pollfd {
            fd: 5,
            events: libc::POLLIN,
            revents: 0x1234,
        };
        let mut timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_789_123,
        };
        let recorded_timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_780_001,
        };
        let event = PpollEvent {
            result: Err(Errno::EFAULT),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 5,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
            timeout_pointer_present: true,
            timeout: Some(recorded_timeout),
        };
        let result = replay_ppoll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw((&mut output as *mut libc::pollfd) as usize),
            AddrMut::from_raw((&mut timeout as *mut Timespec) as usize),
            1,
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert_eq!(output.revents, libc::POLLIN);
        assert_eq!(timeout, recorded_timeout);
    }

    #[test]
    fn replay_poll_restores_outputs_before_returning_eintr() {
        let mut output = libc::pollfd {
            fd: 11,
            events: libc::POLLIN,
            revents: 0x1234,
        };
        let event = PollEvent {
            result: Err(Errno::EINTR),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 11,
                events: PollFlags::POLLIN,
                revents: PollFlags::empty(),
            }]),
        };
        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::<libc::pollfd>::from_raw((&mut output as *mut libc::pollfd) as usize)
                .map(|address| address.cast::<PollFd>()),
            1,
            event,
        );

        assert_eq!(result, Err(Errno::EINTR));
        assert_eq!(output.revents, 0);
    }

    #[test]
    fn replay_einval_performs_no_pollfd_write() {
        let event = PollEvent {
            result: Err(Errno::EINVAL),
            fds_pointer_present: true,
            fds: None,
        };
        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw(1),
            usize::MAX,
            event,
        );

        assert_eq!(result, Err(Errno::EINVAL));
    }
}

#[cfg(test)]
mod current_main_tests {
    use std::io;

    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::MemoryAccess;
    use reverie::syscalls::PollFlags;

    use super::*;

    struct FailSecondWrite {
        memory: LocalMemory,
        writes: usize,
    }

    impl MemoryAccess for FailSecondWrite {
        fn read_vectored(
            &self,
            read_from: &[io::IoSlice],
            write_to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.memory.read_vectored(read_from, write_to)
        }

        fn write_vectored(
            &mut self,
            _read_from: &[io::IoSlice],
            _write_to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            unreachable!("write_exact dispatches through the overridden write method")
        }

        fn write(&mut self, address: AddrMut<u8>, bytes: &[u8]) -> Result<usize, Errno> {
            self.writes += 1;
            if self.writes == 2 {
                Err(Errno::EFAULT)
            } else {
                self.memory.write(address, bytes)
            }
        }
    }

    #[test]
    fn replay_ppoll_restores_exact_timeout_and_successful_pollfds() {
        let mut output_fd = libc::pollfd {
            fd: 7,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut output_timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let recorded_timeout = Timespec {
            tv_sec: 3,
            tv_nsec: 456_789_123,
        };
        let event = PpollEvent {
            result: Ok(1),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 7,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
            timeout_pointer_present: true,
            timeout: Some(recorded_timeout),
        };

        let result = replay_ppoll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw((&mut output_fd as *mut libc::pollfd) as usize),
            AddrMut::from_raw((&mut output_timeout as *mut Timespec) as usize),
            1,
            event,
        );

        assert_eq!(result, Ok(1));
        assert_eq!(output_fd.revents, libc::POLLIN);
        assert_eq!(output_timeout, recorded_timeout);
    }

    #[test]
    fn replay_ppoll_restores_exact_timeout_before_recorded_error() {
        let mut output_timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let recorded_timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 345_678_901,
        };
        let event = PpollEvent {
            result: Err(Errno::EINTR),
            fds_pointer_present: false,
            fds: None,
            timeout_pointer_present: true,
            timeout: Some(recorded_timeout),
        };

        let result = replay_ppoll_event(
            &mut LocalMemory::new(),
            None,
            AddrMut::from_raw((&mut output_timeout as *mut Timespec) as usize),
            0,
            event,
        );

        assert_eq!(result, Err(Errno::EINTR));
        assert_eq!(output_timeout, recorded_timeout);
    }

    #[test]
    fn replay_ppoll_writes_ready_fds_and_preserves_result_on_timeout_copyout_error() {
        let mut output_fd = libc::pollfd {
            fd: 7,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut output_timeout = Timespec {
            tv_sec: 9,
            tv_nsec: 876_543_210,
        };
        let original_timeout = output_timeout;
        let event = PpollEvent {
            result: Ok(1),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 7,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
            timeout_pointer_present: true,
            timeout: Some(Timespec {
                tv_sec: 3,
                tv_nsec: 456_789_123,
            }),
        };
        let mut memory = FailSecondWrite {
            memory: LocalMemory::new(),
            writes: 0,
        };

        let result = replay_ppoll_event(
            &mut memory,
            AddrMut::from_raw((&mut output_fd as *mut libc::pollfd) as usize),
            AddrMut::from_raw((&mut output_timeout as *mut Timespec) as usize),
            1,
            event,
        );

        assert_eq!(result, Ok(1));
        assert_eq!(memory.writes, 2);
        assert_eq!(output_fd.revents, libc::POLLIN);
        assert_eq!(output_timeout, original_timeout);
    }

    #[test]
    fn replay_poll_restores_fds_before_returning_a_recorded_error() {
        // THE ORDERING IS THE FIX. The old replayer returned the recorded error
        // before writing anything, so a guest that recorded a partial `revents`
        // copy-out saw its own pre-syscall sentinels instead.
        let mut observed = [PollFd {
            fd: 7,
            events: PollFlags::POLLIN,
            revents: PollFlags::empty(),
        }];
        let event = PollEvent {
            result: Err(Errno::EFAULT),
            fds_pointer_present: true,
            fds: Some(vec![PollFd {
                fd: 7,
                events: PollFlags::POLLIN,
                revents: PollFlags::POLLIN,
            }]),
        };

        let result = replay_poll_event(
            &mut LocalMemory::new(),
            AddrMut::from_raw(observed.as_mut_ptr() as usize),
            observed.len(),
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert_eq!(
            observed[0].revents,
            PollFlags::POLLIN,
            "the recorded partial copy-out must be restored even though the call errored"
        );
    }

    #[test]
    fn replay_poll_early_error_with_pollfd_pointer_performs_no_fd_write() {
        let event = PollEvent {
            result: Err(Errno::EINVAL),
            fds_pointer_present: true,
            fds: None,
        };
        let mut memory = FailSecondWrite {
            memory: LocalMemory::new(),
            writes: 0,
        };

        let result = replay_poll_event(&mut memory, AddrMut::from_raw(1), usize::MAX, event);

        assert_eq!(result, Err(Errno::EINVAL));
        assert_eq!(memory.writes, 0);
    }

    #[test]
    fn replay_ppoll_early_error_with_pollfd_pointer_performs_no_fd_write() {
        let event = PpollEvent {
            result: Err(Errno::EINVAL),
            fds_pointer_present: true,
            fds: None,
            timeout_pointer_present: false,
            timeout: None,
        };
        let mut memory = FailSecondWrite {
            memory: LocalMemory::new(),
            writes: 0,
        };

        let result = replay_ppoll_event(&mut memory, AddrMut::from_raw(1), None, usize::MAX, event);

        assert_eq!(result, Err(Errno::EINVAL));
        assert_eq!(memory.writes, 0);
    }

    #[test]
    fn replay_select_restores_sets_and_exact_timeout() {
        let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe { libc::FD_SET(3, &mut readfds) };
        let mut timeout = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        let recorded_timeout = libc::timeval {
            tv_sec: 1,
            tv_usec: 999_873,
        };
        let recorded_timeout_bytes = unsafe {
            std::slice::from_raw_parts(
                (&recorded_timeout as *const libc::timeval).cast::<u8>(),
                std::mem::size_of::<libc::timeval>(),
            )
        }
        .to_vec();
        let event = SelectEvent {
            result: Ok(0),
            fd_sets: [Some(vec![0; 8]), None, None],
            timeout: Some(recorded_timeout_bytes),
        };
        let result = replay_select_event(
            &mut LocalMemory::new(),
            4,
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                None,
                None,
            ],
            AddrMut::from_raw((&mut timeout as *mut libc::timeval) as usize),
            event,
        );

        assert_eq!(result, Ok(0));
        assert!(!unsafe { libc::FD_ISSET(3, &readfds) });
        assert_eq!(timeout.tv_sec, recorded_timeout.tv_sec);
        assert_eq!(timeout.tv_usec, recorded_timeout.tv_usec);
    }

    #[test]
    fn replay_select_restores_only_the_recorded_prefix_before_efault() {
        // Recorded: the read set was partly copied out (its first long), then
        // the kernel faulted before reaching the write set.
        let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
        let mut writefds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe {
            libc::FD_SET(3, &mut readfds);
            libc::FD_SET(100, &mut readfds);
            libc::FD_SET(5, &mut writefds);
        }
        let event = SelectEvent {
            result: Err(Errno::EFAULT),
            fd_sets: [Some(vec![0b0100_0000, 0, 0, 0, 0, 0, 0, 0]), None, None],
            timeout: None,
        };
        let result = replay_select_event(
            &mut LocalMemory::new(),
            128,
            [
                AddrMut::from_raw((&mut readfds as *mut libc::fd_set) as usize),
                AddrMut::from_raw((&mut writefds as *mut libc::fd_set) as usize),
                None,
            ],
            None,
            event,
        );

        assert_eq!(result, Err(Errno::EFAULT));
        assert!(!unsafe { libc::FD_ISSET(3, &readfds) });
        assert!(unsafe { libc::FD_ISSET(6, &readfds) });
        // Past the recorded prefix, and the set the kernel never reached,
        // keep the guest's own bytes.
        assert!(unsafe { libc::FD_ISSET(100, &readfds) });
        assert!(unsafe { libc::FD_ISSET(5, &writefds) });
    }
}
