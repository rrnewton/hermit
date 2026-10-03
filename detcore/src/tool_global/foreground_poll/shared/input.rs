//! Original Poll input is captured by the backend under its complete native
//! hold and true worker join. These copied operands confer no memory authority.
use reverie::syscalls::SyscallInfo;

use super::*;

#[derive(Debug)]
pub(super) struct InputCustody {
    pub(super) owner: NetworkStreamOwner,
    pub(super) root: Arc<crate::network_runtime::ForegroundRoot>,
    pub(super) raw: (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs),
    pub(super) epoch: u64,
    pub(super) started: LogicalTime,
}

/// Only finish_shared_poll_input constructs production tokens. FD admission
/// follows capture; the returned descriptor is never itself an FD capability.
pub(crate) struct CapturedSharedPollInput {
    pub(super) custody: Arc<InputCustody>,
    pub(super) input: reverie::syscalls::OriginalPollInput,
    pub(super) deadline: LogicalTime,
}
impl CapturedSharedPollInput {
    pub(crate) fn input(&self) -> reverie::syscalls::OriginalPollInput {
        self.input
    }
    pub(crate) fn original(&self) -> reverie::syscalls::Syscall {
        reverie::syscalls::Syscall::from_raw(self.custody.raw.0, self.custody.raw.1)
    }
}

/// Owns the exact exclusion across the backend call without borrowing Guest.
pub(crate) struct PreparedSharedPollInput {
    custody: Arc<InputCustody>,
    interval: Arc<crate::network_runtime::NativeSourceInterval>,
}
impl PreparedSharedPollInput {
    pub(crate) fn original(&self) -> reverie::syscalls::Syscall {
        reverie::syscalls::Syscall::from_raw(self.custody.raw.0, self.custody.raw.1)
    }
    pub(crate) fn retention(&self) -> Box<dyn Send + Sync> {
        Box::new((self.custody.clone(), self.interval.clone()))
    }
}

impl GlobalState {
    pub(crate) async fn prepare_shared_poll_input<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        original: reverie::syscalls::Syscall,
    ) -> Result<PreparedSharedPollInput, NetworkRpcError> {
        let raw = original.into_parts();
        if raw.0 != reverie::syscalls::Sysno::poll
            || raw.1.arg1 != 1
            || raw.1.arg0 == 0
            || (raw.1.arg2 as i32) < 0
        {
            return Err(internal(
                "shared Poll requires its finite original one-row profile",
            ));
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll input lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Poll input lost engine"))?;
        let owner = NetworkStreamOwner {
            thread: guest.thread_state().dettid,
            mm: guest.thread_state().mm_id,
        };
        let custody = {
            let scheduler = self.sched.lock().unwrap();
            runtime
                .with_shared_foreground_lineage(owner, |lineage| {
                    let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                    self.check_native_source_task(guest.tid(), guest.thread_state(), grant.root())
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    let engine = engine.lock().unwrap();
                    if !engine.uses_shared_mm_attempts()
                        || !matches!(
                            engine.mode(),
                            crate::network_replay::NetworkEngineMode::Record
                                | crate::network_replay::NetworkEngineMode::Replay
                        )
                    {
                        return Err(std::io::Error::other(
                            "shared Poll input changed closed policy",
                        ));
                    }
                    Ok(Arc::new(InputCustody {
                        owner,
                        root: grant.root().clone(),
                        raw,
                        epoch: grant.epoch(),
                        started: self.global_time.lock().unwrap().as_nanos(),
                    }))
                })
                .map_err(internal)?
        };
        let prefix = runtime
            .join_shared_foreground_prefix(custody.root.clone(), engine, None)
            .await
            .map_err(internal)?;
        let interval = {
            let state = guest.thread_state();
            let scheduler = self.sched.lock().unwrap();
            runtime
                .with_shared_foreground_lineage(owner, |lineage| {
                    let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                    self.check_native_source_task(guest.tid(), state, grant.root())
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    if !Arc::ptr_eq(grant.root(), &custody.root) || grant.epoch() != custody.epoch {
                        return Err(std::io::Error::other(
                            "shared Poll input crossed original grant",
                        ));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = engine.lock().unwrap();
                    engine
                        .validate_fd_metadata(
                            owner,
                            custody.root.files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(std::io::Error::other)?;
                    // This existing reservation excludes new H native workers.
                    // The callback transfers no reader, Call, or guest authority.
                    runtime
                        .prepare_shared_poll_input(&prefix, lineage, &mut engine)
                        .map(Arc::new)
                })
                .map_err(internal)?
        };
        Ok(PreparedSharedPollInput { custody, interval })
    }

    pub(crate) fn finish_shared_poll_input<
        T: crate::RecordOrReplay,
        G: reverie::Guest<crate::Detcore<T>>,
    >(
        &self,
        guest: &G,
        prepared: PreparedSharedPollInput,
        input: reverie::syscalls::OriginalPollInput,
    ) -> Result<CapturedSharedPollInput, NetworkRpcError> {
        let PreparedSharedPollInput { custody, interval } = prepared;
        let owner = custody.owner;
        let raw = custody.raw;
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("shared Poll input completion lost runtime"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("shared Poll input completion lost engine"))?;
        {
            let scheduler = self.sched.lock().unwrap();
            runtime
                .with_shared_foreground_lineage(owner, |lineage| {
                    let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
                    self.check_native_source_task(guest.tid(), guest.thread_state(), grant.root())
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    if !Arc::ptr_eq(grant.root(), &custody.root)
                        || grant.epoch() != custody.epoch
                        || input.fd < 0
                        || input.events != libc::POLLIN
                        || input.timeout_millis < 0
                        || input.timeout_millis != raw.1.arg2 as i32
                    {
                        return Err(std::io::Error::other(
                            "shared Poll capture changed original profile/grant",
                        ));
                    }
                    let state = guest.thread_state();
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = engine.lock().unwrap();
                    engine
                        .validate_fd_metadata(
                            owner,
                            custody.root.files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(std::io::Error::other)?;
                    runtime
                        .with_shared_poll_input_interval(&interval, lineage, &mut engine, || Ok(()))
                })
                .map_err(internal)?;
        }
        let nanos = u64::try_from(input.timeout_millis)
            .map_err(internal)?
            .checked_mul(1_000_000)
            .ok_or_else(|| internal("shared Poll timeout overflow"))?;
        let deadline = custody
            .started
            .as_nanos()
            .checked_add(nanos)
            .filter(|end| *end != LogicalTime::INDEFINITE.as_nanos())
            .map(LogicalTime::from_nanos)
            .ok_or_else(|| internal("shared Poll original deadline overflow"))?;
        drop(interval);
        Ok(CapturedSharedPollInput {
            custody,
            input,
            deadline,
        })
    }
}
