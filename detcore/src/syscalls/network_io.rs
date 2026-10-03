/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
*/

//! Single guest-memory adapter for engine-owned network syscalls.

mod native_poll;

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
use crate::network_replay::NetworkStreamQueueStatus;
use crate::network_replay::NetworkStreamSocketOption;
use crate::network_replay::NetworkStreamSocketState;
use crate::network_replay::NetworkZeroStreamReceive;
use crate::network_replay::original_sendto_shape;
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
    fn network_fd_is_capability_probe<G: Guest<Self>>(&self, guest: &G, fd: i32) -> bool {
        guest.thread_state().with_detfd(fd, |fd| fd.is_network_capability_probe()).unwrap_or(false)
    }

    /// A successful capability allocation proves an installed socket, not UDP
    /// communication support. Check before guest payload access or injection.
    /// The marker belongs to the OFD, so dup aliases cannot bypass this boundary.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3464): Review allocation-only IPv6 probe isolation.
    // https://github.com/rrnewton/hermit/pull/3464
    pub(crate) fn check_network_capability_probe_use<G: Guest<Self>>(
        &self, guest: &mut G, call: Syscall,
    ) -> Result<(), Error> {
        use reverie::syscalls::Sysno;
        if !matches!(guest.config().network_trace.policy, NetworkPolicy::Record | NetworkPolicy::Replay) {
            return Ok(());
        }
        let forbidden_local = match call {
            Syscall::Connect(c) => Some(c.fd()),
            Syscall::Bind(c) => Some(c.fd()),
            Syscall::Listen(c) => Some(c.fd()),
            Syscall::Accept(c) => Some(c.sockfd()),
            Syscall::Accept4(c) => Some(c.sockfd()),
            Syscall::Sendmsg(c) => Some(c.fd()),
            Syscall::Recvmsg(c) => Some(c.sockfd()),
            Syscall::Sendmmsg(c) => Some(c.sockfd()),
            Syscall::Recvmmsg(c) => Some(c.fd()),
            Syscall::Sendto(c) if {
                let (_, args) = Syscall::from(c).into_parts();
                args.arg4 != 0 || args.arg5 != 0
            } => Some(c.fd()),
            _ => None,
        };
        if forbidden_local.is_some_and(|fd| guest.thread_state().with_detfd(fd,
            |fd| fd.is_local_socket_pair()).unwrap_or(false))
        {
            return Err(engine_error("local socketpair does not authorize another endpoint or descriptor transfer"));
        }
        if !guest.thread_state().file_metadata.lock().unwrap().has_network_capability_probe() {
            return Ok(());
        }
        let (number, args) = call.into_parts();
        let blocked = match call {
            Syscall::Fcntl(fcntl) => !matches!(fcntl.cmd(),
                syscalls::FcntlCmd::F_GETFL | syscalls::FcntlCmd::F_GETFD
                | syscalls::FcntlCmd::F_DUPFD(_) | syscalls::FcntlCmd::F_DUPFD_CLOEXEC(_))
                && self.network_fd_is_capability_probe(guest, fcntl.fd()),
            Syscall::Poll(poll) => read_pollfds(guest, poll.fds().map(|p| p.cast()), poll.nfds())
                .is_ok_and(|fds| fds.iter().any(|p| self.network_fd_is_capability_probe(guest, p.fd))),
            Syscall::Ppoll(poll) => read_pollfds(guest, poll.fds(), poll.nfds())
                .is_ok_and(|fds| fds.iter().any(|p| self.network_fd_is_capability_probe(guest, p.fd))),
            Syscall::Select(select) => read_select_state(guest, select.nfds(), select.readfds(),
                select.writefds(), select.exceptfds(), None, SelectTimeoutAddress::None)
                .is_ok_and(|state| (0..state.nfds).any(|fd| state.requested(fd)
                    && self.network_fd_is_capability_probe(guest, fd))),
            Syscall::Pselect6(select) => read_select_state(guest, select.nfds(), select.readfds(),
                select.writefds(), select.exceptfds(), None, SelectTimeoutAddress::None)
                .is_ok_and(|state| (0..state.nfds).any(|fd| state.requested(fd)
                    && self.network_fd_is_capability_probe(guest, fd))),
            _ => {
                let descriptors = match number {
                    Sysno::read | Sysno::readv | Sysno::pread64 | Sysno::preadv | Sysno::preadv2
                    | Sysno::write | Sysno::writev | Sysno::pwrite64 | Sysno::pwritev | Sysno::pwritev2
                    | Sysno::recvfrom | Sysno::recvmsg | Sysno::recvmmsg
                    | Sysno::sendto | Sysno::sendmsg | Sysno::sendmmsg
                    | Sysno::connect | Sysno::bind | Sysno::listen | Sysno::accept | Sysno::accept4
                    | Sysno::shutdown | Sysno::getsockname | Sysno::getpeername
                    | Sysno::getsockopt | Sysno::setsockopt | Sysno::ioctl | Sysno::vmsplice =>
                        [Some(args.arg0 as i32), None],
                    Sysno::sendfile | Sysno::tee => [Some(args.arg0 as i32), Some(args.arg1 as i32)],
                    Sysno::splice | Sysno::copy_file_range => [Some(args.arg0 as i32), Some(args.arg2 as i32)],
                    Sysno::epoll_ctl => [Some(args.arg2 as i32), None],
                    _ => [None, None],
                };
                descriptors.into_iter().flatten().any(|fd| self.network_fd_is_capability_probe(guest, fd))
            }
        };
        if blocked {
            Err(engine_error(format!("IPv6 capability probe does not authorize {number}")))
        } else {
            Ok(())
        }
    }

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
            Syscall::Recvmmsg(call) => self.network_open_file(guest, call.fd()).is_some(),
            Syscall::Sendmmsg(call) => self.network_open_file(guest, call.sockfd()).is_some(),
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

        if let Err(error) = self.check_network_capability_probe_use(guest, call) {
            return Some(Err(error));
        }

        // Guest memory sampled before submission is not evidence of the bytes
        // Linux later commits, including partial faults or concurrent mutation.
        // Keep network output owned here and refuse before payload access or
        // native submission until the shared engine has an authenticated TX join.
        // Ordinary file/stdout writes and explicit live policy retain dispatch.
        if policy == NetworkPolicy::Record {
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464): original nonblocking TCP Sendto capture.
            if let Syscall::Sendto(send) = call
                && original_sendto_shape(send)
                && self.network_open_file(guest, send.fd()).is_some()
                && guest.thread_state().with_detfd(send.fd(), |fd| fd.is_nonblocking()).unwrap_or(false)
                && guest.local_global_state().and_then(|global| global.native_receive_mode())
                    == Some(crate::network_replay::NetworkEngineMode::Record)
            {
                let result = self.network_original_sendto(guest, send).await;
                return Some(self.finish_original_invocation(guest, result).await);
            }
            let output_fd = match call {
                Syscall::Write(call) => Some(call.fd()),
                Syscall::Writev(call) => Some(call.fd()),
                Syscall::Sendto(call) => Some(call.fd()),
                _ => None,
            };
            if output_fd.is_some_and(|fd| self.network_open_file(guest, fd).is_some()) {
                return Some(Err(engine_error(
                    "Record network output requires authenticated native transmission receipts",
                )));
            }
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
                Some(self.network_read(guest, call, policy, true).await)
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
            Syscall::Recvmmsg(call) if self.network_open_file(guest, call.fd()).is_some() => {
                Some(Err(engine_error(
                    "recvmmsg batch and ancillary capture is not yet implemented",
                )))
            }
            Syscall::Sendmmsg(call) if self.network_open_file(guest, call.sockfd()).is_some() => {
                Some(Err(engine_error(
                    "sendmmsg batch and ancillary capture is not yet implemented",
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
        if self.network_fd_tracking_active(guest) {
            let result = self.network_original_socket(guest, call).await;
            return self.finish_original_invocation(guest, result).await;
        }
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
            crate::network_replay::original_installation::socket_installation_flags(call.r#type()),
            FdType::Socket,
        )
        .await?;
        if crate::network_replay::original_installation::is_udp6_capability_probe(
            call.family(), call.r#type(), call.protocol(),
        ) {
            guest.thread_state().with_detfd(fd, |fd| fd.restrict_network_capability_probe())?;
        }
        self.enroll_fresh_stream_socket(guest, fd, call).await?;
        self.complete_network_fd_installation(guest, admission.as_ref(), fd)
            .await?;
        Ok(i64::from(fd))
    }

    async fn network_original_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        use crate::network_replay::original_connect::Kind;
        let arguments = self
            .stage_original_call(
                guest,
                call.into(),
                Kind::Socket, (call.family(),
                u64::from(call.r#type() as u32),
                call.protocol()),
                crate::OriginalFileExecution::Native,
            )
            .await?;
        let admission = match network_request(
            guest,
            NetworkRequest::NativeBeginOriginalAllocator { arguments },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalConnectAdmission(admission) => admission,
            reply => {
                return Err(engine_error(format!(
                    "Socket admission changed reply {reply:?}"
                )));
            }
        };
        let local = guest.thread_state_mut().original_connect.as_mut().unwrap();
        local.arguments = admission.arguments.clone();
        local.admission = Some(admission.clone());
        self.shadow_ack(
            guest,
            NetworkRequest::NativeSubmitOriginalConnect {
                admission: admission.clone(),
            },
        )
        .await?;
        self.mark_original_syscall_invoked(guest);
        let result = guest.inject(call).await.map_err(Error::from);
        let (admission, outcome, result, returned) = self
            .observe_original_call_result(guest, admission, result)
            .await?;
        // The service has already authenticated the held file against this
        // original installation and positively closed its auxiliary duplicate.
        // No guest fstat/getsockopt buffer is accessed while the table is held.
        // None still needs the publisher's exact Install->Remove proof.
        let stat = if guest.config().virtualize_metadata {
            outcome
                .socket
                .as_ref()
                .map(|observed| observed.metadata.stat)
        } else {
            None
        };
        let enrollment = if let Some(observed) = outcome.socket.as_ref() {
            self.capture_original_socket_profile(guest, call, observed)
                .await?
        } else {
            None
        };
        match network_request(
            guest,
            NetworkRequest::NativePublishOriginalSocket {
                admission: admission.clone(),
                stat,
                enrollment,
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalSocketInstallation(binding)
                if binding.map(|b| i64::from(b.slot.fd)) == (returned >= 0).then_some(returned) => {
            }
            reply => {
                return Err(engine_error(format!(
                    "Socket publication changed original result {reply:?}"
                )));
            }
        };
        self.shadow_ack(
            guest,
            NetworkRequest::NativeRetireOriginalConnect { admission },
        )
        .await?;
        guest.thread_state_mut().original_connect = None;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174): native epoll allocator authority.
    pub(crate) async fn network_original_epoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let result = self.network_original_epoll_inner(guest, call).await;
        self.finish_original_invocation(guest, result).await
    }

    async fn network_original_epoll_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        use crate::network_replay::original_connect::Kind;
        let kind = match call {
            Syscall::EpollCreate(_) => Kind::EpollCreate { legacy: true },
            Syscall::EpollCreate1(_) => Kind::EpollCreate { legacy: false },
            _ => {
                return Err(engine_error(
                    "epoll allocator received another original syscall",
                ));
            }
        };
        let source = self.record_or_replay.original_file_execution(call);
        if source != crate::OriginalFileExecution::Native {
            return Err(engine_error(
                "original epoll admission requires the actual native allocator",
            ));
        }
        let (nr, raw) = call.into_parts();
        // Keep the original syscall and complete raw register tuple. In particular
        // epoll_create(size <= 0) must reach Linux rather than become create1(0).
        let arguments = self
            .stage_original_call(guest, call, kind, (raw.arg0 as i32, nr as u64, 0), source)
            .await?;
        let admission = match network_request(
            guest,
            NetworkRequest::NativeBeginOriginalAllocator { arguments },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalConnectAdmission(admission) => admission,
            reply => {
                return Err(engine_error(format!(
                    "epoll admission changed reply {reply:?}"
                )));
            }
        };
        let local = guest.thread_state_mut().original_connect.as_mut().unwrap();
        local.arguments = admission.arguments.clone();
        local.admission = Some(admission.clone());
        self.shadow_ack(
            guest,
            NetworkRequest::NativeSubmitOriginalConnect {
                admission: admission.clone(),
            },
        )
        .await?;
        self.mark_original_syscall_invoked(guest);
        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;
        if matches!(&result, Err(Error::Tool(_) | Error::Io(_))) {
            return Err(result.unwrap_err());
        }
        let (admission, _, result, returned) = self
            .observe_original_call_result(guest, admission, result)
            .await?;
        match network_request(
            guest,
            NetworkRequest::NativePublishOriginalEpoll {
                admission: admission.clone(),
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalEpollInstallation(binding)
                if binding.map(|b| i64::from(b.slot.fd)) == (returned >= 0).then_some(returned) => {
            }
            reply => {
                return Err(engine_error(format!(
                    "epoll publication changed actual result {reply:?}"
                )));
            }
        }
        self.shadow_ack(
            guest,
            NetworkRequest::NativeRetireOriginalConnect { admission },
        )
        .await?;
        guest.thread_state_mut().original_connect = None;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174): paired native epoll control prerequisite.
    pub(crate) async fn network_original_epoll_ctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCtl,
    ) -> Result<i64, Error> {
        let result = self.network_original_epoll_ctl_inner(guest, call).await;
        self.finish_original_invocation(guest, result).await
    }
    async fn network_original_epoll_ctl_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollCtl,
    ) -> Result<i64, Error> {
        use crate::network_replay::original_connect::Kind;
        let source = self.record_or_replay.original_file_execution(call.into());
        if source != crate::OriginalFileExecution::Native {
            return Err(engine_error(
                "epoll control requires actual native registration authority",
            ));
        }
        let (_, raw) = call.into_parts();
        let arguments = self
            .stage_original_call(
                guest,
                call.into(),
                Kind::EpollCtl, (raw.arg0 as i32,
                raw.arg3 as u64,
                raw.arg1 as i32),
                source,
            )
            .await?;
        let operation = arguments.operation;
        self.begin_original_epoll_ctl_wait(guest, operation).await?;
        let observed = async {
            let admission = self
                .admit_original_call(guest, arguments, source, None)
                .await?;
            self.mark_original_syscall_invoked(guest);
            let result = self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
            if matches!(&result, Err(Error::Tool(_) | Error::Io(_))) {
                return Err(result.unwrap_err());
            }
            // A selected native file may still need another allocator's
            // foreground publication. Keep this same Call outside the run
            // queue through the actual history/effect/retirement observation.
            // There is no fd_read/table exclusion across original input copy.
            self.observe_original_call_result(guest, admission, result)
                .await
        }
        .await;
        // Every returned preparation/delegate/observation error pays the same
        // continuation. Actual terminal cancellation retains Local/Call for
        // backend final wait; it cannot invent a native result or handback.
        self.finish_original_epoll_ctl_wait(guest, operation).await;
        let (admission, _, result, _) = observed?;
        self.shadow_ack(
            guest,
            NetworkRequest::NativeRetireOriginalConnect { admission },
        )
        .await?;
        guest.thread_state_mut().original_connect = None;
        result
    }

    pub(crate) async fn network_original_openat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<
        (
            i64,
            crate::network_runtime::original_installation::OpenatPublication,
        ),
        Error,
    > {
        let (result, published) = match self.network_original_openat_inner(guest, call).await {
            Ok((raw, published)) => (Ok(raw), Some(published)),
            Err(error) => (Err(error), None),
        };
        let raw = self.finish_original_invocation(guest, result).await?;
        Ok((
            raw,
            published.ok_or_else(|| engine_error("Openat lost its completed publication"))?,
        ))
    }

    /// Existing external-IO turn protocol only. Local syscall latency is not
    /// an external network input. These adapters do not prove independence or
    /// activate Openat/EpollCtl in the initial Record gate.
    async fn begin_original_local_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        operation: ExternalOpId,
        label: &str,
        lost_grant: &str,
    ) -> Result<(), Error> {
        if guest.config().sequentialize_threads {
            let mut request = Resources::new(guest.thread_state().dettid);
            request.insert(ResourceID::BlockingExternalIO(operation), Permission::RW);
            request.fyi(label);
            if resource_request(guest, request).await != crate::tool_global::ResumeStatus::Normal {
                return Err(engine_error(lost_grant));
            }
        }
        Ok(())
    }

    async fn finish_original_local_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        operation: ExternalOpId,
        label: &str,
    ) {
        if guest.config().sequentialize_threads {
            let mut continuation = Resources::new(guest.thread_state().dettid);
            continuation.insert(
                ResourceID::BlockedExternalContinue(operation),
                Permission::RW,
            );
            continuation.fyi(label);
            // The response is scheduler handback, never a replacement native
            // result. An actual signal/terminal path keeps its existing owner.
            resource_request(guest, continuation).await;
        }
    }

    pub(crate) async fn begin_original_openat_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        operation: ExternalOpId,
    ) -> Result<(), Error> {
        self.begin_original_local_wait(
            guest,
            operation,
            "original openat and held-file observation",
            "Openat lost its actual external-IO grant before invocation",
        )
        .await
    }

    pub(crate) async fn finish_original_openat_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        operation: ExternalOpId,
    ) {
        self.finish_original_local_wait(
            guest,
            operation,
            "original openat and held-file observation complete",
        )
        .await;
    }

    pub(crate) async fn begin_original_epoll_ctl_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        operation: ExternalOpId,
    ) -> Result<(), Error> {
        self.begin_original_local_wait(
            guest,
            operation,
            "epoll_ctl",
            "epoll control lost its actual external-IO grant before invocation",
        )
        .await
    }

    pub(crate) async fn finish_original_epoll_ctl_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        operation: ExternalOpId,
    ) {
        self.finish_original_local_wait(guest, operation, "epoll_ctl")
            .await;
    }

    async fn network_original_openat_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<
        (
            i64,
            crate::network_runtime::original_installation::OpenatPublication,
        ),
        Error,
    > {
        use crate::network_replay::original_connect::Kind;
        let source = self.record_or_replay.original_file_execution(call.into());
        if source != crate::OriginalFileExecution::Native {
            return Err(engine_error(
                "original Openat admission requires the actual native allocator",
            ));
        }
        let (_, raw) = call.into_parts();
        let arguments = self
            .stage_original_call(
                guest,
                call.into(),
                Kind::Openat, (raw.arg0 as i32,
                raw.arg1 as u64,
                raw.arg2 as i32),
                source,
            )
            .await?;
        let operation = arguments.operation;
        self.begin_original_openat_wait(guest, operation).await?;
        let observed = async {
            let admission = match network_request(
                guest,
                NetworkRequest::NativeBeginOriginalAllocator { arguments },
            )
            .await
            .map_err(engine_rpc_error)?
            {
                NetworkReply::OriginalConnectAdmission(admission) => admission,
                reply => {
                    return Err(engine_error(format!(
                        "Openat admission changed reply {reply:?}"
                    )));
                }
            };
            let local = guest.thread_state_mut().original_connect.as_mut().unwrap();
            local.arguments = admission.arguments.clone();
            local.admission = Some(admission.clone());
            self.shadow_ack(
                guest,
                NetworkRequest::NativeSubmitOriginalConnect {
                    admission: admission.clone(),
                },
            )
            .await?;
            self.mark_original_syscall_invoked(guest);
            // Keep the actual delegate/registers. Neither an ordinary turn nor
            // a table permit spans FIFO open, original uaccess or helper getattr.
            let result = self
                .record_or_replay_preserving_tool_errors(guest, call)
                .await;
            if matches!(&result, Err(Error::Tool(_) | Error::Io(_))) {
                return Err(result.unwrap_err());
            }
            let (admission, _, result, returned) = self
                .observe_original_call_result(guest, admission, result)
                .await?;
            self.shadow_ack(
                guest,
                NetworkRequest::NativeObserveOriginalOpenat {
                    admission: admission.clone(),
                },
            )
            .await?;
            Ok((admission, result, returned))
        }
        .await;
        // Every returned admission/delegate/observation error pays the same
        // continuation before fail_original_connect publishes failure. A future
        // canceled by actual task termination retains the original Call instead.
        self.finish_original_openat_wait(guest, operation).await;
        let (admission, result, returned) = observed?;
        let published = match network_request(
            guest,
            NetworkRequest::NativePublishOriginalOpenat {
                admission: admission.clone(),
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalOpenatInstallation(published)
                if published.binding.map(|b| i64::from(b.slot.fd))
                    == (returned >= 0).then_some(returned) =>
            {
                published
            }
            reply => {
                return Err(engine_error(format!(
                    "Openat publication changed original result {reply:?}"
                )));
            }
        };
        self.shadow_ack(
            guest,
            NetworkRequest::NativeRetireOriginalConnect { admission },
        )
        .await?;
        guest.thread_state_mut().original_connect = None;
        result.map(|raw| (raw, published))
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
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW: original fdget and kernel-copy receipt consumer.
        // Replay remains on its existing unactivated path until real topology
        // has been prepared. The admitted Record path never pre-reads sockaddr.
        if policy == NetworkPolicy::Record && self.original_connect_record_route(guest).await? {
            return self.network_original_connect(guest, call).await;
        }
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
                        self.wait_for_network(
                            guest,
                            open_file,
                            NetworkWaitKind::Readable,
                            call.signal_interrupt_errno(),
                        )
                        .await?;
                    }
                    reply => {
                        break Err(engine_error(format!("unexpected connect reply {reply:?}")));
                    }
                }
            },
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    /// Original Record connect: actual kernel selection, copy and return join
    /// the same Call which held the table before capture. The provider Driver
    /// releases that table at positive fdget, even if sockaddr uaccess blocks.
    async fn network_original_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        let result = self.network_original_connect_inner(guest, call).await;
        self.finish_original_invocation(guest, result).await
    }

    pub(crate) async fn finish_original_invocation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        result: Result<i64, Error>,
    ) -> Result<i64, Error> {
        if let Err(error) = &result
            && let Some(local) = guest.thread_state().original_connect.clone()
        {
                // Returning a Tool error would let the backend drop this task
                // before its final wait. Publish the existing run-failure fence
                // and retain this future until exact-task cleanup cancels it.
                // A completed native errno has already retired Local custody.
                let published = network_request(
                    guest,
                    NetworkRequest::NativeOriginalConnectFailed {
                        local,
                        detail: format!("{error:#}"),
                    },
                )
                .await;
                if !matches!(published, Ok(NetworkReply::Unit)) {
                    tracing::error!(
                        ?published,
                        "original Connect failure publication was not acknowledged"
                    );
                }
                return futures::future::pending().await;
            }
        result
    }

    async fn prepare_original_call_from<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        kind: crate::network_replay::original_connect::Kind,
        (fd, address, length): (i32, u64, i32),
        source: crate::OriginalFileExecution,
    ) -> Result<crate::network_replay::original_connect::Admission, Error> {
        let arguments = self
            .stage_original_call(guest, call, kind, (fd, address, length), source)
            .await?;
        self.admit_original_call(guest, arguments, source, None)
            .await
    }

    /// Install exact local cancellation custody before the first admission
    /// await. The binding here is only a preview until the selected grant.
    async fn stage_original_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        kind: crate::network_replay::original_connect::Kind,
        (fd, address, length): (i32, u64, i32),
        source: crate::OriginalFileExecution,
    ) -> Result<crate::network_replay::original_connect::Arguments, Error> {
        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Local;
        if source == crate::OriginalFileExecution::Native && !self.network_fd_tracking_active(guest)
        {
            return Err(engine_error(
                "original syscall requires complete FD-table mutation custody",
            ));
        }
        if source == crate::OriginalFileExecution::Native
            && (!<Self as reverie::Tool>::observe_injected_syscalls(guest.config())
                || !<Self as reverie::Tool>::observe_injected_syscall_preparation(guest.config()))
        {
            return Err(engine_error(
                "original syscall requires actual backend preparation observation",
            ));
        }
        if !kind.allocator() {
            self.publish_network_fd_installations(guest).await?;
        }
        let (_, raw) = call.into_parts();
        let arguments = {
            let state = guest.thread_state();
            let metadata = state.file_metadata.lock().unwrap();
            Arguments {
                kind,
                operation: ExternalOpId::new(state.dettid, state.stats.syscall_count),
                files: metadata.files_id,
                binding: if kind.allocator()
                    || kind == crate::network_replay::original_connect::Kind::EpollCtl
                {
                    None // An allocator does not select the returned descriptor before entry
                } else {
                    metadata.descriptor_binding(fd).ok()
                },
                fd,
                address,
                length,
                original_count: match kind {
                    crate::network_replay::original_connect::Kind::Read
                    | crate::network_replay::original_connect::Kind::Sendto => raw.arg2 as u64,
                    crate::network_replay::original_connect::Kind::Openat => raw.arg3 as u64,
                    crate::network_replay::original_connect::Kind::EpollCtl => {
                        u64::from(raw.arg2 as u32)
                    }
                    _ => 0,
                },
            }
        };
        if guest.thread_state().original_connect.is_some() {
            return Err(engine_error("original Connect already has local custody"));
        }
        guest.thread_state_mut().original_connect = Some(Local {
            arguments: arguments.clone(),
            raw_arguments: [raw.arg0, raw.arg1, raw.arg2, raw.arg3, raw.arg4, raw.arg5],
            admission: None,
            invoked: false,
            returned: None,
        });
        Ok(arguments)
    }

    async fn admit_original_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        arguments: crate::network_replay::original_connect::Arguments,
        source: crate::OriginalFileExecution,
        read: Option<crate::network_replay::NetworkFdReadAdmission>,
    ) -> Result<crate::network_replay::original_connect::Admission, Error> {
        let request = match (source, read) {
            (crate::OriginalFileExecution::Native, Some(read)) => {
                NetworkRequest::NativeBeginOriginalExternalFromRead { arguments, read }
            }
            (crate::OriginalFileExecution::Native, None) => {
                NetworkRequest::NativeBeginOriginalConnect { arguments }
            }
            (crate::OriginalFileExecution::Recorded, None) => {
                NetworkRequest::BeginRecordedOriginalFile { arguments }
            }
            (crate::OriginalFileExecution::Recorded, Some(_)) => {
                return Err(engine_error(
                    "external read transfer cannot produce a recorded result",
                ));
            }
        };
        let admission = match network_request(guest, request)
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalConnectAdmission(admission) => admission,
            reply => {
                return Err(engine_error(format!(
                    "unexpected original Connect admission {reply:?}"
                )));
            }
        };
        let local = guest.thread_state_mut().original_connect.as_mut().unwrap();
        local.arguments = admission.arguments.clone();
        local.admission = Some(admission.clone());
        if source == crate::OriginalFileExecution::Native {
            self.shadow_ack(
                guest,
                NetworkRequest::NativeSubmitOriginalConnect {
                    admission: admission.clone(),
                },
            )
            .await?;
        }
        Ok(admission)
    }

    async fn observe_original_call_result<G: Guest<Self>>(
        &self,
        guest: &mut G,
        admission: crate::network_replay::original_connect::Admission,
        result: Result<i64, Error>,
    ) -> Result<
        (
            crate::network_replay::original_connect::Admission,
            crate::network_runtime::original_connect::Outcome,
            Result<i64, Error>,
            i64,
        ),
        Error,
    > {
        // A synthetic injection interruption is not a native syscall return.
        // Refuse immediately with custody retained; never wait forever or turn
        // a missing provider session into a successful negative observation.
        let returned = guest
            .thread_state()
            .original_connect
            .as_ref()
            .and_then(|local| local.returned)
            .ok_or_else(|| engine_error("original Connect has no real backend completion"))?;
        let outcome = match network_request(
            guest,
            NetworkRequest::NativeOriginalConnectOutcome {
                admission: admission.clone(),
            },
        )
        .await
        .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalConnectOutcome(outcome)
                if outcome.admission == admission && outcome.returned == returned =>
            {
                outcome
            }
            reply => {
                return Err(engine_error(format!(
                    "original Connect changed retained completion {reply:?}"
                )));
            }
        };
        let actual = match &result {
            Ok(value) => Some(*value),
            Err(error) => error_errno(error).map(|errno| -i64::from(errno)),
        };
        if actual != Some(returned) {
            return Err(engine_error(
                "original Connect backend and guest completion disagree",
            ));
        }
        Ok((admission, *outcome, result, returned))
    }

    async fn network_original_invoke<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        kind: crate::network_replay::original_connect::Kind,
        fd: i32,
        address: u64,
        length: i32,
    ) -> Result<
        (
            crate::network_replay::original_connect::Admission,
            crate::network_runtime::original_connect::Outcome,
            Result<i64, Error>,
            i64,
        ),
        Error,
    > {
        let mut arguments = self
            .stage_original_call(
                guest,
                call,
                kind, (fd,
                address,
                length),
                crate::OriginalFileExecution::Native,
            )
            .await?;
        // A queued request owns no table. Strict mode admits this lookup at
        // the existing selected external grant; NoSeq uses the same engine's
        // atomic read admission without inventing a scheduler request.
        let read = if guest.config().sequentialize_threads {
            let state = guest.thread_state();
            let mut resources = Resources::new(state.dettid);
            resources.insert(
                ResourceID::BlockingNetworkCapture(arguments.operation),
                Permission::RW,
            );
            resources.fyi(call.name());
            resources.fd_read = Some(crate::scheduler::fd_read::FdReadIntent {
                owner: crate::network_replay::NetworkStreamOwner {
                    thread: state.dettid,
                    mm: state.mm_id,
                },
                files: arguments.files,
                fd,
                operation: arguments.operation,
            });
            match crate::tool_global::fd_read_resource_request(guest, resources).await {
                crate::scheduler::parked::ResourceReply::ReadGrant {
                    status: crate::tool_global::ResumeStatus::Normal,
                    read,
                } => *read,
                // An actual cancelled/terminal transport is handled by the
                // existing backend consuming path. Never inject this numeric
                // FD using a signal response in place of an owned lookup.
                _ => {
                    return Err(engine_error(
                        "original external invocation lost its selected grant",
                    ));
                }
            }
        } else {
            self.begin_network_fd_read(guest, fd).await?
        };
        arguments.binding = read.binding;
        guest
            .thread_state_mut()
            .original_connect
            .as_mut()
            .expect("local custody precedes selected request")
            .arguments = arguments.clone();
        let admission = self
            .admit_original_call(
                guest,
                arguments,
                crate::OriginalFileExecution::Native,
                Some(read),
            )
            .await?;
        let result = self
            .live_network_syscall_after_grant(
                guest,
                call,
                NetworkPhysicalCompletion::OriginalConnect,
            )
            .await;
        self.observe_original_call_result(guest, admission, result)
            .await
    }

    fn mark_original_syscall_invoked<G: Guest<Self>>(&self, guest: &mut G) {
        let local = guest
            .thread_state_mut()
            .original_connect
            .as_mut()
            .expect("original Connect marker installed before admission");
        assert!(!local.invoked && local.admission.is_some());
        local.invoked = true;
    }

    async fn network_original_connect_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Connect,
    ) -> Result<i64, Error> {
        use crate::network_replay::original_connect::Kind;
        let (_, raw) = call.into_parts();
        let (admission, outcome, result, returned) = self
            .network_original_invoke(
                guest,
                call.into(),
                Kind::Connect,
                call.fd(),
                raw.arg1 as u64,
                call.addrlen(),
            )
            .await?;
        let capture = self
            .capture_original_connect(guest, &admission, &outcome, returned)
            .await;
        // Retire even if semantic capture refuses; a failed run must not turn
        // into an unbounded Call/transport journal. Physical errors retain custody.
        let retirement = self
            .shadow_ack(
                guest,
                NetworkRequest::NativeRetireOriginalConnect { admission },
            )
            .await;
        if retirement.is_ok() {
            guest.thread_state_mut().original_connect = None;
        }
        finish_shadow_operation(result, capture.and(retirement))
    }

    /// Semantic capture of a returned original Connect, before retirement.
    /// The V4 recorder publishes from the retained native completion; the V3
    /// stream capture stays the only other consumer and refuses a V4 engine.
    pub(crate) async fn capture_original_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        admission: &crate::network_replay::original_connect::Admission,
        outcome: &crate::network_runtime::original_connect::Outcome,
        returned: i64,
    ) -> Result<(), Error> {
        use crate::network_replay::original_connect::Pin;
        let native_record = guest
            .local_global_state()
            .and_then(|global| global.native_receive_mode())
            == Some(crate::network_replay::NetworkEngineMode::Record);
        {
            if let Some(Pin::Socket {
                domain,
                kind,
                protocol,
            }) = &outcome.pin
            {
                // Local validation/copy failures need no external channel. A
                // positive native copy supplies bytes; no late guest reread or
                // fd-based getsockopt supplies this original operation's facts.
                if let Some(bytes) = &outcome.address {
                    // Linux can reject a successfully copied but too-short
                    // sockaddr before any external endpoint exists. Preserve
                    // that actual error just like native EBADF/copy-EFAULT.
                    let peer = match read_captured_network_address(bytes) {
                        Ok(peer) => peer,
                        Err(_) if returned < 0 && captured_sockaddr_too_short(bytes) => {
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    };
                    let transport = classify_stream_transport([*domain, *kind, *protocol])?;
                    let open_file = admission
                        .arguments
                        .binding
                        .ok_or_else(|| engine_error("selected socket lost its admitted OFD"))?
                        .open_file;
                    if native_record && returned != 0 && returned != -i64::from(libc::EINPROGRESS) {
                        // Only the original -EINPROGRESS plus an independently
                        // observed, already-established retained pin is added.
                        // Other failures and pending handshakes remain refused.
                        return Err(engine_error(
                            "V4 Record Connect error result has no native publisher",
                        ));
                    }
                    self.ensure_channel(
                        guest,
                        open_file,
                        NetworkChannelBinding {
                            transport,
                            role: NetworkEndpointRoleV2::OutboundClient,
                            peer_address: Some(peer),
                            requested_local_constraint: None,
                            observed_local_address: None,
                            accepted_from: None,
                            selected_channel: None,
                        },
                    )
                    .await?;
                    if native_record {
                        // The paid continuation made this the foreground turn;
                        // the provider and pin are retired but not consumed.
                        let global = guest.local_global_state().ok_or_else(|| {
                            engine_error("V4 Connect publication lost actual local global state")
                        })?;
                        return global
                            .publish_foreground_native_connected(
                                guest.tid(),
                                guest.thread_state(),
                                admission,
                            )
                            .map_err(engine_rpc_error);
                    }
                    let observed_at = thread_observe_time(guest).await;
                    let connection = if returned == 0 {
                        NetworkConnectionResultV2::Connected
                    } else {
                        NetworkConnectionResultV2::Error(
                            i32::try_from(-returned).map_err(engine_error)?,
                        )
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
                } else if returned == 0 {
                    return Err(engine_error(
                        "successful original Connect lacks copied address",
                    ));
                }
            }
            Ok(())
        }
    }

    /// Execute the already supported F_GETFL through the shared original Call.
    /// Prepared snapshots only the admitted local virtual flag. The provider's
    /// actual fdget_raw owns file selection; original sys_exit and the backend
    /// raw completion own the result. A later reused numeric FD is never read.
    pub(crate) async fn network_original_get_flags<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        let result = self.original_get_flags_inner(guest, call).await;
        self.finish_original_invocation(guest, result).await
    }

    async fn original_get_flags_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        use crate::network_replay::original_connect::FileOperation;
        use crate::network_replay::original_connect::Kind;
        let operation = FileOperation::GetFlags;
        if guest.thread_state().original_file_metadata.is_some() {
            return Err(engine_error(
                "previous file metadata observation remains unconsumed",
            ));
        }
        let source = self.record_or_replay.original_file_execution(call.into());
        let admission = self
            .prepare_original_call_from(
                guest,
                call.into(),
                Kind::File(operation), (call.fd(),
                operation.syscall() as u64,
                operation.command()),
                source,
            )
            .await?;
        let result = match source {
            crate::OriginalFileExecution::Native => {
                self.mark_original_syscall_invoked(guest);
                // The ordinary caller keeps its original turn/resources.
                let result = self
                    .record_or_replay_preserving_tool_errors(guest, call)
                    .await;
                if matches!(&result, Err(Error::Tool(_) | Error::Io(_))) {
                    // Delegate failure supplies no kernel result. Preserve its
                    // original diagnostic for finish_original_invocation's
                    // retained-custody failure fence; do not replace it with a
                    // missing-backend-return error or fabricate a completion.
                    return result;
                }
                self.observe_original_call_result(guest, admission.clone(), result)
                    .await?
                    .2
            }
            crate::OriginalFileExecution::Recorded => {
                let observed = guest
                    .thread_state()
                    .file_metadata
                    .lock()
                    .unwrap()
                    .observe_original_file_metadata(&admission)
                    .map_err(engine_error)?;
                guest.thread_state_mut().original_file_metadata = Some(observed);
                self.shadow_ack(
                    guest,
                    NetworkRequest::SelectRecordedOriginalFile {
                        admission: admission.clone(),
                    },
                )
                .await?;
                // This producer consumes the same Return event as legacy replay.
                // It never injects and never marks Local.invoked/returned.
                self.record_or_replay
                    .consume_recorded_original_file(&mut guest.into_guest(), call.into())
                    .await
            }
        };
        let observed_return = match &result {
            Ok(value) => Some(*value),
            Err(Error::Errno(errno)) => Some(-i64::from(errno.into_raw())),
            Err(_) => None,
        };
        if observed_return.is_none() {
            return result;
        }
        let adjusted = (|| -> Result<i64, Error> {
            let observed = guest
                .thread_state()
                .original_file_metadata
                .as_ref()
                .filter(|observation| observation.admission == admission)
                .ok_or_else(|| {
                    engine_error("original file lacks its admitted metadata observation")
                })?;
            let returned_flags = result?;
            let logical_nonblocking = observed
                .logical_nonblocking
                .ok_or_else(|| engine_error("successful F_GETFL had no admitted descriptor"))?;
            let nonblocking = i64::from(OFlag::O_NONBLOCK.bits());
            Ok(if logical_nonblocking {
                returned_flags | nonblocking
            } else {
                returned_flags & !nonblocking
            })
        })();
        let retirement = match source {
            crate::OriginalFileExecution::Native => {
                self.shadow_ack(
                    guest,
                    NetworkRequest::NativeRetireOriginalConnect { admission },
                )
                .await
            }
            crate::OriginalFileExecution::Recorded => {
                let returned = observed_return.expect("only a consumed Return reaches retirement");
                self.shadow_ack(
                    guest,
                    NetworkRequest::CompleteRecordedOriginalFile {
                        admission,
                        returned,
                    },
                )
                .await
            }
        };
        if retirement.is_ok() {
            guest.thread_state_mut().original_connect = None;
            guest.thread_state_mut().original_file_metadata = None;
        }
        finish_shadow_operation(adjusted, retirement)
    }

    /// Transfer one actual scalar Read attempt to the existing original Call.
    /// The caller has already obtained the final ordinary/external grant. This
    /// helper neither asks for a resource nor retries or moves guest bytes.
    pub(crate) async fn original_read_attempt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        read: crate::network_replay::NetworkFdReadAdmission,
        delegated: bool,
    ) -> Result<i64, Error> {
        let result = async {
            let source = if delegated {
                self.record_or_replay.original_file_execution(call.into())
            } else {
                crate::OriginalFileExecution::Native
            };
            let admission = self
                .begin_original_read_call(guest, call, read, Some(source))
                .await?;
            let result = match source {
                crate::OriginalFileExecution::Native => self
                    .execute_original_read_call(guest, call, admission.clone(), delegated)
                    .await?
                    .map_err(Error::from),
                crate::OriginalFileExecution::Recorded => {
                    let observed = guest
                        .thread_state()
                        .file_metadata
                        .lock()
                        .unwrap()
                        .observe_original_file_metadata(&admission)
                        .map_err(engine_error)?;
                    guest.thread_state_mut().original_file_metadata = Some(observed);
                    self.shadow_ack(
                        guest,
                        NetworkRequest::SelectRecordedOriginalFile {
                            admission: admission.clone(),
                        },
                    )
                    .await?;
                    match self
                        .record_or_replay
                        .invoke_original_read(&mut guest.into_guest(), call)
                        .await?
                    {
                        reverie::InjectedReadResult::Complete(result) => {
                            result.map_err(Error::from)
                        }
                        reverie::InjectedReadResult::RecordedInterruption(ticket) => {
                            self.shadow_ack(
                                guest,
                                NetworkRequest::CompleteRecordedReadInterruption {
                                    admission: admission.clone(),
                                },
                            )
                            .await?;
                            guest.thread_state_mut().original_connect = None;
                            guest.thread_state_mut().original_file_metadata = None;
                            return Err(Error::Tool(anyhow::Error::new(ticket)));
                        }
                        reverie::InjectedReadResult::Interrupted(_) => {
                            return Err(engine_error(
                                "recorded Read acquired native interruption custody",
                            ));
                        }
                    }
                }
            };
            let returned = match &result {
                Ok(value) => *value,
                Err(Error::Errno(errno)) => -i64::from(errno.into_raw()),
                Err(_) => return result,
            };
            let cleanup = match source {
                crate::OriginalFileExecution::Native => {
                    self.shadow_ack(
                        guest,
                        NetworkRequest::NativeRetireOriginalConnect { admission },
                    )
                    .await
                }
                crate::OriginalFileExecution::Recorded => {
                    self.shadow_ack(
                        guest,
                        NetworkRequest::CompleteRecordedOriginalFile {
                            admission,
                            returned,
                        },
                    )
                    .await
                }
            };
            if cleanup.is_ok() {
                guest.thread_state_mut().original_connect = None;
                guest.thread_state_mut().original_file_metadata = None;
            }
            finish_shadow_operation(result, cleanup)
        }
        .await;
        self.finish_original_invocation(guest, result).await
    }

    /// Execute one already admitted Read through the same backend boundary for
    /// ordinary and network callers. The inner result is an authenticated native
    /// return; interruption and ownership/delegate failures remain outer errors.
    /// This helper adds no scheduling request and moves no guest payload bytes.
    async fn execute_original_read_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        admission: crate::network_replay::original_connect::Admission,
        delegated: bool,
    ) -> Result<Result<i64, Errno>, Error> {
        self.execute_original_read_with_outcome(guest, call, admission, delegated)
            .await
            .map(|(result, _)| result)
    }

    async fn execute_original_read_with_outcome<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        admission: crate::network_replay::original_connect::Admission,
        delegated: bool,
    ) -> Result<
        (
            Result<i64, Errno>,
            crate::network_runtime::original_connect::Outcome,
        ),
        Error,
    > {
        self.mark_original_syscall_invoked(guest);
        let outcome = if delegated {
            self.record_or_replay
                .invoke_original_read(&mut guest.into_guest(), call)
                .await?
        } else {
            guest.inject_original_read(call).await
        };
        match outcome {
            reverie::InjectedReadResult::RecordedInterruption(_) => Err(engine_error(
                "native Read received recorded interruption control",
            )),
            reverie::InjectedReadResult::Complete(result) => {
                let (_, outcome, observed, _) = self
                    .observe_original_call_result(guest, admission, result.map_err(Error::from))
                    .await?;
                match observed {
                    Ok(value) => Ok((Ok(value), outcome)),
                    Err(Error::Errno(errno)) => Ok((Err(errno), outcome)),
                    Err(error) => Err(error),
                }
            }
            reverie::InjectedReadResult::Interrupted(ticket) => {
                let local = guest
                    .thread_state()
                    .original_connect
                    .as_ref()
                    .ok_or_else(|| engine_error("interrupted Read lost Local"))?;
                if local.invoked
                    || local.returned.is_some()
                    || local.admission.as_ref() != Some(&admission)
                {
                    return Err(engine_error(
                        "interrupted Read lacks actual backend no-entry observation",
                    ));
                }
                self.shadow_ack(
                    guest,
                    NetworkRequest::NativeRetireInterruptedRead { admission },
                )
                .await?;
                guest.thread_state_mut().original_connect = None;
                guest.thread_state_mut().original_file_metadata = None;
                Err(Error::Tool(anyhow::Error::new(ticket)))
            }
        }
    }

    /// Install Local before the consuming RPC; a lost reply remains owned by
    /// the same Call and actual ThreadState cleanup. None denotes Detcore's
    /// modeled Read, never an unknown or inferred native completion.
    pub(crate) async fn begin_original_read_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        read: crate::network_replay::NetworkFdReadAdmission,
        source: Option<crate::OriginalFileExecution>,
    ) -> Result<crate::network_replay::original_connect::Admission, Error> {
        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Kind;
        use crate::network_replay::original_connect::Local;
        let (_, raw) = call.into_parts();
        let state = guest.thread_state();
        if state.original_connect.is_some() || state.original_file_metadata.is_some() {
            return Err(engine_error(
                "Read attempt overlaps retained original invocation",
            ));
        }
        if source == Some(crate::OriginalFileExecution::Native)
            && (!<Self as reverie::Tool>::observe_injected_syscalls(guest.config())
                || !<Self as reverie::Tool>::observe_injected_syscall_preparation(guest.config()))
        {
            return Err(engine_error(
                "Read requires actual backend preparation and completion observations",
            ));
        }
        let arguments = Arguments {
            kind: Kind::Read,
            operation: ExternalOpId::new(state.dettid, state.stats.syscall_count),
            files: read.publication.permit.files,
            binding: read.binding,
            fd: call.fd(),
            address: raw.arg1 as u64,
            length: 0,
            original_count: raw.arg2 as u64,
        };
        guest.thread_state_mut().original_connect = Some(Local {
            arguments: arguments.clone(),
            raw_arguments: [raw.arg0, raw.arg1, raw.arg2, raw.arg3, raw.arg4, raw.arg5],
            admission: None,
            invoked: false,
            returned: None,
        });
        let request = match source {
            Some(source) => NetworkRequest::BeginOriginalFileFromRead {
                arguments,
                read,
                source,
            },
            None => NetworkRequest::BeginEmulatedReadFromRead { arguments, read },
        };
        let admission = match network_request(guest, request)
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::OriginalConnectAdmission(admission) => admission,
            other => return Err(engine_error(format!("unexpected Read admission {other:?}"))),
        };
        let local = guest
            .thread_state_mut()
            .original_connect
            .as_mut()
            .expect("staged Read owner");
        local.arguments = admission.arguments.clone();
        local.admission = Some(admission.clone());
        if source == Some(crate::OriginalFileExecution::Native) {
            self.shadow_ack(
                guest,
                NetworkRequest::NativeSubmitOriginalConnect {
                    admission: admission.clone(),
                },
            )
            .await?;
        }
        Ok(admission)
    }

    pub(crate) async fn finish_emulated_read_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        admission: crate::network_replay::original_connect::Admission,
        result: Result<i64, Error>,
    ) -> Result<i64, Error> {
        let returned = match &result {
            Ok(value) => Some(*value),
            Err(Error::Errno(errno)) => Some(-i64::from(errno.into_raw())),
            Err(_) => None,
        };
        let result = if let Some(returned) = returned {
            let cleanup = self
                .shadow_ack(
                    guest,
                    NetworkRequest::CompleteEmulatedRead {
                        admission,
                        returned,
                    },
                )
                .await;
            if cleanup.is_ok() {
                guest.thread_state_mut().original_connect = None;
            }
            finish_shadow_operation(result, cleanup)
        } else {
            result
        };
        self.finish_original_invocation(guest, result).await
    }

    /// SYS_close uses the same admitted Call and actual completion channel.
    /// Early physical removal is published by the Driver, before flush returns;
    /// this wrapper does not remove by errno or retry the numeric descriptor.
    pub(super) async fn network_original_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<i64, Error> {
        let result = async {
            let (admission, outcome, result, _) = self
                .network_original_invoke(
                    guest,
                    call.into(),
                    crate::network_replay::original_connect::Kind::Close,
                    call.fd(),
                    0,
                    0,
                )
                .await?;
            if outcome.pin.is_some() || outcome.address.is_some() {
                return Err(engine_error(
                    "close completion contains a duplicate file pin or connect copy",
                ));
            }
            let retirement = self
                .shadow_ack(
                    guest,
                    NetworkRequest::NativeRetireOriginalConnect { admission },
                )
                .await;
            if retirement.is_ok() {
                guest.thread_state_mut().original_connect = None;
            }
            finish_shadow_operation(result, retirement)
        }
        .await;
        self.finish_original_invocation(guest, result).await
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
            // Trace-v2 has no reserved-FD/copyout receipt. Refuse before
            // consuming an outcome or creating any native/semantic descriptor.
            NetworkPolicy::Replay => Err(engine_error(
                "Replay accept requires original reservation/copyout authority",
            )),
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    pub(crate) async fn owned_network_read_uses_shadow<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
    ) -> Result<bool, Error> {
        Ok(self.shadow_socket_state(guest, open_file).await?.is_some())
    }

    /// Consume the scalar dispatcher's single admitted observation. The V3
    /// handoff moves its permit/control into capture_publication; no numeric
    /// descriptor classification is repeated after the transfer.
    pub(crate) async fn network_read_from_admission<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        policy: NetworkPolicy,
        read: crate::network_replay::NetworkFdReadAdmission,
        metadata: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<i64, Error> {
        let open_file = metadata
            .socket
            .ok_or_else(|| engine_error("owned network Read lost socket identity"))?;
        let nonblocking = metadata
            .nonblocking
            .ok_or_else(|| engine_error("owned Read lost status flags"))?;
        if call.len() == 0 {
            // Zero payload does not prove a successful syscall. Preserve the
            // original pointer, selected file and Linux validation/restart path
            // in both policies. Network-owned Read deliberately bypasses the
            // ordinary Recorder/Replayer: local validation is not peer ingress.
            // The existing Call still requires real Prepared/Returned evidence
            // or positive cancellation and exact retirement before handback.
            return self.original_read_attempt(guest, call, read, false).await;
        }
        let native_mode = guest
            .local_global_state()
            .and_then(|global| global.native_receive_mode());
        if let Some(mode) = native_mode {
            return self.network_scalar_receive_from_admission(
                guest, call.into(), mode, read, metadata,
            ).await;
        }
        if self.shadow_socket_state(guest, open_file).await?.is_some() {
            let pin = self
                .capture_admitted_host_stream_call(guest, read, metadata)
                .await?;
            let segments = [(call.buf().map_or(0, |address| address.as_raw()), call.len())];
            return self
                .shadow_stream_receive_with_pin(
                    guest,
                    pin,
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
        match policy {
            NetworkPolicy::Record => {
                let result = async {
                    let admission = self
                        .begin_original_read_call(
                            guest,
                            call,
                            read,
                            Some(crate::OriginalFileExecution::Native),
                        )
                        .await?;
                    // The existing network grant precedes this shared Read
                    // boundary; capture still consumes only its actual result.
                    let result = self
                        .execute_original_read_call(guest, call, admission.clone(), false)
                        .await?
                        .map_err(Error::from);
                    let capture = self
                        .capture_scalar_network_read(guest, open_file, call, &result)
                        .await;
                    let cleanup = self
                        .shadow_ack(
                            guest,
                            NetworkRequest::NativeRetireOriginalConnect { admission },
                        )
                        .await;
                    if cleanup.is_ok() {
                        guest.thread_state_mut().original_connect = None;
                        guest.thread_state_mut().original_file_metadata = None;
                    }
                    finish_shadow_operation(result, capture.and(cleanup))
                }
                .await;
                self.finish_original_invocation(guest, result).await
            }
            NetworkPolicy::Replay => {
                let admission = self
                    .begin_original_read_call(guest, call, read, None)
                    .await?;
                let result = self
                    .replay_scalar_network_read(guest, open_file, call, nonblocking)
                    .await;
                self.finish_emulated_read_call(guest, admission, result)
                    .await
            }
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => unreachable!(),
        }
    }

    /// Both scalar syscalls retain their exact original backend tuple while
    /// sharing the existing bounded V4 issuer, publication and cleanup owners.
    pub(crate) async fn network_scalar_receive_from_admission<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: crate::tool_global::ScalarReceive,
        mode: crate::network_replay::NetworkEngineMode,
        read: crate::network_replay::NetworkFdReadAdmission,
        metadata: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<i64, Error> {
        // Range precedence belongs to the authenticated, still-stopped
        // original Read/Recvfrom. It grants neither mapped access nor a Store.
        let range =
            crate::tool_global::CheckedReadRange::inspect(guest, call, &read, metadata);
        let checked_range = match range {
            Ok(range) => range,
            Err(primary) => {
                let cleanup = self
                    .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                    .await;
                return finish_shadow_operation(Err(primary), cleanup);
            }
        };
        let Some(global) = guest.local_global_state() else {
            let cleanup = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(
                Err(engine_error("V4 receive lost actual local global state")),
                cleanup,
            );
        };
        // The existing reader transfers through the actual local issuer.
        // On failure, its consuming disposition owns all subsequent cleanup;
        // an error does not authorize blindly finishing the old read again.
        let tid = guest.tid();
        let state = guest.thread_state();
        let destination = call.destination();
        let nonblocking = metadata.nonblocking.expect("checked scalar receive retains original flags");
        let admitted = match mode {
            crate::network_replay::NetworkEngineMode::Record => {
                global
                    .begin_private_receive_call(tid, state, read, destination, call.admission_maximum())
                    .await
            }
            crate::network_replay::NetworkEngineMode::Replay => {
                global.begin_replay_receive_call(tid, state, read, destination, call.admission_maximum())
            }
        };
        let admitted = match admitted {
            Ok(admitted) => admitted,
            Err(failure) => {
                let failure = global.cleanup_receive_admission_failure(failure).await;
                let primary = engine_rpc_error(failure.primary().clone());
                let cleanup = match failure.cleanup_diagnostic() {
                    Some(error) => Err(engine_rpc_error(error.clone())),
                    None => Ok(()),
                };
                return finish_shadow_operation(Err(primary), cleanup);
            }
        };
        if let Err(primary) = global.bind_saved_receive_policy(
            &checked_range, tid, state, call, admitted,
        ) {
            return self.finish_host_stream_call(
                guest,
                NetworkHostSocketPin {
                    call: admitted.id,
                    native: admitted.physical_pin_required,
                    nonblocking,
                },
                Err(engine_rpc_error(primary)),
            ).await;
        }
        let invocation = match mode {
            crate::network_replay::NetworkEngineMode::Record => match global
                .bind_private_receive_invocation(checked_range, tid, state, call, admitted)
            {
                Ok(invocation) => Some(invocation),
                Err(failure) => {
                    let failure = global.cleanup_receive_admission_failure(failure).await;
                    let primary = engine_rpc_error(failure.primary().clone());
                    let cleanup = match failure.cleanup_diagnostic() {
                        Some(error) => Err(engine_rpc_error(error.clone())),
                        None => Ok(()),
                    };
                    return finish_shadow_operation(Err(primary), cleanup);
                }
            },
            crate::network_replay::NetworkEngineMode::Replay => None,
        };
        self
            .foreground_v4_receive_from_call(
                guest,
                call,
                admitted,
                mode,
                nonblocking,
                invocation,
            )
            .await
    }

    async fn capture_scalar_network_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        call: syscalls::Read,
        result: &Result<i64, Error>,
    ) -> Result<(), Error> {
        let input = match result {
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
        self.capture_input(guest, open_file, input).await
    }

    async fn replay_scalar_network_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        call: syscalls::Read,
        nonblocking: bool,
    ) -> Result<i64, Error> {
        let outcome = self
            .replay_stream_receive(
                guest,
                open_file,
                call.len(),
                nonblocking,
                0,
                call.signal_interrupt_errno(),
            )
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

    async fn network_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        policy: NetworkPolicy,
        zero_read: bool,
    ) -> Result<i64, Error> {
        if zero_read && call.len() == 0 {
            // The capability-None dispatcher retains the ordinary zero-Read
            // operation in both policies. It must still execute local Linux
            // validation and must not publish EOF or a local errno as ingress.
            // recv(len=0) does not have this Read-only behavior.
            return self.execute_native_zero_read(guest, call).await;
        }
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
                        zero_read,
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
                self.capture_scalar_network_read(guest, open_file, call, &result)
                    .await?;
                result
            }
            NetworkPolicy::Replay => {
                self.replay_scalar_network_read(guest, open_file, call, nonblocking)
                    .await
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
                    .replay_stream_receive(
                        guest,
                        open_file,
                        maximum,
                        nonblocking,
                        0,
                        call.signal_interrupt_errno(),
                    )
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
        let timeout = poll_timeout_duration(call.timeout());
        self.network_poll_common(
            guest,
            call.into(),
            NetworkPollState {
                address: call.fds().map(|address| address.as_raw()),
                count: call.nfds(),
                timeout,
                remaining_address: None,
            },
            call.signal_interrupt_errno(),
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
            call.signal_interrupt_errno(),
            policy,
        )
        .await
    }

    async fn network_poll_common<G: Guest<Self>>(
        &self,
        guest: &mut G,
        live_call: Syscall,
        state: NetworkPollState,
        interrupt_errno: Errno,
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
                    resources.set_signal_interrupt_errno(interrupt_errno);
                    if matches!(
                        resource_request(guest, resources).await,
                        ResumeStatus::Signaled(_)
                    ) {
                        return Err(interrupt_errno.into());
                    }
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
        self.network_select_common(guest, call.into(), state, Errno::EINTR, policy)
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
        self.network_select_common(guest, call.into(), state, Errno::EINTR, policy)
            .await
    }

    async fn network_select_common<G: Guest<Self>>(
        &self,
        guest: &mut G,
        live_call: Syscall,
        state: NetworkSelectState,
        interrupt_errno: Errno,
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
                    resources.set_signal_interrupt_errno(interrupt_errno);
                    if matches!(
                        resource_request(guest, resources).await,
                        ResumeStatus::Signaled(_)
                    ) {
                        return Err(interrupt_errno.into());
                    }
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
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464): retain genuine scalar Recvfrom authority.
        if guest.local_global_state().and_then(|global| global.native_receive_mode()).is_some() {
            return self.handle_owned_recvfrom(guest, call).await;
        }
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
        let nonblocking = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.is_nonblocking())?;
        match policy {
            NetworkPolicy::Record => {
                // Execute the actual recvfrom so message flags retain their
                // Linux meaning.  The Read-shaped value below is only a local
                // description of the scalar output buffer for trace capture.
                let result = self.live_network_syscall(guest, call.into()).await;
                let read = syscalls::Read::new()
                    .with_fd(call.fd())
                    .with_buf(call.buf())
                    .with_len(call.len());
                self.capture_scalar_network_read(guest, open_file, read, &result)
                    .await?;
                result
            }
            NetworkPolicy::Replay => {
                let outcome = self
                    .replay_stream_receive(
                        guest,
                        open_file,
                        call.len(),
                        nonblocking,
                        call.flags(),
                        call.signal_interrupt_errno(),
                    )
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

    async fn network_original_sendto<G: Guest<Self>>(
        &self, guest: &mut G, call: syscalls::Sendto,
    ) -> Result<i64, Error> {
        use crate::network_replay::original_connect::Kind;
        let source = self.record_or_replay.original_file_execution(call.into());
        if source != crate::OriginalFileExecution::Native || !original_sendto_shape(call) {
            return Err(engine_error("original Sendto requires the supported actual native shape"));
        }
        let (_, raw) = Syscall::from(call).into_parts();
        let admission = self.prepare_original_call_from(guest, call.into(), Kind::Sendto,
            (call.fd(), raw.arg1 as u64, call.flags() as i32), source).await?;
        self.mark_original_syscall_invoked(guest);
        // The actual OFD was checked O_NONBLOCK before provider preparation.
        // Preserve the current Normal turn, syscall accounting and common tail.
        let result = self.record_or_replay_preserving_tool_errors(guest, call).await;
        if matches!(&result, Err(Error::Tool(_) | Error::Io(_))) { return result; }
        let (_, _, result, _) = self.observe_original_call_result(guest, admission.clone(), result).await?;
        guest.local_global_state().ok_or_else(|| engine_error("Sendto lost actual local global state"))?
            .publish_foreground_native_sent(guest.tid(), guest.thread_state(), &admission)
            .map_err(engine_rpc_error)?;
        self.shadow_ack(guest, NetworkRequest::NativeRetireOriginalConnect { admission }).await?;
        guest.thread_state_mut().original_connect = None;
        result
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
        if policy == NetworkPolicy::Replay
            && self.network_fd_tracking_active(guest)
            && guest
                .local_global_state()
                .and_then(|global| global.native_receive_mode())
                != Some(crate::network_replay::NetworkEngineMode::Replay)
        {
            return Err(engine_error(
                "tracked Sendto Replay requires actual local V4 authority",
            ));
        }
        if policy == NetworkPolicy::Replay
            && guest
                .local_global_state()
                .and_then(|global| global.native_receive_mode())
                == Some(crate::network_replay::NetworkEngineMode::Replay)
        {
            if !original_sendto_shape(call)
                || !guest
                    .thread_state()
                    .with_detfd(call.fd(), |fd| fd.is_nonblocking())?
            {
                return Err(engine_error(
                    "V4 Sendto Replay requires nonblocking scalar MSG_NOSIGNAL",
                ));
            }
            let read = self.begin_network_fd_read(guest, call.fd()).await?;
            let result = async {
                let maximum = guest.local_global_state().unwrap().replay_sendto_read_limit(
                    guest.tid(), guest.thread_state(), &read, call.size(),
                ).map_err(engine_rpc_error)?;
                if maximum != 0 {
                    // Only the selected recorded prefix is read, never the
                    // requested tail. Record's original native Sendto path is
                    // separate and supplies its bytes from the real producer.
                    let prepared = guest.local_global_state().unwrap()
                        .prepare_replay_transmit_source(
                            guest.tid(), guest.thread_state(), &read,
                            (call.buf().ok_or(Errno::EFAULT)?.as_raw(), maximum, call.flags()),
                        ).await.map_err(engine_rpc_error)?;
                    let bytes = guest.read_native_source(
                        prepared.address(), prepared.length(), prepared.retention(),
                    ).await.map_err(|error| {
                        // Preserve the existing explicit unsupported-backend
                        // refusal; no unsafe memory() fallback or guest errno.
                        tracing::error!(%error, "Replay source worker refused");
                        engine_error("V4 Sendto Replay positive prefix requires source-stop/MM-bound read custody")
                    })?;
                    let outcome = guest.local_global_state().ok_or_else(||
                        engine_error("Replay source lost actual local state")
                    )?.finish_replay_transmit_source(
                        guest.tid(), guest.thread_state(), prepared, bytes,
                    ).map_err(engine_rpc_error)?;
                    return match outcome {
                        NetworkStreamTransmit::Accepted(count) => Ok(count as i64),
                        NetworkStreamTransmit::Error(errno) => errno_result(errno),
                    };
                }
                // A recorded errno has no source bytes. Preserve that outcome
                // without touching even an inaccessible original payload.
                self.stream_transmit(guest, call.into(), open_file, Vec::new(), policy).await
            }.await;
            let released = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(result, released);
        }
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
        // The shared engine must refuse unsupported V4 Record before either
        // native branch, including an unenrolled socket on an RPC-only Guest.
        self.shadow_ack(guest, NetworkRequest::PreflightSocketShutdown)
            .await?;
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
        flags: i32,
        interrupt_errno: Errno,
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
                    flags,
                    receive_low_water: 1,
                },
            )
            .await
            .map_err(engine_rpc_error)?
            {
                NetworkReply::StreamReceive(NetworkStreamReceive::Pending) => {
                    self.wait_for_network(
                        guest,
                        open_file,
                        NetworkWaitKind::Readable,
                        interrupt_errno,
                    )
                    .await?;
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
        interrupt_errno: Errno,
    ) -> Result<(), Error> {
        let mut resources = Resources::new(guest.thread_state().dettid);
        resources.insert(ResourceID::NetworkWait { open_file, kind }, Permission::R);
        resources.set_signal_interrupt_errno(interrupt_errno);
        if matches!(
            resource_request(guest, resources).await,
            ResumeStatus::Signaled(_)
        ) {
            Err(interrupt_errno.into())
        } else {
            Ok(())
        }
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
        self.live_network_syscall_after_grant(guest, call, completion)
            .await
    }

    /// The caller has paid its original external grant. Physical observation
    /// and the one existing continuation remain independent of table release.
    async fn live_network_syscall_after_grant<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        completion: NetworkPhysicalCompletion,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        if matches!(completion, NetworkPhysicalCompletion::OriginalConnect) {
            self.mark_original_syscall_invoked(guest);
        }
        // No await separates this marker transition from the first injection poll.
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
    OriginalConnect,
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
        let captured = match network_request(
            guest,
            NetworkRequest::CaptureAcceptedReturn {
                lease,
                kernel_result,
            },
        )
        .await
        .map_err(engine_rpc_error)
        {
            Ok(NetworkReply::Unit) => Ok(()),
            Err(error) => Err(error),
            reply => Err(engine_error(format!(
                "unexpected accepted capture reply {reply:?}"
            ))),
        };
        let collected =
            match network_request(guest, NetworkRequest::CollectAcceptedEffect { lease })
                .await
                .map_err(engine_rpc_error)
            {
                Ok(NetworkReply::Unit) => Ok(()),
                Ok(reply) => Err(engine_error(format!(
                    "unexpected accepted effect collection {reply:?}"
                ))),
                Err(error) => Err(error),
            };
        finish_shadow_operation(captured, collected)
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

/// Linux poll(2) treats every negative timeout as an infinite wait.
/// This is distinct from ppoll's timespec, where negative fields are invalid.
fn poll_timeout_duration(timeout: i32) -> Option<Duration> {
    (timeout >= 0).then(|| Duration::from_millis(timeout as u64))
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
    let family = i32::from(family);
    let copy_length = match family {
        libc::AF_INET => {
            let required = std::mem::size_of::<libc::sockaddr_in>();
            if length < required as i32 {
                return Err(Errno::EINVAL.into());
            }
            required
        }
        libc::AF_INET6 => {
            // Linux accepts the original 24-byte IPv6 sockaddr without scope.
            // Preserve supplied 24..27 bytes; use scope only when all 28 exist.
            let required = std::mem::offset_of!(libc::sockaddr_in6, sin6_scope_id);
            if length < required as i32 {
                return Err(Errno::EINVAL.into());
            }
            (length as usize).min(std::mem::size_of::<libc::sockaddr_in6>())
        }
        libc::AF_UNIX => {
            let length = usize::try_from(length).map_err(|_| Errno::EINVAL)?;
            if length > std::mem::size_of::<libc::sockaddr_un>() {
                return Err(Errno::EINVAL.into());
            }
            if length <= std::mem::offset_of!(libc::sockaddr_un, sun_path) {
                return Ok(NetworkAddressV2::UnixUnnamed);
            }
            length
        }
        family => {
            return Err(engine_error(format!(
                "unsupported connect address family {family}"
            )));
        }
    };
    // This legacy guest-input wrapper retains its original first-family read
    // and bounded copy order. Original Record Connect never enters this path:
    // its input to the same pure decoder is the retained kernel copy.
    let mut bytes = vec![0; copy_length];
    guest.memory().read_exact(address.cast(), &mut bytes)?;
    decode_network_address(family, &bytes)
}

fn captured_sockaddr_too_short(bytes: &[u8]) -> bool {
    let Some(family) = bytes.get(..2) else {
        return true;
    };
    let required = match i32::from(u16::from_ne_bytes([family[0], family[1]])) {
        libc::AF_INET => std::mem::size_of::<libc::sockaddr_in>(),
        libc::AF_INET6 => std::mem::offset_of!(libc::sockaddr_in6, sin6_scope_id),
        libc::AF_UNIX => std::mem::offset_of!(libc::sockaddr_un, sun_path),
        _ => return false,
    };
    bytes.len() < required
}

fn read_captured_network_address(bytes: &[u8]) -> Result<NetworkAddressV2, Error> {
    let family = bytes
        .get(..2)
        .ok_or_else(|| engine_error("captured sockaddr lacks family"))?;
    decode_network_address(i32::from(u16::from_ne_bytes([family[0], family[1]])), bytes)
}

/// Normalize bytes only; neither adapter is permitted to obtain the other's
/// input by rereading guest memory or looking up the numeric FD after return.
fn decode_network_address(family: i32, bytes: &[u8]) -> Result<NetworkAddressV2, Error> {
    let required = |size: usize| {
        if bytes.len() >= size {
            Ok(())
        } else {
            Err(engine_error(
                "captured sockaddr is too short for its family",
            ))
        }
    };
    match family {
        libc::AF_INET => {
            required(std::mem::size_of::<libc::sockaddr_in>())?;
            Ok(NetworkAddressV2::Inet4 {
                address: bytes[4..8].try_into().unwrap(),
                port: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
            })
        }
        libc::AF_INET6 => {
            required(std::mem::offset_of!(libc::sockaddr_in6, sin6_scope_id))?;
            Ok(NetworkAddressV2::Inet6 {
                address: bytes[8..24].try_into().unwrap(),
                port: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
                flowinfo: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
                scope_id: bytes
                    .get(24..28)
                    .map_or(0, |scope| u32::from_ne_bytes(scope.try_into().unwrap())),
            })
        }
        libc::AF_UNIX => {
            let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
            if bytes.len() > std::mem::size_of::<libc::sockaddr_un>() {
                return Err(engine_error(
                    "captured Unix sockaddr exceeds native storage",
                ));
            }
            let path = bytes
                .get(offset..)
                .ok_or_else(|| engine_error("captured Unix sockaddr lacks native path offset"))?;
            Ok(match path.first() {
                None => NetworkAddressV2::UnixUnnamed,
                Some(0) => NetworkAddressV2::UnixAbstract(path[1..].to_vec()),
                Some(_) => NetworkAddressV2::UnixPath(path.to_vec()),
            })
        }
        other => Err(engine_error(format!(
            "unsupported captured Connect address family {other}"
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
    fn ipv6_original_24_byte_sockaddr_matches_native_and_ignores_incomplete_scope() {
        // Native ::1 stream connect(length=24) accepted and transferred payload
        // in root-connect-address-audit-v1. These literal bytes also exercise
        // the common guest/captured codec without executing any socket here.
        let mut bytes = [0u8; 29];
        bytes[..2].copy_from_slice(&(libc::AF_INET6 as u16).to_ne_bytes());
        bytes[2..4].copy_from_slice(&0x1234u16.to_be_bytes());
        bytes[4..8].copy_from_slice(&0x10203040u32.to_ne_bytes());
        bytes[23] = 1;
        bytes[24..28].copy_from_slice(&0x55667788u32.to_ne_bytes());
        for length in 24..=29 {
            let mut address = [0; 16];
            address[15] = 1;
            let expected = NetworkAddressV2::Inet6 {
                address,
                port: 0x1234,
                flowinfo: 0x10203040,
                scope_id: if length < 28 { 0 } else { 0x55667788 },
            };
            assert_eq!(
                read_captured_network_address(&bytes[..length]).unwrap(),
                expected
            );
            assert_eq!(
                decode_network_address(libc::AF_INET6, &bytes[..length]).unwrap(),
                expected
            );
            assert!(!captured_sockaddr_too_short(&bytes[..length]));
        }
        for length in 0..24 {
            assert!(read_captured_network_address(&bytes[..length]).is_err());
            assert!(decode_network_address(libc::AF_INET6, &bytes[..length]).is_err());
            assert!(captured_sockaddr_too_short(&bytes[..length]));
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
struct NetworkHostSocketPin {
    call: NetworkStreamCallId,
    native: bool,
    nonblocking: bool,
}

impl<T: RecordOrReplay> Detcore<T> {
    fn check_host_stream_capture<G: Guest<Self>>(
        &self,
        guest: &G,
        policy: NetworkPolicy,
    ) -> Result<(), Error> {
        if policy == NetworkPolicy::Record && !guest.config().backend_supports_host_socket_pin {
            return Err(engine_error(
                "selected backend has no authenticated host socket-pin implementation",
            ));
        }
        if !self.network_fd_tracking_active(guest) {
            return Err(engine_error(
                "native stream capture lacks complete backend FD-table admission",
            ));
        }
        // Backend pinning and helper-private Peek bytes do not authenticate
        // current kernel rollback layout or an exact foreground copy. V3's
        // declared publication units cannot supply either missing authority.
        if policy == NetworkPolicy::Record {
            return Err(engine_error(
                "Record host stream capture requires authenticated current-layout and copy authority",
            ));
        }
        Ok(())
    }

    async fn begin_host_stream_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        expected: OpenFileId,
        policy: NetworkPolicy,
    ) -> Result<NetworkHostSocketPin, Error> {
        self.check_host_stream_capture(guest, policy)?;
        let read = self.begin_network_fd_read(guest, fd).await?;
        let observed = {
            let table = guest.thread_state().file_metadata.clone();

            table.lock().unwrap().observe_fd_read(&read)
        };
        let observed = match observed {
            Ok(observed) if observed.socket == Some(expected) => observed,
            other => {
                let error = match other {
                    Err(error) => error,
                    _ => engine_error("FD slot changed before stream-call admission"),
                };
                let released = self
                    .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                    .await;
                return finish_shadow_operation(Err(error), released);
            }
        };
        self.capture_admitted_host_stream_call(guest, read, observed)
            .await
    }

    /// Recovery uses the existing publication owner. No metadata mutex, table
    /// permit or OFD control survives a wait on another admitted reader/mutator.
    pub(crate) async fn begin_network_fd_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<crate::network_replay::NetworkFdReadAdmission, Error> {
        loop {
            self.publish_network_fd_installations(guest).await?;
            let files = guest.thread_state().file_metadata.lock().unwrap().files_id;
            match network_request(guest, NetworkRequest::BeginFdRead { files, fd })
                .await
                .map_err(engine_rpc_error)?
            {
                NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) => {
                    let read = *read;
                    return Ok(read);
                }
                NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Recover) => {}
                reply => {
                    return Err(engine_error(format!(
                        "unexpected reader admission {reply:?}"
                    )));
                }
            }
        }
    }

    /// Consuming transfer point for the owned dispatcher/scan input. The Global
    /// side validates this exact read and moves its permit/control into the
    /// existing Call before starting capture; it never reacquires the table.
    async fn capture_admitted_host_stream_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        read: crate::network_replay::NetworkFdReadAdmission,
        observed: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<NetworkHostSocketPin, Error> {
        // Owned scalar dispatch reaches this consuming entry directly. Refuse
        // before capture, while releasing the exact admission it already owns.
        if let Err(error) =
            self.check_host_stream_capture(guest, guest.config().network_trace.policy)
        {
            let cleanup = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(Err(error), cleanup);
        }
        let expected = read
            .binding
            .ok_or_else(|| engine_error("capture read has no binding"))?
            .open_file;
        if observed.binding != read.binding || observed.socket != Some(expected) {
            return Err(engine_error(
                "capture observation differs from its admitted binding",
            ));
        }
        // The old begin_shadow_fd_control required an enrolled V3 state.
        // Read that state under this exact descriptor control before capture.
        let state = self.shadow_socket_state(guest, expected).await;
        if !matches!(state, Ok(Some(_))) {
            let error = match state {
                Err(error) => error,
                _ => engine_error("stream enrollment disappeared before pin admission"),
            };
            let cleanup = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(Err(error), cleanup);
        }
        let nonblocking = observed
            .nonblocking
            .ok_or_else(|| engine_error("capture read has no flag observation"))?;
        let control = read
            .control
            .ok_or_else(|| engine_error("capture read has no descriptor control"))?;
        let call = match network_request(guest, NetworkRequest::NativeBeginStreamCall { read })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::StreamCall(call) if call.open_file == expected => call,
            NetworkReply::NativeStreamPinFailed(errno) => {
                return Err(engine_error(format!(
                    "cannot acquire physical stream reference: errno {errno}"
                )));
            }
            other => {
                return Err(engine_error(format!(
                    "unexpected stream call admission {other:?}"
                )));
            }
        };
        self.shadow_ack(
            guest,
            NetworkRequest::FinishSocketControl {
                lease: control,
                disposition: NetworkSocketControlFinish::Unchanged,
            },
        )
        .await?;
        Ok(NetworkHostSocketPin {
            call: call.id,
            native: call.physical_pin_required,
            nonblocking,
        })
    }

    async fn finish_host_stream_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: NetworkHostSocketPin,
        result: Result<i64, Error>,
    ) -> Result<i64, Error> {
        let cleanup = async {
            if pin.native {
                self.shadow_ack(
                    guest,
                    NetworkRequest::NativeReleaseStreamCall { call: pin.call },
                )
                .await
            } else {
                self.shadow_ack(
                    guest,
                    NetworkRequest::BeginStreamCallRelease { id: pin.call },
                )
                .await?;
                self.shadow_ack(
                    guest,
                    NetworkRequest::FinishStreamCallRelease { id: pin.call },
                )
                .await
            }
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
    pub(super) async fn accepted_model_mode<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<bool, Error> {
        if !matches!(
            guest.config().network_trace.policy,
            NetworkPolicy::Record | NetworkPolicy::Replay
        ) {
            return Ok(false);
        }
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
    /// Record Connect admission follows the actual engine capability. V3 binds
    /// it to the accepted runtime; V4 binds it to admitted descriptor tracking
    /// in a V4 Record engine, whose global entry is the native Connect join.
    pub(crate) async fn original_connect_record_route<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<bool, Error> {
        if guest.config().network_trace.policy != NetworkPolicy::Record {
            return Ok(false);
        }
        let native = guest
            .local_global_state()
            .and_then(|global| global.native_receive_mode());
        if native.is_some() {
            return Ok(
                native == Some(crate::network_replay::NetworkEngineMode::Record)
                    && self.network_fd_tracking_active(guest),
            );
        }
        self.accepted_model_mode(guest).await
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
        if policy == NetworkPolicy::Replay {
            checked_accept4_socket_type(call.flags().bits())?;
            return Err(engine_error(
                "Replay accept requires original reservation/copyout authority",
            ));
        }
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
                        self.wait_for_network(
                            guest,
                            listener,
                            NetworkWaitKind::Readable,
                            call.signal_interrupt_errno(),
                        )
                        .await?;
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
            // Replay was refused above, before queue reservation or allocation.
            self.shadow_ack(
                guest,
                NetworkRequest::PrepareAcceptedEffect {
                    lease: reservation.lease,
                    fd: call.sockfd(),
                    flags: call.flags().bits(),
                },
            )
            .await?;
            // Original guest address/length preserve Linux allocation and
            // copyout priority. No preliminary scratch accept or validation.
            let result = self
                .live_network_syscall_with_completion(
                    guest,
                    call.into(),
                    NetworkPhysicalCompletion::Accept(reservation.lease),
                )
                .await;
            let kernel_result = match &result {
                Ok(fd) => Ok(i32::try_from(*fd).map_err(|_| Errno::EIO)?),
                Err(Error::Errno(errno)) => Err(errno.into_raw()),
                Err(_) => return result,
            };
            let installed = if policy == NetworkPolicy::Record {
                match network_request(
                    guest,
                    NetworkRequest::ResolveAcceptedProvider {
                        lease: reservation.lease,
                    },
                )
                .await
                .map_err(engine_rpc_error)?
                {
                    NetworkReply::AcceptedInstallation(binding)
                        if Some(binding.slot.fd) == kernel_result.ok() =>
                    {
                        Some(binding)
                    }
                    NetworkReply::AcceptedNoInstallation if kernel_result.is_err() => None,
                    reply => {
                        return Err(engine_error(format!(
                            "accepted original installation changed: {reply:?}"
                        )));
                    }
                }
            } else {
                None
            };
            // The service has to publish its matched descriptor fact BEFORE
            // this RPC. A return value/add_fd never fabricates that authority.
            let completion = network_request(
                guest,
                NetworkRequest::CompleteAcceptedSocket {
                    lease: reservation.lease,
                    kernel_result,
                    installed_open_file: installed.map(|binding| binding.open_file),
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
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464):
        // V4 outbound replies are same-pin Connect observations, released
        // only after establishment. Errors must not fall through to a real
        // getpeername/getsockname on an unconnected Replay placeholder.
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
    async fn native_stream_effect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        lease: NetworkStreamLeaseId,
        effect: NetworkStreamPhysicalEffect,
    ) -> Result<crate::network_runtime::native_peer::Observation, Error> {
        match network_request(guest, NetworkRequest::NativeStreamEffect { lease, effect })
            .await
            .map_err(engine_rpc_error)?
        {
            NetworkReply::NativeStreamObservation(observed) => Ok(observed),
            reply => Err(engine_error(format!(
                "unexpected native stream effect reply {reply:?}"
            ))),
        }
    }
    async fn set_host_shadow_cursor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
        lease: NetworkStreamLeaseId,
        value: i32,
    ) -> Result<(), Error> {
        let _ = pin; // The lease was joined to this call by its engine-produced reply.
        let observed = self
            .native_stream_effect(
                guest,
                lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value },
            )
            .await?;
        match observed.confirmation {
            NetworkStreamPhysicalResult::Unit => Ok(()),
            other => Err(engine_error(format!(
                "physical cursor transition failed: {other:?}"
            ))),
        }
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
        let observed = self
            .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::ReadPeekOffset)
            .await?;
        let saved = match observed.confirmation {
            NetworkStreamPhysicalResult::PeekOffset(value) => Some(value),
            NetworkStreamPhysicalResult::Errno(errno)
                if errno == libc::ENOPROTOOPT || errno == libc::EOPNOTSUPP =>
            {
                None
            }
            other => {
                return Err(engine_error(format!(
                    "owned socket cursor query failed: {other:?}"
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
        let observed = self
            .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::Peek { maximum })
            .await?;
        let peek = match observed.confirmation {
            NetworkStreamPhysicalResult::Peeked { count } => Ok((count, observed.bytes)),
            NetworkStreamPhysicalResult::Errno(errno) => Err(Errno::new(errno)),
            other => {
                return Err(engine_error(format!(
                    "unexpected native PEEK result {other:?}"
                )));
            }
        };
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
        let observed = self
            .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::PollState)
            .await?;
        let NetworkStreamPhysicalResult::PollState { revents } = observed.confirmation else {
            return Err(engine_error(
                "owned poll0 did not return a poll observation",
            ));
        };
        let observed = self
            .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::QueuedBytes)
            .await?;
        let NetworkStreamPhysicalResult::QueuedBytes { count: queued } = observed.confirmation
        else {
            return Err(engine_error(
                "owned FIONREAD did not return a queue observation",
            ));
        };
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
        let _ = pin; // This lease already identifies the captured original OFD.
        let mut drained = 0;
        while drained < selection_len {
            let maximum = (selection_len - drained).min(SHADOW_VIEW);
            let observed = self
                .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::Drain { maximum })
                .await?;
            let NetworkStreamPhysicalResult::Drained { bytes } = observed.confirmation else {
                return Err(engine_error("physical stream drain did not complete"));
            };
            // The shared engine has already checked every actual byte against
            // the immutable selection. Zero/short-error remains unresolved.
            drained += bytes.len();
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

// Keep the live-task namespace check at the adapter boundary. Terminal recovery
// calls the same physical observer with the original Call's held namespace.
fn record_receive_normalization(
    physical_tid: i32,
    key: StreamSocketKeyV3,
) -> Result<(LinuxReceiveNormalizationV3, bool), Error> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    let guest_path = format!("/proc/{physical_tid}/ns/net");
    let guest_namespace = std::fs::File::open(&guest_path).map_err(Error::Io)?;
    let before = guest_namespace.metadata().map_err(Error::Io)?;
    let profile =
        crate::network_runtime::socket_profile::record_receive_normalization_in_namespace(
            guest_namespace.as_fd(),
            key,
        )?;
    let after = std::fs::File::open(&guest_path)
        .map_err(Error::Io)?
        .metadata()
        .map_err(Error::Io)?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(engine_error(
            "receive profile namespace or sysctl changed during bootstrap",
        ));
    }
    Ok(profile)
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
        if let Some((open_file, enrollment)) = self
            .capture_fresh_stream_socket(guest, fd, call, true)
            .await?
        {
            self.register_fresh_stream_socket(
                guest,
                open_file.expect("existing path captured its binding"),
                enrollment,
            )
            .await?;
        }
        Ok(())
    }

    async fn capture_original_socket_profile<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
        observed: &crate::network_runtime::installation_observation::Checked,
    ) -> Result<Option<FreshStreamEnrollment>, Error> {
        if !self.shadow_mode(guest).await?
            || !matches!(call.family(), libc::AF_INET | libc::AF_INET6)
            || call.r#type() & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) != libc::SOCK_STREAM
            || !matches!(call.protocol(), 0 | libc::IPPROTO_TCP)
        {
            return Ok(None);
        }
        if !guest.config().backend_supports_host_socket_pin {
            return Err(engine_error(
                "original Socket profile lacks authenticated namespace support",
            ));
        }
        let key = StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: call.family(),
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        };
        let namespace = authenticated_stream_namespace(guest.tid().as_raw())?;
        if namespace.inode != observed.namespace {
            return Err(engine_error(
                "held original socket differs from admitted network namespace",
            ));
        }
        let observed_profile = if guest.config().network_trace.policy == NetworkPolicy::Record {
            // Existing Record-only normalization uses host-owned scratch and
            // checks namespace/sysctl identity. It never touches guest buffers.
            let (normalization, _) = record_receive_normalization(guest.tid().as_raw(), key)?;
            Some(
                observed
                    .fresh_profile(key, normalization)
                    .map_err(Error::Io)?,
            )
        } else {
            None
        };
        if namespace != authenticated_stream_namespace(guest.tid().as_raw())? {
            return Err(engine_error(
                "original Socket namespace changed during normalization",
            ));
        }
        if self.accepted_model_mode(guest).await? {
            self.shadow_ack(
                guest,
                NetworkRequest::RegisterAcceptedFreshSend {
                    key,
                    observed: observed_profile
                        .as_ref()
                        .map(|_| ReceiveTimeoutV3::Infinite),
                },
            )
            .await?;
        }
        Ok(Some(FreshStreamEnrollment {
            key,
            namespace,
            observed_profile,
        }))
    }

    async fn capture_fresh_stream_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        call: syscalls::Socket,
        existing_local: bool,
    ) -> Result<Option<(Option<OpenFileId>, FreshStreamEnrollment)>, Error> {
        if !self.shadow_mode(guest).await? {
            return Ok(None);
        }
        if !matches!(call.family(), libc::AF_INET | libc::AF_INET6)
            || call.r#type() & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) != libc::SOCK_STREAM
            || !matches!(call.protocol(), 0 | libc::IPPROTO_TCP)
        {
            return Ok(None);
        }
        let key = StreamSocketKeyV3 {
            transport: NetworkTransportV2::Tcp,
            domain: call.family(),
            socket_type: libc::SOCK_STREAM,
            protocol: libc::IPPROTO_TCP,
        };
        // Preserve the ordinary path's original lookup/copy order. Original
        // Socket uses capture_original_socket_profile with its held observation.
        let open_file = if existing_local {
            Some(guest.thread_state().socket_open_file_id(fd)?)
        } else {
            None
        };
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
        Ok(Some((
            open_file,
            FreshStreamEnrollment {
                key,
                namespace,
                observed_profile,
            },
        )))
    }

    async fn register_fresh_stream_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file: OpenFileId,
        enrollment: FreshStreamEnrollment,
    ) -> Result<(), Error> {
        let FreshStreamEnrollment {
            key,
            namespace,
            observed_profile,
        } = enrollment;
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

use crate::network_replay::original_installation::FreshStreamEnrollment;

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

enum NetworkShadowWaitProgress {
    /// The clock observation expired the deadline before requesting a turn.
    DeadlineWithoutWait,
    /// The actual timer reported EINTR, but its continuation resumed Normal.
    /// This is a fresh foreground grant, unlike scheduler SignalResume.
    InterruptedAfterNormal,
    Wait(NetworkShadowWaitOutcome),
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
        Ok(
            match self
                .wait_shadow_network_progress(
                    guest,
                    interests,
                    deadline,
                    interrupt_errno,
                    policy,
                    zero_receive_call,
                )
                .await?
            {
                NetworkShadowWaitProgress::DeadlineWithoutWait => NetworkShadowWaitOutcome::Ready {
                    entered_zero_wait: false,
                },
                NetworkShadowWaitProgress::InterruptedAfterNormal => {
                    NetworkShadowWaitOutcome::Signaled {
                        entered_zero_wait: false,
                    }
                }
                NetworkShadowWaitProgress::Wait(outcome) => outcome,
            },
        )
    }

    async fn wait_shadow_network_progress<G: Guest<Self>>(
        &self,
        guest: &mut G,
        interests: Vec<(NetworkStreamCallId, NetworkWaitKind)>,
        deadline: Option<LogicalTime>,
        interrupt_errno: Errno,
        policy: NetworkPolicy,
        zero_receive_call: Option<NetworkStreamCallId>,
    ) -> Result<NetworkShadowWaitProgress, Error> {
        let now = thread_observe_time(guest).await;
        if deadline.is_some_and(|end| now >= end) {
            return Ok(NetworkShadowWaitProgress::DeadlineWithoutWait);
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
            return Ok(NetworkShadowWaitProgress::Wait(if matches!(resumed, ResumeStatus::Signaled(_)) {
                NetworkShadowWaitOutcome::Signaled { entered_zero_wait }
            } else {
                NetworkShadowWaitOutcome::Ready { entered_zero_wait }
            }));
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
            return Ok(NetworkShadowWaitProgress::Wait(NetworkShadowWaitOutcome::Signaled { entered_zero_wait }));
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
        if matches!(resumed, ResumeStatus::Signaled(_)) {
            return Ok(NetworkShadowWaitProgress::Wait(
                NetworkShadowWaitOutcome::Signaled { entered_zero_wait },
            ));
        }
        if result == Err(Errno::EINTR) {
            if zero_receive_call.is_none() {
                return Ok(NetworkShadowWaitProgress::InterruptedAfterNormal);
            }
            return Ok(NetworkShadowWaitProgress::Wait(
                NetworkShadowWaitOutcome::Signaled { entered_zero_wait },
            ));
        }
        match result {
            Ok(0) => Ok(NetworkShadowWaitProgress::Wait(NetworkShadowWaitOutcome::Ready { entered_zero_wait })),
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
        self.shadow_stream_receive_with_pin(guest, pin, request)
            .await
    }

    async fn shadow_stream_receive_with_pin<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: NetworkHostSocketPin,
        request: NetworkReceiveContext<'_>,
    ) -> Result<i64, Error> {
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
    /// Probe only the admitted local endpoint, never a Replay TCP placeholder.
    /// The short reader and original sole-root grant protect numeric selection;
    /// neither a copied local marker nor a native readiness bit grants a turn.
    async fn probe_shadow_local_pair<G: Guest<Self>>(
        &self, guest: &mut G, row: libc::pollfd,
    ) -> Result<libc::pollfd, Error> {
        let read = self.begin_network_fd_read(guest, row.fd).await?;
        let operation = async {
            guest.local_global_state().ok_or_else(|| engine_error("local pair poll lacks local global state"))?
                .validate_local_pair_poll(guest.thread_state(), &read)?;
            let mut stack = guest.stack().await;
            let address = stack.reserve::<libc::pollfd>();
            let timeout = stack.reserve::<syscalls::Timespec>();
            let _guard = stack.commit()?;
            guest.memory().write_value(address, &libc::pollfd { revents: 0, ..row })?;
            guest.memory().write_value(timeout, &syscalls::Timespec { tv_sec: 0, tv_nsec: 0 })?;
            let count = guest.inject(syscalls::Ppoll::new().with_fds(Some(address.cast()))
                .with_nfds(1).with_timeout(Some(timeout)).with_sigmask(None).with_sigsetsize(0)).await?;
            let observed = guest.memory().read_value(address)?;
            guest.local_global_state().ok_or_else(|| engine_error("local pair poll lost local global state"))?
                .validate_local_pair_poll(guest.thread_state(), &read)?;
            checked_local_poll_result(row, observed, count)
        }.await;
        let cleanup = self.shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read }).await;
        finish_shadow_operation(operation, cleanup)
    }

    /// One scan lookup. Copying the poll/select input happens before entry;
    /// neither this short admission nor capture spans guest-memory access.
    async fn admit_shadow_poll_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        policy: NetworkPolicy,
    ) -> Result<Option<NetworkHostSocketPin>, Error> {
        self.check_host_stream_capture(guest, policy)?;
        let read = self.begin_network_fd_read(guest, fd).await?;
        let observed = {
            let table = guest.thread_state().file_metadata.clone();

            table.lock().unwrap().observe_fd_read(&read)
        };
        match observed {
            Ok(observed) if observed.socket.is_some() => self
                .capture_admitted_host_stream_call(guest, read, observed)
                .await
                .map(Some),
            observed => {
                let result = match observed {
                    Ok(observed) if observed.binding.is_none() => Ok(None),
                    Ok(_) => Err(engine_error(
                        "mixed network and non-network poll sets are not replayable",
                    )),
                    Err(error) => Err(error),
                };
                let cleanup = self
                    .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                    .await;
                finish_shadow_operation(result, cleanup)
            }
        }
    }

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
                // Observe local rows before acquiring external Call pins. Each
                // reader is released before another table admission. Duplicate
                // rows retain their own requested mask and Linux-ready count.
                let mut local_rows = vec![false; output.len()];
                for (index, row) in output.iter_mut().enumerate() {
                    if row.fd >= 0 && guest.thread_state().with_detfd(row.fd,
                        |fd| fd.is_local_socket_pair()).unwrap_or(false)
                    {
                        *row = self.probe_shadow_local_pair(guest, *row).await?;
                        local_rows[index] = true;
                        ready |= row.revents != 0;
                    }
                }
                for (index, pollfd) in output.iter_mut().enumerate() {
                    if local_rows[index] { continue; }
                    pollfd.revents = 0;
                    if pollfd.fd < 0 {
                        continue;
                    }
                    // Repeated rows retain the existing scan reference, but
                    // never combine it with metadata from a reused numeric FD.
                    let at = if let Some(at) = pins.iter().position(|(fd, _)| *fd == pollfd.fd) {
                        at
                    } else {
                        let Some(pin) = self.admit_shadow_poll_fd(guest, pollfd.fd, policy).await?
                        else {
                            pollfd.revents = libc::POLLNVAL;
                            ready = true;
                            continue;
                        };
                        pins.push((pollfd.fd, pin));
                        pins.len() - 1
                    };
                    let pin = &pins[at].1;
                    let state = self.shadow_call_socket_state(guest, pin.call).await?;
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
        if guest.local_global_state().is_some_and(|global| global.native_poll_enabled()) {
            return self.native_poll_single_scan(guest, state, policy).await;
        }
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

fn checked_local_poll_result(
    requested: libc::pollfd, observed: libc::pollfd, count: i64,
) -> Result<libc::pollfd, Error> {
    if observed.fd != requested.fd || observed.events != requested.events
        || count != i64::from(observed.revents != 0)
    {
        return Err(engine_error("local poll changed its exact native row/count"));
    }
    Ok(observed)
}

pub(super) async fn read_socket_i32<G, T>(guest: &mut G, fd: i32, name: i32) -> Result<i32, Error>
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
    #[tokio::test]
    async fn local_socket_pair_poll_authority_refuses_shared_root_after_selected_grant() {
        // Exercise the exact scheduler/root predicate used by the local poll
        // wrapper. Census and birth wire replies are the existing controlled
        // fixture, not a native provider or a full mixed-poll execution.
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let mut selected = None;
        let fixture = crate::network_runtime::ForegroundRoot::controlled_shared_birth_after_entry(
            raw, |_, root, _| {
                let mut scheduler = crate::scheduler::Scheduler::new(&crate::Config::default());
                scheduler.controlled_foreground_store_grant(root);
                assert!(root.is_sole_initial_root(root.owner()));
                assert!(scheduler.foreground_native_observation(root.owner(), root).is_ok());
                selected = Some(scheduler);
            }).await;
        let scheduler = selected.unwrap();
        let owner = fixture.parent.owner();
        let current = fixture.parent.is_current(owner);
        let sole = fixture.parent.is_sole_initial_root(owner);
        let refusal = scheduler.foreground_native_observation(owner, &fixture.parent)
            .err().map(|error| error.to_string());
        drop(scheduler);
        drop(fixture);
        assert!(current, "birth does not invent an unrelated or stale root");
        assert!(!sole);
        assert_eq!(refusal.as_deref(), Some("native observation lacks unchanged sole initial root"));
    }

    #[test]
    fn local_socket_pair_poll_preserves_real_empty_readable_hup_and_duplicate_masks() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        fn capture(fd: i32) -> (i32, [libc::pollfd; 3], [(libc::pollfd, i32); 3]) {
            let requested = [libc::POLLIN, libc::POLLOUT, 0].map(|events|
                libc::pollfd { fd, events, revents: 0 });
            let timeout = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            let mut together = requested;
            let count = unsafe { libc::ppoll(together.as_mut_ptr(), together.len() as _,
                &timeout, std::ptr::null()) };
            let separate = requested.map(|mut row| {
                let count = unsafe { libc::ppoll(&mut row, 1, &timeout, std::ptr::null()) };
                (row, count)
            });
            (count, together, separate)
        }

        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.set_nonblocking(true).unwrap();
        let empty = capture(reader.as_raw_fd());
        let write = writer.write(b"x");
        let readable = capture(reader.as_raw_fd());
        let mut byte = [0];
        let read = reader.read(&mut byte);
        drop(writer);
        let closed = capture(reader.as_raw_fd());
        drop(reader);
        // Owned originals are closed before evaluating any readiness result.
        assert_eq!(write.unwrap(), 1);
        assert_eq!(read.unwrap(), 1);
        assert_eq!(byte, *b"x");
        for ((count, together, separate), expected) in [
            (empty, [0, libc::POLLOUT, 0]),
            (readable, [libc::POLLIN, libc::POLLOUT, 0]),
            (closed, [libc::POLLIN | libc::POLLHUP, libc::POLLOUT | libc::POLLHUP, libc::POLLHUP]),
        ] {
            assert_eq!(together.map(|row| row.revents), expected);
            assert_eq!(count as usize, expected.into_iter().filter(|bits| *bits != 0).count());
            for (index, (observed, count)) in separate.into_iter().enumerate() {
                let requested = libc::pollfd { revents: 0, ..together[index] };
                let checked = super::checked_local_poll_result(requested, observed, i64::from(count)).unwrap();
                assert_eq!((checked.fd, checked.events, checked.revents),
                    (together[index].fd, together[index].events, together[index].revents));
            }
        }
    }

    #[test]
    fn local_socket_pair_poll_rejects_changed_row_or_count_without_filtering_error_bits() {
        let requested = libc::pollfd { fd: 7, events: 0, revents: 0 };
        for bits in [0, libc::POLLERR, libc::POLLHUP, libc::POLLNVAL,
            libc::POLLERR | libc::POLLHUP] {
            let observed = libc::pollfd { revents: bits, ..requested };
            let count = i64::from(bits != 0);
            assert_eq!(super::checked_local_poll_result(requested, observed, count).unwrap().revents, bits);
            assert!(super::checked_local_poll_result(requested, observed, count + 1).is_err());
            assert!(super::checked_local_poll_result(requested, observed, -1).is_err());
            assert!(super::checked_local_poll_result(requested,
                libc::pollfd { fd: 8, ..observed }, count).is_err());
            assert!(super::checked_local_poll_result(requested,
                libc::pollfd { events: libc::POLLIN, ..observed }, count).is_err());
        }
    }

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
    fn negative_poll_timeouts_match_linux_without_a_deadline() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"x").unwrap();
        for timeout in [-1, -2, i32::MIN] {
            let mut descriptor = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Keep the witness ready, so the native infinite wait is bounded
            // by a queued byte. poll does not consume it between cases.
            assert_eq!(unsafe { libc::poll(&mut descriptor, 1, timeout) }, 1);
            assert_eq!(descriptor.revents, libc::POLLIN);
            assert_eq!(poll_timeout_duration(timeout), None);
        }
        assert_eq!(poll_timeout_duration(0), Some(Duration::ZERO));
        assert_eq!(poll_timeout_duration(1), Some(Duration::from_millis(1)));
        assert_eq!(
            poll_timeout_duration(i32::MAX),
            Some(Duration::from_millis(i32::MAX as u64)),
        );
    }

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

#[cfg(test)]
mod original_file_delegate_error_tests {
    use std::future::Future;
    use std::sync::Mutex;

    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Never;
    use reverie::Tid;
    use reverie::TimerSchedule;
    use reverie::Tool;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Sysno;
    use serde::Deserialize;
    use serde::Serialize;

    use super::*;
    use crate::Config;
    use crate::network_replay::original_connect::Arguments;
    use crate::network_replay::original_connect::FileOperation;
    use crate::network_replay::original_connect::Kind;
    use crate::network_replay::original_connect::Local;
    use crate::tool_global::GlobalRequest;
    use crate::tool_global::GlobalResponse;
    use crate::tool_global::GlobalState;

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct FailingDelegate;
    #[reverie::tool]
    impl Tool for FailingDelegate {
        type GlobalState = GlobalState;
        type ThreadState = usize;
        async fn handle_syscall_event<G: Guest<Self>>(
            &self,
            guest: &mut G,
            call: Syscall,
        ) -> Result<i64, Error> {
            assert!(matches!(call, Syscall::Fcntl(c)
                if matches!(c.cmd(), syscalls::FcntlCmd::F_GETFL)));
            let mode = *guest.thread_state();
            *guest.thread_state_mut() += 1;
            match mode {
                0 => Err(Error::Tool(anyhow::anyhow!("exact delegate tool failure"))),
                2 => Err(Error::Io(std::io::Error::other(
                    "exact delegate IO failure",
                ))),
                _ => panic!("delegate called more than once"),
            }
        }
    }
    impl RecordOrReplay for FailingDelegate {
        fn original_file_execution(&self, call: Syscall) -> crate::OriginalFileExecution {
            if matches!(call, Syscall::Openat(_)) {
                crate::OriginalFileExecution::Recorded
            } else {
                crate::OriginalFileExecution::Native
            }
        }
        async fn invoke_original_read<G: Guest<Self>>(
            &self,
            _guest: &mut G,
            _call: reverie::syscalls::Read,
        ) -> Result<reverie::InjectedReadResult, Error> {
            panic!("the failing F_GETFL fixture must not invoke a native Read")
        }
    }

    struct NoStack;
    struct NoStackGuard;
    impl Drop for NoStackGuard {
        fn drop(&mut self) {}
    }
    impl Stack for NoStack {
        type StackGuard = NoStackGuard;
        fn size(&self) -> usize {
            panic!("unexpected stack")
        }
        fn capacity(&self) -> usize {
            panic!("unexpected stack")
        }
        fn push<'s, V>(&mut self, _: V) -> Addr<'s, V> {
            panic!("unexpected stack")
        }
        fn reserve<'s, V>(&mut self) -> AddrMut<'s, V> {
            panic!("unexpected stack")
        }
        fn commit(self) -> Result<NoStackGuard, Errno> {
            panic!("unexpected stack")
        }
    }
    // These controlled marked-OFD cases exercise the actual refusal helper and
    // adapter. The strict Guest rejects memory, injection and scheduler access;
    // they do not claim a native original-installation or close observation.
    #[tokio::test]
    async fn network_capability_probe_refuses_communication_before_guest_effects() {
        for policy in [NetworkPolicy::Record, NetworkPolicy::Replay] {
            let mut config = Config::default();
            config.network_trace.policy = policy;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let mut thread = tool.init_thread_state(tid, None);
            thread.add_fd(77, OFlag::O_RDWR, FdType::Socket, None).unwrap();
            thread.with_detfd(77, |fd| fd.restrict_network_capability_probe()).unwrap();
            thread.dup_fd(77, 79, OFlag::O_CLOEXEC).unwrap();
            let mut guest = DelegateGuest { config: &config, thread, requests: Mutex::new(vec![]) };
            for fd in [77, 79] {
                for number in [Sysno::read, Sysno::readv, Sysno::write, Sysno::writev,
                    Sysno::recvfrom, Sysno::recvmsg, Sysno::recvmmsg, Sysno::sendto,
                    Sysno::sendmsg, Sysno::sendmmsg, Sysno::connect, Sysno::bind,
                    Sysno::listen, Sysno::accept, Sysno::accept4, Sysno::shutdown,
                    Sysno::getsockopt, Sysno::setsockopt, Sysno::getsockname,
                    Sysno::getpeername, Sysno::ioctl, Sysno::vmsplice] {
                    let call = Syscall::from_raw(number,
                        syscalls::SyscallArgs::new(fd, 0, 1, 0, 0, 0));
                    let result = tool.try_handle_network_io(&mut guest, call).await
                        .expect("probe use must not escape to the ordinary dispatcher");
                    assert!(matches!(&result, Err(Error::Tool(_))));
                    assert_eq!(result.unwrap_err().to_string(), format!(
                        "shared network engine refused operation: IPv6 capability probe does not authorize {number}"));
                }
                for (number, args) in [
                    (Sysno::sendfile, [78, fd, 0, 1, 0, 0]),
                    (Sysno::tee, [78, fd, 1, 0, 0, 0]),
                    (Sysno::splice, [78, 0, fd, 0, 1, 0]),
                    (Sysno::copy_file_range, [78, 0, fd, 0, 1, 0]),
                    (Sysno::epoll_ctl, [78, libc::EPOLL_CTL_ADD as usize, fd, 0, 0, 0]),
                ] {
                    let [a,b,c,d,e,f] = args;
                    let call = Syscall::from_raw(number, syscalls::SyscallArgs::new(a,b,c,d,e,f));
                    assert!(matches!(tool.check_network_capability_probe_use(&mut guest, call), Err(Error::Tool(_))));
                }
            }
            assert!(guest.requests.lock().unwrap().is_empty());
            assert_eq!(*guest.thread.as_ref(), 0);
            assert!(guest.thread.original_connect.is_none());
        }
    }

    #[test]
    fn network_capability_probe_boundary_keeps_close_aliases_and_non_network_modes() {
        for policy in [NetworkPolicy::Record, NetworkPolicy::Replay,
            NetworkPolicy::Deny, NetworkPolicy::UnsafeLive] {
            let mut config = Config::default();
            config.network_trace.policy = policy;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let thread = tool.init_thread_state(tid, None);
            thread.add_fd(77, OFlag::O_RDWR, FdType::Socket, None).unwrap();
            thread.with_detfd(77, |fd| fd.restrict_network_capability_probe()).unwrap();
            thread.add_fd(78, OFlag::O_RDWR, FdType::Socket, None).unwrap();
            let mut guest = DelegateGuest { config: &config, thread, requests: Mutex::new(vec![]) };
            for number in [Sysno::close, Sysno::dup, Sysno::dup2, Sysno::dup3, Sysno::fstat] {
                let call = Syscall::from_raw(number, syscalls::SyscallArgs::new(77, 0, 0, 0, 0, 0));
                assert!(tool.check_network_capability_probe_use(&mut guest, call).is_ok());
            }
            for cmd in [syscalls::FcntlCmd::F_GETFL, syscalls::FcntlCmd::F_GETFD] {
                assert!(tool.check_network_capability_probe_use(&mut guest,
                    syscalls::Fcntl::new().with_fd(77).with_cmd(cmd).into()).is_ok());
            }
            for fd in [78, -1] {
                assert!(tool.check_network_capability_probe_use(&mut guest,
                    syscalls::Connect::new().with_fd(fd).into()).is_ok());
            }
            let result = tool.check_network_capability_probe_use(&mut guest,
                syscalls::Connect::new().with_fd(77).into());
            assert_eq!(result.is_err(), matches!(policy, NetworkPolicy::Record | NetworkPolicy::Replay));
            let result = tool.check_network_capability_probe_use(&mut guest,
                syscalls::Fcntl::new().with_fd(77)
                    .with_cmd(syscalls::FcntlCmd::F_SETFL(OFlag::O_NONBLOCK.bits())).into());
            assert_eq!(result.is_err(), matches!(policy, NetworkPolicy::Record | NetworkPolicy::Replay));
            guest.thread.remove_fd(77);
            assert!(tool.check_network_capability_probe_use(&mut guest,
                syscalls::Poll::new().into()).is_ok(),
                "without a live probe, even poll must not access the strict fixture's memory");
            assert!(guest.requests.lock().unwrap().is_empty());
            assert_eq!(*guest.thread.as_ref(), 0);
        }
    }

    // Batch traffic must not fall through to the per-thread recorder/replayer
    // while the shared message engine lacks its typed partial-batch contract.
    // The strict Guest panics on memory access, injection, clock, or RPC. The
    // queued native byte witnesses that refusing the batch has no data effect.
    #[tokio::test]
    async fn unsupported_socket_batches_stay_in_shared_dispatch_without_native_effects() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        for policy in [NetworkPolicy::Record, NetworkPolicy::Replay] {
            let (socket, mut peer) = UnixStream::pair().unwrap();
            peer.write_all(b"preserved").unwrap();
            let mut config = Config::default();
            config.network_trace.policy = policy;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let thread = tool.init_thread_state(tid, None);
            thread
                .add_fd(socket.as_raw_fd(), OFlag::empty(), FdType::Socket, None)
                .unwrap();
            let mut guest = DelegateGuest {
                config: &config,
                thread,
                requests: Mutex::new(vec![]),
            };
            let mut payload = [0xabu8; 9];
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr().cast(),
                iov_len: payload.len(),
            };
            let mut message: libc::mmsghdr = unsafe { std::mem::zeroed() };
            message.msg_hdr.msg_iov = &raw mut iov;
            message.msg_hdr.msg_iovlen = 1;
            message.msg_len = u32::MAX;
            let receive: Syscall = syscalls::Recvmmsg::new()
                .with_fd(socket.as_raw_fd())
                .with_mmsg(AddrMut::from_raw((&raw mut message) as usize))
                .with_vlen(1)
                .with_flags(libc::MSG_DONTWAIT as u32)
                .into();
            let send: Syscall = syscalls::Sendmmsg::new()
                .with_sockfd(socket.as_raw_fd())
                .with_msgvec(Addr::from_raw((&raw const message) as usize))
                .with_vlen(1)
                .with_flags(libc::MSG_DONTWAIT)
                .into();
            for call in [receive, send] {
                assert!(tool.network_io_owns(&mut guest, call));
                let result = tool
                    .try_handle_network_io(&mut guest, call)
                    .await
                    .expect("socket batch must remain owned by shared adapter");
                assert!(matches!(&result, Err(Error::Tool(_))));
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("batch and ancillary capture is not yet implemented")
                );
            }
            assert_eq!(
                *guest.thread.as_ref(),
                0,
                "ordinary delegate was not called"
            );
            assert!(guest.requests.lock().unwrap().is_empty());
            assert!(guest.thread.original_connect.is_none());
            assert_eq!(payload, [0xab; 9]);
            assert_eq!(message.msg_len, u32::MAX);
            let mut peer_available = -1;
            assert_eq!(
                unsafe { libc::ioctl(peer.as_raw_fd(), libc::FIONREAD, &mut peer_available) },
                0
            );
            assert_eq!(peer_available, 0, "refused send batch emitted no bytes");
            // The exact nonempty header is a valid native receive, not a
            // zero-vlen/null-pointer fixture that would have no data effect.
            assert_eq!(
                unsafe {
                    libc::recvmmsg(
                        socket.as_raw_fd(),
                        &raw mut message,
                        1,
                        libc::MSG_DONTWAIT,
                        std::ptr::null_mut(),
                    )
                },
                1
            );
            assert_eq!(message.msg_len, 9);
            assert_eq!(&payload, b"preserved");
            assert_eq!(
                unsafe {
                    libc::sendmmsg(socket.as_raw_fd(), &raw mut message, 1, libc::MSG_DONTWAIT)
                },
                1
            );
            assert_eq!(message.msg_len, 9);
            let mut echoed = [0; 9];
            peer.read_exact(&mut echoed).unwrap();
            assert_eq!(&echoed, b"preserved");
        }
    }

    #[tokio::test]
    async fn batch_boundary_does_not_reclassify_non_socket_or_explicit_live_policy() {
        for policy in [
            NetworkPolicy::Record,
            NetworkPolicy::Replay,
            NetworkPolicy::Deny,
            NetworkPolicy::UnsafeLive,
        ] {
            let mut config = Config::default();
            config.network_trace.policy = policy;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let thread = tool.init_thread_state(tid, None);
            thread
                .add_fd(77, OFlag::empty(), FdType::Socket, None)
                .unwrap();
            let mut guest = DelegateGuest {
                config: &config,
                thread,
                requests: Mutex::new(vec![]),
            };
            for fd in [77, 78] {
                if fd == 77 && matches!(policy, NetworkPolicy::Record | NetworkPolicy::Replay) {
                    continue;
                }
                for call in [
                    Syscall::from(syscalls::Recvmmsg::new().with_fd(fd)),
                    Syscall::from(syscalls::Sendmmsg::new().with_sockfd(fd)),
                ] {
                    assert!(!tool.network_io_owns(&mut guest, call));
                    assert!(tool.try_handle_network_io(&mut guest, call).await.is_none());
                }
            }
            assert!(guest.requests.lock().unwrap().is_empty());
            assert_eq!(*guest.thread.as_ref(), 0);
        }
    }

    #[tokio::test]
    async fn record_output_without_native_receipts_refuses_before_memory_or_native_effects() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let mut config = Config::default();
        config.network_trace.policy = NetworkPolicy::Record;
        let tid = Tid::from_raw(73);
        let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
        let thread = tool.init_thread_state(tid, None);
        thread
            .add_fd(socket.as_raw_fd(), OFlag::empty(), FdType::Socket, None)
            .unwrap();
        let mut guest = DelegateGuest {
            config: &config,
            thread,
            requests: Mutex::new(vec![]),
        };
        let payload = *b"exact native output";
        let iov = libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        };
        let fd = socket.as_raw_fd() as usize;
        let calls = [
            (Sysno::write, payload.as_ptr() as usize, payload.len()),
            (Sysno::writev, (&raw const iov) as usize, 1),
            (Sysno::sendto, payload.as_ptr() as usize, payload.len()),
        ];
        for (number, address, count) in calls {
            for (address, count) in [(address, count), (0, 0), (0, 1)] {
                let call = Syscall::from_raw(
                    number,
                    syscalls::SyscallArgs::new(fd, address, count, 0, 0, 0),
                );
                assert!(tool.network_io_owns(&mut guest, call));
                let result = tool
                    .try_handle_network_io(&mut guest, call)
                    .await
                    .expect("unqualified output must remain owned by the shared adapter");
                let Err(Error::Tool(error)) = result else {
                    panic!("output refusal changed type")
                };
                assert_eq!(
                    error.to_string(),
                    "shared network engine refused operation: Record network output requires authenticated native transmission receipts"
                );
            }
        }
        assert_eq!(
            *guest.thread.as_ref(),
            0,
            "ordinary delegate was not called"
        );
        assert!(guest.requests.lock().unwrap().is_empty());
        assert!(guest.thread.original_connect.is_none());
        let mut available = -1;
        assert_eq!(
            unsafe { libc::ioctl(peer.as_raw_fd(), libc::FIONREAD, &mut available) },
            0
        );
        assert_eq!(available, 0, "refused output emitted no bytes");
        // The exact nonempty operands are real native output, not inert shapes.
        for (number, address, count) in calls {
            assert_eq!(
                unsafe {
                    libc::syscall(
                        number as libc::c_long,
                        fd,
                        address,
                        count,
                        0usize,
                        0usize,
                        0usize,
                    )
                },
                payload.len() as libc::c_long
            );
            let mut observed = [0; 19];
            peer.read_exact(&mut observed).unwrap();
            assert_eq!(observed, payload);
        }
    }

    #[tokio::test]
    async fn record_output_refusal_preserves_non_socket_and_explicit_live_dispatch() {
        for policy in [
            NetworkPolicy::Record,
            NetworkPolicy::Replay,
            NetworkPolicy::Deny,
            NetworkPolicy::UnsafeLive,
        ] {
            let mut config = Config::default();
            config.network_trace.policy = policy;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let thread = tool.init_thread_state(tid, None);
            thread
                .add_fd(77, OFlag::empty(), FdType::Socket, None)
                .unwrap();
            let mut guest = DelegateGuest {
                config: &config,
                thread,
                requests: Mutex::new(vec![]),
            };
            for fd in [libc::STDOUT_FILENO, 78, 77] {
                if fd == 77 && matches!(policy, NetworkPolicy::Record | NetworkPolicy::Replay) {
                    continue;
                }
                for number in [Sysno::write, Sysno::writev, Sysno::sendto] {
                    let call = Syscall::from_raw(
                        number,
                        syscalls::SyscallArgs::new(fd as usize, 0, 1, 0, 0, 0),
                    );
                    assert!(!tool.network_io_owns(&mut guest, call));
                    assert!(tool.try_handle_network_io(&mut guest, call).await.is_none());
                }
            }
            assert!(guest.requests.lock().unwrap().is_empty());
            assert_eq!(*guest.thread.as_ref(), 0);
        }
    }

    // Explicit boundary inputs exercise the actual direct/owned adapter paths.
    // These do not manufacture a provider receipt or claim a qualified V3 state.
    struct CaptureGuardGuest<'a> {
        config: &'a Config,
        thread: crate::ThreadState<usize>,
        admitted: Option<crate::network_replay::NetworkFdReadAdmission>,
        state_query: bool,
        cleanup_failure: bool,
        events: Mutex<Vec<&'static str>>,
    }
    #[reverie::tool]
    impl GlobalRPC<GlobalState> for CaptureGuardGuest<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let mut events = self.events.lock().unwrap();
            let reply = match request.2 {
                GlobalRequest::Network(NetworkRequest::StreamSocketState { open_file }) => {
                    assert!(self.state_query);
                    assert!(events.is_empty());
                    let read = self.admitted.as_ref().expect("owned read fixture");
                    assert_eq!(
                        Some(open_file),
                        read.binding.map(|binding| binding.open_file)
                    );
                    events.push("state");
                    Ok(NetworkReply::StreamSocketState(Some(
                        NetworkStreamSocketState {
                            key: StreamSocketKeyV3 {
                                transport: NetworkTransportV2::UnixStream,
                                domain: libc::AF_UNIX,
                                socket_type: libc::SOCK_STREAM,
                                protocol: 0,
                            },
                            normalization: LinuxReceiveNormalizationV3 {
                                hz: LinuxReceiveHzV3::Hz1000,
                                peek_offset_set_supported: true,
                                system_rmem_max: 212992,
                                namespace_tcp_rmem_max: 6291456,
                                minimum_receive_buffer: 2304,
                            },
                            options: StreamSocketOptionsV3 {
                                peek_offset: Some(-1),
                                receive_low_water: 1,
                                receive_timeout: ReceiveTimeoutV3::Infinite,
                                receive_buffer: ReceiveBufferStateV3 {
                                    bytes: 212992,
                                    user_locked: false,
                                    tcp_scaling_ratio: 128,
                                },
                            },
                            consume_epoch: 0,
                            send_timeout: None,
                            option_generation: 0,
                        },
                    )))
                }
                GlobalRequest::Network(NetworkRequest::FinishFdRead { admission }) => {
                    assert_eq!(Some(&admission), self.admitted.as_ref());
                    assert_eq!(
                        events.as_slice(),
                        if self.state_query {
                            &["state"][..]
                        } else {
                            &[]
                        }
                    );
                    events.push("finish");
                    if self.cleanup_failure {
                        Err(NetworkRpcError::internal(
                            "controlled exact admission cleanup failure",
                        ))
                    } else {
                        Ok(NetworkReply::Unit)
                    }
                }
                other => panic!(
                    "unqualified capture must not obtain a Call, touch ingress or wait: {other:?}"
                ),
            };
            (None, GlobalResponse::Network(reply))
        }
        fn config(&self) -> &Config {
            self.config
        }
    }
    #[reverie::tool]
    impl Guest<Detcore<FailingDelegate>> for CaptureGuardGuest<'_> {
        type Memory = LocalMemory;
        type Stack = NoStack;
        fn tid(&self) -> Tid {
            Tid::from_raw(self.thread.dettid.as_raw())
        }
        fn pid(&self) -> Tid {
            self.tid()
        }
        fn ppid(&self) -> Option<Tid> {
            None
        }
        fn memory(&self) -> LocalMemory {
            panic!("unqualified capture accessed payload memory")
        }
        fn thread_state(&self) -> &crate::ThreadState<usize> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<usize> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected regs")
        }
        async fn stack(&mut self) -> NoStack {
            panic!("unexpected stack")
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemonization")
        }
        async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
            panic!("unqualified capture invoked native receive")
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("unexpected tail injection")
        }
        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("unqualified capture observed ingress time")
        }
    }

    #[tokio::test]
    async fn record_host_capture_refuses_missing_layout_authority_before_admission() {
        for (pin, tracking, detail) in [
            (
                false,
                false,
                "selected backend has no authenticated host socket-pin implementation",
            ),
            (
                true,
                false,
                "native stream capture lacks complete backend FD-table admission",
            ),
            (
                true,
                true,
                "Record host stream capture requires authenticated current-layout and copy authority",
            ),
        ] {
            let mut config = Config::default();
            config.network_trace.policy = NetworkPolicy::Record;
            config.backend_supports_host_socket_pin = pin;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let mut thread = tool.init_thread_state(tid, None);
            if tracking {
                thread.file_metadata = std::sync::Arc::new(Mutex::new(
                    crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid),
                ));
            }
            thread
                .add_fd(77, OFlag::empty(), FdType::Socket, None)
                .unwrap();
            let open_file = thread.socket_open_file_id(77).unwrap();
            let mut guest = CaptureGuardGuest {
                config: &config,
                thread,
                admitted: None,
                state_query: false,
                cleanup_failure: false,
                events: Mutex::new(Vec::new()),
            };
            let Err(Error::Tool(error)) = tool
                .begin_host_stream_call(&mut guest, 77, open_file, NetworkPolicy::Record)
                .await
            else {
                panic!("unqualified host capture did not refuse before admission")
            };
            assert_eq!(
                error.to_string(),
                format!("shared network engine refused operation: {detail}")
            );
            assert!(guest.events.lock().unwrap().is_empty());
            assert_eq!(*guest.thread.as_ref(), 0);
            assert!(guest.thread.original_connect.is_none());
            if tracking {
                // This narrow guard does not grant Replay fidelity; its prior
                // pin/census-only check is unchanged and still separate work.
                tool.check_host_stream_capture(&guest, NetworkPolicy::Replay)
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn record_owned_capture_refuses_and_releases_exact_read_without_touching_ingress() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        use crate::network_replay::NetworkFdPublicationAdmission;
        use crate::network_replay::NetworkFdPublicationPermit;
        use crate::network_replay::NetworkFdReadAdmission;
        use crate::network_replay::NetworkStreamOwner;
        for through_read in [false, true] {
            for cleanup_failure in [false, true] {
                let (mut endpoint, mut peer) = UnixStream::pair().unwrap();
                peer.write_all(b"retained").unwrap();
                let mut destination = [0xa5; 8];
                let mut config = Config::default();
                config.network_trace.policy = NetworkPolicy::Record;
                config.backend_supports_host_socket_pin = true;
                let tid = Tid::from_raw(73);
                let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
                let mut thread = tool.init_thread_state(tid, None);
                thread.file_metadata = std::sync::Arc::new(Mutex::new(
                    crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid),
                ));
                thread
                    .add_fd(endpoint.as_raw_fd(), OFlag::empty(), FdType::Socket, None)
                    .unwrap();
                let owner = NetworkStreamOwner {
                    thread: thread.dettid,
                    mm: thread.mm_id,
                };
                let (read, metadata) = {
                    let table = thread.file_metadata.lock().unwrap();
                    let binding = table.descriptor_binding(endpoint.as_raw_fd()).unwrap();
                    let read = NetworkFdReadAdmission {
                        publication: NetworkFdPublicationAdmission {
                            permit: NetworkFdPublicationPermit {
                                files: table.files_id,
                                owner,
                                lease: NetworkStreamLeaseId::controlled_fixture(1),
                            },
                            acknowledged_sequence: 0,
                            acknowledged_generation: 0,
                            recovery: None,
                        },
                        fd: endpoint.as_raw_fd(),
                        binding: Some(binding),
                        control: Some(NetworkStreamLeaseId::controlled_fixture(2)),
                        external_grant: None,
                    };
                    let metadata = crate::tool_local::NetworkFdReadMetadata {
                        binding: Some(binding),
                        socket: Some(binding.open_file),
                        nonblocking: Some(false),
                    };
                    (read, metadata)
                };
                let mut guest = CaptureGuardGuest {
                    config: &config,
                    thread,
                    admitted: Some(read.clone()),
                    state_query: through_read,
                    cleanup_failure,
                    events: Mutex::new(Vec::new()),
                };
                let result = if through_read {
                    let call = syscalls::Read::new()
                        .with_fd(endpoint.as_raw_fd())
                        .with_buf(AddrMut::from_ptr(destination.as_mut_ptr()))
                        .with_len(destination.len());
                    tool.network_read_from_admission(
                        &mut guest,
                        call,
                        NetworkPolicy::Record,
                        read,
                        metadata,
                    )
                    .await
                    .map(|_| ())
                } else {
                    tool.capture_admitted_host_stream_call(&mut guest, read, metadata)
                        .await
                        .map(|_| ())
                };
                let Err(Error::Tool(error)) = result else {
                    panic!("capture refusal changed type")
                };
                let primary = "shared network engine refused operation: Record host stream capture requires authenticated current-layout and copy authority";
                if cleanup_failure {
                    let chain = format!("{error:#}");
                    assert!(chain.contains(primary));
                    assert!(chain.contains("controlled exact admission cleanup failure"));
                    assert!(chain.contains("secondary network cleanup failure"));
                } else {
                    assert_eq!(error.to_string(), primary);
                }
                assert_eq!(
                    guest.events.lock().unwrap().as_slice(),
                    if through_read {
                        &["state", "finish"][..]
                    } else {
                        &["finish"][..]
                    }
                );
                assert_eq!(*guest.thread.as_ref(), 0);
                assert!(guest.thread.original_connect.is_none());
                assert!(guest.thread.original_file_metadata.is_none());
                assert_eq!(destination, [0xa5; 8]);
                let mut available = -1;
                assert_eq!(
                    unsafe { libc::ioctl(endpoint.as_raw_fd(), libc::FIONREAD, &mut available) },
                    0
                );
                assert_eq!(available, 8);
                endpoint.read_exact(&mut destination).unwrap();
                assert_eq!(&destination, b"retained");
            }
        }
    }

    struct DelegateGuest<'a> {
        config: &'a Config,
        thread: crate::ThreadState<usize>,
        requests: Mutex<Vec<GlobalRequest>>,
    }
    #[reverie::tool]
    impl GlobalRPC<GlobalState> for DelegateGuest<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            assert!(matches!(
                &request.2,
                GlobalRequest::Network(NetworkRequest::NativeOriginalConnectFailed { .. })
            ));
            self.requests.lock().unwrap().push(request.2.clone());
            (None, GlobalResponse::Network(Ok(NetworkReply::Unit)))
        }
        fn config(&self) -> &Config {
            self.config
        }
    }
    #[reverie::tool]
    impl Guest<Detcore<FailingDelegate>> for DelegateGuest<'_> {
        type Memory = LocalMemory;
        type Stack = NoStack;
        fn tid(&self) -> Tid {
            Tid::from_raw(self.thread.dettid.as_raw())
        }
        fn pid(&self) -> Tid {
            self.tid()
        }
        fn ppid(&self) -> Option<Tid> {
            None
        }
        fn memory(&self) -> LocalMemory {
            panic!("unexpected memory")
        }
        fn thread_state(&self) -> &crate::ThreadState<usize> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<usize> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected regs")
        }
        async fn stack(&mut self) -> NoStack {
            panic!("unexpected stack")
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemonization")
        }
        async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
            panic!("a delegate failure must not manufacture physical invocation")
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("unexpected tail injection")
        }
        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("unexpected clock")
        }
    }

    // This exercises the real delegate/lossless-helper and failure-fence
    // boundaries, not unreachable Native admission. Local is explicit setup;
    // no capability, Call, Prepared, Returned, provider or final-wait receipt is
    // created. RPC ACK is explicit boundary input; this checks the actual
    // failure request and pending custody, not scheduler final cleanup. The
    // production None gate remains untouched.
    #[tokio::test]
    async fn original_file_delegate_tool_and_io_errors_reach_retained_failure_fence_unchanged() {
        for sequential in [true, false] {
            for (mode, expected) in [
                (0, "exact delegate tool failure"),
                (2, "exact delegate IO failure"),
            ] {
                let config = Config {
                    sequentialize_threads: sequential,
                    ..Config::default()
                };
                let tid = Tid::from_raw(73);
                let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
                let mut thread = tool.init_thread_state(tid, None);
                *thread.as_mut() = mode;
                let call = syscalls::Fcntl::new()
                    .with_fd(7)
                    .with_cmd(syscalls::FcntlCmd::F_GETFL);
                let operation = FileOperation::GetFlags;
                let local = Local {
                    arguments: Arguments {
                        kind: Kind::File(operation),
                        operation: ExternalOpId::new(thread.dettid, thread.stats.syscall_count),
                        files: thread.file_metadata.lock().unwrap().files_id,
                        binding: None,
                        fd: 7,
                        address: operation.syscall() as u64,
                        length: operation.command(),
                        original_count: 0,
                    },
                    raw_arguments: [7, libc::F_GETFL as usize, 0, 0, 0, 0],
                    admission: None,
                    invoked: false,
                    returned: None,
                };
                thread.original_connect = Some(local.clone());
                let mut guest = DelegateGuest {
                    config: &config,
                    thread,
                    requests: Mutex::new(vec![]),
                };
                assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
                let result = tool
                    .record_or_replay_preserving_tool_errors(&mut guest, call)
                    .await;
                assert_eq!(result.as_ref().unwrap_err().to_string(), expected);
                assert!(matches!(
                    (&result, mode),
                    (Err(Error::Tool(_)), 0) | (Err(Error::Io(_)), 2)
                ));
                assert_eq!(*guest.thread.as_ref(), mode + 1);
                assert!(guest.requests.lock().unwrap().is_empty());
                let mut fence = Box::pin(tool.finish_original_invocation(&mut guest, result));
                let waker = futures::task::noop_waker();
                assert!(
                    fence
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(&waker))
                        .is_pending()
                );
                drop(fence);
                let requests = guest.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                assert!(matches!(&requests[0], GlobalRequest::Network(
                    NetworkRequest::NativeOriginalConnectFailed { local: actual, detail })
                    if actual == &local && detail == expected));
                assert_eq!(guest.thread.original_connect.as_ref(), Some(&local));
                assert!(guest.thread.original_file_metadata.is_none());
            }
        }
    }

    // The actual CLI Replayer classification is checked in its own module.
    // Here the real adapter must reject that declaration before creating Local
    // custody, contacting the provider, or calling a delegate that can create a
    // placeholder. No backend admission is fabricated by this boundary test.
    #[tokio::test]
    async fn recorded_openat_refuses_before_local_call_or_provider_enrollment() {
        for policy in [NetworkPolicy::Record, NetworkPolicy::Replay] {
            for sequential in [true, false] {
                let mut config = Config {
                    sequentialize_threads: sequential,
                    ..Config::default()
                };
                config.network_trace.policy = policy;
                let tid = Tid::from_raw(73);
                let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
                let thread = tool.init_thread_state(tid, None);
                let mut guest = DelegateGuest {
                    config: &config,
                    thread,
                    requests: Mutex::new(vec![]),
                };
                let result = tool
                    .network_original_openat(&mut guest, syscalls::Openat::new())
                    .await;
                assert!(matches!(&result, Err(Error::Tool(_))));
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "shared network engine refused operation: original Openat admission requires the actual native allocator"
                );
                assert!(guest.thread.original_connect.is_none());
                assert!(guest.thread.original_file_metadata.is_none());
                assert!(guest.requests.lock().unwrap().is_empty());
                assert_eq!(*guest.thread.as_ref(), 0, "delegate was never invoked");
            }
        }
    }

    struct ZeroReadGuest<'a> {
        config: &'a Config,
        thread: crate::ThreadState<usize>,
        calls: usize,
        admitted: Option<Mutex<ZeroReadAdmissionBoundary>>,
    }
    // Explicit adapter boundary inputs, not a provider-issued admission or
    // physical selection receipt. The real host Read supplies only its result.
    struct ZeroReadAdmissionBoundary {
        read: crate::network_replay::NetworkFdReadAdmission,
        admission: Option<crate::network_replay::original_connect::Admission>,
        events: Vec<&'static str>,
        returned: Option<i64>,
    }
    fn native_zero_read(call: syscalls::Read) -> Result<i64, Errno> {
        assert_eq!(call.len(), 0);
        let result = unsafe {
            libc::read(
                call.fd(),
                call.buf()
                    .map_or(std::ptr::null_mut(), |p| p.as_raw() as *mut libc::c_void),
                0,
            )
        };
        if result < 0 {
            Err(Errno::new(
                std::io::Error::last_os_error().raw_os_error().unwrap(),
            ))
        } else {
            Ok(result as i64)
        }
    }
    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ZeroReadGuest<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let Some(boundary) = &self.admitted else {
                panic!(
                    "local zero-Read validation must not publish or release network ingress: {:?}",
                    request.2
                )
            };
            let mut boundary = boundary.lock().unwrap();
            let local = self
                .thread
                .original_connect
                .as_ref()
                .expect("Local precedes admission RPC");
            let reply = match request.2 {
                GlobalRequest::Network(NetworkRequest::BeginOriginalFileFromRead {
                    arguments,
                    read,
                    source,
                }) => {
                    assert!(boundary.events.is_empty());
                    assert_eq!(source, crate::OriginalFileExecution::Native);
                    assert_eq!(read, boundary.read);
                    assert_eq!(arguments, local.arguments);
                    assert_eq!(arguments.kind, Kind::Read);
                    assert_eq!(arguments.files, read.publication.permit.files);
                    assert_eq!(arguments.fd, read.fd);
                    assert_eq!(arguments.binding, read.binding);
                    assert_eq!(arguments.address, local.raw_arguments[1] as u64);
                    assert_eq!(arguments.original_count, 0);
                    assert!(
                        !local.invoked && local.returned.is_none() && local.admission.is_none()
                    );
                    let admission = crate::network_replay::original_connect::Admission {
                        call: crate::network_replay::NetworkStreamCallId::controlled_fixture(1),
                        arguments,
                    };
                    boundary.admission = Some(admission.clone());
                    boundary.events.push("begin-native");
                    NetworkReply::OriginalConnectAdmission(admission)
                }
                GlobalRequest::Network(NetworkRequest::NativeSubmitOriginalConnect {
                    admission,
                }) => {
                    assert_eq!(boundary.events, ["begin-native"]);
                    assert_eq!(boundary.admission.as_ref(), Some(&admission));
                    assert_eq!(local.admission.as_ref(), Some(&admission));
                    assert!(!local.invoked && local.returned.is_none());
                    boundary.events.push("submit");
                    NetworkReply::Unit
                }
                GlobalRequest::Network(NetworkRequest::NativeOriginalConnectOutcome {
                    admission,
                }) => {
                    assert_eq!(
                        boundary.events,
                        ["begin-native", "submit", "typed-native-read"]
                    );
                    assert_eq!(boundary.admission.as_ref(), Some(&admission));
                    assert_eq!(local.admission.as_ref(), Some(&admission));
                    assert!(local.invoked);
                    let returned = boundary.returned.expect("native Read completed");
                    assert_eq!(local.returned, Some(returned));
                    boundary.events.push("outcome");
                    NetworkReply::OriginalConnectOutcome(
                        Box::new(crate::network_runtime::original_connect::Outcome {
                            admission,
                            returned,
                            pin: None,
                            address: None,
                            socket: None,
                            read_copy: None,
                        }),
                    )
                }
                GlobalRequest::Network(NetworkRequest::NativeRetireOriginalConnect {
                    admission,
                }) => {
                    assert_eq!(
                        boundary.events,
                        ["begin-native", "submit", "typed-native-read", "outcome"]
                    );
                    assert_eq!(boundary.admission.as_ref(), Some(&admission));
                    assert_eq!(local.admission.as_ref(), Some(&admission));
                    assert_eq!(local.returned, boundary.returned);
                    boundary.events.push("retire");
                    NetworkReply::Unit
                }
                other => panic!(
                    "zero Read must not release, emulate or publish network ingress: {other:?}"
                ),
            };
            (None, GlobalResponse::Network(Ok(reply)))
        }
        fn config(&self) -> &Config {
            self.config
        }
    }
    #[reverie::tool]
    impl Guest<Detcore<FailingDelegate>> for ZeroReadGuest<'_> {
        type Memory = LocalMemory;
        type Stack = NoStack;
        fn tid(&self) -> Tid {
            Tid::from_raw(self.thread.dettid.as_raw())
        }
        fn pid(&self) -> Tid {
            self.tid()
        }
        fn ppid(&self) -> Option<Tid> {
            None
        }
        fn memory(&self) -> LocalMemory {
            panic!("zero Read must not access payload in the adapter")
        }
        fn thread_state(&self) -> &crate::ThreadState<usize> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<usize> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected regs")
        }
        async fn stack(&mut self) -> NoStack {
            panic!("unexpected stack")
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemonization")
        }
        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            assert!(
                self.admitted.is_none(),
                "admitted zero Read requires the typed shared boundary"
            );
            let (number, arguments) = syscall.into_parts();
            let Syscall::Read(call) = Syscall::from_raw(number, arguments) else {
                panic!("zero Read changed syscall shape")
            };
            self.calls += 1;
            native_zero_read(call)
        }
        async fn inject_original_read(
            &mut self,
            call: syscalls::Read,
        ) -> reverie::InjectedReadResult {
            let mut boundary = self
                .admitted
                .as_ref()
                .expect("typed Read requires explicit admission")
                .lock()
                .unwrap();
            assert_eq!(boundary.events, ["begin-native", "submit"]);
            let local = self.thread.original_connect.as_mut().unwrap();
            assert_eq!(local.admission, boundary.admission);
            assert!(local.invoked && local.returned.is_none());
            let (_, raw) = call.into_parts();
            assert_eq!(
                local.raw_arguments,
                [raw.arg0, raw.arg1, raw.arg2, raw.arg3, raw.arg4, raw.arg5]
            );
            self.calls += 1;
            let result = native_zero_read(call);
            let returned = match result {
                Ok(n) => n,
                Err(errno) => -i64::from(errno.into_raw()),
            };
            // Supply the adapter's expected completion input explicitly. This
            // assignment is not an authenticated backend observation.
            local.returned = Some(returned);
            boundary.returned = Some(returned);
            boundary.events.push("typed-native-read");
            reverie::InjectedReadResult::Complete(result)
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("unexpected tail injection")
        }
        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("local zero Read must not observe ingress time")
        }
    }

    struct CloseDispatchBoundary {
        admission: Option<crate::network_replay::original_connect::Admission>,
        outcome: Option<crate::network_runtime::original_connect::Outcome>,
        events: Vec<&'static str>,
    }

    struct CloseDispatchGuest<'a> {
        config: &'a Config,
        global: crate::tool_global::GlobalState,
        thread: crate::ThreadState<usize>,
        engine: Mutex<crate::network_replay::NetworkReplayEngine>,
        owner: crate::network_replay::NetworkStreamOwner,
        boundary: Mutex<CloseDispatchBoundary>,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for CloseDispatchGuest<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let mut engine = self.engine.lock().unwrap();
            let mut boundary = self.boundary.lock().unwrap();
            let reply = match request.2 {
                GlobalRequest::Network(NetworkRequest::FdPublication(request)) => {
                    use crate::network_replay::NetworkFdPublicationReply as P;
                    use crate::network_replay::NetworkFdPublicationRequest as Q;
                    let reply = match request {
                        Q::Acquire { files } => {
                            P::Admitted(engine.acquire_fd_publication(self.owner, files).unwrap())
                        }
                        Q::Publish { permit, batch } => P::Published(
                            engine
                                .publish_fd_publication(self.owner, permit, &batch)
                                .unwrap(),
                        ),
                        Q::Acknowledge { permit, batch } => {
                            engine
                                .acknowledge_fd_publication(self.owner, permit, &batch)
                                .unwrap();
                            P::Released
                        }
                        Q::ReleaseEmpty { permit } => {
                            engine
                                .release_empty_fd_publication(self.owner, permit)
                                .unwrap();
                            P::Released
                        }
                    };
                    NetworkReply::FdPublication(reply)
                }
                GlobalRequest::Network(NetworkRequest::BeginFdRead { files, fd })
                | GlobalRequest::Network(NetworkRequest::BeginOrdinaryFdRead { files, fd }) => {
                    NetworkReply::FdRead(engine.begin_fd_read(self.owner, files, fd).unwrap())
                }
                GlobalRequest::Network(NetworkRequest::FinishFdRead { admission }) => {
                    engine.finish_fd_read(self.owner, admission).unwrap();
                    boundary.events.push("read-ebadf-release");
                    NetworkReply::Unit
                }
                GlobalRequest::Network(NetworkRequest::NativeBeginOriginalExternalFromRead {
                    arguments,
                    read,
                }) => {
                    assert!(boundary.events.is_empty());
                    assert_eq!(arguments.kind, Kind::Close);
                    let admission = engine
                        .begin_original_external_from_read(self.owner, arguments, read)
                        .unwrap();
                    engine
                        .original_connect_provider_submitted(self.owner, &admission)
                        .unwrap();
                    engine
                        .original_call_prepared(self.owner, &admission, None, 17)
                        .unwrap();
                    boundary.admission = Some(admission.clone());
                    boundary.events.push("admitted-prepared");
                    NetworkReply::OriginalConnectAdmission(admission)
                }
                GlobalRequest::Network(NetworkRequest::NativeSubmitOriginalConnect {
                    admission,
                }) => {
                    assert_eq!(boundary.admission.as_ref(), Some(&admission));
                    assert_eq!(boundary.events, ["admitted-prepared"]);
                    engine
                        .original_connect_invoked(self.owner, &admission)
                        .unwrap();
                    boundary.events.push("submitted");
                    NetworkReply::Unit
                }
                GlobalRequest::Network(NetworkRequest::NativeOriginalConnectOutcome {
                    admission,
                }) => {
                    assert_eq!(boundary.admission.as_ref(), Some(&admission));
                    assert_eq!(
                        boundary.events,
                        ["admitted-prepared", "submitted", "closed"]
                    );
                    boundary.events.push("outcome");
                    NetworkReply::OriginalConnectOutcome(Box::new(
                        boundary
                            .outcome
                            .clone()
                            .expect("physical close outcome missing"),
                    ))
                }
                GlobalRequest::Network(NetworkRequest::NativeRetireOriginalConnect {
                    admission,
                }) => {
                    assert_eq!(boundary.admission.as_ref(), Some(&admission));
                    assert_eq!(
                        boundary.events,
                        ["admitted-prepared", "submitted", "closed", "outcome"]
                    );
                    engine
                        .finish_original_connect(self.owner, &admission)
                        .unwrap();
                    boundary.events.push("retired");
                    NetworkReply::Unit
                }
                other => panic!("unexpected close-dispatch request: {other:?}"),
            };
            (None, GlobalResponse::Network(Ok(reply)))
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore<FailingDelegate>> for CloseDispatchGuest<'_> {
        type Memory = LocalMemory;
        type Stack = NoStack;

        fn tid(&self) -> Tid {
            Tid::from_raw(self.owner.thread.as_raw())
        }

        fn pid(&self) -> Tid {
            self.tid()
        }

        fn ppid(&self) -> Option<Tid> {
            None
        }

        fn local_global_state(&self) -> Option<&GlobalState> {
            Some(&self.global)
        }

        fn memory(&self) -> LocalMemory {
            LocalMemory::new()
        }

        fn thread_state(&self) -> &crate::ThreadState<usize> {
            &self.thread
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<usize> {
            &mut self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            unsafe { std::mem::zeroed() }
        }

        async fn stack(&mut self) -> NoStack {
            panic!("close dispatcher must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("close dispatcher must not daemonize")
        }

        async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
            let (number, arguments) = syscall.into_parts();
            let Syscall::Close(call) = Syscall::from_raw(number, arguments) else {
                panic!("close dispatcher changed physical syscall shape")
            };
            let admission = self
                .thread
                .original_connect
                .as_ref()
                .and_then(|local| local.admission.clone())
                .expect("close injection lost admission");
            assert!(self.thread.original_connect.as_ref().unwrap().invoked);
            let selected =
                crate::tool_local::original_close_tests::selected(self.owner, &admission);
            {
                let mut engine = self.engine.lock().unwrap();
                let mut metadata = self.thread.file_metadata.lock().unwrap();
                engine
                    .publish_original_close_selection(
                        self.owner,
                        &admission,
                        &selected,
                        &mut metadata,
                    )
                    .unwrap();
            }
            let result = unsafe { libc::close(call.fd()) };
            let returned = if result == 0 {
                0
            } else {
                -i64::from(
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .expect("close errno missing"),
                )
            };
            {
                let mut engine = self.engine.lock().unwrap();
                engine
                    .original_connect_returned(self.owner, &admission, returned)
                    .unwrap();
                engine
                    .original_connect_provider_retired(self.owner, &admission, returned)
                    .unwrap();
                engine
                    .original_connect_pin_released(self.owner, &admission)
                    .unwrap();
            }
            self.thread.original_connect.as_mut().unwrap().returned = Some(returned);
            let mut boundary = self.boundary.lock().unwrap();
            assert_eq!(boundary.events, ["admitted-prepared", "submitted"]);
            boundary.outcome = Some(crate::network_runtime::original_connect::Outcome {
                admission,
                returned,
                pin: None,
                address: None,
                socket: None,
                read_copy: None,
            });
            boundary.events.push("closed");
            if returned == 0 {
                Ok(0)
            } else {
                Err(Errno::new((-returned) as i32))
            }
        }

        async fn inject_original_read(&mut self, _: syscalls::Read) -> reverie::InjectedReadResult {
            panic!("retired close generation must refuse Read before physical injection")
        }

        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("unexpected tail injection")
        }

        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }

        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }

        fn read_clock(&mut self) -> Result<u64, Error> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn v4_close_dispatch_joins_physical_effect_lifetime_and_fd_generation() {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::fd::IntoRawFd;
        use std::os::fd::OwnedFd;

        fn pipe() -> (OwnedFd, OwnedFd) {
            let mut fds = [-1; 2];
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        }

        let (read_end, _write_end) = pipe();
        let fd = read_end.into_raw_fd();
        let (engine, metadata, owner) =
            crate::tool_local::original_close_tests::dispatcher_fixture(fd);
        let old = metadata.descriptor_binding(fd).unwrap();
        let mut config = Config {
            sequentialize_threads: false,
            max_timeslice: None,
            epoch_explicit: true,
            epoch: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let tool: Detcore<FailingDelegate> =
            Detcore::new(Tid::from_raw(owner.thread.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(owner.thread.as_raw()), None);
        thread.dettid = owner.thread;
        thread.mm_id = owner.mm;
        thread.stats.syscall_count = 10;
        thread.end_of_timeslice = Some(LogicalTime::MAX);
        thread.file_metadata = std::sync::Arc::new(Mutex::new(metadata));
        let mut guest = CloseDispatchGuest {
            config: &config,
            global: GlobalState::native_record_view_fixture(&config),
            thread,
            engine: Mutex::new(engine),
            owner,
            boundary: Mutex::new(CloseDispatchBoundary {
                admission: None,
                outcome: None,
                events: Vec::new(),
            }),
        };

        assert_eq!(
            tool.handle_syscall_event(&mut guest, syscalls::Close::new().with_fd(fd).into(),)
                .await
                .unwrap(),
            0
        );
        assert!(guest.thread.original_connect.is_none());
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        let mut byte = 0u8;
        let read = syscalls::Read::new()
            .with_fd(fd)
            .with_buf(AddrMut::from_ptr(std::ptr::addr_of_mut!(byte)))
            .with_len(1);
        assert!(matches!(
            tool.handle_syscall_event(&mut guest, read.into()).await,
            Err(Error::Errno(errno)) if errno == Errno::EBADF
        ));
        assert_eq!(guest.thread.descriptor_binding(fd), Err(Errno::EBADF));

        let (replacement_source, replacement_peer) = pipe();
        let source = replacement_source.into_raw_fd();
        if source != fd {
            assert_eq!(unsafe { libc::dup3(source, fd, libc::O_CLOEXEC) }, fd);
            assert_eq!(unsafe { libc::close(source) }, 0);
        }
        let replacement_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(replacement_fd.as_raw_fd(), fd);
        let replacement = {
            let mut engine = guest.engine.lock().unwrap();
            let mut metadata = guest.thread.file_metadata.lock().unwrap();
            crate::tool_local::original_close_tests::dispatcher_install(
                &mut engine,
                &mut metadata,
                owner,
                fd,
            )
        };
        assert_eq!(replacement.slot, old.slot);
        assert_ne!(replacement.generation, old.generation);
        assert_eq!(guest.thread.descriptor_binding(fd).unwrap(), replacement);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC
        );
        assert_eq!(
            guest.boundary.lock().unwrap().events,
            [
                "admitted-prepared",
                "submitted",
                "closed",
                "outcome",
                "retired",
                "read-ebadf-release"
            ]
        );
        drop(replacement_peer);
        drop(replacement_fd);
    }

    struct LegacyFlagsGuest<'a> {
        config: &'a Config,
        thread: crate::ThreadState<usize>,
        engine: Mutex<crate::network_replay::NetworkReplayEngine>,
        now: LogicalTime,
        resume: Option<ResumeStatus>,
    }
    #[reverie::tool]
    impl GlobalRPC<GlobalState> for LegacyFlagsGuest<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            use crate::network_replay::NetworkReceiveOptions;
            use crate::network_replay::StreamReceiveOutcome;
            let mut engine = self.engine.lock().unwrap();
            let response = match request.2 {
                GlobalRequest::GlobalTimeLowerBound => {
                    GlobalResponse::GlobalTimeLowerBound(self.now)
                }
                GlobalRequest::RequestResources(resources, _) => {
                    assert!(resources.signal_interrupt_errno().is_some());
                    GlobalResponse::RequestResources(
                        self.resume.clone().expect("wait response fixture missing"),
                    )
                }
                GlobalRequest::Network(request) => {
                    let reply = match request {
                        NetworkRequest::StreamSocketState { open_file } => {
                            NetworkReply::StreamSocketState(
                                engine.stream_socket_state(open_file).unwrap(),
                            )
                        }
                        NetworkRequest::ReleaseEligible(now) => NetworkReply::ReadyChannels(
                            engine.release_eligible(now).unwrap().into_iter().collect(),
                        ),
                        NetworkRequest::ReceiveStream {
                            open_file,
                            maximum,
                            nonblocking,
                            flags,
                            receive_low_water,
                        } => {
                            let outcome = engine
                                .receive_stream_with_options(
                                    open_file,
                                    NetworkReceiveOptions {
                                        maximum,
                                        nonblocking,
                                        flags,
                                        receive_low_water,
                                    },
                                )
                                .unwrap();
                            NetworkReply::StreamReceive(match outcome {
                                StreamReceiveOutcome::Bytes(bytes) => {
                                    NetworkStreamReceive::Bytes(bytes)
                                }
                                StreamReceiveOutcome::EndOfFile => NetworkStreamReceive::EndOfFile,
                                StreamReceiveOutcome::Error(errno) => {
                                    NetworkStreamReceive::Error(errno)
                                }
                                StreamReceiveOutcome::WouldBlock => {
                                    NetworkStreamReceive::WouldBlock
                                }
                                StreamReceiveOutcome::Pending => NetworkStreamReceive::Pending,
                            })
                        }
                        other => panic!("unexpected network request: {other:?}"),
                    };
                    GlobalResponse::Network(Ok(reply))
                }
                other => panic!("unexpected request: {other:?}"),
            };
            (None, response)
        }
        fn config(&self) -> &Config {
            self.config
        }
    }
    #[reverie::tool]
    impl Guest<Detcore<FailingDelegate>> for LegacyFlagsGuest<'_> {
        type Memory = LocalMemory;
        type Stack = NoStack;
        fn tid(&self) -> Tid {
            Tid::from_raw(self.thread.dettid.as_raw())
        }
        fn pid(&self) -> Tid {
            self.tid()
        }
        fn ppid(&self) -> Option<Tid> {
            None
        }
        fn memory(&self) -> LocalMemory {
            LocalMemory::new()
        }
        fn thread_state(&self) -> &crate::ThreadState<usize> {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<usize> {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected regs")
        }
        async fn stack(&mut self) -> NoStack {
            panic!("unexpected stack")
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemonization")
        }
        async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
            panic!("a delegate failure must not manufacture physical invocation")
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("unexpected tail injection")
        }
        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("unexpected clock")
        }
    }

    #[tokio::test]
    async fn adversarial_v2_recv_peek_retains_payload_in_actual_adapter() {
        use detcore_model::network_trace::NetworkChannelV2;
        use detcore_model::network_trace::NetworkTraceV2;

        use crate::network_replay::NetworkReplayEngine;
        use crate::network_replay::StreamReceiveOutcome;
        for flags in [0, libc::MSG_PEEK] {
            let mut config = Config::default();
            config.network_trace.policy = NetworkPolicy::Replay;
            let tid = Tid::from_raw(73);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let thread = tool.init_thread_state(tid, None);
            thread
                .add_fd(77, OFlag::empty(), FdType::Socket, None)
                .unwrap();
            let ofd = thread.socket_open_file_id(77).unwrap();
            let now =
                LogicalTime::from_nanos(config.epoch.timestamp_nanos_opt().unwrap() as u64 + 1);
            let channel = NetworkChannelId(1);
            let trace = NetworkTraceV2 {
                epoch: config.epoch,
                channels: vec![NetworkChannelV2 {
                    id: channel,
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::OutboundClient,
                    local_address: None,
                    peer_address: Some(NetworkAddressV2::Inet4 {
                        address: [127, 0, 0, 1],
                        port: 1234,
                    }),
                    accepted_from: None,
                }],
                inputs: vec![NetworkInputEventV2 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: now,
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::StreamBytes {
                        stream_offset: 0,
                        bytes: b"abc".to_vec(),
                    },
                }],
                outputs: vec![],
            };
            let mut engine = NetworkReplayEngine::replay(trace).unwrap();
            engine.bind(ofd, channel).unwrap();
            let mut guest = LegacyFlagsGuest {
                config: &config,
                thread,
                engine: Mutex::new(engine),
                now,
                resume: None,
            };
            let mut buffer = [0u8; 1];
            let call = syscalls::Recvfrom::new()
                .with_fd(77)
                .with_buf(AddrMut::from_ptr(buffer.as_mut_ptr()))
                .with_len(1)
                .with_flags(flags);
            let result = tool
                .network_recvfrom(&mut guest, call, NetworkPolicy::Replay)
                .await
                .unwrap();
            assert_eq!(result, 1);
            assert_eq!(buffer, [b'a']);
            let remaining = guest
                .engine
                .lock()
                .unwrap()
                .receive_stream(ofd, 3, true)
                .unwrap();
            assert_eq!(
                remaining,
                StreamReceiveOutcome::Bytes(if flags == 0 {
                    b"bc".to_vec()
                } else {
                    b"abc".to_vec()
                }),
                "recv flags {flags} changed consumption"
            );
        }
    }

    #[tokio::test]
    async fn legacy_network_wait_surfaces_signal_with_the_syscall_specific_errno() {
        use crate::network_replay::NetworkReplayEngine;

        for (resume, interrupt, expected) in [
            (ResumeStatus::Normal, Errno::EINTR, None),
            (
                ResumeStatus::Signaled(None),
                Errno::EINTR,
                Some(Errno::EINTR),
            ),
            (
                ResumeStatus::Signaled(None),
                Errno::ERESTARTSYS,
                Some(Errno::ERESTARTSYS),
            ),
        ] {
            let config = Config { sequentialize_threads: true, ..Config::default() };
            let tid = Tid::from_raw(74);
            let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
            let mut thread = tool.init_thread_state(tid, None);
            thread.detpid = Some(crate::DetPid::from_raw(tid.as_raw()));
            thread
                .add_fd(78, OFlag::empty(), FdType::Socket, None)
                .unwrap();
            let open_file = thread.socket_open_file_id(78).unwrap();
            let mut guest = LegacyFlagsGuest {
                config: &config,
                thread,
                engine: Mutex::new(NetworkReplayEngine::record(config.epoch)),
                now: LogicalTime::from_nanos(1),
                resume: Some(resume),
            };
            let result = tool
                .wait_for_network(&mut guest, open_file, NetworkWaitKind::Readable, interrupt)
                .await;
            match expected {
                None => result.unwrap(),
                Some(errno) => {
                    assert!(
                        matches!(&result, Err(Error::Errno(actual)) if *actual == errno),
                        "wait returned {result:?}, expected {errno}"
                    )
                }
            }
        }
    }

    struct InaccessibleZeroBuffer(*mut libc::c_void);
    impl InaccessibleZeroBuffer {
        fn new() -> Self {
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            Self(address)
        }
    }
    impl Drop for InaccessibleZeroBuffer {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::munmap(self.0, 4096) }, 0);
        }
    }

    // Real Linux results pass through the capability-None network dispatcher
    // callee and existing ordinary zero-Read handler. This is not a native
    // provider/Call or complete Record->Replay qualification. The strict Guest
    // rejects payload access, clock/RPC activity and Recorder/Replayer calls.
    #[tokio::test]
    async fn zero_scalar_read_network_policies_preserve_native_validation_without_ingress() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        for policy in [NetworkPolicy::Record, NetworkPolicy::Replay] {
            for queued in [false, true] {
                for nonblocking in [false, true] {
                    let (mut endpoint, mut peer) = UnixStream::pair().unwrap();
                    endpoint.set_nonblocking(nonblocking).unwrap();
                    if queued {
                        peer.write_all(b"retained").unwrap();
                    }
                    let invalid = InaccessibleZeroBuffer::new();
                    let mut config = Config::default();
                    config.network_trace.policy = policy;
                    let tid = Tid::from_raw(73);
                    let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
                    let thread = tool.init_thread_state(tid, None);
                    thread
                        .add_fd(
                            endpoint.as_raw_fd(),
                            if nonblocking {
                                OFlag::O_NONBLOCK
                            } else {
                                OFlag::empty()
                            },
                            FdType::Socket,
                            None,
                        )
                        .unwrap();
                    let mut guest = ZeroReadGuest {
                        config: &config,
                        thread,
                        calls: 0,
                        admitted: None,
                    };
                    for (address, expected) in [
                        (0, None),
                        (invalid.0 as usize, None),
                        (usize::MAX, Some(Errno::EFAULT)),
                    ] {
                        let call = syscalls::Read::new()
                            .with_fd(endpoint.as_raw_fd())
                            .with_buf(AddrMut::from_raw(address))
                            .with_len(0);
                        let result = tool
                            .try_handle_network_io(&mut guest, call.into())
                            .await
                            .expect("registered socket Read remains network-owned");
                        match expected {
                            None => assert_eq!(result.unwrap(), 0),
                            Some(errno) => assert!(
                                matches!(result, Err(Error::Errno(actual)) if actual == errno)
                            ),
                        }
                        let mut available = -1;
                        assert_eq!(
                            unsafe {
                                libc::ioctl(endpoint.as_raw_fd(), libc::FIONREAD, &mut available)
                            },
                            0
                        );
                        assert_eq!(available, if queued { 8 } else { 0 });
                    }
                    assert_eq!(guest.calls, 3);
                    assert_eq!(
                        *guest.thread.as_ref(),
                        0,
                        "ordinary Recorder/Replayer was not called"
                    );
                    assert!(guest.thread.original_connect.is_none());
                    assert!(crate::network_replay::backend_fd_table_capability(&config).is_none());
                    if !queued {
                        peer.write_all(b"retained").unwrap();
                    }
                    let mut bytes = [0; 8];
                    endpoint.read_exact(&mut bytes).unwrap();
                    assert_eq!(&bytes, b"retained");
                }
            }
        }
    }

    // Exercises the admitted adapter path with explicit admission/ACK and
    // completion boundary inputs. It does not qualify provider selection,
    // backend Prepared/Returned issuance or a real Record->Replay session.
    #[tokio::test]
    async fn zero_scalar_read_admitted_policies_use_native_call_and_retire_without_ingress() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        use crate::network_replay::NetworkFdPublicationAdmission;
        use crate::network_replay::NetworkFdPublicationPermit;
        use crate::network_replay::NetworkFdReadAdmission;
        use crate::network_replay::NetworkStreamLeaseId;
        use crate::network_replay::NetworkStreamOwner;
        for policy in [NetworkPolicy::Record, NetworkPolicy::Replay] {
            for queued in [false, true] {
                for nonblocking in [false, true] {
                    let (mut endpoint, mut peer) = UnixStream::pair().unwrap();
                    endpoint.set_nonblocking(nonblocking).unwrap();
                    if queued {
                        peer.write_all(b"retained").unwrap();
                    }
                    let inaccessible = InaccessibleZeroBuffer::new();
                    for (address, expected) in [
                        (0, None),
                        (inaccessible.0 as usize, None),
                        (usize::MAX, Some(Errno::EFAULT)),
                    ] {
                        let mut config = Config::default();
                        config.network_trace.policy = policy;
                        let tid = Tid::from_raw(73);
                        let tool: Detcore<FailingDelegate> = Detcore::new(tid, &config);
                        let thread = tool.init_thread_state(tid, None);
                        thread
                            .add_fd(
                                endpoint.as_raw_fd(),
                                if nonblocking {
                                    OFlag::O_NONBLOCK
                                } else {
                                    OFlag::empty()
                                },
                                FdType::Socket,
                                None,
                            )
                            .unwrap();
                        let owner = NetworkStreamOwner {
                            thread: thread.dettid,
                            mm: thread.mm_id,
                        };
                        let (read, metadata) = {
                            let file_metadata = thread.file_metadata.lock().unwrap();
                            let binding = file_metadata
                                .descriptor_binding(endpoint.as_raw_fd())
                                .unwrap();
                            let read = NetworkFdReadAdmission {
                                publication: NetworkFdPublicationAdmission {
                                    permit: NetworkFdPublicationPermit {
                                        files: file_metadata.files_id,
                                        owner,
                                        lease: NetworkStreamLeaseId::controlled_fixture(1),
                                    },
                                    acknowledged_sequence: 0,
                                    acknowledged_generation: 0,
                                    recovery: None,
                                },
                                fd: endpoint.as_raw_fd(),
                                binding: Some(binding),
                                control: Some(NetworkStreamLeaseId::controlled_fixture(2)),
                                external_grant: None,
                            };
                            let metadata = crate::tool_local::NetworkFdReadMetadata {
                                binding: Some(binding),
                                socket: Some(binding.open_file),
                                nonblocking: Some(nonblocking),
                            };
                            (read, metadata)
                        };
                        let mut guest = ZeroReadGuest {
                            config: &config,
                            thread,
                            calls: 0,
                            admitted: Some(Mutex::new(ZeroReadAdmissionBoundary {
                                read: read.clone(),
                                admission: None,
                                events: Vec::new(),
                                returned: None,
                            })),
                        };
                        assert!(
                            crate::network_replay::backend_fd_table_capability(&config).is_none()
                        );
                        let call = syscalls::Read::new()
                            .with_fd(endpoint.as_raw_fd())
                            .with_buf(AddrMut::from_raw(address))
                            .with_len(0);
                        let result = tool
                            .network_read_from_admission(&mut guest, call, policy, read, metadata)
                            .await;
                        match expected {
                            None => assert_eq!(result.unwrap(), 0),
                            Some(errno) => assert!(
                                matches!(result, Err(Error::Errno(actual)) if actual == errno)
                            ),
                        }
                        assert_eq!(guest.calls, 1);
                        assert_eq!(
                            *guest.thread.as_ref(),
                            0,
                            "network-owned Read bypasses the ordinary delegate"
                        );
                        assert!(guest.thread.original_connect.is_none());
                        assert!(guest.thread.original_file_metadata.is_none());
                        let boundary = guest.admitted.as_ref().unwrap().lock().unwrap();
                        assert_eq!(
                            boundary.events,
                            [
                                "begin-native",
                                "submit",
                                "typed-native-read",
                                "outcome",
                                "retire"
                            ]
                        );
                        assert_eq!(
                            boundary.returned,
                            Some(expected.map_or(0, |e| -i64::from(e.into_raw())))
                        );
                        let mut available = -1;
                        assert_eq!(
                            unsafe {
                                libc::ioctl(endpoint.as_raw_fd(), libc::FIONREAD, &mut available)
                            },
                            0
                        );
                        assert_eq!(available, if queued { 8 } else { 0 });
                    }
                    if !queued {
                        peer.write_all(b"retained").unwrap();
                    }
                    let mut bytes = [0; 8];
                    endpoint.read_exact(&mut bytes).unwrap();
                    assert_eq!(&bytes, b"retained");
                }
            }
        }
    }

    // Corrects the rejected packet's Linux oracle. This preserves its exact
    // UINTPTR_MAX zero-iovec as an explicit EFAULT, and keeps separate valid
    // user-range inaccessible/NULL zero vectors. It does not claim the current
    // network Readv adapter retains the kernel's imported iterator.
    #[test]
    fn zero_readv_native_import_distinguishes_invalid_range_without_consuming_bytes() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        for queued in [false, true] {
            let (mut endpoint, mut peer) = UnixStream::pair().unwrap();
            endpoint.set_nonblocking(true).unwrap();
            if queued {
                peer.write_all(b"retained").unwrap();
            }
            let inaccessible = InaccessibleZeroBuffer::new();
            // Preserve the rejected packet's contrasting receive operation:
            // recv(len=0) may wait or return EAGAIN even though Read is empty.
            let received = unsafe {
                libc::recv(
                    endpoint.as_raw_fd(),
                    std::ptr::null_mut(),
                    0,
                    libc::MSG_DONTWAIT,
                )
            };
            if queued {
                assert_eq!(received, 0);
            } else {
                assert_eq!(received, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EAGAIN)
                );
            }
            for (base, expected) in [(inaccessible.0, 0), (usize::MAX as *mut libc::c_void, -1)] {
                let iov = [
                    libc::iovec {
                        iov_base: std::ptr::null_mut(),
                        iov_len: 0,
                    },
                    libc::iovec {
                        iov_base: base,
                        iov_len: 0,
                    },
                ];
                let result =
                    unsafe { libc::readv(endpoint.as_raw_fd(), iov.as_ptr(), iov.len() as i32) };
                assert_eq!(result, expected);
                if expected < 0 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EFAULT)
                    );
                }
                let mut available = -1;
                assert_eq!(
                    unsafe { libc::ioctl(endpoint.as_raw_fd(), libc::FIONREAD, &mut available) },
                    0
                );
                assert_eq!(available, if queued { 8 } else { 0 });
            }
            if !queued {
                peer.write_all(b"retained").unwrap();
            }
            let mut bytes = [0; 8];
            endpoint.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"retained");
        }
    }
}

#[cfg(test)]
mod epoll_ctl_scheduling_tests;

impl<T: RecordOrReplay> Detcore<T> {
    /// Execute one exact V4 foreground store from the same admitted Call.
    /// The positive bounded scalar slice has no guest-memory helper thread.
    async fn foreground_v4_receive_from_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: crate::tool_global::ScalarReceive,
        admitted: crate::network_replay::NetworkStreamCall,
        mode: crate::network_replay::NetworkEngineMode,
        nonblocking: bool,
        invocation: Option<crate::tool_global::CheckedReadInvocation>,
    ) -> Result<i64, Error> {
        // The authenticated range/flags stay in this live callback; only a
        // blocking Record EAGAIN consumes them, for the same-Call retry.
        let pin = NetworkHostSocketPin {
            call: admitted.id,
            native: admitted.physical_pin_required,
            nonblocking,
        };
        // Production may not probe or select through the old controlled
        // unqualified-profile path. This is the original Call's saved policy,
        // not a new option snapshot on each retry.
        let policy = guest
            .local_global_state()
            .ok_or_else(|| engine_error("V4 receive lost its local policy issuer"))
            .and_then(|global| {
                global
                    .saved_receive_policy(
                        guest.tid(),
                        guest.thread_state(),
                        call,
                        admitted,
                        nonblocking,
                    )
                    .map_err(engine_rpc_error)
            })
            .and_then(|policy| {
                policy.ok_or_else(|| engine_error("V4 receive lacks its original saved policy"))
            });
        let prepared = match (policy, mode) {
            (Err(error), _) => Err(error),
            (Ok(_), crate::network_replay::NetworkEngineMode::Record) => {
                self.prepare_v4_private_probe(guest, &pin).await.map(Some)
            }
            (Ok(_), crate::network_replay::NetworkEngineMode::Replay) => Ok(None),
        };
        self.foreground_v4_receive_after_probe(
            guest,
            call,
            admitted, (mode,
            nonblocking),
            prepared,
            invocation,
        )
        .await
    }

    /// Continue the same admitted Call after canonical Peek/cursor restoration.
    /// Preparation failures use the same physical release and preserve their
    /// primary error; this continuation neither captures nor invents a source.
    pub(crate) async fn foreground_v4_receive_after_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: impl Into<crate::tool_global::ScalarReceive> + Send,
        admitted: crate::network_replay::NetworkStreamCall,
        (mode, nonblocking): (crate::network_replay::NetworkEngineMode, bool),
        prepared_probe: Result<Option<NetworkStreamLeaseId>, Error>,
        mut invocation: Option<crate::tool_global::CheckedReadInvocation>,
    ) -> Result<i64, Error> {
        let call = call.into();
        let pin = NetworkHostSocketPin {
            call: admitted.id,
            native: admitted.physical_pin_required,
            nonblocking,
        };
        let destination = call.destination();
        // A failed retry transfers the Call to its own ordinary release; the
        // common release below must not submit a second one.
        let mut retry_owns_release = false;
        let work = async {
            let policy = guest.local_global_state()
                .ok_or_else(|| engine_error("V4 receive lost its local policy issuer"))?
                .saved_receive_policy(
                    guest.tid(), guest.thread_state(), call, admitted, nonblocking,
                ).map_err(engine_rpc_error)?;
            let deadline = policy.as_ref().and_then(|policy| policy.deadline());
            let interrupt_errno = if deadline.is_some() {
                Errno::EINTR
            } else {
                call.signal_interrupt_errno()
            };
            let mut probe = prepared_probe?;
            let mut interrupted_after_normal = false;
            loop {
                let mut empty = None;
                if mode == crate::network_replay::NetworkEngineMode::Replay {
                    let now = thread_observe_time(guest).await;
                    match network_request(guest, NetworkRequest::ReleaseEligible(now))
                        .await.map_err(engine_rpc_error)?
                    {
                        NetworkReply::ReadyChannels(_) => {}
                        other => return Err(engine_error(format!(
                            "unexpected V4 Replay input-release reply {other:?}"))),
                    }
                }
                // End ALL borrowed Guest/local-global/store authority before a park.
                {
                    let tid = guest.tid();
                    let state = guest.thread_state();
                    let global = guest.local_global_state().ok_or_else(||
                        engine_error("V4 receive backend lacks its actual local global state"))?;
                    let selection = match mode {
                        crate::network_replay::NetworkEngineMode::Record =>
                            global.prepare_private_receive(tid, state, admitted.id,
                                probe.expect("Record owns its actual private probe"),
                                call.selected_maximum(), destination).await.map_err(engine_rpc_error)?,
                        crate::network_replay::NetworkEngineMode::Replay =>
                            global.prepare_replay_receive(tid, state, admitted.id,
                                call.selected_maximum(), destination, nonblocking).await.map_err(engine_rpc_error)?,
                    };
                    match selection {
                    crate::network_replay::ReceiveSelection::Bytes(permit) => {
                        let mut memory = guest.memory();
                        let (raw, full) = permit.copy(tid, state, &mut memory)
                            .map_err(engine_rpc_error)?;
                        let full = full.ok_or_else(|| engine_error(format!(
                            "V4 receive retains incomplete foreground memory outcome: {raw:?}")))?;
                        let returned = match mode {
                            crate::network_replay::NetworkEngineMode::Record => {
                                global.reconcile_foreground_store(full.clone()).await
                                    .map_err(engine_rpc_error)?;
                                let prepared = global.prepare_foreground_receive_publication(&full)
                                    .map_err(engine_rpc_error)?;
                                global.publish_foreground_native_receive(&prepared)
                                    .map_err(engine_rpc_error)?
                            }
                            crate::network_replay::NetworkEngineMode::Replay =>
                                global.commit_replay_receive_store(&full).map_err(engine_rpc_error)?,
                        };
                        return i64::try_from(returned).map_err(engine_error);
                    }
                    crate::network_replay::ReceiveSelection::NoStore(completed) => {
                        match completed.outcome() {
                            crate::network_replay::NoStoreReturn::Eof => return Ok(0),
                            crate::network_replay::NoStoreReturn::WouldBlock if nonblocking =>
                                return Err(Errno::EAGAIN.into()),
                            crate::network_replay::NoStoreReturn::WouldBlock
                                if completed.replay_timed_out() =>
                                return Err(Errno::EAGAIN.into()),
                            crate::network_replay::NoStoreReturn::WouldBlock
                                if mode == crate::network_replay::NetworkEngineMode::Replay =>
                                return Err(engine_error(
                                    "V4 Replay blocking empty attempt completed without a logical wait")),
                            crate::network_replay::NoStoreReturn::WouldBlock => empty = Some(completed),
                        }
                    }
                    crate::network_replay::ReceiveSelection::Wait => {}
                    }
                }
                if let Some(completed) = empty {
                    // Only THIS canonical empty result may end an elapsed
                    // finite wait. A Ready wake still takes a fresh probe first,
                    // so new data/EOF wins even when the deadline has elapsed.
                    if let Some(policy) = &policy {
                        let global = guest.local_global_state().ok_or_else(||
                            engine_error("V4 receive lost its local policy clock"))?;
                        if global.receive_policy_expired(policy).map_err(engine_rpc_error)? {
                            return Err(Errno::EAGAIN.into());
                        }
                    }
                    if interrupted_after_normal {
                        return Err(interrupt_errno.into());
                    }
                    // Blocking Record EAGAIN: the kernel would sleep in this
                    // same read. Use the existing Record observation timer,
                    // then rearm the SAME Call under a new Normal grant. The
                    // timer is an observation-latency bound, not a timeout.
                    let invocation = invocation.as_mut().ok_or_else(|| engine_error(
                        "V4 Record blocking retry lacks its authenticated original invocation"))?;
                    match self.wait_shadow_network_progress(
                        guest,
                        vec![(pin.call, NetworkWaitKind::ReadableAtLeast(1))],
                        deadline,
                        interrupt_errno,
                        NetworkPolicy::Record,
                        None,
                    ).await? {
                        NetworkShadowWaitProgress::DeadlineWithoutWait =>
                            return Err(Errno::EAGAIN.into()),
                        NetworkShadowWaitProgress::InterruptedAfterNormal if deadline.is_some() =>
                            interrupted_after_normal = true,
                        NetworkShadowWaitProgress::InterruptedAfterNormal =>
                            return Err(interrupt_errno.into()),
                        NetworkShadowWaitProgress::Wait(NetworkShadowWaitOutcome::Ready { entered_zero_wait: false }) => {}
                        // Dropping the unconsumed token leaves the Call to the
                        // ordinary release below; the read restarts or fails
                        // with its own interrupt errno, as Linux would.
                        NetworkShadowWaitProgress::Wait(NetworkShadowWaitOutcome::Signaled { entered_zero_wait: false })
                            if deadline.is_some() =>
                            return Err(engine_error("finite receive SignalResume lacks a fresh observation grant")),
                        NetworkShadowWaitProgress::Wait(NetworkShadowWaitOutcome::Signaled { entered_zero_wait: false }) =>
                            return Err(interrupt_errno.into()),
                        _ => return Err(engine_error(
                            "nonzero V4 Record receive acquired a zero-wait receipt")),
                    }
                    let tid = guest.tid();
                    let global = guest.local_global_state().ok_or_else(||
                        engine_error("V4 receive backend lacks its actual local global state"))?;
                    if let Err(failure) = global
                        .resume_private_receive_call(tid, guest.thread_state(), call, invocation, completed)
                        .await
                    {
                        retry_owns_release = true;
                        let failure = global.cleanup_receive_retry_failure(failure).await;
                        let cleanup = match failure.cleanup_diagnostic() {
                            Some(error) => Err(engine_rpc_error(error.clone())),
                            None if failure.released() => Ok(()),
                            None => Err(engine_error("V4 Record retry cleanup retained its Call")),
                        };
                        return finish_shadow_operation(
                            Err(engine_rpc_error(failure.primary().clone())), cleanup);
                    }
                    // Fresh canonical probe on the same OFD and new grant.
                    probe = Some(self.prepare_v4_private_probe(guest, &pin).await?);
                    continue;
                }
                // Wait can arise only from untouched logical Replay selection.
                // Record completes one actual source or returns its exact error.
                if mode != crate::network_replay::NetworkEngineMode::Replay {
                    return Err(engine_error("Record source unexpectedly absent before store"));
                }
                if nonblocking {
                    return Err(engine_error("V4 nonblocking selection unexpectedly requested a wait"));
                }
                // Replay selection has checked the saved low-water-one policy
                // or the unchanged controlled infinite profile.
                // Retain the same logical Call; no source, permit or exclusion is held.
                match self.wait_shadow_network(
                    guest,
                    vec![(pin.call, NetworkWaitKind::ReadableAtLeast(1))],
                    deadline,
                    interrupt_errno,
                    NetworkPolicy::Replay,
                    None,
                ).await? {
                    NetworkShadowWaitOutcome::Ready { entered_zero_wait: false } => {}
                    NetworkShadowWaitOutcome::Signaled { entered_zero_wait: false } if deadline.is_some() =>
                        return Err(engine_error("finite receive SignalResume lacks a fresh observation grant")),
                    NetworkShadowWaitOutcome::Signaled { entered_zero_wait: false } =>
                        return Err(interrupt_errno.into()),
                    _ => return Err(engine_error(
                        "nonzero V4 Replay receive acquired a zero-wait receipt")),
                }
                // A fresh iteration obtains actual new TID/state/root/MM/grant and
                // mapping authority. No prior permit or old epoch crosses this await.
            }
        }.await;
        if retry_owns_release {
            return work;
        }
        // Existing release preserves the primary and retains any unresolved
        // physical/helper/store custody. It cannot report cleanup on a busy Call.
        self.finish_host_stream_call(guest, pin, work).await
    }

    /// Canonical helper PEEK is a private source. No PollState/FIONREAD or V3
    /// CompleteShadowProbe transition may turn it into publication authority.
    async fn prepare_v4_private_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        pin: &NetworkHostSocketPin,
    ) -> Result<NetworkStreamLeaseId, Error> {
        let probe =
            match network_request(guest, NetworkRequest::BeginShadowProbe { call: pin.call })
                .await
                .map_err(engine_rpc_error)?
            {
                NetworkReply::ShadowProbe(probe) => probe,
                other => return Err(engine_error(format!("unexpected V4 probe reply {other:?}"))),
            };
        let lease = probe.lease;
        let observed = self
            .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::ReadPeekOffset)
            .await?;
        let saved = match observed.confirmation {
            NetworkStreamPhysicalResult::PeekOffset(value) => Some(value),
            NetworkStreamPhysicalResult::Errno(errno)
                if errno == libc::ENOPROTOOPT || errno == libc::EOPNOTSUPP =>
            {
                None
            }
            other => return Err(engine_error(format!("V4 cursor query failed: {other:?}"))),
        };
        if saved.is_some_and(|value| value >= 0) {
            self.set_host_shadow_cursor(guest, pin, lease, -1).await?;
        }
        let maximum = probe
            .retained_prefix
            .checked_add(SHADOW_UNIT)
            .ok_or_else(|| engine_error("V4 probe prefix overflow"))?;
        let observed = self
            .native_stream_effect(guest, lease, NetworkStreamPhysicalEffect::Peek { maximum })
            .await?;
        // Only a known completed effect permits cursor restoration. An unknown
        // native RPC remains retained and cannot authorize a second command.
        if let Some(value) = saved.filter(|value| *value >= 0) {
            self.set_host_shadow_cursor(guest, pin, lease, value)
                .await?;
        }
        self.complete_v4_private_probe_observation(lease, observed)
    }

    pub(crate) fn complete_v4_private_probe_observation(
        &self,
        lease: NetworkStreamLeaseId,
        observed: crate::network_runtime::native_peer::Observation,
    ) -> Result<NetworkStreamLeaseId, Error> {
        // This result only selects the existing probe for the local issuer.
        // Canonical no-store authority still comes from its retained joined
        // helper/entry/cursor transaction, never from zero or errno alone.
        match observed.confirmation {
            NetworkStreamPhysicalResult::Peeked { .. } => Ok(lease),
            NetworkStreamPhysicalResult::Errno(errno) if errno == libc::EAGAIN => Ok(lease),
            other => Err(engine_error(format!(
                "V4 positive-source adapter has no qualified terminal/wait outcome: {other:?}"
            ))),
        }
    }
}
