//! Closed shared original Poll admission. Output and retry retain this Call.
use super::*;
use crate::network_replay::shared_waits::OriginalPollIntent;
use crate::network_replay::shared_waits::SharedWaitIntent;
use crate::network_runtime::SharedForegroundLineage;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

mod input;
mod observation;
mod output;
pub(crate) use input::CapturedSharedPollInput;
pub(crate) use output::SharedPollStoreAttempt;

fn internal(error: impl std::fmt::Display) -> NetworkRpcError {
    NetworkRpcError::internal(error.to_string())
}

pub(crate) struct SharedPollInvocation {
    call: crate::network_replay::NetworkStreamCall,
    captured: CapturedSharedPollInput,
    // Retain the original intent for the whole invocation, including Call retirement.
    _intent: Arc<OriginalPollIntent>,
}
impl SharedPollInvocation {
    pub(crate) fn call(&self) -> crate::network_replay::NetworkStreamCallId {
        self.call.id
    }
    pub(crate) fn deadline(&self) -> LogicalTime {
        self.captured.deadline
    }
    pub(crate) fn original(&self) -> reverie::syscalls::Syscall {
        self.captured.original()
    }
}

impl GlobalState {
    /// The original array was already read by R's held native capture. This
    /// transaction authenticates the corresponding FD reader and transfers it
    /// to one existing Call before any native acquisition or replay operation.
    pub(crate) async fn begin_shared_poll_call<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        captured: CapturedSharedPollInput,
        read: crate::network_replay::NetworkFdReadAdmission,
        expected: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<SharedPollInvocation, Box<ReceiveAdmissionFailure>> {
        let owner = captured.custody.owner;
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let result = async {
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| internal("shared Poll lost runtime"))?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| internal("shared Poll lost engine"))?;
            let mode = match self.cfg.network_trace.policy {
                NetworkPolicy::Record => crate::network_replay::NetworkEngineMode::Record,
                NetworkPolicy::Replay => crate::network_replay::NetworkEngineMode::Replay,
                _ => return Err(internal("shared Poll requires closed Record/Replay policy")),
            };
            let state = guest.thread_state();
            let root = captured.custody.root.clone();
            let epoch = {
                let scheduler = self.sched.lock().unwrap();
                runtime
                    .with_shared_foreground_lineage(owner, |lineage| {
                        let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                        self.check_native_source_task(guest.tid(), state, grant.root())
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                        if !Arc::ptr_eq(grant.root(), &root)
                            || grant.epoch() < captured.custody.epoch
                            || state.dettid != owner.thread
                            || state.mm_id != owner.mm
                        {
                            return Err(std::io::Error::other(
                                "shared Poll changed captured task/MM/root",
                            ));
                        }
                        Ok(grant.epoch())
                    })
                    .map_err(internal)?
            };
            let prefix = runtime
                .join_shared_foreground_prefix(root.clone(), engine, None)
                .await
                .map_err(internal)?;
            let (call, intent, submission) = {
                let scheduler = self.sched.lock().unwrap();
                runtime
                    .with_shared_foreground_lineage(owner, |lineage| {
                        let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                        self.check_native_source_task(guest.tid(), state, grant.root())
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                        if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                            return Err(std::io::Error::other(
                                "shared Poll crossed FD admission grant",
                            ));
                        }
                        let _memory = state.memory_metadata.lock().unwrap();
                        let mut metadata = state.file_metadata.lock().unwrap();
                        let actual = metadata
                            .observe_fd_read(&read)
                            .map_err(std::io::Error::other)?;
                        let binding = read
                            .binding
                            .ok_or_else(|| std::io::Error::other("shared Poll lost FD binding"))?;
                        let control = read
                            .control
                            .ok_or_else(|| std::io::Error::other("shared Poll lost FD control"))?;
                        if actual != expected
                            || actual.binding != Some(binding)
                            || actual.socket != Some(binding.open_file)
                            || read.fd != captured.input.fd
                            || read.publication.permit.files != root.files()
                        {
                            return Err(std::io::Error::other(
                                "shared Poll changed original FD/OFD",
                            ));
                        }
                        let mut engine = engine.lock().unwrap();
                        if !engine.uses_shared_mm_attempts() || engine.mode() != mode {
                            return Err(std::io::Error::other("shared Poll changed engine policy"));
                        }
                        engine
                            .validate_fd_metadata(
                                owner,
                                root.files(),
                                &state.file_metadata,
                                &metadata,
                            )
                            .map_err(std::io::Error::other)?;
                        let identity = if mode == crate::network_replay::NetworkEngineMode::Record {
                            Some(
                                engine
                                    .native_stream_capture_identity(
                                        owner,
                                        &read,
                                        &state.file_metadata,
                                        &metadata,
                                    )
                                    .map_err(std::io::Error::other)?
                                    .ok_or_else(|| {
                                        std::io::Error::other(
                                            "shared Poll lacks original file identity",
                                        )
                                    })?,
                            )
                        } else {
                            None
                        };
                        runtime.with_shared_attempt_prefix(
                            &prefix,
                            &mut engine,
                            |engine, admission| {
                                engine
                                    .preflight_shared_wait_begin(&read, &grant, admission)
                                    .map_err(std::io::Error::other)?;
                                let now = self.global_time.lock().unwrap().as_nanos();
                                if now < captured.custody.started {
                                    return Err(std::io::Error::other(
                                        "shared Poll logical start moved backward",
                                    ));
                                }
                                let intent = Arc::new(
                                    OriginalPollIntent::new(
                                        captured.custody.raw,
                                        vec![(binding, captured.input.events)],
                                        captured.custody.started,
                                        Some(captured.deadline),
                                    )
                                    .map_err(std::io::Error::other)?,
                                );
                                let call = engine
                                    .begin_native_stream_call_from_read(owner, read.clone())
                                    .map_err(std::io::Error::other)?;
                                custody = ReceiveAdmissionCustody::RetainedCall(
                                    RetainedReceiveAdmission {
                                        call: call.id,
                                        stage: if identity.is_some() {
                                            ReceiveAdmissionStage::Capture
                                        } else {
                                            ReceiveAdmissionStage::Replay(control)
                                        },
                                    },
                                );
                                if identity.is_none() {
                                    engine.finish_socket_control(owner, control,
                                crate::network_replay::NetworkSocketControlFinish::Unchanged)
                                .map_err(std::io::Error::other)?;
                                }
                                engine
                                    .attach_shared_wait_call(
                                        call.id,
                                        binding,
                                        SharedWaitIntent::Poll(intent.clone()),
                                        &grant,
                                        admission,
                                        now,
                                    )
                                    .map_err(std::io::Error::other)?;
                                let submission = identity
                                    .map(|identity| {
                                        engine
                                            .prepare_shared_record_capture(
                                                call.id, identity, &grant, admission,
                                            )
                                            .map_err(std::io::Error::other)
                                    })
                                    .transpose()?;
                                Ok((call, intent, submission))
                            },
                        )
                    })
                    .map_err(internal)?
            };
            if let Some(submission) = submission {
                let recovery = self
                    .native_capture_recovery()
                    .ok_or_else(|| internal("shared Poll lost Record recovery owner"))?;
                let joined = runtime
                    .capture_shared_wait(submission, recovery)
                    .await
                    .map_err(internal)?;
                if joined.outcome() != crate::network_replay::NetworkStreamPinOutcome::Acquired {
                    return Err(internal(format!(
                        "shared Poll actual capture: {:?}",
                        joined.outcome()
                    )));
                }
                let scheduler = self.sched.lock().unwrap();
                runtime
                    .with_shared_foreground_lineage(owner, |lineage| {
                        let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                        self.check_native_source_task(guest.tid(), state, grant.root())
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                        if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                            return Err(std::io::Error::other(
                                "shared Poll capture crossed original grant",
                            ));
                        }
                        let _memory = state.memory_metadata.lock().unwrap();
                        let metadata = state.file_metadata.lock().unwrap();
                        let mut engine = engine.lock().unwrap();
                        engine
                            .validate_fd_metadata(
                                owner,
                                root.files(),
                                &state.file_metadata,
                                &metadata,
                            )
                            .map_err(std::io::Error::other)?;
                        runtime.with_shared_capture_completion(
                            &joined,
                            &mut engine,
                            |engine, confirmed| {
                                let actual = engine
                                    .complete_shared_record_capture(confirmed, &grant)
                                    .map_err(std::io::Error::other)?;
                                assert_eq!(
                                    actual.id, call.id,
                                    "typed capture retains the original Poll Call"
                                );
                                Ok(())
                            },
                        )
                    })
                    .map_err(internal)?;
            }
            Ok(SharedPollInvocation {
                call,
                captured,
                _intent: intent,
            })
        }
        .await;
        match result {
            Ok(invocation) => {
                self.finish_local_receive_release();
                Ok(invocation)
            }
            Err(primary) => {
                let failure = self.receive_admission_failure(owner, primary, custody);
                if matches!(failure.custody, ReceiveAdmissionCustody::RetainedCall(_)) {
                    Err(self.cleanup_receive_admission_failure(failure).await)
                } else {
                    Err(failure)
                }
            }
        }
    }
}
