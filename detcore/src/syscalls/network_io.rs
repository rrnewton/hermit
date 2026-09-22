/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Single guest-memory adapter for engine-owned network syscalls.

use std::time::Duration;

use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkChannelV2;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkEndpointRoleV2;
use detcore_model::network_trace::NetworkInputEventV2;
use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkPolicy;
use detcore_model::network_trace::NetworkReadinessV2;
use detcore_model::network_trace::NetworkReleaseV2;
use detcore_model::network_trace::NetworkShutdownV2;
use detcore_model::network_trace::NetworkTransportV2;
use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use crate::Detcore;
use crate::fd::FdType;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::NetworkWaitKind;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::tool_global::NetworkCapturedStreamInput;
use crate::tool_global::NetworkCapturedStreamOutput;
use crate::tool_global::NetworkReply;
use crate::tool_global::NetworkRequest;
use crate::tool_global::NetworkStreamReceive;
use crate::tool_global::NetworkStreamTransmit;
use crate::tool_global::network_request;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::types::LogicalTime;
use crate::types::OpenFileId;

fn engine_error(error: impl std::fmt::Display) -> Error {
    Error::Tool(anyhow::anyhow!(
        "shared network engine refused operation: {error}"
    ))
}

fn errno_result(errno: i32) -> Result<i64, Error> {
    Err(Error::Errno(Errno::new(errno)))
}

fn error_errno(error: &Error) -> Option<i32> {
    match error {
        Error::Errno(errno) => Some(errno.into_raw()),
        Error::Tool(_) | Error::Io(_) => None,
    }
}

fn shutdown_direction(how: i32) -> Result<NetworkShutdownV2, Error> {
    match how {
        libc::SHUT_RD => Ok(NetworkShutdownV2::Read),
        libc::SHUT_WR => Ok(NetworkShutdownV2::Write),
        libc::SHUT_RDWR => Ok(NetworkShutdownV2::Both),
        _ => Err(Error::Errno(Errno::EINVAL)),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Classify engine-owned calls before the main syscall dispatcher moves the
    /// typed value. Descriptor-wide calls are owned only for socket OFDs.
    pub(crate) fn network_io_owns<G: Guest<Self>>(&self, guest: &mut G, call: Syscall) -> bool {
        if !matches!(
            guest.config().network_trace.policy,
            NetworkPolicy::Record | NetworkPolicy::Replay
        ) {
            return false;
        }
        match call {
            Syscall::Socket(_) | Syscall::Connect(_) => true,
            Syscall::Listen(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Accept(call) => self.network_open_file(guest, call.sockfd()).is_some(),
            Syscall::Accept4(call) => self.network_open_file(guest, call.sockfd()).is_some(),
            Syscall::Read(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Write(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Readv(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Writev(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Recvfrom(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Sendto(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Recvmsg(call) => self.network_open_file(guest, call.sockfd()).is_some(),
            Syscall::Sendmsg(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Shutdown(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Poll(call) => self.poll_array_has_network_fd(
                guest,
                call.fds().map(|address| address.cast()),
                call.nfds(),
            ),
            Syscall::Ppoll(call) => self.poll_array_has_network_fd(guest, call.fds(), call.nfds()),
            Syscall::Select(call) => self.select_has_network_fd(
                guest,
                call.nfds(),
                call.readfds(),
                call.writefds(),
                call.exceptfds(),
            ),
            Syscall::Pselect6(call) => self.select_has_network_fd(
                guest,
                call.nfds(),
                call.readfds(),
                call.writefds(),
                call.exceptfds(),
            ),
            _ => false,
        }
    }

    /// Execute one call already classified by [`Self::network_io_owns`].
    pub(crate) async fn handle_network_io<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        self.try_handle_network_io(guest, call)
            .await
            .expect("network ownership classification drifted from adapter dispatch")
    }

    /// Route every currently supported engine-owned shape through this one
    /// adapter. `None` means the syscall is not an external socket operation.
    pub(crate) async fn try_handle_network_io<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Option<Result<i64, Error>> {
        let policy = guest.config().network_trace.policy;
        if !matches!(policy, NetworkPolicy::Record | NetworkPolicy::Replay) {
            return None;
        }

        match call {
            Syscall::Socket(call) => Some(self.network_socket(guest, call).await),
            Syscall::Connect(call) => Some(self.network_connect(guest, call, policy).await),
            Syscall::Listen(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_listen(guest, call, policy).await)
            }
            Syscall::Accept(call) if self.network_open_file(guest, call.sockfd()).is_some() => {
                Some(self.network_accept4(guest, call.into(), policy).await)
            }
            Syscall::Accept4(call) if self.network_open_file(guest, call.sockfd()).is_some() => {
                Some(self.network_accept4(guest, call, policy).await)
            }
            Syscall::Read(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_read(guest, call, policy).await)
            }
            Syscall::Write(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_write(guest, call, policy).await)
            }
            Syscall::Recvfrom(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_recvfrom(guest, call, policy).await)
            }
            Syscall::Sendto(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_sendto(guest, call, policy).await)
            }
            Syscall::Shutdown(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_shutdown(guest, call, policy).await)
            }
            Syscall::Poll(call)
                if self.poll_array_has_network_fd(
                    guest,
                    call.fds().map(|address| address.cast()),
                    call.nfds(),
                ) =>
            {
                Some(self.network_poll(guest, call, policy).await)
            }
            Syscall::Ppoll(call)
                if self.poll_array_has_network_fd(guest, call.fds(), call.nfds()) =>
            {
                Some(self.network_ppoll(guest, call, policy).await)
            }
            Syscall::Select(call)
                if self.select_has_network_fd(
                    guest,
                    call.nfds(),
                    call.readfds(),
                    call.writefds(),
                    call.exceptfds(),
                ) =>
            {
                Some(self.network_select(guest, call, policy).await)
            }
            Syscall::Pselect6(call)
                if self.select_has_network_fd(
                    guest,
                    call.nfds(),
                    call.readfds(),
                    call.writefds(),
                    call.exceptfds(),
                ) =>
            {
                Some(self.network_pselect(guest, call, policy).await)
            }
            Syscall::Readv(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_readv(guest, call, policy).await)
            }
            Syscall::Writev(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(self.network_writev(guest, call, policy).await)
            }
            Syscall::Recvmsg(call) if self.network_open_file(guest, call.sockfd()).is_some() => {
                Some(Err(engine_error(
                    "recvmsg ancillary relocation is not yet implemented",
                )))
            }
            Syscall::Sendmsg(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(Err(engine_error(
                    "sendmsg ancillary relocation is not yet implemented",
                )))
            }
            _ => None,
        }
    }

    fn network_open_file<G: Guest<Self>>(&self, guest: &mut G, fd: i32) -> Option<OpenFileId> {
        guest.thread_state().socket_open_file_id(fd).ok()
    }

    async fn network_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        // Creating the local descriptor is not external communication. Bypass
        // Recorder/Replayer so it cannot create a second per-thread event.
        let fd = guest.inject(call).await? as i32;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.r#type()),
            FdType::Socket,
        )
        .await?;
        Ok(i64::from(fd))
    }

    async fn ensure_stream_channel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        policy: NetworkPolicy,
        peer: NetworkAddressV2,
    ) -> Result<(), Error> {
        let reply = network_request(guest, NetworkRequest::ChannelFor(open_file))
            .await
            .map_err(engine_error)?;
        if matches!(reply, NetworkReply::Channel(Some(_))) {
            return Ok(());
        }
        let channel = NetworkChannelId(open_file.deterministic_socket_cookie());
        if policy == NetworkPolicy::Record {
            let transport = match peer {
                NetworkAddressV2::Inet4 { .. } | NetworkAddressV2::Inet6 { .. } => {
                    NetworkTransportV2::Tcp
                }
                NetworkAddressV2::UnixPath(_)
                | NetworkAddressV2::UnixAbstract(_)
                | NetworkAddressV2::UnixUnnamed => NetworkTransportV2::UnixStream,
            };
            network_request(
                guest,
                NetworkRequest::RecordChannel(NetworkChannelV2 {
                    id: channel,
                    transport,
                    role: NetworkEndpointRoleV2::OutboundClient,
                    local_address: None,
                    peer_address: Some(peer),
                    accepted_from: None,
                }),
            )
            .await
            .map_err(engine_error)?;
        }
        match network_request(guest, NetworkRequest::Bind(open_file, channel))
            .await
            .map_err(engine_error)?
        {
            NetworkReply::Unit => Ok(()),
            reply => Err(engine_error(format!("unexpected bind reply {reply:?}"))),
        }
    }

    async fn network_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let peer = read_network_address(guest, call.uservaddr(), call.addrlen())?;
        self.ensure_stream_channel(guest, open_file, policy, peer)
            .await?;
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, call.into()).await;
                let observed_at = thread_observe_time(guest).await;
                let connection = match &result {
                    Ok(_) => NetworkConnectionResultV2::Connected,
                    Err(error) => NetworkConnectionResultV2::Error(
                        error_errno(error).ok_or_else(|| engine_error(error))?,
                    ),
                };
                network_request(
                    guest,
                    NetworkRequest::CaptureStreamInput {
                        open_file,
                        observed_at,
                        input: NetworkCapturedStreamInput::Connect(connection),
                    },
                )
                .await
                .map_err(engine_error)?;
                result
            }
            NetworkPolicy::Replay => loop {
                let now = thread_observe_time(guest).await;
                network_request(guest, NetworkRequest::ReleaseEligible(now))
                    .await
                    .map_err(engine_error)?;
                match network_request(guest, NetworkRequest::TakeConnectionOutcome(open_file))
                    .await
                    .map_err(engine_error)?
                {
                    NetworkReply::Connection(Some(crate::NetworkConnection::Connect(
                        NetworkConnectionResultV2::Connected,
                    ))) => break Ok(0),
                    NetworkReply::Connection(Some(crate::NetworkConnection::Connect(
                        NetworkConnectionResultV2::Error(errno),
                    ))) => break errno_result(errno),
                    NetworkReply::Connection(None) => {
                        self.wait_for_network(guest, open_file, NetworkWaitKind::Readable)
                            .await;
                    }
                    reply => {
                        break Err(engine_error(format!("unexpected connect reply {reply:?}")));
                    }
                }
            },
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn ensure_listener_channel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        policy: NetworkPolicy,
    ) -> Result<NetworkChannelId, Error> {
        let channel = NetworkChannelId(open_file.deterministic_socket_cookie());
        if matches!(
            network_request(guest, NetworkRequest::ChannelFor(open_file))
                .await
                .map_err(engine_error)?,
            NetworkReply::Channel(Some(_))
        ) {
            return Ok(channel);
        }
        if policy == NetworkPolicy::Record {
            network_request(
                guest,
                NetworkRequest::RecordChannel(NetworkChannelV2 {
                    id: channel,
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::Listener,
                    local_address: None,
                    peer_address: None,
                    accepted_from: None,
                }),
            )
            .await
            .map_err(engine_error)?;
        }
        match network_request(guest, NetworkRequest::Bind(open_file, channel))
            .await
            .map_err(engine_error)?
        {
            NetworkReply::Unit => Ok(channel),
            reply => Err(engine_error(format!(
                "unexpected listener bind reply {reply:?}"
            ))),
        }
    }

    async fn network_listen<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Listen,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        self.ensure_listener_channel(guest, open_file, policy)
            .await?;
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, call.into()).await;
                if result.is_err() {
                    return Err(engine_error(
                        "unsuccessful listen cannot be represented by trace v2",
                    ));
                }
                result
            }
            NetworkPolicy::Replay => Ok(0),
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn network_accept4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let listener = guest.thread_state().socket_open_file_id(call.sockfd())?;
        let listener_channel = self
            .ensure_listener_channel(guest, listener, policy)
            .await?;
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, call.into()).await;
                let fd = match result {
                    Ok(fd) => i32::try_from(fd).map_err(|_| Errno::EIO)?,
                    Err(error) => {
                        let errno = error_errno(&error).ok_or_else(|| engine_error(&error))?;
                        self.capture_input(
                            guest,
                            listener,
                            NetworkCapturedStreamInput::Error(errno),
                        )
                        .await?;
                        return Err(error);
                    }
                };
                self.add_fd(
                    guest,
                    fd,
                    OFlag::from_bits_truncate(call.flags().bits()),
                    FdType::Socket,
                )
                .await?;
                let accepted = guest.thread_state().socket_open_file_id(fd)?;
                let accepted_channel = NetworkChannelId(accepted.deterministic_socket_cookie());
                let peer = read_accept_peer(guest, call)?;
                let transport = match peer.as_ref() {
                    Some(NetworkAddressV2::UnixPath(_))
                    | Some(NetworkAddressV2::UnixAbstract(_))
                    | Some(NetworkAddressV2::UnixUnnamed) => NetworkTransportV2::UnixStream,
                    _ => NetworkTransportV2::Tcp,
                };
                network_request(
                    guest,
                    NetworkRequest::RecordChannel(NetworkChannelV2 {
                        id: accepted_channel,
                        transport,
                        role: NetworkEndpointRoleV2::Accepted,
                        local_address: None,
                        peer_address: peer.clone(),
                        accepted_from: Some(listener_channel),
                    }),
                )
                .await
                .map_err(engine_error)?;
                network_request(guest, NetworkRequest::Bind(accepted, accepted_channel))
                    .await
                    .map_err(engine_error)?;
                let observed_at = thread_observe_time(guest).await;
                network_request(
                    guest,
                    NetworkRequest::RecordInput(NetworkInputEventV2 {
                        ordinal: 0,
                        channel: listener_channel,
                        release: NetworkReleaseV2 {
                            not_before_global_time: observed_at,
                            after_transmitted_offset: 0,
                        },
                        event: NetworkInputKindV2::Accept {
                            accepted: accepted_channel,
                            peer,
                            ancillary: None,
                        },
                    }),
                )
                .await
                .map_err(engine_error)?;
                Ok(i64::from(fd))
            }
            NetworkPolicy::Replay => loop {
                let now = thread_observe_time(guest).await;
                network_request(guest, NetworkRequest::ReleaseEligible(now))
                    .await
                    .map_err(engine_error)?;
                match network_request(guest, NetworkRequest::TakeConnectionOutcome(listener))
                    .await
                    .map_err(engine_error)?
                {
                    NetworkReply::Connection(Some(crate::NetworkConnection::Accept {
                        accepted,
                        peer,
                        ancillary: None,
                    })) => {
                        let family = match peer.as_ref() {
                            Some(NetworkAddressV2::Inet6 { .. }) => libc::AF_INET6,
                            Some(NetworkAddressV2::UnixPath(_))
                            | Some(NetworkAddressV2::UnixAbstract(_))
                            | Some(NetworkAddressV2::UnixUnnamed) => libc::AF_UNIX,
                            _ => libc::AF_INET,
                        };
                        let socket = syscalls::Socket::new()
                            .with_family(family)
                            .with_type(libc::SOCK_STREAM | call.flags().bits())
                            .with_protocol(0);
                        let fd =
                            i32::try_from(guest.inject(socket).await?).map_err(|_| Errno::EIO)?;
                        self.add_fd(
                            guest,
                            fd,
                            OFlag::from_bits_truncate(call.flags().bits()),
                            FdType::Socket,
                        )
                        .await?;
                        let open_file = guest.thread_state().socket_open_file_id(fd)?;
                        match network_request(guest, NetworkRequest::Bind(open_file, accepted))
                            .await
                            .map_err(engine_error)?
                        {
                            NetworkReply::Unit => {}
                            reply => {
                                return Err(engine_error(format!(
                                    "unexpected accepted bind reply {reply:?}"
                                )));
                            }
                        }
                        write_accept_peer(guest, call, peer.as_ref())?;
                        break Ok(i64::from(fd));
                    }
                    NetworkReply::Connection(Some(crate::NetworkConnection::Accept {
                        ancillary: Some(_),
                        ..
                    })) => {
                        break Err(engine_error(
                            "accept ancillary objects require message-level materialization",
                        ));
                    }
                    NetworkReply::Connection(Some(crate::NetworkConnection::Error(errno))) => {
                        break errno_result(errno);
                    }
                    NetworkReply::Connection(None) => {
                        self.wait_for_network(guest, listener, NetworkWaitKind::Readable)
                            .await;
                    }
                    reply => {
                        break Err(engine_error(format!("unexpected accept reply {reply:?}")));
                    }
                }
            },
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn network_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let nonblocking = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.is_nonblocking())?;
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, call.into()).await;
                let input = match &result {
                    Ok(0) => NetworkCapturedStreamInput::EndOfFile,
                    Ok(count) => {
                        let count = usize::try_from(*count).map_err(|_| Errno::EIO)?;
                        let address = call.buf().ok_or(Errno::EFAULT)?;
                        let mut bytes = vec![0; count];
                        guest.memory().read_exact(address, &mut bytes)?;
                        NetworkCapturedStreamInput::Bytes(bytes)
                    }
                    Err(error) => NetworkCapturedStreamInput::Error(
                        error_errno(error).ok_or_else(|| engine_error(error))?,
                    ),
                };
                self.capture_input(guest, open_file, input).await?;
                result
            }
            NetworkPolicy::Replay => {
                let outcome = self
                    .replay_stream_receive(guest, open_file, call.len(), nonblocking)
                    .await?;
                match outcome {
                    NetworkStreamReceive::Bytes(bytes) => {
                        if !bytes.is_empty() {
                            guest
                                .memory()
                                .write_exact(call.buf().ok_or(Errno::EFAULT)?, &bytes)?;
                        }
                        Ok(bytes.len() as i64)
                    }
                    NetworkStreamReceive::EndOfFile => Ok(0),
                    NetworkStreamReceive::Error(errno) => errno_result(errno),
                    NetworkStreamReceive::WouldBlock => Err(Errno::EAGAIN.into()),
                    NetworkStreamReceive::Pending => unreachable!(),
                }
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn network_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Write,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let mut bytes = vec![0; call.len()];
        if !bytes.is_empty() {
            guest
                .memory()
                .read_exact(call.buf().ok_or(Errno::EFAULT)?, &mut bytes)?;
        }
        self.stream_transmit(guest, call.into(), open_file, bytes, policy)
            .await
    }

    async fn network_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let segments = read_network_iovecs(guest, call.iov(), call.len())?;
        let maximum = iovec_capacity(&segments)?;
        let nonblocking = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.is_nonblocking())?;
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, call.into()).await;
                let input = match &result {
                    Ok(0) => NetworkCapturedStreamInput::EndOfFile,
                    Ok(count) => {
                        let count = usize::try_from(*count).map_err(|_| Errno::EIO)?;
                        NetworkCapturedStreamInput::Bytes(gather_iovec_prefix(
                            guest, &segments, count,
                        )?)
                    }
                    Err(error) => NetworkCapturedStreamInput::Error(
                        error_errno(error).ok_or_else(|| engine_error(error))?,
                    ),
                };
                self.capture_input(guest, open_file, input).await?;
                result
            }
            NetworkPolicy::Replay => {
                let outcome = self
                    .replay_stream_receive(guest, open_file, maximum, nonblocking)
                    .await?;
                match outcome {
                    NetworkStreamReceive::Bytes(bytes) => {
                        scatter_iovec_prefix(guest, &segments, &bytes)?;
                        Ok(bytes.len() as i64)
                    }
                    NetworkStreamReceive::EndOfFile => Ok(0),
                    NetworkStreamReceive::Error(errno) => errno_result(errno),
                    NetworkStreamReceive::WouldBlock => Err(Errno::EAGAIN.into()),
                    NetworkStreamReceive::Pending => unreachable!(),
                }
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn network_writev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Writev,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let segments = read_network_iovecs(guest, call.iov(), call.len())?;
        let bytes = gather_iovec_prefix(guest, &segments, iovec_capacity(&segments)?)?;
        self.stream_transmit(guest, call.into(), open_file, bytes, policy)
            .await
    }

    fn poll_array_has_network_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        address: Option<AddrMut<'_, libc::pollfd>>,
        count: libc::nfds_t,
    ) -> bool {
        read_pollfds(guest, address, count).is_ok_and(|fds| {
            fds.iter()
                .any(|pollfd| self.network_open_file(guest, pollfd.fd).is_some())
        })
    }

    async fn network_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let timeout = match call.timeout() {
            -1 => None,
            timeout if timeout < -1 => return Err(Errno::EINVAL.into()),
            timeout => Some(Duration::from_millis(timeout as u64)),
        };
        self.network_poll_common(
            guest,
            call.into(),
            NetworkPollState {
                address: call.fds().map(|address| address.as_raw()),
                count: call.nfds(),
                timeout,
                remaining_address: None,
            },
            policy,
        )
        .await
    }

    async fn network_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        if call.sigmask().is_some() {
            return Err(engine_error(
                "ppoll with a temporary signal mask is not replayable",
            ));
        }
        let timeout = call
            .timeout()
            .map(|address| guest.memory().read_value(address))
            .transpose()?
            .map(timespec_duration)
            .transpose()?;
        self.network_poll_common(
            guest,
            call.into(),
            NetworkPollState {
                address: call.fds().map(|address| address.as_raw()),
                count: call.nfds(),
                timeout,
                remaining_address: call.timeout().map(|address| address.as_raw()),
            },
            policy,
        )
        .await
    }

    async fn network_poll_common<G: Guest<Self>>(
        &self,
        guest: &mut G,
        live_call: Syscall,
        state: NetworkPollState,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let address = state.poll_address()?;
        let fds = read_pollfds(guest, address, state.count)?;
        let mut interests = Vec::new();
        for pollfd in &fds {
            if pollfd.fd < 0 {
                continue;
            }
            if pollfd.events & (libc::POLLPRI | libc::POLLRDBAND | libc::POLLWRBAND) != 0 {
                return Err(engine_error(format!(
                    "unsupported poll event mask {:#x}",
                    pollfd.events
                )));
            }
            match self.network_open_file(guest, pollfd.fd) {
                Some(open_file) => interests.push((open_file, NetworkWaitKind::Any)),
                None if guest.thread_state().with_detfd(pollfd.fd, |_| ()).is_ok() => {
                    return Err(engine_error(
                        "mixed network and non-network poll sets are not replayable",
                    ));
                }
                None => {}
            }
        }

        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, live_call).await;
                let ready_count = match result {
                    Ok(ready_count) => ready_count,
                    Err(error) => {
                        return Err(engine_error(format!(
                            "poll error outcome is not represented in trace v2: {error}"
                        )));
                    }
                };
                let observed_at = thread_observe_time(guest).await;
                let observed = read_pollfds(guest, address, state.count)?;
                for pollfd in observed {
                    let Some(open_file) = self.network_open_file(guest, pollfd.fd) else {
                        continue;
                    };
                    network_request(
                        guest,
                        NetworkRequest::CaptureReadiness {
                            open_file,
                            observed_at,
                            readiness: poll_revents_to_readiness(pollfd.revents),
                        },
                    )
                    .await
                    .map_err(engine_error)?;
                }
                Ok(ready_count)
            }
            NetworkPolicy::Replay => {
                let start = thread_observe_time(guest).await;
                let deadline = state.timeout.map(|duration| start + duration);
                loop {
                    let now = thread_observe_time(guest).await;
                    network_request(guest, NetworkRequest::ReleaseEligible(now))
                        .await
                        .map_err(engine_error)?;
                    let mut result = fds.clone();
                    let mut ready_count = 0i64;
                    for pollfd in &mut result {
                        pollfd.revents = 0;
                        if pollfd.fd < 0 {
                            continue;
                        }
                        let Some(open_file) = self.network_open_file(guest, pollfd.fd) else {
                            pollfd.revents = libc::POLLNVAL;
                            ready_count += 1;
                            continue;
                        };
                        let readiness =
                            match network_request(guest, NetworkRequest::Readiness(open_file))
                                .await
                                .map_err(engine_error)?
                            {
                                NetworkReply::Readiness(readiness) => readiness,
                                reply => {
                                    return Err(engine_error(format!(
                                        "unexpected readiness reply {reply:?}"
                                    )));
                                }
                            };
                        pollfd.revents = readiness_to_poll_revents(readiness, pollfd.events);
                        ready_count += i64::from(pollfd.revents != 0);
                    }
                    if ready_count != 0 || deadline.is_some_and(|deadline| now >= deadline) {
                        write_pollfds(guest, address, &result)?;
                        write_remaining_timeout(guest, state.remaining_address, deadline, now)?;
                        return Ok(ready_count);
                    }
                    let mut resources = Resources::new(guest.thread_state().dettid);
                    resources.insert(
                        ResourceID::NetworkWaitSet {
                            interests: interests.clone(),
                            deadline,
                        },
                        Permission::R,
                    );
                    resource_request(guest, resources).await;
                }
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    fn select_has_network_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        nfds: i32,
        read_address: Option<AddrMut<'_, libc::fd_set>>,
        write_address: Option<AddrMut<'_, libc::fd_set>>,
        except_address: Option<AddrMut<'_, libc::fd_set>>,
    ) -> bool {
        read_select_state(
            guest,
            nfds,
            read_address,
            write_address,
            except_address,
            None,
            SelectTimeoutAddress::None,
        )
        .is_ok_and(|state| {
            (0..state.nfds)
                .any(|fd| state.requested(fd) && self.network_open_file(guest, fd).is_some())
        })
    }

    async fn network_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Select,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let timeout = call
            .timeout()
            .map(|address| guest.memory().read_value(address))
            .transpose()?
            .map(timeval_duration)
            .transpose()?;
        let state = read_select_state(
            guest,
            call.nfds(),
            call.readfds(),
            call.writefds(),
            call.exceptfds(),
            timeout,
            call.timeout()
                .map_or(SelectTimeoutAddress::None, |address| {
                    SelectTimeoutAddress::Timeval(address.as_raw())
                }),
        )?;
        self.network_select_common(guest, call.into(), state, policy)
            .await
    }

    async fn network_pselect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        if call.sigmask().is_some() {
            return Err(engine_error(
                "pselect with a temporary signal mask is not replayable",
            ));
        }
        let timeout = call
            .timeout()
            .map(|address| guest.memory().read_value(address))
            .transpose()?
            .map(timespec_duration)
            .transpose()?;
        let state = read_select_state(
            guest,
            call.nfds(),
            call.readfds(),
            call.writefds(),
            call.exceptfds(),
            timeout,
            call.timeout()
                .map_or(SelectTimeoutAddress::None, |address| {
                    SelectTimeoutAddress::Timespec(address.as_raw())
                }),
        )?;
        self.network_select_common(guest, call.into(), state, policy)
            .await
    }

    async fn network_select_common<G: Guest<Self>>(
        &self,
        guest: &mut G,
        live_call: Syscall,
        state: NetworkSelectState,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        if state
            .except
            .as_ref()
            .is_some_and(|set| (0..state.nfds).any(|fd| fd_is_set(fd, set)))
        {
            return Err(engine_error(
                "select exceptional/OOB readiness is not represented by trace v2",
            ));
        }
        let mut interests = Vec::new();
        for fd in 0..state.nfds {
            if !state.requested(fd) {
                continue;
            }
            match self.network_open_file(guest, fd) {
                Some(open_file) => {
                    if !interests.contains(&(open_file, NetworkWaitKind::Any)) {
                        interests.push((open_file, NetworkWaitKind::Any));
                    }
                }
                None if guest.thread_state().with_detfd(fd, |_| ()).is_ok() => {
                    return Err(engine_error(
                        "mixed network and non-network select sets are not replayable",
                    ));
                }
                None => return Err(Errno::EBADF.into()),
            }
        }
        match policy {
            NetworkPolicy::Record => {
                let ready_count =
                    self.live_network_syscall(guest, live_call)
                        .await
                        .map_err(|error| {
                            engine_error(format!(
                                "select error outcome is not represented in trace v2: {error}"
                            ))
                        })?;
                let observed_at = thread_observe_time(guest).await;
                let observed = state.read_outputs(guest)?;
                for fd in 0..state.nfds {
                    let Some(open_file) = self.network_open_file(guest, fd) else {
                        continue;
                    };
                    network_request(
                        guest,
                        NetworkRequest::CaptureReadiness {
                            open_file,
                            observed_at,
                            readiness: NetworkReadinessV2 {
                                readable: observed
                                    .read
                                    .as_ref()
                                    .is_some_and(|set| fd_is_set(fd, set)),
                                writable: observed
                                    .write
                                    .as_ref()
                                    .is_some_and(|set| fd_is_set(fd, set)),
                                error: false,
                                hangup: false,
                            },
                        },
                    )
                    .await
                    .map_err(engine_error)?;
                }
                Ok(ready_count)
            }
            NetworkPolicy::Replay => {
                let start = thread_observe_time(guest).await;
                let deadline = state.timeout.map(|duration| start + duration);
                loop {
                    let now = thread_observe_time(guest).await;
                    network_request(guest, NetworkRequest::ReleaseEligible(now))
                        .await
                        .map_err(engine_error)?;
                    let mut output = state.empty_output();
                    let mut ready_count = 0i64;
                    for fd in 0..state.nfds {
                        if !state.requested(fd) {
                            continue;
                        }
                        let open_file = guest.thread_state().socket_open_file_id(fd)?;
                        let readiness =
                            match network_request(guest, NetworkRequest::Readiness(open_file))
                                .await
                                .map_err(engine_error)?
                            {
                                NetworkReply::Readiness(readiness) => readiness,
                                reply => {
                                    return Err(engine_error(format!(
                                        "unexpected readiness reply {reply:?}"
                                    )));
                                }
                            };
                        let read_ready = state.read.as_ref().is_some_and(|set| fd_is_set(fd, set))
                            && (readiness.readable || readiness.error || readiness.hangup);
                        let write_ready =
                            state.write.as_ref().is_some_and(|set| fd_is_set(fd, set))
                                && (readiness.writable || readiness.error);
                        if read_ready {
                            output.set_read(fd);
                        }
                        if write_ready {
                            output.set_write(fd);
                        }
                        ready_count += i64::from(read_ready || write_ready);
                    }
                    if ready_count != 0 || deadline.is_some_and(|deadline| now >= deadline) {
                        output.write(guest)?;
                        state.write_remaining(guest, deadline, now)?;
                        return Ok(ready_count);
                    }
                    let mut resources = Resources::new(guest.thread_state().dettid);
                    resources.insert(
                        ResourceID::NetworkWaitSet {
                            interests: interests.clone(),
                            deadline,
                        },
                        Permission::R,
                    );
                    resource_request(guest, resources).await;
                }
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn network_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvfrom,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        if call.addr().is_some() || call.addr_len().is_some() {
            return Err(engine_error(
                "stream recvfrom with source-address output is not yet representable",
            ));
        }
        let read = syscalls::Read::new()
            .with_fd(call.fd())
            .with_buf(call.buf())
            .with_len(call.len());
        self.network_read(guest, read, policy).await
    }

    async fn network_sendto<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendto,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        if call.addr().is_some() {
            return Err(engine_error(
                "destination-address sendto requires datagram trace semantics",
            ));
        }
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let mut bytes = vec![0; call.size()];
        if !bytes.is_empty() {
            let address = call.buf().ok_or(Errno::EFAULT)?.cast::<u8>();
            guest.memory().read_exact(address, &mut bytes)?;
        }
        self.stream_transmit(guest, call.into(), open_file, bytes, policy)
            .await
    }

    async fn stream_transmit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        live_call: Syscall,
        open_file: OpenFileId,
        bytes: Vec<u8>,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, live_call).await;
                let output = match &result {
                    Ok(count) => {
                        let count = usize::try_from(*count).map_err(|_| Errno::EIO)?;
                        NetworkCapturedStreamOutput::Bytes(bytes[..count].to_vec())
                    }
                    Err(error) => NetworkCapturedStreamOutput::Error(
                        error_errno(error).ok_or_else(|| engine_error(error))?,
                    ),
                };
                network_request(
                    guest,
                    NetworkRequest::CaptureStreamOutput { open_file, output },
                )
                .await
                .map_err(engine_error)?;
                result
            }
            NetworkPolicy::Replay => {
                match network_request(guest, NetworkRequest::TransmitStream { open_file, bytes })
                    .await
                    .map_err(engine_error)?
                {
                    NetworkReply::StreamTransmit(NetworkStreamTransmit::Accepted(count)) => {
                        Ok(count as i64)
                    }
                    NetworkReply::StreamTransmit(NetworkStreamTransmit::Error(errno)) => {
                        errno_result(errno)
                    }
                    reply => Err(engine_error(format!("unexpected transmit reply {reply:?}"))),
                }
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn network_shutdown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Shutdown,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let direction = shutdown_direction(call.how())?;
        match policy {
            NetworkPolicy::Record => {
                let result = self.live_network_syscall(guest, call.into()).await;
                if result.is_ok() {
                    network_request(
                        guest,
                        NetworkRequest::CaptureStreamOutput {
                            open_file,
                            output: NetworkCapturedStreamOutput::Shutdown(direction),
                        },
                    )
                    .await
                    .map_err(engine_error)?;
                }
                result
            }
            NetworkPolicy::Replay => {
                match network_request(guest, NetworkRequest::Shutdown(open_file, direction))
                    .await
                    .map_err(engine_error)?
                {
                    NetworkReply::Unit => Ok(0),
                    reply => Err(engine_error(format!("unexpected shutdown reply {reply:?}"))),
                }
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    async fn capture_input<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        input: NetworkCapturedStreamInput,
    ) -> Result<(), Error> {
        let observed_at = thread_observe_time(guest).await;
        network_request(
            guest,
            NetworkRequest::CaptureStreamInput {
                open_file,
                observed_at,
                input,
            },
        )
        .await
        .map_err(engine_error)?;
        Ok(())
    }

    async fn replay_stream_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        maximum: usize,
        nonblocking: bool,
    ) -> Result<NetworkStreamReceive, Error> {
        loop {
            let now = thread_observe_time(guest).await;
            network_request(guest, NetworkRequest::ReleaseEligible(now))
                .await
                .map_err(engine_error)?;
            match network_request(
                guest,
                NetworkRequest::ReceiveStream {
                    open_file,
                    maximum,
                    nonblocking,
                    flags: 0,
                    receive_low_water: 1,
                },
            )
            .await
            .map_err(engine_error)?
            {
                NetworkReply::StreamReceive(NetworkStreamReceive::Pending) => {
                    self.wait_for_network(guest, open_file, NetworkWaitKind::Readable)
                        .await;
                }
                NetworkReply::StreamReceive(outcome) => return Ok(outcome),
                reply => return Err(engine_error(format!("unexpected receive reply {reply:?}"))),
            }
        }
    }

    async fn wait_for_network<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        kind: NetworkWaitKind,
    ) {
        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::NetworkWait { open_file, kind }, Permission::R);
        resource_request(guest, resources).await;
    }

    async fn live_network_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        let mut resources = Resources::new(dettid);
        resources.insert(ResourceID::BlockingExternalIO(op_id), Permission::RW);
        resources.fyi(call.name());
        resource_request(guest, resources).await;
        let result = guest.inject(call).await.map_err(Error::from);
        let mut continuation = Resources::new(dettid);
        continuation.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
        continuation.fyi(call.name());
        resource_request(guest, continuation).await;
        result
    }
}

fn read_network_iovecs<G, T>(
    guest: &mut G,
    address: Option<Addr<'_, libc::iovec>>,
    count: usize,
) -> Result<Vec<(usize, usize)>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if count > libc::UIO_MAXIOV as usize {
        return Err(Errno::EINVAL.into());
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let address = address.ok_or(Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        count
    ];
    guest.memory().read_values(address, &mut iovecs)?;
    let segments: Vec<_> = iovecs
        .into_iter()
        .map(|iov| (iov.iov_base as usize, iov.iov_len))
        .collect();
    iovec_capacity(&segments)?;
    Ok(segments)
}

const MAX_NETWORK_POLL_FDS: usize = 4096;

#[derive(Clone, Copy)]
struct NetworkPollState {
    address: Option<usize>,
    count: libc::nfds_t,
    timeout: Option<Duration>,
    remaining_address: Option<usize>,
}

impl NetworkPollState {
    fn poll_address(&self) -> Result<Option<AddrMut<'_, libc::pollfd>>, Error> {
        self.address
            .map(|raw| AddrMut::from_raw(raw).ok_or_else(|| Errno::EFAULT.into()))
            .transpose()
    }
}

#[derive(Clone, Copy)]
enum SelectTimeoutAddress {
    None,
    Timeval(usize),
    Timespec(usize),
}

#[derive(Clone)]
struct NetworkSelectState {
    nfds: i32,
    read: Option<libc::fd_set>,
    write: Option<libc::fd_set>,
    except: Option<libc::fd_set>,
    read_address: Option<usize>,
    write_address: Option<usize>,
    except_address: Option<usize>,
    timeout: Option<Duration>,
    timeout_address: SelectTimeoutAddress,
}

impl NetworkSelectState {
    fn requested(&self, fd: i32) -> bool {
        self.read.as_ref().is_some_and(|set| fd_is_set(fd, set))
            || self.write.as_ref().is_some_and(|set| fd_is_set(fd, set))
            || self.except.as_ref().is_some_and(|set| fd_is_set(fd, set))
    }

    fn empty_output(&self) -> NetworkSelectOutput {
        NetworkSelectOutput {
            read: self.read_address.map(|_| empty_fd_set()),
            write: self.write_address.map(|_| empty_fd_set()),
            except: self.except_address.map(|_| empty_fd_set()),
            read_address: self.read_address,
            write_address: self.write_address,
            except_address: self.except_address,
        }
    }

    fn read_outputs<G, T>(&self, guest: &mut G) -> Result<NetworkSelectOutput, Error>
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        Ok(NetworkSelectOutput {
            read: read_fd_set_at(guest, self.read_address)?,
            write: read_fd_set_at(guest, self.write_address)?,
            except: read_fd_set_at(guest, self.except_address)?,
            read_address: self.read_address,
            write_address: self.write_address,
            except_address: self.except_address,
        })
    }

    fn write_remaining<G, T>(
        &self,
        guest: &mut G,
        deadline: Option<LogicalTime>,
        now: LogicalTime,
    ) -> Result<(), Error>
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        let Some(deadline) = deadline else {
            return Ok(());
        };
        let remaining = if deadline > now {
            deadline.duration_since(now)
        } else {
            Duration::ZERO
        };
        match self.timeout_address {
            SelectTimeoutAddress::None => Ok(()),
            SelectTimeoutAddress::Timeval(raw) => {
                let address = AddrMut::from_raw(raw).ok_or(Errno::EFAULT)?;
                guest.memory().write_value(
                    address,
                    &libc::timeval {
                        tv_sec: remaining.as_secs() as libc::time_t,
                        tv_usec: remaining.subsec_micros() as libc::suseconds_t,
                    },
                )?;
                Ok(())
            }
            SelectTimeoutAddress::Timespec(raw) => {
                let address = AddrMut::from_raw(raw).ok_or(Errno::EFAULT)?;
                guest.memory().write_value(
                    address,
                    &reverie::syscalls::Timespec {
                        tv_sec: remaining.as_secs() as libc::time_t,
                        tv_nsec: remaining.subsec_nanos() as libc::c_long,
                    },
                )?;
                Ok(())
            }
        }
    }
}

struct NetworkSelectOutput {
    read: Option<libc::fd_set>,
    write: Option<libc::fd_set>,
    except: Option<libc::fd_set>,
    read_address: Option<usize>,
    write_address: Option<usize>,
    except_address: Option<usize>,
}

impl NetworkSelectOutput {
    fn set_read(&mut self, fd: i32) {
        if let Some(set) = &mut self.read {
            fd_set(fd, set);
        }
    }

    fn set_write(&mut self, fd: i32) {
        if let Some(set) = &mut self.write {
            fd_set(fd, set);
        }
    }

    fn write<G, T>(&self, guest: &mut G) -> Result<(), Error>
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        write_fd_set_at(guest, self.read_address, self.read.as_ref())?;
        write_fd_set_at(guest, self.write_address, self.write.as_ref())?;
        write_fd_set_at(guest, self.except_address, self.except.as_ref())?;
        Ok(())
    }
}

fn empty_fd_set() -> libc::fd_set {
    // SAFETY: an all-zero fd_set is the empty set.
    unsafe { std::mem::zeroed() }
}

fn fd_is_set(fd: i32, set: &libc::fd_set) -> bool {
    // SAFETY: callers bound fd to [0, FD_SETSIZE), and set is initialized.
    unsafe { libc::FD_ISSET(fd, set) }
}

fn fd_set(fd: i32, set: &mut libc::fd_set) {
    // SAFETY: callers bound fd to [0, FD_SETSIZE), and set is initialized.
    unsafe { libc::FD_SET(fd, set) }
}

fn read_fd_set_at<G, T>(guest: &mut G, raw: Option<usize>) -> Result<Option<libc::fd_set>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    raw.map(|raw| {
        let address = Addr::<libc::fd_set>::from_raw(raw).ok_or(Errno::EFAULT)?;
        guest.memory().read_value(address).map_err(Error::from)
    })
    .transpose()
}

fn write_fd_set_at<G, T>(
    guest: &mut G,
    raw: Option<usize>,
    set: Option<&libc::fd_set>,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let (Some(raw), Some(set)) = (raw, set) {
        let address = AddrMut::from_raw(raw).ok_or(Errno::EFAULT)?;
        guest.memory().write_value(address, set)?;
    }
    Ok(())
}

fn read_select_state<G, T>(
    guest: &mut G,
    nfds: i32,
    read_address: Option<AddrMut<'_, libc::fd_set>>,
    write_address: Option<AddrMut<'_, libc::fd_set>>,
    except_address: Option<AddrMut<'_, libc::fd_set>>,
    timeout: Option<Duration>,
    timeout_address: SelectTimeoutAddress,
) -> Result<NetworkSelectState, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if nfds < 0 || nfds as usize > libc::FD_SETSIZE {
        return Err(Errno::EINVAL.into());
    }
    let read_address = read_address.map(|address| address.as_raw());
    let write_address = write_address.map(|address| address.as_raw());
    let except_address = except_address.map(|address| address.as_raw());
    Ok(NetworkSelectState {
        nfds,
        read: read_fd_set_at(guest, read_address)?,
        write: read_fd_set_at(guest, write_address)?,
        except: read_fd_set_at(guest, except_address)?,
        read_address,
        write_address,
        except_address,
        timeout,
        timeout_address,
    })
}

fn read_pollfds<G, T>(
    guest: &mut G,
    address: Option<AddrMut<'_, libc::pollfd>>,
    count: libc::nfds_t,
) -> Result<Vec<libc::pollfd>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let count = usize::try_from(count).map_err(|_| Errno::EINVAL)?;
    if count > MAX_NETWORK_POLL_FDS {
        return Err(Errno::EINVAL.into());
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let address = address.ok_or(Errno::EFAULT)?;
    let mut pollfds = vec![
        libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        };
        count
    ];
    guest.memory().read_values(address.into(), &mut pollfds)?;
    Ok(pollfds)
}

fn write_pollfds<G, T>(
    guest: &mut G,
    address: Option<AddrMut<'_, libc::pollfd>>,
    pollfds: &[libc::pollfd],
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if pollfds.is_empty() {
        return Ok(());
    }
    guest
        .memory()
        .write_values(address.ok_or(Errno::EFAULT)?, pollfds)?;
    Ok(())
}

fn timespec_duration(timespec: reverie::syscalls::Timespec) -> Result<Duration, Error> {
    if timespec.tv_sec < 0 || !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return Err(Errno::EINVAL.into());
    }
    Ok(Duration::new(
        timespec.tv_sec as u64,
        timespec.tv_nsec as u32,
    ))
}

fn timeval_duration(timeval: libc::timeval) -> Result<Duration, Error> {
    if timeval.tv_sec < 0 || !(0..1_000_000).contains(&timeval.tv_usec) {
        return Err(Errno::EINVAL.into());
    }
    Ok(Duration::new(
        timeval.tv_sec as u64,
        (timeval.tv_usec as u32) * 1_000,
    ))
}

fn write_remaining_timeout<G, T>(
    guest: &mut G,
    address: Option<usize>,
    deadline: Option<LogicalTime>,
    now: LogicalTime,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (Some(address), Some(deadline)) = (address, deadline) else {
        return Ok(());
    };
    let address = AddrMut::from_raw(address).ok_or(Errno::EFAULT)?;
    let remaining = if deadline > now {
        deadline.duration_since(now)
    } else {
        Duration::ZERO
    };
    let remaining = reverie::syscalls::Timespec {
        tv_sec: remaining.as_secs() as libc::time_t,
        tv_nsec: remaining.subsec_nanos() as libc::c_long,
    };
    guest.memory().write_value(address, &remaining)?;
    Ok(())
}

fn poll_revents_to_readiness(revents: i16) -> NetworkReadinessV2 {
    NetworkReadinessV2 {
        readable: revents & (libc::POLLIN | libc::POLLRDNORM) != 0,
        writable: revents & (libc::POLLOUT | libc::POLLWRNORM) != 0,
        error: revents & libc::POLLERR != 0,
        hangup: revents & libc::POLLHUP != 0,
    }
}

fn readiness_to_poll_revents(readiness: NetworkReadinessV2, events: i16) -> i16 {
    let mut revents = 0;
    if readiness.readable {
        revents |= events & (libc::POLLIN | libc::POLLRDNORM);
    }
    if readiness.writable {
        revents |= events & (libc::POLLOUT | libc::POLLWRNORM);
    }
    if readiness.error {
        revents |= libc::POLLERR;
    }
    if readiness.hangup {
        revents |= libc::POLLHUP;
    }
    revents
}

fn iovec_capacity(segments: &[(usize, usize)]) -> Result<usize, Error> {
    let total = segments.iter().try_fold(0usize, |total, (_, length)| {
        total.checked_add(*length).ok_or(Errno::EINVAL)
    })?;
    if total > isize::MAX as usize {
        return Err(Errno::EINVAL.into());
    }
    Ok(total)
}

fn gather_iovec_prefix<G, T>(
    guest: &mut G,
    segments: &[(usize, usize)],
    count: usize,
) -> Result<Vec<u8>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if count > iovec_capacity(segments)? {
        return Err(Errno::EIO.into());
    }
    let mut bytes = Vec::with_capacity(count);
    let mut remaining = count;
    for &(base, length) in segments {
        if remaining == 0 {
            break;
        }
        let length = length.min(remaining);
        if length == 0 {
            continue;
        }
        let address = Addr::<u8>::from_raw(base).ok_or(Errno::EFAULT)?;
        let start = bytes.len();
        bytes.resize(start + length, 0);
        guest
            .memory()
            .read_exact(address, &mut bytes[start..start + length])?;
        remaining -= length;
    }
    Ok(bytes)
}

fn scatter_iovec_prefix<G, T>(
    guest: &mut G,
    segments: &[(usize, usize)],
    bytes: &[u8],
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if bytes.len() > iovec_capacity(segments)? {
        return Err(Errno::EIO.into());
    }
    let mut offset = 0;
    for &(base, length) in segments {
        if offset == bytes.len() {
            break;
        }
        let length = length.min(bytes.len() - offset);
        if length == 0 {
            continue;
        }
        let address = AddrMut::<u8>::from_raw(base).ok_or(Errno::EFAULT)?;
        guest
            .memory()
            .write_exact(address, &bytes[offset..offset + length])?;
        offset += length;
    }
    Ok(())
}

fn read_accept_peer<G, T>(
    guest: &mut G,
    call: syscalls::Accept4,
) -> Result<Option<NetworkAddressV2>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(address) = call.sockaddr() else {
        return Ok(None);
    };
    let length = guest
        .memory()
        .read_value(call.addrlen().ok_or(Errno::EFAULT)?)?;
    let length = i32::try_from(length).map_err(|_| Errno::EINVAL)?;
    read_network_address(guest, Some(address), length).map(Some)
}

fn write_accept_peer<G, T>(
    guest: &mut G,
    call: syscalls::Accept4,
    peer: Option<&NetworkAddressV2>,
) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let Some(address) = call.sockaddr() else {
        return Ok(());
    };
    let length_address = call.addrlen().ok_or(Errno::EFAULT)?;
    let capacity: usize = guest.memory().read_value(length_address)?;
    let bytes = peer
        .map(network_address_bytes)
        .transpose()?
        .unwrap_or_default();
    let copied = capacity.min(bytes.len());
    if copied != 0 {
        guest
            .memory()
            .write_exact(address.cast(), &bytes[..copied])?;
    }
    guest.memory().write_value(length_address, &bytes.len())?;
    Ok(())
}

fn network_address_bytes(address: &NetworkAddressV2) -> Result<Vec<u8>, Error> {
    fn bytes_of<T>(value: &T) -> &[u8] {
        // SAFETY: these libc socket-address records contain only integer fields
        // and byte arrays, are initialized from zero, and are copied as bytes.
        unsafe {
            std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
        }
    }

    match address {
        NetworkAddressV2::Inet4 { address, port } => {
            // SAFETY: all-zero is a valid initialized sockaddr_in.
            let mut raw: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            raw.sin_port = port.to_be();
            raw.sin_addr.s_addr = u32::from_ne_bytes(*address);
            Ok(bytes_of(&raw).to_vec())
        }
        NetworkAddressV2::Inet6 {
            address,
            port,
            flowinfo,
            scope_id,
        } => {
            // SAFETY: all-zero is a valid initialized sockaddr_in6.
            let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = port.to_be();
            raw.sin6_addr.s6_addr = *address;
            raw.sin6_flowinfo = *flowinfo;
            raw.sin6_scope_id = *scope_id;
            Ok(bytes_of(&raw).to_vec())
        }
        NetworkAddressV2::UnixPath(path) | NetworkAddressV2::UnixAbstract(path) => {
            let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
            let abstract_prefix = usize::from(matches!(address, NetworkAddressV2::UnixAbstract(_)));
            if path.len() + abstract_prefix > 108 {
                return Err(engine_error("Unix socket address exceeds sun_path"));
            }
            // SAFETY: all-zero is a valid initialized sockaddr_un.
            let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            raw.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let raw_bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    (&mut raw as *mut libc::sockaddr_un).cast::<u8>(),
                    std::mem::size_of::<libc::sockaddr_un>(),
                )
            };
            let start = path_offset + abstract_prefix;
            raw_bytes[start..start + path.len()].copy_from_slice(path);
            Ok(raw_bytes[..start + path.len()].to_vec())
        }
        NetworkAddressV2::UnixUnnamed => {
            // SAFETY: all-zero is a valid initialized sockaddr_un.
            let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            raw.sun_family = libc::AF_UNIX as libc::sa_family_t;
            Ok(bytes_of(&raw)[..std::mem::offset_of!(libc::sockaddr_un, sun_path)].to_vec())
        }
    }
}

fn read_network_address<G, T>(
    guest: &mut G,
    address: Option<reverie::syscalls::AddrMut<'_, libc::sockaddr>>,
    length: i32,
) -> Result<NetworkAddressV2, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let address = address.ok_or(Errno::EFAULT)?;
    if length < std::mem::size_of::<libc::sa_family_t>() as i32 {
        return Err(Errno::EINVAL.into());
    }
    let family: libc::sa_family_t = guest.memory().read_value(address.cast())?;
    match i32::from(family) {
        libc::AF_INET => {
            if length < std::mem::size_of::<libc::sockaddr_in>() as i32 {
                return Err(Errno::EINVAL.into());
            }
            let raw: libc::sockaddr_in = guest.memory().read_value(address.cast())?;
            Ok(NetworkAddressV2::Inet4 {
                address: raw.sin_addr.s_addr.to_ne_bytes(),
                port: u16::from_be(raw.sin_port),
            })
        }
        libc::AF_INET6 => {
            if length < std::mem::size_of::<libc::sockaddr_in6>() as i32 {
                return Err(Errno::EINVAL.into());
            }
            let raw: libc::sockaddr_in6 = guest.memory().read_value(address.cast())?;
            Ok(NetworkAddressV2::Inet6 {
                address: raw.sin6_addr.s6_addr,
                port: u16::from_be(raw.sin6_port),
                flowinfo: raw.sin6_flowinfo,
                scope_id: raw.sin6_scope_id,
            })
        }
        libc::AF_UNIX => {
            let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
            let length = usize::try_from(length).map_err(|_| Errno::EINVAL)?;
            if length > std::mem::size_of::<libc::sockaddr_un>() {
                return Err(Errno::EINVAL.into());
            }
            if length <= path_offset {
                return Ok(NetworkAddressV2::UnixUnnamed);
            }
            let mut bytes = vec![0; length];
            guest.memory().read_exact(address.cast(), &mut bytes)?;
            let path = &bytes[path_offset..];
            if path.first() == Some(&0) {
                Ok(NetworkAddressV2::UnixAbstract(path[1..].to_vec()))
            } else {
                Ok(NetworkAddressV2::UnixPath(path.to_vec()))
            }
        }
        family => Err(engine_error(format!(
            "unsupported connect address family {family}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iovec_capacity_preserves_empty_segments_and_rejects_overflow() {
        assert_eq!(iovec_capacity(&[]).unwrap(), 0);
        assert_eq!(iovec_capacity(&[(0, 0), (1, 3), (4, 5)]).unwrap(), 8);
        assert!(matches!(
            iovec_capacity(&[(1, isize::MAX as usize), (2, 1)]),
            Err(Error::Errno(errno)) if errno == Errno::EINVAL
        ));
    }

    #[test]
    fn normalized_unix_addresses_preserve_exact_path_bytes() {
        let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
        let pathname =
            network_address_bytes(&NetworkAddressV2::UnixPath(vec![b'a', b'b', b'c', 0])).unwrap();
        assert_eq!(&pathname[path_offset..], b"abc\0");

        let abstract_name =
            network_address_bytes(&NetworkAddressV2::UnixAbstract(b"abc".to_vec())).unwrap();
        assert_eq!(&abstract_name[path_offset..], b"\0abc");
        assert_eq!(
            network_address_bytes(&NetworkAddressV2::UnixUnnamed)
                .unwrap()
                .len(),
            path_offset
        );
    }

    #[test]
    fn poll_readiness_respects_interest_but_always_reports_error_and_hangup() {
        let readiness = NetworkReadinessV2 {
            readable: true,
            writable: true,
            error: true,
            hangup: true,
        };
        assert_eq!(
            readiness_to_poll_revents(readiness, libc::POLLIN),
            libc::POLLIN | libc::POLLERR | libc::POLLHUP
        );
        assert_eq!(
            readiness_to_poll_revents(readiness, libc::POLLOUT),
            libc::POLLOUT | libc::POLLERR | libc::POLLHUP
        );
    }

    #[test]
    fn readiness_timeouts_reject_non_linux_shapes_without_rounding() {
        assert_eq!(
            timespec_duration(reverie::syscalls::Timespec {
                tv_sec: 2,
                tv_nsec: 345_678_901,
            })
            .unwrap(),
            Duration::new(2, 345_678_901)
        );
        assert!(matches!(
            timespec_duration(reverie::syscalls::Timespec {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            }),
            Err(Error::Errno(errno)) if errno == Errno::EINVAL
        ));
        assert!(matches!(
            timeval_duration(libc::timeval {
                tv_sec: -1,
                tv_usec: 0,
            }),
            Err(Error::Errno(errno)) if errno == Errno::EINVAL
        ));
    }
}
