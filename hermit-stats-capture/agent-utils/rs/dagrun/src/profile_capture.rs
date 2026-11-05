//! Opt-in profiler captures at a step's model-selected parallelism width.
//!
//! Scaling trials remain uninstrumented estimator input.  This module runs separate, isolated
//! `perf record` and/or centred `wprof` flight-recorder trials at the model's economic plateau and
//! retains their private artifacts plus a language-neutral manifest. Every callback request and
//! manifest trial is permanently marked `included_in_model: false`.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::estimates::StepSpeedup;

/// Stable capture-manifest schema identifier.
pub const PROFILE_CAPTURE_MANIFEST_SCHEMA: &str = "dagrun-profile-capture-v1";
/// Default duration of one centred wprof window.
pub const DEFAULT_WPROF_WINDOW_S: f64 = 0.4;
const MAX_DIAGNOSTIC_CHARS: usize = 4096;
const WPROF_READY_PREFIX: &str = "Running in flight recorder mode";
const WPROF_PID_PREFIX: &str = "DAGRUN_WPROF_PID=";
const WPROF_GATE_TOKEN: &str = "DAGRUN_WPROF_GO";
/// Hidden subprocess mode used to send one signal through a pidfd inherited as stdin.
#[doc(hidden)]
pub const PROFILE_PIDFD_SIGNAL_COMMAND: &str = "__profile-capture-pidfd-signal";
/// Hidden subprocess mode used to signal one safely owned profiler process group.
#[doc(hidden)]
pub const PROFILE_GROUP_SIGNAL_COMMAND: &str = "__profile-capture-group-signal";
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Supported expensive profiler modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaptureKind {
    /// Linux perf record data.
    Perf,
    /// Wprof data plus its Perfetto trace.
    Wprof,
}

impl CaptureKind {
    /// Stable manifest spelling.
    pub fn value(self) -> &'static str {
        match self {
            Self::Perf => "perf",
            Self::Wprof => "wprof",
        }
    }
}

/// Stable states written to capture manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    /// Capture setup or trials are still running.
    Running,
    /// Every requested and available capture completed.
    Complete,
    /// At least one preflight or trial failed.
    Failed,
    /// A requested capture was intentionally skipped.
    Skipped,
}

impl CaptureState {
    /// Stable manifest spelling.
    pub fn value(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Economic width selected from an uninstrumented scaling model.
#[derive(Debug, Clone, PartialEq)]
pub struct SweetSpotSelection {
    /// Fully-qualified step tag.
    pub step: String,
    /// Stable digest of the command and its width-control channels.
    pub workload_digest: String,
    /// Recommended worker width.
    pub inner_jobs: i64,
    /// Raw wall time used to centre the next instrumented trial.
    pub expected_wall_s: f64,
    /// Narrowest measured width in the fitted curve.
    pub baseline_inner_jobs: i64,
    /// Fitted speedup at `inner_jobs`.
    pub speedup: f64,
    /// Contention-adjusted model wall at `inner_jobs`.
    pub model_wall_s: f64,
    /// Raw median wall at `inner_jobs`, when measured.
    pub raw_wall_s: Option<f64>,
    /// Stable provenance for how the width was chosen.
    pub source: String,
    /// Commit whose profile model selected this width.
    pub git_sha: String,
}

impl SweetSpotSelection {
    /// Validate the manifest invariants for a selection.
    pub fn validate(&self) -> Result<(), String> {
        if self.step.is_empty() {
            return Err("sweet-spot selection needs a step tag".to_string());
        }
        if self.inner_jobs < 1 || self.baseline_inner_jobs < 1 {
            return Err("sweet-spot widths must be positive".to_string());
        }
        for (name, value) in [
            ("expected_wall_s", self.expected_wall_s),
            ("speedup", self.speedup),
            ("model_wall_s", self.model_wall_s),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be finite and positive"));
            }
        }
        if self
            .raw_wall_s
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err("raw_wall_s must be finite and positive when present".to_string());
        }
        Ok(())
    }

    /// Encode the portable manifest object.
    pub fn to_json(&self) -> Value {
        json!({
            "step": self.step,
            "workload_digest": self.workload_digest,
            "inner_jobs": self.inner_jobs,
            "expected_wall_s": self.expected_wall_s,
            "baseline_inner_jobs": self.baseline_inner_jobs,
            "speedup": self.speedup,
            "model_wall_s": self.model_wall_s,
            "raw_wall_s": self.raw_wall_s,
            "source": self.source,
            "git_sha": self.git_sha,
        })
    }
}

/// Turn a fitted speedup recommendation into profiler-capture provenance.
pub fn select_capture_sweet_spot(
    speedup: &StepSpeedup,
    workload_digest: &str,
    git_sha: &str,
) -> Result<SweetSpotSelection, String> {
    let level = speedup
        .levels
        .iter()
        .find(|level| level.inner_jobs == speedup.recommended_inner_jobs)
        .ok_or_else(|| {
            format!(
                "recommended width {} is absent from the scaling curve for {:?}",
                speedup.recommended_inner_jobs, speedup.step
            )
        })?;
    let selection = SweetSpotSelection {
        step: speedup.step.clone(),
        workload_digest: workload_digest.to_string(),
        inner_jobs: speedup.recommended_inner_jobs,
        expected_wall_s: level.raw_wall_s.unwrap_or(level.wall_s),
        baseline_inner_jobs: speedup.baseline_inner_jobs,
        speedup: level.speedup,
        model_wall_s: level.wall_s,
        raw_wall_s: level.raw_wall_s,
        source: "scaling-model-economic-plateau".to_string(),
        git_sha: git_sha.to_string(),
    };
    selection.validate()?;
    Ok(selection)
}

/// A centred half-open profiling interval relative to trial launch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureWindow {
    /// Operator-requested duration before clipping.
    pub requested_duration_s: f64,
    /// Offset from trial launch to capture activation.
    pub start_offset_s: f64,
    /// Actual profiler-active duration.
    pub duration_s: f64,
    /// Whether a short expected trial forced duration clipping.
    pub clipped: bool,
}

impl CaptureWindow {
    /// Offset from trial launch at which capture should stop.
    pub fn end_offset_s(self) -> f64 {
        self.start_offset_s + self.duration_s
    }

    /// Encode the portable manifest object.
    pub fn to_json(self) -> Value {
        json!({
            "requested_duration_s": self.requested_duration_s,
            "start_offset_s": self.start_offset_s,
            "duration_s": self.duration_s,
            "end_offset_s": self.end_offset_s(),
            "clipped": self.clipped,
        })
    }
}

/// Centre a capture while retaining ten-percent startup and shutdown margins when possible.
pub fn centered_capture_window(
    expected_wall_s: f64,
    requested_duration_s: f64,
) -> Result<CaptureWindow, String> {
    if !expected_wall_s.is_finite() || expected_wall_s <= 0.0 {
        return Err("expected wall time must be finite and positive".to_string());
    }
    if !requested_duration_s.is_finite() || requested_duration_s <= 0.0 {
        return Err("capture duration must be finite and positive".to_string());
    }
    let maximum = expected_wall_s * 0.8;
    let actual = requested_duration_s.min(maximum);
    Ok(CaptureWindow {
        requested_duration_s,
        start_offset_s: (expected_wall_s - actual) / 2.0,
        duration_s: actual,
        clipped: actual < requested_duration_s,
    })
}

/// Opt-in profiler-capture policy.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureConfig {
    /// Profile store beneath which `captures/` is created.
    pub output_dir: PathBuf,
    /// Request one perf trial.
    pub capture_perf: bool,
    /// Number of separate wprof-window trials.
    pub wprof_windows: i64,
    /// Optional perf active-window duration; absent means 80% of expected wall.
    pub perf_window_s: Option<f64>,
    /// Duration of each wprof window.
    pub wprof_window_s: f64,
    /// Explicit privilege prefix, normally empty or `sudo -n`.
    pub sudo: Vec<String>,
    /// Requested perf executable.
    pub perf_binary: String,
    /// Requested wprof executable.
    pub wprof_binary: String,
    /// Extra perf-record argv tokens.
    pub perf_args: Vec<String>,
    /// Extra wprof argv tokens.
    pub wprof_args: Vec<String>,
    /// Maximum duration of a real preflight probe.
    pub preflight_timeout_s: f64,
    /// Maximum wait for each perf FIFO transition acknowledgement.
    pub control_ack_timeout_s: f64,
    /// Maximum wait for wprof to announce that flight-recorder preparation is complete.
    pub wprof_ready_timeout_s: f64,
    /// Grace before an unresponsive profiler process is killed.
    pub profiler_exit_grace_s: f64,
}

impl CaptureConfig {
    /// Construct the default opt-in policy for a profile store.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            capture_perf: false,
            wprof_windows: 0,
            perf_window_s: None,
            wprof_window_s: DEFAULT_WPROF_WINDOW_S,
            sudo: Vec::new(),
            perf_binary: "perf".to_string(),
            wprof_binary: "wprof".to_string(),
            perf_args: vec!["--call-graph".to_string(), "dwarf".to_string()],
            wprof_args: Vec::new(),
            preflight_timeout_s: 10.0,
            control_ack_timeout_s: 2.0,
            wprof_ready_timeout_s: 10.0,
            profiler_exit_grace_s: 2.0,
        }
    }

    /// Validate all duration, count, and executable-name fields.
    pub fn validate(&self) -> Result<(), String> {
        if self.wprof_windows < 0 {
            return Err("wprof_windows must be non-negative".to_string());
        }
        if self
            .perf_window_s
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err("perf_window_s must be finite and positive when present".to_string());
        }
        for (name, value) in [
            ("wprof_window_s", self.wprof_window_s),
            ("preflight_timeout_s", self.preflight_timeout_s),
            ("control_ack_timeout_s", self.control_ack_timeout_s),
            ("wprof_ready_timeout_s", self.wprof_ready_timeout_s),
            ("profiler_exit_grace_s", self.profiler_exit_grace_s),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be finite and positive"));
            }
        }
        if self.perf_binary.is_empty() || self.wprof_binary.is_empty() {
            return Err("profiler binary names must be non-empty".to_string());
        }
        Ok(())
    }

    /// Requested profiler kinds, in stable manifest order.
    pub fn requested_kinds(&self) -> Vec<CaptureKind> {
        let mut kinds = Vec::new();
        if self.capture_perf {
            kinds.push(CaptureKind::Perf);
        }
        if self.wprof_windows > 0 {
            kinds.push(CaptureKind::Wprof);
        }
        kinds
    }
}

/// Availability and a real minimal-capture probe for one profiler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPreflight {
    /// Profiler kind probed.
    pub kind: CaptureKind,
    /// Executable spelling requested by the operator.
    pub requested_binary: String,
    /// Resolved executable path, when found.
    pub resolved_binary: Option<String>,
    /// Whether the probe exited successfully and produced required artifacts.
    pub usable: bool,
    /// Probe return code, or absent when it could not execute.
    pub returncode: Option<i32>,
    /// First line of version output.
    pub version: String,
    /// Bounded combined probe diagnostic.
    pub diagnostic: String,
    /// Explicit privilege prefix used by capture commands.
    pub sudo: Vec<String>,
}

impl ToolPreflight {
    /// Encode the portable manifest object.
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind.value(),
            "requested_binary": self.requested_binary,
            "resolved_binary": self.resolved_binary,
            "usable": self.usable,
            "returncode": self.returncode,
            "version": self.version,
            "diagnostic": self.diagnostic,
            "sudo": self.sudo,
        })
    }
}

/// Instructions passed to the caller's isolated-step callback.
#[derive(Debug, Clone, PartialEq)]
pub struct IsolatedTrialRequest {
    /// Stable trial directory identifier.
    pub trial_id: String,
    /// Fully-qualified step tag.
    pub step: String,
    /// Worker width to give the step.
    pub inner_jobs: i64,
    /// Profiler attached to this trial.
    pub kind: CaptureKind,
    /// Private directory for this trial's artifacts.
    pub output_dir: PathBuf,
    /// Expected uninstrumented wall time.
    pub expected_wall_s: f64,
    /// Requested active profiling interval.
    pub window: CaptureWindow,
    /// Executable prefix to prepend for perf; empty for sidecar wprof.
    pub argv_prefix: Vec<String>,
    /// Additional environment for the isolated trial.
    pub env: BTreeMap<String, String>,
    /// One-shot signal fired by the runner immediately after the guest process is spawned.
    pub guest_launch: GuestLaunchSignal,
    /// Always false: instrumented trials are never estimator input.
    pub include_in_model: bool,
}

/// The exact successful-spawn boundary for one isolated guest.
#[derive(Debug, Clone, PartialEq)]
pub struct GuestLaunch {
    /// Fully-qualified step tag reported by the scheduler.
    pub step: String,
    /// PID returned by the successful guest spawn.
    pub pid: u32,
    /// Monotonic instant sampled immediately after that spawn.
    pub monotonic: Instant,
}

#[derive(Default)]
struct GuestLaunchState {
    launch: Mutex<Option<GuestLaunch>>,
    ready: Condvar,
    #[cfg(test)]
    wprof_control_events: Mutex<Option<mpsc::SyncSender<&'static str>>>,
}

/// Cloneable one-shot notification used to anchor profiler windows to guest launch.
#[derive(Clone, Default)]
pub struct GuestLaunchSignal {
    state: Arc<GuestLaunchState>,
}

impl fmt::Debug for GuestLaunchSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let notified = self
            .state
            .launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some();
        formatter
            .debug_struct("GuestLaunchSignal")
            .field("notified", &notified)
            .finish()
    }
}

impl PartialEq for GuestLaunchSignal {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl GuestLaunchSignal {
    /// Publish the one successful guest spawn. Duplicate notifications are refused.
    pub fn notify(&self, step: &str, pid: u32, monotonic: Instant) -> Result<(), String> {
        if step.is_empty() || pid == 0 {
            return Err("invalid guest-launch notification".to_string());
        }
        let mut launch = self
            .state
            .launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if launch.is_some() {
            return Err("guest launch was reported more than once".to_string());
        }
        *launch = Some(GuestLaunch {
            step: step.to_string(),
            pid,
            monotonic,
        });
        self.state.ready.notify_all();
        Ok(())
    }

    /// Wait up to `timeout` for the isolated runner to publish its guest spawn.
    pub fn wait(&self, timeout: Duration) -> Option<GuestLaunch> {
        let deadline = Instant::now() + timeout;
        let mut launch = self
            .state
            .launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(launch) = launch.as_ref() {
                return Some(launch.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, timeout_result) = self
                .state
                .ready
                .wait_timeout(launch, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            launch = next;
            if timeout_result.timed_out() && launch.is_none() {
                return None;
            }
        }
    }

    // A per-trial observer lets lifecycle tests order real process exits against
    // controller milestones without modifying release builds or relying on sleeps.
    #[cfg(test)]
    fn observe_wprof_control(&self, event: &'static str) {
        let sender = self
            .state
            .wprof_control_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(sender) = sender {
            let _ = sender.send(event);
        }
    }

    /// Whether the launch boundary has already been published.
    pub fn notified(&self) -> bool {
        self.state
            .launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

impl IsolatedTrialRequest {
    /// Notify capture control immediately after this isolated guest's successful spawn.
    pub fn notify_guest_launched(
        &self,
        step: &str,
        pid: u32,
        monotonic: Instant,
    ) -> Result<(), String> {
        if step != self.step {
            return Err(format!(
                "guest-launch step {step:?} does not match capture step {:?}",
                self.step
            ));
        }
        self.guest_launch.notify(step, pid, monotonic)
    }

    /// Private FIFO the actual guest writes immediately before it is released.
    pub fn guest_launch_ready_path(&self) -> PathBuf {
        self.output_dir.join("guest-launch-ready.fifo")
    }

    /// Private FIFO which releases the guest after capture clocks are anchored.
    pub fn guest_launch_release_path(&self) -> PathBuf {
        self.output_dir.join("guest-launch-release.fifo")
    }
}

/// Minimal result returned by the isolated-step callback.
#[derive(Debug, Clone, PartialEq)]
pub struct IsolatedTrialResult {
    /// Workload return code.
    pub returncode: i32,
    /// Measured trial wall time.
    pub wall_s: f64,
    /// Optional workload diagnostic.
    pub detail: String,
}

impl IsolatedTrialResult {
    /// Validate the measured wall time.
    pub fn validate(&self) -> Result<(), String> {
        if !self.wall_s.is_finite() || self.wall_s < 0.0 {
            Err("trial wall_s must be finite and non-negative".to_string())
        } else {
            Ok(())
        }
    }

    /// Whether the workload exited successfully.
    pub fn ok(&self) -> bool {
        self.returncode == 0
    }
}

/// Callback that executes one fresh, isolated, explicitly non-modelled step trial.
pub trait RunIsolatedTrial {
    /// Run the requested trial and return its workload status and wall time.
    fn run_isolated_trial(
        &mut self,
        request: &IsolatedTrialRequest,
    ) -> Result<IsolatedTrialResult, String>;
}

impl<F> RunIsolatedTrial for F
where
    F: FnMut(&IsolatedTrialRequest) -> Result<IsolatedTrialResult, String>,
{
    fn run_isolated_trial(
        &mut self,
        request: &IsolatedTrialRequest,
    ) -> Result<IsolatedTrialResult, String> {
        self(request)
    }
}

/// One retained private profiler output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureArtifact {
    /// Stable artifact role.
    pub role: String,
    /// Path relative to the capture directory.
    pub path: String,
    /// File size at manifest creation.
    pub size_bytes: u64,
    /// Four-digit octal permission mode.
    pub mode: String,
}

impl CaptureArtifact {
    /// Encode the portable manifest object.
    pub fn to_json(&self) -> Value {
        json!({
            "role": self.role,
            "path": self.path,
            "size_bytes": self.size_bytes,
            "mode": self.mode,
        })
    }
}

/// Manifest record for one separate recommended-width execution.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureTrialRecord {
    /// Stable trial directory identifier.
    pub trial_id: String,
    /// Profiler kind.
    pub kind: CaptureKind,
    /// Trial completion state.
    pub state: CaptureState,
    /// Worker width used by the workload.
    pub inner_jobs: i64,
    /// Expected uninstrumented wall time.
    pub expected_wall_s: f64,
    /// Active profiling interval.
    pub window: CaptureWindow,
    /// UTC trial start time.
    pub started_at: String,
    /// UTC trial finish time.
    pub finished_at: String,
    /// Observed instrumented workload wall time.
    pub measured_wall_s: Option<f64>,
    /// Workload return code, when it ran.
    pub workload_returncode: Option<i32>,
    /// Profiler return code, when independently observable.
    pub profiler_returncode: Option<i32>,
    /// Executable prefix used for the workload.
    pub argv_prefix: Vec<String>,
    /// Retained artifacts.
    pub artifacts: Vec<CaptureArtifact>,
    /// Stable, human-readable failure detail.
    pub error: String,
    /// Always false: this trial is excluded from scaling models.
    pub included_in_model: bool,
}

impl CaptureTrialRecord {
    /// Encode the portable manifest object.
    pub fn to_json(&self) -> Value {
        json!({
            "trial_id": self.trial_id,
            "kind": self.kind.value(),
            "state": self.state.value(),
            "inner_jobs": self.inner_jobs,
            "expected_wall_s": self.expected_wall_s,
            "window": self.window.to_json(),
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "measured_wall_s": self.measured_wall_s,
            "workload_returncode": self.workload_returncode,
            "profiler_returncode": self.profiler_returncode,
            "argv_prefix": self.argv_prefix,
            "artifacts": self.artifacts.iter().map(CaptureArtifact::to_json).collect::<Vec<_>>(),
            "error": self.error,
            "included_in_model": self.included_in_model,
        })
    }
}

/// Complete retained account of a post-sweep capture session.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureManifest {
    /// On-disk manifest path; omitted from the JSON document itself.
    pub path: PathBuf,
    /// Capture directory name.
    pub capture_id: String,
    /// Overall session state.
    pub state: CaptureState,
    /// UTC creation time.
    pub created_at: String,
    /// UTC completion time, blank while running.
    pub finished_at: String,
    /// Stable host identity shared with per-step profile rows.
    pub machine_id: String,
    /// Stable containment/topology identity shared with per-step profile rows.
    pub container_class: String,
    /// Model-selected workload and width.
    pub selection: SweetSpotSelection,
    /// Tool probes in stable perf-then-wprof order.
    pub preflight: Vec<ToolPreflight>,
    /// Instrumented trials.
    pub trials: Vec<CaptureTrialRecord>,
    /// Session-level failures.
    pub errors: Vec<String>,
}

impl CaptureManifest {
    /// Encode the complete portable manifest document.
    pub fn to_json(&self) -> Value {
        json!({
            "schema": PROFILE_CAPTURE_MANIFEST_SCHEMA,
            "capture_id": self.capture_id,
            "state": self.state.value(),
            "created_at": self.created_at,
            "finished_at": self.finished_at,
            "machine_id": self.machine_id,
            "container_class": self.container_class,
            "artifact_root": ".",
            "selection": self.selection.to_json(),
            "preflight": self.preflight.iter().map(ToolPreflight::to_json).collect::<Vec<_>>(),
            "trials": self.trials.iter().map(CaptureTrialRecord::to_json).collect::<Vec<_>>(),
            "errors": self.errors,
        })
    }
}

/// A requested capture failed after retaining its manifest whenever possible.
#[derive(Debug, Clone)]
pub struct ProfileCaptureError {
    message: String,
    /// Retained failed manifest, absent only for validation/setup failures before creation.
    pub manifest: Option<Box<CaptureManifest>>,
}

impl ProfileCaptureError {
    fn before_manifest(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            manifest: None,
        }
    }

    fn with_manifest(message: impl Into<String>, manifest: CaptureManifest) -> Self {
        Self {
            message: message.into(),
            manifest: Some(Box::new(manifest)),
        }
    }
}

impl fmt::Display for ProfileCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if let Some(manifest) = &self.manifest {
            write!(formatter, "; capture manifest: {}", manifest.path.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for ProfileCaptureError {}

fn utc_components() -> (String, String) {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (remainder / 3600, (remainder % 3600) / 60, remainder % 60);
    let (year, month, day) = civil_from_days(days);
    (
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
    )
}

fn utc_now() -> String {
    utc_components().0
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn diagnostic(stdout: &str, stderr: &str) -> String {
    let text = [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let count = text.chars().count();
    text.chars()
        .skip(count.saturating_sub(MAX_DIAGNOSTIC_CHARS))
        .collect()
}

fn resolve_binary(binary: &str) -> Option<PathBuf> {
    if binary.as_bytes().contains(&b'/') {
        let path = PathBuf::from(binary);
        return executable_file(&path).then(|| fs::canonicalize(&path).unwrap_or(path));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(binary))
        .find(|candidate| executable_file(candidate))
        .map(|candidate| fs::canonicalize(&candidate).unwrap_or(candidate))
}

fn executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn set_private_directory(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:08x}",
        (nanos ^ (u64::from(std::process::id()) << 16) ^ counter) as u32
    )
}

fn create_unique_directory(parent: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(parent)?;
    for _ in 0..100 {
        let path = parent.join(format!("{prefix}{}", unique_suffix()));
        match fs::create_dir(&path) {
            Ok(()) => {
                set_private_directory(&path)?;
                return fs::canonicalize(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique capture directory",
    ))
}

fn safe_component(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    if safe.is_empty() {
        "step".to_string()
    } else {
        safe
    }
}

fn new_capture_directory(
    config: &CaptureConfig,
    selection: &SweetSpotSelection,
) -> std::io::Result<(String, PathBuf)> {
    let compact_time = utc_components().1;
    let prefix = format!("{}-{compact_time}-", safe_component(&selection.step));
    let directory = create_unique_directory(&config.output_dir.join("captures"), &prefix)?;
    let capture_id = directory
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("capture")
        .to_string();
    Ok((capture_id, directory))
}

fn write_manifest(manifest: &CaptureManifest) -> Result<(), String> {
    let parent = manifest
        .path
        .parent()
        .ok_or_else(|| "capture manifest has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".manifest-{}.json", unique_suffix()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let mut document =
            serde_json::to_string_pretty(&manifest.to_json()).map_err(std::io::Error::other)?;
        document.push('\n');
        file.write_all(document.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &manifest.path)?;
        fs::set_permissions(&manifest.path, fs::Permissions::from_mode(0o600))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| error.to_string())
}

fn capture_artifact(root: &Path, path: &Path, role: &str) -> Option<CaptureArtifact> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).ok()?;
    }
    let metadata = fs::metadata(path).ok()?;
    let relative = path.strip_prefix(root).ok()?;
    Some(CaptureArtifact {
        role: role.to_string(),
        path: relative.to_string_lossy().replace('\\', "/"),
        size_bytes: metadata.len(),
        mode: format!("{:04o}", metadata.permissions().mode() & 0o777),
    })
}

#[derive(Debug)]
struct CommandResult {
    returncode: i32,
    stdout: String,
    stderr: String,
}

fn exit_returncode(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(-1)
}

fn signal_number(signal_name: &str) -> Option<i32> {
    match signal_name {
        "INT" => Some(libc::SIGINT),
        "TERM" => Some(libc::SIGTERM),
        "KILL" => Some(libc::SIGKILL),
        _ => None,
    }
}

fn pidfd_send_signal_raw(pidfd: i32, signal: i32) -> std::io::Result<()> {
    // SAFETY: `pidfd` is an open pidfd owned by this process and the null siginfo pointer is the
    // documented form for sending an ordinary signal. The kernel copies no userspace data.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Execute the private pidfd signalling mode used behind an explicit sudo prefix.
#[doc(hidden)]
pub fn run_pidfd_signal_helper(args: &[String]) -> i32 {
    let [signal_name] = args else {
        eprintln!("dagrun: internal pidfd signal helper requires exactly one signal name");
        return 2;
    };
    let Some(signal) = signal_number(signal_name) else {
        eprintln!("dagrun: internal pidfd signal helper does not support SIG{signal_name}");
        return 2;
    };
    match pidfd_send_signal_raw(libc::STDIN_FILENO, signal) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("dagrun: internal pidfd signal helper failed: {error}");
            1
        }
    }
}

/// Execute the private process-group signalling mode used behind an explicit sudo prefix.
#[doc(hidden)]
pub fn run_group_signal_helper(args: &[String]) -> i32 {
    let [pgid_text, signal_name] = args else {
        eprintln!("dagrun: internal group signal helper requires PGID and signal name");
        return 2;
    };
    let Ok(pgid) = pgid_text.parse::<i32>() else {
        eprintln!("dagrun: internal group signal helper received an invalid PGID");
        return 2;
    };
    let Some(signal) = signal_number(signal_name) else {
        eprintln!("dagrun: internal group signal helper does not support SIG{signal_name}");
        return 2;
    };
    if pgid <= 1 {
        eprintln!("dagrun: internal group signal helper refuses PGID {pgid}");
        return 2;
    }
    // SAFETY: the caller supplies a positive, freshly-owned child process group; the negative
    // value intentionally addresses exactly that group. ESRCH means cleanup is already complete.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return 0;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        0
    } else {
        eprintln!("dagrun: internal group signal helper failed: {error}");
        1
    }
}

fn finish_command(mut child: Child, timeout_s: f64) -> Result<CommandResult, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);
    loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(_) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
                return Ok(CommandResult {
                    returncode: exit_returncode(output.status),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("command timed out after {timeout_s}s"));
            }
        }
    }
}

fn run_command(argv: &[String], cwd: &Path, timeout_s: f64) -> Result<CommandResult, String> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| "cannot execute an empty command".to_string())?;
    let child = Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    finish_command(child, timeout_s)
}

fn preflight_failure(kind: CaptureKind, binary: &str, config: &CaptureConfig) -> ToolPreflight {
    ToolPreflight {
        kind,
        requested_binary: binary.to_string(),
        resolved_binary: None,
        usable: false,
        returncode: None,
        version: String::new(),
        diagnostic: format!("executable {binary:?} was not found"),
        sudo: config.sudo.clone(),
    }
}

fn command_with_prefix(prefix: &[String], program: &Path, arguments: &[String]) -> Vec<String> {
    let mut command = prefix.to_vec();
    command.push(program.to_string_lossy().into_owned());
    command.extend(arguments.iter().cloned());
    command
}

fn preflight_perf(config: &CaptureConfig, scratch: &Path) -> ToolPreflight {
    let Some(resolved) = resolve_binary(&config.perf_binary) else {
        return preflight_failure(CaptureKind::Perf, &config.perf_binary, config);
    };
    let version_argv = vec![
        resolved.to_string_lossy().into_owned(),
        "version".to_string(),
    ];
    let version = run_command(&version_argv, scratch, config.preflight_timeout_s)
        .map(|result| diagnostic(&result.stdout, &result.stderr))
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let probe = scratch.join("perf-probe.data");
    if let Err(error) = create_private_file(&probe) {
        return ToolPreflight {
            kind: CaptureKind::Perf,
            requested_binary: config.perf_binary.clone(),
            resolved_binary: Some(resolved.to_string_lossy().into_owned()),
            usable: false,
            returncode: None,
            version,
            diagnostic: error.to_string(),
            sudo: config.sudo.clone(),
        };
    }
    let mut arguments = vec![
        "record".to_string(),
        "--quiet".to_string(),
        "--output".to_string(),
        probe.to_string_lossy().into_owned(),
    ];
    arguments.extend(config.perf_args.iter().cloned());
    arguments.extend(["--".to_string(), "/bin/true".to_string()]);
    let command = command_with_prefix(&config.sudo, &resolved, &arguments);
    match run_command(&command, scratch, config.preflight_timeout_s) {
        Ok(result) => ToolPreflight {
            kind: CaptureKind::Perf,
            requested_binary: config.perf_binary.clone(),
            resolved_binary: Some(resolved.to_string_lossy().into_owned()),
            usable: result.returncode == 0
                && fs::metadata(&probe).is_ok_and(|metadata| metadata.len() > 0),
            returncode: Some(result.returncode),
            version,
            diagnostic: diagnostic(&result.stdout, &result.stderr),
            sudo: config.sudo.clone(),
        },
        Err(error) => ToolPreflight {
            kind: CaptureKind::Perf,
            requested_binary: config.perf_binary.clone(),
            resolved_binary: Some(resolved.to_string_lossy().into_owned()),
            usable: false,
            returncode: None,
            version,
            diagnostic: error,
            sudo: config.sudo.clone(),
        },
    }
}

fn wprof_bounded_command(
    config: &CaptureConfig,
    resolved: &Path,
    data_path: &Path,
    trace_path: &Path,
    window: CaptureWindow,
    selection: Option<&SweetSpotSelection>,
) -> Vec<String> {
    let activation = if window.start_offset_s <= 0.0 {
        "@now".to_string()
    } else {
        format!("+{:.6}s", window.start_offset_s)
    };
    let mut arguments = vec![
        "--record".to_string(),
        "--prepare=@now".to_string(),
        format!("--activate={activation}"),
        format!("--dur={:.6}s", window.duration_s),
        format!("--data={}", data_path.display()),
        format!("--trace={}", trace_path.display()),
    ];
    if let Some(selection) = selection {
        arguments.extend([
            format!("--metadata=dagrun.step={}", selection.step),
            format!("--metadata=dagrun.inner_jobs={}", selection.inner_jobs),
            format!(
                "--metadata=dagrun.workload_digest={}",
                selection.workload_digest
            ),
        ]);
    }
    arguments.extend(config.wprof_args.iter().cloned());
    command_with_prefix(&config.sudo, resolved, &arguments)
}

/// Build a flight-recorder command for an actual workload capture.
///
/// Delayed activation is intentionally not used here. Real wprof startup can spend hundreds of
/// milliseconds preparing BPF state, enough for a short relative activation deadline to expire
/// before the recorder is ready. Flight-recorder mode lets us wait for an explicit ready marker,
/// start the workload only then, and stop at the desired workload-relative endpoint with SIGINT.
fn wprof_flight_command(
    config: &CaptureConfig,
    resolved: &Path,
    data_path: &Path,
    trace_path: &Path,
    launch_gate_path: Option<&Path>,
    window: CaptureWindow,
    selection: &SweetSpotSelection,
) -> Vec<String> {
    let arguments = [
        "--record".to_string(),
        format!("--flight-record={:.6}s", window.duration_s),
        format!("--data={}", data_path.display()),
        format!("--trace={}", trace_path.display()),
        format!("--metadata=dagrun.step={}", selection.step),
        format!("--metadata=dagrun.inner_jobs={}", selection.inner_jobs),
        format!(
            "--metadata=dagrun.workload_digest={}",
            selection.workload_digest
        ),
    ];
    let mut wprof = vec![resolved.to_string_lossy().into_owned()];
    wprof.extend(arguments);
    wprof.extend(config.wprof_args.iter().cloned());
    // Pin the shell before it becomes wprof: report its exec-stable PID, wait while the parent
    // opens a pidfd, and only then exec. This closes the spawn/marker/PID-reuse race in both lanes.
    let launch_gate_path = launch_gate_path.expect("wprof needs a launch gate");
    let mut command = config.sudo.clone();
    command.extend([
        "/bin/sh".to_string(),
        "-c".to_string(),
        "printf 'DAGRUN_WPROF_PID=%s\\n' \"$$\" >&2; \
         IFS= read -r dagrun_gate < \"$1\" || exit 125; \
         [ \"$dagrun_gate\" = DAGRUN_WPROF_GO ] || exit 125; \
         shift; exec \"$@\""
            .to_string(),
        "dagrun-wprof".to_string(),
        launch_gate_path.to_string_lossy().into_owned(),
    ]);
    command.extend(wprof);
    command
}

fn preflight_wprof(config: &CaptureConfig, scratch: &Path) -> ToolPreflight {
    let Some(resolved) = resolve_binary(&config.wprof_binary) else {
        return preflight_failure(CaptureKind::Wprof, &config.wprof_binary, config);
    };
    let version_argv = vec![
        resolved.to_string_lossy().into_owned(),
        "--version".to_string(),
    ];
    let version = run_command(&version_argv, scratch, config.preflight_timeout_s)
        .map(|result| diagnostic(&result.stdout, &result.stderr))
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let data_path = scratch.join("wprof-probe.data");
    let trace_path = scratch.join("wprof-probe.pb");
    if let Err(error) =
        create_private_file(&data_path).and_then(|_| create_private_file(&trace_path))
    {
        return ToolPreflight {
            kind: CaptureKind::Wprof,
            requested_binary: config.wprof_binary.clone(),
            resolved_binary: Some(resolved.to_string_lossy().into_owned()),
            usable: false,
            returncode: None,
            version,
            diagnostic: error.to_string(),
            sudo: config.sudo.clone(),
        };
    }
    let window = CaptureWindow {
        requested_duration_s: 0.001,
        start_offset_s: 0.0,
        duration_s: 0.001,
        clipped: false,
    };
    let command_data_path = absolute_path(&data_path).unwrap_or_else(|_| data_path.clone());
    let command_trace_path = absolute_path(&trace_path).unwrap_or_else(|_| trace_path.clone());
    let command = wprof_bounded_command(
        config,
        &resolved,
        &command_data_path,
        &command_trace_path,
        window,
        None,
    );
    match run_command(&command, scratch, config.preflight_timeout_s) {
        Ok(result) => ToolPreflight {
            kind: CaptureKind::Wprof,
            requested_binary: config.wprof_binary.clone(),
            resolved_binary: Some(resolved.to_string_lossy().into_owned()),
            usable: result.returncode == 0
                && fs::metadata(&data_path).is_ok_and(|metadata| metadata.len() > 0)
                && fs::metadata(&trace_path).is_ok_and(|metadata| metadata.len() > 0),
            returncode: Some(result.returncode),
            version,
            diagnostic: diagnostic(&result.stdout, &result.stderr),
            sudo: config.sudo.clone(),
        },
        Err(error) => ToolPreflight {
            kind: CaptureKind::Wprof,
            requested_binary: config.wprof_binary.clone(),
            resolved_binary: Some(resolved.to_string_lossy().into_owned()),
            usable: false,
            returncode: None,
            version,
            diagnostic: error,
            sudo: config.sudo.clone(),
        },
    }
}

/// Probe every requested profiler without implicitly adding privilege escalation.
pub fn preflight_capture_tools(
    config: &CaptureConfig,
) -> Result<BTreeMap<CaptureKind, ToolPreflight>, ProfileCaptureError> {
    config
        .validate()
        .map_err(ProfileCaptureError::before_manifest)?;
    fs::create_dir_all(&config.output_dir)
        .map_err(|error| ProfileCaptureError::before_manifest(error.to_string()))?;
    let scratch = create_unique_directory(&config.output_dir, ".capture-preflight-")
        .map_err(|error| ProfileCaptureError::before_manifest(error.to_string()))?;
    let mut results = BTreeMap::new();
    if config.capture_perf {
        results.insert(CaptureKind::Perf, preflight_perf(config, &scratch));
    }
    if config.wprof_windows > 0 {
        results.insert(CaptureKind::Wprof, preflight_wprof(config, &scratch));
    }
    let _ = fs::remove_dir_all(&scratch);
    Ok(results)
}

#[derive(Debug, Default)]
struct GuestLaunchOutcome {
    released: bool,
    error: String,
}

/// Translate the inner shell's two-FIFO handshake into one in-process monotonic launch instant.
struct GuestLaunchBridge {
    signal: GuestLaunchSignal,
    finished: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<GuestLaunchOutcome>>,
}

impl GuestLaunchBridge {
    fn start(request: &IsolatedTrialRequest, timeout: Duration) -> Result<Self, String> {
        let ready_path = request.guest_launch_ready_path();
        let release_path = request.guest_launch_release_path();
        let step = request.step.clone();
        let signal = request.guest_launch.clone();
        let worker_signal = signal.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let (initialized_sender, initialized_receiver) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("dagrun-guest-launch".to_string())
            .spawn(move || {
                run_guest_launch_bridge(
                    &ready_path,
                    &release_path,
                    &step,
                    &worker_signal,
                    timeout,
                    &worker_finished,
                    initialized_sender,
                )
            })
            .map_err(|error| error.to_string())?;
        match initialized_receiver.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(Self {
                signal,
                finished,
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                finished.store(true, Ordering::Release);
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                finished.store(true, Ordering::Release);
                let _ = handle.join();
                Err("guest-launch FIFO bridge did not initialize".to_string())
            }
        }
    }

    fn finish(mut self) -> GuestLaunchOutcome {
        self.finished.store(true, Ordering::Release);
        let mut outcome = self
            .handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_else(|| GuestLaunchOutcome {
                error: "guest-launch FIFO bridge did not stop".to_string(),
                ..GuestLaunchOutcome::default()
            });
        if !outcome.released && !self.signal.notified() && outcome.error.is_empty() {
            outcome.error = "isolated callback did not report guest launch".to_string();
        }
        outcome
    }
}

#[allow(clippy::too_many_arguments)]
fn run_guest_launch_bridge(
    ready_path: &Path,
    release_path: &Path,
    step: &str,
    signal: &GuestLaunchSignal,
    timeout: Duration,
    finished: &AtomicBool,
    initialized: mpsc::SyncSender<Result<(), String>>,
) -> GuestLaunchOutcome {
    let mut outcome = GuestLaunchOutcome::default();
    let mut ready = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(ready_path)
    {
        Ok(file) => {
            let _ = initialized.send(Ok(()));
            file
        }
        Err(error) => {
            let message = format!("guest-launch FIFO handshake failed: {error}");
            let _ = initialized.send(Err(message.clone()));
            outcome.error = message;
            return outcome;
        }
    };
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 256];
    while !finished.load(Ordering::Acquire) {
        if signal.notified() {
            return outcome;
        }
        match ready.read(&mut chunk) {
            Ok(0) => thread::sleep(Duration::from_millis(5)),
            Ok(count) => {
                buffered.extend_from_slice(&chunk[..count]);
                if buffered.len() > 128 {
                    outcome.error =
                        "guest-launch FIFO handshake failed: PID marker is too long".to_string();
                    return outcome;
                }
                let Some(newline) = buffered.iter().position(|byte| *byte == b'\n') else {
                    continue;
                };
                let marker = String::from_utf8_lossy(&buffered[..newline]);
                let pid = match marker.trim().parse::<u32>() {
                    Ok(pid) if pid > 0 => pid,
                    _ => {
                        outcome.error = format!(
                            "guest-launch FIFO handshake failed: invalid PID marker {:?}",
                            marker.trim()
                        );
                        return outcome;
                    }
                };
                let mut release = match open_fifo_writer(release_path, timeout, finished) {
                    Ok(Some(file)) => file,
                    Ok(None) => {
                        if !finished.load(Ordering::Acquire) {
                            outcome.error =
                                "guest did not open its launch-release FIFO".to_string();
                        }
                        return outcome;
                    }
                    Err(error) => {
                        outcome.error = format!("guest-launch FIFO handshake failed: {error}");
                        return outcome;
                    }
                };
                let launched_at = Instant::now();
                if let Err(error) = signal.notify(step, pid, launched_at) {
                    outcome.error = format!("guest-launch FIFO handshake failed: {error}");
                    return outcome;
                }
                if let Err(error) = release.write_all(b"go\n") {
                    outcome.error = format!("guest-launch FIFO handshake failed: {error}");
                    return outcome;
                }
                outcome.released = true;
                return outcome;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                outcome.error = format!("guest-launch FIFO handshake failed: {error}");
                return outcome;
            }
        }
    }
    outcome
}

fn prepare_guest_launch_fifos(request: &IsolatedTrialRequest) -> Result<(), String> {
    create_fifo(&request.guest_launch_ready_path()).map_err(|error| error.to_string())?;
    create_fifo(&request.guest_launch_release_path()).map_err(|error| error.to_string())
}

#[derive(Debug, Default)]
struct PerfControlOutcome {
    enabled: bool,
    disabled: bool,
    error: String,
}

struct PerfWindowController {
    finished: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<PerfControlOutcome>>,
}

impl PerfWindowController {
    fn start(
        control_path: PathBuf,
        ack_path: PathBuf,
        window: CaptureWindow,
        guest_launch: GuestLaunchSignal,
        ack_timeout: Duration,
    ) -> Result<Self, String> {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let handle = thread::Builder::new()
            .name("dagrun-perf-control".to_string())
            .spawn(move || {
                run_perf_controller(
                    &control_path,
                    &ack_path,
                    window,
                    ack_timeout,
                    &worker_finished,
                    ready_sender,
                    guest_launch,
                )
            })
            .map_err(|error| error.to_string())?;
        match ready_receiver.recv_timeout(ack_timeout) {
            Ok(Ok(())) => Ok(Self {
                finished,
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                finished.store(true, Ordering::Release);
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                finished.store(true, Ordering::Release);
                let _ = handle.join();
                Err("perf FIFO controller did not initialize".to_string())
            }
        }
    }

    fn finish(mut self) -> PerfControlOutcome {
        self.finished.store(true, Ordering::Release);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_else(|| PerfControlOutcome {
                error: "perf FIFO controller did not stop".to_string(),
                ..PerfControlOutcome::default()
            })
    }
}

fn run_perf_controller(
    control_path: &Path,
    ack_path: &Path,
    window: CaptureWindow,
    ack_timeout: Duration,
    finished: &AtomicBool,
    ready_sender: mpsc::SyncSender<Result<(), String>>,
    guest_launch: GuestLaunchSignal,
) -> PerfControlOutcome {
    let mut outcome = PerfControlOutcome::default();
    let mut ack = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(ack_path)
    {
        Ok(file) => {
            let _ = ready_sender.send(Ok(()));
            file
        }
        Err(error) => {
            let message = format!("perf FIFO control failed: {error}");
            let _ = ready_sender.send(Err(message.clone()));
            outcome.error = message;
            return outcome;
        }
    };
    let origin = loop {
        if finished.load(Ordering::Acquire) {
            outcome.error = "isolated callback did not report guest launch".to_string();
            return outcome;
        }
        if let Some(launch) = guest_launch.wait(Duration::from_millis(50)) {
            break launch.monotonic;
        }
    };
    let mut control = match open_fifo_writer(control_path, ack_timeout, finished) {
        Ok(Some(file)) => file,
        Ok(None) => {
            if !finished.load(Ordering::Acquire) {
                outcome.error = "perf did not open its control FIFO".to_string();
            }
            return outcome;
        }
        Err(error) => {
            outcome.error = format!("perf FIFO control failed: {error}");
            return outcome;
        }
    };
    if !wait_until(origin, window.start_offset_s, finished) {
        outcome.error = "step ended before the perf window opened".to_string();
        return outcome;
    }
    if !send_perf_command(&mut control, &mut ack, b"enable\n", ack_timeout, finished) {
        outcome.error = "perf did not acknowledge enable".to_string();
        return outcome;
    }
    outcome.enabled = true;
    if !wait_until(origin, window.end_offset_s(), finished) {
        outcome.error = "step ended before the perf window closed".to_string();
        return outcome;
    }
    if !send_perf_command(&mut control, &mut ack, b"disable\n", ack_timeout, finished) {
        outcome.error = "perf did not acknowledge disable".to_string();
        return outcome;
    }
    outcome.disabled = true;
    outcome
}

fn open_fifo_writer(
    path: &Path,
    timeout: Duration,
    finished: &AtomicBool,
) -> std::io::Result<Option<File>> {
    let deadline = Instant::now() + timeout;
    while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => return Ok(Some(file)),
            Err(error)
                if matches!(error.raw_os_error(), Some(libc::ENXIO) | Some(libc::ENOENT)) =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn wait_until(origin: Instant, offset_s: f64, finished: &AtomicBool) -> bool {
    let deadline = origin + Duration::from_secs_f64(offset_s);
    while Instant::now() < deadline {
        if finished.load(Ordering::Acquire) {
            return false;
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10)),
        );
    }
    !finished.load(Ordering::Acquire)
}

fn send_perf_command(
    control: &mut File,
    ack: &mut File,
    command: &[u8],
    timeout: Duration,
    finished: &AtomicBool,
) -> bool {
    if control.write_all(command).is_err() {
        return false;
    }
    let deadline = Instant::now() + timeout;
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 256];
    while Instant::now() < deadline && !finished.load(Ordering::Acquire) {
        match ack.read(&mut chunk) {
            Ok(0) => thread::sleep(Duration::from_millis(5)),
            Ok(count) => {
                buffered.extend_from_slice(&chunk[..count]);
                if let Some(newline) = buffered.iter().position(|byte| *byte == b'\n') {
                    return buffered[..newline]
                        .iter()
                        .copied()
                        .filter(|byte| !byte.is_ascii_whitespace())
                        .eq(b"ack".iter().copied());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return false,
        }
    }
    false
}

fn create_fifo(path: &Path) -> std::io::Result<()> {
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "FIFO path contains NUL")
    })?;
    // SAFETY: `name` is a live NUL-terminated path, and mkfifo retains no pointer.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn join_error(existing: &str, addition: impl AsRef<str>) -> String {
    let addition = addition.as_ref();
    if existing.is_empty() {
        addition.to_string()
    } else if addition.is_empty() {
        existing.to_string()
    } else {
        format!("{existing}; {addition}")
    }
}

fn perf_trial<R>(
    capture_root: &Path,
    selection: &SweetSpotSelection,
    config: &CaptureConfig,
    preflight: &ToolPreflight,
    run_trial: &mut R,
) -> Result<CaptureTrialRecord, String>
where
    R: RunIsolatedTrial + ?Sized,
{
    let trial_id = "perf-001";
    let trial_dir = capture_root.join(trial_id);
    fs::create_dir(&trial_dir).map_err(|error| error.to_string())?;
    set_private_directory(&trial_dir).map_err(|error| error.to_string())?;
    let perf_data = trial_dir.join("perf.data");
    let control_path = trial_dir.join("control.fifo");
    let ack_path = trial_dir.join("ack.fifo");
    create_private_file(&perf_data).map_err(|error| error.to_string())?;
    create_fifo(&control_path).map_err(|error| error.to_string())?;
    create_fifo(&ack_path).map_err(|error| error.to_string())?;
    let perf_duration = config
        .perf_window_s
        .unwrap_or(selection.expected_wall_s * 0.8);
    let window = centered_capture_window(selection.expected_wall_s, perf_duration)?;
    let resolved = preflight
        .resolved_binary
        .as_deref()
        .ok_or_else(|| "usable perf preflight has no resolved binary".to_string())?;
    let perf_data_arg = absolute_path(&perf_data)?;
    let control_arg = absolute_path(&control_path)?;
    let ack_arg = absolute_path(&ack_path)?;
    let mut prefix = config.sudo.clone();
    prefix.extend([
        resolved.to_string(),
        "record".to_string(),
        "--quiet".to_string(),
        "--output".to_string(),
        perf_data_arg.to_string_lossy().into_owned(),
        "--control".to_string(),
        format!("fifo:{},{}", control_arg.display(), ack_arg.display()),
        "--delay=-1".to_string(),
    ]);
    prefix.extend(config.perf_args.iter().cloned());
    prefix.push("--".to_string());
    let request = IsolatedTrialRequest {
        trial_id: trial_id.to_string(),
        step: selection.step.clone(),
        inner_jobs: selection.inner_jobs,
        kind: CaptureKind::Perf,
        output_dir: trial_dir.clone(),
        expected_wall_s: selection.expected_wall_s,
        window,
        argv_prefix: prefix.clone(),
        env: BTreeMap::new(),
        guest_launch: GuestLaunchSignal::default(),
        include_in_model: false,
    };
    prepare_guest_launch_fifos(&request)?;
    let started_at = utc_now();
    let launch_bridge = GuestLaunchBridge::start(
        &request,
        Duration::from_secs_f64(config.control_ack_timeout_s),
    )?;
    let controller = match PerfWindowController::start(
        control_path,
        ack_path,
        window,
        request.guest_launch.clone(),
        Duration::from_secs_f64(config.control_ack_timeout_s),
    ) {
        Ok(controller) => controller,
        Err(error) => {
            let launch = launch_bridge.finish();
            return Err(join_error(&error, launch.error));
        }
    };
    let callback_result = run_trial.run_isolated_trial(&request);
    let launch = launch_bridge.finish();
    let control = controller.finish();
    let mut error = String::new();
    let result = match callback_result {
        Ok(result) => match result.validate() {
            Ok(()) => Some(result),
            Err(problem) => {
                error = join_error(&error, format!("isolated perf trial failed: {problem}"));
                None
            }
        },
        Err(problem) => {
            error = join_error(&error, format!("isolated perf trial failed: {problem}"));
            None
        }
    };
    if !control.error.is_empty() {
        error = join_error(&error, control.error);
    }
    if !launch.error.is_empty() && !error.contains(&launch.error) {
        error = join_error(&error, launch.error);
    }
    let _ = fs::remove_file(request.guest_launch_ready_path());
    let _ = fs::remove_file(request.guest_launch_release_path());
    if let Err(problem) = make_artifacts_private(config, &request.output_dir, &[&perf_data]) {
        error = join_error(&error, problem);
    }
    let artifact = capture_artifact(capture_root, &perf_data, "perf-data");
    if artifact
        .as_ref()
        .is_none_or(|artifact| artifact.size_bytes == 0)
    {
        error = join_error(&error, "perf produced no data");
    }
    if let Some(result) = &result {
        if !result.ok() {
            let detail = if result.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", result.detail)
            };
            error = join_error(
                &error,
                format!("perf wrapper exited {}{detail}", result.returncode),
            );
        }
    }
    let complete = result.as_ref().is_some_and(IsolatedTrialResult::ok)
        && control.enabled
        && control.disabled
        && error.is_empty();
    Ok(CaptureTrialRecord {
        trial_id: trial_id.to_string(),
        kind: CaptureKind::Perf,
        state: if complete {
            CaptureState::Complete
        } else {
            CaptureState::Failed
        },
        inner_jobs: selection.inner_jobs,
        expected_wall_s: selection.expected_wall_s,
        window,
        started_at,
        finished_at: utc_now(),
        measured_wall_s: result.as_ref().map(|result| result.wall_s),
        workload_returncode: None,
        profiler_returncode: result.as_ref().map(|result| result.returncode),
        argv_prefix: prefix,
        artifacts: artifact.into_iter().collect(),
        error,
        included_in_model: false,
    })
}

fn spawn_wprof(argv: &[String], cwd: &Path, log_path: &Path) -> Result<Child, String> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| "cannot execute an empty wprof command".to_string())?;
    let log = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(log_path)
        .map_err(|error| error.to_string())?;
    let error_log = log.try_clone().map_err(|error| error.to_string())?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log));
    // SAFETY: the child-only closure calls async-signal-safe libc functions before exec and
    // returns an owned io::Error; it captures no borrowed process state.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|error| error.to_string())
}

fn wait_child(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wprof_pid_from_log(log_path: &Path) -> Option<i32> {
    let text = fs::read_to_string(log_path).ok()?;
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(WPROF_PID_PREFIX)
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|pid| *pid > 1)
    })
}

#[derive(Debug)]
struct PinnedProcess {
    pid: i32,
    pidfd: File,
    privileged: bool,
}

impl PinnedProcess {
    fn try_clone(&self) -> Result<Self, String> {
        Ok(Self {
            pid: self.pid,
            pidfd: self.pidfd.try_clone().map_err(|error| error.to_string())?,
            privileged: self.privileged,
        })
    }

    fn exited(&self, timeout: Duration) -> Result<bool, String> {
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and remains live for the call.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result < 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(result > 0)
        }
    }
}

fn pin_process(pid: i32, privileged: bool) -> Result<PinnedProcess, String> {
    if pid <= 1 {
        return Err(format!("could not pin invalid wprof PID {pid}"));
    }
    // SAFETY: pidfd_open returns a new owned descriptor on success and retains no pointer.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if descriptor < 0 {
        return Err(format!(
            "could not pin wprof process identity for PID {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: pidfd_open returned this fresh descriptor and ownership is transferred to File.
    let pidfd = unsafe { File::from_raw_fd(descriptor) };
    Ok(PinnedProcess {
        pid,
        pidfd,
        privileged,
    })
}

fn wait_for_wprof_pid(
    supervisor: &PinnedProcess,
    log_path: &Path,
    timeout: Duration,
) -> Result<i32, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pid) = wprof_pid_from_log(log_path) {
            return Ok(pid);
        }
        if supervisor.exited(Duration::ZERO)? {
            return Err("privileged wprof exited before reporting its exec PID".to_string());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "privileged wprof did not report its exec PID within {:.3}s",
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn release_wprof_gate(path: &Path, timeout: Duration) -> Result<(), String> {
    let finished = AtomicBool::new(false);
    let Some(mut gate) = open_fifo_writer(path, timeout, &finished)
        .map_err(|error| format!("could not open privileged wprof exec gate: {error}"))?
    else {
        return Err(format!(
            "privileged wprof did not wait on its exec gate within {:.3}s",
            timeout.as_secs_f64()
        ));
    };
    gate.write_all(format!("{WPROF_GATE_TOKEN}\n").as_bytes())
        .map_err(|error| format!("could not release privileged wprof exec gate: {error}"))
}

fn wait_for_wprof_ready(
    profiler: &PinnedProcess,
    log_path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let log = fs::read_to_string(log_path).unwrap_or_default();
        let ready = log
            .lines()
            .any(|line| line.trim_start().starts_with(WPROF_READY_PREFIX));
        if ready {
            return Ok(());
        }
        if profiler.exited(Duration::ZERO)? {
            let detail = diagnostic("", &log);
            return Err(format!(
                "wprof exited before announcing flight-recorder readiness{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "wprof did not announce flight-recorder readiness within {:.3}s",
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn privileged_signal_command(
    sudo: &[String],
    helper: &str,
    helper_args: &[String],
    stdin: Stdio,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandResult, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate dagrun signal helper: {error}"))?;
    let (program, prefix) = sudo
        .split_first()
        .ok_or_else(|| "privileged signal command needs a sudo prefix".to_string())?;
    let mut command = Command::new(program);
    command
        .args(prefix)
        .arg(executable)
        .arg(helper)
        .args(helper_args)
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|error| error.to_string())?;
    finish_command(child, timeout.as_secs_f64())
}

fn send_pinned_wprof_signal(
    config: &CaptureConfig,
    profiler: &PinnedProcess,
    signal_name: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let Some(signal) = signal_number(signal_name) else {
        return Err(format!("unsupported wprof signal {signal_name}"));
    };
    if !profiler.privileged {
        return pidfd_send_signal_raw(profiler.pidfd.as_raw_fd(), signal)
            .map_err(|error| format!("could not send SIG{signal_name} to wprof: {error}"));
    }

    let pidfd = profiler
        .pidfd
        .try_clone()
        .map_err(|error| format!("could not duplicate pinned wprof identity: {error}"))?;
    let result = privileged_signal_command(
        &config.sudo,
        PROFILE_PIDFD_SIGNAL_COMMAND,
        &[signal_name.to_string()],
        Stdio::from(pidfd),
        cwd,
        timeout,
    )
    .map_err(|error| format!("could not send SIG{signal_name} to wprof: {error}"))?;
    if result.returncode == 0 {
        Ok(())
    } else {
        Err(format!(
            "could not send SIG{signal_name} to wprof (exit {}){}",
            result.returncode,
            if diagnostic(&result.stdout, &result.stderr).is_empty() {
                String::new()
            } else {
                format!(": {}", diagnostic(&result.stdout, &result.stderr))
            }
        ))
    }
}

fn send_wprof_group_signal(
    config: &CaptureConfig,
    pgid: i32,
    signal_name: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let Some(signal) = signal_number(signal_name) else {
        return Err(format!("unsupported wprof signal {signal_name}"));
    };
    if config.sudo.is_empty() {
        // SAFETY: spawn_wprof created this positive PGID in a fresh session and its direct leader
        // remains unreaped. ESRCH means every member has already exited.
        if unsafe { libc::kill(-pgid, signal) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(format!(
            "could not send SIG{signal_name} to wprof process group: {error}"
        ));
    }

    let result = privileged_signal_command(
        &config.sudo,
        PROFILE_GROUP_SIGNAL_COMMAND,
        &[pgid.to_string(), signal_name.to_string()],
        Stdio::null(),
        cwd,
        timeout,
    )
    .map_err(|error| format!("could not send SIG{signal_name} to wprof process group: {error}"))?;
    if result.returncode == 0 {
        Ok(())
    } else {
        Err(format!(
            "could not send SIG{signal_name} to wprof process group (exit {}){}",
            result.returncode,
            if diagnostic(&result.stdout, &result.stderr).is_empty() {
                String::new()
            } else {
                format!(": {}", diagnostic(&result.stdout, &result.stderr))
            }
        ))
    }
}

fn teardown_wprof_group(
    child: &mut Child,
    supervisor: Option<&PinnedProcess>,
    config: &CaptureConfig,
    cwd: &Path,
    graceful_interrupt_sent: bool,
) -> (Option<ExitStatus>, String) {
    let grace = Duration::from_secs_f64(config.profiler_exit_grace_s);
    let mut error = String::new();
    if graceful_interrupt_sent
        && !supervisor.is_some_and(|identity| identity.exited(grace).unwrap_or(false))
    {
        error = join_error(&error, "wprof did not exit after SIGINT");
    }
    let pgid = i32::try_from(child.id()).unwrap_or(-1);
    if pgid > 1 {
        if let Err(problem) = send_wprof_group_signal(config, pgid, "TERM", cwd, grace) {
            error = join_error(&error, problem);
        }
        thread::sleep(grace.min(Duration::from_millis(50)));
        // The direct session leader is deliberately still unreaped here, so its process-group
        // number cannot be recycled between the TERM and KILL sweeps.
        if let Err(problem) = send_wprof_group_signal(config, pgid, "KILL", cwd, grace) {
            error = join_error(&error, problem);
        }
    }
    let status = match wait_child(child, grace) {
        Ok(status) => status,
        Err(problem) => {
            error = join_error(&error, problem.to_string());
            None
        }
    };
    if status.is_none() {
        error = join_error(
            &error,
            "wprof process-group teardown did not reap its session leader",
        );
    }
    (status, error)
}

#[derive(Debug, Default)]
struct WprofStopOutcome {
    sent: bool,
    workload_elapsed_s: Option<f64>,
    error: String,
}

fn start_wprof_stop_controller(
    config: CaptureConfig,
    profiler: PinnedProcess,
    cwd: PathBuf,
    guest_launch: GuestLaunchSignal,
    end_offset_s: f64,
    signal_timeout: Duration,
    workload_finished: mpsc::Receiver<()>,
) -> Result<thread::JoinHandle<WprofStopOutcome>, String> {
    thread::Builder::new()
        .name("dagrun-wprof-control".to_string())
        .spawn(move || {
            let launch = loop {
                match workload_finished.try_recv() {
                    Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                        return WprofStopOutcome {
                            error: "isolated callback did not report guest launch".to_string(),
                            ..WprofStopOutcome::default()
                        };
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                if profiler.exited(Duration::ZERO).unwrap_or(false) {
                    return WprofStopOutcome {
                        error: "wprof exited before the profiling window opened".to_string(),
                        ..WprofStopOutcome::default()
                    };
                }
                if let Some(launch) = guest_launch.wait(Duration::from_millis(50)) {
                    break launch;
                }
            };
            let origin = launch.monotonic;
            let deadline = origin + Duration::from_secs_f64(end_offset_s);
            #[cfg(test)]
            guest_launch.observe_wprof_control("launch-observed");
            while Instant::now() < deadline {
                match workload_finished.try_recv() {
                    Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                        return WprofStopOutcome {
                            workload_elapsed_s: Some(origin.elapsed().as_secs_f64()),
                            ..WprofStopOutcome::default()
                        };
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                if profiler.exited(Duration::ZERO).unwrap_or(false) {
                    #[cfg(test)]
                    guest_launch.observe_wprof_control("early-exit-observed");
                    return WprofStopOutcome {
                        error: "wprof exited before the profiling window ended".to_string(),
                        ..WprofStopOutcome::default()
                    };
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                );
            }
            match send_pinned_wprof_signal(&config, &profiler, "INT", &cwd, signal_timeout) {
                Ok(()) => WprofStopOutcome {
                    sent: true,
                    error: String::new(),
                    ..WprofStopOutcome::default()
                },
                Err(problem) => WprofStopOutcome {
                    sent: false,
                    error: problem,
                    ..WprofStopOutcome::default()
                },
            }
        })
        .map_err(|error| error.to_string())
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| error.to_string())
    }
}

/// Reclaim outputs created by an explicitly privileged profiler and pin every retained artifact
/// to private permissions. Only the exact paths allocated inside this capture directory are ever
/// passed to chown/chmod.
fn make_artifacts_private(
    config: &CaptureConfig,
    cwd: &Path,
    paths: &[&Path],
) -> Result<(), String> {
    let mut files = Vec::new();
    for path in paths {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            return Err(format!(
                "profiler artifact is not a regular file: {}",
                path.display()
            ));
        }
        files.push(absolute_path(path)?);
    }
    if files.is_empty() {
        return Ok(());
    }

    if !config.sudo.is_empty() {
        // SAFETY: these calls only read this process's credentials.
        let owner = format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
            libc::getegid()
        });
        let mut chown = config.sudo.clone();
        chown.extend([
            "/bin/chown".to_string(),
            "--no-dereference".to_string(),
            owner,
            "--".to_string(),
        ]);
        chown.extend(files.iter().map(|path| path.to_string_lossy().into_owned()));
        let chown_result = run_command(&chown, cwd, config.profiler_exit_grace_s)
            .map_err(|error| format!("could not reclaim privileged profiler artifacts: {error}"))?;
        if chown_result.returncode != 0 {
            let detail = diagnostic(&chown_result.stdout, &chown_result.stderr);
            return Err(format!(
                "could not reclaim privileged profiler artifacts (exit {}){}",
                chown_result.returncode,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
        }
    }

    // SAFETY: these calls only read this process's credentials.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    for path in &files {
        let before = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "could not secure profiler artifact {}: {error}",
                path.display()
            )
        })?;
        if !before.file_type().is_file() {
            return Err(format!(
                "profiler artifact is not a regular file: {}",
                path.display()
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!(
                "could not secure profiler artifact {}: {error}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "could not secure profiler artifact {}: {error}",
                path.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "profiler artifact is not a regular file: {}",
                path.display()
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(format!(
                "profiler artifact mode is not 0600: {}",
                path.display()
            ));
        }
        if metadata.uid() != uid || metadata.gid() != gid {
            return Err(format!(
                "profiler artifact ownership was not reclaimed: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn wprof_trial<R>(
    capture_root: &Path,
    index: i64,
    selection: &SweetSpotSelection,
    config: &CaptureConfig,
    preflight: &ToolPreflight,
    run_trial: &mut R,
) -> Result<CaptureTrialRecord, String>
where
    R: RunIsolatedTrial + ?Sized,
{
    let trial_id = format!("wprof-{index:03}");
    let trial_dir = capture_root.join(&trial_id);
    fs::create_dir(&trial_dir).map_err(|error| error.to_string())?;
    set_private_directory(&trial_dir).map_err(|error| error.to_string())?;
    let data_path = trial_dir.join("wprof.data");
    let trace_path = trial_dir.join("trace.pb");
    let log_path = trial_dir.join("wprof.log");
    let exec_gate = trial_dir.join("wprof-exec-gate.fifo");
    for path in [&data_path, &trace_path, &log_path] {
        create_private_file(path).map_err(|error| error.to_string())?;
    }
    create_fifo(&exec_gate).map_err(|error| error.to_string())?;
    let window = centered_capture_window(selection.expected_wall_s, config.wprof_window_s)?;
    let resolved = preflight
        .resolved_binary
        .as_deref()
        .ok_or_else(|| "usable wprof preflight has no resolved binary".to_string())?;
    let command_data_path = absolute_path(&data_path)?;
    let command_trace_path = absolute_path(&trace_path)?;
    let command_exec_gate = absolute_path(&exec_gate)?;
    let command = wprof_flight_command(
        config,
        Path::new(resolved),
        &command_data_path,
        &command_trace_path,
        Some(&command_exec_gate),
        window,
        selection,
    );
    let request = IsolatedTrialRequest {
        trial_id: trial_id.clone(),
        step: selection.step.clone(),
        inner_jobs: selection.inner_jobs,
        kind: CaptureKind::Wprof,
        output_dir: trial_dir.clone(),
        expected_wall_s: selection.expected_wall_s,
        window,
        argv_prefix: Vec::new(),
        env: BTreeMap::new(),
        guest_launch: GuestLaunchSignal::default(),
        include_in_model: false,
    };
    prepare_guest_launch_fifos(&request)?;
    let started_at = utc_now();
    let mut child = spawn_wprof(&command, &trial_dir, &log_path)?;
    let mut error = String::new();
    let ready_timeout = Duration::from_secs_f64(config.wprof_ready_timeout_s);
    let supervisor = match i32::try_from(child.id())
        .map_err(|_| "wprof supervisor PID does not fit i32".to_string())
        .and_then(|pid| pin_process(pid, false))
    {
        Ok(identity) => Some(identity),
        Err(problem) => {
            error = join_error(&error, problem);
            None
        }
    };
    let mut profiler = None;
    if error.is_empty() {
        if config.sudo.is_empty() {
            profiler = supervisor
                .as_ref()
                .map(PinnedProcess::try_clone)
                .transpose()?;
            if let Err(problem) = release_wprof_gate(&exec_gate, ready_timeout) {
                error = join_error(&error, problem);
            }
        } else if let Some(supervisor) = &supervisor {
            match wait_for_wprof_pid(supervisor, &log_path, ready_timeout)
                .and_then(|pid| pin_process(pid, true))
            {
                Ok(identity) => {
                    profiler = Some(identity);
                    if let Err(problem) = release_wprof_gate(&exec_gate, ready_timeout) {
                        error = join_error(&error, problem);
                    }
                }
                Err(problem) => error = join_error(&error, problem),
            }
        }
    }
    if error.is_empty() {
        if let Some(profiler) = &profiler {
            if let Err(problem) = wait_for_wprof_ready(profiler, &log_path, ready_timeout) {
                error = join_error(&error, problem);
            }
        }
    }

    let mut result = None;
    let mut stop_sent = false;
    if error.is_empty() {
        let launch_bridge = GuestLaunchBridge::start(
            &request,
            Duration::from_secs_f64(config.control_ack_timeout_s),
        );
        match (launch_bridge, profiler.as_ref()) {
            (Ok(launch_bridge), Some(profiler)) => {
                let (finished_sender, finished_receiver) = mpsc::sync_channel(1);
                match profiler.try_clone().and_then(|identity| {
                    start_wprof_stop_controller(
                        config.clone(),
                        identity,
                        trial_dir.clone(),
                        request.guest_launch.clone(),
                        window.end_offset_s(),
                        Duration::from_secs_f64(config.profiler_exit_grace_s),
                        finished_receiver,
                    )
                }) {
                    Ok(controller) => {
                        let callback_result = run_trial.run_isolated_trial(&request);
                        let launch = launch_bridge.finish();
                        let _ = finished_sender.send(());
                        let stop = controller.join().unwrap_or_else(|_| WprofStopOutcome {
                            error: "wprof stop controller panicked".to_string(),
                            ..WprofStopOutcome::default()
                        });
                        stop_sent = stop.sent;
                        if !launch.error.is_empty() {
                            error = join_error(&error, launch.error);
                        }
                        if !stop.sent && stop.error.is_empty() {
                            let elapsed = stop.workload_elapsed_s.unwrap_or_default();
                            error = join_error(
                                &error,
                                format!(
                                    "step ended at {elapsed:.6}s before the wprof window ended at {:.6}s",
                                    window.end_offset_s()
                                ),
                            );
                        }
                        if !stop.error.is_empty() {
                            error = join_error(&error, stop.error);
                        }
                        result = match callback_result {
                            Ok(candidate) => match candidate.validate() {
                                Ok(()) => Some(candidate),
                                Err(problem) => {
                                    error = join_error(
                                        &error,
                                        format!("wprof trial failed: {problem}"),
                                    );
                                    None
                                }
                            },
                            Err(problem) => {
                                error =
                                    join_error(&error, format!("wprof trial failed: {problem}"));
                                None
                            }
                        };
                    }
                    Err(problem) => {
                        let launch = launch_bridge.finish();
                        error = join_error(
                            &error,
                            format!("could not start wprof stop controller: {problem}"),
                        );
                        if !launch.error.is_empty() {
                            error = join_error(&error, launch.error);
                        }
                    }
                }
            }
            (Err(problem), _) => error = join_error(&error, problem),
            (Ok(launch_bridge), None) => {
                let launch = launch_bridge.finish();
                error = join_error(&error, "wprof process identity is unavailable");
                if !launch.error.is_empty() {
                    error = join_error(&error, launch.error);
                }
            }
        }
    }

    let (status, teardown_error) = teardown_wprof_group(
        &mut child,
        supervisor.as_ref(),
        config,
        &trial_dir,
        stop_sent,
    );
    if !teardown_error.is_empty() {
        error = join_error(&error, teardown_error);
    }
    let profiler_returncode = status.map(exit_returncode);
    let _ = fs::remove_file(&exec_gate);
    let _ = fs::remove_file(request.guest_launch_ready_path());
    let _ = fs::remove_file(request.guest_launch_release_path());
    if let Err(problem) =
        make_artifacts_private(config, &trial_dir, &[&data_path, &trace_path, &log_path])
    {
        error = join_error(&error, problem);
    }
    let artifacts: Vec<CaptureArtifact> = [
        capture_artifact(capture_root, &data_path, "wprof-data"),
        capture_artifact(capture_root, &trace_path, "perfetto-trace"),
        capture_artifact(capture_root, &log_path, "wprof-log"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let has_data = artifacts
        .iter()
        .any(|artifact| artifact.role == "wprof-data" && artifact.size_bytes > 0);
    let has_trace = artifacts
        .iter()
        .any(|artifact| artifact.role == "perfetto-trace" && artifact.size_bytes > 0);
    if !has_data || !has_trace {
        error = join_error(&error, "wprof produced incomplete artifacts");
    }
    if profiler_returncode.is_some_and(|returncode| returncode != 0) {
        error = join_error(
            &error,
            format!("wprof exited {}", profiler_returncode.unwrap_or_default()),
        );
    }
    if let Some(result) = &result {
        if !result.ok() {
            let detail = if result.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", result.detail)
            };
            error = join_error(
                &error,
                format!("instrumented step exited {}{detail}", result.returncode),
            );
        }
    }
    let complete = result.as_ref().is_some_and(IsolatedTrialResult::ok)
        && profiler_returncode == Some(0)
        && error.is_empty();
    Ok(CaptureTrialRecord {
        trial_id,
        kind: CaptureKind::Wprof,
        state: if complete {
            CaptureState::Complete
        } else {
            CaptureState::Failed
        },
        inner_jobs: selection.inner_jobs,
        expected_wall_s: selection.expected_wall_s,
        window,
        started_at,
        finished_at: utc_now(),
        measured_wall_s: result.as_ref().map(|result| result.wall_s),
        workload_returncode: result.as_ref().map(|result| result.returncode),
        profiler_returncode,
        argv_prefix: Vec::new(),
        artifacts,
        error,
        included_in_model: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn manifest(
    path: &Path,
    capture_id: &str,
    state: CaptureState,
    created_at: &str,
    finished_at: &str,
    selection: &SweetSpotSelection,
    preflight: &BTreeMap<CaptureKind, ToolPreflight>,
    trials: &[CaptureTrialRecord],
    errors: &[String],
) -> CaptureManifest {
    CaptureManifest {
        path: path.to_path_buf(),
        capture_id: capture_id.to_string(),
        state,
        created_at: created_at.to_string(),
        finished_at: finished_at.to_string(),
        machine_id: crate::perflog::machine_id(),
        container_class: crate::perflog::container_class(),
        selection: selection.clone(),
        preflight: [CaptureKind::Perf, CaptureKind::Wprof]
            .into_iter()
            .filter_map(|kind| preflight.get(&kind).cloned())
            .collect(),
        trials: trials.to_vec(),
        errors: errors.to_vec(),
    }
}

fn capture_with_preflight<R, P>(
    selection: &SweetSpotSelection,
    config: &CaptureConfig,
    run_trial: &mut R,
    preflight_runner: P,
) -> Result<CaptureManifest, ProfileCaptureError>
where
    R: RunIsolatedTrial + ?Sized,
    P: FnOnce(&CaptureConfig) -> Result<BTreeMap<CaptureKind, ToolPreflight>, ProfileCaptureError>,
{
    selection
        .validate()
        .map_err(ProfileCaptureError::before_manifest)?;
    config
        .validate()
        .map_err(ProfileCaptureError::before_manifest)?;
    if config.requested_kinds().is_empty() {
        return Err(ProfileCaptureError::before_manifest(
            "at least one profiler capture must be requested",
        ));
    }
    fs::create_dir_all(&config.output_dir)
        .map_err(|error| ProfileCaptureError::before_manifest(error.to_string()))?;
    let (capture_id, capture_root) = new_capture_directory(config, selection)
        .map_err(|error| ProfileCaptureError::before_manifest(error.to_string()))?;
    let manifest_path = capture_root.join("manifest.json");
    let created_at = utc_now();
    let mut trials = Vec::new();
    let mut errors = Vec::new();
    let mut preflight = BTreeMap::new();
    let initial = manifest(
        &manifest_path,
        &capture_id,
        CaptureState::Running,
        &created_at,
        "",
        selection,
        &preflight,
        &trials,
        &errors,
    );
    write_manifest(&initial).map_err(|error| ProfileCaptureError::with_manifest(error, initial))?;

    let orchestration = (|| -> Result<(), String> {
        preflight = preflight_runner(config).map_err(|error| error.to_string())?;
        for kind in config.requested_kinds() {
            let result = preflight
                .get(&kind)
                .ok_or_else(|| format!("{} preflight result is missing", kind.value()))?;
            if !result.usable {
                let detail = if result.diagnostic.is_empty() {
                    format!("{} preflight failed", kind.value())
                } else {
                    result.diagnostic.clone()
                };
                errors.push(format!("{}: {detail}", kind.value()));
            }
        }
        write_manifest(&manifest(
            &manifest_path,
            &capture_id,
            CaptureState::Running,
            &created_at,
            "",
            selection,
            &preflight,
            &trials,
            &errors,
        ))?;

        if config.capture_perf
            && preflight
                .get(&CaptureKind::Perf)
                .is_some_and(|item| item.usable)
        {
            let record = perf_trial(
                &capture_root,
                selection,
                config,
                &preflight[&CaptureKind::Perf],
                run_trial,
            )?;
            if record.state == CaptureState::Failed {
                errors.push(format!("{}: {}", record.trial_id, record.error));
            }
            trials.push(record);
            write_manifest(&manifest(
                &manifest_path,
                &capture_id,
                CaptureState::Running,
                &created_at,
                "",
                selection,
                &preflight,
                &trials,
                &errors,
            ))?;
        }
        if config.wprof_windows > 0
            && preflight
                .get(&CaptureKind::Wprof)
                .is_some_and(|item| item.usable)
        {
            for index in 1..=config.wprof_windows {
                let record = wprof_trial(
                    &capture_root,
                    index,
                    selection,
                    config,
                    &preflight[&CaptureKind::Wprof],
                    run_trial,
                )?;
                let failed = record.state == CaptureState::Failed;
                if failed {
                    errors.push(format!("{}: {}", record.trial_id, record.error));
                }
                trials.push(record);
                if failed {
                    break;
                }
                write_manifest(&manifest(
                    &manifest_path,
                    &capture_id,
                    CaptureState::Running,
                    &created_at,
                    "",
                    selection,
                    &preflight,
                    &trials,
                    &errors,
                ))?;
            }
        }
        Ok(())
    })();

    if let Err(error) = orchestration {
        errors.push(format!("capture orchestration failed: {error}"));
        let failed = manifest(
            &manifest_path,
            &capture_id,
            CaptureState::Failed,
            &created_at,
            &utc_now(),
            selection,
            &preflight,
            &trials,
            &errors,
        );
        let _ = write_manifest(&failed);
        return Err(ProfileCaptureError::with_manifest(error, failed));
    }

    let state = if errors.is_empty() {
        CaptureState::Complete
    } else {
        CaptureState::Failed
    };
    let final_manifest = manifest(
        &manifest_path,
        &capture_id,
        state,
        &created_at,
        &utc_now(),
        selection,
        &preflight,
        &trials,
        &errors,
    );
    write_manifest(&final_manifest)
        .map_err(|error| ProfileCaptureError::with_manifest(error, final_manifest.clone()))?;
    if let Some(error) = errors.first() {
        return Err(ProfileCaptureError::with_manifest(
            error.clone(),
            final_manifest,
        ));
    }
    Ok(final_manifest)
}

/// Capture requested profilers at the recommended width and retain a private manifest.
pub fn capture_at_sweet_spot<R>(
    selection: &SweetSpotSelection,
    config: &CaptureConfig,
    mut run_trial: R,
) -> Result<CaptureManifest, ProfileCaptureError>
where
    R: RunIsolatedTrial,
{
    capture_with_preflight(selection, config, &mut run_trial, preflight_capture_tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimates::SpeedupLevel;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dagrun-profile-capture-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn selection(expected_wall_s: f64) -> SweetSpotSelection {
        SweetSpotSelection {
            step: "build.app".to_string(),
            workload_digest: "0123456789abcdef".to_string(),
            inner_jobs: 8,
            expected_wall_s,
            baseline_inner_jobs: 1,
            speedup: 7.2,
            model_wall_s: expected_wall_s,
            raw_wall_s: Some(expected_wall_s),
            source: "scaling-model-economic-plateau".to_string(),
            git_sha: "deadbeef".to_string(),
        }
    }

    fn usable(kind: CaptureKind, binary: &Path) -> ToolPreflight {
        ToolPreflight {
            kind,
            requested_binary: binary.to_string_lossy().into_owned(),
            resolved_binary: Some(binary.to_string_lossy().into_owned()),
            usable: true,
            returncode: Some(0),
            version: "test".to_string(),
            diagnostic: String::new(),
            sudo: Vec::new(),
        }
    }

    #[test]
    fn sweet_spot_uses_recommended_level_and_raw_wall() {
        let levels = vec![
            SpeedupLevel {
                inner_jobs: 1,
                samples: 3,
                wall_s: 10.0,
                raw_wall_s: Some(12.0),
                wall_min_s: Some(9.8),
                wall_max_s: Some(10.2),
                cpu_s: Some(10.0),
                effective_cores: Some(1.0),
                throttled_s: Some(0.0),
                peak_bytes: None,
                peak_samples: 0,
                peak_floor_bytes: None,
                peak_floor_samples: 0,
                speedup: 1.0,
            },
            SpeedupLevel {
                inner_jobs: 8,
                samples: 3,
                wall_s: 1.4,
                raw_wall_s: Some(1.6),
                wall_min_s: Some(1.3),
                wall_max_s: Some(1.5),
                cpu_s: Some(10.8),
                effective_cores: Some(7.1),
                throttled_s: Some(0.0),
                peak_bytes: None,
                peak_samples: 0,
                peak_floor_bytes: None,
                peak_floor_samples: 0,
                speedup: 7.142857,
            },
        ];
        let model = StepSpeedup {
            step: "build.app".to_string(),
            baseline_inner_jobs: 1,
            recommended_inner_jobs: 8,
            measured_effective_cores: Some(7.1),
            regression_inner_jobs: None,
            levels,
        };
        let selected = select_capture_sweet_spot(&model, "digest", "abc").unwrap();
        assert_eq!(selected.inner_jobs, 8);
        assert_eq!(selected.expected_wall_s, 1.6);
        assert_eq!(selected.model_wall_s, 1.4);
        assert_eq!(selected.source, "scaling-model-economic-plateau");
    }

    #[test]
    fn centered_window_preserves_edges_and_reports_clipping() {
        let ordinary = centered_capture_window(60.0, 0.4).unwrap();
        assert!((ordinary.start_offset_s - 29.8).abs() < 1e-12);
        assert!((ordinary.end_offset_s() - 30.2).abs() < 1e-12);
        assert!(!ordinary.clipped);
        let short = centered_capture_window(0.25, 0.4).unwrap();
        assert!((short.start_offset_s - 0.025).abs() < 1e-12);
        assert!((short.duration_s - 0.2).abs() < 1e-12);
        assert!(short.clipped);
    }

    #[test]
    fn guest_launch_bridge_timestamps_after_delayed_inner_guest_setup() {
        let root = temp_dir("guest-launch-delay");
        let request = IsolatedTrialRequest {
            trial_id: "perf-001".to_string(),
            step: "build.app".to_string(),
            inner_jobs: 8,
            kind: CaptureKind::Perf,
            output_dir: root.clone(),
            expected_wall_s: 0.2,
            window: centered_capture_window(0.2, 0.04).unwrap(),
            argv_prefix: Vec::new(),
            env: BTreeMap::new(),
            guest_launch: GuestLaunchSignal::default(),
            include_in_model: false,
        };
        prepare_guest_launch_fifos(&request).unwrap();
        let before_setup = Instant::now();
        let bridge = GuestLaunchBridge::start(&request, Duration::from_secs(1)).unwrap();
        let ready_path = request.guest_launch_ready_path();
        let release_path = request.guest_launch_release_path();
        let guest = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            fs::write(ready_path, format!("{}\n", std::process::id())).unwrap();
            let mut release = String::new();
            File::open(release_path)
                .unwrap()
                .read_to_string(&mut release)
                .unwrap();
            release
        });
        let launch = request
            .guest_launch
            .wait(Duration::from_secs(1))
            .expect("bridge should publish launch");
        assert!(launch.monotonic.duration_since(before_setup) >= Duration::from_millis(90));
        assert_eq!(guest.join().unwrap().trim(), "go");
        assert!(bridge.finish().released);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn perf_capture_uses_fifo_window_and_never_models_trial() {
        let root = temp_dir("perf");
        let mut config = CaptureConfig::new(&root);
        config.capture_perf = true;
        config.perf_window_s = Some(0.02);
        let fake = PathBuf::from("/fake/perf");
        let preflight = BTreeMap::from([(CaptureKind::Perf, usable(CaptureKind::Perf, &fake))]);
        let mut commands = Vec::new();
        let mut run = |request: &IsolatedTrialRequest| {
            assert!(!request.include_in_model);
            assert_eq!(request.inner_jobs, 8);
            let control_spec = &request.argv_prefix[request
                .argv_prefix
                .iter()
                .position(|value| value == "--control")
                .unwrap()
                + 1];
            let paths = control_spec.strip_prefix("fifo:").unwrap();
            let (control_path, ack_path) = paths.split_once(',').unwrap();
            let output_path = PathBuf::from(
                &request.argv_prefix[request
                    .argv_prefix
                    .iter()
                    .position(|value| value == "--output")
                    .unwrap()
                    + 1],
            );
            let control_path = PathBuf::from(control_path);
            let ack_path = PathBuf::from(ack_path);
            let responder = thread::spawn(move || {
                let mut ack = OpenOptions::new().write(true).open(ack_path).unwrap();
                let mut control = OpenOptions::new().read(true).open(control_path).unwrap();
                let mut seen = Vec::new();
                for _ in 0..2 {
                    let mut bytes = Vec::new();
                    loop {
                        let mut byte = [0u8; 1];
                        control.read_exact(&mut byte).unwrap();
                        if byte[0] == b'\n' {
                            break;
                        }
                        bytes.push(byte[0]);
                    }
                    seen.push(String::from_utf8(bytes).unwrap());
                    ack.write_all(b"ack\n").unwrap();
                }
                fs::write(output_path, b"PERFILE2\0test").unwrap();
                seen
            });
            // Scheduler/cgroup/profiler preparation before the real guest must not consume the
            // centred profiling window.
            thread::sleep(Duration::from_millis(100));
            request
                .notify_guest_launched(&request.step, std::process::id(), Instant::now())
                .unwrap();
            thread::sleep(Duration::from_millis(140));
            commands = responder.join().unwrap();
            Ok(IsolatedTrialResult {
                returncode: 0,
                wall_s: 0.14,
                detail: String::new(),
            })
        };
        let manifest =
            capture_with_preflight(&selection(0.12), &config, &mut run, |_| Ok(preflight)).unwrap();
        assert_eq!(commands, ["enable", "disable"]);
        assert_eq!(manifest.state, CaptureState::Complete);
        assert!(!manifest.trials[0].included_in_model);
        assert_eq!(manifest.trials[0].state, CaptureState::Complete);
        assert_eq!(
            fs::metadata(manifest.path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&manifest.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            manifest.to_json()["schema"],
            PROFILE_CAPTURE_MANIFEST_SCHEMA
        );
        assert_eq!(manifest.to_json()["trials"][0]["included_in_model"], false);
        assert_eq!(
            manifest.to_json()["trials"][0]["workload_returncode"],
            Value::Null
        );
        assert_eq!(manifest.to_json()["trials"][0]["profiler_returncode"], 0);
        assert!(!manifest.machine_id.is_empty());
        assert!(!manifest.container_class.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wprof_windows_are_separate_private_trials_with_interoperable_manifest() {
        let root = temp_dir("wprof");
        let fake = root.join("fake-wprof");
        fs::write(
            &fake,
            r#"#!/bin/sh
data=''
trace=''
args="$*"
for arg in "$@"; do
  case "$arg" in
    --data=*) data=${arg#--data=} ;;
    --trace=*) trace=${arg#--trace=} ;;
  esac
done
trap 'printf wprof-data > "$data"; printf perfetto-trace > "$trace"; printf "fake wprof complete\n"; exit 0' INT
printf 'args:%s\n' "$args"
# Deliberately exceed the 70ms centred stop offset. The workload must not start and the stop
# clock must not begin until this readiness line is observed.
sleep 0.10
printf 'Running in flight recorder mode, press Ctrl-C to stop...\n'
while :; do sleep 0.01; done
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = CaptureConfig::new(&root);
        config.wprof_windows = 2;
        config.wprof_window_s = 0.02;
        let preflight = BTreeMap::from([(CaptureKind::Wprof, usable(CaptureKind::Wprof, &fake))]);
        let mut requests = Vec::new();
        let mut run = |request: &IsolatedTrialRequest| {
            requests.push(request.trial_id.clone());
            assert!(request.argv_prefix.is_empty());
            assert!(!request.include_in_model);
            assert!(fs::read_to_string(request.output_dir.join("wprof.log"))
                .unwrap()
                .contains(WPROF_READY_PREFIX));
            // This exceeds the old readiness-relative stop offset. The window must remain armed
            // until the actual guest launch is explicitly reported.
            thread::sleep(Duration::from_millis(100));
            request
                .notify_guest_launched(&request.step, std::process::id(), Instant::now())
                .unwrap();
            thread::sleep(Duration::from_millis(130));
            Ok(IsolatedTrialResult {
                returncode: 0,
                wall_s: 0.13,
                detail: String::new(),
            })
        };
        let manifest =
            capture_with_preflight(&selection(0.12), &config, &mut run, |_| Ok(preflight)).unwrap();
        assert_eq!(requests, ["wprof-001", "wprof-002"]);
        assert_eq!(manifest.state, CaptureState::Complete);
        for trial in &manifest.trials {
            assert_eq!(trial.state, CaptureState::Complete);
            let roles: BTreeMap<&str, u64> = trial
                .artifacts
                .iter()
                .map(|artifact| (artifact.role.as_str(), artifact.size_bytes))
                .collect();
            assert!(roles["wprof-data"] > 0);
            assert!(roles["perfetto-trace"] > 0);
            assert!(roles.contains_key("wprof-log"));
            for artifact in &trial.artifacts {
                assert_eq!(artifact.mode, "0600");
            }
        }
        let log = fs::read_to_string(manifest.path.parent().unwrap().join("wprof-001/wprof.log"))
            .unwrap();
        assert!(log.contains("--flight-record=0.020000s"));
        assert!(!log.contains("--activate="));
        assert!(!log.contains("--dur="));
        let payload: Value =
            serde_json::from_str(&fs::read_to_string(&manifest.path).unwrap()).unwrap();
        assert_eq!(payload, manifest.to_json());
        assert_eq!(payload["artifact_root"], ".");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wprof_readiness_timeout_skips_workload_and_retains_failure_log() {
        let root = temp_dir("wprof-not-ready");
        let fake = root.join("fake-wprof");
        fs::write(
            &fake,
            r#"#!/bin/sh
printf 'still preparing\n'
while :; do sleep 0.01; done
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = CaptureConfig::new(&root);
        config.wprof_windows = 1;
        config.wprof_window_s = 0.02;
        config.wprof_ready_timeout_s = 0.05;
        config.profiler_exit_grace_s = 0.05;
        let preflight = BTreeMap::from([(CaptureKind::Wprof, usable(CaptureKind::Wprof, &fake))]);
        let ran = Arc::new(AtomicBool::new(false));
        let callback_ran = Arc::clone(&ran);
        let mut run = move |_request: &IsolatedTrialRequest| {
            callback_ran.store(true, Ordering::Release);
            Ok(IsolatedTrialResult {
                returncode: 0,
                wall_s: 0.1,
                detail: String::new(),
            })
        };
        let error = capture_with_preflight(&selection(0.12), &config, &mut run, |_| Ok(preflight))
            .unwrap_err();
        let manifest = error.manifest.unwrap();
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(manifest.trials.len(), 1);
        assert!(manifest.trials[0]
            .error
            .contains("did not announce flight-recorder readiness"));
        let log = manifest.trials[0]
            .artifacts
            .iter()
            .find(|artifact| artifact.role == "wprof-log")
            .unwrap();
        assert!(
            fs::read_to_string(manifest.path.parent().unwrap().join(&log.path))
                .unwrap()
                .contains("still preparing")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wprof_early_exit_after_ready_is_safe_and_reaps_its_process_group() {
        let root = temp_dir("wprof-early-exit");
        let fake = root.join("early-wprof");
        let descendant_path = root.join("descendant.pid");
        let exit_gate = root.join("exit.fifo");
        create_fifo(&exit_gate).unwrap();
        fs::write(
            &fake,
            r#"#!/bin/sh
child_file=''
exit_gate=''
for arg in "$@"; do
  case "$arg" in
    --child-file=*) child_file=${arg#--child-file=} ;;
    --exit-gate=*) exit_gate=${arg#--exit-gate=} ;;
  esac
done
sleep 30 &
printf '%s\n' "$!" > "$child_file"
printf 'Running in flight recorder mode, press Ctrl-C to stop...\n'
IFS= read -r release < "$exit_gate" || exit 125
[ "$release" = exit ] || exit 125
exit 0
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = CaptureConfig::new(&root);
        config.wprof_windows = 1;
        config.wprof_window_s = 0.02;
        config.profiler_exit_grace_s = 0.05;
        config.wprof_args.extend([
            format!("--child-file={}", descendant_path.display()),
            format!("--exit-gate={}", exit_gate.display()),
        ]);
        let preflight = BTreeMap::from([(CaptureKind::Wprof, usable(CaptureKind::Wprof, &fake))]);
        let mut run = |request: &IsolatedTrialRequest| {
            // Readiness is already confirmed before this callback. Wait for the
            // controller to consume guest launch as well, so the deliberate exit
            // cannot be misclassified as occurring before the window opened.
            let (events, observed) = mpsc::sync_channel(2);
            *request
                .guest_launch
                .state
                .wprof_control_events
                .lock()
                .unwrap() = Some(events);
            let started = Instant::now();
            request.notify_guest_launched(&request.step, std::process::id(), started)?;
            let await_event = |expected| -> Result<(), String> {
                match observed.recv_timeout(Duration::from_secs(10)) {
                    Ok(actual) if actual == expected => Ok(()),
                    Ok(actual) => Err(format!("expected control event {expected}, got {actual}")),
                    Err(error) => Err(format!("did not observe control event {expected}: {error}")),
                }
            };
            await_event("launch-observed")?;
            let mut gate =
                open_fifo_writer(&exit_gate, Duration::from_secs(10), &AtomicBool::new(false))
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "fake profiler did not wait on its exit gate".to_string())?;
            gate.write_all(b"exit\n")
                .map_err(|error| error.to_string())?;
            // Keep the callback active until the controller observes the exit;
            // returning first would instead exercise the workload-finished path.
            await_event("early-exit-observed")?;
            Ok(IsolatedTrialResult {
                returncode: 0,
                wall_s: started.elapsed().as_secs_f64(),
                detail: String::new(),
            })
        };
        // The endpoint is only a watchdog. Events, not elapsed sleeps, cause the
        // tested early-exit path; no test waits for this deliberately distant end.
        let error = capture_with_preflight(&selection(120.0), &config, &mut run, |_| Ok(preflight))
            .unwrap_err();
        let manifest = error.manifest.unwrap();
        assert!(
            manifest.trials[0]
                .error
                .contains("wprof exited before the profiling window ended"),
            "{}",
            manifest.trials[0].error,
        );
        let descendant = fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let reap_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 performs existence/permission checking only.
            if unsafe { libc::kill(descendant, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                Instant::now() < reap_deadline,
                "wprof descendant {descendant} remained observable after process-group teardown"
            );
            thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn privileged_wprof_launch_reports_the_exact_signal_target() {
        let root = temp_dir("wprof-command");
        let mut config = CaptureConfig::new(&root);
        config.sudo = vec!["sudo".to_string(), "-n".to_string()];
        let command = wprof_flight_command(
            &config,
            Path::new("/usr/local/bin/wprof"),
            Path::new("/tmp/wprof.data"),
            Path::new("/tmp/trace.pb"),
            Some(Path::new("/tmp/wprof-exec-gate.fifo")),
            centered_capture_window(1.0, 0.4).unwrap(),
            &selection(1.0),
        );
        assert_eq!(&command[..4], ["sudo", "-n", "/bin/sh", "-c"]);
        assert!(command[4].contains(WPROF_PID_PREFIX));
        assert_eq!(command[5], "dagrun-wprof");
        assert_eq!(command[6], "/tmp/wprof-exec-gate.fifo");
        assert_eq!(command[7], "/usr/local/bin/wprof");
        assert!(command.iter().any(|arg| arg == "--flight-record=0.400000s"));
        assert!(!command.iter().any(|arg| arg.starts_with("--activate=")));
        assert!(!command.iter().any(|arg| arg.starts_with("--dur=")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wprof_failure_is_explicit_and_retains_manifest_and_log() {
        let root = temp_dir("wprof-failure");
        let fake = root.join("failing-wprof");
        fs::write(
            &fake,
            r#"#!/bin/sh
data=''
trace=''
for arg in "$@"; do
  case "$arg" in
    --data=*) data=${arg#--data=} ;;
    --trace=*) trace=${arg#--trace=} ;;
  esac
done
trap 'printf wprof-data > "$data"; printf perfetto-trace > "$trace"; printf "fake wprof failed\n"; exit 7' INT
printf 'Running in flight recorder mode, press Ctrl-C to stop...\n'
while :; do sleep 0.01; done
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = CaptureConfig::new(&root);
        config.wprof_windows = 2;
        config.wprof_window_s = 0.02;
        let preflight = BTreeMap::from([(CaptureKind::Wprof, usable(CaptureKind::Wprof, &fake))]);
        let mut run = |request: &IsolatedTrialRequest| {
            request
                .notify_guest_launched(&request.step, std::process::id(), Instant::now())
                .unwrap();
            thread::sleep(Duration::from_millis(130));
            Ok(IsolatedTrialResult {
                returncode: 0,
                wall_s: 0.13,
                detail: String::new(),
            })
        };
        let error = capture_with_preflight(&selection(0.12), &config, &mut run, |_| Ok(preflight))
            .unwrap_err();
        let manifest = error.manifest.unwrap();
        assert!(manifest.path.exists());
        assert_eq!(manifest.state, CaptureState::Failed);
        assert_eq!(manifest.trials.len(), 1);
        assert_eq!(manifest.trials[0].profiler_returncode, Some(7));
        assert!(manifest.trials[0].error.contains("wprof exited 7"));
        let log = manifest.trials[0]
            .artifacts
            .iter()
            .find(|artifact| artifact.role == "wprof-log")
            .unwrap();
        assert!(
            !fs::read_to_string(manifest.path.parent().unwrap().join(&log.path))
                .unwrap()
                .trim()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_tools_are_explicit_and_never_add_sudo() {
        let root = temp_dir("missing");
        let mut config = CaptureConfig::new(&root);
        config.capture_perf = true;
        config.wprof_windows = 1;
        config.perf_binary = "dagrun-test-no-such-perf".to_string();
        config.wprof_binary = "dagrun-test-no-such-wprof".to_string();
        let results = preflight_capture_tools(&config).unwrap();
        assert!(!results[&CaptureKind::Perf].usable);
        assert!(!results[&CaptureKind::Wprof].usable);
        assert!(results[&CaptureKind::Perf].sudo.is_empty());
        assert!(results[&CaptureKind::Wprof].sudo.is_empty());
        assert!(!fs::read_dir(&root).unwrap().flatten().any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(".capture-preflight-")));
        fs::remove_dir_all(root).unwrap();
    }
}
