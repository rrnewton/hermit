//! Advisory selection followed by exact current shared-Call attachment. Reading
//! maps selects a backend; it grants no source authority and has no fallback.
use super::*;
impl GlobalState {
    pub(crate) fn prepare_replay_executable_source<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        prepared: &mut PreparedNativeSource,
    ) -> Result<Option<Box<dyn reverie::syscalls::ExecutableSourceArmer>>, NetworkRpcError> {
        let PreparedNativeSource::Shared(source) = prepared else {
            return Ok(None);
        };
        if source.executable.is_some() {
            return Err(internal("source backend selection repeated"));
        }
        // Pure advisory observation. R later authenticates the actual MM/VMA
        // and permissions under its complete physical hold. Neither an inode
        // nor a pathname supplies backing authority; mapping changes refuse.
        let process = procfs::process::Process::new(source.root.thread()).map_err(internal)?;
        let maps = process.maps().map_err(internal)?;
        let end = source
            .address
            .checked_add(source.length)
            .ok_or_else(|| internal("source range overflow"))?;
        let mut matching = maps
            .iter()
            .filter(|m| m.address.0 <= source.address as u64 && end as u64 <= m.address.1);
        let map = matching
            .next()
            .ok_or_else(|| internal("source advisory VMA absent"))?;
        if matching.next().is_some() {
            return Err(internal("source advisory VMA ambiguous"));
        }
        if map.inode == 0 {
            return Ok(None);
        }
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| internal("executable source lost runtime"))?;
        // Duplicate registered PIDFD outside scheduler/lineage locks. Every
        // subsequent error remains on the existing transferred Transmit Call.
        let capture = runtime
            .prepare_executable_capture(
                source.root.clone(),
                source.call,
                source.address,
                source.length,
            )
            .map_err(internal)?;
        let owner = source.root.owner();
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(owner, lineage)
                        .map_err(internal)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    if self.cfg.network_trace.policy != NetworkPolicy::Replay
                        || grant.epoch() != source.epoch
                        || !Arc::ptr_eq(grant.root(), &source.root)
                    {
                        return Err(internal("executable source changed original grant/root"));
                    }
                    let _memory = state.memory_metadata.lock().unwrap();
                    let metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| internal("executable source lost engine"))?
                        .lock()
                        .unwrap();
                    engine
                        .validate_fd_metadata(
                            owner,
                            source.root.files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(internal)?;
                    runtime
                        .with_shared_source_interval(
                            &source.interval,
                            &mut engine,
                            source.call,
                            |engine| {
                                engine
                                    .attach_shared_executable_capture(
                                        source.call,
                                        &grant,
                                        &source.prefix,
                                        source.address,
                                        capture.clone(),
                                    )
                                    .map_err(std::io::Error::other)
                            },
                        )
                        .map_err(internal)?;
                    Ok(())
                })())
            })
            .map_err(internal)??;
        source.executable = Some(capture.clone());
        Ok(Some(capture.armer()))
    }
}
