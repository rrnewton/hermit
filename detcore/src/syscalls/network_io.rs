/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Single guest-memory adapter for engine-owned network syscalls.

use std::io::IoSlice;
use std::time::Duration;

use detcore_model::network_trace::FreshStreamSocketProfileV3;
use detcore_model::network_trace::LinuxReceiveHzV3;
use detcore_model::network_trace::LinuxReceiveNormalizationV3;
use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkEndpointRoleV2;
use detcore_model::network_trace::NetworkInputEventV2;
use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkPolicy;
use detcore_model::network_trace::NetworkReadinessV2;
use detcore_model::network_trace::NetworkReleaseV2;
use detcore_model::network_trace::NetworkShutdownV2;
use detcore_model::network_trace::NetworkTransportV2;
use detcore_model::network_trace::ReceiveBufferStateV3;
use detcore_model::network_trace::ReceiveTimeoutV3;
use detcore_model::network_trace::StreamSocketKeyV3;
use detcore_model::network_trace::StreamSocketOptionsV3;
use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::AddrSliceMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use crate::Detcore;
use crate::fd::FdType;
use crate::network_failure::NetworkRpcError;
use crate::network_replay::NetworkChannelBinding;
use crate::network_replay::NetworkSocketControl;
use crate::network_replay::NetworkSocketControlFinish;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamChunk;
use crate::network_replay::NetworkStreamChunkDisposition;
use crate::network_replay::NetworkStreamChunkOutcome;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamNamespace;
use crate::network_replay::NetworkStreamPhysicalEffect;
use crate::network_replay::NetworkStreamPhysicalResult;
use crate::network_replay::NetworkStreamPinOutcome;
use crate::network_replay::NetworkStreamQueueStatus;
use crate::network_replay::NetworkStreamSocketOption;
use crate::network_replay::NetworkStreamSocketState;
use crate::network_replay::NetworkZeroStreamReceive;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::NetworkWaitKind;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::tool_global::NetworkCapturedStreamInput;
use crate::tool_global::NetworkCapturedStreamOutput;
use crate::tool_global::NetworkReply;
use crate::tool_global::NetworkRequest;
use crate::tool_global::NetworkStreamReceive;
use crate::tool_global::NetworkStreamTransmit;
use crate::tool_global::ResumeStatus;
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

// Only the typed RPC result uses this conversion. Local protocol failures
// continue through engine_error, and Linux errno results keep Error::Errno.
fn engine_rpc_error(error: NetworkRpcError) -> Error {
    Error::Tool(
        error
            .into_error()
            .context("shared network engine refused operation"),
    )
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3174): Review socket ownership and shared V3 receive dispatch.
// https://github.com/rrnewton/hermit/pull/3174
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
        let admission = self
            .begin_network_fd_mutation(guest, crate::network_replay::NetworkFdMutationKind::Socket)
            .await?;
        let physical = guest.inject(call).await;
        let fd = self
            .observe_network_fd_result(guest, admission.as_ref(), physical)
            .await? as i32;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.r#type()),
            FdType::Socket,
        )
        .await?;
        self.enroll_fresh_stream_socket(guest, fd, call).await?;
        self.complete_network_fd_installation(guest, admission.as_ref(), fd)
            .await?;
        Ok(i64::from(fd))
    }

    async fn ensure_channel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        binding: NetworkChannelBinding,
    ) -> Result<NetworkChannelId, Error> {
        match network_request(guest, NetworkRequest::EnsureChannel { open_file, binding })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::Channel(Some(channel)) => Ok(channel),
            reply => Err(engine_error(format!(
                "unexpected channel binding reply {reply:?}"
            ))),
        }
    }

    async fn ensure_stream_channel<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        fd: i32,
        peer: NetworkAddressV2,
    ) -> Result<NetworkChannelId, Error> {
        let transport = read_socket_stream_transport(guest, fd).await?;
        self.ensure_channel(
            guest,
            open_file,
            NetworkChannelBinding {
                transport,
                role: NetworkEndpointRoleV2::OutboundClient,
                peer_address: Some(peer),
                // Bind requests are not retained by this adapter yet. An
                // assigned getsockname endpoint is not a requested constraint.
                requested_local_constraint: None,
                observed_local_address: None,
                accepted_from: None,
                selected_channel: None,
            },
        )
        .await
    }

    async fn network_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        let peer = read_network_address(guest, call.uservaddr(), call.addrlen())?;
        self.ensure_stream_channel(guest, open_file, call.fd(), peer)
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
                .map_err(engine_rpc_error)?;
                result
            }
            NetworkPolicy::Replay => loop {
                let now = thread_observe_time(guest).await;
                network_request(guest, NetworkRequest::ReleaseEligible(now))
                    .await
                    .map_err(engine_rpc_error)?;
                match network_request(guest, NetworkRequest::TakeConnectionOutcome(open_file))
                    .await
                    .map_err(engine_rpc_error)?
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
        fd: i32,
    ) -> Result<NetworkChannelId, Error> {
        let transport = read_socket_stream_transport(guest, fd).await?;
        self.ensure_channel(
            guest,
            open_file,
            NetworkChannelBinding {
                transport,
                role: NetworkEndpointRoleV2::Listener,
                peer_address: None,
                requested_local_constraint: None,
                observed_local_address: None,
                accepted_from: None,
                selected_channel: None,
            },
        )
        .await
    }

    async fn network_listen<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Listen,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        self.ensure_listener_channel(guest, open_file, call.fd())
            .await?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3174): provider enrollment must precede listen.
        if self.accepted_model_mode(guest).await? && policy == NetworkPolicy::Record {
            let pin = self
                .begin_host_stream_call(guest, call.fd(), open_file, policy)
                .await?;
            let work = async {
                self.shadow_ack(
                    guest,
                    NetworkRequest::EnrollAcceptedListener {
                        call: pin.call,
                        fd: call.fd(),
                    },
                )
                .await?;
                self.live_network_syscall(guest, call.into()).await
            }
            .await;
            return self.finish_host_stream_call(guest, pin, work).await;
        }
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
            .ensure_listener_channel(guest, listener, call.sockfd())
            .await?;
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3174): accepted-child provider/custody enrollment.
        if self.accepted_model_mode(guest).await? {
            return self
                .accepted_socket_transaction(guest, call, listener, policy)
                .await;
        }
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
                let peer = Some(read_accepted_peer(guest, fd).await?);
                let transport = read_socket_stream_transport(guest, fd).await?;
                let accepted_channel = self
                    .ensure_channel(
                        guest,
                        accepted,
                        NetworkChannelBinding {
                            transport,
                            role: NetworkEndpointRoleV2::Accepted,
                            peer_address: peer.clone(),
                            requested_local_constraint: None,
                            observed_local_address: None,
                            accepted_from: Some(listener_channel),
                            selected_channel: None,
                        },
                    )
                    .await?;
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
                .map_err(engine_rpc_error)?;
                Ok(i64::from(fd))
            }
            NetworkPolicy::Replay => loop {
                let now = thread_observe_time(guest).await;
                network_request(guest, NetworkRequest::ReleaseEligible(now))
                    .await
                    .map_err(engine_rpc_error)?;
                match network_request(guest, NetworkRequest::TakeConnectionOutcome(listener))
                    .await
                    .map_err(engine_rpc_error)?
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
                        let transport = read_socket_stream_transport(guest, fd).await?;
                        self.ensure_channel(
                            guest,
                            open_file,
                            NetworkChannelBinding {
                                transport,
                                role: NetworkEndpointRoleV2::Accepted,
                                peer_address: peer.clone(),
                                requested_local_constraint: None,
                                observed_local_address: None,
                                accepted_from: Some(listener_channel),
                                selected_channel: Some(accepted),
                            },
                        )
                        .await?;
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
        if self.shadow_socket_state(guest, open_file).await?.is_some() {
            let segments = [(call.buf().map_or(0, |address| address.as_raw()), call.len())];
            return self
                .shadow_stream_receive(
                    guest,
                    call.fd(),
                    NetworkReceiveContext {
                        segments: &segments,
                        flags: 0,
                        zero_read: true,
                        restart_errno: call.signal_interrupt_errno(),
                        policy,
                    },
                )
                .await;
        }
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
        if self.shadow_socket_state(guest, open_file).await?.is_some() {
            let segments = read_network_iovecs(guest, call.iov(), call.len())?;
            return self
                .shadow_stream_receive(
                    guest,
                    call.fd(),
                    NetworkReceiveContext {
                        segments: &segments,
                        flags: 0,
                        zero_read: true,
                        restart_errno: call.signal_interrupt_errno(),
                        policy,
                    },
                )
                .await;
        }
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
        if self.shadow_mode(guest).await? {
            return self.shadow_network_poll(guest, state, policy).await;
        }
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
                    .map_err(engine_rpc_error)?;
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
                        .map_err(engine_rpc_error)?;
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
                                .map_err(engine_rpc_error)?
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
        if self.shadow_mode(guest).await? {
            return self.shadow_network_select(guest, state, policy).await;
        }
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
                    .map_err(engine_rpc_error)?;
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
                        .map_err(engine_rpc_error)?;
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
                                .map_err(engine_rpc_error)?
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
                        ready_count += select_ready_bit_count(read_ready, write_ready);
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
        let open_file = guest.thread_state().socket_open_file_id(call.fd())?;
        if self.shadow_socket_state(guest, open_file).await?.is_some() {
            if call.addr().is_some() || call.addr_len().is_some() {
                return Err(engine_error(
                    "stream recvfrom with source-address output is not yet representable",
                ));
            }
            let segments = [(call.buf().map_or(0, |address| address.as_raw()), call.len())];
            return self
                .shadow_stream_receive(
                    guest,
                    call.fd(),
                    NetworkReceiveContext {
                        segments: &segments,
                        flags: call.flags(),
                        zero_read: false,
                        restart_errno: call.signal_interrupt_errno(),
                        policy,
                    },
                )
                .await;
        }
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
                .map_err(engine_rpc_error)?;
                result
            }
            NetworkPolicy::Replay => {
                match network_request(guest, NetworkRequest::TransmitStream { open_file, bytes })
                    .await
                    .map_err(engine_rpc_error)?
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
        if self.shadow_socket_state(guest, open_file).await?.is_some()
            && let Some(control) = self.begin_shadow_fd_control(guest, call.fd()).await?
        {
            // The same OFD control excludes observation until the local effect
            // and its existing Shutdown output are published together. An
            // interrupted callback leaves the submitted effect unresolved.
            let result = async {
                self.shadow_submit(
                    guest,
                    control.lease,
                    NetworkStreamPhysicalEffect::Shutdown { direction },
                )
                .await?;
                let result = if policy == NetworkPolicy::Record {
                    guest.inject(call).await
                } else {
                    // Submit validated the next expected local Shutdown. This
                    // is a modeled transition, never a placeholder observation.
                    Ok(0)
                };
                self.shadow_confirm(
                    guest,
                    control.lease,
                    NetworkStreamPhysicalResult::Shutdown {
                        result: result.map(|_| ()).map_err(|errno| errno.into_raw()),
                    },
                )
                .await?;
                result.map_err(Error::from)
            }
            .await;
            return self
                .finish_shadow_fd_control(guest, Some(control), result)
                .await;
        }
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
                    .map_err(engine_rpc_error)?;
                }
                result
            }
            NetworkPolicy::Replay => {
                match network_request(guest, NetworkRequest::Shutdown(open_file, direction))
                    .await
                    .map_err(engine_rpc_error)?
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
        .map_err(engine_rpc_error)?;
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
                .map_err(engine_rpc_error)?;
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
            .map_err(engine_rpc_error)?
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
        self.live_network_syscall_with_completion(guest, call, NetworkPhysicalCompletion::None)
            .await
    }

    async fn live_network_syscall_with_completion<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        completion: NetworkPhysicalCompletion,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        let mut resources = Resources::new(dettid);
        resources.insert(ResourceID::BlockingNetworkCapture(op_id), Permission::RW);
        resources.fyi(call.name());
        resource_request(guest, resources).await;
        let result = guest.inject(call).await.map_err(Error::from);
        // Ptrace capture's first poll is synchronous through global dispatch.
        // No scheduler continuation or other await intervenes after injection.
        // A capture failure never skips the paid external continuation below.
        let capture = completion.capture(guest, &result).await;
        let mut continuation = Resources::new(dettid);
        continuation.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
        continuation.fyi(call.name());
        resource_request(guest, continuation).await;
        finish_shadow_operation(result, capture)
    }
}

enum NetworkPhysicalCompletion {
    None,
    Accept(crate::network_replay::NetworkAcceptLeaseId),
}

impl NetworkPhysicalCompletion {
    async fn capture<G, T>(self, guest: &mut G, result: &Result<i64, Error>) -> Result<(), Error>
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        let Self::Accept(lease) = self else {
            return Ok(());
        };
        let kernel_result = match result {
            Ok(fd) => Ok(i32::try_from(*fd)
                .map_err(|_| engine_error("accept returned an invalid descriptor"))?),
            Err(Error::Errno(errno)) => Err(errno.into_raw()),
            Err(_) => return Ok(()), // Submitted receipt retains an unknown effect.
        };
        match network_request(
            guest,
            NetworkRequest::CaptureAcceptedReturn {
                lease,
                kernel_result,
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::Unit => Ok(()),
            reply => Err(engine_error(format!(
                "unexpected accepted capture reply {reply:?}"
            ))),
        }
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

fn shadow_readiness_to_poll_revents(
    status: &crate::network_replay::NetworkStreamQueueStatus,
    low_water: usize,
    events: i16,
) -> i16 {
    let mut readiness = status.readiness;
    if !status.listener {
        readiness.readable = status.queued_bytes >= low_water.max(1)
            || status.eof
            || status.local_read_shutdown
            || status.error.is_some();
        readiness.error |= status.error.is_some();
    }
    let mut revents = readiness_to_poll_revents(readiness, events);
    if events & libc::POLLRDHUP != 0 && (status.eof || status.local_read_shutdown) {
        revents |= libc::POLLRDHUP;
    }
    revents
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

async fn read_socket_stream_transport<G, T>(
    guest: &mut G,
    fd: i32,
) -> Result<NetworkTransportV2, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut stack = guest.stack().await;
    let value = stack.reserve::<i32>();
    let length = stack.reserve::<libc::socklen_t>();
    let _guard = stack.commit()?;
    let mut properties = [0; 3];
    for (output, option) in
        properties
            .iter_mut()
            .zip([libc::SO_DOMAIN, libc::SO_TYPE, libc::SO_PROTOCOL])
    {
        let size = std::mem::size_of::<i32>() as libc::socklen_t;
        guest.memory().write_value(length, &size)?;
        guest
            .inject(
                syscalls::Getsockopt::new()
                    .with_fd(fd)
                    .with_level(libc::SOL_SOCKET)
                    .with_optname(option)
                    .with_optval(Some(value.cast()))
                    .with_optlen(Some(length)),
            )
            .await?;
        if guest.memory().read_value(length)? != size {
            return Err(engine_error(
                "socket transport query returned an invalid integer size",
            ));
        }
        *output = guest.memory().read_value(value)?;
    }
    classify_stream_transport(properties)
}

fn classify_stream_transport(properties: [i32; 3]) -> Result<NetworkTransportV2, Error> {
    match properties {
        [
            libc::AF_INET | libc::AF_INET6,
            libc::SOCK_STREAM,
            libc::IPPROTO_TCP,
        ] => Ok(NetworkTransportV2::Tcp),
        [libc::AF_UNIX, libc::SOCK_STREAM, 0] => Ok(NetworkTransportV2::UnixStream),
        [domain, socket_type, protocol] => Err(engine_error(format!(
            "unsupported stream socket transport: domain={domain} type={socket_type} protocol={protocol}"
        ))),
    }
}

async fn read_accepted_peer<G, T>(guest: &mut G, fd: i32) -> Result<NetworkAddressV2, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // accept may have returned only a prefix, or no address at all. Query the
    // accepted local socket into owned scratch instead of reading beyond the
    // caller's address buffer to construct the complete recorded endpoint.
    let mut stack = guest.stack().await;
    let address = stack.reserve::<libc::sockaddr_storage>();
    let capacity = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let length = stack.reserve::<libc::socklen_t>();
    let _guard = stack.commit()?;
    guest.memory().write_value(length, &capacity)?;
    guest
        .inject(
            syscalls::Getpeername::new()
                .with_fd(fd)
                .with_usockaddr(Some(address.cast()))
                .with_usockaddr_len(Some(length)),
        )
        .await?;
    let length: libc::socklen_t = guest.memory().read_value(length)?;
    if length > capacity {
        return Err(engine_error("accepted peer exceeds sockaddr_storage"));
    }
    read_network_address(guest, Some(address.cast()), length as i32)
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
    // The typed Accept4 facade uses usize here; Linux's ABI is socklen_t.
    let length_address = call.addrlen().ok_or(Errno::EFAULT)?.cast();
    write_accept_peer_memory(&mut guest.memory(), address, length_address, peer)
}

fn write_accept_peer_memory<M: MemoryAccess>(
    memory: &mut M,
    address: AddrMut<libc::sockaddr>,
    length_address: AddrMut<libc::socklen_t>,
    peer: Option<&NetworkAddressV2>,
) -> Result<(), Error> {
    let mut length_bytes = [0; std::mem::size_of::<libc::socklen_t>()];
    memory.read_exact_with_user_access(length_address.cast::<u8>(), &mut length_bytes)?;
    let capacity = libc::socklen_t::from_ne_bytes(length_bytes);
    // move_addr_to_user interprets the input socklen as a signed integer.
    let capacity = usize::try_from(i32::try_from(capacity).map_err(|_| Errno::EINVAL)?)
        .map_err(|_| Errno::EINVAL)?;
    let bytes = peer
        .map(network_address_bytes)
        .transpose()?
        .unwrap_or_default();
    let copied = capacity.min(bytes.len());
    let actual = libc::socklen_t::try_from(bytes.len()).map_err(|_| Errno::EINVAL)?;
    // Linux publishes the full length before copying the truncated address.
    // A later address fault retains this update; a length fault copies no address.
    write_sockaddr_user_bytes(memory, length_address.cast(), &actual.to_ne_bytes())?;
    write_sockaddr_user_bytes(memory, address.cast(), &bytes[..copied])?;
    Ok(())
}

fn write_sockaddr_user_bytes<M: MemoryAccess>(
    memory: &mut M,
    address: AddrMut<u8>,
    bytes: &[u8],
) -> Result<(), Errno> {
    if bytes.is_empty() {
        return Ok(());
    }
    address
        .as_raw()
        .checked_add(bytes.len())
        .ok_or(Errno::EFAULT)?;
    // Use the remote-memory vectored interface: the scalar ptrace eight-byte
    // fast path can force writes through read-only or inaccessible mappings.
    // Socket addresses are bounded; split at a page boundary to retain the
    // kernel's observable prefix if a later destination page is inaccessible.
    let mut remote = unsafe { AddrSliceMut::from_raw_parts(address, bytes.len()) };
    let local = [IoSlice::new(bytes)];
    let written = if let Some((mut first, mut second)) = remote.split_at_page_boundary() {
        let mut destinations = unsafe { [first.as_ioslice_mut(), second.as_ioslice_mut()] };
        memory.write_vectored(&local, &mut destinations)?
    } else {
        let mut page = unsafe { AddrSliceMut::from_raw_parts(address, bytes.len()) };
        let mut destinations = unsafe { [page.as_ioslice_mut()] };
        memory.write_vectored(&local, &mut destinations)?
    };
    if written != bytes.len() {
        return Err(Errno::EFAULT);
    }
    Ok(())
}

fn select_ready_bit_count(readable: bool, writable: bool) -> i64 {
    // select counts set bits across the returned sets, including an fd that is
    // simultaneously readable and writable in both sets.
    i64::from(readable) + i64::from(writable)
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

#[derive(Debug)]
struct StreamUserCopy {
    // Observable guest-memory side effects, NOT the consumed stream count.
    written: usize,
    error: Option<Errno>,
}

fn copy_stream_chunk_to_user<M: MemoryAccess>(
    memory: &mut M,
    segments: &[(usize, usize)],
    mut destination_offset: usize,
    bytes: &[u8],
) -> StreamUserCopy {
    if bytes.len() > SHADOW_VIEW {
        return StreamUserCopy {
            written: 0,
            error: Some(Errno::EIO),
        };
    }
    let mut written = 0;
    for &(base, capacity) in segments {
        if written == bytes.len() {
            break;
        }
        if destination_offset >= capacity {
            destination_offset -= capacity;
            continue;
        }
        let count = (capacity - destination_offset).min(bytes.len() - written);
        let raw = match base
            .checked_add(destination_offset)
            .filter(|raw| raw.checked_add(count).is_some())
        {
            Some(raw) => raw,
            None => {
                return StreamUserCopy {
                    written,
                    error: Some(Errno::EFAULT),
                };
            }
        };
        let Some(address) = AddrMut::<u8>::from_raw(raw) else {
            return StreamUserCopy {
                written,
                error: Some(Errno::EFAULT),
            };
        };
        // The caller's reserved view is <=512 bytes, so each iovec piece spans
        // at most two pages. Never use the scalar eight-byte ptrace fast path.
        let mut remote = unsafe { AddrSliceMut::from_raw_parts(address, count) };
        let local = [IoSlice::new(&bytes[written..written + count])];
        let result = if let Some((mut first, mut second)) = remote.split_at_page_boundary() {
            let mut remote = unsafe { [first.as_ioslice_mut(), second.as_ioslice_mut()] };
            memory.write_vectored(&local, &mut remote)
        } else {
            let mut page = unsafe { AddrSliceMut::from_raw_parts(address, count) };
            let mut remote = unsafe { [page.as_ioslice_mut()] };
            memory.write_vectored(&local, &mut remote)
        };
        match result {
            Ok(count_written) if count_written <= count => {
                written += count_written;
                if count_written != count {
                    return StreamUserCopy {
                        written,
                        error: Some(Errno::EFAULT),
                    };
                }
            }
            Ok(_) => {
                return StreamUserCopy {
                    written,
                    error: Some(Errno::EIO),
                };
            }
            Err(error) => {
                return StreamUserCopy {
                    written,
                    error: Some(error),
                };
            }
        }
        destination_offset = 0;
    }
    StreamUserCopy {
        written,
        error: (written != bytes.len()).then_some(Errno::EFAULT),
    }
}

// Proposal fragment only. Requires exact core seam API.fragment.rs and
// NetworkWaitKind::ReadableAtLeast(usize); not applied or compiled.

// Preserve the primary typed failure across independent cleanup. This mirrors
// the CLI's finish_run_with_cleanup boundary without reclassifying prose.
fn finish_shadow_operation<V>(
    primary: Result<V, Error>,
    cleanup: Result<(), Error>,
) -> Result<V, Error> {
    match (primary, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(Error::Tool(primary)), Err(secondary)) => Err(Error::Tool(
            primary.context(format!("secondary network cleanup failure: {secondary:#}")),
        )),
        (Err(primary), Err(secondary)) => Err(Error::Tool(
            anyhow::Error::new(primary)
                .context(format!("secondary network cleanup failure: {secondary:#}")),
        )),
    }
}

#[cfg(test)]
mod shadow_completion_tests {
    use super::*;
    use crate::network_failure::NetworkFailurePhase;
    use crate::network_failure::NetworkPolicyRefusal;
    use crate::network_replay::NetworkReplayError;

    fn refusal() -> Error {
        engine_rpc_error(NetworkRpcError::from_engine(
            NetworkPolicy::Replay,
            NetworkFailurePhase::Binding,
            NetworkReplayError::NoMatchingChannel,
        ))
    }

    #[test]
    fn poll_readiness_review_adapter_error_hup_masks_are_unconditional_and_clear() {
        for bit in [libc::POLLERR, libc::POLLHUP] {
            for mask in [
                0,
                libc::POLLIN,
                libc::POLLRDHUP,
                libc::POLLIN | libc::POLLRDHUP,
            ] {
                let mut status = crate::network_replay::NetworkStreamQueueStatus {
                    consume_epoch: 0,
                    queued_bytes: 1,
                    eof: false,
                    local_read_shutdown: false,
                    readiness: NetworkReadinessV2 {
                        readable: false,
                        writable: false,
                        error: bit == libc::POLLERR,
                        hangup: bit == libc::POLLHUP,
                    },
                    error: None,
                    listener: false,
                    ingress_busy: false,
                    delivery_busy: false,
                };
                assert_eq!(shadow_readiness_to_poll_revents(&status, 8, mask), bit);
                status.readiness.error = false;
                status.readiness.hangup = false;
                assert_eq!(shadow_readiness_to_poll_revents(&status, 8, mask), 0);
            }
        }
    }

    #[test]
    fn shadow_poll_preserves_receive_half_close_masks_with_and_without_payload() {
        for queued_bytes in [0, 3] {
            for local in [false, true] {
                let status = crate::network_replay::NetworkStreamQueueStatus {
                    queued_bytes,
                    eof: !local,
                    local_read_shutdown: local,
                    readiness: NetworkReadinessV2 {
                        readable: false,
                        writable: true,
                        error: false,
                        hangup: false,
                    },
                    error: None,
                    listener: false,
                    ingress_busy: false,
                    delivery_busy: false,
                    consume_epoch: 0,
                };
                assert_eq!(
                    shadow_readiness_to_poll_revents(&status, 4096, libc::POLLRDHUP),
                    libc::POLLRDHUP
                );
                assert_eq!(
                    shadow_readiness_to_poll_revents(&status, 4096, libc::POLLIN | libc::POLLRDHUP),
                    libc::POLLIN | libc::POLLRDHUP
                );
                assert_eq!(
                    shadow_readiness_to_poll_revents(&status, 4096, libc::POLLOUT),
                    libc::POLLOUT
                );
                assert_eq!(shadow_readiness_to_poll_revents(&status, 4096, 0), 0);
                let mut open = status.clone();
                open.eof = false;
                open.local_read_shutdown = false;
                assert_eq!(
                    shadow_readiness_to_poll_revents(&open, 4096, libc::POLLIN | libc::POLLRDHUP),
                    0
                );
                assert_eq!(
                    shadow_readiness_to_poll_revents(&open, 1, libc::POLLRDHUP),
                    0
                );
            }
        }
    }

    #[test]
    fn primary_refusal_survives_independent_cleanup_failure() {
        let error = finish_shadow_operation::<()>(
            Err(refusal()),
            Err(engine_error("owned pin release failed")),
        )
        .unwrap_err();
        let Error::Tool(inner) = error else {
            panic!("typed refusal lost Tool boundary");
        };
        assert!(inner.downcast_ref::<NetworkPolicyRefusal>().is_some());
        assert!(format!("{inner:#}").contains("owned pin release failed"));
    }

    #[test]
    fn cleanup_refusal_does_not_reclassify_internal_primary() {
        let error = finish_shadow_operation::<()>(
            Err(engine_error("invalid physical drain")),
            Err(refusal()),
        )
        .unwrap_err();
        let Error::Tool(inner) = error else {
            panic!("internal error lost Tool boundary");
        };
        assert!(inner.downcast_ref::<NetworkPolicyRefusal>().is_none());
        assert!(format!("{inner:#}").contains("invalid physical drain"));
    }

    #[test]
    fn successful_cleanup_preserves_guest_errno_and_success() {
        assert!(
            matches!(finish_shadow_operation::<()>(Err(Errno::EFAULT.into()), Ok(())),
            Err(Error::Errno(errno)) if errno == Errno::EFAULT)
        );
        assert_eq!(finish_shadow_operation(Ok(37), Ok(())).unwrap(), 37);
        let Error::Tool(inner) = finish_shadow_operation::<()>(Ok(()), Err(refusal())).unwrap_err()
        else {
            panic!("sole cleanup refusal lost Tool boundary");
        };
        assert!(inner.downcast_ref::<NetworkPolicyRefusal>().is_some());
    }
}

// Current selected physical path. The remote-mapping version is retained in
// remote-mapping-draft/, not composed into this candidate.
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;

struct NetworkHostSocketPin {
    call: NetworkStreamCallId,
    fd: Option<OwnedFd>,
    nonblocking: bool,
}

impl NetworkHostSocketPin {
    fn physical_fd(&self) -> Result<RawFd, Error> {
        self.fd
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .ok_or_else(|| engine_error("Replay/socket call has no physical descriptor authority"))
    }
}

// Called only after explicit ptrace capability and short OFD/FD-slot admission.
// No await or scheduler yield occurs between these calls. PIDFD_THREAD selects
// THIS thread's files table, including CLONE_THREAD without CLONE_FILES.
fn duplicate_ptrace_socket(tid: i32, fd: i32) -> Result<OwnedFd, std::io::Error> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL as libc::c_uint) };
    if pidfd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as RawFd) };
    let duplicate = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0u32) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate as RawFd) };
    // pidfd_getfd itself specifies CLOEXEC. Check rather than create a window
    // in which a concurrently spawned process could inherit this reference.
    let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(std::io::Error::other("pidfd_getfd did not set CLOEXEC"));
    }
    Ok(duplicate)
}

impl<T: RecordOrReplay> Detcore<T> {
    async fn begin_host_stream_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        expected: OpenFileId,
        policy: NetworkPolicy,
    ) -> Result<NetworkHostSocketPin, Error> {
        if policy == NetworkPolicy::Record && !guest.config().backend_supports_host_socket_pin {
            // No numeric KVM/DBT scheduler ID ever reaches host pidfd_open.
            return Err(engine_error(
                "selected backend has no authenticated host socket-pin implementation",
            ));
        }
        let control = self
            .begin_shadow_fd_control(guest, fd)
            .await?
            .ok_or_else(|| engine_error("stream enrollment disappeared before pin admission"))?;
        if self.network_open_file(guest, fd) != Some(expected) {
            self.shadow_ack(
                guest,
                NetworkRequest::FinishSocketControl {
                    lease: control.lease,
                    disposition: NetworkSocketControlFinish::Unchanged,
                },
            )
            .await?;
            // This is an internal restart requirement, never an invented guest
            // EBADF. Full FD-slot admission must prevent this before activation.
            return Err(engine_error("FD slot changed before stream-call admission"));
        }
        let call = match network_request(
            guest,
            NetworkRequest::BeginStreamCall {
                control_lease: control.lease,
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::StreamCall(call) if call.open_file == expected => call,
            other => {
                return Err(engine_error(format!(
                    "unexpected stream call admission {other:?}"
                )));
            }
        };
        let nonblocking = guest
            .thread_state()
            .with_detfd(fd, |entry| entry.is_nonblocking())?;
        let duplicate = if call.physical_pin_required {
            if policy != NetworkPolicy::Record {
                return Err(engine_error("Replay requested a physical pin"));
            }
            // BeginStreamCall durably marked PinAcquireSubmitted first. The
            // stopped ptrace callback and FD-slot admission authenticate tid/fd.
            let result = duplicate_ptrace_socket(guest.tid().as_raw(), fd);
            match result {
                Ok(duplicate) => {
                    self.shadow_ack(
                        guest,
                        NetworkRequest::ConfirmStreamCallPin {
                            id: call.id,
                            outcome: NetworkStreamPinOutcome::Acquired,
                        },
                    )
                    .await?;
                    Some(duplicate)
                }
                Err(error) => {
                    let errno = error.raw_os_error().unwrap_or(libc::EIO);
                    self.shadow_ack(
                        guest,
                        NetworkRequest::ConfirmStreamCallPin {
                            id: call.id,
                            outcome: NetworkStreamPinOutcome::Failed(errno),
                        },
                    )
                    .await?;
                    self.shadow_ack(
                        guest,
                        NetworkRequest::FinishSocketControl {
                            lease: control.lease,
                            disposition: NetworkSocketControlFinish::Unchanged,
                        },
                    )
                    .await?;
                    return Err(engine_error(format!(
                        "cannot acquire physical stream reference: {error}"
                    )));
                }
            }
        } else {
            None
        };
        self.shadow_ack(
            guest,
            NetworkRequest::FinishSocketControl {
                lease: control.lease,
                disposition: NetworkSocketControlFinish::Unchanged,
            },
        )
        .await?;
        Ok(NetworkHostSocketPin {
            call: call.id,
            fd: duplicate,
            nonblocking,
        })
    }

    async fn finish_host_stream_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut pin: NetworkHostSocketPin,
        result: Result<i64, Error>,
    ) -> Result<i64, Error> {
        let cleanup = async {
            self.shadow_ack(
                guest,
                NetworkRequest::BeginStreamCallRelease { id: pin.call },
            )
            .await?;
            if let Some(fd) = pin.fd.take() {
                let raw = fd.into_raw_fd();
                let closed = unsafe { libc::close(raw) };
                // Linux releases a valid close descriptor even if a later error is
                // returned. EBADF means ownership itself failed and stays latched.
                if closed < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
                {
                    return Err(engine_error(
                        "owned socket pin became invalid before release",
                    ));
                }
            }
            self.shadow_ack(
                guest,
                NetworkRequest::FinishStreamCallRelease { id: pin.call },
            )
            .await?;
            Ok(())
        }
        .await;
        finish_shadow_operation(result, cleanup)
    }
}

// Linux v7.1 net/socket.c:2071-2105: invalid FD is resolved first by
// __sys_accept4; this allowed-mask check then precedes do_accept/FD_ADD.
// SockFlag retains unknown bits, so its Rust type is not validation.
fn checked_accept4_socket_type(flags: i32) -> Result<i32, Errno> {
    if flags & !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(libc::SOCK_STREAM | flags)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3174): production activation requires the independently
// qualified child observer and fd installation service. This is their actual
// syscall caller, not a post-hoc RegisterAccepted/default-copy shortcut.
impl<T: RecordOrReplay> Detcore<T> {
    async fn accepted_model_mode<G: Guest<Self>>(&self, guest: &mut G) -> Result<bool, Error> {
        match network_request(guest, NetworkRequest::AcceptedMode)
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::AcceptedMode(value) => Ok(value),
            reply => Err(engine_error(format!(
                "unexpected accepted-mode reply {reply:?}"
            ))),
        }
    }
    async fn accepted_socket_transaction<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
        listener: OpenFileId,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        // __sys_accept4 resolves the FD first; the network dispatcher has
        // already selected this tracked socket. __sys_accept4_file then checks
        // the raw flag mask before FD allocation, pointer copy or queue wait.
        let replay_socket_type = if policy == NetworkPolicy::Replay {
            Some(checked_accept4_socket_type(call.flags().bits())?)
        } else {
            None
        };
        let pin = self
            .begin_host_stream_call(guest, call.sockfd(), listener, policy)
            .await?;
        let work = async {
            let reservation = loop {
                if policy == NetworkPolicy::Replay {
                    let now = thread_observe_time(guest).await;
                    match network_request(guest, NetworkRequest::ReleaseEligible(now))
                        .await
                        .map_err(engine_rpc_error)?
                    {
                        NetworkReply::ReadyChannels(_) => {}
                        reply => {
                            return Err(engine_error(format!(
                                "unexpected accepted release reply {reply:?}"
                            )));
                        }
                    }
                }
                match network_request(
                    guest,
                    NetworkRequest::BeginAcceptedSocket { call: pin.call },
                )
                .await
                .map_err(engine_rpc_error)?
                {
                    NetworkReply::AcceptedChild(Some(child)) => break child,
                    NetworkReply::AcceptedChild(None) => {
                        if pin.nonblocking {
                            return Err(Errno::EAGAIN.into());
                        }
                        self.wait_for_network(guest, listener, NetworkWaitKind::Readable)
                            .await;
                    }
                    reply => {
                        return Err(engine_error(format!(
                            "unexpected accepted reservation {reply:?}"
                        )));
                    }
                }
            };
            self.shadow_ack(
                guest,
                NetworkRequest::SubmitAcceptedSocket {
                    lease: reservation.lease,
                },
            )
            .await?;
            let result: Result<i64, Error> = if policy == NetworkPolicy::Record {
                // Original guest address/length preserve Linux allocation and
                // copyout priority. No preliminary scratch accept or validation.
                self.live_network_syscall_with_completion(
                    guest,
                    call.into(),
                    NetworkPhysicalCompletion::Accept(reservation.lease),
                )
                .await
            } else {
                let child = reservation
                    .child
                    .as_ref()
                    .ok_or_else(|| engine_error("Replay lacks reserved child"))?;
                let socket = syscalls::Socket::new()
                    .with_family(child.key.domain)
                    .with_type(
                        replay_socket_type.expect("Replay flags validated before queue access"),
                    )
                    .with_protocol(child.key.protocol);
                let fd = guest.inject(socket).await?;
                // The allocation service must retain this actual descriptor
                // through any copyout fault; the errno is not a no-effect proof.
                write_accept_peer(guest, call, Some(&child.peer))?;
                Ok(fd)
            };
            let kernel_result = match &result {
                Ok(fd) => Ok(i32::try_from(*fd).map_err(|_| Errno::EIO)?),
                Err(Error::Errno(errno)) => Err(errno.into_raw()),
                Err(_) => return result,
            };
            if policy == NetworkPolicy::Record && kernel_result.is_ok() {
                self.shadow_ack(
                    guest,
                    NetworkRequest::ResolveAcceptedProvider {
                        lease: reservation.lease,
                    },
                )
                .await?;
            }
            if let Ok(fd) = kernel_result {
                self.add_fd(
                    guest,
                    fd,
                    OFlag::from_bits_truncate(call.flags().bits()),
                    FdType::Socket,
                )
                .await?;
            }
            // The service has to publish its matched descriptor fact BEFORE
            // this RPC. A return value/add_fd never fabricates that authority.
            let completion = network_request(
                guest,
                NetworkRequest::CompleteAcceptedSocket {
                    lease: reservation.lease,
                    kernel_result,
                    installed_open_file: kernel_result
                        .ok()
                        .map(|fd| guest.thread_state().socket_open_file_id(fd))
                        .transpose()?,
                },
            )
            .await
            .map_err(engine_rpc_error)?;
            match (kernel_result, completion) {
                (Ok(fd), NetworkReply::AcceptedCompletion(Some(done))) if done.fd == fd => result,
                (Err(_), NetworkReply::AcceptedCompletion(None)) => result,
                (_, reply) => Err(engine_error(format!(
                    "accepted completion differs from physical result: {reply:?}"
                ))),
            }
        }
        .await;
        self.finish_host_stream_call(guest, pin, work).await
    }
    pub(crate) async fn try_accepted_endpoint<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        address: Option<AddrMut<'_, libc::sockaddr>>,
        length: Option<AddrMut<'_, libc::socklen_t>>,
        peer: bool,
    ) -> Result<Option<i64>, Error> {
        if !matches!(
            guest.config().network_trace.policy,
            NetworkPolicy::Record | NetworkPolicy::Replay
        ) {
            return Ok(None);
        }
        let Some(open_file) = self.network_open_file(guest, fd) else {
            return Ok(None);
        };
        let endpoint =
            match network_request(guest, NetworkRequest::AcceptedEndpoint { open_file, peer })
                .await
                .map_err(engine_rpc_error)?
            {
                NetworkReply::AcceptedEndpoint(endpoint) => endpoint,
                reply => return Err(engine_error(format!("unexpected endpoint reply {reply:?}"))),
            };
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        write_accept_peer_memory(
            &mut guest.memory(),
            address.ok_or(Errno::EFAULT)?,
            length.ok_or(Errno::EFAULT)?,
            Some(&endpoint),
        )?;
        Ok(Some(0))
    }
}

fn host_socket_i32(fd: RawFd, name: i32) -> Result<i32, Errno> {
    let mut value = 0i32;
    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &raw mut length,
        )
    };
    if result < 0 {
        return Err(Errno::new(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        ));
    }
    if length != std::mem::size_of::<i32>() as libc::socklen_t {
        return Err(Errno::EIO);
    }
    Ok(value)
}

fn host_set_cursor(fd: RawFd, value: i32) -> Result<(), Errno> {
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEEK_OFF,
            (&raw const value).cast(),
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if result < 0 {
        Err(Errno::new(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        ))
    } else {
        Ok(())
    }
}

fn host_peek_suffix(fd: RawFd, prefix: usize) -> Result<(usize, Vec<u8>), Errno> {
    let maximum = prefix
        .checked_add(SHADOW_UNIT)
        .filter(|count| *count <= NETWORK_MAX_RW_COUNT)
        .ok_or(Errno::EOVERFLOW)?;
    let vectors = prefix
        .div_ceil(SHADOW_VIEW)
        .min(libc::UIO_MAXIOV as usize - 1);
    let sink_size = if vectors == 0 {
        0
    } else {
        prefix.div_ceil(vectors)
    };
    let mut sink = vec![0u8; sink_size];
    let mut suffix = vec![0u8; SHADOW_UNIT];
    let mut iov = Vec::with_capacity(vectors + 1);
    let mut left = prefix;
    for _ in 0..vectors {
        let count = left.min(sink_size);
        iov.push(libc::iovec {
            iov_base: sink.as_mut_ptr().cast(),
            iov_len: count,
        });
        left -= count;
    }
    iov.push(libc::iovec {
        iov_base: suffix.as_mut_ptr().cast(),
        iov_len: SHADOW_UNIT,
    });
    let mut header = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: iov.as_mut_ptr(),
        msg_iovlen: iov.len(),
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let result = unsafe { libc::recvmsg(fd, &raw mut header, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    if result < 0 {
        return Err(Errno::new(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        ));
    }
    let count = result as usize;
    if count < prefix || count > maximum {
        return Err(Errno::EIO);
    }
    suffix.truncate(count - prefix);
    Ok((count, suffix))
}

fn host_poll_state(fd: RawFd) -> Result<i16, Errno> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLOUT | libc::POLLRDHUP,
        revents: 0,
    };
    let result = unsafe { libc::poll(&raw mut pollfd, 1, 0) };
    if result < 0 {
        return Err(Errno::new(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        ));
    }
    if !(0..=1).contains(&result) || pollfd.revents & libc::POLLNVAL != 0 {
        return Err(Errno::EIO);
    }
    Ok(pollfd.revents)
}

fn host_queued_bytes(fd: RawFd) -> Result<usize, Errno> {
    let mut count = 0i32;
    let result = unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut count) };
    if result < 0 {
        return Err(Errno::new(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        ));
    }
    usize::try_from(count).map_err(|_| Errno::EIO)
}

const SHADOW_UNIT: usize = 1024;
const SHADOW_VIEW: usize = 512;
const NETWORK_MAX_RW_COUNT: usize = 0x7fff_f000;

struct NetworkShadowObservation {
    revents: i16,
    published_through: u64,
    observed_through: u64,
}

impl<T: RecordOrReplay> Detcore<T> {
    async fn shadow_ack<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: NetworkRequest,
    ) -> Result<(), Error> {
        match network_request(guest, request)
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::Unit => Ok(()),
            other => Err(engine_error(format!(
                "unexpected shadow acknowledgement {other:?}"
            ))),
        }
    }
    async fn shadow_submit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        lease: NetworkStreamLeaseId,
        effect: NetworkStreamPhysicalEffect,
    ) -> Result<(), Error> {
        self.shadow_ack(
            guest,
            NetworkRequest::SubmitStreamPhysical { lease, effect },
        )
        .await
    }
    async fn shadow_confirm<G: Guest<Self>>(
        &self,
        guest: &mut G,
        lease: NetworkStreamLeaseId,
        result: NetworkStreamPhysicalResult,
    ) -> Result<(), Error> {
        self.shadow_ack(
            guest,
            NetworkRequest::ConfirmStreamPhysical { lease, result },
        )
        .await
    }
    async fn set_host_shadow_cursor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
        lease: NetworkStreamLeaseId,
        value: i32,
    ) -> Result<(), Error> {
        self.shadow_submit(
            guest,
            lease,
            NetworkStreamPhysicalEffect::SetPeekOffset { value },
        )
        .await?;
        let result = host_set_cursor(pin.physical_fd()?, value);
        let confirmation = match result {
            Ok(()) => NetworkStreamPhysicalResult::Unit,
            Err(errno) => NetworkStreamPhysicalResult::Errno(errno.into_raw()),
        };
        self.shadow_confirm(guest, lease, confirmation).await?;
        result.map_err(|error| engine_error(format!("physical cursor transition failed: {error}")))
    }

    async fn observe_shadow_stream<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
    ) -> Result<i16, Error> {
        let mut observation = self.observe_shadow_unit(guest, pin).await?;
        // This horizon is fixed by the first ordered kernel queue observation,
        // not by guest length, pointer, seed, low-water, or later arrivals.
        let horizon = observation.observed_through;
        while observation.published_through < horizon {
            let previous = observation.published_through;
            observation = self.observe_shadow_unit(guest, pin).await?;
            if observation.published_through <= previous {
                return Err(engine_error(
                    "physical shadow failed to reach its observed ingress horizon",
                ));
            }
        }
        Ok(observation.revents)
    }

    async fn observe_shadow_unit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
    ) -> Result<NetworkShadowObservation, Error> {
        let probe =
            match network_request(guest, NetworkRequest::BeginShadowProbe { call: pin.call })
                .await
                .map_err(engine_rpc_error)?
            {
                NetworkReply::ShadowProbe(probe) => probe,
                other => return Err(engine_error(format!("unexpected shadow probe {other:?}"))),
            };
        let lease = probe.lease;
        let fd = pin.physical_fd()?;
        self.shadow_submit(guest, lease, NetworkStreamPhysicalEffect::ReadPeekOffset)
            .await?;
        let saved = match host_socket_i32(fd, libc::SO_PEEK_OFF) {
            Ok(value) => {
                self.shadow_confirm(guest, lease, NetworkStreamPhysicalResult::PeekOffset(value))
                    .await?;
                Some(value)
            }
            Err(errno) if errno == Errno::ENOPROTOOPT || errno == Errno::EOPNOTSUPP => {
                self.shadow_confirm(
                    guest,
                    lease,
                    NetworkStreamPhysicalResult::Errno(errno.into_raw()),
                )
                .await?;
                None
            }
            Err(errno) => {
                self.shadow_confirm(
                    guest,
                    lease,
                    NetworkStreamPhysicalResult::Errno(errno.into_raw()),
                )
                .await?;
                return Err(engine_error(format!(
                    "owned socket cursor query failed: {errno}"
                )));
            }
        };
        if saved.is_some_and(|value| value >= 0) {
            self.set_host_shadow_cursor(guest, pin, lease, -1).await?;
        }
        let maximum = probe
            .retained_prefix
            .checked_add(SHADOW_UNIT)
            .ok_or_else(|| engine_error("shadow prefix overflow"))?;
        self.shadow_submit(guest, lease, NetworkStreamPhysicalEffect::Peek { maximum })
            .await?;
        let peek = host_peek_suffix(fd, probe.retained_prefix);
        let confirmation = match &peek {
            Ok((count, _)) => NetworkStreamPhysicalResult::Peeked { count: *count },
            Err(errno) => NetworkStreamPhysicalResult::Errno(errno.into_raw()),
        };
        self.shadow_confirm(guest, lease, confirmation).await?;
        // The restoration is attempted before any interpretation of known
        // recv errors. Unknown RPC/effect completion remains durably unresolved.
        if let Some(value) = saved.filter(|value| *value >= 0) {
            self.set_host_shadow_cursor(guest, pin, lease, value)
                .await?;
        }
        let bytes = match peek {
            Ok((_, bytes)) => bytes,
            Err(errno)
                if probe.retained_prefix == 0
                    && errno != Errno::EFAULT
                    && errno != Errno::EBADF
                    && errno != Errno::EIO =>
            {
                Vec::new()
            }
            Err(errno) => {
                return Err(engine_error(format!(
                    "owned shadow lost a published prefix or copy: {errno}"
                )));
            }
        };
        self.shadow_submit(guest, lease, NetworkStreamPhysicalEffect::PollState)
            .await?;
        let revents = host_poll_state(fd)
            .map_err(|error| engine_error(format!("owned poll0 failed: {error}")))?;
        self.shadow_confirm(
            guest,
            lease,
            NetworkStreamPhysicalResult::PollState { revents },
        )
        .await?;
        self.shadow_submit(guest, lease, NetworkStreamPhysicalEffect::QueuedBytes)
            .await?;
        let queued = host_queued_bytes(fd)
            .map_err(|error| engine_error(format!("owned FIONREAD failed: {error}")))?;
        self.shadow_confirm(
            guest,
            lease,
            NetworkStreamPhysicalResult::QueuedBytes { count: queued },
        )
        .await?;
        // sk_err/soft-error observations are core-owned effect transitions;
        // a plain readiness bit never fabricates an errno or clears a slot.
        let unseen = queued.checked_sub(probe.retained_prefix).ok_or_else(|| {
            engine_error("physical queued bytes fell below the protected shadow prefix")
        })?;
        if unseen < bytes.len() {
            return Err(engine_error(
                "physical queued bytes fell below the just-observed suffix",
            ));
        }
        let observed_through = probe
            .captured_through
            .checked_add(
                u64::try_from(unseen)
                    .map_err(|_| engine_error("observed ingress horizon does not fit u64"))?,
            )
            .ok_or_else(|| engine_error("observed ingress horizon overflow"))?;
        let published_through = probe
            .captured_through
            .checked_add(
                u64::try_from(bytes.len())
                    .map_err(|_| engine_error("published suffix does not fit u64"))?,
            )
            .ok_or_else(|| engine_error("published ingress frontier overflow"))?;
        let eof =
            !probe.local_read_shutdown && revents & libc::POLLRDHUP != 0 && unseen == bytes.len();
        self.shadow_ack(
            guest,
            NetworkRequest::CompleteShadowProbe { lease, bytes, eof },
        )
        .await?;
        Ok(NetworkShadowObservation {
            revents,
            published_through,
            observed_through,
        })
    }

    async fn drain_shadow_selection<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
        lease: NetworkStreamLeaseId,
        selection_len: usize,
    ) -> Result<(), Error> {
        self.shadow_ack(guest, NetworkRequest::BeginRecordDrain { lease })
            .await?;
        let fd = pin.physical_fd()?;
        let mut drained = 0;
        while drained < selection_len {
            let maximum = (selection_len - drained).min(SHADOW_VIEW);
            self.shadow_submit(guest, lease, NetworkStreamPhysicalEffect::Drain { maximum })
                .await?;
            let mut bytes = vec![0u8; maximum];
            let result =
                unsafe { libc::recv(fd, bytes.as_mut_ptr().cast(), maximum, libc::MSG_DONTWAIT) };
            if result < 0 {
                let errno = std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO);
                self.shadow_confirm(guest, lease, NetworkStreamPhysicalResult::Errno(errno))
                    .await?;
                return Err(engine_error(format!(
                    "physical stream drain failed: errno {errno}"
                )));
            }
            let count = result as usize;
            if count == 0 || count > maximum {
                return Err(engine_error("physical stream drain stopped short"));
            }
            bytes.truncate(count);
            // This confirms physical bytes against the immutable reservation;
            // it does not yet consume any logical prefix of the selected unit.
            self.shadow_confirm(guest, lease, NetworkStreamPhysicalResult::Drained { bytes })
                .await?;
            drained += count;
        }
        self.shadow_ack(guest, NetworkRequest::FinishRecordDrain { lease })
            .await
    }

    async fn finish_record_peek<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
        lease: NetworkStreamLeaseId,
        selection_len: usize,
    ) -> Result<(), Error> {
        let state = self.shadow_call_socket_state(guest, pin.call).await?;
        if let Some(value) = state.options.peek_offset.filter(|value| *value >= 0) {
            // Match sk_peek_offset_fwd's signed-int transition. The core derives
            // the same desired cursor from this receipt, not a caller guess.
            let next = value
                .wrapping_add(
                    i32::try_from(selection_len)
                        .map_err(|_| engine_error("peek selection exceeds signed ABI"))?,
                )
                .max(0);
            self.set_host_shadow_cursor(guest, pin, lease, next).await?;
        }
        self.finish_shadow_chunk(guest, lease, NetworkStreamChunkDisposition::Peeked)
            .await
    }
}

// A transient run-local namespace identity, never a portable trace identity.
// The explicit dispatch capability is checked before the caller supplies tid.
fn authenticated_stream_namespace(physical_tid: i32) -> Result<NetworkStreamNamespace, Error> {
    use std::os::unix::fs::MetadataExt;
    let path = format!("/proc/{physical_tid}/ns/net");
    let pinned = std::fs::File::open(&path).map_err(Error::Io)?;
    let identity = pinned.metadata().map_err(Error::Io)?;
    let again = std::fs::File::open(&path).map_err(Error::Io)?;
    let current = again.metadata().map_err(Error::Io)?;
    if (identity.dev(), identity.ino()) != (current.dev(), current.ino()) {
        return Err(engine_error(
            "guest network namespace changed while enrolling socket",
        ));
    }
    Ok(NetworkStreamNamespace {
        device: identity.dev(),
        inode: identity.ino(),
    })
}

// Record-only namespace bootstrap. The scratch descriptor is in the recorder's
// table, never the guest's. It is never connected, bound, or exposed to the guest.
// A different namespace is an explicit integration failure, not host fallback.
fn record_receive_normalization(
    physical_tid: i32,
    key: StreamSocketKeyV3,
) -> Result<(LinuxReceiveNormalizationV3, bool), Error> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::MetadataExt;
    let namespace = |path: &str| -> Result<std::fs::File, Error> {
        std::fs::File::open(path).map_err(Error::Io)
    };
    let identity = |file: &std::fs::File| -> Result<(u64, u64), Error> {
        let stat = file.metadata().map_err(Error::Io)?;
        Ok((stat.dev(), stat.ino()))
    };
    let guest_path = format!("/proc/{physical_tid}/ns/net");
    let guest_ns = namespace(&guest_path)?;
    let recorder_ns = namespace("/proc/thread-self/ns/net")?;
    if identity(&guest_ns)? != identity(&recorder_ns)? {
        return Err(engine_error(
            "V3 profile bootstrap needs a namespace-owned helper; host defaults are not authoritative",
        ));
    }
    // No await between pinning the current thread namespace and these syscalls.
    let raw = unsafe {
        libc::socket(
            key.domain,
            key.socket_type | libc::SOCK_CLOEXEC,
            key.protocol,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful socket returned one uniquely owned descriptor.
    let scratch = unsafe { OwnedFd::from_raw_fd(raw) };
    let fd = scratch.as_raw_fd();
    let scalar_get = |name: i32| -> Result<i32, Error> {
        let mut value = 0i32;
        let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw mut value).cast(),
                &raw mut length,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if length != std::mem::size_of::<i32>() as libc::socklen_t {
            return Err(engine_error("profile scratch scalar size mismatch"));
        }
        Ok(value)
    };
    let scalar_set = |name: i32, value: i32| -> std::io::Result<()> {
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw const value).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    let timeout_set = |seconds: i64, microseconds: i64| -> Result<(), Error> {
        let value = libc::timeval {
            tv_sec: seconds,
            tv_usec: microseconds,
        };
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const value).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    };
    let timeout_get = || -> Result<(i64, i64), Error> {
        let mut value = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut length = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw mut value).cast(),
                &raw mut length,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if length != std::mem::size_of::<libc::timeval>() as libc::socklen_t {
            return Err(engine_error("profile scratch timeout size mismatch"));
        }
        Ok((value.tv_sec, value.tv_usec))
    };
    let read_limits = || -> Result<(u32, u32), Error> {
        fn values(path: &str) -> Result<Vec<u32>, Error> {
            let text = std::fs::read_to_string(path).map_err(Error::Io)?;
            text.split_whitespace()
                .map(|part| {
                    part.parse::<u32>()
                        .map_err(|error| engine_error(format!("invalid {path}: {error}")))
                })
                .collect()
        }
        let system = values("/proc/sys/net/core/rmem_max")?;
        let namespace = values("/proc/sys/net/ipv4/tcp_rmem")?;
        if system.len() != 1 || namespace.len() != 3 {
            return Err(engine_error("receive buffer sysctl shape mismatch"));
        }
        Ok((system[0], namespace[2]))
    };
    let before_limits = read_limits()?;
    timeout_set(0, 1)?;
    let (seconds, microseconds) = timeout_get()?;
    let hz = LinuxReceiveHzV3::from_one_microsecond_probe(seconds, microseconds)
        .ok_or_else(|| engine_error("kernel timeout rounding is outside the audited profile"))?;
    // This socket is private, so irreversible RCVBUF_LOCK is harmless here.
    scalar_set(libc::SO_RCVBUF, 0).map_err(Error::Io)?;
    let minimum_receive_buffer = u32::try_from(scalar_get(libc::SO_RCVBUF)?)
        .map_err(|_| engine_error("negative minimum receive buffer"))?;
    let mut normalization = LinuxReceiveNormalizationV3 {
        hz,
        system_rmem_max: before_limits.0,
        namespace_tcp_rmem_max: before_limits.1,
        minimum_receive_buffer,
        peek_offset_set_supported: false,
    };
    normalization
        .validate()
        .map_err(|error| engine_error(format!("invalid receive profile: {error:?}")))?;
    // Check the HZ300 integer-division boundary rather than infer a whole
    // normalization law from only the one-microsecond witness.
    for (seconds, microseconds) in [(0, 999_999), (-1, 0), (0, 0)] {
        timeout_set(seconds, microseconds)?;
        let expected = normalization
            .normalize_timeout(seconds, microseconds)
            .map_err(|error| engine_error(format!("timeout normalization: {error:?}")))?
            .exposed_timeval(hz);
        if timeout_get()? != expected {
            return Err(engine_error("kernel timeout profile witness mismatch"));
        }
    }
    let peek_offset_set_supported = match scalar_set(libc::SO_PEEK_OFF, 0) {
        Ok(()) => {
            if scalar_get(libc::SO_PEEK_OFF)? != 0 {
                return Err(engine_error("scratch peek cursor mismatch"));
            }
            true
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::ENOPROTOOPT)
            ) =>
        {
            false
        }
        Err(error) => return Err(Error::Io(error)),
    };
    normalization.peek_offset_set_supported = peek_offset_set_supported;
    if before_limits != read_limits()?
        || identity(&guest_ns)? != identity(&namespace(&guest_path)?)?
        || identity(&recorder_ns)? != identity(&namespace("/proc/thread-self/ns/net")?)?
    {
        return Err(engine_error(
            "receive profile namespace or sysctl changed during bootstrap",
        ));
    }
    // OwnedFd drops on every success/error path. No socket network operation ran.
    Ok((normalization, peek_offset_set_supported))
}

impl<T: RecordOrReplay> Detcore<T> {
    async fn shadow_mode<G: Guest<Self>>(&self, guest: &mut G) -> Result<bool, Error> {
        match network_request(guest, NetworkRequest::ShadowMode)
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::ShadowMode(value) => Ok(value),
            other => Err(engine_error(format!("unexpected shadow mode {other:?}"))),
        }
    }

    async fn enroll_fresh_stream_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        call: syscalls::Socket,
    ) -> Result<(), Error> {
        if !self.shadow_mode(guest).await? {
            return Ok(());
        }
        if !matches!(call.family(), libc::AF_INET | libc::AF_INET6)
            || call.r#type() & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) != libc::SOCK_STREAM
            || !matches!(call.protocol(), 0 | libc::IPPROTO_TCP)
        {
            return Ok(());
        }
        let key = StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: call.family(),
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        };
        let open_file = guest.thread_state().socket_open_file_id(fd)?;
        if !guest.config().backend_supports_host_socket_pin {
            return Err(engine_error(
                "selected backend has no authenticated socket-namespace implementation",
            ));
        }
        let namespace = authenticated_stream_namespace(guest.tid().as_raw())?;
        let observed_profile = if guest.config().network_trace.policy == NetworkPolicy::Record {
            // The caller checked the explicit ptrace capability before this
            // physical task identity is used; no numeric-ID inference.
            let physical_tid = guest.tid().as_raw();
            let (normalization, _) = record_receive_normalization(physical_tid, key)?;
            let actual_key = StreamSocketKeyV3 {
                transport: NetworkTransportV2::Tcp,
                domain: read_socket_i32(guest, fd, libc::SO_DOMAIN).await?,
                socket_type: read_socket_i32(guest, fd, libc::SO_TYPE).await?,
                protocol: read_socket_i32(guest, fd, libc::SO_PROTOCOL).await?,
            };
            if actual_key != key {
                return Err(engine_error(
                    "fresh socket class differs from creation request",
                ));
            }
            let low_water = read_socket_i32(guest, fd, libc::SO_RCVLOWAT).await?;
            let receive_buffer = read_socket_i32(guest, fd, libc::SO_RCVBUF).await?;
            let peek_offset = match read_socket_i32(guest, fd, libc::SO_PEEK_OFF).await {
                Ok(value) => Some(value),
                Err(Error::Errno(errno))
                    if errno == Errno::ENOPROTOOPT || errno == Errno::EOPNOTSUPP =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            let timeout = read_socket_timeval(guest, fd, libc::SO_RCVTIMEO).await?;
            if timeout != (0, 0) {
                return Err(engine_error(
                    "fresh TCP receive timeout is not the declared default",
                ));
            }
            Some(FreshStreamSocketProfileV3 {
                key,
                normalization,
                initial: StreamSocketOptionsV3 {
                    peek_offset,
                    receive_low_water: u32::try_from(low_water)
                        .ok()
                        .filter(|value| *value != 0)
                        .ok_or_else(|| engine_error("invalid fresh TCP low-water"))?,
                    // Fresh socket state, not inference from a mutated getter.
                    receive_timeout: ReceiveTimeoutV3::Infinite,
                    receive_buffer: ReceiveBufferStateV3 {
                        bytes: u32::try_from(receive_buffer)
                            .map_err(|_| engine_error("negative fresh receive buffer"))?,
                        user_locked: false,
                        tcp_scaling_ratio: 128,
                    },
                },
            })
        } else {
            None
        };
        if namespace != authenticated_stream_namespace(guest.tid().as_raw())? {
            return Err(engine_error(
                "guest network namespace changed during socket profile capture",
            ));
        }
        if self.accepted_model_mode(guest).await? {
            let observed = if guest.config().network_trace.policy == NetworkPolicy::Record {
                if read_socket_timeval(guest, fd, libc::SO_SNDTIMEO).await? != (0, 0) {
                    return Err(engine_error(
                        "fresh TCP send timeout differs from declared default",
                    ));
                }
                Some(ReceiveTimeoutV3::Infinite)
            } else {
                None
            };
            self.shadow_ack(
                guest,
                NetworkRequest::RegisterAcceptedFreshSend { key, observed },
            )
            .await?;
        }
        match network_request(
            guest,
            NetworkRequest::RegisterStreamSocket {
                open_file,
                key,
                namespace,
                observed_profile,
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::StreamSocketState(Some(_)) => Ok(()),
            other => Err(engine_error(format!(
                "fresh TCP enrollment failed: {other:?}"
            ))),
        }
    }
}

async fn read_socket_timeval<G, T>(guest: &mut G, fd: i32, name: i32) -> Result<(i64, i64), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut stack = guest.stack().await;
    let value = stack.reserve::<libc::timeval>();
    let length = stack.reserve::<libc::socklen_t>();
    let _guard = stack.commit()?;
    let expected = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
    guest.memory().write_value(length, &expected)?;
    guest
        .inject(
            syscalls::Getsockopt::new()
                .with_fd(fd)
                .with_level(libc::SOL_SOCKET)
                .with_optname(name)
                .with_optval(Some(value.cast()))
                .with_optlen(Some(length)),
        )
        .await?;
    if guest.memory().read_value(length)? != expected {
        return Err(engine_error("socket timeout size mismatch"));
    }
    let value = guest.memory().read_value(value)?;
    Ok((value.tv_sec, value.tv_usec))
}

// Snapshot exactly the fields consumed by Linux's SOL_SOCKET setter. The first
// scalar read precedes timeout's larger length check in sk_setsockopt; the
// timeout helper then copies its complete timeval independently.
struct ShadowSocketOptionArgument {
    option: NetworkStreamSocketOption,
    bytes: [u8; 16],
}

fn snapshot_shadow_socket_option<M: MemoryAccess>(
    memory: &M,
    call: syscalls::Setsockopt,
) -> Result<Option<ShadowSocketOptionArgument>, Error> {
    if call.level() != libc::SOL_SOCKET
        || !matches!(
            call.optname(),
            libc::SO_PEEK_OFF
                | libc::SO_RCVLOWAT
                | libc::SO_RCVTIMEO
                | libc::SO_SNDTIMEO
                | libc::SO_RCVBUF
                | libc::SO_RCVBUFFORCE
        )
    {
        return Ok(None);
    }
    // Linux treats the outer syscall length as signed before any user copy.
    if call.optlen() > i32::MAX as u32 || call.optlen() < 4 {
        return Err(Errno::EINVAL.into());
    }
    let address = call.optval().ok_or(Errno::EFAULT)?.cast::<u8>();
    let mut bytes = [0; 16];
    memory.read_exact_with_user_access(address, &mut bytes[..4])?;
    let value = i32::from_ne_bytes(bytes[..4].try_into().expect("four-byte field"));
    let option = match call.optname() {
        libc::SO_PEEK_OFF => NetworkStreamSocketOption::PeekOffset(value),
        libc::SO_RCVLOWAT => NetworkStreamSocketOption::ReceiveLowWater(value),
        libc::SO_RCVBUF => NetworkStreamSocketOption::ReceiveBuffer(value),
        libc::SO_RCVBUFFORCE => NetworkStreamSocketOption::ForcedReceiveBuffer(value),
        libc::SO_RCVTIMEO | libc::SO_SNDTIMEO => {
            if call.optlen() < 16 {
                return Err(Errno::EINVAL.into());
            }
            memory.read_exact_with_user_access(address, &mut bytes)?;
            let seconds = i64::from_ne_bytes(bytes[..8].try_into().expect("eight-byte seconds"));
            let microseconds =
                i64::from_ne_bytes(bytes[8..].try_into().expect("eight-byte useconds"));
            if call.optname() == libc::SO_SNDTIMEO {
                NetworkStreamSocketOption::SendTimeout {
                    seconds,
                    microseconds,
                }
            } else {
                NetworkStreamSocketOption::ReceiveTimeout {
                    seconds,
                    microseconds,
                }
            }
        }
        _ => unreachable!(),
    };
    Ok(Some(ShadowSocketOptionArgument { option, bytes }))
}

impl<T: RecordOrReplay> Detcore<T> {
    pub(crate) async fn try_shadow_setsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsockopt,
    ) -> Result<Option<i64>, Error> {
        if call.level() == libc::SOL_SOCKET
            && call.optname() == libc::SO_SNDTIMEO
            && !self.accepted_model_mode(guest).await?
        {
            return Ok(None);
        }
        let Some(control) = self.begin_shadow_fd_control(guest, call.fd()).await? else {
            return Ok(None);
        };
        let work = async {
            let Some(argument) = snapshot_shadow_socket_option(&guest.memory(), call)? else {
                // Other options retain the existing guest-context operation
                // under the same exclusion. Other receive-affecting options still need
                // their own modeled authority before full feature qualification.
                return self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await;
            };
            let policy = guest.config().network_trace.policy;
            // RCVBUFFORCE must execute in the actual guest context in both
            // modes: namespace capability/security admission is not a tracer
            // privilege or a value inferred from the placeholder's buffer.
            let needs_guest_admission = matches!(
                argument.option,
                NetworkStreamSocketOption::ForcedReceiveBuffer(_)
            );
            let modeled_result = if policy == NetworkPolicy::Replay && !needs_guest_admission {
                match network_request(
                    guest,
                    NetworkRequest::PreviewSocketOption {
                        lease: control.lease,
                        option: argument.option.clone(),
                    },
                )
                .await
                .map_err(engine_rpc_error)?
                {
                    NetworkReply::SocketOptionResult(result) => Some(result),
                    other => {
                        return Err(engine_error(format!("unexpected option preview {other:?}")));
                    }
                }
            } else {
                None
            };
            self.shadow_submit(
                guest,
                control.lease,
                NetworkStreamPhysicalEffect::SetSocketOption {
                    option: argument.option,
                },
            )
            .await?;
            let result: Result<i64, Error> = if let Some(result) = modeled_result {
                // Replay applies the same typed transition, but this is a
                // modeled effect: no placeholder socket is consulted.
                result.map(|()| 0).map_err(|errno| Errno::new(errno).into())
            } else {
                // Snapshotting the input once avoids a second guest-memory
                // read racing the value used by the model. The original fd,
                // option, option length and guest credentials are preserved.
                let mut stack = guest.stack().await;
                let snapshot = stack.push(argument.bytes);
                let _guard = stack.commit()?;
                guest
                    .inject(call.with_optval(Some(snapshot.cast())))
                    .await
                    .map_err(Error::from)
            };
            let confirmed = match &result {
                Ok(0) => Ok(()),
                Ok(other) => {
                    return Err(engine_error(format!(
                        "setsockopt returned unexpected {other}"
                    )));
                }
                Err(Error::Errno(errno)) => Err(errno.into_raw()),
                // Unknown backend/tool completion stays submitted. Do not
                // fabricate a guest errno or acknowledge the effect on cleanup.
                Err(_) => return result,
            };
            let completion = self
                .shadow_confirm(
                    guest,
                    control.lease,
                    NetworkStreamPhysicalResult::SocketOption { result: confirmed },
                )
                .await;
            finish_shadow_operation(result, completion)
        }
        .await;
        let finish = self
            .shadow_ack(
                guest,
                NetworkRequest::FinishSocketControl {
                    lease: control.lease,
                    disposition: NetworkSocketControlFinish::Unchanged,
                },
            )
            .await;
        finish_shadow_operation(work, finish).map(Some)
    }
}

// The same receipt family serializes option access with probe/delivery/drain.
// Ordinary unmodeled option handlers keep their existing behavior inside this
// guard; the integration inventory records mutations that still need modeling.
impl<T: RecordOrReplay> Detcore<T> {
    pub(crate) async fn begin_shadow_fd_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<Option<NetworkSocketControl>, Error> {
        if !matches!(
            guest.config().network_trace.policy,
            NetworkPolicy::Record | NetworkPolicy::Replay
        ) {
            return Ok(None);
        }
        loop {
            let Some(open_file) = self.network_open_file(guest, fd) else {
                return Ok(None);
            };
            if self.shadow_socket_state(guest, open_file).await?.is_none() {
                return Ok(None);
            }
            let control =
                match network_request(guest, NetworkRequest::BeginSocketControl { open_file })
                    .await
                    .map_err(engine_rpc_error)?
                {
                    NetworkReply::SocketControl(control) => control,
                    other => {
                        return Err(engine_error(format!("unexpected socket control {other:?}")));
                    }
                };
            if self.network_open_file(guest, fd) == Some(open_file) {
                return Ok(Some(control));
            }
            // Acquiring an OFD receipt can wait. Never use it against a replaced
            // numeric descriptor. Retry the new lookup with no receipt held.
            self.shadow_ack(
                guest,
                NetworkRequest::FinishSocketControl {
                    lease: control.lease,
                    disposition: NetworkSocketControlFinish::Unchanged,
                },
            )
            .await?;
        }
    }

    pub(crate) async fn finish_shadow_fd_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        control: Option<NetworkSocketControl>,
        result: Result<i64, Error>,
    ) -> Result<i64, Error> {
        let Some(control) = control else {
            return result;
        };
        let finish = self
            .shadow_ack(
                guest,
                NetworkRequest::FinishSocketControl {
                    lease: control.lease,
                    disposition: NetworkSocketControlFinish::Unchanged,
                },
            )
            .await;
        finish_shadow_operation(result, finish)
    }

    pub(crate) async fn try_shadow_getsockopt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getsockopt,
    ) -> Result<Option<i64>, Error> {
        if !matches!(
            guest.config().network_trace.policy,
            NetworkPolicy::Record | NetworkPolicy::Replay
        ) {
            return Ok(None);
        }
        if call.level() != libc::SOL_SOCKET
            || !matches!(
                call.optname(),
                libc::SO_RCVLOWAT
                    | libc::SO_RCVTIMEO
                    | libc::SO_SNDTIMEO
                    | libc::SO_PEEK_OFF
                    | libc::SO_RCVBUF
                    | libc::SO_ERROR
                    | libc::SO_DOMAIN
                    | libc::SO_TYPE
                    | libc::SO_PROTOCOL
            )
        {
            return Ok(None);
        }
        if call.optname() == libc::SO_SNDTIMEO && !self.accepted_model_mode(guest).await? {
            return Ok(None);
        }
        let Some(open_file) = self.network_open_file(guest, call.fd()) else {
            return Ok(None);
        };
        if self.shadow_socket_state(guest, open_file).await?.is_none() {
            return Ok(None);
        }
        let Some(control) = self.begin_shadow_fd_control(guest, call.fd()).await? else {
            return Ok(None);
        };
        let mut took_error = false;
        let work = async {
            let actual_open_file = guest.thread_state().socket_open_file_id(call.fd())?;
            let initial = self
                .shadow_socket_state(guest, actual_open_file)
                .await?
                .ok_or_else(|| engine_error("enrolled option state disappeared under control"))?;
            // sk_getsockopt copies signed socklen BEFORE consuming SO_ERROR.
            let length_address = call.optlen().ok_or(Errno::EFAULT)?;
            let mut length_bytes = [0; 4];
            guest
                .memory()
                .read_exact_with_user_access(length_address.cast(), &mut length_bytes)?;
            let capacity =
                usize::try_from(i32::from_ne_bytes(length_bytes)).map_err(|_| Errno::EINVAL)?;
            let options = control
                .options
                .as_ref()
                .ok_or_else(|| engine_error("enrolled socket lacks options"))?;
            let bytes = match call.optname() {
                libc::SO_DOMAIN => initial.key.domain.to_ne_bytes().to_vec(),
                libc::SO_TYPE => initial.key.socket_type.to_ne_bytes().to_vec(),
                libc::SO_PROTOCOL => initial.key.protocol.to_ne_bytes().to_vec(),
                libc::SO_RCVLOWAT => (options.receive_low_water as i32).to_ne_bytes().to_vec(),
                libc::SO_RCVBUF => (options.receive_buffer.bytes as i32).to_ne_bytes().to_vec(),
                libc::SO_PEEK_OFF => options
                    .peek_offset
                    .ok_or(Errno::ENOPROTOOPT)?
                    .to_ne_bytes()
                    .to_vec(),
                libc::SO_RCVTIMEO | libc::SO_SNDTIMEO => {
                    let timeout = if call.optname() == libc::SO_SNDTIMEO {
                        initial.send_timeout.ok_or_else(|| {
                            engine_error("send timeout lacks explicit model authority")
                        })?
                    } else {
                        options.receive_timeout
                    };
                    let (seconds, microseconds) = timeout.exposed_timeval(initial.normalization.hz);
                    let mut bytes = seconds.to_ne_bytes().to_vec();
                    bytes.extend_from_slice(&microseconds.to_ne_bytes());
                    bytes
                }
                libc::SO_ERROR => {
                    took_error = true;
                    let errno = if let Some(errno) = control.pending_error {
                        errno
                    } else if guest.config().network_trace.policy == NetworkPolicy::Record {
                        self.shadow_submit(
                            guest,
                            control.lease,
                            NetworkStreamPhysicalEffect::ReadSocketError,
                        )
                        .await?;
                        let errno = read_socket_i32(guest, call.fd(), libc::SO_ERROR).await?;
                        self.shadow_confirm(
                            guest,
                            control.lease,
                            NetworkStreamPhysicalResult::SocketError(errno),
                        )
                        .await?;
                        errno
                    } else {
                        0
                    };
                    errno.to_ne_bytes().to_vec()
                }
                _ => unreachable!(),
            };
            let count = capacity.min(bytes.len());
            // Unlike move_addr_to_user, sk_getsockopt copies VALUE first and
            // publishes its truncated LENGTH afterward. A later length fault
            // retains the value prefix and consumed SO_ERROR.
            let copied = (|| -> Result<(), Error> {
                if count != 0 {
                    write_sockaddr_user_bytes(
                        &mut guest.memory(),
                        call.optval().ok_or(Errno::EFAULT)?.cast(),
                        &bytes[..count],
                    )?;
                }
                write_sockaddr_user_bytes(
                    &mut guest.memory(),
                    length_address.cast(),
                    &(count as i32).to_ne_bytes(),
                )?;
                Ok(())
            })();
            copied?;
            Ok(0)
        }
        .await;
        let disposition = if took_error {
            NetworkSocketControlFinish::ErrorTaken
        } else {
            NetworkSocketControlFinish::Unchanged
        };
        let finish = self
            .shadow_ack(
                guest,
                NetworkRequest::FinishSocketControl {
                    lease: control.lease,
                    disposition,
                },
            )
            .await;
        finish_shadow_operation(work, finish).map(Some)
    }
}

enum NetworkShadowWaitOutcome {
    Ready { entered_zero_wait: bool },
    Signaled { entered_zero_wait: bool },
}

// No RX/control/delivery receipt may be held when calling this helper.
// The only Record background syscall is an effect-free nfds=0 timer; all
// network observations occur after its matching continuation regains a turn.
impl<T: RecordOrReplay> Detcore<T> {
    async fn begin_shadow_zero_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Option<NetworkStreamCallId>,
        record_operation: Option<ExternalOpId>,
    ) -> Result<Option<crate::network_replay::NetworkZeroStreamWaitId>, Error> {
        let Some(call) = call else {
            return Ok(None);
        };
        match network_request(
            guest,
            NetworkRequest::BeginZeroStreamWait {
                call,
                record_operation,
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::ZeroStreamWait(id) => Ok(Some(id)),
            other => Err(engine_error(format!(
                "unexpected zero-wait admission {other:?}"
            ))),
        }
    }
    async fn inspect_shadow_zero_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        id: Option<crate::network_replay::NetworkZeroStreamWaitId>,
    ) -> Result<bool, Error> {
        let Some(id) = id else {
            return Ok(false);
        };
        match network_request(guest, NetworkRequest::InspectZeroStreamWait { id })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::ZeroStreamWaitEntered(entered) => Ok(entered),
            other => Err(engine_error(format!(
                "unexpected zero-wait completion {other:?}"
            ))),
        }
    }

    async fn wait_shadow_network<G: Guest<Self>>(
        &self,
        guest: &mut G,
        interests: Vec<(NetworkStreamCallId, NetworkWaitKind)>,
        deadline: Option<LogicalTime>,
        interrupt_errno: Errno,
        policy: NetworkPolicy,
        zero_receive_call: Option<NetworkStreamCallId>,
    ) -> Result<NetworkShadowWaitOutcome, Error> {
        let now = thread_observe_time(guest).await;
        if deadline.is_some_and(|end| now >= end) {
            return Ok(NetworkShadowWaitOutcome::Ready {
                entered_zero_wait: false,
            });
        }
        // This observes the modeled wait decision. In Record it does not
        // assert that the later injected timer has entered the kernel.
        tracing::trace!(
            "[network-wait-decision] policy={:?} dtid={} mm={:?} syscall={} interests={:?}",
            policy,
            guest.thread_state().dettid,
            guest.thread_state().mm_id,
            guest.thread_state().stats.syscall_count,
            interests,
        );
        if policy == NetworkPolicy::Replay {
            let zero_wait = self
                .begin_shadow_zero_wait(guest, zero_receive_call, None)
                .await?;
            let mut resources = Resources::new(guest.thread_state().dettid);
            resources.insert(
                ResourceID::NetworkCallWaitSet {
                    interests,
                    deadline,
                    zero_wait,
                },
                Permission::R,
            );
            resources.set_signal_interrupt_errno(interrupt_errno);
            let resumed = resource_request(guest, resources).await;
            let entered_zero_wait = self.inspect_shadow_zero_wait(guest, zero_wait).await?;
            return Ok(if matches!(resumed, ResumeStatus::Signaled(_)) {
                NetworkShadowWaitOutcome::Signaled { entered_zero_wait }
            } else {
                NetworkShadowWaitOutcome::Ready { entered_zero_wait }
            });
        }
        // This is an observation-latency bound, never a virtual-clock tick.
        // Do not increment logical time by this value or by an attempt count.
        let duration = deadline
            .map(|end| end.duration_since(now))
            .unwrap_or(Duration::from_millis(1))
            .min(Duration::from_millis(1));
        let mut stack = guest.stack().await;
        let timeout = stack.reserve::<reverie::syscalls::Timespec>();
        let _guard = stack.commit()?;
        guest.memory().write_value(
            timeout,
            &reverie::syscalls::Timespec {
                tv_sec: duration.as_secs() as libc::time_t,
                tv_nsec: duration.subsec_nanos() as libc::c_long,
            },
        )?;
        let dettid = guest.thread_state().dettid;
        // Sequential attempts fully finish this protocol before using the same
        // guest syscall identity again. No timer request survives a granted
        // continuation, and there is never more than one in flight per task.
        let operation = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        let zero_wait = self
            .begin_shadow_zero_wait(guest, zero_receive_call, Some(operation))
            .await?;
        let mut begin = Resources::new(dettid);
        begin.insert(
            ResourceID::BlockingNetworkCapture(operation),
            Permission::RW,
        );
        begin.set_signal_interrupt_errno(interrupt_errno);
        begin.fyi("network observation timer");
        if matches!(
            resource_request(guest, begin).await,
            ResumeStatus::Signaled(_)
        ) {
            let entered_zero_wait = self.inspect_shadow_zero_wait(guest, zero_wait).await?;
            return Ok(NetworkShadowWaitOutcome::Signaled { entered_zero_wait });
        }
        let result = guest
            .inject(
                syscalls::Ppoll::new()
                    .with_fds(None)
                    .with_nfds(0)
                    .with_timeout(Some(timeout))
                    .with_sigmask(None)
                    .with_sigsetsize(0),
            )
            .await;
        let mut continuation = Resources::new(dettid);
        continuation.insert(
            ResourceID::BlockedExternalContinue(operation),
            Permission::RW,
        );
        continuation.set_signal_interrupt_errno(interrupt_errno);
        continuation.fyi("network observation timer");
        let resumed = resource_request(guest, continuation).await;
        let entered_zero_wait = self.inspect_shadow_zero_wait(guest, zero_wait).await?;
        if matches!(resumed, ResumeStatus::Signaled(_)) || result == Err(Errno::EINTR) {
            return Ok(NetworkShadowWaitOutcome::Signaled { entered_zero_wait });
        }
        match result {
            Ok(0) => Ok(NetworkShadowWaitOutcome::Ready { entered_zero_wait }),
            Ok(value) => Err(engine_error(format!(
                "nfds=0 observation timer returned {value}"
            ))),
            Err(errno) => Err(engine_error(format!(
                "owned observation timer failed: {errno}"
            ))),
        }
    }
}

fn receive_timeout_duration(
    timeout: ReceiveTimeoutV3,
    hz: LinuxReceiveHzV3,
) -> Result<Option<Duration>, Error> {
    // Declared kernel timeout conversion, never global-clock quantization.
    // FiniteTicks(0) deliberately remains Some(Duration::ZERO).
    Ok(timeout.duration(hz))
}

// Per-syscall arguments retained through call admission and every wait. The
// option snapshot remains separate because it is taken AFTER admission.
#[derive(Default)]
struct NetworkZeroReceiveWait {
    id: Option<crate::network_replay::NetworkZeroStreamWaitId>,
    entered: bool,
}

struct NetworkReceiveContext<'a> {
    segments: &'a [(usize, usize)],
    flags: i32,
    zero_read: bool,
    restart_errno: Errno,
    policy: NetworkPolicy,
}

// Concrete V3 adapter branch. Legacy method bodies remain the fallback only
// when StreamSocketState says this OFD was never enrolled in the V3 model.
impl<T: RecordOrReplay> Detcore<T> {
    async fn shadow_socket_state<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
    ) -> Result<Option<NetworkStreamSocketState>, Error> {
        match network_request(guest, NetworkRequest::StreamSocketState { open_file })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::StreamSocketState(state) => Ok(state),
            other => Err(engine_error(format!(
                "unexpected stream socket state {other:?}"
            ))),
        }
    }

    async fn shadow_call_socket_state<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: NetworkStreamCallId,
    ) -> Result<NetworkStreamSocketState, Error> {
        match network_request(guest, NetworkRequest::StreamCallSocketState { call })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::StreamSocketState(Some(state)) => Ok(state),
            other => Err(engine_error(format!(
                "active stream call lost socket state {other:?}"
            ))),
        }
    }

    async fn shadow_queue_status<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: NetworkStreamCallId,
    ) -> Result<NetworkStreamQueueStatus, Error> {
        match network_request(guest, NetworkRequest::StreamCallQueueStatus { call })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::StreamQueueStatus(state) => Ok(state),
            other => Err(engine_error(format!(
                "unexpected stream queue status {other:?}"
            ))),
        }
    }

    async fn finish_shadow_chunk<G: Guest<Self>>(
        &self,
        guest: &mut G,
        lease: NetworkStreamLeaseId,
        disposition: NetworkStreamChunkDisposition,
    ) -> Result<(), Error> {
        self.shadow_ack(
            guest,
            NetworkRequest::FinishStreamChunk { lease, disposition },
        )
        .await
    }

    async fn shadow_stream_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        request: NetworkReceiveContext<'_>,
    ) -> Result<i64, Error> {
        if request.zero_read && iovec_capacity(request.segments)? == 0 {
            return Ok(0);
        }
        let open_file = guest.thread_state().socket_open_file_id(fd)?;
        let pin = self
            .begin_host_stream_call(guest, fd, open_file, request.policy)
            .await?;
        let mut zero_wait = NetworkZeroReceiveWait::default();
        let result = async {
            // Snapshot timeout/low-water at admitted-call linearization, after
            // any wait for another alias's option or descriptor mutation.
            let initial = self.shadow_call_socket_state(guest, pin.call).await?;
            self.shadow_stream_receive_inner(guest, &pin, request, initial, &mut zero_wait)
                .await
        }
        .await;
        let cleanup = if let Some(id) = zero_wait.id {
            if zero_wait.entered {
                match network_request(guest, NetworkRequest::FinishZeroStreamWait { id })
                    .await
                    .map_err(engine_rpc_error)
                {
                    Ok(NetworkReply::ZeroStreamWaitEntered(true)) => Ok(()),
                    Ok(other) => Err(engine_error(format!(
                        "unexpected entered zero-wait completion {other:?}"
                    ))),
                    Err(error) => Err(error),
                }
            } else {
                self.shadow_ack(guest, NetworkRequest::CancelZeroStreamWait { id })
                    .await
            }
        } else {
            Ok(())
        };
        let result = finish_shadow_operation(result, cleanup);
        self.finish_host_stream_call(guest, pin, result).await
    }

    async fn shadow_stream_receive_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
        request: NetworkReceiveContext<'_>,
        initial: NetworkStreamSocketState,
        zero_wait: &mut NetworkZeroReceiveWait,
    ) -> Result<i64, Error> {
        let NetworkReceiveContext {
            segments,
            flags,
            zero_read,
            restart_errno,
            policy,
        } = request;
        let maximum = iovec_capacity(segments)?.min(NETWORK_MAX_RW_COUNT);
        if maximum == 0 && zero_read {
            // sock_read_iter's empty iterator returns before tcp_recvmsg.
            return Ok(0);
        }
        let supported = libc::MSG_PEEK | libc::MSG_WAITALL | libc::MSG_DONTWAIT | libc::MSG_TRUNC;
        if flags & !supported != 0 {
            return Err(engine_error(format!(
                "unsupported receive flags {:#x}",
                flags & !supported
            )));
        }
        let nonblocking = flags & libc::MSG_DONTWAIT != 0 || pin.nonblocking;
        let timeout =
            receive_timeout_duration(initial.options.receive_timeout, initial.normalization.hz)?;
        let start = thread_observe_time(guest).await;
        let deadline = timeout.map(|duration| start + duration);
        let interrupt_errno = if timeout.is_some() {
            Errno::EINTR
        } else {
            restart_errno
        };
        // Linux sock_rcvlowat uses ?: 1, including a zero-length recv request.
        let target = (if flags & libc::MSG_WAITALL != 0 {
            maximum
        } else {
            maximum.min(initial.options.receive_low_water as usize)
        })
        .max(1);
        let peeking = flags & libc::MSG_PEEK != 0;
        let original_peek = if peeking {
            initial.options.peek_offset.unwrap_or(-1).max(0) as usize
        } else {
            0
        };
        let mut peek_offset = original_peek;
        let mut epoch = initial.consume_epoch;
        let mut accepted = 0usize;
        let mut observed_without_wait = false;
        loop {
            let now = thread_observe_time(guest).await;
            if maximum == 0 && zero_wait.entered && deadline.is_some_and(|end| now >= end) {
                return Ok(0);
            }
            if policy == NetworkPolicy::Replay {
                match network_request(guest, NetworkRequest::ReleaseEligible(now))
                    .await
                    .map_err(engine_rpc_error)?
                {
                    NetworkReply::ReadyChannels(_) => {}
                    other => {
                        return Err(engine_error(format!(
                            "unexpected eligible release {other:?}"
                        )));
                    }
                }
            }
            if maximum == 0 && policy == NetworkPolicy::Record && !observed_without_wait {
                // Establish current physical input before the first atomic
                // empty decision; later maintenance observations retain that
                // same generation receipt instead of moving its baseline.
                self.observe_shadow_stream(guest, pin).await?;
                observed_without_wait = true;
            }
            let status = self.shadow_queue_status(guest, pin.call).await?;
            if status.listener {
                return Err(Errno::ENOTCONN.into());
            }
            // A terminal wake after entering sk_wait_data reaches the len==0
            // loop tail. It must not consume a newly pending error as though
            // that error had been present before the wait.
            if maximum == 0
                && zero_wait.entered
                && (status.eof || status.local_read_shutdown || status.error.is_some())
            {
                return Ok(0);
            }
            if peeking && status.consume_epoch != epoch {
                // tcp_recvmsg's post-wait peek_seq reset uses the ORIGINAL
                // per-call offset, while its copied count remains unchanged.
                peek_offset = original_peek;
                epoch = status.consume_epoch;
            }
            let offset = if peeking { peek_offset } else { 0 };
            if accepted != 0
                && status.queued_bytes <= offset
                && (status.eof || status.local_read_shutdown || status.error.is_some())
            {
                // Completed bytes win over a subsequent terminal error.
                return Ok(accepted as i64);
            }
            let chunk = if maximum == 0 {
                match network_request(
                    guest,
                    NetworkRequest::ZeroStreamReceive {
                        call: pin.call,
                        peek_offset: offset,
                    },
                )
                .await
                .map_err(engine_rpc_error)?
                {
                    NetworkReply::ZeroStreamReceive(
                        NetworkZeroStreamReceive::Ready | NetworkZeroStreamReceive::EndOfFile,
                    ) => return Ok(0),
                    NetworkReply::ZeroStreamReceive(NetworkZeroStreamReceive::Error(errno)) => {
                        return errno_result(errno);
                    }
                    NetworkReply::ZeroStreamReceive(NetworkZeroStreamReceive::Waiting(id)) => {
                        if zero_wait.id.is_some_and(|prior| prior != id) {
                            return Err(engine_error("zero receive replaced its unresolved wait"));
                        }
                        zero_wait.id = Some(id);
                        NetworkStreamChunk::Empty
                    }
                    other => {
                        return Err(engine_error(format!("unexpected zero receive {other:?}")));
                    }
                }
            } else {
                match network_request(
                    guest,
                    NetworkRequest::ReserveStreamCallChunk {
                        call: pin.call,
                        maximum: maximum - accepted,
                        peek_offset: offset,
                    },
                )
                .await
                .map_err(engine_rpc_error)?
                {
                    NetworkReply::StreamChunk(chunk) => chunk,
                    other => {
                        return Err(engine_error(format!(
                            "unexpected stream reservation {other:?}"
                        )));
                    }
                }
            };
            match chunk {
                NetworkStreamChunk::Reserved {
                    lease,
                    selection_len,
                    outcome: NetworkStreamChunkOutcome::Bytes(mut view),
                } => {
                    if selection_len == 0
                        || selection_len > maximum - accepted
                        || view.len() != selection_len.min(SHADOW_VIEW)
                    {
                        return Err(engine_error("invalid whole-unit stream selection/view"));
                    }
                    if flags & libc::MSG_TRUNC == 0 {
                        let mut at = 0usize;
                        loop {
                            let copied = copy_stream_chunk_to_user(
                                &mut guest.memory(),
                                segments,
                                accepted + at,
                                &view,
                            );
                            if let Some(errno) = copied.error {
                                // The actual written prefix remains visible. It
                                // is NOT a stream-consumption count. One final
                                // abort retains this entire selected unit.
                                self.finish_shadow_chunk(
                                    guest,
                                    lease,
                                    NetworkStreamChunkDisposition::CopyFailed,
                                )
                                .await?;
                                tracing::trace!(
                                    "receive fault after {} memory bytes in retained unit",
                                    at + copied.written
                                );
                                return if accepted != 0 {
                                    Ok(accepted as i64)
                                } else {
                                    Err(errno.into())
                                };
                            }
                            at += view.len();
                            if at == selection_len {
                                break;
                            }
                            let maximum = (selection_len - at).min(SHADOW_VIEW);
                            view = match network_request(
                                guest,
                                NetworkRequest::ReadStreamChunkView {
                                    lease,
                                    offset: at,
                                    maximum,
                                },
                            )
                            .await
                            .map_err(engine_rpc_error)?
                            {
                                NetworkReply::StreamChunkView(view) if view.len() == maximum => {
                                    view
                                }
                                other => {
                                    return Err(engine_error(format!(
                                        "invalid stream view {other:?}"
                                    )));
                                }
                            };
                        }
                    }
                    if peeking {
                        // This also advances the semantic nonnegative PEEK_OFF
                        // in BOTH modes. Physical probe restoration is separate.
                        if policy == NetworkPolicy::Record {
                            self.finish_record_peek(guest, pin, lease, selection_len)
                                .await?;
                        } else {
                            self.finish_shadow_chunk(
                                guest,
                                lease,
                                NetworkStreamChunkDisposition::Peeked,
                            )
                            .await?;
                        }
                        peek_offset = peek_offset
                            .checked_add(selection_len)
                            .ok_or_else(|| engine_error("per-call peek offset overflow"))?;
                    } else if policy == NetworkPolicy::Record {
                        // Each physical syscall is Submitted before injection;
                        // only the final exact drain advances logical C.
                        self.drain_shadow_selection(guest, pin, lease, selection_len)
                            .await?;
                    } else {
                        self.finish_shadow_chunk(
                            guest,
                            lease,
                            NetworkStreamChunkDisposition::Consumed,
                        )
                        .await?;
                    }
                    accepted += selection_len;
                    if accepted == maximum {
                        return Ok(accepted as i64);
                    }
                    observed_without_wait = false;
                    continue;
                }
                NetworkStreamChunk::Reserved {
                    lease,
                    selection_len: 0,
                    outcome: NetworkStreamChunkOutcome::EndOfFile,
                } => {
                    self.finish_shadow_chunk(
                        guest,
                        lease,
                        if peeking {
                            NetworkStreamChunkDisposition::Peeked
                        } else {
                            NetworkStreamChunkDisposition::Consumed
                        },
                    )
                    .await?;
                    return Ok(accepted as i64);
                }
                NetworkStreamChunk::Reserved {
                    lease,
                    selection_len: 0,
                    outcome: NetworkStreamChunkOutcome::Error(errno),
                } => {
                    // Core permits consuming this terminal error under PEEK;
                    // consuming a byte selection with nonzero offset stays invalid.
                    self.finish_shadow_chunk(guest, lease, NetworkStreamChunkDisposition::Consumed)
                        .await?;
                    return errno_result(errno);
                }
                NetworkStreamChunk::Reserved { .. } => {
                    return Err(engine_error("invalid terminal selection"));
                }
                NetworkStreamChunk::LocalReadClosed => {
                    if policy != NetworkPolicy::Record || observed_without_wait {
                        return Ok(accepted as i64);
                    }
                    // Local shutdown does not discard physical queued or later
                    // input. Observe it before deciding that this read is empty.
                }
                NetworkStreamChunk::Empty => {}
            }
            if accepted >= target {
                return Ok(accepted as i64);
            }
            if policy == NetworkPolicy::Record && !observed_without_wait {
                self.observe_shadow_stream(guest, pin).await?;
                observed_without_wait = true;
                continue;
            }
            // In Record, one actual nonblocking observation must precede this
            // decision; in Replay the released V3 queue is the sole authority.
            if nonblocking || deadline.is_some_and(|end| now >= end) {
                return if accepted != 0 {
                    Ok(accepted as i64)
                } else {
                    Err(Errno::EAGAIN.into())
                };
            }
            let minimum = offset
                .checked_add(target.saturating_sub(accepted).max(1))
                .ok_or_else(|| engine_error("receive wait threshold overflow"))?;
            let wait = self
                .wait_shadow_network(
                    guest,
                    vec![(pin.call, NetworkWaitKind::ReadableAtLeast(minimum))],
                    deadline,
                    interrupt_errno,
                    policy,
                    (maximum == 0).then_some(pin.call),
                )
                .await;
            match wait {
                Ok(NetworkShadowWaitOutcome::Signaled { entered_zero_wait }) => {
                    zero_wait.entered |= entered_zero_wait;
                    return if accepted != 0
                        || (maximum == 0 && (zero_wait.entered || entered_zero_wait))
                    {
                        Ok(accepted as i64)
                    } else {
                        Err(interrupt_errno.into())
                    };
                }
                Ok(NetworkShadowWaitOutcome::Ready { entered_zero_wait }) => {
                    zero_wait.entered |= entered_zero_wait;
                }
                Err(error) => return Err(error),
            }
            observed_without_wait = false;
        }
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    // Caller preserves the existing poll/select ABI copying and timeout fields.
    async fn shadow_poll_fds<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds: &[libc::pollfd],
        deadline: Option<LogicalTime>,
        policy: NetworkPolicy,
    ) -> Result<Vec<libc::pollfd>, Error> {
        loop {
            let mut pins = Vec::<(i32, NetworkHostSocketPin)>::new();
            let operation = async {
                let now = thread_observe_time(guest).await;
                if policy == NetworkPolicy::Replay {
                    network_request(guest, NetworkRequest::ReleaseEligible(now))
                        .await
                        .map_err(engine_rpc_error)?;
                }
                let mut output = fds.to_vec();
                let mut interests = Vec::new();
                let mut ready = false;
                for pollfd in &mut output {
                    pollfd.revents = 0;
                    if pollfd.fd < 0 {
                        continue;
                    }
                    let Some(open_file) = self.network_open_file(guest, pollfd.fd) else {
                        if guest.thread_state().with_detfd(pollfd.fd, |_| ()).is_ok() {
                            return Err(engine_error(
                                "mixed network and non-network poll sets are not replayable",
                            ));
                        }
                        pollfd.revents = libc::POLLNVAL;
                        ready = true;
                        continue;
                    };
                    let Some(state) = self.shadow_socket_state(guest, open_file).await? else {
                        return Err(engine_error(
                            "V3 poll needs the separate legacy-socket composition path",
                        ));
                    };
                    // Reuse one per-call file reference for repeated pollfd
                    // entries, while preserving each input row and its mask.
                    let at = if let Some(at) = pins.iter().position(|(fd, _)| *fd == pollfd.fd) {
                        at
                    } else {
                        let pin = self
                            .begin_host_stream_call(guest, pollfd.fd, open_file, policy)
                            .await?;
                        pins.push((pollfd.fd, pin));
                        pins.len() - 1
                    };
                    let pin = &pins[at].1;
                    let before = self.shadow_queue_status(guest, pin.call).await?;
                    if policy == NetworkPolicy::Record && !before.listener {
                        // CompleteShadowProbe atomically journals its confirmed
                        // non-READ bits with payload/EOF at engine-owned time.
                        // A second legacy CaptureReadiness would both split
                        // that boundary and reject a shared-ingress channel.
                        self.observe_shadow_stream(guest, pin).await?;
                    }
                    let status = self.shadow_queue_status(guest, pin.call).await?;
                    pollfd.revents = shadow_readiness_to_poll_revents(
                        &status,
                        state.options.receive_low_water as usize,
                        pollfd.events,
                    );
                    ready |= pollfd.revents != 0;
                    if pollfd.events & (libc::POLLIN | libc::POLLRDNORM) != 0 {
                        interests.push((pin.call, NetworkWaitKind::PollReadable));
                    }
                    if pollfd.events & libc::POLLRDHUP != 0 {
                        interests.push((pin.call, NetworkWaitKind::ReceiveHalfClosed));
                    }
                    if pollfd.events & (libc::POLLOUT | libc::POLLWRNORM) != 0 {
                        interests.push((pin.call, NetworkWaitKind::Writable));
                    }
                    // events==0 still observes ERR/HUP but must not wake merely
                    // because the socket is writable. Core's terminal-only
                    // predicate is a required integration edge (see inventory).
                    if pollfd.events
                        & (libc::POLLIN
                            | libc::POLLRDNORM
                            | libc::POLLOUT
                            | libc::POLLWRNORM
                            | libc::POLLRDHUP)
                        == 0
                    {
                        interests.push((pin.call, NetworkWaitKind::Terminal));
                    }
                }
                if ready || deadline.is_some_and(|end| now >= end) {
                    return Ok(Some(output));
                }
                if matches!(
                    self.wait_shadow_network(
                        guest,
                        interests,
                        deadline,
                        Errno::EINTR,
                        policy,
                        None
                    )
                    .await?,
                    NetworkShadowWaitOutcome::Signaled { .. }
                ) {
                    return Err(Errno::EINTR.into());
                }
                Ok(None)
            }
            .await;
            // Keep call references while parked, then release before the next
            // original-FD lookup. Each poll row is rechecked like do_pollfd.
            let mut cleanup = Ok(());
            for (_, pin) in pins {
                let released = self
                    .finish_host_stream_call(guest, pin, Ok(0))
                    .await
                    .map(|_| ());
                cleanup = finish_shadow_operation(cleanup, released);
            }
            if let Some(output) = finish_shadow_operation(operation, cleanup)? {
                return Ok(output);
            }
        }
    }

    async fn shadow_network_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        state: NetworkPollState,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let address = state.poll_address()?;
        let fds = read_pollfds(guest, address, state.count)?;
        // Preserve the prior explicit OOB/band limitation; do not quietly map it
        // to ordinary TCP payload readiness.
        if fds
            .iter()
            .any(|fd| fd.events & (libc::POLLPRI | libc::POLLRDBAND | libc::POLLWRBAND) != 0)
        {
            return Err(engine_error("unsupported poll exceptional/band event mask"));
        }
        let start = thread_observe_time(guest).await;
        let deadline = state.timeout.map(|duration| start + duration);
        let output = self.shadow_poll_fds(guest, &fds, deadline, policy).await?;
        let count = output.iter().filter(|fd| fd.revents != 0).count() as i64;
        write_pollfds(guest, address, &output)?;
        let now = thread_observe_time(guest).await;
        write_remaining_timeout(guest, state.remaining_address, deadline, now)?;
        Ok(count)
    }

    async fn shadow_network_select<G: Guest<Self>>(
        &self,
        guest: &mut G,
        state: NetworkSelectState,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        if state
            .except
            .as_ref()
            .is_some_and(|set| (0..state.nfds).any(|fd| fd_is_set(fd, set)))
        {
            return Err(engine_error(
                "select exceptional/OOB readiness is not represented",
            ));
        }
        let mut fds = Vec::new();
        for fd in 0..state.nfds {
            let read = state.read.as_ref().is_some_and(|set| fd_is_set(fd, set));
            let write = state.write.as_ref().is_some_and(|set| fd_is_set(fd, set));
            if read || write {
                fds.push(libc::pollfd {
                    fd,
                    events: (if read { libc::POLLIN } else { 0 })
                        | (if write { libc::POLLOUT } else { 0 }),
                    revents: 0,
                });
            }
        }
        let start = thread_observe_time(guest).await;
        let deadline = state.timeout.map(|duration| start + duration);
        let ready = self.shadow_poll_fds(guest, &fds, deadline, policy).await?;
        let mut output = state.empty_output();
        let mut count = 0i64;
        for fd in ready {
            if fd.revents & libc::POLLNVAL != 0 {
                return Err(Errno::EBADF.into());
            }
            let read = fd.events & libc::POLLIN != 0
                && fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
            let write =
                fd.events & libc::POLLOUT != 0 && fd.revents & (libc::POLLOUT | libc::POLLERR) != 0;
            if read {
                output.set_read(fd.fd);
            }
            if write {
                output.set_write(fd.fd);
            }
            count += select_ready_bit_count(read, write);
        }
        output.write(guest)?;
        let now = thread_observe_time(guest).await;
        state.write_remaining(guest, deadline, now)?;
        Ok(count)
    }
}

async fn read_socket_i32<G, T>(guest: &mut G, fd: i32, name: i32) -> Result<i32, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut stack = guest.stack().await;
    let value = stack.reserve::<i32>();
    let length = stack.reserve::<libc::socklen_t>();
    let _guard = stack.commit()?;
    guest
        .memory()
        .write_value(length, &(std::mem::size_of::<i32>() as libc::socklen_t))?;
    let result = guest
        .inject(
            syscalls::Getsockopt::new()
                .with_fd(fd)
                .with_level(libc::SOL_SOCKET)
                .with_optname(name)
                .with_optval(Some(value.cast()))
                .with_optlen(Some(length)),
        )
        .await?;
    if result != 0
        || guest.memory().read_value(length)? != std::mem::size_of::<i32>() as libc::socklen_t
    {
        return Err(engine_error("socket option returned wrong scalar size"));
    }
    Ok(guest.memory().read_value(value)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_rpc_refusal_does_not_reclassify_adapter_protocol_or_errno_failures() {
        use detcore_model::network_trace::NetworkPolicy;

        use crate::network_failure::NetworkFailurePhase;
        use crate::network_failure::NetworkPolicyRefusal;
        use crate::network_failure::NetworkRpcError;
        use crate::network_replay::NetworkReplayError;
        let rpc = NetworkRpcError::from_engine(
            NetworkPolicy::Replay,
            NetworkFailurePhase::Transmit,
            NetworkReplayError::TraceExhausted(detcore_model::network_trace::NetworkChannelId(1)),
        );
        let text = rpc.to_string();
        let reverie::Error::Tool(typed) = super::engine_rpc_error(rpc) else {
            panic!("expected Tool refusal")
        };
        assert!(typed.downcast_ref::<NetworkPolicyRefusal>().is_some());
        assert!(format!("{typed:#}").contains(&text));
        for error in [
            super::engine_error(&text),
            super::engine_rpc_error(NetworkRpcError::internal(&text)),
        ] {
            let reverie::Error::Tool(internal) = error else {
                panic!("expected Tool internal error")
            };
            assert!(internal.downcast_ref::<NetworkPolicyRefusal>().is_none());
        }
        for errno in [libc::EFAULT, libc::EINTR, libc::EAGAIN, libc::ECONNREFUSED] {
            let error = super::errno_result(errno).unwrap_err();
            assert_eq!(super::error_errno(&error), Some(errno));
        }
    }

    use super::*;

    #[test]
    fn stream_transport_requires_exact_domain_type_and_protocol() {
        for domain in [libc::AF_INET, libc::AF_INET6] {
            assert_eq!(
                classify_stream_transport([domain, libc::SOCK_STREAM, libc::IPPROTO_TCP]).unwrap(),
                NetworkTransportV2::Tcp,
            );
        }
        assert_eq!(
            classify_stream_transport([libc::AF_UNIX, libc::SOCK_STREAM, 0]).unwrap(),
            NetworkTransportV2::UnixStream,
        );
        for properties in [
            [libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP],
            [libc::AF_INET, libc::SOCK_STREAM, 262], // MPTCP is not ordinary TCP.
            [libc::AF_UNIX, libc::SOCK_SEQPACKET, 0],
            [libc::AF_UNIX, libc::SOCK_STREAM, libc::IPPROTO_TCP],
        ] {
            assert!(
                classify_stream_transport(properties).is_err(),
                "{properties:?}"
            );
        }
    }

    // The ptrace backend's scalar fast paths may ignore guest VMA permissions.
    // Tests use real process_vm copies and reject accidental scalar dispatch.
    struct UserAccessMemory(reverie::syscalls::LocalMemory);

    impl MemoryAccess for UserAccessMemory {
        fn read_vectored(
            &self,
            remote: &[std::io::IoSlice],
            local: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.0.read_vectored(remote, local)
        }

        fn write_vectored(
            &mut self,
            local: &[std::io::IoSlice],
            remote: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.0.write_vectored(local, remote)
        }

        fn read<'a, A>(&self, _address: A, _bytes: &mut [u8]) -> Result<usize, Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            panic!("ABI copy must respect user access, not ptrace scalar reads")
        }

        fn read_exact_with_user_access<'a, A>(
            &self,
            address: A,
            bytes: &mut [u8],
        ) -> Result<(), Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            self.0.read_exact_with_user_access(address, bytes)
        }

        fn write(&mut self, _address: AddrMut<u8>, _bytes: &[u8]) -> Result<usize, Errno> {
            panic!("ABI copy must respect user access, not ptrace scalar writes")
        }
    }

    fn test_peer() -> NetworkAddressV2 {
        NetworkAddressV2::Inet4 {
            address: [127, 0, 0, 1],
            port: 12345,
        }
    }

    #[test]
    fn accept_peer_preserves_socklen32_sentinel_and_exact_truncated_prefix() {
        #[repr(C)]
        struct LengthAndSentinel {
            length: libc::socklen_t,
            sentinel: u32,
        }
        let peer = test_peer();
        let expected = network_address_bytes(&peer).unwrap();
        let mut memory = UserAccessMemory(reverie::syscalls::LocalMemory::new());
        for capacity in [0, 3, 8, 16, 32] {
            let mut bytes = [0xa5_u8; 32];
            let mut length = LengthAndSentinel {
                length: capacity,
                sentinel: 0xfeedface,
            };
            write_accept_peer_memory(
                &mut memory,
                AddrMut::from_ptr(bytes.as_mut_ptr()).unwrap().cast(),
                AddrMut::from_ptr(&raw mut length.length).unwrap(),
                Some(&peer),
            )
            .unwrap();
            let copied = (capacity as usize).min(expected.len());
            assert_eq!(&bytes[..copied], &expected[..copied]);
            assert!(bytes[copied..].iter().all(|byte| *byte == 0xa5));
            assert_eq!(length.length, 16);
            assert_eq!(length.sentinel, 0xfeedface);
        }
    }

    #[test]
    fn accept_peer_rejects_negative_linux_socklen_before_writing() {
        let mut memory = UserAccessMemory(reverie::syscalls::LocalMemory::new());
        let mut bytes = [0xa5_u8; 32];
        let mut length: libc::socklen_t = 0x8000_0000;
        assert!(matches!(
            write_accept_peer_memory(
                &mut memory,
                AddrMut::from_ptr(bytes.as_mut_ptr()).unwrap().cast(),
                AddrMut::from_ptr(&raw mut length).unwrap(),
                Some(&test_peer()),
            ),
            Err(Error::Errno(errno)) if errno == Errno::EINVAL
        ));
        assert_eq!(bytes, [0xa5; 32]);
        assert_eq!(length, 0x8000_0000);
    }

    struct SocketAddressPages {
        address: *mut u8,
        page_size: usize,
    }

    impl SocketAddressPages {
        fn new() -> Self {
            // SAFETY: sysconf has no pointer arguments. The private anonymous
            // mapping is owned here and released by Drop, including on panic.
            let page_size = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    page_size * 2,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            Self {
                address: address.cast(),
                page_size,
            }
        }

        fn protect(&self, offset: usize, length: usize, protection: i32) {
            assert_eq!(
                unsafe { libc::mprotect(self.address.add(offset).cast(), length, protection) },
                0
            );
        }
    }

    impl Drop for SocketAddressPages {
        fn drop(&mut self) {
            // SAFETY: this object owns exactly this mapping, regardless of its
            // current protections. munmap does not read the mapped contents.
            let result = unsafe { libc::munmap(self.address.cast(), self.page_size * 2) };
            assert_eq!(result, 0);
        }
    }

    #[test]
    fn accept_peer_faults_publish_length_before_partial_address_copy() {
        let pages = SocketAddressPages::new();
        let peer = test_peer();
        let expected = network_address_bytes(&peer).unwrap();
        let mut memory = UserAccessMemory(reverie::syscalls::LocalMemory::new());
        for (offset, protected_offset, protection, copied) in [
            (0, 0, libc::PROT_READ, 0),
            (0, 0, libc::PROT_NONE, 0),
            (pages.page_size - 4, pages.page_size, libc::PROT_NONE, 4),
        ] {
            pages.protect(0, pages.page_size * 2, libc::PROT_READ | libc::PROT_WRITE);
            // SAFETY: both owned pages are writable at this point.
            unsafe { std::ptr::write_bytes(pages.address, 0xa5, pages.page_size * 2) };
            pages.protect(protected_offset, pages.page_size, protection);
            let mut length: libc::socklen_t = 8;
            assert!(matches!(
                write_accept_peer_memory(
                    &mut memory,
                    AddrMut::from_raw(pages.address as usize + offset).unwrap(),
                    AddrMut::from_ptr(&raw mut length).unwrap(),
                    Some(&peer),
                ),
                Err(Error::Errno(errno)) if errno == Errno::EFAULT
            ));
            assert_eq!(length, 16);
            pages.protect(0, pages.page_size * 2, libc::PROT_READ | libc::PROT_WRITE);
            // SAFETY: the complete owned mapping is readable again.
            let observed = unsafe { std::slice::from_raw_parts(pages.address.add(offset), 8) };
            assert_eq!(&observed[..copied], &expected[..copied]);
            assert!(observed[copied..].iter().all(|byte| *byte == 0xa5));
        }
    }

    #[test]
    fn accept_peer_length_protection_fault_does_not_copy_address() {
        let pages = SocketAddressPages::new();
        let mut memory = UserAccessMemory(reverie::syscalls::LocalMemory::new());
        for protection in [libc::PROT_READ, libc::PROT_NONE] {
            pages.protect(0, pages.page_size * 2, libc::PROT_READ | libc::PROT_WRITE);
            // SAFETY: both pages are writable, and the page-aligned length
            // pointer is aligned for socklen_t. Neither overlaps the address.
            unsafe {
                std::ptr::write_bytes(pages.address, 0xa5, pages.page_size * 2);
                pages
                    .address
                    .add(pages.page_size)
                    .cast::<libc::socklen_t>()
                    .write(8);
            }
            pages.protect(pages.page_size, pages.page_size, protection);
            assert!(matches!(
                write_accept_peer_memory(
                    &mut memory,
                    AddrMut::from_raw(pages.address as usize).unwrap(),
                    AddrMut::from_raw(pages.address as usize + pages.page_size).unwrap(),
                    Some(&test_peer()),
                ),
                Err(Error::Errno(errno)) if errno == Errno::EFAULT
            ));
            pages.protect(0, pages.page_size * 2, libc::PROT_READ | libc::PROT_WRITE);
            // SAFETY: both owned pages are readable again.
            let observed = unsafe { std::slice::from_raw_parts(pages.address, 16) };
            assert_eq!(observed, &[0xa5; 16]);
            let length = unsafe {
                pages
                    .address
                    .add(pages.page_size)
                    .cast::<libc::socklen_t>()
                    .read()
            };
            assert_eq!(length, 8);
        }
    }

    #[test]
    fn select_counts_each_ready_set_bit_for_a_duplex_descriptor() {
        assert_eq!(select_ready_bit_count(false, false), 0);
        assert_eq!(select_ready_bit_count(true, false), 1);
        assert_eq!(select_ready_bit_count(false, true), 1);
        assert_eq!(select_ready_bit_count(true, true), 2);
    }

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

#[cfg(test)]
mod shadow_socket_option_tests {
    use std::cell::RefCell;

    use super::*;

    struct SnapshotMemory {
        reads: RefCell<Vec<usize>>,
        inner: reverie::syscalls::LocalMemory,
    }

    impl Default for SnapshotMemory {
        fn default() -> Self {
            Self {
                reads: RefCell::new(Vec::new()),
                inner: reverie::syscalls::LocalMemory {},
            }
        }
    }

    impl MemoryAccess for SnapshotMemory {
        fn read_vectored(
            &self,
            remote: &[std::io::IoSlice],
            local: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.inner.read_vectored(remote, local)
        }
        fn write_vectored(
            &mut self,
            local: &[std::io::IoSlice],
            remote: &mut [std::io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.inner.write_vectored(local, remote)
        }
        fn read_exact_with_user_access<'a, A>(
            &self,
            address: A,
            bytes: &mut [u8],
        ) -> Result<(), Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            self.reads.borrow_mut().push(bytes.len());
            self.inner.read_exact_with_user_access(address, bytes)
        }
    }

    #[test]
    fn setter_short_length_precedes_pointer_and_timeout_second_copy() {
        let memory = SnapshotMemory::default();
        let call = syscalls::Setsockopt::new()
            .with_level(libc::SOL_SOCKET)
            .with_optname(libc::SO_RCVTIMEO);
        for length in 0..4 {
            assert!(
                matches!(snapshot_shadow_socket_option(&memory, call.with_optlen(length)), Err(Error::Errno(errno)) if errno == Errno::EINVAL)
            );
        }
        assert!(memory.reads.borrow().is_empty());
        for length in [i32::MAX as u32 + 1, u32::MAX] {
            assert!(
                matches!(snapshot_shadow_socket_option(&memory,call.with_optlen(length)),Err(Error::Errno(errno)) if errno==Errno::EINVAL)
            );
        }
        assert!(memory.reads.borrow().is_empty());
        assert!(
            matches!(snapshot_shadow_socket_option(&memory, call.with_optlen(4)), Err(Error::Errno(errno)) if errno == Errno::EFAULT)
        );
        assert!(memory.reads.borrow().is_empty());
        let value = 0_i32;
        let call = call.with_optval(Some(
            Addr::from_ptr(&value)
                .expect("nonnull local fixture")
                .cast(),
        ));
        assert!(
            matches!(snapshot_shadow_socket_option(&memory, call.with_optlen(4)), Err(Error::Errno(errno)) if errno == Errno::EINVAL)
        );
        assert_eq!(&*memory.reads.borrow(), &[4]);
    }

    #[test]
    fn setter_snapshot_retains_negative_timeout_and_full_raw_timeval() {
        let memory = SnapshotMemory::default();
        let raw = [-1_i64, 0_i64];
        let call = syscalls::Setsockopt::new()
            .with_level(libc::SOL_SOCKET)
            .with_optname(libc::SO_RCVTIMEO)
            .with_optlen(16)
            .with_optval(Some(
                Addr::from_ptr(&raw).expect("nonnull local fixture").cast(),
            ));
        let argument = snapshot_shadow_socket_option(&memory, call)
            .unwrap()
            .unwrap();
        assert!(matches!(
            argument.option,
            NetworkStreamSocketOption::ReceiveTimeout {
                seconds: -1,
                microseconds: 0
            }
        ));
        assert_eq!(&argument.bytes[..8], &(-1_i64).to_ne_bytes());
        assert_eq!(&argument.bytes[8..], &0_i64.to_ne_bytes());
        assert_eq!(&*memory.reads.borrow(), &[4, 16]);
    }

    #[test]
    fn setter_scalar_snapshot_preserves_signed_values_and_ignores_excess_capacity() {
        let memory = SnapshotMemory::default();
        let raw = -1_i32;
        let call = syscalls::Setsockopt::new()
            .with_level(libc::SOL_SOCKET)
            .with_optname(libc::SO_RCVBUF)
            .with_optlen(i32::MAX as u32)
            .with_optval(Some(
                Addr::from_ptr(&raw).expect("nonnull local fixture").cast(),
            ));
        let argument = snapshot_shadow_socket_option(&memory, call)
            .unwrap()
            .unwrap();
        assert!(matches!(
            argument.option,
            NetworkStreamSocketOption::ReceiveBuffer(-1)
        ));
        assert_eq!(&argument.bytes[..4], &raw.to_ne_bytes());
        assert_eq!(&*memory.reads.borrow(), &[4]);
    }
}

#[cfg(test)]
mod accepted_flags_tests {
    use super::*;
    #[test]
    fn accept4_retained_unknown_bits_never_become_socket_type_or_queue_availability() {
        for invalid in [
            libc::SOCK_DGRAM,
            libc::SOCK_STREAM,
            libc::SOCK_RAW,
            64,
            i32::MIN,
            -1,
        ] {
            let call = syscalls::Accept4::new()
                .with_flags(nix::sys::socket::SockFlag::from_bits_retain(invalid));
            assert_eq!(
                call.flags().bits(),
                invalid,
                "facade must retain the actual guest flags"
            );
            assert_eq!(
                checked_accept4_socket_type(call.flags().bits()),
                Err(Errno::EINVAL)
            );
        }
        for valid in [
            0,
            libc::SOCK_CLOEXEC,
            libc::SOCK_NONBLOCK,
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        ] {
            let socket_type = checked_accept4_socket_type(valid).unwrap();
            assert_eq!(
                socket_type & !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK),
                libc::SOCK_STREAM
            );
            assert_eq!(
                socket_type & (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK),
                valid
            );
        }
    }
}
