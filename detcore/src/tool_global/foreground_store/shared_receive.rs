//! Shared scalar receives keep their original callback, saved policy and Call.
//! The old sole-root receive path is unchanged.
use super::*;
use crate::network_replay::SharedReplaySource;
use crate::network_replay::shared_waits::SharedReplayNoStorePlan;
use crate::network_replay::shared_waits::SharedReplayReceivePlan;
use crate::network_replay::shared_waits::SharedWaitIntent;
use crate::network_runtime::NativeSourceInterval;
use crate::network_runtime::SharedForegroundLineage;
use crate::network_runtime::shared_waits::JoinedSharedPrefix;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

fn internal(error: impl std::fmt::Display) -> NetworkRpcError {
    NetworkRpcError::internal(error.to_string())
}

pub(crate) struct SharedReceiveInvocation {
    call: crate::network_replay::NetworkStreamCall,
    policy: Arc<SavedReceivePolicy>,
}
impl SharedReceiveInvocation {
    pub(crate) fn call(&self) -> crate::network_replay::NetworkStreamCallId {
        self.call.id
    }
    pub(crate) fn policy(&self) -> &Arc<SavedReceivePolicy> {
        &self.policy
    }
}

pub(crate) struct PreparedSharedReceiveStore {
    source: Arc<SharedReplaySource>,
    interval: Arc<NativeSourceInterval>,
}
pub(crate) struct PreparedSharedReceiveNoStore {
    plan: SharedReplayNoStorePlan,
    prefix: JoinedSharedPrefix,
}
pub(crate) enum SharedReceivePreparation {
    Store(PreparedSharedReceiveStore),
    NoStore(Box<PreparedSharedReceiveNoStore>),
    Wait,
}

impl GlobalState {
    /// The original Guest inspection is performed before any transfer, then
    /// repeated after the actual worker join under the same Normal grant.
    /// A post-transfer failure returns the existing retained Call custody.
    pub(crate) async fn begin_shared_replay_receive<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        original: ScalarReceive,
        read: crate::network_replay::NetworkFdReadAdmission,
        expected: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<SharedReceiveInvocation, Box<ReceiveAdmissionFailure>> {
        let state = guest.thread_state();
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let result = async {
            let runtime = self.network_runtime.as_ref().ok_or_else(|| internal("shared receive lost runtime"))?;
            let engine = self.network_engine.as_ref().ok_or_else(|| internal("shared receive lost engine"))?;
            if self.cfg.network_trace.policy != NetworkPolicy::Replay || !(1..=512).contains(&original.capacity()) {
                return Err(internal("shared receive requires original bounded Replay capacity"));
            }
            let inspect = || -> Result<(), NetworkRpcError> {
                if original.inspect_original_range::<crate::Detcore<T>, G>(guest).map_err(internal)?
                    != reverie::OriginalReadRangeVerdict::Allowed {
                    return Err(internal("shared original receive range was not admitted"));
                }
                Ok(())
            };
            inspect()?;
            let (root, epoch) = {
                let scheduler = self.sched.lock().unwrap();
                runtime.with_shared_foreground_lineage(owner, |lineage| {
                    let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                    self.check_native_source_task(guest.tid(), state, grant.root()).map_err(|error| std::io::Error::other(error.to_string()))?;
                    Ok((grant.root().clone(), grant.epoch()))
                }).map_err(internal)?
            };
            let prefix = runtime.join_shared_foreground_prefix(root.clone(), engine, None).await.map_err(internal)?;
            let scheduler = self.sched.lock().unwrap();
            runtime.with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler.shared_mm_foreground_observation(owner, lineage).map_err(internal)?;
                    self.check_native_source_task(guest.tid(), state, grant.root())?;
                    if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                        return Err(internal("shared original receive crossed its entry grant"));
                    }
                    inspect()?;
                    let _memory = state.memory_metadata.lock().unwrap();
                    let mut metadata = state.file_metadata.lock().unwrap();
                    let actual = metadata.observe_fd_read(&read).map_err(internal)?;
                    let binding = read.binding.ok_or_else(|| internal("shared receive lost original binding"))?;
                    let control = read.control.ok_or_else(|| internal("shared receive lost original control"))?;
                    let nonblocking = actual.nonblocking.ok_or_else(|| internal("shared receive lost original flags"))?;
                    if actual != expected || original.fd() != read.fd || actual.binding != read.binding
                        || actual.socket != Some(binding.open_file) || read.publication.permit.files != root.files() {
                        return Err(internal("shared receive changed original FD/OFD/options"));
                    }
                    let mut engine = engine.lock().unwrap();
                    if !engine.uses_shared_mm_attempts() || engine.mode() != crate::network_replay::NetworkEngineMode::Replay {
                        return Err(internal("shared receive changed closed Replay policy"));
                    }
                    engine.validate_fd_metadata(owner, root.files(), &state.file_metadata, &metadata).map_err(internal)?;
                    runtime.with_shared_attempt_prefix(&prefix, &mut engine, |engine, admission| {
                        engine.preflight_shared_wait_begin(&read, &grant, admission).map_err(std::io::Error::other)?;
                        let socket = engine.stream_socket_state(binding.open_file).map_err(std::io::Error::other)?
                            .ok_or_else(|| std::io::Error::other("shared receive lost live socket profile"))?;
                        let started = self.global_time.lock().unwrap().as_nanos();
                        let deadline = socket.options.receive_timeout.duration(socket.normalization.hz).map(|duration| {
                            let nanos = u64::try_from(duration.as_nanos()).map_err(std::io::Error::other)?;
                            started.as_nanos().checked_add(nanos)
                                .filter(|end| *end != LogicalTime::INDEFINITE.as_nanos())
                                .map(LogicalTime::from_nanos)
                                .ok_or_else(|| std::io::Error::other("shared original receive deadline overflow"))
                        }).transpose()?;
                        let target = original.capacity().min(usize::try_from(socket.options.receive_low_water.max(1)).map_err(std::io::Error::other)?);
                        let call = engine.begin_native_stream_call_from_read(owner, read.clone()).map_err(std::io::Error::other)?;
                        custody = ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                            call: call.id, stage: ReceiveAdmissionStage::Replay(control),
                        });
                        let policy = Arc::new(SavedReceivePolicy {
                            origin: ReceivePolicyOrigin::SharedFollowed, target, owner, call: call.id,
                            open_file: call.open_file, root: root.clone(), raw: original.into_parts(),
                            nonblocking, started, deadline,
                        });
                        engine.finish_socket_control(owner, control, crate::network_replay::NetworkSocketControlFinish::Unchanged).map_err(std::io::Error::other)?;
                        engine.attach_shared_wait_call(call.id, binding, SharedWaitIntent::Receive(policy.clone()), &grant, admission, started).map_err(std::io::Error::other)?;
                        Ok(SharedReceiveInvocation { call, policy })
                    }).map_err(internal)
                })())
            }).map_err(internal)?
        }.await;
        match result {
            Ok(invocation) => {
                self.finish_local_receive_release();
                Ok(invocation)
            }
            Err(primary) => Err(self.receive_admission_failure(owner, primary, custody)),
        }
    }

    /// Attach the shared Record entry before the first native acquisition. Its
    /// original Call owns all submission, actual capture and cleanup debt.
    pub(crate) async fn begin_shared_record_receive<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        original: ScalarReceive,
        read: crate::network_replay::NetworkFdReadAdmission,
        expected: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<SharedReceiveInvocation, Box<ReceiveAdmissionFailure>> {
        let state = guest.thread_state();
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let result = async {
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| internal("shared Record receive lost runtime"))?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| internal("shared Record receive lost engine"))?;
            let recovery = self
                .native_capture_recovery()
                .ok_or_else(|| internal("shared Record receive lost recovery owner"))?;
            if self.cfg.network_trace.policy != NetworkPolicy::Record
                || !(1..=512).contains(&original.capacity())
            {
                return Err(internal(
                    "shared Record receive requires bounded original capacity",
                ));
            }
            let inspect = || -> Result<(), NetworkRpcError> {
                if original
                    .inspect_original_range::<crate::Detcore<T>, G>(guest)
                    .map_err(internal)?
                    != reverie::OriginalReadRangeVerdict::Allowed
                {
                    return Err(internal(
                        "shared Record original receive range was not admitted",
                    ));
                }
                Ok(())
            };
            inspect()?;
            let (root, epoch) = {
                let scheduler = self.sched.lock().unwrap();
                runtime
                    .with_shared_foreground_lineage(owner, |lineage| {
                        let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                        self.check_native_source_task(guest.tid(), state, grant.root())
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                        Ok((grant.root().clone(), grant.epoch()))
                    })
                    .map_err(internal)?
            };
            let prefix = runtime
                .join_shared_foreground_prefix(root.clone(), engine, None)
                .await
                .map_err(internal)?;
            let (invocation, submission) = {
                let scheduler = self.sched.lock().unwrap();
                runtime
                    .with_shared_foreground_lineage(owner, |lineage| {
                        Ok((|| {
                            let grant = scheduler
                                .shared_mm_foreground_observation(owner, lineage)
                                .map_err(internal)?;
                            self.check_native_source_task(guest.tid(), state, grant.root())?;
                            if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                                return Err(internal(
                                    "shared Record receive crossed original entry grant",
                                ));
                            }
                            inspect()?;
                            let _memory = state.memory_metadata.lock().unwrap();
                            let mut metadata = state.file_metadata.lock().unwrap();
                            let actual = metadata.observe_fd_read(&read).map_err(internal)?;
                            let binding = read
                                .binding
                                .ok_or_else(|| internal("shared Record receive lost binding"))?;
                            let nonblocking = actual
                                .nonblocking
                                .ok_or_else(|| internal("shared Record receive lost flags"))?;
                            if actual != expected
                                || original.fd() != read.fd
                                || actual.binding != read.binding
                                || actual.socket != Some(binding.open_file)
                                || read.publication.permit.files != root.files()
                            {
                                return Err(internal(
                                    "shared Record receive changed original FD/OFD/options",
                                ));
                            }
                            let mut engine = engine.lock().unwrap();
                            if !engine.uses_shared_mm_attempts()
                                || engine.mode() != crate::network_replay::NetworkEngineMode::Record
                            {
                                return Err(internal(
                                    "shared Record receive changed closed policy",
                                ));
                            }
                            let identity = engine
                                .native_stream_capture_identity(
                                    owner,
                                    &read,
                                    &state.file_metadata,
                                    &metadata,
                                )
                                .map_err(internal)?
                                .ok_or_else(|| {
                                    internal(
                                        "shared Record receive lost authenticated original file",
                                    )
                                })?;
                            runtime
                                .with_shared_attempt_prefix(
                                    &prefix,
                                    &mut engine,
                                    |engine, admission| {
                                        engine
                                            .preflight_shared_wait_begin(&read, &grant, admission)
                                            .map_err(std::io::Error::other)?;
                                        let socket = engine
                                            .stream_socket_state(binding.open_file)
                                            .map_err(std::io::Error::other)?
                                            .ok_or_else(|| {
                                                std::io::Error::other(
                                                    "shared Record receive lost socket profile",
                                                )
                                            })?;
                                        let started = self.global_time.lock().unwrap().as_nanos();
                                        let deadline = socket
                                            .options
                                            .receive_timeout
                                            .duration(socket.normalization.hz)
                                            .map(|duration| {
                                                let nanos = u64::try_from(duration.as_nanos())
                                                    .map_err(std::io::Error::other)?;
                                                started
                                                    .as_nanos()
                                                    .checked_add(nanos)
                                                    .filter(|end| {
                                                        *end != LogicalTime::INDEFINITE.as_nanos()
                                                    })
                                                    .map(LogicalTime::from_nanos)
                                                    .ok_or_else(|| {
                                                        std::io::Error::other(
                                                            "shared Record deadline overflow",
                                                        )
                                                    })
                                            })
                                            .transpose()?;
                                        let target = original.capacity().min(
                                            usize::try_from(
                                                socket.options.receive_low_water.max(1),
                                            )
                                            .map_err(std::io::Error::other)?,
                                        );
                                        let call = engine
                                            .begin_native_stream_call_from_read(owner, read.clone())
                                            .map_err(std::io::Error::other)?;
                                        custody = ReceiveAdmissionCustody::RetainedCall(
                                            RetainedReceiveAdmission {
                                                call: call.id,
                                                stage: ReceiveAdmissionStage::Capture,
                                            },
                                        );
                                        let policy = Arc::new(SavedReceivePolicy {
                                            origin: ReceivePolicyOrigin::SharedFollowed,
                                            target,
                                            owner,
                                            call: call.id,
                                            open_file: call.open_file,
                                            root: root.clone(),
                                            raw: original.into_parts(),
                                            nonblocking,
                                            started,
                                            deadline,
                                        });
                                        // This attachment owns the Record cut before pidfd_getfd.
                                        // The original publication/control remain held until
                                        // the typed actual-capture completion commits below.
                                        engine
                                            .attach_shared_wait_call(
                                                call.id,
                                                binding,
                                                SharedWaitIntent::Receive(policy.clone()),
                                                &grant,
                                                admission,
                                                started,
                                            )
                                            .map_err(std::io::Error::other)?;
                                        let submission = engine
                                            .prepare_shared_record_capture(
                                                call.id, identity, &grant, admission,
                                            )
                                            .map_err(std::io::Error::other)?;
                                        Ok((SharedReceiveInvocation { call, policy }, submission))
                                    },
                                )
                                .map_err(internal)
                        })())
                    })
                    .map_err(internal)??
            };
            let joined = runtime
                .capture_shared_wait(submission, recovery)
                .await
                .map_err(internal)?;
            if joined.outcome() != crate::network_replay::NetworkStreamPinOutcome::Acquired {
                return Err(internal(format!(
                    "shared Record capture failed: {:?}",
                    joined.outcome()
                )));
            }
            {
                let scheduler = self.sched.lock().unwrap();
                runtime
                    .with_shared_foreground_lineage(owner, |lineage| {
                        Ok((|| {
                            let grant = scheduler
                                .shared_mm_foreground_observation(owner, lineage)
                                .map_err(internal)?;
                            self.check_native_source_task(guest.tid(), state, grant.root())?;
                            if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                                return Err(internal(
                                    "shared Record capture crossed original Normal grant",
                                ));
                            }
                            inspect()?;
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
                                .map_err(internal)?;
                            runtime
                                .with_shared_capture_completion(
                                    &joined,
                                    &mut engine,
                                    |engine, confirmed| {
                                        let actual = engine
                                            .complete_shared_record_capture(confirmed, &grant)
                                            .map_err(std::io::Error::other)?;
                                        assert_eq!(
                                            actual.id, invocation.call.id,
                                            "typed completion retained its original Call"
                                        );
                                        Ok(())
                                    },
                                )
                                .map_err(internal)
                        })())
                    })
                    .map_err(internal)??;
            }
            Ok(invocation)
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

    fn with_shared_receive_context<T, R>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
        action: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedMmForegroundObservation<'_>,
            &SharedForegroundLineage<'_>,
        ) -> Result<R, NetworkRpcError>,
    ) -> Result<R, NetworkRpcError> {
        let policy = &invocation.policy;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared receive lost runtime"))?;
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(policy.owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(policy.owner, lineage)
                        .map_err(internal)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    if self.cfg.network_trace.policy != NetworkPolicy::Replay
                        || !policy.matches_shared(
                            grant.owner(),
                            invocation.call.id,
                            invocation.call.open_file,
                        )
                        || !Arc::ptr_eq(policy.root(), grant.root())
                    {
                        return Err(internal("shared receive lost original task/policy/root"));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| internal("shared receive lost engine"))?
                        .lock()
                        .unwrap();
                    if !engine.uses_shared_mm_attempts()
                        || engine.mode() != crate::network_replay::NetworkEngineMode::Replay
                    {
                        return Err(internal("shared receive changed engine policy"));
                    }
                    engine
                        .validate_fd_metadata(
                            policy.owner,
                            policy.root.files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(internal)?;
                    action(&mut engine, &grant, lineage)
                })())
            })
            .map_err(internal)?
    }

    pub(crate) async fn prepare_shared_replay_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        invocation: &SharedReceiveInvocation,
    ) -> Result<SharedReceivePreparation, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared receive lost runtime"))?;
        let epoch = self
            .with_shared_receive_context(tid, state, invocation, |_, grant, _| Ok(grant.epoch()))?;
        let prefix = runtime
            .join_shared_foreground_prefix(
                invocation.policy.root.clone(),
                self.network_engine
                    .as_ref()
                    .ok_or_else(|| internal("shared receive lost engine"))?,
                Some(invocation.call.id),
            )
            .await
            .map_err(internal)?;
        self.with_shared_receive_context(tid, state, invocation, |engine, grant, lineage| {
            if grant.epoch() != epoch {
                return Err(internal(
                    "shared selection crossed its original Normal grant",
                ));
            }
            let now = self.global_time.lock().unwrap().as_nanos();
            match engine
                .plan_shared_replay_receive(grant.owner(), invocation.call.id, grant, now)
                .map_err(internal)?
            {
                SharedReplayReceivePlan::Bytes(plan) => {
                    let (source, interval) = runtime
                        .prepare_shared_replay_output(
                            &prefix,
                            lineage,
                            engine,
                            invocation.call.id,
                            |engine, admission| {
                                engine
                                    .reserve_shared_replay_store(&plan, grant, admission)
                                    .map_err(std::io::Error::other)
                            },
                        )
                        .map_err(internal)?;
                    Ok(SharedReceivePreparation::Store(
                        PreparedSharedReceiveStore {
                            source,
                            interval: Arc::new(interval),
                        },
                    ))
                }
                // Consumption needs a fresh backend context check inside its
                // actual held callback, even though it performs no store.
                SharedReplayReceivePlan::NoStore(plan) => Ok(SharedReceivePreparation::NoStore(
                    Box::new(PreparedSharedReceiveNoStore { plan, prefix }),
                )),
                SharedReplayReceivePlan::Wait => Ok(SharedReceivePreparation::Wait),
            }
        })
    }

    /// `restored` selects an API; it supplies no authority. Each backend checks
    /// the exact original context independently, with no fallback or retry.
    /// The actual outcome is retained before the backend releases its hold.
    pub(crate) fn store_shared_replay_receive<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        invocation: &SharedReceiveInvocation,
        prepared: PreparedSharedReceiveStore,
        restored: bool,
    ) -> Result<usize, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared store lost runtime"))?;
        let original =
            reverie::syscalls::Syscall::from_raw(invocation.policy.raw.0, invocation.policy.raw.1);
        if prepared.source.call() != invocation.call.id
            || prepared.source.raw() != invocation.policy.raw
            || !Arc::ptr_eq(prepared.source.root(), invocation.policy.root())
        {
            return Err(internal(
                "shared held store changed selected original callback",
            ));
        }
        let action = |writer: &mut dyn reverie::syscalls::FollowedStore| {
            self.with_shared_receive_context(
                guest.tid(),
                guest.thread_state(),
                invocation,
                |engine, grant, _| {
                    runtime
                        .with_shared_output_interval(
                            &prepared.interval,
                            engine,
                            &prepared.source,
                            |engine| {
                                let outcome = writer.store(prepared.source.bytes());
                                engine
                                    .retain_shared_replay_store_attempt(SharedStoreAttempt {
                                        source: prepared.source.clone(),
                                        outcome,
                                        _interval: Some(prepared.interval.clone()),
                                    })
                                    .map_err(std::io::Error::other)?;
                                engine
                                    .complete_shared_replay_store(&prepared.source, grant)
                                    .map_err(std::io::Error::other)
                            },
                        )
                        .map_err(internal)
                },
            )
        };
        let result = if restored {
            guest.with_restored_followed_store(original, action)
        } else {
            guest.with_followed_store(original, action)
        }
        .map_err(|e| internal(format!("shared held store refused before callback: {e:?}")))?;
        drop(prepared);
        if result.is_ok() {
            self.finish_local_receive_release();
        }
        result
    }
    /// Consumes an exact empty result only while the original/restored backend
    /// context is freshly validated inside the same H commit transaction.
    pub(crate) fn complete_shared_replay_receive_no_store<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        invocation: &SharedReceiveInvocation,
        prepared: PreparedSharedReceiveNoStore,
        restored: bool,
    ) -> Result<crate::network_replay::shared_waits::SharedNoStoreResult, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared no-store lost runtime"))?;
        let original =
            reverie::syscalls::Syscall::from_raw(invocation.policy.raw.0, invocation.policy.raw.1);
        let PreparedSharedReceiveNoStore { plan, prefix } = prepared;
        let action = |context: &mut dyn reverie::syscalls::FollowedStore| {
            self.with_shared_receive_context(
                guest.tid(),
                guest.thread_state(),
                invocation,
                |engine, grant, _| {
                    runtime
                        .with_shared_attempt_prefix(&prefix, engine, |engine, admission| {
                            // No guest payload read/write and no original one-use claim.
                            // This is the actual held backend check, never a supplied bool.
                            context.validate_context().map_err(|error| {
                                std::io::Error::other(format!(
                                    "shared no-store original context: {error:?}"
                                ))
                            })?;
                            let now = self.global_time.lock().unwrap().as_nanos();
                            engine
                                .complete_shared_replay_no_store(plan, grant, admission, now)
                                .map_err(std::io::Error::other)
                        })
                        .map_err(internal)
                },
            )
        };
        let result = if restored {
            guest.with_restored_followed_store(original, action)
        } else {
            guest.with_followed_store(original, action)
        }
        .map_err(|error| internal(format!("shared no-store callback refused: {error:?}")))?;
        if result.is_ok() {
            self.finish_local_receive_release();
        }
        result
    }
}
