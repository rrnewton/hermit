//! Shared scalar Receive preserves the original Call and absolute deadline.
use super::*;
use crate::network_replay::shared_waits::SharedNoStoreResult;
use crate::tool_global::SharedReceiveInvocation;
use crate::tool_global::SharedReceivePreparation;
use crate::tool_global::SharedRecordReceiveEffect;

impl<T: RecordOrReplay> Detcore<T> {
    fn shared_receive_global<G: Guest<Self>>(
        guest: &G,
    ) -> Result<&crate::tool_global::GlobalState, Error> {
        guest
            .local_global_state()
            .ok_or_else(|| engine_error("shared Receive lost actual local Global"))
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464): Review shared original scalar Receive, held store, Drain and unchanged deadline.
    pub(super) async fn network_shared_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: crate::tool_global::ScalarReceive,
        mode: crate::network_replay::NetworkEngineMode,
        read: crate::network_replay::NetworkFdReadAdmission,
        metadata: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<i64, Error> {
        let policy = guest.config().network_trace.policy;
        if !matches!(
            (policy, mode),
            (
                NetworkPolicy::Record,
                crate::network_replay::NetworkEngineMode::Record
            ) | (
                NetworkPolicy::Replay,
                crate::network_replay::NetworkEngineMode::Replay
            )
        ) {
            let released = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(
                Err(engine_error("shared Receive changed admitted mode")),
                released,
            );
        }
        if let Err(error) = guest.join_followed_observation_timers(call.syscall()).await {
            let released = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(
                Err(engine_error(format!(
                    "shared Receive entry timer join: {error:?}"
                ))),
                released,
            );
        }
        let admitted = match mode {
            crate::network_replay::NetworkEngineMode::Record => {
                Self::shared_receive_global(guest)?
                    .begin_shared_record_receive(guest, call, read, metadata)
                    .await
            }
            crate::network_replay::NetworkEngineMode::Replay => {
                Self::shared_receive_global(guest)?
                    .begin_shared_replay_receive(guest, call, read, metadata)
                    .await
            }
        };
        let invocation = match admitted {
            Ok(invocation) => invocation,
            Err(failure) => {
                let failure = Self::shared_receive_global(guest)?
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
        let mut restored = false;
        loop {
            let interests = match mode {
                crate::network_replay::NetworkEngineMode::Record => {
                    let prepared = Self::shared_receive_global(guest)?
                        .prepare_shared_record_receive(
                            guest.tid(),
                            guest.thread_state(),
                            &invocation,
                        )
                        .await
                        .map_err(engine_rpc_error)?;
                    if prepared.pending() {
                        Self::shared_receive_global(guest)?
                            .suspend_shared_receive(
                                guest.tid(),
                                guest.thread_state(),
                                &invocation,
                                Some(prepared.origin()),
                            )
                            .await
                            .map_err(engine_rpc_error)?
                    } else {
                        guest
                            .join_followed_observation_timers(call.syscall())
                            .await
                            .map_err(|e| {
                                engine_error(format!("shared Receive output timer join: {e:?}"))
                            })?;
                        let effect = Self::shared_receive_global(guest)?
                            .store_shared_record_receive(guest, &invocation, prepared, restored)
                            .map_err(engine_rpc_error)?;
                        let result = match effect {
                            SharedRecordReceiveEffect::Stored(stored) => {
                                let count = Self::shared_receive_global(guest)?
                                    .drain_shared_record_receive(
                                        guest.tid(),
                                        guest.thread_state(),
                                        &invocation,
                                        stored,
                                    )
                                    .await
                                    .map_err(engine_rpc_error)?;
                                Ok(count as i64)
                            }
                            SharedRecordReceiveEffect::NoStore(empty) => {
                                Self::shared_receive_empty_result(empty, invocation.policy())?
                            }
                        };
                        let released = self
                            .shadow_ack(
                                guest,
                                NetworkRequest::NativeReleaseStreamCall {
                                    call: invocation.call(),
                                },
                            )
                            .await;
                        return finish_shadow_operation(result, released);
                    }
                }
                crate::network_replay::NetworkEngineMode::Replay => {
                    let prepared = Self::shared_receive_global(guest)?
                        .prepare_shared_replay_receive(
                            guest.tid(),
                            guest.thread_state(),
                            &invocation,
                        )
                        .await
                        .map_err(engine_rpc_error)?;
                    match prepared {
                        SharedReceivePreparation::Store(prepared) => {
                            guest
                                .join_followed_observation_timers(call.syscall())
                                .await
                                .map_err(|e| {
                                    engine_error(format!(
                                        "shared Replay Receive store timer join: {e:?}"
                                    ))
                                })?;
                            return Self::shared_receive_global(guest)?
                                .store_shared_replay_receive(guest, &invocation, prepared, restored)
                                .map(|count| count as i64)
                                .map_err(engine_rpc_error);
                        }
                        SharedReceivePreparation::NoStore(prepared) => {
                            guest
                                .join_followed_observation_timers(call.syscall())
                                .await
                                .map_err(|e| {
                                    engine_error(format!(
                                        "shared Replay Receive no-store timer join: {e:?}"
                                    ))
                                })?;
                            let empty = Self::shared_receive_global(guest)?
                                .complete_shared_replay_receive_no_store(
                                    guest,
                                    &invocation,
                                    *prepared,
                                    restored,
                                )
                                .map_err(engine_rpc_error)?;
                            return Self::shared_receive_empty_result(empty, invocation.policy())?;
                        }
                        SharedReceivePreparation::Wait => Self::shared_receive_global(guest)?
                            .suspend_shared_receive(
                                guest.tid(),
                                guest.thread_state(),
                                &invocation,
                                None,
                            )
                            .await
                            .map_err(engine_rpc_error)?,
                    }
                }
            };
            // This flag selects the R API only. The actual backend validates
            // its original/restored stop again before every output callback.
            restored |= self
                .wait_shared_receive(guest, &invocation, interests, call.signal_interrupt_errno())
                .await?;
            Self::shared_receive_global(guest)?
                .resume_shared_receive(guest.tid(), guest.thread_state(), &invocation)
                .await
                .map_err(engine_rpc_error)?;
        }
    }

    fn shared_receive_empty_result(
        empty: SharedNoStoreResult,
        policy: &crate::tool_global::SavedReceivePolicy,
    ) -> Result<Result<i64, Error>, Error> {
        match empty {
            SharedNoStoreResult::Eof => Ok(Ok(0)),
            SharedNoStoreResult::WouldBlock { timed_out }
                if policy.nonblocking() || (timed_out && policy.deadline().is_some()) =>
            {
                Ok(Err(Errno::EAGAIN.into()))
            }
            SharedNoStoreResult::WouldBlock { .. } => Err(engine_error(
                "shared blocking Receive lacks exact timeout result",
            )),
        }
    }

    async fn wait_shared_receive_deadline<G: Guest<Self>>(
        &self,
        guest: &mut G,
        invocation: &SharedReceiveInvocation,
        interests: Vec<NetworkWaitKind>,
        interrupt_errno: Errno,
    ) -> Result<bool, Error> {
        let mut request = Resources::new(guest.thread_state().dettid);
        request.insert(
            ResourceID::NetworkCallWaitSet {
                interests: interests
                    .into_iter()
                    .map(|kind| (invocation.call(), kind))
                    .collect(),
                deadline: invocation.policy().deadline(),
                zero_wait: None,
            },
            Permission::R,
        );
        request.set_signal_interrupt_errno(interrupt_errno);
        if matches!(
            resource_request(guest, request).await,
            ResumeStatus::Signaled(_)
        ) {
            return Err(engine_error(
                "shared Receive signal completion is unsupported",
            ));
        }
        Ok(false)
    }

    async fn wait_shared_receive<G: Guest<Self>>(
        &self,
        guest: &mut G,
        invocation: &SharedReceiveInvocation,
        interests: Vec<NetworkWaitKind>,
        interrupt_errno: Errno,
    ) -> Result<bool, Error> {
        match guest.config().network_trace.policy {
            NetworkPolicy::Replay => {
                self.wait_shared_receive_deadline(guest, invocation, interests, interrupt_errno)
                    .await
            }
            NetworkPolicy::Record => {
                let now = thread_observe_time(guest).await;
                if invocation
                    .policy()
                    .deadline()
                    .is_some_and(|deadline| now >= deadline)
                {
                    return self
                        .wait_shared_receive_deadline(guest, invocation, interests, interrupt_errno)
                        .await;
                }
                let duration = invocation
                    .policy()
                    .deadline()
                    .map(|deadline| {
                        Duration::from_nanos(deadline.as_nanos() - now.as_nanos())
                            .min(Duration::from_millis(1))
                    })
                    .unwrap_or(Duration::from_millis(1));
                let dettid = guest.thread_state().dettid;
                let operation = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
                let mut begin = Resources::new(dettid);
                begin.insert(
                    ResourceID::BlockingNetworkCapture(operation),
                    Permission::RW,
                );
                begin.set_signal_interrupt_errno(interrupt_errno);
                begin.fyi("shared Receive observation timer");
                if matches!(
                    resource_request(guest, begin).await,
                    ResumeStatus::Signaled(_)
                ) {
                    return Err(engine_error(
                        "shared Receive signal before physical timer is unsupported",
                    ));
                }
                let raw = invocation.policy().raw();
                let physical = guest
                    .inject_receive_observation_timer(Syscall::from_raw(raw.0, raw.1), duration)
                    .await;
                // This continuation is first, including after actual timer failure.
                let mut continuation = Resources::new(dettid);
                continuation.insert(
                    ResourceID::BlockedExternalContinue(operation),
                    Permission::RW,
                );
                continuation.set_signal_interrupt_errno(interrupt_errno);
                continuation.fyi("shared Receive observation timer");
                let resumed = resource_request(guest, continuation).await;
                physical?;
                if matches!(resumed, ResumeStatus::Signaled(_)) {
                    return Err(engine_error(
                        "shared Receive signal after physical timer is unsupported",
                    ));
                }
                Ok(true)
            }
            _ => Err(engine_error("shared Receive wait changed closed policy")),
        }
    }
}
