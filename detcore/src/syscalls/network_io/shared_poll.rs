//! Closed shared Poll keeps one original Call and absolute deadline across
//! physical Record retries or immutable-source Replay waits.
use super::*;

impl<T: RecordOrReplay> Detcore<T> {
    fn shared_poll_global<G: Guest<Self>>(
        guest: &G,
    ) -> Result<&crate::tool_global::GlobalState, Error> {
        guest
            .local_global_state()
            .ok_or_else(|| engine_error("shared Poll lost actual local Global"))
    }
    pub(super) fn shared_poll_profile<G: Guest<Self>>(&self, guest: &G) -> bool {
        guest
            .local_global_state()
            .is_some_and(|global| global.shared_mm_attempts_active())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464): Review shared original Poll custody and deadline transitions.
    pub(super) async fn network_shared_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Poll,
    ) -> Result<i64, Error> {
        let policy = guest.config().network_trace.policy;
        let prepared = Self::shared_poll_global(guest)?
            .prepare_shared_poll_input(guest, call.into())
            .await
            .map_err(engine_rpc_error)?;
        // Global's immutable Guest borrow ended with preparation. Only owned
        // original custody crosses the actual backend mutation and true join.
        guest
            .join_followed_observation_timers(prepared.original())
            .await
            .map_err(|error| engine_error(format!("shared Poll input timer join: {error:?}")))?;
        let input = guest
            .capture_original_followed_poll(prepared.original(), prepared.retention())
            .await
            .map_err(|error| engine_error(format!("shared Poll original input: {error:?}")))?;
        let captured = Self::shared_poll_global(guest)?
            .finish_shared_poll_input(guest, prepared, input)
            .map_err(engine_rpc_error)?;
        let read = self
            .begin_network_fd_read(guest, captured.input().fd)
            .await?;
        let observed = {
            let mut table = guest.thread_state().file_metadata.lock().unwrap();
            table.observe_fd_read(&read)
        };
        let observed = match observed {
            Ok(observed)
                if observed.socket.is_some()
                    && observed.binding == read.binding
                    && !self.network_fd_is_capability_probe(guest, captured.input().fd) =>
            {
                observed
            }
            other => {
                let primary = match other {
                    Err(error) => error,
                    _ => engine_error("shared Poll requires its admitted connected TCP descriptor"),
                };
                let released = self
                    .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                    .await;
                return finish_shadow_operation(Err(primary), released);
            }
        };
        let invocation = match Self::shared_poll_global(guest)?
            .begin_shared_poll_call(guest, captured, read, observed)
            .await
        {
            Ok(invocation) => invocation,
            Err(failure) => {
                let failure = Self::shared_poll_global(guest)?
                    .cleanup_receive_admission_failure(failure)
                    .await;
                let primary = engine_rpc_error(failure.primary().clone());
                let cleanup = failure
                    .cleanup_diagnostic()
                    .map(|error| Err(engine_rpc_error(error.clone())))
                    .unwrap_or(Ok(()));
                return finish_shadow_operation(Err(primary), cleanup);
            }
        };
        loop {
            let prepared = Self::shared_poll_global(guest)?
                .prepare_shared_poll_attempt(guest, &invocation)
                .await
                .map_err(engine_rpc_error)?;
            // Joining a peer's positively marked physical timer issues no
            // source, readiness, or Normal scheduling grant.
            guest
                .join_followed_observation_timers(invocation.original())
                .await
                .map_err(|error| {
                    engine_error(format!("shared Poll output timer join: {error:?}"))
                })?;
            if let Some(count) = Self::shared_poll_global(guest)?
                .store_shared_poll_attempt(guest, &invocation, &prepared)
                .map_err(engine_rpc_error)?
            {
                if policy == NetworkPolicy::Record {
                    self.shadow_ack(
                        guest,
                        NetworkRequest::NativeReleaseStreamCall {
                            call: invocation.call(),
                        },
                    )
                    .await?;
                }
                return Ok(count);
            }
            let interests = Self::shared_poll_global(guest)?
                .suspend_prepared_shared_poll(guest, &invocation, prepared)
                .await
                .map_err(engine_rpc_error)?;
            self.wait_shared_poll(guest, &invocation, interests, call.signal_interrupt_errno())
                .await?;
            Self::shared_poll_global(guest)?
                .resume_shared_poll(guest.tid(), guest.thread_state(), &invocation)
                .await
                .map_err(engine_rpc_error)?;
        }
    }

    async fn wait_shared_poll_deadline<G: Guest<Self>>(
        &self,
        guest: &mut G,
        invocation: &crate::tool_global::SharedPollInvocation,
        interests: Vec<NetworkWaitKind>,
        interrupt_errno: Errno,
    ) -> Result<(), Error> {
        let mut request = Resources::new(guest.thread_state().dettid);
        request.insert(
            ResourceID::NetworkCallWaitSet {
                interests: interests
                    .into_iter()
                    .map(|kind| (invocation.call(), kind))
                    .collect(),
                deadline: Some(invocation.deadline()),
                zero_wait: None,
            },
            Permission::R,
        );
        request.set_signal_interrupt_errno(interrupt_errno);
        if matches!(
            resource_request(guest, request).await,
            ResumeStatus::Signaled(_)
        ) {
            return Err(engine_error("shared Poll signal completion is unsupported"));
        }
        Ok(())
    }

    async fn wait_shared_poll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        invocation: &crate::tool_global::SharedPollInvocation,
        interests: Vec<NetworkWaitKind>,
        interrupt_errno: Errno,
    ) -> Result<(), Error> {
        let dettid = guest.thread_state().dettid;
        match guest.config().network_trace.policy {
            NetworkPolicy::Replay => {
                self.wait_shared_poll_deadline(guest, invocation, interests, interrupt_errno)
                    .await
            }
            NetworkPolicy::Record => {
                let now = thread_observe_time(guest).await;
                if now >= invocation.deadline() {
                    // A Pending decision can cross its deadline before parking.
                    // Use the original authenticated logical wait to obtain the
                    // real next Normal grant, then take a fresh final scan.
                    // Do not manufacture a zero-duration backend completion.
                    return self
                        .wait_shared_poll_deadline(guest, invocation, interests, interrupt_errno)
                        .await;
                }
                let remaining =
                    Duration::from_nanos(invocation.deadline().as_nanos() - now.as_nanos())
                        .min(Duration::from_millis(1));
                let operation = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
                let mut begin = Resources::new(dettid);
                begin.insert(
                    ResourceID::BlockingNetworkCapture(operation),
                    Permission::RW,
                );
                begin.set_signal_interrupt_errno(interrupt_errno);
                begin.fyi("shared Poll observation timer");
                if matches!(
                    resource_request(guest, begin).await,
                    ResumeStatus::Signaled(_)
                ) {
                    return Err(engine_error(
                        "shared Poll signal before physical timer is unsupported",
                    ));
                }
                let physical = guest
                    .inject_poll_observation_timer(invocation.original(), remaining)
                    .await;
                // Pay the original continuation first even when the actual
                // timer returned an error; no other resource request intervenes.
                let mut continuation = Resources::new(dettid);
                continuation.insert(
                    ResourceID::BlockedExternalContinue(operation),
                    Permission::RW,
                );
                continuation.set_signal_interrupt_errno(interrupt_errno);
                continuation.fyi("shared Poll observation timer");
                let resumed = resource_request(guest, continuation).await;
                physical?;
                if matches!(resumed, ResumeStatus::Signaled(_)) {
                    return Err(engine_error(
                        "shared Poll signal after physical timer is unsupported",
                    ));
                }
                Ok(())
            }
            _ => Err(engine_error("shared Poll wait changed closed policy")),
        }
    }
}
