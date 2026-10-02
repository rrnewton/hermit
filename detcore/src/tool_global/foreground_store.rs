//! Local foreground copy issuer. This is deliberately absent from NetworkRequest:
//! serialized correlation cannot authorize a write or reconstruct its outcome.
use super::*;
use crate::network_replay::ForegroundStore;
use crate::network_replay::FullStoreCompletion;
use crate::network_replay::ReceiveSelection;
use crate::network_replay::ReplayReceivePlan;
use crate::network_replay::StoreOutcome;

/// Owned only in one live Guest callback. Its constructor performs the actual
/// original-entry inspection; an Allowed enum or serialized tuple cannot mint it.
pub(crate) struct CheckedReadRange {
    raw: (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs),
    tid: Tid,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    files: Arc<Mutex<crate::tool_local::FileMetadata>>,
    open_file: OpenFileId,
    nonblocking: bool,
    epoch: u64,
}
pub(crate) struct CheckedReadInvocation {
    range: CheckedReadRange,
    call: crate::network_replay::NetworkStreamCall,
    failure: Option<NetworkRpcError>,
}

/// Immutable policy of one actual local Read admission. Numeric socket options
/// alone cannot construct this permission to use the finite-timeout path.
#[derive(Debug)]
pub(crate) struct SavedReceivePolicy {
    owner: NetworkStreamOwner,
    call: crate::network_replay::NetworkStreamCallId,
    open_file: OpenFileId,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    raw: (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs),
    nonblocking: bool,
    started: LogicalTime,
    deadline: Option<LogicalTime>,
}

impl PartialEq for SavedReceivePolicy {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for SavedReceivePolicy {}

impl SavedReceivePolicy {
    pub(crate) fn matches(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        open_file: OpenFileId,
    ) -> bool {
        self.owner == owner
            && self.call == call
            && self.open_file == open_file
            && self.root.is_current(owner)
            && self.root.is_sole_initial_root(owner)
    }

    pub(crate) fn deadline(&self) -> Option<LogicalTime> {
        self.deadline
    }

    pub(crate) fn expired(&self, now: LogicalTime) -> bool {
        self.deadline.is_some_and(|end| now >= end)
    }

    pub(crate) fn started(&self) -> LogicalTime {
        self.started
    }

    pub(crate) fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    pub(crate) fn matches_span(&self, maximum: usize, destination: u64) -> bool {
        self.raw.0 == reverie::syscalls::Sysno::read
            && self.raw.1.arg1 as u64 == destination
            && self.raw.1.arg2 == maximum
    }
}
/// Borrowed from this live callback and its currently held scheduler grant.
/// Only the checked blocking issuer below can construct it; a completed empty
/// helper or joined prefix alone grants no authority to rearm the engine.
pub(crate) struct CheckedBlockingReadRetry<'a, 's> {
    invocation: &'a CheckedReadInvocation,
    grant: &'a crate::scheduler::ordinary_fd::OrdinaryFdObservation<'s>,
}
impl CheckedBlockingReadRetry<'_, '_> {
    pub(crate) fn matches(
        &self,
        retry: &crate::network_replay::RecordReceiveRetry,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        open_file: OpenFileId,
    ) -> bool {
        let invocation = self.invocation;
        !invocation.range.nonblocking
            && invocation.failure.is_none()
            && invocation.call.physical_pin_required
            && invocation.call.id == retry.call()
            && invocation.call.open_file == open_file
            && invocation.range.open_file == open_file
            && Arc::ptr_eq(&invocation.range.root, retry.root())
            && invocation.range.root.is_current(retry.owner())
            && invocation.range.root.owner() == retry.owner()
            && invocation.range.epoch == retry.epoch()
            && std::ptr::eq(self.grant, grant)
            && grant.owner() == retry.owner()
            && grant.epoch() > invocation.range.epoch
    }
}
#[derive(Debug)]
pub(crate) struct ReceiveRetryFailure {
    primary: NetworkRpcError,
    cleanup: Option<NetworkRpcError>,
    owner: NetworkStreamOwner,
    call: crate::network_replay::NetworkStreamCallId,
    engine: std::sync::Weak<Mutex<NetworkReplayEngine>>,
    custody: ReceiveRetryCustody,
}
#[derive(Debug)]
enum ReceiveRetryCustody {
    Call,
    ReleaseSubmitted,
    Acknowledgement(crate::network_replay::CompletedStreamCallRelease),
    Released,
}
impl ReceiveRetryFailure {
    pub(crate) fn primary(&self) -> &NetworkRpcError {
        &self.primary
    }
    pub(crate) fn cleanup_diagnostic(&self) -> Option<&NetworkRpcError> {
        self.cleanup.as_ref()
    }
    pub(crate) fn released(&self) -> bool {
        matches!(self.custody, ReceiveRetryCustody::Released)
    }
}

impl CheckedReadRange {
    pub(crate) fn inspect<T: crate::RecordOrReplay, G: reverie::Guest<crate::Detcore<T>>>(
        guest: &G,
        call: reverie::syscalls::Read,
        read: &crate::network_replay::NetworkFdReadAdmission,
        metadata: crate::tool_local::NetworkFdReadMetadata,
    ) -> Result<Self, reverie::Error> {
        use reverie::syscalls::SyscallInfo;
        match guest.inspect_original_read_range(call)? {
            reverie::OriginalReadRangeVerdict::Fault => return Err(Errno::EFAULT.into()),
            reverie::OriginalReadRangeVerdict::Allowed => {}
        }
        let refuse = |text: &str| reverie::Error::Tool(anyhow::anyhow!(text.to_owned()));
        let global = guest
            .local_global_state()
            .ok_or_else(|| refuse("checked Read lost local global state"))?;
        let state = guest.thread_state();
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = global
            .network_runtime
            .as_ref()
            .ok_or_else(|| refuse("checked Read runtime absent"))?;
        let root = runtime
            .foreground_root(owner)
            .map_err(|e| refuse(&e.to_string()))?;
        let actual = state.file_metadata.lock().unwrap().observe_fd_read(read)?;
        if actual != metadata || call.fd() != read.fd || metadata.binding != read.binding {
            return Err(refuse("checked Read changed its admitted descriptor/flags"));
        }
        let open_file = metadata
            .socket
            .ok_or_else(|| refuse("checked Read lost socket identity"))?;
        let nonblocking = metadata
            .nonblocking
            .ok_or_else(|| refuse("checked Read lost original flags"))?;
        let epoch = global
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(owner, &root)
            .map_err(|e| refuse(&e.to_string()))?
            .epoch();
        let checked = Self {
            raw: call.into_parts(),
            tid: guest.tid(),
            root,
            memory: state.memory_metadata.clone(),
            files: state.file_metadata.clone(),
            open_file,
            nonblocking,
            epoch,
        };
        checked
            .check(global, guest.tid(), state, call)
            .map_err(|e| refuse(&e.to_string()))?;
        Ok(checked)
    }

    fn check<T>(
        &self,
        global: &GlobalState,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
    ) -> Result<(), NetworkRpcError> {
        use reverie::syscalls::SyscallInfo;
        let owner = self.root.owner();
        if !global.cfg.sequentialize_threads
            || tid != self.tid
            || tid.as_raw() != self.root.association().process()
            || state.dettid != owner.thread
            || state.mm_id != owner.mm
            || state.detpid != Some(owner.thread)
            || !self.root.is_current(owner)
            || self.raw != read.into_parts()
            || !Arc::ptr_eq(&self.memory, &state.memory_metadata)
            || !Arc::ptr_eq(&self.files, &state.file_metadata)
            || !self.root.matches_memory(&self.memory)
            || !self.root.matches_metadata(&self.files)
            || global
                .registered_exec_mms
                .lock()
                .unwrap()
                .get(&owner.thread)
                != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "checked Read changed its actual invocation/root/MM/arguments",
            ));
        }
        let runtime = global
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("checked Read runtime absent"))?;
        if !Arc::ptr_eq(
            &self.root,
            &runtime
                .foreground_root(owner)
                .map_err(|e| NetworkRpcError::internal(e.to_string()))?,
        ) {
            return Err(NetworkRpcError::internal(
                "checked Read changed registered root",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
impl GlobalState {
    /// Controlled original-range premise, followed by the production issuer.
    /// Used only before the fixture creates its first physical probe.
    pub(super) fn controlled_bind_receive_policy<T>(
        &self,
        invocation: &CheckedReadInvocation,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
    ) -> Result<(), NetworkRpcError> {
        self.bind_saved_receive_policy(&invocation.range, tid, state, read, invocation.call)
    }

    /// Controlled callback/range/flags premise for the actual helper transaction
    /// controls. This is not native backend original-entry evidence.
    pub(super) fn controlled_receive_invocation<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        call: crate::network_replay::NetworkStreamCall,
        nonblocking: bool,
    ) -> CheckedReadInvocation {
        use reverie::syscalls::SyscallInfo;
        let root = self
            .network_runtime
            .as_ref()
            .unwrap()
            .foreground_root(NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            })
            .unwrap();
        let epoch = self
            .sched
            .lock()
            .unwrap()
            .foreground_native_observation(root.owner(), &root)
            .unwrap()
            .epoch();
        let range = CheckedReadRange {
            raw: read.into_parts(),
            tid,
            root,
            memory: state.memory_metadata.clone(),
            files: state.file_metadata.clone(),
            open_file: call.open_file,
            nonblocking,
            epoch,
        };
        self.bind_private_receive_invocation(range, tid, state, read, call)
            .unwrap_or_else(|e| panic!("controlled invocation failed: {:?}", e.primary()))
    }

    /// The actual local scheduler guard remains held through the negative
    /// lower-boundary control; tests cannot fabricate an observation/witness.
    pub(super) fn controlled_with_checked_blocking_retry<T, U>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        invocation: &CheckedReadInvocation,
        check: impl for<'a, 's> FnOnce(CheckedBlockingReadRetry<'a, 's>) -> U,
    ) -> Result<U, NetworkRpcError> {
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(invocation.range.root.owner(), &invocation.range.root)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        let checked = self.checked_blocking_read_retry(tid, state, read, invocation, &grant)?;
        Ok(check(checked))
    }
}

impl GlobalState {
    /// Bind only after the actual original range, FD reader and local Call have
    /// joined, and before the first helper probe or Replay selection. No RPC
    /// carries this authority and no caller supplies the logical start time.
    pub(crate) fn bind_saved_receive_policy<T>(
        &self,
        range: &CheckedReadRange,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        call: crate::network_replay::NetworkStreamCall,
    ) -> Result<(), NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        range.check(self, tid, state, read)?;
        let owner = range.root.owner();
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &range.root)
            .map_err(|e| fail(&e))?;
        if grant.epoch() != range.epoch || call.open_file != range.open_file {
            return Err(NetworkRpcError::internal(
                "receive policy crossed original grant/OFD",
            ));
        }
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("receive policy engine absent"))?
            .lock()
            .unwrap();
        let socket = engine
            .stream_call_socket_state(owner, call.id)
            .map_err(|e| fail(&e))?;
        if socket.options.receive_low_water != 1 {
            return Err(NetworkRpcError::internal(
                "saved receive policy requires low-water one",
            ));
        }
        let started = self.global_time.lock().unwrap().as_nanos();
        let deadline = socket
            .options
            .receive_timeout
            .duration(socket.normalization.hz)
            .map(|duration| {
                let nanos = u64::try_from(duration.as_nanos())
                    .map_err(|_| NetworkRpcError::internal("receive timeout duration overflow"))?;
                started
                    .as_nanos()
                    .checked_add(nanos)
                    .filter(|end| *end != LogicalTime::INDEFINITE.as_nanos())
                    .map(LogicalTime::from_nanos)
                    .ok_or_else(|| NetworkRpcError::internal("receive deadline overflow"))
            })
            .transpose()?;
        let policy = Arc::new(SavedReceivePolicy {
            owner,
            call: call.id,
            open_file: call.open_file,
            root: range.root.clone(),
            raw: range.raw,
            nonblocking: range.nonblocking,
            started,
            deadline,
        });
        engine
            .bind_saved_receive_policy(owner, call.id, policy)
            .map_err(|e| fail(&e))
    }

    pub(crate) fn saved_receive_policy<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        call: crate::network_replay::NetworkStreamCall,
        nonblocking: bool,
    ) -> Result<Option<Arc<SavedReceivePolicy>>, NetworkRpcError> {
        use reverie::syscalls::SyscallInfo;
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let policy = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("receive policy engine absent"))?
            .lock()
            .unwrap()
            .saved_receive_policy(owner, call.id)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        if let Some(policy) = &policy
            && (policy.raw != read.into_parts()
                || policy.nonblocking != nonblocking
                || !policy.matches(owner, call.id, call.open_file)
                || tid.as_raw() != policy.root.association().process()
                || !policy.root.matches_memory(&state.memory_metadata)
                || !policy.root.matches_metadata(&state.file_metadata)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm))
        {
            return Err(NetworkRpcError::internal(
                "saved receive policy changed original Read",
            ));
        }
        Ok(policy)
    }

    pub(crate) fn receive_policy_expired(
        &self,
        policy: &Arc<SavedReceivePolicy>,
    ) -> Result<bool, NetworkRpcError> {
        let saved = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("receive policy engine absent"))?
            .lock()
            .unwrap()
            .saved_receive_policy(policy.owner, policy.call)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        if saved
            .as_ref()
            .is_none_or(|saved| !Arc::ptr_eq(saved, policy))
        {
            return Err(NetworkRpcError::internal(
                "receive deadline lost its exact saved policy",
            ));
        }
        let now = self.global_time.lock().unwrap().as_nanos();
        if now < policy.started {
            return Err(NetworkRpcError::internal(
                "receive deadline clock precedes original start",
            ));
        }
        Ok(policy.expired(now))
    }

    fn checked_blocking_read_retry<'a, 's, T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        invocation: &'a CheckedReadInvocation,
        grant: &'a crate::scheduler::ordinary_fd::OrdinaryFdObservation<'s>,
    ) -> Result<CheckedBlockingReadRetry<'a, 's>, NetworkRpcError> {
        if let Some(first) = &invocation.failure {
            return Err(first.clone());
        }
        invocation.range.check(self, tid, state, read)?;
        if invocation.range.nonblocking
            || !invocation.call.physical_pin_required
            || grant.owner() != invocation.range.root.owner()
            || grant.epoch() <= invocation.range.epoch
        {
            return Err(NetworkRpcError::internal(
                "receive retry lacks a checked blocking invocation and new Normal grant",
            ));
        }
        Ok(CheckedBlockingReadRetry { invocation, grant })
    }

    pub(crate) fn bind_private_receive_invocation<T>(
        &self,
        range: CheckedReadRange,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        call: crate::network_replay::NetworkStreamCall,
    ) -> Result<CheckedReadInvocation, ReceiveAdmissionFailure> {
        let owner = range.root.owner();
        let result = (|| {
            range.check(self, tid, state, read)?;
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &range.root)
                .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
            if call.open_file != range.open_file
                || !call.physical_pin_required
                || grant.epoch() != range.epoch
            {
                return Err(NetworkRpcError::internal(
                    "checked Read binding crossed actual capture/grant",
                ));
            }
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("checked Read engine absent"))?;
            engine
                .lock()
                .unwrap()
                .check_receive_invocation_entry(
                    owner,
                    call.id,
                    &range.root,
                    range.epoch,
                    range.open_file,
                )
                .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
            Ok(())
        })();
        result.map_err(|primary| {
            self.receive_admission_failure(
                owner,
                primary,
                ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                    call: call.id,
                    stage: ReceiveAdmissionStage::Capture,
                }),
            )
        })?;
        Ok(CheckedReadInvocation {
            range,
            call,
            failure: None,
        })
    }

    /// The caller retains this exact callback receipt across its real wait.
    /// This operation issues no helper and never looks up the old numeric fd.
    pub(crate) async fn resume_private_receive_call<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: reverie::syscalls::Read,
        invocation: &mut CheckedReadInvocation,
        completed: crate::network_replay::CompletedNoStore,
    ) -> Result<(), ReceiveRetryFailure> {
        let owner = invocation.range.root.owner();
        let call = invocation.call.id;
        let mut retry = None;
        let result = async {
            if let Some(first) = &invocation.failure {
                return Err(first.clone());
            }
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            // Latch the actual completed attempt before any caller/reentry
            // validation; failed checks cannot be retried with repaired inputs.
            let attempt = completed
                .into_record_empty()
                .map_err(|e| fail(&e))?
                .begin()
                .map_err(|e| fail(&e))?;
            retry = Some(attempt);
            let attempt = retry.as_ref().unwrap();
            invocation.range.check(self, tid, state, read)?;
            if invocation.range.nonblocking
                || attempt.owner() != owner
                || attempt.call() != call
                || !Arc::ptr_eq(attempt.root(), &invocation.range.root)
                || attempt.epoch() != invocation.range.epoch
            {
                return Err(NetworkRpcError::internal(
                    "receive retry changed original blocking invocation or completed Call",
                ));
            }
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("receive retry runtime absent"))?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("receive retry engine absent"))?;
            let epoch = {
                let scheduler = self.sched.lock().unwrap();
                let grant = scheduler
                    .foreground_native_observation(owner, &invocation.range.root)
                    .map_err(|e| fail(&e))?;
                if grant.epoch() <= invocation.range.epoch {
                    return Err(NetworkRpcError::internal(
                        "receive retry requires a new actual Normal grant",
                    ));
                }
                engine
                    .lock()
                    .unwrap()
                    .validate_native_receive_retry(attempt)
                    .map_err(|e| fail(&e))?;
                grant.epoch()
            };
            let joined = runtime
                .join_receive_retry_prefix(attempt)
                .await
                .map_err(|e| fail(&e))?;
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &invocation.range.root)
                .map_err(|e| fail(&e))?;
            invocation.range.check(self, tid, state, read)?;
            if grant.epoch() != epoch {
                return Err(NetworkRpcError::internal(
                    "receive retry crossed actual foreground grant during join",
                ));
            }
            let checked = self.checked_blocking_read_retry(tid, state, read, invocation, &grant)?;
            let _memory = state.memory_metadata.lock().unwrap();
            let mut engine = engine.lock().unwrap();
            runtime
                .with_receive_retry_prefix(&joined, attempt, |admission| {
                    engine
                        .stamp_native_receive_retry(
                            attempt,
                            admission,
                            checked,
                            &grant,
                            self.global_time.lock().unwrap().as_nanos(),
                        )
                        .map_err(std::io::Error::other)
                })
                .map_err(|e| fail(&e))?;
            invocation.range.epoch = epoch;
            Ok(())
        }
        .await;
        result.map_err(|primary| {
            // Keep the original public cause, even when Drop or cleanup also
            // reports a failure. Cancellation independently latches on source.
            let primary = invocation.failure.get_or_insert(primary).clone();
            if let Some(attempt) = &retry {
                attempt.fail(&primary);
            }
            ReceiveRetryFailure {
                primary,
                cleanup: None,
                owner,
                call,
                engine: self
                    .network_engine
                    .as_ref()
                    .map(Arc::downgrade)
                    .unwrap_or_default(),
                custody: ReceiveRetryCustody::Call,
            }
        })
    }

    /// Retry cleanup is ordinary physical release, not failed initial capture.
    /// Actual worker joins precede close; the F6 completion alone permits ACK.
    pub(crate) async fn cleanup_receive_retry_failure(
        &self,
        mut failure: ReceiveRetryFailure,
    ) -> ReceiveRetryFailure {
        if failure.released() {
            return failure;
        }
        let work = async {
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("retry cleanup engine absent"))?;
            if !failure.engine.ptr_eq(&Arc::downgrade(engine)) {
                return Err(NetworkRpcError::internal(
                    "retry cleanup changed its actual engine",
                ));
            }
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("retry cleanup runtime absent"))?;
            if matches!(
                failure.custody,
                ReceiveRetryCustody::Call | ReceiveRetryCustody::ReleaseSubmitted
            ) {
                runtime
                    .join_receive_retry_release(failure.owner, failure.call)
                    .await
                    .map_err(|e| fail(&e))?;
            }
            if matches!(failure.custody, ReceiveRetryCustody::Call) {
                engine
                    .lock()
                    .unwrap()
                    .begin_stream_call_release(failure.owner, failure.call)
                    .map_err(|e| fail(&e))?;
                failure.custody = ReceiveRetryCustody::ReleaseSubmitted;
            }
            if matches!(failure.custody, ReceiveRetryCustody::ReleaseSubmitted) {
                if !runtime
                    .receive_retry_release_known(failure.owner, failure.call)
                    .map_err(|e| fail(&e))?
                {
                    runtime
                        .release_native_stream(failure.owner, failure.call)
                        .await
                        .map_err(|e| fail(&e))?;
                }
                let completed = engine
                    .lock()
                    .unwrap()
                    .complete_stream_call_release(failure.owner, failure.call)
                    .map_err(|e| fail(&e))?;
                failure.custody = ReceiveRetryCustody::Acknowledgement(completed);
                self.finish_local_receive_release();
            }
            if let ReceiveRetryCustody::Acknowledgement(completed) = &failure.custody {
                let (owner, call) = completed.identity();
                runtime
                    .finish_native_stream_release(owner, call)
                    .map_err(|e| fail(&e))?;
                let ReceiveRetryCustody::Acknowledgement(completed) =
                    std::mem::replace(&mut failure.custody, ReceiveRetryCustody::Released)
                else {
                    unreachable!()
                };
                if let Err(error) = completed.into_result() {
                    failure.cleanup.get_or_insert_with(|| fail(&error));
                }
                self.finish_local_receive_release();
            }
            Ok::<(), NetworkRpcError>(())
        }
        .await;
        if let Err(error) = work {
            failure.cleanup.get_or_insert(error);
        }
        failure
    }
}

/// A local, one-use handle to the preparation retained on the existing Call.
/// Dropping this handle leaves the Call's preparation and runtime exclusion live.
/// There is no strong runtime/engine ownership cycle and no asynchronous writer.
#[derive(Debug)]
pub(crate) struct ForegroundStorePermit<'a> {
    global: &'a GlobalState,
    engine: std::sync::Weak<Mutex<NetworkReplayEngine>>,
    store: Arc<ForegroundStore>,
}

/// Local ownership returned by an unsuccessful admission. It is deliberately
/// neither serialized nor cloneable; diagnostics are never cleanup authority.
#[derive(Debug)]
pub(crate) struct ReceiveAdmissionFailure {
    primary: NetworkRpcError,
    cleanup: Option<NetworkRpcError>,
    owner: NetworkStreamOwner,
    engine: std::sync::Weak<Mutex<NetworkReplayEngine>>,
    custody: ReceiveAdmissionCustody,
}
#[derive(Debug)]
pub(crate) enum ReceiveAdmissionCustody {
    /// The supplied token is returned exactly; this does not validate a forgery.
    ReturnedRead(crate::network_replay::NetworkFdReadAdmission),
    Released,
    RetainedCall(RetainedReceiveAdmission),
}
#[derive(Debug)]
pub(crate) struct RetainedReceiveAdmission {
    call: crate::network_replay::NetworkStreamCallId,
    stage: ReceiveAdmissionStage,
}
#[derive(Debug)]
enum ReceiveAdmissionStage {
    Unsubmitted(crate::network_runtime::JoinedNativePrefix),
    Capture,
    Replay(NetworkStreamLeaseId),
}
impl ReceiveAdmissionFailure {
    pub(crate) fn primary(&self) -> &NetworkRpcError {
        &self.primary
    }
    pub(crate) fn cleanup_diagnostic(&self) -> Option<&NetworkRpcError> {
        self.cleanup.as_ref()
    }
    pub(crate) fn custody(&self) -> &ReceiveAdmissionCustody {
        &self.custody
    }
    pub(super) fn into_rpc_error(self) -> NetworkRpcError {
        match self.cleanup {
            None => self.primary,
            Some(cleanup) => {
                NetworkRpcError::internal(format!("{}; admission cleanup: {cleanup}", self.primary))
            }
        }
    }
}

impl GlobalState {
    /// Consume this local outcome without guessing from its error text. A
    /// failed cleanup returns its original custody and an additional diagnostic.
    pub(crate) async fn cleanup_receive_admission_failure(
        &self,
        mut failure: ReceiveAdmissionFailure,
    ) -> ReceiveAdmissionFailure {
        if matches!(failure.custody, ReceiveAdmissionCustody::Released) {
            return failure;
        }
        let cleanup = async {
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("admission cleanup lost engine"))?;
            if !failure.engine.ptr_eq(&Arc::downgrade(engine)) {
                return Err(NetworkRpcError::internal(
                    "admission cleanup changed actual engine",
                ));
            }
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            match &failure.custody {
                ReceiveAdmissionCustody::ReturnedRead(read) => {
                    engine
                        .lock()
                        .unwrap()
                        .finish_fd_read(failure.owner, read.clone())
                        .map_err(|e| fail(&e))?;
                    Ok((true, None))
                }
                ReceiveAdmissionCustody::Released => {
                    unreachable!("already released outcome returned above")
                }
                ReceiveAdmissionCustody::RetainedCall(retained) => match &retained.stage {
                    ReceiveAdmissionStage::Replay(control) => {
                        let completed = engine
                            .lock()
                            .unwrap()
                            .cancel_replay_receive_admission(failure.owner, retained.call, *control)
                            .map_err(|e| fail(&e))?;
                        // Call removal is positive even if the terminal journal
                        // refuses. Preserve that separate diagnostic without
                        // claiming custody of a nonexistent logical Call.
                        Ok((true, completed.into_result().err().map(|e| fail(&e))))
                    }
                    ReceiveAdmissionStage::Unsubmitted(prefix) => {
                        let runtime = self.network_runtime.as_ref().ok_or_else(|| {
                            NetworkRpcError::internal("admission cleanup lost runtime")
                        })?;
                        let mut engine = engine.lock().unwrap();
                        runtime
                            .with_foreground_prefix(prefix, |proof| {
                                engine
                                    .cancel_retained_unsubmitted_native_entry(
                                        failure.owner,
                                        retained.call,
                                        prefix,
                                        proof,
                                    )
                                    .map_err(std::io::Error::other)
                            })
                            .map_err(|e| fail(&e))?;
                        Ok((true, None))
                    }
                    ReceiveAdmissionStage::Capture => {
                        let runtime = self.network_runtime.as_ref().ok_or_else(|| {
                            NetworkRpcError::internal("admission cleanup lost runtime")
                        })?;
                        let recovery = self.native_capture_recovery().ok_or_else(|| {
                            NetworkRpcError::internal("admission cleanup lost recovery owner")
                        })?;
                        runtime
                            .retire_failed_receive_admission(failure.owner, retained.call, recovery)
                            .await
                            .map(|released| (released, None))
                            .map_err(|e| fail(&e))
                    }
                },
            }
        }
        .await;
        match cleanup {
            Ok((true, diagnostic)) => {
                failure.custody = ReceiveAdmissionCustody::Released;
                if let Some(error) = diagnostic {
                    failure.cleanup.get_or_insert(error);
                }
                self.finish_local_receive_release();
            }
            Ok((false, _)) => {
                failure.cleanup.get_or_insert_with(|| {
                    NetworkRpcError::internal("capture outcome or retirement remains unresolved")
                });
            }
            Err(error) => {
                failure.cleanup.get_or_insert(error);
            }
        }
        failure
    }

    /// Called only after engine/runtime guards are dropped and an actual local
    /// release is known. Match the ordinary RPC's retired-port and waiter work.
    fn finish_local_receive_release(&self) {
        let retired = self
            .network_engine
            .as_ref()
            .expect("local release owns engine")
            .lock()
            .unwrap()
            .take_lifetime_retired_ports();
        self.release_lifetime_ports(retired);
        self.network_stream_changed.notify_waiters();
    }

    fn receive_admission_failure(
        &self,
        owner: NetworkStreamOwner,
        primary: NetworkRpcError,
        custody: ReceiveAdmissionCustody,
    ) -> ReceiveAdmissionFailure {
        ReceiveAdmissionFailure {
            primary,
            cleanup: None,
            owner,
            engine: self
                .network_engine
                .as_ref()
                .map(Arc::downgrade)
                .unwrap_or_default(),
            custody,
        }
    }
}

impl GlobalState {
    /// Offline admission transfers the same reader directly into a logical
    /// Call. No native file capture, helper command or release stamp is made.
    pub(crate) fn begin_replay_receive_call<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: crate::network_replay::NetworkFdReadAdmission,
        destination: u64,
        maximum: usize,
    ) -> Result<crate::network_replay::NetworkStreamCall, ReceiveAdmissionFailure> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let result = (|| -> Result<_, NetworkRpcError> {
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            if !self.cfg.sequentialize_threads || !(1..=512).contains(&maximum) {
                return Err(NetworkRpcError::internal(
                    "Replay receive requires bounded strict scalar capacity",
                ));
            }
            let owner = NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            };
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("Replay local runtime absent"))?;
            let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
            let scheduler = self.sched.lock().unwrap();
            scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            if tid.as_raw() != root.association().process()
                || state.detpid != Some(owner.thread)
                || !root.matches_memory(&state.memory_metadata)
                || !root.matches_metadata(&state.file_metadata)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "Replay entry changed actual local task/MM metadata",
                ));
            }
            destination
                .checked_add(maximum as u64)
                .ok_or_else(|| NetworkRpcError::internal("Replay original range overflow"))?;
            let _memory = state.memory_metadata.lock().unwrap();
            let metadata = state.file_metadata.lock().unwrap();
            let mut engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("Replay engine absent"))?
                .lock()
                .unwrap();
            if !engine.native_receive_version()
                || engine.mode() != crate::network_replay::NetworkEngineMode::Replay
            {
                return Err(NetworkRpcError::internal(
                    "offline entry requires the shared V4 Replay engine",
                ));
            }
            engine
                .validate_fd_metadata(
                    owner,
                    read.binding
                        .ok_or_else(|| {
                            NetworkRpcError::internal("Replay entry requires its admitted socket")
                        })?
                        .slot
                        .files,
                    &state.file_metadata,
                    &metadata,
                )
                .map_err(|e| fail(&e))?;
            let control = read
                .control
                .ok_or_else(|| NetworkRpcError::internal("Replay entry lost admitted control"))?;
            let call = engine
                .begin_native_stream_call_from_read(owner, read)
                .map_err(|e| fail(&e))?;
            custody = ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                call: call.id,
                stage: ReceiveAdmissionStage::Replay(control),
            });
            engine
                .finish_socket_control(
                    owner,
                    control,
                    crate::network_replay::NetworkSocketControlFinish::Unchanged,
                )
                .map_err(|e| fail(&e))?;
            Ok(call)
        })();
        match result {
            Ok(call) => {
                self.finish_local_receive_release();
                Ok(call)
            }
            Err(primary) => Err(self.receive_admission_failure(owner, primary, custody)),
        }
    }

    pub(crate) async fn prepare_replay_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        call: crate::network_replay::NetworkStreamCallId,
        maximum: usize,
        destination: u64,
        nonblocking: bool,
    ) -> Result<ReceiveSelection<ForegroundStorePermit<'_>>, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        if !self.cfg.sequentialize_threads || !(1..=512).contains(&maximum) {
            return Err(NetworkRpcError::internal(
                "Replay selection requires bounded strict foreground capacity",
            ));
        }
        destination
            .checked_add(maximum as u64)
            .ok_or_else(|| NetworkRpcError::internal("Replay original range overflow"))?;
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Replay runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Replay engine absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let check_local = || {
            if tid.as_raw() != root.association().process()
                || state.detpid != Some(owner.thread)
                || !root.matches_memory(&state.memory_metadata)
                || !root.matches_metadata(&state.file_metadata)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "Replay selection changed actual task/MM metadata",
                ));
            }
            Ok(())
        };
        let (plan, epoch) = {
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            check_local()?;
            if engine
                .lock()
                .unwrap()
                .saved_receive_policy(owner, call)
                .map_err(|e| fail(&e))?
                .as_ref()
                .is_some_and(|policy| {
                    !policy.matches_span(maximum, destination)
                        || policy.nonblocking() != nonblocking
                })
            {
                return Err(NetworkRpcError::internal(
                    "Replay selection changed saved Read span/flags",
                ));
            }
            (
                engine
                    .lock()
                    .unwrap()
                    .plan_replay_receive_at(
                        owner, call, maximum, nonblocking,
                        Some(self.global_time.lock().unwrap().as_nanos()),
                    )
                    .map_err(|e| fail(&e))?,
                grant.epoch(),
            )
        };
        match plan {
            ReplayReceivePlan::Bytes(plan) => {
                let source = {
                    let scheduler = self.sched.lock().unwrap();
                    let grant = scheduler
                        .foreground_native_observation(owner, &root)
                        .map_err(|e| fail(&e))?;
                    check_local()?;
                    if grant.epoch() != epoch {
                        return Err(NetworkRpcError::internal(
                            "Replay selection crossed foreground grant",
                        ));
                    }
                    let memory = state.memory_metadata.lock().unwrap();
                    // Validate only the actual selected copy footprint, before
                    // any Delivery/Store owner exists. No-store never maps it.
                    memory
                        .original_copy_span(owner, destination, plan.length() as u64)
                        .map_err(|e| fail(&e))?;
                    engine
                        .lock()
                        .unwrap()
                        .reserve_replay_planned_store(&plan)
                        .map_err(|e| fail(&e))?
                };
                self.prepare_foreground_store(tid, state, source.lease(), destination)
                    .await
                    .map(ReceiveSelection::Bytes)
            }
            ReplayReceivePlan::NoStore(plan) => {
                // The exact GlobalState/engine and private plan remain borrowed
                // by this one operation. No Guest continuation or plan export
                // can substitute another engine with matching numeric IDs.
                let joined = runtime
                    .join_foreground_prefix(root.clone())
                    .await
                    .map_err(|e| fail(&e))?;
                let completed = {
                    let scheduler = self.sched.lock().unwrap();
                    let grant = scheduler
                        .foreground_native_observation(owner, &root)
                        .map_err(|e| fail(&e))?;
                    check_local()?;
                    if grant.epoch() != epoch {
                        return Err(NetworkRpcError::internal(
                            "Replay no-store crossed foreground grant",
                        ));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let mut engine = engine.lock().unwrap();
                    runtime
                        .with_foreground_prefix(&joined, |admission| {
                            engine
                                .commit_replay_no_store(
                                    plan, admission, &root,
                                    self.global_time.lock().unwrap().as_nanos(),
                                )
                                .map_err(std::io::Error::other)
                        })
                        .map_err(|e| fail(&e))?
                };
                self.finish_local_receive_release();
                Ok(ReceiveSelection::NoStore(completed))
            }
            ReplayReceivePlan::Wait => Ok(ReceiveSelection::Wait),
        }
    }

    pub(crate) fn commit_replay_receive_store(
        &self,
        full: &FullStoreCompletion,
    ) -> Result<usize, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let store = full.store();
        let owner = store.owner();
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Replay runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .map_err(|e| fail(&e))?;
        if !self.cfg.sequentialize_threads
            || !Arc::ptr_eq(&root, store.root())
            || grant.epoch() != store.epoch()
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "Replay consumption changed actual store root/MM/grant",
            ));
        }
        let actual_memory = root.memory().map_err(|e| fail(&e))?;
        let memory = actual_memory.lock().unwrap();
        memory
            .validate_original_copy_span(owner, store.span())
            .map_err(|e| fail(&e))?;
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Replay engine absent"))?
            .lock()
            .unwrap();
        runtime
            .with_ended_replay_copy(store.exclusion(), store.source(), || {
                engine
                    .commit_replay_foreground_store(full)
                    .map_err(std::io::Error::other)
            })
            .map_err(|e| fail(&e))
    }
}

impl GlobalState {
    /// Preparation may join old worker tails. No scheduler, metadata or engine
    /// mutex survives the await, and the exact grant epoch is checked afterward.
    pub(crate) async fn prepare_foreground_store<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        lease: NetworkStreamLeaseId,
        destination: u64,
    ) -> Result<ForegroundStorePermit<'_>, NetworkRpcError> {
        let fail = |error: &dyn std::fmt::Display| NetworkRpcError::internal(error.to_string());
        if !self.cfg.sequentialize_threads {
            return Err(NetworkRpcError::internal(
                "foreground stores require the actual strict scheduler",
            ));
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground engine absent"))?;
        let root = runtime
            .foreground_root(owner)
            .map_err(|error| fail(&error))?;
        let check_local = || {
            if tid.as_raw() != root.association().process()
                || state.detpid != Some(owner.thread)
                || !root.matches_memory(&state.memory_metadata)
                || !root.matches_metadata(&state.file_metadata)
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                Err(NetworkRpcError::internal(
                    "foreground store changed actual backend task/MM metadata",
                ))
            } else {
                Ok(())
            }
        };
        let (completion, length, epoch) = {
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|error| fail(&error))?;
            check_local()?;
            let memory = state.memory_metadata.lock().unwrap();
            let (completion, length) = engine
                .lock()
                .unwrap()
                .foreground_store_selection(owner, lease)
                .map_err(|error| fail(&error))?;
            memory
                .original_copy_span(owner, destination, length as u64)
                .map_err(|error| fail(&error))?;
            (completion, length, grant.epoch())
        };
        let exclusion = runtime
            .exclude_native_for_store(root.clone(), completion)
            .await
            .map_err(|error| fail(&error))?;
        // If a post-join check refuses, the runtime retains the exclusion. Only
        // actual backend-ended cleanup can abandon that unresolved interval.
        let store = {
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|error| fail(&error))?;
            check_local()?;
            if grant.epoch() != epoch {
                return Err(NetworkRpcError::internal(
                    "foreground preparation crossed a scheduler grant",
                ));
            }
            let memory = state.memory_metadata.lock().unwrap();
            let span = memory
                .original_copy_span(owner, destination, length as u64)
                .map_err(|error| fail(&error))?;
            runtime
                .validate_native_copy_exclusion(&exclusion)
                .map_err(|error| fail(&error))?;
            engine
                .lock()
                .unwrap()
                .prepare_foreground_store(
                    owner,
                    lease,
                    root.clone(),
                    &memory,
                    span,
                    exclusion,
                    epoch,
                )
                .map_err(|error| fail(&error))?
        };
        Ok(ForegroundStorePermit {
            global: self,
            engine: Arc::downgrade(engine),
            store,
        })
    }
}

impl ForegroundStorePermit<'_> {
    #[cfg(test)]
    pub(super) fn retained_store(&self) -> Arc<ForegroundStore> {
        self.store.clone()
    }

    /// The backend's actual stopped MemoryAccess is borrowed for this complete
    /// synchronous operation. A failed/partial/unknown write issues no handoff.
    pub(crate) fn copy<T, M: MemoryAccess>(
        self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        memory: &mut M,
    ) -> Result<(StoreOutcome, Option<FullStoreCompletion>), NetworkRpcError> {
        let fail = |error: &dyn std::fmt::Display| NetworkRpcError::internal(error.to_string());
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| NetworkRpcError::internal("foreground copy lost its engine"))?;
        let runtime = self
            .global
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground runtime absent"))?;
        let owner = self.store.owner();
        let root = self.store.root();
        let scheduler = self.global.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, root)
            .map_err(|error| fail(&error))?;
        let registered = self.global.registered_exec_mms.lock().unwrap();
        if tid.as_raw() != root.association().process()
            || state.dettid != owner.thread
            || state.mm_id != owner.mm
            || state.detpid != Some(owner.thread)
            || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || registered.get(&owner.thread) != Some(&owner.mm)
            || self
                .global
                .network_engine
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, &engine))
        {
            return Err(NetworkRpcError::internal(
                "foreground copy changed actual local task/MM/engine custody",
            ));
        }
        let metadata = state.memory_metadata.lock().unwrap();
        let mut engine = engine.lock().unwrap();
        engine
            .perform_foreground_store(&self.store, &grant, &metadata, runtime, memory)
            .map_err(|error| fail(&error))
    }
}

impl GlobalState {
    /// Local source-to-delivery adapter. Both engine selection and the actual
    /// confirmed runtime probe transfer before the store can be prepared.
    pub(crate) async fn prepare_private_receive_store<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        call: crate::network_replay::NetworkStreamCallId,
        probe: NetworkStreamLeaseId,
        maximum: usize,
        destination: u64,
    ) -> Result<ForegroundStorePermit<'_>, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        if !self.cfg.sequentialize_threads || !(1..=512).contains(&maximum) {
            return Err(NetworkRpcError::internal(
                "private receive store requires a bounded strict foreground call",
            ));
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let lease = {
            let scheduler = self.sched.lock().unwrap();
            scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            let registered = self.registered_exec_mms.lock().unwrap();
            if tid.as_raw() != root.association().process()
                || state.detpid != Some(owner.thread)
                || !root.matches_memory(&state.memory_metadata)
                || !root.matches_metadata(&state.file_metadata)
                || registered.get(&owner.thread) != Some(&owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "private selection changed actual task/MM metadata",
                ));
            }
            let memory = state.memory_metadata.lock().unwrap();
            let mut engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("foreground engine absent"))?
                .lock()
                .unwrap();
            let source = engine
                .private_receive_completion(owner, probe)
                .map_err(|e| fail(&e))?;
            let length = maximum.min(source.capture().committed.len());
            memory
                .original_copy_span(owner, destination, length as u64)
                .map_err(|e| fail(&e))?;
            let crate::network_replay::NetworkStreamChunk::Reserved { lease, .. } = runtime
                .reserve_private_receive_span(&mut engine, owner, call, probe, maximum, 0)
                .map_err(|e| fail(&e))?
            else {
                unreachable!("private nonempty source reserves a delivery")
            };
            lease
        };
        self.prepare_foreground_store(tid, state, lease, destination)
            .await
    }

    /// The actual helper worker owns the native call across cancellation. The
    /// immutable result is retained before physical validation; no guest result
    /// or deterministic release is issued by this local reconciliation.
    pub(crate) async fn reconcile_foreground_store(
        &self,
        full: FullStoreCompletion,
    ) -> Result<(), NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground engine absent"))?;
        let store = full.store();
        let owner = store.owner();
        let lease = store.lease();
        let effect = {
            let scheduler = self.sched.lock().unwrap();
            let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            if !Arc::ptr_eq(&root, store.root())
                || grant.epoch() != store.epoch()
                || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            {
                return Err(NetworkRpcError::internal(
                    "private Drain changed local root or foreground grant",
                ));
            }
            engine
                .lock()
                .unwrap()
                .begin_private_drain(&full)
                .map_err(|e| fail(&e))?
        };
        let observed = runtime
            .execute_private_receive_drain(full)
            .await
            .map_err(|e| fail(&e))?;
        runtime
            .preflight_native_stream(owner, lease, &effect, &observed)
            .map_err(|e| fail(&e))?;
        engine
            .lock()
            .unwrap()
            .confirm_retained_stream_physical(owner, lease, &observed)
            .map_err(|e| fail(&e))?;
        runtime
            .confirm_native_stream(owner, lease, &effect, &observed)
            .map_err(|e| fail(&e))?;
        Ok(())
    }
}

impl GlobalState {
    /// Same local foreground root and MM join the exact matched Drain to its
    /// confirmed runtime Pending. This cannot emit a trace or return a syscall.
    pub(crate) fn prepare_foreground_receive_publication(
        &self,
        full: &FullStoreCompletion,
    ) -> Result<Arc<crate::network_replay::PreparedPrivatePublication>, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("foreground engine absent"))?;
        let store = full.store();
        let owner = store.owner();
        let scheduler = self.sched.lock().unwrap();
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .map_err(|e| fail(&e))?;
        let registered = self.registered_exec_mms.lock().unwrap();
        if !self.cfg.sequentialize_threads
            || !Arc::ptr_eq(&root, store.root())
            || grant.epoch() != store.epoch()
            || registered.get(&owner.thread) != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "private publication changed local root/MM/grant",
            ));
        }
        runtime
            .prepare_private_receive_publication(&mut engine.lock().unwrap(), full)
            .map_err(|e| fail(&e))
    }
}

impl GlobalState {
    /// Prepare the selected Connect before its reader can become a Call. The
    /// exact joined-prefix borrow spans creation, stamp and never-submitted
    /// rollback; no provider/capture receipt is fabricated by this transaction.
    pub(super) async fn begin_original_native_entry_from_read(
        &self,
        owner: NetworkStreamOwner,
        arguments: crate::network_replay::original_connect::Arguments,
        read: crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<
        (
            crate::network_replay::original_connect::Admission,
            std::os::fd::OwnedFd,
        ),
        ReceiveAdmissionFailure,
    > {
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let mut cleanup_diagnostic = None;
        let result = async {
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            if !self.cfg.sequentialize_threads
                || arguments.kind != crate::network_replay::original_connect::Kind::Connect
                || read.external_grant != Some(arguments.operation)
            {
                return Err(NetworkRpcError::internal(
                    "native Connect entry requires its strict selected reader",
                ));
            }
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("native entry engine absent"))?;
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("native entry runtime absent"))?;
            let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
            let actual = root.metadata().map_err(|e| fail(&e))?;
            {
                let scheduler = self.sched.lock().unwrap();
                scheduler
                    .native_capture_entry_observation(owner, arguments.operation, &root)
                    .map_err(|e| fail(&e))?;
                if self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm) {
                    return Err(NetworkRpcError::internal(
                        "native Connect entry changed registered MM",
                    ));
                }
            }
            let task = runtime
                .prepare_native_capture_task(owner)
                .map_err(|e| fail(&e))?;
            let joined = runtime
                .join_foreground_prefix(root.clone())
                .await
                .map_err(|e| fail(&e))?;
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .native_capture_entry_observation(owner, arguments.operation, &root)
                .map_err(|e| fail(&e))?;
            let registered = self.registered_exec_mms.lock().unwrap();
            if registered.get(&owner.thread) != Some(&owner.mm) {
                return Err(NetworkRpcError::internal(
                    "native Connect entry lost same registered MM after join",
                ));
            }
            let metadata = actual.lock().unwrap();
            let mut engine = engine.lock().unwrap();
            if !engine.native_receive_version()
                || engine.mode() != crate::network_replay::NetworkEngineMode::Record
            {
                return Err(NetworkRpcError::internal(
                    "native Connect entry requires the actual V4 recorder",
                ));
            }
            engine
                .validate_fd_metadata(owner, arguments.files, &actual, &metadata)
                .map_err(|e| fail(&e))?;
            let admission = runtime
                .with_foreground_prefix(&joined, |prefix| {
                    let admission = engine
                        .begin_original_external_from_read(owner, arguments, read)
                        .map_err(std::io::Error::other)?;
                    custody = ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                        call: admission.call,
                        stage: ReceiveAdmissionStage::Unsubmitted(joined.clone()),
                    });
                    let mut attempt = engine
                        .begin_native_entry_stamp(owner, admission.call)
                        .map_err(std::io::Error::other)?;
                    let retained = attempt
                        .retain_unsubmitted_recovery(&joined)
                        .map_err(std::io::Error::other)?;
                    let now = self.global_time.lock().unwrap().as_nanos();
                    if let Err(primary) =
                        engine.stamp_native_connect_entry(attempt, prefix, &grant, now)
                    {
                        match engine.cancel_unsubmitted_native_entry(&retained, prefix) {
                            Ok(()) => custody = ReceiveAdmissionCustody::Released,
                            Err(secondary) => cleanup_diagnostic = Some(fail(&secondary)),
                        }
                        return Err(std::io::Error::other(primary));
                    }
                    Ok(admission)
                })
                .map_err(|e| fail(&e))?;
            Ok((admission, task))
        }
        .await;
        result.map_err(|primary| {
            if matches!(custody, ReceiveAdmissionCustody::Released) {
                self.finish_local_receive_release();
            }
            let mut failure = self.receive_admission_failure(owner, primary, custody);
            failure.cleanup = cleanup_diagnostic;
            failure
        })
    }

    /// The Guest supplies its actual local state, not serialized metadata. The
    /// existing FD reader transfers directly into the same physical Call, and
    /// its immutable V4 entry precedes the first pidfd_getfd/Peek worker.
    pub(crate) async fn begin_private_receive_call<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: crate::network_replay::NetworkFdReadAdmission,
        destination: u64,
        maximum: usize,
    ) -> Result<crate::network_replay::NetworkStreamCall, ReceiveAdmissionFailure> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let mut custody = ReceiveAdmissionCustody::ReturnedRead(read.clone());
        let mut cleanup_diagnostic = None;
        let prepared = async {
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            if !self.cfg.sequentialize_threads || !(1..=512).contains(&maximum) {
                return Err(NetworkRpcError::internal(
                    "private receive requires bounded strict scalar capacity",
                ));
            }
            let owner = NetworkStreamOwner {
                thread: state.dettid,
                mm: state.mm_id,
            };
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("private entry runtime absent"))?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("private entry engine absent"))?;
            let recovery = self
                .native_capture_recovery()
                .ok_or_else(|| NetworkRpcError::internal("private entry lost recovery owner"))?;
            let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
            let check_local = || {
                if tid.as_raw() != root.association().process()
                    || state.detpid != Some(owner.thread)
                    || !root.matches_memory(&state.memory_metadata)
                    || !root.matches_metadata(&state.file_metadata)
                    || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                {
                    return Err(NetworkRpcError::internal(
                        "private entry changed actual local task/MM metadata",
                    ));
                }
                Ok(())
            };
            let epoch = {
                let scheduler = self.sched.lock().unwrap();
                let grant = scheduler
                    .foreground_native_observation(owner, &root)
                    .map_err(|e| fail(&e))?;
                check_local()?;
                destination
                    .checked_add(maximum as u64)
                    .ok_or_else(|| NetworkRpcError::internal("private original range overflow"))?;
                grant.epoch()
            };
            let joined = runtime
                .join_foreground_prefix(root.clone())
                .await
                .map_err(|e| fail(&e))?;
            let task = runtime
                .prepare_native_capture_task(owner)
                .map_err(|e| fail(&e))?;
            let binding = read
                .binding
                .ok_or_else(|| NetworkRpcError::internal("private receive lost admitted socket"))?;
            let control = read.control.ok_or_else(|| {
                NetworkRpcError::internal("private receive lost admitted control")
            })?;
            let (call, identity) = {
                let scheduler = self.sched.lock().unwrap();
                let grant = scheduler
                    .foreground_native_observation(owner, &root)
                    .map_err(|e| fail(&e))?;
                check_local()?;
                if grant.epoch() != epoch {
                    return Err(NetworkRpcError::internal(
                        "private entry crossed foreground grant",
                    ));
                }
                let _memory = state.memory_metadata.lock().unwrap();
                destination
                    .checked_add(maximum as u64)
                    .ok_or_else(|| NetworkRpcError::internal("private original range overflow"))?;
                let metadata = state.file_metadata.lock().unwrap();
                let mut engine = engine.lock().unwrap();
                if !engine.native_receive_version()
                    || engine.mode() != crate::network_replay::NetworkEngineMode::Record
                {
                    return Err(NetworkRpcError::internal(
                        "private native entry requires explicit V4 recorder",
                    ));
                }
                let identity = engine
                    .native_stream_capture_identity(owner, &read, &state.file_metadata, &metadata)
                    .map_err(|e| fail(&e))?
                    .ok_or_else(|| {
                        NetworkRpcError::internal("private entry lacks original file identity")
                    })?;
                let call = runtime
                    .with_foreground_prefix(&joined, |prefix| {
                        // No native admission can interleave with transfer/stamp/abort.
                        let call = engine
                            .begin_native_stream_call_from_read(owner, read.clone())
                            .map_err(std::io::Error::other)?;
                        custody = ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                            call: call.id,
                            stage: ReceiveAdmissionStage::Unsubmitted(joined.clone()),
                        });
                        let mut attempt = engine
                            .begin_native_entry_stamp(owner, call.id)
                            .map_err(std::io::Error::other)?;
                        let unsubmitted = attempt
                            .retain_unsubmitted_recovery(&joined)
                            .map_err(std::io::Error::other)?;
                        let now = self.global_time.lock().unwrap().as_nanos();
                        if let Err(primary) =
                            engine.stamp_native_receive_entry(attempt, prefix, &grant, now)
                        {
                            match engine.cancel_unsubmitted_native_entry(&unsubmitted, prefix) {
                                Ok(()) => custody = ReceiveAdmissionCustody::Released,
                                Err(secondary) => cleanup_diagnostic = Some(fail(&secondary)),
                            }
                            return Err(std::io::Error::other(primary));
                        }
                        Ok(call)
                    })
                    .map_err(|e| fail(&e))?;
                (call, identity)
            };
            Ok((
                root, epoch, call, control, binding, task, identity, recovery,
            ))
        }
        .await;
        let (root, epoch, call, control, binding, task, identity, recovery) = match prepared {
            Ok(prepared) => prepared,
            Err(primary) => {
                if matches!(custody, ReceiveAdmissionCustody::Released) {
                    self.finish_local_receive_release();
                }
                let mut failure = self.receive_admission_failure(owner, primary, custody);
                failure.cleanup = cleanup_diagnostic;
                return Err(
                    if matches!(failure.custody, ReceiveAdmissionCustody::RetainedCall(_)) {
                        self.cleanup_receive_admission_failure(failure).await
                    } else {
                        failure
                    },
                );
            }
        };
        let runtime = self.network_runtime.as_ref().expect("prepared runtime");
        let captured = runtime
            .capture_native_stream(owner, call.id, binding.slot.fd, task, identity, recovery)
            .await;
        match captured {
            Ok(captured) => {
                self.complete_private_receive_capture(
                    tid, state, &root, epoch, call, control, captured,
                )
                .await
            }
            Err(primary) => Err(self
                .fail_private_receive_capture(
                    owner,
                    call,
                    NetworkRpcError::internal(primary.to_string()),
                )
                .await),
        }
    }

    pub(super) async fn fail_private_receive_capture(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCall,
        primary: NetworkRpcError,
    ) -> ReceiveAdmissionFailure {
        let failure = self.receive_admission_failure(
            owner,
            primary,
            ReceiveAdmissionCustody::RetainedCall(RetainedReceiveAdmission {
                call: call.id,
                stage: ReceiveAdmissionStage::Capture,
            }),
        );
        self.cleanup_receive_admission_failure(failure).await
    }

    /// Complete the captured original FD on the actual local continuation.
    /// This is also the sole completion operation used by the admission path.
    pub(super) async fn complete_private_receive_capture<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        epoch: u64,
        call: crate::network_replay::NetworkStreamCall,
        control: NetworkStreamLeaseId,
        captured: crate::network_replay::NetworkStreamPinOutcome,
    ) -> Result<crate::network_replay::NetworkStreamCall, ReceiveAdmissionFailure> {
        let owner = root.owner();
        let result = (|| -> Result<_, NetworkRpcError> {
            use crate::network_replay::NetworkSocketControlFinish;
            use crate::network_replay::NetworkStreamPinOutcome;
            let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
            let runtime = self
                .network_runtime
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("private entry runtime absent"))?;
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| NetworkRpcError::internal("private entry engine absent"))?;
            let check_local = || {
                if state.dettid != owner.thread
                    || state.mm_id != owner.mm
                    || tid.as_raw() != root.association().process()
                    || state.detpid != Some(owner.thread)
                    || !root.matches_memory(&state.memory_metadata)
                    || !root.matches_metadata(&state.file_metadata)
                    || self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                {
                    return Err(NetworkRpcError::internal(
                        "private entry changed actual local task/MM metadata",
                    ));
                }
                Ok(())
            };
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            check_local()?;
            if grant.epoch() != epoch {
                return Err(NetworkRpcError::internal(
                    "private capture crossed foreground grant",
                ));
            }
            if let NetworkStreamPinOutcome::Failed(errno) = captured {
                return Err(NetworkRpcError::internal(format!(
                    "private original capture failed with errno {errno}"
                )));
            }
            let mut engine = engine.lock().unwrap();
            engine
                .confirm_stream_call_pin(owner, call.id, captured)
                .map_err(|e| fail(&e))?;
            engine
                .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
                .map_err(|e| fail(&e))?;
            Ok(call)
        })();
        match result {
            Ok(call) => {
                self.finish_local_receive_release();
                Ok(call)
            }
            Err(primary) => Err(self
                .fail_private_receive_capture(owner, call, primary)
                .await),
        }
    }

    pub(crate) fn publish_foreground_native_receive(
        &self,
        prepared: &Arc<crate::network_replay::PreparedPrivatePublication>,
    ) -> Result<usize, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("publication runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("publication engine absent"))?;
        let store = prepared.full().store();
        let owner = store.owner();
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &root)
            .map_err(|e| fail(&e))?;
        let registered = self.registered_exec_mms.lock().unwrap();
        if !self.cfg.sequentialize_threads
            || !Arc::ptr_eq(&root, store.root())
            || grant.epoch() != store.epoch()
            || registered.get(&owner.thread) != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "V4 publication changed actual store root/MM/grant",
            ));
        }
        let mut engine = engine.lock().unwrap();
        runtime
            .publish_private_native_receive(
                &mut engine,
                prepared,
                self.global_time.lock().unwrap().as_nanos(),
            )
            .map_err(|e| fail(&e))
    }

    pub(crate) fn publish_foreground_native_connected<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> Result<(), NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Connect publication runtime absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let scheduler = self.sched.lock().unwrap();
        scheduler
            .foreground_native_observation(owner, &root)
            .map_err(|e| fail(&e))?;
        let registered = self.registered_exec_mms.lock().unwrap();
        if !self.cfg.sequentialize_threads
            || tid.as_raw() != root.association().process()
            || state.detpid != Some(owner.thread)
            || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || registered.get(&owner.thread) != Some(&owner.mm)
        {
            return Err(NetworkRpcError::internal(
                "Connect publication changed actual root/continuation",
            ));
        }
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Connect engine absent"))?
            .lock()
            .unwrap();
        runtime
            .publish_native_connected(
                &mut engine,
                owner,
                admission,
                &root,
                self.global_time.lock().unwrap().as_nanos(),
            )
            .map_err(|e| fail(&e))
    }
}

impl GlobalState {
    /// Complete an actual no-unit Record receive. This operation never inspects
    /// mappings or grants a copy; Guest range precedence is a separate issuer.
    pub(crate) async fn complete_foreground_record_no_store<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        call: crate::network_replay::NetworkStreamCallId,
        probe: NetworkStreamLeaseId,
    ) -> Result<crate::network_replay::CompletedNoStore, NetworkRpcError> {
        let fail = |e: &dyn std::fmt::Display| NetworkRpcError::internal(e.to_string());
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("no-store runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("no-store engine absent"))?;
        let root = runtime.foreground_root(owner).map_err(|e| fail(&e))?;
        let check_local = || {
            if !self.cfg.sequentialize_threads
                || tid.as_raw() != root.association().process()
                || state.detpid != Some(owner.thread)
                || !root.matches_memory(&state.memory_metadata)
                || !root.matches_metadata(&state.file_metadata)
            {
                return Err(NetworkRpcError::internal(
                    "no-store changed actual local task/root/MM metadata",
                ));
            }
            Ok(())
        };
        let (source, epoch) = {
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            let registered = self.registered_exec_mms.lock().unwrap();
            check_local()?;
            if registered.get(&owner.thread) != Some(&owner.mm) {
                return Err(NetworkRpcError::internal("no-store changed registered MM"));
            }
            (
                engine
                    .lock()
                    .unwrap()
                    .record_no_store_source(owner, call, probe)
                    .map_err(|e| fail(&e))?,
                grant.epoch(),
            )
        };
        // No engine/scheduler/MM/admission guard crosses an actual worker join.
        let joined = runtime
            .join_record_no_store(root.clone(), source)
            .await
            .map_err(|e| fail(&e))?;
        let result = {
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(|e| fail(&e))?;
            let registered = self.registered_exec_mms.lock().unwrap();
            check_local()?;
            if registered.get(&owner.thread) != Some(&owner.mm) || grant.epoch() != epoch {
                return Err(NetworkRpcError::internal(
                    "no-store completion crossed actual MM/grant",
                ));
            }
            let _memory = state.memory_metadata.lock().unwrap();
            runtime
                .complete_record_no_store(
                    joined,
                    &mut engine.lock().unwrap(),
                    epoch,
                    self.global_time.lock().unwrap().as_nanos(),
                )
                .map_err(|e| fail(&e))?
        };
        self.finish_local_receive_release();
        Ok(result)
    }
}

impl GlobalState {
    pub(crate) async fn prepare_private_receive<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        call: crate::network_replay::NetworkStreamCallId,
        probe: NetworkStreamLeaseId,
        maximum: usize,
        destination: u64,
    ) -> Result<ReceiveSelection<ForegroundStorePermit<'_>>, NetworkRpcError> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        if self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("private engine absent"))?
            .lock()
            .unwrap()
            .saved_receive_policy(owner, call)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?
            .as_ref()
            .is_some_and(|policy| !policy.matches_span(maximum, destination))
        {
            return Err(NetworkRpcError::internal(
                "Record selection changed saved Read span",
            ));
        }
        let no_store = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("private engine absent"))?
            .lock()
            .unwrap()
            .record_no_store_available(owner, call, probe)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        if no_store {
            self.complete_foreground_record_no_store(tid, state, call, probe)
                .await
                .map(ReceiveSelection::NoStore)
        } else {
            self.prepare_private_receive_store(tid, state, call, probe, maximum, destination)
                .await
                .map(ReceiveSelection::Bytes)
        }
    }
}
