//! Scalar Read classification and transfer through the existing FD/Call owners.
//! Queued requests own no table. Every subsequent wait discards the preview;
//! the final admitted description is the one used by the selected operation.
use detcore_model::network_trace::NetworkPolicy;

use super::*;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkFdReadBegin;
use crate::network_replay::NetworkStreamOwner;
use crate::resources::ExternalOpId;
use crate::syscalls::helpers::IOAction;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::ioaction_for_description;
use crate::syscalls::helpers::record_retry_event;
use crate::tool_local::NetworkFdReadMetadata;

struct ReadInput {
    read: NetworkFdReadAdmission,
    descriptor: Option<DetFd>,
    metadata: NetworkFdReadMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadWait {
    Current,
    Poll,
    External,
    Network,
}

fn read_protocol(detail: impl std::fmt::Display) -> Error {
    Error::Tool(anyhow::anyhow!("scalar Read ownership: {detail}"))
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The scalar V4 receive shares Read's existing FD admission and Call
    /// lifetime, but never changes the original syscall or its guest capacity.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464): genuine scalar Recvfrom entry.
    pub(crate) async fn handle_owned_recvfrom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Recvfrom,
    ) -> Result<i64, Error> {
        let result = async {
            let call = crate::tool_global::ScalarReceive::from_syscall(call.into())?;
            // recv(len=0) can wait/EAGAIN. It is not Read's zero-length path.
            if call.capacity() == 0 {
                return Err(read_protocol("V4 zero-capacity Recvfrom remains unsupported"));
            }
            let input = self.acquire_ordinary_read(guest, call.fd()).await?;
            let mode = guest.local_global_state().and_then(|global| global.native_receive_mode());
            let selected = async {
                if !matches!(guest.config().network_trace.policy,
                    NetworkPolicy::Record | NetworkPolicy::Replay)
                    || input.metadata.socket.is_none()
                    || input.descriptor.as_ref().is_none_or(|fd|
                        fd.is_local_socket_pair() || fd.is_network_capability_probe())
                {
                    return Err(read_protocol("V4 Recvfrom changed its admitted external socket"));
                }
                let mode = mode.ok_or_else(|| read_protocol("V4 Recvfrom lost its native receive engine"))?;
                if !self.owned_network_read_uses_shadow(guest, input.metadata.socket.unwrap()).await? {
                    return Err(read_protocol("V4 Recvfrom requires its admitted stream state"));
                }
                Ok(mode)
            }.await;
            let mode = match selected {
                Ok(mode) => mode,
                Err(primary) => {
                    self.release_read_input(guest, input.read).await?;
                    return Err(primary);
                }
            };
            self.network_scalar_receive_from_admission(guest, call, mode, input.read, input.metadata)
                .await
        }.await;
        self.finish_original_invocation(guest, result).await
    }

    async fn observe_read_input<G: Guest<Self>>(
        &self,
        guest: &mut G,
        read: NetworkFdReadAdmission,
    ) -> Result<ReadInput, Error> {
        let observed = {
            let actual = guest.thread_state().file_metadata.clone();
            let mut metadata = actual.lock().unwrap();
            metadata.observe_fd_read(&read).and_then(|observed| {
                metadata
                    .observe_read_descriptor(&read)
                    .map(|descriptor| (observed, descriptor))
            })
        };
        match observed {
            Ok((metadata, descriptor)) => Ok(ReadInput {
                read,
                descriptor,
                metadata,
            }),
            Err(error) => {
                self.release_read_input(guest, read).await?;
                Err(error)
            }
        }
    }

    async fn acquire_ordinary_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<ReadInput, Error> {
        loop {
            self.publish_network_fd_installations(guest).await?;
            let files = guest.thread_state().file_metadata.lock().unwrap().files_id;
            match network_request(guest, NetworkRequest::BeginOrdinaryFdRead { files, fd })
                .await
                .map_err(|error| read_protocol(error.into_error()))?
            {
                NetworkReply::FdRead(NetworkFdReadBegin::Admitted(read)) => {
                    let read = *read;
                    return self.observe_read_input(guest, read).await;
                }
                NetworkReply::FdRead(NetworkFdReadBegin::Recover) => {}
                reply => return Err(read_protocol(format!("unexpected admission {reply:?}"))),
            }
        }
    }

    async fn release_read_input<G: Guest<Self>>(
        &self,
        guest: &mut G,
        read: NetworkFdReadAdmission,
    ) -> Result<(), Error> {
        match network_request(guest, NetworkRequest::FinishFdRead { admission: read })
            .await
            .map_err(|error| read_protocol(error.into_error()))?
        {
            NetworkReply::Unit => Ok(()),
            reply => Err(read_protocol(format!("unexpected release {reply:?}"))),
        }
    }

    async fn request_external_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        network: bool,
    ) -> Result<ReadInput, Error> {
        if !guest.config().sequentialize_threads {
            return self.acquire_ordinary_read(guest, call.fd()).await;
        }
        let state = guest.thread_state();
        let operation = ExternalOpId::new(state.dettid, state.stats.syscall_count);
        let mut request = Resources::new(state.dettid);
        request.insert(
            if network {
                ResourceID::BlockingNetworkCapture(operation)
            } else {
                ResourceID::BlockingExternalIO(operation)
            },
            Permission::RW,
        );
        request.fyi(call.name());
        request.fd_read = Some(crate::scheduler::fd_read::FdReadIntent {
            owner: NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            },
            files: state.file_metadata.lock().unwrap().files_id,
            fd: call.fd(),
            operation,
        });
        match fd_read_resource_request(guest, request).await {
            crate::scheduler::parked::ResourceReply::ReadGrant {
                status: ResumeStatus::Normal,
                read,
            } => self.observe_read_input(guest, *read).await,
            // Cancellation/observation is not a file-selection or Read result.
            reply => Err(read_protocol(format!(
                "external Read lost its selected grant {reply:?}"
            ))),
        }
    }

    async fn continue_external_read<G: Guest<Self>>(&self, guest: &mut G, call: syscalls::Read) {
        let state = guest.thread_state();
        let operation = ExternalOpId::new(state.dettid, state.stats.syscall_count);
        let mut request = Resources::new(state.dettid);
        request.insert(
            ResourceID::BlockedExternalContinue(operation),
            Permission::RW,
        );
        request.fyi(call.name());
        resource_request(guest, request).await;
    }

    /// One actual dispatcher for scalar Read. The capability guard lives at
    /// the original dispatch boundary; no syscall family gets an exemption.
    pub(crate) async fn handle_owned_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        let result = self.handle_owned_read_inner(guest, call).await;
        self.finish_original_invocation(guest, result).await
    }

    async fn handle_owned_read_inner<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Read,
    ) -> Result<i64, Error> {
        let mut input = self.acquire_ordinary_read(guest, call.fd()).await?;
        let mut outer_request: Option<(crate::types::FdSlotBinding, Resources)> = None;
        let mut granted = ReadWait::Current;
        let mut poll_attempt = 0;
        let mut total = 0;
        loop {
            let descriptor = input.descriptor.as_ref();
            let policy = guest.config().network_trace.policy;
            let network = matches!(policy, NetworkPolicy::Record | NetworkPolicy::Replay)
                && input.metadata.socket.is_some();
            let timer = descriptor.is_some_and(|fd| fd.procfs_timer_slack_binding().is_some());
            let procfs = descriptor.is_some_and(|fd| fd.procfs_position().is_some());

            // Preserve the existing timer-slack-before-zero precedence and the
            // direct zero Read branch (it bypasses Recorder/Replayer).
            let modeled = timer
                || (call.len() != 0 && procfs)
                || (call.len() != 0 && descriptor.is_some_and(|fd| fd.ty() == FdType::Rng));
            if !network && !timer && call.len() != 0 && descriptor.is_none() {
                self.release_read_input(guest, input.read).await?;
                if matches!(granted, ReadWait::External | ReadWait::Network) {
                    self.continue_external_read(guest, call).await;
                }
                if outer_request.is_some() {
                    resource_release_all(guest).await;
                }
                return if total != 0 {
                    Ok(total)
                } else {
                    Err(Errno::EBADF.into())
                };
            }

            let desired = if network {
                if self
                    .owned_network_read_uses_shadow(guest, input.metadata.socket.unwrap())
                    .await?
                    || policy == NetworkPolicy::Replay
                {
                    ReadWait::Current
                } else {
                    ReadWait::Network
                }
            } else if modeled || call.len() == 0 {
                ReadWait::Current
            } else {
                let fd = descriptor.expect("nonzero ordinary Read checked descriptor");
                match fd.ty() {
                    FdType::Regular
                    | FdType::Memfd
                    | FdType::Pidfd
                    | FdType::Userfaultfd
                    | FdType::Epoll => ReadWait::Current,
                    _ => {
                        let internal = fd.ty() == FdType::Pipe || fd.is_local_socket_pair();
                        let action = ioaction_for_description(call.fd(), fd);
                        if !self.cfg.sequentialize_threads
                            || (self.cfg.recordreplay_modes && !internal)
                            || action == IOAction::Blocking
                        {
                            ReadWait::External
                        } else if action == IOAction::NonblockizeRetry {
                            ReadWait::Poll
                        } else {
                            ReadWait::Current
                        }
                    }
                }
            };

            let requested_outer = if !network && !timer && !procfs && call.len() != 0 {
                descriptor.and_then(|fd| {
                    fd.resource().map(|resource| {
                        let mut request = guest.thread_state().mk_request(resource, Permission::R);
                        if should_tag_sabre_internal_pipe_io(
                            guest.config().discover_live_file_metadata,
                            fd.ty(),
                            fd.physically_nonblocking(),
                            fd.is_nonblocking(),
                        ) {
                            request.fyi(SABRE_INTERNAL_PIPE_IO_FYI);
                        }
                        request
                    })
                })
            } else {
                None
            };
            let changed_outer =
                outer_request.as_ref().map(|(_, request)| request) != requested_outer.as_ref();

            // A queued descriptor may have been replaced or its flags changed.
            // Consume the real old request's continuation, then reclassify in
            // the foreground; no stale FD is injected and no guest errno is
            // invented from that permitted race.
            if matches!(granted, ReadWait::External | ReadWait::Network)
                && (desired != granted || changed_outer)
            {
                self.release_read_input(guest, input.read).await?;
                self.continue_external_read(guest, call).await;
                granted = ReadWait::Current;
                input = self.acquire_ordinary_read(guest, call.fd()).await?;
                continue;
            }
            if outer_request.is_some() && changed_outer {
                // The old resource request did commit. Preserve that event,
                // close its scope, and select the replacement's actual request.
                // Release the short reader before either RPC can suspend.
                self.release_read_input(guest, input.read).await?;
                resource_release_all(guest).await;
                outer_request = None;
                input = self.acquire_ordinary_read(guest, call.fd()).await?;
                continue;
            }
            if let Some(request) = requested_outer {
                let binding = input
                    .read
                    .binding
                    .expect("resource belongs to present descriptor");
                if let Some((previous, granted_request)) = outer_request.as_mut() {
                    // A replacement requiring the exact same resource/FYI is
                    // already covered by that real grant. Bind it to this new
                    // observation without inventing a redundant scheduling turn.
                    debug_assert_eq!(*granted_request, request);
                    *previous = binding;
                } else {
                    outer_request = Some((binding, request.clone()));
                    self.release_read_input(guest, input.read).await?;
                    resource_request(guest, request).await;
                    input = self.acquire_ordinary_read(guest, call.fd()).await?;
                    continue;
                }
            }
            if desired != ReadWait::Current && granted != desired {
                self.release_read_input(guest, input.read).await?;
                match desired {
                    ReadWait::External | ReadWait::Network => {
                        input = self
                            .request_external_read(guest, call, desired == ReadWait::Network)
                            .await?;
                    }
                    ReadWait::Poll => {
                        let mut request = Resources::new(guest.thread_state().dettid);
                        request.insert(ResourceID::InternalIOPolling, Permission::W);
                        request.fyi(call.name());
                        request.poll_attempt = poll_attempt;
                        if matches!(
                            polled_read_request(guest, call, request).await,
                            ResumeStatus::Signaled(_)
                        ) {
                            return if total != 0 {
                                Ok(total)
                            } else {
                                Err(call.signal_interrupt_errno().into())
                            };
                        }
                        input = self.acquire_ordinary_read(guest, call.fd()).await?;
                    }
                    ReadWait::Current => unreachable!(),
                }
                granted = desired;
                continue;
            }

            let fd = input.descriptor.clone();
            let deterministic = !network
                && !modeled
                && call.len() != 0
                && fd.as_ref().is_some_and(|fd| fd.ty() == FdType::Regular)
                && self.cfg.deterministic_io;
            let delegated = call.len() != 0
                && !deterministic
                && (desired != ReadWait::Poll || self.cfg.recordreplay_modes);
            let result = if network {
                self.network_read_from_admission(guest, call, policy, input.read, input.metadata)
                    .await
            } else if modeled {
                let admission = self
                    .begin_original_read_call(guest, call, input.read, None)
                    .await?;
                let fd = fd
                    .as_ref()
                    .expect("modeled Read classified a present description");
                let result = if timer {
                    self.read_selected_timer_slack(guest, fd, call.buf(), call.len())
                        .await
                } else if procfs {

                    async {
                        if fd.procfs_needs_snapshot() {
                            self.initialize_selected_procfs_snapshot(guest, call, fd)
                                .await?;
                        }
                        let bytes = fd
                            .take_procfs(call.len())
                            .ok_or_else(|| read_protocol("selected procfs state disappeared"))?;
                        guest
                            .memory()
                            .write_exact(call.buf().ok_or(Errno::EFAULT)?, &bytes)?;
                        Ok(bytes.len() as i64)
                    }
                    .await
                } else {
                    fd.with_random_device_stream(|offset| {
                        self.fill_random_device_bytes(
                            guest,
                            call.buf().ok_or(Errno::EFAULT)?,
                            call.len(),
                            offset,
                        )
                    })
                    .map(|n| n as i64)
                };
                self.finish_emulated_read_call(guest, admission, result)
                    .await
            } else {
                self.original_read_attempt(guest, call, input.read, delegated)
                    .await
            };
            if matches!(granted, ReadWait::External | ReadWait::Network) {
                self.continue_external_read(guest, call).await;
                granted = ReadWait::Current;
            }
            if let Err(Error::Tool(error)) = &result
                && let Some(ticket) = error.downcast_ref::<reverie::InterruptedSyscall>()
            {
                // Cancellation/ACK has retired only this unentered attempt.
                // Preserve prior partial progress, the original post-hook and
                // the actual pending signal; do not look up the numeric FD again.
                if outer_request.is_some() {
                    resource_release_all(guest).await;
                }
                guest
                    .finish_interrupted_syscall(ticket.clone(), (total != 0).then_some(total))
                    .await?;
                return if total != 0 { Ok(total) } else { result };
            }
            // A real kernel restart return belongs to Linux's handler/restart
            // path. It must reach the original frame after ACK, not start a
            // second numeric-FD Read inside this callback before the signal.
            if matches!(&result, Err(Error::Errno(Errno::EINTR)))
                && !delegated
                && (deterministic || desired == ReadWait::Poll)
            {
                // This is the existing final-EINTR retry policy, after actual
                // completion. Pre-entry control and kernel restart are separate.
                input = self.acquire_ordinary_read(guest, call.fd()).await?;
                continue;
            }
            if desired == ReadWait::Poll && matches!(&result, Err(Error::Errno(Errno::EAGAIN))) {
                poll_attempt += 1;
                record_retry_event(guest, call).await;
                granted = ReadWait::Current;
                input = self.acquire_ordinary_read(guest, call.fd()).await?;
                continue;
            }
            if let (Some(fd), Ok(count)) = (&fd, &result)
                && !network
                && self.cfg.virtualize_metadata
                && (fd.is_sock_diag() || fd.is_netlink_route())
            {
                self.sanitize_selected_sock_diag_segments(
                    guest,
                    fd.is_netlink_route(),
                    &[(call.buf().map_or(0, |address| address.as_raw()), call.len())],
                    *count as usize,
                )?;
            }
            if deterministic {
                match result {
                    Ok(count) => {
                        total += count;
                        if count != 0 && (count as usize) < call.len() {
                            call = call.with_len(call.len() - count as usize).with_buf(
                                AddrMut::from_raw(
                                    call.buf().expect("successful read buffer").as_raw()
                                        + count as usize,
                                ),
                            );
                            input = self.acquire_ordinary_read(guest, call.fd()).await?;
                            continue;
                        }
                        resource_release_all(guest).await;
                        return Ok(total);
                    }
                    Err(error) => {
                        resource_release_all(guest).await;
                        return match error {
                            Error::Errno(_) if total != 0 => Ok(total),
                            error => Err(error),
                        };
                    }
                }
            }
            if !modeled && !network && call.len() != 0 {
                resource_release_all(guest).await;
            }
            return match result {
                Ok(count) => Ok(total + count),
                Err(Error::Errno(_)) if total != 0 => Ok(total),
                other => other,
            };
        }
    }
}
