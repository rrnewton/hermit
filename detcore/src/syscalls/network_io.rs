/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Single guest-memory adapter for engine-owned network syscalls.

use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkChannelV2;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkEndpointRoleV2;
use detcore_model::network_trace::NetworkPolicy;
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
            Syscall::Read(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Write(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Readv(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Writev(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Recvfrom(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Sendto(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Recvmsg(call) => self.network_open_file(guest, call.sockfd()).is_some(),
            Syscall::Sendmsg(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Shutdown(call) => self.network_open_file(guest, call.fd()).is_some(),
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
}
