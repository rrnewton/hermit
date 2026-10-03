//! Selected shared Record TX retains its original Normal turn through native
//! invocation. This module issues no source-memory proof or guest wait.
use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::shared_send::SharedRecordSend;
use crate::network_replay::shared_send::SharedRecordSendEntry;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

fn failed(error: impl std::fmt::Display) -> NetworkRpcError {
    NetworkRpcError::internal(error.to_string())
}
pub(crate) struct PreparedSharedSend {
    pub(crate) origin: Arc<SharedRecordSend>,
    task: std::os::fd::OwnedFd,
    publication: crate::network_runtime::NativeCaptureRecovery,
}
impl GlobalState {
    fn with_shared_send<T, U>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &SharedMmForegroundObservation<'_>,
            &mut crate::tool_local::FileMetadata,
        ) -> Result<U, NetworkRpcError>,
    ) -> Result<U, NetworkRpcError> {
        if self.cfg.network_trace.policy != NetworkPolicy::Record || !self.cfg.sequentialize_threads
        {
            return Err(failed(
                "shared original Sendto requires Record sequential policy",
            ));
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| failed("shared send runtime absent"))?;
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
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| failed("shared send engine absent"))?
                        .lock()
                        .unwrap();
                    engine
                        .validate_shared_initial_origin(grant.root())
                        .map_err(failed)?;
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
    pub(crate) fn shared_send_timeout<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: &crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<u64, NetworkRpcError> {
        self.with_shared_send(tid, state, |engine, grant, metadata| {
            engine
                .validate_fd_read_grant(grant.owner(), read)
                .map_err(failed)?;
            let observed = metadata.observe_fd_read(read).map_err(failed)?;
            let binding = read
                .binding
                .ok_or_else(|| failed("shared send descriptor absent"))?;
            if read.external_grant.is_some()
                || observed.binding != Some(binding)
                || observed.socket != Some(binding.open_file)
                || observed.nonblocking != Some(false)
            {
                return Err(failed(
                    "shared original Sendto requires exact blocking socket reader",
                ));
            }
            engine
                .shared_record_send_timeout(binding.open_file)
                .map_err(failed)
        })
    }
    pub(crate) async fn begin_shared_original_send<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        read: crate::network_replay::NetworkFdReadAdmission,
        arguments: Arguments,
        raw: [usize; 6],
    ) -> Result<PreparedSharedSend, NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| failed("shared send runtime absent"))?;
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| failed("shared send engine absent"))?;
        let (root, epoch) = self.with_shared_send(tid, state, |_, grant, _| {
            Ok((grant.root().clone(), grant.epoch()))
        })?;
        let prefix = runtime
            .join_shared_foreground_prefix(root.clone(), engine, None)
            .await
            .map_err(failed)?;
        // Actual PIDFD clone precedes the physical-census borrow; no recursive
        // registry lock or awaited operation occurs inside the consuming join.
        let task = runtime
            .prepare_native_capture_task(root.owner())
            .map_err(failed)?;
        let publication = self
            .native_capture_recovery()
            .ok_or_else(|| failed("shared send recovery owner absent"))?;
        let origin = self.with_shared_send(tid, state, |engine, grant, metadata| {
            if !Arc::ptr_eq(grant.root(), &root) || grant.epoch() != epoch {
                return Err(failed("shared send crossed original selected turn"));
            }
            engine
                .validate_fd_read_grant(grant.owner(), &read)
                .map_err(failed)?;
            let observed = metadata.observe_fd_read(&read).map_err(failed)?;
            if observed.binding != read.binding
                || observed.nonblocking != Some(false)
                || observed.socket != read.binding.map(|b| b.open_file)
            {
                return Err(failed("shared send changed original blocking descriptor"));
            }
            runtime
                .with_shared_attempt_prefix(&prefix, engine, |engine, admission| {
                    engine
                        .begin_shared_record_send(
                            SharedRecordSendEntry {
                                read,
                                arguments,
                                raw,
                            },
                            grant,
                            &prefix,
                            admission,
                            self.global_time.lock().unwrap().as_nanos(),
                        )
                        .map_err(std::io::Error::other)
                })
                .map_err(failed)
        })?;
        // No fallible work follows reader transfer. Caller installs this exact
        // origin in its Local before preparation's next await.
        Ok(PreparedSharedSend {
            origin,
            task,
            publication,
        })
    }
    pub(crate) async fn prepare_shared_original_send(
        &self,
        prepared: PreparedSharedSend,
    ) -> Result<(), NetworkRpcError> {
        self.network_runtime
            .as_ref()
            .ok_or_else(|| failed("shared send runtime absent"))?
            .prepare_shared_original_send(prepared.origin, prepared.task, prepared.publication)
            .await
            .map_err(failed)
    }
    pub(crate) fn validate_shared_send_callback<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        admission: &Admission,
        raw: [usize; 6],
    ) -> Result<(), NetworkRpcError> {
        self.with_shared_send(tid, state, |engine, grant, _| {
            let origin = engine
                .shared_record_send_origin(admission)
                .map_err(failed)?;
            self.network_runtime
                .as_ref()
                .unwrap()
                .check_shared_send_prepared(&origin)
                .map_err(failed)?;
            engine
                .validate_shared_record_send(
                    &origin,
                    grant,
                    raw,
                    self.global_time.lock().unwrap().as_nanos(),
                )
                .map_err(failed)
        })
    }
    /// RPC submission has no ThreadState: join the retained root/epoch and full
    /// physical census here; actual Prepared separately binds the current state.
    pub(in crate::tool_global) fn validate_shared_send_submission(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkRpcError> {
        let runtime = self
            .network_runtime
            .as_ref()
            .ok_or_else(|| failed("shared send runtime absent"))?;
        let scheduler = self.sched.lock().unwrap();
        runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                Ok((|| {
                    let grant = scheduler
                        .shared_mm_foreground_observation(owner, lineage)
                        .map_err(failed)?;
                    let mut engine = self
                        .network_engine
                        .as_ref()
                        .ok_or_else(|| failed("shared send engine absent"))?
                        .lock()
                        .unwrap();
                    let origin = engine
                        .shared_record_send_origin(admission)
                        .map_err(failed)?;
                    if self.registered_exec_mms.lock().unwrap().get(&owner.thread)
                        != Some(&owner.mm)
                    {
                        return Err(failed("shared send submission lost current MM"));
                    }
                    runtime
                        .check_shared_send_prepared(&origin)
                        .map_err(failed)?;
                    engine
                        .validate_shared_record_send(
                            &origin,
                            &grant,
                            origin.raw(),
                            self.global_time.lock().unwrap().as_nanos(),
                        )
                        .map_err(failed)?;
                    engine
                        .original_connect_invoked(owner, admission)
                        .map_err(failed)
                })())
            })
            .map_err(failed)?
    }
    pub(crate) async fn join_shared_send_close(
        &self,
        origin: &Arc<SharedRecordSend>,
    ) -> Result<(), NetworkRpcError> {
        self.network_runtime
            .as_ref()
            .ok_or_else(|| failed("shared send runtime absent"))?
            .join_shared_original_send_completion(origin)
            .await
            .map_err(failed)
    }
    pub(crate) fn publish_shared_original_send<T>(
        &self,
        tid: Tid,
        state: &crate::tool_local::ThreadState<T>,
        origin: &Arc<SharedRecordSend>,
    ) -> Result<(), NetworkRpcError> {
        self.with_shared_send(tid, state, |engine, grant, _| {
            self.network_runtime
                .as_ref()
                .unwrap()
                .with_shared_send_completion(origin, engine, |engine, completion| {
                    engine
                        .publish_shared_native_sent(
                            origin,
                            grant,
                            completion,
                            self.global_time.lock().unwrap().as_nanos(),
                        )
                        .map_err(std::io::Error::other)
                })
                .map_err(failed)
        })
    }
}

#[cfg(test)]
mod tests;
