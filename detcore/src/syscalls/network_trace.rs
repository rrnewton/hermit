/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3464): Review external network record and replay.

//! External network record and replay for outbound TCP clients.
//!
//! With a network trace mode configured, an outbound TCP `connect` becomes a
//! channel of the [`NetworkEngine`] held in global state, and every later
//! operation on that socket is answered by the engine:
//!
//! - Record mode connects for real, pulls whatever the host has received with
//!   a nonblocking `recv` at each guest check, and hands it to the engine,
//!   which stamps it with the global time of that check.
//! - Replay mode never touches the host network. The engine releases each
//!   recorded input once global time and the guest's own transmissions reach
//!   the recorded point, and compares every outbound byte with the recording.
//!
//! A pull and the check it serves travel in one request, so the engine
//! observes both at the same global time. Blocking operations retry on
//! ordinary polling turns, which advance global time, so the release point of
//! every input is reached in replay under any schedule.
//!
//! Host waits that the guest cannot observe as time passing -- the connect
//! handshake and a full send buffer -- block the thread without yielding, so
//! record and replay take the same scheduler turns.
//!
//! Anything the engine cannot answer ends the run with the policy-refusal
//! status, naming the reason; replay never falls back to the host.

use std::os::unix::io::RawFd;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use detcore_model::network_engine::NetworkArrival;
use detcore_model::network_engine::NetworkRecvOutcome;
use detcore_model::network_engine::NetworkReply;
use detcore_model::network_engine::NetworkRequest;
use detcore_model::network_trace::NetworkAddressV1;
use detcore_model::network_trace::NetworkTraceMode;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::record_retry_event;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::ResumeStatus;
use crate::tool_global::resource_request;
use crate::tool_global::send_and_update_time;
use crate::tool_global::thread_observe_time;
use crate::tool_global::unrecoverable_shutdown;
use crate::tool_local::Detcore;

/// How long record mode waits for the host to finish a TCP handshake.
const CONNECT_WAIT_MILLIS: i32 = 60_000;

/// How long record mode waits for a full host send buffer to drain.
const SEND_WAIT_MILLIS: i32 = 60_000;

/// Size of the scratch buffer a readiness check pulls into.
const POLL_PULL_BYTES: usize = 512;

/// `recv` flags with a modelled meaning.
const RECV_FLAGS: i32 =
    libc::MSG_PEEK | libc::MSG_WAITALL | libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;

/// `send` flags with a modelled meaning.
const SEND_FLAGS: i32 = libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT | libc::MSG_MORE;

/// The traced address of an IPv4 or IPv6 `sockaddr`, or `None` for any other
/// family or a short buffer.
fn address_from_sockaddr(bytes: &[u8]) -> Option<NetworkAddressV1> {
    let family = u16::from_ne_bytes(bytes.get(..2)?.try_into().ok()?);
    if family == libc::AF_INET as u16 && bytes.len() >= size_of::<libc::sockaddr_in>() {
        Some(NetworkAddressV1::Inet4 {
            port: u16::from_be_bytes(bytes[2..4].try_into().ok()?),
            address: bytes[4..8].try_into().ok()?,
        })
    } else if family == libc::AF_INET6 as u16 && bytes.len() >= size_of::<libc::sockaddr_in6>() {
        Some(NetworkAddressV1::Inet6 {
            port: u16::from_be_bytes(bytes[2..4].try_into().ok()?),
            flowinfo: u32::from_be_bytes(bytes[4..8].try_into().ok()?),
            address: bytes[8..24].try_into().ok()?,
            scope_id: u32::from_ne_bytes(bytes[24..28].try_into().ok()?),
        })
    } else {
        None
    }
}

/// The `sockaddr` bytes Linux reports for `address`.
fn sockaddr_bytes(address: &NetworkAddressV1) -> Vec<u8> {
    match address {
        NetworkAddressV1::Inet4 { address, port } => {
            let mut bytes = vec![0; size_of::<libc::sockaddr_in>()];
            bytes[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
            bytes[2..4].copy_from_slice(&port.to_be_bytes());
            bytes[4..8].copy_from_slice(address);
            bytes
        }
        NetworkAddressV1::Inet6 {
            address,
            port,
            flowinfo,
            scope_id,
        } => {
            let mut bytes = vec![0; size_of::<libc::sockaddr_in6>()];
            bytes[..2].copy_from_slice(&(libc::AF_INET6 as u16).to_ne_bytes());
            bytes[2..4].copy_from_slice(&port.to_be_bytes());
            bytes[4..8].copy_from_slice(&flowinfo.to_be_bytes());
            bytes[8..24].copy_from_slice(address);
            bytes[24..28].copy_from_slice(&scope_id.to_ne_bytes());
            bytes
        }
    }
}

/// The unspecified address of `address`'s family, as `getsockname` reports
/// for a socket whose connect failed.
fn unspecified_like(address: &NetworkAddressV1) -> NetworkAddressV1 {
    match address {
        NetworkAddressV1::Inet4 { .. } => NetworkAddressV1::Inet4 {
            address: [0; 4],
            port: 0,
        },
        NetworkAddressV1::Inet6 { .. } => NetworkAddressV1::Inet6 {
            address: [0; 16],
            port: 0,
            flowinfo: 0,
            scope_id: 0,
        },
    }
}

/// `SO_RCVLOWAT` as Linux stores it: negative means `INT_MAX`, zero means one.
fn normalized_lowat(value: i32) -> usize {
    if value < 0 {
        i32::MAX as usize
    } else {
        value.max(1) as usize
    }
}

/// The `poll` result for one descriptor: what the socket reports, limited to
/// the requested events plus the ones Linux always reports.
fn masked_revents(readiness: i16, requested: i16) -> i16 {
    readiness & (requested | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
}

/// Classify a nonblocking host `recv` into what the recorder saw.
fn arrival_from_pull(result: Result<i64, Errno>, buffer: &[u8]) -> Option<NetworkArrival> {
    match result {
        Ok(0) => Some(NetworkArrival::PeerWriteClosed),
        Ok(received) => Some(NetworkArrival::Bytes(buffer[..received as usize].to_vec())),
        Err(Errno::EAGAIN | Errno::EINTR) => None,
        Err(errno) => Some(NetworkArrival::Error(errno.into_raw())),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    fn network_mode(&self) -> NetworkTraceMode {
        self.cfg.network_trace.mode
    }

    fn is_network_channel<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> bool {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.is_network_channel())
            .unwrap_or(false)
    }

    /// Whether the network trace handles `call`. Always false with no trace
    /// mode, so ordinary runs never reach this module.
    pub(crate) fn network_trace_owns<G: Guest<Self>>(&self, guest: &mut G, call: &Syscall) -> bool {
        if self.network_mode() == NetworkTraceMode::Off {
            return false;
        }
        let fd = match call {
            // A connect may create a channel and a poll may name one; both
            // fall back to their ordinary handlers otherwise.
            Syscall::Connect(_) | Syscall::Poll(_) => return true,
            // The receive low-water mark may be set before the socket connects.
            Syscall::Setsockopt(c) => {
                if c.level() == libc::SOL_SOCKET && c.optname() == libc::SO_RCVLOWAT {
                    return true;
                }
                c.fd()
            }
            Syscall::Read(c) => c.fd(),
            Syscall::Write(c) => c.fd(),
            Syscall::Recvfrom(c) => c.fd(),
            Syscall::Sendto(c) => c.fd(),
            Syscall::Shutdown(c) => c.fd(),
            Syscall::Getsockname(c) => c.fd(),
            Syscall::Getpeername(c) => c.fd(),
            Syscall::Getsockopt(c) => c.fd(),
            Syscall::Readv(c) => c.fd(),
            Syscall::Writev(c) => c.fd(),
            Syscall::Recvmsg(c) => c.sockfd(),
            Syscall::Sendmsg(c) => c.fd(),
            Syscall::Recvmmsg(c) => c.fd(),
            Syscall::Sendmmsg(c) => c.sockfd(),
            Syscall::Ioctl(c) => c.fd(),
            Syscall::EpollCtl(c) => c.fd(),
            _ => return false,
        };
        Self::is_network_channel(guest, fd)
    }

    /// Handle a call that [`Self::network_trace_owns`] accepted.
    pub(crate) async fn handle_network_trace_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        match call {
            Syscall::Connect(c) => self.network_connect(guest, c).await,
            Syscall::Poll(c) => self.network_poll(guest, c).await,
            Syscall::Setsockopt(c) => self.network_setsockopt(guest, c).await,
            Syscall::Read(c) => {
                self.network_recv(
                    guest,
                    c.fd(),
                    c.buf(),
                    c.len(),
                    0,
                    c.signal_interrupt_errno(),
                )
                .await
            }
            Syscall::Recvfrom(c) => {
                // A connected TCP socket reports no source address.
                if let Some(length) = c.addr_len() {
                    guest.memory().write_value(length, &0u32)?;
                }
                self.network_recv(
                    guest,
                    c.fd(),
                    c.buf(),
                    c.len(),
                    c.flags(),
                    c.signal_interrupt_errno(),
                )
                .await
            }
            Syscall::Write(c) => self.network_send(guest, c.fd(), c.buf(), c.len(), 0).await,
            Syscall::Sendto(c) => {
                // A connected TCP socket ignores the destination address.
                let flags = c.flags() as i32;
                self.network_send(
                    guest,
                    c.fd(),
                    c.buf().map(|buf| Addr::from(buf.cast::<u8>())),
                    c.size(),
                    flags,
                )
                .await
            }
            Syscall::Shutdown(c) => self.network_shutdown(guest, c).await,
            Syscall::Getsockname(c) => {
                self.network_address(guest, c.fd(), c.usockaddr(), c.usockaddr_len(), true)
                    .await
            }
            Syscall::Getpeername(c) => {
                self.network_address(guest, c.fd(), c.usockaddr(), c.usockaddr_len(), false)
                    .await
            }
            Syscall::Getsockopt(c)
                if c.level() == libc::SOL_SOCKET && c.optname() == libc::SO_ERROR =>
            {
                self.network_so_error(guest, c).await
            }
            // Options other than the pending error describe the socket rather
            // than the connection; the host socket answers them in both modes.
            Syscall::Getsockopt(c) => self.handle_getsockopt(guest, c).await,
            other => {
                self.network_refuse(
                    guest,
                    &format!(
                        "network trace does not model {} on a recorded socket",
                        other.name()
                    ),
                )
                .await
            }
        }
    }

    /// End the run: the engine cannot answer this operation faithfully.
    async fn network_refuse<G: Guest<Self>>(&self, guest: &mut G, reason: &str) -> ! {
        eprintln!("hermit: {reason}");
        unrecoverable_shutdown(guest, HERMIT_POLICY_REFUSAL_EXIT).await
    }

    /// Perform one engine operation at the current global time.
    async fn network_request<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: NetworkRequest,
    ) -> NetworkReply {
        let reply = match send_and_update_time(guest, GlobalRequest::Network(request))
            .await
            .1
        {
            GlobalResponse::Network(reply) => reply,
            _ => unreachable!(),
        };
        match reply {
            Ok(reply) => reply,
            Err(error) => self.network_refuse(guest, &error.to_string()).await,
        }
    }

    fn network_id<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> detcore_model::fd::OpenFileId {
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.open_file_id())
            .expect("network channel descriptor is tracked")
    }

    /// Wait in the host, without yielding, for `events` on `fd`. Returns the
    /// reported events, or `None` on timeout.
    async fn network_host_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        events: i16,
        timeout_millis: i32,
    ) -> Result<Option<i16>, Error> {
        let mut stack = guest.stack().await;
        let pollfd: AddrMut<libc::pollfd> = stack.reserve();
        let _guard = stack.commit()?;
        guest.memory().write_value(
            pollfd,
            &libc::pollfd {
                fd,
                events,
                revents: 0,
            },
        )?;
        loop {
            let call = syscalls::Poll::new()
                .with_fds(Some(pollfd.cast()))
                .with_nfds(1)
                .with_timeout(timeout_millis);
            match guest.inject(call).await {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    let pollfd: libc::pollfd = guest.memory().read_value(pollfd)?;
                    return Ok(Some(pollfd.revents));
                }
                Err(Errno::EINTR) => continue,
                Err(errno) => return Err(errno.into()),
            }
        }
    }

    /// Read an `int` socket option from the host socket.
    async fn network_host_int_option<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        name: i32,
    ) -> Result<i32, Errno> {
        let mut stack = guest.stack().await;
        let value: AddrMut<i32> = stack.reserve();
        let length = stack.push(size_of::<i32>() as libc::socklen_t);
        let _guard = stack.commit()?;
        let call = syscalls::Getsockopt::new()
            .with_fd(fd)
            .with_level(libc::SOL_SOCKET)
            .with_optname(name)
            .with_optval(Some(value.cast()))
            .with_optlen(Some(
                AddrMut::from_raw(length.as_raw()).ok_or(Errno::EFAULT)?,
            ));
        guest.inject(call).await?;
        guest.memory().read_value(value)
    }

    /// The local address of the host socket.
    async fn network_host_local_address<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<NetworkAddressV1>, Errno> {
        let mut stack = guest.stack().await;
        let address: AddrMut<libc::sockaddr_in6> = stack.reserve();
        let length = stack.push(size_of::<libc::sockaddr_in6>() as libc::socklen_t);
        let _guard = stack.commit()?;
        let length = AddrMut::<libc::socklen_t>::from_raw(length.as_raw()).ok_or(Errno::EFAULT)?;
        let call = syscalls::Getsockname::new()
            .with_fd(fd)
            .with_usockaddr(Some(address.cast()))
            .with_usockaddr_len(Some(length));
        guest.inject(call).await?;
        let length: libc::socklen_t = guest.memory().read_value(length)?;
        let mut bytes = vec![0; (length as usize).min(size_of::<libc::sockaddr_in6>())];
        guest.memory().read_exact(address.cast(), &mut bytes)?;
        Ok(address_from_sockaddr(&bytes))
    }

    /// One nonblocking host receive into `buffer`, as the recorder sees it.
    async fn network_pull<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: AddrMut<'_, u8>,
        len: usize,
    ) -> Result<Option<NetworkArrival>, Error> {
        let call = syscalls::Recvfrom::new()
            .with_fd(fd)
            .with_buf(Some(buffer))
            .with_len(len)
            .with_flags(libc::MSG_DONTWAIT);
        let result = guest.inject(call).await;
        let mut bytes = Vec::new();
        if let Ok(received) = result {
            bytes.resize(received as usize, 0);
            guest.memory().read_exact(buffer, &mut bytes)?;
        }
        Ok(arrival_from_pull(result, &bytes))
    }

    /// `connect`: an outbound TCP connect to an IPv4 or IPv6 peer becomes a
    /// channel; any other connect takes the ordinary path.
    async fn network_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let socket = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                (detfd.ty() == FdType::Socket && detfd.open_file_id().is_socket()).then(|| {
                    (
                        detfd.open_file_id(),
                        detfd.is_nonblocking(),
                        detfd.is_network_channel(),
                    )
                })
            })
            .ok()
            .flatten();
        let Some((id, nonblocking, already_channel)) = socket else {
            return self.handle_connect(guest, call).await;
        };
        if already_channel {
            // Linux reports a finished connect's error once, then EISCONN.
            let NetworkReply::Errno(errno) = self
                .network_request(guest, NetworkRequest::TakeError(id))
                .await
            else {
                unreachable!()
            };
            return Err(Errno::new(if errno == 0 { libc::EISCONN } else { errno }).into());
        }
        let peer = (|| {
            let address = call.uservaddr()?;
            let length = usize::try_from(call.addrlen()).ok()?;
            let mut bytes = vec![0; length.min(size_of::<libc::sockaddr_in6>())];
            guest.memory().read_exact(address.cast(), &mut bytes).ok()?;
            address_from_sockaddr(&bytes)
        })();
        let Some(peer) = peer else {
            return self.handle_connect(guest, call).await;
        };
        if self
            .network_host_int_option(guest, fd, libc::SO_PROTOCOL)
            .await
            != Ok(libc::IPPROTO_TCP)
        {
            return self.handle_connect(guest, call).await;
        }

        let request = if self.network_mode() == NetworkTraceMode::Record {
            let errno = match guest.inject(call).await {
                Ok(_) => 0,
                Err(Errno::EINPROGRESS) => {
                    if self
                        .network_host_wait(guest, fd, libc::POLLOUT, CONNECT_WAIT_MILLIS)
                        .await?
                        .is_none()
                    {
                        self.network_refuse(guest, "network connect did not finish in time")
                            .await
                    }
                    self.network_host_int_option(guest, fd, libc::SO_ERROR)
                        .await?
                }
                // Argument errors precede any network effect and need no record.
                Err(
                    errno @ (Errno::EBADF
                    | Errno::EFAULT
                    | Errno::EINVAL
                    | Errno::EAFNOSUPPORT
                    | Errno::EALREADY
                    | Errno::EISCONN),
                ) => return Err(errno.into()),
                Err(errno) => errno.into_raw(),
            };
            let local = if errno == 0 {
                match self.network_host_local_address(guest, fd).await? {
                    Some(local) => Some(local),
                    None => {
                        self.network_refuse(guest, "network connect has no local address")
                            .await
                    }
                }
            } else {
                None
            };
            NetworkRequest::RecordConnect {
                id,
                peer,
                local,
                errno,
                nonblocking,
            }
        } else {
            NetworkRequest::ReplayConnect {
                id,
                peer,
                nonblocking,
            }
        };
        let NetworkReply::Errno(errno) = self.network_request(guest, request).await else {
            unreachable!()
        };
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.set_network_channel())?;
        if errno == 0 {
            Ok(0)
        } else {
            Err(Errno::new(errno).into())
        }
    }

    /// `setsockopt`: track `SO_RCVLOWAT`, which the engine needs for receive
    /// targets and readiness. The host socket applies every option.
    async fn network_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsockopt,
    ) -> Result<i64, Error> {
        let result = self.handle_setsockopt(guest, call).await?;
        if call.level() == libc::SOL_SOCKET
            && call.optname() == libc::SO_RCVLOWAT
            && let Some(value) = call.optval()
        {
            let value: i32 = guest.memory().read_value(value.cast())?;
            let _ = guest.thread_state().with_detfd(call.fd(), |detfd| {
                detfd.set_network_lowat(normalized_lowat(value))
            });
        }
        Ok(result)
    }

    /// `recv`, `recvfrom` and `read` on a channel.
    async fn network_recv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<AddrMut<'_, u8>>,
        len: usize,
        flags: i32,
        interrupted: Errno,
    ) -> Result<i64, Error> {
        if flags & !RECV_FLAGS != 0 {
            self.network_refuse(
                guest,
                &format!("network trace does not model recv flags {flags:#x}"),
            )
            .await
        }
        if len == 0 {
            return Ok(0);
        }
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        let (id, logically_nonblocking, lowat) = guest.thread_state().with_detfd(fd, |detfd| {
            (
                detfd.open_file_id(),
                detfd.is_nonblocking(),
                detfd.network_lowat(),
            )
        })?;
        let nonblocking = logically_nonblocking || flags & libc::MSG_DONTWAIT != 0;
        let target = if nonblocking {
            1
        } else if flags & libc::MSG_WAITALL != 0 {
            len
        } else {
            lowat.min(len)
        };
        let record = self.network_mode() == NetworkTraceMode::Record;
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("network recv");
        loop {
            if !nonblocking
                && matches!(
                    resource_request(guest, rsrc.clone()).await,
                    ResumeStatus::Signaled(_)
                )
            {
                return Err(interrupted.into());
            }
            let arrivals = if record {
                self.network_pull(guest, fd, buffer, len)
                    .await?
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            let request = NetworkRequest::Recv {
                id,
                arrivals,
                max_len: len,
                target,
                peek: flags & libc::MSG_PEEK != 0,
            };
            let NetworkReply::Recv(outcome) = self.network_request(guest, request).await else {
                unreachable!()
            };
            match outcome {
                NetworkRecvOutcome::Data(bytes) => {
                    guest
                        .memory()
                        .write_exact(buffer, &bytes)
                        .map_err(|_| Errno::EFAULT)?;
                    return Ok(bytes.len() as i64);
                }
                NetworkRecvOutcome::Eof => return Ok(0),
                NetworkRecvOutcome::Error(errno) => return Err(Errno::new(errno).into()),
                NetworkRecvOutcome::WouldBlock if nonblocking => {
                    return Err(Errno::EAGAIN.into());
                }
                NetworkRecvOutcome::WouldBlock => {
                    rsrc.poll_attempt += 1;
                    record_retry_event(guest, syscalls::Recvfrom::new().with_fd(fd)).await;
                }
            }
        }
    }

    /// `send`, `sendto` and `write` on a channel.
    async fn network_send<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffer: Option<Addr<'_, u8>>,
        len: usize,
        flags: i32,
    ) -> Result<i64, Error> {
        if flags & !SEND_FLAGS != 0 {
            self.network_refuse(
                guest,
                &format!("network trace does not model send flags {flags:#x}"),
            )
            .await
        }
        let (id, logically_nonblocking) = guest
            .thread_state()
            .with_detfd(fd, |detfd| (detfd.open_file_id(), detfd.is_nonblocking()))?;
        let mut bytes = vec![0; len];
        if len != 0 {
            guest
                .memory()
                .read_exact(buffer.ok_or(Errno::EFAULT)?, &mut bytes)
                .map_err(|_| Errno::EFAULT)?;
        }
        let NetworkReply::Failure(failure) = self
            .network_request(guest, NetworkRequest::SendFailure(id))
            .await
        else {
            unreachable!()
        };
        if let Some(errno) = failure {
            if errno == libc::EPIPE && flags & libc::MSG_NOSIGNAL == 0 {
                self.network_refuse(
                    guest,
                    "network trace does not model SIGPIPE; send with MSG_NOSIGNAL",
                )
                .await
            }
            return Err(Errno::new(errno).into());
        }
        if len == 0 {
            return Ok(0);
        }
        let nonblocking = logically_nonblocking || flags & libc::MSG_DONTWAIT != 0;
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        let mut sent = 0;
        if self.network_mode() == NetworkTraceMode::Record {
            // Never report EAGAIN: replay could not reproduce it. A nonblocking
            // send waits for some progress, a blocking one for all of it.
            while sent < len && !(nonblocking && sent > 0) {
                let call = syscalls::Sendto::new()
                    .with_fd(fd)
                    // SAFETY: `sent < len` stays within the guest's buffer.
                    .with_buf(AddrMut::from_raw(unsafe { buffer.add(sent) }.as_raw()))
                    .with_size(len - sent)
                    .with_flags(
                        (flags & libc::MSG_MORE | libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u32,
                    );
                match guest.inject(call).await {
                    Ok(written) => sent += written as usize,
                    Err(Errno::EINTR) => {}
                    Err(Errno::EAGAIN) => {
                        if self
                            .network_host_wait(guest, fd, libc::POLLOUT, SEND_WAIT_MILLIS)
                            .await?
                            .is_none()
                        {
                            self.network_refuse(guest, "network send did not drain in time")
                                .await
                        }
                    }
                    Err(errno) => {
                        self.network_refuse(
                            guest,
                            &format!("network send failed in the host with {errno}"),
                        )
                        .await
                    }
                }
            }
            let request = NetworkRequest::RecordSend {
                id,
                bytes: bytes[..sent].to_vec(),
            };
            self.network_request(guest, request).await;
        } else {
            while sent < len && !(nonblocking && sent > 0) {
                let request = NetworkRequest::ReplaySend {
                    id,
                    bytes: bytes[sent..].to_vec(),
                };
                let NetworkReply::Sent(accepted) = self.network_request(guest, request).await
                else {
                    unreachable!()
                };
                sent += accepted;
            }
        }
        Ok(sent as i64)
    }

    /// `poll` over channels only; a poll naming no channel takes the
    /// ordinary path.
    async fn network_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let nfds = call.nfds() as usize;
        let mut pollfds = vec![
            libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0,
            };
            nfds
        ];
        let Some(address) = call.fds().map(|fds| fds.cast::<libc::pollfd>()) else {
            return self.handle_poll(guest, call).await;
        };
        if guest
            .memory()
            .read_values(address.into(), &mut pollfds)
            .is_err()
        {
            return self.handle_poll(guest, call).await;
        }
        let mut channels = Vec::new();
        let mut others = false;
        for pollfd in &pollfds {
            if pollfd.fd < 0 {
                channels.push(None);
            } else if Self::is_network_channel(guest, pollfd.fd) {
                let (id, lowat) = guest.thread_state().with_detfd(pollfd.fd, |detfd| {
                    (detfd.open_file_id(), detfd.network_lowat())
                })?;
                channels.push(Some((id, lowat)));
            } else {
                others = true;
                channels.push(None);
            }
        }
        if channels.iter().all(Option::is_none) {
            return self.handle_poll(guest, call).await;
        }
        if others {
            self.network_refuse(
                guest,
                "network trace does not model a poll mixing recorded sockets with other descriptors",
            )
            .await
        }

        let deadline = millis_duration_to_absolute_timeout(guest, call.timeout()).await;
        let record = self.network_mode() == NetworkTraceMode::Record;
        let mut stack = guest.stack().await;
        let scratch: AddrMut<[u8; POLL_PULL_BYTES]> = stack.reserve();
        let _guard = stack.commit()?;
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("network poll");
        loop {
            if matches!(
                resource_request(guest, rsrc.clone()).await,
                ResumeStatus::Signaled(_)
            ) {
                return Err(call.signal_interrupt_errno().into());
            }
            let mut request = Vec::new();
            for (pollfd, channel) in pollfds.iter().zip(&channels) {
                if let Some((id, lowat)) = channel {
                    let arrivals = if record {
                        self.network_pull(guest, pollfd.fd, scratch.cast(), POLL_PULL_BYTES)
                            .await?
                            .into_iter()
                            .collect()
                    } else {
                        Vec::new()
                    };
                    request.push((*id, *lowat, arrivals));
                }
            }
            let NetworkReply::Events(events) = self
                .network_request(guest, NetworkRequest::Readiness(request))
                .await
            else {
                unreachable!()
            };
            let mut events = events.into_iter();
            let mut ready = 0;
            for (pollfd, channel) in pollfds.iter_mut().zip(&channels) {
                pollfd.revents = match channel {
                    Some(_) => masked_revents(events.next().unwrap(), pollfd.events),
                    None => 0,
                };
                if pollfd.revents != 0 {
                    ready += 1;
                }
            }
            let expired = match deadline {
                Some(deadline) => thread_observe_time(guest).await >= deadline,
                None => call.timeout() == 0,
            };
            if ready > 0 || expired {
                guest
                    .memory()
                    .write_values(address, &pollfds)
                    .map_err(|_| Errno::EFAULT)?;
                return Ok(ready);
            }
            rsrc.poll_attempt += 1;
            record_retry_event(guest, call).await;
        }
    }

    /// `shutdown` on a channel; record mode also shuts the host socket so
    /// the peer sees it.
    async fn network_shutdown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Shutdown,
    ) -> Result<i64, Error> {
        let id = Self::network_id(guest, call.fd());
        if self.network_mode() == NetworkTraceMode::Record {
            let _ = guest.inject(call).await;
        }
        let NetworkReply::Errno(errno) = self
            .network_request(
                guest,
                NetworkRequest::Shutdown {
                    id,
                    how: call.how(),
                },
            )
            .await
        else {
            unreachable!()
        };
        if errno == 0 {
            Ok(0)
        } else {
            Err(Errno::new(errno).into())
        }
    }

    /// `getsockname` (`local`) or `getpeername` on a channel.
    async fn network_address<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        address: Option<AddrMut<'_, libc::sockaddr>>,
        length: Option<AddrMut<'_, libc::socklen_t>>,
        local: bool,
    ) -> Result<i64, Error> {
        let id = Self::network_id(guest, fd);
        let NetworkReply::Addresses(addresses) = self
            .network_request(guest, NetworkRequest::Addresses(id))
            .await
        else {
            unreachable!()
        };
        let (recorded_local, peer) = addresses.expect("network channel has addresses");
        let reported = match (local, recorded_local) {
            (true, Some(recorded_local)) => recorded_local,
            (true, None) => unspecified_like(&peer),
            (false, Some(_)) => peer,
            (false, None) => return Err(Errno::ENOTCONN.into()),
        };
        let bytes = sockaddr_bytes(&reported);
        let length = length.ok_or(Errno::EFAULT)?;
        let capacity: libc::socklen_t = guest.memory().read_value(length)?;
        if (capacity as i32) < 0 {
            return Err(Errno::EINVAL.into());
        }
        let copied = bytes.len().min(capacity as usize);
        if copied != 0 {
            guest
                .memory()
                .write_exact(address.ok_or(Errno::EFAULT)?.cast(), &bytes[..copied])
                .map_err(|_| Errno::EFAULT)?;
        }
        guest
            .memory()
            .write_value(length, &(bytes.len() as libc::socklen_t))?;
        Ok(0)
    }

    /// `getsockopt(SO_ERROR)` on a channel consumes the engine's pending error.
    async fn network_so_error<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockopt,
    ) -> Result<i64, Error> {
        let length = call.optlen().ok_or(Errno::EFAULT)?;
        let capacity: libc::socklen_t = guest.memory().read_value(length)?;
        if (capacity as i32) < 0 {
            return Err(Errno::EINVAL.into());
        }
        let id = Self::network_id(guest, call.fd());
        let NetworkReply::Errno(errno) = self
            .network_request(guest, NetworkRequest::TakeError(id))
            .await
        else {
            unreachable!()
        };
        let bytes = errno.to_ne_bytes();
        let copied = bytes.len().min(capacity as usize);
        if copied != 0 {
            guest
                .memory()
                .write_exact(call.optval().ok_or(Errno::EFAULT)?.cast(), &bytes[..copied])
                .map_err(|_| Errno::EFAULT)?;
        }
        guest
            .memory()
            .write_value(length, &(copied as libc::socklen_t))?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_round_trips_both_families() {
        let v4 = NetworkAddressV1::Inet4 {
            address: [192, 0, 2, 7],
            port: 8080,
        };
        let v6 = NetworkAddressV1::Inet6 {
            address: [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            port: 443,
            flowinfo: 5,
            scope_id: 2,
        };
        for address in [v4, v6] {
            assert_eq!(
                address_from_sockaddr(&sockaddr_bytes(&address)),
                Some(address)
            );
        }
    }

    #[test]
    fn sockaddr_matches_the_libc_layout() {
        let bytes = sockaddr_bytes(&NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 0x1234,
        });
        // SAFETY: `bytes` is exactly one `sockaddr_in`.
        let native: libc::sockaddr_in = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast()) };
        assert_eq!(native.sin_family, libc::AF_INET as libc::sa_family_t);
        assert_eq!(u16::from_be(native.sin_port), 0x1234);
        assert_eq!(native.sin_addr.s_addr.to_ne_bytes(), [127, 0, 0, 1]);
    }

    #[test]
    fn other_families_and_short_buffers_are_not_traced() {
        let mut unix = vec![0u8; size_of::<libc::sockaddr_un>()];
        unix[..2].copy_from_slice(&(libc::AF_UNIX as u16).to_ne_bytes());
        assert_eq!(address_from_sockaddr(&unix), None);
        let v4 = sockaddr_bytes(&NetworkAddressV1::Inet4 {
            address: [192, 0, 2, 7],
            port: 1,
        });
        assert_eq!(address_from_sockaddr(&v4[..v4.len() - 1]), None);
        assert_eq!(address_from_sockaddr(&[]), None);
    }

    #[test]
    fn lowat_normalisation_follows_linux() {
        assert_eq!(normalized_lowat(-1), i32::MAX as usize);
        assert_eq!(normalized_lowat(0), 1);
        assert_eq!(normalized_lowat(3), 3);
    }

    #[test]
    fn poll_reports_requested_events_and_the_unmaskable_ones() {
        let readiness = libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT | libc::POLLWRNORM;
        assert_eq!(masked_revents(readiness, libc::POLLIN), libc::POLLIN);
        assert_eq!(
            masked_revents(libc::POLLOUT | libc::POLLHUP | libc::POLLERR, libc::POLLIN),
            libc::POLLHUP | libc::POLLERR
        );
    }

    #[test]
    fn host_pulls_classify_like_the_recorder() {
        assert_eq!(arrival_from_pull(Err(Errno::EAGAIN), &[]), None);
        assert_eq!(
            arrival_from_pull(Ok(0), &[]),
            Some(NetworkArrival::PeerWriteClosed)
        );
        assert_eq!(
            arrival_from_pull(Ok(2), b"abc"),
            Some(NetworkArrival::Bytes(b"ab".to_vec()))
        );
        assert_eq!(
            arrival_from_pull(Err(Errno::ECONNRESET), &[]),
            Some(NetworkArrival::Error(libc::ECONNRESET))
        );
    }
}
