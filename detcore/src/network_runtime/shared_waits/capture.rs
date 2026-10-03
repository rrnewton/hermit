//! One actual pin acquisition on the original shared Call. The engine owns
//! its pre-effect entry before submission; a real JoinHandle issues completion.
use super::*;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::NetworkStreamPinOutcome;
use crate::network_replay::shared_waits::SharedCaptureOrigin;
use crate::network_replay::shared_waits::SharedCaptureSubmission;
use crate::network_runtime::original_installation::FileIdentity;

impl RuntimeShared {
    // Shared implementation of the unchanged legacy capture operation. Both
    // callers retain the worker/run owner before permitting this body to run.
    pub(in crate::network_runtime) fn perform_native_stream_capture(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        capture: impl FnOnce() -> std::io::Result<OwnedFd>,
        identity: Option<FileIdentity>,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<NetworkStreamPinOutcome> {
        let shared = self;
        let result = (|| {
            let pin = match capture() {
                Ok(pin) => pin,
                Err(error) => {
                    // The production closure is exactly pidfd_getfd. An
                    // internal failure without Linux errno is UNKNOWN.
                    let errno = error.raw_os_error().ok_or(error)?;
                    shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .capture_failed(owner, call, errno)?;
                    return Ok(crate::network_replay::NetworkStreamPinOutcome::Failed(
                        errno,
                    ));
                }
            };
            let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFD) };
            let error = (flags < 0).then(std::io::Error::last_os_error);
            {
                let mut calls = shared.native_streams.lock().unwrap();
                match identity {
                    Some(identity) => calls.capture_authenticated(owner, call, pin, identity)?,
                    None => calls.capture(owner, call, pin)?,
                }
                calls.retain_capture_publication(owner, call, recovery.clone())?;
            }
            if let Some(error) = error {
                return Err(error);
            }
            if flags & libc::FD_CLOEXEC == 0 {
                return Err(std::io::Error::other("native stream capture lacks CLOEXEC"));
            }
            Ok(crate::network_replay::NetworkStreamPinOutcome::Acquired)
        })();
        // This executes even when the RPC receiver has been canceled. A
        // known acquisition is still distinct from a known physical close.
        let retired = recovery.retire_known(shared, owner, call);
        match (result, retired) {
            (Ok(value), Ok(_)) => Ok(value),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(primary), Err(cleanup)) => Err(std::io::Error::other(format!(
                "{primary}; capture retirement: {cleanup}"
            ))),
        }
    }
}

/// Only an actual successful JoinHandle produces this callback value. A known
/// failed capture is retained too; it cannot construct ConfirmedSharedCapture.
#[derive(Debug)]
pub(crate) struct JoinedSharedCapture {
    origin: Arc<SharedCaptureOrigin>,
}
impl JoinedSharedCapture {
    pub(crate) fn outcome(&self) -> NetworkStreamPinOutcome {
        self.origin.joined().expect("actual joined issuer").1
    }
}

pub(crate) struct ConfirmedSharedCapture<'a> {
    origin: &'a Arc<SharedCaptureOrigin>,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl ConfirmedSharedCapture<'_> {
    pub(crate) fn origin(&self) -> &Arc<SharedCaptureOrigin> {
        self.origin
    }
}

impl NetworkRuntimeResources {
    pub(crate) async fn capture_shared_wait(
        &self,
        submission: SharedCaptureSubmission,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<JoinedSharedCapture> {
        let origin = submission.into_origin();
        let task = {
            let physical = self.shared.physical.lock().unwrap();
            let lineage = physical.shared_foreground_lineage(origin.owner())?;
            if !Arc::ptr_eq(lineage.root(), origin.root()) {
                return Err(std::io::Error::other(
                    "shared capture changed original physical root",
                ));
            }
            physical.get(origin.owner())?.as_fd().try_clone_to_owned()?
        };
        let fd = origin.fd();
        self.capture_shared_wait_with(origin, recovery, move || capture_socket(&task, fd))
            .await
    }

    async fn capture_shared_wait_with(
        &self,
        origin: Arc<SharedCaptureOrigin>,
        recovery: NativeCaptureRecovery,
        capture: impl FnOnce() -> std::io::Result<OwnedFd> + Send + 'static,
    ) -> std::io::Result<JoinedSharedCapture> {
        {
            let engine = recovery.engine().lock().unwrap();
            let peers = engine
                .validate_shared_record_capture(&origin)
                .map_err(std::io::Error::other)?;
            let workers = self.shared.native_workers.lock().unwrap();
            self.check_shared_capture_workers(&origin, &workers, false)?;
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .require_shared_capture(&peers, &origin, false)?;
            origin.claim_submission()?;
        }
        // The worker waits until its actual JoinHandle is retained in the
        // runtime. Cancellation/arm refusal cannot erase the latched Call.
        let (permit, entered) = std::sync::mpsc::channel();
        let shared = self.shared.clone();
        let retained = origin.clone();
        let worker_recovery = recovery.clone();
        let (worker, receive) = self.shared.start_native_worker(
            tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?,
            move || {
                entered
                    .recv()
                    .map_err(|_| std::io::Error::other("shared capture was not armed"))?;
                shared.perform_native_stream_capture(
                    retained.owner(),
                    retained.call(),
                    capture,
                    Some(retained.identity()),
                    worker_recovery,
                )
            },
        )?;
        {
            let engine = recovery.engine().lock().unwrap();
            let peers = engine
                .validate_shared_record_capture(&origin)
                .map_err(std::io::Error::other)?;
            let workers = self.shared.native_workers.lock().unwrap();
            self.check_shared_capture_workers(&origin, &workers, true)?;
            if workers.tasks.len() != 1 || !Arc::ptr_eq(&workers.tasks[0], &worker) {
                return Err(std::io::Error::other(
                    "shared capture changed exact submitted worker",
                ));
            }
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .require_shared_capture(&peers, &origin, false)?;
            permit
                .send(())
                .map_err(|_| std::io::Error::other("shared capture lost its held worker"))?;
        }
        let outcome = receive.await;
        let joined = self.shared.join_native_worker_receipt(&worker).await?;
        let outcome = outcome.map_err(std::io::Error::other)??;
        origin.retain_join(joined, outcome)?;
        Ok(JoinedSharedCapture { origin })
    }

    fn check_shared_capture_workers(
        &self,
        origin: &Arc<SharedCaptureOrigin>,
        workers: &NativeWorkers,
        submitted: bool,
    ) -> std::io::Result<()> {
        let prefix = origin.prefix();
        let expected = prefix
            .prefix
            .generation
            .checked_add(u64::from(submitted))
            .ok_or_else(|| std::io::Error::other("shared capture worker generation overflow"))?;
        if !prefix.prefix.shared.ptr_eq(&Arc::downgrade(&self.shared))
            || prefix.selected.is_some()
            || workers.submission_generation != expected
            || workers.closed
            || workers.copy_exclusion.is_some()
            || workers.source_read_active()
            || (!submitted && !workers.tasks.is_empty())
            || !origin.root().is_current(origin.owner())
            || !origin.root().has_shared_mm_history()
        {
            return Err(std::io::Error::other(
                "shared capture changed its original runtime prefix",
            ));
        }
        if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        Ok(())
    }

    /// The caller holds the current scheduler/complete physical lineage. Keep
    /// the actual native population borrowed until the exact engine commit.
    pub(crate) fn with_shared_capture_completion<T>(
        &self,
        joined: &JoinedSharedCapture,
        engine: &mut NetworkReplayEngine,
        transition: impl FnOnce(
            &mut NetworkReplayEngine,
            &ConfirmedSharedCapture<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let origin = &joined.origin;
        let (receipt, outcome) = origin
            .joined()
            .ok_or_else(|| std::io::Error::other("shared capture lost actual join"))?;
        receipt.validate_runtime(&self.shared)?;
        if *outcome != NetworkStreamPinOutcome::Acquired {
            return Err(std::io::Error::other(
                "shared failed capture requires physical recovery",
            ));
        }
        let peers = engine
            .validate_shared_record_capture(origin)
            .map_err(std::io::Error::other)?;
        let workers = self.shared.native_workers.lock().unwrap();
        self.check_shared_capture_workers(origin, &workers, true)?;
        if !workers.tasks.is_empty() {
            return Err(std::io::Error::other(
                "shared capture still owns unjoined native workers",
            ));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        calls.require_shared_capture(&peers, origin, true)?;
        transition(
            engine,
            &ConfirmedSharedCapture {
                origin,
                _workers: &workers,
                _calls: &calls,
            },
        )
    }
}

#[cfg(test)]
impl NetworkRuntimeResources {
    /// Supply only the native pidfd_getfd result. Submission custody, actual
    /// worker execution/join and runtime/Call matching stay production code.
    pub(crate) async fn controlled_shared_capture_with(
        &self,
        submission: SharedCaptureSubmission,
        recovery: NativeCaptureRecovery,
        capture: impl FnOnce() -> std::io::Result<OwnedFd> + Send + 'static,
    ) -> std::io::Result<JoinedSharedCapture> {
        self.capture_shared_wait_with(submission.into_origin(), recovery, capture)
            .await
    }
}
