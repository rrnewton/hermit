//! Agentcloud sessions presented through an attached terminal in the agent's Herdr tab.
//!
//! `agentcloudctl create` owns the session and any node it provisions; `agentterm -s ID`
//! only renders that session in the tab. Input, status, waits, output, and retirement therefore
//! use the durable agentcloud verbs instead of terminal keystrokes or Herdr's harness detection.

use super::*;

/// Registry adapter and harness name for an agentcloud-backed agent.
pub(crate) const CLOUD_HARNESS: &str = "agentcloud";
/// Session drivers accepted by `agentcloudctl create --harness`.
pub(crate) const CLOUD_DRIVERS: [&str; 4] = ["native", "claude-code", "codex", "muse-code"];
/// First inputs travel as one process argument; stay well below Linux's per-argument limit.
pub(crate) const MAX_CLOUD_TEXT_BYTES: usize = 100 * 1024;
/// `agentcloudctl create` returns once the session exists; provisioning continues server-side.
const CREATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Bound for one ordinary agentcloud control verb.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(not(test))]
fn control_timeout() -> Duration {
    CONTROL_TIMEOUT
}

#[cfg(test)]
thread_local! {
    /// Per-test bound, so timeout paths are exercised without waiting two minutes.
    static CONTROL_TIMEOUT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn control_timeout() -> Duration {
    CONTROL_TIMEOUT_OVERRIDE
        .with(std::cell::Cell::get)
        .unwrap_or(CONTROL_TIMEOUT)
}
/// Extra process allowance beyond the wait bound passed to `agentcloudctl wait`.
const WAIT_MARGIN: Duration = Duration::from_secs(60);
const MAX_NOTE_CHARS: usize = 2000;
const VALID_EFFORTS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
/// Options that agentctl owns, or that must stay identical for every later verb it runs.
const RESERVED_CREATE_OPTIONS: [&str; 14] = [
    "--prompt",
    "--title",
    "--harness",
    "--model",
    "--effort",
    "--provision",
    "--envspec",
    "--purpose",
    "--workspace",
    "--node-id",
    "--ws-url",
    "--client-cert",
    "--client-key",
    "--as-crewmate",
];

/// Agentcloud-specific settings for `agentctl start --harness agentcloud`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudLaunch {
    /// Session driver for `agentcloudctl create --harness`; `None` keeps the server default.
    #[serde(default)]
    pub harness: Option<String>,
    /// Provision a fresh node for the session (`agentcloudctl create --provision`).
    #[serde(default)]
    pub provision: bool,
    /// Envspec the provisioned node is reserved from, which decides its checkout.
    #[serde(default)]
    pub envspec: Option<String>,
    /// Roster label for the provisioned node; requires `provision`.
    #[serde(default)]
    pub purpose: Option<String>,
    /// Absolute session working directory on the node; `None` keeps the server default.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Existing node bound at creation; conflicts with `provision`.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Session title for `agentcloudctl create --title`; `None` uses the agent name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

fn single_line(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains(['\0', '\n', '\r'])
}

/// Placeholders that `title`, `workspace`, and `node_id` may contain; `agentctl start` expands
/// them, so one profile can name the agent, this host, and the agent's directory.
pub(crate) const CLOUD_PLACEHOLDERS: [&str; 4] = ["{name}", "{host}", "{fqdn}", "{cwd}"];

/// The values the placeholders expand to for one launch.
pub(crate) struct LaunchContext<'a> {
    /// Agent name (`{name}`).
    pub(crate) name: &'a str,
    /// Full local host name (`{fqdn}`); `{host}` is its first label. `None` when unknown.
    pub(crate) hostname: Option<&'a str>,
    /// Absolute agent working directory (`{cwd}`).
    pub(crate) cwd: &'a str,
}

/// Fixed values that check a template's shape without depending on the local host.
const SAMPLE_CONTEXT: LaunchContext<'static> = LaunchContext {
    name: "agent",
    hostname: Some("host.example"),
    cwd: "/directory",
};

fn expand_template(
    label: &str,
    field: &str,
    template: &str,
    context: &LaunchContext<'_>,
) -> Result<String> {
    let mut expanded = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        expanded.push_str(&rest[..start]);
        let placeholder = rest[start..]
            .find('}')
            .map(|end| &rest[start..=start + end])
            .filter(|candidate| CLOUD_PLACEHOLDERS.contains(candidate))
            .ok_or_else(|| {
                fail(format!(
                    "{label} agentcloud {field} {template:?} has an unknown placeholder; use {}",
                    CLOUD_PLACEHOLDERS.join(", ")
                ))
            })?;
        let host = || {
            context.hostname.ok_or_else(|| {
                fail(format!(
                    "{label} agentcloud {field} uses {placeholder}, but the local host name is unknown"
                ))
            })
        };
        match placeholder {
            "{name}" => expanded.push_str(context.name),
            "{fqdn}" => expanded.push_str(host()?),
            "{host}" => expanded.push_str(host()?.split('.').next().unwrap_or_default()),
            _ => expanded.push_str(context.cwd),
        }
        rest = &rest[start + placeholder.len()..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

/// The local host name, as `hostname` prints it.
pub(crate) fn local_hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its full length and gethostname writes at most that much.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let length = buffer.iter().position(|byte| *byte == 0)?;
    String::from_utf8(buffer[..length].to_vec())
        .ok()
        .filter(|name| !name.is_empty())
}

impl CloudLaunch {
    /// Refuse contradictory or malformed settings before any registry or remote state exists.
    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        // Placeholders are checked by expanding fixed sample values, so a template is held to
        // exactly the rules its expansion must meet.
        self.expand(label, &SAMPLE_CONTEXT)?
            .validate_expanded(label)
    }

    /// Substitute the placeholders in `title`, `workspace`, and `node_id`.
    pub(crate) fn expand(&self, label: &str, context: &LaunchContext<'_>) -> Result<CloudLaunch> {
        let expand = |field: &str, value: &Option<String>| {
            value
                .as_deref()
                .map(|template| expand_template(label, field, template, context))
                .transpose()
        };
        Ok(CloudLaunch {
            workspace: expand("workspace", &self.workspace)?,
            node_id: expand("node_id", &self.node_id)?,
            title: expand("title", &self.title)?,
            ..self.clone()
        })
    }

    pub(super) fn validate_expanded(&self, label: &str) -> Result<()> {
        if let Some(driver) = self.harness.as_deref() {
            if !CLOUD_DRIVERS.contains(&driver) {
                return Err(fail(format!(
                    "{label} has unsupported agentcloud driver {driver:?}; use one of {}",
                    CLOUD_DRIVERS.join(", ")
                )));
            }
        }
        for (field, value) in [
            ("envspec", self.envspec.as_deref()),
            ("purpose", self.purpose.as_deref()),
            ("workspace", self.workspace.as_deref()),
            ("node_id", self.node_id.as_deref()),
            ("title", self.title.as_deref()),
        ] {
            if value.is_some_and(|value| !single_line(value)) {
                return Err(fail(format!(
                    "{label} agentcloud {field} must be a nonempty single-line string"
                )));
            }
        }
        if !self.provision && (self.envspec.is_some() || self.purpose.is_some()) {
            return Err(fail(format!(
                "{label} sets an agentcloud envspec or purpose without provision; enable provision (--provision) or remove them"
            )));
        }
        if self.provision && self.node_id.is_some() {
            return Err(fail(format!(
                "{label} cannot both provision a fresh node and bind node_id; choose one"
            )));
        }
        if self
            .workspace
            .as_deref()
            .is_some_and(|workspace| !workspace.starts_with('/'))
        {
            return Err(fail(format!(
                "{label} agentcloud workspace must be an absolute path on the session's node"
            )));
        }
        Ok(())
    }
}

/// Validate literal extra `agentcloudctl create` arguments from a profile or `--harness-arg`.
pub(crate) fn validate_create_arguments(label: &str, argv: &[String]) -> Result<()> {
    for item in argv {
        if item.is_empty() || item.contains('\0') {
            return Err(fail(format!(
                "{label} agentcloud arguments must be nonempty and contain no NUL"
            )));
        }
        let key = item.split_once('=').map_or(item.as_str(), |(key, _)| key);
        if !item.starts_with("--") || key == "--" {
            return Err(fail(format!(
                "{label} agentcloud argument {item:?} must be a long option; join values as --option=value because positional values and short options are reserved"
            )));
        }
        if RESERVED_CREATE_OPTIONS.contains(&key) {
            return Err(fail(format!(
                "{label} cannot pass {key} through raw agentcloud arguments; agentctl sets it from the name, brief, model, reasoning effort, or structured agentcloud settings, and endpoint or identity options must match every later agentctl verb"
            )));
        }
    }
    Ok(())
}

/// Executables used by agentcloud-backed agents.
#[derive(Clone, Debug)]
pub struct CloudTools {
    /// `agentcloudctl` name on PATH or explicit path.
    pub agentcloudctl: PathBuf,
    /// `agentterm` name on PATH or explicit path.
    pub agentterm: PathBuf,
    /// Agentcloud session this caller itself runs in. Input for another session is then sent as
    /// an attributed `agentcloudctl send-message` from it, because `agentcloudctl send` refuses
    /// to journal text as the human from inside a different session.
    pub caller_session: Option<String>,
    /// Orchestrator endpoint for new sessions (`--agentcloud-url`, else the environment). It is
    /// recorded at creation and passed as `--ws-url` to agentcloudctl and agentterm alike.
    pub endpoint: Option<String>,
    /// Whether `endpoint` was given explicitly, so a conflicting recorded endpoint is refused.
    pub endpoint_explicit: bool,
    /// The agentcloud session this process runs inside (`$AGENTCLOUD_SESSION_ID`). `Some` is
    /// passed to every agentcloudctl child as that exact value (an empty string meaning "outside
    /// any session"); `None` leaves the inherited environment untouched. Absence is never an
    /// instruction to erase attribution, so agentcloudctl's own guard always sees the real context.
    pub ambient_session: Option<String>,
    /// Local host name that `{host}` and `{fqdn}` in agentcloud settings expand to.
    pub hostname: Option<String>,
}

impl CloudTools {
    /// Settings derived from the agentcloud context variables, exactly as the command line does:
    /// the ambient session is both the sender and the context passed to children, and
    /// `$AGENTCLOUD_ORCHESTRATOR_URL` is the endpoint for new sessions.
    pub fn from_environment(environment: &dyn Fn(&str) -> Option<String>) -> Self {
        // The observed value is kept exactly (an empty variable stays empty) so children get
        // precisely the context this process had; only a sender needs a nonempty ID.
        let observed = environment("AGENTCLOUD_SESSION_ID");
        Self {
            agentcloudctl: PathBuf::from("agentcloudctl"),
            agentterm: PathBuf::from("agentterm"),
            caller_session: observed.clone().filter(|value| !value.is_empty()),
            endpoint: environment("AGENTCLOUD_ORCHESTRATOR_URL").filter(|value| !value.is_empty()),
            endpoint_explicit: false,
            ambient_session: observed,
            hostname: local_hostname(),
        }
    }
}

impl Default for CloudTools {
    /// Reads the real process environment, so no constructor can silently drop the session
    /// context a caller runs in.
    fn default() -> Self {
        Self::from_environment(&|name| std::env::var(name).ok())
    }
}

/// Durable agentcloud facts stored in the agent record. The session ID is `session_value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct CloudRecord {
    pub(super) launch: CloudLaunch,
    /// Resolved absolute `agentterm` executable run in the tab.
    pub(super) agentterm: String,
    /// Orchestrator endpoint every agentcloudctl verb and the viewer use for this session.
    /// Absent in records written before endpoints were recorded; the first mutating command
    /// resolves and saves it.
    #[serde(default)]
    pub(super) endpoint: Option<String>,
    #[serde(default)]
    pub(super) create_exit: Option<i32>,
    /// Whether `agentcloudctl create` verified its node clause and queued the brief.
    #[serde(default)]
    pub(super) verified: Option<bool>,
    #[serde(default)]
    pub(super) create_note: Option<String>,
    #[serde(default)]
    pub(super) terminal_identity: Option<CustomProcessIdentity>,
    #[serde(default)]
    pub(super) halted: bool,
    /// The session is the one `agentctl attach-self` ran inside, so stop only closes its viewer
    /// and archives the record; halting or archiving it would end the caller itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) attached_self: bool,
}

impl CloudRecord {
    pub(super) fn valid(&self) -> bool {
        // A record holds expanded settings, whose values may legitimately contain braces.
        self.launch.validate_expanded("agent record").is_ok()
            && self.agentterm.starts_with('/')
            && !self.agentterm.contains('\0')
            && self.endpoint.as_deref().is_none_or(valid_endpoint)
            // Diagnostics such as `create_note` never decide whether a record loads: an
            // unloadable record would strand a live session that status and stop must reach.
            && self
                .terminal_identity
                .as_ref()
                .is_none_or(CustomProcessIdentity::valid)
    }
}

/// A complete canonical session UUID, exactly as `agentcloudctl create` prints it.
///
/// Anything shorter is refused: `agentterm -s` resolves a unique *prefix* against its ledger and
/// the server, so a prefix could attach a different session than the one just created.
pub(super) fn valid_session_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// agentcloudctl's documented production endpoint. `agentcloudctl create --help` states that an
/// unset `--ws-url` falls back to `$AGENTCLOUD_ORCHESTRATOR_URL`, "then to the canonical prod
/// endpoint wss://mm.internalmeta.com/ws/chat"; agentterm documents different fallbacks, so
/// agentctl resolves this chain once and passes the result explicitly to both tools.
pub(crate) const DEFAULT_AGENTCLOUD_ENDPOINT: &str = "wss://mm.internalmeta.com/ws/chat";

/// Quote one word for a POSIX shell; plain words stay readable.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:=,+@%".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// A printable, copy-pasteable shell command.
fn shell_command<S: AsRef<str>>(words: &[S]) -> String {
    words
        .iter()
        .map(|word| shell_quote(word.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A websocket endpoint that can be passed literally as `--ws-url`.
pub(crate) fn valid_endpoint(value: &str) -> bool {
    (value.starts_with("ws://") || value.starts_with("wss://"))
        && value.len() <= 2048
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Insert the recorded endpoint after the verb, so no fallback can pick another orchestrator.
fn with_endpoint(mut arguments: Vec<String>, endpoint: &str) -> Vec<String> {
    arguments.splice(1..1, ["--ws-url".to_owned(), endpoint.to_owned()]);
    arguments
}

/// Fleet listings fetched during one invocation, keyed by endpoint.
pub(super) type FleetCache = BTreeMap<String, std::result::Result<Vec<Value>, String>>;

/// A fleet-listing activity word such as `waiting`; never a session identity.
fn valid_activity(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'_' | b'-'))
}

/// `create` prints only the session ID on stdout; anything else is not a recognizable ID.
fn parse_session_id(stdout: &str) -> Option<String> {
    let mut lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next()?;
    (lines.next().is_none() && valid_session_id(first)).then(|| first.to_owned())
}

/// Bound diagnostic text to `MAX_NOTE_CHARS` characters in total, suffix included, and replace
/// NUL so the value is always safe to store and print.
fn excerpt(text: &str) -> String {
    const SUFFIX: &str = " [truncated]";
    let text = text.trim().replace('\0', "\u{fffd}");
    if text.chars().count() <= MAX_NOTE_CHARS {
        return text;
    }
    let mut kept = text
        .chars()
        .take(MAX_NOTE_CHARS - SUFFIX.len())
        .collect::<String>();
    kept.push_str(SUFFIX);
    kept
}

/// Resolve a configured executable before any registry or remote state changes.
pub(crate) fn resolve_tool(configured: &Path, flag: &str) -> Result<PathBuf> {
    if configured.as_os_str().is_empty() {
        return Err(fail(format!("{flag} must not be empty")));
    }
    let candidates = if configured.components().count() > 1 || configured.is_absolute() {
        vec![configured.to_owned()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(configured))
            .collect()
    };
    let mut not_executable = None;
    for path in candidates {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
                return fs::canonicalize(&path)
                    .map_err(|error| fail(format!("cannot resolve {}: {error}", path.display())));
            }
            Ok(metadata) if metadata.is_file() => not_executable = Some(path),
            _ => {}
        }
    }
    Err(fail(match not_executable {
        Some(path) => format!(
            "{} is not executable; fix its permissions or pass {flag} PATH",
            path.display()
        ),
        None => format!(
            "{} executable not found; install it or pass {flag} PATH",
            configured.display()
        ),
    }))
}

struct ToolRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_tool(
    executable: &Path,
    arguments: &[String],
    timeout: Duration,
    ambient_session: Option<&str>,
) -> Result<ToolRun> {
    let mut command = std::process::Command::new(executable);
    command.args(arguments);
    // A known context is passed exactly; an unknown one is inherited unchanged, never removed.
    if let Some(session) = ambient_session {
        command.env("AGENTCLOUD_SESSION_ID", session);
    }
    let output = crate::client::bounded_output(command, timeout).map_err(|error| {
        AgentError::Client(crate::error::AdapterError::unavailable(format!(
            "cannot run {} {}: {error}",
            executable.display(),
            arguments.first().map_or("", String::as_str)
        )))
    })?;
    Ok(ToolRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Describe a failed invocation; 126 and 127 are named as invocation failures, never success.
fn failure(verb: &str, run: &ToolRun) -> String {
    let detail = if run.stderr.trim().is_empty() {
        excerpt(&run.stdout)
    } else {
        excerpt(&run.stderr)
    };
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    match run.code {
        Some(126) => format!("agentcloudctl {verb} could not execute a command (exit 126){detail}"),
        Some(127) => format!("agentcloudctl {verb} could not find a command (exit 127){detail}"),
        Some(code) => format!("agentcloudctl {verb} exited {code}{detail}"),
        None => format!("agentcloudctl {verb} was terminated by a signal{detail}"),
    }
}

fn client_error(message: impl Into<String>) -> AgentError {
    AgentError::Client(crate::error::AdapterError::unavailable(message))
}

impl AgentRecord {
    pub(super) fn is_cloud(&self) -> bool {
        self.adapter == CLOUD_HARNESS
    }

    fn cloud(&self) -> Result<&CloudRecord> {
        self.agentcloud
            .as_ref()
            .filter(|_| self.is_cloud())
            .ok_or_else(|| {
                fail(format!(
                    "agent {:?} is not an agentcloud session; use the ordinary agentctl commands",
                    self.name
                ))
            })
    }
}

impl<A: ManagedApi + ?Sized> ManagedAgents<'_, A> {
    /// Report whether a registered agent is backed by an agentcloud session.
    pub fn is_cloud(&self, agent_name: &str) -> Result<bool> {
        Ok(self.load(agent_name)?.is_cloud())
    }

    fn cloud_launch_failed(
        &self,
        record: &mut AgentRecord,
        options: &StartOptions,
        error: String,
    ) -> Result<Value> {
        record.lifecycle = "launch_failed".to_owned();
        record.error = Some(if options.environment.is_empty() {
            error.clone()
        } else {
            "launch failed with caller-supplied environment; details omitted from status".to_owned()
        });
        self.save(record)?;
        let directory = self.directory(&record.name)?;
        let stop = self.agentctl_command(&["stop", &record.name]);
        let attached_self = record
            .agentcloud
            .as_ref()
            .is_some_and(|cloud| cloud.attached_self);
        let remedy = match record.session_value.as_deref() {
            Some(_) if attached_self => format!(
                "; run `{stop}` to archive the record, which never halts this session"
            ),
            Some(session) => format!(
                "; agentcloud session {session} exists and may be live: attach with `{}`, or run `{stop}` to halt it and archive the record",
                self.viewer_command(record, session)?
            ),
            None => format!(
                "; no agentcloud session was recorded; run `{stop}` to archive the record"
            ),
        };
        Err(client_error(format!(
            "launch of {:?} failed: {error}; record retained at {}{remedy}",
            record.name,
            directory.display()
        )))
    }

    /// Create the agentcloud session, then attach its terminal in a fresh Herdr tab.
    pub(super) fn start_cloud(
        &self,
        agent_name: &str,
        cwd: PathBuf,
        mut options: StartOptions,
        reasoning_effort: Option<&str>,
    ) -> Result<Value> {
        let cwd_text = cwd.display().to_string();
        let launch = options.cloud.clone().unwrap_or_default().expand(
            "agentcloud launch",
            &LaunchContext {
                name: agent_name,
                hostname: self.cloud_tools.hostname.as_deref(),
                cwd: &cwd_text,
            },
        )?;
        launch.validate_expanded("agentcloud launch")?;
        if options.resume.is_some() {
            return Err(fail(
                "--resume does not apply to agentcloud agents; attach an existing session directly with `agentterm --ws-url ENDPOINT -s SESSION_ID`",
            ));
        }
        validate_create_arguments("agentcloud launch", &options.harness_args)?;
        if options
            .model
            .as_deref()
            .is_some_and(|model| !single_line(model))
        {
            return Err(fail("model must be a nonempty single-line string"));
        }
        if reasoning_effort.is_some_and(|effort| !VALID_EFFORTS.contains(&effort)) {
            return Err(fail(format!(
                "unsupported reasoning effort {reasoning_effort:?}; use one of {}",
                VALID_EFFORTS.join(", ")
            )));
        }
        if options
            .brief
            .as_deref()
            .is_some_and(|brief| brief.len() > MAX_CLOUD_TEXT_BYTES)
        {
            return Err(fail(format!(
                "agentcloud brief exceeds {MAX_CLOUD_TEXT_BYTES} bytes; agentcloudctl takes the first input as one argument, so shorten it and send the remainder with agentctl send"
            )));
        }
        options.environment = environment_entries(&options.environment)?;
        // agentcloudctl and agentterm fall back to different settings when no endpoint is given,
        // so the viewer could otherwise watch another orchestrator than the one controlled.
        let endpoint = self.default_endpoint();
        if !valid_endpoint(&endpoint) {
            return Err(fail(format!(
                "agentcloud endpoint {endpoint:?} must be a ws:// or wss:// URL without whitespace"
            )));
        }
        let agentcloudctl = resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin")?;
        let agentterm = resolve_tool(&self.cloud_tools.agentterm, "--agentterm-bin")?;

        let mut create = vec![
            "create".to_owned(),
            "--ws-url".to_owned(),
            endpoint.clone(),
            "--title".to_owned(),
            launch
                .title
                .clone()
                .unwrap_or_else(|| agent_name.to_owned()),
        ];
        push_value(&mut create, "--harness", launch.harness.as_deref());
        push_value(&mut create, "--model", options.model.as_deref());
        push_value(&mut create, "--effort", reasoning_effort);
        push_value(&mut create, "--workspace", launch.workspace.as_deref());
        push_value(&mut create, "--node-id", launch.node_id.as_deref());
        if launch.provision {
            create.push("--provision".to_owned());
        }
        push_value(&mut create, "--envspec", launch.envspec.as_deref());
        push_value(&mut create, "--purpose", launch.purpose.as_deref());
        create.extend_from_slice(&options.harness_args);
        let recorded_arguments = create.clone();
        // agentcloudctl refuses a leading /goal in --prompt; it must travel through send.
        let goal_brief = options
            .brief
            .as_deref()
            .filter(|brief| brief.trim_start().starts_with("/goal"));
        if let Some(brief) = options.brief.as_deref().filter(|_| goal_brief.is_none()) {
            create.extend(["--prompt".to_owned(), brief.to_owned()]);
        }

        // The project workspace policy places the terminal tab exactly as for a local worker,
        // and a mismatched explicit workspace is refused before any session is created.
        let project_workspace = self.start_workspace_policy(&options)?;
        let (_lock, mut record) = self.register_cloud_record(
            agent_name,
            &cwd,
            &options,
            reasoning_effort,
            CloudRecord {
                launch,
                agentterm: agentterm.display().to_string(),
                endpoint: Some(endpoint.clone()),
                create_exit: None,
                verified: None,
                create_note: None,
                terminal_identity: None,
                halted: false,
                attached_self: false,
            },
            recorded_arguments,
        )?;

        let run = match run_tool(
            &agentcloudctl,
            &create,
            CREATE_TIMEOUT,
            self.child_session(),
        ) {
            Ok(run) => run,
            Err(error) => {
                return self.cloud_launch_failed(&mut record, &options, error.to_string())
            }
        };
        let session = parse_session_id(&run.stdout);
        let verified = match (run.code, session.as_deref()) {
            (Some(0), Some(_)) => true,
            (Some(9), Some(_)) => false,
            (_, Some(session)) => {
                record.session_agent = Some(CLOUD_HARNESS.to_owned());
                record.session_value = Some(session.to_owned());
                record.native_session =
                    Some(NativeSession::new(CLOUD_HARNESS, session, "observed"));
                let message = format!(
                    "{} after printing session ID {session}",
                    failure("create", &run)
                );
                return self.cloud_launch_failed(&mut record, &options, message);
            }
            (Some(0), None) => {
                let message = format!(
                    "agentcloudctl create exited 0 but printed no recognizable session ID on stdout ({:?}); a session may exist, so check `{}` before retrying",
                    excerpt(&run.stdout),
                    self.agentcloudctl_command("list", &endpoint, &[])
                );
                return self.cloud_launch_failed(&mut record, &options, message);
            }
            (_, None) => {
                return self.cloud_launch_failed(&mut record, &options, failure("create", &run))
            }
        };
        let session = session.expect("matched session");
        record.session_agent = Some(CLOUD_HARNESS.to_owned());
        record.session_value = Some(session.clone());
        record.native_session = Some(NativeSession::new(CLOUD_HARNESS, &session, "observed"));
        let cloud = record.agentcloud.as_mut().expect("cloud record");
        cloud.create_exit = run.code;
        cloud.verified = Some(verified);
        cloud.create_note = (!run.stderr.trim().is_empty()).then(|| excerpt(&run.stderr));
        self.save(&record)?;

        if let Err(error) =
            self.create_presentation(&mut record, &options, project_workspace.as_deref())
        {
            return self.cloud_launch_failed(
                &mut record,
                &options,
                format!("cannot create the agent's Herdr tab: {error}"),
            );
        }
        let pane_id = record.pane_id.clone().expect("new tab has pane");
        let arguments = [
            "--ws-url".to_owned(),
            endpoint.clone(),
            "-s".to_owned(),
            session.clone(),
        ];
        match self.client.start_pane_command(
            &pane_id,
            &agentterm,
            &arguments,
            options.startup_timeout,
        ) {
            Ok(identity) => {
                record
                    .agentcloud
                    .as_mut()
                    .expect("cloud record")
                    .terminal_identity = Some(identity);
            }
            Err(error) => {
                return self.cloud_launch_failed(
                    &mut record,
                    &options,
                    format!("agentterm did not attach in pane {pane_id}: {error}"),
                );
            }
        }
        record.lifecycle = "running".to_owned();
        self.save(&record)?;

        let mut deferred = None;
        if let Some(brief) = goal_brief.filter(|_| verified) {
            // The viewer start can take seconds; recheck placement immediately before delivery,
            // exactly as an ordinary send does.
            let delivered = self
                .cloud_workspace_policy(&record)
                .and_then(|()| self.cloud_send_record(&record, brief, None));
            if let Err(error) = delivered {
                deferred = Some(format!(
                    "session {session} is running and attached, but its /goal brief was not delivered: {error}"
                ));
            }
        }
        if !verified {
            let brief = if options.brief.is_some() {
                "the brief was NOT queued; "
            } else {
                ""
            };
            let note = record
                .agentcloud
                .as_ref()
                .and_then(|cloud| cloud.create_note.clone())
                .map_or_else(String::new, |note| format!(" ({note})"));
            deferred = Some(format!(
                "agentcloudctl create exited 9: session {session} exists but its node clause is UNVERIFIED{note}; {brief}the agentterm tab is attached. Do not start it again: inspect it with `{}`, deliver work with `{}`, or retire it with `{}`",
                self.agentctl_command(&["status", agent_name]),
                self.agentctl_command(&["send", agent_name, "--file", "PROMPT"]),
                self.agentctl_command(&["stop", agent_name]),
            ));
        }
        if let Some(message) = deferred {
            return Err(client_error(message));
        }
        self.status(agent_name)
    }

    /// Register the agentcloud session this process runs inside as `agent_name` and present it
    /// through the viewer in a Herdr tab, so it appears beside the agents it starts. Nothing is
    /// created or changed in agentcloud: the session already exists and is the caller itself.
    pub fn attach_self(
        &self,
        agent_name: &str,
        cwd: &Path,
        options: StartOptions,
    ) -> Result<Value> {
        name(agent_name)?;
        let cwd = fs::canonicalize(cwd)
            .ok()
            .filter(|cwd| cwd.is_dir())
            .ok_or_else(|| fail(format!("cwd is not a directory: {}", cwd.display())))?;
        if options.startup_timeout.is_zero() || options.startup_timeout > Duration::from_secs(300) {
            return Err(fail("startup timeout must be between 0 and 300 seconds"));
        }
        let session = self.ambient().map(str::to_owned).ok_or_else(|| {
            fail(
                "attach-self presents the agentcloud session this process runs in, and $AGENTCLOUD_SESSION_ID is unset or empty; a session in a local terminal cannot be moved into Herdr, so start it in Herdr instead",
            )
        })?;
        if !valid_session_id(&session) {
            return Err(fail(format!(
                "$AGENTCLOUD_SESSION_ID {session:?} is not a complete session ID"
            )));
        }
        if !options.environment.is_empty() {
            return Err(fail(
                "attach-self takes no environment entries; the viewer tab needs none",
            ));
        }
        if let Some(owner) = self.identity_owner(CLOUD_HARNESS, &session, None)? {
            return Err(fail(format!(
                "this session is already registered as {:?}; inspect it with `{}`, or retire that record with `{}` first",
                owner.name,
                self.agentctl_command(&["status", &owner.name]),
                self.agentctl_command(&["stop", &owner.name]),
            )));
        }
        let endpoint = self.default_endpoint();
        if !valid_endpoint(&endpoint) {
            return Err(fail(format!(
                "agentcloud endpoint {endpoint:?} must be a ws:// or wss:// URL without whitespace"
            )));
        }
        let agentterm = resolve_tool(&self.cloud_tools.agentterm, "--agentterm-bin")?;
        let project_workspace = self.start_workspace_policy(&options)?;
        // Without a project workspace, the tab goes to the workspace named after the directory,
        // which is created when absent; agents this session starts later join it.
        let default_label = match project_workspace.clone() {
            Some(label) => label,
            None => cwd
                .file_name()
                .map(|base| base.to_string_lossy().into_owned())
                .filter(|base| !base.trim().is_empty())
                .unwrap_or_else(|| "subagents".to_owned()),
        };
        let (_lock, mut record) = self.register_cloud_record(
            agent_name,
            &cwd,
            &options,
            None,
            CloudRecord {
                launch: CloudLaunch::default(),
                agentterm: agentterm.display().to_string(),
                endpoint: Some(endpoint.clone()),
                create_exit: None,
                verified: None,
                create_note: None,
                terminal_identity: None,
                halted: false,
                attached_self: true,
            },
            Vec::new(),
        )?;
        record.session_agent = Some(CLOUD_HARNESS.to_owned());
        record.session_value = Some(session.clone());
        record.native_session = Some(NativeSession::new(CLOUD_HARNESS, &session, "observed"));
        self.save(&record)?;
        if let Err(error) = self.create_presentation_in(
            &mut record,
            &options,
            project_workspace.as_deref(),
            &default_label,
        ) {
            return self.cloud_launch_failed(
                &mut record,
                &options,
                format!("cannot create the Herdr tab: {error}"),
            );
        }
        let pane_id = record.pane_id.clone().expect("new tab has pane");
        let arguments = ["--ws-url".to_owned(), endpoint, "-s".to_owned(), session];
        match self.client.start_pane_command(
            &pane_id,
            &agentterm,
            &arguments,
            options.startup_timeout,
        ) {
            Ok(identity) => {
                record
                    .agentcloud
                    .as_mut()
                    .expect("cloud record")
                    .terminal_identity = Some(identity);
            }
            Err(error) => {
                return self.cloud_launch_failed(
                    &mut record,
                    &options,
                    format!("agentterm did not attach in pane {pane_id}: {error}"),
                );
            }
        }
        record.lifecycle = "running".to_owned();
        self.save(&record)?;
        self.status(agent_name)
    }

    /// The live Herdr workspace of the record `attach-self` made for the caller's own session,
    /// so agents that session starts appear beside it.
    pub(super) fn attached_self_workspace(&self) -> Option<String> {
        let session = self.ambient()?;
        let owner = self.identity_owner(CLOUD_HARNESS, session, None).ok()??;
        if !owner
            .agentcloud
            .as_ref()
            .is_some_and(|cloud| cloud.attached_self)
        {
            return None;
        }
        let workspace = owner.workspace_id?;
        self.client
            .panes()
            .ok()?
            .iter()
            .any(|pane| pane.workspace_id == workspace)
            .then_some(workspace)
    }

    /// Reserve the name and save a fresh `starting` record for an agentcloud session; the
    /// returned lock must be held until the launch finishes.
    fn register_cloud_record(
        &self,
        agent_name: &str,
        cwd: &Path,
        options: &StartOptions,
        reasoning_effort: Option<&str>,
        cloud: CloudRecord,
        arguments: Vec<String>,
    ) -> Result<(File, AgentRecord)> {
        let lock = self.lock(agent_name)?;
        let directory = self.directory(agent_name)?;
        if fs::symlink_metadata(&directory).is_ok() {
            return Err(fail(format!(
                "agent {agent_name:?} already registered; stop it before reusing the name"
            )));
        }
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&self.registry)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?;
        let record = AgentRecord {
            storage_directory: None,
            nested: None,
            adapter: CLOUD_HARNESS.to_owned(),
            mode: interactive_mode(),
            backend: herdr_adapter(),
            paused: false,
            runtime_home: None,
            pane_reported_by_agentctl: false,
            custom_process_identity: None,
            foreign_shell_identity: None,
            terminal_id: None,
            harness_identity: None,
            name_history: Vec::new(),
            anchor_rule: None,
            former_names: Vec::new(),
            native_session: None,
            profile: options.profile.clone(),
            slot: None,
            slot_project: None,
            slot_isolation: None,
            reasoning_effort: reasoning_effort.map(str::to_owned),
            environment_names: environment_names(&options.environment),
            agentcloud: Some(cloud),
            extra: BTreeMap::new(),
            name: agent_name.to_owned(),
            token: format!("{}-{}", now.as_nanos(), std::process::id()),
            harness: CLOUD_HARNESS.to_owned(),
            cwd: cwd.display().to_string(),
            created_at: now.as_secs_f64(),
            schema: 1,
            lifecycle: "starting".to_owned(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            session_agent: None,
            session_value: None,
            model: options.model.clone(),
            resume: None,
            arguments,
            startup_warning: None,
            effective_reasoning_effort: reasoning_effort.map(str::to_owned),
            error: None,
            goal: None,
            goal_delivery: None,
            goal_session_id: None,
            goal_command: None,
            goal_messages: BTreeMap::new(),
            goal_message_id: None,
        };
        self.save(&record)?;
        Ok((lock, record))
    }

    fn cloud_fleet<'f>(
        &self,
        fleet: &'f mut FleetCache,
        endpoint: &str,
    ) -> &'f std::result::Result<Vec<Value>, String> {
        fleet
            .entry(endpoint.to_owned())
            .or_insert_with(|| self.fetch_fleet(endpoint))
    }

    fn fetch_fleet(&self, endpoint: &str) -> std::result::Result<Vec<Value>, String> {
        resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin")
            .map_err(|error| error.to_string())
            .and_then(|executable| {
                run_tool(
                    &executable,
                    &with_endpoint(vec!["list".to_owned()], endpoint),
                    control_timeout(),
                    self.child_session(),
                )
                .map_err(|error| error.to_string())
            })
            .and_then(|run| {
                if run.code != Some(0) {
                    return Err(failure("list", &run));
                }
                serde_json::from_str::<Vec<Value>>(&run.stdout)
                    .map_err(|error| format!("agentcloudctl list printed invalid JSON: {error}"))
            })
    }

    /// The endpoint recorded for this session; an explicit conflicting `--agentcloud-url` is
    /// refused rather than silently ignored.
    fn cloud_endpoint(&self, record: &AgentRecord) -> Result<String> {
        let Some(recorded) = record.cloud()?.endpoint.clone() else {
            // Written before endpoints were recorded: resolve the same chain as a new start, and
            // validate it here so a migration can never save a value the loader rejects.
            let endpoint = self.default_endpoint();
            if !valid_endpoint(&endpoint) {
                return Err(fail(format!(
                    "agentcloud endpoint {endpoint:?} must be a ws:// or wss:// URL without whitespace; agent {:?} records none yet, so pass a valid --agentcloud-url",
                    record.name
                )));
            }
            return Ok(endpoint);
        };
        if let Some(explicit) = self
            .cloud_tools
            .endpoint
            .as_deref()
            .filter(|_| self.cloud_tools.endpoint_explicit)
        {
            if explicit != recorded {
                return Err(fail(format!(
                    "agent {:?} was created on agentcloud endpoint {recorded:?}; drop --agentcloud-url or pass that endpoint",
                    record.name
                )));
            }
        }
        Ok(recorded)
    }

    /// The session this process acts inside, for attribution decisions.
    fn ambient(&self) -> Option<&str> {
        self.cloud_tools
            .ambient_session
            .as_deref()
            .filter(|session| !session.is_empty())
    }

    /// The exact context handed to children: a known value, or `None` to inherit unchanged.
    fn child_session(&self) -> Option<&str> {
        self.cloud_tools.ambient_session.as_deref()
    }

    /// The endpoint for a new session: `--agentcloud-url`, else the environment (both supplied
    /// by the caller), else agentcloudctl's documented production default.
    fn default_endpoint(&self) -> String {
        self.cloud_tools
            .endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_AGENTCLOUD_ENDPOINT.to_owned())
    }

    /// Resolve the endpoint for a mutating command, saving it into a record that lacks one.
    fn persist_endpoint(&self, record: &mut AgentRecord) -> Result<String> {
        let endpoint = self.cloud_endpoint(record)?;
        let cloud = record.agentcloud.as_mut().expect("cloud record");
        if cloud.endpoint.is_none() {
            cloud.endpoint = Some(endpoint.clone());
            self.save(record)?;
        }
        Ok(endpoint)
    }

    /// `agentctl --registry REGISTRY ...`, so a printed command addresses this exact registry.
    fn agentctl_command(&self, arguments: &[&str]) -> String {
        let registry = self.registry.display().to_string();
        let mut words = vec!["agentctl", "--registry", registry.as_str()];
        words.extend_from_slice(arguments);
        shell_command(&words)
    }

    /// `agentcloudctl VERB --ws-url ENDPOINT ...` with the executable agentctl itself runs.
    fn agentcloudctl_command(&self, verb: &str, endpoint: &str, arguments: &[&str]) -> String {
        let executable = resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin")
            .unwrap_or_else(|_| self.cloud_tools.agentcloudctl.clone())
            .display()
            .to_string();
        let mut words = vec![executable.as_str(), verb, "--ws-url", endpoint];
        words.extend_from_slice(arguments);
        shell_command(&words)
    }

    /// The manual viewer command: the recorded agentterm, endpoint, and complete session ID.
    /// A recorded endpoint always wins, even over a conflicting explicit flag, because the
    /// session lives there; a record without one uses the validated resolution chain.
    fn viewer_command(&self, record: &AgentRecord, session: &str) -> Result<String> {
        let cloud = record.cloud()?;
        let endpoint = match cloud.endpoint.clone() {
            Some(recorded) => recorded,
            None => self.cloud_endpoint(record)?,
        };
        Ok(shell_command(&[
            cloud.agentterm.as_str(),
            "--ws-url",
            &endpoint,
            "-s",
            session,
        ]))
    }

    /// Apply the project workspace policy to automated input and terminal reads, exactly as
    /// for a local agent: the agent's tab must be live in the configured workspace.
    fn cloud_workspace_policy(&self, record: &AgentRecord) -> Result<()> {
        let Some(expected) = self.project_workspace()? else {
            return Ok(());
        };
        let agent_name = &record.name;
        let stop = self.agentctl_command(&["stop", agent_name]);
        let pane = record.pane_id.as_deref().and_then(|pane_id| {
            self.client
                .panes()
                .ok()?
                .into_iter()
                .find(|pane| pane.pane_id == pane_id)
        });
        let Some(pane) = pane else {
            return Err(fail(format!(
                "refusing agent {agent_name:?}: its terminal tab is gone, so its placement in the project workspace {expected:?} cannot be confirmed; inspect it with `{}` or retire it with `{stop}`",
                self.agentctl_command(&["status", agent_name])
            )));
        };
        let actual = self.client.workspace_label(&pane.workspace_id)?;
        if actual != expected {
            return Err(fail(format!(
                "refusing agent {agent_name:?}: its terminal tab is in workspace {actual:?}, but project configuration requires {expected:?}; move the tab into {expected:?} in Herdr, or retire the agent with `{stop}`"
            )));
        }
        Ok(())
    }

    fn cloud_session_of<'r>(&self, record: &'r AgentRecord) -> Result<&'r str> {
        record.cloud()?;
        record.session_value.as_deref().ok_or_else(|| {
            fail(format!(
                "agent {:?} has no recorded agentcloud session; inspect its launch error with `{}`, then archive it with `{}`",
                record.name,
                self.agentctl_command(&["status", &record.name]),
                self.agentctl_command(&["stop", &record.name]),
            ))
        })
    }

    fn cloud_terminal(&self, record: &AgentRecord) -> Value {
        let Some(pane_id) = record.pane_id.as_deref() else {
            return json!({"pane_id": null, "pane_present": false, "agentterm_attached": false});
        };
        let panes = match self.client.panes() {
            Ok(panes) => panes,
            Err(error) => {
                return json!({"pane_id": pane_id, "probe_error": error.to_string()});
            }
        };
        let Some(pane) = panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return json!({"pane_id": pane_id, "pane_present": false, "agentterm_attached": false});
        };
        let identity = record
            .agentcloud
            .as_ref()
            .and_then(|cloud| cloud.terminal_identity.as_ref());
        let mut terminal = json!({
            "pane_id": pane_id,
            "tab_id": pane.tab_id,
            "pane_present": true,
            "in_recorded_tab": Some(&pane.tab_id) == record.tab_id.as_ref(),
            "agentterm_attached": null,
        });
        if let Some(identity) = identity {
            match self.client.pane_runs_command(pane_id, identity) {
                Ok(attached) => terminal["agentterm_attached"] = json!(attached),
                Err(error) => terminal["probe_error"] = json!(error.to_string()),
            }
        }
        terminal
    }

    /// Status without attaching to the session: the fleet listing never wakes a dormant session.
    pub(super) fn cloud_status_record(
        &self,
        record: &AgentRecord,
        fleet: &mut FleetCache,
    ) -> Result<Value> {
        let mut result = json!(record);
        result["output"] = json!(self.directory(&record.name)?.join("output.json"));
        result["session_id"] = json!(record.session_value);
        result["terminal"] = self.cloud_terminal(record);
        result["cloud"] = Value::Null;
        let Some(session) = record.session_value.as_deref() else {
            result["agent_status"] = json!("unknown");
            result["probe_error"] = json!("no agentcloud session was recorded for this agent");
            return Ok(result);
        };
        let endpoint = self.cloud_endpoint(record)?;
        match self.cloud_fleet(fleet, &endpoint) {
            Err(error) => {
                result["agent_status"] = json!("unknown");
                result["probe_error"] = json!(error);
            }
            Ok(rows) => match rows.iter().find(|row| row["session_id"] == session) {
                None => {
                    result["agent_status"] = json!("unknown");
                    result["probe_error"] = json!(format!(
                        "session {session} is not in `agentcloudctl list`; the listing is a lagged projection, so retry shortly or run `{}`",
                        self.agentcloudctl_command("inspect", &endpoint, &["--session", session])
                    ));
                }
                Some(row) => {
                    let mut cloud = serde_json::Map::new();
                    for key in [
                        "title",
                        "harness",
                        "workspace",
                        "running",
                        "activity",
                        "attached_nodes",
                        "status",
                        "last_outcome",
                        "resolved_status",
                        "last_activity_unix_ms",
                    ] {
                        if let Some(value) = row.get(key) {
                            cloud.insert(key.to_owned(), value.clone());
                        }
                    }
                    // A queued first prompt on a still-provisioning node is `waiting`, not idle.
                    let activity = row["activity"]
                        .as_str()
                        .filter(|activity| *activity != "idle" && valid_activity(activity));
                    result["agent_status"] = json!(if row["running"] == true {
                        "working"
                    } else {
                        activity.unwrap_or("idle")
                    });
                    result["cloud"] = Value::Object(cloud);
                    result["probe_error"] = Value::Null;
                }
            },
        }
        Ok(result)
    }

    fn cloud_send_record(
        &self,
        record: &AgentRecord,
        text: &str,
        message_id: Option<&str>,
    ) -> Result<Value> {
        let session = self.cloud_session_of(record)?;
        if record.paused {
            return Err(fail(format!(
                "agent {:?} is paused for human input; run `{}` before sending automation input",
                record.name,
                self.agentctl_command(&["resume", &record.name])
            )));
        }
        if text.trim().is_empty() {
            return Err(fail("instruction must not be empty"));
        }
        if text.len() > MAX_CLOUD_TEXT_BYTES {
            return Err(fail(format!(
                "agentcloud input exceeds {MAX_CLOUD_TEXT_BYTES} bytes; agentcloudctl sends one argument, so split it into smaller messages"
            )));
        }
        let key = match message_id {
            Some(identifier) if super::message_id(identifier) => identifier.to_owned(),
            Some(_) => {
                return Err(fail(
                    "message id must be 1-255 ASCII letters, digits, dots, underscores, or hyphens and start with a letter or digit",
                ))
            }
            None => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| fail(error.to_string()))?;
                format!("agentctl-{}-{}-{}", record.name, now.as_nanos(), std::process::id())
            }
        };
        let caller = self
            .cloud_tools
            .caller_session
            .as_deref()
            .filter(|caller| *caller != session);
        if caller.is_some_and(|caller| !valid_session_id(caller)) {
            return Err(fail(
                "--from-session must be a complete lowercase agentcloud session UUID",
            ));
        }
        let goal = text.trim_start().starts_with("/goal");
        let endpoint = self.cloud_endpoint(record)?;
        let human_retry = || {
            self.agentctl_command(&[
                "--agentcloud-url",
                &endpoint,
                "--from-session",
                "",
                "send",
                &record.name,
                "--message-id",
                &key,
                "--file",
                "PROMPT_FILE",
            ])
        };
        if caller.is_none() {
            if let Some(ambient) = self.ambient().filter(|ambient| *ambient != session) {
                // agentcloudctl deliberately refuses human input to another session from inside
                // an agentcloud session. agentctl does not strip that context to get around it:
                // an agent session must not speak as the owner.
                return Err(fail(format!(
                    "refusing a human send to agent {:?} from inside agentcloud session {ambient}: agentcloudctl rejects human input to another session there, and agentctl will not bypass that attribution guard. Send it as the human from a terminal outside any agentcloud session with `{}`, where PROMPT_FILE holds the text; or send it attributed to this session with --from-session {ambient}",
                    record.name,
                    human_retry()
                )));
            }
        }
        let (verb, arguments, end_of_turn) = match caller {
            // Inside another session, plain `send` is refused: it would journal the text as the
            // human. The attributed peer message arrives on the target as an ordinary input.
            Some(caller) => (
                "send-message",
                vec![
                    "send-message".to_owned(),
                    "--session".to_owned(),
                    caller.to_owned(),
                    "--to".to_owned(),
                    session.to_owned(),
                    "--body".to_owned(),
                    text.to_owned(),
                    "--idempotency-key".to_owned(),
                    key.clone(),
                ],
                None,
            ),
            // A message-initial /goal is consumed at ingress; everything else waits for the turn
            // to end, matching agentctl's deliver-when-ready contract instead of steering it.
            None => {
                let mut arguments = vec![
                    "send".to_owned(),
                    "--session".to_owned(),
                    session.to_owned(),
                    "--text".to_owned(),
                    text.to_owned(),
                    "--idempotency-key".to_owned(),
                    key.clone(),
                ];
                if !goal {
                    arguments.push("--end-of-turn".to_owned());
                }
                ("send", arguments, Some(!goal))
            }
        };
        // agentcloudctl keeps sender-scoped keys for peer messages apart from human-input keys,
        // so a safe retry must repeat the same verb and sender as well as endpoint and key. An
        // empty --from-session selects human attribution (refused inside another session).
        let retry = format!(
            "Retry with `{}`, where PROMPT_FILE holds the same text; agentcloudctl deduplicates that idempotency key only for the same sender and verb",
            self.agentctl_command(&[
                "--agentcloud-url",
                &endpoint,
                "--from-session",
                caller.unwrap_or(""),
                "send",
                &record.name,
                "--message-id",
                &key,
                "--file",
                "PROMPT_FILE",
            ])
        );
        let executable = resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin")
            .map_err(|error| {
                client_error(format!(
                    "{error}; nothing was sent (message ID {key}). After fixing it, {}",
                    retry.replacen("Retry with", "retry with", 1)
                ))
            })?;
        // A timeout or capture failure can follow the service's durable acceptance, so every
        // uncertain outcome carries the same key and retry command as a nonzero exit.
        let arguments = with_endpoint(arguments, &endpoint);
        let run = run_tool(
            &executable,
            &arguments,
            control_timeout(),
            self.child_session(),
        )
        .map_err(|error| {
            client_error(format!(
                "{error}; the input may or may not have been delivered (message ID {key}). {retry}"
            ))
        })?;
        if run.code != Some(0) {
            return Err(client_error(format!(
                "{}; the input may or may not have been delivered (message ID {key}). {retry}",
                failure(verb, &run)
            )));
        }
        Ok(json!({
            "name": record.name,
            "session_id": session,
            "message_id": key,
            "outcome": "journaled",
            "delivery": verb,
            "from_session": caller,
            "end_of_turn": end_of_turn,
            "agentcloud": excerpt(&run.stdout),
        }))
    }

    /// Durably journal one input through `agentcloudctl send`.
    pub fn send_cloud(
        &self,
        agent_name: &str,
        text: &str,
        message_id: Option<&str>,
    ) -> Result<Value> {
        let _lock = self.lock(agent_name)?;
        let mut record = self.load(agent_name)?;
        record.cloud()?;
        self.cloud_workspace_policy(&record)?;
        if record.session_value.is_some() && !record.paused {
            self.persist_endpoint(&mut record)?;
        }
        self.cloud_send_record(&record, text, message_id)
    }

    /// Wait for the session's current or last run to settle through `agentcloudctl wait`.
    pub(super) fn cloud_wait(&self, agent_name: &str, timeout: Duration) -> Result<Value> {
        let (token, session, endpoint) = {
            let _lock = self.lock(agent_name)?;
            let record = self.load(agent_name)?;
            (
                record.token.clone(),
                self.cloud_session_of(&record)?.to_owned(),
                self.cloud_endpoint(&record)?,
            )
        };
        let milliseconds = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let executable = resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin")?;
        let run = run_tool(
            &executable,
            &with_endpoint(
                vec![
                    "wait".to_owned(),
                    "--session".to_owned(),
                    session.clone(),
                    "--until".to_owned(),
                    "settle".to_owned(),
                    "--timeout-ms".to_owned(),
                    milliseconds.to_string(),
                ],
                &endpoint,
            ),
            timeout.saturating_add(WAIT_MARGIN),
            self.child_session(),
        )?;
        let outcome = run
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned();
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        if record.token != token {
            return Err(fail(format!(
                "agent {agent_name:?} was replaced while waiting"
            )));
        }
        match run.code {
            Some(0) => {
                let mut status = self.cloud_status_record(&record, &mut FleetCache::new())?;
                status["cloud_outcome"] = json!(outcome);
                Ok(status)
            }
            Some(1) => Err(fail(format!(
                "agent {agent_name:?} settled without completing its run (agentcloudctl wait printed {outcome:?}, exit 1); read its reply with `{}`",
                self.agentctl_command(&["read", agent_name, "--output", "last"])
            ))),
            Some(2) => Err(fail(format!(
                "agent {agent_name:?}'s run was interrupted (agentcloudctl wait printed {outcome:?}, exit 2); send new input or inspect it with `{}`",
                self.agentctl_command(&["status", agent_name])
            ))),
            Some(3) => Err(fail(format!(
                "timed out or disconnected waiting for agent {agent_name:?} to settle; the outcome is unknown, so run `{}` again or inspect `{}`",
                self.agentctl_command(&["wait", agent_name]),
                self.agentctl_command(&["status", agent_name])
            ))),
            _ => Err(client_error(failure("wait", &run))),
        }
    }

    /// Read the durable final reply (`last`) or the attached terminal (`tail`/`all`).
    pub fn read_cloud(&self, agent_name: &str, lines: usize, output: &str) -> Result<String> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        record.cloud()?;
        let text = match output {
            "last" => {
                let session = self.cloud_session_of(&record)?;
                let executable =
                    resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin")?;
                let run = run_tool(
                    &executable,
                    &with_endpoint(
                        vec![
                            "output".to_owned(),
                            "--session".to_owned(),
                            session.to_owned(),
                        ],
                        &self.cloud_endpoint(&record)?,
                    ),
                    control_timeout(),
                    self.child_session(),
                )?;
                if run.code != Some(0) {
                    return Err(client_error(format!(
                        "{}; a session with no settled run has no final reply yet, so check `{}`",
                        failure("output", &run),
                        self.agentctl_command(&["status", agent_name])
                    )));
                }
                run.stdout
            }
            "tail" | "all" => {
                self.cloud_workspace_policy(&record)?;
                let pane_id = record
                    .pane_id
                    .as_deref()
                    .filter(|pane_id| {
                        self.client.panes().is_ok_and(|panes| {
                            panes.iter().any(|pane| {
                                pane.pane_id == *pane_id && Some(&pane.tab_id) == record.tab_id.as_ref()
                            })
                        })
                    })
                    .ok_or_else(|| {
                        fail(format!(
                            "agent {agent_name:?} has no live terminal tab; read its durable reply with `{}`",
                            self.agentctl_command(&["read", agent_name, "--output", "last"])
                        ))
                    })?;
                self.client.read(pane_id, "recent", Some(lines))?
            }
            _ => {
                return Err(fail(
                    "agentcloud agents support --output last, tail, or all; since_turn requires a headless transcript",
                ))
            }
        };
        self.snapshot(&record, &text)?;
        Ok(text)
    }

    /// Focus the agent's terminal tab after confirming it is still the recorded pane.
    pub(super) fn cloud_attach(&self, record: &AgentRecord) -> Result<Value> {
        let session = self.cloud_session_of(record).ok();
        let pane_id = record.pane_id.as_deref().filter(|pane_id| {
            self.client.panes().is_ok_and(|panes| {
                panes.iter().any(|pane| {
                    pane.pane_id == *pane_id && Some(&pane.tab_id) == record.tab_id.as_ref()
                })
            })
        });
        let Some(pane_id) = pane_id else {
            return Err(fail(match session {
                Some(session) => format!(
                    "agent {:?} has no live terminal tab; attach from any terminal with `{}`",
                    record.name,
                    self.viewer_command(record, session)?
                ),
                None => format!("agent {:?} has no live terminal tab", record.name),
            }));
        };
        self.client.focus_pane(pane_id)?;
        Ok(
            json!({"name": record.name, "pane_id": pane_id, "paused": record.paused, "session_id": session}),
        )
    }

    /// Revalidate the saved pane just before closing it. `Ok(false)` means it already vanished;
    /// a pane that moved or now runs something else is refused so no human's pane is closed.
    fn cloud_pane_still_owned(
        &self,
        record: &AgentRecord,
        saved: &Pane,
        halt_state: &str,
        retry: &str,
    ) -> Result<bool> {
        let agent_name = &record.name;
        let panes = self.client.panes().map_err(|error| {
            fail(format!(
                "cannot recheck the terminal pane before closing it for {agent_name:?}: {error}; {halt_state} and the record retained, so retry `{retry}`"
            ))
        })?;
        let Some(current) = panes.iter().find(|pane| pane.pane_id == saved.pane_id) else {
            return Ok(false);
        };
        if current.tab_id != saved.tab_id || current.workspace_id != saved.workspace_id {
            return Err(fail(format!(
                "refusing to close the terminal pane of {agent_name:?}: it moved to another tab or workspace during stop; {halt_state} and the record retained, so move the pane back or close it yourself, then retry `{retry}`"
            )));
        }
        let identity = record
            .agentcloud
            .as_ref()
            .and_then(|cloud| cloud.terminal_identity.as_ref());
        let viewer = match identity {
            Some(identity) => self.client.pane_runs_command(&saved.pane_id, identity)?,
            None => false,
        };
        if !viewer && !self.client.pane_is_idle_shell(&saved.pane_id)? {
            return Err(fail(format!(
                "refusing to close the terminal pane of {agent_name:?}: it runs something other than the recorded agentterm viewer or its idle shell; {halt_state} and the record retained, so finish or close that process yourself, then retry `{retry}`"
            )));
        }
        Ok(true)
    }

    /// Halt and archive the session, close the recorded terminal pane, and archive the record.
    pub(super) fn cloud_stop(
        &self,
        mut record: AgentRecord,
        options: &StopOptions,
    ) -> Result<Value> {
        let agent_name = record.name.clone();
        if options.recover_legacy_adoption {
            return Err(fail(
                "--recover-legacy-adoption applies only to adopted Herdr agents",
            ));
        }
        let panes = self.client.panes().map_err(|error| {
            fail(format!(
                "cannot inspect Herdr before stopping {agent_name:?}: {error}; nothing was halted, closed, or archived"
            ))
        })?;
        let owned = record
            .pane_id
            .as_ref()
            .and_then(|pane_id| panes.iter().find(|pane| &pane.pane_id == pane_id))
            .cloned();
        if let Some(pane) = &owned {
            if Some(&pane.tab_id) != record.tab_id.as_ref()
                || Some(&pane.workspace_id) != record.workspace_id.as_ref()
            {
                return Err(fail(format!(
                    "refusing to stop {agent_name:?}: its terminal pane moved to another tab or workspace; move it back or close it yourself, then retry"
                )));
            }
        }
        let session = record.session_value.clone();
        let mut retry_arguments = vec!["stop", agent_name.as_str()];
        if options.skip_cloud_halt {
            retry_arguments.push("--skip-cloud-halt");
        }
        let retry = self.agentctl_command(&retry_arguments);
        // --skip-cloud-halt never contacts agentcloud, so it needs no endpoint and always works.
        // An attach-self record is the caller's own session: halting it would end the caller.
        let attached_self = record
            .agentcloud
            .as_ref()
            .is_some_and(|cloud| cloud.attached_self);
        let contact = session.is_some() && !options.skip_cloud_halt && !attached_self;
        let endpoint = if contact {
            Some(self.persist_endpoint(&mut record)?)
        } else {
            None
        };
        let executable = contact
            .then(|| resolve_tool(&self.cloud_tools.agentcloudctl, "--agentcloudctl-bin"))
            .transpose()?;
        if let (Some(session), Some(executable), Some(endpoint)) = (
            session.as_deref(),
            executable.as_deref(),
            endpoint.as_deref(),
        ) {
            let run = run_tool(
                executable,
                &with_endpoint(
                    vec![
                        "halt".to_owned(),
                        "--session".to_owned(),
                        session.to_owned(),
                        "--reason".to_owned(),
                        format!("agentctl stop {agent_name}"),
                    ],
                    endpoint,
                ),
                control_timeout(),
                self.child_session(),
            )?;
            if run.code != Some(0) {
                return Err(client_error(format!(
                    "{}; nothing was closed or archived. Retry `{retry}` (halt is idempotent), or `{}` if the session no longer exists or was retired elsewhere",
                    failure("halt", &run),
                    self.agentctl_command(&["stop", &agent_name, "--skip-cloud-halt"])
                )));
            }
            if let Some(cloud) = record.agentcloud.as_mut() {
                cloud.halted = true;
            }
            self.save(&record)?;
        }
        let (archive, destination) = self.archive_destination(&record)?;
        let mut snapshot_error = None;
        let mut pane_closed = false;
        if let Some(pane) = &owned {
            match self.bounded_terminal_text(&pane.pane_id) {
                Ok(text) => self.snapshot(&record, &text)?,
                Err(error) => snapshot_error = Some(error.to_string()),
            }
            // The halt and the capture above can take minutes. Prove again, immediately before
            // closing, that the saved pane is still ours: same tab and workspace, running the
            // recorded viewer or back at its idle shell.
            let halt_state = if session.is_none() {
                "no agentcloud session was recorded"
            } else if attached_self {
                "the session was NOT halted (attach-self registered the caller's own session)"
            } else if executable.is_some() {
                "the session is halted"
            } else {
                "the session was NOT halted (--skip-cloud-halt left it untouched)"
            };
            if self.cloud_pane_still_owned(&record, pane, halt_state, &retry)? {
                record.lifecycle = "stopping".to_owned();
                self.save(&record)?;
                self.client.close_pane(&pane.pane_id)?;
                pane_closed = true;
            }
        }
        let mut session_archived = false;
        let mut archive_error = None;
        if let (Some(session), Some(executable), Some(endpoint)) = (
            session.as_deref(),
            executable.as_deref(),
            endpoint.as_deref(),
        ) {
            match run_tool(
                executable,
                &with_endpoint(
                    vec![
                        "archive".to_owned(),
                        "--session".to_owned(),
                        session.to_owned(),
                    ],
                    endpoint,
                ),
                control_timeout(),
                self.child_session(),
            ) {
                Ok(run) if run.code == Some(0) => session_archived = true,
                Ok(run) => archive_error = Some(failure("archive", &run)),
                Err(error) => archive_error = Some(error.to_string()),
            }
        }
        record.lifecycle = "stopped".to_owned();
        self.save(&record)?;
        fs::rename(self.directory(&agent_name)?, &destination)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&archive)?;
        agent::sync_directory(&self.registry)?;
        let tab_closed = if !pane_closed {
            Some(false)
        } else {
            self.client.panes().ok().map(|panes| {
                panes
                    .iter()
                    .all(|pane| Some(&pane.tab_id) != record.tab_id.as_ref())
            })
        };
        let halted = executable.is_some();
        Ok(json!({
            "name": agent_name,
            "archive": destination,
            "session_id": session,
            "session_halted": halted,
            "session_archived": session_archived,
            "session_archive_error": archive_error,
            "pane_closed": pane_closed,
            "tab_closed": tab_closed,
            "snapshot_error": snapshot_error,
            "node_lease": match (session.as_deref(), endpoint.as_deref()) {
                (None, _) => "no agentcloud session was recorded".to_owned(),
                (Some(session), Some(endpoint)) => format!(
                    "stop does not release the node reservation; the orchestrator releases a provisioned node's reservation on its own schedule after the halted session goes idle, which can be long after stop returns. Check the reservation line of `{}`",
                    self.agentcloudctl_command("inspect", endpoint, &["--session", session])
                ),
                (Some(_), None) if attached_self => "attach-self registered the caller's own session, so stop left it untouched, including any node reservation".to_owned(),
                (Some(_), None) => "--skip-cloud-halt left the agentcloud session untouched, including any node reservation".to_owned(),
            },
        }))
    }
}

fn push_value(arguments: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        arguments.extend([flag.to_owned(), value.to_owned()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{AdapterError, Result as AdapterResult};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const SESSION: &str = "0b7c9a2e-1f3d-4c5b-8a69-2d4e6f8a0b1c";
    const ENDPOINT: &str = "wss://orchestrator.test/ws/chat";

    /// Herdr stand-in: one tab per start, a pinned terminal identity, and recorded closes.
    #[derive(Default)]
    struct Herdr {
        panes: Mutex<Vec<Pane>>,
        environments: Mutex<Vec<Vec<String>>>,
        commands: Mutex<Vec<(String, PathBuf, Vec<String>)>>,
        closed: Mutex<Vec<String>>,
        focused: Mutex<Vec<String>>,
        fail_command: AtomicBool,
        detached: AtomicBool,
        /// Label reported for every workspace; `None` means `subagents`.
        label: Mutex<Option<String>>,
        /// Label to switch to while the viewer starts, modelling the tab leaving its workspace.
        relabel_on_command: Mutex<Option<String>>,
        labels: Mutex<Vec<String>>,
        /// Move the recorded pane to a human's tab while its output is being captured.
        move_on_read: AtomicBool,
        /// The pane runs some foreground process other than the recorded viewer or a shell.
        busy_foreign: AtomicBool,
    }

    fn identity() -> CustomProcessIdentity {
        CustomProcessIdentity {
            version: 1,
            boot_id: "11111111-2222-3333-4444-555555555555".to_owned(),
            pid: 4343,
            starttime_ticks: 77,
            executable_device: 5,
            executable_inode: 9,
        }
    }

    fn unused<T>() -> AdapterResult<T> {
        Err(AdapterError::unavailable(
            "agentcloud agents never use Herdr harness control",
        ))
    }

    impl AgentApi for Herdr {
        fn panes(&self) -> AdapterResult<Vec<Pane>> {
            Ok(self.panes.lock().unwrap().clone())
        }
        fn pane_info(&self, _: &str) -> AdapterResult<AgentPaneInfo> {
            unused()
        }
        fn workspace_label(&self, _: &str) -> AdapterResult<String> {
            Ok(self
                .label
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "subagents".to_owned()))
        }
        fn run(&self, _: &str, _: &str) -> AdapterResult<()> {
            unused()
        }
        fn wait_agent_status(&self, _: &str, _: &str, _: u64) -> AdapterResult<()> {
            unused()
        }
        fn read(&self, _: &str, _: &str, _: Option<usize>) -> AdapterResult<String> {
            if self.move_on_read.swap(false, Ordering::Relaxed) {
                for pane in self.panes.lock().unwrap().iter_mut() {
                    pane.tab_id = "human-tab".to_owned();
                }
            }
            Ok("agentterm screen".to_owned())
        }
    }

    impl ManagedApi for Herdr {
        fn workspace_id_for_label(&self, label: &str) -> AdapterResult<Option<String>> {
            self.labels.lock().unwrap().push(label.to_owned());
            Ok(Some("workspace".to_owned()))
        }
        fn create_workspace(
            &self,
            _: &str,
            _: &str,
            _: &[String],
        ) -> AdapterResult<(String, String, String)> {
            unused()
        }
        fn create_tab(&self, _: &str, _: &str, _: &str, _: &[String]) -> AdapterResult<String> {
            unused()
        }
        fn create_tab_with_pane(
            &self,
            workspace: &str,
            _: &str,
            _: &str,
            environment: &[String],
        ) -> AdapterResult<(String, String)> {
            self.environments.lock().unwrap().push(environment.to_vec());
            self.panes.lock().unwrap().push(Pane {
                pane_id: "cloud-pane".to_owned(),
                tab_id: "cloud-tab".to_owned(),
                workspace_id: workspace.to_owned(),
            });
            Ok(("cloud-tab".to_owned(), "cloud-pane".to_owned()))
        }
        fn close_pane(&self, pane: &str) -> AdapterResult<()> {
            self.closed.lock().unwrap().push(pane.to_owned());
            self.panes
                .lock()
                .unwrap()
                .retain(|entry| entry.pane_id != pane);
            Ok(())
        }
        fn focus_pane(&self, pane: &str) -> AdapterResult<()> {
            self.focused.lock().unwrap().push(pane.to_owned());
            Ok(())
        }
        fn rename_tab(&self, _: &str, _: &str) -> AdapterResult<()> {
            Ok(())
        }
        fn start_agent(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[String],
            _: Duration,
        ) -> AdapterResult<()> {
            unused()
        }
        fn start_pane_command(
            &self,
            pane: &str,
            executable: &Path,
            arguments: &[String],
            _: Duration,
        ) -> AdapterResult<CustomProcessIdentity> {
            self.commands.lock().unwrap().push((
                pane.to_owned(),
                executable.to_owned(),
                arguments.to_vec(),
            ));
            if self.fail_command.load(Ordering::Relaxed) {
                return Err(AdapterError::unavailable(
                    "agentterm was not the foreground process",
                ));
            }
            if let Some(label) = self.relabel_on_command.lock().unwrap().take() {
                *self.label.lock().unwrap() = Some(label);
            }
            Ok(identity())
        }
        fn pane_runs_command(
            &self,
            _: &str,
            recorded: &CustomProcessIdentity,
        ) -> AdapterResult<bool> {
            Ok(*recorded == identity() && !self.detached.load(Ordering::Relaxed))
        }
        fn pane_is_idle_shell(&self, _: &str) -> AdapterResult<bool> {
            Ok(!self.busy_foreign.load(Ordering::Relaxed))
        }
        fn agent_pane(&self, _: &str) -> AdapterResult<String> {
            unused()
        }
        fn report_agent_session(&self, _: &str, _: &str, _: &str, _: &str) -> AdapterResult<()> {
            unused()
        }
        fn send_keys(&self, _: &str, _: &str) -> AdapterResult<()> {
            unused()
        }
        fn close_tab(&self, _: &str) -> AdapterResult<()> {
            panic!("agentcloud stop must close exactly the recorded pane")
        }
    }

    /// A fake `agentcloudctl` that records each argv and replays per-verb canned responses.
    const FAKE_AGENTCLOUDCTL: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
verb=$1
mkdir -p "$dir/calls"
n=$(ls "$dir/calls" | wc -l | tr -d ' ')
for arg in "$@"; do printf '%s\0' "$arg"; done > "$dir/calls/$(printf %04d "$n")-$verb"
mkdir -p "$dir/env"
printf '%s' "${AGENTCLOUD_SESSION_ID-<unset>}" > "$dir/env/$(printf %04d "$n")-$verb"
# agentcloudctl's own guard: inside a session, human `send` may only target that session.
if [ "$verb" = send ] && [ -n "${AGENTCLOUD_SESSION_ID:-}" ]; then
  target=""; previous=""
  for arg in "$@"; do [ "$previous" = --session ] && target=$arg; previous=$arg; done
  if [ "$target" != "$AGENTCLOUD_SESSION_ID" ]; then
    echo "agentcloudctl: refusing this send: it runs inside session $AGENTCLOUD_SESSION_ID and targets session $target" >&2
    exit 1
  fi
fi
[ -f "$dir/responses/$verb.stdout" ] && cat "$dir/responses/$verb.stdout"
[ -f "$dir/responses/$verb.sleep" ] && sleep "$(cat "$dir/responses/$verb.sleep")"
[ -f "$dir/responses/$verb.stderr" ] && cat "$dir/responses/$verb.stderr" >&2
[ -f "$dir/responses/$verb.exit" ] && exit "$(cat "$dir/responses/$verb.exit")"
exit 0
"#;

    /// Every agentcloudctl call in these tests is a real subprocess admitted to the process-wide
    /// plugin cleanup registry (32 slots), which the plugin, client, and chat tests also use.
    /// Running the cloud tests one at a time keeps their share to one or two admissions, so a
    /// default parallel `cargo test` cannot saturate it; the product limit is unchanged.
    static SUBPROCESS_TESTS: Mutex<()> = Mutex::new(());

    thread_local! {
        static SUBPROCESS_TURNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// One test's turn; re-entrant on its thread, since some tests hold two fixtures.
    struct SubprocessTurn {
        _guard: Option<std::sync::MutexGuard<'static, ()>>,
    }

    impl SubprocessTurn {
        fn take() -> Self {
            let first = SUBPROCESS_TURNS.with(|turns| {
                turns.set(turns.get() + 1);
                turns.get() == 1
            });
            Self {
                _guard: first.then(|| {
                    SUBPROCESS_TESTS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                }),
            }
        }
    }

    impl Drop for SubprocessTurn {
        fn drop(&mut self) {
            SUBPROCESS_TURNS.with(|turns| turns.set(turns.get() - 1));
        }
    }

    struct Fixture {
        root: PathBuf,
        herdr: Herdr,
        /// Declared last so the turn is released only after everything else is dropped.
        _turn: SubprocessTurn,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new() -> Self {
            let turn = SubprocessTurn::take();
            let root = std::env::temp_dir().join(format!(
                "agentctl-cloud-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("tools/responses")).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            for (name, body) in [
                ("agentcloudctl", FAKE_AGENTCLOUDCTL),
                ("agentterm", "#!/bin/sh\nexit 0\n"),
            ] {
                let path = root.join("tools").join(name);
                fs::write(&path, body).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let fixture = Self {
                root,
                herdr: Herdr::default(),
                _turn: turn,
            };
            fixture.respond("create", Some(&format!("{SESSION}\n")), None, None);
            fixture.respond("send", Some("42\n"), None, None);
            fixture
        }

        fn respond(
            &self,
            verb: &str,
            stdout: Option<&str>,
            stderr: Option<&str>,
            exit: Option<i32>,
        ) {
            let responses = self.root.join("tools/responses");
            for (suffix, value) in [
                ("stdout", stdout.map(str::to_owned)),
                ("stderr", stderr.map(str::to_owned)),
                ("exit", exit.map(|code| code.to_string())),
            ] {
                let path = responses.join(format!("{verb}.{suffix}"));
                match value {
                    Some(value) => fs::write(path, value).unwrap(),
                    None => {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }

        fn tools(&self) -> CloudTools {
            CloudTools {
                agentcloudctl: self.root.join("tools/agentcloudctl"),
                agentterm: self.root.join("tools/agentterm"),
                caller_session: None,
                endpoint: Some(ENDPOINT.to_owned()),
                endpoint_explicit: false,
                // Explicitly outside any session, whatever this test process inherited.
                ambient_session: Some(String::new()),
                hostname: Some("node-a.example.test".to_owned()),
            }
        }

        fn manager(&self) -> ManagedAgents<'_, Herdr> {
            ManagedAgents::new(&self.herdr, &self.root.join("registry"))
                .unwrap()
                .with_inherited_workspace(None)
                .with_cloud_tools(self.tools())
        }

        fn options(&self, brief: Option<&str>) -> StartOptions {
            StartOptions {
                workspace_id: Some("workspace".to_owned()),
                harness: CLOUD_HARNESS.to_owned(),
                model: Some("claude-opus".to_owned()),
                brief: brief.map(str::to_owned),
                environment: vec!["TAB_ONLY=literal $(value)".to_owned()],
                cloud: Some(CloudLaunch {
                    harness: Some("claude-code".to_owned()),
                    provision: true,
                    envspec: Some("monorepo".to_owned()),
                    purpose: Some("sub-worker checkout".to_owned()),
                    ..CloudLaunch::default()
                }),
                ..StartOptions::default()
            }
        }

        fn start(&self, brief: Option<&str>) -> Result<Value> {
            self.manager().start_with_reasoning_effort(
                "sub-worker",
                &self.root,
                "high",
                self.options(brief),
            )
        }

        /// Every recorded agentcloudctl argv, in invocation order.
        fn calls(&self) -> Vec<Vec<String>> {
            let directory = self.root.join("tools/calls");
            let Ok(entries) = fs::read_dir(&directory) else {
                return Vec::new();
            };
            let mut names = entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            names
                .iter()
                .map(|name| {
                    fs::read(directory.join(name))
                        .unwrap()
                        .split(|byte| *byte == 0)
                        .filter(|value| !value.is_empty())
                        .map(|value| String::from_utf8(value.to_vec()).unwrap())
                        .collect()
                })
                .collect()
        }

        /// The `AGENTCLOUD_SESSION_ID` each agentcloudctl child received, in invocation order.
        fn child_sessions(&self) -> Vec<String> {
            let directory = self.root.join("tools/env");
            let mut names = fs::read_dir(&directory)
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            names.sort();
            names
                .iter()
                .map(|name| fs::read_to_string(directory.join(name)).unwrap())
                .collect()
        }

        fn verbs(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .map(|call| call[0].clone())
                .collect()
        }

        fn record(&self) -> Value {
            agent::read_private_json(&self.root.join("registry/sub-worker/agent.json")).unwrap()
        }

        fn list_row(&self, running: bool) {
            self.respond(
                "list",
                Some(
                    &json!([
                        {"session_id": "someone-else", "running": true},
                        {"session_id": SESSION, "running": running, "attached_nodes": ["node-a"], "title": "sub-worker", "tokens": {"input_durable": 1}}
                    ])
                    .to_string(),
                ),
                None,
                None,
            );
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn start_creates_the_session_then_attaches_agentterm_in_a_new_tab() {
        let fixture = Fixture::new();
        fixture.list_row(true);
        let status = fixture
            .start(Some("Run hostname, then reply DONE."))
            .unwrap();

        let calls = fixture.calls();
        assert_eq!(
            calls[0],
            strings(&[
                "create",
                "--ws-url",
                ENDPOINT,
                "--title",
                "sub-worker",
                "--harness",
                "claude-code",
                "--model",
                "claude-opus",
                "--effort",
                "high",
                "--provision",
                "--envspec",
                "monorepo",
                "--purpose",
                "sub-worker checkout",
                "--prompt",
                "Run hostname, then reply DONE.",
            ])
        );
        assert_eq!(calls[1], ["list", "--ws-url", ENDPOINT]);
        let commands = fixture.herdr.commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "cloud-pane");
        assert_eq!(
            commands[0].1,
            fs::canonicalize(fixture.root.join("tools/agentterm")).unwrap()
        );
        assert_eq!(
            commands[0].2,
            strings(&["--ws-url", ENDPOINT, "-s", SESSION])
        );
        assert_eq!(
            *fixture.herdr.environments.lock().unwrap(),
            [strings(&["TAB_ONLY=literal $(value)"])]
        );

        assert_eq!(status["lifecycle"], "running");
        assert_eq!(status["adapter"], "agentcloud");
        assert_eq!(status["session_id"], SESSION);
        assert_eq!(
            status["native_session"],
            json!({
                "schema": "agentctl-native-session/v1",
                "agent": CLOUD_HARNESS,
                "value": SESSION,
                "source": "observed",
            })
        );
        assert_eq!(status["reasoning_effort"], "high");
        assert_eq!(status["environment_names"], json!(["TAB_ONLY"]));
        assert_eq!(status["agent_status"], "working");
        assert_eq!(status["cloud"]["attached_nodes"], json!(["node-a"]));
        assert!(status["cloud"].get("tokens").is_none());
        assert_eq!(status["terminal"]["agentterm_attached"], true);
        assert_eq!(status["probe_error"], Value::Null);
        assert!(!status.to_string().contains("literal $(value)"));

        let record = fixture.record();
        assert_eq!(record["session_value"], SESSION);
        assert_eq!(record["agentcloud"]["verified"], true);
        assert_eq!(record["agentcloud"]["terminal_identity"]["pid"], 4343);
        assert!(
            !record["arguments"].to_string().contains("reply DONE"),
            "the brief is input, not launch policy"
        );
    }

    #[test]
    fn create_failure_reports_stderr_and_never_opens_a_tab() {
        let fixture = Fixture::new();
        fixture.respond(
            "create",
            None,
            Some("envspec refused: no capacity\n"),
            Some(1),
        );
        let error = fixture.start(Some("task")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("agentcloudctl create exited 1: envspec refused: no capacity"));
        assert!(message.contains("no agentcloud session was recorded"));
        assert_eq!(error.exit_code(), 69);
        assert!(fixture.herdr.panes.lock().unwrap().is_empty());
        let record = fixture.record();
        assert_eq!(record["lifecycle"], "launch_failed");
        assert_eq!(record["session_value"], Value::Null);

        let stopped = fixture.manager().stop("sub-worker").unwrap();
        assert_eq!(stopped["session_halted"], false);
        assert_eq!(
            fixture.verbs(),
            ["create"],
            "nothing to halt without a session"
        );
    }

    #[test]
    fn invocation_failures_126_and_127_are_failures_not_success() {
        for code in [126, 127] {
            let fixture = Fixture::new();
            fixture.respond("create", Some(&format!("{SESSION}\n")), None, Some(code));
            let message = fixture.start(None).unwrap_err().to_string();
            assert!(
                message.contains(&format!("(exit {code})")),
                "exit {code} was not named: {message}"
            );
            assert_eq!(fixture.record()["lifecycle"], "launch_failed");
            // A printed ID after a failed exit is kept so stop can still halt a possible session.
            assert_eq!(fixture.record()["session_value"], SESSION);
        }
    }

    #[test]
    fn unrecognizable_create_output_is_refused_without_a_tab() {
        let fixture = Fixture::new();
        fixture.respond("create", Some("created!\nsee the inbox\n"), None, None);
        let message = fixture.start(None).unwrap_err().to_string();
        assert!(message.contains("no recognizable session ID"));
        assert!(message.contains("agentcloudctl list"));
        assert!(fixture.herdr.panes.lock().unwrap().is_empty());
    }

    #[test]
    fn unverified_create_keeps_the_session_attached_but_fails_loudly() {
        let fixture = Fixture::new();
        fixture.respond(
            "create",
            Some(&format!("{SESSION}\n")),
            Some("node clause did not land on the attach read\n"),
            Some(9),
        );
        let message = fixture.start(Some("task")).unwrap_err().to_string();
        assert!(message.contains("UNVERIFIED"));
        assert!(message.contains("brief was NOT queued"));
        assert!(message.contains("Do not start it again"));
        let record = fixture.record();
        assert_eq!(record["lifecycle"], "running");
        assert_eq!(record["agentcloud"]["verified"], false);
        assert_eq!(fixture.herdr.commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn terminal_attach_failure_keeps_the_live_session_stoppable() {
        let fixture = Fixture::new();
        fixture.herdr.fail_command.store(true, Ordering::Relaxed);
        let message = fixture.start(None).unwrap_err().to_string();
        assert!(message.contains("agentterm did not attach"));
        assert!(message.contains(&format!("agentterm --ws-url {ENDPOINT} -s {SESSION}")));
        assert_eq!(fixture.record()["lifecycle"], "launch_failed");

        let stopped = fixture.manager().stop("sub-worker").unwrap();
        assert_eq!(stopped["session_halted"], true);
        assert_eq!(stopped["pane_closed"], true);
        assert_eq!(fixture.verbs(), ["create", "halt", "archive"]);
    }

    #[test]
    fn goal_briefs_are_sent_after_create_instead_of_as_a_prompt() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(Some("/goal finish the migration")).unwrap();
        let calls = fixture.calls();
        assert!(!calls[0].contains(&"--prompt".to_owned()));
        assert_eq!(calls[1][0], "send");
        assert_eq!(calls[1][6], "/goal finish the migration");
        assert!(!calls[1].contains(&"--end-of-turn".to_owned()));
    }

    #[test]
    fn missing_tools_are_refused_before_registry_state() {
        let fixture = Fixture::new();
        let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                agentcloudctl: fixture.root.join("tools/absent-agentcloudctl"),
                ..fixture.tools()
            });
        let message = manager
            .start("sub-worker", &fixture.root, fixture.options(None))
            .unwrap_err()
            .to_string();
        assert!(message.contains("--agentcloudctl-bin"));
        assert!(!fixture.root.join("registry/sub-worker").exists());
    }

    #[test]
    fn launch_settings_refuse_contradictions_and_reserved_arguments() {
        let fixture = Fixture::new();
        for (launch, expected) in [
            (
                CloudLaunch {
                    envspec: Some("monorepo".to_owned()),
                    ..CloudLaunch::default()
                },
                "without provision",
            ),
            (
                CloudLaunch {
                    provision: true,
                    node_id: Some("node-a".to_owned()),
                    ..CloudLaunch::default()
                },
                "choose one",
            ),
            (
                CloudLaunch {
                    workspace: Some("relative/dir".to_owned()),
                    ..CloudLaunch::default()
                },
                "absolute path",
            ),
            (
                CloudLaunch {
                    harness: Some("claude".to_owned()),
                    ..CloudLaunch::default()
                },
                "claude-code",
            ),
        ] {
            let mut options = fixture.options(None);
            options.cloud = Some(launch);
            let message = fixture
                .manager()
                .start("sub-worker", &fixture.root, options)
                .unwrap_err()
                .to_string();
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
        for (argv, expected) in [
            (strings(&["--prompt=hi"]), "--prompt"),
            (strings(&["--ws-url=ws://elsewhere"]), "--ws-url"),
            (strings(&["--skill", "builder"]), "--option=value"),
            (strings(&["-p"]), "long option"),
        ] {
            let mut options = fixture.options(None);
            options.harness_args = argv;
            let message = fixture
                .manager()
                .start("sub-worker", &fixture.root, options)
                .unwrap_err()
                .to_string();
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
        let mut options = fixture.options(None);
        options.resume = Some(SESSION.to_owned());
        assert!(fixture
            .manager()
            .start("sub-worker", &fixture.root, options)
            .unwrap_err()
            .to_string()
            .contains("agentterm --ws-url ENDPOINT -s SESSION_ID"));
        assert!(fixture.calls().is_empty());
        assert!(!fixture.root.join("registry/sub-worker").exists());

        let mut options = fixture.options(None);
        options.harness_args = strings(&["--skill=builder", "--narration"]);
        fixture.list_row(false);
        fixture
            .manager()
            .start("sub-worker", &fixture.root, options)
            .unwrap();
        let create = &fixture.calls()[0];
        assert_eq!(
            &create[create.len() - 2..],
            ["--skill=builder", "--narration"]
        );
    }

    #[test]
    fn send_journals_after_the_turn_with_an_idempotency_key() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let sent = fixture
            .manager()
            .send_cloud("sub-worker", "follow-up", Some("review-1"))
            .unwrap();
        assert_eq!(sent["outcome"], "journaled");
        assert_eq!(sent["message_id"], "review-1");
        assert_eq!(sent["end_of_turn"], true);
        assert_eq!(sent["delivery"], "send");
        let send = fixture.calls().pop().unwrap();
        assert_eq!(
            send,
            strings(&[
                "send",
                "--ws-url",
                ENDPOINT,
                "--session",
                SESSION,
                "--text",
                "follow-up",
                "--idempotency-key",
                "review-1",
                "--end-of-turn",
            ])
        );

        let generated = fixture
            .manager()
            .send_cloud("sub-worker", "another", None)
            .unwrap();
        assert!(generated["message_id"]
            .as_str()
            .unwrap()
            .starts_with("agentctl-sub-worker-"));

        fixture.respond("send", None, Some("transport lost\n"), Some(3));
        let message = fixture
            .manager()
            .send_cloud("sub-worker", "retry me", Some("retry-1"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("exited 3: transport lost"));
        assert!(message.contains("--message-id retry-1"));

        fixture.manager().pause("sub-worker", true).unwrap();
        let before = fixture.calls().len();
        assert!(fixture
            .manager()
            .send_cloud("sub-worker", "blocked", None)
            .unwrap_err()
            .to_string()
            .contains("paused"));
        assert_eq!(
            fixture.calls().len(),
            before,
            "a paused agent is not contacted"
        );
    }

    #[test]
    fn send_from_inside_another_session_uses_the_attributed_message() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("send-message", Some("msg-7\n"), None, None);
        let coordinator = "c0ffee00-0000-4000-8000-000000000001";
        let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                caller_session: Some(coordinator.to_owned()),
                ..fixture.tools()
            });
        let sent = manager
            .send_cloud("sub-worker", "follow-up", Some("review-2"))
            .unwrap();
        assert_eq!(sent["delivery"], "send-message");
        assert_eq!(sent["from_session"], coordinator);
        assert_eq!(sent["end_of_turn"], Value::Null);
        assert_eq!(
            fixture.calls().pop().unwrap(),
            strings(&[
                "send-message",
                "--ws-url",
                ENDPOINT,
                "--session",
                coordinator,
                "--to",
                SESSION,
                "--body",
                "follow-up",
                "--idempotency-key",
                "review-2",
            ])
        );

        // A worker addressing its own session keeps the plain send.
        let own = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                caller_session: Some(SESSION.to_owned()),
                ..fixture.tools()
            });
        assert_eq!(
            own.send_cloud("sub-worker", "self", None).unwrap()["delivery"],
            "send"
        );

        fixture.respond("send-message", None, Some("not same owner\n"), Some(1));
        let message = manager
            .send_cloud("sub-worker", "again", Some("review-3"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("agentcloudctl send-message exited 1: not same owner"));
        assert!(message.contains("--message-id review-3"));
    }

    #[test]
    fn status_reports_missing_listing_rows_and_detached_terminals_truthfully() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("list", Some("[]"), None, None);
        fixture.herdr.detached.store(true, Ordering::Relaxed);
        let status = fixture.manager().status("sub-worker").unwrap();
        assert_eq!(status["agent_status"], "unknown");
        assert!(status["probe_error"]
            .as_str()
            .unwrap()
            .contains("lagged projection"));
        assert_eq!(status["terminal"]["agentterm_attached"], false);

        fixture.respond("list", None, Some("unauthenticated\n"), Some(1));
        let status = fixture.manager().status("sub-worker").unwrap();
        assert!(status["probe_error"]
            .as_str()
            .unwrap()
            .contains("agentcloudctl list exited 1: unauthenticated"));

        fixture.list_row(false);
        let listed = fixture.manager().list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["agent_status"], "idle");

        fixture.respond(
            "list",
            Some(
                &json!([{"session_id": SESSION, "running": false, "activity": "waiting"}])
                    .to_string(),
            ),
            None,
            None,
        );
        let status = fixture.manager().status("sub-worker").unwrap();
        assert_eq!(status["agent_status"], "waiting");
    }

    #[test]
    fn wait_maps_settle_exit_codes_to_readiness_and_failures() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("wait", Some("completed\n"), None, Some(0));
        let status = fixture
            .manager()
            .wait("sub-worker", Duration::from_secs(90))
            .unwrap();
        assert_eq!(status["cloud_outcome"], "completed");
        let wait = fixture
            .calls()
            .into_iter()
            .find(|call| call[0] == "wait")
            .unwrap();
        assert_eq!(
            wait,
            strings(&[
                "wait",
                "--ws-url",
                ENDPOINT,
                "--session",
                SESSION,
                "--until",
                "settle",
                "--timeout-ms",
                "90000"
            ])
        );
        for (code, expected) in [
            (1, "without completing"),
            (2, "interrupted"),
            (3, "outcome is unknown"),
            (127, "(exit 127)"),
        ] {
            fixture.respond("wait", Some("failed\n"), None, Some(code));
            let message = fixture
                .manager()
                .wait("sub-worker", Duration::from_secs(1))
                .unwrap_err()
                .to_string();
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
    }

    #[test]
    fn read_last_returns_the_durable_reply_and_tail_reads_the_terminal() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("output", Some("DONE\n"), None, None);
        let manager = fixture.manager();
        assert_eq!(
            manager.read_cloud("sub-worker", 50, "last").unwrap(),
            "DONE\n"
        );
        let snapshot =
            agent::read_private_json(&fixture.root.join("registry/sub-worker/output.json"))
                .unwrap();
        assert_eq!(snapshot["text"], "DONE\n");
        assert_eq!(
            manager.read_cloud("sub-worker", 50, "tail").unwrap(),
            "agentterm screen"
        );
        assert!(manager
            .read_cloud("sub-worker", 50, "since_turn")
            .unwrap_err()
            .to_string()
            .contains("since_turn"));
        fixture.herdr.panes.lock().unwrap().clear();
        assert!(manager
            .read_cloud("sub-worker", 50, "tail")
            .unwrap_err()
            .to_string()
            .contains("--output last"));
    }

    #[test]
    fn stop_halts_then_closes_the_recorded_pane_and_archives_both() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let stopped = fixture.manager().stop("sub-worker").unwrap();
        assert_eq!(stopped["session_halted"], true);
        assert_eq!(stopped["session_archived"], true);
        assert_eq!(stopped["pane_closed"], true);
        assert_eq!(stopped["tab_closed"], true);
        assert!(stopped["node_lease"]
            .as_str()
            .unwrap()
            .contains("stop does not release the node reservation"));
        assert_eq!(*fixture.herdr.closed.lock().unwrap(), ["cloud-pane"]);
        let calls = fixture.calls();
        let halt = calls.iter().find(|call| call[0] == "halt").unwrap();
        assert_eq!(
            *halt,
            strings(&[
                "halt",
                "--ws-url",
                ENDPOINT,
                "--session",
                SESSION,
                "--reason",
                "agentctl stop sub-worker"
            ])
        );
        assert!(
            calls
                .iter()
                .any(|call| *call
                    == strings(&["archive", "--ws-url", ENDPOINT, "--session", SESSION]))
        );
        assert!(!fixture.root.join("registry/sub-worker").exists());
        let archived = stopped["archive"].as_str().unwrap();
        let record = agent::read_private_json(&Path::new(archived).join("agent.json")).unwrap();
        assert_eq!(record["lifecycle"], "stopped");
        assert_eq!(record["agentcloud"]["halted"], true);
    }

    #[test]
    fn stop_refuses_to_archive_when_halt_fails_and_offers_the_explicit_skip() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("halt", None, Some("unknown session\n"), Some(1));
        let message = fixture
            .manager()
            .stop("sub-worker")
            .unwrap_err()
            .to_string();
        assert!(message.contains("exited 1: unknown session"));
        assert!(message.contains("--skip-cloud-halt"));
        assert!(fixture.herdr.closed.lock().unwrap().is_empty());
        assert!(fixture.root.join("registry/sub-worker/agent.json").exists());

        let stopped = fixture
            .manager()
            .stop_with_options(
                "sub-worker",
                StopOptions {
                    skip_cloud_halt: true,
                    ..StopOptions::default()
                },
            )
            .unwrap();
        assert_eq!(stopped["session_halted"], false);
        assert_eq!(stopped["session_archived"], false);
        assert_eq!(stopped["pane_closed"], true);
        assert_eq!(
            fixture
                .verbs()
                .iter()
                .filter(|verb| *verb == "archive")
                .count(),
            0
        );
    }

    #[test]
    fn stop_reports_a_failed_session_archive_without_hiding_the_halt() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond(
            "archive",
            None,
            Some("archive_v1 not advertised\n"),
            Some(1),
        );
        let stopped = fixture.manager().stop("sub-worker").unwrap();
        assert_eq!(stopped["session_halted"], true);
        assert_eq!(stopped["session_archived"], false);
        assert!(stopped["session_archive_error"]
            .as_str()
            .unwrap()
            .contains("archive_v1 not advertised"));
    }

    #[test]
    fn herdr_only_operations_redirect_to_the_cloud_interface() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let manager = fixture.manager();
        for message in [
            manager
                .drain("sub-worker", DrainOptions::default())
                .unwrap_err()
                .to_string(),
            manager
                .goal(
                    "sub-worker",
                    Some("objective"),
                    DrainOptions::default(),
                    None,
                )
                .unwrap_err()
                .to_string(),
            manager
                .send("sub-worker", "via herdr", DrainOptions::default())
                .unwrap_err()
                .to_string(),
        ] {
            assert!(message.contains("agentcloud session"), "{message}");
            assert!(message.contains("agentctl send"), "{message}");
        }
        let attached = manager.attach("sub-worker").unwrap();
        assert_eq!(attached["pane_id"], "cloud-pane");
        assert_eq!(*fixture.herdr.focused.lock().unwrap(), ["cloud-pane"]);
    }

    #[test]
    fn noisy_create_diagnostics_never_make_the_record_unloadable() {
        for (exit, stderr) in [
            (0, "x".repeat(2001)),
            (0, "warn\0ing: detail".to_owned()),
            (9, format!("{}\0tail", "y".repeat(5000))),
        ] {
            let fixture = Fixture::new();
            fixture.respond(
                "create",
                Some(&format!("{SESSION}\n")),
                Some(&stderr),
                Some(exit),
            );
            fixture.list_row(false);
            let started = fixture.start(Some("task"));
            assert_eq!(started.is_ok(), exit == 0, "exit {exit}: {started:?}");
            let status = fixture
                .manager()
                .status("sub-worker")
                .unwrap_or_else(|error| panic!("exit {exit}: record became unloadable: {error}"));
            let note = status["agentcloud"]["create_note"].as_str().unwrap();
            assert!(
                note.chars().count() <= 2000,
                "exit {exit}: note has {} chars",
                note.chars().count()
            );
            assert!(!note.contains('\0'));
            let stopped = fixture.manager().stop("sub-worker").unwrap();
            assert_eq!(stopped["session_halted"], true);
        }
    }

    #[test]
    fn send_capture_failures_still_report_the_idempotency_key_and_retry_command() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        // The service may already have journaled the input when output capture fails.
        fixture.respond("send", Some("42\n"), Some(&"e".repeat(65_537)), None);
        let message = fixture
            .manager()
            .send_cloud("sub-worker", "follow-up", None)
            .unwrap_err()
            .to_string();
        let sent = fixture.calls().pop().unwrap();
        let key = &sent[sent
            .iter()
            .position(|arg| arg == "--idempotency-key")
            .unwrap()
            + 1];
        assert!(key.starts_with("agentctl-sub-worker-"));
        assert!(
            message.contains(&format!("--message-id {key}")),
            "{message}"
        );
        assert!(message.contains(" send sub-worker "), "{message}");
        assert!(
            message.contains(&format!("--agentcloud-url {ENDPOINT}")),
            "{message}"
        );

        fixture.respond(
            "send-message",
            Some("m-1\n"),
            Some(&"e".repeat(65_537)),
            None,
        );
        let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                caller_session: Some("c0ffee00-0000-4000-8000-000000000001".to_owned()),
                ..fixture.tools()
            });
        let message = manager
            .send_cloud("sub-worker", "follow-up", Some("retry-9"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("--message-id retry-9"), "{message}");
    }

    #[test]
    fn send_timeouts_after_acceptance_still_report_the_idempotency_key() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("send", Some("42\n"), None, None);
        fs::write(fixture.root.join("tools/responses/send.sleep"), "30").unwrap();
        CONTROL_TIMEOUT_OVERRIDE.with(|timeout| timeout.set(Some(Duration::from_millis(500))));
        let started = Instant::now();
        let message = fixture
            .manager()
            .send_cloud("sub-worker", "follow-up", Some("slow-1"))
            .unwrap_err()
            .to_string();
        CONTROL_TIMEOUT_OVERRIDE.with(|timeout| timeout.set(None));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the bound was not applied"
        );
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("--message-id slow-1"), "{message}");
        assert!(
            message.contains("may or may not have been delivered"),
            "{message}"
        );
    }

    #[test]
    fn stop_refuses_to_close_a_pane_that_moved_during_output_capture() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.herdr.move_on_read.store(true, Ordering::Relaxed);
        let message = fixture
            .manager()
            .stop("sub-worker")
            .unwrap_err()
            .to_string();
        assert!(message.contains("moved"), "{message}");
        assert!(fixture.herdr.closed.lock().unwrap().is_empty());
        let record = fixture.record();
        assert_eq!(
            record["agentcloud"]["halted"], true,
            "the halt is recorded for the retry"
        );
    }

    #[test]
    fn stop_refuses_to_close_a_pane_running_a_foreign_process() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.herdr.detached.store(true, Ordering::Relaxed);
        fixture.herdr.busy_foreign.store(true, Ordering::Relaxed);
        let message = fixture
            .manager()
            .stop("sub-worker")
            .unwrap_err()
            .to_string();
        assert!(message.contains("agentterm"), "{message}");
        assert!(fixture.herdr.closed.lock().unwrap().is_empty());
        assert!(fixture.root.join("registry/sub-worker/agent.json").exists());

        // Once the viewer has exited back to its shell, the owned tab may close.
        fixture.herdr.busy_foreign.store(false, Ordering::Relaxed);
        let stopped = fixture.manager().stop("sub-worker").unwrap();
        assert_eq!(stopped["pane_closed"], true);
    }

    #[test]
    fn viewer_and_every_control_verb_use_the_one_recorded_endpoint() {
        let fixture = Fixture::new();
        // An unset endpoint now resolves to agentcloudctl's documented default (covered by
        // a_legacy_record_without_any_configured_endpoint_uses_the_documented_default); only
        // malformed endpoints are refused.
        for (endpoint, expected) in [
            (Some("https://orchestrator.test"), "ws:// or wss://"),
            (Some("wss://orchestrator.test/ws chat"), "ws:// or wss://"),
        ] {
            let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
                .unwrap()
                .with_inherited_workspace(None)
                .with_cloud_tools(CloudTools {
                    endpoint: endpoint.map(str::to_owned),
                    ..fixture.tools()
                });
            let message = manager
                .start("sub-worker", &fixture.root, fixture.options(None))
                .unwrap_err()
                .to_string();
            assert!(message.contains(expected), "{endpoint:?}: {message}");
        }
        assert!(
            fixture.calls().is_empty(),
            "no session is created with a malformed endpoint"
        );
        assert!(!fixture.root.join("registry/sub-worker").exists());

        fixture.list_row(false);
        fixture.start(None).unwrap();
        assert_eq!(fixture.record()["agentcloud"]["endpoint"], ENDPOINT);
        let viewer = fixture.herdr.commands.lock().unwrap()[0].2.clone();
        assert_eq!(viewer, strings(&["--ws-url", ENDPOINT, "-s", SESSION]));

        // A later default (for example a changed environment) never redirects a recorded agent.
        let moved = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                endpoint: Some("wss://elsewhere.test/ws/chat".to_owned()),
                ..fixture.tools()
            });
        fixture.respond("wait", Some("completed\n"), None, None);
        fixture.respond("output", Some("DONE\n"), None, None);
        moved
            .send_cloud("sub-worker", "follow-up", Some("e-1"))
            .unwrap();
        moved.status("sub-worker").unwrap();
        moved.wait("sub-worker", Duration::from_secs(5)).unwrap();
        moved.read_cloud("sub-worker", 10, "last").unwrap();
        moved.stop("sub-worker").unwrap();
        let calls = fixture.calls();
        assert!(calls.len() >= 8);
        for call in &calls {
            assert_eq!(
                call[1..3],
                ["--ws-url".to_owned(), ENDPOINT.to_owned()],
                "{call:?}"
            );
        }
    }

    #[test]
    fn an_explicit_conflicting_endpoint_is_refused_before_any_control_call() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let before = fixture.calls().len();
        let explicit = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                endpoint: Some("wss://elsewhere.test/ws/chat".to_owned()),
                endpoint_explicit: true,
                ..fixture.tools()
            });
        for message in [
            explicit
                .send_cloud("sub-worker", "x", None)
                .unwrap_err()
                .to_string(),
            explicit.stop("sub-worker").unwrap_err().to_string(),
            explicit
                .wait("sub-worker", Duration::from_secs(1))
                .unwrap_err()
                .to_string(),
        ] {
            assert!(message.contains(ENDPOINT), "{message}");
            assert!(message.contains("--agentcloud-url"), "{message}");
        }
        assert_eq!(fixture.calls().len(), before);
        assert!(fixture.root.join("registry/sub-worker/agent.json").exists());
    }

    /// Split a printed POSIX shell command (plain words, single quotes, backslash escapes).
    fn shell_words(command: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut started = false;
        let mut quoted = false;
        let mut characters = command.chars();
        while let Some(character) = characters.next() {
            match character {
                '\'' => {
                    quoted = !quoted;
                    started = true;
                }
                '\\' if !quoted => {
                    word.extend(characters.next());
                    started = true;
                }
                ' ' if !quoted => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                other => {
                    word.push(other);
                    started = true;
                }
            }
        }
        if started {
            words.push(word);
        }
        words
    }

    /// The command between the backticks that follow `marker` in an error message.
    fn printed_command(message: &str, marker: &str) -> String {
        let start = message
            .find(marker)
            .unwrap_or_else(|| panic!("{marker:?} not in {message}"))
            + marker.len();
        let length = message[start..].find('`').expect("closing backtick");
        message[start..start + length].to_owned()
    }

    /// Run a printed `agentctl ...` retry through the real CLI against this fixture's fakes.
    fn run_printed_retry(fixture: &Fixture, command: &str, text: &str) -> i32 {
        run_printed_retry_in(fixture, command, text, None)
    }

    /// Run a printed retry as if from a terminal inside `session`, or outside any session, which
    /// is modelled as an empty `AGENTCLOUD_SESSION_ID` so children never inherit this process's.
    fn run_printed_retry_in(
        fixture: &Fixture,
        command: &str,
        text: &str,
        session: Option<&str>,
    ) -> i32 {
        let words = shell_words(command);
        assert_eq!(words[0], "agentctl", "{command}");
        let prompt = fixture.root.join("retry-prompt.txt");
        fs::write(&prompt, text).unwrap();
        let mut arguments = vec![
            "--agentcloudctl-bin".to_owned(),
            fixture
                .root
                .join("tools/agentcloudctl")
                .display()
                .to_string(),
            "--herdr-bin".to_owned(),
            "/nonexistent/herdr".to_owned(),
        ];
        arguments.extend(words[1..].iter().map(|word| {
            if word == "PROMPT_FILE" {
                prompt.display().to_string()
            } else {
                word.clone()
            }
        }));
        let session = Some(session.unwrap_or_default().to_owned());
        crate::cli::main_with_environment(
            arguments.into_iter().map(std::ffi::OsString::from),
            &move |name| match name {
                "AGENTCLOUD_SESSION_ID" => session.clone(),
                _ => None,
            },
        )
    }

    #[test]
    fn a_peer_message_retry_reproduces_the_verb_sender_endpoint_and_key() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let sender = "c0ffee00-0000-4000-8000-000000000001";
        let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                caller_session: Some(sender.to_owned()),
                ..fixture.tools()
            });
        fixture.respond("send-message", None, Some("transport lost\n"), Some(3));
        let message = manager
            .send_cloud("sub-worker", "follow-up", Some("retry-7"))
            .unwrap_err()
            .to_string();
        let command = printed_command(&message, "Retry with `");
        let words = shell_words(&command);
        for required in [
            "--from-session",
            sender,
            "--agentcloud-url",
            ENDPOINT,
            "retry-7",
        ] {
            assert!(
                words.iter().any(|word| word == required),
                "{required} missing: {command}"
            );
        }

        // Following the printed retry must repeat the attributed peer message, not a human send.
        fixture.respond("send-message", Some("m-1\n"), None, None);
        assert_eq!(
            run_printed_retry(&fixture, &command, "follow-up"),
            0,
            "{command}"
        );
        assert_eq!(
            fixture.calls().pop().unwrap(),
            strings(&[
                "send-message",
                "--ws-url",
                ENDPOINT,
                "--session",
                sender,
                "--to",
                SESSION,
                "--body",
                "follow-up",
                "--idempotency-key",
                "retry-7",
            ])
        );
    }

    #[test]
    fn an_explicit_human_send_from_inside_another_session_is_refused_not_impersonated() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let other = "c0ffee00-0000-4000-8000-000000000002";
        let inside = |caller: Option<&str>, ambient: &str| {
            ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
                .unwrap()
                .with_cloud_tools(CloudTools {
                    caller_session: caller.map(str::to_owned),
                    ambient_session: Some(ambient.to_owned()),
                    ..fixture.tools()
                })
        };
        let calls = fixture.calls().len();
        let message = inside(None, other)
            .send_cloud("sub-worker", "as the owner", Some("human-2"))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("will not bypass that attribution guard"),
            "{message}"
        );
        assert_eq!(
            fixture.calls().len(),
            calls,
            "agentcloudctl must not be asked to impersonate"
        );
        let command = printed_command(&message, "outside any agentcloud session with `");
        let words = shell_words(&command);
        for required in [
            "--from-session",
            "",
            "send",
            "sub-worker",
            "human-2",
            ENDPOINT,
        ] {
            assert!(
                words.iter().any(|word| word == required),
                "{required:?}: {command}"
            );
        }
        // The printed human retry works from outside a session and is refused from inside one.
        assert_eq!(
            run_printed_retry_in(&fixture, &command, "as the owner", Some(other)),
            75
        );
        assert_eq!(fixture.calls().len(), calls);
        assert_eq!(
            run_printed_retry_in(&fixture, &command, "as the owner", None),
            0
        );
        let retried = fixture.calls().pop().unwrap();
        assert_eq!(retried[0], "send");
        assert!(retried.contains(&"human-2".to_owned()));

        // Attribution to the ambient session is allowed, as is steering the session's own queue.
        fixture.respond("send-message", Some("m-2\n"), None, None);
        let attributed = inside(Some(other), other)
            .send_cloud("sub-worker", "attributed", Some("peer-2"))
            .unwrap();
        assert_eq!(attributed["delivery"], "send-message");
        let own = inside(None, SESSION)
            .send_cloud("sub-worker", "own queue", Some("own-2"))
            .unwrap();
        assert_eq!(own["delivery"], "send");

        // Every child saw exactly the session context agentctl was given: none for the
        // fixture's start (whatever this test process inherited), then each ambient session.
        let sessions = fixture.child_sessions();
        assert_eq!(sessions[0], "", "create ran explicitly outside any session");
        let tail = &sessions[sessions.len() - 2..];
        assert_eq!(tail, [other.to_owned(), SESSION.to_owned()]);
    }

    #[test]
    fn default_tools_read_the_real_session_environment() {
        let inherited = std::env::var("AGENTCLOUD_SESSION_ID").ok();
        let defaults = CloudTools::default();
        assert_eq!(defaults.ambient_session, inherited);
        assert_eq!(
            defaults.caller_session,
            inherited.clone().filter(|session| !session.is_empty())
        );
        let herdr = Herdr::default();
        let manager = ManagedAgents::new(&herdr, Path::new("/nonexistent/registry")).unwrap();
        assert_eq!(manager.cloud_tools.ambient_session, inherited);

        let inside = CloudTools::from_environment(&|name| match name {
            "AGENTCLOUD_SESSION_ID" => Some(SESSION.to_owned()),
            "AGENTCLOUD_ORCHESTRATOR_URL" => Some(ENDPOINT.to_owned()),
            _ => None,
        });
        assert_eq!(inside.ambient_session.as_deref(), Some(SESSION));
        assert_eq!(inside.caller_session.as_deref(), Some(SESSION));
        assert_eq!(inside.endpoint.as_deref(), Some(ENDPOINT));
        let outside = CloudTools::from_environment(&|_| Some(String::new()));
        assert_eq!(outside.ambient_session.as_deref(), Some(""));
        assert_eq!(outside.caller_session, None);
        assert_eq!(outside.endpoint, None);
        let unset = CloudTools::from_environment(&|_| None);
        assert_eq!(
            unset.ambient_session, None,
            "unset means inherit, not remove"
        );
    }

    #[test]
    fn an_unknown_session_context_is_inherited_unchanged_never_removed() {
        const MARKER: &str = "AGENTCTL_TEST_EXPECT_INHERITED_SESSION";
        let Ok(expected) = std::env::var(MARKER) else {
            // Re-run this exact test in a child whose environment carries a known session, so the
            // check is deterministic whatever this process inherited.
            let inherited = "c0ffee00-0000-4000-8000-00000000000b";
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "subagents::cloud::tests::an_unknown_session_context_is_inherited_unchanged_never_removed",
                    "--test-threads=1",
                ])
                .env("AGENTCLOUD_SESSION_ID", inherited)
                .env(MARKER, inherited)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains("1 passed"),
                "the child did not run the check: {stdout}"
            );
            return;
        };
        let fixture = Fixture::new();
        fixture.list_row(false);
        let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_inherited_workspace(None)
            .with_cloud_tools(CloudTools {
                ambient_session: None,
                ..fixture.tools()
            });
        manager
            .start("sub-worker", &fixture.root, fixture.options(None))
            .unwrap();
        assert_eq!(fixture.child_sessions()[0], expected);
    }

    #[test]
    fn a_human_send_retry_pins_the_absence_of_a_sender() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.respond("send", None, Some("transport lost\n"), Some(3));
        let message = fixture
            .manager()
            .send_cloud("sub-worker", "follow-up", Some("human-1"))
            .unwrap_err()
            .to_string();
        let command = printed_command(&message, "Retry with `");
        let words = shell_words(&command);
        let sender = words
            .iter()
            .position(|word| word == "--from-session")
            .expect("pinned sender");
        assert_eq!(
            words[sender + 1],
            "",
            "an empty sender selects human attribution: {command}"
        );
        // Even when the retry runs inside some agentcloud session, it must stay a human send.
        fixture.respond("send", Some("43\n"), None, None);
        assert_eq!(
            run_printed_retry(&fixture, &command, "follow-up"),
            0,
            "{command}"
        );
        let retried = fixture.calls().pop().unwrap();
        assert_eq!(retried[0], "send", "{command}");
        assert!(retried.contains(&"human-1".to_owned()));
        assert!(retried.contains(&"--end-of-turn".to_owned()));
    }

    #[test]
    fn every_printed_recovery_command_names_the_recorded_endpoint() {
        let fixture = Fixture::new();
        let agentterm = fs::canonicalize(fixture.root.join("tools/agentterm")).unwrap();
        let viewer = format!("{} --ws-url {ENDPOINT} -s {SESSION}", agentterm.display());
        fixture.herdr.fail_command.store(true, Ordering::Relaxed);
        let message = fixture.start(None).unwrap_err().to_string();
        assert!(message.contains(&viewer), "{message}");
        let registry = fixture.root.join("registry");
        assert!(
            message.contains(&format!(
                "agentctl --registry {} stop sub-worker",
                registry.display()
            )),
            "{message}"
        );

        fixture.herdr.fail_command.store(false, Ordering::Relaxed);
        fixture.list_row(false);
        fixture.manager().stop("sub-worker").unwrap();
        fixture.start(None).unwrap();
        fixture.herdr.panes.lock().unwrap().clear();
        let message = fixture
            .manager()
            .attach("sub-worker")
            .unwrap_err()
            .to_string();
        assert!(message.contains(&viewer), "{message}");

        fixture.respond("list", Some("[]"), None, None);
        let status = fixture.manager().status("sub-worker").unwrap();
        assert!(status["probe_error"]
            .as_str()
            .unwrap()
            .contains(&format!("inspect --ws-url {ENDPOINT} --session {SESSION}")));
        let stopped = fixture.manager().stop("sub-worker").unwrap();
        assert!(stopped["node_lease"]
            .as_str()
            .unwrap()
            .contains(&format!("inspect --ws-url {ENDPOINT} --session {SESSION}")));

        let fresh = Fixture::new();
        fresh.respond("create", Some("created!\n"), None, None);
        let message = fresh.start(None).unwrap_err().to_string();
        assert!(
            message.contains(&format!("list --ws-url {ENDPOINT}")),
            "{message}"
        );
    }

    #[test]
    fn records_written_before_the_endpoint_field_still_load_and_retire() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        let path = fixture.root.join("registry/sub-worker/agent.json");
        let mut record = agent::read_private_json(&path).unwrap();
        record["agentcloud"]
            .as_object_mut()
            .unwrap()
            .remove("endpoint");
        agent::atomic_json(&path, &record).unwrap();

        let status = fixture.manager().status("sub-worker").unwrap();
        assert_eq!(status["agent_status"], "idle");
        assert!(fixture.manager().list().is_ok());
        // A read-only command does not rewrite the record.
        assert!(fixture.record()["agentcloud"].get("endpoint").is_none());

        // The first mutating command resolves the endpoint once and saves it.
        fixture
            .manager()
            .send_cloud("sub-worker", "x", Some("legacy-1"))
            .unwrap();
        assert_eq!(fixture.record()["agentcloud"]["endpoint"], ENDPOINT);

        // --skip-cloud-halt needs no endpoint at all, even with a conflicting explicit one.
        record["agentcloud"]
            .as_object_mut()
            .unwrap()
            .remove("endpoint");
        agent::atomic_json(&path, &record).unwrap();
        let conflicting = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                endpoint: Some("wss://elsewhere.test/ws/chat".to_owned()),
                endpoint_explicit: true,
                ..fixture.tools()
            });
        let stopped = conflicting
            .stop_with_options(
                "sub-worker",
                StopOptions {
                    skip_cloud_halt: true,
                    ..StopOptions::default()
                },
            )
            .unwrap();
        assert_eq!(stopped["session_halted"], false);
    }

    #[test]
    fn a_legacy_record_without_any_configured_endpoint_uses_the_documented_default() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        let manager = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_inherited_workspace(None)
            .with_cloud_tools(CloudTools {
                endpoint: None,
                ..fixture.tools()
            });
        manager
            .start("sub-worker", &fixture.root, fixture.options(None))
            .unwrap();
        let default = "wss://mm.internalmeta.com/ws/chat";
        assert_eq!(fixture.record()["agentcloud"]["endpoint"], default);
        assert_eq!(
            fixture.calls()[0][1..3],
            ["--ws-url".to_owned(), default.to_owned()]
        );
        assert_eq!(
            fixture.herdr.commands.lock().unwrap()[0].2,
            strings(&["--ws-url", default, "-s", SESSION])
        );
    }

    #[test]
    fn a_refusal_after_skip_cloud_halt_says_the_session_was_not_halted() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.herdr.detached.store(true, Ordering::Relaxed);
        fixture.herdr.busy_foreign.store(true, Ordering::Relaxed);
        let message = fixture
            .manager()
            .stop_with_options(
                "sub-worker",
                StopOptions {
                    skip_cloud_halt: true,
                    ..StopOptions::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(!message.contains("the session is halted"), "{message}");
        assert!(message.contains("NOT halted"), "{message}");
        assert!(!fixture.verbs().contains(&"halt".to_owned()));
    }

    #[test]
    fn project_workspace_policy_places_and_guards_the_terminal_tab() {
        let fixture = Fixture::new();
        let mut options = fixture.options(None);
        options.workspace_id = Some("workspace".to_owned());
        let message = fixture
            .manager()
            .with_project_workspace(Some("project-agents"))
            .start("sub-worker", &fixture.root, options)
            .unwrap_err()
            .to_string();
        assert!(message.contains("project configuration requires \"project-agents\""));
        assert!(
            fixture.calls().is_empty(),
            "no session is created on a policy refusal"
        );
        assert!(!fixture.root.join("registry/sub-worker").exists());

        fixture.list_row(false);
        let mut options = fixture.options(None);
        options.workspace_id = None;
        fixture
            .manager()
            .with_project_workspace(Some("project-agents"))
            .start("sub-worker", &fixture.root, options)
            .unwrap();
        assert_eq!(*fixture.herdr.labels.lock().unwrap(), ["project-agents"]);
    }

    #[test]
    fn an_invalid_endpoint_is_never_saved_into_a_record_that_lacks_one() {
        for bad in ["https://bad.invalid", "", "wss://a b", "wss://a\nb"] {
            let fixture = Fixture::new();
            fixture.list_row(false);
            fixture.start(None).unwrap();
            let path = fixture.root.join("registry/sub-worker/agent.json");
            let mut record = agent::read_private_json(&path).unwrap();
            record["agentcloud"]
                .as_object_mut()
                .unwrap()
                .remove("endpoint");
            agent::atomic_json(&path, &record).unwrap();
            let before = fs::read(&path).unwrap();
            let calls = fixture.calls().len();
            let poisoned = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
                .unwrap()
                .with_cloud_tools(CloudTools {
                    agentcloudctl: fixture.root.join("tools/absent-agentcloudctl"),
                    endpoint: Some(bad.to_owned()),
                    endpoint_explicit: true,
                    ..fixture.tools()
                });
            let message = poisoned
                .send_cloud("sub-worker", "x", Some("bad-1"))
                .unwrap_err()
                .to_string();
            assert!(message.contains("ws:// or wss://"), "{bad:?}: {message}");
            assert!(poisoned.stop("sub-worker").is_err(), "{bad:?}");
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "{bad:?}: the record was rewritten"
            );
            assert_eq!(
                fixture.calls().len(),
                calls,
                "{bad:?}: agentcloudctl was contacted"
            );
            // The record stays loadable and retirable.
            fixture.manager().status("sub-worker").unwrap();
            fixture
                .manager()
                .stop_with_options(
                    "sub-worker",
                    StopOptions {
                        skip_cloud_halt: true,
                        ..StopOptions::default()
                    },
                )
                .unwrap();
        }
    }

    #[test]
    fn viewer_recovery_uses_the_recorded_endpoint_despite_a_conflicting_flag() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        fixture.start(None).unwrap();
        fixture.herdr.panes.lock().unwrap().clear();
        let elsewhere = "wss://elsewhere.test/ws/chat";
        let message = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                endpoint: Some(elsewhere.to_owned()),
                endpoint_explicit: true,
                ..fixture.tools()
            })
            .attach("sub-worker")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(&format!("--ws-url {ENDPOINT} -s {SESSION}")),
            "{message}"
        );
        assert!(!message.contains(elsewhere), "{message}");
    }

    #[test]
    fn cloud_input_and_terminal_reads_follow_the_project_workspace_policy() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        let manager = || fixture.manager().with_project_workspace(Some("subagents"));
        manager()
            .start("sub-worker", &fixture.root, fixture.options(None))
            .unwrap();
        fixture.respond("output", Some("DONE\n"), None, None);
        manager()
            .send_cloud("sub-worker", "in place", Some("w-1"))
            .unwrap();
        manager().read_cloud("sub-worker", 10, "tail").unwrap();

        // The tab's workspace no longer matches the project policy.
        *fixture.herdr.label.lock().unwrap() = Some("elsewhere".to_owned());
        let calls = fixture.calls().len();
        for message in [
            manager()
                .send_cloud("sub-worker", "misplaced", Some("w-2"))
                .unwrap_err()
                .to_string(),
            manager()
                .read_cloud("sub-worker", 10, "tail")
                .unwrap_err()
                .to_string(),
        ] {
            assert!(message.contains("\"elsewhere\""), "{message}");
            assert!(message.contains("\"subagents\""), "{message}");
        }
        assert_eq!(
            fixture.calls().len(),
            calls,
            "a misplaced agent is not contacted"
        );
        // Observation and retirement stay available.
        manager().status("sub-worker").unwrap();
        assert_eq!(
            manager().read_cloud("sub-worker", 10, "last").unwrap(),
            "DONE\n"
        );
        manager().stop("sub-worker").unwrap();
    }

    #[test]
    fn a_deferred_startup_goal_rechecks_the_workspace_policy() {
        let fixture = Fixture::new();
        fixture.list_row(false);
        *fixture.herdr.relabel_on_command.lock().unwrap() = Some("elsewhere".to_owned());
        let message = fixture
            .manager()
            .with_project_workspace(Some("subagents"))
            .start(
                "sub-worker",
                &fixture.root,
                fixture.options(Some("/goal finish the migration")),
            )
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("/goal brief was not delivered"),
            "{message}"
        );
        assert!(message.contains("\"elsewhere\""), "{message}");
        assert!(
            !fixture.verbs().iter().any(|verb| verb == "send"),
            "the goal was delivered to a misplaced agent"
        );
        assert_eq!(fixture.record()["lifecycle"], "running");
    }

    #[test]
    fn session_prefixes_are_never_recorded_or_attached() {
        for printed in ["0b7c", "0B7C9A2E-1F3D-4C5B-8A69-2D4E6F8A0B1C", "session-7"] {
            let fixture = Fixture::new();
            fixture.respond("create", Some(&format!("{printed}\n")), None, None);
            let message = fixture.start(None).unwrap_err().to_string();
            assert!(
                message.contains("no recognizable session ID"),
                "{printed}: {message}"
            );
            assert!(
                fixture.herdr.commands.lock().unwrap().is_empty(),
                "{printed}"
            );
            assert_eq!(fixture.record()["session_value"], Value::Null, "{printed}");
        }
    }

    #[test]
    fn session_ids_must_be_complete_canonical_uuids() {
        assert_eq!(
            parse_session_id(&format!("  {SESSION}\n")).as_deref(),
            Some(SESSION)
        );
        for invalid in [
            "",
            "\n",
            "two\nlines",
            "has space",
            "-leading",
            "a\0b",
            "0b7c",
            "0b7c9a2e-1f3d-4c5b-8a69",
            "0B7C9A2E-1F3D-4C5B-8A69-2D4E6F8A0B1C",
            "0b7c9a2e-1f3d-4c5b-8a69-2d4e6f8a0b1cz",
            "0b7c9a2e_1f3d_4c5b_8a69_2d4e6f8a0b1c",
            "gb7c9a2e-1f3d-4c5b-8a69-2d4e6f8a0b1c",
        ] {
            assert!(parse_session_id(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn launch_placeholders_expand_to_the_name_host_and_directory() {
        let fixture = Fixture::new();
        fixture.list_row(true);
        let mut options = fixture.options(None);
        options.cloud = Some(CloudLaunch {
            harness: Some("claude-code".to_owned()),
            node_id: Some("{fqdn}".to_owned()),
            workspace: Some("{cwd}".to_owned()),
            title: Some("{name}-{host}".to_owned()),
            ..CloudLaunch::default()
        });
        fixture
            .manager()
            .start("sub-worker", &fixture.root, options)
            .unwrap();
        let cwd = fs::canonicalize(&fixture.root).unwrap();
        let create = &fixture.calls()[0];
        let value = |flag: &str| {
            let index = create.iter().position(|item| item == flag).unwrap();
            create[index + 1].clone()
        };
        assert_eq!(value("--title"), "sub-worker-node-a");
        assert_eq!(value("--node-id"), "node-a.example.test");
        assert_eq!(value("--workspace"), cwd.display().to_string());
        // The record keeps the expanded values, which status and every later verb report.
        let launch = &fixture.record()["agentcloud"]["launch"];
        assert_eq!(launch["title"], "sub-worker-node-a");
        assert_eq!(launch["node_id"], "node-a.example.test");
    }

    #[test]
    fn launch_placeholders_refuse_unknown_names_and_unresolvable_hosts() {
        assert!(CloudLaunch::default().validate("profile").is_ok());
        for (launch, expected) in [
            (
                CloudLaunch {
                    title: Some("{nme}".to_owned()),
                    ..CloudLaunch::default()
                },
                "unknown placeholder",
            ),
            (
                CloudLaunch {
                    title: Some("open {name".to_owned()),
                    ..CloudLaunch::default()
                },
                "unknown placeholder",
            ),
            (
                CloudLaunch {
                    workspace: Some("relative{cwd}".to_owned()),
                    ..CloudLaunch::default()
                },
                "absolute path",
            ),
            (
                CloudLaunch {
                    title: Some(" ".to_owned()),
                    ..CloudLaunch::default()
                },
                "title must be a nonempty single-line string",
            ),
        ] {
            let message = launch.validate("profile").unwrap_err().to_string();
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
        let template = CloudLaunch {
            node_id: Some("{host}".to_owned()),
            ..CloudLaunch::default()
        };
        template.validate("profile").unwrap();
        let unknown_host = LaunchContext {
            name: "sub-worker",
            hostname: None,
            cwd: "/work",
        };
        let message = template
            .expand("profile", &unknown_host)
            .unwrap_err()
            .to_string();
        assert!(message.contains("host name is unknown"), "{message}");
        // Braces that arrive through an expanded value are literal, never re-expanded.
        let braced = CloudLaunch {
            workspace: Some("{cwd}".to_owned()),
            ..CloudLaunch::default()
        }
        .expand(
            "profile",
            &LaunchContext {
                name: "sub-worker",
                hostname: None,
                cwd: "/work/{name}",
            },
        )
        .unwrap();
        assert_eq!(braced.workspace.as_deref(), Some("/work/{name}"));
        braced.validate_expanded("agent record").unwrap();
    }

    fn self_manager(fixture: &Fixture) -> ManagedAgents<'_, Herdr> {
        ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_inherited_workspace(None)
            .with_cloud_tools(CloudTools {
                caller_session: Some(SESSION.to_owned()),
                ambient_session: Some(SESSION.to_owned()),
                ..fixture.tools()
            })
    }

    fn self_options() -> StartOptions {
        StartOptions {
            harness: CLOUD_HARNESS.to_owned(),
            ..StartOptions::default()
        }
    }

    #[test]
    fn attach_self_presents_the_callers_session_without_touching_agentcloud() {
        let fixture = Fixture::new();
        fixture.list_row(true);
        let manager = self_manager(&fixture);
        let status = manager
            .attach_self("sub-worker", &fixture.root, self_options())
            .unwrap();
        assert_eq!(status["name"], "sub-worker");
        // No create, send, or halt: only the status listing ran.
        assert!(
            fixture.verbs().iter().all(|verb| verb == "list"),
            "{:?}",
            fixture.verbs()
        );
        let base = fs::canonicalize(&fixture.root)
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(*fixture.herdr.labels.lock().unwrap(), vec![base]);
        let commands = fixture.herdr.commands.lock().unwrap().clone();
        assert_eq!(
            commands[0].2,
            strings(&["--ws-url", ENDPOINT, "-s", SESSION])
        );
        let record = fixture.record();
        assert_eq!(record["session_value"], SESSION);
        assert_eq!(record["lifecycle"], "running");
        assert_eq!(record["agentcloud"]["attached_self"], true);

        let message = manager
            .attach_self("sub-again", &fixture.root, self_options())
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("already registered as \"sub-worker\""),
            "{message}"
        );
        assert!(!fixture.root.join("registry/sub-again").exists());

        // Agents this session starts without an explicit workspace join the attached one.
        // The fake gives every label the same workspace, so move the attached tab elsewhere.
        fixture.herdr.panes.lock().unwrap()[0].workspace_id = "self-workspace".to_owned();
        let mut attached = manager.load("sub-worker").unwrap();
        attached.workspace_id = Some("self-workspace".to_owned());
        manager.save(&attached).unwrap();
        let mut options = fixture.options(None);
        options.workspace_id = None;
        fixture.respond(
            "create",
            Some("c0ffee00-0000-4000-8000-000000000003\n"),
            None,
            None,
        );
        manager.start("sub-child", &fixture.root, options).unwrap();
        let child: Value =
            agent::read_private_json(&fixture.root.join("registry/sub-child/agent.json")).unwrap();
        assert_eq!(child["workspace_id"], "self-workspace");

        let stopped = manager.stop("sub-worker").unwrap();
        assert_eq!(stopped["session_halted"], false);
        assert_eq!(stopped["session_archived"], false);
        assert_eq!(stopped["pane_closed"], true);
        assert!(
            !fixture
                .verbs()
                .iter()
                .any(|verb| verb == "halt" || verb == "archive"),
            "stop must never halt or archive the caller's own session: {:?}",
            fixture.verbs()
        );
        assert!(!fixture.root.join("registry/sub-worker").exists());
    }

    #[test]
    fn attach_self_is_refused_outside_an_agentcloud_session() {
        let fixture = Fixture::new();
        let message = fixture
            .manager()
            .attach_self("sub-worker", &fixture.root, self_options())
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("$AGENTCLOUD_SESSION_ID is unset"),
            "{message}"
        );
        let partial = ManagedAgents::new(&fixture.herdr, &fixture.root.join("registry"))
            .unwrap()
            .with_cloud_tools(CloudTools {
                ambient_session: Some("0b7c9a2e".to_owned()),
                ..fixture.tools()
            });
        let message = partial
            .attach_self("sub-worker", &fixture.root, self_options())
            .unwrap_err()
            .to_string();
        assert!(message.contains("not a complete session ID"), "{message}");
        assert!(fixture.calls().is_empty());
        assert!(fixture.herdr.commands.lock().unwrap().is_empty());
        assert!(!fixture.root.join("registry/sub-worker").exists());
    }
}
