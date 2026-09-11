use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Error;
use detcore::Detcore;
use reverie_liteinst::LiteinstBackend;
pub use reverie_liteinst::retained_guest_log::CaptureDestination;
pub use reverie_liteinst::retained_guest_log::CaptureLimits;
pub use reverie_liteinst::retained_guest_log::CaptureOptions;
use reverie_liteinst::retained_guest_log::CaptureOwner;
use reverie_liteinst::retained_guest_log::CaptureReport;
pub use reverie_liteinst::retained_guest_log::CaptureTimeouts;
pub use reverie_liteinst::retained_guest_log::DestinationProgress;
use reverie_liteinst::retained_guest_log::GuestStopReason;
use reverie_liteinst::retained_guest_log::HostProducer;
use reverie_liteinst::retained_guest_log::IssueKind;
use reverie_liteinst::retained_guest_log::LogHandle;
use reverie_liteinst::retained_guest_log::LogSink;
use reverie_liteinst::retained_guest_log::{self as logs};
use reverie_liteinst::run_evidence::RunCompletion;
use reverie_liteinst::run_evidence::RunEvidence;
use reverie_liteinst::run_evidence::RunObserver;
use reverie_liteinst::run_evidence::SnapshotUnavailable;
use reverie_liteinst::run_evidence::StdioMode;
use reverie_liteinst::run_evidence::StreamState;

use crate::Command;
use crate::DetConfig;
use crate::Output;
use crate::liteinst_bootstrap::EffectiveFilter;

pub mod startup;

type RecordFailure = Arc<dyn StdError + Send + Sync>;
type RecordObserver = dyn Fn() -> Option<RecordFailure> + Send + Sync;

#[derive(Clone, Debug)]
pub enum StdioEvidence {
    Unavailable,
    Inherited,
    Partial { stdout: Vec<u8>, stderr: Vec<u8> },
    Complete(Output),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Prepared,
    Running,
    CleaningUp,
    Returned,
    Interrupted,
}

struct State {
    stage: Stage,
    finalization_started: bool,
    observer: Option<Arc<RecordObserver>>,
    stdio: StdioEvidence,
    primary: Option<Arc<Error>>,
    runtime: Option<PathBuf>,
    guest: Option<logs::GuestReport>,
    run_observer: Option<RunObserver>,
}

struct Shared {
    handle: LogHandle,
    state: Mutex<State>,
}

impl Shared {
    fn retain_run_observer(&self, observer: RunObserver) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        if state.finalization_started || state.run_observer.is_some() {
            anyhow::bail!("run observer must be retained once before finalization");
        }
        state.run_observer = Some(observer);
        Ok(())
    }

    fn fail(&self, error: Error) -> Arc<Error> {
        let mut state = self.state.lock().unwrap();
        state.primary.get_or_insert_with(|| Arc::new(error)).clone()
    }

    fn record_failure(&self) -> Result<Option<RecordFailure>, Error> {
        let observer =
            self.state.lock().unwrap().observer.clone().ok_or_else(|| {
                anyhow::anyhow!("host RecordStatus observer has not been retained")
            })?;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer()))
            .map_err(|_| anyhow::anyhow!("host RecordStatus observer panicked"))
    }
}

pub struct Session {
    owner: CaptureOwner,
    shared: Arc<Shared>,
}

pub type NamespaceConfigurator =
    Box<dyn FnOnce(&mut DetConfig) -> Result<Box<dyn Send>, Error> + Send>;

pub struct LogInput {
    sink: Option<LogSink>,
    payload: Vec<u8>,
    shared: Arc<Shared>,
    runtime_override: Option<PathBuf>,
    entered_container: Option<startup::kernel_binding::EnteredContainer>,
    namespace_configurator: Option<NamespaceConfigurator>,
}

impl LogInput {
    /// Explicit, unactivated preparation seam for the container-side launch caller.
    /// This neither changes the command nor authorizes the owned installer.
    pub fn prepare_startup_in_current_filesystem(
        &self,
        command: &Command,
        max_image_bytes: usize,
    ) -> io::Result<startup::InterpreterInputs> {
        startup::prepare_in_current_filesystem(command, max_image_bytes)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().finalization_started = true;
    }
}

pub struct PendingRun<T> {
    result: Result<T, Arc<Error>>,
    shared: Arc<Shared>,
}

impl<T> PendingRun<T> {
    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> PendingRun<U> {
        PendingRun {
            result: self.result.map(map),
            shared: self.shared,
        }
    }
}

pub struct Evidence {
    pub capture: CaptureReport,
    pub handle: LogHandle,
    pub stage: Stage,
    pub finalization_started: bool,
    pub stdio: StdioEvidence,
    /// Incremental R evidence, independent of returned-output `stdio` above.
    /// None means no observer was installed; Busy/Poisoned are not empty output.
    /// Stream states distinguish NotStarted, Reading, EOF, interruption and inheritance.
    pub run: Option<Result<RunEvidence, SnapshotUnavailable>>,
    pub primary: Option<Arc<Error>>,
    pub record_failure: Option<RecordFailure>,
    pub record_status_retained: bool,
    pub runtime: Option<PathBuf>,
    pub guest: Option<logs::GuestReport>,
}

impl std::fmt::Debug for Evidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiteinstEvidence")
            .field("capture", &self.capture)
            .field("stage", &self.stage)
            .field("finalization_started", &self.finalization_started)
            .field("stdio", &self.stdio)
            .field("run", &self.run)
            .field("primary", &self.primary)
            .field("record_failure", &self.record_failure)
            .field("record_status_retained", &self.record_status_retained)
            .field("runtime", &self.runtime)
            .field("guest", &self.guest)
            .finish()
    }
}

fn run_completed(run: &Option<Result<RunEvidence, SnapshotUnavailable>>) -> bool {
    run.as_ref().is_some_and(|snapshot| {
        snapshot.as_ref().is_ok_and(|snapshot| {
            let expected = match snapshot.mode {
                StdioMode::Captured => StreamState::Eof,
                StdioMode::Inherited => StreamState::Inherited,
            };
            snapshot.completion == RunCompletion::Succeeded
                && snapshot.polled
                && snapshot.worker_submitted
                && snapshot.spawned
                && !snapshot.caller_cancelled
                && snapshot.first_error.is_none()
                && snapshot.cleanup_issue.is_none()
                && snapshot.wait_error.is_none()
                && snapshot.reaped
                && snapshot.wait_status.is_some()
                && snapshot.stdout.state == expected
                && snapshot.stderr.state == expected
        })
    })
}

async fn orderly_run_snapshot(
    mut snapshot: impl FnMut() -> Result<RunEvidence, SnapshotUnavailable>,
    mut stopped: impl FnMut() -> bool,
) -> Result<RunEvidence, Error> {
    loop {
        if stopped() {
            anyhow::bail!("owned exit evidence acquisition cancelled");
        }
        match snapshot() {
            Ok(run) => return Ok(run),
            Err(SnapshotUnavailable::Busy) => tokio::task::yield_now().await,
            Err(error) => anyhow::bail!("owned exit run evidence unavailable: {error:?}"),
        }
    }
}

fn verify_owned_exit(
    status: crate::ExitStatus,
    run: RunEvidence,
    capture: &CaptureReport,
) -> Result<(), Error> {
    let matches_wait = run.pid.is_some_and(|pid| pid != 0)
        && run
            .wait_status
            .is_some_and(|wait| crate::ExitStatus::from(wait) == status);
    if !matches_wait || !run_completed(&Some(Ok(run))) {
        anyhow::bail!("owned exit lacks matching completed kernel wait evidence");
    }
    let guest = &capture.guest;
    if guest.phase != logs::Phase::Complete
        || guest.run != logs::RunState::Succeeded
        || !guest.root_reaped
        || !guest.peer_closed
        || !guest.issues.is_empty()
        || !guest.rpc_issues.is_empty()
        || !capture.guest_admission.closed
        || capture.guest_admission.entrants != 0
        || capture.host.closed
        || capture.late_host_writes != 0
        || capture.omitted_issues != 0
        || capture.error.is_some()
        || capture.publication.error.is_some()
        || capture.publication.progress.output_ceiling
        || capture.publication.progress.marker_failed
    {
        anyhow::bail!("owned exit capture contains incomplete or failed evidence");
    }
    Ok(())
}

async fn owned_exit_evidence(shared: &Shared, status: crate::ExitStatus) -> Result<(), Arc<Error>> {
    let evidence = async {
        let observer = {
            let state = shared.state.lock().unwrap();
            if state.finalization_started || state.primary.is_some() {
                anyhow::bail!("owned exit session is failed or already finalizing");
            }
            state
                .run_observer
                .clone()
                .ok_or_else(|| anyhow::anyhow!("owned exit run observer was not retained"))?
        };
        let run =
            orderly_run_snapshot(|| observer.try_snapshot(), || shared.handle.stopped()).await?;
        if let Some(error) = shared.record_failure()? {
            shared.handle.issue(IssueKind::Publication, &error);
            anyhow::bail!("host record emission failed before exit cleanup: {error}");
        }
        let capture = shared
            .handle
            .capture_snapshot()
            .ok_or_else(|| anyhow::anyhow!("owned exit prepared capture was not retained"))?;
        verify_owned_exit(status, run, &capture)
    }
    .await;
    evidence.map_err(|error| shared.fail(error))
}

fn exit_requires_forced_shutdown(
    owned: bool,
    status: crate::ExitStatus,
    evidence: Result<(), Arc<Error>>,
) -> Result<bool, Arc<Error>> {
    if owned {
        evidence.map(|()| matches!(status, crate::ExitStatus::Signaled(_, _)))
    } else {
        Ok(crate::liteinst_requires_forced_shutdown(status))
    }
}

async fn complete_exit_cleanup<Global>(
    mut global: Global,
    forced: Result<bool, Arc<Error>>,
    cancel: impl std::ops::AsyncFnOnce(&mut Global),
    cleanup: impl std::ops::AsyncFnOnce(Global),
) -> Result<(), Arc<Error>> {
    if !matches!(&forced, Ok(false)) {
        cancel(&mut global).await;
    }
    cleanup(global).await;
    forced.map(|_| ())
}

pub struct FinalizedRun<T> {
    pub result: Result<T, Arc<Error>>,
    pub evidence: Evidence,
    /// Present only on identity rejection. Return this token to its original session.
    /// In that case evidence is a non-finalizing snapshot of the receiving session,
    /// with the identity error as its primary, not a status-observer check or a
    /// mutation of either session's primary failure.
    pub rejected_pending: Option<PendingRun<T>>,
}

pub fn prepare<D: CaptureDestination>(
    options: CaptureOptions,
    destination: D,
    filter: &EffectiveFilter,
) -> Result<(Session, LogInput, HostProducer), Error> {
    let payload = crate::liteinst_bootstrap::encode(&detcore::config_wire_fingerprint(), filter)?;
    let (owner, sink, host) = logs::prepared_capture(options, destination)?;
    let shared = Arc::new(Shared {
        handle: owner.handle(),
        state: Mutex::new(State {
            stage: Stage::Prepared,
            finalization_started: false,
            observer: None,
            stdio: StdioEvidence::Unavailable,
            primary: None,
            runtime: None,
            guest: None,
            run_observer: None,
        }),
    });
    Ok((
        Session {
            owner,
            shared: shared.clone(),
        },
        LogInput {
            sink: Some(sink),
            payload,
            shared,
            runtime_override: None,
            entered_container: None,
            namespace_configurator: None,
        },
        host,
    ))
}

impl Session {
    pub fn handle(&self) -> LogHandle {
        self.shared.handle.clone()
    }

    /// Retained outside the launch runtime; cloning does not retain a child or pipe.
    pub fn run_observer(&self) -> Option<RunObserver> {
        self.shared.state.lock().unwrap().run_observer.clone()
    }

    pub fn retain_record_status(
        &mut self,
        observer: impl Fn() -> Option<RecordFailure> + Send + Sync + 'static,
    ) -> Result<(), Error> {
        let mut state = self.shared.state.lock().unwrap();
        if state.finalization_started || state.stage != Stage::Prepared || state.observer.is_some()
        {
            anyhow::bail!("RecordStatus observer must be retained once before launch");
        }
        state.observer = Some(Arc::new(observer));
        Ok(())
    }

    pub fn finish<T>(&mut self, pending: PendingRun<T>, deadline: Instant) -> FinalizedRun<T> {
        if !Arc::ptr_eq(&self.shared, &pending.shared) {
            let error = Arc::new(anyhow::anyhow!("pending run belongs to another capture"));
            let state = self.shared.state.lock().unwrap();
            return FinalizedRun {
                result: Err(error.clone()),
                evidence: Evidence {
                    capture: self.handle().capture_snapshot().expect("prepared capture"),
                    handle: self.handle(),
                    stage: state.stage,
                    finalization_started: state.finalization_started,
                    stdio: state.stdio.clone(),
                    run: state.run_observer.as_ref().map(RunObserver::try_snapshot),
                    primary: Some(error),
                    record_failure: None,
                    record_status_retained: state.observer.is_some(),
                    runtime: state.runtime.clone(),
                    guest: state.guest.clone(),
                },
                rejected_pending: Some(pending),
            };
        }
        let evidence = self.finish_evidence(deadline);
        let result = match pending.result {
            Err(error) => Err(error),
            Ok(value)
                if evidence.primary.is_none()
                    && run_completed(&evidence.run)
                    && evidence.capture.qualifies()
                    && evidence.record_status_retained
                    && evidence.record_failure.is_none()
                    && evidence.stage == Stage::Returned =>
            {
                Ok(value)
            }
            Ok(_) => Err(evidence.primary.clone().unwrap_or_else(|| {
                Arc::new(anyhow::anyhow!(
                    "incomplete LiteInst execution/log evidence; not a parity candidate"
                ))
            })),
        };
        FinalizedRun {
            result,
            evidence,
            rejected_pending: None,
        }
    }

    pub fn finish_evidence(&mut self, deadline: Instant) -> Evidence {
        self.shared.state.lock().unwrap().finalization_started = true;
        let (retained, record_failure) = match self.shared.record_failure() {
            Ok(failure) => (true, failure),
            Err(error) => {
                self.shared.fail(error);
                (false, None)
            }
        };
        if let Some(error) = &record_failure {
            self.shared.handle.issue(IssueKind::Publication, error);
            self.shared
                .fail(anyhow::anyhow!("host record emission failed: {error}"));
        }
        let capture = self.owner.finish_until(deadline);
        let (retained, record_failure) = match self.shared.record_failure() {
            Ok(failure) => (retained, record_failure.or(failure)),
            Err(error) => {
                self.shared.fail(error);
                (false, record_failure)
            }
        };
        if let Some(error) = &record_failure {
            self.shared.fail(anyhow::anyhow!(
                "host record emission failed during finalization: {error}"
            ));
        }
        let state = self.shared.state.lock().unwrap();
        Evidence {
            capture,
            handle: self.handle(),
            stage: state.stage,
            finalization_started: state.finalization_started,
            stdio: state.stdio.clone(),
            run: state.run_observer.as_ref().map(RunObserver::try_snapshot),
            primary: state.primary.clone(),
            record_failure,
            record_status_retained: retained,
            runtime: state.runtime.clone(),
            guest: state.guest.clone(),
        }
    }
}

impl LogInput {
    pub fn with_runtime_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.runtime_override = Some(path.into());
        self
    }

    pub fn with_entered_container(
        mut self,
        entered: Option<startup::kernel_binding::EnteredContainer>,
    ) -> Self {
        self.entered_container = entered;
        self
    }

    pub fn with_namespace_configuration(mut self, configure: NamespaceConfigurator) -> Self {
        self.namespace_configurator = Some(configure);
        self
    }
}

impl Drop for LogInput {
    fn drop(&mut self) {
        if self.sink.is_some() {
            let mut state = self.shared.state.lock().unwrap();
            if state.finalization_started {
                return;
            }
            state.stage = Stage::Interrupted;
            drop(state);
            self.shared
                .handle
                .request_guest_stop(GuestStopReason::Cancelled);
        }
    }
}

pub fn runtime_library_path() -> io::Result<PathBuf> {
    crate::validate_liteinst_detcore_runtime_library(&runtime_candidate_path()?)
}

fn runtime_candidate_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let parent = executable
        .parent()
        .ok_or_else(|| io::Error::other("Hermit executable has no parent directory"))?;
    crate::liteinst_artifact::runtime_candidate_from(parent, |name| {
        hermit_resources::resource(name)
    })
}

struct RunGuard {
    shared: Arc<Shared>,
    complete: bool,
}

fn retain_backend_error(
    shared: &Shared,
    error: reverie_liteinst::LoggedRunError,
    capture_output: bool,
) -> (LogHandle, Arc<Error>) {
    if capture_output {
        shared.state.lock().unwrap().stdio = StdioEvidence::Partial {
            stdout: error.stdout.clone(),
            stderr: error.stderr.clone(),
        };
    }
    let handle = error.logs.clone();
    (handle, shared.fail(Error::new(error)))
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.shared.state.lock().unwrap().stage = Stage::Interrupted;
            self.shared
                .handle
                .request_guest_stop(GuestStopReason::Cancelled);
        }
    }
}

pub(crate) fn run(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    summary_json: &Option<PathBuf>,
    timeout: Option<Duration>,
    log: LogInput,
    capture_output: bool,
) -> PendingRun<Output> {
    let shared = log.shared.clone();
    {
        let mut state = shared.state.lock().unwrap();
        if state.finalization_started || state.stage != Stage::Prepared {
            let error = state
                .primary
                .get_or_insert_with(|| {
                    Arc::new(anyhow::anyhow!(
                        "LiteInst session is no longer prepared for launch"
                    ))
                })
                .clone();
            drop(state);
            return PendingRun {
                result: Err(error),
                shared,
            };
        }
        state.stage = Stage::Running;
    }
    let mut guard = RunGuard {
        shared: shared.clone(),
        complete: false,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| shared.fail(error.into()))?;
        runtime
            .block_on(crate::with_run_deadline(
                timeout,
                execute(
                    command,
                    config,
                    print_summary,
                    summary_json,
                    log,
                    capture_output,
                ),
            ))
            .map_err(|error| shared.fail(error))
    }))
    .unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        Err(shared.fail(anyhow::anyhow!(
            "LiteInst launch/cleanup unwound: {message}"
        )))
    });
    let result = match result {
        Ok(output) => {
            guard.complete = true;
            shared.state.lock().unwrap().stage = Stage::Returned;
            Ok(output)
        }
        Err(error) => Err(error),
    };
    drop(guard);
    PendingRun { result, shared }
}

async fn execute(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
    summary_json: &Option<PathBuf>,
    mut log: LogInput,
    capture_output: bool,
) -> Result<Output, Error> {
    if let Some(error) = log.shared.record_failure()? {
        anyhow::bail!("host record emission failed before launch: {error}");
    }
    let preload = match &log.runtime_override {
        Some(path) => path.clone(),
        None => runtime_candidate_path()?,
    };
    log.shared.handle.ready().await?;
    {
        let mut state = log.shared.state.lock().unwrap();
        if state.finalization_started {
            anyhow::bail!("LiteInst session was finalized before launch");
        }
        state.stdio = if capture_output {
            StdioEvidence::Unavailable
        } else {
            StdioEvidence::Inherited
        };
    }
    if capture_output {
        command.stdin(crate::output_backend_stdin()?);
    }
    command.env(
        "HERMIT_LITEINST_DETCORE_BOOTSTRAP",
        "hermit-detcore-liteinst-v1",
    );
    let launch = startup::launch::prepare(command, &preload)?;
    let owned = matches!(&launch, startup::launch::RuntimeLaunch::Private(_));
    let selected_runtime = match &launch {
        startup::launch::RuntimeLaunch::Preload(launch) => launch.source().lookup_path(),
        startup::launch::RuntimeLaunch::Private(launch) => {
            launch.runtime().image().source().lookup_path()
        }
    };
    log.shared.state.lock().unwrap().runtime = Some(selected_runtime.to_owned());
    let config = crate::prepare_backend_config(config, crate::Backend::Liteinst);
    let sink = log
        .sink
        .take()
        .ok_or_else(|| anyhow::anyhow!("LiteInst guest sink was already consumed"))?;
    let payload = std::mem::take(&mut log.payload);
    let result = match launch {
        startup::launch::RuntimeLaunch::Private(launch) => {
            let mode = if capture_output {
                StdioMode::Captured
            } else {
                StdioMode::Inherited
            };
            let (observer, future) =
                LiteinstBackend::prepare_with_owned_configuration::<Detcore, _, _>(
                    startup::launch::owned_command(*launch, log.entered_container.take()),
                    config,
                    payload,
                    sink,
                    mode,
                    startup::kernel_binding::configure_launch(log.namespace_configurator.take()),
                );
            log.shared.retain_run_observer(observer)?;
            future.await
        }
        startup::launch::RuntimeLaunch::Preload(launch) => {
            let mode = if capture_output {
                StdioMode::Captured
            } else {
                StdioMode::Inherited
            };
            let (observer, future) =
                LiteinstBackend::prepare_with_owned_command_data_and_log_sink::<Detcore, _>(
                    startup::launch::owned_preload_command(*launch)?,
                    config,
                    payload,
                    sink,
                    mode,
                );
            log.shared.retain_run_observer(observer)?;
            future.await
        }
    };
    let (output, global) = match result {
        Ok(result) => result,
        Err(error) => {
            let (handle, cause) = retain_backend_error(&log.shared, error, capture_output);
            let guest = handle.guest_finished().await;
            log.shared.state.lock().unwrap().guest = Some(guest);
            return Err(anyhow::anyhow!(
                "LiteInst run failed; primary retained: {cause}"
            ));
        }
    };
    let output = Output {
        status: output.status.into(),
        stdout: output.stdout,
        stderr: output.stderr,
    };
    {
        let mut state = log.shared.state.lock().unwrap();
        if capture_output {
            state.stdio = StdioEvidence::Complete(output.clone());
        }
        state.stage = Stage::CleaningUp;
    }
    let guest = log.shared.handle.guest_finished().await;
    log.shared.state.lock().unwrap().guest = Some(guest);
    let evidence = if owned {
        owned_exit_evidence(&log.shared, output.status).await
    } else {
        Ok(())
    };
    let forced = exit_requires_forced_shutdown(owned, output.status, evidence);
    let cleaned = complete_exit_cleanup(
        global,
        forced,
        async |global| {
            global.force_shutdown_with_error();
            global.cancel_internal_scheduler().await;
        },
        async |global| global.clean_up(print_summary, summary_json).await,
    )
    .await;
    if let Err(error) = cleaned {
        return Err(anyhow::anyhow!(
            "owned exit cleanup retained failure: {error}"
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests;
