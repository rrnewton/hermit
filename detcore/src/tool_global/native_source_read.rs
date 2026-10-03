//! Replay-only prepare / backend join / revalidate / publish source path.
//! No scheduler, memory, FD, engine or native-admission lock spans source IO.
use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::NativeSourceInterval;

mod shared_source;
use shared_source::SharedNativeSource;

struct Context {
    root: Arc<ForegroundRoot>,
    epoch: u64,
    transmitted: u64,
    read: crate::network_replay::NetworkFdReadAdmission,
    address: usize,
    length: usize,
}

/// Local and consuming: neither a copied byte vector nor a fresh Normal grant
/// can replace the original read admission and actually joined prefix.
pub(crate) struct LegacyNativeSource {
    context: Context,
    interval: NativeSourceInterval,
}

pub(crate) enum PreparedNativeSource {
    Legacy(Box<LegacyNativeSource>),
    Shared(SharedNativeSource),
}

impl PreparedNativeSource {
    pub(crate) fn address(&self) -> usize {
        match self {
            Self::Legacy(source) => source.context.address,
            Self::Shared(source) => source.address,
        }
    }
    pub(crate) fn length(&self) -> usize {
        match self {
            Self::Legacy(source) => source.context.length,
            Self::Shared(source) => source.length,
        }
    }
    pub(crate) fn retention(&self) -> Box<dyn Send + Sync> {
        match self {
            Self::Legacy(source) => source.interval.keepalive(),
            Self::Shared(source) => source.interval.keepalive(),
        }
    }
    /// A successful shared preparation transferred the selected reader into
    /// its Call. The old FinishFdRead path must never release it afterward.
    pub(crate) fn transferred_read(&self) -> bool {
        matches!(self, Self::Shared(_))
    }
}

fn internal(error: impl std::fmt::Display) -> NetworkRpcError {
    NetworkRpcError::internal(error.to_string())
}

fn check_range(address: usize, length: usize) -> Result<(), NetworkRpcError> {
    let end = address
        .checked_add(length)
        .ok_or_else(|| internal("Replay transmit source range overflow"))?;
    if address == 0 || !(1..=512).contains(&length) || address / 4096 != (end - 1) / 4096 {
        return Err(internal(
            "Replay transmit requires bounded single-page source",
        ));
    }
    Ok(())
}

impl GlobalState {
    pub(crate) async fn prepare_replay_transmit_source<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
        source: (usize, usize, u32),
    ) -> Result<PreparedNativeSource, NetworkRpcError> {
        let (address, length, flags) = source;
        if flags != libc::MSG_NOSIGNAL as u32
            && flags != (libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) as u32
        {
            return Err(internal(
                "Replay transmit requires scalar MSG_NOSIGNAL source",
            ));
        }
        check_range(address, length)?;
        if self.shared_mm_attempts_active() {
            return self
                .prepare_shared_replay_transmit_source(tid, state, read, address, length)
                .await;
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("Replay source lost actual local runtime"))?;
        let root = runtime.foreground_root(owner).map_err(internal)?;
        let (epoch, transmitted) = {
            let scheduler = self.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .map_err(internal)?;
            self.check_native_source_task(tid, state, &root)?;
            if self.cfg.network_trace.policy != NetworkPolicy::Replay
                || read.publication.permit.files != root.files()
            {
                return Err(internal("Replay source changed original policy/files"));
            }
            let _memory = state.memory_metadata.lock().unwrap();
            let mut metadata = state.file_metadata.lock().unwrap();
            let engine = self
                .network_engine
                .as_ref()
                .ok_or_else(|| internal("Replay source lost engine"))?
                .lock()
                .unwrap();
            let open_file = engine
                .validate_replay_transmit_read(owner, read, &state.file_metadata, &mut metadata)
                .map_err(internal)?;
            if engine
                .transmit_stream_read_limit(open_file, length)
                .map_err(internal)?
                != length
            {
                return Err(internal("Replay source changed selected output prefix"));
            }
            (
                grant.epoch(),
                engine.replay_transmit_offset(open_file).map_err(internal)?,
            )
        };
        let context = Context {
            root,
            epoch,
            transmitted,
            read: read.clone(),
            address,
            length,
        };
        let prefix = runtime
            .join_foreground_prefix(context.root.clone())
            .await
            .map_err(internal)?;
        let interval = self.with_native_source_authority(tid, state, &context, |_, _| {
            runtime
                .reserve_replay_source_interval(&prefix)
                .map_err(internal)
        })?;
        Ok(PreparedNativeSource::Legacy(Box::new(LegacyNativeSource {
            context,
            interval,
        })))
    }

    pub(in crate::tool_global) fn check_native_source_task<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        root: &Arc<ForegroundRoot>,
    ) -> Result<(), NetworkRpcError> {
        let owner = root.owner();
        if !self.cfg.sequentialize_threads
            || state.dettid != owner.thread
            || state.mm_id != owner.mm
            || tid.as_raw() != root.thread()
            || state.detpid != Some(root.logical_process())
            || !root.matches_memory(&state.memory_metadata)
            || !root.matches_metadata(&state.file_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(internal("Replay transmit changed actual task/MM/metadata"));
        }
        Ok(())
    }

    // The callback neither awaits nor performs IO; the original epoch is
    // rechecked rather than replacing it with whichever grant is current.
    fn with_native_source_authority<T, R>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        context: &Context,
        commit: impl FnOnce(&mut NetworkReplayEngine, OpenFileId) -> Result<R, NetworkRpcError>,
    ) -> Result<R, NetworkRpcError> {
        let owner = context.root.owner();
        let scheduler = self.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &context.root)
            .map_err(internal)?;
        self.check_native_source_task(tid, state, &context.root)?;
        if grant.epoch() != context.epoch {
            return Err(internal("Replay transmit crossed current Normal grant"));
        }
        let _memory = state.memory_metadata.lock().unwrap();
        let mut metadata = state.file_metadata.lock().unwrap();
        let mut engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| internal("Replay source lost engine"))?
            .lock()
            .unwrap();
        if self.cfg.network_trace.policy != NetworkPolicy::Replay
            || context.read.publication.permit.files != context.root.files()
        {
            return Err(internal("Replay source changed original policy/files"));
        }
        let open_file = engine
            .validate_replay_transmit_read(
                owner,
                &context.read,
                &state.file_metadata,
                &mut metadata,
            )
            .map_err(internal)?;
        if engine.replay_transmit_offset(open_file).map_err(internal)? != context.transmitted
            || engine
                .transmit_stream_read_limit(open_file, context.length)
                .map_err(internal)?
                != context.length
        {
            return Err(internal("Replay source changed selected output prefix"));
        }
        commit(&mut engine, open_file)
    }

    pub(crate) fn finish_replay_transmit_source<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        prepared: PreparedNativeSource,
        bytes: Vec<u8>,
    ) -> Result<NetworkStreamTransmit, NetworkRpcError> {
        if bytes.len() != prepared.length() {
            return Err(internal("Replay source changed exact length"));
        }
        let prepared = match prepared {
            PreparedNativeSource::Legacy(prepared) => prepared,
            PreparedNativeSource::Shared(prepared) => {
                return self.finish_shared_replay_transmit_source(tid, state, prepared, bytes);
            }
        };
        self.with_native_source_authority(tid, state, &prepared.context, |engine, open_file| {
            self.network_runtime
                .as_ref()
                .ok_or_else(|| internal("Replay source lost runtime"))?
                .with_source_interval(&prepared.interval, || {
                    Ok(engine.transmit_stream(open_file, &bytes))
                })
                .map_err(internal)?
                .map(|outcome| match outcome {
                    StreamTransmitOutcome::Accepted(count) => {
                        NetworkStreamTransmit::Accepted(count)
                    }
                    StreamTransmitOutcome::Error(errno) => NetworkStreamTransmit::Error(errno),
                })
                .map_err(|error| {
                    NetworkRpcError::from_engine(
                        NetworkPolicy::Replay,
                        NetworkFailurePhase::Transmit,
                        error,
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_source_range_uses_only_bounded_selected_prefix() {
        assert!(check_range(4093, 3).is_ok());
        assert!(check_range(4093, 4).is_err());
        assert!(check_range(4096, 512).is_ok());
        for (address, length) in [(0, 1), (4096, 0), (4096, 513), (usize::MAX, 2)] {
            assert!(check_range(address, length).is_err());
        }
    }
}
