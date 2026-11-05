//! Durable, serialized messaging for interactive agents hosted by Herdr.
//!
//! This module owns durable FIFO files, target validation, idle/done readiness, atomic multiline
//! submission, working-state confirmation, and at-most-once quarantine after an ambiguous pane
//! injection. Queue and target locks use the command's stable, package-independent disk format.

use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::client::{AgentPaneInfo, HerdrClient, Pane};
use crate::error::{AdapterError, AdapterErrorKind, EXIT_BUSY, EXIT_TIMEOUT};
use crate::submission::{PromptTerminal, Submission, SubmitTimeouts};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MESSAGE_ID_MAX: usize = 255;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Identity assertions for one already-running interactive Herdr agent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Target {
    /// Exact pane identifier, when known.
    pub pane_id: Option<String>,
    /// Agent name associated with the stable session value, when known.
    pub session_agent: Option<String>,
    /// Stable interactive-agent session value, when known.
    pub session_value: Option<String>,
    /// Expected live agent implementation.
    pub expected_agent: Option<String>,
    /// Expected live workspace label.
    pub expected_workspace: Option<String>,
    /// Expected live working directory.
    pub expected_cwd: Option<PathBuf>,
}

/// Overall state produced by one queue drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueOutcome {
    /// Every valid prompt examined by this drain was confirmed delivered.
    Delivered,
    /// At least one prompt remains safely queued before injection.
    Pending,
    /// At least one prompt may have been injected and was quarantined rather than retried.
    PossiblySubmitted,
}

/// Exact durable location of one caller-selected queue message identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueMessageState {
    /// The message is known not to have crossed the injection barrier.
    Pending,
    /// The message crossed the barrier and its outcome remains unsettled.
    Inflight,
    /// The native working transition confirmed delivery.
    Processed,
    /// Delivery may have occurred and automatic replay is unsafe.
    Failed,
}

impl QueueOutcome {
    /// Return the stable machine-readable spelling used by both packages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Pending => "pending",
            Self::PossiblySubmitted => "possibly_submitted",
        }
    }
}

/// Structured outcome of one durable queue send or drain operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueueResult {
    /// Identifier of the prompt created by `send`, or an empty string for a plain drain.
    pub message_id: String,
    /// Prompt identifiers whose working transition was confirmed.
    pub delivered: Vec<String>,
    /// Prompt identifiers retained in `failed` because retrying could be unsafe.
    pub quarantined: Vec<String>,
    /// Prompt identifiers that remain safe in `inbox`.
    pub pending: Vec<String>,
    /// Readiness or identity failure that stopped FIFO progress.
    pub blocked: Option<String>,
    /// Machine-readable aggregate state.
    pub outcome: QueueOutcome,
}

/// Validated live-agent identity plus observational queue state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueueStatus {
    /// Resolved live pane identifier.
    pub pane_id: String,
    /// Live interactive-agent implementation, when reported.
    pub agent: Option<String>,
    /// Native live Herdr agent state.
    pub agent_status: String,
    /// Agent name from the stable session identity, when present.
    pub session_agent: Option<String>,
    /// Stable session value, when present.
    pub session_value: Option<String>,
    /// Owning workspace identifier.
    pub workspace_id: String,
    /// Live pane working directory.
    pub cwd: String,
    /// Prompt identifiers waiting safely before injection.
    pub pending: Vec<String>,
    /// Prompt identifiers behind the durable injection-intent barrier.
    pub inflight: Vec<String>,
    /// Prompt identifiers retained after malformed or ambiguous delivery.
    pub failed: Vec<String>,
}

/// One durable prompt that was not confirmed delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndeliveredMessage {
    /// Human-readable diagnostic.
    pub message: String,
    /// Stable prompt identifier.
    pub message_id: String,
    /// Durable artifact holding the prompt.
    pub artifact: PathBuf,
}

/// Typed failure outcomes for interactive-agent delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentError {
    /// Herdr itself or its control protocol is unavailable.
    Client(AdapterError),
    /// Delivery could not proceed, with no more specific send outcome.
    Delivery(String),
    /// Nothing was injected; the durable prompt remains safe to retry.
    Pending(UndeliveredMessage),
    /// Injection may have succeeded; an automatic retry could duplicate a turn.
    PossiblySubmitted(UndeliveredMessage),
}

impl AgentError {
    fn delivery(message: impl Into<String>) -> Self {
        Self::Delivery(message.into())
    }

    /// Return the process status assigned to this failure.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Client(error) => error.exit_code(),
            Self::Delivery(_) | Self::Pending(_) => EXIT_BUSY,
            Self::PossiblySubmitted(_) => EXIT_TIMEOUT,
        }
    }

    /// Return the durable message details for typed send outcomes.
    #[must_use]
    pub const fn undelivered(&self) -> Option<&UndeliveredMessage> {
        match self {
            Self::Pending(message) | Self::PossiblySubmitted(message) => Some(message),
            Self::Client(_) | Self::Delivery(_) => None,
        }
    }

    /// Return the stable machine-readable outcome for typed send failures.
    #[must_use]
    pub const fn outcome(&self) -> Option<QueueOutcome> {
        match self {
            Self::Client(_) | Self::Delivery(_) => None,
            Self::Pending(_) => Some(QueueOutcome::Pending),
            Self::PossiblySubmitted(_) => Some(QueueOutcome::PossiblySubmitted),
        }
    }

    /// Report whether repeating the same send is known to be safe.
    #[must_use]
    pub const fn safe_to_retry(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => fmt::Display::fmt(error, formatter),
            Self::Delivery(message) => formatter.write_str(message),
            Self::Pending(message) | Self::PossiblySubmitted(message) => {
                formatter.write_str(&message.message)
            }
        }
    }
}

impl std::error::Error for AgentError {}

impl From<AdapterError> for AgentError {
    fn from(error: AdapterError) -> Self {
        Self::Client(error)
    }
}

/// Result type used by durable interactive-agent operations.
pub type AgentResult<T> = std::result::Result<T, AgentError>;

/// Herdr operations required by the durable interactive-agent transport.
pub trait AgentApi: Send + Sync {
    /// List every live pane.
    fn panes(&self) -> crate::error::Result<Vec<Pane>>;
    /// Read validated identity and readiness fields for one pane.
    fn pane_info(&self, pane_id: &str) -> crate::error::Result<AgentPaneInfo>;
    /// Resolve one exact workspace identifier to its live label.
    fn workspace_label(&self, workspace_id: &str) -> crate::error::Result<String>;
    /// Atomically type and submit one prompt.
    fn run(&self, pane_id: &str, text: &str) -> crate::error::Result<()>;
    /// Wait for a native agent-state transition.
    fn wait_agent_status(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
    ) -> crate::error::Result<()>;
    /// Read rendered terminal text.
    fn read(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
    ) -> crate::error::Result<String>;

    /// Cancellation-aware pane enumeration used by long-running service owners.
    fn panes_with_runtime(&self, _runtime: &dyn AgentRuntime) -> crate::error::Result<Vec<Pane>> {
        self.panes()
    }
    /// Cancellation-aware pane inspection used by long-running service owners.
    fn pane_info_with_runtime(
        &self,
        pane_id: &str,
        _runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<AgentPaneInfo> {
        self.pane_info(pane_id)
    }
    /// Cancellation-aware workspace inspection used by long-running service owners.
    fn workspace_label_with_runtime(
        &self,
        workspace_id: &str,
        _runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<String> {
        self.workspace_label(workspace_id)
    }
    /// Cancellation-aware prompt submission used by long-running service owners.
    fn run_with_runtime(
        &self,
        pane_id: &str,
        text: &str,
        _runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<()> {
        self.run(pane_id, text)
    }
    /// Submit one prompt and report how the submission was established.
    ///
    /// The default hands the prompt to [`AgentApi::run_with_runtime`], which
    /// proves nothing, so the queue then waits for a native working transition.
    fn submit_with_runtime(
        &self,
        pane_id: &str,
        text: &str,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<Submission> {
        self.run_with_runtime(pane_id, text, runtime)
            .map(|()| Submission::Unconfirmed)
    }
    /// Cancellation-aware native status wait used by long-running service owners.
    fn wait_agent_status_with_runtime(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
        _runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<()> {
        self.wait_agent_status(pane_id, status, timeout_ms)
    }
    /// Cancellation-aware pane read used by long-running service owners.
    fn read_with_runtime(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
        _runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<String> {
        self.read(pane_id, source, lines)
    }
    /// Verify an idle Codex composer when its native status is unknown.
    ///
    /// Implementations must independently prove the same live recipient around the screen
    /// observation. Unsupported adapters leave unknown status unready.
    fn codex_idle_ready_with_runtime(
        &self,
        _info: &AgentPaneInfo,
        _runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<bool> {
        Ok(false)
    }
}

impl AgentApi for HerdrClient {
    fn panes(&self) -> crate::error::Result<Vec<Pane>> {
        HerdrClient::panes(self, None)
    }

    fn pane_info(&self, pane_id: &str) -> crate::error::Result<AgentPaneInfo> {
        HerdrClient::pane_info(self, pane_id)
    }

    fn codex_idle_ready_with_runtime(
        &self,
        info: &AgentPaneInfo,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<bool> {
        self.codex_idle_ready_with_cancellation(info, &|| runtime.cancelled())
    }

    fn workspace_label(&self, workspace_id: &str) -> crate::error::Result<String> {
        HerdrClient::workspace_label(self, workspace_id)
    }

    fn run(&self, pane_id: &str, text: &str) -> crate::error::Result<()> {
        HerdrClient::prompt_agent(self, pane_id, text)
    }

    fn wait_agent_status(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
    ) -> crate::error::Result<()> {
        HerdrClient::wait_agent_status(self, pane_id, status, timeout_ms)
    }

    fn read(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
    ) -> crate::error::Result<String> {
        HerdrClient::read(self, pane_id, source, lines)
    }

    fn panes_with_runtime(&self, runtime: &dyn AgentRuntime) -> crate::error::Result<Vec<Pane>> {
        HerdrClient::panes_with_cancellation(self, None, &|| runtime.cancelled())
    }

    fn pane_info_with_runtime(
        &self,
        pane_id: &str,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<AgentPaneInfo> {
        HerdrClient::pane_info_with_cancellation(self, pane_id, &|| runtime.cancelled())
    }

    fn workspace_label_with_runtime(
        &self,
        workspace_id: &str,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<String> {
        HerdrClient::workspace_label_with_cancellation(self, workspace_id, &|| runtime.cancelled())
    }

    fn run_with_runtime(
        &self,
        pane_id: &str,
        text: &str,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::prompt_agent_with_cancellation(self, pane_id, text, &|| runtime.cancelled())
    }

    fn submit_with_runtime(
        &self,
        pane_id: &str,
        text: &str,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<Submission> {
        let cancelled = || runtime.cancelled();
        let harness = match HerdrClient::pane_info_with_cancellation(self, pane_id, &cancelled) {
            Ok(info) => info.agent.unwrap_or_default(),
            Err(error) => {
                return Ok(Submission::NotStaged(format!(
                    "pane {pane_id}: harness lookup failed before typing: {error}"
                )))
            }
        };
        if crate::submission::verifies(&harness, text) {
            let terminal = HerdrTerminal {
                client: self,
                runtime,
            };
            return crate::submission::submit_verified(
                &terminal,
                pane_id,
                &harness,
                text,
                SubmitTimeouts::default(),
                runtime,
            );
        }
        HerdrClient::prompt_agent_with_cancellation(self, pane_id, text, &cancelled)
            .map(|()| Submission::Unconfirmed)
    }

    fn wait_agent_status_with_runtime(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::wait_agent_status_with_cancellation(self, pane_id, status, timeout_ms, &|| {
            runtime.cancelled()
        })
    }

    fn read_with_runtime(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
        runtime: &dyn AgentRuntime,
    ) -> crate::error::Result<String> {
        HerdrClient::read_with_cancellation(self, pane_id, source, lines, &|| runtime.cancelled())
    }
}

/// Herdr pane operations bound to one cancellation source.
struct HerdrTerminal<'a> {
    client: &'a HerdrClient,
    runtime: &'a dyn AgentRuntime,
}

impl PromptTerminal for HerdrTerminal<'_> {
    fn read_screen(&self, pane_id: &str) -> crate::error::Result<String> {
        self.client
            .read_screen_with_cancellation(pane_id, &|| self.runtime.cancelled())
    }
    fn send_text(&self, pane_id: &str, text: &str) -> crate::error::Result<()> {
        self.client
            .send_text_with_cancellation(pane_id, text, &|| self.runtime.cancelled())
    }
    fn send_keys(&self, pane_id: &str, keys: &str) -> crate::error::Result<()> {
        self.client
            .send_keys_with_cancellation(pane_id, keys, &|| self.runtime.cancelled())
    }
}

/// Timing and retry controls for one queue drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainOptions {
    /// Maximum pre-injection wait for idle/done readiness.
    pub ready_timeout: Duration,
    /// Maximum post-injection wait for the native working transition.
    pub working_timeout: Duration,
    /// Maximum recorded delivery-attempt count accepted for a prompt.
    pub max_attempts: u64,
    /// Also type into a Claude Code pane that Herdr reports `working`, and let Claude Code
    /// queue the prompt behind its current turn. The submission must still be verified against
    /// the composer, which must be empty before typing; a prompt Claude Code never accepts is
    /// taken back out of the composer and stays pending. Slash commands and other harnesses
    /// still wait for `idle` or `done`. Off by default.
    pub deliver_while_working: bool,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            ready_timeout: Duration::from_secs(900),
            working_timeout: Duration::from_secs(30),
            max_attempts: 3,
            deliver_while_working: false,
        }
    }
}

/// Injectable clock used to make readiness polling deterministic in tests and embedders.
pub trait AgentRuntime {
    /// Return a monotonic duration from an arbitrary fixed origin.
    fn monotonic(&self) -> Duration;
    /// Pause for at most `duration`.
    fn sleep(&self, duration: Duration);
    /// Return whether a service owner requested cancellation.
    fn cancelled(&self) -> bool {
        false
    }
    /// Bound one native working-state wait so cancellation can be observed.
    fn delivery_wait_chunk(&self) -> Option<Duration> {
        None
    }
    /// Note that a queue drain typed the queued prompt `message_id` and verified the submission
    /// against the agent's composer: the pane printed the prompt after the submission key, or
    /// the prompt left the composer that was checked before typing, as when the agent queued it
    /// behind another prompt and showed nothing of it. A submission confirmed only by the pane's
    /// status is not noted. The drain calls this before it records the prompt as processed, so
    /// a note saved here is not lost when the drain stops in between. The default ignores the
    /// note.
    fn prompt_printed(&self, message_id: &str) {
        let _ = message_id;
    }
    /// Whether a verified submission that the agent never accepts, so that the prompt is still
    /// staged in its composer when the submission deadline passes, should take the prompt back
    /// out of the composer and report it as not typed, rather than leave it there and report the
    /// outcome as unknown. A queue drain asks for this only when it types into a pane that is
    /// working, as `DrainOptions::deliver_while_working` allows. The default is `false`.
    fn withdraw_unaccepted_prompt(&self) -> bool {
        false
    }
}

/// A runtime that behaves as `inner` and asks a verified submission to withdraw a prompt the
/// agent never accepts: see [`AgentRuntime::withdraw_unaccepted_prompt`].
struct WithdrawingRuntime<'a> {
    inner: &'a dyn AgentRuntime,
}

impl AgentRuntime for WithdrawingRuntime<'_> {
    fn monotonic(&self) -> Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        self.inner.sleep(duration);
    }

    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }

    fn delivery_wait_chunk(&self) -> Option<Duration> {
        self.inner.delivery_wait_chunk()
    }

    fn prompt_printed(&self, message_id: &str) {
        self.inner.prompt_printed(message_id);
    }

    fn withdraw_unaccepted_prompt(&self) -> bool {
        true
    }
}

/// Production wall-clock runtime for readiness polling.
#[derive(Clone, Copy, Debug)]
pub struct SystemRuntime {
    origin: Instant,
}

impl Default for SystemRuntime {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl AgentRuntime for SystemRuntime {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Resolve by stable session when supplied, then revalidate every asserted field.
pub fn resolve_target<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
) -> AgentResult<AgentPaneInfo> {
    resolve_target_with_runtime(client, target, &SystemRuntime::default())
}

pub(crate) fn resolve_target_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    runtime: &dyn AgentRuntime,
) -> AgentResult<AgentPaneInfo> {
    validate_target_authority(target)?;
    let asserted_pane = target.pane_id.as_deref();
    let mut pane_id = target.pane_id.clone();
    if let Some(session_value) = target.session_value.as_deref() {
        let mut matches = Vec::new();
        for pane in client.panes_with_runtime(runtime).map_err(client_error)? {
            let info = client
                .pane_info_with_runtime(&pane.pane_id, runtime)
                .map_err(client_error)?;
            if info.session_value.as_deref() == Some(session_value)
                && target
                    .session_agent
                    .as_deref()
                    .is_none_or(|agent| info.session_agent.as_deref() == Some(agent))
            {
                matches.push(pane.pane_id);
            }
        }
        if matches.len() != 1 {
            return Err(AgentError::delivery(format!(
                "expected exactly one live pane for session {session_value:?}, found {}",
                matches.len()
            )));
        }
        let resolved = matches.remove(0);
        if asserted_pane.is_some_and(|asserted| asserted != resolved) {
            return Err(AgentError::delivery(format!(
                "refusing session target pane {resolved:?}: expected exact pane {asserted_pane:?}"
            )));
        }
        pane_id = Some(resolved);
    }
    let pane_id = pane_id
        .ok_or_else(|| AgentError::delivery("target needs --pane or a stable session value"))?;
    let info = client
        .pane_info_with_runtime(&pane_id, runtime)
        .map_err(client_error)?;
    validate_target_with_runtime(client, &info, target, runtime)?;
    Ok(info)
}

fn validate_target_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    info: &AgentPaneInfo,
    target: &Target,
    runtime: &dyn AgentRuntime,
) -> AgentResult<()> {
    let mut failures = Vec::new();
    if let Some(expected) = target.expected_agent.as_deref() {
        if info.agent.as_deref() != Some(expected) {
            failures.push(format!("agent is {:?}, expected {expected:?}", info.agent));
        }
    }
    if let Some(expected) = target.session_agent.as_deref() {
        if info.session_agent.as_deref() != Some(expected) {
            failures.push(format!(
                "session agent is {:?}, expected {expected:?}",
                info.session_agent
            ));
        }
    }
    if let Some(expected) = target.session_value.as_deref() {
        if info.session_value.as_deref() != Some(expected) {
            failures.push(format!(
                "session is {:?}, expected {expected:?}",
                info.session_value
            ));
        }
    }
    if let Some(expected) = target.expected_workspace.as_deref() {
        let actual = client
            .workspace_label_with_runtime(&info.workspace_id, runtime)
            .map_err(client_error)?;
        if actual != expected {
            failures.push(format!("workspace is {actual:?}, expected {expected:?}"));
        }
    }
    if let Some(expected) = target.expected_cwd.as_deref() {
        let actual = real_path(Path::new(&info.cwd))?;
        let expected = real_path(expected)?;
        if actual != expected {
            failures.push(format!(
                "cwd is {:?}, expected {:?}",
                info.cwd,
                expected.display().to_string()
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AgentError::delivery(format!(
            "refusing pane {}: {}",
            info.pane_id,
            failures.join("; ")
        )))
    }
}

/// Persist one prompt before any readiness or transport operation.
pub fn enqueue(root: &Path, text: &str, message_id: Option<&str>) -> AgentResult<String> {
    enqueue_internal(root, text, message_id, true, &SystemRuntime::default())
}

fn enqueue_internal(
    root: &Path,
    text: &str,
    message_id: Option<&str>,
    serialize: bool,
    runtime: &dyn AgentRuntime,
) -> AgentResult<String> {
    if text.is_empty() {
        return Err(AgentError::delivery("message must not be empty"));
    }
    let directories = prepare(root)?;
    let identifier = message_id.map_or_else(generated_message_id, str::to_owned);
    validate_message_id(&identifier)?;
    let filename = format!("{identifier}.json");
    let path = directories.inbox.join(&filename);
    let _lock = if serialize {
        let directory_identity = queue_directory_identity(root)?;
        let lock_path = root.join(".delivery.lock");
        let lock = open_private_lock(&lock_path, "queue delivery lock")?;
        lock_exclusive_with_runtime(&lock, &lock_path, "queue delivery", runtime)?;
        verify_queue_lock(
            root,
            directory_identity,
            &lock_path,
            &lock,
            "queue delivery lock",
        )?;
        Some(lock)
    } else {
        None
    };
    if directories
        .all()
        .iter()
        .any(|directory| fs::symlink_metadata(directory.join(&filename)).is_ok())
    {
        return Err(AgentError::delivery(format!(
            "message id already exists: {identifier}"
        )));
    }
    // A chat request prompt names its message's create time, so the drain can open its first
    // line with the delay as of typing; the stored text stays printable. See `prompt_time`.
    let document = match crate::prompt_time::queue_deferred(text, crate::prompt_time::now_millis())
    {
        Some(queued) => json!({
            "id": identifier,
            "text": queued.text,
            "sent_at": queued.sent_at,
            "opening": queued.opening,
            "queued_at": unix_seconds(),
            "delivery_attempts": 0,
        }),
        None => json!({
            "id": identifier,
            "text": text,
            "queued_at": unix_seconds(),
            "delivery_attempts": 0,
        }),
    };
    atomic_json_create(&path, &document).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            AgentError::delivery(format!("message id already exists: {identifier}"))
        } else {
            io_error("persist queued message", &path, error)
        }
    })?;
    Ok(identifier)
}

/// Serialize and drain a FIFO with production timing.
pub fn drain<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    options: DrainOptions,
) -> AgentResult<QueueResult> {
    drain_with_runtime(client, target, root, options, &SystemRuntime::default())
}

/// Serialize and drain a FIFO using an injected runtime.
pub fn drain_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    options: DrainOptions,
    runtime: &dyn AgentRuntime,
) -> AgentResult<QueueResult> {
    drain_with_runtime_binding(client, target, root, options, runtime, false)
}

pub(crate) fn drain_managed<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    options: DrainOptions,
) -> AgentResult<QueueResult> {
    drain_with_runtime_binding(
        client,
        target,
        root,
        options,
        &SystemRuntime::default(),
        true,
    )
}

pub(crate) fn drain_managed_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    options: DrainOptions,
    runtime: &dyn AgentRuntime,
) -> AgentResult<QueueResult> {
    drain_with_runtime_binding(client, target, root, options, runtime, true)
}

fn drain_with_runtime_binding<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    options: DrainOptions,
    runtime: &dyn AgentRuntime,
    allow_legacy_workspace_binding: bool,
) -> AgentResult<QueueResult> {
    let directory_identity =
        bind_queue_with_runtime(root, target, runtime, allow_legacy_workspace_binding)?;
    let directories = QueueDirectories::new(root);
    let queue_lock_path = root.join(".delivery.lock");
    let queue_lock = open_private_lock(&queue_lock_path, "queue delivery lock")?;
    lock_exclusive_with_runtime(&queue_lock, &queue_lock_path, "queue delivery", runtime)?;
    verify_queue_lock(
        root,
        directory_identity,
        &queue_lock_path,
        &queue_lock,
        "queue delivery lock",
    )?;
    let mut delivered = Vec::new();
    let mut quarantined = recover_inflight(&directories)?;
    let mut blocked = None;
    let mut target_lock: Option<TargetLock> = None;
    let mut locked_pane_id: Option<String> = None;
    let mut initial_info: Option<AgentPaneInfo> = None;
    'message: for path in inbox_in_queue_order(&directories.inbox)? {
        'retry: loop {
            let (mut document, attempts) = match load_message(&path).and_then(|document| {
                let attempts = delivery_attempts(&document, &path)?;
                Ok((document, attempts))
            }) {
                Ok(loaded) => loaded,
                Err(error) => {
                    let identifier = quarantine_raw(
                        &path,
                        &directories.failed,
                        "invalid_message",
                        &error.to_string(),
                    )?;
                    quarantined.push(identifier);
                    continue 'message;
                }
            };
            let filename_id = message_id_from_path(&path)?;
            let identifier = document
                .get("id")
                .and_then(Value::as_str)
                .map_or(filename_id.clone(), str::to_owned);
            if attempts >= options.max_attempts {
                let detail = format!(
                "message {identifier} reached the maximum delivery-attempt count ({attempts} >= {}); retained pending",
                options.max_attempts
            );
                document.insert("delivery_state".to_owned(), json!("pending"));
                document.insert("delivery_error".to_owned(), json!(detail));
                document.insert("delivery_blocked_at".to_owned(), json!(unix_seconds()));
                atomic_json(&path, &Value::Object(document))?;
                blocked = Some(detail);
                break 'message;
            }

            // A slash command opens a harness menu rather than reaching the composer, so it is
            // never typed into a working pane.
            let accept_working = options.deliver_while_working
                && message_text(&document)
                    .is_ok_and(|text| crate::submission::verifies("claude", &text));
            let readiness = (|| -> AgentResult<AgentPaneInfo> {
                if target_lock.is_none() {
                    let (lock, info) = lock_resolved_target_with_runtime(client, target, runtime)?;
                    target_lock = Some(lock);
                    locked_pane_id = Some(info.pane_id.clone());
                    initial_info = Some(info);
                }
                wait_ready(
                    client,
                    target,
                    options.ready_timeout,
                    runtime,
                    locked_pane_id
                        .as_deref()
                        .expect("target lock always records its pane"),
                    initial_info.take(),
                    accept_working,
                )
            })();
            let info = match readiness {
                Ok(info) => info,
                Err(error) => {
                    let detail = error.to_string();
                    document.insert("delivery_state".to_owned(), json!("pending"));
                    document.insert("delivery_error".to_owned(), json!(detail));
                    document.insert("delivery_blocked_at".to_owned(), json!(unix_seconds()));
                    atomic_json(&path, &Value::Object(document))?;
                    blocked = Some(detail);
                    break 'message;
                }
            };

            let inflight_path = directories.inflight.join(path.file_name().ok_or_else(|| {
                AgentError::delivery(format!(
                    "queued message has no filename: {}",
                    path.display()
                ))
            })?);
            transition(&path, &inflight_path)?;
            document.insert("possibly_submitted".to_owned(), Value::Bool(true));
            document.insert("delivery_state".to_owned(), json!("inflight"));
            document.insert("inflight_at".to_owned(), json!(unix_seconds()));
            atomic_json(&inflight_path, &Value::Object(document.clone()))?;

            // Typing into a working pane relies on the agent to queue the prompt; one it never
            // accepts is taken back out of the composer so that it stays safely pending.
            let withdrawing = WithdrawingRuntime { inner: runtime };
            let delivery_runtime: &dyn AgentRuntime = if accept_working && info.status == "working"
            {
                &withdrawing
            } else {
                runtime
            };
            match deliver_one(
                client,
                &info,
                &typed_text(&document)?,
                options.working_timeout,
                delivery_runtime,
            ) {
                Ok(Delivered::NotStaged(detail)) => {
                    // Nothing reached the composer: undo the at-most-once barrier so the
                    // prompt stays pending. A crash before the rename still quarantines it.
                    document.remove("possibly_submitted");
                    document.remove("inflight_at");
                    document.insert("delivery_state".to_owned(), json!("pending"));
                    document.insert("delivery_error".to_owned(), json!(detail));
                    document.insert("delivery_blocked_at".to_owned(), json!(unix_seconds()));
                    atomic_json(&inflight_path, &Value::Object(document))?;
                    transition(&inflight_path, &path)?;
                    blocked = Some(detail);
                    break 'message;
                }
                Ok(Delivered::Misrouted(detail)) => {
                    // The wrong program was interrupted and told to ignore the prompt. Retry
                    // from readiness, which re-resolves and re-verifies the recipient;
                    // quarantine once the same message misroutes again.
                    let mut misroutes = document
                        .get("misroutes")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    misroutes.push(json!({"at": unix_seconds(), "error": detail}));
                    let exhausted = misroutes.len() >= MAX_MISROUTES;
                    document.insert("misroutes".to_owned(), Value::Array(misroutes));
                    if exhausted {
                        let attempts = attempts.saturating_add(1);
                        document.insert("delivery_attempts".to_owned(), json!(attempts));
                        document.insert("tui_delivery_attempts".to_owned(), json!(attempts));
                        document.insert("delivery_error".to_owned(), json!(detail));
                        document.insert("probable_misroute".to_owned(), Value::Bool(true));
                        document.insert("delivery_failed_at".to_owned(), json!(unix_seconds()));
                        atomic_json(&inflight_path, &Value::Object(document))?;
                        let failed_path = directories
                            .failed
                            .join(path.file_name().expect("validated queued filename"));
                        transition(&inflight_path, &failed_path)?;
                        failed_metadata(&failed_path, "possibly_submitted", &detail)?;
                        quarantined.push(identifier);
                        break 'retry;
                    }
                    document.remove("possibly_submitted");
                    document.remove("inflight_at");
                    document.insert("delivery_state".to_owned(), json!("pending"));
                    document.insert("delivery_error".to_owned(), json!(detail));
                    atomic_json(&inflight_path, &Value::Object(document))?;
                    transition(&inflight_path, &path)?;
                    continue 'retry;
                }
                Ok(Delivered::Confirmed { verified }) => {
                    if verified {
                        runtime.prompt_printed(&identifier);
                    }
                    document.insert("delivery_state".to_owned(), json!("processed"));
                    document.insert("confirmed_at".to_owned(), json!(unix_seconds()));
                    atomic_json(&inflight_path, &Value::Object(document))?;
                    transition(
                        &inflight_path,
                        &directories
                            .processed
                            .join(path.file_name().expect("validated queued filename")),
                    )?;
                    delivered.push(identifier);
                }
                Err(Undelivered {
                    error,
                    probable_misroute,
                }) => {
                    let attempts = attempts.saturating_add(1);
                    let detail = error.to_string();
                    document.insert("delivery_attempts".to_owned(), json!(attempts));
                    document.insert("tui_delivery_attempts".to_owned(), json!(attempts));
                    document.insert("delivery_error".to_owned(), json!(detail));
                    document.insert("possibly_submitted".to_owned(), Value::Bool(true));
                    if probable_misroute {
                        document.insert("probable_misroute".to_owned(), Value::Bool(true));
                    }
                    document.insert("delivery_failed_at".to_owned(), json!(unix_seconds()));
                    atomic_json(&inflight_path, &Value::Object(document))?;
                    let failed_path = directories
                        .failed
                        .join(path.file_name().expect("validated queued filename"));
                    transition(&inflight_path, &failed_path)?;
                    failed_metadata(&failed_path, "possibly_submitted", &detail)?;
                    quarantined.push(identifier);
                }
            }
            break 'retry;
        }
    }

    let pending = identifiers(&directories.inbox)?;
    let outcome = if blocked.is_some() {
        QueueOutcome::Pending
    } else if quarantined.is_empty() {
        QueueOutcome::Delivered
    } else {
        QueueOutcome::PossiblySubmitted
    };
    Ok(QueueResult {
        message_id: String::new(),
        delivered,
        quarantined,
        pending,
        blocked,
        outcome,
    })
}

/// Durably enqueue one prompt, drain its bound FIFO, and require confirmed delivery.
pub fn send<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
) -> AgentResult<QueueResult> {
    send_with_runtime(
        client,
        target,
        root,
        text,
        options,
        &SystemRuntime::default(),
    )
}

/// Send one prompt using an injected readiness runtime.
pub fn send_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
    runtime: &dyn AgentRuntime,
) -> AgentResult<QueueResult> {
    send_identified_with_runtime(client, target, root, text, options, runtime, None)
}

/// Persist a caller-identified prompt without replacing an existing queue artifact.
pub fn send_identified<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    message_id: &str,
    options: DrainOptions,
) -> AgentResult<QueueResult> {
    send_identified_with_runtime(
        client,
        target,
        root,
        text,
        options,
        &SystemRuntime::default(),
        Some(message_id),
    )
}

pub(crate) fn send_identified_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
    runtime: &dyn AgentRuntime,
    message_id: Option<&str>,
) -> AgentResult<QueueResult> {
    send_identified_with_runtime_binding(
        client,
        target,
        root,
        text,
        options,
        message_id,
        (runtime, false),
    )
}

pub(crate) fn send_managed<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
) -> AgentResult<QueueResult> {
    send_identified_with_runtime_binding(
        client,
        target,
        root,
        text,
        options,
        None,
        (&SystemRuntime::default(), true),
    )
}

pub(crate) fn send_identified_managed<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
    message_id: &str,
) -> AgentResult<QueueResult> {
    send_identified_with_runtime_binding(
        client,
        target,
        root,
        text,
        options,
        Some(message_id),
        (&SystemRuntime::default(), true),
    )
}

pub(crate) fn send_identified_managed_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
    runtime: &dyn AgentRuntime,
    message_id: Option<&str>,
) -> AgentResult<QueueResult> {
    send_identified_with_runtime_binding(
        client,
        target,
        root,
        text,
        options,
        message_id,
        (runtime, true),
    )
}

fn send_identified_with_runtime_binding<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    text: &str,
    options: DrainOptions,
    message_id: Option<&str>,
    runtime_and_binding_mode: (&dyn AgentRuntime, bool),
) -> AgentResult<QueueResult> {
    let (runtime, allow_legacy_workspace_binding) = runtime_and_binding_mode;
    bind_queue_with_runtime(root, target, runtime, allow_legacy_workspace_binding)?;
    // Explicit IDs serialize their cross-directory check against delivery transitions. Generated
    // identifiers plus no-replace creation need not wait behind a long-running drain. The drain and terminal-artifact inspection below resolve any message
    // consumed by another sender between these phases.
    let identifier = enqueue_internal(root, text, message_id, message_id.is_some(), runtime)?;
    let result = drain_with_runtime_binding(
        client,
        target,
        root,
        options,
        runtime,
        allow_legacy_workspace_binding,
    )?;
    let filename = format!("{identifier}.json");
    let failed_path = root.join("failed").join(&filename);
    if result.quarantined.contains(&identifier) || fs::symlink_metadata(&failed_path).is_ok() {
        let detail = load_message(&failed_path)
            .ok()
            .and_then(|document| {
                document
                    .get("delivery_error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown delivery failure".to_owned());
        return Err(AgentError::PossiblySubmitted(UndeliveredMessage {
            message: format!(
                "message {identifier} has an ambiguous outcome after one injection: {detail}; it is retained under {}/failed",
                root.display()
            ),
            message_id: identifier,
            artifact: failed_path,
        }));
    }
    let inflight_path = root.join("inflight").join(&filename);
    if fs::symlink_metadata(&inflight_path).is_ok() {
        return Err(AgentError::PossiblySubmitted(UndeliveredMessage {
            message: format!(
                "message {identifier} remains behind the durable inflight barrier; automatic resubmission is unsafe"
            ),
            message_id: identifier,
            artifact: inflight_path,
        }));
    }
    let inbox_path = root.join("inbox").join(&filename);
    if result.pending.contains(&identifier) || fs::symlink_metadata(&inbox_path).is_ok() {
        return Err(AgentError::Pending(UndeliveredMessage {
            message: format!(
                "message {identifier} remains pending without consuming a retry attempt: {}",
                result
                    .blocked
                    .as_deref()
                    .unwrap_or("unknown readiness failure")
            ),
            message_id: identifier.clone(),
            artifact: inbox_path,
        }));
    }
    let processed_path = root.join("processed").join(&filename);
    if !result.delivered.contains(&identifier) && fs::symlink_metadata(&processed_path).is_err() {
        return Err(AgentError::delivery(format!(
            "message {identifier} disappeared without a durable terminal artifact"
        )));
    }
    let mut delivered = result.delivered;
    if !delivered.contains(&identifier) {
        delivered.push(identifier.clone());
    }
    Ok(QueueResult {
        message_id: identifier,
        delivered,
        quarantined: result.quarantined,
        pending: result.pending,
        blocked: result.blocked,
        outcome: QueueOutcome::Delivered,
    })
}

/// Read validated live-agent and queue state without creating or changing queue files.
pub fn status<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
) -> AgentResult<QueueStatus> {
    status_with_binding_mode(client, target, root, false)
}

pub(crate) fn status_managed<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
) -> AgentResult<QueueStatus> {
    status_with_binding_mode(client, target, root, true)
}

fn status_with_binding_mode<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    root: &Path,
    allow_legacy_workspace_binding: bool,
) -> AgentResult<QueueStatus> {
    validate_existing_queue(root)?;
    validate_existing_binding(root, target, allow_legacy_workspace_binding)?;
    let info = resolve_target(client, target)?;
    let directories = QueueDirectories::new(root);
    Ok(QueueStatus {
        pane_id: info.pane_id,
        agent: info.agent,
        agent_status: info.status,
        session_agent: info.session_agent,
        session_value: info.session_value,
        workspace_id: info.workspace_id,
        cwd: info.cwd,
        pending: identifiers_if_directory(&directories.inbox)?,
        inflight: identifiers_if_directory(&directories.inflight)?,
        failed: identifiers_if_directory(&directories.failed)?,
    })
}

/// Locate one message identifier without scanning a queue directory.
///
/// This is a recovery primitive for durable callers that recorded intent before invoking
/// [`send_identified`]. `None` proves that no artifact with this identifier exists in any queue
/// phase while the caller holds the surrounding agent lifecycle lock.
pub fn message_state(root: &Path, message_id: &str) -> AgentResult<Option<QueueMessageState>> {
    validate_message_id(message_id)?;
    if fs::symlink_metadata(root).is_err() {
        return Ok(None);
    }
    validate_existing_queue(root)?;
    let filename = format!("{message_id}.json");
    let directories = QueueDirectories::new(root);
    let candidates = [
        (&directories.inbox, QueueMessageState::Pending),
        (&directories.inflight, QueueMessageState::Inflight),
        (&directories.processed, QueueMessageState::Processed),
        (&directories.failed, QueueMessageState::Failed),
    ];
    let mut observed = None;
    for (directory, state) in candidates {
        let path = directory.join(&filename);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let document = load_message(&path)?;
                if document.get("id").and_then(Value::as_str) != Some(message_id) {
                    return Err(AgentError::delivery(format!(
                        "queue artifact {} contains a different message id",
                        path.display()
                    )));
                }
                if observed.replace(state).is_some() {
                    return Err(AgentError::delivery(format!(
                        "message id {message_id:?} exists in multiple queue phases"
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect queue message", &path, error)),
        }
    }
    Ok(observed)
}

/// Read recent terminal output from a validated interactive-agent target.
pub fn read<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    lines: usize,
) -> AgentResult<String> {
    let info = resolve_target(client, target)?;
    read_recent(client, &info.pane_id, lines)
}

/// Read what a chat bridge scans for replies from a validated interactive-agent target: the
/// screen of a pane that herdr reports keeps no scrollback, otherwise the same recent rows as
/// [`read`]. A recent read of such a pane can return more than one screen, joined from what the
/// program drew at different times.
pub fn read_capture<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    lines: usize,
) -> AgentResult<String> {
    let info = resolve_target(client, target)?;
    if info.keeps_no_scrollback() {
        client
            .read(&info.pane_id, "visible", Some(lines))
            .map_err(client_error)
    } else {
        read_recent(client, &info.pane_id, lines)
    }
}

fn read_recent<A: AgentApi + ?Sized>(
    client: &A,
    pane_id: &str,
    lines: usize,
) -> AgentResult<String> {
    let text = client
        .read(pane_id, "recent-unwrapped", Some(lines))
        .map_err(client_error)?;
    if text.is_empty() {
        client
            .read(pane_id, "recent", Some(lines))
            .map_err(client_error)
    } else {
        Ok(text)
    }
}

/// Return the stable SHA-256 lock identity for one resolved live pane.
#[must_use]
pub fn pane_lock_digest(pane_id: &str) -> String {
    let identity = json!({"kind": "pane", "pane_id": pane_id});
    let encoded = serde_json::to_vec(&identity).expect("JSON value serialization cannot fail");
    format!("{:x}", Sha256::digest(encoded))
}

fn wait_ready<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    timeout: Duration,
    runtime: &dyn AgentRuntime,
    locked_pane_id: &str,
    mut initial_info: Option<AgentPaneInfo>,
    accept_working: bool,
) -> AgentResult<AgentPaneInfo> {
    let deadline = runtime.monotonic().saturating_add(timeout);
    loop {
        if runtime.cancelled() {
            return Err(AgentError::delivery(
                "delivery was cancelled before terminal injection",
            ));
        }
        let info = match initial_info.take() {
            Some(info) => info,
            None => resolve_target_with_runtime(client, target, runtime)?,
        };
        if info.pane_id != locked_pane_id {
            return Err(AgentError::delivery(format!(
                "target moved from locked pane {locked_pane_id:?} to {:?}",
                info.pane_id
            )));
        }
        if matches!(info.status.as_str(), "idle" | "done") {
            return Ok(info);
        }
        // Claude Code queues a prompt submitted while it works, and shows that it did; the
        // verified submission checks the composer is empty before typing and proves the prompt
        // left it. Other harnesses are not typed into while they work.
        if accept_working && info.status == "working" && info.agent.as_deref() == Some("claude") {
            return Ok(info);
        }
        if info.status == "unknown"
            && info.agent.as_deref() == Some("codex")
            && client
                .codex_idle_ready_with_runtime(&info, runtime)
                .map_err(client_error)?
        {
            return Ok(info);
        }
        if info.status == "blocked" {
            return Err(AgentError::delivery(format!(
                "pane {} is blocked; resolve its visible prompt",
                info.pane_id
            )));
        }
        let now = runtime.monotonic();
        if now >= deadline {
            return Err(AgentError::delivery(format!(
                "pane {} did not become idle/done within {}s; last status={}",
                info.pane_id,
                timeout.as_secs_f64(),
                info.status
            )));
        }
        runtime.sleep(POLL_INTERVAL.min(deadline.saturating_sub(runtime.monotonic())));
    }
}

/// Result of one delivery that did not fail after typing began.
enum Delivered {
    /// The prompt was submitted and confirmed.
    Confirmed {
        /// Whether the submission was verified against the agent's composer, which the queue
        /// checked before typing: with printed evidence or with the prompt leaving the composer.
        verified: bool,
    },
    /// Nothing was typed, so the prompt remains safe to retry.
    NotStaged(String),
    /// The prompt reached the wrong program, which was told to ignore it; retry is safe.
    Misrouted(String),
}

/// Misroutes tolerated for one message before it is quarantined instead of retried.
pub const MAX_MISROUTES: usize = 2;

/// A delivery that failed after typing may have begun; the prompt is quarantined.
struct Undelivered {
    error: AgentError,
    /// The pane failed its recipient check immediately after input was written, so the
    /// input may have reached another program.
    probable_misroute: bool,
}

impl From<AgentError> for Undelivered {
    fn from(error: AgentError) -> Self {
        Self {
            error,
            probable_misroute: false,
        }
    }
}

fn deliver_one<A: AgentApi + ?Sized>(
    client: &A,
    info: &AgentPaneInfo,
    text: &str,
    working_timeout: Duration,
    runtime: &dyn AgentRuntime,
) -> std::result::Result<Delivered, Undelivered> {
    let submission = match client.submit_with_runtime(&info.pane_id, text, runtime) {
        Ok(submission) => submission,
        // A recipient check refused the first input effect: nothing reached the pane.
        Err(error) if error.kind() == AdapterErrorKind::NotStaged => {
            return Ok(Delivered::NotStaged(error.to_string()))
        }
        Err(error) if error.kind() == AdapterErrorKind::MisrouteRecovered => {
            return Ok(Delivered::Misrouted(error.to_string()))
        }
        Err(error) if error.kind() == AdapterErrorKind::ProbableMisroute => {
            return Err(Undelivered {
                error: AgentError::delivery(format!(
                    "pane {}: PROBABLE MISROUTE, quarantined: {error}",
                    info.pane_id
                )),
                probable_misroute: true,
            })
        }
        Err(error) => {
            return Err(AgentError::delivery(format!(
                "pane {} agent-prompt outcome is unknown; prompt may have been submitted: {error}",
                info.pane_id
            ))
            .into())
        }
    };
    match submission {
        // The screen already proved the prompt left the composer. A lifecycle
        // transition adds nothing and is absent when the agent queues the prompt.
        // A weak verification counts only once its wait settled: see `SubmissionReceipt::settled`.
        // A receipt that neither printed nor settled proves nothing about the recipient.
        Submission::Verified(receipt) if !receipt.printed && !receipt.settled => {
            return Err(AgentError::delivery(format!(
                "pane {} submission receipt neither printed nor settled; prompt may have been submitted",
                info.pane_id
            ))
            .into())
        }
        Submission::Verified(_) => return Ok(Delivered::Confirmed { verified: true }),
        Submission::NotStaged(reason) => return Ok(Delivered::NotStaged(reason)),
        Submission::Unconfirmed => {}
    }
    // A confirmation key (such as a goal-replacement Enter) can itself misroute.
    let misrouted = |error: &AdapterError| -> Option<std::result::Result<Delivered, Undelivered>> {
        match error.kind() {
            AdapterErrorKind::MisrouteRecovered => {
                Some(Ok(Delivered::Misrouted(error.to_string())))
            }
            AdapterErrorKind::ProbableMisroute => Some(Err(Undelivered {
                error: AgentError::delivery(format!(
                    "pane {}: PROBABLE MISROUTE, quarantined: {error}",
                    info.pane_id
                )),
                probable_misroute: true,
            })),
            // The confirmation came from a pane that no longer holds the recipient.
            AdapterErrorKind::RecipientChanged => Some(Err(AgentError::delivery(format!(
                "pane {} did not confirm idle/done -> working submission: {error}",
                info.pane_id
            ))
            .into())),
            _ => None,
        }
    };
    let Some(chunk) = runtime.delivery_wait_chunk() else {
        let millis = working_timeout.as_millis().clamp(1, u64::MAX.into()) as u64;
        return match client.wait_agent_status_with_runtime(
            &info.pane_id,
            "working",
            millis,
            runtime,
        ) {
            Ok(()) => Ok(Delivered::Confirmed { verified: false }),
            Err(error) => misrouted(&error).unwrap_or_else(|| {
                Err(AgentError::delivery(format!(
                    "pane {} did not confirm idle/done -> working submission: {error}",
                    info.pane_id
                ))
                .into())
            }),
        };
    };
    let deadline = runtime.monotonic().saturating_add(working_timeout);
    let mut last_error = None;
    loop {
        if runtime.cancelled() {
            return Err(AgentError::delivery(format!(
                "pane {} delivery was cancelled after terminal injection; outcome is unknown",
                info.pane_id
            ))
            .into());
        }
        let remaining = deadline.saturating_sub(runtime.monotonic());
        if remaining.is_zero() {
            let detail = last_error.unwrap_or_else(|| "working-state deadline elapsed".to_owned());
            return Err(AgentError::delivery(format!(
                "pane {} did not confirm idle/done -> working submission: {detail}",
                info.pane_id
            ))
            .into());
        }
        let wait = chunk.min(remaining);
        let millis = wait.as_millis().clamp(1, u64::MAX.into()) as u64;
        match client.wait_agent_status_with_runtime(&info.pane_id, "working", millis, runtime) {
            Ok(()) => return Ok(Delivered::Confirmed { verified: false }),
            Err(error) => {
                if let Some(outcome) = misrouted(&error) {
                    return outcome;
                }
                last_error = Some(error.to_string());
            }
        }
    }
}

#[derive(Clone, Debug)]
struct QueueDirectories {
    inbox: PathBuf,
    inflight: PathBuf,
    processed: PathBuf,
    failed: PathBuf,
}

impl QueueDirectories {
    fn new(root: &Path) -> Self {
        Self {
            inbox: root.join("inbox"),
            inflight: root.join("inflight"),
            processed: root.join("processed"),
            failed: root.join("failed"),
        }
    }

    fn all(&self) -> [&Path; 4] {
        [&self.inbox, &self.inflight, &self.processed, &self.failed]
    }
}

fn prepare(root: &Path) -> AgentResult<QueueDirectories> {
    let root_existed = fs::symlink_metadata(root).is_ok();
    create_private_directory(root, "queue directory", true, true)?;
    if !root_existed {
        if let Some(parent) = root.parent().filter(|path| !path.as_os_str().is_empty()) {
            sync_directory(parent)?;
        }
    }
    let directories = QueueDirectories::new(root);
    for path in directories.all() {
        create_private_directory(path, "queue state directory", true, true)?;
    }
    sync_directory(root)?;
    Ok(directories)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QueueDirectoryIdentity {
    device: u64,
    inode: u64,
}

fn queue_directory_identity(root: &Path) -> AgentResult<QueueDirectoryIdentity> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| io_error("inspect queue directory", root, error))?;
    let uid = unsafe { libc::getuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err(AgentError::delivery(format!(
            "unsafe queue directory: {}",
            root.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(AgentError::delivery(format!(
            "queue directory is not private: {}",
            root.display()
        )));
    }
    Ok(QueueDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn verify_queue_lock(
    root: &Path,
    directory_identity: QueueDirectoryIdentity,
    path: &Path,
    lock: &File,
    purpose: &str,
) -> AgentResult<()> {
    if queue_directory_identity(root)? != directory_identity {
        return Err(AgentError::delivery(format!(
            "queue directory changed while waiting for {purpose}: {}",
            root.display()
        )));
    }
    let current = fs::symlink_metadata(path)
        .map_err(|error| io_error(&format!("inspect {purpose} after locking"), path, error))?;
    let opened = lock
        .metadata()
        .map_err(|error| io_error(&format!("inspect {purpose} after locking"), path, error))?;
    let uid = unsafe { libc::getuid() };
    for metadata in [&current, &opened] {
        if !metadata.file_type().is_file()
            || metadata.uid() != uid
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(AgentError::delivery(format!(
                "unsafe {purpose}: {}",
                path.display()
            )));
        }
    }
    if (current.dev(), current.ino()) != (opened.dev(), opened.ino()) {
        return Err(AgentError::delivery(format!(
            "{purpose} changed while waiting: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn create_private_directory(
    path: &Path,
    purpose: &str,
    recursive: bool,
    tighten: bool,
) -> AgentResult<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(recursive).mode(0o700);
    if let Err(error) = builder.create(path) {
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(io_error(&format!("prepare {purpose}"), path, error));
        }
    }
    validate_private_directory(path, purpose, tighten)
}

pub(crate) fn validate_private_directory(
    path: &Path,
    purpose: &str,
    tighten: bool,
) -> AgentResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error(&format!("inspect {purpose}"), path, error))?;
    let uid = unsafe { libc::getuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err(AgentError::delivery(format!(
            "unsafe {purpose}: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        if !tighten {
            return Err(AgentError::delivery(format!(
                "{purpose} is not private: {}",
                path.display()
            )));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_error(&format!("make {purpose} private"), path, error))?;
    }
    Ok(())
}

pub(crate) fn open_private_lock(path: &Path, purpose: &str) -> AgentResult<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error(&format!("open {purpose}"), path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error(&format!("inspect {purpose}"), path, error))?;
    let uid = unsafe { libc::getuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(AgentError::delivery(format!(
            "unsafe {purpose}: {}",
            path.display()
        )));
    }
    Ok(file)
}

fn binding(target: &Target) -> AgentResult<Value> {
    let expected_cwd = target
        .expected_cwd
        .as_deref()
        .map(real_path)
        .transpose()?
        .map(|path| path.display().to_string());
    if let Some(value) = target.session_value.as_deref() {
        Ok(json!({
            "kind": "session",
            "agent": target.session_agent,
            "value": value,
            "expected_agent": target.expected_agent,
            "expected_workspace": target.expected_workspace,
            "expected_cwd": expected_cwd,
        }))
    } else {
        Ok(json!({
            "kind": "pane",
            "pane_id": target.pane_id,
            "expected_agent": target.expected_agent,
            "expected_workspace": target.expected_workspace,
            "expected_cwd": expected_cwd,
        }))
    }
}

fn same_binding_identity(left: &Value, right: &Value) -> bool {
    left == right
}

fn binding_identity_matches(
    actual: &Value,
    expected: &Value,
    allow_legacy_workspace_binding: bool,
) -> bool {
    if same_binding_identity(actual, expected) {
        return true;
    }
    let (Some(mut compatible), Some(expected)) =
        (actual.as_object().cloned(), expected.as_object())
    else {
        return false;
    };
    if !allow_legacy_workspace_binding
        || !expected
            .get("expected_workspace")
            .is_some_and(Value::is_null)
        || !compatible
            .get("expected_workspace")
            .is_some_and(Value::is_string)
    {
        return false;
    }
    compatible.insert("expected_workspace".to_owned(), Value::Null);
    &compatible == expected
}

#[cfg(test)]
fn bind_queue(root: &Path, target: &Target) -> AgentResult<()> {
    bind_queue_with_runtime(root, target, &SystemRuntime::default(), false).map(|_| ())
}

fn bind_queue_with_runtime(
    root: &Path,
    target: &Target,
    runtime: &dyn AgentRuntime,
    allow_legacy_workspace_binding: bool,
) -> AgentResult<QueueDirectoryIdentity> {
    validate_target_authority(target)?;
    prepare(root)?;
    let directory_identity = queue_directory_identity(root)?;
    let lock_path = root.join(".binding.lock");
    let binding_path = root.join("target.json");
    let lock = open_private_lock(&lock_path, "queue binding lock")?;
    lock_exclusive_with_runtime(&lock, &lock_path, "queue binding", runtime)?;
    verify_queue_lock(
        root,
        directory_identity,
        &lock_path,
        &lock,
        "queue binding lock",
    )?;
    let expected = binding(target)?;
    match fs::symlink_metadata(&binding_path) {
        Ok(_) => {
            let actual = read_private_json(&binding_path)?;
            if !binding_identity_matches(&actual, &expected, allow_legacy_workspace_binding) {
                return Err(AgentError::delivery(format!(
                    "queue {} is bound to {actual}, refusing different target {expected}",
                    root.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_json(&binding_path, &expected)?;
        }
        Err(error) => {
            return Err(io_error(
                "inspect queue target binding",
                &binding_path,
                error,
            ))
        }
    }
    Ok(directory_identity)
}

/// Replace one queue's exact target binding after a verified pane move.
///
/// Delivery is excluded while the binding changes. The old binding must be
/// byte-semantically identical to `previous`; accepting `replacement` makes
/// recovery idempotent when the binding commit succeeded but the surrounding
/// agent-record commit did not.
#[cfg(test)]
pub(crate) fn rebind_queue(
    root: &Path,
    previous: &Target,
    replacement: &Target,
) -> AgentResult<()> {
    validate_target_authority(previous)?;
    validate_target_authority(replacement)?;
    prepare(root)?;
    let directory_identity = queue_directory_identity(root)?;

    let delivery_path = root.join(".delivery.lock");
    let delivery = open_private_lock(&delivery_path, "queue delivery lock")?;
    lock_exclusive_with_runtime(
        &delivery,
        &delivery_path,
        "queue delivery",
        &SystemRuntime::default(),
    )?;
    verify_queue_lock(
        root,
        directory_identity,
        &delivery_path,
        &delivery,
        "queue delivery lock",
    )?;

    let binding_lock_path = root.join(".binding.lock");
    let binding_lock = open_private_lock(&binding_lock_path, "queue binding lock")?;
    lock_exclusive_with_runtime(
        &binding_lock,
        &binding_lock_path,
        "queue binding",
        &SystemRuntime::default(),
    )?;
    verify_queue_lock(
        root,
        directory_identity,
        &binding_lock_path,
        &binding_lock,
        "queue binding lock",
    )?;

    let path = root.join("target.json");
    let old = binding(previous)?;
    let new = binding(replacement)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let actual = read_private_json(&path)?;
            if !same_binding_identity(&actual, &old) && !same_binding_identity(&actual, &new) {
                return Err(AgentError::delivery(format!(
                    "queue {} is bound to {actual}, refusing move from {old} to {new}",
                    root.display()
                )));
            }
            if actual != new {
                atomic_json(&path, &new)?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => atomic_json(&path, &new)?,
        Err(error) => return Err(io_error("inspect queue target binding", &path, error)),
    }
    Ok(())
}

pub(crate) fn rebind_queue_after<T, F>(
    root: &Path,
    previous: &Target,
    already_replacement: Option<&Target>,
    allow_legacy_workspace_binding: bool,
    operation: F,
) -> AgentResult<T>
where
    F: FnOnce() -> AgentResult<(Target, T)>,
{
    validate_target_authority(previous)?;
    prepare(root)?;
    let directory_identity = queue_directory_identity(root)?;
    let delivery_path = root.join(".delivery.lock");
    let delivery = open_private_lock(&delivery_path, "queue delivery lock")?;
    lock_exclusive_with_runtime(
        &delivery,
        &delivery_path,
        "queue delivery",
        &SystemRuntime::default(),
    )?;
    verify_queue_lock(
        root,
        directory_identity,
        &delivery_path,
        &delivery,
        "queue delivery lock",
    )?;
    let binding_lock_path = root.join(".binding.lock");
    let binding_lock = open_private_lock(&binding_lock_path, "queue binding lock")?;
    lock_exclusive_with_runtime(
        &binding_lock,
        &binding_lock_path,
        "queue binding",
        &SystemRuntime::default(),
    )?;
    verify_queue_lock(
        root,
        directory_identity,
        &binding_lock_path,
        &binding_lock,
        "queue binding lock",
    )?;
    let path = root.join("target.json");
    let old = binding(previous)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let actual = read_private_json(&path)?;
            let replacement_allowed = already_replacement
                .map(binding)
                .transpose()?
                .as_ref()
                .is_some_and(|replacement| same_binding_identity(&actual, replacement));
            if !binding_identity_matches(&actual, &old, allow_legacy_workspace_binding)
                && !replacement_allowed
            {
                return Err(AgentError::delivery(format!(
                    "queue {} is bound to {actual}, refusing move from {old}",
                    root.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspect queue target binding", &path, error)),
    }
    let (replacement, result) = operation()?;
    validate_target_authority(&replacement)?;
    atomic_json(&path, &binding(&replacement)?)?;
    Ok(result)
}

fn validate_existing_binding(
    root: &Path,
    target: &Target,
    allow_legacy_workspace_binding: bool,
) -> AgentResult<()> {
    let binding_path = root.join("target.json");
    let actual = match fs::symlink_metadata(&binding_path) {
        Ok(_) => read_private_json(&binding_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(io_error(
                "inspect queue target binding",
                &binding_path,
                error,
            ))
        }
    };
    let expected = binding(target)?;
    if !binding_identity_matches(&actual, &expected, allow_legacy_workspace_binding) {
        return Err(AgentError::delivery(format!(
            "queue {} is bound to {actual}, refusing different target {expected}",
            root.display()
        )));
    }
    Ok(())
}

fn validate_existing_queue(root: &Path) -> AgentResult<()> {
    match fs::symlink_metadata(root) {
        Ok(_) => validate_private_directory(root, "queue directory", false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("inspect queue directory", root, error)),
    }
    for directory in QueueDirectories::new(root).all() {
        match fs::symlink_metadata(directory) {
            Ok(_) => validate_private_directory(directory, "queue state directory", false)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect queue state directory", directory, error)),
        }
    }
    Ok(())
}

pub(crate) fn validate_target_authority(target: &Target) -> AgentResult<()> {
    let pane = target
        .pane_id
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    let session = target
        .session_value
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    if pane || session {
        Ok(())
    } else {
        Err(AgentError::delivery(
            "target needs --pane or a stable session value",
        ))
    }
}

/// One held host-wide target lock. Dropping it releases every file it holds.
///
/// It holds the file under the account-state root and, while editions that know only the
/// shared `/tmp` root may still run, the compatibility file there as well, so a new and an
/// old process still exclude each other. Both are taken in that fixed order by every new
/// process, and an old process takes only the `/tmp` file, so the two orders cannot deadlock.
pub(crate) struct TargetLock {
    _account: File,
    _legacy: File,
}

/// The lock files for one target name, in acquisition order: account state, then `/tmp`.
pub(crate) fn target_lock_paths(name: &str) -> AgentResult<[PathBuf; 2]> {
    let file = format!("{}.lock", pane_lock_digest(name));
    let [account, legacy] = target_lock_roots()?;
    create_private_directory(&account, "host-wide target lock directory", true, false)?;
    create_private_directory(
        &legacy,
        "legacy host-wide target lock directory",
        true,
        false,
    )?;
    Ok([account.join(&file), legacy.join(file)])
}

/// Open and lock every file of one target lock in order, waiting on each with `wait`.
fn lock_target_with(
    name: &str,
    purpose: &str,
    mut wait: impl FnMut(&File, &Path) -> AgentResult<()>,
) -> AgentResult<TargetLock> {
    let [account_path, legacy_path] = target_lock_paths(name)?;
    let account = lock_current_file(&account_path, purpose, &mut wait)?;
    let legacy = lock_current_file(&legacy_path, purpose, &mut wait)?;
    if let (Some(account_directory), Some(legacy_directory)) =
        (account_path.parent(), legacy_path.parent())
    {
        refresh_lock_files_when_due(
            &account_directory.join(LEGACY_REFRESH_MARKER),
            legacy_directory,
        );
    }
    Ok(TargetLock {
        _account: account,
        _legacy: legacy,
    })
}

/// Open and lock the file at `path`, retrying until the locked file is still the one the path
/// names, then set its modification time to now.
///
/// The host's daily cleaner deletes `/tmp` files by modification time, so it can unlink a lock
/// file while a sender waits on it. A sender that then took the lock would hold a file no
/// other process can open, while the next sender locked a fresh file at the same path; the
/// check after locking sends it back to contend on the file that is there.
fn lock_current_file(
    path: &Path,
    purpose: &str,
    wait: &mut dyn FnMut(&File, &Path) -> AgentResult<()>,
) -> AgentResult<File> {
    loop {
        let file = open_private_lock(path, purpose)?;
        wait(&file, path)?;
        let held = file
            .metadata()
            .map_err(|error| io_error(&format!("inspect {purpose}"), path, error))?;
        match fs::symlink_metadata(path) {
            Ok(current) if current.dev() == held.dev() && current.ino() == held.ino() => {
                file.set_modified(SystemTime::now())
                    .map_err(|error| io_error(&format!("mark {purpose} used"), path, error))?;
                return Ok(file);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&format!("inspect {purpose}"), path, error)),
        }
    }
}

/// The account-state file whose modification time records the last refresh of the `/tmp` lock
/// files. It lives outside `/tmp`, so the cleaner never removes it.
const LEGACY_REFRESH_MARKER: &str = "legacy-locks-refreshed";

/// How often senders refresh the `/tmp` lock files: far inside the cleaner's four days, and
/// rare enough that a steady stream of deliveries does not scan a directory each time.
const LEGACY_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Refresh every lock file in `directory` unless `marker` shows a refresh within the last
/// [`LEGACY_REFRESH_INTERVAL`]. A missing marker, or one dated in the future, is due.
fn refresh_lock_files_when_due(marker: &Path, directory: &Path) {
    let now = SystemTime::now();
    let fresh = fs::metadata(marker)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < LEGACY_REFRESH_INTERVAL);
    if fresh {
        return;
    }
    // A marker that cannot be written only means the next sender refreshes again.
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(marker)
        .and_then(|file| file.set_modified(now));
    refresh_lock_files(directory);
}

/// Set the modification time of every lock file in `directory` to now.
///
/// Refreshing only the file a sender locks leaves two gaps. The cleaner examines a file and
/// deletes it a moment later, so a file that was four days old when examined can be deleted
/// just after a sender refreshed and locked it. And a target no one sends to for four days
/// ages out while a process that takes only the `/tmp` file may still open it. Refreshing the
/// whole directory at least hourly means no file there reaches four days while any process
/// that takes both files delivers on the host. Failures are ignored: a file that another
/// sender is creating or the cleaner is deleting needs nothing from this sender, whose own
/// lock is already valid.
fn refresh_lock_files(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().as_bytes().ends_with(b".lock") {
            continue;
        }
        let Ok(path) = CString::new(entry.path().as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: `path` is NUL-terminated and outlives the call; null times mean now.
        unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                path.as_ptr(),
                std::ptr::null(),
                libc::AT_SYMLINK_NOFOLLOW,
            );
        }
    }
}

/// Lock one target like [`lock_target`], giving up after `within` instead of waiting.
///
/// Used where the caller already holds another target lock, so it can never deadlock.
pub(crate) fn lock_target_within(
    name: &str,
    purpose: &str,
    within: Duration,
) -> AgentResult<TargetLock> {
    let deadline = Instant::now() + within;
    lock_target_with(name, purpose, |file, path| loop {
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(AgentError::delivery(format!(
                        "{purpose} for {name} is held by another sender"
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(io_error(&format!("lock {purpose}"), path, error)),
        }
    })
}

/// Lock one target name, blocking until every holder releases it.
pub(crate) fn lock_target(name: &str, purpose: &str) -> AgentResult<TargetLock> {
    lock_target_with(name, purpose, |file, path| {
        FileExt::lock_exclusive(file)
            .map_err(|error| io_error(&format!("lock {purpose}"), path, error))
    })
}

/// The directory every agentctl process for this user serialises pane delivery in.
///
/// It is account state, resolved from the account database rather than `$HOME` or
/// `$TMPDIR`, so callers with different environments still meet on one file. It must not be
/// under `/tmp` or `/var/tmp`: the host's daily cleaner deletes files there by modification
/// time, and a lock file is never written, so once the file was four days old the cleaner
/// could delete it while it was held and a second sender would lock a fresh file at the same
/// path.
fn host_target_lock_root() -> AgentResult<PathBuf> {
    target_lock_root_under(crate::client::account_home()?)
}

/// The account-state lock root under `home`, which must be absolute: a relative home would
/// resolve against each caller's working directory, so senders started in different
/// directories would lock different files.
fn target_lock_root_under(home: PathBuf) -> AgentResult<PathBuf> {
    if !home.is_absolute() {
        return Err(AgentError::delivery(format!(
            "account home directory is not an absolute path: {}",
            home.display()
        )));
    }
    Ok(home.join(".local/state/agentctl/target-locks"))
}

/// The root earlier editions lock in. New editions lock it second, after the account root.
fn legacy_host_target_lock_root() -> PathBuf {
    Path::new("/tmp").join(format!("herdr-agent-target-locks-{}", unsafe {
        libc::getuid()
    }))
}

/// The roots every agentctl process on the host locks in, in acquisition order.
fn host_target_lock_roots() -> AgentResult<[PathBuf; 2]> {
    Ok([host_target_lock_root()?, legacy_host_target_lock_root()])
}

#[cfg(not(test))]
fn target_lock_roots() -> AgentResult<[PathBuf; 2]> {
    host_target_lock_roots()
}

/// Unit tests lock roots private to this test process. The fixture panes (`w1:p1`) are
/// real pane identities, so on the host roots any concurrent holder (another checkout's
/// validation, or a live agent on that pane) stalls them. Within one process the tests still
/// share one root, so concurrent senders keep exercising the real cross-queue flock.
#[cfg(test)]
fn target_lock_roots() -> AgentResult<[PathBuf; 2]> {
    let root = test_target_lock_root();
    Ok([root.join("account"), root.join("legacy")])
}

#[cfg(test)]
fn test_target_lock_root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    extern "C" fn remove_root() {
        if let Some(root) = ROOT.get() {
            let _ = fs::remove_dir_all(root);
        }
    }
    ROOT.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "agentctl-test-target-locks-{}-{}-{nanos}",
            unsafe { libc::getuid() },
            std::process::id()
        ));
        unsafe { libc::atexit(remove_root) };
        root
    })
    .clone()
}

pub(crate) fn lock_resolved_target<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
) -> AgentResult<(TargetLock, AgentPaneInfo)> {
    lock_resolved_target_with_runtime(client, target, &SystemRuntime::default())
}

pub(crate) fn lock_resolved_target_with_runtime<A: AgentApi + ?Sized>(
    client: &A,
    target: &Target,
    runtime: &dyn AgentRuntime,
) -> AgentResult<(TargetLock, AgentPaneInfo)> {
    let initial = resolve_target_with_runtime(client, target, runtime)?;
    let lock = lock_target_with(&initial.pane_id, "host-wide target lock", |file, path| {
        lock_exclusive_with_runtime(file, path, "interactive-agent target", runtime)
    })?;
    let confirmed = resolve_target_with_runtime(client, target, runtime)?;
    if confirmed.pane_id != initial.pane_id {
        return Err(AgentError::delivery(format!(
            "target moved from pane {:?} to {:?} while waiting for its host-wide lock",
            initial.pane_id, confirmed.pane_id
        )));
    }
    Ok((lock, confirmed))
}

pub(crate) fn lock_exclusive_with_runtime(
    lock: &File,
    path: &Path,
    purpose: &str,
    runtime: &dyn AgentRuntime,
) -> AgentResult<()> {
    loop {
        match FileExt::try_lock_exclusive(lock) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if runtime.cancelled() {
                    return Err(AgentError::delivery(format!(
                        "cancelled while waiting to lock {purpose}: {}",
                        path.display()
                    )));
                }
                runtime.sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(io_error(&format!("lock {purpose}"), path, error)),
        }
    }
}

pub(crate) fn atomic_json(path: &Path, value: &Value) -> AgentResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AgentError::delivery(format!("JSON path has no parent: {}", path.display()))
    })?;
    let (temporary, mut file) = temporary_file(parent)?;
    let write_result = (|| -> io::Result<()> {
        {
            let mut writer = BufWriter::new(&mut file);
            serde_json::to_writer_pretty(&mut writer, value).map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory_io(parent)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map_err(|error| io_error("write durable JSON", path, error))
}

pub(crate) fn atomic_temporary_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name.strip_prefix(".message.") else {
        return false;
    };
    let Some((process, sequence)) = suffix.split_once('.') else {
        return false;
    };
    !process.is_empty()
        && !sequence.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

fn open_atomic_temporary_at(directory: &File, name: &OsStr) -> AgentResult<Option<File>> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| AgentError::delivery("durable JSON temporary name contains NUL".to_owned()))?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(io_error(
            "open durable JSON temporary without following links",
            Path::new(name.to_str().unwrap_or("<non-UTF8>")),
            error,
        ));
    }
    Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
}

fn validate_atomic_temporary(file: &File, path: &Path) -> AgentResult<()> {
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect durable JSON temporary", path, error))?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(AgentError::delivery(format!(
            "unsafe durable JSON temporary: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Recognize one securely owned named temporary created by [`atomic_json`].
pub(crate) fn is_atomic_json_temporary(path: &Path) -> AgentResult<bool> {
    let Some(name) = path.file_name().filter(|name| atomic_temporary_name(name)) else {
        return Ok(false);
    };
    let parent = path.parent().ok_or_else(|| {
        AgentError::delivery(format!(
            "durable JSON temporary has no parent: {}",
            path.display()
        ))
    })?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|error| io_error("open durable JSON temporary directory", parent, error))?;
    let Some(file) = open_atomic_temporary_at(&directory, name)? else {
        return Ok(false);
    };
    validate_atomic_temporary(&file, path)?;
    Ok(true)
}

/// Remove only securely recognized atomic-write residue and fsync the containing directory.
///
/// The containing state directory is private mode 0700 and the same UID is trusted. Linux has no
/// inode-conditional unlink, so the held descriptor plus immediate `fstatat` identity comparison
/// prevents traversal and accidental foreign-artifact removal but cannot defeat a malicious
/// same-UID process that swaps the pathname after that comparison.
pub(crate) fn cleanup_atomic_json_temporaries(directory: &Path) -> AgentResult<()> {
    let directory_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)
        .map_err(|error| io_error("open durable JSON temporary directory", directory, error))?;
    let mut removed = false;
    for entry in fs::read_dir(directory)
        .map_err(|error| io_error("scan durable JSON temporaries", directory, error))?
    {
        let entry = entry
            .map_err(|error| io_error("read durable JSON temporary entry", directory, error))?;
        let name = entry.file_name();
        if !atomic_temporary_name(&name) {
            continue;
        }
        let Some(file) = open_atomic_temporary_at(&directory_file, &name)? else {
            continue;
        };
        validate_atomic_temporary(&file, &entry.path())?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error("inspect durable JSON temporary", &entry.path(), error))?;
        let name_c = CString::new(name.as_bytes()).map_err(|_| {
            AgentError::delivery("durable JSON temporary name contains NUL".to_owned())
        })?;
        let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
        let status = unsafe {
            libc::fstatat(
                directory_file.as_raw_fd(),
                name_c.as_ptr(),
                current.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                continue;
            }
            return Err(io_error(
                "revalidate durable JSON temporary",
                &entry.path(),
                error,
            ));
        }
        let current = unsafe { current.assume_init() };
        if current.st_dev != metadata.dev() || current.st_ino != metadata.ino() {
            return Err(AgentError::delivery(format!(
                "durable JSON temporary changed before cleanup: {}",
                entry.path().display()
            )));
        }
        let status = unsafe { libc::unlinkat(directory_file.as_raw_fd(), name_c.as_ptr(), 0) };
        if status < 0 {
            return Err(io_error(
                "remove durable JSON temporary",
                &entry.path(),
                io::Error::last_os_error(),
            ));
        }
        removed = true;
    }
    if removed {
        directory_file
            .sync_all()
            .map_err(|error| io_error("sync durable JSON temporary directory", directory, error))?;
    }
    Ok(())
}

fn atomic_json_create(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("JSON path has no parent"))?;
    let (temporary, mut file) = temporary_file_io(parent)?;
    let result = (|| {
        {
            let mut writer = BufWriter::new(&mut file);
            serde_json::to_writer_pretty(&mut writer, value).map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        fs::remove_file(&temporary)?;
        sync_directory_io(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_file(parent: &Path) -> AgentResult<(PathBuf, File)> {
    temporary_file_io(parent)
        .map_err(|error| io_error("create durable JSON temporary", parent, error))
}

fn temporary_file_io(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..100 {
        let path = parent.join(format!(
            ".message.{}.{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique JSON temporary file",
    ))
}

fn read_json(path: &Path) -> AgentResult<Value> {
    read_json_with_policy(path, false)
}

pub(crate) fn read_private_json(path: &Path) -> AgentResult<Value> {
    read_json_with_policy(path, true)
}

fn read_json_with_policy(path: &Path, require_private: bool) -> AgentResult<Value> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| io_error("read JSON", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect JSON", path, error))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || require_private && metadata.permissions().mode() & 0o077 != 0
    {
        return Err(AgentError::delivery(format!(
            "unsafe JSON artifact: {}",
            path.display()
        )));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| io_error("read JSON", path, error))?;
    serde_json::from_str(&contents).map_err(|error| {
        AgentError::delivery(format!("cannot read JSON {}: {error}", path.display()))
    })
}

fn load_message(path: &Path) -> AgentResult<Map<String, Value>> {
    let value = read_json(path).map_err(|error| {
        AgentError::delivery(format!(
            "cannot read queued message {}: {error}",
            path.display()
        ))
    })?;
    let document = value.as_object().cloned().ok_or_else(|| {
        AgentError::delivery(format!(
            "queued message {} has no string text field",
            path.display()
        ))
    })?;
    if document
        .get("text")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(AgentError::delivery(format!(
            "queued message {} must have a nonempty string text field",
            path.display()
        )));
    }
    if document.get("id").is_some_and(|value| !value.is_string()) {
        return Err(AgentError::delivery(format!(
            "queued message {} has a non-string id field",
            path.display()
        )));
    }
    Ok(document)
}

fn delivery_attempts(document: &Map<String, Value>, path: &Path) -> AgentResult<u64> {
    let (key, value) = if let Some(value) = document.get("delivery_attempts") {
        ("delivery_attempts", Some(value))
    } else {
        (
            "tui_delivery_attempts",
            document.get("tui_delivery_attempts"),
        )
    };
    match value {
        None => Ok(0),
        Some(value) => value.as_u64().ok_or_else(|| {
            AgentError::delivery(format!(
                "queued message {} has an invalid nonnegative integer {key} field",
                path.display()
            ))
        }),
    }
}

/// The text a drain types for a queued message: its stored text, except that a chat request prompt
/// that recorded its message's create time gets its opening decided now. See `prompt_time`.
fn typed_text(document: &Map<String, Value>) -> AgentResult<String> {
    let text = message_text(document)?;
    let retimed = document
        .get("sent_at")
        .and_then(Value::as_str)
        .zip(document.get("opening").and_then(Value::as_str))
        .and_then(|(sent_at, opening)| {
            crate::prompt_time::retime(&text, sent_at, opening, crate::prompt_time::now_millis())
        });
    Ok(retimed.unwrap_or(text))
}

fn message_text(document: &Map<String, Value>) -> AgentResult<String> {
    document
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AgentError::delivery("queued message has no string text field"))
}

fn transition(source: &Path, destination: &Path) -> AgentResult<()> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(AgentError::delivery(format!(
                "refusing to replace durable queue artifact {}",
                destination.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io_error(
                "inspect durable queue destination",
                destination,
                error,
            ))
        }
    }
    fs::rename(source, destination)
        .map_err(|error| io_error("transition durable queue artifact", source, error))?;
    let destination_parent = destination.parent().expect("queue destination has parent");
    sync_directory(destination_parent)?;
    let source_parent = source.parent().expect("queue source has parent");
    if source_parent != destination_parent {
        sync_directory(source_parent)?;
    }
    Ok(())
}

fn failed_metadata(failed_path: &Path, outcome: &str, error: &str) -> AgentResult<()> {
    let filename = failed_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AgentError::delivery("failed artifact name is not valid UTF-8"))?;
    atomic_json(
        &failed_path.with_file_name(format!("{filename}.error")),
        &json!({
            "artifact": filename,
            "outcome": outcome,
            "error": error,
            "failed_at": unix_seconds(),
        }),
    )
}

fn quarantine_raw(path: &Path, failed: &Path, outcome: &str, error: &str) -> AgentResult<String> {
    let filename = path.file_name().ok_or_else(|| {
        AgentError::delivery(format!(
            "queued message has no filename: {}",
            path.display()
        ))
    })?;
    let destination = failed.join(filename);
    transition(path, &destination)?;
    failed_metadata(&destination, outcome, error)?;
    message_id_from_path(path)
}

fn recover_inflight(directories: &QueueDirectories) -> AgentResult<Vec<String>> {
    let mut recovered = Vec::new();
    for path in json_paths(&directories.inflight)? {
        let filename = path.file_name().expect("listed path has filename");
        let destination = directories.failed.join(filename);
        transition(&path, &destination)?;
        failed_metadata(
            &destination,
            "possibly_submitted",
            "recovered an inflight prompt after process restart; refusing automatic resubmission",
        )?;
        recovered.push(message_id_from_path(&path)?);
    }
    Ok(recovered)
}

fn json_paths(directory: &Path) -> AgentResult<Vec<PathBuf>> {
    let entries = fs::read_dir(directory)
        .map_err(|error| io_error("list durable queue directory", directory, error))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| io_error("read durable queue directory entry", directory, error))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".json"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// The inbox's prompts in the order they were queued: by `queued_at`, then by file name.
///
/// A chat request's file is named for its key, a hash, so name order alone is not the order of
/// arrival. An entry without a numeric `queued_at` comes first, by name: one in the subagent
/// message shape, which numbers its file names in sequence and records `queued_at` as text, if
/// at all, or one that cannot be read, which the drain loop then quarantines.
fn inbox_in_queue_order(inbox: &Path) -> AgentResult<Vec<PathBuf>> {
    let mut entries: Vec<(Option<f64>, PathBuf)> = json_paths(inbox)?
        .into_iter()
        .map(|path| {
            let queued_at = read_json(&path)
                .ok()
                .and_then(|document| document.get("queued_at").and_then(Value::as_f64))
                // `total_cmp` puts -0.0 before 0.0; make them one time, as Python does.
                .map(|seconds| if seconds == 0.0 { 0.0 } else { seconds });
            (queued_at, path)
        })
        .collect();
    entries.sort_by(|(left_at, left), (right_at, right)| {
        match (left_at, right_at) {
            (Some(left_time), Some(right_time)) => left_time.total_cmp(right_time),
            _ => left_at.is_some().cmp(&right_at.is_some()),
        }
        .then_with(|| left.cmp(right))
    });
    Ok(entries.into_iter().map(|(_, path)| path).collect())
}

fn identifiers(directory: &Path) -> AgentResult<Vec<String>> {
    json_paths(directory)?
        .iter()
        .map(|path| message_id_from_path(path))
        .collect()
}

fn identifiers_if_directory(directory: &Path) -> AgentResult<Vec<String>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => identifiers(directory),
        Ok(_) => Err(AgentError::delivery(format!(
            "unsafe queue state directory: {}",
            directory.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(io_error("inspect queue state directory", directory, error)),
    }
}

fn message_id_from_path(path: &Path) -> AgentResult<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))
        .map(str::to_owned)
        .ok_or_else(|| {
            AgentError::delivery(format!(
                "queued message name is not valid UTF-8 JSON: {}",
                path.display()
            ))
        })
}

pub(crate) fn sync_directory(path: &Path) -> AgentResult<()> {
    sync_directory_io(path).map_err(|error| io_error("sync durable queue directory", path, error))
}

/// A test's hook for [`DIRECTORY_SYNC_HOOK`].
#[cfg(test)]
pub(crate) type DirectorySyncHook = Box<dyn FnMut(&Path) -> io::Result<()>>;

#[cfg(test)]
thread_local! {
    /// Called on this thread with each directory just before it is synced, so a test can record
    /// the syncs or make one fail.
    pub(crate) static DIRECTORY_SYNC_HOOK: std::cell::RefCell<Option<DirectorySyncHook>> =
        const { std::cell::RefCell::new(None) };
}

fn sync_directory_io(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    DIRECTORY_SYNC_HOOK
        .with(|hook| hook.borrow_mut().as_mut().map_or(Ok(()), |hook| hook(path)))?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?
        .sync_all()
}

fn validate_message_id(identifier: &str) -> AgentResult<()> {
    let bytes = identifier.as_bytes();
    let first_ok = bytes.first().is_some_and(u8::is_ascii_alphanumeric);
    let rest_ok = bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !first_ok || !rest_ok || bytes.len() > MESSAGE_ID_MAX {
        return Err(AgentError::delivery(
            "message id must be 1-255 ASCII letters, digits, dots, underscores, or hyphens and must start with a letter or digit",
        ));
    }
    Ok(())
}

fn generated_message_id() -> String {
    let nanoseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanoseconds:020}-{}", std::process::id())
}

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn real_path(path: &Path) -> AgentResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                AgentError::delivery(format!("cannot read current directory: {error}"))
            })?
            .join(path)
    };
    Ok(fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_normalize(&absolute)))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn client_error(error: AdapterError) -> AgentError {
    AgentError::Client(error)
}

fn io_error(action: &str, path: &Path, error: io::Error) -> AgentError {
    AgentError::delivery(format!("cannot {action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-agent-rust-{label}-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir(&path).expect("create test directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("set test directory mode");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct FakeState {
        infos: BTreeMap<String, AgentPaneInfo>,
        states: VecDeque<String>,
        last_state: String,
        runs: Vec<String>,
        waits: Vec<(String, String, u64)>,
        reads: Vec<String>,
        fail_run: bool,
        fail_wait: bool,
        unwrapped_empty: bool,
        /// A receipt the submission returns after writing, instead of `Unconfirmed`.
        receipt: Option<crate::submission::SubmissionReceipt>,
        codex_ready: bool,
        readiness_checks: usize,
    }

    struct FakeAgent {
        panes: Vec<Pane>,
        workspace_label: String,
        state: Mutex<FakeState>,
        run_gate: Option<Arc<RunGate>>,
        cancel_after_run: Option<Arc<AtomicBool>>,
        screen: Option<crate::submission::fake::FakeScreen>,
    }

    impl FakeAgent {
        fn new(states: &[&str]) -> Self {
            let mut infos = BTreeMap::new();
            infos.insert("w1:p1".to_owned(), default_info("w1:p1", "session-1"));
            Self {
                panes: vec![Pane {
                    pane_id: "w1:p1".to_owned(),
                    tab_id: "w1:t1".to_owned(),
                    workspace_id: "w1".to_owned(),
                }],
                workspace_label: "acme".to_owned(),
                state: Mutex::new(FakeState {
                    infos,
                    states: states.iter().map(|state| (*state).to_owned()).collect(),
                    last_state: states.last().copied().unwrap_or("idle").to_owned(),
                    ..FakeState::default()
                }),
                run_gate: None,
                cancel_after_run: None,
                screen: None,
            }
        }

        fn with_screen(screen: crate::submission::fake::FakeScreen) -> Self {
            let mut fake = Self::new(&["idle"]);
            fake.screen = Some(screen);
            fake
        }

        fn with_run_gate(states: &[&str], run_gate: Arc<RunGate>) -> Self {
            let mut fake = Self::new(states);
            fake.run_gate = Some(run_gate);
            fake
        }

        fn with_cancel_after_run(states: &[&str], cancelled: Arc<AtomicBool>) -> Self {
            let mut fake = Self::new(states);
            fake.cancel_after_run = Some(cancelled);
            fake
        }

        fn with_duplicate_session() -> Self {
            let mut fake = Self::new(&["idle"]);
            fake.panes.push(Pane {
                pane_id: "w1:p2".to_owned(),
                tab_id: "w1:t1".to_owned(),
                workspace_id: "w1".to_owned(),
            });
            fake.state
                .lock()
                .expect("fake state")
                .infos
                .insert("w1:p2".to_owned(), default_info("w1:p2", "session-1"));
            fake
        }

        fn runs(&self) -> Vec<String> {
            self.state.lock().expect("fake state").runs.clone()
        }

        fn waits(&self) -> Vec<(String, String, u64)> {
            self.state.lock().expect("fake state").waits.clone()
        }
    }

    impl AgentApi for FakeAgent {
        fn codex_idle_ready_with_runtime(
            &self,
            _info: &AgentPaneInfo,
            _runtime: &dyn AgentRuntime,
        ) -> crate::error::Result<bool> {
            let mut state = self.state.lock().unwrap();
            state.readiness_checks += 1;
            Ok(state.codex_ready)
        }

        fn panes(&self) -> crate::error::Result<Vec<Pane>> {
            Ok(self.panes.clone())
        }

        fn pane_info(&self, pane_id: &str) -> crate::error::Result<AgentPaneInfo> {
            let mut state = self.state.lock().expect("fake state");
            let mut info = state
                .infos
                .get(pane_id)
                .cloned()
                .ok_or_else(|| AdapterError::unavailable("missing pane"))?;
            if let Some(next) = state.states.pop_front() {
                state.last_state = next;
            }
            info.status.clone_from(&state.last_state);
            Ok(info)
        }

        fn workspace_label(&self, _workspace_id: &str) -> crate::error::Result<String> {
            Ok(self.workspace_label.clone())
        }

        fn run(&self, _pane_id: &str, text: &str) -> crate::error::Result<()> {
            if let Some(gate) = &self.run_gate {
                gate.enter_and_wait();
            }
            let mut state = self.state.lock().expect("fake state");
            state.runs.push(text.to_owned());
            if let Some(cancelled) = &self.cancel_after_run {
                cancelled.store(true, AtomicOrdering::SeqCst);
            }
            if state.fail_run {
                Err(AdapterError::unavailable("connection vanished after write"))
            } else {
                Ok(())
            }
        }

        fn submit_with_runtime(
            &self,
            pane_id: &str,
            text: &str,
            runtime: &dyn AgentRuntime,
        ) -> crate::error::Result<Submission> {
            match &self.screen {
                // Mirrors the production override: the composer is driven directly, for the
                // harness the screen draws.
                Some(screen) => {
                    // Read before the call: the screen's lock must not be held while it types.
                    let harness = screen.state().harness;
                    crate::submission::submit_verified(
                        screen,
                        pane_id,
                        harness,
                        text,
                        crate::submission::SubmitTimeouts {
                            stage: Duration::from_secs(3),
                            submit: Duration::from_secs(5),
                            ..crate::submission::SubmitTimeouts::default()
                        },
                        runtime,
                    )
                }
                None => {
                    self.run_with_runtime(pane_id, text, runtime)?;
                    let receipt = self.state.lock().expect("fake state").receipt.clone();
                    Ok(receipt.map_or(Submission::Unconfirmed, Submission::Verified))
                }
            }
        }

        fn wait_agent_status(
            &self,
            pane_id: &str,
            status: &str,
            timeout_ms: u64,
        ) -> crate::error::Result<()> {
            let mut state = self.state.lock().expect("fake state");
            state
                .waits
                .push((pane_id.to_owned(), status.to_owned(), timeout_ms));
            if state.fail_wait {
                Err(AdapterError::unavailable("no working transition"))
            } else {
                Ok(())
            }
        }

        fn read(
            &self,
            _pane_id: &str,
            source: &str,
            _lines: Option<usize>,
        ) -> crate::error::Result<String> {
            let mut state = self.state.lock().expect("fake state");
            state.reads.push(source.to_owned());
            if source == "recent-unwrapped" && state.unwrapped_empty {
                Ok(String::new())
            } else {
                Ok("agent transcript\n".to_owned())
            }
        }
    }

    #[derive(Default)]
    struct RunGate {
        state: Mutex<(usize, bool)>,
        changed: Condvar,
    }

    impl RunGate {
        fn enter_and_wait(&self) {
            let mut state = self.state.lock().expect("run gate");
            state.0 += 1;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).expect("run gate wait");
            }
        }

        fn wait_for_entries(&self, expected: usize, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut state = self.state.lock().expect("run gate");
            while state.0 < expected {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let (next, timed) = self
                    .changed
                    .wait_timeout(state, remaining)
                    .expect("run gate timed wait");
                state = next;
                if timed.timed_out() && state.0 < expected {
                    return false;
                }
            }
            true
        }

        fn release(&self) {
            let mut state = self.state.lock().expect("run gate");
            state.1 = true;
            self.changed.notify_all();
        }
    }

    #[derive(Default)]
    struct FakeRuntime {
        millis: AtomicU64,
    }

    impl AgentRuntime for FakeRuntime {
        fn monotonic(&self) -> Duration {
            Duration::from_millis(self.millis.load(AtomicOrdering::SeqCst))
        }

        fn sleep(&self, duration: Duration) {
            self.millis.fetch_add(
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                AtomicOrdering::SeqCst,
            );
        }
    }

    struct CancelRuntime {
        cancel_after_checks: u64,
        checks: AtomicU64,
    }

    impl CancelRuntime {
        fn new(cancel_after_checks: u64) -> Self {
            Self {
                cancel_after_checks,
                checks: AtomicU64::new(0),
            }
        }
    }

    impl AgentRuntime for CancelRuntime {
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }

        fn sleep(&self, _duration: Duration) {}

        fn cancelled(&self) -> bool {
            self.checks.fetch_add(1, AtomicOrdering::SeqCst) >= self.cancel_after_checks
        }

        fn delivery_wait_chunk(&self) -> Option<Duration> {
            Some(Duration::from_millis(1))
        }
    }

    struct CancellationFlagRuntime {
        cancelled: Arc<AtomicBool>,
    }

    impl AgentRuntime for CancellationFlagRuntime {
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }

        fn sleep(&self, _duration: Duration) {}

        fn cancelled(&self) -> bool {
            self.cancelled.load(AtomicOrdering::SeqCst)
        }

        fn delivery_wait_chunk(&self) -> Option<Duration> {
            Some(Duration::from_millis(1))
        }
    }

    #[derive(Default)]
    struct CancelOnSleep {
        cancelled: AtomicBool,
    }

    impl AgentRuntime for CancelOnSleep {
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }

        fn sleep(&self, _duration: Duration) {
            self.cancelled.store(true, AtomicOrdering::SeqCst);
        }

        fn cancelled(&self) -> bool {
            self.cancelled.load(AtomicOrdering::SeqCst)
        }
    }

    fn default_info(pane_id: &str, session: &str) -> AgentPaneInfo {
        AgentPaneInfo {
            pane_id: pane_id.to_owned(),
            workspace_id: "w1".to_owned(),
            cwd: "/work/mtg".to_owned(),
            agent: Some("codex".to_owned()),
            status: "idle".to_owned(),
            session_agent: Some("codex".to_owned()),
            session_value: Some(session.to_owned()),
            scroll: None,
            terminal_id: None,
            tab_id: None,
        }
    }

    fn target() -> Target {
        Target {
            pane_id: Some("w1:p1".to_owned()),
            session_agent: Some("codex".to_owned()),
            session_value: Some("session-1".to_owned()),
            expected_agent: Some("codex".to_owned()),
            expected_workspace: Some("acme".to_owned()),
            expected_cwd: Some(PathBuf::from("/work/mtg")),
        }
    }

    #[test]
    fn resolved_pane_lock_digest_is_byte_for_byte_compatible() {
        assert_eq!(
            pane_lock_digest("w1:p1"),
            "a176b65b6a799f512519f02899c894b47e31ae17567009e92b90383974dcd38c"
        );
    }

    #[test]
    fn herdr_093_drain_uses_codex_readiness_only_for_unknown_and_preserves_native_states() {
        for (status, ready, delivered, checked) in [
            ("unknown", true, true, 1),
            ("unknown", false, false, 1),
            ("idle", false, true, 0),
            ("done", false, true, 0),
            ("working", true, false, 0),
            ("blocked", true, false, 0),
        ] {
            let directory = TestDirectory::new("herdr093-ready");
            let fake = FakeAgent::new(&[status]);
            fake.state.lock().unwrap().codex_ready = ready;
            enqueue(directory.path(), "one prompt", Some("once")).unwrap();
            let result = drain_with_runtime(
                &fake,
                &target(),
                directory.path(),
                DrainOptions {
                    ready_timeout: Duration::ZERO,
                    ..DrainOptions::default()
                },
                &FakeRuntime::default(),
            )
            .unwrap();
            assert_eq!(result.delivered == ["once"], delivered, "{status}");
            assert_eq!(
                fake.state.lock().unwrap().readiness_checks,
                checked,
                "{status}"
            );
            if delivered {
                assert_eq!(fake.runs(), ["one prompt"]);
            } else {
                assert!(fake.runs().is_empty());
                let document =
                    read_private_json(&directory.path().join("inbox/once.json")).unwrap();
                assert_eq!(document["delivery_attempts"], 0);
                assert_eq!(document["delivery_state"], "pending");
            }
        }
    }

    #[test]
    fn herdr_093_unknown_other_harness_never_uses_codex_readiness() {
        let directory = TestDirectory::new("herdr093-other");
        let fake = FakeAgent::new(&["unknown"]);
        {
            let mut state = fake.state.lock().unwrap();
            state.codex_ready = true;
            let info = state.infos.get_mut("w1:p1").unwrap();
            info.agent = Some("claude".to_owned());
            info.session_agent = Some("claude".to_owned());
        }
        let mut target = target();
        target.expected_agent = Some("claude".to_owned());
        target.session_agent = Some("claude".to_owned());
        enqueue(directory.path(), "one prompt", Some("once")).unwrap();
        let result = drain_with_runtime(
            &fake,
            &target,
            directory.path(),
            DrainOptions {
                ready_timeout: Duration::ZERO,
                ..DrainOptions::default()
            },
            &FakeRuntime::default(),
        )
        .unwrap();
        assert!(result.delivered.is_empty());
        assert!(fake.runs().is_empty());
        assert_eq!(fake.state.lock().unwrap().readiness_checks, 0);
    }

    #[test]
    fn session_resolution_refuses_ambiguity_and_contradictory_exact_pane() {
        let fake = FakeAgent::new(&["idle"]);
        let empty = Target {
            pane_id: Some(String::new()),
            ..Target::default()
        };
        let error = resolve_target(&fake, &empty).unwrap_err();
        assert!(error.to_string().contains("target needs"));

        let ambiguous = FakeAgent::with_duplicate_session();
        let error = resolve_target(&ambiguous, &target()).unwrap_err();
        assert!(error.to_string().contains("found 2"));

        let mut contradiction = target();
        contradiction.pane_id = Some("w1:p9".to_owned());
        let error = resolve_target(&fake, &contradiction).unwrap_err();
        assert!(error.to_string().contains("expected exact pane"));
    }

    /// A runtime that keeps `FakeRuntime`'s clock and notes each prompt reported printed, with
    /// how many prompts the queue at `queue` had in `inflight` and `processed` at that moment.
    #[derive(Default)]
    struct PrintNotingRuntime {
        clock: FakeRuntime,
        printed: Mutex<Vec<String>>,
        queue: Option<PathBuf>,
        queue_when_printed: Mutex<Vec<(usize, usize)>>,
    }

    impl AgentRuntime for PrintNotingRuntime {
        fn monotonic(&self) -> Duration {
            self.clock.monotonic()
        }

        fn sleep(&self, duration: Duration) {
            self.clock.sleep(duration);
        }

        fn prompt_printed(&self, message_id: &str) {
            self.printed
                .lock()
                .expect("printed prompts")
                .push(message_id.to_owned());
            if let Some(queue) = &self.queue {
                let count = |name: &str| json_paths(&queue.join(name)).expect("queue files").len();
                self.queue_when_printed
                    .lock()
                    .expect("queue counts")
                    .push((count("inflight"), count("processed")));
            }
        }
    }

    #[test]
    fn a_drain_notes_a_prompt_only_when_the_pane_printed_it_after_the_submission_key() {
        let directory = TestDirectory::new("printed");
        let fake = FakeAgent::with_screen(crate::submission::fake::FakeScreen::new("codex", false));
        let runtime = PrintNotingRuntime {
            queue: Some(directory.path().to_owned()),
            ..PrintNotingRuntime::default()
        };
        let result = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "print me",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        assert_eq!(
            *runtime.printed.lock().expect("printed prompts"),
            [result.message_id]
        );
        // The note comes while the prompt is still in flight, before the queue records it as
        // processed, so a stop between the two does not lose it.
        assert_eq!(
            *runtime.queue_when_printed.lock().expect("queue counts"),
            [(1, 0)]
        );
        assert_eq!(
            json_paths(&directory.path().join("processed"))
                .unwrap()
                .len(),
            1
        );

        // Without a screen to read, only the working state confirms the delivery.
        let directory = TestDirectory::new("unprinted");
        let fake = FakeAgent::new(&["idle"]);
        let runtime = PrintNotingRuntime::default();
        let result = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "trust me",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap();
        assert_eq!(result.delivered, [result.message_id]);
        assert!(runtime.printed.lock().expect("printed prompts").is_empty());
    }

    #[test]
    fn a_drain_notes_a_prompt_the_agent_queued_behind_another_without_printing_it() {
        // A busy Claude already shows its running-turn marker and its queued-message marker for
        // an earlier prompt, and shows nothing of a prompt queued behind it, so the submission
        // has no printed evidence. The prompt still verifiably left the checked composer.
        let directory = TestDirectory::new("queued-unprinted");
        let screen = crate::submission::fake::FakeScreen::new("claude", true);
        screen.state().queued.push("an earlier prompt".to_owned());
        screen.state().queued_hidden = true;
        let fake = FakeAgent::with_screen(screen);
        let runtime = PrintNotingRuntime::default();
        let result = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "queued behind it",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap();
        assert_eq!(result.delivered, std::slice::from_ref(&result.message_id));
        assert_eq!(
            *runtime.printed.lock().expect("noted prompts"),
            [result.message_id]
        );
    }

    #[test]
    fn multiline_busy_then_idle_is_atomic_and_confirmed() {
        let directory = TestDirectory::new("multiline");
        let fake = FakeAgent::new(&["working", "working", "idle"]);
        let runtime = FakeRuntime::default();
        let text = "first line\nsecond line\nthird line";
        let result = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            text,
            DrainOptions {
                ready_timeout: Duration::from_secs(1),
                ..DrainOptions::default()
            },
            &runtime,
        )
        .unwrap();
        assert_eq!(fake.runs(), [text]);
        assert_eq!(
            fake.waits(),
            [("w1:p1".to_owned(), "working".to_owned(), 30_000)]
        );
        assert_eq!(result.delivered, [result.message_id]);
    }

    #[test]
    fn queue_delivers_through_a_dropped_submit_key_without_a_working_wait() {
        let directory = TestDirectory::new("dropped-key");
        let screen = crate::submission::fake::FakeScreen::new("codex", false);
        screen.state().drop_keys = 2;
        let fake = FakeAgent::with_screen(screen);
        let runtime = FakeRuntime::default();
        let result = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "survive a dropped key",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        assert_eq!(result.delivered, [result.message_id]);
        let screen = fake.screen.as_ref().unwrap().state();
        assert_eq!(screen.pastes, ["survive a dropped key"]);
        assert_eq!(screen.keys, ["Enter", "Enter", "Enter"]);
        assert_eq!(screen.submitted, ["survive a dropped key"]);
        drop(screen);
        assert!(
            fake.runs().is_empty(),
            "the unverified primitive must not be used"
        );
        assert!(
            fake.waits().is_empty(),
            "screen proof replaces the working wait"
        );
    }

    #[test]
    fn a_receipt_that_neither_printed_nor_settled_is_quarantined() {
        let directory = TestDirectory::new("unproven-receipt");
        let fake = FakeAgent::new(&["idle"]);
        fake.state.lock().expect("fake state").receipt =
            Some(crate::submission::SubmissionReceipt {
                key: "Enter",
                key_presses: 1,
                elapsed: Duration::from_millis(10),
                evidence: "the composer emptied".to_owned(),
                printed: false,
                settled: false,
            });
        let runtime = FakeRuntime::default();
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "a receipt proves nothing",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap_err();
        assert_eq!(
            error.outcome(),
            Some(QueueOutcome::PossiblySubmitted),
            "{error}"
        );
        assert!(
            error.to_string().contains("neither printed nor settled"),
            "{error}"
        );
        assert_eq!(fake.runs(), ["a receipt proves nothing"]);
        assert_eq!(
            json_paths(&directory.path().join("failed")).unwrap().len(),
            1
        );
    }

    #[test]
    fn queue_keeps_a_prompt_pending_when_nothing_was_typed() {
        let directory = TestDirectory::new("not-staged");
        let screen = crate::submission::fake::FakeScreen::new("codex", false);
        screen.state().composer = "an operator's draft".to_owned();
        let fake = FakeAgent::with_screen(screen);
        let runtime = FakeRuntime::default();
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "wait your turn",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 75, "{error}");
        assert!(error.to_string().contains("refusing to append"), "{error}");
        let pending = json_paths(&directory.path().join("inbox")).unwrap();
        assert_eq!(pending.len(), 1);
        let document = read_json(&pending[0]).unwrap();
        assert_eq!(document["delivery_state"], "pending");
        assert!(document.get("possibly_submitted").is_none(), "{document}");
        assert!(document.get("inflight_at").is_none(), "{document}");
        assert!(json_paths(&directory.path().join("inflight"))
            .unwrap()
            .is_empty());
        assert!(json_paths(&directory.path().join("failed"))
            .unwrap_or_default()
            .is_empty());
        assert!(fake.screen.as_ref().unwrap().state().pastes.is_empty());

        fake.screen.as_ref().unwrap().state().composer.clear();
        let result = drain_with_runtime(
            &fake,
            &target(),
            directory.path(),
            DrainOptions::default(),
            &runtime,
        )
        .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        assert_eq!(
            fake.screen.as_ref().unwrap().state().submitted,
            ["wait your turn"]
        );
    }

    #[test]
    fn queue_quarantines_a_prompt_left_staged() {
        let directory = TestDirectory::new("left-staged");
        let screen = crate::submission::fake::FakeScreen::new("codex", false);
        screen.state().drop_keys = u32::MAX;
        let fake = FakeAgent::with_screen(screen);
        let runtime = FakeRuntime::default();
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "stuck in the composer",
            DrainOptions::default(),
            &runtime,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 76, "{error}");
        assert!(error.to_string().contains("NOT submitted"), "{error}");
        let artifact = &error.undelivered().expect("ambiguous details").artifact;
        let document = read_json(artifact).unwrap();
        assert_eq!(document["possibly_submitted"], true);
        let screen = fake.screen.as_ref().unwrap().state();
        assert_eq!(screen.pastes.len(), 1, "never typed twice");
        assert_eq!(screen.composer, "stuck in the composer");
    }

    /// A Claude Code pane that Herdr reports `working`, drawn by `screen`, and its target.
    fn working_claude(screen: crate::submission::fake::FakeScreen) -> (FakeAgent, Target) {
        let fake = FakeAgent::with_screen(screen);
        {
            let mut state = fake.state.lock().expect("fake state");
            state.states.clear();
            state.last_state = "working".to_owned();
            let info = state.infos.get_mut("w1:p1").expect("pane info");
            info.agent = Some("claude".to_owned());
            info.session_agent = Some("claude".to_owned());
        }
        let target = Target {
            session_agent: Some("claude".to_owned()),
            expected_agent: Some("claude".to_owned()),
            ..target()
        };
        (fake, target)
    }

    fn while_working() -> DrainOptions {
        DrainOptions {
            ready_timeout: Duration::ZERO,
            max_attempts: 1,
            deliver_while_working: true,
            ..DrainOptions::default()
        }
    }

    #[test]
    fn a_working_claude_is_typed_into_only_with_deliver_while_working() {
        let directory = TestDirectory::new("while-working");
        let (fake, target) =
            working_claude(crate::submission::fake::FakeScreen::new("claude", true));
        let runtime = FakeRuntime::default();
        let error = send_with_runtime(
            &fake,
            &target,
            directory.path(),
            "queue behind the turn",
            DrainOptions {
                deliver_while_working: false,
                ..while_working()
            },
            &runtime,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("did not become idle/done"),
            "{error}"
        );
        assert!(fake.screen.as_ref().unwrap().state().pastes.is_empty());

        let result =
            drain_with_runtime(&fake, &target, directory.path(), while_working(), &runtime)
                .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        let screen = fake.screen.as_ref().unwrap().state();
        assert_eq!(screen.queued, ["queue behind the turn"]);
        assert_eq!(screen.pastes.len(), 1);
        assert!(screen.composer.is_empty());
    }

    #[test]
    fn a_dropped_enter_in_a_working_claude_is_pressed_again() {
        let directory = TestDirectory::new("while-working-dropped");
        let screen = crate::submission::fake::FakeScreen::new("claude", true);
        screen.state().drop_keys = 2;
        let (fake, target) = working_claude(screen);
        let result = send_with_runtime(
            &fake,
            &target,
            directory.path(),
            "after two drops",
            while_working(),
            &FakeRuntime::default(),
        )
        .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        let screen = fake.screen.as_ref().unwrap().state();
        assert_eq!(screen.keys, ["Enter", "Enter", "Enter"]);
        assert_eq!(screen.queued, ["after two drops"]);
        assert_eq!(screen.pastes.len(), 1, "never typed twice");
    }

    #[test]
    fn a_prompt_a_working_claude_never_accepts_stays_pending_and_is_taken_back() {
        let directory = TestDirectory::new("while-working-unaccepted");
        let screen = crate::submission::fake::FakeScreen::new("claude", true);
        screen.state().drop_submit_keys = true;
        let (fake, target) = working_claude(screen);
        let runtime = FakeRuntime::default();
        let error = send_with_runtime(
            &fake,
            &target,
            directory.path(),
            "never accepted",
            while_working(),
            &runtime,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 75, "{error}");
        assert!(
            error.to_string().contains("taken back out of the composer"),
            "{error}"
        );
        let pending = json_paths(&directory.path().join("inbox")).unwrap();
        assert_eq!(pending.len(), 1);
        let document = read_json(&pending[0]).unwrap();
        assert_eq!(document["delivery_state"], "pending");
        assert!(document.get("possibly_submitted").is_none(), "{document}");
        assert!(
            document
                .get("delivery_attempts")
                .is_none_or(|attempts| attempts == 0),
            "{document}"
        );
        assert!(json_paths(&directory.path().join("failed"))
            .unwrap_or_default()
            .is_empty());
        {
            let screen = fake.screen.as_ref().unwrap().state();
            assert!(screen.composer.is_empty(), "{:?}", screen.composer);
            assert!(screen.queued.is_empty());
        }

        // Once Claude Code accepts Enter again, the same prompt is typed once more and queued.
        fake.screen.as_ref().unwrap().state().drop_submit_keys = false;
        let result =
            drain_with_runtime(&fake, &target, directory.path(), while_working(), &runtime)
                .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        assert_eq!(
            fake.screen.as_ref().unwrap().state().queued,
            ["never accepted"]
        );
    }

    #[test]
    fn deliver_while_working_still_waits_for_a_working_codex_and_for_a_draft() {
        let directory = TestDirectory::new("while-working-codex");
        let screen = crate::submission::fake::FakeScreen::new("codex", true);
        let fake = FakeAgent::with_screen(screen);
        {
            let mut state = fake.state.lock().expect("fake state");
            state.states.clear();
            state.last_state = "working".to_owned();
        }
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "codex keeps waiting",
            while_working(),
            &FakeRuntime::default(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("did not become idle/done"),
            "{error}"
        );
        assert!(fake.screen.as_ref().unwrap().state().pastes.is_empty());

        let directory = TestDirectory::new("while-working-draft");
        let screen = crate::submission::fake::FakeScreen::new("claude", true);
        screen.state().composer = "an operator's draft".to_owned();
        let (fake, target) = working_claude(screen);
        let error = send_with_runtime(
            &fake,
            &target,
            directory.path(),
            "wait for the draft",
            while_working(),
            &FakeRuntime::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("refusing to append"), "{error}");
        assert_eq!(
            fake.screen.as_ref().unwrap().state().composer,
            "an operator's draft"
        );
    }

    #[test]
    fn done_is_submit_safe_and_zero_working_timeout_still_waits_one_millisecond() {
        let directory = TestDirectory::new("done");
        let fake = FakeAgent::new(&["done"]);
        let result = send(
            &fake,
            &target(),
            directory.path(),
            "from done",
            DrainOptions {
                working_timeout: Duration::ZERO,
                ..DrainOptions::default()
            },
        )
        .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Delivered);
        assert_eq!(fake.runs(), ["from done"]);
        assert_eq!(fake.waits()[0].2, 1);
    }

    #[test]
    fn concurrent_senders_serialize_within_and_across_queue_roots() {
        for distinct_roots in [false, true] {
            let parent = TestDirectory::new(if distinct_roots {
                "distinct-roots"
            } else {
                "same-root"
            });
            let root_a = parent.path().join("queue-a");
            let root_b = if distinct_roots {
                parent.path().join("queue-b")
            } else {
                root_a.clone()
            };
            let gate = Arc::new(RunGate::default());
            let fake = Arc::new(FakeAgent::with_run_gate(&["idle"], gate.clone()));

            let first_fake = fake.clone();
            let first_target = target();
            let first = thread::spawn(move || {
                send(
                    first_fake.as_ref(),
                    &first_target,
                    &root_a,
                    "first",
                    DrainOptions::default(),
                )
            });
            assert!(gate.wait_for_entries(1, Duration::from_secs(5)));

            let second_fake = fake.clone();
            let second_target = target();
            let second = thread::spawn(move || {
                send(
                    second_fake.as_ref(),
                    &second_target,
                    &root_b,
                    "second",
                    DrainOptions::default(),
                )
            });
            assert!(
                !gate.wait_for_entries(2, Duration::from_millis(100)),
                "a second sender reached pane.run before the first released its lock"
            );
            assert!(fake.runs().is_empty());
            gate.release();
            first.join().expect("first sender").unwrap();
            second.join().expect("second sender").unwrap();
            assert_eq!(fake.runs(), ["first", "second"]);
        }
    }

    #[test]
    fn unit_test_target_locks_live_outside_the_host_wide_roots() {
        // Only the roots differ: the same pane digest a live agent or a real
        // agentctl binary locks on the host resolves to private files here, so
        // neither can stall these tests and these tests never touch the host locks.
        let hosts = [
            host_target_lock_root().expect("host target lock root"),
            legacy_host_target_lock_root(),
        ];
        let isolated = target_lock_paths("w1:p1").expect("isolated target lock paths");
        for path in &isolated {
            for host in &hosts {
                assert!(!path.starts_with(host), "{}", path.display());
            }
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some(format!("{}.lock", pane_lock_digest("w1:p1")).as_str())
            );
        }
    }

    #[test]
    fn host_target_locks_live_in_account_state_not_a_cleaned_temporary_directory() {
        // The host's daily cleaner deletes files under these directories by modification
        // time, and a lock file is never written, so a held lock there was deleted once it
        // was four days old and a second sender locked a fresh file at the same path.
        let [host, legacy] = host_target_lock_roots().expect("host target lock roots");
        let home = crate::client::account_home().expect("account home");
        assert_eq!(host, home.join(".local/state/agentctl/target-locks"));
        for cleaned in ["/tmp", "/var/tmp", "/dev/shm"] {
            assert!(!host.starts_with(cleaned), "{}", host.display());
        }
        // Earlier editions keep the legacy name, so new editions must keep locking it too.
        assert_eq!(
            legacy,
            PathBuf::from(format!("/tmp/herdr-agent-target-locks-{}", unsafe {
                libc::getuid()
            }))
        );
    }

    #[test]
    fn a_relative_account_home_is_refused() {
        // Senders started in different directories would resolve a relative home to
        // different lock files and stop excluding each other.
        let error = target_lock_root_under(PathBuf::from("relative-home"))
            .expect_err("a relative home is refused");
        assert!(
            error.to_string().contains("not an absolute path"),
            "{error}"
        );
        assert_eq!(
            target_lock_root_under(PathBuf::from("/home/someone")).expect("absolute home"),
            PathBuf::from("/home/someone/.local/state/agentctl/target-locks")
        );
    }

    #[test]
    fn a_lock_file_unlinked_while_waited_on_is_not_held() {
        // The cleaner unlinks the file while this sender waits on it, and a sender that takes
        // only the /tmp file then locks a fresh file at the same path. Taking the lock on the
        // unlinked file would let both senders in; this sender must contend on the new file.
        let directory = TestDirectory::new("unlinked-target-lock");
        let path = directory.path().join("target.lock");
        let mut replacement: Option<File> = None;
        let mut waits = 0;
        let held = lock_current_file(&path, "test target lock", &mut |file, path| {
            waits += 1;
            if waits == 1 {
                fs::remove_file(path).expect("unlink waited-on lock");
                let other = open_private_lock(path, "replacement lock").expect("replacement");
                other
                    .lock_exclusive()
                    .expect("other sender holds the replacement");
                replacement = Some(other);
            } else {
                assert!(
                    FileExt::try_lock_exclusive(file).is_err(),
                    "the retry must open the file the other sender holds"
                );
                replacement = None;
            }
            FileExt::lock_exclusive(file).map_err(|error| io_error("lock", path, error))
        })
        .expect("lock the current file");
        assert_eq!(waits, 2);
        let current = fs::metadata(&path).expect("current lock file");
        let metadata = held.metadata().expect("held lock file");
        assert_eq!(
            (current.dev(), current.ino()),
            (metadata.dev(), metadata.ino())
        );
    }

    #[test]
    fn lock_files_are_refreshed_only_when_the_marker_is_due() {
        let root = test_target_lock_root().join("refresh-when-due");
        let directory = root.join("legacy");
        fs::create_dir_all(&directory).expect("create test directory");
        let marker = root.join(LEGACY_REFRESH_MARKER);
        let lock_file = directory.join("idle.lock");
        let stale = SystemTime::now() - Duration::from_secs(5 * 24 * 60 * 60);
        let age = |path: &Path, when: SystemTime| {
            File::options()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .and_then(|file| file.set_modified(when))
                .expect("set test file time");
        };
        let refreshed = || {
            fs::metadata(&lock_file)
                .and_then(|meta| meta.modified())
                .unwrap()
                > SystemTime::now() - Duration::from_secs(60)
        };
        let ages = [
            ("fresh", SystemTime::now(), false),
            ("due", SystemTime::now() - LEGACY_REFRESH_INTERVAL, true),
            (
                "future",
                SystemTime::now() + Duration::from_secs(24 * 60 * 60),
                true,
            ),
        ];
        for (case, marked, expected) in ages {
            age(&marker, marked);
            age(&lock_file, stale);
            refresh_lock_files_when_due(&marker, &directory);
            assert_eq!(refreshed(), expected, "marker {case}");
        }
        fs::remove_file(&marker).expect("remove marker");
        age(&lock_file, stale);
        refresh_lock_files_when_due(&marker, &directory);
        assert!(refreshed(), "missing marker");
        assert!(marker.exists(), "the refresh is recorded");
    }

    #[test]
    fn taking_a_target_lock_marks_every_lock_file_used() {
        // The cleaner deletes by modification time, and only a file it sees as four days old.
        // The cleaner also deletes a file a moment after examining it, and a target no one
        // sends to ages out, so every /tmp file is refreshed, not only this target's.
        let name = "marks-files-used";
        let mut paths = target_lock_paths(name).expect("target lock paths").to_vec();
        paths.push(target_lock_paths("an-idle-target").expect("idle target paths")[1].clone());
        let stale = SystemTime::now() - Duration::from_secs(5 * 24 * 60 * 60);
        for path in &paths {
            open_private_lock(path, "stale test lock")
                .expect("create lock file")
                .set_modified(stale)
                .expect("age lock file");
        }
        // No refresh recorded yet, as on a host's first new-edition delivery.
        let marker = paths[0].with_file_name(LEGACY_REFRESH_MARKER);
        let _ = fs::remove_file(&marker);
        let before = SystemTime::now() - Duration::from_secs(1);
        let lock = lock_target(name, "test target lock").expect("take target lock");
        for path in &paths {
            let modified = fs::metadata(path).and_then(|meta| meta.modified()).unwrap();
            assert!(modified >= before, "{} was not marked used", path.display());
        }
        drop(lock);
    }

    #[test]
    fn a_target_lock_waits_for_a_holder_of_either_root() {
        // A holder of the legacy file is an earlier edition; a holder of the account file
        // is a current one. A new sender must wait for both.
        let name = "either-root-holder";
        for held in 0..2 {
            let path = target_lock_paths(name).expect("target lock paths")[held].clone();
            let holder = open_private_lock(&path, "held test lock").expect("open holder");
            holder.lock_exclusive().expect("hold lock");
            let runtime = CancelOnSleep::default();
            let error = lock_target_with(name, "contended test lock", |file, path| {
                lock_exclusive_with_runtime(file, path, "test contention", &runtime)
            })
            .err()
            .expect("a held root blocks the sender");
            assert!(error.to_string().contains("cancelled"), "{error}");
            drop(holder);
            let lock = lock_target(name, "uncontended test lock").expect("released lock");
            drop(lock);
        }
    }

    #[test]
    fn explicit_send_reserves_ids_under_the_delivery_transition_lock() {
        let directory = TestDirectory::new("explicit-send-reservation");
        let fake = FakeAgent::new(&["idle"]);
        bind_queue(directory.path(), &target()).unwrap();
        let lock =
            open_private_lock(&directory.path().join(".delivery.lock"), "test lock").unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        std::thread::scope(|scope| {
            let (started, ready) = std::sync::mpsc::channel();
            let fake = &fake;
            let directory = &directory;
            let sender = scope.spawn(move || {
                started.send(()).unwrap();
                send_identified(
                    fake,
                    &target(),
                    directory.path(),
                    "task",
                    "same-id",
                    DrainOptions::default(),
                )
            });
            ready.recv().unwrap();
            // The locked delivery transaction advances the ID while another sender waits.
            std::thread::sleep(Duration::from_millis(100));
            assert!(!directory.path().join("inbox/same-id.json").exists());
            atomic_json(
                &directory.path().join("processed/same-id.json"),
                &json!({"id":"same-id","text":"prior task","queued_at":0,"delivery_attempts":1}),
            )
            .unwrap();
            FileExt::unlock(&lock).unwrap();
            let error = sender.join().unwrap().unwrap_err();
            assert!(error.to_string().contains("already exists"));
        });
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn queue_rebind_is_idempotent_and_refuses_unrelated_identity() {
        let directory = TestDirectory::new("queue-rebind");
        let mut previous = target();
        previous.session_agent = None;
        previous.session_value = None;
        previous.expected_workspace = Some("old-label".to_owned());
        bind_queue(directory.path(), &previous).unwrap();
        let mut replacement = previous.clone();
        replacement.pane_id = Some("w2:p2".to_owned());
        replacement.expected_workspace = Some("new-label".to_owned());
        rebind_queue(directory.path(), &previous, &replacement).unwrap();
        rebind_queue(directory.path(), &previous, &replacement).unwrap();
        assert_eq!(
            read_private_json(&directory.path().join("target.json")).unwrap(),
            binding(&replacement).unwrap()
        );
        atomic_json(
            &directory.path().join("target.json"),
            &json!({
                "kind": "pane", "pane_id": "foreign", "expected_agent": "codex",
                "expected_workspace": null, "expected_cwd": "/work/mtg",
            }),
        )
        .unwrap();
        let error = rebind_queue(directory.path(), &previous, &replacement).unwrap_err();
        assert!(error.to_string().contains("refusing move"));
    }

    #[test]
    fn legacy_cli_workspace_pin_is_exact() {
        let directory = TestDirectory::new("legacy-workspace-pin");
        let mut pinned = target();
        pinned.expected_workspace = Some("legacy-project".to_owned());
        bind_queue(directory.path(), &pinned).unwrap();
        let mut unpinned = pinned;
        unpinned.expected_workspace = None;
        let error = validate_existing_binding(directory.path(), &unpinned, false).unwrap_err();
        assert!(error.to_string().contains("refusing different target"));
    }

    #[test]
    fn readiness_timeout_is_typed_pending_and_consumes_no_attempt() {
        let directory = TestDirectory::new("pending");
        let fake = FakeAgent::new(&["working"]);
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "poll artifact survives",
            DrainOptions {
                ready_timeout: Duration::ZERO,
                ..DrainOptions::default()
            },
            &FakeRuntime::default(),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 75);
        assert_eq!(error.outcome(), Some(QueueOutcome::Pending));
        assert!(error.safe_to_retry());
        let artifact = &error.undelivered().expect("pending details").artifact;
        let document = read_json(artifact).unwrap();
        assert_eq!(document["text"], "poll artifact survives");
        assert_eq!(document["delivery_attempts"], 0);
        assert!(json_paths(&directory.path().join("failed"))
            .unwrap()
            .is_empty());
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn cancellation_before_injection_leaves_prompt_safely_pending() {
        let directory = TestDirectory::new("cancel-before-injection");
        let fake = FakeAgent::new(&["idle"]);
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "do not inject",
            DrainOptions::default(),
            &CancelRuntime::new(0),
        )
        .expect_err("cancelled delivery remains pending");
        assert_eq!(error.outcome(), Some(QueueOutcome::Pending));
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn cancellation_after_injection_quarantines_unknown_outcome() {
        let directory = TestDirectory::new("cancel-after-injection");
        let cancelled = Arc::new(AtomicBool::new(false));
        let fake = FakeAgent::with_cancel_after_run(&["idle"], Arc::clone(&cancelled));
        let error = send_with_runtime(
            &fake,
            &target(),
            directory.path(),
            "inject once",
            DrainOptions::default(),
            &CancellationFlagRuntime { cancelled },
        )
        .expect_err("post-injection cancellation is uncertain");
        assert_eq!(error.outcome(), Some(QueueOutcome::PossiblySubmitted));
        assert_eq!(fake.runs(), vec!["inject once"]);
        assert_eq!(
            json_paths(&directory.path().join("failed"))
                .expect("failed artifacts")
                .len(),
            1
        );
    }

    #[test]
    fn cancellation_bounds_binding_delivery_and_target_flock_contention() {
        let directory = TestDirectory::new("cancel-lock-contention");
        let lock_paths = [
            directory.path().join(".binding.lock"),
            directory.path().join(".delivery.lock"),
            target_lock_paths("cancel-lock-contention").expect("target lock paths")[0].clone(),
        ];
        for path in lock_paths {
            if let Some(parent) = path.parent() {
                create_private_directory(parent, "test lock parent", true, true)
                    .expect("lock parent");
            }
            let holder = open_private_lock(&path, "held test lock").expect("open holder");
            holder.lock_exclusive().expect("hold lock");
            let contender = open_private_lock(&path, "contended test lock").expect("contender");
            let runtime = CancelOnSleep::default();
            let started = Instant::now();
            let error = lock_exclusive_with_runtime(&contender, &path, "test contention", &runtime)
                .expect_err("contention is cancelled");
            assert!(error.to_string().contains("cancelled"));
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[test]
    fn blocked_agent_is_pending_without_injection() {
        let directory = TestDirectory::new("blocked");
        let fake = FakeAgent::new(&["blocked"]);
        let error = send(
            &fake,
            &target(),
            directory.path(),
            "do not type",
            DrainOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("visible prompt"));
        assert!(matches!(error, AgentError::Pending(_)));
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn post_injection_failures_are_quarantined_once_as_possibly_submitted() {
        for failure in ["run", "wait"] {
            let directory = TestDirectory::new(failure);
            let fake = FakeAgent::new(&["idle"]);
            {
                let mut state = fake.state.lock().unwrap();
                state.fail_run = failure == "run";
                state.fail_wait = failure == "wait";
            }
            let error = send(
                &fake,
                &target(),
                directory.path(),
                "only once",
                DrainOptions::default(),
            )
            .unwrap_err();
            assert_eq!(error.exit_code(), 76);
            assert_eq!(error.outcome(), Some(QueueOutcome::PossiblySubmitted));
            assert!(!error.safe_to_retry());
            let artifact = &error.undelivered().expect("ambiguous details").artifact;
            let document = read_json(artifact).unwrap();
            assert_eq!(document["possibly_submitted"], true);
            assert_eq!(document["delivery_attempts"], 1);
            assert_eq!(fake.runs(), ["only once"]);
            assert!(json_paths(&directory.path().join("inflight"))
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn a_chat_prompt_waiting_in_the_queue_says_how_late_it_is_when_typed() {
        // Queued 30 seconds after its message was written, at 2026-10-07T12:45:00Z, and typed
        // an hour and twelve minutes after it, once the busy coordinator became ready.
        let directory = TestDirectory::new("retimed");
        crate::prompt_time::test_clock::set(1_791_377_130_000, -4 * 3_600, "EDT");
        let marked = crate::prompt_time::deferred("2026-10-07T12:45:00Z", "The user's request.");
        let identifier = enqueue(directory.path(), &marked, Some("chat-retimed")).unwrap();
        let queued = read_json(&directory.path().join(format!("inbox/{identifier}.json"))).unwrap();
        assert_eq!(
            queued["text"],
            "Sent 2026.10.07:08:45 EDT. The user's request."
        );
        assert_eq!(queued["sent_at"], "2026-10-07T12:45:00Z");
        crate::prompt_time::test_clock::set(1_791_381_420_000, -4 * 3_600, "EDT");
        let fake = FakeAgent::new(&["idle"]);
        let result = drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(result.delivered, [identifier]);
        assert_eq!(
            fake.runs(),
            ["Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. The user's request."]
        );
    }

    #[test]
    fn restart_quarantines_inflight_without_resubmission() {
        let directory = TestDirectory::new("restart");
        let identifier = enqueue(directory.path(), "at most once", Some("000000000007")).unwrap();
        fs::rename(
            directory.path().join("inbox/000000000007.json"),
            directory.path().join("inflight/000000000007.json"),
        )
        .unwrap();
        let fake = FakeAgent::new(&["idle"]);
        let result = drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(result.outcome, QueueOutcome::PossiblySubmitted);
        assert_eq!(result.quarantined, [identifier]);
        assert!(fake.runs().is_empty());
        assert!(directory
            .path()
            .join("failed/000000000007.json.error")
            .is_file());
    }

    #[test]
    fn malformed_fifo_head_preserves_raw_and_does_not_block_valid_prompt() {
        let directory = TestDirectory::new("malformed");
        prepare(directory.path()).unwrap();
        let raw = b"{not json\n";
        fs::write(directory.path().join("inbox/000000000001.json"), raw).unwrap();
        fs::write(
            directory.path().join("inbox/000000000002.json"),
            br#"{"id":[],"text":"invalid id"}
"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("inbox/000000000003.json"),
            br#"{"id":"good","text":"deliver me"}
"#,
        )
        .unwrap();
        let fake = FakeAgent::new(&["idle"]);
        let result = drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(
            fs::read(directory.path().join("failed/000000000001.json")).unwrap(),
            raw
        );
        let metadata = read_json(&directory.path().join("failed/000000000001.json.error")).unwrap();
        assert_eq!(metadata["outcome"], "invalid_message");
        assert!(directory.path().join("failed/000000000002.json").is_file());
        assert_eq!(result.quarantined, ["000000000001", "000000000002"]);
        assert_eq!(result.delivered, ["good"]);
        assert_eq!(fake.runs(), ["deliver me"]);
    }

    #[test]
    fn queue_binding_refuses_a_different_session_without_moving_prompt() {
        let directory = TestDirectory::new("binding");
        let fake = FakeAgent::new(&["working"]);
        let first = send(
            &fake,
            &target(),
            directory.path(),
            "bound prompt",
            DrainOptions {
                ready_timeout: Duration::ZERO,
                ..DrainOptions::default()
            },
        );
        assert!(matches!(first, Err(AgentError::Pending(_))));
        let mut different = target();
        different.session_value = Some("different".to_owned());
        let error =
            drain(&fake, &different, directory.path(), DrainOptions::default()).unwrap_err();
        assert!(error.to_string().contains("bound to"));
        let pending = json_paths(&directory.path().join("inbox")).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(fs::read_to_string(&pending[0])
            .unwrap()
            .contains("bound prompt"));
    }

    #[test]
    fn status_is_read_only_and_read_falls_back_from_empty_unwrapped_source() {
        let parent = TestDirectory::new("status");
        let root = parent.path().join("absent");
        let fake = FakeAgent::new(&["idle"]);
        fake.state.lock().unwrap().unwrapped_empty = true;
        let snapshot = status(&fake, &target(), &root).unwrap();
        assert!(snapshot.pending.is_empty());
        assert!(snapshot.inflight.is_empty());
        assert!(snapshot.failed.is_empty());
        assert!(!root.exists());
        assert_eq!(read(&fake, &target(), 17).unwrap(), "agent transcript\n");
        assert_eq!(
            fake.state.lock().unwrap().reads,
            ["recent-unwrapped", "recent"]
        );
    }

    #[test]
    fn read_capture_reads_only_the_screen_of_a_pane_that_keeps_no_scrollback() {
        let scroll = |max_offset_from_bottom| crate::client::PaneScroll {
            offset_from_bottom: 0,
            max_offset_from_bottom,
            viewport_rows: 52,
        };
        // Herdr reports no scrollback for Claude Code, which redraws one screen in place. Some
        // for a program such as Codex that writes into the scrollback, and none from an older
        // Herdr, which leaves the reads as they were.
        for (reported, reads) in [
            (Some(scroll(0)), &["visible"][..]),
            (Some(scroll(120)), &["recent-unwrapped", "recent"][..]),
            (None, &["recent-unwrapped", "recent"][..]),
        ] {
            let fake = FakeAgent::new(&["idle"]);
            {
                let mut state = fake.state.lock().unwrap();
                state.unwrapped_empty = true;
                state.infos.get_mut("w1:p1").unwrap().scroll = reported;
            }
            assert_eq!(
                read_capture(&fake, &target(), 17).unwrap(),
                "agent transcript\n"
            );
            assert_eq!(fake.state.lock().unwrap().reads, reads, "{reported:?}");
            // The `agentctl read` command keeps reading recent rows, which a person may want.
            fake.state.lock().unwrap().reads.clear();
            read(&fake, &target(), 17).unwrap();
            assert_eq!(
                fake.state.lock().unwrap().reads,
                ["recent-unwrapped", "recent"]
            );
        }
    }

    #[test]
    fn status_rejects_unsafe_existing_queue_without_mutating_it() {
        let parent = TestDirectory::new("status-safety");
        let root = parent.path().join("queue");
        prepare(&root).unwrap();
        bind_queue(&root, &target()).unwrap();
        let fake = FakeAgent::new(&["idle"]);

        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = status(&fake, &target(), &root).unwrap_err();
        assert!(error.to_string().contains("not private"));
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );

        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("target.json"), fs::Permissions::from_mode(0o644)).unwrap();
        let error = status(&fake, &target(), &root).unwrap_err();
        assert!(error.to_string().contains("unsafe JSON artifact"));

        fs::set_permissions(root.join("target.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let link = parent.path().join("queue-link");
        symlink(&root, &link).unwrap();
        let error = status(&fake, &target(), &link).unwrap_err();
        assert!(error.to_string().contains("unsafe queue directory"));
    }

    #[test]
    fn existing_python_subagent_message_shape_is_accepted() {
        let directory = TestDirectory::new("legacy");
        prepare(directory.path()).unwrap();
        fs::write(
            directory.path().join("inbox/000000000007.json"),
            br#"{"seq":7,"text":"legacy fifo","tui_delivery_attempts":0}
"#,
        )
        .unwrap();
        let fake = FakeAgent::new(&["idle"]);
        let result = drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(result.delivered, ["000000000007"]);
        assert_eq!(fake.runs(), ["legacy fifo"]);
        assert!(directory
            .path()
            .join("processed/000000000007.json")
            .is_file());
    }

    /// Writes one inbox entry as `enqueue` would, with the given `queued_at`.
    fn queue_at(directory: &TestDirectory, name: &str, queued_at: &str, text: &str) {
        fs::write(
            directory.path().join(format!("inbox/{name}.json")),
            format!(
                r#"{{"id":"{name}","text":"{text}","queued_at":{queued_at},"delivery_attempts":0}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn drain_types_the_inbox_in_queue_order_not_file_name_order() {
        // A chat request's file is named for its key, a hash, so names sort in no useful order.
        // Here they sort opposite to the queue times, and two entries share a time.
        let directory = TestDirectory::new("queue-order");
        prepare(directory.path()).unwrap();
        queue_at(&directory, "chat-d", "100.5", "first");
        queue_at(&directory, "chat-c", "200", "second");
        queue_at(&directory, "chat-a", "300", "third");
        queue_at(&directory, "chat-b", "300.0", "fourth");
        let fake = FakeAgent::new(&["idle"]);
        let result = drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(fake.runs(), ["first", "second", "third", "fourth"]);
        assert_eq!(result.delivered, ["chat-d", "chat-c", "chat-a", "chat-b"]);
        assert!(result.pending.is_empty());
    }

    #[test]
    fn negative_zero_and_zero_are_one_queue_time() {
        // Python compares -0.0 and 0.0 as equal, so both editions order the two by name.
        let directory = TestDirectory::new("queue-order-zero");
        prepare(directory.path()).unwrap();
        queue_at(&directory, "chat-a", "0.0", "first");
        queue_at(&directory, "chat-b", "-0.0", "second");
        let fake = FakeAgent::new(&["idle"]);
        drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(fake.runs(), ["first", "second"]);
    }

    #[test]
    fn a_held_oldest_prompt_holds_newer_prompts_whose_names_sort_first() {
        let directory = TestDirectory::new("queue-order-held");
        prepare(directory.path()).unwrap();
        queue_at(&directory, "chat-zzz", "100", "oldest");
        queue_at(&directory, "chat-aaa", "200", "middle");
        queue_at(&directory, "chat-mmm", "300", "newest");
        let busy = FakeAgent::new(&["working"]);
        let held = drain(
            &busy,
            &target(),
            directory.path(),
            DrainOptions {
                ready_timeout: Duration::ZERO,
                ..DrainOptions::default()
            },
        )
        .unwrap();
        assert_eq!(held.outcome, QueueOutcome::Pending);
        assert!(held.delivered.is_empty());
        assert!(busy.runs().is_empty());
        let inbox = directory.path().join("inbox");
        let oldest = read_json(&inbox.join("chat-zzz.json")).unwrap();
        assert_eq!(oldest["delivery_state"], "pending");
        assert!(oldest.get("delivery_error").is_some());
        for name in ["chat-aaa", "chat-mmm"] {
            let entry = read_json(&inbox.join(format!("{name}.json"))).unwrap();
            assert!(entry.get("delivery_state").is_none(), "{name}: {entry}");
        }

        let idle = FakeAgent::new(&["idle"]);
        let result = drain(&idle, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(idle.runs(), ["oldest", "middle", "newest"]);
        assert_eq!(result.delivered, ["chat-zzz", "chat-aaa", "chat-mmm"]);
    }

    #[test]
    fn an_entry_without_a_numeric_queue_time_comes_before_every_timed_entry() {
        // The subagent message shape numbers its file names in sequence and records
        // `queued_at` as text, if at all, so it cannot be placed by time; nor can a `queued_at`
        // that is true or null. The timed entry is queued at time 0, so an untimed entry taken
        // as queued at time 0 would follow it by name.
        for (label, queued_at) in [
            ("absent", None),
            ("text", Some(r#""2026-10-03T10:00:00Z""#)),
            ("bool", Some("true")),
            ("null", Some("null")),
        ] {
            let directory = TestDirectory::new(&format!("queue-order-{label}"));
            prepare(directory.path()).unwrap();
            queue_at(&directory, "chat-a", "0", "timed");
            let untimed = match queued_at {
                Some(queued_at) => format!(r#"{{"text":"untimed","queued_at":{queued_at}}}"#),
                None => r#"{"seq":9,"text":"untimed","tui_delivery_attempts":0}"#.to_owned(),
            };
            fs::write(directory.path().join("inbox/chat-z.json"), untimed).unwrap();
            let fake = FakeAgent::new(&["idle"]);
            drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
            assert_eq!(fake.runs(), ["untimed", "timed"], "{label}");
        }
    }

    #[test]
    fn entries_without_a_numeric_queue_time_go_first_by_file_name() {
        let directory = TestDirectory::new("queue-order-untimed-names");
        prepare(directory.path()).unwrap();
        queue_at(&directory, "chat-a", "0", "timed");
        fs::write(
            directory.path().join("inbox/chat-y.json"),
            r#"{"text":"untimed y"}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("inbox/chat-z.json"),
            r#"{"text":"untimed z","queued_at":"2026-10-03T10:00:00Z"}"#,
        )
        .unwrap();
        let fake = FakeAgent::new(&["idle"]);
        drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap();
        assert_eq!(fake.runs(), ["untimed y", "untimed z", "timed"]);
    }

    #[test]
    fn message_ids_and_queue_files_cannot_escape_the_private_root() {
        let directory = TestDirectory::new("ids");
        for identifier in ["../escape", "/absolute", "bad space", "é", "", ".hidden"] {
            let error = enqueue(directory.path(), "message", Some(identifier)).unwrap_err();
            assert!(error.to_string().contains("message id must"));
        }
        let identifier = "a".repeat(256);
        assert!(enqueue(directory.path(), "message", Some(&identifier)).is_err());
        assert!(!directory.0.parent().unwrap().join("escape.json").exists());

        let accepted = enqueue(directory.path(), "message", Some("A.good_id-7")).unwrap();
        assert_eq!(accepted, "A.good_id-7");
        assert!(enqueue(directory.path(), "other", Some("A.good_id-7"))
            .unwrap_err()
            .to_string()
            .contains("already exists"));
    }

    #[test]
    fn exact_message_state_reconciles_without_directory_scan() {
        let directory = TestDirectory::new("message-state");
        enqueue(directory.path(), "task", Some("chat-request-1")).unwrap();
        assert_eq!(
            message_state(directory.path(), "chat-request-1").unwrap(),
            Some(QueueMessageState::Pending)
        );
        assert_eq!(message_state(directory.path(), "absent").unwrap(), None);

        let directories = QueueDirectories::new(directory.path());
        transition(
            &directories.inbox.join("chat-request-1.json"),
            &directories.processed.join("chat-request-1.json"),
        )
        .unwrap();
        assert_eq!(
            message_state(directory.path(), "chat-request-1").unwrap(),
            Some(QueueMessageState::Processed)
        );
    }

    #[test]
    fn queue_directories_tighten_legacy_modes_but_reject_symlinks() {
        let parent = TestDirectory::new("directory-safety");
        let root = parent.path().join("queue");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        enqueue(&root, "legacy", Some("legacy")).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for directory in QueueDirectories::new(&root).all() {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let symlink_root = parent.path().join("queue-link");
        symlink(&root, &symlink_root).unwrap();
        let error = enqueue(&symlink_root, "unsafe", Some("unsafe")).unwrap_err();
        assert!(error.to_string().contains("unsafe queue directory"));
    }

    #[test]
    fn planted_delivery_lock_symlink_is_refused_without_touching_target() {
        let directory = TestDirectory::new("lock-symlink");
        enqueue(directory.path(), "queued", Some("queued")).unwrap();
        let victim = directory.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        fs::remove_file(directory.path().join(".delivery.lock")).unwrap();
        symlink(&victim, directory.path().join(".delivery.lock")).unwrap();
        let fake = FakeAgent::new(&["idle"]);
        let error = drain(&fake, &target(), directory.path(), DrainOptions::default()).unwrap_err();
        assert!(error.to_string().contains("queue delivery lock"));
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        assert!(fake.runs().is_empty());
    }

    fn queue_artifacts(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).expect("read fixture directory") {
                let path = entry.expect("fixture entry").path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        fs::read(&path).expect("read fixture artifact"),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    #[test]
    fn queue_waiter_refuses_archived_directory_or_replaced_lock() {
        struct OnLockWait<F> {
            clock: FakeRuntime,
            operation: Mutex<Option<F>>,
        }
        impl<F: FnOnce()> AgentRuntime for OnLockWait<F> {
            fn monotonic(&self) -> Duration {
                self.clock.monotonic()
            }

            fn sleep(&self, duration: Duration) {
                if let Some(operation) = self.operation.lock().expect("wait operation").take() {
                    operation();
                }
                self.clock.sleep(duration);
            }
        }

        for operation in ["enqueue", "drain", "drain-binding"] {
            for replacement in ["directory", "directory-reused-lock", "lock"] {
                let parent = TestDirectory::new("queue-generation-waiter");
                let active = parent.path().join("worker");
                let queue = active.join("queue");
                let archive = parent.path().join("archive/worker-old");
                create_private_directory(archive.parent().unwrap(), "test archive", false, false)
                    .unwrap();
                enqueue(&queue, "old pending prompt", Some("old")).unwrap();
                bind_queue(&queue, &target()).unwrap();
                fs::write(active.join("agent.json"), b"old lifetime\n").unwrap();
                for folder in ["inflight", "failed", "processed"] {
                    atomic_json(
                        &queue.join(folder).join("retained.json"),
                        &json!({"text": folder}),
                    )
                    .unwrap();
                }
                let lock_name = if operation == "drain-binding" {
                    ".binding.lock"
                } else {
                    ".delivery.lock"
                };
                let holder =
                    open_private_lock(&queue.join(lock_name), "test queue holder").unwrap();
                holder.lock_exclusive().unwrap();
                let attempted = AtomicBool::new(false);
                let expected = Mutex::new(BTreeMap::new());
                let runtime = OnLockWait {
                    clock: FakeRuntime::default(),
                    operation: Mutex::new(Some(|| {
                        attempted.store(true, AtomicOrdering::SeqCst);
                        if replacement == "lock" {
                            let path = queue.join(lock_name);
                            fs::rename(&path, queue.join(format!(".retired{lock_name}"))).unwrap();
                            drop(open_private_lock(&path, "replacement queue lock").unwrap());
                            expected
                                .lock()
                                .unwrap()
                                .insert(active.clone(), queue_artifacts(&active));
                        } else {
                            fs::rename(&active, &archive).unwrap();
                            enqueue(&queue, "new lifetime prompt", Some("new")).unwrap();
                            let mut next_target = target();
                            next_target.session_value = Some("session-2".to_owned());
                            bind_queue(&queue, &next_target).unwrap();
                            fs::write(active.join("agent.json"), b"new lifetime\n").unwrap();
                            if replacement == "directory-reused-lock" {
                                let path = queue.join(lock_name);
                                fs::rename(&path, queue.join(format!(".replacement{lock_name}")))
                                    .unwrap();
                                fs::hard_link(archive.join("queue").join(lock_name), &path)
                                    .unwrap();
                            }
                            let mut snapshots = expected.lock().unwrap();
                            snapshots.insert(archive.clone(), queue_artifacts(&archive));
                            snapshots.insert(active.clone(), queue_artifacts(&active));
                        }
                        FileExt::unlock(&holder).unwrap();
                    })),
                };
                let fake = FakeAgent::new(&["idle", "working", "idle"]);
                let result = if operation == "enqueue" {
                    enqueue_internal(&queue, "stale writer prompt", Some("stale"), true, &runtime)
                        .map(|_| ())
                } else {
                    drain_with_runtime(&fake, &target(), &queue, DrainOptions::default(), &runtime)
                        .map(|_| ())
                };
                let error = result.expect_err("a stale waiter must refuse the reused path");
                assert!(
                    attempted.load(AtomicOrdering::SeqCst),
                    "{operation}/{replacement}"
                );
                assert!(matches!(error, AgentError::Delivery(_)), "{error}");
                assert!(
                    error.to_string().contains("changed while waiting"),
                    "{error}"
                );
                assert!(fake.runs().is_empty());
                let snapshots = expected.lock().unwrap();
                assert!(!snapshots.is_empty());
                for (root, snapshot) in snapshots.iter() {
                    assert_eq!(
                        &queue_artifacts(root),
                        snapshot,
                        "{operation}/{replacement}"
                    );
                }
            }
        }
    }

    #[test]
    fn drain_keeps_queue_generation_from_binding_until_delivery() {
        let parent = TestDirectory::new("queue-generation-between-locks");
        let active = parent.path().join("worker");
        let candidate = parent.path().join("candidate");
        let archive = parent.path().join("old-worker");
        let queue = active.join("queue");
        enqueue(&queue, "old pending prompt", Some("old")).unwrap();
        let next_queue = candidate.join("queue");
        enqueue(&next_queue, "new pending prompt", Some("new")).unwrap();
        let mut next_target = target();
        next_target.session_value = Some("session-2".to_owned());
        bind_queue(&next_queue, &next_target).unwrap();
        let expected_new = queue_artifacts(&candidate);
        let swapped = Arc::new(AtomicBool::new(false));
        let noticed = Arc::clone(&swapped);
        let observed_queue = queue.clone();
        let observed_active = active.clone();
        let observed_archive = archive.clone();
        struct ClearSyncHook;
        impl Drop for ClearSyncHook {
            fn drop(&mut self) {
                DIRECTORY_SYNC_HOOK.with(|hook| *hook.borrow_mut() = None);
            }
        }
        let _clear_hook = ClearSyncHook;
        DIRECTORY_SYNC_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path| {
                if path == observed_queue
                    && observed_queue.join("target.json").exists()
                    && !noticed.swap(true, AtomicOrdering::SeqCst)
                {
                    fs::rename(&observed_active, &observed_archive)?;
                    fs::rename(&candidate, &observed_active)?;
                }
                Ok(())
            }));
        });
        let fake = FakeAgent::new(&["idle", "working", "idle"]);
        let error = drain(&fake, &target(), &queue, DrainOptions::default()).unwrap_err();
        assert!(swapped.load(AtomicOrdering::SeqCst));
        assert!(
            error
                .to_string()
                .contains("queue directory changed while waiting"),
            "{error}"
        );
        assert!(fake.runs().is_empty());
        assert_eq!(queue_artifacts(&active), expected_new);
        let old = read_private_json(&archive.join("queue/inbox/old.json")).unwrap();
        assert_eq!(old["delivery_attempts"], 0);
    }

    #[test]
    fn invalid_json_domains_and_empty_text_are_quarantined() {
        for (name, payload) in [
            ("nan", r#"{"id":"bad","text":NaN}"#),
            (
                "bool-attempts",
                r#"{"id":"bad","text":"prompt","delivery_attempts":true}"#,
            ),
            ("empty", r#"{"id":"bad","text":""}"#),
        ] {
            let directory = TestDirectory::new(name);
            let directories = prepare(directory.path()).unwrap();
            fs::write(directories.inbox.join("bad.json"), payload).unwrap();
            let result = drain(
                &FakeAgent::new(&["idle"]),
                &target(),
                directory.path(),
                DrainOptions::default(),
            )
            .unwrap();
            assert_eq!(result.quarantined, ["bad"]);
        }
    }

    #[test]
    fn exhausted_head_is_pending_and_missing_target_mutates_nothing() {
        let directory = TestDirectory::new("exhausted");
        let directories = prepare(directory.path()).unwrap();
        fs::write(
            directories.inbox.join("exhausted.json"),
            r#"{"id":"exhausted","text":"keep me","delivery_attempts":1}"#,
        )
        .unwrap();
        let result = drain(
            &FakeAgent::new(&["idle"]),
            &target(),
            directory.path(),
            DrainOptions {
                max_attempts: 1,
                ..DrainOptions::default()
            },
        )
        .unwrap();
        assert_eq!(result.outcome, QueueOutcome::Pending);
        assert_eq!(result.pending, ["exhausted"]);
        assert!(result
            .blocked
            .as_deref()
            .is_some_and(|value| value.contains("maximum delivery-attempt")));

        let missing = directory.path().join("missing-target");
        let error = send(
            &FakeAgent::new(&["idle"]),
            &Target::default(),
            &missing,
            "do not strand",
            DrainOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("target needs"));
        assert!(!missing.exists());
    }

    #[test]
    fn atomic_json_cleanup_removes_only_exact_secure_owned_temporaries() {
        let directory = TestDirectory::new("atomic-temp-cleanup");
        let qualifying = directory.path().join(".message.123.456");
        fs::write(&qualifying, b"partial").expect("write qualifying temporary");
        fs::set_permissions(&qualifying, fs::Permissions::from_mode(0o600))
            .expect("qualifying mode");
        let noncanonical = directory.path().join(".message.123.bad");
        fs::write(&noncanonical, b"preserve").expect("write noncanonical file");
        let non_utf8 = directory.path().join(std::ffi::OsString::from_vec(vec![
            b'.', b'm', b'e', b's', b's', b'a', b'g', b'e', b'.', 0xff, b'.', b'1',
        ]));
        fs::write(&non_utf8, b"preserve").expect("write non-UTF8 file");

        cleanup_atomic_json_temporaries(directory.path()).expect("clean qualifying temporary");
        assert!(!qualifying.exists());
        assert!(noncanonical.exists());
        assert!(non_utf8.exists());

        for label in ["symlink", "directory", "mode", "hardlink", "fifo"] {
            let unsafe_directory = TestDirectory::new(&format!("atomic-temp-{label}"));
            let path = unsafe_directory.path().join(".message.1.2");
            match label {
                "symlink" => {
                    let target = unsafe_directory.path().join("target");
                    fs::write(&target, b"target").expect("write symlink target");
                    symlink(&target, &path).expect("plant symlink");
                }
                "directory" => fs::create_dir(&path).expect("plant directory"),
                "mode" => {
                    fs::write(&path, b"wide").expect("plant wide file");
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
                        .expect("wide mode");
                }
                "hardlink" => {
                    let source = unsafe_directory.path().join("source");
                    fs::write(&source, b"linked").expect("write hardlink source");
                    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))
                        .expect("hardlink mode");
                    fs::hard_link(&source, &path).expect("plant hardlink");
                }
                "fifo" => {
                    let path_c = CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
                    assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
                }
                _ => unreachable!(),
            }
            assert!(
                cleanup_atomic_json_temporaries(unsafe_directory.path()).is_err(),
                "unsafe canonical temporary {label} must be refused"
            );
            assert!(path.exists() || fs::symlink_metadata(&path).is_ok());
        }
    }
}
