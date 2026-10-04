//! Finite original Close keeps its selected Normal grant. No virtual-time
//! adjustment, captured release time, or external continuation is issued here.
use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::original_connect::foreground_close::ForegroundCloseEntry;
use crate::network_replay::original_connect::foreground_close::ForegroundCloseOrigin;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

fn failed(error: impl std::fmt::Display) -> NetworkRpcError {
    NetworkRpcError::internal(error.to_string())
}
pub(crate) struct PreparedForegroundClose {
    pub(crate) origin: Arc<ForegroundCloseOrigin>,
    task: std::os::fd::OwnedFd,
    publication: crate::network_runtime::NativeCaptureRecovery,
}
impl GlobalState {
    pub(crate) fn foreground_close_policy_enabled(&self) -> bool {
        self.cfg.sequentialize_threads
            && self
                .network_engine
                .as_ref()
                .is_some_and(|engine| engine.lock().unwrap().uses_shared_mm_attempts())
    }
    fn with_foreground_close<T, U>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedMmForegroundObservation<'_>,
            &mut crate::tool_local::FileMetadata,
        ) -> Result<U, NetworkRpcError>,
    ) -> Result<U, NetworkRpcError> {
        if !self.foreground_close_policy_enabled() {
            return Err(failed(
                "finite Close requires sequential shared network policy",
            ));
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| failed("finite Close runtime absent"))?;
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(owner, lineage)
                        .map_err(failed)?;
                    self.check_native_source_task(tid, state, grant.root())?;
                    let _memory = state.memory_metadata.lock().unwrap();
                    let mut metadata = state.file_metadata.lock().unwrap();
                    let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
                    engine
                        .validate_fd_metadata(
                            owner,
                            grant.root().files(),
                            &state.file_metadata,
                            &metadata,
                        )
                        .map_err(failed)?;
                    transition(&mut engine, &grant, &mut metadata)
                })())
            })
            .map_err(failed)?
    }
    pub(crate) fn foreground_close_candidate<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<bool, NetworkRpcError> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        // Known absent/revoked provenance is an ordinary Close, even before a
        // shared lineage has formed. Invalid reader custody is still an error.
        if !self
            .network_engine
            .as_ref()
            .ok_or_else(|| failed("finite Close engine absent"))?
            .lock()
            .unwrap()
            .foreground_close_candidate(owner, read)
            .map_err(failed)?
        {
            return Ok(false);
        }
        let result = self.with_foreground_close(tid, state, |engine, grant, metadata| {
            engine.validate_fd_read_grant(owner, read).map_err(failed)?;
            let observed = metadata.observe_fd_read(read).map_err(failed)?;
            if observed.binding != read.binding
                || observed.socket != read.binding.map(|b| b.open_file)
                || read.external_grant.is_some()
            {
                return Err(failed("finite Close changed selected descriptor"));
            }
            // A valid shared child or multiple-live-member selection is an
            // ordinary Close. Corrupt custody still fails the checks above.
            if !grant.admits_initial_singleton() {
                return Ok(false);
            }
            engine
                .validate_foreground_close_birth_root(owner, read, grant.root())
                .map_err(failed)?;
            Ok(true)
        });
        crate::network_runtime::socket_birth_policy::decline_diagnostic(format_args!(
            "phase=close-census fd={} outcome={}",
            read.fd,
            match &result {
                Ok(true) => "initial-singleton",
                Ok(false) => "other-topology",
                Err(_) => "error",
            }
        ));
        result
    }
    pub(crate) async fn begin_foreground_original_close<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: crate::network_replay::NetworkFdReadAdmission,
        arguments: Arguments,
        raw: [usize; 6],
    ) -> Result<PreparedForegroundClose, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| failed("finite Close runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| failed("finite Close engine absent"))?;
        let (root, epoch) = self.with_foreground_close(tid, state, |_, grant, _| {
            Ok((grant.root().clone(), grant.epoch()))
        })?;
        let prefix = runtime
            .join_shared_foreground_prefix(root.clone(), engine, None)
            .await
            .map_err(failed)?;
        let task = runtime
            .prepare_native_capture_task(root.owner())
            .map_err(failed)?;
        let publication = self
            .native_capture_recovery()
            .ok_or_else(|| failed("finite Close recovery absent"))?;
        let origin = self.with_foreground_close(tid, state, |engine, grant, metadata| {
            if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                return Err(failed("finite Close crossed original selected turn"));
            }
            engine
                .validate_fd_read_grant(grant.owner(), &read)
                .map_err(failed)?;
            let observed = metadata.observe_fd_read(&read).map_err(failed)?;
            if observed.binding != read.binding
                || observed.socket != read.binding.map(|b| b.open_file)
            {
                return Err(failed("finite Close changed original descriptor"));
            }
            runtime
                .with_shared_attempt_prefix(&prefix, engine, |engine, physical| {
                    engine
                        .begin_foreground_close(
                            ForegroundCloseEntry {
                                read,
                                arguments,
                                raw,
                            },
                            grant,
                            &prefix,
                            physical,
                        )
                        .map_err(std::io::Error::other)
                })
                .map_err(failed)
        })?;
        // The next fallible operation is after the caller installs the exact
        // admitted Local. No second FD acquisition follows reader transfer.
        Ok(PreparedForegroundClose {
            origin,
            task,
            publication,
        })
    }
    pub(crate) async fn prepare_foreground_original_close(
        &self,
        prepared: PreparedForegroundClose,
    ) -> Result<(), NetworkRpcError> {
        self.network_runtime
            .as_ref()
            .ok_or_else(|| failed("finite Close runtime absent"))?
            .prepare_original_connect(
                prepared.origin.owner(),
                prepared.origin.admission().clone(),
                prepared.task,
                prepared.publication,
            )
            .await
            .map_err(failed)
    }
    pub(super) fn submit_foreground_original_close(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<bool, NetworkRpcError> {
        let origin = self
            .network_engine
            .as_ref()
            .ok_or_else(|| failed("finite Close engine absent"))?
            .lock()
            .unwrap()
            .foreground_close_origin(owner, admission)
            .map_err(failed)?;
        let Some(origin) = origin else {
            return Ok(false);
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| failed("finite Close runtime absent"))?;
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(owner, lineage)
                        .map_err(failed)?;
                    if self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                    {
                        return Err(failed("finite Close lost current MM"));
                    }
                    let mut engine = self.network_engine.as_ref().unwrap().lock().unwrap();
                    engine
                        .validate_foreground_close(&origin, &grant, origin.raw())
                        .map_err(failed)?;
                    engine
                        .original_connect_invoked(owner, admission)
                        .map_err(failed)?;
                    Ok(true)
                })())
            })
            .map_err(failed)?
    }
    pub(super) fn validate_foreground_close_callback<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        admission: &Admission,
        raw: [usize; 6],
    ) -> Result<(), NetworkRpcError> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let origin = self
            .network_engine
            .as_ref()
            .ok_or_else(|| failed("finite Close engine absent"))?
            .lock()
            .unwrap()
            .foreground_close_origin(owner, admission)
            .map_err(failed)?;
        let Some(origin) = origin else {
            return Ok(());
        };
        self.with_foreground_close(tid, state, |engine, grant, _| {
            engine
                .validate_foreground_close(&origin, grant, raw)
                .map_err(failed)
        })
    }
}
